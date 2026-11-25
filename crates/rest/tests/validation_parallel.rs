//! Write-path validation on the validation thread pool
//! (`ValidationService::prepare_writes`): a Bundle's entries are validated in
//! parallel chunks, and the outcome is the one validating them one after the
//! other gives — the same cleaned resources, the first failing entry's error.

use std::sync::Arc;

use axum_test::TestServer;
use helios_fhir::FhirVersion;
use helios_persistence::backends::sqlite::SqliteBackend;
use helios_rest::config::ValidationConfig;
use helios_rest::error::RestError;
use helios_rest::validation::{PendingWrite, ValidationService};
use helios_rest::{ServerConfig, create_app_with_config};
use serde_json::{Value, json};

const TENANT: &str = "t1";

fn service(mode: &str) -> Arc<ValidationService> {
    Arc::new(ValidationService::from_config(
        &ValidationConfig {
            mode: mode.to_string(),
            ..Default::default()
        },
        None,
        FhirVersion::R4,
    ))
}

/// Patients with members the cleanup removes (`[]`, `""`, `{}`).
fn untidy_patient(n: usize) -> Value {
    json!({
        "resourceType": "Patient",
        "active": n % 2 == 0,
        "name": [{ "family": format!("Family{n}"), "given": [] }],
        "telecom": [],
        "address": [{ "city": "Taipei", "line": [], "period": {} }]
    })
}

fn tidy_patient(n: usize) -> Value {
    json!({
        "resourceType": "Patient",
        "active": n % 2 == 0,
        "name": [{ "family": format!("Family{n}") }]
    })
}

/// What the request loop did before: one entry after the other, stopping at
/// the first failure.
async fn one_by_one(
    service: &ValidationService,
    resources: &mut [(String, Value)],
) -> Result<(), RestError> {
    for (resource_type, resource) in resources.iter_mut() {
        service
            .prepare_write(TENANT, FhirVersion::R4, resource_type, resource)
            .await?;
    }
    Ok(())
}

async fn pooled(
    service: &Arc<ValidationService>,
    resources: &mut [(String, Value)],
) -> Result<(), RestError> {
    let pending = resources
        .iter_mut()
        .map(|(resource_type, resource)| PendingWrite {
            resource_type: resource_type.clone(),
            resource,
        })
        .collect();
    service
        .prepare_writes(TENANT, FhirVersion::R4, pending)
        .await
}

fn outcome_of(result: &Result<(), RestError>) -> Option<Value> {
    match result {
        Ok(()) => None,
        Err(RestError::ValidationFailed { outcome }) => Some(outcome.clone()),
        Err(other) => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pooled_validation_cleans_and_passes_like_one_by_one() {
    for (mode, make) in [
        ("off", untidy_patient as fn(usize) -> Value),
        ("log", untidy_patient),
        ("enforce", tidy_patient),
    ] {
        let service = service(mode);
        let original: Vec<(String, Value)> =
            (0..150).map(|n| ("Patient".to_string(), make(n))).collect();
        let mut expected = original.clone();
        let mut actual = original.clone();
        let sequential = one_by_one(&service, &mut expected).await;
        let parallel = pooled(&service, &mut actual).await;
        assert!(sequential.is_ok(), "{mode}: {sequential:?}");
        assert!(parallel.is_ok(), "{mode}: {parallel:?}");
        assert_eq!(actual, expected, "{mode}: same stored content");
        if mode != "enforce" {
            assert_ne!(actual, original, "{mode}: the empty members are gone");
            assert!(actual[0].1.get("telecom").is_none(), "{}", actual[0].1);
        }
        // Order and types survive the round trip through the pool.
        for (n, (resource_type, resource)) in actual.iter().enumerate() {
            assert_eq!(resource_type, "Patient");
            assert_eq!(resource["name"][0]["family"], format!("Family{n}"));
        }
    }
}

/// Two invalid entries in different chunks: the error is the first one's,
/// exactly as the sequential loop reports it, whichever chunk finishes first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enforce_reports_the_first_failing_entry() {
    let service = service("enforce");
    for round in 0..5 {
        let mut resources: Vec<(String, Value)> = (0..200)
            .map(|n| ("Patient".to_string(), tidy_patient(n)))
            .collect();
        // A late, cheap failure and an earlier one of another kind.
        resources[150 + round].1 = json!({ "resourceType": "Patient", "bogusElement": true });
        resources[37 + round].1 = json!({
            "resourceType": "Patient",
            "gender": "not-a-gender",
            "birthDate": "yesterday"
        });
        let mut expected = resources.clone();
        let sequential = outcome_of(&one_by_one(&service, &mut expected).await)
            .expect("the sequential loop rejects");
        let parallel =
            outcome_of(&pooled(&service, &mut resources).await).expect("the pool rejects");
        assert_eq!(parallel, sequential, "round {round}");
        let text = parallel.to_string();
        assert!(
            text.contains("birthDate"),
            "the first failure: {parallel:#}"
        );
        assert!(
            !text.contains("bogusElement"),
            "not the later one: {parallel:#}"
        );
        // Nothing was lost on the way back from the pool.
        for (n, (_, resource)) in resources.iter().enumerate() {
            assert_eq!(resource["resourceType"], "Patient", "entry {n}");
        }
    }
}

/// `log` mode: the entries of one Bundle that repeat an issue share one
/// line, and the count is the number of entries.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bundles_repeated_issues_are_one_line() {
    let service = service("log");
    let mut resources: Vec<(String, Value)> = (0..90)
        .map(|n| {
            (
                "Patient".to_string(),
                json!({
                    "resourceType": "Patient",
                    "gender": "not-a-gender",
                    "contact": [{ "name": { "family": format!("C{n}") }, "gender": "nope" }]
                }),
            )
        })
        .collect();
    pooled(&service, &mut resources)
        .await
        .expect("log mode proceeds");

    let counts = service.issue_log().snapshot();
    assert!(!counts.is_empty(), "the invalid codes were noticed");
    for count in &counts {
        assert_eq!(count.tenant, TENANT);
        assert_eq!(count.resource_type, "Patient");
        assert_eq!(count.issues, 90, "{count:?}");
        assert_eq!(count.lines_written, 1, "{count:?}");
        assert_eq!(count.lines_suppressed, 0, "{count:?}");
    }
    assert!(
        counts.iter().any(|c| c.path == "Patient.contact.N.gender"),
        "indices are normalised: {counts:?}"
    );

    // The same Bundle again and again: each key stops at its line quota.
    for _ in 0..9 {
        pooled(&service, &mut resources)
            .await
            .expect("log mode proceeds");
    }
    let limit = service.issue_log().limits().lines_per_key;
    for count in service.issue_log().snapshot() {
        assert_eq!(count.issues, 900, "{count:?}");
        assert_eq!(count.lines_written, limit.min(10), "{count:?}");
        assert_eq!(
            u64::from(count.lines_written) + count.lines_suppressed,
            10,
            "{count:?}"
        );
    }
}

