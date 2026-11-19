//! SQLite side of [`crate::resource_ids`]: the `fhir_id` column, its
//! per-write maintenance and the startup repair.

use std::collections::{BTreeSet, HashSet};

use rusqlite::{Connection, OptionalExtension};

use crate::error::HtsError;
use crate::resource_ids::{self, GroupRow};

/// Resource tables that carry a reported `fhir_id`.
pub(crate) const TABLES: [&str; 3] = ["code_systems", "value_sets", "concept_maps"];

fn storage(e: rusqlite::Error) -> HtsError {
    HtsError::StorageError(e.to_string())
}

/// Precedence among the rows of one declared-id group: the backend's
/// default-version order, so the row reporting the plain id is the row the
/// versionless resolvers pick.
fn group_order_by(table: &str) -> std::borrow::Cow<'static, str> {
    precedence(table, table)
}

/// The precedence order of `table` rows, with the table aliased `alias`.
fn precedence(table: &str, alias: &str) -> std::borrow::Cow<'static, str> {
    match table {
        "code_systems" => crate::backends::cs_precedence_order_by(alias),
        "concept_maps" => crate::backends::cm_precedence_order_by(alias),
        _ => crate::backends::vs_precedence_order_by(alias),
    }
}

/// The canonical an instance-level operation (`/ValueSet/{id}/$expand`, …)
/// runs on for the row stored under `storage_id`: its `url`, pinned to
/// `url|version` when the row is not the version a versionless request would
/// pick — so `ValueSet/LL1-1-2.80/$expand` expands 2.80, not the default.
pub(crate) fn instance_canonical(
    conn: &Connection,
    table: &str,
    storage_id: &str,
) -> Option<String> {
    let sql = format!(
        "SELECT t.url, t.version, \
                (SELECT c2.id FROM {table} c2 WHERE c2.url = t.url ORDER BY {} LIMIT 1) \
         FROM {table} t WHERE t.id = ?1",
        precedence(table, "c2")
    );
    let (url, version, default_id): (String, Option<String>, Option<String>) = conn
        .query_row(&sql, [storage_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .ok()?;
    Some(match version {
        Some(v) if default_id.as_deref() != Some(storage_id) => format!("{url}|{v}"),
        _ => url,
    })
}

/// Add `fhir_id` and the `declared_id` generated column (the stored
/// resource's own `id`) with their indexes. `(id, fhir_id)` lets listings
/// read the reported id from an index: `fhir_id` sits after `resource_json`
/// in the row, and reading it from the table would walk the JSON's overflow
/// pages. Idempotent; must run after the
/// `drop_url_unique` rebuilds, which recreate the tables from fixed column
/// lists.
pub fn migrate_columns(conn: &Connection) -> rusqlite::Result<()> {
    for table in TABLES {
        for sql in [
            format!("ALTER TABLE {table} ADD COLUMN fhir_id TEXT"),
            format!(
                "ALTER TABLE {table} ADD COLUMN declared_id TEXT GENERATED ALWAYS AS \
                 (CASE WHEN json_valid(resource_json) \
                       THEN json_extract(resource_json, '$.id') END) VIRTUAL"
            ),
        ] {
            match conn.execute_batch(&sql) {
                Ok(()) => {}
                Err(e) if e.to_string().contains("duplicate column name") => {}
                Err(e) => return Err(e),
            }
        }
        conn.execute_batch(&format!(
            "CREATE INDEX IF NOT EXISTS idx_{table}_fhir_id ON {table}(fhir_id, id);
             CREATE INDEX IF NOT EXISTS idx_{table}_id_fhir_id ON {table}(id, fhir_id);
             CREATE INDEX IF NOT EXISTS idx_{table}_declared_id ON {table}(declared_id);"
        ))?;
    }
    Ok(())
}

/// Recompute the reported ids of every row whose declared id is in
/// `declared` (and of the groups currently reporting one of those ids).
pub(crate) fn reassign(
    conn: &Connection,
    table: &str,
    declared: &[String],
) -> Result<(), HtsError> {
    if declared.is_empty() {
        return Ok(());
    }
    let mut ids: BTreeSet<String> = declared.iter().cloned().collect();
    {
        let json = serde_json::to_string(declared).unwrap_or_default();
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT DISTINCT declared_id FROM {table} \
                 WHERE fhir_id IN (SELECT value FROM json_each(?1)) AND declared_id IS NOT NULL"
            ))
            .map_err(storage)?;
        let more = stmt
            .query_map([&json], |r| r.get::<_, String>(0))
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;
        ids.extend(more);
    }
    let ids: Vec<String> = ids.into_iter().collect();
    let json = serde_json::to_string(&ids).unwrap_or_default();
    let rows: Vec<GroupRow> = {
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT id, declared_id, version, fhir_id FROM {table} \
                 WHERE declared_id IN (SELECT value FROM json_each(?1)) \
                 ORDER BY declared_id, {}",
                group_order_by(table)
            ))
            .map_err(storage)?;
        stmt.query_map([&json], |r| {
            Ok(GroupRow {
                storage_id: r.get(0)?,
                declared: r.get(1)?,
                version: r.get(2)?,
                current: r.get(3)?,
            })
        })
        .map_err(storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage)?
    };
    if rows.is_empty() {
        return Ok(());
    }
    let in_batch: HashSet<&str> = rows.iter().map(|r| r.storage_id.as_str()).collect();
    let mut reserved: HashSet<String> = HashSet::new();
    let mut changes = resource_ids::assign(&rows, &reserved);
    // A versioned id may already be reported (or declared) by a row outside
    // these groups; reserve it and recompute. Converges in a step or two.
    for _ in 0..8 {
        let wanted: Vec<&str> = changes.iter().map(|(_, id)| id.as_str()).collect();
        if wanted.is_empty() {
            break;
        }
        let wanted_json = serde_json::to_string(&wanted).unwrap_or_default();
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT id, fhir_id FROM {table} WHERE fhir_id IN (SELECT value FROM json_each(?1)) \
                 UNION ALL \
                 SELECT id, declared_id FROM {table} WHERE declared_id IN (SELECT value FROM json_each(?1))"
            ))
            .map_err(storage)?;
        let taken: Vec<String> = stmt
            .query_map([&wanted_json], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(storage)?
            .filter_map(Result::ok)
            .filter(|(sid, _)| !in_batch.contains(sid.as_str()))
            .map(|(_, id)| id)
            .filter(|id| !reserved.contains(id))
            .collect();
        if taken.is_empty() {
            break;
        }
        reserved.extend(taken);
        changes = resource_ids::assign(&rows, &reserved);
    }
    let mut update = conn
        .prepare_cached(&format!("UPDATE {table} SET fhir_id = ?1 WHERE id = ?2"))
        .map_err(storage)?;
    for (storage_id, fhir_id) in &changes {
        update.execute([fhir_id, storage_id]).map_err(storage)?;
    }
    Ok(())
}

