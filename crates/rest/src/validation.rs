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
    CodedValue, CompositeResolver, EffectHandlers, ErrorKind, SchemaRegistry, SchemaResolver,
    Severity, TerminologyError, TerminologyProvider, UnknownProfilePolicy, ValidationError,
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
    /// Escalate terminology-service failures to errors.
    terminology_fail_closed: bool,
    /// Validate against `meta.profile` claims.
    use_meta_profiles: bool,
    unknown_profile: UnknownProfilePolicy,
    /// Per-tenant profile overlays, fed from stored StructureDefinition
    /// writes. `None` disables the feature.
    tenant_profiles: Option<TenantProfileMap>,
    /// Allow stored StructureDefinitions to override embedded core
    /// definitions with the same canonical URL (`HFS_VALIDATION_SHADOW_CORE`).
    shadow_core: bool,
}

impl Default for ValidationService {
    fn default() -> Self {
        Self {
            mode: ValidationMode::Off,
            constraint_evaluator: Some(FhirPathConstraintEvaluator::new()),
            terminology: None,
            suppress_constraints: vec!["dom-6".to_string()],
            terminology_fail_closed: false,
            use_meta_profiles: true,
            unknown_profile: UnknownProfilePolicy::Warn,
            tenant_profiles: Some(DashMap::new()),
            shadow_core: false,
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
                ("remote", Some(base)) => Some(Arc::new(RemoteTerminologyProvider::new(
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
        Self {
            mode,
            constraint_evaluator: config.constraints.then(FhirPathConstraintEvaluator::new),
            terminology,
            suppress_constraints: config.suppress_constraints.clone(),
            terminology_fail_closed: config.terminology_fail == "closed",
            use_meta_profiles: config.meta_profiles,
            unknown_profile,
            tenant_profiles: config.stored_profiles.then(DashMap::new),
            shadow_core: config.shadow_core,
        }
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
        let core = helios_fhir_validator::packs::core_registry(version);
        let resolver: Arc<dyn SchemaResolver> = match self.tenant_overlay(tenant, version) {
            Some(overlay) => Arc::new(CompositeResolver::new(vec![overlay, core])),
            None => core,
        };
        let validator = Validator::new(resolver);
        let opts = ValidationOptions {
            profiles,
            use_meta_profiles: self.use_meta_profiles,
            unknown_profile: self.unknown_profile,
            ..Default::default()
        };
        let handlers = EffectHandlers {
            constraints: self
                .constraint_evaluator
                .as_ref()
                .map(|e| e as &dyn helios_fhir_validator::ConstraintEvaluator),
            terminology: self.terminology.as_deref(),
            suppress_constraints: &self.suppress_constraints,
            terminology_fail_closed: self.terminology_fail_closed,
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
        let Some(registries) = &self.tenant_profiles else {
            return false;
        };
        let core = helios_fhir_validator::packs::core_registry(version);
        let url = sd.get("url").and_then(Value::as_str);
        if !self.shadow_core
            && let Some(url) = url
            && core.resolve(url).is_some()
        {
            debug!(
                tenant = %tenant,
                url = %url,
                "stored StructureDefinition matches an embedded core definition; not registered \
                 (set HFS_VALIDATION_SHADOW_CORE=true to override core definitions)"
            );
            return false;
        }
        match helios_fhir_validator::converter::convert(sd) {
            Ok(conversion) => {
                for w in &conversion.warnings {
                    warn!(tenant = %tenant, "profile conversion warning: {w}");
                }
                let schema = conversion.schema;
                let is_constraint = schema.derivation.as_deref() == Some("constraint")
                    || schema.kind.as_deref()
                        == Some(helios_fhir_validator::schema::kind::EXTENSION);
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
            Err(e) => {
                warn!(tenant = %tenant, "stored StructureDefinition failed to convert: {e}");
                false
            }
        }
    }

    /// Drop a stored StructureDefinition from the tenant's profile registry
    /// (called after a successful StructureDefinition delete). Returns `true`
    /// if a definition under `url` was registered.
    pub fn remove_stored_profile(&self, tenant: &str, version: FhirVersion, url: &str) -> bool {
        let Some(registries) = &self.tenant_profiles else {
            return false;
        };
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
    /// Pages through storage with [`ExportDataProvider::fetch_export_batch`]
    /// (the one enumeration primitive every backend implements, deleted
    /// resources already filtered out) for the default tenant plus every
    /// registered tenant, then reads each page back through
    /// [`ResourceStorage::read_batch`] so the definition is registered under
    /// the FHIR version it was stored with — exactly as the write path did.
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
            return report;
        }
        let batch_size = batch_size.max(1);

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

        report.elapsed = started.elapsed();
        if report.storage_errors == 0 {
            info!(
                tenants = report.tenants,
                registered = report.registered,
                skipped = report.skipped,
                elapsed_ms = report.elapsed.as_millis() as u64,
                "stored StructureDefinitions rehydrated into the profile registry"
            );
        } else {
            warn!(
                tenants = report.tenants,
                registered = report.registered,
                skipped = report.skipped,
                storage_errors = report.storage_errors,
                elapsed_ms = report.elapsed.as_millis() as u64,
                "profile rehydration incomplete; some stored profiles may not resolve until re-written"
            );
        }
        report
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
        const TYPE: &str = "StructureDefinition";
        /// Just enough of an exported line to address the row again.
        #[derive(serde::Deserialize)]
        struct IdOnly {
            id: String,
        }

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
            let ids: Vec<String> = batch
                .lines
                .iter()
                .filter_map(|line| serde_json::from_str::<IdOnly>(line).ok().map(|r| r.id))
                .collect();
            if !ids.is_empty() {
                let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
                match storage.read_batch(&tenant, TYPE, &id_refs).await {
                    Ok(stored) => {
                        for sd in &stored {
                            if self.upsert_stored_profile(
                                tenant_id,
                                sd.fhir_version(),
                                sd.content(),
                            ) {
                                report.registered += 1;
                            } else {
                                report.skipped += 1;
                            }
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

// ---------------------------------------------------------------------
// Remote terminology provider
// ---------------------------------------------------------------------

/// `TerminologyProvider` backed by a FHIR terminology server's
/// `ValueSet/$validate-code`, with a small in-memory TTL cache (neither the
/// fhirpath nor the search terminology clients cache).
pub struct RemoteTerminologyProvider {
    base_url: String,
    client: reqwest::Client,
    /// `(valueSet, coded-token)` → validity, cached for [`CACHE_TTL`].
    cache: DashMap<String, (bool, Instant)>,
}

/// How long `$validate-code` verdicts are cached.
const CACHE_TTL: Duration = Duration::from_secs(300);

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
        }
    }

    fn payload(value_set: &str, coded: &CodedValue) -> Value {
        let mut parameter = vec![json!({ "name": "url", "valueUri": value_set })];
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
}

#[async_trait]
impl TerminologyProvider for RemoteTerminologyProvider {
    async fn validate_code(
        &self,
        value_set: &str,
        coded: &CodedValue,
    ) -> Result<bool, TerminologyError> {
        let key = format!("{value_set}|{coded:?}");
        if let Some(entry) = self.cache.get(&key) {
            let (verdict, at) = *entry;
            if at.elapsed() < CACHE_TTL {
                return Ok(verdict);
            }
        }

        let url = format!("{}/ValueSet/$validate-code", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(&Self::payload(value_set, coded))
            .send()
            .await
            .map_err(|e| TerminologyError(format!("request to {url} failed: {e}")))?;
        if !response.status().is_success() {
            return Err(TerminologyError(format!(
                "{url} returned {}",
                response.status()
            )));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|e| TerminologyError(format!("invalid $validate-code response: {e}")))?;
        let verdict = body
            .get("parameter")
            .and_then(Value::as_array)
            .and_then(|params| {
                params
                    .iter()
                    .find(|p| p.get("name").and_then(Value::as_str) == Some("result"))
            })
            .and_then(|p| p.get("valueBoolean"))
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                TerminologyError("no boolean 'result' parameter in response".to_string())
            })?;

        self.cache.insert(key, (verdict, Instant::now()));
        Ok(verdict)
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
        ErrorKind::TerminologyBinding => IssueType::CodeInvalid,
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
        | ErrorKind::ChoiceExcluded => IssueType::Structure,
    };
    let severity = match error.severity {
        Severity::Error => IssueSeverity::Error,
        Severity::Warning => IssueSeverity::Warning,
    };
    Issue::new(severity, code, error.message.clone())
        .with_expression(dotted_to_fhirpath(&error.path))
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
