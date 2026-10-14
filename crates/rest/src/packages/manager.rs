//! The package manager: plans, installs and removes FHIR packages.
//!
//! # Where resources go
//!
//! | Resources | Target |
//! |---|---|
//! | CodeSystem, ValueSet, ConceptMap | the terminology server's `/import` when `HFS_TERMINOLOGY_SERVER` is set (option `terminology: hfs` keeps them here, `both` writes both), else this server |
//! | everything else (StructureDefinition, SearchParameter, CapabilityStatement, OperationDefinition, ImplementationGuide, Questionnaire, NamingSystem, StructureMap, …) | this server's storage |
//! | examples (`package/example/`, or IG-declared) | skipped unless `includeExamples` |
//!
//! StructureDefinitions are registered in the validator's profile registry
//! as they are written (and replayed from storage at every start, like any
//! stored profile), so `$validate` against an installed profile works
//! immediately and after a restart. SearchParameters become active through
//! storage's own registry hook; existing data needs `$reindex` to be
//! searchable by them, which the install report says.
//!
//! # Ownership
//!
//! Every written resource is recorded against the package
//! ([`PackageStore`]). A key another package also wrote is *shared*; a key
//! that existed before any package touched it is *pre-existing* — except
//! that on this server a stored resource with the package resource's own
//! canonical url and version is the same artifact (loaded by hand earlier)
//! and is adopted, while on the terminology server anything already loaded
//! under the same url|version stays pre-existing. Uninstall
//! deletes only keys that are neither, unregisters deleted profiles (and
//! re-registers another package's profile with the same canonical), and
//! deletes the package's terminology from the terminology server.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use futures::StreamExt;
use helios_audit::{AuditAction, AuditEventBuilder, AuditSink};
use helios_fhir::FhirVersion;
use helios_persistence::core::{
    OwnedPackageResource, PackageRecord, PackageResourceRecord, PackageStore, ResourceStorage,
    ResourceWrite, TARGET_HFS, TARGET_HTS, WriteEvent, WriteObserver, WriteObservers,
};
use helios_persistence::error::{ResourceError, StorageError};
use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use super::hts::HtsClient;
use super::jobs::{Job, JobKind, JobStatus, JobTable, JobView};
use super::registry::{Registry, RegistrySource};
use super::resolver::{InstallMode, NodeAction, Plan, PlanRequest, resolve};
use super::tarball::{Limits, Manifest, PackageEntry, ParsedPackage, parse_package};
use super::version::{DependencyPolicy, Range};
use crate::config::{PackageConfig, ServerConfig};
use crate::validation::ValidationService;

/// Terminology resource types (routed to the terminology server).
const TERMINOLOGY_TYPES: &[&str] = &["CodeSystem", "ValueSet", "ConceptMap"];
/// Ownership rows buffered before a registry write.
const ROW_FLUSH: usize = 250;
/// Resources per terminology `/import` call.
const HTS_CHUNK_RESOURCES: usize = 200;
/// Bytes per terminology `/import` call.
const HTS_CHUNK_BYTES: usize = 8 * 1024 * 1024;
/// Conflicts listed per package in a plan report.
const MAX_CONFLICTS: usize = 200;

/// Where terminology resources are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TerminologyTarget {
    /// The terminology server when configured, else this server.
    #[default]
    Auto,
    /// Always this server.
    Hfs,
    /// The terminology server and this server.
    Both,
}

/// Install options.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InstallOptions {
    /// Also write example resources.
    pub include_examples: bool,
    /// Resolve and install dependencies.
    pub include_dependencies: bool,
    /// Upgrade (replace installed versions) or side by side.
    pub mode: InstallMode,
    /// How dependency versions are matched.
    pub dependency_policy: DependencyPolicy,
    /// Dependencies not to install.
    pub skip: Vec<String>,
    /// Where terminology goes.
    pub terminology: TerminologyTarget,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            include_examples: false,
            include_dependencies: true,
            mode: InstallMode::Upgrade,
            dependency_policy: DependencyPolicy::Newer,
            skip: Vec::new(),
            terminology: TerminologyTarget::Auto,
        }
    }
}

/// `POST /admin/packages` body.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallRequest {
    /// `name#version`, or just `name` for the latest.
    #[serde(default)]
    pub package: Option<String>,
    /// Package name (alternative to `package`).
    #[serde(default)]
    pub name: Option<String>,
    /// Version (alternative to `package`).
    #[serde(default)]
    pub version: Option<String>,
    /// Plan only.
    #[serde(default)]
    pub dry_run: bool,
    /// Options.
    #[serde(default)]
    pub options: InstallOptions,
}

impl InstallRequest {
    /// `(name, version)` from either form.
    pub fn target(&self) -> Result<(String, Option<String>), String> {
        if let Some(spec) = self.package.as_deref().filter(|s| !s.trim().is_empty()) {
            return parse_spec(spec);
        }
        match self.name.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(name) => {
                let version = self
                    .version
                    .as_deref()
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string);
                check_spec(name.trim(), version.as_deref())?;
                Ok((name.trim().to_string(), version))
            }
            None => Err("name the package: `package: \"name#version\"` or `name`/`version`".into()),
        }
    }
}

/// Parses `name#version`, `name@version` or `name`.
pub fn parse_spec(spec: &str) -> Result<(String, Option<String>), String> {
    let spec = spec.trim();
    let (name, version) = match spec.split_once(['#', '@']) {
        Some((n, v)) => (n.trim(), Some(v.trim()).filter(|v| !v.is_empty())),
        None => (spec, None),
    };
    check_spec(name, version)?;
    Ok((name.to_string(), version.map(str::to_string)))
}

fn check_spec(name: &str, version: Option<&str>) -> Result<(), String> {
    if !super::tarball::valid_package_name(name) {
        return Err(format!("invalid package name '{name}'"));
    }
    if let Some(v) = version
        && !super::tarball::valid_package_version(v)
    {
        return Err(format!("invalid package version '{v}'"));
    }
    Ok(())
}

/// Why a request was refused before a job started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerError {
    /// Bad input (400).
    Invalid(String),
    /// Unknown package (404).
    NotFound(String),
    /// Refused because of state (409): dependents, a running job.
    Conflict(String),
    /// Storage or I/O failure (500).
    Internal(String),
}

impl std::fmt::Display for ManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) | Self::NotFound(m) | Self::Conflict(m) | Self::Internal(m) => {
                f.write_str(m)
            }
        }
    }
}

impl From<StorageError> for ManagerError {
    fn from(e: StorageError) -> Self {
        Self::Internal(e.to_string())
    }
}

/// Everything the manager needs from the server.
pub struct ManagerDeps {
    /// Resource storage.
    pub storage: Arc<dyn ResourceStorage>,
    /// Registry persistence.
    pub store: Arc<dyn PackageStore>,
    /// Validator (profile registry).
    pub validation: Arc<ValidationService>,
    /// Post-commit write observers (dashboard counters).
    pub observers: Arc<WriteObservers>,
    /// Audit sink, when auditing is on.
    pub audit: Option<Arc<dyn AuditSink>>,
    /// AuditEvent source observer.
    pub audit_source: String,
}

/// The package manager.
pub struct PackageManager {
    deps: ManagerDeps,
    registry: Registry,
    hts: Option<HtsClient>,
    fhir_version: FhirVersion,
    default_tenant: String,
    config: PackageConfig,
    jobs: JobTable,
    install_lock: tokio::sync::Mutex<()>,
}

