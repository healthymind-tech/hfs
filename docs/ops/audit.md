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
- Sizing: the queue drains at `HFS_AUDIT_WRITERS` × `HFS_AUDIT_BATCH_SIZE`
  events per round of writes. When it cannot keep up, requests wait for room
  (`audit_queue_full_waits_total`, `audit_queue_blocked_seconds`) — the audit
  trail is never sampled. With `HFS_AUDIT_STORE=resource` (events stored as
  FHIR resources, the pre-table layout) a batch transaction takes one
  transaction-level advisory lock per event, so keep
  `HFS_AUDIT_WRITERS × HFS_AUDIT_BATCH_SIZE` (default 8 × 256 = 2,048) well
  below PostgreSQL's lock table, `max_locks_per_transaction × max_connections`
  (64 × 100 = 6,400 with stock settings; set `max_locks_per_transaction=256`
  or more on a server that also takes large transaction Bundles). The table
  store takes no advisory locks.
- Lost only on a hard kill (`SIGKILL`, OOM, power loss) while events are
  queued: at most the queue's content.

Measured overhead: see `hfs-eval/eval/results-p3-audit/REPORT.md` (target
< 10 % on p95 read and write latency).

## The audit tables (`HFS_AUDIT_STORE=table`)

On PostgreSQL the database backend stores events in two dedicated tables
(fork migration `hm-0030-audit-event-table`) instead of the general FHIR
resource tables:

| Table | Content |
|---|---|
| `audit_event` | one row per event: tenant, `recorded`, id, `action`, `outcome`, `type` code, `subtype` codes and the complete `AuditEvent` JSON (`jsonb`), partitioned by month on `recorded` (`audit_event_pYYYY_MM`, plus `audit_event_default`) |
| `audit_event_ref` | one narrow row per `entity.what` / `agent.who` reference or identifier of an event, for the `patient`, `entity` and `agent` searches; partitioned the same way |

An event is **not** a row of `resources`: it has no history row, no
`search_index` rows and no full-text row, and writing it takes neither the
tenant gate nor an advisory lock. A writer stores a whole batch
(`HFS_AUDIT_BATCH_SIZE`) with one statement and one commit. The event JSON is
exactly what the previous layout stored — the same builder produces it, BALP
profile claims included.

What the API serves from the tables (`crates/rest/src/handlers/audit_events.rs`):

- `GET /AuditEvent/{id}`, `GET /AuditEvent/{id}/_history/1` (an event has
  exactly one version; `meta.versionId` is `1` and `meta.lastUpdated` is
  `recorded`).
- `GET /AuditEvent?…`, `POST /AuditEvent/_search` and `GET
  /Patient/{id}/AuditEvent` with these parameters only: `_id`,
  `_lastUpdated`, `date` (all prefixes, repeatable), `patient`, `entity`,
  `agent` (a reference `Type/id`, a bare id, or the identifier value of an
  agent such as the token `sub`), `type`, `subtype`, `action`, `outcome`
  (tokens, `:not` supported), plus `_count`, `_sort=date|-date|
  _lastUpdated|-_lastUpdated` (default `-date`), `_total`, `_summary`
  (including `count`), `_elements` and `next` links.
- **Dropped** compared with events stored as resources: every other
  `AuditEvent` search parameter (`address`, `agent-name`, `agent-role`,
  `altid`, `entity-name`, `entity-role`, `entity-type`, `policy`, `site`,
  `source`), `_content` / `_text` / `_filter`, `_tag` / `_profile` /
  `_security`, chained and `_has` searches, `_include` / `_revinclude`, sorting
  by anything but the event time, all modifiers other than `:not` on tokens,
  and `GET /AuditEvent/{id}/_history`. Such a parameter is ignored with a
  warning entry in the Bundle, or refused with `400` under
  `Prefer: handling=strict`. Events in the tables are also not reachable
  through `_revinclude=AuditEvent:*` on another type, `$export`
  (`_type=AuditEvent`), endpoint duplicate/sync (which never carried the
  audit trail by default) or `$reindex`.
- Tenant and endpoint isolation are unchanged: every query carries the
  request's tenant, which is the leading column of every index.
- A token with a `patient/` scope sees the launch patient's events only.

**Immutability** is enforced twice: the REST layer refuses create, update,
patch and delete of `AuditEvent` as before, and a row trigger on both tables
refuses every `UPDATE` and every `DELETE` outside a maintenance transaction
(retention, tenant purge) — a client with plain SQL access cannot rewrite the
trail without first disabling the trigger, which requires table ownership.
For defence in depth run HFS with a role that does not own the tables and
grant it only `SELECT, INSERT, DELETE` on them.

