//! Concurrent writes on a file-backed SQLite store.

#![cfg(feature = "sqlite")]

use std::sync::Arc;

use helios_fhir::FhirVersion;
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use helios_persistence::core::ResourceStorage;
use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
use serde_json::json;

fn tenant() -> TenantContext {
    TenantContext::new(TenantId::new("writers"), TenantPermissions::full_access())
}

fn backend(dir: &tempfile::TempDir) -> Arc<SqliteBackend> {
    let backend = SqliteBackend::with_config(
        dir.path().join("fhir.db"),
        SqliteBackendConfig {
            max_connections: 8,
            ..Default::default()
        },
    )
    .unwrap();
    backend.init_schema().unwrap();
    Arc::new(backend)
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
