//! HTS embedded in another process — the terminology service of `hfs`
//! (design `docs/design/unified-server.md` §3–§4, package U2).
//!
//! [`start`] opens the terminology store, runs the bootstrap sync and the
//! closure rebuild, starts the cross-instance cache sync and returns an
//! [`EmbeddedHts`]:
//!
//! * [`EmbeddedHts::api`] — the terminology FHIR API (operations, CRUD,
//!   search, batch, `/import`, `/health`, `/metadata`) with the write guard,
//!   the audit layer, the body limit, request decompression and the request
//!   timeout. `hfs` nests it at `/tx` (adding compression, CORS, the
//!   anonymous rate limit and the 10 MiB read body limit) and drives the same
//!   router in-process through [`EmbeddedTerminology`];
//! * [`EmbeddedHts::admin`] — the terminology admin API (`HFS_TERMINOLOGY_ADMIN`),
//!   which `hfs` mounts at `/admin/terminology` behind its own admin guard;
//! * [`EmbeddedHts::service`] — the in-process [`TerminologyService`]: no
//!   socket, the same handlers and caches as `/tx`, so remote and embedded
//!   mode answer byte for byte the same; its [`TerminologyService::changes`]
//!   fires on every local write and every cross-instance epoch move.
//!
//! What the standalone `hts` binary adds and the embedded service leaves
//! out: its own `/metrics` (hfs has one), the root redirect, the `/ui/hts`
//! console (U4 mounts the pages inside the hfs console), its own password
//! gate (the hfs administrator gate is passed in) and its own audit queue
//! (hfs passes its sink in).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use helios_terminology::loopback::LoopbackTerminology;
use helios_terminology::{
    ServiceKind, TerminologyHealth, TerminologyService, TxError, TxRequest, TxResponse,
};
use tower_http::decompression::RequestDecompressionLayer;
use tower_http::timeout::TimeoutLayer;

use crate::config::HtsConfig;
use crate::import::BundleImportBackend;
use crate::state::AppState;
use crate::traits::TerminologyBackend;

/// PostgreSQL pool settings of the terminology store
/// (`HFS_TERMINOLOGY_PG_*`; standalone HTS reads `HTS_PG_*`).
#[derive(Debug, Clone, Default)]
pub struct PoolSettings {
    pub size: usize,
    pub wait_timeout_secs: Option<u64>,
    pub connect_timeout_secs: Option<u64>,
    pub statement_timeout_ms: Option<u64>,
}

/// Everything the host process decides for the embedded service.
pub struct EmbeddedOptions {
    /// HTS settings resolved from `HFS_TERMINOLOGY_*` (legacy `HTS_*` folded in).
    pub config: HtsConfig,
    pub pool: PoolSettings,
    /// Public base of the `/tx` API (`HFS_TERMINOLOGY_PUBLIC_URL`), used for
    /// absolute search links.
    pub public_base: Option<String>,
    /// Runtime for imports, bootstrap and admin jobs (design §8.2).
    pub import_runtime: Option<tokio::runtime::Handle>,
    /// The host's audit trail (one sink, one queue, one retention).
    pub audit: Option<crate::audit::HtsAudit>,
    /// The host's administrator gate: its credential (break-glass) is
    /// accepted for `/tx` writes.
    pub write_gate: helios_admin_auth::AdminGate,
    /// SMART/OIDC bearer verification for `/tx` writes.
    pub bearer: Option<Arc<dyn helios_auth::AuthProvider>>,
    /// Default deadline of in-process calls (`HFS_TERMINOLOGY_TIMEOUT_SECS`).
    pub call_timeout: Duration,
    /// Connections of the separate import pool (design §8.2 measure 2;
    /// `HFS_TERMINOLOGY_IMPORT_PG_POOL_SIZE`, default 4). 0: imports share
    /// the serving pool.
    pub import_pool_size: usize,
}

/// The running embedded terminology service.
pub struct EmbeddedHts {
    pub api: Router,
    pub admin: Option<Router>,
    pub service: Arc<EmbeddedTerminology>,
    /// `postgres` or `sqlite`.
    pub storage: &'static str,
}

/// The in-process terminology service: [`LoopbackTerminology`] over the
/// embedded API router, plus the change feed.
pub struct EmbeddedTerminology {
    loopback: LoopbackTerminology<Router>,
    changes: tokio::sync::watch::Receiver<u64>,
}

