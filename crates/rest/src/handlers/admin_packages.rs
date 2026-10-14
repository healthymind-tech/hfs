//! FHIR package (IG) administration API: `/admin/packages`.
//!
//! | Method & path | |
//! |---|---|
//! | `GET /admin/packages` | installed packages |
//! | `POST /admin/packages` | JSON `{package, dryRun, options}` → plan or install job (`202`); a `.tgz` body (`application/gzip`/`octet-stream`) → stored in the cache, plan job started |
//! | `GET /admin/packages/{name}/{version}` | one package, resources by type |
//! | `DELETE /admin/packages/{name}/{version}` | uninstall job (`409` when another package depends on it) |
//! | `GET /admin/packages/jobs`, `GET /admin/packages/jobs/{id}` | job progress and result |
//! | `DELETE /admin/packages/jobs/{id}` | cancel |
//! | `GET /admin/packages/registry/search?q=&fhirversion=` | registry catalog search (marketplace metadata merged, Taiwan first) |
//! | `GET /admin/packages/registry/marketplace?q=&category=&country=&authority=` | IG marketplace index |
//! | `GET /admin/packages/registry/versions/{name}` | published versions |
//! | `GET /admin/packages/cache` | archives in the local package cache |
//! | `GET /admin/packages/config` | registry mode, limits, terminology server |
//!
//! Job-creating calls accept `?wait=<seconds>` (≤ 60): the response is held
//! until the job ends or the wait expires, so scripts get a finished plan
//! (`200`) in one call while long installs still answer `202` promptly.
//!
//! Everything but `config` answers `403` unless `HFS_ADMIN_UI=true`. The
//! router sits in the console admin tier (system-scope token when auth is
//! on). The tenant is `X-Tenant-ID` / `?tenant=`, default the server's.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use helios_auth::Principal;
use helios_persistence::tenant::TenantId;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::packages::{InstallRequest, Job, JobStatus, ManagerError, PackageManager};

/// Router state.
pub type Manager = Arc<PackageManager>;

fn outcome(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    let message = message.into();
    (
        status,
        [(header::CONTENT_TYPE, "application/fhir+json")],
        Json(json!({
            "resourceType": "OperationOutcome",
            "issue": [{"severity": "error", "code": code, "diagnostics": message}],
        })),
    )
        .into_response()
}

fn manager_error(e: ManagerError) -> Response {
    match e {
        ManagerError::Invalid(m) => outcome(StatusCode::BAD_REQUEST, "invalid", m),
        ManagerError::NotFound(m) => outcome(StatusCode::NOT_FOUND, "not-found", m),
        ManagerError::Conflict(m) => outcome(StatusCode::CONFLICT, "conflict", m),
        ManagerError::Internal(m) => outcome(StatusCode::INTERNAL_SERVER_ERROR, "exception", m),
    }
}

fn gate(manager: &PackageManager) -> Option<Response> {
    if manager.admin_enabled() {
        None
    } else {
        Some(outcome(
            StatusCode::FORBIDDEN,
            "forbidden",
            "package administration is disabled; set HFS_ADMIN_UI=true to enable it",
        ))
    }
}

/// Common query parameters.
#[derive(Debug, Default, Deserialize)]
pub struct CommonQuery {
    tenant: Option<String>,
    wait: Option<u64>,
    #[serde(rename = "dryRun", alias = "dry_run")]
    dry_run: Option<bool>,
    resources: Option<bool>,
}

