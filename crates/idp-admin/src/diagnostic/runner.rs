//! Steps 1–10 of the diagnostic (design §11.3), run against the realm's
//! PUBLIC URLs as a client would: discovery, JWKS, authorization code +
//! PKCE through the real hosted login form, negative checks, a FHIR call on
//! every bound endpoint, refresh rotation and reuse detection, Backend
//! Services, introspection, revocation, userinfo and RP-initiated logout.
//!
//! The throw-away client pair and user ([`Fixture`]) are created and deleted
//! by the caller (the admin service); the runner itself only speaks HTTP.
//! Steps are executed in an order that keeps the tokens later steps need
//! alive (refresh-reuse detection revokes a family, revocation kills a
//! grant), and reported in design order.

// Step helpers return `Result<_, Check>` (a failed check is the error).
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use chrono::{DateTime, Utc};
use helios_idp::keys::{SigningAlg, SigningKey, thumbprint};
use helios_idp::pkce;
use helios_idp::realm::RealmFeatures;
use serde_json::{Value, json};
use url::Url;

use super::http::{
    Browser, Exchange, FetchError, PageKind, classify_page, page_text, query_map, resolve,
};
use super::preflight::{AppProbe, EndpointView};
use super::report::{Check, Report, Status, Step};

/// The per-run throw-away identities.
#[derive(Clone)]
pub struct Fixture {
    /// Public PKCE client (auth method `none`).
    pub public_client_id: String,
    /// Registered for the public client only; never fetched.
    pub redirect_uri: String,
    /// Backend Services client (`private_key_jwt`), if the realm has
    /// backend services on.
    pub backend_client_id: Option<String>,
    /// Its throw-away ES256 key (public JWK registered on the client).
    pub backend_key: Option<Arc<SigningKey>>,
    pub backend_scope: String,
    pub username: String,
    pub password: String,
    /// Patient the test user is linked to (patient-scope runs).
    pub patient: Option<String>,
    /// Scopes the public client requests.
    pub scopes: String,
}

impl std::fmt::Debug for Fixture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fixture")
            .field("public_client_id", &self.public_client_id)
            .field("backend_client_id", &self.backend_client_id)
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

/// Run parameters.
#[derive(Clone, Debug)]
pub struct RunConfig {
    pub realm: String,
    /// Public issuer (`{public_base}/idp/{realm}`); compared exactly.
    pub issuer: String,
    /// Send requests for the public origin here instead (same server,
    /// in-network address), keeping public URLs everywhere else.
    pub fetch_via: Option<Url>,
    /// Bound endpoints (public FHIR bases).
    pub endpoints: Vec<EndpointView>,
    pub features: RealmFeatures,
    pub fixture: Fixture,
    pub app: Option<AppProbe>,
    pub timeout: Duration,
    /// Largest tolerated difference between the server's and our clock.
    pub max_clock_skew_secs: i64,
    /// How long revocation may take to reach the FHIR endpoints.
    pub revocation_wait: Duration,
    /// The endpoints are about to be bound (wizard): use them as `aud` but
    /// do not expect them to trust the realm yet (endpoint checks skipped).
    pub audience_only: bool,
    /// Test hook: shifts the runner's own clock (simulated skew).
    #[doc(hidden)]
    pub clock_offset_secs: i64,
}

impl RunConfig {
    pub fn new(realm: &str, issuer: &str, fixture: Fixture) -> Self {
        Self {
            realm: realm.to_string(),
            issuer: issuer.trim_end_matches('/').to_string(),
            fetch_via: None,
            endpoints: Vec::new(),
            features: RealmFeatures::default(),
            fixture,
            app: None,
            timeout: Duration::from_secs(10),
            max_clock_skew_secs: 60,
            revocation_wait: Duration::from_secs(3),
            audience_only: false,
            clock_offset_secs: 0,
        }
    }
}

/// Tokens from one authorization-code flow.
#[derive(Debug, Clone, Default)]
struct Tokens {
    access: String,
    id_token: Option<String>,
    id_sub: Option<String>,
}

struct Ctx {
    cfg: RunConfig,
    b: Browser,
    meta: Option<Value>,
    jwks: Option<Value>,
    /// Endpoint base used as `aud` (first bound endpoint).
    aud: Option<String>,
}

impl Ctx {
    fn now(&self) -> DateTime<Utc> {
        Utc::now() + chrono::Duration::seconds(self.cfg.clock_offset_secs)
    }

    fn ep(&self, key: &str, fallback: &str) -> String {
        self.meta
            .as_ref()
            .and_then(|m| m.get(key))
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| format!("{}/{fallback}", self.cfg.issuer))
    }
}

const STEP_TITLES: [(&str, &str); 10] = [
    ("discovery", "Discovery documents"),
    ("jwks", "Signing keys (JWKS)"),
    ("authorization-code", "Sign-in: authorization code + PKCE"),
    ("negative", "Attacks are refused"),
    ("fhir", "FHIR call with the token"),
    ("refresh", "Refresh token rotation"),
    ("backend", "Backend Services"),
    ("introspection", "Token introspection"),
    ("revocation", "Revocation"),
    ("userinfo-logout", "Userinfo and sign-out"),
];

fn new_step(n: u8) -> Step {
    let (id, title) = STEP_TITLES[(n - 1) as usize];
    Step::new(n, id, title)
}

fn net_fail(id: &str, url: &str, e: &FetchError) -> Check {
    Check::fail(id, e.code(), format!("{url}: {e}"))
        .param("url", url)
        .param("detail", e.to_string())
        .url(url)
        .fix(match e {
            FetchError::Connect(_) => "The server could not reach its own public URL. Check HFS_BASE_URL / HFS_IDP_PUBLIC_BASE and the reverse proxy (nginx needs a `location /idp/` rule), or set HFS_IDP_DIAGNOSTIC_VIA to the in-network address.",
            FetchError::Timeout => "The request timed out. Check the reverse proxy and server load.",
            FetchError::Tls(_) => "The TLS certificate of the public URL is not trusted by the server. Fix the certificate chain or set HFS_IDP_DIAGNOSTIC_VIA to the in-network address.",
            FetchError::Other(_) => "Check the public URL configuration.",
        })
}

