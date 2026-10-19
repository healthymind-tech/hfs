//! Regressions found by running the HL7 tx-ecosystem test suite
//! (HL7/fhir-tx-ecosystem-ig `tests/`, `validator_cli.jar txTests`) against
//! HTS. Each test names the suite/test it mirrors. Runs on SQLite by default
//! and on PostgreSQL with `HTS_TEST_BACKEND=postgres`.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};

const TX_ISSUE_TYPE: &str = "http://hl7.org/fhir/tools/CodeSystem/tx-issue-type";

/// A versionless, flat CodeSystem whose hierarchy is stated through
/// `subsumedBy` properties (the shape of THO act-class), plus filtered
/// ValueSets over it.
fn act_bundle() -> String {
    json!({
      "resourceType": "Bundle", "type": "collection",
      "entry": [
        {"resource": {
          "resourceType": "CodeSystem", "id": "act", "url": "http://hts.test/act",
          "status": "active", "content": "complete", "hierarchyMeaning": "is-a",
          "property": [
            {"code": "subsumedBy", "uri": "http://hl7.org/fhir/concept-properties#parent", "type": "code"},
            {"code": "status", "uri": "http://hl7.org/fhir/concept-properties#status", "type": "code"}
          ],
          "concept": [
            {"code": "ACT", "display": "act"},
            {"code": "ORG", "display": "organizer",
             "property": [{"code": "subsumedBy", "valueCode": "ACT"}]},
            {"code": "FOLDER", "display": "folder",
             "property": [{"code": "subsumedBy", "valueCode": "ORG"}]},
            {"code": "OBS", "display": "observation",
             "property": [{"code": "subsumedBy", "valueCode": "ACT"}]},
            {"code": "OLD", "display": "old",
             "property": [{"code": "subsumedBy", "valueCode": "ACT"},
                          {"code": "status", "valueCode": "retired"}]}
          ]
        }},
        {"resource": {
          "resourceType": "ValueSet", "id": "act-isa", "url": "http://hts.test/vs/act-isa",
          "version": "0.1.0", "status": "active",
          "compose": {"include": [{"system": "http://hts.test/act",
            "filter": [{"property": "concept", "op": "is-a", "value": "ORG"}]}]}
        }}
      ]
    })
    .to_string()
}

fn param<'a>(body: &'a Value, name: &str) -> Option<&'a Value> {
    body["parameter"]
        .as_array()?
        .iter()
        .find(|p| p["name"] == name)
}

fn issues(body: &Value) -> Vec<Value> {
    param(body, "issues")
        .and_then(|p| p["resource"]["issue"].as_array().cloned())
        .unwrap_or_default()
}

fn tx_codes(body: &Value) -> Vec<String> {
    let mut v: Vec<String> = issues(body)
        .iter()
        .filter_map(|i| {
            i["details"]["coding"]
                .as_array()?
                .iter()
                .find(|c| c["system"] == TX_ISSUE_TYPE)
                .and_then(|c| c["code"].as_str().map(str::to_owned))
        })
        .collect();
    v.sort();
    v
}

async fn vs_validate(app: &TestApp, params: Value) -> Value {
    let (st, body) = app
        .post_fhir("/ValueSet/$validate-code", params.to_string())
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    body
}

/// tho/validate-isa-parent, tho/validate-isa-unknown-code: a code outside a
/// filtered ValueSet over a versionless CodeSystem still echoes the
/// CodeSystem display, and a code missing from the CodeSystem gets the
/// invalid-code issue next to not-in-vs.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn not_in_vs_against_versionless_code_system() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&act_bundle()).await;

    let body = vs_validate(
        &app,
        json!({"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": "http://hts.test/vs/act-isa"},
            {"name": "system", "valueUri": "http://hts.test/act"},
            {"name": "code", "valueCode": "ACT"}
        ]}),
    )
    .await;
    assert_eq!(param(&body, "result").unwrap()["valueBoolean"], false);
    assert_eq!(param(&body, "display").unwrap()["valueString"], "act");
    assert_eq!(tx_codes(&body), ["not-in-vs"]);

    let body = vs_validate(
        &app,
        json!({"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": "http://hts.test/vs/act-isa"},
            {"name": "system", "valueUri": "http://hts.test/act"},
            {"name": "code", "valueCode": "NOPE"}
        ]}),
    )
    .await;
    assert_eq!(param(&body, "result").unwrap()["valueBoolean"], false);
    assert!(param(&body, "display").is_none(), "{body}");
    assert_eq!(tx_codes(&body), ["invalid-code", "not-in-vs"]);
}

/// tho/validate-isa-other-branch: an inactive code that the ValueSet leaves
/// out for hierarchy reasons (not because it excludes inactive codes) gets
/// not-in-vs plus the inactive warning, but not the "valid but not active"
/// error; the warning is about the `code` parameter.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn inactive_code_outside_filtered_vs_is_not_a_status_error() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&act_bundle()).await;

    let body = vs_validate(
        &app,
        json!({"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": "http://hts.test/vs/act-isa"},
            {"name": "system", "valueUri": "http://hts.test/act"},
            {"name": "code", "valueCode": "OLD"}
        ]}),
    )
    .await;
    assert_eq!(param(&body, "result").unwrap()["valueBoolean"], false);
    assert_eq!(param(&body, "inactive").unwrap()["valueBoolean"], true);
    assert_eq!(tx_codes(&body), ["code-comment", "not-in-vs"], "{body}");
    let warning = issues(&body)
        .into_iter()
        .find(|i| i["severity"] == "warning")
        .unwrap();
    assert_eq!(warning["expression"], json!(["code"]));
}
