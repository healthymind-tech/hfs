# Unified server: one database (terminology in schema `terminology`)

Operator runbook for Stage 2 of the unified server
([design](../design/unified-server.md) §5, §13): moving the terminology
database `hts` (≈9.5 GB in production) into schema `terminology` of the HFS
database `hfs`, verifying it, switching the server over, and rolling back.
The script is `scripts/ops/unify-db.sh`; production calls it through
`hfs-eval/deploy/unify-db.sh` (deploy lock, production defaults).

Rehearsed 2026-11-23 on a restored copy of the newest production dumps
(`*-pre-mgr-5f4c6a4f8-202611222337.dump`); numbers in §4, full transcript in
`hfs-eval/eval/results-unify/U1.md`.

## 1. Layout after the move

| Schema in `hfs` | Contents |
|---|---|
| `public` | everything HFS had (resources, search, audit, console, endpoints, sync, sessions, …) plus `hfs_terminology_state` (hm-0011) and `hfs_console_scope_bindings` (hm-0012) |
| `terminology` | HTS's tables (`code_systems`, `concepts`, `concept_*`, `value_set*`, `concept_map*`, `refset_members`, `snomed_*`, `bootstrap_imports`, `terminology_epoch`, `hts_schema_meta`) **and** HTS's embedded resource store (`resources`, `search_index`, `schema_version`, `hm_schema_migrations`, …: 64 tables in production) |

