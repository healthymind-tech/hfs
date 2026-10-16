//! Background job queue for every write the admin surface performs.
//!
//! # One writer at a time
//!
//! SQLite has a single write lock. Phase 1C measured what happens when two
//! bulk imports overlap: each holds the lock for a batch, the other's batch
//! waits out `busy_timeout`, and one of them eventually fails with "database is
//! locked". So imports, deletes and `VACUUM` are never run from a request
//! handler: the handler validates, enqueues and returns `202`, and a single
//! worker task drains the queue FIFO. A second import submitted while one is
//! running is reported as `queued` with its position, and starts when the
//! first finishes.
//!
//! # Persistence
//!
//! Each job is `<data>/jobs/<id>.json` (rewritten atomically on every state
//! change and every few seconds while running) plus an append-only
//! `<id>.log`. On startup, jobs that were `queued` are queued again in their
//! original order; a job that was `running` when the process died is marked
//! `interrupted` — the importer is idempotent per (url, version), so the
//! operator re-runs it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Notify;

use super::audit::{Actor, Outcome};
use super::ops::OpProgress;
use super::{AdminCore, AdminError, Provenance, ProvenanceStore, formats, uploads};
use crate::import::LanguageFilter;
use crate::import::progress::{ImportProgress, ImportTransforms, ProgressBackend, with_progress};
use helios_persistence::tenant::TenantContext;

/// Where an import reads its source from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ImportSource {
    /// A completed chunked upload.
    Upload { upload_id: String },
    /// A path relative to `HTS_IMPORT_DIR`.
    ImportDir { path: String },
    /// A FHIR package from the registry (`version` default: latest).
    Registry {
        package: String,
        #[serde(default)]
        version: Option<String>,
    },
}

/// What a job does.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum JobSpec {
    Import {
        source: ImportSource,
        /// Format id; detected from the archive when absent.
        #[serde(default)]
        format: Option<String>,
        /// Override the CodeSystem version (single-system formats only).
        #[serde(default)]
        version: Option<String>,
        /// BCP-47 list restricting translations (SNOMED, LOINC).
        #[serde(default)]
        languages: Option<String>,
        /// Concept property codes to keep (LOINC); `None` keeps all.
        #[serde(default)]
        properties: Option<Vec<String>>,
        #[serde(default)]
        batch_size: Option<usize>,
    },
    DeleteCodeSystem {
        url: String,
        #[serde(default)]
        version: Option<String>,
    },
    DeleteValueSet {
        url: String,
        #[serde(default)]
        version: Option<String>,
    },
    DeleteConceptMap {
        url: String,
    },
    /// SQLite `VACUUM` / Postgres `VACUUM (ANALYZE)`.
    Compact,
}

impl JobSpec {
    /// Short human label (`import snomed-rf2 from upload SnomedCT….zip`).
    pub fn label(&self) -> String {
        fn ver(v: &Option<String>) -> String {
            v.as_deref().map(|v| format!("|{v}")).unwrap_or_default()
        }
        match self {
            JobSpec::Import { source, format, .. } => {
                let src = match source {
                    ImportSource::Upload { upload_id } => format!("upload {upload_id}"),
                    ImportSource::ImportDir { path } => format!("import dir {path}"),
                    ImportSource::Registry { package, version } => format!(
                        "registry {package}#{}",
                        version.as_deref().unwrap_or("latest")
                    ),
                };
                match format {
                    Some(f) => format!("import {f} from {src}"),
                    None => format!("import from {src}"),
                }
            }
            JobSpec::DeleteCodeSystem { url, version } => {
                format!("delete CodeSystem {url}{}", ver(version))
            }
            JobSpec::DeleteValueSet { url, version } => {
                format!("delete ValueSet {url}{}", ver(version))
            }
            JobSpec::DeleteConceptMap { url } => format!("delete ConceptMap {url}"),
            JobSpec::Compact => "compact database".into(),
        }
    }

    /// Audit action code.
    pub fn action(&self) -> &'static str {
        match self {
            JobSpec::Import { .. } => "import",
            JobSpec::DeleteCodeSystem { .. } => "delete-code-system",
            JobSpec::DeleteValueSet { .. } => "delete-value-set",
            JobSpec::DeleteConceptMap { .. } => "delete-concept-map",
            JobSpec::Compact => "compact",
        }
    }

    /// Phases in display order.
    pub fn phases(&self) -> Vec<&'static str> {
        match self {
            JobSpec::Import { source, .. } => {
                let mut p = vec!["prepare"];
                if matches!(source, ImportSource::Registry { .. }) {
                    p.push("download");
                }
                p.extend(["parse", "write", "closure", "fts", "analyze", "warm"]);
                p
            }
            JobSpec::DeleteCodeSystem { .. } => {
                vec!["prepare", "closure", "hierarchy", "concepts", "caches"]
            }
            JobSpec::DeleteValueSet { .. } | JobSpec::DeleteConceptMap { .. } => {
                vec!["prepare", "delete"]
            }
            JobSpec::Compact => vec!["prepare", "vacuum"],
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum JobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    /// The process stopped while the job was running.
    Interrupted,
}

