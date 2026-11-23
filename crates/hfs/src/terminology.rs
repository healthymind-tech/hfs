//! The server's terminology service (design `docs/design/unified-server.md`
//! §3–§8, packages U2 + U3).
//!
//! `HFS_TERMINOLOGY=embedded|remote|off` (default: `embedded` in a build with
//! the `terminology` feature, but `remote` whenever `HFS_TERMINOLOGY_SERVER`
//! is set and `HFS_TERMINOLOGY` is not — an upgrade changes nothing until the
//! deployment opts in). This module
//!
//! * resolves the settings (`HFS_TERMINOLOGY_*`, legacy `HTS_*` folded in and
//!   each logged once as a deprecation);
//! * builds the service — the embedded HTS on the `terminology-import`
//!   runtime ([`crate::terminology_runtime`]), a remote server over HTTP, or
//!   nothing — and installs it for every caller (validation, `:in` search,
//!   SQL-on-FHIR, FHIRPath, package manager, console);
//! * mounts the embedded terminology API at `/tx` (byte-identical to the
//!   standalone `hts` behind nginx) and its admin API at
//!   `/admin/terminology`, with the 10 MiB read body limit and the anonymous
//!   rate limit of design §12;
//! * keeps `/health`'s `components.terminology` and the `hfs_terminology_up`,
//!   `terminology_permits_available` and `terminology_epoch` gauges fresh.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use helios_rest::config::ServerConfig;
use helios_terminology::config::ConfigError;
use helios_terminology::{EnvView, TerminologyMode, TerminologyService, TerminologySettings};
use tracing::{info, warn};

/// Body limit of `/tx` requests that only read (`$expand`, `$validate-code`
/// … sent as `POST`): design §12. Writes keep `HFS_TERMINOLOGY_MAX_BODY_SIZE`.
pub const READ_BODY_LIMIT: usize = 10 * 1024 * 1024;

/// Resolved settings plus the environment view they came from.
pub struct Resolved {
    pub settings: TerminologySettings,
    pub view: EnvView,
}

/// Resolve `HFS_TERMINOLOGY_*` and align `config` with the mode: embedded
/// clears `terminology_server` (callers then use the installed service) and
/// sets `terminology_embedded`; remote keeps the server URL.
pub fn resolve(config: &mut ServerConfig) -> Result<Resolved, ConfigError> {
    // The store derived from `HFS_DATABASE_URL` is worked out when the
    // embedded service starts ([`derived_store`]), so a FHIR URL the schema
    // helper cannot rewrite never fails configuration here.
    resolve_from(
        config,
        EnvView::from_vars(std::env::vars().filter(|(k, _)| k != "HFS_DATABASE_URL")),
    )
}

/// [`resolve`] over an explicit environment (tests).
pub fn resolve_from(config: &mut ServerConfig, view: EnvView) -> Result<Resolved, ConfigError> {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let settings = TerminologySettings::resolve(&view, cfg!(feature = "terminology"), cores)?;
    match settings.mode {
        TerminologyMode::Embedded => {
            config.terminology_embedded = true;
            config.terminology_server = None;
            let _ = EMBEDDED_SCHEMA.set(settings.embedded.schema.clone());
        }
        TerminologyMode::Remote => {
            config.terminology_embedded = false;
            config.terminology_server = settings.server_url.clone();
        }
        TerminologyMode::Off => {
            config.terminology_embedded = false;
            config.terminology_server = None;
        }
    }
    Ok(Resolved { settings, view })
}

/// Log every legacy variable once (names only, never values).
pub fn log_deprecations(view: &EnvView) {
    for deprecation in &view.deprecations {
        warn!("{}", deprecation.message());
    }
}

/// Schema of the embedded terminology store when terminology is embedded
/// (set by [`resolve`]); drives the unified fork migrations.
static EMBEDDED_SCHEMA: OnceLock<String> = OnceLock::new();

