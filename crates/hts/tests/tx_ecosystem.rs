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

/// A versioned CodeSystem and a ValueSet including all of it.
fn simple_bundle() -> String {
    json!({
      "resourceType": "Bundle", "type": "collection",
      "entry": [
        {"resource": {
          "resourceType": "CodeSystem", "id": "simple", "url": "http://hts.test/simple",
          "version": "0.1.0", "status": "active", "content": "complete",
          "concept": [{"code": "code1", "display": "Display 1"}, {"code": "code2", "display": "Display 2"}]
        }},
        {"resource": {
          "resourceType": "ValueSet", "id": "simple-all", "url": "http://hts.test/vs/simple-all",
          "version": "5.0.0", "status": "active",
          "compose": {"include": [{"system": "http://hts.test/simple"}]}
        }}
      ]
    })
    .to_string()
}

fn vs_params(extra: Value) -> Value {
    let mut p = vec![json!({"name": "url", "valueUri": "http://hts.test/vs/simple-all"})];
    p.extend(extra.as_array().unwrap().iter().cloned());
    json!({"resourceType": "Parameters", "parameter": p})
}

/// validation/validation-missing-*: nothing to validate is a 400
/// invalid / invalid-data; an incomplete Coding is result=false with a
/// warning against that Coding.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn incomplete_validate_code_input() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&simple_bundle()).await;

    // Request errors.
    for (path, body) in [
        ("/ValueSet/$validate-code", vs_params(json!([]))),
        // A bare code with no system and no inferSystem.
        (
            "/ValueSet/$validate-code",
            vs_params(json!([{"name": "code", "valueCode": "code1"}])),
        ),
        (
            "/CodeSystem/$validate-code",
            json!({"resourceType": "Parameters", "parameter": [{"name": "code", "valueCode": "code1"}]}),
        ),
    ] {
        let (st, oo) = app.post_fhir(path, body.to_string()).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{oo}");
        assert_eq!(oo["issue"][0]["code"], "invalid");
        assert_eq!(
            oo["issue"][0]["details"]["coding"][0]["code"],
            "invalid-data"
        );
        assert!(oo["issue"][0].get("diagnostics").is_none(), "{oo}");
    }

    // inferSystem lets a bare code through.
    let body = vs_validate(
        &app,
        vs_params(json!([
            {"name": "code", "valueCode": "code1"},
            {"name": "inferSystem", "valueBoolean": true}
        ])),
    )
    .await;
    assert_eq!(param(&body, "result").unwrap()["valueBoolean"], true);

    // A Coding with a system but no code.
    let body = vs_validate(
        &app,
        vs_params(json!([{"name": "coding", "valueCoding": {"system": "http://hts.test/simple"}}])),
    )
    .await;
    assert_eq!(
        param(&body, "result").unwrap()["valueBoolean"],
        false,
        "{body}"
    );
    assert_eq!(tx_codes(&body), ["invalid-data"]);
    assert_eq!(issues(&body)[0]["severity"], "warning");
    assert_eq!(issues(&body)[0]["expression"], json!(["Coding"]));
    assert!(param(&body, "message").is_some());

    // A CodeableConcept with text only: fails as a whole.
    let body = vs_validate(
        &app,
        vs_params(json!([{"name": "codeableConcept", "valueCodeableConcept": {"text": "x"}}])),
    )
    .await;
    assert_eq!(param(&body, "result").unwrap()["valueBoolean"], false);
    assert_eq!(tx_codes(&body), ["not-in-vs"]);
    let msg = param(&body, "message").unwrap()["valueString"]
        .as_str()
        .unwrap();
    assert!(msg.contains("http://hts.test/vs/simple-all|5.0.0"), "{msg}");

    // A systemless coding next to a good one: the good one carries the
    // result, the systemless one is reported against its own index.
    let body = vs_validate(
        &app,
        vs_params(
            json!([{"name": "codeableConcept", "valueCodeableConcept": {"coding": [
                {"code": "code1"},
                {"system": "http://hts.test/simple", "code": "code1"}
            ]}}]),
        ),
    )
    .await;
    assert_eq!(
        param(&body, "result").unwrap()["valueBoolean"],
        true,
        "{body}"
    );
    assert_eq!(tx_codes(&body), ["invalid-data", "this-code-not-in-vs"]);
    let warning = issues(&body)
        .into_iter()
        .find(|i| i["severity"] == "warning")
        .unwrap();
    assert_eq!(warning["expression"], json!(["CodeableConcept.coding[0]"]));

    // CodeSystem/$validate-code: a CodeableConcept whose coding has no code.
    let (st, body) = app
        .post_fhir(
            "/CodeSystem/$validate-code",
            json!({"resourceType": "Parameters", "parameter": [
                {"name": "codeableConcept", "valueCodeableConcept": {"coding": [{"system": "http://hts.test/simple"}]}}
            ]})
            .to_string(),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(param(&body, "result").unwrap()["valueBoolean"], false);
    assert_eq!(tx_codes(&body), ["invalid-data"]);
}

/// deprecated/draft-validate: a draft CodeSystem used by an active ValueSet
/// is noted as such; a normative one is not noted at all.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn status_check_notes() {
    let app = TestApp::new().await;
    app.import_bundle_ok(
        &json!({"resourceType": "Bundle", "type": "collection", "entry": [
            {"resource": {"resourceType": "CodeSystem", "id": "draft", "url": "http://hts.test/draft",
              "version": "0.1.0", "status": "draft", "content": "complete",
              "concept": [{"code": "a", "display": "A"}]}},
            {"resource": {"resourceType": "CodeSystem", "id": "norm", "url": "http://hts.test/norm",
              "version": "1.0", "status": "active", "content": "complete",
              "extension": [{"url": "http://hl7.org/fhir/StructureDefinition/structuredefinition-standards-status",
                             "valueCode": "normative"}],
              "concept": [{"code": "b", "display": "B"}]}},
            {"resource": {"resourceType": "ValueSet", "id": "both", "url": "http://hts.test/vs/both",
              "version": "1.0", "status": "active",
              "compose": {"include": [{"system": "http://hts.test/draft"}, {"system": "http://hts.test/norm"}]}}}
        ]})
        .to_string(),
    )
    .await;

    let coding = |system: &str, code: &str| {
        json!({"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": "http://hts.test/vs/both"},
            {"name": "coding", "valueCoding": {"system": system, "code": code}}
        ]})
    };
    let body = vs_validate(&app, coding("http://hts.test/draft", "a")).await;
    let notes = issues(&body);
    assert_eq!(notes.len(), 1, "{body}");
    assert_eq!(
        notes[0]["details"]["text"],
        "Reference to draft CodeSystem http://hts.test/draft|0.1.0 from active ValueSet http://hts.test/vs/both|1.0"
    );

    let body = vs_validate(&app, coding("http://hts.test/norm", "b")).await;
    assert!(issues(&body).is_empty(), "{body}");
}