fn oauth_error(x: &Exchange) -> (String, String) {
    let j = x.json().unwrap_or(Value::Null);
    (
        j.get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        j.get("error_description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    )
}

/// Runs steps 1–10 and appends them to `report` (after step 0 if present).
pub async fn run(cfg: RunConfig, report: &mut Report) {
    let mut b = Browser::new(cfg.timeout);
    if let Some(via) = &cfg.fetch_via
        && let Ok(issuer) = Url::parse(&cfg.issuer)
    {
        let origin = Url::parse(&issuer.origin().ascii_serialization()).expect("origin");
        b = b.with_rewrite(origin, via.clone());
    }
    let aud = cfg.endpoints.first().map(|e| e.base_url.clone());
    let mut cx = Ctx {
        cfg,
        b,
        meta: None,
        jwks: None,
        aud,
    };
    let mut steps: BTreeMap<u8, Step> = BTreeMap::new();

    let s1 = timed(step_discovery(&mut cx)).await;
    let discovery_ok = s1.status != Status::Fail || cx.meta.is_some();
    steps.insert(1, s1);
    if !discovery_ok {
        for n in 2..=10 {
            let (id, title) = STEP_TITLES[(n - 1) as usize];
            steps.insert(n, Step::skipped_because(n, id, title, "discovery"));
        }
        report.steps.extend(steps.into_values());
        return;
    }
    let s2 = timed(step_jwks(&mut cx)).await;
    let jwks_ok = cx.jwks.is_some();
    steps.insert(2, s2);

    let (s3, tokens) = timed_with(step_authorization_code(&mut cx, jwks_ok)).await;
    steps.insert(3, s3);
    let Some(tokens) = tokens else {
        steps.insert(4, timed(step_negative(&mut cx, None)).await);
        for (n, dep) in [(5u8, 3u8), (6, 3), (8, 3), (9, 3), (10, 3)] {
            let (id, title) = STEP_TITLES[(n - 1) as usize];
            steps.insert(
                n,
                Step::skipped_because(n, id, title, STEP_TITLES[(dep - 1) as usize].1),
            );
        }
        steps.insert(7, timed(step_backend(&mut cx)).await);
        report.steps.extend(steps.into_values());
        return;
    };
    steps.insert(4, timed(step_negative(&mut cx, Some(&tokens))).await);
    steps.insert(5, timed(step_fhir(&mut cx, &tokens.access)).await);
    steps.insert(8, timed(step_introspection(&mut cx, &tokens.access)).await);
    let mut s10 = new_step(10);
    let t10 = Instant::now();
    userinfo(&mut cx, &tokens, &mut s10).await;
    let userinfo_ms = t10.elapsed().as_millis() as u64;
    steps.insert(6, timed(step_refresh(&mut cx)).await);
    steps.insert(7, timed(step_backend(&mut cx)).await);
    steps.insert(9, timed(step_revocation(&mut cx, &tokens.access)).await);
    let t10 = Instant::now();
    logout(&mut cx, &tokens, &mut s10).await;
    s10.duration_ms = userinfo_ms + t10.elapsed().as_millis() as u64;
    s10.finish();
    steps.insert(10, s10);
    note_rate_limits(&cx, &mut steps);
    report.steps.extend(steps.into_values());
}

/// Tells the operator when the realm's rate limits slowed the test down.
fn note_rate_limits(cx: &Ctx, steps: &mut BTreeMap<u8, Step>) {
    let waited = cx.b.rate_waited.as_secs();
    if waited == 0 {
        return;
    }
    if let Some(s) = steps.get_mut(&3) {
        s.push(
            Check::pass("authorization-code.rate", "rate-waited", format!("The authorization server's rate limits paused the test for {waited} s (expected when tests run back to back)."))
                .param("secs", waited.to_string()),
        );
        s.finish();
    }
}

async fn timed(f: impl std::future::Future<Output = Step>) -> Step {
    let t = Instant::now();
    let mut s = f.await;
    s.duration_ms = t.elapsed().as_millis() as u64;
    s.finish();
    s
}

async fn timed_with<T>(f: impl std::future::Future<Output = (Step, T)>) -> (Step, T) {
    let t = Instant::now();
    let (mut s, v) = f.await;
    s.duration_ms = t.elapsed().as_millis() as u64;
    s.finish();
    (s, v)
}

// ---------------------------------------------------------------- step 1

async fn fetch_json_doc(cx: &mut Ctx, s: &mut Step, id: &str, url: &str) -> Option<Value> {
    let x = match cx
        .b
        .get_with(url, vec![("accept", "application/json".into())])
        .await
    {
        Ok(x) => x,
        Err(e) => {
            s.push(net_fail(id, url, &e));
            return None;
        }
    };
    clock_check(cx, s, &x);
    if x.status == 404 {
        s.push(
            Check::fail(id, "doc-404", format!("No document at {url} (404)."))
                .param("url", url)
                .url(url)
                .fix("Check that HFS_IDP=on and the realm exists and is enabled; behind a proxy, route /idp/ and /.well-known/ to HFS."),
        );
        return None;
    }
    if x.status != 200 {
        s.push(
            Check::fail(
                id,
                "doc-status",
                format!("{url} answered HTTP {}.", x.status),
            )
            .param("url", url)
            .param("status", x.status.to_string())
            .url(url)
            .fix("Look at the server log for this request."),
        );
        return None;
    }
    match x.json() {
        Some(v) if v.is_object() => Some(v),
        _ => {
            s.push(
                Check::fail(
                    id,
                    "doc-not-json",
                    format!("{url} answered with a web page, not JSON."),
                )
                .param("url", url)
                .url(url)
                .fix("A proxy or another application answers this path. Route it to HFS."),
            );
            None
        }
    }
}

/// Reports a server clock that differs from ours (once per step).
fn clock_check(cx: &Ctx, s: &mut Step, x: &Exchange) {
    if s.checks.iter().any(|c| c.id.ends_with(".clock")) {
        return;
    }
    let Some(server) = x.date() else { return };
    let skew = (server - cx.now()).num_seconds();
    let id = format!("{}.clock", s.id);
    if skew.abs() > cx.cfg.max_clock_skew_secs {
        s.push(
            Check::fail(&id, "clock-skew", format!(
                "The server's clock differs from this checker's by {skew} s (limit {} s): tokens may be refused as not yet valid or expired.",
                cx.cfg.max_clock_skew_secs
            ))
            .param("skew", skew.to_string())
            .param("limit", cx.cfg.max_clock_skew_secs.to_string())
            .url(&x.url)
            .spec("RFC 7519 §4.1.4/§4.1.5 (exp/nbf)")
            .fix("Synchronise the clocks (NTP / chrony) of every HFS instance and of the reverse proxy."),
        );
    }
}

async fn step_discovery(cx: &mut Ctx) -> Step {
    let mut s = new_step(1);
    let issuer = cx.cfg.issuer.clone();
    let oidc_url = format!("{issuer}/.well-known/openid-configuration");
    let meta = fetch_json_doc(cx, &mut s, "discovery.oidc", &oidc_url).await;
    if let Some(m) = &meta {
        let got = m.get("issuer").and_then(Value::as_str).unwrap_or("");
        if got == issuer {
            s.push(
                Check::pass(
                    "discovery.oidc",
                    "discovery-ok",
                    format!("{oidc_url} names issuer {issuer}."),
                )
                .url(&oidc_url),
            );
        } else {
            s.push(
                Check::fail("discovery.oidc", "issuer-mismatch", format!(
                    "The discovery document says its issuer is {got:?}; expected exactly {issuer}. Every token would be refused."
                ))
                .param("expected", &issuer)
                .param("actual", got)
                .url(&oidc_url)
                .spec("OIDC Discovery §4.3 / RFC 8414 §3.3")
                .fix("Set HFS_IDP_PUBLIC_BASE (or HFS_BASE_URL) to the public origin users reach, without a trailing slash."),
            );
        }
        let mut missing: Vec<&str> = [
            "authorization_endpoint",
            "token_endpoint",
            "jwks_uri",
            "response_types_supported",
            "code_challenge_methods_supported",
        ]
        .into_iter()
        .filter(|k| m.get(*k).is_none())
        .collect();
        if cx.cfg.features.openid_connect {
            for k in [
                "subject_types_supported",
                "id_token_signing_alg_values_supported",
            ] {
                if m.get(k).is_none() {
                    missing.push(k);
                }
            }
        }
        if missing.is_empty() {
            s.push(Check::pass(
                "discovery.fields",
                "discovery-fields-ok",
                "Required metadata fields are present.",
            ));
        } else {
            s.push(
                Check::fail(
                    "discovery.fields",
                    "discovery-fields-missing",
                    format!("Discovery lacks: {}.", missing.join(", ")),
                )
                .param("fields", missing.join(", "))
                .url(&oidc_url)
                .spec("RFC 8414 §2 / OIDC Discovery §3")
                .fix("This is a server defect; report it with the JSON report attached."),
            );
        }
        let pkce: Vec<&str> = m
            .get("code_challenge_methods_supported")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if pkce.contains(&"S256") && !pkce.contains(&"plain") {
            s.push(Check::pass(
                "discovery.pkce",
                "pkce-s256-only",
                "PKCE S256 is advertised and plain is not.",
            ));
        } else {
            s.push(
                Check::fail(
                    "discovery.pkce",
                    "pkce-advertised-wrong",
                    format!("PKCE methods advertised: {pkce:?}; expected S256 only."),
                )
                .param("methods", format!("{pkce:?}"))
                .spec("SMART 2.2 “Considerations for PKCE Support”")
                .fix("This is a server defect; report it."),
            );
        }
    }
    // RFC 8414 path insertion.
    if let Ok(iu) = Url::parse(&issuer) {
        let as_url = format!(
            "{}/.well-known/oauth-authorization-server{}",
            iu.origin().ascii_serialization(),
            iu.path().trim_end_matches('/')
        );
        if let Some(m2) = fetch_json_doc(cx, &mut s, "discovery.rfc8414", &as_url).await {
            let got = m2.get("issuer").and_then(Value::as_str).unwrap_or("");
            if got == issuer {
                s.push(
                    Check::pass(
                        "discovery.rfc8414",
                        "discovery-ok",
                        format!("{as_url} names issuer {issuer}."),
                    )
                    .url(&as_url),
                );
            } else {
                s.push(
                    Check::fail("discovery.rfc8414", "issuer-mismatch", format!("RFC 8414 metadata says issuer {got:?}; expected {issuer}."))
                        .param("expected", &issuer)
                        .param("actual", got)
                        .url(&as_url)
                        .spec("RFC 8414 §3.3")
                        .fix("Route /.well-known/oauth-authorization-server/idp/ to HFS and check HFS_IDP_PUBLIC_BASE."),
                );
            }
        }
    }
    cx.meta = meta;
    // SMART configuration of every bound endpoint.
    let eps = if cx.cfg.audience_only {
        vec![]
    } else {
        cx.cfg.endpoints.clone()
    };
    if cx.cfg.audience_only {
        s.push(Check::skipped("discovery.smart", "endpoints-pending", "The endpoints are bound after these checks pass; their SMART configuration is checked then."));
    } else if eps.is_empty() {
        s.push(
            Check::warn(
                "discovery.smart",
                "endpoints-none",
                "No FHIR endpoint is bound, so no SMART configuration to check.",
            )
            .field("endpoints")
            .fix("Bind an endpoint (Settings → Endpoints)."),
        );
    }
    for e in eps {
        let id = format!("discovery.smart.{}", e.name);
        let url = format!(
            "{}/.well-known/smart-configuration",
            e.base_url.trim_end_matches('/')
        );
        let Some(sc) = fetch_json_doc(cx, &mut s, &id, &url).await else {
            if let Some(last) = s.checks.last_mut() {
                last.field = Some(format!("endpoints.{}", e.name));
            }
            continue;
        };
        let got = sc.get("issuer").and_then(Value::as_str).unwrap_or("");
        let authz = sc
            .get("authorization_endpoint")
            .and_then(Value::as_str)
            .unwrap_or("");
        let want_authz = cx.ep("authorization_endpoint", "authorize");
        if got != issuer || authz != want_authz {
            s.push(
                Check::fail(&id, "endpoint-smart-other-issuer", format!(
                    "Endpoint {} advertises issuer {got:?} (authorize {authz:?}), not this server ({issuer}). Apps launched against it sign in elsewhere.",
                    e.name
                ))
                .param("endpoint", &e.name)
                .param("actual", got)
                .param("expected", &issuer)
                .field(format!("endpoints.{}", e.name))
                .url(&url)
                .spec("SMART 2.2 “Conformance” (.well-known/smart-configuration)")
                .fix("Bind the endpoint to this server (Settings → Endpoints → Bind) or set its Security → Authorization server to Internal."),
            );
            continue;
        }
        let caps: Vec<String> = sc
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let f = &cx.cfg.features;
        let mut want = vec![];
        if f.launch_standalone {
            want.push("launch-standalone");
        }
        if f.launch_ehr {
            want.push("launch-ehr");
        }
        if f.client_public {
            want.push("client-public");
        }
        if f.openid_connect {
            want.push("sso-openid-connect");
        }
        let missing: Vec<&str> = want
            .into_iter()
            .filter(|w| !caps.iter().any(|c| c == w))
            .collect();
        if missing.is_empty() {
            s.push(
                Check::pass(
                    &id,
                    "endpoint-smart-ok",
                    format!(
                        "Endpoint {} advertises this server and its capabilities.",
                        e.name
                    ),
                )
                .param("endpoint", &e.name)
                .url(&url),
            );
        } else {
            s.push(
                Check::warn(&id, "endpoint-smart-capabilities", format!(
                    "Endpoint {} does not advertise {} although the realm has them on.", e.name, missing.join(", ")
                ))
                .param("endpoint", &e.name)
                .param("missing", missing.join(", "))
                .field(format!("endpoints.{}", e.name))
                .url(&url)
                .fix("Re-bind the endpoint so its SMART configuration is regenerated from the realm."),
            );
        }
    }
    s
}

// ---------------------------------------------------------------- step 2

async fn step_jwks(cx: &mut Ctx) -> Step {
    let mut s = new_step(2);
    let url = cx.ep("jwks_uri", "jwks");
    let Some(doc) = fetch_json_doc(cx, &mut s, "jwks.fetch", &url).await else {
        return s;
    };
    let keys: Vec<Value> = doc
        .get("keys")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if keys.is_empty() {
        s.push(
            Check::fail(
                "jwks.keys",
                "jwks-empty",
                "The JWKS has no keys: no token can be verified.",
            )
            .url(&url)
            .field("keys")
            .fix("Open Signing keys and generate keys (“Rotate now”)."),
        );
        return s;
    }
    let mut bad_kid = vec![];
    let mut private = vec![];
    for k in &keys {
        let kid = k.get("kid").and_then(Value::as_str).unwrap_or("");
        if thumbprint(k) != kid {
            bad_kid.push(kid.to_string());
        }
        if ["d", "p", "q", "dp", "dq", "qi", "k"]
            .iter()
            .any(|m| k.get(*m).is_some())
        {
            private.push(kid.to_string());
        }
    }
    if private.is_empty() {
        s.push(
            Check::pass(
                "jwks.private",
                "jwks-public-only",
                format!("{} published keys, no private members.", keys.len()),
            )
            .param("count", keys.len().to_string()),
        );
    } else {
        s.push(
            Check::fail("jwks.private", "jwks-private-member", format!("Keys {} publish private key material!", private.join(", ")))
                .param("kids", private.join(", "))
                .field("keys")
                .fix("Emergency-rotate the keys immediately (Signing keys → Emergency rotate) and report the defect."),
        );
    }
    if bad_kid.is_empty() {
        s.push(Check::pass(
            "jwks.kid",
            "jwks-kid-thumbprint",
            "Every kid is the key's RFC 7638 thumbprint.",
        ));
    } else {
        s.push(
            Check::warn(
                "jwks.kid",
                "jwks-kid-not-thumbprint",
                format!(
                    "kid differs from the RFC 7638 thumbprint for {}.",
                    bad_kid.join(", ")
                ),
            )
            .param("kids", bad_kid.join(", "))
            .spec("RFC 7638")
            .field("keys"),
        );
    }
    // An active key per advertised signing algorithm.
    let algs: Vec<String> = cx
        .meta
        .as_ref()
        .and_then(|m| m.get("id_token_signing_alg_values_supported"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    for alg in algs {
        let kty = if alg.starts_with("ES") { "EC" } else { "RSA" };
        let found = keys.iter().any(|k| {
            k.get("alg")
                .and_then(Value::as_str)
                .map(|a| a == alg)
                .unwrap_or_else(|| k.get("kty").and_then(Value::as_str) == Some(kty))
        });
        let id = format!("jwks.alg.{alg}");
        if found {
            s.push(
                Check::pass(&id, "jwks-alg-ok", format!("A {alg} key is published."))
                    .param("alg", &alg),
            );
        } else {
            s.push(
                Check::fail(
                    &id,
                    "jwks-alg-missing",
                    format!("{alg} is advertised but no {alg} key is published."),
                )
                .param("alg", &alg)
                .field("keys")
                .fix("Open Signing keys and rotate the missing algorithm."),
            );
        }
    }
    match cx
        .b
        .get(&url)
        .await
        .ok()
        .and_then(|x| x.header("cache-control").map(String::from))
    {
        Some(cc) if cc.contains("max-age") => s.push(Check::pass("jwks.cache", "jwks-cache-ok", format!("Cache-Control: {cc}")).param("value", cc)),
        _ => s.push(
            Check::warn("jwks.cache", "jwks-cache-missing", "The JWKS has no Cache-Control max-age; relying parties may refetch on every request.")
                .url(&url),
        ),
    }
    cx.jwks = Some(doc);
    s
}

// ---------------------------------------------------------------- step 3

/// Result of driving one authorization request.
enum AuthzOutcome {
    /// Redirected back with these query parameters.
    Redirect(BTreeMap<String, String>),
    /// The server stopped on a page (error page, or a login page that did
    /// not accept the credentials).
    Page {
        status: u16,
        url: String,
        text: String,
        login_rejected: bool,
    },
    Json {
        status: u16,
        error: String,
        description: String,
        url: String,
    },
    Net(String, FetchError),
    TooManyHops(String),
}

struct AuthzRequest<'a> {
    client_id: &'a str,
    redirect_uri: &'a str,
    scope: &'a str,
    state: &'a str,
    nonce: &'a str,
    challenge: &'a str,
    method: &'a str,
    extra: Vec<(&'a str, String)>,
    /// Stop at the first page instead of signing in (app probe).
    interactive: bool,
}

fn authorize_url(cx: &Ctx, r: &AuthzRequest<'_>) -> String {
    let mut u = Url::parse(&cx.ep("authorization_endpoint", "authorize")).expect("authorize url");
    {
        let mut q = u.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", r.client_id)
            .append_pair("redirect_uri", r.redirect_uri)
            .append_pair("scope", r.scope)
            .append_pair("state", r.state)
            .append_pair("nonce", r.nonce)
            .append_pair("code_challenge", r.challenge)
            .append_pair("code_challenge_method", r.method);
        if let Some(aud) = &cx.aud {
            q.append_pair("aud", aud);
        }
        for (k, v) in &r.extra {
            q.append_pair(k, v);
        }
    }
    u.to_string()
}

async fn drive(cx: &mut Ctx, r: &AuthzRequest<'_>) -> AuthzOutcome {
    let mut url = authorize_url(cx, r);
    let mut pending: Option<(String, Vec<(String, String)>)> = None;
    let mut logins = 0;
    for _ in 0..12 {
        let res = match pending.take() {
            Some((action, fields)) => cx.b.post_form(&action, fields, vec![]).await,
            None => cx.b.get(&url).await,
        };
        let x = match res {
            Ok(x) => x,
            Err(e) => return AuthzOutcome::Net(url, e),
        };
        if (300..400).contains(&x.status) {
            let Some(loc) = x.location().and_then(|l| resolve(&x.url, l)) else {
                return AuthzOutcome::Page {
                    status: x.status,
                    url: x.url,
                    text: "redirect without Location".into(),
                    login_rejected: false,
                };
            };
            if loc.starts_with(r.redirect_uri) {
                return AuthzOutcome::Redirect(query_map(&loc));
            }
            url = loc;
            continue;
        }
        if x.is_json() {
            let (error, description) = oauth_error(&x);
            return AuthzOutcome::Json {
                status: x.status,
                error,
                description,
                url: x.url,
            };
        }
        // form_post response mode: an auto-submitting form to redirect_uri.
        let kind = classify_page(&x.body, &x.url);
        if let Some(f) = super::http::forms(&x.body, &x.url)
            .into_iter()
            .find(|f| f.action.starts_with(r.redirect_uri))
        {
            return AuthzOutcome::Redirect(f.fields.into_iter().collect());
        }
        if r.interactive || x.status >= 400 {
            let login_rejected = false;
            return AuthzOutcome::Page {
                status: x.status,
                url: x.url.clone(),
                text: page_text(&x.body, 400),
                login_rejected,
            };
        }
        match kind {
            PageKind::Login(mut f) => {
                logins += 1;
                if logins > 1 {
                    return AuthzOutcome::Page {
                        status: x.status,
                        url: x.url.clone(),
                        text: page_text(&x.body, 400),
                        login_rejected: true,
                    };
                }
                if let Some(u) = f.username_field.clone() {
                    f.set(&u, &cx.cfg.fixture.username);
                }
                if let Some(p) = f.password_field.clone() {
                    f.set(&p, &cx.cfg.fixture.password);
                }
                if let Some((n, v)) = f.approve_button().filter(|(n, _)| !n.is_empty()) {
                    f.set(&n, &v);
                }
                pending = Some((f.action.clone(), f.fields.clone()));
            }
            PageKind::PatientPicker(mut f) => {
                let key = f
                    .choices
                    .keys()
                    .find(|k| k.ends_with("patient"))
                    .cloned()
                    .unwrap_or_else(|| "patient".into());
                let opts = f.choices.get(&key).cloned().unwrap_or_default();
                let pick = cx
                    .cfg
                    .fixture
                    .patient
                    .clone()
                    .filter(|p| opts.iter().any(|o| o == p || o.ends_with(&format!("/{p}"))))
                    .and_then(|p| {
                        opts.iter()
                            .find(|o| **o == p || o.ends_with(&format!("/{p}")))
                            .cloned()
                    })
                    .or_else(|| opts.iter().find(|o| !o.is_empty()).cloned())
                    .unwrap_or_default();
                f.set(&key, &pick);
                if let Some((n, v)) = f.approve_button().filter(|(n, _)| !n.is_empty()) {
                    f.set(&n, &v);
                }
                pending = Some((f.action.clone(), f.fields.clone()));
            }
            PageKind::Consent(mut f) => {
                if let Some((n, v)) = f.approve_button().filter(|(n, _)| !n.is_empty()) {
                    f.set(&n, &v);
                }
                pending = Some((f.action.clone(), f.fields.clone()));
            }
            PageKind::Other => {
                return AuthzOutcome::Page {
                    status: x.status,
                    url: x.url.clone(),
                    text: page_text(&x.body, 400),
                    login_rejected: false,
                };
            }
        }
    }
    AuthzOutcome::TooManyHops(url)
}

/// A fresh PKCE pair, state and nonce.
fn secrets() -> (String, String, String, String) {
    let v = helios_idp::util::random_b64(32);
    let c = pkce::s256_challenge(&v);
    (
        v,
        c,
        helios_idp::util::random_b64(16),
        helios_idp::util::random_b64(16),
    )
}

async fn exchange_code(cx: &mut Ctx, code: &str, verifier: &str) -> Result<Exchange, FetchError> {
    let token_url = cx.ep("token_endpoint", "token");
    let f = &cx.cfg.fixture;
    let pairs = vec![
        ("grant_type".into(), "authorization_code".into()),
        ("code".into(), code.to_string()),
        ("redirect_uri".into(), f.redirect_uri.clone()),
        ("client_id".into(), f.public_client_id.clone()),
        ("code_verifier".into(), verifier.to_string()),
    ];
    cx.b.post_form(&token_url, pairs, vec![]).await
}

/// Runs a full code flow with the fixture client (SSO after the first) and
/// returns the token response, or a check explaining the failure.
async fn code_flow(cx: &mut Ctx, id: &str) -> Result<(Value, String, String), Check> {
    let (verifier, challenge, state, nonce) = secrets();
    let f = cx.cfg.fixture.clone();
    let req = AuthzRequest {
        client_id: &f.public_client_id,
        redirect_uri: &f.redirect_uri,
        scope: &f.scopes,
        state: &state,
        nonce: &nonce,
        challenge: &challenge,
        method: "S256",
        extra: vec![],
        interactive: false,
    };
    let params = match drive(cx, &req).await {
        AuthzOutcome::Redirect(p) => p,
        other => return Err(authz_failure(id, other, &f)),
    };
    if let Some(err) = params.get("error") {
        return Err(Check::fail(
            id,
            "authorize-error",
            format!(
                "The authorization server redirected back with error={err} ({}).",
                params.get("error_description").cloned().unwrap_or_default()
            ),
        )
        .param("error", err)
        .param(
            "detail",
            params.get("error_description").cloned().unwrap_or_default(),
        )
        .fix(hint_for_error(err)));
    }
    let Some(code) = params.get("code") else {
        return Err(Check::fail(
            id,
            "authorize-no-code",
            "Redirected back without a code.",
        ));
    };
    let x = exchange_code(cx, code, &verifier)
        .await
        .map_err(|e| net_fail(id, &cx.ep("token_endpoint", "token"), &e))?;
    if x.status != 200 {
        let (e, d) = oauth_error(&x);
        return Err(Check::fail(
            id,
            "token-refused",
            format!(
                "The token endpoint refused the code: HTTP {} {e} ({d}).",
                x.status
            ),
        )
        .param("status", x.status.to_string())
        .param("error", &e)
        .param("detail", &d)
        .url(&x.url)
        .fix(hint_for_error(&e)));
    }
    let body = x.json().unwrap_or(Value::Null);
    Ok((body, nonce, state_check(&params, &state)))
}

fn state_check(params: &BTreeMap<String, String>, state: &str) -> String {
    match params.get("state") {
        Some(s) if s == state => "ok".into(),
        Some(_) => "mismatch".into(),
        None => "missing".into(),
    }
}

fn rate_limited_check(id: &str, url: &str) -> Check {
    Check::warn(id, "rate-limited", "The authorization server rate-limited the test (HTTP 429) even after waiting; the remaining checks could not run.")
        .url(url)
        .spec("design §10.2 rate limits")
        .fix("Wait a minute and run the test again. Each run makes about ten sign-in requests from the server's own address; raise HFS_IDP_RATE_INTERACTIVE_IP if tests run often.")
}

fn hint_for_error(err: &str) -> &'static str {
    match err {
        "invalid_client" | "unauthorized_client" => {
            "The client is unknown, disabled or not allowed this grant. Check the app under Clients."
        }
        "invalid_scope" => {
            "A requested scope is not allowed for this client. Widen the client's allowed scopes or the realm's capabilities."
        }
        "invalid_request" => {
            "The request was refused; the description says which parameter. A common cause is an aud that is not a bound endpoint."
        }
        "access_denied" => "Consent was denied or the user is not allowed to use this app.",
        "invalid_grant" => {
            "The code or refresh token was refused (expired, reused, or PKCE mismatch)."
        }
        "login_required" | "interaction_required" => "The session is not valid.",
        "server_error" | "temporarily_unavailable" => "The server failed; look at its log.",
        _ => "Look at the server log for this request.",
    }
}

fn authz_failure(id: &str, o: AuthzOutcome, f: &Fixture) -> Check {
    match o {
        AuthzOutcome::Page { status, url, text, login_rejected: true } => Check::fail(id, "login-rejected", format!(
            "The hosted sign-in page did not accept the test user {} (HTTP {status}): {text}", f.username
        ))
        .param("user", &f.username)
        .param("detail", text)
        .url(url)
        .field("users")
        .fix("Check the realm's identity source (Local accounts) and password policy; the test user is created with a policy-compliant password."),
        AuthzOutcome::Page { status, url, text, .. } => Check::fail(id, "authorize-error-page", format!(
            "The authorization request stopped on an error page (HTTP {status}): {text}"
        ))
        .param("status", status.to_string())
        .param("detail", text)
        .url(url)
        .fix("The page text names the problem: typically an unknown or disabled client, an unregistered redirect URI, or an aud that is not a bound endpoint."),
        AuthzOutcome::Json { status: 429, url, .. } => rate_limited_check(id, &url),
        AuthzOutcome::Json { status, error, description, url } => Check::fail(id, "authorize-error", format!(
            "The authorization endpoint answered HTTP {status} {error}: {description}"
        ))
        .param("error", &error)
        .param("detail", description)
        .url(url)
        .fix(hint_for_error(&error)),
        AuthzOutcome::Net(url, e) => net_fail(id, &url, &e),
        AuthzOutcome::TooManyHops(url) => Check::fail(id, "authorize-loop", "The sign-in did not finish after 12 pages (redirect loop?).")
            .url(url)
            .fix("Check the session cookie: behind a proxy the realm must be served over https for its __Host- cookie."),
        AuthzOutcome::Redirect(_) => Check::fail(id, "authorize-unexpected", "Unexpected redirect."),
    }
}

/// Verifies a JWS against the fetched JWKS; returns header + claims.
fn verify_jwt(jwks: Option<&Value>, token: &str) -> Result<(Value, Value), String> {
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(_sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("not a compact JWS".into());
    };
    let dec = |s: &str| -> Result<Value, String> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s)
            .map_err(|e| e.to_string())?;
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())
    };
    let header = dec(h)?;
    let claims = dec(p)?;
    let kid = header.get("kid").and_then(Value::as_str).unwrap_or("");
    let alg_s = header.get("alg").and_then(Value::as_str).unwrap_or("");
    let alg: jsonwebtoken::Algorithm = alg_s
        .parse()
        .map_err(|_| format!("unsupported alg {alg_s:?}"))?;
    if matches!(
        alg,
        jsonwebtoken::Algorithm::HS256
            | jsonwebtoken::Algorithm::HS384
            | jsonwebtoken::Algorithm::HS512
    ) {
        return Err(format!("symmetric alg {alg_s} must not be used"));
    }
    let jwk = jwks
        .and_then(|j| j.get("keys"))
        .and_then(Value::as_array)
        .and_then(|ks| {
            ks.iter()
                .find(|k| k.get("kid").and_then(Value::as_str) == Some(kid))
        })
        .ok_or_else(|| format!("kid {kid:?} is not in the published JWKS"))?;
    let jwk: jsonwebtoken::jwk::Jwk =
        serde_json::from_value(jwk.clone()).map_err(|e| e.to_string())?;
    let key = jsonwebtoken::DecodingKey::from_jwk(&jwk).map_err(|e| e.to_string())?;
    let mut v = jsonwebtoken::Validation::new(alg);
    v.validate_exp = false;
    v.validate_aud = false;
    v.required_spec_claims.clear();
    jsonwebtoken::decode::<Value>(token, &key, &v)
        .map_err(|e| format!("signature does not verify: {e}"))?;
    Ok((header, claims))
}

