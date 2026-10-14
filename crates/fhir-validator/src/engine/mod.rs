//! The cooperative validation engine.
//!
//! Validation never flattens schemas: it builds a *set* of schemas (the
//! *schemata*) for every data node — root schema from `resourceType`,
//! profiles, plus every transitively referenced `base` and complex `type` —
//! and every member of the set judges the same node together. See
//! `docs/guide/mental-model.md` in the FHIR Schema repo for the model, and
//! `walk.rs` for the deterministic error-emission order.

mod errors;
mod path;
mod primitives;
mod references;
pub(crate) mod slicing;
mod walk;

pub use errors::{ErrorKind, Severity, ValidationError};
pub use path::dotted_to_fhirpath;

/// Crate-internal access to the effect-issue message templates (the full
/// template catalog lives in `errors.rs`, the single source of truth).
pub(crate) mod messages {
    pub(crate) use super::errors::{
        msg_constraint_not_evaluable as constraint_not_evaluable,
        msg_fhirpath_constraint as fhirpath_constraint,
        msg_terminology_binding as terminology_binding,
        msg_terminology_unavailable as terminology_unavailable,
    };
}

use crate::effects::{Deferred, EffectHandlers};
use crate::resolver::SchemaResolver;
use helios_fhir::FhirVersion;
use serde_json::Value;
use std::sync::Arc;

/// Policy for `meta.profile` claims the resolver cannot resolve. A profile
/// the caller asked for ([`ValidationOptions::profiles`]) that cannot be
/// resolved is always an error: the requested validation did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnknownProfilePolicy {
    /// Report as a warning-severity issue (default).
    #[default]
    Warn,
    /// Report as an error-severity issue.
    Error,
    /// Skip silently.
    Ignore,
}

/// Options for one validation run.
#[derive(Debug, Clone)]
pub struct ValidationOptions {
    /// Extra profile canonicals/names to layer on, in addition to the root
    /// schema and (optionally) `meta.profile`.
    pub profiles: Vec<String>,
    /// Layer on the profiles the resource claims in `meta.profile`.
    pub use_meta_profiles: bool,
    /// What to do when a profile reference cannot be resolved.
    pub unknown_profile: UnknownProfilePolicy,
    /// Warn when a known extension sits at a resource root outside its
    /// declared context (#615). Off by default: HL7's own publisher places
    /// `structuredefinition-*` metadata extensions on CodeSystem/ValueSet
    /// roots throughout the published spec, so a faithful check flags
    /// hundreds of canonical resources — useful to a profile author,
    /// noise to everyone else.
    pub check_extension_context: bool,
}

impl Default for ValidationOptions {
    fn default() -> Self {
        Self {
            profiles: Vec::new(),
            use_meta_profiles: true,
            check_extension_context: false,
            unknown_profile: UnknownProfilePolicy::default(),
        }
    }
}

/// Result of the pure synchronous walk.
#[derive(Debug, Default)]
pub struct SyncOutcome {
    /// Structural issues, in deterministic emission order.
    pub errors: Vec<ValidationError>,
    /// Constraint/binding obligations for the async effects pass (Phase 4).
    pub deferred: Vec<Deferred>,
}

/// `batch`/`transaction` entries whose `request.url` addresses a different
/// resource type than the entry's `resource`: `POST Patient` carrying an
/// Observation would be stored as an Observation the client asked to file
/// as a Patient. Only relative URLs are judged (`Type`, `Type/id`,
/// `Type?criteria`); an absolute URL's base cannot be told apart from its
/// type without knowing the server base. Methods without a resource body
/// (GET, DELETE, HEAD) are skipped.
fn bundle_entry_type_mismatches(resource: &Value) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    if resource.get("resourceType").and_then(Value::as_str) != Some("Bundle")
        || !matches!(
            resource.get("type").and_then(Value::as_str),
            Some("batch" | "transaction")
        )
    {
        return errors;
    }
    let Some(entries) = resource.get("entry").and_then(Value::as_array) else {
        return errors;
    };
    for (index, entry) in entries.iter().enumerate() {
        let Some(request) = entry.get("request") else {
            continue;
        };
        if !matches!(
            request.get("method").and_then(Value::as_str),
            Some("POST" | "PUT" | "PATCH")
        ) {
            continue;
        }
        let (Some(url), Some(actual)) = (
            request.get("url").and_then(Value::as_str),
            entry
                .get("resource")
                .and_then(|r| r.get("resourceType"))
                .and_then(Value::as_str),
        ) else {
            continue;
        };
        if url.contains("://") {
            continue;
        }
        let path = url.split(['?', '#']).next().unwrap_or_default();
        let Some(addressed) = path.trim_start_matches('/').split('/').next() else {
            continue;
        };
        // `Parameters` is a PATCH body (FHIRPath Patch), not the target.
        if addressed.is_empty()
            || addressed.starts_with('$')
            || addressed == actual
            || actual == "Parameters"
            || actual == "Binary"
        {
            continue;
        }
        errors.push(ValidationError::new(
            ErrorKind::BundleEntryMismatch,
            format!("Bundle.entry.{index}.request.url"),
            format!(
                "request.url '{url}' addresses resource type {addressed}, but the entry's resource is of type {actual}"
            ),
        ));
    }
    errors
}

