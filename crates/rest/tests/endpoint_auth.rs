//! Per-endpoint FHIR authentication (FHIR server manager P2,
//! `docs/design/fhir-server-manager.md` §6).
//!
//! The whole app (SQLite in memory) runs with the endpoint runtime (package
//! P1's dispatcher: prefix stripped, `ResolvedEndpoint` stored, one tenant
//! per endpoint), exactly as `HFS_MANAGER=on` wires it. Two in-process IdPs
//! (wiremock) sign real RS256 tokens with different keys and issuers.

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum_test::TestServer;
use helios_audit::ExclusionFilter;
use helios_auth::{AuthConfig, AuthError, AuthProvider, Principal, ScopeSet};
use helios_endpoints::model::{AuthMode, ConsoleAccess, EndpointAuth, RateLimitSettings};
use helios_endpoints::{
    Actor, ConsolePrincipal, ConsoleRole, ConsoleSessionResolver, EndpointName, EndpointRegistry,
    EndpointSpec, EndpointStore, FhirRelease, ManagedBy, MemoryEndpointStore,
    ensure_default_endpoint,
};
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use helios_rest::endpoint::EndpointRuntime;
use helios_rest::{AuthMiddlewareState, OperationsBundle, ServerConfig};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const N_1: &str = "zbeWVBfqrPsdF1phb2EB5hxjz1EMb5WT4MnhMatFnNFquEZIcgXG1DBAUICEZLQM8gRj9kZ83YA1cFhUtkgUx1qMio8YkKweLV6r8RAqnuxrS5Khm4a25M85qB5MhGoCLZgGtla9lJ79mjVN5XBqD7sPL0ENN1GPcnus1I6WF-E-5Uiih-QWsQVlGeUqMH81vTBLemDxvMQv-TiKZP3LHH_T41G1FfyF-rlbkj7WXGkbqG_WvR4FKeRm1X2_RRVw87h2j1lfK4AiUKs4h_DqwqZju3fgOH_0MHj2RKCu0wo0cTx8mQGbLCNSb-sRoWRnWGpGPphI9xVS8znuPB1JYQ";
const N_2: &str = "uF_UcxtWC3Z2CJfQzMGUN7VfjqhynYYPL4ftRXnC4Oo5wfB0DG1rg8PEslctm6XyShwP4PBzLyzqsHVWfV1S03mLsEz3LHzr4CF0aiaaZVPV2uKcKMAlA6pD3MARAzXF0Q038XTy_JCcr0jZn98OvxFv9chaz2q38rLn-vyptITbDqfxThLrEbjp4e3Pd-utrTYbGq_BQ9UEpbQ-ONrLBkLvWJMJTbYi6omOrmqxh9IrPuMGavUpCbK_YxkI2P9PQ2dc4iNASK1jq6g8zafscMErY2hxbF20iTN2my7laL0c0GpNVy-822GU8Ooefn2ZfG736Pys9b5-HfQF-YQZEw";
const KEY_1: &[u8] = include_bytes!("../../auth/tests/data/test_rsa_key.pem");
const KEY_2: &[u8] = include_bytes!("../../auth/tests/data/test_rsa_key_2.pem");

const AUTHORIZATION: HeaderName = HeaderName::from_static("authorization");

// ------------------------------------------------------------------ IdPs

/// An in-process IdP: JWKS + OpenID configuration for one realm.
struct Idp {
    server: MockServer,
    issuer: String,
    kid: &'static str,
    key: &'static [u8],
}

