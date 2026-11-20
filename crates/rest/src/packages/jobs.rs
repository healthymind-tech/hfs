//! Background package jobs: plan (dry run), install and uninstall.
//!
//! Jobs live in memory (the last [`MAX_JOBS`] are kept); what a job did is
//! durable in the package registry, so a restart loses only the progress
//! view. Install and uninstall jobs run one at a time — they share storage
//! and the terminology server — while plan jobs run freely. Cancellation is
//! cooperative: the running job checks the flag between resources and
//! stops, leaving whatever it had already written recorded against the
//! package (status `cancelled`) so an uninstall can clean it up.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Utc};
use helios_persistence::core::package_jobs::{PackageJobCoordinator, PackageJobRecord};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Jobs remembered.
pub const MAX_JOBS: usize = 50;
/// Log lines kept per job.
const MAX_LOG: usize = 400;
/// Error details kept per job.
const MAX_ERRORS: usize = 1000;

/// Job kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobKind {
    /// Dry run: resolve and count.
    Plan,
    /// Install a plan.
    Install,
    /// Remove a package.
    Uninstall,
}

/// Job lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobStatus {
    /// Waiting for the install lock.
    Queued,
    /// Running.
    Running,
    /// Finished without errors.
    Succeeded,
    /// Finished, some resources failed.
    CompletedWithErrors,
    /// Stopped by an error.
    Failed,
    /// Stopped on request.
    Cancelled,
}

impl JobStatus {
    /// Whether the job is over.
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

/// Per-type counters for one package.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TypeCount {
    /// Written (HFS) or imported (HTS).
    pub written: u64,
    /// Failed.
    pub failed: u64,
    /// Not written (examples off, unchanged owner, …).
    pub skipped: u64,
}

/// One failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobError {
    /// `name#version`.
    pub package: String,
    /// `Type/id` (or a description of the batch).
    pub resource: String,
    /// Why.
    pub message: String,
}

/// The observable state of a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobView {
    /// Job id.
    pub id: String,
    /// Kind.
    pub kind: JobKind,
    /// The requested package (`name#version`).
    pub package: String,
    /// Tenant.
    pub tenant: String,
    /// Status.
    pub status: JobStatus,
    /// Human-readable phase ("downloading", "writing", …).
    pub phase: String,
    /// The package being processed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// Packages in the job.
    pub packages_total: u64,
    /// Packages finished.
    pub packages_done: u64,
    /// Resources in the job (known once the packages are read).
    pub resources_total: u64,
    /// Resources processed.
    pub resources_done: u64,
    /// `name#version` → resource type → counts.
    pub counts: BTreeMap<String, BTreeMap<String, TypeCount>>,
    /// Failures (capped).
    pub errors: Vec<JobError>,
    /// Errors beyond the cap.
    pub errors_dropped: u64,
    /// Non-fatal problems: a dependency that could not be installed, a
    /// terminology resource the terminology server kept its own copy of, …
    /// (capped like `errors`).
    pub warnings: Vec<JobError>,
    /// Recent log lines.
    pub log: VecDeque<String>,
    /// Who asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<String>,
    /// Created.
    pub created_at: DateTime<Utc>,
    /// Started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// Finished.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    /// Kind-specific result (the plan, or the install/uninstall report).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The request that created the job.
    pub request: Value,
}

/// A job handle shared between the runner and readers.
pub struct Job {
    state: Mutex<JobView>,
    cancel: AtomicBool,
    /// Bumped on every state change; the cluster mirror writes a job only
    /// when it moved since the last write.
    version: AtomicU64,
}

impl Job {
    /// A snapshot of the state.
    pub fn view(&self) -> JobView {
        self.state.lock().expect("job state").clone()
    }

    /// Requests cancellation. Returns `false` when the job is already over.
    pub fn cancel(&self) -> bool {
        let state = self.state.lock().expect("job state");
        if state.status.is_terminal() {
            return false;
        }
        self.cancel.store(true, Ordering::SeqCst);
        true
    }

    /// Whether cancellation was requested.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Mutates the state.
    pub fn update(&self, f: impl FnOnce(&mut JobView)) {
        f(&mut self.state.lock().expect("job state"));
        self.version.fetch_add(1, Ordering::SeqCst);
    }

    /// Appends a log line (timestamped).
    pub fn log(&self, line: impl Into<String>) {
        let line = format!("{} {}", Utc::now().format("%H:%M:%S"), line.into());
        self.update(|s| {
            s.log.push_back(line);
            while s.log.len() > MAX_LOG {
                s.log.pop_front();
            }
        });
    }

    /// Sets the phase (and logs it).
    pub fn phase(&self, phase: impl Into<String>) {
        let phase = phase.into();
        self.log(phase.clone());
        self.update(|s| s.phase = phase);
    }

