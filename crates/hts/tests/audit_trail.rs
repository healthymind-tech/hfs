//! HTS audit trail: terminology writes become BALP AuditEvents in the audit
//! store, reads are left out unless asked for, admin actions are recorded,
//! and the trail is searchable at `GET /AuditEvent` by the administrator.
#![cfg(feature = "sqlite")]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use helios_admin_auth::AdminSettings;
use helios_audit::{AuditConfig, AuditMiddlewareState, DatabaseSink};
use helios_hts::audit::HtsAudit;
use helios_hts::config::HtsConfig;
use helios_hts::state::AppState;
use helios_persistence::audit_store::TxAuditStore;
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use serde_json::{Value, json};
use tower::ServiceExt;

const PASSWORD: &str = "audit-trail-test-password";
const BALP: &str = "https://profiles.ihe.net/ITI/BALP/StructureDefinition/IHE.BasicAudit.";

struct App {
    _dir: tempfile::TempDir,
    router: Router,
}

fn resource_store() -> SqliteBackend {
    let data_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("data"))
        .unwrap();
    let store = SqliteBackend::with_config(
        ":memory:",
        SqliteBackendConfig {
            data_dir: Some(data_dir),
            ..Default::default()
        },
    )
    .expect("resource store");
    store.init_schema().expect("schema");
    store
}

fn app(reads: bool, password: bool) -> App {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("hts.db");
    let backend =
        helios_hts::backends::SqliteTerminologyBackend::new(db.to_str().unwrap()).unwrap();
    let pool = backend.pool().clone();
    let store = resource_store();
    let audit_store = Arc::new(TxAuditStore::new(
        Arc::new(store.clone()),
        None,
        "default".to_string(),
    ));
    let sink = Arc::new(DatabaseSink::new(
        audit_store.clone(),
        helios_fhir::FhirVersion::default(),
    ));
    let config = AuditConfig {
        reads,
        source_observer: "Device/hts".into(),
        ..AuditConfig::default()
    };
    let audit = HtsAudit {
        middleware: Arc::new(
            AuditMiddlewareState::new(sink, config).with_server_address("http://hts.test"),
        ),
        search: Some(audit_store),
    };
    let state = AppState::new(backend)
        .with_resource_store(store)
        .with_hts_pool(pool)
        .with_audit(Some(audit));
    let auth = if password {
        AdminSettings::default().with_password(PASSWORD)
    } else {
        AdminSettings::default()
    };
    let config = HtsConfig {
        database_url: db.to_string_lossy().into_owned(),
        admin_auth: Some(auth),
        admin_data_dir: dir.path().join("admin").to_string_lossy().into_owned(),
        ..HtsConfig::default()
    };
    App {
        router: helios_hts::server::create_app(&config, state),
        _dir: dir,
    }
}