/// Declared id of the row stored under `storage_id`.
pub(crate) fn declared_of(conn: &Connection, table: &str, storage_id: &str) -> Option<String> {
    conn.query_row(
        &format!("SELECT declared_id FROM {table} WHERE id = ?1"),
        [storage_id],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()
    .ok()
    .flatten()
    .flatten()
}

/// Startup repair: assign ids to rows that have none (rows written before
/// the column existed) and split any group whose rows report the same id.
/// Idempotent; on an up-to-date database it is two index scans.
pub fn repair(conn: &Connection) -> Result<usize, HtsError> {
    let mut fixed = 0;
    for table in TABLES {
        let pending: Vec<String> = {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT DISTINCT declared_id FROM {table} \
                     WHERE fhir_id IS NULL AND declared_id IS NOT NULL \
                     UNION \
                     SELECT declared_id FROM {table} WHERE declared_id IS NOT NULL AND fhir_id IN \
                       (SELECT fhir_id FROM {table} WHERE fhir_id IS NOT NULL \
                        GROUP BY fhir_id HAVING COUNT(*) > 1)"
                ))
                .map_err(storage)?;
            stmt.query_map([], |r| r.get::<_, String>(0))
                .map_err(storage)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage)?
        };
        if pending.is_empty() {
            continue;
        }
        conn.execute_batch("BEGIN IMMEDIATE").map_err(storage)?;
        let res = pending
            .chunks(500)
            .try_for_each(|chunk| reassign(conn, table, chunk));
        match res {
            Ok(()) => conn.execute_batch("COMMIT").map_err(storage)?,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
        }
        tracing::info!(
            table,
            groups = pending.len(),
            "Assigned unique resource ids"
        );
        fixed += pending.len();
    }
    Ok(fixed)
}