impl Idp {
    async fn start(realm: &str, kid: &'static str, n: &str, key: &'static [u8]) -> Self {
        let server = MockServer::start().await;
        let issuer = format!("{}/realms/{realm}", server.uri());
        Mock::given(method("GET"))
            .and(path(format!("/realms/{realm}/protocol/openid-connect/certs")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "keys": [{"kty": "RSA", "kid": kid, "use": "sig", "alg": "RS256", "n": n, "e": "AQAB"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/realms/{realm}/.well-known/openid-configuration"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/protocol/openid-connect/auth"),
                "token_endpoint": format!("{issuer}/protocol/openid-connect/token"),
                "jwks_uri": format!("{issuer}/protocol/openid-connect/certs"),
                "end_session_endpoint": format!("{issuer}/protocol/openid-connect/logout"),
                "revocation_endpoint": format!("{issuer}/protocol/openid-connect/revoke"),
            })))
            .mount(&server)
            .await;
        Self {
            server,
            issuer,
            kid,
            key,
        }
    }

    fn jwks_url(&self) -> String {
        format!("{}/protocol/openid-connect/certs", self.issuer)
    }

    fn sign(&self, claims: Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(self.kid.to_string());
        encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_pem(self.key).unwrap(),
        )
        .unwrap()
    }

    fn token(&self, audience: &str, scope: &str, patient: Option<&str>) -> String {
        let mut claims = json!({
            "sub": format!("client-of-{audience}"),
            "iss": self.issuer,
            "aud": audience,
            "exp": jsonwebtoken::get_current_timestamp() + 600,
            "scope": scope,
            "azp": format!("app-{audience}"),
        });
        if let Some(p) = patient {
            claims["patient"] = json!(p);
        }
        self.sign(claims)
    }

    async fn certs_fetches(&self) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path().ends_with("/certs"))
            .count()
    }

    /// Endpoint auth on this IdP (operator-managed endpoints are trusted, so
    /// the in-process IdP on 127.0.0.1 is reachable).
    fn auth(&self, mode: AuthMode, audience: &str) -> EndpointAuth {
        EndpointAuth {
            mode,
            issuer: Some(self.issuer.clone()),
            jwks_url: Some(self.jwks_url()),
            audience: Some(audience.into()),
            jwks_max_age_secs: Some(300),
            ..EndpointAuth::default()
        }
    }
}

fn spec(name: &str, auth: Option<EndpointAuth>) -> EndpointSpec {
    let mut spec = EndpointSpec::named(EndpointName::parse(name).unwrap(), name);
    spec.auth = auth;
    spec.managed_by = ManagedBy::Env;
    spec
}

// -------------------------------------------------------------- the stack

struct Stack {
    server: TestServer,
    store: Arc<MemoryEndpointStore>,
    registry: Arc<EndpointRegistry>,
}

impl Stack {
    async fn new(
        env_config: AuthConfig,
        env_state: Option<Arc<AuthMiddlewareState>>,
        endpoints: Vec<EndpointSpec>,
    ) -> Self {
        let store = Arc::new(MemoryEndpointStore::new());
        ensure_default_endpoint(store.as_ref(), "default", FhirRelease::R4)
            .await
            .unwrap();
        for spec in endpoints {
            store.create(spec, &Actor::bootstrap()).await.unwrap();
        }
        let registry = Arc::new(EndpointRegistry::load(store.as_ref()).await.unwrap());
        // The spec SearchParameters (compartment membership) live in data/.
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
            env_config,
            env_state,
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
        let server = TestServer::new(app).unwrap();
        Self {
            server,
            store,
            registry,
        }
    }

    /// Changes an endpoint and reloads the registry (hot reload).
    async fn edit(&self, name: &str, change: impl FnOnce(&mut EndpointSpec)) {
        let current = self.store.get(name).await.unwrap().unwrap();
        let mut spec = current.spec.clone();
        change(&mut spec);
        self.store
            .update(name, current.revision, spec, &Actor::bootstrap())
            .await
            .unwrap();
        assert!(
            self.registry
                .reload_if_stale(self.store.as_ref())
                .await
                .unwrap()
        );
    }

    async fn get(&self, path: &str, token: Option<&str>) -> axum_test::TestResponse {
        let mut req = self.server.get(path);
        if let Some(t) = token {
            req = req.add_header(AUTHORIZATION, bearer(t));
        }
        req.await
    }

    async fn post_patient(&self, path: &str, token: Option<&str>) -> axum_test::TestResponse {
        let mut req = self
            .server
            .post(path)
            .add_header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/fhir+json"),
            )
            .json(&json!({"resourceType": "Patient", "name": [{"family": "Lin"}]}));
        if let Some(t) = token {
            req = req.add_header(AUTHORIZATION, bearer(t));
        }
        req.await
    }

    async fn put(&self, path: &str, token: &str, body: Value) {
        let r = self
            .server
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
}

fn bearer(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("Bearer {token}")).unwrap()
}

