//! FHIRPath-backed [`ConstraintEvaluator`] (feature `fhirpath`).
//!
//! Per resource: the raw JSON is parsed **once** into the typed
//! `helios_fhir` model (the `crates/sof` pattern) — helios-fhirpath
//! evaluates against typed resources, not raw JSON. Each constraint is then
//! evaluated as one combined expression: for a constraint attached below
//! the root, the validator path is rendered as a relative FHIRPath, and the
//! invariant runs per node inside `.all(...)`, coerced with `allTrue()`:
//!
//! ```text
//! path Patient.contact.0, expr "name.exists() or telecom.exists()"
//!   → contact[0].all((name.exists() or telecom.exists()).allTrue())
//! ```
//!
//! `all()` gives the per-node focus and passes an absent node. `allTrue()`
//! turns a node's empty result into a pass, as FHIR invariant semantics
//! require for every node, not only the root: `all()` alone wants each
//! criterion to be `true`, so ref-1 on a logical, identifier-only reference
//! (`{} or {}`) was reported as a violation. (`select(...).allTrue()` has the
//! same semantics but measured twice as slow in helios-fhirpath.)
//! Root-attached constraints evaluate as-is.
//!
//! Result coercion (FHIRPath 5.1.1 / FHIR invariant semantics): empty →
//! pass; a single boolean → itself; anything else → not evaluable
//! (surfaced as a warning issue by the effects layer). Note the JS
//! reference validator fails on empty — a known bug there, upstream
//! fixtures don't pin it.
//!
//! Parsed expression ASTs are memoized per evaluator instance — core
//! invariants repeat across every resource of a type.
//!
//! Known limitation (documented in the plan): helios-fhirpath resolves
//! `%resource`/`%rootResource` to the root resource, so constraints that
//! depend on nested-resource `%resource` semantics may misfire; and the
//! `.all()` wrapping makes `%context` the root rather than the node.

use crate::effects::{ConstraintEvaluator, ConstraintOutcome, DeferredConstraint};
use helios_fhir::{FhirResource, FhirVersion};
use helios_fhirpath::parser::{Expression, Invocation, Term};
use helios_fhirpath::{EvaluationContext, EvaluationResult};
use serde::Deserialize;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Evaluates invariants with helios-fhirpath. Cheap to construct; hold one
/// per service for the lifetime of the process to benefit from the AST
/// cache.
#[derive(Default)]
pub struct FhirPathConstraintEvaluator {
    /// Parse cache, keyed by the invariant's own (effective) expression —
    /// never by the node path, so `ele-1` on the 600th element of a
    /// snapshot reuses the AST parsed for the first one. Read-mostly, so a
    /// `RwLock`: concurrent validations only share read access once warm.
    asts: RwLock<HashMap<String, Arc<Expression>>>,
}

impl FhirPathConstraintEvaluator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of distinct expressions parsed so far (cache size).
    pub fn cached_expressions(&self) -> usize {
        self.asts.read().expect("ast cache lock").len()
    }

    /// The parsed AST for `expression`, parsed at most once per process.
    fn ast(&self, expression: &str) -> Result<Arc<Expression>, String> {
        if let Some(ast) = self.asts.read().expect("ast cache lock").get(expression) {
            return Ok(Arc::clone(ast));
        }
        let ast = Arc::new(helios_fhirpath::parse_expression(expression)?);
        self.asts
            .write()
            .expect("ast cache lock")
            .entry(expression.to_string())
            .or_insert_with(|| Arc::clone(&ast));
        Ok(ast)
    }
}

