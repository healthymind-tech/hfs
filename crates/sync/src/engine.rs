//! Executes one run of a job (design §9.2): builds the source reader and
//! target writer, runs the initial load (storage copy, paged read or bulk
//! export) and incremental passes, applies batches with item-state
//! idempotency and the conflict policy, and checkpoints cursor + progress
//! after every committed batch under the run's fencing token.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use helios_endpoints::registry::EndpointRegistry;
use helios_endpoints::secret::SecretKeyRing;
use helios_endpoints::sync::{
    InitialLoad, LogLevel, SyncConflict, SyncCursor, SyncItemState, SyncJob, SyncPeer,
    SyncProgress, SyncRun, SyncRunState, SyncStore, SyncTrigger,
};
use helios_persistence::core::WriteObserver;
use tokio::sync::Notify;

use crate::change::Change;
use crate::conflict::{Decision, decide, needs_target, unchanged_since_last_apply};
use crate::copier::{CopyRequest, TenantCopier};
use crate::cursor::{
    PHASE_INITIAL, admit, advance, batches, install_plan, needs_plan, next_type, order_and_dedupe,
    plan_types, start_cursor,
};
use crate::error::EngineError;
use crate::external::{ExternalSource, ExternalTarget};
use crate::hash::content_hash;
use crate::http::{FhirClient, GuardedHttp};
use crate::local::{LocalSource, LocalStore, LocalTarget, LocalWriteHooks, fhir_version_of};
use crate::peer::{SourceReader, TargetWriter, WriteOp, WriteResult};
use crate::secrets::open_auth;

/// Engine knobs (env: `HFS_SYNC_*`).
#[derive(Debug, Clone)]
pub struct EngineSettings {
    /// The high-water mark never advances past `now - lag`.
    pub safety_lag: ChronoDuration,
    /// External peers allowed (`HFS_SYNC_EXTERNAL`).
    pub external_allowed: bool,
    /// Resources per source page.
    pub page_size: u32,
    /// Floor of the continuous poll period (tests lower it).
    pub min_poll: Duration,
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self {
            safety_lag: ChronoDuration::milliseconds(2000),
            external_allowed: true,
            page_size: 200,
            min_poll: Duration::from_secs(1),
        }
    }
}

/// Wake-ups for continuous / subscription-triggered runs on this instance.
#[derive(Default)]
pub struct WakeMap {
    map: std::sync::Mutex<HashMap<String, Arc<Notify>>>,
}

impl WakeMap {
    /// The notifier of a job.
    pub fn get(&self, job_id: &str) -> Arc<Notify> {
        self.map
            .lock()
            .unwrap()
            .entry(job_id.to_string())
            .or_default()
            .clone()
    }

    /// Wakes a job's run if it runs here.
    pub fn wake(&self, job_id: &str) {
        self.get(job_id).notify_one();
    }
}

/// Everything a run needs.
pub struct EngineEnv {
    /// Jobs, runs, logs, item state.
    pub store: Arc<dyn SyncStore>,
    /// Endpoints by name → tenant, release, status.
    pub registry: Arc<EndpointRegistry>,
    /// In-process storage for local endpoints.
    pub local: Option<Arc<dyn LocalStore>>,
    /// Set-based tenant copy (PostgreSQL).
    pub copier: Option<Arc<dyn TenantCopier>>,
    /// SSRF-guarded HTTP.
    pub http: Arc<GuardedHttp>,
    /// Secrets.
    pub key_ring: Option<Arc<SecretKeyRing>>,
    /// Write observers of local target writes.
    pub observer: Option<Arc<dyn WriteObserver>>,
    /// The server's normal write path for local targets (validation,
    /// profile registry); set by the REST layer once its state exists.
    pub hooks: std::sync::OnceLock<Arc<dyn LocalWriteHooks>>,
    /// Knobs.
    pub settings: EngineSettings,
    /// Wake-ups.
    pub wake: WakeMap,
}

/// The run being worked: id, fencing token, stop flag.
#[derive(Clone)]
pub struct RunHandle {
    /// Run id.
    pub run_id: String,
    /// Fencing token of the lease.
    pub token: i64,
    /// Set when the run must stop (cancel, lease lost, shutdown).
    pub stop: Arc<AtomicBool>,
}

