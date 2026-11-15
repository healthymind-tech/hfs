# Running HFS and HTS as several instances (cluster)

This fork can run **N HFS instances and M HTS instances behind one load
balancer, all sharing one PostgreSQL**. This page says what to configure,
what is shared and how, what is still single-instance, and how to restart
without downtime. `book/src/ch15-cluster-deployment.md` (from upstream) has
the general background; this page is authoritative for this fork.

## Topology

```
                 TLS edge / hospital reverse proxy
                               │
                         nginx (LB, :80)
          ┌────────────────────┼───────────────────────┐
     /fhir/, /ui          /tx/, /ui/hts           (health checks)
   ┌──────┼──────┐        ┌────┴────┐
 hfs-1  hfs-2  hfs-3     hts-1    hts-2
   └──────┴──────┴────┬───┴─────────┘
                      │
              PostgreSQL 16 (databases hfs, hts)
              shared volume: bulk export output, IG package cache
```

- Every instance is identical and stateless apart from caches that are kept
  coherent through the database (below). Any request may go to any
  instance; no sticky sessions are needed.
- **PostgreSQL is required.** SQLite cannot be shared; `HFS_CLUSTER=true`
  refuses to start on it.
- Two instances of each are enough for availability; three HFS instances
  give headroom for a rolling restart under load.

## Configuration

### HFS (every instance)

| Variable | Value | Why |
|---|---|---|
| `HFS_CLUSTER` | `true` | Turns on the cluster-safe defaults and the startup refusal of unsafe settings (SQLite, `HFS_SUBSCRIPTIONS_FANOUT=memory`, node-local bulk output, file audit sink). |
| `HFS_STORAGE_BACKEND` / `HFS_DATABASE_URL` | `postgres` / the shared database | |
| `HFS_INSTANCE_ID` | optional, e.g. `hfs-1` | Name in package-job and lock bookkeeping (defaults to the host name). |
| `HFS_CLUSTER_SYNC_INTERVAL_MS` | `1000` (default) | How often each instance polls for StructureDefinitions / SearchParameters written by another instance. Upper bound on how long a new profile is unknown elsewhere. |
| `HFS_SUBSCRIPTIONS_ENABLED` / `HFS_SUBSCRIPTIONS_FANOUT` | `true` / unset (→ `pg-notify`) | Subscription delivery across instances (LISTEN/NOTIFY + shared outbox). |
| `HFS_BULK_EXPORT_OUTPUT_DIR` + `HFS_BULK_EXPORT_OUTPUT_SHARED=true` | a directory on a volume every instance mounts | Any instance can serve any `$export` file. (Or `HFS_BULK_EXPORT_OUTPUT_BACKEND=s3`.) |
| `HFS_BULK_SUBMIT_OUTPUT_DIR` + `HFS_BULK_SUBMIT_OUTPUT_SHARED=true` | on the shared volume | Bulk submit is enabled by default; `HFS_CLUSTER` refuses node-local output (or `HFS_BULK_SUBMIT_ENABLED=false`, or `s3`). |
| `HFS_PACKAGE_CACHE_DIR` | on the shared volume | An uploaded `.tgz` is installable from whichever instance receives the install request; registry downloads happen once. |
| `HFS_VALIDATION_PROFILE_CACHE` | **per instance** (not shared) | The compiled-profile cache file; each instance writes its own. |
| `HFS_PG_MAX_CONNECTIONS` | e.g. `16`–`32` | Per instance. Size PostgreSQL `max_connections` ≥ N × this + HTS pools + 20. |
| `HFS_AUDIT_BACKEND` | `database` (or none) | `file` is refused under `HFS_CLUSTER`. |
| `HFS_ADMIN_PASSWORD` (+ `_TRUSTED_PROXIES`, `_COOKIE_SECURE`) | same on every instance | Sessions are shared through the database (below). |

### HTS (every instance)

