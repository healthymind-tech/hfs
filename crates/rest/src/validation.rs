//! Resource validation service: the REST layer's bridge to
//! `helios-fhir-validator`.
//!
//! Owns the per-version core validators (embedded schema packs), the
//! FHIRPath constraint evaluator, the optional terminology provider,
//! per-tenant profile registries fed from stored StructureDefinitions, and
//! the mapping from validator issues onto FHIR `OperationOutcome` issues.
//!
//! The `$validate` operation uses [`ValidationService::validate_resource`]
//! unconditionally; the write path (create/update/batch) goes through
//! [`ValidationService::check_write`], which is gated by
//! `HFS_VALIDATION_MODE` (`off` | `log` | `enforce`).

use crate::config::ValidationConfig;
use crate::error::RestError;
use crate::responses::operation_outcome::{
    Issue, IssueSeverity, IssueType, OperationOutcomeBuilder,
};
use async_trait::async_trait;
use dashmap::DashMap;
use helios_fhir::FhirVersion;
use helios_fhir_validator::fhirpath_effects::FhirPathConstraintEvaluator;
use helios_fhir_validator::{
    CodedValue, CompositeResolver, CompositeTerminologyProvider, EffectHandlers, ErrorKind,
    SchemaRegistry, SchemaResolver, Severity, TerminologyError, TerminologyFailMode,
    TerminologyProvider, TxCheck, TxVerdict, UnknownProfilePolicy, ValidationError,
    ValidationOptions, Validator, dotted_to_fhirpath,
};
use helios_persistence::core::{ExportDataProvider, ExportRequest, ResourceStorage};
use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Per-tenant, per-version profile registries fed from stored
/// StructureDefinitions.
type TenantProfileMap = DashMap<(String, FhirVersion), Arc<RwLock<SchemaRegistry>>>;

/// Outcome of a startup profile rehydration pass
/// ([`ValidationService::rehydrate_stored_profiles`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileRehydrationReport {
    /// Tenants scanned.
    pub tenants: usize,
    /// StructureDefinitions folded into a registry.
    pub registered: usize,
    /// StructureDefinitions read but not registered (conversion failure, or
    /// a core canonical with `HFS_VALIDATION_SHADOW_CORE=false`).
    pub skipped: usize,
    /// Storage failures; a non-zero count means some tenant's profiles may be
    /// missing until they are re-written.
    pub storage_errors: usize,
    /// Entries taken from the compiled-profile cache before the database
    /// was read (validation stopped waiting once these were registered).
    pub cached: usize,
    /// StructureDefinitions read from storage and converted in this pass
    /// (new, changed, or everything on a cold start).
    pub converted: usize,
    /// Cached entries whose row had disappeared from storage; dropped.
    pub removed: usize,
    /// Time until the registries were complete enough for validation to
    /// stop waiting (the cache load on a warm start, the whole pass on a
    /// cold one).
    pub ready_after: Duration,
    /// Wall-clock duration of the pass.
    pub elapsed: Duration,
}

/// Pages a cursor can advance before a tenant scan is abandoned (a backend
/// whose cursor never advances must not spin forever).
const MAX_REHYDRATION_PAGES: usize = 10_000;

/// Write-path behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ValidationMode {
    /// Skip write-path validation entirely.
    #[default]
    Off,
    /// Validate, log issues, and proceed.
    Log,
    /// Reject invalid resources with `422 Unprocessable Entity`.
    Enforce,
}

/// The validation service. Cheap to construct (core registries load lazily,
/// once per process); hold one in `AppState` for the AST cache to pay off.
pub struct ValidationService {
    mode: ValidationMode,
    constraint_evaluator: Option<FhirPathConstraintEvaluator>,
    terminology: Option<Arc<dyn TerminologyProvider>>,
    /// Constraint ids never evaluated (default: `dom-6`, the narrative
    /// warning that fires on almost every machine-generated resource).
    suppress_constraints: Vec<String>,
    /// How terminology-service failures on required bindings are reported.
    terminology_fail: TerminologyFailMode,
    /// Severity of a wrong `Coding.display` (`HFS_VALIDATION_DISPLAY_ISSUES`).
    display_issues: Severity,
    /// Check `extensible` bindings (`HFS_VALIDATION_EXTENSIBLE_BINDINGS`).
    check_extensible: bool,
    /// HL7 best-practice warnings (`HFS_VALIDATION_BEST_PRACTICE`).
    best_practice: bool,
    /// Validate against `meta.profile` claims.
    use_meta_profiles: bool,
    unknown_profile: UnknownProfilePolicy,
    /// Per-tenant profile overlays, fed from stored StructureDefinition
    /// writes. `None` disables the feature.
    tenant_profiles: Option<TenantProfileMap>,
    /// Allow stored StructureDefinitions to override embedded core
    /// definitions with the same canonical URL (`HFS_VALIDATION_SHADOW_CORE`).
    shadow_core: bool,
    /// `true` while a startup rehydration pass is announced but not finished;
    /// validation waits for it (see [`Self::begin_rehydration`]).
    rehydrating: tokio::sync::watch::Sender<bool>,
    /// Upper bound on that wait (`HFS_VALIDATION_REHYDRATE_WAIT_MS`).
    rehydrate_wait: Duration,
    /// Compiled-profile cache on disk (`HFS_VALIDATION_PROFILE_CACHE`).
    profile_cache: Option<crate::validation_cache::ProfileCache>,
    /// The stored StructureDefinitions folded into the registries, with
    /// their row stamps and schemas — what the cache file is written from.
    stored_index: std::sync::Mutex<crate::validation_cache::StoredIndex>,
    /// Signalled when `stored_index` changed and the cache file is stale.
    cache_dirty: tokio::sync::Notify,
}

impl Default for ValidationService {
    fn default() -> Self {
        Self {
            mode: ValidationMode::Off,
            constraint_evaluator: Some(FhirPathConstraintEvaluator::new()),
            terminology: None,
            suppress_constraints: vec!["dom-6".to_string()],
            terminology_fail: TerminologyFailMode::Open,
            display_issues: Severity::Warning,
            check_extensible: true,
            best_practice: true,
            use_meta_profiles: true,
            unknown_profile: UnknownProfilePolicy::Warn,
            tenant_profiles: Some(DashMap::new()),
            shadow_core: false,
            rehydrating: tokio::sync::watch::Sender::new(false),
            rehydrate_wait: Duration::from_millis(60_000),
            profile_cache: None,
            stored_index: Default::default(),
            cache_dirty: tokio::sync::Notify::new(),
        }
    }
}

impl ValidationService {
    /// A service with the default posture: write path off, constraints on
    /// (dom-6 suppressed), meta.profile honored, unresolvable profiles
    /// warned, stored profiles on, no terminology provider.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a service from `HFS_VALIDATION_*` configuration.
    /// `terminology_server` is `HFS_TERMINOLOGY_SERVER` (required for
    /// `terminology = remote`; the config validator enforces the pairing).
    pub fn from_config(
        config: &ValidationConfig,
        terminology_server: Option<&str>,
        version: helios_fhir::FhirVersion,
    ) -> Self {
        let mode = match config.mode.as_str() {
            "log" => ValidationMode::Log,
            "enforce" => ValidationMode::Enforce,
            _ => ValidationMode::Off,
        };
        let unknown_profile = match config.unknown_profile.as_str() {
            "error" => UnknownProfilePolicy::Error,
            "ignore" => UnknownProfilePolicy::Ignore,
            _ => UnknownProfilePolicy::Warn,
        };
        let terminology: Option<Arc<dyn TerminologyProvider>> =
            match (config.terminology.as_str(), terminology_server) {
                // Embedded core value sets answer the FHIR base bindings
                // offline; everything they cannot judge goes to the server.
                ("remote", Some(base)) => Some(Arc::new(CompositeTerminologyProvider::new(
                    helios_fhir_validator::core_terminology(version),
                    Arc::new(RemoteTerminologyProvider::new(
                        base.to_string(),
                        Duration::from_millis(config.terminology_timeout_ms),
                    )),
                ))),
                ("remote-only", Some(base)) => Some(Arc::new(RemoteTerminologyProvider::new(
                    base.to_string(),
                    Duration::from_millis(config.terminology_timeout_ms),
                ))),
                // Offline required-binding checks against the FHIR core value
                // sets embedded in helios-fhir-validator (no server needed).
                ("embedded", _) => {
                    let provider: Arc<dyn TerminologyProvider> =
                        helios_fhir_validator::core_terminology(version);
                    Some(provider)
                }
                _ => None,
            };
        let terminology_fail = match config.terminology_fail.as_str() {
            "closed" => TerminologyFailMode::Closed,
            "closed-required" => TerminologyFailMode::ClosedRequired,
            _ => TerminologyFailMode::Open,
        };
        Self {
            mode,
            constraint_evaluator: config.constraints.then(FhirPathConstraintEvaluator::new),
            terminology,
            suppress_constraints: config.suppress_constraints.clone(),
            terminology_fail,
            display_issues: match config.display_issues.as_str() {
                "error" => Severity::Error,
                _ => Severity::Warning,
            },
            check_extensible: config.extensible_bindings != "off",
            best_practice: config.best_practice != "off",
            use_meta_profiles: config.meta_profiles,
            unknown_profile,
            tenant_profiles: config.stored_profiles.then(DashMap::new),
            shadow_core: config.shadow_core,
            rehydrating: tokio::sync::watch::Sender::new(false),
            rehydrate_wait: Duration::from_millis(config.rehydrate_wait_ms),
            profile_cache: None,
            stored_index: Default::default(),
            cache_dirty: tokio::sync::Notify::new(),
        }
    }

