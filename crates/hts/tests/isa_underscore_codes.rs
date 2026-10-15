//! `is-a` / `descendent-of` compose filters whose value is not a valid ECL
//! token — THO's `_ActEncounterCode`-style abstract codes — must work like any
//! other code (fork issue T3). Checked through stored and inline `$expand`,
//! the bounded-candidate path, `ValueSet/$validate-code` (implicit expansion)
//! and the ECL `constraint` filter, whose tokenizer now accepts `_`-prefixed
//! and dotted codes.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};

const CS_URL: &str = "http://hts.test/filters/cs";

fn fixture_bundle() -> String {
    json!({
      "resourceType": "Bundle", "type": "collection",
      "entry": [
        {"resource": {
          "resourceType": "CodeSystem", "id": "filters-cs", "url": CS_URL, "version": "1.0",
          "name": "FiltersCS", "status": "active", "content": "complete", "hierarchyMeaning": "is-a",
          "property": [
            {"code": "side", "type": "string"},
            {"code": "rank", "type": "integer"},
            {"code": "notSelectable", "uri": "http://hl7.org/fhir/concept-properties#notSelectable", "type": "boolean"}
          ],
          "concept": [
            {"code": "_Abstract", "display": "Abstract root",
             "property": [{"code": "notSelectable", "valueBoolean": true}],
             "concept": [
               {"code": "A1", "display": "A one",
                "property": [{"code": "side", "valueString": "left"}, {"code": "rank", "valueInteger": 1}]},
               {"code": "A2", "display": "A two",
                "property": [{"code": "side", "valueString": "right"}],
                "concept": [
                  {"code": "A2.1", "display": "A two point one",
                   "property": [{"code": "side", "valueString": "bilateral"}]}
                ]}
             ]},
            {"code": "B", "display": "B", "property": [{"code": "rank", "valueInteger": 2}]}
          ]
        }},
        {"resource": {
          "resourceType": "ValueSet", "id": "filters-isa", "url": "http://hts.test/filters/vs-isa",
          "version": "1.0", "name": "IsaUnderscore", "status": "active",
          "compose": {"include": [{"system": CS_URL,
            "filter": [{"property": "concept", "op": "is-a", "value": "_Abstract"}]}]}
        }},
        {"resource": {
          "resourceType": "ValueSet", "id": "filters-ecl", "url": "http://hts.test/filters/vs-ecl",
          "version": "1.0", "name": "EclUnderscore", "status": "active",
          "compose": {"include": [{"system": CS_URL,
            "filter": [{"property": "constraint", "op": "=", "value": "< _Abstract"}]}]}
        }}
      ]
    })
    .to_string()
}

fn inline_vs(filters: Value) -> String {
    json!({
      "resourceType": "Parameters",
      "parameter": [{"name": "valueSet", "resource": {
        "resourceType": "ValueSet", "status": "active",
        "compose": {"include": [{"system": CS_URL, "filter": filters}]}
      }}]
    })
    .to_string()
}

fn sorted_codes(body: &Value) -> Vec<String> {
    fn walk(items: &[Value], out: &mut Vec<String>) {
        for c in items {
            if let Some(code) = c["code"].as_str() {
                out.push(code.to_owned());
            }
            if let Some(nested) = c["contains"].as_array() {
                walk(nested, out);
            }
        }
    }
    let mut v = Vec::new();
    if let Some(items) = body["expansion"]["contains"].as_array() {
        walk(items, &mut v);
    }
    v.sort();
    v
}

async fn expand_inline(app: &TestApp, filters: Value) -> Vec<String> {
    let (st, body) = app
        .post_fhir("/ValueSet/$expand?excludeNested=true", inline_vs(filters))
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    sorted_codes(&body)
}

// ── T3: underscore-prefixed codes ─────────────────────────────────────────────

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn is_a_underscore_code_expands_subtree() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&fixture_bundle()).await;

    let (st, body) = app
        .get_fhir("/ValueSet/$expand?url=http://hts.test/filters/vs-isa&excludeNested=true")
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(sorted_codes(&body), ["A1", "A2", "A2.1", "_Abstract"]);

    let codes = expand_inline(
        &app,
        json!([{"property": "concept", "op": "descendent-of", "value": "_Abstract"}]),
    )
    .await;
    assert_eq!(codes, ["A1", "A2", "A2.1"]);

    // Combined with a property filter (hits the bounded-candidate path).
    let codes = expand_inline(
        &app,
        json!([
            {"property": "concept", "op": "is-a", "value": "_Abstract"},
            {"property": "side", "op": "=", "value": "left"}
        ]),
    )
    .await;
    assert_eq!(codes, ["A1"]);
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn is_a_underscore_code_validates_membership() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&fixture_bundle()).await;

    for (code, expected) in [("A2.1", true), ("_Abstract", true), ("B", false)] {
        let (st, body) = app
            .get_fhir(&format!(
                "/ValueSet/$validate-code?url=http://hts.test/filters/vs-isa&system={CS_URL}&code={code}"
            ))
            .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let result = body["parameter"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "result")
            .and_then(|p| p["valueBoolean"].as_bool());
        assert_eq!(result, Some(expected), "{code}: {body}");
    }
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn ecl_constraint_accepts_underscore_and_dotted_codes() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&fixture_bundle()).await;

    let (st, body) = app
        .get_fhir("/ValueSet/$expand?url=http://hts.test/filters/vs-ecl&excludeNested=true")
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(sorted_codes(&body), ["A1", "A2", "A2.1"]);

    let codes = expand_inline(
        &app,
        json!([{"property": "constraint", "op": "=", "value": ">> A2.1"}]),
    )
    .await;
    assert_eq!(codes, ["A2", "A2.1", "_Abstract"]);
}
