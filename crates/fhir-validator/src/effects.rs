//! Deferred effects: validations collected during the pure synchronous walk
//! and executed afterwards.
//!
//! The structural walk ([`crate::engine`]) never performs I/O and never
//! evaluates FHIRPath. When it encounters a `constraints` or `binding`
//! keyword it records a [`Deferred`] entry; [`execute`] runs them through
//! pluggable handlers — a synchronous [`ConstraintEvaluator`] (the
//! FHIRPath-backed implementation lives behind the `fhirpath` feature) and
//! an async [`TerminologyProvider`] (HTTP `$validate-code`, in-process hts,
//! or a test stub).
//!
//! Execution semantics:
//! - constraints: deduplicated by `(path, id)` across cooperative layers;
//!   `severity: error` failures emit error issues, `warning` failures emit
//!   warning issues, `guideline` entries are skipped, ids on the suppress
//!   list are skipped, and evaluation problems surface as warning issues
//!   (never silent). An **empty** FHIRPath result counts as a pass, per
//!   FHIR invariant semantics (the reference validator treats empty as a
//!   failure — a known bug there).
//! - bindings: only `strength: required` is enforced. The coded shape is
//!   the element's declared type when known, else inferred from the value
//!   (`code` string / Coding / CodeableConcept, mirroring the reference).
//!   Provider errors surface as warning issues by default (fail-open);
//!   [`TerminologyFailMode`] escalates them — all of them (`Closed`), or
//!   only "ValueSet not known" (`ClosedRequired`) — to errors.
//!
//! Issue order: all constraint issues (deferred order), then all binding
//! issues (deferred order) — always after the structural errors.

use crate::engine::{ErrorKind, Severity, ValidationError};
use crate::schema::Binding;
use async_trait::async_trait;
use helios_fhir::FhirVersion;
use serde_json::Value;
use std::collections::HashSet;

// Message templates live with the rest of the contract in engine::errors;
// re-exported crate-internally via these thin wrappers.
use crate::engine::messages;

/// A validation obligation collected during the sync walk.
#[derive(Debug, Clone, PartialEq)]
pub enum Deferred {
    /// A FHIRPath invariant to evaluate with the node at `path` as focus.
    Constraint {
        /// Dotted conformance path of the node the constraint attaches to.
        path: String,
        /// Constraint id (e.g. `pat-1`).
        id: String,
        /// FHIRPath expression.
        expression: String,
        /// Human-readable description (used in the emitted message).
        human: Option<String>,
        /// `error` | `warning` | `guideline`.
        severity: Option<String>,
    },
    /// A terminology binding to check (filtered to `required` strength at
    /// execution time).
    Binding {
        /// Dotted conformance path of the coded node.
        path: String,
        /// The binding (ValueSet canonical + strength).
        binding: Binding,
        /// The coded value as found in the data.
        value: Value,
        /// The element's declared type, when the schema stated one.
        type_hint: Option<String>,
    },
}

/// A coded value in one of FHIR's three coded shapes.
#[derive(Debug, Clone, PartialEq)]
pub enum CodedValue {
    /// A bare `code` string (system to be inferred from the ValueSet).
    Code(String),
    /// A Coding object.
    Coding(Value),
    /// A CodeableConcept object.
    CodeableConcept(Value),
}

/// Checks coded-value membership in a ValueSet (`$validate-code`).
#[async_trait]
pub trait TerminologyProvider: Send + Sync {
    /// `Ok(true)` = the coded value is in the ValueSet.
    async fn validate_code(
        &self,
        value_set: &str,
        coded: &CodedValue,
    ) -> Result<bool, TerminologyError>;
}

/// Why a terminology check could not produce a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TerminologyErrorKind {
    /// The service could not be reached, timed out, or answered
    /// unintelligibly — the code may well be valid.
    #[default]
    Unavailable,
    /// The service does not know the ValueSet the binding names. For a
    /// `required` binding this is a defect of the deployment (the IG's
    /// terminology was never loaded), not a transient outage.
    UnknownValueSet,
    /// The ValueSet is known, but the code system of the coded value is not
    /// loaded on the service (absent, or only a `content: not-present`
    /// stub such as HGNC, HGVS or the CDC race codes in `hl7.terminology`),
    /// so the code itself cannot be checked. The HL7 validator reports this
    /// as a warning ("A definition for CodeSystem … could not be found, so
    /// the code cannot be validated"), not as an invalid code.
    UnknownCodeSystem,
}

/// A terminology-service failure (network, unknown ValueSet, ...).
#[derive(Debug, Clone)]
pub struct TerminologyError {
    message: String,
    kind: TerminologyErrorKind,
}

impl TerminologyError {
    /// A failure of kind [`TerminologyErrorKind::Unavailable`].
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: TerminologyErrorKind::Unavailable,
        }
    }

    /// A failure of kind [`TerminologyErrorKind::UnknownValueSet`].
    pub fn unknown_value_set(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: TerminologyErrorKind::UnknownValueSet,
        }
    }

    /// A failure of kind [`TerminologyErrorKind::UnknownCodeSystem`].
    pub fn unknown_code_system(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: TerminologyErrorKind::UnknownCodeSystem,
        }
    }

    pub fn kind(&self) -> TerminologyErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for TerminologyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for TerminologyError {}

