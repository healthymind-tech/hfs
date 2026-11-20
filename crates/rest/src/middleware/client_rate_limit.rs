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
    fn take(&self, key: &str, per_minute: u32, now: Instant) -> Result<(), u64> {
        let rate = per_minute as f64 / 60.0;
        let capacity = (rate * self.config.burst_seconds as f64).max(1.0);
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
/// (when any) is known.
pub async fn client_rate_limit_middleware(
    State(limiter): State<Arc<ClientRateLimiter>>,
    request: Request,
    next: Next,
) -> Response {
    if exempt(request.uri().path()) {
        return next.run(request).await;
    }
    let config = &limiter.config;
    let (key, per_minute) = match request.extensions().get::<Principal>() {
        Some(p) if p.issuer == crate::middleware::auth::ADMIN_ISSUER => {
            return next.run(request).await;
        }
        Some(p) => {
            let who = p
                .client_id()
                .map(str::to_string)
                .unwrap_or_else(|| format!("{}|{}", p.issuer, p.subject));
            (format!("client:{who}"), config.authenticated_per_minute)
        }
        None => {
            let peer = request
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip());
            let ip = client_ip(request.headers(), peer, &config.trusted_proxies);
            (
                format!("ip:{}", ip.map(|ip| ip.to_string()).unwrap_or_default()),
                config.anonymous_per_minute,
            )
        }
    };
    if per_minute == 0 {
        return next.run(request).await;
    }
    match limiter.take(&key, per_minute, Instant::now()) {
        Ok(()) => next.run(request).await,
        Err(retry_after) => {
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
            response
        }
    }
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

#[cfg(test)]
#[path = "client_rate_limit_hm_tests.rs"]
mod hm_tests;