fn challenge(r: &axum_test::TestResponse) -> String {
    r.headers()
        .get(header::WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// Two IdPs and five endpoints: `alpha` (IdP A, full, aud api-alpha),
/// `gamma` (IdP A, full, aud api-gamma), `beta` (IdP B, write-protect,
/// aud api-beta), `open` (auth explicitly off), `strict` (IdP A, full, aud
/// api-strict, no patient/ scopes, no SMART v1). The environment's auth is
/// off, so the default endpoint (auth inherited) is open.
async fn two_idps() -> (Stack, Idp, Idp) {
    let a = Idp::start("a", "key-a", N_1, KEY_1).await;
    let b = Idp::start("b", "key-b", N_2, KEY_2).await;
    let mut strict = a.auth(AuthMode::Full, "api-strict");
    strict.scopes.allow_patient = false;
    strict.scopes.accept_v1 = false;
    let stack = Stack::new(
        AuthConfig::default(),
        None,
        vec![
            spec("alpha", Some(a.auth(AuthMode::Full, "api-alpha"))),
            spec("gamma", Some(a.auth(AuthMode::Full, "api-gamma"))),
            spec("beta", Some(b.auth(AuthMode::WriteProtect, "api-beta"))),
            spec("open", Some(EndpointAuth::default())),
            spec("strict", Some(strict)),
        ],
    )
    .await;
    (stack, a, b)
}

// ------------------------------------------------------------------ tests

#[tokio::test]
async fn a_token_for_one_endpoint_is_refused_by_the_others() {
    let (s, a, b) = two_idps().await;
    let alpha = a.token("api-alpha", "system/*.cruds", None);

    let ok = s.get("/alpha/Patient", Some(&alpha)).await;
    assert_eq!(ok.status_code(), StatusCode::OK, "{}", ok.text());

    // Same IdP, other audience.
    let gamma = s.get("/gamma/Patient", Some(&alpha)).await;
    assert_eq!(
        gamma.status_code(),
        StatusCode::UNAUTHORIZED,
        "{}",
        gamma.text()
    );
    assert!(
        challenge(&gamma).contains("error=\"invalid_token\""),
        "{}",
        challenge(&gamma)
    );
    assert!(gamma.text().contains("audience"), "{}", gamma.text());

    // Other IdP (write-protect: a presented token is validated anyway).
    let beta = s.get("/beta/Patient", Some(&alpha)).await;
    assert_eq!(
        beta.status_code(),
        StatusCode::UNAUTHORIZED,
        "{}",
        beta.text()
    );

    // And B's token is refused on A's endpoints.
    let b_token = b.token("api-alpha", "system/*.cruds", None);
    let r = s.get("/alpha/Patient", Some(&b_token)).await;
    assert_eq!(r.status_code(), StatusCode::UNAUTHORIZED, "{}", r.text());

    // The default endpoint inherits the environment (auth off): open.
    assert_eq!(s.get("/Patient", None).await.status_code(), StatusCode::OK);
}

#[tokio::test]
async fn full_mode_needs_a_token_except_for_discovery() {
    let (s, a, _b) = two_idps().await;
    let r = s.get("/alpha/Patient", None).await;
    assert_eq!(r.status_code(), StatusCode::UNAUTHORIZED);
    assert_eq!(challenge(&r), "Bearer realm=\"hfs\"");

    let meta = s.get("/alpha/metadata", None).await;
    assert_eq!(meta.status_code(), StatusCode::OK, "{}", meta.text());
    let meta: Value = meta.json();
    let security = &meta["rest"][0]["security"];
    assert_eq!(
        security["service"][0]["coding"][0]["code"], "SMART-on-FHIR",
        "{security}"
    );
    let uris = security["extension"][0]["extension"].as_array().unwrap();
    assert!(uris.iter().any(|u| u["url"] == "authorize"
        && u["valueUri"] == format!("{}/protocol/openid-connect/auth", a.issuer)));
    assert!(uris.iter().any(|u| u["url"] == "revoke"));

    let smart = s.get("/alpha/.well-known/smart-configuration", None).await;
    assert_eq!(smart.status_code(), StatusCode::OK);
    let smart: Value = smart.json();
    assert_eq!(smart["issuer"], a.issuer);
    assert_eq!(
        smart["token_endpoint"],
        format!("{}/protocol/openid-connect/token", a.issuer)
    );
    assert_eq!(smart["jwks_uri"], a.jwks_url());
    assert!(smart["end_session_endpoint"].is_string());
    assert!(
        smart["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("launch-standalone"))
    );
}

#[tokio::test]
async fn discovery_and_security_follow_each_endpoint() {
    let (s, _a, b) = two_idps().await;
    // B's endpoint describes B.
    let smart: Value = s
        .get("/beta/.well-known/smart-configuration", None)
        .await
        .json();
    assert_eq!(smart["issuer"], b.issuer);
    // auth off → no SMART document, cors-only security.
    let r = s.get("/open/.well-known/smart-configuration", None).await;
    assert_eq!(r.status_code(), StatusCode::NOT_FOUND);
    let meta: Value = s.get("/open/metadata", None).await.json();
    assert!(meta["rest"][0]["security"].get("service").is_none());
    // The default endpoint inherits the environment: today's minimal document.
    let root = s.get("/.well-known/smart-configuration", None).await;
    assert_eq!(root.status_code(), StatusCode::OK);
    assert!(root.json::<Value>().get("issuer").is_none());
}

#[tokio::test]
async fn write_protect_reads_anonymously_and_checks_scopes_on_writes() {
    let (s, _a, b) = two_idps().await;
    assert_eq!(
        s.get("/beta/Patient", None).await.status_code(),
        StatusCode::OK
    );
    let anon = s.post_patient("/beta/Patient", None).await;
    assert_eq!(
        anon.status_code(),
        StatusCode::UNAUTHORIZED,
        "{}",
        anon.text()
    );

    let reader = b.token("api-beta", "system/*.rs", None);
    let r = s.post_patient("/beta/Patient", Some(&reader)).await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN, "{}", r.text());
    assert!(
        challenge(&r).contains("insufficient_scope"),
        "{}",
        challenge(&r)
    );

    let writer = b.token("api-beta", "system/*.cruds", None);
    let r = s.post_patient("/beta/Patient", Some(&writer)).await;
    assert_eq!(r.status_code(), StatusCode::CREATED, "{}", r.text());

    // Auth explicitly off: anyone writes, a token is ignored.
    let r = s.post_patient("/open/Patient", Some("not-a-token")).await;
    assert_eq!(r.status_code(), StatusCode::CREATED, "{}", r.text());
}

#[tokio::test]
async fn endpoints_on_one_idp_share_its_key_fetcher() {
    let (s, a, _b) = two_idps().await;
    let r = s
        .get(
            "/alpha/Patient",
            Some(&a.token("api-alpha", "system/*.rs", None)),
        )
        .await;
    assert_eq!(r.status_code(), StatusCode::OK, "{}", r.text());
    let fetches = a.certs_fetches().await;
    assert!(fetches >= 1);
    // gamma and strict name the same issuer and JWKS: no new fetch.
    let r = s
        .get(
            "/gamma/Patient",
            Some(&a.token("api-gamma", "system/*.rs", None)),
        )
        .await;
    assert_eq!(r.status_code(), StatusCode::OK, "{}", r.text());
    let r = s
        .get(
            "/strict/Patient",
            Some(&a.token("api-strict", "system/*.rs", None)),
        )
        .await;
    assert_eq!(r.status_code(), StatusCode::OK, "{}", r.text());
    assert_eq!(
        a.certs_fetches().await,
        fetches,
        "keys fetched once per IdP"
    );
}

#[tokio::test]
async fn patient_tokens_are_confined_per_endpoint_and_refused_where_not_allowed() {
    let (s, a, _b) = two_idps().await;
    // Each endpoint has its own tenant: seed both with a system token.
    for (base, audience) in [("/alpha", "api-alpha"), ("/strict", "api-strict")] {
        let system = a.token(audience, "system/*.cruds", None);
        for p in ["p1", "p2"] {
            s.put(
                &format!("{base}/Patient/{p}"),
                &system,
                json!({"resourceType": "Patient", "id": p}),
            )
            .await;
            s.put(
                &format!("{base}/Observation/o-{p}"),
                &system,
                json!({"resourceType": "Observation", "id": format!("o-{p}"), "status": "final",
                       "code": {"text": "x"}, "subject": {"reference": format!("Patient/{p}")}}),
            )
            .await;
        }
    }
    // ... and not the default endpoint's.
    assert_eq!(
        s.get("/Patient/p1", None).await.status_code(),
        StatusCode::NOT_FOUND
    );
    let p1 = a.token("api-alpha", "patient/*.rs", Some("p1"));
    let own = s.get("/alpha/Observation", Some(&p1)).await;
    assert_eq!(own.status_code(), StatusCode::OK, "{}", own.text());
    let text = own.text();
    assert!(text.contains("o-p1") && !text.contains("o-p2"), "{text}");
    let other = s.get("/alpha/Patient/p2", Some(&p1)).await;
    assert!(
        matches!(
            other.status_code(),
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ),
        "{} {}",
        other.status_code(),
        other.text()
    );
    assert_eq!(
        s.get("/alpha/Patient/p1", Some(&p1)).await.status_code(),
        StatusCode::OK
    );

    // strict refuses patient/ scopes altogether.
    let p1_strict = a.token("api-strict", "patient/*.rs", Some("p1"));
    let r = s.get("/strict/Observation", Some(&p1_strict)).await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN, "{}", r.text());
    assert!(
        r.text().contains("patient/ scopes are not accepted"),
        "{}",
        r.text()
    );
    // ... and SMART v1 scopes.
    let v1 = a.token("api-strict", "system/*.read", None);
    let r = s.get("/strict/Observation", Some(&v1)).await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN, "{}", r.text());
    assert!(r.text().contains("SMART v1"), "{}", r.text());
    // A mixed token keeps what the endpoint accepts.
    let mixed = a.token(
        "api-strict",
        "openid patient/*.rs system/Observation.rs",
        Some("p1"),
    );
    let r = s.get("/strict/Observation", Some(&mixed)).await;
    assert_eq!(r.status_code(), StatusCode::OK, "{}", r.text());
    assert!(
        r.text().contains("o-p2"),
        "unconfined system scope: {}",
        r.text()
    );
}

