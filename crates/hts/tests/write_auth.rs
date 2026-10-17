//! `HTS_REQUIRE_AUTH_FOR_WRITES`: state-changing terminology requests need
//! the administrator credential once a password is configured; reads and the
//! non-persisting operations — including their POST forms and read-only
//! batches — stay public.
#![cfg(feature = "sqlite")]

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use helios_admin_auth::AdminSettings;
use helios_hts::config::HtsConfig;
use helios_hts::state::AppState;
use serde_json::{Value, json};
use tower::ServiceExt;

const PASSWORD: &str = "write-auth-test-password";
const CS_URL: &str = "http://hts.test/write-auth/cs";

struct App {
    _dir: tempfile::TempDir,
    router: Router,
}

fn app(auth: AdminSettings, require: Option<bool>) -> App {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("hts.db");
    let backend =
        helios_hts::backends::SqliteTerminologyBackend::new(db.to_str().unwrap()).unwrap();
    let pool = backend.pool().clone();
    let resource_store =
        helios_persistence::backends::sqlite::SqliteBackend::in_memory().expect("resource store");
    resource_store.init_schema().expect("resource store schema");
    let state = AppState::new(backend)
        .with_resource_store(resource_store)
        .with_hts_pool(pool);
    let config = HtsConfig {
        database_url: db.to_string_lossy().into_owned(),
        admin_auth: Some(auth),
        require_auth_for_writes: require,
        ..HtsConfig::default()
    };
    App {
        router: helios_hts::server::create_app(&config, state),
        _dir: dir,
    }
}

fn with_password() -> AdminSettings {
    AdminSettings::default().with_password(PASSWORD)
}

async fn send(
    app: &App,
    method: &str,
    uri: &str,
    auth: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/fhir+json")
        .header("accept", "application/fhir+json");
    if let Some(a) = auth {
        req = req.header("authorization", a);
    }
    let body = body.map(|b| Body::from(b.to_string())).unwrap_or_default();
    let resp = app
        .router
        .clone()
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 16 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn code_system(id: &str) -> Value {
    json!({
        "resourceType": "CodeSystem", "id": id, "url": CS_URL, "version": "1",
        "status": "active", "content": "complete",
        "concept": [{"code": "a", "display": "Alpha"}, {"code": "b", "display": "Beta"}]
    })
}

fn value_set() -> Value {
    json!({
        "resourceType": "ValueSet", "id": "wa-vs", "url": "http://hts.test/write-auth/vs",
        "status": "active", "compose": {"include": [{"system": CS_URL}]}
    })
}

fn bearer() -> String {
    format!("Bearer {PASSWORD}")
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_need_the_administrator_and_reads_do_not() {
    let app = app(with_password(), None);

    // Writes without a credential: 401, with an OperationOutcome.
    let (s, body) = send(&app, "POST", "/CodeSystem", None, Some(code_system("wa"))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["resourceType"], "OperationOutcome");
    let (s, _) = send(&app, "PUT", "/CodeSystem/wa", None, Some(code_system("wa"))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = send(&app, "DELETE", "/CodeSystem/wa", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let bundle = json!({"resourceType": "Bundle", "type": "collection",
        "entry": [{"resource": code_system("wa")}, {"resource": value_set()}]});
    let (s, _) = send(&app, "POST", "/import", None, Some(bundle.clone())).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = send(
        &app,
        "POST",
        "/import",
        Some("Bearer wrong"),
        Some(bundle.clone()),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // With the credential they succeed.
    let (s, body) = send(&app, "POST", "/import", Some(&bearer()), Some(bundle)).await;
    assert!(
        s == StatusCode::OK || s == StatusCode::MULTI_STATUS,
        "{s} {body}"
    );
    let mut cs2 = code_system("wa2");
    cs2["url"] = json!("http://hts.test/write-auth/cs2");
    let (s, body) = send(&app, "POST", "/CodeSystem", Some(&bearer()), Some(cs2)).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");

    // Reads stay public.
    let (s, _) = send(&app, "GET", "/metadata", None, None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, body) = send(
        &app,
        "GET",
        &format!("/CodeSystem?url={CS_URL}"),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, body) = send(
        &app,
        "GET",
        &format!("/CodeSystem/$lookup?system={CS_URL}&code=a"),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");

    // POST forms of the read operations stay public.
    let (s, body) = send(
        &app,
        "POST",
        "/ValueSet/$expand",
        None,
        Some(json!({"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": "http://hts.test/write-auth/vs"}]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, body) = send(
        &app,
        "POST",
        "/CodeSystem/$validate-code",
        None,
        Some(json!({"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": CS_URL}, {"name": "code", "valueCode": "a"}]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, body) = send(
        &app,
        "POST",
        "/ValueSet/$validate-code",
        None,
        Some(json!({"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": "http://hts.test/write-auth/vs"},
            {"name": "system", "valueUri": CS_URL}, {"name": "code", "valueCode": "b"}]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");

    // A batch of reads (how HFS's validator sends its checks) is public; a
    // batch that writes is not.
    let reads = json!({"resourceType": "Bundle", "type": "batch", "entry": [
        {"request": {"method": "POST", "url": "CodeSystem/$validate-code"},
         "resource": {"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": CS_URL}, {"name": "code", "valueCode": "a"}]}},
        {"request": {"method": "GET", "url": format!("CodeSystem?url={CS_URL}")}}
    ]});
    let (s, body) = send(&app, "POST", "/", None, Some(reads)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let writes = json!({"resourceType": "Bundle", "type": "batch", "entry": [
        {"request": {"method": "DELETE", "url": "CodeSystem/wa"}}
    ]});
    let (s, _) = send(&app, "POST", "/", None, Some(writes.clone())).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, body) = send(&app, "POST", "/", Some(&bearer()), Some(writes)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_password_writes_stay_open_unless_explicitly_required() {
    // Today's behaviour: no password, admin off → open (with a warning).
    let open = app(AdminSettings::default(), None);
    let (s, body) = send(&open, "POST", "/CodeSystem", None, Some(code_system("o"))).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");

    // Explicitly required without a password: locked, not open.
    let locked = app(AdminSettings::default(), Some(true));
    let (s, body) = send(&locked, "POST", "/CodeSystem", None, Some(code_system("l"))).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let (s, _) = send(&locked, "GET", "/metadata", None, None).await;
    assert_eq!(s, StatusCode::OK);

    // A password with the switch turned off: writes open again.
    let off = app(with_password(), Some(false));
    let (s, body) = send(&off, "POST", "/CodeSystem", None, Some(code_system("f"))).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
}
