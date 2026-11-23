//! HTTP handlers of `/admin/endpoints` and `/admin/secrets/$rewrap`: JSON
//! parsing, `If-Match`, status codes and headers around the
//! [service](super::service), which holds every rule (shared with the
//! manager console).

use axum::{
    Extension, Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use helios_endpoints::model::{EndpointAuth, EndpointStatus};
use helios_endpoints::secret::redact_sealed;
use helios_endpoints::sync::DuplicateRequest;
use helios_endpoints::{Actor, Endpoint};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::error::{ApiError, ApiResult, Issue};
use super::service::Written;
use super::spec_input;
use super::{AdminEndpointsState, etag, expected_revision};

/// The JSON of one endpoint: redacted, plus its public base URL.
pub(crate) fn view(state: &AdminEndpointsState, e: &Endpoint) -> Value {
    let mut v = e.redacted();
    if let Some(o) = v.as_object_mut() {
        o.insert("publicBaseUrl".into(), json!(state.public_base(e)));
    }
    v
}

fn with_etag(mut response: Response, revision: u64) -> Response {
    if let Ok(v) = HeaderValue::from_str(&etag(revision)) {
        response.headers_mut().insert(header::ETAG, v);
    }
    response
}

fn endpoint_response(
    state: &AdminEndpointsState,
    status: StatusCode,
    e: &Endpoint,
    warnings: Vec<String>,
) -> Response {
    let mut body = view(state, e);
    if !warnings.is_empty() {
        body["warnings"] = json!(warnings);
    }
    let mut r = with_etag((status, Json(body)).into_response(), e.revision);
    if status == StatusCode::CREATED
        && let Ok(v) = HeaderValue::from_str(&format!("admin/endpoints/{}", e.name()))
    {
        r.headers_mut().insert(header::LOCATION, v);
    }
    r
}

fn written(state: &AdminEndpointsState, status: StatusCode, w: Written) -> Response {
    endpoint_response(state, status, &w.endpoint, w.warnings)
}

pub(crate) async fn load(state: &AdminEndpointsState, name: &str) -> ApiResult<Endpoint> {
    state.get_endpoint(name).await
}

fn json_body<T: serde::de::DeserializeOwned>(body: &[u8], what: &str) -> ApiResult<T> {
    let obj = spec_input::json_object(body)?;
    serde_json::from_value(Value::Object(obj)).map_err(|e| {
        ApiError::unprocessable(vec![Issue {
            code: "invalid",
            message: format!("invalid {what}: {e}"),
            field: None,
        }])
    })
}

// ------------------------------------------------------------------ list / get

/// Query of `GET /admin/endpoints`.
#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    /// Comma list: `deleted`, `stats`.
    #[serde(default)]
    include: Option<String>,
}

/// `GET /admin/endpoints`
pub async fn list_handler(
    State(state): State<AdminEndpointsState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Response> {
    let include: Vec<String> = q
        .include
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    for i in &include {
        if !matches!(i.as_str(), "deleted" | "stats") {
            return Err(ApiError::bad_request(format!(
                "unknown include {i:?} (allowed: deleted, stats)"
            )));
        }
    }
    let deleted = include.iter().any(|i| i == "deleted");
    let stats = include.iter().any(|i| i == "stats");
    let list = state.list_endpoints(deleted).await?;
    let epoch = state.epoch().await?;
    let mut out = Vec::with_capacity(list.len());
    for e in &list {
        let mut v = view(&state, e);
        if stats && e.spec.status != EndpointStatus::Deleted {
            match state.tenant_stats(e).await {
                Some(Ok(s)) => v["stats"] = json!(s),
                Some(Err(err)) => v["stats"] = json!({"error": err.to_string()}),
                None => {}
            }
        }
        out.push(v);
    }
    Ok(Json(json!({"total": out.len(), "epoch": epoch, "endpoints": out})).into_response())
}

/// `GET /admin/endpoints/{name}`
pub async fn get_handler(
    State(state): State<AdminEndpointsState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let e = load(&state, &name).await?;
    let tag = etag(e.revision);
    let not_modified = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .map(|t| t.trim().trim_start_matches("W/"))
                .any(|t| t == tag || t == "*")
        });
    if not_modified {
        return Ok(with_etag(
            StatusCode::NOT_MODIFIED.into_response(),
            e.revision,
        ));
    }
    Ok(endpoint_response(&state, StatusCode::OK, &e, Vec::new()))
}

