//! The cooperative validation walk.
//!
//! Port of the FHIR Schema validation algorithm (`docs/algorythm.md` and the
//! reference validator `fhirschema-js/src/index.js`), with the divergences
//! called out inline. The behavioral contract is the vendored conformance
//! suite; where the reference validator and this port differ, no fixture
//! distinguishes them and the difference is documented.
//!
//! # Deterministic error-emission order
//!
//! 1. Schema sets iterate in insertion order, ancestors first: a schema's
//!    `base` chain is added before the schema itself (pinned by fixture
//!    `4_required.json`: the base's `required` errors precede the profile's).
//! 2. Within one schema, keyword validators run in the canonical order
//!    `fixed, pattern, constraints, required, excluded, binding, elements,
//!    choices` (the reference validator follows JSON key declaration order;
//!    no fixture distinguishes).
//! 3. Data keys are walked in document order (`serde_json`'s
//!    `preserve_order`), array items in index order.
//! 4. Node-level keyword errors precede per-key errors; choice-conflict
//!    errors (`postValidate`) come last for each node.
//!
//! # Known divergences from the reference validator
//!
//! - Unresolvable schema references produce an `unknown-schema` error and
//!   validation continues; the reference validator throws.
//! - `excluded` is enforced (the reference implementation crashes on it).
//! - Dynamic `Resource` schemas are resolved per array item; the reference
//!   validator leaks them into the shared element set across siblings.
//! - A choice branch declared by several layers is counted once per node in
//!   the multiple-choices-present check (the reference validator counts it
//!   once per layer, which would false-positive on real profiles).
//! - The choice-group *declarer* schema joins a branch element's set under
//!   the key `{layer}.{group}` (the reference validator uses `{layer}.{key}`,
//!   which silently overwrites the branch schema and loses its constraints).

use super::errors::{self, ErrorKind, Severity, ValidationError};
use super::path::PathTracker;
use super::{SyncOutcome, UnknownProfilePolicy, ValidationOptions};
use crate::effects::Deferred;
use crate::resolver::SchemaResolver;
use crate::schema::FhirSchema;
use indexmap::IndexMap;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

/// The special element type resolved dynamically from `data.resourceType`.
const RESOURCE_TYPE_NAME: &str = "Resource";

/// A cooperative schema set (the *schemata*): insertion-ordered, ancestors
/// first, deduplicated both by resolution key and by schema identity.
#[derive(Clone, Default)]
pub(super) struct SchemaSet {
    map: IndexMap<String, Arc<FhirSchema>>,
    /// Identity dedup (`Arc` pointer): the same schema reachable under
    /// several aliases (name and canonical URL) is added once.
    seen: HashSet<usize>,
}

impl SchemaSet {
    fn new() -> Self {
        Self::default()
    }

    fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn contains_key(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    fn iter(&self) -> impl Iterator<Item = (&str, &Arc<FhirSchema>)> {
        self.map.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub(super) fn schemas(&self) -> impl Iterator<Item = &Arc<FhirSchema>> {
        self.map.values()
    }
}

pub(super) struct WalkCtx<'a> {
    resolver: &'a dyn SchemaResolver,
    errors: Vec<ValidationError>,
    deferred: Vec<Deferred>,
    pub(super) path: PathTracker,
    /// Opt-in extension-context warnings (#615).
    check_extension_context: bool,
    /// Layer `meta.profile` claims, at the root and on nested resources.
    use_meta_profiles: bool,
    /// What an unresolvable `meta.profile` claim reports.
    unknown_profile: UnknownProfilePolicy,
    /// Issues already emitted, by `(kind, severity, path, message)`. A
    /// profile compiled from its snapshot repeats every keyword it inherits
    /// (`required`, `min`, slice cardinalities, ...), and each layer of the
    /// cooperative set judges the node on its own — so a fact asserted by
    /// the base and restated by two profiles would otherwise be reported
    /// three times, identically.
    emitted: HashSet<(ErrorKind, Severity, String, String)>,
    /// Binding obligations already deferred, by `(path, valueSet,
    /// strength)`: the same inherited binding from several layers is checked
    /// (and reported) once. Constraints are deduplicated by `(path, id)` in
    /// the effects pass; bindings carry no id, so they are folded here.
    deferred_bindings: HashSet<(String, String, Option<String>)>,
    /// `(id, resourceType)` of the contained resources of each resource
    /// being walked, innermost last (for `#id` reference targets).
    contained: Vec<Vec<(String, String)>>,
}

impl WalkCtx<'_> {
    pub(super) fn resolver(&self) -> &dyn SchemaResolver {
        self.resolver
    }

    /// The type of the contained resource `id` of the innermost resource.
    pub(super) fn contained_type(&self, id: &str) -> Option<String> {
        self.contained
            .last()?
            .iter()
            .find(|(cid, _)| cid == id)
            .map(|(_, rt)| rt.clone())
    }

    pub(super) fn error(&mut self, kind: ErrorKind, message: String) {
        self.error_with_severity(kind, message, Severity::Error);
    }

    fn error_with_severity(&mut self, kind: ErrorKind, message: String, severity: Severity) {
        let path = self.path.render_dotted();
        if self
            .emitted
            .insert((kind, severity, path.clone(), message.clone()))
        {
            self.errors
                .push(ValidationError::new(kind, path, message).with_severity(severity));
        }
    }
}

pub(super) fn validate(
    resolver: &dyn SchemaResolver,
    resource: &Value,
    opts: &ValidationOptions,
) -> SyncOutcome {
    let resource_type = resource
        .get("resourceType")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let mut ctx = WalkCtx {
        resolver,
        errors: Vec::new(),
        deferred: Vec::new(),
        path: PathTracker::new(resource_type),
        check_extension_context: opts.check_extension_context,
        use_meta_profiles: opts.use_meta_profiles,
        unknown_profile: opts.unknown_profile,
        emitted: HashSet::new(),
        deferred_bindings: HashSet::new(),
        contained: Vec::new(),
    };

    // Root schema-set: resourceType, then meta.profile claims, then
    // caller-supplied profiles (the draft-algorithm order).
    let mut set = SchemaSet::new();
    if !resource_type.is_empty() {
        match resolver.resolve(resource_type) {
            Some(schema) => add_schemas_to_set(&mut ctx, &mut set, schema, resource_type),
            None => ctx.error(
                ErrorKind::UnknownSchema,
                errors::msg_unknown_schema(resource_type),
            ),
        }
    }
    add_meta_profiles(&mut ctx, &mut set, resource);
    for profile in &opts.profiles {
        add_profile(&mut ctx, &mut set, profile, opts.unknown_profile, true);
    }

    validate_node(&mut ctx, &set, resource);

    SyncOutcome {
        errors: ctx.errors,
        deferred: ctx.deferred,
    }
}