impl RunHandle {
    fn check(&self) -> Result<(), EngineError> {
        if self.stop.load(Ordering::Relaxed) {
            Err(EngineError::Stopped("cancelled or lease lost".into()))
        } else {
            Ok(())
        }
    }
}

/// A resolved side.
pub struct Peers {
    /// Reader.
    pub source: Box<dyn SourceReader>,
    /// Writer.
    pub target: Box<dyn TargetWriter>,
    /// Source tenant when local.
    pub source_tenant: Option<String>,
    /// Target tenant when local.
    pub target_tenant: Option<String>,
}

fn local_endpoint(
    env: &EngineEnv,
    name: &str,
) -> Result<(String, helios_endpoints::FhirRelease), EngineError> {
    let snap = env.registry.snapshot();
    let ep = snap
        .by_name(name)
        .ok_or_else(|| EngineError::Failed(format!("endpoint {name:?} does not exist")))?;
    Ok((ep.tenant_id().to_string(), ep.spec.fhir_version))
}

/// Builds the reader and writer of a job.
pub fn build_peers(env: &EngineEnv, job: &SyncJob) -> Result<Peers, EngineError> {
    let spec = &job.spec;
    let ring = env.key_ring.as_deref();
    let client = |side: &str,
                  x: &helios_endpoints::sync::ExternalServer|
     -> Result<Arc<FhirClient>, EngineError> {
        if !env.settings.external_allowed {
            return Err(EngineError::Failed(
                "external sync peers are disabled on this server (HFS_SYNC_EXTERNAL=off)".into(),
            ));
        }
        let auth = open_auth(&x.auth, &job.id, side, ring)
            .map_err(|e| EngineError::Failed(e.to_string()))?;
        FhirClient::new(
            env.http.clone(),
            &x.base_url,
            auth,
            x.headers.clone(),
            x.max_requests_per_second,
        )
        .map(Arc::new)
        .map_err(|e| EngineError::Failed(e.to_string()))
    };
    let local = || {
        env.local.clone().ok_or_else(|| {
            EngineError::Failed("this server has no storage access for local endpoints".into())
        })
    };
    let (source, source_tenant): (Box<dyn SourceReader>, Option<String>) = match &spec.source {
        SyncPeer::Endpoint { name } => {
            let (tenant, _) = local_endpoint(env, name)?;
            (Box::new(LocalSource::new(local()?, &tenant)), Some(tenant))
        }
        SyncPeer::External(x) => (
            Box::new(ExternalSource::new(
                client("source", x)?,
                spec.filters.clone(),
                spec.initial_load == InitialLoad::BulkExport,
                spec.resource_types.clone(),
            )),
            None,
        ),
    };
    let (target, target_tenant): (Box<dyn TargetWriter>, Option<String>) = match &spec.target {
        SyncPeer::Endpoint { name } => {
            let (tenant, release) = local_endpoint(env, name)?;
            let version = fhir_version_of(release).ok_or_else(|| {
                EngineError::Failed(format!("FHIR {release:?} is not compiled into this server"))
            })?;
            (
                Box::new(
                    LocalTarget::new(local()?, &tenant, version, env.observer.clone())
                        .with_hooks(name, env.hooks.get().cloned()),
                ),
                Some(tenant),
            )
        }
        SyncPeer::External(x) => (Box::new(ExternalTarget::new(client("target", x)?)), None),
    };
    Ok(Peers {
        source,
        target,
        source_tenant,
        target_tenant,
    })
}

/// The run's mutable state and its checkpointing.
pub struct RunState<'a> {
    env: &'a EngineEnv,
    job: &'a SyncJob,
    handle: &'a RunHandle,
    /// Cursor.
    pub cursor: SyncCursor,
    /// Counters.
    pub progress: SyncProgress,
}

