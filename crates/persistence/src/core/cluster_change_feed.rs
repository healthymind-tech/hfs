//! Cross-instance change feed for definition resources (cluster deployments).
//!
//! Several HFS subsystems compile stored resources into per-process caches —
//! most importantly the validation profile registry, built from stored
//! `StructureDefinition`s. A write served by one instance updates only that
//! instance's cache; with N instances behind a load balancer the others would
//! keep validating against the old profile set until they restart.
//!
//! The feed closes that gap. The primary database records every write to a
//! watched resource type (a trigger on the resource table, so no write path
//! can forget to), and each instance polls the feed and re-applies the
//! changed resources to its own caches.
//!
//! # Completeness
//!
//! Rows are ordered and consumed by the *writing transaction's id*, never by
//! insertion sequence: a sequence number is taken at insert time, so a slow
//! transaction can commit a lower number after a reader already moved past
//! it. A transaction id below the snapshot's `xmin` belongs to a transaction
//! that has finished, so once a reader has consumed every visible row below
//! `xmin` no further row below it can appear. Rows of still-running
//! transactions that are already visible are processed early as well
//! (consumers re-read the resource's current state, so processing is
//! idempotent) and deduplicated until the cursor passes them.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;

use crate::error::StorageResult;

/// One recorded write to a watched resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceChange {
    /// Tenant that owns the resource.
    pub tenant_id: String,
    /// Resource type (`StructureDefinition`, …).
    pub resource_type: String,
    /// Logical id.
    pub resource_id: String,
}

/// A reader's position in the feed. Opaque to callers; obtained from
/// [`ClusterChangeFeed::start`] and advanced by [`ClusterChangeFeed::poll`].
#[derive(Debug, Clone, Default)]
pub struct ChangeCursor {
    /// Every transaction id below this has been fully consumed.
    pub low_xid: u64,
    /// Feed rows (by sequence) already delivered whose transaction id is not
    /// yet below `low_xid`, mapped to that transaction id.
    pub delivered: HashMap<i64, u64>,
}

impl ChangeCursor {
    /// Records `seq` (written by transaction `xid`) as delivered; returns
    /// `false` when it already was.
    pub fn mark(&mut self, seq: i64, xid: u64) -> bool {
        if xid < self.low_xid {
            return false;
        }
        self.delivered.insert(seq, xid).is_none()
    }

    /// Moves the low-water mark to `xmin` and forgets deliveries below it.
    pub fn advance(&mut self, xmin: u64) {
        if xmin > self.low_xid {
            self.low_xid = xmin;
        }
        let low = self.low_xid;
        self.delivered.retain(|_, xid| *xid >= low);
    }
}

/// The feed of writes to watched resource types.
#[async_trait]
pub trait ClusterChangeFeed: Send + Sync {
    /// A cursor positioned at "now": changes committed before this call are
    /// not returned. Take it *before* a full reload of the cache it feeds, so
    /// nothing committed between the reload's read and the first poll is
    /// lost.
    async fn start(&self) -> StorageResult<ChangeCursor>;

    /// Changes not yet delivered to this cursor, oldest first, and advances it.
    async fn poll(&self, cursor: &mut ChangeCursor) -> StorageResult<Vec<ResourceChange>>;

    /// Deletes feed rows older than `older_than` (any instance may call it;
    /// idempotent). Returns how many rows were removed.
    async fn prune(&self, older_than: Duration) -> StorageResult<u64>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_dedupes_until_the_low_water_mark_passes() {
        let mut cursor = ChangeCursor {
            low_xid: 100,
            ..Default::default()
        };
        assert!(cursor.mark(1, 105), "first delivery");
        assert!(
            !cursor.mark(1, 105),
            "redelivery of a running transaction's row"
        );
        assert!(
            !cursor.mark(2, 99),
            "a row below the mark was consumed already"
        );
        cursor.advance(106);
        assert!(
            cursor.delivered.is_empty(),
            "rows below the new mark are forgotten"
        );
        cursor.advance(50);
        assert_eq!(cursor.low_xid, 106, "the mark never moves back");
    }
}
