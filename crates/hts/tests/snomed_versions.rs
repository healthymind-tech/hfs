//! SNOMED CT version URIs (`http://snomed.info/sct/<module>[/version/<date>]`)
//! name the stored release in `$validate-code` and `$lookup`; an edition that
//! is not loaded stays an unknown version, as on tx.fhir.org.
//!
//! Runs on SQLite, or on PostgreSQL with `HTS_TEST_BACKEND=postgres`.

mod common;

use common::TestApp;
use serde_json::{Value, json};

const SCT: &str = "http://snomed.info/sct";
const INT: &str = "http://snomed.info/sct/900000000000207008";
const US: &str = "http://snomed.info/sct/731000124108";

async fn app() -> TestApp {
    let app = TestApp::new().await;
    let bundle = json!({"resourceType": "Bundle", "type": "collection", "entry": [
        {"resource": {"resourceType": "CodeSystem", "id": "sct", "url": SCT, "version": "20250601",
            "status": "active", "content": "complete",
            "concept": [{"code": "763875007", "display": "Sulfonamide-containing product"}]}},
        {"resource": {"resourceType": "ValueSet", "id": "sulfa", "url": "http://example.org/vs/sulfa",
            "status": "active",
            "compose": {"include": [{"system": SCT, "concept": [{"code": "763875007"}]}]}}}
    ]});
    app.import_bundle_ok(&bundle.to_string()).await;
    app
}

fn param<'a>(out: &'a Value, name: &str) -> Option<&'a Value> {
    out["parameter"].as_array()?.iter().find(|p| p["name"] == name)
}

fn result(out: &Value) -> Option<bool> {
    param(out, "result").and_then(|p| p["valueBoolean"].as_bool())
}

fn enc(s: &str) -> String {
    s.replace(':', "%3A").replace('/', "%2F")
}

#[tokio::test]
async fn code_system_validate_code_accepts_loaded_edition_uris() {
    let app = app().await;
    for version in [INT.to_string(), format!("{INT}/version/20250601"), "20250601".into()] {
        let (_, out) = app
            .get_fhir(&format!(
                "/CodeSystem/$validate-code?url={}&code=763875007&version={}",
                enc(SCT),
                enc(&version)
            ))
            .await;
        assert_eq!(result(&out), Some(true), "{version}: {out}");
    }
    for version in [US.to_string(), format!("{INT}/version/20190731")] {
        let (_, out) = app
            .get_fhir(&format!(
                "/CodeSystem/$validate-code?url={}&code=763875007&version={}",
                enc(SCT),
                enc(&version)
            ))
            .await;
        assert_eq!(result(&out), Some(false), "{version}: {out}");
        let message = param(&out, "message")
            .and_then(|p| p["valueString"].as_str())
            .unwrap_or_default();
        assert!(message.contains("could not be found"), "{version}: {out}");
    }
}

#[tokio::test]
async fn lookup_accepts_the_international_edition_uri() {
    let app = app().await;
    let (status, out) = app
        .get_fhir(&format!(
            "/CodeSystem/$lookup?system={}&code=763875007&version={}",
            enc(SCT),
            enc(INT)
        ))
        .await;
    assert!(status.is_success(), "{out}");
    assert_eq!(
        param(&out, "display").and_then(|p| p["valueString"].as_str()),
        Some("Sulfonamide-containing product"),
        "{out}"
    );
}

#[tokio::test]
async fn value_set_validate_code_maps_coding_versions() {
    let app = app().await;
    let call = |version: &str| {
        json!({"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": "http://example.org/vs/sulfa"},
            {"name": "coding", "valueCoding": {"system": SCT, "version": version, "code": "763875007"}}
        ]})
        .to_string()
    };
    let (_, out) = app
        .post_fhir("/ValueSet/$validate-code", call(&format!("{INT}/version/20250601")))
        .await;
    assert_eq!(result(&out), Some(true), "{out}");
    let (_, out) = app.post_fhir("/ValueSet/$validate-code", call(INT)).await;
    assert_eq!(result(&out), Some(true), "{out}");
    let (_, out) = app.post_fhir("/ValueSet/$validate-code", call(US)).await;
    assert_eq!(result(&out), Some(false), "US edition is not loaded: {out}");
}
