//! SMART discovery documents and, for endpoints configured at runtime
//! (FHIR server manager, `docs/design/fhir-server-manager.md` §6), the
//! outbound-policy-guarded HTTP access to administrator-entered IdP URLs:
//! [`OutboundGuard`], the cached issuer discovery ([`IssuerDiscovery`]) and
//! the `$test-auth` probe ([`EndpointAuthProbe`]).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use helios_endpoints::OutboundPolicy;
use helios_endpoints::model::{AuthMode as EndpointAuthMode, EndpointAuth};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{AuthConfig, redact_url};

/// SMART on FHIR configuration document served at
/// `/.well-known/smart-configuration`.
#[derive(Debug, Serialize)]
pub struct SmartConfiguration {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwks_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub introspection_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub management_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revocation_endpoint: Option<String>,
    /// RP-initiated logout; only set for endpoints configured in the manager
    /// (the environment-configured document never carried it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_session_endpoint: Option<String>,
    pub scopes_supported: Vec<String>,
    pub response_types_supported: Vec<String>,
    pub grant_types_supported: Vec<String>,
    pub token_endpoint_auth_methods_supported: Vec<String>,
    pub code_challenge_methods_supported: Vec<String>,
    pub token_endpoint_auth_signing_alg_values_supported: Vec<String>,
    pub capabilities: Vec<String>,
}

/// `scopes_supported` advertised when `HFS_SMART_SCOPES_SUPPORTED` is unset:
/// the scope shapes HFS enforces (SMART v1 and v2, patient/user/system).
pub const DEFAULT_SCOPES_SUPPORTED: &[&str] = &[
    "openid",
    "fhirUser",
    "profile",
    "launch/patient",
    "offline_access",
    "online_access",
    "patient/*.rs",
    "patient/*.read",
    "user/*.cruds",
    "user/*.rs",
    "user/*.read",
    "user/*.write",
    "system/*.cruds",
    "system/*.rs",
    "system/*.read",
];

impl SmartConfiguration {
    /// Build the SMART configuration document from `AuthConfig`.
    ///
    /// Every list can be overridden from the environment (`HFS_SMART_*`), so
    /// the document describes what the IdP in front of this server actually
    /// offers; the defaults describe a Keycloak-style IdP with public,
    /// client-secret and `private_key_jwt` clients.
    pub fn from_config(config: &AuthConfig) -> Self {
        let interactive = config.smart_authorize_endpoint.is_some();
        let owned = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        let response_types_supported = if interactive {
            owned(&["code"])
        } else {
            Vec::new()
        };
        let grant_types_supported = config.smart_grant_types.clone().unwrap_or_else(|| {
            if interactive {
                owned(&["authorization_code", "client_credentials", "refresh_token"])
            } else {
                owned(&["client_credentials"])
            }
        });
        let capabilities = config.smart_capabilities.clone().unwrap_or_else(|| {
            let mut caps = Vec::new();
            if interactive {
                caps.extend(owned(&[
                    "launch-standalone",
                    "context-standalone-patient",
                    "client-public",
                    "client-confidential-symmetric",
                    "sso-openid-connect",
                    "permission-offline",
                    "permission-online",
                    "permission-patient",
                    "permission-user",
                ]));
            }
            caps.extend(owned(&[
                "client-confidential-asymmetric",
                "permission-v1",
                "permission-v2",
            ]));
            caps
        });

        Self {
            issuer: config.expected_issuer.clone(),
            jwks_uri: config
                .smart_jwks_url
                .clone()
                .or_else(|| config.jwks_url.clone()),
            authorization_endpoint: config.smart_authorize_endpoint.clone(),
            token_endpoint: config.smart_token_endpoint.clone(),
            introspection_endpoint: config.smart_introspection_endpoint.clone(),
            management_endpoint: config.smart_management_endpoint.clone(),
            registration_endpoint: config.smart_registration_endpoint.clone(),
            revocation_endpoint: config.smart_revocation_endpoint.clone(),
            end_session_endpoint: None,
            scopes_supported: config
                .smart_scopes_supported
                .clone()
                .unwrap_or_else(|| owned(DEFAULT_SCOPES_SUPPORTED)),
            response_types_supported,
            grant_types_supported,
            token_endpoint_auth_methods_supported: config
                .smart_token_auth_methods
                .clone()
                .unwrap_or_else(|| {
                    owned(&[
                        "client_secret_basic",
                        "client_secret_post",
                        "private_key_jwt",
                    ])
                }),
            code_challenge_methods_supported: owned(&["S256"]),
            token_endpoint_auth_signing_alg_values_supported: owned(&["RS384", "ES384"]),
            capabilities,
        }
    }

    /// Serialize to a JSON `Value`.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("SmartConfiguration serialization cannot fail")
    }
}

impl SmartConfiguration {
    /// The document of an endpoint configured in the manager: `config` is
    /// the endpoint's [`AuthConfig::from_endpoint`]; endpoints it leaves
    /// unset are filled from the issuer's OpenID configuration when one was
    /// discovered.
    pub fn for_endpoint(
        config: &AuthConfig,
        discovered: Option<&OidcMetadata>,
        end_session_endpoint: Option<&str>,
    ) -> Self {
        let mut filled = config.clone();
        if let Some(meta) = discovered {
            fill(
                &mut filled.smart_authorize_endpoint,
                &meta.authorization_endpoint,
            );
            fill(&mut filled.smart_token_endpoint, &meta.token_endpoint);
            fill(
                &mut filled.smart_introspection_endpoint,
                &meta.introspection_endpoint,
            );
            fill(
                &mut filled.smart_revocation_endpoint,
                &meta.revocation_endpoint,
            );
            fill(
                &mut filled.smart_registration_endpoint,
                &meta.registration_endpoint,
            );
            fill(&mut filled.smart_jwks_url, &meta.jwks_uri);
        }
        let mut doc = Self::from_config(&filled);
        doc.end_session_endpoint = end_session_endpoint
            .map(str::to_string)
            .or_else(|| discovered.and_then(|m| m.end_session_endpoint.clone()));
        doc
    }
}