fn aud_contains(claims: &Value, want: &str) -> bool {
    match claims.get("aud") {
        Some(Value::String(s)) => s.trim_end_matches('/') == want.trim_end_matches('/'),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .any(|s| s.trim_end_matches('/') == want.trim_end_matches('/')),
        _ => false,
    }
}

async fn step_authorization_code(cx: &mut Ctx, jwks_ok: bool) -> (Step, Option<Tokens>) {
    let mut s = new_step(3);
    // A registered app the operator asked about: is it accepted at all?
    if let Some(app) = cx.cfg.app.clone() {
        let (_, challenge, state, nonce) = secrets();
        let req = AuthzRequest {
            client_id: &app.client_id,
            redirect_uri: &app.redirect_uri,
            scope: "openid",
            state: &state,
            nonce: &nonce,
            challenge: &challenge,
            method: "S256",
            extra: vec![],
            interactive: true,
        };
        let id = format!("authorization-code.app.{}", app.client_id);
        let mut probe = Browser::new(cx.cfg.timeout);
        std::mem::swap(&mut probe, &mut cx.b);
        let outcome = drive(cx, &req).await;
        std::mem::swap(&mut probe, &mut cx.b);
        match outcome {
            AuthzOutcome::Page { status, text, .. } if status < 400 => s.push(
                Check::pass(
                    &id,
                    "app-authorize-ok",
                    format!(
                        "App {} reaches the sign-in page with redirect URI {}.",
                        app.client_id, app.redirect_uri
                    ),
                )
                .param("client", &app.client_id)
                .param("uri", &app.redirect_uri)
                .param("detail", text),
            ),
            AuthzOutcome::Redirect(p) if !p.contains_key("error") => s.push(
                Check::pass(
                    &id,
                    "app-authorize-ok",
                    format!("App {} is accepted.", app.client_id),
                )
                .param("client", &app.client_id)
                .param("uri", &app.redirect_uri),
            ),
            other => {
                let mut c = authz_failure(&id, other, &cx.cfg.fixture);
                c.code = "app-authorize-refused".into();
                c.message = format!(
                    "App {} is refused at the authorization endpoint — {}",
                    app.client_id, c.message
                );
                c.field = Some(format!("client.{}.redirectUris", app.client_id));
                c.fix = "See the Configuration step for the exact cause (disabled app, unregistered redirect URI).".into();
                s.push(
                    c.param("client", &app.client_id)
                        .param("uri", &app.redirect_uri),
                );
            }
        }
    }
    let (body, nonce, state) = match code_flow(cx, "authorization-code.flow").await {
        Ok(v) => v,
        Err(c) => {
            s.push(c);
            return (s, None);
        }
    };
    s.push(Check::pass(
        "authorization-code.flow",
        "code-flow-ok",
        "Signed in through the hosted login form and exchanged the code with PKCE S256.",
    ));
    match state.as_str() {
        "ok" => s.push(Check::pass(
            "authorization-code.state",
            "state-echoed",
            "state was echoed.",
        )),
        other => s.push(
            Check::fail(
                "authorization-code.state",
                "state-not-echoed",
                format!("state was {other} in the authorization response."),
            )
            .spec("RFC 6749 §4.1.2")
            .fix("Server defect; report it."),
        ),
    }
    let access = body
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let refresh = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(String::from);
    let id_token = body
        .get("id_token")
        .and_then(Value::as_str)
        .map(String::from);
    if body
        .get("token_type")
        .and_then(Value::as_str)
        .map(|t| t.eq_ignore_ascii_case("bearer"))
        != Some(true)
        || access.is_empty()
        || body.get("expires_in").is_none()
    {
        s.push(
            Check::fail(
                "authorization-code.response",
                "token-response-shape",
                "The token response lacks access_token, token_type=Bearer or expires_in.",
            )
            .spec("SMART “Obtain access token”")
            .fix("Server defect; report it."),
        );
    }
    let mut tokens = Tokens {
        access: access.clone(),
        id_token: id_token.clone(),
        id_sub: None,
    };
    let jwks = if jwks_ok { cx.jwks.clone() } else { None };
    // Access token.
    match verify_jwt(jwks.as_ref(), &access) {
        Err(e) => s.push(
            Check::fail("authorization-code.access-token", "access-token-signature", format!("The access token does not verify against the JWKS: {e}"))
                .param("detail", e)
                .field("keys")
                .fix("Usually a key was rotated without being published first. Open Signing keys; if a key shows as active but not published, rotate again."),
        ),
        Ok((h, c)) => {
            let mut problems = vec![];
            if h.get("typ").and_then(Value::as_str).map(|t| t.eq_ignore_ascii_case("at+jwt")) != Some(true) {
                problems.push("typ is not at+jwt".to_string());
            }
            if c.get("iss").and_then(Value::as_str) != Some(cx.cfg.issuer.as_str()) {
                problems.push(format!("iss is {:?}", c.get("iss")));
            }
            if c.get("client_id").and_then(Value::as_str) != Some(cx.cfg.fixture.public_client_id.as_str()) {
                problems.push("client_id does not name the client".into());
            }
            if c.get("scope").and_then(Value::as_str).unwrap_or("").is_empty() {
                problems.push("scope is empty".into());
            }
            if c.get("gid").is_none() {
                problems.push("gid (grant id) is missing".into());
            }
            if let Some(aud) = &cx.aud
                && !aud_contains(&c, aud)
            {
                problems.push(format!("aud does not contain the endpoint base {aud}"));
            }
            if let Some(p) = &cx.cfg.fixture.patient
                && cx.cfg.fixture.scopes.contains("launch/patient")
                && body.get("patient").and_then(Value::as_str) != Some(p.as_str())
            {
                problems.push(format!("patient context is {:?}, expected {p}", body.get("patient")));
            }
            token_clock(cx, &mut s, "authorization-code.clock", &c);
            if problems.is_empty() {
                s.push(Check::pass("authorization-code.access-token", "access-token-ok", "The access token verifies and carries iss, aud, scope, client_id and gid."));
            } else {
                s.push(
                    Check::fail("authorization-code.access-token", "access-token-claims", format!("Access token claims are wrong: {}.", problems.join("; ")))
                        .param("detail", problems.join("; "))
                        .spec("RFC 9068 §2.2; SMART “Scopes and Launch Context”")
                        .fix("Check the endpoint binding (aud) and the realm's public base URL (iss)."),
                );
            }
        }
    }
    // id_token.
    if cx.cfg.features.openid_connect && cx.cfg.fixture.scopes.split(' ').any(|t| t == "openid") {
        match id_token.as_deref().map(|t| verify_jwt(jwks.as_ref(), t)) {
            None => s.push(
                Check::fail(
                    "authorization-code.id-token",
                    "id-token-missing",
                    "scope openid was granted but no id_token was returned.",
                )
                .spec("OIDC Core §3.1.3.3")
                .fix("Server defect; report it."),
            ),
            Some(Err(e)) => s.push(
                Check::fail(
                    "authorization-code.id-token",
                    "id-token-signature",
                    format!("The id_token does not verify: {e}"),
                )
                .param("detail", e)
                .field("keys"),
            ),
            Some(Ok((h, c))) => {
                let mut problems = vec![];
                if c.get("aud").and_then(Value::as_str)
                    != Some(cx.cfg.fixture.public_client_id.as_str())
                    && !aud_contains(&c, &cx.cfg.fixture.public_client_id)
                {
                    problems.push("aud is not the client id".to_string());
                }
                if c.get("nonce").and_then(Value::as_str) != Some(nonce.as_str()) {
                    problems.push("nonce was not echoed".into());
                }
                if c.get("iss").and_then(Value::as_str) != Some(cx.cfg.issuer.as_str()) {
                    problems.push("iss mismatch".into());
                }
                if let (Some(ah), Some(alg)) = (
                    c.get("at_hash").and_then(Value::as_str),
                    h.get("alg")
                        .and_then(Value::as_str)
                        .and_then(|a| a.parse::<SigningAlg>().ok()),
                ) && ah != helios_idp::token::at_hash(&access, alg)
                {
                    problems.push("at_hash does not match the access token".into());
                }
                tokens.id_sub = c.get("sub").and_then(Value::as_str).map(String::from);
                if problems.is_empty() {
                    s.push(Check::pass(
                        "authorization-code.id-token",
                        "id-token-ok",
                        "The id_token verifies (signature, iss, aud, nonce, at_hash).",
                    ));
                } else {
                    s.push(
                        Check::fail(
                            "authorization-code.id-token",
                            "id-token-claims",
                            format!("id_token claims are wrong: {}.", problems.join("; ")),
                        )
                        .param("detail", problems.join("; "))
                        .spec("OIDC Core §3.1.3.7"),
                    );
                }
            }
        }
    }
    if cx.cfg.features.offline_access
        && cx.cfg.fixture.scopes.contains("offline_access")
        && refresh.is_none()
    {
        s.push(
            Check::warn(
                "authorization-code.refresh-token",
                "refresh-token-missing",
                "offline_access was requested but no refresh token was issued.",
            )
            .field("realm.features.offlineAccess"),
        );
    }
    (s, Some(tokens))
}

