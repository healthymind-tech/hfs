//! SMART access refinements the handlers do not make.
//!
//! Patient-context confinement (`patient/` scopes, the launch patient's
//! compartment) is decided once, by [`authz_middleware`](super::auth::authz_middleware)
//! (and per entry by the batch/transaction handler) through
//! [`helios_auth::SmartScopePolicy::check`], and enforced by the handlers
//! through [`crate::patient_compartment::PatientCompartment`] (upstream
//! #1618): search filters, payload membership on reads and writes, include
//! filtering, per-entry confinement in batch and transaction, and the shared
//! reference types (`HFS_AUTH_PATIENT_NON_COMPARTMENT`). This layer does not
//! repeat any of that. It adds the two things that path does
//! not cover:
//!
//! * **SMART v2 granular scopes** (`?param=value`). The grant of the context
//!   the policy picks (a `user/`/`system/` scope over a `patient/` one) is
//!   constrained when every scope of that context covering the type carries a
//!   constraint. Searches then get the constraint appended (OR-ed across
//!   scopes that constrain the same parameter); reads, instance history,
//!   creates, updates and deletes must match it; bundles are filtered. What
//!   cannot be checked against a constraint — batch/transaction, PATCH,
//!   conditional writes, type history, other operations — is refused when the
//!   type's grant is constrained.
//! * **Type leakage through bundles.** A token that does not read every type
//!   still gets other types back from `_include`/`_revinclude`,
//!   `$everything`, compartment `*` searches and searches nested in a batch
//!   response; entries of a type the token cannot read (or that miss its
//!   granular constraint) are removed.
//!
//! It sits inside the FHIR router (it needs the tenant's search-parameter
//! registry) in front of route matching, after authorization. Requests
//! without a [`Principal`] and tokens that read every type unconstrained pass
//! straight through.

use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    extract::{FromRequestParts, Request, State},
    http::{HeaderValue, Method, StatusCode, Uri, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use helios_auth::scope::ScopeContext;
use helios_auth::{Principal, ScopeSet, SmartPermissions};
use helios_persistence::core::{ResourceStorage, SearchProvider};
use helios_persistence::search::{IndexValue, SearchParameterExtractor, SearchParameterRegistry};
use parking_lot::RwLock;
use serde_json::Value;
use tracing::{debug, warn};

use crate::extractors::TenantExtractor;
use crate::state::AppState;

/// What a request does, as far as this layer cares. Paths are tenant-stripped.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    /// `GET /T`, `POST /T/_search`.
    Search { ty: String, post: bool },
    /// `GET /T/id`, `GET /T/id/_history[/vid]`.
    Read { ty: String, id: String },
    /// `GET /T/_history`.
    TypeHistory { ty: String },
    /// `GET /_history`.
    SystemHistory,
    /// `GET /C/id/T` (T may be `*`).
    Compartment {
        ctype: String,
        cid: String,
        target: String,
    },
    /// `/Patient/$everything` (`id` None) or `/Patient/id/$everything`.
    Everything { id: Option<String> },
    /// `POST /T`.
    Create { ty: String },
    /// `PUT /T/id`.
    Update { ty: String, id: String },
    /// `DELETE /T/id`.
    Delete { ty: String, id: String },
    /// `PATCH /T/id`.
    Patch { ty: String },
    /// `PUT|PATCH|DELETE /T?criteria`.
    ConditionalWrite { ty: String },
    /// Any `$validate`: persists nothing and returns no stored data.
    Validate,
    /// `GET|POST /Patient/$export` (type level). Upstream's handler pins a
    /// patient-scoped export to the launch patient and its permitted types.
    PatientExport,
    /// Any other type- or instance-level `$operation`.
    Operation { ty: String },
    /// `POST /` (batch / transaction).
    Batch,
    /// Everything else (system-level paths, handled by the authz layer).
    Other,
}

