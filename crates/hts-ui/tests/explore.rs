//! Terminology explorer routes (HealthyMind): the page, the `explore/data`
//! pass-through to `$hm-explore` and the `explore/concept` side-panel JSON,
//! against an in-process mock terminology server that records what it was
//! asked.

use axum::{
    Router,
    body::Body,
    extract::{RawQuery, Request, State},
    http::{StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tower::ServiceExt;

type Seen = Arc<Mutex<Vec<String>>>;

async fn explore_mock(State(seen): State<Seen>, RawQuery(q): RawQuery) -> impl IntoResponse {
    let q = q.unwrap_or_default();
    seen.lock().await.push(q.clone());
    if q.contains("system=http%3A%2F%2Fmissing") {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/fhir+json")],
            json!({"resourceType": "OperationOutcome", "issue": [{"severity": "error", "code": "not-found", "diagnostics": "CodeSystem http://missing is not stored"}]}).to_string(),
        );
    }
    let body = if q.contains("mode=stats") {
        json!({"mode": "stats", "url": "http://snomed.info/sct", "version": "20250601", "title": "SNOMED CT",
               "concepts": 522354, "hierarchy": true, "content": "complete", "id": "sct"})
    } else {
        json!({"mode": "children", "total": 1, "items": [{"code": "138875005", "display": "SNOMED CT Concept", "children": 19}]})
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
}

async fn lookup_mock() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/fhir+json")],
        json!({"resourceType": "Parameters", "parameter": [
            {"name": "display", "valueString": "Type 2 diabetes mellitus"},
            {"name": "system", "valueUri": "http://snomed.info/sct"},
            {"name": "code", "valueCode": "44054006"},
            {"name": "designation", "part": [
                {"name": "language", "valueCode": "zh-Hant"},
                {"name": "value", "valueString": "第二型糖尿病"}]},
            {"name": "property", "part": [
                {"name": "code", "valueCode": "parent"},
                {"name": "value", "valueCode": "73211009"},
                {"name": "description", "valueString": "Diabetes mellitus"}]}
        ]})
        .to_string(),
    )
}

async fn translate_mock(request: Request) -> impl IntoResponse {
    let body = request.into_body().collect().await.unwrap().to_bytes();
    let reverse = String::from_utf8_lossy(&body).contains("targetCode");
    let params = if reverse {
        json!({"resourceType": "Parameters", "parameter": [{"name": "result", "valueBoolean": false}]})
    } else {
        json!({"resourceType": "Parameters", "parameter": [
            {"name": "result", "valueBoolean": true},
            {"name": "match", "part": [
                {"name": "equivalence", "valueCode": "equivalent"},
                {"name": "concept", "valueCoding": {"system": "http://hl7.org/fhir/sid/icd-10-cm", "code": "E11.9", "display": "Type 2 diabetes mellitus without complications"}},
                {"name": "originMap", "valueUri": "http://example.org/cm/sct-icd"}]}
        ]})
    };
    (
        [(header::CONTENT_TYPE, "application/fhir+json")],
        params.to_string(),
    )
}

