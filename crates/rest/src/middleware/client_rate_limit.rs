//! Per-client request rate limiting for the FHIR API.
//!
//! A token bucket per caller: an authenticated request is billed to its
//! OAuth client (`client_id`/`azp`, else issuer + subject), an anonymous one
//! to its client address. The address is the TCP peer, or — when the peer is
//! a trusted reverse proxy — the right-most untrusted address in
//! `X-Forwarded-For`, so a client cannot dodge the limit by prepending fake
//! hops.
//!
//! Off unless configured:
//!
//! | Variable | Meaning |
//! |---|---|
//! | `HFS_RATE_LIMIT_ANONYMOUS` | requests per minute per client address without credentials (0 = off) |
//! | `HFS_RATE_LIMIT_AUTHENTICATED` | requests per minute per OAuth client (0 = off) |
//! | `HFS_RATE_LIMIT_BURST_SECONDS` | how many seconds of budget a quiet client may spend at once (default 10) |
//! | `HFS_RATE_LIMIT_TRUSTED_PROXIES` | CIDRs whose `X-Forwarded-For` is believed (default loopback + private ranges; `none` for none) |
//!
//! A rejected request gets `429` with an `OperationOutcome` (`throttled`)
//! and `Retry-After`. Health probes are never limited, and neither is the
//! administrator credential.
//!
//! **Per endpoint** (FHIR server manager, design §5.2): the per-endpoint auth
//! layer stores a [`RateBudget`] in the request extensions — the endpoint's
//! `rateLimit` section, or the `HFS_RATE_LIMIT_*` values when the endpoint
//! inherits them — and [`endpoint_rate_limit_middleware`] bills the request
//! to `(endpoint, client)`: each endpoint has its own buckets, so a client
//! exhausting one endpoint's budget is not throttled on another. Buckets of
//! an endpoint that no longer exists are dropped
//! ([`ClientRateLimiter::retain_endpoints`]). Requests that carry no budget
//! (no manager) are billed exactly as before, with the limiter's own
//! configuration.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::{
    Json,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use helios_admin_auth::IpNet;
use helios_auth::Principal;

/// Rate-limit settings. See the module documentation.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Requests per minute per anonymous client address; 0 disables.
    pub anonymous_per_minute: u32,
    /// Requests per minute per authenticated client; 0 disables.
    pub authenticated_per_minute: u32,
    /// Seconds of budget a bucket holds (the burst size).
    pub burst_seconds: u32,
    /// Proxies whose `X-Forwarded-For` is believed.
    pub trusted_proxies: Vec<IpNet>,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            anonymous_per_minute: 0,
            authenticated_per_minute: 0,
            burst_seconds: 10,
            trusted_proxies: IpNet::default_trusted(),
        }
    }
}

impl RateLimitConfig {
    /// Reads `HFS_RATE_LIMIT_*`.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Reads the settings through `get` (testable without the process env).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let num = |k: &str, default: u32| {
            get(k)
                .and_then(|v| v.trim().parse::<u32>().ok())
                .unwrap_or(default)
        };
        let trusted_proxies = match get("HFS_RATE_LIMIT_TRUSTED_PROXIES") {
            Some(raw) if raw.trim().eq_ignore_ascii_case("none") => Vec::new(),
            Some(raw) if !raw.trim().is_empty() => raw
                .split([',', ' '])
                .filter(|s| !s.trim().is_empty())
                .filter_map(|s| {
                    let parsed = IpNet::parse(s.trim());
                    if parsed.is_none() {
                        tracing::warn!(value = s, "HFS_RATE_LIMIT_TRUSTED_PROXIES: not a CIDR");
                    }
                    parsed
                })
                .collect(),
            _ => IpNet::default_trusted(),
        };
        Self {
            anonymous_per_minute: num("HFS_RATE_LIMIT_ANONYMOUS", 0),
            authenticated_per_minute: num("HFS_RATE_LIMIT_AUTHENTICATED", 0),
            burst_seconds: num("HFS_RATE_LIMIT_BURST_SECONDS", 10).max(1),
            trusted_proxies,
        }
    }

    /// Whether any limit is on.
    pub fn enabled(&self) -> bool {
        self.anonymous_per_minute > 0 || self.authenticated_per_minute > 0
    }
}

