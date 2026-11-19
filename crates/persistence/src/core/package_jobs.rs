//! Cluster coordination for IG package jobs (fork; multi-instance HFS).
//!
//! The package manager runs install / uninstall jobs in the process that
//! received the request and keeps their progress in memory. Behind a load
//! balancer that has two problems: a status poll or cancel that lands on
//! another instance finds no job, and two instances can install into the same
//! tenant at once (the per-process install mutex does not reach across
//! processes; the `HFS_PACKAGES` startup installs of N instances booting
//! together race each other).
//!
//! A [`PackageJobCoordinator`] backed by the shared database fixes both: a
//! cluster-wide install lock, and a mirror of every job's observable state
//! (written by the owning instance, read by any) with a cancel flag the owner
//! polls.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::error::StorageResult;

/// One package job as the shared mirror holds it.
#[derive(Debug, Clone)]
pub struct PackageJobRecord {
    /// Job id.
    pub id: String,
    /// Tenant the job installs into / removes from.
    pub tenant: String,
    /// Job kind (`plan`, `install`, `uninstall`).
    pub kind: String,
    /// Lifecycle status (`queued`, `running`, `succeeded`, …).
    pub status: String,
    /// Whether the status is final.
    pub terminal: bool,
    /// The full observable job state (the package manager's `JobView`).
    pub view: Value,
    /// Whether a cancel was requested through any instance.
    pub cancel_requested: bool,
    /// Instance running the job.
    pub owner: String,
    /// Last write by the owner.
    pub updated_at: DateTime<Utc>,
}

/// Held while a package job installs or uninstalls; dropping it releases the
/// cluster-wide lock.
pub trait InstallLockGuard: Send + Sync {}

/// Shared install lock and job mirror.
#[async_trait]
pub trait PackageJobCoordinator: Send + Sync {
    /// Waits for, then takes, the cluster-wide package install lock. `key`
    /// separates independent critical sections (`0` for install/uninstall
    /// jobs, `1` for the `HFS_PACKAGES` startup pass).
    async fn lock_installs(&self, key: i32) -> StorageResult<Box<dyn InstallLockGuard>>;

    /// Inserts or replaces a job's mirrored state (the owner calls this as the
    /// job progresses). Keeps a `cancel_requested` flag already set.
    async fn save_job(&self, job: &PackageJobRecord) -> StorageResult<()>;

    /// One job.
    async fn load_job(&self, id: &str) -> StorageResult<Option<PackageJobRecord>>;

    /// The newest `limit` jobs, newest first.
    async fn list_jobs(&self, limit: usize) -> StorageResult<Vec<PackageJobRecord>>;

    /// Asks the owner to cancel a running job. Returns `false` when the job
    /// is unknown or already over.
    async fn request_cancel(&self, id: &str) -> StorageResult<bool>;

    /// Whether a cancel was requested for the job.
    async fn cancel_requested(&self, id: &str) -> StorageResult<bool>;
}