impl JobStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, JobStatus::Queued | JobStatus::Running)
    }
}

/// Progress snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JobProgress {
    pub phase: String,
    pub done: u64,
    pub total: Option<u64>,
    /// `batches`, `rows`, `bytes`.
    pub unit: String,
    pub concepts: u64,
    /// Overall completion estimate, 0–100.
    pub percent: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub spec: JobSpec,
    pub label: String,
    pub status: JobStatus,
    pub requested_by: Actor,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub phases: Vec<String>,
    pub progress: JobProgress,
    pub result: Option<Value>,
    pub error: Option<String>,
    /// 1-based position in the queue (queued jobs only; computed on read).
    #[serde(default, skip_deserializing)]
    pub queue_position: Option<usize>,
}

/// In-memory handles of the running job.
#[derive(Clone)]
pub struct Live {
    pub id: String,
    pub phase: Arc<Mutex<String>>,
    pub import: Arc<ImportProgress>,
    pub op: Arc<OpProgress>,
}

impl Live {
    fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            phase: Arc::new(Mutex::new("prepare".into())),
            import: Arc::default(),
            op: Arc::default(),
        }
    }

    pub fn set_phase(&self, p: &str) {
        if let Ok(mut g) = self.phase.lock() {
            *g = p.to_string();
        }
    }

    fn phase(&self) -> String {
        self.phase.lock().map(|g| g.clone()).unwrap_or_default()
    }

    fn cancelled(&self) -> bool {
        self.import.is_cancelled() || self.op.is_cancelled()
    }

    /// Current progress, mapping the counters of whichever layer is active.
    fn snapshot(&self, spec: &JobSpec) -> JobProgress {
        let mut phase = self.phase();
        if phase == "op" {
            phase = self.op.phase();
        }
        // The importer starts writing without telling us; flip parse → write.
        if phase == "parse" && self.import.writing.load(Ordering::SeqCst) {
            phase = "write".into();
            self.set_phase("write");
        }
        let concepts = self.import.concepts.load(Ordering::Relaxed);
        let (done, total, unit) = if phase == "write" {
            let t = self.import.batches_total.load(Ordering::Relaxed);
            (
                self.import.batches_done.load(Ordering::Relaxed),
                (t > 0).then_some(t),
                "batches",
            )
        } else {
            let t = self.op.total.load(Ordering::Relaxed);
            let unit = if phase == "download" { "bytes" } else { "rows" };
            (
                self.op.done.load(Ordering::Relaxed),
                (t > 0).then_some(t),
                unit,
            )
        };
        let fraction = total.map(|t| (done as f64 / t as f64).min(1.0));
        JobProgress {
            percent: overall_percent(spec, &phase, fraction),
            phase,
            done,
            total,
            unit: unit.into(),
            concepts,
        }
    }
}

/// Weighted overall percentage for `phase` at `fraction` through it.
pub fn overall_percent(spec: &JobSpec, phase: &str, fraction: Option<f64>) -> Option<f64> {
    let weights: &[(&str, f64)] = match spec {
        JobSpec::Import { .. } => &[
            ("prepare", 1.0),
            ("download", 4.0),
            ("parse", 12.0),
            ("write", 68.0),
            ("closure", 7.0),
            ("fts", 5.0),
            ("analyze", 2.0),
            ("warm", 1.0),
        ],
        JobSpec::DeleteCodeSystem { .. } => &[
            ("prepare", 2.0),
            ("closure", 30.0),
            ("hierarchy", 5.0),
            ("concepts", 60.0),
            ("caches", 3.0),
        ],
        _ => &[("prepare", 5.0), ("delete", 95.0), ("vacuum", 95.0)],
    };
    let phases = spec.phases();
    let total: f64 = weights
        .iter()
        .filter(|(p, _)| phases.contains(p))
        .map(|(_, w)| w)
        .sum();
    let mut acc = 0.0;
    for (p, w) in weights.iter().filter(|(p, _)| phases.contains(p)) {
        if *p == phase {
            let within = fraction.unwrap_or(0.0) * w;
            return Some(((acc + within) / total * 100.0).clamp(0.0, 100.0));
        }
        acc += w;
    }
    None
}

struct Inner {
    jobs: HashMap<String, Job>,
    order: Vec<String>,
    queue: VecDeque<String>,
    running: Option<Live>,
}

