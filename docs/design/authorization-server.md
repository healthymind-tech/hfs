# Built-in Authorization Server — SMART on FHIR / OAuth 2.x / OpenID Connect

Status: **design, recommended for implementation** (line IdP0, 2026-11-23).
Branch `hm/design-idp` (from `hm/main` 09eb7ee02 = production `hfs.gugulu.tw`).
Package 0 (core types, key management, migration SQL) is implemented on this
branch as `crates/idp` (`helios-idp`), 40 unit tests green.

The question was: *can HFS host its own authorization server, so a FHIR
endpoint's SMART config can say "External" (Keycloak, Entra, Epic …) or
"Internal" — with internal authorization servers created by a step-by-step
wizard, each at its own path, supporting everything the standards and the
other products support?*

**Short answer: yes, and it should be built natively, but in three phases,
with conformance suites as release gates and an external penetration test
before it holds real patient data.** Phase 1 alone covers every SMART App
Launch 2.2 capability Inferno tests, plus Backend Services, OIDC sign-in,
introspection and revocation. "Everything" also includes features that the
current specs *forbid* (implicit grant, password grant, PKCE `plain`). Those
are listed in the matrix as **refused on purpose**, with the spec section that
forbids them. Building them would make us fail the conformance suites.

Decisions are marked **D-n**; the summary table is in §0. Spec citations use
the section headings of the specs (SMART App Launch 2.2.0 has unnumbered
headings, so those are quoted by name).

---

## 0. Key decisions

| # | Decision | Why |
|---|---|---|
| D-1 | **Build natively in Rust** as a module of the unified server (`crates/idp` core + `crates/idp-server` HTTP/pages). No Keycloak/Hydra sidecar. External IdPs stay first-class via the existing "External" mode. | One process, one DB, one console (unification line). No embeddable Rust OP library exists. Sidecars cannot serve SMART's token-response context without a proxy, are single-issuer (Hydra) or need Java SPIs for the patient picker (Keycloak). §3. |
| D-2 | Realms are served at **`{origin}/idp/{realm}`**. The issuer is exactly that URL, with no trailing slash. `idp` is a reserved endpoint segment (added in P0). Not `/auth`: the reference deployment routes that to Keycloak. | Path-separated, as the user asked. Discovery works by both OIDC suffix and RFC 8414 path insertion. nginx needs one `location /idp/` rule. |
| D-3 | A FHIR endpoint's auth gets a **provider**: `External{issuer,jwksUrl}` (today) or `Internal{realm}`. Internal endpoints verify with the realm's **in-process key set**: no network JWKS fetch, immediate revocation through a grant deny-set, and SMART discovery generated from the realm. | That is the "External vs Internal" choice the user described. Internal is strictly stronger: no fetch, no JWKS cache window, revocation actually works. |
| D-4 | **Access tokens = JWT (RFC 9068, `typ: at+jwt`)** with SMART claims (`scope`, `patient`, `fhirUser`, `fhirContext`…) and a grant id `gid`. Refresh tokens, codes and launch handles are **opaque 256-bit handles stored only as SHA-256**. | External resource servers can verify JWTs offline. Opaque refresh tokens can be rotated and revoked, and a stolen DB yields nothing usable. |
| D-5 | **Refresh rotation on every use + reuse detection revokes the family** (RFC 9700 §4.14.2). Implemented as a compare-and-set on `current_generation`. | BCP for public clients. CAS makes it cluster-safe without locks. |
| D-6 | **PKCE S256 mandatory for every authorization-code client** (public and confidential). `plain` is not supported. | SMART 2.2 "Considerations for PKCE Support"; OAuth 2.1 §4.1.1; RFC 9700 §2.1.1. |
| D-7 | Signing keys per realm: an active **RS256** key (needed by OIDC as the id_token default) and an active **ES256** key. PS256/RS384/ES384/PS384 are optional. Keys are generated in-process (ring for EC, aws-lc-rs for RSA keygen, ring for all signing), **sealed with `HFS_SECRET_KEYS`** and AAD-bound to realm+kid. `kid` = RFC 7638 thumbprint. Rotation goes next → active → retiring → retired, run by a cluster-leased job. | Reuses the proven sealed-secret machinery. Constant-time primitives only. Avoids the RustCrypto `rsa` crate (RUSTSEC-2023-0071, no fix). |
| D-8 | Users: a realm's identity source is **local accounts** (argon2id), **console accounts** (staff realms, SSO with the console), or, in phase 2, an **upstream OIDC IdP** (brokering, which is also the base for UDAP Tiered OAuth). Users link to FHIR resources **per endpoint** (`Patient/…`, `Practitioner/…`), and that link becomes `fhirUser` and drives the patient picker. | Patients need accounts the console never sees. Clinicians already have console identities. The FHIR link is what SMART needs. |
| D-9 | Hosted pages (login, consent, patient picker, account/apps, logout) are **server-rendered Askama + minimal JS** under strict CSP (`default-src 'self'`, no inline script), `frame-ancestors 'none'` by default (per-realm allow-list for EHR embedding), en + zh-TW. | Same stack as `crates/ui`. Server rendering keeps the attack surface small. |
| D-10 | Storage = fork migrations **`hm-0020-idp-core`** (phase 1), `hm-0021` and `hm-0022` reserved. The IdP owns the **hm-0020…hm-0029** block so it never collides with the unification line. Config rows bump the `idp` scope of `hfs_config_epochs`. | Same pattern as hm-0008…hm-0010; cluster reload through the existing epoch/NOTIFY mechanism. |
| D-11 | One-time artefacts (code, launch handle, client-assertion `jti`, PAR URI, refresh generation) are consumed by **single atomic statements** (`DELETE … RETURNING`, CAS `UPDATE`). A replayed code revokes its grant (RFC 6749 §4.1.2). | Correct under N instances without distributed locks. |
| D-12 | Every token/authorize/login decision emits a **BALP AuditEvent** through `helios-audit` (login success/failure, logout, consent, token issued/refreshed/revoked, refresh reuse, client auth failure, key rotation, admin changes). | Existing audit pipeline. Authentication events are the ones an auditor asks for first. |
| D-13 | **Feature flag `HFS_IDP=off|on`** (default off). With the flag off, the schema is still created, `/idp` answers 404 and the console pages are hidden. "Internal" endpoints refuse to start in that state instead of silently opening. | Ship dark, enable per environment, roll back with the flag. |
| D-14 | **Release gates**: Inferno SMART App Launch (STU2 / STU2.2 suites) + Backend Services + Token Introspection groups green, and OpenID conformance suite `oidcc-basic` + `oidcc-config` (+ `oidcc-formpost-basic`, `oidcc-rp-initiated-logout`) green in CI. **No default-on and no production PHI before an external pentest.** | This is security-critical code. Passing suites is necessary, not sufficient. |
| D-15 | Deliberately **not supported**: implicit grant, resource-owner password grant, PKCE `plain`, `alg: none` / `HS*` signed tokens, wildcard or non-exact redirect URIs, bearer tokens in query strings, OIDC hybrid/front-channel token responses, OIDC Session Management iframes. | Removed or forbidden by SMART 2.x, OAuth 2.1, RFC 9700. Session iframes are broken by third-party-cookie blocking. Each refusal is listed in the matrix with its citation. |

---

## 1. Goals and non-goals

### Goals
1. Any number of **internal authorization servers (realms)** on one HFS, each with its own path, issuer, keys, users, clients, policy and branding. They are created with a **wizard** and managed in the console.
2. **Full SMART App Launch 2.2**: EHR launch and standalone launch, public, symmetric and asymmetric clients, OIDC SSO, all launch contexts (`patient`, `encounter`, `fhirContext`, `need_patient_banner`, `smart_style_url`, `intent`, `tenant`), v1 + v2 scopes including granular `?param=` scopes, offline and online access, introspection, `authorize-post`, the patient picker, and **Backend Services** (`client_credentials` + `private_key_jwt`, system scopes).
3. **OAuth 2.0/2.1 + OIDC** as other servers ship them (compliance matrix §13), phased.
4. Per-endpoint choice **External | Internal(realm)**, with internal verification in-process.
5. A built-in **"test this auth server"** diagnostic, plus CI conformance runs (Inferno, OIDF).
6. Cluster-safe, audited, rate-limited, secrets sealed. No plaintext private keys, client secrets, codes or refresh tokens at rest.

### Non-goals (this design)
* Replacing an enterprise IAM. LDAP/Kerberos/SAML federation, fine-grained authorization policies and identity brokering beyond OIDC belong to Keycloak/Entra, which HFS keeps supporting through "External". A realm can broker to such an IdP in phase 2.
* Hosting SMART apps or an app gallery (only launching registered apps).
* Patient self-registration with identity proofing (IAL2). Phase 2 offers invitation-based enrolment only.
* Being an OpenID **certified** OP in phase 1. We run the certification test plans in CI; formal certification (OIDF fee) is an operator decision after phase 2.

---

## 2. Baseline (what exists today)

