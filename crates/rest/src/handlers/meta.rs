//! `$meta`, `$meta-add` and `$meta-delete` operation handlers.
//!
//! Implements the FHIR [Resource-meta](https://hl7.org/fhir/R4/resource-operation-meta.html),
//! [Resource-meta-add](https://hl7.org/fhir/R4/resource-operation-meta-add.html) and
//! [Resource-meta-delete](https://hl7.org/fhir/R4/resource-operation-meta-delete.html)
//! operations:
//!
//! - `GET|POST [base]/$meta` — every profile, tag and security label in use
//!   on the server (system level)
//! - `GET|POST [base]/[type]/$meta` — the same, for one resource type
//! - `GET|POST [base]/[type]/[id]/$meta` — the resource's `meta`
//! - `POST [base]/[type]/[id]/$meta-add` — add the profiles, tags and
//!   security labels of the `meta` input parameter to the resource
//! - `POST [base]/[type]/[id]/$meta-delete` — remove them
//!
//! Each answers a `Parameters` whose single `return` parameter is a `Meta`.
//!
//! # Versioning
//!
//! `$meta-add` and `$meta-delete` change the resource's `meta` only — its
//! other content is untouched — but they are writes: like HAPI, the server
//! stores a **new version** of the resource (so `meta.versionId` advances, the
//! change is in `_history`, and `_tag` / `_profile` / `_security` searches
//! see it at once). An input that would change nothing (every tag already
//! present, or none present to delete) stores nothing and answers the current
//! `meta`.
//!
//! The type- and system-level `$meta` are read from the search index
//! ([`SearchProvider::distinct_meta_values`]), so they list what a `_tag=` /
//! `_profile=` / `_security=` search would match: live resources only.
//! Backends without a queryable index answer `501`.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use helios_persistence::core::{DistinctMetaValue, ResourceStorage, SearchProvider};
use helios_persistence::types::StoredResource;
use serde_json::{Map, Value, json};
use tracing::debug;

use crate::error::{RestError, RestResult};
use crate::extractors::{FhirResource, FhirVersionExtractor, TenantExtractor};
use crate::middleware::content_type::negotiate_format;
use crate::responses::ResourceHeaders;
use crate::responses::format_resource_response;
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
    respond(StatusCode::OK, HeaderMap::new(), meta, &req_headers)
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
    respond(StatusCode::OK, HeaderMap::new(), meta, &req_headers)
}

/// `GET|POST [base]/[type]/[id]/$meta` — the resource's `meta`.
pub async fn meta_instance_handler<S>(
    State(state): State<AppState<S>>,
    Path((resource_type, id)): Path<(String, String)>,
    tenant: TenantExtractor,
    req_headers: HeaderMap,
) -> RestResult<Response>
where
    S: ResourceStorage + Send + Sync,
{
    let stored = read_current(&state, &tenant, &resource_type, &id).await?;
    let headers = ResourceHeaders::from_stored(&stored, &state).to_header_map();
    respond(StatusCode::OK, headers, meta_of(&stored), &req_headers)
}

/// `POST [base]/[type]/[id]/$meta-add`.
pub async fn meta_add_handler<S>(
    State(state): State<AppState<S>>,
    Path((resource_type, id)): Path<(String, String)>,
    tenant: TenantExtractor,
    version: FhirVersionExtractor,
    req_headers: HeaderMap,
    FhirResource(body): FhirResource,
) -> RestResult<Response>
where
    S: ResourceStorage + Send + Sync,
{
    let input = meta_input(&body, "$meta-add")?;
    let fhir_version = version.storage_version_or(state.config().default_fhir_version);
    let stored = apply_meta_change(
        &state,
        &tenant,
        fhir_version,
        &resource_type,
        &id,
        &input,
        MetaChange::Add,
    )
    .await?;
    let headers = ResourceHeaders::from_stored(&stored, &state).to_header_map();
    respond(StatusCode::OK, headers, meta_of(&stored), &req_headers)
}

/// `POST [base]/[type]/[id]/$meta-delete`.
pub async fn meta_delete_handler<S>(
    State(state): State<AppState<S>>,
    Path((resource_type, id)): Path<(String, String)>,
    tenant: TenantExtractor,
    version: FhirVersionExtractor,
    req_headers: HeaderMap,
    FhirResource(body): FhirResource,
) -> RestResult<Response>
where
    S: ResourceStorage + Send + Sync,
{
    let input = meta_input(&body, "$meta-delete")?;
    let fhir_version = version.storage_version_or(state.config().default_fhir_version);
    let stored = apply_meta_change(
        &state,
        &tenant,
        fhir_version,
        &resource_type,
        &id,
        &input,
        MetaChange::Delete,
    )
    .await?;
    let headers = ResourceHeaders::from_stored(&stored, &state).to_header_map();
    respond(StatusCode::OK, headers, meta_of(&stored), &req_headers)
}

