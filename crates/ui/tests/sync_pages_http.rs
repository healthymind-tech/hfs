//! The sync console pages (FHIR server manager P6) through the mounted UI
//! router: unavailable state, list, create form (with `?source=`), job
//! page actions, run page and its htmx status fragment, zh-Hant strings.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use helios_endpoints::{
    Actor, EndpointName, EndpointRegistry, EndpointSpec, EndpointStore, FhirRelease,
    MemoryEndpointStore,
};
use helios_persistence::backends::sqlite::SqliteBackend;
use helios_persistence::core::cluster_job_store::testing::InMemoryClusterJobStore;
use helios_persistence::core::{ResourceStorage, WorkerId};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn app() -> Router {
    helios_ui::mount_with_conformance_source(
        Router::new(),
        "9.9.9",
        Some(std::path::PathBuf::from("../../data")),
        helios_ui::NlSearch::default(),
        None,
        None,
        "default".to_string(),
        Arc::new(helios_ui::StaticConformanceSource::from_data_dir(
            std::path::Path::new("../../data"),
        )),
        helios_fhir::FhirVersion::R4,
        None,
        "http://localhost:8080/fhir".to_string(),
        None,
    )
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn post_form(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::ORIGIN, "http://localhost")
        .header(header::HOST, "localhost")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_pages_end_to_end() {
    let app = app();

    // Before the service is installed: an explained unavailable state.
    let (s, _, html) = send(&app, get("/ui/sync")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("HFS_MANAGER=off"), "{html}");

    // Install a sync service over SQLite with two endpoints.
    let backend = Arc::new(SqliteBackend::in_memory().unwrap());
    backend.init_schema().unwrap();
    let store: Arc<dyn EndpointStore> = Arc::new(MemoryEndpointStore::new());
    store
        .create(
            EndpointSpec::default_endpoint("default", FhirRelease::R4),
            &Actor::bootstrap(),
        )
        .await
        .unwrap();
    store
        .create(
            EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab"),
            &Actor::bootstrap(),
        )
        .await
        .unwrap();
    let registry = Arc::new(EndpointRegistry::load(store.as_ref()).await.unwrap());
    let env = [
        ("HFS_SYNC_ENABLED", "false"),
        ("HFS_SYNC_SAFETY_LAG_MS", "0"),
    ];
    let cfg = helios_sync::SyncConfig::from_lookup(&|k| {
        env.iter()
            .find(|(a, _)| *a == k)
            .map(|(_, v)| v.to_string())
    });
    let (svc, _) = helios_sync::build(
        &cfg,
        helios_sync::SyncDeps {
            store: Arc::new(helios_sync::memory::MemorySyncStore::new()),
            cluster: Arc::new(InMemoryClusterJobStore::new()),
            registry,
            endpoints: Some(store),
            local: Some(backend.clone()),
            copier: None,
            key_ring: None,
            observer: None,
            audit: None,
            instance: "ui-test".into(),
        },
    );
    assert!(helios_sync::install_global(svc.clone()));
    backend
        .create_or_update(
            &helios_sync::local::tenant_context("default"),
            "Patient",
            "p1",
            serde_json::json!({"resourceType": "Patient", "id": "p1"}),
            helios_fhir::FhirVersion::R4,
        )
        .await
        .unwrap();

    let (s, _, html) = send(&app, get("/ui/sync")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("/ui/sync/new"));

    // `?source=` preselects the source endpoint.
    let (s, _, html) = send(&app, get("/ui/sync/new?source=lab")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains(r#"<option value="lab" selected>"#), "{html}");

    // Invalid form: 422 with the field error, no secret echoed.
    let (s, _, html) = send(
        &app,
        post_form("/ui/sync/new", "name=x&source_kind=endpoint&source_endpoint=default&target_kind=endpoint&target_endpoint=default&target_secret=hunter2"),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("target"));
    assert!(!html.contains("hunter2"));

    // Valid form → job page.
    let (s, h, _) = send(
        &app,
        post_form(
            "/ui/sync/new",
            "name=default+to+lab&source_kind=endpoint&source_endpoint=default&target_kind=endpoint&target_endpoint=lab&initial_load=paged-read&trigger=manual&conflict=source-wins&batch_size=50&propagate_deletes=on&enabled=on",
        ),
    )
    .await;
    assert_eq!(s, StatusCode::SEE_OTHER, "create redirects");
    let job_url = h[header::LOCATION].to_str().unwrap().to_string();
    let (s, _, html) = send(&app, get(&job_url)).await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("default to lab"));

    // Run → run page; the worker finishes it; terminal fragment stops polling.
    let (s, h, _) = send(&app, post_form(&format!("{job_url}/run"), "")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let run_url = h[header::LOCATION].to_str().unwrap().to_string();
    let (_, _, live) = send(&app, get(&format!("{run_url}/status"))).await;
    assert!(live.contains("hx-trigger=\"every 2s\""), "active run polls");
    while svc.runtime().work_next(&WorkerId::new("w")).await.unwrap() {}
    let (s, _, html) = send(&app, get(&run_url)).await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("succeeded"));
    let (_, _, done) = send(&app, get(&format!("{run_url}/status"))).await;
    assert!(!done.contains("hx-trigger"), "terminal run stops polling");
    assert!(done.contains(r#"data-counter="created">1<"#), "{done}");

    // zh-Hant.
    let mut req = get("/ui/sync");
    req.headers_mut()
        .insert(header::ACCEPT_LANGUAGE, "zh-Hant".parse().unwrap());
    let (_, _, html) = send(&app, req).await;
    assert!(html.contains("同步工作"));

    // Unknown ids render the not-found page; delete returns to the list.
    let (s, _, _) = send(&app, get("/ui/sync/nope")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, h, _) = send(&app, post_form(&format!("{job_url}/delete"), "")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(h[header::LOCATION], "/ui/sync");
}
