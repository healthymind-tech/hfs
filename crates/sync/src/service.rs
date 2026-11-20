//! The sync service behind the admin API and the console pages: job CRUD
//! with validation (peers exist, same FHIR release, no cycles, outbound
//! policy, secrets sealed), run / cancel / retry, conflicts, the
//! notification receiver, and endpoint duplication (design §9.1, §9.3).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use chrono::Utc;
use helios_endpoints::model::{EndpointSpec, EndpointStatus, ManagedBy, StorageBinding, ValidationErrors};
use helios_endpoints::name::{EndpointName, EndpointPath};
use helios_endpoints::store::{Actor, StoreError};
use helios_endpoints::sync::{
    DuplicateData, DuplicateRequest, InitialLoad, RunReason, SyncConflict, SyncJob, SyncJobSpec,
    SyncLogLine, SyncPeer, SyncRun, SyncRunState, SyncTrigger,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::engine::{build_peers, keep_target, overwrite_one};
use crate::error::{EngineError, SyncError, invalid};
use crate::external::{ExternalSource, capability_release, capability_types};
use crate::http::FhirClient;
use crate::runtime::SyncRuntime;
use crate::secrets::{open_auth, open_notify_secret, redacted_job, secrets_equal, spec_from_body};

/// Conformance resource types copied when a duplicate copies packages only.
pub const CONFORMANCE_TYPES: &[&str] = &[
    "StructureDefinition",
    "ValueSet",
    "CodeSystem",
    "ConceptMap",
    "SearchParameter",
    "ImplementationGuide",
    "CapabilityStatement",
    "OperationDefinition",
    "CompartmentDefinition",
    "NamingSystem",
    "StructureMap",
];

const FOLLOW_UP: &str = "follow-up job: ";

/// The follow-up continuous job id a duplication job names, if any.
pub fn follow_up_job(job: &SyncJob) -> Option<String> {
    let d = job.spec.description.as_deref()?;
    let i = d.find(FOLLOW_UP)?;
    let rest = &d[i + FOLLOW_UP.len()..];
    Some(rest.split_whitespace().next()?.trim_end_matches('.').to_string())
}

/// A job with its latest run, for lists.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSummary {
    /// The job (redacted).
    pub job: Value,
    /// Latest run.
    pub last_run: Option<SyncRun>,
}

/// Result of a create (the notify secret is shown once).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedJob {
    /// The job (redacted).
    pub job: Value,
    /// Plaintext notify secret of a subscription trigger (only now).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify_secret: Option<String>,
    /// Notification URL path of a subscription trigger.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify_path: Option<String>,
}

/// Result of a duplicate.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateOutcome {
    /// The new endpoint (redacted).
    pub endpoint: Value,
    /// The copy job, when data/packages are copied.
    pub job_id: Option<String>,
    /// The copy run.
    pub run_id: Option<String>,
    /// The follow-up continuous job (`keepInSync`).
    pub follow_up_job_id: Option<String>,
}

/// Connection test report.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PeerTest {
    /// Side.
    pub side: String,
    /// Reachable.
    pub ok: bool,
    /// FHIR release the server declares.
    pub fhir_version: Option<String>,
    /// Resource types offered.
    pub resource_types: usize,
    /// Software name.
    pub software: Option<String>,
    /// Error.
    pub error: Option<String>,
}

/// See the module docs.
pub struct SyncService {
    runtime: Arc<SyncRuntime>,
}

static GLOBAL: OnceLock<Arc<SyncService>> = OnceLock::new();

/// Registers the process's service (the admin API and console pages find
/// it here; `None` until `HFS_MANAGER=on` installs sync).
pub fn install_global(s: Arc<SyncService>) -> bool {
    GLOBAL.set(s).is_ok()
}

/// The process's service, if installed.
pub fn global() -> Option<Arc<SyncService>> {
    GLOBAL.get().cloned()
}

