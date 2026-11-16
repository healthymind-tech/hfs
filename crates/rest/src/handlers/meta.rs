//! `$meta`, `$meta-add` and `$meta-delete` (FHIR `Resource-meta`,
//! `Resource-meta-add`, `Resource-meta-delete`).
//!
//! - `GET|POST [base]/[type]/[id]/$meta` — the resource's current `meta`.
//! - `GET|POST [base]/[type]/[id]/_history/[vid]/$meta` — the `meta` of one
//!   version.
//! - `POST [base]/[type]/[id]/$meta-add` — add profiles, tags and security
//!   labels.
//! - `POST [base]/[type]/[id]/$meta-delete` — remove them.
//! - `GET|POST [base]/$meta`, `[base]/[type]/$meta` — every profile, tag and
//!   security label in use on live resources of the system / type, read from
//!   the search index (`SearchProvider::distinct_meta_values`; fork, kept at
//!   the v0.2.9 merge — upstream answers these with `400 not-supported`).
//!
//! Every form answers a `Parameters` whose `return` is a `Meta`.
//!
//! An update merges tags and security labels instead of replacing them
//! (#2562), so `$meta-delete` is the only way a client can remove one.
//!
//! # Versions
//!
//! The specification does not say whether `$meta-add` and `$meta-delete`
//! create a new version. Here a change to `meta` writes a new version, like
//! any other update: history, the ETag, subscriptions and the audit trail all
//! see it. An operation that changes nothing writes nothing and returns the
//! current `meta`.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use helios_persistence::core::{DistinctMetaValue, ResourceStorage, SearchProvider, VersionedStorage};
use helios_persistence::types::{MetaOperation, StoredResource, apply_meta_operation};
use serde_json::{Map, Value, json};
use tracing::debug;

use crate::error::{RestError, RestResult};
use crate::extractors::{FhirResource, TenantExtractor};
use crate::handlers::extract_patient_from_resource;
use crate::middleware::conditional::ConditionalHeaders;
use crate::middleware::content_type::negotiate_format;
use crate::patient_compartment::PatientCompartment;
use crate::responses::format_resource_response;
use crate::responses::headers::ResourceHeaders;
use crate::state::AppState;

/// `GET|POST [base]/$meta` — every profile, tag and security label in use.
pub async fn meta_system_handler<S>(
    State(state): State<AppState<S>>,
    tenant: TenantExtractor,
    req_headers: HeaderMap,
) -> RestResult<Response>
where
    S: ResourceStorage + SearchProvider + Send + Sync,
{
    let meta = collect_meta(&state, &tenant, None).await?;
    respond_summary(meta, &req_headers)
}

/// `GET|POST [base]/[type]/$meta` — every profile, tag and security label in
/// use on resources of the type.
pub async fn meta_type_handler<S>(
    State(state): State<AppState<S>>,
    Path(resource_type): Path<String>,
    tenant: TenantExtractor,
    req_headers: HeaderMap,
) -> RestResult<Response>
where
    S: ResourceStorage + SearchProvider + Send + Sync,
{
    let meta = collect_meta(&state, &tenant, Some(&resource_type)).await?;
    respond_summary(meta, &req_headers)
}

/// `GET|POST [base]/[type]/[id]/$meta`
pub async fn meta_instance_handler<S>(
    State(state): State<AppState<S>>,
    Path((resource_type, id)): Path<(String, String)>,
    tenant: TenantExtractor,
    compartment: PatientCompartment,
    headers: HeaderMap,
) -> RestResult<Response>
where
    S: ResourceStorage + SearchProvider + Send + Sync,
{
    let stored = read_current(&state, &tenant, &compartment, &resource_type, &id).await?;
    respond(&state, &stored, &headers)
}