/// What a terminology failure on a `required` binding becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TerminologyFailMode {
    /// Every failure is a warning: validation never blocks on terminology
    /// (the default).
    #[default]
    Open,
    /// Every failure is an error, outages included.
    Closed,
    /// An unknown ValueSet ([`TerminologyErrorKind::UnknownValueSet`]) is an
    /// error — the binding could not have been satisfied by any deployment
    /// state the client controls — while an outage, or a code system the
    /// service has no content for, stays a warning.
    ClosedRequired,
}

impl TerminologyFailMode {
    /// The severity a failure of `kind` is reported with.
    pub fn severity(self, kind: TerminologyErrorKind) -> Severity {
        match (self, kind) {
            (TerminologyFailMode::Open, _) => Severity::Warning,
            (TerminologyFailMode::Closed, _) => Severity::Error,
            (TerminologyFailMode::ClosedRequired, TerminologyErrorKind::UnknownValueSet) => {
                Severity::Error
            }
            (
                TerminologyFailMode::ClosedRequired,
                TerminologyErrorKind::Unavailable | TerminologyErrorKind::UnknownCodeSystem,
            ) => Severity::Warning,
        }
    }
}

/// The outcome of evaluating one constraint.
#[derive(Debug, Clone, PartialEq)]
pub enum ConstraintOutcome {
    Passed,
    Failed,
    /// The expression could not be evaluated (parse error, non-boolean
    /// result, typed-model mismatch); surfaced as a warning issue.
    NotEvaluable(String),
}

/// Evaluates FHIRPath invariants against a resource. Implementations parse
/// the resource once per call and evaluate every constraint against it.
pub trait ConstraintEvaluator: Send + Sync {
    /// Outcomes aligned index-for-index with `constraints`.
    fn evaluate_all(
        &self,
        resource: &Value,
        version: FhirVersion,
        constraints: &[DeferredConstraint<'_>],
    ) -> Vec<ConstraintOutcome>;
}

/// A borrowed view of one deferred constraint, handed to evaluators.
#[derive(Debug, Clone, Copy)]
pub struct DeferredConstraint<'a> {
    /// Dotted conformance path of the node the constraint attaches to
    /// (rooted at the resource type, numeric indices).
    pub path: &'a str,
    pub id: &'a str,
    pub expression: &'a str,
}

/// The handlers for one validation run.
#[derive(Default)]
pub struct EffectHandlers<'a> {
    pub terminology: Option<&'a dyn TerminologyProvider>,
    pub constraints: Option<&'a dyn ConstraintEvaluator>,
    /// Constraint ids to skip entirely (e.g. `dom-6`).
    pub suppress_constraints: &'a [String],
    /// How terminology-service failures on required bindings are reported.
    pub terminology_fail: TerminologyFailMode,
}

/// Execute the deferred obligations, appending issues to `errors`.
pub async fn execute(
    resource: &Value,
    version: FhirVersion,
    deferred: &[Deferred],
    handlers: &EffectHandlers<'_>,
    errors: &mut Vec<ValidationError>,
) {
    execute_constraints(resource, version, deferred, handlers, errors);
    execute_bindings(deferred, handlers, errors).await;
    // Wrong displays on required-bound codings, judged offline against
    // the embedded core code systems whenever bindings are checked at all.
    if handlers.terminology.is_some() {
        errors.extend(crate::terminology::display_issues(version, deferred));
    }
}