/// The limits that apply to one request (see the module documentation).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RateBudget {
    /// Prefix of the bucket keys, `ep:{endpoint id}|` (empty without an
    /// endpoint, which keeps the keys of the environment configuration).
    pub key_prefix: String,
    /// Requests / minute / anonymous client address (0 = unlimited).
    pub anonymous_per_minute: u32,
    /// Requests / minute / authenticated client (0 = unlimited).
    pub authenticated_per_minute: u32,
    /// Seconds of budget a bucket holds.
    pub burst_seconds: u32,
}

impl RateBudget {
    /// The budget of `config` (no endpoint prefix).
    pub fn of(config: &RateLimitConfig) -> Self {
        Self {
            key_prefix: String::new(),
            anonymous_per_minute: config.anonymous_per_minute,
            authenticated_per_minute: config.authenticated_per_minute,
            burst_seconds: config.burst_seconds.max(1),
        }
    }

    /// The bucket-key prefix of an endpoint id.
    pub fn endpoint_prefix(endpoint_id: &str) -> String {
        format!("ep:{endpoint_id}|")
    }

    /// Whether any limit is on.
    pub fn enabled(&self) -> bool {
        self.anonymous_per_minute > 0 || self.authenticated_per_minute > 0
    }
}

struct Bucket {
    tokens: f64,
    updated: Instant,
}

/// Shared limiter state (one per server).
pub struct ClientRateLimiter {
    config: RateLimitConfig,
    buckets: Mutex<HashMap<String, Bucket>>,
}

/// Buckets are pruned of idle (full) entries once the map grows past this.
const PRUNE_AT: usize = 50_000;

impl ClientRateLimiter {
    /// A limiter with `config`.
    pub fn new(config: RateLimitConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            buckets: Mutex::new(HashMap::new()),
        })
    }

    /// Takes one token for `key` at `per_minute`; `Err(seconds)` until the
    /// bucket has one again.
    #[cfg(test)]
    fn take(&self, key: &str, per_minute: u32, now: Instant) -> Result<(), u64> {
        self.take_with(key, per_minute, self.config.burst_seconds, now)
    }

    /// [`take`](Self::take) with an explicit burst.
    fn take_with(
        &self,
        key: &str,
        per_minute: u32,
        burst_seconds: u32,
        now: Instant,
    ) -> Result<(), u64> {
        let rate = per_minute as f64 / 60.0;
        let capacity = (rate * burst_seconds.max(1) as f64).max(1.0);
        let mut buckets = self.buckets.lock().unwrap_or_else(|p| p.into_inner());
        if buckets.len() > PRUNE_AT {
            buckets.retain(|_, b| {
                b.tokens + now.duration_since(b.updated).as_secs_f64() * rate < capacity
            });
        }
        let bucket = buckets.entry(key.to_string()).or_insert(Bucket {
            tokens: capacity,
            updated: now,
        });
        let elapsed = now.duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(capacity);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - bucket.tokens) / rate).ceil().max(1.0) as u64)
        }
    }
}

impl ClientRateLimiter {
    /// The limiter's own configuration (`HFS_RATE_LIMIT_*`).
    pub fn config(&self) -> &RateLimitConfig {
        &self.config
    }

    /// Drops the buckets of endpoints not in `live` (endpoint ids). Buckets
    /// without an endpoint prefix are kept.
    pub fn retain_endpoints(&self, live: &std::collections::HashSet<String>) {
        let mut buckets = self.buckets.lock().unwrap_or_else(|p| p.into_inner());
        buckets.retain(|key, _| match key.strip_prefix("ep:") {
            Some(rest) => rest
                .split_once('|')
                .is_some_and(|(id, _)| live.contains(id)),
            None => true,
        });
    }

