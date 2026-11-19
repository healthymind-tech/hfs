//! Concurrent writes on a file-backed SQLite store must queue, not fail.
//!
//! Under the benchmark's 16 concurrent importers 0.2-0.5% of transaction
//! bundles came back 500 "rolled back" after waiting 30-50 s in SQLite's busy
//! handler, and concurrent creates fell from 375/s to 108/s. Writers in one
//! process now take turns in a first-come first-served queue before they ask
//! SQLite for the write lock. These tests use a busy timeout far shorter than
//! the total work, so they fail if any writer is still left racing for the
//! lock in the busy handler.

#![cfg(feature = "sqlite")]

use std::sync::Arc;
use std::time::Duration;

use helios_fhir::FhirVersion;
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use helios_persistence::core::{BundleEntry, BundleMethod, BundleProvider, ResourceStorage};
use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
use serde_json::json;

fn tenant() -> TenantContext {
    TenantContext::new(TenantId::new("writers"), TenantPermissions::full_access())
}

fn backend(dir: &tempfile::TempDir) -> Arc<SqliteBackend> {
    let backend = SqliteBackend::with_config(
        dir.path().join("fhir.db"),
        SqliteBackendConfig {
            // Far below the time the whole test spends writing: a writer that
            // waited in the busy handler instead of the queue would give up.
            busy_timeout_ms: 200,
            max_connections: 8,
            ..Default::default()
        },
    )
    .unwrap();
    backend.init_schema().unwrap();
    Arc::new(backend)
}

fn post_patient(family: &str, n: usize) -> BundleEntry {
    BundleEntry {
        method: BundleMethod::Post,
        url: "Patient".to_string(),
        resource: Some(json!({
            "resourceType": "Patient",
            "name": [{"family": family, "given": [format!("g{n}")]}],
            "identifier": [{"system": "http://example.org/mrn", "value": format!("{family}-{n}")}]
        })),
        full_url: Some(format!("urn:uuid:{}", uuid::Uuid::new_v4())),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_transaction_bundles_all_commit() {
    let dir = tempfile::tempdir().unwrap();
    let backend = backend(&dir);
    let tenant = tenant();

    let mut tasks = Vec::new();
    for client in 0..16 {
        let backend = backend.clone();
        let tenant = tenant.clone();
        tasks.push(tokio::spawn(async move {
            for round in 0..4 {
                let entries: Vec<BundleEntry> = (0..80)
                    .map(|n| post_patient(&format!("c{client}r{round}"), n))
                    .collect();
                backend
                    .process_transaction(&tenant, entries, FhirVersion::default())
                    .await
                    .map_err(|e| format!("client {client} round {round}: {e}"))?;
            }
            Ok::<(), String>(())
        }));
    }
    let all = async {
        for task in tasks {
            task.await.unwrap().unwrap();
        }
    };
    tokio::time::timeout(Duration::from_secs(300), all)
        .await
        .expect("concurrent bundles did not finish");
    assert_eq!(
        backend.count(&tenant, Some("Patient")).await.unwrap(),
        16 * 4 * 80
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_creates_and_updates_all_succeed() {
    let dir = tempfile::tempdir().unwrap();
    let backend = backend(&dir);
    let tenant = tenant();

    let mut tasks = Vec::new();
    for client in 0..32 {
        let backend = backend.clone();
        let tenant = tenant.clone();
        tasks.push(tokio::spawn(async move {
            for n in 0..40 {
                let created = backend
                    .create(
                        &tenant,
                        "Observation",
                        json!({
                            "resourceType": "Observation",
                            "status": "final",
                            "code": {"text": format!("c{client}-{n}")}
                        }),
                        FhirVersion::default(),
                    )
                    .await
                    .map_err(|e| format!("create {client}/{n}: {e}"))?;
                backend
                    .update(
                        &tenant,
                        &created,
                        json!({
                            "resourceType": "Observation",
                            "status": "amended",
                            "code": {"text": format!("c{client}-{n}")}
                        }),
                    )
                    .await
                    .map_err(|e| format!("update {client}/{n}: {e}"))?;
            }
            Ok::<(), String>(())
        }));
    }
    let all = async {
        for task in tasks {
            task.await.unwrap().unwrap();
        }
    };
    tokio::time::timeout(Duration::from_secs(300), all)
        .await
        .expect("concurrent creates did not finish");
    assert_eq!(
        backend.count(&tenant, Some("Observation")).await.unwrap(),
        32 * 40
    );
}

/// A create is one unit: two creates racing on the same id give one success
/// and one `AlreadyExists`, never a 500 and never a second row.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn racing_creates_of_one_id_conflict_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let backend = backend(&dir);
    let tenant = tenant();

    let mut tasks = Vec::new();
    for _ in 0..16 {
        let backend = backend.clone();
        let tenant = tenant.clone();
        tasks.push(tokio::spawn(async move {
            backend
                .create(
                    &tenant,
                    "Patient",
                    json!({"resourceType": "Patient", "id": "same"}),
                    FhirVersion::default(),
                )
                .await
        }));
    }
    let mut created = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => created += 1,
            Err(helios_persistence::error::StorageError::Resource(
                helios_persistence::error::ResourceError::AlreadyExists { .. },
            )) => {}
            Err(other) => panic!("unexpected error: {other}"),
        }
    }
    assert_eq!(created, 1);
}