fn fill(slot: &mut Option<String>, value: &Option<String>) {
    if slot.is_none() {
        slot.clone_from(value);
    }
}

// ------------------------------------------------------------ outbound guard

/// How URLs of one endpoint's auth configuration may be fetched.
///
/// Endpoints declared by the operator (`HFS_ENDPOINTS_FILE`, `managedBy=env`)
/// and the environment configuration are trusted like `HFS_AUTH_JWKS_URL`
/// today; endpoints edited in the console go through the SSRF policy
/// ([`OutboundPolicy`], `HFS_OUTBOUND_ALLOW`): the URL is checked before
/// every request and the client's DNS resolver refuses internal addresses,
/// so the connection always goes to a checked address (no DNS rebinding);
/// redirects are not followed.
#[derive(Debug, Clone)]
pub enum OutboundGuard {
    /// Operator-configured URLs: no policy.
    Trusted,
    /// Administrator-entered URLs: the policy applies.
    Policy(Arc<OutboundPolicy>),
}

impl OutboundGuard {
    /// Syntax check of `url` (scheme, user-info, metadata hosts, literal
    /// internal addresses). Always `Ok` for [`OutboundGuard::Trusted`].
    pub fn check_url(&self, url: &str) -> Result<(), String> {
        match self {
            OutboundGuard::Trusted => Ok(()),
            OutboundGuard::Policy(policy) => policy
                .check_url(url)
                .map(|_| ())
                .map_err(|e| format!("{} refused by the outbound policy: {e}", redact_url(url))),
        }
    }

    /// An HTTP client for this guard: the policy's DNS resolver and no
    /// redirects under [`OutboundGuard::Policy`].
    pub fn client(&self, timeout: Duration) -> reqwest::Client {
        let builder = reqwest::Client::builder().timeout(timeout);
        let builder = match self {
            OutboundGuard::Trusted => builder,
            OutboundGuard::Policy(policy) => builder
                .redirect(reqwest::redirect::Policy::none())
                .dns_resolver(Arc::new(PolicyResolver {
                    policy: Arc::clone(policy),
                })),
        };
        builder.build().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "building the outbound HTTP client failed; using defaults");
            reqwest::Client::new()
        })
    }
}

/// DNS resolver that drops every address the outbound policy refuses (and
/// fails when none is left), so a connection can only go to a checked
/// address.
struct PolicyResolver {
    policy: Arc<OutboundPolicy>,
}

impl reqwest::dns::Resolve for PolicyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let policy = Arc::clone(&self.policy);
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let mut refused = None;
            let allowed: Vec<SocketAddr> = addrs
                .into_iter()
                .filter(|a| match policy.check_ip(&host, a.ip()) {
                    Ok(()) => true,
                    Err(e) => {
                        refused = Some(e);
                        false
                    }
                })
                .collect();
            if allowed.is_empty() {
                let why = match refused {
                    Some(e) => format!("{POLICY_RESOLVER_MARK} {e}"),
                    None => format!("{host} did not resolve"),
                };
                return Err(why.into());
            }
            let addrs: reqwest::dns::Addrs = Box::new(allowed.into_iter());
            Ok(addrs)
        })
    }
}

// ---------------------------------------------------------- issuer discovery

/// The parts of an OpenID Provider configuration
/// (`{issuer}/.well-known/openid-configuration`) SMART discovery and the
/// configuration verifier ([`crate::verify`]) use.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct OidcMetadata {
    /// `issuer` as the IdP states it.
    #[serde(default)]
    pub issuer: Option<String>,
    /// `authorization_endpoint`.
    #[serde(default)]
    pub authorization_endpoint: Option<String>,
    /// `token_endpoint`.
    #[serde(default)]
    pub token_endpoint: Option<String>,
    /// `jwks_uri`.
    #[serde(default)]
    pub jwks_uri: Option<String>,
    /// `introspection_endpoint`.
    #[serde(default)]
    pub introspection_endpoint: Option<String>,
    /// `revocation_endpoint`.
    #[serde(default)]
    pub revocation_endpoint: Option<String>,
    /// `registration_endpoint`.
    #[serde(default)]
    pub registration_endpoint: Option<String>,
    /// `end_session_endpoint`.
    #[serde(default)]
    pub end_session_endpoint: Option<String>,
    /// `grant_types_supported` (empty when not stated).
    #[serde(default)]
    pub grant_types_supported: Vec<String>,
    /// `code_challenge_methods_supported` (empty when not stated).
    #[serde(default)]
    pub code_challenge_methods_supported: Vec<String>,
    /// `id_token_signing_alg_values_supported` (empty when not stated).
    #[serde(default)]
    pub id_token_signing_alg_values_supported: Vec<String>,
}

/// The discovery document URL of an issuer.
pub fn openid_configuration_url(issuer: &str) -> String {
    format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    )
}

