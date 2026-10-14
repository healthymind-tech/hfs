# HTS terminology management (admin API and console)

HTS can upload, import, inspect and delete terminology at runtime — the same
importers as `hts import`, run as background jobs — from the console at
`/ui/hts/terminology`, `/ui/hts/import` and `/ui/hts/jobs`, or from scripts via
the JSON API under `/admin`.

> **Access control.** The whole surface is off unless `HTS_ADMIN_UI=true`.
> Until SMART/OIDC admin scopes land, that flag *is* the access control:
> enable it only where `/admin` and `/ui/hts` are reachable by administrators
> alone (internal network, VPN, or an authenticating reverse proxy that sets
> `X-Forwarded-User`). Every action is audited (see below).

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `HTS_ADMIN_UI` | `false` | Mount `/admin`, `/ui/hts/api/admin` and enable the console pages. |
| `HTS_ADMIN_DATA_DIR` | `<database dir>/hts-admin` | Uploads (`uploads/`), job records and logs (`jobs/`), downloaded packages (`packages/`), `audit.ndjson`, `provenance.json`. Put it on the same volume as the database or one at least as large as the biggest source file. |
| `HTS_IMPORT_DIR` | *(off)* | Directory operators copy files into (e.g. a read-only mount of a share). Listed by the console; imported without an upload. |
| `HTS_PACKAGE_REGISTRY` | `https://packages.fhir.org` | FHIR package registry (or an internal mirror). `off` disables it for hospitals without outbound internet; uploads and the import directory keep working. |
| `HTS_UPLOAD_CHUNK_SIZE` | `8388608` (8 MiB) | Bytes per upload request. Keep each request well inside proxy limits and the 30 s request timeout. |
| `HTS_MAX_UPLOAD_SIZE` | `17179869184` (16 GiB) | Largest accepted upload. |
| `HTS_DISK_HEADROOM` | `1073741824` (1 GiB) | Free space required on top of each growth estimate. |
| `HTS_BOOTSTRAP_BATCH_SIZE` | `5000` | Default importer batch size for jobs. |
| `HTS_IMPORT_LANGUAGES` | *(all)* | Default translation filter for SNOMED/LOINC jobs. |

The reference nginx (`client_max_body_size 0`, 600 s timeouts) needs no
change: `/tx/admin/...` reaches `/admin`, and the console's browser code uses
`/ui/hts/api/admin/...`, which the existing `/ui/hts` location already
forwards to HTS.

## Model

* **Uploads** are resumable: the browser splits the file into `chunk_size`
  pieces, each a separate `PUT`. Chunks land in place in a pre-sized file, so
  order and retries do not matter. Session state survives server restarts; a
  browser that reloads picks the same session up again by `client_key`
  (derived from file name, size and modification time) and sends only the
  missing chunks. Completion computes the SHA-256 and runs format detection.
  Incomplete sessions idle for 7 days are purged at startup.
* **Jobs** perform every write (imports, deletes, compaction). One worker runs
  them strictly one at a time, FIFO: SQLite has a single write lock and two
  concurrent bulk imports fail with `database is locked`. A job submitted while
  another runs is `queued` with a `queue_position`. States: `queued`,
  `running`, `succeeded`, `failed`, `cancelled`, `interrupted` (the server
  stopped mid-job; queued jobs are re-queued on restart). Each job has a log
  (`<data>/jobs/<id>.log`).
