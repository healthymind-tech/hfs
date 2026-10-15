//! `hts migrate`: copy an HTS SQLite database into PostgreSQL.
//!
//! Moves a server from the SQLite backend to the PostgreSQL one without
//! re-importing the source terminologies: every normalized table is streamed
//! from the SQLite file into PostgreSQL with binary `COPY`, so a 6–7 GB
//! production database (SNOMED CT, LOINC, ICD, THO, IG packages) moves in
//! minutes instead of the hours a re-import of every source takes — and the
//! result holds exactly the same rows, including the `bootstrap_imports`
//! ledger (so a PostgreSQL server started on the same `HTS_BOOTSTRAP_DIR` does
//! not re-import anything) and the REST-created resources of the persistence
//! store.
//!
//! What is copied:
//!
//! | SQLite | PostgreSQL |
//! |---|---|
//! | `code_systems`, `value_sets`, `concept_maps` | same; `resource_json` TEXT → JSONB |
//! | `concepts`, `concept_properties`, `concept_designations`, `concept_map_elements` | same, ids preserved, sequences advanced |
//! | `concept_hierarchy`, `concept_closure`, `refset_members` | same (orphan rows of deleted CodeSystems dropped) |
//! | `value_set_expansions` | same (persistent expansion cache) |
//! | `bootstrap_imports` | same (`imported_at` → TIMESTAMPTZ) |
//! | persistence `resources` (REST CRUD) | latest version of every live resource, via `create_or_update` |
//!
//! Not copied, because PostgreSQL derives or does not use them:
//! `implicit_expansion_cache` and the FTS5 tables (`concepts_fts*`,
//! `concepts_search_fts`, `implicit_expansion_*fts*`), and persistence version
//! history (each resource restarts at version 1).
//!
//! The source is opened read-only and is never modified. Stop the SQLite
//! server first (or migrate from a copy) so the snapshot is consistent.
//!
//! PostgreSQL text cannot hold NUL characters; any found in the source are
//! removed and counted in the report.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, bail};
use futures::pin_mut;
use rusqlite::{Connection, OpenFlags};
use tokio_postgres::binary_copy::BinaryCopyInWriter;
use tokio_postgres::types::{ToSql, Type};
use tracing::{info, warn};

/// Options for [`migrate_sqlite_to_postgres`].
#[derive(Debug, Clone)]
pub struct MigrateOptions {
    /// SQLite `hts.db` to read.
    pub from: PathBuf,
    /// PostgreSQL connection string to write.
    pub to: String,
    /// Empty the target's HTS tables first instead of refusing a non-empty
    /// target.
    pub replace: bool,
    /// Skip the persistence-store resources (REST CRUD).
    pub skip_resources: bool,
    /// Tables copied concurrently.
    pub jobs: usize,
}

/// Per-table outcome of a migration.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TableReport {
    pub table: String,
    pub source_rows: u64,
    pub copied_rows: u64,
    pub seconds: f64,
}

/// Outcome of [`migrate_sqlite_to_postgres`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct MigrateReport {
    pub tables: Vec<TableReport>,
    pub resources: u64,
    pub nul_chars_removed: u64,
    pub json_unparseable: u64,
    pub seconds: f64,
}

impl MigrateReport {
    /// Tables whose copied row count differs from the rows selected at the
    /// source.
    pub fn mismatches(&self) -> Vec<&TableReport> {
        self.tables
            .iter()
            .filter(|t| t.source_rows != t.copied_rows)
            .collect()
    }
}

/// PostgreSQL column type of a copied column, and how the SQLite value is
/// converted.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Col {
    Text,
    Int8,
    Int4,
    Jsonb,
    Timestamptz,
}

impl Col {
    fn pg_type(self) -> Type {
        match self {
            Col::Text => Type::TEXT,
            Col::Int8 => Type::INT8,
            Col::Int4 => Type::INT4,
            Col::Jsonb => Type::JSONB,
            Col::Timestamptz => Type::TIMESTAMPTZ,
        }
    }
}

