//! Endpoint runtime wiring (package P1): default endpoint, `HFS_ENDPOINTS_FILE`
//! reconciliation, registry, watcher and `LISTEN hfs_config` tasks.

use std::sync::Arc;
use std::time::Duration;

use helios_endpoints::{
    Actor, Endpoint, EndpointRegistry, EndpointSpec, EndpointStatus, EndpointStore, ManagedBy,
    StoreError, ensure_default_endpoint,
};
use helios_rest::ServerConfig;
use tracing::{info, warn};

use super::ManagerContext;

/// `HFS_ENDPOINTS_FILE`: a JSON array of `EndpointSpec` declaring
/// environment-managed endpoints (read-only in the API and console). The only
/// way to declare endpoints on non-PostgreSQL deployments.
pub const ENDPOINTS_FILE_ENV: &str = "HFS_ENDPOINTS_FILE";

/// Dedicated-connection config for `LISTEN` (PostgreSQL only).
#[cfg(feature = "postgres")]
pub type Listen = Option<helios_endpoints::pg::tokio_postgres::Config>;
/// Dedicated-connection config for `LISTEN` (PostgreSQL only).
#[cfg(not(feature = "postgres"))]
pub type Listen = Option<()>;

/// Creates the default endpoint (root path, the server's default tenant and
/// FHIR version, every section inherited) unless it exists.
pub async fn ensure_default(
    store: &dyn EndpointStore,
    config: &ServerConfig,
) -> Result<Endpoint, StoreError> {
    ensure_default_endpoint(
        store,
        &config.default_tenant,
        helios_rest::endpoint::release_of(config.default_fhir_version),
    )
    .await
}

/// Ensures the default endpoint, applies `HFS_ENDPOINTS_FILE`, and loads the
/// registry.
pub async fn open_registry(
    store: &dyn EndpointStore,
    config: &ServerConfig,
    env: &impl Fn(&str) -> Option<String>,
) -> anyhow::Result<Arc<EndpointRegistry>> {
    ensure_default(store, config)
        .await
        .map_err(|e| anyhow::anyhow!("cannot create the default endpoint: {e}"))?;
    if let Some(path) = env(ENDPOINTS_FILE_ENV).filter(|p| !p.trim().is_empty()) {
        let text = std::fs::read_to_string(path.trim())
            .map_err(|e| anyhow::anyhow!("{ENDPOINTS_FILE_ENV}={path}: {e}"))?;
        let specs = parse_endpoints_file(&text)
            .map_err(|e| anyhow::anyhow!("{ENDPOINTS_FILE_ENV}={path}: {e}"))?;
        let report = reconcile(store, specs).await?;
        info!(
            created = report.created,
            updated = report.updated,
            unchanged = report.unchanged,
            disabled = report.disabled,
            file = %path,
            "{ENDPOINTS_FILE_ENV} applied"
        );
    }
    let registry = EndpointRegistry::load(store)
        .await
        .map_err(|e| anyhow::anyhow!("cannot load the endpoint configuration: {e}"))?;
    Ok(Arc::new(registry))
}

/// Parses `HFS_ENDPOINTS_FILE` content. Every entry becomes `managedBy=env`.
pub fn parse_endpoints_file(text: &str) -> anyhow::Result<Vec<EndpointSpec>> {
    let mut specs: Vec<EndpointSpec> = serde_json::from_str(text)
        .map_err(|e| anyhow::anyhow!("not a JSON array of endpoint specs: {e}"))?;
    let mut seen = std::collections::HashSet::new();
    for spec in &mut specs {
        spec.managed_by = ManagedBy::Env;
        if !seen.insert(spec.name.as_str().to_string()) {
            anyhow::bail!("endpoint {} is declared twice", spec.name);
        }
        spec.validate()
            .map_err(|e| anyhow::anyhow!("endpoint {}: {e}", spec.name))?;
    }
    Ok(specs)
}

/// What [`reconcile`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// New endpoints.
    pub created: usize,
    /// Endpoints whose stored spec differed from the file.
    pub updated: usize,
    /// Endpoints already as declared.
    pub unchanged: usize,
    /// Environment-managed endpoints no longer in the file, now disabled.
    pub disabled: usize,
}

/// Makes the store match the file: creates or updates every declared
/// endpoint (as the bootstrap actor), and disables environment-managed
/// endpoints the file no longer declares (their data and config are kept).
pub async fn reconcile(
    store: &dyn EndpointStore,
    specs: Vec<EndpointSpec>,
) -> anyhow::Result<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let actor = Actor::bootstrap();
    let declared: std::collections::HashSet<String> =
        specs.iter().map(|s| s.name.as_str().to_string()).collect();
    for spec in specs {
        let name = spec.name.as_str().to_string();
        match store.get(&name).await? {
            Some(current) if current.spec == spec => report.unchanged += 1,
            Some(current) => {
                store
                    .update(&name, current.revision, spec, &actor)
                    .await
                    .map_err(|e| anyhow::anyhow!("endpoint {name}: {e}"))?;
                report.updated += 1;
            }
            None => {
                store
                    .create(spec, &actor)
                    .await
                    .map_err(|e| anyhow::anyhow!("endpoint {name}: {e}"))?;
                report.created += 1;
            }
        }
    }
    for e in store.list(false).await? {
        if e.spec.managed_by == ManagedBy::Env
            && !declared.contains(e.name())
            && e.spec.status == EndpointStatus::Enabled
            && !e.spec.is_default()
        {
            store
                .set_status(e.name(), e.revision, EndpointStatus::Disabled, &actor)
                .await
                .map_err(|err| anyhow::anyhow!("endpoint {}: {err}", e.name()))?;
            warn!(
                endpoint = e.name(),
                "environment-managed endpoint is no longer in {ENDPOINTS_FILE_ENV}; disabled"
            );
            report.disabled += 1;
        }
    }
    Ok(report)
}

