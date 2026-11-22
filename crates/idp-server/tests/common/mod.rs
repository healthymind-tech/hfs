//! Test harness: a realm `clinic` on the memory store, bound to the FHIR
//! endpoint `clinic` (aud `https://hfs.test/fhir/clinic`), with clients,
//! users and a router driven through `tower::ServiceExt::oneshot`. The
//! hosted pages are simulated by calling the completer directly.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use helios_endpoints::SecretKeyRing;
use helios_idp::admin::IdpAdmin;
use helios_idp::client::{ClientAuth, ClientSpec, GrantType, LaunchType};
use helios_idp::events::MemoryEventSink;
use helios_idp::interaction::{AuthorizationResponse, Decision};
use helios_idp::keys::{RsaBits, SigningAlg, SigningKey};
use helios_idp::launch::LaunchContext;
use helios_idp::memory::MemoryStore;
use helios_idp::pkce;
use helios_idp::realm::{RealmName, RealmSpec};
use helios_idp::runtime::{ConventionalEndpointUrls, IdpRuntime};
use helios_idp::scope::ScopePolicy;
use helios_idp::user::{FhirLink, RealmUser, UserKind};
use helios_idp_server::jwks_fetch::{FetchedJwks, JwksFetcher};
use helios_idp_server::{IdpServer, IdpServerOptions};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

pub const BASE: &str = "https://hfs.test";
pub const ISS: &str = "https://hfs.test/idp/clinic";
pub const AUD: &str = "https://hfs.test/fhir/clinic";
pub const REDIRECT: &str = "https://app.example/cb";
pub const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
pub const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

/// Mock client JWKS host (key rotation tests).
#[derive(Default)]
pub struct MockJwks {
    pub sets: Mutex<HashMap<String, Value>>,
    pub fetches: Mutex<u32>,
}

#[async_trait]
impl JwksFetcher for MockJwks {
    async fn fetch(&self, url: &str) -> Result<FetchedJwks, String> {
        *self.fetches.lock().unwrap() += 1;
        let v = self.sets.lock().unwrap().get(url).cloned().ok_or("404")?;
        Ok(FetchedJwks {
            jwks: v,
            ttl: std::time::Duration::from_secs(3600),
        })
    }
}

