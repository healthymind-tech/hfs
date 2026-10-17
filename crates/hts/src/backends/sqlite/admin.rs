//! SQLite implementation of [`TerminologyAdmin`].
//!
//! Lives inside `backends::sqlite` because the FTS and in-memory index
//! helpers it drives (`value_set::prebuild_*`) are private to this backend.
//!
//! # Delete strategy
//!
//! A SNOMED CT edition is ~370 k concepts, ~1.5 M designations, a few million
//! role properties and well over ten million closure rows. Deleting that in one
//! statement would hold SQLite's single write lock for minutes and grow the WAL
//! by gigabytes, so [`delete_code_system`] works in bounded batches, each in its
//! own `BEGIN IMMEDIATE` transaction:
//!
//! 1. `closure` — `concept_closure` has no foreign key, so it is deleted
//!    explicitly, 50 000 rows per transaction.
//! 2. `hierarchy` — same, for `concept_hierarchy`.
//! 3. `concepts` — 2 000 concepts per transaction. Their FTS rows are deleted
//!    first *by rowid* (every FTS writer uses the concept id as the rowid, and
//!    `-designation id` for synonym rows in `concepts_search_fts`), which avoids
//!    the full-table scan a `WHERE system_id = ?` on an `UNINDEXED` FTS column
//!    costs; then the concepts themselves, whose `ON DELETE CASCADE` removes
//!    designations and properties through their `concept_id` indexes.
//! 4. `caches` — persisted expansion caches derived from the system's url.
//! 5. the `code_systems` row itself.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{OptionalExtension, TransactionBehavior};

use super::{SqliteTerminologyBackend, schema, value_set};
use crate::admin::ops::{
    CodeSystemRow, DbFootprint, DeleteReport, DependentRef, Dependents, OpProgress, SystemStats,
    TerminologyAdmin, compose_references, effective_current,
};
use crate::error::HtsError;
use crate::traits::TerminologyCaches;

const CLOSURE_BATCH: usize = 50_000;
const CONCEPT_BATCH: usize = 2_000;

fn storage(e: impl std::fmt::Display) -> HtsError {
    HtsError::StorageError(e.to_string())
}

impl SqliteTerminologyBackend {
    async fn blocking<T, F>(&self, f: F) -> Result<T, HtsError>
    where
        T: Send + 'static,
        F: FnOnce(&mut rusqlite::Connection) -> Result<T, HtsError> + Send + 'static,
    {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(storage)?;
            f(&mut conn)
        })
        .await
        .map_err(|e| HtsError::Internal(format!("blocking task failed: {e}")))?
    }
}

fn read_rows(
    conn: &rusqlite::Connection,
    where_sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<CodeSystemRow>, HtsError> {
    let sql = format!(
        "SELECT id, url, version, name, title, status, content, \
                CASE WHEN content = 'supplement' \
                     THEN json_extract(resource_json, '$.supplements') END, \
                COALESCE(current_pin, 0), created_at, updated_at \
         FROM code_systems {where_sql}"
    );
    let mut stmt = conn.prepare(&sql).map_err(storage)?;
    let rows = stmt
        .query_map(params, |r| {
            Ok(CodeSystemRow {
                id: r.get(0)?,
                url: r.get(1)?,
                version: r.get(2)?,
                name: r.get(3)?,
                title: r.get(4)?,
                status: r.get(5)?,
                content: r.get(6)?,
                supplements: r.get(7)?,
                pinned: r.get::<_, i64>(8)? == 1,
                created_at: r.get(9)?,
                updated_at: r.get(10)?,
            })
        })
        .map_err(storage)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage)?;
    Ok(rows)
}

