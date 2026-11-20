//! Outbound HTTP for external FHIR servers, behind the SSRF policy
//! (design D-13).
//!
//! Every request: the URL passes [`OutboundPolicy::check_url`]; the host is
//! resolved and **every** address passes [`OutboundPolicy::check_ip`]; the
//! connection is pinned to the checked address (`reqwest` `resolve`), so a
//! DNS answer that changes between check and connect cannot redirect it;
//! redirects are not followed; responses are size-capped. Paging links from
//! a server must stay on the server's own origin.
//!
//! [`FhirClient`] adds authentication (static or OAuth token endpoint),
//! a token bucket and `Retry-After`-aware retries.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use helios_endpoints::outbound::OutboundPolicy;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio::sync::Mutex;
use url::Url;

use crate::secrets::ResolvedAuth;
use crate::throttle::{Backoff, TokenBucket, parse_retry_after, retryable_status};

/// Limits for outbound requests.
#[derive(Debug, Clone)]
pub struct HttpSettings {
    /// Per-request timeout.
    pub timeout: Duration,
    /// Response body cap.
    pub max_response_bytes: usize,
    /// How long a resolved + checked address is reused.
    pub resolve_ttl: Duration,
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            max_response_bytes: 64 * 1024 * 1024,
            resolve_ttl: Duration::from_secs(60),
        }
    }
}

/// Why a request failed.
#[derive(Debug, Clone, thiserror::Error)]
pub enum HttpError {
    /// Refused by the outbound policy.
    #[error("outbound policy refused {url}: {reason}")]
    Denied {
        /// URL (without query).
        url: String,
        /// Reason.
        reason: String,
    },
    /// Connection / protocol failure.
    #[error("request to {url} failed: {reason}")]
    Transport {
        /// URL (without query).
        url: String,
        /// Reason.
        reason: String,
    },
    /// Body over the cap.
    #[error("response from {0} exceeds the size limit")]
    TooLarge(String),
    /// Non-success status after retries.
    #[error("{url} answered HTTP {status}{detail}")]
    Status {
        /// URL (without query).
        url: String,
        /// Status.
        status: u16,
        /// Short detail (OperationOutcome diagnostics).
        detail: String,
    },
    /// Authentication against the token endpoint failed.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// Unparseable body.
    #[error("unexpected response from {url}: {reason}")]
    Body {
        /// URL.
        url: String,
        /// Reason.
        reason: String,
    },
}

/// A buffered response.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// Status code.
    pub status: u16,
    /// Headers.
    pub headers: HeaderMap,
    /// Body bytes.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// The body as JSON.
    pub fn json(&self, url: &Url) -> Result<Value, HttpError> {
        serde_json::from_slice(&self.body).map_err(|e| HttpError::Body {
            url: display_url(url),
            reason: format!("not JSON: {e}"),
        })
    }

    /// A header as text.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// Whether 2xx.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// URL without query (logs never carry tokens or search terms).
pub fn display_url(u: &Url) -> String {
    let mut c = u.clone();
    c.set_query(None);
    c.set_fragment(None);
    c.to_string()
}

/// Whether two URLs share scheme, host and port.
pub fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str().map(str::to_ascii_lowercase) == b.host_str().map(str::to_ascii_lowercase)
        && a.port_or_known_default() == b.port_or_known_default()
}

/// One outbound request.
#[derive(Debug, Clone)]
pub struct OutboundRequest {
    /// Method.
    pub method: reqwest::Method,
    /// URL.
    pub url: Url,
    /// Headers.
    pub headers: Vec<(String, String)>,
    /// Body.
    pub body: Option<Vec<u8>>,
}

/// SSRF-guarded HTTP with connection pinning.
pub struct GuardedHttp {
    policy: OutboundPolicy,
    settings: HttpSettings,
    clients: Mutex<HashMap<(String, u16), (reqwest::Client, Instant)>>,
}

