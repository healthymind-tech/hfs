//! [`RetentionStorage`] for PostgreSQL: age-based expiry of append-only
//! resources, in bounded batches so a large backlog never holds one long
//! transaction.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;

use super::PostgresBackend;
use crate::core::retention::{ExpiredResource, RetentionStorage};
use crate::error::{BackendError, StorageError, StorageResult};

fn internal_error(message: String) -> StorageError {
    StorageError::Backend(BackendError::Internal {
        backend_name: "postgres".to_string(),
        message,
        source: None,
    })
}

fn checked_type(resource_type: &str) -> StorageResult<&str> {
    if !resource_type.is_empty() && resource_type.chars().all(|c| c.is_ascii_alphanumeric()) {
        Ok(resource_type)
    } else {
        Err(internal_error(format!(
            "invalid resource type for retention: {resource_type:?}"
        )))
    }
}

#[async_trait]
impl RetentionStorage for PostgresBackend {
    async fn expired_resources(
        &self,
        resource_type: &str,
        cutoff: DateTime<Utc>,
        limit: usize,
    ) -> StorageResult<Vec<ExpiredResource>> {
        let rt = checked_type(resource_type)?;
        let client = self.get_client().await?;
        // A partial index keeps the oldest-first scan cheap without taxing
        // writes of any other resource type.
        client
            .batch_execute(&format!(
                "CREATE INDEX IF NOT EXISTS idx_resources_expiry_{lower} \
                 ON resources(last_updated) WHERE resource_type = '{rt}'",
                lower = rt.to_ascii_lowercase()
            ))
            .await
            .map_err(|e| internal_error(format!("retention index: {e}")))?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = client
            .query(
                "SELECT tenant_id, id, last_updated, data FROM resources
                 WHERE resource_type = $1 AND last_updated < $2
                 ORDER BY last_updated ASC LIMIT $3",
                &[&rt, &cutoff, &limit],
            )
            .await
            .map_err(|e| internal_error(format!("retention select: {e}")))?;
        Ok(rows
            .into_iter()
            .map(|row| ExpiredResource {
                tenant_id: row.get(0),
                id: row.get(1),
                last_updated: row.get(2),
                content: row.get::<_, Value>(3),
            })
            .collect())
    }

    async fn purge_expired(
        &self,
        resource_type: &str,
        keys: &[(String, String)],
    ) -> StorageResult<u64> {
        if keys.is_empty() {
            return Ok(0);
        }
        let rt = checked_type(resource_type)?;
        let tenants: Vec<&str> = keys.iter().map(|(t, _)| t.as_str()).collect();
        let ids: Vec<&str> = keys.iter().map(|(_, i)| i.as_str()).collect();
        let mut client = self.get_client().await?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| internal_error(format!("retention begin: {e}")))?;
        let has_fts: bool = tx
            .query_one("SELECT to_regclass('resource_fts') IS NOT NULL", &[])
            .await
            .map(|r| r.get(0))
            .unwrap_or(false);
        // `search_index` and `resource_fts` have no foreign key to `resources`
        // (nothing cascades), so every table is cleared explicitly.
        let mut statements = vec![
            "DELETE FROM search_index s USING unnest($2::text[], $3::text[]) AS k(t, i)
             WHERE s.tenant_id = k.t AND s.resource_type = $1 AND s.resource_id = k.i",
            "DELETE FROM resource_history h USING unnest($2::text[], $3::text[]) AS k(t, i)
             WHERE h.tenant_id = k.t AND h.resource_type = $1 AND h.id = k.i",
        ];
        if has_fts {
            statements.push(
                "DELETE FROM resource_fts f USING unnest($2::text[], $3::text[]) AS k(t, i)
                 WHERE f.tenant_id = k.t AND f.resource_type = $1 AND f.resource_id = k.i",
            );
        }
        for sql in statements {
            tx.execute(sql, &[&rt, &tenants, &ids])
                .await
                .map_err(|e| internal_error(format!("retention delete: {e}")))?;
        }
        let removed = tx
            .execute(
                "DELETE FROM resources r USING unnest($2::text[], $3::text[]) AS k(t, i)
                 WHERE r.tenant_id = k.t AND r.resource_type = $1 AND r.id = k.i",
                &[&rt, &tenants, &ids],
            )
            .await
            .map_err(|e| internal_error(format!("retention delete: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| internal_error(format!("retention commit: {e}")))?;
        Ok(removed)
    }
}
