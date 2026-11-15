# Upgrade procedure (HFS / HTS images)

Applies to every image change on production: a new fork build, an upstream
merge, a hotfix. Expected outage: 1–5 minutes for a build without heavy
migrations (container recreate + HFS startup); a release that adds indexes over
`search_index` can take longer — the rehearsal (step 2) tells you how long.

## 0. Before the maintenance window

1. **Release notes** for the build: commits since the running revision
   (`git log --oneline <running>..<new>`), and specifically:
   - `git diff <running> <new> -- crates/persistence/src/backends/postgres/schema.rs`
     → new upstream steps (`SCHEMA_VERSION` change) or fork migrations
     (`FORK_MIGRATIONS` change)? Each one is a one-way door (see Rollback).
   - `git diff <running> <new> -- crates/hts/src/backends/postgres/schema.rs`
     → new HTS columns/indexes (idempotent, applied at start; a new column with
     a computed default rewrites `concepts`).
   - data steps the release needs: terminology re-imports (e.g. SNOMED for the
     `snomed_*` concept-model tables), IG re-installs, `$reindex` of a resource
     type after a SearchParameter change, profile rehydration.
   - new or changed environment variables.
2. **Build** the images (`remote-build.sh <checkout> <tag>`), push to the
   registry, note the tag.
3. **Rehearse** on a copy of production (mandatory when step 1 found a schema
   change, recommended always):
   ```bash
   # newest nightly dumps -> scratch PostgreSQL (any host with docker)
   PG_CONTAINER=scratch-pg scripts/ops/pg-restore.sh /backup/hfs/dumps/hfs-<stamp>.dump hfs
   PG_CONTAINER=scratch-pg scripts/ops/pg-restore.sh /backup/hfs/dumps/hts-<stamp>.dump hts
   # start the NEW hfs/hts images against it, time from start to first 200 on /metadata,
   # check the log for migration lines and errors, then:
   python3 scripts/ops/search-bench.py --base http://127.0.0.1:<port> --psql "docker exec -i scratch-pg psql -U hfs -d hfs" --out new.json
   ```
   Compare row counts (`pg-restore.sh` prints them) before/after startup and the
   search timings against the previous run.
4. Announce the window to integrating systems (HIS, LIS, PACS feeds) — they
   must queue or retry writes for its duration.

## 1. Upgrade

```bash
cd /opt/hfs/docker/production            # (evaluation stack: hfs-eval/, deploy/deploy.sh does 3-6)
# 1. fresh logical backup = the rollback point (≈ 1–5 min for today's data)
PG_CONTAINER=hfs-db PGUSER=postgres BACKUP_DIR=/backup/hfs/dumps scripts/ops/pg-backup.sh -k 0
docker compose exec db psql -U postgres -d hfs -Atc "SELECT version FROM schema_version"   # note it
# 2. point .env at the new tag
sed -i 's|^HFS_IMAGE=.*|HFS_IMAGE=healthymind/hfs:<tag>|; s|^HTS_IMAGE=.*|HTS_IMAGE=healthymind/hts:<tag>|' .env
# 3. HTS first (HFS validates against it), then HFS
docker compose up -d hts && docker compose logs -f hts     # until it listens
docker compose up -d hfs && docker compose logs -f hfs     # migrations run here; until "Server listening"
# 4. nginx caches upstream IPs: restart it after recreating hfs/hts
docker compose restart nginx
```

## 2. Verify

```bash
for p in /fhir/metadata /tx/metadata /ui /ui/hts; do printf '%-16s ' $p; curl -s -o /dev/null -w '%{http_code}\n' "http://127.0.0.1:8082$p"; done
docker compose exec db psql -U postgres -d hfs -Atc "SELECT version FROM schema_version" \
  -c "SELECT string_agg(name, ',') FROM hm_schema_migrations" \
  -c "SELECT indexrelid::regclass FROM pg_index WHERE NOT indisvalid"     # must be empty
curl -s http://hfs:8080/metrics | grep '^build_info'        # from the docker network / Prometheus: new revision
```

- Grafana "Running builds" shows the new revision on every instance; error
  ratio and p95 back at baseline within 15 minutes.
- Functional smoke: read a known Patient, run one search per resource type the
  hospital uses, `$validate` a TW Core example, `$expand` a LOINC ValueSet,
  `$lookup` an ICD-10-CM code.
- Evaluation stack only: `deploy/deploy.sh` also rehydrates profiles and runs
  `eval/scripts/test_validator.py`, `test_hts.py`, `test_hfs_server.py`.

## 3. Rollback

| Situation | Action |
|---|---|
| new image fails before any schema change (schema_version and `hm_schema_migrations` unchanged) | put the previous tags back in `.env`, `docker compose up -d hts hfs && docker compose restart nginx` |
| a schema change ran (version or fork-migration list changed) | **restore the backup from step 1.1** (below). Images from `hm/p3-pgops` on refuse to start on a newer schema (`Database schema version N is newer than this build supports`); older images would start and silently miss what the migration maintains. `HFS_PG_ALLOW_NEWER_SCHEMA=true` is only for releases whose notes declare the schema backward compatible |
| data written after the upgrade must be kept | restore the backup, then re-play from the integrating systems' queues, or use point-in-time recovery to a moment just before the upgrade and accept the loss after it ([backup-restore.md](backup-restore.md)) |

Restore for rollback:

```bash
docker compose stop hfs hts
PG_CONTAINER=hfs-db PGUSER=postgres scripts/ops/pg-restore.sh /backup/hfs/dumps/hfs-<stamp>.dump hfs --replace --owner hfs
PG_CONTAINER=hfs-db PGUSER=postgres scripts/ops/pg-restore.sh /backup/hfs/dumps/hts-<stamp>.dump hts --replace --owner hts
# previous tags in .env, then
docker compose up -d hts hfs && docker compose restart nginx
```

(`--owner` creates the database owned by the application role and restores
every object as that role, so HFS/HTS can run their migrations afterwards.)

## 4. Upgrading PostgreSQL itself

- Minor (16.x → 16.y): new tag in `docker-compose.yml`, `docker compose up -d db`,
  then restart hfs/hts (their pools reconnect, but in-flight requests fail).
- Major (16 → 17): new data directory, `pg-backup.sh` → start the new major on
  the empty directory → `pg-restore.sh` both databases → switch. Allow the RTO
  measured in [backup-restore.md](backup-restore.md); `pg_upgrade --link` is
  faster for large databases and needs both majors' binaries.
