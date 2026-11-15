//! The topbar product switcher and the brand link (UI/UX review §4.2): HTS
//! links back to the FHIR console only when an HFS console on the same
//! origin has announced itself with its cookie.

use axum::{
    Router,
    body::Body,
    http::{Request, header},
};
use http_body_util::BodyExt;
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

fn app() -> Router {
    let state = Arc::new(helios_hts_ui::HtsUiState {
        fhir_version: "R4",
        version: "9.9.9-test",
        upstream: helios_hts_ui::UpstreamClient::new_with_timeouts(
            "http://127.0.0.1:1",
            Duration::from_millis(250),
            Duration::from_millis(100),
        )
        .expect("closed loopback URL always parses"),
        bundled_data_bytes: None,
        metrics_ring: Default::default(),
    });
    Router::new().nest("/ui", helios_hts_ui::router(state))
}

async fn get(uri: &str, cookie: Option<&str>) -> String {
    let mut request = Request::get(uri);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    let response = app()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn switcher_links_back_to_a_same_origin_fhir_console() {
    for uri in ["/ui/hts", "/ui/hts/value-sets", "/ui/hts/no-such-page"] {
        let html = get(uri, Some("hfs_lang=en; helios_hfs_console=1")).await;
        assert!(
            html.contains(r#"<nav class="product-switch""#),
            "{uri}: {html}"
        );
        assert!(html.contains(r#"href="/ui">FHIR Server</a>"#), "{uri}");
        assert!(
            html.contains(r#"href="/ui/hts" aria-current="page">Terminology</a>"#),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn a_stand_alone_hts_shows_no_switcher() {
    let html = get("/ui/hts", None).await;
    assert!(!html.contains("product-switch"));
}

#[tokio::test]
async fn the_brand_links_home() {
    let html = get("/ui/hts/code-systems", None).await;
    assert!(
        html.contains(r#"<a class="brand" href="/ui/hts""#),
        "{html}"
    );
}
