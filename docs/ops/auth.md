# SMART on FHIR / OIDC authentication and authorization (HFS and HTS)

HFS is a **resource server**: it never issues tokens. An OpenID Connect
identity provider (Keycloak, Entra ID, Okta, Auth0, …) signs in users and
clients and issues JWT access tokens; HFS validates them locally against the
IdP's JWKS and enforces SMART App Launch scopes. HTS accepts the same tokens
for terminology writes. The administrator password
([admin-sign-in.md](admin-sign-in.md)) keeps working alongside tokens.

This page covers configuration, what is enforced, a Keycloak setup, the
reverse proxy, and the rollout plan for `hfs.gugulu.tw`.

## 1. Modes

`HFS_AUTH_MODE` decides how much of the FHIR API needs a credential:

| Mode | Anonymous request | Request with a token / admin credential |
|---|---|---|
| `off` (default) | everything allowed | token ignored |
| `write-protect` | reads and searches allowed (`GET`/`HEAD`, `POST …/_search`, `$validate`, `$everything`); every write, batch/transaction, bulk export, job status, `$sql-*`, `_user/*` → `401` | validated and scope-checked exactly as in `full` |
| `full` | `401` (except discovery, `/metadata`, health) | validated and scope-checked |

`HFS_AUTH_ENABLED=true` without `HFS_AUTH_MODE` means `full` (backwards
compatible). An explicit `HFS_AUTH_MODE` wins over `HFS_AUTH_ENABLED`.

In both enforcing modes **a token that is presented is always validated**: a
bad token gets `401` even on a request that would have been allowed
anonymously, so a client never silently runs with less access than it thinks.

## 2. Configuration reference

### Token validation

| Variable | Default | Meaning |
|---|---|---|
| `HFS_AUTH_MODE` | `off` | `off`, `write-protect`, `full` (§1) |
| `HFS_AUTH_ISSUER` | — (required) | Exact `iss` of the IdP's tokens. Required when auth is on: it pins whose tokens are accepted and qualifies every user identity. |
| `HFS_AUTH_JWKS_URL` | — (required) | The IdP's JWKS (`…/protocol/openid-connect/certs` on Keycloak). |
| `HFS_AUTH_AUDIENCE` | — | Required `aud`. **Set it in production**: without it any token the issuer mints for any other client is accepted. A token without `aud` is refused when this is set. |
| `HFS_AUTH_ALGORITHMS` | `RS256,RS384,ES256,ES384` | Accepted signing algorithms (`none` and HMAC are never accepted). |
| `HFS_AUTH_CLOCK_SKEW_SECONDS` | `60` | Leeway for `exp` and `nbf`. 30–60 s is normal; keep server clocks on NTP. |
| `HFS_AUTH_JWKS_MIN_REFRESH_INTERVAL` | `10` | Minimum seconds between JWKS fetches triggered by an unknown `kid` (protects the IdP from token-spraying). |
| `HFS_AUTH_JWKS_MAX_AGE_SECONDS` | `300` | Longest time a fetched JWKS is trusted. Bounds how long a key the IdP retired (or revoked after a compromise) still validates here. Keycloak sends no cache lifetime, so this is the effective refresh period. |
| `HFS_AUTH_TENANT_CLAIM` | `tenant_id` | Claim that pins the tenant (multi-tenant deployments). |
| `HFS_AUTH_PATIENT_NON_COMPARTMENT` | `allow` | What `patient/` scopes may *read* outside the Patient compartment (Medication, Practitioner, Organization, Location, …): `allow` (US Core apps need them) or `deny`. Writes there are never allowed for `patient/` scopes. |

Startup is resilient: if the IdP is unreachable when HFS starts, HFS still
starts, logs a warning and answers token-bearing requests with
`503 Retry-After: 5` until the keys can be fetched (background retry and
refetch on first use). Requests without tokens in `write-protect` keep
working.

### SMART discovery (`/.well-known/smart-configuration`, CapabilityStatement)

`token_endpoint` / `authorization_endpoint` (and the end-session endpoint)
are read from the issuer's `/.well-known/openid-configuration` at startup
when not set explicitly. `GET /metadata` carries the `SMART-on-FHIR` service
coding and the `oauth-uris` extension.

