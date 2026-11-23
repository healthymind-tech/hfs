//! One database: the schema plan (design §5).
//!
//! PostgreSQL: the terminology store — HTS's own tables **and** the
//! helios-persistence resource store HTS embeds for raw CodeSystem /
//! ValueSet / ConceptMap resources — lives in schema [`TERMINOLOGY_SCHEMA`]
//! of the HFS database. FHIR data, console, endpoints, sync, sessions and
//! audit stay in `public`. The terminology pools connect with
//! `search_path = terminology, public` (the extensions `pg_trgm` /
//! `btree_gin` live in `public` and are found through the second entry), so
//! HTS's unqualified SQL lands in its own schema without a code change.
//!
//! The fall-through to `public` is the one hazard: an unqualified
//! `to_regclass('x')` or an `information_schema` lookup without a schema
//! filter finds HFS's object when the terminology one is missing. Package U1
//! makes every such lookup schema-qualified (`current_schema()`) and adds
//! the isolation test (design §5.3); [`SCHEMA_SENSITIVE_LOOKUPS`] lists the
//! sites known at design time.
//!
//! SQLite: terminology keeps its own file next to the FHIR database
//! (`terminology.db`), opened by the same process.

use std::borrow::Cow;

/// Default schema of the terminology store (`HFS_TERMINOLOGY_SCHEMA`).
pub const TERMINOLOGY_SCHEMA: &str = "terminology";

/// Schemas that must never be used for terminology.
pub const RESERVED_SCHEMAS: &[&str] = &["public", "pg_catalog", "information_schema", "pg_toast"];

/// Fork-migration block owned by the unified server (design §5.4). Names
/// are recorded in `hm_schema_migrations`; numbers are allocated, not
/// necessarily all used. `hm-0020`..`hm-0029` belong to the built-in
/// authorization server (docs/design/authorization-server.md).
pub const UNIFIED_MIGRATION_BLOCK: (u16, u16) = (11, 19);
/// Reserved for the built-in authorization server (line IdP).
pub const IDP_MIGRATION_BLOCK: (u16, u16) = (20, 29);

/// Fork migrations of the unified server, in order.
pub const UNIFIED_FORK_MIGRATIONS: &[&str] = &[
    // CREATE SCHEMA IF NOT EXISTS terminology (owner = the HFS role) +
    // hfs_terminology_state (one row: store location, data-move status).
    "hm-0011-terminology-schema",
    // Console role bindings gain the `terminology` scope (design §7.2).
    "hm-0012-console-terminology-bindings",
];

