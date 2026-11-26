//! Verified fact: "every `search_index` row of this `(tenant, resource type)`
//! belongs to a live resource".
//!
//! `search_count` can answer a lone membership test from `search_index` alone
//! (~15 ms for 17,837 matches) instead of probing `resources` once per match
//! for `is_deleted` (~130 ms). The two are the same number exactly when the
//! index holds no *orphan* rows — rows whose resource is deleted or absent.
//! That shortcut shipped in the fork (`72621af6d`) and was switched off at the
//! v0.2.9 merge because nothing guaranteed it. This module is the guarantee.
//!
//! # What keeps the index orphan-free
//!
//! * **In the database** (fork migration `hm-0040-search-index-liveness`): row
//!   triggers on `resources` delete a resource's `search_index` rows when the
//!   resource row is deleted or its `is_deleted` turns true. Every delete path
//!   — CRUD, bundles, purge, retention, tenant purge, raw maintenance SQL —
//!   passes through them, in the deleting transaction.
//! * **In the writers**: index rows are written in the transaction that
//!   inserts or updates the resource row, or (reindex, deferred bulk indexing)
//!   for resources re-read as live under the resource's advisory key, which
//!   every delete path of the server also takes.
//!
//! # What is verified, and when
//!
//! Neither rule says anything about rows already in the table (older builds,
//! a partially restored dump, hand-written SQL). So the shortcut is used only
//! for a `(tenant, resource type)` that has been *checked*: one anti-join of
//! the type's distinct indexed ids against its live resources. A clean result
//! is recorded in `hfs_search_index_liveness` and trusted from then on by
//! every instance sharing the database; `TRUNCATE resources` clears the table.
//!
//! The check runs in the background the first time a count wants the fact;
//! until it has finished — and whenever it finds orphans, the table is
//! missing, or anything fails — the count keeps the exact `resources` form.
//! The fact can therefore only ever replace a slow exact count with a fast
//! exact one. Orphans are reported, never deleted here: what to do with index
//! rows that have no resource is the operator's decision (see
//! `docs/ops/index-strategy.md`).
//!
//! `HFS_PG_INDEX_COUNT=0|false|off|no` turns the shortcut off entirely.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::PostgresBackend;
use crate::error::{BackendError, StorageError, StorageResult};

/// How long an answer read from the marker table is used before it is read
/// again. A stale *clean* answer keeps being served while it is re-read.
const TTL: Duration = Duration::from_secs(60);
/// A recorded "has orphans" result is re-checked against the data this often,
/// so repairing the rows re-enables the shortcut without a restart.
const RECHECK_ORPHANS_AFTER_SECS: f64 = 600.0;
/// Upper bound on remembered keys; the table is cleared when it is exceeded.
const MAX_KEYS: usize = 8192;

/// Distinct indexed ids of the type that have no live resource.
const ORPHAN_SQL: &str = "SELECT COUNT(*) FROM ( \
       SELECT DISTINCT resource_id FROM search_index \
       WHERE tenant_id = $1 AND resource_type = $2 \
     ) ids \
     WHERE NOT EXISTS ( \
       SELECT 1 FROM resources r \
       WHERE r.tenant_id = $1 AND r.resource_type = $2 \
         AND r.id = ids.resource_id AND r.is_deleted = FALSE)";

const READ_MARKER_SQL: &str = "SELECT orphan_resources, \
            EXTRACT(EPOCH FROM (now() - verified_at))::float8 \
     FROM hfs_search_index_liveness WHERE tenant_id = $1 AND resource_type = $2";

const WRITE_MARKER_SQL: &str = "INSERT INTO hfs_search_index_liveness \
            (tenant_id, resource_type, verified_at, orphan_resources) \
     VALUES ($1, $2, now(), $3) \
     ON CONFLICT (tenant_id, resource_type) \
     DO UPDATE SET verified_at = now(), orphan_resources = EXCLUDED.orphan_resources";

type Key = (String, String);

#[derive(Clone, Copy)]
struct Entry {
    clean: bool,
    at: Instant,
    refreshing: bool,
}

/// Per-backend cache of the marker table. Shared by clones of the backend.
#[derive(Default)]
pub(crate) struct IndexLiveness {
    entries: Mutex<HashMap<Key, Entry>>,
    /// Counts answered from `search_index` alone since this backend started.
    index_only_counts: AtomicU64,
}

/// `HFS_PG_INDEX_COUNT`, read once. Default on.
pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        !matches!(
            std::env::var("HFS_PG_INDEX_COUNT")
                .map(|v| v.trim().to_ascii_lowercase())
                .as_deref(),
            Ok("0" | "false" | "off" | "no")
        )
    })
}

fn internal(message: String) -> StorageError {
    StorageError::Backend(BackendError::Internal {
        backend_name: "postgres".to_string(),
        message,
        source: None,
    })
}

