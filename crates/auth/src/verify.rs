//! "Configure, then verify": step-by-step diagnostics of configuration the
//! console asks an administrator for (UX audit UX-02, UX-07, UX-08, UX-10).
//!
//! Every check returns a [`VerifyReport`]: an ordered list of
//! [`VerifyStep`]s, each with a stable `id`, a [`VerifyStatus`], the form
//! `field` it checks, a message `code` (the console looks up the Fluent
//! messages `verify-{code}` and `verify-{code}-fix`, see
//! `locales/*/verify.ftl`), string `params` for the message, the URL
//! contacted (credentials redacted), the time taken and an English
//! `detail` sentence for API clients and logs. A step whose prerequisite
//! failed is [`VerifyStatus::Skipped`] with code `skipped-after-failure` and
//! `params.after` = the failed step's id.
//!
//! Three verifiers, all fetching through an [`OutboundGuard`]:
//!
//! * [`EndpointAuthProbe::verify`] — an endpoint's SMART / JWT settings
//!   (kind `endpoint-auth`): input, outbound policy, connection, discovery
//!   document, issuer, JWKS URL, JWKS, algorithms, SMART capabilities,
//!   audience and, optionally, a token.
//! * [`verify_terminology_server`] — a terminology server (kind
//!   `terminology`): `/metadata`, token, `$lookup`, `$expand`, latency.
//! * [`verify_fhir_peer`] — the external side of a sync job (kinds
//!   `sync-source` / `sync-target`): `/metadata` and FHIR version, OAuth
//!   token request, per-type counts, write and incremental support.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use base64::Engine;
use helios_endpoints::model::{AuthMode as EndpointAuthMode, EndpointAuth};
use helios_endpoints::outbound::OutboundDenied;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{AuthConfig, redact_url};
use crate::discovery::{
    EndpointAuthProbe, FetchError, OidcMetadata, OutboundGuard, RawResponse, VERIFIER_ALGORITHMS,
    openid_configuration_url, send_raw,
};

// ------------------------------------------------------------------ contract

/// Outcome of one step.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VerifyStatus {
    /// Checked and fine.
    Pass,
    /// Works, but something deserves attention.
    Warn,
    /// Broken: requests relying on this would fail.
    Fail,
    /// Not checked (a prerequisite failed, or nothing to check).
    Skipped,
}

/// One step of a [`VerifyReport`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VerifyStep {
    /// Stable step id (`input`, `outbound`, `connect`, `discovery`, …).
    pub id: String,
    /// Outcome.
    pub status: VerifyStatus,
    /// The console form field the step checks ("Go to field").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// Message code: Fluent `verify-{code}` and `verify-{code}-fix`.
    pub code: String,
    /// Fluent arguments.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    /// URL contacted, credentials redacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Time taken.
    pub elapsed_ms: u64,
    /// English sentence (API clients, logs).
    pub detail: String,
}

/// What a verification answers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VerifyReport {
    /// `endpoint-auth`, `terminology`, `sync-source` or `sync-target`.
    pub kind: String,
    /// No step failed (warnings and skipped steps allowed).
    pub ok: bool,
    /// The steps, in order.
    pub steps: Vec<VerifyStep>,
    /// Field → value the console may offer to fill in (`jwksUrl` from the
    /// discovery document, `issuer` as the IdP states it).
    #[serde(default)]
    pub suggestions: BTreeMap<String, String>,
    /// When the check ran (RFC 3339, UTC).
    pub checked_at: String,
}

impl VerifyReport {
    /// Failed steps.
    pub fn failures(&self) -> usize {
        self.count(VerifyStatus::Fail)
    }

    /// Steps with a warning.
    pub fn warnings(&self) -> usize {
        self.count(VerifyStatus::Warn)
    }

    /// The first failed step (fix it first; later ones may be consequences).
    pub fn first_failure(&self) -> Option<&VerifyStep> {
        self.steps.iter().find(|s| s.status == VerifyStatus::Fail)
    }

    /// Appends a step (and keeps `ok` in step).
    pub fn push(&mut self, step: VerifyStep) {
        if step.status == VerifyStatus::Fail {
            self.ok = false;
        }
        self.steps.push(step);
    }

    fn count(&self, status: VerifyStatus) -> usize {
        self.steps.iter().filter(|s| s.status == status).count()
    }
}

impl VerifyStep {
    /// A step.
    pub fn new(id: &str, status: VerifyStatus, code: &str, detail: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status,
            field: None,
            code: code.into(),
            params: BTreeMap::new(),
            url: None,
            elapsed_ms: 0,
            detail: detail.into(),
        }
    }

    /// A step skipped because `after` failed.
    pub fn skipped_after(id: &str, after: &str) -> Self {
        Self::new(
            id,
            VerifyStatus::Skipped,
            "skipped-after-failure",
            format!("Not checked because step {after} failed."),
        )
        .param("after", after)
    }

    /// Sets the form field.
    pub fn field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    /// Adds a message argument.
    pub fn param(mut self, key: &str, value: impl Into<String>) -> Self {
        self.params.insert(key.into(), value.into());
        self
    }

    /// Sets the URL contacted (redacted here).
    pub fn url(mut self, url: &str) -> Self {
        self.url = Some(display_url(url));
        self
    }

    /// Sets the elapsed time since `started`.
    pub fn since(mut self, started: Instant) -> Self {
        self.elapsed_ms = started.elapsed().as_millis() as u64;
        self
    }

    /// Sets the elapsed time.
    pub fn ms(mut self, ms: u64) -> Self {
        self.elapsed_ms = ms;
        self
    }
}

use VerifyStatus::{Fail, Pass, Skipped, Warn};

struct Builder {
    report: VerifyReport,
}

impl Builder {
    fn new(kind: &str) -> Self {
        Self {
            report: VerifyReport {
                kind: kind.into(),
                ok: true,
                steps: Vec::new(),
                suggestions: BTreeMap::new(),
                checked_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            },
        }
    }

    fn push(&mut self, step: VerifyStep) -> VerifyStatus {
        let status = step.status;
        self.report.push(step);
        status
    }

    fn suggest(&mut self, field: &str, value: &str) {
        self.report
            .suggestions
            .insert(field.to_string(), value.to_string());
    }

    fn finish(self) -> VerifyReport {
        self.report
    }
}

/// A URL for display: user-info removed; the query is kept only when it
/// holds nothing that looks like a credential (we build our own queries).
fn display_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut u) => {
            let query = u.query().map(str::to_string);
            let safe = query.as_deref().is_none_or(|q| {
                let q = q.to_ascii_lowercase();
                !["token", "secret", "password", "key", "assertion"]
                    .iter()
                    .any(|w| q.contains(w))
            });
            let _ = u.set_username("");
            let _ = u.set_password(None);
            if safe {
                u.set_fragment(None);
                u.to_string()
            } else {
                redact_url(u.as_str())
            }
        }
        Err(_) => redact_url(url),
    }
}

fn host_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| url.to_string())
}

/// Parses an absolute http(s) URL with a host.
fn parse_http_url(raw: &str) -> Option<reqwest::Url> {
    let u = reqwest::Url::parse(raw.trim()).ok()?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none_or(str::is_empty) {
        return None;
    }
    Some(u)
}

fn trimmed(v: &Option<String>) -> Option<String> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn join_url(base: &str, rest: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), rest)
}

// ------------------------------------------------------------ shared steps

/// `input`: every `(field, url)` must be an absolute http(s) URL. Under
/// the trusted guard a plain-http URL is a warning (no policy will refuse
/// it, but it travels unencrypted).
fn input_step(guard: &OutboundGuard, urls: &[(&str, &str)], started: Instant) -> VerifyStep {
    for (field, raw) in urls {
        if raw.trim().is_empty() {
            return VerifyStep::new(
                "input",
                Fail,
                "input-required",
                format!("{field} is required."),
            )
            .field(*field)
            .param("field", *field)
            .since(started);
        }
        if parse_http_url(raw).is_none() {
            return VerifyStep::new(
                "input",
                Fail,
                "input-not-url",
                format!(
                    "{field} is not an absolute http(s) URL (for example https://idp.example.org/realms/fhir)."
                ),
            )
            .field(*field)
            .param("field", *field)
            .param("value", raw.trim())
            .since(started);
        }
    }
    if matches!(guard, OutboundGuard::Trusted)
        && let Some((field, _)) = urls
            .iter()
            .find(|(_, u)| u.trim().to_ascii_lowercase().starts_with("http://"))
    {
        return VerifyStep::new(
            "input",
            Warn,
            "input-plain-http",
            format!("{field} uses plain http: requests and keys travel unencrypted."),
        )
        .field(*field)
        .param("field", *field)
        .since(started);
    }
    VerifyStep::new("input", Pass, "input-ok", "Every URL is well formed.").since(started)
}

/// `outbound`: the outbound (SSRF) policy admits every URL, and the host
/// names resolve to admitted addresses.
async fn outbound_step(guard: &OutboundGuard, urls: &[(&str, &str)]) -> VerifyStep {
    let started = Instant::now();
    let policy = match guard {
        OutboundGuard::Trusted => {
            return VerifyStep::new(
                "outbound",
                Pass,
                "outbound-trusted",
                "Configured by the operator: the outbound policy does not apply.",
            )
            .since(started);
        }
        OutboundGuard::Policy(p) => p,
    };
    let mut hosts = BTreeSet::new();
    for (field, raw) in urls {
        let raw = raw.trim();
        let host = host_of(raw);
        if let Err(denied) = policy.check_url(raw) {
            let (code, detail) = match &denied {
                OutboundDenied::Scheme(s) if s == "http" => (
                    "outbound-plain-http",
                    format!(
                        "{host} is reached over plain http, which the outbound policy only allows for hosts in HFS_OUTBOUND_ALLOW."
                    ),
                ),
                OutboundDenied::UserInfo => (
                    "outbound-credentials",
                    "Credentials inside the URL are not allowed.".to_string(),
                ),
                other => (
                    "outbound-blocked",
                    format!("The outbound policy refuses {host}: {other}."),
                ),
            };
            return VerifyStep::new("outbound", Fail, code, detail)
                .field(*field)
                .param("host", host)
                .param("reason", denied.to_string())
                .url(raw)
                .since(started);
        }
        // Resolve now so a refused address is reported as the policy's,
        // not as a connection failure; a name that does not resolve is the
        // `connect` step's to report.
        if let Ok(u) = reqwest::Url::parse(raw)
            && let Some(name) = u.host_str()
        {
            let bare = name.trim_start_matches('[').trim_end_matches(']');
            if bare.parse::<std::net::IpAddr>().is_err() {
                let port = u.port_or_known_default().unwrap_or(443);
                let lookup = tokio::time::timeout(
                    Duration::from_secs(5),
                    tokio::net::lookup_host((bare.to_string(), port)),
                )
                .await;
                if let Ok(Ok(addrs)) = lookup {
                    let addrs: Vec<_> = addrs.collect();
                    let refused: Vec<OutboundDenied> = addrs
                        .iter()
                        .filter_map(|a| policy.check_ip(bare, a.ip()).err())
                        .collect();
                    if !addrs.is_empty() && refused.len() == addrs.len() {
                        return VerifyStep::new(
                            "outbound",
                            Fail,
                            "outbound-blocked",
                            format!("The outbound policy refuses {host}: {}.", refused[0]),
                        )
                        .field(*field)
                        .param("host", host)
                        .param("reason", refused[0].to_string())
                        .url(raw)
                        .since(started);
                    }
                }
            }
        }
        hosts.insert(host);
    }
    let hosts = hosts.into_iter().collect::<Vec<_>>().join(", ");
    VerifyStep::new(
        "outbound",
        Pass,
        "outbound-ok",
        format!("The outbound policy allows {hosts}."),
    )
    .param("hosts", hosts)
    .since(started)
}

/// `connect` step for a transport error.
fn connect_failure(field: &str, url: &str, e: &FetchError, started: Instant) -> VerifyStep {
    let host = host_of(url);
    let (code, detail) = match e {
        FetchError::Dns { reason, .. } => (
            "connect-dns",
            format!("The host name {host} does not resolve ({reason})."),
        ),
        FetchError::Refused { .. } => (
            "connect-refused",
            format!("{host} refused the connection: nothing listens at that address and port."),
        ),
        FetchError::Timeout { .. } => {
            ("connect-timeout", format!("{host} did not answer in time."))
        }
        FetchError::Tls { reason, .. } => (
            "connect-tls",
            format!("The TLS handshake with {host} failed ({reason})."),
        ),
        FetchError::Blocked { reason, .. } => (
            "outbound-blocked",
            format!("The outbound policy refuses {host}: {reason}."),
        ),
        other => (
            "connect-failed",
            format!("Could not reach {host}: {other}."),
        ),
    };
    let mut step = VerifyStep::new("connect", Fail, code, detail)
        .field(field)
        .param("host", host)
        .url(url)
        .since(started);
    if let FetchError::Dns { reason, .. }
    | FetchError::Tls { reason, .. }
    | FetchError::Connect { reason, .. }
    | FetchError::Blocked { reason, .. } = e
    {
        step = step.param("reason", reason.clone());
    }
    step
}