impl EmbeddedTerminology {
    pub fn new(api: Router, changes: tokio::sync::watch::Receiver<u64>, timeout: Duration) -> Self {
        Self {
            loopback: LoopbackTerminology::new(api, timeout),
            changes,
        }
    }
}

#[async_trait]
impl TerminologyService for EmbeddedTerminology {
    fn kind(&self) -> ServiceKind {
        ServiceKind::Embedded
    }

    async fn call(&self, request: TxRequest) -> Result<TxResponse, TxError> {
        self.loopback.call(request).await
    }

    fn changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        Some(self.changes.clone())
    }

    async fn health(&self) -> TerminologyHealth {
        self.loopback.health().await
    }
}

/// Create `schema` when it does not exist yet, before the terminology store
/// opens with `search_path={schema},public` — otherwise its tables would
/// land in `public` next to HFS's (the one-database plan, design §5).
#[cfg(feature = "postgres")]
pub async fn ensure_schema(database_url: &str, schema: &str) -> anyhow::Result<()> {
    helios_terminology::schema::validate_schema_name(schema).map_err(|e| anyhow::anyhow!("{e}"))?;
    let (client, connection) = tokio_postgres::connect(database_url, tokio_postgres::NoTls)
        .await
        .map_err(|e| anyhow::anyhow!("terminology schema check: {e}"))?;
    let conn = tokio::spawn(connection);
    client
        .batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\""))
        .await
        .map_err(|e| anyhow::anyhow!("CREATE SCHEMA {schema}: {e}"))?;
    drop(client);
    let _ = conn.await;
    Ok(())
}

/// Run `fut` on the import runtime when there is one.
async fn on<F, T>(handle: &Option<tokio::runtime::Handle>, fut: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    match handle {
        Some(h) => h
            .spawn(fut)
            .await
            .map_err(|e| anyhow::anyhow!("terminology startup task: {e}"))?,
        None => fut.await,
    }
}

/// Open the store, sync, and assemble the embedded service.
pub async fn start(options: EmbeddedOptions) -> anyhow::Result<EmbeddedHts> {
    let storage = crate::config::effective_storage_backend(
        &options.config.storage_backend,
        &options.config.database_url,
    );
    if storage == "postgres" {
        #[cfg(feature = "postgres")]
        return start_postgres(options).await;
        #[cfg(not(feature = "postgres"))]
        anyhow::bail!(
            "the terminology store is PostgreSQL but this build has no `postgres` support"
        );
    }
    #[cfg(feature = "sqlite")]
    return start_sqlite(options).await;
    #[cfg(not(feature = "sqlite"))]
    anyhow::bail!("the terminology store is SQLite but this build has no `sqlite` support");
}