impl IndexLiveness {
    /// The cached answer, and whether the caller should refresh it.
    fn lookup(&self, key: &Key) -> (bool, bool) {
        let Ok(mut entries) = self.entries.lock() else {
            return (false, false);
        };
        match entries.get_mut(key) {
            Some(entry) => {
                let stale = entry.at.elapsed() >= TTL;
                let claim = stale && !entry.refreshing;
                if claim {
                    entry.refreshing = true;
                }
                (entry.clean, claim)
            }
            None => {
                if entries.len() >= MAX_KEYS {
                    entries.clear();
                }
                entries.insert(
                    key.clone(),
                    Entry {
                        clean: false,
                        at: Instant::now(),
                        refreshing: true,
                    },
                );
                (false, true)
            }
        }
    }

    fn store(&self, key: Key, clean: bool) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(
                key,
                Entry {
                    clean,
                    at: Instant::now(),
                    refreshing: false,
                },
            );
        }
    }

    fn forget_tenant(&self, tenant: &str) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.retain(|(t, _), _| t != tenant);
        }
    }

    fn forget(&self, key: &Key) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(key);
        }
    }
}

impl PostgresBackend {
    /// How many counts (`_summary=count`, `_total=accurate`) this backend has
    /// answered from `search_index` alone, without joining `resources`.
    pub fn index_only_count_total(&self) -> u64 {
        self.index_liveness
            .index_only_counts
            .load(Ordering::Relaxed)
    }

    pub(crate) fn note_index_only_count(&self) {
        self.index_liveness
            .index_only_counts
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Drops this process's cached answers for `tenant`, after its markers
    /// were deleted (tenant purge, tenant copy into it). Other instances
    /// re-read the marker table within [`TTL`] and verify again.
    pub(crate) fn forget_search_index_liveness(&self, tenant: &str) {
        self.index_liveness.forget_tenant(tenant);
    }

    /// True only when `(tenant, resource_type)` is known to have no orphan
    /// `search_index` rows. Unknown answers start a background check and read
    /// as false; so do errors and a disabled shortcut.
    pub(crate) fn search_index_is_live_only(&self, tenant: &str, resource_type: &str) -> bool {
        if !enabled() {
            return false;
        }
        let key = (tenant.to_string(), resource_type.to_string());
        let (clean, refresh) = self.index_liveness.lookup(&key);
        if refresh {
            let backend = self.clone();
            tokio::spawn(async move {
                if let Err(error) = backend.refresh_search_index_liveness(&key.0, &key.1).await {
                    // Forget the claim so a later count tries again.
                    backend.index_liveness.forget(&key);
                    tracing::debug!(
                        tenant = %key.0,
                        resource_type = %key.1,
                        %error,
                        "search_index liveness check failed; counts keep the resources join"
                    );
                }
            });
        }
        clean
    }

    /// Reads the recorded result, checking the data when there is none (or
    /// when a recorded "has orphans" is old enough to look again).
    async fn refresh_search_index_liveness(
        &self,
        tenant: &str,
        resource_type: &str,
    ) -> StorageResult<()> {
        let recorded = {
            let client = self.get_client().await?;
            client
                .query_opt(READ_MARKER_SQL, &[&tenant, &resource_type])
                .await
                .map_err(|e| internal(format!("read search_index liveness marker: {e}")))?
                .map(|row| (row.get::<_, i64>(0), row.get::<_, f64>(1)))
        };
        match recorded {
            Some((0, _)) => {
                self.index_liveness
                    .store((tenant.to_string(), resource_type.to_string()), true);
                Ok(())
            }
            Some((_, age)) if age < RECHECK_ORPHANS_AFTER_SECS => {
                self.index_liveness
                    .store((tenant.to_string(), resource_type.to_string()), false);
                Ok(())
            }
            _ => self
                .verify_search_index_liveness(tenant, resource_type)
                .await
                .map(|_| ()),
        }
    }

    /// Checks `(tenant, resource_type)` for orphan `search_index` rows now,
    /// records the result and returns the number of indexed ids without a live
    /// resource. Zero enables the index-only count for the type.
    pub async fn verify_search_index_liveness(
        &self,
        tenant: &str,
        resource_type: &str,
    ) -> StorageResult<u64> {
        let mut client = self.get_client().await?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| internal(format!("search_index liveness begin: {e}")))?;
        // One pass over the type's index rows: minutes on a very large type,
        // which the pool's statement timeout must not cut short.
        tx.batch_execute("SET LOCAL statement_timeout = 0")
            .await
            .map_err(|e| internal(format!("search_index liveness timeout: {e}")))?;
        let orphans: i64 = tx
            .query_one(ORPHAN_SQL, &[&tenant, &resource_type])
            .await
            .map_err(|e| internal(format!("search_index liveness check: {e}")))?
            .get(0);
        tx.execute(WRITE_MARKER_SQL, &[&tenant, &resource_type, &orphans])
            .await
            .map_err(|e| internal(format!("record search_index liveness: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| internal(format!("search_index liveness commit: {e}")))?;
        if orphans > 0 {
            tracing::warn!(
                tenant,
                resource_type,
                orphan_resources = orphans,
                "search_index holds rows for resources that are deleted or absent; \
                 counts for this type keep the slower resources join until they are removed \
                 (docs/ops/index-strategy.md, \"Orphan index rows\")"
            );
        }
        self.index_liveness.store(
            (tenant.to_string(), resource_type.to_string()),
            orphans == 0,
        );
        Ok(orphans as u64)
    }
}
