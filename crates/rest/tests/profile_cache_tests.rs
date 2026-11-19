//! Startup profile rehydration with the compiled-profile cache
//! (`HFS_VALIDATION_PROFILE_CACHE`): a warm restart registers the cached
//! schemas without converting anything, rows changed or deleted while the
//! server was down are reconciled, and a validation that needs no stored
//! profile never waits for a cold rehydration.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum_test::TestServer;
use helios_fhir::FhirVersion;
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use helios_persistence::core::ResourceStorage;
use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
use helios_rest::config::ValidationConfig;
use helios_rest::{MultitenancyConfig, ServerConfig, TenantRoutingMode};
use serde_json::{Value, json};

const TENANT: &str = "test-tenant";

fn profile(id: &str, required: &str) -> Value {
    json!({
        "resourceType": "StructureDefinition",
        "id": id,
        "url": format!("http://example.org/StructureDefinition/{id}"),
        "name": format!("Requires{required}"),
        "status": "active",
        "kind": "resource",
        "abstract": false,
        "type": "Patient",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
        "derivation": "constraint",
        "differential": {
            "element": [
                { "id": "Patient", "path": "Patient" },
                { "id": format!("Patient.{required}"), "path": format!("Patient.{required}"), "min": 1 }
            ]
        }
    })
}

struct Env {
    _dir: tempfile::TempDir,
    db: std::path::PathBuf,
    cache: std::path::PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        Self {
            db: dir.path().join("fhir.db"),
            cache: dir.path().join("validation-cache"),
            _dir: dir,
        }
    }

    fn backend(&self) -> Arc<SqliteBackend> {
        let backend = SqliteBackend::with_config(
            &self.db,
            SqliteBackendConfig {
                data_dir: Some(self.db.parent().unwrap().join("data")),
                ..Default::default()
            },
        )
        .expect("sqlite backend");
        backend.init_schema().expect("schema");
        Arc::new(backend)
    }

    fn config(&self, cache: bool) -> ServerConfig {
        ServerConfig {
            multitenancy: MultitenancyConfig {
                routing_mode: TenantRoutingMode::HeaderOnly,
                ..Default::default()
            },
            base_url: "http://localhost:8080".to_string(),
            default_tenant: TENANT.to_string(),
            validation: ValidationConfig {
                profile_cache: Some(if cache {
                    self.cache.display().to_string()
                } else {
                    "off".to_string()
                }),
                ..Default::default()
            },
            ..ServerConfig::for_testing()
        }
    }

    /// A fresh "process": app state over the same database, profiles
    /// rehydrated, plus a test server.
    async fn boot(
        &self,
    ) -> (
        TestServer,
        helios_rest::validation::ProfileRehydrationReport,
        Arc<SqliteBackend>,
    ) {
        let backend = self.backend();
        let state = helios_rest::AppState::new(Arc::clone(&backend), self.config(true));
        let report = state
            .validation()
            .rehydrate_stored_profiles(backend.as_ref(), TENANT, 2)
            .await;
        let server = TestServer::new(helios_rest::routing::fhir_routes::create_routes(state))
            .expect("server");
        (server, report, backend)
    }
}

/// The required element a profile enforces on an empty Patient, or `None`
/// when the profile does not resolve.
async fn enforced(server: &TestServer, id: &str) -> Option<String> {
    let url = format!("http://example.org/StructureDefinition/{id}");
    let response = server
        .post(&format!("/Patient/$validate?profile={url}"))
        .json(&json!({ "resourceType": "Patient" }))
        .await;
    response.assert_status_ok();
    let outcome: Value = response.json();
    let issues = outcome["issue"].as_array().expect("issues");
    issues
        .iter()
        .find(|i| i["code"] == "required")
        .and_then(|i| i["expression"][0].as_str())
        .map(str::to_string)
}

fn tenant() -> TenantContext {
    TenantContext::new(TenantId::new(TENANT), TenantPermissions::full_access())
}

