//! End-to-end tests of the terminology admin API (`HTS_ADMIN_UI=true`):
//! chunked upload → detection → queued import job → inventory / pin /
//! dependents → delete job, plus the single-writer queue and the switches.
#![cfg(feature = "sqlite")]

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
mod common;

use helios_hts::config::HtsConfig;
use helios_hts::state::AppState;
use serde_json::{Value, json};
use tower::ServiceExt;

const CS_URL: &str = "http://example.org/admin-test/cs";

struct Env {
    _dir: tempfile::TempDir,
    app: Router,
    import_dir: std::path::PathBuf,
}

/// The backend label `/admin` reports for the store under test.
fn backend_label() -> &'static str {
    if common::pg_mode() {
        "postgres"
    } else {
        "sqlite"
    }
}

async fn env(admin: bool) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let import_dir = dir.path().join("import");
    std::fs::create_dir_all(&import_dir).unwrap();
    #[cfg(feature = "postgres")]
    if common::pg_mode() {
        let url = common::fresh_pg_database().await;
        let backend = helios_hts::backends::PostgresTerminologyBackend::new(&url)
            .await
            .unwrap();
        let state = AppState::new(backend);
        let config = HtsConfig {
            storage_backend: "postgres".into(),
            database_url: url,
            ..env_config(&dir, &import_dir, admin)
        };
        let app = helios_hts::server::create_app(&config, state);
        return Env {
            _dir: dir,
            app,
            import_dir,
        };
    }
    #[cfg(feature = "sqlite")]
    {
        let db = dir.path().join("hts.db");
        let backend =
            helios_hts::backends::SqliteTerminologyBackend::new(db.to_str().unwrap()).unwrap();
        let pool = backend.pool().clone();
        let state = AppState::new(backend).with_hts_pool(pool);
        let config = HtsConfig {
            database_url: db.to_string_lossy().into_owned(),
            ..env_config(&dir, &import_dir, admin)
        };
        let app = helios_hts::server::create_app(&config, state);
        Env {
            _dir: dir,
            app,
            import_dir,
        }
    }
    #[cfg(not(feature = "sqlite"))]
    panic!("no backend")
}

fn env_config(dir: &tempfile::TempDir, import_dir: &std::path::Path, admin: bool) -> HtsConfig {
    HtsConfig {
        admin_ui: admin,
        admin_data_dir: dir.path().join("admin").to_string_lossy().into_owned(),
        import_dir: import_dir.to_string_lossy().into_owned(),
        package_registry: "off".into(),
        upload_chunk_size: 64 * 1024,
        disk_headroom: 0,
        ..HtsConfig::default()
    }
}

/// A Bundle with one CodeSystem version (A ⊃ B ⊃ C), a ValueSet over it and
/// a long description so the upload spans several 64 KiB chunks.
fn bundle(version: &str, pad: usize) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "resourceType": "Bundle",
        "type": "collection",
        "entry": [
            {"resource": {
                "resourceType": "CodeSystem", "id": "admin-test", "url": CS_URL,
                "version": version, "status": "active", "content": "complete",
                "hierarchyMeaning": "is-a",
                "description": "x".repeat(pad),
                "property": [{"code": "parent", "type": "code"}],
                "concept": [
                    {"code": "A", "display": "Alpha"},
                    {"code": "B", "display": "Beta", "property": [{"code": "parent", "valueCode": "A"}]},
                    {"code": "C", "display": "Gamma", "property": [{"code": "parent", "valueCode": "B"}]}
                ]
            }},
            {"resource": {
                "resourceType": "ValueSet", "id": "admin-test-vs",
                "url": "http://example.org/admin-test/vs", "status": "active",
                "compose": {"include": [{"system": CS_URL}]}
            }}
        ]
    }))
    .unwrap()
}

async fn send(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    req = req.header("x-forwarded-for", "10.1.2.3");
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 << 20)
        .await
        .unwrap();
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

