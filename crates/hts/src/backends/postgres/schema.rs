//! PostgreSQL DDL for the HTS schema.
//!
//! Uses `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF NOT EXISTS` throughout
//! so this is safe to re-apply on every startup (idempotent).
//!
//! Key differences from the SQLite schema:
//! - `INTEGER PRIMARY KEY` → `BIGSERIAL PRIMARY KEY` for auto-increment tables
//! - `resource_json TEXT` → `resource_json JSONB`
//! - `title TEXT` and `resource_json JSONB` are part of the base schema (no migrations)
//! - `name TEXT` is included in `concept_maps` from the start
//! - Trigram GIN index on `concepts.display` for fast substring search

/// Full PostgreSQL DDL for the HTS normalized schema.
pub const SCHEMA: &str = "
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- ── Code Systems ──────────────────────────────────────────────────────────────
-- Multi-version: a canonical URL may have multiple rows so long as each row has
-- a distinct `version`. Uniqueness is enforced via the composite expression
-- index `idx_code_systems_url_version` (the column-level UNIQUE on `url` was
-- dropped). Rows with NULL version are coalesced to the empty string by the
-- index so two un-versioned imports of the same URL still collide.
CREATE TABLE IF NOT EXISTS code_systems (
    id           TEXT PRIMARY KEY,
    url          TEXT NOT NULL,
    version      TEXT,
    name         TEXT,
    title        TEXT,
    status       TEXT NOT NULL DEFAULT 'active',
    content      TEXT NOT NULL DEFAULT 'complete',
    resource_json JSONB,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    -- Provenance precedence among rows sharing `url`; lower wins. 0 = the source
    -- owns this canonical URL (or came from outside any package); 2 = a package
    -- re-published someone else's canonical (e.g. hl7.fhir.r4.core shipping a
    -- truncated copy of a terminology.hl7.org CodeSystem).
    --
    -- Deliberately NULLABLE with no default: NULL means 'no source has claimed
    -- this row yet', which is what every pre-existing row looks like right after
    -- the column is added. Readers COALESCE it to 0, so an un-re-imported database
    -- behaves exactly as before. A DEFAULT 0 would make 'asserted authoritative'
    -- indistinguishable from 'never asserted', and a re-imported copy could then
    -- never demote itself.
    authority_rank INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_code_systems_url_version
    ON code_systems(url, COALESCE(version, ''));
CREATE INDEX IF NOT EXISTS idx_code_systems_url ON code_systems(url);
-- CRUD and `/CodeSystem/{id}` resolve a FHIR id through the stored resource
-- (`resource_json->>'id'`), since multi-version rows carry synthetic ids.
CREATE INDEX IF NOT EXISTS idx_code_systems_json_id ON code_systems ((resource_json->>'id'));
CREATE INDEX IF NOT EXISTS idx_code_systems_id_base ON code_systems (split_part(id, '|', 1));
-- A ValueSet URL that names no stored ValueSet is tried as a CodeSystem's
-- `valueSet` (its implicit all-codes ValueSet): an index probe instead of
-- reading every CodeSystem's JSON.
CREATE INDEX IF NOT EXISTS idx_code_systems_json_valueset ON code_systems ((resource_json->>'valueSet'));

-- Legacy installs created code_systems with `url TEXT NOT NULL UNIQUE`, which
-- bakes uniqueness into a hidden constraint that blocks multi-version imports.
-- Drop both the standalone constraint name and the auto-named one so subsequent
-- (url, version) duplicates can coexist; both forms are silently ignored when
-- absent (legacy index is unnamed across PG versions).
DO $$
DECLARE
    cons_name text;
BEGIN
    FOR cons_name IN
        SELECT conname FROM pg_constraint c
        JOIN pg_class t ON t.oid = c.conrelid
        WHERE t.relname = 'code_systems'
          AND c.contype = 'u'
          AND pg_get_constraintdef(c.oid) = 'UNIQUE (url)'
    LOOP
        EXECUTE format('ALTER TABLE code_systems DROP CONSTRAINT %I', cons_name);
    END LOOP;
END $$;