/// How often the registry polls the store epoch: `HFS_CLUSTER_SYNC_INTERVAL_MS`
/// (`0` → 1 s; the poll is one indexed row read).
pub fn poll_interval(config: &ServerConfig) -> Duration {
    match config.cluster_sync_interval_ms {
        0 => Duration::from_secs(1),
        ms => Duration::from_millis(ms),
    }
}

/// P1's hook: keeps the registry in step with the store (epoch poll, plus
/// `LISTEN hfs_config` on PostgreSQL for low-latency cross-instance reloads).
pub fn install(ctx: &mut ManagerContext, listen: Listen, persistent: bool) {
    let interval = poll_interval(&ctx.config);
    ctx.tasks.push(tokio::spawn(helios_endpoints::run_watcher(
        Arc::clone(&ctx.registry),
        Arc::clone(&ctx.store),
        interval,
    )));
    #[cfg(feature = "postgres")]
    if let Some(listen) = listen {
        ctx.tasks
            .push(tokio::spawn(helios_endpoints::pg::run_config_listener(
                listen,
                Arc::clone(&ctx.registry),
            )));
    }
    #[cfg(not(feature = "postgres"))]
    let _ = listen;
    info!(
        interval_ms = interval.as_millis() as u64,
        shared = persistent,
        "endpoint registry watcher started"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_endpoints::MemoryEndpointStore;

    const FILE: &str = r#"[
        {"name": "lab", "path": "lab", "displayName": "Lab",
         "storage": {"kind": "tenant", "tenantId": "lab"}},
        {"name": "twcore", "path": "tw", "displayName": "TW Core",
         "storage": {"kind": "tenant", "tenantId": "twcore"},
         "validation": {"mode": "enforce"}}
    ]"#;

    #[test]
    fn file_parsing() {
        let specs = parse_endpoints_file(FILE).unwrap();
        assert_eq!(specs.len(), 2);
        assert!(specs.iter().all(|s| s.managed_by == ManagedBy::Env));
        assert!(parse_endpoints_file("{}").is_err());
        let dup = r#"[{"name":"a","path":"a","displayName":"A","storage":{"kind":"tenant","tenantId":"a"}},
                      {"name":"a","path":"b","displayName":"B","storage":{"kind":"tenant","tenantId":"b"}}]"#;
        assert!(
            parse_endpoints_file(dup)
                .unwrap_err()
                .to_string()
                .contains("twice")
        );
        let bad = r#"[{"name":"a","path":"","displayName":"A","storage":{"kind":"tenant","tenantId":"a"}}]"#;
        assert!(parse_endpoints_file(bad).is_err());
    }

    #[tokio::test]
    async fn reconcile_creates_updates_and_disables() {
        let store = MemoryEndpointStore::new();
        let config = ServerConfig::for_testing();
        ensure_default(&store, &config).await.unwrap();
        let specs = parse_endpoints_file(FILE).unwrap();
        let r = reconcile(&store, specs.clone()).await.unwrap();
        assert_eq!(
            r,
            ReconcileReport {
                created: 2,
                ..Default::default()
            }
        );
        let r = reconcile(&store, specs.clone()).await.unwrap();
        assert_eq!(
            r,
            ReconcileReport {
                unchanged: 2,
                ..Default::default()
            }
        );
        let mut changed = specs.clone();
        changed[0].display_name = "Laboratory".into();
        changed.pop();
        let r = reconcile(&store, changed).await.unwrap();
        assert_eq!(
            r,
            ReconcileReport {
                updated: 1,
                disabled: 1,
                ..Default::default()
            }
        );
        assert_eq!(
            store.get("twcore").await.unwrap().unwrap().spec.status,
            EndpointStatus::Disabled
        );
        assert_eq!(
            store.get("lab").await.unwrap().unwrap().spec.display_name,
            "Laboratory"
        );
    }

    #[tokio::test]
    async fn open_registry_reads_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("endpoints.json");
        std::fs::write(&file, FILE).unwrap();
        let store = MemoryEndpointStore::new();
        let config = ServerConfig::for_testing();
        let f = file.to_string_lossy().to_string();
        let reg = open_registry(&store, &config, &|k: &str| {
            (k == ENDPOINTS_FILE_ENV).then(|| f.clone())
        })
        .await
        .unwrap();
        let snap = reg.snapshot();
        assert_eq!(snap.endpoints().len(), 3);
        assert_eq!(snap.by_name("twcore").unwrap().spec.path.as_str(), "tw");
        let missing = open_registry(&store, &config, &|k: &str| {
            (k == ENDPOINTS_FILE_ENV).then(|| "/nonexistent/endpoints.json".to_string())
        })
        .await;
        assert!(missing.is_err());
    }

    #[test]
    fn poll_interval_defaults() {
        let mut c = ServerConfig::for_testing();
        c.cluster_sync_interval_ms = 0;
        assert_eq!(poll_interval(&c), Duration::from_secs(1));
        c.cluster_sync_interval_ms = 250;
        assert_eq!(poll_interval(&c), Duration::from_millis(250));
    }
}
