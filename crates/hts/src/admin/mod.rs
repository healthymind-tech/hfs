//! Terminology management: upload, import, delete and maintenance.
//!
//! Everything here is an *administrative* surface and is mounted only when
//! `HTS_ADMIN_UI=true` (default off) — until SMART/OIDC lands, that flag is the
//! access control, so deployments must keep `/admin` behind the same network
//! boundary as the console. See `docs/hts-terminology-admin.md` for the API.
//!
//! | Piece | Module |
//! |---|---|
//! | Chunked resumable uploads, server import directory | [`uploads`] |
//! | Format registry and archive inspection | [`formats`] |
//! | Background job queue (one writer at a time) | [`jobs`] |
//! | Storage operations (inventory, delete, pin, vacuum) | [`ops`] |
//! | packages.fhir.org client | [`registry`] |
//! | AuditEvent trail | [`audit`] |
//! | HTTP routes | [`routes`] |

pub mod audit;
pub mod disk;
pub mod formats;
pub mod jobs;
pub mod ops;
pub mod registry;
pub mod routes;
pub mod uploads;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::HtsError;
use crate::import::BundleImportBackend;
use helios_persistence::ResourceStorage;

pub use ops::TerminologyAdmin;

/// Errors of the admin API (plain JSON, not OperationOutcome: this is not a
/// FHIR surface).
#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    TooLarge(String),
    #[error("insufficient storage")]
    InsufficientStorage(serde_json::Value),
    #[error("{0}")]
    Disabled(String),
    #[error("{0}")]
    Upstream(String),
    #[error("{0}")]
    Internal(String),
}

impl From<HtsError> for AdminError {
    fn from(e: HtsError) -> Self {
        match e {
            HtsError::NotFound(m) => AdminError::NotFound(m),
            HtsError::InvalidRequest(m) => AdminError::BadRequest(m),
            other => AdminError::Internal(other.to_string()),
        }
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            AdminError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad-request"),
            AdminError::NotFound(_) => (StatusCode::NOT_FOUND, "not-found"),
            AdminError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            AdminError::TooLarge(_) => (StatusCode::PAYLOAD_TOO_LARGE, "too-large"),
            AdminError::InsufficientStorage(_) => {
                (StatusCode::INSUFFICIENT_STORAGE, "insufficient-storage")
            }
            AdminError::Disabled(_) => (StatusCode::SERVICE_UNAVAILABLE, "disabled"),
            AdminError::Upstream(_) => (StatusCode::BAD_GATEWAY, "upstream"),
            AdminError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        let mut body = json!({ "error": code, "message": self.to_string() });
        if let AdminError::InsufficientStorage(detail) = &self {
            body["space"] = detail.clone();
        }
        (status, Json(body)).into_response()
    }
}

/// Runtime configuration of the admin surface (from [`crate::config::HtsConfig`]).
#[derive(Debug, Clone)]
pub struct AdminConfig {
    /// Root for uploads, job records/logs, downloaded packages, the audit log.
    pub data_dir: PathBuf,
    /// Operator-managed directory of source files (`HTS_IMPORT_DIR`).
    pub import_dir: Option<PathBuf>,
    /// Package registry base URL; `None` = disabled.
    pub registry_url: Option<String>,
    pub chunk_size: u64,
    pub max_upload_size: u64,
    /// Default importer batch size.
    pub batch_size: usize,
    /// Default `HTS_IMPORT_LANGUAGES`.
    pub languages: String,
    /// Extra free space required on top of every growth estimate.
    pub disk_headroom: u64,
    /// Directory holding the database file (SQLite) for space checks.
    pub db_dir: Option<PathBuf>,
}

impl AdminConfig {
    /// Derive from the server config.
    pub fn from_hts(c: &crate::config::HtsConfig) -> Self {
        let db_dir = (c.storage_backend != "postgres")
            .then(|| {
                std::path::Path::new(&c.database_url).parent().map(|p| {
                    if p.as_os_str().is_empty() {
                        PathBuf::from(".")
                    } else {
                        p.to_path_buf()
                    }
                })
            })
            .flatten();
        let data_dir = if c.admin_data_dir.trim().is_empty() {
            db_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("./data"))
                .join("hts-admin")
        } else {
            PathBuf::from(c.admin_data_dir.trim())
        };
        let registry_url = match c.package_registry.trim() {
            "" | "off" | "false" | "none" => None,
            url => Some(url.to_string()),
        };
        Self {
            data_dir,
            import_dir: (!c.import_dir.trim().is_empty())
                .then(|| PathBuf::from(c.import_dir.trim())),
            registry_url,
            chunk_size: c.upload_chunk_size,
            max_upload_size: c.max_upload_size,
            batch_size: c.bootstrap_batch_size.max(1),
            languages: c.import_languages.clone(),
            disk_headroom: c.disk_headroom,
            db_dir,
        }
    }
}