/// The cache directory for a server configuration.
pub fn cache_dir(config: &ServerConfig) -> std::path::PathBuf {
    if let Some(dir) = &config.packages.cache_dir {
        return dir.clone();
    }
    if let Some(url) = config.database_url.as_deref()
        && config.storage_backend.starts_with("sqlite")
        && !url.contains("://")
        && url != ":memory:"
        && let Some(parent) = std::path::Path::new(url).parent()
    {
        let base = if parent.as_os_str().is_empty() {
            std::path::Path::new(".")
        } else {
            parent
        };
        return base.join("packages");
    }
    config
        .data_dir
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("./data"))
        .join("packages")
}

impl PackageManager {
    /// Builds the manager for a server configuration.
    pub fn new(config: &ServerConfig, deps: ManagerDeps) -> Self {
        let limits = Limits {
            max_compressed: config.packages.max_size,
            max_unpacked: config.packages.max_unpacked,
            max_entry: config.packages.max_entry,
            ..Limits::default()
        };
        let marketplace = config
            .packages
            .registry
            .is_online()
            .then(|| config.packages.marketplace_url.clone())
            .flatten();
        Self {
            registry: Registry::new(
                config.packages.registry.clone(),
                marketplace,
                cache_dir(config),
                limits,
            ),
            hts: config.terminology_server.as_deref().map(HtsClient::new),
            fhir_version: config.default_fhir_version,
            default_tenant: config.default_tenant.clone(),
            config: config.packages.clone(),
            deps,
            jobs: JobTable::default(),
            install_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// The registry client.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Whether the admin API and page are enabled (`HFS_ADMIN_UI`).
    pub fn admin_enabled(&self) -> bool {
        self.config.admin_ui
    }

    /// The default tenant.
    pub fn default_tenant(&self) -> &str {
        &self.default_tenant
    }

    /// The job table.
    pub fn jobs(&self) -> &JobTable {
        &self.jobs
    }

    /// The public configuration summary (`GET /admin/packages/config`).
    pub fn config_view(&self) -> Value {
        json!({
            "adminEnabled": self.config.admin_ui,
            "registry": self.registry.mode().label(),
            "registrySources": self.registry.mode().sources(),
            "online": self.registry.mode().is_online(),
            "marketplace": self.registry.marketplace_enabled(),
            "terminologyServer": self.hts.as_ref().map(|h| h.base().to_string()),
            "fhirVersion": self.fhir_version.as_str(),
            "maxSize": self.config.max_size,
            "maxUnpacked": self.config.max_unpacked,
            "startupPackages": self.config.startup,
            "cacheDir": self.registry.cache_dir().display().to_string(),
        })
    }

    fn release(&self) -> String {
        self.fhir_version.as_str().to_ascii_lowercase()
    }

    fn ctx(tenant: &str) -> TenantContext {
        TenantContext::new(TenantId::new(tenant), TenantPermissions::full_access())
    }

    // ---------------------------------------------------------------- reads

    /// Installed packages with their counts.
    pub async fn list(&self, tenant: &str) -> Result<Vec<Value>, ManagerError> {
        let packages = self.deps.store.list_packages(tenant).await?;
        let all = self.deps.store.package_resources(tenant, None).await?;
        let mut owned: HashMap<(String, String), usize> = HashMap::new();
        for r in &all {
            *owned
                .entry((r.package_name.clone(), r.package_version.clone()))
                .or_default() += 1;
        }
        let dependents = dependents_index(&packages);
        Ok(packages
            .iter()
            .map(|p| {
                let key = format!("{}#{}", p.name, p.version);
                package_json(
                    p,
                    owned
                        .get(&(p.name.clone(), p.version.clone()))
                        .copied()
                        .unwrap_or(0),
                    dependents.get(&key).cloned().unwrap_or_default(),
                )
            })
            .collect())
    }

    /// One package with its resources grouped by type.
    pub async fn detail(
        &self,
        tenant: &str,
        name: &str,
        version: &str,
        with_resources: bool,
    ) -> Result<Value, ManagerError> {
        let Some(p) = self.deps.store.get_package(tenant, name, version).await? else {
            return Err(ManagerError::NotFound(format!(
                "{name}#{version} is not installed"
            )));
        };
        let packages = self.deps.store.list_packages(tenant).await?;
        let all = self.deps.store.package_resources(tenant, None).await?;
        let mut owners: HashMap<(String, String, String), Vec<String>> = HashMap::new();
        for r in &all {
            owners
                .entry(r.resource.key())
                .or_default()
                .push(format!("{}#{}", r.package_name, r.package_version));
        }
        let mine: Vec<&OwnedPackageResource> = all
            .iter()
            .filter(|r| r.package_name == name && r.package_version == version)
            .collect();
        let key = format!("{name}#{version}");
        let dependents = dependents_index(&packages).remove(&key).unwrap_or_default();
        let mut out = package_json(&p, mine.len(), dependents);
        let mut by_type: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        let mut summary: BTreeMap<String, BTreeMap<&str, u64>> = BTreeMap::new();
        for r in &mine {
            let row = &r.resource;
            let shared_with: Vec<String> = owners
                .get(&row.key())
                .map(|o| o.iter().filter(|k| **k != key).cloned().collect())
                .unwrap_or_default();
            *summary
                .entry(row.resource_type.clone())
                .or_default()
                .entry(row.target.as_str())
                .or_default() += 1;
            if with_resources {
                by_type
                    .entry(row.resource_type.clone())
                    .or_default()
                    .push(json!({
                        "target": row.target,
                        "id": if row.target == TARGET_HFS { Value::from(row.resource_id.clone()) } else { Value::Null },
                        "url": row.url,
                        "version": row.version,
                        "preexisting": row.preexisting,
                        "sharedWith": shared_with,
                    }));
            }
        }
        out["resourceSummary"] = json!(summary);
        if with_resources {
            out["resources"] = json!(by_type);
        }
        Ok(out)
    }

    // ---------------------------------------------------------------- jobs

    /// Starts a plan (dry run) or install job.
    pub fn start_install(
        self: &Arc<Self>,
        tenant: &str,
        request: InstallRequest,
        requested_by: Option<String>,
    ) -> Result<Arc<Job>, ManagerError> {
        let (name, version) = request.target().map_err(ManagerError::Invalid)?;
        let label = match &version {
            Some(v) => format!("{name}#{v}"),
            None => name.clone(),
        };
        let kind = if request.dry_run {
            JobKind::Plan
        } else {
            JobKind::Install
        };
        let job = self.jobs.create(
            kind,
            &label,
            tenant,
            serde_json::to_value(&request).unwrap_or(Value::Null),
            requested_by.clone(),
        );
        let this = Arc::clone(self);
        let tenant = tenant.to_string();
        let handle = Arc::clone(&job);
        tokio::spawn(async move {
            if kind == JobKind::Plan {
                this.run_plan(&handle, &tenant, &name, version, &request.options)
                    .await;
            } else {
                this.run_install(
                    &handle,
                    &tenant,
                    &name,
                    version,
                    &request.options,
                    requested_by,
                )
                .await;
            }
        });
        Ok(job)
    }

    /// Starts an uninstall job after checking dependents.
    pub async fn start_uninstall(
        self: &Arc<Self>,
        tenant: &str,
        name: &str,
        version: &str,
        requested_by: Option<String>,
    ) -> Result<Arc<Job>, ManagerError> {
        if self
            .deps
            .store
            .get_package(tenant, name, version)
            .await?
            .is_none()
        {
            return Err(ManagerError::NotFound(format!(
                "{name}#{version} is not installed"
            )));
        }
        let packages = self.deps.store.list_packages(tenant).await?;
        let key = format!("{name}#{version}");
        if let Some(dependents) = dependents_index(&packages).get(&key)
            && !dependents.is_empty()
        {
            return Err(ManagerError::Conflict(format!(
                "{key} is required by {}; remove those first",
                dependents.join(", ")
            )));
        }
        let job = self.jobs.create(
            JobKind::Uninstall,
            &key,
            tenant,
            json!({"package": key}),
            requested_by.clone(),
        );
        let this = Arc::clone(self);
        let (tenant, name, version) = (tenant.to_string(), name.to_string(), version.to_string());
        let handle = Arc::clone(&job);
        tokio::spawn(async move {
            this.run_uninstall(&handle, &tenant, &name, &version, requested_by)
                .await;
        });
        Ok(job)
    }

    /// Stores an uploaded archive in the cache.
    pub fn accept_upload(&self, bytes: &[u8]) -> Result<Manifest, ManagerError> {
        // A full read validates every entry (paths, sizes, JSON) before the
        // archive is kept.
        let parsed = parse_package(bytes, self.registry.limits())
            .map_err(|e| ManagerError::Invalid(e.to_string()))?;
        self.registry
            .store(bytes)
            .map_err(ManagerError::Internal)
            .map(|_| parsed.manifest)
    }

    async fn installed_pairs(&self, tenant: &str) -> Result<Vec<(String, String)>, String> {
        Ok(self
            .deps
            .store
            .list_packages(tenant)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|p| p.status == "installed")
            .map(|p| (p.name, p.version))
            .collect())
    }

    async fn make_plan(
        &self,
        tenant: &str,
        name: &str,
        version: Option<String>,
        options: &InstallOptions,
    ) -> Result<Plan, String> {
        let installed = self.installed_pairs(tenant).await?;
        let request = PlanRequest {
            name: name.to_string(),
            version,
            mode: options.mode,
            policy: options.dependency_policy,
            include_dependencies: options.include_dependencies,
            skip: options.skip.iter().cloned().collect(),
            fhir_release: self.release(),
        };
        resolve(&request, &installed, &RegistrySource(&self.registry)).await
    }

    fn routes_terminology_to_hts(&self, options: &InstallOptions) -> bool {
        self.hts.is_some() && options.terminology != TerminologyTarget::Hfs
    }

    fn routes_terminology_to_hfs(&self, options: &InstallOptions) -> bool {
        self.hts.is_none() || options.terminology != TerminologyTarget::Auto
    }

    /// Splits a package's entries into HFS writes, terminology-server writes
    /// and skipped examples.
    fn classify<'a>(
        &self,
        parsed: &'a ParsedPackage,
        options: &InstallOptions,
    ) -> (
        Vec<&'a PackageEntry>,
        Vec<&'a PackageEntry>,
        Vec<&'a PackageEntry>,
    ) {
        let mut hfs = Vec::new();
        let mut hts = Vec::new();
        let mut skipped = Vec::new();
        for e in &parsed.entries {
            if e.example && !options.include_examples {
                skipped.push(e);
                continue;
            }
            let terminology = TERMINOLOGY_TYPES.contains(&e.resource_type.as_str());
            if terminology && self.routes_terminology_to_hts(options) {
                hts.push(e);
                if self.routes_terminology_to_hfs(options) {
                    hfs.push(e);
                }
            } else {
                hfs.push(e);
            }
        }
        // Terminology in dependency order: systems before sets before maps.
        hts.sort_by_key(|e| {
            TERMINOLOGY_TYPES
                .iter()
                .position(|t| *t == e.resource_type)
                .unwrap_or(9)
        });
        (hfs, hts, skipped)
    }

