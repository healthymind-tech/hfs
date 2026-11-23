//! Entry point for the `hts` binary.

use clap::Parser;
use helios_hts::config::{
    Cli, Command, HtsConfig, ImportArgs, ImportFormat, MigrateArgs, detect_format,
    effective_storage_backend,
};
use helios_hts::import::dispatch::dispatch_import;
use helios_hts::import::{BundleImportBackend, ImportResult, ImportStats, LanguageFilter};
use helios_persistence::core::database_url::redact_database_url;
use helios_persistence::tenant::TenantContext;
use tracing::info;

#[cfg(feature = "sqlite")]
use helios_hts::backends::SqliteTerminologyBackend;
#[cfg(feature = "sqlite")]
use helios_hts::state::AppState;

#[cfg(feature = "postgres")]
use helios_hts::backends::PostgresTerminologyBackend;

/// The stop-signal guard of `hts run` (see
/// [`helios_observability::shutdown::StopGuard`]); the servers mark it
/// serving once they listen.
static STOP: std::sync::OnceLock<helios_observability::shutdown::StopGuard> =
    std::sync::OnceLock::new();

fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run());
    // Dropping the runtime would wait without limit for blocking tasks still
    // running (SQLite queries, an import job); a stopped server must not
    // linger — and keep its port — for them.
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    result
}

/// Marks the server as listening (a stop signal now drains it).
fn serving() {
    if let Some(stop) = STOP.get() {
        stop.serving();
    }
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli
        .command
        .unwrap_or_else(|| Command::Run(HtsConfig::parse_from(["hts"])))
    {
        Command::Run(config) => {
            let mut config = config;
            config.storage_backend =
                effective_storage_backend(&config.storage_backend, &config.database_url);
            // Listen for SIGTERM/SIGINT before the startup work (bootstrap
            // imports, closure and FTS builds): as PID 1 in a container an
            // unhandled SIGTERM is discarded. `HTS_SHUTDOWN_TIMEOUT` bounds
            // the drain (default 9 s, under `docker stop`'s 10 s).
            let _ = STOP.set(helios_observability::shutdown::StopGuard::install(
                "hts",
                helios_observability::shutdown::parse_grace(
                    std::env::var("HTS_SHUTDOWN_TIMEOUT").ok().as_deref(),
                    std::time::Duration::from_secs(9),
                ),
            ));
            helios_observability::uptime::init();
            helios_observability::telemetry::init("hts", &config.log_level);
            helios_observability::metrics::init("hts");
            helios_observability::build_info::set(
                env!("CARGO_PKG_VERSION"),
                option_env!("HFS_GIT_SHA").unwrap_or("unknown"),
            );
            info!(
                port = config.port,
                host = %config.host,
                storage_backend = %config.storage_backend,
                database_url = %redact_database_url(&config.database_url, &config.storage_backend),
                // Eagerly resolve the HELIOS_OBS_MODE OnceLock at boot and stamp
                // it into the startup line. The same-session A/B benchmark (#297)
                // greps this to assert each arm's server actually resolved to the
                // arm it set — a server that silently ignored the env var (e.g.
                // built without the observability arm switch) is caught here
                // rather than producing a clean-looking table of ~zero deltas.
                obs_mode = ?helios_observability::mode::mode(),
                "Starting Helios Terminology Server"
            );
            run_server(config).await
        }
        Command::Migrate(args) => {
            // Schema DDL replays as NOTICEs that tokio-postgres logs at INFO;
            // keep them out of the migration log unless asked for explicitly.
            let filter = if args.log_level.contains('=') {
                args.log_level.clone()
            } else {
                format!("{},tokio_postgres=warn", args.log_level)
            };
            helios_observability::telemetry::init("hts", &filter);
            run_migrate(args).await
        }
        Command::Import(args) => {
            let mut args = args;
            args.storage_backend =
                effective_storage_backend(&args.storage_backend, &args.database_url);
            let log_level = if args.verbose {
                "debug"
            } else {
                &args.log_level
            };
            helios_observability::telemetry::init("hts", log_level);
            let code = run_import(args).await?;
            std::process::exit(code);
        }
    }
}