-- ── Concepts ───────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS concepts (
    id         BIGSERIAL PRIMARY KEY,
    system_id  TEXT NOT NULL REFERENCES code_systems(id) ON DELETE CASCADE,
    code       TEXT NOT NULL,
    display    TEXT,
    definition TEXT,
    UNIQUE (system_id, code)
);
-- The UNIQUE (system_id, code) constraint already creates a backing index on
-- exactly those columns, so the old explicit idx_concepts_system_code was a
-- redundant duplicate that doubled index maintenance on every concept insert.
-- Drop it on startup (idempotent); the constraint index serves all the same
-- lookups.
DROP INDEX IF EXISTS idx_concepts_system_code;
CREATE INDEX IF NOT EXISTS idx_concepts_display_trgm ON concepts USING gin(display gin_trgm_ops);

-- ── Hierarchy (pre-materialized parent-child links) ───────────────────────────
CREATE TABLE IF NOT EXISTS concept_hierarchy (
    system_id   TEXT NOT NULL REFERENCES code_systems(id) ON DELETE CASCADE,
    parent_code TEXT NOT NULL,
    child_code  TEXT NOT NULL,
    PRIMARY KEY (system_id, parent_code, child_code)
);
CREATE INDEX IF NOT EXISTS idx_hierarchy_child ON concept_hierarchy(system_id, child_code);