* **Import phases**: `prepare` (resolve source, detect format, disk check) →
  `download` (registry only) → `parse` → `write` (batch *x*/*y*) → `closure`
  (transitive is-a closure) → `fts` (full-text indexes) → `analyze` (planner
  statistics) → `warm` (in-memory indexes). Cancelling stops at the next batch
  and deletes every CodeSystem version the run created.
* **Disk pre-check**: growth is estimated per format (measured: SNOMED CT INT
  3–4 GB, LOINC 2.80 2.2 GB) as `max(floor, factor × file size)`; the import is
  refused with `507` when the database volume lacks estimate + headroom.
  Compaction requires twice the database size.
* **Delete** works per CodeSystem *version*: closure, hierarchy, concepts
  (with designations and properties), FTS rows, persisted expansion caches and
  the resource-store copy (if the version was written through the REST API),
  in bounded batches so the write lock is released between them. Dependents
  (ValueSets including the system, supplements, ConceptMaps) are listed first;
  version-less references fall back to the remaining current version.
* **Current version**: by default the semantically newest populated version
  answers version-less requests. `PUT /admin/code-systems/current` pins
  another; the pin is a precedence tier in the same SQL ordering every
  resolver uses, so `$lookup`, `$validate-code` and `$expand` agree.
* **Compaction**: SQLite never shrinks the file after a delete; `VACUUM`
  rewrites it. It blocks other writers for its duration (minutes on a
  multi-GB database) — run it in a maintenance window. Postgres runs
  `VACUUM (ANALYZE)`.

## Formats

`GET /admin/formats` lists every importer the binary has (the CLI's
`--format` values): `hl7-npm`, `snomed-rf2`, `loinc`, `icd10-cm`, `icd9-cm`,
`rxnorm`, `ucum`, `nci-thesaurus`, `mesh`, `dicom`, `hl7-v2-tables`, `nucc`,
`ndc`, `fhir-bundle` (and `icd10-pcs` once that importer is merged). Detection
inspects archive contents (`sct2_Concept_*` → SNOMED and its release date,
`Loinc_<ver>/…Loinc.csv` → LOINC and its version, `package/package.json` →
FHIR package name/version, …); the operator can always override the format.

Import parameters: `version` (override the CodeSystem version; single-system
formats only), `languages` (BCP-47 list, SNOMED/LOINC), `properties` (LOINC:
concept properties to keep, comma-separated; hierarchy and status properties
are always kept), `batch_size`.

**Licences:** SNOMED CT and LOINC (and RxNorm) files must be obtained and
supplied by the operator under their own licence terms; HTS never downloads
them. FHIR packages carry their own licence.

## API

All endpoints are relative to `/admin` (also served at `/ui/hts/api/admin`).
Request and response bodies are JSON; errors are
`{"error": "<code>", "message": "…"}` with the HTTP status.

| Method & path | Purpose |
|---|---|
| `GET /` | Status: backend, directories, registry, limits, busy flag, database size, free space. |
| `GET /formats` | Format registry (licence, growth estimate inputs, supported parameters). |
| `GET /terminology?q=&sort=name\|size&limit=&populated=true` | Inventory: per CodeSystem url, every version with concept/designation/property/closure counts, estimated size, current/pinned flags and provenance (import time, source, SHA-256, measured DB growth). Counts are cached; when not ready within 8 s the response has `stats_pending: true`. |
| `POST /terminology/refresh-stats` | Recompute the cached counts. |
| `GET /code-systems/dependents?url=&version=` | Dependents of one version and the version that becomes current after deleting it. |
| `PUT /code-systems/current` `{url, version}` | Pin `version` as current; `{url, version: null}` clears the pin (`unversioned: true` pins the un-versioned row). |
| `DELETE /code-systems?url=&version=` (or `POST /code-systems/delete` with the same JSON) | Queue a delete job → `202` + job. |
| `DELETE /value-sets?url=&version=` | Queue a ValueSet delete. |
| `DELETE /concept-maps?url=` | Queue a ConceptMap delete. |
| `POST /maintenance/compact` | Queue `VACUUM`. |
| `GET /uploads` | Upload sessions (complete and incomplete). |
| `POST /uploads` `{filename, size, client_key?}` | Create (`201`) or resume (`200`) a session → `{id, chunk_size, chunks, received[], missing[]}`. |
| `GET /uploads/{id}` | Session status. |
| `PUT /uploads/{id}/chunks/{index}` | Raw chunk bytes; optional `X-Chunk-Sha256` (hex) is verified. |
| `POST /uploads/{id}/complete` | Verify, hash, detect → session with `sha256` and `detection`. |
| `GET /uploads/{id}/detect?format=` | Detection and disk check (optionally for an overridden format). |
| `DELETE /uploads/{id}` | Remove the staged file. |
| `GET /import-dir` | Files under `HTS_IMPORT_DIR` (four levels deep, plus RxNorm RRF directories). |
| `GET /import-dir/detect?path=&format=` | Detection and disk check for one file. |
| `POST /imports` `{source, format?, version?, languages?, properties?, batch_size?}` | Queue an import → `202` + job. `source` is `{"type":"upload","upload_id":…}`, `{"type":"import-dir","path":…}` or `{"type":"registry","package":…,"version"?:…}`. |
| `GET /registry/search?q=&fhirVersion=R4` | Search the package registry catalog. |
| `GET /registry/packages/{name}?version=` | Resolve a package version (tarball URL, shasum). |
| `POST /registry/install` `{package, version?}` | Queue a registry import (downloaded to `packages/`, SHA-1 verified, imported as `hl7-npm`). |
| `GET /jobs?limit=` | Jobs, newest first, with live progress (`phase`, `done`/`total`, `unit`, `concepts`, `percent`). |
| `GET /jobs/{id}` | One job. |
| `GET /jobs/{id}/log?tail=` | Last lines of the job log. |
| `POST /jobs/{id}/cancel` | Cancel a queued job, or ask a running import to stop at the next batch (running deletes and compaction cannot be cancelled: `409`). |
| `GET /audit?limit=` | Recent audit events. |

Example — import SNOMED from the server import directory and follow it:

```sh
curl -s -XPOST localhost:8090/admin/imports -H 'content-type: application/json' \
  -d '{"source":{"type":"import-dir","path":"SnomedCT_InternationalRF2_PRODUCTION_20250601T120000Z.zip"}}'
curl -s localhost:8090/admin/jobs/<id> | jq .progress
```

## Audit

Every action — upload, import, delete, pin, compaction, cancellation — is
appended to `<data>/audit.ndjson` as a FHIR R4 `AuditEvent` (type DCM
110100 *Application Activity*, `subtype` = action code, `agent.who` = the
`X-Forwarded-User`/`X-Remote-User` header or the client address, `entity` =
the terminology version or upload, `outcome` 0/4/8) and logged on the
`hts::audit` tracing target.