fn server(mode: &str) -> TestServer {
    let backend = SqliteBackend::in_memory().expect("sqlite");
    backend.init_schema().expect("schema");
    let config = ServerConfig {
        validation: ValidationConfig {
            mode: mode.to_string(),
            ..Default::default()
        },
        ..ServerConfig::for_testing()
    };
    TestServer::new(create_app_with_config(backend, config)).expect("server")
}

fn transaction(resources: Vec<Value>) -> Value {
    json!({
        "resourceType": "Bundle",
        "type": "transaction",
        "entry": resources
            .into_iter()
            .map(|resource| json!({
                "resource": resource,
                "request": { "method": "POST", "url": "Patient" }
            }))
            .collect::<Vec<_>>()
    })
}

async fn patient_count(server: &TestServer) -> u64 {
    let response = server.get("/Patient?_summary=count").await;
    response.assert_status_ok();
    response.json::<Value>()["total"].as_u64().expect("total")
}

/// Through HTTP: a transaction with one invalid entry among many is rejected
/// whole under `enforce` (422 with that entry's issues, nothing stored), and
/// stored whole, cleaned, under `log`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transactions_are_validated_as_a_whole() {
    // `enforce` judges the body as sent, so its valid entries are tidy ones.
    let mut strict: Vec<Value> = (0..80).map(tidy_patient).collect();
    strict[57] = json!({ "resourceType": "Patient", "bogusElement": true });
    let mut resources: Vec<Value> = (0..80).map(untidy_patient).collect();
    resources[57] = json!({ "resourceType": "Patient", "bogusElement": true });

    let enforce = server("enforce");
    let response = enforce.post("/").json(&transaction(strict)).await;
    response.assert_status_unprocessable_entity();
    let outcome: Value = response.json();
    assert!(outcome.to_string().contains("bogusElement"), "{outcome:#}");
    assert_eq!(patient_count(&enforce).await, 0, "nothing was written");

    let log = server("log");
    let response = log.post("/").json(&transaction(resources)).await;
    response.assert_status_ok();
    assert_eq!(patient_count(&log).await, 80);
    let stored: Value = log.get("/Patient?_count=200").await.json();
    let patient = stored["entry"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| &e["resource"])
        .find(|r| r["name"][0]["family"] == "Family3")
        .expect("Family3 is stored");
    assert!(patient.get("telecom").is_none(), "cleaned: {patient:#}");
    assert!(
        patient["name"][0].get("given").is_none(),
        "cleaned: {patient:#}"
    );
}
