//! `ImplementationGuide.global`: the profiles a stored ImplementationGuide
//! declares global apply to every resource of the named type — on
//! `$validate`, on the enforce-mode write path, after a restart — and stop
//! applying when the IG is deleted (`HFS_VALIDATION_IG_GLOBAL`).

use std::path::PathBuf;
use std::sync::Arc;

use axum_test::TestServer;
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use helios_rest::config::ValidationConfig;
use helios_rest::{MultitenancyConfig, ServerConfig, TenantRoutingMode};
use serde_json::{Value, json};

const PROFILE: &str = "http://example.org/StructureDefinition/patient-needs-name";

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("data"))
        .unwrap_or_else(|| PathBuf::from("data"))
}

fn open_backend(path: &std::path::Path) -> Arc<SqliteBackend> {
    let backend = SqliteBackend::with_config(
        path,
        SqliteBackendConfig {
            data_dir: Some(data_dir()),
            ..Default::default()
        },
    )
    .expect("sqlite backend");
    backend.init_schema().expect("schema");
    Arc::new(backend)
}

fn config(validation: ValidationConfig) -> ServerConfig {
    ServerConfig {
        multitenancy: MultitenancyConfig {
            routing_mode: TenantRoutingMode::HeaderOnly,
            ..Default::default()
        },
        base_url: "http://localhost:8080".to_string(),
        default_tenant: "test-tenant".to_string(),
        validation,
        ..ServerConfig::for_testing()
    }
}

fn server_over(
    backend: Arc<SqliteBackend>,
    validation: ValidationConfig,
) -> (TestServer, helios_rest::AppState<SqliteBackend>) {
    let state = helios_rest::AppState::new(backend, config(validation));
    let server = TestServer::new(helios_rest::routing::fhir_routes::create_routes(
        state.clone(),
    ))
    .expect("server");
    (server, state)
}

fn profile() -> Value {
    json!({
        "resourceType": "StructureDefinition",
        "id": "patient-needs-name",
        "url": PROFILE,
        "name": "PatientNeedsName",
        "status": "active",
        "kind": "resource",
        "abstract": false,
        "type": "Patient",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
        "derivation": "constraint",
        "differential": {"element": [
            {"id": "Patient", "path": "Patient"},
            {"id": "Patient.name", "path": "Patient.name", "min": 1}
        ]}
    })
}

fn ig(global: bool) -> Value {
    let mut ig = json!({
        "resourceType": "ImplementationGuide",
        "id": "test-ig",
        "url": "http://example.org/ImplementationGuide/test-ig",
        "name": "TestIg",
        "status": "active",
        "packageId": "example.test-ig",
        "fhirVersion": ["4.0.1"]
    });
    if global {
        ig["global"] = json!([{"type": "Patient", "profile": PROFILE}]);
    }
    ig
}

/// Error issues `$validate` reports for a nameless Patient.
async fn nameless_patient_errors(server: &TestServer) -> Vec<Value> {
    let response = server
        .post("/Patient/$validate")
        .json(&json!({"resourceType": "Patient", "active": true}))
        .await;
    response.assert_status_ok();
    let outcome: Value = response.json();
    outcome["issue"]
        .as_array()
        .expect("issues")
        .iter()
        .filter(|i| i["severity"] == "error" || i["severity"] == "fatal")
        .cloned()
        .collect()
}

async fn put(server: &TestServer, path: &str, body: &Value) {
    let response = server.put(path).json(body).await;
    assert!(
        response.status_code().is_success(),
        "PUT {path}: {}",
        response.text()
    );
}