impl RunState<'_> {
    async fn checkpoint(
        &self,
        state: SyncRunState,
        error: Option<&str>,
    ) -> Result<(), EngineError> {
        self.env
            .store
            .checkpoint(
                &self.handle.run_id,
                self.handle.token,
                state,
                &self.cursor,
                &self.progress,
                error,
            )
            .await
            .map_err(EngineError::from)
    }

    async fn log(&self, level: LogLevel, msg: impl Into<String>) {
        let msg = msg.into();
        if let Err(e) = self.env.store.log(&self.handle.run_id, level, &msg).await {
            tracing::warn!(run = %self.handle.run_id, error = %e, "sync log write failed");
        }
    }

    /// Applies one batch of changes (oldest first) at the target.
    async fn apply(
        &mut self,
        target: &dyn TargetWriter,
        changes: &[Change],
    ) -> Result<(), EngineError> {
        if changes.is_empty() {
            return Ok(());
        }
        self.handle.check()?;
        let policy = self.job.spec.conflict;
        let keys: Vec<(String, String)> = changes
            .iter()
            .map(|c| (c.resource_type.clone(), c.id.clone()))
            .collect();
        let states: HashMap<(String, String), SyncItemState> = self
            .env
            .store
            .item_states(&self.job.id, &keys)
            .await?
            .into_iter()
            .map(|s| ((s.resource_type.clone(), s.resource_id.clone()), s))
            .collect();
        let mut ops: Vec<PlannedWrite<'_>> = Vec::new();
        let mut new_states: Vec<SyncItemState> = Vec::new();
        let now = Utc::now();
        for c in changes {
            self.progress.read += 1;
            if c.deleted && !self.job.spec.propagate_deletes {
                self.progress.skipped += 1;
                continue;
            }
            let hash = c.resource.as_ref().map(content_hash);
            let state = states.get(&(c.resource_type.clone(), c.id.clone()));
            if matches!(policy, helios_endpoints::sync::ConflictPolicy::SourceWins)
                && unchanged_since_last_apply(state, c.deleted, hash.as_ref())
            {
                self.progress.skipped += 1;
                continue;
            }
            let target_info = if needs_target(policy, c.deleted) {
                target
                    .current(&c.resource_type, &c.id)
                    .await
                    .map_err(|e| EngineError::Failed(e.to_string()))?
            } else {
                None
            };
            match decide(
                policy,
                state,
                c.deleted,
                hash.as_ref(),
                c.last_updated,
                target_info.as_ref(),
            ) {
                Decision::Skip => {
                    self.progress.skipped += 1;
                    if state.is_none() {
                        new_states.push(SyncItemState {
                            resource_type: c.resource_type.clone(),
                            resource_id: c.id.clone(),
                            source_version: c.version.clone(),
                            target_version: target_info.as_ref().and_then(|t| t.version.clone()),
                            content_sha256: hash.map(|h| h.to_vec()),
                            deleted: c.deleted,
                            applied_at: now,
                        });
                    }
                }
                Decision::Conflict(reason) => {
                    self.record_conflict(
                        c,
                        target_info.as_ref().and_then(|t| t.version.clone()),
                        &reason,
                    )
                    .await?;
                }
                Decision::Apply { if_match } => {
                    let op = if c.deleted {
                        WriteOp::Delete {
                            resource_type: c.resource_type.clone(),
                            id: c.id.clone(),
                            if_match,
                        }
                    } else {
                        WriteOp::Put {
                            resource_type: c.resource_type.clone(),
                            id: c.id.clone(),
                            resource: c.resource.clone().unwrap_or_default(),
                            if_match,
                        }
                    };
                    ops.push((op, c, hash));
                }
            }
        }
        if !ops.is_empty() {
            let mut write_ops = Vec::with_capacity(ops.len());
            let mut meta = Vec::with_capacity(ops.len());
            for (o, c, h) in ops {
                write_ops.push(o);
                meta.push((c, h));
            }
            let results = target
                .apply(write_ops)
                .await
                .map_err(|e| EngineError::Failed(format!("target write failed: {e}")))?;
            for ((c, hash), r) in meta.into_iter().zip(results) {
                let target_version = match &r {
                    WriteResult::Created(v) => {
                        self.progress.created += 1;
                        v.clone()
                    }
                    WriteResult::Updated(v) => {
                        self.progress.updated += 1;
                        v.clone()
                    }
                    WriteResult::Deleted => {
                        self.progress.deleted += 1;
                        None
                    }
                    WriteResult::Conflict(reason) => {
                        self.record_conflict(c, None, reason).await?;
                        continue;
                    }
                    WriteResult::Invalid(reason) => {
                        // A write the endpoint's validation refused: a run
                        // error, listed with the conflicts (with its
                        // OperationOutcome) so an operator can act on it.
                        self.progress.failed += 1;
                        self.env
                            .store
                            .conflict(&SyncConflict {
                                run_id: self.handle.run_id.clone(),
                                resource_type: c.resource_type.clone(),
                                resource_id: c.id.clone(),
                                source_version: c.version.clone(),
                                target_version: None,
                                reason: format!("rejected by target validation: {reason}"),
                                at: Utc::now(),
                            })
                            .await?;
                        let short: String = reason.chars().take(300).collect();
                        self.log(
                            LogLevel::Error,
                            format!("{}: rejected by target validation: {short}", c.token()),
                        )
                        .await;
                        continue;
                    }
                    WriteResult::Failed(reason) => {
                        self.progress.failed += 1;
                        self.log(LogLevel::Error, format!("{}: {reason}", c.token()))
                            .await;
                        continue;
                    }
                };
                new_states.push(SyncItemState {
                    resource_type: c.resource_type.clone(),
                    resource_id: c.id.clone(),
                    source_version: c.version.clone(),
                    target_version,
                    content_sha256: hash.map(|h| h.to_vec()),
                    deleted: c.deleted,
                    applied_at: now,
                });
            }
        }
        let findings = target.take_findings();
        let total = findings.len();
        for f in findings.into_iter().take(50) {
            self.log(LogLevel::Warn, format!("validation (log): {f}")).await;
        }
        if total > 50 {
            self.log(
                LogLevel::Warn,
                format!("validation (log): {} more findings not shown", total - 50),
            )
            .await;
        }
        self.env
            .store
            .put_item_states(&self.job.id, &new_states)
            .await?;
        Ok(())
    }

    async fn record_conflict(
        &mut self,
        c: &Change,
        target_version: Option<String>,
        reason: &str,
    ) -> Result<(), EngineError> {
        self.progress.conflicts += 1;
        self.env
            .store
            .conflict(&SyncConflict {
                run_id: self.handle.run_id.clone(),
                resource_type: c.resource_type.clone(),
                resource_id: c.id.clone(),
                source_version: c.version.clone(),
                target_version,
                reason: reason.to_string(),
                at: Utc::now(),
            })
            .await?;
        self.log(LogLevel::Warn, format!("conflict {}: {reason}", c.token()))
            .await;
        Ok(())
    }
}