/// Why an outbound fetch failed, classified so a caller can tell the
/// operator what to fix (DNS, refused connection, timeout, TLS, a web page
/// where JSON was expected, …). Every `url` is redacted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// The outbound policy refused the URL or every address of its host.
    #[error("{url} refused by the outbound policy: {reason}")]
    Blocked {
        /// The URL.
        url: String,
        /// The policy's reason.
        reason: String,
    },
    /// The host name did not resolve.
    #[error("fetching {url}: the host name did not resolve ({reason})")]
    Dns {
        /// The URL.
        url: String,
        /// Resolver error.
        reason: String,
    },
    /// Nothing listens at the address.
    #[error("fetching {url}: connection refused")]
    Refused {
        /// The URL.
        url: String,
    },
    /// No answer in time.
    #[error("fetching {url}: timed out")]
    Timeout {
        /// The URL.
        url: String,
    },
    /// The TLS handshake failed (certificate, protocol).
    #[error("fetching {url}: TLS handshake failed ({reason})")]
    Tls {
        /// The URL.
        url: String,
        /// Handshake error.
        reason: String,
    },
    /// Another transport error.
    #[error("fetching {url}: {reason}")]
    Connect {
        /// The URL.
        url: String,
        /// The error.
        reason: String,
    },
    /// A non-success HTTP status.
    #[error("{url} answered HTTP {status}")]
    Status {
        /// The URL.
        url: String,
        /// The status code.
        status: u16,
    },
    /// An HTML page where JSON was expected.
    #[error("{url} answered with a web page, not JSON")]
    Html {
        /// The URL.
        url: String,
    },
    /// The body is not the JSON document expected.
    #[error("{url} is not valid JSON of the expected shape: {reason}")]
    NotJson {
        /// The URL.
        url: String,
        /// Parser error.
        reason: String,
    },
    /// The body exceeds 1 MiB.
    #[error("{url} is larger than 1 MiB")]
    TooLarge {
        /// The URL.
        url: String,
    },
}

impl FetchError {
    /// Whether the request never got an HTTP answer (policy, DNS,
    /// connection, timeout, TLS).
    pub fn is_transport(&self) -> bool {
        matches!(
            self,
            FetchError::Blocked { .. }
                | FetchError::Dns { .. }
                | FetchError::Refused { .. }
                | FetchError::Timeout { .. }
                | FetchError::Tls { .. }
                | FetchError::Connect { .. }
        )
    }

    /// Classifies a reqwest transport error for `url`.
    pub fn from_reqwest(url: &str, e: &reqwest::Error) -> Self {
        let url = redact_url(url);
        if e.is_timeout() {
            return FetchError::Timeout { url };
        }
        let mut texts = Vec::new();
        let mut refused = false;
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(e);
        while let Some(err) = source {
            if let Some(io) = err.downcast_ref::<std::io::Error>() {
                match io.kind() {
                    std::io::ErrorKind::ConnectionRefused => refused = true,
                    std::io::ErrorKind::TimedOut => return FetchError::Timeout { url },
                    _ => {}
                }
            }
            texts.push(err.to_string());
            source = err.source();
        }
        let reason = texts.last().cloned().unwrap_or_default();
        let all = texts.join(" | ").to_ascii_lowercase();
        if all.contains(POLICY_RESOLVER_MARK) {
            let reason = texts
                .iter()
                .find(|t| t.contains(POLICY_RESOLVER_MARK))
                .map(|t| t.replace(POLICY_RESOLVER_MARK, "").trim().to_string())
                .unwrap_or_default();
            return FetchError::Blocked { url, reason };
        }
        if refused || all.contains("connection refused") {
            return FetchError::Refused { url };
        }
        if all.contains("dns error")
            || all.contains("failed to lookup address")
            || all.contains("name or service not known")
            || all.contains("no such host")
            || all.contains("nodename nor servname")
            || all.contains("did not resolve")
        {
            return FetchError::Dns { url, reason };
        }
        if all.contains("certificate")
            || all.contains("tls")
            || all.contains("handshake")
            || all.contains("rustls")
            || all.contains("corrupt message")
            || all.contains("invalidcontenttype")
        {
            return FetchError::Tls { url, reason };
        }
        if all.contains("timed out") {
            return FetchError::Timeout { url };
        }
        FetchError::Connect { url, reason }
    }
}

/// Marker the policy resolver puts in its errors, so a refusal at
/// connection time is reported as the policy's, not as a DNS failure.
pub(crate) const POLICY_RESOLVER_MARK: &str = "[outbound-policy]";

/// One HTTP answer, body read (at most 1 MiB).
#[derive(Debug, Clone)]
pub(crate) struct RawResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    /// Time until the response headers arrived.
    pub headers_ms: u64,
    /// Time until the body was read.
    pub total_ms: u64,
}

impl RawResponse {
    /// Whether the body looks like an HTML page.
    pub fn is_html(&self) -> bool {
        if self.content_type.contains("text/html") {
            return true;
        }
        let head =
            String::from_utf8_lossy(&self.body[..self.body.len().min(64)]).to_ascii_lowercase();
        let head = head.trim_start();
        head.starts_with("<!doctype html") || head.starts_with("<html")
    }

    /// The body as `T`: `Status` for non-2xx answers, `Html` for a web page,
    /// `NotJson` otherwise.
    pub fn json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, FetchError> {
        if !(200..300).contains(&self.status) {
            return Err(FetchError::Status {
                url: redact_url(url),
                status: self.status,
            });
        }
        if self.is_html() {
            return Err(FetchError::Html {
                url: redact_url(url),
            });
        }
        serde_json::from_slice(&self.body).map_err(|e| FetchError::NotJson {
            url: redact_url(url),
            reason: e.to_string(),
        })
    }
}

/// Sends `request` (built on `url`) and reads the answer; transport errors
/// are classified ([`FetchError::from_reqwest`]).
pub(crate) async fn send_raw(
    url: &str,
    request: reqwest::RequestBuilder,
) -> Result<RawResponse, FetchError> {
    let started = Instant::now();
    let response = request
        .send()
        .await
        .map_err(|e| FetchError::from_reqwest(url, &e))?;
    let headers_ms = started.elapsed().as_millis() as u64;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let body = response
        .bytes()
        .await
        .map_err(|e| FetchError::from_reqwest(url, &e))?;
    if body.len() > 1024 * 1024 {
        return Err(FetchError::TooLarge {
            url: redact_url(url),
        });
    }
    Ok(RawResponse {
        status,
        content_type,
        body: body.to_vec(),
        headers_ms,
        total_ms: started.elapsed().as_millis() as u64,
    })
}

