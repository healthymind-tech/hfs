//! FHIR `AuditEvent` audit trail for HTS (`HTS_AUDIT_*`).
//!
//! HTS uses the same machinery as HFS (`helios-audit`): the REST middleware
//! records each terminology request as an IHE BALP event (a CodeSystem /
//! ValueSet / ConceptMap create, update or delete is a BALP Create / Update /
//! Delete on that resource; `/import` and the admin API are recorded
//! unprofiled), the admin actions and their background jobs (uploads,
//! imports, package installs, deletes) add an application-activity event
//! each, and everything goes through the batching queue into a
//! `helios-persistence` store.
//!
//! **Where events are stored.** By default in HTS's own database — the
//! resource store HTS already keeps beside its terminology tables — and
//! searchable at `GET /AuditEvent` / `GET /AuditEvent/{id}` (administrator
//! only). With `HTS_AUDIT_DATABASE_URL` pointing at HFS's database (and
//! `HTS_AUDIT_TENANT` at the HFS tenant, `default`), HTS writes into HFS's
//! audit store instead, so one `GET /AuditEvent` on HFS covers both servers
//! and HFS's retention applies to both.
//!
//! **Reads** (`GET`, `$expand`, `$lookup`, `$validate-code`, ...) are not
//! recorded by default: they are the bulk of terminology traffic and carry no
//! patient data. `HTS_AUDIT_READS=true` records them too.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use helios_audit::{AuditBackend, AuditConfig, AuditMiddlewareState, AuditSink, AuditUser};
use helios_persistence::audit_store::{
    AuditBatchTransaction, AuditEventSearch, AuditSearchError, TxAuditStore,
};
use helios_persistence::core::{RetentionStorage, SearchProvider};
use serde_json::{Value, json};

/// The audit wiring of a running HTS.
#[derive(Clone)]
pub struct HtsAudit {
    /// Middleware state (sink, configuration).
    pub middleware: Arc<AuditMiddlewareState>,
    /// The searchable store, when events go to a database.
    pub search: Option<Arc<dyn AuditEventSearch>>,
}

/// Builds the audit trail from `HTS_AUDIT_*`. `own_store` is HTS's resource
/// store, used unless `HTS_AUDIT_DATABASE_URL` names another database.
/// `None` when `HTS_AUDIT_BACKEND` is unset/`none`.
pub async fn init<B>(
    config: &crate::config::HtsConfig,
    own_store: Option<Arc<B>>,
) -> anyhow::Result<Option<HtsAudit>>
where
    B: AuditBatchTransaction + SearchProvider + RetentionStorage + Send + Sync + 'static,
{
    let audit = AuditConfig::from_env_prefixed("HTS");
    let fallback = audit
        .fallback_file
        .clone()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            crate::admin::AdminConfig::from_hts(config)
                .data_dir
                .join("audit-fallback.ndjson")
        });
    let mut search: Option<Arc<dyn AuditEventSearch>> = None;
    let sink: Arc<dyn AuditSink> = match audit.backend {
        AuditBackend::None => {
            tracing::info!("HTS audit logging is DISABLED (HTS_AUDIT_BACKEND)");
            return Ok(None);
        }
        AuditBackend::File => {
            let path = audit.file_path.clone().ok_or_else(|| {
                anyhow::anyhow!("HTS_AUDIT_FILE_PATH is required when HTS_AUDIT_BACKEND=file")
            })?;
            Arc::new(helios_audit::FileSink::new(path).await?)
        }
        AuditBackend::Database => {
            let (storage, searcher, retention) = match audit.database_url.as_deref() {
                Some(url) => open_dedicated(url, &audit).await?,
                None => {
                    let store = own_store.ok_or_else(|| {
                        anyhow::anyhow!(
                            "HTS_AUDIT_BACKEND=database needs HTS's resource store or HTS_AUDIT_DATABASE_URL"
                        )
                    })?;
                    tx_store(store, &audit)
                }
            };
            search = Some(searcher);
            let db = helios_audit::DatabaseSink::new(storage, helios_fhir::FhirVersion::default())
                .with_fallback(&fallback);
            match db.replay_fallback().await {
                Ok(0) => {}
                Ok(n) => tracing::info!(
                    events = n,
                    "Replayed HTS audit events from the fallback file"
                ),
                Err(e) => {
                    tracing::warn!(error = %e, "Could not replay the HTS audit fallback file")
                }
            }
            let db: Arc<dyn AuditSink> = Arc::new(db);
            if let Some(store) = retention {
                helios_persistence::audit_store::retention::spawn(
                    store,
                    audit.retention.clone(),
                    Arc::clone(&db),
                    audit.source_observer.clone(),
                );
            }
            db
        }
        AuditBackend::CloudWatch => {
            anyhow::bail!("HTS_AUDIT_BACKEND=cloudwatch is not supported; use database or file")
        }
    };
    let mut queue = audit.queue_config();
    queue.spill_file.get_or_insert(fallback);
    let sink: Arc<dyn AuditSink> = Arc::new(helios_audit::QueuedSink::new(sink, queue));
    tracing::info!(
        backend = %audit.backend,
        reads = audit.reads,
        dedicated = audit.database_url.is_some(),
        "HTS audit logging is ENABLED"
    );
    let base = std::env::var("HTS_PUBLIC_BASE_URL")
        .unwrap_or_else(|_| format!("http://{}", config.socket_addr()));
    let middleware = Arc::new(AuditMiddlewareState::new(sink, audit).with_server_address(base));
    Ok(Some(HtsAudit { middleware, search }))
}

