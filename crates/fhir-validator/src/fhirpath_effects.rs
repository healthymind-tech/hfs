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
//! Environment variables (FHIR invariant semantics): `%context` is the node
//! the invariant is evaluated on; `%resource` the resource that node belongs
//! to — the node itself for a resource definition's own invariants on a
//! contained or entry resource, else the innermost resource containing it;
//! `%rootResource` the container of `%resource` when that is a contained
//! resource, else `%resource`. helios-fhirpath resolves all three to the root
//! resource unless bound, so they are bound whenever they differ from it.

use crate::effects::{
    CodedValue, ConstraintEvaluator, ConstraintOutcome, ConstraintSession, DeferredConstraint,
    MemberOfAnswers,
};
use helios_fhir::{FhirResource, FhirVersion};
use helios_fhirpath::parser::{Expression, Invocation, Term};
use helios_fhirpath::{EvaluationContext, EvaluationResult};
use serde::Deserialize;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

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
    /// Treat an empty result as a failed invariant, as the HL7 Java
    /// validator's `evaluateToBoolean` does (default: empty passes, the
    /// FHIRPath boolean coercion).
    empty_fails: bool,
    /// Typed parses done so far (see [`Self::typed_parses`]).
    parses: AtomicUsize,
}

impl FhirPathConstraintEvaluator {
    pub fn new() -> Self {
        Self::default()
    }

    /// See [`Self::empty_fails`](FhirPathConstraintEvaluator).
    pub fn with_empty_fails(mut self, empty_fails: bool) -> Self {
        self.empty_fails = empty_fails;
        self
    }

    /// Number of distinct expressions parsed so far (cache size).
    pub fn cached_expressions(&self) -> usize {
        self.asts.read().expect("ast cache lock").len()
    }

    /// Number of resources parsed into the typed model so far: one per
    /// resource whose invariants reached the FHIRPath engine, none for a
    /// resource without such invariants.
    pub fn typed_parses(&self) -> usize {
        self.parses.load(Ordering::Relaxed)
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
        self.evaluate(resource, version, constraints, None)
    }

    fn evaluate_all_with_members(
        &self,
        resource: &Value,
        version: FhirVersion,
        constraints: &[DeferredConstraint<'_>],
        members: &Arc<MemberOfAnswers>,
    ) -> Vec<ConstraintOutcome> {
        self.evaluate(resource, version, constraints, Some(members))
    }

    fn session<'a>(
        &'a self,
        resource: &'a Value,
        version: FhirVersion,
    ) -> Box<dyn ConstraintSession + 'a> {
        Box::new(TypedSession::new(self, resource, version))
    }
}

/// One resource's evaluations: the typed parse (and its conversion into the
/// evaluator's value model) is done once, on the first evaluation that has
/// constraints, and shared by the later ones. Before, the walk's
/// slice-candidate checks and each pass over the deferred constraints
/// deserialized the resource again.
struct TypedSession<'a> {
    evaluator: &'a FhirPathConstraintEvaluator,
    resource: &'a Value,
    version: FhirVersion,
    /// The context every evaluation starts from (no memo, no `memberOf()`
    /// answers), or why the resource does not parse.
    base: OnceLock<Result<EvaluationContext, String>>,
    /// The resource's JSON as the `conformsTo()` resolver holds it.
    raw: OnceLock<Arc<Value>>,
}

impl<'a> TypedSession<'a> {
    fn new(
        evaluator: &'a FhirPathConstraintEvaluator,
        resource: &'a Value,
        version: FhirVersion,
    ) -> Self {
        Self {
            evaluator,
            resource,
            version,
            base: OnceLock::new(),
            raw: OnceLock::new(),
        }
    }
}

