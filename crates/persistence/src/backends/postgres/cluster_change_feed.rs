//! [`ClusterChangeFeed`] over the `resource_change_feed` table (fork migration `hm-0004-resource-change-feed`).
//!
//! See [`crate::core::cluster_change_feed`] for the completeness argument.
//! `txid` is `xid8` (64-bit, wraparound-free); it travels as text because
//! `tokio-postgres` has no `xid8` mapping.

use std::time::Duration;

use async_trait::async_trait;

use crate::core::cluster_change_feed::{ChangeCursor, ClusterChangeFeed, ResourceChange};
use crate::error::{BackendError, StorageError, StorageResult};

/// Pool-sharing change-feed handle for PostgreSQL.
pub struct PgClusterChangeFeed {
    pool: deadpool_postgres::Pool,
}

impl PgClusterChangeFeed {
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

fn parse_xid(text: &str) -> StorageResult<u64> {
    text.parse::<u64>()
        .map_err(|e| internal(format!("unparseable transaction id {text:?}: {e}")))
}

#[async_trait]
impl ClusterChangeFeed for PgClusterChangeFeed {
    async fn start(&self) -> StorageResult<ChangeCursor> {
        let client = self.client().await?;
        let row = client
            .query_one("SELECT pg_snapshot_xmin(pg_current_snapshot())::text", &[])
            .await
            .map_err(|e| internal(format!("read snapshot xmin: {e}")))?;
        let xmin = parse_xid(&row.get::<_, String>(0))?;
        Ok(ChangeCursor {
            low_xid: xmin,
            ..Default::default()
        })
    }

    async fn poll(&self, cursor: &mut ChangeCursor) -> StorageResult<Vec<ResourceChange>> {
        let client = self.client().await?;
        // One statement, one snapshot: the xmin and the rows agree.
        let rows = client
            .query(
                "WITH s AS (SELECT pg_snapshot_xmin(pg_current_snapshot())::text AS xmin)
                 SELECT s.xmin, f.seq, f.txid::text, f.tenant_id, f.resource_type, f.resource_id
                 FROM s
                 LEFT JOIN resource_change_feed f ON f.txid >= ($1::text)::xid8
                 ORDER BY f.seq NULLS FIRST",
                &[&cursor.low_xid.to_string()],
            )
            .await
            .map_err(|e| internal(format!("poll resource_change_feed: {e}")))?;
        let mut xmin = cursor.low_xid;
        let mut changes = Vec::new();
        for row in rows {
            xmin = parse_xid(&row.get::<_, String>(0))?;
            let Some(seq) = row.get::<_, Option<i64>>(1) else {
                continue;
            };
            let xid = parse_xid(&row.get::<_, String>(2))?;
            if cursor.mark(seq, xid) {
                changes.push(ResourceChange {
                    tenant_id: row.get(3),
                    resource_type: row.get(4),
                    resource_id: row.get(5),
                });
            }
        }
        cursor.advance(xmin);
        Ok(changes)
    }

    async fn prune(&self, older_than: Duration) -> StorageResult<u64> {
        let client = self.client().await?;
        client
            .execute(
                "DELETE FROM resource_change_feed
                 WHERE changed_at < NOW() - $1 * INTERVAL '1 second'",
                &[&older_than.as_secs_f64()],
            )
            .await
            .map_err(|e| internal(format!("prune resource_change_feed: {e}")))
    }
}