fn count_by_system(
    conn: &rusqlite::Connection,
    sql: &str,
    out: &mut HashMap<String, SystemStats>,
    set: fn(&mut SystemStats, u64),
) -> Result<(), HtsError> {
    let mut stmt = conn.prepare(sql).map_err(storage)?;
    let mut rows = stmt.query([]).map_err(storage)?;
    while let Some(r) = rows.next().map_err(storage)? {
        let id: String = r.get(0).map_err(storage)?;
        let n: i64 = r.get(1).map_err(storage)?;
        set(out.entry(id).or_default(), n.max(0) as u64);
    }
    Ok(())
}

fn id_list(ids: &[i64]) -> String {
    ids.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Delete up to `limit` rows of `table` belonging to `system_id`; returns the
/// number deleted.
fn delete_rowid_batch(
    conn: &mut rusqlite::Connection,
    table: &str,
    system_id: &str,
    limit: usize,
) -> Result<u64, HtsError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage)?;
    let n = tx
        .execute(
            &format!(
                "DELETE FROM {table} WHERE rowid IN \
                 (SELECT rowid FROM {table} WHERE system_id = ?1 LIMIT {limit})"
            ),
            rusqlite::params![system_id],
        )
        .map_err(storage)?;
    tx.commit().map_err(storage)?;
    Ok(n as u64)
}

/// One concept batch: FTS rows by rowid, then the concepts (cascading to
/// designations and properties). Returns `(concepts, fts rows)`.
fn delete_concept_batch(
    conn: &mut rusqlite::Connection,
    system_id: &str,
) -> Result<(u64, u64), HtsError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage)?;
    let ids: Vec<i64> = {
        let mut stmt = tx
            .prepare(&format!(
                "SELECT id FROM concepts WHERE system_id = ?1 LIMIT {CONCEPT_BATCH}"
            ))
            .map_err(storage)?;
        stmt.query_map(rusqlite::params![system_id], |r| r.get(0))
            .map_err(storage)?
            .collect::<rusqlite::Result<_>>()
            .map_err(storage)?
    };
    if ids.is_empty() {
        tx.commit().map_err(storage)?;
        return Ok((0, 0));
    }
    let list = id_list(&ids);
    let desig: Vec<i64> = {
        let mut stmt = tx
            .prepare(&format!(
                "SELECT id FROM concept_designations WHERE concept_id IN ({list})"
            ))
            .map_err(storage)?;
        stmt.query_map([], |r| r.get(0))
            .map_err(storage)?
            .collect::<rusqlite::Result<_>>()
            .map_err(storage)?
    };
    let mut fts = 0u64;
    for table in ["concepts_fts", "concepts_word_fts", "concepts_search_fts"] {
        fts += tx
            .execute(&format!("DELETE FROM {table} WHERE rowid IN ({list})"), [])
            .map_err(storage)? as u64;
    }
    for chunk in desig.chunks(5_000) {
        let neg = chunk
            .iter()
            .map(|d| (-d).to_string())
            .collect::<Vec<_>>()
            .join(",");
        fts += tx
            .execute(
                &format!("DELETE FROM concepts_search_fts WHERE rowid IN ({neg})"),
                [],
            )
            .map_err(storage)? as u64;
    }
    let n = tx
        .execute(&format!("DELETE FROM concepts WHERE id IN ({list})"), [])
        .map_err(storage)?;
    tx.commit().map_err(storage)?;
    Ok((n as u64, fts))
}

