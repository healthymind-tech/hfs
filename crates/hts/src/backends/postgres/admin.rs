//! PostgreSQL implementation of [`TerminologyAdmin`].
//!
//! Mirrors `backends::sqlite::admin`. Differences that follow from the schema:
//! `concept_closure` has a foreign key here, so the cascade from `concepts`
//! / `code_systems` covers it; there are no FTS shadow tables (text search is a
//! trigram GIN index on `concepts.display`, maintained by Postgres); and
//! "compact" is `VACUUM (ANALYZE)` rather than a file rewrite.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;

use super::{PostgresTerminologyBackend, error_chain};
use crate::admin::ops::{
    CodeSystemRow, DbFootprint, DeleteReport, DependentRef, Dependents, OpProgress, SystemStats,
    TerminologyAdmin, compose_references, effective_current,
};
use crate::error::HtsError;
use crate::traits::TerminologyCaches;

const ROW_SELECT: &str = "SELECT id, url, version, name, title, status, content, \
     CASE WHEN content = 'supplement' THEN resource_json->>'supplements' END, \
     COALESCE(current_pin, 0), created_at, updated_at FROM code_systems";

fn pg(e: tokio_postgres::Error) -> HtsError {
    HtsError::StorageError(error_chain(&e))
}

fn row_of(r: &tokio_postgres::Row) -> CodeSystemRow {
    CodeSystemRow {
        id: r.get(0),
        url: r.get(1),
        version: r.get(2),
        name: r.get(3),
        title: r.get(4),
        status: r.get(5),
        content: r.get(6),
        supplements: r.get(7),
        pinned: r.get::<_, i32>(8) == 1,
        created_at: r.get(9),
        updated_at: r.get(10),
    }
}

impl PostgresTerminologyBackend {
    async fn client(&self) -> Result<deadpool_postgres::Object, HtsError> {
        self.pool
            .get()
            .await
            .map_err(|e| HtsError::StorageError(format!("Pool error: {}", error_chain(&e))))
    }
}

async fn counts(
    client: &deadpool_postgres::Object,
    sql: &str,
    out: &mut HashMap<String, SystemStats>,
    set: fn(&mut SystemStats, u64),
) -> Result<(), HtsError> {
    for r in client.query(sql, &[]).await.map_err(pg)? {
        let id: String = r.get(0);
        let n: i64 = r.get(1);
        set(out.entry(id).or_default(), n.max(0) as u64);
    }
    Ok(())
}

