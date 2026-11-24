# Built-in authorization server — operations

Design: [`docs/design/authorization-server.md`](../design/authorization-server.md).
Crates: `helios-idp` (core, store), `helios-idp-server` (protocol endpoints),
`helios-idp-pages` (hosted pages, CSRF, lockout, rate limits, audit),
`helios-auth` (`internal.rs`: verification on FHIR endpoints).

> Status: phase 1, **not for production PHI** before the external penetration
> test (design D-14). Ship dark: `HFS_IDP=off` is the default.

## 1. Switch

| Variable | Default | Meaning |
|---|---|---|
| `HFS_IDP` | `off` | `on` mounts `/idp/*` and enables Internal(realm) endpoints |
| `HFS_IDP_PUBLIC_BASE` | origin of `HFS_BASE_URL` | public origin; issuers are `{base}/idp/{realm}` (no trailing slash) |
| `HFS_SECRET_KEYS` | — | required: realm signing keys are sealed with it; also keys the CSRF tokens so every instance accepts every other's forms |
| `HFS_IDP_TRUST_FORWARDED` | `true` | take the caller address from `X-Real-IP` / the last `X-Forwarded-For` hop (nginx). Set `false` when HFS is reachable without the proxy |
| `HFS_IDP_INTERNAL_URL` | `http://127.0.0.1:{HFS_SERVER_PORT}` | loopback base the patient picker uses to search the FHIR API as the signed-in practitioner |
| `HFS_IDP_RATE_TOKEN_CLIENT` / `_TOKEN_IP` / `_INTERACTIVE_IP` / `_INTROSPECT_CLIENT` / `_REVOKE_CLIENT` | 60 / 300 / 20 / 600 / 60 | per-minute token buckets (0 = unlimited), per instance |
| `HFS_IDP_DEV_SEED` | — | **test stacks only**: JSON file of realms/clients/users created at start-up when missing (`crates/idp-pages/e2e/seed.json`) |
| `HFS_IDP_DEV_SEED_SECRETS` | — | **test stacks only**: file (created mode 0600) that receives the seed's generated client secrets as `client_id<TAB>secret`; without it they are discarded — never logged |

With `HFS_IDP=off` nothing answers under `/idp` (404) and an endpoint whose
auth provider is `internal` answers **503** to every token — closed, never open.

Storage is PostgreSQL (the primary's pool; tables of migration
`hm-0020-idp-core`). On other backends the server keeps realms in memory and
logs a warning: development only.

## 2. FHIR endpoints: External vs Internal

An endpoint's `auth.provider`:

```json
{"mode": "full", "provider": {"kind": "internal", "realm": "clinic"}}
```

* `issuer`, `jwksUrl` are not used; the realm's **in-process public keys**
  verify the token (no JWKS fetch, no cache window).
* `aud` must be the endpoint's public base URL (`audience` overrides it).
* Every request checks the grant id (`gid`) against the realm's revoked set:
  `/idp/{realm}/revoke`, "remove access" on the connected-apps page, refresh
  reuse and code replay take effect **on the next request** on the instance
  that handled them, and within the 1 s epoch poll on the others.
* The endpoint must be listed in the realm's `endpoints`; a token for another
  endpoint or realm is refused (401).
* `/.well-known/smart-configuration` of the endpoint is generated from the realm.

Rows without `provider` are `external` (unchanged behaviour).

## 3. Hosted pages

`/idp/{realm}/login`, `/select-patient`, `/consent`, `/logout`,
`/account/apps` (SMART `management_endpoint`), `/account/login`,
`/idp/assets/idp.css`. They reuse the console's design system
(`/ui/assets/app.css`, theme cookie, language cookie), so the console UI
assets must be served by the same HFS (default).

Security properties (tests in `crates/idp-pages/tests/flows.rs`):

* CSRF: synchronizer token = HMAC(browser cookie ‖ form ‖ interaction) + Origin
  / `Sec-Fetch-Site` check on every POST.
* Clickjacking: `frame-ancestors 'none'` + `X-Frame-Options: DENY`; a realm may
  allow EHR embedding with `branding.frameAncestors: ["https://ehr.example"]`
  (https origins only; anything else is ignored).
* Strict CSP (`script-src 'self' 'nonce-…'`), `Referrer-Policy: same-origin` (no referrer to other origins; same-origin posts keep a real `Origin`),
  `Cache-Control: no-store`.
* No open redirect: redirects only go to the redirect URI validated at
  `/authorize` or to a registered post-logout URI.
* A leaked interaction handle cannot be finished in another browser: after
  sign-in every step requires the same browser's SSO session.