/// GETs `url` (JSON accepted) through `guard`: the policy's syntax check,
/// then the guarded client.
pub(crate) async fn get_raw(
    url: &str,
    guard: &OutboundGuard,
    timeout: Duration,
    bearer: Option<&str>,
) -> Result<RawResponse, FetchError> {
    guard.check_url(url).map_err(|reason| FetchError::Blocked {
        url: redact_url(url),
        reason,
    })?;
    let mut request = guard.client(timeout).get(url).header(
        reqwest::header::ACCEPT,
        "application/json, application/fhir+json",
    );
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    send_raw(url, request).await
}

/// Fetches an issuer's OpenID configuration through `guard`.
pub async fn fetch_oidc_metadata(
    issuer: &str,
    guard: &OutboundGuard,
) -> Result<OidcMetadata, FetchError> {
    let url = openid_configuration_url(issuer);
    get_raw(&url, guard, Duration::from_secs(10), None)
        .await?
        .json(&url)
}

/// How long a discovered (or failed) issuer configuration is reused.
const DISCOVERY_TTL: Duration = Duration::from_secs(300);
const DISCOVERY_FAILURE_TTL: Duration = Duration::from_secs(30);

type DiscoveryEntry = (Instant, Result<Arc<OidcMetadata>, String>);

/// Process-wide cache of issuer discovery documents, keyed by issuer (and
/// whether the fetch was policy-guarded), so endpoints sharing an IdP fetch
/// its configuration once. Successes are kept 5 minutes, failures 30 s.
#[derive(Default)]
pub struct IssuerDiscovery {
    entries: Mutex<HashMap<(String, bool), DiscoveryEntry>>,
}

impl IssuerDiscovery {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// The issuer's configuration, from the cache or fetched now.
    pub async fn get(
        &self,
        issuer: &str,
        guard: &OutboundGuard,
    ) -> Result<Arc<OidcMetadata>, String> {
        let key = (
            issuer.to_string(),
            matches!(guard, OutboundGuard::Policy(_)),
        );
        {
            let entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((at, result)) = entries.get(&key) {
                let ttl = if result.is_ok() {
                    DISCOVERY_TTL
                } else {
                    DISCOVERY_FAILURE_TTL
                };
                if at.elapsed() < ttl {
                    return result.clone();
                }
            }
        }
        let result = fetch_oidc_metadata(issuer, guard)
            .await
            .map(Arc::new)
            .map_err(|e| e.to_string());
        if let Err(e) = &result {
            tracing::warn!(error = %e, "issuer discovery failed");
        }
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key, (Instant::now(), result.clone()));
        result
    }

    /// Drops entries of issuers no longer configured.
    pub fn retain_issuers(&self, live: &std::collections::HashSet<String>) {
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(issuer, _), _| live.contains(issuer));
    }
}

// ------------------------------------------------------------------- probe

/// One step of a [`ProbeReport`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProbeStep {
    /// Whether the step succeeded.
    pub ok: bool,
    /// The URL fetched (credentials redacted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// What happened.
    pub detail: String,
    /// Signing keys found (JWKS step).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_count: Option<usize>,
    /// Time taken.
    pub elapsed_ms: u64,
}

/// What `POST /admin/endpoints/{name}/$test-auth` answers: whether the
/// issuer's discovery document and the JWKS can be fetched (through the
/// outbound policy) and are coherent, plus configuration warnings.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProbeReport {
    /// Every step succeeded (warnings do not count).
    pub ok: bool,
    /// The auth mode probed.
    pub mode: String,
    /// Issuer discovery (`None` when auth is off).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer: Option<ProbeStep>,
    /// JWKS fetch (`None` when auth is off).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwks: Option<ProbeStep>,
    /// Non-blocking findings (no audience, admin credential accepted, …).
    pub warnings: Vec<String>,
    /// The same outcome as a flat list (`issuer-discovery`, `jwks`,
    /// `algorithms`, and `token` when one was supplied), the shape the admin
    /// API's `$test-auth` answers with (package P4's `ProbeCheck`).
    pub checks: Vec<ProbeCheck>,
}

/// One check of a [`ProbeReport`] (`{name, ok, detail}`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProbeCheck {
    /// `issuer-discovery`, `jwks`, `algorithms`, `token`.
    pub name: String,
    /// Whether it passed.
    pub ok: bool,
    /// Details or the error.
    #[serde(default)]
    pub detail: Option<String>,
}

impl ProbeCheck {
    fn new(name: &str, ok: bool, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ok,
            detail: Some(detail.into()),
        }
    }
}

/// Algorithms the token verifier implements (`EndpointAuth::algorithms`
/// may name others the model accepts, e.g. `PS256`; those never verify).
pub(crate) const VERIFIER_ALGORITHMS: &[&str] = &["RS256", "RS384", "RS512", "ES256", "ES384"];

/// Probes an endpoint's auth configuration (package P2's contract for the
/// admin API's `$test-auth`, package P4).
#[derive(Debug, Clone)]
pub struct EndpointAuthProbe {
    pub(crate) guard: OutboundGuard,
    /// Per-request timeout of [`verify`](Self::verify) (default 10 s).
    pub(crate) timeout: Duration,
}

impl EndpointAuthProbe {
    /// A probe fetching through `guard`.
    pub fn new(guard: OutboundGuard) -> Self {
        Self {
            guard,
            timeout: Duration::from_secs(10),
        }
    }