| Variable | Value | Why |
|---|---|---|
| `HTS_STORAGE_BACKEND` / `HTS_DATABASE_URL` | `postgres` / the shared database | |
| `HTS_CACHE_SYNC_INTERVAL_MS` | `1000` (default) | Poll of the shared `terminology_epoch`; bounds cross-replica staleness of `$expand` / `$validate-code` / `$lookup` / `$translate` / `$subsumes`. |
| `HTS_ADMIN_PASSWORD` | same on every instance | Shared sessions. |
| `HTS_ADMIN_UI` | `true` on **one** instance (see limitations) | Terminology import jobs run in the process and keep their state in `HTS_ADMIN_DATA_DIR`. |

## What is shared, and how

| State | Mechanism | Staleness bound |
|---|---|---|
| Resources, history, versions | PostgreSQL rows; per-resource advisory write lock + version check | none (transactional) |
| Server-assigned ids | UUIDv4 | none |
| Conditional create / update | serialized by transaction-scoped advisory locks in the database (per tenant), so the condition check and the write are atomic across instances | none |
| Validation profile registry (stored StructureDefinitions, IG installs/uninstalls) | `resource_change_feed` (trigger on `resources`, consumed by transaction id) polled by every instance | `HFS_CLUSTER_SYNC_INTERVAL_MS` |
| SearchParameter registry (query side) | trigger-maintained `search_parameter_epochs`, polled | `HFS_CLUSTER_SYNC_INTERVAL_MS` (write side was already exact) |
| Subscriptions: topics, subscriptions, event numbers, failure streaks, status | `subscription_state`, LISTEN/NOTIFY lifecycle fan-out, startup hydration | ~ms (NOTIFY) |
| Subscription delivery (rest-hook, email, messaging) | durable `subscription_delivery_outbox`, claimed with `FOR UPDATE SKIP LOCKED` + fencing token, persisted retry schedule | delivered once by one instance; survives restarts |
| WebSocket notifications | event stored once, NOTIFY to every instance, delivered by the instance holding the socket; binding tokens in `subscription_ws_tokens` (redeem once, any instance) | ~ms |
| Bulk `$export` jobs | database job store (lease + fencing); files on the shared volume or S3 | none |
| Admin sessions + sign-in lockout (HFS and HTS) | `admin_sessions`, `admin_login_failures` | sign-in: immediate; sign-out reaches other instances within 10 s |
| IG package jobs | cluster-wide advisory lock (one install at a time), `hfs_package_jobs` mirror (status, log, cancel flag) | ~1 s |
| `HFS_PACKAGES` startup installs | separate advisory lock: instances booting together take turns | none |
| HTS response caches | `terminology_epoch` (triggers on header tables + bump after each write), polled | `HTS_CACHE_SYNC_INTERVAL_MS` |
| HTS bootstrap import | advisory lock around the directory sync | none |
| PostgreSQL schema migrations | advisory lock | none |

The full per-process state inventory (what is safe to keep per process and
why) is in the Line F report, `hfs-eval/eval/results-p3-cluster/REPORT.md`.

## Known single-instance features

Route these to one instance (nginx `location` → a one-server upstream) or
avoid them in a cluster:

- **SQL-on-FHIR async export** (`$viewdefinition-export`, `$sqlquery-export`):
  job state in memory (upstream #169; upstream's database controller is not
  ported). Synchronous `$sql-run` and Bulk `$export` are fine.
- **`$reindex` status/cancel**: the job runs on the instance that accepted it;
  poll that instance. The reindex itself is database-wide and correct.
- **HTS terminology administration** (`/admin`, the `/ui/hts` management
  pages: uploads, import jobs, provenance, audit trail files in
  `HTS_ADMIN_DATA_DIR`): enable `HTS_ADMIN_UI` on one HTS instance and route
  `/tx/admin` and `/ui/hts` there. Terminology *reads and writes* through the
  FHIR API are cluster-safe.
