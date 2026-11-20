//! # helios-sync — sync and duplicate for the FHIR server manager
//!
//! Package P6 of `docs/design/fhir-server-manager.md` (§9): one-way sync
//! jobs between endpoints of this server and external FHIR servers, and
//! endpoint duplication.
//!
//! | Module | Role |
//! |---|---|
//! | [`cursor`] | run planning, high-water mark + boundary checkpoint arithmetic |
//! | [`conflict`] | per-item decision: write / skip unchanged / conflict |
//! | [`hash`] | canonical content hash for idempotent writes |
//! | [`secrets`] | write-only credentials, sealed under `HFS_SECRET_KEYS` |
//! | [`http`] | SSRF-guarded, connection-pinned HTTP; FHIR client with auth, pacing, retries |
//! | [`local`], [`external`] | source readers / target writers |
//! | [`engine`] | executes one run: initial load + incremental passes |
//! | [`runtime`] | `cluster_jobs` workers (lease, heartbeat, fencing) and scheduler |
//! | [`service`] | the admin API / console logic, duplication |
//! | [`memory`] | in-memory store + the store conformance suite |
//!
//! The PostgreSQL store is `helios_endpoints::pg_sync::PgSyncStore`
//! (feature `postgres`); the PostgreSQL tenant copy is
//! `PostgresBackend::copy_tenant`.

pub mod change;
pub mod conflict;
pub mod copier;
pub mod cursor;
pub mod engine;
pub mod error;
pub mod external;
pub mod hash;
pub mod http;
pub mod local;
pub mod memory;
pub mod peer;
pub mod runtime;
pub mod secrets;
pub mod service;
pub mod throttle;

use std::sync::Arc;
use std::time::Duration;

use helios_endpoints::outbound::OutboundPolicy;
use helios_endpoints::registry::EndpointRegistry;
use helios_endpoints::secret::SecretKeyRing;
use helios_endpoints::store::EndpointStore;
use helios_endpoints::sync::SyncStore;
use helios_persistence::core::{ClusterJobStore, WriteObserver};

pub use engine::{EngineEnv, EngineSettings};
pub use error::SyncError;
pub use runtime::{RuntimeSettings, SyncAuditor, SyncRuntime};
pub use service::{SyncService, global, install_global};

/// Configuration from the environment.
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// `HFS_SYNC_ENABLED` (default on): workers and scheduler run here.
    pub enabled: bool,
    /// Engine knobs (`HFS_SYNC_EXTERNAL`, `HFS_SYNC_SAFETY_LAG_MS`,
    /// `HFS_SYNC_PAGE_SIZE`).
    pub engine: EngineSettings,
    /// Runtime knobs (`HFS_SYNC_WORKERS`, `HFS_SYNC_LEASE_SECS`).
    pub runtime: RuntimeSettings,
    /// Outbound policy (`HFS_OUTBOUND_ALLOW`).
    pub outbound: OutboundPolicy,
    /// Outbound request timeout (`HFS_SYNC_HTTP_TIMEOUT_SECS`).
    pub http_timeout: Duration,
}

fn flag(get: &dyn Fn(&str) -> Option<String>, key: &str, default: bool) -> bool {
    match get(key).map(|v| v.trim().to_ascii_lowercase()) {
        Some(v) if ["0", "false", "off", "no"].contains(&v.as_str()) => false,
        Some(v) if ["1", "true", "on", "yes"].contains(&v.as_str()) => true,
        _ => default,
    }
}

fn num<T: std::str::FromStr>(get: &dyn Fn(&str) -> Option<String>, key: &str) -> Option<T> {
    get(key).and_then(|v| v.trim().parse().ok())
}

impl SyncConfig {
    /// From the process environment.
    pub fn from_env() -> Self {
        Self::from_lookup(&|k| std::env::var(k).ok())
    }

