# PostgreSQL as the production backend

PostgreSQL 16 is the production configuration for both servers. SQLite stays
for development and single-user tools only: in the October benchmark
(`hfs-eval/eval/bench/BENCHMARK.md`) HFS-SQLite fell to 12 req/s under mixed
concurrent load where HFS-PG served 2,687 req/s.

One PostgreSQL instance hosts two databases:

| Database | Owner role | Used by | Production size (2026-11-15) |
|---|---|---|---|
| `hfs` | `hfs` | HFS: resources, history, search index, bulk jobs, package registry | 361 MB (8.7k resources) |
| `hts` | `hts` | HTS: terminology tables + its own resource store | 5.06 GB (SNOMED, LOINC ×2, ICD-10, 28 IGs' CodeSystems) |

Capacity rule of thumb measured on the benchmark corpora: **≈ 10.5 KB of
database per FHIR resource** including history and the search index
(527k Synthea resources → 5.56 GB), i.e. ~10 GB per million resources, plus
WAL. A hospital loading 10M resources needs ~110 GB for `hfs` before history
growth; size the data disk at 3× the expected database for VACUUM, index
rebuilds and a restore copy.

## 1. Server parameters

The values in `docker/production/docker-compose.yml`, for a 64 GB host where
PostgreSQL may use ~24 GB:

| Parameter | Value | Why |
|---|---|---|
| `shared_buffers` | 6 GB (≈25% of PG's share) | hot search_index pages |
| `effective_cache_size` | 24 GB | planner hint: OS cache + shared_buffers |
| `work_mem` | 32 MB | per sort/hash node; 200 connections × a few nodes must fit |
| `maintenance_work_mem` | 1 GB | index builds in migrations, VACUUM |
| `max_connections` | 200 | see the pool budget in §2 |
| `random_page_cost` | 1.1 on SSD/NVMe, 4 on HDD | the search planner leans on it heavily |
| `effective_io_concurrency` | 200 SSD, 2 HDD | bitmap heap prefetch |
| `max_wal_size` / `checkpoint_timeout` | 8 GB / 15 min | bulk loads without checkpoint storms |
| `wal_compression` | lz4 | ~2× less WAL to archive |
| `archive_mode` / `archive_timeout` | on / 300 s | PITR, RPO ≤ 5 min ([backup-restore.md](backup-restore.md)) |
| `shared_preload_libraries` | pg_stat_statements | the slow-query review loop (§4) |
| `log_min_duration_statement` | 1000 ms | every slow statement lands in the log |
| `idle_in_transaction_session_timeout` | 10 min | a crashed client must not hold locks forever |
| `autovacuum_vacuum_scale_factor` / `analyze` | 0.05 / 0.02 | `search_index` is the largest table by far (≈30 rows per resource) and the default 20% means a 6M-row backlog before it is vacuumed |
| `jit` | off | JIT compile time dominates the short search queries |
| `max_locks_per_transaction` | 1024 (default 64; needs a restart) | transaction Bundles hold one advisory lock per id-bearing entry. With the default lock table a Bundle of more than 128 such entries takes the tenant's exclusive write gate and every other writer waits for it; with 1024 the cap rises with the table (§2a). About 55 MB of shared memory at `max_connections=200` |

**Disk.** The production database currently sits on a rotating HDD
(`random_page_cost=4`, `effective_io_concurrency=2` in
`hfs-eval/docker-compose.yml`). Move it to SSD/NVMe before go-live: cold
searches on the benchmark host were measured on NVMe, and a search that reads
1–2k random pages (e.g. a broad `_revinclude`) costs ~10 ms there and several
seconds on an HDD.

## 2. Connection pools and timeouts

Every HFS and HTS process opens its own pools; the sum must stay under
`max_connections` with headroom for maintenance (psql, pg_dump, the exporter,
replication).

| Pool | Variable | Default | Production value | Notes |
|---|---|---|---|---|
| HFS primary | `HFS_PG_MAX_CONNECTIONS` | `cores × 4`, clamped 16..64 | **32** | search is round-trip bound; past ~2× DB cores more connections only move the queue into PostgreSQL |
| HFS audit (only with a PG audit sink) | same variable, separate pool | as above | 32 | `HFS_AUDIT_DATABASE_URL` for a dedicated database |
| HTS terminology | `HTS_PG_POOL_SIZE` | 32 | **16** | warm `$expand`/`$validate-code` hits bypass the pool |
| HTS resource store | `HFS_PG_MAX_CONNECTIONS` **in the HTS container** | `cores × 4` (64 on a 24-core host!) | **8** | HTS reads the HFS variable for its embedded store; set it explicitly |

Budget for the production compose: 32 + 16 + 8 = 56 connections per HFS+HTS
pair, so up to three pairs fit in 200 with room for tooling.

Timeouts:

| Variable | Default | Production | Effect |
|---|---|---|---|
| `HFS_PG_STATEMENT_TIMEOUT_MS` | 30000 | 60000 | server-side cap per statement, sent in the startup packet so every pooled connection has it. 60 s because IG-sized transaction Bundles hold the tenant write gate for tens of seconds; concurrent bulk loaders at 30 s saw 0.5–4.9% of Bundles rolled back in the benchmark |
| `HFS_PG_POOL_WAIT_TIMEOUT_SECS` | 10 | 10 | how long a request waits for a pooled connection before failing with 503 (`Unavailable`) |
| `HFS_PG_CONNECT_TIMEOUT_SECS` | 5 | 10 | whole connection establishment (DNS, TCP, TLS, auth) |
| `HFS_PG_PLAN_CACHE_MODE` | `force_custom_plan` | leave | see the type docs in `backends/postgres/backend.rs`: `auto` makes reference/OR-list searches up to 100× slower |
| `HTS_PG_POOL_WAIT_TIMEOUT_SECS` | unset (wait forever) | 60 | new in this branch; opt-in |
| `HTS_PG_CONNECT_TIMEOUT_SECS` | unset | 10 | new in this branch; opt-in |
| `HTS_PG_STATEMENT_TIMEOUT_MS` | unset | unset | leave unset when the same HTS runs `hts import` / large IG terminology batches |
| `HFS_REQUEST_TIMEOUT` / `HTS_REQUEST_TIMEOUT` | 30 s | 600 s | HTTP-level; large Bundles need it. nginx `proxy_read_timeout` must match |

Pool occupancy is exported on `/metrics` as `postgres_pool_max_connections`,
`postgres_pool_connections`, `postgres_pool_available_connections` and
`postgres_pool_waiting_callers` (label `pool` = `primary`, `audit` on HFS; `terminology`, `resources` on HTS); `PostgresPoolWaiting` and `PostgresPoolSaturated` alert on them
([monitoring.md](monitoring.md)). If `postgres_pool_waiting_callers` is above zero
for minutes while PostgreSQL CPU is low, raise the pool; if PostgreSQL CPU is
saturated, raising it makes things worse — find the slow query instead (§4).

## 2a. Write locks: what runs side by side

Writes take PostgreSQL advisory locks, all at transaction level:

| Write | Locks | Runs beside other writes of the tenant |
|---|---|---|
| single create / update / delete, `PUT Type/id` | tenant gate shared + the resource's key | yes |
| conditional create (`If-None-Exist`), `PUT Type?criteria` | the same, plus the lock of its criteria | yes; two writers on the same criteria queue |
| transaction Bundle | tenant gate shared + one key per id-bearing entry (`PUT`/`PATCH`/`DELETE`/`GET Type/id`, `POST` with an id); nothing for a plain `POST` | yes |
| Bundle with `PUT`/`PATCH`/`DELETE Type?criteria` entries | shared; the targets' keys and the criteria locks are taken without waiting once the targets are known; if one is busy the Bundle starts over under the exclusive gate | yes, unless it had to fall back |
| Bundle over the key cap, anything touching a `SearchParameter`, purge, reindex clear | tenant gate **exclusive** | no: it waits for every writer and they wait for it |

Each of these uses one pooled connection for the whole request. (Until
hm/perf-count a `PUT Type/id` and a conditional create held a session lock on
a second pooled connection: with as many of them in flight as the pool has
connections they all failed 503 after `HFS_PG_POOL_WAIT_TIMEOUT_SECS`.)

| Variable | Default | Meaning |
|---|---|---|
| `HFS_PG_BUNDLE_LOCK_KEYS` | derived: half of the lock table (`max_locks_per_transaction × (max_connections + max_prepared_transactions)`) shared among `HFS_PG_MAX_CONNECTIONS` Bundles of `2 × cap + 1` locks, never below 128, never above 4096 | most id-bearing entries (and, separately, most `ifNoneExist` creates) a Bundle may carry under the shared gate. Logged at startup when it is not 128. A stock server gives 128; `max_locks_per_transaction=1024`, `max_connections=200` and a pool of 64 give 800 |
| `HFS_PG_BUNDLE_CONDITIONAL_SHARED` | on | `off` sends every Bundle with a criteria-addressed entry back to the exclusive gate (the behaviour before hm/perf-count) |
| `HFS_PG_INDEX_COUNT` | on | `off` keeps `_summary=count` / `_total=accurate` on the `resources` join ([index-strategy.md §7](index-strategy.md#7-counting-from-the-index-and-orphan-index-rows-hm-0040)) |

A Bundle that finds the lock table full (`out of shared memory`, SQLSTATE
53200) ends as a retryable 503, not as a failed request; if that shows up in
the log, raise `max_locks_per_transaction` or set `HFS_PG_BUNDLE_LOCK_KEYS`
lower.

## 3. Roles and security

- One login role per server (`hfs`, `hts`), each owning only its database
  (`docker/production/initdb/10-roles.sh`). Neither needs superuser: the
  extensions HFS wants (`pg_trgm`, `btree_gin`) are created by the init script,
  and both are trusted extensions anyway.
- The `postgres` superuser is used only by the backup scripts and the exporter.
  For a stricter setup give the exporter `pg_monitor` and backups a role with
  `REPLICATION` + `pg_read_all_data`.
- PostgreSQL is reachable only on the compose `backend` network (no published
  port). For a remote database set `sslmode=require` in the URLs.

## 4. Slow-query review loop

1. `log_min_duration_statement=1000` writes every statement over 1 s to the
   PostgreSQL log (`docker compose logs db | grep duration`).
2. `pg_stat_statements` ranks statements by total time:

   ```sql
   SELECT round(total_exec_time) AS total_ms, calls, round(mean_exec_time,1) AS mean_ms,
          left(regexp_replace(query, '\s+', ' ', 'g'), 160) AS query
   FROM pg_stat_statements ORDER BY total_exec_time DESC LIMIT 20;
   ```
3. Reproduce with `EXPLAIN (ANALYZE, BUFFERS)`. `scripts/ops/search-bench.py`
   times the standard search set through HFS and, with `auto_explain` enabled,
   collects the plan of every search; see [index-strategy.md](index-strategy.md)
   for the method and the current results.
4. Unused or bloated indexes:

   ```sql
   SELECT relname, indexrelname, idx_scan, pg_size_pretty(pg_relation_size(indexrelid))
   FROM pg_stat_user_indexes WHERE schemaname='public' ORDER BY idx_scan, pg_relation_size(indexrelid) DESC;
   ```

## 5. Routine maintenance

Autovacuum handles normal operation. Extra steps:

| When | What |
|---|---|
| after a bulk load (> 20% growth) or a restore | `VACUUM (ANALYZE) search_index; ANALYZE;` — HFS runs a bounded ANALYZE after its own migrations, not after loads |
| after an upgrade that adds indexes | check `SELECT indexrelid::regclass FROM pg_index WHERE NOT indisvalid;` is empty |
| monthly | the unused-index query above; `pg_stat_statements_reset()` after reviewing |
| PostgreSQL minor release | pull the new `postgres:16.x-alpine` tag, `docker compose up -d db` (a restart; data compatible) |
| PostgreSQL major upgrade (16 → 17) | logical: `pg-backup.sh` → new cluster → `pg-restore.sh` (≈ RTO in [backup-restore.md](backup-restore.md)); or `pg_upgrade --link` in a maintenance window |
