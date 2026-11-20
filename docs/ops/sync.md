# Endpoint sync and duplication (FHIR server manager P6)

One-way sync jobs between endpoints of this server and external FHIR
servers, and endpoint duplication. Design: `docs/design/fhir-server-manager.md`
§9. Code: `crates/sync` (`helios-sync`), PostgreSQL store
`crates/endpoints/src/pg_sync.rs`, tenant copy
`crates/persistence/src/backends/postgres/tenant_copy.rs`, admin API
`crates/rest/src/{routing,handlers}/admin_sync.rs`, console
`crates/ui/src/sync_pages.rs`, wiring `crates/hfs/src/manager/sync.rs`.

## Switches

| Variable | Default | Meaning |
|---|---|---|
| `HFS_MANAGER` | `off` | Sync (API, console pages, workers) is inert unless `on`. |
| `HFS_SYNC_ENABLED` | `true` | Workers and scheduler run on this instance. Set `false` on instances that should only serve the API. |
| `HFS_SYNC_WORKERS` | `2` | Concurrent runs per instance (1–32). |
| `HFS_SYNC_LEASE_SECS` | `60` | Run lease; heartbeat every lease/6. |
| `HFS_SYNC_EXTERNAL` | `true` | `false` refuses external peers (API 422, runs fail). |
| `HFS_SYNC_SAFETY_LAG_MS` | `2000` | The incremental high-water mark never passes `now - lag` (late commits are not lost). |
| `HFS_SYNC_PAGE_SIZE` | `200` | Resources per source page. |
| `HFS_SYNC_HTTP_TIMEOUT_SECS` | `60` | Outbound request timeout. |
| `HFS_SECRET_KEYS` / `_FILE` | — | Key ring sealing peer credentials (AES-256-GCM). Without it, jobs with credentials are refused (409). |
| `HFS_OUTBOUND_ALLOW` | — | Env-only allow list (hosts, `*.suffix`, CIDRs) for external URLs; everything else must be public `https`. |

## Behaviour

- **Runs** are `cluster_jobs` rows of kind `endpoint-sync`: any instance claims
  them (`FOR UPDATE SKIP LOCKED`), heartbeats, and the run row records the
  lease's fencing token — a worker that lost its lease cannot checkpoint. A
  crashed instance's run is reclaimed after the lease expires and resumes from
  the last checkpoint (cursor + counters after every batch).
- **Initial load**: `storage-copy` (PostgreSQL, local → local, one set-based
  copy of resources, history (optional), search index, FTS rows and package
  registry), `paged-read` (`GET [type]?_count&_sort=_lastUpdated`, following
  `next` links on the same origin), `bulk-export` (external `$export`; a local
  source falls back to paged read), or `none`.
- **Incremental**: `_history?_since=<high-water>` (inclusive, newest-first
  pages read completely then applied oldest first) for external sources; for
  local sources the ascending differential read plus deletes from history.
  `manual`, `interval` (≥ 60 s), `continuous` (poll ≥ 1 s local / ≥ 10 s
  external) or `subscription` (rest-hook notifications wake a pass; fallback
  poll).
- **Idempotency**: every write is `PUT [type]/[id]`; `hfs_sync_item_state`
  keeps the source version and the SHA-256 of the canonical content (without
  `meta.versionId`/`lastUpdated`); unchanged items are skipped, so re-running
  a batch after a crash causes no version churn.
- **Conflicts**: `source-wins`; `skip-if-target-newer`; `fail` (target changed
  since this job wrote it → conflict recorded, item skipped, run continues).
  Resolve per item in the console or with
  `POST /admin/sync/runs/{run}/conflicts/{type}/{id}/$resolve {"action":"overwrite"|"keep"}`.
- **External writes**: `batch` Bundles of `PUT`/`DELETE`; when the server
  refuses Bundles the job switches to individual requests. `If-Match` is sent
  under the `fail` policy. `429`/`503` honour `Retry-After`; a token bucket
  caps requests per second (`maxRequestsPerSecond`, default 10).
- **Safety**: external URLs pass the outbound policy before every connection
  (after DNS resolution, connection pinned to the checked address, no
  redirects, 64 MB response cap); server-supplied paging links must stay on
  the server's origin; credentials are sealed with AAD
  `sync-job:{id}:{side}.auth.{field}` and never returned (`{"set": true}`).
  Logs carry `type/id/version` and errors only (10 000 lines per run).
