# Unified server — HFS and HTS as one process, one database, one console

Status: **design, accepted for implementation** (line U0, 2026-11-23).
Branch `hm/design-unify` (from `hm/main` 09eb7ee02 = production `mgr-5f4c6a4f8`).
Package U0 (the shared contracts) is implemented on this branch as the crate
`crates/terminology` (`helios-terminology`, 19 unit tests, clippy clean), plus
two reserved path segments in `helios-endpoints`.

The user's request: *"Merge HFS and HTS thoroughly — backend, database and
especially the web. Today, after signing in to HFS, switching to the HTS
console asks me to sign in again. Merge them and optimise the combined UI/UX
deeply."*

This document turns the two servers into **one product**:

- one process (`hfs`, terminology built in);
- one PostgreSQL database (terminology in its own schema);
- one web console with one sign-in;
- one admin API;
- one configuration surface;
- one `/metrics` and one health endpoint.

Every public URL keeps working, and an external terminology server is still
supported.

Decisions are marked **U-n**. §0 has the summary table and §14 the work
packages.

---

## 0. Key decisions

| # | Decision | Why |
|---|---|---|
| U-1 | **One binary, `hfs`, with HTS linked in** (`helios-hts` as a library behind the hfs feature `terminology`, on by default). `HFS_TERMINOLOGY=embedded\|remote\|off`. The **`hts` binary stays**, unchanged in behaviour, as the terminology-only server (external deployments, upstream compatibility, and the `hts import` / `hts migrate` CLIs used for full SNOMED/LOINC loads). | `helios-hts` is already a library (`server::create_app`, `AppState`, `pub` modules); the hfs binary only has to build its state and mount its routers. Keeping `hts` costs nothing — it is the same crate — and keeps upstream merges clean. |
| U-2 | Default mode: `embedded` on a fresh deployment. **A deployment that sets `HFS_TERMINOLOGY_SERVER` and not `HFS_TERMINOLOGY` stays `remote`.** Production switches by flag, so the binary upgrade and the switch-over are separate steps. | The upgrade changes nothing by itself; rollback is a flag. |
| U-3 | **All terminology access goes through one trait, `TerminologyService`** (U0). It has one required method, `call(TxRequest) -> TxResponse` (HTTP-shaped), plus typed helpers with default bodies. There are three implementations: `LoopbackTerminology` (in-process: `tower::oneshot` into the embedded HTS router — no socket, same handlers, same caches, byte-identical answers), `RemoteTerminology` (today's HTTP), and `DisabledTerminology`. The six ad-hoc reqwest clients in rest/ui/fhirpath (§2) are replaced. | Phase 1 then has zero semantic drift between embedded and remote mode, and the HTTP-level test suites stay valid for both. Typed fast paths (direct calls into HTS operations, no JSON) can come later without touching callers. |
| U-4 | The caller's identity is a **`TxPrincipal` request extension** (`System`, `Console{user, role}` or `Anonymous`), never a header. The embedded write guard trusts `System`. Remote mode turns `System` into `HFS_TERMINOLOGY_SERVER_TOKEN`. | Extensions cannot be forged from the network. In embedded mode the package manager needs no token. |
| U-5 | **Public URLs stay byte-identical.** `/tx/**` is the terminology FHIR base, now served by hfs (nested router at `/tx`, which replaces nginx's `/tx/ → hts:8090/` and its `sub_filter` URL rewriting). `/fhir/**`, `/auth/**` and SMART discovery are unchanged. **`/ui/hts/**` redirects with 301/308 to `/ui/terminology/**`**. `/tx/admin/**` redirects to `/admin/terminology/**` for one release. | No client, IG publisher or integration needs to change. |
| U-6 | **One database.** The terminology store moves into **schema `terminology` of the `hfs` database**. That covers the HTS tables *and* the helios-persistence resource store HTS embeds for raw CodeSystem/ValueSet/ConceptMap JSON. The terminology pools connect with `search_path=terminology,public`, so HTS's unqualified SQL needs no change. FHIR data, console, endpoints, sync, sessions and audit stay in `public`. | Table names only collide where HTS embeds persistence (`resources`, `search_index`, `schema_version`, …). A schema separates them with no SQL rewrite. Moving the terminology resources into an HFS tenant would expose them on `/fhir` and drag them into reindex, export and purge. |
| U-7 | **The process merge and the data move are separate stages.** Stage 1 runs embedded terminology against the *old* `hts` database (`HFS_TERMINOLOGY_DATABASE_URL`). Stage 2 moves 9.5 GB into the schema with `hfs-unify-db` (§5.5) and then drops the URL. | One sign-in and one console ship without touching terminology data. The risky copy gets its own window and its own rollback. |
| U-8 | **One sign-in.** Terminology pages are pages of the HFS console: the same `hfs_console` session (cookie `Path=/`), the same guard and modes (`none`/`local`/`oidc`), the same CSRF. A role table maps terminology actions to console roles (browse/workbench → viewer; edit resource → editor; import/maintain/operations → admin) (U0 `access`). `HTS_ADMIN_PASSWORD` is folded into the break-glass account (§7.3). The `hts_admin` cookie and the HTS login page are gone. | That is the user's complaint, solved structurally rather than bridged. |
| U-9 | **One console shell.** A single sidebar has sections Overview · FHIR endpoints · **Terminology** · (Authorization servers, when on) · Server · Settings. The product switcher is removed. hts-ui stays a crate of terminology pages but renders inside the shared shell, which moves to `ui-chrome`. | One information architecture. Terminology is a section, not a different product. |
| U-10 | **Persistent-shell navigation.** Sidebar and topbar DOM never reload. Navigation is `hx-boost` with the `#main` region swapped and history pushed, plus out-of-band nav updates only when the endpoint/role context changes. There is a delayed progress bar, skeletons for slow pages, fingerprinted immutable assets, view transitions where supported, and full reload only for sign-in/out, language and build change (§9.4). | Removes the full-page flash the user notices on every click, without an SPA. Works without JS (plain links). |
| U-11 | **Bulk terminology work cannot starve FHIR requests.** Imports, bootstrap sync and closure rebuilds run on a **dedicated `terminology-import` tokio runtime** (default ¼ of the cores, at most 4 workers, niced threads) with its own small PG pool. Terminology calls from FHIR request paths take permits from a semaphore. Heavy full-release loads stay on the `hts import` CLI as a one-off container (§8). | Tokio's shared blocking pool and one PG pool would otherwise let a SNOMED import take CPU and connections from `/fhir`. |
| U-12 | **One config surface:** `HFS_TERMINOLOGY_*`. Every `HTS_*` variable is classified as *renamed*, *merged into* an HFS variable, or *ignored*. Legacy values are adopted when the new name is unset, and each is logged once and listed on the console's About page. Aliases are removed two releases later (U0 `config`). | Existing compose files keep working, and operators see exactly what to rename. |
| U-13 | **One observability surface.** One `/metrics` (`service="hfs"`, terminology routes labelled `/tx/...`, new `terminology_*` series, pool gauges `pool="terminology"` and `"terminology_resources"`). `/health` gains `components.terminology`. `/_readiness` reports terminology as degraded, never unready, unless `HFS_READINESS_REQUIRE_TERMINOLOGY=true`. One audit trail (the HFS sink; terminology events carry `component=terminology`). | One scrape target, one dashboard row, one alert per symptom. |
| U-14 | **Fork-migration numbers: the unified server owns `hm-0011`…`hm-0019`; the built-in authorization server (line IdP) owns `hm-0020`…`hm-0029`.** U0 reserves the root segments `tx`, `terminology` and `idp` against endpoint names. | Two lines work in parallel on `hm_schema_migrations` without renumbering at merge. |
| U-15 | **SQLite equivalence:** one process and one config, but terminology keeps its own file `terminology.db` next to `fhir.db` (derived automatically). | SQLite has no schemas, and HTS's SQLite backend has its own pool and FTS5 tables. Two files in one data directory is the SQLite form of "one database". |

---

## 1. Goals and non-goals

### Goals

1. One process serves FHIR (`/fhir`), terminology (`/tx`), the console (`/ui`)
   and the admin API (`/admin`). One container, one image, one port.
2. One PostgreSQL database holds everything (the Keycloak DB stays separate
   while Keycloak exists).
3. One console with one sign-in. Terminology pages follow the console auth
   mode and roles. The double sign-in disappears.
4. Navigating the console never reloads the shell. Slow actions show
   progress. Filters and scroll survive navigation.
5. Validation, `:in` / `:not-in` / `:above` / `:below` search, SQL-on-FHIR
   terminology, FHIRPath `memberOf()`/`subsumes()` and IG package
   terminology imports call terminology in-process by default, with
   immediate cache invalidation after terminology writes.
6. An external terminology server remains supported (`HFS_TERMINOLOGY=remote`),
   per deployment and per endpoint (`TerminologyBinding.server_url`).
7. Every public URL and every SMART/auth behaviour stays as it is.
8. Staged rollout. Each stage can be rolled back by a flag or an image swap,
   and no stage needs a data restore for rollback.

### Non-goals

- Terminology multi-tenancy (per-endpoint terminology *content*). All
  endpoints share the server's terminology store, as today.
- Terminology operations on FHIR endpoint bases (`/fhir/{ep}/ValueSet/$expand`
  delegating to the terminology service). This is an open item (§16): HFS
  already stores IG ValueSets in its own store, and the semantics need a
  separate decision.
- Rewriting HTS internals (backends, importers, caches). They are embedded
  as they are.
- Retiring Keycloak (that is line IdP; §15 lists the interplay).
- The GCE VM deployment (paused; §13.6).

---

## 2. Baseline (production `mgr-5f4c6a4f8`, `hm/main` 09eb7ee02)

| Area | Today |
|---|---|
| Processes | `hfs` (8080) and `hts` (8090), two images built from the same workspace (`deploy/build-images.sh`; the hts image also carries `terminology-data/`, ~148 MiB). |
| nginx | `/fhir/ → hfs:/`, `/tx/ → hts:/` with `sub_filter` rewriting `https://host/` → `https://host/tx/` in JSON bodies, `/ui/hts → hts`, `/ui → hfs`, `/auth/ → keycloak`, `/metrics`, `/fhir/metrics` and `/tx/metrics` restricted to internal addresses. |
| Databases | One PG 16.15 cluster: `hfs` (role `hfs`, ~0.3 GB + audit), `hts` (role `hts`, ~9.5 GB: SNOMED CT, LOINC 2.80/2.83, NDC 1.5M, ICD-10-CM, NCIt, TW Core …; 3.8M concepts; HTS tables **plus** a full helios-persistence store for raw terminology resources), `keycloak`. WAL archiving on. |
| HFS → HTS calls | 6 reqwest clients, no shared trait: search `TerminologyServiceClient`/`SearchTerminology` (`rest/src/terminology.rs`, TTL cache 60 s / 512 entries); SoF dependencies (`rest/src/handlers/sof/terminology.rs`); validator `RemoteTerminologyProvider` (`rest/src/validation.rs`, 5 DashMaps without size bounds, TTL 300/60 s, batches of 25 × concurrency 2); FHIRPath `TerminologyClient` (own tokio runtime, wired via process env `FHIRPATH_TERMINOLOGY_SERVER` set with `unsafe set_var` *after* the runtime started); package manager `HtsClient` (`rest/src/packages/hts.rs`, bearer `HFS_TERMINOLOGY_SERVER_TOKEN`, 900 s); UI editor `$expand` proxy and reference resolver (`ui/src/editor.rs`, `ui/src/lib.rs`); console health probe (`ui/src/manager/backend.rs`). |
| HTS console | `/ui/hts/**` (hts-ui, Askama + htmx, same `app.css` through a symlink). It calls HTS **over loopback HTTP** (`UpstreamClient`, 6.2k lines, `http://127.0.0.1:8090`). Login: its own `hts_admin` password session (`Path=/ui/hts`, admin-auth gate, PG `admin_sessions` scope `HTS`). `HTS_CONSOLE_REQUIRE_LOGIN=true` gates every page. It does **not** read HFS console sessions. |
| HFS console | `/ui/**`, manager on, console mode `local` (`hfs_console` cookie `Path=/`, `login_sessions`, CSRF token `hfs_csrf`). Plain full-page navigation; `hx-boost` only on two filter blocks. Assets `Cache-Control: no-cache` (every page revalidates ~50 files). |
| HTS writes | `HTS_REQUIRE_AUTH_FOR_WRITES` gate: HTS admin password (Basic/Bearer) or a JWKS-verified token (`HTS_AUTH_*`). Reads are public. |
| Audit | HFS sink in DB `hfs`. HTS writes AuditEvents into DB `hfs` too (`HTS_AUDIT_DATABASE_URL`, tenant `default`), plus admin `audit.ndjson` in `HTS_ADMIN_DATA_DIR`. |
| Metrics | Two targets, `service="hfs"` / `service="hts"`. Alerts `HfsDown` and `HtsDown`. |
| Cluster | HTS caches coherent through the `terminology_epoch` poll (`HTS_CACHE_SYNC_INTERVAL_MS`). HTS admin and import jobs run on one instance only (state in `HTS_ADMIN_DATA_DIR`). |

---

## 3. Architecture

### 3.1 Process

```
 browser / SMART app / IG tooling / integrations              operator
   https://host/fhir/{ep}/…   https://host/tx/…                https://host/ui/…
            │                        │                               │
   nginx   /fhir/ → hfs:/   /tx/ → hfs:/tx/  (no sub_filter)   /ui → hfs:/ui   /auth → keycloak
            └────────────┬───────────┴───────────────────────────────┘
 ┌───────────────────────┴──────────────────── hfs process ─────────────────────────────┐
 │ axum Router                                                                           │
 │  ├─ /ui/**                 ONE console (helios-ui + helios-hts-ui pages)              │
 │  │                         console guard (admin-auth console) → role table (U0 access)│
 │  ├─ /ui/hts/**             301/308 → /ui/terminology/** (U0 routes)                   │
 │  ├─ /admin/**              admin API: endpoints, sync, packages, console,             │
 │  │                         /admin/terminology/** (was HTS /admin)                     │
 │  ├─ /tx/**                 terminology FHIR API = embedded HTS tx router              │
 │  │                         (write guard: console role / admin / SMART scope / System) │
 │  ├─ /idp/{name}/**         reserved: built-in authorization servers (line IdP)        │
 │  ├─ /metrics /health /_readiness   one surface                                        │
 │  └─ fallback → EndpointDispatcher → per-endpoint FHIR stack (unchanged)               │
 │                                                                                       │
 │  Arc<dyn TerminologyService> ──► LoopbackTerminology(tx router + admin router)        │
 │     used by: validator provider · :in search · SoF · FHIRPath resolver ·              │
 │              package manager · console pages · health                                 │
 │  per-endpoint binding with server_url ──► RemoteTerminology (cached per URL)          │
 │                                                                                       │
 │  runtimes: main (HTTP, FHIR)   │ terminology-import (imports, bootstrap, closure)     │
 │            fhirpath bridge     │ validator large-stack pool (debug)                   │
 └───────────────────────────────┬───────────────────────────────────────────────────────┘
                                 ▼
  PostgreSQL db hfs:  schema public      — FHIR resources, search, audit, endpoints,
                                            console, sessions, sync, cluster_jobs …
                      schema terminology — HTS tables + HTS's embedded resource store
                      (schema idp        — reserved suggestion for line IdP)
```

### 3.2 Route namespace (U0 `routes`)

| Path | Owner | Notes |
|---|---|---|
| `/fhir/{ep}/…` (inside: `/{ep}/…`) | FHIR endpoints | unchanged |
| `/tx`, `/tx/**` | embedded HTS tx router | `TX_PREFIX`. Inside hfs the router is nested at `/tx`; HTS emits absolute URLs (paging links, `fullUrl`) from `HFS_TERMINOLOGY_PUBLIC_URL` (default `{origin of HFS_BASE_URL}/tx`), which makes nginx's `sub_filter` unnecessary (U2 adds `public_base` to HTS's link builder; until then the nested router gets `X-Forwarded-Prefix: /tx` and the same rewrite in-process). |
| `/tx/health`, `/tx/metadata` | HTS | kept for external checkers. `/tx/metrics` → the one `/metrics`. |
| `/tx/admin/**` | redirect | 308 → `/admin/terminology/**` for one release, then 404. |
| `/admin/terminology/**` | HTS admin router | behind the admin API guard (console session + CSRF, break-glass credential, system-scope token). |
| `/ui/terminology/**` | hts-ui pages | `CONSOLE_PREFIX`. The old setup page `/ui/terminology` becomes the terminology home; in `off` mode it explains how to enable terminology. |
| `/ui/hts/**` | redirect | `legacy_console_redirect` (inventory page `/ui/hts/terminology` → `/ui/terminology/manage`, `/ui/hts/login` → `/ui/login`, `/ui/hts/api/admin/**` → `/admin/terminology/**`). |
| `/idp/**` | line IdP | reserved (`IDP_PREFIX`, `RESERVED_SEGMENTS` gains `idp`, `terminology`). |
| `/metrics`, `/health`, `/_liveness`, `/_readiness` | hfs | one surface (§10). |

Root `/` of the hts router (redirect to `/ui/hts`) and its own `/metrics`
are **not** mounted in embedded mode. U2 splits `create_app` into
`tx_router()`, `admin_router()` and `ui_state()` builders, and the
standalone binary keeps composing them as today.

---

## 4. Terminology in-process

### 4.1 The contract (U0, `crates/terminology/src/service.rs`)

```rust
#[async_trait]
pub trait TerminologyService: Send + Sync + 'static {
    fn kind(&self) -> ServiceKind;                                   // Embedded | Remote | Disabled
    async fn call(&self, request: TxRequest) -> Result<TxResponse, TxError>;
    fn changes(&self) -> Option<watch::Receiver<u64>> { None }      // terminology_epoch
    // typed helpers with default bodies built on `call`:
    async fn health(&self) -> TerminologyHealth;
    async fn expand(&self, &ExpandParams) -> Result<Value, TxError>;
    async fn validate_code(&self, &ValidateCodeParams) -> Result<ValidateCodeOutcome, TxError>;
    async fn lookup / subsumes / translate / batch / find_by_url;
    async fn import_bundle(&self, &Value) -> Result<ImportReport, TxError>;   // System principal
    async fn delete(&self, resource_type, id) -> Result<(), TxError>;          // System principal
}
```

`TxRequest { method, path (relative to the terminology base), query, body,
principal, timeout, accept_language }`. `TxError::{Disabled, Unavailable,
Timeout, Status{status, message}, Decode}` with `is_transient()`, which
callers map onto their existing policies (`HFS_SEARCH_TERMINOLOGY_FAIL`,
`HFS_VALIDATION_TERMINOLOGY_FAIL`, package import retry/split).

### 4.2 Implementations

| Impl | Where | Behaviour |
|---|---|---|
| `LoopbackTerminology<S: tower::Service>` | U0 (generic) | In production `S` = `tx_router.merge(admin_router)` of the embedded HTS. `oneshot` per call, principal as an extension, body cap 256 MiB, deadline via `tokio::time::timeout`. Same handlers, so the same caches (`ExpandCache`, `ValidateCodeHandlerCache`, backend response caches) serve the console, validator, `/tx` clients and search alike. |
| `RemoteTerminology` | U0 + `HttpTransport` impl in rest (U2) | URL join, token only for `System`, `http(s)` only, SSRF via `OutboundPolicy` for console-entered URLs, 2 s connect timeout (reqwest impl). |
| `DisabledTerminology` | U0 | every call `Disabled`; health Down. |
| typed fast paths (phase 2) | U2, later | an `EmbeddedTerminology` wrapping `LoopbackTerminology` that overrides `validate_code`/`expand` with direct calls into `helios_hts::operations::*` (no JSON round trip). Only if U2's profiling shows serialisation matters. |

### 4.3 Callers migrated (U2)

| Caller today | Becomes |
|---|---|
| `rest/src/terminology.rs` `TerminologyServiceClient` + `SearchTerminology` | `SearchTerminology` over `Arc<dyn TerminologyService>`; keeps paging (`count`/`offset`, max codes), keeps its cache, which is cleared on `changes()`. |
| `rest/src/handlers/sof/terminology.rs` | same service. |
| `rest/src/validation.rs` `RemoteTerminologyProvider` | `ServiceTerminologyProvider` implementing the validator's `TerminologyProvider` over the service. Batches stay (`POST /` Bundle of reads). The 5 DashMaps get size bounds (LRU 50k) and clear on `changes()`. `HFS_VALIDATION_TERMINOLOGY=remote` keeps its meaning ("core first, then the terminology service"), with or without a remote server. |
| fhirpath `TerminologyClient` + env `FHIRPATH_TERMINOLOGY_SERVER` | `EvaluationContext.terminology: Option<Arc<dyn TerminologyService>>`, set by rest/sof/cds callers. The `unsafe set_var` and the process-global URL are removed, and the env var is honoured only by the standalone CLIs. The private `fhirpath-terminology` runtime stays for the sync bridge. |
| `rest/src/packages/hts.rs` `HtsClient` | `import_bundle` / `find_by_url` / `delete` on the service (System principal; no token in embedded mode). Retry/split logic unchanged. |
| `ui/src/editor.rs` expand proxy, `resolve_terminology_reference` | service from `WebState`. |
| `ui/src/manager/backend.rs` `probe_terminology` | `service.health()`. |
| hts-ui `UpstreamClient` (loopback reqwest) | Its transport becomes a trait: a reqwest impl (standalone `hts`) and a `TerminologyService` impl (embedded; admin calls go to `admin/...` paths on the same loopback with the console principal). The 6.2k lines of parsing stay. |

### 4.4 Effects

- **Validation latency.** Today each check batch crosses the Docker
  network, HTTP/1.1 and JSON. In-process removes the socket, the HTTP
  parse and the connection pool, but not JSON. Expect a small win per call
  (sub-millisecond to ~2 ms) and a visible win on transaction Bundles with
  many codings (20-entry transaction: 1.25–1.7× the cost of
  validation-off in log mode today). **Acceptance:** with validation on,
  p50/p95 of `validator-bench` sets set-new/set-9 and the 20-entry
  transaction are ≤ remote mode on the same box, and identical verdicts
  (`fhir-test-cases` and validator 94/94 unchanged).
- **`:in` expansion:** the same call path, without socket overhead. The
  search cache keeps its TTL as a backstop but clears immediately on
  terminology change (embedded), which fixes stale `:in` results after an
  import.
- **Package installs:** IG terminology goes into the store directly, with
  no token. The 900 s client timeout becomes the service deadline. A
  terminology import inside a package install runs its heavy part on the
  import runtime (§8).
- **Caches:** one copy per process instead of an HFS copy plus an HTS
  copy. HTS's caches are the authoritative layer. HFS-side caches are
  short-circuits that subscribe to `changes()`.

### 4.5 Cluster cache invalidation

HTS's `terminology_epoch` poll (`spawn_cache_sync`) already clears HTS
caches on every instance. U2 publishes the epoch on a `watch` channel, and
`TerminologyService::changes()` exposes it, so validator, search and SoF
caches on every instance clear within `HFS_TERMINOLOGY_CACHE_SYNC_INTERVAL_MS`
(1 s). The writing instance clears its own caches immediately. Remote mode
keeps today's TTLs. (Optional later: `NOTIFY terminology_epoch` from the
existing trigger to wake pollers early, the same pattern as `hfs_config`.)

