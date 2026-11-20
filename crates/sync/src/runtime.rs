//! Cluster-safe execution (design §9.2): runs are `cluster_jobs` rows of
//! kind `endpoint-sync`, claimed with `FOR UPDATE SKIP LOCKED` by any
//! instance's worker, heartbeated, and fenced (the run row records the
//! lease token; a stale worker's checkpoint is refused). A scheduler on
//! every instance enqueues due interval runs and keeps continuous jobs
//! running; the one-active-run-per-job unique index makes concurrent
//! schedulers harmless. Queued runs whose cluster job vanished (a crash
//! between recording and enqueueing) are re-enqueued by the orphan sweep.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use helios_endpoints::model::EndpointStatus;
use helios_endpoints::store::{Actor, EndpointStore};
use helios_endpoints::sync::{
    RunReason, SyncJob, SyncRun, SyncRunState, SyncStoreError, SyncTrigger,
};
use helios_persistence::core::{
    ClusterJobId, ClusterJobLease, ClusterJobStore, ClusterLeaseError, JobKind, WorkerId,
};
use helios_persistence::tenant::TenantContext;
use serde_json::json;
use tokio::task::JoinHandle;

use crate::engine::{EngineEnv, RunHandle, execute};
use crate::error::{EngineError, SyncError};

/// Records sync events as audit events (AuditEvent in the default tenant,
/// agent `Device/hfs-sync`); implemented over `helios-audit` by the server.
#[async_trait]
pub trait SyncAuditor: Send + Sync {
    /// Records `action` (`create`, `update`, `delete`, `run`, `cancel`,
    /// `run-finished`…) on `job` by `actor` with outcome and description.
    async fn record(
        &self,
        action: &str,
        job_id: &str,
        job_name: &str,
        actor: &str,
        success: bool,
        description: &str,
    );
}

/// Worker / scheduler knobs (env `HFS_SYNC_*`).
#[derive(Debug, Clone)]
pub struct RuntimeSettings {
    /// Concurrent runs per instance (`HFS_SYNC_WORKERS`, default 2).
    pub workers: usize,
    /// Lease duration (60 s).
    pub lease: Duration,
    /// Heartbeat period (10 s).
    pub heartbeat: Duration,
    /// Idle claim poll (2 s).
    pub idle_poll: Duration,
    /// Scheduler period (5 s).
    pub scheduler_tick: Duration,
    /// Queued runs older than this are checked by the orphan sweep (60 s).
    pub orphan_after: Duration,
    /// Restart delay of a failed continuous run (60 s).
    pub continuous_retry: Duration,
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            workers: 2,
            lease: Duration::from_secs(60),
            heartbeat: Duration::from_secs(10),
            idle_poll: Duration::from_secs(2),
            scheduler_tick: Duration::from_secs(5),
            orphan_after: Duration::from_secs(60),
            continuous_retry: Duration::from_secs(60),
        }
    }
}

/// The run queue, workers and scheduler of one instance.
pub struct SyncRuntime {
    /// Engine environment.
    pub env: Arc<EngineEnv>,
    cluster: Arc<dyn ClusterJobStore>,
    instance: String,
    settings: RuntimeSettings,
    endpoints: Option<Arc<dyn EndpointStore>>,
    audit: Option<Arc<dyn SyncAuditor>>,
    shutdown: Arc<AtomicBool>,
    /// Wakes idle local workers when this instance enqueues a run.
    queued: tokio::sync::Notify,
    /// Source tenant → continuous jobs reading it (local writes wake them).
    tenant_jobs: std::sync::RwLock<std::collections::HashMap<String, Vec<String>>>,
    /// Cross-instance wake-ups (PostgreSQL NOTIFY).
    remote_wake: std::sync::OnceLock<Arc<dyn RemoteWake>>,
}

/// Publishes a job wake-up to every instance (the one holding the run
/// acts on it).
#[async_trait]
pub trait RemoteWake: Send + Sync {
    /// Announces `job_id`.
    async fn publish(&self, job_id: &str);
}

/// The tenant `cluster_jobs` rows of sync runs belong to.
pub fn queue_tenant() -> TenantContext {
    TenantContext::system()
}

impl SyncRuntime {
    /// A runtime; `instance` names this process in leases.
    pub fn new(
        env: Arc<EngineEnv>,
        cluster: Arc<dyn ClusterJobStore>,
        instance: impl Into<String>,
        settings: RuntimeSettings,
        endpoints: Option<Arc<dyn EndpointStore>>,
        audit: Option<Arc<dyn SyncAuditor>>,
    ) -> Self {
        Self {
            env,
            cluster,
            instance: instance.into(),
            settings,
            endpoints,
            audit,
            shutdown: Arc::new(AtomicBool::new(false)),
            queued: tokio::sync::Notify::new(),
            tenant_jobs: Default::default(),
            remote_wake: std::sync::OnceLock::new(),
        }
    }