    /// Number of buckets (tests, diagnostics).
    pub fn bucket_count(&self) -> usize {
        self.buckets.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}

/// The client address: the peer, or through trusted proxies the right-most
/// untrusted `X-Forwarded-For` hop.
pub fn client_ip(headers: &HeaderMap, peer: Option<IpAddr>, trusted: &[IpNet]) -> Option<IpAddr> {
    let peer = peer?;
    if !trusted.iter().any(|net| net.contains(&peer)) {
        return Some(peer);
    }
    let hops: Vec<IpAddr> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|s| s.trim().parse::<IpAddr>().ok())
        .collect();
    for hop in hops.iter().rev() {
        if !trusted.iter().any(|net| net.contains(hop)) {
            return Some(*hop);
        }
    }
    hops.first().copied().or(Some(peer))
}

fn exempt(path: &str) -> bool {
    matches!(
        path.trim_end_matches('/'),
        "/health" | "/_liveness" | "/_readiness" | "/.well-known/smart-configuration"
    )
}

/// The rate-limit middleware. Runs after authentication, so the principal
/// (when any) is known. Uses the limiter's own configuration.
pub async fn client_rate_limit_middleware(
    State(limiter): State<Arc<ClientRateLimiter>>,
    request: Request,
    next: Next,
) -> Response {
    let budget = RateBudget::of(&limiter.config);
    limit(&limiter, &budget, request, next).await
}

/// The per-endpoint rate-limit middleware: the request's [`RateBudget`]
/// (stored by the per-endpoint auth layer), else the limiter's own
/// configuration.
pub async fn endpoint_rate_limit_middleware(
    State(limiter): State<Arc<ClientRateLimiter>>,
    request: Request,
    next: Next,
) -> Response {
    let budget = request
        .extensions()
        .get::<RateBudget>()
        .cloned()
        .unwrap_or_else(|| RateBudget::of(&limiter.config));
    limit(&limiter, &budget, request, next).await
}

async fn limit(
    limiter: &ClientRateLimiter,
    budget: &RateBudget,
    request: Request,
    next: Next,
) -> Response {
    match check(limiter, budget, &request) {
        Some(refusal) => refusal,
        None => next.run(request).await,
    }
}