fn classify(method: &Method, path: &str) -> Route {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return if *method == Method::POST {
            Route::Batch
        } else {
            Route::Other
        };
    }
    let first = segs[0];
    if segs == ["_history"] {
        return Route::SystemHistory;
    }
    if first.starts_with('_') || first.starts_with('$') || !is_type_name(first) {
        return Route::Other;
    }
    let ty = first.to_string();
    if let Some(last) = segs.last().filter(|s| s.starts_with('$')) {
        return match *last {
            "$validate" => Route::Validate,
            "$everything" if ty == "Patient" && segs.len() <= 3 => Route::Everything {
                id: (segs.len() == 3).then(|| segs[1].to_string()),
            },
            "$export" if ty == "Patient" && segs.len() == 2 => Route::PatientExport,
            _ => Route::Operation { ty },
        };
    }
    let read = matches!(*method, Method::GET | Method::HEAD);
    match segs.len() {
        1 if read => Route::Search { ty, post: false },
        1 if *method == Method::POST => Route::Create { ty },
        1 if matches!(*method, Method::PUT | Method::PATCH | Method::DELETE) => {
            Route::ConditionalWrite { ty }
        }
        2 if segs[1] == "_search" && *method == Method::POST => Route::Search { ty, post: true },
        2 if segs[1] == "_history" && read => Route::TypeHistory { ty },
        2 if read => Route::Read {
            ty,
            id: segs[1].to_string(),
        },
        2 if *method == Method::PUT => Route::Update {
            ty,
            id: segs[1].to_string(),
        },
        2 if *method == Method::DELETE => Route::Delete {
            ty,
            id: segs[1].to_string(),
        },
        2 if *method == Method::PATCH => Route::Patch { ty },
        3 if read && segs[2] == "_history" => Route::Read {
            ty,
            id: segs[1].to_string(),
        },
        3 if read && !segs[2].starts_with('_') => Route::Compartment {
            ctype: ty,
            cid: segs[1].to_string(),
            target: segs[2].to_string(),
        },
        4 if read && segs[2] == "_history" => Route::Read {
            ty,
            id: segs[1].to_string(),
        },
        _ => Route::Other,
    }
}

fn is_type_name(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Whether a query asks for other resource types to be included.
fn asks_for_includes(query: &str) -> bool {
    url::form_urlencoded::parse(query.as_bytes())
        .any(|(k, _)| k.starts_with("_include") || k.starts_with("_revinclude"))
}

/// What a token may see of one type for one permission.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reach {
    /// No scope covers it.
    Denied,
    /// Covered without a granular constraint (compartment confinement, if
    /// any, is the handlers' business).
    Free,
    /// Covered only by granular scopes: a resource must match every
    /// constraint of at least one of these sets.
    Constrained(Vec<Vec<(String, String)>>),
}

/// The token's [`Reach`] for `perm` on `ty`. The context is chosen as
/// [`helios_auth::SmartScopePolicy::check`] chooses it: the `user/`/`system/`
/// scopes when any covers the type, else the `patient/` ones.
fn reach(scopes: &ScopeSet, ty: &str, perm: SmartPermissions) -> Reach {
    let covering: Vec<_> = scopes
        .scopes()
        .iter()
        .filter(|s| s.covers(ty, perm))
        .collect();
    if covering.is_empty() {
        return Reach::Denied;
    }
    let wide: Vec<_> = covering
        .iter()
        .copied()
        .filter(|s| s.context != ScopeContext::Patient)
        .collect();
    let chosen = if wide.is_empty() { covering } else { wide };
    if chosen.iter().any(|s| s.constraints.is_empty()) {
        return Reach::Free;
    }
    let mut sets: Vec<Vec<(String, String)>> = Vec::new();
    for s in chosen {
        if !sets.contains(&s.constraints) {
            sets.push(s.constraints.clone());
        }
    }
    Reach::Constrained(sets)
}

/// Reach for reading `ty` as a returned or included resource: read, else search.
fn read_reach(scopes: &ScopeSet, ty: &str) -> Reach {
    match reach(scopes, ty, SmartPermissions::READ) {
        Reach::Denied => reach(scopes, ty, SmartPermissions::SEARCH),
        other => other,
    }
}

/// Per-request evaluation state.
struct Ctx {
    principal: Principal,
    registry: Arc<RwLock<SearchParameterRegistry>>,
}