fn token_clock(cx: &Ctx, s: &mut Step, id: &str, claims: &Value) {
    let now = cx.now().timestamp();
    let iat = claims.get("iat").and_then(Value::as_i64);
    let exp = claims.get("exp").and_then(Value::as_i64);
    if let Some(iat) = iat {
        let skew = iat - now;
        if skew.abs() > cx.cfg.max_clock_skew_secs {
            s.push(
                Check::fail(id, "clock-skew-token", format!(
                    "The token was issued at {iat}, {skew} s away from this checker's clock (limit {} s): resource servers will refuse it as not yet valid or expired.",
                    cx.cfg.max_clock_skew_secs
                ))
                .param("skew", skew.to_string())
                .param("limit", cx.cfg.max_clock_skew_secs.to_string())
                .spec("RFC 7519 §4.1.6 (iat)")
                .fix("Synchronise the clocks (NTP / chrony) of every HFS instance."),
            );
            return;
        }
    }
    if let Some(exp) = exp
        && exp <= now
    {
        s.push(
            Check::fail(
                id,
                "token-already-expired",
                "The token is already expired when received.",
            )
            .fix("Check token lifetimes (Settings → Lifetimes) and the server clock."),
        );
    }
}

// ---------------------------------------------------------------- step 4

async fn step_negative(cx: &mut Ctx, tokens: Option<&Tokens>) -> Step {
    let mut s = new_step(4);
    let f = cx.cfg.fixture.clone();
    // Unregistered redirect_uri → error page, never a redirect.
    {
        let (_, challenge, state, nonce) = secrets();
        let evil = format!("{}-not-registered", f.redirect_uri.trim_end_matches('/'));
        let req = AuthzRequest {
            client_id: &f.public_client_id,
            redirect_uri: &evil,
            scope: &f.scopes,
            state: &state,
            nonce: &nonce,
            challenge: &challenge,
            method: "S256",
            extra: vec![],
            interactive: true,
        };
        match drive(cx, &req).await {
            AuthzOutcome::Redirect(_) => s.push(
                Check::fail(
                    "negative.redirect",
                    "open-redirect",
                    "An unregistered redirect_uri was redirected to: this is an open redirector.",
                )
                .spec("RFC 9700 §4.1.3; RFC 6749 §4.1.2.1")
                .fix("Server defect — do not use this realm until fixed; report it."),
            ),
            AuthzOutcome::Net(u, e) => s.push(net_fail("negative.redirect", &u, &e)),
            _ => s.push(Check::pass(
                "negative.redirect",
                "redirect-refused",
                "An unregistered redirect_uri shows an error page and is never redirected to.",
            )),
        }
    }
    // PKCE plain refused.
    {
        let (v, _, state, nonce) = secrets();
        let req = AuthzRequest {
            client_id: &f.public_client_id,
            redirect_uri: &f.redirect_uri,
            scope: &f.scopes,
            state: &state,
            nonce: &nonce,
            challenge: &v,
            method: "plain",
            extra: vec![],
            interactive: false,
        };
        match drive(cx, &req).await {
            AuthzOutcome::Redirect(p) if p.contains_key("code") => s.push(
                Check::fail(
                    "negative.pkce-plain",
                    "pkce-plain-accepted",
                    "code_challenge_method=plain was accepted.",
                )
                .spec("SMART 2.2 PKCE; OAuth 2.1 §4.1.1")
                .fix("Server defect; report it."),
            ),
            AuthzOutcome::Net(u, e) => s.push(net_fail("negative.pkce-plain", &u, &e)),
            _ => s.push(Check::pass(
                "negative.pkce-plain",
                "pkce-plain-refused",
                "PKCE plain is refused.",
            )),
        }
    }
    if tokens.is_none() {
        s.push(
            Check::skipped(
                "negative.code",
                "skipped-dependency",
                "Code checks skipped: sign-in failed.",
            )
            .param("step", "authorization-code"),
        );
        return s;
    }
    // Wrong code_verifier → invalid_grant.
    match fresh_code(cx).await {
        Ok((code, _verifier)) => {
            let (wrong, _, _, _) = secrets();
            match exchange_code(cx, &code, &wrong).await {
                Ok(x) if x.status == 400 && oauth_error(&x).0 == "invalid_grant" => {
                    s.push(Check::pass(
                        "negative.verifier",
                        "verifier-refused",
                        "A wrong code_verifier gets invalid_grant.",
                    ))
                }
                Ok(x) => s.push(
                    Check::fail(
                        "negative.verifier",
                        "verifier-accepted",
                        format!(
                            "A wrong code_verifier got HTTP {} {}.",
                            x.status,
                            oauth_error(&x).0
                        ),
                    )
                    .param("status", x.status.to_string())
                    .spec("RFC 7636 §4.6")
                    .fix("Server defect; report it."),
                ),
                Err(e) => s.push(net_fail(
                    "negative.verifier",
                    &cx.ep("token_endpoint", "token"),
                    &e,
                )),
            }
        }
        Err(c) => s.push(c),
    }
    // Code replay → refused, and the grant of the first use is revoked.
    match fresh_code(cx).await {
        Ok((code, verifier)) => {
            let first = exchange_code(cx, &code, &verifier).await;
            let first_at = first
                .ok()
                .filter(|x| x.status == 200)
                .and_then(|x| x.json())
                .and_then(|j| {
                    j.get("access_token")
                        .and_then(Value::as_str)
                        .map(String::from)
                });
            match exchange_code(cx, &code, &verifier).await {
                Ok(x) if x.status == 400 && oauth_error(&x).0 == "invalid_grant" => {
                    s.push(Check::pass(
                        "negative.replay",
                        "code-replay-refused",
                        "A replayed code gets invalid_grant.",
                    ));
                    if let Some(at) = first_at
                        && cx.cfg.features.introspection
                    {
                        match introspect(cx, &at).await {
                            Some(false) => s.push(Check::pass(
                                "negative.replay-revokes",
                                "code-replay-revokes",
                                "The replay revoked the tokens of the first exchange.",
                            )),
                            Some(true) => s.push(
                                Check::warn(
                                    "negative.replay-revokes",
                                    "code-replay-not-revoking",
                                    "The replay did not revoke the tokens issued for the code.",
                                )
                                .spec("RFC 6749 §4.1.2 (SHOULD revoke)"),
                            ),
                            None => {}
                        }
                    }
                }
                Ok(x) => s.push(
                    Check::fail(
                        "negative.replay",
                        "code-replay-accepted",
                        format!("A replayed code got HTTP {}.", x.status),
                    )
                    .spec("RFC 6749 §4.1.2")
                    .fix("Server defect; report it."),
                ),
                Err(e) => s.push(net_fail(
                    "negative.replay",
                    &cx.ep("token_endpoint", "token"),
                    &e,
                )),
            }
        }
        Err(c) => s.push(c),
    }
    s
}