#[async_trait]
impl TerminologyAdmin for PostgresTerminologyBackend {
    fn kind(&self) -> &'static str {
        "postgres"
    }

    async fn code_system_rows(&self) -> Result<Vec<CodeSystemRow>, HtsError> {
        let c = self.client().await?;
        let rows = c
            .query(
                &format!("{ROW_SELECT} ORDER BY url, version_sort DESC"),
                &[],
            )
            .await
            .map_err(pg)?;
        Ok(rows.iter().map(row_of).collect())
    }

    async fn resource_counts(&self) -> Result<(u64, u64), HtsError> {
        let c = self.client().await?;
        let vs: i64 = c
            .query_one("SELECT COUNT(*) FROM value_sets", &[])
            .await
            .map_err(pg)?
            .get(0);
        let cm: i64 = c
            .query_one("SELECT COUNT(*) FROM concept_maps", &[])
            .await
            .map_err(pg)?
            .get(0);
        Ok((vs.max(0) as u64, cm.max(0) as u64))
    }

    async fn system_stats(&self) -> Result<HashMap<String, SystemStats>, HtsError> {
        let c = self.client().await?;
        let mut out = HashMap::new();
        counts(
            &c,
            "SELECT system_id, COUNT(*) FROM concepts GROUP BY system_id",
            &mut out,
            |s, n| s.concepts = n,
        )
        .await?;
        counts(
            &c,
            "SELECT c.system_id, COUNT(*) FROM concept_designations d \
             JOIN concepts c ON c.id = d.concept_id GROUP BY c.system_id",
            &mut out,
            |s, n| s.designations = n,
        )
        .await?;
        counts(
            &c,
            "SELECT c.system_id, COUNT(*) FROM concept_properties p \
             JOIN concepts c ON c.id = p.concept_id GROUP BY c.system_id",
            &mut out,
            |s, n| s.properties = n,
        )
        .await?;
        counts(
            &c,
            "SELECT system_id, COUNT(*) FROM concept_hierarchy GROUP BY system_id",
            &mut out,
            |s, n| s.hierarchy = n,
        )
        .await?;
        counts(
            &c,
            "SELECT system_id, COUNT(*) FROM concept_closure GROUP BY system_id",
            &mut out,
            |s, n| s.closure = n,
        )
        .await?;
        for s in out.values_mut() {
            s.estimate_bytes();
        }
        Ok(out)
    }

    async fn footprint(&self) -> Result<DbFootprint, HtsError> {
        let c = self.client().await?;
        let bytes: i64 = c
            .query_one("SELECT pg_database_size(current_database())", &[])
            .await
            .map_err(pg)?
            .get(0);
        // Space held by dead row versions, which VACUUM (the `compact`
        // job) makes reusable — PostgreSQL's analogue of SQLite's free list
        // (estimated from the statistics collector's live/dead counts).
        let reclaimable: i64 = c
            .query_one(
                "SELECT COALESCE(SUM(pg_relation_size(relid) * n_dead_tup::float8
                                     / NULLIF(n_live_tup + n_dead_tup, 0)), 0)::bigint
                   FROM pg_stat_user_tables",
                &[],
            )
            .await
            .map_err(pg)?
            .get(0);
        Ok(DbFootprint {
            bytes: bytes.max(0) as u64,
            reclaimable_bytes: reclaimable.max(0) as u64,
            ..Default::default()
        })
    }

    async fn find_code_system(
        &self,
        url: &str,
        version: Option<&str>,
    ) -> Result<Option<CodeSystemRow>, HtsError> {
        let c = self.client().await?;
        let row = c
            .query_opt(
                &format!(
                    "{ROW_SELECT} WHERE url = $1 AND COALESCE(version, '') = COALESCE($2, '')"
                ),
                &[&url, &version],
            )
            .await
            .map_err(pg)?;
        Ok(row.as_ref().map(row_of))
    }

    async fn dependents(&self, url: &str, version: Option<&str>) -> Result<Dependents, HtsError> {
        let c = self.client().await?;
        let mut deps = Dependents::default();
        for r in c
            .query(
                "SELECT id, url, version, compose_json FROM value_sets \
                 WHERE compose_json IS NOT NULL AND strpos(compose_json, $1) > 0",
                &[&url],
            )
            .await
            .map_err(pg)?
        {
            let compose: String = r.get(3);
            if let Some(pins) = compose_references(&compose, url, version) {
                deps.value_sets.push(DependentRef {
                    resource_type: "ValueSet".into(),
                    id: r.get(0),
                    url: r.get(1),
                    version: r.get(2),
                    relation: "compose.include".into(),
                    pins_version: pins,
                });
            }
        }
        for r in c
            .query(&format!("{ROW_SELECT} WHERE content = 'supplement'"), &[])
            .await
            .map_err(pg)?
        {
            let sup = row_of(&r);
            let Some(target) = sup.supplements.as_deref() else {
                continue;
            };
            let (t_url, t_ver) = match target.split_once('|') {
                Some((u, v)) => (u, Some(v)),
                None => (target, None),
            };
            if t_url != url || matches!((t_ver, version), (Some(a), Some(b)) if a != b) {
                continue;
            }
            deps.supplements.push(DependentRef {
                resource_type: "CodeSystem".into(),
                id: sup.id.clone(),
                url: sup.url.clone(),
                version: sup.version.clone(),
                relation: "supplements".into(),
                pins_version: t_ver.is_some(),
            });
        }
        let mut seen = HashSet::new();
        for r in c
            .query(
                "SELECT id, url, version FROM concept_maps \
                 WHERE source_uri = $1 OR target_uri = $1 \
                    OR id IN (SELECT DISTINCT map_id FROM concept_map_elements \
                              WHERE source_system = $1 OR target_system = $1)",
                &[&url],
            )
            .await
            .map_err(pg)?
        {
            let id: String = r.get(0);
            if seen.insert(id.clone()) {
                deps.concept_maps.push(DependentRef {
                    resource_type: "ConceptMap".into(),
                    id,
                    url: r.get(1),
                    version: r.get(2),
                    relation: "group.source/target".into(),
                    pins_version: false,
                });
            }
        }
        let siblings: Vec<CodeSystemRow> = c
            .query(&format!("{ROW_SELECT} WHERE url = $1"), &[&url])
            .await
            .map_err(pg)?
            .iter()
            .map(row_of)
            .filter(|r| r.version.as_deref() != version)
            .collect();
        let mut populated = HashMap::new();
        for s in &siblings {
            let has: bool = c
                .query_one(
                    "SELECT EXISTS(SELECT 1 FROM concepts WHERE system_id = $1)",
                    &[&s.id],
                )
                .await
                .map_err(pg)?
                .get(0);
            if has {
                populated.insert(
                    s.id.clone(),
                    SystemStats {
                        concepts: 1,
                        ..Default::default()
                    },
                );
            }
        }
        let refs: Vec<&CodeSystemRow> = siblings.iter().collect();
        deps.current_after_delete = effective_current(&refs, &populated).map(|r| r.version.clone());
        deps.other_versions = siblings.iter().map(|r| r.version.clone()).collect();
        Ok(deps)
    }

    async fn delete_code_system(
        &self,
        row: &CodeSystemRow,
        progress: Arc<OpProgress>,
    ) -> Result<DeleteReport, HtsError> {
        let c = self.client().await?;
        let mut report = DeleteReport::default();
        progress.set_phase("concepts");
        let total: i64 = c
            .query_one(
                "SELECT COUNT(*) FROM concepts WHERE system_id = $1",
                &[&row.id],
            )
            .await
            .map_err(pg)?
            .get(0);
        progress.set_total(total.max(0) as u64);
        loop {
            // Closure and hierarchy cascade from code_systems only, so delete
            // concepts in batches (cascading designations/properties) and let
            // the final row delete take the rest.
            let n = c
                .execute(
                    "DELETE FROM concepts WHERE id IN \
                     (SELECT id FROM concepts WHERE system_id = $1 LIMIT 5000)",
                    &[&row.id],
                )
                .await
                .map_err(pg)?;
            report.concepts += n;
            progress.add(n);
            if n == 0 {
                break;
            }
        }
        // Closure and hierarchy in bounded batches with progress, as SQLite
        // does: SNOMED CT's ~10 M closure rows in one statement is one huge
        // transaction with no progress and no cancellation point.
        for (phase, table) in [
            ("closure", "concept_closure"),
            ("hierarchy", "concept_hierarchy"),
        ] {
            progress.set_phase(phase);
            let total: i64 = c
                .query_one(
                    &format!("SELECT COUNT(*) FROM {table} WHERE system_id = $1"),
                    &[&row.id],
                )
                .await
                .map_err(pg)?
                .get(0);
            progress.set_total(total.max(0) as u64);
            let sql = format!(
                "DELETE FROM {table} WHERE ctid = ANY(ARRAY(
                     SELECT ctid FROM {table} WHERE system_id = $1 LIMIT 50000))"
            );
            loop {
                let n = c.execute(&sql, &[&row.id]).await.map_err(pg)?;
                if phase == "closure" {
                    report.closure += n;
                } else {
                    report.hierarchy += n;
                }
                progress.add(n);
                if n == 0 {
                    break;
                }
            }
        }
        progress.set_phase("caches");
        report.expansion_cache = c
            .execute(
                "DELETE FROM value_set_expansions WHERE system_url = $1",
                &[&row.url],
            )
            .await
            .map_err(pg)?;
        let deleted = c
            .execute("DELETE FROM code_systems WHERE id = $1", &[&row.id])
            .await
            .map_err(pg);
        // Rows may already be gone even when a later step failed: drop the
        // caches either way (as SQLite does).
        self.invalidate_caches();
        deleted?;
        Ok(report)
    }

    async fn delete_value_set(&self, url: &str, version: Option<&str>) -> Result<u64, HtsError> {
        let c = self.client().await?;
        let n = c
            .execute(
                "DELETE FROM value_sets \
                 WHERE url = $1 AND COALESCE(version, '') = COALESCE($2, '')",
                &[&url, &version],
            )
            .await
            .map_err(pg)?;
        self.invalidate_caches();
        Ok(n)
    }

    async fn delete_concept_map(&self, url: &str) -> Result<u64, HtsError> {
        let c = self.client().await?;
        let n = c
            .execute("DELETE FROM concept_maps WHERE url = $1", &[&url])
            .await
            .map_err(pg)?;
        self.invalidate_caches();
        Ok(n)
    }

    async fn set_current_version(
        &self,
        url: &str,
        version: Option<Option<&str>>,
    ) -> Result<(), HtsError> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(pg)?;
        if let Some(v) = version {
            let exists: bool = tx
                .query_one(
                    "SELECT EXISTS(SELECT 1 FROM code_systems \
                     WHERE url = $1 AND COALESCE(version, '') = COALESCE($2, ''))",
                    &[&url, &v],
                )
                .await
                .map_err(pg)?
                .get(0);
            if !exists {
                return Err(HtsError::NotFound(format!(
                    "CodeSystem {url}|{}",
                    v.unwrap_or("")
                )));
            }
        }
        tx.execute(
            "UPDATE code_systems SET current_pin = NULL \
             WHERE url = $1 AND current_pin IS NOT NULL",
            &[&url],
        )
        .await
        .map_err(pg)?;
        if let Some(v) = version {
            tx.execute(
                "UPDATE code_systems SET current_pin = 1 \
                 WHERE url = $1 AND COALESCE(version, '') = COALESCE($2, '')",
                &[&url, &v],
            )
            .await
            .map_err(pg)?;
        }
        // The pin decides the default version, which reports the plain id.
        let declared: Vec<String> = tx
            .query(
                "SELECT DISTINCT resource_json->>'id' FROM code_systems \
                 WHERE url = $1 AND resource_json->>'id' IS NOT NULL",
                &[&url],
            )
            .await
            .map_err(pg)?
            .into_iter()
            .map(|r| r.get(0))
            .collect();
        super::fhir_ids::reassign(&*tx, "code_systems", &declared).await?;
        tx.commit().await.map_err(pg)?;
        self.invalidate_caches();
        Ok(())
    }

    async fn finalize_step(&self, step: &str) -> Result<(), HtsError> {
        match step {
            "closure" => self.rebuild_missing_closures().await?,
            "analyze" => {
                let c = self.client().await?;
                c.batch_execute("ANALYZE").await.map_err(pg)?;
            }
            _ => {}
        }
        self.invalidate_caches();
        Ok(())
    }

    async fn compact(&self) -> Result<(), HtsError> {
        let c = self.client().await?;
        c.batch_execute("VACUUM (ANALYZE)").await.map_err(pg)
    }
}