    async fn run_plan(
        &self,
        job: &Job,
        tenant: &str,
        name: &str,
        version: Option<String>,
        options: &InstallOptions,
    ) {
        job.start();
        job.phase("resolving dependencies");
        let plan = match self.make_plan(tenant, name, version, options).await {
            Ok(plan) => plan,
            Err(e) => {
                job.error(name, "plan", e.clone());
                job.finish(JobStatus::Failed, Some(json!({"error": e})));
                return;
            }
        };
        job.update(|s| s.packages_total = plan.install_order.len() as u64);
        let owners = match self.owner_map(tenant).await {
            Ok(o) => o,
            Err(e) => {
                job.finish(JobStatus::Failed, Some(json!({"error": e})));
                return;
            }
        };
        let mut reports = Vec::new();
        let (mut total_hfs, mut total_hts, mut total_examples) = (0u64, 0u64, 0u64);
        for key in &plan.install_order {
            let node = plan.node(key).expect("planned node");
            let version = node.version.clone().unwrap_or_default();
            job.update(|s| s.current = Some(key.clone()));
            job.phase(format!("reading {key}"));
            let report = match self.registry.fetch(&node.name, &version).await {
                Ok((bytes, fetch)) => {
                    let limits = *self.registry.limits();
                    match tokio::task::spawn_blocking(move || parse_package(&bytes, &limits)).await
                    {
                        Ok(Ok(parsed)) => {
                            let (hfs, hts, skipped) = self.classify(&parsed, options);
                            total_hfs += hfs.len() as u64;
                            total_hts += hts.len() as u64;
                            total_examples += skipped.len() as u64;
                            let conflicts = self
                                .conflicts(tenant, &node.name, &version, &hfs, &owners, options)
                                .await;
                            json!({
                                "package": key,
                                "name": node.name,
                                "version": version,
                                "action": node.action,
                                "title": parsed.manifest.title,
                                "fhirVersions": parsed.manifest.fhir_versions,
                                "size": parsed.size,
                                "sha1": parsed.sha1,
                                "source": fetch.source,
                                "counts": {
                                    "hfs": type_counts(&hfs),
                                    "hts": type_counts(&hts),
                                    "examplesSkipped": type_counts(&skipped),
                                },
                                "searchParameters": hfs.iter().filter(|e| e.resource_type == "SearchParameter").count(),
                                "unreadable": parsed.skipped.len(),
                                "unsupportedTypes": hfs.iter()
                                    .filter(|e| !crate::fhir_types::is_valid_resource_type_for_version(&e.resource_type, self.fhir_version))
                                    .map(|e| e.resource_type.clone())
                                    .collect::<BTreeSet<_>>(),
                                "conflicts": conflicts,
                            })
                        }
                        Ok(Err(e)) => json!({"package": key, "error": e.to_string()}),
                        Err(e) => json!({"package": key, "error": e.to_string()}),
                    }
                }
                Err(e) => json!({"package": key, "error": e}),
            };
            if let Some(e) = report.get("error").and_then(Value::as_str) {
                job.error(key, "package", e.to_string());
            }
            reports.push(report);
            job.update(|s| s.packages_done += 1);
        }
        let status = if job.view().errors.is_empty() {
            JobStatus::Succeeded
        } else {
            JobStatus::CompletedWithErrors
        };
        job.finish(
            status,
            Some(json!({
                "plan": plan,
                "packages": reports,
                "totals": {"hfs": total_hfs, "hts": total_hts, "examplesSkipped": total_examples},
                "terminologyServer": self.hts.as_ref().map(|h| h.base().to_string()),
                "options": options,
            })),
        );
    }