/// Where a stored CodeSystem version came from (written by import jobs).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Provenance {
    pub job_id: String,
    pub source: String,
    pub sha256: Option<String>,
    pub format: String,
    pub imported_at: String,
    /// Measured database growth of the import job that wrote it.
    pub db_growth_bytes: Option<i64>,
}

/// `url|version` → provenance, persisted as `provenance.json`.
pub struct ProvenanceStore {
    path: PathBuf,
    map: Mutex<HashMap<String, Provenance>>,
}

impl ProvenanceStore {
    fn load(path: PathBuf) -> Self {
        let map = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            path,
            map: Mutex::new(map),
        }
    }

    pub fn key(url: &str, version: Option<&str>) -> String {
        format!("{url}|{}", version.unwrap_or(""))
    }

    pub fn get(&self, url: &str, version: Option<&str>) -> Option<Provenance> {
        self.map.lock().ok()?.get(&Self::key(url, version)).cloned()
    }

    pub fn set_many(&self, entries: Vec<(String, Provenance)>) {
        if let Ok(mut m) = self.map.lock() {
            for (k, v) in entries {
                m.insert(k, v);
            }
            self.persist(&m);
        }
    }

    pub fn remove(&self, url: &str, version: Option<&str>) {
        if let Ok(mut m) = self.map.lock() {
            m.remove(&Self::key(url, version));
            self.persist(&m);
        }
    }

    fn persist(&self, m: &HashMap<String, Provenance>) {
        let tmp = self.path.with_extension("json.tmp");
        if let Ok(bytes) = serde_json::to_vec_pretty(m)
            && std::fs::write(&tmp, bytes).is_ok()
        {
            let _ = std::fs::rename(tmp, &self.path);
        }
    }
}

/// Cached [`ops::SystemStats`]: scanning the concept tables takes seconds on a
/// production database, so the inventory reuses the last scan until a write
/// job invalidates it.
/// `(computed_at, storage id → stats)`.
pub type StatsSnapshot = (String, Arc<HashMap<String, ops::SystemStats>>);

#[derive(Default)]
pub struct StatsCache {
    pub value: tokio::sync::RwLock<Option<StatsSnapshot>>,
    pub refresh: tokio::sync::Mutex<()>,
}

/// Shared state of the admin surface.
pub struct AdminCore {
    pub config: AdminConfig,
    pub ops: Arc<dyn TerminologyAdmin>,
    pub importer: Arc<dyn BundleImportBackend>,
    pub resource_store: Option<Arc<dyn ResourceStorage>>,
    pub uploads: uploads::UploadStore,
    pub jobs: jobs::JobManager,
    pub registry: registry::Registry,
    pub audit: audit::AuditLog,
    pub provenance: ProvenanceStore,
    pub stats: StatsCache,
    /// Clears the AppState handler caches (`$expand`/`$lookup` responses);
    /// the backend tier is cleared by the ops themselves.
    pub on_change: Arc<dyn Fn() + Send + Sync>,
}

impl AdminCore {
    /// Build the core and create its directories. Does not start the worker;
    /// call [`jobs::start_worker`].
    pub fn new(
        config: AdminConfig,
        ops: Arc<dyn TerminologyAdmin>,
        importer: Arc<dyn BundleImportBackend>,
        resource_store: Option<Arc<dyn ResourceStorage>>,
        on_change: Arc<dyn Fn() + Send + Sync>,
    ) -> std::io::Result<Arc<Self>> {
        std::fs::create_dir_all(config.data_dir.join("uploads"))?;
        std::fs::create_dir_all(config.data_dir.join("jobs"))?;
        std::fs::create_dir_all(config.data_dir.join("packages"))?;
        let core = Self {
            uploads: uploads::UploadStore::new(
                config.data_dir.join("uploads"),
                config.chunk_size,
                config.max_upload_size,
            ),
            jobs: jobs::JobManager::load(config.data_dir.join("jobs")),
            registry: registry::Registry::new(config.registry_url.clone()),
            audit: audit::AuditLog::new(config.data_dir.join("audit.ndjson")),
            provenance: ProvenanceStore::load(config.data_dir.join("provenance.json")),
            stats: StatsCache::default(),
            config,
            ops,
            importer,
            resource_store,
            on_change,
        };
        Ok(Arc::new(core))
    }

    /// Drop cached stats (after any write job).
    pub async fn invalidate_stats(&self) {
        *self.stats.value.write().await = None;
    }

    /// Current per-system stats, computing them if the cache is empty.
    pub async fn system_stats(&self) -> Result<StatsSnapshot, AdminError> {
        if let Some(v) = self.stats.value.read().await.clone() {
            return Ok(v);
        }
        let _guard = self.stats.refresh.lock().await;
        if let Some(v) = self.stats.value.read().await.clone() {
            return Ok(v);
        }
        let stats = Arc::new(self.ops.system_stats().await?);
        let v = (chrono::Utc::now().to_rfc3339(), stats);
        *self.stats.value.write().await = Some(v.clone());
        Ok(v)
    }
}
