//! Required-binding validation against a terminology server
//! (`HFS_VALIDATION_TERMINOLOGY=remote|remote-only`, `HFS_VALIDATION_TERMINOLOGY_FAIL`).
//!
//! A mock HTS answers `POST /ValueSet/$validate-code`, records every request
//! body, knows a configurable set of value sets and answers `404` +
//! OperationOutcome `not-found` for the rest — what a real server does for an
//! IG value set that was never loaded.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum_test::TestServer;
use helios_persistence::backends::sqlite::SqliteBackend;
use helios_rest::config::ValidationConfig;
use helios_rest::{ServerConfig, create_app_with_config};
use serde_json::{Value, json};
use tokio::net::TcpListener;

#[derive(Clone)]
struct MockHts {
    requests: Arc<Mutex<Vec<Value>>>,
    /// value set url → codes it contains.
    known: Arc<HashMap<String, Vec<String>>>,
}

fn param<'a>(body: &'a Value, name: &str) -> Option<&'a Value> {
    body["parameter"]
        .as_array()?
        .iter()
        .find(|p| p["name"] == name)
}

async fn validate_code(
    State(state): State<MockHts>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    state.requests.lock().unwrap().push(body.clone());
    let url = param(&body, "url")
        .and_then(|p| p["valueUri"].as_str())
        .unwrap_or_default();
    let Some(codes) = state.known.get(url) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "resourceType": "OperationOutcome",
                "issue": [{ "severity": "error", "code": "not-found",
                            "diagnostics": format!("ValueSet {url} not found") }]
            })),
        );
    };
    let code = param(&body, "code")
        .and_then(|p| p["valueCode"].as_str())
        .or_else(|| param(&body, "coding").and_then(|p| p["valueCoding"]["code"].as_str()))
        .or_else(|| {
            param(&body, "codeableConcept")
                .and_then(|p| p["valueCodeableConcept"]["coding"][0]["code"].as_str())
        })
        .unwrap_or_default();
    let result = codes.iter().any(|c| c == code);
    (
        StatusCode::OK,
        Json(json!({
            "resourceType": "Parameters",
            "parameter": [{ "name": "result", "valueBoolean": result }]
        })),
    )
}

async fn start_mock_hts(known: &[(&str, &[&str])]) -> (String, Arc<Mutex<Vec<Value>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let state = MockHts {
        requests: Arc::clone(&requests),
        known: Arc::new(
            known
                .iter()
                .map(|(url, codes)| {
                    (
                        url.to_string(),
                        codes.iter().map(|c| c.to_string()).collect(),
                    )
                })
                .collect(),
        ),
    };
    let router = Router::new()
        .route(
            "/ValueSet/$validate-code",
            axum::routing::post(validate_code),
        )
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (base_url, requests)
}

fn hfs(ts_url: &str, terminology: &str, fail: &str) -> TestServer {
    let backend = SqliteBackend::in_memory().expect("sqlite");
    backend.init_schema().expect("schema");
    let config = ServerConfig {
        terminology_server: Some(ts_url.to_string()),
        validation: ValidationConfig {
            terminology: terminology.to_string(),
            terminology_fail: fail.to_string(),
            ..Default::default()
        },
        ..ServerConfig::for_testing()
    };
    TestServer::new(create_app_with_config(backend, config)).expect("server")
}

const IG_VS: &str = "http://example.org/ValueSet/marital-status-ig";
const PROFILE: &str = "http://example.org/StructureDefinition/ig-patient";

/// A profile binding `Patient.maritalStatus` (required) to a versioned IG
/// value set — the shape every IG StructureDefinition has.
async fn upload_profile(server: &TestServer) {
    let response = server
        .put("/StructureDefinition/ig-patient")
        .json(&json!({
            "resourceType": "StructureDefinition",
            "id": "ig-patient",
            "url": PROFILE,
            "name": "IgPatient",
            "status": "active",
            "kind": "resource",
            "abstract": false,
            "type": "Patient",
            "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
            "derivation": "constraint",
            "differential": { "element": [
                { "id": "Patient", "path": "Patient" },
                { "id": "Patient.maritalStatus", "path": "Patient.maritalStatus",
                  "binding": { "strength": "required", "valueSet": format!("{IG_VS}|1.0.0") } }
            ] }
        }))
        .await;
    assert!(response.status_code().is_success(), "{}", response.text());
}

fn patient(gender: &str, marital: &str) -> Value {
    json!({
        "resourceType": "Patient",
        "gender": gender,
        "maritalStatus": { "coding": [{ "system": "http://example.org/cs", "code": marital }] }
    })
}

async fn validate(server: &TestServer, resource: &Value, profile: bool) -> Value {
    let path = if profile {
        format!("/Patient/$validate?profile={PROFILE}")
    } else {
        "/Patient/$validate".to_string()
    };
    let response = server.post(&path).json(resource).await;
    response.assert_status_ok();
    response.json()
}