    async fn owner_map(
        &self,
        tenant: &str,
    ) -> Result<HashMap<(String, String, String), Vec<String>>, String> {
        let all = self
            .deps
            .store
            .package_resources(tenant, None)
            .await
            .map_err(|e| e.to_string())?;
        let mut map: HashMap<(String, String, String), Vec<String>> = HashMap::new();
        for r in all {
            map.entry(r.resource.key())
                .or_default()
                .push(format!("{}#{}", r.package_name, r.package_version));
        }
        Ok(map)
    }

    async fn conflicts(
        &self,
        tenant: &str,
        name: &str,
        version: &str,
        hfs: &[&PackageEntry],
        owners: &HashMap<(String, String, String), Vec<String>>,
        options: &InstallOptions,
    ) -> Value {
        let key = format!("{name}#{version}");
        let mut list = Vec::new();
        let mut kinds: BTreeMap<&str, u64> = BTreeMap::new();
        let ctx = Self::ctx(tenant);
        // What is stored under each id now.
        let mut by_type: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for e in hfs {
            if let Some(id) = e.id.as_deref() {
                by_type
                    .entry(e.resource_type.as_str())
                    .or_default()
                    .push(id);
            }
        }
        let mut stored: HashMap<(String, String), Value> = HashMap::new();
        for (resource_type, ids) in by_type {
            for chunk in ids.chunks(100) {
                if let Ok(found) = self
                    .deps
                    .storage
                    .read_batch(&ctx, resource_type, chunk)
                    .await
                {
                    for r in found {
                        stored.insert(
                            (resource_type.to_string(), r.id().to_string()),
                            r.content().clone(),
                        );
                    }
                }
            }
        }
        for e in hfs {
            let Some(id) = e.id.as_deref() else { continue };
            let Some(existing) = stored.get(&(e.resource_type.clone(), id.to_string())) else {
                continue;
            };
            let k = (
                TARGET_HFS.to_string(),
                e.resource_type.clone(),
                id.to_string(),
            );
            let others: Vec<&String> = owners
                .get(&k)
                .map(|by| by.iter().filter(|o| **o != key).collect())
                .unwrap_or_default();
            let url = text_of(existing, "url");
            let kind = if e.url.is_some() && url != e.url {
                "renamed-clash"
            } else if !others.is_empty() {
                let same_name = others
                    .iter()
                    .all(|o| o.split_once('#').is_some_and(|(n, _)| n == name));
                match (same_name, options.mode) {
                    (true, InstallMode::Upgrade) => "replaces-version",
                    (true, InstallMode::SideBySide) => "renamed",
                    (false, _) => "shared",
                }
            } else if owners.contains_key(&k) {
                // Owned only by this very package (a reinstall).
                continue;
            } else if url == e.url && text_of(existing, "version") == e.version {
                "adopts"
            } else {
                "overwrites-unmanaged"
            };
            *kinds.entry(kind).or_default() += 1;
            if kind != "adopts" && list.len() < MAX_CONFLICTS {
                list.push(json!({
                    "kind": kind, "resourceType": e.resource_type, "id": id, "url": e.url,
                    "storedUrl": url, "storedVersion": text_of(existing, "version"), "owners": others,
                }));
            }
        }
        json!({"counts": kinds, "items": list})
    }

    async fn read_content(
        &self,
        ctx: &TenantContext,
        resource_type: &str,
        id: &str,
    ) -> Option<Value> {
        match self.deps.storage.read(ctx, resource_type, id).await {
            Ok(Some(r)) => Some(r.content().clone()),
            _ => None,
        }
    }

    async fn run_install(
        &self,
        job: &Job,
        tenant: &str,
        name: &str,
        version: Option<String>,
        options: &InstallOptions,
        requested_by: Option<String>,
    ) {
        job.phase("waiting for other package jobs");
        let _guard = self.install_lock.lock().await;
        job.start();
        job.phase("resolving dependencies");
        let plan = match self.make_plan(tenant, name, version, options).await {
            Ok(plan) => plan,
            Err(e) => {
                job.error(name, "plan", e.clone());
                job.finish(JobStatus::Failed, Some(json!({"error": e})));
                return;
            }
        };
        job.update(|s| s.packages_total = plan.install_order.len() as u64);
        let mut installed = Vec::new();
        let mut failed = Vec::new();
        let mut search_parameters = 0u64;
        for key in &plan.install_order {
            if job.cancelled() {
                break;
            }
            let node = plan.node(key).expect("planned node").clone();
            let explicit = *key == plan.root;
            job.update(|s| s.current = Some(key.clone()));
            match self
                .install_one(
                    job,
                    tenant,
                    &node.name,
                    node.version.as_deref().unwrap_or_default(),
                    &node.action,
                    explicit,
                    options,
                    requested_by.as_deref(),
                )
                .await
            {
                Ok(sp) => {
                    search_parameters += sp;
                    installed.push(key.clone());
                }
                Err(e) => {
                    job.error(key, "package", e.clone());
                    job.log(format!("{key}: {e}"));
                    failed.push(json!({"package": key, "error": e}));
                }
            }
            job.update(|s| s.packages_done += 1);
        }
        let view = job.view();
        let status = if job.cancelled() {
            JobStatus::Cancelled
        } else if view.errors.is_empty() {
            JobStatus::Succeeded
        } else {
            JobStatus::CompletedWithErrors
        };
        let mut notes = Vec::new();
        if search_parameters > 0 {
            notes.push(format!(
                "{search_parameters} SearchParameters were installed: new searches work for \
                 resources written from now on; run $reindex to index existing data"
            ));
        }
        job.finish(
            status,
            Some(json!({
                "plan": plan,
                "installed": installed,
                "failed": failed,
                "searchParameters": search_parameters,
                "notes": notes,
            })),
        );
    }