- **HTS `$closure`** tables are per process (stateful client protocol).
- **Rate limits** (`$nl-search`, bulk-submit polling) are per instance, so the
  effective limit is N × the configured value.
- Prometheus `/metrics` and dashboard counters are per instance by design;
  scrape every instance.

## Load balancer

```nginx
upstream hfs {
    server hfs-1:8080 max_fails=1 fail_timeout=5s;
    server hfs-2:8080 max_fails=1 fail_timeout=5s;
    server hfs-3:8080 max_fails=1 fail_timeout=5s;
}
upstream hts {
    server hts-1:8090 max_fails=1 fail_timeout=5s;
    server hts-2:8090 max_fails=1 fail_timeout=5s;
}
upstream hts_admin { server hts-1:8090; }   # HTS admin on one instance

map $http_upgrade $connection_upgrade { default upgrade; '' close; }

server {
    listen 80;
    client_max_body_size 0;
    proxy_read_timeout 600;
    proxy_http_version 1.1;
    proxy_set_header Host $http_host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;
    # A request that could not reach an instance (restarting, crashed) is
    # retried on the next one. Requests already sent are retried only when
    # idempotent, so a POST is never applied twice.
    proxy_next_upstream error timeout http_502 http_503;
    proxy_next_upstream_tries 3;

    location /fhir/ {
        proxy_pass http://hfs/;
        # WebSocket subscriptions (any instance can hold the socket).
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
    }
    location /ui/hts { proxy_pass http://hts_admin; }
    location /tx/admin/ { proxy_pass http://hts_admin/admin/; }
    location /tx/ { proxy_pass http://hts/; }
    location /ui { proxy_pass http://hfs; }
    location / { proxy_pass http://hfs; }
}
```

## Rolling restart (zero downtime)

On `SIGTERM` an instance stops accepting connections, drains in-flight
requests and exits; nginx retries refused connections on the next instance.
Restart one instance at a time, wait until it answers `/health`, **and then
wait longer than nginx's `fail_timeout`** before stopping the next one: nginx
marks a refusing instance down for `fail_timeout`, and if the next instance
goes down inside that window there is briefly no live upstream (measured: an
HTS restart takes ~1 s, so without the pause 1,096 of 2,326 requests failed;
with a 6 s pause, 0 of 3,577).

```bash
for i in 1 2 3; do
  docker compose up -d --no-deps --force-recreate hfs-$i
  until curl -sf http://hfs-$i:8080/health >/dev/null; do sleep 1; done
  sleep 6   # > fail_timeout (5 s)
done
```

Measured on the test cluster (3 HFS, 2 HTS, debug builds): 2,687 HFS
requests (reads + creates through nginx) during a full HFS rolling restart,
0 failed; 3,577 HTS requests during an HTS rolling restart, 0 failed.

Schema migrations run under an advisory lock at startup: the first upgraded
instance migrates, the others wait. Only additive migrations are safe while
old instances still run; this fork's cluster migrations (fork track `hm-0002`…`hm-0005` in `hm_schema_migrations`, see database-migrations.md) only add
tables, indexes and triggers. Read the release notes of any upgrade that
alters existing tables before rolling it.

## Sample compose (N = 3 HFS, 2 HTS)