// ------------------------------------------------------------------ create / replace / patch

/// `POST /admin/endpoints`
pub async fn create_handler(
    State(state): State<AdminEndpointsState>,
    Extension(actor): Extension<Actor>,
    body: Bytes,
) -> ApiResult<Response> {
    let input = spec_input::parse_create(spec_input::json_object(&body)?)?;
    let w = state
        .create_endpoint(input.spec, input.secrets, &actor)
        .await?;
    Ok(written(&state, StatusCode::CREATED, w))
}

fn status_given(obj: &Map<String, Value>) -> bool {
    obj.get("status").is_some_and(|v| !v.is_null())
}

/// `PUT /admin/endpoints/{name}`
pub async fn put_handler(
    State(state): State<AdminEndpointsState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let current = load(&state, &name).await?;
    let expected = expected_revision(&headers, current.revision, true)?;
    let obj = spec_input::json_object(&body)?;
    let given = status_given(&obj);
    let mut input = spec_input::parse_replace(obj, &current.spec)?;
    if !given {
        input.spec.status = current.spec.status;
    }
    let w = state
        .update_endpoint(
            &name,
            expected,
            input.spec,
            input.secrets,
            &actor,
            "replaced",
        )
        .await?;
    Ok(written(&state, StatusCode::OK, w))
}

/// `PATCH /admin/endpoints/{name}` (`application/merge-patch+json`)
pub async fn patch_handler(
    State(state): State<AdminEndpointsState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    if let Some(ct) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    {
        let ct = ct.to_ascii_lowercase();
        if !(ct.starts_with("application/merge-patch+json") || ct.starts_with("application/json")) {
            return Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "not-supported",
                "PATCH takes a JSON merge patch (application/merge-patch+json)",
            ));
        }
    }
    let current = load(&state, &name).await?;
    let expected = expected_revision(&headers, current.revision, true)?;
    let input = spec_input::parse_patch(spec_input::json_object(&body)?, &current.spec)?;
    let w = state
        .update_endpoint(
            &name,
            expected,
            input.spec,
            input.secrets,
            &actor,
            "patched",
        )
        .await?;
    Ok(written(&state, StatusCode::OK, w))
}

// ------------------------------------------------------------------ status

async fn status_route(
    state: &AdminEndpointsState,
    actor: &Actor,
    name: &str,
    headers: &HeaderMap,
    status: EndpointStatus,
) -> ApiResult<Response> {
    let current = load(state, name).await?;
    let expected = expected_revision(headers, current.revision, false)?;
    let e = state
        .set_endpoint_status(name, Some(expected), status, actor)
        .await?;
    Ok(endpoint_response(state, StatusCode::OK, &e, Vec::new()))
}

/// `POST /admin/endpoints/{name}/$enable`
pub async fn enable_handler(
    State(state): State<AdminEndpointsState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    status_route(&state, &actor, &name, &headers, EndpointStatus::Enabled).await
}

/// `POST /admin/endpoints/{name}/$disable`
pub async fn disable_handler(
    State(state): State<AdminEndpointsState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    status_route(&state, &actor, &name, &headers, EndpointStatus::Disabled).await
}

// ------------------------------------------------------------------ delete

/// Query of `DELETE /admin/endpoints/{name}`.
#[derive(Debug, Default, Deserialize)]
pub struct DeleteQuery {
    /// Must repeat the endpoint name.
    #[serde(default)]
    confirm: Option<String>,
    /// Also purge the endpoint's tenant data.
    #[serde(default)]
    purge: bool,
}

