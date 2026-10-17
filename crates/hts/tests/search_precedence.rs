//! A search restricted to one canonical URL lists the default version first
//! whatever other parameters narrow it (`name`, `title`, `_id`, CodeSystem
//! `system`, …), with paging over that same order. Searches spanning several
//! canonicals keep insertion order.
//!
//! Before, only a bare `url=` search used the precedence order; adding
//! `name=` sent it down the string-matching path, which listed rows in import
//! order — so `GET /CodeSystem?url=http://loinc.org&name=LOINC` led with the
//! THO stub (`content=not-present`, version 3.1.0) imported first.
//!
//! Runs on SQLite by default and on PostgreSQL with
//! `HTS_TEST_BACKEND=postgres` (feature `postgres`).

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};

const CS_URL: &str = "http://hts.test/search-precedence/loinc";
const VS_URL: &str = "http://hts.test/search-precedence/vs";
const CM_URL: &str = "http://hts.test/search-precedence/cm";

fn bundle(resources: Vec<Value>) -> String {
    json!({
        "resourceType": "Bundle", "type": "collection",
        "entry": resources.into_iter().map(|r| json!({"resource": r})).collect::<Vec<_>>()
    })
    .to_string()
}

fn cs(id: &str, version: &str, content: &str) -> Value {
    let mut cs = json!({
        "resourceType": "CodeSystem", "id": id, "url": CS_URL, "version": version,
        "name": "LOINC", "title": "LOINC", "status": "active", "content": content
    });
    if content == "complete" {
        cs["concept"] = json!([{"code": "1-1", "display": "One"}]);
    }
    cs
}

fn vs(version: &str) -> Value {
    json!({
        "resourceType": "ValueSet", "id": "vs", "url": VS_URL, "version": version,
        "name": "Answers", "title": "Answers", "status": "active",
        "compose": {"include": [{"system": CS_URL, "concept": [{"code": "1-1"}]}]}
    })
}

fn cm(version: &str) -> Value {
    json!({
        "resourceType": "ConceptMap", "id": "cm", "url": CM_URL, "version": version,
        "name": "Map", "title": "Map", "status": "active",
        "group": [{"source": CS_URL, "target": CS_URL, "element": [
            {"code": "1-1", "target": [{"code": "1-1", "equivalence": "equivalent"}]}]}]
    })
}

fn enc(s: &str) -> String {
    s.replace(':', "%3A").replace('/', "%2F")
}

/// Oldest-first imports: the stub, then 2.80, then 2.83; ValueSet and
/// ConceptMap 1.9 before 1.10.
async fn app() -> TestApp {
    let app = TestApp::new().await;
    for resources in [
        vec![cs("stub", "3.1.0", "not-present"), vs("1.9"), cm("1.9")],
        vec![cs("loinc", "2.80", "complete")],
        vec![cs("loinc", "2.83", "complete"), vs("1.10"), cm("1.10")],
    ] {
        app.import_bundle_ok(&bundle(resources)).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    app
}

async fn versions(app: &TestApp, path: &str) -> Vec<String> {
    let (status, body) = app.get_fhir(path).await;
    assert_eq!(status, StatusCode::OK, "{path}: {body}");
    body["entry"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| {
                    e["resource"]["version"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn filtered_single_url_searches_follow_precedence() {
    let app = app().await;
    let url = enc(CS_URL);
    let default_first = ["2.83", "2.80", "3.1.0"];
    for query in [
        format!("url={url}"),
        format!("url={url}&name=LOINC"),
        format!("url={url}&name:contains=oin"),
        format!("url={url}&title=LOINC&status=active"),
        format!("system={url}"),
        format!("system={url}&name=LOINC"),
        format!("url={url}&name=LOINC&_summary=true"),
    ] {
        assert_eq!(
            versions(&app, &format!("/CodeSystem?{query}")).await,
            default_first,
            "{query}"
        );
    }
    // Paging walks the same order.
    let mut paged = Vec::new();
    for offset in 0..3 {
        paged.extend(
            versions(
                &app,
                &format!("/CodeSystem?url={url}&name=LOINC&_count=1&_offset={offset}"),
            )
            .await,
        );
    }
    assert_eq!(paged, default_first);
    let (_, body) = app
        .get_fhir(&format!("/CodeSystem?url={url}&name=LOINC&_count=1"))
        .await;
    assert_eq!(body["total"], 3, "{body}");

    let vs_url = enc(VS_URL);
    for query in [
        format!("url={vs_url}&name=Answers"),
        format!("url={vs_url}&title=Answers"),
    ] {
        assert_eq!(
            versions(&app, &format!("/ValueSet?{query}")).await,
            ["1.10", "1.9"],
            "{query}"
        );
    }
    let cm_url = enc(CM_URL);
    for query in [
        format!("url={cm_url}&name=Map"),
        format!("url={cm_url}&title=Map"),
    ] {
        assert_eq!(
            versions(&app, &format!("/ConceptMap?{query}")).await,
            ["1.10", "1.9"],
            "{query}"
        );
    }
}

#[tokio::test]
async fn searches_across_canonicals_keep_import_order() {
    let app = app().await;
    assert_eq!(
        versions(&app, "/CodeSystem?name=LOINC").await,
        ["3.1.0", "2.80", "2.83"]
    );
    assert_eq!(
        versions(&app, "/ValueSet?name=Answers").await,
        ["1.9", "1.10"]
    );
    assert_eq!(
        versions(&app, "/ConceptMap?name=Map").await,
        ["1.9", "1.10"]
    );
}