impl Ctx {
    fn matches_constraint(&self, resource: &Value, param: &str, wanted: &str) -> bool {
        let ty = resource
            .get("resourceType")
            .and_then(Value::as_str)
            .unwrap_or("");
        if param == "_id" {
            return wanted
                .split(',')
                .any(|w| resource.get("id").and_then(Value::as_str) == Some(w));
        }
        let Some(definition) = self.registry.read().get_param(ty, param) else {
            // Fail closed: a constraint on a parameter this server does not
            // know can never be shown to hold.
            return false;
        };
        let extractor = SearchParameterExtractor::new(Arc::clone(&self.registry));
        let Ok(values) = extractor.extract_for_param(resource, &definition) else {
            return false;
        };
        wanted.split(',').any(|alternative| {
            values
                .iter()
                .any(|v| value_matches(&v.value, alternative.trim()))
        })
    }

    fn resource_allowed(&self, resource: &Value, reach: &Reach) -> bool {
        match reach {
            Reach::Denied => false,
            Reach::Free => true,
            Reach::Constrained(sets) => sets.iter().any(|set| {
                set.iter()
                    .all(|(param, value)| self.matches_constraint(resource, param, value))
            }),
        }
    }

    /// Whether `resource` may be returned to this token.
    fn may_return(&self, resource: &Value) -> bool {
        let Some(ty) = resource.get("resourceType").and_then(Value::as_str) else {
            return false;
        };
        if ty == "OperationOutcome" {
            return true;
        }
        let reach = read_reach(&self.principal.scopes, ty);
        self.resource_allowed(resource, &reach)
    }

    /// Drops every entry this token may not see. Recurses into Bundles
    /// carried as entry resources (searches inside a batch response). With
    /// `drop_bare`, an entry without a resource (a history delete) is dropped
    /// too: nothing shows it meets the granular constraint.
    fn filter_bundle(&self, bundle: &mut Value, drop_bare: bool) -> usize {
        let Some(entries) = bundle.get_mut("entry").and_then(Value::as_array_mut) else {
            return 0;
        };
        let mut dropped = 0;
        let mut kept = Vec::with_capacity(entries.len());
        for mut entry in entries.drain(..) {
            let keep = match entry.get_mut("resource") {
                Some(resource)
                    if resource.get("resourceType").and_then(Value::as_str) == Some("Bundle") =>
                {
                    dropped += self.filter_bundle(resource, drop_bare);
                    true
                }
                Some(resource) => self.may_return(resource),
                None => !drop_bare,
            };
            if keep {
                kept.push(entry);
            } else {
                dropped += 1;
            }
        }
        *entries = kept;
        dropped
    }
}

/// Whether an extracted index value satisfies one constraint alternative
/// (`system|code`, `|code`, `code`, or a plain string).
fn value_matches(value: &IndexValue, wanted: &str) -> bool {
    match value {
        IndexValue::Token { system, code, .. } => match wanted.split_once('|') {
            Some(("", c)) => system.is_none() && code == c,
            Some((s, "")) => system.as_deref() == Some(s),
            Some((s, c)) => system.as_deref() == Some(s) && code == c,
            None => code == wanted,
        },
        IndexValue::String(s) => s.eq_ignore_ascii_case(wanted),
        IndexValue::Reference { reference, .. } => reference == wanted,
        _ => false,
    }
}

/// The search parameters that express a constrained grant, when they can be
/// expressed as one AND-ed query; `Err` when they cannot.
fn constraint_params(sets: &[Vec<(String, String)>]) -> Result<Vec<(String, String)>, String> {
    if sets.len() == 1 {
        return Ok(sets[0].clone());
    }
    let single: Vec<&(String, String)> = sets
        .iter()
        .filter_map(|c| (c.len() == 1).then(|| &c[0]))
        .collect();
    if single.len() == sets.len() && single.iter().all(|(p, _)| *p == single[0].0) {
        let values: Vec<&str> = single.iter().map(|(_, v)| v.as_str()).collect();
        return Ok(vec![(single[0].0.clone(), values.join(","))]);
    }
    Err("the token's granular scopes for this type constrain different search parameters and \
         cannot be combined into one search"
        .to_string())
}

fn forbidden(message: &str) -> Response {
    let body = serde_json::json!({
        "resourceType": "OperationOutcome",
        "issue": [{
            "severity": "error",
            "code": "forbidden",
            "diagnostics": message,
        }]
    });
    let mut response = (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "application/fhir+json")],
        body.to_string(),
    )
        .into_response();
    if let Ok(v) = HeaderValue::from_str(&format!(
        "Bearer realm=\"hfs\", error=\"insufficient_scope\", error_description=\"{}\"",
        message.replace(['"', '\\'], "'")
    )) {
        response.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    response
}