fn peer_key(p: &SyncPeer) -> String {
    match p {
        SyncPeer::Endpoint { name } => format!("endpoint:{name}"),
        SyncPeer::External(x) => format!("external:{}", x.base_url.trim_end_matches('/').to_ascii_lowercase()),
    }
}

/// Whether adding `edge` (source → target) to `edges` closes a cycle.
pub fn creates_cycle(edges: &[(String, String)], edge: &(String, String)) -> bool {
    // Is there a path target → … → source?
    let mut stack = vec![edge.1.clone()];
    let mut seen = BTreeSet::new();
    while let Some(n) = stack.pop() {
        if n == edge.0 {
            return true;
        }
        if !seen.insert(n.clone()) {
            continue;
        }
        for (a, b) in edges {
            if *a == n {
                stack.push(b.clone());
            }
        }
    }
    false
}

fn engine_err(e: EngineError) -> SyncError {
    match e {
        EngineError::Failed(m) | EngineError::Stopped(m) => SyncError::Unavailable(m),
    }
}

fn store_err(e: StoreError) -> SyncError {
    match e {
        StoreError::NotFound(n) => SyncError::NotFound(format!("endpoint {n}")),
        StoreError::Conflict { current } => SyncError::Revision { current },
        StoreError::Invalid(v) => SyncError::Invalid(v),
        StoreError::Duplicate { .. } | StoreError::Forbidden(_) => SyncError::Conflict(e.to_string()),
        StoreError::Unavailable(m) => SyncError::Unavailable(m),
    }
}

impl SyncService {
    /// A service over a runtime.
    pub fn new(runtime: Arc<SyncRuntime>) -> Self {
        Self { runtime }
    }

    /// The runtime.
    pub fn runtime(&self) -> &Arc<SyncRuntime> {
        &self.runtime
    }

    fn store(&self) -> &Arc<dyn helios_endpoints::sync::SyncStore> {
        &self.runtime.env.store
    }

    async fn audit(&self, action: &str, job: &SyncJob, actor: &str, ok: bool, desc: &str) {
        if let Some(a) = self.runtime.auditor() {
            a.record(action, &job.id, &job.spec.name, actor, ok, desc).await;
        }
    }

    /// Checks a spec against the registry, the policy and the other jobs.
    pub async fn validate(&self, spec: &SyncJobSpec, job_id: Option<&str>) -> Result<(), SyncError> {
        let mut e = match spec.validate() {
            Ok(()) => ValidationErrors::default(),
            Err(v) => v,
        };
        let env = &self.runtime.env;
        let snap = env.registry.snapshot();
        let mut releases = Vec::new();
        for (field, peer) in [("source", &spec.source), ("target", &spec.target)] {
            match peer {
                SyncPeer::Endpoint { name } => match snap.by_name(name) {
                    Some(ep) => {
                        releases.push(Some(ep.spec.fhir_version));
                        if env.local.is_none() {
                            e.push(&format!("{field}.name"), "this server has no storage access for local endpoints");
                        }
                    }
                    None => e.push(&format!("{field}.name"), format!("endpoint {name:?} does not exist")),
                },
                SyncPeer::External(x) => {
                    releases.push(x.fhir_version);
                    if !env.settings.external_allowed {
                        e.push(&format!("{field}.kind"), "external peers are disabled (HFS_SYNC_EXTERNAL=off)");
                    }
                    if let Err(d) = env.http.policy().check_url(&x.base_url) {
                        e.push(&format!("{field}.baseUrl"), d.to_string());
                    }
                    if let helios_endpoints::sync::ExternalAuth::ClientCredentials { token_url, .. }
                    | helios_endpoints::sync::ExternalAuth::PrivateKeyJwt { token_url, .. } = &x.auth
                        && let Err(d) = env.http.policy().check_url(token_url)
                    {
                        e.push(&format!("{field}.auth.tokenUrl"), d.to_string());
                    }
                }
            }
        }
        if let [Some(a), Some(b)] = releases.as_slice()
            && a != b
        {
            e.push("target", format!("FHIR releases differ ({a:?} → {b:?}); version conversion is not supported"));
        }
        if !spec.filters.is_empty() && matches!(spec.source, SyncPeer::Endpoint { .. }) {
            e.push("filters", "filters are supported for external sources only");
        }
        for t in spec.filters.keys().chain(spec.resource_types.iter()) {
            if !crate::change::valid_type(t) {
                e.push("resourceTypes", format!("{t:?} is not a resource type"));
            }
        }
        if spec.initial_load == InitialLoad::StorageCopy && env.copier.is_none() {
            e.push("initialLoad", "storage-copy needs the PostgreSQL backend");
        }
        if spec.enabled {
            let mut edges = Vec::new();
            for j in self.store().jobs().await? {
                if Some(j.id.as_str()) != job_id && j.spec.enabled {
                    edges.push((peer_key(&j.spec.source), peer_key(&j.spec.target)));
                }
            }
            if creates_cycle(&edges, &(peer_key(&spec.source), peer_key(&spec.target))) {
                e.push("target", "an enabled job already syncs back from this target to the source (cycles are refused)");
            }
        }
        e.into_result().map_err(SyncError::Invalid)
    }