    /// Sets the per-request timeout of [`verify`](Self::verify).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Runs the probe.
    pub async fn test(&self, auth: &EndpointAuth) -> ProbeReport {
        let mode = match auth.mode {
            EndpointAuthMode::Off => "off",
            EndpointAuthMode::WriteProtect => "write-protect",
            EndpointAuthMode::Full => "full",
        };
        let warnings = auth_warnings(auth);
        if auth.mode == EndpointAuthMode::Off {
            return ProbeReport {
                ok: true,
                mode: mode.into(),
                issuer: None,
                jwks: None,
                warnings,
                checks: vec![ProbeCheck::new(
                    "mode",
                    true,
                    "auth is off: no credential is needed",
                )],
            };
        }
        let issuer = self.issuer_step(auth.issuer.as_deref()).await;
        let jwks = self.jwks_step(auth.jwks_url.as_deref()).await;
        let unsupported: Vec<&str> = auth
            .algorithms
            .iter()
            .map(String::as_str)
            .filter(|a| !VERIFIER_ALGORITHMS.contains(a))
            .collect();
        let algorithms = if auth.algorithms.is_empty() {
            ProbeCheck::new("algorithms", true, "default: RS256, RS384, ES256, ES384")
        } else if unsupported.len() == auth.algorithms.len() {
            ProbeCheck::new(
                "algorithms",
                false,
                format!(
                    "none of {} is supported by the verifier ({})",
                    unsupported.join(", "),
                    VERIFIER_ALGORITHMS.join(", ")
                ),
            )
        } else if unsupported.is_empty() {
            ProbeCheck::new("algorithms", true, auth.algorithms.join(", "))
        } else {
            ProbeCheck::new(
                "algorithms",
                true,
                format!("{} are ignored (unsupported)", unsupported.join(", ")),
            )
        };
        let checks = vec![
            ProbeCheck::new("issuer-discovery", issuer.ok, issuer.detail.clone()),
            ProbeCheck::new("jwks", jwks.ok, jwks.detail.clone()),
            algorithms,
        ];
        ProbeReport {
            ok: checks.iter().all(|c| c.ok),
            mode: mode.into(),
            issuer: Some(issuer),
            jwks: Some(jwks),
            warnings,
            checks,
        }
    }

    /// [`test`](Self::test), plus a `token` check: `token` (a bare JWT, no
    /// `Bearer `) is validated exactly as the endpoint would validate it
    /// (issuer, audience, algorithms, signature against the JWKS fetched
    /// through the guard, expiry with the endpoint's skew). The detail names
    /// the subject and scopes on success, the reason otherwise. The token is
    /// never logged or echoed.
    pub async fn test_with_token(&self, auth: &EndpointAuth, token: &str) -> ProbeReport {
        let mut report = self.test(auth).await;
        let check = self.token_check(auth, token.trim()).await;
        report.ok &= check.ok;
        report.checks.push(check);
        report
    }

    async fn token_check(&self, auth: &EndpointAuth, token: &str) -> ProbeCheck {
        use crate::provider::AuthProvider;
        if auth.mode == EndpointAuthMode::Off {
            return ProbeCheck::new("token", true, "auth is off: tokens are ignored");
        }
        let config = AuthConfig::from_endpoint(auth, &AuthConfig::default());
        let Some(jwks_url) = config.jwks_url.clone() else {
            return ProbeCheck::new("token", false, "no JWKS URL configured");
        };
        if let Err(e) = self.guard.check_url(&jwks_url) {
            return ProbeCheck::new("token", false, e);
        }
        let cache = Arc::new(
            crate::jwks::JwksCache::new(&jwks_url, 0)
                .with_max_ttl(config.jwks_max_age_secs)
                .with_http_client(self.guard.client(Duration::from_secs(10))),
        );
        let provider = crate::provider::jwks_bearer::JwksBearerAuthProvider::new(cache, &config);
        match provider.authenticate(&format!("Bearer {token}")).await {
            Ok(p) => ProbeCheck::new(
                "token",
                true,
                format!(
                    "accepted: sub {}, scopes {}",
                    p.subject,
                    if p.scopes.raw().is_empty() {
                        "(none)".to_string()
                    } else {
                        p.scopes.raw().join(" ")
                    }
                ),
            ),
            Err(e) => ProbeCheck::new("token", false, format!("refused: {e}")),
        }
    }

    async fn issuer_step(&self, issuer: Option<&str>) -> ProbeStep {
        let started = Instant::now();
        let Some(issuer) = issuer.map(str::trim).filter(|i| !i.is_empty()) else {
            return step(false, None, "no issuer configured".into(), None, started);
        };
        let url = openid_configuration_url(issuer);
        match fetch_oidc_metadata(issuer, &self.guard).await {
            Ok(meta) => {
                let stated = meta.issuer.as_deref().unwrap_or("");
                if stated.trim_end_matches('/') != issuer.trim_end_matches('/') {
                    step(
                        false,
                        Some(&url),
                        format!(
                            "the IdP states issuer {stated:?}, the endpoint expects {issuer:?}; tokens would be refused"
                        ),
                        None,
                        started,
                    )
                } else {
                    let mut found = Vec::new();
                    if meta.authorization_endpoint.is_some() {
                        found.push("authorization_endpoint");
                    }
                    if meta.token_endpoint.is_some() {
                        found.push("token_endpoint");
                    }
                    step(
                        true,
                        Some(&url),
                        format!("discovered ({})", found.join(", ")),
                        None,
                        started,
                    )
                }
            }
            Err(e) => step(false, Some(&url), e.to_string(), None, started),
        }
    }

