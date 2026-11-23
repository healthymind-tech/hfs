# Browser end-to-end tests — built-in authorization server

`idp_e2e.py` drives Chromium (Playwright for Python) through the hosted pages
of a live `hfs` and checks the protocol results, the security controls and the
FHIR side (Internal(realm) endpoints). Screenshots are written dark first,
then light, phone width and zh-Hant.

## Stack

```sh
export HFS_IDP=on HFS_MANAGER=on HFS_ADMIN_PASSWORD=admin-e2e-password
export HFS_SECRET_KEYS="k1:$(head -c32 /dev/urandom | base64)"
export HFS_IDP_DEV_SEED=crates/idp-pages/e2e/seed.json   # realms clinic + other
export HFS_BASE_URL=http://localhost:19612 HFS_SERVER_PORT=19612
export HFS_IDP_TRUST_FORWARDED=false HFS_IDP_RATE_INTERACTIVE_IP=1000   # the suite signs in many times from one address
export HFS_AUDIT_BACKEND=file HFS_AUDIT_FILE_PATH=/tmp/idp-audit.ndjson HFS_AUDIT_AUTH_EVENTS=true
hfs   # any storage backend; PostgreSQL keeps the IdP state across restarts
```

The browser must reach the server as `http://localhost:19612` (the issuer and
the Origin check use `HFS_BASE_URL`); forward the port when the server is
remote: `ssh -L 19612:127.0.0.1:19612 host`.

## Run

```sh
pip install playwright && playwright install chromium   # once
HFS_E2E_BASE=http://localhost:19612 HFS_E2E_ADMIN_PASSWORD=admin-e2e-password \
  python3 crates/idp-pages/e2e/idp_e2e.py --shots /tmp/idp-shots --audit-file /tmp/idp-audit.ndjson
```

The script creates the FHIR endpoints `lab` (realm `clinic`) and `ward`
(realm `other`) through the admin API, seeds three patients while they are
open, then switches them to `{"mode":"full","provider":{"kind":"internal",…}}`.
Exit status 0 means every check passed.
