//! Display checks and HL7 code-system resolution against a terminology
//! server (`HFS_VALIDATION_DISPLAY_ISSUES`), through the batched
//! terminology path: one round trip per resource, value-set and
//! code-system checks in the same batch.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::response::Json;
use axum_test::TestServer;
use helios_persistence::backends::sqlite::SqliteBackend;
use helios_rest::config::ValidationConfig;
use helios_rest::{ServerConfig, create_app_with_config};
use serde_json::{Value, json};
use tokio::net::TcpListener;

fn param<'a>(body: &'a Value, name: &str) -> Option<&'a Value> {
    body["parameter"]
        .as_array()?
        .iter()
        .find(|p| p["name"] == name)
}

fn display_issue(text: &str) -> Value {
    json!({"name": "issues", "resource": {"resourceType": "OperationOutcome", "issue": [{
        "severity": "error", "code": "invalid",
        "details": {"coding": [{"system": "http://hl7.org/fhir/tools/CodeSystem/tx-issue-type",
            "code": "invalid-display"}], "text": text}}]}})
}

/// One `$validate-code` answer, HTS-style: LOINC 8867-4 is "Heart rate"
/// (any other display is wrong, which also makes `result` false), the
/// value set holds 8867-4, `http://terminology.hl7.org/CodeSystem/nope` is
/// unknown.
fn answer(op: &str, body: &Value) -> Value {
    let coding = param(body, "coding").map(|p| p["valueCoding"].clone());
    let concept = param(body, "codeableConcept").map(|p| p["valueCodeableConcept"]["coding"][0].clone());
    let (system, code, display) = match coding.or(concept) {
        Some(c) => (
            c["system"].as_str().unwrap_or_default().to_string(),
            c["code"].as_str().unwrap_or_default().to_string(),
            c["display"].as_str().map(str::to_string),
        ),
        None => (
            param(body, "url").and_then(|p| p["valueUri"].as_str()).unwrap_or_default().to_string(),
            param(body, "code").and_then(|p| p["valueCode"].as_str()).unwrap_or_default().to_string(),
            param(body, "display").and_then(|p| p["valueString"].as_str()).map(str::to_string),
        ),
    };
    let mut parameter = Vec::new();
    let mut result = true;
    if op.starts_with("CodeSystem") && system == "http://terminology.hl7.org/CodeSystem/nope" {
        result = false;
        parameter.push(json!({"name": "x-unknown-system", "valueCanonical": system}));
    }
    if op.starts_with("ValueSet") && code != "8867-4" {
        result = false;
    }
    if system == "http://loinc.org" && display.as_deref().is_some_and(|d| d != "Heart rate") {
        result = false;
        parameter.push(display_issue(&format!(
            "Wrong Display Name '{}' for http://loinc.org#{code}. Valid display is 'Heart rate' (for the language(s) 'en')",
            display.unwrap()
        )));
    }
    parameter.push(json!({"name": "result", "valueBoolean": result}));
    json!({"resourceType": "Parameters", "parameter": parameter})
}

/// A batch-only mock HTS that logs every batch.
async fn start_mock() -> (String, Arc<Mutex<Vec<Value>>>) {
    let batches = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&batches);
    let batch = move |Json(bundle): Json<Value>| {
        let log = Arc::clone(&log);
        async move {
            log.lock().unwrap().push(bundle.clone());
            let entries: Vec<Value> = bundle["entry"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|e| {
                    json!({"resource": answer(e["request"]["url"].as_str().unwrap_or_default(), &e["resource"]),
                           "response": {"status": "200"}})
                })
                .collect();
            Json(json!({"resourceType": "Bundle", "type": "batch-response", "entry": entries}))
        }
    };
    let router = Router::new().route("/", axum::routing::post(batch));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (base_url, batches)
}

const VS: &str = "http://example.org/ValueSet/heart";
const PROFILE: &str = "http://example.org/StructureDefinition/heart-obs";

fn hfs(ts_url: &str, display_issues: &str) -> TestServer {
    let backend = SqliteBackend::in_memory().expect("sqlite");
    backend.init_schema().expect("schema");
    let config = ServerConfig {
        terminology_server: Some(ts_url.to_string()),
        validation: ValidationConfig {
            terminology: "remote".to_string(),
            display_issues: display_issues.to_string(),
            ..Default::default()
        },
        ..ServerConfig::for_testing()
    };
    TestServer::new(create_app_with_config(backend, config)).expect("server")
}

