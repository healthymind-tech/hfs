//! The process-wide, hot-swappable view of the endpoints.
//!
//! One [`EndpointRegistry`] per process (`Arc`), shared by the REST
//! dispatcher, the auth verifier cache, the console and the sync workers.
//! Readers take [`EndpointRegistry::snapshot`] (lock-free `ArcSwap` load) and
//! keep it for the whole request, so a request never sees half of a change.
//!
//! Freshness across a cluster: [`run_watcher`] polls
//! [`EndpointStore::epoch`] every `HFS_CLUSTER_SYNC_INTERVAL_MS` (default
//! 1 s) and reloads when it moved; the PostgreSQL store additionally wakes the
//! watcher early on `NOTIFY hfs_config` (package P1) through
//! [`EndpointRegistry::poke`]. The instance that made a change reloads
//! synchronously before answering the admin API, so the writer always reads
//! its own write.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::sync::{Notify, watch};

use crate::snapshot::{EndpointSnapshot, SnapshotError};
use crate::store::{EndpointStore, StoreError};

/// Why a reload failed (the previous snapshot stays in force).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ReloadError {
    /// Reading the store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The stored endpoints do not form a consistent snapshot.
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
}

/// Hot-swappable endpoint snapshot plus change notification.
pub struct EndpointRegistry {
    current: ArcSwap<EndpointSnapshot>,
    changed: watch::Sender<u64>,
    poke: Notify,
}

impl std::fmt::Debug for EndpointRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointRegistry")
            .field("epoch", &self.current.load().epoch())
            .finish()
    }
}

impl EndpointRegistry {
    /// A registry serving `initial`.
    pub fn new(initial: EndpointSnapshot) -> Self {
        let (changed, _) = watch::channel(initial.epoch());
        Self {
            current: ArcSwap::from_pointee(initial),
            changed,
            poke: Notify::new(),
        }
    }

    /// Loads the store once and builds a registry (startup).
    pub async fn load(store: &dyn EndpointStore) -> Result<Self, ReloadError> {
        let epoch = store.epoch().await?;
        let endpoints = store.list(false).await?;
        Ok(Self::new(EndpointSnapshot::new(epoch, endpoints)?))
    }

    /// The current snapshot.
    pub fn snapshot(&self) -> Arc<EndpointSnapshot> {
        self.current.load_full()
    }

    /// Installs `next` if its epoch is newer than the current one (or
    /// `force`). Returns whether it was installed. Subscribers are notified.
    pub fn install(&self, next: EndpointSnapshot, force: bool) -> bool {
        let cur = self.current.load();
        if !force && next.epoch() <= cur.epoch() {
            return false;
        }
        let epoch = next.epoch();
        self.current.store(Arc::new(next));
        self.changed.send_replace(epoch);
        tracing::info!(epoch, "endpoint configuration reloaded");
        true
    }

    /// Reloads from the store if its epoch moved. Returns whether a new
    /// snapshot was installed.
    pub async fn reload_if_stale(&self, store: &dyn EndpointStore) -> Result<bool, ReloadError> {
        let epoch = store.epoch().await?;
        if epoch == self.current.load().epoch() {
            return Ok(false);
        }
        let endpoints = store.list(false).await?;
        // Install even if the epoch went backwards (store restored from a
        // backup): what the store says is the truth.
        Ok(self.install(EndpointSnapshot::new(epoch, endpoints)?, true))
    }

    /// Subscribes to epoch changes (verifier caches, CapabilityStatement
    /// caches, rate-limit tables rebuild on change).
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// Wakes [`run_watcher`] now (on `NOTIFY hfs_config`).
    pub fn poke(&self) {
        self.poke.notify_one();
    }
}

/// Keeps `registry` in step with `store` forever: every `interval`, or
/// earlier when poked. Errors are logged and retried; the last good snapshot
/// keeps serving.
pub async fn run_watcher(
    registry: Arc<EndpointRegistry>,
    store: Arc<dyn EndpointStore>,
    interval: Duration,
) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = registry.poke.notified() => {}
        }
        if let Err(e) = registry.reload_if_stale(store.as_ref()).await {
            tracing::warn!(error = %e, "endpoint configuration reload failed; keeping the previous snapshot");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EndpointSpec, FhirRelease};
    use crate::name::EndpointName;
    use crate::snapshot::Resolution;
    use crate::store::{Actor, MemoryEndpointStore, ensure_default_endpoint};

    #[tokio::test]
    async fn reload_picks_up_changes() {
        let store = MemoryEndpointStore::new();
        ensure_default_endpoint(&store, "default", FhirRelease::R4)
            .await
            .unwrap();
        let reg = EndpointRegistry::load(&store).await.unwrap();
        let mut rx = reg.subscribe();
        assert!(
            matches!(reg.snapshot().resolve("/lab/Patient"), Resolution::Serve(r) if r.is_default())
        );
        assert!(!reg.reload_if_stale(&store).await.unwrap());

        store
            .create(
                EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab"),
                &Actor::bootstrap(),
            )
            .await
            .unwrap();
        let old = reg.snapshot();
        assert!(reg.reload_if_stale(&store).await.unwrap());
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), store.epoch().await.unwrap());
        // A request holding the old snapshot keeps its view.
        assert!(old.by_name("lab").is_none());
        assert!(
            matches!(reg.snapshot().resolve("/lab/Patient"), Resolution::Serve(r) if r.endpoint.name() == "lab")
        );
        // Older snapshots are not installed without force.
        assert!(!reg.install(EndpointSnapshot::new(0, vec![]).unwrap(), false));
    }

    #[tokio::test(start_paused = true)]
    async fn watcher_reloads_on_poke() {
        let store = Arc::new(MemoryEndpointStore::new());
        ensure_default_endpoint(store.as_ref(), "default", FhirRelease::R4)
            .await
            .unwrap();
        let reg = Arc::new(EndpointRegistry::load(store.as_ref()).await.unwrap());
        let mut rx = reg.subscribe();
        tokio::spawn(run_watcher(
            Arc::clone(&reg),
            store.clone() as Arc<dyn EndpointStore>,
            Duration::from_secs(3600),
        ));
        store
            .create(
                EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab"),
                &Actor::bootstrap(),
            )
            .await
            .unwrap();
        reg.poke();
        rx.changed().await.unwrap();
        assert!(reg.snapshot().by_name("lab").is_some());
    }
}
