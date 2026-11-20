//! Sync writes into local endpoints go through the target endpoint's normal
//! write path (FHIR server manager P6): `enforce` rejects and records the
//! item with its OperationOutcome, `log` writes and records the findings,
//! `off` writes silently; and a duplicated endpoint validates against the
//! copied IG profiles at once (`$validate`), without a restart.

use std::path::PathBuf;
use std::sync::Arc;

use axum::http::StatusCode;
use axum_test::TestServer;
use helios_endpoints::model::{ProfileRequirement, TerminologyValidation};
use helios_endpoints::sync::SyncRunState;
use helios_endpoints::{
    Actor, EndpointName, EndpointRegistry, EndpointSpec, EndpointStatus, EndpointStore,
    FhirRelease, MemoryEndpointStore, ValidationMode, ValidationSettings, ensure_default_endpoint,
};
use helios_fhir::FhirVersion;
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use helios_persistence::core::cluster_job_store::testing::InMemoryClusterJobStore;
use helios_persistence::core::{ResourceStorage, WorkerId};
use helios_rest::endpoint::EndpointRuntime;
use helios_rest::{OperationsBundle, ServerConfig};
use helios_sync::local::tenant_context;
use serde_json::{Value, json};

const VITALS: &str = "http://hl7.org/fhir/StructureDefinition/vitalsigns";
const PROFILE: &str = "http://example.org/StructureDefinition/p-gender";

fn endpoint(name: &str, mode: Option<ValidationMode>) -> EndpointSpec {
    let mut s = EndpointSpec::named(EndpointName::parse(name).unwrap(), name);
    s.validation = mode.map(|mode| ValidationSettings {
        mode,
        terminology: TerminologyValidation::Off,
        stored_profiles: true,
        required_profiles: vec![ProfileRequirement {
            resource_type: "Observation".into(),
            profile: VITALS.into(),
        }],
    });
    s
}

fn gender_profile() -> Value {
    json!({
        "resourceType": "StructureDefinition", "id": "p-gender", "url": PROFILE,
        "name": "RequiresGender", "status": "active", "kind": "resource", "abstract": false,
        "type": "Patient", "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
        "derivation": "constraint", "fhirVersion": "4.0.1",
        "differential": {"element": [
            {"id": "Patient", "path": "Patient"},
            {"id": "Patient.gender", "path": "Patient.gender", "min": 1}
        ]}
    })
}

async fn put(b: &SqliteBackend, tenant: &str, rt: &str, id: &str, mut body: Value) {
    body["resourceType"] = json!(rt);
    body["id"] = json!(id);
    ResourceStorage::create_or_update(b, &tenant_context(tenant), rt, id, body, FhirVersion::R4)
        .await
        .unwrap();
}