| Area | Today (`hm/main` 09eb7ee02) | Reused by the IdP |
|---|---|---|
| Resource server | `crates/auth`: JWT validation (`jsonwebtoken` 9), JWKS fetch/cache (`JwksCache`), `VerifierCache` keyed by (issuer, jwks_url, …), SMART v1/v2 scope parsing and enforcement, patient confinement, per-endpoint discovery (`discovery.rs`), `$test-auth` | Internal verifier plugs in beside `JwksBearerAuthProvider`. Scope enforcement is unchanged. |
| Console sign-in | `crates/admin-auth` (`console` feature): none/local/oidc, argon2id, sessions in PG (`login_sessions`), lockout (`admin_login_failures`), CSRF | argon2 parameters, lockout table, CSRF pattern, OIDC client (for brokering, phase 2), console accounts as a realm identity source |
| Endpoints | `crates/endpoints`: `EndpointSpec.auth: EndpointAuth{mode, issuer, jwks_url, audience, scopes, smart}`, names/paths, registry + epochs, **sealed secrets** (`SecretKeyRing`, `hmsec:v1`), **outbound SSRF policy** | `EndpointAuth` gains a provider. Realm and endpoint names share one grammar. Private keys and upstream secrets are sealed. Client JWKS URLs are fetched through the outbound guard. |
| Audit | `crates/audit`: BALP AuditEvents, sinks | auth events (§10.1) |
| Cluster | `hfs_config_epochs` + NOTIFY, `cluster_jobs` leases | realm/client/key reload, key rotation job, cleanup job |
| UI | `crates/ui` (Askama + htmx), `ui-chrome`, HealthyMind branding | wizard and management pages. Hosted IdP pages reuse the design tokens. |
| Test IdP | Keycloak at `/auth` (realm `hfs-test`) | stays as the reference "External" IdP for regression tests |

---

## 3. Build vs embed — options and recommendation

| Option | What it is | Pros | Cons | Verdict |
|---|---|---|---|---|
| **A. Native module** (`crates/idp`) on `ring`/`aws-lc-rs` (signatures, keygen), `argon2`, existing `aes-gcm` sealing, `jsonwebtoken` (verifying client assertions) | We write the protocol layer and use vetted primitives only | One process/DB/console, as the unification line wants. SMART-native: launch context, patient picker, `fhirUser`, endpoint binding, in-process verification and revocation. Multi-realm by design. Same language, review and CI as the rest. | Security-critical protocol code is ours to get right. Mitigated by small scope, refusing legacy flows, conformance suites as gates, fuzz/property tests and an external pentest. Largest effort. | **Recommended** |
| B. Embed a Rust OAuth library | `oxide-auth` (OAuth 2 server framework). `oauth2`/`openidconnect` are **client-only**. | Some request plumbing | No OIDC OP, no SMART, low maintenance activity. We would still write OIDC, keys, sessions, consent and SMART. It saves maybe 10 % and adds a dependency to audit. | Rejected |
| C. Rust OP as a sidecar | Rauthy, Kanidm (full IdM apps) | Mature-ish Rust OPs | They are applications, not libraries. Separate DB/schema and UI. No SMART launch context or patient picker. A second console. | Rejected |
| D. Ory Hydra sidecar (Go, OpenID Certified, headless) | Hydra does the protocol; we implement the login/consent/patient-picker "apps" | Certified protocol core | **One issuer per deployment** (N realms = N Hydra instances, or the paid multi-tenant edition). SMART needs `patient`/`fhirContext` **at the top level of the token response**, which Hydra cannot emit without a rewriting proxy in front of `/token`. Separate migrations, admin API and binary to ship, upgrade and secure. | Rejected |
| E. Keycloak as a managed sidecar | We would provision Keycloak realms through its admin API from our wizard | Most complete feature set. Realms. OpenID Certified. | JVM (~0.5–1 GB RSS per node). No SMART out of the box: the community FHIR extensions are unmaintained, and the patient picker, launch context and `fhirUser` need custom Java SPIs. Two admin UIs and two user stores. Upgrades are our burden. **It is already supported today as "External"**, so anyone who wants Keycloak can keep using it. | Rejected as built-in; remains the External option |

**Recommendation: A**, under these constraints:
1. No custom cryptography. Signing and verification use ring or aws-lc-rs, password hashing uses argon2, sealing uses AES-GCM. Every handle comes from 256-bit OS CSPRNG output.
2. A protocol surface no larger than the matrix says. Every refused legacy flow is refused with a test.
3. Conformance suites run in CI on every IdP change (D-14).
4. Fuzzing (cargo-fuzz) for the parsers: scope grammar, authorize query/form, client assertion JWTs, DCR metadata. Property tests for the scope algebra.
5. Two-person review on `crates/idp*`, recorded in CODEOWNERS.
6. An external penetration test before production PHI or default-on.

### 3.1 Effort (honest)

| Phase | Scope | Size (Rust + templates) | Effort |
|---|---|---|---|
| **1 — SMART-complete** | Core, PG store, authorize/token/introspect/revoke/userinfo/jwks/discovery/logout(RP-initiated), client auth (none, secret, private_key_jwt), users (local + console), login/consent/patient picker/account pages, EHR launch from the console, Internal provider on endpoints, wizard + management, diagnostic, audit, rate limits, Inferno + OIDF in CI | ~12–15 k LOC | **12–16 engineer-weeks**. With the parallel packages of §15, about 4–5 calendar weeks of implementation plus 1–2 weeks of conformance hardening. |
| **2 — Modern OAuth** | DCR (7591/7592, initial access tokens, software statements), PAR, DPoP, device flow, token exchange, resource indicators (multi-aud), TOTP MFA, upstream OIDC brokering, front/back-channel logout, `online_access`, `context-style`, JWT introspection responses (9701), Protected Resource Metadata (9728) | ~8 k LOC | 8–10 engineer-weeks |
| **3 — Ecosystem / high assurance** | UDAP (discovery, certificate-based DCR, B2B, Tiered OAuth), JAR (9101), RAR (9396), mTLS (8705), FAPI 2.0 Security Profile, WebAuthn/passkeys, SMART App State | ~8–10 k LOC | 8–12 engineer-weeks |

The risk is in the details, not the size: redirect handling, session and CSRF
logic, consent UX and the many conformance edge cases. Budget 30 % of each
phase for conformance and security hardening. The estimates above include it.

---

## 4. Requirement interpretation: what "support every item" means

* **Supported** = implemented, advertised in discovery, and tested by a conformance suite or our own tests.
* **Refused on purpose** = the spec family the user named removes or forbids it (SMART 2.x, OAuth 2.1, RFC 9700). We return the standard error (`unsupported_grant_type`, `unsupported_response_type`, `invalid_request`), document why, and test the refusal. Implementing these would fail Inferno/OIDF checks or contradict the BCP.
* **Phase 2/3** = valid, implemented later. The model already has fields for them (P0 types carry `require_dpop`, `require_par`, `DcrPolicy`, `UserSource::UpstreamOidc`, …) so no redesign is needed.

---

## 5. Architecture

```
                 https://host
  ┌───────────────┬──────────────────────────────┬──────────────────────────┐
  │ /fhir/{ep}/…  │ /idp/{realm}/…                │ /ui/…  (console)         │
  └──────┬────────┴──────────────┬───────────────┴───────────┬──────────────┘
         │ nginx strips /fhir     │ nginx passes /idp as is   │
  ┌──────▼───────────────────────▼───────────────────────────▼──────────────┐
  │ HFS (unified server, one process)                                       │
  │  endpoint dispatcher ──► FHIR router ──► auth middleware (helios-auth)  │
  │        │                                   │ External: JwksCache        │
  │        │                                   │ Internal: RealmKeySet ◄─┐  │
  │  idp router (crates/idp-server)            │ + grant deny-set        │  │
  │   /idp/{realm}/authorize token jwks …  ───────────────────────────────┘  │
  │   hosted pages (Askama)                                                 │
  │  RealmRuntime (ArcSwap snapshot: realms, clients, public keys, signers) │
  │  console (crates/ui): wizard, realm pages, diagnostic                   │
  └──────────────────────────────┬──────────────────────────────────────────┘
                                 │ one PostgreSQL (hfs_idp_* tables, epochs)
```

### 5.1 Realms and URLs (D-2)

`issuer = {HFS_IDP_PUBLIC_BASE}/idp/{realm}`. `HFS_IDP_PUBLIC_BASE` defaults to
the origin of `HFS_BASE_URL` (`https://hfs.gugulu.tw`). It can be set
separately when the public origin differs (e.g. a dedicated `login.` host).

