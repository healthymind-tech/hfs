//! HTTP surface of the admin API (`/admin/...`).
//!
//! Plain JSON in and out. Reads answer directly; anything that writes
//! terminology content is turned into a job (`202 Accepted` + the job) so
//! the single-writer queue in [`super::jobs`] serialises it. The only
//! synchronous write is pinning the current version, a one-row update.
//!
//! The same router is mounted twice by `server::create_app`: at `/admin`
//! (operators, scripts — `/tx/admin` behind the reference nginx) and at
//! `/ui/hts/api/admin`, so the console's browser code reaches it on the
//! console's own origin and path prefix.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::audit::{Actor, Outcome};
use super::jobs::{ImportSource, JobSpec};
use super::ops::{CodeSystemRow, SystemStats, effective_current};
use super::{AdminCore, AdminError, formats, uploads};

type Core = State<Arc<AdminCore>>;
type ApiResult = Result<Response, AdminError>;

/// Build the admin router (state already applied).
pub fn router(core: Arc<AdminCore>) -> Router {
    let chunk_limit = (core.config.chunk_size as usize).saturating_add(1024 * 1024);
    Router::new()
        .route("/", get(status))
        .route("/formats", get(list_formats))
        .route("/terminology", get(inventory))
        .route("/terminology/refresh-stats", post(refresh_stats))
        .route("/code-systems", axum::routing::delete(delete_code_system_q))
        .route("/code-systems/delete", post(delete_code_system_body))
        .route("/code-systems/dependents", get(dependents))
        .route("/code-systems/current", put(set_current))
        .route("/value-sets", axum::routing::delete(delete_value_set))
        .route("/concept-maps", axum::routing::delete(delete_concept_map))
        .route("/maintenance/compact", post(compact))
        .route("/uploads", get(list_uploads).post(create_upload))
        .route("/uploads/{id}", get(upload_status).delete(delete_upload))
        .route(
            "/uploads/{id}/chunks/{index}",
            put(put_chunk).layer(DefaultBodyLimit::max(chunk_limit)),
        )
        .route("/uploads/{id}/complete", post(complete_upload))
        .route("/uploads/{id}/detect", get(detect_upload))
        .route("/import-dir", get(list_import_dir))
        .route("/import-dir/detect", get(detect_import_dir))
        .route("/imports", post(start_import))
        .route("/registry/search", get(registry_search))
        .route("/registry/packages/{name}", get(registry_package))
        .route("/registry/install", post(registry_install))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{id}", get(get_job))
        .route("/jobs/{id}/log", get(job_log))
        .route("/jobs/{id}/cancel", post(cancel_job))
        .route("/audit", get(audit))
        .with_state(core)
}

fn accepted(job: super::jobs::Job) -> Response {
    (StatusCode::ACCEPTED, Json(job)).into_response()
}

// ── Status / formats ──────────────────────────────────────────────────────────

async fn status(State(core): Core) -> ApiResult {
    let fp = core.ops.footprint().await.ok();
    let free_db = core
        .config
        .db_dir
        .as_deref()
        .and_then(super::disk::free_bytes);
    let free_data = super::disk::free_bytes(&core.config.data_dir);
    Ok(Json(json!({
        "enabled": true,
        "backend": core.ops.kind(),
        "data_dir": core.config.data_dir,
        "import_dir": core.config.import_dir,
        "registry": core.registry.base(),
        "chunk_size": core.config.chunk_size,
        "max_upload_size": core.config.max_upload_size,
        "batch_size": core.config.batch_size,
        "busy": core.jobs.busy(),
        "database": fp,
        "free_bytes": { "database": free_db, "data_dir": free_data },
    }))
    .into_response())
}

async fn list_formats() -> Json<Value> {
    Json(json!({ "formats": formats::registry() }))
}

// ── Inventory ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
struct InventoryQuery {
    q: Option<String>,
    /// `name` (default) or `size`.
    sort: Option<String>,
    limit: Option<usize>,
    /// Hide versions without concepts (stubs, `not-present` placeholders).
    #[serde(default)]
    populated: bool,
}