/// A new code through the existing SSO session (login/consent if shown).
async fn fresh_code(cx: &mut Ctx) -> Result<(String, String), Check> {
    let (verifier, challenge, state, nonce) = secrets();
    let f = cx.cfg.fixture.clone();
    let req = AuthzRequest {
        client_id: &f.public_client_id,
        redirect_uri: &f.redirect_uri,
        scope: &f.scopes,
        state: &state,
        nonce: &nonce,
        challenge: &challenge,
        method: "S256",
        extra: vec![],
        interactive: false,
    };
    match drive(cx, &req).await {
        AuthzOutcome::Redirect(p) => {
            p.get("code")
                .cloned()
                .map(|c| (c, verifier))
                .ok_or_else(|| {
                    Check::fail(
                        "negative.code",
                        "authorize-error",
                        format!("No code: {:?}", p.get("error")),
                    )
                })
        }
        other => Err(authz_failure("negative.code", other, &f)),
    }
}

// ---------------------------------------------------------------- step 5

async fn step_fhir(cx: &mut Ctx, access: &str) -> Step {
    let mut s = new_step(5);
    if cx.cfg.audience_only {
        s.push(Check::skipped(
            "fhir.pending",
            "endpoints-pending",
            "The endpoints are bound after these checks pass; the FHIR call runs then.",
        ));
        return s;
    }
    let eps = cx.cfg.endpoints.clone();
    if eps.is_empty() {
        s.push(
            Check::skipped("fhir.none", "endpoints-none", "No bound endpoint to call.")
                .field("endpoints"),
        );
        return s;
    }
    for e in eps {
        let base = e.base_url.trim_end_matches('/').to_string();
        let target_aud = cx.aud.clone().unwrap_or_default();
        if base != target_aud.trim_end_matches('/') {
            // Tokens are audience-bound: only the first endpoint is called
            // with this token; the others were checked by discovery.
            s.push(Check::skipped(&format!("fhir.{}", e.name), "fhir-other-audience", format!("Endpoint {} is checked by discovery only (the test token's aud is {target_aud}).", e.name)).param("endpoint", &e.name));
            continue;
        }
        let url = match &cx.cfg.fixture.patient {
            Some(p) => format!("{base}/Patient/{p}"),
            None => format!("{base}/Patient?_count=1"),
        };
        let id = format!("fhir.{}.read", e.name);
        match cx
            .b
            .get_with(
                &url,
                vec![
                    ("authorization", format!("Bearer {access}")),
                    ("accept", "application/fhir+json".into()),
                ],
            )
            .await
        {
            Ok(x) if x.status == 200 => s.push(
                Check::pass(
                    &id,
                    "fhir-read-ok",
                    format!("GET {url} with the token → 200."),
                )
                .param("endpoint", &e.name)
                .url(&url),
            ),
            Ok(x) if x.status == 401 || x.status == 403 => {
                let www = x.header("www-authenticate").unwrap_or("").to_string();
                s.push(
                    Check::fail(&id, "fhir-token-refused", format!(
                        "Endpoint {} refused the token: HTTP {} {}", e.name, x.status, if www.is_empty() { page_text(&x.body, 200) } else { www.clone() }
                    ))
                    .param("endpoint", &e.name)
                    .param("status", x.status.to_string())
                    .param("detail", www)
                    .field(format!("endpoints.{}", e.name))
                    .url(&url)
                    .fix("The endpoint does not trust this server, or expects another audience. Bind the endpoint (Settings → Endpoints) and check its Security page."),
                );
            }
            Ok(x) if x.status == 404 && cx.cfg.fixture.patient.is_some() => s.push(
                Check::warn(
                    &id,
                    "fhir-patient-missing",
                    format!(
                        "The token was accepted but Patient/{} does not exist on {}.",
                        cx.cfg.fixture.patient.clone().unwrap_or_default(),
                        e.name
                    ),
                )
                .param("endpoint", &e.name)
                .url(&url),
            ),
            Ok(x) => s.push(
                Check::fail(
                    &id,
                    "fhir-status",
                    format!("GET {url} → HTTP {}.", x.status),
                )
                .param("endpoint", &e.name)
                .param("status", x.status.to_string())
                .url(&url),
            ),
            Err(err) => s.push(net_fail(&id, &url, &err)),
        }
        // Anonymous requests must be refused when the endpoint is in Full mode.
        if e.auth_mode == "full" {
            let id = format!("fhir.{}.anonymous", e.name);
            let anon = format!("{base}/Patient?_count=1");
            match cx.b.get_with(&anon, vec![("accept", "application/fhir+json".into())]).await {
                Ok(x) if x.status == 401 => s.push(Check::pass(&id, "fhir-anonymous-refused", "Without a token the endpoint answers 401.").param("endpoint", &e.name)),
                Ok(x) => s.push(
                    Check::fail(&id, "fhir-anonymous-open", format!("Without a token the endpoint answered HTTP {} although its auth mode is Full.", x.status))
                        .param("endpoint", &e.name)
                        .field(format!("endpoints.{}", e.name))
                        .url(&anon),
                ),
                Err(err) => s.push(net_fail(&id, &anon, &err)),
            }
        }
        // A write with a read-only token must be refused (the body is
        // invalid on purpose, so nothing is stored even if it is not).
        let id = format!("fhir.{}.scope", e.name);
        let wurl = format!("{base}/Patient");
        match cx
            .b
            .post_raw(
                &wurl,
                "application/fhir+json",
                r#"{"resourceType":"Patient","active":"diagnostic-not-a-boolean"}"#.into(),
                vec![("authorization", format!("Bearer {access}"))],
            )
            .await
        {
            Ok(x) if x.status == 403 => s.push(
                Check::pass(
                    &id,
                    "fhir-scope-enforced",
                    "A write with a read-only token is refused (403).",
                )
                .param("endpoint", &e.name),
            ),
            Ok(x) if x.status == 401 => s.push(
                Check::pass(
                    &id,
                    "fhir-scope-enforced",
                    "A write with a read-only token is refused (401).",
                )
                .param("endpoint", &e.name),
            ),
            Ok(x) => s.push(
                Check::warn(
                    &id,
                    "fhir-scope-not-enforced",
                    format!(
                        "A write with a read-only token got HTTP {} instead of 403.",
                        x.status
                    ),
                )
                .param("endpoint", &e.name)
                .param("status", x.status.to_string())
                .field(format!("endpoints.{}", e.name))
                .fix("Check the endpoint's auth mode (Write-protect/Full) and scope policy."),
            ),
            Err(err) => s.push(net_fail(&id, &wurl, &err)),
        }
    }
    s
}