/// SQL of `hm-0011-terminology-schema` (idempotent). `{schema}` is
/// substituted after [`validate_schema_name`].
pub const HM_0011_SQL: &str = "\
CREATE SCHEMA IF NOT EXISTS {schema};
CREATE TABLE IF NOT EXISTS hfs_terminology_state (
    id          BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
    store       TEXT NOT NULL,            -- 'schema:terminology' | 'external-db' | 'sqlite-file'
    moved_from  TEXT,                     -- e.g. 'database:hts' after hfs-unify-db
    moved_at    TIMESTAMPTZ,
    source_fingerprint TEXT,              -- row counts + hts_schema_meta fingerprint of the source
    note        TEXT,                     -- hfs-unify-db progress: last completed step
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
ALTER TABLE hfs_terminology_state ADD COLUMN IF NOT EXISTS note TEXT;";

/// HTS-owned tables (crates/hts/src/backends/postgres/schema.rs) — what the
/// data move verifies table by table (row counts).
pub const TERMINOLOGY_TABLES: &[&str] = &[
    "code_systems",
    "concepts",
    "concept_hierarchy",
    "concept_properties",
    "concept_designations",
    "value_sets",
    "value_set_expansions",
    "value_set_dependencies",
    "concept_maps",
    "concept_map_elements",
    "concept_closure",
    "refset_members",
    "snomed_concepts",
    "snomed_relationships",
    "snomed_mrcm",
    "snomed_model_stamps",
    "bootstrap_imports",
    "terminology_epoch",
    "hts_schema_meta",
];

/// helios-persistence tables HTS's embedded resource store also creates in
/// the terminology schema (verified by count too).
pub const EMBEDDED_STORE_TABLES: &[&str] = &[
    "resources",
    "resource_history",
    "search_index",
    "schema_version",
    "hm_schema_migrations",
];

/// Lookups that resolve through `search_path` and would fall through to
/// `public` (design §5.3). Package U1 qualifies each and the isolation test
/// guards the class.
pub const SCHEMA_SENSITIVE_LOOKUPS: &[&str] = &[
    // U1: all qualified with current_schema(); the class is guarded by
    // crates/persistence/tests/terminology_schema.rs (static scan + PG test).
    "persistence postgres/schema.rs: to_regclass('search_parameter_epochs'|'idx_resources_reindex_id')",
    "persistence postgres/schema.rs: to_regclass('hm_schema_migrations')",
    "persistence postgres/schema.rs: to_regclass('schema_version')",
    "persistence postgres/schema.rs: information_schema.columns / key_column_usage without table_schema",
    "hts postgres/schema.rs: 4 information_schema/pg_class guards in DO blocks",
    "persistence advisory lock 0x48465300_4d494752 is per database: HFS and the embedded store serialise their migrations (harmless, noted)",
];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SchemaError {
    #[error("schema name {0:?} must match ^[a-z_][a-z0-9_]{{0,62}}$ and not be reserved")]
    BadName(String),
    #[error("database URL already sets `options`; set HFS_TERMINOLOGY_DATABASE_URL explicitly")]
    OptionsAlreadySet,
    #[error("database URL could not be parsed: {0}")]
    BadUrl(String),
}

pub fn validate_schema_name(name: &str) -> Result<(), SchemaError> {
    let mut chars = name.chars();
    let ok_first = matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_');
    let ok_rest = name.len() <= 63
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok_first && ok_rest && !RESERVED_SCHEMAS.contains(&name) && !name.starts_with("pg_") {
        Ok(())
    } else {
        Err(SchemaError::BadName(name.to_string()))
    }
}

/// `HM_0011_SQL` for a validated schema.
pub fn hm_0011_sql(schema: &str) -> Result<String, SchemaError> {
    validate_schema_name(schema)?;
    Ok(HM_0011_SQL.replace("{schema}", schema))
}

pub fn is_postgres_url(url: &str) -> bool {
    let u = url.trim_start();
    u.starts_with("postgres://") || u.starts_with("postgresql://") || u.contains("host=")
}

/// The search path the terminology pools use.
pub fn search_path(schema: &str) -> String {
    format!("{schema},public")
}

/// Add `options=-c search_path=<schema>,public` to a PostgreSQL connection
/// string, in URI form (`postgres://…?…`) or key/value form (`host=… …`),
/// which tokio-postgres both accept.
pub fn with_search_path(url: &str, schema: &str) -> Result<String, SchemaError> {
    validate_schema_name(schema)?;
    let option = format!("-c search_path={}", search_path(schema));
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        let mut parsed = url::Url::parse(url).map_err(|e| SchemaError::BadUrl(e.to_string()))?;
        if parsed.query_pairs().any(|(k, _)| k == "options") {
            return Err(SchemaError::OptionsAlreadySet);
        }
        // Percent-encode like libpq expects (space as %20, not +).
        let encoded: Cow<'_, str> = Cow::Owned(
            option
                .bytes()
                .map(|b| match b {
                    b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                        (b as char).to_string()
                    }
                    other => format!("%{other:02X}"),
                })
                .collect(),
        );
        let query = match parsed.query() {
            Some(q) if !q.is_empty() => format!("{q}&options={encoded}"),
            _ => format!("options={encoded}"),
        };
        parsed.set_query(Some(&query));
        Ok(parsed.to_string())
    } else {
        if url.split_whitespace().any(|kv| kv.starts_with("options=")) {
            return Err(SchemaError::OptionsAlreadySet);
        }
        Ok(format!("{} options='{}'", url.trim_end(), option))
    }
}