    /// Installs one package; returns the number of SearchParameters written.
    #[allow(clippy::too_many_arguments)]
    async fn install_one(
        &self,
        job: &Job,
        tenant: &str,
        name: &str,
        version: &str,
        action: &NodeAction,
        explicit: bool,
        options: &InstallOptions,
        requested_by: Option<&str>,
    ) -> Result<u64, String> {
        let key = format!("{name}#{version}");
        job.phase(format!("fetching {key}"));
        let (bytes, fetch) = self.registry.fetch(name, version).await?;
        let limits = *self.registry.limits();
        let parsed = tokio::task::spawn_blocking(move || parse_package(&bytes, &limits))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        let (hfs, hts, skipped) = self.classify(&parsed, options);
        job.update(|s| s.resources_total += (hfs.len() + hts.len()) as u64);
        for e in &skipped {
            job.count(&key, &e.resource_type, |c| c.skipped += 1);
        }
        for (path, reason) in &parsed.skipped {
            job.error(&key, path.clone(), format!("unreadable: {reason}"));
        }

        let started = Utc::now();
        let mut document = json!({
            "title": parsed.manifest.title,
            "description": parsed.manifest.description,
            "canonical": parsed.manifest.canonical,
            "url": parsed.manifest.url,
            "fhirVersions": parsed.manifest.fhir_versions,
            "type": parsed.manifest.kind,
            "date": parsed.manifest.date,
            "author": parsed.manifest.author,
            "license": parsed.manifest.license,
            "dependencies": parsed.manifest.dependencies,
            "source": fetch.source,
            "shasumVerified": fetch.verified,
            "sha1": parsed.sha1,
            "sha256": parsed.sha256,
            "size": parsed.size,
            "explicit": explicit,
            "options": options,
            "requestedBy": requested_by,
        });
        // Keep an explicit flag once set (a dependency later installed on
        // purpose stays explicit; a re-install as a dependency keeps it).
        if let Ok(Some(prior)) = self.deps.store.get_package(tenant, name, version).await
            && prior.document.get("explicit").and_then(Value::as_bool) == Some(true)
        {
            document["explicit"] = json!(true);
        }
        let mut record = PackageRecord {
            name: name.to_string(),
            version: version.to_string(),
            status: "installing".into(),
            installed_at: started,
            document,
        };
        self.deps
            .store
            .put_package(tenant, &record)
            .await
            .map_err(|e| e.to_string())?;

        let owners = self.owner_map(tenant).await?;
        let ctx = Self::ctx(tenant);
        let mut rows: Vec<PackageResourceRecord> = Vec::new();
        let mut written: BTreeMap<String, u64> = BTreeMap::new();
        let mut failures = 0u64;
        let mut search_parameters = 0u64;

        // ---- this server
        job.phase(format!("writing {} resources of {key}", hfs.len()));
        for e in &hfs {
            if job.cancelled() {
                break;
            }
            let resource_label = format!("{}/{}", e.resource_type, e.id.as_deref().unwrap_or("?"));
            if !crate::fhir_types::is_valid_resource_type_for_version(
                &e.resource_type,
                self.fhir_version,
            ) {
                job.count(&key, &e.resource_type, |c| c.failed += 1);
                job.error(
                    &key,
                    resource_label,
                    format!(
                        "{} is not a {} resource type",
                        e.resource_type,
                        self.fhir_version.as_str()
                    ),
                );
                failures += 1;
                job.update(|s| s.resources_done += 1);
                continue;
            }
            let mut id = e.id.clone().unwrap_or_else(|| id_from_path(&e.path));
            if options.mode == InstallMode::SideBySide
                && owners
                    .get(&(TARGET_HFS.to_string(), e.resource_type.clone(), id.clone()))
                    .is_some_and(|by| {
                        by.iter()
                            .any(|o| o != &key && o.split_once('#').is_some_and(|(n, _)| n == name))
                    })
            {
                id = suffixed_id(&id, version);
            }
            let mut json = match e.json() {
                Ok(v) => v,
                Err(err) => {
                    job.count(&key, &e.resource_type, |c| c.failed += 1);
                    job.error(&key, resource_label, err);
                    failures += 1;
                    job.update(|s| s.resources_done += 1);
                    continue;
                }
            };
            // Identity follows the canonical: a different artifact already
            // stored under this id (another IG's SearchParameter `Condition-code`,
            // say) is never overwritten — this one gets `id-<hash of url>`.
            let mut existing = self.read_content(&ctx, &e.resource_type, &id).await;
            if let (Some(ex), Some(url)) = (&existing, e.url.as_deref())
                && text_of(ex, "url").as_deref() != Some(url)
            {
                id = clash_id(&id, url);
                existing = self.read_content(&ctx, &e.resource_type, &id).await;
            }
            json["id"] = Value::from(id.clone());
            let k = (TARGET_HFS.to_string(), e.resource_type.clone(), id.clone());
            // A stored resource no package owns is the operator's — unless
            // it is this very artifact (same canonical and version, e.g.
            // loaded by a script before the package manager existed), which
            // the package adopts.
            let foreign = !owners.contains_key(&k)
                && existing.as_ref().is_some_and(|ex| {
                    !(text_of(ex, "url") == e.url && text_of(ex, "version") == e.version)
                });
            match self
                .deps
                .storage
                .create_or_update(&ctx, &e.resource_type, &id, json, self.fhir_version)
                .await
            {
                Ok((stored, created)) => {
                    let preexisting = !created && foreign;
                    if e.resource_type == "StructureDefinition" {
                        self.deps.validation.upsert_stored_profile(
                            tenant,
                            stored.fhir_version(),
                            stored.content(),
                        );
                    }
                    if e.resource_type == "SearchParameter" {
                        search_parameters += 1;
                    }
                    self.report_write(&ctx, &e.resource_type, i64::from(created));
                    rows.push(PackageResourceRecord {
                        target: TARGET_HFS.into(),
                        resource_type: e.resource_type.clone(),
                        resource_id: id,
                        url: e.url.clone(),
                        version: e.version.clone(),
                        preexisting,
                    });
                    *written.entry(e.resource_type.clone()).or_default() += 1;
                    job.count(&key, &e.resource_type, |c| c.written += 1);
                }
                Err(err) => {
                    job.count(&key, &e.resource_type, |c| c.failed += 1);
                    job.error(&key, resource_label, err.to_string());
                    failures += 1;
                }
            }
            job.update(|s| s.resources_done += 1);
            if rows.len() >= ROW_FLUSH {
                self.flush_rows(tenant, name, version, &mut rows).await?;
            }
        }
        self.flush_rows(tenant, name, version, &mut rows).await?;

        // ---- terminology server
        if let (Some(client), false) = (&self.hts, hts.is_empty() || job.cancelled()) {
            job.phase(format!(
                "sending {} terminology resources of {key} to {}",
                hts.len(),
                client.base()
            ));
            let mut start = 0;
            while start < hts.len() && !job.cancelled() {
                let mut end = start;
                let mut bytes = 0usize;
                while end < hts.len()
                    && end - start < HTS_CHUNK_RESOURCES
                    && (end == start || bytes + hts[end].bytes.len() <= HTS_CHUNK_BYTES)
                    // Keep one type per call so systems land before sets.
                    && hts[end].resource_type == hts[start].resource_type
                {
                    bytes += hts[end].bytes.len();
                    end += 1;
                }
                let chunk = &hts[start..end];
                start = end;
                let (ok, failed_n) = self
                    .send_terminology(job, client, &key, chunk, &owners, &mut rows)
                    .await;
                failures += failed_n;
                *written.entry(chunk[0].resource_type.clone()).or_default() += ok;
                job.update(|s| s.resources_done += chunk.len() as u64);
                self.flush_rows(tenant, name, version, &mut rows).await?;
            }
        }

        // ---- finish this package
        let installed_now = self.installed_pairs(tenant).await.unwrap_or_default();
        let mut resolved = BTreeMap::new();
        for (dep, range) in &parsed.manifest.dependencies {
            resolved.insert(
                dep.clone(),
                self.resolve_dependency(dep, range, &installed_now, options.dependency_policy),
            );
        }
        record.status = if job.cancelled() {
            "cancelled".into()
        } else {
            "installed".into()
        };
        record.installed_at = Utc::now();
        record.document["resolved"] = json!(resolved);
        record.document["counts"] = json!(written);
        record.document["failures"] = json!(failures);
        record.document["examplesSkipped"] = json!(skipped.len());
        record.document["installedAt"] = json!(Utc::now());
        self.deps
            .store
            .put_package(tenant, &record)
            .await
            .map_err(|e| e.to_string())?;

        // ---- upgrade: retire the replaced versions
        if let (NodeAction::Upgrade { from }, false) = (action, job.cancelled()) {
            for old in from {
                job.phase(format!("removing replaced {name}#{old}"));
                match self.remove_owned(job, tenant, name, old).await {
                    Ok(report) => {
                        job.log(format!("{name}#{old} replaced: {report}"));
                        let _ = self.deps.store.delete_package(tenant, name, old).await;
                        self.retarget_dependents(tenant, name, old, version).await;
                        self.audit(
                            tenant,
                            AuditAction::Delete,
                            "0",
                            format!("Removed FHIR package {name}#{old} (upgraded to {version})"),
                            &format!("{name}#{old}"),
                            requested_by,
                        )
                        .await;
                    }
                    Err(e) => job.error(&format!("{name}#{old}"), "upgrade", e),
                }
            }
        }

        let outcome = if failures == 0 { "0" } else { "4" };
        self.audit(
            tenant,
            AuditAction::Create,
            outcome,
            format!(
                "Installed FHIR package {key} ({} resources written, {failures} failed)",
                written.values().sum::<u64>()
            ),
            &key,
            requested_by,
        )
        .await;
        info!(package = %key, written = written.values().sum::<u64>(), failures, "FHIR package installed");
        Ok(search_parameters)
    }