    /// Persist compiled stored profiles under `dir` so a restart only
    /// converts what changed (see [`crate::validation_cache`]). `None`
    /// leaves the cache off.
    pub fn with_profile_cache(mut self, dir: Option<std::path::PathBuf>) -> Self {
        self.profile_cache = dir.and_then(|dir| {
            let cache = crate::validation_cache::ProfileCache::new(&dir, self.shadow_core);
            if cache.is_none() {
                warn!(dir = %dir.display(), "cannot fingerprint this build; compiled-profile cache disabled");
            }
            cache
        });
        self
    }

    /// Use an explicit cache (tests pin the fingerprint).
    pub fn with_profile_cache_at(
        mut self,
        cache: Option<crate::validation_cache::ProfileCache>,
    ) -> Self {
        self.profile_cache = cache;
        self
    }

    /// Replace the terminology provider (bindings stay unchecked without one).
    pub fn with_terminology(mut self, provider: Arc<dyn TerminologyProvider>) -> Self {
        self.terminology = Some(provider);
        self
    }

    /// The configured write-path mode.
    pub fn mode(&self) -> ValidationMode {
        self.mode
    }

    /// Validate a resource against the core pack for `version` (overlaid
    /// with the tenant's stored profiles) plus any extra profile canonicals.
    /// Structural issues first, then constraint issues, then binding issues.
    pub async fn validate_resource(
        &self,
        version: FhirVersion,
        resource: &Value,
        profiles: Vec<String>,
        tenant: Option<&str>,
    ) -> Vec<ValidationError> {
        // While a cold rehydration is still filling the registries, validate
        // right away and only wait (then validate again) when the pass hit a
        // definition the registries could not resolve — a request whose
        // profiles are already there, or that needs none, never waits.
        if self.is_rehydrating() {
            let misses = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let issues = self
                .validate_with(version, resource, profiles.clone(), tenant, Some(&misses))
                .await;
            if !misses.load(std::sync::atomic::Ordering::Relaxed) {
                return issues;
            }
            self.await_rehydration().await;
        }
        self.validate_with(version, resource, profiles, tenant, None)
            .await
    }

    async fn validate_with(
        &self,
        version: FhirVersion,
        resource: &Value,
        profiles: Vec<String>,
        tenant: Option<&str>,
        misses: Option<&Arc<std::sync::atomic::AtomicBool>>,
    ) -> Vec<ValidationError> {
        let core = helios_fhir_validator::packs::core_registry(version);
        let resolver: Arc<dyn SchemaResolver> = match self.tenant_overlay(tenant, version) {
            Some(overlay) => Arc::new(CompositeResolver::new(vec![overlay, core])),
            None => core,
        };
        let resolver: Arc<dyn SchemaResolver> = match misses {
            Some(flag) => Arc::new(MissRecorder {
                inner: resolver,
                missed: Arc::clone(flag),
            }),
            None => resolver,
        };
        let validator = Validator::new(resolver);
        let opts = ValidationOptions {
            profiles,
            use_meta_profiles: self.use_meta_profiles,
            unknown_profile: self.unknown_profile,
            // An extension outside its declared context is an error, as
            // the HL7 validator reports it (warning on canonical resources).
            check_extension_context: true,
        };
        let handlers = EffectHandlers {
            constraints: self
                .constraint_evaluator
                .as_ref()
                .map(|e| e as &dyn helios_fhir_validator::ConstraintEvaluator),
            terminology: self.terminology.as_deref(),
            suppress_constraints: &self.suppress_constraints,
            terminology_fail: self.terminology_fail,
            display_issues: self.display_issues,
            check_extensible: self.check_extensible,
            // Unresolved internal narrative links are errors, as the HL7
            // validator reports them.
            narrative_links: true,
            best_practice: self.best_practice,
        };
        validator
            .validate(resource, version, &opts, &handlers)
            .await
    }

    /// Write-path gate. `Ok(())` = proceed with the write; `Err` = reject
    /// (enforce mode with error-severity issues → `422` carrying the full
    /// OperationOutcome).
    pub async fn check_write(
        &self,
        tenant: &str,
        version: FhirVersion,
        resource_type: &str,
        resource: &Value,
    ) -> Result<(), RestError> {
        if self.mode == ValidationMode::Off {
            return Ok(());
        }
        let issues = self
            .validate_resource(version, resource, Vec::new(), Some(tenant))
            .await;
        if issues.is_empty() {
            return Ok(());
        }
        match self.mode {
            ValidationMode::Off => Ok(()),
            ValidationMode::Log => {
                for issue in &issues {
                    warn!(
                        tenant = %tenant,
                        resource_type = %resource_type,
                        path = %issue.path,
                        kind = ?issue.kind,
                        "validation (log mode): {}",
                        issue.message
                    );
                }
                Ok(())
            }
            ValidationMode::Enforce => {
                if has_errors(&issues) {
                    debug!(
                        tenant = %tenant,
                        resource_type = %resource_type,
                        issues = issues.len(),
                        "rejecting write: validation failed"
                    );
                    Err(RestError::ValidationFailed {
                        outcome: validation_outcome(&issues),
                    })
                } else {
                    // Warnings only: proceed, but leave a trace.
                    for issue in &issues {
                        warn!(
                            tenant = %tenant,
                            resource_type = %resource_type,
                            path = %issue.path,
                            "validation warning on write: {}",
                            issue.message
                        );
                    }
                    Ok(())
                }
            }
        }
    }

    /// Fold a stored StructureDefinition into the tenant's profile registry
    /// (called after successful StructureDefinition writes). Conversion
    /// failures are logged, never fatal — the write itself already
    /// succeeded.
    ///
    /// Registration rules (#H4):
    /// - A canonical URL the embedded core pack already defines is **not**
    ///   registered unless `HFS_VALIDATION_SHADOW_CORE=true`: importing
    ///   `hl7.fhir.r4.core` must not replace the base types and core
    ///   profiles the validator shipped with.
    /// - Profiles and extensions (`derivation: constraint`) are indexed by
    ///   URL only. Nothing references a profile by *name*, while element
    ///   `type`s resolve base types by name — so a `name` alias would let
    ///   `rendering-xhtml` (`name: "xhtml"`) shadow the primitive `xhtml`.
    /// - Other definitions (specializations, logical models) keep their
    ///   name alias unless that name is already a core type.
    ///
    /// Returns `true` when the definition ended up in the registry.
    pub fn upsert_stored_profile(&self, tenant: &str, version: FhirVersion, sd: &Value) -> bool {
        if self.tenant_profiles.is_none() {
            return false;
        }
        let schema = prepare_stored_profile(self.shadow_core, tenant, version, sd);
        let registered = match &schema {
            Some(schema) => self.install_profile(tenant, version, schema.clone()),
            None => false,
        };
        // Index it for the compiled-profile cache. The body handed to the
        // write path carries no server `meta`, so the stamp has no version:
        // the next start re-reads this row once and records the real one.
        if let Some(stamp) = crate::validation_cache::RowStamp::of(sd) {
            let url = sd.get("url").and_then(Value::as_str).map(str::to_string);
            self.stored_index
                .lock()
                .expect("stored profile index lock")
                .put(crate::validation_cache::StoredProfile {
                    tenant: tenant.to_string(),
                    fhir_version: version,
                    stamp,
                    url,
                    schema: schema.filter(|_| registered),
                    seq: 0,
                    confirmed: true,
                    live: true,
                });
            self.cache_dirty.notify_one();
        }
        registered
    }

