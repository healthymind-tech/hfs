//! PostgreSQL [`SyncStore`] over fork migration `hm-0010-endpoint-sync`
//! (package P6 of `docs/design/fhir-server-manager.md`).
//!
//! Every instance of a cluster shares these rows, so a run's progress, log
//! and conflicts are readable from any instance, and the fencing token on
//! `hfs_sync_runs` (the claiming `cluster_jobs` lease token) makes a worker
//! that lost its lease unable to write a checkpoint.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deadpool_postgres::Pool;
use serde::de::DeserializeOwned;
use tokio_postgres::Row;
use tokio_postgres::error::SqlState;

use crate::sync::{
    LogLevel, RunReason, SyncConflict, SyncCursor, SyncItemState, SyncJob, SyncJobSpec,
    SyncLogLine, SyncProgress, SyncRun, SyncRunState, SyncStore, SyncStoreError,
};

/// Log lines kept per run; older ones are pruned.
pub const MAX_LOG_LINES: i64 = 10_000;

/// How often (in inserted lines, by global sequence) the log is pruned.
const PRUNE_EVERY: i64 = 256;

const RUN_COLUMNS: &str = "id, job_id, cluster_job_id, state, reason, cursor, progress, error, \
                           created_at, started_at, finished_at";

const JOB_COLUMNS: &str = "id, revision, spec, next_run_at, created_at, updated_at, created_by";

/// The PostgreSQL sync store.
#[derive(Clone)]
pub struct PgSyncStore {
    pool: Pool,
}

impl std::fmt::Debug for PgSyncStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgSyncStore").finish_non_exhaustive()
    }
}

impl PgSyncStore {
    /// Wraps the server's shared pool. The tables come from the fork
    /// migration track (`hm-0010-endpoint-sync`); nothing is created here.
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    async fn client(&self) -> Result<deadpool_postgres::Object, SyncStoreError> {
        self.pool
            .get()
            .await
            .map_err(|e| SyncStoreError::Unavailable(e.to_string()))
    }
}

fn db(e: tokio_postgres::Error) -> SyncStoreError {
    SyncStoreError::Unavailable(e.to_string())
}

fn is_unique_violation(e: &tokio_postgres::Error, constraint: &str) -> bool {
    e.as_db_error().is_some_and(|d| {
        d.code() == &SqlState::UNIQUE_VIOLATION && d.constraint().is_some_and(|c| c == constraint)
    })
}

fn is_fk_violation(e: &tokio_postgres::Error) -> bool {
    e.as_db_error()
        .is_some_and(|d| d.code() == &SqlState::FOREIGN_KEY_VIOLATION)
}

/// Parses a lowercase / kebab-case enum stored as text.
fn text_enum<T: DeserializeOwned>(s: &str) -> Result<T, SyncStoreError> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| SyncStoreError::Unavailable(format!("unexpected stored value {s:?}")))
}

fn enum_text<T: serde::Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