#[tokio::test]
async fn warm_restart_registers_cached_profiles_and_reconciles_changes() {
    let env = Env::new();
    // Profiles written by a server without a cache.
    {
        let state = helios_rest::AppState::new(env.backend(), env.config(false));
        let server = TestServer::new(helios_rest::routing::fhir_routes::create_routes(state))
            .expect("server");
        for (id, required) in [("p-a", "birthDate"), ("p-b", "birthDate"), ("p-c", "gender")] {
            let response = server
                .put(&format!("/StructureDefinition/{id}"))
                .json(&profile(id, required))
                .await;
            assert!(response.status_code().is_success(), "{}", response.text());
        }
    }

    // Cold start: everything converted, the cache written.
    let (server, report, _) = env.boot().await;
    assert_eq!(
        (report.cached, report.converted, report.registered),
        (0, 3, 3),
        "{report:?}"
    );
    assert!(env.cache.join("profiles.json.gz").is_file());
    assert_eq!(
        enforced(&server, "p-a").await.as_deref(),
        Some("Patient.birthDate")
    );
    drop(server);

    // Warm start: all three come from the cache, nothing is converted.
    let (server, report, backend) = env.boot().await;
    assert_eq!(
        (report.cached, report.converted, report.registered, report.removed),
        (3, 0, 3, 0),
        "{report:?}"
    );
    assert_eq!(
        enforced(&server, "p-c").await.as_deref(),
        Some("Patient.gender")
    );
    drop(server);

    // While "down": p-a changes, p-b is deleted (straight in storage, no
    // validation service sees it).
    backend
        .update(
            &tenant(),
            &backend
                .read(&tenant(), "StructureDefinition", "p-a")
                .await
                .expect("read")
                .expect("exists"),
            profile("p-a", "gender"),
        )
        .await
        .expect("update");
    backend
        .delete(&tenant(), "StructureDefinition", "p-b")
        .await
        .expect("delete");
    drop(backend);

    let (server, report, _) = env.boot().await;
    assert_eq!(report.cached, 3, "{report:?}");
    assert_eq!(report.converted, 1, "only the changed row: {report:?}");
    assert_eq!(report.removed, 1, "the deleted row: {report:?}");
    assert_eq!(
        enforced(&server, "p-a").await.as_deref(),
        Some("Patient.gender"),
        "the changed definition replaced the cached one"
    );
    assert_eq!(enforced(&server, "p-b").await, None, "deleted stays deleted");
    drop(server);

    // And the reconciled state was written back.
    let (_server, report, _) = env.boot().await;
    assert_eq!(
        (report.cached, report.converted, report.removed),
        (2, 0, 0),
        "{report:?}"
    );
}

#[tokio::test]
async fn a_cache_from_another_build_is_ignored() {
    let env = Env::new();
    {
        let state = helios_rest::AppState::new(env.backend(), env.config(false));
        let server = TestServer::new(helios_rest::routing::fhir_routes::create_routes(state))
            .expect("server");
        let response = server
            .put("/StructureDefinition/p-a")
            .json(&profile("p-a", "birthDate"))
            .await;
        assert!(response.status_code().is_success(), "{}", response.text());
    }
    let (_server, report, _) = env.boot().await;
    assert_eq!(report.converted, 1, "{report:?}");

    // Same directory, different fingerprint: a full conversion again.
    let backend = env.backend();
    let validation = helios_rest::validation::ValidationService::new().with_profile_cache_at(Some(
        helios_rest::validation_cache::ProfileCache::with_fingerprint(&env.cache, "another-build"),
    ));
    let report = validation
        .rehydrate_stored_profiles(backend.as_ref(), TENANT, 10)
        .await;
    assert_eq!((report.cached, report.converted), (0, 1), "{report:?}");
}

/// A validation that resolves everything it needs answers at once while a
/// cold rehydration is announced; one that needs a missing profile waits.
#[tokio::test]
async fn cold_rehydration_only_blocks_validations_that_miss_a_definition() {
    let validation = helios_rest::validation::ValidationService::new();
    validation.begin_rehydration();
    assert!(validation.is_rehydrating());

    let started = Instant::now();
    let issues = validation
        .validate_resource(
            FhirVersion::R4,
            &json!({ "resourceType": "Patient", "gender": "male" }),
            Vec::new(),
            Some(TENANT),
        )
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a core-only validation must not wait for rehydration ({:?})",
        started.elapsed()
    );
    assert!(
        !helios_rest::validation::has_errors(&issues),
        "{issues:?}"
    );

    // A missing profile does wait (here until the pass is released).
    let validation = Arc::new(validation);
    let waiter = {
        let validation = Arc::clone(&validation);
        tokio::spawn(async move {
            let started = Instant::now();
            validation
                .validate_resource(
                    FhirVersion::R4,
                    &json!({ "resourceType": "Patient" }),
                    vec!["http://example.org/StructureDefinition/not-yet".to_string()],
                    Some(TENANT),
                )
                .await;
            started.elapsed()
        })
    };
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!waiter.is_finished(), "a missing profile waits for the pass");
    // Finishing the (empty) pass releases it.
    let env = Env::new();
    validation
        .rehydrate_stored_profiles(env.backend().as_ref(), TENANT, 10)
        .await;
    let waited = waiter.await.expect("waiter");
    assert!(waited >= Duration::from_millis(400), "{waited:?}");
}