/// One table to copy: target columns, and the SQLite query producing them.
struct TableSpec {
    table: &'static str,
    columns: &'static [(&'static str, Col)],
    /// SQLite rows to copy; `{cols}` is replaced by the select list (missing
    /// source columns become `NULL`).
    from: &'static str,
    /// Copy phase: tables of one phase run concurrently; a phase starts after
    /// the previous one finished (foreign keys).
    phase: u8,
}

const CS_COLUMNS: &[(&str, Col)] = &[
    ("id", Col::Text),
    ("url", Col::Text),
    ("version", Col::Text),
    ("name", Col::Text),
    ("title", Col::Text),
    ("status", Col::Text),
    ("content", Col::Text),
    ("resource_json", Col::Jsonb),
    ("created_at", Col::Text),
    ("updated_at", Col::Text),
    ("authority_rank", Col::Int4),
    ("version_sort", Col::Text),
    ("current_pin", Col::Int4),
];

const VS_COLUMNS: &[(&str, Col)] = &[
    ("id", Col::Text),
    ("url", Col::Text),
    ("version", Col::Text),
    ("name", Col::Text),
    ("title", Col::Text),
    ("status", Col::Text),
    ("compose_json", Col::Text),
    ("resource_json", Col::Jsonb),
    ("created_at", Col::Text),
    ("updated_at", Col::Text),
    ("authority_rank", Col::Int4),
    ("version_sort", Col::Text),
];

const CM_COLUMNS: &[(&str, Col)] = &[
    ("id", Col::Text),
    ("url", Col::Text),
    ("version", Col::Text),
    ("name", Col::Text),
    ("title", Col::Text),
    ("source_uri", Col::Text),
    ("target_uri", Col::Text),
    ("status", Col::Text),
    ("resource_json", Col::Jsonb),
    ("created_at", Col::Text),
];