/// SQLite: `terminology.db` next to the FHIR database file. `None` for
/// in-memory databases (the embedded service then uses its own in-memory
/// store) or unrecognised forms.
pub fn sqlite_terminology_url(hfs_url: &str) -> Option<String> {
    let path = hfs_url
        .strip_prefix("sqlite://")
        .or_else(|| hfs_url.strip_prefix("sqlite:"))
        .unwrap_or(hfs_url);
    if path.is_empty() || path.contains(":memory:") || path.contains("mode=memory") {
        return None;
    }
    let path = path.split('?').next().unwrap_or(path);
    let file = std::path::Path::new(path);
    let dir = file.parent().filter(|p| !p.as_os_str().is_empty());
    let target = match dir {
        Some(d) => d.join("terminology.db"),
        None => std::path::PathBuf::from("terminology.db"),
    };
    Some(target.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_names() {
        assert!(validate_schema_name("terminology").is_ok());
        assert!(validate_schema_name("tx_2").is_ok());
        for bad in ["public", "pg_x", "Terminology", "1tx", "tx;drop", "", "a b"] {
            assert!(validate_schema_name(bad).is_err(), "{bad}");
        }
        assert!(
            hm_0011_sql("terminology")
                .unwrap()
                .starts_with("CREATE SCHEMA IF NOT EXISTS terminology;")
        );
    }

    #[test]
    fn search_path_on_uri_and_keyvalue_urls() {
        assert_eq!(
            with_search_path("postgres://hfs:pw@hfs-pg:5432/hfs", "terminology").unwrap(),
            "postgres://hfs:pw@hfs-pg:5432/hfs?options=-c%20search_path%3Dterminology%2Cpublic"
        );
        assert_eq!(
            with_search_path("postgresql://h/hfs?sslmode=disable", "terminology").unwrap(),
            "postgresql://h/hfs?sslmode=disable&options=-c%20search_path%3Dterminology%2Cpublic"
        );
        assert_eq!(
            with_search_path("host=db user=hfs dbname=hfs", "terminology").unwrap(),
            "host=db user=hfs dbname=hfs options='-c search_path=terminology,public'"
        );
        assert_eq!(
            with_search_path("postgres://h/hfs?options=-c%20x%3Dy", "terminology"),
            Err(SchemaError::OptionsAlreadySet)
        );
        assert!(with_search_path("postgres://h/hfs", "public").is_err());
    }

    #[test]
    fn sqlite_sibling_file() {
        assert_eq!(
            sqlite_terminology_url("/data/fhir.db").as_deref(),
            Some("/data/terminology.db")
        );
        assert_eq!(
            sqlite_terminology_url("sqlite:///data/fhir.db?mode=rwc").as_deref(),
            Some("/data/terminology.db")
        );
        assert_eq!(
            sqlite_terminology_url("fhir.db").as_deref(),
            Some("terminology.db")
        );
        assert_eq!(sqlite_terminology_url(":memory:"), None);
    }

    #[test]
    fn migration_blocks_do_not_overlap() {
        assert!(UNIFIED_MIGRATION_BLOCK.1 < IDP_MIGRATION_BLOCK.0);
        for name in UNIFIED_FORK_MIGRATIONS {
            let n: u16 = name[3..7].parse().unwrap();
            assert!((UNIFIED_MIGRATION_BLOCK.0..=UNIFIED_MIGRATION_BLOCK.1).contains(&n));
        }
        // No terminology table collides with a persistence table name the
        // HFS schema owns in `public` except the embedded store's own copies.
        for t in TERMINOLOGY_TABLES {
            assert!(!EMBEDDED_STORE_TABLES.contains(t));
        }
    }
}
