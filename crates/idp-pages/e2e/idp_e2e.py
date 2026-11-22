#!/usr/bin/env python3
"""Browser end-to-end tests of the built-in authorization server's hosted
pages (docs/design/authorization-server.md, packages I4/I5/I8).

Runs against a live `hfs` with HFS_IDP=on, HFS_MANAGER=on, the dev seed
`seed.json` (HFS_IDP_DEV_SEED) and HFS_ADMIN_PASSWORD set — see README.md.
Drives Chromium with Playwright (python), checks every flow, the security
controls and the FHIR side (internal-realm verification), and writes
screenshots (dark first, then light, phone, zh-Hant).

    HFS_E2E_BASE=http://localhost:19612 HFS_E2E_ADMIN_PASSWORD=... \\
        python3 idp_e2e.py --shots ./shots

Exit status 0 = every check passed.
"""

import argparse
import base64
import hashlib
import json
import os
import secrets
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

from playwright.sync_api import sync_playwright

BASE = os.environ.get("HFS_E2E_BASE", "http://localhost:19612").rstrip("/")
ADMIN = "Basic " + base64.b64encode(
    ("admin:" + os.environ.get("HFS_E2E_ADMIN_PASSWORD", "")).encode()
).decode()
CB = "https://app.example/cb"
CLIENT = "growth-chart"
LAB = f"{BASE}/lab"
WARD = f"{BASE}/ward"
CHROME = os.environ.get(
    "HFS_E2E_CHROME",
    os.path.expanduser("~/.cache/ms-playwright/chromium-1243/chrome-linux64/chrome"),
)
FAILS = []


def check(name, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + name + ("" if ok else f"  [{str(detail)[:500]}]"), flush=True)
    if not ok:
        FAILS.append(name)
    return ok


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *a, **k):
        return None


OPENER = urllib.request.build_opener(NoRedirect())


def http(method, url, body=None, headers=None, form=None):
    h = {"Accept": "application/fhir+json, application/json"}
    data = None
    if form is not None:
        data = urllib.parse.urlencode(form).encode()
        h["Content-Type"] = "application/x-www-form-urlencoded"
    elif body is not None:
        data = json.dumps(body).encode()
        h["Content-Type"] = "application/json" if "/admin/" in url else "application/fhir+json"
    h.update(headers or {})
    r = urllib.request.Request(url, data=data, method=method, headers=h)
    try:
        resp = OPENER.open(r, timeout=60)
        st, hd, raw = resp.status, resp.headers, resp.read()
    except urllib.error.HTTPError as e:
        st, hd, raw = e.code, e.headers, e.read()
    try:
        js = json.loads(raw) if raw else None
    except ValueError:
        js = raw.decode(errors="replace")
    return st, hd, js


def claims(jwt):
    p = jwt.split(".")[1]
    return json.loads(base64.urlsafe_b64decode(p + "=" * (-len(p) % 4)))


# ------------------------------------------------------------------ setup

def ensure_endpoints():
    """`lab` (realm clinic) and `ward` (realm other): created open, seeded
    with patients, then switched to Internal(realm) / full."""
    patients = [("p1", "Amy", "Shaw", "1980-04-01"), ("p2", "Ben", "Shaw", "2015-09-12"),
                ("p3", "Cleo", "Ortiz", "1955-01-30")]
    for name, realm in (("lab", "clinic"), ("ward", "other")):
        st, hd, ep = http("GET", f"{BASE}/admin/endpoints/{name}", headers={"Authorization": ADMIN})
        if st == 404:
            st, hd, ep = http("POST", f"{BASE}/admin/endpoints", {
                "name": name, "displayName": name.title(), "fhirVersion": "R4",
                "auth": {"mode": "off"}}, headers={"Authorization": ADMIN})
            check(f"endpoint {name} created", st == 201, (st, ep))
        for _ in range(30):
            if http("GET", f"{BASE}/{name}/metadata")[0] in (200, 401):
                break
            time.sleep(1)
        st, hd, ep = http("GET", f"{BASE}/admin/endpoints/{name}", headers={"Authorization": ADMIN})
        spec = ep.get("spec", ep)
        if (spec.get("auth") or {}).get("provider", {}).get("kind") != "internal":
            for pid, given, family, born in patients:
                st, _, b = http("PUT", f"{BASE}/{name}/Patient/{pid}", {
                    "resourceType": "Patient", "id": pid, "birthDate": born,
                    "name": [{"given": [given], "family": family}]})
                check(f"{name}: seed Patient/{pid}", st in (200, 201), (st, b))
                st, _, b = http("PUT", f"{BASE}/{name}/Observation/o-{pid}", {
                    "resourceType": "Observation", "id": f"o-{pid}", "status": "final",
                    "code": {"text": "weight"}, "subject": {"reference": f"Patient/{pid}"}})
                check(f"{name}: seed Observation/o-{pid}", st in (200, 201), (st, b))
            spec["auth"] = {"mode": "full", "provider": {"kind": "internal", "realm": realm},
                            "acceptAdminCredential": False}
            body = {k: v for k, v in spec.items() if k not in ("id", "revision", "createdAt", "updatedAt", "status")}
            st, hd2, b = http("PUT", f"{BASE}/admin/endpoints/{name}", body,
                              headers={"Authorization": ADMIN, "If-Match": hd.get("ETag") or "*"})
            check(f"endpoint {name} -> Internal({realm})", st == 200, (st, b))
    for _ in range(30):
        if http("GET", f"{LAB}/Patient/p1")[0] == 401:
            break
        time.sleep(1)