---

## 5. One database

### 5.1 Layout

| Schema | Contents | Owner |
|---|---|---|
| `public` | everything HFS has today (resources, search, history, audit, bulk, subscriptions, cluster_jobs, `hfs_endpoints*`, `hfs_console_*`, `hfs_sync_*`, `login_sessions`, `admin_sessions`, …) plus `hfs_terminology_state` (hm-0011) | role `hfs` |
| `terminology` | HTS tables (`code_systems`, `concepts`, `concept_*`, `value_sets`, `value_set_*`, `concept_maps`, `concept_map_elements`, `refset_members`, `snomed_*`, `bootstrap_imports`, `terminology_epoch`, `hts_schema_meta`) **and** HTS's embedded helios-persistence store (`resources`, `resource_history`, `search_index`, `schema_version`, `hm_schema_migrations`, …) | role `hfs` (default) or a dedicated role via `HFS_TERMINOLOGY_DATABASE_URL` |
| `idp` | suggested for line IdP (their decision) | |

Extensions (`pg_trgm`, `btree_gin`, `pg_stat_statements`) stay in `public`
(owned by `postgres`). The terminology pools reach them through the second
`search_path` entry.

Pools in one process (PG `max_connections` 200):

| Pool | Size | Notes |
|---|---|---|
| HFS primary | 64 | unchanged |
| HFS audit | as today | |
| terminology (HTS backend) | 16 | `HFS_TERMINOLOGY_PG_POOL_SIZE` |
| terminology resources (HTS embedded store) | 4 | was a full default pool; it only serves CRUD of raw resources |
| terminology import | 4 | new, used only by the import runtime (§8) |
| sessions/endpoint/console stores | share HFS primary | |

