//! The default (versionless) version of a CodeSystem must be the one with
//! content, then the newest — never an empty stub that merely carries a
//! higher version or was imported first.
//!
//! Production reproduction: `http://loinc.org` holds LOINC 2.80 and 2.83
//! (`content=complete`) plus the HL7 THO stub `v3-loinc` (version `3.1.0`,
//! `content=not-present`, no concepts), imported first. A versionless
//! `$expand` expanded the 2.83 concepts but reported
//! `used-codesystem = http://loinc.org|3.1.0`, because the reported version
//! came from `search(url, count: 1)`, which listed rows by `created_at`.
//!
//! Runs on SQLite by default and on PostgreSQL with
//! `HTS_TEST_BACKEND=postgres` (feature `postgres`).

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};

const CS_URL: &str = "http://hts.test/default-version/loinc";

/// The THO-style stub: highest version, no concepts.
fn stub_bundle() -> String {
    json!({
        "resourceType": "Bundle", "type": "collection",
        "entry": [{"resource": {
            "resourceType": "CodeSystem", "id": "v3-loinc-stub", "url": CS_URL,
            "version": "3.1.0", "name": "LOINCStub", "status": "active",
            "content": "not-present"
        }}]
    })
    .to_string()
}

fn real_cs(version: &str, display_suffix: &str) -> Value {
    json!({
        "resourceType": "CodeSystem", "id": format!("loinc-{version}"), "url": CS_URL,
        "version": version, "name": "LOINC", "status": "active", "content": "complete",
        "property": [{"code": "SCALE_TYP", "type": "string"}],
        "concept": [
            {"code": "1-1", "display": format!("Note {display_suffix}"),
             "property": [{"code": "SCALE_TYP", "valueString": "DOC"}]},
            {"code": "2-2", "display": format!("Glucose {display_suffix}"),
             "property": [{"code": "SCALE_TYP", "valueString": "QN"}]}
        ]
    })
}

fn real_bundle() -> String {
    json!({
        "resourceType": "Bundle", "type": "collection",
        "entry": [
            {"resource": real_cs("2.80", "v2.80")},
            {"resource": real_cs("2.83", "v2.83")}
        ]
    })
    .to_string()
}

/// Stub first, so it is the oldest row (what insertion order would pick).
async fn app() -> TestApp {
    let app = TestApp::new().await;
    app.import_bundle_ok(&stub_bundle()).await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    app.import_bundle_ok(&real_bundle()).await;
    app
}

fn params_named<'a>(body: &'a Value, name: &str) -> Vec<&'a Value> {
    body["expansion"]["parameter"]
        .as_array()
        .map(|a| a.iter().filter(|p| p["name"] == name).collect())
        .unwrap_or_default()
}

fn out_param<'a>(body: &'a Value, name: &str) -> Option<&'a Value> {
    body["parameter"]
        .as_array()?
        .iter()
        .find(|p| p["name"] == name)
}

async fn expand_inline(app: &TestApp, include: Value) -> Value {
    let body = json!({
        "resourceType": "Parameters",
        "parameter": [{"name": "valueSet", "resource": {
            "resourceType": "ValueSet", "status": "active",
            "compose": {"include": [include]}
        }}]
    });
    let (st, out) = app.post_fhir("/ValueSet/$expand", body.to_string()).await;
    assert_eq!(st, StatusCode::OK, "{out}");
    out
}

fn used_codesystems(body: &Value) -> Vec<String> {
    params_named(body, "used-codesystem")
        .iter()
        .filter_map(|p| p["valueUri"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn versionless_filter_expand_reports_the_version_it_expanded() {
    let app = app().await;
    let out = expand_inline(
        &app,
        json!({"system": CS_URL,
               "filter": [{"property": "SCALE_TYP", "op": "=", "value": "DOC"}]}),
    )
    .await;
    let contains = out["expansion"]["contains"].as_array().expect("contains");
    assert_eq!(contains.len(), 1, "{out}");
    assert_eq!(contains[0]["code"], "1-1", "{out}");
    assert_eq!(contains[0]["display"], "Note v2.83", "{out}");
    assert_eq!(
        used_codesystems(&out),
        vec![format!("{CS_URL}|2.83")],
        "{out}"
    );
    if let Some(v) = contains[0].get("version") {
        assert_eq!(v, "2.83", "{out}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn versionless_whole_system_expand_reports_the_version_it_expanded() {
    let app = app().await;
    let out = expand_inline(&app, json!({"system": CS_URL})).await;
    assert_eq!(out["expansion"]["total"], 2, "{out}");
    assert_eq!(
        used_codesystems(&out),
        vec![format!("{CS_URL}|2.83")],
        "{out}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn versionless_empty_filter_expand_reports_the_default_version() {
    // No contains[] at all: used-codesystem falls back to the CS metadata.
    let app = app().await;
    let out = expand_inline(
        &app,
        json!({"system": CS_URL,
               "filter": [{"property": "SCALE_TYP", "op": "=", "value": "NOPE"}]}),
    )
    .await;
    assert_eq!(
        used_codesystems(&out),
        vec![format!("{CS_URL}|2.83")],
        "{out}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn versionless_lookup_and_validate_code_report_the_default_version() {
    let app = app().await;
    let (st, lookup) = app
        .get_fhir(&format!("/CodeSystem/$lookup?system={CS_URL}&code=1-1"))
        .await;
    assert_eq!(st, StatusCode::OK, "{lookup}");
    assert_eq!(
        out_param(&lookup, "version").unwrap()["valueString"],
        "2.83"
    );
    assert_eq!(
        out_param(&lookup, "display").unwrap()["valueString"],
        "Note v2.83"
    );

    let (st, vc) = app
        .get_fhir(&format!("/CodeSystem/$validate-code?url={CS_URL}&code=1-1"))
        .await;
    assert_eq!(st, StatusCode::OK, "{vc}");
    assert_eq!(
        out_param(&vc, "result").unwrap()["valueBoolean"],
        true,
        "{vc}"
    );
    assert_eq!(
        out_param(&vc, "version").unwrap()["valueString"],
        "2.83",
        "{vc}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn versionless_url_search_lists_the_default_version_first() {
    let app = app().await;
    let (st, all) = app.get_fhir(&format!("/CodeSystem?url={CS_URL}")).await;
    assert_eq!(st, StatusCode::OK, "{all}");
    assert_eq!(all["total"], 3, "{all}");
    assert_eq!(all["entry"][0]["resource"]["version"], "2.83", "{all}");
}