    /// From an injectable lookup (tests).
    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Self {
        let mut engine = EngineSettings {
            external_allowed: flag(get, "HFS_SYNC_EXTERNAL", true),
            ..EngineSettings::default()
        };
        if let Some(ms) = num::<i64>(get, "HFS_SYNC_SAFETY_LAG_MS") {
            engine.safety_lag = chrono::Duration::milliseconds(ms.clamp(0, 600_000));
        }
        if let Some(n) = num::<u32>(get, "HFS_SYNC_PAGE_SIZE") {
            engine.page_size = n.clamp(1, 1000);
        }
        let mut runtime = RuntimeSettings::default();
        if let Some(n) = num::<usize>(get, "HFS_SYNC_WORKERS") {
            runtime.workers = n.clamp(1, 32);
        }
        if let Some(s) = num::<u64>(get, "HFS_SYNC_LEASE_SECS") {
            runtime.lease = Duration::from_secs(s.clamp(15, 3600));
            runtime.heartbeat = runtime.lease / 6;
        }
        Self {
            enabled: flag(get, "HFS_SYNC_ENABLED", true),
            engine,
            runtime,
            outbound: OutboundPolicy::parse(&get(helios_endpoints::outbound::ALLOW_ENV).unwrap_or_default()),
            http_timeout: Duration::from_secs(num::<u64>(get, "HFS_SYNC_HTTP_TIMEOUT_SECS").unwrap_or(60).clamp(5, 600)),
        }
    }
}

/// What the server hands to [`build`].
pub struct SyncDeps {
    /// Job store (PostgreSQL in a cluster, memory otherwise).
    pub store: Arc<dyn SyncStore>,
    /// Shared run queue (`cluster_jobs`; in-memory without a shared DB).
    pub cluster: Arc<dyn ClusterJobStore>,
    /// Endpoint registry.
    pub registry: Arc<EndpointRegistry>,
    /// Endpoint store (duplication).
    pub endpoints: Option<Arc<dyn EndpointStore>>,
    /// In-process storage of local endpoints.
    pub local: Option<Arc<dyn local::LocalStore>>,
    /// PostgreSQL tenant copy.
    pub copier: Option<Arc<dyn copier::TenantCopier>>,
    /// `HFS_SECRET_KEYS`.
    pub key_ring: Option<Arc<SecretKeyRing>>,
    /// The server's write observers.
    pub observer: Option<Arc<dyn WriteObserver>>,
    /// Auditor.
    pub audit: Option<Arc<dyn SyncAuditor>>,
    /// This instance's id (leases).
    pub instance: String,
}

/// Builds the service (and starts workers + scheduler when enabled).
pub fn build(cfg: &SyncConfig, deps: SyncDeps) -> (Arc<SyncService>, Vec<tokio::task::JoinHandle<()>>) {
    let http = Arc::new(http::GuardedHttp::new(
        cfg.outbound.clone(),
        http::HttpSettings {
            timeout: cfg.http_timeout,
            ..Default::default()
        },
    ));
    let env = Arc::new(EngineEnv {
        store: deps.store,
        registry: deps.registry,
        local: deps.local,
        copier: deps.copier,
        http,
        key_ring: deps.key_ring,
        observer: deps.observer,
        settings: cfg.engine.clone(),
        wake: Default::default(),
    });
    let runtime = Arc::new(SyncRuntime::new(
        env,
        deps.cluster,
        deps.instance,
        cfg.runtime.clone(),
        deps.endpoints,
        deps.audit,
    ));
    let handles = if cfg.enabled { runtime.spawn() } else { Vec::new() };
    (Arc::new(SyncService::new(runtime)), handles)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn config_from_env() {
        let env: HashMap<&str, &str> = [
            ("HFS_SYNC_EXTERNAL", "off"),
            ("HFS_SYNC_SAFETY_LAG_MS", "500"),
            ("HFS_SYNC_PAGE_SIZE", "5000"),
            ("HFS_SYNC_WORKERS", "4"),
            ("HFS_SYNC_LEASE_SECS", "120"),
            ("HFS_SYNC_ENABLED", "false"),
            ("HFS_OUTBOUND_ALLOW", "127.0.0.1"),
            ("HFS_SYNC_HTTP_TIMEOUT_SECS", "1"),
        ]
        .into_iter()
        .collect();
        let c = SyncConfig::from_lookup(&|k| env.get(k).map(|v| v.to_string()));
        assert!(!c.enabled && !c.engine.external_allowed);
        assert_eq!(c.engine.safety_lag, chrono::Duration::milliseconds(500));
        assert_eq!(c.engine.page_size, 1000);
        assert_eq!(c.runtime.workers, 4);
        assert_eq!(c.runtime.lease, Duration::from_secs(120));
        assert_eq!(c.runtime.heartbeat, Duration::from_secs(20));
        assert_eq!(c.http_timeout, Duration::from_secs(5));
        assert!(c.outbound.check_url("http://127.0.0.1:1/").is_ok());
        let d = SyncConfig::from_lookup(&|_| None);
        assert!(d.enabled && d.engine.external_allowed);
        assert_eq!(d.runtime.workers, 2);
        assert_eq!(d.http_timeout, Duration::from_secs(60));
    }
}