| Route (under the issuer) | Purpose | Spec |
|---|---|---|
| `GET/POST /authorize` | authorization endpoint (`authorize-post`) | RFC 6749 §3.1, SMART "Obtain authorization code", OIDC Core §3.1.2 |
| `POST /token` | code, refresh, client_credentials (+ device, token-exchange in P2) | RFC 6749 §3.2, SMART, Backend Services |
| `GET /jwks` | realm public keys | RFC 7517, OIDC Discovery `jwks_uri` |
| `GET /.well-known/openid-configuration` | OIDC Discovery | OIDC Discovery §4 |
| `GET {origin}/.well-known/oauth-authorization-server/idp/{realm}` | RFC 8414 path insertion (same JSON) | RFC 8414 §3 |
| `POST /introspect` | RFC 7662 + SMART fields | RFC 7662, SMART "Token Introspection" |
| `POST /revoke` | RFC 7009 | RFC 7009 |
| `GET/POST /userinfo` | OIDC userinfo | OIDC Core §5.3 |
| `GET/POST /logout` | RP-initiated logout | OIDC RP-Initiated Logout 1.0 |
| `GET /login`, `/consent`, `/select-patient`, `/account/apps` | hosted pages (`/account/apps` = SMART `management_endpoint`) | SMART "Conformance" (management_endpoint) |
| `POST /register`, `/register/{id}` (P2) | DCR | RFC 7591/7592 |
| `POST /par` (P2), `/device_authorization` (P2) | PAR, device flow | RFC 9126, RFC 8628 |

**FHIR side:** `{fhir-base}/.well-known/smart-configuration` and the
CapabilityStatement `oauth-uris` extension of an Internal endpoint are
generated from the realm (`metadata::smart_configuration`). `aud` = the
endpoint's public FHIR base URL. SMART requires `aud`; `resource` (RFC 8707) is
accepted as a synonym. Phase 2 adds `{fhir-base}/.well-known/oauth-protected-resource`
(RFC 9728) and, in phase 3, `{fhir-base}/.well-known/udap`.

**nginx:** add `location /idp/ { proxy_pass http://hfs:8080; }` and
`location /.well-known/oauth-authorization-server/idp/ { proxy_pass http://hfs:8080; }`.
Package I8 changes `docker/production/nginx.conf`.

### 5.2 Endpoint binding (D-3)

`helios_endpoints::EndpointAuth` gains:

```rust
pub enum AuthProviderRef {               // serde tag "kind"
    External,                            // today's issuer + jwksUrl fields
    Internal { realm: String },          // issuer/jwks derived; audience = endpoint base URL
}
pub provider: AuthProviderRef            // default External ⇒ existing rows unchanged
```

* Validation: `Internal` requires the realm to exist and be enabled, and the endpoint to be listed in `realm.endpoints`. Both are written in one admin transaction by the wizard's "Endpoints bound" step.
* Runtime: `VerifierCache` resolves `Internal{realm}` to an `InternalVerifier`. It verifies signatures against `RealmRuntime`'s public keys (ArcSwap, reloaded on the `idp` epoch), checks `iss`, `aud`, `exp`/`nbf`, `typ=at+jwt`, then **checks `gid` against the realm's revoked-grant set**. That set is loaded from `hfs_idp_grants WHERE revoked_at IS NOT NULL AND expires_at > now()`, bounded by token lifetime, and refreshed on epoch. Revocation therefore takes effect within the cluster sync interval (≈1 s). External resource servers that verify our JWTs offline see revocation only at token expiry, so they should introspect. This is documented and is why access tokens default to ≤ 1 h and backend tokens to 300 s.
* Scope enforcement, patient confinement and `$test-auth` are unchanged: they consume the same `Principal`.
* "Internal" with `HFS_IDP=off` → the endpoint stays **closed** (503 with an OperationOutcome naming the flag), never open.

### 5.3 Discovery documents

These are generated from `RealmSpec.features` (P0 `metadata.rs`), so the
wizard's capability toggles, the metadata and the SMART `capabilities` array
can never disagree. Each capability string maps to one feature flag (§13).

---

## 6. Key management (D-7)

* **Generation:** EC P-256/P-384 with `ring::EcdsaKeyPair::generate_pkcs8`. RSA 2048/3072/4096 with `aws_lc_rs::rsa::KeyPair::generate`, exported as PKCS#8 and then loaded into `ring::RsaKeyPair`. **All signing goes through ring.** RSA keys under 2048 bits are refused. Operators may **import** PKCS#8 (PEM/DER), e.g. from an HSM export. HSM/KMS signing is an open item (§17).
* **At rest:** `hfs_idp_signing_keys.private_key` = `hmsec:v1` envelope under `HFS_SECRET_KEYS`, AAD `idp-realm:{realm_id}:signing-key:{kid}`. Opening checks that the thumbprint equals the row's `kid`. Without a key ring, creating a realm is refused (409, "configure HFS_SECRET_KEYS"), the same fail-closed rule as other secrets. `POST /admin/secrets/$rewrap` covers IdP keys.
* **Algorithms:** `RS256` is always present when OIDC is on, because it is the id_token default (OIDC Core §3.1.3.7, Discovery §3). `ES256` is the default access-token algorithm (smaller tokens). PS256/RS384/ES384/PS384 are optional per realm. Per-client `id_token_signed_response_alg` arrives with DCR in phase 2. `HS*` and `none` are never used.
* **Rotation:** `keys::plan_rotation` is a pure, deterministic planner run under a cluster lease every few minutes. For each algorithm it creates a `next` key at `rotate_after_days − prepublish_hours`, promotes it after `prepublish_hours` (≥ relying parties' JWKS cache lifetime; HFS's own is 300 s), and keeps the old key `retiring` (still published) for `retain_hours` (≥ the longest token lifetime, validated). After that the key is `retired`. A unique partial index keeps exactly one `active` key per (realm, alg). **Emergency rotation** ("key compromised") retires the key immediately and revokes all grants: every token is invalid at once on internal endpoints, and clients re-authenticate.
* **JWKS:** published keys only (`next`, `active`, `retiring`), with `Cache-Control: max-age=300`.

---

## 7. Tokens

### 7.1 Access token (RFC 9068 + SMART)