// ---------------------------------------------------------------- step 6

async fn refresh(cx: &mut Ctx, rt: &str) -> Result<Exchange, FetchError> {
    let url = cx.ep("token_endpoint", "token");
    let pairs = vec![
        ("grant_type".into(), "refresh_token".into()),
        ("refresh_token".into(), rt.to_string()),
        ("client_id".into(), cx.cfg.fixture.public_client_id.clone()),
    ];
    cx.b.post_form(&url, pairs, vec![]).await
}

async fn step_refresh(cx: &mut Ctx) -> Step {
    let mut s = new_step(6);
    if !cx.cfg.features.offline_access || !cx.cfg.fixture.scopes.contains("offline_access") {
        s.push(
            Check::skipped(
                "refresh.off",
                "refresh-off",
                "Refresh tokens are off for this realm.",
            )
            .field("realm.features.offlineAccess"),
        );
        return s;
    }
    // A separate grant, so reuse detection does not kill the tokens the
    // other steps use.
    let rt1 = match code_flow(cx, "refresh.flow").await {
        Ok((body, _, _)) => body
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(String::from),
        Err(c) => {
            s.push(c);
            return s;
        }
    };
    let Some(rt1) = rt1 else {
        s.push(
            Check::fail(
                "refresh.issued",
                "refresh-token-missing",
                "No refresh token was issued for offline_access.",
            )
            .field("realm.features.offlineAccess"),
        );
        return s;
    };
    let url = cx.ep("token_endpoint", "token");
    let rt2 = match refresh(cx, &rt1).await {
        Ok(x) if x.status == 200 => {
            let j = x.json().unwrap_or(Value::Null);
            let rt2 = j
                .get("refresh_token")
                .and_then(Value::as_str)
                .map(String::from);
            match &rt2 {
                Some(r) if *r != rt1 => s.push(Check::pass(
                    "refresh.rotate",
                    "refresh-rotated",
                    "Refreshing returned a new access token and a rotated refresh token.",
                )),
                _ => s.push(
                    Check::fail(
                        "refresh.rotate",
                        "refresh-not-rotated",
                        "The refresh token was not rotated.",
                    )
                    .spec("RFC 9700 §4.14.2")
                    .fix("Server defect; report it."),
                ),
            }
            rt2
        }
        Ok(x) => {
            let (e, d) = oauth_error(&x);
            s.push(
                Check::fail(
                    "refresh.rotate",
                    "refresh-refused",
                    format!("Refreshing failed: HTTP {} {e} ({d}).", x.status),
                )
                .param("error", e)
                .param("detail", d)
                .url(&url),
            );
            return s;
        }
        Err(e) => {
            s.push(net_fail("refresh.rotate", &url, &e));
            return s;
        }
    };
    match refresh(cx, &rt1).await {
        Ok(x) if x.status == 400 && oauth_error(&x).0 == "invalid_grant" => {
            s.push(Check::pass(
                "refresh.reuse",
                "refresh-reuse-refused",
                "Reusing the old refresh token gets invalid_grant.",
            ));
            if let Some(rt2) = rt2 {
                match refresh(cx, &rt2).await {
                    Ok(x) if x.status == 400 => s.push(Check::pass(
                        "refresh.family",
                        "refresh-family-revoked",
                        "The reuse revoked the whole token family.",
                    )),
                    Ok(x) => s.push(
                        Check::fail(
                            "refresh.family",
                            "refresh-family-alive",
                            format!(
                                "After reuse, the newest refresh token still works (HTTP {}).",
                                x.status
                            ),
                        )
                        .spec("RFC 9700 §4.14.2")
                        .fix("Server defect; report it."),
                    ),
                    Err(e) => s.push(net_fail("refresh.family", &url, &e)),
                }
            }
        }
        Ok(x) => s.push(
            Check::fail(
                "refresh.reuse",
                "refresh-reuse-accepted",
                format!("Reusing the old refresh token got HTTP {}.", x.status),
            )
            .spec("RFC 9700 §4.14.2")
            .fix("Server defect; report it."),
        ),
        Err(e) => s.push(net_fail("refresh.reuse", &url, &e)),
    }
    s
}