/// The store under test: memory, or PostgreSQL when built with
/// `--features postgres` and run with `IDP_TEST_STORE=pg` +
/// `HFS_TEST_PG_HOST`/`HFS_TEST_PG_PORT` (fresh database per test, real
/// fork migrations).
pub async fn test_store() -> Arc<dyn helios_idp::store::IdpStore> {
    #[cfg(feature = "postgres")]
    if std::env::var("IDP_TEST_STORE").as_deref() == Ok("pg") {
        use helios_persistence::backends::postgres::{PostgresBackend, PostgresConfig};
        let host = std::env::var("HFS_TEST_PG_HOST").expect("HFS_TEST_PG_HOST");
        let port: u16 = std::env::var("HFS_TEST_PG_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(5432);
        let dbname = format!("idpflow_{}", uuid::Uuid::new_v4().simple());
        let (admin, conn) = tokio_postgres::Config::new()
            .host(&host)
            .port(port)
            .user("postgres")
            .password("postgres")
            .dbname("postgres")
            .connect(tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(conn);
        admin
            .batch_execute(&format!("CREATE DATABASE {dbname}"))
            .await
            .unwrap();
        let backend = PostgresBackend::new(PostgresConfig {
            host,
            port,
            dbname,
            user: "postgres".into(),
            password: Some("postgres".into()),
            max_connections: 8,
            ..Default::default()
        })
        .await
        .unwrap();
        backend.init_schema().await.unwrap();
        let pool = backend.shared_pool();
        std::mem::forget(backend);
        return Arc::new(helios_idp::pg::PgStore::new(pool));
    }
    Arc::new(MemoryStore::new())
}

pub struct Harness {
    pub router: Router,
    pub server: IdpServer,
    pub rt: Arc<IdpRuntime>,
    pub admin: IdpAdmin,
    pub realm_id: String,
    pub events: Arc<MemoryEventSink>,
    pub secret: String,
    pub rs_secret: String,
    /// Key of `asym` (ES384, inline JWKS) and `backend` (RS384 via jwks_uri).
    pub asym_key: SigningKey,
    pub backend_key: SigningKey,
    pub jwks: Arc<MockJwks>,
}

pub fn policy(s: &str) -> ScopePolicy {
    ScopePolicy {
        allowed: s.split(' ').map(str::to_string).collect(),
        accept_v1: true,
    }
}

fn client(id: &str, auth: ClientAuth, grants: Vec<GrantType>, scopes: &str) -> ClientSpec {
    ClientSpec {
        client_id: id.into(),
        name: format!("{id} app"),
        auth,
        grant_types: grants,
        redirect_uris: vec![REDIRECT.into(), "http://127.0.0.1/cb".into()],
        post_logout_redirect_uris: vec!["https://app.example/bye".into()],
        launch_types: vec![LaunchType::Standalone, LaunchType::Ehr],
        scopes: policy(scopes),
        trusted: false,
        require_dpop: false,
        require_par: false,
        backchannel_logout_uri: None,
        access_token_ttl_secs: None,
        disabled: false,
    }
}

pub const USER_SCOPES: &str = "openid fhirUser profile email launch launch/patient launch/encounter offline_access online_access patient/*.cruds user/*.cruds patient/Observation.rs?category=laboratory";

impl Harness {
    pub async fn new() -> Harness {
        let store = test_store().await;
        let ring = Arc::new(SecretKeyRing::parse(&SecretKeyRing::generate_spec("t1")).unwrap());
        let rt = IdpRuntime::new(
            store,
            Some(ring),
            url::Url::parse(BASE).unwrap(),
            Arc::new(ConventionalEndpointUrls {
                public_base: BASE.into(),
            }),
        );
        let events = Arc::new(MemoryEventSink::default());
        rt.set_event_sink(events.clone());
        let admin = IdpAdmin::new(rt.clone());
        let mut spec = RealmSpec::new(RealmName::parse("clinic").unwrap(), "Clinic");
        spec.endpoints = vec!["clinic".into()];
        let realm = admin.create_realm(spec).await.unwrap();
        let rid = realm.id.clone();

        admin
            .create_client(
                &rid,
                client(
                    "app",
                    ClientAuth::None,
                    vec![GrantType::AuthorizationCode, GrantType::RefreshToken],
                    USER_SCOPES,
                ),
            )
            .await
            .unwrap();
        let conf = admin
            .create_client(
                &rid,
                client(
                    "conf",
                    ClientAuth::ClientSecret {
                        secret_hash: String::new(),
                    },
                    vec![GrantType::AuthorizationCode, GrantType::RefreshToken],
                    USER_SCOPES,
                ),
            )
            .await
            .unwrap();
        let asym_key = SigningKey::generate(SigningAlg::ES384, RsaBits::B2048).unwrap();
        admin
            .create_client(
                &rid,
                client(
                    "asym",
                    ClientAuth::PrivateKeyJwt {
                        jwks_uri: None,
                        jwks: Some(json!({"keys": [asym_key.public_jwk()]})),
                        algs: vec![],
                    },
                    vec![
                        GrantType::AuthorizationCode,
                        GrantType::RefreshToken,
                        GrantType::ClientCredentials,
                    ],
                    &format!("{USER_SCOPES} system/*.rs system/Patient.cruds"),
                ),
            )
            .await
            .unwrap();
        let backend_key = SigningKey::generate(SigningAlg::RS384, RsaBits::B2048).unwrap();
        let jwks = Arc::new(MockJwks::default());
        jwks.sets.lock().unwrap().insert(
            "https://backend.example/jwks.json".into(),
            json!({"keys": [backend_key.public_jwk()]}),
        );
        let mut b = client(
            "backend",
            ClientAuth::PrivateKeyJwt {
                jwks_uri: Some("https://backend.example/jwks.json".into()),
                jwks: None,
                algs: vec![],
            },
            vec![GrantType::ClientCredentials],
            "system/*.rs system/Observation.cruds",
        );
        b.redirect_uris.clear();
        b.launch_types = vec![LaunchType::Backend];
        admin.create_client(&rid, b).await.unwrap();
        // A resource server that introspects.
        let mut rs = client(
            "rs",
            ClientAuth::ClientSecret {
                secret_hash: String::new(),
            },
            vec![],
            "",
        );
        rs.redirect_uris.clear();
        rs.launch_types.clear();
        let rs = admin.create_client(&rid, rs).await.unwrap();

        admin
            .create_user(
                &rid,
                RealmUser {
                    id: "alice".into(),
                    username: "alice".into(),
                    kind: UserKind::Patient,
                    display_name: Some("Alice Example".into()),
                    email: Some("alice@example.org".into()),
                    email_verified: true,
                    links: vec![FhirLink {
                        endpoint: "clinic".into(),
                        reference: "Patient/p1".into(),
                        is_fhir_user: true,
                        accessible_patients: vec!["child1".into()],
                    }],
                    external_subject: None,
                    disabled: false,
                },
                Some("correct horse battery"),
                false,
            )
            .await
            .unwrap();
        admin
            .create_user(
                &rid,
                RealmUser {
                    id: "bob".into(),
                    username: "bob".into(),
                    kind: UserKind::Practitioner,
                    display_name: None,
                    email: None,
                    email_verified: false,
                    links: vec![FhirLink {
                        endpoint: "clinic".into(),
                        reference: "Practitioner/dr1".into(),
                        is_fhir_user: true,
                        accessible_patients: vec![],
                    }],
                    external_subject: None,
                    disabled: false,
                },
                None,
                false,
            )
            .await
            .unwrap();
        let server = IdpServer::new(
            rt.clone(),
            IdpServerOptions {
                jwks_fetcher: Some(jwks.clone()),
                jwks_min_refetch: std::time::Duration::ZERO,
                ..Default::default()
            },
        );
        Harness {
            router: server.router(),
            server,
            rt,
            admin,
            realm_id: rid,
            events,
            secret: conf.client_secret.unwrap(),
            rs_secret: rs.client_secret.unwrap(),
            asym_key,
            backend_key,
            jwks,
        }
    }

    pub async fn send(&self, req: Request<Body>) -> Resp {
        let r = self.router.clone().oneshot(req).await.unwrap();
        let status = r.status();
        let headers = r.headers().clone();
        let body = r.into_body().collect().await.unwrap().to_bytes().to_vec();
        Resp {
            status,
            headers,
            body,
        }
    }

    pub async fn get(&self, uri: &str) -> Resp {
        self.send(Request::get(uri).body(Body::empty()).unwrap())
            .await
    }

    pub async fn post(&self, uri: &str, form: &[(&str, &str)], extra: &[(&str, &str)]) -> Resp {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form)
            .finish();
        let mut b =
            Request::post(uri).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        for (k, v) in extra {
            b = b.header(*k, *v);
        }
        self.send(b.body(Body::from(body)).unwrap()).await
    }

    /// Token endpoint POST with optional Basic credentials.
    pub async fn token(&self, form: &[(&str, &str)], basic: Option<(&str, &str)>) -> Resp {
        let auth;
        let mut extra = vec![];
        if let Some((u, p)) = basic {
            use base64::Engine;
            auth = format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"))
            );
            extra.push(("authorization", auth.as_str()));
        }
        self.post("/idp/clinic/token", form, &extra).await
    }

    /// Standard authorize parameters for `client_id`.
    pub fn authz_params(&self, client_id: &str, scope: &str) -> Vec<(String, String)> {
        vec![
            ("response_type".into(), "code".into()),
            ("client_id".into(), client_id.into()),
            ("redirect_uri".into(), REDIRECT.into()),
            ("scope".into(), scope.into()),
            ("state".into(), "st-123".into()),
            ("aud".into(), AUD.into()),
            ("code_challenge".into(), CHALLENGE.into()),
            ("code_challenge_method".into(), "S256".into()),
            ("nonce".into(), "n-456".into()),
        ]
    }

    pub async fn authorize(&self, params: &[(String, String)]) -> Resp {
        let q = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(params)
            .finish();
        self.get(&format!("/idp/clinic/authorize?{q}")).await
    }

    /// `/authorize` → interaction handle (asserts the 303 to the login page).
    pub async fn start(&self, params: &[(String, String)]) -> String {
        let r = self.authorize(params).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.text());
        let loc = url::Url::parse(r.location()).unwrap();
        assert_eq!(loc.path(), "/idp/clinic/login");
        loc.query_pairs()
            .find(|(k, _)| k == "ix")
            .unwrap()
            .1
            .into_owned()
    }

    pub fn decision(&self, subject: &str, scopes: &[&str], patient: Option<&str>) -> Decision {
        Decision {
            subject: subject.into(),
            sid: Some("sid-1".into()),
            session_id_hash: Some("sess-hash".into()),
            auth_time: chrono::Utc::now(),
            amr: vec!["pwd".into()],
            acr: None,
            granted_scopes: scopes.iter().map(|s| s.to_string()).collect(),
            context: LaunchContext {
                patient: patient.map(str::to_string),
                ..Default::default()
            },
            fhir_user: None,
        }
    }

    pub async fn complete(&self, ix: &str, d: Decision) -> AuthorizationResponse {
        self.server
            .completer()
            .complete("clinic", ix, d)
            .await
            .unwrap()
    }

    /// Full code flow for the public client `app`; returns the token JSON.
    pub async fn code_flow(&self, scope: &str, patient: Option<&str>) -> Value {
        let ix = self.start(&self.authz_params("app", scope)).await;
        let scopes: Vec<&str> = scope.split(' ').collect();
        let resp = self
            .complete(&ix, self.decision("alice", &scopes, patient))
            .await;
        let code = resp.param("code").unwrap().to_string();
        let r = self
            .token(
                &[
                    ("grant_type", "authorization_code"),
                    ("code", &code),
                    ("redirect_uri", REDIRECT),
                    ("code_verifier", VERIFIER),
                    ("client_id", "app"),
                ],
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text());
        r.json()
    }

    /// A client assertion for `client_id` signed with `key`.
    pub fn assertion(&self, key: &SigningKey, client_id: &str, claims: Value) -> String {
        let now = chrono::Utc::now().timestamp();
        let mut c = json!({
            "iss": client_id,
            "sub": client_id,
            "aud": format!("{ISS}/token"),
            "exp": now + 240,
            "iat": now,
            "jti": helios_idp::util::random_b64(12),
        });
        for (k, v) in claims.as_object().unwrap() {
            if v.is_null() {
                c.as_object_mut().unwrap().remove(k);
            } else {
                c[k] = v.clone();
            }
        }
        key.sign_jws("JWT", &c).unwrap()
    }

    pub async fn introspect(&self, token: &str) -> Value {
        let r = self
            .post(
                "/idp/clinic/introspect",
                &[("token", token)],
                &[("authorization", &basic("rs", &self.rs_secret))],
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text());
        r.json()
    }
}

pub fn basic(u: &str, p: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"))
    )
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn location(&self) -> &str {
        self.headers
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }
    pub fn header(&self, n: &str) -> &str {
        self.headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }
    /// Query parameters of a redirect.
    pub fn redirect_params(&self) -> HashMap<String, String> {
        url::Url::parse(self.location())
            .map(|u| u.query_pairs().into_owned().collect())
            .unwrap_or_default()
    }
}

pub fn _unused() {
    let _ = pkce::METHOD_S256;
}