fn is_json(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("json"))
}

fn with_query(path: &str, query: &str) -> Option<Uri> {
    let pq = if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{query}")
    };
    pq.parse().ok()
}

fn append_params(query: &str, extra: &[(String, String)]) -> String {
    let mut out = query.to_string();
    for (k, v) in extra {
        let pair: String = url::form_urlencoded::Serializer::new(String::new())
            .append_pair(k, v)
            .finish();
        if !out.is_empty() {
            out.push('&');
        }
        out.push_str(&pair);
    }
    out
}

/// The SMART refinement middleware. See the module documentation.
pub async fn smart_access_middleware<S>(
    State(state): State<AppState<S>>,
    request: Request,
    next: Next,
) -> Response
where
    S: ResourceStorage + SearchProvider + Send + Sync + 'static,
{
    let Some(principal) = request.extensions().get::<Principal>().cloned() else {
        return next.run(request).await;
    };
    let constrained = principal.scopes.has_constrained_scopes();
    if !constrained && principal.scopes.reads_everything() {
        return next.run(request).await;
    }

    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();
    let route = classify(&method, &path);
    let multi_type = match &route {
        Route::Compartment { target, .. } if target == "*" => true,
        Route::Search { .. } | Route::Compartment { .. } => asks_for_includes(&query),
        Route::Everything { .. } | Route::Batch => true,
        _ => false,
    };
    let scopes = principal.scopes.clone();
    let granular = |ty: &str, perm: SmartPermissions| -> Option<Vec<Vec<(String, String)>>> {
        if !constrained {
            return None;
        }
        match reach(&scopes, ty, perm) {
            Reach::Constrained(sets) => Some(sets),
            _ => None,
        }
    };
    // Nothing granular about this request and nothing to filter: done.
    let route_granular = match &route {
        Route::Search { ty, .. } => granular(ty, SmartPermissions::SEARCH).is_some(),
        Route::Read { ty, .. } => {
            granular(ty, SmartPermissions::READ).is_some()
                || (reach(&scopes, ty, SmartPermissions::READ) == Reach::Denied
                    && granular(ty, SmartPermissions::SEARCH).is_some())
        }
        Route::TypeHistory { ty } => granular(ty, SmartPermissions::READ).is_some(),
        Route::Compartment { target, .. } if target != "*" => {
            granular(target, SmartPermissions::SEARCH).is_some()
        }
        Route::Create { ty } => granular(ty, SmartPermissions::CREATE).is_some(),
        Route::Update { ty, .. } => granular(ty, SmartPermissions::UPDATE).is_some(),
        Route::Delete { ty, .. } => granular(ty, SmartPermissions::DELETE).is_some(),
        Route::Patch { ty } => granular(ty, SmartPermissions::UPDATE).is_some(),
        Route::ConditionalWrite { ty } | Route::Operation { ty } => [
            SmartPermissions::READ,
            SmartPermissions::CREATE,
            SmartPermissions::UPDATE,
            SmartPermissions::DELETE,
        ]
        .into_iter()
        .any(|p| granular(ty, p).is_some()),
        Route::Batch => constrained && !principal.scopes.writes_and_reads_everything(),
        // Bulk export cannot apply a granular constraint to what it writes.
        Route::PatientExport => constrained && !principal.scopes.reads_everything(),
        _ => false,
    };
    if !route_granular && !multi_type {
        return next.run(request).await;
    }

    let (mut parts, body) = request.into_parts();
    let tenant = match TenantExtractor::from_request_parts(&mut parts, &state).await {
        Ok(tenant) => tenant,
        // Let the handler produce the tenant error it always produces.
        Err(_) => return next.run(Request::from_parts(parts, body)).await,
    };
    let ctx = Ctx {
        principal: principal.clone(),
        registry: state.storage().search_param_registry(tenant.context()),
    };
    let max_body = state.config().max_body_size;
    let request = Request::from_parts(parts, body);

    if !route_granular {
        let response = next.run(request).await;
        return filter_response(response, &ctx, false, max_body).await;
    }

    match route {
        Route::Other | Route::Validate | Route::SystemHistory => next.run(request).await,
        Route::Batch => forbidden(
            "batch and transaction requests are not available to tokens with granular scopes; \
             send the requests individually",
        ),
        Route::TypeHistory { .. } => forbidden(
            "type-level history is not available for a type the token reaches only through \
             granular scopes",
        ),
        Route::PatientExport => forbidden(
            "Patient/$export is not available to tokens with granular scopes",
        ),
        Route::ConditionalWrite { .. } | Route::Patch { .. } | Route::Operation { .. } => forbidden(
            "conditional writes, PATCH and operations are not available for a type the token \
             reaches only through granular scopes",
        ),
        Route::Search { ty, post } => {
            let sets = granular(&ty, SmartPermissions::SEARCH).unwrap_or_default();
            let extra = match constraint_params(&sets) {
                Ok(extra) => extra,
                Err(message) => return forbidden(&message),
            };
            let (mut parts, body) = request.into_parts();
            let mut query = query;
            if post {
                // Fold the form body into the query and search with GET, so
                // the constraint is applied by the same query parser.
                let bytes = match axum::body::to_bytes(body, max_body).await {
                    Ok(b) => b,
                    Err(_) => return forbidden("request body too large"),
                };
                let form = String::from_utf8_lossy(&bytes).trim().to_string();
                if !form.is_empty() {
                    query = if query.is_empty() {
                        form
                    } else {
                        format!("{query}&{form}")
                    };
                }
                parts.method = Method::GET;
                parts.headers.remove(header::CONTENT_TYPE);
                parts.headers.remove(header::CONTENT_LENGTH);
            }
            let new_path = format!("/{ty}");
            let new_query = append_params(&query, &extra);
            match with_query(&new_path, &new_query) {
                Some(uri) => parts.uri = uri,
                None => return forbidden("could not restrict the search to the token's scopes"),
            }
            debug!(path = %new_path, query = %new_query, "SMART access: granular search");
            let response = next.run(Request::from_parts(parts, Body::empty())).await;
            filter_response(response, &ctx, false, max_body).await
        }
        Route::Compartment { target, .. } => {
            let sets = granular(&target, SmartPermissions::SEARCH).unwrap_or_default();
            let extra = match constraint_params(&sets) {
                Ok(extra) => extra,
                Err(message) => return forbidden(&message),
            };
            let (mut parts, body) = request.into_parts();
            if let Some(uri) = with_query(&path, &append_params(&query, &extra)) {
                parts.uri = uri;
            }
            let response = next.run(Request::from_parts(parts, body)).await;
            filter_response(response, &ctx, false, max_body).await
        }
        Route::Everything { .. } => {
            let response = next.run(request).await;
            filter_response(response, &ctx, false, max_body).await
        }
        Route::Read { ty, .. } => {
            let reach = read_reach(&principal.scopes, &ty);
            let response = next.run(request).await;
            check_single_response(response, &ctx, &reach, max_body).await
        }
        Route::Create { ty } => {
            let reach = reach(&principal.scopes, &ty, SmartPermissions::CREATE);
            let (parts, body) = request.into_parts();
            let bytes = match axum::body::to_bytes(body, max_body).await {
                Ok(b) => b,
                Err(_) => return forbidden("request body too large"),
            };
            if !body_allowed(&bytes, &ty, &ctx, &reach) {
                return forbidden("the resource does not match the token's granular scope");
            }
            next.run(Request::from_parts(parts, Body::from(bytes))).await
        }
        Route::Update { ty, id } => {
            let reach = reach(&principal.scopes, &ty, SmartPermissions::UPDATE);
            let (mut parts, body) = request.into_parts();
            let bytes = match axum::body::to_bytes(body, max_body).await {
                Ok(b) => b,
                Err(_) => return forbidden("request body too large"),
            };
            if !body_allowed(&bytes, &ty, &ctx, &reach) {
                return forbidden("the updated resource would not match the token's granular scope");
            }
            match stored(&state, &mut parts, &ty, &id).await {
                Ok(Some(existing)) if !ctx.resource_allowed(&existing, &reach) => {
                    return forbidden(
                        "the stored resource does not match the token's granular scope",
                    );
                }
                Ok(_) => {}
                Err(response) => return response,
            }
            next.run(Request::from_parts(parts, Body::from(bytes))).await
        }
        Route::Delete { ty, id } => {
            let reach = reach(&principal.scopes, &ty, SmartPermissions::DELETE);
            let (mut parts, body) = request.into_parts();
            match stored(&state, &mut parts, &ty, &id).await {
                Ok(Some(existing)) if !ctx.resource_allowed(&existing, &reach) => {
                    return forbidden(
                        "the stored resource does not match the token's granular scope",
                    );
                }
                Ok(_) => {}
                Err(response) => return response,
            }
            next.run(Request::from_parts(parts, body)).await
        }
    }
}