fn table_specs() -> Vec<TableSpec> {
    vec![
        TableSpec {
            table: "code_systems",
            columns: CS_COLUMNS,
            from: "SELECT {cols} FROM code_systems",
            phase: 0,
        },
        TableSpec {
            table: "value_sets",
            columns: VS_COLUMNS,
            from: "SELECT {cols} FROM value_sets",
            phase: 0,
        },
        TableSpec {
            table: "concept_maps",
            columns: CM_COLUMNS,
            from: "SELECT {cols} FROM concept_maps",
            phase: 0,
        },
        TableSpec {
            table: "bootstrap_imports",
            columns: &[
                ("path", Col::Text),
                ("content_hash", Col::Text),
                ("size_bytes", Col::Int8),
                ("mtime_unix", Col::Int8),
                ("languages", Col::Text),
                ("imported_at", Col::Timestamptz),
            ],
            from: "SELECT {cols} FROM bootstrap_imports",
            phase: 0,
        },
        TableSpec {
            table: "concepts",
            columns: &[
                ("id", Col::Int8),
                ("system_id", Col::Text),
                ("code", Col::Text),
                ("display", Col::Text),
                ("definition", Col::Text),
            ],
            from: "SELECT {cols} FROM concepts \
                   WHERE system_id IN (SELECT id FROM code_systems)",
            phase: 1,
        },
        TableSpec {
            table: "concept_hierarchy",
            columns: &[
                ("system_id", Col::Text),
                ("parent_code", Col::Text),
                ("child_code", Col::Text),
            ],
            from: "SELECT {cols} FROM concept_hierarchy \
                   WHERE system_id IN (SELECT id FROM code_systems)",
            phase: 1,
        },
        TableSpec {
            table: "concept_closure",
            columns: &[
                ("system_id", Col::Text),
                ("ancestor_code", Col::Text),
                ("descendant_code", Col::Text),
            ],
            from: "SELECT {cols} FROM concept_closure \
                   WHERE system_id IN (SELECT id FROM code_systems)",
            phase: 1,
        },
        TableSpec {
            table: "refset_members",
            columns: &[
                ("system_id", Col::Text),
                ("refset_id", Col::Text),
                ("referenced_code", Col::Text),
            ],
            from: "SELECT {cols} FROM refset_members \
                   WHERE system_id IN (SELECT id FROM code_systems)",
            phase: 1,
        },
        TableSpec {
            table: "value_set_expansions",
            columns: &[
                ("value_set_id", Col::Text),
                ("system_url", Col::Text),
                ("code", Col::Text),
                ("display", Col::Text),
                ("version", Col::Text),
            ],
            from: "SELECT {cols} FROM value_set_expansions \
                   WHERE value_set_id IN (SELECT id FROM value_sets)",
            phase: 1,
        },
        TableSpec {
            table: "concept_map_elements",
            columns: &[
                ("id", Col::Int8),
                ("map_id", Col::Text),
                ("source_system", Col::Text),
                ("source_code", Col::Text),
                ("target_system", Col::Text),
                ("target_code", Col::Text),
                ("equivalence", Col::Text),
            ],
            from: "SELECT {cols} FROM concept_map_elements \
                   WHERE map_id IN (SELECT id FROM concept_maps)",
            phase: 1,
        },
        TableSpec {
            table: "concept_properties",
            columns: &[
                ("id", Col::Int8),
                ("concept_id", Col::Int8),
                ("property", Col::Text),
                ("value_type", Col::Text),
                ("value", Col::Text),
            ],
            from: "SELECT {cols} FROM concept_properties \
                   WHERE concept_id IN (SELECT c.id FROM concepts c \
                                        JOIN code_systems s ON s.id = c.system_id)",
            phase: 2,
        },
        TableSpec {
            table: "concept_designations",
            columns: &[
                ("id", Col::Int8),
                ("concept_id", Col::Int8),
                ("language", Col::Text),
                ("use_system", Col::Text),
                ("use_code", Col::Text),
                ("value", Col::Text),
            ],
            from: "SELECT {cols} FROM concept_designations \
                   WHERE concept_id IN (SELECT c.id FROM concepts c \
                                        JOIN code_systems s ON s.id = c.system_id)",
            phase: 2,
        },
    ]
}

/// HTS tables emptied by `--replace` (children first is not needed: CASCADE).
const HTS_TABLES: &[&str] = &[
    "code_systems",
    "value_sets",
    "concept_maps",
    "bootstrap_imports",
];

/// Sequences advanced past the copied ids.
const SEQUENCES: &[(&str, &str)] = &[
    ("concepts", "id"),
    ("concept_properties", "id"),
    ("concept_designations", "id"),
    ("concept_map_elements", "id"),
];

/// Rows per message from the SQLite reader to the `COPY` writer.
const BATCH: usize = 10_000;

/// A converted cell, ready for binary `COPY`.
enum Cell {
    Text(Option<String>),
    Int8(Option<i64>),
    Int4(Option<i32>),
    Json(Option<serde_json::Value>),
    Ts(Option<chrono::DateTime<chrono::Utc>>),
}