/// `GET|POST [base]/[type]/[id]/_history/[vid]/$meta`
pub async fn meta_version_handler<S>(
    State(state): State<AppState<S>>,
    Path((resource_type, id, version_id)): Path<(String, String, String)>,
    tenant: TenantExtractor,
    compartment: PatientCompartment,
    headers: HeaderMap,
) -> RestResult<Response>
where
    S: ResourceStorage + VersionedStorage + SearchProvider + Send + Sync,
{
    let stored = super::vread::execute_vread(
        &state,
        &tenant,
        &compartment,
        &resource_type,
        &id,
        &version_id,
    )
    .await?;
    respond(&state, &stored, &headers)
}

/// `GET|POST [base]/$meta` and `[base]/[type]/$meta`: not supported.
pub async fn meta_summary_not_supported_handler() -> RestResult<Response> {
    Err(RestError::NotSupported {
        feature: "$meta at the system or type level (a summary of the labels in use); \
                  invoke it on a resource instance: [type]/[id]/$meta"
            .to_string(),
    })
}

/// `POST [base]/[type]/[id]/$meta-add`
#[allow(clippy::too_many_arguments)]
pub async fn meta_add_handler<S>(
    State(state): State<AppState<S>>,
    Path((resource_type, id)): Path<(String, String)>,
    tenant: TenantExtractor,
    compartment: PatientCompartment,
    conditional: ConditionalHeaders,
    headers: HeaderMap,
    FhirResource(body): FhirResource,
) -> RestResult<Response>
where
    S: ResourceStorage + SearchProvider + Send + Sync,
{
    change_meta(
        &state,
        MetaChange {
            resource_type,
            id,
            operation: MetaOperation::Add,
        },
        &tenant,
        &compartment,
        &conditional,
        &headers,
        &body,
    )
    .await
}

/// `POST [base]/[type]/[id]/$meta-delete`
#[allow(clippy::too_many_arguments)]
pub async fn meta_delete_handler<S>(
    State(state): State<AppState<S>>,
    Path((resource_type, id)): Path<(String, String)>,
    tenant: TenantExtractor,
    compartment: PatientCompartment,
    conditional: ConditionalHeaders,
    headers: HeaderMap,
    FhirResource(body): FhirResource,
) -> RestResult<Response>
where
    S: ResourceStorage + SearchProvider + Send + Sync,
{
    change_meta(
        &state,
        MetaChange {
            resource_type,
            id,
            operation: MetaOperation::Delete,
        },
        &tenant,
        &compartment,
        &conditional,
        &headers,
        &body,
    )
    .await
}

/// The target and kind of a `$meta-add` / `$meta-delete`.
struct MetaChange {
    resource_type: String,
    id: String,
    operation: MetaOperation,
}

impl MetaChange {
    fn name(&self) -> &'static str {
        match self.operation {
            MetaOperation::Add => "$meta-add",
            MetaOperation::Delete => "$meta-delete",
        }
    }
}