    /// Every job with its latest run.
    pub async fn list(&self) -> Result<Vec<JobSummary>, SyncError> {
        let mut out = Vec::new();
        for j in self.store().jobs().await? {
            let last = self.store().runs(&j.id, 1).await?.into_iter().next();
            out.push(JobSummary {
                job: redacted_job(&j),
                last_run: last,
            });
        }
        Ok(out)
    }

    /// One job.
    pub async fn job(&self, id: &str) -> Result<SyncJob, SyncError> {
        self.store()
            .job(id)
            .await?
            .ok_or_else(|| SyncError::NotFound(format!("sync job {id}")))
    }

    /// Creates a job from an API body.
    pub async fn create(&self, body: Value, actor: &str) -> Result<CreatedJob, SyncError> {
        let id = uuid::Uuid::new_v4().to_string();
        let (spec, notify) = spec_from_body(body, &id, None, self.runtime.env.key_ring.as_deref())?;
        self.validate(&spec, None).await?;
        let job = self.store().create_job_with_id(&id, spec, actor).await?;
        self.audit("create", &job, actor, true, "sync job created").await;
        let notify_path = matches!(job.spec.trigger, SyncTrigger::Subscription { .. })
            .then(|| format!("/admin/sync/jobs/{}/notify", job.id));
        Ok(CreatedJob {
            job: redacted_job(&job),
            notify_secret: notify,
            notify_path,
        })
    }

    /// Replaces a job's spec (`expected_revision` from `If-Match`, or the
    /// current one when absent).
    pub async fn update(&self, id: &str, expected_revision: Option<u64>, body: Value, actor: &str) -> Result<SyncJob, SyncError> {
        let current = self.job(id).await?;
        let (spec, _) = spec_from_body(body, id, Some(&current.spec), self.runtime.env.key_ring.as_deref())?;
        self.validate(&spec, Some(id)).await?;
        let rev = expected_revision.unwrap_or(current.revision);
        let disabled = current.spec.enabled && !spec.enabled;
        let job = self.store().update_job(id, rev, spec).await?;
        if disabled && let Some(r) = self.active_run(id).await? {
            self.runtime.cancel_run(&r, "job disabled").await?;
        }
        self.runtime.env.wake.wake(id);
        self.audit("update", &job, actor, true, "sync job updated").await;
        Ok(job)
    }

    /// Deletes a job (cancelling its active run) with its runs and state.
    pub async fn delete(&self, id: &str, actor: &str) -> Result<(), SyncError> {
        let job = self.job(id).await?;
        if let Some(r) = self.active_run(id).await? {
            self.runtime.cancel_run(&r, "job deleted").await?;
        }
        self.store().delete_job(id).await?;
        self.audit("delete", &job, actor, true, "sync job deleted").await;
        Ok(())
    }