/// Resolve a requested id to `(storage id, reported id)`:
///
/// 1. the row reporting it;
/// 2. a storage id (`loinc|2.80`, or a plain id of a row without `fhir_id`),
///    best row by precedence when it names several versions;
/// 3. `<declared>-<version>` of the default version, which reports the plain
///    declared id.
pub(crate) fn resolve(conn: &Connection, table: &str, id: &str) -> Option<(String, String)> {
    if let Ok(Some(sid)) = conn
        .query_row(
            &format!("SELECT id FROM {table} WHERE fhir_id = ?1 LIMIT 1"),
            [id],
            |r| r.get::<_, String>(0),
        )
        .optional()
    {
        return Some((sid, id.to_owned()));
    }
    // Served by the primary key (the `|` prefix is expressed as a range; `}`
    // is the next ASCII character after `|`).
    if let Ok(Some((sid, fhir))) = conn
        .query_row(
            &format!(
                "SELECT id, fhir_id FROM {table} \
                 WHERE id = ?1 OR (id >= ?1 || '|' AND id < ?1 || '}}') \
                 ORDER BY (id = ?1) DESC, {} LIMIT 1",
                group_order_by(table)
            ),
            [id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()
    {
        let reported = resource_ids::reported_id(&sid, fhir.as_deref());
        return Some((sid, reported));
    }
    for (declared, _) in resource_ids::versioned_id_candidates(id) {
        let Ok(mut stmt) = conn.prepare_cached(&format!(
            "SELECT id, version FROM {table} WHERE declared_id = ?1"
        )) else {
            return None;
        };
        let Ok(rows) = stmt
            .query_map([declared], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .map(|rows| rows.filter_map(Result::ok).collect::<Vec<_>>())
        else {
            continue;
        };
        if let Some((sid, _)) = rows
            .into_iter()
            .find(|(_, v)| resource_ids::versioned_id(declared, v.as_deref()) == id)
        {
            return Some((sid, id.to_owned()));
        }
    }
    None
}

/// Reported ids of the rows stored under `storage_ids` (index-only lookups).
pub(crate) fn reported_ids(
    conn: &Connection,
    table: &str,
    storage_ids: &[&str],
) -> Result<std::collections::HashMap<String, String>, HtsError> {
    let mut out = std::collections::HashMap::with_capacity(storage_ids.len());
    for chunk in storage_ids.chunks(900) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let mut stmt = conn
            .prepare(&format!(
                "SELECT id, fhir_id FROM {table} INDEXED BY idx_{table}_id_fhir_id \
                 WHERE id IN ({placeholders})"
            ))
            .map_err(storage)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .map_err(storage)?;
        for row in rows {
            let (sid, fhir) = row.map_err(storage)?;
            let reported = resource_ids::reported_id(&sid, fhir.as_deref());
            out.insert(sid, reported);
        }
    }
    Ok(out)
}

/// `storage id → fhir_id` of every row (an index-only scan), for the `_id`
/// search filter.
pub(crate) fn all_fhir_ids(
    conn: &Connection,
    table: &str,
) -> Result<std::collections::HashMap<String, Option<String>>, HtsError> {
    let mut stmt = conn
        .prepare_cached(&format!(
            "SELECT id, fhir_id FROM {table} INDEXED BY idx_{table}_id_fhir_id"
        ))
        .map_err(storage)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .map_err(storage)?
        .collect::<Result<_, _>>()
        .map_err(storage)?;
    Ok(rows)
}