fn job_from_row(row: &Row) -> Result<SyncJob, SyncStoreError> {
    let spec: serde_json::Value = row.get("spec");
    let spec: SyncJobSpec = serde_json::from_value(spec)
        .map_err(|e| SyncStoreError::Unavailable(format!("stored job spec: {e}")))?;
    let revision: i64 = row.get("revision");
    Ok(SyncJob {
        id: row.get("id"),
        revision: revision.max(0) as u64,
        spec,
        next_run_at: row.get("next_run_at"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        created_by: row.get("created_by"),
    })
}

fn run_from_row(row: &Row) -> Result<SyncRun, SyncStoreError> {
    let state: String = row.get("state");
    let reason: String = row.get("reason");
    let cursor: serde_json::Value = row.get("cursor");
    let progress: serde_json::Value = row.get("progress");
    Ok(SyncRun {
        id: row.get("id"),
        job_id: row.get("job_id"),
        state: text_enum::<SyncRunState>(&state)?,
        reason: text_enum::<RunReason>(&reason)?,
        cursor: serde_json::from_value::<SyncCursor>(cursor).unwrap_or_default(),
        progress: serde_json::from_value::<SyncProgress>(progress).unwrap_or_default(),
        error: row.get("error"),
        cluster_job_id: row.get("cluster_job_id"),
        created_at: row.get("created_at"),
        started_at: row.get("started_at"),
        finished_at: row.get("finished_at"),
    })
}

#[async_trait]
impl SyncStore for PgSyncStore {
    async fn create_job_with_id(
        &self,
        id: &str,
        spec: SyncJobSpec,
        actor: &str,
    ) -> Result<SyncJob, SyncStoreError> {
        let c = self.client().await?;
        let json =
            serde_json::to_value(&spec).map_err(|e| SyncStoreError::Unavailable(e.to_string()))?;
        let row = c
            .query_one(
                &format!(
                    "INSERT INTO hfs_sync_jobs (id, name, spec, enabled, created_by, updated_by)
                     VALUES ($1, $2, $3, $4, $5, $5) RETURNING {JOB_COLUMNS}"
                ),
                &[&id, &spec.name, &json, &spec.enabled, &actor],
            )
            .await
            .map_err(|e| {
                if is_unique_violation(&e, "uq_hfs_sync_jobs_name") {
                    SyncStoreError::DuplicateName(spec.name.clone())
                } else {
                    db(e)
                }
            })?;
        job_from_row(&row)
    }

    async fn update_job(
        &self,
        id: &str,
        expected_revision: u64,
        spec: SyncJobSpec,
    ) -> Result<SyncJob, SyncStoreError> {
        let c = self.client().await?;
        let json =
            serde_json::to_value(&spec).map_err(|e| SyncStoreError::Unavailable(e.to_string()))?;
        let expected = expected_revision as i64;
        let row = c
            .query_opt(
                &format!(
                    "UPDATE hfs_sync_jobs
                        SET name = $3, spec = $4, enabled = $5,
                            revision = revision + 1, updated_at = NOW()
                      WHERE id = $1 AND revision = $2
                  RETURNING {JOB_COLUMNS}"
                ),
                &[&id, &expected, &spec.name, &json, &spec.enabled],
            )
            .await
            .map_err(|e| {
                if is_unique_violation(&e, "uq_hfs_sync_jobs_name") {
                    SyncStoreError::DuplicateName(spec.name.clone())
                } else {
                    db(e)
                }
            })?;
        match row {
            Some(row) => job_from_row(&row),
            None => {
                let current = c
                    .query_opt("SELECT revision FROM hfs_sync_jobs WHERE id = $1", &[&id])
                    .await
                    .map_err(db)?;
                match current {
                    Some(r) => Err(SyncStoreError::Conflict {
                        current: r.get::<_, i64>(0).max(0) as u64,
                    }),
                    None => Err(SyncStoreError::NotFound),
                }
            }
        }
    }

    async fn job(&self, id: &str) -> Result<Option<SyncJob>, SyncStoreError> {
        let c = self.client().await?;
        c.query_opt(
            &format!("SELECT {JOB_COLUMNS} FROM hfs_sync_jobs WHERE id = $1"),
            &[&id],
        )
        .await
        .map_err(db)?
        .map(|r| job_from_row(&r))
        .transpose()
    }

    async fn jobs(&self) -> Result<Vec<SyncJob>, SyncStoreError> {
        let c = self.client().await?;
        c.query(
            &format!("SELECT {JOB_COLUMNS} FROM hfs_sync_jobs ORDER BY name"),
            &[],
        )
        .await
        .map_err(db)?
        .iter()
        .map(job_from_row)
        .collect()
    }

    async fn delete_job(&self, id: &str) -> Result<(), SyncStoreError> {
        let c = self.client().await?;
        let n = c
            .execute("DELETE FROM hfs_sync_jobs WHERE id = $1", &[&id])
            .await
            .map_err(db)?;
        if n == 0 {
            return Err(SyncStoreError::NotFound);
        }
        Ok(())
    }

    async fn create_run(
        &self,
        job_id: &str,
        reason: RunReason,
        resume_from: Option<SyncCursor>,
    ) -> Result<SyncRun, SyncStoreError> {
        let c = self.client().await?;
        let id = uuid::Uuid::new_v4().to_string();
        let cursor = serde_json::to_value(resume_from.unwrap_or_default())
            .map_err(|e| SyncStoreError::Unavailable(e.to_string()))?;
        let progress = serde_json::to_value(SyncProgress::default())
            .map_err(|e| SyncStoreError::Unavailable(e.to_string()))?;
        let reason_s = enum_text(&reason);
        let row = c
            .query_one(
                &format!(
                    "INSERT INTO hfs_sync_runs (id, job_id, state, reason, cursor, progress)
                     VALUES ($1, $2, 'queued', $3, $4, $5) RETURNING {RUN_COLUMNS}"
                ),
                &[&id, &job_id, &reason_s, &cursor, &progress],
            )
            .await
            .map_err(|e| {
                if is_unique_violation(&e, "uq_hfs_sync_runs_active") {
                    SyncStoreError::ActiveRun(job_id.to_string())
                } else if is_fk_violation(&e) {
                    SyncStoreError::NotFound
                } else {
                    db(e)
                }
            })?;
        run_from_row(&row)
    }

    async fn run(&self, id: &str) -> Result<Option<SyncRun>, SyncStoreError> {
        let c = self.client().await?;
        c.query_opt(
            &format!("SELECT {RUN_COLUMNS} FROM hfs_sync_runs WHERE id = $1"),
            &[&id],
        )
        .await
        .map_err(db)?
        .map(|r| run_from_row(&r))
        .transpose()
    }

    async fn runs(&self, job_id: &str, limit: usize) -> Result<Vec<SyncRun>, SyncStoreError> {
        let c = self.client().await?;
        let limit = limit.min(10_000) as i64;
        c.query(
            &format!(
                "SELECT {RUN_COLUMNS} FROM hfs_sync_runs WHERE job_id = $1
                  ORDER BY created_at DESC, id DESC LIMIT $2"
            ),
            &[&job_id, &limit],
        )
        .await
        .map_err(db)?
        .iter()
        .map(run_from_row)
        .collect()
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
        let c = self.client().await?;
        let cursor =
            serde_json::to_value(cursor).map_err(|e| SyncStoreError::Unavailable(e.to_string()))?;
        let progress = serde_json::to_value(progress)
            .map_err(|e| SyncStoreError::Unavailable(e.to_string()))?;
        let state_s = enum_text(&state);
        let terminal = state.is_terminal();
        let n = c
            .execute(
                "UPDATE hfs_sync_runs
                    SET state = $3, cursor = $4, progress = $5, error = $6,
                        updated_at = NOW(),
                        finished_at = CASE WHEN $7 THEN NOW() ELSE finished_at END
                  WHERE id = $1 AND fencing_token = $2 AND state IN ('queued', 'running')",
                &[
                    &run_id,
                    &fencing_token,
                    &state_s,
                    &cursor,
                    &progress,
                    &error,
                    &terminal,
                ],
            )
            .await
            .map_err(db)?;
        if n == 0 {
            return Err(SyncStoreError::LeaseLost);
        }
        Ok(())
    }

    async fn log(
        &self,
        run_id: &str,
        level: LogLevel,
        message: &str,
    ) -> Result<(), SyncStoreError> {
        let c = self.client().await?;
        let level_s = enum_text(&level);
        // Bounded line length: messages carry type/id/version and errors.
        let message: String = message.chars().take(2000).collect();
        let row = c
            .query_one(
                "INSERT INTO hfs_sync_log (run_id, level, message) VALUES ($1, $2, $3)
                 RETURNING seq",
                &[&run_id, &level_s, &message],
            )
            .await
            .map_err(|e| {
                if is_fk_violation(&e) {
                    SyncStoreError::NotFound
                } else {
                    db(e)
                }
            })?;
        let seq: i64 = row.get(0);
        if seq % PRUNE_EVERY == 0 {
            c.execute(
                "DELETE FROM hfs_sync_log
                  WHERE run_id = $1
                    AND seq <= (SELECT seq FROM hfs_sync_log WHERE run_id = $1
                                 ORDER BY seq DESC OFFSET $2 LIMIT 1)",
                &[&run_id, &MAX_LOG_LINES],
            )
            .await
            .map_err(db)?;
        }
        Ok(())
    }

    async fn log_lines(
        &self,
        run_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SyncLogLine>, SyncStoreError> {
        let c = self.client().await?;
        let after = after_seq.min(i64::MAX as u64) as i64;
        let limit = limit.min(10_000) as i64;
        let rows = c
            .query(
                "SELECT seq, at, level, message FROM hfs_sync_log
                  WHERE run_id = $1 AND seq > $2 ORDER BY seq LIMIT $3",
                &[&run_id, &after, &limit],
            )
            .await
            .map_err(db)?;
        rows.iter()
            .map(|r| {
                let level: String = r.get(2);
                Ok(SyncLogLine {
                    seq: r.get::<_, i64>(0).max(0) as u64,
                    at: r.get(1),
                    level: text_enum(&level)?,
                    message: r.get(3),
                })
            })
            .collect()
    }

    async fn conflict(&self, x: &SyncConflict) -> Result<(), SyncStoreError> {
        let c = self.client().await?;
        c.execute(
            "INSERT INTO hfs_sync_conflicts
                 (run_id, resource_type, resource_id, source_version, target_version, reason, at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (run_id, resource_type, resource_id) DO UPDATE
                SET source_version = EXCLUDED.source_version,
                    target_version = EXCLUDED.target_version,
                    reason = EXCLUDED.reason, at = EXCLUDED.at",
            &[
                &x.run_id,
                &x.resource_type,
                &x.resource_id,
                &x.source_version,
                &x.target_version,
                &x.reason,
                &x.at,
            ],
        )
        .await
        .map_err(|e| {
            if is_fk_violation(&e) {
                SyncStoreError::NotFound
            } else {
                db(e)
            }
        })?;
        Ok(())
    }

    async fn conflicts(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<SyncConflict>, SyncStoreError> {
        let c = self.client().await?;
        let limit = limit.min(10_000) as i64;
        let rows = c
            .query(
                "SELECT run_id, resource_type, resource_id, source_version, target_version,
                        reason, at
                   FROM hfs_sync_conflicts WHERE run_id = $1
                  ORDER BY at, resource_type, resource_id LIMIT $2",
                &[&run_id, &limit],
            )
            .await
            .map_err(db)?;
        Ok(rows
            .iter()
            .map(|r| SyncConflict {
                run_id: r.get(0),
                resource_type: r.get(1),
                resource_id: r.get(2),
                source_version: r.get(3),
                target_version: r.get(4),
                reason: r.get(5),
                at: r.get(6),
            })
            .collect())
    }

    async fn delete_conflict(
        &self,
        run_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<bool, SyncStoreError> {
        let c = self.client().await?;
        let n = c
            .execute(
                "DELETE FROM hfs_sync_conflicts
                  WHERE run_id = $1 AND resource_type = $2 AND resource_id = $3",
                &[&run_id, &resource_type, &resource_id],
            )
            .await
            .map_err(db)?;
        Ok(n > 0)
    }

    async fn attach_cluster_job(
        &self,
        run_id: &str,
        cluster_job_id: &str,
    ) -> Result<(), SyncStoreError> {
        let c = self.client().await?;
        let n = c
            .execute(
                "UPDATE hfs_sync_runs SET cluster_job_id = $2, updated_at = NOW()
                  WHERE id = $1 AND state = 'queued'",
                &[&run_id, &cluster_job_id],
            )
            .await
            .map_err(db)?;
        if n == 0 {
            return Err(SyncStoreError::NotFound);
        }
        Ok(())
    }

    async fn claim_run(
        &self,
        run_id: &str,
        cluster_job_id: &str,
        fencing_token: i64,
    ) -> Result<SyncRun, SyncStoreError> {
        let c = self.client().await?;
        let row = c
            .query_opt(
                &format!(
                    "UPDATE hfs_sync_runs
                        SET state = 'running', fencing_token = $3,
                            started_at = COALESCE(started_at, NOW()), updated_at = NOW()
                      WHERE id = $1 AND cluster_job_id = $2
                        AND state IN ('queued', 'running') AND fencing_token < $3
                  RETURNING {RUN_COLUMNS}"
                ),
                &[&run_id, &cluster_job_id, &fencing_token],
            )
            .await
            .map_err(db)?;
        match row {
            Some(r) => run_from_row(&r),
            None => Err(SyncStoreError::LeaseLost),
        }
    }

    async fn cancel_run(&self, run_id: &str, reason: &str) -> Result<bool, SyncStoreError> {
        let c = self.client().await?;
        let n = c
            .execute(
                "UPDATE hfs_sync_runs
                    SET state = 'cancelled', error = $2, finished_at = NOW(), updated_at = NOW()
                  WHERE id = $1 AND state IN ('queued', 'running')",
                &[&run_id, &reason],
            )
            .await
            .map_err(db)?;
        Ok(n > 0)
    }

    async fn stale_queued_runs(
        &self,
        before: DateTime<Utc>,
    ) -> Result<Vec<SyncRun>, SyncStoreError> {
        let c = self.client().await?;
        c.query(
            &format!(
                "SELECT {RUN_COLUMNS} FROM hfs_sync_runs
                  WHERE state = 'queued' AND created_at < $1 ORDER BY created_at LIMIT 500"
            ),
            &[&before],
        )
        .await
        .map_err(db)?
        .iter()
        .map(run_from_row)
        .collect()
    }

    async fn set_next_run_at(
        &self,
        job_id: &str,
        at: Option<DateTime<Utc>>,
    ) -> Result<(), SyncStoreError> {
        let c = self.client().await?;
        // A plain column write: no revision bump, but the statement trigger
        // still announces it (scope `sync`), which is harmless.
        let n = c
            .execute(
                "UPDATE hfs_sync_jobs SET next_run_at = $2 WHERE id = $1",
                &[&job_id, &at],
            )
            .await
            .map_err(db)?;
        if n == 0 {
            return Err(SyncStoreError::NotFound);
        }
        Ok(())
    }

    async fn item_states(
        &self,
        job_id: &str,
        keys: &[(String, String)],
    ) -> Result<Vec<SyncItemState>, SyncStoreError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let c = self.client().await?;
        let types: Vec<&str> = keys.iter().map(|k| k.0.as_str()).collect();
        let ids: Vec<&str> = keys.iter().map(|k| k.1.as_str()).collect();
        let rows = c
            .query(
                "SELECT s.resource_type, s.resource_id, s.source_version, s.target_version,
                        s.content_sha256, s.deleted, s.applied_at
                   FROM hfs_sync_item_state s
                   JOIN unnest($2::text[], $3::text[]) AS k(t, i)
                     ON s.resource_type = k.t AND s.resource_id = k.i
                  WHERE s.job_id = $1",
                &[&job_id, &types, &ids],
            )
            .await
            .map_err(db)?;
        Ok(rows
            .iter()
            .map(|r| SyncItemState {
                resource_type: r.get(0),
                resource_id: r.get(1),
                source_version: r.get(2),
                target_version: r.get(3),
                content_sha256: r.get(4),
                deleted: r.get(5),
                applied_at: r.get(6),
            })
            .collect())
    }

    async fn put_item_states(
        &self,
        job_id: &str,
        items: &[SyncItemState],
    ) -> Result<(), SyncStoreError> {
        if items.is_empty() {
            return Ok(());
        }
        let c = self.client().await?;
        let types: Vec<&str> = items.iter().map(|i| i.resource_type.as_str()).collect();
        let ids: Vec<&str> = items.iter().map(|i| i.resource_id.as_str()).collect();
        let sv: Vec<Option<&str>> = items.iter().map(|i| i.source_version.as_deref()).collect();
        let tv: Vec<Option<&str>> = items.iter().map(|i| i.target_version.as_deref()).collect();
        let hashes: Vec<Option<&[u8]>> =
            items.iter().map(|i| i.content_sha256.as_deref()).collect();
        let deleted: Vec<bool> = items.iter().map(|i| i.deleted).collect();
        let at: Vec<DateTime<Utc>> = items.iter().map(|i| i.applied_at).collect();
        c.execute(
            "INSERT INTO hfs_sync_item_state
                 (job_id, resource_type, resource_id, source_version, target_version,
                  content_sha256, deleted, applied_at)
             SELECT $1, t, i, sv, tv, h, d, a
               FROM unnest($2::text[], $3::text[], $4::text[], $5::text[], $6::bytea[],
                           $7::bool[], $8::timestamptz[]) AS x(t, i, sv, tv, h, d, a)
             ON CONFLICT (job_id, resource_type, resource_id) DO UPDATE
                SET source_version = EXCLUDED.source_version,
                    target_version = EXCLUDED.target_version,
                    content_sha256 = EXCLUDED.content_sha256,
                    deleted = EXCLUDED.deleted,
                    applied_at = EXCLUDED.applied_at",
            &[&job_id, &types, &ids, &sv, &tv, &hashes, &deleted, &at],
        )
        .await
        .map_err(|e| {
            if is_fk_violation(&e) {
                SyncStoreError::NotFound
            } else {
                db(e)
            }
        })?;
        Ok(())
    }
}
