//! The cooperative validation engine.
//!
//! Validation never flattens schemas: it builds a *set* of schemas (the
//! *schemata*) for every data node — root schema from `resourceType`,
//! profiles, plus every transitively referenced `base` and complex `type` —
//! and every member of the set judges the same node together. See
//! `docs/guide/mental-model.md` in the FHIR Schema repo for the model, and
//! `walk.rs` for the deterministic error-emission order.

mod attachment;
mod bounds;
mod bundle;
mod errors;
mod narrative;
mod path;
mod primitives;
mod references;
mod resource_rules;
pub(crate) mod slicing;
mod walk;

pub(crate) use walk::resolve_element_reference;

pub use errors::{ErrorKind, Severity, ValidationError};
pub use path::dotted_to_fhirpath;

/// Crate-internal access to the effect-issue message templates (the full
/// template catalog lives in `errors.rs`, the single source of truth).
pub(crate) mod messages {
    pub(crate) use super::errors::{
        hl7_slice_max, hl7_slice_min, msg_code_system_unknown as code_system_unknown,
        msg_constraint_not_evaluable as constraint_not_evaluable,
        msg_constraints_not_evaluable as constraints_not_evaluable,
        msg_extensible_binding as extensible_binding,
        msg_fhirpath_constraint as fhirpath_constraint,
        msg_slice_closed_unmatched as slice_closed_unmatched, msg_slice_max as slice_max,
        msg_slice_min as slice_min, msg_terminology_binding as terminology_binding,
        msg_terminology_unavailable as terminology_unavailable,
        msg_unknown_code_system_version as unknown_code_system_version,
        msg_url_unresolved as url_unresolved, msg_wrong_display as wrong_display,
    };
}

use crate::effects::{Deferred, EffectHandlers};

thread_local! {
    static ON_LARGE_STACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Stack of the threads validation runs its recursive parts on (virtual
/// reservation; only the pages touched become resident).
const VALIDATION_STACK: usize = 16 * 1024 * 1024;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// See [`on_large_stack`].
fn use_large_stack_pool() -> bool {
    static USE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *USE.get_or_init(
        || match std::env::var("HFS_VALIDATION_STACK_THREAD").as_deref() {
            Ok("always") => true,
            Ok("never") => false,
            _ => cfg!(debug_assertions),
        },
    )
}

/// A bounded pool of large-stack threads, grown on demand up to the number
/// of CPUs (at least 2, at most 64). Threads stay alive, so their
/// allocator caches stay warm and no thread is created per call.
struct LargeStackPool {
    state: std::sync::Mutex<PoolState>,
    work: std::sync::Condvar,
    limit: usize,
}

#[derive(Default)]
struct PoolState {
    queue: std::collections::VecDeque<Job>,
    threads: usize,
    idle: usize,
}

impl LargeStackPool {
    fn global() -> &'static LargeStackPool {
        static POOL: std::sync::OnceLock<LargeStackPool> = std::sync::OnceLock::new();
        POOL.get_or_init(|| LargeStackPool {
            state: Default::default(),
            work: std::sync::Condvar::new(),
            limit: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .clamp(2, 64),
        })
    }

    fn submit(&'static self, job: Job) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.queue.push_back(job);
        if state.idle == 0 && state.threads < self.limit {
            let spawned = std::thread::Builder::new()
                .name("hfs-validation".to_string())
                .stack_size(VALIDATION_STACK)
                .spawn(move || self.run());
            match spawned {
                Ok(_) => state.threads += 1,
                // No thread at all to run it (thread limits): run it here,
                // on the caller's stack, as before the pool existed.
                Err(_) if state.threads == 0 => {
                    let job = state.queue.pop_back().expect("just queued");
                    drop(state);
                    job();
                    return;
                }
                Err(_) => {}
            }
        }
        drop(state);
        self.work.notify_one();
    }

    fn run(&'static self) {
        ON_LARGE_STACK.with(|flag| flag.set(true));
        loop {
            let job = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(job) = state.queue.pop_front() {
                        break job;
                    }
                    state.idle += 1;
                    state = self.work.wait(state).unwrap_or_else(|e| e.into_inner());
                    state.idle -= 1;
                }
            };
            job();
        }
    }
}

