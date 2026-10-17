# Administrator sign-in (HFS and HTS)

Both servers can run an administrative surface: HFS installs and removes IG
packages and maintains tenants (`HFS_ADMIN_UI`), HTS uploads, imports and
deletes terminology (`HTS_ADMIN_UI`). Until SMART/OIDC admin scopes land,
that surface is protected by one administrator password per server
(crate `helios-admin-auth`). The public FHIR REST API and the terminology
operations are **not** affected; they stay as they are (and will get
SMART/OIDC separately).

## Configuration

Every variable exists for both servers, with the `HFS_` or `HTS_` prefix.

| Variable | Default | Meaning |
|---|---|---|
| `*_ADMIN_PASSWORD` | (none) | The administrator password. Use a long random value. |
| `*_ADMIN_PASSWORD_FILE` | (none) | A file holding the password (Docker/Kubernetes secrets). Wins over `*_ADMIN_PASSWORD`; a trailing newline is ignored. An unreadable or empty file locks administration. |
| `*_ADMIN_USER` | `admin` | The administrator user name. |
| `*_ADMIN_SESSION_IDLE_MINUTES` | `480` (8 h) | A browser session ends after this long without a request (and after 24 h, or the idle time if longer, in any case). Sessions live in memory: a restart signs everyone out. |
| `*_ADMIN_TRUSTED_PROXIES` | loopback and private ranges | Comma-separated IPs/CIDRs of reverse proxies whose `X-Forwarded-For` / `X-Real-IP` are believed for the sign-in rate limit. `none` trusts no proxy. |
| `*_ADMIN_ALLOWED_ORIGINS` | (none) | Extra origins (`https://host[:port]`) accepted on cookie-authenticated writes, for proxies that rewrite `Host`. HFS always accepts the origin of `HFS_BASE_URL`. |
| `*_ADMIN_COOKIE_SECURE` | `auto` | `auto`: the session cookie is `Secure` when the sign-in came over HTTPS (`X-Forwarded-Proto: https`, or the browser's `https://` `Origin`). `true` / `false` force it. |

**Fail closed.** With `*_ADMIN_UI=true` and no password (or a broken
password file) every administrative endpoint answers `503` and the sign-in
page explains which variable to set; the server logs a warning at startup.
It never runs open. A password configured with `*_ADMIN_UI=false` still
guards whatever the server classifies as administrative.

**HFS with SMART/OIDC on** (`HFS_AUTH_ENABLED=true`): the admin tier already
requires a system-scope bearer token and the console its own IdP session, so
the password is ignored (with a warning).

## What is protected

### HFS

| Path | Access |
|---|---|
| `/admin/packages/**`, `/admin/tenants/**` (also via `/fhir/admin/...` behind the reference nginx) | admin |
| `/console/metrics/tenants`, `/console/metrics/traffic` (cross-tenant) | admin |
| `/ui/packages/**`, `/ui/tenants/**`, `/ui/bulk-import/**`, `/ui/bulk-export/**`, `/ui/sql/export/**` | admin (pages and their actions) |
| Any other state-changing request under `/ui` (e.g. saving a ViewDefinition / SQL query / SQL view) | admin |
| Read-only previews under `/ui` (`.../lint`, `.../complete`, `.../run`, `.../document`, `/ui/lookup/*`, `/ui/editor/render`, `/ui/json-view/render`, `/ui/history/diff`, `/ui/capability-statement/json-expand`) and per-browser preferences (`/ui/version`, `/ui/tenant`) | public |
| Browsing pages (`/ui`, resources, queries, search parameters, compartments, capability statement, history, editor, subscriptions, …) | public |
| FHIR REST API, `/metadata`, `/console/metrics/uptime`, `/health`, `/metrics` | public (unchanged) |

### HTS

| Path | Access |
|---|---|
| `/admin/**` (`/tx/admin/...` behind nginx) and its console mount `/ui/hts/api/admin/**` | admin — includes job lists with requester addresses and user agents, the audit trail and the configuration |
| `/ui/hts/terminology/**`, `/ui/hts/jobs/**`, `/ui/hts/import/**` | admin (pages and their forms) |
| Any other state-changing request under `/ui/hts` except the workbenches | admin |
| Browsing CodeSystems / ValueSets / ConceptMaps / concepts, the `$lookup` `$validate-code` `$subsumes` `$expand` `$translate` workbenches, capability statement, home | public |
| The terminology API (`/CodeSystem`, `/ValueSet`, `/ConceptMap`, operations, `/import`, `/metadata`, `/health`) | public (unchanged) |

## Signing in

**Browsers.** Opening an admin page without a session redirects to the
sign-in page — `/ui/admin/login` (HFS) or `/ui/hts/login` (HTS) — and back
after a successful sign-in. htmx requests get `HX-Redirect` instead. The
account menu then names the administrator and offers **Sign out**.

The session cookie (`hfs_admin`, `Path=/`; `hts_admin`, `Path=/ui/hts`) is a
random 256-bit token, `HttpOnly; SameSite=Strict`, `Secure` over HTTPS. The
server stores only its SHA-256. Because of `SameSite=Strict`, following a
link to an admin page from another site asks you to sign in again in that
tab. HFS's cookie path is `/` because the IG Packages page's script calls the
package API under the FHIR base (`/admin/packages`, `/fhir/admin/packages`).

**CSRF.** Besides `SameSite=Strict`, every state-changing request made with
the session cookie must carry an `Origin` (or, failing that, `Referer`) of
this site — the request's `Host`, or an allowed origin. The sign-in and
sign-out forms are checked the same way. Any write whose `Origin` names
another site is refused whatever credential it carries.

**Scripts.** Send the credentials on each request:

```sh
curl -u admin:"$HTS_ADMIN_PASSWORD" https://example.org/tx/admin/jobs
curl -H "Authorization: Bearer $HFS_ADMIN_PASSWORD" https://example.org/fhir/admin/packages
curl -u admin:"$HTS_ADMIN_PASSWORD" -H 'content-type: application/json' \
  -d '{"source":{"type":"import-dir","path":"loinc/Loinc_2.83.zip"},"format":"loinc"}' \
  https://example.org/tx/admin/imports
```

Unauthenticated API requests get `401` with `WWW-Authenticate: Bearer`
(not `Basic`, so browsers never pop up a native dialog).

**Rate limit.** Five failed attempts (form or API) from one client address
within 15 minutes lock that address out for 15 minutes — `429` with
`Retry-After`, even for the right password. Failures are logged at `warn`
with the client address only; passwords and tokens are never logged.

The client address is the TCP peer, unless the peer is a trusted proxy
(`*_ADMIN_TRUSTED_PROXIES`, by default loopback and private ranges, which
covers nginx in the same Docker network). Behind a trusted proxy it is the
right-most `X-Forwarded-For` hop that is not itself trusted (left-most
entries can be forged by the client), else `X-Real-IP`. If another proxy
sits in front of nginx on a public address, add it to
`*_ADMIN_TRUSTED_PROXIES`, or every client shares its address — and one
attacker's failures lock everyone out. The address logged with each failure
shows what the server resolved.

**Audit.** HTS records the signed-in administrator (`X-Remote-User` is set
to the admin user on admitted requests; a client-supplied
`X-Forwarded-User` is dropped) together with the client address.

## Reverse proxy

The reference nginx (`absolute_redirect off`, `/ui` and `/ui/hts` passed
through unchanged, `/tx/` and `/fhir/` stripped) needs no change: the sign-in
pages live under the consoles' own prefixes and every redirect is a relative
path. nginx sets `X-Forwarded-Proto $scheme` for `/ui`, which is `http` when
TLS ends at an edge proxy in front of it; the `auto` cookie mode then relies
on the browser's `https://` `Origin` at sign-in, so no change is needed
either (set `*_ADMIN_COOKIE_SECURE=true` to force it).