The total drops by ~8 against today's two processes. Keycloak's 10 is
unchanged.

### 5.2 Connection string

`schema::with_search_path(url, schema)` adds
`options=-c search_path=terminology,public` (URI or key/value form, which
tokio-postgres both accept). It refuses URLs that already set `options`
(the operator must then set `HFS_TERMINOLOGY_DATABASE_URL`). Schema names
are validated (`^[a-z_][a-z0-9_]{0,62}$`, not `public`/`pg_*`).

### 5.3 The isolation hazard (must fix before Stage 2)

With `search_path=terminology,public`, any **unqualified lookup of an
object that does not exist in `terminology` falls through to `public`**.
The obvious failure: a fresh terminology schema, the embedded persistence
store runs `to_regclass('hm_schema_migrations')`, finds HFS's table, and
skips creating its own fork track. Known sites are listed in
`schema::SCHEMA_SENSITIVE_LOOKUPS`:

- persistence `schema.rs`: `to_regclass('search_parameter_epochs' | 'idx_resources_reindex_id' | 'hm_schema_migrations' | 'schema_version')`, plus `information_schema.columns/key_column_usage` queries without `table_schema`;
- HTS `schema.rs`: 4 `information_schema` / `pg_class` guards.

U1 qualifies each with `current_schema()` (`to_regclass(current_schema() || '.x')`,
`AND table_schema = current_schema()`, `n.nspname = current_schema()`) and
adds `postgres_integration_terminology_schema_isolation`. That test builds
the full HFS schema in `public` first, then initialises both terminology
stores with the derived URL, and asserts that (a) every object in
`TERMINOLOGY_TABLES ∪ EMBEDDED_STORE_TABLES` exists in `terminology`,
(b) nothing was created in or altered on `public`, and (c) dropping one
terminology table and restarting recreates it in `terminology`. The test
guards the whole class, not just the known sites.

Also checked:

- The persistence migration advisory lock is per database, so HFS and the
  embedded store serialise their startup migrations. That is harmless
  (seconds).
- NOTIFY channels (`hfs_config`, `hfs_sync_wake`) come from triggers on
  `public` tables only. The embedded store's triggers (`resource_change_feed`)
  fire on its own tables, and its feed consumers are not started in HTS.
- `CREATE EXTENSION IF NOT EXISTS` is a no-op when the extension exists.
- The advisory lock keys `HTS_SCHM`/`HTS_BOOT` stay unique.

### 5.4 Migrations — how the tracks coexist

