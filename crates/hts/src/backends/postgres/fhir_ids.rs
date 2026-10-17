//! PostgreSQL side of [`crate::resource_ids`]: per-write maintenance of the
//! `fhir_id` column, the startup repair and id resolution. Mirrors
//! `backends::sqlite::fhir_ids`; the declared id is `resource_json->>'id'`
//! (expression-indexed by `idx_*_json_id`).

use std::collections::{BTreeSet, HashSet};

use tokio_postgres::GenericClient;

use super::error_chain;
use crate::error::HtsError;
use crate::resource_ids::{self, GroupRow};

/// Resource tables that carry a reported `fhir_id`.
pub(crate) const TABLES: [&str; 2] = ["code_systems", "value_sets"];

fn db(e: tokio_postgres::Error) -> HtsError {
    HtsError::StorageError(error_chain(&e))
}

fn group_order_by(table: &str) -> std::borrow::Cow<'static, str> {
    precedence(table, table)
}

/// The precedence order of `table` rows, with the table aliased `alias`.
fn precedence(table: &str, alias: &str) -> std::borrow::Cow<'static, str> {
    match table {
        "code_systems" => crate::backends::cs_precedence_order_by(alias),
        _ => crate::backends::vs_precedence_order_by(alias),
    }
}

/// The canonical an instance-level operation runs on for the row stored
/// under `storage_id`: `url`, or `url|version` when the row is not the
/// default version. See the SQLite twin.
pub(crate) async fn instance_canonical<C: GenericClient + Sync>(
    client: &C,
    table: &str,
    storage_id: &str,
) -> Result<Option<String>, HtsError> {
    let sql = format!(
        "SELECT t.url, t.version, \
                (SELECT c2.id FROM {table} c2 WHERE c2.url = t.url ORDER BY {} LIMIT 1) \
         FROM {table} t WHERE t.id = $1",
        precedence(table, "c2")
    );
    let Some(row) = client.query_opt(&sql, &[&storage_id]).await.map_err(db)? else {
        return Ok(None);
    };
    let url: String = row.get(0);
    let version: Option<String> = row.get(1);
    let default_id: Option<String> = row.get(2);
    Ok(Some(match version {
        Some(v) if default_id.as_deref() != Some(storage_id) => format!("{url}|{v}"),
        _ => url,
    }))
}