    async fn jwks_step(&self, jwks_url: Option<&str>) -> ProbeStep {
        let started = Instant::now();
        let Some(url) = jwks_url.map(str::trim).filter(|u| !u.is_empty()) else {
            return step(false, None, "no JWKS URL configured".into(), None, started);
        };
        if let Err(e) = self.guard.check_url(url) {
            return step(false, Some(url), e, None, started);
        }
        let fetcher = crate::jwks::fetcher::JwksFetcher::with_client(
            self.guard.client(Duration::from_secs(10)),
        );
        match fetcher.fetch(url).await {
            Ok(resp) if resp.keys.is_empty() => step(
                false,
                Some(url),
                "the JWKS holds no usable signing key".into(),
                Some(0),
                started,
            ),
            Ok(resp) => {
                let n = resp.keys.len();
                step(
                    true,
                    Some(url),
                    format!("{n} signing key(s)"),
                    Some(n),
                    started,
                )
            }
            Err(e) => step(false, Some(url), e.to_string(), None, started),
        }
    }
}

fn step(
    ok: bool,
    url: Option<&str>,
    detail: String,
    key_count: Option<usize>,
    started: Instant,
) -> ProbeStep {
    ProbeStep {
        ok,
        url: url.map(redact_url),
        detail,
        key_count,
        elapsed_ms: started.elapsed().as_millis() as u64,
    }
}

/// Non-blocking warnings about one endpoint's auth settings (design §6):
/// auth on without an audience; the administrator credential accepted under
/// write-protect (informational, normal for break-glass); plain-http URLs.
pub fn auth_warnings(auth: &EndpointAuth) -> Vec<String> {
    let mut out = Vec::new();
    if auth.mode == EndpointAuthMode::Off {
        return out;
    }
    if auth.audience.as_deref().is_none_or(|a| a.trim().is_empty()) {
        out.push(
            "no audience: any token of this issuer is accepted, including tokens minted for other endpoints or services"
                .into(),
        );
    }
    for (field, url) in [("jwksUrl", &auth.jwks_url), ("issuer", &auth.issuer)] {
        if url.as_deref().is_some_and(|u| u.starts_with("http://")) {
            out.push(format!(
                "{field} uses plain http: it is fetched only when its host is in HFS_OUTBOUND_ALLOW"
            ));
        }
    }
    if auth.mode == EndpointAuthMode::WriteProtect && auth.accept_admin_credential {
        out.push(
            "info: the administrator credential (HFS_ADMIN_PASSWORD) is accepted for writes".into(),
        );
    }
    out
}

