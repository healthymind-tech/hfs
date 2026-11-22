//! FHIR endpoints whose auth provider is an internal realm of the built-in
//! authorization server (docs/design/authorization-server.md §5.2, D-3,
//! package I5), through the whole app with the endpoint runtime.
//!
//! The realm source is installed once per test binary (process-wide, as
//! the `hfs` binary does with `HFS_IDP=on`); every test uses its own realm
//! names so they can run in parallel. The "IdP off" case lives in
//! `endpoint_internal_realm_off.rs` (a separate process).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum_test::TestServer;
use chrono::Utc;
use helios_auth::{AuthConfig, MemoryRealmSource, RealmTrust, TrustedKey};
use helios_endpoints::model::{AuthMode, AuthProviderRef, EndpointAuth};
use helios_endpoints::{
    Actor, EndpointName, EndpointRegistry, EndpointSpec, EndpointStore, FhirRelease, ManagedBy,
    MemoryEndpointStore, ensure_default_endpoint,
};
use helios_idp::keys::{RsaBits, SigningAlg, SigningKey};
use helios_idp::launch::LaunchContext;
use helios_idp::token::{AccessTokenClaims, AccessTokenInput};
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use helios_rest::endpoint::EndpointRuntime;
use helios_rest::{OperationsBundle, ServerConfig};
use serde_json::{Value, json};

const AUTHORIZATION: HeaderName = HeaderName::from_static("authorization");

fn source() -> Arc<MemoryRealmSource> {
    static SOURCE: OnceLock<Arc<MemoryRealmSource>> = OnceLock::new();
    SOURCE
        .get_or_init(|| {
            let s = Arc::new(MemoryRealmSource::new());
            assert!(helios_auth::install_internal_realms(s.clone()));
            s
        })
        .clone()
}

struct Realm {
    name: String,
    issuer: String,
    key: SigningKey,
}

impl Realm {
    /// Publishes a realm with one ES256 key, bound to `endpoints`.
    fn publish(name: &str, endpoints: &[&str]) -> Self {
        let key = SigningKey::generate(SigningAlg::ES256, RsaBits::default()).unwrap();
        let issuer = format!("https://hfs.test/idp/{name}");
        let (kid, tk) = TrustedKey::from_public_jwk(key.public_jwk()).unwrap();
        let mut smart = HashMap::new();
        for e in endpoints {
            smart.insert(
                e.to_string(),
                json!({
                    "issuer": issuer,
                    "jwks_uri": format!("{issuer}/jwks"),
                    "authorization_endpoint": format!("{issuer}/authorize"),
                    "token_endpoint": format!("{issuer}/token"),
                    "revocation_endpoint": format!("{issuer}/revoke"),
                    "capabilities": ["launch-standalone", "context-standalone-patient"],
                    "code_challenge_methods_supported": ["S256"],
                    "grant_types_supported": ["authorization_code"],
                }),
            );
        }
        source().set_realm(
            name,
            RealmTrust {
                issuer: issuer.clone(),
                keys: HashMap::from([(kid, tk)]),
                audiences: endpoints.iter().map(|e| (e.to_string(), aud(e))).collect(),
                smart_configuration: smart,
            },
        );
        Self {
            name: name.into(),
            issuer,
            key,
        }
    }

    fn token(&self, endpoint: &str, scope: &str, patient: Option<&str>, gid: &str) -> String {
        let claims = AccessTokenClaims::build(
            AccessTokenInput {
                issuer: &self.issuer,
                subject: "user-1",
                audience: vec![aud(endpoint)],
                client_id: "app",
                scope,
                ttl_secs: 300,
                auth_time: None,
                fhir_user: None,
                context: LaunchContext {
                    patient: patient.map(String::from),
                    ..Default::default()
                },
                cnf: None,
                grant_id: gid,
            },
            Utc::now(),
        );
        self.key.sign_jws("at+jwt", &claims).unwrap()
    }
}

fn aud(endpoint: &str) -> String {
    format!("https://hfs.test/fhir/{endpoint}")
}

fn internal(realm: &str) -> EndpointAuth {
    EndpointAuth {
        mode: AuthMode::Full,
        provider: AuthProviderRef::Internal {
            realm: realm.into(),
        },
        ..EndpointAuth::default()
    }
}

fn spec(name: &str, auth: EndpointAuth) -> EndpointSpec {
    let mut spec = EndpointSpec::named(EndpointName::parse(name).unwrap(), name);
    spec.auth = Some(auth);
    spec.managed_by = ManagedBy::Env;
    spec
}

async fn stack(endpoints: Vec<EndpointSpec>) -> TestServer {
    let store = Arc::new(MemoryEndpointStore::new());
    ensure_default_endpoint(store.as_ref(), "default", FhirRelease::R4)
        .await
        .unwrap();
    for spec in endpoints {
        store.create(spec, &Actor::bootstrap()).await.unwrap();
    }
    let registry = Arc::new(EndpointRegistry::load(store.as_ref()).await.unwrap());
    let backend = Arc::new(
        SqliteBackend::with_config(
            ":memory:",
            SqliteBackendConfig {
                data_dir: Some(
                    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data"),
                ),
                ..Default::default()
            },
        )
        .expect("sqlite"),
    );
    backend.init_schema().expect("schema");
    let app = helios_rest::create_app_with_auth_bulk_settings_and_ops(
        backend,
        ServerConfig::for_testing(),
        AuthConfig::default(),
        None,
        None,
        None,
        None,
        None,
        OperationsBundle {
            endpoints: Some(EndpointRuntime::new(
                Arc::clone(&registry),
                Arc::clone(&store) as Arc<dyn EndpointStore>,
            )),
            ..Default::default()
        },
    );
    TestServer::new(app).unwrap()
}

