# FHIR Server Manager — multi-endpoint HFS, console sign-in, sync

Status: **design, accepted for implementation** (line M0, 2026-11-20).
Branch `hm/design-manager` (from `hm/main` e5258f91e = production `p3-e5258f91e`).
Package 0 (the shared contracts) is implemented on this branch as the crate
`crates/endpoints` (`helios-endpoints`) plus fork migrations `hm-0008`…`hm-0010`.

This document turns the HFS console from "one FHIR server with an admin page"
into a **FHIR servers manager**: many FHIR endpoints on one HFS deployment,
each with its own base path, FHIR version, auth, validation, terminology,
CORS and rate limits, created and edited at runtime; a console that requires
sign-in (none / OIDC / built-in accounts) with roles; and jobs that duplicate
endpoints and sync data between endpoints and external FHIR servers.

Decisions are marked **D-n**; the summary table is in §0.

---

## 0. Key decisions

| # | Decision | Why |
|---|---|---|
| D-1 | An endpoint is served at **`{HFS_BASE_URL}/{path}`** — publicly `https://host/fhir/{path}/…`, inside HFS `/{path}/…` (nginx already strips `/fhir/`). The existing root endpoint becomes the **default endpoint** (empty path) and is unchanged. | Same shape as HFS's existing URL-tenant routing and its public-URL builder; no nginx change; `hfs.gugulu.tw/fhir/Patient` keeps working byte for byte. `/{name}/fhir` would need nginx rules per endpoint and breaks the "FHIR base = one prefix" property. |
| D-2 | Path segments are `^[a-z][a-z0-9-]{0,62}$`, not ending in `-`, minus a reserved list (`ui`, `admin`, `metrics`, `fhir`, `tx`, `auth`, …). | Lower case can never collide with a resource type, `_param`, `$op` or `.well-known`; every such segment is also a valid tenant id. |
| D-3 | **Storage isolation = the existing tenant mechanism, one tenant per endpoint (v1).** The default endpoint keeps tenant `default`. Dedicated databases are reserved in the model (`StorageBinding` enum) but not built. | Every backend already enforces `tenant_id` at the query level; packages, the validation profile registry, SearchParameter epochs, bulk jobs, subscriptions, audit and purge are already per tenant. Honest limits in §4.4. |
| D-4 | Endpoint config lives in PostgreSQL (`hfs_endpoints`, JSONB spec + denormalized unique columns), with every policy section `null` = **inherit the environment**. Upgrade inserts the default endpoint with everything inherited → zero behaviour change. | Lets prod upgrade with no config change, and lets an admin override section by section. |
| D-5 | Routing is a **dispatcher in front of the FHIR router** (tower `map_request` + `fallback_service`, the pattern `strip_tenant_prefix` already uses) reading an `ArcSwap` snapshot; it strips the prefix and inserts `ResolvedEndpoint` as a request extension. Per-request layers read the extension instead of global config. No router rebuild on change. | Hot reload without restart; one routing table; old requests finish on the snapshot they started with. |
| D-6 | Cluster coherence: statement triggers bump `hfs_config_epochs` and `NOTIFY hfs_config`; every instance polls the epoch (`HFS_CLUSTER_SYNC_INTERVAL_MS`, 1 s) and wakes early on NOTIFY. The writing instance reloads before replying. | Same model as `search_parameter_epochs` / `resource_change_feed` (line F), proven in prod. |
| D-7 | A tenant bound to a named endpoint has **exactly one door**: `X-Tenant-ID` on the default endpoint is refused (403) for it; named endpoints ignore-and-refuse a mismatching `X-Tenant-ID`. Legacy header tenancy remains only on the default endpoint for unbound tenants (`legacyTenantHeader`, default on for compatibility). | Prevents bypassing an endpoint's stricter auth through the root door. |
| D-8 | Per-endpoint FHIR auth reuses `helios-auth`'s validator unchanged; a **verifier cache keyed by `(issuer, jwks_url, algorithms)`** is shared by endpoints on the same IdP; audience, mode, scope policy and SMART discovery are per endpoint. | One JWKS fetch per IdP; distinct audiences stop cross-endpoint token reuse. |
| D-9 | Console modes `none` / `oidc` / `local`. In `oidc` and `local` **every** `/ui` page requires a session. Roles `viewer < editor < admin`, global with optional per-endpoint bindings. The `HFS_ADMIN_PASSWORD` administrator stays as **break-glass** in all modes. Settings in DB, editable by admins; `HFS_CONSOLE_AUTH_*` env vars bootstrap and, when set, pin fields. | User requirement; pinning guarantees recovery from a bad IdP config. |
| D-10 | A console session is a credential on the FHIR API as a **console principal** (`iss=urn:helios:console`, scopes from role × endpoint `consoleAccess`), not by forwarding the IdP access token. | Works in `local` mode and when the endpoint's issuer differs from the console's IdP; least privilege per endpoint. |
| D-11 | CSRF: session cookie `SameSite=Strict; HttpOnly; Secure`, plus same-origin `Origin`/`Referer` check on every state-changing cookie-authenticated request (existing admin-auth rule), plus a per-session synchronizer token sent by htmx as `X-CSRF-Token`. | Defence in depth; the first two already ship. |
| D-12 | Secrets (OIDC client secret, terminology tokens, sync credentials) are stored **AES-256-GCM sealed** under an env-held key ring `HFS_SECRET_KEYS`, AAD-bound to row+field; write-only in the API. No key ring → storing a secret is refused. | Never plaintext at rest; rotation without downtime. |
| D-13 | URLs typed into the console (external sync servers, issuers, JWKS, token and terminology URLs) pass an **SSRF policy**: https only, internal/metadata ranges refused after DNS resolution with the connection pinned to the checked IP; the allow list is env-only (`HFS_OUTBOUND_ALLOW`). | GCE/cloud metadata and the internal Docker network are reachable from HFS. |
| D-14 | Sync is **one-way** jobs (source → target; endpoint or external), initial load (storage copy / paged read / bulk export) + incremental (`_history?_since` polling, interval or continuous, or Subscription-triggered), ids preserved, idempotent `PUT`s, per-item state for skip/conflict detection, runs claimed through `cluster_jobs` with lease + fencing, checkpointed cursor → resumable. Bidirectional sync is a non-goal. | Correct and resumable with the machinery the fork already has. |
| D-15 | Duplicate = new endpoint (status `provisioning`) + a `StorageCopy` sync run (set-based SQL copy inside PostgreSQL, current or current+history) + package registry copy; optional continuous sync afterwards. | Seconds-to-minutes for prod-sized data instead of hours through the API. |
| D-16 | Console URL scheme: `/ui` overview; `/ui/e/{endpoint}/…` per endpoint (endpoint in the URL, not a browser preference); old `/ui/...` pages redirect to `/ui/e/default/...`. | Deep links, multiple tabs on different endpoints. |
| D-17 | Product name in the console: **療心智能 FHIR Server Manager** (en: **HealthyMind FHIR Server Manager**); HealthyMind Tech branding with tokens derived from the logo gradient; "Based on Helios FHIR Server (MIT)" on the About page and footer. | Coordinator requirement (§10.2). |
| D-18 | Feature flag `HFS_MANAGER=off|on` (default **off** in the first release): schema is created, the default endpoint row is written, but the dispatcher, console auth and sync stay inert until switched on. | Ship schema first; enable per environment; rollback = flag off. |

---

## 1. Goals and non-goals

### Goals

1. Console sign-in in three modes (none, OIDC with Authorization Code + PKCE and
   server-side sessions, built-in accounts) — configurable by env bootstrap
   and in the console; roles at least admin vs viewer; in OIDC/local mode the
   whole console needs sign-in on entry.
2. FHIR auth per endpoint: off / write-protect / full SMART on FHIR, with
   issuer, JWKS, audience, scope policy and SMART discovery per endpoint,
   editable in the console.
3. Many FHIR endpoints by base path, each with name, path, FHIR version (R4
   now; model ready for R4B/R5/R6), storage isolation, auth, validation
   (off/log/enforce, terminology mode, required profiles; IG packages per
   endpoint), terminology server binding, CORS, rate limits,
   enable/disable; CRUD from console and admin REST API; hot reload; persisted
   in PostgreSQL; cluster-safe.
4. Duplicate an endpoint (config ± data); sync between endpoints and to/from
   external FHIR servers (push/pull, scheduled/continuous, initial load,
   conflict policy, job status/progress/logs, resumable, audited).
5. A console that is a FHIR servers manager: overview with health and
   metrics, per-endpoint pages (config, auth, validation & IGs, data browser,
   sync, audit), one design system, responsive, zh-Hant + en, HealthyMind
   branding.

### Non-goals (this design)

- The validator itself (line V) — only per-endpoint validation *config*.
- Bidirectional / multi-master sync, id remapping with reference rewriting
  (reserved as `IdStrategy`), FHIR version conversion during sync (source and
  target must speak the same release; checked at job creation).
- Dedicated database or schema per endpoint (reserved in `StorageBinding`).
- HFS as an authorization server (token issuance stays with the IdP).
- Several endpoints sharing one tenant (aliases) — reserved; v1 is 1:1.
- Making SQLite / MongoDB / S3 deployments editable: they get a read-only
  endpoint list from `HFS_ENDPOINTS_FILE` (memory store). Production is PG.
- HTS multi-tenancy. All endpoints share the configured HTS unless an endpoint
  binds another terminology server.
- The GCE VM deployment (paused by the user); nothing here depends on it,
  but D-1/D-18 keep any deployment that runs `hm/main` unchanged.

---

## 2. Baseline (what exists today, `hm/main` e5258f91e)