- **Audit**: job create/update/delete/run/cancel/retry/resolve and every run's
  end are AuditEvents (agent `Device/hfs-sync`); local target writes go
  through the server's write observers (subscriptions, counters).
- **Validation of local target writes**: every write into an endpoint of this
  server goes through that endpoint's normal write path
  (`ValidationService::prepare_write` scoped to the target endpoint: its mode,
  required profiles and terminology, or the server's `HFS_VALIDATION_*` when
  inherited). `enforce`: the resource is not written; the run counts it as
  failed, logs it and lists it with the conflicts (reason `rejected by target
  validation: …` carrying the OperationOutcome). `log`: written; the findings
  go to the run log (`validation (log): Type/id: …`, at most 50 lines per
  batch). `off`: written. A written StructureDefinition joins the target's
  validation profile registry at once.
- **In-database copy exemption**: `storage-copy` (and duplication on
  PostgreSQL) skips write validation **only when source and target endpoints
  have identical `validation` and `terminology` sections** (inherited counts as
  equal) — the data then already passed the same rules at the source. A
  duplicate copies the source's sections, so it qualifies. Otherwise the job
  logs `source and target endpoints validate differently` and loads through
  the target's validation path (paged read) instead. After an in-database copy
  the copied StructureDefinitions are registered with this instance's profile
  registry immediately; other instances pick them up through the
  `resource_change_feed` trigger (cluster profile sync), and copied
  SearchParameters through the `search_parameter_epochs` trigger.
- **Wake-ups**: a committed local write wakes the continuous jobs reading that
  endpoint at once (write observer), so local→local latency is the pass time,
  not the poll period. Subscription notifications and job edits are published
  with `NOTIFY hfs_sync_wake` (PostgreSQL) and wake the run on whichever
  instance holds it.

## API

```
GET|POST /admin/sync/jobs                 POST /admin/sync/jobs/$test
GET|PUT|DELETE /admin/sync/jobs/{id}      (ETag / If-Match)
POST /admin/sync/jobs/{id}/$run | $cancel | $retry
GET  /admin/sync/jobs/{id}/runs           GET /admin/sync/runs/{run}[/log?after=&limit=|/conflicts]
POST /admin/sync/runs/{run}/conflicts/{type}/{id}/$resolve
POST /admin/sync/duplicates               {"source": "<endpoint>", ...DuplicateRequest}
POST /admin/sync/$rewrap
POST /sync/notify/{job}                   X-Sync-Notify-Secret: <secret>  (outside /admin)
```

Example (pull from an external server every 10 minutes):

```json
POST /admin/sync/jobs
{ "name": "hapi to lab", "source": {"kind": "external", "baseUrl": "https://hapi.example.org/fhir",
  "auth": {"type": "bearer"}}, "target": {"kind": "endpoint", "name": "lab"},
  "resourceTypes": ["Patient", "Observation"], "trigger": {"type": "interval", "everySecs": 600},
  "secrets": {"source.auth.token": {"set": "…"}} }
```

The subscription trigger's notify secret is returned once in the create
response (`notifySecret`, `notifyPath`); configure the source's rest-hook
Subscription to `POST {HFS_BASE_URL}{notifyPath}` with header
`X-Sync-Notify-Secret`.

Console: `/ui/sync` (list), `/ui/sync/new`, `/ui/sync/{job}`,
`/ui/sync/runs/{run}` (live counters, polled every 2 s while active).

## Duplicate

`POST /admin/sync/duplicates {"source": "lab", "name": "lab-copy", "data": "none|current|history", "packages": true, "auth": true, "secrets": false, "keepInSync": false}`
creates the endpoint (`provisioning`), copies data (storage copy on
PostgreSQL, paged read elsewhere; `none` + `packages` copies conformance
resources and the package registry), enables the endpoint when the copy
succeeds, and with `keepInSync` switches on a continuous job source → copy.
A failed copy leaves the endpoint `provisioning`; retry the job's run.

## Limits (v1)

- Sync is one-way; cycles between enabled jobs are refused.
- Source and target must speak the same FHIR release.
- Per-type `filters` only for external sources (applied to the initial read and
  to the incremental search of the filtered types; deletes of filtered types are
  not propagated).
- A local write made on another instance wakes a continuous run only at its
  next poll (the instant wake-up is per instance; notifications and job
  edits are cluster-wide).
