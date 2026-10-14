//! Compose filters that used to fall through to "unsupported — empty set"
//! (fork issue T4): multi-value `in` / `not-in` and `exists`.
//!
//! Every case is checked through `$expand` (inline compose, both the full and
//! the bounded-candidate path) and through `ValueSet/$validate-code`, which
//! performs an implicit expansion.

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

async fn validate_inline(app: &TestApp, filters: Value, code: &str) -> bool {
    let mut params: Value = serde_json::from_str(&inline_vs(filters)).unwrap();
    params["parameter"].as_array_mut().unwrap().extend([
        json!({"name": "system", "valueUri": CS_URL}),
        json!({"name": "code", "valueCode": code}),
    ]);
    let (st, body) = app
        .post_fhir("/ValueSet/$validate-code", params.to_string())
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    body["parameter"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "result")
        .and_then(|p| p["valueBoolean"].as_bool())
        .unwrap_or_else(|| panic!("no result in {body}"))
}

// ── T4: in / not-in / exists ──────────────────────────────────────────────────

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn in_filter_with_multiple_values_is_a_union() {
    let app = TestApp::new();
    app.import_bundle_ok(&fixture_bundle()).await;

    let codes = expand_inline(
        &app,
        json!([{"property": "side", "op": "in", "value": "left,right"}]),
    )
    .await;
    assert_eq!(codes, ["A1", "A2"]);

    // Single value still reduces to equality.
    let codes = expand_inline(
        &app,
        json!([{"property": "side", "op": "in", "value": "bilateral"}]),
    )
    .await;
    assert_eq!(codes, ["A2.1"]);

    // `property=concept` selects the listed codes.
    let codes = expand_inline(
        &app,
        json!([{"property": "concept", "op": "in", "value": "B, A1,nope"}]),
    )
    .await;
    assert_eq!(codes, ["A1", "B"]);

    assert!(
        validate_inline(
            &app,
            json!([{"property": "side", "op": "in", "value": "left,right"}]),
            "A2"
        )
        .await
    );
    assert!(
        !validate_inline(
            &app,
            json!([{"property": "side", "op": "in", "value": "left,right"}]),
            "A2.1"
        )
        .await
    );
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn not_in_filter_with_multiple_values_excludes_all_listed() {
    let app = TestApp::new();
    app.import_bundle_ok(&fixture_bundle()).await;

    let codes = expand_inline(
        &app,
        json!([{"property": "side", "op": "not-in", "value": "left,right"}]),
    )
    .await;
    // Concepts without the property pass too.
    assert_eq!(codes, ["A2.1", "B", "_Abstract"]);

    // Intersected with a hierarchy filter (bounded-candidate path).
    let codes = expand_inline(
        &app,
        json!([
            {"property": "concept", "op": "is-a", "value": "_Abstract"},
            {"property": "side", "op": "not-in", "value": "left,right"}
        ]),
    )
    .await;
    assert_eq!(codes, ["A2.1", "_Abstract"]);

    assert!(
        !validate_inline(
            &app,
            json!([{"property": "side", "op": "not-in", "value": "left,right"}]),
            "A1"
        )
        .await
    );
    assert!(
        validate_inline(
            &app,
            json!([{"property": "side", "op": "not-in", "value": "left,right"}]),
            "B"
        )
        .await
    );
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn exists_filter_selects_by_property_presence() {
    let app = TestApp::new();
    app.import_bundle_ok(&fixture_bundle()).await;

    let codes = expand_inline(
        &app,
        json!([{"property": "side", "op": "exists", "value": "true"}]),
    )
    .await;
    assert_eq!(codes, ["A1", "A2", "A2.1"]);

    let codes = expand_inline(
        &app,
        json!([{"property": "side", "op": "exists", "value": "false"}]),
    )
    .await;
    assert_eq!(codes, ["B", "_Abstract"]);

    // ANDed with another filter.
    let codes = expand_inline(
        &app,
        json!([
            {"property": "rank", "op": "exists", "value": "true"},
            {"property": "concept", "op": "is-a", "value": "_Abstract"}
        ]),
    )
    .await;
    assert_eq!(codes, ["A1"]);

    assert!(
        validate_inline(
            &app,
            json!([{"property": "rank", "op": "exists", "value": "true"}]),
            "B"
        )
        .await
    );
    assert!(
        !validate_inline(
            &app,
            json!([{"property": "rank", "op": "exists", "value": "false"}]),
            "B"
        )
        .await
    );

    // Malformed value → 4xx OperationOutcome rather than a silent empty set.
    let (st, body) = app
        .post_fhir(
            "/ValueSet/$expand",
            inline_vs(json!([{"property": "side", "op": "exists", "value": "maybe"}])),
        )
        .await;
    assert!(st.is_client_error(), "{st} {body}");
}