fn delete_code_system_blocking(
    conn: &mut rusqlite::Connection,
    row: &CodeSystemRow,
    progress: &OpProgress,
) -> Result<DeleteReport, HtsError> {
    let sid = row.id.as_str();
    let mut report = DeleteReport::default();

    // Foreign keys are enabled per pooled connection; the concept cascade
    // depends on it, so refuse rather than orphan designations.
    let fk: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .map_err(storage)?;
    if fk != 1 {
        conn.execute_batch("PRAGMA foreign_keys=ON")
            .map_err(storage)?;
    }

    progress.set_phase("closure");
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM concept_closure WHERE system_id = ?1",
            [sid],
            |r| r.get(0),
        )
        .map_err(storage)?;
    progress.set_total(total.max(0) as u64);
    loop {
        let n = delete_rowid_batch(conn, "concept_closure", sid, CLOSURE_BATCH)?;
        report.closure += n;
        progress.add(n);
        if n == 0 {
            break;
        }
    }

    progress.set_phase("hierarchy");
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM concept_hierarchy WHERE system_id = ?1",
            [sid],
            |r| r.get(0),
        )
        .map_err(storage)?;
    progress.set_total(total.max(0) as u64);
    loop {
        let n = delete_rowid_batch(conn, "concept_hierarchy", sid, CLOSURE_BATCH)?;
        report.hierarchy += n;
        progress.add(n);
        if n == 0 {
            break;
        }
    }

    progress.set_phase("concepts");
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM concepts WHERE system_id = ?1",
            [sid],
            |r| r.get(0),
        )
        .map_err(storage)?;
    progress.set_total(total.max(0) as u64);
    loop {
        let (n, fts) = delete_concept_batch(conn, sid)?;
        report.concepts += n;
        report.fts += fts;
        progress.add(n);
        if n == 0 {
            break;
        }
    }

    progress.set_phase("caches");
    // Persisted caches derived from the url. They are caches: dropping the
    // entries of sibling versions too only costs a recomputation.
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage)?;
    report.expansion_cache += tx
        .execute(
            "DELETE FROM value_set_expansions WHERE system_url = ?1",
            [&row.url],
        )
        .map_err(storage)? as u64;
    report.expansion_cache +=
        crate::import::fhir_bundle::invalidate_implicit_expansions(&tx, &row.url)?;
    tx.execute("DELETE FROM concepts_fts_built WHERE system_id = ?1", [sid])
        .map_err(storage)?;
    tx.execute("DELETE FROM code_systems WHERE id = ?1", [sid])
        .map_err(storage)?;
    tx.commit().map_err(storage)?;

    super::value_set::invalidate_cs_id_cache();
    super::code_system::invalidate_cs_language_cache();
    Ok(report)
}