/// The stored current version of `ty/id`, as JSON.
async fn stored<S>(
    state: &AppState<S>,
    parts: &mut axum::http::request::Parts,
    ty: &str,
    id: &str,
) -> Result<Option<Value>, Response>
where
    S: ResourceStorage + SearchProvider + Send + Sync + 'static,
{
    let tenant = match TenantExtractor::from_request_parts(parts, state).await {
        Ok(t) => t,
        Err(rejection) => return Err(rejection.into_response()),
    };
    match state.storage().read(tenant.context(), ty, id).await {
        Ok(found) => Ok(found.map(|r| r.content().clone())),
        Err(err) => {
            warn!(error = %err, "SMART access: could not read the stored resource");
            Ok(None)
        }
    }
}

fn body_allowed(bytes: &Bytes, ty: &str, ctx: &Ctx, reach: &Reach) -> bool {
    let Ok(resource) = serde_json::from_slice::<Value>(bytes) else {
        // XML or malformed: cannot be shown to match the constraint.
        return false;
    };
    if resource.get("resourceType").and_then(Value::as_str) != Some(ty) {
        return false;
    }
    ctx.resource_allowed(&resource, reach)
}

/// Checks a single-resource (or instance-history) response against `reach`.
async fn check_single_response(
    response: Response,
    ctx: &Ctx,
    reach: &Reach,
    max_body: usize,
) -> Response {
    if !response.status().is_success() || response.status() == StatusCode::NOT_MODIFIED {
        return response;
    }
    if !is_json(&response) {
        return forbidden(
            "a resource reached through granular scopes can only be returned as JSON; request \
             application/fhir+json",
        );
    }
    let (parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, max_body.max(64 * 1024 * 1024)).await else {
        return forbidden("response too large to check");
    };
    let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    if value.get("resourceType").and_then(Value::as_str) == Some("Bundle") {
        ctx.filter_bundle(&mut value, true);
        return rebuild(parts, &value);
    }
    if ctx.resource_allowed(&value, reach) {
        Response::from_parts(parts, Body::from(bytes))
    } else {
        forbidden("the resource does not match the token's granular scope")
    }
}