/// Bills `request` to its bucket under `budget`: `None` when it may proceed,
/// the `429` otherwise. Health probes and the administrator credential are
/// never limited.
pub(crate) fn check(
    limiter: &ClientRateLimiter,
    budget: &RateBudget,
    request: &Request,
) -> Option<Response> {
    if !budget.enabled() || exempt(request.uri().path()) {
        return None;
    }
    let config = &limiter.config;
    let (key, per_minute) = match request.extensions().get::<Principal>() {
        Some(p) if p.issuer == crate::middleware::auth::ADMIN_ISSUER => return None,
        Some(p) => {
            let who = p
                .client_id()
                .map(str::to_string)
                .unwrap_or_else(|| format!("{}|{}", p.issuer, p.subject));
            (
                format!("{}client:{who}", budget.key_prefix),
                budget.authenticated_per_minute,
            )
        }
        None => {
            let peer = request
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip());
            let ip = client_ip(request.headers(), peer, &config.trusted_proxies);
            (
                format!(
                    "{}ip:{}",
                    budget.key_prefix,
                    ip.map(|ip| ip.to_string()).unwrap_or_default()
                ),
                budget.anonymous_per_minute,
            )
        }
    };
    if per_minute == 0 {
        return None;
    }
    let retry_after = limiter
        .take_with(&key, per_minute, budget.burst_seconds, Instant::now())
        .err()?;
    tracing::warn!(key = %key, retry_after, "Rate limit exceeded");
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(serde_json::json!({
            "resourceType": "OperationOutcome",
            "issue": [{
                "severity": "error",
                "code": "throttled",
                "diagnostics": format!(
                    "rate limit of {per_minute} requests per minute exceeded; retry in {retry_after} s"
                ),
            }]
        })),
    )
        .into_response();
    if let Ok(v) = HeaderValue::from_str(&retry_after.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, v);
    }
    Some(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn limiter(burst: u32) -> Arc<ClientRateLimiter> {
        ClientRateLimiter::new(RateLimitConfig {
            anonymous_per_minute: 60,
            authenticated_per_minute: 60,
            burst_seconds: burst,
            trusted_proxies: IpNet::default_trusted(),
        })
    }

    #[test]
    fn a_bucket_allows_its_burst_then_refills() {
        let l = limiter(5);
        let t0 = Instant::now();
        for _ in 0..5 {
            assert!(l.take("a", 60, t0).is_ok());
        }
        assert_eq!(l.take("a", 60, t0), Err(1));
        // Other keys are independent.
        assert!(l.take("b", 60, t0).is_ok());
        // One token per second at 60/min.
        assert!(l.take("a", 60, t0 + Duration::from_millis(1001)).is_ok());
        assert!(l.take("a", 60, t0 + Duration::from_millis(1001)).is_err());
    }

    #[test]
    fn forwarded_for_is_believed_only_from_trusted_proxies() {
        let trusted = IpNet::default_trusted();
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "6.6.6.6, 203.0.113.9".parse().unwrap());
        // Through a private-range proxy: the right-most untrusted hop.
        assert_eq!(
            client_ip(&headers, Some("10.0.0.2".parse().unwrap()), &trusted),
            Some("203.0.113.9".parse().unwrap())
        );
        // From a public peer: the peer itself, the header is ignored.
        assert_eq!(
            client_ip(&headers, Some("198.51.100.1".parse().unwrap()), &trusted),
            Some("198.51.100.1".parse().unwrap())
        );
    }

    #[test]
    fn endpoint_buckets_are_separate_and_pruned_with_their_endpoint() {
        let l = limiter(1);
        let t0 = Instant::now();
        let a = format!("{}ip:1.2.3.4", RateBudget::endpoint_prefix("A"));
        let b = format!("{}ip:1.2.3.4", RateBudget::endpoint_prefix("B"));
        assert!(l.take_with(&a, 60, 1, t0).is_ok());
        assert!(l.take_with(&a, 60, 1, t0).is_err(), "A's bucket is empty");
        assert!(l.take_with(&b, 60, 1, t0).is_ok(), "B has its own bucket");
        assert!(l.take("ip:1.2.3.4", 60, t0).is_ok(), "env bucket untouched");
        assert_eq!(l.bucket_count(), 3);
        l.retain_endpoints(&std::collections::HashSet::from(["B".to_string()]));
        assert_eq!(l.bucket_count(), 2, "A's buckets dropped, env kept");
        // A larger burst holds more.
        let c = "ep:C|ip:x";
        for _ in 0..30 {
            assert!(l.take_with(c, 60, 30, t0).is_ok());
        }
        assert!(l.take_with(c, 60, 30, t0).is_err());
    }

    #[test]
    fn budgets_follow_the_config() {
        let c = RateLimitConfig {
            anonymous_per_minute: 5,
            ..RateLimitConfig::default()
        };
        let b = RateBudget::of(&c);
        assert!(b.enabled());
        assert_eq!(b.key_prefix, "");
        assert_eq!(b.burst_seconds, 10);
        assert!(!RateBudget::default().enabled());
        assert_eq!(RateBudget::endpoint_prefix("x"), "ep:x|");
    }

    #[test]
    fn config_reads_the_environment_names() {
        let c = RateLimitConfig::from_lookup(|k| match k {
            "HFS_RATE_LIMIT_ANONYMOUS" => Some("120".into()),
            "HFS_RATE_LIMIT_TRUSTED_PROXIES" => Some("none".into()),
            _ => None,
        });
        assert_eq!(c.anonymous_per_minute, 120);
        assert_eq!(c.authenticated_per_minute, 0);
        assert!(c.trusted_proxies.is_empty());
        assert!(c.enabled());
        assert!(!RateLimitConfig::default().enabled());
    }
}
