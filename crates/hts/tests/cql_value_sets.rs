//! What a CQL engine (cqf `R4FhirTerminologyProvider`, the HAPI/cqf-ruler
//! stack, cql-workbench) needs from HTS for eCQM ValueSets:
//!
//! * VSAC / eCQM content publishes ValueSets with only an `expansion` (no
//!   `compose`); their members are that expansion — including codes of a
//!   CodeSystem edition the server did not load (SNOMED US, older LOINC) and
//!   of CodeSystems it does not have at all (CPT).
//! * `GET ValueSet?url=` resolves the id, then `GET ValueSet/{id}/$expand`
//!   and `GET ValueSet/{id}/$validate-code?code=&system=`; the provider reads
//!   the membership answer as `parameter[0]`, so `result` must come first.
//!
//! Runs on SQLite by default and on PostgreSQL with `HTS_TEST_BACKEND=postgres`.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};

const VS_URL: &str =
    "http://cts.nlm.nih.gov/fhir/ValueSet/2.16.840.1.113883.3.464.1003.103.12.1001";
const VS_ID: &str = "2.16.840.1.113883.3.464.1003.103.12.1001";
const LOCAL_CS: &str = "http://hts.test/cql/icd";
const CPT: &str = "http://www.ama-assn.org/go/cpt";

fn bundle() -> String {
    json!({
      "resourceType": "Bundle", "type": "collection",
      "entry": [
        {"resource": {
          "resourceType": "CodeSystem", "id": "cql-icd", "url": LOCAL_CS, "version": "2025",
          "status": "active", "content": "complete",
          "concept": [{"code": "E11.9", "display": "Type 2 diabetes mellitus without complications"},
                      {"code": "I10", "display": "Essential hypertension"}]
        }},
        {"resource": {
          "resourceType": "ValueSet", "id": VS_ID, "url": VS_URL, "version": "20190315",
          "name": "Diabetes", "status": "active",
          "expansion": {
            "timestamp": "2019-03-15T00:00:00Z",
            "contains": [
              {"system": LOCAL_CS, "version": "2019", "code": "E11.9",
               "display": "Type 2 diabetes mellitus without complications"},
              // a code of the 2019 edition this server's 2025 CodeSystem no longer has
              {"system": LOCAL_CS, "version": "2019", "code": "E11.OLD", "display": "Retired code"},
              // a CodeSystem the server does not have
              {"system": CPT, "version": "2019", "code": "99202", "display": "Office visit"}
            ]
          }
        }}
      ]
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

fn codes(body: &Value) -> Vec<String> {
    let mut v: Vec<String> = body["expansion"]["contains"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|c| {
            format!(
                "{}|{}",
                c["system"].as_str().unwrap_or(""),
                c["code"].as_str().unwrap_or("")
            )
        })
        .collect();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread")]
async fn expansion_only_value_set_expands_to_its_stored_members() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&bundle()).await;

    let mut want = vec![
        format!("{CPT}|99202"),
        format!("{LOCAL_CS}|E11.9"),
        format!("{LOCAL_CS}|E11.OLD"),
    ];
    want.sort();
    for path in [
        format!("/ValueSet/$expand?url={VS_URL}"),
        format!("/ValueSet/{VS_ID}/$expand"),
        // what HFS sends for `code:in` (flat, paged)
        format!("/ValueSet/$expand?url={VS_URL}&excludeNested=true&count=1000&offset=0"),
    ] {
        let (st, body) = app.get_fhir(&path).await;
        assert_eq!(st, StatusCode::OK, "{path}: {body}");
        assert_eq!(codes(&body), want, "{path}: {body}");
        assert_eq!(body["expansion"]["total"], 3, "{path}: {body}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn expansion_only_value_set_membership() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&bundle()).await;

    for (system, code, member) in [
        (LOCAL_CS, "E11.9", true),
        (CPT, "99202", true),
        // listed in the stored expansion, absent from the loaded edition
        (LOCAL_CS, "E11.OLD", true),
        (LOCAL_CS, "I10", false),
        (CPT, "99999", false),
    ] {
        for path in [
            format!("/ValueSet/{VS_ID}/$validate-code?code={code}&system={system}"),
            format!("/ValueSet/$validate-code?url={VS_URL}&code={code}&system={system}"),
        ] {
            let (st, body) = app.get_fhir(&path).await;
            assert_eq!(st, StatusCode::OK, "{path}: {body}");
            assert_eq!(result(&body), Some(member), "{path}: {body}");
        }
    }
}

/// cqf `R4FhirTerminologyProvider.in()` does
/// `((BooleanType) respParam.getParameter().get(0).getValue()).booleanValue()`.
#[tokio::test(flavor = "multi_thread")]
async fn validate_code_puts_result_first() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&bundle()).await;
    for path in [
        format!("/ValueSet/{VS_ID}/$validate-code?code=E11.9&system={LOCAL_CS}"),
        format!("/ValueSet/{VS_ID}/$validate-code?code=I10&system={LOCAL_CS}"),
        format!("/ValueSet/$validate-code?url={VS_URL}&code=E11.9&system={LOCAL_CS}"),
        format!("/CodeSystem/$validate-code?url={LOCAL_CS}&code=E11.9"),
        format!("/CodeSystem/$validate-code?url={LOCAL_CS}&code=NOPE"),
    ] {
        let (st, body) = app.get_fhir(&path).await;
        assert_eq!(st, StatusCode::OK, "{path}: {body}");
        assert_eq!(body["parameter"][0]["name"], "result", "{path}: {body}");
        assert!(
            body["parameter"][0]["valueBoolean"].is_boolean(),
            "{path}: {body}"
        );
    }
    let (st, body) = app
        .post_fhir(
            &format!("/ValueSet/{VS_ID}/$validate-code"),
            json!({"resourceType": "Parameters", "parameter": [
                {"name": "coding", "valueCoding": {"system": LOCAL_CS, "code": "E11.9"}}]})
            .to_string(),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["parameter"][0]["name"], "result", "{body}");
}

/// `resolveValueSetId` fails on anything but exactly one match.
#[tokio::test(flavor = "multi_thread")]
async fn value_set_search_by_url_finds_one() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&bundle()).await;
    let (st, body) = app.get_fhir(&format!("/ValueSet?url={VS_URL}")).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let entries = body["entry"].as_array().cloned().unwrap_or_default();
    assert_eq!(entries.len(), 1, "{body}");
    assert_eq!(entries[0]["resource"]["id"], VS_ID, "{body}");
}
