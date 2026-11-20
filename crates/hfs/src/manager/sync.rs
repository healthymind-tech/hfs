//! FHIR server manager P6 wiring: builds the sync service (store, run
//! queue, workers, scheduler) and registers it for the admin API
//! (`/admin/sync/**`, `/sync/notify/{job}`) and the console pages
//! (`/ui/sync/**`).
//!
//! Inert unless `HFS_MANAGER=on` (design D-18); workers run unless
//! `HFS_SYNC_ENABLED=false`. On PostgreSQL the jobs live in the shared
//! `hfs_sync_*` tables and runs in `cluster_jobs`, so any instance works
//! any run; on other backends the store and the queue are in memory
//! (single instance).
//!
//! Until P1's `ManagerContext` lands this module brings its own endpoint
//! registry stand-in (an in-memory store holding the default endpoint);
//! [`install_sync`] takes the registry and store as arguments so the
//! `ManagerContext` can pass the real ones.

use std::sync::Arc;

use async_trait::async_trait;
use helios_audit::{AuditAction, AuditEventBuilder, AuditMiddlewareState};
use helios_endpoints::registry::EndpointRegistry;
use helios_endpoints::secret::SecretKeyRing;
use helios_endpoints::store::{EndpointStore, MemoryEndpointStore, ensure_default_endpoint};
use helios_endpoints::sync::SyncStore;
use helios_persistence::core::{ClusterJobStore, WriteObserver};
use helios_rest::ServerConfig;
use helios_sync::copier::TenantCopier;
use helios_sync::local::LocalStore;
use helios_sync::{SyncAuditor, SyncConfig, SyncDeps, SyncService};
use tracing::{info, warn};

/// Whether the manager is switched on (`HFS_MANAGER=on|true|1`).
pub fn manager_enabled() -> bool {
    std::env::var("HFS_MANAGER")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "on" | "true" | "1" | "yes"))
        .unwrap_or(false)
}

/// Sync events as AuditEvents through the server's audit sink.
pub struct AuditSinkAuditor {
    state: Arc<AuditMiddlewareState>,
}

impl AuditSinkAuditor {
    /// Over the server's audit pipeline.
    pub fn new(state: Arc<AuditMiddlewareState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl SyncAuditor for AuditSinkAuditor {
    async fn record(&self, action: &str, job_id: &str, job_name: &str, actor: &str, success: bool, description: &str) {
        let audit_action = match action {
            "create" => AuditAction::Create,
            "update" => AuditAction::Update,
            "delete" => AuditAction::Delete,
            _ => AuditAction::Execute,
        };
        let event = AuditEventBuilder::new(self.state.config.source_observer.clone())
            .action(audit_action)
            .outcome(if success { "0" } else { "8" })
            .outcome_desc(description.to_string())
            .event_type("http://dicom.nema.org/resources/ontology/DCM", "110100")
            .detail("sync-action", action.to_string())
            .detail("sync-job", format!("{job_name} ({job_id})"))
            .agent(actor, None, actor != "Device/hfs-sync")
            .agent("Device/hfs-sync", Some("HFS sync".into()), false)
            .build();
        self.state.sink.record(event).await;
    }
}

/// What the server provides.
pub struct SyncInstall {
    /// Local endpoints' storage.
    pub local: Arc<dyn LocalStore>,
    /// Shared run queue, when the backend has one.
    pub cluster: Option<Arc<dyn ClusterJobStore>>,
    /// PostgreSQL job store.
    pub pg_store: Option<Arc<dyn SyncStore>>,
    /// PostgreSQL tenant copy.
    pub copier: Option<Arc<dyn TenantCopier>>,
    /// Write observers of the server.
    pub observer: Option<Arc<dyn WriteObserver>>,
    /// Audit pipeline.
    pub audit: Option<Arc<AuditMiddlewareState>>,
    /// Endpoint registry + store (P1); `None` = the stand-in.
    pub endpoints: Option<(Arc<EndpointRegistry>, Arc<dyn EndpointStore>)>,
}

/// The endpoint registry stand-in: the default endpoint only.
pub async fn standalone_endpoints(config: &ServerConfig) -> anyhow::Result<(Arc<EndpointRegistry>, Arc<dyn EndpointStore>)> {
    let store: Arc<dyn EndpointStore> = Arc::new(MemoryEndpointStore::new());
    let release = match format!("{:?}", config.default_fhir_version).as_str() {
        "R4B" => helios_endpoints::FhirRelease::R4B,
        "R5" => helios_endpoints::FhirRelease::R5,
        "R6" => helios_endpoints::FhirRelease::R6,
        _ => helios_endpoints::FhirRelease::R4,
    };
    ensure_default_endpoint(store.as_ref(), &config.default_tenant, release).await?;
    let registry = Arc::new(EndpointRegistry::load(store.as_ref()).await?);
    Ok((registry, store))
}

/// Builds, starts and registers the sync service. `Ok(None)` when the
/// manager is off.
pub async fn install_sync(config: &ServerConfig, i: SyncInstall) -> anyhow::Result<Option<Arc<SyncService>>> {
    if !manager_enabled() {
        return Ok(None);
    }
    let cfg = SyncConfig::from_env();
    let key_ring = SecretKeyRing::from_env()?.map(Arc::new);
    if key_ring.is_none() {
        warn!("HFS_SECRET_KEYS is not set: sync jobs with credentials cannot be created");
    }
    let (registry, endpoints) = match i.endpoints {
        Some(e) => e,
        None => standalone_endpoints(config).await?,
    };
    let shared = i.pg_store.is_some() && i.cluster.is_some();
    let store: Arc<dyn SyncStore> = i
        .pg_store
        .unwrap_or_else(|| Arc::new(helios_sync::memory::MemorySyncStore::new()));
    let cluster: Arc<dyn ClusterJobStore> = i.cluster.unwrap_or_else(|| {
        Arc::new(helios_persistence::core::cluster_job_store::testing::InMemoryClusterJobStore::new())
    });
    let (service, _handles) = helios_sync::build(
        &cfg,
        SyncDeps {
            store,
            cluster,
            registry,
            endpoints: Some(endpoints),
            local: Some(i.local),
            copier: i.copier,
            key_ring,
            observer: i.observer,
            audit: i
                .audit
                .map(|a| Arc::new(AuditSinkAuditor::new(a)) as Arc<dyn SyncAuditor>),
            instance: helios_rest::cluster_sync::instance_id().to_string(),
        },
    );
    if !helios_sync::install_global(service.clone()) {
        warn!("sync service already installed; keeping the first one");
    }
    info!(
        cluster_shared = shared,
        workers = if cfg.enabled { cfg.runtime.workers } else { 0 },
        external_peers = cfg.engine.external_allowed,
        "FHIR server manager sync installed (/admin/sync, /ui/sync)"
    );
    Ok(Some(service))
}
