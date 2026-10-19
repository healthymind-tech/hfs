//! Reading and writing the `snomed_*` tables, for both backends.

use std::collections::HashMap;
use std::sync::Arc;

use super::model::{self, ModelRows, SnomedModel};
use super::{ConceptInfo, ConceptRow, ModelImport};
use crate::error::HtsError;

fn split_terms(s: &str) -> Vec<String> {
    s.split('\n').filter(|t| !t.is_empty()).map(str::to_string).collect()
}

// ── SQLite ──────────────────────────────────────────────────────────────────

#[cfg(feature = "sqlite")]
mod sqlite {
    use super::*;
    use rusqlite::{Connection, OptionalExtension, params};

    fn err(e: rusqlite::Error) -> HtsError {
        HtsError::StorageError(e.to_string())
    }

    /// Replace the model rows of `url|version`; returns the rows written.
    pub fn write(
        conn: &mut Connection,
        url: &str,
        version: Option<&str>,
        data: &ModelImport,
    ) -> Result<u64, HtsError> {
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(err)?;
        let system_id: Option<String> = tx
            .query_row(
                "SELECT id FROM code_systems WHERE url = ?1 AND COALESCE(version, '') = COALESCE(?2, '')",
                params![url, version],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)?;
        let Some(system_id) = system_id else {
            return Err(HtsError::NotFound(format!(
                "CodeSystem {url}|{} is not imported; cannot store its concept model",
                version.unwrap_or("")
            )));
        };
        for t in ["snomed_concepts", "snomed_relationships", "snomed_mrcm", "snomed_model_stamps"] {
            tx.execute(&format!("DELETE FROM {t} WHERE system_id = ?1"), [&system_id])
                .map_err(err)?;
        }
        let mut n = 0u64;
        {
            let mut st = tx
                .prepare_cached(
                    "INSERT OR REPLACE INTO snomed_concepts
                     (system_id, code, active, primitive, effective_time, module_id, terms, inactive_terms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )
                .map_err(err)?;
            for c in &data.concepts {
                n += st
                    .execute(params![
                        system_id,
                        c.code,
                        c.active,
                        c.primitive,
                        c.effective_time,
                        c.module,
                        c.terms.join("\n"),
                        c.inactive_terms.join("\n")
                    ])
                    .map_err(err)? as u64;
            }
            let mut st = tx
                .prepare_cached(
                    "INSERT INTO snomed_relationships
                     (system_id, source_code, rel_group, type_code, value, concrete)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .map_err(err)?;
            for (s, g, t, v, c) in &data.relationships {
                n += st.execute(params![system_id, s, g, t, v, c]).map_err(err)? as u64;
            }
            let mut st = tx
                .prepare_cached(
                    "INSERT INTO snomed_mrcm (system_id, kind, attribute_code, c1, c2, c3, c4)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .map_err(err)?;
            for (k, a, c1, c2, c3, c4) in &data.mrcm {
                n += st
                    .execute(params![system_id, k, a, c1, c2, c3, c4])
                    .map_err(err)? as u64;
            }
        }
        tx.execute(
            "INSERT INTO snomed_model_stamps (system_id, stamp) VALUES (?1, ?2)",
            params![system_id, uuid::Uuid::new_v4().to_string()],
        )
        .map_err(err)?;
        tx.commit().map_err(err)?;
        Ok(n)
    }

    /// The import stamp of `system_id`'s model, if it has one.
    pub fn stamp(conn: &Connection, system_id: &str) -> Result<Option<String>, HtsError> {
        conn.query_row(
            "SELECT stamp FROM snomed_model_stamps WHERE system_id = ?1",
            [system_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(err)
    }

    /// The model of `system_id`, built on first use.
    pub fn load(conn: &Connection, system_id: &str) -> Result<Option<Arc<SnomedModel>>, HtsError> {
        let Some(stamp) = stamp(conn, system_id)? else { return Ok(None) };
        if let Some(m) = model::cached(&stamp) {
            return Ok(Some(m));
        }
        let mut rows = ModelRows::default();
        let mut st = conn
            .prepare(
                "SELECT s.code, s.active, s.primitive, c.display,
                        substr(s.terms, 1, instr(s.terms || char(10), char(10)) - 1)
                 FROM snomed_concepts s
                 LEFT JOIN concepts c ON c.system_id = s.system_id AND c.code = s.code
                 WHERE s.system_id = ?1",
            )
            .map_err(err)?;
        rows.concepts = st
            .query_map([system_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get::<_, Option<String>>(4)?.filter(|s| !s.is_empty())))
            })
            .map_err(err)?
            .collect::<rusqlite::Result<_>>()
            .map_err(err)?;
        let mut st = conn
            .prepare(
                "SELECT source_code, rel_group, type_code, value, concrete
                 FROM snomed_relationships WHERE system_id = ?1",
            )
            .map_err(err)?;
        rows.relationships = st
            .query_map([system_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .map_err(err)?
            .collect::<rusqlite::Result<_>>()
            .map_err(err)?;
        let mut st = conn
            .prepare("SELECT refset_id, referenced_code FROM refset_members WHERE system_id = ?1")
            .map_err(err)?;
        rows.refset_members = st
            .query_map([system_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(err)?
            .collect::<rusqlite::Result<_>>()
            .map_err(err)?;
        let mut st = conn
            .prepare(
                "SELECT kind, attribute_code, c1, c2, c3, c4 FROM snomed_mrcm WHERE system_id = ?1",
            )
            .map_err(err)?;
        rows.mrcm = st
            .query_map([system_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })
            .map_err(err)?
            .collect::<rusqlite::Result<_>>()
            .map_err(err)?;
        let mut m = SnomedModel::build(rows);
        m.system_id = system_id.to_string();
        m.version = conn
            .query_row("SELECT version FROM code_systems WHERE id = ?1", [system_id], |r| r.get(0))
            .optional()
            .map_err(err)?
            .flatten();
        Ok(Some(model::remember(&stamp, m)))
    }

    /// Stored rows of `codes`.
    pub fn concepts(
        conn: &Connection,
        system_id: &str,
        codes: &[String],
    ) -> Result<HashMap<String, ConceptInfo>, HtsError> {
        let mut st = conn
            .prepare_cached(
                "SELECT s.code, s.active, s.primitive, s.effective_time, s.module_id, s.terms,
                        s.inactive_terms, c.display
                 FROM snomed_concepts s
                 LEFT JOIN concepts c ON c.system_id = s.system_id AND c.code = s.code
                 WHERE s.system_id = ?1 AND s.code = ?2",
            )
            .map_err(err)?;
        let mut out = HashMap::new();
        for code in codes {
            let row = st
                .query_row(params![system_id, code], |r| {
                    Ok(ConceptInfo {
                        row: ConceptRow {
                            code: r.get(0)?,
                            active: r.get(1)?,
                            primitive: r.get(2)?,
                            effective_time: r.get(3)?,
                            module: r.get(4)?,
                            terms: split_terms(&r.get::<_, String>(5)?),
                            inactive_terms: split_terms(&r.get::<_, String>(6)?),
                        },
                        display: r.get(7)?,
                    })
                })
                .optional()
                .map_err(err)?;
            if let Some(info) = row {
                out.insert(code.clone(), info);
            }
        }
        Ok(out)
    }

    /// The codes among `codes` the system does not hold.
    pub fn missing(conn: &Connection, system_id: &str, codes: &[String]) -> Result<Vec<String>, HtsError> {
        let mut st = conn
            .prepare_cached("SELECT 1 FROM concepts WHERE system_id = ?1 AND code = ?2")
            .map_err(err)?;
        let mut out = Vec::new();
        for c in codes {
            if !st.exists(params![system_id, c]).map_err(err)? {
                out.push(c.clone());
            }
        }
        Ok(out)
    }

    /// The stored system id of `url|version` (the latest version when
    /// unpinned).
    pub fn system_id(conn: &Connection, url: &str, version: Option<&str>) -> Result<Option<String>, HtsError> {
        conn.query_row(
            "SELECT id FROM code_systems c WHERE url = ?1 AND (?2 IS NULL OR version = ?2)
             ORDER BY EXISTS (SELECT 1 FROM snomed_model_stamps s WHERE s.system_id = c.id) DESC,
                      version IS NULL, version DESC
             LIMIT 1",
            params![url, version],
            |r| r.get(0),
        )
        .optional()
        .map_err(err)
    }
}

#[cfg(feature = "sqlite")]
pub use sqlite::{
    concepts as concepts_sqlite, load as load_sqlite, missing as missing_sqlite,
    stamp as stamp_sqlite, system_id as system_id_sqlite, write as write_sqlite,
};

// ── PostgreSQL ──────────────────────────────────────────────────────────────

#[cfg(feature = "postgres")]
mod pg {
    use super::*;
    use tokio_postgres::Client;

    fn err(e: tokio_postgres::Error) -> HtsError {
        HtsError::StorageError(crate::backends::postgres::error_chain(&e))
    }

    pub async fn write(
        client: &mut Client,
        url: &str,
        version: Option<&str>,
        data: &ModelImport,
    ) -> Result<u64, HtsError> {
        let tx = client.transaction().await.map_err(err)?;
        let system_id: Option<String> = tx
            .query_opt(
                "SELECT id FROM code_systems WHERE url = $1 AND COALESCE(version, '') = COALESCE($2, '')",
                &[&url, &version],
            )
            .await
            .map_err(err)?
            .map(|r| r.get(0));
        let Some(system_id) = system_id else {
            return Err(HtsError::NotFound(format!(
                "CodeSystem {url}|{} is not imported; cannot store its concept model",
                version.unwrap_or("")
            )));
        };
        for t in ["snomed_concepts", "snomed_relationships", "snomed_mrcm", "snomed_model_stamps"] {
            tx.execute(&format!("DELETE FROM {t} WHERE system_id = $1"), &[&system_id])
                .await
                .map_err(err)?;
        }
        let mut n = 0u64;
        for chunk in data.concepts.chunks(20_000) {
            let code: Vec<&str> = chunk.iter().map(|c| c.code.as_str()).collect();
            let active: Vec<bool> = chunk.iter().map(|c| c.active).collect();
            let primitive: Vec<bool> = chunk.iter().map(|c| c.primitive).collect();
            let eff: Vec<Option<&str>> = chunk.iter().map(|c| c.effective_time.as_deref()).collect();
            let module: Vec<Option<&str>> = chunk.iter().map(|c| c.module.as_deref()).collect();
            let terms: Vec<String> = chunk.iter().map(|c| c.terms.join("\n")).collect();
            let inactive: Vec<String> = chunk.iter().map(|c| c.inactive_terms.join("\n")).collect();
            n += tx
                .execute(
                    "INSERT INTO snomed_concepts
                     (system_id, code, active, primitive, effective_time, module_id, terms, inactive_terms)
                     SELECT $1, * FROM UNNEST($2::text[], $3::bool[], $4::bool[], $5::text[], $6::text[], $7::text[], $8::text[])
                     ON CONFLICT DO NOTHING",
                    &[&system_id, &code, &active, &primitive, &eff, &module, &terms, &inactive],
                )
                .await
                .map_err(err)?;
        }
        for chunk in data.relationships.chunks(50_000) {
            let s: Vec<&str> = chunk.iter().map(|r| r.0.as_str()).collect();
            let g: Vec<i32> = chunk.iter().map(|r| r.1).collect();
            let t: Vec<&str> = chunk.iter().map(|r| r.2.as_str()).collect();
            let v: Vec<&str> = chunk.iter().map(|r| r.3.as_str()).collect();
            let c: Vec<bool> = chunk.iter().map(|r| r.4).collect();
            n += tx
                .execute(
                    "INSERT INTO snomed_relationships (system_id, source_code, rel_group, type_code, value, concrete)
                     SELECT $1, * FROM UNNEST($2::text[], $3::int4[], $4::text[], $5::text[], $6::bool[])",
                    &[&system_id, &s, &g, &t, &v, &c],
                )
                .await
                .map_err(err)?;
        }
        for (k, a, c1, c2, c3, c4) in &data.mrcm {
            n += tx
                .execute(
                    "INSERT INTO snomed_mrcm (system_id, kind, attribute_code, c1, c2, c3, c4)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                    &[&system_id, k, a, c1, c2, c3, c4],
                )
                .await
                .map_err(err)?;
        }
        tx.execute(
            "INSERT INTO snomed_model_stamps (system_id, stamp) VALUES ($1, $2)",
            &[&system_id, &uuid::Uuid::new_v4().to_string()],
        )
        .await
        .map_err(err)?;
        tx.commit().await.map_err(err)?;
        Ok(n)
    }

    pub async fn stamp(client: &Client, system_id: &str) -> Result<Option<String>, HtsError> {
        Ok(client
            .query_opt("SELECT stamp FROM snomed_model_stamps WHERE system_id = $1", &[&system_id])
            .await
            .map_err(err)?
            .map(|r| r.get(0)))
    }

    pub async fn load(client: &Client, system_id: &str) -> Result<Option<Arc<SnomedModel>>, HtsError> {
        let Some(stamp) = stamp(client, system_id).await? else { return Ok(None) };
        if let Some(m) = model::cached(&stamp) {
            return Ok(Some(m));
        }
        let mut rows = ModelRows::default();
        rows.concepts = client
            .query(
                "SELECT s.code, s.active, s.primitive, c.display, NULLIF(split_part(s.terms, E'\\n', 1), '')
                 FROM snomed_concepts s
                 LEFT JOIN concepts c ON c.system_id = s.system_id AND c.code = s.code
                 WHERE s.system_id = $1",
                &[&system_id],
            )
            .await
            .map_err(err)?
            .into_iter()
            .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4)))
            .collect();
        rows.relationships = client
            .query(
                "SELECT source_code, rel_group, type_code, value, concrete
                 FROM snomed_relationships WHERE system_id = $1",
                &[&system_id],
            )
            .await
            .map_err(err)?
            .into_iter()
            .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4)))
            .collect();
        rows.refset_members = client
            .query(
                "SELECT refset_id, referenced_code FROM refset_members WHERE system_id = $1",
                &[&system_id],
            )
            .await
            .map_err(err)?
            .into_iter()
            .map(|r| (r.get(0), r.get(1)))
            .collect();
        rows.mrcm = client
            .query(
                "SELECT kind, attribute_code, c1, c2, c3, c4 FROM snomed_mrcm WHERE system_id = $1",
                &[&system_id],
            )
            .await
            .map_err(err)?
            .into_iter()
            .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5)))
            .collect();
        let mut m = SnomedModel::build(rows);
        m.system_id = system_id.to_string();
        m.version = client
            .query_opt("SELECT version FROM code_systems WHERE id = $1", &[&system_id])
            .await
            .map_err(err)?
            .and_then(|r| r.get(0));
        Ok(Some(model::remember(&stamp, m)))
    }

    pub async fn concepts(
        client: &Client,
        system_id: &str,
        codes: &[String],
    ) -> Result<HashMap<String, ConceptInfo>, HtsError> {
        let rows = client
            .query(
                "SELECT s.code, s.active, s.primitive, s.effective_time, s.module_id, s.terms,
                        s.inactive_terms, c.display
                 FROM snomed_concepts s
                 LEFT JOIN concepts c ON c.system_id = s.system_id AND c.code = s.code
                 WHERE s.system_id = $1 AND s.code = ANY($2)",
                &[&system_id, &codes],
            )
            .await
            .map_err(err)?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let code: String = r.get(0);
                let info = ConceptInfo {
                    row: ConceptRow {
                        code: code.clone(),
                        active: r.get(1),
                        primitive: r.get(2),
                        effective_time: r.get(3),
                        module: r.get(4),
                        terms: split_terms(r.get(5)),
                        inactive_terms: split_terms(r.get(6)),
                    },
                    display: r.get(7),
                };
                (code, info)
            })
            .collect())
    }

    pub async fn missing(client: &Client, system_id: &str, codes: &[String]) -> Result<Vec<String>, HtsError> {
        let found: std::collections::HashSet<String> = client
            .query(
                "SELECT code FROM concepts WHERE system_id = $1 AND code = ANY($2)",
                &[&system_id, &codes],
            )
            .await
            .map_err(err)?
            .into_iter()
            .map(|r| r.get(0))
            .collect();
        Ok(codes.iter().filter(|c| !found.contains(*c)).cloned().collect())
    }

    pub async fn system_id(client: &Client, url: &str, version: Option<&str>) -> Result<Option<String>, HtsError> {
        Ok(client
            .query_opt(
                "SELECT id FROM code_systems c WHERE url = $1 AND ($2::text IS NULL OR version = $2)
                 ORDER BY EXISTS (SELECT 1 FROM snomed_model_stamps s WHERE s.system_id = c.id) DESC,
                          version DESC NULLS LAST
                 LIMIT 1",
                &[&url, &version],
            )
            .await
            .map_err(err)?
            .map(|r| r.get(0)))
    }
}

#[cfg(feature = "postgres")]
pub use pg::{
    concepts as concepts_pg, load as load_pg, missing as missing_pg, stamp as stamp_pg,
    system_id as system_id_pg, write as write_pg,
};