fn execute_constraints(
    resource: &Value,
    version: FhirVersion,
    deferred: &[Deferred],
    handlers: &EffectHandlers<'_>,
    errors: &mut Vec<ValidationError>,
) {
    let Some(evaluator) = handlers.constraints else {
        return;
    };

    // Select, filter, and dedup — preserving deferred order.
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut selected: Vec<(&str, &str, &str, Option<&str>, Severity)> = Vec::new();
    for d in deferred {
        let Deferred::Constraint {
            path,
            id,
            expression,
            human,
            severity,
        } = d
        else {
            continue;
        };
        let issue_severity = match severity.as_deref() {
            None | Some("error") => Severity::Error,
            Some("warning") => Severity::Warning,
            _ => continue, // guideline and unknown severities: not evaluated
        };
        if handlers.suppress_constraints.iter().any(|s| s == id) {
            continue;
        }
        if !seen.insert((path.clone(), id.clone())) {
            continue; // same constraint from several cooperative layers
        }
        selected.push((path, id, expression, human.as_deref(), issue_severity));
    }
    if selected.is_empty() {
        return;
    }

    let refs: Vec<DeferredConstraint<'_>> = selected
        .iter()
        .map(|(path, id, expression, _, _)| DeferredConstraint {
            path,
            id,
            expression,
        })
        .collect();
    let outcomes = evaluator.evaluate_all(resource, version, &refs);
    debug_assert_eq!(outcomes.len(), refs.len(), "evaluator must align outcomes");

    // When nothing could be evaluated for the same reason (the resource does
    // not parse into the typed model — its structural errors are already on
    // the report), say so once instead of once per invariant.
    if outcomes.len() > 1
        && let Some(ConstraintOutcome::NotEvaluable(first)) = outcomes.first()
        && outcomes
            .iter()
            .all(|o| matches!(o, ConstraintOutcome::NotEvaluable(d) if d == first))
    {
        let root = resource
            .get("resourceType")
            .and_then(Value::as_str)
            .unwrap_or_default();
        errors.push(
            ValidationError::new(
                ErrorKind::FhirpathConstraint,
                root.to_string(),
                messages::constraints_not_evaluable(first),
            )
            .with_severity(Severity::Warning),
        );
        return;
    }

    for ((path, id, _, human, severity), outcome) in selected.iter().zip(outcomes) {
        match outcome {
            ConstraintOutcome::Passed => {}
            ConstraintOutcome::Failed => {
                errors.push(
                    ValidationError::new(
                        ErrorKind::FhirpathConstraint,
                        path.to_string(),
                        messages::fhirpath_constraint(id, *human),
                    )
                    .with_severity(*severity)
                    .with_extra("constraint", Value::String(id.to_string())),
                );
            }
            ConstraintOutcome::NotEvaluable(detail) => {
                errors.push(
                    ValidationError::new(
                        ErrorKind::FhirpathConstraint,
                        path.to_string(),
                        messages::constraint_not_evaluable(id, &detail),
                    )
                    .with_severity(Severity::Warning)
                    .with_extra("constraint", Value::String(id.to_string())),
                );
            }
        }
    }
}

async fn execute_bindings(
    deferred: &[Deferred],
    handlers: &EffectHandlers<'_>,
    errors: &mut Vec<ValidationError>,
) {
    let Some(provider) = handlers.terminology else {
        return;
    };

    for d in deferred {
        let Deferred::Binding {
            path,
            binding,
            value,
            type_hint,
        } = d
        else {
            continue;
        };
        if binding.strength.as_deref() != Some("required") {
            continue;
        }
        let Some(coded) = coded_value(value, type_hint.as_deref()) else {
            continue; // shape not coded (e.g. null gap) — structural checks own it
        };
        match provider.validate_code(&binding.value_set, &coded).await {
            Ok(true) => {}
            Ok(false) => {
                errors.push(
                    ValidationError::new(
                        ErrorKind::TerminologyBinding,
                        path.clone(),
                        messages::terminology_binding(value, &binding.value_set),
                    )
                    .with_extra(
                        "binding",
                        serde_json::to_value(binding).expect("binding serializes"),
                    ),
                );
            }
            Err(e) => {
                let severity = handlers.terminology_fail.severity(e.kind());
                errors.push(
                    ValidationError::new(
                        ErrorKind::TerminologyBinding,
                        path.clone(),
                        messages::terminology_unavailable(&binding.value_set, e.message()),
                    )
                    .with_severity(severity),
                );
            }
        }
    }
}

/// Determine the coded shape: the element's declared type wins, inference
/// from the value is the fallback (mirroring the reference validator).
pub(crate) fn coded_value(value: &Value, type_hint: Option<&str>) -> Option<CodedValue> {
    match type_hint {
        Some("code") | Some("string") | Some("uri") | Some("canonical") => {
            value.as_str().map(|s| CodedValue::Code(s.to_string()))
        }
        Some("Coding") => value.is_object().then(|| CodedValue::Coding(value.clone())),
        Some("CodeableConcept") => value
            .is_object()
            .then(|| CodedValue::CodeableConcept(value.clone())),
        Some("Quantity") => {
            // Quantity binds via its system+code pair — treated as a Coding.
            value.is_object().then(|| CodedValue::Coding(value.clone()))
        }
        _ => match value {
            Value::String(s) => Some(CodedValue::Code(s.clone())),
            Value::Object(o) if o.contains_key("coding") || o.contains_key("text") => {
                Some(CodedValue::CodeableConcept(value.clone()))
            }
            Value::Object(_) => Some(CodedValue::Coding(value.clone())),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_code_system_blocks_only_when_fully_closed() {
        let kind = TerminologyError::unknown_code_system("no content").kind();
        assert_eq!(kind, TerminologyErrorKind::UnknownCodeSystem);
        assert_eq!(TerminologyFailMode::Open.severity(kind), Severity::Warning);
        assert_eq!(
            TerminologyFailMode::ClosedRequired.severity(kind),
            Severity::Warning
        );
        assert_eq!(TerminologyFailMode::Closed.severity(kind), Severity::Error);
        assert_eq!(
            TerminologyFailMode::ClosedRequired.severity(TerminologyErrorKind::UnknownValueSet),
            Severity::Error
        );
    }
}