impl ConstraintEvaluator for FhirPathConstraintEvaluator {
    fn evaluate_all(
        &self,
        resource: &Value,
        version: FhirVersion,
        constraints: &[DeferredConstraint<'_>],
    ) -> Vec<ConstraintOutcome> {
        if constraints.is_empty() {
            return Vec::new();
        }
        let typed = match parse_typed(resource, version) {
            Ok(t) => t,
            Err(e) => {
                // Structural errors are already on the report when the
                // typed parse fails; constraints simply cannot run.
                let detail = format!("resource does not parse into the typed model: {e}");
                return constraints
                    .iter()
                    .map(|_| ConstraintOutcome::NotEvaluable(detail.clone()))
                    .collect();
            }
        };
        let mut context = EvaluationContext::new(vec![typed]);
        // Sound: every AST evaluated here is held by `self.asts` for the
        // life of the evaluator, which outlives this context.
        context.enable_root_memo();
        let mut nodes = NodeCache::default();

        constraints
            .iter()
            .map(|c| {
                let expression = effective_expression(c.id, c.expression);
                match self.ast(&expression) {
                    Ok(ast) => evaluate_at(&ast, c.path, &context, &mut nodes),
                    Err(e) => ConstraintOutcome::NotEvaluable(format!("parse error: {e}")),
                }
            })
            .collect()
    }
}

/// The focus collections of the constraint paths seen so far in one
/// resource, keyed by the relative path (`contact.0`). Every node is
/// navigated to once, from its already-navigated parent — before, every
/// constraint re-navigated from the root, and indexing into an array
/// (`element[523]`) copied the whole array first, so a 600-element
/// StructureDefinition paid ~600 array copies per element invariant.
#[derive(Default)]
struct NodeCache {
    nodes: HashMap<String, EvaluationResult>,
}

impl NodeCache {
    /// The focus for a relative dotted path (segments after the resource
    /// type), navigated with the evaluator's own member access and the
    /// indexer's semantics, i.e. exactly what `contact[0]` evaluates to.
    fn focus(
        &mut self,
        relative: &str,
        context: &EvaluationContext,
    ) -> Result<&EvaluationResult, String> {
        if !self.nodes.contains_key(relative) {
            let value = match relative.rsplit_once('.') {
                Some((parent, last)) => {
                    self.focus(parent, context)?;
                    step(&self.nodes[parent], last, context)?
                }
                None => match &context.this {
                    Some(root) => step(root, relative, context)?,
                    None => EvaluationResult::Empty,
                },
            };
            self.nodes.insert(relative.to_string(), value);
        }
        Ok(&self.nodes[relative])
    }
}

/// One navigation step: a numeric segment indexes (FHIRPath indexer: an
/// item of a collection, nothing on a single non-string value), anything
/// else is a member access evaluated by helios-fhirpath on `base`.
fn step(
    base: &EvaluationResult,
    segment: &str,
    context: &EvaluationContext,
) -> Result<EvaluationResult, String> {
    if !segment.is_empty() && segment.chars().all(|c| c.is_ascii_digit()) {
        let index: usize = segment.parse().unwrap_or(usize::MAX);
        return Ok(match base {
            EvaluationResult::Collection { items, .. } => {
                items.get(index).cloned().unwrap_or(EvaluationResult::Empty)
            }
            EvaluationResult::String(s, _, _) => match s.chars().nth(index) {
                Some(c) => EvaluationResult::string(c.to_string()),
                None => EvaluationResult::Empty,
            },
            _ => EvaluationResult::Empty,
        });
    }
    if matches!(base, EvaluationResult::Empty) {
        return Ok(EvaluationResult::Empty);
    }
    let member = Expression::Term(Term::Invocation(Invocation::Member(segment.to_string())));
    helios_fhirpath::evaluator::evaluate(&member, context, Some(base)).map_err(|e| e.to_string())
}

/// Evaluate one invariant AST with the node at the dotted validator `path`
/// as focus — the same semantics as the combined expression
/// `<relative path>.all((<expr>).allTrue())` (see the module docs), without
/// parsing a path-specific expression or re-navigating from the root.
fn evaluate_at(
    ast: &Expression,
    path: &str,
    context: &EvaluationContext,
    nodes: &mut NodeCache,
) -> ConstraintOutcome {
    let relative = path.split_once('.').map(|(_, rest)| rest).unwrap_or("");
    if relative.is_empty() {
        return match helios_fhirpath::evaluator::evaluate(ast, context, None) {
            Ok(result) => coerce(result),
            Err(e) => ConstraintOutcome::NotEvaluable(format!("evaluation error: {e}")),
        };
    }
    let focus = match nodes.focus(relative, context) {
        Ok(focus) => focus,
        Err(e) => return ConstraintOutcome::NotEvaluable(format!("evaluation error: {e}")),
    };
    let items: &[EvaluationResult] = match focus {
        EvaluationResult::Collection { items, .. } => items,
        EvaluationResult::Empty => &[],
        single => std::slice::from_ref(single),
    };
    for item in items {
        let result = match helios_fhirpath::evaluator::evaluate(ast, context, Some(item)) {
            Ok(result) => result,
            Err(e) => return ConstraintOutcome::NotEvaluable(format!("evaluation error: {e}")),
        };
        // `(expr).allTrue()`: empty passes, every item must be `true`.
        let values: &[EvaluationResult] = match &result {
            EvaluationResult::Collection { items, .. } => items,
            EvaluationResult::Empty => &[],
            single => std::slice::from_ref(single),
        };
        for value in values {
            match value {
                EvaluationResult::Boolean(true, ..) => {}
                EvaluationResult::Boolean(false, ..) | EvaluationResult::Empty => {
                    return ConstraintOutcome::Failed;
                }
                _ => {
                    return ConstraintOutcome::NotEvaluable(
                        "evaluation error: Type Error: allTrue expects a collection of Booleans"
                            .to_string(),
                    );
                }
            }
        }
    }
    ConstraintOutcome::Passed
}

/// The expression actually evaluated for a constraint.
///
/// - R4 through R5 give the two narrative invariants the same expression,
///   `htmlChecks()`, though they state different rules: `txt-1` the allowed
///   elements and attributes, `txt-2` "SHALL have some non-whitespace
///   content". Evaluating `txt-2` as written would re-check `txt-1` and leave
///   the content rule unchecked, so it runs `htmlChecks2()`, the content
///   check (the HL7 validator's name for it). R6 folds both rules into
///   `txt-1` and has no `txt-2`.
/// - The `as(type)` *function* filters a collection by type in the HL7
///   validator, and invariants rely on it: dom-3 applies it to
///   `%resource.descendants()`. helios-fhirpath follows the FHIRPath
///   spec's singleton rule and fails, so dom-3 never ran. The function form
///   is evaluated as `ofType(type)`, which is the same on a single item and
///   filters a collection; the `as` operator is left alone.
fn effective_expression<'a>(id: &str, expression: &'a str) -> Cow<'a, str> {
    if id == "txt-2" && expression.trim() == "htmlChecks()" {
        return Cow::Borrowed("htmlChecks2()");
    }
    if expression.contains(".as(") {
        return Cow::Owned(expression.replace(".as(", ".ofType("));
    }
    Cow::Borrowed(expression)
}