impl ConstraintSession for TypedSession<'_> {
    fn evaluate(
        &self,
        constraints: &[DeferredConstraint<'_>],
        members: Option<&Arc<MemberOfAnswers>>,
    ) -> Vec<ConstraintOutcome> {
        if constraints.is_empty() {
            return Vec::new();
        }
        let base = self.base.get_or_init(|| {
            self.evaluator.parses.fetch_add(1, Ordering::Relaxed);
            parse_typed(self.resource, self.version)
                .map(|typed| EvaluationContext::new(vec![typed]))
        });
        let base = match base {
            Ok(base) => base,
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
        // A clone shares the parsed resource and its converted root (`Arc`s);
        // the terminology cache and call budget stay per evaluation.
        let mut context = base.clone();
        context.set_terminology_session(Arc::default());
        if let Some(m) = members {
            let raw = self.raw.get_or_init(|| Arc::new(self.resource.clone()));
            context.member_of = Some(Arc::new(Members(Arc::clone(m), Arc::clone(raw))));
        }
        self.evaluator
            .evaluate_in(self.resource, context, constraints)
    }
}

/// `memberOf()` and `conformsTo()` answered from [`MemberOfAnswers`]; the
/// resource's JSON locates a `conformsTo()` focus.
struct Members(Arc<MemberOfAnswers>, Arc<Value>);

impl helios_fhirpath::MemberOfResolver for Members {
    fn member_of(&self, coded: &EvaluationResult, value_set: &str) -> Option<bool> {
        self.0.lookup(value_set, coded_value(coded)?)
    }

    fn conforms_to(&self, focus: &EvaluationResult, profile: &str) -> Option<bool> {
        let json = resource_json(&self.1, focus)?;
        self.0.conforms(json, profile)
    }
}

/// The JSON of a resource focus: the resource with that type and id in
/// `raw` (the root itself when it matches and has no id).
fn resource_json<'v>(raw: &'v Value, focus: &EvaluationResult) -> Option<&'v Value> {
    let EvaluationResult::Object { map, type_info } = focus else {
        return None;
    };
    let text = |e: &EvaluationResult| match e {
        EvaluationResult::String(s, _, _) => Some(s.clone()),
        _ => None,
    };
    let rt = map
        .get("resourceType")
        .and_then(text)
        .or_else(|| type_info.as_ref().map(|t| t.name.clone()))?;
    let id = map.get("id").and_then(text);
    fn find<'v>(v: &'v Value, rt: &str, id: Option<&str>) -> Option<&'v Value> {
        match v {
            Value::Object(o) => {
                if o.get("resourceType").and_then(Value::as_str) == Some(rt)
                    && o.get("id").and_then(Value::as_str) == id
                {
                    return Some(v);
                }
                o.values().find_map(|c| find(c, rt, id))
            }
            Value::Array(a) => a.iter().find_map(|c| find(c, rt, id)),
            _ => None,
        }
    }
    find(raw, &rt, id.as_deref())
}

/// A FHIRPath code, Coding or CodeableConcept as a [`CodedValue`].
fn coded_value(item: &EvaluationResult) -> Option<CodedValue> {
    fn text(e: &EvaluationResult) -> Option<String> {
        match e {
            EvaluationResult::String(s, _, _) => Some(s.clone()),
            EvaluationResult::Collection { items, .. } if items.len() == 1 => text(&items[0]),
            _ => None,
        }
    }
    fn coding(map: &HashMap<String, EvaluationResult>) -> Value {
        let mut out = serde_json::Map::new();
        for key in ["system", "version", "code", "display"] {
            if let Some(t) = map.get(key).and_then(text) {
                out.insert(key.to_string(), Value::String(t));
            }
        }
        Value::Object(out)
    }
    match item {
        EvaluationResult::Collection { items, .. } if items.len() == 1 => coded_value(&items[0]),
        EvaluationResult::String(s, _, _) => Some(CodedValue::Code(s.clone())),
        EvaluationResult::Object { map, .. } => {
            if let Some(codings) = map.get("coding") {
                let list: Vec<Value> = match codings {
                    EvaluationResult::Collection { items, .. } => items
                        .iter()
                        .filter_map(|i| match i {
                            EvaluationResult::Object { map, .. } => Some(coding(map)),
                            _ => None,
                        })
                        .collect(),
                    EvaluationResult::Object { map, .. } => vec![coding(map)],
                    _ => Vec::new(),
                };
                let mut cc = serde_json::Map::new();
                cc.insert("coding".into(), Value::Array(list));
                if let Some(t) = map.get("text").and_then(text) {
                    cc.insert("text".into(), Value::String(t));
                }
                Some(CodedValue::CodeableConcept(Value::Object(cc)))
            } else if map.contains_key("code") {
                Some(CodedValue::Coding(coding(map)))
            } else if map.contains_key("text") {
                Some(CodedValue::CodeableConcept(
                    serde_json::json!({"text": map.get("text").and_then(text)}),
                ))
            } else {
                None
            }
        }
        _ => None,
    }
}

