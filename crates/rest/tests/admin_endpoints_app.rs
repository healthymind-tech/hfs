//! The endpoint admin API mounted in the full application (`build_app`
//! through `create_app_with_auth_bulk_settings_and_ops`) with the FHIR server
//! manager on: an endpoint created through `/admin/endpoints` is served by the
//! dispatcher on the very next request, disabling it answers `503`, deleting
//! it stops routing it; with the manager off the API is not mounted.
//!
//! Auth is off, so — as in `hfs` — the administrator is established by the
//! server's admin guard in front of the app; a small layer stands in for it
//! by inserting `helios_admin_auth::AdminUser`.

#[path = "admin_endpoints_suite/mod.rs"]
mod suite;

use std::path::PathBuf;
use std::sync::Arc;

use axum::http::{HeaderValue, StatusCode, header};
use axum_test::TestServer;
use helios_endpoints::{
    EndpointRegistry, EndpointStore, FhirRelease, MemoryEndpointStore, ensure_default_endpoint,
};
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use helios_rest::endpoint::EndpointRuntime;
use helios_rest::{OperationsBundle, ServerConfig};
use serde_json::{Value, json};

async fn server(manager: bool, admin: bool) -> TestServer {
    server_with(manager, admin, None).await.0
}

/// Also returns the runtime (when the manager is on); `sink` = an audit
/// pipeline for the app.
async fn server_with(
    manager: bool,
    admin: bool,
    sink: Option<Arc<suite::CaptureSink>>,
) -> (TestServer, Option<EndpointRuntime>) {
    let data_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("data"));
    let backend = Arc::new(
        SqliteBackend::with_config(
            ":memory:",
            SqliteBackendConfig {
                data_dir,
                ..Default::default()
            },
        )
        .unwrap(),
    );
    backend.init_schema().unwrap();
    let config = ServerConfig {
        base_url: "https://hfs.example/fhir".to_string(),
        default_tenant: "default".to_string(),
        seed_conformance: false,
        ..ServerConfig::for_testing()
    };
    let runtime = if manager {
        let store = Arc::new(MemoryEndpointStore::new());
        ensure_default_endpoint(store.as_ref(), "default", FhirRelease::R4)
            .await
            .unwrap();
        let registry = Arc::new(EndpointRegistry::load(store.as_ref()).await.unwrap());
        Some(EndpointRuntime::new(
            registry,
            store as Arc<dyn EndpointStore>,
        ))
    } else {
        None
    };
    let audit = sink.map(|s| {
        Arc::new(helios_audit::AuditMiddlewareState::new(
            s,
            helios_audit::AuditConfig::default(),
        ))
    });
    let app = helios_rest::create_app_with_auth_bulk_settings_and_ops(
        backend,
        config,
        helios_auth::AuthConfig::default(),
        None,
        audit,
        None,
        None,
        None,
        OperationsBundle {
            endpoints: runtime.clone(),
            ..Default::default()
        },
    );
    let app = if admin {
        app.layer(axum::middleware::from_fn(
            |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
                req.extensions_mut()
                    .insert(helios_admin_auth::AdminUser("admin".into()));
                next.run(req).await
            },
        ))
    } else {
        app
    };
    (TestServer::new(app).unwrap(), runtime)
}