async fn inventory(State(core): Core, Query(q): Query<InventoryQuery>) -> ApiResult {
    let rows = core.ops.code_system_rows().await?;
    // Stats can take seconds on a cold production database; wait a bounded
    // time, then answer without them (the computation keeps going and fills
    // the cache for the next request).
    let stats_task = {
        let core = core.clone();
        tokio::spawn(async move { core.system_stats().await })
    };
    let (stats_at, stats) =
        match tokio::time::timeout(std::time::Duration::from_secs(8), stats_task).await {
            Ok(Ok(Ok((at, s)))) => (Some(at), Some(s)),
            _ => (None, None),
        };
    let fp = core.ops.footprint().await.ok();
    let (vs, cm) = core.ops.resource_counts().await.unwrap_or((0, 0));

    let needle =
        q.q.as_deref()
            .map(str::to_lowercase)
            .filter(|s| !s.is_empty());
    let mut by_url: BTreeMap<String, Vec<CodeSystemRow>> = BTreeMap::new();
    for r in rows {
        if let Some(n) = &needle {
            let hay = format!(
                "{} {} {}",
                r.url,
                r.name.as_deref().unwrap_or(""),
                r.title.as_deref().unwrap_or("")
            )
            .to_lowercase();
            if !hay.contains(n) {
                continue;
            }
        }
        by_url.entry(r.url.clone()).or_default().push(r);
    }

    let empty: HashMap<String, SystemStats> = HashMap::new();
    let stats_ref = stats.as_deref().unwrap_or(&empty);
    let mut systems: Vec<Value> = Vec::new();
    let mut total_concepts = 0u64;
    for (url, mut versions) in by_url {
        versions.sort_by(|a, b| {
            crate::version_order::compare_versions(b.version.as_deref(), a.version.as_deref())
        });
        // Without stats, treat every row as populated so the pin and
        // version order still decide.
        let assumed: HashMap<String, SystemStats>;
        let for_current = if stats.is_some() {
            stats_ref
        } else {
            assumed = versions
                .iter()
                .map(|r| {
                    (
                        r.id.clone(),
                        SystemStats {
                            concepts: 1,
                            ..Default::default()
                        },
                    )
                })
                .collect();
            &assumed
        };
        let refs: Vec<&CodeSystemRow> = versions.iter().collect();
        let current_id = effective_current(&refs, for_current).map(|r| r.id.clone());
        let mut sys_bytes = 0u64;
        let mut sys_concepts = 0u64;
        let vjson: Vec<Value> = versions
            .iter()
            .map(|r| {
                let s = stats_ref.get(&r.id);
                let prov = core.provenance.get(&r.url, r.version.as_deref());
                sys_bytes += s.map(|s| s.est_bytes).unwrap_or(0);
                sys_concepts += s.map(|s| s.concepts).unwrap_or(0);
                json!({
                    "id": r.id,
                    "version": r.version,
                    "name": r.name,
                    "title": r.title,
                    "status": r.status,
                    "content": r.content,
                    "supplements": r.supplements,
                    "pinned": r.pinned,
                    "current": current_id.as_deref() == Some(r.id.as_str()),
                    "stats": s,
                    "imported_at": prov.as_ref().map(|p| p.imported_at.clone()),
                    "source": prov.as_ref().map(|p| p.source.clone()),
                    "sha256": prov.as_ref().and_then(|p| p.sha256.clone()),
                    "format": prov.as_ref().map(|p| p.format.clone()),
                    "job_id": prov.as_ref().map(|p| p.job_id.clone()),
                    "db_growth_bytes": prov.as_ref().and_then(|p| p.db_growth_bytes),
                    "created_at": r.created_at,
                    "updated_at": r.updated_at,
                })
            })
            .collect();
        if q.populated && stats.is_some() && sys_concepts == 0 {
            continue;
        }
        total_concepts += sys_concepts;
        let title = versions
            .iter()
            .find_map(|r| r.title.clone().or_else(|| r.name.clone()));
        systems.push(json!({
            "url": url,
            "title": title,
            "concepts": stats.as_ref().map(|_| sys_concepts),
            "est_bytes": stats.as_ref().map(|_| sys_bytes),
            "versions": vjson,
        }));
    }
    if q.sort.as_deref() == Some("size") {
        systems.sort_by_key(|s| std::cmp::Reverse(s["est_bytes"].as_u64().unwrap_or(0)));
    }
    let matched = systems.len();
    if let Some(limit) = q.limit {
        systems.truncate(limit);
    }
    Ok(Json(json!({
        "stats_computed_at": stats_at,
        "stats_pending": stats.is_none(),
        "database": fp,
        "value_sets": vs,
        "concept_maps": cm,
        "code_system_count": matched,
        "total_concepts": stats.as_ref().map(|_| total_concepts),
        "systems": systems,
    }))
    .into_response())
}

