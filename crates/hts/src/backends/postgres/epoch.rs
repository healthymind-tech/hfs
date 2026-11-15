//! Cross-instance terminology cache coherence (cluster deployments).
//!
//! Every HTS process memoizes terminology answers — the `AppState` handler
//! caches and this backend's response caches — and drops them only when *it*
//! performs a write. With two or more replicas behind a load balancer (or a
//! `hts import` run against the live database) the other processes would keep
//! serving stale `$expand` / `$validate-code` / `$lookup` / `$translate` /
//! `$subsumes` answers forever.
//!
//! The fix is a single counter, `terminology_epoch` (see the schema): the
//! header-table triggers bump it inside every transaction that writes a
//! CodeSystem / ValueSet / ConceptMap row, and [`PostgresTerminologyBackend::bump_terminology_epoch`]
//! bumps it once more *after* every write this process completes (a bulk
//! import writes concepts after its header row, so the trigger alone could
//! fire before the content is in place). [`spawn_cache_sync`] polls the
//! counter and runs the caller's eviction whenever it moves, then once more a
//! poll later so a reader that re-filled a cache between the bump and the
//! commit becoming visible cannot pin a stale answer.
//!
//! Inspired by upstream's `feat/cluster-capable-state` C3 (`EpochGuard`),
//! which checks the epoch at eight read choke points; this fork has more
//! caches than that list covers, so it evicts *everything* from one poller
//! instead and adds the triggers so out-of-process writers are seen too.

use std::sync::Arc;
use std::time::Duration;

use super::PostgresTerminologyBackend;
use crate::error::HtsError;

impl PostgresTerminologyBackend {
    /// The current value of the shared terminology epoch.
    pub async fn terminology_epoch(&self) -> Result<i64, HtsError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| HtsError::StorageError(format!("pool: {e}")))?;
        let row = client
            .query_opt("SELECT epoch FROM terminology_epoch WHERE id = 1", &[])
            .await
            .map_err(|e| HtsError::StorageError(format!("read terminology_epoch: {e}")))?;
        Ok(row.map(|r| r.get::<_, i64>(0)).unwrap_or(0))
    }

    /// Bumps the shared terminology epoch so every other replica drops its
    /// caches. Best-effort: called after the write has committed, and a
    /// failure only delays other replicas until the next write (it is logged).
    pub async fn bump_terminology_epoch(&self) {
        let result = async {
            let client = self
                .pool
                .get()
                .await
                .map_err(|e| HtsError::StorageError(format!("pool: {e}")))?;
            client
                .execute(
                    "UPDATE terminology_epoch SET epoch = epoch + 1 WHERE id = 1",
                    &[],
                )
                .await
                .map_err(|e| HtsError::StorageError(format!("bump terminology_epoch: {e}")))?;
            Ok::<(), HtsError>(())
        }
        .await;
        if let Err(e) = result {
            tracing::warn!(error = %e, "could not bump the terminology epoch; other replicas keep their caches until the next write");
        }
    }
}

/// Polls the terminology epoch every `interval` and calls `evict` whenever it
/// changed (and once more on the following poll). Returns immediately when
/// `interval` is zero (cache sync disabled) or there is no Tokio runtime.
pub fn spawn_cache_sync(
    backend: PostgresTerminologyBackend,
    interval: Duration,
    evict: Arc<dyn Fn() + Send + Sync>,
) {
    if interval.is_zero() || tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        let mut seen: Option<i64> = None;
        let mut settle = false;
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let epoch = match backend.terminology_epoch().await {
                Ok(epoch) => epoch,
                Err(e) => {
                    tracing::warn!(error = %e, "terminology cache sync: epoch poll failed");
                    continue;
                }
            };
            match seen {
                None => seen = Some(epoch),
                Some(prev) if prev != epoch => {
                    tracing::debug!(from = prev, to = epoch, "terminology epoch moved; evicting caches");
                    evict();
                    seen = Some(epoch);
                    settle = true;
                }
                Some(_) if settle => {
                    evict();
                    settle = false;
                }
                Some(_) => {}
            }
        }
    });
}