async fn upload_profile(server: &TestServer) {
    let response = server
        .put("/StructureDefinition/heart-obs")
        .json(&json!({
            "resourceType": "StructureDefinition", "id": "heart-obs", "url": PROFILE,
            "name": "HeartObs", "status": "active", "kind": "resource", "abstract": false,
            "type": "Observation",
            "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Observation",
            "derivation": "constraint",
            "differential": {"element": [
                {"id": "Observation", "path": "Observation"},
                {"id": "Observation.code", "path": "Observation.code",
                 "binding": {"strength": "required", "valueSet": VS}}
            ]}
        }))
        .await;
    assert!(response.status_code().is_success(), "{}", response.text());
}

fn observation(display: &str) -> Value {
    json!({
        "resourceType": "Observation", "status": "final",
        "code": {"coding": [{"system": "http://loinc.org", "code": "8867-4", "display": display}]},
        "interpretation": [{"coding": [{"system": "http://terminology.hl7.org/CodeSystem/nope", "code": "N"}]}],
        "subject": {"reference": "Patient/p"},
        "performer": [{"reference": "Practitioner/x"}],
        "effectiveDateTime": "2024-01-01",
        "valueQuantity": {"value": 60, "unit": "/min", "system": "http://unitsofmeasure.org", "code": "/min"}
    })
}

/// (severity, expression, text) of every issue.
fn issues(outcome: &Value) -> Vec<(String, String, String)> {
    outcome["issue"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["severity"] != "information")
        .map(|i| {
            (
                i["severity"].as_str().unwrap().to_string(),
                i["expression"][0].as_str().unwrap_or_default().to_string(),
                i["details"]["text"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn wrong_display_follows_the_setting_and_never_fails_the_binding() {
    let (ts_url, batches) = start_mock().await;
    for (setting, severity) in [("warning", "warning"), ("error", "error")] {
        batches.lock().unwrap().clear();
        let server = hfs(&ts_url, setting);
        upload_profile(&server).await;
        let response = server
            .post(&format!("/Observation/$validate?profile={PROFILE}"))
            .json(&observation("Pulse"))
            .await;
        response.assert_status_ok();
        let outcome: Value = response.json();
        let found = issues(&outcome);
        let display: Vec<_> = found
            .iter()
            .filter(|(_, _, t)| t.starts_with("Wrong Display Name 'Pulse'"))
            .collect();
        assert_eq!(display.len(), 1, "{setting}: {outcome:#}");
        assert_eq!(display[0].0, severity, "{setting}");
        assert_eq!(display[0].1, "Observation.code.coding[0].display");
        assert!(
            !found.iter().any(|(_, _, t)| t.contains("does not pass validation")),
            "a wrong display is not a binding failure: {outcome:#}"
        );
        // The unknown HL7 code system is an error on Coding.system.
        assert!(
            found.iter().any(|(s, e, t)| s == "error"
                && e == "Observation.interpretation[0].coding[0].system"
                && t == "No definition could be found for URL value 'http://terminology.hl7.org/CodeSystem/nope'"),
            "{outcome:#}"
        );
        let sent = batches.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "one round trip per resource: {sent:#?}");
        let ops: Vec<&str> = sent[0]["entry"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["request"]["url"].as_str().unwrap())
            .collect();
        assert!(ops.contains(&"ValueSet/$validate-code"), "{ops:?}");
        assert!(ops.contains(&"CodeSystem/$validate-code"), "{ops:?}");
    }

    // The right display: nothing to report, and the answers are cached.
    let server = hfs(&ts_url, "error");
    upload_profile(&server).await;
    for _ in 0..2 {
        let response = server
            .post(&format!("/Observation/$validate?profile={PROFILE}"))
            .json(&observation("Heart rate"))
            .await;
        let outcome: Value = response.json();
        let found = issues(&outcome);
        assert!(
            found
                .iter()
                .all(|(_, e, _)| e.starts_with("Observation.interpretation")),
            "only the interpretation coding is wrong: {outcome:#}"
        );
    }
}