-- ── Concept Properties (arbitrary FHIR properties) ────────────────────────────
CREATE TABLE IF NOT EXISTS concept_properties (
    id         BIGSERIAL PRIMARY KEY,
    concept_id BIGINT NOT NULL REFERENCES concepts(id) ON DELETE CASCADE,
    property   TEXT NOT NULL,
    value_type TEXT NOT NULL,
    value      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_props_concept ON concept_properties(concept_id);
-- Forward lookup: concepts carrying a given (property, value) pair. Drives
-- compose `filter[]` property-equality expansion (EX05's `=` / `in` filters).
-- Without this index `pg_filter_property_eq` had to scan every concept in the
-- system (~350k for SNOMED) and probe `concept_properties` per row. The
-- trailing `concept_id` lets the (property, value) probe stay index-only.
CREATE INDEX IF NOT EXISTS idx_props_property_value
    ON concept_properties(property, value, concept_id);

-- ── Designations (alternate names / translations) ─────────────────────────────
CREATE TABLE IF NOT EXISTS concept_designations (
    id         BIGSERIAL PRIMARY KEY,
    concept_id BIGINT NOT NULL REFERENCES concepts(id) ON DELETE CASCADE,
    language   TEXT,
    use_system TEXT,
    use_code   TEXT,
    value      TEXT NOT NULL
);
-- Index on concept_id is essential: Postgres does not auto-index foreign keys,
-- so without it each per-concept designation read and the delete-before-reinsert
-- that every import performs is a sequential scan, making a bulk import of a
-- designation-heavy source (SNOMED CT, LOINC) O(n²). (concept_properties has the
-- equivalent idx_props_concept.)
CREATE INDEX IF NOT EXISTS idx_designations_concept
    ON concept_designations(concept_id);

-- ── Value Sets ─────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS value_sets (
    id            TEXT PRIMARY KEY,
    url           TEXT NOT NULL,
    version       TEXT,
    name          TEXT,
    title         TEXT,
    status        TEXT NOT NULL DEFAULT 'active',
    compose_json  TEXT,
    resource_json JSONB,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    -- See code_systems.authority_rank.
    authority_rank INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_value_sets_url_version
    ON value_sets(url, COALESCE(version, ''));
CREATE INDEX IF NOT EXISTS idx_value_sets_url ON value_sets(url);
-- FHIR-id resolution of versioned rows (`<id>|<version>` storage ids).
CREATE INDEX IF NOT EXISTS idx_value_sets_json_id ON value_sets ((resource_json->>'id'));
CREATE INDEX IF NOT EXISTS idx_value_sets_id_base ON value_sets (split_part(id, '|', 1));

-- Drop any legacy column-level UNIQUE on value_sets.url for the same reason
-- code_systems above had it removed: tx-ecosystem ships per-version VS
-- fixtures sharing a canonical URL.
DO $$
DECLARE
    cons_name text;
BEGIN
    FOR cons_name IN
        SELECT conname FROM pg_constraint c
        JOIN pg_class t ON t.oid = c.conrelid
        WHERE t.relname = 'value_sets'
          AND c.contype = 'u'
          AND pg_get_constraintdef(c.oid) = 'UNIQUE (url)'
    LOOP
        EXECUTE format('ALTER TABLE value_sets DROP CONSTRAINT %I', cons_name);
    END LOOP;
END $$;

-- Metadata listing (`GET /CodeSystem`, `/ValueSet`) orders by created_at.
CREATE INDEX IF NOT EXISTS idx_code_systems_created_at ON code_systems(created_at);
CREATE INDEX IF NOT EXISTS idx_value_sets_created_at ON value_sets(created_at);

-- ── Value Set Expansions (materialized cache) ─────────────────────────────────
-- `version` carries the resolved CodeSystem version for each cached entry so
-- that callers reading from the cache can echo the right CS version on
-- validate-code and so the version-pinned overload candidate-selection branch
-- has the data it needs to discriminate. The PRIMARY KEY excludes `version`
-- because PG PK columns must be NOT NULL — multi-version overload composes
-- bypass the cache (`compose_has_multi_version_pins`) instead of relying on PK
-- dedupe behaviour.
CREATE TABLE IF NOT EXISTS value_set_expansions (
    value_set_id TEXT NOT NULL REFERENCES value_sets(id) ON DELETE CASCADE,
    system_url   TEXT NOT NULL,
    code         TEXT NOT NULL,
    display      TEXT,
    version      TEXT,
    PRIMARY KEY (value_set_id, system_url, code)
);
-- Legacy installs may have a value_set_expansions table without the `version`
-- column; add it idempotently so upgrades round-trip.
ALTER TABLE value_set_expansions ADD COLUMN IF NOT EXISTS version TEXT;
-- Re-importing or deleting a CodeSystem drops the cached expansions over it.
CREATE INDEX IF NOT EXISTS idx_value_set_expansions_system ON value_set_expansions(system_url);

-- ── Concept Maps ───────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS concept_maps (
    id            TEXT PRIMARY KEY,
    url           TEXT NOT NULL UNIQUE,
    version       TEXT,
    name          TEXT,
    title         TEXT,
    source_uri    TEXT,
    target_uri    TEXT,
    status        TEXT NOT NULL DEFAULT 'active',
    resource_json JSONB,
    created_at    TEXT NOT NULL
);

-- ── Concept Map Elements ───────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS concept_map_elements (
    id            BIGSERIAL PRIMARY KEY,
    map_id        TEXT NOT NULL REFERENCES concept_maps(id) ON DELETE CASCADE,
    source_system TEXT NOT NULL,
    source_code   TEXT NOT NULL,
    target_system TEXT NOT NULL,
    target_code   TEXT NOT NULL,
    equivalence   TEXT NOT NULL DEFAULT 'equivalent'
);
CREATE INDEX IF NOT EXISTS idx_map_source
    ON concept_map_elements(map_id, source_system, source_code);
-- Forward and reverse $translate by code without a map url (the common
-- `system`+`code` form): without these every such request scans every
-- ConceptMap element. Same indexes as the SQLite schema.
CREATE INDEX IF NOT EXISTS idx_map_elements_source_code
    ON concept_map_elements(source_code, source_system, map_id);
CREATE INDEX IF NOT EXISTS idx_map_elements_target_code
    ON concept_map_elements(target_code, target_system, map_id);

-- ── Transitive ancestor closure ────────────────────────────────────────────────
-- Precomputed (ancestor, descendant) pairs for every code system, including
-- self-links (code, code). Populated at import time for each code system so
-- is-a, descendent-of, generalizes, and $subsumes queries are O(1) index
-- lookups rather than O(depth) recursive CTEs. Mirrors the SQLite
-- concept_closure table at backends/sqlite/schema.rs.
CREATE TABLE IF NOT EXISTS concept_closure (
    system_id       TEXT NOT NULL REFERENCES code_systems(id) ON DELETE CASCADE,
    ancestor_code   TEXT NOT NULL,
    descendant_code TEXT NOT NULL,
    PRIMARY KEY (system_id, ancestor_code, descendant_code)
);
-- Reverse lookup: all ancestors of a given descendant code.
CREATE INDEX IF NOT EXISTS idx_closure_descendant
    ON concept_closure(system_id, descendant_code);

-- ── Reference set membership ──────────────────────────────────────────────────
-- Active members of SNOMED CT Simple reference sets (RF2
-- der2_Refset_Simple*): ECL `^ refset` and `?fhir_vs=refset/<id>` read it.
-- Replaced per system on re-import; goes with the CodeSystem row. Mirrors the
-- SQLite table of the same name.
CREATE TABLE IF NOT EXISTS refset_members (
    system_id       TEXT NOT NULL REFERENCES code_systems(id) ON DELETE CASCADE,
    refset_id       TEXT NOT NULL,
    referenced_code TEXT NOT NULL,
    PRIMARY KEY (system_id, refset_id, referenced_code)
);

-- ── Bootstrap import ledger ───────────────────────────────────────────────────
-- Records which files in HTS_BOOTSTRAP_DIR have already been imported, keyed on
-- file name with the SHA-256 of the file's contents. On every startup the
-- bootstrap sync stats each file in the directory and skips re-importing those
-- whose (size_bytes, mtime_unix) match the recorded values, falling back to a
-- full content hash only when the cheap stat disagrees — so unchanged files
-- (including multi-GB SNOMED/LOINC archives) are recognized without re-reading
-- their contents, while newly added files and updated terminology releases are
-- still picked up. `languages` records the BCP-47 filter that was applied so a
-- changed HTS_IMPORT_LANGUAGES re-triggers import of affected files.
CREATE TABLE IF NOT EXISTS bootstrap_imports (
    path          TEXT PRIMARY KEY,
    content_hash  TEXT NOT NULL,
    size_bytes    BIGINT NOT NULL,
    mtime_unix    BIGINT,
    languages     TEXT NOT NULL DEFAULT '',
    imported_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- Bring pre-existing ledgers up to the current shape (idempotent).
ALTER TABLE bootstrap_imports ADD COLUMN IF NOT EXISTS mtime_unix BIGINT;
ALTER TABLE bootstrap_imports ADD COLUMN IF NOT EXISTS languages TEXT NOT NULL DEFAULT '';

-- ── Provenance backfill ────────────────────────────────────────────────────────
-- `authority_rank` records whether the package that shipped a row actually owns
-- the canonical URL it claims. It cannot be derived after the fact — nothing
-- already stored on the row says which package supplied it — so a database that
-- predates the column must re-import its packages to learn the truth.
--
-- Adding the column is therefore the signal to invalidate the `.tgz` entries in
-- the bootstrap ledger, so the next startup re-imports those packages and stamps
-- the ranks. Without this the column would sit at its DEFAULT 0 on every existing
-- row (readers coalesce NULL to 0) and the fix would be a silent no-op on any
-- server with a persistent database: the ledger skips any file whose size and
-- mtime are unchanged, so packages would never re-import and never stamp a rank.
--
-- Only `.tgz` rows are cleared. The multi-GB SNOMED/LOINC/RxNorm archives keep
-- their ledger entries and are not re-imported — they come from native importers
-- that are authoritative by construction and have nothing to learn from a reload.
-- Must run after bootstrap_imports exists, hence its position at the end.
DO $$
DECLARE
    added boolean := false;
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_attribute
        WHERE attrelid = 'code_systems'::regclass
          AND attname = 'authority_rank' AND NOT attisdropped
    ) THEN
        ALTER TABLE code_systems ADD COLUMN authority_rank INTEGER;
        added := true;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_attribute
        WHERE attrelid = 'value_sets'::regclass
          AND attname = 'authority_rank' AND NOT attisdropped
    ) THEN
        ALTER TABLE value_sets ADD COLUMN authority_rank INTEGER;
        added := true;
    END IF;

    IF added THEN
        DELETE FROM bootstrap_imports WHERE path LIKE '%.tgz';
    END IF;
END $$;

-- ── version_sort (semantic version ordering) ───────────────────────────────────
-- Byte-sortable key derived from `version` (see `version_order.rs`) so the
-- precedence ORDER BY picks the semantically newest version. Backfilled by
-- `migrate_version_sort_pg` right after this schema is applied.
ALTER TABLE code_systems ADD COLUMN IF NOT EXISTS version_sort TEXT;
-- Operator-pinned current version (admin API); see the SQLite schema.
ALTER TABLE code_systems ADD COLUMN IF NOT EXISTS current_pin INTEGER;
ALTER TABLE value_sets ADD COLUMN IF NOT EXISTS version_sort TEXT;

-- ── ICD-9-CM re-import backfill (issue #802) ────────────────────────────────────
-- The importer previously required a pipe delimiter that the real bundled CMS
-- file never has, so every code silently failed to parse while the file was
-- still recorded as a successfully-imported ledger row. Fixing the parser
-- alone does not repair a database that already has that stale, content-hash-
-- unchanged row — it would be skipped on every future restart. This clears it
-- exactly once, using a sentinel row (rather than a new column, since this fix
-- needs no schema change) to detect whether it has already run. Postgres
-- LIKE is case-sensitive, unlike SQLite's, hence ILIKE here.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM bootstrap_imports WHERE path = '#migration:icd9cm-space-delimited-802'
    ) THEN
        DELETE FROM bootstrap_imports WHERE path ILIKE '%icd-9-cm%' OR path ILIKE '%icd9cm%';
        INSERT INTO bootstrap_imports (path, content_hash, size_bytes)
        VALUES ('#migration:icd9cm-space-delimited-802', '', 0);
    END IF;
END $$;

-- ── Hierarchy from subsumedBy/parent properties (fork P1-B) ────────────────────
-- Mirrors sqlite `migrate_property_hierarchy`: CodeSystems imported before the
-- parser recognised `subsumedBy` as a parent edge (THO v3-*) get their
-- concept_hierarchy derived from the stored properties; their closure rows are
-- dropped so `migrate_concept_closure_pg` (run right after apply) rebuilds them.
DO $$
DECLARE
    sid text;
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM bootstrap_imports WHERE path = '#migration:hierarchy-from-subsumedby-p1b'
    ) THEN
        FOR sid IN
            SELECT c.system_id
              FROM concept_properties cp
              JOIN concepts c ON c.id = cp.concept_id
             WHERE cp.property IN ('subsumedBy', 'parent') AND cp.value_type = 'code'
             GROUP BY c.system_id
            HAVING COUNT(*) > 2 * (SELECT COUNT(*) FROM concept_hierarchy h
                                    WHERE h.system_id = c.system_id)
        LOOP
            INSERT INTO concept_hierarchy (system_id, parent_code, child_code)
            SELECT c.system_id, cp.value, c.code
              FROM concept_properties cp
              JOIN concepts c ON c.id = cp.concept_id
             WHERE c.system_id = sid
               AND cp.property IN ('subsumedBy', 'parent') AND cp.value_type = 'code'
               AND cp.value <> c.code
            ON CONFLICT DO NOTHING;
            DELETE FROM concept_closure WHERE system_id = sid;
        END LOOP;
        INSERT INTO bootstrap_imports (path, content_hash, size_bytes)
        VALUES ('#migration:hierarchy-from-subsumedby-p1b', '', 0);
    END IF;
END $$;

-- ── Byte-order collation for codes and versions ───────────────────────────────
-- SQLite compares TEXT bytewise (BINARY). A PostgreSQL database usually runs a
-- linguistic default collation (`en_US.utf8` in the official image), under
-- which `ORDER BY code` puts LOINC `10000-8` before `1000-9`, `version_sort`
-- keys (`2:…`, `1:…`) stop sorting as bytes and storage-id prefix ranges stop
-- being prefixes. Code, hierarchy, closure, refset, map and version columns are
-- therefore `COLLATE \"C\"`: identical order to SQLite, and cheaper (memcmp)
-- comparisons. Display and designation text keep the database collation so
-- case folding stays Unicode-aware. Every column a code is compared with is in
-- the list, so joins never mix collations. On an existing database this
-- rewrites the affected indexes once.
DO $$
DECLARE
    c_coll oid := (SELECT oid FROM pg_collation
                   WHERE collname = 'C' AND collnamespace = 'pg_catalog'::regnamespace);
    r record;
BEGIN
    FOR r IN
        SELECT a.attrelid::regclass AS tbl,
               string_agg(format('ALTER COLUMN %I TYPE text COLLATE \"C\"', a.attname), ', ') AS alters
        FROM pg_attribute a
        JOIN (VALUES
                ('concepts', 'code'),
                ('concept_hierarchy', 'parent_code'),
                ('concept_hierarchy', 'child_code'),
                ('concept_closure', 'ancestor_code'),
                ('concept_closure', 'descendant_code'),
                ('concept_properties', 'value'),
                ('value_set_expansions', 'code'),
                ('value_set_expansions', 'system_url'),
                ('refset_members', 'refset_id'),
                ('refset_members', 'referenced_code'),
                ('concept_map_elements', 'source_code'),
                ('concept_map_elements', 'target_code'),
                ('code_systems', 'version'),
                ('code_systems', 'version_sort'),
                ('value_sets', 'version'),
                ('value_sets', 'version_sort')
             ) AS want(tbl, col)
          ON a.attrelid = to_regclass(want.tbl) AND a.attname = want.col
        WHERE NOT a.attisdropped AND a.attcollation <> c_coll
        GROUP BY a.attrelid
    LOOP
        EXECUTE format('ALTER TABLE %s %s', r.tbl, r.alters);
    END LOOP;
END $$;
";

/// Apply the HTS PostgreSQL schema to the given client connection.
///
/// Safe to call on every startup — all statements are idempotent. Concurrent
/// callers are serialized via a session-scoped advisory lock so that racing
/// `CREATE EXTENSION IF NOT EXISTS pg_trgm` calls cannot collide on
/// `pg_extension_name_index` (Postgres's `IF NOT EXISTS` guard is not
/// transactional with concurrent writers).
pub async fn apply(client: &tokio_postgres::Client) -> Result<(), tokio_postgres::Error> {
    // Arbitrary constant — must match across all callers that apply this
    // schema. Use an application-scoped value that won't collide with
    // user-chosen advisory locks.
    const HTS_SCHEMA_LOCK: i64 = 0x4854_535f_5343_484du64 as i64; // "HTS_SCHM"

    client
        .execute("SELECT pg_advisory_lock($1)", &[&HTS_SCHEMA_LOCK])
        .await?;
    let result = apply_if_changed(client).await;
    // Always release the lock, even if schema apply failed.
    let _ = client
        .execute("SELECT pg_advisory_unlock($1)", &[&HTS_SCHEMA_LOCK])
        .await;
    result
}

/// Fingerprint of [`SCHEMA`] (FNV-1a), recorded once the DDL has been applied.
fn schema_fingerprint() -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in SCHEMA.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Run [`SCHEMA`] unless this exact DDL has already been applied to the
/// database.
///
/// Re-running it is idempotent but not free: `ALTER TABLE … ADD COLUMN IF NOT
/// EXISTS` and the `DO` blocks take `ACCESS EXCLUSIVE` locks even when they end
/// up doing nothing, and the whole script runs as one implicit transaction. A
/// server starting while another process writes (a second replica, a running
/// `hts import`) could then deadlock against it, and every start paid for
/// the DDL. The fingerprint row makes an up-to-date database a single
/// lookup; any change to the DDL text re-applies it.
/// Run [`SCHEMA`] unconditionally (under the schema advisory lock) and record
/// its fingerprint. `hts migrate` uses it to re-create the secondary indexes
/// it dropped for the bulk load.
pub async fn apply_ddl(client: &tokio_postgres::Client) -> Result<(), tokio_postgres::Error> {
    const HTS_SCHEMA_LOCK: i64 = 0x4854_535f_5343_484du64 as i64;
    client
        .execute("SELECT pg_advisory_lock($1)", &[&HTS_SCHEMA_LOCK])
        .await?;
    let result = async {
        client.batch_execute(SCHEMA).await?;
        record_fingerprint(client).await
    }
    .await;
    let _ = client
        .execute("SELECT pg_advisory_unlock($1)", &[&HTS_SCHEMA_LOCK])
        .await;
    result
}

async fn record_fingerprint(client: &tokio_postgres::Client) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "INSERT INTO hts_schema_meta (key, value) VALUES ('schema_fingerprint', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            &[&schema_fingerprint()],
        )
        .await?;
    Ok(())
}