async fn present(b: &SqliteBackend, tenant: &str, rt: &str, id: &str) -> bool {
    matches!(
        ResourceStorage::read(b, &tenant_context(tenant), rt, id).await,
        Ok(Some(r)) if !r.is_deleted()
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_writes_follow_target_validation_and_duplicates_validate_at_once() {
    // Endpoints: a permissive source and targets under each mode.
    let store = Arc::new(MemoryEndpointStore::new());
    ensure_default_endpoint(store.as_ref(), "default", FhirRelease::R4)
        .await
        .unwrap();
    let a = Actor::bootstrap();
    for (name, mode) in [
        ("src", None),
        ("enf", Some(ValidationMode::Enforce)),
        ("logep", Some(ValidationMode::Log)),
        ("offep", Some(ValidationMode::Off)),
        ("igsrc", None),
    ] {
        store.create(endpoint(name, mode), &a).await.unwrap();
    }
    let registry = Arc::new(EndpointRegistry::load(store.as_ref()).await.unwrap());

    let data_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("data"));
    let backend = Arc::new(
        SqliteBackend::with_config(":memory:", SqliteBackendConfig { data_dir, ..Default::default() })
            .unwrap(),
    );
    backend.init_schema().unwrap();

    // The sync service, installed before the app (the app installs its
    // write path into it).
    let env = [("HFS_SYNC_ENABLED", "false"), ("HFS_SYNC_SAFETY_LAG_MS", "0")];
    let cfg = helios_sync::SyncConfig::from_lookup(&|k| {
        env.iter().find(|(x, _)| *x == k).map(|(_, v)| v.to_string())
    });
    let (svc, _) = helios_sync::build(
        &cfg,
        helios_sync::SyncDeps {
            store: Arc::new(helios_sync::memory::MemorySyncStore::new()),
            cluster: Arc::new(InMemoryClusterJobStore::new()),
            registry: registry.clone(),
            endpoints: Some(store.clone() as Arc<dyn EndpointStore>),
            local: Some(backend.clone()),
            copier: None,
            key_ring: None,
            observer: None,
            audit: None,
            instance: "validation-test".into(),
        },
    );
    assert!(helios_sync::install_global(svc.clone()));
    let config = ServerConfig {
        base_url: "https://hfs.example/fhir".into(),
        default_tenant: "default".into(),
        ..ServerConfig::for_testing()
    };
    let app = helios_rest::create_app_with_auth_bulk_settings_and_ops(
        Arc::clone(&backend),
        config,
        helios_auth::AuthConfig::default(),
        None,
        None,
        None,
        None,
        None,
        OperationsBundle {
            endpoints: Some(EndpointRuntime::new(
                registry.clone(),
                store.clone() as Arc<dyn EndpointStore>,
            )),
            ..Default::default()
        },
    );
    let server = TestServer::new(app).unwrap();

    // Source data: one Observation that is not a vital-signs Observation.
    put(&backend, "src", "Observation", "o1", json!({"status": "final", "code": {"text": "glucose"}})).await;
    put(&backend, "src", "Patient", "p1", json!({"gender": "female"})).await;

    let worker = WorkerId::new("w");
    let run_job = |target: &'static str| {
        let svc = svc.clone();
        let worker = worker.clone();
        async move {
            let created = svc
                .create(
                    json!({"name": format!("src to {target}"),
                           "source": {"kind": "endpoint", "name": "src"},
                           "target": {"kind": "endpoint", "name": target}}),
                    "t",
                )
                .await
                .unwrap();
            let id = created.job["id"].as_str().unwrap().to_string();
            let run = svc.run_now(&id, "t").await.unwrap();
            while svc.runtime().work_next(&worker).await.unwrap() {}
            svc.run(&run.id).await.unwrap()
        }
    };

    // enforce: rejected, recorded as a failed item with the OperationOutcome.
    let r = run_job("enf").await;
    assert_eq!(r.state, SyncRunState::Succeeded, "{:?}", r.error);
    assert_eq!(r.progress.failed, 1, "{:?}", r.progress);
    assert_eq!(r.progress.created, 1, "{:?}", r.progress);
    assert!(!present(&backend, "enf", "Observation", "o1").await, "rejected under enforce");
    assert!(present(&backend, "enf", "Patient", "p1").await);
    let conflicts = svc.conflicts(&r.id, 10).await.unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].resource_id, "o1");
    assert!(conflicts[0].reason.contains("rejected by target validation"), "{}", conflicts[0].reason);
    assert!(conflicts[0].reason.contains("OperationOutcome"), "{}", conflicts[0].reason);
    let log = svc.log(&r.id, 0, 100).await.unwrap();
    assert!(log.iter().any(|l| l.message.contains("Observation/o1") && l.message.contains("rejected")));

    // log: written, findings recorded in the run log.
    let r = run_job("logep").await;
    assert_eq!((r.progress.created, r.progress.failed), (2, 0), "{:?}", r.progress);
    assert!(present(&backend, "logep", "Observation", "o1").await);
    let log = svc.log(&r.id, 0, 200).await.unwrap();
    assert!(
        log.iter().any(|l| l.message.starts_with("validation (log): Observation/o1")),
        "{log:#?}"
    );

    // off: written, nothing recorded.
    let r = run_job("offep").await;
    assert_eq!((r.progress.created, r.progress.failed), (2, 0), "{:?}", r.progress);
    assert!(present(&backend, "offep", "Observation", "o1").await);
    let log = svc.log(&r.id, 0, 200).await.unwrap();
    assert!(!log.iter().any(|l| l.message.contains("validation (log)")));

    // Duplicate an endpoint holding an IG profile: `$validate` on the copy
    // applies it immediately.
    put(&backend, "igsrc", "StructureDefinition", "p-gender", gender_profile()).await;
    let out = svc
        .duplicate(
            "igsrc",
            serde_json::from_value(json!({"name": "dup", "data": "current"})).unwrap(),
            &Actor::bootstrap(),
        )
        .await
        .unwrap();
    while svc.runtime().work_next(&worker).await.unwrap() {}
    let run = svc.run(out.run_id.as_deref().unwrap()).await.unwrap();
    assert_eq!(run.state, SyncRunState::Succeeded, "{:?}", run.error);
    assert_eq!(
        store.get("dup").await.unwrap().unwrap().spec.status,
        EndpointStatus::Enabled
    );
    let res = server
        .post(&format!("/dup/Patient/$validate?profile={PROFILE}"))
        .json(&json!({"resourceType": "Patient"}))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK, "{}", res.text());
    let outcome: Value = res.json();
    let issues = outcome["issue"].as_array().unwrap();
    assert!(
        issues.iter().any(|i| i["code"] == "required"
            && i["expression"][0].as_str().is_some_and(|e| e.contains("gender"))),
        "copied profile enforced on the duplicate: {outcome}"
    );
    // A conforming Patient passes the same check.
    let res = server
        .post(&format!("/dup/Patient/$validate?profile={PROFILE}"))
        .json(&json!({"resourceType": "Patient", "gender": "male"}))
        .await;
    let outcome: Value = res.json();
    assert!(
        !outcome["issue"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["severity"] == "error"),
        "{outcome}"
    );
}
