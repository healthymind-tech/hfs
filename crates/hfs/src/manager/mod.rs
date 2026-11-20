//! FHIR server manager wiring (`docs/design/fhir-server-manager.md` §15).
//!
//! [`init`] runs once per process, after the primary storage is up and
//! before the app is built. It opens the endpoint store (PostgreSQL when the
//! primary is PostgreSQL, else the memory store fed from `HFS_ENDPOINTS_FILE`),
//! makes sure the default endpoint row exists, and — only with
//! `HFS_MANAGER=on` — builds the [`ManagerContext`] every manager package
//! hooks into: the registry and its watcher/LISTEN tasks (P1, [`endpoints`]),
//! console sign-in (P3, `install_console_auth`), sync workers (P6,
//! `install_sync`). Packages add routes to [`ManagerContext::routes`] (merged
//! into the app by [`attach`]) and background tasks to
//! [`ManagerContext::tasks`].
//!
//! With `HFS_MANAGER=off` (the default) [`init`] returns `None` and the app is
//! built exactly as before; on PostgreSQL the default endpoint row is still
//! written (rollout step R0), which nothing reads.

pub mod endpoints;

use std::sync::Arc;

use helios_endpoints::{EndpointRegistry, EndpointStore, OutboundPolicy, SecretKeyRing};
use helios_rest::ServerConfig;
use helios_rest::endpoint::EndpointRuntime;
use tracing::{info, warn};

/// `HFS_MANAGER=off|on` (design D-18).
pub const MANAGER_ENV: &str = "HFS_MANAGER";

/// `HFS_MANAGER_ALLOW_URL_TENANTS=true` lets the manager start with URL-path
/// tenant routing (`url_path` / `both`), which otherwise is refused: implicit
/// URL tenants would sit next to named endpoints (design §4.4).
pub const ALLOW_URL_TENANTS_ENV: &str = "HFS_MANAGER_ALLOW_URL_TENANTS";

/// Whether `HFS_MANAGER` switches the manager on.
pub fn manager_enabled() -> bool {
    parse_switch(std::env::var(MANAGER_ENV).ok().as_deref())
}

/// `on`/`true`/`1`/`yes` (any case) → on; anything else → off.
pub fn parse_switch(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "on" | "true" | "1" | "yes"
        )
    })
}

/// The PostgreSQL primary, for the shared endpoint store and its LISTEN
/// session.
#[cfg(feature = "postgres")]
pub struct PgHandle {
    /// The backend's shared pool.
    pub pool: helios_endpoints::pg::deadpool_postgres::Pool,
    /// A dedicated-connection config for `LISTEN hfs_config`.
    pub listen: helios_endpoints::pg::tokio_postgres::Config,
}

#[cfg(feature = "postgres")]
impl PgHandle {
    /// From a PostgreSQL backend (pool + its retained connection settings).
    pub fn from_backend(backend: &helios_persistence::backends::postgres::PostgresBackend) -> Self {
        let c = backend.config();
        let mut listen = helios_endpoints::pg::tokio_postgres::Config::new();
        listen
            .host(&c.host)
            .port(c.port)
            .dbname(&c.dbname)
            .user(&c.user)
            .application_name("hfs-endpoint-config");
        if let Some(password) = &c.password {
            listen.password(password);
        }
        Self {
            pool: backend.shared_pool(),
            listen,
        }
    }
}

/// Placeholder when the binary has no PostgreSQL support.
#[cfg(not(feature = "postgres"))]
pub struct PgHandle;