| Variable | Default | Meaning |
|---|---|---|
| `HFS_SMART_TOKEN_ENDPOINT`, `HFS_SMART_AUTHORIZE_ENDPOINT` | discovered | Override the discovered endpoints. |
| `HFS_SMART_JWKS_URL`, `HFS_SMART_INTROSPECTION_ENDPOINT`, `HFS_SMART_REVOCATION_ENDPOINT`, `HFS_SMART_REGISTRATION_ENDPOINT`, `HFS_SMART_MANAGEMENT_ENDPOINT`, `HFS_SMART_END_SESSION_ENDPOINT` | — | Advertised as given. |
| `HFS_SMART_CAPABILITIES` | launch-standalone, context-standalone-patient, client-public, client-confidential-symmetric, client-confidential-asymmetric, sso-openid-connect, permission-offline, permission-online, permission-patient, permission-user, permission-v1, permission-v2 (launch ones only with an authorize endpoint) | Comma list. Advertise only what the IdP really supports (e.g. drop `context-standalone-patient` if users have no `patient` attribute). |
| `HFS_SMART_SCOPES_SUPPORTED` | openid, fhirUser, profile, launch/patient, offline_access, online_access, patient/\*.rs, patient/\*.read, user/\*.cruds, user/\*.rs, user/\*.read, user/\*.write, system/\*.cruds, system/\*.rs, system/\*.read | Comma list. |
| `HFS_SMART_TOKEN_AUTH_METHODS` | client_secret_basic, client_secret_post, private_key_jwt | Comma list. |
| `HFS_SMART_GRANT_TYPES` | authorization_code, client_credentials, refresh_token | Comma list. |

### Rate limiting

| Variable | Default | Meaning |
|---|---|---|
| `HFS_RATE_LIMIT_ANONYMOUS` | `0` (off) | Requests per minute per client IP without credentials. |
| `HFS_RATE_LIMIT_AUTHENTICATED` | `0` (off) | Requests per minute per OAuth client (`client_id`/`azp`, else issuer + subject). |
| `HFS_RATE_LIMIT_BURST_SECONDS` | `10` | Seconds of budget a quiet client may spend at once (token bucket). |
| `HFS_RATE_LIMIT_TRUSTED_PROXIES` | loopback + private ranges | CIDRs whose `X-Forwarded-For` is believed; the right-most untrusted hop is the client. `none` = trust no proxy. |

Over the limit: `429` + `Retry-After` + `OperationOutcome` (`throttled`).
Health probes and the administrator credential are never limited. Limits are
per HFS process (a cluster of N nodes allows N × the limit); for a hard
global ceiling use nginx `limit_req` too (§6).

### Administrator credential alongside tokens

With `HFS_ADMIN_PASSWORD` set, the admin credential — `Authorization: Basic`
(`curl -u admin:…`), `Authorization: Bearer <password>`, or the admin session
cookie of the web console (same-site writes only) — is accepted on the FHIR
API **and** the admin API as an administrator holding every scope. A bearer
that looks like a JWT (`eyJ….….…`) always goes to IdP validation instead.
Failed admin attempts count toward the admin lockout (5 per 15 min per
client). This is what keeps the web console's editing pages, scripts such as
`test_validator.py`, and IG installs working in `write-protect` mode without
an IdP login in the console.

The `/admin/*` API and the cross-tenant console metrics accept either the
admin credential or a token with `system/*.r` (or broader).

## 3. What is enforced

### Scopes

* SMART **v2** (`patient/Observation.rs`, `user/*.cruds`, `system/*.rs`) and
  SMART **v1** (`.read` = `rs`, `.write` = `cud`, `.*` = `cruds`) are both
  accepted, from the `scope` claim (string) or `scp` (array, Okta/Entra).
* Operation→permission: read/vread/history = `r`, search = `s`, create = `c`,
  update/PATCH = `u`, delete = `d`. `$everything`, `$export` and `$validate`
  need `r` on their type; `$purge`/`$reindex`/`$bulk-submit` need the
  operation scopes `system/purge`, `system/reindex`, `system/bulk-submit`.
* System-wide reads — `GET /_history`, `GET|POST /$export` without `_type`,
  `/$sql-run`, `/$sql-export`, `/$nl-search`, `/$meta` — need a scope that
  reads every type (`system/*.r…` or `user/*.r…`).
* Batch/transaction entries are checked one by one.
* **Type leakage through bundles is closed**: entries of a type the token may
  not read are removed from `_include`/`_revinclude` results, `$everything`
  and searches nested in a batch response.
* `user/` scopes reach everything the scope names (HFS has no per-practitioner
  ACL; grant narrower `user/` scopes in the IdP for narrower roles).