    /// Records a failure.
    pub fn error(&self, package: &str, resource: impl Into<String>, message: impl Into<String>) {
        let (resource, message) = (resource.into(), message.into());
        self.update(|s| {
            if s.errors.len() < MAX_ERRORS {
                s.errors.push(JobError {
                    package: package.to_string(),
                    resource,
                    message,
                });
            } else {
                s.errors_dropped += 1;
            }
        });
    }

    /// Records a warning: kept in `warnings`, logged on the job and in the
    /// server log.
    pub fn warn(&self, package: &str, resource: impl Into<String>, message: impl Into<String>) {
        let (resource, message) = (resource.into(), message.into());
        tracing::warn!(package, resource = %resource, "package job warning: {message}");
        self.log(format!("warning: {package} {resource}: {message}"));
        self.update(|s| {
            if s.warnings.len() < MAX_ERRORS {
                s.warnings.push(JobError {
                    package: package.to_string(),
                    resource,
                    message,
                });
            }
        });
    }

    /// Bumps one counter.
    pub fn count(&self, package: &str, resource_type: &str, f: impl FnOnce(&mut TypeCount)) {
        self.update(|s| {
            f(s.counts
                .entry(package.to_string())
                .or_default()
                .entry(resource_type.to_string())
                .or_default())
        });
    }

    /// Marks the job running.
    pub fn start(&self) {
        self.update(|s| {
            s.status = JobStatus::Running;
            s.started_at = Some(Utc::now());
        });
    }

    /// Ends the job.
    pub fn finish(&self, status: JobStatus, result: Option<Value>) {
        self.update(|s| {
            s.status = status;
            s.finished_at = Some(Utc::now());
            s.phase = match status {
                JobStatus::Succeeded => "done".into(),
                JobStatus::CompletedWithErrors => "done with errors".into(),
                JobStatus::Failed => "failed".into(),
                JobStatus::Cancelled => "cancelled".into(),
                _ => s.phase.clone(),
            };
            if result.is_some() {
                s.result = result;
            }
        });
    }
}

/// The in-memory job table, optionally mirrored to the shared database so
/// any instance of a cluster can report and cancel any job.
#[derive(Default)]
pub struct JobTable {
    jobs: Mutex<VecDeque<Arc<Job>>>,
    /// Cluster mirror and this instance's id.
    mirror: OnceLock<(Arc<dyn PackageJobCoordinator>, String)>,
}

/// How often the owner writes its jobs' progress to the shared mirror and
/// reads their cancel flags.
const MIRROR_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