#[tokio::test]
async fn stored_ig_global_profile_applies_until_deleted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (server, _) = server_over(
        open_backend(&dir.path().join("fhir.db")),
        ValidationConfig::default(),
    );
    put(
        &server,
        "/StructureDefinition/patient-needs-name",
        &profile(),
    )
    .await;
    assert!(nameless_patient_errors(&server).await.is_empty());

    put(&server, "/ImplementationGuide/test-ig", &ig(true)).await;
    let errors = nameless_patient_errors(&server).await;
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(errors[0]["expression"][0], "Patient.name", "{errors:#?}");

    // Other types are untouched.
    let response = server
        .post("/Organization/$validate")
        .json(&json!({"resourceType": "Organization", "name": "Acme"}))
        .await;
    let outcome: Value = response.json();
    assert!(
        outcome["issue"]
            .as_array()
            .expect("issues")
            .iter()
            .all(|i| i["severity"] != "error"),
        "{outcome:#}"
    );

    // A new version without `global` drops it; putting it back restores it.
    put(&server, "/ImplementationGuide/test-ig", &ig(false)).await;
    assert!(nameless_patient_errors(&server).await.is_empty());
    put(&server, "/ImplementationGuide/test-ig", &ig(true)).await;
    assert_eq!(nameless_patient_errors(&server).await.len(), 1);

    server
        .delete("/ImplementationGuide/test-ig")
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);
    assert!(nameless_patient_errors(&server).await.is_empty());

    // A transaction that writes the IG, then one that deletes it. (A new id:
    // a transaction PUT onto a deleted id is refused by the SQLite backend,
    // "resource already exists" — a separate persistence defect.)
    let mut tx_ig = ig(true);
    tx_ig["id"] = json!("tx-ig");
    let response = server
        .post("/")
        .json(&json!({"resourceType": "Bundle", "type": "transaction", "entry": [
            {"resource": tx_ig, "request": {"method": "PUT", "url": "ImplementationGuide/tx-ig"}}
        ]}))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(nameless_patient_errors(&server).await.len(), 1);
    let response = server
        .post("/")
        .json(
            &json!({"resourceType": "Bundle", "type": "transaction", "entry": [
                {"request": {"method": "DELETE", "url": "ImplementationGuide/tx-ig"}}
            ]}),
        )
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert!(nameless_patient_errors(&server).await.is_empty());
}

#[tokio::test]
async fn enforce_mode_rejects_writes_breaking_a_global_profile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (server, _) = server_over(
        open_backend(&dir.path().join("fhir.db")),
        ValidationConfig {
            mode: "enforce".to_string(),
            ..Default::default()
        },
    );
    put(
        &server,
        "/StructureDefinition/patient-needs-name",
        &profile(),
    )
    .await;
    put(&server, "/ImplementationGuide/test-ig", &ig(true)).await;
    let response = server
        .post("/Patient")
        .json(&json!({"resourceType": "Patient", "active": true}))
        .await;
    assert_eq!(response.status_code(), 422, "{}", response.text());
    let response = server
        .post("/Patient")
        .json(&json!({"resourceType": "Patient", "name": [{"family": "Doe"}]}))
        .await;
    assert_eq!(response.status_code(), 201, "{}", response.text());
}

#[tokio::test]
async fn global_profiles_are_rehydrated_and_can_be_switched_off() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("fhir.db");
    {
        let (server, _) = server_over(open_backend(&db), ValidationConfig::default());
        put(
            &server,
            "/StructureDefinition/patient-needs-name",
            &profile(),
        )
        .await;
        put(&server, "/ImplementationGuide/test-ig", &ig(true)).await;
        // An IG without `global` next to it changes nothing.
        let mut plain = ig(false);
        plain["id"] = json!("plain-ig");
        put(&server, "/ImplementationGuide/plain-ig", &plain).await;
    }

    let backend = open_backend(&db);
    let (server, state) = server_over(Arc::clone(&backend), ValidationConfig::default());
    let report = state
        .validation()
        .rehydrate_stored_profiles(backend.as_ref(), "test-tenant", 1)
        .await;
    assert_eq!(report.implementation_guides, 1, "{report:?}");
    assert_eq!(report.storage_errors, 0, "{report:?}");
    assert_eq!(nameless_patient_errors(&server).await.len(), 1);

    let (server, state) = server_over(
        Arc::clone(&backend),
        ValidationConfig {
            ig_global: false,
            ..Default::default()
        },
    );
    let report = state
        .validation()
        .rehydrate_stored_profiles(backend.as_ref(), "test-tenant", 1)
        .await;
    assert_eq!(report.implementation_guides, 0, "{report:?}");
    assert!(nameless_patient_errors(&server).await.is_empty());
}
