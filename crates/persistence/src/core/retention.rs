//! Time-based retention of append-only resources (the audit trail).
//!
//! The audit retention job asks for the oldest resources of a type that were
//! last updated before a cutoff — across **all** tenants, since retention is a
//! server-wide policy — optionally archives them, and then erases them.
//!
//! The erase is a hard purge of every row the resource has (current row,
//! history, search index, full text), done without the per-tenant write locks
//! a regular purge takes: it is meant for resources that are never written
//! again once created (`AuditEvent`, which the REST API refuses to update or
//! delete), so there is no concurrent writer to serialize against. Do not use
//! it for mutable resource types.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::error::StorageResult;

/// A resource selected for expiry.
#[derive(Debug, Clone)]
pub struct ExpiredResource {
    /// Tenant the resource belongs to.
    pub tenant_id: String,
    /// Logical id.
    pub id: String,
    /// When the current version was written.
    pub last_updated: DateTime<Utc>,
    /// The stored content (for archiving).
    pub content: Value,
}

/// Storage that can expire append-only resources by age.
#[async_trait]
pub trait RetentionStorage: Send + Sync {
    /// Up to `limit` resources of `resource_type`, oldest first, whose current
    /// version was last updated before `cutoff`, in any tenant.
    async fn expired_resources(
        &self,
        resource_type: &str,
        cutoff: DateTime<Utc>,
        limit: usize,
    ) -> StorageResult<Vec<ExpiredResource>>;

    /// Permanently erases the given `(tenant_id, id)` resources of
    /// `resource_type` in one transaction. Returns how many current rows were
    /// removed.
    async fn purge_expired(
        &self,
        resource_type: &str,
        keys: &[(String, String)],
    ) -> StorageResult<u64>;
}
