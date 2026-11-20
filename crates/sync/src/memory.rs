//! In-memory [`SyncStore`] (single instance: tests, SQLite deployments) and
//! the store conformance suite every implementation must pass (the
//! PostgreSQL store runs it in `tests/postgres_sync.rs`).

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use helios_endpoints::sync::{
    LogLevel, RunReason, SyncConflict, SyncCursor, SyncItemState, SyncJob, SyncJobSpec,
    SyncLogLine, SyncProgress, SyncRun, SyncRunState, SyncStore, SyncStoreError,
};

#[derive(Default)]
struct Inner {
    jobs: HashMap<String, SyncJob>,
    runs: HashMap<String, (SyncRun, i64)>,
    logs: HashMap<String, Vec<SyncLogLine>>,
    conflicts: HashMap<String, Vec<SyncConflict>>,
    items: HashMap<(String, String, String), SyncItemState>,
    seq: u64,
}

/// See the module docs.
#[derive(Default)]
pub struct MemorySyncStore {
    inner: Mutex<Inner>,
}

impl MemorySyncStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

const MAX_LOG: usize = 10_000;

#[async_trait]
impl SyncStore for MemorySyncStore {
    async fn create_job_with_id(
        &self,
        id: &str,
        spec: SyncJobSpec,
        actor: &str,
    ) -> Result<SyncJob, SyncStoreError> {
        let mut g = self.inner.lock().unwrap();
        if g.jobs.values().any(|j| j.spec.name == spec.name) {
            return Err(SyncStoreError::DuplicateName(spec.name));
        }
        let now = Utc::now();
        let job = SyncJob {
            id: id.to_string(),
            revision: 1,
            spec,
            next_run_at: None,
            created_at: now,
            updated_at: now,
            created_by: Some(actor.to_string()),
        };
        g.jobs.insert(id.to_string(), job.clone());
        Ok(job)
    }

    async fn update_job(
        &self,
        id: &str,
        expected_revision: u64,
        spec: SyncJobSpec,
    ) -> Result<SyncJob, SyncStoreError> {
        let mut g = self.inner.lock().unwrap();
        if !g.jobs.contains_key(id) {
            return Err(SyncStoreError::NotFound);
        }
        if g.jobs
            .values()
            .any(|j| j.id != id && j.spec.name == spec.name)
        {
            return Err(SyncStoreError::DuplicateName(spec.name));
        }
        let job = g.jobs.get_mut(id).ok_or(SyncStoreError::NotFound)?;
        if job.revision != expected_revision {
            return Err(SyncStoreError::Conflict {
                current: job.revision,
            });
        }
        job.spec = spec;
        job.revision += 1;
        job.updated_at = Utc::now();
        Ok(job.clone())
    }

    async fn job(&self, id: &str) -> Result<Option<SyncJob>, SyncStoreError> {
        Ok(self.inner.lock().unwrap().jobs.get(id).cloned())
    }

    async fn jobs(&self) -> Result<Vec<SyncJob>, SyncStoreError> {
        let mut v: Vec<SyncJob> = self.inner.lock().unwrap().jobs.values().cloned().collect();
        v.sort_by(|a, b| a.spec.name.cmp(&b.spec.name));
        Ok(v)
    }

    async fn delete_job(&self, id: &str) -> Result<(), SyncStoreError> {
        let mut g = self.inner.lock().unwrap();
        g.jobs.remove(id).ok_or(SyncStoreError::NotFound)?;
        let runs: Vec<String> = g
            .runs
            .values()
            .filter(|(r, _)| r.job_id == id)
            .map(|(r, _)| r.id.clone())
            .collect();
        for r in runs {
            g.runs.remove(&r);
            g.logs.remove(&r);
            g.conflicts.remove(&r);
        }
        g.items.retain(|k, _| k.0 != id);
        Ok(())
    }