/// Whether `HFS_TERMINOLOGY_READONLY` is on: every terminology write — `/tx`
/// CRUD, `/import`, write batches, admin imports and deletes, package
/// terminology installs — answers 503; reads keep being served. The freeze
/// step of `unify-db.sh` (docs/ops/unified-server.md).
pub fn readonly(view: &EnvView) -> bool {
    view.get("HFS_TERMINOLOGY_READONLY").is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Apply the unified server's fork migrations (`hm-0011`: terminology schema
/// and `hfs_terminology_state`; `hm-0012`: console scope bindings) to the
/// HFS PostgreSQL database (U1 contract: not part of `init_schema`).
/// Embedded terminology gets both; otherwise only `hm-0012`, the console's
/// terminology-scope bindings (the Terminology section exists with a remote
/// server too) — no `terminology` schema is created before the data move.
#[cfg(feature = "postgres")]
pub async fn apply_unified_migrations(
    backend: &helios_persistence::backends::postgres::PostgresBackend,
) -> anyhow::Result<()> {
    let Some(schema) = EMBEDDED_SCHEMA.get() else {
        let applied = backend
            .apply_console_fork_migrations()
            .await
            .map_err(|e| anyhow::anyhow!("console fork migration hm-0012: {e}"))?;
        if !applied.is_empty() {
            info!(?applied, "applied console fork migration");
        }
        return Ok(());
    };
    let applied = backend
        .apply_unified_migrations(schema)
        .await
        .map_err(|e| anyhow::anyhow!("unified fork migrations: {e}"))?;
    if !applied.is_empty() {
        info!(?applied, schema = %schema, "applied unified-server fork migrations");
    }
    Ok(())
}

#[cfg(not(feature = "postgres"))]
pub async fn apply_unified_migrations<B>(_backend: &B) -> anyhow::Result<()> {
    Ok(())
}

/// The embedded routers, mounted by [`attach`].
struct Mount {
    api: Router,
    admin: Option<Router>,
    rate_limit: u32,
}

static MOUNT: OnceLock<Mount> = OnceLock::new();

/// Build and install the server's terminology service. Call once, before
/// the FHIR application state is built.
pub async fn start(
    config: &ServerConfig,
    resolved: &Resolved,
    audit: Option<&Arc<helios_audit::AuditMiddlewareState>>,
    admin_gate: helios_admin_auth::AdminGate,
) -> anyhow::Result<()> {
    let settings = &resolved.settings;
    info!(
        mode = ?settings.mode,
        server = ?settings.server_url.as_deref().map(helios_observability::redact::redact_url),
        public_url = ?settings.public_url,
        "terminology service"
    );
    let service: Arc<dyn TerminologyService> = match settings.mode {
        TerminologyMode::Off => {
            helios_rest::terminology::record_health(
                helios_terminology::DisabledTerminology.health().await,
            );
            return Ok(());
        }
        TerminologyMode::Remote => {
            let url = settings
                .server_url
                .clone()
                .ok_or(ConfigError::RemoteWithoutServer)?;
            let token = config
                .packages
                .terminology_token
                .as_ref()
                .map(|t| t.expose().to_string());
            let remote = helios_terminology::remote::RemoteTerminology::new(
                &url,
                token,
                settings.timeout,
                Arc::new(helios_rest::terminology::ReqwestTransport::new()),
            )
            .map_err(|e| anyhow::anyhow!("HFS_TERMINOLOGY_SERVER: {e}"))?;
            Arc::new(remote)
        }
        TerminologyMode::Embedded => {
            match start_embedded(config, resolved, audit, admin_gate).await {
                Ok(Some(service)) => service,
                Ok(None) => return Ok(()),
                // FHIR comes first: a terminology store that cannot be
                // opened leaves terminology down (health, readiness with
                // HFS_READINESS_REQUIRE_TERMINOLOGY) instead of the server.
                Err(e) => {
                    tracing::error!(error = %e, "embedded terminology failed to start; terminology is unavailable");
                    helios_rest::terminology::record_health(
                        helios_terminology::TerminologyHealth {
                            kind: helios_terminology::ServiceKind::Embedded,
                            state: helios_terminology::service::HealthState::Down(format!(
                                "embedded terminology failed to start: {e}"
                            )),
                        },
                    );
                    return Ok(());
                }
            }
        }
    };
    let url = match settings.mode {
        TerminologyMode::Remote => settings.server_url.clone(),
        _ => None,
    };
    helios_rest::terminology::install_server_terminology(
        Arc::clone(&service),
        url,
        settings.embedded.operation_permits,
        settings.timeout,
    );
    #[cfg(feature = "ui")]
    helios_ui::set_terminology_service(Some(Arc::clone(&service)));
    spawn_health_probe(service);
    Ok(())
}

/// The terminology store when no URL was resolved from the environment:
/// derived from the FHIR store the server actually opens (`fhir.db` by
/// default) — schema `terminology` of a PostgreSQL database, or
/// `terminology.db` next to a SQLite file. `None` for an in-memory store.
fn derived_store(config: &ServerConfig, schema: &str) -> Option<String> {
    let fhir = config.database_url.as_deref().unwrap_or("fhir.db");
    if helios_terminology::schema::is_postgres_url(fhir) {
        helios_terminology::schema::with_search_path(fhir, schema).ok()
    } else {
        helios_terminology::schema::sqlite_terminology_url(fhir)
    }
}

#[cfg(feature = "terminology")]
async fn start_embedded(
    config: &ServerConfig,
    resolved: &Resolved,
    audit: Option<&Arc<helios_audit::AuditMiddlewareState>>,
    admin_gate: helios_admin_auth::AdminGate,
) -> anyhow::Result<Option<Arc<dyn TerminologyService>>> {
    use helios_hts::embedded::{EmbeddedOptions, PoolSettings};
    let settings = &resolved.settings;
    let view = &resolved.view;
    let embedded = &settings.embedded;
    let Some(database_url) = embedded
        .database_url
        .clone()
        .or_else(|| derived_store(config, &embedded.schema))
    else {
        warn!(
            "HFS_TERMINOLOGY=embedded with an in-memory FHIR store and no \
             HFS_TERMINOLOGY_DATABASE_URL: terminology is off for this process"
        );
        return Ok(None);
    };
    // A store derived from HFS_DATABASE_URL lives in its own schema of the
    // hfs database; create the schema first so HTS's tables cannot land in
    // `public` (design §5).
    #[cfg(feature = "postgres")]
    if view.get("HFS_TERMINOLOGY_DATABASE_URL").is_none()
        && helios_terminology::schema::is_postgres_url(&database_url)
    {
        helios_hts::embedded::ensure_schema(&database_url, &embedded.schema).await?;
    }

    let import = crate::terminology_runtime::start(
        embedded.import_threads,
        embedded.import_blocking_threads,
    )?;
    let hts = hts_config(view, &database_url)?;
    let bearer = {
        let prefix = if view.get("HFS_TERMINOLOGY_AUTH_JWKS_URL").is_some() {
            "HFS_TERMINOLOGY_AUTH"
        } else {
            "HFS_AUTH"
        };
        helios_hts::write_guard::bearer_provider_with_prefix(prefix, |k| {
            view.get(k).map(str::to_string)
        })
    };
    let audit = audit.map(|state| embedded_audit(state, settings.public_url.as_deref()));
    let started = Instant::now();
    let hts = helios_hts::embedded::start(EmbeddedOptions {
        config: hts,
        pool: PoolSettings {
            size: embedded.pg_pool_size,
            wait_timeout_secs: Some(embedded.pg_pool_wait_timeout.as_secs()),
            connect_timeout_secs: view
                .get("HFS_TERMINOLOGY_PG_CONNECT_TIMEOUT_SECS")
                .and_then(|v| v.trim().parse().ok()),
            statement_timeout_ms: Some(embedded.pg_statement_timeout_ms),
        },
        public_base: settings.public_url.clone(),
        import_runtime: Some(import),
        audit,
        write_gate: admin_gate,
        bearer,
        call_timeout: settings.timeout,
        readonly: readonly(view),
        import_throttle: import_throttle(view),
        import_async_commit: !view
            .get("HFS_TERMINOLOGY_IMPORT_SYNC_COMMIT")
            .is_some_and(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "on" | "yes"
                )
            }),
        import_pool_size: view
            .get("HFS_TERMINOLOGY_IMPORT_PG_POOL_SIZE")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(4),
    })
    .await?;
    if readonly(view) {
        warn!(
            "HFS_TERMINOLOGY_READONLY=true: terminology writes (CRUD, /import, write batches, \
             admin imports) answer 503 until it is unset"
        );
    }
    info!(
        storage = hts.storage,
        elapsed_ms = started.elapsed().as_millis() as u64,
        admin = hts.admin.is_some(),
        "embedded terminology service ready at /tx"
    );
    let rate_limit = view
        .get("HFS_TERMINOLOGY_RATE_LIMIT_ANONYMOUS")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(600);
    let _ = MOUNT.set(Mount {
        api: hts.api.clone(),
        admin: hts.admin.clone(),
        rate_limit,
    });
    Ok(Some(hts.service))
}