/// The queue, its persisted records and the running job's live counters.
pub struct JobManager {
    dir: PathBuf,
    inner: Mutex<Inner>,
    notify: Notify,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn valid_job_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl JobManager {
    /// Load persisted jobs from `dir`, re-queueing `queued` ones and marking
    /// `running` ones `interrupted`.
    pub fn load(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        let mut jobs: Vec<Job> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                    .filter_map(|e| std::fs::read(e.path()).ok())
                    .filter_map(|b| serde_json::from_slice::<Job>(&b).ok())
                    .collect()
            })
            .unwrap_or_default();
        jobs.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        let mut queue = VecDeque::new();
        let mut map = HashMap::new();
        let mut order = Vec::new();
        let mgr_dir = dir.clone();
        for mut j in jobs {
            match j.status {
                JobStatus::Running => {
                    j.status = JobStatus::Interrupted;
                    j.finished_at = Some(now());
                    j.error = Some(
                        "the server stopped while this job was running; re-run it \
                         (imports are idempotent per url|version)"
                            .into(),
                    );
                    persist_job(&mgr_dir, &j);
                    append_log_line(&mgr_dir, &j.id, "interrupted by server restart");
                }
                JobStatus::Queued => queue.push_back(j.id.clone()),
                _ => {}
            }
            order.push(j.id.clone());
            map.insert(j.id.clone(), j);
        }
        Self {
            dir,
            inner: Mutex::new(Inner {
                jobs: map,
                order,
                queue,
                running: None,
            }),
            notify: Notify::new(),
        }
    }

    /// Enqueue `spec`; returns the stored job (with its queue position).
    pub fn submit(&self, spec: JobSpec, actor: Actor) -> Job {
        self.submit_labeled(spec, actor, None)
    }

    /// [`Self::submit`] with a human label replacing [`JobSpec::label`]
    /// (e.g. naming the uploaded file instead of its session id).
    pub fn submit_labeled(&self, spec: JobSpec, actor: Actor, label: Option<String>) -> Job {
        let id = format!(
            "{}-{}",
            chrono::Utc::now().format("%Y%m%dT%H%M%S"),
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        let job = Job {
            id: id.clone(),
            label: label.unwrap_or_else(|| spec.label()),
            phases: spec.phases().into_iter().map(String::from).collect(),
            spec,
            status: JobStatus::Queued,
            requested_by: actor,
            created_at: now(),
            started_at: None,
            finished_at: None,
            progress: JobProgress {
                phase: "queued".into(),
                ..Default::default()
            },
            result: None,
            error: None,
            queue_position: None,
        };
        persist_job(&self.dir, &job);
        append_log_line(&self.dir, &id, &format!("queued: {}", job.label));
        {
            let mut g = self.inner.lock().unwrap();
            g.jobs.insert(id.clone(), job);
            g.order.push(id.clone());
            g.queue.push_back(id.clone());
        }
        self.notify.notify_one();
        self.get(&id).expect("just inserted")
    }

    fn decorate(g: &Inner, mut job: Job) -> Job {
        if let Some(live) = &g.running
            && live.id == job.id
        {
            job.progress = live.snapshot(&job.spec);
        }
        if job.status == JobStatus::Queued {
            job.queue_position = g.queue.iter().position(|q| *q == job.id).map(|p| p + 1);
        }
        job
    }

    /// One job with live progress merged in.
    pub fn get(&self, id: &str) -> Option<Job> {
        let g = self.inner.lock().unwrap();
        g.jobs.get(id).cloned().map(|j| Self::decorate(&g, j))
    }

    /// Newest first.
    pub fn list(&self, limit: usize) -> Vec<Job> {
        let g = self.inner.lock().unwrap();
        g.order
            .iter()
            .rev()
            .take(limit)
            .filter_map(|id| g.jobs.get(id).cloned())
            .map(|j| Self::decorate(&g, j))
            .collect()
    }

    /// `true` while any job is queued or running.
    pub fn busy(&self) -> bool {
        let g = self.inner.lock().unwrap();
        g.running.is_some() || !g.queue.is_empty()
    }

    /// Cancel a queued job outright, or ask a running import to stop at the
    /// next batch. Running deletes and compaction cannot be interrupted
    /// safely and are refused.
    pub fn cancel(&self, id: &str, actor: &Actor) -> Result<Job, AdminError> {
        let mut g = self.inner.lock().unwrap();
        let Some(job) = g.jobs.get(id).cloned() else {
            return Err(AdminError::NotFound(format!("job {id}")));
        };
        match job.status {
            JobStatus::Queued => {
                g.queue.retain(|q| q != id);
                let j = g.jobs.get_mut(id).unwrap();
                j.status = JobStatus::Cancelled;
                j.finished_at = Some(now());
                j.progress.phase = "cancelled".into();
                persist_job(&self.dir, j);
                append_log_line(
                    &self.dir,
                    id,
                    &format!("cancelled while queued by {}", actor.label()),
                );
                let out = j.clone();
                Ok(out)
            }
            JobStatus::Running => {
                if !matches!(job.spec, JobSpec::Import { .. }) {
                    return Err(AdminError::Conflict(
                        "only imports can be cancelled while running".into(),
                    ));
                }
                if let Some(live) = &g.running {
                    live.import.request_cancel();
                    live.op.cancel.store(true, Ordering::SeqCst);
                }
                append_log_line(
                    &self.dir,
                    id,
                    &format!(
                        "cancellation requested by {}; stopping at the next batch",
                        actor.label()
                    ),
                );
                Ok(Self::decorate(&g, job))
            }
            _ => Err(AdminError::Conflict(format!(
                "job {id} already finished ({:?})",
                job.status
            ))),
        }
    }

    /// Last `lines` log lines.
    pub fn log_tail(&self, id: &str, lines: usize) -> Result<Vec<String>, AdminError> {
        if !valid_job_id(id) || !self.inner.lock().unwrap().jobs.contains_key(id) {
            return Err(AdminError::NotFound(format!("job {id}")));
        }
        let text = std::fs::read_to_string(self.dir.join(format!("{id}.log"))).unwrap_or_default();
        let all: Vec<&str> = text.lines().collect();
        let start = all.len().saturating_sub(lines);
        Ok(all[start..].iter().map(|s| s.to_string()).collect())
    }

    pub fn log(&self, id: &str, line: &str) {
        append_log_line(&self.dir, id, line);
    }

    fn take_next(&self) -> Option<(Job, Live)> {
        let mut g = self.inner.lock().unwrap();
        if g.running.is_some() {
            return None;
        }
        let id = g.queue.pop_front()?;
        let live = Live::new(&id);
        let job = g.jobs.get_mut(&id)?;
        job.status = JobStatus::Running;
        job.started_at = Some(now());
        job.progress.phase = "prepare".into();
        persist_job(&self.dir, job);
        let job = job.clone();
        g.running = Some(live.clone());
        Some((job, live))
    }

    fn checkpoint(&self, id: &str) {
        let g = self.inner.lock().unwrap();
        if let (Some(live), Some(job)) = (&g.running, g.jobs.get(id))
            && live.id == id
        {
            let mut j = job.clone();
            j.progress = live.snapshot(&j.spec);
            persist_job(&self.dir, &j);
        }
    }

    fn finish(&self, id: &str, status: JobStatus, result: Option<Value>, error: Option<String>) {
        let mut g = self.inner.lock().unwrap();
        let snapshot = g
            .running
            .as_ref()
            .filter(|l| l.id == id)
            .zip(g.jobs.get(id))
            .map(|(l, j)| l.snapshot(&j.spec));
        if let Some(j) = g.jobs.get_mut(id) {
            j.status = status;
            j.finished_at = Some(now());
            if let Some(mut p) = snapshot {
                if status == JobStatus::Succeeded {
                    p.percent = Some(100.0);
                    p.phase = "done".into();
                } else {
                    p.phase = format!("{status:?}").to_lowercase();
                }
                j.progress = p;
            }
            j.result = result;
            j.error = error;
            persist_job(&self.dir, j);
        }
        g.running = None;
    }
}

