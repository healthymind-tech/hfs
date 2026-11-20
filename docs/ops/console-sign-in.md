# Console sign-in, users and roles

The HFS web console (`/ui`) supports three sign-in modes (design
`docs/design/fhir-server-manager.md` §7, package P3):

| Mode | Entry to `/ui` | Who gets which role |
|---|---|---|
| `none` (default) | open, as before | visitors get `anonymousRole` (default `viewer`; `none` = sign-in required); admin pages need the break-glass administrator |
| `oidc` | **every page needs a session**: sign-in page → "Sign in with single sign-on" → IdP (Authorization Code + PKCE S256, `state`, `nonce`) | role from a token claim (`roleClaim`, e.g. `realm_access.roles` or `groups`) through the role mapping; `defaultRole` when nothing matches, else "no access" |
| `local` | **every page needs a session**: user name / password form | per built-in account (argon2id), set by admins |

Roles: `viewer` (browse everything, secrets never shown) < `editor` (+ write
FHIR data through the console: editor, batch, saved queries) < `admin` (+
server administration: settings, users, endpoints, IG packages, tenants,
bulk import/export, SQL export, sync). Per-endpoint bindings replace the
global role on one endpoint (`/ui/e/{endpoint}/…`); server administration
always needs the global admin role.

**Break-glass.** The `HFS_ADMIN_PASSWORD` administrator (user
`HFS_ADMIN_USER`, default `admin`) can sign in in every mode at
`/ui/login?local=1` (the sign-in page links it), unless `breakGlass` is
turned off. Its session also carries the legacy `hfs_admin` cookie, so the
admin API and the FHIR API keep accepting it as before. In mode `none`
break-glass cannot be turned off (it is how administrators sign in there).

## Switching it on

Console sign-in is installed when `HFS_MANAGER=on` or when
`HFS_CONSOLE_AUTH_MODE` is set explicitly, and the UI is served. Without
either, the console behaves exactly as before.

Settings live in the database (`hfs_console_settings`) and are edited by
admins at **Settings → Console sign-in** (`/ui/settings/auth`) or through
`PUT /admin/console/settings`. Environment variables bootstrap them and,
when set, **pin** their field (shown read-only with a 🔒 and the variable
name; a save that changes it is refused) — so a broken configuration can
always be repaired from the environment and a restart.

| Variable | Field | Notes |
|---|---|---|
| `HFS_CONSOLE_AUTH_MODE` | `mode` | `none` \| `oidc` \| `local` |
| `HFS_CONSOLE_ANONYMOUS_ROLE` | `anonymousRole` | `viewer` \| `editor` \| `none` (`admin` only with `HFS_CONSOLE_ALLOW_ANONYMOUS_ADMIN=true`, development) |
| `HFS_CONSOLE_OIDC_ISSUER` | `oidc.issuer` | discovery at `{issuer}/.well-known/openid-configuration` |
| `HFS_CONSOLE_OIDC_CLIENT_ID` | `oidc.clientId` | e.g. `hfs-web` |
| `HFS_CONSOLE_OIDC_CLIENT_SECRET` / `_FILE` | `oidc.clientSecret` | confidential clients; never stored in the DB. A secret typed in the console is stored AES-256-GCM sealed and needs `HFS_SECRET_KEYS` |
| `HFS_CONSOLE_OIDC_SCOPES` | `oidc.scopes` | default `openid profile email` |
| `HFS_CONSOLE_OIDC_ROLE_CLAIM` | `oidc.roleClaim` | dotted path; ID token first, then the access token |
| `HFS_CONSOLE_OIDC_ADMIN_VALUES` / `_EDITOR_VALUES` / `_VIEWER_VALUES` | `oidc.roleMapping.*` | comma lists; the highest match wins |
| `HFS_CONSOLE_OIDC_DEFAULT_ROLE` | `oidc.defaultRole` | role when nothing matches; `none` = no access |
| `HFS_CONSOLE_SESSION_IDLE_MINUTES` | `sessionIdleMinutes` | default 480 |
| `HFS_CONSOLE_SESSION_ABSOLUTE_HOURS` | `sessionAbsoluteHours` | default 24 |
| `HFS_CONSOLE_BREAK_GLASS` | `breakGlass` | default `true` |
| `HFS_CONSOLE_PASSWORD_MIN_LENGTH` | `local.minPasswordLength` | default 12, never below 8 |
| `HFS_CONSOLE_LOCKOUT_FAILURES` / `_LOCKOUT_MINUTES` | `local.lockout*` | default 5 failures / 15 min, per client address **and** per account |
| `HFS_CONSOLE_ALLOWED_ORIGINS` | `allowedOrigins` | extra origins accepted on writes |
| `HFS_CONSOLE_PUBLIC_URL` | — | the console's public origin (default: the origin of `HFS_BASE_URL`); the OIDC redirect URI is `{origin}/ui/callback` |
| `HFS_CONSOLE_RECHECK_MS` | — | how long an instance trusts a session/settings snapshot (default 10000) — the cluster revocation bound |