The terminology pools connect to `hfs` with
`options=-c search_path=terminology,public`
(`helios_terminology::schema::with_search_path`); the extensions `pg_trgm` and
`btree_gin` stay in `public`. Why that is safe — schema-qualified catalog
lookups, the schema created before any DDL, and the guard tests — is in
[database-migrations.md §3a](database-migrations.md#3a-one-database-the-terminology-store-in-schema-terminology).

`hfs_terminology_state` (one row) says where the store is and how far a move
got:

```sql
SELECT store, moved_from, moved_at, source_fingerprint, note FROM hfs_terminology_state;
-- schema:terminology | database:hts | 2026-11-23 02:43 | tables=64 rows=45030160 checksum=… hts_schema=… | done
```

## 2. Before the window

1. **Stage 1 is live** (embedded terminology reading the old database through
   `HFS_TERMINOLOGY_DATABASE_URL`), so the switch in step 3 is a configuration
   change and a restart. The move itself does not need it: on Stage 0 (separate
   `hts` container) the same script works and the switch is "point HTS at
   `hfs` with the search path".
2. **Disk.** The template variant holds, at peak, the stage copy (= size of
   `hts` on disk, bloat included), the new copy (compact), the WAL of the copy
   in `pg_wal` until archived and recycled, and the archive's copy of that WAL.
   Rehearsal: 8.0 GB source → **+31.1 GB peak** (stage 8.0, copy 8.0, `pg_wal`
   7.7, archive 7.7). For production's 9.5 GB plan **≈ 35 GB free** on the
   filesystem holding both the PostgreSQL volume and `backups/wal-archive`.
   The local host has ≈ 26 GB free, so do one of:
   - free space first (old image tags, old dump sets, `docker builder prune`);
   - pause archiving for the copy and take a base backup right after
     (`ALTER SYSTEM SET archive_command = '/bin/true'; SELECT pg_reload_conf();`
     … move … `ALTER SYSTEM RESET archive_command; SELECT pg_reload_conf();`
     then `scripts/ops/pg-basebackup.sh`). PITR has a gap for the copy window
     only; saves ≈ size(hts);
   - `--in-place` (no stage copy, saves ≈ size(hts)): terminology reads are down
     for the whole copy, and rollback means renaming the schema back (§6).
   `unify-db.sh --dry-run` prints the preflight; with `--wal-dir` (the wrapper
   sets it) it refuses to start when copy + WAL do not fit on a shared
   filesystem (`UNIFY_ALLOW_TIGHT_DISK=1` overrides).
3. **Backups.** `predeploy-dump.sh unify` (the last two-database set: the
   rollback target if everything else fails).
4. Run at night. Terminology writes are frozen for the whole move (§4: ≈ 5 min
   on the rehearsal host, estimate 15–45 min on the production disk).

## 3. The move

```bash
cd /home/brian/projects/hfs-workspace/hfs-eval
deploy/unify-db.sh --dry-run                       # preflight + plan, changes nothing
deploy/unify-db.sh run --yes \
  --freeze-cmd   '<set HFS_TERMINOLOGY_READONLY=true in docker-compose.yml> && docker compose up -d hfs' \
  --switch-cmd   '<remove HFS_TERMINOLOGY_DATABASE_URL> && docker compose up -d hfs' \
  --unfreeze-cmd '<unset HFS_TERMINOLOGY_READONLY> && docker compose up -d hfs'
deploy/unify-db.sh status
```

Steps (each recorded in `hfs_terminology_state.note`; a re-run skips what is
done, and after `done` a re-run is a no-op):

| Step | What happens | Readers |
|---|---|---|
| preflight | sizes, free space (data volume, WAL archive, both on one filesystem), target schema empty, recorded state | |
| freeze | `--freeze-cmd`: terminology writes answer 503 (FHIR untouched) | |
| snapshot | `template` (default): `ALTER DATABASE hts ALLOW_CONNECTIONS false`, end its sessions, `CREATE DATABASE hts_stage TEMPLATE hts`, allow again (also on failure). `dump`: `pg_dump hts \| pg_restore hts_stage`, no pause, slower | template: **terminology reads that miss the in-memory caches fail** for the file copy (27 s rehearsal) |
| rename | in the stage: every non-extension table/view/sequence/function/type of `public` → `terminology` (extensions stay, so the DDL references `public.gin_trgm_ops`, which `hfs` has too) | |
| copy | `pg_dump -n terminology` pre-data → `pg_restore --role=hfs` (schema owned by `hfs`); data as one COPY stream (no triggers fire: they are post-data); post-data (indexes, constraints, triggers) with `-j 8`; `vacuumdb -Z` | |
| verify | every table: exact row count **and** an order-independent checksum (sum of md5 of each row's text); index/constraint/trigger/sequence(last value)/function inventory; `hts_schema_meta` fingerprint; no HTS table in `public`; then `--smoke-cmd` | |
| record | `store = schema:terminology`, `moved_from = database:hts`, `moved_at`, `source_fingerprint` | |
| switch | `--switch-cmd` (≈ 10 s restart; the first start on the schema re-applies HTS's idempotent DDL once: 9 s to ready, restarts 5 s) | |
| unfreeze | `--unfreeze-cmd` | |

Failure handling:

- any failure after `freeze` runs `--unfreeze-cmd` and leaves the state at the
  last completed step; fix the cause and re-run;
- a killed or failed **copy** leaves tables in `hfs.terminology` without a
  recorded copy: the next run refuses and says to re-run with `--replace`, which
  drops `hfs.terminology` and copies again from the kept stage (rehearsed:
  killed 45 s into the data stream, `--replace` completed in 271 s and verified
  identical);
- a failed **verify** never switches; `--replace` redoes the copy;
- `--replace` is refused once the state is `switched`/`done` (the server uses
  the schema: roll back first).

`unify-db.sh verify` re-runs the verification at any time while the stage
exists.

## 4. Rehearsal results (2026-11-23)

Remote test host (NVMe, 48 cores, shared and busy: a release build ran
alongside), PostgreSQL 16.15 container with the production settings (6 GB
shared buffers, WAL archiving on, lz4 WAL compression). Restored from
`hfs-pre-mgr-5f4c6a4f8-202611222337.dump` (hfs 1.65 GB restored) and
`hts-pre-…` (hts 8.0 GB restored, 64 tables, 45,030,160 rows; production's
9.5 GB includes bloat). Production images `mgr-5f4c6a4f8` served FHIR and
terminology from the copy during the move, probed every 250 ms.

| Step | Seconds |
|---|---|
| preflight + freeze | 2 |
| snapshot (template copy = terminology read pause) | **27** |
| rename | 1 |
| copy: pre-data / data / post-data (`-j 8`) | 1 / 114 / 79 |
| ANALYZE | 4 |
| verify (full checksums, 45 M rows) | 73 |
| record + switch | 1 |
| **total** | **302** |

| Measure | Result |
|---|---|
| FHIR (`/Patient?_count=1`, `/metadata`) during the move | **0 failed of 2,576**; p99 94 / 128 ms, max 165 ms |
| terminology `$lookup` (served from HTS's response cache) | 0 failed |
| terminology `$expand` (needs the database) | **25.1 s outage** = the template copy |
| terminology writes | frozen ≈ 5 min (whole move) |
| switch: new HTS on `hfs.terminology` ready | 9.0 s (first start, DDL re-applied once), 5.1 s restart |
| WAL written | 7.70 GB (0.96 × source); archive +7.70 GB |
| peak disk growth | +31.1 GB |
| `hfs` size | 1.65 → 9.64 GB (`terminology` 7.6 GB) |
| verification | 64 tables, 45,030,160 rows, checksums and 281 objects identical, fingerprint equal |
| answers before/after | 556/556 identical (`$lookup`, `$validate-code` ×2, stored and implicit `$expand`, `$subsumes`, searches; 40 random codes each of SNOMED CT, LOINC 2.83, ICD-10-CM, NDC + 60 random stored ValueSets) |
| `test_hts` battery | 112/112 statuses identical |
| write after the switch | lands in `terminology.code_systems` / `terminology.resources`, none in `public` |
| re-run after success | no-op |
| `--snapshot dump` | see `hfs-eval/eval/results-unify/U1.md` |

**Production estimate.** The production data directory is on a rotational
disk. The template copy is a sequential read + write of 9.5 GB on one disk
(≈ 1–3 min of terminology read errors, more under load); the COPY stream and
index builds took 1,459 s for a 4.6 GB HTS restore on a busy host
([backup-restore.md §4](backup-restore.md#4-drill-results)). Expect **15–45 min**
for the whole move with writes frozen, **≈ 10 s** FHIR interruption (the hfs
restart of the switch) and no other FHIR impact.

## 5. After the move

- Watch for a week: `/health` `components.terminology`, terminology latency,
  `postgres_pool_*{pool="terminology"}`.
- Base backup right after the move (`scripts/ops/pg-basebackup.sh`), then prune
  WAL older than it (`scripts/ops/wal-prune.sh`): the copy wrote ≈ size(hts) of
  WAL.
- **Pre-deploy dumps** (`hfs-eval/deploy/predeploy-dump.sh`) detect the layout:
  `hfs` is dumped without the schema (309 MB, 77 s on the rehearsal copy), the
  schema separately as `hfs.terminology-pre-<tag>-<stamp>.dump` (505 MB, 129 s)
  **only when `terminology_epoch` changed** since the newest one (or it is older
  than 7 days), otherwise that one is reused and named in the output. The old
  `hts` database and `hts_stage` are not dumped again (`PREDEPLOY_ALL=1` to
  force). Same disk use per set as before the move, less when terminology did
  not change.
- `hfs-eval/deploy/rollback-restore.sh <tag> <dump-tag-stamp>` restores a
  post-move set: `hfs` from its dump, then the matching (or newest older)
  terminology dump into it (rehearsed into a scratch database: 65 s + 79 s,
  row counts of both schemas identical to the source).
- Nightly `scripts/ops/pg-backup.sh`: back up `hfs` (it now contains the
  schema, ≈ 0.8 GB dump); drop `hts` from the list after cleanup.
- **Cleanup** after the observation window: `deploy/unify-db.sh cleanup` drops
  `hts_stage`; `cleanup --drop-source` also drops `hts` (no rollback to the old
  database after that).

## 6. Rollback

Before cleanup the old `hts` database is untouched (the move only reads it).

1. **Back to the old database** (Stage 1/0): restore `HFS_TERMINOLOGY_DATABASE_URL`
   (or start the `hts` container again) and restart. Rehearsed: the U1 binary on
   the old database answered 556/556 identical to the production image.
2. **Writes made after the switch** exist only in the schema. Export them first:
   ```bash
   deploy/unify-db.sh export-delta > delta.json   # CodeSystem/ValueSet/ConceptMap changed after moved_at
   curl -u admin:… -X POST -H 'Content-Type: application/fhir+json' --data-binary @delta.json <old terminology base>/import
   ```
   (rehearsed: one CodeSystem written after the switch, 404 on the rolled-back
   server, 200 after the re-import.)
3. Mark the state so a later move starts clean:
   `DELETE FROM hfs_terminology_state; DROP SCHEMA terminology CASCADE; CREATE SCHEMA terminology AUTHORIZATION hfs;`
   (keep `hts_stage` or drop it).
4. Images: Stage 1/0 images keep working against the old database; nothing in
   `public` changed except the two hm-0011/hm-0012 objects, which older images
   ignore.
5. `--in-place` moves have no untouched source: roll back by renaming in `hts`
   (`ALTER TABLE … SET SCHEMA public` for every object, the reverse of the
   rename step) — rehearse before choosing that variant.
6. FHIR data is never involved.

## 7. Kubernetes / other deployments

Same procedure with the tools in the PostgreSQL pod: run `unify-db.sh` with
`--container` pointing at a container that has `psql`/`pg_dump`/`pg_restore`
16, or copy the SQL steps. Size the PVC for §2's peak, and remove
`HFS_TERMINOLOGY_DATABASE_URL` from the deployment for the switch. Deployments
that keep a dedicated terminology role can set
`HFS_TERMINOLOGY_DATABASE_URL` to `postgres://<role>@…/hfs?options=-c%20search_path%3Dterminology%2Cpublic`
after granting it the schema.

## 8. SQLite

No move: the terminology store is the file `terminology.db` next to
`fhir.db` in the same data directory (`helios_terminology::schema::sqlite_terminology_url`).
`hts migrate` (SQLite → PostgreSQL) can target the HFS database with the search
path in the URL.