    async fn create_run(
        &self,
        job_id: &str,
        reason: RunReason,
        resume_from: Option<SyncCursor>,
    ) -> Result<SyncRun, SyncStoreError> {
        let mut g = self.inner.lock().unwrap();
        if !g.jobs.contains_key(job_id) {
            return Err(SyncStoreError::NotFound);
        }
        if g.runs
            .values()
            .any(|(r, _)| r.job_id == job_id && !r.state.is_terminal())
        {
            return Err(SyncStoreError::ActiveRun(job_id.to_string()));
        }
        let run = SyncRun {
            id: uuid::Uuid::new_v4().to_string(),
            job_id: job_id.to_string(),
            state: SyncRunState::Queued,
            reason,
            cursor: resume_from.unwrap_or_default(),
            progress: SyncProgress::default(),
            error: None,
            cluster_job_id: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
        };
        g.runs.insert(run.id.clone(), (run.clone(), 0));
        Ok(run)
    }

    async fn run(&self, id: &str) -> Result<Option<SyncRun>, SyncStoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .runs
            .get(id)
            .map(|(r, _)| r.clone()))
    }

    async fn runs(&self, job_id: &str, limit: usize) -> Result<Vec<SyncRun>, SyncStoreError> {
        let g = self.inner.lock().unwrap();
        let mut v: Vec<SyncRun> = g
            .runs
            .values()
            .filter(|(r, _)| r.job_id == job_id)
            .map(|(r, _)| r.clone())
            .collect();
        v.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        v.truncate(limit);
        Ok(v)
    }

    async fn checkpoint(
        &self,
        run_id: &str,
        fencing_token: i64,
        state: SyncRunState,
        cursor: &SyncCursor,
        progress: &SyncProgress,
        error: Option<&str>,
    ) -> Result<(), SyncStoreError> {
        let mut g = self.inner.lock().unwrap();
        let (run, token) = g.runs.get_mut(run_id).ok_or(SyncStoreError::LeaseLost)?;
        if *token != fencing_token || run.state.is_terminal() {
            return Err(SyncStoreError::LeaseLost);
        }
        run.state = state;
        run.cursor = cursor.clone();
        run.progress = progress.clone();
        run.error = error.map(str::to_string);
        if state.is_terminal() {
            run.finished_at = Some(Utc::now());
        }
        Ok(())
    }

    async fn log(
        &self,
        run_id: &str,
        level: LogLevel,
        message: &str,
    ) -> Result<(), SyncStoreError> {
        let mut g = self.inner.lock().unwrap();
        if !g.runs.contains_key(run_id) {
            return Err(SyncStoreError::NotFound);
        }
        g.seq += 1;
        let seq = g.seq;
        let lines = g.logs.entry(run_id.to_string()).or_default();
        lines.push(SyncLogLine {
            seq,
            at: Utc::now(),
            level,
            message: message.chars().take(2000).collect(),
        });
        if lines.len() > MAX_LOG {
            let drop = lines.len() - MAX_LOG;
            lines.drain(..drop);
        }
        Ok(())
    }

    async fn log_lines(
        &self,
        run_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SyncLogLine>, SyncStoreError> {
        let g = self.inner.lock().unwrap();
        Ok(g.logs
            .get(run_id)
            .map(|l| {
                l.iter()
                    .filter(|x| x.seq > after_seq)
                    .take(limit)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn conflict(&self, c: &SyncConflict) -> Result<(), SyncStoreError> {
        let mut g = self.inner.lock().unwrap();
        if !g.runs.contains_key(&c.run_id) {
            return Err(SyncStoreError::NotFound);
        }
        let list = g.conflicts.entry(c.run_id.clone()).or_default();
        list.retain(|x| !(x.resource_type == c.resource_type && x.resource_id == c.resource_id));
        list.push(c.clone());
        Ok(())
    }

    async fn conflicts(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<SyncConflict>, SyncStoreError> {
        let g = self.inner.lock().unwrap();
        Ok(g.conflicts
            .get(run_id)
            .map(|l| l.iter().take(limit).cloned().collect())
            .unwrap_or_default())
    }

    async fn delete_conflict(
        &self,
        run_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<bool, SyncStoreError> {
        let mut g = self.inner.lock().unwrap();
        let Some(list) = g.conflicts.get_mut(run_id) else {
            return Ok(false);
        };
        let before = list.len();
        list.retain(|x| !(x.resource_type == resource_type && x.resource_id == resource_id));
        Ok(list.len() < before)
    }

    async fn attach_cluster_job(
        &self,
        run_id: &str,
        cluster_job_id: &str,
    ) -> Result<(), SyncStoreError> {
        let mut g = self.inner.lock().unwrap();
        match g.runs.get_mut(run_id) {
            Some((r, _)) if r.state == SyncRunState::Queued => {
                r.cluster_job_id = Some(cluster_job_id.to_string());
                Ok(())
            }
            _ => Err(SyncStoreError::NotFound),
        }
    }

    async fn claim_run(
        &self,
        run_id: &str,
        cluster_job_id: &str,
        fencing_token: i64,
    ) -> Result<SyncRun, SyncStoreError> {
        let mut g = self.inner.lock().unwrap();
        let (r, token) = g.runs.get_mut(run_id).ok_or(SyncStoreError::LeaseLost)?;
        if r.state.is_terminal()
            || r.cluster_job_id.as_deref() != Some(cluster_job_id)
            || *token >= fencing_token
        {
            return Err(SyncStoreError::LeaseLost);
        }
        *token = fencing_token;
        r.state = SyncRunState::Running;
        r.started_at.get_or_insert_with(Utc::now);
        Ok(r.clone())
    }

    async fn cancel_run(&self, run_id: &str, reason: &str) -> Result<bool, SyncStoreError> {
        let mut g = self.inner.lock().unwrap();
        match g.runs.get_mut(run_id) {
            Some((r, _)) if !r.state.is_terminal() => {
                r.state = SyncRunState::Cancelled;
                r.error = Some(reason.to_string());
                r.finished_at = Some(Utc::now());
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn stale_queued_runs(
        &self,
        before: DateTime<Utc>,
    ) -> Result<Vec<SyncRun>, SyncStoreError> {
        let g = self.inner.lock().unwrap();
        Ok(g.runs
            .values()
            .filter(|(r, _)| r.state == SyncRunState::Queued && r.created_at < before)
            .map(|(r, _)| r.clone())
            .collect())
    }

    async fn set_next_run_at(
        &self,
        job_id: &str,
        at: Option<DateTime<Utc>>,
    ) -> Result<(), SyncStoreError> {
        let mut g = self.inner.lock().unwrap();
        g.jobs
            .get_mut(job_id)
            .ok_or(SyncStoreError::NotFound)?
            .next_run_at = at;
        Ok(())
    }

    async fn item_states(
        &self,
        job_id: &str,
        keys: &[(String, String)],
    ) -> Result<Vec<SyncItemState>, SyncStoreError> {
        let g = self.inner.lock().unwrap();
        Ok(keys
            .iter()
            .filter_map(|(t, i)| {
                g.items
                    .get(&(job_id.to_string(), t.clone(), i.clone()))
                    .cloned()
            })
            .collect())
    }

    async fn put_item_states(
        &self,
        job_id: &str,
        items: &[SyncItemState],
    ) -> Result<(), SyncStoreError> {
        let mut g = self.inner.lock().unwrap();
        if !g.jobs.contains_key(job_id) {
            return Err(SyncStoreError::NotFound);
        }
        for i in items {
            g.items.insert(
                (
                    job_id.to_string(),
                    i.resource_type.clone(),
                    i.resource_id.clone(),
                ),
                i.clone(),
            );
        }
        Ok(())
    }
}

/// The store conformance suite (shared by every [`SyncStore`]).
pub mod conformance {
    use super::*;
    use helios_endpoints::sync::{ConflictPolicy, SyncPeer};

    fn spec(name: &str) -> SyncJobSpec {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "source": {"kind": "endpoint", "name": "a"},
            "target": {"kind": "endpoint", "name": "b"},
        }))
        .expect("spec")
    }

    /// Runs every check against `store` (job names are made unique per call).
    pub async fn run_all(store: &dyn SyncStore) {
        let tag = uuid::Uuid::new_v4().simple().to_string();
        jobs(store, &tag).await;
        runs_and_fencing(store, &tag).await;
        logs_conflicts_items(store, &tag).await;
    }

    async fn jobs(store: &dyn SyncStore, tag: &str) {
        let j = store
            .create_job(spec(&format!("j1-{tag}")), "admin")
            .await
            .unwrap();
        assert_eq!(j.revision, 1);
        assert_eq!(j.created_by.as_deref(), Some("admin"));
        assert!(matches!(
            store.create_job(spec(&format!("j1-{tag}")), "admin").await,
            Err(SyncStoreError::DuplicateName(_))
        ));
        let fixed = store
            .create_job_with_id(&format!("fixed-{tag}"), spec(&format!("j2-{tag}")), "admin")
            .await
            .unwrap();
        assert_eq!(fixed.id, format!("fixed-{tag}"));
        let mut s = j.spec.clone();
        s.conflict = ConflictPolicy::Fail;
        s.target = SyncPeer::Endpoint { name: "c".into() };
        let u = store.update_job(&j.id, 1, s.clone()).await.unwrap();
        assert_eq!(u.revision, 2);
        assert_eq!(u.spec, s);
        assert!(matches!(
            store.update_job(&j.id, 1, s.clone()).await,
            Err(SyncStoreError::Conflict { current: 2 })
        ));
        assert!(matches!(
            store.update_job("nope", 1, s.clone()).await,
            Err(SyncStoreError::NotFound)
        ));
        let mut clash = s.clone();
        clash.name = format!("j2-{tag}");
        assert!(matches!(
            store.update_job(&j.id, 2, clash).await,
            Err(SyncStoreError::DuplicateName(_))
        ));
        assert_eq!(store.job(&j.id).await.unwrap().unwrap().spec, s);
        assert!(store.job("nope").await.unwrap().is_none());
        let names: Vec<String> = store
            .jobs()
            .await
            .unwrap()
            .into_iter()
            .map(|j| j.spec.name)
            .collect();
        assert!(names.contains(&format!("j1-{tag}")) && names.contains(&format!("j2-{tag}")));
        let at = Utc::now();
        store.set_next_run_at(&j.id, Some(at)).await.unwrap();
        let got = store.job(&j.id).await.unwrap().unwrap();
        assert_eq!(got.revision, 2, "next_run_at does not bump the revision");
        assert!(
            got.next_run_at
                .is_some_and(|t| (t - at).num_milliseconds().abs() < 5)
        );
        assert!(matches!(
            store.set_next_run_at("nope", None).await,
            Err(SyncStoreError::NotFound)
        ));
        store.delete_job(&fixed.id).await.unwrap();
        assert!(matches!(
            store.delete_job(&fixed.id).await,
            Err(SyncStoreError::NotFound)
        ));
        store.delete_job(&j.id).await.unwrap();
    }

    async fn runs_and_fencing(store: &dyn SyncStore, tag: &str) {
        let j = store
            .create_job(spec(&format!("runs-{tag}")), "admin")
            .await
            .unwrap();
        let cursor = SyncCursor {
            phase: "incremental".into(),
            high_water: Some(Utc::now()),
            boundary: vec!["Patient/1/1".into()],
            ..Default::default()
        };
        let r = store
            .create_run(&j.id, RunReason::Retry, Some(cursor.clone()))
            .await
            .unwrap();
        assert_eq!(r.state, SyncRunState::Queued);
        assert_eq!(r.cursor, cursor);
        assert!(matches!(
            store.create_run(&j.id, RunReason::Manual, None).await,
            Err(SyncStoreError::ActiveRun(_))
        ));
        assert!(matches!(
            store.create_run("nope", RunReason::Manual, None).await,
            Err(SyncStoreError::NotFound)
        ));
        // Stale queued (orphan) sweep.
        let later = Utc::now() + chrono::Duration::seconds(5);
        assert!(
            store
                .stale_queued_runs(later)
                .await
                .unwrap()
                .iter()
                .any(|x| x.id == r.id)
        );
        // Claim needs the attached cluster job and an increasing token.
        assert!(matches!(
            store.claim_run(&r.id, "cj1", 1).await,
            Err(SyncStoreError::LeaseLost)
        ));
        store.attach_cluster_job(&r.id, "cj1").await.unwrap();
        assert_eq!(
            store
                .run(&r.id)
                .await
                .unwrap()
                .unwrap()
                .cluster_job_id
                .as_deref(),
            Some("cj1")
        );
        let c = store.claim_run(&r.id, "cj1", 1).await.unwrap();
        assert_eq!(c.state, SyncRunState::Running);
        assert!(c.started_at.is_some());
        assert!(matches!(
            store.claim_run(&r.id, "cj1", 1).await,
            Err(SyncStoreError::LeaseLost)
        ));
        assert!(matches!(
            store.claim_run(&r.id, "other", 9).await,
            Err(SyncStoreError::LeaseLost)
        ));
        let mut p = SyncProgress {
            read: 5,
            created: 3,
            ..Default::default()
        };
        store
            .checkpoint(&r.id, 1, SyncRunState::Running, &cursor, &p, None)
            .await
            .unwrap();
        // A newer worker reclaims (expired lease): the old token is fenced.
        store.claim_run(&r.id, "cj1", 2).await.unwrap();
        assert!(matches!(
            store
                .checkpoint(&r.id, 1, SyncRunState::Running, &cursor, &p, None)
                .await,
            Err(SyncStoreError::LeaseLost)
        ));
        p.updated = 1;
        store
            .checkpoint(&r.id, 2, SyncRunState::Running, &cursor, &p, None)
            .await
            .unwrap();
        assert_eq!(store.run(&r.id).await.unwrap().unwrap().progress, p);
        // Cancel: the worker's next checkpoint is refused.
        assert!(store.cancel_run(&r.id, "cancelled by admin").await.unwrap());
        assert!(!store.cancel_run(&r.id, "again").await.unwrap());
        assert!(matches!(
            store
                .checkpoint(&r.id, 2, SyncRunState::Succeeded, &cursor, &p, None)
                .await,
            Err(SyncStoreError::LeaseLost)
        ));
        let done = store.run(&r.id).await.unwrap().unwrap();
        assert_eq!(done.state, SyncRunState::Cancelled);
        assert!(done.finished_at.is_some());
        assert!(matches!(
            store.attach_cluster_job(&r.id, "x").await,
            Err(SyncStoreError::NotFound)
        ));
        // Terminal: a new run can start; finishing records the state.
        let r2 = store
            .create_run(&j.id, RunReason::Manual, None)
            .await
            .unwrap();
        store.attach_cluster_job(&r2.id, "cj2").await.unwrap();
        store.claim_run(&r2.id, "cj2", 1).await.unwrap();
        store
            .checkpoint(&r2.id, 1, SyncRunState::Failed, &cursor, &p, Some("boom"))
            .await
            .unwrap();
        let f = store.run(&r2.id).await.unwrap().unwrap();
        assert_eq!(
            (f.state, f.error.as_deref()),
            (SyncRunState::Failed, Some("boom"))
        );
        let listed = store.runs(&j.id, 10).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, r2.id, "newest first");
        assert_eq!(store.runs(&j.id, 1).await.unwrap().len(), 1);
        assert!(store.run("nope").await.unwrap().is_none());
        store.delete_job(&j.id).await.unwrap();
        assert!(store.run(&r.id).await.unwrap().is_none(), "runs cascade");
    }

    async fn logs_conflicts_items(store: &dyn SyncStore, tag: &str) {
        let j = store
            .create_job(spec(&format!("logs-{tag}")), "admin")
            .await
            .unwrap();
        let r = store
            .create_run(&j.id, RunReason::Manual, None)
            .await
            .unwrap();
        for i in 0..5 {
            store
                .log(&r.id, LogLevel::Info, &format!("line {i}"))
                .await
                .unwrap();
        }
        store
            .log(&r.id, LogLevel::Error, &"x".repeat(5000))
            .await
            .unwrap();
        let all = store.log_lines(&r.id, 0, 100).await.unwrap();
        assert_eq!(all.len(), 6);
        assert!(all.windows(2).all(|w| w[0].seq < w[1].seq));
        assert_eq!(all[5].level, LogLevel::Error);
        assert!(all[5].message.len() <= 2000);
        let tail = store.log_lines(&r.id, all[2].seq, 2).await.unwrap();
        assert_eq!(
            tail.iter().map(|l| l.message.as_str()).collect::<Vec<_>>(),
            vec!["line 3", "line 4"]
        );
        assert!(matches!(
            store.log("nope", LogLevel::Info, "x").await,
            Err(SyncStoreError::NotFound)
        ));

        let c = SyncConflict {
            run_id: r.id.clone(),
            resource_type: "Patient".into(),
            resource_id: "p1".into(),
            source_version: Some("2".into()),
            target_version: Some("5".into()),
            reason: "target changed".into(),
            at: Utc::now(),
        };
        store.conflict(&c).await.unwrap();
        let mut again = c.clone();
        again.reason = "still".into();
        store.conflict(&again).await.unwrap();
        let cs = store.conflicts(&r.id, 10).await.unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].reason, "still");
        assert!(store.delete_conflict(&r.id, "Patient", "p1").await.unwrap());
        assert!(!store.delete_conflict(&r.id, "Patient", "p1").await.unwrap());

        let now = Utc::now();
        let items = vec![
            SyncItemState {
                resource_type: "Patient".into(),
                resource_id: "p1".into(),
                source_version: Some("1".into()),
                target_version: Some("1".into()),
                content_sha256: Some(vec![1; 32]),
                deleted: false,
                applied_at: now,
            },
            SyncItemState {
                resource_type: "Patient".into(),
                resource_id: "p2".into(),
                source_version: None,
                target_version: None,
                content_sha256: None,
                deleted: true,
                applied_at: now,
            },
        ];
        store.put_item_states(&j.id, &items).await.unwrap();
        let mut upd = items[0].clone();
        upd.source_version = Some("2".into());
        store.put_item_states(&j.id, &[upd.clone()]).await.unwrap();
        let keys = vec![
            ("Patient".to_string(), "p1".to_string()),
            ("Patient".to_string(), "p2".to_string()),
            ("Patient".to_string(), "absent".to_string()),
        ];
        let mut got = store.item_states(&j.id, &keys).await.unwrap();
        got.sort_by(|a, b| a.resource_id.cmp(&b.resource_id));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].source_version.as_deref(), Some("2"));
        assert_eq!(got[0].content_sha256.as_deref(), Some(&[1u8; 32][..]));
        assert!(got[1].deleted && got[1].content_sha256.is_none());
        assert!(store.item_states(&j.id, &[]).await.unwrap().is_empty());
        store.put_item_states(&j.id, &[]).await.unwrap();
        store.delete_job(&j.id).await.unwrap();
        assert!(
            store.item_states(&j.id, &keys).await.unwrap().is_empty(),
            "items cascade"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_store_conformance() {
        conformance::run_all(&MemorySyncStore::new()).await;
    }

    #[tokio::test]
    async fn log_is_bounded() {
        let s = MemorySyncStore::new();
        let j = s
            .create_job(
                serde_json::from_value(serde_json::json!({
            "name": "n", "source": {"kind": "endpoint", "name": "a"},
            "target": {"kind": "endpoint", "name": "b"}}))
                .unwrap(),
                "a",
            )
            .await
            .unwrap();
        let r = s.create_run(&j.id, RunReason::Manual, None).await.unwrap();
        for i in 0..(MAX_LOG + 5) {
            s.log(&r.id, LogLevel::Info, &i.to_string()).await.unwrap();
        }
        let lines = s.log_lines(&r.id, 0, MAX_LOG * 2).await.unwrap();
        assert_eq!(lines.len(), MAX_LOG);
        assert_eq!(lines[0].message, "5");
    }
}
