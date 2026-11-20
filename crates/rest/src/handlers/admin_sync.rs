//! Handlers of the sync admin API (`/admin/sync/**`, FHIR server manager
//! P6, design §8). JSON in and out; errors are OperationOutcomes (`422`
//! with one issue per field and `expression` = field path, `409` conflicts,
//! `412` stale `If-Match`); jobs carry `ETag: "<revision>"`. Credentials are
//! write-only: responses show every secret as `{"set": true}`.
//!
//! The handlers are thin: the logic lives in [`helios_sync::SyncService`].

use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use helios_endpoints::store::Actor;
use helios_endpoints::sync::DuplicateRequest;
use helios_sync::{SyncError, SyncService};
use serde::Deserialize;
use serde_json::{Value, json};

/// Where handlers find the service: the process-global one the server
/// installs (`HFS_MANAGER=on`), or a fixed one (tests).
#[derive(Clone)]
pub enum SyncApi {
    /// [`helios_sync::global`].
    Global,
    /// A given service.
    Fixed(Arc<SyncService>),
}

impl SyncApi {
    #[allow(clippy::result_large_err)] // the error is the ready response
    fn service(&self) -> Result<Arc<SyncService>, Response> {
        match self {
            SyncApi::Fixed(s) => Ok(s.clone()),
            SyncApi::Global => helios_sync::global().ok_or_else(|| {
                outcome(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "not-supported",
                    "endpoint sync is not enabled on this server (HFS_MANAGER=off)",
                    None,
                )
            }),
        }
    }
}

/// An OperationOutcome response.
pub fn outcome(
    status: StatusCode,
    code: &str,
    diagnostics: &str,
    expression: Option<&str>,
) -> Response {
    let mut issue = json!({"severity": "error", "code": code, "diagnostics": diagnostics});
    if let Some(e) = expression {
        issue["expression"] = json!([e]);
    }
    (
        status,
        [(header::CONTENT_TYPE, "application/fhir+json")],
        Json(json!({"resourceType": "OperationOutcome", "issue": [issue]})),
    )
        .into_response()
}

/// Maps a service error to its HTTP response.
pub fn error_response(e: SyncError) -> Response {
    match e {
        SyncError::Invalid(v) => {
            let issues: Vec<Value> = v
                .0
                .iter()
                .map(|f| {
                    let mut i =
                        json!({"severity": "error", "code": "invalid", "diagnostics": f.message});
                    if !f.field.is_empty() {
                        i["expression"] = json!([f.field]);
                    }
                    i
                })
                .collect();
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                [(header::CONTENT_TYPE, "application/fhir+json")],
                Json(json!({"resourceType": "OperationOutcome", "issue": issues})),
            )
                .into_response()
        }
        SyncError::NotFound(m) => outcome(
            StatusCode::NOT_FOUND,
            "not-found",
            &format!("{m} not found"),
            None,
        ),
        SyncError::Revision { current } => {
            let mut r = outcome(
                StatusCode::CONFLICT,
                "conflict",
                &format!("revision conflict: current revision is {current}"),
                None,
            );
            if let Ok(v) = HeaderValue::from_str(&format!("\"{current}\"")) {
                r.headers_mut().insert(header::ETAG, v);
            }
            r
        }
        SyncError::Conflict(m) => outcome(StatusCode::CONFLICT, "conflict", &m, None),
        SyncError::NoKeyRing => outcome(
            StatusCode::CONFLICT,
            "business-rule",
            &SyncError::NoKeyRing.to_string(),
            None,
        ),
        SyncError::Forbidden(m) => outcome(StatusCode::FORBIDDEN, "forbidden", &m, None),
        SyncError::Unavailable(m) => {
            outcome(StatusCode::SERVICE_UNAVAILABLE, "transient", &m, None)
        }
    }
}

fn result<T: serde::Serialize>(status: StatusCode, r: Result<T, SyncError>) -> Response {
    match r {
        Ok(v) => (status, Json(v)).into_response(),
        Err(e) => error_response(e),
    }
}

