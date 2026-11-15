//! [`RetentionStorage`] for SQLite: age-based expiry of append-only resources.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::params;

use super::SqliteBackend;
use super::storage::fts_table_exists;
use crate::core::retention::{ExpiredResource, RetentionStorage};
use crate::error::{BackendError, StorageError, StorageResult};

fn internal_error(message: String) -> StorageError {
    StorageError::Backend(BackendError::Internal {
        backend_name: "sqlite".to_string(),
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
impl RetentionStorage for SqliteBackend {
    async fn expired_resources(
        &self,
        resource_type: &str,
        cutoff: DateTime<Utc>,
        limit: usize,
    ) -> StorageResult<Vec<ExpiredResource>> {
        let rt = checked_type(resource_type)?.to_string();
        let index = format!(
            "CREATE INDEX IF NOT EXISTS idx_resources_expiry_{rt} \
             ON resources(last_updated) WHERE resource_type = '{rt}'"
        );
        let cutoff = cutoff.to_rfc3339();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run_blocking(move |conn| {
            conn.execute(&index, [])
                .map_err(|e| internal_error(format!("retention index: {e}")))?;
            let mut stmt = conn
                .prepare(
                    "SELECT tenant_id, id, last_updated, data FROM resources
                     WHERE resource_type = ?1 AND last_updated < ?2
                     ORDER BY last_updated ASC LIMIT ?3",
                )
                .map_err(|e| internal_error(format!("retention select: {e}")))?;
            let rows = stmt
                .query_map(params![rt, cutoff, limit], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                })
                .map_err(|e| internal_error(format!("retention select: {e}")))?;
            let mut out = Vec::new();
            for row in rows {
                let (tenant_id, id, last_updated, data) =
                    row.map_err(|e| internal_error(format!("retention row: {e}")))?;
                let last_updated = DateTime::parse_from_rfc3339(&last_updated)
                    .map(|d| d.with_timezone(&Utc))
                    .map_err(|e| internal_error(format!("retention last_updated: {e}")))?;
                let content = serde_json::from_slice(&data).unwrap_or(serde_json::Value::Null);
                out.push(ExpiredResource {
                    tenant_id,
                    id,
                    last_updated,
                    content,
                });
            }
            Ok(out)
        })
        .await
    }

    async fn purge_expired(
        &self,
        resource_type: &str,
        keys: &[(String, String)],
    ) -> StorageResult<u64> {
        if keys.is_empty() {
            return Ok(0);
        }
        let rt = checked_type(resource_type)?.to_string();
        let keys = keys.to_vec();
        let _write = self.write_permit().await?;
        self.run_blocking(move |conn| {
            let has_fts = fts_table_exists(conn)?;
            conn.execute_batch("BEGIN IMMEDIATE")
                .map_err(|e| internal_error(format!("retention begin: {e}")))?;
            let result = (|| -> StorageResult<u64> {
                let mut removed = 0u64;
                for (tenant_id, id) in &keys {
                    let p = params![tenant_id, rt, id];
                    removed += conn
                        .execute(
                            "DELETE FROM resources WHERE tenant_id = ?1 AND resource_type = ?2 AND id = ?3",
                            p,
                        )
                        .map_err(|e| internal_error(format!("retention delete: {e}")))?
                        as u64;
                    for sql in [
                        "DELETE FROM resource_history WHERE tenant_id = ?1 AND resource_type = ?2 AND id = ?3",
                        "DELETE FROM search_index WHERE tenant_id = ?1 AND resource_type = ?2 AND resource_id = ?3",
                    ] {
                        conn.execute(sql, params![tenant_id, rt, id])
                            .map_err(|e| internal_error(format!("retention delete: {e}")))?;
                    }
                    if has_fts {
                        conn.execute(
                            "DELETE FROM resource_fts WHERE tenant_id = ?1 AND resource_type = ?2 AND resource_id = ?3",
                            params![tenant_id, rt, id],
                        )
                        .map_err(|e| internal_error(format!("retention fts delete: {e}")))?;
                        let _ = conn.execute(
                            "DELETE FROM resource_fts_map WHERE tenant_id = ?1 AND resource_type = ?2 AND resource_id = ?3",
                            params![tenant_id, rt, id],
                        );
                    }
                }
                Ok(removed)
            })();
            match result {
                Ok(n) => {
                    conn.execute_batch("COMMIT")
                        .map_err(|e| internal_error(format!("retention commit: {e}")))?;
                    Ok(n)
                }
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    Err(e)
                }
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ResourceStorage;
    use crate::tenant::{TenantContext, TenantId, TenantPermissions};
    use helios_fhir::FhirVersion;

    fn tenant(id: &str) -> TenantContext {
        TenantContext::new(TenantId::new(id), TenantPermissions::full_access())
    }

    fn count(backend: &SqliteBackend, sql: &str) -> i64 {
        backend
            .get_connection()
            .unwrap()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    }

    #[tokio::test]
    async fn expires_old_audit_events_in_every_tenant_and_nothing_else() {
        let backend = SqliteBackend::in_memory().unwrap();
        backend.init_schema().unwrap();
        for (t, id) in [("a", "old-a"), ("b", "old-b"), ("a", "new-a")] {
            backend
                .create(
                    &tenant(t),
                    "AuditEvent",
                    serde_json::json!({"resourceType": "AuditEvent", "id": id}),
                    FhirVersion::default(),
                )
                .await
                .unwrap();
        }
        backend
            .create(
                &tenant("a"),
                "Patient",
                serde_json::json!({"resourceType": "Patient", "id": "old-p"}),
                FhirVersion::default(),
            )
            .await
            .unwrap();
        let long_ago = (Utc::now() - chrono::Duration::days(400)).to_rfc3339();
        backend
            .get_connection()
            .unwrap()
            .execute(
                "UPDATE resources SET last_updated = ?1 WHERE id IN ('old-a', 'old-b', 'old-p')",
                params![long_ago],
            )
            .unwrap();

        let cutoff = Utc::now() - chrono::Duration::days(365);
        let expired = backend
            .expired_resources("AuditEvent", cutoff, 10)
            .await
            .unwrap();
        let mut ids: Vec<_> = expired.iter().map(|e| e.id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["old-a", "old-b"]);
        assert_eq!(expired[0].content["resourceType"], "AuditEvent");

        let keys: Vec<_> = expired
            .iter()
            .map(|e| (e.tenant_id.clone(), e.id.clone()))
            .collect();
        assert_eq!(backend.purge_expired("AuditEvent", &keys).await.unwrap(), 2);

        assert_eq!(
            count(
                &backend,
                "SELECT COUNT(*) FROM resources WHERE resource_type='AuditEvent'"
            ),
            1
        );
        assert_eq!(
            count(
                &backend,
                "SELECT COUNT(*) FROM resource_history WHERE id IN ('old-a','old-b')"
            ),
            0
        );
        assert_eq!(
            count(
                &backend,
                "SELECT COUNT(*) FROM search_index WHERE resource_id IN ('old-a','old-b')"
            ),
            0
        );
        // Other resource types are untouched, however old.
        assert_eq!(
            count(&backend, "SELECT COUNT(*) FROM resources WHERE id='old-p'"),
            1
        );
        assert!(
            backend
                .expired_resources("AuditEvent", cutoff, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn rejects_a_resource_type_that_is_not_an_identifier() {
        let backend = SqliteBackend::in_memory().unwrap();
        backend.init_schema().unwrap();
        assert!(
            backend
                .expired_resources("AuditEvent'; DROP TABLE resources; --", Utc::now(), 1)
                .await
                .is_err()
        );
    }
}