/// A write decided for one change, with the change and its content hash.
type PlannedWrite<'a> = (WriteOp, &'a Change, Option<[u8; 32]>);

/// Whether source and target endpoints apply the same write policy
/// (validation and terminology sections, inherited counting as equal).
pub fn same_write_policy(env: &EngineEnv, spec: &helios_endpoints::sync::SyncJobSpec) -> bool {
    let (SyncPeer::Endpoint { name: a }, SyncPeer::Endpoint { name: b }) = (&spec.source, &spec.target)
    else {
        return false;
    };
    let snap = env.registry.snapshot();
    match (snap.by_name(a), snap.by_name(b)) {
        (Some(x), Some(y)) => {
            x.spec.validation == y.spec.validation && x.spec.terminology == y.spec.terminology
        }
        _ => false,
    }
}

/// After an in-database copy: hand the copied StructureDefinitions to the
/// target's write hooks so this instance's validation profile registry has
/// them at once (other instances follow the database change feed).
async fn register_copied_definitions(st: &RunState<'_>, tenant: &str) -> Result<(), EngineError> {
    let (Some(hooks), Some(local), SyncPeer::Endpoint { name }) =
        (st.env.hooks.get(), st.env.local.clone(), &st.job.spec.target)
    else {
        return Ok(());
    };
    let source = LocalSource::new(local, tenant);
    let mut page: Option<String> = None;
    let mut n = 0usize;
    loop {
        let p = source
            .initial_page("StructureDefinition", page.as_deref(), None, 200)
            .await
            .map_err(|e| EngineError::Failed(format!("reading copied definitions: {e}")))?;
        for c in &p.changes {
            if let Some(r) = &c.resource {
                hooks.written(name, "StructureDefinition", r);
                n += 1;
            }
        }
        match p.next {
            Some(next) => page = Some(next),
            None => break,
        }
    }
    if n > 0 {
        st.log(
            LogLevel::Info,
            format!("{n} copied StructureDefinitions registered for validation"),
        )
        .await;
    }
    Ok(())
}