// ---------------------------------------------------------------- step 7

fn assertion(cx: &Ctx, client: &str, key: &SigningKey, exp_secs: i64) -> String {
    let now = cx.now().timestamp();
    let claims = json!({
        "iss": client,
        "sub": client,
        "aud": cx.ep("token_endpoint", "token"),
        "exp": now + exp_secs,
        "iat": now,
        "jti": helios_idp::util::random_b64(16),
    });
    key.sign_jws("JWT", &claims).unwrap_or_default()
}

async fn client_credentials(
    cx: &mut Ctx,
    scope: &str,
    assertion: &str,
) -> Result<Exchange, FetchError> {
    let url = cx.ep("token_endpoint", "token");
    let pairs = vec![
        ("grant_type".into(), "client_credentials".into()),
        ("scope".into(), scope.to_string()),
        (
            "client_assertion_type".into(),
            "urn:ietf:params:oauth:client-assertion-type:jwt-bearer".into(),
        ),
        ("client_assertion".into(), assertion.to_string()),
    ];
    cx.b.post_form(&url, pairs, vec![]).await
}

async fn step_backend(cx: &mut Ctx) -> Step {
    let mut s = new_step(7);
    let (Some(client), Some(key)) = (
        cx.cfg.fixture.backend_client_id.clone(),
        cx.cfg.fixture.backend_key.clone(),
    ) else {
        s.push(
            Check::skipped(
                "backend.off",
                "backend-off",
                "Backend Services is off for this realm.",
            )
            .field("realm.features.backendServices"),
        );
        return s;
    };
    let url = cx.ep("token_endpoint", "token");
    let scope = cx.cfg.fixture.backend_scope.clone();
    let a = assertion(cx, &client, &key, 240);
    match client_credentials(cx, &scope, &a).await {
        Ok(x) if x.status == 200 => {
            let j = x.json().unwrap_or(Value::Null);
            let mut problems = vec![];
            if j.get("access_token").and_then(Value::as_str).is_none() {
                problems.push("no access_token".to_string());
            }
            if j.get("refresh_token").is_some() {
                problems.push("a refresh token was issued (SHOULD NOT)".into());
            }
            if j.get("expires_in")
                .and_then(Value::as_i64)
                .is_some_and(|e| e > 300)
            {
                problems.push("expires_in exceeds 300 s".into());
            }
            if let Some(at) = j.get("access_token").and_then(Value::as_str)
                && let Ok((_, c)) = verify_jwt(cx.jwks.as_ref(), at)
            {
                token_clock(cx, &mut s, "backend.clock", &c);
            }
            if problems.is_empty() {
                s.push(Check::pass(
                    "backend.token",
                    "backend-ok",
                    "client_credentials with private_key_jwt issued a short-lived token.",
                ));
            } else {
                s.push(
                    Check::warn(
                        "backend.token",
                        "backend-token-shape",
                        format!("Backend token: {}.", problems.join("; ")),
                    )
                    .param("detail", problems.join("; "))
                    .spec("SMART Backend Services")
                    .field("realm.lifetimes.backendAccessToken"),
                );
            }
        }
        Ok(x) => {
            let (e, d) = oauth_error(&x);
            s.push(
                Check::fail(
                    "backend.token",
                    "backend-refused",
                    format!(
                        "client_credentials was refused: HTTP {} {e} ({d}).",
                        x.status
                    ),
                )
                .param("error", &e)
                .param("detail", d)
                .url(&url)
                .fix(hint_for_error(&e)),
            );
            return s;
        }
        Err(e) => {
            s.push(net_fail("backend.token", &url, &e));
            return s;
        }
    }
    match client_credentials(cx, &scope, &a).await {
        Ok(x) if x.status == 401 || (x.status == 400 && oauth_error(&x).0 == "invalid_client") => s
            .push(Check::pass(
                "backend.replay",
                "assertion-replay-refused",
                "A replayed client assertion (same jti) is refused.",
            )),
        Ok(x) => s.push(
            Check::fail(
                "backend.replay",
                "assertion-replay-accepted",
                format!("A replayed client assertion got HTTP {}.", x.status),
            )
            .spec("SMART Client Authentication: Asymmetric (jti)")
            .fix("Server defect; report it."),
        ),
        Err(e) => s.push(net_fail("backend.replay", &url, &e)),
    }
    let long = assertion(cx, &client, &key, 3600);
    match client_credentials(cx, &scope, &long).await {
        Ok(x) if x.status == 401 || x.status == 400 => s.push(Check::pass(
            "backend.exp",
            "assertion-exp-refused",
            "An assertion valid for more than 5 minutes is refused.",
        )),
        Ok(x) => s.push(
            Check::fail(
                "backend.exp",
                "assertion-exp-accepted",
                format!(
                    "An assertion with exp one hour ahead got HTTP {}.",
                    x.status
                ),
            )
            .spec("SMART Client Authentication: Asymmetric (exp ≤ 5 min)")
            .fix("Server defect; report it."),
        ),
        Err(e) => s.push(net_fail("backend.exp", &url, &e)),
    }
    s
}

// ---------------------------------------------------------------- step 8

/// `Some(active)`; `None` when introspection could not be called.
async fn introspect(cx: &mut Ctx, token: &str) -> Option<bool> {
    let x = introspect_raw(cx, token).await.ok()?;
    if x.status != 200 {
        return None;
    }
    x.json()?.get("active").and_then(Value::as_bool)
}

async fn introspect_raw(cx: &mut Ctx, token: &str) -> Result<Exchange, FetchError> {
    let url = cx.ep("introspection_endpoint", "introspect");
    let mut pairs = vec![("token".to_string(), token.to_string())];
    if let (Some(client), Some(key)) = (
        cx.cfg.fixture.backend_client_id.clone(),
        cx.cfg.fixture.backend_key.clone(),
    ) {
        let a = assertion(cx, &client, &key, 120);
        pairs.push(("client_id".into(), client));
        pairs.push((
            "client_assertion_type".into(),
            "urn:ietf:params:oauth:client-assertion-type:jwt-bearer".into(),
        ));
        pairs.push(("client_assertion".into(), a));
    } else {
        pairs.push(("client_id".into(), cx.cfg.fixture.public_client_id.clone()));
    }
    cx.b.post_form(&url, pairs, vec![]).await
}