Header `{"alg","kid","typ":"at+jwt"}`. Claims: `iss sub aud exp iat jti
client_id scope gid` plus optional `auth_time acr amr fhirUser`, flattened
launch context (`patient encounter fhirContext need_patient_banner
smart_style_url intent tenant`), and `cnf` (`jkt` for DPoP, `x5t#S256` for
mTLS). For `client_credentials`, `sub` = `client_id` (RFC 9068 §2.2).
Lifetimes: user launches ≤ 3600 s (SMART "Obtain access token": "SHOULD … no
more than one hour"), backend ≤ 300 s by default (Backend Services: "SHOULD
NOT exceed 300 seconds").

### 7.2 id_token (OIDC Core §2)

`iss sub aud exp iat auth_time nonce at_hash sid acr amr fhirUser`, plus
`name email` when those scopes are granted. It is signed with the realm's
`RS256` key unless the client registered another algorithm. `nonce` is
required when the client sent one (OIDC Core §3.1.2.1) and echoed exactly.

### 7.3 Token response (SMART)

`access_token token_type=Bearer|DPoP expires_in scope id_token? refresh_token?`
plus the launch context **at the top level**, with `Cache-Control: no-store`
and `Pragma: no-cache` (SMART "Obtain access token"). `scope` may be narrower
than requested. The consent screen lets the user untick scopes.

### 7.4 Refresh tokens (D-5)

These are opaque `hmrt_…` handles. Each grant with `offline_access` or
`online_access` owns one family, and each use rotates the token
(`current_generation + 1`). Presenting an older generation → **family revoked +
AuditEvent "refresh token reuse"** (RFC 9700 §4.14.2). The refresh token is
bound to its `client_id` (SMART: "SHALL be bound to the same client_id"), the
requested scope must narrow the original (`scope::is_narrowing`), and the
launch context is carried over. `online_access` families die with the SSO
session. Lifetimes: absolute 90 d and idle 30 d by default. Backend Services
gets no refresh tokens ("SHOULD NOT"). In phase 2, DPoP-bound refresh tokens
for public clients allow a longer idle time.

### 7.5 Opaque handles

`hmac_` (code, 60 s, single use), `hmlh_` (launch handle, 300 s), `hmdc_`,
`hmrat_`, `hmiat_`, `urn:ietf:params:oauth:request_uri:hmpar_` (PAR, RFC 9126
§2.2). All carry 256 bits of entropy. Only SHA-256 digests are stored. A
handle with the wrong prefix is rejected before any DB lookup.

### 7.6 Introspection / revocation

* `/introspect` (RFC 7662): callers authenticate as a client of the realm (any configured method) **or** with a bearer token carrying scope `introspect` (SMART "Token Introspection": access control is optional, but if present it must accept a scoped SMART bearer token). Response: `active scope client_id exp iat sub iss aud token_type`, plus the **launch context and `fhirUser`** (SMART requires the context parameters from the original token response). Inactive, unknown, malformed and foreign-realm tokens all return `{"active":false}`. Phase 2: `application/token-introspection+jwt` (RFC 9701).
* `/revoke` (RFC 7009): a refresh token revokes its family and grant. An access token revokes its grant (`gid`), which kills the sibling refresh family too. The response is always 200, including for unknown tokens (RFC 7009 §2.2). The client must own the token.

---

## 8. Flows

### 8.1 Users, identity sources, FHIR links (D-8)

* `UserSource::Local`: `hfs_idp_users` + `hfs_idp_credentials` (argon2id, OWASP 2024 parameters as in `admin-auth`). Admins create users, import them from CSV, or send an invite link (phase 2). Users choose and reset passwords through a time-limited one-time link (phase 2 needs an email transport; phase 1 = admin-issued reset links).
* `UserSource::Console`: the console's users. A realm session is created from a live console session (same browser, so the realm uses the console cookie) or by signing in on the realm's login page with the console credentials. Console roles map to user kind `practitioner`/`staff`.
* `UserSource::UpstreamOidc` (P2): authorization code + PKCE to the upstream IdP (reuses the `admin-auth` OIDC client). Accounts are linked by `external_subject`, with just-in-time creation optional.
* **FHIR links** (`hfs_idp_user_links`): per endpoint, a reference (`Patient/123`, `Practitioner/abc`, …). One link is `is_fhir_user` → `fhirUser` = `{endpoint base}/{reference}` (absolute URL, SMART "fhirUser"). `accessible_patients` holds the extra patients a caregiver or proxy may pick. The admin user page has a "link to FHIR resource" search that runs against the endpoint.

### 8.2 Standalone launch (authorization code + PKCE)

1. `GET|POST /authorize`. Checks: realm enabled; `client_id` known and enabled; `redirect_uri` **exact match** (RFC 9700 §4.1.3; loopback port exception RFC 8252 §7.3) — if it fails, show an error page and **never redirect**; `response_type=code`; PKCE S256; `aud` (or `resource`) is the base URL of an endpoint bound to the realm (else `invalid_request`); `state` is echoed. `scope` is intersected with the client policy (`scope::grant`). `prompt`, `max_age`, `login_hint`, `ui_locales` and `nonce` are honoured.
2. Login (or a reused SSO session if `max_age`/`prompt` allow). `prompt=none` without a session → `login_required` redirect.
3. **Patient picker** when `launch/patient` was granted and the user is not confined to exactly one patient. A Patient user with one link is selected automatically. A caregiver sees a list. A practitioner searches the bound endpoint (name, birth date, identifier). The search runs in-process against the FHIR layer **with the practitioner's own permissions**, so the picker cannot reveal patients the user could not read. `launch/encounter` gives the same picker for that patient's encounters (optional; skippable unless the client requires it).
4. **Consent** unless the client is `trusted` or a stored consent covers all granted scopes for (user, client, endpoint). The screen shows the app name, logo, redirect host and a plain-language scope list (resource names, read/write, granular filters), with per-scope checkboxes. A denial redirects with `access_denied`.
5. Redirect `code`, `state` and `iss` (RFC 9207) via `query` or `form_post` (OAuth 2.0 Form Post Response Mode).
6. `POST /token`: client auth, then `take_code` (atomic). The code's `redirect_uri` and `client_id` must match and PKCE is verified. If the code was already used, the grant is revoked. The token response follows §7.3.

### 8.3 EHR launch

The **console is the EHR**. On an Internal endpoint, the page *Apps → Launch*
(or the patient page's "Launch app…") creates a launch handle with `{client,
endpoint, patient, encounter?, fhirContext[], need_patient_banner, intent,
smart_style_url, tenant}`. The browser is then sent to
`{app launch_url}?iss={fhir-base}&launch=hmlh_…`. At `/authorize`, `launch` is
required with scope `launch`, the handle is consumed once, and the context is
bound to the code. The user is the console user (console-source realms: SSO
without a prompt) or must sign in (local realms). The admin API
`POST /admin/idp/realms/{r}/launches` lets external portals create launch
handles (service token required).

### 8.4 Backend Services

`client_credentials` + `client_assertion_type=…jwt-bearer` + `client_assertion`.
Checks: `iss = sub = client_id`. `aud` must be a single value equal to the
token endpoint URL (SMART) or the issuer — accepted for RFC 7523bis-aligned
clients; multi-valued `aud` is refused. `exp` ≤ now + 5 min, `jti` unseen for
(realm, client) until `exp` (`hfs_idp_jti`). `jku`, if present, must equal the
registered `jwks_uri`. Exactly one key matches `kid` with the right `kty`.
Algorithms are in the allowed list (RS384 and ES384 at least, SMART
"Client Authentication: Asymmetric"). The client JWKS is fetched through the
**outbound SSRF guard** with timeouts, a size cap and Cache-Control-bounded
caching. Granted scope = requested ∩ pre-authorized `system/` scopes
(Backend Services "SHALL ensure requested scope falls within the client's
pre-authorized scope"). Token TTL is 300 s and no refresh token is issued. All
failures return `invalid_client` (401).

### 8.5 Sessions, SSO, logout

The realm SSO cookie is `__Host-hmidp-{realm}` (`Secure; HttpOnly;
SameSite=Lax; Path=/`). Lax is needed for the top-level redirect back from an
app. The cookie holds only a random handle; its digest is the key in
`hfs_idp_sessions`. Idle 30 min, max 10 h by default. The session ID is
regenerated at login (prevents fixation). All hosted forms carry a
synchronizer CSRF token and pass an Origin check. Phase 1 has **RP-initiated
logout** (`id_token_hint`, registered `post_logout_redirect_uris` exact
match, `state`). Phase 2 adds front-channel and back-channel logout
(`logout+jwt`, delivery through the outbox).

### 8.6 MFA, brute force

* Lockout: `admin_login_failures` scopes `IDP:{realm}:user:{id}` and `IDP:{realm}:ip:{ip}`; 5 failures → 15 min by default. Errors are generic ("username or password is incorrect"). For unknown users the server still runs a dummy argon2 verification, so timing does not reveal which accounts exist. A semaphore bounds concurrent argon2 work (DoS).
* **TOTP (RFC 6238) in phase 2.** The seed is sealed and recovery codes are hashed. A realm policy can make MFA optional or required, and `amr`/`acr` are reported. **WebAuthn/passkeys in phase 3** (`webauthn-rs`).

---

## 9. Storage (`hm-0020-idp-core`, P0 `schema.rs`)

| Table | Content |
|---|---|
| `hfs_idp_realms` | realm spec JSONB, name unique among non-deleted rows, revision |
| `hfs_idp_signing_keys` | kid, alg, status, public JWK, **sealed** private key, lifecycle times; one `active` per (realm, alg) |
| `hfs_idp_clients` | client spec JSONB (secret = argon2id hash), registration kind (`admin`/`dcr`/`udap`), RFC 7592 token hash |
| `hfs_idp_users`, `hfs_idp_user_links`, `hfs_idp_credentials` | users, FHIR links, password/TOTP/WebAuthn/recovery credentials |
| `hfs_idp_sessions` | SSO sessions (digest of the cookie, `sid`, `auth_time`, `amr`, `acr`) |
| `hfs_idp_consents` | remembered consent per (user, client, endpoint) |
| `hfs_idp_grants` | grant = `gid`; revocation flag (feeds the internal verifier's deny-set) |
| `hfs_idp_codes`, `hfs_idp_code_tombstones`, `hfs_idp_launches`, `hfs_idp_jti` | one-time artefacts (digest keys, expiry) |
| `hfs_idp_refresh_families`, `hfs_idp_refresh_tokens` | rotation state (CAS on `current_generation`) |

Phase 2 (`hm-0021`): `hfs_idp_par`, `hfs_idp_device_codes`, `hfs_idp_dpop_nonces`,
`hfs_idp_initial_access_tokens`, `hfs_idp_logout_outbox`. Phase 3 (`hm-0022`):
`hfs_idp_udap_trust_anchors`, `hfs_idp_webauthn`. A cleanup job (cluster lease)
deletes expired artefacts every minute. SQLite deployments: the IdP needs
PostgreSQL in phase 1 (production already runs PG). A SQLite store is an
open item.

---

## 10. Cross-cutting

### 10.1 Audit (BALP via `helios-audit`)

| Event | type / subtype | outcome detail |
|---|---|---|
| login success/failure, logout | DICOM `110114` User Authenticated; `110122` Login / `110123` Logout | user, realm, client, IP, amr; failure reason (internal) |
| token issued / refreshed | IHE IUA "Get Access Token" (ITI-71) pattern, BALP basic profile | client, subject, scope, aud, gid (never the token) |
| refresh reuse, code replay | security alert (`110113`) | family, client, IP |
| client authentication failure | `110113` | client, reason |
| consent granted/withdrawn, grant revoked | BALP "privacy consent" pattern | scopes |
| key created/promoted/retired/emergency | `110113` + admin actor | kid |
| realm/client/user admin changes | REST audit of admin API | actor, diff (redacted) |

### 10.2 Rate limits

Token-bucket limits per realm × (IP, client): `/token` 60/min/client and
300/min/IP; `/authorize` and `/login` POST 20/min/IP; `/register` (P2) 10/h/IP;
`/introspect` 600/min/client. They are configurable per realm, return 429 with
`Retry-After`, and are cluster-approximate (per instance, scaled by instance
count). Lockout (§8.6) is the exact, cluster-wide control.

### 10.3 Cluster safety

All state lives in PG. Consumption is atomic (D-11). Caches (realms, clients,
public keys, grant deny-set) reload on the `idp` epoch (NOTIFY plus a 1 s
poll). The writing instance reloads before replying. Key rotation and cleanup
run under `cluster_jobs` leases with fencing. Signing keys are opened once per
instance per epoch and held as ring key pairs; PKCS#8 bytes are wiped on drop.

### 10.4 Security risk analysis

| Threat | Mitigation | Test |
|---|---|---|
| Code interception / injection | PKCE S256 for all clients; code single-use, 60 s, bound to client + redirect_uri; replay revokes grant | unit + Inferno PKCE checks + OIDF `oidcc-codereuse` |
| Open redirect / redirect manipulation | exact matching; error page (no redirect) on a bad client or redirect_uri; no wildcards | `client.rs` tests + OIDF `oidcc-ensure-registered-redirect-uri` |
| Mix-up attacks (multiple realms/IdPs) | `iss` in the authorization response (RFC 9207); distinct issuers per realm | OIDF + own |
| CSRF on login/consent | synchronizer token + Origin check; SameSite; `state` echoed | own |
| Clickjacking | `frame-ancestors 'none'` (per-realm allow-list) + `X-Frame-Options` | own |
| XSS on hosted pages | Askama auto-escape; strict CSP without inline script; no user HTML | own + ZAP baseline in CI |
| Token leakage | no tokens in URLs (no implicit), `Referrer-Policy: no-referrer`, `no-store`, short ATs, bearer tokens refused in the query string | own |
| Refresh token theft | rotation + reuse detection (family revoke); client binding; DPoP (P2) | `token.rs` + PG concurrency test |
| Client impersonation | private_key_jwt with `jti` replay cache, `exp` ≤ 5 min, single `aud`, `jku` pinned; secrets as argon2id, compared in constant time | own + Inferno backend services |
| Algorithm confusion | verification algorithm fixed per key type; `none`/`HS*` refused for assertions; `typ=at+jwt` required by the internal verifier | own |
| SSRF via client `jwks_uri` / DCR URLs / logo URIs | outbound guard (`HFS_OUTBOUND_ALLOW`), https-only, resolved-IP check, size and time caps; logos fetched by the browser only behind CSP `img-src` | existing guard tests + own |
| Credential stuffing / brute force | lockout per user and IP, argon2id, generic errors, dummy hashing, rate limits, MFA (P2) | own |
| User enumeration | uniform responses and timing on login and reset | own |
| Session fixation / hijack | regeneration at login, `__Host-` cookie, idle/max timeouts | own |
| Signing key compromise | sealed at rest, never exported by the API, rotation, emergency rotate + revoke-all | own |
| DB exfiltration | only digests of codes, refresh tokens, sessions and secrets; keys sealed under an env-held key ring | review |
| Cross-realm / cross-endpoint token reuse | `iss` per realm, `aud` per endpoint; internal verifier checks both; realm id in every query | own + PG |
| Patient-context escalation | the picked patient must be in the user's selectable set; the picker searches with the user's permissions; the FHIR layer enforces `patient` confinement | own + Inferno (patient access) |
| Consent bypass | trusted flag settable only by admins (never DCR); stored consent must cover every granted scope | own |
| DoS | rate limits, argon2 semaphore, bounded request sizes, bounded JWKS fetches | load test |
| Logging secrets | request logging redacts `client_secret`, `client_assertion`, `code`, `code_verifier`, `refresh_token`, `password`, `Authorization` | own (log capture test) |
| Insider misuse of admin power | every admin action audited; key material write-only; break-glass separate | review |

---

## 11. Console UI

### 11.1 Information architecture

The top-level nav gains **"Authorization servers"** (`/ui/idp`), and each
endpoint's settings get an **Auth** tab with the choice *Off / External /
Internal(realm)*. Internal shows the realm, its discovery link and the
"Test" button.

```
/ui/idp                         list: realm, issuer, endpoints bound, clients, users, key age, status
/ui/idp/new                     wizard (7 steps)
/ui/idp/{realm}                 overview: health, discovery links, last diagnostic, recent auth events
/ui/idp/{realm}/clients[/id]    clients: create (type picker), redirect URIs, scopes, keys, secret reset (shown once)
/ui/idp/{realm}/users[/id]      users: create/import CSV, FHIR links (search endpoint), reset link, lock/unlock, sessions, grants
/ui/idp/{realm}/keys            keys: status timeline, rotate now, emergency rotate, import
/ui/idp/{realm}/sessions        active SSO sessions, revoke
/ui/idp/{realm}/grants          active grants by client/user, revoke
/ui/idp/{realm}/settings        features/capabilities, lifetimes, security, branding, endpoints
/ui/idp/{realm}/test            diagnostic runner + history
/ui/e/{ep}/apps                 launchable apps of the endpoint's realm → "Launch" (EHR launch)
```

### 11.2 Creation wizard (htmx, server-side draft)

The draft is stored as a realm row with `enabled=false` and `draft=true`, so
a user can leave and resume. It is committed in one transaction at the last
step. Every step validates on the server (`RealmSpec::validate`,
`ClientSpec::validate`) and shows field errors in place.

| Step | Content | Notes |
|---|---|---|
| 1 Basics | name (live path + issuer preview `https://host/idp/{name}`), display name, **template**: *Patient-facing apps* / *Clinician apps with EHR launch* / *Backend services (system-to-system)* / *Everything (SMART full)* / *Custom* | the template presets steps 3–5 |
| 2 Signing keys | algorithms (RS256 required with OIDC, ES256 recommended, others optional), RSA size, rotation policy, *Generate now* or *Import PKCS#8* | blocked with an explanation when `HFS_SECRET_KEYS` is missing |
| 3 Users / identity source | Local / Console accounts / Upstream OIDC (P2); lockout + password policy; MFA (P2); optional CSV import with Patient-id links | shows how `fhirUser` will be formed |
| 4 Clients | add one or more apps: *Public (browser/mobile)*, *Confidential – secret*, *Confidential – private key (JWKS URL / paste JWKS)*, *Backend service*. Redirect URIs (validated live), launch URL, launch types, allowed scopes (scope builder with granular filters), trusted flag | secrets shown once, with a copy button and a warning |
| 5 SMART capabilities & scopes | toggles grouped like the SMART "Capabilities" section (launch modes, client types, SSO, context, permissions), token lifetimes, `scopes_supported` preview | live preview of `smart-configuration` JSON |
| 6 Endpoints bound | choose endpoints → each switches to `Internal(realm)`; mode (write-protect/full); audience preview; **warning and diff** when an endpoint is currently External or Off | changing an endpoint's auth is shown as a diff and audited |
| 7 Review & test | summary + JSON of everything to be created; *Create* → runs the diagnostic (§11.3) live and shows a per-step report; links to discovery, JWKS and the Inferno instructions | when the diagnostic fails, the realm stays enabled, the endpoints are not switched yet, and the user can fix and retry |

### 11.3 "Test this auth server" diagnostic

The diagnostic runs server-side against the **public URLs** (through the real
router, as a client would). It uses a per-run **ephemeral test client pair**
(public PKCE + private_key_jwt with a throw-away EC key) and an **ephemeral
test user**, all deleted at the end and audited as diagnostic. The flow
drives the **real hosted login form** (cookie jar, CSRF token), so no code
path bypasses authentication.

| # | Step | Checks |
|---|---|---|
| 1 | Discovery | OIDC + RFC 8414 + each bound endpoint's `smart-configuration`; required fields; issuer exact match; capabilities ↔ features |
| 2 | JWKS | keys parse, `kid` = thumbprint, an active key per configured algorithm, no private members |
| 3 | Authorization code + PKCE | authorize → login → (patient picker) → consent → code → token; `state`/`iss` echoed; id_token signature, `nonce`, `at_hash`; access token claims (`aud`, `scope`, `patient`, `fhirUser`) |
| 4 | Negative checks | wrong `code_verifier` → `invalid_grant`; code replay → refused + grant revoked; unregistered redirect_uri → error page, no redirect; `plain` PKCE refused |
| 5 | FHIR call | `GET {fhir-base}/Patient/{patient}` with the token → 200; out-of-scope resource → 403 (via `$test-auth`) |
| 6 | Refresh | rotate → new token; reuse of the old token → `invalid_grant` + family revoked |
| 7 | Backend Services | client_credentials with private_key_jwt → token; `jti` replay → `invalid_client`; `exp` > 5 min → refused |
| 8 | Introspection | active token → `active:true` + context; garbage → `active:false` |
| 9 | Revocation | revoke → introspection `active:false`; FHIR call → 401 within the sync interval |
| 10 | Userinfo / logout | userinfo `sub` = id_token `sub`; RP-initiated logout ends the session |

The report shows status, timing, spec reference and remediation hint per step.
It is stored (last 20 runs), downloadable as JSON, and available through
`POST /admin/idp/realms/{r}/$diagnose` for CI.

---

## 12. Admin API (console writes through it, like manager P4)

`/admin/idp/realms` (CRUD, `$diagnose`, `$rotate-keys`, `$emergency-rotate`),
`/admin/idp/realms/{r}/clients|users|sessions|grants|launches|keys`.
Admin role required, CSRF and console session or admin token, optimistic
concurrency via `revision`/`If-Match`. Secrets are write-only (`{"set":true}`).
Every write is audited.

---

## 13. Compliance matrix

Legend: **P1/P2/P3** = phase; **✗** = refused on purpose (reason cited);
**Inf** = Inferno SMART App Launch test kit (STU2 and STU2.2 suites, incl.
Backend Services and Token Introspection groups); **OIDF** = OpenID
conformance suite plan; **Own** = our unit/integration/PG tests;
**Diag** = built-in diagnostic.

### 13.1 SMART App Launch 2.2 (hl7.org/fhir/smart-app-launch, v2.2.0)

| Item (section) | Phase | How | Test |
|---|---|---|---|
| `.well-known/smart-configuration` required fields: `grant_types_supported`, `token_endpoint`, `capabilities`, `code_challenge_methods_supported`; conditional `issuer`, `jwks_uri`, `authorization_endpoint` ("Conformance") | P1 | `metadata::smart_configuration` per bound endpoint | Inf discovery, Own (P0 test), Diag 1 |
| Recommended: `scopes_supported`, `response_types_supported`, `management_endpoint`, `introspection_endpoint`, `revocation_endpoint`; optional `token_endpoint_auth_methods_supported`, `registration_endpoint` | P1 (registration P2) | same | Inf, Own |
| `user_access_brand_bundle` / `user_access_brand_identifier` (User-access Brands) | P2 | realm branding → Brand Bundle (Organization + Endpoint) served by the bound endpoint | Own |
| `associated_endpoints` (experimental) | P3 | list other endpoints of the realm | Own |
| CapabilityStatement `oauth-uris` extension | P1 | existing discovery code, fed from the realm | Inf |
| `launch-ehr` | P1 | console EHR launcher + launch handles (§8.3) | Inf EHR launch, Diag |
| `launch-standalone` | P1 | §8.2 | Inf standalone, Diag 3 |
| `authorize-post` | P1 | `/authorize` accepts form POST | Inf (STU2.2), Own |
| `client-public` | P1 | `ClientAuth::None` + PKCE | Inf |
| `client-confidential-symmetric` | P1 | `client_secret_basic`/`_post`, argon2id | Inf |
| `client-confidential-asymmetric` (RS384/ES384, `jti`, `jku`, 5-min `exp`, `invalid_client`) | P1 | §8.4 | Inf backend services + asymmetric standalone, Own |
| `sso-openid-connect` (`openid`, `fhirUser`, id_token) | P1 | §7.2 | Inf OpenID group, OIDF basic |
| `context-banner` (`need_patient_banner`) | P1 | launch handle field; standalone default `true` for patient realms | Inf, Own |
| `context-style` (`smart_style_url`, experimental) | P2 | realm branding → style JSON URL | Own |
| `context-ehr-patient`, `context-ehr-encounter` | P1 | launch handle | Inf EHR |
| `context-standalone-patient` (patient picker), `context-standalone-encounter` | P1 | §8.2 step 3 | Inf standalone |
| `fhirContext` (reference/canonical/identifier/type/role) | P1 | launch handle + `launch/<type>?role=` scopes | Own (P0 test), Inf STU2.2 |
| `intent`, `tenant` | P1 | launch handle fields | Own |
| `permission-offline` (`offline_access`) | P1 | §7.4 | Inf refresh |
| `permission-online` (`online_access`, experimental) | P2 | refresh family bound to SSO session | Own |
| `permission-patient`, `permission-user` | P1 | scope policy + picker | Inf |
| `permission-v1` (`.read`/`.write`/`.*`) | P1 | `scope.rs` v1 → v2 coverage | Inf STU1-style scopes, Own |
| `permission-v2` (`cruds`), granular `?param=` scopes | P1 | `scope.rs` (query must match exactly; enforcement already in `helios-auth`) | Inf STU2 granular scopes, Own |
| `smart-app-state` (experimental, deprecated endpoint) | P3 | app state store + endpoint | Own |
| PKCE S256 required, `plain` SHALL NOT | P1 | `pkce.rs` | Inf, Own (P0) |
| `aud` required; `resource` as synonym | P1 | §8.2 | Inf (invalid aud), Own |
| Token response `Cache-Control: no-store`, `Pragma: no-cache` | P1 | token handler | Inf |
| Refresh: same client, narrower scopes, optional rotation | P1 | §7.4 | Inf refresh, Own |
| Token introspection fields (`active scope client_id exp`, context, `iss`/`sub`/`fhirUser`) | P1 | §7.6 | Inf introspection |
| Backend Services: client_credentials, system scopes, ≤ 300 s, no refresh, RFC 6749 §5.2 errors | P1 | §8.4 | Inf backend services |
| Scopes and launch context: `launch`, `launch/patient`, `launch/encounter`, `openid`, `fhirUser`, `profile` | P1 | `scope.rs`, `metadata.rs` | Inf |

### 13.2 OAuth 2.0 / 2.1 family

| Spec | Item | Phase | How | Test |
|---|---|---|---|---|
| RFC 6749 | authorization code grant (§4.1) | P1 | §8.2 | Inf, OIDF |
| RFC 6749 | client credentials (§4.4) | P1 | §8.4 (asymmetric; symmetric optional per client) | Inf backend |
| RFC 6749 | refresh (§6) | P1 | §7.4 | Inf, OIDF refresh-token module |
| RFC 6749 | **implicit (§4.2)** | ✗ | removed in OAuth 2.1 §10.1 and SMART 2.0; RFC 9700 §2.1.2 "SHOULD NOT" | Own: `unsupported_response_type` |
| RFC 6749 | **resource owner password (§4.3)** | ✗ | RFC 9700 §2.4 "MUST NOT"; OAuth 2.1 removes it. The console "test token" feature covers testing needs. | Own: `unsupported_grant_type` |
| RFC 6749 | errors (§4.1.2.1, §5.2) | P1 | `error.rs` | Own, OIDF |
| RFC 6750 | bearer header (§2.1), form body (§2.2) | P1 | resource server (existing) | Own |
| RFC 6750 | **query parameter (§2.3)** | ✗ | RFC 9700 §4.3.2; OAuth 2.1 | Own |
| OAuth 2.1 draft | PKCE for all, exact redirect match, no implicit/password, refresh sender-constraint or rotation | P1 | D-5, D-6 | Own |
| RFC 7636 | PKCE S256 | P1 | `pkce.rs` (RFC test vector) | Own, Inf |
| RFC 7636 | **`plain`** | ✗ | SMART 2.2 "SHALL NOT" | Own |
| RFC 7662 | introspection | P1 | §7.6 | Inf, Own |
| RFC 9701 | JWT introspection response | P2 | `Accept: application/token-introspection+jwt` | Own |
| RFC 7009 | revocation | P1 | §7.6 | Own, Diag 9 |
| RFC 7591 | dynamic registration (open public / initial access token / software statement) | P2 | `DcrPolicy` (P0) | OIDF dynamic, Own |
| RFC 7592 | registration management | P2 | registration access token (`hmrat_`) | Own |
| RFC 8414 | AS metadata (path insertion) | P1 | §5.1 | Own, OIDF config |
| RFC 9068 | JWT access tokens (`at+jwt`) | P1 | §7.1 (P0 claims + signing) | Own (cross-verified with `jsonwebtoken`) |
| RFC 7523 | `private_key_jwt` client auth (§2.2) | P1 | §8.4 | Inf, OIDF |
| RFC 7523 | JWT bearer authorization grant (§2.1) | P3 | trusted issuer JWT → token (system integrations) | Own |
| RFC 7521 | assertion framework | P1 | via 7523 | Own |
| RFC 9126 | PAR | P2 | `/par`, `request_uri` single use, `require_par` per client | OIDF FAPI2 (P3), Own |
| RFC 9101 | JAR (signed request objects) | P3 | verify with client JWKS; required under FAPI 2 message signing | OIDF |
| RFC 9396 | RAR (`authorization_details`) | P3 | typed details → consent screen + token claim | Own |
| RFC 9449 | DPoP (sender-constrained AT + RT, nonces) | P2 | `cnf.jkt`, `token_type=DPoP`; internal verifier checks the proof | OIDF FAPI2-DPoP (P3), Own |
| RFC 8705 | mTLS client auth + bound tokens | P3 | TLS terminates at nginx; client cert forwarded in a header from a trusted proxy only | Own |
| RFC 8628 | device authorization grant | P2 | `/device_authorization`, user code page, `slow_down` | Own |
| RFC 8693 | token exchange | P2 | impersonation/delegation between realm endpoints; downscoping | Own |
| RFC 8707 | resource indicators (multi-aud) | P1 single `resource` = `aud`; P2 multiple | §8.2 | Own |
| RFC 9207 | `iss` in authorization response | P1 | §8.2 step 5 | OIDF, Own |
| RFC 9700 | security BCP (exact redirect, PKCE, refresh rotation, no implicit/ROPC, no tokens in URLs, mix-up defence) | P1 | §10.4 | Own, review |
| RFC 8252 | native apps (loopback any port, private-use schemes, no `localhost`) | P1 | `client.rs` | Own (P0) |
| RFC 9728 | OAuth Protected Resource Metadata on FHIR endpoints | P2 | `{fhir-base}/.well-known/oauth-protected-resource` + `WWW-Authenticate resource_metadata` | Own |
| RFC 7517/7518/7638 | JWK/JWA/thumbprint | P1 | `keys.rs` (RFC 7638 vector) | Own (P0) |
| OAuth 2.0 Form Post Response Mode | `response_mode=form_post` | P1 | auto-submitting form (CSP nonce) | OIDF formpost |
| OAuth 2.0 Multiple Response Types | `response_mode=query` | P1 | default | OIDF |
| FAPI 2.0 Security Profile | PAR + PKCE + DPoP/mTLS + `private_key_jwt` | P3 | profile switch per realm | OIDF `fapi2-security-profile` |
| CIBA | backchannel authentication | not planned | no healthcare demand seen; revisit on request | — |

### 13.3 OpenID Connect

| Spec | Item | Phase | How | Test |
|---|---|---|---|---|
| Core §3.1 | authorization code flow, id_token, `nonce`, `at_hash`, `auth_time` | P1 | §7.2 | OIDF basic |
| Core §3.1.2.1 | `prompt` (none, login, consent, select_account), `max_age`, `login_hint`, `ui_locales`, `acr_values` | P1 (acr values P2 with MFA) | §8.2 | OIDF basic (prompt/max_age modules) |
| Core §5.3 | userinfo (GET/POST, bearer) | P1 | `profile`, `email`, `fhirUser` claims | OIDF basic |
| Core §5.5 | `claims` request parameter | P3 | — | OIDF |
| Core §6 | request objects (`request`, `request_uri`) | P3 (with JAR) | — | OIDF |
| Core §8 | pairwise subject identifiers | P3 | per-sector `sub` | OIDF |
| Core §3.2/3.3 | **implicit and hybrid flows** | ✗ | front-channel tokens; OAuth 2.1 §10.1; SMART 2 code only | Own |
| Discovery 1.0 | `/.well-known/openid-configuration` | P1 | `metadata.rs` | OIDF config |
| Dynamic Registration 1.0 | OIDC client metadata | P2 | with RFC 7591 | OIDF dynamic |
| RP-Initiated Logout 1.0 | `end_session_endpoint` | P1 | §8.5 | OIDF rp-initiated-logout |
| Front-Channel Logout 1.0 | iframes to RPs | P2 | logout page | OIDF frontchannel |
| Back-Channel Logout 1.0 | `logout+jwt` POST to RPs | P2 | outbox, retries | OIDF backchannel |
| Session Management 1.0 | `check_session_iframe` | ✗ | broken by third-party-cookie blocking; superseded by back-channel logout | — |
| Form Post Response Mode | see 13.2 | P1 | | OIDF |

### 13.4 UDAP / HL7 FAST Security IG (v2.0.0 STU2) — **include, phase 3**

| Item | Phase | How | Test |
|---|---|---|---|
| UDAP discovery `{fhir-base}/.well-known/udap` with `signed_metadata` (x5c, RS256) | P3 | per Internal endpoint, signed by a realm UDAP certificate | Inferno UDAP Security test kit |
| Trusted dynamic registration (software statement JWT signed by a community certificate, `x5c` chain validation to trust anchors, certifications) | P3 | `DcrPolicy::SoftwareStatement` + `hfs_idp_udap_trust_anchors` | Inferno UDAP |
| Consumer-facing (auth code) and B2B (client_credentials + `hl7-b2b` extension) | P3 | reuses P1 flows; B2B extension claims → audit + policy | Inferno UDAP |
| Tiered OAuth for user authentication (`udap` scope, `idp` parameter) | P3 | built on upstream OIDC brokering (P2) with dynamic registration at the upstream IdP | Inferno UDAP |
| JWT-based client authentication with x5c | P3 | `private_key_jwt` variant | Inferno UDAP |

Why include it: UDAP is how TEFCA-era and US B2B FHIR exchange does
scalable registration. It sits naturally on DCR, brokering and certificates.
It is not needed for SMART certification, so it goes in phase 3.

### 13.5 Product features beyond specs ("what others support")

| Feature | Keycloak | Smile CDR | Epic | HAPI FHIR (OSS) | Medplum | **HFS** |
|---|---|---|---|---|---|---|
| Built-in AS | ✓ | ✓ (SMART Outbound Security) | ✓ | ✗ (resource server only) | ✓ | P1 |
| Realms / multi-tenant AS | ✓ | ✓ (multiple modules) | per org | — | per project | P1 |
| SMART EHR + standalone launch, patient picker | via extensions | ✓ | ✓ | — | ✓ | P1 |
| Backend Services | client creds (no SMART specifics) | ✓ | ✓ | — | ✓ | P1 |
| Introspection / revocation | ✓ | ✓ | ✓ (introspection) | — | partial | P1 |
| Consent screen + remembered consent | ✓ | ✓ | ✓ | — | ✓ | P1 |
| DCR | ✓ | ✓ | patient-app DCR | — | — | P2 |
| PAR / DPoP / device flow / token exchange | ✓ | partial | — | — | partial (token exchange) | P2 |
| MFA: TOTP / WebAuthn | ✓ / ✓ | ✓ (TOTP) | via EHR | — | ✓ (TOTP) | P2 / P3 |
| Identity brokering (OIDC) | ✓ (OIDC, SAML, social) | ✓ (federated OAuth2) | — | — | ✓ (external IdPs) | P2 (OIDC only; SAML/LDAP → External Keycloak) |
| LDAP/Kerberos/SAML | ✓ | LDAP | — | — | — | ✗ (use External) |
| Custom claim scripts | mappers/SPI | JS callback scripts | — | — | access policies | P2: declarative claim mappers (no scripting engine) |
| UDAP | community extension | ✓ (recent versions) | ✓ | — | — | P3 |
| Password grant / implicit | ✓ (legacy toggles) | ✓ (legacy) | — | — | — | ✗ (BCP) |
| Admin console with wizard + self-test | admin UI (no SMART self-test) | console | — | — | app | P1 (wizard + diagnostic) |

Feature claims about third-party products come from public documentation as
of 2026 and should be re-checked before marketing use. They do not affect
the design.

---

## 14. Testing strategy

1. **Unit** (P0 onwards): spec test vectors (RFC 7636 App. B, RFC 7638 §3.1, OIDC Core A.3 `at_hash`). Tokens signed here verify with the resource server's `jsonwebtoken`. Scope algebra, redirect rules, rotation planner and refresh decisions.
2. **Property and fuzz**: `proptest` for `scope::grant` (granted ⊆ requested, granted covered by policy) and `is_narrowing`. `cargo-fuzz` targets for the authorize parser, scope parser, client assertion parser and DCR metadata, in nightly CI.
3. **Integration** (`crates/idp-server/tests`): axum test router + memory store, covering the full flows in §11.3 (the diagnostic steps double as integration tests).
4. **PG** (`pg_idp`, testcontainers pattern of `pg_console`): atomic consumption under concurrency (100 parallel `take_code`, exactly one wins), refresh CAS race (one rotates, the other sees reuse), epoch reload, migrations idempotent (run twice).
5. **Conformance in CI** (`.github/workflows/idp-conformance.yml`, nightly and on `crates/idp*` changes):
   * **Inferno SMART App Launch test kit** (docker compose) against an HFS with a seeded realm and endpoint. Suites: STU2.2 (or STU2 where STU2.2 groups are missing): Standalone Patient App, EHR Practitioner App, OpenID Connect, Token Refresh, Token Introspection, Backend Services, Invalid AUD/launch/PKCE. Browser steps are scripted with Playwright, which drives our hosted login and consent pages.
   * **OpenID conformance suite** (docker): `oidcc-basic-certification-test-plan`, `oidcc-config-certification-test-plan`, `oidcc-formpost-basic-certification-test-plan`, `oidcc-rp-initiated-logout-certification-test-plan` (P1); `oidcc-dynamic-certification-test-plan`, `oidcc-backchannel-rp-initiated-logout-certification-test-plan`, `oidcc-frontchannel-rp-initiated-logout-certification-test-plan` (P2); `fapi2-security-profile-final-test-plan` (P3).
   * **Inferno UDAP Security test kit** (P3).
   * **ZAP baseline** scan of the hosted pages.
6. **Diagnostic** in production (§11.3): after every realm change, on demand, and nightly per realm, with metrics `hfs_idp_diagnostic_success`.
7. **Pentest** (external) before production PHI (D-14).

---

## 15. Work breakdown

Owned files are disjoint so packages can run in parallel worktrees. Contracts
from P0 are frozen; changing them requires an I0 follow-up commit that every
package rebases on.

| Pkg | Scope | Owns | Depends on | Acceptance |
|---|---|---|---|---|
| **I0** (done) | core types, keys, scope, PKCE, tokens, metadata, store trait, migration SQL, `idp` reserved segment | `crates/idp/**`, `crates/endpoints/src/name.rs` (one line) | — | 40 unit tests; clippy `-D warnings` |
| **I1** | protocol endpoints: authorize, token (code/refresh/client_credentials), jwks, discovery (both), introspect, revoke, userinfo, logout; `RealmRuntime` snapshot | `crates/idp-server/src/{routes,flows,runtime}/**` | I0; I2/I3 via traits (memory impls until merged) | integration tests for §11.3 steps 1–4, 6, 8–10 |
| **I2** | PG store, wiring `hm-0020` into `FORK_MIGRATIONS`, cleanup job | `crates/idp/src/pg.rs` (feature `postgres`), one line in `persistence/.../schema.rs` | I0 | `pg_idp` tests (concurrency, idempotent migration) |
| **I3** | client authentication: secret (argon2id), private_key_jwt (JWKS fetch through the outbound guard, `jti`, `jku`), DPoP-ready verifier interface | `crates/idp-server/src/client_auth/**` | I0 | negative-case suite (alg confusion, replay, multi-aud, `jku` mismatch, SSRF) |
| **I4** | hosted pages: login, consent, patient picker (in-process FHIR search), account/apps, logout, errors; i18n en/zh-TW; CSP; branding | `crates/idp-server/templates/**`, `src/pages/**`, `locales/idp-*` | I1 routes (stubs early) | Playwright flows; ZAP baseline clean; a11y AA |
| **I5** | resource-server integration: `AuthProviderRef`, `InternalVerifier`, grant deny-set, SMART discovery for Internal endpoints, `$test-auth` support | `crates/auth/src/{internal.rs,verifier_cache.rs}`, `crates/endpoints/src/model.rs` (auth section) | I0, I2 | existing auth tests green; internal-token tests; revocation within 2 s across 2 instances |
| **I6** | console: realm list/pages, **wizard**, users/clients/keys/sessions/grants pages, endpoint Auth tab, EHR launcher, diagnostic UI | `crates/ui/src/idp/**`, `crates/ui/templates/idp/**` | I7 | Playwright wizard e2e (each template), UX audit sign-off |
| **I7** | admin API `/admin/idp/**` incl. `$diagnose` runner | `crates/idp-server/src/admin/**`, `src/diagnostic/**` | I1, I2 | API tests; diagnostic green against a seeded realm |
| **I8** | audit events, rate limits, metrics, nginx rule, `HFS_IDP` flag wiring in the `hfs` binary, ops doc | `crates/idp-server/src/{audit,limits}.rs`, `crates/hfs/src/idp.rs`, `docker/production/nginx.conf`, `docs/ops/idp.md` | I1 | audit events asserted per flow; 429 behaviour |
| **I9** | conformance CI: Inferno + OIDF compose, seed scripts, Playwright drivers | `.github/workflows/idp-conformance.yml`, `tools/idp-conformance/**` | I1, I4, I5 | **release gate D-14** green |
| I10–I15 (P2) | DCR; PAR + DPoP; TOTP; upstream OIDC brokering; front/back-channel logout; device flow + token exchange; RFC 9701/9728 | new modules under `crates/idp-server/src/` + `hm-0021` | P1 | OIDF dynamic/logout plans; own |
| I16–I18 (P3) | UDAP; JAR/RAR/mTLS/FAPI 2; WebAuthn; SMART App State | + `hm-0022` | P2 | Inferno UDAP; OIDF FAPI2 |

**Merge order:** I0 → {I2, I3} → I1 → {I4, I5, I7, I8} → I6 → I9 (gate) →
`HFS_IDP` allowed on staging → pentest → production opt-in.

**Contracts frozen by I0:** `IdpStore` (store.rs), `RealmSpec`/`ClientSpec`/`RealmUser` JSON (camelCase), `SigningKey` API + AAD format, `AccessTokenClaims` (incl. `gid`), handle prefixes, `metadata::*` outputs, migration name `hm-0020-idp-core`.

---

## 16. Rollout and alignment with the unification line

* **Unified server:** the IdP is a **module of the single HFS process**, with no extra binary or port. `crates/idp` stays HTTP-free, so the unified server can mount `crates/idp-server`'s router wherever its composition root lives (today `crates/hfs`; after unification, the merged binary). Console pages live in the shared console (`crates/ui`) under the same chrome, roles and CSRF. Tables live in the shared PG under their own migration block (D-10). If the unified design (`docs/design/unified-server.md`, branch `hm/design-unify`; not yet published when this was written) renames the composition root or the console URL scheme, only I6/I8 wiring changes.
* **UX audit:** the wizard and management pages follow the console's information architecture. Wizard copy and the scope-builder vocabulary go to the UX line for review before I6 starts.
* **Flag stages:** (1) merge with `HFS_IDP=off`: schema created, nothing mounted. (2) Staging `on`: create a realm with the "Everything" template, bind a test endpoint, run the diagnostic + Inferno + OIDF. (3) Production `on` with no endpoint bound (realms usable for test apps only). (4) After the pentest, bind production endpoints per tenant. Rollback = unbind endpoints (they return to External/Off), then flag off. Data stays.
* **Keycloak at `/auth`** stays as the External reference for regression tests. Realm-to-Keycloak brokering (P2) lets a realm reuse Keycloak users.

---

## 17. Open items (decided defaults, revisit later)

1. **HSM/KMS signing** (GCP KMS, PKCS#11): default = sealed software keys. The `SigningKey` API is narrow (`sign`, `public_jwk`) so a KMS-backed signer can be added behind the same interface.
2. **SQLite store** for single-node dev: default = IdP requires PG. A memory store exists for tests.
3. **Email transport** for invitations and password reset (P2): default = admin-issued one-time links.
4. **OIDF certification** (paid): decide after P2.
5. **Console sign-in through an internal realm** (the console as an OIDC client of its own realm): possible after P1. Break-glass `HFS_ADMIN_PASSWORD` stays mandatory to avoid lock-out loops.
6. **Patient self-enrolment with identity proofing**: out of scope; P2 offers invitations.

---

## 18. References

* HL7 SMART App Launch 2.2.0 (STU 2.2): Conformance; App Launch; Scopes and Launch Context; Backend Services; Client Authentication – Asymmetric / Symmetric; Token Introspection — <https://hl7.org/fhir/smart-app-launch/>
* HL7 FAST Security for Scalable Registration, Authentication, and Authorization (UDAP) 2.0.0 — <https://hl7.org/fhir/us/udap-security/>
* RFC 6749, 6750, 7009, 7517, 7518, 7521, 7523, 7591, 7592, 7636, 7638, 7662, 8252, 8414, 8628, 8693, 8705, 8707, 9068, 9101, 9126, 9207, 9396, 9449, 9700, 9701, 9728; draft-ietf-oauth-v2-1; draft-ietf-oauth-rfc7523bis
* OpenID Connect Core 1.0, Discovery 1.0, Dynamic Client Registration 1.0, RP-Initiated / Front-Channel / Back-Channel Logout 1.0, Form Post Response Mode; FAPI 2.0 Security Profile
* Inferno SMART App Launch test kit — <https://github.com/inferno-framework/smart-app-launch-test-kit>; Inferno UDAP Security test kit; OpenID Foundation conformance suite — <https://gitlab.com/openid/conformance-suite>
* Fork design: `docs/design/fhir-server-manager.md` (D-6 epochs, D-8 verifier cache, D-12 sealed secrets, D-13 outbound policy)

---

## 19. Changelog

### I0 — package 0 (`hm/design-idp`)
* New crate `crates/idp` (`helios-idp`), workspace default member. Dependencies: `ring` (already in the binary via jsonwebtoken) and `aws-lc-rs` (already in the lock via rustls; used for RSA keygen only). No new third-party crates in `Cargo.lock`.
* Modules: `realm`, `client`, `scope`, `pkce`, `keys`, `token`, `launch`, `user`, `metadata`, `store`, `schema`, `error`, `util`.
* `helios-endpoints`: `idp` added to `RESERVED_SEGMENTS`, so no FHIR endpoint can take `/idp`.
* Migration SQL `hm-0020-idp-core` defined (`schema::IDP_CORE_SQL`). Wiring it into `FORK_MIGRATIONS` is package I2.
* Tests: 40 unit tests (RFC 7636 / 7638 / OIDC `at_hash` vectors; ES256/ES384/RS256/RS384/PS256/PS384 tokens verified by `jsonwebtoken`; sealing AAD binding; rotation planner; scope algebra; redirect rules; refresh decisions; SMART/AS metadata; migration idempotency lint).