type Stores = (
    Arc<dyn helios_audit::sinks::database::AuditStorage>,
    Arc<dyn AuditEventSearch>,
    Option<Arc<dyn RetentionStorage>>,
);

fn tx_store<B>(backend: Arc<B>, audit: &AuditConfig) -> Stores
where
    B: AuditBatchTransaction + SearchProvider + RetentionStorage + Send + Sync + 'static,
{
    let store = Arc::new(TxAuditStore::new(
        Arc::clone(&backend),
        audit.tenant.clone(),
        "default".to_string(),
    ));
    (
        store.clone(),
        store,
        Some(backend as Arc<dyn RetentionStorage>),
    )
}

/// A dedicated audit database (`HTS_AUDIT_DATABASE_URL`): PostgreSQL by URL
/// scheme, else a SQLite file path. Typically HFS's own database.
async fn open_dedicated(url: &str, audit: &AuditConfig) -> anyhow::Result<Stores> {
    let lower = url.trim_start().to_ascii_lowercase();
    if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
        #[cfg(feature = "postgres")]
        {
            let backend =
                helios_persistence::backends::postgres::PostgresBackend::from_connection_string(
                    url,
                )
                .await
                .map_err(|e| anyhow::anyhow!("HTS_AUDIT_DATABASE_URL: {e}"))?;
            backend
                .init_schema()
                .await
                .map_err(|e| anyhow::anyhow!("HTS_AUDIT_DATABASE_URL schema: {e}"))?;
            return Ok(tx_store(Arc::new(backend), audit));
        }
        #[cfg(not(feature = "postgres"))]
        anyhow::bail!("HTS_AUDIT_DATABASE_URL is PostgreSQL but HTS was built without `postgres`");
    }
    let backend = helios_persistence::backends::sqlite::SqliteBackend::open(url)
        .map_err(|e| anyhow::anyhow!("HTS_AUDIT_DATABASE_URL: {e}"))?;
    backend
        .init_schema()
        .map_err(|e| anyhow::anyhow!("HTS_AUDIT_DATABASE_URL schema: {e}"))?;
    Ok(tx_store(Arc::new(backend), audit))
}

/// Reports the administrator a request was authenticated as (set by the
/// admin gate) to the audit context.
pub async fn capture_admin_user(request: Request, next: Next) -> Response {
    if let Some(user) = request.extensions().get::<helios_admin_auth::AdminUser>() {
        helios_audit::context::set_user(AuditUser {
            subject: user.0.clone(),
            issuer: Some("hts-admin".to_string()),
            name: Some(user.0.clone()),
            ..Default::default()
        });
    }
    next.run(request).await
}

/// `GET /AuditEvent` and `GET /AuditEvent/{id}` over the audit store.
pub fn router(search: Arc<dyn AuditEventSearch>) -> Router {
    Router::new()
        .route("/AuditEvent", get(search_handler))
        .route("/AuditEvent/{id}", get(read_handler))
        .with_state(search)
}

fn fhir_json(status: StatusCode, body: Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/fhir+json")],
        body.to_string(),
    )
        .into_response()
}

fn outcome(status: StatusCode, code: &str, text: &str) -> Response {
    fhir_json(
        status,
        json!({
            "resourceType": "OperationOutcome",
            "issue": [{"severity": "error", "code": code, "diagnostics": text}]
        }),
    )
}

async fn search_handler(
    State(search): State<Arc<dyn AuditEventSearch>>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    match search.search_audit_events(&params).await {
        Ok(events) => fhir_json(
            StatusCode::OK,
            json!({
                "resourceType": "Bundle",
                "type": "searchset",
                "entry": events
                    .into_iter()
                    .map(|e| json!({"resource": e, "search": {"mode": "match"}}))
                    .collect::<Vec<_>>()
            }),
        ),
        Err(AuditSearchError::BadRequest(m)) => {
            outcome(StatusCode::BAD_REQUEST, "not-supported", &m)
        }
        Err(AuditSearchError::Storage(m)) => {
            outcome(StatusCode::INTERNAL_SERVER_ERROR, "exception", &m)
        }
    }
}

async fn read_handler(
    State(search): State<Arc<dyn AuditEventSearch>>,
    Path(id): Path<String>,
) -> Response {
    match search.read_audit_event(&id).await {
        Ok(Some(e)) => fhir_json(StatusCode::OK, e),
        Ok(None) => outcome(
            StatusCode::NOT_FOUND,
            "not-found",
            &format!("AuditEvent/{id} is not known"),
        ),
        Err(e) => outcome(
            StatusCode::INTERNAL_SERVER_ERROR,
            "exception",
            &e.to_string(),
        ),
    }
}

#[cfg(test)]
#[path = "audit_hm_tests.rs"]
mod hm_tests;