async fn apply_if_changed(client: &tokio_postgres::Client) -> Result<(), tokio_postgres::Error> {
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS hts_schema_meta (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             )",
        )
        .await?;
    let fingerprint = schema_fingerprint();
    let current: Option<String> = client
        .query_opt(
            "SELECT value FROM hts_schema_meta WHERE key = 'schema_fingerprint'",
            &[],
        )
        .await?
        .map(|r| r.get(0));
    if current.as_deref() == Some(fingerprint.as_str()) {
        return Ok(());
    }
    client.batch_execute(SCHEMA).await?;
    record_fingerprint(client).await
}

/// Backfill `version_sort` for every versioned `code_systems` / `value_sets`
/// row that does not have one yet. Mirrors the SQLite
/// `schema::migrate_version_sort`; idempotent and cheap (one SELECT per table,
/// one UPDATE per missing row — only on first start after the upgrade or after
/// an older binary wrote rows without the key).
pub async fn migrate_version_sort_pg(
    client: &tokio_postgres::Client,
) -> Result<(), tokio_postgres::Error> {
    for table in ["code_systems", "value_sets"] {
        let rows = client
            .query(
                &format!(
                    "SELECT id, version FROM {table} \
                     WHERE version IS NOT NULL AND version_sort IS NULL"
                ),
                &[],
            )
            .await?;
        if rows.is_empty() {
            continue;
        }
        let update = client
            .prepare(&format!(
                "UPDATE {table} SET version_sort = $1 WHERE id = $2"
            ))
            .await?;
        for row in &rows {
            let id: String = row.get(0);
            let version: String = row.get(1);
            let key = crate::version_order::version_sort_key(Some(&version));
            client.execute(&update, &[&key, &id]).await?;
        }
        tracing::info!(
            table,
            rows = rows.len(),
            "Backfilled version_sort keys for semantic version ordering"
        );
    }
    Ok(())
}