async fn refresh_stats(State(core): Core) -> ApiResult {
    core.invalidate_stats().await;
    let (at, stats) = core.system_stats().await?;
    Ok(Json(json!({ "stats_computed_at": at, "systems": stats.len() })).into_response())
}

// ── CodeSystem version management ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CsTarget {
    url: String,
    /// Absent / empty = the un-versioned row.
    #[serde(default)]
    version: Option<String>,
}

impl CsTarget {
    fn version(&self) -> Option<&str> {
        self.version.as_deref().filter(|v| !v.is_empty())
    }
}

async fn dependents(State(core): Core, Query(t): Query<CsTarget>) -> ApiResult {
    let row = core
        .ops
        .find_code_system(&t.url, t.version())
        .await?
        .ok_or_else(|| {
            AdminError::NotFound(format!(
                "CodeSystem {}|{}",
                t.url,
                t.version().unwrap_or("")
            ))
        })?;
    let deps = core.ops.dependents(&t.url, t.version()).await?;
    let stats = core
        .stats
        .value
        .read()
        .await
        .as_ref()
        .and_then(|(_, s)| s.get(&row.id).cloned());
    Ok(Json(json!({ "code_system": row, "stats": stats, "dependents": deps })).into_response())
}

async fn enqueue_delete_cs(core: Arc<AdminCore>, headers: HeaderMap, t: CsTarget) -> ApiResult {
    if core
        .ops
        .find_code_system(&t.url, t.version())
        .await?
        .is_none()
    {
        return Err(AdminError::NotFound(format!(
            "CodeSystem {}|{}",
            t.url,
            t.version().unwrap_or("")
        )));
    }
    let job = core.jobs.submit(
        JobSpec::DeleteCodeSystem {
            url: t.url.clone(),
            version: t.version().map(String::from),
        },
        Actor::from_headers(&headers),
    );
    Ok(accepted(job))
}

async fn delete_code_system_q(
    State(core): Core,
    headers: HeaderMap,
    Query(t): Query<CsTarget>,
) -> ApiResult {
    enqueue_delete_cs(core, headers, t).await
}

async fn delete_code_system_body(
    State(core): Core,
    headers: HeaderMap,
    Json(t): Json<CsTarget>,
) -> ApiResult {
    enqueue_delete_cs(core, headers, t).await
}

#[derive(Debug, Deserialize)]
struct SetCurrent {
    url: String,
    /// The version to pin; `null` clears the pin (newest version wins again).
    #[serde(default)]
    version: Option<String>,
    /// Pin the un-versioned row (only meaningful with `version: null`).
    #[serde(default)]
    unversioned: bool,
}

async fn set_current(
    State(core): Core,
    headers: HeaderMap,
    Json(body): Json<SetCurrent>,
) -> ApiResult {
    let actor = Actor::from_headers(&headers);
    let pin: Option<Option<&str>> = match (&body.version, body.unversioned) {
        (Some(v), _) if !v.is_empty() => Some(Some(v.as_str())),
        (_, true) => Some(None),
        _ => None,
    };
    let entity = format!(
        "{}|{}",
        body.url,
        match pin {
            Some(Some(v)) => v.to_string(),
            Some(None) => "(unversioned)".into(),
            None => "(clear pin)".into(),
        }
    );
    match core.ops.set_current_version(&body.url, pin).await {
        Ok(()) => {
            (core.on_change)();
            core.audit.record(
                &actor,
                "set-current-version",
                &entity,
                Outcome::Success,
                None,
            );
            Ok(
                Json(json!({ "url": body.url, "pinned": pin.map(|v| v.map(String::from)) }))
                    .into_response(),
            )
        }
        Err(e) => {
            let msg = e.to_string();
            core.audit.record(
                &actor,
                "set-current-version",
                &entity,
                Outcome::MinorFailure,
                Some(&msg),
            );
            Err(e.into())
        }
    }
}

