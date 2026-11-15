# Database schema migrations (PostgreSQL)

How the HFS and HTS PostgreSQL schemas are versioned, how an upgrade applies
them, and the rules for adding one in this fork. Operators: read
[Upgrade procedure](upgrade.md) for the step-by-step; this page is the model
behind it.

## 1. HFS: two tracks

HFS keeps its schema in one database (`hfs`). Migrations run automatically at
startup, inside `PostgresBackend::init_schema`, serialised across instances by
a session advisory lock (`pg_try_advisory_lock(0x48465300_4d494752)`), so two
HFS processes starting together never migrate twice.

| Track | Recorded in | Numbering | Owner |
|---|---|---|---|
| Upstream ladder | `schema_version(version)` — one row | `v1 → v2 → … → SCHEMA_VERSION` (46 in this build) | upstream HeliosSoftware/hfs |
| Fork migrations | `hm_schema_migrations(name, applied_at, note)` | by **name**, e.g. `hm-0001-search-parameter-epochs` | healthymind fork |

At startup:

1. If the database is stamped **v47** and contains the fork's
   `search_parameter_epochs` table but not upstream's `idx_resources_reindex_id`,
   it is a database created by a fork build up to `hm/p25-z` (the production
   database is one). It is re-stamped to v46 and
   `hm-0001-search-parameter-epochs` is recorded as applied — one transaction,
   no data touched (§3).