/// `urn:uuid:` / `urn:oid:` references inside a Bundle's entries that no
/// entry's `fullUrl` provides. A URN names nothing outside the Bundle, so
/// such a reference cannot be resolved by anyone. In a `transaction` or
/// `batch` the server has to resolve it to process the entry, so it is an
/// error; in other Bundles a warning, as the HL7 validator reports it.
/// References inside a nested Bundle belong to that Bundle and are skipped.
fn bundle_unresolved_urns(resource: &Value) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    if resource.get("resourceType").and_then(Value::as_str) != Some("Bundle") {
        return errors;
    }
    let Some(entries) = resource.get("entry").and_then(Value::as_array) else {
        return errors;
    };
    let severity = match resource.get("type").and_then(Value::as_str) {
        Some("transaction" | "batch") => Severity::Error,
        _ => Severity::Warning,
    };
    let full_urls: std::collections::HashSet<&str> = entries
        .iter()
        .filter_map(|e| e.get("fullUrl").and_then(Value::as_str))
        .collect();

    fn walk(
        value: &Value,
        path: &mut String,
        full_urls: &std::collections::HashSet<&str>,
        severity: Severity,
        out: &mut Vec<ValidationError>,
    ) {
        match value {
            Value::Object(map) => {
                if let Some(reference) = map.get("reference").and_then(Value::as_str)
                    && (reference.starts_with("urn:uuid:") || reference.starts_with("urn:oid:"))
                    && !full_urls.contains(reference)
                {
                    out.push(
                        ValidationError::new(
                            ErrorKind::ReferenceUnresolved,
                            path.clone(),
                            errors::msg_reference_urn_unresolved(reference),
                        )
                        .with_severity(severity),
                    );
                }
                for (key, child) in map {
                    if child
                        .get("resourceType")
                        .and_then(Value::as_str)
                        .is_some_and(|rt| rt == "Bundle")
                    {
                        continue;
                    }
                    let len = path.len();
                    path.push('.');
                    path.push_str(key);
                    walk(child, path, full_urls, severity, out);
                    path.truncate(len);
                }
            }
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    let len = path.len();
                    path.push('.');
                    path.push_str(&index.to_string());
                    walk(item, path, full_urls, severity, out);
                    path.truncate(len);
                }
            }
            _ => {}
        }
    }

    for (index, entry) in entries.iter().enumerate() {
        let Some(res) = entry.get("resource") else {
            continue;
        };
        if res.get("resourceType").and_then(Value::as_str) == Some("Bundle") {
            continue;
        }
        let mut path = format!("Bundle.entry.{index}.resource");
        walk(res, &mut path, &full_urls, severity, &mut errors);
    }
    errors
}

/// The validator: a resolver plus the walk.
pub struct Validator {
    resolver: Arc<dyn SchemaResolver>,
}

impl Validator {
    pub fn new(resolver: Arc<dyn SchemaResolver>) -> Self {
        Self { resolver }
    }

    /// Pure, synchronous structural validation. No I/O, no FHIRPath —
    /// constraint/binding obligations are returned as [`Deferred`] entries
    /// instead of being executed.
    pub fn validate_sync(&self, resource: &Value, opts: &ValidationOptions) -> SyncOutcome {
        let mut outcome = walk::validate(self.resolver.as_ref(), resource, opts);
        outcome
            .errors
            .extend(bundle_entry_type_mismatches(resource));
        outcome.errors.extend(bundle_unresolved_urns(resource));
        outcome
    }

    /// Full validation: the structural walk plus execution of the deferred
    /// FHIRPath constraints and terminology bindings through `effects`.
    /// Structural issues come first, then constraint issues, then binding
    /// issues (each in walk order).
    pub async fn validate(
        &self,
        resource: &Value,
        version: FhirVersion,
        opts: &ValidationOptions,
        effects: &EffectHandlers<'_>,
    ) -> Vec<ValidationError> {
        let SyncOutcome {
            mut errors,
            deferred,
        } = self.validate_sync(resource, opts);
        crate::effects::execute(resource, version, &deferred, effects, &mut errors).await;
        errors
    }
}