// ── Migrate ───────────────────────────────────────────────────────────────────

#[cfg(all(feature = "sqlite", feature = "postgres"))]
async fn run_migrate(args: MigrateArgs) -> anyhow::Result<()> {
    use helios_hts::migrate::{MigrateOptions, migrate_sqlite_to_postgres};
    let opts = MigrateOptions {
        from: args.from,
        to: args.to,
        replace: args.replace,
        skip_resources: args.skip_resources,
        jobs: args.jobs,
    };
    let report = migrate_sqlite_to_postgres(&opts).await?;
    println!(
        "{:<24} {:>12} {:>12} {:>9}",
        "table", "source", "copied", "seconds"
    );
    for t in &report.tables {
        println!(
            "{:<24} {:>12} {:>12} {:>9.1}",
            t.table, t.source_rows, t.copied_rows, t.seconds
        );
    }
    println!(
        "resources: {}  NUL chars removed: {}  unparseable JSON: {}  total: {:.1}s",
        report.resources, report.nul_chars_removed, report.json_unparseable, report.seconds
    );
    if let Some(path) = args.report {
        std::fs::write(&path, serde_json::to_vec_pretty(&report)?)?;
    }
    let bad = report.mismatches();
    if !bad.is_empty() {
        anyhow::bail!(
            "row counts differ for: {}",
            bad.iter()
                .map(|t| t.table.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

#[cfg(not(all(feature = "sqlite", feature = "postgres")))]
async fn run_migrate(_args: MigrateArgs) -> anyhow::Result<()> {
    anyhow::bail!("`hts migrate` needs both the `sqlite` and `postgres` features")
}

// ── Server ────────────────────────────────────────────────────────────────────

#[cfg(feature = "sqlite")]
async fn run_server(config: HtsConfig) -> anyhow::Result<()> {
    if config.storage_backend == "postgres" {
        #[cfg(feature = "postgres")]
        return run_server_postgres(config).await;
        #[cfg(not(feature = "postgres"))]
        anyhow::bail!(
            "postgres storage backend requested but the 'postgres' feature is not enabled. \
             Rebuild with `--features postgres`."
        );
    }

    use helios_persistence::backends::sqlite::SqliteBackend;

    // Defer the FTS prebuild until after bootstrap so the index is built once
    // on the final post-import data rather than on stale data the bootstrap
    // sync is about to change.
    let backend = SqliteTerminologyBackend::new_without_fts_prebuild(&config.database_url)?;
    let hts_pool = backend.pool().clone();

    helios_hts::bootstrap::bootstrap_sync_sqlite(&backend, &config).await?;

    // Rebuild closures invalidated by bootstrap re-import and materialize the
    // FTS index on the final data, matching the post-import steps the CLI
    // `hts import` path performs. Runs before the server accepts requests.
    backend
        .finalize_after_bootstrap()
        .map_err(|e| anyhow::anyhow!("post-bootstrap finalization failed: {e}"))?;

    let resource_store = SqliteBackend::open(&config.database_url)
        .map_err(|e| anyhow::anyhow!("Failed to open resource store: {e}"))?;
    resource_store
        .init_schema()
        .map_err(|e| anyhow::anyhow!("Failed to initialize resource store schema: {e}"))?;

    info!("Resource store (helios-persistence) initialized alongside HTS backend");

    let state = AppState::new(backend)
        .with_resource_store(resource_store)
        .with_hts_pool(hts_pool)
        .with_max_expansion_size(config.max_expansion_size)
        .with_auto_supplements(config.auto_supplements);
    let audit = helios_hts::audit::init(&config, state.resource_store.clone()).await?;
    audit_startup(&audit, "sqlite").await;
    let state = state.with_audit(audit.clone());

    helios_hts::explore::spawn_warmup(state.clone());
    let app = helios_hts::server::create_app(&config, state);

    let addr = config.socket_addr();
    info!(address = %addr, "HTS listening");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    serving();
    axum::serve(
        listener,
        // Peer addresses feed the admin audit trail (`admin::audit`).
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let signal = helios_observability::shutdown::signal().await;
        info!(signal, "Shutdown signal received, draining connections");
    })
    .await?;
    audit_shutdown(&audit).await;
    // After the drain, so spans from the draining requests are exported too.
    helios_observability::telemetry::shutdown();
    Ok(())
}

#[cfg(not(feature = "sqlite"))]
async fn run_server(config: HtsConfig) -> anyhow::Result<()> {
    if config.storage_backend == "postgres" {
        #[cfg(feature = "postgres")]
        return run_server_postgres(config).await;
        #[cfg(not(feature = "postgres"))]
        anyhow::bail!(
            "No storage backend feature is enabled. \
             Rebuild with `--features sqlite` or `--features postgres`."
        );
    }
    anyhow::bail!(
        "No storage backend feature is enabled. \
         Rebuild with `--features sqlite` (or another backend feature)."
    )
}

#[cfg(feature = "postgres")]
async fn run_server_postgres(config: HtsConfig) -> anyhow::Result<()> {
    use helios_persistence::backends::postgres::PostgresBackend;
    use std::sync::Arc;

    let backend = PostgresTerminologyBackend::new(&config.database_url).await?;

    helios_hts::bootstrap::bootstrap_sync_postgres(&backend, &config).await?;

    // Rebuild any concept closures invalidated by bootstrap re-import, matching
    // the post-import step the CLI `hts import` path performs. Without this a
    // bootstrapped server can come up with chunked SNOMED/LOINC closure data
    // wiped during import and never rebuilt. Idempotent — only systems missing
    // closure rows are recomputed.
    let rebuilt = backend
        .rebuild_missing_closures()
        .await
        .map_err(|e| anyhow::anyhow!("post-bootstrap closure rebuild failed: {e}"))?;
    if rebuilt > 0 {
        info!(
            systems = rebuilt,
            "Rebuilt concept closures after bootstrap"
        );
    }

    let resource_store = PostgresBackend::from_connection_string(&config.database_url)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to open PostgreSQL resource store: {e}"))?;
    resource_store.init_schema().await.map_err(|e| {
        anyhow::anyhow!("Failed to initialize PostgreSQL resource store schema: {e}")
    })?;

    info!("PostgreSQL resource store (helios-persistence) initialized");

    // `postgres_pool_*{pool="terminology"|"resources"}` on /metrics, sampled
    // at scrape time like HFS's `primary` / `audit` pools (#1907).
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
        register_postgres_pool("resources", move || gauges(resources.pool_status()));
    }

    let state = helios_hts::state::AppState::new(backend.clone())
        .with_resource_store_pg(resource_store)
        .with_terminology_importer(Arc::new(backend.clone()))
        .with_max_expansion_size(config.max_expansion_size)
        .with_auto_supplements(config.auto_supplements);
    let audit = helios_hts::audit::init(&config, state.resource_store_pg.clone()).await?;
    audit_startup(&audit, "postgres").await;
    let state = state.with_audit(audit.clone());

    // Administrator sessions shared by every replica (and kept across restarts).
    match helios_admin_auth::store::postgres::PgAdminSessionStore::connect(
        backend.pool().clone(),
        "HTS",
    )
    .await
    {
        Ok(store) => {
            let _ = helios_hts::server::ADMIN_SESSION_STORE.set(Arc::new(store));
        }
        Err(e) => tracing::warn!(
            error = %e,
            "admin session store unavailable; administrator sessions stay per instance"
        ),
    }

    // Cluster cache coherence: drop every response cache when another replica
    // (or `hts import`) moved the shared terminology epoch.
    {
        use helios_hts::traits::TerminologyCaches;
        let st = state.clone();
        helios_hts::backends::postgres::epoch::spawn_cache_sync(
            backend,
            std::time::Duration::from_millis(config.cache_sync_interval_ms),
            Arc::new(move || {
                st.clear_expand_cache();
                st.backend().invalidate_caches();
            }),
        );
        if config.cache_sync_interval_ms > 0 {
            info!(
                interval_ms = config.cache_sync_interval_ms,
                "terminology cache sync ENABLED (shared terminology_epoch)"
            );
        }
    }

    helios_hts::explore::spawn_warmup(state.clone());
    let app = helios_hts::server::create_app(&config, state);

    let addr = config.socket_addr();
    info!(address = %addr, "HTS (PostgreSQL) listening");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    serving();
    axum::serve(
        listener,
        // Peer addresses feed the admin audit trail (`admin::audit`).
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let signal = helios_observability::shutdown::signal().await;
        info!(signal, "Shutdown signal received, draining connections");
    })
    .await?;
    audit_shutdown(&audit).await;
    // After the drain, so spans from the draining requests are exported too.
    helios_observability::telemetry::shutdown();
    Ok(())
}

/// Records the start-up `AuditEvent` (storage backend, audit settings).
#[cfg(any(feature = "sqlite", feature = "postgres"))]
async fn audit_startup(audit: &Option<helios_hts::audit::HtsAudit>, storage: &str) {
    if let Some(a) = audit {
        helios_audit::lifecycle::record_startup(
            &*a.middleware.sink,
            &a.middleware.config.source_observer,
            vec![
                ("storage-backend", storage.to_string()),
                ("audit-backend", a.middleware.config.backend.to_string()),
                ("audit-reads", a.middleware.config.reads.to_string()),
            ],
        )
        .await;
    }
}

/// After the HTTP drain: records the shutdown event and drains the audit
/// queue (spilling what does not fit in `HTS_AUDIT_SHUTDOWN_TIMEOUT`).
#[cfg(any(feature = "sqlite", feature = "postgres"))]
async fn audit_shutdown(audit: &Option<helios_hts::audit::HtsAudit>) {
    if let Some(a) = audit {
        helios_audit::lifecycle::record_shutdown(
            &*a.middleware.sink,
            &a.middleware.config.source_observer,
        )
        .await;
        a.middleware.sink.flush().await;
    }
}

// ── Bootstrap: see `helios_hts::bootstrap` (shared with the embedded service).

// ── Import ────────────────────────────────────────────────────────────────────

/// Returns the process exit code:
/// - `0` — success, all resources imported
/// - `1` — fatal error (propagated as `Err` by `?`)
/// - `2` — success with non-fatal errors (some records skipped)
///
/// `args.path` may be a file (imports one format, optionally overridden by
/// `--format`) or a directory (iterates entries, auto-detecting each file's
/// format; `--format` is rejected in this mode).
#[cfg(any(feature = "sqlite", feature = "postgres"))]
async fn run_import(args: ImportArgs) -> anyhow::Result<i32> {
    if !args.path.exists() {
        anyhow::bail!("Path does not exist: '{}'", args.path.display());
    }

    let is_dir = args.path.is_dir();

    // RxNorm's importer takes a directory containing RXNCONSO.RRF and
    // friends, so treat such a directory as a single-format import. Every
    // other directory means "iterate and auto-detect per file."
    let rxnorm_dir = is_dir
        && (args.format == Some(ImportFormat::Rxnorm)
            || helios_hts::bootstrap::contains_rxnorm_rrf(&args.path));

    if is_dir && !rxnorm_dir && args.format.is_some() {
        anyhow::bail!(
            "--format is not compatible with a directory path; each file's \
             format is auto-detected. Point --format at a single file, or \
             remove --format to iterate the directory."
        );
    }

    if args.dry_run {
        eprintln!("[hts-import] dry-run mode — parsing only, no changes will be written");
    }
    if args.verbose {
        eprintln!("[hts-import] verbose mode enabled — debug output active");
    }

    info!(
        path = %args.path.display(),
        is_dir,
        database_url = %redact_database_url(&args.database_url, &args.storage_backend),
        storage_backend = %args.storage_backend,
        dry_run = args.dry_run,
        verbose = args.verbose,
        "Starting bulk import"
    );

    let started = std::time::Instant::now();
    let ctx = TenantContext::system();

    let (stats, label) = if args.storage_backend == "postgres" {
        #[cfg(feature = "postgres")]
        {
            let backend = PostgresTerminologyBackend::new(&args.database_url).await?;
            let result = run_import_for_path(&backend, &ctx, &args, rxnorm_dir).await?;

            // Pre-build concept closures now so server startup only needs to
            // observe them present (idempotent migration sees no work). Without
            // this the first ?fhir_vs=isa request would block on a SNOMED
            // closure build, blowing past the 60 s health-check timeout.
            if !args.dry_run {
                info!("Building concept closures (this may take ~30–60 s for SNOMED CT)…");
                let rebuilt = backend
                    .rebuild_missing_closures()
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to build concept closures: {e}"))?;
                info!(systems = rebuilt, "Concept closures ready");
            }

            result
        }
        #[cfg(not(feature = "postgres"))]
        anyhow::bail!(
            "postgres storage backend requested but the 'postgres' feature is not enabled. \
             Rebuild with `--features postgres`."
        );
    } else {
        #[cfg(feature = "sqlite")]
        {
            // Dry-run uses an in-memory database so the pool opens without
            // requiring the target DB file or its parent directory.
            let database_url = if args.dry_run {
                ":memory:".to_string()
            } else {
                args.database_url.clone()
            };
            let backend = SqliteTerminologyBackend::new(&database_url)?;
            let result = run_import_for_path(&backend, &ctx, &args, rxnorm_dir).await?;

            // Finalize the database now — build concept closures AND the
            // concepts FTS index — so the on-disk DB ships fully serve-ready.
            // Server startup then finds everything already built (tracked in
            // concepts_fts_built + closure rows present) and its
            // finalize_after_bootstrap is a near-instant no-op, instead of
            // re-tokenising the whole corpus inside the health-check window
            // (issue #295). Previously only closures were pre-built here, which
            // left the full FTS rebuild to block server startup on every boot.
            // Runs on a blocking thread (heavy synchronous SQLite work).
            if !args.dry_run {
                info!(
                    "Building concept closures + FTS index (this may take ~40–60 s for SNOMED CT)…"
                );
                tokio::task::spawn_blocking(move || backend.finalize_after_bootstrap())
                    .await
                    .map_err(|e| anyhow::anyhow!("task join error: {e}"))?
                    .map_err(|e| anyhow::anyhow!("failed to finalize terminology database: {e}"))?;
                info!("Concept closures + FTS index ready");
            }

            result
        }
        #[cfg(not(feature = "sqlite"))]
        anyhow::bail!(
            "sqlite storage backend requested but the 'sqlite' feature is not enabled. \
             Rebuild with `--features sqlite`."
        );
    };

    let result = ImportResult::new(stats, label.clone(), started.elapsed());
    print_import_summary(&result, args.dry_run, &label);

    if !result.stats.errors.is_empty() {
        return Ok(2);
    }
    Ok(0)
}

/// Dispatch either a single-file or directory import against an open backend.
/// Returns `(aggregated-stats, label-for-summary)`.
#[cfg(any(feature = "sqlite", feature = "postgres"))]
async fn run_import_for_path(
    backend: &dyn BundleImportBackend,
    ctx: &TenantContext,
    args: &ImportArgs,
    rxnorm_dir: bool,
) -> Result<(ImportStats, String), helios_hts::error::HtsError> {
    use helios_hts::error::HtsError;

    let languages = LanguageFilter::parse(&args.languages);

    if args.path.is_dir() && !rxnorm_dir {
        return import_directory(backend, ctx, args, &languages).await;
    }

    let format = match args.format {
        Some(f) => f,
        None => detect_format(&args.path).ok_or_else(|| {
            HtsError::InvalidRequest(format!(
                "Cannot auto-detect format from '{}'. \
                 Use --format to specify one of: hl7-npm, snomed-rf2, loinc, \
                 icd10-cm, icd10-pcs, icd9-cm, rxnorm, ucum, nci-thesaurus, mesh, \
                 dicom, hl7-v2-tables, nucc, ndc, fhir-bundle. Note: .zip files may \
                 require --format if auto-detection is ambiguous.",
                args.path.display()
            ))
        })?,
    };

    let stats = dispatch_import(
        format,
        backend,
        ctx,
        &args.path,
        args.batch_size,
        args.dry_run,
        &languages,
    )
    .await?;
    Ok((stats, format.to_string()))
}

/// Iterate a directory, auto-detect each file's format, dispatch import for
/// each, and aggregate stats. Files whose format can't be auto-detected and
/// hidden files (dotfiles) are skipped with a warning.
#[cfg(any(feature = "sqlite", feature = "postgres"))]
async fn import_directory(
    backend: &dyn BundleImportBackend,
    ctx: &TenantContext,
    args: &ImportArgs,
    languages: &LanguageFilter,
) -> Result<(ImportStats, String), helios_hts::error::HtsError> {
    let (stats, imported_count, skipped_count) = helios_hts::bootstrap::import_directory_files(
        backend,
        ctx,
        &args.path,
        args.batch_size,
        args.dry_run,
        languages,
        None,
    )
    .await?;
    let label = format!("directory ({imported_count} imported, {skipped_count} skipped)");
    Ok((stats, label))
}

/// Shared directory-iteration loop used by both `hts import <dir>` and the
/// `HTS_BOOTSTRAP_DIR` sync path. Returns (aggregated stats, imported file
/// count, skipped file count).
///
/// When `tracker` is `Some`, each file's SHA-256 content signature is compared
/// against the `bootstrap_imports` ledger: files whose signature is unchanged
/// are skipped (counted as skipped), and successfully imported files have their
/// signature recorded. When `tracker` is `None` (CLI `hts import <dir>`), every
/// recognized file is imported unconditionally.

#[cfg(not(any(feature = "sqlite", feature = "postgres")))]
async fn run_import(_args: ImportArgs) -> anyhow::Result<i32> {
    anyhow::bail!(
        "No storage backend feature is enabled. \
         Rebuild with `--features sqlite` or `--features postgres`."
    )
}

// ── Shared helpers ────────────────────────────────────────────────────────────

#[cfg(any(feature = "sqlite", feature = "postgres"))]
fn print_import_summary(result: &helios_hts::import::ImportResult, dry_run: bool, format: &str) {
    let dry_label = if dry_run { " (dry-run)" } else { "" };
    println!(
        "Import complete{dry_label} [{format}] in {:.1}s: \
         {} CodeSystems, {} ValueSets, {} ConceptMaps, {} concepts",
        result.duration.as_secs_f64(),
        result.stats.code_systems,
        result.stats.value_sets,
        result.stats.concept_maps,
        result.stats.concepts,
    );
    if !result.stats.errors.is_empty() {
        eprintln!(
            "Non-fatal errors ({}); first few shown below:",
            result.stats.errors.len()
        );
        for e in result.stats.errors.iter().take(10) {
            eprintln!("  - {e}");
        }
    }
    if !result.stats.warnings.is_empty() {
        eprintln!("Warnings ({}):", result.stats.warnings.len());
        for w in result.stats.warnings.iter().take(20) {
            eprintln!("  - {w}");
        }
    }
}