fn connect_ok(url: &str, raw: &RawResponse) -> VerifyStep {
    let host = host_of(url);
    VerifyStep::new(
        "connect",
        Pass,
        "connect-ok",
        format!("Connected to {host}."),
    )
    .param("host", host)
    .url(url)
    .ms(raw.headers_ms)
}

/// Credentials of a request.
#[derive(Clone)]
enum Credential {
    None,
    Bearer(String),
    Basic(String, String),
}

async fn get_with(
    url: &str,
    guard: &OutboundGuard,
    timeout: Duration,
    credential: &Credential,
) -> Result<RawResponse, FetchError> {
    guard.check_url(url).map_err(|reason| FetchError::Blocked {
        url: redact_url(url),
        reason,
    })?;
    let mut request = guard.client(timeout).get(url).header(
        reqwest::header::ACCEPT,
        "application/fhir+json, application/json",
    );
    request = match credential {
        Credential::None => request,
        Credential::Bearer(t) => request.bearer_auth(t),
        Credential::Basic(u, p) => request.basic_auth(u, Some(p)),
    };
    send_raw(url, request).await
}

fn transport_reason(e: &FetchError) -> &'static str {
    match e {
        FetchError::Dns { .. } => "dns",
        FetchError::Refused { .. } => "refused",
        FetchError::Timeout { .. } => "timeout",
        FetchError::Tls { .. } => "tls",
        FetchError::Blocked { .. } => "blocked",
        _ => "other",
    }
}

// ------------------------------------------------------------ endpoint auth

/// One signing key of a JWKS, as the verifier would see it.
#[derive(Debug, Clone, Default)]
struct KeyInfo {
    kid: Option<String>,
    kty: String,
    alg: Option<String>,
    crv: Option<String>,
    usable: bool,
}

impl KeyInfo {
    /// Algorithms this key can verify.
    fn algorithms(&self) -> Vec<String> {
        if let Some(a) = &self.alg {
            return vec![a.clone()];
        }
        match self.kty.as_str() {
            "RSA" => vec!["RS256".into(), "RS384".into(), "RS512".into()],
            "EC" => match self.crv.as_deref() {
                Some("P-384") => vec!["ES384".into()],
                _ => vec!["ES256".into()],
            },
            _ => Vec::new(),
        }
    }
}

fn parse_jwks(value: &Value) -> Option<Vec<KeyInfo>> {
    let keys = value.get("keys")?.as_array()?;
    Some(
        keys.iter()
            .map(|k| {
                let s = |name: &str| k.get(name).and_then(Value::as_str).map(str::to_string);
                let kty = s("kty").unwrap_or_default();
                let key_use = s("use");
                let kid = s("kid");
                let material = match kty.as_str() {
                    "RSA" => k.get("n").is_some() && k.get("e").is_some(),
                    "EC" => k.get("x").is_some() && k.get("y").is_some(),
                    _ => false,
                };
                KeyInfo {
                    usable: kid.is_some()
                        && material
                        && key_use.as_deref().is_none_or(|u| u == "sig"),
                    kid,
                    kty,
                    alg: s("alg"),
                    crv: s("crv"),
                }
            })
            .collect(),
    )
}

/// Claims of a JWT, decoded without verification (for messages only).
fn peek_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn claim_text(claims: Option<&Value>, name: &str) -> String {
    match claims.and_then(|c| c.get(name)) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        Some(Value::Number(n)) => n.to_string(),
        _ => "(none)".into(),
    }
}

const DEFAULT_ALGORITHMS: &[&str] = &["RS256", "RS384", "ES256", "ES384"];

impl EndpointAuthProbe {
    /// Verifies an endpoint's auth settings step by step (kind
    /// `endpoint-auth`): `input`, `outbound`, `connect`, `discovery`,
    /// `issuer`, `jwks-url`, `jwks`, `algorithms`, `smart`, `audience` and,
    /// with `token` (a bare JWT or `Bearer …`), `token`. With auth off: one
    /// `mode` step. The discovery document is fetched once. The token is
    /// never logged or echoed.
    ///
    /// The console may pass the server-level (inherited) auth to verify
    /// the effective configuration; nothing here depends on where `auth`
    /// came from.
    pub async fn verify(&self, auth: &EndpointAuth, token: Option<&str>) -> VerifyReport {
        let mut r = Builder::new("endpoint-auth");
        let token = token
            .map(str::trim)
            .map(|t| t.strip_prefix("Bearer ").unwrap_or(t).trim())
            .filter(|t| !t.is_empty());
        if auth.mode == EndpointAuthMode::Off {
            r.push(
                VerifyStep::new(
                    "mode",
                    Pass,
                    "mode-off",
                    "Authentication is off: no credential is needed and tokens are ignored.",
                )
                .field("mode"),
            );
            if token.is_some() {
                r.push(
                    VerifyStep::new(
                        "token",
                        Skipped,
                        "token-mode-off",
                        "Authentication is off, so the token was not checked.",
                    )
                    .field("token"),
                );
            }
            return r.finish();
        }
        let guard = &self.guard;
        let issuer = trimmed(&auth.issuer).unwrap_or_default();
        let jwks_cfg = trimmed(&auth.jwks_url);

        // 1. input
        let mut urls: Vec<(&str, &str)> = vec![("issuer", issuer.as_str())];
        if let Some(j) = &jwks_cfg {
            urls.push(("jwksUrl", j.as_str()));
        }
        let mut net_gate: Option<&str> = None;
        if r.push(input_step(guard, &urls, Instant::now())) == Fail {
            net_gate = Some("input");
        }

        // 2. outbound
        match net_gate {
            Some(g) => {
                r.push(VerifyStep::skipped_after("outbound", g));
            }
            None => {
                if r.push(outbound_step(guard, &urls).await) == Fail {
                    net_gate = Some("outbound");
                }
            }
        }

        // 3. connect + 4. discovery (one request)
        let discovery_url = openid_configuration_url(&issuer);
        let mut meta: Option<OidcMetadata> = None;
        let mut disc_gate = net_gate;
        match net_gate {
            Some(g) => {
                r.push(VerifyStep::skipped_after("connect", g));
                r.push(VerifyStep::skipped_after("discovery", g));
            }
            None => {
                let started = Instant::now();
                match get_with(&discovery_url, guard, self.timeout, &Credential::None).await {
                    Err(e) => {
                        r.push(connect_failure("issuer", &discovery_url, &e, started));
                        r.push(VerifyStep::skipped_after("discovery", "connect"));
                        net_gate = Some("connect");
                        disc_gate = net_gate;
                    }
                    Ok(raw) => {
                        r.push(connect_ok(&discovery_url, &raw));
                        let step = match raw.json::<OidcMetadata>(&discovery_url) {
                            Ok(m) => {
                                meta = Some(m);
                                VerifyStep::new(
                                    "discovery",
                                    Pass,
                                    "discovery-ok",
                                    "The discovery document was found.",
                                )
                            }
                            Err(FetchError::Status { status: 404, .. }) => VerifyStep::new(
                                "discovery",
                                Fail,
                                "discovery-404",
                                format!(
                                    "No discovery document at {}. Check the issuer path.",
                                    display_url(&discovery_url)
                                ),
                            ),
                            Err(FetchError::Status { status, .. }) => VerifyStep::new(
                                "discovery",
                                Fail,
                                "discovery-status",
                                format!("The discovery document URL answered HTTP {status}."),
                            )
                            .param("status", status.to_string()),
                            Err(FetchError::Html { .. }) => VerifyStep::new(
                                "discovery",
                                Fail,
                                "discovery-html",
                                "The URL answered with a web page, not a JSON discovery document.",
                            ),
                            Err(e) => VerifyStep::new(
                                "discovery",
                                Fail,
                                "discovery-not-json",
                                format!("The answer is not a JSON discovery document: {e}."),
                            ),
                        };
                        let step = step
                            .field("issuer")
                            .param("url", display_url(&discovery_url))
                            .url(&discovery_url)
                            .ms(raw.total_ms);
                        if r.push(step) == Fail {
                            disc_gate = Some("discovery");
                        }
                    }
                }
            }
        }

        // 5. issuer
        match (&meta, disc_gate) {
            (Some(m), _) => {
                let started = Instant::now();
                let step = match m.issuer.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    None => VerifyStep::new(
                        "issuer",
                        Fail,
                        "issuer-not-stated",
                        "The discovery document does not state an issuer.",
                    ),
                    Some(stated)
                        if stated.trim_end_matches('/') != issuer.trim_end_matches('/') =>
                    {
                        r.suggest("issuer", stated);
                        VerifyStep::new(
                            "issuer",
                            Fail,
                            "issuer-mismatch",
                            format!(
                                "The IdP says its issuer is {stated}; you entered {issuer}. Every token would be refused."
                            ),
                        )
                        .param("stated", stated)
                        .param("entered", issuer.as_str())
                    }
                    Some(stated) => VerifyStep::new(
                        "issuer",
                        Pass,
                        "issuer-ok",
                        format!("The IdP states the same issuer ({stated})."),
                    )
                    .param("stated", stated),
                };
                r.push(step.field("issuer").since(started));
            }
            (None, gate) => {
                r.push(VerifyStep::skipped_after(
                    "issuer",
                    gate.unwrap_or("discovery"),
                ));
            }
        }

        // 6. jwks-url
        let discovered_jwks = meta.as_ref().and_then(|m| trimmed(&m.jwks_uri));
        let mut jwks_gate: Option<&str> = None;
        let effective_jwks: Option<String> = match (&meta, &jwks_cfg, &discovered_jwks) {
            (Some(_), None, Some(d)) => {
                r.suggest("jwksUrl", d);
                r.push(
                    VerifyStep::new(
                        "jwks-url",
                        Pass,
                        "jwks-url-from-discovery",
                        format!("JWKS URL is empty; the issuer publishes {d}."),
                    )
                    .field("jwksUrl")
                    .param("discovered", d.as_str()),
                );
                Some(d.clone())
            }
            (Some(_), None, None) => {
                r.push(
                    VerifyStep::new(
                        "jwks-url",
                        Fail,
                        "jwks-url-missing",
                        "JWKS URL is empty and the discovery document does not publish jwks_uri.",
                    )
                    .field("jwksUrl"),
                );
                jwks_gate = Some("jwks-url");
                None
            }
            (Some(_), Some(c), Some(d)) if c.trim_end_matches('/') != d.trim_end_matches('/') => {
                r.suggest("jwksUrl", d);
                r.push(
                    VerifyStep::new(
                        "jwks-url",
                        Warn,
                        "jwks-url-differs",
                        format!("JWKS URL is {c}, but the issuer publishes {d}."),
                    )
                    .field("jwksUrl")
                    .param("configured", c.as_str())
                    .param("discovered", d.as_str()),
                );
                Some(c.clone())
            }
            (Some(_), Some(c), _) => {
                r.push(
                    VerifyStep::new(
                        "jwks-url",
                        Pass,
                        "jwks-url-ok",
                        "JWKS URL matches the one the issuer publishes.",
                    )
                    .field("jwksUrl")
                    .param("configured", c.as_str()),
                );
                Some(c.clone())
            }
            (None, cfg, _) => {
                r.push(VerifyStep::skipped_after(
                    "jwks-url",
                    disc_gate.unwrap_or("discovery"),
                ));
                if cfg.is_none() {
                    jwks_gate = Some(disc_gate.unwrap_or("discovery"));
                }
                cfg.clone()
            }
        };