/// Layer the profiles `resource` claims in `meta.profile` onto `set` (when
/// enabled). An unresolvable claim is located at the claim itself
/// (`meta.profile[i]`), as the HL7 validator reports it.
fn add_meta_profiles(ctx: &mut WalkCtx<'_>, set: &mut SchemaSet, resource: &Value) {
    if !ctx.use_meta_profiles {
        return;
    }
    let policy = ctx.unknown_profile;
    for (index, profile) in meta_profiles(resource).iter().enumerate() {
        ctx.path.push_key("meta");
        ctx.path.push_key("profile");
        ctx.path.push_index(index);
        add_profile(ctx, set, profile, policy, false);
        ctx.path.pop();
        ctx.path.pop();
        ctx.path.pop();
    }
}

/// `meta.profile` entries, in document order.
fn meta_profiles(resource: &Value) -> Vec<String> {
    resource
        .get("meta")
        .and_then(|m| m.get("profile"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn add_profile(
    ctx: &mut WalkCtx<'_>,
    set: &mut SchemaSet,
    profile: &str,
    policy: UnknownProfilePolicy,
    requested: bool,
) {
    if let Some(schema) = ctx.resolver.resolve(profile) {
        add_schemas_to_set(ctx, set, schema, profile);
        return;
    }
    // A profile the caller asked to validate against (`$validate?profile=`)
    // that cannot be found means the requested validation did not happen:
    // always an error, whatever the policy for `meta.profile` claims.
    if requested {
        ctx.error(
            ErrorKind::UnknownProfile,
            errors::msg_unknown_requested_profile(profile),
        );
        return;
    }
    match policy {
        UnknownProfilePolicy::Warn => ctx.error_with_severity(
            ErrorKind::UnknownProfile,
            errors::msg_unknown_profile(profile),
            Severity::Warning,
        ),
        UnknownProfilePolicy::Error => ctx.error(
            ErrorKind::UnknownProfile,
            errors::msg_unknown_profile(profile),
        ),
        UnknownProfilePolicy::Ignore => {}
    }
}

/// Pull `schema` into `set` together with everything it references:
/// its `base` chain first (ancestors-first ordering), then its complex
/// `type` (skipped for primitive leaves), then any `elementReference`
/// target, then the schema itself.
pub(super) fn add_schemas_to_set(
    ctx: &mut WalkCtx<'_>,
    set: &mut SchemaSet,
    schema: Arc<FhirSchema>,
    key: &str,
) {
    let identity = Arc::as_ptr(&schema) as usize;
    if !set.seen.insert(identity) {
        return;
    }

    if let Some(base) = schema.base.clone()
        && !set.contains_key(&base)
    {
        match ctx.resolver.resolve(&base) {
            Some(base_schema) => add_schemas_to_set(ctx, set, base_schema, &base),
            None => ctx.error(ErrorKind::UnknownSchema, errors::msg_unknown_schema(&base)),
        }
    }

    if let Some(type_ref) = schema.type_.clone()
        && !schema.is_primitive()
        && !set.contains_key(&type_ref)
    {
        match ctx.resolver.resolve(&type_ref) {
            Some(type_schema) => add_schemas_to_set(ctx, set, type_schema, &type_ref),
            None => ctx.error(
                ErrorKind::UnknownSchema,
                errors::msg_unknown_schema(&type_ref),
            ),
        }
    }

    // `elementReference` pulls another element's schema into the set —
    // the mechanism behind recursive structures like Questionnaire.item.
    // Cycles terminate via the identity dedup above.
    if let Some(segments) = schema.element_reference.clone() {
        let ref_key = segments.join(".");
        match resolve_element_reference(ctx.resolver, &segments) {
            Some(target) => {
                let stripped = Arc::new(target.without_cardinality());
                add_schemas_to_set(ctx, set, stripped, &ref_key);
            }
            None => ctx.error(
                ErrorKind::UnknownSchema,
                errors::msg_unknown_schema(&ref_key),
            ),
        }
    }

    set.map.insert(key.to_string(), schema);
}

/// Navigate an element reference: `["Questionnaire", "elements", "item"]` —
/// a root schema name followed by alternating `"elements"` / element-name
/// segments.
fn resolve_element_reference(
    resolver: &dyn SchemaResolver,
    segments: &[String],
) -> Option<Arc<FhirSchema>> {
    let (root, rest) = segments.split_first()?;
    let mut current = resolver.resolve(root)?;
    let mut iter = rest.iter();
    while let Some(marker) = iter.next() {
        if marker != "elements" {
            return None;
        }
        let name = iter.next()?;
        let next = current.elements.as_ref()?.get(name)?.clone();
        current = next;
    }
    Some(current)
}

/// Validate one data node against a cooperative schema set.
pub(super) fn validate_node(ctx: &mut WalkCtx<'_>, set: &SchemaSet, data: &Value) {
    // Dynamic `Resource` resolution: an element typed `Resource` takes its
    // concrete schema from the nested `resourceType` at validation time
    // (Bundle.entry.resource, contained, Parameters.parameter.resource).
    let mut extended;
    let set = if data.is_object()
        && set
            .schemas()
            .any(|s| s.type_.as_deref() == Some(RESOURCE_TYPE_NAME))
    {
        extended = set.clone();
        if let Some(rt) = data.get("resourceType").and_then(Value::as_str) {
            match ctx.resolver.resolve(rt) {
                Some(schema) => add_schemas_to_set(ctx, &mut extended, schema, rt),
                None => ctx.error(ErrorKind::UnknownSchema, errors::msg_unknown_schema(rt)),
            }
            // A nested resource (Bundle entry, contained, Parameters part)
            // is held to the profiles it claims, as the root resource is: a
            // TW Core Patient POSTed inside a transaction must meet TW Core.
            add_meta_profiles(ctx, &mut extended, data);
        }
        // No resourceType on a Resource-typed node: nothing to resolve; the
        // static layers (e.g. the base `Resource` schema) still apply.
        &extended
    } else {
        set
    };

    eval_validators(ctx, set, data);
    super::references::check_reference(ctx, set, data);

    let Some(obj) = data.as_object() else {
        return;
    };

    // FHIR JSON has no empty objects: an element is present only with a
    // value or children (ele-1). The typed FHIRPath model drops `{}` before
    // ele-1 can see it, so the walk reports it.
    if obj.is_empty() {
        ctx.error(ErrorKind::EmptyObject, errors::msg_empty_object());
        return;
    }

    let is_resource = obj.get("resourceType").is_some_and(Value::is_string);
    if is_resource {
        ctx.contained.push(contained_types(obj));
        check_resource_id(ctx, set, obj);
    }

    // Choice branches seen on this node, keyed by choice group, in first-
    // encounter order.
    let mut multi_choice: IndexMap<String, Vec<String>> = IndexMap::new();

    for (key, value) in obj {
        ctx.path.push_key(key);
        // `_foo` sidecars carry the Element part (id/extension) of a
        // primitive `foo` — they pair with the base element rather than
        // resolving as elements themselves.
        if let Some(base_key) = key.strip_prefix('_')
            && !base_key.is_empty()
        {
            handle_sidecar(ctx, set, key, base_key, value);
        } else {
            eval_element(ctx, set, &mut multi_choice, key, value);
        }
        ctx.path.pop();
    }

    // postValidate: more than one branch of the same choice group present.
    for (group, branches) in &multi_choice {
        if branches.len() > 1 {
            ctx.path.push_key(group);
            ctx.error(ErrorKind::Choice, errors::msg_choice(group, branches));
            ctx.path.pop();
        }
    }

    if is_resource {
        ctx.contained.pop();
    }
}

/// `Resource.id` is an `id` (`[A-Za-z0-9\-\.]{1,64}`), but R4 declares it
/// as `System.String` with a `string` fhir-type, so the packs type it
/// `string` and its pattern never ran. Checked here for every resource
/// (root, contained, Bundle entries), as the HL7 validator does. Versions
/// whose packs already type it `id` (R4B on) check it through the `id`
/// pattern and are skipped here.
fn check_resource_id(ctx: &mut WalkCtx<'_>, set: &SchemaSet, obj: &serde_json::Map<String, Value>) {
    let Some(id) = obj.get("id").and_then(Value::as_str) else {
        return;
    };
    let typed_id = set.schemas().any(|s| {
        s.elements
            .as_ref()
            .and_then(|els| els.get("id"))
            .is_some_and(|el| el.type_.as_deref() == Some("id"))
    });
    if typed_id {
        return;
    }
    let valid = (1..=64).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    if !valid {
        ctx.path.push_key("id");
        ctx.error(ErrorKind::PrimitiveValue, errors::msg_resource_id(id));
        ctx.path.pop();
    }
}

/// `(id, resourceType)` of a resource's `contained` entries.
fn contained_types(resource: &serde_json::Map<String, Value>) -> Vec<(String, String)> {
    resource
        .get("contained")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|c| {
                    Some((
                        c.get("id")?.as_str()?.to_string(),
                        c.get("resourceType")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Run every keyword validator of every schema in the set against the node.
fn eval_validators(ctx: &mut WalkCtx<'_>, set: &SchemaSet, data: &Value) {
    // Clone the Arcs up front: keyword validators need `&mut ctx` while the
    // set is borrowed, and Arc clones are cheap.
    let schemas: Vec<Arc<FhirSchema>> = set.schemas().cloned().collect();
    let primitive_set = schemas.iter().any(|s| s.is_primitive());
    for schema in &schemas {
        if schema.is_primitive() {
            super::primitives::validate_primitive(ctx, schema, data);
        }
        if let Some(fixed) = &schema.fixed
            && data != fixed
        {
            ctx.error(ErrorKind::FixedValue, errors::msg_fixed_value(fixed, data));
        }
        if let Some(pattern) = &schema.pattern
            && !is_partial_match(data, pattern)
        {
            ctx.error(
                ErrorKind::PatternValue,
                errors::msg_pattern_value(pattern, data),
            );
        }
        if let Some(constraints) = &schema.constraints {
            let path = ctx.path.render_dotted();
            for (id, c) in constraints {
                ctx.deferred.push(Deferred::Constraint {
                    path: path.clone(),
                    id: id.clone(),
                    expression: c.expression.clone(),
                    human: c.human.clone(),
                    severity: c.severity.clone(),
                });
            }
        }
        if let Some(required) = &schema.required {
            validate_required(ctx, set, required, data);
        }
        if let Some(excluded) = &schema.excluded {
            validate_excluded(ctx, excluded, data);
        }
        if let Some(binding) = &schema.binding {
            let path = ctx.path.render_dotted();
            if ctx.deferred_bindings.insert((
                path.clone(),
                binding.value_set.clone(),
                binding.strength.clone(),
            )) {
                ctx.deferred.push(Deferred::Binding {
                    path,
                    binding: binding.clone(),
                    value: data.clone(),
                    type_hint: schema.type_.clone(),
                });
            }
        }
        // `elements` on a primitive element (a profile constraining
        // `code.extension`, like TW Core's identifier-suffix on
        // `identifier.type.coding.code`) describe its `_code` sidecar, not
        // the JSON value: a string there is not a wrong container.
        if schema.elements.is_some() && !data.is_object() && !primitive_set {
            ctx.error(
                ErrorKind::Type,
                errors::msg_expected_object_comma(errors::json_type_name(data)),
            );
        }
        if let Some(choices) = &schema.choices {
            // Runs when a choice-group declarer joins a branch element's
            // set; the branch key is the last named path segment.
            if let Some(branch) = ctx.path.last_key()
                && !choices.iter().any(|c| c == branch)
            {
                let message = errors::msg_choice_excluded(branch);
                ctx.error(ErrorKind::ChoiceExcluded, message);
            }
        }
    }
}

fn validate_required(ctx: &mut WalkCtx<'_>, set: &SchemaSet, required: &[String], data: &Value) {
    let Some(obj) = data.as_object() else {
        ctx.error(
            ErrorKind::Type,
            errors::msg_expected_object_no_comma(errors::json_type_name(data)),
        );
        return;
    };
    for key in required {
        ctx.path.push_key(key);
        if !has_key(obj, key) && !choice_branch_satisfies(set, key, obj) {
            ctx.error(ErrorKind::Required, errors::msg_required(key));
        }
        ctx.path.pop();
    }
}

/// Helios extension: a required key that is a choice group (`foo` for
/// `foo[x]`) is satisfied by any declared branch (`fooBoolean`, ...) being
/// present. The converter maps `min >= 1` on `foo[x]` to `required: ["foo"]`,
/// and the literal key never appears in data. No upstream fixture combines
/// `required` with choices, so conformance is unaffected.
fn choice_branch_satisfies(
    set: &SchemaSet,
    key: &str,
    obj: &serde_json::Map<String, Value>,
) -> bool {
    set.schemas().any(|schema| {
        schema
            .elements
            .as_ref()
            .and_then(|els| els.get(key))
            .and_then(|el| el.choices.as_ref())
            .is_some_and(|branches| branches.iter().any(|b| has_key(obj, b)))
    })
}

/// A primitive is present when its value or only its `_key` sidecar is:
/// `{"_valueString": {"extension": [...]}}` is a value[x] carried entirely
/// by extensions (data-absent-reason, translations), which FHIR allows.
fn has_key(obj: &serde_json::Map<String, Value>, key: &str) -> bool {
    obj.contains_key(key) || obj.contains_key(&format!("_{key}"))
}

fn validate_excluded(ctx: &mut WalkCtx<'_>, excluded: &[String], data: &Value) {
    let Some(obj) = data.as_object() else {
        ctx.error(
            ErrorKind::Type,
            errors::msg_expected_object_no_comma(errors::json_type_name(data)),
        );
        return;
    };
    for key in excluded {
        ctx.path.push_key(key);
        if obj.contains_key(key) {
            ctx.error(ErrorKind::Excluded, errors::msg_excluded(key));
        }
        ctx.path.pop();
    }
}

/// Validate one data key against the element schemas collected from every
/// layer of the set.
fn eval_element(
    ctx: &mut WalkCtx<'_>,
    set: &SchemaSet,
    multi_choice: &mut IndexMap<String, Vec<String>>,
    key: &str,
    value: &Value,
) {
    let mut elset = SchemaSet::new();
    collect_schemas_for_element(ctx, set, &mut elset, multi_choice, key);

    if elset.is_empty() {
        ctx.error(ErrorKind::UnknownElement, errors::msg_unknown_element(key));
        return;
    }

    // Array-ness is cooperative: an element is an array if ANY layer says so.
    let expect_array = elset.schemas().any(|s| s.array == Some(true));
    match (expect_array, value.is_array()) {
        (true, false) => ctx.error(ErrorKind::NotArray, errors::msg_not_array(key)),
        (false, true) => ctx.error(ErrorKind::NotSingular, errors::msg_not_singular(key)),
        _ => {}
    }
    // The walk continues after a shape mismatch, mirroring the reference
    // validator (the mismatched value still validates against the set).

    if let Some(items) = value.as_array() {
        // A known extension used outside its declared context draws a
        // warning (#615). Runs for every item, sliced or not — placement is
        // orthogonal to slicing.
        if ctx.check_extension_context && (key == "extension" || key == "modifierExtension") {
            check_extension_context(ctx, items);
        }

        // Whole-collection keywords, per schema: min then max.
        let schemas: Vec<Arc<FhirSchema>> = elset.schemas().cloned().collect();
        for schema in &schemas {
            if let Some(min) = schema.min
                && (items.len() as u64) < min
            {
                ctx.error(ErrorKind::Min, errors::msg_min(min, items.len()));
            }
            if let Some(max) = schema.max
                && (items.len() as u64) > max
            {
                ctx.error(ErrorKind::Max, errors::msg_max(max, items.len()));
            }
        }

        // Slicing (mark/sweep). Items validated cooperatively with their
        // matched slice schemas are consumed — the plain per-item pass below
        // skips them so base-set errors are not emitted twice.
        let consumed = super::slicing::validate_slices(ctx, &elset, items);
        let is_extension_array = key == "extension" || key == "modifierExtension";

        for (index, item) in items.iter().enumerate() {
            if consumed.contains(&index) {
                continue;
            }
            ctx.path.push_index(index);
            if is_extension_array {
                validate_extension_item(ctx, &elset, item, key == "modifierExtension");
            } else {
                validate_node(ctx, &elset, item);
            }
            ctx.path.pop();
        }
    } else {
        validate_node(ctx, &elset, value);
    }
}

/// Validate an extension no profile slice claimed. An extension whose `url`
/// resolves to a known extension definition is held to that definition
/// (value type, sub-extensions, cardinalities), as the HL7 validator does;
/// before, only extensions a profile declared were checked, so
/// `patient-birthPlace` carrying a `valueString` instead of an Address
/// passed. An unknown url stays allowed on `extension`, but a modifier
/// extension changes the meaning of its element, so one nobody can look up
/// is an error (FHIR: unknown modifier extensions SHALL NOT be ignored).
fn validate_extension_item(ctx: &mut WalkCtx<'_>, elset: &SchemaSet, item: &Value, modifier: bool) {
    let url = item
        .get("url")
        .and_then(Value::as_str)
        .filter(|u| u.contains("://") || u.starts_with("urn:"));
    let Some(url) = url else {
        validate_node(ctx, elset, item);
        return;
    };
    match ctx.resolver.resolve(url) {
        Some(definition) if definition.kind.as_deref() == Some(crate::schema::kind::EXTENSION) => {
            let mut extended = elset.clone();
            add_schemas_to_set(ctx, &mut extended, definition, url);
            validate_node(ctx, &extended, item);
        }
        Some(_) => validate_node(ctx, elset, item),
        None => {
            if modifier {
                ctx.error(
                    ErrorKind::UnknownSchema,
                    errors::msg_unknown_modifier_extension(url),
                );
            }
            validate_node(ctx, elset, item);
        }
    }
}

/// Warn when an extension item is used outside its declared context (#615).
///
/// Conservative on purpose, twice over. Only extensions at the **resource
/// root** are judged: an element-level instance path cannot be compared to a
/// declared context without mapping it back to the StructureDefinition path
/// (recursive elements — `Questionnaire.item.item` — collapse to
/// `Questionnaire.item` there, and the spec's own examples falsified the
/// naive comparison at scale). And only element-type contexts that read as
/// resource-rooted paths are judged: a context naming a datatype
/// (`HumanName`), an `Element`/`Resource`/`DomainResource` catch-all, or a
/// FHIRPath expression makes the extension unjudgeable here and passes
/// silently. Unknown urls and relative (sub-extension) urls always pass.
fn check_extension_context(ctx: &mut WalkCtx<'_>, items: &[Value]) {
    let parent = ctx.path.parent_element_path();
    // Root only: the parent path is bare `ResourceType` with no dots.
    if parent.is_empty() || parent.contains('.') {
        return;
    }

    for (index, item) in items.iter().enumerate() {
        let Some(url) = item.get("url").and_then(Value::as_str) else {
            continue;
        };
        if !url.contains("://") {
            continue;
        }
        let Some(schema) = ctx.resolver.resolve(url) else {
            continue;
        };
        let Some(contexts) = schema.context.as_ref().filter(|c| !c.is_empty()) else {
            continue;
        };

        let judgeable = |c: &String| {
            // A resource-rooted element path: dotted segments, the first of
            // which resolves to a resource schema. Everything else (datatype
            // names, catch-alls, FHIRPath) is out of this check's reach.
            !matches!(c.as_str(), "Element" | "Resource" | "DomainResource")
                && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '.')
                && c.split('.')
                    .next()
                    .and_then(|head| ctx.resolver.resolve(head))
                    .is_some_and(|s| crate::editor::is_resource(&s))
        };
        if !contexts.iter().all(judgeable) {
            continue;
        }
        // An element-type context names one exact spot: `Patient` is the
        // resource root, `Patient.name` that element — not "anywhere in
        // Patient".
        if contexts.contains(&parent) {
            continue;
        }

        ctx.path.push_index(index);
        let message = errors::msg_extension_context(url, &parent, contexts);
        ctx.error_with_severity(ErrorKind::ExtensionContext, message, Severity::Warning);
        ctx.path.pop();
    }
}

/// Validate a `_foo` primitive-extension sidecar.
///
/// The sidecar pairs with the base element `foo`: it is only legal when some
/// layer declares `foo` with a primitive type, it mirrors `foo`'s
/// array-vs-singular shape, and its content (per item for arrays, with
/// `null` gaps allowed) validates against the resolver's `Element` schema.
fn handle_sidecar(
    ctx: &mut WalkCtx<'_>,
    set: &SchemaSet,
    key: &str,
    base_key: &str,
    value: &Value,
) {
    // Collect the base element's sub-set; choice bookkeeping is discarded
    // (a sidecar is not a branch occurrence).
    let mut scratch: IndexMap<String, Vec<String>> = IndexMap::new();
    let mut base_elset = SchemaSet::new();
    collect_schemas_for_element(ctx, set, &mut base_elset, &mut scratch, base_key);

    // Stray sidecar, or sidecar on a non-primitive element: unknown.
    if base_elset.is_empty() || !base_elset.schemas().any(|s| s.is_primitive()) {
        ctx.error(ErrorKind::UnknownElement, errors::msg_unknown_element(key));
        return;
    }

    // The sidecar mirrors the base element's shape.
    let expect_array = base_elset.schemas().any(|s| s.array == Some(true));
    match (expect_array, value.is_array()) {
        (true, false) => ctx.error(ErrorKind::NotArray, errors::msg_not_array(key)),
        (false, true) => ctx.error(ErrorKind::NotSingular, errors::msg_not_singular(key)),
        _ => {}
    }

    // Validate the Element part(s). Without a resolvable `Element` schema
    // (core packs always carry one) the content is left unchecked.
    let Some(element_schema) = ctx.resolver.resolve("Element") else {
        return;
    };
    let mut element_set = SchemaSet::new();
    add_schemas_to_set(ctx, &mut element_set, element_schema, "Element");

    match value {
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                if item.is_null() {
                    continue;
                }
                ctx.path.push_index(index);
                validate_node(ctx, &element_set, item);
                ctx.path.pop();
            }
        }
        Value::Null => {}
        other => validate_node(ctx, &element_set, other),
    }
}

/// Gather the element sub-set for `key` across all layers: each layer's
/// `elements[key]` (plus its transitive `base`/`type`), and — when the key
/// is a choice branch — every layer's declarer schema for the choice group.
fn collect_schemas_for_element(
    ctx: &mut WalkCtx<'_>,
    set: &SchemaSet,
    elset: &mut SchemaSet,
    multi_choice: &mut IndexMap<String, Vec<String>>,
    key: &str,
) {
    let layers: Vec<(String, Arc<FhirSchema>)> = set
        .iter()
        .map(|(name, schema)| (name.to_string(), Arc::clone(schema)))
        .collect();

    // Every layer's `extensions` map compiles into a synthetic slicing on
    // the `extension` element, matched by `{url}` — authoring sugar rewritten
    // into the core slicing primitive at validation time.
    if key == "extension" {
        compile_extensions(ctx, &layers, elset);
    }

    let mut choice_group: Option<String> = None;
    for (layer_name, layer) in &layers {
        let Some(subschema) = layer.elements.as_ref().and_then(|els| els.get(key)) else {
            continue;
        };
        // Choice-group declarers are not element schemas for their own key:
        // a bare group key in data (e.g. `"choice": ...`) is unknown.
        if subschema.choices.is_none() {
            let subset_key = format!("{layer_name}.{key}");
            add_schemas_to_set(ctx, elset, Arc::clone(subschema), &subset_key);
        }
        if let Some(group) = &subschema.choice_of {
            choice_group = Some(group.clone());
            let entry = multi_choice.entry(group.clone()).or_default();
            // Count each branch once per node, even when several layers
            // declare it.
            if !entry.iter().any(|k| k == key) {
                entry.push(key.to_string());
            }
        }
    }

    // The key is a choice branch: every layer's declarer for the group joins
    // the branch's set, so `choices` narrowing applies cooperatively.
    if let Some(group) = choice_group {
        for (layer_name, layer) in &layers {
            if let Some(declarer) = layer.elements.as_ref().and_then(|els| els.get(&group)) {
                let subset_key = format!("{layer_name}.{group}");
                add_schemas_to_set(ctx, elset, Arc::clone(declarer), &subset_key);
            }
        }
    }
}

/// Compile `extensions` maps from every layer into one synthetic
/// slicing-bearing schema on the `extension` element. Each entry becomes a
/// slice matched by `{url: <ext.url>}` carrying the entry's `min`/`max`; its
/// slice schema is the entry (minus url/min/max) overlaid by the schema the
/// extension's url resolves to.
///
/// An unresolvable extension url is reported as `unknown-schema` and the
/// entry-only schema is used (the reference validator throws instead).
fn compile_extensions(
    ctx: &mut WalkCtx<'_>,
    layers: &[(String, Arc<FhirSchema>)],
    elset: &mut SchemaSet,
) {
    use crate::schema::{Match, Slice, Slicing};

    let mut slices: IndexMap<String, Slice> = IndexMap::new();
    for (_, layer) in layers {
        let Some(extensions) = &layer.extensions else {
            continue;
        };
        for (ext_name, entry) in extensions {
            let Some(url) = entry.url.clone() else {
                continue;
            };
            let resolved = ctx.resolver.resolve(&url);
            if resolved.is_none() {
                ctx.error(ErrorKind::UnknownSchema, errors::msg_unknown_schema(&url));
            }
            let merged = merge_extension_schema(entry, resolved.as_deref());
            slices.insert(
                ext_name.clone(),
                Slice {
                    match_: Some(Match {
                        type_: Some("pattern".to_string()),
                        value: Some(serde_json::json!({ "url": url })),
                        resolve_ref: None,
                    }),
                    min: entry.min,
                    max: entry.max,
                    order: None,
                    reslice: None,
                    slice_is_constraining: None,
                    schema: Some(Arc::new(merged)),
                },
            );
        }
    }

    if !slices.is_empty() {
        let synthetic = FhirSchema {
            slicing: Some(Slicing {
                slices,
                rules: None,
                ordered: None,
            }),
            ..Default::default()
        };
        add_schemas_to_set(ctx, elset, Arc::new(synthetic), "extension");
    }
}

/// Shallow overlay merge for extension slice schemas, mirroring the
/// reference validator's `Object.assign(entry-minus-url/min/max, resolved)`:
/// fields present on the resolved extension schema win; entry fields fill
/// the gaps.
fn merge_extension_schema(entry: &FhirSchema, resolved: Option<&FhirSchema>) -> FhirSchema {
    let mut out = entry.clone();
    out.url = None;
    out.min = None;
    out.max = None;

    let Some(r) = resolved else {
        return out;
    };

    macro_rules! overlay {
        ($($field:ident),* $(,)?) => {
            $(if r.$field.is_some() {
                out.$field = r.$field.clone();
            })*
        };
    }
    overlay!(
        url,
        name,
        base,
        kind,
        derivation,
        type_,
        array,
        scalar,
        min,
        max,
        elements,
        required,
        excluded,
        element_reference,
        choices,
        choice_of,
        fixed,
        pattern,
        binding,
        constraints,
        refers,
        slicing,
        extensions,
        modifier,
        must_support,
        summary,
        short,
        context,
        regex,
    );
    out
}

/// Partial deep match, with FHIR pattern semantics. Every key present in
/// `pattern` must exist in `data` and match recursively; extra data keys are
/// permitted; scalars match by equality.
///
/// - An array pattern matches when each of its items matches *some* data
///   item, in any order (FHIR `pattern[x]`: "each element in the pattern
///   array must be present in the instance array"; also lodash `_.isMatch`
///   for arrays). A prefix match missed a VSCat category coding that was
///   not the first one.
/// - An object pattern against an array matches when one item matches. A
///   value discriminator path runs through repeating elements: VSCat's
///   `coding.code` + `coding.system` compile to `{coding: {code, system}}`
///   and select every coding, so a category whose codings include
///   vital-signs belongs to the slice.
pub(crate) fn is_partial_match(data: &Value, pattern: &Value) -> bool {
    match pattern {
        Value::Object(pattern_map) => match data {
            Value::Object(data_map) => pattern_map
                .iter()
                .all(|(k, pv)| data_map.get(k).is_some_and(|dv| is_partial_match(dv, pv))),
            Value::Array(data_items) => data_items.iter().any(|dv| is_partial_match(dv, pattern)),
            _ => false,
        },
        Value::Array(pattern_items) => {
            let Some(data_items) = data.as_array() else {
                return false;
            };
            pattern_items
                .iter()
                .all(|pv| data_items.iter().any(|dv| is_partial_match(dv, pv)))
        }
        scalar => data == scalar,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// #615: a known extension outside its declared element context draws a
    /// warning; in context, or unknown, it stays silent.
    #[test]
    fn extension_context_mismatch_warns() {
        use crate::engine::Validator;
        let validator = Validator::new(crate::packs::core_registry(helios_fhir::FhirVersion::R4));
        let opts = crate::ValidationOptions {
            check_extension_context: true,
            ..Default::default()
        };
        let birthplace = "http://hl7.org/fhir/StructureDefinition/patient-birthPlace";

        let warnings = |resource: &serde_json::Value| {
            validator
                .validate_sync(resource, &opts)
                .errors
                .into_iter()
                .filter(|e| e.kind == ErrorKind::ExtensionContext)
                .collect::<Vec<_>>()
        };

        // In context (the Patient root): silent.
        let ok = json!({
            "resourceType": "Patient",
            "extension": [{ "url": birthplace, "valueAddress": {"city": "X"} }]
        });
        assert!(warnings(&ok).is_empty());

        // Out of context (an Observation): one warning, warning-severity.
        let wrong = json!({
            "resourceType": "Observation", "status": "final", "code": {"text": "t"},
            "extension": [{ "url": birthplace, "valueAddress": {"city": "X"} }]
        });
        let found = warnings(&wrong);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].severity, Severity::Warning);
        assert_eq!(found[0].path, "Observation.extension.0");

        // Unknown url: silent, wherever it sits.
        let unknown = json!({
            "resourceType": "Observation", "status": "final", "code": {"text": "t"},
            "extension": [{ "url": "http://example.org/mystery", "valueString": "v" }]
        });
        assert!(warnings(&unknown).is_empty());
    }

    /// A keyword the base declares and every profile in the chain restates
    /// (bp → vitalsigns → Observation all carry `required: [status]` and the
    /// same `status` binding) is reported, and deferred, once.
    #[test]
    fn inherited_keywords_report_once() {
        use crate::effects::Deferred;
        use crate::engine::Validator;
        let validator = Validator::new(crate::packs::core_registry(helios_fhir::FhirVersion::R4));
        let opts = crate::ValidationOptions {
            profiles: vec!["http://hl7.org/fhir/StructureDefinition/bp".to_string()],
            ..Default::default()
        };

        let no_status = json!({ "resourceType": "Observation", "code": {"text": "t"} });
        let outcome = validator.validate_sync(&no_status, &opts);
        let status_required: Vec<_> = outcome
            .errors
            .iter()
            .filter(|e| e.kind == ErrorKind::Required && e.path == "Observation.status")
            .collect();
        assert_eq!(status_required.len(), 1, "{:?}", outcome.errors);
        // No identical issue appears twice anywhere in the report.
        for (i, a) in outcome.errors.iter().enumerate() {
            for b in &outcome.errors[i + 1..] {
                assert!(
                    !(a.kind == b.kind
                        && a.path == b.path
                        && a.message == b.message
                        && a.severity == b.severity),
                    "duplicate issue {a:?}"
                );
            }
        }

        let with_status =
            json!({ "resourceType": "Observation", "status": "final", "code": {"text": "t"} });
        let outcome = validator.validate_sync(&with_status, &opts);
        let status_bindings = outcome
            .deferred
            .iter()
            .filter(|d| matches!(d, Deferred::Binding { path, .. } if path == "Observation.status"))
            .count();
        assert_eq!(status_bindings, 1, "{:?}", outcome.deferred);
    }

    /// The core vitalsigns VSCat slice (value discriminators `coding.code`,
    /// `coding.system`) matches a vital-signs category, wherever the coding
    /// sits in the array; a category without it still misses the slice.
    #[test]
    fn vscat_slice_matches_through_the_coding_array() {
        use crate::engine::Validator;
        let validator = Validator::new(crate::packs::core_registry(helios_fhir::FhirVersion::R4));
        let opts = crate::ValidationOptions {
            profiles: vec!["http://hl7.org/fhir/StructureDefinition/vitalsigns".to_string()],
            ..Default::default()
        };
        let obs = |category: serde_json::Value| {
            json!({
                "resourceType": "Observation", "status": "final", "category": category,
                "code": {"coding": [{"system": "http://loinc.org", "code": "8867-4"}]},
                "subject": {"reference": "Patient/p"}, "effectiveDateTime": "2024-01-01T10:00:00Z",
                "valueQuantity": {"value": 72, "unit": "/min", "system": "http://unitsofmeasure.org", "code": "/min"}
            })
        };
        let slice_errors = |resource: &serde_json::Value| {
            validator
                .validate_sync(resource, &opts)
                .errors
                .into_iter()
                .filter(|e| e.kind == ErrorKind::SliceCardinality)
                .collect::<Vec<_>>()
        };
        let vs = json!({"system": "http://terminology.hl7.org/CodeSystem/observation-category", "code": "vital-signs"});
        let other = json!({"system": "http://example.org/cat", "code": "x"});

        assert!(slice_errors(&obs(json!([{"coding": [vs.clone()]}]))).is_empty());
        assert!(slice_errors(&obs(json!([{"coding": [other.clone(), vs.clone()]}]))).is_empty());
        let missing = slice_errors(&obs(json!([{"coding": [other]}])));
        assert_eq!(missing.len(), 1, "{missing:?}");
        assert_eq!(missing[0].path, "Observation.category");
    }

    /// A resource nested in a Bundle entry is held to its own `meta.profile`
    /// claims; an unknown nested claim warns at that claim.
    #[test]
    fn nested_resources_meet_their_meta_profiles() {
        use crate::engine::Validator;
        let validator = Validator::new(crate::packs::core_registry(helios_fhir::FhirVersion::R4));
        let opts = crate::ValidationOptions::default();
        let bundle = |profile: &str| {
            json!({
                "resourceType": "Bundle", "type": "collection",
                "entry": [{
                    "fullUrl": "urn:uuid:11111111-1111-1111-1111-111111111111",
                    "resource": {
                        "resourceType": "Observation", "status": "final",
                        "meta": {"profile": [profile]},
                        "code": {"coding": [{"system": "http://loinc.org", "code": "8867-4"}]}
                    }
                }]
            })
        };

        let errors = validator
            .validate_sync(
                &bundle("http://hl7.org/fhir/StructureDefinition/vitalsigns"),
                &opts,
            )
            .errors;
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ErrorKind::Required
                    && e.path == "Bundle.entry.0.resource.subject"),
            "vitalsigns requires subject on the entry: {errors:?}"
        );

        let errors = validator
            .validate_sync(
                &bundle("http://example.org/StructureDefinition/nope"),
                &opts,
            )
            .errors;
        let unknown: Vec<_> = errors
            .iter()
            .filter(|e| e.kind == ErrorKind::UnknownProfile)
            .collect();
        assert_eq!(unknown.len(), 1, "{errors:?}");
        assert_eq!(unknown[0].path, "Bundle.entry.0.resource.meta.profile.0");
        assert_eq!(unknown[0].severity, Severity::Warning);
    }

    /// An unknown `meta.profile` claim follows the policy (warning by
    /// default); an unknown profile the caller requested is an error.
    #[test]
    fn unknown_profiles() {
        use crate::engine::Validator;
        let validator = Validator::new(crate::packs::core_registry(helios_fhir::FhirVersion::R4));
        let nope = "http://example.org/StructureDefinition/nope";
        let claimed = json!({"resourceType": "Patient", "meta": {"profile": [nope]}});
        let errors = validator
            .validate_sync(&claimed, &crate::ValidationOptions::default())
            .errors;
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].kind, ErrorKind::UnknownProfile);
        assert_eq!(errors[0].severity, Severity::Warning);
        assert_eq!(errors[0].path, "Patient.meta.profile.0");

        let requested = crate::ValidationOptions {
            profiles: vec![nope.to_string()],
            unknown_profile: UnknownProfilePolicy::Ignore,
            ..Default::default()
        };
        let errors = validator
            .validate_sync(&json!({"resourceType": "Patient"}), &requested)
            .errors;
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].kind, ErrorKind::UnknownProfile);
        assert_eq!(errors[0].severity, Severity::Error);
        assert_eq!(errors[0].path, "Patient");
    }

    fn reference_errors(resource: &serde_json::Value, profiles: &[&str]) -> Vec<ValidationError> {
        let validator = crate::engine::Validator::new(crate::packs::core_registry(
            helios_fhir::FhirVersion::R4,
        ));
        let opts = crate::ValidationOptions {
            profiles: profiles.iter().map(|p| p.to_string()).collect(),
            ..Default::default()
        };
        validator
            .validate_sync(resource, &opts)
            .errors
            .into_iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    ErrorKind::ReferenceInvalid | ErrorKind::ReferenceUnresolved
                )
            })
            .collect()
    }

    fn observation(subject: serde_json::Value) -> serde_json::Value {
        json!({
            "resourceType": "Observation", "status": "final",
            "code": {"text": "t"}, "subject": subject
        })
    }

    #[test]
    fn reference_target_types_are_checked() {
        // Allowed targets, relative, absolute, versioned, conditional, logical.
        for ok in [
            json!({"reference": "Patient/p1"}),
            json!({"reference": "Group/g1"}),
            json!({"reference": "https://example.org/fhir/Patient/p1/_history/2"}),
            json!({"reference": "Patient?identifier=http://x|1"}),
            json!({"identifier": {"system": "http://x", "value": "1"}}),
            json!({"reference": "Patient/p1", "type": "Patient"}),
            json!({"display": "someone"}),
        ] {
            let errors = reference_errors(&observation(ok.clone()), &[]);
            assert!(errors.is_empty(), "{ok}: {errors:?}");
        }

        // A type Observation.subject does not allow.
        let errors = reference_errors(&observation(json!({"reference": "Medication/m1"})), &[]);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "Observation.subject");
        assert!(errors[0].message.contains("Found Medication"), "{errors:?}");

        // The reference contradicts its own `type`.
        let errors = reference_errors(
            &observation(json!({"reference": "Patient/p1", "type": "Group"})),
            &[],
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].message.contains("Reference.type"), "{errors:?}");

        // A profile narrows the targets: vitalsigns allows Patient only.
        let vitals = ["http://hl7.org/fhir/StructureDefinition/vitalsigns"];
        let errors = reference_errors(&observation(json!({"reference": "Group/g1"})), &vitals);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.ends_with("expected one of (Patient)"),
            "{errors:?}"
        );

        // Not a reference at all.
        let errors = reference_errors(
            &observation(json!({"reference": "not a reference at all"})),
            &[],
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].message.contains("is not a valid reference"));

        // A contained target is typed by its resourceType.
        let mut contained = observation(json!({"reference": "#m"}));
        contained["contained"] = json!([{"resourceType": "Medication", "id": "m"}]);
        let errors = reference_errors(&contained, &[]);
        assert_eq!(errors.len(), 1, "{errors:?}");
        contained["contained"] = json!([{"resourceType": "Patient", "id": "m"}]);
        assert!(reference_errors(&contained, &[]).is_empty());
    }

    #[test]
    fn bundle_urn_references_must_resolve() {
        let bundle = |kind: &str, target: &str| {
            json!({
                "resourceType": "Bundle", "type": kind,
                "entry": [
                    {"fullUrl": "urn:uuid:11111111-1111-1111-1111-111111111111",
                     "resource": {"resourceType": "Patient"},
                     "request": {"method": "POST", "url": "Patient"}},
                    {"fullUrl": "urn:uuid:22222222-2222-2222-2222-222222222222",
                     "resource": observation(json!({"reference": target})),
                     "request": {"method": "POST", "url": "Observation"}}
                ]
            })
        };
        let resolved = "urn:uuid:11111111-1111-1111-1111-111111111111";
        let dangling = "urn:uuid:99999999-9999-9999-9999-999999999999";
        assert!(reference_errors(&bundle("transaction", resolved), &[]).is_empty());

        let errors = reference_errors(&bundle("transaction", dangling), &[]);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].kind, ErrorKind::ReferenceUnresolved);
        assert_eq!(errors[0].severity, Severity::Error);
        assert_eq!(errors[0].path, "Bundle.entry.1.resource.subject");

        let errors = reference_errors(&bundle("collection", dangling), &[]);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].severity, Severity::Warning);
    }

    #[test]
    fn empty_objects_are_reported() {
        use crate::engine::Validator;
        let validator = Validator::new(crate::packs::core_registry(helios_fhir::FhirVersion::R4));
        let errors = validator
            .validate_sync(
                &json!({"resourceType": "Patient", "name": [{}], "maritalStatus": {}}),
                &crate::ValidationOptions::default(),
            )
            .errors;
        let empty: Vec<_> = errors
            .iter()
            .filter(|e| e.kind == ErrorKind::EmptyObject)
            .map(|e| e.path.as_str())
            .collect();
        assert_eq!(
            empty,
            vec!["Patient.name.0", "Patient.maritalStatus"],
            "{errors:?}"
        );
        let fine = validator
            .validate_sync(
                &json!({"resourceType": "Patient", "name": [{"family": "x"}]}),
                &crate::ValidationOptions::default(),
            )
            .errors;
        assert!(fine.is_empty(), "{fine:?}");
    }

    /// A profile that constrains a primitive's extensions gives the
    /// primitive element `elements`; the JSON string is still fine.
    #[test]
    fn elements_on_a_primitive_element_accept_the_primitive() {
        use crate::engine::Validator;
        use crate::resolver::SchemaRegistry;
        let mut registry = SchemaRegistry::new();
        registry.insert_named(
            "code",
            serde_json::from_value(json!({"kind": "primitive-type"})).unwrap(),
        );
        registry.insert_named(
            "Thing",
            serde_json::from_value(json!({"elements": {
                "resourceType": {"type": "code"},
                "c": {"type": "code", "elements": {"extension": {"type": "code", "array": true}}}
            }}))
            .unwrap(),
        );
        let validator = Validator::new(Arc::new(registry));
        let errors = validator
            .validate_sync(
                &json!({"resourceType": "Thing", "c": "x"}),
                &crate::ValidationOptions::default(),
            )
            .errors;
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn resource_ids_are_checked() {
        use crate::engine::Validator;
        let validator = Validator::new(crate::packs::core_registry(helios_fhir::FhirVersion::R4));
        let ids = |resource: serde_json::Value| {
            validator
                .validate_sync(&resource, &crate::ValidationOptions::default())
                .errors
                .into_iter()
                .filter(|e| e.kind == ErrorKind::PrimitiveValue)
                .map(|e| e.path)
                .collect::<Vec<_>>()
        };
        assert!(ids(json!({"resourceType": "Patient", "id": "abc-1.2"})).is_empty());
        assert_eq!(
            ids(json!({"resourceType": "Patient", "id": "bad id!"})),
            vec!["Patient.id"]
        );
        assert_eq!(
            ids(json!({"resourceType": "Patient", "id": "x".repeat(65)})),
            vec!["Patient.id"]
        );
        assert_eq!(
            ids(
                json!({"resourceType": "Patient", "managingOrganization": {"reference": "#o_1"},
                       "contained": [{"resourceType": "Organization", "id": "o_1"}]})
            ),
            vec!["Patient.contained.0.id"]
        );
    }

    #[test]
    fn undeclared_extensions_meet_their_definitions() {
        use crate::engine::Validator;
        let validator = Validator::new(crate::packs::core_registry(helios_fhir::FhirVersion::R4));
        let errors = |resource: serde_json::Value| {
            validator
                .validate_sync(&resource, &crate::ValidationOptions::default())
                .errors
        };
        let birthplace = "http://hl7.org/fhir/StructureDefinition/patient-birthPlace";
        assert!(
            errors(json!({"resourceType": "Patient",
                "extension": [{"url": birthplace, "valueAddress": {"city": "Taipei"}}]}))
            .is_empty()
        );
        let wrong = errors(json!({"resourceType": "Patient",
            "extension": [{"url": birthplace, "valueString": "Taipei"}]}));
        assert!(
            wrong.iter().any(|e| e.kind == ErrorKind::ChoiceExcluded
                && e.path.starts_with("Patient.extension.0")),
            "{wrong:?}"
        );
        // Unknown extensions stay allowed; unknown modifier extensions do not.
        assert!(
            errors(json!({"resourceType": "Patient",
                "extension": [{"url": "http://example.org/x", "valueString": "v"}]}))
            .is_empty()
        );
        let modifier = errors(json!({"resourceType": "Patient",
            "modifierExtension": [{"url": "http://example.org/x", "valueBoolean": true}]}));
        assert_eq!(modifier.len(), 1, "{modifier:?}");
        assert_eq!(modifier[0].path, "Patient.modifierExtension.0");
    }

    /// A required primitive (or value[x] branch) carried only by its `_`
    /// sidecar is present.
    #[test]
    fn sidecar_only_primitives_satisfy_required() {
        use crate::engine::Validator;
        let validator = Validator::new(crate::packs::core_registry(helios_fhir::FhirVersion::R4));
        let ext = json!({
            "url": "http://hl7.org/fhir/StructureDefinition/codesystem-concept-comments",
            "_valueString": {"extension": [{
                "url": "http://hl7.org/fhir/StructureDefinition/data-absent-reason",
                "valueCode": "unknown"
            }]}
        });
        let errors = validator
            .validate_sync(
                &json!({"resourceType": "Patient", "extension": [ext]}),
                &crate::ValidationOptions::default(),
            )
            .errors;
        assert!(
            !errors.iter().any(|e| e.kind == ErrorKind::Required),
            "{errors:?}"
        );
        let observation = json!({"resourceType": "Observation", "code": {"text": "t"},
        "_status": {"extension": [{
            "url": "http://hl7.org/fhir/StructureDefinition/data-absent-reason",
            "valueCode": "unknown"
        }]}});
        let errors = validator
            .validate_sync(&observation, &crate::ValidationOptions::default())
            .errors;
        assert!(
            !errors.iter().any(|e| e.kind == ErrorKind::Required),
            "{errors:?}"
        );
    }

    #[test]
    fn partial_match_semantics() {
        assert!(is_partial_match(
            &json!({"use": "home", "city": "X"}),
            &json!({"use": "home"})
        ));
        assert!(!is_partial_match(
            &json!({"use": "work"}),
            &json!({"use": "home"})
        ));
        assert!(!is_partial_match(&json!("home"), &json!({"use": "home"})));
        assert!(is_partial_match(
            &json!({"a": {"b": 1, "c": 2}}),
            &json!({"a": {"b": 1}})
        ));
        assert!(is_partial_match(&json!([1, 2, 3]), &json!([1, 2])));
        // Array patterns are unordered: each pattern item matches some item.
        assert!(is_partial_match(&json!([2, 1]), &json!([1])));
        assert!(is_partial_match(&json!([3, 2, 1]), &json!([1, 2])));
        assert!(!is_partial_match(&json!([2, 3]), &json!([1])));
        assert!(is_partial_match(&json!(5), &json!(5)));
        // An object pattern against an array: one item must match.
        let category = json!({ "coding": [
            { "system": "http://example.org/other", "code": "x" },
            { "system": "http://terminology.hl7.org/CodeSystem/observation-category", "code": "vital-signs" }
        ]});
        let vscat = json!({ "coding": {
            "system": "http://terminology.hl7.org/CodeSystem/observation-category", "code": "vital-signs"
        }});
        assert!(is_partial_match(&category, &vscat));
        // ...the same item: code and system from different codings do not.
        let split = json!({ "coding": [
            { "system": "http://terminology.hl7.org/CodeSystem/observation-category", "code": "x" },
            { "system": "http://example.org/other", "code": "vital-signs" }
        ]});
        assert!(!is_partial_match(&split, &vscat));
    }
}