/// Build the combined focus+invariant expression — the former evaluation
/// plumbing, kept as the reference [`evaluate_at`] is tested against from a dotted validator
/// path (`Patient.contact.0` → `contact[0].all((<expr>).allTrue())`).
#[cfg(test)]
fn combine(path: &str, expression: &str) -> String {
    let mut relative = String::new();
    for segment in path.split('.').skip(1) {
        if segment.chars().all(|c| c.is_ascii_digit()) && !segment.is_empty() {
            relative.push('[');
            relative.push_str(segment);
            relative.push(']');
        } else {
            if !relative.is_empty() {
                relative.push('.');
            }
            relative.push_str(segment);
        }
    }
    if relative.is_empty() {
        expression.to_string()
    } else {
        format!("{relative}.all(({expression}).allTrue())")
    }
}

/// FHIRPath 5.1.1 boolean coercion with FHIR invariant semantics.
fn coerce(result: EvaluationResult) -> ConstraintOutcome {
    match result {
        EvaluationResult::Empty => ConstraintOutcome::Passed,
        EvaluationResult::Boolean(b, ..) => {
            if b {
                ConstraintOutcome::Passed
            } else {
                ConstraintOutcome::Failed
            }
        }
        EvaluationResult::Collection { items, .. } => match items.as_slice() {
            [] => ConstraintOutcome::Passed,
            [EvaluationResult::Boolean(b, ..)] => {
                if *b {
                    ConstraintOutcome::Passed
                } else {
                    ConstraintOutcome::Failed
                }
            }
            [_] => ConstraintOutcome::NotEvaluable("single non-boolean result".to_string()),
            _ => ConstraintOutcome::NotEvaluable("multiple results".to_string()),
        },
        _ => ConstraintOutcome::NotEvaluable("single non-boolean result".to_string()),
    }
}