/// Runs the initial load until done (or stopped).
async fn initial_load(st: &mut RunState<'_>, peers: &Peers) -> Result<(), EngineError> {
    let spec = &st.job.spec;
    let mut in_db = spec.initial_load == InitialLoad::StorageCopy;
    if in_db && st.env.hooks.get().is_some() && !same_write_policy(st.env, spec) {
        // The in-database copy skips write validation; that is only sound
        // when the target validates exactly like the source did.
        st.log(
            LogLevel::Warn,
            "source and target endpoints validate differently: copying through the target's validation path (paged read) instead of the in-database copy",
        )
        .await;
        in_db = false;
    }
    if in_db {
        let copier = st.env.copier.clone().ok_or_else(|| {
            EngineError::Failed("storage-copy needs the PostgreSQL backend".into())
        })?;
        let (Some(src), Some(dst)) = (&peers.source_tenant, &peers.target_tenant) else {
            return Err(EngineError::Failed(
                "storage-copy works only between local endpoints".into(),
            ));
        };
        st.log(
            LogLevel::Info,
            format!("storage copy of tenant {src} into {dst} started"),
        )
        .await;
        let report = copier
            .copy(CopyRequest {
                source_tenant: src.clone(),
                target_tenant: dst.clone(),
                include_history: spec.include_history,
                resource_types: (!spec.resource_types.is_empty())
                    .then(|| spec.resource_types.clone()),
                copy_packages: true,
            })
            .await
            .map_err(|e| EngineError::Failed(format!("storage copy failed: {e}")))?;
        st.progress.read += report.resources;
        st.progress.created += report.resources;
        st.log(
            LogLevel::Info,
            format!(
                "storage copy finished: {} resources ({})",
                report.resources, report.summary
            ),
        )
        .await;
        register_copied_definitions(st, dst).await?;
        install_plan(&mut st.cursor, vec![]);
        st.checkpoint(SyncRunState::Running, None).await?;
        return Ok(());
    }
    if needs_plan(&st.cursor) {
        let available = peers
            .source
            .initial_types()
            .await
            .map_err(|e| EngineError::Failed(format!("listing source types: {e}")))?;
        let plan = plan_types(spec, &available);
        st.log(
            LogLevel::Info,
            format!("initial load planned: {}", plan.join(", ")),
        )
        .await;
        install_plan(&mut st.cursor, plan);
        st.checkpoint(SyncRunState::Running, None).await?;
    }
    let count = spec.batch_size.min(st.env.settings.page_size.max(1)).max(1);
    while st.cursor.phase == PHASE_INITIAL {
        st.handle.check()?;
        let Some(rt) = st.cursor.current_type.clone() else {
            next_type(&mut st.cursor);
            continue;
        };
        let page = peers
            .source
            .initial_page(&rt, st.cursor.page.as_deref(), spec.since, count)
            .await
            .map_err(|e| EngineError::Failed(format!("reading {rt}: {e}")))?;
        let changes: Vec<Change> = page
            .changes
            .into_iter()
            .filter(|c| spec.includes_type(&c.resource_type))
            .collect();
        st.apply(peers.target.as_ref(), &changes).await?;
        match page.next {
            Some(n) => st.cursor.page = Some(n),
            None => {
                st.log(LogLevel::Info, format!("initial load of {rt} finished"))
                    .await;
                next_type(&mut st.cursor);
            }
        }
        st.checkpoint(SyncRunState::Running, None).await?;
    }
    Ok(())
}

