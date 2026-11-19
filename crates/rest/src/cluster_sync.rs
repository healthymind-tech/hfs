//! Cluster cache coherence for HFS (`HFS_CLUSTER=true`).
//!
//! Each instance compiles stored StructureDefinitions into its own validation
//! profile registry. [`run_profile_sync`] follows the primary database's
//! change feed ([`helios_persistence::core::cluster_change_feed`]) and
//! re-applies every StructureDefinition another instance (or any other
//! writer: an IG install, a bundle, a delete, maintenance SQL) changed, so
//! `$validate` and write-time validation on every instance agree within one
//! sync interval (`HFS_CLUSTER_SYNC_INTERVAL_MS`).

use std::sync::Arc;
use std::time::Duration;

use helios_persistence::core::ResourceStorage;
use helios_persistence::core::cluster_change_feed::{
    ChangeCursor, ClusterChangeFeed, ResourceChange,
};
use helios_persistence::error::{ResourceError, StorageError};
use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
use tracing::{debug, warn};

use crate::validation::ValidationService;

/// This process's name in shared job / lock bookkeeping: `HFS_INSTANCE_ID`,
/// else the host name (the container id under Docker), else a random id.
/// Stable for the life of the process.
pub fn instance_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        std::env::var("HFS_INSTANCE_ID")
            .ok()
            .or_else(|| std::env::var("HOSTNAME").ok())
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| format!("hfs-{}", uuid::Uuid::new_v4()))
    })
}

/// Feed rows older than this are pruned (any instance; idempotent). Long
/// enough that an instance stalled by a slow poll still finds its rows.
const FEED_RETENTION: Duration = Duration::from_secs(3600);
/// Prune roughly every this many polls.
const PRUNE_EVERY: u32 = 600;

/// Polls the change feed forever and applies StructureDefinition changes to
/// `validation`. Errors are logged and retried on the next tick; a change
/// whose row could not be read is retried too.
pub async fn run_profile_sync<S>(
    feed: Arc<dyn ClusterChangeFeed>,
    mut cursor: ChangeCursor,
    validation: Arc<ValidationService>,
    storage: Arc<S>,
    interval: Duration,
) where
    S: ResourceStorage + Send + Sync + ?Sized + 'static,
{
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut retry: Vec<ResourceChange> = Vec::new();
    let mut polls: u32 = 0;
    loop {
        ticker.tick().await;
        polls = polls.wrapping_add(1);
        if polls % PRUNE_EVERY == 0
            && let Err(e) = feed.prune(FEED_RETENTION).await
        {
            debug!(error = %e, "pruning the cluster change feed failed");
        }
        let mut changes = std::mem::take(&mut retry);
        match feed.poll(&mut cursor).await {
            Ok(fresh) => changes.extend(fresh),
            Err(e) => {
                warn!(error = %e, "cluster change feed poll failed; retrying");
            }
        }
        // One application per resource per tick, whatever the number of
        // feed rows (an IG install writes a definition several times).
        changes.sort_by(|a, b| {
            (&a.tenant_id, &a.resource_type, &a.resource_id).cmp(&(
                &b.tenant_id,
                &b.resource_type,
                &b.resource_id,
            ))
        });
        changes.dedup();
        for change in changes {
            if change.resource_type != "StructureDefinition" {
                continue;
            }
            if !apply_profile_change(storage.as_ref(), &validation, &change).await {
                retry.push(change);
            }
        }
    }
}

/// Reads the changed StructureDefinition's current state and applies it.
/// Returns `false` when the read failed (retry later).
pub async fn apply_profile_change<S>(
    storage: &S,
    validation: &ValidationService,
    change: &ResourceChange,
) -> bool
where
    S: ResourceStorage + ?Sized,
{
    let tenant = TenantContext::new(
        TenantId::new(&change.tenant_id),
        TenantPermissions::full_access(),
    );
    let current = match storage
        .read(&tenant, &change.resource_type, &change.resource_id)
        .await
    {
        Ok(Some(row)) if !row.is_deleted() => Some(row),
        Ok(_) => None,
        Err(StorageError::Resource(ResourceError::Gone { .. }))
        | Err(StorageError::Resource(ResourceError::NotFound { .. })) => None,
        Err(e) => {
            warn!(
                tenant = %change.tenant_id,
                id = %change.resource_id,
                error = %e,
                "reading a changed StructureDefinition failed; retrying"
            );
            return false;
        }
    };
    let changed = match &current {
        Some(row) => validation.apply_profile_change(
            &change.tenant_id,
            &change.resource_id,
            Some((row.fhir_version(), &row.content_with_meta())),
        ),
        None => validation.apply_profile_change(&change.tenant_id, &change.resource_id, None),
    };
    debug!(
        tenant = %change.tenant_id,
        id = %change.resource_id,
        deleted = current.is_none(),
        changed,
        "applied a StructureDefinition change from the cluster feed"
    );
    true
}