fn persist_job(dir: &Path, job: &Job) {
    let path = dir.join(format!("{}.json", job.id));
    let tmp = dir.join(format!("{}.json.tmp", job.id));
    if let Ok(bytes) = serde_json::to_vec_pretty(job)
        && std::fs::write(&tmp, bytes).is_ok()
    {
        let _ = std::fs::rename(tmp, path);
    }
}

fn append_log_line(dir: &Path, id: &str, line: &str) {
    if !valid_job_id(id) {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{id}.log")))
    {
        let _ = writeln!(
            f,
            "{} {line}",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ")
        );
    }
}

// ── Worker ────────────────────────────────────────────────────────────────────

/// Spawn the single worker that drains the queue. Must be called from within a
/// Tokio runtime.
pub fn start_worker(core: Arc<AdminCore>) {
    tokio::spawn(async move {
        loop {
            match core.jobs.take_next() {
                Some((job, live)) => run_job(&core, job, live).await,
                None => core.jobs.notify.notified().await,
            }
        }
    });
}

async fn run_job(core: &Arc<AdminCore>, job: Job, live: Live) {
    let id = job.id.clone();
    core.jobs.log(&id, &format!("started: {}", job.label));
    // Checkpoint progress to disk every few seconds so a crash or a page
    // reload shows how far it got, and log write-phase milestones.
    let ticker = {
        let core = core.clone();
        let id = id.clone();
        let live = live.clone();
        let spec = job.spec.clone();
        tokio::spawn(async move {
            let mut last_phase = String::new();
            let mut last_logged_batch = 0u64;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                core.jobs.checkpoint(&id);
                let p = live.snapshot(&spec);
                if p.phase != last_phase {
                    core.jobs.log(&id, &format!("phase {}", p.phase));
                    last_phase = p.phase.clone();
                }
                if p.phase == "write" && p.done >= last_logged_batch + 10 {
                    last_logged_batch = p.done;
                    core.jobs.log(
                        &id,
                        &format!(
                            "batch {}/{} — {} concepts",
                            p.done,
                            p.total.map(|t| t.to_string()).unwrap_or_else(|| "?".into()),
                            p.concepts
                        ),
                    );
                }
            }
        })
    };

    let started = std::time::Instant::now();
    let outcome = execute(core, &job, &live).await;
    ticker.abort();
    let elapsed = started.elapsed().as_secs_f64();

    let (status, result, error) = match outcome {
        Ok(mut v) => {
            v["elapsed_seconds"] = json!(elapsed);
            (JobStatus::Succeeded, Some(v), None)
        }
        Err(_) if live.cancelled() => (
            JobStatus::Cancelled,
            None,
            Some("cancelled by operator".to_string()),
        ),
        Err(e) => {
            let msg = match &e {
                AdminError::InsufficientStorage(detail) => format!("{e}: {detail}"),
                other => other.to_string(),
            };
            (JobStatus::Failed, None, Some(msg))
        }
    };
    core.jobs.log(
        &id,
        &format!(
            "{status:?} after {elapsed:.1}s{}",
            error
                .as_deref()
                .map(|e| format!(": {e}"))
                .unwrap_or_default()
        ),
    );
    let audit_outcome = match status {
        JobStatus::Succeeded => Outcome::Success,
        JobStatus::Cancelled => Outcome::MinorFailure,
        _ => Outcome::SeriousFailure,
    };
    core.audit.record(
        &job.requested_by,
        job.spec.action(),
        &format!("{} (job {id})", job.label),
        audit_outcome,
        error.as_deref(),
    );
    core.jobs.finish(&id, status, result, error);
}