#[derive(Debug, Deserialize)]
struct UrlVersion {
    url: String,
    #[serde(default)]
    version: Option<String>,
}

async fn delete_value_set(
    State(core): Core,
    headers: HeaderMap,
    Query(t): Query<UrlVersion>,
) -> ApiResult {
    Ok(accepted(core.jobs.submit(
        JobSpec::DeleteValueSet {
            url: t.url,
            version: t.version.filter(|v| !v.is_empty()),
        },
        Actor::from_headers(&headers),
    )))
}

async fn delete_concept_map(
    State(core): Core,
    headers: HeaderMap,
    Query(t): Query<UrlVersion>,
) -> ApiResult {
    Ok(accepted(core.jobs.submit(
        JobSpec::DeleteConceptMap { url: t.url },
        Actor::from_headers(&headers),
    )))
}

async fn compact(State(core): Core, headers: HeaderMap) -> ApiResult {
    Ok(accepted(
        core.jobs
            .submit(JobSpec::Compact, Actor::from_headers(&headers)),
    ))
}

// ── Uploads ───────────────────────────────────────────────────────────────────

async fn list_uploads(State(core): Core) -> ApiResult {
    Ok(Json(json!({
        "chunk_size": core.config.chunk_size,
        "uploads": core.uploads.list().await,
    }))
    .into_response())
}

#[derive(Debug, Deserialize)]
struct CreateUpload {
    filename: String,
    size: u64,
    #[serde(default)]
    client_key: Option<String>,
}

async fn create_upload(
    State(core): Core,
    headers: HeaderMap,
    Json(body): Json<CreateUpload>,
) -> ApiResult {
    let actor = Actor::from_headers(&headers);
    let meta = core
        .uploads
        .create(
            &body.filename,
            body.size,
            body.client_key.clone(),
            Some(actor.label()),
        )
        .await?;
    let resumed = meta.received.iter().any(|r| *r);
    if !resumed {
        core.audit.record(
            &actor,
            "upload",
            &format!(
                "{} ({} bytes, upload {})",
                meta.filename, meta.size, meta.id
            ),
            Outcome::Success,
            None,
        );
    }
    let status = if resumed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(upload_json(&meta))).into_response())
}

fn upload_json(m: &uploads::UploadMeta) -> Value {
    let mut v = serde_json::to_value(m).unwrap_or_default();
    v["received_bytes"] = json!(m.received_bytes());
    v["missing"] = json!(m.missing());
    v
}

async fn upload_status(State(core): Core, Path(id): Path<String>) -> ApiResult {
    Ok(Json(upload_json(&core.uploads.status(&id).await?)).into_response())
}

async fn put_chunk(
    State(core): Core,
    Path((id, index)): Path<(String, u64)>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> ApiResult {
    let sha = headers.get("x-chunk-sha256").and_then(|v| v.to_str().ok());
    let meta = core.uploads.put_chunk(&id, index, body, sha).await?;
    Ok(Json(json!({
        "id": meta.id,
        "index": index,
        "received_bytes": meta.received_bytes(),
        "size": meta.size,
        "missing": meta.missing().len(),
    }))
    .into_response())
}

async fn complete_upload(State(core): Core, Path(id): Path<String>) -> ApiResult {
    let meta = core.uploads.complete(&id).await?;
    Ok(Json(upload_json(&meta)).into_response())
}

async fn delete_upload(State(core): Core, headers: HeaderMap, Path(id): Path<String>) -> ApiResult {
    core.uploads.delete(&id).await?;
    core.audit.record(
        &Actor::from_headers(&headers),
        "delete-upload",
        &format!("upload {id}"),
        Outcome::Success,
        None,
    );
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Deserialize, Default)]
struct DetectQuery {
    path: Option<String>,
    /// Recompute the growth estimate for a manually chosen format.
    format: Option<String>,
}