```yaml
name: hfs-cluster

x-hfs: &hfs
  image: healthymind/hfs:${TAG}
  environment: &hfs-env
    HFS_CLUSTER: "true"
    HFS_STORAGE_BACKEND: postgres
    HFS_DATABASE_URL: postgres://hfs:${HFS_PG_PASSWORD}@pg:5432/hfs
    HFS_PG_MAX_CONNECTIONS: "24"
    HFS_BASE_URL: https://fhir.hospital.example/fhir
    HFS_DEFAULT_FHIR_VERSION: R4
    HFS_TERMINOLOGY_SERVER: http://nginx/tx
    HFS_TERMINOLOGY_SERVER_TOKEN: ${ADMIN_PASSWORD}
    HFS_VALIDATION_STORED_PROFILES: "true"
    HFS_VALIDATION_PROFILE_CACHE: /local/validation-cache
    HFS_SUBSCRIPTIONS_ENABLED: "true"
    HFS_ADMIN_UI: "true"
    HFS_ADMIN_PASSWORD: ${ADMIN_PASSWORD}
    HFS_ADMIN_COOKIE_SECURE: "true"
    HFS_PACKAGE_CACHE_DIR: /shared/packages
    HFS_BULK_EXPORT_OUTPUT_DIR: /shared/export
    HFS_BULK_EXPORT_OUTPUT_SHARED: "true"
    HFS_AUDIT_BACKEND: database
  volumes:
    - shared:/shared
  depends_on:
    pg: { condition: service_healthy }
  restart: unless-stopped
  stop_grace_period: 60s

x-hts: &hts
  image: healthymind/hts:${TAG}
  environment: &hts-env
    HTS_STORAGE_BACKEND: postgres
    HTS_DATABASE_URL: postgresql://hts:${HTS_PG_PASSWORD}@pg:5432/hts
    HTS_ADMIN_PASSWORD: ${ADMIN_PASSWORD}
    HTS_ADMIN_COOKIE_SECURE: "true"
    HTS_REQUEST_TIMEOUT: "600"
    HTS_MAX_BODY_SIZE: "268435456"
  depends_on:
    pg: { condition: service_healthy }
  restart: unless-stopped
  stop_grace_period: 60s

services:
  pg:
    image: postgres:16-alpine
    environment:
      POSTGRES_USER: hfs
      POSTGRES_PASSWORD: ${HFS_PG_PASSWORD}
      POSTGRES_DB: hfs
    command: postgres -c max_connections=300 -c shared_buffers=6GB
    volumes: [pg-data:/var/lib/postgresql/data]
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U hfs -d hfs"]
      interval: 5s
      retries: 30

  hfs-1: { <<: *hfs, environment: { <<: *hfs-env, HFS_INSTANCE_ID: hfs-1 }, volumes: [shared:/shared, hfs1-local:/local] }
  hfs-2: { <<: *hfs, environment: { <<: *hfs-env, HFS_INSTANCE_ID: hfs-2 }, volumes: [shared:/shared, hfs2-local:/local] }
  hfs-3: { <<: *hfs, environment: { <<: *hfs-env, HFS_INSTANCE_ID: hfs-3 }, volumes: [shared:/shared, hfs3-local:/local] }

  hts-1:   # the HTS admin instance
    <<: *hts
    environment: { <<: *hts-env, HTS_ADMIN_UI: "true", HTS_ADMIN_DATA_DIR: /data/hts-admin }
    volumes: [hts1-data:/data]
  hts-2: { <<: *hts }

  nginx:
    image: nginx:1.27-alpine
    ports: ["8082:80"]
    volumes: [./nginx/cluster.conf:/etc/nginx/conf.d/default.conf:ro]
    depends_on: [hfs-1, hfs-2, hfs-3, hts-1, hts-2]

volumes:
  pg-data:
  shared:        # in production: an NFS / CephFS volume, or use S3 for bulk output
  hfs1-local:
  hfs2-local:
  hfs3-local:
  hts1-data:
```

The HTS database (`hts`, owner `hts`) is created once, as for the
single-instance deployment. For more instances add `hfs-4` etc. to the
service list and the nginx upstream; no other change is needed.

## Verifying a cluster

`hfs-eval/eval/scripts/p3-cluster/cluster_tests.py` runs the end-to-end
checks used for this fork (parallel conflicting updates, conditional
creates, profile / SearchParameter / terminology coherence, shared admin
sessions and lockout, package jobs, subscription delivery, bulk export,
rolling restart under load) against a running cluster.