/// `DELETE /admin/endpoints/{name}?confirm={name}[&purge=true]` (see
/// [`AdminEndpointsState::delete_endpoint`]).
pub async fn delete_handler(
    State(state): State<AdminEndpointsState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    Query(q): Query<DeleteQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    if q.confirm.as_deref() != Some(name.as_str()) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "required",
            format!("deleting an endpoint needs ?confirm={name}"),
        ));
    }
    let current = load(&state, &name).await?;
    let expected = expected_revision(&headers, current.revision, false)?;
    let d = state
        .delete_endpoint(&name, Some(expected), q.purge, &actor)
        .await?;
    Ok(Json(json!({
        "endpoint": view(&state, &d.endpoint),
        "purged": d.resources_removed.is_some(),
        "resourcesRemoved": d.resources_removed,
    }))
    .into_response())
}

// ------------------------------------------------------------------ history

/// Query of `GET /admin/endpoints/{name}/history`.
#[derive(Debug, Default, Deserialize)]
pub struct HistoryQuery {
    #[serde(default)]
    limit: Option<usize>,
}

/// `GET /admin/endpoints/{name}/history`
pub async fn history_handler(
    State(state): State<AdminEndpointsState>,
    Path(name): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Response> {
    let (e, revisions) = state.endpoint_history(&name, q.limit.unwrap_or(50)).await?;
    let mut v =
        serde_json::to_value(&revisions).map_err(|err| ApiError::internal(err.to_string()))?;
    redact_sealed(&mut v);
    Ok(Json(json!({"name": name, "id": e.id, "revisions": v})).into_response())
}

// ------------------------------------------------------------------ duplicate / test-auth

/// `POST /admin/endpoints/{name}/$duplicate` → `202` + `Location` of the job.
pub async fn duplicate_handler(
    State(state): State<AdminEndpointsState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    load(&state, &name).await?;
    let request: DuplicateRequest = json_body(&body, "duplicate request")?;
    let started = state.duplicate_endpoint(&name, request, &actor).await?;
    let mut r = (StatusCode::ACCEPTED, Json(json!(started))).into_response();
    if let Ok(v) = HeaderValue::from_str(&started.status_url) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    Ok(r)
}

/// `POST /admin/endpoints/{name}/$test-auth`. Body: empty (the stored auth
/// section), an `EndpointAuth` (unsaved settings), or
/// `{"auth": EndpointAuth?, "token": "<jwt>"?}` to also check a token.
pub async fn test_auth_handler(
    State(state): State<AdminEndpointsState>,
    Path(name): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    let (auth, token) = auth_body(&body)?;
    let report = state
        .test_endpoint_auth(Some(&name), auth, token.as_deref())
        .await?;
    Ok(Json(json!(report)).into_response())
}

/// Body of `$test-auth` / `$verify-auth`: empty, an `EndpointAuth`, or
/// `{"auth": EndpointAuth?, "token": "<jwt>"?}`.
fn auth_body(body: &[u8]) -> ApiResult<(Option<EndpointAuth>, Option<String>)> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok((None, None));
    }
    let obj = spec_input::json_object(body)?;
    if obj.contains_key("token") || obj.contains_key("auth") {
        let token = match obj.get("token") {
            None | Some(Value::Null) => None,
            Some(Value::String(t)) if t.trim().is_empty() => None,
            Some(Value::String(t)) => Some(t.clone()),
            Some(_) => {
                return Err(ApiError::invalid_field("token", "must be a JWT string"));
            }
        };
        let auth =
            match obj.get("auth") {
                None | Some(Value::Null) => None,
                Some(a) => Some(serde_json::from_value::<EndpointAuth>(a.clone()).map_err(
                    |e| ApiError::invalid_field("auth", format!("invalid auth section: {e}")),
                )?),
            };
        Ok((auth, token))
    } else {
        Ok((Some(json_body(body, "auth section")?), None))
    }
}

/// `POST /admin/endpoints/{name}/$verify-auth`: the step-by-step
/// verification (`VerifyReport`, kind `endpoint-auth`) of the stored auth
/// (empty body; the effective server settings when the endpoint inherits
/// them) or of unsaved settings (`{"auth": …, "token": …}`). Always `200`
/// with the report once the body parses; failures are steps.
pub async fn verify_auth_handler(
    State(state): State<AdminEndpointsState>,
    Path(name): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    let (auth, token) = auth_body(&body)?;
    let report = state
        .verify_endpoint_auth(Some(&name), auth, token.as_deref())
        .await?;
    Ok(Json(json!(report)).into_response())
}