async fn execute(core: &Arc<AdminCore>, job: &Job, live: &Live) -> Result<Value, AdminError> {
    let result = match &job.spec {
        JobSpec::Import {
            source,
            format,
            version,
            languages,
            properties,
            batch_size,
        } => {
            exec_import(
                core,
                job,
                live,
                ImportArgs {
                    source,
                    format: format.as_deref(),
                    version: version.as_deref(),
                    languages: languages.as_deref(),
                    properties: properties.clone(),
                    batch_size: *batch_size,
                },
            )
            .await
        }
        JobSpec::DeleteCodeSystem { url, version } => {
            exec_delete_cs(core, job, live, url, version.as_deref()).await
        }
        JobSpec::DeleteValueSet { url, version } => {
            live.set_phase("delete");
            let n = core.ops.delete_value_set(url, version.as_deref()).await?;
            if n == 0 {
                return Err(AdminError::NotFound(format!("ValueSet {url}")));
            }
            Ok(json!({ "deleted": n }))
        }
        JobSpec::DeleteConceptMap { url } => {
            live.set_phase("delete");
            let n = core.ops.delete_concept_map(url).await?;
            if n == 0 {
                return Err(AdminError::NotFound(format!("ConceptMap {url}")));
            }
            Ok(json!({ "deleted": n }))
        }
        JobSpec::Compact => exec_compact(core, job, live).await,
    };
    // Every write job ends with both cache tiers and the stats cache dropped,
    // whether it succeeded or not — a failed import may have written batches.
    (core.on_change)();
    core.invalidate_stats().await;
    result
}

struct ImportArgs<'a> {
    source: &'a ImportSource,
    format: Option<&'a str>,
    version: Option<&'a str>,
    languages: Option<&'a str>,
    properties: Option<Vec<String>>,
    batch_size: Option<usize>,
}

/// Resolve an import source to a local path, downloading registry packages.
async fn resolve_source(
    core: &Arc<AdminCore>,
    job: &Job,
    live: &Live,
    source: &ImportSource,
) -> Result<(PathBuf, String, Option<String>), AdminError> {
    match source {
        ImportSource::Upload { upload_id } => {
            let (path, meta) = core.uploads.file_path(upload_id).await?;
            Ok((path, format!("upload:{}", meta.filename), meta.sha256))
        }
        ImportSource::ImportDir { path } => {
            let root = core.config.import_dir.as_ref().ok_or_else(|| {
                AdminError::Disabled("no server import directory (HTS_IMPORT_DIR)".into())
            })?;
            let p = uploads::resolve_in(root, path)?;
            Ok((p, format!("import-dir:{path}"), None))
        }
        ImportSource::Registry { package, version } => {
            live.set_phase("download");
            let pv = core.registry.resolve(package, version.as_deref()).await?;
            let dest = core
                .config
                .data_dir
                .join("packages")
                .join(format!("{}#{}.tgz", pv.name, pv.version));
            core.jobs.log(
                &job.id,
                &format!("resolved {}#{} → {}", pv.name, pv.version, pv.tarball),
            );
            let sha = if dest.exists() {
                core.jobs.log(
                    &job.id,
                    "package already in the local cache; not downloading",
                );
                pv.shasum.clone()
            } else {
                let op = live.op.clone();
                Some(
                    core.registry
                        .download(&pv, &dest, move |done, total| {
                            op.done.store(done, Ordering::Relaxed);
                            if let Some(t) = total {
                                op.total.store(t, Ordering::Relaxed);
                            }
                        })
                        .await?,
                )
            };
            Ok((dest, format!("registry:{}#{}", pv.name, pv.version), sha))
        }
    }
}