async fn change_meta<S>(
    state: &AppState<S>,
    change: MetaChange,
    tenant: &TenantExtractor,
    compartment: &PatientCompartment,
    conditional: &ConditionalHeaders,
    headers: &HeaderMap,
    body: &Value,
) -> RestResult<Response>
where
    S: ResourceStorage + SearchProvider + Send + Sync,
{
    let MetaChange {
        resource_type, id, ..
    } = &change;
    let operation = change.name();

    // AuditEvent resources are immutable, as on update and patch.
    if resource_type == "AuditEvent" {
        return Err(RestError::MethodNotAllowed {
            method: operation.to_string(),
            resource_type: resource_type.clone(),
        });
    }

    let meta = meta_parameter(body, operation)?;
    let existing = read_current(state, tenant, compartment, resource_type, id).await?;

    // `If-Match`, as on patch: list-aware, malformed fails the precondition.
    let if_match = conditional
        .if_match_tags()
        .map_err(|e| RestError::PreconditionFailed {
            message: format!("Malformed If-Match header: {e}"),
        })?;
    if !if_match.if_match_satisfied(Some(existing.version_id())) {
        return Err(RestError::PreconditionFailed {
            message: format!(
                "If-Match precondition failed: no supplied entity-tag matches the current version W/\"{}\"",
                existing.version_id()
            ),
        });
    }

    let mut content = existing.content().clone();
    let changed =
        apply_meta_operation(&mut content, meta, change.operation).map_err(|message| {
            RestError::BadRequest {
                message: format!("{operation}: {message}"),
            }
        })?;
    if !changed {
        debug!(%resource_type, %id, operation, "meta unchanged; nothing written");
        return respond(state, &existing, headers);
    }

    // A profile added is a profile the resource now claims; check the write
    // as update and patch do.
    state
        .validation()
        .check_write(
            tenant.tenant_id(),
            existing.fhir_version(),
            resource_type,
            &content,
        )
        .await?;

    let stored = match change.operation {
        // Everything already present is still present, so the merge an update
        // applies is a no-op here.
        MetaOperation::Add => {
            state
                .storage()
                .update(tenant.context(), &existing, content)
                .await?
        }
        // A plain update would merge the deleted labels straight back (#2562).
        MetaOperation::Delete => {
            state
                .storage()
                .update_verbatim(tenant.context(), &existing, content)
                .await?
        }
    };

    if resource_type == "StructureDefinition" {
        state.validation().upsert_stored_profile(
            tenant.tenant_id(),
            stored.fhir_version(),
            stored.content(),
        );
    }

    debug!(
        %resource_type,
        %id,
        operation,
        version = %stored.version_id(),
        "meta changed"
    );

    super::write_event::report(
        state,
        tenant.context(),
        stored.fhir_version(),
        resource_type,
        0,
        Some(super::write_event::stored_notice(
            helios_persistence::core::WriteKind::Update,
            &stored,
        )),
    );

    let mut response = respond(state, &stored, headers)?;
    response
        .extensions_mut()
        .insert(helios_audit::AuditResponseContext {
            resource_type: Some(resource_type.clone()),
            resource_id: Some(stored.id().to_string()),
            patient_reference: extract_patient_from_resource(resource_type, stored.content()),
        });
    Ok(response)
}

/// Reads the live resource, confined to a `patient/` scope's compartment.
async fn read_current<S>(
    state: &AppState<S>,
    tenant: &TenantExtractor,
    compartment: &PatientCompartment,
    resource_type: &str,
    id: &str,
) -> RestResult<StoredResource>
where
    S: ResourceStorage + SearchProvider + Send + Sync,
{
    let stored = state
        .storage()
        .read(tenant.context(), resource_type, id)
        .await?
        .ok_or_else(|| RestError::NotFound {
            resource_type: resource_type.to_string(),
            id: id.to_string(),
        })?;
    compartment.assert_member(
        || state.storage().search_param_registry(tenant.context()),
        stored.fhir_version(),
        stored.content(),
        "meta",
    )?;
    Ok(stored)
}

/// The `meta` input parameter (1..1, `Meta`) of a `Parameters` body.
fn meta_parameter<'a>(body: &'a Value, operation: &str) -> RestResult<&'a Value> {
    if body.get("resourceType").and_then(Value::as_str) != Some("Parameters") {
        return Err(RestError::BadRequest {
            message: format!("{operation} takes a Parameters resource with a 'meta' parameter"),
        });
    }
    let mut metas = body
        .get("parameter")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|p| p.get("name").and_then(Value::as_str) == Some("meta"));
    let meta = metas.next().ok_or_else(|| RestError::BadRequest {
        message: format!("{operation} requires the 'meta' parameter"),
    })?;
    if metas.next().is_some() {
        return Err(RestError::BadRequest {
            message: format!("{operation} takes exactly one 'meta' parameter"),
        });
    }
    meta.get("valueMeta").ok_or_else(|| RestError::BadRequest {
        message: format!("{operation}: the 'meta' parameter must carry a valueMeta"),
    })
}

