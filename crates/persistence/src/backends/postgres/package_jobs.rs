//! [`PackageJobCoordinator`] on PostgreSQL: a session advisory lock held on a
//! dedicated pooled connection, and the `hfs_package_jobs` table (fork migration `hm-0005-package-jobs`).

use async_trait::async_trait;

use crate::core::package_jobs::{InstallLockGuard, PackageJobCoordinator, PackageJobRecord};
use crate::error::{BackendError, StorageError, StorageResult};

/// Advisory-lock namespace ("HMPK").
const LOCK_NAMESPACE: i32 = 0x484d_504b;

/// Pool-sharing handle.
pub struct PgPackageJobCoordinator {
    pool: deadpool_postgres::Pool,
}

impl PgPackageJobCoordinator {
    pub(crate) fn new(pool: deadpool_postgres::Pool) -> Self {
        Self { pool }
    }

    async fn client(&self) -> StorageResult<deadpool_postgres::Client> {
        self.pool.get().await.map_err(|e| {
            StorageError::Backend(BackendError::ConnectionFailed {
                backend_name: "postgres".to_string(),
                message: e.to_string(),
            })
        })
    }
}

fn internal(message: String) -> StorageError {
    StorageError::Backend(BackendError::Internal {
        backend_name: "postgres".to_string(),
        message,
        source: None,
    })
}

/// Holds the connection that owns the session lock; on drop, unlocks on a
/// background task (the pool gets the connection back afterwards). If the
/// process dies the session ends and PostgreSQL releases the lock itself.
struct PgLockGuard {
    client: Option<deadpool_postgres::Client>,
    key: i32,
}

impl InstallLockGuard for PgLockGuard {}

impl Drop for PgLockGuard {
    fn drop(&mut self) {
        let Some(client) = self.client.take() else {
            return;
        };
        let key = self.key;
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    if let Err(e) = client
                        .execute("SELECT pg_advisory_unlock($1, $2)", &[&LOCK_NAMESPACE, &key])
                        .await
                    {
                        tracing::warn!(error = %e, "releasing the package install lock failed");
                    }
                });
            }
            Err(_) => {
                // No runtime: discard the connection so the session (and its
                // lock) ends instead of returning a locked session to the pool.
                let _ = deadpool_postgres::Object::take(client);
            }
        }
    }
}

fn to_record(row: &tokio_postgres::Row) -> PackageJobRecord {
    PackageJobRecord {
        id: row.get(0),
        tenant: row.get(1),
        kind: row.get(2),
        status: row.get(3),
        terminal: row.get(4),
        view: row.get(5),
        cancel_requested: row.get(6),
        owner: row.get(7),
        updated_at: row.get(8),
    }
}

const COLUMNS: &str =
    "id, tenant_id, kind, status, terminal, view, cancel_requested, owner, updated_at";

#[async_trait]
impl PackageJobCoordinator for PgPackageJobCoordinator {
    async fn lock_installs(&self, key: i32) -> StorageResult<Box<dyn InstallLockGuard>> {
        let client = self.client().await?;
        client
            .execute("SELECT pg_advisory_lock($1, $2)", &[&LOCK_NAMESPACE, &key])
            .await
            .map_err(|e| internal(format!("take the package install lock: {e}")))?;
        Ok(Box::new(PgLockGuard {
            client: Some(client),
            key,
        }))
    }

    async fn save_job(&self, job: &PackageJobRecord) -> StorageResult<()> {
        let client = self.client().await?;
        client
            .execute(
                "INSERT INTO hfs_package_jobs
                     (id, tenant_id, kind, status, terminal, view, cancel_requested, owner,
                      updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, FALSE, $7, NOW())
                 ON CONFLICT (id) DO UPDATE SET
                     status = EXCLUDED.status,
                     terminal = EXCLUDED.terminal,
                     view = EXCLUDED.view,
                     owner = EXCLUDED.owner,
                     updated_at = NOW()",
                &[
                    &job.id,
                    &job.tenant,
                    &job.kind,
                    &job.status,
                    &job.terminal,
                    &job.view,
                    &job.owner,
                ],
            )
            .await
            .map_err(|e| internal(format!("save package job: {e}")))?;
        // Keep the table small: drop finished jobs older than a week.
        client
            .execute(
                "DELETE FROM hfs_package_jobs
                 WHERE terminal AND updated_at < NOW() - INTERVAL '7 days'",
                &[],
            )
            .await
            .map_err(|e| internal(format!("prune package jobs: {e}")))?;
        Ok(())
    }

    async fn load_job(&self, id: &str) -> StorageResult<Option<PackageJobRecord>> {
        let client = self.client().await?;
        let row = client
            .query_opt(
                &format!("SELECT {COLUMNS} FROM hfs_package_jobs WHERE id = $1"),
                &[&id],
            )
            .await
            .map_err(|e| internal(format!("load package job: {e}")))?;
        Ok(row.as_ref().map(to_record))
    }

    async fn list_jobs(&self, limit: usize) -> StorageResult<Vec<PackageJobRecord>> {
        let client = self.client().await?;
        let rows = client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM hfs_package_jobs
                     ORDER BY created_at DESC LIMIT $1"
                ),
                &[&(limit as i64)],
            )
            .await
            .map_err(|e| internal(format!("list package jobs: {e}")))?;
        Ok(rows.iter().map(to_record).collect())
    }

    async fn request_cancel(&self, id: &str) -> StorageResult<bool> {
        let client = self.client().await?;
        let n = client
            .execute(
                "UPDATE hfs_package_jobs SET cancel_requested = TRUE
                 WHERE id = $1 AND NOT terminal",
                &[&id],
            )
            .await
            .map_err(|e| internal(format!("request package job cancel: {e}")))?;
        Ok(n > 0)
    }

    async fn cancel_requested(&self, id: &str) -> StorageResult<bool> {
        let client = self.client().await?;
        let row = client
            .query_opt(
                "SELECT cancel_requested FROM hfs_package_jobs WHERE id = $1",
                &[&id],
            )
            .await
            .map_err(|e| internal(format!("read package job cancel flag: {e}")))?;
        Ok(row.is_some_and(|r| r.get(0)))
    }
}