async fn step_introspection(cx: &mut Ctx, access: &str) -> Step {
    let mut s = new_step(8);
    if !cx.cfg.features.introspection {
        s.push(
            Check::skipped(
                "introspection.off",
                "introspection-off",
                "Introspection is off for this realm.",
            )
            .field("realm.features.introspection"),
        );
        return s;
    }
    let url = cx.ep("introspection_endpoint", "introspect");
    match introspect_raw(cx, access).await {
        Ok(x) if x.status == 200 => {
            let j = x.json().unwrap_or(Value::Null);
            if j.get("active").and_then(Value::as_bool) == Some(true) {
                let mut missing = vec![];
                for k in ["scope", "client_id", "exp", "sub"] {
                    if j.get(k).is_none() {
                        missing.push(k);
                    }
                }
                if cx.cfg.fixture.patient.is_some()
                    && cx.cfg.fixture.scopes.contains("launch/patient")
                    && j.get("patient").is_none()
                {
                    missing.push("patient");
                }
                if missing.is_empty() {
                    s.push(Check::pass(
                        "introspection.active",
                        "introspection-active",
                        "A live token introspects as active with its scope and context.",
                    ));
                } else {
                    s.push(
                        Check::warn(
                            "introspection.active",
                            "introspection-fields",
                            format!("Active, but missing {}.", missing.join(", ")),
                        )
                        .param("fields", missing.join(", "))
                        .spec("RFC 7662 §2.2; SMART “Token Introspection”"),
                    );
                }
            } else {
                s.push(
                    Check::fail(
                        "introspection.active",
                        "introspection-inactive",
                        "A live token introspects as inactive.",
                    )
                    .url(&url)
                    .fix("Server defect, or the grant was revoked by an earlier step; report it."),
                );
            }
        }
        Ok(x) => {
            let (e, d) = oauth_error(&x);
            s.push(
                Check::fail(
                    "introspection.active",
                    "introspection-refused",
                    format!("Introspection answered HTTP {} {e} ({d}).", x.status),
                )
                .param("error", e)
                .param("detail", d)
                .url(&url),
            );
            return s;
        }
        Err(e) => {
            s.push(net_fail("introspection.active", &url, &e));
            return s;
        }
    }
    match introspect(cx, "hmac_garbage-token-from-diagnostic").await {
        Some(false) => s.push(Check::pass(
            "introspection.garbage",
            "introspection-garbage-inactive",
            "A garbage token introspects as active:false.",
        )),
        Some(true) => s.push(
            Check::fail(
                "introspection.garbage",
                "introspection-garbage-active",
                "A garbage token introspects as active!",
            )
            .fix("Server defect; report it."),
        ),
        None => s.push(
            Check::fail(
                "introspection.garbage",
                "introspection-garbage-error",
                "Introspecting a garbage token did not answer 200 {\"active\":false}.",
            )
            .spec("RFC 7662 §2.2"),
        ),
    }
    s
}

// ---------------------------------------------------------------- step 9

async fn step_revocation(cx: &mut Ctx, access: &str) -> Step {
    let mut s = new_step(9);
    if !cx.cfg.features.revocation {
        s.push(
            Check::skipped(
                "revocation.off",
                "revocation-off",
                "Revocation is off for this realm.",
            )
            .field("realm.features.revocation"),
        );
        return s;
    }
    let url = cx.ep("revocation_endpoint", "revoke");
    let pairs = vec![
        ("token".to_string(), access.to_string()),
        (
            "client_id".to_string(),
            cx.cfg.fixture.public_client_id.clone(),
        ),
    ];
    match cx.b.post_form(&url, pairs, vec![]).await {
        Ok(x) if x.status == 200 => s.push(Check::pass(
            "revocation.revoke",
            "revoke-ok",
            "Revocation answered 200.",
        )),
        Ok(x) => {
            s.push(
                Check::fail(
                    "revocation.revoke",
                    "revoke-status",
                    format!("Revocation answered HTTP {}.", x.status),
                )
                .url(&url)
                .spec("RFC 7009 §2.2"),
            );
            return s;
        }
        Err(e) => {
            s.push(net_fail("revocation.revoke", &url, &e));
            return s;
        }
    }
    if cx.cfg.features.introspection {
        match introspect(cx, access).await {
            Some(false) => s.push(Check::pass(
                "revocation.introspect",
                "revoked-inactive",
                "The revoked token introspects as inactive.",
            )),
            Some(true) => s.push(
                Check::fail(
                    "revocation.introspect",
                    "revoked-still-active",
                    "The revoked token still introspects as active.",
                )
                .fix("Server defect; report it."),
            ),
            None => {}
        }
    }
    let Some(e) = cx
        .cfg
        .endpoints
        .first()
        .cloned()
        .filter(|_| !cx.cfg.audience_only)
    else {
        return s;
    };
    let base = e.base_url.trim_end_matches('/');
    let furl = match &cx.cfg.fixture.patient {
        Some(p) => format!("{base}/Patient/{p}"),
        None => format!("{base}/Patient?_count=1"),
    };
    let deadline = Instant::now() + cx.cfg.revocation_wait;
    let mut last: u16;
    loop {
        match cx
            .b
            .get_with(&furl, vec![("authorization", format!("Bearer {access}"))])
            .await
        {
            Ok(x) => last = x.status,
            Err(err) => {
                s.push(net_fail("revocation.fhir", &furl, &err));
                return s;
            }
        }
        if last == 401 || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if last == 401 {
        s.push(
            Check::pass(
                "revocation.fhir",
                "revoked-fhir-refused",
                format!("Endpoint {} refuses the revoked token.", e.name),
            )
            .param("endpoint", &e.name),
        );
    } else {
        s.push(
            Check::warn("revocation.fhir", "revoked-fhir-accepted", format!(
                "Endpoint {} still accepted the revoked token after {} s (HTTP {last}).", e.name, cx.cfg.revocation_wait.as_secs()
            ))
            .param("endpoint", &e.name)
            .param("secs", cx.cfg.revocation_wait.as_secs().to_string())
            .field(format!("endpoints.{}", e.name))
            .fix("With the compatibility binding (External issuer = this server), revocation reaches the endpoint only at token expiry. Switch the endpoint's provider to Internal for immediate revocation."),
        );
    }
    s
}

// ---------------------------------------------------------------- step 10

async fn userinfo(cx: &mut Ctx, t: &Tokens, s: &mut Step) {
    if !cx.cfg.features.openid_connect {
        s.push(
            Check::skipped(
                "userinfo-logout.userinfo",
                "oidc-off",
                "OpenID Connect is off.",
            )
            .field("realm.features.openidConnect"),
        );
        return;
    }
    let url = cx.ep("userinfo_endpoint", "userinfo");
    match cx
        .b
        .get_with(
            &url,
            vec![("authorization", format!("Bearer {}", t.access))],
        )
        .await
    {
        Ok(x) if x.status == 200 => {
            let sub = x
                .json()
                .and_then(|j| j.get("sub").and_then(Value::as_str).map(String::from));
            if sub.is_some() && sub == t.id_sub {
                s.push(Check::pass(
                    "userinfo-logout.userinfo",
                    "userinfo-ok",
                    "userinfo sub equals the id_token sub.",
                ));
            } else {
                s.push(
                    Check::fail(
                        "userinfo-logout.userinfo",
                        "userinfo-sub-mismatch",
                        format!(
                            "userinfo sub {sub:?} differs from the id_token sub {:?}.",
                            t.id_sub
                        ),
                    )
                    .spec("OIDC Core §5.3.2"),
                );
            }
        }
        Ok(x) => s.push(
            Check::fail(
                "userinfo-logout.userinfo",
                "userinfo-status",
                format!("userinfo answered HTTP {}.", x.status),
            )
            .url(&url),
        ),
        Err(e) => s.push(net_fail("userinfo-logout.userinfo", &url, &e)),
    }
}

async fn logout(cx: &mut Ctx, t: &Tokens, s: &mut Step) {
    if !cx.cfg.features.openid_connect {
        return;
    }
    let Some(end) = cx
        .meta
        .as_ref()
        .and_then(|m| m.get("end_session_endpoint"))
        .and_then(Value::as_str)
        .map(String::from)
    else {
        s.push(
            Check::warn(
                "userinfo-logout.logout",
                "logout-not-advertised",
                "No end_session_endpoint is advertised.",
            )
            .spec("OIDC RP-Initiated Logout 1.0"),
        );
        return;
    };
    let mut u = Url::parse(&end).expect("end_session url");
    if let Some(h) = &t.id_token {
        u.query_pairs_mut().append_pair("id_token_hint", h);
    }
    u.query_pairs_mut()
        .append_pair("client_id", &cx.cfg.fixture.public_client_id);
    let mut url = u.to_string();
    // Follow a confirmation form if the server asks.
    for _ in 0..4 {
        let x = match cx.b.get(&url).await {
            Ok(x) => x,
            Err(e) => {
                s.push(net_fail("userinfo-logout.logout", &url, &e));
                return;
            }
        };
        if (300..400).contains(&x.status) {
            match x.location().and_then(|l| resolve(&x.url, l)) {
                Some(l) if l.starts_with(&cx.cfg.issuer) => {
                    url = l;
                    continue;
                }
                _ => break,
            }
        }
        if let Some(f) = super::http::forms(&x.body, &x.url)
            .into_iter()
            .find(|f| f.method == "post")
        {
            let mut fields = f.fields.clone();
            if let Some((n, v)) = f.approve_button().filter(|(n, _)| !n.is_empty()) {
                fields.push((n, v));
            }
            let _ = cx.b.post_form(&f.action, fields, vec![]).await;
        }
        break;
    }
    // The session must be gone: prompt=none now answers login_required.
    let (_, challenge, state, nonce) = secrets();
    let f = cx.cfg.fixture.clone();
    let req = AuthzRequest {
        client_id: &f.public_client_id,
        redirect_uri: &f.redirect_uri,
        scope: &f.scopes,
        state: &state,
        nonce: &nonce,
        challenge: &challenge,
        method: "S256",
        extra: vec![("prompt", "none".into())],
        interactive: true,
    };
    match drive(cx, &req).await {
        AuthzOutcome::Redirect(p)
            if p.get("error").map(String::as_str) == Some("login_required") =>
        {
            s.push(Check::pass(
                "userinfo-logout.logout",
                "logout-ok",
                "After RP-initiated logout, prompt=none answers login_required.",
            ))
        }
        AuthzOutcome::Redirect(p) if p.contains_key("code") => s.push(
            Check::fail(
                "userinfo-logout.logout",
                "logout-session-alive",
                "After logout the session still signs in silently (prompt=none returned a code).",
            )
            .spec("OIDC RP-Initiated Logout 1.0 §2")
            .fix("Server defect; report it."),
        ),
        other => {
            let detail = match other {
                AuthzOutcome::Redirect(p) => format!("{p:?}"),
                AuthzOutcome::Page { status, text, .. } => format!("HTTP {status}: {text}"),
                AuthzOutcome::Json { error, .. } => error,
                AuthzOutcome::Net(_, e) => e.to_string(),
                AuthzOutcome::TooManyHops(_) => "redirect loop".into(),
            };
            s.push(
                Check::warn(
                    "userinfo-logout.logout",
                    "logout-prompt-none-unexpected",
                    format!("After logout, prompt=none answered unexpectedly: {detail}"),
                )
                .param("detail", detail)
                .spec("OIDC Core §3.1.2.6"),
            );
        }
    }
}