impl std::fmt::Debug for GuardedHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardedHttp")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl GuardedHttp {
    /// A client under `policy`.
    pub fn new(policy: OutboundPolicy, settings: HttpSettings) -> Self {
        Self {
            policy,
            settings,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// The policy.
    pub fn policy(&self) -> &OutboundPolicy {
        &self.policy
    }

    /// Checks a URL and returns a client pinned to a checked address.
    pub async fn client_for(&self, url: &Url) -> Result<reqwest::Client, HttpError> {
        let denied = |reason: String| HttpError::Denied {
            url: display_url(url),
            reason,
        };
        self.policy
            .check_url(url.as_str())
            .map_err(|e| denied(e.to_string()))?;
        let host = url
            .host_str()
            .ok_or_else(|| denied("no host".into()))?
            .to_string();
        let port = url
            .port_or_known_default()
            .ok_or_else(|| denied("no port".into()))?;
        let key = (host.to_ascii_lowercase(), port);
        {
            let cache = self.clients.lock().await;
            if let Some((c, at)) = cache.get(&key)
                && at.elapsed() < self.settings.resolve_ttl
            {
                return Ok(c.clone());
            }
        }
        let bare = host.trim_start_matches('[').trim_end_matches(']').to_string();
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(self.settings.timeout)
            .user_agent(concat!("helios-sync/", env!("CARGO_PKG_VERSION")));
        if bare.parse::<IpAddr>().is_err() {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((bare.as_str(), port))
                .await
                .map_err(|e| HttpError::Transport {
                    url: display_url(url),
                    reason: format!("cannot resolve {bare}: {e}"),
                })?
                .collect();
            if addrs.is_empty() {
                return Err(denied(format!("{bare} resolves to no address")));
            }
            for a in &addrs {
                self.policy
                    .check_ip(&bare, a.ip())
                    .map_err(|e| denied(e.to_string()))?;
            }
            builder = builder.resolve(&bare, addrs[0]);
        }
        let client = builder.build().map_err(|e| HttpError::Transport {
            url: display_url(url),
            reason: e.to_string(),
        })?;
        self.clients
            .lock()
            .await
            .insert(key, (client.clone(), Instant::now()));
        Ok(client)
    }

    /// Sends one request (no retries) and buffers the capped body.
    pub async fn send(&self, req: OutboundRequest) -> Result<HttpResponse, HttpError> {
        let client = self.client_for(&req.url).await?;
        let shown = display_url(&req.url);
        let mut headers = HeaderMap::new();
        for (k, v) in &req.headers {
            if let (Ok(n), Ok(v)) = (HeaderName::try_from(k.as_str()), HeaderValue::from_str(v)) {
                headers.insert(n, v);
            }
        }
        let mut rb = client.request(req.method, req.url).headers(headers);
        if let Some(b) = req.body {
            rb = rb.body(b);
        }
        let mut resp = rb.send().await.map_err(|e| HttpError::Transport {
            url: shown.clone(),
            reason: error_chain(&e),
        })?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        if let Some(len) = resp.content_length()
            && len as usize > self.settings.max_response_bytes
        {
            return Err(HttpError::TooLarge(shown));
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| HttpError::Transport {
            url: shown.clone(),
            reason: error_chain(&e),
        })? {
            if body.len() + chunk.len() > self.settings.max_response_bytes {
                return Err(HttpError::TooLarge(shown));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(x) = src {
        s.push_str(": ");
        s.push_str(&x.to_string());
        src = x.source();
    }
    s
}

/// Extracts the first OperationOutcome diagnostics for error messages.
pub fn outcome_detail(body: &[u8]) -> String {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return String::new();
    };
    let issue = v.get("issue").and_then(|i| i.get(0));
    let text = issue
        .and_then(|i| i.get("diagnostics").or_else(|| i.get("details").and_then(|d| d.get("text"))))
        .and_then(Value::as_str)
        .unwrap_or("");
    if text.is_empty() {
        String::new()
    } else {
        format!(": {}", text.chars().take(300).collect::<String>())
    }
}

/// A FHIR server client: base URL, credentials, pacing, retries.
pub struct FhirClient {
    http: Arc<GuardedHttp>,
    base: Url,
    auth: ResolvedAuth,
    headers: BTreeMap<String, String>,
    bucket: TokenBucket,
    backoff: Backoff,
    token: Mutex<Option<(String, Instant)>>,
}

impl std::fmt::Debug for FhirClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FhirClient")
            .field("base", &display_url(&self.base))
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

impl FhirClient {
    /// A client for `base_url`.
    pub fn new(
        http: Arc<GuardedHttp>,
        base_url: &str,
        auth: ResolvedAuth,
        headers: BTreeMap<String, String>,
        max_requests_per_second: Option<u32>,
    ) -> Result<Self, HttpError> {
        let mut base = Url::parse(base_url.trim_end_matches('/')).map_err(|e| HttpError::Denied {
            url: base_url.to_string(),
            reason: e.to_string(),
        })?;
        base.set_query(None);
        base.set_fragment(None);
        http.policy()
            .check_url(base.as_str())
            .map_err(|e| HttpError::Denied {
                url: display_url(&base),
                reason: e.to_string(),
            })?;
        Ok(Self {
            http,
            base,
            auth,
            headers,
            bucket: TokenBucket::new(max_requests_per_second.unwrap_or(10)),
            backoff: Backoff::default(),
            token: Mutex::new(None),
        })
    }

    /// Overrides the retry policy (tests).
    pub fn with_backoff(mut self, b: Backoff) -> Self {
        self.backoff = b;
        self
    }

    /// The base URL.
    pub fn base(&self) -> &Url {
        &self.base
    }

    /// `{base}/{relative}` (relative may carry a query).
    pub fn url(&self, relative: &str) -> Url {
        let s = format!("{}/{}", self.base.as_str().trim_end_matches('/'), relative.trim_start_matches('/'));
        Url::parse(&s).unwrap_or_else(|_| self.base.clone())
    }

    /// Refuses a server-supplied link that leaves the server's origin.
    pub fn check_link(&self, link: &str) -> Result<Url, HttpError> {
        let u = Url::parse(link).or_else(|_| self.base.join(link)).map_err(|e| HttpError::Body {
            url: display_url(&self.base),
            reason: format!("bad link {link:?}: {e}"),
        })?;
        if !same_origin(&u, &self.base) {
            return Err(HttpError::Denied {
                url: display_url(&u),
                reason: "link leaves the server's origin".into(),
            });
        }
        Ok(u)
    }

    async fn bearer(&self, force_refresh: bool) -> Result<Option<String>, HttpError> {
        match &self.auth {
            ResolvedAuth::None | ResolvedAuth::Basic { .. } => Ok(None),
            ResolvedAuth::Bearer(t) => Ok(t.expose_str().map(str::to_string)),
            ResolvedAuth::ClientCredentials { .. } | ResolvedAuth::PrivateKeyJwt { .. } => {
                let mut cached = self.token.lock().await;
                if !force_refresh
                    && let Some((t, exp)) = cached.as_ref()
                    && Instant::now() + Duration::from_secs(30) < *exp
                {
                    return Ok(Some(t.clone()));
                }
                let (t, ttl) = self.fetch_token().await?;
                *cached = Some((t.clone(), Instant::now() + ttl));
                Ok(Some(t))
            }
        }
    }

    async fn fetch_token(&self) -> Result<(String, Duration), HttpError> {
        let (token_url, form, basic) = match &self.auth {
            ResolvedAuth::ClientCredentials {
                token_url,
                client_id,
                client_secret,
                scope,
            } => {
                let mut form = vec![("grant_type".to_string(), "client_credentials".to_string())];
                if let Some(s) = scope {
                    form.push(("scope".into(), s.clone()));
                }
                let secret = client_secret.expose_str().unwrap_or_default();
                (token_url.clone(), form, Some(basic_header(client_id, secret)))
            }
            ResolvedAuth::PrivateKeyJwt {
                token_url,
                client_id,
                key,
                alg,
                kid,
                scope,
            } => {
                let assertion = client_assertion(client_id, token_url, key.expose(), alg, kid.as_deref())
                    .map_err(HttpError::Auth)?;
                let mut form = vec![
                    ("grant_type".to_string(), "client_credentials".to_string()),
                    (
                        "client_assertion_type".into(),
                        "urn:ietf:params:oauth:client-assertion-type:jwt-bearer".into(),
                    ),
                    ("client_assertion".into(), assertion),
                ];
                if let Some(s) = scope {
                    form.push(("scope".into(), s.clone()));
                }
                (token_url.clone(), form, None)
            }
            _ => return Err(HttpError::Auth("no token endpoint".into())),
        };
        let url = Url::parse(&token_url).map_err(|e| HttpError::Auth(format!("token URL: {e}")))?;
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form.iter())
            .finish();
        let mut headers = vec![
            ("content-type".to_string(), "application/x-www-form-urlencoded".to_string()),
            ("accept".to_string(), "application/json".to_string()),
        ];
        if let Some(b) = basic {
            headers.push(("authorization".into(), b));
        }
        let resp = self
            .http
            .send(OutboundRequest {
                method: reqwest::Method::POST,
                url: url.clone(),
                headers,
                body: Some(body.into_bytes()),
            })
            .await?;
        if !resp.ok() {
            return Err(HttpError::Auth(format!(
                "token endpoint answered HTTP {}",
                resp.status
            )));
        }
        let v = resp.json(&url)?;
        let token = v
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| HttpError::Auth("no access_token in the token response".into()))?;
        let ttl = v.get("expires_in").and_then(Value::as_u64).unwrap_or(300);
        Ok((token.to_string(), Duration::from_secs(ttl.max(1))))
    }

    /// Sends a request to the server (same origin as the base), with
    /// credentials, pacing and retries. Returns the final response whatever
    /// its status; transport errors after retries are errors.
    pub async fn send(
        &self,
        method: reqwest::Method,
        url: Url,
        body: Option<&Value>,
        extra_headers: &[(&str, String)],
    ) -> Result<HttpResponse, HttpError> {
        if !same_origin(&url, &self.base) {
            return Err(HttpError::Denied {
                url: display_url(&url),
                reason: "not on the configured server's origin".into(),
            });
        }
        let payload = body.map(|b| serde_json::to_vec(b).expect("JSON serializes"));
        let mut attempt = 0u32;
        let mut refreshed = false;
        loop {
            attempt += 1;
            self.bucket.acquire().await;
            let mut headers: Vec<(String, String)> = vec![(
                "accept".into(),
                "application/fhir+json".into(),
            )];
            if payload.is_some() {
                headers.push(("content-type".into(), "application/fhir+json".into()));
            }
            for (k, v) in &self.headers {
                headers.push((k.clone(), v.clone()));
            }
            for (k, v) in extra_headers {
                headers.push(((*k).to_string(), v.clone()));
            }
            match &self.auth {
                ResolvedAuth::Basic { username, password } => headers.push((
                    "authorization".into(),
                    basic_header(username, password.expose_str().unwrap_or_default()),
                )),
                _ => {
                    if let Some(t) = self.bearer(false).await? {
                        headers.push(("authorization".into(), format!("Bearer {t}")));
                    }
                }
            }
            let req = OutboundRequest {
                method: method.clone(),
                url: url.clone(),
                headers,
                body: payload.clone(),
            };
            let result = self.http.send(req).await;
            match result {
                Ok(resp) if resp.status == 401 && !refreshed && self.uses_token_endpoint() => {
                    refreshed = true;
                    self.bearer(true).await?;
                    attempt -= 1;
                }
                Ok(resp) if retryable_status(resp.status) && self.backoff.should_retry(attempt) => {
                    let ra = resp
                        .header("retry-after")
                        .and_then(|v| parse_retry_after(v, Utc::now()));
                    tokio::time::sleep(self.backoff.delay(attempt, ra)).await;
                }
                Ok(resp) => return Ok(resp),
                Err(HttpError::Transport { .. }) if self.backoff.should_retry(attempt) => {
                    tokio::time::sleep(self.backoff.delay(attempt, None)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn uses_token_endpoint(&self) -> bool {
        matches!(
            self.auth,
            ResolvedAuth::ClientCredentials { .. } | ResolvedAuth::PrivateKeyJwt { .. }
        )
    }

    /// `GET` that must answer 2xx with JSON.
    pub async fn get_json(&self, url: Url) -> Result<Value, HttpError> {
        let resp = self.send(reqwest::Method::GET, url.clone(), None, &[]).await?;
        if !resp.ok() {
            return Err(HttpError::Status {
                url: display_url(&url),
                status: resp.status,
                detail: outcome_detail(&resp.body),
            });
        }
        resp.json(&url)
    }

    /// `GET` of a URL on another origin (bulk export output files), still
    /// under the outbound policy; credentials are sent only when the file
    /// server is the FHIR server's origin.
    pub async fn get_foreign(&self, url: Url, with_auth: bool) -> Result<HttpResponse, HttpError> {
        if same_origin(&url, &self.base) {
            return self.send(reqwest::Method::GET, url, None, &[]).await;
        }
        let mut headers = vec![("accept".to_string(), "application/fhir+ndjson".to_string())];
        if with_auth && let Some(t) = self.bearer(false).await? {
            headers.push(("authorization".into(), format!("Bearer {t}")));
        }
        self.bucket.acquire().await;
        self.http
            .send(OutboundRequest {
                method: reqwest::Method::GET,
                url,
                headers,
                body: None,
            })
            .await
    }
}

fn basic_header(user: &str, pass: &str) -> String {
    use base64::Engine as _;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
    )
}

/// Builds a SMART Backend Services client assertion. `key` is a PEM private
/// key, or a P-384 EC private JWK (JSON) for `ES384`.
pub fn client_assertion(
    client_id: &str,
    token_url: &str,
    key: &[u8],
    alg: &str,
    kid: Option<&str>,
) -> Result<String, String> {
    use jsonwebtoken::{Algorithm, EncodingKey, Header};
    let text = std::str::from_utf8(key).map_err(|_| "private key is not UTF-8".to_string())?;
    let algorithm = match alg {
        "ES384" => Algorithm::ES384,
        "RS384" => Algorithm::RS384,
        other => return Err(format!("unsupported alg {other:?} (use RS384 or ES384)")),
    };
    let pem = if text.trim_start().starts_with("-----BEGIN") {
        text.to_string()
    } else {
        jwk_to_pem(text, alg)?
    };
    let encoding = match algorithm {
        Algorithm::ES384 => EncodingKey::from_ec_pem(pem.as_bytes()),
        _ => EncodingKey::from_rsa_pem(pem.as_bytes()),
    }
    .map_err(|e| format!("private key: {e}"))?;
    let mut header = Header::new(algorithm);
    header.kid = kid.map(str::to_string);
    let now = Utc::now().timestamp();
    let claims = serde_json::json!({
        "iss": client_id, "sub": client_id, "aud": token_url,
        "exp": now + 300, "iat": now, "jti": uuid::Uuid::new_v4().to_string(),
    });
    jsonwebtoken::encode(&header, &claims, &encoding).map_err(|e| format!("signing: {e}"))
}

fn jwk_to_pem(jwk: &str, alg: &str) -> Result<String, String> {
    use base64::Engine as _;
    use p384::pkcs8::EncodePrivateKey;
    let v: Value = serde_json::from_str(jwk).map_err(|_| "private key is neither PEM nor a JWK".to_string())?;
    if alg != "ES384" || v.get("kty").and_then(Value::as_str) != Some("EC") {
        return Err("a JWK private key is supported for ES384 (P-384) only; give RS384 keys as PEM".into());
    }
    if v.get("crv").and_then(Value::as_str) != Some("P-384") {
        return Err("ES384 needs a P-384 key".into());
    }
    let d = v
        .get("d")
        .and_then(Value::as_str)
        .ok_or_else(|| "the JWK has no private part (d)".to_string())?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(d.trim_end_matches('='))
        .map_err(|_| "JWK d is not base64url".to_string())?;
    let sk = p384::SecretKey::from_slice(&raw).map_err(|_| "JWK d is not a P-384 scalar".to_string())?;
    sk.to_pkcs8_pem(p384::pkcs8::LineEnding::LF)
        .map(|z| z.to_string())
        .map_err(|e| format!("encoding the key: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_and_display() {
        let a = Url::parse("https://h.example.org/fhir?x=1#f").unwrap();
        let b = Url::parse("https://H.example.org:443/other").unwrap();
        let c = Url::parse("http://h.example.org/fhir").unwrap();
        assert!(same_origin(&a, &b));
        assert!(!same_origin(&a, &c));
        assert_eq!(display_url(&a), "https://h.example.org/fhir");
    }

    #[test]
    fn outcome_detail_reads_diagnostics() {
        let body = br#"{"resourceType":"OperationOutcome","issue":[{"diagnostics":"bad thing"}]}"#;
        assert_eq!(outcome_detail(body), ": bad thing");
        let body = br#"{"issue":[{"details":{"text":"t"}}]}"#;
        assert_eq!(outcome_detail(body), ": t");
        assert_eq!(outcome_detail(b"nope"), "");
        assert_eq!(outcome_detail(b"{}"), "");
    }

    #[tokio::test]
    async fn ssrf_refusals_before_any_connection() {
        let http = GuardedHttp::new(OutboundPolicy::strict(), HttpSettings::default());
        for bad in [
            "http://example.org/fhir",
            "https://127.0.0.1/fhir",
            "https://169.254.169.254/latest/meta-data",
            "https://[::1]/",
            "https://metadata.google.internal/computeMetadata/v1/",
            "https://u:p@example.org/",
        ] {
            let u = Url::parse(bad).unwrap();
            assert!(
                matches!(http.client_for(&u).await, Err(HttpError::Denied { .. })),
                "{bad}"
            );
        }
        // A name that resolves to loopback is refused after DNS resolution.
        let u = Url::parse("https://localhost:9/").unwrap();
        assert!(matches!(
            http.client_for(&u).await,
            Err(HttpError::Denied { .. }) | Err(HttpError::Transport { .. })
        ));
        // Allow-listed loopback over http is permitted (tests, internal peers).
        let allowed = GuardedHttp::new(OutboundPolicy::parse("127.0.0.1"), HttpSettings::default());
        assert!(allowed.client_for(&Url::parse("http://127.0.0.1:9/").unwrap()).await.is_ok());
    }

    #[test]
    fn client_links_stay_on_origin() {
        let http = Arc::new(GuardedHttp::new(OutboundPolicy::strict(), HttpSettings::default()));
        let c = FhirClient::new(http.clone(), "https://fhir.example.org/base/", ResolvedAuth::None, BTreeMap::new(), None)
            .unwrap();
        assert_eq!(c.url("Patient?_count=2").as_str(), "https://fhir.example.org/base/Patient?_count=2");
        assert!(c.check_link("https://fhir.example.org/base?_getpages=x").is_ok());
        assert!(c.check_link("?page=2").is_ok());
        assert!(matches!(c.check_link("https://evil.example.org/x"), Err(HttpError::Denied { .. })));
        assert!(FhirClient::new(http, "http://fhir.example.org", ResolvedAuth::None, BTreeMap::new(), None).is_err());
    }

    #[test]
    fn assertions() {
        use p384::pkcs8::EncodePrivateKey;
        let sk = loop {
            let mut raw = [0u8; 48];
            rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut raw);
            if let Ok(k) = p384::SecretKey::from_slice(&raw) {
                break k;
            }
        };
        let pem = sk.to_pkcs8_pem(p384::pkcs8::LineEnding::LF).unwrap().to_string();
        let jwt = client_assertion("cid", "https://t.example.org/token", pem.as_bytes(), "ES384", Some("k"))
            .unwrap();
        assert_eq!(jwt.split('.').count(), 3);
        // Same key as a JWK.
        use base64::Engine as _;
        let d = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sk.to_bytes());
        let jwk = serde_json::json!({"kty": "EC", "crv": "P-384", "d": d}).to_string();
        assert!(client_assertion("cid", "https://t", jwk.as_bytes(), "ES384", None).is_ok());
        assert!(client_assertion("cid", "https://t", jwk.as_bytes(), "RS384", None).is_err());
        assert!(client_assertion("cid", "https://t", b"garbage", "ES384", None).is_err());
        assert!(client_assertion("cid", "https://t", pem.as_bytes(), "HS256", None).is_err());
        let no_d = serde_json::json!({"kty": "EC", "crv": "P-384"}).to_string();
        assert!(client_assertion("cid", "https://t", no_d.as_bytes(), "ES384", None).is_err());
        let p256 = serde_json::json!({"kty": "EC", "crv": "P-256", "d": "AA"}).to_string();
        assert!(client_assertion("cid", "https://t", p256.as_bytes(), "ES384", None).is_err());
    }
}