fn bearer(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("Bearer {token}")).unwrap()
}

async fn get(s: &TestServer, path: &str, token: Option<&str>) -> axum_test::TestResponse {
    let mut req = s.get(path);
    if let Some(t) = token {
        req = req.add_header(AUTHORIZATION, bearer(t));
    }
    req.await
}

async fn put(s: &TestServer, path: &str, token: &str, body: Value) {
    let r = s
        .put(path)
        .add_header(AUTHORIZATION, bearer(token))
        .add_header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/fhir+json"),
        )
        .json(&body)
        .await;
    assert!(r.status_code().is_success(), "{path}: {}", r.text());
}

#[tokio::test]
async fn accepts_its_realm_only_and_revocation_is_immediate() {
    let a = Realm::publish("rs-a", &["ia"]);
    let b = Realm::publish("rs-b", &["ib"]);
    let s = stack(vec![
        spec("ia", internal(&a.name)),
        spec("ib", internal(&b.name)),
    ])
    .await;

    assert_eq!(
        get(&s, "/ia/Patient", None).await.status_code(),
        StatusCode::UNAUTHORIZED
    );
    let ok = a.token("ia", "system/*.rs", None, "g-a1");
    let r = get(&s, "/ia/Patient", Some(&ok)).await;
    assert_eq!(r.status_code(), StatusCode::OK, "{}", r.text());

    // Realm B's token (right audience shape, other realm) is refused.
    let foreign = b.token("ia", "system/*.rs", None, "g-b1");
    assert_eq!(
        get(&s, "/ia/Patient", Some(&foreign)).await.status_code(),
        StatusCode::UNAUTHORIZED
    );
    // Realm A's token for endpoint ib is refused by ib (other realm) and
    // realm A's token for ia is refused by ib.
    assert_eq!(
        get(&s, "/ib/Patient", Some(&ok)).await.status_code(),
        StatusCode::UNAUTHORIZED
    );

    source().revoke_grant(&a.name, "g-a1");
    let r = get(&s, "/ia/Patient", Some(&ok)).await;
    assert_eq!(r.status_code(), StatusCode::UNAUTHORIZED, "{}", r.text());
    let fresh = a.token("ia", "system/*.rs", None, "g-a2");
    assert_eq!(
        get(&s, "/ia/Patient", Some(&fresh)).await.status_code(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn patient_tokens_are_confined_to_the_launch_patient() {
    let a = Realm::publish("rs-pc", &["pc"]);
    let s = stack(vec![spec("pc", internal(&a.name))]).await;
    let system = a.token("pc", "system/*.cruds", None, "g-sys");
    for p in ["p1", "p2"] {
        put(
            &s,
            &format!("/pc/Patient/{p}"),
            &system,
            json!({"resourceType": "Patient", "id": p}),
        )
        .await;
        put(
            &s,
            &format!("/pc/Observation/o-{p}"),
            &system,
            json!({"resourceType": "Observation", "id": format!("o-{p}"), "status": "final",
                   "code": {"text": "x"}, "subject": {"reference": format!("Patient/{p}")}}),
        )
        .await;
    }
    let p1 = a.token("pc", "patient/*.rs", Some("p1"), "g-p1");
    let own = get(&s, "/pc/Observation", Some(&p1)).await;
    assert_eq!(own.status_code(), StatusCode::OK, "{}", own.text());
    let text = own.text();
    assert!(text.contains("o-p1") && !text.contains("o-p2"), "{text}");
    let other = get(&s, "/pc/Patient/p2", Some(&p1)).await;
    assert!(
        matches!(
            other.status_code(),
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ),
        "{}",
        other.status_code()
    );
    // Writes need a write scope.
    let r = s
        .put("/pc/Patient/p1")
        .add_header(AUTHORIZATION, bearer(&p1))
        .add_header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/fhir+json"),
        )
        .json(&json!({"resourceType": "Patient", "id": "p1"}))
        .await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN, "{}", r.text());
}

#[tokio::test]
async fn discovery_comes_from_the_realm_and_an_unknown_realm_is_closed() {
    let a = Realm::publish("rs-d", &["disc"]);
    let s = stack(vec![
        spec("disc", internal(&a.name)),
        spec("ghost", internal("rs-does-not-exist")),
    ])
    .await;
    let r = get(&s, "/disc/.well-known/smart-configuration", None).await;
    assert_eq!(r.status_code(), StatusCode::OK);
    let doc: Value = r.json();
    assert_eq!(doc["issuer"], a.issuer);
    assert_eq!(
        doc["authorization_endpoint"],
        format!("{}/authorize", a.issuer)
    );
    assert_eq!(doc["code_challenge_methods_supported"], json!(["S256"]));
    assert!(
        doc["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "launch-standalone")
    );

    // A token for a realm that is not loaded: closed with 503, never open.
    let t = a.token("ghost", "system/*.rs", None, "g-x");
    let r = get(&s, "/ghost/Patient", Some(&t)).await;
    assert_eq!(
        r.status_code(),
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        r.text()
    );
    assert_eq!(
        get(&s, "/ghost/Patient", None).await.status_code(),
        StatusCode::UNAUTHORIZED
    );
}
