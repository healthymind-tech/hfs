//! Navigation and information architecture (UI/UX review 2026-10-13 §4.2):
//! the editor and history pages name the resource they show and carry a
//! breadcrumb back into Resources, the sidebar keeps Resources current on
//! them, and the topbar product switcher links the two consoles.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn app_with_terminology(terminology: Option<&str>) -> Router {
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
        terminology.map(str::to_string),
        "http://localhost:8080".to_string(),
        None,
    )
}

fn app() -> Router {
    app_with_terminology(None)
}

async fn page(router: Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    let response = router
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

fn title(html: &str) -> &str {
    let start = html.find("<title>").expect("title") + "<title>".len();
    let end = html[start..].find("</title>").unwrap() + start;
    &html[start..end]
}

#[tokio::test]
async fn editor_names_the_resource_and_trails_back_to_resources() {
    let (status, _, html) = page(app(), "/ui/editor?type=Patient&id=abc").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        title(&html).starts_with("Patient/abc — "),
        "{}",
        title(&html)
    );
    assert!(
        html.contains(r#"<h1 class="page-head__title">Patient/abc</h1>"#),
        "h1 names the resource"
    );
    assert!(html.contains(r#"<nav class="breadcrumb" aria-label="Breadcrumb">"#));
    assert!(html.contains(r#"<a href="/ui/resources">Resources</a>"#));
    assert!(html.contains(r#"<a href="/ui/resources?type=Patient">Patient</a>"#));
    assert!(html.contains(r#"<span aria-current="page">abc</span>"#));
    // The editor lives inside the Resources workspace: its nav entry stays lit.
    assert!(html.contains(r#"href="/ui/resources" aria-current="true""#));
}

#[tokio::test]
async fn editor_without_an_id_is_titled_as_a_new_resource() {
    let (_, _, html) = page(app(), "/ui/editor?type=Observation").await;
    assert!(html.contains(r#"<h1 class="page-head__title">New Observation</h1>"#));
    assert!(html.contains(r#"<span aria-current="page">New</span>"#));
    assert!(title(&html).starts_with("New Observation — "));
}

#[tokio::test]
async fn editor_breadcrumb_escapes_what_the_query_carries() {
    let (_, _, html) = page(app(), "/ui/editor?type=Patient&id=%3Cb%3Ex").await;
    assert!(!html.contains("<b>x"), "the id is escaped");
}

#[tokio::test]
async fn history_deep_link_names_the_instance_and_links_its_editor() {
    let (status, _, html) = page(app(), "/ui/history?type=Patient&id=abc").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("<h1 class=\"page-head__title\">History · Patient/abc</h1>"),
        "{html}"
    );
    assert!(title(&html).starts_with("History · Patient/abc — "));
    assert!(
        html.contains(r#"<a href="/ui/editor?type=Patient&#38;id=abc">abc</a>"#)
            || html.contains(r#"<a href="/ui/editor?type=Patient&amp;id=abc">abc</a>"#),
        "{html}"
    );
    assert!(html.contains(r#"<span aria-current="page">History</span>"#));
    // A plain GET form, so loading another instance lands on its own URL.
    assert!(html.contains(
        r#"<form class="history__locate" id="history-locate" method="get" action="/ui/history">"#
    ));
    assert!(html.contains(r#"value="abc""#));
    assert!(html.contains(r#"href="/ui/resources" aria-current="true""#));

    let (_, _, html) = page(app(), "/ui/history").await;
    assert!(
        html.contains("<h1 class=\"page-head__title\">History &#38; Versions</h1>")
            || html.contains("<h1 class=\"page-head__title\">History &amp; Versions</h1>")
            || html.contains("<h1 class=\"page-head__title\">History & Versions</h1>")
    );
    assert!(html.contains(r#"<span aria-current="page">History</span>"#));
    assert!(!html.contains(r#"href="/ui/resources?type="#));
}

#[tokio::test]
async fn product_switcher_links_the_terminology_console() {
    // An internal terminology host: the browser is sent to /ui/hts on this
    // origin (HFS_TERMINOLOGY_PUBLIC_URL unset).
    let (_, _, html) = page(app_with_terminology(Some("http://127.0.0.1:8090")), "/ui").await;
    assert!(
        html.contains(r#"<nav class="product-switch" aria-label="Switch console">"#),
        "{html}"
    );
    assert!(html.contains(r#"href="/ui" aria-current="page">FHIR Server</a>"#));
    assert!(html.contains(r#"href="/ui/hts">Terminology</a>"#));

    // No terminology server: nothing to switch to.
    let (_, _, html) = page(app(), "/ui").await;
    assert!(!html.contains("product-switch"));
}

#[tokio::test]
async fn pages_announce_the_console_to_a_same_origin_hts() {
    let (_, headers, _) = page(app(), "/ui").await;
    let cookies: Vec<_> = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    assert!(
        cookies
            .iter()
            .any(|c| c.starts_with("helios_hfs_console=1;") && c.contains("Path=/")),
        "{cookies:?}"
    );

    // Already known: not sent again.
    let response = app()
        .oneshot(
            Request::get("/ui")
                .header(header::COOKIE, "helios_hfs_console=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        !response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .any(|c| c.to_str().unwrap_or("").starts_with("helios_hfs_console"))
    );

    // Not on assets.
    let (_, headers, _) = page(app(), "/ui/assets/app.css").await;
    assert!(
        !headers
            .get_all(header::SET_COOKIE)
            .iter()
            .any(|c| c.to_str().unwrap_or("").starts_with("helios_hfs_console"))
    );
}