/// Recompute the reported ids of every row whose declared id is in
/// `declared` (and of the groups currently reporting one of those ids).
pub(crate) async fn reassign<C: GenericClient + Sync>(
    client: &C,
    table: &str,
    declared: &[String],
) -> Result<(), HtsError> {
    if declared.is_empty() {
        return Ok(());
    }
    let mut ids: BTreeSet<String> = declared.iter().cloned().collect();
    let more = client
        .query(
            &format!(
                "SELECT DISTINCT resource_json->>'id' FROM {table} \
                 WHERE fhir_id = ANY($1) AND resource_json->>'id' IS NOT NULL"
            ),
            &[&declared],
        )
        .await
        .map_err(db)?;
    ids.extend(more.into_iter().map(|r| r.get::<_, String>(0)));
    let ids: Vec<String> = ids.into_iter().collect();
    let rows: Vec<GroupRow> = client
        .query(
            &format!(
                "SELECT id, resource_json->>'id', version, fhir_id FROM {table} \
                 WHERE resource_json->>'id' = ANY($1) \
                 ORDER BY resource_json->>'id', {}",
                group_order_by(table)
            ),
            &[&ids],
        )
        .await
        .map_err(db)?
        .into_iter()
        .map(|r| GroupRow {
            storage_id: r.get(0),
            declared: r.get(1),
            version: r.get(2),
            current: r.get(3),
        })
        .collect();
    if rows.is_empty() {
        return Ok(());
    }
    let in_batch: HashSet<&str> = rows.iter().map(|r| r.storage_id.as_str()).collect();
    let mut reserved: HashSet<String> = HashSet::new();
    let mut changes = resource_ids::assign(&rows, &reserved);
    // A versioned id may already be reported (or declared) by a row outside
    // these groups; reserve it and recompute.
    for _ in 0..8 {
        let wanted: Vec<&str> = changes.iter().map(|(_, id)| id.as_str()).collect();
        if wanted.is_empty() {
            break;
        }
        let taken: Vec<String> = client
            .query(
                &format!(
                    "SELECT id, fhir_id FROM {table} WHERE fhir_id = ANY($1) \
                     UNION ALL \
                     SELECT id, resource_json->>'id' FROM {table} WHERE resource_json->>'id' = ANY($1)"
                ),
                &[&wanted],
            )
            .await
            .map_err(db)?
            .into_iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
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
    if changes.is_empty() {
        return Ok(());
    }
    let (sids, fids): (Vec<String>, Vec<String>) = changes.into_iter().unzip();
    client
        .execute(
            &format!(
                "UPDATE {table} t SET fhir_id = v.f \
                 FROM UNNEST($1::text[], $2::text[]) AS v(s, f) WHERE t.id = v.s"
            ),
            &[&sids, &fids],
        )
        .await
        .map_err(db)?;
    Ok(())
}

/// Startup repair: assign ids to rows that have none (written before the
/// column existed) and split every group whose rows report the same id —
/// e.g. LOINC 2.80 and 2.83 both reporting `loinc`. Idempotent.
pub async fn repair(client: &mut deadpool_postgres::Object) -> Result<usize, HtsError> {
    let mut fixed = 0;
    for table in TABLES {
        let pending: Vec<String> = client
            .query(
                &format!(
                    "SELECT DISTINCT resource_json->>'id' FROM {table} \
                     WHERE fhir_id IS NULL AND resource_json->>'id' IS NOT NULL \
                     UNION \
                     SELECT resource_json->>'id' FROM {table} \
                     WHERE resource_json->>'id' IS NOT NULL AND fhir_id IN \
                       (SELECT fhir_id FROM {table} WHERE fhir_id IS NOT NULL \
                        GROUP BY fhir_id HAVING COUNT(*) > 1)"
                ),
                &[],
            )
            .await
            .map_err(db)?
            .into_iter()
            .map(|r| r.get(0))
            .collect();
        if pending.is_empty() {
            continue;
        }
        let tx = client.transaction().await.map_err(db)?;
        for chunk in pending.chunks(1000) {
            reassign(&*tx, table, chunk).await?;
        }
        tx.commit().await.map_err(db)?;
        tracing::info!(
            table,
            groups = pending.len(),
            "Assigned unique resource ids"
        );
        fixed += pending.len();
    }
    Ok(fixed)
}

/// Resolve a requested id to `(storage id, reported id)`: the row reporting
/// it; else a storage id (best version by precedence when it names several);
/// else `<declared>-<version>` of the default version. See the SQLite twin.
pub(crate) async fn resolve<C: GenericClient + Sync>(
    client: &C,
    table: &str,
    id: &str,
) -> Result<Option<(String, String)>, HtsError> {
    if let Some(row) = client
        .query_opt(
            &format!("SELECT id FROM {table} WHERE fhir_id = $1 LIMIT 1"),
            &[&id],
        )
        .await
        .map_err(db)?
    {
        return Ok(Some((row.get(0), id.to_owned())));
    }
    // Matched on the segment before the first `|` (expression-indexed)
    // rather than an `id >= x||'|'` range, which does not hold under a
    // linguistic collation.
    if let Some(row) = client
        .query_opt(
            &format!(
                "SELECT id, fhir_id FROM {table} \
                 WHERE id = $1 OR (split_part(id, '|', 1) = $1 AND strpos(id, '|') > 0) \
                 ORDER BY (id = $1) DESC, {} LIMIT 1",
                group_order_by(table)
            ),
            &[&id],
        )
        .await
        .map_err(db)?
    {
        let sid: String = row.get(0);
        let fhir: Option<String> = row.get(1);
        let reported = resource_ids::reported_id(&sid, fhir.as_deref());
        return Ok(Some((sid, reported)));
    }
    for (declared, _) in resource_ids::versioned_id_candidates(id) {
        let rows = client
            .query(
                &format!("SELECT id, version FROM {table} WHERE resource_json->>'id' = $1"),
                &[&declared],
            )
            .await
            .map_err(db)?;
        if let Some(row) = rows.into_iter().find(|r| {
            resource_ids::versioned_id(declared, r.get::<_, Option<String>>(1).as_deref()) == id
        }) {
            return Ok(Some((row.get(0), id.to_owned())));
        }
    }
    Ok(None)
}

/// `(storage id, declared id)` of the row reporting `id`.
pub(crate) async fn reported_row<C: GenericClient + Sync>(
    client: &C,
    table: &str,
    id: &str,
) -> Result<Option<(String, Option<String>)>, HtsError> {
    Ok(client
        .query_opt(
            &format!("SELECT id, resource_json->>'id' FROM {table} WHERE fhir_id = $1 LIMIT 1"),
            &[&id],
        )
        .await
        .map_err(db)?
        .map(|r| (r.get(0), r.get(1))))
}