impl Cell {
    fn as_sql(&self) -> &(dyn ToSql + Sync) {
        match self {
            Cell::Text(v) => v,
            Cell::Int8(v) => v,
            Cell::Int4(v) => v,
            Cell::Json(v) => v,
            Cell::Ts(v) => v,
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Damage {
    nul: u64,
    bad_json: u64,
}

fn strip_nul(s: String, d: &mut Damage) -> String {
    if s.contains('\0') {
        d.nul += s.matches('\0').count() as u64;
        s.replace('\0', "")
    } else {
        s
    }
}

fn strip_nul_json(v: &mut serde_json::Value, d: &mut Damage) {
    match v {
        serde_json::Value::String(s) if s.contains('\0') => {
            *s = strip_nul(std::mem::take(s), d);
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(|x| strip_nul_json(x, d)),
        serde_json::Value::Object(o) => {
            let keys: Vec<String> = o.keys().filter(|k| k.contains('\0')).cloned().collect();
            for k in keys {
                if let Some(val) = o.remove(&k) {
                    o.insert(strip_nul(k, d), val);
                }
            }
            o.values_mut().for_each(|x| strip_nul_json(x, d));
        }
        _ => {}
    }
}

fn convert(row: &rusqlite::Row<'_>, i: usize, col: Col, d: &mut Damage) -> rusqlite::Result<Cell> {
    use rusqlite::types::ValueRef;
    let v = row.get_ref(i)?;
    let text = |v: ValueRef<'_>| -> Option<String> {
        match v {
            ValueRef::Null => None,
            ValueRef::Text(t) | ValueRef::Blob(t) => Some(String::from_utf8_lossy(t).into_owned()),
            ValueRef::Integer(n) => Some(n.to_string()),
            ValueRef::Real(f) => Some(f.to_string()),
        }
    };
    Ok(match col {
        Col::Text => Cell::Text(text(v).map(|s| strip_nul(s, d))),
        Col::Int8 => Cell::Int8(match v {
            ValueRef::Integer(n) => Some(n),
            ValueRef::Null => None,
            other => text(other).and_then(|s| s.trim().parse().ok()),
        }),
        Col::Int4 => Cell::Int4(match v {
            ValueRef::Integer(n) => i32::try_from(n).ok(),
            ValueRef::Null => None,
            other => text(other).and_then(|s| s.trim().parse().ok()),
        }),
        Col::Jsonb => {
            Cell::Json(
                text(v).and_then(|s| match serde_json::from_str::<serde_json::Value>(&s) {
                    Ok(mut j) => {
                        strip_nul_json(&mut j, d);
                        Some(j)
                    }
                    Err(_) => {
                        d.bad_json += 1;
                        None
                    }
                }),
            )
        }
        Col::Timestamptz => Cell::Ts(text(v).and_then(|s| parse_sqlite_timestamp(&s))),
    })
}

/// SQLite `datetime('now')` (`YYYY-MM-DD HH:MM:SS`, UTC) or RFC 3339.
fn parse_sqlite_timestamp(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&chrono::Utc));
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
    ] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(t.and_utc());
        }
    }
    None
}

fn open_source(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open SQLite database {} read-only", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(30))?;
    Ok(conn)
}

/// Columns present in a SQLite table (empty when the table does not exist).
fn source_columns(conn: &Connection, table: &str) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(cols)
}

async fn pg_connect(url: &str) -> anyhow::Result<tokio_postgres::Client> {
    let (client, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .context("connect to PostgreSQL")?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            warn!(error = %e, "PostgreSQL connection closed with an error");
        }
    });
    Ok(client)
}