fn binding_issues(outcome: &Value) -> Vec<(String, String, String)> {
    outcome["issue"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["code"] == "code-invalid")
        .map(|i| {
            (
                i["severity"].as_str().unwrap().to_string(),
                i["expression"][0].as_str().unwrap_or_default().to_string(),
                i["details"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

/// `remote`: core bindings are answered by the embedded value sets without a
/// round-trip; IG bindings go to the server with `url` and `valueSetVersion`
/// split out of the `url|version` canonical.
#[tokio::test]
async fn remote_mode_checks_core_bindings_locally_and_splits_versioned_canonicals() {
    let (ts_url, requests) = start_mock_hts(&[(IG_VS, &["M", "S"])]).await;
    let server = hfs(&ts_url, "remote", "open");
    upload_profile(&server).await;

    // Core binding, bogus code: an error, and the server was never asked.
    let outcome = validate(&server, &patient("unknownx", "M"), false).await;
    let issues = binding_issues(&outcome);
    assert_eq!(issues.len(), 1, "{outcome:#}");
    assert_eq!(issues[0].0, "error");
    assert_eq!(issues[0].1, "Patient.gender");
    assert!(
        requests.lock().unwrap().is_empty(),
        "core bindings never reach HTS"
    );

    // IG binding: one request, split canonical, verdict honoured.
    let outcome = validate(&server, &patient("male", "X"), true).await;
    let issues = binding_issues(&outcome);
    assert_eq!(issues.len(), 1, "{outcome:#}");
    assert_eq!(issues[0].0, "error");
    assert_eq!(issues[0].1, "Patient.maritalStatus");
    {
        let reqs = requests.lock().unwrap();
        assert_eq!(reqs.len(), 1, "{reqs:#?}");
        assert_eq!(param(&reqs[0], "url").unwrap()["valueUri"], IG_VS);
        assert_eq!(
            param(&reqs[0], "valueSetVersion").unwrap()["valueString"],
            "1.0.0"
        );
        assert_eq!(
            param(&reqs[0], "codeableConcept").unwrap()["valueCodeableConcept"]["coding"][0]["code"],
            "X"
        );
    }

    // A valid IG code passes; the verdict is cached per (value set, value).
    let outcome = validate(&server, &patient("male", "M"), true).await;
    assert!(binding_issues(&outcome).is_empty(), "{outcome:#}");
    let outcome = validate(&server, &patient("male", "M"), true).await;
    assert!(binding_issues(&outcome).is_empty(), "{outcome:#}");
    assert_eq!(
        requests.lock().unwrap().len(),
        2,
        "second identical check is served from cache"
    );
}

/// `remote-only`: every required binding goes to the server (the pre-H3
/// behaviour), still with the canonical split.
#[tokio::test]
async fn remote_only_sends_core_bindings_to_the_server() {
    let (ts_url, requests) = start_mock_hts(&[(
        "http://hl7.org/fhir/ValueSet/administrative-gender",
        &["male", "female", "other", "unknown"],
    )])
    .await;
    let server = hfs(&ts_url, "remote-only", "open");

    let outcome = validate(
        &server,
        &json!({ "resourceType": "Patient", "gender": "unknownx" }),
        false,
    )
    .await;
    let issues = binding_issues(&outcome);
    assert_eq!(issues.len(), 1, "{outcome:#}");
    assert_eq!(issues[0].0, "error");
    let reqs = requests.lock().unwrap();
    assert_eq!(reqs.len(), 1, "{reqs:#?}");
    assert_eq!(
        param(&reqs[0], "url").unwrap()["valueUri"],
        "http://hl7.org/fhir/ValueSet/administrative-gender"
    );
    assert_eq!(
        param(&reqs[0], "valueSetVersion").unwrap()["valueString"],
        "4.0.1"
    );
}

/// The unresolvable-ValueSet policy: `open` warns, `closed-required` errors
/// on a value set the server does not know but still only warns on an
/// outage, `closed` errors on both.
#[tokio::test]
async fn unknown_value_set_policy_is_explicit() {
    let (ts_url, requests) = start_mock_hts(&[]).await; // knows nothing
    // A port nothing listens on: an outage.
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_url = format!("http://127.0.0.1:{}", dead.local_addr().unwrap().port());
    drop(dead);

    for (fail, unknown_severity, outage_severity) in [
        ("open", "warning", "warning"),
        ("closed-required", "error", "warning"),
        ("closed", "error", "error"),
    ] {
        let server = hfs(&ts_url, "remote", fail);
        upload_profile(&server).await;
        let outcome = validate(&server, &patient("male", "M"), true).await;
        let issues = binding_issues(&outcome);
        assert_eq!(issues.len(), 1, "{fail}: {outcome:#}");
        assert_eq!(issues[0].0, unknown_severity, "{fail}: unknown value set");
        assert_eq!(issues[0].1, "Patient.maritalStatus");
        assert!(
            issues[0].2.contains("could not be performed"),
            "{fail}: {}",
            issues[0].2
        );

        let server = hfs(&dead_url, "remote", fail);
        upload_profile(&server).await;
        let outcome = validate(&server, &patient("male", "M"), true).await;
        let issues = binding_issues(&outcome);
        assert_eq!(issues.len(), 1, "{fail}: {outcome:#}");
        assert_eq!(issues[0].0, outage_severity, "{fail}: outage");
    }

    // The unknown value set was asked about once per server instance (three
    // instances above), never once per coded element: the negative answer
    // is cached.
    let server = hfs(&ts_url, "remote", "open");
    upload_profile(&server).await;
    let before = requests.lock().unwrap().len();
    for marital in ["M", "S", "D"] {
        validate(&server, &patient("male", marital), true).await;
    }
    assert_eq!(
        requests.lock().unwrap().len(),
        before + 1,
        "a 404 is remembered for the value set, not retried per code"
    );
}