/// Who is calling, for audit and `created_by`.
pub fn actor(
    principal: Option<&helios_auth::Principal>,
    admin: Option<&helios_admin_auth::AdminUser>,
) -> Actor {
    if let Some(p) = principal {
        return Actor {
            subject: format!("token:{}|{}", p.issuer, p.subject),
            display: None,
            via: "bearer".into(),
        };
    }
    if let Some(a) = admin {
        return Actor {
            subject: format!("admin:{}", a.0),
            display: None,
            via: "admin-password".into(),
        };
    }
    Actor {
        subject: "anonymous".into(),
        display: None,
        via: "none".into(),
    }
}

/// Caller extensions.
pub type Caller = (
    Option<Extension<helios_auth::Principal>>,
    Option<Extension<helios_admin_auth::AdminUser>>,
);

fn caller_actor(c: &Caller) -> Actor {
    actor(c.0.as_ref().map(|e| &e.0), c.1.as_ref().map(|e| &e.0))
}

fn job_response(
    status: StatusCode,
    job: &helios_endpoints::sync::SyncJob,
    extra: Option<Value>,
) -> Response {
    let mut body = helios_sync::secrets::redacted_job(job);
    if let (Some(Value::Object(x)), Value::Object(b)) = (extra, &mut body) {
        b.extend(x);
    }
    let mut r = (status, Json(body)).into_response();
    if let Ok(v) = HeaderValue::from_str(&format!("\"{}\"", job.revision)) {
        r.headers_mut().insert(header::ETAG, v);
    }
    r
}

/// Parses `If-Match: "3"` / `W/"3"`.
#[allow(clippy::result_large_err)] // the error is the ready response
pub fn if_match(headers: &HeaderMap) -> Result<Option<u64>, Response> {
    let Some(v) = headers.get(header::IF_MATCH) else {
        return Ok(None);
    };
    let s = v
        .to_str()
        .unwrap_or("")
        .trim()
        .trim_start_matches("W/")
        .trim_matches('"');
    s.parse::<u64>().map(Some).map_err(|_| {
        outcome(
            StatusCode::BAD_REQUEST,
            "invalid",
            "If-Match must be a quoted revision number",
            None,
        )
    })
}

macro_rules! svc {
    ($api:expr) => {
        match $api.service() {
            Ok(s) => s,
            Err(r) => return r,
        }
    };
}

/// `GET /admin/sync/jobs`
pub async fn list_jobs(State(api): State<SyncApi>) -> Response {
    let s = svc!(api);
    match s.list().await {
        Ok(jobs) => Json(json!({"jobs": jobs})).into_response(),
        Err(e) => error_response(e),
    }
}

/// `POST /admin/sync/jobs`
pub async fn create_job(
    State(api): State<SyncApi>,
    caller: Caller,
    Json(body): Json<Value>,
) -> Response {
    let s = svc!(api);
    let actor = caller_actor(&caller);
    match s.create(body, &actor.subject).await {
        Ok(created) => {
            let id = created
                .job
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mut r = (StatusCode::CREATED, Json(created)).into_response();
            if let Ok(v) = HeaderValue::from_str(&format!("/admin/sync/jobs/{id}")) {
                r.headers_mut().insert(header::LOCATION, v);
            }
            r
        }
        Err(e) => error_response(e),
    }
}

/// `POST /admin/sync/jobs/$test`
pub async fn test_connection(State(api): State<SyncApi>, Json(body): Json<Value>) -> Response {
    let s = svc!(api);
    match s.test_connection(body).await {
        Ok(r) => Json(json!({"peers": r})).into_response(),
        Err(e) => error_response(e),
    }
}

/// `GET /admin/sync/jobs/{id}`
pub async fn get_job(State(api): State<SyncApi>, Path(id): Path<String>) -> Response {
    let s = svc!(api);
    match s.job(&id).await {
        Ok(j) => job_response(StatusCode::OK, &j, None),
        Err(e) => error_response(e),
    }
}