    async fn send_terminology(
        &self,
        job: &Job,
        client: &HtsClient,
        key: &str,
        chunk: &[&PackageEntry],
        owners: &HashMap<(String, String, String), Vec<String>>,
        rows: &mut Vec<PackageResourceRecord>,
    ) -> (u64, u64) {
        // What is already on the terminology server (and not owned by any
        // package) is recorded as pre-existing, so an uninstall leaves it.
        let lookups: Vec<_> = chunk
            .iter()
            .map(|e| {
                let client = client.clone();
                let resource_type = e.resource_type.clone();
                let url = e.url.clone();
                let version = e.version.clone();
                async move {
                    match url {
                        Some(url) => client
                            .find(&resource_type, &url, version.as_deref())
                            .await
                            .map(|ids| !ids.is_empty())
                            .unwrap_or(false),
                        None => false,
                    }
                }
            })
            .collect();
        let checks: Vec<bool> = futures::stream::iter(lookups).buffered(8).collect().await;
        let mut resources = Vec::with_capacity(chunk.len());
        for e in chunk {
            match e.json() {
                Ok(v) => resources.push(v),
                Err(err) => job.error(key, hts_label(e), err),
            }
        }
        let resource_type = chunk[0].resource_type.clone();
        match client.import(&resources).await {
            Ok(outcome) => {
                for (e, existed) in chunk.iter().zip(checks) {
                    let resource_id = hts_key(e);
                    let k = (
                        TARGET_HTS.to_string(),
                        e.resource_type.clone(),
                        resource_id.clone(),
                    );
                    rows.push(PackageResourceRecord {
                        target: TARGET_HTS.into(),
                        resource_type: e.resource_type.clone(),
                        resource_id,
                        url: e.url.clone(),
                        version: e.version.clone(),
                        preexisting: existed && !owners.contains_key(&k),
                    });
                }
                let failed = (outcome.errors.len() as u64).min(chunk.len() as u64);
                for err in outcome.errors.iter().take(50) {
                    job.error(
                        key,
                        format!("{resource_type} (terminology server)"),
                        err.clone(),
                    );
                }
                let ok = chunk.len() as u64 - failed;
                job.count(key, &resource_type, |c| {
                    c.written += ok;
                    c.failed += failed;
                });
                (ok, failed)
            }
            Err(err) => {
                job.error(
                    key,
                    format!("{} {resource_type} (terminology server)", chunk.len()),
                    err,
                );
                job.count(key, &resource_type, |c| c.failed += chunk.len() as u64);
                (0, chunk.len() as u64)
            }
        }
    }

    async fn flush_rows(
        &self,
        tenant: &str,
        name: &str,
        version: &str,
        rows: &mut Vec<PackageResourceRecord>,
    ) -> Result<(), String> {
        if rows.is_empty() {
            return Ok(());
        }
        let batch = std::mem::take(rows);
        self.deps
            .store
            .add_package_resources(tenant, name, version, &batch)
            .await
            .map_err(|e| e.to_string())
    }

    fn resolve_dependency(
        &self,
        dep: &str,
        range: &str,
        installed: &[(String, String)],
        policy: DependencyPolicy,
    ) -> String {
        let release = self.release();
        if let Some(rest) = dep.strip_prefix("hl7.fhir.")
            && let Some((r, part)) = rest.split_once('.')
            && matches!(r, "r2" | "r3" | "r4" | "r4b" | "r5" | "r6")
        {
            return if r == release && part == "core" {
                "embedded".into()
            } else {
                "skipped".into()
            };
        }
        let parsed = Range::parse(range);
        let mut meeting: Vec<&String> = installed
            .iter()
            .filter(|(n, v)| n == dep && parsed.satisfied_by(v, policy))
            .map(|(_, v)| v)
            .collect();
        meeting.sort_by(|a, b| super::version::compare(b, a));
        match meeting.first() {
            Some(v) => format!("{dep}#{v}"),
            None => "missing".into(),
        }
    }

    /// Points dependents of `name#old` at `name#new` after an upgrade.
    async fn retarget_dependents(&self, tenant: &str, name: &str, old: &str, new: &str) {
        let Ok(packages) = self.deps.store.list_packages(tenant).await else {
            return;
        };
        let (old_key, new_key) = (format!("{name}#{old}"), format!("{name}#{new}"));
        for mut p in packages {
            let Some(resolved) = p
                .document
                .get_mut("resolved")
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            let mut changed = false;
            for v in resolved.values_mut() {
                if v.as_str() == Some(old_key.as_str()) {
                    *v = Value::from(new_key.clone());
                    changed = true;
                }
            }
            if changed && let Err(e) = self.deps.store.put_package(tenant, &p).await {
                warn!(error = %e, package = %p.name, "could not retarget a dependent package");
            }
        }
    }

    fn report_write(&self, ctx: &TenantContext, resource_type: &str, live_delta: i64) {
        self.deps
            .observers
            .on_write(&WriteEvent::Resource(ResourceWrite {
                tenant: ctx.tenant_id().clone(),
                fhir_version: self.fhir_version,
                resource_type: resource_type.to_string(),
                live_delta,
                notice: None,
                at: Utc::now(),
            }));
    }

    async fn run_uninstall(
        &self,
        job: &Job,
        tenant: &str,
        name: &str,
        version: &str,
        requested_by: Option<String>,
    ) {
        job.phase("waiting for other package jobs");
        let _guard = self.install_lock.lock().await;
        job.start();
        job.update(|s| s.packages_total = 1);
        let key = format!("{name}#{version}");
        job.update(|s| s.current = Some(key.clone()));
        match self.remove_owned(job, tenant, name, version).await {
            Ok(report) => {
                if let Err(e) = self.deps.store.delete_package(tenant, name, version).await {
                    job.error(&key, "registry", e.to_string());
                }
                job.update(|s| s.packages_done = 1);
                let failed = !job.view().errors.is_empty();
                self.audit(
                    tenant,
                    AuditAction::Delete,
                    if failed { "4" } else { "0" },
                    format!("Uninstalled FHIR package {key}: {report}"),
                    &key,
                    requested_by.as_deref(),
                )
                .await;
                job.finish(
                    if failed {
                        JobStatus::CompletedWithErrors
                    } else {
                        JobStatus::Succeeded
                    },
                    Some(report),
                );
            }
            Err(e) => {
                job.error(&key, "uninstall", e.clone());
                self.audit(
                    tenant,
                    AuditAction::Delete,
                    "8",
                    format!("Uninstall of FHIR package {key} failed: {e}"),
                    &key,
                    requested_by.as_deref(),
                )
                .await;
                job.finish(JobStatus::Failed, Some(json!({"error": e})));
            }
        }
    }

