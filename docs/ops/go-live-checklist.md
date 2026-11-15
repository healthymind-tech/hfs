# Go-live checklist (replacing HAPI FHIR)

Tick every line, with the evidence (command output, ticket, screenshot) linked
in the go-live ticket. "Owner" is a role; fill in names per site.

## A. Decisions and sign-off

- [ ] Scope agreed: which integrations move from HAPI to HFS on day 1, which later (Owner: project lead)
- [ ] Data migration plan from HAPI agreed (export `$export` / per-type paging → HFS transaction Bundles or `$bulk-submit`), incl. id preservation and reference rewriting (project lead + integration)
- [ ] Rollback plan to HAPI agreed and rehearsed: HAPI kept read-only for ≥ 30 days, writes dual-routed or replayable (integration)
- [ ] Downtime window and communication plan approved (IT ops)

## B. Security (blocking)

- [ ] **Authentication for FHIR REST**: SMART on FHIR / OIDC enabled (`HFS_AUTH_*`, Phase 3 item 2) — or a written risk acceptance that the service is reachable only from named hospital network segments (security officer)
- [ ] Admin passwords (`ADMIN_PASSWORD`, DB passwords, Grafana) generated, stored in the password vault, `.env` mode 600
- [ ] TLS at the edge with the hospital certificate; HTTP→HTTPS redirect; `HFS_ADMIN_COOKIE_SECURE=true`
- [ ] `/metrics` not reachable from outside (`curl https://<host>/fhir/metrics` → 404). **The evaluation stack proxies it today** (`hfs-eval/nginx/nginx.conf`)
- [ ] `TRUSTED_PROXIES` lists exactly the edge proxies
- [ ] Rate limiting active at nginx (`limit_req` in `docker/production/nginx.conf`)
- [ ] AuditEvent recording enabled and retention decided (Phase 3 item 3; `HFS_AUDIT_*`)
- [ ] PostgreSQL not published on any host port; per-server DB roles (`hfs`, `hts`)

## C. Platform

- [ ] Host sized per [deployment.md §1](deployment.md#1-sizing); PostgreSQL data on SSD/NVMe (**production is on an HDD today**)
- [ ] Images pinned by tag, built from a tagged commit of `hm/main`; tag recorded
- [ ] Images include the `otel` feature if tracing is wanted
- [ ] `docker compose config` clean; containers restart on reboot (`restart: unless-stopped`, docker enabled at boot)
- [ ] NTP synchronised on the host
- [ ] Log rotation (compose `x-logging`) and log shipping to the hospital SIEM

## D. Database

- [ ] HFS schema on the fork migration track: `SELECT version FROM schema_version` = 46 and `hm_schema_migrations` contains `hm-0001-search-parameter-epochs` ([database-migrations.md](database-migrations.md))
- [ ] Server parameters as in [postgresql.md §1](postgresql.md#1-server-parameters), adjusted to the host
- [ ] Pool budget computed: Σ pools + 20 < `max_connections` ([postgresql.md §2](postgresql.md#2-connection-pools-and-timeouts)); `HFS_PG_MAX_CONNECTIONS` set explicitly in the HTS container
- [ ] `pg_stat_statements` and slow-query log active
- [ ] After the HAPI data load: `VACUUM (ANALYZE)`; search benchmark (`scripts/ops/search-bench.py`) p95 within targets on the real data
- [ ] No invalid indexes (`SELECT indexrelid::regclass FROM pg_index WHERE NOT indisvalid` empty)

## E. Backup and recovery (blocking)

- [ ] Nightly `pg-backup.sh` cron installed, dumps copied off-host, `hfs_backup_*` metrics visible
- [ ] WAL archiving on, archive on a separate disk/mount, `pg_stat_archiver.failed_count` = 0
- [ ] Weekly `pg-basebackup.sh` + daily `wal-prune.sh` installed
- [ ] **Restore drill done on the production host class within the last 30 days**: logical restore and PITR to a chosen timestamp, timings recorded against the RTO ([backup-restore.md](backup-restore.md))
- [ ] RPO/RTO accepted by the business owner (default: RPO ≤ 5 min with PITR, RTO ≤ 1 h at ≤ 10M resources)

## F. Monitoring and on-call

- [ ] Prometheus scraping hfs, hts, postgres-exporter, node-exporter (`up` = 1 for all)
- [ ] Alert rules loaded (`/alerts` shows the groups in `docker/monitoring/alerts.yml`), routed to the on-call channel, one test alert received
- [ ] Grafana dashboard "Helios FHIR" imported; baseline recorded for a week before go-live
- [ ] On-call runbook = [monitoring.md §4](monitoring.md#4-runbook); escalation contacts listed

## G. Content

- [ ] Terminologies loaded and verified: SNOMED CT (version), LOINC (version), ICD-10-CM/PCS (FY), zh-Hant supplements, CVX, ISO 4217 ([terminology-updates.md](terminology-updates.md))
- [ ] Release calendar owners assigned ([terminology-release-calendar.md](terminology-release-calendar.md))
- [ ] IGs installed at the versions the senders use ([ig-update-sop.md](ig-update-sop.md)); examples validate
- [ ] `HFS_VALIDATION_MODE` decided per integration (off / log / enforce) and documented
- [ ] Custom SearchParameters the hospital needs installed and reindexed

## H. Functional acceptance

- [ ] Each integration's message set replayed against HFS in staging with identical results to HAPI (reads, searches, `_include`/`_revinclude`, paging, conditional create/update)
- [ ] `Patient/$everything`, `$export` on hospital data volume within agreed time
- [ ] `$validate` results reviewed for the TW Core profiles in use
- [ ] Web consoles (`/ui`, `/ui/hts`) behind admin sign-in
- [ ] Load test at 2× expected peak: p95 and 5xx ratio within targets, `postgres_pool_waiting_callers` stays 0

## I. Cut-over day

- [ ] Final HAPI export + HFS load (or delta), counts reconciled per resource type
- [ ] Fresh `pg-backup.sh` after the load (first production restore point)
- [ ] Integrations switched one by one, each verified (first message round-trip)
- [ ] Watch Grafana for 2 hours; hypercare for 2 weeks with daily review of error ratio, slow queries, backups

## J. Process

- [ ] [Upgrade procedure](upgrade.md) rehearsed once on staging
- [ ] [Upstream merge plan](upstream-rebase.md) scheduled (fork is 4 upstream releases behind)
- [ ] Change management: every image, IG and terminology change goes through a ticket with backup + verification evidence
