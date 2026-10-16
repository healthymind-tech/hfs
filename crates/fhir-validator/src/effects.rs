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
//! - bindings: `strength: required` is enforced; `extensible` draws a
//!   warning when [`EffectHandlers::check_extensible`] is on. The coded
//!   shape is the element's declared type when known, else inferred from
//!   the value (`code` string / Coding / CodeableConcept, mirroring the
//!   reference). Provider errors on required bindings surface as warning
//!   issues by default (fail-open); [`TerminologyFailMode`] escalates them
//!   — all of them (`Closed`), or only "ValueSet not known"
//!   (`ClosedRequired`) — to errors. Failures on extensible bindings are
//!   silent.
//! - codings: every Coding with a `display` (or an HL7-namespace `system`)
//!   is checked against its code system ([`TxCheck::Coding`]): a wrong
//!   display is a [`ErrorKind::DisplayMismatch`] issue of
//!   [`EffectHandlers::display_issues`] severity — never a binding failure —
//!   and an unknown code system in the HL7 namespaces is an error.
//!   Bindings and codings of one resource are asked of the provider in a
//!   single [`TerminologyProvider::check`] call.
//!
//! Issue order: all constraint issues (deferred order), then binding and
//! coding issues (deferred order, then document order) — always after the
//! structural errors.

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

    /// Every check of one resource at once, answers aligned with `checks`.
    /// The validator calls this once per resource, so a remote provider can
    /// answer them in a single round trip; the default asks one by one.
    async fn validate_codes(
        &self,
        checks: &[(&str, &CodedValue)],
    ) -> Vec<Result<bool, TerminologyError>> {
        let mut out = Vec::with_capacity(checks.len());
        for (value_set, coded) in checks {
            out.push(self.validate_code(value_set, coded).await);
        }
        out
    }

    /// Every terminology question of one resource at once — value-set
    /// membership and code-system checks of individual codings — answers
    /// aligned with `checks`. One call per resource, so a remote provider
    /// can answer them all in a single round trip.
    ///
    /// The default answers [`TxCheck::InValueSet`] through
    /// [`validate_codes`](Self::validate_codes) and leaves every
    /// [`TxCheck::Coding`] unjudged ([`TxVerdict::valid`]).
    async fn check(&self, checks: &[TxCheck<'_>]) -> Vec<Result<TxVerdict, TerminologyError>> {
        let membership: Vec<(usize, (&str, &CodedValue))> = checks
            .iter()
            .enumerate()
            .filter_map(|(i, c)| match c {
                TxCheck::InValueSet(vs, coded) => Some((i, (*vs, *coded))),
                TxCheck::Coding(_) => None,
            })
            .collect();
        let mut out: Vec<Result<TxVerdict, TerminologyError>> =
            vec![Ok(TxVerdict::valid()); checks.len()];
        if !membership.is_empty() {
            let requests: Vec<(&str, &CodedValue)> = membership.iter().map(|(_, c)| *c).collect();
            let verdicts = self.validate_codes(&requests).await;
            for ((i, _), verdict) in membership.iter().zip(verdicts) {
                out[*i] = verdict.map(TxVerdict::member);
            }
        }
        out
    }
}

/// One terminology question asked during a validation run.
#[derive(Debug, Clone, Copy)]
pub enum TxCheck<'a> {
    /// Is the coded value in the value set (`ValueSet/$validate-code`)?
    InValueSet(&'a str, &'a CodedValue),
    /// Is this Coding right for its code system (`CodeSystem/$validate-code`):
    /// is the system known, and is the `display` one the code system
    /// defines for the code? Asked for every Coding with a `system` and
    /// `code`, bound or not, as the HL7 validator does.
    Coding(&'a Value),
}

/// The answer to one [`TxCheck`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxVerdict {
    /// [`TxCheck::InValueSet`]: the coded value is in the value set. A
    /// wrong display never makes a code invalid; it only shows up in
    /// [`TxCheck::Coding`] answers. Always `true` for coding checks.
    pub valid: bool,
    /// [`TxCheck::Coding`]: the display is wrong; the message says which
    /// displays are valid (HL7 validator wording).
    pub wrong_display: Option<String>,
    /// [`TxCheck::Coding`]: the service knows no code system with this URL.
    pub unknown_system: bool,
}

impl TxVerdict {
    /// In the value set / nothing wrong (or nothing judged).
    pub fn valid() -> Self {
        Self::member(true)
    }

    /// A membership answer.
    pub fn member(valid: bool) -> Self {
        Self {
            valid,
            wrong_display: None,
            unknown_system: false,
        }
    }
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
pub struct EffectHandlers<'a> {
    pub terminology: Option<&'a dyn TerminologyProvider>,
    pub constraints: Option<&'a dyn ConstraintEvaluator>,
    /// Constraint ids to skip entirely (e.g. `dom-6`).
    pub suppress_constraints: &'a [String],
    /// How terminology-service failures on required bindings are reported.
    pub terminology_fail: TerminologyFailMode,
    /// Severity of a wrong `display` on a Coding (default warning, as HAPI
    /// and the HL7 validator's `--display-issues-are-warnings`; the HL7
    /// validator's own default is error).
    pub display_issues: Severity,
    /// Check `extensible` bindings too: a coded value from none of the
    /// value set's codes draws a warning (HL7 validator default). Off by
    /// default — only `required` bindings are enforced.
    pub check_extensible: bool,
    /// Report internal narrative hyperlinks (`<a href="#x">`) that resolve
    /// to nothing in the resource (error, as the HL7 validator does).
    pub narrative_links: bool,
    /// The HL7 validator's best-practice warnings (an Observation without
    /// a subject or performer).
    pub best_practice: bool,
}