/// `PUT /admin/sync/jobs/{id}`
pub async fn put_job(
    State(api): State<SyncApi>,
    Path(id): Path<String>,
    headers: HeaderMap,
    caller: Caller,
    Json(body): Json<Value>,
) -> Response {
    let s = svc!(api);
    let expected = match if_match(&headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Some(exp) = expected {
        match s.job(&id).await {
            Ok(j) if j.revision != exp => {
                let mut r = outcome(
                    StatusCode::PRECONDITION_FAILED,
                    "conflict",
                    &format!(
                        "If-Match {exp} is stale: current revision is {}",
                        j.revision
                    ),
                    None,
                );
                if let Ok(v) = HeaderValue::from_str(&format!("\"{}\"", j.revision)) {
                    r.headers_mut().insert(header::ETAG, v);
                }
                return r;
            }
            Ok(_) => {}
            Err(e) => return error_response(e),
        }
    }
    let actor = caller_actor(&caller);
    match s.update(&id, expected, body, &actor.subject).await {
        Ok(j) => job_response(StatusCode::OK, &j, None),
        Err(e) => error_response(e),
    }
}

/// `DELETE /admin/sync/jobs/{id}`
pub async fn delete_job(
    State(api): State<SyncApi>,
    Path(id): Path<String>,
    caller: Caller,
) -> Response {
    let s = svc!(api);
    let actor = caller_actor(&caller);
    match s.delete(&id, &actor.subject).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => error_response(e),
    }
}

fn run_accepted(run: helios_endpoints::sync::SyncRun) -> Response {
    let loc = format!("/admin/sync/runs/{}", run.id);
    let mut r = (StatusCode::ACCEPTED, Json(run)).into_response();
    if let Ok(v) = HeaderValue::from_str(&loc) {
        r.headers_mut().insert(header::LOCATION, v.clone());
        r.headers_mut().insert(header::CONTENT_LOCATION, v);
    }
    r
}

/// `POST /admin/sync/jobs/{id}/$run`
pub async fn run_job(
    State(api): State<SyncApi>,
    Path(id): Path<String>,
    caller: Caller,
) -> Response {
    let s = svc!(api);
    match s.run_now(&id, &caller_actor(&caller).subject).await {
        Ok(run) => run_accepted(run),
        Err(e) => error_response(e),
    }
}

/// `POST /admin/sync/jobs/{id}/$retry`
pub async fn retry_job(
    State(api): State<SyncApi>,
    Path(id): Path<String>,
    caller: Caller,
) -> Response {
    let s = svc!(api);
    match s.retry(&id, &caller_actor(&caller).subject).await {
        Ok(run) => run_accepted(run),
        Err(e) => error_response(e),
    }
}

/// `POST /admin/sync/jobs/{id}/$cancel`
pub async fn cancel_job(
    State(api): State<SyncApi>,
    Path(id): Path<String>,
    caller: Caller,
) -> Response {
    let s = svc!(api);
    match s.cancel(&id, &caller_actor(&caller).subject).await {
        Ok(Some(run)) => Json(run).into_response(),
        Ok(None) => outcome(
            StatusCode::CONFLICT,
            "conflict",
            "the job has no active run",
            None,
        ),
        Err(e) => error_response(e),
    }
}

/// Paging query.
#[derive(Debug, Deserialize, Default)]
pub struct Paging {
    /// Max rows.
    pub limit: Option<usize>,
    /// Log lines after this sequence.
    pub after: Option<u64>,
}

/// `GET /admin/sync/jobs/{id}/runs`
pub async fn job_runs(
    State(api): State<SyncApi>,
    Path(id): Path<String>,
    Query(q): Query<Paging>,
) -> Response {
    let s = svc!(api);
    result(
        StatusCode::OK,
        s.runs(&id, q.limit.unwrap_or(50).min(500))
            .await
            .map(|r| json!({"runs": r})),
    )
}

/// `GET /admin/sync/runs/{run}`
pub async fn get_run(State(api): State<SyncApi>, Path(run): Path<String>) -> Response {
    let s = svc!(api);
    result(StatusCode::OK, s.run(&run).await)
}

/// `GET /admin/sync/runs/{run}/log?after=&limit=`
pub async fn run_log(
    State(api): State<SyncApi>,
    Path(run): Path<String>,
    Query(q): Query<Paging>,
) -> Response {
    let s = svc!(api);
    result(
        StatusCode::OK,
        s.log(&run, q.after.unwrap_or(0), q.limit.unwrap_or(200))
            .await
            .map(|l| json!({"lines": l})),
    )
}