        // 7. jwks
        let mut keys: Vec<KeyInfo> = Vec::new();
        match (net_gate.or(jwks_gate), &effective_jwks) {
            (Some(g), _) => {
                r.push(VerifyStep::skipped_after("jwks", g));
                jwks_gate = Some(g);
            }
            (None, None) => {
                r.push(VerifyStep::skipped_after("jwks", "jwks-url"));
                jwks_gate = Some("jwks-url");
            }
            (None, Some(url)) => {
                let started = Instant::now();
                let step = match get_with(url, guard, self.timeout, &Credential::None).await {
                    Err(e) => VerifyStep::new(
                        "jwks",
                        Fail,
                        "jwks-unreachable",
                        format!("The JWKS could not be fetched: {e}."),
                    )
                    .param("reason", transport_reason(&e)),
                    Ok(raw) => match raw.json::<Value>(url) {
                        Err(FetchError::Status { status: 404, .. }) => VerifyStep::new(
                            "jwks",
                            Fail,
                            "jwks-404",
                            format!("No JWKS at {}.", display_url(url)),
                        ),
                        Err(FetchError::Status { status, .. }) => VerifyStep::new(
                            "jwks",
                            Fail,
                            "jwks-status",
                            format!("The JWKS URL answered HTTP {status}."),
                        )
                        .param("status", status.to_string()),
                        Err(_) => VerifyStep::new(
                            "jwks",
                            Fail,
                            "jwks-not-json",
                            "The JWKS URL did not answer with a JSON key set.",
                        ),
                        Ok(v) => match parse_jwks(&v) {
                            None => VerifyStep::new(
                                "jwks",
                                Fail,
                                "jwks-not-json",
                                "The JWKS URL answered JSON without a \"keys\" list.",
                            ),
                            Some(all) => {
                                let total = all.len();
                                keys = all.into_iter().filter(|k| k.usable).collect();
                                if keys.is_empty() && total > 0 {
                                    VerifyStep::new(
                                        "jwks",
                                        Fail,
                                        "jwks-no-usable-key",
                                        format!(
                                            "The JWKS holds {total} key(s) but none is a signing key with a kid the verifier can use."
                                        ),
                                    )
                                    .param("count", total.to_string())
                                } else if keys.is_empty() {
                                    VerifyStep::new(
                                        "jwks",
                                        Fail,
                                        "jwks-empty",
                                        "The JWKS holds no signing key.",
                                    )
                                } else {
                                    self.jwks_ok_or_foreign(&keys, url, discovered_jwks.as_deref())
                                        .await
                                }
                            }
                        },
                    },
                };
                let step = step
                    .field("jwksUrl")
                    .param("url", display_url(url))
                    .url(url)
                    .since(started);
                if r.push(step) == Fail {
                    jwks_gate = Some("jwks");
                }
            }
        }

        // 8. algorithms
        match jwks_gate {
            Some(g) => {
                r.push(VerifyStep::skipped_after("algorithms", g));
            }
            None => {
                r.push_alg(auth, &keys);
            }
        }

        // 9. smart
        match &meta {
            None => {
                r.push(VerifyStep::skipped_after(
                    "smart",
                    disc_gate.unwrap_or("discovery"),
                ));
            }
            Some(m) => {
                r.push(smart_step(m));
            }
        }

        // 10. audience
        match trimmed(&auth.audience) {
            None => r.push(
                VerifyStep::new(
                    "audience",
                    Warn,
                    "audience-missing",
                    "No audience: any token of this issuer is accepted, including tokens minted for other services.",
                )
                .field("audience"),
            ),
            Some(a) => r.push(
                VerifyStep::new(
                    "audience",
                    Pass,
                    "audience-ok",
                    format!("Tokens must be issued for {a}."),
                )
                .field("audience")
                .param("audience", a),
            ),
        };

        // 11. token
        if let Some(jwt) = token {
            match (jwks_gate, &effective_jwks) {
                (None, Some(jwks_url)) => {
                    let step = self.token_step(auth, jwks_url, jwt).await;
                    r.push(step);
                }
                (g, _) => {
                    r.push(VerifyStep::skipped_after("token", g.unwrap_or("jwks")));
                }
            }
        }
        r.finish()
    }

    /// `jwks-ok`, or `jwks-foreign-keys` when the configured JWKS shares no
    /// key id with the one the issuer publishes (another IdP or realm).
    async fn jwks_ok_or_foreign(
        &self,
        keys: &[KeyInfo],
        url: &str,
        discovered: Option<&str>,
    ) -> VerifyStep {
        let kids: BTreeSet<String> = keys.iter().filter_map(|k| k.kid.clone()).collect();
        if let Some(d) = discovered
            && d.trim_end_matches('/') != url.trim_end_matches('/')
            && let Ok(raw) = get_with(d, &self.guard, self.timeout, &Credential::None).await
            && let Ok(v) = raw.json::<Value>(d)
            && let Some(theirs) = parse_jwks(&v)
        {
            let theirs: BTreeSet<String> = theirs
                .into_iter()
                .filter(|k| k.usable)
                .filter_map(|k| k.kid)
                .collect();
            if !theirs.is_empty() && kids.is_disjoint(&theirs) {
                return VerifyStep::new(
                    "jwks",
                    Fail,
                    "jwks-foreign-keys",
                    format!(
                        "The keys at {} are not the issuer's: none of their key ids appears at {}. Tokens of this issuer would fail signature checks.",
                        display_url(url),
                        display_url(d)
                    ),
                )
                .param("configured", display_url(url))
                .param("discovered", display_url(d));
            }
        }
        let mut algs: BTreeSet<String> = BTreeSet::new();
        for k in keys {
            algs.extend(k.algorithms());
        }
        let kids = kids.into_iter().collect::<Vec<_>>().join(", ");
        let algs = algs.into_iter().collect::<Vec<_>>().join(", ");
        VerifyStep::new(
            "jwks",
            Pass,
            "jwks-ok",
            format!("{} signing key(s): {kids} ({algs}).", keys.len()),
        )
        .param("count", keys.len().to_string())
        .param("kids", kids)
        .param("algs", algs)
    }

    async fn token_step(&self, auth: &EndpointAuth, jwks_url: &str, jwt: &str) -> VerifyStep {
        use crate::error::AuthError;
        use crate::provider::AuthProvider;
        let started = Instant::now();
        let mut effective = auth.clone();
        effective.jwks_url = Some(jwks_url.to_string());
        let config = AuthConfig::from_endpoint(&effective, &AuthConfig::default());
        let cache = std::sync::Arc::new(
            crate::jwks::JwksCache::new(jwks_url, 0)
                .with_max_ttl(config.jwks_max_age_secs)
                .with_http_client(self.guard.client(self.timeout)),
        );
        let provider = crate::provider::jwks_bearer::JwksBearerAuthProvider::new(cache, &config);
        let claims = peek_claims(jwt);
        let claims = claims.as_ref();
        let step = match provider.authenticate(&format!("Bearer {jwt}")).await {
            Ok(p) => {
                let scopes = if p.scopes.raw().is_empty() {
                    "(none)".to_string()
                } else {
                    p.scopes.raw().join(" ")
                };
                VerifyStep::new(
                    "token",
                    Pass,
                    "token-ok",
                    format!("The token is accepted: subject {}, scopes {scopes}.", p.subject),
                )
                .field("token")
                .param("sub", p.subject.clone())
                .param("scopes", scopes)
            }
            Err(e) => match e {
                AuthError::TokenExpired => {
                    let at = claims
                        .and_then(|c| c.get("exp"))
                        .and_then(Value::as_i64)
                        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                        .unwrap_or_else(|| "(unknown)".into());
                    VerifyStep::new(
                        "token",
                        Fail,
                        "token-expired",
                        format!("The token expired at {at}."),
                    )
                    .field("token")
                    .param("expired_at", at)
                }
                AuthError::InvalidSignature | AuthError::UnknownKid { .. } => VerifyStep::new(
                    "token",
                    Fail,
                    "token-signature",
                    "The token's signature does not verify against the JWKS (issued by another realm or IdP?).",
                )
                .field("token"),
                AuthError::UnsupportedAlgorithm { alg } => VerifyStep::new(
                    "token",
                    Fail,
                    "token-algorithm",
                    format!("The token is signed with {alg}, which this endpoint does not accept."),
                )
                .field("algorithms")
                .param("alg", alg),
                AuthError::InvalidTokenFormat(_) => VerifyStep::new(
                    "token",
                    Fail,
                    "token-malformed",
                    "This is not a JWT (three base64url parts separated by dots).",
                )
                .field("token"),
                AuthError::ValidationError(m) if m.contains("audience") => {
                    let expected = trimmed(&auth.audience).unwrap_or_default();
                    let got = claim_text(claims, "aud");
                    VerifyStep::new(
                        "token",
                        Fail,
                        "token-audience",
                        format!("The token's audience is {got}; this endpoint expects {expected}."),
                    )
                    .field("audience")
                    .param("got", got)
                    .param("expected", expected)
                }
                AuthError::ValidationError(m) if m.contains("issuer") => {
                    let expected = trimmed(&auth.issuer).unwrap_or_default();
                    let got = claim_text(claims, "iss");
                    VerifyStep::new(
                        "token",
                        Fail,
                        "token-issuer",
                        format!("The token's issuer is {got}; this endpoint expects {expected}."),
                    )
                    .field("issuer")
                    .param("got", got)
                    .param("expected", expected)
                }
                AuthError::ValidationError(m) if m.contains("nbf") => VerifyStep::new(
                    "token",
                    Fail,
                    "token-not-yet-valid",
                    "The token is not valid yet (nbf in the future; check the clocks).",
                )
                .field("token"),
                AuthError::ValidationError(m) if m.contains("claim") => {
                    let claim = m.rsplit(['\'', ' ']).find(|s| !s.is_empty()).unwrap_or("");
                    VerifyStep::new(
                        "token",
                        Fail,
                        "token-missing-claim",
                        format!("The token lacks a required claim ({m})."),
                    )
                    .field("token")
                    .param("claim", claim.trim_matches('\'').to_string())
                }
                other => VerifyStep::new(
                    "token",
                    Fail,
                    "token-refused",
                    format!("The token is refused: {other}."),
                )
                .field("token")
                .param("reason", other.to_string()),
            },
        };
        step.since(started)
    }
}

impl Builder {
    fn push_alg(&mut self, auth: &EndpointAuth, keys: &[KeyInfo]) -> VerifyStatus {
        let started = Instant::now();
        let configured: Vec<String> = if auth.algorithms.is_empty() {
            DEFAULT_ALGORITHMS.iter().map(|s| s.to_string()).collect()
        } else {
            auth.algorithms.clone()
        };
        let supported: Vec<&String> = configured
            .iter()
            .filter(|a| VERIFIER_ALGORITHMS.contains(&a.as_str()))
            .collect();
        let configured_text = configured.join(", ");
        if supported.is_empty() {
            return self.push(
                VerifyStep::new(
                    "algorithms",
                    Fail,
                    "algorithms-unsupported",
                    format!(
                        "None of {configured_text} is supported by the verifier ({}).",
                        VERIFIER_ALGORITHMS.join(", ")
                    ),
                )
                .field("algorithms")
                .param("configured", configured_text)
                .param("supported", VERIFIER_ALGORITHMS.join(", "))
                .since(started),
            );
        }
        let key_algs: BTreeSet<String> = keys.iter().flat_map(KeyInfo::algorithms).collect();
        let keys_text = key_algs.iter().cloned().collect::<Vec<_>>().join(", ");
        let usable: Vec<String> = supported
            .iter()
            .filter(|a| key_algs.contains(a.as_str()))
            .map(|a| a.to_string())
            .collect();
        if usable.is_empty() {
            return self.push(
                VerifyStep::new(
                    "algorithms",
                    Fail,
                    "algorithms-no-match",
                    format!(
                        "The accepted algorithms ({configured_text}) match none of the keys ({keys_text})."
                    ),
                )
                .field("algorithms")
                .param("configured", configured_text)
                .param("keys", keys_text)
                .since(started),
            );
        }
        let usable = usable.join(", ");
        self.push(
            VerifyStep::new(
                "algorithms",
                Pass,
                "algorithms-ok",
                format!("Tokens signed with {usable} can be verified."),
            )
            .field("algorithms")
            .param("algorithms", usable)
            .since(started),
        )
    }
}

fn smart_step(m: &OidcMetadata) -> VerifyStep {
    let mut missing: Vec<(&str, &str)> = Vec::new();
    if m.token_endpoint.is_none() {
        missing.push(("smart-no-token", "token endpoint"));
    }
    if m.authorization_endpoint.is_none() {
        missing.push(("smart-no-authorize", "authorization endpoint"));
    } else if !m
        .code_challenge_methods_supported
        .iter()
        .any(|c| c == "S256")
    {
        missing.push(("smart-no-pkce", "PKCE S256"));
    }
    if !m.grant_types_supported.is_empty()
        && !m
            .grant_types_supported
            .iter()
            .any(|g| g == "client_credentials")
    {
        missing.push(("smart-no-client-credentials", "client_credentials grant"));
    }
    let grants = if m.grant_types_supported.is_empty() {
        "(not stated)".to_string()
    } else {
        m.grant_types_supported.join(", ")
    };
    match missing.first() {
        None => VerifyStep::new(
            "smart",
            Pass,
            "smart-ok",
            format!(
                "The IdP offers authorization and token endpoints with PKCE S256 (grant types: {grants})."
            ),
        )
        .param("grants", grants),
        Some((code, _)) => {
            let list = missing.iter().map(|(_, w)| *w).collect::<Vec<_>>().join(", ");
            VerifyStep::new(
                "smart",
                Warn,
                code,
                format!("The discovery document does not advertise: {list}."),
            )
            .param("missing", list)
            .param("grants", grants)
        }
    }
}