/// Everything the manager packages share (design §15, P1 "Provides").
/// Some fields are read only by later packages' hooks (P3 console auth, P4
/// admin API, P6 sync).
#[allow(dead_code)]
pub struct ManagerContext {
    /// Server configuration.
    pub config: ServerConfig,
    /// The PostgreSQL primary's shared pool, when the primary is PostgreSQL.
    #[cfg(feature = "postgres")]
    pub pool: Option<helios_endpoints::pg::deadpool_postgres::Pool>,
    /// The endpoint registry.
    pub registry: Arc<EndpointRegistry>,
    /// The endpoint store.
    pub store: Arc<dyn EndpointStore>,
    /// `HFS_SECRET_KEYS`; `None` = storing secrets is refused (D-12).
    pub key_ring: Option<Arc<SecretKeyRing>>,
    /// `HFS_OUTBOUND_ALLOW` SSRF policy (D-13).
    pub outbound: Arc<OutboundPolicy>,
    /// Routers merged into the app by [`attach`].
    pub routes: Vec<axum::Router>,
    /// Where the app publishes the endpoint admin service (P4) it builds;
    /// the console (P5) writes through the same state.
    pub admin_api: helios_rest::endpoint::AdminApiSlot,
    /// Background tasks (aborted when the context is dropped at shutdown).
    pub tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl ManagerContext {
    /// The runtime the REST app builder installs the dispatcher from.
    pub fn runtime(&self) -> EndpointRuntime {
        EndpointRuntime::new(Arc::clone(&self.registry), Arc::clone(&self.store))
            .with_admin_api_slot(Arc::clone(&self.admin_api))
    }
}

impl Drop for ManagerContext {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Opens the endpoint store and, with `HFS_MANAGER=on`, the manager context.
pub async fn init(
    config: &ServerConfig,
    pg: Option<PgHandle>,
) -> anyhow::Result<Option<ManagerContext>> {
    init_with(config, pg, manager_enabled(), |name| {
        std::env::var(name).ok()
    })
    .await
}

/// [`init`] with the switch and the environment injected (tests).
pub async fn init_with(
    config: &ServerConfig,
    pg: Option<PgHandle>,
    enabled: bool,
    env: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<Option<ManagerContext>> {
    #[cfg(feature = "postgres")]
    let (store, pool, listen): (Arc<dyn EndpointStore>, _, _) = match pg {
        Some(pg) => (
            Arc::new(helios_endpoints::pg::PgEndpointStore::new(pg.pool.clone())),
            Some(pg.pool),
            Some(pg.listen),
        ),
        None => (
            Arc::new(helios_endpoints::MemoryEndpointStore::new()),
            None,
            None,
        ),
    };
    #[cfg(not(feature = "postgres"))]
    let (store, listen): (Arc<dyn EndpointStore>, endpoints::Listen) = {
        let _ = pg;
        (Arc::new(helios_endpoints::MemoryEndpointStore::new()), None)
    };
    #[cfg(feature = "postgres")]
    let persistent = pool.is_some();
    #[cfg(not(feature = "postgres"))]
    let persistent = false;

    if !enabled {
        // Rollout R0: the schema and the default endpoint row exist before
        // anyone switches the manager on. Best effort — nothing reads it yet.
        if persistent {
            if let Err(e) = endpoints::ensure_default(store.as_ref(), config).await {
                warn!(error = %e, "could not write the default endpoint row (HFS_MANAGER is off; ignored)");
            }
        }
        return Ok(None);
    }

    if config.multitenancy.routing_mode.supports_url_path()
        && !parse_switch(env(ALLOW_URL_TENANTS_ENV).as_deref())
    {
        anyhow::bail!(
            "HFS_MANAGER=on cannot be combined with HFS_TENANT_ROUTING_MODE=url_path/both: \
             implicit URL tenants would sit next to named endpoints. Use header_only, or set \
             {ALLOW_URL_TENANTS_ENV}=true to accept it"
        );
    }

    let key_ring = SecretKeyRing::from_lookup(&env)
        .map_err(|e| anyhow::anyhow!("HFS_SECRET_KEYS is invalid: {e}"))?
        .map(Arc::new);
    if key_ring.is_none() {
        info!("HFS_SECRET_KEYS is not set: endpoint secrets cannot be stored");
    }
    let outbound = Arc::new(OutboundPolicy::parse(
        &env(helios_endpoints::outbound::ALLOW_ENV).unwrap_or_default(),
    ));

    let registry = endpoints::open_registry(store.as_ref(), config, &env).await?;
    let mut ctx = ManagerContext {
        config: config.clone(),
        #[cfg(feature = "postgres")]
        pool,
        registry,
        store,
        key_ring,
        outbound,
        routes: Vec::new(),
        admin_api: Default::default(),
        tasks: Vec::new(),
    };

    // Package hooks, in integration order (P1 → P6). Each package adds one
    // `install_*` line here.
    endpoints::install(&mut ctx, listen, persistent);

    info!(
        endpoints = ctx.registry.snapshot().endpoints().len(),
        epoch = ctx.registry.snapshot().epoch(),
        "FHIR server manager is ON"
    );
    Ok(Some(ctx))
}

/// The ops bundle with the manager runtime attached (unchanged when off).
pub fn with_endpoints(
    mut ops: helios_rest::OperationsBundle,
    manager: Option<&ManagerContext>,
) -> helios_rest::OperationsBundle {
    ops.endpoints = manager.map(ManagerContext::runtime);
    ops
}

/// Merges the manager's routes into `app` and keeps the context (its tasks)
/// alive for the life of the process.
pub fn attach(manager: Option<ManagerContext>, app: axum::Router) -> axum::Router {
    let Some(mut ctx) = manager else {
        return app;
    };
    let app = std::mem::take(&mut ctx.routes)
        .into_iter()
        .fold(app, |app, routes| app.merge(routes));
    // The tasks run until the process exits.
    std::mem::forget(ctx);
    app
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_spellings() {
        for on in ["on", "ON", "true", "1", "yes", " on "] {
            assert!(parse_switch(Some(on)), "{on}");
        }
        for off in ["off", "false", "0", "", "no", "maybe"] {
            assert!(!parse_switch(Some(off)), "{off}");
        }
        assert!(!parse_switch(None));
    }

    #[tokio::test]
    async fn off_builds_nothing_on_memory() {
        let cfg = ServerConfig::for_testing();
        assert!(
            init_with(&cfg, None, false, |_| None)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn on_builds_a_context_with_the_default_endpoint() {
        let cfg = ServerConfig::for_testing();
        let ctx = init_with(&cfg, None, true, |_| None)
            .await
            .unwrap()
            .unwrap();
        let snap = ctx.registry.snapshot();
        assert!(snap.default_endpoint().is_some());
        assert!(ctx.key_ring.is_none());
        let ops = with_endpoints(helios_rest::OperationsBundle::default(), Some(&ctx));
        assert!(ops.endpoints.is_some());
        let app = attach(Some(ctx), axum::Router::new());
        drop(app);
    }

    #[tokio::test]
    async fn on_refuses_url_tenants_unless_allowed() {
        let mut cfg = ServerConfig::for_testing();
        cfg.multitenancy.routing_mode = helios_rest::config::TenantRoutingMode::UrlPath;
        assert!(init_with(&cfg, None, true, |_| None).await.is_err());
        let ok = init_with(&cfg, None, true, |k| {
            (k == ALLOW_URL_TENANTS_ENV).then(|| "true".to_string())
        })
        .await
        .unwrap();
        assert!(ok.is_some());
    }

    #[tokio::test]
    async fn bad_key_ring_fails_startup() {
        let cfg = ServerConfig::for_testing();
        let r = init_with(&cfg, None, true, |k| {
            (k == helios_endpoints::secret::KEYS_ENV).then(|| "garbage".to_string())
        })
        .await;
        assert!(r.is_err());
    }
}