/// One incremental pass: every change since the high-water mark.
async fn incremental_pass(st: &mut RunState<'_>, peers: &Peers) -> Result<u64, EngineError> {
    let spec = &st.job.spec;
    let since = st.cursor.high_water.unwrap_or_else(Utc::now);
    let limit = Utc::now() - st.env.settings.safety_lag;
    let count = st.env.settings.page_size.max(1);
    let before = st.progress.clone();
    let mut page: Option<String> = None;
    let mut first = true;
    loop {
        st.handle.check()?;
        let w = peers
            .source
            .changes_since(since, page.as_deref(), count)
            .await
            .map_err(|e| EngineError::Failed(format!("reading changes: {e}")))?;
        if first && !w.neutral.is_empty() {
            let neutral: Vec<Change> = order_and_dedupe(
                w.neutral
                    .into_iter()
                    .filter(|c| spec.includes_type(&c.resource_type))
                    .collect(),
            );
            for b in batches(neutral, spec.batch_size as usize) {
                st.apply(peers.target.as_ref(), &b).await?;
            }
        }
        first = false;
        let admitted = admit(
            &st.cursor,
            w.changes
                .into_iter()
                .filter(|c| spec.includes_type(&c.resource_type))
                .collect(),
        );
        for b in batches(order_and_dedupe(admitted), spec.batch_size as usize) {
            st.apply(peers.target.as_ref(), &b).await?;
            advance(&mut st.cursor, &b, limit);
            st.checkpoint(SyncRunState::Running, None).await?;
        }
        match w.next {
            Some(n) => page = Some(n),
            None => break,
        }
    }
    let applied = (st.progress.created + st.progress.updated + st.progress.deleted)
        - (before.created + before.updated + before.deleted);
    Ok(applied)
}

fn poll_period(job: &SyncJob, min: Duration) -> Option<Duration> {
    match job.spec.trigger {
        SyncTrigger::Continuous { poll_secs } => {
            Some(Duration::from_secs(poll_secs.into()).max(min))
        }
        SyncTrigger::Subscription {
            fallback_poll_secs, ..
        } => Some(Duration::from_secs(fallback_poll_secs.max(1).into()).max(min)),
        _ => None,
    }
}

/// Executes a run to its end and writes the final checkpoint. `Ok(state)`
/// is the terminal state recorded; `Err(Stopped)` means the run was taken
/// away (cancel / lease loss) and nothing more may be written.
pub async fn execute(
    env: &EngineEnv,
    job: &SyncJob,
    run: &SyncRun,
    handle: &RunHandle,
) -> Result<SyncRunState, EngineError> {
    let cursor = start_cursor(
        &job.spec,
        Some(&run.cursor),
        Utc::now(),
        job.created_at,
        env.settings.safety_lag,
    );
    let mut st = RunState {
        env,
        job,
        handle,
        cursor,
        progress: run.progress.clone(),
    };
    let outcome = drive(&mut st, run).await;
    match outcome {
        Ok(()) => {
            st.log(
                LogLevel::Info,
                format!(
                    "run finished: read {}, created {}, updated {}, deleted {}, skipped {}, conflicts {}, failed {}",
                    st.progress.read,
                    st.progress.created,
                    st.progress.updated,
                    st.progress.deleted,
                    st.progress.skipped,
                    st.progress.conflicts,
                    st.progress.failed
                ),
            )
            .await;
            st.checkpoint(SyncRunState::Succeeded, None).await?;
            Ok(SyncRunState::Succeeded)
        }
        Err(EngineError::Failed(msg)) => {
            st.log(LogLevel::Error, format!("run failed: {msg}")).await;
            st.checkpoint(SyncRunState::Failed, Some(&msg)).await?;
            Ok(SyncRunState::Failed)
        }
        Err(stopped) => Err(stopped),
    }
}

async fn drive(st: &mut RunState<'_>, run: &SyncRun) -> Result<(), EngineError> {
    let peers = build_peers(st.env, st.job)?;
    st.log(
        LogLevel::Info,
        format!("run started ({:?}, phase {})", run.reason, st.cursor.phase),
    )
    .await;
    st.checkpoint(SyncRunState::Running, None).await?;
    if st.cursor.phase == PHASE_INITIAL {
        initial_load(st, &peers).await?;
    }
    let wake = st.env.wake.get(&st.job.id);
    let mut job = st.job.clone();
    loop {
        let n = incremental_pass(st, &peers).await?;
        if n > 0 {
            st.log(
                LogLevel::Info,
                format!("incremental pass applied {n} changes"),
            )
            .await;
        }
        let Some(period) = poll_period(&job, st.env.settings.min_poll) else {
            return Ok(());
        };
        // Continuous: wait for the period, a wake-up, or a stop.
        let deadline = tokio::time::Instant::now() + period;
        loop {
            st.handle.check()?;
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            let slice = (deadline - now).min(Duration::from_millis(250));
            if tokio::time::timeout(slice, wake.notified()).await.is_ok() {
                break;
            }
        }
        // The job may have been disabled, changed or deleted meanwhile.
        match st.env.store.job(&job.id).await? {
            Some(j) if j.spec.enabled => job = j,
            Some(_) => {
                st.log(LogLevel::Info, "job disabled; continuous run ends")
                    .await;
                return Ok(());
            }
            None => return Err(EngineError::Stopped("job deleted".into())),
        }
    }
}

