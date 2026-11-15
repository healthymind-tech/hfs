# Audit trail (AuditEvent, IHE BALP)

HFS records every FHIR REST interaction as a FHIR `AuditEvent` that follows
the [IHE Basic Audit Log Patterns (BALP)](https://profiles.ihe.net/ITI/BALP/)
profiles, stores the events in its own database where they can be searched
through the FHIR API, and purges them after a configurable retention period.

This page is for operators: what is recorded, how to configure it, what to
run in production, and what it costs.

## What is recorded

One `AuditEvent` per interaction, written in the tenant the request addressed:

| Interaction | `subtype` | `action` | BALP profile (`IHE.BasicAudit.*`) |
|---|---|---|---|
| read, vread | `read`, `vread` | R | `Read` / `PatientRead` |
| create, update-as-create | `create` | C | `Create` / `PatientCreate` |
| update, patch | `update`, `patch` | U | `Update` / `PatientUpdate` |
| delete (incl. conditional) | `delete` | D | `Delete` / `PatientDelete` |
| search (GET and POST `_search`), compartment search | `search-type`, `search-system` | E | `Query` / `PatientQuery` |
| history (instance, type, system) | `history-*` | E | `Query` / `PatientQuery` |
| operations (`$everything`, `$export`, `$validate`, `$purge`, ...) | `operation` | E | `Query` / `PatientQuery` |
| batch / transaction | one event **per entry**, of the entry's own interaction; correlated by `bundle-id`/`bundle-type`/`entry-index` details and the shared request id | | as above |
| admin API writes (`POST/PUT/DELETE /admin/...`), bulk status/download | none | E | none |

Each event carries:

- **agents** — the client (`network.address` = its IP; `who.identifier` = the
  OAuth client id when authenticated), the server (`Device/hfs`, its public
  base URL) and, with SMART/OIDC on, the **user** (`who.reference` =
  `fhirUser`, `who.identifier` = issuer + `sub`, `name`), typed as BALP
  requires (`IRCP` for reads/queries, `AUT` for writes);
- **entities** — the resource acted on (versioned for vread), the **patient**
  (from the path, from `patient`/`subject`/`_id` search parameters, or from
  the `subject`/`patient` of the resource read or written), the query (for
  searches, history and operations: the full request line, base64 in
  `entity.query` and readable in `entity.description`; for a form `POST
  _search` the form body too), and the `X-Request-Id` (taken from the
  request, or generated and returned in the response header);
- **outcome** — `0` success; `4` a 4xx response (401, 403, 404, 412, 422...);
  `8` a 5xx response. A failure also carries `outcomeDesc`
  (`HTTP 404 Not Found: <OperationOutcome text>`).

A BALP profile is claimed in `meta.profile` only when the event conforms to
it. BALP fixes `outcome` to `0`, so failed interactions are recorded without a
profile claim. Operations have no BALP profile of their own; they are recorded
on the Query pattern (BALP's Query profile leaves `subtype` open).

When an interaction names several patients (`Observation?patient=a,b`), one
event per patient is written, as BALP requires (at most 10 per request).

Not recorded (default exclusions): `/health`, `/_liveness`, `/_readiness`,
`/metrics`, `/metadata`, `/.well-known/*`, `/$versions`, `/console/*`, and
`GET`/`HEAD` on `/admin/*`. Add more with `HFS_AUDIT_EXCLUDE_PATHS`.

Server start-up and shutdown, `$purge`, `$reindex`, bulk export jobs, IG
package installs and tenant administration additionally record their own
`object` events, as before.

## Storage, search and protection

With `HFS_AUDIT_BACKEND=database` the events are `AuditEvent` resources in
the primary database (or a dedicated one, `HFS_AUDIT_DATABASE_URL`), written
in the **tenant of the request** — events of requests that never resolved a
tenant (a 401 before tenant resolution, server lifecycle) go to the default
tenant. They are searchable with the standard parameters:

```
GET /AuditEvent?patient=Patient/123&date=ge2026-11-01
GET /AuditEvent?entity=Observation/o1
GET /AuditEvent?agent=Practitioner/pr1
GET /AuditEvent?subtype=read&outcome=4
GET /AuditEvent?type=rest&action=E&_sort=-date
```

Protection:

- `PUT`, `PATCH`, `DELETE`, conditional update/delete, `$purge` and
  transaction entries that would change or remove an `AuditEvent` are refused
  (`405`); the retention job is the only way events leave the store.
- Reading the trail is an ordinary FHIR read: with SMART/OIDC on it needs an
  `AuditEvent` scope (`user/AuditEvent.rs` or `system/AuditEvent.rs`). Grant
  it to auditors only. A patient-context token can be given
  `patient/AuditEvent.rs` to see who accessed that patient's record.
- Searching the trail is itself audited (one event per search); writing an
  event is never audited (the sink writes to storage directly), so there is no
  recursion.
- `HFS_AUDIT_TENANT=<tenant>` puts **all** events into one tenant instead
  (e.g. a dedicated `audit` tenant that only auditors' tokens can address).
  A reserved id (`__system__`) makes the trail unreachable through the API —
  the pre-P3 behaviour; only do that with an external reader.

## Writing path, overhead and durability

The middleware builds the events after the response is known and pushes them
onto a bounded in-memory queue; background writers store them in batches.
Recording costs the request a queue push, not a database write.

- A full queue makes the request wait for room (backpressure) — no event is
  dropped.
- On graceful shutdown (SIGTERM/SIGINT) HFS stops accepting requests, drains
  in-flight ones, then waits up to `HFS_AUDIT_SHUTDOWN_TIMEOUT` (default 5 s)
  for the queue to empty; whatever is still queued then is appended to the
  fallback file (below) and stored at the next start. Keep
  `HFS_AUDIT_SHUTDOWN_TIMEOUT` below `HFS_SHUTDOWN_TIMEOUT` (default 9 s, the
  hard stop) and give the container a matching stop grace period
  (`stop_grace_period: 30s` in compose, `terminationGracePeriodSeconds` in
  Kubernetes) with `HFS_SHUTDOWN_TIMEOUT=25`.
- A write the database refuses is retried (3 attempts, 200 ms back-off) and
  then appended to the **fallback file** (`HFS_AUDIT_FALLBACK_FILE`, default
  `<HFS_DATA_DIR>/audit-fallback.ndjson`); the file is replayed into the
  database at the next start and renamed `*.replayed-<timestamp>`.
- Events are written in batches, one database transaction per batch
  (SQLite/PostgreSQL), by `HFS_AUDIT_WRITERS` concurrent writers.
- Lost only on a hard kill (`SIGKILL`, OOM, power loss) while events are
  queued: at most the queue's content.

Measured overhead: see `hfs-eval/eval/results-p3-audit/REPORT.md` (target
< 10 % on p95 read and write latency).

## Retention

```
HFS_AUDIT_RETENTION_DAYS=2190              # keep 6 years
HFS_AUDIT_RETENTION_ARCHIVE_DIR=/archive/hfs-audit   # optional
HFS_AUDIT_RETENTION_INTERVAL_HOURS=24      # default
HFS_AUDIT_RETENTION_BATCH=1000             # default rows per transaction
```

A background job (first run one minute after start, then every interval)
deletes `AuditEvent`s whose stored version is older than the retention period,
in every tenant, oldest first, `BATCH` at a time (each batch its own
transaction: current row, history, search index and full-text rows). Age is
the server's write time (`meta.lastUpdated`), which for server-generated
events equals `recorded`.

With an archive directory each run writes one gzip NDJSON file
`auditevent-<UTC timestamp>.ndjson.gz`, one line per event:
`{"tenant": ..., "lastUpdated": ..., "resource": {AuditEvent}}`. A batch is
flushed and `fsync`ed to the archive **before** it is deleted; if the archive
cannot be written the run stops and deletes nothing more. To keep archives in
object storage, point the directory at a mounted bucket or sync it (e.g.
`aws s3 sync --storage-class GLACIER`, `rclone`) from a cron job.

Each run that removed something records an `AuditEvent` itself (action `D`,
detail `audit-operation=retention-purge`, `count`, `archive`).

Retention is implemented for the SQLite and PostgreSQL audit stores. With
several HFS instances on one database every instance runs the job; that is
safe (deletes are idempotent) but produces one archive per instance — enable
`HFS_AUDIT_RETENTION_DAYS` on one instance only.

The `file` backend is never purged by HFS; rotate it with `logrotate`.

## Configuration reference

| Variable | Default | Description |
|---|---|---|
| `HFS_AUDIT_BACKEND` | `none` | `none`, `file`, `database`, `cloudwatch` |
| `HFS_AUDIT_SOURCE_OBSERVER` | `Device/hfs` | server agent / `source.observer` |
| `HFS_AUDIT_TENANT` | *(request tenant)* | store every event in this tenant |
| `HFS_AUDIT_DATABASE_URL` | *(primary DB)* | dedicated audit database (not searchable through the API) |
| `HFS_AUDIT_FILE_PATH` | — | NDJSON file for `file` |
| `HFS_AUDIT_EXCLUDE_PATHS` | — | extra exclusions, comma separated: `/path`, `/prefix/*`, `GET /path` |
| `HFS_AUDIT_TRUST_FORWARDED_FOR` | `false` | client address from the first `X-Forwarded-For` hop (only behind a proxy that sets it) |
| `HFS_AUDIT_QUEUE_CAPACITY` | `10000` | events queued before requests wait |
| `HFS_AUDIT_BATCH_SIZE` | `64` | events per writer batch |
| `HFS_AUDIT_WRITERS` | `4` | concurrent writer tasks (database connections in use) |
| `HFS_AUDIT_SHUTDOWN_TIMEOUT` | `5` | seconds to drain the queue on shutdown before spilling the rest to the fallback file |
| `HFS_AUDIT_FALLBACK_FILE` | `<data dir>/audit-fallback.ndjson` | spill file while the database is down |
| `HFS_AUDIT_AUTH_EVENTS` | `false` | also record the auth layer's per-request authentication/authorization events (the REST events already carry the user and the 401/403) |
| `HFS_AUDIT_RETENTION_DAYS` | *(keep forever)* | purge events older than N days |
| `HFS_AUDIT_RETENTION_ARCHIVE_DIR` | — | archive purged events (gzip NDJSON) first |
| `HFS_AUDIT_RETENTION_INTERVAL_HOURS` | `24` | hours between purge runs |
| `HFS_AUDIT_RETENTION_BATCH` | `1000` | rows deleted per transaction |

## Recommended production configuration

```
HFS_AUDIT_BACKEND=database
HFS_AUDIT_SOURCE_OBSERVER=Device/hfs-prod
HFS_AUDIT_TRUST_FORWARDED_FOR=true          # HFS sits behind the reverse proxy
HFS_AUDIT_RETENTION_DAYS=2190               # 6 years; set to your legal requirement
HFS_AUDIT_RETENTION_ARCHIVE_DIR=/var/lib/hfs/audit-archive   # on a backed-up volume
HFS_AUDIT_FALLBACK_FILE=/var/lib/hfs/audit-fallback.ndjson   # persistent volume
HFS_AUDIT_WRITERS=4
```

- Mount the archive and fallback paths on a persistent, backed-up volume.
- Size: an event is ~1.5–2.5 KB of JSONB plus its search-index rows; budget
  ~4 KB per interaction on PostgreSQL. 100 000 interactions/day ≈ 0.4 GB/day,
  ~150 GB/year before retention. Watch the `resources` and `search_index`
  table sizes.
- Run `VACUUM (ANALYZE)` normally (autovacuum is enough); the daily purge
  frees space for reuse rather than shrinking files.
- Give auditors a role/scope with `AuditEvent` read; nobody needs write.
- Keep `/metrics`, `/health` probes excluded (default).

## Privacy notes

- The trail is itself sensitive health data: it says who looked at which
  patient. Treat the database, the archives and the fallback file like the
  clinical data (encryption at rest, access control, backups).
- Search requests are recorded verbatim, including identifying parameters
  (`name=`, `identifier=`, `birthdate=`) and form-encoded `POST _search`
  bodies (up to 8 KB). Resource **bodies** of reads and writes are never
  copied into the trail.
- Client IP addresses are recorded; behind a proxy only with
  `HFS_AUDIT_TRUST_FORWARDED_FOR=true`.
- Patient attribution comes from the request (path, parameters) and from the
  resource read or written. Search **results** are not inspected: a search
  that returns many patients' records without naming a patient is recorded as
  a `Query` without a patient entity.
- Deleting a tenant (`DELETE /admin/tenants/{id}?purge=true`) deletes that
  tenant's audit trail with it; archive first if you must keep it.