/// Copy one table; returns `(source_rows, copied_rows, damage)`.
async fn copy_table(
    from: PathBuf,
    to: String,
    spec: &TableSpec,
    fast_load: bool,
) -> anyhow::Result<(u64, u64, Damage)> {
    let source = open_source(&from)?;
    let present = source_columns(&source, spec.table)?;
    if present.is_empty() {
        info!(table = spec.table, "not in the source database — skipped");
        return Ok((0, 0, Damage::default()));
    }
    let select: Vec<String> = spec
        .columns
        .iter()
        .map(|(c, _)| {
            if present.iter().any(|p| p == c) {
                format!("\"{c}\"")
            } else {
                format!("NULL AS \"{c}\"")
            }
        })
        .collect();
    let sql = spec.from.replace("{cols}", &select.join(", "));
    let count_sql = format!("SELECT COUNT(*) FROM ({sql})");
    let source_rows: i64 = source.query_row(&count_sql, [], |r| r.get(0))?;
    drop(source);

    let client = pg_connect(&to).await?;
    if fast_load {
        // Skip FK triggers for this bulk load (superuser only); the source
        // rows were filtered to rows whose parents exist.
        let _ = client
            .batch_execute("SET session_replication_role = replica")
            .await;
    }
    let columns: Vec<&str> = spec.columns.iter().map(|(c, _)| *c).collect();
    let types: Vec<Type> = spec.columns.iter().map(|(_, t)| t.pg_type()).collect();
    let copy_sql = format!(
        "COPY {} ({}) FROM STDIN (FORMAT binary)",
        spec.table,
        columns.join(", ")
    );
    let sink = client
        .copy_in(&copy_sql)
        .await
        .with_context(|| format!("COPY into {}", spec.table))?;
    let writer = BinaryCopyInWriter::new(sink, &types);
    pin_mut!(writer);

    // The SQLite reader runs on a blocking thread and hands over batches.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<Vec<Cell>>>(4);
    let cols: Vec<Col> = spec.columns.iter().map(|(_, t)| *t).collect();
    let reader = tokio::task::spawn_blocking(move || -> anyhow::Result<Damage> {
        let source = open_source(&from)?;
        let mut damage = Damage::default();
        let mut stmt = source.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        let mut batch = Vec::with_capacity(BATCH);
        while let Some(row) = rows.next()? {
            let mut cells = Vec::with_capacity(cols.len());
            for (i, c) in cols.iter().enumerate() {
                cells.push(convert(row, i, *c, &mut damage)?);
            }
            batch.push(cells);
            if batch.len() == BATCH {
                if tx.blocking_send(std::mem::take(&mut batch)).is_err() {
                    bail!("COPY writer stopped");
                }
                batch.reserve(BATCH);
            }
        }
        if !batch.is_empty() && tx.blocking_send(batch).is_err() {
            bail!("COPY writer stopped");
        }
        Ok(damage)
    });

    let mut copied = 0u64;
    while let Some(batch) = rx.recv().await {
        for cells in &batch {
            let refs: Vec<&(dyn ToSql + Sync)> = cells.iter().map(Cell::as_sql).collect();
            writer
                .as_mut()
                .write(&refs)
                .await
                .with_context(|| format!("COPY row into {}", spec.table))?;
            copied += 1;
        }
    }
    let damage = reader.await??;
    let written = writer
        .finish()
        .await
        .with_context(|| format!("finish COPY into {}", spec.table))?;
    if written != copied {
        warn!(
            table = spec.table,
            written, copied, "COPY row count differs"
        );
    }
    Ok((source_rows.max(0) as u64, written, damage))
}

/// Drop the plain (non-constraint) indexes of the HTS tables; the schema DDL
/// re-creates them after the load (`CREATE INDEX IF NOT EXISTS`).
async fn drop_secondary_indexes(client: &tokio_postgres::Client) -> anyhow::Result<u64> {
    let rows = client
        .query(
            "SELECT i.relname
               FROM pg_index x
               JOIN pg_class i ON i.oid = x.indexrelid
               JOIN pg_class t ON t.oid = x.indrelid
               JOIN pg_namespace n ON n.oid = t.relnamespace
              WHERE n.nspname = current_schema()
                AND t.relname = ANY($1)
                AND NOT EXISTS (SELECT 1 FROM pg_constraint c WHERE c.conindid = x.indexrelid)",
            &[&[
                "concepts",
                "concept_hierarchy",
                "concept_closure",
                "concept_properties",
                "concept_designations",
                "value_set_expansions",
                "concept_map_elements",
                "refset_members",
            ]
            .as_slice()],
        )
        .await?;
    for r in &rows {
        let name: String = r.get(0);
        client
            .batch_execute(&format!("DROP INDEX IF EXISTS \"{name}\""))
            .await?;
    }
    Ok(rows.len() as u64)
}