/// `GET /admin/sync/runs/{run}/conflicts`
pub async fn run_conflicts(
    State(api): State<SyncApi>,
    Path(run): Path<String>,
    Query(q): Query<Paging>,
) -> Response {
    let s = svc!(api);
    result(
        StatusCode::OK,
        s.conflicts(&run, q.limit.unwrap_or(200))
            .await
            .map(|c| json!({"conflicts": c})),
    )
}

/// Body of a conflict resolution.
#[derive(Debug, Deserialize)]
pub struct ResolveBody {
    /// `overwrite` | `keep`.
    pub action: String,
}

/// `POST /admin/sync/runs/{run}/conflicts/{type}/{id}/$resolve`
pub async fn resolve_conflict(
    State(api): State<SyncApi>,
    Path((run, rt, rid)): Path<(String, String, String)>,
    caller: Caller,
    Json(body): Json<ResolveBody>,
) -> Response {
    let s = svc!(api);
    result(
        StatusCode::OK,
        s.resolve_conflict(
            &run,
            &rt,
            &rid,
            &body.action,
            &caller_actor(&caller).subject,
        )
        .await,
    )
}

/// Body of `POST /admin/sync/duplicates`.
#[derive(Debug, Deserialize)]
pub struct DuplicateBody {
    /// Endpoint to duplicate.
    pub source: String,
    /// The request.
    #[serde(flatten)]
    pub request: DuplicateRequest,
}

/// `POST /admin/sync/duplicates` (the service behind P4's
/// `POST /admin/endpoints/{name}/$duplicate`).
pub async fn duplicate(
    State(api): State<SyncApi>,
    caller: Caller,
    Json(body): Json<DuplicateBody>,
) -> Response {
    let s = svc!(api);
    let actor = caller_actor(&caller);
    match s.duplicate(&body.source, body.request, &actor).await {
        Ok(out) => {
            let loc = out.run_id.as_ref().map(|r| format!("/admin/sync/runs/{r}"));
            let mut r = (StatusCode::ACCEPTED, Json(out)).into_response();
            if let Some(l) = loc
                && let Ok(v) = HeaderValue::from_str(&l)
            {
                r.headers_mut().insert(header::LOCATION, v);
            }
            r
        }
        Err(e) => error_response(e),
    }
}

/// `POST /admin/sync/$rewrap`
pub async fn rewrap(State(api): State<SyncApi>) -> Response {
    let s = svc!(api);
    result(
        StatusCode::OK,
        s.rewrap_secrets().await.map(|n| json!({"rewrapped": n})),
    )
}

// ------------------------------------------------- P4 endpoint API hooks

use crate::handlers::admin_endpoints::error::{ApiError, Issue};
use crate::handlers::admin_endpoints::hooks::{DuplicateService, DuplicateStarted};
use crate::handlers::admin_endpoints::{RewrapParticipant, RewrapReport};

fn api_error(e: SyncError) -> ApiError {
    match e {
        SyncError::Invalid(v) => ApiError::unprocessable(
            v.0.into_iter()
                .map(|f| Issue::field("invalid", f.field, f.message))
                .collect(),
        ),
        SyncError::NotFound(m) => {
            ApiError::new(StatusCode::NOT_FOUND, "not-found", format!("{m} not found"))
        }
        SyncError::Revision { current } => ApiError::conflict(current),
        SyncError::Conflict(m) => ApiError::new(StatusCode::CONFLICT, "conflict", m),
        SyncError::NoKeyRing => ApiError::new(
            StatusCode::CONFLICT,
            "business-rule",
            SyncError::NoKeyRing.to_string(),
        ),
        SyncError::Forbidden(m) => ApiError::new(StatusCode::FORBIDDEN, "forbidden", m),
        SyncError::Unavailable(m) => ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "transient", m),
    }
}

/// `POST /admin/endpoints/{name}/$duplicate` (P4) through the sync service.
#[derive(Debug, Default, Clone, Copy)]
pub struct SyncDuplicate;

