//! Background package jobs: plan (dry run), install and uninstall.
//!
//! Jobs live in memory (the last [`MAX_JOBS`] are kept); what a job did is
//! durable in the package registry, so a restart loses only the progress
//! view. Install and uninstall jobs run one at a time — they share storage
//! and the terminology server — while plan jobs run freely. Cancellation is
//! cooperative: the running job checks the flag between resources and
//! stops, leaving whatever it had already written recorded against the
//! package (status `cancelled`) so an uninstall can clean it up.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

/// Jobs remembered.
pub const MAX_JOBS: usize = 50;
/// Log lines kept per job.
const MAX_LOG: usize = 400;
/// Error details kept per job.
const MAX_ERRORS: usize = 1000;

/// Job kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Clone, Default, Serialize)]
pub struct TypeCount {
    /// Written (HFS) or imported (HTS).
    pub written: u64,
    /// Failed.
    pub failed: u64,
    /// Not written (examples off, unchanged owner, …).
    pub skipped: u64,
}

/// One failure.
#[derive(Debug, Clone, Serialize)]
pub struct JobError {
    /// `name#version`.
    pub package: String,
    /// `Type/id` (or a description of the batch).
    pub resource: String,
    /// Why.
    pub message: String,
}

/// The observable state of a job.
#[derive(Debug, Clone, Serialize)]
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

/// The in-memory job table.
#[derive(Default)]
pub struct JobTable {
    jobs: Mutex<VecDeque<Arc<Job>>>,
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
                log: VecDeque::new(),
                requested_by,
                created_at: Utc::now(),
                started_at: None,
                finished_at: None,
                result: None,
                request,
            }),
            cancel: AtomicBool::new(false),
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
}