/// Warnings across endpoints: two endpoints with auth on, the same issuer and
/// the same (or no) audience accept each other's tokens. Input: `(endpoint
/// name, auth)`; output: `(endpoint name, warning)`.
pub fn shared_audience_warnings<'a>(
    endpoints: impl IntoIterator<Item = (&'a str, &'a EndpointAuth)>,
) -> Vec<(String, String)> {
    let live: Vec<(&str, &EndpointAuth)> = endpoints
        .into_iter()
        .filter(|(_, a)| a.mode != EndpointAuthMode::Off)
        .collect();
    let mut out = Vec::new();
    for (i, (name, a)) in live.iter().enumerate() {
        for (j, (other, b)) in live.iter().enumerate() {
            if i == j || a.issuer != b.issuer {
                continue;
            }
            let aud = |x: &EndpointAuth| x.audience.clone().filter(|s| !s.trim().is_empty());
            if aud(a).is_none() || aud(a) == aud(b) {
                out.push((
                    name.to_string(),
                    format!(
                        "shares issuer {} with endpoint {other} and its audience does not tell them apart: a token for one is accepted by the other",
                        a.issuer.as_deref().unwrap_or("")
                    ),
                ));
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const N: &str = "zbeWVBfqrPsdF1phb2EB5hxjz1EMb5WT4MnhMatFnNFquEZIcgXG1DBAUICEZLQM8gRj9kZ83YA1cFhUtkgUx1qMio8YkKweLV6r8RAqnuxrS5Khm4a25M85qB5MhGoCLZgGtla9lJ79mjVN5XBqD7sPL0ENN1GPcnus1I6WF-E-5Uiih-QWsQVlGeUqMH81vTBLemDxvMQv-TiKZP3LHH_T41G1FfyF-rlbkj7WXGkbqG_WvR4FKeRm1X2_RRVw87h2j1lfK4AiUKs4h_DqwqZju3fgOH_0MHj2RKCu0wo0cTx8mQGbLCNSb-sRoWRnWGpGPphI9xVS8znuPB1JYQ";

    async fn idp(stated_issuer: Option<&str>) -> (MockServer, String) {
        let server = MockServer::start().await;
        let issuer = format!("{}/realms/t", server.uri());
        Mock::given(method("GET"))
            .and(path("/realms/t/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issuer": stated_issuer.unwrap_or(&issuer),
                "authorization_endpoint": format!("{issuer}/auth"),
                "token_endpoint": format!("{issuer}/token"),
                "jwks_uri": format!("{issuer}/certs"),
                "end_session_endpoint": format!("{issuer}/logout"),
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/realms/t/certs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "keys": [{"kty": "RSA", "kid": "k", "use": "sig", "n": N, "e": "AQAB"}]
            })))
            .mount(&server)
            .await;
        (server, issuer)
    }

    fn loopback_policy() -> OutboundGuard {
        OutboundGuard::Policy(Arc::new(OutboundPolicy::parse("127.0.0.1")))
    }

    #[tokio::test]
    async fn issuer_discovery_is_cached_and_fills_the_endpoint_document() {
        let (server, issuer) = idp(None).await;
        let cache = IssuerDiscovery::new();
        let meta = cache.get(&issuer, &loopback_policy()).await.unwrap();
        assert_eq!(
            meta.token_endpoint.as_deref(),
            Some(&*format!("{issuer}/token"))
        );
        let _ = cache.get(&issuer, &loopback_policy()).await.unwrap();
        assert_eq!(server.received_requests().await.unwrap().len(), 1, "cached");

        let config = AuthConfig {
            enabled: true,
            expected_issuer: Some(issuer.clone()),
            jwks_url: Some("http://internal/certs".into()),
            smart_token_endpoint: Some("https://explicit/token".into()),
            ..AuthConfig::default()
        };
        let doc = SmartConfiguration::for_endpoint(&config, Some(&meta), None);
        assert_eq!(
            doc.token_endpoint.as_deref(),
            Some("https://explicit/token"),
            "explicit wins"
        );
        assert_eq!(
            doc.authorization_endpoint.as_deref(),
            Some(&*format!("{issuer}/auth"))
        );
        assert_eq!(
            doc.jwks_uri.as_deref(),
            Some(&*format!("{issuer}/certs")),
            "public JWKS"
        );
        assert_eq!(
            doc.end_session_endpoint.as_deref(),
            Some(&*format!("{issuer}/logout"))
        );
        let json = doc.to_json();
        assert_eq!(json["end_session_endpoint"], format!("{issuer}/logout"));
        // The environment's document never carries end_session_endpoint.
        assert!(
            SmartConfiguration::from_config(&config)
                .to_json()
                .get("end_session_endpoint")
                .is_none()
        );

        cache.retain_issuers(&std::collections::HashSet::new());
        let _ = cache.get(&issuer, &loopback_policy()).await.unwrap();
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "dropped, refetched"
        );
    }

    #[tokio::test]
    async fn the_policy_refuses_internal_addresses_before_connecting() {
        let (server, issuer) = idp(None).await;
        let strict = OutboundGuard::Policy(Arc::new(OutboundPolicy::strict()));
        let err = fetch_oidc_metadata(&issuer, &strict)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("outbound policy"), "{err}");
        // A host name resolving to loopback is refused by the resolver too.
        let by_name = issuer.replace("127.0.0.1", "localhost");
        let https = by_name.replace("http://", "https://");
        let err = fetch_oidc_metadata(&https, &strict).await.unwrap_err();
        assert!(err.is_transport(), "{err}");
        assert!(server.received_requests().await.unwrap().is_empty());
        assert!(OutboundGuard::Trusted.check_url("http://anything/").is_ok());
        assert!(strict.check_url("https://user:pw@idp.example/").is_err());
    }

    #[tokio::test]
    async fn the_probe_reports_issuer_jwks_and_warnings() {
        let (_server, issuer) = idp(None).await;
        let probe = EndpointAuthProbe::new(loopback_policy());
        let mut auth = EndpointAuth {
            mode: EndpointAuthMode::WriteProtect,
            issuer: Some(issuer.clone()),
            jwks_url: Some(format!("{issuer}/certs")),
            ..EndpointAuth::default()
        };
        let report = probe.test(&auth).await;
        assert!(report.ok, "{report:?}");
        assert_eq!(report.jwks.as_ref().unwrap().key_count, Some(1));
        assert!(report.warnings.iter().any(|w| w.contains("no audience")));
        assert!(report.warnings.iter().any(|w| w.contains("plain http")));
        assert!(report.warnings.iter().any(|w| w.starts_with("info:")));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["jwks"]["keyCount"], 1);

        auth.jwks_url = Some(format!("{issuer}/missing"));
        let report = probe.test(&auth).await;
        assert!(!report.ok);
        assert!(!report.jwks.unwrap().ok);

        let strict =
            EndpointAuthProbe::new(OutboundGuard::Policy(Arc::new(OutboundPolicy::strict())));
        let report = strict.test(&auth).await;
        assert!(!report.ok);
        assert!(report.issuer.unwrap().detail.contains("outbound policy"));

        auth.mode = EndpointAuthMode::Off;
        let report = probe.test(&auth).await;
        assert!(report.ok && report.issuer.is_none() && report.warnings.is_empty());
    }

    #[tokio::test]
    async fn the_probe_checks_a_supplied_token_and_the_algorithms() {
        let (_server, issuer) = idp(None).await;
        let probe = EndpointAuthProbe::new(loopback_policy());
        let mut auth = EndpointAuth {
            mode: EndpointAuthMode::Full,
            issuer: Some(issuer.clone()),
            jwks_url: Some(format!("{issuer}/certs")),
            audience: Some("api".into()),
            ..EndpointAuth::default()
        };
        let sign = |aud: &str| {
            let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
            header.kid = Some("k".into());
            let claims = serde_json::json!({"sub": "app", "iss": issuer, "aud": aud,
                "exp": jsonwebtoken::get_current_timestamp() + 300, "scope": "system/*.rs"});
            let key = jsonwebtoken::EncodingKey::from_rsa_pem(include_bytes!(
                "../tests/data/test_rsa_key.pem"
            ))
            .unwrap();
            jsonwebtoken::encode(&header, &claims, &key).unwrap()
        };
        let report = probe.test_with_token(&auth, &sign("api")).await;
        assert!(report.ok, "{report:?}");
        let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["issuer-discovery", "jwks", "algorithms", "token"]);
        let token = report.checks.last().unwrap();
        assert!(
            token.detail.as_deref().unwrap().contains("system/*.rs"),
            "{token:?}"
        );

        let report = probe.test_with_token(&auth, &sign("other-api")).await;
        assert!(!report.ok);
        let token = report.checks.last().unwrap();
        assert!(
            !token.ok && token.detail.as_deref().unwrap().contains("audience"),
            "{token:?}"
        );

        auth.algorithms = vec!["PS256".into()];
        let report = probe.test(&auth).await;
        assert!(!report.ok);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "algorithms" && !c.ok)
        );
        auth.algorithms = vec!["PS256".into(), "RS256".into()];
        assert!(probe.test(&auth).await.ok);
        let json = serde_json::to_value(probe.test(&auth).await).unwrap();
        assert_eq!(json["checks"][0]["name"], "issuer-discovery");
    }

    #[tokio::test]
    async fn the_probe_flags_an_issuer_mismatch() {
        let (_server, issuer) = idp(Some("https://somebody-else")).await;
        let probe = EndpointAuthProbe::new(loopback_policy());
        let auth = EndpointAuth {
            mode: EndpointAuthMode::Full,
            issuer: Some(issuer.clone()),
            jwks_url: Some(format!("{issuer}/certs")),
            audience: Some("x".into()),
            ..EndpointAuth::default()
        };
        let report = probe.test(&auth).await;
        assert!(!report.ok);
        assert!(report.issuer.unwrap().detail.contains("somebody-else"));
        let missing = EndpointAuth {
            mode: EndpointAuthMode::Full,
            ..EndpointAuth::default()
        };
        let report = probe.test(&missing).await;
        assert!(!report.ok);
    }

    #[test]
    fn shared_audiences_are_flagged() {
        let on = |iss: &str, aud: Option<&str>| EndpointAuth {
            mode: EndpointAuthMode::Full,
            issuer: Some(iss.into()),
            jwks_url: Some(format!("{iss}/certs")),
            audience: aud.map(str::to_string),
            ..EndpointAuth::default()
        };
        let a = on("https://idp", Some("a"));
        let b = on("https://idp", Some("b"));
        let c = on("https://idp", Some("a"));
        let d = on("https://other", None);
        let off = EndpointAuth::default();
        assert!(
            shared_audience_warnings([("a", &a), ("b", &b), ("d", &d), ("off", &off)]).is_empty()
        );
        let w = shared_audience_warnings([("a", &a), ("c", &c)]);
        assert_eq!(w.len(), 2);
        assert!(w[0].1.contains("endpoint c"));
        let e = on("https://other", None);
        assert_eq!(shared_audience_warnings([("d", &d), ("e", &e)]).len(), 2);
    }

    #[test]
    fn test_smart_config_from_default() {
        let config = AuthConfig::default();
        let smart = SmartConfiguration::from_config(&config);

        assert!(smart.issuer.is_none());
        assert!(smart.token_endpoint.is_none());
        assert!(smart.capabilities.contains(&"permission-v2".to_string()));
        assert_eq!(smart.code_challenge_methods_supported, vec!["S256"]);
    }

    #[test]
    fn test_smart_config_with_values() {
        let config = AuthConfig {
            expected_issuer: Some("https://idp.example.com".to_string()),
            smart_token_endpoint: Some("https://idp.example.com/token".to_string()),
            smart_authorize_endpoint: Some("https://idp.example.com/authorize".to_string()),
            smart_jwks_url: Some("https://idp.example.com/.well-known/jwks.json".to_string()),
            ..AuthConfig::default()
        };
        let smart = SmartConfiguration::from_config(&config);

        assert_eq!(smart.issuer.as_deref(), Some("https://idp.example.com"));
        assert_eq!(
            smart.token_endpoint.as_deref(),
            Some("https://idp.example.com/token")
        );
        assert!(
            smart
                .grant_types_supported
                .contains(&"authorization_code".to_string())
        );
        assert!(smart.response_types_supported.contains(&"code".to_string()));
        assert_eq!(
            smart.jwks_uri.as_deref(),
            Some("https://idp.example.com/.well-known/jwks.json")
        );
    }

    #[test]
    fn test_smart_config_json_serialization() {
        let config = AuthConfig {
            expected_issuer: Some("https://idp.example.com".to_string()),
            ..AuthConfig::default()
        };
        let smart = SmartConfiguration::from_config(&config);
        let json = smart.to_json();

        assert!(json["capabilities"].is_array());
        assert!(json["scopes_supported"].is_array());
        assert_eq!(
            json["code_challenge_methods_supported"],
            serde_json::json!(["S256"])
        );
        // Fields that are None should be omitted
        assert!(json.get("authorization_endpoint").is_none());
    }

    #[test]
    fn interactive_deployments_advertise_launch_and_patient_capabilities() {
        let config = AuthConfig {
            smart_authorize_endpoint: Some("https://idp/auth".to_string()),
            smart_token_endpoint: Some("https://idp/token".to_string()),
            ..AuthConfig::default()
        };
        let smart = SmartConfiguration::from_config(&config);
        for cap in [
            "launch-standalone",
            "permission-patient",
            "permission-v1",
            "permission-v2",
            "sso-openid-connect",
        ] {
            assert!(smart.capabilities.contains(&cap.to_string()), "{cap}");
        }
        assert_eq!(smart.response_types_supported, vec!["code"]);
        assert!(
            smart
                .grant_types_supported
                .contains(&"refresh_token".to_string())
        );
        assert!(
            !smart
                .code_challenge_methods_supported
                .contains(&"plain".to_string())
        );
    }

    #[test]
    fn backend_only_deployments_do_not_claim_a_launch() {
        let smart = SmartConfiguration::from_config(&AuthConfig::default());
        assert!(
            !smart
                .capabilities
                .contains(&"launch-standalone".to_string())
        );
        assert_eq!(smart.grant_types_supported, vec!["client_credentials"]);
        assert!(smart.response_types_supported.is_empty());
    }

    #[test]
    fn lists_are_overridable() {
        let config = AuthConfig {
            smart_capabilities: Some(vec!["launch-ehr".to_string()]),
            smart_scopes_supported: Some(vec!["system/*.rs".to_string()]),
            smart_token_auth_methods: Some(vec!["private_key_jwt".to_string()]),
            ..AuthConfig::default()
        };
        let smart = SmartConfiguration::from_config(&config);
        assert_eq!(smart.capabilities, vec!["launch-ehr"]);
        assert_eq!(smart.scopes_supported, vec!["system/*.rs"]);
        assert_eq!(
            smart.token_endpoint_auth_methods_supported,
            vec!["private_key_jwt"]
        );
    }
}
