//! The sync admin API (`/admin/sync/**`) and the notification receiver
//! (`/sync/notify/{job}`) over a real sync service on SQLite storage:
//! every route, JSON shapes, OperationOutcome errors (422 per field, 404,
//! 409, 412 stale If-Match), ETags, write-only secrets, 503 when sync is
//! not installed.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use helios_endpoints::model::EndpointSpec;
use helios_endpoints::name::EndpointName;
use helios_endpoints::registry::EndpointRegistry;
use helios_endpoints::secret::SecretKeyRing;
use helios_endpoints::store::{Actor, EndpointStore, MemoryEndpointStore, ensure_default_endpoint};
use helios_fhir::FhirVersion;
use helios_persistence::backends::sqlite::SqliteBackend;
use helios_persistence::core::cluster_job_store::testing::InMemoryClusterJobStore;
use helios_persistence::core::{ResourceStorage, WorkerId};
use helios_rest::handlers::admin_sync::SyncApi;
use helios_rest::routing::admin_sync::{notify_routes_with, routes_with};
use helios_sync::local::tenant_context;
use helios_sync::{SyncConfig, SyncDeps, SyncService};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn service() -> (Arc<SyncService>, Arc<SqliteBackend>) {
    let backend = Arc::new(SqliteBackend::in_memory().unwrap());
    backend.init_schema().unwrap();
    let endpoints: Arc<dyn EndpointStore> = Arc::new(MemoryEndpointStore::new());
    ensure_default_endpoint(
        endpoints.as_ref(),
        "default",
        helios_endpoints::FhirRelease::R4,
    )
    .await
    .unwrap();
    for n in ["lab", "copy"] {
        endpoints
            .create(
                EndpointSpec::named(EndpointName::parse(n).unwrap(), n),
                &Actor::bootstrap(),
            )
            .await
            .unwrap();
    }
    let registry = Arc::new(EndpointRegistry::load(endpoints.as_ref()).await.unwrap());
    let env = [
        ("HFS_SYNC_ENABLED", "false"),
        ("HFS_SYNC_SAFETY_LAG_MS", "0"),
    ];
    let cfg = SyncConfig::from_lookup(&|k| {
        env.iter()
            .find(|(a, _)| *a == k)
            .map(|(_, v)| v.to_string())
    });
    let (svc, _) = helios_sync::build(
        &cfg,
        SyncDeps {
            store: Arc::new(helios_sync::memory::MemorySyncStore::new()),
            cluster: Arc::new(InMemoryClusterJobStore::new()),
            registry,
            endpoints: Some(endpoints),
            local: Some(backend.clone()),
            copier: None,
            key_ring: Some(Arc::new(
                SecretKeyRing::parse(&SecretKeyRing::generate_spec("k")).unwrap(),
            )),
            observer: None,
            audit: None,
            instance: "api-test".into(),
        },
    );
    (svc, backend)
}

fn app(svc: &Arc<SyncService>) -> Router {
    routes_with(SyncApi::Fixed(svc.clone())).merge(notify_routes_with(SyncApi::Fixed(svc.clone())))
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, v)
}