/// Which way a `$meta-add` / `$meta-delete` moves the resource's `meta`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetaChange {
    Add,
    Delete,
}

/// Reads the current version of the resource; a missing one is a `404`, a
/// deleted one the `410` the storage error maps to.
async fn read_current<S>(
    state: &AppState<S>,
    tenant: &TenantExtractor,
    resource_type: &str,
    id: &str,
) -> RestResult<StoredResource>
where
    S: ResourceStorage + Send + Sync,
{
    state
        .storage()
        .read(tenant.context(), resource_type, id)
        .await?
        .ok_or_else(|| RestError::NotFound {
            resource_type: resource_type.to_string(),
            id: id.to_string(),
        })
}

/// Applies the input `meta` to the stored resource and stores the result as a
/// new version, unless nothing changes.
async fn apply_meta_change<S>(
    state: &AppState<S>,
    tenant: &TenantExtractor,
    fhir_version: helios_fhir::FhirVersion,
    resource_type: &str,
    id: &str,
    input: &Value,
    change: MetaChange,
) -> RestResult<StoredResource>
where
    S: ResourceStorage + Send + Sync,
{
    let current = read_current(state, tenant, resource_type, id).await?;
    let mut resource = current.content().clone();
    let changed = {
        let meta = resource
            .as_object_mut()
            .ok_or_else(|| RestError::InternalError {
                message: "stored resource is not a JSON object".to_string(),
            })?
            .entry("meta")
            .or_insert_with(|| Value::Object(Map::new()));
        if !meta.is_object() {
            *meta = Value::Object(Map::new());
        }
        let meta = meta.as_object_mut().expect("meta is an object");
        let mut changed = false;
        changed |= merge_list(meta, input, "profile", change, |a, b| a == b);
        changed |= merge_list(meta, input, "security", change, same_coding);
        changed |= merge_list(meta, input, "tag", change, same_coding);
        changed
    };
    if !changed {
        debug!(
            resource_type = %resource_type,
            id = %id,
            ?change,
            "$meta change is a no-op; keeping the current version"
        );
        return Ok(current);
    }

    let stored = state
        .storage()
        .update(tenant.context(), &current, resource)
        .await?;
    super::write_event::report(
        state,
        tenant.context(),
        fhir_version,
        resource_type,
        0,
        Some(super::write_event::stored_notice(
            super::write_event::upsert_kind(false),
            &stored,
        )),
    );
    debug!(
        resource_type = %resource_type,
        id = %id,
        version = %stored.version_id(),
        ?change,
        "$meta change stored as a new version"
    );
    Ok(stored)
}

/// Adds to (or removes from) `meta[key]` the entries of `input[key]`, matching
/// entries with `same`. Returns whether anything changed. An emptied list is
/// removed from `meta`, as an empty array is not valid FHIR JSON.
fn merge_list(
    meta: &mut Map<String, Value>,
    input: &Value,
    key: &str,
    change: MetaChange,
    same: impl Fn(&Value, &Value) -> bool,
) -> bool {
    let Some(incoming) = input.get(key).and_then(Value::as_array) else {
        return false;
    };
    let mut existing: Vec<Value> = meta
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let before = existing.len();
    match change {
        MetaChange::Add => {
            for entry in incoming {
                if !existing.iter().any(|e| same(e, entry)) {
                    existing.push(entry.clone());
                }
            }
        }
        MetaChange::Delete => {
            existing.retain(|e| !incoming.iter().any(|entry| same(e, entry)));
        }
    }
    if existing.len() == before {
        return false;
    }
    if existing.is_empty() {
        meta.remove(key);
    } else {
        meta.insert(key.to_string(), Value::Array(existing));
    }
    true
}

/// Two Codings name the same concept when their `system` and `code` agree
/// (`display` and `version` do not take part, as HAPI matches them).
fn same_coding(a: &Value, b: &Value) -> bool {
    a.get("system") == b.get("system") && a.get("code") == b.get("code")
}