#[cfg(feature = "postgres")]
async fn start_postgres(options: EmbeddedOptions) -> anyhow::Result<EmbeddedHts> {
    use crate::backends::PostgresTerminologyBackend;
    use helios_persistence::backends::postgres::PostgresBackend;

    let config = options.config.clone();
    let url = config.database_url.clone();
    let pool = options.pool.clone();
    let backend = PostgresTerminologyBackend::with_pool_settings(
        &url,
        if pool.size == 0 { 16 } else { pool.size },
        pool.wait_timeout_secs,
        pool.connect_timeout_secs,
        pool.statement_timeout_ms,
    )
    .await
    .map_err(|e| anyhow::anyhow!("terminology store: {e}"))?;

    // Bootstrap sync and closure rebuild: import work, on the import runtime.
    {
        let backend = backend.clone();
        let config = config.clone();
        on(&options.import_runtime, async move {
            crate::bootstrap::bootstrap_sync_postgres(&backend, &config).await?;
            let rebuilt = backend
                .rebuild_missing_closures()
                .await
                .map_err(|e| anyhow::anyhow!("post-bootstrap closure rebuild failed: {e}"))?;
            if rebuilt > 0 {
                tracing::info!(
                    systems = rebuilt,
                    "Rebuilt concept closures after bootstrap"
                );
            }
            Ok(())
        })
        .await?;
    }

    let resource_store = PostgresBackend::from_connection_string(&url)
        .await
        .map_err(|e| anyhow::anyhow!("terminology resource store: {e}"))?;
    resource_store
        .init_schema()
        .await
        .map_err(|e| anyhow::anyhow!("terminology resource store schema: {e}"))?;

    {
        use helios_observability::pool_metrics::{PoolGauges, register_postgres_pool};
        let gauges = |s: deadpool_postgres::Status| PoolGauges {
            max_size: s.max_size,
            size: s.size,
            available: s.available,
            waiting: s.waiting,
        };
        let terminology_pool = backend.pool().clone();
        register_postgres_pool("terminology", move || gauges(terminology_pool.status()));
        let resources = resource_store.clone();
        register_postgres_pool("terminology_resources", move || {
            gauges(resources.pool_status())
        });
    }

    let import_backend = if options.import_pool_size > 0 {
        Some(
            PostgresTerminologyBackend::with_pool_settings(
                &url,
                options.import_pool_size,
                pool.wait_timeout_secs,
                pool.connect_timeout_secs,
                // Imports run long statements; never the serving timeout.
                None,
            )
            .await
            .map_err(|e| anyhow::anyhow!("terminology import pool: {e}"))?,
        )
    } else {
        None
    };
    if let Some(ib) = &import_backend {
        use helios_observability::pool_metrics::{PoolGauges, register_postgres_pool};
        let p = ib.pool().clone();
        register_postgres_pool("terminology_import", move || {
            let s = p.status();
            PoolGauges {
                max_size: s.max_size,
                size: s.size,
                available: s.available,
                waiting: s.waiting,
            }
        });
    }

    let (changes_tx, changes_rx) = tokio::sync::watch::channel(0u64);
    let state = AppState::new(backend.clone())
        .with_import_backend(import_backend)
        .with_resource_store_pg(resource_store)
        .with_terminology_importer(Arc::new(backend.clone()))
        .with_max_expansion_size(config.max_expansion_size)
        .with_auto_supplements(config.auto_supplements)
        .with_audit(options.audit.clone())
        .with_public_base(options.public_base.clone())
        .with_changes(Arc::new(changes_tx))
        .with_import_runtime(options.import_runtime.clone());

    {
        use crate::traits::TerminologyCaches;
        let st = state.clone();
        crate::backends::postgres::epoch::spawn_cache_sync(
            backend,
            Duration::from_millis(config.cache_sync_interval_ms),
            Arc::new(move || {
                st.clear_expand_cache();
                st.backend().invalidate_caches();
            }),
        );
    }

    Ok(assemble(&options, state, changes_rx, "postgres"))
}

#[cfg(feature = "sqlite")]
async fn start_sqlite(options: EmbeddedOptions) -> anyhow::Result<EmbeddedHts> {
    use crate::backends::SqliteTerminologyBackend;
    use helios_persistence::backends::sqlite::SqliteBackend;

    let config = options.config.clone();
    if let Some(parent) = std::path::Path::new(&config.database_url).parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    let backend = SqliteTerminologyBackend::new_without_fts_prebuild(&config.database_url)
        .map_err(|e| anyhow::anyhow!("terminology store: {e}"))?;
    let hts_pool = backend.pool().clone();
    crate::bootstrap::bootstrap_sync_sqlite(&backend, &config).await?;
    let backend = {
        let handle = options.import_runtime.clone();
        on(&handle, async move {
            tokio::task::spawn_blocking(move || {
                backend
                    .finalize_after_bootstrap()
                    .map(|()| backend)
                    .map_err(|e| anyhow::anyhow!("post-bootstrap finalization failed: {e}"))
            })
            .await
            .map_err(|e| anyhow::anyhow!("finalization task: {e}"))?
        })
        .await?
    };
    let resource_store = SqliteBackend::open(&config.database_url)
        .map_err(|e| anyhow::anyhow!("terminology resource store: {e}"))?;
    resource_store
        .init_schema()
        .map_err(|e| anyhow::anyhow!("terminology resource store schema: {e}"))?;

    let (changes_tx, changes_rx) = tokio::sync::watch::channel(0u64);
    let state = AppState::new(backend)
        .with_resource_store(resource_store)
        .with_hts_pool(hts_pool)
        .with_max_expansion_size(config.max_expansion_size)
        .with_auto_supplements(config.auto_supplements)
        .with_audit(options.audit.clone())
        .with_public_base(options.public_base.clone())
        .with_changes(Arc::new(changes_tx))
        .with_import_runtime(options.import_runtime.clone());
    Ok(assemble(&options, state, changes_rx, "sqlite"))
}