/// The server-level (environment, `HFS_AUTH_*`) auth settings as an
/// endpoint auth section, so an endpoint that inherits them can be verified
/// against its effective configuration.
pub fn endpoint_auth_from_config(config: &AuthConfig) -> EndpointAuth {
    use crate::config::AuthMode;
    EndpointAuth {
        mode: if !config.enabled {
            EndpointAuthMode::Off
        } else {
            match config.mode {
                AuthMode::Off => EndpointAuthMode::Off,
                AuthMode::WriteProtect => EndpointAuthMode::WriteProtect,
                AuthMode::Full => EndpointAuthMode::Full,
            }
        },
        issuer: config.expected_issuer.clone(),
        jwks_url: config.jwks_url.clone(),
        audience: config.expected_audience.clone(),
        algorithms: config.allowed_algorithms.clone(),
        clock_skew_secs: u32::try_from(config.clock_skew_secs).ok(),
        jwks_max_age_secs: u32::try_from(config.jwks_max_age_secs).ok(),
        ..EndpointAuth::default()
    }
}

// ------------------------------------------------------------ terminology

const LOOKUP_SYSTEM: &str = "http://loinc.org";
const LOOKUP_CODE: &str = "8867-4";
const EXPAND_VALUESET: &str = "http://hl7.org/fhir/ValueSet/administrative-gender";

fn software_of(cs: &Value) -> (String, String) {
    let sw = cs.get("software");
    let name = sw
        .and_then(|s| s.get("name"))
        .and_then(Value::as_str)
        .or_else(|| cs.get("name").and_then(Value::as_str))
        .unwrap_or("(unnamed)")
        .to_string();
    let version = sw
        .and_then(|s| s.get("version"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    (name, version)
}

/// Verifies a terminology server (kind `terminology`): `input`,
/// `outbound`, `connect`, `metadata`, `token` (when given), `lookup`
/// (LOINC 8867-4), `expand` (administrative-gender) and `latency` (slowest
/// answer against `timeout`). Console-entered URLs go through `guard`.
pub async fn verify_terminology_server(
    url: &str,
    token: Option<&str>,
    timeout: Duration,
    guard: &OutboundGuard,
) -> VerifyReport {
    let mut r = Builder::new("terminology");
    let base = url.trim().trim_end_matches('/').to_string();
    let token = token.map(str::trim).filter(|t| !t.is_empty());
    let token = token.map(|t| t.strip_prefix("Bearer ").unwrap_or(t).trim().to_string());
    let field = "terminologyUrl";
    let timeout = if timeout.is_zero() {
        Duration::from_secs(10)
    } else {
        timeout
    };
    let steps_after = |r: &mut Builder, gate: &str, with_token: bool| {
        for id in ["connect", "metadata"] {
            if r.report.steps.iter().all(|s| s.id != id) {
                r.push(VerifyStep::skipped_after(id, gate));
            }
        }
        if with_token {
            r.push(VerifyStep::skipped_after("token", gate));
        }
        for id in ["lookup", "expand", "latency"] {
            r.push(VerifyStep::skipped_after(id, gate));
        }
    };

    if r.push(input_step(guard, &[(field, &base)], Instant::now())) == Fail {
        r.push(VerifyStep::skipped_after("outbound", "input"));
        steps_after(&mut r, "input", token.is_some());
        return r.finish();
    }
    if r.push(outbound_step(guard, &[(field, &base)]).await) == Fail {
        steps_after(&mut r, "outbound", token.is_some());
        return r.finish();
    }
    let credential = match &token {
        Some(t) => Credential::Bearer(t.clone()),
        None => Credential::None,
    };
    let mut slowest: u64 = 0;

    // connect + metadata
    let metadata_url = join_url(&base, "metadata");
    let started = Instant::now();
    let raw = match get_with(&metadata_url, guard, timeout, &credential).await {
        Err(e) => {
            r.push(connect_failure(field, &metadata_url, &e, started));
            steps_after(&mut r, "connect", token.is_some());
            return r.finish();
        }
        Ok(raw) => raw,
    };
    r.push(connect_ok(&metadata_url, &raw));
    slowest = slowest.max(raw.total_ms);
    let metadata_step = match raw.json::<Value>(&metadata_url) {
        Ok(cs) if cs.get("resourceType").and_then(Value::as_str) == Some("CapabilityStatement") => {
            let (software, version) = software_of(&cs);
            let fhir = cs
                .get("fhirVersion")
                .and_then(Value::as_str)
                .unwrap_or("(not stated)")
                .to_string();
            VerifyStep::new(
                "metadata",
                Pass,
                "metadata-ok",
                format!("{software} {version} answers, FHIR {fhir}."),
            )
            .param("software", software)
            .param("version", version)
            .param("fhir", fhir)
        }
        Err(FetchError::Status { status: 404, .. }) => VerifyStep::new(
            "metadata",
            Fail,
            "metadata-404",
            format!(
                "No CapabilityStatement at {}: this is probably not the FHIR base URL.",
                display_url(&metadata_url)
            ),
        ),
        Err(FetchError::Status { status, .. }) if status == 401 || status == 403 => {
            VerifyStep::new(
                "metadata",
                Fail,
                "metadata-unauthorized",
                format!("The server refused /metadata with HTTP {status}."),
            )
            .param("status", status.to_string())
            .field(if token.is_some() {
                "terminologyToken"
            } else {
                field
            })
        }
        Err(FetchError::Status { status, .. }) => VerifyStep::new(
            "metadata",
            Fail,
            "metadata-status",
            format!("/metadata answered HTTP {status}."),
        )
        .param("status", status.to_string()),
        _ => VerifyStep::new(
            "metadata",
            Fail,
            "metadata-not-fhir",
            "The URL did not answer with a FHIR CapabilityStatement.",
        ),
    };
    let metadata_step = metadata_step
        .param("url", display_url(&metadata_url))
        .url(&metadata_url)
        .ms(raw.total_ms);
    let metadata_step = if metadata_step.field.is_none() {
        metadata_step.field(field)
    } else {
        metadata_step
    };
    if r.push(metadata_step) == Fail {
        if token.is_some() {
            r.push(VerifyStep::skipped_after("token", "metadata"));
        }
        for id in ["lookup", "expand", "latency"] {
            r.push(VerifyStep::skipped_after(id, "metadata"));
        }
        return r.finish();
    }

    // token: the same search with and without it.
    if token.is_some() {
        let probe_url = join_url(&base, "CodeSystem?_summary=count");
        let started = Instant::now();
        let with = get_with(&probe_url, guard, timeout, &credential).await;
        let without = get_with(&probe_url, guard, timeout, &Credential::None).await;
        let status = |x: &Result<RawResponse, FetchError>| x.as_ref().ok().map(|r| r.status);
        let step = match (status(&with), status(&without)) {
            (Some(401) | Some(403), _) => VerifyStep::new(
                "token",
                Fail,
                "token-refused",
                "The server refuses the token (HTTP 401/403).",
            ),
            (Some(s), Some(401) | Some(403)) if (200..300).contains(&s) => VerifyStep::new(
                "token",
                Pass,
                "token-accepted",
                "The server accepts the token (and refuses requests without it).",
            ),
            _ => VerifyStep::new(
                "token",
                Skipped,
                "token-not-checkable",
                "The server answers without a token too, so whether it accepts this one cannot be told.",
            ),
        };
        if let Ok(raw) = &with {
            slowest = slowest.max(raw.total_ms);
        }
        r.push(
            step.field("terminologyToken")
                .url(&probe_url)
                .since(started),
        );
    }

    // lookup
    let lookup_url = join_url(
        &base,
        &format!("CodeSystem/$lookup?system={LOOKUP_SYSTEM}&code={LOOKUP_CODE}"),
    );
    let started = Instant::now();
    let step = match get_with(&lookup_url, guard, timeout, &credential).await {
        Err(e) => VerifyStep::new(
            "lookup",
            Warn,
            "lookup-failed",
            format!("$lookup failed: {e}."),
        )
        .param("reason", transport_reason(&e)),
        Ok(raw) => {
            slowest = slowest.max(raw.total_ms);
            match raw.json::<Value>(&lookup_url) {
                Ok(p) if p.get("resourceType").and_then(Value::as_str) == Some("Parameters") => {
                    let display = p
                        .get("parameter")
                        .and_then(Value::as_array)
                        .and_then(|ps| {
                            ps.iter()
                                .find(|x| x.get("name").and_then(Value::as_str) == Some("display"))
                        })
                        .and_then(|x| x.get("valueString"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    VerifyStep::new(
                        "lookup",
                        Pass,
                        "lookup-ok",
                        format!("LOINC {LOOKUP_CODE} resolves to \"{display}\"."),
                    )
                    .param("display", display)
                }
                Err(FetchError::Status { status, .. }) if (400..500).contains(&status) => {
                    VerifyStep::new(
                        "lookup",
                        Warn,
                        "lookup-missing-system",
                        format!("The server does not know {LOOKUP_SYSTEM} (HTTP {status})."),
                    )
                    .param("status", status.to_string())
                }
                Err(FetchError::Status { status, .. }) => VerifyStep::new(
                    "lookup",
                    Warn,
                    "lookup-failed",
                    format!("$lookup answered HTTP {status}."),
                )
                .param("status", status.to_string()),
                _ => VerifyStep::new(
                    "lookup",
                    Warn,
                    "lookup-failed",
                    "$lookup did not answer with Parameters.",
                ),
            }
        }
    };
    r.push(
        step.param("system", LOOKUP_SYSTEM)
            .param("code", LOOKUP_CODE)
            .url(&lookup_url)
            .since(started),
    );

    // expand
    let expand_url = join_url(&base, &format!("ValueSet/$expand?url={EXPAND_VALUESET}"));
    let started = Instant::now();
    let step = match get_with(&expand_url, guard, timeout, &credential).await {
        Err(e) => VerifyStep::new(
            "expand",
            Warn,
            "expand-failed",
            format!("$expand failed: {e}."),
        )
        .param("reason", transport_reason(&e)),
        Ok(raw) => {
            slowest = slowest.max(raw.total_ms);
            match raw.json::<Value>(&expand_url) {
                Ok(vs) if vs.get("expansion").is_some() => {
                    let exp = &vs["expansion"];
                    let count = exp
                        .get("total")
                        .and_then(Value::as_u64)
                        .or_else(|| {
                            exp.get("contains")
                                .and_then(Value::as_array)
                                .map(|c| c.len() as u64)
                        })
                        .unwrap_or(0);
                    VerifyStep::new(
                        "expand",
                        Pass,
                        "expand-ok",
                        format!("administrative-gender expands to {count} code(s)."),
                    )
                    .param("count", count.to_string())
                }
                Err(FetchError::Status { status, .. }) if (400..500).contains(&status) => {
                    VerifyStep::new(
                        "expand",
                        Warn,
                        "expand-missing",
                        format!("The server cannot expand {EXPAND_VALUESET} (HTTP {status})."),
                    )
                    .param("status", status.to_string())
                }
                Err(FetchError::Status { status, .. }) => VerifyStep::new(
                    "expand",
                    Warn,
                    "expand-failed",
                    format!("$expand answered HTTP {status}."),
                )
                .param("status", status.to_string()),
                _ => VerifyStep::new(
                    "expand",
                    Warn,
                    "expand-failed",
                    "$expand did not answer with an expanded ValueSet.",
                ),
            }
        }
    };
    r.push(
        step.param("valueset", EXPAND_VALUESET)
            .url(&expand_url)
            .since(started),
    );

    // latency
    let timeout_ms = timeout.as_millis() as u64;
    let step = if slowest * 2 > timeout_ms {
        VerifyStep::new(
            "latency",
            Warn,
            "latency-high",
            format!(
                "The slowest answer took {slowest} ms, more than half the {timeout_ms} ms timeout."
            ),
        )
        .field("terminologyTimeout")
    } else {
        VerifyStep::new(
            "latency",
            Pass,
            "latency-ok",
            format!("The slowest answer took {slowest} ms (timeout {timeout_ms} ms)."),
        )
    };
    r.push(
        step.param("ms", slowest.to_string())
            .param("timeout_ms", timeout_ms.to_string()),
    );
    r.finish()
}

// ------------------------------------------------------------ sync peers

/// Which side of a sync job an external server is.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PeerRole {
    /// Read from.
    Source,
    /// Written to.
    Target,
}

impl PeerRole {
    /// `source` / `target` (the sync form's field prefix).
    pub fn as_str(self) -> &'static str {
        match self {
            PeerRole::Source => "source",
            PeerRole::Target => "target",
        }
    }
}

/// Credentials for an external FHIR server, in plaintext (in memory only,
/// for one verification; never logged).
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PeerAuth {
    /// None.
    #[default]
    None,
    /// Static bearer token.
    Bearer {
        /// The token.
        #[serde(default)]
        token: String,
    },
    /// HTTP Basic.
    Basic {
        /// User name.
        #[serde(default)]
        username: String,
        /// Password.
        #[serde(default)]
        password: String,
    },
    /// OAuth 2.0 client credentials.
    #[serde(rename_all = "camelCase")]
    ClientCredentials {
        /// Token endpoint.
        #[serde(default)]
        token_url: String,
        /// Client id.
        #[serde(default)]
        client_id: String,
        /// Client secret.
        #[serde(default)]
        client_secret: String,
        /// Requested scope.
        #[serde(default)]
        scope: Option<String>,
    },
}

impl std::fmt::Debug for PeerAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerAuth::None => write!(f, "PeerAuth::None"),
            PeerAuth::Bearer { .. } => write!(f, "PeerAuth::Bearer([redacted])"),
            PeerAuth::Basic { username, .. } => {
                write!(f, "PeerAuth::Basic({username}, [redacted])")
            }
            PeerAuth::ClientCredentials {
                token_url,
                client_id,
                ..
            } => write!(
                f,
                "PeerAuth::ClientCredentials({}, {client_id}, [redacted])",
                redact_url(token_url)
            ),
        }
    }
}