/// Filters a Bundle response; other responses pass unchanged.
async fn filter_response(response: Response, ctx: &Ctx, drop_bare: bool, max_body: usize) -> Response {
    if !response.status().is_success() {
        return response;
    }
    if !is_json(&response) {
        // A Bundle we cannot inspect would leak whatever it includes.
        return forbidden(
            "this response must be filtered for the token's scopes, which needs JSON; request \
             application/fhir+json",
        );
    }
    let (parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, max_body.max(64 * 1024 * 1024)).await else {
        return forbidden("response too large to filter");
    };
    let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    if value.get("resourceType").and_then(Value::as_str) != Some("Bundle") {
        return Response::from_parts(parts, Body::from(bytes));
    }
    let dropped = ctx.filter_bundle(&mut value, drop_bare);
    if dropped == 0 {
        return Response::from_parts(parts, Body::from(bytes));
    }
    debug!(dropped, "SMART access: entries outside the token's scopes removed");
    rebuild(parts, &value)
}

fn rebuild(mut parts: axum::http::response::Parts, value: &Value) -> Response {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::ETAG);
    Response::from_parts(parts, Body::from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_auth::ScopeSet;

    #[test]
    fn classifies_reads_searches_and_writes() {
        assert_eq!(
            classify(&Method::GET, "/Observation"),
            Route::Search {
                ty: "Observation".into(),
                post: false
            }
        );
        assert_eq!(
            classify(&Method::POST, "/Observation/_search"),
            Route::Search {
                ty: "Observation".into(),
                post: true
            }
        );
        assert_eq!(
            classify(&Method::GET, "/Observation/1"),
            Route::Read {
                ty: "Observation".into(),
                id: "1".into()
            }
        );
        assert_eq!(
            classify(&Method::GET, "/Observation/1/_history/2"),
            Route::Read {
                ty: "Observation".into(),
                id: "1".into()
            }
        );
        assert_eq!(
            classify(&Method::GET, "/Observation/_history"),
            Route::TypeHistory {
                ty: "Observation".into()
            }
        );
        assert_eq!(classify(&Method::GET, "/_history"), Route::SystemHistory);
        assert_eq!(classify(&Method::POST, "/"), Route::Batch);
        assert_eq!(
            classify(&Method::GET, "/Patient/p1/Observation"),
            Route::Compartment {
                ctype: "Patient".into(),
                cid: "p1".into(),
                target: "Observation".into()
            }
        );
        assert_eq!(
            classify(&Method::GET, "/Patient/p1/$everything"),
            Route::Everything {
                id: Some("p1".into())
            }
        );
        assert_eq!(
            classify(&Method::POST, "/Patient/$everything"),
            Route::Everything { id: None }
        );
        assert_eq!(classify(&Method::POST, "/Patient/$validate"), Route::Validate);
        assert_eq!(
            classify(&Method::PUT, "/Observation"),
            Route::ConditionalWrite {
                ty: "Observation".into()
            }
        );
        assert_eq!(
            classify(&Method::PATCH, "/Observation/1"),
            Route::Patch {
                ty: "Observation".into()
            }
        );
        assert_eq!(classify(&Method::GET, "/metadata"), Route::Other);
        assert_eq!(classify(&Method::GET, "/$export"), Route::Other);
    }

    #[test]
    fn constraint_params_or_join_one_parameter() {
        let c = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert_eq!(
            constraint_params(&[c(&[("category", "a")]), c(&[("category", "b")])]).unwrap(),
            vec![("category".to_string(), "a,b".to_string())]
        );
        assert!(constraint_params(&[c(&[("category", "a")]), c(&[("code", "b")])]).is_err());
        assert_eq!(
            constraint_params(&[c(&[("category", "a"), ("code", "x")])]).unwrap(),
            c(&[("category", "a"), ("code", "x")])
        );
    }

    #[test]
    fn reach_follows_the_widest_context_like_the_policy() {
        let s = |x: &str| ScopeSet::parse(x);
        let lab = "?category=laboratory";
        assert_eq!(reach(&s("system/*.rs"), "Observation", SmartPermissions::READ), Reach::Free);
        assert_eq!(reach(&s("patient/*.rs"), "Observation", SmartPermissions::READ), Reach::Free);
        assert_eq!(reach(&s("patient/Patient.rs"), "Observation", SmartPermissions::READ), Reach::Denied);
        assert_eq!(
            reach(&s(&format!("user/Observation.rs{lab}")), "Observation", SmartPermissions::SEARCH),
            Reach::Constrained(vec![vec![("category".into(), "laboratory".into())]])
        );
        // A wider (user/) grant decides, as SmartScopePolicy picks it over the
        // patient/ one: its constraint applies even though the patient/ scope
        // is unconstrained (never wider than either grant).
        assert!(matches!(
            reach(&s(&format!("patient/Observation.rs user/Observation.rs{lab}")), "Observation", SmartPermissions::READ),
            Reach::Constrained(_)
        ));
        // Patient-only granular scopes constrain within the compartment.
        assert!(matches!(
            reach(&s(&format!("patient/Observation.rs{lab}")), "Observation", SmartPermissions::READ),
            Reach::Constrained(_)
        ));
    }

    #[test]
    fn token_values_match_system_and_code_forms() {
        let token = IndexValue::Token {
            system: Some("http://sys".into()),
            code: "lab".into(),
            display: None,
            identifier_type_system: None,
            identifier_type_code: None,
        };
        assert!(value_matches(&token, "lab"));
        assert!(value_matches(&token, "http://sys|lab"));
        assert!(value_matches(&token, "http://sys|"));
        assert!(!value_matches(&token, "|lab"));
        assert!(!value_matches(&token, "http://other|lab"));
    }

    #[test]
    fn append_params_encodes_values() {
        assert_eq!(
            append_params("code=1", &[("category".into(), "http://x|lab".into())]),
            "code=1&category=http%3A%2F%2Fx%7Clab"
        );
        assert_eq!(append_params("", &[("_id".into(), "p".into())]), "_id=p");
    }
}