#[async_trait]
impl TerminologyAdmin for SqliteTerminologyBackend {
    fn kind(&self) -> &'static str {
        "sqlite"
    }

    async fn code_system_rows(&self) -> Result<Vec<CodeSystemRow>, HtsError> {
        self.blocking(|conn| read_rows(conn, "ORDER BY url, version_sort DESC", &[]))
            .await
    }

    async fn resource_counts(&self) -> Result<(u64, u64), HtsError> {
        self.blocking(|conn| {
            let vs: i64 = conn
                .query_row("SELECT COUNT(*) FROM value_sets", [], |r| r.get(0))
                .map_err(storage)?;
            let cm: i64 = conn
                .query_row("SELECT COUNT(*) FROM concept_maps", [], |r| r.get(0))
                .map_err(storage)?;
            Ok((vs.max(0) as u64, cm.max(0) as u64))
        })
        .await
    }

    async fn system_stats(&self) -> Result<HashMap<String, SystemStats>, HtsError> {
        self.blocking(|conn| {
            let mut out: HashMap<String, SystemStats> = HashMap::new();
            count_by_system(
                conn,
                "SELECT system_id, COUNT(*) FROM concepts GROUP BY system_id",
                &mut out,
                |s, n| s.concepts = n,
            )?;
            count_by_system(
                conn,
                "SELECT c.system_id, COUNT(*) FROM concept_designations d \
                 JOIN concepts c ON c.id = d.concept_id GROUP BY c.system_id",
                &mut out,
                |s, n| s.designations = n,
            )?;
            count_by_system(
                conn,
                "SELECT c.system_id, COUNT(*) FROM concept_properties p \
                 JOIN concepts c ON c.id = p.concept_id GROUP BY c.system_id",
                &mut out,
                |s, n| s.properties = n,
            )?;
            count_by_system(
                conn,
                "SELECT system_id, COUNT(*) FROM concept_hierarchy GROUP BY system_id",
                &mut out,
                |s, n| s.hierarchy = n,
            )?;
            count_by_system(
                conn,
                "SELECT system_id, COUNT(*) FROM concept_closure GROUP BY system_id",
                &mut out,
                |s, n| s.closure = n,
            )?;
            for s in out.values_mut() {
                s.estimate_bytes();
            }
            Ok(out)
        })
        .await
    }

    async fn footprint(&self) -> Result<DbFootprint, HtsError> {
        self.blocking(|conn| {
            let page_size: i64 = conn
                .query_row("PRAGMA page_size", [], |r| r.get(0))
                .map_err(storage)?;
            let pages: i64 = conn
                .query_row("PRAGMA page_count", [], |r| r.get(0))
                .map_err(storage)?;
            let free: i64 = conn
                .query_row("PRAGMA freelist_count", [], |r| r.get(0))
                .map_err(storage)?;
            let path: Option<String> = conn
                .query_row("PRAGMA database_list", [], |r| r.get::<_, String>(2))
                .optional()
                .map_err(storage)?
                .filter(|p| !p.is_empty());
            let wal_bytes = path
                .as_ref()
                .and_then(|p| std::fs::metadata(format!("{p}-wal")).ok())
                .map(|m| m.len())
                .unwrap_or(0);
            Ok(DbFootprint {
                bytes: (page_size * pages).max(0) as u64,
                reclaimable_bytes: (page_size * free).max(0) as u64,
                wal_bytes,
                path,
            })
        })
        .await
    }

    async fn find_code_system(
        &self,
        url: &str,
        version: Option<&str>,
    ) -> Result<Option<CodeSystemRow>, HtsError> {
        let (url, version) = (url.to_string(), version.map(str::to_string));
        self.blocking(move |conn| {
            Ok(read_rows(
                conn,
                "WHERE url = ?1 AND COALESCE(version, '') = COALESCE(?2, '')",
                &[&url, &version],
            )?
            .into_iter()
            .next())
        })
        .await
    }

    async fn dependents(&self, url: &str, version: Option<&str>) -> Result<Dependents, HtsError> {
        let (url, version) = (url.to_string(), version.map(str::to_string));
        self.blocking(move |conn| {
            let v = version.as_deref();
            let mut deps = Dependents::default();

            // ValueSets: substring prefilter in SQL, exact check on the parsed
            // compose in Rust (formatting, `system|version`, pinned versions).
            {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, url, version, compose_json FROM value_sets \
                         WHERE compose_json IS NOT NULL AND instr(compose_json, ?1) > 0",
                    )
                    .map_err(storage)?;
                let rows = stmt
                    .query_map([&url], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, String>(3)?,
                        ))
                    })
                    .map_err(storage)?;
                for row in rows {
                    let (id, vs_url, vs_version, compose) = row.map_err(storage)?;
                    if let Some(pins) = compose_references(&compose, &url, v) {
                        deps.value_sets.push(DependentRef {
                            resource_type: "ValueSet".into(),
                            id,
                            url: vs_url,
                            version: vs_version,
                            relation: "compose.include".into(),
                            pins_version: pins,
                        });
                    }
                }
            }

            // Supplements: `supplements` = `url` or `url|version`.
            for sup in read_rows(conn, "WHERE content = 'supplement'", &[])? {
                let Some(target) = sup.supplements.as_deref() else {
                    continue;
                };
                let (t_url, t_ver) = match target.split_once('|') {
                    Some((u, ver)) => (u, Some(ver)),
                    None => (target, None),
                };
                if t_url != url {
                    continue;
                }
                if let (Some(tv), Some(v)) = (t_ver, v)
                    && tv != v
                {
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

            // ConceptMaps: declared scope or any element using the system.
            {
                let mut seen: HashSet<String> = HashSet::new();
                let mut stmt = conn
                    .prepare(
                        "SELECT id, url, version FROM concept_maps \
                         WHERE source_uri = ?1 OR target_uri = ?1 \
                            OR id IN (SELECT DISTINCT map_id FROM concept_map_elements \
                                      WHERE source_system = ?1 OR target_system = ?1)",
                    )
                    .map_err(storage)?;
                let rows = stmt
                    .query_map([&url], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                        ))
                    })
                    .map_err(storage)?;
                for row in rows {
                    let (id, cm_url, cm_version) = row.map_err(storage)?;
                    if seen.insert(id.clone()) {
                        deps.concept_maps.push(DependentRef {
                            resource_type: "ConceptMap".into(),
                            id,
                            url: cm_url,
                            version: cm_version,
                            relation: "group.source/target".into(),
                            pins_version: false,
                        });
                    }
                }
            }

            // Sibling versions and the version that takes over.
            let siblings: Vec<CodeSystemRow> = read_rows(conn, "WHERE url = ?1", &[&url])?
                .into_iter()
                .filter(|r| r.version.as_deref() != v)
                .collect();
            let mut populated: HashMap<String, SystemStats> = HashMap::new();
            for s in &siblings {
                let has: bool = conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM concepts WHERE system_id = ?1)",
                        [&s.id],
                        |r| r.get(0),
                    )
                    .map_err(storage)?;
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
            deps.current_after_delete =
                effective_current(&refs, &populated).map(|r| r.version.clone());
            deps.other_versions = siblings.iter().map(|r| r.version.clone()).collect();
            Ok(deps)
        })
        .await
    }

    async fn delete_code_system(
        &self,
        row: &CodeSystemRow,
        progress: Arc<OpProgress>,
    ) -> Result<DeleteReport, HtsError> {
        let row = row.clone();
        let report = self
            .blocking(move |conn| delete_code_system_blocking(conn, &row, &progress))
            .await;
        self.invalidate_caches();
        report
    }

    async fn delete_value_set(&self, url: &str, version: Option<&str>) -> Result<u64, HtsError> {
        let (url, version) = (url.to_string(), version.map(str::to_string));
        let n = self
            .blocking(move |conn| {
                conn.execute(
                    "DELETE FROM value_sets \
                     WHERE url = ?1 AND COALESCE(version, '') = COALESCE(?2, '')",
                    rusqlite::params![url, version],
                )
                .map(|n| n as u64)
                .map_err(storage)
            })
            .await?;
        self.invalidate_caches();
        Ok(n)
    }

    async fn delete_concept_map(&self, url: &str) -> Result<u64, HtsError> {
        let url = url.to_string();
        let n = self
            .blocking(move |conn| {
                conn.execute("DELETE FROM concept_maps WHERE url = ?1", [&url])
                    .map(|n| n as u64)
                    .map_err(storage)
            })
            .await?;
        self.invalidate_caches();
        Ok(n)
    }

    async fn set_current_version(
        &self,
        url: &str,
        version: Option<Option<&str>>,
    ) -> Result<(), HtsError> {
        let url = url.to_string();
        let version = version.map(|v| v.map(str::to_string));
        self.blocking(move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            if let Some(v) = &version {
                let exists: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM code_systems \
                         WHERE url = ?1 AND COALESCE(version, '') = COALESCE(?2, ''))",
                        rusqlite::params![url, v],
                        |r| r.get(0),
                    )
                    .map_err(storage)?;
                if !exists {
                    return Err(HtsError::NotFound(format!(
                        "CodeSystem {url}|{}",
                        v.as_deref().unwrap_or("")
                    )));
                }
            }
            tx.execute(
                "UPDATE code_systems SET current_pin = NULL \
                 WHERE url = ?1 AND current_pin IS NOT NULL",
                [&url],
            )
            .map_err(storage)?;
            if let Some(v) = &version {
                tx.execute(
                    "UPDATE code_systems SET current_pin = 1 \
                     WHERE url = ?1 AND COALESCE(version, '') = COALESCE(?2, '')",
                    rusqlite::params![url, v],
                )
                .map_err(storage)?;
            }
            // The pin decides the default version, which reports the plain id.
            let declared: Vec<String> = tx
                .prepare(
                    "SELECT DISTINCT declared_id FROM code_systems \
                     WHERE url = ?1 AND declared_id IS NOT NULL",
                )
                .and_then(|mut s| {
                    s.query_map([&url], |r| r.get::<_, String>(0))?
                        .collect::<Result<Vec<_>, _>>()
                })
                .map_err(storage)?;
            super::fhir_ids::reassign(&tx, "code_systems", &declared)?;
            tx.commit().map_err(storage)
        })
        .await?;
        self.invalidate_caches();
        Ok(())
    }

    async fn finalize_step(&self, step: &str) -> Result<(), HtsError> {
        let me = self.clone();
        let step = step.to_string();
        self.blocking(move |conn| {
            match step.as_str() {
                "closure" => schema::migrate_concept_closure(conn).map_err(storage)?,
                "fts" => {
                    value_set::prebuild_concepts_fts(conn);
                }
                "analyze" => conn
                    .execute_batch(
                        "ANALYZE concept_hierarchy; ANALYZE concepts; ANALYZE concept_closure; \
                         ANALYZE concept_properties; ANALYZE concept_designations; \
                         ANALYZE code_systems; ANALYZE value_sets; ANALYZE concept_maps;",
                    )
                    .map_err(storage)?,
                "warm" => {
                    value_set::prebuild_implicit_index(conn, &me.implicit_index);
                    value_set::prebuild_inline_compose_index(conn, &me.inline_compose_index);
                }
                _ => {}
            }
            Ok(())
        })
        .await?;
        self.invalidate_caches();
        Ok(())
    }

    async fn compact(&self) -> Result<(), HtsError> {
        self.blocking(|conn| {
            conn.execute_batch("VACUUM").map_err(storage)?;
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .map_err(storage)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::BundleImportBackend;
    use helios_persistence::tenant::TenantContext;

    fn cs_bundle(version: &str, codes: &[(&str, Option<&str>)]) -> Vec<u8> {
        let concepts: Vec<serde_json::Value> = codes
            .iter()
            .map(|(c, parent)| {
                let mut v = serde_json::json!({"code": c, "display": format!("Disp {c}"),
                    "designation": [{"language": "de", "value": format!("De {c}")}]});
                if let Some(p) = parent {
                    v["property"] = serde_json::json!([{"code": "parent", "valueCode": p}]);
                }
                v
            })
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "resourceType": "Bundle", "type": "collection",
            "entry": [
              {"resource": {"resourceType": "CodeSystem", "id": "fx", "url": "http://ex.org/fx",
                "version": version, "status": "active", "content": "complete",
                "hierarchyMeaning": "is-a",
                "property": [{"code": "parent", "type": "code"}],
                "concept": concepts}},
              {"resource": {"resourceType": "ValueSet", "id": "vs-fx", "url": "http://ex.org/vs-fx",
                "status": "active", "compose": {"include": [{"system": "http://ex.org/fx"}]}}},
              {"resource": {"resourceType": "CodeSystem", "id": "fx-sup", "url": "http://ex.org/fx-sup",
                "status": "active", "content": "supplement", "supplements": "http://ex.org/fx",
                "concept": [{"code": "A", "designation": [{"language": "zh", "value": "甲"}]}]}}
            ]
        }))
        .unwrap()
    }

    fn count(b: &SqliteTerminologyBackend, sql: &str) -> i64 {
        b.pool()
            .get()
            .unwrap()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    }

    #[tokio::test]
    async fn delete_cascade_removes_every_row_of_one_version_only() {
        let b = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        let codes = [("A", None), ("B", Some("A")), ("C", Some("B"))];
        b.import_bundle(&ctx, &cs_bundle("1", &codes))
            .await
            .unwrap();
        b.import_bundle(&ctx, &cs_bundle("2", &codes))
            .await
            .unwrap();
        b.finalize_step("closure").await.unwrap();
        b.finalize_step("fts").await.unwrap();

        let v1 = b
            .find_code_system("http://ex.org/fx", Some("1"))
            .await
            .unwrap()
            .expect("v1 stored");
        let per_sys = |id: &str, table: &str| {
            count(
                &b,
                &format!("SELECT COUNT(*) FROM {table} WHERE system_id = '{id}'"),
            )
        };
        assert_eq!(per_sys(&v1.id, "concepts"), 3);
        assert!(per_sys(&v1.id, "concept_closure") >= 6);
        assert!(per_sys(&v1.id, "concepts_fts") >= 3);

        let deps = b.dependents("http://ex.org/fx", Some("1")).await.unwrap();
        assert_eq!(deps.value_sets.len(), 1);
        assert!(!deps.value_sets[0].pins_version);
        assert_eq!(deps.supplements.len(), 1);
        assert_eq!(deps.other_versions, vec![Some("2".to_string())]);
        assert_eq!(deps.current_after_delete, Some(Some("2".into())));

        let progress = Arc::new(OpProgress::default());
        let report = b.delete_code_system(&v1, progress).await.unwrap();
        assert_eq!(report.concepts, 3);
        assert!(report.closure >= 6);
        assert!(report.fts >= 6, "FTS rows by rowid: {report:?}");

        for table in [
            "concepts",
            "concept_closure",
            "concept_hierarchy",
            "concepts_fts",
            "concepts_word_fts",
            "concepts_search_fts",
            "concepts_fts_built",
        ] {
            assert_eq!(per_sys(&v1.id, table), 0, "{table} still has v1 rows");
        }
        // Children went with their concepts (FK cascade).
        let orphans = count(
            &b,
            "SELECT COUNT(*) FROM concept_designations d \
             WHERE NOT EXISTS (SELECT 1 FROM concepts c WHERE c.id = d.concept_id)",
        );
        assert_eq!(orphans, 0);
        assert!(
            b.find_code_system("http://ex.org/fx", Some("1"))
                .await
                .unwrap()
                .is_none()
        );
        // Version 2 is untouched.
        let v2 = b
            .find_code_system("http://ex.org/fx", Some("2"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(per_sys(&v2.id, "concepts"), 3);
        assert!(per_sys(&v2.id, "concept_closure") >= 6);
    }

    #[tokio::test]
    async fn pin_overrides_semantic_order_for_versionless_lookup() {
        use crate::traits::CodeSystemOperations;
        use crate::types::LookupRequest;
        let b = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        b.import_bundle(&ctx, &cs_bundle("9.0.0", &[("A", None)]))
            .await
            .unwrap();
        b.import_bundle(&ctx, &cs_bundle("10.0.0", &[("A", None)]))
            .await
            .unwrap();
        let lookup = |b: SqliteTerminologyBackend| async move {
            let req = LookupRequest {
                system: "http://ex.org/fx".into(),
                code: "A".into(),
                version: None,
                display_language: None,
                expression: None,
                properties: vec![],
                date: None,
                use_supplements: vec![],
            };
            b.lookup(&TenantContext::system(), req)
                .await
                .unwrap()
                .version
        };
        assert_eq!(lookup(b.clone()).await.as_deref(), Some("10.0.0"));
        b.set_current_version("http://ex.org/fx", Some(Some("9.0.0")))
            .await
            .unwrap();
        assert_eq!(lookup(b.clone()).await.as_deref(), Some("9.0.0"));
        b.set_current_version("http://ex.org/fx", None)
            .await
            .unwrap();
        assert_eq!(lookup(b.clone()).await.as_deref(), Some("10.0.0"));
        assert!(
            b.set_current_version("http://ex.org/fx", Some(Some("1.0")))
                .await
                .is_err()
        );
    }
}
