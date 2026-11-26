//! Two stores in one process answer from their own content, and a canonical
//! URL stored in several versions resolves to the newest however the versions
//! arrived.
//!
//! The SQLite backend memoises "which `code_systems` row does this URL
//! resolve to" process-wide. A process holds several databases (the main
//! store and the `tx-resource` overlays; here, two test apps), so the memo is
//! kept per database: resolved once in one store, a URL must not answer with
//! that store's row in another.
//!
//! Runs on SQLite, or on PostgreSQL with `HTS_TEST_BACKEND=postgres`.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};

fn code_system(url: &str, id: &str, version: &str, codes: &[&str]) -> Value {
    json!({
        "resourceType": "CodeSystem", "id": id, "url": url, "version": version,
        "status": "active", "content": "complete",
        "concept": codes.iter().map(|c| json!({"code": c, "display": format!("Display {c}")})).collect::<Vec<_>>()
    })
}

fn value_set(url: &str, id: &str, system: &str) -> Value {
    json!({
        "resourceType": "ValueSet", "id": id, "url": url, "version": "1", "status": "active",
        "compose": {"include": [{"system": system}]}
    })
}

fn bundle(resources: &[Value]) -> String {
    json!({
        "resourceType": "Bundle", "type": "collection",
        "entry": resources.iter().map(|r| json!({"resource": r})).collect::<Vec<_>>()
    })
    .to_string()
}

async fn codes(app: &TestApp, url: &str) -> Vec<String> {
    let path = format!("/ValueSet/$expand?url={}", url.replace('?', "%3F"));
    let (status, body) = app.get_fhir(&path).await;
    assert_eq!(status, StatusCode::OK, "{path}: {body}");
    let mut codes: Vec<String> = body["expansion"]["contains"]
        .as_array()
        .map(|c| {
            c.iter()
                .filter_map(|e| e["code"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    codes.sort();
    codes
}

async fn lookup_display(app: &TestApp, system: &str, code: &str) -> (StatusCode, Value) {
    app.get_fhir(&format!("/CodeSystem/$lookup?system={system}&code={code}"))
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn two_stores_with_the_same_code_system_url_answer_from_their_own_rows() {
    const CS: &str = "http://hts.test/isolation/cs";
    const VS: &str = "http://hts.test/isolation/vs";
    let implicit = format!("{CS}?fhir_vs");

    let first = TestApp::new().await;
    first
        .import_bundle_ok(&bundle(&[
            code_system(CS, "iso-first", "1", &["a1", "a2"]),
            value_set(VS, "iso-vs", CS),
        ]))
        .await;
    let second = TestApp::new().await;
    second
        .import_bundle_ok(&bundle(&[
            code_system(CS, "iso-second", "7", &["b1", "b2", "b3"]),
            value_set(VS, "iso-vs", CS),
        ]))
        .await;

    // Asked in turn, repeatedly: neither store may answer from the row the
    // other resolved the URL to.
    for _ in 0..3 {
        assert_eq!(codes(&first, VS).await, ["a1", "a2"]);
        assert_eq!(codes(&second, VS).await, ["b1", "b2", "b3"]);
        assert_eq!(codes(&first, &implicit).await, ["a1", "a2"]);
        assert_eq!(codes(&second, &implicit).await, ["b1", "b2", "b3"]);
        let (status, body) = lookup_display(&second, CS, "b3").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = lookup_display(&first, CS, "a2").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, _) = lookup_display(&first, CS, "b3").await;
        assert_ne!(status, StatusCode::OK, "b3 is not in the first store");
    }
}

/// Two versions of one CodeSystem in a single Bundle: a version-less
/// expansion uses the newest, as when they are imported one at a time.
#[tokio::test(flavor = "multi_thread")]
async fn two_versions_in_one_bundle_resolve_to_the_newest() {
    const CS: &str = "http://hts.test/isolation/two-versions/cs";
    const VS: &str = "http://hts.test/isolation/two-versions/vs";
    let implicit = format!("{CS}?fhir_vs");
    let older = code_system(CS, "two-v1", "1", &["old1", "old2", "old3"]);
    let newer = code_system(CS, "two-v2", "2", &["new1", "new2"]);

    for (order, resources) in [
        ("older first", [older.clone(), newer.clone()]),
        ("newer first", [newer.clone(), older.clone()]),
    ] {
        let app = TestApp::new().await;
        let mut all = resources.to_vec();
        all.push(value_set(VS, "two-vs", CS));
        app.import_bundle_ok(&bundle(&all)).await;
        assert_eq!(codes(&app, VS).await, ["new1", "new2"], "{order}");
        assert_eq!(codes(&app, &implicit).await, ["new1", "new2"], "{order}");
        let pinned = format!("/ValueSet/$expand?url={VS}&system-version={CS}%7C1");
        let (status, body) = app.get_fhir(&pinned).await;
        assert_eq!(status, StatusCode::OK, "{order}: {body}");
    }

    let app = TestApp::new().await;
    app.import_bundle_ok(&bundle(&[older, value_set(VS, "two-vs", CS)]))
        .await;
    assert_eq!(codes(&app, VS).await, ["old1", "old2", "old3"]);
    app.import_bundle_ok(&bundle(&[newer])).await;
    assert_eq!(codes(&app, VS).await, ["new1", "new2"]);
    assert_eq!(codes(&app, &implicit).await, ["new1", "new2"]);
}
