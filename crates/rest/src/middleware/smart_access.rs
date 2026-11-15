//! SMART access control finer than a resource type.
//!
//! [`authz_middleware`](super::auth::authz_middleware) answers the coarse
//! question — does any scope cover *this operation on this type*? This layer
//! answers what a covered request may actually reach:
//!
//! * **Patient context.** A `patient/` scope reaches only the launch patient's
//!   compartment (the token's `patient` claim). Searches are rewritten into
//!   compartment searches (`/Observation?…` → `/Patient/{id}/Observation?…`,
//!   `/Patient?…` → `/Patient?…&_id={id}`), reads are checked against the
//!   returned resource, writes against the submitted (and stored) resource,
//!   and `_include`/`_revinclude`, `$everything` and history results are
//!   filtered entry by entry. Type and system history, conditional writes,
//!   PATCH, other `$`-operations and batch/transaction are refused for such a
//!   token: they cannot be confined to one compartment cheaply.
//! * **Granular scopes** (SMART v2 `?param=value`). Searches get the
//!   constraint appended (OR-ed across scopes that constrain the same
//!   parameter); reads and included resources must match it.
//! * **Type leakage through bundles.** A token that does not read every type
//!   still gets other types back from `_include`/`_revinclude`, `$everything`
//!   and searches nested in a batch response; those entries are dropped unless
//!   a scope lets the token read them.
//!
//! The layer sits inside the FHIR router (it needs the store and the tenant's
//! search-parameter registry) and in front of route matching, so a rewritten
//! search is routed as the compartment search it became. It does nothing for
//! a request without a [`Principal`] (auth off, anonymous read in
//! write-protect mode) or for a token whose scopes read everything.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    extract::{FromRequestParts, Request, State},
    http::{HeaderValue, Method, StatusCode, Uri, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use helios_auth::{Access, Grant, NonCompartmentPolicy, Principal, SmartPermissions};
use helios_fhir::FhirVersion;
use helios_persistence::core::patient_compartment::PatientCompartmentMatcher;
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
    TypeHistory,
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
        2 if segs[1] == "_history" && read => Route::TypeHistory,
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

/// A FHIR logical id: `[A-Za-z0-9\-\.]{1,64}`.
fn is_fhir_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// Whether a query asks for other resource types to be included.
fn asks_for_includes(query: &str) -> bool {
    url::form_urlencoded::parse(query.as_bytes())
        .any(|(k, _)| k.starts_with("_include") || k.starts_with("_revinclude"))
}

/// Whether the token's scopes need any of this layer's work at all.
fn principal_is_restricted(principal: &Principal) -> bool {
    let scopes = &principal.scopes;
    scopes.has_patient_scopes() || scopes.has_constrained_scopes()
}

/// Per-request evaluation state.
struct Ctx {
    principal: Principal,
    patient: Option<String>,
    registry: Arc<RwLock<SearchParameterRegistry>>,
    version: FhirVersion,
    non_compartment: NonCompartmentPolicy,
    matchers: HashMap<String, PatientCompartmentMatcher>,
}

impl Ctx {
    fn in_patient_compartment_type(&self, ty: &str) -> bool {
        ty == "Patient" || !helios_fhir::get_compartment_params(self.version, "Patient", ty).is_empty()
    }

    /// The token's reach for `perm` on `ty`, with `patient/` grants on a type
    /// outside the Patient compartment resolved by the configured policy.
    fn access(&self, ty: &str, perm: SmartPermissions) -> Access {
        let access = self.principal.scopes.access(ty, perm);
        let Access::Restricted(grants) = access else {
            return access;
        };
        if self.in_patient_compartment_type(ty) {
            return Access::Restricted(grants);
        }
        let readable = perm == SmartPermissions::READ || perm == SmartPermissions::SEARCH;
        let mut out: Vec<Grant> = Vec::new();
        for grant in grants {
            if grant.patient_compartment {
                if !(readable && self.non_compartment == NonCompartmentPolicy::Allow) {
                    continue;
                }
                if grant.constraints.is_empty() {
                    return Access::Full;
                }
                out.push(Grant {
                    patient_compartment: false,
                    constraints: grant.constraints,
                });
            } else {
                out.push(grant);
            }
        }
        if out.is_empty() {
            Access::Denied
        } else {
            Access::Restricted(out)
        }
    }

    /// Reach for reading `ty` as an included / returned resource: read, else search.
    fn read_access(&self, ty: &str) -> Access {
        match self.access(ty, SmartPermissions::READ) {
            Access::Denied => self.access(ty, SmartPermissions::SEARCH),
            other => other,
        }
    }

    fn patient_refs(&self) -> HashSet<String> {
        self.patient
            .iter()
            .map(|id| format!("Patient/{id}"))
            .collect()
    }

    fn is_compartment_member(&mut self, resource: &Value) -> bool {
        let Some(patient) = self.patient.clone() else {
            return false;
        };
        let ty = resource
            .get("resourceType")
            .and_then(Value::as_str)
            .unwrap_or("");
        if ty == "Patient" {
            return resource.get("id").and_then(Value::as_str) == Some(patient.as_str());
        }
        let refs = self.patient_refs();
        let version = self.version;
        let registry = Arc::clone(&self.registry);
        let matcher = self
            .matchers
            .entry(ty.to_string())
            .or_insert_with(|| PatientCompartmentMatcher::new(registry, version, ty));
        matcher.is_member(resource, &refs)
    }

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

    fn grant_matches(&mut self, grant: &Grant, resource: &Value) -> bool {
        if grant.patient_compartment && !self.is_compartment_member(resource) {
            return false;
        }
        grant
            .constraints
            .iter()
            .all(|(param, value)| self.matches_constraint(resource, param, value))
    }

    fn resource_allowed(&mut self, resource: &Value, access: &Access) -> bool {
        match access {
            Access::Denied => false,
            Access::Full => true,
            Access::Restricted(grants) => {
                let grants = grants.clone();
                grants.iter().any(|g| self.grant_matches(g, resource))
            }
        }
    }

    /// Whether `resource` may be returned to this token.
    fn may_return(&mut self, resource: &Value) -> bool {
        let Some(ty) = resource.get("resourceType").and_then(Value::as_str) else {
            return false;
        };
        if ty == "OperationOutcome" {
            return true;
        }
        let access = self.read_access(ty);
        self.resource_allowed(resource, &access)
    }

    /// Drops every entry this token may not see. Recurses into Bundles
    /// carried as entry resources (searches inside a batch response). With
    /// `drop_bare`, an entry without a resource (a history delete) is dropped
    /// too, since there is nothing to show it belongs to the compartment.
    fn filter_bundle(&mut self, bundle: &mut Value, drop_bare: bool) -> usize {
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
                Some(resource) => {
                    let resource = resource.clone();
                    self.may_return(&resource)
                }
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

/// The search parameters that express the grants' constraints, when they can
/// be expressed as one AND-ed query; `Err` when they cannot.
fn constraint_params(grants: &[Grant]) -> Result<Vec<(String, String)>, String> {
    if grants.iter().any(|g| g.constraints.is_empty()) {
        return Ok(Vec::new());
    }
    if grants.len() == 1 {
        return Ok(grants[0].constraints.clone());
    }
    let single: Vec<&(String, String)> = grants
        .iter()
        .filter_map(|g| (g.constraints.len() == 1).then(|| &g.constraints[0]))
        .collect();
    if single.len() == grants.len() && single.iter().all(|(p, _)| *p == single[0].0) {
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

/// The SMART access middleware. See the module documentation.
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
    let restricted = principal_is_restricted(&principal);
    if !restricted && principal.scopes.reads_everything() {
        return next.run(request).await;
    }

    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();
    let route = classify(&method, &path);

    // A token without patient or granular scopes only needs other types kept
    // out of multi-type results.
    if !restricted {
        let multi_type = match &route {
            Route::Compartment { target, .. } if target == "*" => true,
            Route::Search { .. } | Route::Compartment { .. } => asks_for_includes(&query),
            Route::Everything { .. } | Route::Batch => true,
            _ => false,
        };
        if !multi_type {
            return next.run(request).await;
        }
    }

    let (mut parts, body) = request.into_parts();
    let tenant = match TenantExtractor::from_request_parts(&mut parts, &state).await {
        Ok(tenant) => tenant,
        // Let the handler produce the tenant error it always produces.
        Err(_) => return next.run(Request::from_parts(parts, body)).await,
    };
    let registry = state.storage().search_param_registry(tenant.context());
    let mut ctx = Ctx {
        patient: principal.launch_patient().map(str::to_string),
        principal: principal.clone(),
        registry,
        version: state.config().default_fhir_version,
        non_compartment: state.auth_config().patient_non_compartment,
        matchers: HashMap::new(),
    };
    if let Some(patient) = &ctx.patient
        && !is_fhir_id(patient)
    {
        return forbidden("the token's patient launch context is not a valid FHIR id");
    }
    let max_body = state.config().max_body_size;
    let request = Request::from_parts(parts, body);

    if !restricted {
        let response = next.run(request).await;
        return filter_response(response, &mut ctx, false, max_body).await;
    }

    // From here on the token carries patient or granular scopes.
    let needs_patient = |access: &Access| access.needs_patient();
    let no_patient = "this request needs a patient launch context: the token has patient/ scopes \
                      but no `patient` claim";

    match route.clone() {
        Route::Other | Route::Validate => next.run(request).await,
        Route::Batch => {
            if principal.scopes.writes_and_reads_everything() {
                return next.run(request).await;
            }
            forbidden(
                "batch and transaction requests are not available to tokens with patient/ or \
                 granular scopes; send the requests individually",
            )
        }
        Route::TypeHistory | Route::SystemHistory => forbidden(
            "type- and system-level history are not available to tokens with patient/ or \
             granular scopes",
        ),
        Route::ConditionalWrite { ty } | Route::Patch { ty } | Route::Operation { ty } => {
            let perm = match method {
                Method::GET | Method::HEAD => SmartPermissions::READ,
                Method::DELETE => SmartPermissions::DELETE,
                Method::POST => SmartPermissions::CREATE,
                _ => SmartPermissions::UPDATE,
            };
            if ctx.access(&ty, perm) == Access::Full {
                return next.run(request).await;
            }
            forbidden(
                "conditional writes, PATCH and operations are not available to a token whose \
                 access to this type is limited to a patient compartment or granular scope",
            )
        }
        Route::Search { ty, post } => {
            let access = ctx.access(&ty, SmartPermissions::SEARCH);
            let Access::Restricted(grants) = &access else {
                // Full: only included types need filtering. Denied: authz
                // refused already; nothing to do.
                let response = next.run(request).await;
                return filter_response(response, &mut ctx, false, max_body).await;
            };
            if needs_patient(&access) && ctx.patient.is_none() {
                return forbidden(no_patient);
            }
            let compartment = grants.iter().all(|g| g.patient_compartment);
            if !compartment && grants.iter().any(|g| g.patient_compartment) {
                return forbidden(
                    "the token's patient/ and user/system granular scopes for this type cannot be \
                     combined into one search",
                );
            }
            let extra = match constraint_params(grants) {
                Ok(extra) => extra,
                Err(message) => return forbidden(&message),
            };
            let (mut parts, body) = request.into_parts();
            let mut query = query;
            if post {
                // Fold the form body into the query and search with GET, so
                // the request can be routed as a compartment search.
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
            let mut extra = extra;
            let new_path = if compartment && ty == "Patient" {
                extra.push((
                    "_id".to_string(),
                    ctx.patient.clone().unwrap_or_default(),
                ));
                format!("/{ty}")
            } else if compartment {
                format!("/Patient/{}/{ty}", ctx.patient.clone().unwrap_or_default())
            } else {
                format!("/{ty}")
            };
            let new_query = append_params(&query, &extra);
            match with_query(&new_path, &new_query) {
                Some(uri) => parts.uri = uri,
                None => return forbidden("could not restrict the search to the token's scopes"),
            }
            debug!(path = %new_path, query = %new_query, "SMART access: search restricted");
            let response = next.run(Request::from_parts(parts, Body::empty())).await;
            filter_response(response, &mut ctx, false, max_body).await
        }
        Route::Compartment { ctype, cid, target } => {
            if ctype != "Patient" || (target != "*" && !ctx.in_patient_compartment_type(&target)) {
                // Not a Patient compartment: only allowed if the target type
                // is fully readable; results are filtered either way.
                if target != "*" && ctx.access(&target, SmartPermissions::SEARCH) == Access::Full {
                    let response = next.run(request).await;
                    return filter_response(response, &mut ctx, false, max_body).await;
                }
                return forbidden(
                    "only the launch patient's compartment can be searched with patient/ scopes",
                );
            }
            let access = if target == "*" {
                ctx.access("Patient", SmartPermissions::READ)
            } else {
                ctx.access(&target, SmartPermissions::SEARCH)
            };
            if access.needs_patient() {
                if ctx.patient.is_none() {
                    return forbidden(no_patient);
                }
                if ctx.patient.as_deref() != Some(cid.as_str()) {
                    return forbidden("the requested compartment is not the launch patient's");
                }
            }
            let request = if let (Access::Restricted(grants), false) = (&access, target == "*") {
                let extra = match constraint_params(grants) {
                    Ok(extra) => extra,
                    Err(message) => return forbidden(&message),
                };
                let (mut parts, body) = request.into_parts();
                if let Some(uri) = with_query(&path, &append_params(&query, &extra)) {
                    parts.uri = uri;
                }
                Request::from_parts(parts, body)
            } else {
                request
            };
            let response = next.run(request).await;
            filter_response(response, &mut ctx, true, max_body).await
        }
        Route::Everything { id } => {
            let access = ctx.access("Patient", SmartPermissions::READ);
            if access.needs_patient() {
                let Some(patient) = ctx.patient.clone() else {
                    return forbidden(no_patient);
                };
                if id.as_deref() != Some(patient.as_str()) {
                    return forbidden(
                        "Patient/$everything is limited to the launch patient for patient/ scopes",
                    );
                }
            }
            let response = next.run(request).await;
            filter_response(response, &mut ctx, true, max_body).await
        }
        Route::Read { ty, id } => {
            let access = ctx.read_access(&ty);
            if access.needs_patient() {
                let Some(patient) = ctx.patient.clone() else {
                    return forbidden(no_patient);
                };
                if ty == "Patient"
                    && id != patient
                    && matches!(&access, Access::Restricted(g) if g.iter().all(|g| g.patient_compartment))
                {
                    return forbidden("the requested Patient is not the launch patient");
                }
            }
            if access == Access::Full {
                return next.run(request).await;
            }
            let response = next.run(request).await;
            check_single_response(response, &mut ctx, &access, max_body).await
        }
        Route::Create { ty } => {
            let access = ctx.access(&ty, SmartPermissions::CREATE);
            if !matches!(access, Access::Restricted(_)) {
                return next.run(request).await;
            }
            if access.needs_patient() && ctx.patient.is_none() {
                return forbidden(no_patient);
            }
            if ty == "Patient" && access.needs_patient() {
                return forbidden("a patient/ scope cannot create another Patient");
            }
            let (parts, body) = request.into_parts();
            let bytes = match axum::body::to_bytes(body, max_body).await {
                Ok(b) => b,
                Err(_) => return forbidden("request body too large"),
            };
            if !body_allowed(&bytes, &ty, &mut ctx, &access) {
                return forbidden(
                    "the resource is outside the token's reach (patient compartment or granular \
                     scope)",
                );
            }
            next.run(Request::from_parts(parts, Body::from(bytes))).await
        }
        Route::Update { ty, id } => {
            let access = ctx.access(&ty, SmartPermissions::UPDATE);
            if !matches!(access, Access::Restricted(_)) {
                return next.run(request).await;
            }
            if access.needs_patient() && ctx.patient.is_none() {
                return forbidden(no_patient);
            }
            let (mut parts, body) = request.into_parts();
            let bytes = match axum::body::to_bytes(body, max_body).await {
                Ok(b) => b,
                Err(_) => return forbidden("request body too large"),
            };
            if !body_allowed(&bytes, &ty, &mut ctx, &access) {
                return forbidden(
                    "the updated resource would be outside the token's reach (patient compartment \
                     or granular scope)",
                );
            }
            match stored(&state, &mut parts, &ty, &id).await {
                Ok(Some(existing)) if !ctx.resource_allowed(&existing, &access) => {
                    return forbidden(
                        "the stored resource is outside the token's reach (patient compartment or \
                         granular scope)",
                    );
                }
                Ok(_) => {}
                Err(response) => return response,
            }
            next.run(Request::from_parts(parts, Body::from(bytes))).await
        }
        Route::Delete { ty, id } => {
            let access = ctx.access(&ty, SmartPermissions::DELETE);
            if !matches!(access, Access::Restricted(_)) {
                return next.run(request).await;
            }
            if access.needs_patient() && ctx.patient.is_none() {
                return forbidden(no_patient);
            }
            let (mut parts, body) = request.into_parts();
            match stored(&state, &mut parts, &ty, &id).await {
                Ok(Some(existing)) if !ctx.resource_allowed(&existing, &access) => {
                    return forbidden(
                        "the stored resource is outside the token's reach (patient compartment or \
                         granular scope)",
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
        Err((status, message)) => return Err((status, message).into_response()),
    };
    match state.storage().read(tenant.context(), ty, id).await {
        Ok(found) => Ok(found.map(|r| r.content().clone())),
        Err(err) => {
            warn!(error = %err, "SMART access: could not read the stored resource");
            Ok(None)
        }
    }
}

fn body_allowed(bytes: &Bytes, ty: &str, ctx: &mut Ctx, access: &Access) -> bool {
    let Ok(resource) = serde_json::from_slice::<Value>(bytes) else {
        // XML or malformed: cannot be shown to be inside the compartment.
        return false;
    };
    if resource.get("resourceType").and_then(Value::as_str) != Some(ty) {
        return false;
    }
    ctx.resource_allowed(&resource, access)
}

/// Checks a single-resource (or instance-history) response against `access`.
async fn check_single_response(
    response: Response,
    ctx: &mut Ctx,
    access: &Access,
    max_body: usize,
) -> Response {
    if !response.status().is_success() || response.status() == StatusCode::NOT_MODIFIED {
        return response;
    }
    if !is_json(&response) {
        return forbidden(
            "restricted (patient/ or granular) tokens can only receive JSON; request \
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
    if ctx.resource_allowed(&value, access) {
        Response::from_parts(parts, Body::from(bytes))
    } else {
        forbidden("the resource is outside the token's reach (patient compartment or granular scope)")
    }
}

/// Filters a Bundle response; other responses pass unchanged.
async fn filter_response(
    response: Response,
    ctx: &mut Ctx,
    drop_bare: bool,
    max_body: usize,
) -> Response {
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
        assert_eq!(classify(&Method::GET, "/Observation/_history"), Route::TypeHistory);
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
        let g = |c: &[(&str, &str)]| Grant {
            patient_compartment: true,
            constraints: c
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        assert_eq!(
            constraint_params(&[g(&[("category", "a")]), g(&[("category", "b")])]).unwrap(),
            vec![("category".to_string(), "a,b".to_string())]
        );
        assert!(constraint_params(&[g(&[("category", "a")]), g(&[("code", "b")])]).is_err());
        assert!(constraint_params(&[g(&[("category", "a")]), g(&[])]).unwrap().is_empty());
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
    fn ids_are_validated() {
        assert!(is_fhir_id("pat-1.a"));
        assert!(!is_fhir_id("pat/1"));
        assert!(!is_fhir_id(""));
        assert!(!is_fhir_id("a?b"));
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
