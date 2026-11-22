//! Unit tests for the cluster profile sync: applying a changed or deleted
//! StructureDefinition from the change feed, and the polling loop's
//! de-duplication, type filtering and error handling.

use super::*;
use async_trait::async_trait;
use helios_fhir::FhirVersion;
use helios_persistence::backends::sqlite::SqliteBackend;
use helios_persistence::core::cluster_change_feed::ResourceChange;
use helios_persistence::error::{BackendError, StorageResult};
use serde_json::{Value, json};

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

fn storage() -> Arc<SqliteBackend> {
    let backend = SqliteBackend::in_memory().expect("sqlite");
    backend.init_schema().expect("schema");
    Arc::new(backend)
}

fn tenant(id: &str) -> TenantContext {
    TenantContext::new(TenantId::new(id), TenantPermissions::full_access())
}

fn sd(id: &str, url: &str) -> Value {
    json!({
        "resourceType": "StructureDefinition",
        "id": id,
        "url": url,
        "name": "ClusterSyncProfile",
        "status": "active",
        "kind": "resource",
        "abstract": false,
        "type": "Patient",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
        "derivation": "constraint",
        "differential": {"element": [
            {"id": "Patient", "path": "Patient"},
            {"id": "Patient.gender", "path": "Patient.gender", "min": 1}
        ]}
    })
}

fn change(tenant: &str, rt: &str, id: &str) -> ResourceChange {
    ResourceChange {
        tenant_id: tenant.into(),
        resource_type: rt.into(),
        resource_id: id.into(),
    }
}

async fn create_sd(storage: &SqliteBackend, t: &str, id: &str, url: &str) {
    storage
        .create(
            &tenant(t),
            "StructureDefinition",
            sd(id, url),
            FhirVersion::default(),
        )
        .await
        .expect("create");
}

/// Whether the service has a profile registered for the row. A probe that
/// removes it: applying a deletion reports a change only when there was one.
fn registered(svc: &ValidationService, t: &str, id: &str) -> bool {
    svc.apply_profile_change(t, id, None)
}

fn sd_change(t: &str, id: &str) -> ResourceChange {
    change(t, "StructureDefinition", id)
}

#[tokio::test]
async fn a_stored_profile_is_applied() {
    let storage = storage();
    let svc = ValidationService::new();
    create_sd(
        &storage,
        "t1",
        "sd-a",
        "http://example.org/StructureDefinition/sync-a",
    )
    .await;
    assert!(apply_profile_change(storage.as_ref(), &svc, &sd_change("t1", "sd-a")).await);
    assert!(registered(&svc, "t1", "sd-a"));
}

#[tokio::test]
async fn a_deleted_profile_is_removed() {
    let storage = storage();
    let svc = ValidationService::new();
    create_sd(
        &storage,
        "t1",
        "sd-d",
        "http://example.org/StructureDefinition/sync-d",
    )
    .await;
    assert!(apply_profile_change(storage.as_ref(), &svc, &sd_change("t1", "sd-d")).await);
    storage
        .delete(&tenant("t1"), "StructureDefinition", "sd-d")
        .await
        .expect("delete");
    // The deleted row reads as gone: applied as a removal, not retried.
    assert!(apply_profile_change(storage.as_ref(), &svc, &sd_change("t1", "sd-d")).await);
    assert!(!registered(&svc, "t1", "sd-d"));
}

#[tokio::test]
async fn a_change_to_a_row_that_never_existed_is_a_harmless_delete() {
    let storage = storage();
    let svc = ValidationService::new();
    assert!(
        apply_profile_change(storage.as_ref(), &svc, &sd_change("t1", "ghost")).await,
        "a missing row is not a read failure (no retry)"
    );
    assert!(!registered(&svc, "t1", "ghost"));
}

#[tokio::test]
async fn the_change_is_read_in_its_own_tenant() {
    let storage = storage();
    let svc = ValidationService::new();
    create_sd(
        &storage,
        "t2",
        "sd-t",
        "http://example.org/StructureDefinition/sync-t",
    )
    .await;
    // A feed row for t1 does not see t2's resource.
    assert!(apply_profile_change(storage.as_ref(), &svc, &sd_change("t1", "sd-t")).await);
    assert!(!registered(&svc, "t1", "sd-t"));
    assert!(!registered(&svc, "t2", "sd-t"));
    assert!(apply_profile_change(storage.as_ref(), &svc, &sd_change("t2", "sd-t")).await);
    assert!(registered(&svc, "t2", "sd-t"));
}

/// A feed that hands out scripted batches, then nothing.
struct ScriptedFeed {
    batches: Mutex<Vec<StorageResult<Vec<ResourceChange>>>>,
    polls: AtomicUsize,
}

impl ScriptedFeed {
    fn new(mut batches: Vec<StorageResult<Vec<ResourceChange>>>) -> Arc<Self> {
        batches.reverse();
        Arc::new(Self {
            batches: Mutex::new(batches),
            polls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl ClusterChangeFeed for ScriptedFeed {
    async fn start(&self) -> StorageResult<ChangeCursor> {
        Ok(ChangeCursor::default())
    }
    async fn poll(&self, _cursor: &mut ChangeCursor) -> StorageResult<Vec<ResourceChange>> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        self.batches
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| Ok(Vec::new()))
    }
    async fn prune(&self, _older_than: Duration) -> StorageResult<u64> {
        Ok(0)
    }
}

async fn eventually(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[tokio::test]
async fn the_sync_loop_applies_profile_changes_and_survives_feed_errors() {
    let storage = storage();
    let svc = Arc::new(ValidationService::new());
    create_sd(
        &storage,
        "t1",
        "sd-loop",
        "http://example.org/StructureDefinition/sync-loop",
    )
    .await;
    let feed = ScriptedFeed::new(vec![
        // A failing poll is logged and retried on the next tick.
        Err(StorageError::Backend(BackendError::Unavailable {
            backend_name: "test".into(),
            message: "down".into(),
        })),
        // Duplicates and non-profile rows: one application, others skipped.
        Ok(vec![
            change("t1", "StructureDefinition", "sd-loop"),
            change("t1", "Patient", "p1"),
            change("t1", "StructureDefinition", "sd-loop"),
        ]),
    ]);
    let task = tokio::spawn(run_profile_sync(
        feed.clone() as Arc<dyn ClusterChangeFeed>,
        ChangeCursor::default(),
        svc.clone(),
        storage.clone(),
        Duration::from_millis(5),
    ));
    let applied = eventually(|| feed.polls.load(Ordering::SeqCst) >= 3).await;
    task.abort();
    assert!(applied, "the loop kept polling after the error");
    assert!(registered(&svc, "t1", "sd-loop"), "the profile was applied");
}