/// `POST /admin/endpoints/$verify-auth`: [`verify_auth_handler`] for an
/// endpoint that does not exist yet (the new-endpoint wizard); an auth
/// section is required.
pub async fn verify_new_auth_handler(
    State(state): State<AdminEndpointsState>,
    body: Bytes,
) -> ApiResult<Response> {
    let (auth, token) = auth_body(&body)?;
    let report = state
        .verify_endpoint_auth(None, auth, token.as_deref())
        .await?;
    Ok(Json(json!(report)).into_response())
}

/// Body of `POST /admin/endpoints/$verify-terminology`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyTerminologyBody {
    /// Terminology server base URL.
    pub url: String,
    /// Bearer token the server is called with.
    #[serde(default)]
    pub token: Option<String>,
    /// Request timeout (default 10).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// `POST /admin/endpoints/$verify-terminology` → `VerifyReport` (kind
/// `terminology`).
pub async fn verify_terminology_handler(
    State(state): State<AdminEndpointsState>,
    body: Bytes,
) -> ApiResult<Response> {
    let b: VerifyTerminologyBody = json_body(&body, "terminology verification request")?;
    let report = state
        .verify_terminology(&b.url, b.token.as_deref(), b.timeout_secs)
        .await?;
    Ok(Json(json!(report)).into_response())
}

// ------------------------------------------------------------------ stats / audit

/// Query of `GET /admin/endpoints/{name}/stats`.
#[derive(Debug, Default, Deserialize)]
pub struct StatsQuery {
    /// Traffic window in seconds (60..=86400, default 3600).
    #[serde(default)]
    window: Option<i64>,
}

/// `GET /admin/endpoints/{name}/stats`
pub async fn stats_handler(
    State(state): State<AdminEndpointsState>,
    Path(name): Path<String>,
    Query(q): Query<StatsQuery>,
) -> ApiResult<Response> {
    let s = state
        .endpoint_stats(&name, q.window.unwrap_or(3600))
        .await?;
    Ok(Json(json!(s)).into_response())
}

/// Query of `GET /admin/endpoints/{name}/audit`.
#[derive(Debug, Default, Deserialize)]
pub struct AuditQuery {
    #[serde(default, rename = "_count")]
    count: Option<u32>,
    #[serde(default)]
    cursor: Option<String>,
}

/// `GET /admin/endpoints/{name}/audit` — a searchset Bundle, newest first.
pub async fn audit_handler(
    State(state): State<AdminEndpointsState>,
    Path(name): Path<String>,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Response> {
    let count = q.count.unwrap_or(50).clamp(1, 200);
    let page = state.endpoint_audit(&name, count, q.cursor).await?;
    let mut links = vec![json!({
        "relation": "self",
        "url": format!("admin/endpoints/{name}/audit?_count={count}"),
    })];
    if let Some(next) = &page.next {
        let enc: String = url::form_urlencoded::byte_serialize(next.as_bytes()).collect();
        links.push(json!({
            "relation": "next",
            "url": format!("admin/endpoints/{name}/audit?_count={count}&cursor={enc}"),
        }));
    }
    let entries: Vec<Value> = page
        .events
        .into_iter()
        .map(|r| json!({"resource": r, "search": {"mode": "match"}}))
        .collect();
    Ok((
        [(header::CONTENT_TYPE, "application/fhir+json")],
        Json(json!({
            "resourceType": "Bundle",
            "type": "searchset",
            "link": links,
            "entry": entries,
        })),
    )
        .into_response())
}

// ------------------------------------------------------------------ rewrap

/// `POST /admin/secrets/$rewrap`
pub async fn rewrap_handler(
    State(state): State<AdminEndpointsState>,
    Extension(actor): Extension<Actor>,
    console: Option<Extension<std::sync::Arc<helios_admin_auth::console::ConsoleAuth>>>,
) -> ApiResult<Response> {
    let console = console.map(|Extension(c)| super::authz::ConsoleRewrap(c));
    let extra: Vec<&dyn super::RewrapParticipant> = console
        .iter()
        .map(|c| c as &dyn super::RewrapParticipant)
        .collect();
    Ok(Json(json!(state.rewrap_secrets_with(&actor, &extra).await?)).into_response())
}