async fn start_mock() -> (String, Seen) {
    let seen: Seen = Arc::default();
    let app = Router::new()
        .route("/CodeSystem/$hm-explore", get(explore_mock))
        .route("/CodeSystem/$lookup", post(lookup_mock))
        .route("/ConceptMap/$translate", post(translate_mock))
        .route("/__mock_ready", get(|| async { "ok" }))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://{addr}");
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for _ in 0..50 {
        if client
            .get(format!("{base}/__mock_ready"))
            .send()
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (base, seen)
}

fn app(base: &str) -> Router {
    let state = Arc::new(helios_hts_ui::HtsUiState {
        fhir_version: "R4",
        version: "9.9.9-test",
        upstream: helios_hts_ui::UpstreamClient::new_with_timeouts(
            base,
            Duration::from_secs(30),
            Duration::from_secs(10),
        )
        .unwrap(),
        bundled_data_bytes: None,
        metrics_ring: Default::default(),
    });
    Router::new().nest("/ui", helios_hts_ui::router(state))
}

async fn get_text(app: &Router, uri: &str) -> (StatusCode, String, String) {
    let res = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let ct = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, ct, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn page_picks_the_kind_and_embeds_the_config() {
    let (base, seen) = start_mock().await;
    let app = app(&base);
    let (status, _, html) = get_text(
        &app,
        "/ui/hts/explore?system=http%3A%2F%2Fsnomed.info%2Fsct",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains(r#"data-kind="snomed""#), "{html}");
    assert!(html.contains("term-explorer.js"));
    assert!(html.contains("term-explorer.css"));
    assert!(html.contains("SNOMED CT"));
    let start = html.find(r#"id="tx-explorer-config">"#).unwrap();
    let rest = &html[start + r#"id="tx-explorer-config">"#.len()..];
    let json_text = &rest[..rest.find("</script>").unwrap()];
    let cfg: Value = serde_json::from_str(json_text).unwrap();
    assert_eq!(cfg["kind"], "snomed");
    assert_eq!(cfg["version"], "20250601");
    assert_eq!(cfg["dataUrl"], "/ui/hts/explore/data");
    assert!(
        cfg["strings"]["tx-explore-more-remaining"]
            .as_str()
            .unwrap()
            .contains("__N__")
    );
    let asked = seen.lock().await.clone();
    assert!(asked.iter().any(|q| q.contains("mode=stats")), "{asked:?}");
    // SNOMED gets no property census.
    assert!(!asked.iter().any(|q| q.contains("props=1")), "{asked:?}");
}

#[tokio::test]
async fn unknown_systems_render_the_outcome() {
    let (base, _) = start_mock().await;
    let app = app(&base);
    let (status, _, html) = get_text(&app, "/ui/hts/explore?system=http%3A%2F%2Fmissing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(html.contains("is not stored"), "{html}");
    assert!(!html.contains("term-explorer.js"));
    let (status, _, _) = get_text(&app, "/ui/hts/explore").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn data_forwards_only_explore_parameters() {
    let (base, seen) = start_mock().await;
    let app = app(&base);
    let (status, ct, body) = get_text(
        &app,
        "/ui/hts/explore/data?mode=children&system=http%3A%2F%2Fsnomed.info%2Fsct&code=1&filter=A%3D1&_format=xml&evil=1",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(ct.starts_with("application/json"));
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["items"][0]["code"], "138875005");
    let asked = seen.lock().await.clone();
    let q = asked.last().unwrap();
    assert!(q.contains("filter=A%3D1"), "{q}");
    assert!(!q.contains("evil") && !q.contains("_format"), "{q}");

    let (status, _, body) = get_text(&app, "/ui/hts/explore/data?mode=children").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _, body) = get_text(
        &app,
        "/ui/hts/explore/data?mode=stats&system=http%3A%2F%2Fmissing",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("is not stored"), "{body}");
}

#[tokio::test]
async fn concept_json_has_designations_relatives_and_mappings() {
    let (base, _) = start_mock().await;
    let app = app(&base);
    let (status, ct, body) = get_text(
        &app,
        "/ui/hts/explore/concept?system=http%3A%2F%2Fsnomed.info%2Fsct&code=44054006",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(ct.starts_with("application/json"));
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["display"], "Type 2 diabetes mellitus");
    assert_eq!(v["designations"][0]["language"], "zh-Hant");
    assert_eq!(v["parents"][0]["code"], "73211009");
    assert_eq!(v["mappings"][0]["code"], "E11.9");
    assert_eq!(v["mappings"][0]["direction"], "forward");
    assert_eq!(v["mappings"][0]["map"], "http://example.org/cm/sct-icd");
    assert!(v["permalink"].as_str().unwrap().contains("code=44054006"));
    let (status, _, _) = get_text(&app, "/ui/hts/explore/concept?code=1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[test]
fn explorer_catalogs_define_the_same_keys() {
    fn keys(locale: &str) -> std::collections::BTreeSet<String> {
        let path = format!(
            "{}/locales/{locale}/explorer.ftl",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .lines()
            .filter_map(|l| l.split_once(" = ").map(|(k, _)| k.trim().to_string()))
            .filter(|k| k.starts_with("tx-explore-"))
            .collect()
    }
    let en = keys("en");
    assert!(en.len() > 100);
    for locale in ["zh-Hant", "de", "es"] {
        assert_eq!(keys(locale), en, "{locale}/explorer.ftl");
    }
    let src = include_str!("../src/explore.rs");
    for line in src.lines() {
        let l = line.trim();
        if let Some(k) = l
            .strip_prefix("\"tx-explore-")
            .and_then(|r| r.strip_suffix("\","))
        {
            assert!(
                en.contains(&format!("tx-explore-{k}")),
                "tx-explore-{k} missing in en"
            );
        }
    }
}