/// Detection plus the space check for the effective format.
fn with_space(core: &AdminCore, mut d: formats::Detection, format: Option<&str>) -> Value {
    if let Some(f) = format.and_then(formats::parse_format) {
        let fi = formats::info(f);
        d.format = Some(fi.id.clone());
        d.licence = Some(fi.licence);
        d.growth_estimate_bytes = Some(fi.estimate_growth(d.size_bytes));
    }
    let space = match (&core.config.db_dir, d.growth_estimate_bytes) {
        (Some(dir), Some(g)) => Some(super::disk::check(dir, g, core.config.disk_headroom)),
        _ => None,
    };
    let info = d
        .format
        .as_deref()
        .and_then(formats::parse_format)
        .map(formats::info);
    json!({ "detection": d, "space": space, "format_info": info })
}

async fn detect_upload(
    State(core): Core,
    Path(id): Path<String>,
    Query(q): Query<DetectQuery>,
) -> ApiResult {
    let (path, meta) = core.uploads.file_path(&id).await?;
    let d = match meta.detection {
        Some(d) => d,
        None => tokio::task::spawn_blocking(move || formats::detect(&path))
            .await
            .map_err(|e| AdminError::Internal(e.to_string()))?,
    };
    Ok(Json(with_space(&core, d, q.format.as_deref())).into_response())
}

fn import_root(core: &AdminCore) -> Result<&std::path::Path, AdminError> {
    core.config.import_dir.as_deref().ok_or_else(|| {
        AdminError::Disabled("no server import directory configured (HTS_IMPORT_DIR)".into())
    })
}

async fn list_import_dir(State(core): Core) -> ApiResult {
    let root = import_root(&core)?.to_path_buf();
    let entries = tokio::task::spawn_blocking(move || uploads::list_import_dir(&root))
        .await
        .map_err(|e| AdminError::Internal(e.to_string()))??;
    Ok(Json(json!({ "root": core.config.import_dir, "entries": entries })).into_response())
}

async fn detect_import_dir(State(core): Core, Query(q): Query<DetectQuery>) -> ApiResult {
    let root = import_root(&core)?;
    let rel = q
        .path
        .as_deref()
        .ok_or_else(|| AdminError::BadRequest("path is required".into()))?;
    let path = uploads::resolve_in(root, rel)?;
    let d = tokio::task::spawn_blocking(move || formats::detect(&path))
        .await
        .map_err(|e| AdminError::Internal(e.to_string()))?;
    Ok(Json(with_space(&core, d, q.format.as_deref())).into_response())
}

// ── Imports ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct StartImport {
    source: ImportSource,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    languages: Option<String>,
    /// Comma-separated string or array.
    #[serde(default)]
    properties: Option<Value>,
    #[serde(default)]
    batch_size: Option<usize>,
}

fn parse_properties(v: Option<Value>) -> Option<Vec<String>> {
    let list: Vec<String> = match v? {
        Value::String(s) => s.split(',').map(|p| p.trim().to_string()).collect(),
        Value::Array(a) => a
            .into_iter()
            .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
            .collect(),
        _ => return None,
    };
    let list: Vec<String> = list.into_iter().filter(|s| !s.is_empty()).collect();
    (!list.is_empty()).then_some(list)
}