impl Default for EffectHandlers<'_> {
    fn default() -> Self {
        Self {
            terminology: None,
            constraints: None,
            suppress_constraints: &[],
            terminology_fail: TerminologyFailMode::default(),
            display_issues: Severity::Warning,
            check_extensible: false,
            narrative_links: false,
            best_practice: false,
        }
    }
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
    execute_terminology(resource, deferred, handlers, errors).await;
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

/// What one terminology question in [`execute_terminology`] was asked for.
enum Asked<'a> {
    /// A `required` or (with [`EffectHandlers::check_extensible`])
    /// `extensible` binding on the node at `path`.
    Binding {
        path: &'a String,
        binding: &'a Binding,
        value: &'a Value,
        coded: CodedValue,
    },
    /// A Coding at `path` (dotted, numeric indices).
    Coding { path: String, coding: &'a Value },
}

/// Bindings and per-coding code-system checks, asked of the provider in
/// one call (one round trip for a remote provider).
async fn execute_terminology(
    resource: &Value,
    deferred: &[Deferred],
    handlers: &EffectHandlers<'_>,
    errors: &mut Vec<ValidationError>,
) {
    let Some(provider) = handlers.terminology else {
        return;
    };

    // Every value set bound at each path, across the cooperative layers.
    let mut bound: std::collections::HashMap<&str, Vec<(&str, Option<&str>)>> =
        std::collections::HashMap::new();
    for d in deferred {
        if let Deferred::Binding { path, binding, .. } = d {
            let vs = binding.value_set.split('|').next().unwrap_or_default();
            let entry = bound.entry(path.as_str()).or_default();
            if !entry.iter().any(|(v, s)| *v == vs && *s == binding.strength.as_deref()) {
                entry.push((vs, binding.strength.as_deref()));
            }
        }
    }
    let mut extensible_seen: HashSet<(&str, &str)> = HashSet::new();

    let mut asked: Vec<Asked<'_>> = Vec::new();
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
        match binding.strength.as_deref() {
            Some("required") => {}
            Some("extensible") if handlers.check_extensible => {}
            _ => continue,
        }
        let Some(coded) = coded_value(value, type_hint.as_deref()) else {
            continue; // shape not coded (e.g. null gap) — structural checks own it
        };
        if binding.strength.as_deref() == Some("extensible") {
            let vs = binding.value_set.split('|').next().unwrap_or_default();
            // A text-only concept satisfies an extensible binding; a bare
            // `code` (Address.state & co.) is not judged — its system is
            // implied and often external (USPS), so a negative answer is
            // mostly the service not knowing the system.
            if !has_code(&coded) || matches!(coded, CodedValue::Code(_)) {
                continue;
            }
            if !extensible_seen.insert((path.as_str(), vs)) {
                continue; // same set from several layers (`|4.0.1` or not)
            }
            if overridden(vs, bound.get(path.as_str()).map(Vec::as_slice).unwrap_or(&[])) {
                continue;
            }
        }
        asked.push(Asked::Binding {
            path,
            binding,
            value,
            coded,
        });
    }
    let root = resource
        .get("resourceType")
        .and_then(Value::as_str)
        .unwrap_or_default();
    for (path, coding) in crate::terminology::codings(resource, root) {
        // `system|version` is no code-system URL (the version belongs in
        // Coding.version); in the HL7 namespaces it can never resolve.
        if let Some(system) = coding.get("system").and_then(Value::as_str)
            && system.contains('|')
            && crate::terminology::is_hl7_namespace(system)
        {
            errors.push(ValidationError::new(
                ErrorKind::UnknownCodeSystem,
                format!("{path}.system"),
                messages::url_unresolved(system),
            ));
            continue;
        }
        // Only what can produce an issue is asked: a display to judge, or
        // an HL7-namespace system that must resolve.
        let has_display = coding.get("display").is_some_and(Value::is_string);
        let hl7 = coding
            .get("system")
            .and_then(Value::as_str)
            .is_some_and(crate::terminology::is_hl7_namespace);
        if !has_display && !hl7 {
            continue;
        }
        asked.push(Asked::Coding { path, coding });
    }
    if asked.is_empty() {
        return;
    }
    let requests: Vec<TxCheck<'_>> = asked
        .iter()
        .map(|a| match a {
            Asked::Binding { binding, coded, .. } => {
                TxCheck::InValueSet(binding.value_set.as_str(), coded)
            }
            Asked::Coding { coding, .. } => TxCheck::Coding(coding),
        })
        .collect();
    let verdicts = provider.check(&requests).await;
    debug_assert_eq!(verdicts.len(), asked.len(), "provider must align verdicts");

    for (asked, verdict) in asked.iter().zip(verdicts) {
        match asked {
            Asked::Binding {
                path,
                binding,
                value,
                coded,
            } => {
                let required = binding.strength.as_deref() == Some("required");
                match verdict {
                    Ok(v) if v.valid => {}
                    Ok(_) if required => {
                        errors.push(
                            ValidationError::new(
                                ErrorKind::TerminologyBinding,
                                (*path).clone(),
                                messages::terminology_binding(value, &binding.value_set),
                            )
                            .with_extra(
                                "binding",
                                serde_json::to_value(binding).expect("binding serializes"),
                            ),
                        );
                    }
                    Ok(_) => {
                        errors.push(
                            ValidationError::new(
                                ErrorKind::TerminologyBinding,
                                (*path).clone(),
                                messages::extensible_binding(coded, &binding.value_set),
                            )
                            .with_severity(Severity::Warning),
                        );
                    }
                    Err(e) if required => {
                        let severity = handlers.terminology_fail.severity(e.kind());
                        errors.push(
                            ValidationError::new(
                                ErrorKind::TerminologyBinding,
                                (*path).clone(),
                                messages::terminology_unavailable(
                                    &binding.value_set,
                                    e.message(),
                                ),
                            )
                            .with_severity(severity),
                        );
                    }
                    // An extensible binding the service cannot judge says
                    // nothing about the data.
                    Err(_) => {}
                }
            }
            Asked::Coding { path, coding } => {
                let Ok(verdict) = verdict else { continue };
                if let Some(message) = verdict.wrong_display {
                    errors.push(
                        ValidationError::new(
                            ErrorKind::DisplayMismatch,
                            format!("{path}.display"),
                            message,
                        )
                        .with_severity(handlers.display_issues),
                    );
                }
                if verdict.unknown_system
                    && let Some(system) = coding.get("system").and_then(Value::as_str)
                    && crate::terminology::is_hl7_namespace(system)
                {
                    // The HL7 validator resolves every uri in the HL7
                    // namespaces and rejects one nobody defines
                    // (Type_Specific_Checks_DT_URL_Resolve).
                    errors.push(ValidationError::new(
                        ErrorKind::UnknownCodeSystem,
                        format!("{path}.system"),
                        messages::url_unresolved(system),
                    ));
                }
            }
        }
    }
}