Legacy `HFS_UI_LOGIN_CLIENT_ID` / `_SECRET` / `_SCOPES` with
`HFS_AUTH_ISSUER` (upstream #1449) are read as the OIDC bootstrap when the
`HFS_CONSOLE_OIDC_*` variables are unset.

**Switching modes in the console** is refused unless it can work:
`local` needs at least one enabled admin account; `oidc` needs a passed
**Test sign-in** of the saved OIDC client in the admin's own browser session
(save the client with the mode unchanged → "Test sign-in" → the IdP round
trip → switch). Through the REST API a switch to `oidc` needs a reachable
discovery document instead. URLs typed in the console (issuer, token
endpoint) pass the outbound SSRF policy (`HFS_OUTBOUND_ALLOW` for internal
IdPs); URLs from the environment are trusted.

### Keycloak example (realm `hfs-test`)

```sh
HFS_MANAGER=on
HFS_CONSOLE_AUTH_MODE=oidc
HFS_CONSOLE_OIDC_ISSUER=https://hfs.gugulu.tw/auth/realms/hfs-test
HFS_CONSOLE_OIDC_CLIENT_ID=hfs-web
HFS_CONSOLE_OIDC_CLIENT_SECRET_FILE=/run/secrets/hfs-web
HFS_CONSOLE_OIDC_ROLE_CLAIM=realm_access.roles
HFS_CONSOLE_OIDC_ADMIN_VALUES=hfs-admin
HFS_CONSOLE_OIDC_EDITOR_VALUES=hfs-editor
HFS_CONSOLE_OIDC_DEFAULT_ROLE=viewer
```

Register `https://hfs.gugulu.tw/ui/callback` as a valid redirect URI of the
client and `https://hfs.gugulu.tw/ui/login?signed_out=1` as a valid
post-logout redirect URI.

## Accounts (mode `local`)

**Settings → Users** (`/ui/settings/users`) or `/admin/console/users`:
create (a generated one-time password is shown once, or set one), change
role, disable (ends the account's sessions), reset password (new one-time
password, sessions end), delete. Accounts created by an admin must change
their password at first sign-in (`/ui/account/password`). The last enabled
admin cannot be disabled, demoted or deleted. The user name of the
break-glass administrator is reserved.

Passwords: argon2id (`m=19456 KiB, t=2, p=1`), at least the minimum length,
not containing the user name, not one repeated character, not on the list
of the 10,000 most common passwords (shipped in the binary). Failed
sign-ins are counted per client address and per account; after the limit
the client/account is refused for the lockout window, even with the right
password. Error messages are generic (no user enumeration).

First start in mode `local` with no accounts: only break-glass can sign in;
create the first admin account at Settings → Users.

## Sessions

Cookie `hfs_console` (`Path=/; HttpOnly; SameSite=Strict`, `Secure` over
HTTPS — `HFS_ADMIN_COOKIE_SECURE` applies), a random 256-bit token; only its
SHA-256 is stored, in `login_sessions` (`kind='console'`) on PostgreSQL —
shared by every instance and surviving restarts — else in memory. Idle and
absolute lifetimes as configured. A role change, a disabled account or a
mode switch reaches a session on its next request on the instance that made
the change and within `HFS_CONSOLE_RECHECK_MS` elsewhere. OIDC sessions keep
the IdP tokens server-side, refresh the access token when it expires (and
re-map the role; no role → the session ends), and sign-out sends the
browser to the IdP's end-session endpoint. Admins see and end live
sessions at Settings → Users.

## CSRF

1. `SameSite=Strict` session cookie.
2. Every state-changing request made with the cookie must carry an `Origin`
   (else `Referer`) of this site: the request host, the console's public
   origin, `HFS_ADMIN_ALLOWED_ORIGINS` or `allowedOrigins`.
3. And the session's synchronizer token: header `X-CSRF-Token` or form
   field `_csrf`. The layout loads `/ui/assets/csrf.js`, which adds it to
   htmx requests, same-origin `fetch` writes and post forms from the
   `hfs_csrf` cookie (readable by the page only, `SameSite=Strict`); the
   server compares it with the token stored in the session.

Requests with `Authorization` (scripts) are not cookie-authenticated and
need no token.

## Admin API

```text
GET|PUT /admin/console/settings              PUT needs If-Match: "<revision>"; secrets via {"secrets": {"oidc.clientSecret": {"set": "…"} | "clear"}}
POST    /admin/console/settings/$test-oidc   discovery probe
GET|POST /admin/console/users ; GET|PUT|DELETE /admin/console/users/{id}   (PUT: If-Match)
POST    /admin/console/users/{id}/$reset-password
GET     /admin/console/bindings ; PUT|DELETE /admin/console/bindings/{subject}/{endpoint}
GET     /admin/console/sessions ; DELETE /admin/console/sessions/{key}
```

Credentials: a console admin session (with the CSRF token on writes), the
break-glass `Authorization: Basic`/`Bearer <password>`, or — with
SMART/OIDC on — a bearer JWT validated like a FHIR token that holds
`HFS_ADMIN_TOKEN_SCOPE` (default `system/*.cruds`; it acts as
`token:{sub}`). Errors are OperationOutcomes (`422` per field with
`expression`, `409` rule/conflict, `412` stale `If-Match`, `428` without
it).

The endpoint admin API (`/admin/endpoints/**`) admits console sessions by
role: viewers read (redacted), endpoint writes need admin on that endpoint
(a binding may grant it), creating/deleting endpoints, `$rewrap` and
everything else need the global admin role.

## The FHIR API from the console

A console session is also a credential on the FHIR API (`/{endpoint}/…`)
when the request carries no `Authorization`: the console principal gets
the scopes of `role_for(endpoint)` limited by the endpoint's
`consoleAccess` (`off` = no credential, `read-only` = `system/*.rs`,
`role-based` = viewer `*.rs`, editor `*.cruds`, admin `*.cruds` + purge,
reindex, bulk-submit). Writes need a same-site `Origin`.

## HTS console

HTS keeps its own administrator password. `HTS_CONSOLE_REQUIRE_LOGIN=true`
puts **all** of `/ui/hts` behind the HTS admin session (sign-in at
`/ui/hts/login`), so the visible console has no public area while HFS runs
in mode `oidc` or `local`. It defaults to on when the HTS process sees
`HFS_CONSOLE_AUTH_MODE=oidc|local` (shared environment), else off. With it
on and no `HTS_ADMIN_PASSWORD`, the console refuses every page (`503`) —
it never runs open. HTS single sign-on with the console's OIDC client is a
later step.