/// The `meta` input parameter of a `$meta-add` / `$meta-delete` body: a
/// `Parameters` with one `meta` parameter whose `valueMeta` is a Meta. A bare
/// Meta object (no `resourceType`) is accepted too, for convenience.
fn meta_input(body: &Value, operation: &str) -> RestResult<Value> {
    let invalid = |message: String| RestError::BadRequest { message };
    match body.get("resourceType").and_then(Value::as_str) {
        Some("Parameters") => {
            let metas: Vec<&Value> = body
                .get("parameter")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .filter(|p| p.get("name").and_then(Value::as_str) == Some("meta"))
                .collect();
            let [param] = metas.as_slice() else {
                return Err(invalid(format!(
                    "{operation} requires exactly one 'meta' parameter (found {})",
                    metas.len()
                )));
            };
            let meta = param.get("valueMeta").ok_or_else(|| {
                invalid(format!(
                    "{operation}: the 'meta' parameter must carry a valueMeta"
                ))
            })?;
            validate_meta_input(meta, operation)?;
            Ok(meta.clone())
        }
        Some(other) => Err(invalid(format!(
            "{operation} expects a Parameters resource, not {other}"
        ))),
        None if body.is_object() => {
            validate_meta_input(body, operation)?;
            Ok(body.clone())
        }
        None => Err(invalid(format!(
            "{operation} expects a Parameters resource"
        ))),
    }
}

/// The Meta must be an object whose `profile`, `security` and `tag`, when
/// present, are arrays of the right shape, and must name at least one of them.
fn validate_meta_input(meta: &Value, operation: &str) -> RestResult<()> {
    let invalid = |message: String| RestError::BadRequest { message };
    if !meta.is_object() {
        return Err(invalid(format!(
            "{operation}: valueMeta must be a Meta object"
        )));
    }
    let mut named = 0;
    for key in ["profile", "security", "tag"] {
        let Some(list) = meta.get(key) else {
            continue;
        };
        let Some(list) = list.as_array() else {
            return Err(invalid(format!("{operation}: meta.{key} must be an array")));
        };
        named += 1;
        for entry in list {
            let ok = if key == "profile" {
                entry.as_str().is_some_and(|s| !s.is_empty())
            } else {
                entry.is_object() && entry.get("code").and_then(Value::as_str).is_some()
            };
            if !ok {
                return Err(invalid(format!(
                    "{operation}: every meta.{key} entry must be {}",
                    if key == "profile" {
                        "a canonical URL string"
                    } else {
                        "a Coding with a code"
                    }
                )));
            }
        }
    }
    if named == 0 {
        return Err(invalid(format!(
            "{operation}: valueMeta must carry at least one of profile, security or tag"
        )));
    }
    Ok(())
}

/// The stored resource's `meta`, `versionId` and `lastUpdated` included.
fn meta_of(stored: &StoredResource) -> Value {
    stored
        .content_with_meta()
        .get("meta")
        .cloned()
        .unwrap_or_else(|| json!({}))
}

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