#[tokio::test]
async fn created_endpoint_is_served_on_the_next_request() {
    let s = server(true, true).await;
    let r = s
        .post("/admin/endpoints")
        .json(&json!({"name": "sandbox", "displayName": "Sandbox"}))
        .await;
    assert_eq!(r.status_code(), StatusCode::CREATED, "{}", r.text());
    assert_eq!(
        r.json::<Value>()["publicBaseUrl"],
        "https://hfs.example/fhir/sandbox"
    );

    let meta = s.get("/sandbox/metadata").await;
    assert_eq!(meta.status_code(), StatusCode::OK, "{}", meta.text());
    let cs: Value = meta.json();
    assert_eq!(
        cs["implementation"]["url"],
        "https://hfs.example/fhir/sandbox"
    );

    // A write lands in the endpoint's tenant and its Location is under the endpoint.
    let created = s
        .post("/sandbox/Patient")
        .add_header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/fhir+json"),
        )
        .json(&json!({"resourceType": "Patient", "active": true}))
        .await;
    assert_eq!(
        created.status_code(),
        StatusCode::CREATED,
        "{}",
        created.text()
    );
    let location = created
        .header(header::LOCATION)
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        location.starts_with("https://hfs.example/fhir/sandbox/Patient/"),
        "{location}"
    );
    let stats: Value = s.get("/admin/endpoints/sandbox/stats").await.json();
    assert_eq!(stats["resources"]["Patient"], 1, "{stats}");

    let r = s.post("/admin/endpoints/sandbox/$disable").await;
    assert_eq!(r.status_code(), StatusCode::OK);
    assert_eq!(
        s.get("/sandbox/metadata").await.status_code(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    let r = s.post("/admin/endpoints/sandbox/$enable").await;
    assert_eq!(r.status_code(), StatusCode::OK);
    assert_eq!(
        s.get("/sandbox/metadata").await.status_code(),
        StatusCode::OK
    );

    let r = s
        .delete("/admin/endpoints/sandbox?confirm=sandbox&purge=true")
        .await;
    assert_eq!(r.status_code(), StatusCode::OK, "{}", r.text());
    assert_eq!(r.json::<Value>()["resourcesRemoved"], 1);
    // No longer an endpoint: the segment is not a resource type of the default endpoint.
    assert_ne!(
        s.get("/sandbox/metadata").await.status_code(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn admin_api_needs_an_administrator() {
    let s = server(true, false).await;
    let r = s.get("/admin/endpoints").await;
    assert_eq!(r.status_code(), StatusCode::UNAUTHORIZED, "{}", r.text());
    assert_eq!(r.json::<Value>()["resourceType"], "OperationOutcome");
}

#[tokio::test]
async fn not_mounted_with_the_manager_off() {
    let s = server(false, true).await;
    assert_eq!(
        s.get("/admin/endpoints").await.status_code(),
        StatusCode::NOT_FOUND
    );
    // The legacy admin API is still there.
    assert_ne!(
        s.get("/admin/packages/config").await.status_code(),
        StatusCode::NOT_FOUND
    );
}

/// The admin events (DICOM 110100) the app's audit sink received.
fn admin_events(sink: &suite::CaptureSink) -> Vec<(Option<String>, Value)> {
    sink.0
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, e)| e.to_string().contains("\"110100\""))
        .cloned()
        .collect()
}

/// What must be equal between two admin events of the same kind.
fn shape(e: &Value) -> Value {
    json!({
        "type": e["type"],
        "action": e["action"],
        "outcome": e["outcome"],
        "source": e["source"],
        "what": e["entity"][0]["what"],
        "entityType": e["entity"][0]["type"],
        "detailNames": e["entity"][0]["detail"].as_array().map(|d| d.iter().map(|x| x["type"].clone()).collect::<Vec<_>>()),
        "agentWho": e["agent"][0]["who"],
        "tags": e["meta"]["tag"],
    })
}

#[tokio::test]
async fn console_writes_share_the_api_service_and_its_audit_trail() {
    let sink = Arc::new(suite::CaptureSink::default());
    let (s, runtime) = server_with(true, true, Some(sink.clone())).await;
    let runtime = runtime.unwrap();
    // The app published its admin service on the runtime.
    let admin = runtime.admin_api().expect("published by build_app").clone();

    let r = s
        .post("/admin/endpoints")
        .json(&json!({"name": "lab"}))
        .await;
    assert_eq!(r.status_code(), StatusCode::CREATED, "{}", r.text());

    // API path.
    let r = s
        .patch("/admin/endpoints/lab")
        .add_header(header::IF_MATCH, HeaderValue::from_static("*"))
        .json(&json!({"displayName": "Lab (API)"}))
        .await;
    assert_eq!(r.status_code(), StatusCode::OK, "{}", r.text());
    let api_event = admin_events(&sink).last().cloned().unwrap();

    // Console path: the same service, a typed edit by the same actor.
    let actor = helios_endpoints::Actor {
        subject: "admin".into(),
        display: Some("admin".into()),
        via: "admin-password".into(),
    };
    let current = admin.get_endpoint("lab").await.unwrap();
    let mut spec = current.spec.clone();
    spec.display_name = "Lab (console)".into();
    admin
        .update_endpoint(
            "lab",
            current.revision,
            spec,
            Default::default(),
            &actor,
            "patched",
        )
        .await
        .unwrap();
    let console_event = admin_events(&sink).last().cloned().unwrap();

    assert_eq!(api_event.0.as_deref(), Some("default"));
    assert_eq!(console_event.0, api_event.0, "same tenant");
    assert_eq!(shape(&console_event.1), shape(&api_event.1));
    assert!(console_event.1.to_string().contains("Endpoint/lab"));
    // And the console's write is visible through the API.
    let got: Value = s.get("/admin/endpoints/lab").await.json();
    assert_eq!(got["displayName"], "Lab (console)");
    // Statistics and audit search come with the shared service.
    assert!(admin.endpoint_stats("lab", 600).await.is_ok());
    assert!(admin.endpoint_audit("lab", 10, None).await.is_ok());
}
