# FHIR endpoints (FHIR server manager)

One HFS deployment can serve several FHIR endpoints, each at its own base
path, with its own tenant, FHIR version, validation, terminology server and
CORS (per-endpoint authentication and rate limits arrive with package P2).
Design: [`docs/design/fhir-server-manager.md`](../design/fhir-server-manager.md).

## Switching it on

| Variable | Default | Meaning |
|---|---|---|
| `HFS_MANAGER` | `off` | `on` installs the endpoint dispatcher in front of the FHIR API. `off` = exactly the pre-manager behaviour. |
| `HFS_ENDPOINTS_FILE` | unset | JSON array of `EndpointSpec` (see below), applied at startup as environment-managed (read-only) endpoints. The only way to declare endpoints on SQLite / MongoDB / S3. |
| `HFS_MANAGER_ALLOW_URL_TENANTS` | `false` | With `HFS_MANAGER=on`, `HFS_TENANT_ROUTING_MODE=url_path`/`both` is refused at startup unless this is `true`. |
| `HFS_CLUSTER_SYNC_INTERVAL_MS` | `1000` | How often each instance polls the endpoint configuration epoch (`0` → 1 s). |
| `HFS_SECRET_KEYS` / `_FILE` | unset | Key ring for sealed endpoint secrets; malformed → startup fails, unset → secrets cannot be stored. |

On PostgreSQL the default endpoint row (root path, tenant
`HFS_DEFAULT_TENANT`, every policy section inherited from the environment) is
written at every startup even with `HFS_MANAGER=off` (rollout step R0); it is
not read until the flag is on.

## URLs

An endpoint with path `twcore` is served at `{HFS_BASE_URL}/twcore`, e.g.
`https://hfs.example/fhir/twcore/Patient/1` (nginx unchanged: `/fhir/` →
`hfs:/`). The default endpoint keeps the root. Every URL HFS generates —
`Location`, `Content-Location`, bundle `self`/`next`/`prev`, `fullUrl`,
`_history`, CapabilityStatement `implementation.url`, bulk export status,
manifest and file URLs, subscription notification `fullUrl`s — carries the
endpoint prefix. Batch/transaction entry `response.location` stays relative,
as before, so it resolves against the endpoint base.

Requests to a `disabled` endpoint answer `503` (`OperationOutcome`,
`transient`); `provisioning` adds `Retry-After: 30`. Without a default
endpoint, unprefixed paths answer `404`.

## One door per tenant

* A named endpoint serves exactly its tenant. `X-Tenant-ID` naming another
  tenant → `400`; a token whose tenant claim names another tenant → `403`.
* On the default endpoint, a tenant bound to a named endpoint cannot be
  selected (`403`, the message names the endpoint URL) — by header or token
  claim. Unbound tenants stay selectable by `X-Tenant-ID` while the default
  endpoint's `legacyTenantHeader` is `true` (the upgrade default).

## Per-endpoint behaviour (P1)

| Section | `null` (inherit) | Set |
|---|---|---|
| `fhirVersion` | — | Default version for requests without a `fhirVersion` media-type parameter; CapabilityStatement `fhirVersion`; resource-type gate; `$versions` default. |
| `validation` | `HFS_VALIDATION_*` | Write mode (off/log/enforce), terminology mode, stored profiles, `requiredProfiles` (validated on every write of that type in addition to `meta.profile`). `$validate` uses the endpoint's terminology. |
| `terminology` | `HFS_TERMINOLOGY_SERVER` | Server for `:in`/`:not-in`/`:above`/`:below` search expansion and remote validation; a binding with no `serverUrl` means none. |
| `cors` | `HFS_CORS_*` | Origins, credentials and extra headers for this endpoint (preflights included); `enabled=false` sends no CORS headers. `/ui`, `/admin`, `/console`, `/metrics` always use the global settings. |

## `HFS_ENDPOINTS_FILE`

```json
[
  {"name": "twcore", "path": "twcore", "displayName": "TW Core sandbox",
   "fhirVersion": "R4", "storage": {"kind": "tenant", "tenantId": "twcore"},
   "validation": {"mode": "enforce", "terminology": "remote"}}
]
```

Each entry is created or updated (as `system:bootstrap`) with
`managedBy=env`; an environment-managed endpoint that disappears from the
file is **disabled** (data and config kept). Invalid or duplicate entries fail
startup.

## Hot reload and cluster

Writes to `hfs_endpoints` bump `hfs_config_epochs('endpoints')` and
`NOTIFY hfs_config`. Each instance polls the epoch every
`HFS_CLUSTER_SYNC_INTERVAL_MS` and keeps a `LISTEN hfs_config` session
(dedicated connection) that wakes the poll immediately; requests in flight
finish on the snapshot they started with. A deleted or disabled endpoint can
keep serving on another instance for at most one poll interval if its LISTEN
session is down.

## Metrics

`hfs_endpoint_requests_total{endpoint,status_class}` and
`hfs_endpoint_request_duration_seconds{endpoint}` (bounded: at most 200
endpoints). `http_requests_total` keeps its label set.

## Audit

Every AuditEvent of a request served by the dispatcher carries the
extension `https://healthymind.tech/fhir/StructureDefinition/hfs-endpoint`
(`valueString` = endpoint name), and the event is classified on the path
without the endpoint prefix. Events are written to the endpoint's tenant.

## Rollback

`HFS_MANAGER=off` and restart. The tables stay (fork track only).
