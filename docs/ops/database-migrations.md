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
| Upstream ladder | `schema_version(version)` — one row | `v1 → v2 → … → SCHEMA_VERSION` (55 in this build, upstream v0.2.8 and v0.2.9) | upstream HeliosSoftware/hfs |
| Fork migrations | `hm_schema_migrations(name, applied_at, note)` | by **name**, e.g. `hm-0001-search-parameter-epochs` | healthymind fork |

At startup:

1. If the database is stamped **v47** and contains the fork's
   `search_parameter_epochs` table but not upstream's `idx_resources_reindex_id`,
   it is a database created by a fork build up to `hm/p25-z`. It is
   re-stamped to v46 and `hm-0001-search-parameter-epochs` is recorded as
   applied — one transaction, no data touched (§3) — and the upstream steps
   v46→v47…v54→v55 then run in step 3 as for any v46 database. (Production
   was re-stamped by the hm/p3-pgops deploy and is at v46 + hm-0001..hm-0007.)
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

Fork migrations in this build:

| Name | Adds | From |
|---|---|---|
| `hm-0001-search-parameter-epochs` | `search_parameter_epochs` + triggers | fork (was ladder "v47") |
| `hm-0002-cluster-jobs` | `cluster_jobs` (shared async job queue) | upstream branch `feat/cluster-capable-state` (its v14→v15), via hm/p3-cluster |
| `hm-0003-subscription-cluster-delivery` | `subscription_state`, `subscription_notification_events`, `subscription_delivery_outbox`, `subscription_ws_tokens` | same upstream branch (v16→v17) |
| `hm-0004-resource-change-feed` | `resource_change_feed` + StructureDefinition triggers | fork (hm/p3-cluster) |
| `hm-0005-package-jobs` | `hfs_package_jobs` | fork (hm/p3-cluster) |
| `hm-0006-search-index-statistics` | dependency-only extended statistics on `search_index`, token statistics target 1000, ANALYZE ([index-strategy.md §4](index-strategy.md#4-planning-cost-and-extended-statistics-hm-0006)) | fork (hm/p3-pgops2) |
| `hm-0007-bulk-manifest-index-pending` | `bulk_manifests.index_pending` (deferred bulk-submit indexing) | fork (hm/p3-interop2; was startup DDL, registered here at merge). Upstream v52 adds the same column (`BOOLEAN NOT NULL DEFAULT FALSE`) plus `index_pending_types` and `idx_bulk_manifests_index_pending`, all `IF NOT EXISTS`: on a database that already ran hm-0007, v52 only adds the latter two. |
| `hm-0008-endpoints` | `hfs_endpoints`, `hfs_endpoint_revisions`, `hfs_config_epochs` + `hfs_bump_config_epoch()` (statement triggers, `NOTIFY hfs_config`) | fork (FHIR server manager, [design](../design/fhir-server-manager.md) §4.1; inert until `HFS_MANAGER=on`) |
| `hm-0009-console-auth` | `hfs_console_settings`, `hfs_console_users`, `hfs_console_role_bindings` | fork (design §4.2) |
| `hm-0010-endpoint-sync` | `hfs_sync_jobs`, `hfs_sync_runs`, `hfs_sync_log`, `hfs_sync_conflicts`, `hfs_sync_item_state` | fork (design §4.3) |
| `hm-0040-search-index-liveness` | row triggers on `resources` (`trg_search_index_follow_delete`, `_soft_delete`, `_truncate`) that remove a resource's `search_index` rows with it, and `hfs_search_index_liveness(tenant_id, resource_type, verified_at, orphan_resources)` | fork (hm/perf-count; [index-strategy.md §7](index-strategy.md#7-counting-from-the-index-and-orphan-index-rows-hm-0040)). Catalog-only: no table rewrite, no data touched. The performance line owns `hm-0040`…`hm-0049` |
| `hm-0011-terminology-schema` | schema `terminology` + `hfs_terminology_state` (one row: `store`, `moved_from`, `moved_at`, `source_fingerprint`, `note`, `updated_at`) | unified server ([design](../design/unified-server.md) §5.4). **Not** in `FORK_MIGRATIONS`: applied by `PostgresBackend::apply_unified_migrations` only when terminology is embedded; checks `has_database_privilege(…, 'CREATE')` first |
| `hm-0012-console-terminology-bindings` | `hfs_console_scope_bindings(subject, scope ∈ {terminology}, role)` + console epoch trigger | unified server (design §7.2), same rule as hm-0011. A sibling table, not a column on `hfs_console_role_bindings` (whose key is `(subject, endpoint_id)` with an FK to `hfs_endpoints`) |
| `hm-0030-audit-event-table` | `audit_event`, `audit_event_ref` (monthly partitions + default), immutability trigger `hfs_audit_event_immutable`, `hfs_audit_ensure_partition(date)`, `hm_audit_backfill_state` ([audit.md](audit.md#the-audit-tables-hfs_audit_storetable)). Additive: the previous build is unaffected | fork (hm/perf-audit-table). The audit line owns `hm-0030`…`hm-0039` |
| `hm-0031-audit-backfill` | **not applied at start-up**: recorded when the operator-started backfill (`HFS_AUDIT_BACKFILL=true`) has moved every legacy `AuditEvent` resource into the audit tables | fork (hm/perf-audit-table) |

hm/p3-cluster first numbered 0002–0005 as ladder v48–v51; they were moved onto
this track when it was merged with hm/p3-pgops (upstream has its own v48+). No
deployed database ever carried v48–v51. Checked at the v0.2.8 merge
(hm/upstream-merge): upstream v47–v55 create none of the hm-0002..hm-0006
objects (the cluster tables are still only on upstream's unreleased
`feat/cluster-capable-state` branch); the only overlap is v52 / hm-0007 above.

Upstream v47–v55, in order: v47 `idx_resources_reindex_id`; v48 composite
slots on `search_index` + legacy contained-composite index; v49
`idx_resources_live_tenant` (CONCURRENTLY); v50 `idx_search_date_recent_end`;
v51 SQL-on-FHIR terminology tables; v52 bulk-manifest deferred-index marker;
v53 composite fast-path indexes partial on `is_contained`; v54
`bulk_submissions(updated_at)` index; v55 `search_index.value_reference_target`
+ backfill (minutes per 10M index rows).

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

## 3a. One database: the terminology store in schema `terminology`

The unified server ([design](../design/unified-server.md) §5) keeps HTS's
tables **and** HTS's embedded resource store in schema `terminology` of the
`hfs` database. Their pools connect with
`options=-c search_path=terminology,public` (`helios_terminology::schema::with_search_path`).
What makes that safe (package U1):

- **Connection strings keep the schema.** `PostgresBackend` reads
  `options=-c search_path=<schema>,…` from a URL into `PostgresConfig.schema`
  and sends it in every connection's startup packet (it used to drop every
  query parameter). HTS's pool appends `HTS_PG_STATEMENT_TIMEOUT_MS` to the
  URL's `options` instead of replacing them.
- **The schema must exist before any DDL.** Both stores call
  `ensure_store_schema` at startup: when `search_path` names a schema first
  that does not exist, PostgreSQL would silently fall back to `public` and the
  store would create (or adopt!) its tables next to HFS's. The schema is
  created instead, and startup fails if connections still resolve elsewhere.
- **No unqualified catalog lookup.** Every `to_regclass`, `information_schema`,
  `pg_class`/`pg_attribute`/`pg_constraint`/`pg_index(es)`/`pg_stats` lookup in
  `crates/persistence/src/backends/postgres/**` and `crates/hts/src/**` is pinned
  to `current_schema()` (`to_regclass(format('%I.%I', current_schema(), 'x'))`,
  `table_schema = current_schema()`, `relnamespace = to_regnamespace(current_schema())`).
  DDL naming an existing index by a computed name (`DROP INDEX … IF EXISTS`,
  `ALTER INDEX … RENAME`) qualifies it with `in_store_schema()`.
- **Guards** (`crates/persistence/tests/terminology_schema.rs`, CI group
  persistence-postgres; `crates/hts/tests/postgres_terminology_schema.rs`, CI
  group hts-postgres):
  - `no_unqualified_catalog_lookups` scans the SQL string literals of both
    crates and fails on any catalog lookup without `current_schema()` in the
    same statement, or a bare `'name'::regclass`;
  - `postgres_integration_terminology_schema_isolation` builds HFS in `public`,
    initialises the embedded store in `terminology`, and compares a full
    catalog + row-count snapshot of `public` before and after (and after a
    restart with store tables dropped). On its first run it caught a real
    fall-through: the v43/v50 date-index swaps ran `DROP INDEX IF EXISTS
    idx_search_date_end` while the fresh store had no such index yet and
    **dropped HFS's** `public.idx_search_date_end` / `idx_search_date_recent_end`;
  - the HTS test does the same for HTS's 19 tables, and checks a missing schema
    is created rather than `public` used.
  Static DDL with `IF EXISTS` (`DROP INDEX IF EXISTS idx_x`) is covered by the
  catalog snapshot, not by the scan: write new swaps with `in_store_schema()`.
- The upstream ladder and the fork track run **per schema**: `terminology` has
  its own `schema_version` and `hm_schema_migrations` (hm-0001..hm-0010 only).
  The migration advisory lock is per database, so HFS and the embedded store
  serialise their startup migrations (seconds).
- Changing HTS's DDL text (U1 qualified four guards) changes the
  `hts_schema_meta` fingerprint: the first start of a new image re-applies the
  idempotent DDL once (measured 9 s to ready on the 8 GB production copy,
  restarts 5 s).

The data move from the separate `hts` database is `scripts/ops/unify-db.sh`
([unified-server.md](unified-server.md)).

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
5. Name objects so the terminology store cannot reach HFS's: catalog lookups
   use `current_schema()`, and DDL on an object found by name qualifies it
   (`in_store_schema()`); `terminology_schema.rs` must stay green (§3a).
6. When the change is generally useful, open an upstream PR. Once upstream
   ships it under a number, the fork migration stays in the list (it becomes a
   no-op through its `IF NOT EXISTS` guards) — do not delete fork entries.
