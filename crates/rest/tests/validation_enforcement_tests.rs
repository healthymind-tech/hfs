//! Write-path validation enforcement (`HFS_VALIDATION_MODE`) and
//! stored-profile (tenant StructureDefinition registry) integration tests.

use std::path::PathBuf;
use std::sync::Arc;

use axum_test::TestServer;
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use helios_rest::config::ValidationConfig;
use helios_rest::{MultitenancyConfig, ServerConfig, TenantRoutingMode};
use serde_json::{Value, json};

async fn create_test_server(mode: &str) -> TestServer {
    create_test_server_with(ValidationConfig {
        mode: mode.to_string(),
        ..Default::default()
    })
    .await
}

async fn create_test_server_with(validation: ValidationConfig) -> TestServer {
    let data_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("data"))
        .unwrap_or_else(|| PathBuf::from("data"));

    let backend_config = SqliteBackendConfig {
        data_dir: Some(data_dir),
        ..Default::default()
    };
    let backend = SqliteBackend::with_config(":memory:", backend_config)
        .expect("Failed to create SQLite backend");
    backend.init_schema().expect("Failed to init schema");
    let backend = Arc::new(backend);

    let config = ServerConfig {
        multitenancy: MultitenancyConfig {
            routing_mode: TenantRoutingMode::HeaderOnly,
            ..Default::default()
        },
        base_url: "http://localhost:8080".to_string(),
        default_tenant: "test-tenant".to_string(),
        validation,
        ..ServerConfig::for_testing()
    };

    let state = helios_rest::AppState::new(backend, config);
    let app = helios_rest::routing::fhir_routes::create_routes(state);
    TestServer::new(app).expect("Failed to create test server")
}

fn invalid_patient() -> Value {
    json!({ "resourceType": "Patient", "bogusElement": true })
}

fn valid_patient() -> Value {
    json!({ "resourceType": "Patient", "active": true })
}

#[tokio::test]
async fn enforce_mode_rejects_invalid_writes_with_outcome() {
    let server = create_test_server("enforce").await;

    let response = server.post("/Patient").json(&invalid_patient()).await;
    response.assert_status_unprocessable_entity();
    let outcome: Value = response.json();
    assert_eq!(outcome["resourceType"], "OperationOutcome", "{outcome:#}");
    assert!(
        outcome["issue"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["code"] == "structure" && i["expression"][0] == "Patient.bogusElement"),
        "expected the structural issue in the 422 body: {outcome:#}"
    );

    // Updates are enforced too.
    let response = server
        .put("/Patient/p1")
        .json(&json!({ "resourceType": "Patient", "id": "p1", "bogusElement": true }))
        .await;
    response.assert_status_unprocessable_entity();
}

#[tokio::test]
async fn enforce_mode_allows_valid_writes() {
    let server = create_test_server("enforce").await;
    let response = server.post("/Patient").json(&valid_patient()).await;
    assert_eq!(response.status_code(), 201, "{}", response.text());
}

#[tokio::test]
async fn log_and_off_modes_do_not_reject() {
    for mode in ["log", "off"] {
        let server = create_test_server(mode).await;
        let response = server.post("/Patient").json(&invalid_patient()).await;
        assert_eq!(
            response.status_code(),
            201,
            "mode={mode} must not reject: {}",
            response.text()
        );
    }
}

#[tokio::test]
async fn enforce_mode_rejects_invalid_batch_entries_individually() {
    let server = create_test_server("enforce").await;
    let response = server
        .post("/")
        .json(&json!({
            "resourceType": "Bundle",
            "type": "batch",
            "entry": [
                {
                    "request": { "method": "POST", "url": "Patient" },
                    "resource": invalid_patient()
                },
                {
                    "request": { "method": "POST", "url": "Patient" },
                    "resource": valid_patient()
                }
            ]
        }))
        .await;
    response.assert_status_ok();
    let bundle: Value = response.json();
    let entries = bundle["entry"].as_array().expect("entries");
    assert_eq!(entries.len(), 2);
    assert!(
        entries[0]["response"]["status"]
            .as_str()
            .unwrap_or_default()
            .starts_with("422"),
        "first entry must fail validation: {bundle:#}"
    );
    assert!(
        entries[1]["response"]["status"]
            .as_str()
            .unwrap_or_default()
            .starts_with("201"),
        "second entry must succeed: {bundle:#}"
    );
}