* SSO cookie `__Host-hmidp-{realm}` (`Secure; HttpOnly; SameSite=Lax`), new
  session handle at every sign-in (fixation), idle/max lifetimes of the realm.

## 4. Brute force and rate limits

* Lockout (cluster-wide, table `admin_login_failures`, scope `IDP`): per
  account after `security.lockoutThreshold` failures (default 5) for
  `lockoutSecs` (900); per address at 4 × the threshold. Unknown user names
  lock like real ones (no enumeration); the password check runs a dummy
  argon2id verification for them; at most 4 verifications run at once.
* Token buckets (per instance) per realm × address / client; `429` +
  `Retry-After`.
* nginx adds a coarse `limit_req` on `/idp/` (see `docker/production/nginx.conf`).

To unlock an account before the timeout:

```sql
DELETE FROM admin_login_failures WHERE scope = 'IDP' AND client LIKE '{realm_id}:%';
```

## 5. Audit

Every event goes to the deployment's audit sink (`HFS_AUDIT_*`) as a FHIR R4
AuditEvent:

| Event | type / subtype | profile |
|---|---|---|
| token issued / refreshed | DCM 110114 / `urn:ihe:event-type-code#ITI-71` | `IHE.IUA.71` (BALP 1.1.4) |
| consent granted for a patient | DCM 110113 / `idp-event#consent-granted` | `IHE.BasicAudit.AuthZconsent` |
| login success / failure / lockout | DCM 110114 / DCM 110122 | — (ATNA pattern; BALP has no login profile) |
| logout | DCM 110114 / DCM 110123 | — |
| consent withdrawn / denied, revocation, refresh reuse, code replay, client authentication failure, key change | DCM 110113 | — |
| a user's resource access changed (console editor or `/admin/idp/…/links`) | DCM 110113 / `idp-event#user-access-change`, action `U`, details `action added removed changed total` | — |

No event carries a token, code, verifier, password or client secret.

## 5a. Users' resource access in bulk

A user's FHIR resources (the `fhirUser` identity, accessible patients, other
links) are edited on the user's page (**Resource access**: search the bound
endpoint, paste or import lists, copy all, bulk remove, undo) or through the
admin API — viewer role to read, editor to change:

```
GET   /admin/idp/realms/{realm}/users/{user}/links      JSON {links, entries, total, fingerprint};
                                                         Accept text/csv | text/plain, or ?format=csv|lines|json
PUT   /admin/idp/realms/{realm}/users/{user}/links      replace (JSON, text/csv, text/plain)
PATCH /admin/idp/realms/{realm}/users/{user}/links      add, or JSON {"add": [...], "remove": [...]}
      ?endpoint=  rows without one   ?dryRun=true  plan only   ?skipInvalid=true  apply the good rows
      If-Match: "<fingerprint>"  (412 when the list changed meanwhile)
```

Formats: one reference per line (`Patient/123`, `endpoint/Patient/123`, or an
absolute URL), CSV `endpoint,reference,kind` (`kind` = `fhirUser`, `patient`,
`link`), JSON (the links array, entries, reference strings, or a FHIR search
Bundle). At most 10,000 rows (4 MiB) per request, 20,000 entries per user;
one store transaction each, audited as `user-access-change`. The stored model
is unchanged: patients go into the identity link's `accessiblePatients` (or
become plain `Patient/…` links on an endpoint without an identity), so the
token, consent and patient-picker logic read the same links as before.

The console shows each reference's name and whether it exists on the
endpoint by reading the endpoint as the signed-in console user; on an
endpoint with Internal security it mints a 60-second read-only token of that
realm for the console user (`client_id hfs-console-resource-access`,
`user/{Patient,Practitioner,PractitionerRole,RelatedPerson,Person}.rs`),
like the hosted patient picker does. The API does not check existence.

## 6. nginx

```nginx
location /idp/ {
    limit_req zone=idp_auth burst=40 nodelay;
    proxy_set_header X-Real-IP $remote_addr;
    proxy_set_header X-Forwarded-Proto https;
    client_max_body_size 64k;
    proxy_pass http://hfs:8080;
}
location /.well-known/oauth-authorization-server/idp/ { proxy_pass http://hfs:8080; }
location /.well-known/openid-configuration/idp/       { proxy_pass http://hfs:8080; }
```

## 7. Test stack

`crates/idp-pages/e2e/README.md`: an `hfs` with `HFS_IDP=on`, the dev seed
and `HFS_MANAGER=on`, then `python3 idp_e2e.py` (Playwright, Chromium)
drives every flow and security check and writes screenshots.