/// An external FHIR server to verify.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PeerConfig {
    /// FHIR base URL.
    pub url: String,
    /// Credentials.
    #[serde(default)]
    pub auth: PeerAuth,
}

/// Body of `POST /admin/sync/$verify-peer` (and the console's port).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SyncPeerVerifyRequest {
    /// Which side.
    pub role: PeerRole,
    /// FHIR base URL.
    pub url: String,
    /// Credentials (`{"type": "none" | "bearer" | "basic" |
    /// "client-credentials", …}`).
    #[serde(default)]
    pub auth: PeerAuth,
    /// Resource types the job copies (empty = all).
    #[serde(default)]
    pub resource_types: Vec<String>,
    /// Expected FHIR release or version (`R4`, `4.0.1`, …).
    #[serde(default)]
    pub fhir_version: Option<String>,
    /// An existing job whose stored secret fills an empty secret field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
}

impl SyncPeerVerifyRequest {
    /// The peer part.
    pub fn peer(&self) -> PeerConfig {
        PeerConfig {
            url: self.url.clone(),
            auth: self.auth.clone(),
        }
    }
}

/// `R4`, `R4B`, `R5`, `R6` for a release label or a version number.
fn release_of(v: &str) -> Option<&'static str> {
    let v = v.trim().to_ascii_uppercase();
    Some(match v.as_str() {
        "R4" => "R4",
        "R4B" => "R4B",
        "R5" => "R5",
        "R6" => "R6",
        _ if v.starts_with("4.0") => "R4",
        _ if v.starts_with("4.3") || v.starts_with("4.1") => "R4B",
        _ if v.starts_with("5.") => "R5",
        _ if v.starts_with("6.") => "R6",
        _ => return None,
    })
}

/// Thousands separators (`98,311`).
fn grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `rest[mode=server]` of a CapabilityStatement.
fn server_rest(cs: &Value) -> Option<&Value> {
    let rest = cs.get("rest")?.as_array()?;
    rest.iter()
        .find(|r| r.get("mode").and_then(Value::as_str) == Some("server"))
        .or_else(|| rest.first())
}

fn cs_resource<'a>(cs: &'a Value, rtype: &str) -> Option<&'a Value> {
    server_rest(cs)?
        .get("resource")?
        .as_array()?
        .iter()
        .find(|r| r.get("type").and_then(Value::as_str) == Some(rtype))
}

fn codes(v: Option<&Value>, key: &str, field: &str) -> Vec<String> {
    v.and_then(|v| v.get(key))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get(field).and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

enum TokenOutcome {
    Ok(String),
    Step(VerifyStep),
}

async fn client_credentials_token(
    guard: &OutboundGuard,
    timeout: Duration,
    token_url: &str,
    client_id: &str,
    client_secret: &str,
    scope: Option<&str>,
) -> TokenOutcome {
    let started = Instant::now();
    let field = |f: &str| f.to_string();
    if let Err(reason) = guard.check_url(token_url) {
        return TokenOutcome::Step(
            VerifyStep::new(
                "token",
                Fail,
                "token-endpoint-unreachable",
                format!("The token endpoint is refused by the outbound policy: {reason}."),
            )
            .field(field("tokenUrl"))
            .param("url", display_url(token_url))
            .param("reason", "blocked")
            .url(token_url)
            .since(started),
        );
    }
    let body = {
        let mut form = form_urlencoded::Serializer::new(String::new());
        form.append_pair("grant_type", "client_credentials");
        if let Some(s) = scope.map(str::trim).filter(|s| !s.is_empty()) {
            form.append_pair("scope", s);
        }
        form.finish()
    };
    let request = guard
        .client(timeout)
        .post(token_url)
        .basic_auth(client_id, Some(client_secret))
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(reqwest::header::ACCEPT, "application/json")
        .body(body);
    let step = |status, code: &str, detail: String| {
        VerifyStep::new("token", status, code, detail)
            .param("url", display_url(token_url))
            .url(token_url)
            .since(started)
    };
    let raw = match send_raw(token_url, request).await {
        Err(e) => {
            return TokenOutcome::Step(
                step(
                    Fail,
                    "token-endpoint-unreachable",
                    format!("The token endpoint could not be reached: {e}."),
                )
                .field(field("tokenUrl"))
                .param("reason", transport_reason(&e)),
            );
        }
        Ok(raw) => raw,
    };
    let body: Value = serde_json::from_slice(&raw.body).unwrap_or(Value::Null);
    if (200..300).contains(&raw.status)
        && let Some(t) = body.get("access_token").and_then(Value::as_str)
    {
        return TokenOutcome::Ok(t.to_string());
    }
    let error = body
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let description = body
        .get("error_description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let s = match error.as_str() {
        "invalid_client" | "unauthorized_client" => step(
            Fail,
            "token-invalid-client",
            format!("The token endpoint refused the client ({error}: {description})."),
        )
        .field(field("clientId")),
        "invalid_scope" => step(
            Fail,
            "token-invalid-scope",
            format!("The token endpoint refused the scope ({description})."),
        )
        .field(field("scope")),
        "" if raw.status == 404 => step(
            Fail,
            "token-endpoint-404",
            "No token endpoint at this URL (HTTP 404).".into(),
        )
        .field(field("tokenUrl")),
        "" => step(
            Fail,
            "token-bad-response",
            format!(
                "The token endpoint answered HTTP {} without an access token.",
                raw.status
            ),
        )
        .field(field("tokenUrl"))
        .param("status", raw.status.to_string()),
        other => step(
            Fail,
            "token-error",
            format!("The token endpoint answered {other}: {description}."),
        )
        .field(field("clientId")),
    };
    TokenOutcome::Step(s.param("error", error).param("description", description))
}

/// The form field holding the secret of `auth`.
fn secret_field(auth: &PeerAuth) -> &'static str {
    match auth {
        PeerAuth::Basic { .. } => "password",
        PeerAuth::ClientCredentials { .. } => "clientSecret",
        _ => "token",
    }
}

/// Verifies the external side of a sync job (kind `sync-source` /
/// `sync-target`): `input`, `outbound`, `connect`, `metadata` (and the FHIR
/// release against `expected_fhir`), `token`, `counts` (per type
/// `_summary=count`), `write` (target: create/update advertised) or
/// `incremental` (source: `_lastUpdated` / history advertised). Fields are
/// the console's sync form fields (`url`, `authType`, `tokenUrl`, `clientId`, `clientSecret`, `token`, `username`, `password`, `scope`, `resourceTypes`).
pub async fn verify_fhir_peer(
    side: &PeerConfig,
    role: PeerRole,
    expected_fhir: Option<&str>,
    resource_types: &[String],
    guard: &OutboundGuard,
) -> VerifyReport {
    verify_fhir_peer_with_timeout(
        side,
        role,
        expected_fhir,
        resource_types,
        guard,
        Duration::from_secs(15),
    )
    .await
}