| Area | Today | Code |
|---|---|---|
| FHIR routing | One FHIR router at `/`; tenant from `X-Tenant-ID` (prod: `header_only`), or URL prefix in `url_path`/`both` mode via `strip_tenant_prefix` (`map_request` + `fallback_service`). | `crates/rest/src/routing/fhir_routes.rs`, `middleware/tenant_prefix.rs`, `extractors/tenant.rs` |
| Public URLs | `AppState::public_url_for_request` appends the tenant segment when the tenant came from the URL. ~60 call sites (Location, bundle links, fullUrl, bulk status/file URLs, subscriptions). | `crates/rest/src/state.rs`, `public_url.rs` |
| FHIR auth | One `AuthMiddlewareState` (one provider/issuer), layered only when `HFS_AUTH_MODE≠off`; admin credential and OIDC console session (#1449) accepted. | `crates/rest/src/middleware/auth.rs`, `crates/auth` |
| Console login | Upstream #1449 OIDC login (`HFS_UI_LOGIN_CLIENT_ID`, sessions in `login_sessions`) — not enabled in prod; admin password gate on admin pages only (`helios-admin-auth`, sessions in `admin_sessions`). Browsing pages are public. | `crates/ui/src/login.rs`, `admin_login.rs`, `crates/admin-auth` |
| Validation | Mode/terminology global env; profile registry per tenant; packages installed per tenant (request tenant). | `crates/rest/src/validation.rs`, `packages/` |
| Cluster | Shared PG; `resource_change_feed`, `search_parameter_epochs`, `cluster_jobs` (lease+fencing), subscription outbox + NOTIFY, shared admin sessions. | docs/ops/cluster.md |
| Migrations | Upstream ladder v55 + fork track `hm-0001`…`hm-0007` in `hm_schema_migrations`. | `crates/persistence/src/backends/postgres/schema.rs` |
| Prod | `HFS_BASE_URL=https://hfs.gugulu.tw/fhir`, nginx `/fhir/ → hfs:8080/`, `/ui → hfs:8080/ui`, `HFS_AUTH_MODE=write-protect` (test Keycloak realm `hfs-test`), `HFS_VALIDATION_MODE=log`, tenant `eval-t1` used by eval scripts via `X-Tenant-ID`. | `hfs-eval/docker-compose.yml`, `nginx/nginx.conf` |

---

## 3. Architecture

```
 browser / SMART app / integration                     operator
          │  https://host/fhir/{path}/…                   │ https://host/ui/…
          ▼                                               ▼
   nginx  (/fhir/ → hfs:/ ; /ui → hfs:/ui ; unchanged)
          │
 ┌────────┴──────────────────────── HFS process ─────────────────────────────┐
 │  axum Router                                                              │
 │   ├─ /ui/**            console (helios-ui)  ── console guard (P3) ──┐      │
 │   ├─ /admin/**         admin API (packages, tenants, endpoints P4,  │      │
 │   │                    sync P6, console users P3) ── admin guard ───┤      │
 │   ├─ /metrics /health  (unchanged)                                  │      │
 │   └─ fallback ─► EndpointDispatcher (P1)                            │      │
 │                   resolve(path) on Arc<EndpointSnapshot>            │      │
 │                   strip "/{path}", insert ResolvedEndpoint          │      │
 │                   503 if disabled/provisioning                      │      │
 │                     ▼                                               │      │
 │        per-endpoint stack (all read Extension<ResolvedEndpoint>):   │      │
 │        CORS → audit → rate limit → authn (verifier cache, P2) ──────┤      │
 │        → console-session principal (P3 resolver) → authz/SMART      │      │
 │        → tenant extractor (endpoint tenant authoritative)           │      │
 │        → FHIR router (handlers; public URLs from endpoint prefix;   │      │
 │          validation mode/terminology from endpoint)                 │      │
 │                                                                     │      │
 │  EndpointRegistry (ArcSwap snapshot + watch)  ◄── run_watcher ◄─────┼──┐   │
 │  SecretKeyRing (env)   OutboundPolicy (env)   Sync workers (P6)     │  │   │
 └─────────────────────────────────────────────────────────────────────┼──┼───┘
                                                                       ▼  │
   PostgreSQL: resources/… (tenant_id) │ hfs_endpoints, hfs_endpoint_revisions,
   hfs_config_epochs (+NOTIFY hfs_config) │ hfs_console_* │ hfs_sync_* │
   login_sessions, admin_login_failures │ cluster_jobs ────────────────┘
```

Crates:

- **`helios-endpoints`** (new, package 0): pure model + contracts
  (`Endpoint`, `EndpointSnapshot::resolve`, `EndpointRegistry`,
  `EndpointStore`, `SecretKeyRing`, console + sync contracts,
  `OutboundPolicy`). No axum, no FHIR models, no persistence dependency, so
  every other crate may depend on it. The PostgreSQL implementations of its
  store traits live in it behind feature `postgres` over the shared
  `deadpool_postgres::Pool` (the `helios-admin-auth` pattern); the DDL lives
  in persistence's fork track.
- `helios-rest`: dispatcher, per-endpoint middleware, admin endpoint API.
- `helios-auth`: verifier cache; OIDC client reused for the console.
- `helios-admin-auth`: grows local console accounts and the console guard.
- `helios-ui`, `helios-ui-chrome`: manager console.
- **`helios-sync`** (new, package 6): sync engines and workers.
- `helios-hfs`: wiring (`crates/hfs/src/manager/*.rs`, one file per package).

---

## 4. Data model

All tables are PostgreSQL fork migrations on `hm_schema_migrations`
(implemented on this branch; idempotent bodies; see
`crates/persistence/src/backends/postgres/schema.rs`). **Until the first
production deploy that runs them, their DDL may still be amended in place**;
after that only new `hm-00NN` entries.

### 4.1 `hm-0008-endpoints`

```sql
hfs_config_epochs(scope TEXT PK, epoch BIGINT, updated_at)     -- rows: endpoints, console, sync
hfs_bump_config_epoch() trigger fn: epoch+1, pg_notify('hfs_config', scope)

hfs_endpoints(
  id TEXT PK,                     -- UUID v4, immutable; AAD of the endpoint's secrets
  name TEXT, path TEXT, tenant_id TEXT,         -- denormalized from spec
  status TEXT CHECK (enabled|disabled|provisioning|deleted),
  managed_by TEXT CHECK (ui|env),
  spec JSONB,                     -- helios_endpoints::EndpointSpec (secrets sealed)
  revision BIGINT, created_at, updated_at, created_by, updated_by, deleted_at)
  UNIQUE(name), UNIQUE(path), UNIQUE(tenant_id)  WHERE status <> 'deleted'
  AFTER … FOR EACH STATEMENT → hfs_bump_config_epoch('endpoints')

hfs_endpoint_revisions(endpoint_id → hfs_endpoints, revision, change CHECK(create|update|status),
  status, spec JSONB (redacted), changed_at, changed_by, PK(endpoint_id, revision))
```

`EndpointSpec` (JSON, camelCase; the admin API body and the `spec` column) —
authoritative definition in `crates/endpoints/src/model.rs`:

| Field | Type | Notes |
|---|---|---|
| `name` | slug | immutable identifier; `default` only for the default endpoint |
| `path` | slug or `""` | URL segment; `""` only for default; may be renamed |
| `displayName`, `description`, `labels` | text | console |
| `fhirVersion` | `R4`/`R4B`/`R5`/`R6` | API refuses a release the binary lacks |
| `storage` | `{"kind":"tenant","tenantId":…}` | immutable after create (v1) |
| `status` | `enabled`/`disabled`/`provisioning`/`deleted` | |
| `auth` | `EndpointAuth` or `null` (= `HFS_AUTH_*`) | mode, issuer, jwksUrl, audience, algorithms, clockSkewSecs, jwksMaxAgeSecs, `scopes` (acceptV1, allowPatient/User/System, patientNonCompartment, patientSharedTypes), `smart` (discovery doc fields), acceptAdminCredential |
| `validation` | or `null` (= `HFS_VALIDATION_*`) | mode off/log/enforce, terminology off/embedded/remote/remote-only, storedProfiles, requiredProfiles[{resourceType, profile}] |
| `terminology` | or `null` (= `HFS_TERMINOLOGY_*`) | serverUrl, publicUrl, timeoutSecs, token (sealed) |
| `cors` | or `null` (= `HFS_CORS_*`) | enabled, allowedOrigins, allowCredentials, extraHeaders |
| `rateLimit` | or `null` (= `HFS_RATE_LIMIT_*`) | anonymousPerMinute, authenticatedPerMinute, burstSeconds |
| `consoleAccess` | `off`/`read-only`/`role-based` | console session as credential here (D-10) |
| `legacyTenantHeader` | bool | default endpoint only (D-7) |
| `managedBy` | `ui`/`env` | `env` = from `HFS_ENDPOINTS_FILE`, read-only |

Installed IG packages are **state of the endpoint's tenant** (the existing
`hfs_packages` registry keyed by tenant), not part of the spec.

### 4.2 `hm-0009-console-auth`

```sql
hfs_console_settings(id SMALLINT PK CHECK(id=1), settings JSONB, revision, updated_at, updated_by)
hfs_console_users(id PK, username, display_name, email, role CHECK(viewer|editor|admin),
  password_hash TEXT /* PHC argon2id */, disabled, must_change_password, revision,
  created_at, updated_at, last_login_at)    UNIQUE(lower(username))
hfs_console_role_bindings(subject, endpoint_id → hfs_endpoints ON DELETE CASCADE, role,
  created_at, created_by, PK(subject, endpoint_id))
triggers → hfs_bump_config_epoch('console')
```

Reused: upstream `login_sessions` (v44; `kind='console'` rows hold the console
session: subject, role snapshot, CSRF token, OIDC tokens when present,
expiry) and `admin_login_failures` (scope `HFS-CONSOLE`) for lockout.
Subjects: `local:{username}`, `oidc:{iss}|{sub}`, `admin` (break-glass).

### 4.3 `hm-0010-endpoint-sync`

```sql
hfs_sync_jobs(id PK, name UNIQUE, spec JSONB /* SyncJobSpec, secrets sealed */, enabled,
  source_endpoint_id, target_endpoint_id, next_run_at, revision, created_*, updated_*)
hfs_sync_runs(id PK, job_id → jobs CASCADE, cluster_job_id, state CHECK(queued|running|
  succeeded|failed|cancelled), reason, cursor JSONB, progress JSONB, error, fencing_token,
  created_at, started_at, finished_at, updated_at)   UNIQUE(job_id) WHERE state IN (queued,running)
hfs_sync_log(run_id → runs CASCADE, seq BIGSERIAL, at, level, message, PK(run_id, seq))
hfs_sync_conflicts(run_id, resource_type, resource_id, source_version, target_version,
  reason, at, PK(run_id, resource_type, resource_id))
hfs_sync_item_state(job_id → jobs CASCADE, resource_type, resource_id, source_version,
  target_version, content_sha256 BYTEA, deleted, applied_at, PK(job_id, resource_type, resource_id))
trigger on hfs_sync_jobs → hfs_bump_config_epoch('sync')
```

