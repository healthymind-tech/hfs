//! Handlers of `/admin/endpoints` (CRUD, status, history, duplicate,
//! auth probe, stats, audit) and `/admin/secrets/$rewrap`.

use axum::{
    Extension, Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use helios_audit::AuditAction;
use helios_endpoints::model::{EndpointAuth, EndpointSpec, EndpointStatus, FhirRelease};
use helios_endpoints::name::check_segment;
use helios_endpoints::secret::redact_sealed;
use helios_endpoints::sync::DuplicateRequest;
use helios_endpoints::{Actor, Endpoint, StoreError};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::audit::AdminEvent;
use super::error::{ApiError, ApiResult, Issue};
use super::hooks::UnavailableProbe;
use super::spec_input::{self, SecretInputs};
use super::{AdminEndpointsState, RewrapReport, etag, expected_revision, outbound, secrets};

// ------------------------------------------------------------------ helpers

/// Whether this binary serves `release`.
pub fn release_supported(release: FhirRelease) -> bool {
    match release {
        FhirRelease::R4 => cfg!(feature = "R4"),
        FhirRelease::R4B => cfg!(feature = "R4B"),
        FhirRelease::R5 => cfg!(feature = "R5"),
        FhirRelease::R6 => cfg!(feature = "R6"),
    }
}

fn check_release(spec: &EndpointSpec) -> ApiResult<()> {
    if release_supported(spec.fhir_version) {
        Ok(())
    } else {
        Err(ApiError::invalid_field(
            "fhirVersion",
            format!(
                "this server was built without FHIR {:?} support",
                spec.fhir_version
            ),
        ))
    }
}

/// The JSON of one endpoint: redacted, plus its public base URL.
pub(crate) fn view(state: &AdminEndpointsState, e: &Endpoint) -> Value {
    let mut v = e.redacted();
    if let Some(o) = v.as_object_mut() {
        o.insert(
            "publicBaseUrl".into(),
            json!(e.public_base(&state.inner().server_base)),
        );
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

pub(crate) async fn load(state: &AdminEndpointsState, name: &str) -> ApiResult<Endpoint> {
    state
        .inner()
        .store
        .get(name)
        .await?
        .ok_or_else(|| ApiError::endpoint_not_found(name))
}

/// Reloads this instance's routing snapshot after a write. A failure is
/// logged: the write stands and the watcher retries within the poll interval.
async fn reload(state: &AdminEndpointsState) {
    let i = state.inner();
    if let Err(e) = i.registry.reload_if_stale(i.store.as_ref()).await {
        tracing::warn!(error = %e, "endpoint snapshot reload after an admin write failed; the watcher will retry");
    }
}

async fn record(state: &AdminEndpointsState, actor: &Actor, event: AdminEvent<'_>) {
    state.inner().audit.record(actor, event).await;
}

fn require_key_ring_for(state: &AdminEndpointsState, inputs: &SecretInputs) -> ApiResult<()> {
    if secrets::sets_any(inputs) && state.inner().key_ring.is_none() {
        return Err(secrets::no_key_ring());
    }
    Ok(())
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
    let i = state.inner();
    let list = i.store.list(deleted).await?;
    let epoch = i.store.epoch().await?;
    let mut out = Vec::with_capacity(list.len());
    for e in &list {
        let mut v = view(&state, e);
        if stats && e.spec.status != EndpointStatus::Deleted {
            if let Some(ops) = &i.tenant_ops {
                match ops.stats(e.tenant_id()).await {
                    Ok(s) => v["stats"] = json!(s),
                    Err(err) => v["stats"] = json!({"error": err.to_string()}),
                }
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

// ------------------------------------------------------------------ create

/// `POST /admin/endpoints`
pub async fn create_handler(
    State(state): State<AdminEndpointsState>,
    Extension(actor): Extension<Actor>,
    body: Bytes,
) -> ApiResult<Response> {
    let i = state.inner();
    let obj = spec_input::json_object(&body)?;
    let input = spec_input::parse_create(obj)?;
    let spec = input.spec;
    if !matches!(
        spec.status,
        EndpointStatus::Enabled | EndpointStatus::Disabled
    ) {
        return Err(ApiError::invalid_field(
            "status",
            "a new endpoint is enabled or disabled (provisioning is set by $duplicate)",
        ));
    }
    check_release(&spec)?;
    require_key_ring_for(&state, &input.secrets)?;
    // Dry run of the secret inputs (sections present, key ring) before
    // anything is stored; the real seal needs the id the store assigns.
    secrets::apply(
        &mut spec.clone(),
        "pending",
        &input.secrets,
        i.key_ring.as_deref(),
    )?;
    let warnings = outbound::check_spec(&i.outbound, i.resolver.as_ref(), &spec, None).await?;
    let live = i.store.list(false).await?;
    if live.len() >= i.max_endpoints {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "business-rule",
            format!(
                "this deployment already has {} endpoints (limit {})",
                live.len(),
                i.max_endpoints
            ),
        ));
    }
    let mut created = i.store.create(spec, &actor).await?;
    // Secrets are bound to the endpoint id, which the store assigns: seal
    // them now and store them as revision 2.
    if !input.secrets.is_empty() {
        let mut with_secrets = created.spec.clone();
        let sealed = secrets::apply(
            &mut with_secrets,
            &created.id,
            &input.secrets,
            i.key_ring.as_deref(),
        );
        let stored = match sealed {
            Ok(()) => i
                .store
                .update(created.name(), created.revision, with_secrets, &actor)
                .await
                .map_err(ApiError::from),
            Err(e) => Err(e),
        };
        match stored {
            Ok(e) => created = e,
            Err(err) => {
                // Do not leave a half-configured endpoint behind.
                let _ = i
                    .store
                    .set_status(
                        created.name(),
                        created.revision,
                        EndpointStatus::Deleted,
                        &actor,
                    )
                    .await;
                reload(&state).await;
                return Err(err);
            }
        }
    }
    reload(&state).await;
    record(
        &state,
        &actor,
        AdminEvent {
            action: AuditAction::Create,
            outcome: "0",
            endpoint: Some(created.name()),
            description: format!(
                "created endpoint {} (path /{}, tenant {})",
                created.name(),
                created.spec.path,
                created.tenant_id()
            ),
            details: vec![
                ("revision", created.revision.to_string()),
                ("change", "create".into()),
                ("endpointId", created.id.clone()),
            ],
        },
    )
    .await;
    Ok(endpoint_response(
        &state,
        StatusCode::CREATED,
        &created,
        warnings,
    ))
}

// ------------------------------------------------------------------ replace / patch

async fn store_update(
    state: &AdminEndpointsState,
    actor: &Actor,
    current: Endpoint,
    expected: u64,
    raw_status_given: bool,
    input: spec_input::SpecInput,
    change: &str,
) -> ApiResult<Response> {
    let i = state.inner();
    let mut spec = input.spec;
    if raw_status_given && spec.status != current.spec.status {
        return Err(ApiError::invalid_field(
            "status",
            "status changes go through $enable, $disable and DELETE",
        ));
    }
    spec.status = current.spec.status;
    check_release(&spec)?;
    require_key_ring_for(state, &input.secrets)?;
    let warnings =
        outbound::check_spec(&i.outbound, i.resolver.as_ref(), &spec, Some(&current.spec)).await?;
    secrets::apply(
        &mut spec,
        &current.id,
        &input.secrets,
        i.key_ring.as_deref(),
    )?;
    let updated = i
        .store
        .update(current.name(), expected, spec, actor)
        .await?;
    reload(state).await;
    record(
        state,
        actor,
        AdminEvent {
            action: AuditAction::Update,
            outcome: "0",
            endpoint: Some(updated.name()),
            description: format!(
                "{change} endpoint {} (revision {} → {})",
                updated.name(),
                expected,
                updated.revision
            ),
            details: vec![
                ("revision", updated.revision.to_string()),
                ("change", change.to_string()),
                (
                    "secrets",
                    input
                        .secrets
                        .iter()
                        .map(|(k, v)| format!("{k}:{}", secret_verb(v)))
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            ],
        },
    )
    .await;
    Ok(endpoint_response(state, StatusCode::OK, &updated, warnings))
}

fn secret_verb(i: &helios_endpoints::SecretInput) -> &'static str {
    match i {
        helios_endpoints::SecretInput::Set(_) => "set",
        helios_endpoints::SecretInput::Keep => "keep",
        helios_endpoints::SecretInput::Clear => "clear",
    }
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
    let input = spec_input::parse_replace(obj, &current.spec)?;
    store_update(&state, &actor, current, expected, given, input, "replaced").await
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
    let obj = spec_input::json_object(&body)?;
    let given = status_given(&obj);
    let input = spec_input::parse_patch(obj, &current.spec)?;
    store_update(&state, &actor, current, expected, given, input, "patched").await
}

// ------------------------------------------------------------------ status

async fn set_status(
    state: &AdminEndpointsState,
    actor: &Actor,
    name: &str,
    headers: &HeaderMap,
    status: EndpointStatus,
) -> ApiResult<Response> {
    let i = state.inner();
    let current = load(state, name).await?;
    let expected = expected_revision(headers, current.revision, false)?;
    if current.spec.status == status {
        return Ok(endpoint_response(
            state,
            StatusCode::OK,
            &current,
            Vec::new(),
        ));
    }
    let updated = i.store.set_status(name, expected, status, actor).await?;
    reload(state).await;
    record(
        state,
        actor,
        AdminEvent {
            action: AuditAction::Update,
            outcome: "0",
            endpoint: Some(name),
            description: format!(
                "endpoint {name}: {} → {}",
                current.spec.status.as_str(),
                status.as_str()
            ),
            details: vec![
                ("revision", updated.revision.to_string()),
                ("change", "status".into()),
                ("status", status.as_str().into()),
            ],
        },
    )
    .await;
    Ok(endpoint_response(
        state,
        StatusCode::OK,
        &updated,
        Vec::new(),
    ))
}

/// `POST /admin/endpoints/{name}/$enable`
pub async fn enable_handler(
    State(state): State<AdminEndpointsState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    set_status(&state, &actor, &name, &headers, EndpointStatus::Enabled).await
}

/// `POST /admin/endpoints/{name}/$disable`
pub async fn disable_handler(
    State(state): State<AdminEndpointsState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    set_status(&state, &actor, &name, &headers, EndpointStatus::Disabled).await
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

/// `DELETE /admin/endpoints/{name}?confirm={name}[&purge=true]`
///
/// Tombstones the endpoint (its name, path and tenant become reusable). With
/// `purge=true` the endpoint is first disabled (so it stops taking writes),
/// then its tenant's data is purged (the same storage purge as
/// `DELETE /admin/tenants/{id}?purge=true`), then it is tombstoned — in the
/// request, so the tenant cannot be re-bound to a new endpoint while a purge
/// is still running. A failed purge leaves the endpoint disabled; retrying
/// the DELETE finishes it.
pub async fn delete_handler(
    State(state): State<AdminEndpointsState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    Query(q): Query<DeleteQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let i = state.inner();
    if q.confirm.as_deref() != Some(name.as_str()) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "required",
            format!("deleting an endpoint needs ?confirm={name}"),
        ));
    }
    let current = load(&state, &name).await?;
    let mut expected = expected_revision(&headers, current.revision, false)?;
    if current.spec.is_default() {
        return Err(StoreError::Forbidden(
            "the default endpoint cannot be deleted (disable it instead)".into(),
        )
        .into());
    }
    if current.spec.managed_by == helios_endpoints::ManagedBy::Env {
        return Err(StoreError::Forbidden(
            "endpoint is managed by HFS_ENDPOINTS_FILE; edit the file and restart".into(),
        )
        .into());
    }
    let mut removed: Option<u64> = None;
    if q.purge {
        let ops = i.tenant_ops.as_ref().ok_or_else(|| {
            ApiError::not_implemented("tenant purge is not available in this server")
        })?;
        if current.spec.status != EndpointStatus::Disabled {
            let disabled = i
                .store
                .set_status(&name, expected, EndpointStatus::Disabled, &actor)
                .await?;
            expected = disabled.revision;
            reload(&state).await;
        }
        match ops.purge(current.tenant_id()).await {
            Ok(n) => removed = Some(n),
            Err(e) => {
                record(
                    &state,
                    &actor,
                    AdminEvent {
                        action: AuditAction::Delete,
                        outcome: "8",
                        endpoint: Some(&name),
                        description: format!(
                            "purge of endpoint {name} (tenant {}) failed: {e}; the endpoint is left disabled",
                            current.tenant_id()
                        ),
                        details: vec![("change", "purge".into())],
                    },
                )
                .await;
                return Err(e);
            }
        }
    }
    let gone = i
        .store
        .set_status(&name, expected, EndpointStatus::Deleted, &actor)
        .await?;
    reload(&state).await;
    record(
        &state,
        &actor,
        AdminEvent {
            action: AuditAction::Delete,
            outcome: "0",
            endpoint: Some(&name),
            description: match removed {
                Some(n) => format!(
                    "deleted endpoint {name} and purged tenant {} ({n} resources)",
                    current.tenant_id()
                ),
                None => format!(
                    "deleted endpoint {name} (tenant {} data kept)",
                    current.tenant_id()
                ),
            },
            details: vec![
                ("revision", gone.revision.to_string()),
                ("change", "delete".into()),
                ("purged", removed.is_some().to_string()),
            ],
        },
    )
    .await;
    Ok(Json(json!({
        "endpoint": view(&state, &gone),
        "purged": removed.is_some(),
        "resourcesRemoved": removed,
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

/// `GET /admin/endpoints/{name}/history` — the live endpoint's revisions, or
/// the most recently deleted endpoint of that name.
pub async fn history_handler(
    State(state): State<AdminEndpointsState>,
    Path(name): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Response> {
    let i = state.inner();
    let e = match i.store.get(&name).await? {
        Some(e) => e,
        None => i
            .store
            .list(true)
            .await?
            .into_iter()
            .filter(|e| e.name() == name)
            .max_by_key(|e| e.updated_at)
            .ok_or_else(|| ApiError::endpoint_not_found(&name))?,
    };
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let revisions = i.store.history(&e.id, limit).await?;
    let mut v =
        serde_json::to_value(&revisions).map_err(|err| ApiError::internal(err.to_string()))?;
    redact_sealed(&mut v);
    Ok(Json(json!({"name": name, "id": e.id, "revisions": v})).into_response())
}

// ------------------------------------------------------------------ duplicate

/// `POST /admin/endpoints/{name}/$duplicate` — validates the request and
/// hands it to the sync service (`202` + `Location` of the job).
pub async fn duplicate_handler(
    State(state): State<AdminEndpointsState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    let i = state.inner();
    let source = load(&state, &name).await?;
    let obj = spec_input::json_object(&body)?;
    let request: DuplicateRequest = serde_json::from_value(Value::Object(obj)).map_err(|e| {
        ApiError::unprocessable(vec![Issue {
            code: "invalid",
            message: format!("invalid duplicate request: {e}"),
            field: None,
        }])
    })?;
    let mut issues = Vec::new();
    if let Err(e) = check_segment(&request.name) {
        issues.push(Issue::field("invalid", "name", format!("name: {e}")));
    }
    let path = request.path.clone().unwrap_or_else(|| request.name.clone());
    if let Err(e) = check_segment(&path) {
        issues.push(Issue::field("invalid", "path", format!("path: {e}")));
    }
    if let Some(d) = &request.display_name
        && (d.trim().is_empty() || d.chars().count() > 120)
    {
        issues.push(Issue::field(
            "invalid",
            "displayName",
            "must be 1-120 characters",
        ));
    }
    if !issues.is_empty() {
        return Err(ApiError::unprocessable(issues));
    }
    for e in i.store.list(false).await? {
        let clash = if e.name() == request.name {
            Some(("name", request.name.as_str()))
        } else if e.spec.path.as_str() == path {
            Some(("path", path.as_str()))
        } else if e.tenant_id() == request.name {
            Some(("storage.tenantId", request.name.as_str()))
        } else {
            None
        };
        if let Some((field, value)) = clash {
            return Err(StoreError::Duplicate {
                field,
                value: value.to_string(),
                by: e.name().to_string(),
            }
            .into());
        }
    }
    if request.secrets && i.key_ring.is_none() && !secrets::sealed_fields(&source.spec).is_empty() {
        return Err(secrets::no_key_ring());
    }
    let started = i.duplicate.start(&source, request.clone(), &actor).await?;
    reload(&state).await;
    record(
        &state,
        &actor,
        AdminEvent {
            action: AuditAction::Create,
            outcome: "0",
            endpoint: Some(&name),
            description: format!(
                "duplicate of endpoint {name} as {} started (job {}, data {:?})",
                request.name, started.job_id, request.data
            ),
            details: vec![
                ("change", "duplicate".into()),
                ("target", request.name.clone()),
                ("job", started.job_id.clone()),
            ],
        },
    )
    .await;
    let mut r = (StatusCode::ACCEPTED, Json(json!(started))).into_response();
    if let Ok(v) = HeaderValue::from_str(&started.status_url) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    Ok(r)
}

// ------------------------------------------------------------------ test auth

/// `POST /admin/endpoints/{name}/$test-auth` — probes the endpoint's stored
/// auth section, or an unsaved `EndpointAuth` sent as the body (the console's
/// "Test" button before saving). URLs pass the SSRF policy first.
pub async fn test_auth_handler(
    State(state): State<AdminEndpointsState>,
    Path(name): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    let i = state.inner();
    let current = load(&state, &name).await?;
    let auth: EndpointAuth = if body.iter().all(u8::is_ascii_whitespace) {
        current.spec.auth.clone().ok_or_else(|| {
            ApiError::invalid_field(
                "auth",
                "the endpoint inherits the server's authentication (HFS_AUTH_*); there is no endpoint auth to test",
            )
        })?
    } else {
        let obj = spec_input::json_object(&body)?;
        serde_json::from_value(Value::Object(obj)).map_err(|e| {
            ApiError::unprocessable(vec![Issue {
                code: "invalid",
                message: format!("invalid auth section: {e}"),
                field: None,
            }])
        })?
    };
    let mut probe_spec = current.spec.clone();
    probe_spec.auth = Some(auth.clone());
    probe_spec.terminology = None;
    if auth.issuer.as_deref().is_none_or(|s| s.trim().is_empty()) {
        return Err(ApiError::unprocessable(vec![Issue::field(
            "required",
            "auth.issuer",
            "an issuer is required to test authentication",
        )]));
    }
    // Always checked (the probe connects now), even if unchanged.
    outbound::check_spec(&i.outbound, i.resolver.as_ref(), &probe_spec, None).await?;
    let report = i.auth_probe.test(&auth).await;
    if UnavailableProbe::is_placeholder(&report) {
        return Err(ApiError::not_implemented(
            "the auth probe is not installed in this server",
        ));
    }
    Ok(Json(json!(report)).into_response())
}

// ------------------------------------------------------------------ stats

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
    let i = state.inner();
    let e = load(&state, &name).await?;
    let ops = i.tenant_ops.as_ref().ok_or_else(|| {
        ApiError::not_implemented("endpoint statistics are not available in this server")
    })?;
    let stats = ops.stats(e.tenant_id()).await?;
    let window = q.window.unwrap_or(3600).clamp(60, 86_400);
    let traffic = helios_observability::reqlog::per_tenant(window)
        .into_iter()
        .find(|t| t.tenant == e.tenant_id());
    let traffic = match traffic {
        Some(t) => json!({
            "windowSeconds": window,
            "samples": t.sample_count,
            "requestsPerMinute": t.requests_per_second * 60.0,
            "p95Ms": t.p95_ms,
            "errorRate": t.error_rate,
        }),
        None => json!({"windowSeconds": window, "samples": 0, "requestsPerMinute": 0.0,
                       "p95Ms": 0.0, "errorRate": 0.0}),
    };
    Ok(Json(json!({
        "name": e.name(),
        "tenant": e.tenant_id(),
        "status": e.spec.status.as_str(),
        "resources": stats.resources,
        "total": stats.total,
        "lastWrite": stats.last_write,
        "traffic": traffic,
        "trafficScope": "this-instance",
    }))
    .into_response())
}

// ------------------------------------------------------------------ audit

/// Query of `GET /admin/endpoints/{name}/audit`.
#[derive(Debug, Default, Deserialize)]
pub struct AuditQuery {
    #[serde(default, rename = "_count")]
    count: Option<u32>,
    #[serde(default)]
    cursor: Option<String>,
}

/// `GET /admin/endpoints/{name}/audit` — a searchset Bundle of the
/// endpoint tenant's AuditEvents, newest first.
pub async fn audit_handler(
    State(state): State<AdminEndpointsState>,
    Path(name): Path<String>,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Response> {
    let i = state.inner();
    let e = load(&state, &name).await?;
    let search = i
        .audit_search
        .as_ref()
        .ok_or_else(|| ApiError::not_implemented("audit search is not available in this server"))?;
    let count = q.count.unwrap_or(50).clamp(1, 200);
    let page = search.search(e.tenant_id(), count, q.cursor).await?;
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

/// `POST /admin/secrets/$rewrap` — re-seals every endpoint secret (and
/// those of the registered participants) under the active key.
pub async fn rewrap_handler(
    State(state): State<AdminEndpointsState>,
    Extension(actor): Extension<Actor>,
) -> ApiResult<Response> {
    let i = state.inner();
    let ring = i.key_ring.clone().ok_or_else(secrets::no_key_ring)?;
    let mut report = RewrapReport {
        scope: "endpoints".into(),
        ..RewrapReport::default()
    };
    let mut touched = false;
    for e in i.store.list(false).await? {
        let mut spec = e.spec.clone();
        let out = secrets::rewrap(&mut spec, &e.id, &ring);
        report.current += out.current;
        report.failed.extend(
            out.failed
                .iter()
                .map(|f| format!("endpoint {}/{f}", e.name())),
        );
        if out.rewrapped.is_empty() {
            continue;
        }
        match i.store.update(e.name(), e.revision, spec, &actor).await {
            Ok(_) => {
                touched = true;
                report.rewrapped += out.rewrapped.len();
            }
            Err(err) => report.failed.push(format!("endpoint {}: {err}", e.name())),
        }
    }
    if touched {
        reload(&state).await;
    }
    let mut reports = vec![report];
    for p in &i.rewrap_participants {
        reports.push(p.rewrap(&ring, &actor).await);
    }
    let rewrapped: usize = reports.iter().map(|r| r.rewrapped).sum();
    let failed: usize = reports.iter().map(|r| r.failed.len()).sum();
    record(
        &state,
        &actor,
        AdminEvent {
            action: AuditAction::Update,
            outcome: if failed == 0 { "0" } else { "4" },
            endpoint: None,
            description: format!(
                "re-sealed {rewrapped} secret(s) under key {} ({failed} failure(s))",
                ring.active_key_id()
            ),
            details: vec![("change", "rewrap".into())],
        },
    )
    .await;
    Ok(Json(json!({
        "activeKey": ring.active_key_id(),
        "rewrapped": rewrapped,
        "failed": failed,
        "reports": reports,
    }))
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r4_is_supported_in_the_default_build() {
        assert!(release_supported(FhirRelease::R4));
        let spec = EndpointSpec::named(helios_endpoints::EndpointName::parse("a").unwrap(), "A");
        assert!(check_release(&spec).is_ok());
        assert_eq!(secret_verb(&helios_endpoints::SecretInput::Keep), "keep");
        assert_eq!(secret_verb(&helios_endpoints::SecretInput::Clear), "clear");
        assert_eq!(
            secret_verb(&helios_endpoints::SecretInput::Set("x".into())),
            "set"
        );
    }

    #[cfg(not(feature = "R6"))]
    #[test]
    fn missing_release_is_refused() {
        let mut spec =
            EndpointSpec::named(helios_endpoints::EndpointName::parse("a").unwrap(), "A");
        spec.fhir_version = FhirRelease::R6;
        let e = check_release(&spec).unwrap_err();
        assert_eq!(e.issues[0].field.as_deref(), Some("fhirVersion"));
    }
}