/// Run `f` on a large-stack thread of a bounded pool, and wait for it.
///
/// The structural walk and above all the FHIRPath evaluator recurse with
/// large frames: in a debug build validating a Bundle needed ~1.3 MiB of
/// stack on its own (the walk ~260 KiB, FHIRPath invariants the rest),
/// independent of the Bundle's size — too close to a Tokio worker's 2 MiB
/// once the HTTP stack sits below it. The calling (async) thread now only
/// waits (it blocked on this CPU-bound work before too); the recursion
/// happens on a pool thread whose stack does not depend on the caller's.
/// Calls already on a pool thread run inline, so nested validation never
/// waits for the pool. A panic in `f` is re-raised on the caller.
///
/// Only debug builds need this: a release build validates a Bundle in
/// ~230 KiB of stack, and the hand-over to the pool costs ~0.2–0.6 ms per
/// call on a loaded host — measurable on a 20-entry transaction, which
/// validates every entry. So release builds recurse inline unless
/// `HFS_VALIDATION_STACK_THREAD=always` (`never` disables the pool in debug
/// builds too; the default is `debug`).
pub(crate) fn on_large_stack<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    if ON_LARGE_STACK.with(std::cell::Cell::get) || !use_large_stack_pool() {
        return f();
    }
    let (done, wait) = std::sync::mpsc::sync_channel::<std::thread::Result<R>>(1);
    let job = move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        let _ = done.send(result);
    };
    let job: Box<dyn FnOnce() + Send + '_> = Box::new(job);
    // SAFETY: `f` (and so the job) may borrow from this stack frame. This
    // function does not return until the job has run to completion and sent
    // its result (`catch_unwind` guarantees the send also after a panic),
    // so nothing borrowed is used after the borrow ends. The `'static` is
    // only what the pool's queue type asks for.
    let job: Job = unsafe { std::mem::transmute::<Box<dyn FnOnce() + Send + '_>, Job>(job) };
    LargeStackPool::global().submit(job);
    match wait.recv() {
        Ok(Ok(value)) => value,
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        // The job was dropped unrun (cannot happen: the pool never drops
        // queued jobs) — never return while a borrow could still be live.
        Err(_) => std::process::abort(),
    }
}

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
    /// How an extension whose url resolves to no definition is reported.
    pub unknown_extensions: UnknownExtensionPolicy,
    /// Profiles that loaded ImplementationGuides declare in
    /// `ImplementationGuide.global`, as `(resource type, profile canonical)`:
    /// every resource of that type — the root and every nested resource
    /// (Bundle entries, contained, parameters) — is held to the profile, as
    /// the HL7 validator applies the global profiles of its loaded IGs.
    /// An unresolvable one follows [`Self::unknown_profile`].
    pub global_profiles: Vec<(String, String)>,
    /// uri / url values in the HL7 FHIR namespace (`http://hl7.org/fhir…`)
    /// that cannot name a definition — no path segment is a resource type,
    /// so no canonical resource can have that URL — reported at this
    /// severity (the HL7 validator: Type_Specific_Checks_DT_URL_Resolve,
    /// error). `None` skips the check.
    pub hl7_urls: Option<Severity>,
}

/// Policy for extensions whose url resolves to no StructureDefinition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnknownExtensionPolicy {
    /// FHIR lets receivers accept extensions they do not know: only a url
    /// in HL7's own namespaces (where every extension is published) draws a
    /// warning, when HL7's extension definitions are loaded (default).
    #[default]
    Lenient,
    /// The HL7 validator's default: any unresolvable extension is an error
    /// ("could not be found so is not allowed here"), except example urls
    /// (`example.org`, `acme.com`), `nema.org` (DICOM), HL7 tooling
    /// extensions and cross-version extensions.
    Error,
}