    /// Deletes the resources only `name#version` owns. Returns a report.
    async fn remove_owned(
        &self,
        job: &Job,
        tenant: &str,
        name: &str,
        version: &str,
    ) -> Result<Value, String> {
        let key = format!("{name}#{version}");
        let all = self
            .deps
            .store
            .package_resources(tenant, None)
            .await
            .map_err(|e| e.to_string())?;
        let (mine, others): (Vec<_>, Vec<_>) = all
            .into_iter()
            .partition(|r| r.package_name == name && r.package_version == version);
        let other_keys: HashSet<(String, String, String)> =
            others.iter().map(|r| r.resource.key()).collect();
        // Canonical -> another package's StructureDefinition id, to restore
        // a profile the deletion would otherwise unregister.
        let mut other_profiles: HashMap<String, String> = HashMap::new();
        for r in &others {
            if r.resource.target == TARGET_HFS
                && r.resource.resource_type == "StructureDefinition"
                && let Some(url) = &r.resource.url
            {
                other_profiles.insert(url.clone(), r.resource.resource_id.clone());
            }
        }
        job.update(|s| s.resources_total += mine.len() as u64);
        job.phase(format!("removing {} resources of {key}", mine.len()));
        let ctx = Self::ctx(tenant);
        let mut deleted: BTreeMap<String, u64> = BTreeMap::new();
        let (mut kept_shared, mut kept_preexisting, mut failed) = (0u64, 0u64, 0u64);
        for owned in &mine {
            let r = &owned.resource;
            job.update(|s| s.resources_done += 1);
            if r.preexisting {
                kept_preexisting += 1;
                job.count(&key, &r.resource_type, |c| c.skipped += 1);
                continue;
            }
            if other_keys.contains(&r.key()) {
                kept_shared += 1;
                job.count(&key, &r.resource_type, |c| c.skipped += 1);
                continue;
            }
            let result = if r.target == TARGET_HTS {
                self.delete_terminology(r).await
            } else {
                self.delete_hfs(&ctx, tenant, r, &other_profiles).await
            };
            match result {
                Ok(()) => {
                    *deleted.entry(r.resource_type.clone()).or_default() += 1;
                    job.count(&key, &r.resource_type, |c| c.written += 1);
                }
                Err(e) => {
                    failed += 1;
                    job.count(&key, &r.resource_type, |c| c.failed += 1);
                    job.error(&key, format!("{}/{}", r.resource_type, r.resource_id), e);
                }
            }
        }
        Ok(json!({
            "deleted": deleted,
            "keptShared": kept_shared,
            "keptPreexisting": kept_preexisting,
            "failed": failed,
        }))
    }

    async fn delete_hfs(
        &self,
        ctx: &TenantContext,
        tenant: &str,
        r: &PackageResourceRecord,
        other_profiles: &HashMap<String, String>,
    ) -> Result<(), String> {
        let existing = match self
            .deps
            .storage
            .read(ctx, &r.resource_type, &r.resource_id)
            .await
        {
            Ok(found) => found,
            Err(StorageError::Resource(ResourceError::Gone { .. })) => None,
            Err(e) => return Err(e.to_string()),
        };
        let Some(existing) = existing else {
            return Ok(());
        };
        match self
            .deps
            .storage
            .delete(ctx, &r.resource_type, &r.resource_id)
            .await
        {
            Ok(()) => {}
            Err(StorageError::Resource(ResourceError::NotFound { .. }))
            | Err(StorageError::Resource(ResourceError::Gone { .. })) => return Ok(()),
            Err(e) => return Err(e.to_string()),
        }
        self.report_write(ctx, &r.resource_type, -1);
        if r.resource_type == "StructureDefinition"
            && let Some(url) = existing.content().get("url").and_then(Value::as_str)
        {
            self.deps
                .validation
                .remove_stored_profile(tenant, existing.fhir_version(), url);
            // Another installed package still ships this canonical: put its
            // definition back in the registry.
            if let Some(other_id) = other_profiles.get(url)
                && let Ok(Some(other)) = self
                    .deps
                    .storage
                    .read(ctx, "StructureDefinition", other_id)
                    .await
            {
                self.deps.validation.upsert_stored_profile(
                    tenant,
                    other.fhir_version(),
                    other.content(),
                );
            }
        }
        Ok(())
    }

    async fn delete_terminology(&self, r: &PackageResourceRecord) -> Result<(), String> {
        let Some(client) = &self.hts else {
            return Err("no terminology server is configured (HFS_TERMINOLOGY_SERVER)".into());
        };
        let Some(url) = r.url.as_deref() else {
            return Ok(());
        };
        let ids = client
            .find(&r.resource_type, url, r.version.as_deref())
            .await?;
        for id in ids {
            client.delete(&r.resource_type, &id).await?;
        }
        Ok(())
    }

    async fn audit(
        &self,
        tenant: &str,
        action: AuditAction,
        outcome: &str,
        description: String,
        package: &str,
        who: Option<&str>,
    ) {
        let mut builder = AuditEventBuilder::new(self.deps.audit_source.clone())
            .action(action)
            .outcome(outcome)
            .outcome_desc(description)
            .event_type("http://dicom.nema.org/resources/ontology/DCM", "110100")
            .detail("package", package.to_string())
            .detail("tenant", tenant.to_string());
        builder = builder.agent(
            who.unwrap_or("hfs-package-manager").to_string(),
            None,
            who.is_some(),
        );
        let event = builder.build();
        match &self.deps.audit {
            Some(sink) => sink.record(event).await,
            None => {
                // No audit pipeline configured: still keep the record, as a
                // stored AuditEvent in the tenant the package went into.
                let Ok(value) = serde_json::to_value(&event) else {
                    return;
                };
                if let Err(e) = self
                    .deps
                    .storage
                    .create(&Self::ctx(tenant), "AuditEvent", value, self.fhir_version)
                    .await
                {
                    warn!(error = %e, "could not store the package AuditEvent");
                }
            }
        }
    }

    // ------------------------------------------------------------ startup