    /// Insert a converted stored definition into the tenant registry under
    /// the registration rules of [`Self::upsert_stored_profile`]. Returns
    /// `false` when the schema has no URL.
    fn install_profile(
        &self,
        tenant: &str,
        version: FhirVersion,
        schema: helios_fhir_validator::FhirSchema,
    ) -> bool {
        let Some(registries) = &self.tenant_profiles else {
            return false;
        };
        let core = helios_fhir_validator::packs::core_registry(version);
        let url = schema.url.clone();
        let is_constraint = schema.derivation.as_deref() == Some("constraint")
            || schema.kind.as_deref() == Some(helios_fhir_validator::schema::kind::EXTENSION);
        let name_is_core_type = schema
            .name
            .as_deref()
            .is_some_and(|name| core.resolve(name).is_some());
        let registry = registries
            .entry((tenant.to_string(), version))
            .or_insert_with(|| Arc::new(RwLock::new(SchemaRegistry::new())))
            .clone();
        let mut registry = registry.write().expect("tenant profile registry lock");
        let inserted = if is_constraint || name_is_core_type {
            if name_is_core_type && !is_constraint {
                warn!(
                    tenant = %tenant,
                    url = ?url,
                    name = ?schema.name,
                    "stored StructureDefinition is named like a core type; registered by url only"
                );
            }
            registry.insert_by_url(schema)
        } else {
            registry.insert(schema)
        };
        if inserted {
            debug!(tenant = %tenant, url = ?url, "tenant profile registered");
        } else {
            warn!(tenant = %tenant, "stored StructureDefinition has no url; not registered");
        }
        inserted
    }

    /// Drop a stored StructureDefinition from the tenant's profile registry
    /// (called after a successful StructureDefinition delete). Returns `true`
    /// if a definition under `url` was registered.
    pub fn remove_stored_profile(&self, tenant: &str, version: FhirVersion, url: &str) -> bool {
        let Some(registries) = &self.tenant_profiles else {
            return false;
        };
        {
            let mut index = self.stored_index.lock().expect("stored profile index lock");
            let before = index.entries.len();
            index.entries.retain(|(t, _), p| {
                !(t == tenant && p.fhir_version == version && p.url.as_deref() == Some(url))
            });
            if index.entries.len() != before {
                self.cache_dirty.notify_one();
            }
        }
        let Some(registry) = registries.get(&(tenant.to_string(), version)) else {
            return false;
        };
        let removed = registry
            .write()
            .expect("tenant profile registry lock")
            .remove(url);
        if removed {
            debug!(tenant = %tenant, url = %url, "tenant profile unregistered");
        }
        removed
    }

    /// Announce that a startup rehydration pass is about to run. Until
    /// [`Self::rehydrate_stored_profiles`] has registered the stored
    /// profiles (from the compiled-profile cache when there is one, else
    /// from storage), a validation that needs a definition the registries
    /// cannot resolve yet waits for it (at most
    /// `HFS_VALIDATION_REHYDRATE_WAIT_MS`) instead of answering from a
    /// half-filled registry: a `$validate` right after a restart would
    /// otherwise report the profile as not found, and an enforce-mode
    /// write claiming a stored profile would be accepted unchecked.
    /// Validations that resolve everything they need never wait.
    pub fn begin_rehydration(&self) {
        if self.tenant_profiles.is_some() {
            self.rehydrating.send_replace(true);
        }
    }

    /// Whether a startup rehydration pass is announced and not yet finished.
    pub fn is_rehydrating(&self) -> bool {
        *self.rehydrating.borrow()
    }

    async fn await_rehydration(&self) {
        if !self.is_rehydrating() {
            return;
        }
        let mut rx = self.rehydrating.subscribe();
        if tokio::time::timeout(self.rehydrate_wait, rx.wait_for(|busy| !*busy))
            .await
            .is_err()
        {
            warn!(
                wait_ms = self.rehydrate_wait.as_millis() as u64,
                "profile rehydration still running; validating against the profiles registered so far"
            );
        }
    }

    /// Whether stored StructureDefinitions feed profile registries at all
    /// (`HFS_VALIDATION_STORED_PROFILES`).
    pub fn stored_profiles_enabled(&self) -> bool {
        self.tenant_profiles.is_some()
    }

