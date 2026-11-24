//! `_hm-current`: version-grouped searches of the terminology resources —
//! one entry per canonical URL (the version answering version-less requests,
//! by the resolvers' own precedence), counts and paging on groups, a filter
//! match on any version keeps its group, `_hm-current=older` lists the rest.
//!
//! Runs on SQLite, or on PostgreSQL with `HTS_TEST_BACKEND=postgres`.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::Value;

const BUNDLE: &str = r#"{
  "resourceType": "Bundle",
  "type": "collection",
  "entry": [
    {"resource": {"resourceType": "CodeSystem", "id": "vg-a-1", "url": "http://hts.test/vg/cs/a", "version": "1.0",
      "name": "Alpha", "title": "Legacy Alpha", "status": "retired", "date": "2020-01-01",
      "content": "complete", "concept": [{"code": "a", "display": "A"}]}},
    {"resource": {"resourceType": "CodeSystem", "id": "vg-a-2", "url": "http://hts.test/vg/cs/a", "version": "2.0",
      "name": "Alpha", "title": "Alpha", "status": "active", "date": "2022-01-01",
      "content": "complete", "concept": [{"code": "a", "display": "A"}, {"code": "b", "display": "B"}]}},
    {"resource": {"resourceType": "CodeSystem", "id": "vg-a-3", "url": "http://hts.test/vg/cs/a", "version": "3.0",
      "name": "Alpha", "title": "Alpha", "status": "draft", "date": "2024-01-01",
      "content": "not-present"}},
    {"resource": {"resourceType": "CodeSystem", "id": "vg-b", "url": "http://hts.test/vg/cs/b", "version": "1",
      "name": "Beta", "title": "Beta", "status": "active",
      "content": "complete", "concept": [{"code": "x", "display": "X"}]}},
    {"resource": {"resourceType": "ValueSet", "id": "vg-v-1", "url": "http://hts.test/vg/vs/v", "version": "1.0.0",
      "name": "Vee", "title": "Vee", "status": "active", "date": "2021-05-05",
      "compose": {"include": [{"system": "http://hts.test/vg/cs/b"}]}}},
    {"resource": {"resourceType": "ValueSet", "id": "vg-v-2", "url": "http://hts.test/vg/vs/v", "version": "1.10.0",
      "name": "Vee", "title": "Vee", "status": "active", "date": "2023-05-05",
      "compose": {"include": [{"system": "http://hts.test/vg/cs/b"}]}}},
    {"resource": {"resourceType": "ValueSet", "id": "vg-v-3", "url": "http://hts.test/vg/vs/v", "version": "1.9.0",
      "name": "Vee", "title": "Vee", "status": "active",
      "compose": {"include": [{"system": "http://hts.test/vg/cs/b"}]}}}
  ]
}"#;

fn entries(body: &Value) -> Vec<&Value> {
    body["entry"]
        .as_array()
        .map(|e| e.iter().map(|x| &x["resource"]).collect())
        .unwrap_or_default()
}

fn version_count(resource: &Value) -> i64 {
    resource["meta"]["extension"]
        .as_array()
        .and_then(|e| e.iter().find(|x| x["url"].as_str().is_some_and(|u| u.ends_with("hm-version-count"))))
        .and_then(|x| x["valueInteger"].as_i64())
        .unwrap_or(-1)
}

async fn get(app: &TestApp, path: &str) -> Value {
    let (status, body) = app.get_fhir(path).await;
    assert_eq!(status, StatusCode::OK, "{path}: {body}");
    body
}

#[tokio::test(flavor = "multi_thread")]
async fn one_entry_per_canonical_with_the_resolvers_choice() {
    let app = TestApp::new().await;
    app.import_bundle_ok(BUNDLE).await;

    let all = get(&app, "/CodeSystem?url=http://hts.test/vg/cs/a&_count=50").await;
    assert_eq!(all["total"], 3, "three stored versions: {all}");

    let grouped = get(&app, "/CodeSystem?_hm-current=true&title:contains=a&_count=50").await;
    let rows = entries(&grouped);
    let urls: Vec<&str> = rows.iter().map(|r| r["url"].as_str().unwrap()).collect();
    let mut unique = urls.clone();
    unique.dedup();
    assert_eq!(urls, unique, "no duplicate canonical on a page: {grouped}");
    let a = rows.iter().find(|r| r["url"] == "http://hts.test/vg/cs/a").expect("group a");
    // 3.0 is newest but content=not-present: the precedence order picks the
    // populated 2.0, exactly as a version-less $lookup would.
    assert_eq!(a["version"], "2.0", "{a}");
    assert_eq!(version_count(a), 3);
    assert_eq!(a["date"], "2022-01-01");

    // The version a version-less read resolves to is the same row.
    let read = get(&app, "/CodeSystem?url=http://hts.test/vg/cs/a&_count=1").await;
    assert_eq!(entries(&read)[0]["version"], "2.0");

    // ValueSet: semantic version order (1.10.0 > 1.9.0 > 1.0.0).
    let vs = get(&app, "/ValueSet?_hm-current=true&url=http://hts.test/vg/vs/v").await;
    assert_eq!(vs["total"], 1);
    assert_eq!(entries(&vs)[0]["version"], "1.10.0");
    assert_eq!(version_count(entries(&vs)[0]), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn counts_and_pages_operate_on_groups() {
    let app = TestApp::new().await;
    app.import_bundle_ok(BUNDLE).await;

    let first = get(&app, "/CodeSystem?_hm-current=true&name:contains=a&_count=1&_sort=name").await;
    // Alpha and Beta both contain "a": two groups, four stored versions.
    assert_eq!(first["total"], 2, "{first}");
    assert_eq!(entries(&first).len(), 1);
    assert_eq!(entries(&first)[0]["name"], "Alpha");
    let second = get(&app, "/CodeSystem?_hm-current=true&name:contains=a&_count=1&_offset=1&_sort=name").await;
    assert_eq!(entries(&second)[0]["name"], "Beta");

    let count = get(&app, "/CodeSystem?_hm-current=true&name:contains=a&_summary=count").await;
    assert_eq!(count["total"], 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_match_on_an_older_version_keeps_the_group_and_older_lists_the_rest() {
    let app = TestApp::new().await;
    app.import_bundle_ok(BUNDLE).await;

    // Only version 1.0 is titled "Legacy Alpha"; the group still shows, with
    // its current version.
    let legacy = get(&app, "/CodeSystem?_hm-current=true&title:contains=legacy").await;
    assert_eq!(legacy["total"], 1);
    assert_eq!(entries(&legacy)[0]["version"], "2.0");

    let older = get(&app, "/CodeSystem?_hm-current=older&url=http://hts.test/vg/cs/a").await;
    assert_eq!(older["total"], 2, "{older}");
    let versions: Vec<&str> = entries(&older).iter().map(|r| r["version"].as_str().unwrap()).collect();
    assert!(versions.contains(&"1.0") && versions.contains(&"3.0") && !versions.contains(&"2.0"), "{versions:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_combinations_are_refused() {
    let app = TestApp::new().await;
    for path in [
        "/CodeSystem?_hm-current=true&_id=vg-b",
        "/CodeSystem?_hm-current=maybe",
        "/ValueSet?_hm-current=true&publisher=x",
    ] {
        let (status, body) = app.get_fhir(path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
    }
}