    /// Installs `HFS_PACKAGES` entries that are not installed yet, one after
    /// another, in the background.
    pub fn spawn_startup(self: &Arc<Self>) {
        if self.config.startup.is_empty() {
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            warn!("No Tokio runtime at app construction; HFS_PACKAGES startup installs skipped");
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let tenant = this.default_tenant.clone();
            for spec in this.config.startup.clone() {
                let (name, version) = match parse_spec(&spec) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(entry = %spec, error = %e, "HFS_PACKAGES: skipping invalid entry");
                        continue;
                    }
                };
                let installed = this.installed_pairs(&tenant).await.unwrap_or_default();
                let present = installed
                    .iter()
                    .any(|(n, v)| *n == name && version.as_ref().is_none_or(|want| want == v));
                if present {
                    info!(package = %spec, "HFS_PACKAGES: already installed");
                    continue;
                }
                info!(package = %spec, "HFS_PACKAGES: installing");
                let request = InstallRequest {
                    name: Some(name.clone()),
                    version: version.clone(),
                    ..InstallRequest::default()
                };
                let job = this.jobs.create(
                    JobKind::Install,
                    &spec,
                    &tenant,
                    serde_json::to_value(&request).unwrap_or(Value::Null),
                    Some("HFS_PACKAGES".into()),
                );
                this.run_install(
                    &job,
                    &tenant,
                    &name,
                    version,
                    &request.options,
                    Some("HFS_PACKAGES".into()),
                )
                .await;
                let view: JobView = job.view();
                info!(package = %spec, status = ?view.status, errors = view.errors.len(), "HFS_PACKAGES: done");
            }
        });
    }
}

fn type_counts(entries: &[&PackageEntry]) -> BTreeMap<String, u64> {
    let mut map = BTreeMap::new();
    for e in entries {
        *map.entry(e.resource_type.clone()).or_insert(0) += 1;
    }
    map
}

fn hts_key(e: &PackageEntry) -> String {
    match (&e.url, &e.version) {
        (Some(url), Some(v)) => format!("{url}|{v}"),
        (Some(url), None) => url.clone(),
        _ => e.id.clone().unwrap_or_else(|| id_from_path(&e.path)),
    }
}

fn hts_label(e: &PackageEntry) -> String {
    format!("{}/{}", e.resource_type, e.id.as_deref().unwrap_or("?"))
}

/// Who depends on what: `name#version` → dependents (`name#version`).
fn dependents_index(packages: &[PackageRecord]) -> HashMap<String, Vec<String>> {
    let mut index: HashMap<String, Vec<String>> = HashMap::new();
    for p in packages {
        let me = format!("{}#{}", p.name, p.version);
        if let Some(resolved) = p.document.get("resolved").and_then(Value::as_object) {
            for target in resolved.values().filter_map(Value::as_str) {
                if target.contains('#') && target != me {
                    index
                        .entry(target.to_string())
                        .or_default()
                        .push(me.clone());
                }
            }
        }
    }
    index
}

fn package_json(p: &PackageRecord, resources: usize, dependents: Vec<String>) -> Value {
    let mut out = json!({
        "name": p.name,
        "version": p.version,
        "status": p.status,
        "installedAt": p.installed_at,
        "resourceCount": resources,
        "dependents": dependents,
    });
    if let (Some(dst), Some(src)) = (out.as_object_mut(), p.document.as_object()) {
        for (k, v) in src {
            dst.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    out
}

/// A FHIR id from a package file name (`StructureDefinition-foo.json` → `foo`).
fn id_from_path(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = file.strip_suffix(".json").unwrap_or(file);
    let stem = stem.split_once('-').map(|(_, rest)| rest).unwrap_or(stem);
    let id: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    if id.is_empty() { "resource".into() } else { id }
}

fn text_of(resource: &Value, key: &str) -> Option<String> {
    resource
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// `id-<8 hex of sha1(url)>` for a resource whose id is taken by a
/// different canonical, kept within FHIR's 64 characters; deterministic, so
/// a reinstall lands on the same id.
fn clash_id(id: &str, url: &str) -> String {
    let hash = super::tarball::sha1_hex(url.as_bytes());
    let keep = 64 - 9;
    format!("{}-{}", &id[..id.len().min(keep)], &hash[..8])
}

/// `id-version` for side-by-side installs, kept within FHIR's 64 characters.
fn suffixed_id(id: &str, version: &str) -> String {
    let version: String = version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let candidate = format!("{id}-{version}");
    if candidate.len() <= 64 {
        return candidate;
    }
    let hash = super::tarball::sha1_hex(candidate.as_bytes());
    let keep = 64 - 9;
    format!("{}-{}", &id[..id.len().min(keep)], &hash[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_parse_in_all_three_forms() {
        assert_eq!(
            parse_spec("tw.gov.mohw.twcore#1.0.0").unwrap(),
            ("tw.gov.mohw.twcore".into(), Some("1.0.0".into()))
        );
        assert_eq!(
            parse_spec("hl7.fhir.uv.ips@1.1.0").unwrap(),
            ("hl7.fhir.uv.ips".into(), Some("1.1.0".into()))
        );
        assert_eq!(parse_spec(" a.b ").unwrap(), ("a.b".into(), None));
        assert!(parse_spec("../x#1").is_err());
        assert!(parse_spec("a#1/2").is_err());
        let r = InstallRequest {
            name: Some("a".into()),
            version: Some("".into()),
            ..InstallRequest::default()
        };
        assert_eq!(r.target().unwrap(), ("a".into(), None));
        assert!(InstallRequest::default().target().is_err());
    }

    #[test]
    fn ids_from_paths_and_side_by_side_suffixes_stay_valid() {
        assert_eq!(
            id_from_path("package/StructureDefinition-Patient-twcore.json"),
            "Patient-twcore"
        );
        assert_eq!(id_from_path("package/odd name.json"), "odd-name");
        assert_eq!(
            suffixed_id("Patient-twcore", "1.0.0"),
            "Patient-twcore-1.0.0"
        );
        let c = clash_id(
            "Condition-code",
            "https://twcore.mohw.gov.tw/ig/emr/SearchParameter/Condition-code",
        );
        assert_eq!(c.len(), "Condition-code".len() + 9);
        assert_eq!(
            c,
            clash_id(
                "Condition-code",
                "https://twcore.mohw.gov.tw/ig/emr/SearchParameter/Condition-code"
            )
        );
        assert_eq!(clash_id(&"y".repeat(64), "u").len(), 64);
        let long = "x".repeat(64);
        let s = suffixed_id(&long, "1.0.0");
        assert_eq!(s.len(), 64);
        assert_ne!(s, suffixed_id(&long, "2.0.0"));
    }

    #[test]
    fn dependents_come_from_resolved_dependencies() {
        let p = |name: &str, resolved: Value| PackageRecord {
            name: name.into(),
            version: "1".into(),
            status: "installed".into(),
            installed_at: Utc::now(),
            document: json!({"resolved": resolved}),
        };
        let packages = vec![
            p("a", json!({"core": "embedded", "b": "b#1"})),
            p("c", json!({"b": "b#1"})),
            p("b", json!({})),
        ];
        let index = dependents_index(&packages);
        assert_eq!(index["b#1"], ["a#1", "c#1"]);
        assert!(!index.contains_key("embedded"));
    }

    #[test]
    fn cache_dir_follows_the_sqlite_database() {
        let mut config = ServerConfig {
            database_url: Some("/data/fhir.db".into()),
            ..ServerConfig::default()
        };
        assert_eq!(
            cache_dir(&config),
            std::path::PathBuf::from("/data/packages")
        );
        config.packages.cache_dir = Some("/srv/pkgs".into());
        assert_eq!(cache_dir(&config), std::path::PathBuf::from("/srv/pkgs"));
        config.packages.cache_dir = None;
        config.database_url = None;
        config.data_dir = Some("/opt/hfs".into());
        assert_eq!(
            cache_dir(&config),
            std::path::PathBuf::from("/opt/hfs/packages")
        );
    }
}