/// Copy the persistence store's live resources (latest versions).
async fn copy_resources(from: &Path, to: &str) -> anyhow::Result<u64> {
    use helios_persistence::ResourceStorage;
    use helios_persistence::backends::postgres::PostgresBackend;
    use helios_persistence::tenant::TenantContext;

    let source = open_source(from)?;
    if source_columns(&source, "resources")?.is_empty() {
        return Ok(0);
    }
    let rows: Vec<(String, String, Vec<u8>, Option<String>)> = {
        let mut stmt = source.prepare(
            "SELECT resource_type, id, data, fhir_version FROM resources \
             WHERE COALESCE(is_deleted, 0) = 0",
        )?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<Vec<_>, _>>()?
    };
    drop(source);
    if rows.is_empty() {
        return Ok(0);
    }
    let store = PostgresBackend::from_connection_string(to)
        .await
        .map_err(|e| anyhow::anyhow!("open PostgreSQL resource store: {e}"))?;
    store
        .init_schema()
        .await
        .map_err(|e| anyhow::anyhow!("init PostgreSQL resource store schema: {e}"))?;
    let ctx = TenantContext::system();
    let mut n = 0u64;
    for (rtype, id, data, fhir_version) in rows {
        let json: serde_json::Value =
            serde_json::from_slice(&data).with_context(|| format!("parse stored {rtype}/{id}"))?;
        let version = fhir_version
            .as_deref()
            .and_then(helios_fhir::FhirVersion::from_storage)
            .unwrap_or_else(helios_fhir::FhirVersion::default_enabled);
        store
            .create_or_update(&ctx, &rtype, &id, json, version)
            .await
            .map_err(|e| anyhow::anyhow!("store {rtype}/{id}: {e}"))?;
        n += 1;
    }
    Ok(n)
}