/// Build (or rebuild) the transitive ancestor closure for one code system.
///
/// Deletes any existing closure rows for `system_id`, then recomputes the
/// full set of `(ancestor, descendant)` pairs — including self-links
/// `(code, code)`.
///
/// Uses a Rust-based BFS rather than a recursive SQL CTE. The CTE approach
/// requires Postgres to maintain a working-table that grows to O(closure_size)
/// rows — for SNOMED CT (~20M pairs) the CTE alone takes minutes. The BFS
/// processes each concept once using an integer generation counter for O(1)
/// visited-reset, completing the same work in tens of seconds.
///
/// Inserts are batched via UNNEST (parallel arrays) to amortise per-statement
/// roundtrip cost. All inserts share a single transaction so the ~20M rows for
/// SNOMED CT commit in one WAL flush.
///
/// Mirrors `backends::sqlite::schema::build_concept_closure`.
pub async fn build_concept_closure_pg(
    client: &mut tokio_postgres::Client,
    system_id: &str,
) -> Result<(), tokio_postgres::Error> {
    use std::collections::{HashMap, VecDeque};

    // Do the whole build in ONE transaction that first pins the parent
    // `code_systems` row with `FOR SHARE`. A concurrent `DELETE FROM
    // code_systems` (e.g. `delete_normalized` removing a CodeSystem, which
    // cascades to concepts/hierarchy/closure) then blocks until we commit and
    // can't drop the row mid-build — which would otherwise make the closure
    // INSERTs below violate `concept_closure_system_id_fkey`. If the row is
    // already gone, another writer deleted the system first, so there is
    // nothing to build.
    let tx = client.transaction().await?;
    // FOR UPDATE (not SHARE) also serializes two builders of the same
    // system: the pairs are streamed with COPY, which has no ON CONFLICT.
    if tx
        .query_opt(
            "SELECT 1 FROM code_systems WHERE id = $1 FOR UPDATE",
            &[&system_id],
        )
        .await?
        .is_none()
    {
        tx.commit().await?;
        return Ok(());
    }

    // Load all concept codes for this system (within the locked snapshot).
    let concepts: Vec<String> = tx
        .query(
            "SELECT code FROM concepts WHERE system_id = $1",
            &[&system_id],
        )
        .await?
        .into_iter()
        .map(|r| r.get::<_, String>(0))
        .collect();

    if concepts.is_empty() {
        tx.execute(
            "DELETE FROM concept_closure WHERE system_id = $1",
            &[&system_id],
        )
        .await?;
        tx.commit().await?;
        return Ok(());
    }

    // Map code string → index so we work with usize everywhere (no string
    // clones inside the hot BFS loop).
    let code_to_idx: HashMap<&str, usize> = concepts
        .iter()
        .enumerate()
        .map(|(i, c)| (c.as_str(), i))
        .collect();

    // Build per-node children lists (index-based).
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); concepts.len()];
    let rows = tx
        .query(
            "SELECT parent_code, child_code FROM concept_hierarchy WHERE system_id = $1",
            &[&system_id],
        )
        .await?;
    for row in &rows {
        let parent: &str = row.get(0);
        let child: &str = row.get(1);
        if let (Some(&pi), Some(&ci)) = (code_to_idx.get(parent), code_to_idx.get(child)) {
            children[pi].push(ci);
        }
    }
    drop(rows);

    // BFS from every concept to enumerate descendants, streaming each pair
    // straight into a binary COPY: every (ancestor, descendant) pair is
    // produced exactly once (per-ancestor visited marks), so no conflict
    // handling is needed after the DELETE below. COPY is several times faster
    // than batched INSERTs for SNOMED CT's ~10 M pairs.
    tx.execute(
        "DELETE FROM concept_closure WHERE system_id = $1",
        &[&system_id],
    )
    .await?;

    let sink = tx
        .copy_in(
            "COPY concept_closure (system_id, ancestor_code, descendant_code) \
             FROM STDIN (FORMAT binary)",
        )
        .await?;
    let writer = tokio_postgres::binary_copy::BinaryCopyInWriter::new(
        sink,
        &[
            tokio_postgres::types::Type::TEXT,
            tokio_postgres::types::Type::TEXT,
            tokio_postgres::types::Type::TEXT,
        ],
    );
    futures::pin_mut!(writer);

    let mut visit_gen: Vec<u32> = vec![0; concepts.len()];
    let mut queue: VecDeque<usize> = VecDeque::new();

    for anc_idx in 0..concepts.len() {
        let g = (anc_idx as u32) + 1;

        queue.clear();
        queue.push_back(anc_idx);

        while let Some(idx) = queue.pop_front() {
            if visit_gen[idx] == g {
                continue;
            }
            visit_gen[idx] = g;

            writer
                .as_mut()
                .write(&[&system_id, &concepts[anc_idx], &concepts[idx]])
                .await?;

            for &ci in &children[idx] {
                if visit_gen[ci] != g {
                    queue.push_back(ci);
                }
            }
        }
    }
    writer.finish().await?;

    tx.commit().await?;
    Ok(())
}