/// `Parameters { return: <stored.meta> }`, with the version's ETag and
/// Last-Modified.
fn respond<S>(
    state: &AppState<S>,
    stored: &StoredResource,
    headers: &HeaderMap,
) -> RestResult<Response>
where
    S: ResourceStorage,
{
    let negotiated = negotiate_format(headers, None);
    let meta = stored
        .content_with_meta()
        .get("meta")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let parameters = json!({
        "resourceType": "Parameters",
        "parameter": [{"name": "return", "valueMeta": meta}]
    });
    let header_map = ResourceHeaders::from_stored(stored, state)
        .with_content_type(negotiated.format.mime_type())
        .to_header_map();
    format_resource_response(StatusCode::OK, header_map, &parameters, negotiated.format)
}
// ── System / type summary (fork) ─────────────────────────────────────────────

/// The union of profiles, tags and security labels in use, as a Meta.
async fn collect_meta<S>(
    state: &AppState<S>,
    tenant: &TenantExtractor,
    resource_type: Option<&str>,
) -> RestResult<Value>
where
    S: ResourceStorage + SearchProvider + Send + Sync,
{
    let storage = state.storage();
    let profiles = storage
        .distinct_meta_values(tenant.context(), resource_type, "_profile")
        .await?;
    let security = storage
        .distinct_meta_values(tenant.context(), resource_type, "_security")
        .await?;
    let tags = storage
        .distinct_meta_values(tenant.context(), resource_type, "_tag")
        .await?;

    let mut meta = Map::new();
    let profile_values: Vec<Value> = profiles
        .iter()
        .filter_map(|v| v.uri.clone().or_else(|| v.code.clone()))
        .map(Value::String)
        .collect();
    if !profile_values.is_empty() {
        meta.insert("profile".to_string(), Value::Array(profile_values));
    }
    for (key, values) in [("security", &security), ("tag", &tags)] {
        let codings: Vec<Value> = distinct_codings(values)
            .iter()
            .filter_map(coding_of)
            .collect();
        if !codings.is_empty() {
            meta.insert(key.to_string(), Value::Array(codings));
        }
    }
    Ok(Value::Object(meta))
}

/// One entry per `(system, code)`: the index holds a row per distinct display
/// too, and a Coding is the same concept whatever its display. The first
/// display seen is kept.
fn distinct_codings(values: &[DistinctMetaValue]) -> Vec<DistinctMetaValue> {
    let mut seen: std::collections::BTreeMap<(Option<String>, Option<String>), usize> =
        std::collections::BTreeMap::new();
    let mut out: Vec<DistinctMetaValue> = Vec::new();
    for value in values {
        let key = (value.system.clone(), value.code.clone());
        match seen.get(&key) {
            Some(&i) => {
                if out[i].display.is_none() && value.display.is_some() {
                    out[i].display = value.display.clone();
                }
            }
            None => {
                seen.insert(key, out.len());
                out.push(value.clone());
            }
        }
    }
    out
}

/// A Coding for an indexed token value.
fn coding_of(value: &DistinctMetaValue) -> Option<Value> {
    let code = value.code.as_ref()?;
    let mut coding = Map::new();
    if let Some(system) = &value.system {
        coding.insert("system".to_string(), json!(system));
    }
    coding.insert("code".to_string(), json!(code));
    if let Some(display) = &value.display {
        coding.insert("display".to_string(), json!(display));
    }
    Some(Value::Object(coding))
}

/// The summary's response: a `Parameters` with one `return` Meta.
fn respond_summary(meta: Value, req_headers: &HeaderMap) -> RestResult<Response> {
    let negotiated = negotiate_format(req_headers, None);
    let parameters = json!({
        "resourceType": "Parameters",
        "parameter": [{ "name": "return", "valueMeta": meta }]
    });
    format_resource_response(StatusCode::OK, HeaderMap::new(), &parameters, negotiated.format)
}