async fn start_import(
    State(core): Core,
    headers: HeaderMap,
    Json(body): Json<StartImport>,
) -> ApiResult {
    // Validate now so the operator gets a 4xx instead of a failed job.
    let fmt = match body.format.as_deref().filter(|f| !f.is_empty()) {
        Some(f) => Some(
            formats::parse_format(f)
                .ok_or_else(|| AdminError::BadRequest(format!("unknown format {f:?}")))?,
        ),
        None => None,
    };
    match &body.source {
        ImportSource::Upload { upload_id } => {
            core.uploads.file_path(upload_id).await?;
        }
        ImportSource::ImportDir { path } => {
            uploads::resolve_in(import_root(&core)?, path)?;
        }
        ImportSource::Registry { package, .. } => {
            if !super::registry::valid_package_name(package) {
                return Err(AdminError::BadRequest(format!(
                    "invalid package name {package:?}"
                )));
            }
            if core.registry.base().is_none() {
                return Err(AdminError::Disabled(
                    "package registry is disabled (HTS_PACKAGE_REGISTRY=off)".into(),
                ));
            }
        }
    }
    if let (Some(f), Some(_)) = (fmt, body.version.as_deref().filter(|v| !v.is_empty()))
        && !formats::info(f).single_system
    {
        return Err(AdminError::BadRequest(format!(
            "a version override is only possible for single-system formats, not {f}"
        )));
    }
    let job = core.jobs.submit(
        JobSpec::Import {
            source: body.source,
            format: fmt.map(|f| f.to_string()),
            version: body.version.filter(|v| !v.is_empty()),
            languages: body.languages.filter(|v| !v.trim().is_empty()),
            properties: parse_properties(body.properties),
            batch_size: body.batch_size,
        },
        Actor::from_headers(&headers),
    );
    Ok(accepted(job))
}

// ── Registry ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RegistrySearch {
    q: String,
    #[serde(default, rename = "fhirVersion")]
    fhir_version: Option<String>,
}

async fn registry_search(State(core): Core, Query(q): Query<RegistrySearch>) -> ApiResult {
    let hits = core
        .registry
        .search(&q.q, q.fhir_version.as_deref().or(Some("R4")))
        .await?;
    Ok(Json(json!({ "registry": core.registry.base(), "packages": hits })).into_response())
}

#[derive(Debug, Deserialize)]
struct VersionQuery {
    version: Option<String>,
}

async fn registry_package(
    State(core): Core,
    Path(name): Path<String>,
    Query(q): Query<VersionQuery>,
) -> ApiResult {
    let pv = core.registry.resolve(&name, q.version.as_deref()).await?;
    Ok(Json(json!(pv)).into_response())
}

#[derive(Debug, Deserialize)]
struct Install {
    package: String,
    #[serde(default)]
    version: Option<String>,
}

async fn registry_install(
    State(core): Core,
    headers: HeaderMap,
    Json(body): Json<Install>,
) -> ApiResult {
    start_import(
        State(core),
        headers,
        Json(StartImport {
            source: ImportSource::Registry {
                package: body.package,
                version: body.version.filter(|v| !v.is_empty()),
            },
            format: None,
            version: None,
            languages: None,
            properties: None,
            batch_size: None,
        }),
    )
    .await
}

// ── Jobs / audit ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Limit {
    limit: Option<usize>,
    tail: Option<usize>,
}

async fn list_jobs(State(core): Core, Query(l): Query<Limit>) -> ApiResult {
    Ok(Json(json!({ "jobs": core.jobs.list(l.limit.unwrap_or(50).min(500)) })).into_response())
}

async fn get_job(State(core): Core, Path(id): Path<String>) -> ApiResult {
    let job = core
        .jobs
        .get(&id)
        .ok_or_else(|| AdminError::NotFound(format!("job {id}")))?;
    Ok(Json(job).into_response())
}

async fn job_log(State(core): Core, Path(id): Path<String>, Query(l): Query<Limit>) -> ApiResult {
    let lines = core.jobs.log_tail(&id, l.tail.unwrap_or(200).min(5000))?;
    Ok(Json(json!({ "id": id, "lines": lines })).into_response())
}

async fn cancel_job(State(core): Core, headers: HeaderMap, Path(id): Path<String>) -> ApiResult {
    let actor = Actor::from_headers(&headers);
    let job = core.jobs.cancel(&id, &actor)?;
    core.audit.record(
        &actor,
        "cancel-job",
        &format!("job {id}"),
        Outcome::Success,
        None,
    );
    Ok(Json(job).into_response())
}

async fn audit(State(core): Core, Query(l): Query<Limit>) -> ApiResult {
    Ok(
        Json(json!({ "events": core.audit.recent(l.limit.unwrap_or(100).min(1000)) }))
            .into_response(),
    )
}
