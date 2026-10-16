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

#[derive(Clone, Default)]
struct MockHts {
    requests: Arc<Mutex<Vec<Value>>>,
    /// value set url → codes it contains.
    known: Arc<HashMap<String, Vec<String>>>,
    /// code system url → its `content` (`GET /CodeSystem?url=`).
    code_systems: Arc<HashMap<String, String>>,
    /// value set url → its `compose` (`GET /ValueSet?url=`).
    composes: Arc<HashMap<String, Value>>,
    /// Code systems a negative answer blames via `x-caused-by-unknown-system`.
    blame_unknown_system: Option<String>,
}

#[derive(serde::Deserialize)]
struct UrlQuery {
    url: String,
}

fn searchset(resources: Vec<Value>) -> Json<Value> {
    Json(json!({
        "resourceType": "Bundle", "type": "searchset", "total": resources.len(),
        "entry": resources.into_iter().map(|r| json!({ "resource": r })).collect::<Vec<_>>()
    }))
}

async fn search_code_systems(
    State(state): State<MockHts>,
    axum::extract::Query(q): axum::extract::Query<UrlQuery>,
) -> Json<Value> {
    searchset(
        state
            .code_systems
            .get(&q.url)
            .map(
                |content| json!({ "resourceType": "CodeSystem", "url": q.url, "content": content }),
            )
            .into_iter()
            .collect(),
    )
}

async fn search_value_sets(
    State(state): State<MockHts>,
    axum::extract::Query(q): axum::extract::Query<UrlQuery>,
) -> Json<Value> {
    searchset(
        state
            .composes
            .get(&q.url)
            .map(|compose| json!({ "resourceType": "ValueSet", "url": q.url, "version": "1.0.0", "compose": compose }))
            .into_iter()
            .collect(),
    )
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
    let mut parameter = vec![json!({ "name": "result", "valueBoolean": result })];
    if let (false, Some(system)) = (result, &state.blame_unknown_system) {
        parameter.push(json!({ "name": "x-caused-by-unknown-system", "valueCanonical": system }));
    }
    (
        StatusCode::OK,
        Json(json!({ "resourceType": "Parameters", "parameter": parameter })),
    )
}

async fn start_mock_hts(known: &[(&str, &[&str])]) -> (String, Arc<Mutex<Vec<Value>>>) {
    start_mock(MockHts {
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
        ..MockHts::default()
    })
    .await
}

async fn start_mock(mut state: MockHts) -> (String, Arc<Mutex<Vec<Value>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    state.requests = Arc::clone(&requests);
    let router = Router::new()
        .route(
            "/ValueSet/$validate-code",
            axum::routing::post(validate_code),
        )
        .route("/CodeSystem", axum::routing::get(search_code_systems))
        .route("/ValueSet", axum::routing::get(search_value_sets))
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

const GENES: &str = "http://www.genenames.org";

fn gene_patient(code: &str) -> Value {
    json!({
        "resourceType": "Patient",
        "maritalStatus": { "coding": [{ "system": GENES, "code": code }] }
    })
}

/// A negative `$validate-code` that only reflects a code system the server
/// has no content for (a `content: not-present` stub such as HGNC in
/// `hl7.terminology`) cannot be checked: a warning, like the HL7 validator's
/// "A definition for CodeSystem … could not be found, so the code cannot be
/// validated" — not an invalid code. An enumerated `include.concept` list is
/// still a real membership answer, and the server can say so itself with
/// `x-caused-by-unknown-system`.
#[tokio::test]
async fn codes_of_contentless_code_systems_cannot_be_checked() {
    let stub = |compose: Value| MockHts {
        known: Arc::new(HashMap::from([(
            IG_VS.to_string(),
            vec!["HGNC:1".to_string()],
        )])),
        code_systems: Arc::new(HashMap::from([(
            GENES.to_string(),
            "not-present".to_string(),
        )])),
        composes: Arc::new(HashMap::from([(IG_VS.to_string(), compose)])),
        ..MockHts::default()
    };

    // The value set takes the whole stub system: unverifiable -> warning.
    let (ts_url, _) = start_mock(stub(json!({ "include": [{ "system": GENES }] }))).await;
    for (fail, severity) in [
        ("open", "warning"),
        ("closed-required", "warning"),
        ("closed", "error"),
    ] {
        let server = hfs(&ts_url, "remote", fail);
        upload_profile(&server).await;
        let outcome = validate(&server, &gene_patient("HGNC:1100"), true).await;
        let issues = binding_issues(&outcome);
        assert_eq!(issues.len(), 1, "{fail}: {outcome:#}");
        assert_eq!(issues[0].0, severity, "{fail}: {outcome:#}");
        assert!(
            issues[0].2.contains("could not be performed"),
            "{}",
            issues[0].2
        );
        assert!(issues[0].2.contains(GENES), "{}", issues[0].2);
    }

    // An enumerated list for the same stub system is a real answer: error.
    let (ts_url, _) = start_mock(stub(json!({
        "include": [{ "system": GENES, "concept": [{ "code": "HGNC:1" }] }]
    })))
    .await;
    let server = hfs(&ts_url, "remote", "open");
    upload_profile(&server).await;
    let outcome = validate(&server, &gene_patient("HGNC:1100"), true).await;
    let issues = binding_issues(&outcome);
    assert_eq!(issues.len(), 1, "{outcome:#}");
    assert_eq!(issues[0].0, "error", "{outcome:#}");
    assert!(
        issues[0].2.contains("does not pass validation"),
        "{}",
        issues[0].2
    );

    // A code system with real content: the negative answer stands.
    let (ts_url, _) = start_mock(MockHts {
        code_systems: Arc::new(HashMap::from([(GENES.to_string(), "complete".to_string())])),
        ..stub(json!({ "include": [{ "system": GENES }] }))
    })
    .await;
    let server = hfs(&ts_url, "remote", "open");
    upload_profile(&server).await;
    let outcome = validate(&server, &gene_patient("HGNC:1100"), true).await;
    assert_eq!(binding_issues(&outcome)[0].0, "error", "{outcome:#}");

    // The server blames an unknown code system itself.
    let (ts_url, _) = start_mock(MockHts {
        blame_unknown_system: Some("http://example.org/cs".to_string()),
        ..MockHts {
            known: Arc::new(HashMap::from([(IG_VS.to_string(), vec!["M".to_string()])])),
            ..MockHts::default()
        }
    })
    .await;
    let server = hfs(&ts_url, "remote", "open");
    upload_profile(&server).await;
    let outcome = validate(&server, &patient("male", "X"), true).await;
    let issues = binding_issues(&outcome);
    assert_eq!(issues.len(), 1, "{outcome:#}");
    assert_eq!(issues[0].0, "warning", "{outcome:#}");
    assert!(
        issues[0].2.contains("http://example.org/cs"),
        "{}",
        issues[0].2
    );
}