#[async_trait::async_trait]
impl DuplicateService for SyncDuplicate {
    async fn start(
        &self,
        source: &helios_endpoints::Endpoint,
        request: DuplicateRequest,
        actor: &Actor,
    ) -> Result<DuplicateStarted, ApiError> {
        let svc = helios_sync::global().ok_or_else(|| {
            ApiError::not_implemented(
                "duplicating endpoints needs the sync service (HFS_MANAGER=on)",
            )
        })?;
        let out = svc
            .duplicate(source.name(), request, actor)
            .await
            .map_err(api_error)?;
        let name = out
            .endpoint
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(DuplicateStarted {
            job_id: out.job_id.clone().unwrap_or_default(),
            status_url: match &out.run_id {
                Some(r) => format!("admin/sync/runs/{r}"),
                None => format!("admin/endpoints/{name}"),
            },
            endpoint: Some(out.endpoint),
        })
    }
}

/// Sync job credentials in `POST /admin/secrets/$rewrap` (P4).
#[derive(Debug, Default, Clone, Copy)]
pub struct SyncRewrap;

#[async_trait::async_trait]
impl RewrapParticipant for SyncRewrap {
    async fn rewrap(&self, ring: &helios_endpoints::SecretKeyRing, _actor: &Actor) -> RewrapReport {
        let Some(svc) = helios_sync::global() else {
            return RewrapReport {
                scope: "sync".into(),
                rewrapped: 0,
                current: 0,
                failed: vec![],
            };
        };
        match svc.rewrap_with(ring).await {
            Ok(c) => RewrapReport {
                scope: "sync".into(),
                rewrapped: c.rewrapped,
                current: c.current,
                failed: c.failed,
            },
            Err(e) => RewrapReport {
                scope: "sync".into(),
                rewrapped: 0,
                current: 0,
                failed: vec![format!("sync: {e}")],
            },
        }
    }
}

/// Header carrying the notification shared secret.
pub const NOTIFY_SECRET_HEADER: &str = "x-sync-notify-secret";

/// `POST /sync/notify/{job}` — the rest-hook receiver of
/// subscription-triggered jobs (outside the admin tier; authenticated by
/// the job's shared secret). The body is ignored: notifications only wake
/// an incremental pass, the source's history stays the source of truth.
pub async fn notify(
    State(api): State<SyncApi>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let s = svc!(api);
    let secret = headers
        .get(NOTIFY_SECRET_HEADER)
        .and_then(|v| v.to_str().ok());
    match s.notify(&id, secret).await {
        Ok(true) => StatusCode::ACCEPTED.into_response(),
        Ok(false) => outcome(
            StatusCode::NOT_FOUND,
            "not-found",
            "unknown job or wrong secret",
            None,
        ),
        Err(e) => error_response(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn if_match_parsing() {
        let mut h = HeaderMap::new();
        assert_eq!(if_match(&h).unwrap(), None);
        h.insert(header::IF_MATCH, HeaderValue::from_static("W/\"7\""));
        assert_eq!(if_match(&h).unwrap(), Some(7));
        h.insert(header::IF_MATCH, HeaderValue::from_static("\"x\""));
        assert!(if_match(&h).is_err());
    }

    #[test]
    fn actors() {
        assert_eq!(actor(None, None).subject, "anonymous");
        let a = helios_admin_auth::AdminUser("root".into());
        assert_eq!(actor(None, Some(&a)).subject, "admin:root");
    }

    #[test]
    fn errors_map_to_statuses() {
        let mut v = helios_endpoints::model::ValidationErrors::default();
        v.push("target", "bad");
        assert_eq!(
            error_response(SyncError::Invalid(v)).status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            error_response(SyncError::NotFound("x".into())).status(),
            StatusCode::NOT_FOUND
        );
        let r = error_response(SyncError::Revision { current: 4 });
        assert_eq!(r.status(), StatusCode::CONFLICT);
        assert_eq!(r.headers()[header::ETAG], "\"4\"");
        assert_eq!(
            error_response(SyncError::NoKeyRing).status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            error_response(SyncError::Forbidden("f".into())).status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            error_response(SyncError::Unavailable("u".into())).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            error_response(SyncError::Conflict("c".into())).status(),
            StatusCode::CONFLICT
        );
    }
}