/// Copy the SQLite database `opts.from` into the PostgreSQL database
/// `opts.to` (see the module docs).
pub async fn migrate_sqlite_to_postgres(opts: &MigrateOptions) -> anyhow::Result<MigrateReport> {
    let t0 = Instant::now();
    if !opts.from.exists() {
        bail!("{} does not exist", opts.from.display());
    }

    // Schema (and its migrations) on the target.
    let backend = crate::backends::PostgresTerminologyBackend::new(&opts.to)
        .await
        .map_err(|e| anyhow::anyhow!("prepare PostgreSQL schema: {e}"))?;
    backend.pool().close();
    drop(backend);

    let client = pg_connect(&opts.to).await?;
    let populated: bool = client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM code_systems) OR EXISTS(SELECT 1 FROM value_sets) \
             OR EXISTS(SELECT 1 FROM concept_maps)",
            &[],
        )
        .await?
        .get(0);
    if populated && !opts.replace {
        bail!(
            "the target PostgreSQL database already holds terminology; pass --replace to \
             overwrite it"
        );
    }
    if opts.replace {
        info!("emptying the target HTS tables (--replace)");
        client
            .batch_execute(&format!("TRUNCATE {} CASCADE", HTS_TABLES.join(", ")))
            .await?;
    }
    // The schema DDL plants its one-shot migration markers in the ledger of
    // a fresh database; the source ledger replaces them.
    client
        .batch_execute("DELETE FROM bootstrap_imports")
        .await?;
    let superuser: bool = client
        .query_one(
            "SELECT rolsuper FROM pg_roles WHERE rolname = current_user",
            &[],
        )
        .await
        .map(|r| r.get(0))
        .unwrap_or(false);
    let dropped = drop_secondary_indexes(&client).await?;
    info!(
        dropped_indexes = dropped,
        fk_triggers_off = superuser,
        "bulk load prepared"
    );

    let specs = table_specs();
    let mut tables = Vec::new();
    let mut damage = Damage::default();
    for phase in 0..=2u8 {
        let in_phase: Vec<&TableSpec> = specs.iter().filter(|s| s.phase == phase).collect();
        let mut pending = in_phase.into_iter();
        let mut running = futures::stream::FuturesUnordered::new();
        loop {
            while running.len() < opts.jobs.max(1) {
                let Some(spec) = pending.next() else { break };
                let (from, to) = (opts.from.clone(), opts.to.clone());
                running.push(async move {
                    let t = Instant::now();
                    info!(table = spec.table, "copying");
                    let r = copy_table(from, to, spec, superuser).await;
                    (spec.table, t.elapsed().as_secs_f64(), r)
                });
            }
            use futures::StreamExt;
            let Some((table, secs, res)) = running.next().await else {
                break;
            };
            let (source_rows, copied_rows, d) =
                res.with_context(|| format!("copy table {table}"))?;
            damage.nul += d.nul;
            damage.bad_json += d.bad_json;
            info!(
                table,
                source_rows,
                copied_rows,
                secs = format!("{secs:.1}"),
                "copied"
            );
            tables.push(TableReport {
                table: table.to_owned(),
                source_rows,
                copied_rows,
                seconds: secs,
            });
        }
    }

    for (table, col) in SEQUENCES {
        client
            .batch_execute(&format!(
                "SELECT setval(pg_get_serial_sequence('{table}', '{col}'), \
                 GREATEST((SELECT MAX({col}) FROM {table}), 1))"
            ))
            .await?;
    }

    info!("re-creating indexes");
    let t_idx = Instant::now();
    crate::backends::postgres::schema::apply_ddl(&client)
        .await
        .map_err(|e| anyhow::anyhow!("re-create indexes: {e}"))?;
    let idx_secs = t_idx.elapsed().as_secs_f64();
    info!(secs = format!("{idx_secs:.1}"), "indexes re-created");

    let resources = if opts.skip_resources {
        0
    } else {
        copy_resources(&opts.from, &opts.to).await?
    };

    info!("ANALYZE");
    client.batch_execute("ANALYZE").await?;

    // Closures the source did not have yet (e.g. a system imported while the
    // SQLite server was down) are built now rather than on first start.
    let backend = crate::backends::PostgresTerminologyBackend::new(&opts.to)
        .await
        .map_err(|e| anyhow::anyhow!("reopen PostgreSQL backend: {e}"))?;
    backend
        .rebuild_missing_closures()
        .await
        .map_err(|e| anyhow::anyhow!("rebuild missing closures: {e}"))?;

    tables.push(TableReport {
        table: "(indexes)".into(),
        source_rows: 0,
        copied_rows: 0,
        seconds: idx_secs,
    });
    let mut by_name: HashMap<String, TableReport> = HashMap::new();
    for t in tables {
        by_name.insert(t.table.clone(), t);
    }
    let order: Vec<&str> = specs
        .iter()
        .map(|s| s.table)
        .chain(std::iter::once("(indexes)"))
        .collect();
    let tables = order
        .into_iter()
        .filter_map(|n| by_name.remove(n))
        .collect();
    Ok(MigrateReport {
        tables,
        resources,
        nul_chars_removed: damage.nul,
        json_unparseable: damage.bad_json,
        seconds: t0.elapsed().as_secs_f64(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_timestamps_parse_as_utc() {
        let t = parse_sqlite_timestamp("2026-10-14 09:43:02").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-10-14T09:43:02+00:00");
        assert!(parse_sqlite_timestamp("2026-10-13T02:42:39.745456419+00:00").is_some());
        assert!(parse_sqlite_timestamp("yesterday").is_none());
    }

    #[test]
    fn nul_characters_are_removed_and_counted() {
        let mut d = Damage::default();
        assert_eq!(strip_nul("a\0b\0".into(), &mut d), "ab");
        let mut j = serde_json::json!({"k\0": ["x\0y", {"z": "\0"}]});
        strip_nul_json(&mut j, &mut d);
        assert_eq!(j, serde_json::json!({"k": ["xy", {"z": ""}]}));
        assert_eq!(d.nul, 5);
    }
}