/// [`verify_fhir_peer`] with a per-request timeout.
pub async fn verify_fhir_peer_with_timeout(
    side: &PeerConfig,
    role: PeerRole,
    expected_fhir: Option<&str>,
    resource_types: &[String],
    guard: &OutboundGuard,
    timeout: Duration,
) -> VerifyReport {
    let kind = match role {
        PeerRole::Source => "sync-source",
        PeerRole::Target => "sync-target",
    };
    let mut r = Builder::new(kind);
    let f = |name: &str| name.to_string();
    let base = side.url.trim().trim_end_matches('/').to_string();
    let last = match role {
        PeerRole::Source => "incremental",
        PeerRole::Target => "write",
    };
    let skip_rest = |r: &mut Builder, from: &[&str], gate: &str| {
        for id in from {
            r.push(VerifyStep::skipped_after(id, gate));
        }
    };
    let all_after_outbound = ["connect", "metadata", "token", "counts", last];

    // input
    let url_field = f("url");
    let token_field = f("tokenUrl");
    let mut urls: Vec<(&str, &str)> = vec![(url_field.as_str(), base.as_str())];
    let started = Instant::now();
    let mut input = None;
    match &side.auth {
        PeerAuth::ClientCredentials {
            token_url,
            client_id,
            client_secret,
            ..
        } => {
            urls.push((token_field.as_str(), token_url.as_str()));
            if client_id.trim().is_empty() {
                input = Some(("clientId", "Client id"));
            } else if client_secret.is_empty() {
                input = Some(("clientSecret", "Client secret"));
            }
        }
        PeerAuth::Bearer { token } if token.trim().is_empty() => {
            input = Some(("token", "Token"));
        }
        PeerAuth::Basic { username, .. } if username.trim().is_empty() => {
            input = Some(("username", "User name"));
        }
        _ => {}
    }
    let mut step = input_step(guard, &urls, started);
    if step.status != Fail
        && let Some((name, label)) = input
    {
        step = VerifyStep::new(
            "input",
            Fail,
            "input-required",
            format!("{label} is required."),
        )
        .field(f(name))
        .param("field", f(name))
        .since(started);
    }
    if r.push(step) == Fail {
        r.push(VerifyStep::skipped_after("outbound", "input"));
        skip_rest(&mut r, &all_after_outbound, "input");
        return r.finish();
    }
    if r.push(outbound_step(guard, &urls).await) == Fail {
        skip_rest(&mut r, &all_after_outbound, "outbound");
        return r.finish();
    }

    // credentials (the token request runs first; its step is listed after
    // metadata)
    let mut token_step: Option<VerifyStep> = None;
    let credential = match &side.auth {
        PeerAuth::None => Credential::None,
        PeerAuth::Bearer { token } => Credential::Bearer(token.trim().to_string()),
        PeerAuth::Basic { username, password } => {
            Credential::Basic(username.clone(), password.clone())
        }
        PeerAuth::ClientCredentials {
            token_url,
            client_id,
            client_secret,
            scope,
        } => {
            let started = Instant::now();
            match client_credentials_token(
                guard,
                timeout,
                token_url.trim(),
                client_id.trim(),
                client_secret,
                scope.as_deref(),
            )
            .await
            {
                TokenOutcome::Ok(t) => {
                    token_step = Some(
                        VerifyStep::new(
                            "token",
                            Pass,
                            "token-issued",
                            "The token endpoint issued an access token.",
                        )
                        .field(f("clientId"))
                        .url(token_url)
                        .since(started),
                    );
                    Credential::Bearer(t)
                }
                TokenOutcome::Step(s) => {
                    token_step = Some(s);
                    Credential::None
                }
            }
        }
    };

    // connect + metadata
    let metadata_url = join_url(&base, "metadata");
    let started = Instant::now();
    let raw = match get_with(&metadata_url, guard, timeout, &credential).await {
        Err(e) => {
            r.push(connect_failure(&url_field, &metadata_url, &e, started));
            r.push(VerifyStep::skipped_after("metadata", "connect"));
            match token_step {
                Some(s) if s.status == Fail => {
                    r.push(s);
                }
                _ => {
                    r.push(VerifyStep::skipped_after("token", "connect"));
                }
            }
            skip_rest(&mut r, &["counts", last], "connect");
            return r.finish();
        }
        Ok(raw) => raw,
    };
    r.push(connect_ok(&metadata_url, &raw));
    let mut unauthorized = matches!(raw.status, 401 | 403);
    let cs: Option<Value> = match raw.json::<Value>(&metadata_url) {
        Ok(v) if v.get("resourceType").and_then(Value::as_str) == Some("CapabilityStatement") => {
            Some(v)
        }
        _ => None,
    };
    let metadata_step = match (&cs, raw.status) {
        (Some(cs), _) => {
            let (software, version) = software_of(cs);
            let fhir = cs
                .get("fhirVersion")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let got = release_of(&fhir);
            let want = expected_fhir.and_then(release_of);
            match (got, want) {
                (Some(g), Some(w)) if g != w => VerifyStep::new(
                    "metadata",
                    Fail,
                    "metadata-version-mismatch",
                    format!("The server speaks FHIR {fhir} ({g}); the job expects {w}."),
                )
                .param("got", format!("{g} ({fhir})"))
                .param("expected", w),
                _ => VerifyStep::new(
                    "metadata",
                    Pass,
                    "metadata-ok",
                    format!("{software} {version} answers, FHIR {fhir}."),
                )
                .param("software", software)
                .param("version", version)
                .param("fhir", fhir),
            }
        }
        (None, 404) => VerifyStep::new(
            "metadata",
            Fail,
            "metadata-404",
            format!(
                "No CapabilityStatement at {}: this is probably not the FHIR base URL.",
                display_url(&metadata_url)
            ),
        ),
        (None, 401 | 403) => VerifyStep::new(
            "metadata",
            Fail,
            "metadata-unauthorized",
            format!("The server refused /metadata with HTTP {}.", raw.status),
        )
        .param("status", raw.status.to_string()),
        (None, s) if !(200..300).contains(&s) => VerifyStep::new(
            "metadata",
            Fail,
            "metadata-status",
            format!("/metadata answered HTTP {s}."),
        )
        .param("status", s.to_string()),
        (None, _) => VerifyStep::new(
            "metadata",
            Fail,
            "metadata-not-fhir",
            "The URL did not answer with a FHIR CapabilityStatement.",
        ),
    };
    let metadata_failed = r.push(
        metadata_step
            .field(url_field.clone())
            .param("url", display_url(&metadata_url))
            .url(&metadata_url)
            .ms(raw.total_ms),
    ) == Fail;

    // counts (before the token step is decided: a 401 there tells)
    let mut counts_step = None;
    if !metadata_failed && token_step.as_ref().is_none_or(|s| s.status != Fail) {
        let started = Instant::now();
        if resource_types.is_empty() {
            counts_step = Some(
                VerifyStep::new(
                    "counts",
                    Skipped,
                    "counts-no-types",
                    "No resource types selected: every type is copied, so none is counted.",
                )
                .field("resourceTypes"),
            );
        } else {
            let mut parts = Vec::new();
            let mut unknown = Vec::new();
            let mut refused = None;
            for t in resource_types.iter().take(20) {
                let t = t.trim();
                if t.is_empty() {
                    continue;
                }
                if let Some(cs) = &cs
                    && server_rest(cs).is_some()
                    && cs_resource(cs, t).is_none()
                {
                    unknown.push(t.to_string());
                    continue;
                }
                let u = join_url(&base, &format!("{t}?_summary=count"));
                match get_with(&u, guard, timeout, &credential).await {
                    Ok(raw) if matches!(raw.status, 401 | 403) => {
                        unauthorized = true;
                        refused = Some(raw.status);
                        break;
                    }
                    Ok(raw) if raw.status == 404 => unknown.push(t.to_string()),
                    Ok(raw) => match raw.json::<Value>(&u) {
                        Ok(b) => {
                            let n = b.get("total").and_then(Value::as_u64);
                            parts.push(match n {
                                Some(n) => format!("{t} {}", grouped(n)),
                                None => format!("{t} ?"),
                            });
                        }
                        Err(_) => unknown.push(t.to_string()),
                    },
                    Err(_) => parts.push(format!("{t} ?")),
                }
            }
            let summary = parts.join(" · ");
            counts_step = Some(if let Some(status) = refused {
                VerifyStep::new(
                    "counts",
                    Fail,
                    "counts-refused",
                    format!("The server refused the search with HTTP {status}."),
                )
                .param("status", status.to_string())
            } else if let Some(first) = unknown.first() {
                VerifyStep::new(
                    "counts",
                    Warn,
                    "counts-unknown-type",
                    format!(
                        "The server does not offer {}. Counted: {summary}.",
                        unknown.join(", ")
                    ),
                )
                .field("resourceTypes")
                .param("type", first.clone())
                .param("types", unknown.join(", "))
                .param("summary", summary)
            } else {
                VerifyStep::new(
                    "counts",
                    Pass,
                    "counts-ok",
                    format!("Resources on the server: {summary}."),
                )
                .param("summary", summary)
            });
        }
        if let Some(s) = counts_step.take() {
            counts_step = Some(s.since(started));
        }
    }

    // token
    let token = match token_step {
        Some(s) => s,
        None => match (&side.auth, unauthorized) {
            (PeerAuth::None, true) => VerifyStep::new(
                "token",
                Fail,
                "token-required",
                "The server requires authentication; choose an authentication type.",
            )
            .field(f("authType")),
            (PeerAuth::None, false) => VerifyStep::new(
                "token",
                Pass,
                "token-not-needed",
                "The server answers without credentials.",
            )
            .field(f("authType")),
            (_, true) => VerifyStep::new(
                "token",
                Fail,
                "token-refused",
                "The server refuses the credentials (HTTP 401/403).",
            )
            .field(secret_field(&side.auth)),
            (_, false) => VerifyStep::new(
                "token",
                Pass,
                "token-accepted",
                "The server accepts the credentials.",
            )
            .field(secret_field(&side.auth)),
        },
    };
    let token_failed = r.push(token) == Fail;

    match counts_step {
        Some(s) => {
            r.push(s);
        }
        None => {
            let gate = if metadata_failed { "metadata" } else { "token" };
            r.push(VerifyStep::skipped_after("counts", gate));
        }
    }
    let _ = token_failed;

    // write / incremental
    let Some(cs) = cs else {
        r.push(VerifyStep::skipped_after(last, "metadata"));
        return r.finish();
    };
    let types: Vec<String> = if resource_types.is_empty() {
        codes(server_rest(&cs), "resource", "type")
    } else {
        resource_types
            .iter()
            .map(|t| t.trim().to_string())
            .collect()
    };
    match role {
        PeerRole::Target => {
            let missing: Vec<String> = types
                .iter()
                .filter(|t| {
                    let i = codes(cs_resource(&cs, t), "interaction", "code");
                    !(i.iter().any(|c| c == "update") || i.iter().any(|c| c == "create"))
                })
                .cloned()
                .collect();
            let step = if missing.is_empty() {
                VerifyStep::new(
                    "write",
                    Pass,
                    "write-ok",
                    "The server advertises create/update for the selected types.",
                )
            } else {
                let shown: Vec<_> = missing.iter().take(10).cloned().collect();
                VerifyStep::new(
                    "write",
                    Warn,
                    "write-not-advertised",
                    format!(
                        "The server does not advertise create or update for {}.",
                        shown.join(", ")
                    ),
                )
                .field("resourceTypes")
                .param("types", shown.join(", "))
            };
            r.push(step);
        }
        PeerRole::Source => {
            let rest = server_rest(&cs);
            let system_history = codes(rest, "interaction", "code")
                .iter()
                .any(|c| c == "history-system");
            let system_last_updated = codes(rest, "searchParam", "name")
                .iter()
                .any(|n| n == "_lastUpdated");
            let unsupported: Vec<String> = types
                .iter()
                .filter(|t| {
                    let res = cs_resource(&cs, t);
                    let history = codes(res, "interaction", "code")
                        .iter()
                        .any(|c| c == "history-type");
                    let lu = codes(res, "searchParam", "name")
                        .iter()
                        .any(|n| n == "_lastUpdated");
                    !(history || lu || system_history || system_last_updated)
                })
                .cloned()
                .collect();
            let step = if unsupported.is_empty() {
                VerifyStep::new(
                    "incremental",
                    Pass,
                    "incremental-ok",
                    "The server advertises _lastUpdated search or history: incremental runs read only changes.",
                )
            } else {
                let shown: Vec<_> = unsupported.iter().take(10).cloned().collect();
                VerifyStep::new(
                    "incremental",
                    Warn,
                    "incremental-unsupported",
                    format!(
                        "The server advertises neither _lastUpdated nor history for {}: each run may re-read everything.",
                        shown.join(", ")
                    ),
                )
                .param("types", shown.join(", "))
            };
            r.push(step);
        }
    }
    r.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_endpoints::OutboundPolicy;
    use std::sync::Arc;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const N: &str = "zbeWVBfqrPsdF1phb2EB5hxjz1EMb5WT4MnhMatFnNFquEZIcgXG1DBAUICEZLQM8gRj9kZ83YA1cFhUtkgUx1qMio8YkKweLV6r8RAqnuxrS5Khm4a25M85qB5MhGoCLZgGtla9lJ79mjVN5XBqD7sPL0ENN1GPcnus1I6WF-E-5Uiih-QWsQVlGeUqMH81vTBLemDxvMQv-TiKZP3LHH_T41G1FfyF-rlbkj7WXGkbqG_WvR4FKeRm1X2_RRVw87h2j1lfK4AiUKs4h_DqwqZju3fgOH_0MHj2RKCu0wo0cTx8mQGbLCNSb-sRoWRnWGpGPphI9xVS8znuPB1JYQ";

    fn loopback() -> OutboundGuard {
        OutboundGuard::Policy(Arc::new(OutboundPolicy::parse("127.0.0.1")))
    }

    fn step<'a>(r: &'a VerifyReport, id: &str) -> &'a VerifyStep {
        r.steps
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("no step {id} in {r:#?}"))
    }

    #[track_caller]
    fn expect(r: &VerifyReport, id: &str, status: VerifyStatus, code: &str, field: Option<&str>) {
        let s = step(r, id);
        assert_eq!(
            (s.status, s.code.as_str(), s.field.as_deref()),
            (status, code, field),
            "step {id}: {s:#?}"
        );
        assert!(
            !s.detail.contains("{\""),
            "detail looks like a debug dump: {}",
            s.detail
        );
    }

    /// A realm `/realms/{realm}` on `server` whose JWKS holds key `kid`.
    async fn realm(server: &MockServer, realm: &str, kid: &str, stated: Option<&str>) -> String {
        let issuer = format!("{}/realms/{realm}", server.uri());
        Mock::given(method("GET"))
            .and(path(format!(
                "/realms/{realm}/.well-known/openid-configuration"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issuer": stated.unwrap_or(&issuer),
                "authorization_endpoint": format!("{issuer}/auth"),
                "token_endpoint": format!("{issuer}/token"),
                "jwks_uri": format!("{issuer}/certs"),
                "grant_types_supported": ["authorization_code", "client_credentials"],
                "code_challenge_methods_supported": ["S256"],
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/realms/{realm}/certs")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "keys": [{"kty": "RSA", "kid": kid, "use": "sig", "alg": "RS256", "n": N, "e": "AQAB"}]
            })))
            .mount(server)
            .await;
        issuer
    }

    fn full(issuer: &str, jwks: Option<String>, audience: Option<&str>) -> EndpointAuth {
        EndpointAuth {
            mode: EndpointAuthMode::Full,
            issuer: Some(issuer.to_string()),
            jwks_url: jwks,
            audience: audience.map(str::to_string),
            ..EndpointAuth::default()
        }
    }

    fn sign(claims: Value) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some("k".into());
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(include_bytes!(
            "../tests/data/test_rsa_key.pem"
        ))
        .unwrap();
        jsonwebtoken::encode(&header, &claims, &key).unwrap()
    }

    fn claims(issuer: &str, aud: &str, exp_offset: i64) -> Value {
        serde_json::json!({"sub": "app", "iss": issuer, "aud": aud,
            "exp": jsonwebtoken::get_current_timestamp() as i64 + exp_offset,
            "scope": "system/*.rs"})
    }

    #[tokio::test]
    async fn good_configuration_passes_every_step() {
        let server = MockServer::start().await;
        let issuer = realm(&server, "good", "k", None).await;
        let auth = full(&issuer, Some(format!("{issuer}/certs")), Some("hfs"));
        let r = EndpointAuthProbe::new(loopback()).verify(&auth, None).await;
        assert!(r.ok, "{r:#?}");
        let ids: Vec<&str> = r.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "input",
                "outbound",
                "connect",
                "discovery",
                "issuer",
                "jwks-url",
                "jwks",
                "algorithms",
                "smart",
                "audience"
            ]
        );
        assert!(r.steps.iter().all(|s| s.status == Pass), "{r:#?}");
        expect(&r, "jwks", Pass, "jwks-ok", Some("jwksUrl"));
        assert_eq!(step(&r, "jwks").params["kids"], "k");
        assert_eq!(step(&r, "algorithms").params["algorithms"], "RS256");
        assert!(
            step(&r, "discovery")
                .url
                .as_deref()
                .unwrap()
                .ends_with("openid-configuration")
        );
        assert_eq!(r.failures(), 0);
        assert!(r.first_failure().is_none());
        // Discovery fetched once.
        let discovery_hits = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|q| q.url.path().ends_with("openid-configuration"))
            .count();
        assert_eq!(discovery_hits, 1);
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["kind"], "endpoint-auth");
        assert_eq!(json["steps"][0]["status"], "pass");
        assert!(json["steps"][0]["elapsedMs"].is_u64());
        assert!(json["checkedAt"].as_str().unwrap().ends_with('Z'));
        let back: VerifyReport = serde_json::from_value(json).unwrap();
        assert_eq!(back, r);
    }

    #[tokio::test]
    async fn wrong_issuer_path_is_a_discovery_404() {
        let server = MockServer::start().await;
        let good = realm(&server, "good", "k", None).await;
        let typo = good.replace("/good", "/goood");
        let auth = full(&typo, Some(format!("{good}/certs")), Some("hfs"));
        let r = EndpointAuthProbe::new(loopback()).verify(&auth, None).await;
        assert!(!r.ok);
        expect(&r, "connect", Pass, "connect-ok", None);
        expect(&r, "discovery", Fail, "discovery-404", Some("issuer"));
        assert!(
            step(&r, "discovery").params["url"]
                .ends_with("/realms/goood/.well-known/openid-configuration")
        );
        expect(&r, "issuer", Skipped, "skipped-after-failure", None);
        assert_eq!(step(&r, "issuer").params["after"], "discovery");
        assert_eq!(r.first_failure().unwrap().id, "discovery");
        // The configured JWKS is still checked.
        expect(&r, "jwks", Pass, "jwks-ok", Some("jwksUrl"));
    }

    #[tokio::test]
    async fn html_instead_of_json_is_named() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/login/.well-known/openid-configuration"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html; charset=utf-8")
                    .set_body_string("<!DOCTYPE html><html><body>Sign in</body></html>"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/plain/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_string("hello"))
            .mount(&server)
            .await;
        let probe = EndpointAuthProbe::new(loopback());
        let r = probe
            .verify(
                &full(&format!("{}/login", server.uri()), None, Some("a")),
                None,
            )
            .await;
        expect(&r, "discovery", Fail, "discovery-html", Some("issuer"));
        expect(&r, "jwks-url", Skipped, "skipped-after-failure", None);
        expect(&r, "jwks", Skipped, "skipped-after-failure", None);
        expect(&r, "smart", Skipped, "skipped-after-failure", None);
        let r = probe
            .verify(
                &full(&format!("{}/plain", server.uri()), None, Some("a")),
                None,
            )
            .await;
        expect(&r, "discovery", Fail, "discovery-not-json", Some("issuer"));
    }

    #[tokio::test]
    async fn issuer_mismatch_suggests_the_stated_issuer() {
        let server = MockServer::start().await;
        let issuer = realm(
            &server,
            "wrongiss",
            "k",
            Some("https://somebody-else/realms/x"),
        )
        .await;
        let r = EndpointAuthProbe::new(loopback())
            .verify(&full(&issuer, None, Some("hfs")), None)
            .await;
        assert!(!r.ok);
        expect(&r, "issuer", Fail, "issuer-mismatch", Some("issuer"));
        let s = step(&r, "issuer");
        assert_eq!(s.params["stated"], "https://somebody-else/realms/x");
        assert_eq!(s.params["entered"], issuer);
        assert_eq!(r.suggestions["issuer"], "https://somebody-else/realms/x");
        // Trailing slash tolerated.
        let r = EndpointAuthProbe::new(loopback())
            .verify(
                &full(&format!("{}/realms/ok", server.uri()), None, None),
                None,
            )
            .await;
        assert!(r.steps.iter().any(|s| s.id == "discovery"));
        let server2 = MockServer::start().await;
        let iss = realm(&server2, "slash", "k", None).await;
        let r = EndpointAuthProbe::new(loopback())
            .verify(&full(&format!("{iss}/"), None, Some("a")), None)
            .await;
        expect(&r, "issuer", Pass, "issuer-ok", Some("issuer"));
    }

    #[tokio::test]
    async fn jwks_of_another_realm_fails_and_empty_jwks_url_is_filled() {
        let server = MockServer::start().await;
        let good = realm(&server, "good", "k", None).await;
        let other = realm(&server, "other", "other-kid", None).await;
        let probe = EndpointAuthProbe::new(loopback());
        let r = probe
            .verify(
                &full(&good, Some(format!("{other}/certs")), Some("a")),
                None,
            )
            .await;
        assert!(!r.ok);
        expect(&r, "jwks-url", Warn, "jwks-url-differs", Some("jwksUrl"));
        assert_eq!(r.suggestions["jwksUrl"], format!("{good}/certs"));
        expect(&r, "jwks", Fail, "jwks-foreign-keys", Some("jwksUrl"));
        expect(&r, "algorithms", Skipped, "skipped-after-failure", None);

        let r = probe.verify(&full(&good, None, Some("a")), None).await;
        assert!(r.ok, "{r:#?}");
        expect(
            &r,
            "jwks-url",
            Pass,
            "jwks-url-from-discovery",
            Some("jwksUrl"),
        );
        assert_eq!(r.suggestions["jwksUrl"], format!("{good}/certs"));
        expect(&r, "jwks", Pass, "jwks-ok", Some("jwksUrl"));
    }

    #[tokio::test]
    async fn jwks_problems_have_distinct_codes() {
        let server = MockServer::start().await;
        let good = realm(&server, "good", "k", None).await;
        Mock::given(method("GET"))
            .and(path("/empty"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"keys": []})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/html"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html")
                    .set_body_string("<html></html>"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/nokid"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"keys": [{"kty": "RSA", "n": N, "e": "AQAB"}]}),
                ),
            )
            .mount(&server)
            .await;
        let probe = EndpointAuthProbe::new(loopback());
        for (p, code) in [
            ("/empty", "jwks-empty"),
            ("/html", "jwks-not-json"),
            ("/missing", "jwks-404"),
            ("/nokid", "jwks-no-usable-key"),
        ] {
            let r = probe
                .verify(
                    &full(&good, Some(format!("{}{p}", server.uri())), Some("a")),
                    None,
                )
                .await;
            expect(&r, "jwks", Fail, code, Some("jwksUrl"));
        }
        let mut auth = full(&good, None, Some("a"));
        auth.algorithms = vec!["ES256".into()];
        let r = probe.verify(&auth, None).await;
        expect(
            &r,
            "algorithms",
            Fail,
            "algorithms-no-match",
            Some("algorithms"),
        );
        assert_eq!(step(&r, "algorithms").params["keys"], "RS256");
        auth.algorithms = vec!["PS256".into()];
        let r = probe.verify(&auth, None).await;
        expect(
            &r,
            "algorithms",
            Fail,
            "algorithms-unsupported",
            Some("algorithms"),
        );
    }

    #[tokio::test]
    async fn not_a_url_is_an_input_failure_without_debug_output() {
        let probe = EndpointAuthProbe::new(loopback());
        let mut auth = full("idp.example.org", Some("keys".into()), None);
        let r = probe.verify(&auth, None).await;
        assert!(!r.ok);
        expect(&r, "input", Fail, "input-not-url", Some("issuer"));
        assert_eq!(step(&r, "input").params["value"], "idp.example.org");
        for id in [
            "outbound",
            "connect",
            "discovery",
            "issuer",
            "jwks-url",
            "jwks",
            "algorithms",
            "smart",
        ] {
            expect(&r, id, Skipped, "skipped-after-failure", None);
        }
        expect(&r, "audience", Warn, "audience-missing", Some("audience"));
        auth.issuer = Some("https://idp.example.org".into());
        let r = probe.verify(&auth, None).await;
        expect(&r, "input", Fail, "input-not-url", Some("jwksUrl"));
        auth.issuer = None;
        let r = probe.verify(&auth, None).await;
        expect(&r, "input", Fail, "input-required", Some("issuer"));
    }

    #[tokio::test]
    async fn connection_failures_are_classified() {
        // Refused: nothing listens on port 1.
        let r = EndpointAuthProbe::new(loopback())
            .verify(&full("http://127.0.0.1:1/realms/x", None, Some("a")), None)
            .await;
        expect(&r, "connect", Fail, "connect-refused", Some("issuer"));
        expect(&r, "discovery", Skipped, "skipped-after-failure", None);
        assert_eq!(step(&r, "discovery").params["after"], "connect");
        expect(&r, "jwks", Skipped, "skipped-after-failure", None);

        // DNS: a reserved name that never resolves.
        let r = EndpointAuthProbe::new(OutboundGuard::Trusted)
            .verify(
                &full("https://no-such-host.invalid/realms/x", None, Some("a")),
                None,
            )
            .await;
        expect(&r, "connect", Fail, "connect-dns", Some("issuer"));

        // Timeout.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
            .mount(&server)
            .await;
        let r = EndpointAuthProbe::new(loopback())
            .with_timeout(Duration::from_millis(300))
            .verify(
                &full(&format!("{}/slow", server.uri()), None, Some("a")),
                None,
            )
            .await;
        expect(&r, "connect", Fail, "connect-timeout", Some("issuer"));

        // TLS: https to a plain-http port.
        let r = EndpointAuthProbe::new(loopback())
            .verify(
                &full(
                    &server.uri().replace("http://", "https://"),
                    None,
                    Some("a"),
                ),
                None,
            )
            .await;
        expect(&r, "connect", Fail, "connect-tls", Some("issuer"));
    }

    #[tokio::test]
    async fn outbound_policy_refusals_and_plain_http() {
        let server = MockServer::start().await;
        let issuer = realm(&server, "good", "k", None).await;
        let strict = OutboundGuard::Policy(Arc::new(OutboundPolicy::strict()));
        let r = EndpointAuthProbe::new(strict.clone())
            .verify(&full(&issuer, None, Some("a")), None)
            .await;
        expect(&r, "outbound", Fail, "outbound-plain-http", Some("issuer"));
        assert_eq!(step(&r, "outbound").params["host"], "127.0.0.1");
        expect(&r, "connect", Skipped, "skipped-after-failure", None);
        let https = issuer.replace("http://", "https://");
        let r = EndpointAuthProbe::new(strict.clone())
            .verify(&full(&https, None, Some("a")), None)
            .await;
        expect(&r, "outbound", Fail, "outbound-blocked", Some("issuer"));
        // A name resolving to loopback is refused before connecting.
        let by_name = https.replace("127.0.0.1", "localhost");
        let r = EndpointAuthProbe::new(strict)
            .verify(&full(&by_name, None, Some("a")), None)
            .await;
        expect(&r, "outbound", Fail, "outbound-blocked", Some("issuer"));
        assert!(server.received_requests().await.unwrap().is_empty());

        // Plain http under the trusted guard: a warning, the rest passes.
        let r = EndpointAuthProbe::new(OutboundGuard::Trusted)
            .verify(&full(&issuer, None, Some("a")), None)
            .await;
        assert!(r.ok, "{r:#?}");
        expect(&r, "input", Warn, "input-plain-http", Some("issuer"));
        expect(&r, "outbound", Pass, "outbound-trusted", None);
        // Allow-listed: no plain-http warning.
        let r = EndpointAuthProbe::new(loopback())
            .verify(&full(&issuer, None, Some("a")), None)
            .await;
        expect(&r, "input", Pass, "input-ok", None);
    }

    #[tokio::test]
    async fn missing_audience_and_smart_gaps_warn() {
        let server = MockServer::start().await;
        let issuer = format!("{}/realms/bare", server.uri());
        Mock::given(method("GET"))
            .and(path("/realms/bare/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/auth"),
                "token_endpoint": format!("{issuer}/token"),
                "jwks_uri": format!("{issuer}/certs"),
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/realms/bare/certs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "keys": [{"kty": "RSA", "kid": "k", "n": N, "e": "AQAB"}]
            })))
            .mount(&server)
            .await;
        let r = EndpointAuthProbe::new(loopback())
            .verify(&full(&issuer, None, None), None)
            .await;
        assert!(r.ok, "warnings do not fail: {r:#?}");
        expect(&r, "audience", Warn, "audience-missing", Some("audience"));
        expect(&r, "smart", Warn, "smart-no-pkce", None);
        assert_eq!(r.warnings(), 2);
    }

    #[tokio::test]
    async fn tokens_are_mapped_to_specific_codes() {
        let server = MockServer::start().await;
        let issuer = realm(&server, "good", "k", None).await;
        let auth = full(&issuer, None, Some("hfs-uxa"));
        let probe = EndpointAuthProbe::new(loopback());

        let r = probe
            .verify(&auth, Some(&sign(claims(&issuer, "hfs-uxa", 300))))
            .await;
        assert!(r.ok, "{r:#?}");
        expect(&r, "token", Pass, "token-ok", Some("token"));
        assert_eq!(step(&r, "token").params["sub"], "app");
        assert_eq!(step(&r, "token").params["scopes"], "system/*.rs");
        assert_eq!(r.steps.last().unwrap().id, "token");

        let r = probe
            .verify(
                &auth,
                Some(&format!(
                    "Bearer {}",
                    sign(claims(&issuer, "other-api", 300))
                )),
            )
            .await;
        expect(&r, "token", Fail, "token-audience", Some("audience"));
        assert_eq!(step(&r, "token").params["got"], "other-api");
        assert_eq!(step(&r, "token").params["expected"], "hfs-uxa");

        let r = probe
            .verify(&auth, Some(&sign(claims(&issuer, "hfs-uxa", -3600))))
            .await;
        expect(&r, "token", Fail, "token-expired", Some("token"));
        assert!(step(&r, "token").params["expired_at"].ends_with('Z'));

        let good = sign(claims(&issuer, "hfs-uxa", 300));
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some("k".into());
        let other_key = jsonwebtoken::EncodingKey::from_rsa_pem(include_bytes!(
            "../tests/data/test_rsa_key_2.pem"
        ))
        .unwrap();
        let forged =
            jsonwebtoken::encode(&header, &claims(&issuer, "hfs-uxa", 300), &other_key).unwrap();
        let r = probe.verify(&auth, Some(&forged)).await;
        expect(&r, "token", Fail, "token-signature", Some("token"));

        let r = probe
            .verify(
                &auth,
                Some(&sign(claims("https://elsewhere", "hfs-uxa", 300))),
            )
            .await;
        expect(&r, "token", Fail, "token-issuer", Some("issuer"));
        assert_eq!(step(&r, "token").params["got"], "https://elsewhere");

        let r = probe.verify(&auth, Some("not-a-jwt")).await;
        expect(&r, "token", Fail, "token-malformed", Some("token"));

        // A token while auth is off is skipped.
        let off = EndpointAuth::default();
        let r = probe.verify(&off, Some("x.y.z")).await;
        assert!(r.ok);
        expect(&r, "mode", Pass, "mode-off", Some("mode"));
        expect(&r, "token", Skipped, "token-mode-off", Some("token"));

        // JWKS broken: the token is skipped.
        let broken = full(
            &issuer,
            Some(format!("{}/nope", server.uri())),
            Some("hfs-uxa"),
        );
        let r = probe.verify(&broken, Some(&good)).await;
        expect(&r, "token", Skipped, "skipped-after-failure", None);
        assert_eq!(step(&r, "token").params["after"], "jwks");
    }

    // ------------------------------------------------------- terminology

    async fn tx_server(lookup: bool) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/tx/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "resourceType": "CapabilityStatement",
                "fhirVersion": "4.0.1",
                "software": {"name": "HTS", "version": "1.2"}
            })))
            .mount(&server)
            .await;
        if lookup {
            Mock::given(method("GET"))
                .and(path("/tx/CodeSystem/$lookup"))
                .and(query_param("code", "8867-4"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "resourceType": "Parameters",
                    "parameter": [{"name": "display", "valueString": "Heart rate"}]
                })))
                .mount(&server)
                .await;
        } else {
            Mock::given(method("GET"))
                .and(path("/tx/CodeSystem/$lookup"))
                .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                    "resourceType": "OperationOutcome"
                })))
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/tx/ValueSet/$expand"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "resourceType": "ValueSet",
                "expansion": {"total": 4, "contains": [{}, {}, {}, {}]}
            })))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn terminology_server_checks() {
        let server = tx_server(true).await;
        let url = format!("{}/tx", server.uri());
        let r = verify_terminology_server(&url, None, Duration::from_secs(10), &loopback()).await;
        assert!(r.ok, "{r:#?}");
        assert_eq!(r.kind, "terminology");
        expect(&r, "metadata", Pass, "metadata-ok", Some("terminologyUrl"));
        assert_eq!(step(&r, "metadata").params["software"], "HTS");
        assert_eq!(step(&r, "metadata").params["fhir"], "4.0.1");
        expect(&r, "lookup", Pass, "lookup-ok", None);
        assert_eq!(step(&r, "lookup").params["display"], "Heart rate");
        expect(&r, "expand", Pass, "expand-ok", None);
        assert_eq!(step(&r, "expand").params["count"], "4");
        expect(&r, "latency", Pass, "latency-ok", None);
        assert!(r.steps.iter().all(|s| s.id != "token"));

        let missing = tx_server(false).await;
        let r = verify_terminology_server(
            &format!("{}/tx", missing.uri()),
            Some("tok"),
            Duration::from_secs(10),
            &loopback(),
        )
        .await;
        assert!(r.ok);
        expect(&r, "lookup", Warn, "lookup-missing-system", None);
        assert_eq!(step(&r, "lookup").params["system"], "http://loinc.org");
        expect(
            &r,
            "token",
            Skipped,
            "token-not-checkable",
            Some("terminologyToken"),
        );

        let r = verify_terminology_server(
            &format!("{}/nothing", server.uri()),
            None,
            Duration::from_secs(10),
            &loopback(),
        )
        .await;
        expect(&r, "metadata", Fail, "metadata-404", Some("terminologyUrl"));
        expect(&r, "lookup", Skipped, "skipped-after-failure", None);

        let r =
            verify_terminology_server("not a url", None, Duration::from_secs(5), &loopback()).await;
        expect(&r, "input", Fail, "input-not-url", Some("terminologyUrl"));
        expect(&r, "latency", Skipped, "skipped-after-failure", None);

        let r = verify_terminology_server(
            "http://127.0.0.1:1/tx",
            None,
            Duration::from_secs(5),
            &loopback(),
        )
        .await;
        expect(
            &r,
            "connect",
            Fail,
            "connect-refused",
            Some("terminologyUrl"),
        );
    }

    #[tokio::test]
    async fn terminology_token_and_latency() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/metadata"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(700))
                    .set_body_json(serde_json::json!({"resourceType": "CapabilityStatement", "fhirVersion": "4.0.1"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/CodeSystem"))
            .and(header("authorization", "Bearer good"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"resourceType": "Bundle", "total": 3})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/CodeSystem"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let r = verify_terminology_server(
            &server.uri(),
            Some("good"),
            Duration::from_secs(1),
            &loopback(),
        )
        .await;
        expect(
            &r,
            "token",
            Pass,
            "token-accepted",
            Some("terminologyToken"),
        );
        expect(
            &r,
            "latency",
            Warn,
            "latency-high",
            Some("terminologyTimeout"),
        );
        expect(&r, "lookup", Warn, "lookup-missing-system", None);
        assert!(
            step(&r, "lookup")
                .url
                .as_deref()
                .unwrap()
                .contains("code=8867-4")
        );
        expect(&r, "expand", Warn, "expand-missing", None);
        let r = verify_terminology_server(
            &server.uri(),
            Some("bad"),
            Duration::from_secs(5),
            &loopback(),
        )
        .await;
        expect(&r, "token", Fail, "token-refused", Some("terminologyToken"));
    }

    // ------------------------------------------------------- sync peers

    async fn fhir_peer(fhir: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/fhir/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "resourceType": "CapabilityStatement",
                "fhirVersion": fhir,
                "software": {"name": "Peer", "version": "9"},
                "rest": [{"mode": "server", "resource": [
                    {"type": "Patient", "interaction": [{"code": "read"}, {"code": "update"}, {"code": "history-type"}]},
                    {"type": "Observation", "interaction": [{"code": "read"}, {"code": "search-type"}]}
                ]}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(header("authorization", "Basic YXBwOnNlY3JldA=="))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"access_token": "AT", "expires_in": 300})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(401).set_body_json(
                serde_json::json!({"error": "invalid_client", "error_description": "bad secret"}),
            ))
            .mount(&server)
            .await;
        for (t, n) in [("Patient", 1204), ("Observation", 98311)] {
            Mock::given(method("GET"))
                .and(path(format!("/fhir/{t}")))
                .and(query_param("_summary", "count"))
                .and(header("authorization", "Bearer AT"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"resourceType": "Bundle", "total": n})),
                )
                .mount(&server)
                .await;
        }
        server
    }

    fn cc(server: &MockServer, secret: &str) -> PeerConfig {
        PeerConfig {
            url: format!("{}/fhir", server.uri()),
            auth: PeerAuth::ClientCredentials {
                token_url: format!("{}/token", server.uri()),
                client_id: "app".into(),
                client_secret: secret.into(),
                scope: Some("system/*.rs".into()),
            },
        }
    }

    #[tokio::test]
    async fn sync_peer_checks() {
        let server = fhir_peer("4.0.1").await;
        let types = vec!["Patient".to_string(), "Observation".to_string()];
        let r = verify_fhir_peer(
            &cc(&server, "secret"),
            PeerRole::Source,
            Some("R4"),
            &types,
            &loopback(),
        )
        .await;
        assert!(r.ok, "{r:#?}");
        assert_eq!(r.kind, "sync-source");
        let ids: Vec<&str> = r.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "input",
                "outbound",
                "connect",
                "metadata",
                "token",
                "counts",
                "incremental"
            ]
        );
        expect(&r, "token", Pass, "token-issued", Some("clientId"));
        expect(&r, "counts", Pass, "counts-ok", None);
        assert_eq!(
            step(&r, "counts").params["summary"],
            "Patient 1,204 · Observation 98,311"
        );
        expect(&r, "incremental", Warn, "incremental-unsupported", None);
        assert_eq!(step(&r, "incremental").params["types"], "Observation");

        let r = verify_fhir_peer(
            &cc(&server, "wrong"),
            PeerRole::Target,
            None,
            &types,
            &loopback(),
        )
        .await;
        assert!(!r.ok);
        expect(&r, "token", Fail, "token-invalid-client", Some("clientId"));
        expect(&r, "counts", Skipped, "skipped-after-failure", None);
        expect(
            &r,
            "write",
            Warn,
            "write-not-advertised",
            Some("resourceTypes"),
        );
        assert_eq!(step(&r, "write").params["types"], "Observation");

        let r = verify_fhir_peer(
            &cc(&server, "secret"),
            PeerRole::Target,
            Some("R5"),
            &types,
            &loopback(),
        )
        .await;
        expect(
            &r,
            "metadata",
            Fail,
            "metadata-version-mismatch",
            Some("url"),
        );
        assert_eq!(step(&r, "metadata").params["expected"], "R5");

        let r = verify_fhir_peer(
            &cc(&server, "secret"),
            PeerRole::Source,
            None,
            &["Patient".to_string(), "Basic".to_string()],
            &loopback(),
        )
        .await;
        expect(
            &r,
            "counts",
            Warn,
            "counts-unknown-type",
            Some("resourceTypes"),
        );
        assert_eq!(step(&r, "counts").params["type"], "Basic");

        let none = PeerConfig {
            url: format!("{}/fhir", server.uri()),
            auth: PeerAuth::None,
        };
        let r = verify_fhir_peer(
            &none,
            PeerRole::Source,
            None,
            &["Patient".into()],
            &loopback(),
        )
        .await;
        // Counts without the token are not mocked (404 → unknown); metadata is open.
        expect(&r, "token", Pass, "token-not-needed", Some("authType"));

        let bad = PeerConfig {
            url: "fhir.example.org".into(),
            auth: PeerAuth::None,
        };
        let r = verify_fhir_peer(&bad, PeerRole::Target, None, &[], &loopback()).await;
        expect(&r, "input", Fail, "input-not-url", Some("url"));
        expect(&r, "write", Skipped, "skipped-after-failure", None);

        let unreachable = PeerConfig {
            url: "http://127.0.0.1:1/fhir".into(),
            auth: PeerAuth::ClientCredentials {
                token_url: "http://127.0.0.1:1/token".into(),
                client_id: "app".into(),
                client_secret: "s".into(),
                scope: None,
            },
        };
        let r = verify_fhir_peer(&unreachable, PeerRole::Source, None, &[], &loopback()).await;
        expect(&r, "connect", Fail, "connect-refused", Some("url"));
        expect(
            &r,
            "token",
            Fail,
            "token-endpoint-unreachable",
            Some("tokenUrl"),
        );
    }

    #[test]
    fn verify_futures_are_send() {
        fn send<T: Send>(_: T) {}
        let guard = OutboundGuard::Trusted;
        let auth = EndpointAuth::default();
        let probe = EndpointAuthProbe::new(guard.clone());
        send(probe.verify(&auth, None));
        send(verify_terminology_server(
            "",
            None,
            Duration::from_secs(1),
            &guard,
        ));
        let peer = PeerConfig {
            url: String::new(),
            auth: PeerAuth::None,
        };
        send(verify_fhir_peer(&peer, PeerRole::Source, None, &[], &guard));
    }

    #[test]
    fn sync_request_shape_and_redacted_debug() {
        let req: SyncPeerVerifyRequest = serde_json::from_value(serde_json::json!({
            "role": "target", "url": "https://fhir.example.org",
            "auth": {"type": "client-credentials", "tokenUrl": "https://idp/token", "clientId": "c", "clientSecret": "s3cr3t"},
            "resourceTypes": ["Patient"], "fhirVersion": "R4"
        }))
        .unwrap();
        assert_eq!(req.role, PeerRole::Target);
        assert!(!format!("{req:?}").contains("s3cr3t"));
        let bearer: PeerAuth =
            serde_json::from_value(serde_json::json!({"type": "bearer", "token": "t"})).unwrap();
        assert!(matches!(bearer, PeerAuth::Bearer { .. }));
        assert_eq!(grouped(98311), "98,311");
        assert_eq!(grouped(12), "12");
        assert_eq!(release_of("4.0.1"), Some("R4"));
        assert_eq!(release_of("R4B"), Some("R4B"));
    }
}