fn admin() -> String {
    format!("Bearer {PASSWORD}")
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

fn code_system(id: &str, version: &str) -> Value {
    json!({
        "resourceType": "CodeSystem", "id": id, "url": "http://hts.test/audit/cs",
        "version": version, "status": "active", "content": "complete",
        "concept": [{"code": "a", "display": "Alpha"}]
    })
}

async fn trail(app: &App, query: &str, auth: Option<&str>) -> Vec<Value> {
    let (status, body) = send(app, "GET", &format!("/AuditEvent?{query}"), auth, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["entry"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|e| e["resource"].clone())
        .collect()
}

fn subtype(e: &Value) -> &str {
    e["subtype"][0]["code"].as_str().unwrap_or("")
}

#[tokio::test]
async fn terminology_writes_are_balp_events_and_reads_are_not_by_default() {
    let app = app(false, true);
    let (status, created) = send(
        &app,
        "POST",
        "/CodeSystem",
        Some(&admin()),
        Some(code_system("cs-audit", "1")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_string();
    let (status, _) = send(
        &app,
        "PUT",
        &format!("/CodeSystem/{id}"),
        Some(&admin()),
        Some(code_system(&id, "1")),
    )
    .await;
    assert!(status.is_success(), "{status}");
    // Reads: not recorded by default.
    send(&app, "GET", &format!("/CodeSystem/{id}"), None, None).await;
    send(
        &app,
        "GET",
        "/CodeSystem/$lookup?system=http://hts.test/audit/cs&code=a",
        None,
        None,
    )
    .await;
    // Refused write (no credential): recorded with its outcome.
    let (status, _) = send(&app, "DELETE", &format!("/CodeSystem/{id}"), None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = send(
        &app,
        "DELETE",
        &format!("/CodeSystem/{id}"),
        Some(&admin()),
        None,
    )
    .await;
    assert!(status.is_success(), "{status}");

    let events = trail(&app, "_count=100", Some(&admin())).await;
    let cs_ref = format!("CodeSystem/{id}");
    let refs = |e: &Value| -> Vec<String> {
        e["entity"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| x["what"]["reference"].as_str().map(str::to_string))
            .collect()
    };
    let create = events
        .iter()
        .find(|e| subtype(e) == "create")
        .expect("create event");
    assert_eq!(create["meta"]["profile"][0], format!("{BALP}Create"));
    assert!(refs(create).contains(&cs_ref), "{create}");
    assert_eq!(
        create["agent"][2]["who"]["identifier"]["value"], "admin",
        "{create}"
    );
    assert_eq!(create["source"]["observer"]["reference"], "Device/hts");
    let update = events
        .iter()
        .find(|e| subtype(e) == "update")
        .expect("update event");
    assert_eq!(update["meta"]["profile"][0], format!("{BALP}Update"));
    let deletes: Vec<_> = events.iter().filter(|e| subtype(e) == "delete").collect();
    assert_eq!(deletes.len(), 2, "{events:#?}");
    assert!(
        deletes
            .iter()
            .any(|e| e["outcome"] == "4"
                && e["outcomeDesc"].as_str().unwrap().starts_with("HTTP 401"))
    );
    assert!(
        deletes
            .iter()
            .any(|e| e["meta"]["profile"][0] == format!("{BALP}Delete"))
    );
    assert!(
        !events.iter().any(|e| subtype(e) == "read"),
        "reads are off"
    );
    assert!(
        !events.iter().any(|e| subtype(e) == "operation"),
        "$lookup is a read"
    );

    // Searchable.
    assert_eq!(
        trail(
            &app,
            &format!("entity={cs_ref}&subtype=create"),
            Some(&admin())
        )
        .await
        .len(),
        1
    );
    assert_eq!(trail(&app, "outcome=4", Some(&admin())).await.len(), 1);
}

#[tokio::test]
async fn the_trail_is_for_the_administrator_only() {
    let app = app(false, true);
    let (status, _) = send(&app, "GET", "/AuditEvent", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = send(&app, "GET", "/AuditEvent?name=x", Some(&admin()), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = send(&app, "GET", "/AuditEvent/nope", Some(&admin()), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reads_are_recorded_when_enabled() {
    let app = app(true, false);
    send(
        &app,
        "POST",
        "/CodeSystem",
        None,
        Some(code_system("cs-r", "1")),
    )
    .await;
    send(
        &app,
        "GET",
        "/CodeSystem/$lookup?system=http://hts.test/audit/cs&code=a",
        None,
        None,
    )
    .await;
    let events = trail(&app, "subtype=operation", None).await;
    assert_eq!(events.len(), 1, "{events:#?}");
    assert_eq!(events[0]["meta"]["profile"][0], format!("{BALP}Query"));
    assert!(
        events[0]["entity"][0]["description"]
            .as_str()
            .unwrap()
            .contains("$lookup?system=")
    );
}

#[tokio::test]
async fn imports_are_recorded() {
    let app = app(false, false);
    let bundle = json!({
        "resourceType": "Bundle", "type": "collection",
        "entry": [{"resource": code_system("cs-imp", "1")}]
    });
    let (status, body) = send(&app, "POST", "/import", None, Some(bundle)).await;
    assert!(status.is_success(), "{status} {body}");
    let events = trail(&app, "_count=50", None).await;
    let import = events
        .iter()
        .find(|e| {
            e["entity"][0]["description"]
                .as_str()
                .is_some_and(|d| d.contains("/import"))
        })
        .expect("import event");
    assert_eq!(import["action"], "E");
    assert_eq!(import["outcome"], "0");
}