fn record_of(view: &JobView, owner: &str) -> PackageJobRecord {
    let kind = serde_json::to_value(view.kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    let status = serde_json::to_value(view.status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    PackageJobRecord {
        id: view.id.clone(),
        tenant: view.tenant.clone(),
        kind,
        status,
        terminal: view.status.is_terminal(),
        view: serde_json::to_value(view).unwrap_or(Value::Null),
        cancel_requested: false,
        owner: owner.to_string(),
        updated_at: Utc::now(),
    }
}

impl JobTable {
    /// Registers a new queued job.
    pub fn create(
        &self,
        kind: JobKind,
        package: &str,
        tenant: &str,
        request: Value,
        requested_by: Option<String>,
    ) -> Arc<Job> {
        let job = Arc::new(Job {
            state: Mutex::new(JobView {
                id: uuid::Uuid::new_v4().to_string(),
                kind,
                package: package.to_string(),
                tenant: tenant.to_string(),
                status: JobStatus::Queued,
                phase: "queued".into(),
                current: None,
                packages_total: 0,
                packages_done: 0,
                resources_total: 0,
                resources_done: 0,
                counts: BTreeMap::new(),
                errors: Vec::new(),
                errors_dropped: 0,
                warnings: Vec::new(),
                log: VecDeque::new(),
                requested_by,
                created_at: Utc::now(),
                started_at: None,
                finished_at: None,
                result: None,
                request,
            }),
            cancel: AtomicBool::new(false),
            version: AtomicU64::new(1),
        });
        let mut jobs = self.jobs.lock().expect("job table");
        jobs.push_front(Arc::clone(&job));
        // Drop the oldest finished jobs beyond the cap; never a running one.
        while jobs.len() > MAX_JOBS {
            match jobs.iter().rposition(|j| j.view().status.is_terminal()) {
                Some(i) => {
                    jobs.remove(i);
                }
                None => break,
            }
        }
        job
    }

    /// One job.
    pub fn get(&self, id: &str) -> Option<Arc<Job>> {
        self.jobs
            .lock()
            .expect("job table")
            .iter()
            .find(|j| j.view().id == id)
            .cloned()
    }

    /// Every job, newest first.
    pub fn list(&self) -> Vec<JobView> {
        self.jobs
            .lock()
            .expect("job table")
            .iter()
            .map(|j| j.view())
            .collect()
    }

    /// Mirrors every job to the shared database from now on, and starts the
    /// task that writes progress and picks up cancels requested through other
    /// instances. Only the first call takes effect.
    pub fn attach_mirror(
        self: &Arc<Self>,
        coordinator: Arc<dyn PackageJobCoordinator>,
        owner: String,
    ) {
        if self.mirror.set((coordinator, owner)).is_err() {
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let table = Arc::clone(self);
        tokio::spawn(async move {
            let mut written: HashMap<String, u64> = HashMap::new();
            loop {
                tokio::time::sleep(MIRROR_INTERVAL).await;
                table.flush_mirror(&mut written).await;
            }
        });
    }

    /// One mirror pass: write jobs that changed, adopt remote cancels.
    async fn flush_mirror(&self, written: &mut HashMap<String, u64>) {
        let Some((coordinator, owner)) = self.mirror.get() else {
            return;
        };
        let jobs: Vec<Arc<Job>> = self
            .jobs
            .lock()
            .expect("job table")
            .iter()
            .cloned()
            .collect();
        for job in jobs {
            let version = job.version.load(Ordering::SeqCst);
            let view = job.view();
            if written.get(&view.id) != Some(&version) {
                match coordinator.save_job(&record_of(&view, owner)).await {
                    Ok(()) => {
                        written.insert(view.id.clone(), version);
                    }
                    Err(e) => {
                        tracing::warn!(job = %view.id, error = %e, "mirroring a package job failed")
                    }
                }
            }
            if !view.status.is_terminal()
                && !job.cancelled()
                && coordinator
                    .cancel_requested(&view.id)
                    .await
                    .unwrap_or(false)
            {
                job.cancel();
                job.log("cancel requested through another instance");
            }
        }
        let live: std::collections::HashSet<String> = self
            .jobs
            .lock()
            .expect("job table")
            .iter()
            .map(|j| j.view().id)
            .collect();
        written.retain(|id, _| live.contains(id));
    }

    /// One job's state, from this instance or — in a cluster — the shared
    /// mirror (a job another instance runs).
    pub async fn view_shared(&self, id: &str) -> Option<JobView> {
        if let Some(job) = self.get(id) {
            return Some(job.view());
        }
        let (coordinator, _) = self.mirror.get()?;
        match coordinator.load_job(id).await {
            Ok(Some(record)) => serde_json::from_value(record.view).ok(),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(job = %id, error = %e, "reading a mirrored package job failed");
                None
            }
        }
    }

    /// Every job this instance or (in a cluster) any other knows, newest
    /// first, at most [`MAX_JOBS`].
    pub async fn list_shared(&self) -> Vec<JobView> {
        let mut views = self.list();
        if let Some((coordinator, _)) = self.mirror.get() {
            match coordinator.list_jobs(MAX_JOBS).await {
                Ok(records) => {
                    let known: std::collections::HashSet<String> =
                        views.iter().map(|v| v.id.clone()).collect();
                    views.extend(
                        records
                            .into_iter()
                            .filter(|r| !known.contains(&r.id))
                            .filter_map(|r| serde_json::from_value::<JobView>(r.view).ok()),
                    );
                }
                Err(e) => tracing::warn!(error = %e, "listing mirrored package jobs failed"),
            }
        }
        views.sort_by_key(|view| std::cmp::Reverse(view.created_at));
        views.truncate(MAX_JOBS);
        views
    }

    /// Requests cancellation of a job running here or on another instance.
    /// `None` when the job is unknown; otherwise whether the request was
    /// accepted (the job was still running) and its current state.
    pub async fn cancel_shared(&self, id: &str) -> Option<(bool, JobView)> {
        if let Some(job) = self.get(id) {
            let accepted = job.cancel();
            return Some((accepted, job.view()));
        }
        let (coordinator, _) = self.mirror.get()?;
        let accepted = coordinator.request_cancel(id).await.unwrap_or(false);
        let view = self.view_shared(id).await?;
        Some((accepted, view))
    }

    /// Whether an install or uninstall is queued or running.
    pub fn busy(&self) -> bool {
        self.jobs.lock().expect("job table").iter().any(|j| {
            let v = j.view();
            v.kind != JobKind::Plan && !v.status.is_terminal()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn jobs_track_progress_cancel_and_eviction() {
        let table = JobTable::default();
        let job = table.create(JobKind::Install, "a#1", "default", json!({}), None);
        assert!(table.busy());
        job.start();
        job.count("a#1", "StructureDefinition", |c| c.written += 2);
        job.error("a#1", "StructureDefinition/x", "boom");
        for i in 0..(MAX_LOG + 10) {
            job.log(format!("line {i}"));
        }
        assert!(job.cancel());
        assert!(job.cancelled());
        job.finish(JobStatus::Cancelled, None);
        assert!(!job.cancel(), "a finished job cannot be cancelled again");
        let view = table.get(&job.view().id).unwrap().view();
        assert_eq!(view.counts["a#1"]["StructureDefinition"].written, 2);
        assert_eq!(view.errors.len(), 1);
        assert_eq!(view.log.len(), MAX_LOG);
        assert_eq!(view.phase, "cancelled");
        assert!(!table.busy());

        for _ in 0..(MAX_JOBS + 5) {
            let j = table.create(JobKind::Plan, "p#1", "default", json!({}), None);
            j.finish(JobStatus::Succeeded, None);
        }
        assert_eq!(table.list().len(), MAX_JOBS);
    }

    /// In-memory stand-in for the shared database (two tables sharing one of
    /// these behave like two instances sharing PostgreSQL).
    #[derive(Default)]
    struct FakeCoordinator {
        jobs: Mutex<HashMap<String, PackageJobRecord>>,
    }

    struct NoGuard;
    impl helios_persistence::core::package_jobs::InstallLockGuard for NoGuard {}

    #[async_trait::async_trait]
    impl PackageJobCoordinator for FakeCoordinator {
        async fn lock_installs(
            &self,
            _key: i32,
        ) -> helios_persistence::error::StorageResult<
            Box<dyn helios_persistence::core::package_jobs::InstallLockGuard>,
        > {
            Ok(Box::new(NoGuard))
        }
        async fn save_job(
            &self,
            job: &PackageJobRecord,
        ) -> helios_persistence::error::StorageResult<()> {
            let mut jobs = self.jobs.lock().unwrap();
            let cancel = jobs.get(&job.id).is_some_and(|j| j.cancel_requested);
            let mut job = job.clone();
            job.cancel_requested = cancel;
            jobs.insert(job.id.clone(), job);
            Ok(())
        }
        async fn load_job(
            &self,
            id: &str,
        ) -> helios_persistence::error::StorageResult<Option<PackageJobRecord>> {
            Ok(self.jobs.lock().unwrap().get(id).cloned())
        }
        async fn list_jobs(
            &self,
            _limit: usize,
        ) -> helios_persistence::error::StorageResult<Vec<PackageJobRecord>> {
            Ok(self.jobs.lock().unwrap().values().cloned().collect())
        }
        async fn request_cancel(&self, id: &str) -> helios_persistence::error::StorageResult<bool> {
            Ok(match self.jobs.lock().unwrap().get_mut(id) {
                Some(j) if !j.terminal => {
                    j.cancel_requested = true;
                    true
                }
                _ => false,
            })
        }
        async fn cancel_requested(
            &self,
            id: &str,
        ) -> helios_persistence::error::StorageResult<bool> {
            Ok(self
                .jobs
                .lock()
                .unwrap()
                .get(id)
                .is_some_and(|j| j.cancel_requested))
        }
    }

    /// A job run by instance A is visible from instance B, and a cancel
    /// requested through B reaches the job on A.
    #[tokio::test]
    async fn a_job_on_one_instance_is_seen_and_cancelled_through_another() {
        let shared = Arc::new(FakeCoordinator::default());
        let a = Arc::new(JobTable::default());
        let b = Arc::new(JobTable::default());
        a.mirror.set((shared.clone(), "a".into())).ok().unwrap();
        b.mirror.set((shared.clone(), "b".into())).ok().unwrap();

        let job = a.create(JobKind::Install, "pkg#1", "default", json!({}), None);
        job.start();
        job.count("pkg#1", "StructureDefinition", |c| c.written += 3);
        let id = job.view().id;
        assert!(b.view_shared(&id).await.is_none(), "not mirrored yet");

        let mut written = HashMap::new();
        a.flush_mirror(&mut written).await;
        let seen = b.view_shared(&id).await.expect("B sees A's job");
        assert_eq!(seen.status, JobStatus::Running);
        assert_eq!(seen.counts["pkg#1"]["StructureDefinition"].written, 3);
        assert_eq!(b.list_shared().await.len(), 1);

        let (accepted, _) = b.cancel_shared(&id).await.expect("known job");
        assert!(accepted);
        assert!(!job.cancelled(), "A learns of it on its next mirror pass");
        a.flush_mirror(&mut written).await;
        assert!(job.cancelled(), "the remote cancel reached the running job");

        job.finish(JobStatus::Cancelled, None);
        a.flush_mirror(&mut written).await;
        assert_eq!(
            b.view_shared(&id).await.unwrap().status,
            JobStatus::Cancelled
        );
        let (accepted, _) = b.cancel_shared(&id).await.unwrap();
        assert!(!accepted, "a finished job cannot be cancelled");
    }
}