/// Parse raw JSON into the version's typed model (`crates/sof` pattern).
fn parse_typed(resource: &Value, version: FhirVersion) -> Result<FhirResource, String> {
    match version {
        #[cfg(feature = "R4")]
        FhirVersion::R4 => helios_fhir::r4::Resource::deserialize(resource)
            .map(|r| FhirResource::R4(Box::new(r)))
            .map_err(|e| e.to_string()),
        #[cfg(feature = "R4B")]
        FhirVersion::R4B => helios_fhir::r4b::Resource::deserialize(resource)
            .map(|r| FhirResource::R4B(Box::new(r)))
            .map_err(|e| e.to_string()),
        #[cfg(feature = "R5")]
        FhirVersion::R5 => helios_fhir::r5::Resource::deserialize(resource)
            .map(|r| FhirResource::R5(Box::new(r)))
            .map_err(|e| e.to_string()),
        #[cfg(feature = "R6")]
        FhirVersion::R6 => helios_fhir::r6::Resource::deserialize(resource)
            .map(|r| FhirResource::R6(Box::new(r)))
            .map_err(|e| e.to_string()),
        #[allow(unreachable_patterns)]
        other => Err(format!(
            "FHIR version {other:?} not enabled in helios-fhir-validator"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{combine, effective_expression};
    use crate::effects::{ConstraintEvaluator, ConstraintOutcome, Deferred, DeferredConstraint};
    use helios_fhir::FhirVersion;
    use serde_json::Value;

    /// The old plumbing: one combined expression per constraint, parsed and
    /// evaluated from the root.
    fn legacy(resource: &Value, constraints: &[DeferredConstraint<'_>]) -> Vec<ConstraintOutcome> {
        let typed = match super::parse_typed(resource, FhirVersion::R4) {
            Ok(t) => t,
            Err(e) => {
                let detail = format!("resource does not parse into the typed model: {e}");
                return constraints
                    .iter()
                    .map(|_| ConstraintOutcome::NotEvaluable(detail.clone()))
                    .collect();
            }
        };
        let context = helios_fhirpath::EvaluationContext::new(vec![typed]);
        constraints
            .iter()
            .map(|c| {
                let combined = combine(c.path, &effective_expression(c.id, c.expression));
                match helios_fhirpath::parse_expression(&combined) {
                    Ok(ast) => match helios_fhirpath::evaluator::evaluate(&ast, &context, None) {
                        Ok(result) => super::coerce(result),
                        Err(e) => ConstraintOutcome::NotEvaluable(format!("evaluation error: {e}")),
                    },
                    Err(e) => ConstraintOutcome::NotEvaluable(format!("parse error: {e}")),
                }
            })
            .collect()
    }

    fn same(a: &ConstraintOutcome, b: &ConstraintOutcome) -> bool {
        match (a, b) {
            (ConstraintOutcome::NotEvaluable(_), ConstraintOutcome::NotEvaluable(_)) => true,
            _ => a == b,
        }
    }

    /// Node-cached evaluation gives exactly the outcomes of the combined
    /// expressions it replaced, over the R4 spec example corpus (every
    /// invariant the core pack attaches, at every node path).
    #[test]
    fn node_cached_evaluation_matches_combined_expressions() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fhir/tests/data/json/R4");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("spec corpus not present at {}; skipping", dir.display());
            return;
        };
        let mut files: Vec<_> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        files.sort();
        let validator = crate::Validator::new(crate::packs::core_registry(FhirVersion::R4));
        let evaluator = super::FhirPathConstraintEvaluator::new();
        let (mut resources, mut checked, mut failed) = (0usize, 0usize, 0usize);
        let mut diffs = Vec::new();
        // The legacy plumbing is quadratic in resource size, so by default
        // a sample of small files keeps a debug run short;
        // `VALIDATOR_FULL_CORPUS=1` (best with `--release`) checks them all.
        let full = std::env::var_os("VALIDATOR_FULL_CORPUS").is_some();
        let step = if full { 1 } else { 5 };
        for path in files.iter().step_by(step) {
            if !full && std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > 12_000 {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(path) else { continue };
            let Ok(resource) = serde_json::from_str::<Value>(&text) else { continue };
            if resource.get("resourceType").is_none() {
                continue;
            }
            let outcome = validator.validate_sync(&resource, &crate::ValidationOptions::default());
            let constraints: Vec<DeferredConstraint<'_>> = outcome
                .deferred
                .iter()
                .filter_map(|d| match d {
                    Deferred::Constraint { path, id, expression, .. } => Some(DeferredConstraint {
                        path,
                        id,
                        expression,
                    }),
                    _ => None,
                })
                .collect();
            resources += 1;
            let new = evaluator.evaluate_all(&resource, FhirVersion::R4, &constraints);
            let old = legacy(&resource, &constraints);
            for ((c, n), o) in constraints.iter().zip(&new).zip(&old) {
                checked += 1;
                if *n == ConstraintOutcome::Failed {
                    failed += 1;
                }
                if !same(n, o) {
                    diffs.push(format!(
                        "{}: {} at {}: new {n:?} old {o:?}",
                        path.file_name().unwrap().to_string_lossy(),
                        c.id,
                        c.path
                    ));
                }
            }
        }
        assert!(resources > 300, "corpus too small: {resources}");
        assert!(
            diffs.is_empty(),
            "{} of {checked} outcomes differ ({failed} failures):\n{}",
            diffs.len(),
            diffs.iter().take(40).cloned().collect::<Vec<_>>().join("\n")
        );
        // The cache is keyed by expression, not by node path.
        assert!(evaluator.cached_expressions() < 2_000, "{}", evaluator.cached_expressions());
        eprintln!("{resources} resources, {checked} constraint outcomes identical ({failed} failures)");
    }

    /// Per-invariant cost breakdown for one resource (diagnostics, not a
    /// check): `CONSTRAINT_PROFILE=<file.json> cargo test --release -p
    /// helios-fhir-validator --features fhirpath --lib constraint_cost --
    /// --ignored --nocapture`.
    #[test]
    #[ignore]
    fn constraint_cost_profile() {
        let Some(path) = std::env::var_os("CONSTRAINT_PROFILE") else {
            return;
        };
        let resource: Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("json");
        let validator = crate::Validator::new(crate::packs::core_registry(FhirVersion::R4));
        let outcome = validator.validate_sync(&resource, &crate::ValidationOptions::default());
        let constraints: Vec<DeferredConstraint<'_>> = outcome
            .deferred
            .iter()
            .filter_map(|d| match d {
                Deferred::Constraint { path, id, expression, .. } => Some(DeferredConstraint {
                    path,
                    id,
                    expression,
                }),
                _ => None,
            })
            .collect();
        let evaluator = super::FhirPathConstraintEvaluator::new();
        let started = std::time::Instant::now();
        evaluator.evaluate_all(&resource, FhirVersion::R4, &constraints);
        eprintln!("{} constraints, all at once: {:?}", constraints.len(), started.elapsed());
        // Per invariant id: all its occurrences in one call (one typed parse
        // and node navigation each, subtracted below).
        let mut groups: std::collections::BTreeMap<&str, Vec<DeferredConstraint<'_>>> =
            Default::default();
        for c in &constraints {
            groups.entry(c.id).or_default().push(*c);
        }
        let mut by_id: std::collections::BTreeMap<&str, (usize, std::time::Duration)> =
            Default::default();
        for (id, group) in &groups {
            let t = std::time::Instant::now();
            evaluator.evaluate_all(&resource, FhirVersion::R4, group);
            by_id.insert(id, (group.len(), t.elapsed()));
        }
        let typed = std::time::Instant::now();
        let _ = super::parse_typed(&resource, FhirVersion::R4)
            .map(|t| helios_fhirpath::EvaluationContext::new(vec![t]));
        let per_call = typed.elapsed();
        let mut rows: Vec<_> = by_id.into_iter().collect();
        rows.sort_by_key(|(_, (_, d))| std::cmp::Reverse(*d));
        eprintln!("(each group call includes the typed parse + context: {per_call:?})");
        for (id, (n, d)) in rows.iter().take(25) {
            eprintln!("{id:>10} x{n:<5} {d:?}");
        }
    }

    #[test]
    fn txt_2_checks_content_not_names() {
        assert_eq!(
            effective_expression("txt-2", "htmlChecks()"),
            "htmlChecks2()"
        );
        assert_eq!(
            effective_expression("txt-1", "htmlChecks()"),
            "htmlChecks()"
        );
        assert_eq!(effective_expression("txt-2", "other()"), "other()");
    }

    #[test]
    fn as_function_filters_like_of_type() {
        assert_eq!(
            effective_expression("dom-3", "%resource.descendants().as(canonical)"),
            "%resource.descendants().ofType(canonical)"
        );
        assert_eq!(
            effective_expression("x-1", "(value as Quantity).exists()"),
            "(value as Quantity).exists()"
        );
    }

    #[test]
    fn combines_paths_into_focused_expressions() {
        assert_eq!(combine("Patient", "name.exists()"), "name.exists()");
        assert_eq!(
            combine("Patient.contact.0", "name.exists() or telecom.exists()"),
            "contact[0].all((name.exists() or telecom.exists()).allTrue())"
        );
        assert_eq!(
            combine("Patient.name.2.given", "length() < 10"),
            "name[2].given.all((length() < 10).allTrue())"
        );
    }
}