# ------------------------------------------------------------ browser bits

def pkce():
    v = secrets.token_urlsafe(48)
    c = base64.urlsafe_b64encode(hashlib.sha256(v.encode()).digest()).rstrip(b"=").decode()
    return v, c


def authorize_url(realm, aud, scope, challenge, extra=None, redirect=CB):
    q = {"response_type": "code", "client_id": CLIENT, "redirect_uri": redirect, "scope": scope,
         "state": "st-" + secrets.token_hex(4), "aud": aud, "code_challenge": challenge,
         "code_challenge_method": "S256"}
    q.update(extra or {})
    return f"{BASE}/idp/{realm}/authorize?" + urllib.parse.urlencode(q), q["state"]


class App:
    """Captures the redirect to the app (the browser never leaves)."""

    def __init__(self, ctx):
        self.hits = []
        ctx.route("https://app.example/**", self.handle)

    def handle(self, route):
        self.hits.append(route.request.url)
        route.fulfill(status=200, content_type="text/html", body="<h1>app</h1>")

    def last(self):
        return dict(urllib.parse.parse_qsl(urllib.parse.urlparse(self.hits[-1]).query)) if self.hits else {}


def token(realm, code, verifier):
    return http("POST", f"{BASE}/idp/{realm}/token", form={
        "grant_type": "authorization_code", "code": code, "redirect_uri": CB,
        "client_id": CLIENT, "code_verifier": verifier})


def sign_in(page, user, pw):
    page.fill("#idp-username", user)
    page.fill("#idp-password", pw)
    page.click("#idp-login-submit")
    page.wait_for_load_state()


def shot(page, shots, name):
    if shots:
        os.makedirs(shots, exist_ok=True)
        page.screenshot(path=os.path.join(shots, name + ".png"), full_page=True)


def set_theme(ctx, theme):
    ctx.add_cookies([{"name": "hfs_theme", "value": theme, "url": BASE}])


# ------------------------------------------------------------------- flows