impl FhirPathConstraintEvaluator {
    fn evaluate(
        &self,
        resource: &Value,
        version: FhirVersion,
        constraints: &[DeferredConstraint<'_>],
        members: Option<&Arc<MemberOfAnswers>>,
    ) -> Vec<ConstraintOutcome> {
        TypedSession::new(self, resource, version).evaluate(constraints, members)
    }

    /// Evaluate `constraints` against `context` (one resource's, see
    /// [`TypedSession`]); the root memo lives for this call only.
    fn evaluate_in(
        &self,
        resource: &Value,
        mut context: EvaluationContext,
        constraints: &[DeferredConstraint<'_>],
    ) -> Vec<ConstraintOutcome> {
        // Sound: every AST evaluated here is held by `self.asts` for the
        // life of the evaluator, which outlives this context.
        context.enable_root_memo();
        let mut nodes = NodeCache::default();

        constraints
            .iter()
            .map(|c| {
                let expression = effective_expression(c.id, c.expression);
                match self.ast(&expression) {
                    Ok(ast) => evaluate_at(
                        &ast,
                        &Scope {
                            path: c.path,
                            own_resource: c.own_resource,
                            expression: &expression,
                            raw: resource,
                            empty_fails: self.empty_fails,
                        },
                        &context,
                        &mut nodes,
                    ),
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
    // A primitive's `_name` sidecar (its id and extensions) is the primitive
    // itself in FHIRPath: `_display.extension` is `display.extension`.
    // Navigating `_display` as a member found nothing, so every invariant
    // below a sidecar (rendering-xhtml's xhtml-ext-1, ext-1 on a primitive
    // extension) silently passed.
    let segment = segment.strip_prefix('_').unwrap_or(segment);
    let member = Expression::Term(Term::Invocation(Invocation::Member(segment.to_string())));
    helios_fhirpath::evaluator::evaluate(&member, context, Some(base)).map_err(|e| e.to_string())
}

/// Evaluate one invariant AST with the node at the dotted validator `path`
/// as focus — the same semantics as the combined expression
/// `<relative path>.all((<expr>).allTrue())` (see the module docs), without
/// parsing a path-specific expression or re-navigating from the root.
/// Where one invariant runs: its node, and what decides the environment
/// variables there (see the module docs).
struct Scope<'a> {
    path: &'a str,
    own_resource: bool,
    expression: &'a str,
    raw: &'a Value,
    empty_fails: bool,
}

/// `context` with `%resource` / `%rootResource` bound for a node below the
/// root, or `None` when both are the root resource (the evaluator's default).
fn scoped_context(
    scope: &Scope<'_>,
    relative: &str,
    context: &EvaluationContext,
    nodes: &mut NodeCache,
) -> Result<Option<EvaluationContext>, String> {
    let segments: Vec<&str> = relative.split('.').collect();
    // Resources along the path: (number of leading segments, contained?).
    let mut resources: Vec<(usize, bool)> = vec![(0, false)];
    let mut current = scope.raw;
    let mut last_key = "";
    for (i, segment) in segments.iter().enumerate() {
        let next = match current {
            Value::Object(map) => map.get(*segment),
            Value::Array(items) => segment.parse::<usize>().ok().and_then(|n| items.get(n)),
            _ => None,
        };
        let Some(next) = next else { break };
        current = next;
        if !segment.chars().all(|c| c.is_ascii_digit()) {
            last_key = segment;
        }
        if current
            .get("resourceType")
            .and_then(Value::as_str)
            .is_some()
        {
            resources.push((i + 1, last_key == "contained"));
        }
    }
    let focus_is_resource = resources
        .last()
        .is_some_and(|(len, _)| *len == segments.len());
    let resource_at = if focus_is_resource && !scope.own_resource {
        resources.len() - 2
    } else {
        resources.len() - 1
    };
    let root_at = if resources[resource_at].1 && resource_at > 0 {
        resource_at - 1
    } else {
        resource_at
    };
    if resource_at == 0 {
        return Ok(None);
    }
    let mut node = |len: usize| -> Result<EvaluationResult, String> {
        if len == 0 {
            return Ok(context
                .this
                .as_deref()
                .cloned()
                .unwrap_or(EvaluationResult::Empty));
        }
        Ok(nodes.focus(&segments[..len].join("."), context)?.clone())
    };
    let resource = node(resources[resource_at].0)?;
    let root = node(resources[root_at].0)?;
    let mut scoped = context.clone();
    // The memo caches `%resource`-rooted chains for the root resource.
    scoped.root_memo = None;
    scoped.set_variable_result("resource", resource);
    scoped.set_variable_result("rootResource", root);
    Ok(Some(scoped))
}

fn evaluate_at(
    ast: &Expression,
    scope: &Scope<'_>,
    context: &EvaluationContext,
    nodes: &mut NodeCache,
) -> ConstraintOutcome {
    let path = scope.path;
    let relative = path.split_once('.').map(|(_, rest)| rest).unwrap_or("");
    if relative.is_empty() {
        return match helios_fhirpath::evaluator::evaluate(ast, context, None) {
            Ok(result) => coerce(result, scope.empty_fails),
            Err(e) => ConstraintOutcome::NotEvaluable(format!("evaluation error: {e}")),
        };
    }
    let scoped =
        if scope.expression.contains("%resource") || scope.expression.contains("%rootResource") {
            match scoped_context(scope, relative, context, nodes) {
                Ok(scoped) => scoped,
                Err(e) => return ConstraintOutcome::NotEvaluable(format!("evaluation error: {e}")),
            }
        } else {
            None
        };
    let focus = match nodes.focus(relative, context) {
        Ok(focus) => focus,
        Err(e) => return ConstraintOutcome::NotEvaluable(format!("evaluation error: {e}")),
    };
    let items: &[EvaluationResult] = match focus {
        EvaluationResult::Collection { items, .. } => items,
        EvaluationResult::Empty => &[],
        single => std::slice::from_ref(single),
    };
    let base_context = scoped.as_ref().unwrap_or(context);
    let binds_context = scope.expression.contains("%context");
    for item in items {
        let item_context;
        let context = if binds_context {
            let mut c = base_context.clone();
            c.set_variable_result("context", item.clone());
            item_context = c;
            &item_context
        } else {
            base_context
        };
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
        if values.is_empty() && scope.empty_fails {
            return ConstraintOutcome::Failed;
        }
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
fn coerce(result: EvaluationResult, empty_fails: bool) -> ConstraintOutcome {
    let empty = if empty_fails {
        ConstraintOutcome::Failed
    } else {
        ConstraintOutcome::Passed
    };
    match result {
        EvaluationResult::Empty => empty,
        EvaluationResult::Boolean(b, ..) => {
            if b {
                ConstraintOutcome::Passed
            } else {
                ConstraintOutcome::Failed
            }
        }
        EvaluationResult::Collection { items, .. } => match items.as_slice() {
            [] => empty,
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
mod session_tests {
    use super::FhirPathConstraintEvaluator;
    use crate::effects::{ConstraintEvaluator, ConstraintOutcome, DeferredConstraint};
    use helios_fhir::FhirVersion;
    use serde_json::json;

    fn constraint<'a>(path: &'a str, expression: &'a str) -> DeferredConstraint<'a> {
        DeferredConstraint {
            path,
            id: "t-1",
            expression,
            own_resource: false,
        }
    }

    /// Every evaluation of one session shares one typed parse; a session
    /// that evaluates nothing parses nothing.
    #[test]
    fn a_session_parses_the_resource_once_and_only_when_needed() {
        let evaluator = FhirPathConstraintEvaluator::new();
        let patient = json!({
            "resourceType": "Patient",
            "contact": [{ "name": { "family": "Chalmers" } }, { "gender": "female" }]
        });

        let idle = evaluator.session(&patient, FhirVersion::R4);
        assert!(idle.evaluate(&[], None).is_empty());
        drop(idle);
        assert_eq!(evaluator.typed_parses(), 0, "nothing to evaluate: no parse");

        let session = evaluator.session(&patient, FhirVersion::R4);
        let first = session.evaluate(&[constraint("Patient", "contact.exists()")], None);
        let second = session.evaluate(
            &[
                constraint("Patient.contact.0", "name.exists() or telecom.exists()"),
                constraint("Patient.contact.1", "name.exists() or telecom.exists()"),
            ],
            None,
        );
        assert_eq!(first, vec![ConstraintOutcome::Passed]);
        assert_eq!(
            second,
            vec![ConstraintOutcome::Passed, ConstraintOutcome::Failed]
        );
        assert_eq!(evaluator.typed_parses(), 1, "one parse per session");

        // The per-call entry points give the same answers, one parse each.
        let again = evaluator.evaluate_all(
            &patient,
            FhirVersion::R4,
            &[constraint(
                "Patient.contact.1",
                "name.exists() or telecom.exists()",
            )],
        );
        assert_eq!(again, vec![ConstraintOutcome::Failed]);
        assert_eq!(evaluator.typed_parses(), 2);
    }

    /// A resource the typed model rejects: every constraint is not
    /// evaluable, and the failed parse is not retried within the session.
    #[test]
    fn a_failed_parse_is_remembered_for_the_session() {
        let evaluator = FhirPathConstraintEvaluator::new();
        let broken = json!({ "resourceType": "Patient", "active": "not-a-boolean" });
        let session = evaluator.session(&broken, FhirVersion::R4);
        for _ in 0..2 {
            let out = session.evaluate(&[constraint("Patient", "active.exists()")], None);
            assert!(matches!(
                out.as_slice(),
                [ConstraintOutcome::NotEvaluable(_)]
            ));
        }
        assert_eq!(evaluator.typed_parses(), 1);
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
                        Ok(result) => super::coerce(result, false),
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
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fhir/tests/data/json/R4");
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
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            let Ok(resource) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if resource.get("resourceType").is_none() {
                continue;
            }
            let outcome = validator.validate_sync(&resource, &crate::ValidationOptions::default());
            let constraints: Vec<DeferredConstraint<'_>> = outcome
                .deferred
                .iter()
                .filter_map(|d| match d {
                    Deferred::Constraint {
                        path,
                        id,
                        expression,
                        own_resource,
                        ..
                    } => Some(DeferredConstraint {
                        path,
                        id,
                        expression,
                        own_resource: *own_resource,
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
            diffs
                .iter()
                .take(40)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
        // The cache is keyed by expression, not by node path.
        assert!(
            evaluator.cached_expressions() < 2_000,
            "{}",
            evaluator.cached_expressions()
        );
        eprintln!(
            "{resources} resources, {checked} constraint outcomes identical ({failed} failures)"
        );
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
                Deferred::Constraint {
                    path,
                    id,
                    expression,
                    own_resource,
                    ..
                } => Some(DeferredConstraint {
                    path,
                    id,
                    expression,
                    own_resource: *own_resource,
                }),
                _ => None,
            })
            .collect();
        let evaluator = super::FhirPathConstraintEvaluator::new();
        let started = std::time::Instant::now();
        evaluator.evaluate_all(&resource, FhirVersion::R4, &constraints);
        eprintln!(
            "{} constraints, all at once: {:?}",
            constraints.len(),
            started.elapsed()
        );
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
