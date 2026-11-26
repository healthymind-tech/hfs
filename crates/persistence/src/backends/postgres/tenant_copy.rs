//! Set-based copy of one tenant's data into another tenant, inside
//! PostgreSQL (FHIR server manager P6: the `storage-copy` initial load and
//! endpoint duplication, design §9.2 / D-15).
//!
//! One transaction copies, with `tenant_id` rewritten:
//!
//! * `resources` (current versions, deleted tombstones included),
//! * `resource_history` (every version, or only the rows of the current
//!   versions so `vread` / `_history` of the copy are consistent),
//! * `search_index` and `resource_fts` (so search works on the copy without
//!   a reindex),
//! * optionally the IG package registry (`hfs_packages`,
//!   `hfs_package_resources`),
//! * the `tenants` registry row.
//!
//! Column lists are read from `information_schema` at run time, so columns
//! added by later migrations are carried along without touching this file.
//! The SearchParameter epoch of the target tenant is bumped by the existing
//! row trigger on `resources`; the in-process SearchParameter cache is
//! reloaded after the commit.

use std::collections::BTreeMap;

use crate::error::{BackendError, StorageError, StorageResult};

use super::PostgresBackend;

/// What to copy.
#[derive(Debug, Clone, Default)]
pub struct TenantCopyOptions {
    /// Copy every history version (otherwise only current versions).
    pub include_history: bool,
    /// Only these resource types (`None` = all).
    pub resource_types: Option<Vec<String>>,
    /// Copy the IG package registry rows.
    pub copy_packages: bool,
    /// Refuse when the target tenant already holds resources.
    pub require_empty_target: bool,
}

/// Rows copied per table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TenantCopyReport {
    /// Table name → rows inserted.
    pub rows: BTreeMap<String, u64>,
}

impl TenantCopyReport {
    /// Rows copied into `resources`.
    pub fn resources(&self) -> u64 {
        self.rows.get("resources").copied().unwrap_or(0)
    }
}

fn err(message: String) -> StorageError {
    StorageError::Backend(BackendError::Internal {
        backend_name: "postgres".to_string(),
        message,
        source: None,
    })
}

/// Quotes an identifier read from the catalog.
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

impl PostgresBackend {
    /// Copies tenant `source` into tenant `target` (see the module docs).
    pub async fn copy_tenant(
        &self,
        source: &str,
        target: &str,
        options: &TenantCopyOptions,
    ) -> StorageResult<TenantCopyReport> {
        if source == target {
            return Err(err("source and target tenant are the same".into()));
        }
        let mut client = self
            .pool()
            .get()
            .await
            .map_err(|e| err(format!("tenant copy: no connection: {e}")))?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| err(format!("tenant copy: begin: {e}")))?;
        // A copy of a large tenant legitimately runs longer than the request
        // statement timeout the pool sets.
        tx.batch_execute("SET LOCAL statement_timeout = 0")
            .await
            .map_err(|e| err(format!("tenant copy: {e}")))?;

        if options.require_empty_target {
            let busy: bool = tx
                .query_one(
                    "SELECT EXISTS (SELECT 1 FROM resources WHERE tenant_id = $1)",
                    &[&target],
                )
                .await
                .map_err(|e| err(format!("tenant copy: {e}")))?
                .get(0);
            if busy {
                return Err(err(format!(
                    "target tenant {target:?} already holds resources"
                )));
            }
        }

        let types: Option<Vec<String>> = options.resource_types.clone();
        let mut report = TenantCopyReport::default();

        // (table, excluded columns, extra predicate)
        let history_filter = if options.include_history {
            String::new()
        } else {
            " AND (s.resource_type, s.id, s.version_id) IN \
             (SELECT r.resource_type, r.id, r.version_id FROM resources r WHERE r.tenant_id = $1)"
                .to_string()
        };
        let mut plan: Vec<(&str, &[&str], String, bool)> = vec![
            ("resources", &[], String::new(), true),
            ("resource_history", &[], history_filter, true),
            ("search_index", &["id"], String::new(), true),
            ("resource_fts", &[], String::new(), true),
        ];
        if options.copy_packages {
            plan.push(("hfs_packages", &[], String::new(), false));
            plan.push(("hfs_package_resources", &[], String::new(), true));
        }