def run(shots):
    ensure_endpoints()
    with sync_playwright() as pw:
        browser = pw.chromium.launch(executable_path=CHROME if os.path.exists(CHROME) else None)

        # 1. Standalone launch, patient (one linked record): login → consent.
        ctx = browser.new_context(color_scheme="dark", viewport={"width": 1280, "height": 900})
        set_theme(ctx, "dark")
        app = App(ctx)
        page = ctx.new_page()
        v, c = pkce()
        url, state = authorize_url("clinic", LAB, "openid fhirUser launch/patient patient/*.rs offline_access",
                                   c, {"nonce": "n-e2e"})
        page.goto(url)
        check("authorize -> hosted sign-in", "/idp/clinic/login?ix=hmix_" in page.url, page.url)
        resp = page.request.get(page.url)
        h = resp.headers
        check("login: CSP frame-ancestors 'none'", "frame-ancestors 'none'" in h.get("content-security-policy", ""), h)
        check("login: X-Frame-Options DENY", h.get("x-frame-options") == "DENY", h)
        check("login: no-store", h.get("cache-control") == "no-store", h)
        shot(page, shots, "01-login-dark")
        sign_in(page, "alice", "alice-password-1")
        check("patient with one record skips the picker", "/consent?ix=" in page.url, page.url)
        check("consent lists the app and patient",
              page.is_visible("text=Allow Growth Chart to access your data?") and page.is_visible("text=Patient: Amy Shaw"),
              page.content()[:400])
        shot(page, shots, "02-consent-dark")
        page.check("#idp-remember")
        page.click("#idp-consent-allow")
        page.wait_for_timeout(300)
        q = app.last()
        check("code returned with state and iss", q.get("code") and q.get("state") == state
              and q.get("iss") == f"{BASE}/idp/clinic", q)
        st, _, t = token("clinic", q.get("code", ""), v)
        check("token exchange", st == 200 and isinstance(t, dict) and t.get("access_token"), (st, t))
        t = t if isinstance(t, dict) else {}
        check("token response carries patient context", t.get("patient") == "p1" and t.get("need_patient_banner") is True, t)
        idt = claims(t["id_token"]) if t.get("id_token") else {}
        check("OIDC id_token with nonce and fhirUser", idt.get("nonce") == "n-e2e"
              and idt.get("fhirUser") == f"{LAB}/Patient/p1", idt)
        at = t.get("access_token", "")
        # FHIR side: Internal(realm) verification + confinement.
        st, _, b = http("GET", f"{LAB}/Patient/p1", headers={"Authorization": f"Bearer {at}"})
        check("internal endpoint accepts its realm's token", st == 200, (st, b))
        st, _, b = http("GET", f"{LAB}/Observation", headers={"Authorization": f"Bearer {at}"})
        txt = json.dumps(b)
        check("patient confinement on search", st == 200 and "o-p1" in txt and "o-p2" not in txt, (st, txt[:300]))
        st, _, b = http("GET", f"{LAB}/Patient/p2", headers={"Authorization": f"Bearer {at}"})
        check("other patient refused", st in (403, 404), (st, b))
        st, _, b = http("GET", f"{WARD}/Patient/p1", headers={"Authorization": f"Bearer {at}"})
        check("token refused by an endpoint of another realm", st == 401, (st, b))
        smart = http("GET", f"{LAB}/.well-known/smart-configuration")[2]
        check("SMART discovery from the realm", isinstance(smart, dict)
              and smart.get("authorization_endpoint") == f"{BASE}/idp/clinic/authorize", smart)
        # Revocation is immediate.
        st, _, _ = http("POST", f"{BASE}/idp/clinic/revoke", form={"token": at, "client_id": CLIENT})
        check("revoke answers 200", st == 200, st)
        st, _, b = http("GET", f"{LAB}/Patient/p1", headers={"Authorization": f"Bearer {at}"})
        check("revoked token refused immediately", st == 401, (st, b))

        # 2. Remembered consent: second authorization goes straight back.
        v, c = pkce()
        url, _ = authorize_url("clinic", LAB, "openid fhirUser launch/patient patient/*.rs offline_access", c)
        n = len(app.hits)
        page.goto(url)
        page.wait_for_timeout(300)
        check("SSO + remembered consent: no page shown", len(app.hits) == n + 1 and app.last().get("code"), page.url)
        # Connected apps: remove access.
        page.goto(f"{BASE}/idp/clinic/account/apps")
        check("connected apps lists Growth Chart", page.is_visible("text=Growth Chart"), page.content()[:300])
        shot(page, shots, "05-apps-dark")
        page.click(f"button[data-app='{CLIENT}']")
        page.wait_for_load_state()
        check("access removed", page.is_visible("text=Access for Growth Chart was removed."), page.content()[:300])
        page.goto(url.replace("st-", "st2-"))
        check("consent asked again after removal", "/consent?ix=" in page.url, page.url)
        # Deny → access_denied.
        page.click("#idp-consent-deny")
        page.wait_for_timeout(300)
        check("deny returns access_denied", app.last().get("error") == "access_denied", app.last())

        # 3. RP-initiated logout (confirmation page).
        page.goto(f"{BASE}/idp/clinic/logout?client_id={CLIENT}&post_logout_redirect_uri="
                  + urllib.parse.quote("https://app.example/bye") + "&state=bye1")
        check("logout asks for confirmation", page.is_visible("#idp-logout-submit"), page.content()[:300])
        shot(page, shots, "06-logout-dark")
        page.click("#idp-logout-submit")
        page.wait_for_timeout(300)
        check("logout redirects to the registered URI with state",
              app.hits[-1].startswith("https://app.example/bye") and "state=bye1" in app.hits[-1], app.hits[-1:])
        page.goto(f"{BASE}/idp/clinic/logout?client_id={CLIENT}&post_logout_redirect_uri="
                  + urllib.parse.quote("https://evil.example/"))
        check("unregistered post-logout URI: error page, no redirect",
              page.url.startswith(BASE) and page.is_visible("text=The sign-out request is not valid."), page.url)
        ctx.close()

        # 4. Caregiver picker + practitioner search (light theme).
        ctx = browser.new_context(color_scheme="light", viewport={"width": 1280, "height": 900})
        set_theme(ctx, "light")
        app = App(ctx)
        page = ctx.new_page()
        v, c = pkce()
        url, _ = authorize_url("clinic", LAB, "openid fhirUser launch/patient patient/*.rs", c)
        page.goto(url)
        shot(page, shots, "11-login-light")
        sign_in(page, "carol", "carol-password-1")
        check("caregiver gets the picker", "/select-patient?ix=" in page.url, page.url)
        check("picker lists linked patients only", page.is_visible("text=Ben Shaw") and not page.is_visible("text=Cleo Ortiz"),
              page.content()[:300])
        shot(page, shots, "12-picker-light")
        # Tamper: pick an unlinked patient by editing the radio value.
        page.eval_on_selector("#idp-patient-1", "e => e.value = 'p3'")
        page.click("#idp-picker-submit")
        page.wait_for_load_state()
        check("unlinked patient refused", page.is_visible("text=Choose one of the patients listed."), page.content()[:300])
        page.check("text=Ben Shaw")
        page.click("#idp-picker-submit")
        page.wait_for_load_state()
        shot(page, shots, "13-consent-light")
        page.click("#idp-consent-allow")
        page.wait_for_timeout(300)
        st, _, t = token("clinic", app.last().get("code", ""), v)
        check("caregiver token for the picked patient", st == 200 and t.get("patient") == "p2", (st, t))
        ctx.close()

        ctx = browser.new_context(color_scheme="dark", viewport={"width": 1280, "height": 900})
        set_theme(ctx, "dark")
        app = App(ctx)
        page = ctx.new_page()
        v, c = pkce()
        url, _ = authorize_url("clinic", LAB, "openid fhirUser launch/patient user/*.rs", c)
        page.goto(url)
        sign_in(page, "drbob", "drbob-password-1")
        page.fill("#idp-q", "Ortiz")
        page.click("#idp-picker-search button")
        page.wait_for_load_state()
        check("practitioner search finds the patient", page.is_visible("text=Cleo Ortiz") and not page.is_visible("text=Amy Shaw"),
              page.content()[:300])
        shot(page, shots, "03-picker-search-dark")
        page.click("#idp-picker-submit")
        page.wait_for_load_state()
        page.click("#idp-consent-allow")
        page.wait_for_timeout(300)
        st, _, t = token("clinic", app.last().get("code", ""), v)
        check("practitioner token: picked patient + fhirUser", st == 200 and t.get("patient") == "p3"
              and claims(t["id_token"]).get("fhirUser") == f"{LAB}/Practitioner/dr1", (st, t))
        if st == 200:
            st2, _, b = http("GET", f"{LAB}/Patient", headers={"Authorization": f"Bearer {t['access_token']}"})
            check("practitioner user/*.rs token reads the endpoint", st2 == 200, (st2, str(b)[:200]))
        ctx.close()

        # 5. Security: lockout, CSRF, open redirect, clickjacking.
        ctx = browser.new_context(color_scheme="dark", viewport={"width": 1280, "height": 900})
        set_theme(ctx, "dark")
        app = App(ctx)
        page = ctx.new_page()
        v, c = pkce()
        url, _ = authorize_url("clinic", LAB, "openid launch/patient patient/*.rs", c)
        page.goto(url)
        for i in range(3):
            sign_in(page, "lockme", "wrong-password-x")
        check("generic error, then lockout after 3 failures", page.is_visible("text=Too many failed attempts"), page.content()[:400])
        shot(page, shots, "07-locked-dark")
        sign_in(page, "lockme", "lockme-password-1")
        check("locked account refuses the right password", page.is_visible("text=Too many failed attempts"), page.url)
        # CSRF: corrupt the token.
        page.goto(url)
        page.eval_on_selector("input[name=csrf]", "e => e.value = 'forged'")
        sign_in(page, "alice", "alice-password-1")
        check("forged CSRF token refused", page.is_visible("text=This form has expired."), page.content()[:300])
        shot(page, shots, "08-csrf-dark")
        # Cross-site POST (another origin submits the form).
        evil = ctx.new_page()
        evil.route("https://evil.example/**", lambda r: r.fulfill(status=200, content_type="text/html", body=(
            f"<form id=f method=post action='{BASE}/idp/clinic/login'>"
            "<input name=ix value=x><input name=csrf value=x><input name=username value=alice>"
            "<input name=password value=alice-password-1></form><script>f.submit()</script>")))
        evil.goto("https://evil.example/attack")
        evil.wait_for_load_state()
        check("cross-site form post refused", evil.url.startswith(BASE) and "/consent" not in evil.url
              and not evil.is_visible("text=Allow Growth Chart"), evil.url)
        # Open redirect: unregistered redirect_uri.
        n = len(app.hits)
        bad, _ = authorize_url("clinic", LAB, "openid", c, redirect="https://evil.example/cb")
        evil.goto(bad)
        check("unregistered redirect_uri: no redirect", evil.url.startswith(BASE) and len(app.hits) == n, evil.url)
        shot(evil, shots, "09-bad-redirect-dark")
        # Clickjacking: frame the login page from another origin.
        page.goto(url)
        login_url = page.url
        frame_page = ctx.new_page()
        frame_page.route("https://evil.example/**", lambda r: r.fulfill(
            status=200, content_type="text/html", body=f"<iframe id=x src='{login_url}' width=600 height=600></iframe>"))
        frame_page.goto("https://evil.example/frame")
        frame_page.wait_for_timeout(1500)
        fr = frame_page.frames[1] if len(frame_page.frames) > 1 else None
        framed_ok = True
        if fr is not None:
            try:
                framed_ok = fr.query_selector("#idp-login-form") is None
            except Exception:
                framed_ok = True
        check("login page refuses to be framed", framed_ok, fr.url if fr else None)
        # Unknown interaction.
        page.goto(f"{BASE}/idp/clinic/login?ix=hmix_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        check("unknown interaction: error page", page.is_visible("text=expired or was already used"), page.content()[:200])
        shot(page, shots, "10-error-dark")
        ctx.close()

        # 6. Phone width + zh-Hant.
        ctx = browser.new_context(color_scheme="dark", viewport={"width": 390, "height": 844}, device_scale_factor=2)
        set_theme(ctx, "dark")
        App(ctx)
        page = ctx.new_page()
        v, c = pkce()
        url, _ = authorize_url("clinic", LAB, "openid fhirUser launch/patient patient/*.rs offline_access", c)
        page.goto(url + "&ui_locales=zh-Hant")
        check("ui_locales=zh-Hant renders Chinese", page.is_visible("text=登入 Riverside Clinic 以繼續使用 Growth Chart。"), page.content()[:300])
        check("brand stays English", "HealthyMind" in page.inner_text(".auth-foot") and "療心" not in page.content())
        no_scroll = page.evaluate("document.documentElement.scrollWidth <= window.innerWidth")
        check("phone: no horizontal scroll", no_scroll)
        shot(page, shots, "21-login-phone-zh-dark")
        sign_in(page, "alice", "alice-password-1")
        shot(page, shots, "22-consent-phone-zh-dark")
        ctx.close()
        browser.close()


def audit_checks(audit_file=None):
    """BALP AuditEvents recorded for the auth events: from the audit sink's
    NDJSON file (HFS_AUDIT_BACKEND=file) when given, else the admin API."""
    if audit_file:
        events = []
        with open(audit_file) as f:
            for line in f:
                line = line.strip()
                if line:
                    try:
                        events.append(json.loads(line))
                    except ValueError:
                        pass
        b = {"resourceType": "Bundle", "entry": [{"resource": e} for e in events]}
        st = 200
    else:
        st, _, b = http("GET", f"{BASE}/admin/endpoints/default/audit?_count=500", headers={"Authorization": ADMIN})
    text = json.dumps(b)
    check("AuditEvents readable", st == 200, (st, text[:200]))
    for what, needle in [("login", '"110122"'), ("logout", '"110123"'), ("token issuance (ITI-71)", "ITI-71"),
                         ("IUA.71 profile", "IHE.IUA.71"), ("consent", "consent-granted"),
                         ("AuthZconsent profile", "IHE.BasicAudit.AuthZconsent"),
                         ("revocation", "token-revoked"), ("login failure", '"outcome": "4"')]:
        check(f"audit: {what}", needle in text, needle)
    return b


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--shots", default=None)
    ap.add_argument("--audit-out", default=None)
    ap.add_argument("--audit-file", default=None, help="NDJSON written by HFS_AUDIT_BACKEND=file")
    ap.add_argument("--audit-only", action="store_true")
    a = ap.parse_args()
    if not a.audit_only:
        run(a.shots)
    b = audit_checks(a.audit_file)
    if a.audit_out:
        with open(a.audit_out, "w") as f:
            json.dump(b, f, indent=1)
    print(f"\n{len(FAILS)} failed" if FAILS else "\nall passed")
    sys.exit(1 if FAILS else 0)