/// Nest `api` under `prefix` (`/tx`) in `outer`, including `{prefix}/`
/// itself: axum's `nest` maps `/tx` but not `/tx/` to the inner `/`, and
/// `POST /tx/` is the terminology batch endpoint clients (and nginx's
/// `location /tx/`) use.
pub fn nest_at(outer: Router, prefix: &str, api: Router) -> Router {
    use tower::ServiceExt;
    let root = api.clone();
    outer
        .route(
            &format!("{prefix}/"),
            axum::routing::any(move |request: axum::extract::Request| {
                let root = root.clone();
                async move {
                    let (mut parts, body) = request.into_parts();
                    let target = match parts.uri.query() {
                        Some(q) => format!("/?{q}"),
                        None => "/".to_string(),
                    };
                    if let Ok(uri) = target.parse() {
                        parts.uri = uri;
                    }
                    match root
                        .oneshot(axum::extract::Request::from_parts(parts, body))
                        .await
                    {
                        Ok(response) => response,
                        Err(never) => match never {},
                    }
                }
            }),
        )
        .nest(prefix, api)
}

/// Build the API and admin routers over `state` and the loopback service.
pub fn assemble<B>(
    options: &EmbeddedOptions,
    state: AppState<B>,
    changes: tokio::sync::watch::Receiver<u64>,
    storage: &'static str,
) -> EmbeddedHts
where
    B: TerminologyBackend + BundleImportBackend + crate::admin::TerminologyAdmin + Clone + 'static,
{
    let config = &options.config;
    let gate = crate::server::writes_gate(
        &options.write_gate,
        config.require_auth_for_writes,
        options.bearer.is_some(),
    );
    let api = crate::server::tx_api_router(
        state.clone(),
        crate::write_guard::WriteGuard {
            gate,
            max_body_size: config.max_body_size,
            bearer: options.bearer.clone(),
        },
    );
    let api = match &state.audit {
        Some(audit) => api.layer(axum::middleware::from_fn_with_state(
            Arc::clone(&audit.middleware),
            helios_audit::middleware::audit_middleware,
        )),
        None => api,
    };
    let api = api
        .layer(DefaultBodyLimit::max(config.max_body_size))
        .layer(RequestDecompressionLayer::new())
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(config.request_timeout.max(1)),
        ));
    let admin = if config.admin_ui {
        crate::server::build_admin(config, &state)
            .map(|router| router.layer(axum::middleware::from_fn(crate::audit::capture_admin_user)))
    } else {
        None
    };
    let service = Arc::new(EmbeddedTerminology::new(
        api.clone(),
        changes,
        options.call_timeout,
    ));
    EmbeddedHts {
        api,
        admin,
        service,
        storage,
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use helios_terminology::{TxPrincipal, service::ValidateCodeParams};

    fn options(dir: &std::path::Path) -> EmbeddedOptions {
        let config = HtsConfig {
            database_url: dir.join("terminology.db").display().to_string(),
            storage_backend: "sqlite".into(),
            // Writes need a credential (the gate below is locked), as in
            // production where the administrator password is set.
            require_auth_for_writes: Some(true),
            ..HtsConfig::default()
        };
        EmbeddedOptions {
            config,
            pool: PoolSettings::default(),
            public_base: Some("https://fhir.example.org/tx".into()),
            import_runtime: None,
            audit: None,
            write_gate: helios_admin_auth::AdminGate::Locked(Arc::from("no password")),
            bearer: None,
            call_timeout: Duration::from_secs(30),
            import_pool_size: 0,
        }
    }

    fn code_system() -> serde_json::Value {
        serde_json::json!({
            "resourceType": "Bundle", "type": "collection",
            "entry": [{"resource": {
                "resourceType": "CodeSystem", "url": "http://example.org/cs",
                "version": "1", "status": "active", "content": "complete",
                "concept": [{"code": "a", "display": "Alpha"}, {"code": "b", "display": "Beta"}]
            }}]
        })
    }

    /// The embedded service answers through the same router as `/tx`; a
    /// System write passes the (locked) write guard, an anonymous one does
    /// not; a write moves the change feed; search links use the public base.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loopback_round_trip_writes_and_changes() {
        let dir = tempfile::tempdir().unwrap();
        let hts = start(options(dir.path())).await.unwrap();
        assert_eq!(hts.storage, "sqlite");
        let tx = hts.service.clone();
        assert_eq!(tx.kind(), ServiceKind::Embedded);
        let mut changes = tx.changes().unwrap();
        let before = *changes.borrow_and_update();

        // Anonymous write: refused by the locked gate.
        let anon = tx
            .call(TxRequest::post_json("import", &code_system()))
            .await
            .unwrap();
        assert!(!anon.is_success(), "{}", anon.status);

        let report = tx.import_bundle(&code_system()).await.unwrap();
        assert_eq!(report.code_systems, 1);
        assert!(changes.has_changed().unwrap());
        assert_ne!(*changes.borrow_and_update(), before);

        let ok = tx
            .validate_code(&ValidateCodeParams {
                system: "http://example.org/cs".into(),
                code: "a".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(ok.result);
        let bad = tx
            .validate_code(&ValidateCodeParams {
                system: "http://example.org/cs".into(),
                code: "zz".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!bad.result);

        let bundle = tx
            .call(
                TxRequest::get("CodeSystem")
                    .with_query_param("url", "http://example.org/cs")
                    .as_principal(TxPrincipal::Anonymous),
            )
            .await
            .unwrap()
            .into_json()
            .unwrap();
        let self_link = bundle["link"][0]["url"].as_str().unwrap();
        assert!(
            self_link.starts_with("https://fhir.example.org/tx/CodeSystem?"),
            "{self_link}"
        );
        assert_eq!(
            tx.health().await.state,
            helios_terminology::service::HealthState::Ok
        );
    }

    /// Imports run on the import runtime's threads when one is given.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn import_runs_on_the_import_runtime() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(2)
            .thread_name("tx-import-test")
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut opts = options(dir.path());
        opts.import_runtime = Some(rt.handle().clone());
        let hts = start(opts).await.unwrap();
        let state_probe =
            AppState::new(crate::backends::SqliteTerminologyBackend::in_memory().unwrap())
                .with_import_runtime(Some(rt.handle().clone()));
        let name = state_probe
            .on_import_runtime(async { std::thread::current().name().map(str::to_string) })
            .await;
        assert_eq!(name.as_deref(), Some("tx-import-test"));
        let report = hts.service.import_bundle(&code_system()).await.unwrap();
        assert_eq!(report.code_systems, 1);
        drop(hts);
        rt.shutdown_background();
    }

    fn fixture() -> serde_json::Value {
        serde_json::json!({
            "resourceType": "Bundle", "type": "collection",
            "entry": [
                {"resource": {
                    "resourceType": "CodeSystem", "url": "http://example.org/anatomy",
                    "version": "1", "status": "active", "content": "complete",
                    "hierarchyMeaning": "is-a",
                    "concept": [{"code": "body", "display": "Body", "concept": [
                        {"code": "limb", "display": "Limb", "concept": [
                            {"code": "arm", "display": "Arm"}, {"code": "leg", "display": "Leg"}]}]}]
                }},
                {"resource": {
                    "resourceType": "ValueSet", "url": "http://example.org/vs/limbs",
                    "version": "1", "status": "active",
                    "compose": {"include": [{"system": "http://example.org/anatomy",
                        "filter": [{"property": "concept", "op": "is-a", "value": "limb"}]}]}
                }},
                {"resource": {
                    "resourceType": "ConceptMap", "url": "http://example.org/cm/anatomy",
                    "version": "1", "status": "active",
                    "group": [{"source": "http://example.org/anatomy", "target": "http://example.org/other",
                        "element": [{"code": "arm", "target": [{"code": "A", "equivalence": "equivalent"}]}]}]
                }}
            ]
        })
    }

    /// The CI form of the U2 differential acceptance: the same store served
    /// by the standalone router (behind an nginx-like proxy: public Host,
    /// https, `sub_filter` adding `/tx/`) and by the embedded API mounted at
    /// `/tx` answer byte for byte the same to a corpus of operations.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn embedded_tx_is_byte_identical_to_standalone() {
        use axum::body::Body;
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let dir = tempfile::tempdir().unwrap();
        let opts = options(dir.path());
        let hts = start(opts).await.unwrap();
        let report = hts.service.import_bundle(&fixture()).await.unwrap();
        assert_eq!(report.code_systems, 1);

        // Standalone, over the same database file.
        let config = HtsConfig {
            database_url: dir.path().join("terminology.db").display().to_string(),
            ui_enabled: false,
            ..HtsConfig::default()
        };
        let backend = crate::backends::SqliteTerminologyBackend::new(&config.database_url).unwrap();
        let store = helios_persistence::backends::sqlite::SqliteBackend::open(&config.database_url)
            .unwrap();
        store.init_schema().unwrap();
        let pool = backend.pool().clone();
        let state = AppState::new(backend)
            .with_resource_store(store)
            .with_hts_pool(pool)
            .with_max_expansion_size(config.max_expansion_size);
        let standalone = crate::server::create_app(&config, state);
        let embedded = nest_at(axum::Router::new(), "/tx", hts.api.clone());

        let corpus: Vec<(&str, &str, Option<serde_json::Value>)> = vec![
            (
                "GET",
                "CodeSystem/$lookup?system=http://example.org/anatomy&code=arm",
                None,
            ),
            (
                "GET",
                "CodeSystem/$lookup?system=http://example.org/anatomy&code=nope",
                None,
            ),
            (
                "GET",
                "CodeSystem/$validate-code?url=http://example.org/anatomy&code=leg",
                None,
            ),
            (
                "GET",
                "CodeSystem/$validate-code?url=http://example.org/anatomy&code=leg&display=Wrong",
                None,
            ),
            (
                "GET",
                "CodeSystem/$subsumes?system=http://example.org/anatomy&codeA=limb&codeB=arm",
                None,
            ),
            (
                "GET",
                "ValueSet/$validate-code?url=http://example.org/vs/limbs&system=http://example.org/anatomy&code=arm",
                None,
            ),
            (
                "GET",
                "ValueSet/$validate-code?url=http://example.org/vs/limbs&system=http://example.org/anatomy&code=body",
                None,
            ),
            (
                "GET",
                "ConceptMap/$translate?url=http://example.org/cm/anatomy&system=http://example.org/anatomy&code=arm",
                None,
            ),
            (
                "GET",
                "CodeSystem?url=http://example.org/anatomy&_summary=true",
                None,
            ),
            ("GET", "ValueSet?_count=1", None),
            ("GET", "ConceptMap?url=http://example.org/cm/anatomy", None),
            (
                "POST",
                "CodeSystem/$validate-code",
                Some(
                    serde_json::json!({"resourceType": "Parameters", "parameter": [
                {"name": "url", "valueUri": "http://example.org/anatomy"}, {"name": "code", "valueCode": "arm"}]}),
                ),
            ),
            (
                "POST",
                "",
                Some(
                    serde_json::json!({"resourceType": "Bundle", "type": "batch", "entry": [
                {"request": {"method": "GET", "url": "CodeSystem/$lookup?system=http://example.org/anatomy&code=leg"}}]}),
                ),
            ),
            ("GET", "CodeSystem/does-not-exist", None),
        ];
        let call = |app: axum::Router,
                    method: String,
                    uri: String,
                    body: Option<serde_json::Value>,
                    proxy: bool| async move {
            let mut b = axum::http::Request::builder()
                .method(method.as_str())
                .uri(uri)
                .header("accept", "application/fhir+json");
            if proxy {
                b = b
                    .header("host", "fhir.example.org")
                    .header("x-forwarded-proto", "https");
            }
            let req = match body {
                Some(v) => b
                    .header("content-type", "application/fhir+json")
                    .body(Body::from(v.to_string()))
                    .unwrap(),
                None => b.body(Body::empty()).unwrap(),
            };
            let resp = app.oneshot(req).await.unwrap();
            let status = resp.status();
            let ctype = resp.headers().get("content-type").cloned();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            (status, ctype, bytes)
        };
        for (method, path, body) in corpus {
            let (sa, ca, a) = call(
                standalone.clone(),
                method.to_string(),
                format!("/{path}"),
                body.clone(),
                true,
            )
            .await;
            // nginx: sub_filter 'https://HOST/' 'https://HOST/tx/'.
            let a = String::from_utf8_lossy(&a)
                .replace("https://fhir.example.org/", "https://fhir.example.org/tx/");
            let (sb, cb, b) = call(
                embedded.clone(),
                method.to_string(),
                format!("/tx/{path}"),
                body,
                false,
            )
            .await;
            let b = String::from_utf8_lossy(&b).to_string();
            let scrub = |s: &str| {
                // Search Bundles carry a fresh id and timestamp per response.
                let re = regex::Regex::new(r#""(id|lastUpdated|timestamp)":"[^"]*""#).unwrap();
                re.replace_all(s, "\"$1\":\"*\"").to_string()
            };
            assert_eq!(sa, sb, "{method} {path}");
            assert_eq!(ca, cb, "{method} {path}");
            assert_eq!(scrub(&a), scrub(&b), "{method} {path}");
        }
    }
}