impl Default for ValidationOptions {
    fn default() -> Self {
        Self {
            profiles: Vec::new(),
            use_meta_profiles: true,
            check_extension_context: false,
            unknown_profile: UnknownProfilePolicy::default(),
            unknown_extensions: UnknownExtensionPolicy::default(),
            global_profiles: Vec::new(),
            hl7_urls: None,
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
    /// References to Bundle entries that conform structurally to one of
    /// their target profiles; judged again once the effects pass has found
    /// every issue of the entry (see [`Validator::validate`]).
    pub(crate) cascades: Vec<walk::Cascade>,
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

/// The HL7 validator decides whether a Bundle entry matches a reference's
/// target profile by validating it fully against the profile, invariants and
/// terminology included, so an entry that fails on those is also reported at
/// every reference to it (Reference_REF_CantMatchChoice). The walk can only
/// judge structure; here, after the effects pass, a reference whose target
/// entry ended up with error-severity issues of its own is reported the same
/// way, when the entry claims one of the candidate profiles (so its issues
/// were found against that profile) or has an issue no profile is blamed
/// for. This never changes a verdict: the entry's own errors already make
/// the resource invalid.
fn cascade_target_failures(cascades: &[walk::Cascade], errors: &mut Vec<ValidationError>) {
    let mut found = Vec::new();
    for cascade in cascades {
        let prefix = format!("{}.", cascade.target_path);
        let failing = errors.iter().any(|e| {
            e.severity == Severity::Error
                && (e.path == cascade.target_path || e.path.starts_with(&prefix))
                && (cascade.claims_candidate || e.source.is_none())
        });
        if failing
            && !found.iter().any(|f: &ValidationError| {
                f.path == cascade.path && f.message.contains(&cascade.reference)
            })
        {
            found.push(ValidationError::new(
                ErrorKind::ReferenceInvalid,
                cascade.path.clone(),
                format!(
                    "Unable to find a profile match for {} among choices: {}",
                    cascade.reference,
                    cascade.candidates.join(", ")
                ),
            ));
        }
    }
    errors.extend(found);
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
        on_large_stack(|| self.validate_sync_with(resource, opts, None))
    }

    /// [`Self::validate_sync`], with invariants evaluated where the walk
    /// must choose among type profiles.
    fn validate_sync_with<'a>(
        &'a self,
        resource: &'a Value,
        opts: &ValidationOptions,
        invariants: Option<&'a walk::InvariantCheck<'a>>,
    ) -> SyncOutcome {
        let mut outcome = walk::validate(self.resolver.as_ref(), resource, opts, invariants);
        outcome
            .errors
            .extend(bundle_entry_type_mismatches(resource));
        outcome.errors.extend(bundle_unresolved_urns(resource));
        let resolver = self.resolver.as_ref();
        let is_resource_type = |name: &str| {
            resolver
                .resolve(name)
                .is_some_and(|s| crate::editor::is_resource(&s))
        };
        outcome
            .errors
            .extend(bundle::bundle_rules_with(resource, &is_resource_type));
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
        let check = |root: &Value, path: &str, id: &str, expression: &str| -> bool {
            let Some(evaluator) = effects.constraints else {
                return true;
            };
            let c = [crate::effects::DeferredConstraint {
                path,
                id,
                expression,
                own_resource: false,
            }];
            !matches!(
                evaluator.evaluate_all(root, version, &c).first(),
                Some(crate::effects::ConstraintOutcome::Failed)
            )
        };
        let invariants: Option<&walk::InvariantCheck<'_>> = effects
            .constraints
            .is_some()
            .then_some(&check as &walk::InvariantCheck<'_>);
        let timing =
            tracing::enabled!(target: "helios_fhir_validator::timing", tracing::Level::DEBUG);
        let started = std::time::Instant::now();
        let (
            SyncOutcome {
                mut errors,
                deferred,
                cascades,
            },
            walk,
        ) = on_large_stack(|| {
            let outcome = self.validate_sync_with(resource, opts, invariants);
            (outcome, started.elapsed())
        });
        if effects.narrative_links {
            errors.extend(narrative::narrative_link_errors(resource));
        }
        if effects.resource_rules {
            errors.extend(resource_rules::resource_rule_errors(resource));
        }
        if effects.best_practice {
            errors.extend(narrative::best_practice_warnings(resource));
        }
        if let Some(signatures) = effects.signatures {
            errors.extend(crate::signature::signature_issues(resource, signatures));
        }
        let rules = started.elapsed() - walk;
        let resolver = Arc::clone(&self.resolver);
        let conformance: crate::effects::Conformance = Arc::new(move |value, profile| {
            let opts = ValidationOptions {
                profiles: vec![profile.to_string()],
                use_meta_profiles: false,
                ..Default::default()
            };
            // Structure, plus the core value sets' required bindings
            // (embedded, so answerable synchronously).
            let outcome = Validator::new(Arc::clone(&resolver)).validate_sync(value, &opts);
            let tx = crate::terminology::core_terminology(version)
                .required_binding_errors(&outcome.deferred);
            outcome
                .errors
                .iter()
                .chain(&tx)
                .all(|e| e.severity != Severity::Error)
        });
        crate::effects::execute_with(
            resource,
            version,
            &deferred,
            effects,
            Some(conformance),
            &mut errors,
        )
        .await;
        cascade_target_failures(&cascades, &mut errors);
        if timing {
            let resource_type = resource
                .get("resourceType")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            tracing::debug!(
                target: "helios_fhir_validator::timing",
                resource_type,
                walk_us = walk.as_micros() as u64,
                rules_us = rules.as_micros() as u64,
                effects_us = (started.elapsed() - walk - rules).as_micros() as u64,
                total_us = started.elapsed().as_micros() as u64,
                deferred = deferred.len(),
                issues = errors.len(),
                "validation stages"
            );
        }
        errors
    }
}