#[cfg(not(feature = "terminology"))]
async fn start_embedded(
    _config: &ServerConfig,
    _resolved: &Resolved,
    _audit: Option<&Arc<helios_audit::AuditMiddlewareState>>,
    _admin_gate: helios_admin_auth::AdminGate,
) -> anyhow::Result<Option<Arc<dyn TerminologyService>>> {
    Err(ConfigError::EmbeddedNotBuilt.into())
}

/// Default write duty cycle of admin-job imports (half the wall time).
pub const DEFAULT_IMPORT_DUTY: f64 = 0.5;
/// Default import batch size of the embedded service (standalone HTS: 5000).
pub const DEFAULT_IMPORT_BATCH_SIZE: usize = 2000;

/// Pacing of admin-job imports (design §8.2, U3 follow-up):
/// `HFS_TERMINOLOGY_IMPORT_DUTY` (0.05–1, default 0.5; 1 = no pacing),
/// `HFS_TERMINOLOGY_IMPORT_MAX_PAUSE_MS` (default 2000),
/// `HFS_TERMINOLOGY_IMPORT_PROBE_SLOW_MS` (serving-pool latency that makes
/// the import back off; default 25, 0 = no probe).
#[cfg(feature = "terminology")]
pub fn import_throttle(view: &EnvView) -> helios_hts::import::throttle::ThrottlePolicy {
    let num = |k: &str| view.get(k).and_then(|v| v.trim().parse::<f64>().ok());
    helios_hts::import::throttle::ThrottlePolicy {
        duty: num("HFS_TERMINOLOGY_IMPORT_DUTY")
            .unwrap_or(DEFAULT_IMPORT_DUTY)
            .clamp(0.05, 1.0),
        max_pause: Duration::from_millis(
            num("HFS_TERMINOLOGY_IMPORT_MAX_PAUSE_MS").unwrap_or(2000.0) as u64,
        ),
        probe_slow: Duration::from_millis(
            num("HFS_TERMINOLOGY_IMPORT_PROBE_SLOW_MS").unwrap_or(25.0) as u64,
        ),
        probe: None,
        // HFS_TERMINOLOGY_IMPORT_DEFER_EVICTION=false evicts on every epoch
        // move even while an admin import writes.
        activity: (!view
            .get("HFS_TERMINOLOGY_IMPORT_DEFER_EVICTION")
            .is_some_and(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "off" | "no"
                )
            }))
        .then(Default::default),
    }
}

