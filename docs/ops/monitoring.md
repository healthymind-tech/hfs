# Monitoring: Prometheus, Grafana, OTLP traces

Files:

| File | What |
|---|---|
| `docker/monitoring/prometheus.yml` | scrape config: hfs, hts, postgres-exporter, node-exporter |
| `docker/monitoring/alerts.yml` | alert + recording rules (this page is their runbook) |
| `docker/monitoring/grafana/dashboards/helios-ops.json` | dashboard "Helios FHIR (HFS / HTS / PostgreSQL)" |
| `docker/monitoring/grafana/provisioning/` | datasource + dashboard provisioning |
| `docker/monitoring/otel-collector.yml` | OTLP → tracing backend |
| `docker/production/docker-compose.yml` | profiles `monitoring` and `tracing` run all of the above |

```bash
cd docker/production
docker compose --profile monitoring up -d        # Prometheus :9090, Grafana :3000 (localhost only)
docker compose --profile tracing up -d           # otel-collector + Jaeger UI :16686
```

## 1. What the servers export

Both `hfs` (port 8080) and `hts` (port 8090) serve `GET /metrics` in Prometheus
text format, unauthenticated, with a global `service="hfs|hts"` label.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `http_requests_total` | counter | method, route, status | requests by **route template** (`/{resource_type}/{id}`), never by concrete URL. Before this branch every HFS FHIR request was labelled `route="<unmatched>"` (the FHIR router is mounted as a fallback service); fixed by `expose_matched_path` |
| `http_request_duration_seconds` | histogram | method, route, status | latency, buckets 5 ms … 10 s |
| `uptime_seconds` | gauge | | process uptime; `resets()` counts restarts |
| `build_info` | gauge (=1) | version, revision | which build is running (new in this branch) |
| `postgres_pool_max_connections` / `postgres_pool_connections` / `postgres_pool_available_connections` / `postgres_pool_waiting_callers` | gauge | pool | PostgreSQL pool occupancy sampled at every scrape (upstream #1907, cherry-picked; HTS pools added in this branch). `pool` = `primary`, `audit` (HFS), `terminology`, `resources` (HTS) |
| `dashboard_*` | various | | the web console's resource-count reconcile loop (hfs) |
| `composite_secondary_sync_*` | counter/gauge | backend, operation | only with a composite (Elasticsearch) backend |

`/metrics` carries no tenant, resource type, id or query string, by design.
It still reveals traffic volume and the software version, so do **not** expose
it through the public proxy: `docker/production/nginx.conf` answers 404 for
`/metrics`, `/fhir/metrics` and `/tx/metrics`. The evaluation stack
(`hfs-eval/nginx/nginx.conf`) currently proxies `/fhir/metrics` to the
internet — fix listed in the go-live checklist.

PostgreSQL metrics come from `postgres-exporter` (`pg_up`,
`pg_stat_database_*`, `pg_stat_activity_*`, `pg_stat_archiver_*`,
`pg_database_size_bytes`, `pg_stat_statements_*`), host metrics and the backup
freshness metrics (`hfs_backup_*`, written by `scripts/ops/pg-backup.sh` into
the node-exporter textfile directory) from `node-exporter`.

## 2. Traces (OTLP)

The request middleware opens one span per request (`http.request` with
`http.method`, `http.route`, `tenant`, `http.status_code`) and exports it over
OTLP/gRPC when **both**:

1. the binary was built with the `otel` feature (`helios-hfs/otel`,
   `helios-hts/otel`) — the production images built by
   `hfs-eval/deploy/remote-build.sh` today do **not** include it; add
   `helios-hfs/otel,helios-hts/otel` to its `FEATURES`;
2. `OTEL_EXPORTER_OTLP_ENDPOINT` is set (e.g. `http://otel-collector:4317`).
   `OTEL_SERVICE_NAME` overrides the service name.

The server logs `OTLP trace export enabled` at startup when both hold. Without
an exporter the span is not even created (`HELIOS_OBS_MODE=default`), so the
feature costs nothing when unused. OTLP *metrics* are not pushed; an
OpenTelemetry Collector can scrape `/metrics` with its `prometheus` receiver if
a site wants metrics over OTLP.

## 3. Verification (2026-11-15, this branch, test box)

Setup: HFS on the 2.6M-resource database and on the production copy, HTS on
the production copy, both with `OTEL_EXPORTER_OTLP_ENDPOINT` pointing at
Jaeger all-in-one, `postgres-exporter` v0.17.1, Prometheus v3.5.0 loading
`docker/monitoring/alerts.yml` (`promtool check rules`: 20 rules, SUCCESS).

- Prometheus targets `hfs` ×2, `hts`, `postgres`: all `up`; 6 rule groups /
  20 rules loaded with `health=ok`; `HtsDown` fired while HTS was still
  starting and resolved once it listened (the alert works).
- Recording rule `service:http_requests:rate5m` returned series for both
  `hfs` and `hts`; `pg_up` = 1; `sum(pg_stat_activity_count)` = 20–24.
- `build_info{service="hfs",version="0.2.4",revision="3e4902294"} 1`; HTS
  reports `revision="unknown"` (it has no build script embedding the commit).
- `postgres_pool_*`: HFS `pool="primary"`, HTS `pool="terminology"` (max 16)
  and `pool="resources"` (max 8, from `HFS_PG_MAX_CONNECTIONS=8`). Under 24
  concurrent clients against a 4-connection HFS pool the gauges showed the pool
  full and callers waiting (transcript in the Line D report).
- HTS `http_requests_total` carried real route templates (`/ValueSet/$expand`,
  `/CodeSystem/$lookup`). HFS FHIR requests were all `<unmatched>` — fixed in
  this branch and re-verified (route templates such as `/{resource_type}`,
  `/{resource_type}/{id}`, `/{resource_type}/{id}/$everything`).
- Jaeger listed services `hfs` and `hts`; spans `http.request` with
  `http.method`, `http.route`, `http.status_code`.
- The production stack (`hfs-eval`, images built without `otel`) exports
  `/metrics` but cannot export traces until the image is rebuilt with the
  feature.

## 4. Runbook

### HfsDown / HtsDown
`/metrics` unreachable for 2 min. `docker compose ps`, `docker compose logs
--tail 200 hfs`. Common causes: PostgreSQL down (`PostgresDown` fires too),
startup migration still running (log shows `Applied fork schema migration` /
migration lines; wait), schema newer than the image (log: `Database schema
version N is newer than this build supports` — you started an older image
after an upgrade; see [upgrade.md](upgrade.md) rollback), OOM kill (`dmesg`,
`docker inspect --format '{{.State.OOMKilled}}'`).

### HeliosRestarting
More than two restarts in 30 min: crash loop. Check the last log lines before
each restart and `OOMKilled`. Large transaction Bundles peak at 3–5 GB RSS.

### HeliosHigh5xxRate
More than 5% 5xx for 10 min. Split by route in Grafana ("Requests / s by
service and status", then Explore `sum by (route) (rate(http_requests_total{status=~"5.."}[5m]))`).
503 with `Unavailable` → pool exhaustion (`PostgresPoolWaiting`); 500 `rolled back`
on transactions → statement timeout on the write gate (concurrent bulk loads;
clients must retry, or raise `HFS_PG_STATEMENT_TIMEOUT_MS`); HTS 5xx on
`$expand` → check HTS logs for the terminology system.

### HeliosSlowRequests
p95 above 2 s for 15 min. "Slowest routes" panel names the route; the
PostgreSQL log has every statement over 1 s (`log_min_duration_statement`);
`pg_stat_statements` ranks them ([postgresql.md §4](postgresql.md#4-slow-query-review-loop)).
After a bulk load run `ANALYZE` first — stale statistics are the most common cause.

### PostgresPoolWaiting / PostgresPoolSaturated
Requests are queueing for a PostgreSQL connection. If PostgreSQL CPU and I/O
are low, raise the pool (`HFS_PG_MAX_CONNECTIONS`, `HTS_PG_POOL_SIZE`) within
the `max_connections` budget ([postgresql.md §2](postgresql.md#2-connection-pools-and-timeouts)).
If PostgreSQL is busy, a slow query is holding connections: find it in
`pg_stat_activity` (`SELECT pid, now()-query_start, left(query,120) FROM pg_stat_activity WHERE state='active' ORDER BY 2 DESC`).

### PostgresDown
`pg_up == 0`. `docker compose logs db`. Disk full (WAL directory) is the
classic cause when archiving is failing — check `PostgresWalArchiveFailing`
and free space on `PG_DATA_DIR`. Never delete files from `pg_wal` by hand.

### PostgresConnectionsNearMax
Above 80% of `max_connections`. Usually an extra HFS/HTS instance or a pool
set too large; `SELECT usename, application_name, state, count(*) FROM pg_stat_activity GROUP BY 1,2,3`.

### PostgresLongTransaction
A transaction open > 15 min blocks VACUUM and can hold locks. Identify it in
`pg_stat_activity` (`xact_start`); a long `$reindex`, bulk submit or
`hts import` is expected — anything else, `SELECT pg_cancel_backend(pid)`.

### PostgresDeadlocks
More than 3 in 15 min. PostgreSQL resolves them by aborting one transaction
(the client gets a 500/409 and should retry). Persistent deadlocks point to two
writers updating the same resources in different orders (e.g. two bulk loaders
over overlapping data) — log `log_lock_waits` lines show the statements.

### PostgresWalArchiveFailing / PostgresWalArchiveStale
PITR is degrading and `pg_wal` will grow until the disk fills. Check the
archive target (mounted? full? permissions — owned by uid 70?):
`docker compose exec db ls -la /wal-archive | tail`,
`SELECT * FROM pg_stat_archiver`. PostgreSQL retries the same segment until it
succeeds; fixing the target is enough.

### PostgresCacheHitLow
Informational: the working set no longer fits in memory. More RAM /
`shared_buffers`, or look for a query scanning a big table.

### BackupMissing
No successful `pg-backup.sh` in 26 h, or the last run failed. Check the cron/
systemd timer log; run `scripts/ops/pg-backup.sh` by hand
([backup-restore.md](backup-restore.md)).

### DiskWillFillIn3Days
Linear prediction from the last 6 h. Usual suspects: WAL archive pruning not
running, bulk export output (`/data/export`), docker logs, a second SNOMED/LOINC
version imported into HTS (~1.4 GB each).
