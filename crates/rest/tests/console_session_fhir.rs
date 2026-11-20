//! Console sessions on the FHIR API end to end (P2 × P3, design D-10): the
//! **real** console runtime (`helios_admin_auth::console::ConsoleAuth`) is
//! installed as P2's console session resolver, and a console session cookie
//! acts on an endpoint with `role_for(endpoint).data_scopes(consoleAccess)`:
//! viewers read, editors and admins write (same-site `Origin` required),
//! per-endpoint bindings apply, `consoleAccess` limits it, a signed-out
//! session is no credential.

use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum_test::TestServer;
use helios_admin_auth::console::testing::{env, runtime, session};
use helios_admin_auth::console::{ConsoleAuth, ConsoleOptions};
use helios_auth::AuthConfig;
use helios_endpoints::console::{ConsoleStore, ConsoleVia};
use helios_endpoints::model::{AuthMode, ConsoleAccess, EndpointAuth};
use helios_endpoints::{
    Actor, ConsoleRole, EndpointName, EndpointRegistry, EndpointSpec, EndpointStore, FhirRelease,
    ManagedBy, MemoryEndpointStore, ensure_default_endpoint,
};
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use helios_rest::endpoint::EndpointRuntime;
use helios_rest::{OperationsBundle, ServerConfig};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const N_1: &str = "zbeWVBfqrPsdF1phb2EB5hxjz1EMb5WT4MnhMatFnNFquEZIcgXG1DBAUICEZLQM8gRj9kZ83YA1cFhUtkgUx1qMio8YkKweLV6r8RAqnuxrS5Khm4a25M85qB5MhGoCLZgGtla9lJ79mjVN5XBqD7sPL0ENN1GPcnus1I6WF-E-5Uiih-QWsQVlGeUqMH81vTBLemDxvMQv-TiKZP3LHH_T41G1FfyF-rlbkj7WXGkbqG_WvR4FKeRm1X2_RRVw87h2j1lfK4AiUKs4h_DqwqZju3fgOH_0MHj2RKCu0wo0cTx8mQGbLCNSb-sRoWRnWGpGPphI9xVS8znuPB1JYQ";

/// Endpoint auth on an in-process IdP (no token is ever sent here: the
/// console cookie is the only credential).
async fn idp() -> (MockServer, String) {
    let server = MockServer::start().await;
    let issuer = format!("{}/realms/a", server.uri());
    Mock::given(method("GET"))
        .and(path("/realms/a/protocol/openid-connect/certs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "keys": [{"kty": "RSA", "kid": "k", "use": "sig", "alg": "RS256", "n": N_1, "e": "AQAB"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/realms/a/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/auth"),
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": format!("{issuer}/protocol/openid-connect/certs"),
        })))
        .mount(&server)
        .await;
    (server, issuer)
}

fn spec(name: &str, issuer: &str, access: ConsoleAccess) -> EndpointSpec {
    let mut spec = EndpointSpec::named(EndpointName::parse(name).unwrap(), name);
    spec.auth = Some(EndpointAuth {
        mode: AuthMode::Full,
        issuer: Some(issuer.into()),
        jwks_url: Some(format!("{issuer}/protocol/openid-connect/certs")),
        audience: Some(format!("api-{name}")),
        jwks_max_age_secs: Some(300),
        ..EndpointAuth::default()
    });
    spec.console_access = access;
    spec.managed_by = ManagedBy::Env;
    spec
}