**Transition from events stored as resources.** `hm-0030` is additive: it
creates the tables and touches nothing else, so the previous version keeps
working against a migrated database. From the first start of this version
new events go to the tables; the `AuditEvent` resources written earlier stay
where they are and every read (`/AuditEvent`, the console's Audit trail and
Activity pages, HTS's `/AuditEvent`) merges both sources in `recorded` order,
including paging and counts. The merge costs one extra indexed search per
request for as long as legacy events exist. To end the transition and reclaim
the space, move the legacy events:

```
HFS_AUDIT_BACKFILL=true      # background job; safe to interrupt and repeat
```

The job copies the legacy events into the tables (1,000 per batch, creating
the months' partitions) and erases them from `resources`,
`resource_history`, `search_index` and `resource_fts`; when none is left it
records `hm-0031-audit-backfill` in `hm_schema_migrations` (progress in
`hm_audit_backfill_state`) and logs the `VACUUM (ANALYZE)` to run. It is never
started automatically. Legacy events that are not moved still expire through
the row-by-row retention job below.

**Rollback.** Setting `HFS_AUDIT_STORE=resource` (or starting the previous
version) writes events as resources again; nothing needs restoring because
`hm-0030` is additive. Events already in the tables are then not visible
through the API until `table` is restored (they stay in the database). After
a backfill the previous version no longer sees the moved events either:
restore the pre-backfill dump if you need to go back past that point.

**Partitions.** The current month and the next two always exist: the
migration creates them, and every instance re-checks at start and every five
minutes (creation is serialized by an advisory lock, so any number of
instances may run it). An event whose month has no partition — a clock far
off, a replayed fallback file from long ago — is stored in the default
partition and moved when its partition is created. Nothing is lost if
maintenance does not run.

**`$everything` and compartment searches.** `AuditEvent` is a member of the
R4 Patient compartment, but the record of who looked at a chart is not part
of the chart. `GET /Patient/{id}/$everything` and `GET /Patient/{id}/*` leave
`AuditEvent` out unless the request names it (`_type=AuditEvent`, or the
explicit `GET /Patient/{id}/AuditEvent`); `HFS_COMPARTMENT_INCLUDE_AUDIT_EVENTS=true`
restores the previous behaviour. (With the table store
`$everything?_type=AuditEvent` returns legacy events only; use
`/AuditEvent?patient=` or `/Patient/{id}/AuditEvent`.)

SQLite, MongoDB and S3 keep storing events as resources whatever
`HFS_AUDIT_STORE` says. `HFS_AUDIT_DATABASE_URL` pointing at a PostgreSQL
database uses the tables there, and — new — those events are then served by
the API as well.

## Retention

```
HFS_AUDIT_RETENTION_DAYS=2190              # keep 6 years
HFS_AUDIT_RETENTION_ARCHIVE_DIR=/archive/hfs-audit   # optional
HFS_AUDIT_RETENTION_INTERVAL_HOURS=24      # default
HFS_AUDIT_RETENTION_BATCH=1000             # default rows per transaction
```

With the audit tables, retention works on whole months: a partition whose
month lies entirely before `now − RETENTION_DAYS` is archived (when an archive
directory is set) and then **dropped** — no row-by-row delete, no vacuum
debt. An event is therefore kept for at least the retention period and for at
most one month longer. Only one instance runs retention at a time (an
advisory lock), so a cluster writes one archive, not one per instance. Events
of the default partition and the legacy `AuditEvent` resources are purged row
by row, `BATCH` at a time, by age (`recorded` / `meta.lastUpdated`), oldest
first, in every tenant.

With an archive directory each run writes one gzip NDJSON file
`auditevent-<UTC timestamp>.ndjson.gz`, one line per event:
`{"tenant": ..., "lastUpdated": ..., "resource": {AuditEvent}}`. A batch is
flushed and `fsync`ed to the archive **before** it is deleted (a partition is
dropped only after every one of its events is in the archive); if the archive
cannot be written the run stops and removes nothing more. To keep archives in
object storage, point the directory at a mounted bucket or sync it (e.g.
`aws s3 sync --storage-class GLACIER`, `rclone`) from a cron job.

Each run that removed something records an `AuditEvent` itself (action `D`,
detail `audit-operation=retention-purge`, `count`, `archive`).

Retention is implemented for the SQLite and PostgreSQL audit stores. With
several HFS instances on one database every instance runs the job; that is
safe (deletes are idempotent) but produces one archive per instance — enable
`HFS_AUDIT_RETENTION_DAYS` on one instance only.

The `file` backend is never purged by HFS; rotate it with `logrotate`.

## Terminology server (HTS)

HTS records its own trail with the same machinery (`HTS_AUDIT_*`, same
variables as `HFS_AUDIT_*` with the `HTS_` prefix, observer `Device/hts`):

- **Terminology writes** — `POST/PUT/DELETE` on `CodeSystem`, `ValueSet`,
  `ConceptMap` are BALP `Create` / `Update` / `Delete` events on the resource
  (`Location` names a created one); write batches are recorded per request;
  `POST /import` (IG terminology loads pushed by HFS's package manager, and
  any bundle import) and `/admin` writes are recorded unprofiled with the
  request line. Refusals (401 without the administrator credential) are
  recorded with outcome `4`.
- **Admin actions** — every action of the terminology admin API and its
  background jobs (uploads, imports of SNOMED/LOINC/ICD-10 releases, package
  installs, deletes, set-current) also records an application-activity event
  (DICOM `110100`, subtype `urn:helios:hts:admin-action|<action>`, the
  affected entity as a detail, the job outcome). The console's
  `audit.ndjson` view is unchanged.
- **Who** — the administrator authenticated by `HTS_ADMIN_PASSWORD` (user
  agent `admin`); requests without credentials carry only the client agent.
- **Reads** (`GET`, `$expand`, `$lookup`, `$validate-code`, `$translate`,
  `$subsumes`, `$batch-validate-code`) are **not** recorded by default — they
  are the bulk of terminology traffic and carry no patient data. Set
  `HTS_AUDIT_READS=true` to record them (BALP `Read` / `Query`).
  (`HFS_AUDIT_READS` exists too and defaults to `true`; keep it on for HFS.)

**Where HTS events go** (`HTS_AUDIT_BACKEND=database`):

1. Default: HTS's own database, in the FHIR resource store HTS already keeps
   next to its terminology tables (tenant `default`). Search it at
   `GET <hts>/AuditEvent?date=&entity=&agent=&type=&subtype=&action=&outcome=&_count=`
   and `GET <hts>/AuditEvent/{id}` — **administrator only** (same credential
   as `/admin`; without `HTS_ADMIN_PASSWORD` the trail answers `503` rather
   than being open).
2. `HTS_AUDIT_DATABASE_URL=<HFS's database URL>`: HTS writes into HFS's audit
   store, so `GET <hfs>/AuditEvent` covers both servers and HFS's retention
   job purges both. Use this when HFS and HTS share a PostgreSQL server (they
   do in the reference deployment). HTS then needs credentials for HFS's
   database and runs the same schema initialisation — deploy both from the
   same build.

`HTS_AUDIT_RETENTION_*` works as on HFS when HTS keeps its own store (leave it
unset with option 2; HFS's job covers it). The fallback file defaults to
`<HTS_ADMIN_DATA_DIR>/audit-fallback.ndjson`.

## Metrics

Both servers export the audit write path on `/metrics` (Prometheus, labelled
`service="hfs"` / `service="hts"`, never by tenant):

| Metric | Type | Meaning |
|---|---|---|
| `audit_events_enqueued_total` | counter | events accepted onto the queue |
| `audit_events_written_total` | counter | events handed to the store — `rate(...[5m])` is events/s |
| `audit_queue_depth` | gauge | accepted but not yet written (queued + in flight) |
| `audit_queue_full_waits_total` | counter | requests that waited for room (backpressure) |
| `audit_queue_blocked_seconds` | histogram | how long a request waited for room in the full queue; `rate(audit_queue_blocked_seconds_sum[5m])` is request-seconds lost to audit backpressure per second |
| `audit_batch_write_duration_seconds` | histogram | wall time of one batch write |
| `audit_events_spilled_total{reason}` | counter | written to the fallback file instead (`shutdown`, `write_failure`) |
| `audit_events_replayed_total` | counter | fallback events stored at start-up |
| `audit_events_dropped_total{reason}` | counter | events lost (`serialization`, `no_fallback`, `fallback_failed`) |
| `audit_retention_purged_total` | counter | events removed by retention |
| `audit_retention_runs_total{outcome}` | counter | retention runs (`ok`, `error`) |

Suggested alerts: `audit_events_dropped_total` increases (any loss);
`audit_events_spilled_total{reason="write_failure"}` increases (the database
refuses audit writes); `audit_queue_depth > 5000` for 5 minutes (the writer
does not keep up); `rate(audit_queue_full_waits_total[5m]) > 0` for 5 minutes
(requests are being slowed down by the audit writers: raise
`HFS_AUDIT_WRITERS` / `HFS_AUDIT_BATCH_SIZE` or look at the database);
`histogram_quantile(0.95, rate(audit_batch_write_duration_seconds_bucket[5m])) > 1`.

## Configuration reference

| Variable | Default | Description |
|---|---|---|
| `HFS_AUDIT_BACKEND` | `none` | `none`, `file`, `database`, `cloudwatch` |
| `HFS_AUDIT_SOURCE_OBSERVER` | `Device/hfs` | server agent / `source.observer` |
| `HFS_AUDIT_TENANT` | *(request tenant)* | store every event in this tenant |
| `HFS_AUDIT_STORE` | `table` | `table`: the dedicated audit tables (PostgreSQL); `resource`: `AuditEvent` resources in the general tables (the previous layout; always used on other backends) |
| `HFS_AUDIT_BACKFILL` | `false` | move legacy `AuditEvent` resources into the audit tables in the background (see above) |
| `HFS_COMPARTMENT_INCLUDE_AUDIT_EVENTS` | `false` | let `$everything` and `Patient/{id}/*` return `AuditEvent` without `_type` naming it |
| `HFS_AUDIT_DATABASE_URL` | *(primary DB)* | dedicated audit database (PostgreSQL with the audit tables: searchable through the API; otherwise not) |
| `HFS_AUDIT_FILE_PATH` | — | NDJSON file for `file` |
| `HFS_AUDIT_EXCLUDE_PATHS` | — | extra exclusions, comma separated: `/path`, `/prefix/*`, `GET /path` |
| `HFS_AUDIT_TRUST_FORWARDED_FOR` | `false` | client address from the first `X-Forwarded-For` hop (only behind a proxy that sets it) |
| `HFS_AUDIT_QUEUE_CAPACITY` | `10000` | events queued before requests wait |
| `HFS_AUDIT_BATCH_SIZE` | `256` | events per writer batch (one database round trip and one commit) |
| `HFS_AUDIT_WRITERS` | `8` | concurrent writer tasks (database connections in use) |
| `HFS_AUDIT_SHUTDOWN_TIMEOUT` | `5` | seconds to drain the queue on shutdown before spilling the rest to the fallback file |
| `HFS_AUDIT_FALLBACK_FILE` | `<data dir>/audit-fallback.ndjson` | spill file while the database is down |
| `HFS_AUDIT_READS` | `true` (HTS: `false`) | record read-only requests (`GET`, `_search`, read-only operations) |
| `HFS_AUDIT_AUTH_EVENTS` | `false` | also record the auth layer's per-request authentication/authorization events (the REST events already carry the user and the 401/403) |
| `HFS_AUDIT_RETENTION_DAYS` | *(keep forever)* | purge events older than N days |
| `HFS_AUDIT_RETENTION_ARCHIVE_DIR` | — | archive purged events (gzip NDJSON) first |
| `HFS_AUDIT_RETENTION_INTERVAL_HOURS` | `24` | hours between purge runs |
| `HFS_AUDIT_RETENTION_BATCH` | `1000` | rows deleted per transaction |

Every variable exists for HTS with the `HTS_AUDIT_` prefix.

## Recommended production configuration

```
HFS_AUDIT_BACKEND=database
HFS_AUDIT_SOURCE_OBSERVER=Device/hfs-prod
HFS_AUDIT_TRUST_FORWARDED_FOR=true          # HFS sits behind the reverse proxy
HFS_AUDIT_RETENTION_DAYS=2190               # 6 years; set to your legal requirement
HFS_AUDIT_RETENTION_ARCHIVE_DIR=/var/lib/hfs/audit-archive   # on a backed-up volume
HFS_AUDIT_FALLBACK_FILE=/var/lib/hfs/audit-fallback.ndjson   # persistent volume
HFS_AUDIT_WRITERS=8
HFS_AUDIT_BATCH_SIZE=256
```

- Mount the archive and fallback paths on a persistent, backed-up volume.
- Size: see the measured figures in `hfs-eval/eval/bench/` (2026-11): about
  1.5 KB per event in the audit tables including the lookup rows and indexes,
  against roughly 17 KB when an event was a FHIR resource (history, ~17
  search-index rows, full text). 100 000 interactions/day ≈ 0.15 GB/day,
  ~55 GB/year before retention. Watch `audit_event*` with
  `pg_total_relation_size`.
- Autovacuum is enough: the tables are insert-only, and retention drops
  partitions instead of deleting rows.
- Give auditors a role/scope with `AuditEvent` read; nobody needs write.
- Keep `/metrics`, `/health` probes excluded (default).
- HTS: `HTS_AUDIT_BACKEND=database`, `HTS_AUDIT_DATABASE_URL=<HFS DB URL>`,
  `HTS_AUDIT_TENANT=default`, `HTS_AUDIT_TRUST_FORWARDED_FOR=true`, reads off.

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