/// Populate `concept_closure` for every code system that has hierarchy edges
/// but no closure rows yet. Mirrors `migrate_concept_closure` for SQLite.
///
/// Called once at server startup so existing databases (imported before the
/// closure table was introduced) are migrated automatically, and at the end of
/// CLI imports so the server's first request doesn't pay the build cost.
///
/// Drives the search off the small `code_systems` table (typically <500 rows)
/// with index-friendly EXISTS probes. A previous version used `SELECT DISTINCT
/// h.system_id FROM concept_hierarchy ...`, which the PG planner converted to
/// a parallel sequential scan of the 1.2M-row SNOMED hierarchy and exhausted
/// the dynamic-shared-memory segment on CI containers with a 64 MB `/dev/shm`
/// — even when no closure work was actually needed.
pub async fn migrate_concept_closure_pg(
    client: &mut tokio_postgres::Client,
) -> Result<(), tokio_postgres::Error> {
    let systems_needing_closure: Vec<String> = client
        .query(
            "SELECT cs.id FROM code_systems cs
             WHERE EXISTS (
                       SELECT 1 FROM concept_hierarchy h
                       WHERE  h.system_id = cs.id
                       LIMIT  1
                   )
               AND NOT EXISTS (
                       SELECT 1 FROM concept_closure c
                       WHERE  c.system_id = cs.id
                       LIMIT  1
                   )",
            &[],
        )
        .await?
        .into_iter()
        .map(|r| r.get::<_, String>(0))
        .collect();

    for sid in &systems_needing_closure {
        build_concept_closure_pg(client, sid).await?;
    }
    Ok(())
}
