//! The Terminology section embedded in the console (unified-server design
//! §7.1, §9): pages render inside this console's shell, the boosted
//! (in-place) request gets the whole page, and `/ui/hts/**` redirects.
//!
//! Its own test binary: the section is installed process-wide.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn app() -> Router {
    helios_ui::terminology_console::install(helios_ui::terminology_console::TerminologyConsoleConfig {
        // Nothing listens there: the pages render their degraded state.
        upstream_url: "http://127.0.0.1:1".into(),
        admin_token: Some("t".into()),
        fhir_version: "R4",
        version: "9.9.9",
    });
    helios_ui::mount_with_conformance_source(
        Router::new(),
        "9.9.9",
        Some(std::path::PathBuf::from("../../data")),
        helios_ui::NlSearch::default(),
        None,
        None,
        "default".to_string(),
        std::sync::Arc::new(helios_ui::StaticConformanceSource::empty()),
        helios_fhir::FhirVersion::R4,
        Some("http://127.0.0.1:1".into()),
        "http://localhost:8080".to_string(),
        None,
    )
}

async fn get(uri: &str, headers: &[(&str, &str)]) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut req = Request::get(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let response = app().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = response.status();
    let h = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, h, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn terminology_pages_render_inside_the_console_shell() {
    for path in [
        "/ui/terminology",
        "/ui/terminology/code-systems",
        "/ui/terminology/value-sets",
        "/ui/terminology/concept-maps",
        "/ui/terminology/capability-statement",
    ] {
        let (status, headers, html) = get(path, &[]).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(html.contains(r#"<aside class="sidebar" id="sidebar">"#), "{path}: the console sidebar");
        assert!(html.contains(r#"id="nav-progress""#), "{path}: the console shell");
        assert!(html.contains(r#"hx-boost="true""#), "{path}");
        assert!(!html.contains("product-switch"), "{path}: no product switcher");
        assert!(!html.contains("/ui/hts/assets"), "{path}: assets from /ui/assets");
        assert!(
            headers.contains_key(header::CONTENT_SECURITY_POLICY),
            "{path}: CSP"
        );
        assert_eq!(html.matches("<html").count(), 1, "{path}: one document");
    }
}

#[tokio::test]
async fn a_boosted_request_gets_the_whole_page_not_a_fragment() {
    let (status, _, html) = get(
        "/ui/terminology/code-systems",
        &[("hx-request", "true"), ("hx-boosted", "true")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains(r#"id="main""#), "the page, so the shell can swap #main");
    assert!(html.contains(r#"id="hts-cs-rows""#));
}

#[tokio::test]
async fn links_point_at_the_terminology_section() {
    let (_, _, html) = get("/ui/terminology/code-systems", &[]).await;
    assert!(html.contains(r#"action="/ui/terminology/code-systems/rows""#));
    assert!(!html.contains(r#"href="/ui/hts"#));
}

#[tokio::test]
async fn legacy_paths_redirect() {
    let (status, headers, _) = get("/ui/hts/terminology?q=x", &[]).await;
    assert_eq!(status, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(headers[header::LOCATION], "/ui/terminology/manage?q=x");
}