/// The operations' response: a `Parameters` with one `return` Meta.
fn respond(
    status: StatusCode,
    headers: HeaderMap,
    meta: Value,
    req_headers: &HeaderMap,
) -> RestResult<Response> {
    let negotiated = negotiate_format(req_headers, None);
    let parameters = json!({
        "resourceType": "Parameters",
        "parameter": [{ "name": "return", "valueMeta": meta }]
    });
    format_resource_response(status, headers, &parameters, negotiated.format)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(system: &str, code: &str) -> Value {
        json!({"system": system, "code": code})
    }

    #[test]
    fn meta_input_unwraps_parameters_and_rejects_bad_shapes() {
        let meta = json!({"tag": [tag("http://t", "a")]});
        let body = json!({"resourceType": "Parameters",
            "parameter": [{"name": "meta", "valueMeta": meta}]});
        assert_eq!(meta_input(&body, "$meta-add").unwrap(), meta);

        // A bare Meta is accepted.
        assert_eq!(meta_input(&meta, "$meta-add").unwrap(), meta);

        for (body, needle) in [
            (json!({"resourceType": "Parameters"}), "exactly one 'meta'"),
            (
                json!({"resourceType": "Parameters", "parameter": [
                    {"name": "meta", "valueMeta": meta}, {"name": "meta", "valueMeta": meta}]}),
                "exactly one 'meta'",
            ),
            (
                json!({"resourceType": "Parameters", "parameter": [{"name": "meta"}]}),
                "must carry a valueMeta",
            ),
            (
                json!({"resourceType": "Parameters", "parameter": [
                    {"name": "meta", "valueMeta": {"tag": "x"}}]}),
                "meta.tag must be an array",
            ),
            (
                json!({"resourceType": "Parameters", "parameter": [
                    {"name": "meta", "valueMeta": {"tag": [{"system": "http://t"}]}}]}),
                "Coding with a code",
            ),
            (
                json!({"resourceType": "Parameters", "parameter": [
                    {"name": "meta", "valueMeta": {"profile": [""]}}]}),
                "canonical URL string",
            ),
            (
                json!({"resourceType": "Parameters", "parameter": [
                    {"name": "meta", "valueMeta": {"versionId": "3"}}]}),
                "at least one of profile, security or tag",
            ),
            (json!({"resourceType": "Patient"}), "not Patient"),
            (json!([]), "expects a Parameters"),
        ] {
            let err = meta_input(&body, "$meta-add").unwrap_err();
            assert!(
                matches!(&err, RestError::BadRequest { message } if message.contains(needle)),
                "{body}: {err:?}"
            );
        }
    }

    #[test]
    fn merge_list_adds_without_duplicates_and_deletes_by_system_and_code() {
        let mut meta = Map::new();
        meta.insert(
            "tag".to_string(),
            json!([tag("http://t", "a"), tag("http://t", "b")]),
        );

        // Add: `a` already there (display ignored), `c` new.
        let input = json!({"tag": [
            {"system": "http://t", "code": "a", "display": "A"}, tag("http://t", "c")]});
        assert!(merge_list(
            &mut meta,
            &input,
            "tag",
            MetaChange::Add,
            same_coding
        ));
        assert_eq!(
            meta["tag"],
            json!([
                tag("http://t", "a"),
                tag("http://t", "b"),
                tag("http://t", "c")
            ])
        );
        // Adding again changes nothing.
        assert!(!merge_list(
            &mut meta,
            &input,
            "tag",
            MetaChange::Add,
            same_coding
        ));
        // Absent key in the input: nothing to do.
        assert!(!merge_list(
            &mut meta,
            &input,
            "security",
            MetaChange::Add,
            same_coding
        ));

        // Delete: `b` and an unknown one.
        let input = json!({"tag": [tag("http://t", "b"), tag("http://t", "zzz")]});
        assert!(merge_list(
            &mut meta,
            &input,
            "tag",
            MetaChange::Delete,
            same_coding
        ));
        assert_eq!(
            meta["tag"],
            json!([tag("http://t", "a"), tag("http://t", "c")])
        );
        // Same system, different code is not a match; same code, other system neither.
        let input = json!({"tag": [tag("http://other", "a")]});
        assert!(!merge_list(
            &mut meta,
            &input,
            "tag",
            MetaChange::Delete,
            same_coding
        ));

        // Deleting the last entries removes the key altogether.
        let input = json!({"tag": [tag("http://t", "a"), tag("http://t", "c")]});
        assert!(merge_list(
            &mut meta,
            &input,
            "tag",
            MetaChange::Delete,
            same_coding
        ));
        assert!(!meta.contains_key("tag"));

        // Profiles compare as strings.
        let input = json!({"profile": ["http://p/1", "http://p/1"]});
        assert!(merge_list(
            &mut meta,
            &input,
            "profile",
            MetaChange::Add,
            |a, b| a == b
        ));
        assert_eq!(meta["profile"], json!(["http://p/1"]));
    }

    #[test]
    fn distinct_codings_collapses_displays() {
        let v = |code: &str, display: Option<&str>| DistinctMetaValue {
            system: Some("http://t".into()),
            code: Some(code.into()),
            display: display.map(str::to_string),
            uri: None,
        };
        let out = distinct_codings(&[
            v("a", None),
            v("a", Some("A")),
            v("b", Some("B")),
            v("a", Some("A2")),
        ]);
        assert_eq!(out, vec![v("a", Some("A")), v("b", Some("B"))]);
    }

    #[test]
    fn coding_of_keeps_system_and_display_when_indexed() {
        assert_eq!(
            coding_of(&DistinctMetaValue {
                system: Some("http://t".into()),
                code: Some("a".into()),
                display: Some("A".into()),
                uri: None,
            }),
            Some(json!({"system": "http://t", "code": "a", "display": "A"}))
        );
        assert_eq!(
            coding_of(&DistinctMetaValue {
                code: Some("a".into()),
                ..Default::default()
            }),
            Some(json!({"code": "a"}))
        );
        assert_eq!(coding_of(&DistinctMetaValue::default()), None);
    }
}
