//! The endpoint admin API mounted in the full application (`build_app`
//! through `create_app_with_auth_bulk_settings_and_ops`) with the FHIR server
//! manager on: an endpoint created through `/admin/endpoints` is served by the
//! dispatcher on the very next request, disabling it answers `503`, deleting
//! it stops routing it; with the manager off the API is not mounted.
//!
//! Auth is off, so — as in `hfs` — the administrator is established by the
//! server's admin guard in front of the app; a small layer stands in for it
//! by inserting `helios_admin_auth::AdminUser`.

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
    let app = helios_rest::create_app_with_auth_bulk_settings_and_ops(
        backend,
        config,
        helios_auth::AuthConfig::default(),
        None,
        None,
        None,
        None,
        None,
        OperationsBundle {
            endpoints: runtime,
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
    TestServer::new(app).unwrap()
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