/// The HTS settings of the embedded service, from the `HFS_TERMINOLOGY_*`
/// view (legacy `HTS_*` already folded in). Anything not named keeps the
/// standalone server's default, so embedded answers match it exactly.
#[cfg(feature = "terminology")]
pub fn hts_config(
    view: &EnvView,
    database_url: &str,
) -> anyhow::Result<helios_hts::config::HtsConfig> {
    use clap::Parser;
    const FLAGS: &[(&str, &str)] = &[
        ("HFS_TERMINOLOGY_MAX_EXPANSION_SIZE", "--max-expansion-size"),
        ("HFS_TERMINOLOGY_AUTO_SUPPLEMENTS", "--auto-supplements"),
        ("HFS_TERMINOLOGY_BOOTSTRAP_DIR", "--bootstrap-dir"),
        (
            "HFS_TERMINOLOGY_CACHE_SYNC_INTERVAL_MS",
            "--cache-sync-interval-ms",
        ),
        ("HFS_TERMINOLOGY_IMPORT_LANGUAGES", "--import-languages"),
        ("HFS_TERMINOLOGY_ADMIN", "--admin-ui"),
        ("HFS_TERMINOLOGY_DATA_DIR", "--admin-data-dir"),
        ("HFS_TERMINOLOGY_IMPORT_DIR", "--import-dir"),
        ("HFS_TERMINOLOGY_PACKAGE_REGISTRY", "--package-registry"),
        ("HFS_TERMINOLOGY_UPLOAD_CHUNK_SIZE", "--upload-chunk-size"),
        ("HFS_TERMINOLOGY_MAX_UPLOAD_SIZE", "--max-upload-size"),
        ("HFS_TERMINOLOGY_DISK_HEADROOM", "--disk-headroom"),
        (
            "HFS_TERMINOLOGY_REQUIRE_AUTH_FOR_WRITES",
            "--require-auth-for-writes",
        ),
        ("HFS_TERMINOLOGY_MAX_BODY_SIZE", "--max-body-size"),
        ("HFS_TERMINOLOGY_REQUEST_TIMEOUT", "--request-timeout"),
    ];
    let storage = if helios_terminology::schema::is_postgres_url(database_url) {
        "postgres"
    } else {
        "sqlite"
    };
    let mut args = vec![
        "hts".to_string(),
        format!("--database-url={database_url}"),
        format!("--storage-backend={storage}"),
    ];
    // Embedded imports write in smaller transactions than standalone HTS so
    // FHIR commits on the same database wait less behind them.
    let batch = view
        .get("HFS_TERMINOLOGY_IMPORT_BATCH_SIZE")
        .or_else(|| view.get("HFS_TERMINOLOGY_BOOTSTRAP_BATCH_SIZE"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_IMPORT_BATCH_SIZE)
        .max(1);
    args.push(format!("--bootstrap-batch-size={batch}"));
    for (name, flag) in FLAGS {
        if let Some(value) = view.get(name) {
            let value = match *flag {
                // HTS's switches take true/false; accept the HFS spellings too.
                "--admin-ui" | "--auto-supplements" | "--require-auth-for-writes" => {
                    match value.trim().to_ascii_lowercase().as_str() {
                        "1" | "true" | "yes" | "on" => "true".to_string(),
                        _ => "false".to_string(),
                    }
                }
                _ => value.trim().to_string(),
            };
            args.push(format!("{flag}={value}"));
        }
    }
    helios_hts::config::HtsConfig::try_parse_from(&args)
        .map_err(|e| anyhow::anyhow!("terminology settings: {e}"))
}

/// The embedded service's audit trail: the host's sink (one queue, one
/// fallback, one retention), terminology reads off as in standalone HTS
/// (validator traffic is in-process reads), the `/tx` base as the server
/// address so terminology events are told apart.
#[cfg(feature = "terminology")]
fn embedded_audit(
    state: &Arc<helios_audit::AuditMiddlewareState>,
    public_url: Option<&str>,
) -> helios_hts::audit::HtsAudit {
    let mut config = state.config.clone();
    config.reads = std::env::var("HFS_TERMINOLOGY_AUDIT_READS")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    let mut middleware = helios_audit::AuditMiddlewareState::new(Arc::clone(&state.sink), config);
    if let Some(url) = public_url {
        middleware = middleware.with_server_address(url.to_string());
    }
    helios_hts::audit::HtsAudit {
        middleware: Arc::new(middleware),
        search: None,
    }
}

/// The console's in-process upstream (`/ui/terminology` pages and their
/// browser admin API) when the embedded service is running: the server's
/// terminology service and the admin router mounted at
/// `/admin/terminology`. `None` in remote / off mode.
#[cfg(feature = "ui")]
pub fn console_upstream() -> Option<helios_ui::terminology_console::InProcessUpstream> {
    let mount = MOUNT.get()?;
    let service = helios_rest::terminology::server_terminology()?;
    Some(helios_ui::terminology_console::InProcessUpstream::new(
        service,
        mount.admin.clone(),
    ))
}

/// Mount the embedded terminology API at `/tx` and its admin API at
/// `/admin/terminology` (no-op unless the embedded service is running).
pub fn attach(app: Router) -> Router {
    let Some(mount) = MOUNT.get() else {
        return app;
    };
    attach_routers(
        app,
        mount.api.clone(),
        mount.admin.clone(),
        mount.rate_limit,
    )
}

/// [`attach`] with explicit routers (tests).
pub fn attach_routers(
    app: Router,
    api: Router,
    admin: Option<Router>,
    rate_limit_per_minute: u32,
) -> Router {
    let limiter = Arc::new(RateLimiter::new(rate_limit_per_minute));
    let tx = Router::new()
        // `/tx/admin/**` moved to `/admin/terminology/**` (one release).
        .route("/admin", axum::routing::any(tx_admin_redirect))
        .route("/admin/{*rest}", axum::routing::any(tx_admin_redirect))
        .merge(api)
        .layer(axum::middleware::from_fn(read_body_limit))
        .layer(axum::middleware::from_fn_with_state(limiter, rate_limit))
        .layer(tower_http::compression::CompressionLayer::new())
        .layer(tower_http::cors::CorsLayer::permissive());
    let app = nest_tx(app, tx);
    match admin {
        Some(admin) => app.nest(helios_terminology::routes::ADMIN_PREFIX, admin),
        None => app,
    }
}

#[cfg(feature = "terminology")]
fn nest_tx(app: Router, tx: Router) -> Router {
    helios_hts::embedded::nest_at(app, helios_terminology::routes::TX_PREFIX, tx)
}

#[cfg(not(feature = "terminology"))]
fn nest_tx(app: Router, tx: Router) -> Router {
    app.nest(helios_terminology::routes::TX_PREFIX, tx)
}

async fn tx_admin_redirect(request: Request) -> Response {
    let path = request.uri().path();
    let rest = path.strip_prefix("/admin").unwrap_or("");
    let mut target = format!("{}{rest}", helios_terminology::routes::ADMIN_PREFIX);
    if let Some(q) = request.uri().query() {
        target.push('?');
        target.push_str(q);
    }
    Redirect::permanent(&target).into_response()
}

/// 413 for a `/tx` read whose body exceeds [`READ_BODY_LIMIT`] (writes keep
/// the larger write limit, enforced after the write guard admits them).
async fn read_body_limit(request: Request, next: Next) -> Response {
    if helios_hts::write_guard::is_terminology_write(request.method(), request.uri().path()) {
        return next.run(request).await;
    }
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|n| n > READ_BODY_LIMIT) {
        return too_large();
    }
    if declared.is_none() && request.method() != axum::http::Method::GET {
        // Chunked: read up to the limit, refuse past it.
        let (parts, body) = request.into_parts();
        return match axum::body::to_bytes(body, READ_BODY_LIMIT).await {
            Ok(bytes) => {
                next.run(Request::from_parts(parts, axum::body::Body::from(bytes)))
                    .await
            }
            Err(_) => too_large(),
        };
    }
    next.run(request).await
}