### Patient context (`patient/` scopes)

The launch patient is the token's `patient` claim (SMART launch context).
A `patient/` scope without a `patient` claim reaches nothing (`403`).

| Request | Behaviour |
|---|---|
| `GET /T?…`, `POST /T/_search` (T in the Patient compartment) | rewritten to the compartment search `/Patient/{id}/T?…` |
| `GET /Patient?…` | `_id={id}` is added |
| `GET /T/{id}`, vread, instance history | the returned resource (each version) must be in the compartment, else `403` / dropped |
| `GET /Patient/{other}/T`, `/Patient/{other}/$everything`, `/Patient/$everything` | `403` |
| `_include` / `_revinclude` | entries outside the compartment are removed |
| `GET /T/_history`, `GET /_history`, batch/transaction, PATCH, conditional update/delete, other `$`-operations | `403` (cannot be confined cheaply; send individual requests) |
| create / update | the submitted resource must be in the compartment; for update the stored one too (no moving data between patients) |
| delete | the stored resource must be in the compartment |
| Types outside the compartment (Medication, Practitioner, …) | readable per `HFS_AUTH_PATIENT_NON_COMPARTMENT` |
| XML responses | `403` when the response would need filtering (ask for JSON) |

Compartment membership follows the R4 `CompartmentDefinition` for Patient
(every member parameter, e.g. Observation `subject` and `performer`), the same
definition `/Patient/{id}/*` and `Patient/$export` use.

### SMART v2 granular scopes

`patient/Observation.rs?category=http://terminology.hl7.org/CodeSystem/observation-category|laboratory`:
searches get the constraint appended (several granular scopes on the same
parameter are OR-ed: `category=a,b`); reads and included resources must match
it; writes are checked against it. Token parameters (`system|code`, `code`,
`|code`), strings and references are supported; a constraint on a parameter
HFS does not know fails closed. Granular scopes constraining *different*
parameters for one type cannot be combined into one search (`403` with a
message).

### Responses

* `401` — missing / invalid / expired token: `WWW-Authenticate: Bearer
  realm="hfs"` (missing) or `Bearer realm="hfs", error="invalid_token",
  error_description="…"` (rejected), plus an `OperationOutcome` (`login`).
* `403` — insufficient scope or outside the compartment: `WWW-Authenticate:
  Bearer realm="hfs", error="insufficient_scope", …` + `OperationOutcome`
  (`forbidden`).
* `503 Retry-After` — the IdP's keys cannot be fetched.
* Every decision is recorded through the audit sink when `HFS_AUDIT_*` is on.

## 4. HTS (terminology server)

Terminology reads and the read-only operations (`$lookup`, `$validate-code`,
`$subsumes`, `$expand`, `$translate`, `$closure`, read-only batches) stay
**public**. Writes (CodeSystem/ValueSet/ConceptMap create/update/delete,
`/import`, write batches) need the HTS administrator credential (as before)
**or** a SMART token:

| Variable | Meaning |
|---|---|
| `HTS_AUTH_JWKS_URL` | IdP JWKS; enables token acceptance for writes |
| `HTS_AUTH_ISSUER` | required with it |
| `HTS_AUTH_AUDIENCE` | recommended |
| `HTS_AUTH_ALGORITHMS`, `HTS_AUTH_CLOCK_SKEW_SECONDS` | as for HFS |

A token may write a type when an unconstrained `system/` or `user/` scope
grants it (`system/CodeSystem.cud`, `system/*.cruds`, `system/*.write`);
`/import` and write batches need create+update+delete on all three types.
`patient/` scopes never write terminology. With `HTS_AUTH_JWKS_URL` set and
no `HTS_ADMIN_PASSWORD`, writes require a token (`HTS_REQUIRE_AUTH_FOR_WRITES`
defaults to on).

HFS → HTS (IG installs) keeps using `HFS_TERMINOLOGY_SERVER_TOKEN` (the HTS
admin password).

## 5. Keycloak setup

The repository ships a ready realm: `docker/keycloak/realm.json` (realm
`fhir`). Locally:

```bash
docker compose -f docker/keycloak/docker-compose.yml up -d   # :8180, admin/admin
```

What the realm contains, and what to replicate on a production Keycloak:

1. **Client scopes**, one per SMART scope you grant, with *Include in token
   scope* on: `system/*.cruds`, `system/*.rs`, `system/Patient.rs`,
   `user/*.cruds`, `user/*.rs`, `patient/*.rs`, `patient/*.read`,
   `launch/patient`, `offline_access`, granular ones such as
   `patient/Observation.rs?category=http://terminology.hl7.org/CodeSystem/observation-category|laboratory`.
   Keycloak issues a scope only if it is assigned to the client (Default:
   always; Optional: when requested).
2. **Audience**: on every client that calls HFS, an *Audience* mapper
   (`oidc-audience-mapper`) with included custom audience = your
   `HFS_AUTH_AUDIENCE` (the realm uses `hfs-api`).
3. **`sub`**: keep the built-in `basic` scope assigned (Keycloak 26 puts `sub`
   there; HFS refuses tokens without `sub`).
4. **Backend services** (integrations, ETL, HIS interface engine): a
   confidential client with *Service accounts* on, *Standard flow* off,
   default scopes such as `system/*.rs` or `system/Observation.cruds`. Prefer
   `private_key_jwt` (Credentials → Client Authenticator "Signed JWT", import
   the integration's public JWKS URL) over a client secret.
5. **Patient-facing app** (standalone launch): client with *Standard flow* on,
   PKCE `S256`, default scopes `basic profile patient/*.rs`, and a *User
   Attribute* mapper `patient` → claim `patient` (access token). Each patient
   user gets attribute `patient=<FHIR Patient id>`. Keycloak 24+ drops
   attributes that are not in the user profile: either declare `patient` in
   Realm settings → User profile, or set *Unmanaged attributes* to Enabled.
   (Keycloak has no SMART patient picker; for an EHR-style picker put a SMART
   launch proxy in front of Keycloak.)
6. **Clinician app**: standard flow + PKCE, default scopes `user/*.rs` (or
   finer), no patient mapper.
7. **Web console login** (optional): public client `hfs-web`, redirect
   `https://hfs.gugulu.tw/ui/callback`, scopes `basic profile email
   user/*.cruds`; `HFS_UI_LOGIN_CLIENT_ID=hfs-web` (see crates/auth/README.md).
8. **Tokens**: access token lifespan 5–15 min; refresh tokens via
   `offline_access` only for apps that need them. Keycloak rotates realm keys
   by adding a key provider with higher priority, then deleting the old one
   after the longest token lifetime — HFS picks up the new `kid` immediately
   and stops trusting the deleted key within `HFS_AUTH_JWKS_MAX_AGE_SECONDS`.

HFS settings for that realm:

```bash
HFS_AUTH_MODE=full
HFS_AUTH_ISSUER=https://idp.example.org/realms/fhir
HFS_AUTH_JWKS_URL=https://idp.example.org/realms/fhir/protocol/openid-connect/certs
HFS_AUTH_AUDIENCE=hfs-api
```

Other IdPs: Entra ID (`iss` = `https://login.microsoftonline.com/{tenant}/v2.0`,
scopes in `scp`/`roles` — map app roles named like SMART scopes), Okta
(custom authorization server, scopes in `scp`), Auth0 (API identifier =
audience, issuer with trailing `/`). See `crates/auth/README.md`.

Test it: `curl -s $REALM/protocol/openid-connect/token -d grant_type=client_credentials -d client_id=… -d client_secret=…`
then `curl -H "Authorization: Bearer $T" https://…/fhir/Patient`. The
end-to-end check used for this release is
`hfs-eval/eval/scripts/p3-auth/e2e_keycloak.py`.

## 6. Reverse proxy / TLS termination (nginx)

Tokens are bearer credentials: **TLS is mandatory** on every hop that leaves
the host. Terminate TLS at the edge (nginx, or the existing edge proxy) and
keep HFS/HTS on a private network.

```nginx
server {
    listen 443 ssl http2;
    server_name hfs.example.org;
    ssl_certificate     /etc/ssl/hfs/fullchain.pem;
    ssl_certificate_key /etc/ssl/hfs/privkey.pem;
    ssl_protocols TLSv1.2 TLSv1.3;
    add_header Strict-Transport-Security "max-age=31536000" always;

    # Never log bearer tokens (the default log format does not; custom
    # formats must not include $http_authorization).
    proxy_set_header Authorization $http_authorization;   # passed through untouched
    proxy_set_header Host              $host;
    proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto https;

    # Optional global ceiling in addition to HFS's per-client limits.
    # (limit_req_zone $binary_remote_addr zone=fhir:20m rate=30r/s; in http {})
    location /fhir/ {
        limit_req zone=fhir burst=60 nodelay;
        proxy_pass http://hfs:8080/;
    }
}
server { listen 80; return 301 https://$host$request_uri; }
```

* Set `HFS_BASE_URL=https://…/fhir` so discovery and links carry the public
  URL; set `HFS_RATE_LIMIT_TRUSTED_PROXIES` and `HFS_ADMIN_TRUSTED_PROXIES` to
  the proxies' addresses (on hfs.gugulu.tw also `162.38.2.210/32`).
* CORS for browser SMART apps: CORS is on by default with `HFS_CORS_ORIGINS=*`;
  in production list the app origins explicitly (`HFS_CORS_ORIGINS`);
  `Authorization` is in the default `HFS_CORS_HEADERS`.
* The IdP itself must also be served over HTTPS; its issuer URL must be the
  public one (`KC_HOSTNAME`), since `iss` is compared byte for byte.

## 7. Rollout plan for hfs.gugulu.tw

Today: `HFS_AUTH_*` unset (FHIR REST fully open), admin password on the
console and admin API, HTS writes need the admin password.

1. **Stage 0 — deploy with auth off** (this release, no behaviour change):
   new image, `HFS_AUTH_MODE` unset. Run the regression scripts.
2. **Stage 1 — stand up the IdP**: Keycloak (own PostgreSQL DB, HTTPS at
   e.g. `https://hfs.gugulu.tw/auth/` or a separate host), import
   `docker/keycloak/realm.json`, **change every client secret and remove the
   demo users and direct-access grants**, create real clients per
   integration. Verify discovery from the HFS host.
3. **Stage 2 — write-protect** (recommended first production setting):

   ```bash
   HFS_AUTH_MODE=write-protect
   HFS_AUTH_ISSUER=https://<idp>/realms/fhir
   HFS_AUTH_JWKS_URL=https://<idp>/realms/fhir/protocol/openid-connect/certs
   HFS_AUTH_AUDIENCE=hfs-api
   HFS_AUTH_CLOCK_SKEW_SECONDS=60
   HFS_AUTH_JWKS_MAX_AGE_SECONDS=300
   HFS_ADMIN_PASSWORD=${ADMIN_PASSWORD}        # unchanged: console + scripts
   HFS_RATE_LIMIT_ANONYMOUS=600
   HFS_RATE_LIMIT_AUTHENTICATED=6000
   HFS_RATE_LIMIT_TRUSTED_PROXIES=127.0.0.0/8,::1/128,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,162.38.2.210/32
   HTS_AUTH_ISSUER / HTS_AUTH_JWKS_URL / HTS_AUTH_AUDIENCE   # same IdP
   ```

   Anonymous reads (browsers, validators, demos) keep working; every write
   needs a token or the admin credential. Writers to switch first: the eval
   scripts (`test_hfs_server.py`, `import_ig.py` — pass `-u admin:…` or a
   backend token), the IG installer (already uses the admin session), and any
   HIS interface engine (give it a `system/` client). Watch the logs for
   `401` on writes for a week.
4. **Stage 3 — full**: once every reader that should keep access has a client
   (portal, apps, reporting), set `HFS_AUTH_MODE=full`. Anonymous reads now
   get `401`; `/metadata` and `/.well-known/smart-configuration` stay public.
   Optionally `HFS_UI_LOGIN_CLIENT_ID=hfs-web` for IdP login in the console.
5. **Rollback** at any stage: set `HFS_AUTH_MODE` back to the previous value
   and restart (no data migration involved).

Recommended production setting for a hospital: **`full`**, audience pinned,
access tokens ≤ 15 min, backend integrations on `private_key_jwt` with the
narrowest `system/` scopes, audit on (`HFS_AUDIT_*`), TLS everywhere, admin
password kept as break-glass in a secrets file (`HFS_ADMIN_PASSWORD_FILE`).

## 8. Limitations

* No token introspection: revocation of an access token takes effect when it
  expires (keep lifetimes short). DPoP/mTLS sender-constrained tokens are not
  supported.
* Patient-context tokens cannot use batch/transaction, PATCH, conditional
  writes, type/system history or other `$`-operations (refused, not silently
  widened).
* Rate limits are per process.
* `user/` scopes are not narrowed by the user's identity (no practitioner ACL).
* Keycloak has no built-in SMART EHR launch (`launch` parameter) or patient
  picker; standalone launch with a per-user `patient` attribute works.