async fn exec_import(
    core: &Arc<AdminCore>,
    job: &Job,
    live: &Live,
    args: ImportArgs<'_>,
) -> Result<Value, AdminError> {
    live.set_phase("prepare");
    let (path, source_label, sha256) = resolve_source(core, job, live, args.source).await?;
    live.set_phase("prepare");

    let format = match (args.format, args.source) {
        (Some(f), _) => formats::parse_format(f)
            .ok_or_else(|| AdminError::BadRequest(format!("unknown format {f:?}")))?,
        (None, ImportSource::Registry { .. }) => crate::config::ImportFormat::Hl7Npm,
        (None, _) => {
            let p = path.clone();
            let d = tokio::task::spawn_blocking(move || formats::detect(&p))
                .await
                .map_err(|e| AdminError::Internal(e.to_string()))?;
            d.format
                .as_deref()
                .and_then(formats::parse_format)
                .ok_or_else(|| {
                    AdminError::BadRequest(
                        "could not detect the format of this file; choose one explicitly".into(),
                    )
                })?
        }
    };
    // A directory is resolved to the release inside it (`Loinc_2.80.zip`
    // next to `loincupload.properties`, the `rrf/` folder of RxNorm): only
    // the RxNorm importer reads a directory itself.
    let path = if path.is_dir() {
        let p = path.clone();
        let d = tokio::task::spawn_blocking(move || formats::detect(&p))
            .await
            .map_err(|e| AdminError::Internal(e.to_string()))?;
        let same_format = d.format.as_deref().and_then(formats::parse_format) == Some(format);
        let resolved = match d.import_path.as_deref() {
            Some(rel) if same_format => path.join(rel),
            _ => path.clone(),
        };
        if resolved.is_dir() && format != crate::config::ImportFormat::Rxnorm {
            return Err(AdminError::BadRequest(format!(
                "{} is a directory and no single {format} release file was found in it ({}); \
                 import the release file itself",
                source_label,
                d.evidence.as_deref().unwrap_or("nothing recognised")
            )));
        }
        if resolved != path {
            core.jobs.log(
                &job.id,
                &format!("directory resolved to {}", resolved.display()),
            );
        }
        resolved
    } else {
        path
    };
    let fi = formats::info(format);
    if args.version.is_some() && !fi.single_system {
        return Err(AdminError::BadRequest(format!(
            "a version override is only possible for single-system formats, not {format}"
        )));
    }
    let size = if path.is_dir() {
        std::fs::read_dir(&path)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.metadata().ok())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0)
    } else {
        std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
    };
    let growth = fi.estimate_growth(size);
    if let Some(db_dir) = &core.config.db_dir {
        let space = super::disk::check(db_dir, growth, core.config.disk_headroom);
        core.jobs.log(
            &job.id,
            &format!(
                "disk check at {}: need {} bytes (estimate {growth} + headroom), free {}",
                space.path,
                space.required_bytes,
                space
                    .free_bytes
                    .map(|f| f.to_string())
                    .unwrap_or_else(|| "unknown".into())
            ),
        );
        if !space.ok {
            return Err(AdminError::InsufficientStorage(
                serde_json::to_value(&space).unwrap_or_default(),
            ));
        }
    }
    core.jobs.log(
        &job.id,
        &format!("format {format}, source {source_label} ({size} bytes)"),
    );

    let before: HashSet<(String, Option<String>)> =
        core.ops.code_system_keys().await?.into_iter().collect();
    let fp_before = core.ops.footprint().await.ok();
    let started_at = chrono::Utc::now();

    live.set_phase("parse");
    let transforms = ImportTransforms {
        version_override: args.version.map(String::from),
        keep_properties: args.properties.filter(|p| !p.is_empty()),
    };
    let backend = ProgressBackend::new(core.importer.clone(), live.import.clone(), transforms);
    let languages = LanguageFilter::parse(args.languages.unwrap_or(core.config.languages.as_str()));
    let batch = args.batch_size.unwrap_or(core.config.batch_size).max(1);
    let ctx = TenantContext::system();
    let result = with_progress(
        live.import.clone(),
        crate::import::dispatch::dispatch_import(
            format, &backend, &ctx, &path, batch, false, &languages,
        ),
    )
    .await;

    let stats = match result {
        Ok(stats) => stats,
        Err(e) => {
            // Undo what this run created (new url|version rows); a re-import of
            // an existing version cannot be undone and is reported instead.
            live.set_phase("op");
            let after = core.ops.code_system_keys().await.unwrap_or_default();
            let mut removed = Vec::new();
            for (url, version) in after.into_iter().filter(|k| !before.contains(k)) {
                if let Ok(Some(row)) = core.ops.find_code_system(&url, version.as_deref()).await {
                    let progress = Arc::new(OpProgress::default());
                    if core.ops.delete_code_system(&row, progress).await.is_ok() {
                        removed.push(ProvenanceStore::key(&url, version.as_deref()));
                    }
                }
            }
            core.jobs.log(
                &job.id,
                &format!(
                    "import stopped: {e}; rolled back {} new CodeSystem version(s): {}",
                    removed.len(),
                    removed.join(", ")
                ),
            );
            return Err(AdminError::Internal(format!(
                "{e} (rolled back {} new CodeSystem version(s))",
                removed.len()
            )));
        }
    };
    core.jobs.log(
        &job.id,
        &format!(
            "written: {} CodeSystems, {} ValueSets, {} ConceptMaps, {} concepts, {} non-fatal errors",
            stats.code_systems,
            stats.value_sets,
            stats.concept_maps,
            stats.concepts,
            stats.errors.len()
        ),
    );
    for e in stats.errors.iter().take(20) {
        core.jobs.log(&job.id, &format!("  error: {e}"));
    }

    for step in ["closure", "fts", "analyze", "warm"] {
        live.set_phase(step);
        let t = std::time::Instant::now();
        core.ops.finalize_step(step).await?;
        core.jobs.log(
            &job.id,
            &format!("{step} done in {:.1}s", t.elapsed().as_secs_f64()),
        );
    }

    // Record provenance for every CodeSystem version this run wrote.
    let fp_after = core.ops.footprint().await.ok();
    let growth_bytes = fp_before
        .zip(fp_after)
        .map(|(b, a)| a.bytes as i64 - b.bytes as i64);
    let rows = core.ops.code_system_rows().await?;
    let touched: Vec<String> = rows
        .iter()
        .filter(|r| {
            !before.contains(&(r.url.clone(), r.version.clone()))
                || chrono::DateTime::parse_from_rfc3339(&r.updated_at)
                    .map(|t| t >= started_at)
                    .unwrap_or(false)
        })
        .map(|r| ProvenanceStore::key(&r.url, r.version.as_deref()))
        .collect();
    let prov = Provenance {
        job_id: job.id.clone(),
        source: source_label.clone(),
        sha256: sha256.clone(),
        format: format.to_string(),
        imported_at: chrono::Utc::now().to_rfc3339(),
        db_growth_bytes: if touched.len() == 1 {
            growth_bytes
        } else {
            None
        },
    };
    core.provenance
        .set_many(touched.iter().map(|k| (k.clone(), prov.clone())).collect());

    Ok(json!({
        "format": format.to_string(),
        "source": source_label,
        "sha256": sha256,
        "stats": stats,
        "code_system_versions": touched,
        "db_growth_bytes": growth_bytes,
        "estimated_growth_bytes": growth,
    }))
}