async fn put_bytes(app: &Router, uri: &str, data: Vec<u8>) -> StatusCode {
    let req = Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/octet-stream")
        .body(Body::from(data))
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

async fn wait_job(app: &Router, id: &str) -> Value {
    for _ in 0..600 {
        let (s, job) = send(app, "GET", &format!("/admin/jobs/{id}"), None).await;
        assert_eq!(s, StatusCode::OK);
        let status = job["status"].as_str().unwrap().to_string();
        if !matches!(status.as_str(), "queued" | "running") {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("job {id} did not finish");
}

async fn upload(app: &Router, name: &str, data: &[u8]) -> Value {
    let (s, created) = send(
        app,
        "POST",
        "/admin/uploads",
        Some(json!({"filename": name, "size": data.len(), "client_key": format!("{name}-{}", data.len())})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_string();
    let chunk = created["chunk_size"].as_u64().unwrap() as usize;
    let chunks = created["chunks"].as_u64().unwrap() as usize;
    assert!(chunks >= 2, "test bundle must span several chunks");
    // Send all but the last chunk, then "reconnect" and resume.
    for i in 0..chunks - 1 {
        let part = data[i * chunk..((i + 1) * chunk).min(data.len())].to_vec();
        assert_eq!(
            put_bytes(app, &format!("/admin/uploads/{id}/chunks/{i}"), part).await,
            StatusCode::OK
        );
    }
    let (s, resumed) = send(
        app,
        "POST",
        "/admin/uploads",
        Some(json!({"filename": name, "size": data.len(), "client_key": format!("{name}-{}", data.len())})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "resume returns the existing session");
    assert_eq!(resumed["id"], created["id"]);
    assert_eq!(resumed["missing"], json!([chunks - 1]));
    let last = chunks - 1;
    let part = data[last * chunk..].to_vec();
    assert_eq!(
        put_bytes(app, &format!("/admin/uploads/{id}/chunks/{last}"), part).await,
        StatusCode::OK
    );
    let (s, done) = send(app, "POST", &format!("/admin/uploads/{id}/complete"), None).await;
    assert_eq!(s, StatusCode::OK, "{done}");
    done
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upload_import_pin_delete_round_trip() {
    let e = env(true).await;
    let app = &e.app;

    let (s, status) = send(app, "GET", "/admin", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(status["backend"], backend_label());
    let (_, formats) = send(app, "GET", "/admin/formats", None).await;
    assert!(
        formats["formats"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "snomed-rf2" && f["licence"] == "user-supplied")
    );

    // ── Chunked upload with resume, then detection ───────────────────────────
    let v1 = bundle("1.0.0", 150_000);
    let done = upload(app, "admin-test-v1.json", &v1).await;
    assert_eq!(done["completed"], true);
    assert_eq!(done["detection"]["format"], "fhir-bundle");
    assert_eq!(done["sha256"].as_str().unwrap().len(), 64);
    let upload_id = done["id"].as_str().unwrap().to_string();

    // A wrong-length chunk is refused; an oversized body hits the route limit.
    assert_eq!(
        put_bytes(
            app,
            &format!("/admin/uploads/{upload_id}/chunks/0"),
            vec![0; 10]
        )
        .await,
        StatusCode::CONFLICT,
        "a completed upload accepts no more chunks"
    );
    let (_, other) = send(
        app,
        "POST",
        "/admin/uploads",
        Some(json!({"filename": "x.bin", "size": 10_000_000})),
    )
    .await;
    assert_eq!(
        put_bytes(
            app,
            &format!("/admin/uploads/{}/chunks/0", other["id"].as_str().unwrap()),
            vec![0; 2 * 1024 * 1024]
        )
        .await,
        StatusCode::PAYLOAD_TOO_LARGE
    );

    // A version override is refused for a multi-resource format.
    let (s, _) = send(
        app,
        "POST",
        "/admin/imports",
        Some(json!({"source": {"type": "upload", "upload_id": upload_id}, "format": "fhir-bundle", "version": "9"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // ── Import job ───────────────────────────────────────────────────────────
    let (s, job) = send(
        app,
        "POST",
        "/admin/imports",
        Some(json!({"source": {"type": "upload", "upload_id": upload_id}})),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{job}");
    let job = wait_job(app, job["id"].as_str().unwrap()).await;
    assert_eq!(job["status"], "succeeded", "{job}");
    assert_eq!(job["result"]["format"], "fhir-bundle");
    assert_eq!(job["progress"]["percent"], 100.0);
    let (_, log) = send(
        app,
        "GET",
        &format!("/admin/jobs/{}/log", job["id"].as_str().unwrap()),
        None,
    )
    .await;
    let log = log["lines"].as_array().unwrap();
    assert!(
        log.iter()
            .any(|l| l.as_str().unwrap().contains("closure done"))
    );

    let (s, lookup) = send(
        app,
        "GET",
        &format!("/CodeSystem/$lookup?system={CS_URL}&code=C"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{lookup}");

    // ── Two more versions from the server import directory, queued ───────────
    std::fs::write(e.import_dir.join("v2.json"), bundle("2.0.0", 10)).unwrap();
    std::fs::write(e.import_dir.join("v10.json"), bundle("10.0.0", 10)).unwrap();
    let (_, listing) = send(app, "GET", "/admin/import-dir", None).await;
    assert_eq!(listing["entries"].as_array().unwrap().len(), 2);
    let (s, _) = send(
        app,
        "GET",
        "/admin/import-dir/detect?path=../etc/passwd",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (_, j2) = send(
        app,
        "POST",
        "/admin/imports",
        Some(json!({"source": {"type": "import-dir", "path": "v2.json"}})),
    )
    .await;
    let (_, j3) = send(
        app,
        "POST",
        "/admin/imports",
        Some(json!({"source": {"type": "import-dir", "path": "v10.json"}})),
    )
    .await;
    assert_eq!(j3["status"], "queued", "second import waits: {j3}");
    assert!(j3["queue_position"].as_u64().unwrap() >= 1);
    let j2 = wait_job(app, j2["id"].as_str().unwrap()).await;
    let j3 = wait_job(app, j3["id"].as_str().unwrap()).await;
    assert_eq!(j2["status"], "succeeded", "{j2}");
    assert_eq!(j3["status"], "succeeded", "{j3}");
    let t =
        |j: &Value, k: &str| chrono::DateTime::parse_from_rfc3339(j[k].as_str().unwrap()).unwrap();
    assert!(
        t(&j3, "started_at") >= t(&j2, "finished_at"),
        "jobs must not overlap"
    );

    // ── Inventory, pin, dependents ───────────────────────────────────────────
    let lookup_version = |app: Router| async move {
        let (_, v) = send(
            &app,
            "GET",
            &format!("/CodeSystem/$lookup?system={CS_URL}&code=A"),
            None,
        )
        .await;
        v["parameter"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "version")
            .and_then(|p| p["valueString"].as_str())
            .map(String::from)
    };
    assert_eq!(lookup_version(app.clone()).await.as_deref(), Some("10.0.0"));

    let (_, inv) = send(app, "GET", "/admin/terminology?q=admin-test", None).await;
    let sys = &inv["systems"][0];
    assert_eq!(sys["url"], CS_URL);
    let versions = sys["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 3);
    assert_eq!(versions[0]["version"], "10.0.0", "semantic order");
    assert_eq!(versions[0]["current"], true);
    assert_eq!(versions[0]["stats"]["concepts"], 3);
    assert!(
        versions[2]["source"]
            .as_str()
            .unwrap()
            .starts_with("upload:")
    );

    let (s, _) = send(
        app,
        "PUT",
        "/admin/code-systems/current",
        Some(json!({"url": CS_URL, "version": "1.0.0"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(lookup_version(app.clone()).await.as_deref(), Some("1.0.0"));
    let (_, inv) = send(app, "GET", "/admin/terminology?q=admin-test", None).await;
    let v1row = inv["systems"][0]["versions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["version"] == "1.0.0")
        .unwrap()
        .clone();
    assert_eq!(
        (v1row["pinned"].clone(), v1row["current"].clone()),
        (json!(true), json!(true))
    );
    let (s, _) = send(
        app,
        "PUT",
        "/admin/code-systems/current",
        Some(json!({"url": CS_URL, "version": "7.7.7"})),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (_, deps) = send(
        app,
        "GET",
        &format!("/admin/code-systems/dependents?url={CS_URL}&version=1.0.0"),
        None,
    )
    .await;
    assert_eq!(
        deps["dependents"]["value_sets"][0]["url"],
        "http://example.org/admin-test/vs"
    );
    assert_eq!(deps["dependents"]["current_after_delete"], "10.0.0");

    // ── Delete the pinned version: lookups fall back to the newest ───────────
    let (s, dj) = send(
        app,
        "DELETE",
        &format!("/admin/code-systems?url={CS_URL}&version=1.0.0"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let dj = wait_job(app, dj["id"].as_str().unwrap()).await;
    assert_eq!(dj["status"], "succeeded", "{dj}");
    assert_eq!(dj["result"]["rows"]["concepts"], 3);
    let (s, _) = send(
        app,
        "GET",
        &format!("/CodeSystem/$lookup?system={CS_URL}&code=A&version=1.0.0"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(lookup_version(app.clone()).await.as_deref(), Some("10.0.0"));
    let (s, _) = send(
        app,
        "DELETE",
        &format!("/admin/code-systems?url={CS_URL}&version=1.0.0"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // ── Registry off, audit trail ────────────────────────────────────────────
    let (s, _) = send(
        app,
        "POST",
        "/admin/registry/install",
        Some(json!({"package": "hl7.terminology.r4"})),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    let (_, audit) = send(app, "GET", "/admin/audit", None).await;
    let events = audit["events"].as_array().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e["subtype"][0]["code"] == "delete-code-system"
                && e["agent"][0]["who"]["display"] == "10.1.2.3")
    );
    assert!(
        events
            .iter()
            .any(|e| e["subtype"][0]["code"] == "set-current-version")
    );

    // The console mounts the same router under its own prefix.
    let (s, _) = send(app, "GET", "/ui/hts/api/admin/formats", None).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_import_is_rolled_back_and_queued_jobs_cancel() {
    let e = env(true).await;
    let app = &e.app;
    std::fs::write(e.import_dir.join("a.json"), bundle("1.0.0", 10)).unwrap();
    // Queue two compaction jobs ahead so the import stays queued long enough
    // to cancel it before it starts.
    let (_, c1) = send(app, "POST", "/admin/maintenance/compact", None).await;
    let (_, imp) = send(
        app,
        "POST",
        "/admin/imports",
        Some(json!({"source": {"type": "import-dir", "path": "a.json"}})),
    )
    .await;
    let id = imp["id"].as_str().unwrap();
    let (s, cancelled) = send(app, "POST", &format!("/admin/jobs/{id}/cancel"), None).await;
    // Either still queued (cancelled now) or already picked up (cancel
    // requested): both end in `cancelled` and leave nothing behind.
    assert_eq!(s, StatusCode::OK, "{cancelled}");
    let job = wait_job(app, id).await;
    let c1 = wait_job(app, c1["id"].as_str().unwrap()).await;
    assert_eq!(c1["status"], "succeeded", "{c1}");
    if job["status"] == "cancelled" {
        let (s, _) = send(
            app,
            "GET",
            &format!("/CodeSystem/$lookup?system={CS_URL}&code=A"),
            None,
        )
        .await;
        assert_eq!(
            s,
            StatusCode::NOT_FOUND,
            "a cancelled import leaves no CodeSystem"
        );
    } else {
        // The import finished between submit and cancel.
        assert_eq!(job["status"], "succeeded");
    }
    let (s, _) = send(app, "POST", &format!("/admin/jobs/{id}/cancel"), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "finished jobs cannot be cancelled");
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_api_is_off_by_default() {
    let e = env(false).await;
    let (s, _) = send(&e.app, "GET", "/admin", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = send(&e.app, "GET", "/ui/hts/api/admin/formats", None).await;
    assert_ne!(s, StatusCode::OK);
}