#[tokio::test]
async fn job_lifecycle_over_the_api() {
    let (svc, backend) = service().await;
    backend
        .create_or_update(
            &tenant_context("default"),
            "Patient",
            "p1",
            json!({"resourceType": "Patient", "id": "p1"}),
            FhirVersion::R4,
        )
        .await
        .unwrap();
    let app = app(&svc);

    // Create (422 first).
    let (s, _, oo) = call(
        &app,
        "POST",
        "/admin/sync/jobs",
        Some(
            json!({"name": "x", "source": {"kind": "endpoint", "name": "default"},
        "target": {"kind": "endpoint", "name": "nope"}}),
        ),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(oo["resourceType"], "OperationOutcome");
    assert_eq!(oo["issue"][0]["expression"][0], "target.name");

    let (s, h, created) = call(&app, "POST", "/admin/sync/jobs", Some(json!({"name": "default to lab",
        "source": {"kind": "endpoint", "name": "default"}, "target": {"kind": "endpoint", "name": "lab"}})), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    let id = created["job"]["id"].as_str().unwrap().to_string();
    assert_eq!(h["location"], format!("/admin/sync/jobs/{id}"));

    let (s, _, list) = call(&app, "GET", "/admin/sync/jobs", None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list["jobs"].as_array().unwrap().len(), 1);

    let (s, h, job) = call(&app, "GET", &format!("/admin/sync/jobs/{id}"), None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["etag"], "\"1\"");
    assert_eq!(job["spec"]["name"], "default to lab");

    // PUT with a stale If-Match → 412; with the right one → 200 + new ETag.
    let mut spec = job["spec"].clone();
    spec["description"] = json!("nightly");
    let (s, _, _) = call(
        &app,
        "PUT",
        &format!("/admin/sync/jobs/{id}"),
        Some(spec.clone()),
        &[("if-match", "\"7\"")],
    )
    .await;
    assert_eq!(s, StatusCode::PRECONDITION_FAILED);
    let (s, h, upd) = call(
        &app,
        "PUT",
        &format!("/admin/sync/jobs/{id}"),
        Some(spec),
        &[("if-match", "W/\"1\"")],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{upd}");
    assert_eq!(h["etag"], "\"2\"");
    let (s, _, _) = call(
        &app,
        "PUT",
        &format!("/admin/sync/jobs/{id}"),
        Some(json!({})),
        &[("if-match", "bogus")],
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Run → 202 + Location; second run while queued → 409.
    let (s, h, run) = call(
        &app,
        "POST",
        &format!("/admin/sync/jobs/{id}/$run"),
        None,
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let run_id = run["id"].as_str().unwrap().to_string();
    assert_eq!(h["location"], format!("/admin/sync/runs/{run_id}"));
    let (s, _, _) = call(
        &app,
        "POST",
        &format!("/admin/sync/jobs/{id}/$run"),
        None,
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    while svc.runtime().work_next(&WorkerId::new("w")).await.unwrap() {}
    let (s, _, r) = call(
        &app,
        "GET",
        &format!("/admin/sync/runs/{run_id}"),
        None,
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["state"], "succeeded", "{r}");
    assert_eq!(r["progress"]["created"], 1);
    let (_, _, runs) = call(
        &app,
        "GET",
        &format!("/admin/sync/jobs/{id}/runs?limit=5"),
        None,
        &[],
    )
    .await;
    assert_eq!(runs["runs"].as_array().unwrap().len(), 1);
    let (_, _, log) = call(
        &app,
        "GET",
        &format!("/admin/sync/runs/{run_id}/log?after=0&limit=50"),
        None,
        &[],
    )
    .await;
    assert!(!log["lines"].as_array().unwrap().is_empty());
    let (_, _, c) = call(
        &app,
        "GET",
        &format!("/admin/sync/runs/{run_id}/conflicts"),
        None,
        &[],
    )
    .await;
    assert_eq!(c["conflicts"], json!([]));
    let (s, _, _) = call(
        &app,
        "POST",
        &format!("/admin/sync/runs/{run_id}/conflicts/Patient/p1/$resolve"),
        Some(json!({"action": "keep"})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Cancel with nothing active → 409; retry after success → 409.
    let (s, _, _) = call(
        &app,
        "POST",
        &format!("/admin/sync/jobs/{id}/$cancel"),
        None,
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _, _) = call(
        &app,
        "POST",
        &format!("/admin/sync/jobs/{id}/$retry"),
        None,
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    // Queue one and cancel it.
    call(
        &app,
        "POST",
        &format!("/admin/sync/jobs/{id}/$run"),
        None,
        &[],
    )
    .await;
    let (s, _, cancelled) = call(
        &app,
        "POST",
        &format!("/admin/sync/jobs/{id}/$cancel"),
        None,
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(cancelled["state"], "cancelled");
    let (s, _, _) = call(
        &app,
        "POST",
        &format!("/admin/sync/jobs/{id}/$retry"),
        None,
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);

    // Unknown ids → 404 OperationOutcome.
    let (s, _, oo) = call(&app, "GET", "/admin/sync/jobs/nope", None, &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(oo["issue"][0]["code"], "not-found");
    let (s, _, _) = call(&app, "GET", "/admin/sync/runs/nope", None, &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Delete.
    let (s, _, _) = call(&app, "DELETE", &format!("/admin/sync/jobs/{id}"), None, &[]).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _, _) = call(&app, "DELETE", &format!("/admin/sync/jobs/{id}"), None, &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn secrets_are_write_only_and_notify_receiver() {
    let (svc, _) = service().await;
    let app = app(&svc);
    let (s, _, created) = call(&app, "POST", "/admin/sync/jobs", Some(json!({
        "name": "pull",
        "source": {"kind": "external", "baseUrl": "https://fhir.example.org/r4", "auth": {"type": "bearer"}},
        "target": {"kind": "endpoint", "name": "lab"},
        "initialLoad": "none",
        "trigger": {"type": "subscription", "fallbackPollSecs": 600},
        "secrets": {"source.auth.token": {"set": "very-secret-token"}}
    })), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    let secret = created["notifySecret"].as_str().unwrap().to_string();
    let id = created["job"]["id"].as_str().unwrap().to_string();
    let (_, _, job) = call(&app, "GET", &format!("/admin/sync/jobs/{id}"), None, &[]).await;
    let text = job.to_string();
    assert!(
        !text.contains("very-secret-token") && !text.contains("hmsec:") && !text.contains(&secret)
    );
    assert_eq!(job["spec"]["source"]["auth"]["token"], json!({"set": true}));
    // Plaintext in the peer is refused.
    let (s, _, _) = call(
        &app,
        "POST",
        "/admin/sync/jobs",
        Some(json!({
        "name": "bad", "source": {"kind": "external", "baseUrl": "https://fhir.example.org/r4",
                                  "auth": {"type": "bearer", "token": "plain"}},
        "target": {"kind": "endpoint", "name": "copy"}})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    let (s, _, _) = call(
        &app,
        "POST",
        &format!("/sync/notify/{id}"),
        Some(json!({})),
        &[("x-sync-notify-secret", "wrong")],
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = call(
        &app,
        "POST",
        &format!("/sync/notify/{id}"),
        Some(json!({})),
        &[("x-sync-notify-secret", &secret)],
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);

    // Rewrap (same key) and duplicate through the API.
    let (s, _, r) = call(&app, "POST", "/admin/sync/$rewrap", None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["rewrapped"], 0);
    let (s, _, d) = call(
        &app,
        "POST",
        "/admin/sync/duplicates",
        Some(json!({"source": "lab", "name": "lab2", "packages": false})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{d}");
    assert_eq!(d["endpoint"]["status"], "enabled");
    let (s, _, _) = call(
        &app,
        "POST",
        "/admin/sync/duplicates",
        Some(json!({"source": "nope", "name": "x"})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // Connection test of a local pair needs no network.
    let (s, _, t) = call(&app, "POST", "/admin/sync/jobs/$test", Some(json!({"name": "t",
        "source": {"kind": "endpoint", "name": "lab"}, "target": {"kind": "endpoint", "name": "copy"}})), &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(t["peers"][0]["ok"], true);
}

#[tokio::test]
async fn not_installed_answers_503() {
    let app = helios_rest::routing::admin_sync::routes();
    let (s, _, oo) = call(&app, "GET", "/admin/sync/jobs", None, &[]).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(oo["resourceType"], "OperationOutcome");
}