    /// Replay every stored StructureDefinition into the profile registries
    /// — the startup mirror of the write-handler path. Before this existed
    /// the registries were in-memory only, so every restart silently
    /// forgot every uploaded profile until it was re-written.
    ///
    /// With a compiled-profile cache ([`crate::validation_cache`]) the
    /// cached schemas are registered first and validation stops waiting;
    /// the storage scan then only converts rows whose `meta.versionId` /
    /// `meta.lastUpdated` changed, and drops cached rows that are gone.
    ///
    /// Pages through storage with [`ExportDataProvider::fetch_export_batch`]
    /// (the one enumeration primitive every backend implements, deleted
    /// resources already filtered out) for the default tenant plus every
    /// registered tenant, then reads the changed rows back through
    /// [`ResourceStorage::read_batch`] so each definition is registered
    /// under the FHIR version it was stored with — exactly as the write path
    /// did. Conversion runs on blocking threads, in parallel.
    ///
    /// Errors are absorbed and counted, never propagated: one tenant's
    /// storage failure must not stop the others, one malformed
    /// StructureDefinition must not stop the scan.
    pub async fn rehydrate_stored_profiles<S>(
        &self,
        storage: &S,
        default_tenant: &str,
        batch_size: u32,
    ) -> ProfileRehydrationReport
    where
        S: ResourceStorage + ExportDataProvider + ?Sized,
    {
        let started = Instant::now();
        let mut report = ProfileRehydrationReport::default();
        if self.tenant_profiles.is_none() {
            debug!("stored profiles disabled; skipping profile rehydration");
            self.rehydrating.send_replace(false);
            return report;
        }
        // Released however the pass ends, so a waiting request never hangs
        // on a pass that panicked or returned early.
        struct Done<'a>(&'a tokio::sync::watch::Sender<bool>);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                self.0.send_replace(false);
            }
        }
        let _done = Done(&self.rehydrating);
        let batch_size = batch_size.max(1);

        self.load_profile_cache(&mut report).await;
        if report.cached > 0 {
            report.ready_after = started.elapsed();
            self.rehydrating.send_replace(false);
        }

        let mut tenants: BTreeSet<String> = BTreeSet::new();
        tenants.insert(default_tenant.to_string());
        if storage.supports_tenant_registry() {
            match storage.list_tenants().await {
                Ok(records) => tenants.extend(records.into_iter().map(|r| r.id)),
                Err(e) => {
                    warn!(error = %e, "listing tenants for profile rehydration failed; default tenant only");
                    report.storage_errors += 1;
                }
            }
        }
        report.tenants = tenants.len();

        for tenant_id in &tenants {
            self.rehydrate_tenant_profiles(storage, tenant_id, batch_size, &mut report)
                .await;
        }
        // Rows that are gone are only known for certain after a clean scan.
        if report.storage_errors == 0 {
            self.prune_unconfirmed_profiles(&mut report);
        }
        if report.cached == 0 {
            report.ready_after = started.elapsed();
        }
        self.rehydrating.send_replace(false);
        // A warm start that found nothing to change leaves the file as is.
        if report.cached == 0 || report.converted > 0 || report.removed > 0 {
            self.save_profile_cache().await;
        }

        report.elapsed = started.elapsed();
        if report.storage_errors == 0 {
            info!(
                tenants = report.tenants,
                registered = report.registered,
                skipped = report.skipped,
                cached = report.cached,
                converted = report.converted,
                removed = report.removed,
                ready_ms = report.ready_after.as_millis() as u64,
                elapsed_ms = report.elapsed.as_millis() as u64,
                "stored StructureDefinitions rehydrated into the profile registry"
            );
        } else {
            warn!(
                tenants = report.tenants,
                registered = report.registered,
                skipped = report.skipped,
                cached = report.cached,
                converted = report.converted,
                storage_errors = report.storage_errors,
                elapsed_ms = report.elapsed.as_millis() as u64,
                "profile rehydration incomplete; some stored profiles may not resolve until re-written"
            );
        }
        report
    }

    /// Register the compiled-profile cache's entries (warm start).
    async fn load_profile_cache(&self, report: &mut ProfileRehydrationReport) {
        let Some(cache) = self.profile_cache.clone() else {
            return;
        };
        let started = Instant::now();
        let entries = match tokio::task::spawn_blocking(move || cache.load()).await {
            Ok(Ok(Some(entries))) => entries,
            Ok(Ok(None)) => {
                debug!("no compiled-profile cache for this build; converting every stored profile");
                return;
            }
            Ok(Err(e)) => {
                warn!(error = %e, "compiled-profile cache unreadable; converting every stored profile");
                return;
            }
            Err(e) => {
                warn!(error = %e, "loading the compiled-profile cache failed");
                return;
            }
        };
        for mut entry in entries {
            let key = (entry.tenant.clone(), entry.stamp.id.clone());
            if self
                .stored_index
                .lock()
                .expect("stored profile index lock")
                .entries
                .get(&key)
                .is_some_and(|e| e.live)
            {
                continue; // written since this process started: newer
            }
            if let Some(schema) = entry.schema.clone() {
                self.install_profile(&entry.tenant, entry.fhir_version, schema);
            }
            entry.confirmed = false;
            entry.live = false;
            self.stored_index
                .lock()
                .expect("stored profile index lock")
                .put(entry);
            report.cached += 1;
        }
        info!(
            cached = report.cached,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "compiled-profile cache loaded; validation no longer waits for rehydration"
        );
    }

    /// Drop cached entries the storage scan did not see (deleted while the
    /// server was down), re-registering any other row that shares the URL.
    fn prune_unconfirmed_profiles(&self, report: &mut ProfileRehydrationReport) {
        let stale: Vec<crate::validation_cache::StoredProfile> = {
            let mut index = self.stored_index.lock().expect("stored profile index lock");
            let keys: Vec<(String, String)> = index
                .entries
                .iter()
                .filter(|(_, p)| !p.confirmed && !p.live)
                .map(|(k, _)| k.clone())
                .collect();
            keys.iter()
                .filter_map(|k| index.entries.remove(k))
                .collect()
        };
        for gone in stale {
            report.removed += 1;
            let Some(url) = gone.url.as_deref() else {
                continue;
            };
            if gone.schema.is_some()
                && let Some(registries) = &self.tenant_profiles
                && let Some(registry) = registries.get(&(gone.tenant.clone(), gone.fhir_version))
            {
                registry
                    .write()
                    .expect("tenant profile registry lock")
                    .remove(url);
            }
            let survivor = self
                .stored_index
                .lock()
                .expect("stored profile index lock")
                .entries
                .values()
                .filter(|p| {
                    p.tenant == gone.tenant
                        && p.fhir_version == gone.fhir_version
                        && p.url.as_deref() == Some(url)
                })
                .max_by_key(|p| p.seq)
                .and_then(|p| p.schema.clone());
            if let Some(schema) = survivor {
                self.install_profile(&gone.tenant, gone.fhir_version, schema);
            }
        }
        if report.removed > 0 {
            debug!(removed = report.removed, "dropped cached profiles no longer in storage");
        }
    }

    /// Write the compiled-profile cache (no-op without one).
    pub async fn save_profile_cache(&self) {
        let Some(cache) = self.profile_cache.clone() else {
            return;
        };
        let entries = self
            .stored_index
            .lock()
            .expect("stored profile index lock")
            .ordered();
        let started = Instant::now();
        match tokio::task::spawn_blocking(move || cache.save(entries)).await {
            Ok(Ok(count)) => debug!(
                entries = count,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "compiled-profile cache written"
            ),
            Ok(Err(e)) => warn!(error = %e, "writing the compiled-profile cache failed"),
            Err(e) => warn!(error = %e, "writing the compiled-profile cache failed"),
        }
    }

    /// Keep the compiled-profile cache current after StructureDefinition
    /// writes: rewritten at most every few seconds while writes continue
    /// (an IG install), never during a rehydration pass (which writes it
    /// when done).
    pub fn spawn_profile_cache_writer(self: &Arc<Self>) {
        if self.profile_cache.is_none() {
            return;
        }
        // The service lives as long as the server; the task with it.
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                svc.cache_dirty.notified().await;
                tokio::time::sleep(Duration::from_secs(10)).await;
                if !svc.is_rehydrating() {
                    svc.save_profile_cache().await;
                }
            }
        });
    }

    async fn rehydrate_tenant_profiles<S>(
        &self,
        storage: &S,
        tenant_id: &str,
        batch_size: u32,
        report: &mut ProfileRehydrationReport,
    ) where
        S: ResourceStorage + ExportDataProvider + ?Sized,
    {
        use crate::validation_cache::{RowStamp, StoredProfile};
        const TYPE: &str = "StructureDefinition";

        let tenant = TenantContext::new(TenantId::new(tenant_id), TenantPermissions::full_access());
        let mut request = ExportRequest::system();
        request.resource_types = vec![TYPE.to_string()];
        request.batch_size = batch_size;

        let mut cursor: Option<String> = None;
        for page in 0.. {
            if page >= MAX_REHYDRATION_PAGES {
                warn!(tenant = %tenant_id, pages = page, "stopping profile rehydration at the page backstop");
                report.storage_errors += 1;
                return;
            }
            let batch = match storage
                .fetch_export_batch(&tenant, &request, TYPE, cursor.as_deref(), batch_size)
                .await
            {
                Ok(batch) => batch,
                Err(e) => {
                    warn!(tenant = %tenant_id, error = %e, "paging stored StructureDefinitions failed");
                    report.storage_errors += 1;
                    return;
                }
            };
            // Rows whose cached entry is still current are kept as they are.
            let mut changed: std::collections::HashMap<String, RowStamp> =
                std::collections::HashMap::new();
            {
                let mut index = self.stored_index.lock().expect("stored profile index lock");
                for stamp in batch.lines.iter().filter_map(|l| RowStamp::of_line(l)) {
                    match index
                        .entries
                        .get_mut(&(tenant_id.to_string(), stamp.id.clone()))
                    {
                        Some(entry)
                            if entry.live
                                || (entry.stamp == stamp && stamp.version_id.is_some()) =>
                        {
                            entry.confirmed = true;
                            if entry.schema.is_some() {
                                report.registered += 1;
                            } else {
                                report.skipped += 1;
                            }
                        }
                        _ => {
                            changed.insert(stamp.id.clone(), stamp);
                        }
                    }
                }
            }
            if !changed.is_empty() {
                let id_refs: Vec<&str> = changed.keys().map(String::as_str).collect();
                match storage.read_batch(&tenant, TYPE, &id_refs).await {
                    Ok(stored) => {
                        let items: Vec<(FhirVersion, Value)> = stored
                            .into_iter()
                            .map(|sd| (sd.fhir_version(), sd.into_content()))
                            .collect();
                        let shadow_core = self.shadow_core;
                        let owner = tenant_id.to_string();
                        let converted = match tokio::task::spawn_blocking(move || {
                            prepare_stored_profiles(shadow_core, &owner, items)
                        })
                        .await
                        {
                            Ok(converted) => converted,
                            Err(e) => {
                                warn!(tenant = %tenant_id, error = %e, "converting stored StructureDefinitions failed");
                                report.storage_errors += 1;
                                return;
                            }
                        };
                        for (version, content, schema) in converted {
                            report.converted += 1;
                            let Some(id) = content.get("id").and_then(Value::as_str) else {
                                report.skipped += 1;
                                continue;
                            };
                            let key = (tenant_id.to_string(), id.to_string());
                            if self
                                .stored_index
                                .lock()
                                .expect("stored profile index lock")
                                .entries
                                .get(&key)
                                .is_some_and(|e| e.live)
                            {
                                report.registered += 1;
                                continue; // a live write already registered a newer one
                            }
                            let registered = match &schema {
                                Some(schema) => {
                                    self.install_profile(tenant_id, version, schema.clone())
                                }
                                None => false,
                            };
                            if registered {
                                report.registered += 1;
                            } else {
                                report.skipped += 1;
                            }
                            let stamp = changed
                                .remove(id)
                                .or_else(|| RowStamp::of(&content))
                                .expect("row has an id");
                            self.stored_index
                                .lock()
                                .expect("stored profile index lock")
                                .put(StoredProfile {
                                    tenant: tenant_id.to_string(),
                                    fhir_version: version,
                                    stamp,
                                    url: content
                                        .get("url")
                                        .and_then(Value::as_str)
                                        .map(str::to_string),
                                    schema: schema.filter(|_| registered),
                                    seq: 0,
                                    confirmed: true,
                                    live: false,
                                });
                        }
                    }
                    Err(e) => {
                        warn!(tenant = %tenant_id, error = %e, "reading stored StructureDefinitions failed");
                        report.storage_errors += 1;
                        return;
                    }
                }
            }
            match batch.next_cursor {
                Some(next) if !batch.is_last => cursor = Some(next),
                _ => return,
            }
        }
    }

    fn tenant_overlay(
        &self,
        tenant: Option<&str>,
        version: FhirVersion,
    ) -> Option<Arc<dyn SchemaResolver>> {
        let registries = self.tenant_profiles.as_ref()?;
        let tenant = tenant?;
        let registry = registries.get(&(tenant.to_string(), version))?.clone();
        Some(Arc::new(LockedRegistryResolver(registry)))
    }
}

/// Resolver adapter over a shared, mutable registry.
struct LockedRegistryResolver(Arc<RwLock<SchemaRegistry>>);

impl SchemaResolver for LockedRegistryResolver {
    fn resolve(&self, reference: &str) -> Option<Arc<helios_fhir_validator::FhirSchema>> {
        self.0
            .read()
            .expect("tenant profile registry lock")
            .resolve(reference)
    }
}

/// Resolver adapter that notes whether any lookup came back empty — how a
/// validation running during a cold rehydration learns that it needed a
/// definition the registries do not have yet.
struct MissRecorder {
    inner: Arc<dyn SchemaResolver>,
    missed: Arc<std::sync::atomic::AtomicBool>,
}