#[tokio::test]
async fn configuration_changes_apply_on_the_next_request() {
    let (s, a, _b) = two_idps().await;
    // beta: write-protect → off.
    assert_eq!(
        s.post_patient("/beta/Patient", None).await.status_code(),
        StatusCode::UNAUTHORIZED
    );
    s.edit("beta", |spec| {
        spec.auth.as_mut().unwrap().mode = AuthMode::Off
    })
    .await;
    assert_eq!(
        s.post_patient("/beta/Patient", None).await.status_code(),
        StatusCode::CREATED
    );
    // alpha: new audience → the old token is refused, a new one accepted.
    let old = a.token("api-alpha", "system/*.rs", None);
    assert_eq!(
        s.get("/alpha/Patient", Some(&old)).await.status_code(),
        StatusCode::OK
    );
    s.edit("alpha", |spec| {
        spec.auth.as_mut().unwrap().audience = Some("api-alpha-v2".into())
    })
    .await;
    assert_eq!(
        s.get("/alpha/Patient", Some(&old)).await.status_code(),
        StatusCode::UNAUTHORIZED
    );
    let new = a.token("api-alpha-v2", "system/*.rs", None);
    assert_eq!(
        s.get("/alpha/Patient", Some(&new)).await.status_code(),
        StatusCode::OK
    );
    // inherit the environment again (auth off there).
    s.edit("alpha", |spec| spec.auth = None).await;
    assert_eq!(
        s.get("/alpha/Patient", None).await.status_code(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn rate_limits_are_per_endpoint() {
    let a = Idp::start("a", "key-a", N_1, KEY_1).await;
    let mut limited = spec("limited", Some(EndpointAuth::default()));
    limited.rate_limit = Some(RateLimitSettings {
        anonymous_per_minute: 1,
        authenticated_per_minute: 0,
        burst_seconds: 1,
    });
    let mut tokens = spec("tokens", Some(a.auth(AuthMode::WriteProtect, "api-t")));
    tokens.rate_limit = Some(RateLimitSettings {
        anonymous_per_minute: 0,
        authenticated_per_minute: 1,
        burst_seconds: 1,
    });
    let s = Stack::new(AuthConfig::default(), None, vec![limited, tokens]).await;
    assert_eq!(
        s.get("/limited/Patient", None).await.status_code(),
        StatusCode::OK
    );
    let r = s.get("/limited/Patient", None).await;
    assert_eq!(
        r.status_code(),
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        r.text()
    );
    assert!(r.headers().get(header::RETRY_AFTER).is_some());
    // Health and other endpoints are not billed to it.
    assert_eq!(
        s.get("/limited/health", None).await.status_code(),
        StatusCode::OK
    );
    for _ in 0..3 {
        assert_eq!(s.get("/Patient", None).await.status_code(), StatusCode::OK);
        assert_eq!(
            s.get("/tokens/Patient", None).await.status_code(),
            StatusCode::OK
        );
    }
    // Authenticated clients are billed per client.
    let t = a.token("api-t", "system/*.rs", None);
    assert_eq!(
        s.get("/tokens/Patient", Some(&t)).await.status_code(),
        StatusCode::OK
    );
    assert_eq!(
        s.get("/tokens/Patient", Some(&t)).await.status_code(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn an_unreachable_idp_answers_503_and_an_unknown_endpoint_404() {
    let a = Idp::start("a", "key-a", N_1, KEY_1).await;
    let mut down = a.auth(AuthMode::Full, "api-down");
    down.jwks_url = Some("http://127.0.0.1:9/certs".into());
    let s = Stack::new(AuthConfig::default(), None, vec![spec("down", Some(down))]).await;
    let r = s
        .get(
            "/down/Patient",
            Some(&a.token("api-down", "system/*.rs", None)),
        )
        .await;
    assert_eq!(
        r.status_code(),
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        r.text()
    );
    assert!(r.headers().get(header::RETRY_AFTER).is_some());
}

#[tokio::test]
async fn console_edited_endpoints_fetch_through_the_outbound_policy() {
    // Without HFS_OUTBOUND_ALLOW, an endpoint created in the console (not
    // operator-managed) may not reach an IdP on the loopback interface.
    let a = Idp::start("a", "key-a", N_1, KEY_1).await;
    let mut edited = spec("edited", Some(a.auth(AuthMode::Full, "api-ui")));
    edited.managed_by = ManagedBy::Ui;
    let s = Stack::new(AuthConfig::default(), None, vec![edited]).await;
    let r = s
        .get(
            "/edited/Patient",
            Some(&a.token("api-ui", "system/*.rs", None)),
        )
        .await;
    assert_eq!(
        r.status_code(),
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        r.text()
    );
    assert_eq!(a.certs_fetches().await, 0, "the IdP was never contacted");
}

// -------------------------------------------------------- env inheritance

const ENV_TOKEN: &str = "env-token";

struct EnvProvider;

#[async_trait]
impl AuthProvider for EnvProvider {
    async fn authenticate(&self, header: &str) -> Result<Principal, AuthError> {
        if header != format!("Bearer {ENV_TOKEN}") {
            return Err(AuthError::InvalidSignature);
        }
        Ok(Principal {
            subject: "env-client".into(),
            issuer: "https://env-idp.example".into(),
            scopes: ScopeSet::parse("system/*.cruds"),
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "env"
    }
}

fn env_write_protect() -> (AuthConfig, Arc<AuthMiddlewareState>) {
    let config = AuthConfig {
        enabled: true,
        mode: helios_auth::AuthMode::WriteProtect,
        ..AuthConfig::default()
    };
    let state = Arc::new(AuthMiddlewareState {
        provider: Arc::new(EnvProvider),
        config: Arc::new(config.clone()),
        audit_sink: Arc::new(helios_audit::sinks::NullSink),
        audit_source_observer: "Device/test".into(),
        audit_exclusion_filter: ExclusionFilter::new(vec![]),
        tenant_url_routing: false,
        sessions: None,
        admin: None,
    });
    (config, state)
}

#[tokio::test]
async fn the_default_endpoint_keeps_the_environment_auth() {
    let a = Idp::start("a", "key-a", N_1, KEY_1).await;
    let (config, state) = env_write_protect();
    let s = Stack::new(
        config,
        Some(state),
        vec![
            spec("inherits", None),
            spec("alpha", Some(a.auth(AuthMode::Full, "api-alpha"))),
        ],
    )
    .await;
    for base in ["", "/inherits"] {
        assert_eq!(
            s.get(&format!("{base}/Patient"), None).await.status_code(),
            StatusCode::OK
        );
        assert_eq!(
            s.post_patient(&format!("{base}/Patient"), None)
                .await
                .status_code(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            s.post_patient(&format!("{base}/Patient"), Some(ENV_TOKEN))
                .await
                .status_code(),
            StatusCode::CREATED
        );
    }
    // The environment's credential is no credential on an endpoint with its
    // own IdP, and the endpoint's token is none on the default endpoint.
    assert_eq!(
        s.get("/alpha/Patient", Some(ENV_TOKEN)).await.status_code(),
        StatusCode::UNAUTHORIZED
    );
    let alpha = a.token("api-alpha", "system/*.cruds", None);
    assert_eq!(
        s.post_patient("/Patient", Some(&alpha)).await.status_code(),
        StatusCode::UNAUTHORIZED
    );
}

// ------------------------------------------------------- console sessions

struct FakeConsole;

#[async_trait]
impl ConsoleSessionResolver for FakeConsole {
    async fn resolve(&self, cookie_header: &str) -> Option<ConsolePrincipal> {
        let role = if cookie_header.contains("hfs_console=editor") {
            ConsoleRole::Editor
        } else if cookie_header.contains("hfs_console=viewer") {
            ConsoleRole::Viewer
        } else {
            return None;
        };
        Some(ConsolePrincipal {
            subject: format!("local:{}", role.as_str()),
            display_name: role.as_str().into(),
            role,
            endpoint_roles: Default::default(),
            via: helios_endpoints::console::ConsoleVia::Local,
        })
    }
}

#[tokio::test]
async fn a_console_session_acts_on_the_fhir_api_by_role_and_console_access() {
    helios_rest::endpoint::auth::install_console_resolver(Arc::new(FakeConsole));
    let a = Idp::start("a", "key-a", N_1, KEY_1).await;
    let mut closed = spec("closed", Some(a.auth(AuthMode::Full, "api-closed")));
    closed.console_access = ConsoleAccess::Off;
    let mut ro = spec("ro", Some(a.auth(AuthMode::Full, "api-ro")));
    ro.console_access = ConsoleAccess::ReadOnly;
    let s = Stack::new(
        AuthConfig::default(),
        None,
        vec![
            spec("alpha", Some(a.auth(AuthMode::Full, "api-alpha"))),
            closed,
            ro,
        ],
    )
    .await;
    let cookie = |who: &str| HeaderValue::from_str(&format!("hfs_console={who}")).unwrap();
    let get = |p: &'static str, who: &'static str| {
        s.server.get(p).add_header(header::COOKIE, cookie(who))
    };
    let post = |p: &'static str, who: &'static str| {
        s.server
            .post(p)
            .add_header(header::COOKIE, cookie(who))
            .add_header(header::HOST, HeaderValue::from_static("hfs.test"))
            .add_header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/fhir+json"),
            )
            .json(&json!({"resourceType": "Patient"}))
    };

    assert_eq!(
        get("/alpha/Patient", "viewer").await.status_code(),
        StatusCode::OK
    );
    assert_eq!(
        post("/alpha/Patient", "viewer")
            .add_header(header::ORIGIN, HeaderValue::from_static("http://hfs.test"))
            .await
            .status_code(),
        StatusCode::FORBIDDEN,
        "a viewer reads only"
    );
    let r = post("/alpha/Patient", "editor")
        .add_header(header::ORIGIN, HeaderValue::from_static("http://hfs.test"))
        .await;
    assert_eq!(r.status_code(), StatusCode::CREATED, "{}", r.text());
    let r = post("/alpha/Patient", "editor")
        .add_header(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        )
        .await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN, "cross-site write");
    let r = post("/alpha/Patient", "editor").await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN, "no Origin: refused");

    // consoleAccess=off: the session is no credential.
    assert_eq!(
        get("/closed/Patient", "editor").await.status_code(),
        StatusCode::UNAUTHORIZED
    );
    // consoleAccess=read-only: even an editor only reads.
    assert_eq!(
        get("/ro/Patient", "editor").await.status_code(),
        StatusCode::OK
    );
    assert_eq!(
        post("/ro/Patient", "editor")
            .add_header(header::ORIGIN, HeaderValue::from_static("http://hfs.test"))
            .await
            .status_code(),
        StatusCode::FORBIDDEN
    );
    // An unknown cookie is anonymous.
    assert_eq!(
        get("/alpha/Patient", "nobody").await.status_code(),
        StatusCode::UNAUTHORIZED
    );
}
