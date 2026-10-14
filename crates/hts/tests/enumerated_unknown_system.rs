//! An enumerated ValueSet (`compose.include.concept[]`) must validate its own
//! codes even when the referenced CodeSystem is not loaded (fork issue T8).
//! The result is `true` with a warning about the unknown system rather than a
//! hard `false`.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};

const LOINC: &str = "http://loinc.org";
const VS_URL: &str = "http://hts.test/t8/vital-signs";

fn bundle() -> String {
    json!({
      "resourceType": "Bundle", "type": "collection",
      "entry": [{"resource": {
        "resourceType": "ValueSet", "id": "t8-vs", "url": VS_URL, "version": "1.0", "status": "active",
        "compose": {"include": [{"system": LOINC, "concept": [
          {"code": "8302-2", "display": "Body height"},
          {"code": "29463-7", "display": "Body weight"}
        ]}]}
      }}]
    })
    .to_string()
}

fn result(body: &Value) -> Option<bool> {
    body["parameter"]
        .as_array()?
        .iter()
        .find(|p| p["name"] == "result")
        .and_then(|p| p["valueBoolean"].as_bool())
}

fn issue_severities(body: &Value) -> Vec<String> {
    body["parameter"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["name"] == "issues")
        .flat_map(|p| {
            p["resource"]["issue"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|i| i["severity"].as_str().map(str::to_owned))
        })
        .collect()
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn enumerated_code_validates_true_with_warning_when_system_missing() {
    let app = TestApp::new();
    app.import_bundle_ok(&bundle()).await;

    // Expansion lists the enumerated codes.
    let (st, body) = app
        .get_fhir(&format!("/ValueSet/$expand?url={VS_URL}"))
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        body["expansion"]["contains"].as_array().map(Vec::len),
        Some(2),
        "{body}"
    );

    for path in [
        format!("/ValueSet/$validate-code?url={VS_URL}&system={LOINC}&code=8302-2"),
        format!(
            "/ValueSet/$validate-code?url={VS_URL}&system={LOINC}&code=8302-2&display=Body%20height"
        ),
    ] {
        let (st, body) = app.get_fhir(&path).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(result(&body), Some(true), "{path}: {body}");
        let sev = issue_severities(&body);
        assert!(
            !sev.iter().any(|s| s == "error"),
            "no error issues expected: {body}"
        );
        assert!(
            sev.iter().any(|s| s == "warning"),
            "a warning about the unknown system is expected: {body}"
        );
    }

    // POST coding / codeableConcept forms agree.
    let (st, body) = app
        .post_fhir(
            "/ValueSet/$validate-code",
            json!({"resourceType": "Parameters", "parameter": [
                {"name": "url", "valueUri": VS_URL},
                {"name": "coding", "valueCoding": {"system": LOINC, "code": "29463-7"}}]})
            .to_string(),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(result(&body), Some(true), "{body}");

    let (st, body) = app
        .post_fhir(
            "/ValueSet/$validate-code",
            json!({"resourceType": "Parameters", "parameter": [
                {"name": "url", "valueUri": VS_URL},
                {"name": "codeableConcept", "valueCodeableConcept": {"coding": [{"system": LOINC, "code": "29463-7"}]}}]})
            .to_string(),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(result(&body), Some(true), "{body}");

    // A code that is NOT enumerated stays false.
    let (st, body) = app
        .get_fhir(&format!(
            "/ValueSet/$validate-code?url={VS_URL}&system={LOINC}&code=2339-0"
        ))
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(result(&body), Some(false), "{body}");
}