impl SchemaResolver for MissRecorder {
    fn resolve(&self, reference: &str) -> Option<Arc<helios_fhir_validator::FhirSchema>> {
        let found = self.inner.resolve(reference);
        if found.is_none() {
            self.missed
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        found
    }
}

/// The pure half of registering a stored StructureDefinition: the
/// core-canonical check and the conversion. `None` = not registered.
fn prepare_stored_profile(
    shadow_core: bool,
    tenant: &str,
    version: FhirVersion,
    sd: &Value,
) -> Option<helios_fhir_validator::FhirSchema> {
    let url = sd.get("url").and_then(Value::as_str);
    if !shadow_core
        && let Some(url) = url
        && helios_fhir_validator::packs::core_registry(version)
            .resolve(url)
            .is_some()
    {
        debug!(
            tenant = %tenant,
            url = %url,
            "stored StructureDefinition matches an embedded core definition; not registered \
             (set HFS_VALIDATION_SHADOW_CORE=true to override core definitions)"
        );
        return None;
    }
    match helios_fhir_validator::converter::convert(sd) {
        Ok(conversion) => {
            for w in &conversion.warnings {
                warn!(tenant = %tenant, "profile conversion warning: {w}");
            }
            Some(conversion.schema)
        }
        Err(e) => {
            warn!(tenant = %tenant, "stored StructureDefinition failed to convert: {e}");
            None
        }
    }
}

/// [`prepare_stored_profile`] over a page of rows, spread across threads
/// (blocking context). Output order follows input order.
fn prepare_stored_profiles(
    shadow_core: bool,
    tenant: &str,
    items: Vec<(FhirVersion, Value)>,
) -> Vec<(FhirVersion, Value, Option<helios_fhir_validator::FhirSchema>)> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 16)
        .min(items.len().max(1));
    if threads <= 1 {
        return items
            .into_iter()
            .map(|(v, sd)| {
                let schema = prepare_stored_profile(shadow_core, tenant, v, &sd);
                (v, sd, schema)
            })
            .collect();
    }
    let chunk = items.len().div_ceil(threads);
    let mut chunks: Vec<Vec<(FhirVersion, Value)>> = Vec::with_capacity(threads);
    let mut rest = items;
    while !rest.is_empty() {
        let tail = rest.split_off(chunk.min(rest.len()));
        chunks.push(rest);
        rest = tail;
    }
    std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|part| {
                scope.spawn(move || {
                    part.into_iter()
                        .map(|(v, sd)| {
                            let schema = prepare_stored_profile(shadow_core, tenant, v, &sd);
                            (v, sd, schema)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("profile conversion thread"))
            .collect()
    })
}

// ---------------------------------------------------------------------
// Remote terminology provider
// ---------------------------------------------------------------------

/// `TerminologyProvider` backed by a FHIR terminology server's
/// `ValueSet/$validate-code`, with a small in-memory TTL cache (neither the
/// fhirpath nor the search terminology clients cache).
pub struct RemoteTerminologyProvider {
    base_url: String,
    client: reqwest::Client,
    /// Check key (value set + coded value, or code system + coding) →
    /// verdict, cached for [`CACHE_TTL`].
    cache: DashMap<String, (TxVerdict, Instant)>,
    /// ValueSets the server answered "not found" for, cached for
    /// [`UNKNOWN_VALUE_SET_TTL`] so one missing IG value set costs one
    /// round-trip per TTL instead of one per coded element.
    unknown_value_sets: DashMap<String, Instant>,
    /// Code system url → whether the server has no content to check codes
    /// against (absent, or only `content: not-present` stubs).
    contentless_systems: DashMap<String, (bool, Instant)>,
    /// ValueSet canonical → its `compose` (`Value::Null` when the server
    /// does not return one).
    value_set_composes: DashMap<String, (Value, Instant)>,
    /// Check key → "cannot be checked" message (a negative answer over a
    /// code system without content), cached like verdicts so a resource
    /// using such a system does not cost a round trip on every validation.
    unverifiable: DashMap<String, (String, Instant)>,
}

/// How long `$validate-code` verdicts are cached.
const CACHE_TTL: Duration = Duration::from_secs(300);
/// How long a "ValueSet not found" answer is remembered.
const UNKNOWN_VALUE_SET_TTL: Duration = Duration::from_secs(60);

/// Splits a canonical `url|version` reference into its parts. HL7's own
/// StructureDefinitions bind to versioned canonicals
/// (`…/ValueSet/observation-status|4.0.1`); a server matching `url` literally
/// would never find them.
pub fn split_canonical(reference: &str) -> (&str, Option<&str>) {
    match reference.split_once('|') {
        Some((url, version)) if !version.is_empty() => (url, Some(version)),
        Some((url, _)) => (url, None),
        None => (reference, None),
    }
}

impl RemoteTerminologyProvider {
    /// `base_url` is the terminology server root (e.g. `http://hts:8090`).
    pub fn new(base_url: String, timeout: Duration) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("reqwest client builds"),
            cache: DashMap::new(),
            unknown_value_sets: DashMap::new(),
            contentless_systems: DashMap::new(),
            value_set_composes: DashMap::new(),
            unverifiable: DashMap::new(),
        }
    }

    /// The code systems a coded value names (none for a bare `code`).
    fn coded_systems(coded: &CodedValue) -> Vec<String> {
        let system = |c: &Value| c.get("system").and_then(Value::as_str).map(str::to_string);
        match coded {
            CodedValue::Code(_) => Vec::new(),
            CodedValue::Coding(coding) => system(coding).into_iter().collect(),
            CodedValue::CodeableConcept(concept) => concept
                .get("coding")
                .and_then(Value::as_array)
                .map(|codings| codings.iter().filter_map(system).collect())
                .unwrap_or_default(),
        }
    }

    /// The canonical a `$validate-code` answer blames on an unknown code
    /// system (`x-caused-by-unknown-system` / `x-unknown-system`).
    fn caused_by_unknown_system(body: &Value) -> Option<String> {
        body.get("parameter")?.as_array()?.iter().find_map(|p| {
            let name = p.get("name")?.as_str()?;
            if name != "x-caused-by-unknown-system" && name != "x-unknown-system" {
                return None;
            }
            p.get("valueCanonical")
                .or_else(|| p.get("valueUri"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
    }

    /// Whether every CodeSystem the server holds for `system` is a
    /// `content: not-present` stub (or there is none). `None` when the server
    /// could not be asked — the caller then keeps the server's verdict.
    async fn system_has_no_content(&self, system: &str) -> Option<bool> {
        if let Some(entry) = self.contentless_systems.get(system)
            && entry.1.elapsed() < CACHE_TTL
        {
            return Some(entry.0);
        }
        let url = format!("{}/CodeSystem", self.base_url);
        let response = self
            .client
            .get(&url)
            .query(&[
                ("url", system),
                ("_elements", "url,version,content"),
                ("_count", "50"),
            ])
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let bundle: Value = response.json().await.ok()?;
        if bundle.get("resourceType").and_then(Value::as_str) != Some("Bundle") {
            return None;
        }
        // A searchset may omit `entry` when nothing matched: no CodeSystem.
        let entries = bundle
            .get("entry")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let contentless = entries
            .iter()
            .all(|e| e.pointer("/resource/content").and_then(Value::as_str) == Some("not-present"));
        self.contentless_systems
            .insert(system.to_string(), (contentless, Instant::now()));
        Some(contentless)
    }

    /// The `compose` of `value_set` (`url|version` honoured when the server
    /// returns several versions). `None` when the server could not be asked.
    async fn value_set_compose(&self, value_set: &str) -> Option<Value> {
        if let Some(entry) = self.value_set_composes.get(value_set)
            && entry.1.elapsed() < CACHE_TTL
        {
            return Some(entry.0.clone());
        }
        let (canonical, version) = split_canonical(value_set);
        let url = format!("{}/ValueSet", self.base_url);
        let response = self
            .client
            .get(&url)
            .query(&[("url", canonical), ("_count", "50")])
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let bundle: Value = response.json().await.ok()?;
        let entries = bundle.get("entry").and_then(Value::as_array)?;
        let pick = entries
            .iter()
            .find(|e| {
                version.is_some()
                    && e.pointer("/resource/version").and_then(Value::as_str) == version
            })
            .or_else(|| entries.first());
        let compose = pick
            .and_then(|e| e.pointer("/resource/compose"))
            .cloned()
            .unwrap_or(Value::Null);
        self.value_set_composes
            .insert(value_set.to_string(), (compose.clone(), Instant::now()));
        Some(compose)
    }

    /// After a negative `$validate-code`: the code system to blame when the
    /// "not in the value set" answer only reflects that the server cannot
    /// check the code at all — the server said so (`x-caused-by-unknown-system`),
    /// or every code system of the coded value has no content on the server
    /// while the value set takes that system wholesale or by filter (an
    /// enumerated `include.concept` list is still a real membership answer).
    async fn unverifiable_system(
        &self,
        value_set: &str,
        coded: &CodedValue,
        body: &Value,
    ) -> Option<String> {
        if let Some(system) = Self::caused_by_unknown_system(body) {
            return Some(system);
        }
        let systems = Self::coded_systems(coded);
        if systems.is_empty() {
            return None;
        }
        for system in &systems {
            if !self.system_has_no_content(system).await? {
                return None;
            }
        }
        let compose = self.value_set_compose(value_set).await?;
        let includes = compose.get("include").and_then(Value::as_array)?;
        let unenumerated = |system: &str| {
            includes.iter().any(|inc| {
                inc.get("system").and_then(Value::as_str) == Some(system)
                    && inc
                        .get("concept")
                        .and_then(Value::as_array)
                        .is_none_or(|c| c.is_empty())
            })
        };
        systems
            .iter()
            .all(|s| unenumerated(s))
            .then(|| systems[0].clone())
    }

    /// The `$validate-code` Parameters for one check: `url` (version
    /// stripped) plus `valueSetVersion` when the binding named one, and the
    /// coded value in its own shape.
    fn payload(value_set: &str, coded: &CodedValue) -> Value {
        let (url, version) = split_canonical(value_set);
        let mut parameter = vec![json!({ "name": "url", "valueUri": url })];
        if let Some(version) = version {
            parameter.push(json!({ "name": "valueSetVersion", "valueString": version }));
        }
        match coded {
            CodedValue::Code(code) => {
                parameter.push(json!({ "name": "inferSystem", "valueBoolean": true }));
                parameter.push(json!({ "name": "code", "valueCode": code }));
            }
            CodedValue::Coding(coding) => {
                parameter.push(json!({ "name": "coding", "valueCoding": coding }));
            }
            CodedValue::CodeableConcept(concept) => {
                parameter
                    .push(json!({ "name": "codeableConcept", "valueCodeableConcept": concept }));
            }
        }
        json!({ "resourceType": "Parameters", "parameter": parameter })
    }

    /// Classifies a non-2xx answer: a `404`, or an OperationOutcome whose
    /// issue says `not-found`, means the server does not know the value set;
    /// anything else is treated as the service being unavailable.
    fn classify_failure(url: &str, status: reqwest::StatusCode, body: &str) -> TerminologyError {
        let message = format!("{url} returned {status}");
        if status == reqwest::StatusCode::NOT_FOUND {
            return TerminologyError::unknown_value_set(message);
        }
        if status.is_client_error()
            && let Ok(outcome) = serde_json::from_str::<Value>(body)
            && outcome
                .get("issue")
                .and_then(Value::as_array)
                .is_some_and(|issues| {
                    issues
                        .iter()
                        .any(|i| i.get("code").and_then(Value::as_str) == Some("not-found"))
                })
        {
            return TerminologyError::unknown_value_set(message);
        }
        TerminologyError::new(message)
    }
}

impl RemoteTerminologyProvider {
    fn cache_key(check: &TxCheck<'_>) -> String {
        match check {
            TxCheck::InValueSet(value_set, coded) => format!("{value_set}|{coded:?}"),
            TxCheck::Coding(coding) => format!(
                "cs|{}|{}|{}|{}",
                coding.get("system").and_then(Value::as_str).unwrap_or_default(),
                coding.get("version").and_then(Value::as_str).unwrap_or_default(),
                coding.get("code").and_then(Value::as_str).unwrap_or_default(),
                coding.get("display").and_then(Value::as_str).unwrap_or_default(),
            ),
        }
    }

    /// The operation and Parameters for one check.
    fn request(check: &TxCheck<'_>) -> (&'static str, Value) {
        match check {
            TxCheck::InValueSet(value_set, coded) => {
                ("ValueSet/$validate-code", Self::payload(value_set, coded))
            }
            TxCheck::Coding(coding) => ("CodeSystem/$validate-code", Self::coding_payload(coding)),
        }
    }

    /// `CodeSystem/$validate-code` Parameters for one Coding: the system as
    /// `url`, the code, and the display and version when the Coding has them.
    fn coding_payload(coding: &Value) -> Value {
        let text = |k: &str| coding.get(k).and_then(Value::as_str);
        let mut parameter = vec![
            json!({ "name": "url", "valueUri": text("system").unwrap_or_default() }),
            json!({ "name": "code", "valueCode": text("code").unwrap_or_default() }),
        ];
        if let Some(version) = text("version") {
            parameter.push(json!({ "name": "version", "valueString": version }));
        }
        if let Some(display) = text("display") {
            parameter.push(json!({ "name": "display", "valueString": display }));
        }
        json!({ "resourceType": "Parameters", "parameter": parameter })
    }

    /// A still-fresh cached verdict (or remembered unknown value set).
    fn cached(&self, check: &TxCheck<'_>, key: &str) -> Option<Result<TxVerdict, TerminologyError>> {
        if let Some(entry) = self.cache.get(key) {
            let (verdict, at) = &*entry;
            if at.elapsed() < CACHE_TTL {
                return Some(Ok(verdict.clone()));
            }
        }
        if let Some(entry) = self.unverifiable.get(key) {
            let (message, at) = &*entry;
            if at.elapsed() < CACHE_TTL {
                return Some(Err(TerminologyError::unknown_code_system(message.clone())));
            }
        }
        if let TxCheck::InValueSet(value_set, _) = check
            && let Some(at) = self.unknown_value_sets.get(*value_set)
            && at.elapsed() < UNKNOWN_VALUE_SET_TTL
        {
            return Some(Err(TerminologyError::unknown_value_set(format!(
                "value set '{value_set}' is not known to the terminology server (cached)"
            ))));
        }
        None
    }

    /// The `issues` OperationOutcome issues of a `$validate-code` answer.
    fn answer_issues(body: &Value) -> Vec<&Value> {
        body.get("parameter")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|p| p.get("name").and_then(Value::as_str) == Some("issues"))
            .filter_map(|p| p.pointer("/resource/issue").and_then(Value::as_array))
            .flatten()
            .collect()
    }

    /// The `tx-issue-type` code of an issue (`invalid-display`, `not-found`, ...).
    fn tx_issue_type(issue: &Value) -> Option<&str> {
        issue
            .pointer("/details/coding")
            .and_then(Value::as_array)?
            .iter()
            .find(|c| {
                c.get("system").and_then(Value::as_str)
                    == Some("http://hl7.org/fhir/tools/CodeSystem/tx-issue-type")
            })?
            .get("code")
            .and_then(Value::as_str)
    }

    fn is_display_issue(issue: &Value) -> bool {
        Self::tx_issue_type(issue) == Some("invalid-display")
    }

    /// Read the verdict out of a successful `$validate-code` answer.
    ///
    /// A wrong display never makes the code itself invalid: a `false` whose
    /// only error issues are `invalid-display` counts as valid (the HL7
    /// validator reports the display separately, as a display issue). For a
    /// Coding check the display issue text and an unknown code system are
    /// carried in the verdict.
    fn read_verdict(check: &TxCheck<'_>, body: &Value) -> Result<TxVerdict, TerminologyError> {
        let result = body
            .get("parameter")
            .and_then(Value::as_array)
            .and_then(|params| {
                params
                    .iter()
                    .find(|p| p.get("name").and_then(Value::as_str) == Some("result"))
            })
            .and_then(|p| p.get("valueBoolean"))
            .and_then(Value::as_bool)
            .ok_or_else(|| TerminologyError::new("no boolean 'result' parameter in response"))?;
        let issues = Self::answer_issues(body);
        let display_only = !result
            && issues.iter().any(|i| Self::is_display_issue(i))
            && issues
                .iter()
                .filter(|i| i.get("severity").and_then(Value::as_str) == Some("error"))
                .all(|i| Self::is_display_issue(i));
        match check {
            TxCheck::InValueSet(..) => Ok(TxVerdict::member(result || display_only)),
            TxCheck::Coding(_) => {
                let wrong_display = issues
                    .iter()
                    .find(|i| Self::is_display_issue(i))
                    .and_then(|i| i.pointer("/details/text").and_then(Value::as_str))
                    .map(str::to_string);
                let unknown_system = Self::caused_by_unknown_system(body).is_some()
                    || issues.iter().any(|i| {
                        i.pointer("/extension/0/valueString").and_then(Value::as_str)
                            == Some("UNKNOWN_CODESYSTEM")
                    });
                Ok(TxVerdict {
                    valid: true,
                    wrong_display,
                    unknown_system,
                })
            }
        }
    }

    /// Interpret one `$validate-code` answer (plain or batch entry) and
    /// cache what it says.
    fn settle(
        &self,
        check: &TxCheck<'_>,
        key: String,
        url: &str,
        status: reqwest::StatusCode,
        body: Option<&Value>,
        raw: &str,
    ) -> Result<TxVerdict, TerminologyError> {
        // A code-system check the server refuses (unknown version such as a
        // SNOMED US edition URI, malformed system ...) can produce no issue;
        // remember it as "nothing to report" so it is not asked again on
        // every validation.
        if matches!(check, TxCheck::Coding(_)) && status.is_client_error() {
            let verdict = TxVerdict::valid();
            self.cache.insert(key, (verdict.clone(), Instant::now()));
            return Ok(verdict);
        }
        if !status.is_success() {
            let error = Self::classify_failure(url, status, raw);
            if let TxCheck::InValueSet(value_set, _) = check
                && error.kind() == helios_fhir_validator::TerminologyErrorKind::UnknownValueSet
            {
                self.unknown_value_sets
                    .insert(value_set.to_string(), Instant::now());
            }
            return Err(error);
        }
        let verdict = Self::read_verdict(check, body.unwrap_or(&Value::Null))?;
        self.cache.insert(key, (verdict.clone(), Instant::now()));
        Ok(verdict)
    }

    /// One check, one request.
    async fn check_one(&self, check: &TxCheck<'_>) -> Result<TxVerdict, TerminologyError> {
        let key = Self::cache_key(check);
        if let Some(cached) = self.cached(check, &key) {
            return cached;
        }
        let (op, payload) = Self::request(check);
        let url = format!("{}/{op}", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| TerminologyError::new(format!("request to {url} failed: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return self.settle(check, key, &url, status, None, &body);
        }
        let body: Value = response
            .json()
            .await
            .map_err(|e| TerminologyError::new(format!("invalid $validate-code response: {e}")))?;
        let verdict = self.settle(check, key, &url, status, Some(&body), "");
        self.check_unverifiable(check, &body, verdict).await
    }

    /// Several uncached checks in one FHIR `batch` Bundle (`POST [base]`,
    /// one `POST ValueSet/$validate-code` or `CodeSystem/$validate-code`
    /// entry each). `None` when the server did not answer the batch as a
    /// whole, so the caller can fall back to single requests.
    async fn check_batch(
        &self,
        checks: &[TxCheck<'_>],
    ) -> Option<Vec<Result<TxVerdict, TerminologyError>>> {
        let entries: Vec<Value> = checks
            .iter()
            .map(|check| {
                let (op, payload) = Self::request(check);
                json!({
                    "request": {"method": "POST", "url": op},
                    "resource": payload,
                })
            })
            .collect();
        let bundle = json!({"resourceType": "Bundle", "type": "batch", "entry": entries});
        let url = format!("{}/", self.base_url);
        let response = self.client.post(&url).json(&bundle).send().await.ok()?;
        if !response.status().is_success() {
            debug!(status = %response.status(), "terminology server refused a batch; checking one by one");
            return None;
        }
        let body: Value = response.json().await.ok()?;
        let answers = body.get("entry").and_then(Value::as_array)?;
        if answers.len() != checks.len() {
            return None;
        }
        let mut out = Vec::with_capacity(checks.len());
        for (check, answer) in checks.iter().zip(answers) {
            let status = answer
                .pointer("/response/status")
                .and_then(Value::as_str)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|code| code.parse::<u16>().ok())
                .and_then(|code| reqwest::StatusCode::from_u16(code).ok())
                .unwrap_or(reqwest::StatusCode::BAD_GATEWAY);
            let resource = answer.get("resource");
            let raw = resource.map(Value::to_string).unwrap_or_default();
            let op_url = format!("{}/{}", self.base_url, Self::request(check).0);
            let verdict = self.settle(
                check,
                Self::cache_key(check),
                &op_url,
                status,
                resource,
                &raw,
            );
            // A `false` may only mean the code system has no content on the
            // server; that check stays per verdict (rare: only on failures).
            let body = resource.cloned().unwrap_or(Value::Null);
            out.push(self.check_unverifiable(check, &body, verdict).await);
        }
        Some(out)
    }

    /// N's rule: a negative membership verdict over a code system the
    /// server has no content for is "cannot check", not "invalid" (and is
    /// not cached).
    async fn check_unverifiable(
        &self,
        check: &TxCheck<'_>,
        body: &Value,
        verdict: Result<TxVerdict, TerminologyError>,
    ) -> Result<TxVerdict, TerminologyError> {
        let TxCheck::InValueSet(value_set, coded) = check else {
            return verdict;
        };
        if matches!(&verdict, Ok(v) if !v.valid)
            && let Some(system) = self.unverifiable_system(value_set, coded, body).await
        {
            let key = Self::cache_key(check);
            self.cache.remove(&key);
            let message = format!(
                "code system '{system}' has no content on the terminology server \
                 (unknown, or a content=not-present stub), so the code cannot be checked"
            );
            self.unverifiable
                .insert(key, (message.clone(), Instant::now()));
            return Err(TerminologyError::unknown_code_system(message));
        }
        verdict
    }
}

#[async_trait]
impl TerminologyProvider for RemoteTerminologyProvider {
    async fn validate_code(
        &self,
        value_set: &str,
        coded: &CodedValue,
    ) -> Result<bool, TerminologyError> {
        self.check_one(&TxCheck::InValueSet(value_set, coded))
            .await
            .map(|v| v.valid)
    }

    async fn validate_codes(
        &self,
        checks: &[(&str, &CodedValue)],
    ) -> Vec<Result<bool, TerminologyError>> {
        let checks: Vec<TxCheck<'_>> = checks
            .iter()
            .map(|(value_set, coded)| TxCheck::InValueSet(value_set, coded))
            .collect();
        self.check(&checks)
            .await
            .into_iter()
            .map(|v| v.map(|v| v.valid))
            .collect()
    }

    /// One round trip per resource: cached verdicts answer locally,
    /// duplicates are asked once, the rest go out as one batch Bundle
    /// (single requests, concurrently, if the server rejects batches).
    async fn check(&self, checks: &[TxCheck<'_>]) -> Vec<Result<TxVerdict, TerminologyError>> {
        let mut out: Vec<Option<Result<TxVerdict, TerminologyError>>> = vec![None; checks.len()];
        // Distinct uncached checks, each with the positions it answers.
        let mut pending: Vec<(TxCheck<'_>, Vec<usize>)> = Vec::new();
        let mut by_key: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (i, check) in checks.iter().enumerate() {
            let key = Self::cache_key(check);
            if let Some(cached) = self.cached(check, &key) {
                out[i] = Some(cached);
                continue;
            }
            match by_key.get(&key) {
                Some(&slot) => pending[slot].1.push(i),
                None => {
                    by_key.insert(key, pending.len());
                    pending.push((*check, vec![i]));
                }
            }
        }
        if !pending.is_empty() {
            let requests: Vec<TxCheck<'_>> = pending.iter().map(|(c, _)| *c).collect();
            let verdicts = match requests.len() {
                1 => None,
                _ => self.check_batch(&requests).await,
            };
            let verdicts = match verdicts {
                Some(v) => v,
                None => {
                    futures::future::join_all(requests.iter().map(|c| self.check_one(c))).await
                }
            };
            for ((_, positions), verdict) in pending.iter().zip(verdicts) {
                for &i in positions {
                    out[i] = Some(verdict.clone());
                }
            }
        }
        out.into_iter()
            .map(|v| v.unwrap_or_else(|| Err(TerminologyError::new("no verdict"))))
            .collect()
    }
}

// ---------------------------------------------------------------------
// OperationOutcome mapping
// ---------------------------------------------------------------------

/// Map one validator issue onto an OperationOutcome issue.
pub fn to_outcome_issue(error: &ValidationError) -> Issue {
    let code = match error.kind {
        ErrorKind::Required => IssueType::Required,
        ErrorKind::FixedValue | ErrorKind::PatternValue | ErrorKind::PrimitiveValue => {
            IssueType::Value
        }
        ErrorKind::FhirpathConstraint => IssueType::Invariant,
        // A known extension outside its declared context (#615): invalid
        // content by placement, not a structural malformation.
        ErrorKind::ExtensionContext => IssueType::Invalid,
        // An HL7-namespace extension nobody defines.
        ErrorKind::UnknownExtension => IssueType::Structure,
        // A batch/transaction entry filed under the wrong resource type.
        ErrorKind::BundleEntryMismatch => IssueType::Invalid,
        // A malformed or wrongly-typed reference.
        ErrorKind::ReferenceInvalid => IssueType::Invalid,
        // A Bundle-internal URN reference no entry provides.
        ErrorKind::ReferenceUnresolved => IssueType::NotFound,
        ErrorKind::TerminologyBinding | ErrorKind::DisplayMismatch => IssueType::CodeInvalid,
        // An HL7-namespace code system nobody defines, a narrative link to
        // nothing, a best-practice recommendation: invalid content.
        ErrorKind::UnknownCodeSystem | ErrorKind::Narrative | ErrorKind::BestPractice => {
            IssueType::Invalid
        }
        ErrorKind::UnknownSchema | ErrorKind::UnknownProfile => IssueType::NotSupported,
        // Everything structural: unknown-element, shape, cardinality,
        // slicing, choices, wrong container type.
        ErrorKind::UnknownElement
        | ErrorKind::NotArray
        | ErrorKind::NotSingular
        | ErrorKind::Type
        | ErrorKind::Excluded
        | ErrorKind::Min
        | ErrorKind::Max
        | ErrorKind::SliceCardinality
        | ErrorKind::SliceUnmatched
        | ErrorKind::SliceOrder
        | ErrorKind::Choice
        | ErrorKind::ChoiceExcluded
        | ErrorKind::EmptyObject => IssueType::Structure,
    };
    let severity = match error.severity {
        Severity::Error => IssueSeverity::Error,
        Severity::Warning => IssueSeverity::Warning,
    };
    // The profile behind the rule, as the HL7 validator names it.
    let text = error.hl7_message.as_deref().unwrap_or(&error.message);
    let message = match &error.source {
        Some(source) => format!("{text} (from {source})"),
        None => text.to_string(),
    };
    Issue::new(severity, code, message).with_expression(dotted_to_fhirpath(&error.path))
}

/// Build the `$validate` OperationOutcome: the mapped issues, or the
/// canonical all-clear information issue when there are none.
pub fn validation_outcome(errors: &[ValidationError]) -> Value {
    let mut builder = OperationOutcomeBuilder::new();
    if errors.is_empty() {
        builder = builder.information(IssueType::Informational, "Validation successful");
    } else {
        for error in errors {
            builder = builder.add_issue(to_outcome_issue(error));
        }
    }
    builder.build()
}

/// Whether any issue is error severity (drives `$validate` reporting and
/// enforce-mode rejection).
pub fn has_errors(errors: &[ValidationError]) -> bool {
    errors.iter().any(|e| e.severity == Severity::Error)
}

#[cfg(test)]
mod remote_provider_tests {
    use super::*;

    fn names_and_values(payload: &Value) -> Vec<(String, Value)> {
        payload["parameter"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                let name = p["name"].as_str().unwrap().to_string();
                let value = p
                    .as_object()
                    .unwrap()
                    .iter()
                    .find(|(k, _)| k.starts_with("value"))
                    .map(|(_, v)| v.clone())
                    .unwrap();
                (name, value)
            })
            .collect()
    }

    #[test]
    fn split_canonical_separates_url_and_version() {
        assert_eq!(
            split_canonical("http://hl7.org/fhir/ValueSet/observation-status|4.0.1"),
            (
                "http://hl7.org/fhir/ValueSet/observation-status",
                Some("4.0.1")
            )
        );
        assert_eq!(
            split_canonical("http://hl7.org/fhir/ValueSet/observation-status"),
            ("http://hl7.org/fhir/ValueSet/observation-status", None)
        );
        assert_eq!(split_canonical("http://x/vs|"), ("http://x/vs", None));
    }

    #[test]
    fn payload_sends_version_as_value_set_version() {
        let payload = RemoteTerminologyProvider::payload(
            "http://hl7.org/fhir/ValueSet/observation-status|4.0.1",
            &CodedValue::Code("final".into()),
        );
        assert_eq!(
            names_and_values(&payload),
            vec![
                (
                    "url".into(),
                    json!("http://hl7.org/fhir/ValueSet/observation-status")
                ),
                ("valueSetVersion".into(), json!("4.0.1")),
                ("inferSystem".into(), json!(true)),
                ("code".into(), json!("final")),
            ]
        );

        let payload = RemoteTerminologyProvider::payload(
            "http://example.org/ValueSet/plain",
            &CodedValue::Coding(json!({ "system": "http://s", "code": "c" })),
        );
        assert_eq!(
            names_and_values(&payload),
            vec![
                ("url".into(), json!("http://example.org/ValueSet/plain")),
                (
                    "coding".into(),
                    json!({ "system": "http://s", "code": "c" })
                ),
            ]
        );
    }

    fn answer(result: bool, issues: Vec<Value>, extra: Vec<Value>) -> Value {
        let mut parameter = vec![json!({"name": "result", "valueBoolean": result})];
        if !issues.is_empty() {
            parameter.push(json!({"name": "issues", "resource": {
                "resourceType": "OperationOutcome", "issue": issues}}));
        }
        parameter.extend(extra);
        json!({"resourceType": "Parameters", "parameter": parameter})
    }

    fn issue(severity: &str, tx_type: &str, text: &str, message_id: Option<&str>) -> Value {
        let mut i = json!({"severity": severity, "code": "invalid",
            "details": {"coding": [{"system": "http://hl7.org/fhir/tools/CodeSystem/tx-issue-type",
                "code": tx_type}], "text": text}});
        if let Some(id) = message_id {
            i["extension"] = json!([{"url": "http://hl7.org/fhir/StructureDefinition/operationoutcome-message-id",
                "valueString": id}]);
        }
        i
    }

    /// A `false` whose only error is a wrong display is a valid code — the
    /// NGS specimen types (`LP7057-5` "Bld") must not fail their binding.
    #[test]
    fn a_wrong_display_never_makes_a_code_invalid() {
        let coded = CodedValue::Coding(json!({"system": "http://loinc.org", "code": "LP7057-5", "display": "Bld"}));
        let vs = TxCheck::InValueSet("http://example.org/vs", &coded);
        let display = issue("error", "invalid-display", "Wrong Display Name 'Bld'", None);
        let verdict = RemoteTerminologyProvider::read_verdict(
            &vs,
            &answer(false, vec![display.clone()], vec![]),
        )
        .unwrap();
        assert!(verdict.valid);

        // Another error next to the display one: really not in the set.
        let not_in = issue("error", "not-in-vs", "not in the value set", None);
        let verdict = RemoteTerminologyProvider::read_verdict(
            &vs,
            &answer(false, vec![display.clone(), not_in], vec![]),
        )
        .unwrap();
        assert!(!verdict.valid);

        // A plain `false` without issues stays invalid.
        let verdict =
            RemoteTerminologyProvider::read_verdict(&vs, &answer(false, vec![], vec![])).unwrap();
        assert!(!verdict.valid);
    }

    #[test]
    fn coding_checks_carry_display_and_unknown_system() {
        let coding = json!({"system": "http://loinc.org", "code": "787-2", "display": "MCV"});
        let check = TxCheck::Coding(&coding);
        let text = "Wrong Display Name 'MCV' for http://loinc.org#787-2. Valid display is 'MCV [Entitic mean volume] in Red Blood Cells by Automated count'";
        let verdict = RemoteTerminologyProvider::read_verdict(
            &check,
            &answer(false, vec![issue("error", "invalid-display", text, Some("Display_Name_for__should_be_one_of__instead_of"))], vec![]),
        )
        .unwrap();
        assert!(verdict.valid);
        assert_eq!(verdict.wrong_display.as_deref(), Some(text));
        assert!(!verdict.unknown_system);

        let unknown = issue(
            "error",
            "not-found",
            "A definition for CodeSystem 'http://terminology.hl7.org/CodeSystem/v3-ActEncounterCode' could not be found",
            Some("UNKNOWN_CODESYSTEM"),
        );
        let verdict =
            RemoteTerminologyProvider::read_verdict(&check, &answer(false, vec![unknown], vec![]))
                .unwrap();
        assert!(verdict.unknown_system);
        assert!(verdict.wrong_display.is_none());

        let verdict = RemoteTerminologyProvider::read_verdict(
            &check,
            &answer(false, vec![], vec![json!({"name": "x-unknown-system", "valueCanonical": "http://x"})]),
        )
        .unwrap();
        assert!(verdict.unknown_system);

        let payload = RemoteTerminologyProvider::coding_payload(&coding);
        assert_eq!(
            names_and_values(&payload),
            vec![
                ("url".into(), json!("http://loinc.org")),
                ("code".into(), json!("787-2")),
                ("display".into(), json!("MCV")),
            ]
        );
    }

    #[test]
    fn failures_are_classified() {
        use helios_fhir_validator::TerminologyErrorKind;
        let url = "http://hts/ValueSet/$validate-code";
        assert_eq!(
            RemoteTerminologyProvider::classify_failure(url, reqwest::StatusCode::NOT_FOUND, "")
                .kind(),
            TerminologyErrorKind::UnknownValueSet
        );
        let outcome = json!({
            "resourceType": "OperationOutcome",
            "issue": [{ "severity": "error", "code": "not-found" }]
        })
        .to_string();
        assert_eq!(
            RemoteTerminologyProvider::classify_failure(
                url,
                reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                &outcome
            )
            .kind(),
            TerminologyErrorKind::UnknownValueSet
        );
        assert_eq!(
            RemoteTerminologyProvider::classify_failure(
                url,
                reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                &outcome
            )
            .kind(),
            TerminologyErrorKind::Unavailable
        );
        assert_eq!(
            RemoteTerminologyProvider::classify_failure(
                url,
                reqwest::StatusCode::BAD_REQUEST,
                "not json"
            )
            .kind(),
            TerminologyErrorKind::Unavailable
        );
    }
}
