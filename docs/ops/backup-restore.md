# Backup, restore and point-in-time recovery

Two complementary mechanisms, both scripted in `scripts/ops/`:

| | Logical dump | Physical base backup + WAL archive (PITR) |
|---|---|---|
| Scripts | `pg-backup.sh`, `pg-restore.sh` | `pg-basebackup.sh`, `pg-pitr-restore.sh`, `wal-prune.sh` |
| Granularity | per database (`hfs`, `hts`), consistent snapshot of each | whole cluster, **any moment** covered by the archive |
| Typical use | nightly off-host copy, rollback before an upgrade, copying production into a rehearsal/test environment, PostgreSQL major upgrades | recovery from a disk/host loss or a bad write (human error, buggy client) with minimal data loss |
| RPO | time since the last dump (24 h with a nightly cron) | `archive_timeout` = **5 min** worst case (WAL is archived when a 16 MB segment fills or every 5 min) |
| Restore speed | rebuilds every index (slow for big databases) | copies files + replays WAL (fast, scales with change volume since the base backup) |

Use both. The logical dumps are also the only format that restores into a
different PostgreSQL major version.

## 1. Logical backups

```bash
# compose layout: tools run inside the database container
PG_CONTAINER=hfs-db PGUSER=postgres BACKUP_DIR=/backup/hfs/dumps KEEP_DAYS=14 \
  PROM_TEXTFILE_DIR=/var/lib/node_exporter/textfile scripts/ops/pg-backup.sh
```

- one `pg_dump -Fc -Z6` per database → `<db>-<UTC stamp>.dump` + `.sha256`,
  plus `<stamp>-globals.sql` (roles; contains password hashes — mode 600);
- each archive is verified with `pg_restore --list` before it is renamed from
  `.partial`, so a half-written file is never mistaken for a backup;
- non-zero exit if any database failed; `hfs_backup_last_status` /
  `hfs_backup_last_success_timestamp_seconds` for the `BackupMissing` /
  `BackupFailed` alerts;
- `-j N` (without `PG_CONTAINER`) switches to the parallel directory format.

Restore into a **new** database (never over the live one unless rolling back
with HFS/HTS stopped):

```bash
PG_CONTAINER=hfs-db PGUSER=postgres scripts/ops/pg-restore.sh /backup/hfs/dumps/hfs-<stamp>.dump hfs_check -j 4
# production rollback: stop hfs/hts first, then
PG_CONTAINER=hfs-db PGUSER=postgres scripts/ops/pg-restore.sh <dump> hfs --replace --owner hfs
```

It checks the sha256, creates the database, restores in parallel, runs
`ANALYZE`, and prints the schema version, fork migrations and row counts.

### One database (unified server)

After the Stage-2 move ([unified-server.md](unified-server.md)) terminology is
schema `terminology` inside `hfs` (≈ 9.6 GB instead of 1.6 GB on the
rehearsal copy), and `hts` disappears after `unify-db.sh cleanup`:

- `pg-backup.sh` without arguments backs up the default databases that exist
  (`hts` is skipped once dropped); the `hfs` dump then contains terminology
  (≈ 0.8 GB, ≈ 3–4 min);
- production's pre-deploy dumps (`hfs-eval/deploy/predeploy-dump.sh`) split
  it: `hfs-pre-<tag>-<stamp>.dump` with `--exclude-schema=terminology`
  (309 MB, 77 s) and `hfs.terminology-pre-<tag>-<stamp>.dump` (`-n
  terminology`, 505 MB, 129 s) only when `terminology_epoch` moved since the
  newest one or it is older than 7 days; `rollback-restore.sh` restores both
  (65 s + 79 s with `-j 8` on the rehearsal host, row counts identical);
- restoring an `hfs` dump into a scratch database restores both schemas; a
  terminology-only restore is `pg_restore -n terminology` (or the split dump)
  into an `hfs` database whose `terminology` schema was dropped first.

## 2. WAL archiving and base backups (PITR)

`docker/production/docker-compose.yml` runs PostgreSQL with

```
wal_level=replica  archive_mode=on  archive_timeout=300
archive_command='test ! -f /wal-archive/%f.gz && gzip -c %p > /wal-archive/%f.gz.tmp && mv /wal-archive/%f.gz.tmp /wal-archive/%f.gz'
```

with `/wal-archive` bind-mounted from `PG_WAL_ARCHIVE_DIR` — put it on a
different disk or a network mount, owned by uid 70. The command never
overwrites an existing segment and only publishes complete files.

```bash
# weekly (or nightly for large change volumes)
PG_CONTAINER=hfs-db PGUSER=postgres BASEBACKUP_DIR=/backup/hfs/base KEEP_COUNT=2 scripts/ops/pg-basebackup.sh
# daily: drop WAL older than the oldest kept base backup
scripts/ops/wal-prune.sh /backup/hfs/base /backup/hfs/wal-archive
```

### Point-in-time recovery