/// Resolves one recorded conflict by writing the source's current state to
/// the target unconditionally ("overwrite"). Returns what was written.
pub async fn overwrite_one(
    env: &EngineEnv,
    job: &SyncJob,
    resource_type: &str,
    id: &str,
) -> Result<WriteResult, EngineError> {
    let peers = build_peers(env, job)?;
    let current = peers
        .source
        .read(resource_type, id)
        .await
        .map_err(|e| EngineError::Failed(e.to_string()))?;
    let (op, change) = match current {
        Some(c) => (
            WriteOp::Put {
                resource_type: resource_type.into(),
                id: id.into(),
                resource: c.resource.clone().unwrap_or_default(),
                if_match: None,
            },
            c,
        ),
        None => (
            WriteOp::Delete {
                resource_type: resource_type.into(),
                id: id.into(),
                if_match: None,
            },
            Change::delete(resource_type, id, None, Utc::now()),
        ),
    };
    let r = peers
        .target
        .apply(vec![op])
        .await
        .map_err(|e| EngineError::Failed(e.to_string()))?
        .pop()
        .unwrap_or(WriteResult::Failed("no result".into()));
    let target_version = match &r {
        WriteResult::Created(v) | WriteResult::Updated(v) => v.clone(),
        WriteResult::Deleted => None,
        other => return Ok(other.clone()),
    };
    env.store
        .put_item_states(
            &job.id,
            &[SyncItemState {
                resource_type: resource_type.into(),
                resource_id: id.into(),
                source_version: change.version.clone(),
                target_version,
                content_sha256: change.resource.as_ref().map(|r| content_hash(r).to_vec()),
                deleted: change.deleted,
                applied_at: Utc::now(),
            }],
        )
        .await?;
    Ok(r)
}

/// Accepts the target's current state for one conflict ("keep"): the item
/// state records it, so a `fail` job stops reporting it until either side
/// changes again.
pub async fn keep_target(
    env: &EngineEnv,
    job: &SyncJob,
    resource_type: &str,
    id: &str,
) -> Result<(), EngineError> {
    let peers = build_peers(env, job)?;
    let t = peers
        .target
        .current(resource_type, id)
        .await
        .map_err(|e| EngineError::Failed(e.to_string()))?;
    let s = peers
        .source
        .read(resource_type, id)
        .await
        .map_err(|e| EngineError::Failed(e.to_string()))?;
    env.store
        .put_item_states(
            &job.id,
            &[SyncItemState {
                resource_type: resource_type.into(),
                resource_id: id.into(),
                source_version: s.as_ref().and_then(|c| c.version.clone()),
                target_version: t.as_ref().and_then(|t| t.version.clone()),
                content_sha256: s
                    .as_ref()
                    .and_then(|c| c.resource.as_ref())
                    .map(|r| content_hash(r).to_vec()),
                deleted: s.is_none(),
                applied_at: Utc::now(),
            }],
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_periods() {
        let mut job: SyncJob = serde_json::from_value(serde_json::json!({
            "id": "j", "revision": 1, "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
            "spec": {"name": "n", "source": {"kind": "endpoint", "name": "a"},
                     "target": {"kind": "endpoint", "name": "b"}}
        }))
        .unwrap();
        assert_eq!(poll_period(&job, Duration::from_secs(1)), None);
        job.spec.trigger = SyncTrigger::Continuous { poll_secs: 0 };
        assert_eq!(
            poll_period(&job, Duration::from_secs(1)),
            Some(Duration::from_secs(1))
        );
        job.spec.trigger = SyncTrigger::Subscription {
            fallback_poll_secs: 30,
            notify_secret: None,
        };
        assert_eq!(
            poll_period(&job, Duration::from_millis(10)),
            Some(Duration::from_secs(30))
        );
        let w = WakeMap::default();
        let n = w.get("j");
        w.wake("j");
        assert!(Arc::ptr_eq(&n, &w.get("j")));
    }
}