2. If the stamped version is **newer** than the binary's `SCHEMA_VERSION`, HFS
   **refuses to start** with an error naming the versions. An older binary does
   not maintain columns and indexes a newer migration added (for example
   upstream v55's `value_reference_target`), so letting it write corrupts search
   results silently. Override with `HFS_PG_ALLOW_NEWER_SCHEMA=true` only when
   the release notes say the newer schema is backward compatible.
3. Upstream steps from the stamped version up to `SCHEMA_VERSION` run in order.
   Each step either commits its own marker inside its transaction or the loop
   stamps the new version right after it, so an interrupted upgrade resumes at
   the step that failed.
4. Fork migrations not yet in `hm_schema_migrations` run in list order
   (`FORK_MIGRATIONS` in `crates/persistence/src/backends/postgres/schema.rs`).

Fork migrations so far (append-only; `FORK_MIGRATIONS` in `schema.rs`):

| Name | What |
|---|---|
| `hm-0001-search-parameter-epochs` | per-tenant SearchParameter epoch table + triggers (formerly upstream-ladder "v47") |
| `hm-0002` … `hm-0005` | reserved for line F (clustering), assigned by the integration merge |
| `hm-0006-search-index-statistics` | dependency-only extended statistics on `search_index`, token statistics target 1000, ANALYZE ([index-strategy.md §4](index-strategy.md#4-planning-cost-and-extended-statistics-hm-0006)) |

```sql
-- what a database is at
SELECT version FROM schema_version;
SELECT name, applied_at, note FROM hm_schema_migrations ORDER BY applied_at;
```

### Why the fork has its own track

The fork shipped its SearchParameter-epoch migration (commit `081b09fe9`, a
74 ms → ~5 ms single-create improvement) as upstream-ladder **v47**. Upstream
v0.2.5 then shipped a *different* v47 (`idx_resources_reindex_id`, the ranged
`$reindex` walk index) and has since reached v55 (v0.2.8). After a rebase onto
upstream, a database stamped with the fork's v47 would have skipped upstream's
v47 silently: no `idx_resources_reindex_id`, and ranged `$reindex` scanning the
whole type. Taking fork migrations off the numbered ladder removes that class
of bug for good: upstream numbers stay upstream's, and fork changes are
recorded by name.

## 2. HTS: idempotent DDL

HTS (`crates/hts/src/backends/postgres/schema.rs`) has no version table. Its
schema is a list of `CREATE TABLE IF NOT EXISTS`, `ALTER TABLE … ADD COLUMN IF
NOT EXISTS`, `CREATE INDEX IF NOT EXISTS` and guarded `DO $$ … $$` blocks,
re-applied in full at every startup, followed by data backfills that only touch
rows still missing the derived value (`version_sort`, `fhir_id`). Consequences:

- an upgrade needs no operator step; a rollback to an older HTS also works as
  long as it does not depend on a column the newer one dropped (HTS never drops
  columns today);
- a new HTS column with a non-constant default rewrites its table at startup —
  for `concepts` (≈1.7M rows in production) that is minutes. Check the diff of
  `schema.rs` before an upgrade (§5 of [upgrade.md](upgrade.md));
- some features need a re-import, not a migration, to populate new tables (for
  example the SNOMED concept-model tables `snomed_*` from Phase 2.5 X). Those are
  listed in the release notes.

HTS also embeds the HFS resource store (`helios-persistence`) for its
non-terminology FHIR resources, in the same `hts` database. That store follows
the HFS rules in §1.

## 3. Upgrade test against a copy of production

Tested 2026-11-15 on the build of this branch, against
`backups/hfs-pre-p3-202610171149.dump` / `hts-pre-p3-202610171147.dump`
(production dumps, PostgreSQL 16.15, HFS schema stamped 47, 8,747 resources,
10,608 history rows; HTS 1,725,404 concepts):

| Check | Before | After first start | After restart |
|---|---|---|---|
| `schema_version` | 47 | **46** | 46 |
| `hm_schema_migrations` | (absent) | `hm-0001-search-parameter-epochs` ("re-stamped from upstream-ladder v47") | unchanged |
| resources / history / search_index | 8,747 / 10,606 / 236,467 | identical | identical |
| `search_parameter_epochs` rows, `hfs_packages` | 1 / 28 | identical | identical |
| HFS startup to first `/metadata` 200 | | 1.2 s | 0.9 s |
| smoke: `/metadata`, `Patient?_count=5`, `Observation?_sort=-_lastUpdated`, TW Core SD by url, a create | | 200 / 200 / 200 / 200 / 201 | |

HFS logged `Moved fork-stamped schema version back onto the upstream migration
ladder from=47 to=46 migration="hm-0001-search-parameter-epochs"`. HTS
started on the restored `hts` (pre-p3, so it also ran the U+W startup
backfills: `version_sort` keys, unique resource ids for 1,947 CodeSystem
groups) and served `$lookup` / `$expand`; its embedded resource store was
re-stamped the same way. Transcript: `hfs-eval/eval/results-p3-pgops/migtest.out`.

Automated tests (`crates/persistence/src/backends/postgres/schema.rs`, module
`postgres_integration_migrations`):

- `postgres_integration_fresh_schema_records_fork_migrations_by_name`
- `postgres_integration_fork_stamped_v47_is_restamped_without_data_loss`
- `postgres_integration_upstream_v47_is_not_mistaken_for_the_fork`
- `postgres_integration_newer_schema_is_refused_unless_allowed`

Run them (16 tests in the module, all passing on 2026-11-15):

```bash
cargo test -p helios-persistence --features postgres --lib \
  backends::postgres::schema::postgres_integration_migrations
# shared hosts that require named containers:
HELIOS_TEST_CONTAINER_PREFIX=myteam-pgtest cargo test ...
# no Docker: an existing PostgreSQL 16 with superuser postgres/postgres
HFS_TEST_PG_HOST=127.0.0.1 HFS_TEST_PG_PORT=5432 cargo test ...
```

## 4. Rules for adding a schema change in this fork

1. **Never** bump `SCHEMA_VERSION` or add a `vN → vN+1` step in the fork. That
   number belongs to upstream.
2. Append a name to `FORK_MIGRATIONS` (`hm-NNNN-short-description`), add its
   arm to the `match` in `run_fork_migrations`, and write the body so it is
   **idempotent** (`IF NOT EXISTS`, `CREATE OR REPLACE`, `DROP … IF EXISTS`
   then create). A fork migration runs after every upstream step, and after a
   rebase it may meet objects an upstream migration already created.
3. Large index builds: `CREATE INDEX CONCURRENTLY` outside a transaction, with
   `SET statement_timeout = 0` on that session, and check `pg_index.indisvalid`
   afterwards (an interrupted concurrent build leaves an invalid index that
   `IF NOT EXISTS` will not repair; see `ensure_patient_export_index`).
4. Test first: a `postgres_integration_*` test that builds the *previous*
   schema (initialize, then remove what the migration adds), runs
   `initialize_schema`, and asserts both the new objects and that pre-existing
   rows survive.
5. When the change is generally useful, open an upstream PR. Once upstream
   ships it under a number, the fork migration stays in the list (it becomes a
   no-op through its `IF NOT EXISTS` guards) — do not delete fork entries.