/// Whether an `extensible` binding to `vs` is superseded by another
/// binding on the same element: a profile that rebinds an element (TW Core
/// `identifier.type` to its own value set) replaces the base binding, as
/// the HL7 validator judges only the most specific one. A required binding
/// elsewhere on the element, or a non-core (IG) value set next to a core
/// one, wins; among several IG value sets the last layer's is checked.
fn overridden(vs: &str, bindings: &[(&str, Option<&str>)]) -> bool {
    let core = |v: &str| {
        v.starts_with("http://hl7.org/fhir/ValueSet/")
            || v.starts_with("http://terminology.hl7.org/ValueSet/")
    };
    let others: Vec<&(&str, Option<&str>)> = bindings.iter().filter(|(v, _)| *v != vs).collect();
    if others.iter().any(|(_, s)| *s == Some("required")) {
        return true;
    }
    if core(vs) {
        return others.iter().any(|(v, _)| !core(v));
    }
    // vs is an IG set: superseded only by a later IG set.
    let pos = bindings.iter().position(|(v, _)| *v == vs).unwrap_or(0);
    bindings[pos + 1..].iter().any(|(v, _)| *v != vs && !core(v))
}

/// Whether the coded value carries any code at all.
fn has_code(coded: &CodedValue) -> bool {
    match coded {
        CodedValue::Code(c) => !c.is_empty(),
        CodedValue::Coding(c) => c.get("code").and_then(Value::as_str).is_some(),
        CodedValue::CodeableConcept(cc) => cc
            .get("coding")
            .and_then(Value::as_array)
            .is_some_and(|cs| cs.iter().any(|c| c.get("code").and_then(Value::as_str).is_some())),
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
    fn an_ig_rebinding_supersedes_the_core_extensible_binding() {
        let core = "http://hl7.org/fhir/ValueSet/identifier-type";
        let tw = "https://twcore.mohw.gov.tw/ig/twcore/ValueSet/identifier-type-tw";
        let ext = Some("extensible");
        // TW Core rebinds Identifier.type: the core set is not checked, TW's is.
        let both = [(core, ext), (tw, ext)];
        assert!(overridden(core, &both));
        assert!(!overridden(tw, &both));
        // Alone, the core binding is checked.
        assert!(!overridden(core, &[(core, ext)]));
        // A required binding on the element supersedes an extensible one.
        let req = [(tw, Some("required")), (core, ext)];
        assert!(overridden(core, &req));
        // Profile on profile: the later IG set wins.
        let ig2 = "https://example.org/ValueSet/narrower";
        let chain = [(core, ext), (tw, ext), (ig2, ext)];
        assert!(overridden(tw, &chain));
        assert!(!overridden(ig2, &chain));
    }

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