    /// The endpoint store, when this server manages endpoints.
    pub fn endpoints(&self) -> Option<&Arc<dyn EndpointStore>> {
        self.endpoints.as_ref()
    }

    /// The auditor.
    pub fn auditor(&self) -> Option<&Arc<dyn SyncAuditor>> {
        self.audit.as_ref()
    }

    /// Installs the cross-instance wake publisher.
    pub fn set_remote_wake(&self, w: Arc<dyn RemoteWake>) -> bool {
        self.remote_wake.set(w).is_ok()
    }

    /// Wakes a job's run here and, through the publisher, on every instance.
    pub async fn wake_everywhere(&self, job_id: &str) {
        self.env.wake.wake(job_id);
        if let Some(w) = self.remote_wake.get() {
            w.publish(job_id).await;
        }
    }

    /// A local write committed in `tenant`: wakes the continuous jobs that
    /// read it (cheap; called from a write observer).
    pub fn wake_tenant(&self, tenant: &str) {
        let map = self.tenant_jobs.read().unwrap_or_else(|p| p.into_inner());
        if let Some(jobs) = map.get(tenant) {
            for j in jobs {
                self.env.wake.wake(j);
            }
        }
    }

    /// Rebuilds the tenant → continuous-job index.
    pub async fn refresh_wake_index(&self) {
        let Ok(jobs) = self.env.store.jobs().await else {
            return;
        };
        let snap = self.env.registry.snapshot();
        let mut map: std::collections::HashMap<String, Vec<String>> = Default::default();
        for j in jobs.iter().filter(|j| j.spec.enabled) {
            if let (SyncTrigger::Continuous { .. }, helios_endpoints::sync::SyncPeer::Endpoint { name }) =
                (&j.spec.trigger, &j.spec.source)
                && let Some(ep) = snap.by_name(name)
            {
                map.entry(ep.tenant_id().to_string()).or_default().push(j.id.clone());
            }
        }
        *self.tenant_jobs.write().unwrap_or_else(|p| p.into_inner()) = map;
    }

    /// Stops claiming new runs; running ones stop at their next check.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Enqueues the cluster job carrying a queued run.
    pub async fn enqueue(&self, run: &SyncRun) -> Result<(), SyncError> {
        let id = self
            .cluster
            .enqueue(
                &queue_tenant(),
                JobKind::EndpointSync,
                json!({"runId": run.id, "jobId": run.job_id}),
            )
            .await
            .map_err(|e| SyncError::Unavailable(format!("job queue: {e}")))?;
        self.env
            .store
            .attach_cluster_job(&run.id, id.as_str())
            .await?;
        self.queued.notify_one();
        Ok(())
    }

    /// Records a run and enqueues it.
    pub async fn start_run(&self, job: &SyncJob, reason: RunReason) -> Result<SyncRun, SyncError> {
        let previous = self.env.store.runs(&job.id, 1).await?.into_iter().next();
        let resume = previous.map(|r| r.cursor).filter(|c| !c.phase.is_empty());
        let run = self.env.store.create_run(&job.id, reason, resume).await?;
        self.enqueue(&run).await?;
        Ok(run)
    }

    /// Cancels a run: the run row and its cluster job.
    pub async fn cancel_run(&self, run: &SyncRun, reason: &str) -> Result<bool, SyncError> {
        let changed = self.env.store.cancel_run(&run.id, reason).await?;
        if let Some(cj) = &run.cluster_job_id {
            let _ = self
                .cluster
                .cancel(&queue_tenant(), &ClusterJobId::from_string(cj.clone()))
                .await;
        }
        self.env.wake.wake(&run.job_id);
        Ok(changed)
    }

    /// Starts the workers and the scheduler.
    pub fn spawn(self: &Arc<Self>) -> Vec<JoinHandle<()>> {
        let mut handles = Vec::new();
        for i in 0..self.settings.workers.max(1) {
            let me = Arc::clone(self);
            let worker = WorkerId::new(format!("{}-sync-{i}", me.instance));
            handles.push(tokio::spawn(async move { me.worker_loop(worker).await }));
        }
        let me = Arc::clone(self);
        handles.push(tokio::spawn(async move { me.scheduler_loop().await }));
        handles
    }