        for (table, excluded, extra, typed) in plan {
            let exists: bool = tx
                .query_one(
                    "SELECT to_regclass(format('%I.%I', current_schema(), $1::text)) IS NOT NULL",
                    &[&table],
                )
                .await
                .map_err(|e| err(format!("tenant copy: {e}")))?
                .get(0);
            if !exists {
                continue;
            }
            let columns: Vec<String> = tx
                .query(
                    "SELECT column_name::text FROM information_schema.columns
                      WHERE table_schema = current_schema() AND table_name = $1
                        AND is_generated = 'NEVER'
                      ORDER BY ordinal_position",
                    &[&table],
                )
                .await
                .map_err(|e| err(format!("tenant copy: columns of {table}: {e}")))?
                .iter()
                .map(|r| r.get::<_, String>(0))
                .filter(|c| !excluded.contains(&c.as_str()))
                .collect();
            if !columns.iter().any(|c| c == "tenant_id") {
                continue;
            }
            let insert_cols = columns
                .iter()
                .map(|c| ident(c))
                .collect::<Vec<_>>()
                .join(", ");
            let select_cols = columns
                .iter()
                .map(|c| {
                    if c == "tenant_id" {
                        "$2::text".to_string()
                    } else {
                        format!("s.{}", ident(c))
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            let type_filter = if typed && columns.iter().any(|c| c == "resource_type") {
                " AND ($3::text[] IS NULL OR s.resource_type = ANY($3::text[]))"
            } else {
                " AND $3::text[] IS NOT DISTINCT FROM $3::text[]"
            };
            let sql = format!(
                "INSERT INTO {t} ({insert_cols}) SELECT {select_cols} FROM {t} s \
                 WHERE s.tenant_id = $1{type_filter}{extra} ON CONFLICT DO NOTHING",
                t = ident(table)
            );
            let n = tx
                .execute(&sql, &[&source, &target, &types])
                .await
                .map_err(|e| err(format!("tenant copy: {table}: {e}")))?;
            report.rows.insert(table.to_string(), n);
        }

        // hm-0040: the copied index rows have not been checked against the
        // target's resources, so the target's "verified" markers go; its
        // counts use the resources join until the next verification.
        let markers: bool = tx
            .query_one(
                "SELECT to_regclass(format('%I.%I', current_schema(), 'hfs_search_index_liveness')) IS NOT NULL",
                &[],
            )
            .await
            .map_err(|e| err(format!("tenant copy: {e}")))?
            .get(0);
        if markers {
            tx.execute(
                "DELETE FROM hfs_search_index_liveness WHERE tenant_id = $1",
                &[&target],
            )
            .await
            .map_err(|e| err(format!("tenant copy: liveness markers: {e}")))?;
        }

        // The tenant registry row (display name carried over when present).
        let registered = tx
            .query_one(
                "SELECT to_regclass(format('%I.%I', current_schema(), 'tenants')) IS NOT NULL",
                &[],
            )
            .await
            .map_err(|e| err(format!("tenant copy: {e}")))?
            .get::<_, bool>(0);
        if registered {
            tx.execute(
                "INSERT INTO tenants (id) VALUES ($1) ON CONFLICT (id) DO NOTHING",
                &[&target],
            )
            .await
            .map_err(|e| err(format!("tenant copy: register tenant: {e}")))?;
        }

        tx.commit()
            .await
            .map_err(|e| err(format!("tenant copy: commit: {e}")))?;
        drop(client);
        self.forget_search_index_liveness(target);
        if let Err(e) = self.reload_stored_cache().await {
            tracing::warn!("SearchParameter cache reload after tenant copy failed: {e}");
        }
        tracing::info!(source, target, rows = ?report.rows, "tenant copied");
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_quoted() {
        assert_eq!(ident("tenant_id"), "\"tenant_id\"");
        assert_eq!(ident("we\"ird"), "\"we\"\"ird\"");
    }

    #[test]
    fn report_counts_resources() {
        let mut r = TenantCopyReport::default();
        assert_eq!(r.resources(), 0);
        r.rows.insert("resources".into(), 7);
        assert_eq!(r.resources(), 7);
    }
}
