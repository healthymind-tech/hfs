//! In-memory (shared-cache) SQLite: readers must not fail while another
//! connection has a write transaction open.
//!
//! An in-memory backend's pool shares one database through `cache=shared`,
//! where SQLite locks whole tables and a reader that meets a writer's table
//! lock fails at once with SQLITE_LOCKED ("database table is locked"):
//! `busy_timeout` does not cover shared-cache locks. That made reads racing
//! a background writer — the HTS audit trail query against the batching
//! audit writer (`audit_trail::admin_actions_are_recorded`, Windows nightly
//! and Linux) — fail intermittently with 500 "backend unavailable".

use crate::core::{ResourceStorage, Transaction, TransactionOptions, TransactionProvider};
use crate::tenant::{TenantContext, TenantId, TenantPermissions};
use helios_fhir::FhirVersion;
use serde_json::json;

use super::SqliteBackend;

fn tenant() -> TenantContext {
    TenantContext::new(TenantId::new("t"), TenantPermissions::full_access())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_succeed_while_a_write_transaction_is_open_in_memory() {
    let backend = SqliteBackend::in_memory().expect("backend");
    backend.init_schema().expect("schema");
    let t = tenant();
    backend
        .create(
            &t,
            "Patient",
            json!({"resourceType": "Patient", "id": "before"}),
            FhirVersion::default(),
        )
        .await
        .expect("seed");

    // A write transaction holds the table's write lock until it commits.
    let mut tx = backend
        .begin_transaction(&t, TransactionOptions::new())
        .await
        .expect("begin");
    tx.create(
        "Patient",
        json!({"resourceType": "Patient", "id": "inflight"}),
    )
    .await
    .expect("create in the transaction");

    // Reads on other pooled connections, deterministically while the lock
    // is held. Before the fix each of these failed with SQLITE_LOCKED.
    let read = backend.read(&t, "Patient", "before").await;
    assert!(
        read.is_ok(),
        "read during an open write transaction: {read:?}"
    );
    assert!(read.unwrap().is_some());
    let count = backend.count(&t, Some("Patient")).await;
    assert!(
        count.is_ok(),
        "count during an open write transaction: {count:?}"
    );

    Box::new(tx).commit().await.expect("commit");
    assert_eq!(backend.count(&t, Some("Patient")).await.unwrap(), 2);
    assert!(
        backend
            .read(&t, "Patient", "inflight")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rolled_back_write_leaves_nothing_behind_in_memory() {
    let backend = SqliteBackend::in_memory().expect("backend");
    backend.init_schema().expect("schema");
    let t = tenant();
    let mut tx = backend
        .begin_transaction(&t, TransactionOptions::new())
        .await
        .expect("begin");
    tx.create("Patient", json!({"resourceType": "Patient", "id": "gone"}))
        .await
        .expect("create");
    assert!(backend.count(&t, Some("Patient")).await.is_ok());
    Box::new(tx).rollback().await.expect("rollback");
    assert_eq!(backend.count(&t, Some("Patient")).await.unwrap(), 0);
    assert!(backend.read(&t, "Patient", "gone").await.unwrap().is_none());
}