    async fn worker_loop(self: Arc<Self>, worker: WorkerId) {
        while !self.shutdown.load(Ordering::Relaxed) {
            match self.work_next(&worker).await {
                Ok(true) => {}
                Ok(false) => {
                    let _ =
                        tokio::time::timeout(self.settings.idle_poll, self.queued.notified()).await;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "sync worker: claim failed");
                    tokio::time::sleep(self.settings.idle_poll).await;
                }
            }
        }
    }

    /// Claims and works one run; `Ok(false)` when nothing was claimable.
    pub async fn work_next(&self, worker: &WorkerId) -> Result<bool, SyncError> {
        let Some((lease, payload)) = self
            .cluster
            .claim_next(JobKind::EndpointSync, worker, self.settings.lease)
            .await
            .map_err(|e| SyncError::Unavailable(e.to_string()))?
        else {
            return Ok(false);
        };
        let run_id = payload
            .get("runId")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        self.work(lease, &run_id).await;
        Ok(true)
    }

    async fn work(&self, lease: ClusterJobLease, run_id: &str) {
        let token = lease.fencing_token as i64;
        let run = match self
            .env
            .store
            .claim_run(run_id, lease.job_id.as_str(), token)
            .await
        {
            Ok(r) => r,
            Err(SyncStoreError::LeaseLost) | Err(SyncStoreError::NotFound) => {
                // Superseded (re-enqueued elsewhere), cancelled or deleted.
                let _ = self
                    .cluster
                    .complete(&lease, json!({"superseded": true}))
                    .await;
                return;
            }
            Err(e) => {
                tracing::warn!(run = run_id, error = %e, "sync worker: claiming the run failed");
                let _ = self.cluster.release(lease).await;
                return;
            }
        };
        let Some(job) = self.env.store.job(&run.job_id).await.ok().flatten() else {
            let _ = self.env.store.cancel_run(&run.id, "job deleted").await;
            let _ = self
                .cluster
                .complete(&lease, json!({"jobDeleted": true}))
                .await;
            return;
        };
        let stop = Arc::new(AtomicBool::new(false));
        let handle = RunHandle {
            run_id: run.id.clone(),
            token,
            stop: stop.clone(),
        };
        let beat = {
            let cluster = Arc::clone(&self.cluster);
            let store = Arc::clone(&self.env.store);
            let lease = lease.clone();
            let stop = stop.clone();
            let shutdown = self.shutdown.clone();
            let period = self.settings.heartbeat;
            let run_id = run.id.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(period).await;
                    if shutdown.load(Ordering::Relaxed) {
                        stop.store(true, Ordering::Relaxed);
                    }
                    match cluster.heartbeat(&lease).await {
                        Err(ClusterLeaseError::LeaseLost { .. }) => {
                            stop.store(true, Ordering::Relaxed)
                        }
                        Err(e) => tracing::warn!(error = %e, "sync heartbeat failed"),
                        Ok(_) => {}
                    }
                    if matches!(cluster.cancel_requested(&lease).await, Ok(true)) {
                        stop.store(true, Ordering::Relaxed);
                    }
                    if let Ok(Some(r)) = store.run(&run_id).await
                        && r.state.is_terminal()
                    {
                        stop.store(true, Ordering::Relaxed);
                    }
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                }
            })
        };
        let result = execute(&self.env, &job, &run, &handle).await;
        beat.abort();
        match result {
            Ok(state) => {
                let _ = self.cluster.complete(&lease, json!({"state": state})).await;
                self.finished(&job, &run, state).await;
            }
            Err(EngineError::Stopped(why)) => {
                tracing::info!(run = %run.id, why, "sync run stopped");
                if self.shutdown.load(Ordering::Relaxed) {
                    // Graceful shutdown: another instance resumes it.
                    let _ = self.cluster.release(lease).await;
                } else {
                    let _ = self.cluster.fail(&lease, &why).await;
                    if let Some(a) = &self.audit {
                        a.record("run-stopped", &job.id, &job.spec.name, "system", true, &why)
                            .await;
                    }
                }
            }
            Err(EngineError::Failed(msg)) => {
                let _ = self.cluster.fail(&lease, &msg).await;
            }
        }
    }

    async fn finished(&self, job: &SyncJob, run: &SyncRun, state: SyncRunState) {
        let ok = state == SyncRunState::Succeeded;
        let final_run = self.env.store.run(&run.id).await.ok().flatten();
        let desc = match &final_run {
            Some(r) => format!(
                "run {} {:?}: created {}, updated {}, deleted {}, conflicts {}, failed {}{}",
                r.id,
                r.state,
                r.progress.created,
                r.progress.updated,
                r.progress.deleted,
                r.progress.conflicts,
                r.progress.failed,
                r.error
                    .as_deref()
                    .map(|e| format!(" — {e}"))
                    .unwrap_or_default()
            ),
            None => format!("run {} {state:?}", run.id),
        };
        if let Some(a) = &self.audit {
            a.record(
                "run-finished",
                &job.id,
                &job.spec.name,
                "Device/hfs-sync",
                ok,
                &desc,
            )
            .await;
        }
        if run.reason == RunReason::Duplicate && ok {
            self.finish_duplicate(job).await;
        }
    }

    /// After a successful duplication copy: enable the new endpoint and
    /// the follow-up continuous job (if any).
    async fn finish_duplicate(&self, job: &SyncJob) {
        if let (Some(store), helios_endpoints::sync::SyncPeer::Endpoint { name }) =
            (&self.endpoints, &job.spec.target)
            && let Ok(Some(ep)) = store.get(name).await
            && ep.spec.status == EndpointStatus::Provisioning
        {
            match store
                .set_status(name, ep.revision, EndpointStatus::Enabled, &sync_actor())
                .await
            {
                Ok(_) => {
                    let _ = self.env.registry.reload_if_stale(store.as_ref()).await;
                }
                Err(e) => {
                    tracing::warn!(endpoint = %name, error = %e, "enabling the duplicated endpoint failed")
                }
            }
        }
        if let Some(follow) = crate::service::follow_up_job(job)
            && let Ok(Some(mut f)) = self.env.store.job(&follow).await
            && !f.spec.enabled
        {
            f.spec.enabled = true;
            let _ = self.env.store.update_job(&f.id, f.revision, f.spec).await;
        }
    }

    async fn scheduler_loop(self: Arc<Self>) {
        while !self.shutdown.load(Ordering::Relaxed) {
            if let Err(e) = self.schedule_once().await {
                tracing::warn!(error = %e, "sync scheduler tick failed");
            }
            tokio::time::sleep(self.settings.scheduler_tick).await;
        }
    }

    /// One scheduler pass (public for tests).
    pub async fn schedule_once(&self) -> Result<(), SyncError> {
        self.refresh_wake_index().await;
        let now = Utc::now();
        for job in self.env.store.jobs().await? {
            if !job.spec.enabled {
                continue;
            }
            match &job.spec.trigger {
                SyncTrigger::Manual => {}
                SyncTrigger::Interval { every_secs } => {
                    let due = job.next_run_at.is_none_or(|t| t <= now);
                    if due {
                        match self.start_run(&job, RunReason::Schedule).await {
                            Ok(_) | Err(SyncError::Conflict(_)) => {}
                            Err(e) => {
                                tracing::warn!(job = %job.id, error = %e, "scheduling a sync run failed")
                            }
                        }
                        let next = now + chrono::Duration::seconds(i64::from(*every_secs));
                        self.env.store.set_next_run_at(&job.id, Some(next)).await?;
                    }
                }
                SyncTrigger::Continuous { .. } | SyncTrigger::Subscription { .. } => {
                    let last = self.env.store.runs(&job.id, 1).await?.into_iter().next();
                    let restart = match &last {
                        None => true,
                        Some(r) if !r.state.is_terminal() => false,
                        Some(r) if r.state == SyncRunState::Failed => {
                            r.finished_at.is_none_or(|f| {
                                (now - f).to_std().unwrap_or_default()
                                    >= self.settings.continuous_retry
                            })
                        }
                        Some(_) => true,
                    };
                    if restart {
                        match self.start_run(&job, RunReason::Continuous).await {
                            Ok(_) | Err(SyncError::Conflict(_)) => {}
                            Err(e) => {
                                tracing::warn!(job = %job.id, error = %e, "starting a continuous run failed")
                            }
                        }
                    }
                }
            }
        }
        self.sweep_orphans().await
    }

    async fn sweep_orphans(&self) -> Result<(), SyncError> {
        let before =
            Utc::now() - chrono::Duration::from_std(self.settings.orphan_after).unwrap_or_default();
        for run in self.env.store.stale_queued_runs(before).await? {
            let alive = match &run.cluster_job_id {
                None => false,
                Some(cj) => matches!(
                    self.cluster
                        .get_status(&queue_tenant(), &ClusterJobId::from_string(cj.clone()))
                        .await,
                    Ok(Some(rec)) if !rec.state.is_terminal()
                ),
            };
            if !alive {
                tracing::info!(run = %run.id, "re-enqueueing an orphaned sync run");
                self.enqueue(&run).await?;
            }
        }
        Ok(())
    }
}

/// The actor of server-initiated endpoint changes.
pub fn sync_actor() -> Actor {
    Actor {
        subject: "system:sync".into(),
        display: Some("HFS sync".into()),
        via: "sync".into(),
    }
}

/// Write observer waking continuous jobs whose local source was written.
pub struct SyncWriteWaker(pub std::sync::Weak<SyncRuntime>);

impl helios_persistence::core::WriteObserver for SyncWriteWaker {
    fn on_write(&self, event: &helios_persistence::core::WriteEvent) {
        if let Some(rt) = self.0.upgrade() {
            rt.wake_tenant(event.tenant().as_str());
        }
    }

    fn retired(&self) -> bool {
        self.0.strong_count() == 0
    }
}