    async fn active_run(&self, job_id: &str) -> Result<Option<SyncRun>, SyncError> {
        Ok(self
            .store()
            .runs(job_id, 1)
            .await?
            .into_iter()
            .next()
            .filter(|r| !r.state.is_terminal()))
    }

    /// `$run`: starts a run now (continuing from the last cursor).
    pub async fn run_now(&self, id: &str, actor: &str) -> Result<SyncRun, SyncError> {
        let job = self.job(id).await?;
        let run = self.runtime.start_run(&job, RunReason::Manual).await?;
        self.audit("run", &job, actor, true, &format!("run {} started", run.id)).await;
        Ok(run)
    }

    /// `$retry`: resumes the latest failed or cancelled run's cursor.
    pub async fn retry(&self, id: &str, actor: &str) -> Result<SyncRun, SyncError> {
        let job = self.job(id).await?;
        match self.store().runs(id, 1).await?.into_iter().next() {
            Some(r) if matches!(r.state, SyncRunState::Failed | SyncRunState::Cancelled) => {}
            _ => return Err(SyncError::Conflict("the latest run did not fail; nothing to retry".into())),
        }
        let run = self.runtime.start_run(&job, RunReason::Retry).await?;
        self.audit("retry", &job, actor, true, &format!("run {} resumes", run.id)).await;
        Ok(run)
    }

    /// `$cancel`: cancels the active run, if any.
    pub async fn cancel(&self, id: &str, actor: &str) -> Result<Option<SyncRun>, SyncError> {
        let job = self.job(id).await?;
        let Some(r) = self.active_run(id).await? else {
            return Ok(None);
        };
        self.runtime.cancel_run(&r, &format!("cancelled by {actor}")).await?;
        self.audit("cancel", &job, actor, true, &format!("run {} cancelled", r.id)).await;
        Ok(self.store().run(&r.id).await?)
    }

    /// A job's runs, newest first.
    pub async fn runs(&self, id: &str, limit: usize) -> Result<Vec<SyncRun>, SyncError> {
        self.job(id).await?;
        Ok(self.store().runs(id, limit).await?)
    }

    /// One run.
    pub async fn run(&self, run_id: &str) -> Result<SyncRun, SyncError> {
        self.store()
            .run(run_id)
            .await?
            .ok_or_else(|| SyncError::NotFound(format!("sync run {run_id}")))
    }

    /// Log lines after `after`.
    pub async fn log(&self, run_id: &str, after: u64, limit: usize) -> Result<Vec<SyncLogLine>, SyncError> {
        self.run(run_id).await?;
        Ok(self.store().log_lines(run_id, after, limit.min(1000)).await?)
    }

    /// A run's conflicts.
    pub async fn conflicts(&self, run_id: &str, limit: usize) -> Result<Vec<SyncConflict>, SyncError> {
        self.run(run_id).await?;
        Ok(self.store().conflicts(run_id, limit.min(1000)).await?)
    }

    /// Resolves a conflict: `overwrite` (source wins for this item) or
    /// `keep` (accept the target).
    pub async fn resolve_conflict(
        &self,
        run_id: &str,
        resource_type: &str,
        resource_id: &str,
        action: &str,
        actor: &str,
    ) -> Result<Value, SyncError> {
        let run = self.run(run_id).await?;
        let job = self.job(&run.job_id).await?;
        let exists = self
            .store()
            .conflicts(run_id, 10_000)
            .await?
            .iter()
            .any(|c| c.resource_type == resource_type && c.resource_id == resource_id);
        if !exists {
            return Err(SyncError::NotFound(format!("conflict {resource_type}/{resource_id}")));
        }
        let env = &self.runtime.env;
        let result = match action {
            "overwrite" => {
                let r = overwrite_one(env, &job, resource_type, resource_id).await.map_err(engine_err)?;
                format!("{r:?}")
            }
            "keep" => {
                keep_target(env, &job, resource_type, resource_id).await.map_err(engine_err)?;
                "kept".to_string()
            }
            _ => return Err(invalid("action", "must be overwrite or keep")),
        };
        self.store().delete_conflict(run_id, resource_type, resource_id).await?;
        self.audit(
            "resolve-conflict",
            &job,
            actor,
            true,
            &format!("{resource_type}/{resource_id}: {action} ({result})"),
        )
        .await;
        Ok(json!({"resolved": true, "action": action, "result": result}))
    }