```bash
# 1. stop the writers
docker compose stop hfs hts
# 2. rebuild a data directory from the newest base backup before the target and
#    replay WAL up to the target; a throw-away container verifies it
PITR_CONTAINER=hfs-pitr scripts/ops/pg-pitr-restore.sh /backup/hfs/base/base-<stamp> \
  /backup/hfs/wal-archive /srv/hfs/pgdata-pitr '2026-11-15 10:42:00+08' 15432
# 3. inspect: docker exec hfs-pitr psql -U postgres -d hfs -c "SELECT max(last_updated) FROM resources"
# 4. switch: docker rm -f hfs-pitr; point PG_DATA_DIR in .env at /srv/hfs/pgdata-pitr
#    (keep the old directory until verified), docker compose up -d db hts hfs
```

The restored server starts a new timeline and is promoted at the target; it
has `archive_mode=off` in its `postgresql.auto.conf` so the verification
container cannot write into the production archive — remove that line before
running it as the new primary, and take a fresh base backup right after.

## 3. RPO / RTO

| Scenario | Mechanism | RPO | RTO (measured / extrapolated) |
|---|---|---|---|
| bad deploy / migration | logical dump taken in the upgrade procedure | 0 (taken in the window) | see §4: minutes for today's data |
| accidental mass update/delete | PITR to just before the event | seconds before the event | base extract + WAL replay (§4) + switch ≈ 15–30 min today |
| disk or host loss | PITR from the off-host base backup + archive | ≤ 5 min (`archive_timeout`) — the archive must be off the failed disk | new host provisioning + PITR |
| site loss | off-site copy of the nightly dumps | ≤ 24 h | new site + logical restore |

Targets to agree with the business owner: **RPO ≤ 5 min, RTO ≤ 1 h** for up
to ~10M resources. A streaming replica (CloudNativePG on Kubernetes, or a
second PostgreSQL with `primary_conninfo`) brings RTO down to a failover
(minutes) and RPO to ~0; it is the next step once the single-host stack is live.

## 4. Drill results

Scripts: `hfs-eval/eval/results-p3-pgops/scripts/drill.sh` (container run)
and `drill-pitr-native.sh` / `native-restore.sh` (native PostgreSQL 16, used
after the test box's Docker daemon wedged). Data: the production dumps of
2026-10-17 — HFS 8,747 resources / 300 MB, HTS 1.73M concepts / 4.6 GB.
Test box: 48 cores, NVMe, shared with other builds (load average 55–170).

| Step | Native PG 16, NVMe (load ≈ 55) | Container PG with WAL archiving, NVMe bind mount, box load 100–170 |
|---|---|---|
| `pg-backup.sh` hfs (62 MB dump) | **17 s** | 28 s |
| `pg-backup.sh` hts (378 MB dump) | **91 s** | 382 s |
| `pg-restore.sh -j 8` hfs | **4 s + 6 s ANALYZE** | 61 s + 24 s |
| `pg-restore.sh -j 8` hts | **62 s + 4 s ANALYZE** | 1,459 s + 173 s |
| `pg-restore.sh` hts single job (earlier run, load ≈ 30) | 247 s | |
| `pg-basebackup.sh` (whole cluster, 1.2 GB compressed) | — | 2,398 s (container writable layer on HDD) |
| `pg-pitr-restore.sh` to a target time: extract + WAL replay + promote | **43 s + 6 s = 49 s** | |
| WAL archive after the drill | 543 segments, 2.1 GB gzip (restores generate a lot of WAL) | |

PITR correctness: 1,000 marker rows written after the base backup, target
time taken, then 1,000 more rows and a soft-delete of every Patient. The
recovered server stopped "before commit of transaction 1737" at the target and
showed batch 1 only (1,000 rows) and 59 live Patients; the source had both
batches and 0 live Patients. Row counts of every restore matched the source
(8,738 live resources / 10,606 history rows; 3,920 code systems / 1,725,404
concepts) and checksums verified.

What this means for RTO (≈ today's data, 5 GB in total):

- logical restore of both databases: **≈ 1.5 min** on an idle SSD host,
  plus HFS/HTS restart (~1 min); the busy-host column shows the same restore can
  take 25+ minutes when the I/O is contended — size the restore host accordingly;
- PITR: **≈ 1 min** of restore work plus the time to switch the data directory
  and restart (~5 min with a runbook), plus WAL replay proportional to the writes
  since the base backup (take base backups weekly or nightly so replay stays short);
- at 10M resources (~110 GB `hfs`) extrapolate logical restore to 30–60 min
  (index rebuilds dominate) and PITR extract to ~10–15 min: PITR is the primary
  recovery path, logical dumps the portable fallback.

The PITR restore in this drill restored an alpine (musl) base backup with
glibc binaries. That works for verification, but a real recovery must use the
same image as production (text index ordering depends on the C library's
collation); `pg-pitr-restore.sh` defaults to the container path for that reason.

## 5. Drill schedule

- **Monthly**: restore last night's dumps into a scratch database
  (`pg-restore.sh … hfs_check`), compare row counts with production, drop it.
- **Quarterly**: full PITR drill on the production host class to a chosen
  timestamp; record the timings in the go-live / ops ticket and compare with §4.
- After every PostgreSQL major upgrade or storage change: both.