fn tenant_of(
    manager: &PackageManager,
    headers: &HeaderMap,
    query: &CommonQuery,
) -> Result<String, Box<Response>> {
    let raw = query
        .tenant
        .clone()
        .or_else(|| {
            headers
                .get("x-tenant-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        })
        .filter(|t| !t.trim().is_empty());
    match raw {
        None => Ok(manager.default_tenant().to_string()),
        Some(t) => TenantId::parse(t.trim())
            .map(|_| t.trim().to_string())
            .map_err(|e| Box::new(outcome(StatusCode::BAD_REQUEST, "invalid", e.to_string()))),
    }
}

fn who(principal: Option<&Principal>) -> Option<String> {
    principal.map(|p| p.subject.clone())
}

async fn job_response(job: Arc<Job>, wait: Option<u64>) -> Response {
    let wait = Duration::from_secs(wait.unwrap_or(0).min(60));
    let deadline = tokio::time::Instant::now() + wait;
    while !job.view().status.is_terminal() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let view = job.view();
    let status = if view.status.is_terminal() {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    let location = format!("admin/packages/jobs/{}", view.id);
    (status, [(header::LOCATION, location)], Json(view)).into_response()
}

/// `GET /admin/packages/config`
pub async fn config_handler(State(manager): State<Manager>) -> Response {
    Json(manager.config_view()).into_response()
}

/// `GET /admin/packages`
pub async fn list_handler(
    State(manager): State<Manager>,
    headers: HeaderMap,
    Query(query): Query<CommonQuery>,
) -> Response {
    if let Some(r) = gate(&manager) {
        return r;
    }
    let tenant = match tenant_of(&manager, &headers, &query) {
        Ok(t) => t,
        Err(r) => return *r,
    };
    match manager.list(&tenant).await {
        Ok(packages) => Json(json!({"tenant": tenant, "packages": packages})).into_response(),
        Err(e) => manager_error(e),
    }
}

fn is_archive(headers: &HeaderMap) -> bool {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    ct.starts_with("application/gzip")
        || ct.starts_with("application/x-gzip")
        || ct.starts_with("application/octet-stream")
        || ct.starts_with("application/x-tar")
        || ct.starts_with("application/x-compressed-tar")
        || ct.starts_with("application/tar+gzip")
}

/// `POST /admin/packages`
pub async fn create_handler(
    State(manager): State<Manager>,
    headers: HeaderMap,
    Query(query): Query<CommonQuery>,
    principal: Option<axum::Extension<Principal>>,
    body: Bytes,
) -> Response {
    if let Some(r) = gate(&manager) {
        return r;
    }
    let tenant = match tenant_of(&manager, &headers, &query) {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let requested_by = who(principal.as_deref());
    let request = if is_archive(&headers) {
        // Upload: validate and cache, then plan (or install with
        // ?dryRun=false). Works with the registry off.
        let bytes = body.to_vec();
        let accepted = {
            let manager = Arc::clone(&manager);
            tokio::task::spawn_blocking(move || manager.accept_upload(&bytes)).await
        };
        let manifest = match accepted {
            Ok(Ok(m)) => m,
            Ok(Err(e)) => return manager_error(e),
            Err(e) => {
                return outcome(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "exception",
                    e.to_string(),
                );
            }
        };
        InstallRequest {
            package: Some(manifest.key()),
            dry_run: query.dry_run.unwrap_or(true),
            ..InstallRequest::default()
        }
    } else {
        match serde_json::from_slice::<InstallRequest>(&body) {
            Ok(mut r) => {
                if let Some(d) = query.dry_run {
                    r.dry_run = d;
                }
                r
            }
            Err(e) => {
                return outcome(
                    StatusCode::BAD_REQUEST,
                    "invalid",
                    format!("expected a JSON install request or a .tgz body: {e}"),
                );
            }
        }
    };
    match manager.start_install(&tenant, request, requested_by) {
        Ok(job) => job_response(job, query.wait).await,
        Err(e) => manager_error(e),
    }
}

/// `GET /admin/packages/{name}/{version}`
pub async fn detail_handler(
    State(manager): State<Manager>,
    headers: HeaderMap,
    Path((name, version)): Path<(String, String)>,
    Query(query): Query<CommonQuery>,
) -> Response {
    if let Some(r) = gate(&manager) {
        return r;
    }
    let tenant = match tenant_of(&manager, &headers, &query) {
        Ok(t) => t,
        Err(r) => return *r,
    };
    match manager
        .detail(&tenant, &name, &version, query.resources.unwrap_or(true))
        .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => manager_error(e),
    }
}

/// `DELETE /admin/packages/{name}/{version}`
pub async fn delete_handler(
    State(manager): State<Manager>,
    headers: HeaderMap,
    Path((name, version)): Path<(String, String)>,
    Query(query): Query<CommonQuery>,
    principal: Option<axum::Extension<Principal>>,
) -> Response {
    if let Some(r) = gate(&manager) {
        return r;
    }
    let tenant = match tenant_of(&manager, &headers, &query) {
        Ok(t) => t,
        Err(r) => return *r,
    };
    match manager
        .start_uninstall(&tenant, &name, &version, who(principal.as_deref()))
        .await
    {
        Ok(job) => job_response(job, query.wait).await,
        Err(e) => manager_error(e),
    }
}

/// `GET /admin/packages/jobs`
pub async fn jobs_handler(State(manager): State<Manager>) -> Response {
    if let Some(r) = gate(&manager) {
        return r;
    }
    // The list omits per-job plans/logs to stay small.
    let jobs: Vec<Value> = manager
        .jobs()
        .list()
        .into_iter()
        .map(|v| {
            json!({
                "id": v.id, "kind": v.kind, "package": v.package, "tenant": v.tenant,
                "status": v.status, "phase": v.phase, "packagesTotal": v.packages_total,
                "packagesDone": v.packages_done, "resourcesTotal": v.resources_total,
                "resourcesDone": v.resources_done, "errors": v.errors.len() as u64 + v.errors_dropped,
                "createdAt": v.created_at, "finishedAt": v.finished_at,
            })
        })
        .collect();
    Json(json!({"jobs": jobs})).into_response()
}

/// `GET /admin/packages/jobs/{id}`
pub async fn job_handler(
    State(manager): State<Manager>,
    Path(id): Path<String>,
    Query(query): Query<CommonQuery>,
) -> Response {
    if let Some(r) = gate(&manager) {
        return r;
    }
    match manager.jobs().get(&id) {
        Some(job) => job_response(job, query.wait).await,
        None => outcome(
            StatusCode::NOT_FOUND,
            "not-found",
            format!("no package job {id}"),
        ),
    }
}

/// `DELETE /admin/packages/jobs/{id}` — cancel.
pub async fn cancel_handler(State(manager): State<Manager>, Path(id): Path<String>) -> Response {
    if let Some(r) = gate(&manager) {
        return r;
    }
    match manager.jobs().get(&id) {
        Some(job) => {
            let accepted = job.cancel();
            let status = if accepted {
                StatusCode::ACCEPTED
            } else {
                StatusCode::CONFLICT
            };
            (
                status,
                Json(json!({"cancelRequested": accepted, "job": job.view()})),
            )
                .into_response()
        }
        None => outcome(
            StatusCode::NOT_FOUND,
            "not-found",
            format!("no package job {id}"),
        ),
    }
}

/// Registry query parameters.
#[derive(Debug, Default, Deserialize)]
pub struct RegistryQuery {
    q: Option<String>,
    #[serde(alias = "fhirVersion")]
    fhirversion: Option<String>,
    category: Option<String>,
    country: Option<String>,
    authority: Option<String>,
    prefer: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
    tenant: Option<String>,
}

async fn installed_versions(
    manager: &PackageManager,
    headers: &HeaderMap,
    tenant: Option<String>,
) -> HashMap<String, Vec<String>> {
    let query = CommonQuery {
        tenant,
        ..CommonQuery::default()
    };
    let Ok(tenant) = tenant_of(manager, headers, &query) else {
        return HashMap::new();
    };
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    if let Ok(list) = manager.list(&tenant).await {
        for p in list {
            if let (Some(n), Some(v)) = (p["name"].as_str(), p["version"].as_str()) {
                map.entry(n.to_string()).or_default().push(v.to_string());
            }
        }
    }
    map
}

/// `GET /admin/packages/registry/search`
pub async fn search_handler(
    State(manager): State<Manager>,
    headers: HeaderMap,
    Query(query): Query<RegistryQuery>,
) -> Response {
    if let Some(r) = gate(&manager) {
        return r;
    }
    let q = query.q.clone().unwrap_or_default();
    let prefer = query
        .prefer
        .clone()
        .unwrap_or_else(|| "tw".into())
        .to_ascii_lowercase();
    let market = manager.registry().marketplace().await.ok();
    let by_name: HashMap<&str, &crate::packages::registry::MarketEntry> = market
        .as_ref()
        .map(|m| m.iter().map(|e| (e.name.as_str(), e)).collect())
        .unwrap_or_default();
    let installed = installed_versions(&manager, &headers, query.tenant.clone()).await;
    let (entries, source, error) = match manager
        .registry()
        .search(&q, query.fhirversion.as_deref())
        .await
    {
        Ok(entries) => (entries, "registry", None),
        Err(e) => {
            // Registry unreachable or off: answer from the marketplace index.
            let needle = q.to_ascii_lowercase();
            let fallback = market
                .as_ref()
                .map(|m| {
                    m.iter()
                        .filter(|e| {
                            needle.is_empty()
                                || e.name.to_ascii_lowercase().contains(&needle)
                                || e.title.to_ascii_lowercase().contains(&needle)
                        })
                        .map(|e| crate::packages::registry::CatalogEntry {
                            name: e.name.clone(),
                            description: Some(e.description.clone()),
                            fhir_version: e
                                .editions
                                .last()
                                .and_then(|ed| ed.fhir_versions.first().cloned()),
                            version: e.latest().map(str::to_string),
                            canonical: Some(e.canonical.clone()),
                        })
                        .collect()
                })
                .unwrap_or_default();
            (fallback, "marketplace", Some(e))
        }
    };
    let mut results: Vec<Value> = entries
        .into_iter()
        .map(|e| {
            let m = by_name.get(e.name.as_str());
            json!({
                "name": e.name,
                "description": e.description,
                "fhirVersion": e.fhir_version,
                "version": e.version.clone().or_else(|| m.and_then(|m| m.latest().map(str::to_string))),
                "canonical": e.canonical.clone().or_else(|| m.map(|m| m.canonical.clone())),
                "title": m.map(|m| m.title.clone()),
                "category": m.map(|m| m.category.clone()),
                "country": m.map(|m| m.country.clone()),
                "authority": m.map(|m| m.authority.clone()),
                "installed": installed.get(&e.name).cloned().unwrap_or_default(),
            })
        })
        .collect();
    results.sort_by(|a, b| {
        let pa = a["country"].as_str() != Some(prefer.as_str());
        let pb = b["country"].as_str() != Some(prefer.as_str());
        pa.cmp(&pb)
            .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });
    let total = results.len();
    let offset = query.offset.unwrap_or(0).min(total);
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let page: Vec<Value> = results.into_iter().skip(offset).take(limit).collect();
    Json(json!({"source": source, "error": error, "total": total, "offset": offset, "results": page}))
        .into_response()
}

/// `GET /admin/packages/registry/marketplace`
pub async fn marketplace_handler(
    State(manager): State<Manager>,
    headers: HeaderMap,
    Query(query): Query<RegistryQuery>,
) -> Response {
    if let Some(r) = gate(&manager) {
        return r;
    }
    let list = match manager.registry().marketplace().await {
        Ok(list) => list,
        Err(e) => {
            return Json(
                json!({"available": false, "error": e, "total": 0, "results": [],
                               "facets": {"categories": {}, "countries": {}, "authorities": {}}}),
            )
            .into_response();
        }
    };
    let installed = installed_versions(&manager, &headers, query.tenant.clone()).await;
    let needle = query.q.clone().unwrap_or_default().to_ascii_lowercase();
    let eq = |filter: &Option<String>, value: &str| {
        filter
            .as_deref()
            .filter(|f| !f.is_empty())
            .is_none_or(|f| f.eq_ignore_ascii_case(value))
    };
    let fhir = query.fhirversion.clone().unwrap_or_default();
    let fhir_prefix = match fhir.to_ascii_uppercase().as_str() {
        "R4" => "4.0",
        "R4B" => "4.3",
        "R5" => "5.0",
        "R6" => "6.0",
        other if !other.is_empty() => other,
        _ => "",
    }
    .to_string();
    let mut facets: [BTreeMap<String, u64>; 3] = Default::default();
    for e in list.iter() {
        *facets[0].entry(e.category.clone()).or_default() += 1;
        *facets[1].entry(e.country.clone()).or_default() += 1;
        *facets[2].entry(e.authority.clone()).or_default() += 1;
    }
    let prefer = query
        .prefer
        .clone()
        .unwrap_or_else(|| "tw".into())
        .to_ascii_lowercase();
    let mut hits: Vec<&crate::packages::registry::MarketEntry> = list
        .iter()
        .filter(|e| eq(&query.category, &e.category))
        .filter(|e| eq(&query.country, &e.country))
        .filter(|e| eq(&query.authority, &e.authority))
        .filter(|e| {
            fhir_prefix.is_empty()
                || e.editions
                    .iter()
                    .any(|ed| ed.fhir_versions.iter().any(|v| v.starts_with(&fhir_prefix)))
        })
        .filter(|e| {
            needle.is_empty()
                || [
                    &e.name,
                    &e.title,
                    &e.description,
                    &e.canonical,
                    &e.authority,
                ]
                .iter()
                .any(|s| s.to_ascii_lowercase().contains(&needle))
        })
        .collect();
    hits.sort_by(|a, b| {
        (a.country != prefer)
            .cmp(&(b.country != prefer))
            .then_with(|| {
                a.title
                    .to_ascii_lowercase()
                    .cmp(&b.title.to_ascii_lowercase())
            })
    });
    let total = hits.len();
    let offset = query.offset.unwrap_or(0).min(total);
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let results: Vec<Value> = hits
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|e| {
            let mut v = serde_json::to_value(e).unwrap_or(Value::Null);
            v["latest"] = json!(e.latest());
            v["installed"] = json!(installed.get(&e.name).cloned().unwrap_or_default());
            v
        })
        .collect();
    Json(json!({
        "available": true,
        "total": total,
        "offset": offset,
        "results": results,
        "facets": {"categories": facets[0], "countries": facets[1], "authorities": facets[2]},
    }))
    .into_response()
}

/// `GET /admin/packages/registry/versions/{name}`
pub async fn versions_handler(
    State(manager): State<Manager>,
    Path(name): Path<String>,
) -> Response {
    if let Some(r) = gate(&manager) {
        return r;
    }
    if !crate::packages::tarball::valid_package_name(&name) {
        return outcome(
            StatusCode::BAD_REQUEST,
            "invalid",
            format!("invalid package name '{name}'"),
        );
    }
    match manager.registry().doc(&name).await {
        Ok(doc) => Json(doc).into_response(),
        Err(e) => outcome(StatusCode::NOT_FOUND, "not-found", e),
    }
}

/// `GET /admin/packages/cache`
pub async fn cache_handler(State(manager): State<Manager>) -> Response {
    if let Some(r) = gate(&manager) {
        return r;
    }
    let packages: Vec<Value> = manager
        .registry()
        .cached_packages()
        .into_iter()
        .map(|(name, version)| json!({"name": name, "version": version, "package": format!("{name}#{version}")}))
        .collect();
    Json(json!({"cacheDir": manager.registry().cache_dir().display().to_string(), "packages": packages}))
        .into_response()
}

/// Terminal-state check used by tests.
pub fn job_finished(status: JobStatus) -> bool {
    status.is_terminal()
}