    /// The rest-hook notification receiver: checks the shared secret and
    /// wakes the job. `false` = unknown job or wrong secret (same answer).
    pub async fn notify(&self, job_id: &str, secret: Option<&str>) -> Result<bool, SyncError> {
        let Some(job) = self.store().job(job_id).await? else {
            return Ok(false);
        };
        let Some(expected) = open_notify_secret(&job.spec, &job.id, self.runtime.env.key_ring.as_deref()) else {
            return Ok(false);
        };
        let ok = secret.is_some_and(|s| secrets_equal(s.as_bytes(), expected.expose()));
        if !ok {
            return Ok(false);
        }
        if job.spec.enabled {
            match self.active_run(job_id).await? {
                Some(_) => self.runtime.env.wake.wake(job_id),
                None => match self.runtime.start_run(&job, RunReason::Notification).await {
                    Ok(_) | Err(SyncError::Conflict(_)) => {}
                    Err(e) => return Err(e),
                },
            }
        }
        Ok(true)
    }

    /// Tests the external peers of a job body (CapabilityStatement fetch
    /// through the outbound policy and the given credentials).
    pub async fn test_connection(&self, body: Value) -> Result<Vec<PeerTest>, SyncError> {
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let (spec, _) = spec_from_body(body, &id, None, self.runtime.env.key_ring.as_deref())?;
        let env = &self.runtime.env;
        let mut out = Vec::new();
        for (side, peer) in [("source", &spec.source), ("target", &spec.target)] {
            let mut t = PeerTest {
                side: side.into(),
                ..Default::default()
            };
            match peer {
                SyncPeer::Endpoint { name } => match env.registry.snapshot().by_name(name) {
                    Some(ep) => {
                        t.ok = true;
                        t.fhir_version = Some(ep.spec.fhir_version.fhir_version_string().into());
                        t.software = Some("local endpoint".into());
                    }
                    None => t.error = Some(format!("endpoint {name:?} does not exist")),
                },
                SyncPeer::External(x) => {
                    let r: Result<Value, String> = async {
                        let auth = open_auth(&x.auth, &id, side, env.key_ring.as_deref()).map_err(|e| e.to_string())?;
                        let c = FhirClient::new(env.http.clone(), &x.base_url, auth, x.headers.clone(), x.max_requests_per_second)
                            .map_err(|e| e.to_string())?;
                        ExternalSource::new(Arc::new(c), BTreeMap::new(), false, vec![])
                            .capability()
                            .await
                            .map_err(|e| e.to_string())
                    }
                    .await;
                    match r {
                        Ok(cs) => {
                            t.ok = true;
                            t.fhir_version = capability_release(&cs).map(|r| r.fhir_version_string().to_string());
                            t.resource_types = capability_types(&cs).len();
                            t.software = cs
                                .get("software")
                                .and_then(|s| s.get("name"))
                                .and_then(Value::as_str)
                                .map(str::to_string);
                        }
                        Err(e) => t.error = Some(e),
                    }
                }
            }
            out.push(t);
        }
        Ok(out)
    }

    /// Re-seals every job secret under the active key.
    pub async fn rewrap_secrets(&self) -> Result<usize, SyncError> {
        let ring = self.runtime.env.key_ring.clone().ok_or(SyncError::NoKeyRing)?;
        let mut n = 0;
        for mut j in self.store().jobs().await? {
            let k = crate::secrets::rewrap(&mut j.spec, &j.id, &ring)?;
            if k > 0 {
                self.store().update_job(&j.id, j.revision, j.spec).await?;
                n += k;
            }
        }
        Ok(n)
    }