| Track | Where | Rule |
|---|---|---|
| upstream ladder `schema_version` (55) | `public` (HFS) and `terminology` (embedded store, its own row) | unchanged; each store migrates its own schema |
| fork `hm_schema_migrations` | `public` and `terminology` (separate tables, after U1's qualification) | **`hm-0011`…`hm-0019` = unified server; `hm-0020`…`hm-0029` = line IdP** (U0 `schema::UNIFIED_MIGRATION_BLOCK`, `IDP_MIGRATION_BLOCK`) |
| HTS idempotent DDL + fingerprint (`hts_schema_meta`) | `terminology` | unchanged |

Unified fork migrations (in `public`):

- `hm-0011-terminology-schema`: `CREATE SCHEMA IF NOT EXISTS terminology`,
  table `hfs_terminology_state` (one row: store location, `moved_from`,
  `moved_at`, source fingerprint). The SQL is in U0 (`schema::HM_0011_SQL`).
  It requires role `hfs` to hold `CREATE` on database `hfs`. It already does,
  as the database owner (`pg-nonsuper-hfs.sh`), but the migration checks
  `has_database_privilege` first and fails with a clear message instead of a
  half-started server.
- `hm-0012-console-terminology-bindings`: `hfs_console_role_bindings` gains
  scope `terminology` (a user can be terminology admin without being FHIR
  admin) (§7.2).
- `hm-0013`…`hm-0019`: reserved.

They run only when terminology is embedded (`HFS_TERMINOLOGY=embedded`), so
an upgrade with the flag off creates nothing.

### 5.5 Data move: `hfs-unify-db` (U1)

The plan for the 9.5 GB `hts` database. It is idempotent and resumable,
with a dry run, and lives at `hfs-eval/deploy/unify-db.sh` with a matching
section in `docs/ops/unified-server.md`.

```
preflight  free disk ≥ 2.2 × size(hts) (staging copy) or ≥ 1.2 × (in-place variant);
           hfs-pg healthy; no `terminology` tables in hfs yet (or --replace);
           running image = Stage-1 image; WAL archive disk ≥ size(hts)
freeze     terminology writes off: HFS_TERMINOLOGY_ADMIN=false + HFS_TERMINOLOGY_READONLY=true
           (write guard answers 503 "terminology maintenance") — FHIR untouched, terminology
           reads keep being served from the old db
snapshot   CREATE DATABASE hts_stage TEMPLATE hts   (needs no other sessions on hts:
           hfs closes its terminology pools for the copy — ~1–4 min, reads 503 in that window,
           validation fail=open in log mode, :in search per HFS_SEARCH_TERMINOLOGY_FAIL)
           → reopen pools on hts (reads back)
rename     in hts_stage: ALTER SCHEMA public RENAME TO terminology
copy       pg_dump -Fd -j 8 -n terminology hts_stage | pg_restore -j 8 -d hfs --no-owner --role=hfs
           (directory format to a scratch dir when disk allows, else a single-stream pipe)
verify     per table row counts (TERMINOLOGY_TABLES ∪ EMBEDDED_STORE_TABLES) = hts_stage;
           hts_schema_meta fingerprint equal; ANALYZE terminology.*;
           smoke: $lookup SNOMED, $expand LOINC, $validate-code ICD-10-CM through a
           temporary second pool on the new schema
record     hfs_terminology_state(store='schema:terminology', moved_from='database:hts', fingerprint)
switch     drop HFS_TERMINOLOGY_DATABASE_URL from compose → restart hfs (~10 s)
           → unfreeze writes
cleanup    (after the observation window) DROP DATABASE hts_stage; later DROP DATABASE hts
```

**Downtime estimate.** FHIR has none apart from the one restart (~10 s).
Terminology reads are unavailable only during the template copy. On the
production volume that is ~1–4 min, from a measured 9.5 GB file copy at
the box's disk speed (U1 measures it on a restored copy first). Terminology
writes are frozen for the whole move. Restore timing from
`backup-restore.md` (HTS 4.6 GB: `pg_restore -j 8` 62 s idle / 1,459 s
busy) puts a 9.5 GB copy at roughly **3–30 min** depending on load. The GIN
trigram index on `concepts.display` dominates. **Run it at night, freeze
window ≤ 45 min.**

**In-place variant** (when disk is too small for the staging copy). Run
the rename on `hts` itself with hfs's terminology pools closed, and stream
the dump. Terminology reads are then down for the whole copy. The script
picks this variant only with `--in-place` and an explicit warning.

**Idempotence and resume.** Each step records progress in
`hfs_terminology_state.note`. Re-running skips done steps. `verify` failing
leaves the switch undone. `--replace` drops `terminology.*` and restarts at
`copy`.

**WAL.** The restore writes ~10 GB of WAL into `backups/wal-archive` (the
archive is on). Take a fresh base backup right after
(`backup-restore.md`), then prune archive segments older than it.

**Rollback.** Before cleanup the old `hts` database is untouched. Set
`HFS_TERMINOLOGY_DATABASE_URL` back to it and restart, or drop to Stage 1
or Stage 0 images. Terminology writes made after the switch exist only in
the new schema. The script's `export-delta` lists CodeSystem/ValueSet/
ConceptMap resources with `meta.lastUpdated` after `moved_at`, so they can
be re-imported if the rollback has to stick. No FHIR data is involved.

**Backups after the move.** `predeploy-dump.sh` dumps `hfs` (now ~10 GB,
~5 min) instead of `hfs` + `hts`. It adds `--exclude-schema=terminology`
for the frequent dumps and `-n terminology` for a weekly one, since
terminology changes rarely and is re-importable.

### 5.6 SQLite

`schema::sqlite_terminology_url`: `/data/fhir.db` → `/data/terminology.db`
(in-memory FHIR DB → in-memory terminology). One process, one data dir,
two files. `hts migrate` (SQLite → PG) keeps working. Moving a SQLite
deployment to PG then moves both files' content into the one PG database
(`hts migrate --to <hfs url with search_path>`).

---

## 6. One configuration surface

U0 `config`: `EnvView::from_vars(std::env::vars())` folds legacy names in,
and `TerminologySettings::resolve(&view, embedded_built, cores)` produces
the typed settings. hfs logs every `Deprecation::message()` once at WARN.
Messages never contain values (test). The About page lists them.

| Class | Variables (complete table in `HTS_ENV_ALIASES`) |
|---|---|
| New | `HFS_TERMINOLOGY` (embedded/remote/off), `HFS_TERMINOLOGY_SCHEMA`, `HFS_TERMINOLOGY_IMPORT_THREADS`, `HFS_TERMINOLOGY_IMPORT_BLOCKING_THREADS`, `HFS_TERMINOLOGY_OPERATION_PERMITS`, `HFS_TERMINOLOGY_READONLY`, `HFS_READINESS_REQUIRE_TERMINOLOGY` |
| Kept | `HFS_TERMINOLOGY_SERVER` (remote base), `HFS_TERMINOLOGY_SERVER_TOKEN(_FILE)` (remote only), `HFS_TERMINOLOGY_TIMEOUT_SECS`, `HFS_TERMINOLOGY_PUBLIC_URL` (now also the `/tx` base HTS prints) |
| Renamed `HTS_X` → `HFS_TERMINOLOGY_X` | `DATABASE_URL`, `PG_POOL_SIZE`, `PG_POOL_WAIT_TIMEOUT_SECS`, `PG_CONNECT_TIMEOUT_SECS`, `PG_STATEMENT_TIMEOUT_MS`, `MAX_EXPANSION_SIZE`, `AUTO_SUPPLEMENTS`, `BOOTSTRAP_DIR`, `BOOTSTRAP_BATCH_SIZE`, `CACHE_SYNC_INTERVAL_MS`, `IMPORT_LANGUAGES`, `LOINC_*`, `IMPORT_DIR`, `PACKAGE_REGISTRY`, `UPLOAD_CHUNK_SIZE`, `MAX_UPLOAD_SIZE`, `DISK_HEADROOM`, `REQUIRE_AUTH_FOR_WRITES`, `MAX_BODY_SIZE`, `REQUEST_TIMEOUT`, `AUTH_{ISSUER,JWKS_URL,AUDIENCE,ALGORITHMS,CLOCK_SKEW_SECONDS}` (default: inherit `HFS_AUTH_*`); `HTS_ADMIN_UI` → `HFS_TERMINOLOGY_ADMIN`; `HTS_ADMIN_DATA_DIR` → `HFS_TERMINOLOGY_DATA_DIR`; `HTS_PUBLIC_BASE_URL` → `HFS_TERMINOLOGY_PUBLIC_URL` |
| Merged into HFS | `HTS_LOG_LEVEL`, `HTS_SHUTDOWN_TIMEOUT`, `HTS_ENABLE_CORS`, `HTS_CORS_ORIGINS`, `HTS_UI_ENABLED`, `HTS_STORAGE_BACKEND`, `HTS_ADMIN_{PASSWORD,PASSWORD_FILE,USER,SESSION_IDLE_MINUTES,TRUSTED_PROXIES,ALLOWED_ORIGINS,COOKIE_SECURE}` |
| Ignored | `HTS_SERVER_PORT/HOST`, `HTS_UI_UPSTREAM_URL`, `HTS_UI_HFS_CONSOLE_URL`, `HTS_CONSOLE_REQUIRE_LOGIN` (console mode governs), `HTS_AUDIT_*` (one audit trail) |

Merge rule: the legacy value is adopted when the HFS name is unset. When
both are set and differ, the HFS name wins and a *conflict* is reported.
Fail fast on `HFS_TERMINOLOGY=remote` without a server, `embedded` in a
build without the feature, an invalid mode, or an invalid schema.

Endpoint configuration: `TerminologyBinding` keeps `server_url` (remote per
endpoint). `null` now means *the server's terminology service* (embedded or
remote).

---

## 7. Authentication and authorization

### 7.1 One sign-in

hts-ui pages are mounted inside helios-ui's router (`/ui/terminology`), so
`login::require_session` → `console::guard::gate` applies to them unchanged.
Results:

- the same `hfs_console` cookie;
- `SignIn` → `/ui/login?next=/ui/terminology/...`, or `HX-Redirect` for htmx
  requests;
- CSRF via `X-CSRF-Token` (`csrf.js` is already loaded by the shared shell);
- console mode `none` → `anonymousRole`.

hts-ui's own `login.rs`, its `effective_gate`, the `hts_admin` cookie and
`/ui/hts/login` are removed from the embedded path. They stay only in the
standalone `hts` binary. On the first visit a stale `hts_admin` cookie is
expired (`Path=/ui/hts`).

### 7.2 Roles (U0 `access`, hm-0012)

| Action | Pages / API | Min role |
|---|---|---|
| Browse | terminology home, code systems, value sets, concept maps, concepts, capability | viewer |
| Workbench | `$lookup` `$validate-code` `$subsumes` `$expand` `$translate` forms | viewer |
| EditResource | create/update/delete one CodeSystem/ValueSet/ConceptMap from the console | editor |
| Import | upload, import dir, registry install, jobs (write) | admin |
| Maintain | delete version, set current, compact | admin |
| ViewOperations | job list with requester IPs, terminology audit, config | admin |

A console role binding with scope `terminology` (hm-0012) can raise one
user to terminology admin without FHIR admin. The effective role is
max(global, terminology binding). Nav items the role cannot use are hidden;
direct URLs answer the console's 403 page.

### 7.3 Break-glass and `HTS_ADMIN_PASSWORD`

The `HFS_ADMIN_PASSWORD` break-glass account works on every terminology
page and on `/admin/terminology`, as on the rest of the console.
`HTS_ADMIN_PASSWORD` is folded in (§6): it is adopted when the HFS one is
unset. When both are set and differ (not the case in production, where
both are `ADMIN_PASSWORD`), the HTS value is accepted for one release
**only as a machine credential on `/tx` writes** (Basic/Bearer), so
external scripts keep working, and a startup WARN plus an About-page entry
announce its removal.

### 7.4 `/tx` writes

The write guard (`helios_hts::write_guard`, classification unchanged)
accepts, in order:

1. `TxPrincipal::System` (in-process only, an extension);
2. a console session with the role from §7.2 plus a valid CSRF token
   (cookie writes from the console's own pages);
3. the break-glass credential (Basic/Bearer);
4. a SMART token verified by HFS's verifier cache (issuer/JWKS/audience
   from `HFS_TERMINOLOGY_AUTH_*`, defaulting to `HFS_AUTH_*`) with
   `system/CodeSystem.cud`, `system/ValueSet.cud` or `system/ConceptMap.cud`
   (or `system/*.cud`).

Reads stay public (unchanged). A future `HFS_TERMINOLOGY_READ_AUTH=token`
is listed in §16. SMART behaviour of FHIR endpoints is untouched.

### 7.5 Built-in authorization server (line IdP) interplay

- **Paths:** `/idp/{name}/…` is reserved (U0) and routed by hfs before the
  endpoint dispatcher. Endpoint names cannot take `idp`.
- **Console sign-in:** console mode `oidc` can point at an internal realm
  (`/idp/console`). Discovery and JWKS of an *internal* issuer must resolve
  in-process (verifier cache source `InProcess`), not over HTTP to itself.
  That avoids a startup dependency loop and the nginx hairpin. Break-glass
  stays independent of any IdP (D-9).
- **Endpoint SMART "Internal":** the endpoint's issuer is an internal IdP.
  Terminology write tokens (§7.4) from internal issuers work the same way.
- **Console IA:** an **Authorization servers** section between Terminology
  and Server (hidden unless `HFS_IDP=on`), using the same shell and nav
  registration API (§9.3).
- **Resources:** IdP token/authorize endpoints are latency-critical and run
  on the main runtime. Terminology imports are on their own runtime (§8),
  so they cannot slow token issuance.
- **DB:** IdP migrations `hm-0020`…`hm-0029`. A schema `idp` is suggested.

---

## 8. Performance and isolation in one process

### 8.1 Risks