fn too_large() -> Response {
    outcome(
        StatusCode::PAYLOAD_TOO_LARGE,
        "too-costly",
        "terminology read requests are limited to 10 MiB",
    )
}

fn outcome(status: StatusCode, code: &str, message: &str) -> Response {
    let body = serde_json::json!({
        "resourceType": "OperationOutcome",
        "issue": [{"severity": "error", "code": code, "diagnostics": message}],
    });
    (
        status,
        [(header::CONTENT_TYPE, "application/fhir+json")],
        body.to_string(),
    )
        .into_response()
}

/// Fixed-window per-client limiter for anonymous `/tx` requests
/// (`HFS_TERMINOLOGY_RATE_LIMIT_ANONYMOUS` per minute, default 600, 0 off).
/// Requests with an `Authorization` header are not counted; in-process calls
/// never pass here.
pub struct RateLimiter {
    per_minute: u32,
    windows: dashmap::DashMap<IpAddr, (Instant, u32)>,
}

impl RateLimiter {
    pub fn new(per_minute: u32) -> Self {
        Self {
            per_minute,
            windows: dashmap::DashMap::new(),
        }
    }

    /// Whether one more request from `ip` is allowed now.
    pub fn allow(&self, ip: IpAddr, now: Instant) -> bool {
        if self.per_minute == 0 {
            return true;
        }
        if self.windows.len() > 100_000 {
            self.windows
                .retain(|_, (start, _)| now.duration_since(*start) < Duration::from_secs(60));
        }
        let mut entry = self.windows.entry(ip).or_insert((now, 0));
        if now.duration_since(entry.0) >= Duration::from_secs(60) {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= self.per_minute
    }
}

/// The client address: the TCP peer, or — when the peer is a private or
/// loopback proxy (nginx) — the last `X-Forwarded-For` hop it added.
fn client_ip(request: &Request) -> Option<IpAddr> {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let forwarded = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .and_then(|v| v.trim().parse::<IpAddr>().ok());
    match (peer, forwarded) {
        (Some(p), Some(f)) if is_private(p) => Some(f),
        (Some(p), _) => Some(p),
        (None, f) => f,
    }
}

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

async fn rate_limit(
    State(limiter): State<Arc<RateLimiter>>,
    request: Request,
    next: Next,
) -> Response {
    if limiter.per_minute > 0
        && !request.headers().contains_key(header::AUTHORIZATION)
        && let Some(ip) = client_ip(&request)
        && !limiter.allow(ip, Instant::now())
    {
        let mut response = outcome(
            StatusCode::TOO_MANY_REQUESTS,
            "throttled",
            "too many terminology requests; retry in a minute",
        );
        response.headers_mut().insert(
            header::RETRY_AFTER,
            axum::http::HeaderValue::from_static("60"),
        );
        return response;
    }
    next.run(request).await
}

/// Probe health every 15 s (`/health` component, `hfs_terminology_up`) and
/// sample the permit and epoch gauges.
fn spawn_health_probe(service: Arc<dyn TerminologyService>) {
    let mut changes = service.changes();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(15));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            helios_rest::terminology::record_health(service.health().await);
            if let Some(n) = helios_rest::terminology::permits_available() {
                metrics::gauge!("terminology_permits_available")
                    .set(n.min(u32::MAX as usize) as f64);
            }
            if let Some(rx) = changes.as_mut() {
                metrics::gauge!("terminology_epoch").set(*rx.borrow_and_update() as f64);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(pairs: &[(&str, &str)]) -> EnvView {
        EnvView::from_vars(pairs.iter().map(|(k, v)| (*k, *v)))
    }

    fn config() -> ServerConfig {
        ServerConfig::default()
    }

    #[test]
    fn upgrade_with_a_server_stays_remote() {
        let mut c = config();
        let r = resolve_from(
            &mut c,
            view(&[("HFS_TERMINOLOGY_SERVER", "http://hts:8090")]),
        )
        .unwrap();
        assert_eq!(r.settings.mode, TerminologyMode::Remote);
        assert_eq!(c.terminology_server.as_deref(), Some("http://hts:8090"));
        assert!(!c.terminology_embedded);
    }

    #[cfg(feature = "terminology")]
    #[test]
    fn explicit_embedded_wins_over_a_named_server() {
        let mut c = config();
        c.terminology_server = Some("http://hts:8090".into());
        let r = resolve_from(
            &mut c,
            view(&[
                ("HFS_TERMINOLOGY", "embedded"),
                ("HFS_TERMINOLOGY_SERVER", "http://hts:8090"),
                ("HFS_TERMINOLOGY_DATABASE_URL", "postgresql://hts@db/hts"),
            ]),
        )
        .unwrap();
        assert_eq!(r.settings.mode, TerminologyMode::Embedded);
        assert!(c.terminology_server.is_none());
        assert!(c.terminology_embedded);
        // Remote validation is valid without a server once embedded.
        c.validation.terminology = "remote".into();
        if let Err(errors) = c.validate() {
            assert!(
                errors
                    .iter()
                    .all(|m| !m.contains("HFS_VALIDATION_TERMINOLOGY")),
                "{errors:?}"
            );
        }
    }

    #[test]
    fn off_clears_the_server() {
        let mut c = config();
        c.terminology_server = Some("http://hts:8090".into());
        resolve_from(&mut c, view(&[("HFS_TERMINOLOGY", "off")])).unwrap();
        assert!(c.terminology_server.is_none());
    }

    #[test]
    fn remote_without_server_fails_fast() {
        let mut c = config();
        assert!(resolve_from(&mut c, view(&[("HFS_TERMINOLOGY", "remote")])).is_err());
    }

    /// A Stage 0 compose's `HTS_*` variables map onto the embedded settings,
    /// and each is reported as a deprecation without its value.
    #[cfg(feature = "terminology")]
    #[test]
    fn hts_only_environment_maps_to_embedded_settings() {
        let v = view(&[
            ("HFS_TERMINOLOGY", "embedded"),
            ("HTS_DATABASE_URL", "postgresql://hts:secret@db/hts"),
            ("HTS_ADMIN_UI", "true"),
            ("HTS_MAX_BODY_SIZE", "268435456"),
            ("HTS_REQUEST_TIMEOUT", "600"),
            ("HTS_IMPORT_DIR", "/import"),
        ]);
        let mut c = config();
        let r = resolve_from(&mut c, v).unwrap();
        let url = r.settings.embedded.database_url.clone().unwrap();
        assert_eq!(url, "postgresql://hts:secret@db/hts");
        let hts = hts_config(&r.view, &url).unwrap();
        assert!(hts.admin_ui);
        assert_eq!(hts.max_body_size, 268_435_456);
        assert_eq!(hts.request_timeout, 600);
        assert_eq!(hts.import_dir, "/import");
        assert_eq!(hts.storage_backend, "postgres");
        // Defaults the embedded service must share with standalone HTS.
        assert_eq!(hts.max_expansion_size, 3500);
        assert_eq!(hts.cache_sync_interval_ms, 1000);
        assert_eq!(hts.bootstrap_batch_size, DEFAULT_IMPORT_BATCH_SIZE);
        let t = import_throttle(&r.view);
        assert_eq!(t.duty, DEFAULT_IMPORT_DUTY);
        let tuned = import_throttle(&view(&[
            ("HFS_TERMINOLOGY_IMPORT_DUTY", "0.25"),
            ("HFS_TERMINOLOGY_IMPORT_PROBE_SLOW_MS", "0"),
        ]));
        assert_eq!(tuned.duty, 0.25);
        assert!(tuned.probe_slow.is_zero());
        assert!(r.view.deprecations.len() >= 5);
        for d in &r.view.deprecations {
            assert!(!d.message().contains("secret"));
        }
    }

    #[test]
    fn readonly_flag() {
        assert!(readonly(&view(&[("HFS_TERMINOLOGY_READONLY", "true")])));
        assert!(!readonly(&view(&[("HFS_TERMINOLOGY_READONLY", "false")])));
        assert!(!readonly(&view(&[])));
    }

    #[test]
    fn store_is_derived_from_the_fhir_store() {
        let mut c = config();
        c.database_url = None;
        assert_eq!(
            derived_store(&c, "terminology").as_deref(),
            Some("terminology.db")
        );
        c.database_url = Some("./data/fhir.db".into());
        assert_eq!(
            derived_store(&c, "terminology").as_deref(),
            Some("./data/terminology.db")
        );
        c.database_url = Some(":memory:".into());
        assert!(derived_store(&c, "terminology").is_none());
        c.database_url = Some("postgres://hfs@db/hfs".into());
        assert!(
            derived_store(&c, "terminology")
                .unwrap()
                .contains("search_path")
        );
    }

    #[test]
    fn rate_limiter_counts_per_client_per_minute() {
        let limiter = RateLimiter::new(3);
        let a: IpAddr = "10.1.2.3".parse().unwrap();
        let b: IpAddr = "10.1.2.4".parse().unwrap();
        let t0 = Instant::now();
        assert!((0..3).all(|_| limiter.allow(a, t0)));
        assert!(!limiter.allow(a, t0));
        assert!(limiter.allow(b, t0));
        assert!(limiter.allow(a, t0 + Duration::from_secs(61)));
        assert!(RateLimiter::new(0).allow(a, t0));
    }

    /// `/tx` mount: reads above 10 MiB are refused, the old `/tx/admin`
    /// path redirects, anonymous clients are rate limited, and requests
    /// reach the embedded API with the `/tx` prefix stripped.
    #[tokio::test]
    async fn tx_mount_limits_and_redirects() {
        use tower::ServiceExt;
        let api = Router::new()
            .route("/metadata", axum::routing::get(|| async { "cap" }))
            .route(
                "/ValueSet/$expand",
                axum::routing::post(
                    |body: axum::body::Bytes| async move { body.len().to_string() },
                ),
            )
            .route("/", axum::routing::post(|| async { "batch" }));
        let app = attach_routers(Router::new(), api, None, 2);
        let get = |uri: &str| {
            axum::http::Request::builder()
                .uri(uri)
                .header("x-forwarded-for", "203.0.113.9")
                .body(axum::body::Body::empty())
                .unwrap()
        };
        let r = app.clone().oneshot(get("/tx/metadata")).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        #[cfg(feature = "terminology")]
        {
            let batch = axum::http::Request::builder()
                .method("POST")
                .uri("/tx/")
                .header("authorization", "Bearer x")
                .body(axum::body::Body::from("{}"))
                .unwrap();
            let r = app.clone().oneshot(batch).await.unwrap();
            assert_eq!(
                r.status(),
                StatusCode::OK,
                "POST /tx/ reaches the batch route"
            );
        }
        let r = app
            .clone()
            .oneshot(get("/tx/admin/jobs?x=1"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(r.headers()[header::LOCATION], "/admin/terminology/jobs?x=1");
        let big = axum::http::Request::builder()
            .method("POST")
            .uri("/tx/ValueSet/$expand")
            .header(header::CONTENT_LENGTH, (READ_BODY_LIMIT + 1).to_string())
            .header("authorization", "Bearer x")
            .body(axum::body::Body::from(vec![b' '; READ_BODY_LIMIT + 1]))
            .unwrap();
        let r = app.clone().oneshot(big).await.unwrap();
        assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
        // The third anonymous request in the window (two above) is throttled.
        let r = app.clone().oneshot(get("/tx/metadata")).await.unwrap();
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