    /// Checks the peers of a stored job can be built (credentials open,
    /// endpoints exist) — used by the console's job page.
    pub async fn check_job(&self, id: &str) -> Result<(), SyncError> {
        let job = self.job(id).await?;
        build_peers(&self.runtime.env, &job).map(|_| ()).map_err(engine_err)
    }

    /// Duplicates endpoint `source` (design §9.3).
    pub async fn duplicate(&self, source: &str, req: DuplicateRequest, actor: &Actor) -> Result<DuplicateOutcome, SyncError> {
        let endpoints = self
            .runtime
            .endpoints()
            .cloned()
            .ok_or_else(|| SyncError::Unavailable("this server has no endpoint store".into()))?;
        let src = endpoints
            .get(source)
            .await
            .map_err(store_err)?
            .ok_or_else(|| SyncError::NotFound(format!("endpoint {source}")))?;
        let name = EndpointName::parse(&req.name).map_err(|e| invalid("name", e.to_string()))?;
        let path = EndpointPath::parse(req.path.as_deref().unwrap_or(&req.name)).map_err(|e| invalid("path", e.to_string()))?;
        let mut spec: EndpointSpec = src.spec.clone();
        spec.name = name.clone();
        spec.path = path;
        spec.storage = StorageBinding::Tenant {
            tenant_id: name.as_str().to_string(),
        };
        spec.display_name = req
            .display_name
            .clone()
            .unwrap_or_else(|| format!("{} (copy)", src.spec.display_name));
        spec.status = EndpointStatus::Provisioning;
        spec.managed_by = ManagedBy::Ui;
        spec.legacy_tenant_header = false;
        if !req.auth {
            spec.auth = None;
        }
        // Secrets are bound to the endpoint id: create without them, then
        // re-seal under the new id.
        let token = spec.terminology.as_mut().and_then(|t| t.token.take());
        let mut created = endpoints.create(spec, actor).await.map_err(store_err)?;
        if req.secrets
            && let Some(sealed) = token
        {
            let ring = self.runtime.env.key_ring.clone().ok_or(SyncError::NoKeyRing)?;
            let plain = ring.open(&sealed, &format!("endpoint:{}:terminology.token", src.id))?;
            let mut s = created.spec.clone();
            if let Some(t) = s.terminology.as_mut() {
                t.token = Some(ring.seal(plain.expose(), &format!("endpoint:{}:terminology.token", created.id)));
            }
            created = endpoints.update(created.name(), created.revision, s, actor).await.map_err(store_err)?;
        }
        let _ = self.runtime.env.registry.reload_if_stale(endpoints.as_ref()).await;
        let new_name = created.name().to_string();
        if req.data == DuplicateData::None && !req.packages {
            let enabled = endpoints
                .set_status(&new_name, created.revision, EndpointStatus::Enabled, actor)
                .await
                .map_err(store_err)?;
            let _ = self.runtime.env.registry.reload_if_stale(endpoints.as_ref()).await;
            return Ok(DuplicateOutcome {
                endpoint: enabled.redacted(),
                job_id: None,
                run_id: None,
                follow_up_job_id: None,
            });
        }
        let started = Utc::now() - self.runtime.env.settings.safety_lag;
        let mut follow_up_id = None;
        if req.keep_in_sync {
            let f: SyncJobSpec = serde_json::from_value(json!({
                "name": format!("keep {new_name} in sync with {source}"),
                "description": format!("Continuous sync after duplicating endpoint {source} into {new_name}."),
                "source": {"kind": "endpoint", "name": source},
                "target": {"kind": "endpoint", "name": new_name},
                "initialLoad": "none",
                "since": started,
                "trigger": {"type": "continuous", "pollSecs": 5},
                "enabled": false,
            }))
            .expect("follow-up spec");
            let fj = self.store().create_job(f, &actor.subject).await?;
            follow_up_id = Some(fj.id);
        }
        let copy_types: Vec<String> = if req.data == DuplicateData::None {
            CONFORMANCE_TYPES.iter().map(|s| s.to_string()).collect()
        } else {
            vec![]
        };
        let initial = if self.runtime.env.copier.is_some() {
            InitialLoad::StorageCopy
        } else {
            InitialLoad::PagedRead
        };
        let mut description = format!("Endpoint duplication {source} → {new_name}.");
        if let Some(f) = &follow_up_id {
            description.push_str(&format!(" {FOLLOW_UP}{f}"));
        }
        let job_spec: SyncJobSpec = serde_json::from_value(json!({
            "name": format!("duplicate {source} → {new_name} ({})", &uuid::Uuid::new_v4().simple().to_string()[..8]),
            "description": description,
            "source": {"kind": "endpoint", "name": source},
            "target": {"kind": "endpoint", "name": new_name},
            "resourceTypes": copy_types,
            "initialLoad": initial,
            "includeHistory": req.data == DuplicateData::History && initial == InitialLoad::StorageCopy,
            "trigger": {"type": "manual"},
            // `none`: the incremental pass after the copy then has nothing
            // older than the copy to catch up with.
            "propagateDeletes": true,
            "enabled": true,
        }))
        .expect("duplicate spec");
        let job = self.store().create_job(job_spec, &actor.subject).await?;
        let run = self.runtime.start_run(&job, RunReason::Duplicate).await?;
        self.audit(
            "duplicate",
            &job,
            &actor.subject,
            true,
            &format!("endpoint {source} duplicated into {new_name} (data {:?})", req.data),
        )
        .await;
        Ok(DuplicateOutcome {
            endpoint: created.redacted(),
            job_id: Some(job.id),
            run_id: Some(run.id),
            follow_up_job_id: follow_up_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[test]
    fn cycles() {
        let edges = vec![e("A", "B"), e("B", "C")];
        assert!(creates_cycle(&edges, &e("C", "A")));
        assert!(creates_cycle(&edges, &e("B", "A")));
        assert!(!creates_cycle(&edges, &e("A", "C")));
        assert!(!creates_cycle(&edges, &e("D", "A")));
        assert!(creates_cycle(&[], &e("A", "A")));
        // Diamonds are fine.
        let d = vec![e("A", "B"), e("A", "C"), e("B", "D")];
        assert!(!creates_cycle(&d, &e("C", "D")));
    }

    #[test]
    fn peer_keys_normalise() {
        let a: SyncPeer = serde_json::from_value(json!({"kind": "external", "baseUrl": "https://X.org/fhir/"})).unwrap();
        let b: SyncPeer = serde_json::from_value(json!({"kind": "external", "baseUrl": "https://x.org/fhir"})).unwrap();
        assert_eq!(peer_key(&a), peer_key(&b));
        assert_eq!(peer_key(&SyncPeer::Endpoint { name: "lab".into() }), "endpoint:lab");
    }

    #[test]
    fn follow_up_marker() {
        let mut j: SyncJob = serde_json::from_value(json!({
            "id": "j", "revision": 1, "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
            "spec": {"name": "n", "source": {"kind": "endpoint", "name": "a"},
                     "target": {"kind": "endpoint", "name": "b"}}
        }))
        .unwrap();
        assert_eq!(follow_up_job(&j), None);
        j.spec.description = Some(format!("Endpoint duplication a → b. {FOLLOW_UP}abc-123"));
        assert_eq!(follow_up_job(&j).as_deref(), Some("abc-123"));
    }

    #[test]
    fn error_mapping() {
        assert!(matches!(store_err(StoreError::NotFound("x".into())), SyncError::NotFound(_)));
        assert!(matches!(store_err(StoreError::Conflict { current: 2 }), SyncError::Revision { current: 2 }));
        assert!(matches!(store_err(StoreError::Forbidden("no".into())), SyncError::Conflict(_)));
        assert!(matches!(store_err(StoreError::Unavailable("db".into())), SyncError::Unavailable(_)));
        assert!(matches!(store_err(StoreError::Invalid(ValidationErrors::default())), SyncError::Invalid(_)));
        assert!(matches!(engine_err(EngineError::Failed("x".into())), SyncError::Unavailable(_)));
    }
}