| Risk | Source |
|---|---|
| CPU | HTS importers parse in `spawn_blocking` (tokio's shared blocking pool, up to 512 threads), so a SNOMED/LOINC parse can occupy every core while FHIR workers wait for CPU. |
| PG | Import batches (`INSERT … UNNEST`) hold connections and IO. On one pool, `/tx` reads and validator checks would queue behind them. |
| Memory | HTS import peaks of 1–8 GB plus HFS Bundle peaks of 3–5 GB, now in one cgroup. An OOM kill takes FHIR down with it. |
| Latency | Large `$expand` (closure rebuild, 20 s finalize wait) runs on the main runtime. |

### 8.2 Measures (U3)

1. **Dedicated `terminology-import` runtime.** A multi-thread runtime with
   `HFS_TERMINOLOGY_IMPORT_THREADS` workers (default `clamp(cores/4, 1, 4)`,
   `config::default_import_threads`), `max_blocking_threads =
   HFS_TERMINOLOGY_IMPORT_BLOCKING_THREADS` (4), thread names `tx-import-*`,
   and threads lowered to nice 10 at start (`setpriority` on the thread id;
   Linux only, no-op elsewhere). The following run there (spawned onto its
   handle and awaited):
   - the admin job worker (`admin/jobs.rs`);
   - bootstrap sync;
   - `POST /tx/import` bodies;
   - package-manager terminology imports;
   - closure rebuild/finalize.

   HTS importers keep their `spawn_blocking` calls, which then use *that*
   runtime's capped blocking pool.
2. **Import PG pool** (4 connections) is separate from the terminology read
   pool (16) and HFS primary (64). Imports wait on their own pool.
3. **Operation permits.** A semaphore (`HFS_TERMINOLOGY_OPERATION_PERMITS`,
   64) caps concurrent terminology calls from FHIR request paths
   (validator, search, SoF, FHIRPath). Excess calls queue up to their
   deadline, then `TxError::Unavailable`, which maps onto the existing fail
   policy. `/tx` external clients get the existing `TimeoutLayer` and the
   anonymous rate limit (§12).
4. **Bounded caches.** Validator DashMaps become LRU (50k entries). HTS
   caches are already bounded (2048 / 16384 / 10k).
5. **Memory.** The compose `hfs` service gets `mem_limit: 16g` (today the
   two containers are unlimited; HFS peaks 3–5 GB, HTS imports 1–8 GB). U3
   records `process_resident_memory_bytes` during a LOINC import plus a
   concurrent 20-entry transaction load, and sets the final value from
   that.
6. **Full releases off the serving process.** A SNOMED/LOINC/NDC full load
   runs as the one-off CLI
   `docker compose run --rm hfs hts import …` (the hts binary ships in the
   hfs image) against the same database. This is the documented path for
   multi-GB loads. The console import is for packages and incremental
   files.
7. **Cluster option.** `HFS_TERMINOLOGY_ADMIN=true` on one instance only
   (as today with `HTS_ADMIN_UI`). A dedicated "maintenance" instance can
   take imports while serving instances keep `false`.

**Acceptance (U3):** during a LOINC 2.83 console import on the production
copy, FHIR p95 for `GET Patient?_count=50`, `create Observation` (US Core)
and a 20-entry transaction rises ≤ 15 % against idle, `/tx`
`$validate-code` p95 rises ≤ 25 %, and RSS stays under the limit.

---

## 9. Web console unification

### 9.1 Information architecture

```
┌ topbar: [☰] brand(heart mark + "HealthyMind" / "FHIR Server Manager")   [search]  [theme] [account ▾] ┐
├ sidebar ────────────────────┬ main ───────────────────────────────────────────────────────────────────┤
│ Overview                    │ breadcrumb                                                              │
│ FHIR                        │ page header                                                             │
│   Endpoint ▾ (switcher)     │ content (#main swapped on navigation)                                   │
│   Data · Configure · Operate│                                                                         │
│ Terminology                 │                                                                         │
│   Home                      │                                                                         │
│   Code systems              │                                                                         │
│   Value sets                │                                                                         │
│   Concept maps              │                                                                         │
│   Concepts                  │                                                                         │
│   Manage        (admin)     │                                                                         │
│   Import · Jobs (admin)     │                                                                         │
│ Authorization servers (IdP) │                                                                         │
│ Server: Packages · Tenants ·│                                                                         │
│   Sync · Subscriptions ·    │                                                                         │
│   Status · Audit            │                                                                         │
│ Settings: Sign-in · Users · │                                                                         │
│   Sessions · About          │                                                                         │
└─────────────────────────────┴─────────────────────────────────────────────────────────────────────────┘
```

- The **product switcher is removed** (`ProductLinks` deleted; the
  `helios_hfs_console` cookie is no longer needed).
- The **brand partial from line `hm/quick-brand`** (heart mark + English
  "HealthyMind" / "FHIR Server Manager", no Chinese in the brand) is *the*
  brand of the shell. U4 takes it as is.
- **Cross-links.** A CodeSystem/ValueSet reference anywhere in the FHIR
  pages (editor bindings, validation issues, search-parameter `:in` help,
  package detail) links to its terminology page. Terminology pages show
  "used by" for IG packages installed in HFS (package registry lookup).
- **Remote mode:** the Terminology section browses the remote server
  through `RemoteTerminology` (read pages only). Manage/Import/Jobs are
  hidden. **Off mode:** the section shows one Home page with enablement
  instructions (today's `/ui/terminology` setup page).
- Global search (topbar, `/` shortcut) gains a terminology provider: a code
  or display text resolves through `$lookup` / `$expand?filter=` next to
  FHIR resource ids. This is U5's scope if the audit asks for it; otherwise
  U4 ships the plain section.

### 9.2 Routing

`/ui/terminology/{code-systems,value-sets,concept-maps,concepts,manage,import,jobs,capability-statement}/…`
mirrors the hts-ui routes one to one, with `terminology` (inventory)
renamed to `manage`. `/ui/hts/**` → `legacy_console_redirect` (301 for
GET/HEAD, 308 otherwise). hts-ui's internal links, `hx-get` targets and
assets come from one `TerminologyPaths` value (prefix `/ui/terminology` in
hfs, `/ui/hts` in the standalone binary). Assets are served once from
`/ui/assets` (hts-ui already embeds the same files through the symlink).

### 9.3 Shared shell and design system

- **The shell moves to `ui-chrome`.** `shell.html` (topbar, sidebar, main,
  footer, progress bar, toasts) plus the `Shell` context (`brand`, `nav:
  Vec<NavSection>`, `user`, `csrf`, `build_id`, `lang`, `theme`). Both
  helios-ui's and hts-ui's page templates extend it (Askama `dirs` include
  ui-chrome's templates; ui-chrome already re-exports askama). Each crate's
  own `layouts/base.html` disappears.
- **Nav registration.** `ui-chrome::nav::{NavSection, NavItem, NavRegistry}`.
  helios-ui registers FHIR/Server/Settings (from `manager/nav.rs`), hts-ui
  registers Terminology, and line IdP registers Authorization servers.
  Items carry `required_role` and `href`. The host filters by principal.
- **Design system.** One `app.css` with tokens (already shared). hts-ui's
  `hts-*` component classes are mapped onto the shared components
  (`data_table`, `page_header`, `status_pill`, `job_progress`, `toolbar`,
  `form_section`) where identical, and kept namespaced where genuinely
  terminology-specific (concept tree, expansion viewer). Spacing, type
  scale and table density are reconciled. U5 consumes the UX audit's
  concrete findings.
- **One i18n catalog.** hts-ui keys are moved into `locales/*/terminology.ftl`
  with the same `I18n` type.

### 9.4 Persistent-shell navigation — exact specification

Goal: clicking a sidebar link, breadcrumb, pager or in-page link replaces
only the content, keeps the sidebar and topbar DOM (including sidebar
scroll and open/closed groups), updates the URL and title, supports
back/forward, never shows an unstyled flash, and signals progress on slow
pages.

1. **Boost scope.** `<body hx-boost="true" hx-target="#main" hx-select="#main"
   hx-swap="outerHTML show:window:top" hx-push-url="true">` in `shell.html`.
   Opt-outs (`hx-boost="false"`):
   - sign-in/out;
   - language links;
   - downloads (bulk export files, NDJSON);
   - links opening new tabs or leaving the origin;
   - `/tx/**` raw API links;
   - pages whose scripts are not yet lifecycle-clean (CodeMirror editor
     pages, until they move to the page registry, step 5).
2. **Server side (helios-ui + hts-ui, one helper `render_page`).** When the
   request has `HX-Request: true` and `HX-Boosted: true`, and is not a
   history-restore request (`HX-History-Restore-Request`), the handler
   renders only `<title>` plus the `#main` element (Askama block
   rendering: `{% block main %}` rendered alone through a `*Fragment`
   template that shares the block). Otherwise it renders the full shell.
   Responses set `Vary: HX-Request, HX-Boosted` (the `AutoVaryLayer` is
   extended).
3. **Nav state without re-rendering the sidebar.**
   - **Active item.** After every swap, `nav.js` sets `aria-current="page"`
     on the nav link whose `data-match` prefix best matches
     `location.pathname`, and opens its group. This is pure client work;
     the nav markup is identical across pages of one context.
   - **Context changes** (endpoint switch `/ui/e/{a}` → `/ui/e/{b}`, role
     change, sign-in mode change). The server compares the context key of
     the target page (`{endpoint}:{role}:{idp_on}`) with the client's
     `X-HM-Nav` header (sent from `<body data-nav-key>` by
     `htmx:configRequest`). On mismatch it appends
     `<nav id="sidebar-nav" hx-swap-oob="true">…</nav>` and the new
     `data-nav-key`. The sidebar scroll container (`#sidebar-scroll`, outside
     the OOB element) keeps its position.
   - **Breadcrumb and page title** live inside `#main`.
4. **History.** `htmx.config.historyCacheSize = 20`; `historyRestoreAsHxRequest = false`
   (restores get the full page when not cached). The scroll position is
   restored by htmx's snapshot. Filter and sort state is **always in the
   URL**: every filter form uses `hx-get` + `hx-replace-url="true"` (the
   UIF inventory/browse pages already put sort/filter in the query; U4
   audits the rest). Back therefore returns to the same filtered view.
   Per-page "Load more" lists push the page cursor into the URL
   (`hx-replace-url`).
5. **Page scripts lifecycle.** A tiny registry in `app.js`:
   `HM.page('code-systems', {init(root), destroy()})`. It runs `init` on
   `htmx:load` for elements with `data-page`, and `destroy` on
   `htmx:beforeSwap` of `#main` (clearing intervals, EventSources, chart
   observers, the metrics sampler polling, CodeMirror instances). Page
   scripts are loaded once with `defer` from the shell (bundled per
   section, `terminology.js`, `fhir.js`, `manager.js`), not per page, so
   swaps execute no `<script src>`. Inline scripts in `#main` are
   forbidden (CSP `script-src 'self'` becomes possible, §12).
6. **Loading indicators.**
   - **Global progress bar.** `#nav-progress`, 2 px at the top of `main`,
     driven by `htmx:beforeRequest`/`afterSettle` on boosted requests. It
     appears only after **150 ms** (no flicker on fast pages) and animates
     indeterminate. `aria-busy="true"` goes on `#main`.
   - **Skeleton.** If the request is still running after **400 ms**, `#main`
     content gets the `is-loading` class (dimmed, `pointer-events:none`,
     skeleton rows for tables via CSS on `[data-skeleton]`). The skeleton
     is removed on swap.
   - **Slow actions** (import, compact, delete version, sync run, duplicate,
     validate): buttons use `hx-disabled-elt="this"` and
     `hx-indicator="closest .action"`, showing a spinner plus label
     ("Importing…"). Actions over ~2 s answer `202` with a job card that
     polls (`hx-trigger="every 2s"` until a terminal state, then a toast).
     This builds on the quick-brand line's htmx indicators.
   - **Filter inputs** debounce (`hx-trigger="input changed delay:300ms"`)
     and show an inline spinner in the field. This fixes the missing filter
     loading state found on production on 11-23 (Value Sets name filter,
     ~2.2 s with no feedback).
7. **No flash.** Theme and language are attributes on `<html>`, which is
   never swapped. CSS is one fingerprinted file. Fonts are preloaded once.
   Optional `htmx.config.globalViewTransitions = true` cross-fades only
   `#main` (`view-transition-name: main`). It is disabled under
   `prefers-reduced-motion` and in browsers without the API.
8. **Assets.** URLs become `/ui/assets/{build_id}/…` with
   `Cache-Control: public, max-age=31536000, immutable`. The unversioned
   path keeps serving with `no-cache` for compatibility. A full page load
   then costs 1 HTML request plus 0 revalidations, instead of ~50 `304`s.
9. **Build change.** Every htmx request sends `X-HM-Build`. If it differs
   from the server's build id, the server answers `HX-Refresh: true`, and
   the client does a full reload, so stale shell JS never talks to a new
   server.
10. **Errors and session expiry.**
    - `htmx:responseError` / `sendError` render an inline error panel into
      `#main` (status, message, Retry), never a blank page.
    - A 401/303 to login on a boosted request arrives as `HX-Redirect`
      (already implemented in the console guard) → full navigation to
      sign-in with `next=`.
    - 403 renders the console's denied panel inside `#main`.
11. **Accessibility.** After a swap, focus moves to the new `<h1>`
    (`tabindex="-1"`), and an `aria-live="polite"` region announces the
    page title. The skip link targets `#main`.
12. **No-JS.** Every link is a real `href`, and every filter form a real
    GET form. Without JS the console works with full page loads (existing
    `nojs.spec.ts` keeps passing).

**Acceptance (U4):** Playwright asserts the following.

- Navigating sidebar → 10 pages across FHIR and Terminology keeps the same
  `aside` element (`elementHandle` identity) and its `scrollTop`.
- Back/forward restores filter values and table scroll.
- No request for `app.css`/fonts after the first load.
- The progress bar is not shown for responses < 150 ms and is shown for a
  delayed route (test hook adds 600 ms).
- `aria-current` is correct.
- Endpoint switch replaces the nav.
- A build-id mismatch triggers a reload.
- axe has no serious violations.
- Visual diff of the shell between pages is 0 px.

---

## 10. Admin API, metrics, health, audit

- **Admin API:** `/admin/terminology/**` (the HTS admin router unchanged
  inside) joins `/admin/endpoints`, `/admin/sync`, `/admin/packages` and
  `/admin/console`. It uses the same guard (`admin_guard_unless_jwt`:
  console session + CSRF, break-glass, system-scope token). OpenAPI
  fragment `docs/api/admin-terminology.yaml`. Job state stays in
  `HFS_TERMINOLOGY_DATA_DIR/jobs` (U2 keeps the file store; moving job
  state to `cluster_jobs` is §16).
- **Metrics:** one recorder (`metrics::init("hfs")`; HTS's `init("hts")` is
  skipped when embedded). New series:
  - `terminology_calls_total{caller="validator|search|sof|fhirpath|packages|console",op,outcome}`
  - `terminology_call_duration_seconds{caller,op}`
  - `terminology_permits_available`
  - `terminology_import_running`
  - `terminology_epoch`

  Pool gauges `pool="terminology"`, `"terminology_resources"` and
  `"terminology_import"`. HTTP route labels for `/tx/...` come from the
  nested router's templates. Grafana "Helios ops" gains a Terminology row
  (replacing the `service="hts"` panels). The `HtsDown` alert becomes
  `TerminologyUnhealthy` (`hfs_terminology_up == 0 for 5m`, from the health
  probe), and `HfsDown` covers both.
- **Health:** `/health` → `{status, service:"hfs", version, uptime, components:{storage, terminology:{mode, state}}}`.
  `/_readiness` stays storage-only by default (FHIR must not go unready
  because terminology is degraded). `HFS_READINESS_REQUIRE_TERMINOLOGY=true`
  is for deployments that prefer it. `/tx/health` (HTS's own) remains.
- **Audit:** the embedded HTS gets HFS's `AuditSink` (one queue, one
  fallback file, one retention). Terminology AuditEvents carry
  `source.observer = HFS_AUDIT_SOURCE_OBSERVER` and the extension
  `component=terminology`. Console terminology actions are console
  AuditEvents (same format as other console writes). The admin
  `audit.ndjson` file stays (it is HTS's job log).
- **Tracing:** one OTLP service `hfs`. Terminology spans are children of
  the FHIR request span that triggered them (validation, search), which
  the loopback makes possible because the call is in-process.

---

## 11. Clustering

- All instances run embedded terminology against the same schema. Caches
  stay coherent through the `terminology_epoch` poll, now including the
  HFS-side caches (§4.5).
- `HFS_TERMINOLOGY_ADMIN=true` on exactly one instance (import jobs and
  upload staging live in its data dir), as today with `HTS_ADMIN_UI`.
  Other instances answer `/admin/terminology/**` and the Manage/Import/Jobs
  pages with a page that names the admin instance (from
  `hfs_terminology_state.admin_instance`, a heartbeat written by the admin
  instance; U2). nginx needs no special routing: the console links the
  user there, or the operator routes `/ui/terminology/import*`,
  `/ui/terminology/jobs*` and `/admin/terminology*` to it (documented in
  `cluster.md`).
- Bootstrap import keeps its advisory lock (`HTS_BOOT`), so N instances
  starting together import once.
- Sessions, endpoints, console settings and sync are already
  cluster-shared (PG).

---

## 12. Security review

| Topic | Assessment / measure |
|---|---|
| Merged attack surface | HTS code now runs in the process that holds FHIR data and the secret key ring. Both are Rust with no `unsafe` in HTS request paths. The main new risk is **availability**: a pathological `$expand` or import can now OOM or starve FHIR. Measures: §8 (runtime, pools, permits, mem limit), HTS `max_expansion_size`, and per-route body limits (below). |
| Body limits | Today `/tx` accepts 256 MiB for everything. New: **`/tx` reads 10 MiB** (Parameters/Bundles of reads); `/tx/import`, `/admin/terminology/uploads/*` and authenticated writes up to `HFS_TERMINOLOGY_MAX_BODY_SIZE` (256 MiB), applied after the write guard admits the request. |
| Admin endpoints | `/tx/admin/**` was public-path, password-gated. Now `/admin/terminology/**` sits behind the admin API guard (console session + CSRF + role, break-glass, system token). nginx already restricts nothing on `/admin`, so the guard is the boundary, as for `/admin/endpoints`. The `/tx/admin` redirect is a 308 to the same origin and leaks nothing. |
| `/tx` writes | §7.4. In-process `System` cannot be forged (extension). The console cookie alone is not enough without a CSRF token, and `SameSite=Strict` stays. SMART tokens need `system/*` terminology write scopes. The HTS password as an extra credential is time-boxed (§7.3). |
| Anonymous `/tx` reads | Unchanged exposure, now subject to HFS's anonymous rate limit (`HFS_RATE_LIMIT_ANONYMOUS`, 600/min per IP). Today HTS has no rate limit. Validator/search traffic is in-process and not rate-limited. |
| DB privileges | Default: one role `hfs` owns both schemas, so an SQL injection in either subsystem reaches both. That is accepted: both already live in one process memory with both credentials. Least-privilege option: `HFS_TERMINOLOGY_DATABASE_URL` with role `hts` granted only `USAGE, CREATE` on `terminology`; the unify script prints the GRANTs. |
| Sessions | One cookie (`hfs_console`, `Path=/`, `HttpOnly`, `SameSite=Strict`, `Secure`). The `hts_admin` cookie is retired. The HTS-scope `admin_sessions` rows expire on their own. |
| CSP | With page scripts out of `#main` (§9.4.5), the console sends `Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; frame-ancestors 'none'` (the theme-boot inline script gets a hash). |
| SSRF | Remote terminology URLs entered in endpoint config pass `OutboundPolicy` (unchanged). The embedded mode removes `hts` from `HFS_OUTBOUND_ALLOW`. |
| Secrets | `HFS_TERMINOLOGY_SERVER_TOKEN` is unneeded in embedded mode (prod: unset it after Stage 3). Deprecation messages never print values (tested). |

---

## 13. Rollout

### 13.1 Stages

| Stage | Image | Config | Effect | Rollback |
|---|---|---|---|---|
| **0** | unified image (`hfs` with terminology feature; `hts` binary inside too) | unchanged (`HFS_TERMINOLOGY_SERVER=http://hts:8090` → remote per U-2); the hts container runs the same image's `hts` binary | binary swap only. Gates identical to `mgr-5f4c6a4f8` | previous image |
| **1** | same | hfs: `HFS_TERMINOLOGY=embedded`, `HFS_TERMINOLOGY_DATABASE_URL=<hts db>`, `HFS_TERMINOLOGY_ADMIN=true`, `HFS_TERMINOLOGY_BOOTSTRAP_DIR=/app/terminology-data`, `HFS_TERMINOLOGY_IMPORT_DIR=/import` (bind mount moves to hfs), `HFS_TERMINOLOGY_DATA_DIR=/data/terminology-admin` (copy of `hts-data:/data/hts-admin`); **stop the hts container**; nginx `/tx/ → hfs:8080/tx/` without sub_filter, `/ui/hts` block removed | **one process, one console, one sign-in.** Data still in DB `hts` | flags back + `docker compose up -d hts` + old nginx.conf (kept as `nginx.conf.pre-unify`) |
| **2** | same | run `unify-db.sh` (§5.5), then drop `HFS_TERMINOLOGY_DATABASE_URL` | **one database** | URL back to `hts` (untouched until cleanup) |
| **3** | same | remove all `HTS_*` from compose, the `hts` service, `HFS_TERMINOLOGY_SERVER(_TOKEN)`, `hts` from `HFS_OUTBOUND_ALLOW`; after 2 weeks `DROP DATABASE hts`, delete the `hts-data` volume | cleanup | — (by then 2 weeks of evidence) |

The UI changes (U4/U5) ship with Stage 1 at the earliest. The `/ui/terminology`
pages exist only in embedded mode. In Stage 0 the HFS console still links
to `/ui/hts` (served by the hts container).

### 13.2 Compose after Stage 3 (diff in words)

- The `hts` service is removed.
- `hfs` gains:
  - `HFS_TERMINOLOGY: embedded`
  - `HFS_TERMINOLOGY_ADMIN: "true"`
  - `HFS_TERMINOLOGY_BOOTSTRAP_DIR: /app/terminology-data`
  - `HFS_TERMINOLOGY_IMPORT_DIR: /import`
  - `HFS_TERMINOLOGY_DATA_DIR: /data/terminology-admin`
  - `HFS_TERMINOLOGY_PG_POOL_SIZE: "16"`
  - the `/import:ro` bind mount
  - `mem_limit` (§8.2)
- `hfs` loses `HFS_TERMINOLOGY_SERVER`, `HFS_TERMINOLOGY_SERVER_TOKEN`,
  `depends_on: hts`, and `hts` from `HFS_OUTBOUND_ALLOW`.
- `HTS_AUTH_*` are inherited from `HFS_AUTH_*`, so nothing replaces them.

### 13.3 nginx after Stage 1

```nginx
location = /tx/metrics { allow 127.0.0.1; allow ::1; allow 172.16.0.0/12; deny all; proxy_pass http://hfs:8080/metrics; }
location = /tx  { proxy_pass http://hfs:8080/tx; }
location /tx/   { proxy_pass http://hfs:8080/tx/; }          # no Host override, no sub_filter
# /ui/hts block removed: /ui covers it (hfs redirects /ui/hts/** itself)
location /ui    { proxy_pass http://hfs:8080; }
```

nginx then has only two upstreams (hfs, keycloak). The hts upstream and the
`sub_filter` (which today buffers and rewrites every terminology JSON
response) disappear.

### 13.4 Build and deploy

- `build-images.sh` builds one image `healthymind/hfs:<tag>` containing
  `hfs`, `hts` (CLI) and `terminology-data/`. `HTS_IMAGE` stays in `.env`
  until Stage 3 and points at the same tag (the hts container runs
  `command: ["hts"]`).
- `deploy.sh`:
  - smoke paths stay (`/fhir/metadata /tx/metadata /ui /ui/hts`, where
    `/ui/hts` now expects 301);
  - the gates (prod_verify 42, validator 94, test_hts 112, test_hfs_server
    184, stage_b) must give identical results in every stage;
  - test_hts runs against `/tx` unchanged.
- `predeploy-dump.sh` follows §5.5 after Stage 2.
- `rollback-restore.sh` is unchanged until Stage 2. After that it restores
  one `hfs` dump, with the terminology schema in a separate file.

### 13.5 Observation per stage

24 h between stages:

- error rates;
- `terminology_call_duration_seconds` vs the remote baseline;
- RSS;
- the weekly validation-log review (`HFS_VALIDATION_MODE=log`) must show no
  new terminology-caused issue classes.

### 13.6 GCE VM (paused)

Nothing here depends on it. When it resumes, it gets the post-Stage-3
shape directly: one VM container plus PG, so the HTS VM sizing and its
firewall rule disappear. The Kubernetes manifests (`hfs.yaml`, `hts.yaml`,
ingress `/tx` → hts) need the same `/tx` → hfs change, which is tracked in
U1's ops doc.

---

## 14. Work breakdown

U0 is done on this branch. U1–U5 start in parallel from `hm/design-unify`,
each on `hm/unify-u{N}-…`, rebased on merged predecessors, and green in
HealthyMind CI. File ownership is exclusive except for the listed **shared
touch points** (one-line additions, merged by the integration line in
order). Merge order: **U1 → U2 → U3 → U4 → U5**. U6 (UX audit) merges
last.

### U0 — Foundation (DONE, `hm/design-unify`)

- **Owns:** `crates/terminology/**`, the workspace member line, the
  `Cargo.lock` entry, the CI optional group `terminology:helios-terminology`,
  `RESERVED_SEGMENTS` += `terminology`, `idp`, and this document.
- **Provides:**
  - `service::{TerminologyService, TxRequest, TxResponse, TxPrincipal,
    TxMethod, TxError, ServiceKind, TerminologyHealth, HealthState,
    ExpandParams, ValidateCodeParams, ValidateCodeOutcome, ImportReport,
    DisabledTerminology}`
  - `loopback::{LoopbackTerminology, to_http_request}`
  - `remote::{RemoteTerminology, HttpTransport, OutboundRequest}`
  - `config::{TerminologyMode, TerminologySettings, EmbeddedSettings,
    EnvView, HTS_ENV_ALIASES, classify, Deprecation, AliasKind,
    default_import_threads, tx_public_url}`
  - `schema::{TERMINOLOGY_SCHEMA, with_search_path, search_path,
    validate_schema_name, sqlite_terminology_url, HM_0011_SQL, hm_0011_sql,
    TERMINOLOGY_TABLES, EMBEDDED_STORE_TABLES, SCHEMA_SENSITIVE_LOOKUPS,
    UNIFIED_FORK_MIGRATIONS, UNIFIED_MIGRATION_BLOCK, IDP_MIGRATION_BLOCK}`
  - `access::{TerminologyAction, required_role, classify_console}`
  - `routes::{TX_PREFIX, ADMIN_PREFIX, CONSOLE_PREFIX, LEGACY_CONSOLE_PREFIX,
    IDP_PREFIX, RESERVED_ROOT_SEGMENTS, legacy_console_redirect, strip_tx}`
- **Tests:** 19 unit tests (`cargo test -p helios-terminology`): loopback
  round trip, principal as an extension gating writes, query encoding,
  remote token rules, base validation, alias adoption/conflict, no values
  in messages, mode defaults (production stays remote), Stage-1 URL,
  fail-fast cases, search-path URL forms, SQLite sibling, migration blocks,
  role table, legacy redirects, `/tx` stripping, reserved segments.

### U1 — One database: schema, isolation, data move

- **Scope:**
  - fork migrations `hm-0011` (+ privilege preflight) and `hm-0012` table
    change;
  - schema qualification of every `SCHEMA_SENSITIVE_LOOKUPS` site (both
    stores) plus the isolation test (§5.3);
  - `hfs_terminology_state` read/write helpers;
  - `unify-db.sh` (preflight, freeze hooks, template snapshot, rename,
    dump/restore, verify, record, export-delta, `--in-place`, `--replace`,
    `--dry-run`);
  - restore rehearsal on the newest production dumps with timings;
  - predeploy-dump / rollback-restore changes;
  - docs `docs/ops/unified-server.md` (migration + rollback section),
    `database-migrations.md`, `backup-restore.md`;
  - the K8s manifest note.
- **Owns:**
  - `crates/persistence/src/backends/postgres/schema.rs` (fork list +
    qualification only);
  - `crates/hts/src/backends/postgres/schema.rs`;
  - `crates/hts/src/backends/postgres/mod.rs` (pool URL/role only);
  - new `crates/persistence/tests/terminology_schema.rs`;
  - `hfs-eval/deploy/unify-db.sh` (new);
  - `hfs-eval/deploy/{predeploy-dump,rollback-restore}.sh`;
  - `docs/ops/{unified-server,database-migrations,backup-restore}.md`.
- **Contracts:** tables in §5.1, `hfs_terminology_state` columns
  (`HM_0011_SQL`), migration names in `UNIFIED_FORK_MIGRATIONS`.
- **Acceptance:**
  - the isolation test passes;
  - existing persistence PG and HTS PG suites are unchanged (3116/2015);
  - the rehearsal on a production copy: counts and fingerprint equal,
    `test_hts` 112/112 against embedded-on-schema identical to remote, and
    measured downtime documented;
  - re-running the script after success is a no-op;
  - a killed copy resumes with `--replace`.
- **Depends on:** U0. It can run before U2 lands (the test uses HTS's
  backend directly).

### U2 — Embedded terminology runtime and callers

- **Scope:**
  - split `helios_hts::server::create_app` into `tx_router`, `admin_router`
    and `ui_state` builders, plus `EmbeddedHts::start(settings, audit_sink,
    import_runtime) -> EmbeddedHts { service: Arc<dyn TerminologyService>,
    tx_router, admin_router, ui: HtsUiState }`;
  - `public_base` for HTS links (`/tx`);
  - write guard accepting `TxPrincipal`, console principal and HFS verifier
    tokens (§7.4);
  - `changes()` from the epoch poll;
  - hfs feature `terminology` and wiring in `crates/hfs/src/main.rs` +
    `crates/hfs/src/terminology.rs` (new);
  - `TerminologySettings` resolution and deprecation logging;
  - the reqwest `HttpTransport` in rest;
  - migrate all callers in §4.3 (rest search/SoF/validation/packages,
    fhirpath context, ui editor/probe);
  - remove `FHIRPATH_TERMINOLOGY_SERVER` `set_var`;
  - metrics (`terminology_*`), `/health` component, readiness flag;
  - audit sink sharing;
  - admin-instance heartbeat;
  - `docs/api/admin-terminology.yaml`;
  - nginx/compose Stage 0/1 templates.
- **Owns:**
  - `crates/hts/src/{server.rs,state.rs,write_guard.rs,audit.rs,main.rs}`;
  - `crates/hts/src/backends/postgres/epoch.rs`;
  - `crates/hts/Cargo.toml`;
  - `crates/hfs/src/{main.rs,terminology.rs}`;
  - `crates/hfs/Cargo.toml`;
  - `crates/rest/src/{terminology.rs,validation.rs (provider only),packages/hts.rs,handlers/sof/terminology.rs,config.rs (terminology section),handlers/health.rs}`;
  - `crates/fhirpath/src/{terminology_client.rs,terminology_functions.rs,evaluator.rs (context field)}`;
  - `crates/ui/src/{editor.rs (expand),manager/backend.rs (probe)}`;
  - `crates/observability/src/metrics.rs` (label helper);
  - `hfs-eval` compose/nginx templates under `deploy/unify/`.
- **Shared touch points:** one `WebState.terminology: Arc<dyn TerminologyService>`
  field in `crates/ui/src/lib.rs` (U4 owns the file) and one
  `AppState::terminology()` accessor in `crates/rest/src/state.rs`.
- **Provides:**
  - `EmbeddedHts`;
  - `Arc<dyn TerminologyService>` in `AppState` / `WebState` /
    `ManagerContext`;
  - `HtsUiState` with a `TerminologyService`-backed upstream for U4.
- **Acceptance:**
  - `test_hts` 112/112 through `/tx` on the embedded server, identical to
    standalone;
  - `test_hfs_server` 184/184 and validator 94/94 identical in remote and
    embedded mode;
  - `:in` search results identical (eval set);
  - `fhir-test-cases` verdicts identical;
  - package install of US Core + TW Core imports the same terminology
    counts;
  - an `HTS_*`-only compose (Stage 0 env passed to hfs) starts with the
    expected deprecations;
  - the latency acceptance in §4.4;
  - CI: rest, hfs and hts groups green in both modes (a new matrix entry
    `hfs-embedded-terminology`).
- **Depends on:** U0. Integrates first after U1.

### U3 — Isolation: runtimes, pools, permits, limits

- **Scope:**
  - `terminology-import` runtime (niced threads) and moving the job worker,
    bootstrap, `/import` bodies, package-terminology imports and closure
    finalize onto it;
  - import PG pool;
  - operation-permit semaphore in `LoopbackTerminology` callers (a wrapper
    `PermittedTerminology<T>`);
  - LRU bounds for validator caches;
  - per-route body limits on `/tx` (10 MiB reads);
  - anonymous rate limit on `/tx`;
  - compose `mem_limit`;
  - the load test (§8.2 acceptance) as `eval/scripts/unify/isolation.py`.
- **Owns:**
  - `crates/hts/src/admin/jobs.rs`;
  - `crates/hts/src/operations/import_bundle.rs`;
  - `crates/hts/src/import/dispatch.rs` (runtime handle plumbing only);
  - `crates/terminology/src/permits.rs` (new; U0 crate, additive);
  - `crates/rest/src/validation.rs` (cache bounds only, after U2);
  - `crates/hfs/src/terminology_runtime.rs` (new);
  - `hfs-eval/eval/scripts/unify/isolation.py`.
- **Acceptance:** §8.2 numbers on the production copy; no regression in
  HTS import tests; and a unit test that an import job's tasks run on
  `tx-import-*` threads.
- **Depends on:** U0, and U2's `EmbeddedHts` signature (develops against
  the U0 trait plus HTS directly; integrates after U2).

### U4 — One console: shell, sign-in, routes, persistent navigation

- **Scope:**
  - move the shell into `ui-chrome` (`shell.html`, `Shell`, `NavRegistry`);
  - both crates' pages extend it;
  - hts-ui mounted inside helios-ui at `/ui/terminology` under the console
    guard;
  - hts-ui login/gate removed from the embedded path (kept for standalone);
  - `TerminologyPaths`;
  - legacy redirects;
  - role table enforcement plus nav filtering;
  - product switcher removed;
  - brand partial from `hm/quick-brand`;
  - persistent-shell navigation (§9.4 items 1–12: `render_page` helper,
    `Vary`, nav key OOB, `nav.js` active state, page-script registry and
    section bundles, progress bar, skeletons, slow-action pattern,
    debounced filters, fingerprinted immutable assets, build refresh,
    error panel, focus management);
  - CSP header;
  - one i18n catalog (`terminology.ftl`);
  - remote/off mode variants of the Terminology section;
  - cross-links FHIR ↔ terminology.
- **Owns:**
  - `crates/ui-chrome/**`;
  - `crates/hts-ui/**`;
  - `crates/ui/src/{lib.rs (router/shell),navigation.rs,manager/nav.rs,i18n.rs}`;
  - `crates/ui/templates/layouts/**` (removed → ui-chrome);
  - `crates/ui/assets/{app.js (new),nav.js,busy.js,app.css}`;
  - `locales/*/terminology.ftl`;
  - `crates/hfs/src/manager/ui.rs`;
  - the journey scripts below.
- **Shared touch points:** the `WebState.terminology` field (from U2) and
  page templates of other lines only gaining `{% extends "shell.html" %}`
  (mechanical).
- **Provides:** `NavRegistry::register(section, item)` for line IdP;
  `render_page`; the page-script registry; the slow-action component.
- **Acceptance:**
  - the route gating matrix: every `/ui/terminology` route × {anonymous,
    viewer, editor, admin, terminology-admin binding} × {none, local,
    oidc};
  - one sign-in journey (sign in once at `/ui/login`, open `/ui/terminology/manage`,
    no second prompt);
  - `/ui/hts/*` redirects;
  - the §9.4 acceptance list;
  - existing hts-ui Playwright specs moved to `/ui/terminology` and passing;
  - `nojs.spec.ts` passing;
  - UI tests (1428+) green.
- **Depends on:** U0, and U2 (for the in-process upstream). Before U2 it
  develops against `LoopbackTerminology` over a test router.

### U5 — Terminology pages unification polish (design system + data correctness)

- **Scope:**
  - map `hts-*` components onto the shared ones;
  - reconcile density/typography;
  - terminology home as an overview card set on the manager Overview
    (Terminology health tile, concepts count, last import, epoch);
  - "used by" (IG packages) on CodeSystem/ValueSet pages;
  - global search terminology provider;
  - loading states on every terminology filter (the 11-23 finding);
  - mobile layouts of terminology tables.
- **Owns:**
  - `crates/hts-ui/templates/pages/**`;
  - `crates/hts-ui/templates/partials/hts-*`;
  - `crates/ui/src/manager/overview*.rs` (terminology tiles only);
  - `crates/ui/assets/terminology.js` (new section bundle).

  It rebases on U4: same crate, different files after U4's shell move.
- **Acceptance:** journeys (below) with numbers = API; visual pass of 14
  terminology pages × light/dark × desktop/phone; axe clean.
- **Depends on:** U4.

### U6 — UX improvements from the audit (reserved)

- **Input:** `hfs-eval/eval/results-ux-audit/` (the parallel UX audit line).
- **Scope:** the audit's prioritised findings that are not already covered
  by U4/U5. Triage happens when the report lands. Each finding is mapped to
  (a) already fixed by U4/U5, (b) U6 change, or (c) rejected with reason,
  in `results-unify/ux-triage.md`.
- **Owns:** decided at triage. It works only on files whose owner package
  has merged (U4/U5 files after their merge), so it never conflicts.
- **Acceptance:** each accepted finding has a before/after screenshot and a
  journey check where it is behavioural.
- **Depends on:** U4, U5 merged.

### Functional journeys (extension of `hfs-eval/eval/scripts/ui-functional/journeys.py`)

Owned by U4 (structure) and U5 (terminology numbers):

- **Login once.** `login(page)` replaces `hfs_login` + `hts_login`.
  `UIF_HTS_*` credentials are dropped. The script fails if any `/ui/hts/login`
  or second sign-in form appears.
- `unified-sso`:
  - sign in at `/ui/login`;
  - visit `/ui/terminology`, `/ui/terminology/manage`, `/ui/terminology/import`
    and `/ui/e/default/data`;
  - assert no redirect to any login;
  - sign out once and assert both a FHIR page and a terminology page
    redirect to `/ui/login`.
- `unified-redirects`: `/ui/hts`, `/ui/hts/code-systems?sort=name` and
  `/ui/hts/terminology` → their `/ui/terminology/...` targets with the
  query preserved.
- `unified-shell-persistence`:
  - click through 10 sidebar items (5 FHIR, 5 Terminology);
  - the `aside` handle is identical, `scrollTop` is preserved, there are no
    `app.css` requests after the first, and the progress bar appears on the
    slow route;
  - back ×3 restores the Value Sets filter value and its result count.
- `unified-roles`: a viewer sees no Manage/Import/Jobs; direct
  `/ui/terminology/import` gives the console 403 panel; the editor/admin
  matrix.
- `unified-crosslinks`: from an IG package detail, open a ValueSet and land
  on its terminology page with the same URL/version.
- The existing `HTS_JOURNEYS` move to `/ui/terminology` paths and new
  selectors (shared components). `hts-topbar` becomes `shell-topbar` (no
  product switcher; brand = quick-brand partial). `hfs_crawl` stops
  skipping terminology links.
- The visual pass covers `VISUAL_PAGES` with terminology pages under the
  new paths.
- **Readonly production run after each stage:** 0 failures. Write journeys
  run on the test stack (`stack/up.sh` gains a unified mode: one hfs
  binary, both dumps).

### Order and parallelism

```
U0 (done) ──┬── U1 one DB (schema, isolation, unify-db) ─────┐
            ├── U2 embedded runtime + callers ───────────────┤ integrate in order
            ├── U3 isolation (runtime/pools/permits) ────────┤ U1 → U2 → U3 → U4 → U5 → U6
            ├── U4 one console + persistent shell ───────────┤ (rebase on previous merge,
            ├── U5 terminology pages polish (after U4 files) ┤  CI green, gates identical,
            └── U6 UX audit (after U4/U5) ───────────────────┘  journeys on test stack)
Deploy:  Stage 0 after U2 · Stage 1 after U3+U4 (U5 may follow) · Stage 2 after U1 rehearsal · Stage 3 +2 weeks
```

Highest conflict risk:

- `crates/hts/src/server.rs` (U2 owns; U3 touches only `admin/jobs.rs` and
  `import_bundle.rs`);
- `crates/ui/src/lib.rs` (U4 owns; U2 adds one field);
- `crates/rest/src/validation.rs` (U2 provider, then U3 cache bounds,
  sequential);
- `Cargo.lock` (regenerate on rebase, never hand-merge).

---

## 15. Coordination with parallel lines

| Line | Interface |
|---|---|
| **IdP** (`hm/design-idp`, docs/design/authorization-server.md) | Migration block `hm-0020`…`hm-0029` (ours `hm-0011`…`hm-0019`). Path `/idp/**` is reserved in `RESERVED_SEGMENTS` and `routes::IDP_PREFIX` (U0 already added `idp`, so the IdP line must not re-add it; a merge conflict there is one line). Console IA: an "Authorization servers" section registered through U4's `NavRegistry`. Internal issuers resolve discovery/JWKS in-process (verifier cache source). Token endpoints are on the main runtime; terminology imports are isolated (§8). DB schema `idp` suggested. |
| **quick-brand** (`hm/quick-brand`) | Its brand partial (heart mark + "HealthyMind" / "FHIR Server Manager", no Chinese) and htmx loading indicators are kept. U4 moves the partial into `ui-chrome::shell` unchanged and builds §9.4's indicators on its classes. |
| **UX audit** (`results-ux-audit/`) | Consumed by U6 (and U5 where the scope overlaps). |
| **Validation log review** (until ~12-01) | The Stage 1 switch should not overlap the decision to move to `enforce`. Do Stage 1 either before the review window closes, with a one-week comparison of log output in remote vs embedded mode, or after the enforce decision. |

---

## 16. Open items (decided defaults, revisit later)

- Terminology operations on FHIR endpoint bases (`/fhir/{ep}/ValueSet/$expand`)
  delegating to the service — not in v1.
- `HFS_TERMINOLOGY_READ_AUTH=token` (require tokens for `/tx` reads).
- Terminology job state in `cluster_jobs` instead of the admin data dir
  (would lift the one-admin-instance rule).
- Typed in-process fast paths (no JSON) if U2 profiling shows they matter.
- `NOTIFY` on `terminology_epoch` to wake pollers early.
- Retiring the standalone `hts` server mode (keep the CLIs) once no
  deployment uses it.

---

## 17. Changelog

- 2026-11-23 U0: design, `crates/terminology` (19 tests), `RESERVED_SEGMENTS`
  += `terminology`, `idp`, CI optional group.
