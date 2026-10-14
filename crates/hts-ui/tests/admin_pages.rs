//! Terminology management pages against an in-process mock of the HTS admin
//! API: inventory, delete confirmation, jobs list/detail polling, the archive
//! sources on the Import page, the form → job redirects, and the "admin off"
//! state.

use axum::{
    Router,
    body::Body,
    extract::{Path, Request, State},
    http::{Method, StatusCode, header},
    response::IntoResponse,
    routing::{get, post, put},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tower::ServiceExt;

type Captured = Arc<Mutex<Vec<(String, String, String)>>>;

fn job(id: &str, status: &str, phase: &str, percent: f64) -> Value {
    json!({
        "id": id, "label": format!("import snomed-rf2 from upload {id}"), "status": status,
        "spec": {"kind": "import"},
        "requested_by": {"address": "10.0.0.9"},
        "created_at": "2026-10-14T10:00:00+00:00",
        "started_at": "2026-10-14T10:00:02+00:00",
        "finished_at": if status == "succeeded" { json!("2026-10-14T10:30:02+00:00") } else { Value::Null },
        "phases": ["prepare", "parse", "write", "closure", "fts", "analyze", "warm"],
        "progress": {"phase": phase, "done": 12, "total": 74, "unit": "batches", "concepts": 60000, "percent": percent},
        "result": if status == "succeeded" { json!({"stats": {"code_systems": 1, "value_sets": 0, "concept_maps": 9, "concepts": 369000}, "db_growth_bytes": 3600000000i64, "elapsed_seconds": 1800.0}) } else { Value::Null },
        "error": null,
        "queue_position": null
    })
}

async fn capture(cap: &Captured, method: &Method, path: String, body: Body) -> String {
    let bytes = axum::body::to_bytes(body, 1 << 20)
        .await
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    cap.lock()
        .await
        .push((method.to_string(), path, text.clone()));
    text
}

async fn start_mock(admin_on: bool) -> (String, Captured) {
    let cap: Captured = Arc::default();
    let mut router: Router<Captured> = Router::new().route(
        "/health",
        get(|| async {
            axum::Json(json!({"status":"ok","service":"hts","version":"0","backend":"sqlite","uptime_seconds":1}))
        }),
    );
    if admin_on {
        router = router
            .route(
                "/admin",
                get(|| async {
                    axum::Json(json!({
                        "enabled": true, "backend": "sqlite", "import_dir": "/import",
                        "registry": "https://packages.fhir.org", "chunk_size": 8388608,
                        "max_upload_size": 17179869184u64
                    }))
                }),
            )
            .route(
                "/admin/formats",
                get(|| async {
                    axum::Json(json!({"formats": [
                        {"id": "snomed-rf2", "licence": "user-supplied"},
                        {"id": "loinc", "licence": "user-supplied"},
                        {"id": "fhir-bundle", "licence": "package"}
                    ]}))
                }),
            )
            .route(
                "/admin/uploads",
                get(|| async {
                    axum::Json(json!({"uploads": [
                        {"id": "u1", "filename": "SnomedCT.zip", "size": 565268798, "completed": false,
                         "received_bytes": 282634399, "updated_at": "2026-10-14T09:00:00+00:00"},
                        {"id": "u2", "filename": "Loinc_2.80.zip", "size": 77489692, "completed": true,
                         "detection": {"format": "loinc", "version": "2.80"}}
                    ]}))
                }),
            )
            .route(
                "/admin/import-dir",
                get(|| async {
                    axum::Json(json!({"entries": [
                        {"path": "loinc/Loinc_2.80.zip", "size": 77489692, "is_dir": false, "modified": "2026-10-14T01:28:00+00:00", "format_hint": null}
                    ]}))
                }),
            )
            .route(
                "/admin/terminology",
                get(|| async {
                    axum::Json(json!({
                        "stats_pending": false, "stats_computed_at": "2026-10-14T10:00:00+00:00",
                        "database": {"bytes": 7200000000u64, "reclaimable_bytes": 1000000},
                        "value_sets": 21334, "concept_maps": 88, "code_system_count": 2, "total_concepts": 545668,
                        "systems": [
                            {"url": "http://snomed.info/sct", "title": "SNOMED CT", "versions": [
                                {"id": "sct|20250601", "version": "20250601", "content": "complete", "status": "active",
                                 "pinned": false, "current": true, "stats": {"concepts": 369000, "est_bytes": 2500000000u64},
                                 "imported_at": "2026-10-14T10:30:02+00:00", "source": "upload:SnomedCT.zip",
                                 "sha256": "abcdef0123456789abcdef", "job_id": "j-done", "db_growth_bytes": 3600000000i64},
                                {"id": "sct|20240101", "version": "20240101", "content": "complete", "status": "active",
                                 "pinned": false, "current": false, "stats": {"concepts": 360000, "est_bytes": 2400000000u64}}
                            ]},
                            {"url": "http://loinc.org", "title": "LOINC", "versions": [
                                {"id": "loinc|2.80", "version": "2.80", "content": "complete", "status": "active",
                                 "pinned": true, "current": true, "stats": {"concepts": 176668, "est_bytes": 2200000000u64}}
                            ]}
                        ]
                    }))
                }),
            )
            .route(
                "/admin/code-systems/dependents",
                get(|| async {
                    axum::Json(json!({
                        "code_system": {"pinned": false},
                        "stats": {"concepts": 360000, "est_bytes": 2400000000u64},
                        "dependents": {
                            "value_sets": [{"url": "http://example.org/vs/findings", "version": "1", "pins_version": false}],
                            "supplements": [], "concept_maps": [],
                            "other_versions": ["20250601"], "current_after_delete": "20250601"
                        }
                    }))
                }),
            )
            .route(
                "/admin/code-systems/delete",
                post(|State(cap): State<Captured>, req: Request| async move {
                    let (p, b) = req.into_parts();
                    capture(&cap, &p.method, p.uri.to_string(), b).await;
                    (StatusCode::ACCEPTED, axum::Json(job("j-del", "queued", "queued", 0.0)))
                }),
            )
            .route(
                "/admin/imports",
                post(|State(cap): State<Captured>, req: Request| async move {
                    let (p, b) = req.into_parts();
                    capture(&cap, &p.method, p.uri.to_string(), b).await;
                    (StatusCode::ACCEPTED, axum::Json(job("j-new", "queued", "queued", 0.0)))
                }),
            )
            .route(
                "/admin/code-systems/current",
                put(|State(cap): State<Captured>, req: Request| async move {
                    let (p, b) = req.into_parts();
                    capture(&cap, &p.method, p.uri.to_string(), b).await;
                    axum::Json(json!({"url": "http://snomed.info/sct"}))
                }),
            )
            .route(
                "/admin/jobs",
                get(|| async {
                    axum::Json(json!({"jobs": [
                        job("j-run", "running", "write", 41.0),
                        job("j-done", "succeeded", "done", 100.0)
                    ]}))
                }),
            )
            .route(
                "/admin/jobs/{id}",
                get(|Path(id): Path<String>| async move {
                    if id == "missing" {
                        return (StatusCode::NOT_FOUND, axum::Json(json!({"error": "not-found", "message": "job missing"}))).into_response();
                    }
                    let status = if id == "j-done" { "succeeded" } else { "running" };
                    axum::Json(job(&id, status, if status == "running" { "write" } else { "done" }, 41.0)).into_response()
                }),
            )
            .route(
                "/admin/jobs/{id}/log",
                get(|| async { axum::Json(json!({"lines": ["2026-10-14T10:00:02Z started", "2026-10-14T10:00:05Z phase write"]})) }),
            )
            .route(
                "/admin/jobs/{id}/cancel",
                post(|State(cap): State<Captured>, req: Request| async move {
                    let (p, b) = req.into_parts();
                    capture(&cap, &p.method, p.uri.to_string(), b).await;
                    axum::Json(job("j-run", "running", "write", 41.0))
                }),
            );
    }
    let router = router
        .fallback(|| async { (StatusCode::NOT_FOUND, "") })
        .with_state(cap.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}"), cap)
}

fn app(base: &str) -> Router {
    let state = Arc::new(helios_hts_ui::HtsUiState {
        fhir_version: "R4",
        version: "9.9.9-test",
        upstream: helios_hts_ui::UpstreamClient::new_with_timeouts(
            base,
            Duration::from_secs(5),
            Duration::from_secs(2),
        )
        .unwrap(),
        bundled_data_bytes: None,
        metrics_ring: Default::default(),
    });
    Router::new().nest("/ui", helios_hts_ui::router(state))
}

async fn get_page(app: &Router, uri: &str, lang: &str, htmx: bool) -> (StatusCode, String) {
    let mut req = axum::http::Request::get(uri).header(header::ACCEPT_LANGUAGE, lang);
    if htmx {
        req = req.header("HX-Request", "true");
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn post_form(app: &Router, uri: &str, form: &[(&str, &str)]) -> (StatusCode, String) {
    let mut ser = form_urlencoded::Serializer::new(String::new());
    for (k, v) in form {
        ser.append_pair(k, v);
    }
    let resp = app
        .clone()
        .oneshot(
            axum::http::Request::post(uri)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(ser.finish()))
                .unwrap(),
        )
        .await
        .unwrap();
    let loc = resp
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    (resp.status(), loc)
}

fn no_raw_keys(html: &str) {
    for prefix in [
        "hts-term-",
        "hts-del-",
        "hts-job-",
        "hts-jobs-",
        "hts-up-",
        "hts-admin-",
        "hts-reg-",
        "hts-dir-",
    ] {
        // Element ids reuse some prefixes; a raw key renders as visible text
        // `>hts-…<` or inside an attribute value that starts with it and has
        // no further markup — check the visible-text form.
        assert!(
            !html.contains(&format!(">{prefix}")),
            "raw catalog key with prefix {prefix} leaked"
        );
    }
}

#[tokio::test]
async fn inventory_lists_versions_with_current_pin_and_actions() {
    let (base, _) = start_mock(true).await;
    let app = app(&base);
    let (s, html) = get_page(&app, "/ui/hts/terminology", "en", false).await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains(">Terminology inventory<"));
    assert_eq!(html.matches("data-current=\"true\"").count(), 2);
    assert!(
        html.contains("3.6 GB"),
        "measured growth preferred over the estimate"
    );
    assert!(
        html.contains("≈ 2.4 GB"),
        "estimates are marked approximate"
    );
    assert!(html.contains("369,000"));
    assert!(html.contains(">pinned<"));
    assert!(html.contains(">Unpin<"), "pinned row offers unpin");
    assert!(
        html.contains(">Set as current<"),
        "multi-version system offers pinning"
    );
    assert!(html.contains(
        "/ui/hts/terminology/delete?url=http%3A%2F%2Fsnomed.info%2Fsct&amp;version=20240101"
    ));
    assert!(
        html.contains("/ui/hts/jobs/j-done"),
        "import time links to its job"
    );
    assert!(html.contains("Compact database"));
    no_raw_keys(&html);

    let (_, zh) = get_page(&app, "/ui/hts/terminology", "zh-TW", false).await;
    assert!(zh.contains("術語總覽"));
    assert!(zh.contains("設為目前版本"));
    no_raw_keys(&zh);
}

#[tokio::test]
async fn set_current_posts_pin_and_redirects_back() {
    let (base, cap) = start_mock(true).await;
    let app = app(&base);
    let (s, loc) = post_form(
        &app,
        "/ui/hts/terminology/current",
        &[
            ("url", "http://snomed.info/sct"),
            ("version", "20240101"),
            ("action", "pin"),
            ("back", "/ui/hts/terminology?q=sct"),
        ],
    )
    .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(loc, "/ui/hts/terminology?q=sct&notice=pinned");
    let c = cap.lock().await;
    let (m, p, b) = c.last().unwrap();
    assert_eq!(
        (m.as_str(), p.as_str()),
        ("PUT", "/admin/code-systems/current")
    );
    let body: Value = serde_json::from_str(b).unwrap();
    assert_eq!(body["version"], "20240101");
}

#[tokio::test]
async fn delete_confirmation_lists_dependents_and_requires_the_url() {
    let (base, cap) = start_mock(true).await;
    let app = app(&base);
    let (s, html) = get_page(
        &app,
        "/ui/hts/terminology/delete?url=http%3A%2F%2Fsnomed.info%2Fsct&version=20240101",
        "en",
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("http://example.org/vs/findings"));
    assert!(
        html.contains("version 20250601"),
        "names the version that takes over"
    );
    assert!(html.contains("1 dependent resource"));
    no_raw_keys(&html);

    let (s, loc) = post_form(
        &app,
        "/ui/hts/terminology/delete",
        &[
            ("url", "http://snomed.info/sct"),
            ("version", "20240101"),
            ("confirm", "wrong"),
        ],
    )
    .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(loc.contains("error=confirm"));
    assert!(
        cap.lock().await.is_empty(),
        "nothing is sent without confirmation"
    );

    let (s, loc) = post_form(
        &app,
        "/ui/hts/terminology/delete",
        &[
            ("url", "http://snomed.info/sct"),
            ("version", "20240101"),
            ("confirm", "http://snomed.info/sct"),
        ],
    )
    .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(loc, "/ui/hts/jobs/j-del");
    let c = cap.lock().await;
    let body: Value = serde_json::from_str(&c[0].2).unwrap();
    assert_eq!(
        body,
        json!({"url": "http://snomed.info/sct", "version": "20240101"})
    );
}

#[tokio::test]
async fn jobs_list_polls_while_active_and_detail_shows_progress() {
    let (base, cap) = start_mock(true).await;
    let app = app(&base);
    let (_, html) = get_page(&app, "/ui/hts/jobs", "en", false).await;
    assert!(
        html.contains(r#"hx-trigger="every 2s""#),
        "active jobs poll"
    );
    assert!(html.contains("batch 12 of 74"));
    assert!(html.contains(">Running<") && html.contains(">Succeeded<"));
    no_raw_keys(&html);

    let (_, frag) = get_page(&app, "/ui/hts/jobs", "en", true).await;
    assert!(
        frag.starts_with("<div id=\"hts-jobs-list\""),
        "htmx gets the fragment only"
    );

    let (_, detail) = get_page(&app, "/ui/hts/jobs/j-run", "en", false).await;
    assert!(detail.contains(r#"aria-current="step""#));
    assert!(detail.contains("data-percent=\"41\""));
    assert!(detail.contains("phase write"), "log tail rendered");
    assert!(detail.contains(">Cancel job<"));
    no_raw_keys(&detail);

    let (_, done) = get_page(&app, "/ui/hts/jobs/j-done?fragment=1", "zh-TW", false).await;
    assert!(!done.contains("hx-trigger"), "a finished job stops polling");
    assert!(done.contains("369,000"));
    assert!(done.contains("完成"));

    let (s, _) = get_page(&app, "/ui/hts/jobs/missing", "en", false).await;
    assert_eq!(s, StatusCode::OK, "admin error renders inside the page");

    let (s, loc) = post_form(&app, "/ui/hts/jobs/j-run/cancel", &[]).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(loc, "/ui/hts/jobs/j-run?notice=cancel-requested");
    assert!(
        cap.lock()
            .await
            .iter()
            .any(|(_, p, _)| p == "/admin/jobs/j-run/cancel")
    );
}

#[tokio::test]
async fn import_page_offers_archive_sources_and_queues_jobs() {
    let (base, cap) = start_mock(true).await;
    let app = app(&base);
    let (_, html) = get_page(&app, "/ui/hts/import", "en", false).await;
    assert!(html.contains(r#"id="hts-admin-import""#));
    assert!(html.contains(r#"data-api="/ui/hts/api/admin""#));
    assert!(html.contains(r#"value="import-dir""#) && html.contains(r#"value="registry""#));
    assert!(html.contains("loinc/Loinc_2.80.zip"));
    assert!(
        html.contains("SnomedCT.zip") && html.contains("50%"),
        "unfinished upload listed with progress"
    );
    assert!(
        html.contains(r#"value="u2""#),
        "completed upload selectable"
    );
    assert!(html.contains("snomed-rf2 *"), "licensed formats marked");
    assert!(html.contains("never downloads them"), "licence note");
    assert!(
        html.contains(r#"id="hts-import-bundle""#),
        "Bundle source still there"
    );
    no_raw_keys(&html);

    let (s, loc) = post_form(
        &app,
        "/ui/hts/import/job",
        &[
            ("admin_source", "import-dir"),
            ("path", "loinc/Loinc_2.80.zip"),
            ("format", "auto"),
            ("properties", "COMPONENT,SYSTEM"),
        ],
    )
    .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(loc, "/ui/hts/jobs/j-new");
    let body: Value = serde_json::from_str(&cap.lock().await.last().unwrap().2).unwrap();
    assert_eq!(
        body["source"],
        json!({"type": "import-dir", "path": "loinc/Loinc_2.80.zip"})
    );
    assert_eq!(body["format"], Value::Null);
    assert_eq!(body["properties"], "COMPONENT,SYSTEM");

    let (_, loc) = post_form(
        &app,
        "/ui/hts/import/job",
        &[
            ("admin_source", "registry"),
            ("package", "hl7.terminology.r4"),
        ],
    )
    .await;
    assert_eq!(loc, "/ui/hts/jobs/j-new");
    let body: Value = serde_json::from_str(&cap.lock().await.last().unwrap().2).unwrap();
    assert_eq!(body["source"]["package"], "hl7.terminology.r4");

    let (_, loc) = post_form(&app, "/ui/hts/import/job", &[("admin_source", "upload")]).await;
    assert!(
        loc.starts_with("/ui/hts/import?error="),
        "no upload chosen → back with error"
    );
}

#[tokio::test]
async fn admin_off_shows_how_to_enable_it() {
    let (base, _) = start_mock(false).await;
    let app = app(&base);
    for uri in ["/ui/hts/terminology", "/ui/hts/jobs", "/ui/hts/import"] {
        let (s, html) = get_page(&app, uri, "en", false).await;
        assert_eq!(s, StatusCode::OK, "{uri}");
        assert!(html.contains(r#"data-admin-state="disabled""#), "{uri}");
        assert!(html.contains("HTS_ADMIN_UI=true"), "{uri}");
        no_raw_keys(&html);
    }
    let (_, html) = get_page(&app, "/ui/hts/import", "en", false).await;
    assert!(
        html.contains(r#"id="hts-import-bundle""#),
        "Bundle import keeps working"
    );
    assert!(!html.contains(r#"id="hts-admin-import""#));
}

#[tokio::test]
async fn nav_lists_terminology_and_jobs() {
    let (base, _) = start_mock(true).await;
    let (_, html) = get_page(&app(&base), "/ui/hts/jobs", "de", false).await;
    assert!(html.contains(r#"href="/ui/hts/terminology""#));
    assert!(html.contains(r#"href="/ui/hts/jobs" aria-current="page""#));
    assert!(html.contains(">Aufträge<"));
}

// Keep the unused-import lint quiet for helpers some tests do not call.
#[allow(dead_code)]
fn _unused(_: Method) -> impl IntoResponse {
    StatusCode::OK
}