async fn exec_delete_cs(
    core: &Arc<AdminCore>,
    job: &Job,
    live: &Live,
    url: &str,
    version: Option<&str>,
) -> Result<Value, AdminError> {
    live.set_phase("prepare");
    let row = core
        .ops
        .find_code_system(url, version)
        .await?
        .ok_or_else(|| {
            AdminError::NotFound(format!("CodeSystem {url}|{}", version.unwrap_or("")))
        })?;
    let deps = core.ops.dependents(url, version).await?;
    core.jobs.log(
        &job.id,
        &format!(
            "dependents: {} ValueSet(s), {} supplement(s), {} ConceptMap(s); other versions: {:?}",
            deps.value_sets.len(),
            deps.supplements.len(),
            deps.concept_maps.len(),
            deps.other_versions
        ),
    );
    let fp_before = core.ops.footprint().await.ok();
    live.set_phase("op");
    let report = core.ops.delete_code_system(&row, live.op.clone()).await?;
    core.jobs.log(&job.id, &format!("deleted rows: {report:?}"));

    // A copy written through the REST API also lives in the resource store;
    // remove it so `GET /CodeSystem/{id}` stops returning it.
    let mut resource_store_deleted = false;
    if let Some(store) = &core.resource_store {
        let fhir_id = row.id.split('|').next().unwrap_or(&row.id);
        let ctx = TenantContext::system();
        if let Ok(Some(stored)) = store.read(&ctx, "CodeSystem", fhir_id).await
            && !stored.is_deleted()
        {
            let c = stored.content();
            let same_url = c.get("url").and_then(|u| u.as_str()) == Some(row.url.as_str());
            let same_version = c.get("version").and_then(|v| v.as_str()) == row.version.as_deref();
            if same_url && same_version && store.delete(&ctx, "CodeSystem", fhir_id).await.is_ok() {
                resource_store_deleted = true;
            }
        }
    }
    core.provenance.remove(url, version);
    let fp_after = core.ops.footprint().await.ok();
    Ok(json!({
        "deleted": { "url": row.url, "version": row.version, "id": row.id },
        "rows": report,
        "resource_store_copy_deleted": resource_store_deleted,
        "dependents": deps,
        "file_bytes_before": fp_before.as_ref().map(|f| f.bytes),
        "file_bytes_after": fp_after.as_ref().map(|f| f.bytes),
        "reclaimable_bytes_after": fp_after.map(|f| f.reclaimable_bytes),
    }))
}