async fn app(issuer: &str) -> TestServer {
    let store = Arc::new(MemoryEndpointStore::new());
    ensure_default_endpoint(store.as_ref(), "default", FhirRelease::R4)
        .await
        .unwrap();
    for s in [
        spec("alpha", issuer, ConsoleAccess::RoleBased),
        spec("ro", issuer, ConsoleAccess::ReadOnly),
        spec("closed", issuer, ConsoleAccess::Off),
    ] {
        store.create(s, &Actor::bootstrap()).await.unwrap();
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

async fn console() -> (
    Arc<ConsoleAuth>,
    Arc<helios_admin_auth::console::memory::MemoryConsoleStore>,
) {
    runtime(
        env(&[
            ("HFS_CONSOLE_AUTH_MODE", "oidc"),
            ("HFS_CONSOLE_OIDC_ISSUER", "http://127.0.0.1:1"),
            ("HFS_CONSOLE_OIDC_CLIENT_ID", "hfs-web"),
        ]),
        None,
        ConsoleOptions {
            public_base_url: "http://hfs.test".into(),
            recheck: std::time::Duration::from_secs(0),
            ..Default::default()
        },
    )
    .await
}

#[tokio::test]
async fn real_console_sessions_act_on_the_fhir_api_by_role() {
    let (rt, store) = console().await;
    assert!(helios_rest::endpoint::auth::install_console_resolver(
        rt.clone() as Arc<dyn helios_endpoints::ConsoleSessionResolver>
    ));
    let (_idp, issuer) = idp().await;
    let s = app(&issuer).await;

    let mut cookies = std::collections::HashMap::new();
    let mut tokens = std::collections::HashMap::new();
    for (who, role) in [
        ("viewer", ConsoleRole::Viewer),
        ("editor", ConsoleRole::Editor),
        ("admin", ConsoleRole::Admin),
        ("bound", ConsoleRole::Viewer),
    ] {
        let subject = format!("oidc:https://idp|{who}");
        let (cookie, csrf) = session(&rt, &subject, role, ConsoleVia::Oidc, None).await;
        cookies.insert(who, cookie);
        tokens.insert(who, csrf);
    }
    // A viewer bound as editor on `alpha`.
    store
        .set_role_binding("oidc:https://idp|bound", "alpha", ConsoleRole::Editor, "t")
        .await
        .unwrap();

    let cookie = |who: &str| HeaderValue::from_str(&cookies[who]).unwrap();
    let get = |p: &'static str, who: &'static str| s.get(p).add_header(header::COOKIE, cookie(who));
    let post_raw = |p: &'static str, who: &'static str, origin: Option<&'static str>| {
        let mut r = s
            .post(p)
            .add_header(header::COOKIE, cookie(who))
            .add_header(header::HOST, HeaderValue::from_static("hfs.test"))
            .add_header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/fhir+json"),
            )
            .json(&json!({"resourceType": "Patient"}));
        if let Some(o) = origin {
            r = r.add_header(header::ORIGIN, HeaderValue::from_static(o));
        }
        r
    };
    // Console JS (csrf.js) sends the session's synchronizer token on writes.
    let post = |p: &'static str, who: &'static str, origin: Option<&'static str>| {
        post_raw(p, who, origin).add_header(
            "x-csrf-token",
            HeaderValue::from_str(&tokens[who]).unwrap(),
        )
    };
    const SITE: Option<&str> = Some("http://hfs.test");

    // Viewer: reads, never writes.
    assert_eq!(
        get("/alpha/Patient", "viewer").await.status_code(),
        StatusCode::OK
    );
    assert_eq!(
        post("/alpha/Patient", "viewer", SITE).await.status_code(),
        StatusCode::FORBIDDEN
    );
    // Editor and admin write — same-site only.
    for who in ["editor", "admin"] {
        let r = post("/alpha/Patient", who, SITE).await;
        assert_eq!(r.status_code(), StatusCode::CREATED, "{who}: {}", r.text());
        assert_eq!(
            post("/alpha/Patient", who, Some("https://evil.example"))
                .await
                .status_code(),
            StatusCode::FORBIDDEN,
            "{who} cross-site"
        );
        assert_eq!(
            post("/alpha/Patient", who, None).await.status_code(),
            StatusCode::FORBIDDEN,
            "{who} without Origin"
        );
    }
    // Same-site but without the session's CSRF token: refused.
    assert_eq!(
        post_raw("/alpha/Patient", "editor", SITE).await.status_code(),
        StatusCode::FORBIDDEN,
        "no CSRF token"
    );
    // Per-endpoint binding: editor on alpha only.
    let r = post("/alpha/Patient", "bound", SITE).await;
    assert_eq!(r.status_code(), StatusCode::CREATED, "{}", r.text());
    assert_eq!(
        post("/ro/Patient", "bound", SITE).await.status_code(),
        StatusCode::FORBIDDEN
    );
    // consoleAccess read-only / off.
    assert_eq!(
        get("/ro/Patient", "admin").await.status_code(),
        StatusCode::OK
    );
    assert_eq!(
        post("/ro/Patient", "admin", SITE).await.status_code(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get("/closed/Patient", "admin").await.status_code(),
        StatusCode::UNAUTHORIZED
    );
    // Forged and signed-out sessions are anonymous.
    let forged = s
        .get("/alpha/Patient")
        .add_header(
            header::COOKIE,
            HeaderValue::from_static("hfs_console=forged"),
        )
        .await;
    assert_eq!(forged.status_code(), StatusCode::UNAUTHORIZED);
    let mut h = HeaderMap::new();
    h.insert(header::COOKIE, cookie("editor"));
    let _ = rt.logout(&h).await;
    assert_eq!(
        get("/alpha/Patient", "editor").await.status_code(),
        StatusCode::UNAUTHORIZED,
        "signed out"
    );
}