/// The write validator's own issues reach a batch entry, not a flattened
/// sentence.
///
/// `check_write` returns a fully-formed multi-issue OperationOutcome — one
/// issue per finding, each with its own code, severity and `expression` giving
/// the FHIRPath location of the element that failed. The batch arm used to walk
/// `issue[].details.text`, join the strings with `"; "`, and hand one sentence
/// to a wrapper that stamped `processing` over it: N coded, located issues
/// became one uncoded, unlocated issue.
///
/// The transaction arm never did that — it propagates the same error from the
/// same call with a bare `?`, reaching `IntoResponse`'s pass-through. So an
/// identical resource returned typed codes and FHIRPath expressions as a
/// `transaction` and one English sentence as a `batch`, decided purely by
/// `Bundle.type` (#504).
///
/// Complements `enforce_mode_rejects_invalid_writes_with_outcome` above, which
/// pinned the single-resource half and left the batch half asserting only a
/// status.
#[tokio::test]
async fn the_two_surfaces_report_the_same_validation_issues() {
    let server = create_test_server("enforce").await;

    /// Every `(code, expression)` pair in an outcome, sorted.
    fn issue_keys(outcome: &Value) -> Vec<(String, String)> {
        let mut keys: Vec<(String, String)> = outcome["issue"]
            .as_array()
            .expect("issue array")
            .iter()
            .map(|i| {
                (
                    i["code"].as_str().unwrap_or_default().to_string(),
                    i["expression"][0].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        keys.sort();
        keys
    }

    let single: Value = server
        .post("/Patient")
        .json(&invalid_patient())
        .await
        .json();

    let batch: Value = server
        .post("/")
        .json(&json!({
            "resourceType": "Bundle",
            "type": "batch",
            "entry": [{
                "request": { "method": "POST", "url": "Patient" },
                "resource": invalid_patient()
            }]
        }))
        .await
        .json();
    let entry = &batch["entry"][0]["response"];

    assert!(
        entry["status"]
            .as_str()
            .unwrap_or_default()
            .starts_with("422"),
        "entry status: {batch:#}"
    );

    let entry_outcome = &entry["outcome"];
    assert_eq!(
        entry_outcome["resourceType"], "OperationOutcome",
        "{batch:#}"
    );

    let single_keys = issue_keys(&single);
    let entry_keys = issue_keys(entry_outcome);
    assert_eq!(
        entry_keys, single_keys,
        "a batch entry must report the issues the resource endpoint reports\n\
         single: {single:#}\nentry: {entry_outcome:#}"
    );

    // Pinned literally, not just to each other: a regression that flattened
    // *both* surfaces would satisfy the equality above vacuously.
    assert!(
        entry_keys.contains(&("structure".to_string(), "Patient.bogusElement".to_string())),
        "the structural issue must survive with its FHIRPath location: {entry_outcome:#}"
    );
}

#[tokio::test]
async fn stored_profile_registers_and_validates() {
    let server = create_test_server("off").await;

    // Upload a differential profile constraining Patient: birthDate 1..1,
    // gender prohibited.
    let profile = json!({
        "resourceType": "StructureDefinition",
        "id": "strict-patient",
        "url": "http://example.org/StructureDefinition/strict-patient",
        "name": "StrictPatient",
        "status": "active",
        "kind": "resource",
        "abstract": false,
        "type": "Patient",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
        "derivation": "constraint",
        "differential": {
            "element": [
                { "id": "Patient", "path": "Patient" },
                { "id": "Patient.birthDate", "path": "Patient.birthDate", "min": 1 },
                { "id": "Patient.gender", "path": "Patient.gender", "max": "0" }
            ]
        }
    });
    let response = server
        .put("/StructureDefinition/strict-patient")
        .json(&profile)
        .await;
    assert!(
        response.status_code() == 201 || response.status_code() == 200,
        "profile upload failed: {}",
        response.text()
    );

    // $validate against the stored profile: violations reported.
    let response = server
        .post("/Patient/$validate?profile=http://example.org/StructureDefinition/strict-patient")
        .json(&json!({ "resourceType": "Patient", "gender": "male" }))
        .await;
    response.assert_status_ok();
    let outcome: Value = response.json();
    let issues = outcome["issue"].as_array().expect("issues");
    assert!(
        issues
            .iter()
            .any(|i| i["code"] == "required" && i["expression"][0] == "Patient.birthDate"),
        "profile-required birthDate must be reported: {outcome:#}"
    );
    assert!(
        issues
            .iter()
            .any(|i| i["code"] == "structure" && i["expression"][0] == "Patient.gender"),
        "profile-excluded gender must be reported: {outcome:#}"
    );

    // A conforming resource passes.
    let response = server
        .post("/Patient/$validate?profile=http://example.org/StructureDefinition/strict-patient")
        .json(&json!({ "resourceType": "Patient", "birthDate": "1980-01-01" }))
        .await;
    response.assert_status_ok();
    let outcome: Value = response.json();
    assert_eq!(
        outcome["issue"][0]["severity"], "information",
        "conforming resource must validate clean: {outcome:#}"
    );
}

#[tokio::test]
async fn enforce_mode_honors_meta_profile_claims() {
    let server = create_test_server("enforce").await;

    let profile = json!({
        "resourceType": "StructureDefinition",
        "id": "must-have-birthdate",
        "url": "http://example.org/StructureDefinition/must-have-birthdate",
        "name": "MustHaveBirthDate",
        "status": "active",
        "kind": "resource",
        "abstract": false,
        "type": "Patient",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
        "derivation": "constraint",
        "differential": {
            "element": [
                { "id": "Patient", "path": "Patient" },
                { "id": "Patient.birthDate", "path": "Patient.birthDate", "min": 1 }
            ]
        }
    });
    let response = server
        .put("/StructureDefinition/must-have-birthdate")
        .json(&profile)
        .await;
    assert!(
        response.status_code().is_success(),
        "profile upload failed: {}",
        response.text()
    );

    // A write claiming the profile but violating it is rejected.
    let response = server
        .post("/Patient")
        .json(&json!({
            "resourceType": "Patient",
            "meta": { "profile": ["http://example.org/StructureDefinition/must-have-birthdate"] }
        }))
        .await;
    response.assert_status_unprocessable_entity();

    // Satisfying the claimed profile passes.
    let response = server
        .post("/Patient")
        .json(&json!({
            "resourceType": "Patient",
            "meta": { "profile": ["http://example.org/StructureDefinition/must-have-birthdate"] },
            "birthDate": "1980-01-01"
        }))
        .await;
    assert_eq!(response.status_code(), 201, "{}", response.text());
}

/// A minimal stand-in for `hl7.fhir.r4.core`'s `rendering-xhtml` extension:
/// an extension whose `name` is `xhtml`, the same as the primitive type.
fn xhtml_named_extension() -> Value {
    json!({
        "resourceType": "StructureDefinition",
        "id": "rendering-xhtml",
        "url": "http://example.org/StructureDefinition/rendering-xhtml",
        "name": "xhtml",
        "status": "active",
        "kind": "complex-type",
        "abstract": false,
        "context": [{ "type": "element", "expression": "Element" }],
        "type": "Extension",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Extension",
        "derivation": "constraint",
        "differential": {
            "element": [
                { "id": "Extension", "path": "Extension", "short": "String equivalent with html markup" },
                { "id": "Extension.extension", "path": "Extension.extension", "max": "0" },
                { "id": "Extension.url", "path": "Extension.url",
                  "fixedUri": "http://example.org/StructureDefinition/rendering-xhtml" },
                { "id": "Extension.value[x]", "path": "Extension.value[x]", "min": 1,
                  "type": [{ "code": "string" }] }
            ]
        }
    })
}

fn patient_with_narrative() -> Value {
    json!({
        "resourceType": "Patient",
        "text": {
            "status": "generated",
            "div": "<div xmlns=\"http://www.w3.org/1999/xhtml\">Hello</div>"
        },
        "name": [{ "family": "X" }]
    })
}

fn error_issues(outcome: &Value) -> Vec<Value> {
    outcome["issue"]
        .as_array()
        .map(|issues| {
            issues
                .iter()
                .filter(|i| i["severity"] == "error")
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// H4: a stored extension whose `name` collides with a core type must not be
/// resolvable by that name — `Narrative.div` (type `xhtml`) kept resolving to
/// the `rendering-xhtml` extension schema and every narrative failed with
/// "expected object, got string".
#[tokio::test]
async fn stored_extension_named_like_a_core_type_does_not_shadow_it() {
    let server = create_test_server("off").await;

    // Narrative validates clean before the upload…
    let response = server
        .post("/Patient/$validate")
        .json(&patient_with_narrative())
        .await;
    response.assert_status_ok();
    let before: Value = response.json();
    assert!(error_issues(&before).is_empty(), "{before:#}");

    let response = server
        .put("/StructureDefinition/rendering-xhtml")
        .json(&xhtml_named_extension())
        .await;
    assert!(response.status_code().is_success(), "{}", response.text());

    // …and still does afterwards.
    let response = server
        .post("/Patient/$validate")
        .json(&patient_with_narrative())
        .await;
    response.assert_status_ok();
    let after: Value = response.json();
    assert!(
        error_issues(&after).is_empty(),
        "a stored extension named 'xhtml' must not shadow the xhtml type: {after:#}"
    );

    // The extension itself is still registered under its canonical URL.
    let response = server
        .post("/Patient/$validate?profile=http://example.org/StructureDefinition/rendering-xhtml")
        .json(&patient_with_narrative())
        .await;
    response.assert_status_ok();
    let outcome: Value = response.json();
    assert!(
        !outcome["issue"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["code"] == "not-supported"),
        "the extension must resolve by url: {outcome:#}"
    );
}

/// A StructureDefinition carrying a core canonical URL (what importing the
/// `hl7.fhir.r4.core` package produces) must not replace the embedded
/// definition.
fn core_patient_requiring_birthdate() -> Value {
    json!({
        "resourceType": "StructureDefinition",
        "id": "Patient",
        "url": "http://hl7.org/fhir/StructureDefinition/Patient",
        "name": "Patient",
        "status": "active",
        "kind": "resource",
        "abstract": false,
        "type": "Patient",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/DomainResource",
        "derivation": "specialization",
        "differential": {
            "element": [
                { "id": "Patient", "path": "Patient" },
                { "id": "Patient.birthDate", "path": "Patient.birthDate", "min": 1, "max": "1",
                  "type": [{ "code": "date" }] }
            ]
        }
    })
}

#[tokio::test]
async fn core_canonicals_do_not_override_embedded_definitions() {
    let server = create_test_server("off").await;
    let response = server
        .put("/StructureDefinition/Patient")
        .json(&core_patient_requiring_birthdate())
        .await;
    assert!(response.status_code().is_success(), "{}", response.text());

    for path in [
        "/Patient/$validate",
        "/Patient/$validate?profile=http://hl7.org/fhir/StructureDefinition/Patient",
    ] {
        let response = server
            .post(path)
            .json(&json!({ "resourceType": "Patient", "name": [{ "family": "X" }] }))
            .await;
        response.assert_status_ok();
        let outcome: Value = response.json();
        assert!(
            error_issues(&outcome).is_empty(),
            "{path}: the embedded Patient definition must win over the stored core canonical: {outcome:#}"
        );
    }
}

#[tokio::test]
async fn shadow_core_opt_in_lets_a_stored_core_canonical_win() {
    let server = create_test_server_with(ValidationConfig {
        shadow_core: true,
        ..Default::default()
    })
    .await;
    let response = server
        .put("/StructureDefinition/Patient")
        .json(&core_patient_requiring_birthdate())
        .await;
    assert!(response.status_code().is_success(), "{}", response.text());

    // The stored definition is reachable by its canonical URL. (The root
    // resource type itself still resolves by *name* to the embedded schema —
    // stored definitions never take a core type's name alias.)
    let response = server
        .post("/Patient/$validate?profile=http://hl7.org/fhir/StructureDefinition/Patient")
        .json(&json!({ "resourceType": "Patient", "name": [{ "family": "X" }] }))
        .await;
    response.assert_status_ok();
    let outcome: Value = response.json();
    assert!(
        error_issues(&outcome)
            .iter()
            .any(|i| i["code"] == "required" && i["expression"][0] == "Patient.birthDate"),
        "HFS_VALIDATION_SHADOW_CORE=true must let the stored definition override: {outcome:#}"
    );
}

/// A differential profile constraining Patient: birthDate 1..1.
fn birthdate_profile(id: &str) -> Value {
    json!({
        "resourceType": "StructureDefinition",
        "id": id,
        "url": format!("http://example.org/StructureDefinition/{id}"),
        "name": "MustHaveBirthDate",
        "status": "active",
        "kind": "resource",
        "abstract": false,
        "type": "Patient",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
        "derivation": "constraint",
        "differential": {
            "element": [
                { "id": "Patient", "path": "Patient" },
                { "id": "Patient.birthDate", "path": "Patient.birthDate", "min": 1 }
            ]
        }
    })
}

/// Whether `$validate?profile=<url>` currently resolves the profile.
async fn profile_resolves(server: &TestServer, url: &str) -> bool {
    let response = server
        .post(&format!("/Patient/$validate?profile={url}"))
        .json(&json!({ "resourceType": "Patient" }))
        .await;
    response.assert_status_ok();
    let outcome: Value = response.json();
    let issues = outcome["issue"].as_array().expect("issues");
    let unresolved = issues.iter().any(|i| {
        i["code"] == "not-supported"
            && i["details"]["text"]
                .as_str()
                .is_some_and(|t| t.contains("could not be found"))
    });
    let enforced = issues
        .iter()
        .any(|i| i["code"] == "required" && i["expression"][0] == "Patient.birthDate");
    assert_ne!(unresolved, enforced, "{outcome:#}");
    enforced
}

/// H2: deleting a StructureDefinition must take it out of the profile
/// registry (there was no remove path at all — a deleted profile kept
/// validating until the next restart).
#[tokio::test]
async fn deleting_a_stored_profile_unregisters_it() {
    let server = create_test_server("off").await;
    let url = "http://example.org/StructureDefinition/del-me";

    let response = server
        .put("/StructureDefinition/del-me")
        .json(&birthdate_profile("del-me"))
        .await;
    assert!(response.status_code().is_success(), "{}", response.text());
    assert!(profile_resolves(&server, url).await);

    server
        .delete("/StructureDefinition/del-me")
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);
    assert!(
        !profile_resolves(&server, url).await,
        "a deleted profile must no longer resolve"
    );

    // Re-uploading brings it back; a conditional delete removes it again.
    let response = server
        .put("/StructureDefinition/del-me")
        .json(&birthdate_profile("del-me"))
        .await;
    assert!(response.status_code().is_success(), "{}", response.text());
    assert!(profile_resolves(&server, url).await);
    server
        .delete(&format!("/StructureDefinition?url={url}"))
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);
    assert!(!profile_resolves(&server, url).await);
}

#[tokio::test]
async fn bundle_delete_of_a_stored_profile_unregisters_it() {
    let server = create_test_server("off").await;
    let url_a = "http://example.org/StructureDefinition/bundle-a";
    let url_b = "http://example.org/StructureDefinition/bundle-b";
    for id in ["bundle-a", "bundle-b"] {
        let response = server
            .put(&format!("/StructureDefinition/{id}"))
            .json(&birthdate_profile(id))
            .await;
        assert!(response.status_code().is_success(), "{}", response.text());
    }
    assert!(profile_resolves(&server, url_a).await);
    assert!(profile_resolves(&server, url_b).await);

    for bundle_type in ["batch", "transaction"] {
        let response = server
            .post("/")
            .json(&json!({
                "resourceType": "Bundle",
                "type": bundle_type,
                "entry": [
                    { "request": { "method": "DELETE", "url": "StructureDefinition/bundle-a" } },
                    { "request": { "method": "DELETE", "url": format!("StructureDefinition?url={url_b}") } }
                ]
            }))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        assert!(
            !profile_resolves(&server, url_a).await,
            "{bundle_type}: instance DELETE"
        );
        assert!(
            !profile_resolves(&server, url_b).await,
            "{bundle_type}: conditional DELETE"
        );
        for id in ["bundle-a", "bundle-b"] {
            let response = server
                .put(&format!("/StructureDefinition/{id}"))
                .json(&birthdate_profile(id))
                .await;
            assert!(response.status_code().is_success(), "{}", response.text());
        }
    }
}

/// H2: a fresh process over the same database must know the stored profiles
/// without every one of them being re-written.
#[tokio::test]
async fn stored_profiles_are_rehydrated_from_storage_on_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("fhir.db");
    let data_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("data"))
        .unwrap_or_else(|| PathBuf::from("data"));
    let open_backend = || {
        let backend = SqliteBackend::with_config(
            &db_path,
            SqliteBackendConfig {
                data_dir: Some(data_dir.clone()),
                ..Default::default()
            },
        )
        .expect("sqlite backend");
        backend.init_schema().expect("schema");
        Arc::new(backend)
    };
    let config = || ServerConfig {
        multitenancy: MultitenancyConfig {
            routing_mode: TenantRoutingMode::HeaderOnly,
            ..Default::default()
        },
        base_url: "http://localhost:8080".to_string(),
        default_tenant: "test-tenant".to_string(),
        ..ServerConfig::for_testing()
    };
    let url = "http://example.org/StructureDefinition/survives-restart";

    // Process 1: upload the profile (plus one that is deleted again, and a
    // core canonical that must stay excluded).
    {
        let state = helios_rest::AppState::new(open_backend(), config());
        let server = TestServer::new(helios_rest::routing::fhir_routes::create_routes(state))
            .expect("server");
        for id in ["survives-restart", "deleted-before-restart"] {
            let response = server
                .put(&format!("/StructureDefinition/{id}"))
                .json(&birthdate_profile(id))
                .await;
            assert!(response.status_code().is_success(), "{}", response.text());
        }
        server
            .delete("/StructureDefinition/deleted-before-restart")
            .await
            .assert_status(axum::http::StatusCode::NO_CONTENT);
        let response = server
            .put("/StructureDefinition/Patient")
            .json(&core_patient_requiring_birthdate())
            .await;
        assert!(response.status_code().is_success(), "{}", response.text());
        assert!(profile_resolves(&server, url).await);
    }

    // Process 2: a new AppState over the same file. Nothing is known until
    // the replay runs (the router builder does not spawn it — `build_app`
    // does); after the replay the profile resolves.
    let backend = open_backend();
    let state = helios_rest::AppState::new(Arc::clone(&backend), config());
    let server = TestServer::new(helios_rest::routing::fhir_routes::create_routes(
        state.clone(),
    ))
    .expect("server");
    assert!(
        !profile_resolves(&server, url).await,
        "registries start empty in a new process"
    );

    let report = state
        .validation()
        .rehydrate_stored_profiles(backend.as_ref(), "test-tenant", 1)
        .await;
    assert_eq!(report.tenants, 1, "{report:?}");
    assert_eq!(
        report.registered, 1,
        "only the live, non-core profile: {report:?}"
    );
    assert_eq!(
        report.skipped, 1,
        "the core canonical is skipped: {report:?}"
    );
    assert_eq!(report.storage_errors, 0, "{report:?}");
    assert!(
        profile_resolves(&server, url).await,
        "rehydrated profile resolves"
    );
    assert!(
        !profile_resolves(
            &server,
            "http://example.org/StructureDefinition/deleted-before-restart"
        )
        .await,
        "deleted profiles are not resurrected"
    );
}

/// While a startup rehydration pass is announced, validation waits for it
/// instead of reporting a stored profile as unresolvable.
#[tokio::test]
async fn validation_waits_for_an_announced_rehydration() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("fhir.db");
    let backend = || {
        let backend = SqliteBackend::with_config(&db_path, SqliteBackendConfig::default())
            .expect("sqlite backend");
        backend.init_schema().expect("schema");
        Arc::new(backend)
    };
    let config = || ServerConfig {
        multitenancy: MultitenancyConfig {
            routing_mode: TenantRoutingMode::HeaderOnly,
            ..Default::default()
        },
        base_url: "http://localhost:8080".to_string(),
        default_tenant: "test-tenant".to_string(),
        ..ServerConfig::for_testing()
    };
    let url = "http://example.org/StructureDefinition/gated";
    {
        let state = helios_rest::AppState::new(backend(), config());
        let server = TestServer::new(helios_rest::routing::fhir_routes::create_routes(state))
            .expect("server");
        let response = server
            .put("/StructureDefinition/gated")
            .json(&birthdate_profile("gated"))
            .await;
        assert!(response.status_code().is_success(), "{}", response.text());
    }

    let storage = backend();
    let state = helios_rest::AppState::new(Arc::clone(&storage), config());
    let validation = state.validation_shared();
    validation.begin_rehydration();
    assert!(validation.is_rehydrating());
    let server =
        TestServer::new(helios_rest::routing::fhir_routes::create_routes(state)).expect("server");

    // The replay starts only after the request is already waiting.
    let replay = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        validation
            .rehydrate_stored_profiles(storage.as_ref(), "test-tenant", 100)
            .await
    });
    assert!(
        profile_resolves(&server, url).await,
        "a request during rehydration must see the replayed profile"
    );
    assert_eq!(replay.await.expect("replay").registered, 1);
}