async fn exec_compact(core: &Arc<AdminCore>, job: &Job, live: &Live) -> Result<Value, AdminError> {
    live.set_phase("prepare");
    let before = core.ops.footprint().await?;
    if let Some(db_dir) = &core.config.db_dir {
        // VACUUM writes a full copy (temp file) and, in WAL mode, the new
        // pages pass through the WAL: budget twice the database size.
        let space = super::disk::check(
            db_dir,
            before.bytes.saturating_mul(2),
            core.config.disk_headroom,
        );
        if !space.ok {
            return Err(AdminError::InsufficientStorage(
                serde_json::to_value(&space).unwrap_or_default(),
            ));
        }
    }
    core.jobs.log(
        &job.id,
        &format!(
            "database {} bytes, {} reclaimable; starting VACUUM",
            before.bytes, before.reclaimable_bytes
        ),
    );
    live.set_phase("vacuum");
    core.ops.compact().await?;
    let after = core.ops.footprint().await?;
    Ok(json!({
        "bytes_before": before.bytes,
        "bytes_after": after.bytes,
        "reclaimed_bytes": before.bytes as i64 - after.bytes as i64,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(n: u32) -> JobSpec {
        JobSpec::Import {
            source: ImportSource::ImportDir {
                path: format!("f{n}.json"),
            },
            format: None,
            version: None,
            languages: None,
            properties: None,
            batch_size: None,
        }
    }

    #[test]
    fn queue_is_fifo_and_single_runner() {
        let d = tempfile::tempdir().unwrap();
        let m = JobManager::load(d.path().to_path_buf());
        let a = m.submit(spec(1), Actor::system());
        let b = m.submit(spec(2), Actor::system());
        assert_eq!(a.queue_position, Some(1));
        assert_eq!(b.queue_position, Some(2));
        let (first, _live) = m.take_next().unwrap();
        assert_eq!(first.id, a.id);
        // While one runs, nothing else is handed out.
        assert!(m.take_next().is_none());
        assert_eq!(m.get(&b.id).unwrap().queue_position, Some(1));
        assert_eq!(m.get(&b.id).unwrap().status, JobStatus::Queued);
        m.finish(&a.id, JobStatus::Succeeded, Some(json!({})), None);
        let (second, _live) = m.take_next().unwrap();
        assert_eq!(second.id, b.id);
    }

    #[test]
    fn restart_requeues_queued_and_interrupts_running() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = {
            let m = JobManager::load(d.path().to_path_buf());
            let a = m.submit(spec(1), Actor::system());
            let b = m.submit(spec(2), Actor::system());
            let _ = m.take_next(); // a running
            (a.id, b.id)
        };
        let m = JobManager::load(d.path().to_path_buf());
        assert_eq!(m.get(&a).unwrap().status, JobStatus::Interrupted);
        assert_eq!(m.get(&b).unwrap().status, JobStatus::Queued);
        let (next, _) = m.take_next().unwrap();
        assert_eq!(next.id, b);
        assert!(
            m.log_tail(&a, 10)
                .unwrap()
                .iter()
                .any(|l| l.contains("interrupted"))
        );
    }

    #[test]
    fn cancel_queued_and_running_import() {
        let d = tempfile::tempdir().unwrap();
        let m = JobManager::load(d.path().to_path_buf());
        let a = m.submit(spec(1), Actor::system());
        let b = m.submit(spec(2), Actor::system());
        let c = m.submit(JobSpec::Compact, Actor::system());
        assert_eq!(
            m.cancel(&b.id, &Actor::system()).unwrap().status,
            JobStatus::Cancelled
        );
        let (_, live) = m.take_next().unwrap();
        m.cancel(&a.id, &Actor::system()).unwrap();
        assert!(live.import.is_cancelled());
        m.finish(&a.id, JobStatus::Cancelled, None, None);
        let (running_c, _) = m.take_next().unwrap();
        assert_eq!(running_c.id, c.id);
        assert!(matches!(
            m.cancel(&c.id, &Actor::system()),
            Err(AdminError::Conflict(_))
        ));
    }

    #[test]
    fn write_phase_progress_maps_batches_to_percent() {
        let live = Live::new("x");
        let s = spec(1);
        live.set_phase("parse");
        live.import.writing.store(true, Ordering::SeqCst);
        live.import.batches_done.store(5, Ordering::Relaxed);
        live.import.batches_total.store(10, Ordering::Relaxed);
        let p = live.snapshot(&s);
        assert_eq!(p.phase, "write");
        assert_eq!((p.done, p.total), (5, Some(10)));
        let pct = p.percent.unwrap();
        assert!(pct > 40.0 && pct < 60.0, "{pct}");
    }

    #[test]
    fn spec_round_trips_as_tagged_json() {
        let v = json!({"kind": "import", "source": {"type": "registry", "package": "hl7.terminology.r4"}});
        let s: JobSpec = serde_json::from_value(v).unwrap();
        assert!(matches!(
            s,
            JobSpec::Import {
                source: ImportSource::Registry { .. },
                ..
            }
        ));
        let v = json!({"kind": "delete-code-system", "url": "http://x", "version": "1"});
        let s: JobSpec = serde_json::from_value(v).unwrap();
        assert_eq!(s.label(), "delete CodeSystem http://x|1");
    }
}
