# Running HFS in a Cluster

This chapter describes what happens when you run more than one `hfs` instance
behind a load balancer, which subsystems are cluster-safe today, and how the
`HFS_CLUSTER` switch enforces a safe configuration at boot.

The full design and its status live in the repository under
[`docs/cluster-capable-state-design.md`](https://github.com/HeliosSoftware/hfs/blob/main/docs/cluster-capable-state-design.md)
(discussion #223). This chapter is the operator-facing summary.

---

## Prerequisites

A cluster is only as shared as its slowest-moving state. Before starting a
second instance, you need:

1. **A shared primary database** — PostgreSQL, MongoDB, or Elasticsearch-backed
   modes (`HFS_STORAGE_BACKEND=postgres`, `postgres-elasticsearch`, `mongodb`,
   …). **SQLite cannot be clustered**: it is a single-writer local file, and
   `HFS_CLUSTER=true` refuses to boot on it.
2. **Shared object storage for bulk output** — set
   `HFS_BULK_EXPORT_OUTPUT_BACKEND=s3` and `HFS_BULK_SUBMIT_OUTPUT_BACKEND=s3`.
   The default `local-fs` writes to node-local disk, so a download routed to a
   different node than the writer returns 404.
3. **A shared replay/coordination store for auth** — Redis
   (`HFS_AUTH_REDIS_URL` + `HFS_AUTH_JTI_BACKEND=redis`) today; a
   database-backed alternative is planned so a cluster needs no extra
   dependency beyond the primary database.
4. **A shared audit sink** — `HFS_AUDIT_BACKEND=database` or `cloudwatch`.
   The `file` sink writes node-local NDJSON and produces a fragmented,
   restart-lossy audit trail.

---

## The one-switch setup: `HFS_CLUSTER=true`

Setting `HFS_CLUSTER=true` declares "this process is one of N". It does two
things:

1. **Flips cluster-safe defaults** — e.g. `HFS_JOB_STORE_BACKEND` defaults to
   `database` instead of `memory` (takes effect as the unified job store
   lands; see the status matrix below).
2. **Fails fast on unsafe combinations** instead of silently degrading. The
   server refuses to boot, printing one `Configuration error:` line per
   violation:

| Check | Refused when clustered | Fix |
|-------|------------------------|-----|
| Primary backend | `HFS_STORAGE_BACKEND=sqlite` (or `sqlite-elasticsearch`) | Use `postgres`, `mongodb`, or an `*-elasticsearch` mode over them |
| JWT replay cache | `HFS_AUTH_JTI_BACKEND=memory` (with auth enabled) | Use `redis` (shared) — `disabled` also boots, but replay protection is then off everywhere |
| Bulk export output | `HFS_BULK_EXPORT_OUTPUT_BACKEND=local-fs` (with bulk export enabled) | Use `s3` |
| Bulk submit output | `HFS_BULK_SUBMIT_OUTPUT_BACKEND=local-fs` (with bulk submit enabled) | Use `s3` |
| Audit sink | `HFS_AUDIT_BACKEND=file` (with audit enabled) | Use `database` or `cloudwatch` |
| Job store | explicit `HFS_JOB_STORE_BACKEND=memory` | Remove it or set `database` |
| Subscriptions fan-out | explicit `HFS_SUBSCRIPTIONS_FANOUT=memory` (with subscriptions enabled) | Remove it (defaults to `pg-notify` when clustered) or set `pg-notify` |
| Subscriptions primary | subscriptions enabled on a non-PostgreSQL primary | Use a `postgres` primary (the fan-out and shared delivery state ride the primary database) |

With `HFS_CLUSTER` unset (the default), nothing changes: a single instance
keeps its zero-configuration SQLite defaults.

A minimal two-instance setup on one shared PostgreSQL:

```bash
export HFS_CLUSTER=true
export HFS_STORAGE_BACKEND=postgres
export HFS_DATABASE_URL=postgresql://…      # same URL on every instance
export HFS_BULK_EXPORT_OUTPUT_BACKEND=s3
export HFS_BULK_SUBMIT_OUTPUT_BACKEND=s3
export HFS_AUDIT_BACKEND=database
export HFS_AUTH_JTI_BACKEND=redis           # when auth is enabled
export HFS_AUTH_REDIS_URL=redis://…

hfs --port 8080 &   # instance A
hfs --port 8081 &   # instance B (in practice: another machine/pod)
```

---

## Per-subsystem behavior with 1 vs N instances

The cluster-capable-state work lands in phases; this matrix reflects the
current state and is updated as each phase ships.

| Subsystem | 1 instance | N instances today | Cluster-safe since / planned |
|-----------|------------|-------------------|------------------------------|
| FHIR CRUD, search, history | ✅ | ✅ shared-database backends are stateless per request | already safe |
| Bulk Data `$export` / `$bulk-submit` (jobs) | ✅ | ✅ DB-leased jobs with fencing tokens; every instance runs competing workers | already safe (Postgres primary) |
| Bulk output download | ✅ | ✅ with `s3` output; ❌ with `local-fs` | F2 — enforced by fail-fast |
| Concurrent cold-start schema init | ✅ | ✅ serialized by a Postgres advisory lock | fixed 2026-07-14 (D3) |
| SQL-on-FHIR `$viewdefinition-export` (async jobs) | ✅ | ⚠️ **single-instance only** — job state is in-memory; poll/cancel/download on another instance 404s | Phase 1 (#169, unified job store) |
| Search reindex jobs | ✅ | ⚠️ single-instance only (in-memory job registry) | Phase 1 |
| JWT `jti` replay protection | ✅ | ⚠️ `memory` is per-node (replay possible on other nodes) — refused by fail-fast; use `redis` | Phase 2 hardens defaults, adds `database` |
| JWKS refresh | ✅ | ✅ functionally (each node fetches the same keys); redundant IdP fetches | Phase 2 (coordinated refresh) |
| Subscriptions (topics, delivery, WebSockets) | ✅ | ✅ with `HFS_SUBSCRIPTIONS_FANOUT=pg-notify` (the default under `HFS_CLUSTER`, Postgres primary): registries hydrate from the database at boot and stay in sync over LISTEN/NOTIFY, `eventNumber`/failure counters are shared (migration v17), WebSocket binding tokens mint/redeem on any instance, notifications reach sockets on every instance, and push-channel deliveries (rest-hook/email/message) run on a durable leased outbox that survives restarts | Phase 3 (#170) — landed |
| Terminology (HTS) response caches | ✅ | ⚠️ a terminology import on one instance leaves stale `$expand`/`$validate-code` answers on the others | Phase 4 |
| HTS bootstrap import | ✅ | ⚠️ N cold-starting instances all run the heavy import (correct but N× cost) | Phase 4 |
| Composite async search sync | ✅ | ⚠️ in-process queue; a crash loses queued secondary-index writes | Phase 4 (durable outbox) |
| Audit, metrics, health | ✅ | ✅ with shared sinks; `/metrics` is per-instance (aggregate in your scraper) | already safe |

**⚠️ single-instance-only features (this fork):** pin SQL-on-FHIR async
exports and `$reindex` status polling to one instance (or route their requests
to one instance with sticky sessions) — the same guidance issues #169 carries.
Subscriptions are cluster-safe with `HFS_SUBSCRIPTIONS_FANOUT=pg-notify`
(Postgres primary). See `docs/ops/cluster.md` for the fork's operator guide.

### Subscriptions cluster delivery (Phase 3)

`HFS_SUBSCRIPTIONS_FANOUT` selects the behavior (with subscriptions enabled):

| Value | Effect |
|-------|--------|
| *(unset)* | `pg-notify` under `HFS_CLUSTER=true`, `memory` otherwise |
| `memory` | Per-instance registries and delivery — single-instance only (refused under `HFS_CLUSTER=true`) |
| `pg-notify` | Cluster delivery over the PostgreSQL primary: shared counters/status/outbox tables (migration v17) plus a LISTEN/NOTIFY fan-out on a dedicated connection. Non-Postgres primaries log a warning and stay per-instance |

How the pieces behave, and the guarantees they carry:

- **Registries** (SubscriptionTopic + Subscription, including the R4
  backport `Basic` form) hydrate from the database at startup and stay in
  sync at runtime via lifecycle announcements — a Subscription created on
  any instance fires for matching writes served by every instance. Topics
  are tenant-scoped as of Phase 3: one tenant's SubscriptionTopic is no
  longer resolvable by another tenant's Subscription.
- **`eventNumber` is cluster-wide monotonic and gap-free** (a shared
  counter row per subscription); resource updates never reset it.
  Consecutive delivery failures accumulate across instances, so the
  `error`/`off` status thresholds work no matter which instance failed.
- **WebSockets**: `$get-ws-binding-token` mints into a shared single-use
  table, so the WebSocket upgrade may land on any instance. Notifications
  produced anywhere reach sockets everywhere via the fan-out. Delivery to
  *remote* sockets is best-effort: envelopes published while an instance's
  listen connection is down are lost, the instance re-syncs on reconnect,
  and clients detect gaps via `eventNumber` (the FHIR WebSocket channel
  contract).
- **Push channels** (rest-hook, email, message) deliver from a durable
  outbox with worker leases and persisted retry backoff — a redeploy no
  longer drops pending retries. Semantics are **at-least-once**: endpoints
  should tolerate a duplicate delivery after a worker crash (dedupe by
  `eventNumber`).
- **Pooler caveat**: the fan-out LISTENs on a dedicated session-level
  connection. A transaction-pooling proxy (e.g. pgbouncer in transaction
  mode) between HFS and PostgreSQL breaks LISTEN/NOTIFY — connect the
  instances directly, or use session pooling.
- The subscription heartbeat interval remains unwired; if it lands later it
  must be lease-guarded, not a per-instance timer.

---

## Load-balancer notes

- **Round-robin is fine for CRUD** — any instance answers any request against
  the shared database.
- **Sticky sessions are no longer needed for Subscriptions** (Phase 3): a
  WebSocket client pinned to instance B receives notifications for writes
  served by instance A via the pg-notify fan-out, and binding tokens redeem
  on any instance.
- **WebSocket upgrades** need the usual `Upgrade`/`Connection` header
  pass-through on the proxy.
- **Health checks**: probe each instance's `/health` directly, not through
  the balancer, so one bad instance can't mask another.

---

## Verifying a cluster

The repository ships a two-instance smoke harness — two `hfs` processes
sharing one PostgreSQL behind an nginx round-robin front
(`.github/workflows/cluster-smoke.yml`). It asserts health on both instances
and the front, real round-robin distribution, and cross-instance visibility
(write on A, read on B). The same checks are easy to run by hand against any
deployment: create a resource through the balancer, then `GET` it repeatedly
and confirm every instance serves it.