Run claiming reuses `cluster_jobs` (`JobKind::EndpointSync`, new variant).

### 4.4 Storage isolation — honest evaluation of "endpoint = tenant"

What the tenant mechanism gives for free: query-level isolation on every
backend (reviewed and fuzzed upstream: issues #317, #385, #1338); per-tenant
packages, profile registry, SearchParameter epochs, binding pins, bulk export
jobs and files, bulk submit, subscriptions, audit trail (AuditEvents are
written into the tenant of the request — so each endpoint's audit is its own
`/AuditEvent`), tenant purge (`DELETE /admin/tenants/{id}?purge=true`), and
tenant inventory counts in the console. Copying a tenant is a set-based SQL
operation (D-15).

What it does **not** give, and how we handle it:

| Limit | Consequence | Handling |
|---|---|---|
| Shared tables/indexes and one connection pool | Noisy neighbour: a big import on endpoint A slows B. | Per-endpoint rate limits; bulk worker concurrency is already bounded; document; dedicated DB reserved (`StorageBinding`). |
| One database = one backup/restore unit, one set of DB credentials | Cannot restore one endpoint alone; a SQL injection bug would cross tenants. | Restore-one-endpoint = restore dump to scratch DB and sync the tenant back (§9 tooling). For PHI vs public-sandbox separation, recommend a **separate deployment** (documented in the console's "new endpoint" help). |
| Terminology server shared (IG installs import CodeSystems into HTS globally) | A CodeSystem from endpoint A's IG is visible to B's remote terminology validation. | Acceptable (terminology is public content); per-endpoint `terminology.serverUrl` can point at another HTS. |
| Global env knobs that are not per tenant (`HFS_MAX_BODY_SIZE`, timeouts, SOF, NL search, subscriptions channels) | Same for every endpoint. | Listed as inherited server settings in the console; can be added to the spec later. |
| Implicit tenants in `url_path` routing mode | Unbound segments still route as tenants. | Prod uses `header_only`; with `HFS_MANAGER=on`, `url_path`/`both` is refused at startup unless `HFS_MANAGER_ALLOW_URL_TENANTS=true` (dispatcher runs first; a named endpoint never re-strips). |

Conclusion: the tenant mechanism fits v1's purpose (several FHIR services for
IG work, test/staging/prod-mirror, partner sandboxes on one installation)
and is the only option that reuses everything above. It is not a hard
multi-customer isolation boundary; the console says so.

---

## 5. Request routing for base paths

### 5.1 Dispatcher (P1)

`crates/rest/src/endpoint/dispatch.rs` wraps the FHIR stack exactly like
`create_url_tenant_routes` does today:

```rust
let fhir_stack = /* FHIR router + per-endpoint layers (auth, audit, …) */;
Router::new().fallback_service(
    fhir_stack.map_request(move |req| dispatch(req, &registry))   // strips prefix
)
```

`dispatch`: `registry.snapshot().resolve(path)` →
`Serve(r)`: rewrite URI to `r.remaining_path` (+ query), insert
`ResolvedEndpoint`, `OriginalPath`, and, for named endpoints,
`ExtractedTenantFromUrl(r.endpoint.tenant_id())` (so existing tenant code sees
a URL-routed tenant); `Unavailable(r)`: `503` OperationOutcome
`code=transient` "endpoint {name} is disabled" + `Retry-After` for
`provisioning`; `NotFound`: `404`. Explicit routes (`/ui`, `/admin`,
`/console`, `/metrics`, `/health`) match before the fallback, so the
dispatcher never sees them; reserved segments keep endpoints from shadowing
them. With `HFS_MANAGER=off` the dispatcher is not installed.

`TenantExtractor` gains `url_prefix: Option<String>` (the endpoint path) and
`public_base_url_for_request` / `public_url_for_request*` use the prefix
instead of the tenant id. Since tenant = name by default this changes
nothing for URL tenants and is the single place that makes every URL below
endpoint-correct.

### 5.2 Per-concern behaviour

| Concern | Per-endpoint behaviour | Where |
|---|---|---|
| `GET /{path}/metadata` | CapabilityStatement with `implementation.url` = endpoint public base, `fhirVersion` = endpoint release, `software.name` = product name, `security` from the endpoint auth (SMART service coding + `oauth-uris` only when auth ≠ off), resources/supported profiles from the endpoint's tenant. Cache keyed by `(endpoint id, snapshot epoch, version)`. | `handlers/capabilities.rs` |
| Bundle links (`self`/`next`/`prev`), `fullUrl`, `Location`, `Content-Location`, `_history` links, batch/transaction `response.location` | via `public_url_for_request*` → endpoint prefix. | `state.rs` + existing call sites (no change beyond state.rs if every site uses the helpers; P1 audits the ~60 sites; any raw `config.base_url` use is a bug) |
| `_include` / `_revinclude` / `$everything` | data is tenant-scoped already; `fullUrl`s use the prefix. | unchanged |
| Bulk `$export` | kick-off `Content-Location` = `{endpoint}/export-status/{id}`; manifest `output[].url` = `{endpoint}/export-file/…`; job row carries tenant; status/file requests are resolved through the dispatcher and must match the job's tenant (already enforced by `bulk_export_auth`). | `handlers/bulk_export.rs`, `bulk_export_auth.rs` |
| `$bulk-submit` | status/file URLs per endpoint; submissions land in the endpoint's tenant. Manifest/file fetching keeps its existing controls (`HFS_BULK_SUBMIT_*` allow list, OAuth/JWE) — not moved to `OutboundPolicy` in v1. | `handlers/bulk_submit.rs` |
| SMART discovery | `/{path}/.well-known/smart-configuration` from the endpoint's `auth.smart` (+ issuer discovery); `404` when auth is off. | `handlers/smart_discovery.rs` (P2) |
| Audit | the audit layer adds the endpoint to every AuditEvent: `source.observer` unchanged; `entity` of type "endpoint" is not standard, so an extension `https://healthymind.tech/fhir/StructureDefinition/hfs-endpoint` (valueString = name) on `AuditEvent` and the `agent.network` unchanged. Events go to the endpoint's tenant (existing rule). Admin API events go to the default tenant with entity `Endpoint/{name}` description. | `middleware/audit.rs`, `helios-audit` |
| Subscriptions | engine is tenant-scoped already; notification bundles' `fullUrl` and the `SubscriptionStatus` reference base come from a `BaseUrlResolver` (tenant → endpoint public base, from the snapshot) injected into the engine; WebSocket URL `wss://host/fhir/{path}/ws`. | `helios-subscriptions::notification::builder`, `handlers/ws.rs` |
| Validation | `ValidationService::check_write` takes an `EffectiveValidation` (mode, terminology mode, required profiles) resolved from the endpoint (or env when inherited) instead of reading `ServerConfig`; profile registry already per tenant. Terminology client chosen by URL from a small cache (`TerminologyClients: url → client`). | `validation.rs`, `terminology.rs` |
| IG packages | `/admin/endpoints/{name}/packages[/…]` = existing package API with tenant = endpoint tenant and the endpoint's terminology server/token for HTS imports; old `/admin/packages` keeps meaning default endpoint (+ `X-Tenant-ID` for unbound tenants). | `routing/admin_packages.rs` |
| SearchParameters | custom SPs are stored per tenant and the epoch is per tenant: an SP posted to endpoint A does not affect B. | unchanged |
| FHIR version | `ServerConfig::default_fhir_version` reads become `effective_version(req)` = endpoint release; the resource-type gate uses it; `$versions` lists the endpoint's release (plus negotiable ones). | `extractors/fhir_version.rs`, `middleware/resource_type.rs` |
| CORS | the global `CorsLayer` on the FHIR stack is replaced by `endpoint_cors` middleware (answers preflight and decorates responses from the endpoint's `cors` or env defaults). Non-FHIR routes keep the global layer. | `middleware/cors.rs` (new, P1) |
| Rate limits | `ClientRateLimiter` keyed by `(endpoint id, client)`; budgets from the endpoint; buckets of a removed endpoint dropped on snapshot change. | `middleware/client_rate_limit.rs` (P2) |
| `X-Tenant-ID` | default endpoint: honoured for unbound tenants iff `legacyTenantHeader`; refused `403` for a tenant bound to a named endpoint (message names its URL). Named endpoint: a header naming another tenant → `400` (strict) — never selects. JWT tenant claims: on named endpoints a claim must equal the endpoint's tenant or be absent. | `extractors/tenant.rs` |
| Metrics | Prometheus label `endpoint` on request metrics (bounded: endpoints are few; cap 200 endpoints per deployment, enforced by the API). | `helios-observability` |

### 5.3 Hot reload and cluster

1. Admin API write → store (transaction: row + revision) → trigger bumps
   `hfs_config_epochs('endpoints')` and NOTIFYs.
2. The writing instance calls `registry.reload_if_stale()` before answering.
3. Every instance: `run_watcher` polls the epoch every
   `HFS_CLUSTER_SYNC_INTERVAL_MS` and a LISTEN task (`hfs_config`) calls
   `registry.poke()`; snapshot swapped atomically; `watch` subscribers
   (verifier cache, CapabilityStatement cache, rate-limit tables, CORS) rebuild.
4. In-flight requests keep the snapshot they resolved with.

Staleness bound: NOTIFY ~ms; poll 1 s worst case. A deleted endpoint
therefore may serve for ≤ 1 s on other instances — documented, acceptable.

---

## 6. Per-endpoint FHIR authentication (P2)

- **Verifier cache** (`crates/auth/src/verifier_cache.rs`):
  `VerifierKey { issuer, jwks_url, algorithms, clock_skew, jwks_max_age }` →
  `Arc<JwksBearerAuthProvider>` (existing type, built from an `AuthConfig`
  derived from `EndpointAuth`). Entries unused by the current snapshot are
  dropped on snapshot change. IdP-down behaviour per verifier as today
  (503 Retry-After for token requests, background refetch).
- **Effective auth**: `EffectiveAuth::for_request(&ResolvedEndpoint, &env)`
  → `{ mode, verifier, audience, scope_policy, smart, accept_admin, console_access }`.
  Inherited (`auth: null`) = the env `AuthConfig` exactly as today.
- **Middleware**: one always-installed `endpoint_auth` layer replaces the
  conditional `auth_middleware` + `authz_middleware` pair; with mode `off`
  it is a pass-through (identical to today's "no layer"). Inside, the
  existing functions run unchanged with the effective state, so the
  write-protect classification (`anonymous_read_allowed`), patient
  compartment, granular scopes and bundle filtering (`smart_access`) are
  reused as is. Audience is checked per endpoint even when two endpoints
  share a verifier.
- **Credentials accepted, in order**: `Authorization: Bearer <JWT>` →
  endpoint verifier; Basic / non-JWT bearer → admin credential iff
  `acceptAdminCredential`; no `Authorization` + console session cookie →
  P3's `ConsoleSessionResolver` → console principal with
  `ConsoleRole::data_scopes(consoleAccess)` (writes also need a same-origin
  `Origin` — existing rule); else anonymous.
- **Scope policy**: `allowPatient/User/System=false` refuses tokens whose
  scopes are all of that context (403); `acceptV1=false` ignores v1 scopes.
- **SMART discovery** per endpoint (§5.2).
- **Admin API tokens**: `/admin/**` accepts a JWT only from the default
  endpoint's issuer (or the console OIDC issuer) carrying scope
  `HFS_ADMIN_TOKEN_SCOPE` (default `system/*.cruds`, stricter than today's
  `system/*.r` for the *new* endpoint/sync/user APIs; the existing
  `/admin/packages` and `/admin/tenants` keep today's rule for compatibility).

Warnings surfaced by the console (non-blocking): auth on without audience;
two endpoints with the same issuer and same/absent audience; `http` JWKS URL
not allow-listed; write-protect with `accept_admin_credential` on (normal for
break-glass, shown as info).

---

## 7. Web console authentication (P3)

### 7.1 Modes

| | `none` | `oidc` | `local` |
|---|---|---|---|
| Entry to `/ui` | open; visitor role = `anonymousRole` (default viewer; `null` = sign-in required) | redirect to IdP (Authorization Code + PKCE S256, `state`, `nonce`) | sign-in form |
| Roles | anonymous role; admin pages need break-glass | from `roleClaim` via `roleMapping`, else `defaultRole`, else "no access" page | per account (+ endpoint bindings) |
| Sessions | — | server-side; IdP tokens kept server-side; refresh before expiry; IdP end-session on sign-out | server-side |
| Break-glass `HFS_ADMIN_PASSWORD` | `/ui/login?local=1` | same | same (user `admin`, not stored) |

Session: random 256-bit id, cookie `hfs_console` `Path=/; HttpOnly;
SameSite=Strict; Secure` (auto by scheme as today), SHA-256 of the id stored
in `login_sessions` (`kind='console'`), idle (default 8 h) and absolute (24 h)
lifetime, revocation reaches other instances within 10 s (same mechanism as
admin sessions), user disable / role change ends or downgrades sessions on
the next request (session holds the user revision; mismatch → re-read).
The OIDC pending-login cookie is `SameSite=Lax` (cross-site return from the
IdP), 10 min, bound to `state`.

Local accounts: argon2id (`m=19456 KiB, t=2, p=1`, OWASP 2024), new crate
dependency `argon2` (the only new third-party dependency in this design,
added by P3 with the lock update); min length 12 (configurable), check
against the top-10k breached list shipped in the binary; lockout 5 failures /
15 min per client address and per account (existing `admin_login_failures`);
forced change on first sign-in for accounts created by an admin; the last
enabled admin cannot be disabled/demoted/deleted. First run in `local` mode
with no accounts: only break-glass can sign in, and the console prompts to
create the first admin.

### 7.2 Authorization of console, admin API and FHIR calls

| Request | Credential | Rule |
|---|---|---|
| `/ui/**` pages | console session (or anonymous role in `none`) | viewer: all browse pages; editor: + data-writing pages; admin: + Endpoints/Auth/Users/Sync/Packages/Tenants pages. Endpoint pages use `role_for(endpoint)`. |
| `/admin/**` | console session (admin), break-glass (Basic/Bearer/session), admin JWT (§6) | admin role only; same-origin + CSRF token for cookie writes |
| FHIR `/{path}/…` from the console's JS | console session cookie (same-site) | §6 console principal; writes need editor+ and `consoleAccess=role-based` |

`helios-admin-auth` becomes the single guard: `ConsoleGuard { settings,
resolver, break_glass }` replaces the admin-pages-only gate; HTS keeps its
own password gate, plus a new `HTS_CONSOLE_REQUIRE_LOGIN=true` switch that
puts all of `/ui/hts` behind its admin session so the visible console has no
public area in `oidc`/`local` mode (HTS OIDC SSO: future).

### 7.3 CSRF

1. `SameSite=Strict` session cookie.
2. Every state-changing request authenticated by cookie must carry `Origin`
   (else `Referer`) equal to the request host, `HFS_BASE_URL`'s origin or
   `allowedOrigins` (existing `same_origin` rule).
3. Synchronizer token: `login_sessions.data.csrf` (random 128-bit), rendered
   into `<meta name="csrf-token">`; `hx-headers` sends `X-CSRF-Token` on every
   htmx request, plain forms carry a hidden field; checked in constant time.
   FHIR API writes from the console JS send the header too; requests with a
   bearer token are exempt (not ambient).

### 7.4 Configuration

Env bootstrap (applied when `hfs_console_settings` is empty; a set variable
also pins its field, shown read-only):
`HFS_CONSOLE_AUTH_MODE`, `HFS_CONSOLE_ANONYMOUS_ROLE`,
`HFS_CONSOLE_OIDC_ISSUER`, `…_CLIENT_ID`, `…_CLIENT_SECRET(_FILE)`,
`…_SCOPES`, `…_ROLE_CLAIM`, `…_ADMIN_VALUES`, `…_EDITOR_VALUES`,
`…_VIEWER_VALUES`, `…_DEFAULT_ROLE`, `HFS_CONSOLE_SESSION_IDLE_MINUTES`,
`HFS_CONSOLE_BREAK_GLASS` (default true), `HFS_CONSOLE_ALLOW_ANONYMOUS_ADMIN`
(dev only). Legacy `HFS_UI_LOGIN_*` are read as the OIDC bootstrap when the
new ones are unset (mode `oidc`), so an upstream-style deployment keeps
working. Switching mode in the console requires: a working test sign-in
(OIDC: "Test sign-in" round trip must succeed in the same browser) and, for
`local`, at least one enabled admin account — otherwise the switch is refused.

---

## 8. Admin REST API (P4 endpoints, P3 console, P6 sync)

All JSON, errors as OperationOutcome (`422` validation with one issue per
field, `expression` = field path; `409` conflict carrying `currentRevision`;
`412` on stale `If-Match`), concurrency via `ETag: "rev"` / `If-Match`,
every write audited (AuditEvent in the default tenant, `entity.what` =
`Endpoint/{name}` descriptor, actor = `Actor`).

```
GET    /admin/endpoints                         list (redacted), ?include=deleted,stats
POST   /admin/endpoints                         create  (body EndpointSpec + secret inputs) → 201
GET    /admin/endpoints/{name}                  one (redacted) + ETag
PUT    /admin/endpoints/{name}                  replace spec (If-Match) — secrets via SecretInput
PATCH  /admin/endpoints/{name}                  JSON merge patch (If-Match)
POST   /admin/endpoints/{name}/$enable | $disable
DELETE /admin/endpoints/{name}?confirm={name}[&purge=true]   tombstone (+ async tenant purge job)
GET    /admin/endpoints/{name}/history          revisions (redacted)
POST   /admin/endpoints/{name}/$duplicate       DuplicateRequest → 202 + job (Location)
POST   /admin/endpoints/{name}/$test-auth       fetch issuer discovery + JWKS through OutboundPolicy; report
GET    /admin/endpoints/{name}/stats            counts by type, req/min, p95, errors, last write
GET|POST|DELETE /admin/endpoints/{name}/packages[/{pkg}/{ver}]  package API scoped to the endpoint
GET    /admin/endpoints/{name}/audit?…          AuditEvent search in the endpoint tenant (paged)
POST   /admin/secrets/$rewrap                   re-seal everything under the active key

GET|PUT /admin/console/settings                 (P3) redacted; If-Match
POST    /admin/console/settings/$test-oidc      (P3)
GET|POST /admin/console/users ; GET|PUT|DELETE /admin/console/users/{id}
POST    /admin/console/users/{id}/$reset-password
GET|PUT|DELETE /admin/console/bindings/{subject}/{endpoint}

GET|POST /admin/sync/jobs ; GET|PUT|DELETE /admin/sync/jobs/{id}      (P6)
POST    /admin/sync/jobs/{id}/$run | $cancel | $retry
GET     /admin/sync/jobs/{id}/runs ; /admin/sync/runs/{run}  (+ /log?after=, /conflicts)
POST    /admin/sync/jobs/{id}/notify             rest-hook receiver (shared-secret header)
```

Example create:

```json
POST /admin/endpoints
{ "name": "twcore", "path": "twcore", "displayName": "TW Core sandbox",
  "fhirVersion": "R4", "storage": {"kind": "tenant", "tenantId": "twcore"},
  "auth": {"mode": "write-protect", "issuer": "https://hfs.gugulu.tw/auth/realms/hfs-test",
           "jwksUrl": "http://keycloak:8080/auth/realms/hfs-test/protocol/openid-connect/certs",
           "audience": "twcore-api"},
  "validation": {"mode": "enforce", "terminology": "remote"},
  "secrets": {"terminology.token": {"set": "…"}} }
```

Secret fields are never in responses (`{"set": true}`); requests change them
through `secrets: { "<field path>": SecretInput }` so a client that PUTs back
a GET response cannot accidentally clear them (`keep` is the default).

`HFS_ENDPOINTS_FILE` (JSON array of `EndpointSpec`, env-managed): reconciled
at startup by the bootstrap actor; those endpoints are `managedBy=env` and
read-only in API/console. This is also the only way to declare endpoints on
non-PG backends (memory store).

---

## 9. Sync and duplicate (P6)

### 9.1 Job model

`SyncJobSpec` (`crates/endpoints/src/sync.rs`): `source`, `target`
(`{"kind":"endpoint","name":…}` or `{"kind":"external","baseUrl":…,"auth":…}`),
`resourceTypes` (empty = all but `AuditEvent`, `Subscription`,
`SubscriptionStatus`), per-type `filters`, `since`, `initialLoad`
(`none|storage-copy|paged-read|bulk-export`), `includeHistory`, `trigger`
(`manual|interval|continuous|subscription`), `conflict`
(`source-wins|skip-if-target-newer|fail`), `propagateDeletes`, `idStrategy`
(`preserve`), `batchSize`, `enabled`.

API-level checks: peers exist; source ≠ target; same FHIR release on both
sides (external: read from its CapabilityStatement at `$test`); **no cycles**
(a job whose target is the source of an enabled job back to this source is
refused — loop prevention instead of bidirectional semantics); external URLs
pass `OutboundPolicy`; a local target endpoint with validation `enforce` is
allowed (writes are validated like any write; failures become `failed` items).

### 9.2 Execution

- Scheduler (one per cluster: advisory lock `HFS\0SYNC`) enqueues runs:
  interval jobs at `next_run_at`; continuous jobs keep one long run; manual
  via API. Runs are `cluster_jobs` rows (`kind=endpoint-sync`), claimed with
  `FOR UPDATE SKIP LOCKED`, heartbeat every 10 s, lease 60 s; the run row's
  `fencing_token` = lease token; `checkpoint()` refuses a stale token
  (`LeaseLost` → worker stops). Worker pool `HFS_SYNC_WORKERS` (default 2).
- **Engines** (trait `SourceReader` / `TargetWriter`):
  - Local source: reads through storage (`SystemHistoryProvider` with
    `_since`, search pages by `_lastUpdated`), no HTTP, tenant context of the
    source endpoint.
  - External source: HTTP client (reqwest, rustls) with `OutboundPolicy`
    (DNS-resolve, check, pin), auth from `ExternalAuth` (token cache for
    client-credentials / private_key_jwt — reuse `bulk_submit_oauth.rs`),
    `Retry-After`-aware backoff, rate limit `maxRequestsPerSecond` (token
    bucket), response size cap.
  - Local target: storage writes in batches (`BundleProvider` transaction of
    `PUT`s, versioned) — write observers fire (subscriptions, audit) like any
    write, audit agent = `Device/hfs-sync` + job id.
  - External target: transaction or batch Bundle of `PUT [type]/[id]`
    (fallback to individual PUTs when the server refuses transactions);
    `If-Match` when `conflict≠source-wins`.
  - `StorageCopy` (PG only, local→local): one transaction per type:
    `INSERT INTO resources … SELECT … tenant_id := target`, same for
    `resource_history` (optional) and `search_index`; then package registry
    rows; then the profile registry rehydrates for the new tenant. Measured
    target for prod size (8.7k resources / 240k index rows): < 30 s.
  - `BulkExport` initial load: `$export` at the source (local: in-process
    job), stream NDJSON → target writer.
- **Incremental**: `_history?_since={high_water}&_count=N` (inclusive),
  cursor `boundary` = entries applied at exactly `high_water` (skip on
  re-read); deletes in history → `DELETE` at target when
  `propagateDeletes`. Subscription trigger: the job creates (or the operator
  provides) a rest-hook Subscription at the source with id-only payload to
  `/admin/sync/jobs/{id}/notify` (header secret, sealed); notifications only
  *wake* a pass — the history query stays the source of truth.
- **Idempotency**: every write is `PUT` by id; `hfs_sync_item_state` stores
  source version + SHA-256 of canonical JSON (minus `meta.versionId`,
  `meta.lastUpdated`); unchanged → `skipped`, no target version churn.
  Re-running a batch after a crash is harmless.
- **Conflicts**: `skip-if-target-newer` compares target `meta.lastUpdated`;
  `fail` detects target changes since our last write (target hash ≠ stored
  `target_version`/hash) → `hfs_sync_conflicts`, item skipped, run continues;
  UI lists conflicts with "overwrite / keep" actions (overwrite = one-item
  run with `source-wins`).
- **Status / logs**: progress counters + cursor checkpointed per batch;
  bounded log (10k lines/run) of `type/id/version` and errors only — never
  resource content or credentials. UI polls `/admin/sync/runs/{id}` (htmx
  every 2 s while running).
- **Resumable**: `$retry` creates a run with `resume_from` = failed run's
  cursor; a lost lease resumes automatically on another instance.
- **Audit**: job create/update/delete/run/cancel → AuditEvent (default
  tenant); every target write audited as a normal write with the sync agent.

### 9.3 Duplicate (`POST /admin/endpoints/{src}/$duplicate`)

1. Validate `DuplicateRequest`; create the new endpoint `status=provisioning`,
   spec copied (auth if `auth`, secrets re-sealed under the new id's AAD if
   `secrets`, else secret fields cleared; new tenant = name).
2. `data=none`: install the source's packages into the new tenant (package
   manager jobs), then `enabled`.
   `data=current|history`: a one-off sync job (hidden, `reason=duplicate`)
   with `StorageCopy` (copies package registry and conformance resources with
   the data), then `enabled`. On failure the endpoint stays `provisioning`
   with the error; "Retry" resumes; "Discard" deletes + purges.
3. `keepInSync`: create a continuous job src → copy.

---

## 10. Console UI / UX (P5, with P3 and P6 pages)

### 10.1 Information architecture

```
/ui                         Overview (all endpoints)                    viewer
/ui/endpoints/new           New endpoint wizard                         admin
/ui/e/{ep}                  Endpoint home (dashboard)                   viewer
/ui/e/{ep}/data/…           Data browser: resources, search, editor,    viewer (write: editor)
                            history, batch, compartments, validate
/ui/e/{ep}/config           General: name, path, version, status,       viewer (edit: admin)
                            labels, storage, danger zone
/ui/e/{ep}/security         FHIR auth + SMART + CORS + rate limits      viewer (edit: admin)
/ui/e/{ep}/validation       Validation mode, terminology, required      viewer (edit: admin)
                            profiles, IG packages (install/remove)
/ui/e/{ep}/terminology      Terminology binding, link to HTS console    viewer (edit: admin)
/ui/e/{ep}/subscriptions    Subscriptions of this endpoint              viewer
/ui/e/{ep}/bulk             Bulk export / import jobs                   editor
/ui/e/{ep}/sync             Jobs touching this endpoint                 viewer (edit: admin)
/ui/e/{ep}/audit            AuditEvents of this endpoint                viewer
/ui/e/{ep}/history          Config revision history (diffs)             viewer
/ui/sync, /ui/sync/{job}, /ui/sync/runs/{run}   all sync jobs            viewer (edit: admin)
/ui/settings/auth           Console sign-in mode, OIDC, local policy    admin
/ui/settings/users          Users & roles, endpoint bindings            admin
/ui/settings/server         Effective env settings (read-only), secret  admin
                            key status, instances, migrations
/ui/about                   Product, version, licenses, attribution     everyone
/ui/login, /ui/logout, /ui/callback, /ui/account (password, language, theme)
```

Legacy paths (`/ui/resources`, `/ui/search`, `/ui/packages`, `/ui/tenants`,
…) answer `302` to the `/ui/e/default/…` equivalent for one release, then are
removed. The tenants page remains under Settings → Server for unbound legacy
tenants, with an "adopt as endpoint" action (creates an endpoint bound to an
existing tenant).

### 10.2 Branding (HealthyMind Tech)

- **Product name**: zh-Hant **療心智能 FHIR 伺服器管理平台**, en **HealthyMind
  FHIR Server Manager**; short name "HealthyMind FHIR" where space is tight
  (mobile topbar, `<title>` suffix). The CapabilityStatement `software.name`
  stays `Helios FHIR Server` with `software.version` the build version
  (spec conformance tools key on it) — the console brand is UI-only.
- **Assets** (official logos from the brand folder, copied by the README line
  to `docs/images/brand/` on `hm/readme`; P5 vendors them into
  `crates/ui/assets/brand/` as optimized PNG + a traced SVG of the heart mark):
  `healthymind-logo-horizontal.png` (heart + 療心智能 + HealthyMind.Tech),
  `healthymind-logo-square.png` (heart + HealthyMind), transparent and
  white-background variants.
- **Placement**:
  - Topbar (desktop ≥ 1024 px): horizontal logo, transparent variant, 28 px
    high, left; at < 1024 px the heart mark alone (32 px) + short name.
    Dark theme: the transparent logo is used as is (the gradient keeps ≥ 3:1
    against the dark surface for the mark; the wordmark text in the logo is
    decorative — the product name is also rendered as real text next to it
    for screen readers / i18n).
  - Sign-in page: square logo 120 px centred above the form, product name as
    `<h1>`, on a subtle full-bleed brand gradient band (decorative only, no
    text over it).
  - Favicon and app icons from the heart mark of the square logo:
    `favicon.svg` (traced mark), `favicon-32.png`, `apple-touch-icon-180.png`,
    `icon-192/512.png` + `manifest.webmanifest` (`theme_color` = primary).
  - About page and footer: "療心智能 HealthyMind Tech · Based on Helios FHIR
    Server (MIT License) by Helios Software" with link to the upstream
    repository and the bundled `LICENSE.md`; third-party licenses list.
  - HTS console (`/ui/hts`) uses the same chrome (`helios-ui-chrome`) and
    tokens; name "療心智能 術語伺服器 / HealthyMind Terminology Server".
- **Design tokens** (CSS custom properties in `crates/ui-chrome` shared
  stylesheet; sampled from the logo: gradient `#0AEAFF` (cyan, top-left) →
  `#3EADFF` → `#756DFF` (violet, bottom-right)). The raw brand colours fail
  text contrast on white (cyan 1.6:1, mid 2.4:1, violet 3.9:1), so they are
  used **only decoratively** (logo, gradient bands, focus glow, chart series);
  text and controls use derived shades that meet WCAG 2.2 AA:

  | Token | Light | Dark | Contrast (measured) |
  |---|---|---|---|
  | `--brand-gradient` | `linear-gradient(135deg,#0AEAFF,#3EADFF 50%,#756DFF)` | same | decorative |
  | `--color-bg` | `#F6F8FC` | `#0E1726` | — |
  | `--color-surface` | `#FFFFFF` | `#131C2E` | — |
  | `--color-text` | `#0E1726` | `#E8EEF8` | 17.9:1 / 15:1+ |
  | `--color-text-muted` | `#4A5568` | `#A7B3C6` | ≥ 7:1 / ≥ 7:1 |
  | `--color-primary` (links, primary buttons) | `#2F5BE0` | `#5CB8FF` | 5.7:1 on white; 8.3:1 on `#0E1726` |
  | `--color-on-primary` | `#FFFFFF` | `#0E1726` | 5.7:1 / 8.3:1 |
  | `--color-accent` (secondary emphasis, selected tab) | `#5A54E8` | `#A5A0FF` | 5.4:1 / 7.8:1 |
  | `--color-info` (cyan family, text) | `#0A7BB8` | `#49C6FF` | 4.6:1 / 9.2:1 |
  | `--color-focus-ring` | `#3EADFF` 2 px + 2 px offset | `#49C6FF` | ≥ 3:1 non-text |
  | `--color-success / warning / danger` | `#1E7F4F / #9A5B00 / #C62828` | `#5FD39A / #F2B84B / #FF7A7A` | ≥ 4.5:1 |

  Status pills (enabled/disabled/provisioning), auth badges
  (off/write-protect/full) and validation badges (off/log/enforce) use icon +
  text, never colour alone. Typography: system UI stack with
  `"Noto Sans TC"` fallback for zh-Hant (vendored subset, no CDN);
  spacing scale 4/8/12/16/24/32; radius 8; one elevation level.
  `prefers-color-scheme` + manual theme toggle (stored per user setting).

### 10.3 Page wireframes (structure, not pixels)

- **Overview** (`/ui`): header "FHIR endpoints" + [New endpoint] (admin).
  Health strip: DB ok/latency, HTS ok, instances N (from cluster heartbeat),
  secret key ring set/not set, console mode. Grid of endpoint cards
  (responsive 3/2/1 columns): name + display name, public base URL with copy
  button, FHIR version chip, status pill, auth badge, validation badge,
  resources count, req/min sparkline (last hour), error % and p95, last write
  time, sync status dot (running/failed). Card menu: Open, Duplicate,
  Disable/Enable (admin). Filter by label/status, search by name. Empty state
  explains endpoints with a link to docs.
- **New endpoint wizard**: 1 Basics (display name → suggested name/path with
  live availability check, FHIR version), 2 Data (empty / adopt existing
  tenant / duplicate from endpoint [none/current/history]), 3 Security (inherit
  server / off / write-protect / full; issuer + JWKS + audience; "Test" button
  calling `$test-auth`), 4 Validation (inherit / mode / terminology; pick IG
  packages from registry), 5 Review (JSON preview, warnings) → Create; then a
  progress page if data is being copied.
- **Endpoint home**: title bar (name, base URL copy, status, quick actions),
  KPI tiles (resources, writes 24 h, errors 24 h, p95), resources-over-time
  chart (existing dashboard, now per endpoint), top resource types, recent
  audit events, active jobs.
- **Config / Security / Validation pages**: form sections with "Inherit from
  server" toggle per section showing the effective inherited values
  read-only; inline field validation (server returns 422 field errors mapped
  to inputs); sticky save bar with diff preview and revision conflict
  handling ("changed by X at T — reload / overwrite"); secrets show "set /
  not set" with Replace and Clear buttons. Env-managed endpoints show a lock
  banner. Danger zone: rename path (shows impact: clients must change URL),
  disable, delete (type the name), purge data (type the name, admin).
- **Data browser**: today's pages moved under the endpoint, with endpoint
  switcher in the rail.
- **Sync**: job list (name, source → target, trigger, last run state,
  progress bar, next run); job page (definition, run history, Run/Cancel/
  Retry); run page (live counters, phase, per-type progress, log tail with
  level filter, conflicts table with actions). Job editor: source/target
  pickers (endpoint dropdown or external URL + auth type with secret inputs +
  "Test connection"), types, initial load, trigger, conflict policy.
- **Settings → Authentication**: mode radio (with consequences explained),
  OIDC form (issuer, client id, secret, scopes, role claim, mapping chips,
  default role) + "Test sign-in" button, local policy, session lifetimes,
  break-glass toggle; pinned-by-env fields locked with tooltip.
- **Settings → Users**: table (username, name, role, status, last sign-in),
  create/edit drawer, reset password (one-time shown), per-endpoint bindings.
- **Sign-in page**: brand (§10.2), username/password (local) or "Sign in
  with {IdP}" (oidc), language switch, break-glass link small at bottom.

### 10.4 Responsiveness, accessibility, i18n

Breakpoints: ≥ 1200 px rail + content; 768–1199 collapsible rail; < 768
top bar with menu drawer, cards single column, tables become stacked lists.
Keyboard navigable, visible focus ring, `aria-live` for job progress, WCAG
2.2 AA contrast (tokens above). All strings in Fluent catalogs
(`crates/ui/locales/{en,zh-Hant}`) — new keys added in both in the same
commit (CI check: no key missing in en or zh-Hant; de/es fall back to en).
Server-rendered htmx stays (no SPA framework, no CDN).

---

## 11. Migration plan from today's single endpoint (zero downtime)

1. **Release R0 (`HFS_MANAGER=off`)**: binary with migrations hm-0008..0010
   (additive tables only, safe for rolling restart with older instances —
   older instances ignore them). At startup each instance runs
   `ensure_default_endpoint(default tenant, HFS_DEFAULT_FHIR_VERSION)`
   (idempotent, race-safe). Nothing else changes. Rollback: previous image;
   the tables stay (harmless), schema version unchanged (fork track only).
2. **R1 test stack**: `HFS_MANAGER=on` with only the default endpoint → the
   dispatcher resolves every request to the default endpoint with all
   sections inherited: regression gates (prod_verify 42/42, validator 94/94,
   test_hts 112/112, test_hfs_server 184/184) must be identical.
3. **R2 prod**: same as R1 on hfs.gugulu.tw (flag on, no named endpoints).
   Set `HFS_SECRET_KEYS` (file) first. Gates as above.
4. **R3**: create first named endpoint(s) (e.g. `twcore` with enforce
   validation; `eval` adopting tenant `eval-t1` so eval scripts move from
   `X-Tenant-ID` to `/fhir/eval`), then turn `legacyTenantHeader` off when no
   client uses the header (audit shows header-selected tenants).
5. **R4 console sign-in**: `HFS_CONSOLE_AUTH_MODE=oidc` against the test
   Keycloak (`hfs-web` client already exists, disabled) or `local`; break-glass
   remains.
6. **R5 sync**: first jobs (e.g. nightly prod → staging copy).

The default endpoint is the current data (tenant `default`), its URL is
unchanged, and no data moves at any step.

---

## 12. Security review

| Threat | Mitigation |
|---|---|
| Cross-endpoint data access via `X-Tenant-ID` or URL tenant on the root door | D-7: bound tenants have one door; named endpoints never select by header; JWT tenant claim must match the endpoint tenant. Tests in P1. |
| Token for endpoint A accepted at B | per-endpoint audience; console warning when audiences are absent/shared; issuer pinned per endpoint. |
| Console session used as ambient credential (CSRF) | D-11 (SameSite=Strict, Origin check, synchronizer token); session is a credential only per `consoleAccess`. |
| Console role escalation | role checked server-side on every page/action/API; role changes invalidate sessions via user revision; last-admin rule; endpoint bindings cannot grant server administration. |
| Admin lockout | break-glass `HFS_ADMIN_PASSWORD`; env-pinned console settings; mode switch requires a successful test sign-in. |
| Credential stuffing on local sign-in | argon2id, lockout per IP and per account, breached-password list, generic error messages, constant-time compare, audit of failures (no passwords logged). |
| Secrets in DB dumps / backups | AES-GCM sealed (D-12), key ring only in env/file; dumps are useless without it; rotation via `$rewrap`; API never returns secrets; logs redact (`Debug` impls). Lost key ring = re-enter secrets (documented); key ring backed up separately from DB dumps. |
| SSRF via sync targets, issuer/JWKS/token/terminology URLs | D-13 `OutboundPolicy` (https, metadata names, private/link-local/CGNAT/ULA/NAT64 ranges after DNS, pinned connection, redirect re-check or off, size + time limits); allow list env-only. Env-configured URLs trusted. |
| Malicious external server responses (sync pull) | size caps, JSON depth limit, resource type checked against scope, writes go through normal validation and storage paths, ids validated, no reference following to other hosts. |
| Data exfiltration via sync push by a compromised admin | sync jobs and external targets are admin-only, audited, shown on overview; optional `HFS_SYNC_EXTERNAL=off` env switch disables external peers entirely. |
| Path squatting / route confusion | reserved segments; lowercase grammar; snapshot refuses duplicates; dispatcher only matches whole first segment. |
| Disabled/deleted endpoint still serving | ≤ 1 s staleness across cluster; deleted = not routed; purge is explicit and typed-name confirmed. |
| Stale caches across endpoints (CapabilityStatement, validation, terminology expansions, rate-limit buckets) | caches keyed by endpoint id + snapshot epoch or by tenant; review checklist item for every cache in P1/P2. |
| Audit gaps | endpoint name on every AuditEvent; admin API and sync writes audited; config history in `hfs_endpoint_revisions`. |
| Denial of service by one endpoint | per-endpoint rate limits; bulk/sync worker caps; max 200 endpoints. |

---

## 13. Testing strategy

Every package ships **complete unit tests plus integration tests wherever
routing, auth or the database is involved**, and they must pass in the
HealthyMind CI workflow (line T, `hm/tests-ci`): `cargo fmt --check`,
`cargo clippy -D warnings` for the production feature set and
`--all-features`, crate-grouped test jobs including a `postgres:16` service
container reached through `HFS_TEST_PG_HOST`/`HFS_TEST_PG_PORT` (the
persistence tests' existing switch), and `cargo-llvm-cov` coverage. Target:
≥ 85 % line coverage of new modules. PG tests are named `postgres_integration_*`
and gated on `--features postgres` like the existing ones.

| Layer | What |
|---|---|
| Unit | model validation, name grammar, snapshot resolution, registry reload, store conformance (memory + PG run the same `store::conformance::run_all`), secret seal/open/rotation/tamper, outbound policy ranges, role mapping, CSRF token check, sync cursor/boundary logic, conflict decisions. |
| Router integration (axum `oneshot`, memory/SQLite storage) | for default and a named endpoint: `metadata` URLs, `Location`, bundle `self/next`, `fullUrl`, `_include`, history links, batch `response.location`, export kick-off/status/file URLs, SMART discovery, 503 on disabled, 404 unknown, `X-Tenant-ID` door rules, CORS per endpoint, rate limit per endpoint, hot reload mid-test. |
| Auth integration | in-process JWKS server (existing test helpers in `crates/auth/tests`): two endpoints/two issuers, shared issuer different audiences, write-protect vs full, console principal scopes, admin credential toggle, IdP down. |
| PG integration | migrations (`postgres_integration_manager_tables_and_epochs` — on this branch), PG `EndpointStore` conformance, NOTIFY wake-up, two registries on one DB (cluster coherence ≤ interval), console store last-admin rule, sync store fencing, `StorageCopy` correctness (counts, search works on copy). |
| UI | Rust page tests (role gating per route, CSRF, i18n keys present en + zh-Hant), Playwright ring (`ui-tests.yml`): sign-in flows (local, OIDC against Keycloak container), wizard, config edit with conflict, sync job run page. |
| End-to-end (eval scripts, not CI) | prod_verify 42/42, validator 94/94, test_hts 112/112, test_hfs_server 184/184 on the default endpoint with the flag on; same scripts parametrized with `BASE=/fhir/{endpoint}` on a named endpoint; cluster_tests.py extended with endpoint create on instance A visible on B; sync against a HAPI container. |

---

## 14. Rollout

As §11; each release gated by the regression set above and a pre-deploy
dump (existing `deploy/predeploy-dump.sh`), under `flock .deploy.lock`.
Feature flags: `HFS_MANAGER` (dispatcher + API + console manager pages),
`HFS_CONSOLE_AUTH_MODE` (sign-in), `HFS_SYNC_ENABLED` (workers;
default on when `HFS_MANAGER=on`), `HFS_SYNC_EXTERNAL` (external peers).
Rollback: flags off (instant, restart only); image rollback is safe while
only fork-track tables were added.

Ops docs to add with the packages: `docs/ops/endpoints.md`,
`docs/ops/console-sign-in.md` (supersedes admin-sign-in.md for the console),
`docs/ops/sync.md`, updates to `auth.md`, `cluster.md`,
`database-migrations.md` (hm-0008..0010 rows).

---

## 15. Work breakdown

Package 0 is done on this branch. P1–P6 run in parallel from it; file
ownership is exclusive except for the **shared touch points** listed per
package (one-line additions, merged by the integration line in order
P1 → P2 → P3 → P4 → P5 → P6). Each package works on its own branch
`hm/mgr-p{N}-…` from `hm/design-manager`, rebases on merged predecessors
before integration, and must be green in HealthyMind CI.

### P0 — Foundation (DONE, `hm/design-manager`)

- **Owns**: `crates/endpoints/**` (all modules below), `Cargo.toml`
  (workspace member), `Cargo.lock` (new package entry), fork migrations
  `hm-0008-endpoints`, `hm-0009-console-auth`, `hm-0010-endpoint-sync` in
  `crates/persistence/src/backends/postgres/schema.rs`, this document.
- **Provides**: `EndpointName/EndpointPath` + `RESERVED_SEGMENTS`;
  `EndpointSpec/Endpoint` + sections + `validate()`; `EndpointSnapshot::resolve`
  → `Resolution::{Serve,Unavailable,NotFound}(ResolvedEndpoint)`;
  `EndpointRegistry::{load,snapshot,install,reload_if_stale,subscribe,poke}`,
  `run_watcher`; `EndpointStore` trait + `MemoryEndpointStore` +
  `store::check_write` + `store::conformance::run_all` +
  `ensure_default_endpoint`; `SecretKeyRing/SealedSecret/SecretInput/
  redact_sealed`; console contracts (`ConsoleAuthMode`, `ConsoleRole::data_scopes`,
  `RoleMapping`, `claim_values`, `ConsoleAuthSettings`, `ConsoleUser`,
  `ConsolePrincipal`, `ConsoleSessionResolver`, `ConsoleStore`,
  `CONSOLE_ISSUER`); sync contracts (`SyncJobSpec`+`validate`, `SyncPeer`,
  `ExternalAuth`, `InitialLoad`, `SyncTrigger`, `ConflictPolicy`, `SyncRun`,
  `SyncCursor`, `SyncProgress`, `DuplicateRequest`, `SyncStore`);
  `OutboundPolicy::{parse,from_env,check_url,check_ip}`.
- **Tests**: 37 unit tests (`cargo test -p helios-endpoints`), PG migration
  test `postgres_integration_manager_tables_and_epochs`.
- **CI groups added**: `helios-endpoints` unit group; persistence PG group
  gains the migration test.

### P1 — Endpoint runtime: routing, URLs, per-endpoint config, PG store

- **Scope**: dispatcher (§5.1); `TenantExtractor` prefix + door rules (D-7);
  public URL builder; every URL site audited; CapabilityStatement per
  endpoint; effective FHIR version; per-endpoint validation config +
  terminology client cache; per-endpoint CORS middleware; subscriptions
  `BaseUrlResolver`; audit endpoint extension; metrics label; PG
  `EndpointStore` (`crates/endpoints/src/pg.rs`, feature `postgres`) +
  LISTEN task; `HFS_ENDPOINTS_FILE` bootstrap; `HFS_MANAGER` flag; wiring.
- **Owns**: `crates/rest/src/endpoint/**` (new: `dispatch.rs`, `effective.rs`,
  `cors.rs`, `base_url.rs`), `crates/rest/src/routing/fhir_routes.rs`,
  `middleware/tenant_prefix.rs`, `extractors/tenant.rs`, `extractors/fhir_version.rs`,
  `middleware/resource_type.rs`, `state.rs`, `public_url.rs`, `lib.rs`
  (`build_app`), `handlers/capabilities.rs`, `handlers/{bulk_export,bulk_submit,
  history,search,batch,compartment,ws,subscriptions}.rs` (URL sites only),
  `validation.rs` (effective config entry points), `terminology.rs`,
  `middleware/audit.rs`, `crates/subscriptions/src/notification/builder.rs`,
  `crates/endpoints/src/pg.rs`, `crates/hfs/src/main.rs`,
  `crates/hfs/src/manager/{mod.rs,endpoints.rs}` (creates the
  `ManagerContext` hook module that P3/P6 add files to).
- **Provides**: `Extension<ResolvedEndpoint>` on every FHIR request;
  `rest::endpoint::EffectiveConfig::for_request(&parts) -> {validation,
  terminology, cors, rate_limit, fhir_version}`; `Arc<EndpointRegistry>` and
  `Arc<dyn EndpointStore>` in `AppState` (`state.endpoints()`,
  `state.endpoint_store()`); `BaseUrlResolver` trait (tenant → public base);
  `crates/hfs/src/manager/mod.rs::ManagerContext { pool, registry, store,
  key_ring, outbound, routes: Vec<Router>, tasks }` with `install_*` call
  sites; hook `rest::endpoint::auth_layer(router, AuthHook)` that P2 fills
  (P1 ships a pass-through that reproduces today's global auth behaviour).
- **Consumes**: P0.
- **Acceptance**: router integration suite (§13) for default + named
  endpoint; PG store passes `conformance::run_all`; two registries on one
  PG converge ≤ 1 s; `HFS_MANAGER=off` diff = none (existing rest tests
  unchanged); with flag on and only default endpoint, existing rest
  integration tests pass unchanged; eval regression set identical.
- **CI groups**: rest unit + `endpoint_routing` integration test file;
  endpoints PG group (`postgres_integration_pg_store_*`).
- **Dependencies**: P0 only. Critical path for P2/P4 integration (they code
  against the P0 types and the hooks named above, mocking `ResolvedEndpoint`
  in their tests).

### P2 — Per-endpoint FHIR authentication

- **Scope**: verifier cache; `EffectiveAuth`; always-on `endpoint_auth`
  layer reusing `auth_middleware`/`authz_middleware`/`smart_access` logic;
  scope policy flags; console principal from `ConsoleSessionResolver`
  (consumes the trait; P3 implements); admin credential toggle; per-endpoint
  rate limiter; per-endpoint SMART discovery; admin-API token scope rule;
  `$test-auth` probe (with `OutboundPolicy`).
- **Owns**: `crates/auth/src/verifier_cache.rs` (new), `crates/auth/src/config.rs`
  (`AuthConfig::from_endpoint`), `crates/auth/src/discovery.rs`,
  `crates/rest/src/middleware/{auth.rs,smart_access.rs,client_rate_limit.rs}`,
  `crates/rest/src/endpoint/auth.rs` (fills P1's hook),
  `crates/rest/src/handlers/smart_discovery.rs`.
- **Provides**: `EndpointAuthProbe::test(&EndpointAuth) -> ProbeReport`
  (used by P4's `$test-auth`); `AuthHook` implementation.
- **Consumes**: P0 (`EndpointAuth`, `ConsoleSessionResolver`,
  `ConsoleRole::data_scopes`, `OutboundPolicy`), P1 (`ResolvedEndpoint`,
  hook signature).
- **Acceptance**: auth integration suite (§13); existing `crates/auth/tests`
  and rest auth tests unchanged; Keycloak e2e (`e2e_keycloak.py`) 84/84 on
  default endpoint; new e2e: two endpoints, two realms.
- **CI groups**: auth unit; rest `endpoint_auth` integration file;
  `keycloak-smoke.yml` extended with a named endpoint.
- **Dependencies**: P0; integrates after P1.

### P3 — Console authentication, users and roles

- **Scope**: §7 entirely: console guard for all `/ui`; modes none/oidc/local;
  local accounts (argon2), lockout, password policy; OIDC role mapping on top
  of the existing #1449 flow; console sessions in `login_sessions`; CSRF
  token; `ConsoleSessionResolver` impl; PG `ConsoleStore`
  (`crates/endpoints/src/pg_console.rs`); console settings + users admin API;
  sign-in, account, settings/auth and settings/users pages; break-glass;
  `HTS_CONSOLE_REQUIRE_LOGIN`.
- **Owns**: `crates/admin-auth/**`, `crates/auth/src/session.rs`,
  `crates/ui/src/{login.rs,admin_login.rs,console_users.rs,console_settings.rs,
  account.rs,csrf.rs}`, their templates (`templates/pages/{login,account,
  settings-auth,settings-users}.html`), `crates/rest/src/routing/admin_console.rs`
  + `handlers/admin_console.rs`, `crates/endpoints/src/pg_console.rs`,
  `crates/hfs/src/manager/console_auth.rs`, `crates/hts-ui/src/login.rs`,
  `Cargo.lock` (argon2).
- **Shared touch points**: one `.merge(console::routes())` line in
  `crates/ui/src/lib.rs`; one `install_console_auth(&mut ctx)` call in
  `manager/mod.rs`; locale keys in their own Fluent file
  `locales/*/console-auth.ftl`.
- **Provides**: `Arc<dyn ConsoleSessionResolver>` in `ManagerContext`;
  `ConsoleUser` extractor for UI handlers (`Option<ConsolePrincipal>` +
  `RequireRole(ConsoleRole)`); CSRF meta/header helpers for P5 templates.
- **Acceptance**: unit (hashing, policy, mapping, CSRF, last-admin);
  PG `ConsoleStore` tests; UI route-gating matrix (every `/ui` route ×
  {anonymous, viewer, editor, admin} in each mode); Playwright: local
  sign-in, OIDC sign-in against Keycloak container, sign-out, lockout,
  forced password change; break-glass works in all modes.
- **CI groups**: admin-auth unit; ui `console_auth` tests; PG console store;
  Playwright `ui-tests.yml` new specs.
- **Dependencies**: P0; integrates after P2 (the FHIR console principal
  path is exercised end-to-end only then; before that P3 tests the resolver
  directly).

### P4 — Admin REST API for endpoints

- **Scope**: §8 endpoint routes: CRUD, enable/disable, delete (+ purge job
  through existing tenant purge), history, duplicate kick-off (calls P6's
  `DuplicateService` trait; returns 501 until P6 lands), `$test-auth`
  (P2 probe), stats, endpoint-scoped packages and audit search, secrets
  `$rewrap`; OperationOutcome mapping of `StoreError`/`ValidationErrors`;
  ETag/If-Match; JSON merge patch; secret input handling; audit of admin
  writes; OpenAPI fragment `docs/api/admin-endpoints.yaml`.
- **Owns**: `crates/rest/src/routing/admin_endpoints.rs`,
  `crates/rest/src/handlers/admin_endpoints/**`,
  `crates/rest/src/routing/admin_packages.rs` (endpoint-scoped variant),
  `docs/api/admin-endpoints.yaml`.
- **Shared touch points**: one `.merge(admin_endpoints::routes(..))` in
  `build_app` (P1's file).
- **Provides**: the API P5's pages call (shapes in §8, JSON = P0 types,
  responses = `Endpoint::redacted()`); `trait DuplicateService` (P0-typed
  `DuplicateRequest` → job id) for P6 to implement.
- **Consumes**: P0, P1 (`state.endpoint_store()`, registry reload), P2
  (probe).
- **Acceptance**: API integration tests over memory and PG stores: every
  route, every error status, CAS conflicts, secrets never echoed (scan all
  responses for `hmsec:`), env-managed refusal, audit events written,
  hot-reload visible on the next request.
- **CI groups**: rest `admin_endpoints` integration file (+ PG variant).
- **Dependencies**: P0; P1 for integration (develops against
  `MemoryEndpointStore` and a test `AppState`).

### P5 — Manager console UI and design system

- **Scope**: §10: design tokens + branding assets in `crates/ui-chrome`,
  new layout (topbar, rail, endpoint switcher), Overview, wizard, endpoint
  home, config/security/validation/terminology/subscriptions/bulk/audit/
  history pages, `/ui/e/{ep}/…` re-scoping of the existing data-browser
  pages and legacy redirects, About page with attribution, responsive
  layouts, zh-Hant + en strings, HTS console chrome alignment.
- **Owns**: `crates/ui-chrome/**`, `crates/ui/src/{manager/**,navigation.rs,
  rail_state.rs,i18n.rs}`, `crates/ui/src/lib.rs` (route table; P3/P6 add
  one merge line each), `crates/ui/templates/{layouts,partials}/**`,
  `crates/ui/templates/pages/` except P3's and P6's pages,
  `crates/ui/assets/**` (css, js, brand), `crates/ui/locales/*/main.ftl`,
  `crates/hts-ui/templates/layouts/**`.
- **Provides**: layout blocks/partials and CSS components P3/P6 pages use
  (`page_header`, `form_section`, `secret_field`, `status_pill`,
  `job_progress`, `data_table`); nav registration API
  `manager::nav::register(section, item)`.
- **Consumes**: P4 API (develops against a fake API server in tests and the
  §8 shapes), P3 extractors (until P3 lands, a stub `ConsolePrincipal`
  = admin in `none` mode).
- **Acceptance**: Rust page tests (render every page in en + zh-Hant, no
  missing keys, role-gated actions hidden/disabled), Playwright: overview,
  wizard create → endpoint usable at `/fhir/{path}/metadata`, edit with
  conflict, mobile viewport screenshots (375/768/1280), axe-core a11y check
  (no serious violations), contrast of tokens asserted in a unit test.
- **CI groups**: ui unit/page tests; `ui-tests.yml` Playwright specs;
  ui-chrome tests.
- **Dependencies**: P0; integrates after P3 and P4.

### P6 — Sync and duplicate

- **Scope**: §9: new crate `crates/sync` (`helios-sync`): scheduler, worker
  pool on `cluster_jobs` (`JobKind::EndpointSync`), engines (local reader/
  writer, external HTTP reader/writer with OutboundPolicy + ExternalAuth
  token providers, StorageCopy, BulkExport loader), cursor/boundary logic,
  item state, conflicts, logs; PG `SyncStore`
  (`crates/endpoints/src/pg_sync.rs`); persistence `TenantCopy` capability
  for PG (`crates/persistence/src/backends/postgres/tenant_copy.rs`);
  `DuplicateService`; sync admin API; sync UI pages; notify receiver.
- **Owns**: `crates/sync/**`, `crates/endpoints/src/pg_sync.rs`,
  `crates/persistence/src/backends/postgres/tenant_copy.rs`,
  `crates/persistence/src/core/cluster_job_store.rs` (new `JobKind` variant
  only), `crates/rest/src/routing/admin_sync.rs` + `handlers/admin_sync.rs`,
  `crates/ui/src/sync_pages.rs` + `templates/pages/sync-*.html`,
  `locales/*/sync.ftl`, `crates/hfs/src/manager/sync.rs`.
- **Shared touch points**: one merge line in `build_app`, one in
  `crates/ui/src/lib.rs`, one `install_sync(&mut ctx)` in `manager/mod.rs`,
  workspace member line.
- **Consumes**: P0 (sync contracts, secrets, outbound), P1 (registry,
  storage access per endpoint tenant), P4 (`DuplicateService` trait), P5
  (UI components).
- **Acceptance**: unit (cursor/boundary, conflict decisions, idempotency
  hashing, retry/backoff, rate limiter); PG integration: StorageCopy copies
  counts + search works on the copy + package registry copied; run resumes
  after simulated crash (lease expiry) without duplicates; fencing refuses
  stale worker; local→local continuous sync latency ≤ 2 s; external sync
  against an in-process mock FHIR server (pagination, 429 Retry-After,
  transaction refusal fallback, deletes); SSRF refusal tests; Playwright run
  page.
- **CI groups**: sync unit; sync PG integration; ui sync page tests.
- **Dependencies**: P0; integrates last (needs P1 storage access and P4
  duplicate hook; develops against memory stores and a fake registry).

### Order and parallelism

```
P0 (done) ──┬── P1 runtime ──────────┐
            ├── P2 auth ─────────────┤ integrate in order
            ├── P3 console auth ─────┤ P1 → P2 → P3 → P4 → P5 → P6
            ├── P4 admin API ────────┤ (each rebases on the previous merge,
            ├── P5 UI ───────────────┤  CI green, eval regression set on
            └── P6 sync ─────────────┘  the test stack before prod)
```

All six start immediately from `hm/design-manager`. Highest merge-conflict
risk: `crates/rest/src/lib.rs` (P1 owns; P4/P6 add one line each),
`crates/ui/src/lib.rs` (P5 owns; P3/P6 one line each), `Cargo.lock`
(P3 argon2, P6 new crate — regenerate on rebase, never hand-merge).

---

## 16. Open items (decided defaults, revisit after R3)

- Endpoint aliases (N endpoints : 1 tenant, e.g. a public read-only face) —
  model reserved; needs a "primary endpoint per tenant" for subscription URLs.
- Dedicated database per endpoint (`StorageBinding::Database`) — needs
  multiple storage pools in `AppState`.
- HTS single sign-on with the console OIDC client.
- Id remapping sync (`IdStrategy::Remap` with reference rewriting).
- Per-endpoint `HFS_MAX_BODY_SIZE` / request timeout overrides.
