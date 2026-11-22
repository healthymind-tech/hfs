//! Rate limits of the authorization server (design §10.2).
//!
//! Token buckets per realm × (IP or client), per instance (cluster-
//! approximate; the lockout of `lockout.rs` is the exact, cluster-wide
//! control). Defaults:
//!
//! | route (under `/idp/{realm}`) | limit |
//! |---|---|
//! | `POST /token` | 60/min per client, 300/min per IP |
//! | `/authorize`, `POST /login` (and the other hosted forms) | 20/min per IP |
//! | `POST /introspect` | 600/min per client |
//! | `POST /revoke` | 60/min per client |
//!
//! A refused request gets `429` with `Retry-After` (seconds). The limiter is
//! an axum middleware ([`layer`]) wrapped around the whole `/idp` router by
//! the `hfs` wiring, so the protocol router needs no code of its own.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;

/// The caller address as resolved by the limiter (trusted proxy aware),
/// stored in the request extensions for the handlers and audit events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerIp(pub String);

/// Per-minute budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitConfig {
    pub token_per_client: u32,
    pub token_per_ip: u32,
    pub interactive_per_ip: u32,
    pub introspect_per_client: u32,
    pub revoke_per_client: u32,
}

impl Default for LimitConfig {
    fn default() -> Self {
        Self {
            token_per_client: 60,
            token_per_ip: 300,
            interactive_per_ip: 20,
            introspect_per_client: 600,
            revoke_per_client: 60,
        }
    }
}

impl LimitConfig {
    /// Defaults overridden by `HFS_IDP_RATE_{TOKEN_CLIENT,TOKEN_IP,
    /// INTERACTIVE_IP,INTROSPECT_CLIENT,REVOKE_CLIENT}` (per minute; 0 =
    /// unlimited).
    pub fn from_env() -> Self {
        let get = |k: &str, d: u32| {
            std::env::var(format!("HFS_IDP_RATE_{k}"))
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(d)
        };
        let d = Self::default();
        Self {
            token_per_client: get("TOKEN_CLIENT", d.token_per_client),
            token_per_ip: get("TOKEN_IP", d.token_per_ip),
            interactive_per_ip: get("INTERACTIVE_IP", d.interactive_per_ip),
            introspect_per_client: get("INTROSPECT_CLIENT", d.introspect_per_client),
            revoke_per_client: get("REVOKE_CLIENT", d.revoke_per_client),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

/// The token buckets.
#[derive(Debug)]
pub struct RateLimiter {
    config: LimitConfig,
    buckets: Mutex<HashMap<String, Bucket>>,
    trust_forwarded: bool,
}

impl RateLimiter {
    /// A limiter. `trust_forwarded`: take the caller IP from the first
    /// `X-Forwarded-For` / `X-Real-IP` value (set by nginx in front of HFS).
    pub fn new(config: LimitConfig, trust_forwarded: bool) -> Arc<Self> {
        Arc::new(Self {
            config,
            buckets: Mutex::new(HashMap::new()),
            trust_forwarded,
        })
    }

    pub fn config(&self) -> LimitConfig {
        self.config
    }

    /// Takes one token from `key`'s bucket of `per_minute`; `Err(seconds)`
    /// until a token is available.
    pub fn take(&self, key: &str, per_minute: u32, now: Instant) -> Result<(), u64> {
        if per_minute == 0 {
            return Ok(());
        }
        let cap = per_minute as f64;
        let rate = cap / 60.0;
        let mut map = self.buckets.lock().unwrap_or_else(|p| p.into_inner());
        if map.len() > 100_000 {
            // Bound memory under address-spraying: drop full buckets.
            map.retain(|_, b| now.duration_since(b.last).as_secs() < 120);
        }
        let b = map.entry(key.to_string()).or_insert(Bucket {
            tokens: cap,
            last: now,
        });
        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * rate).min(cap);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - b.tokens) / rate).ceil().max(1.0) as u64)
        }
    }

    fn client_ip(&self, req: &Request) -> String {
        if self.trust_forwarded && let Some(ip) = crate::limits::forwarded_ip(req.headers()) {
            return ip;
        }
        req.extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip().to_string())
            .unwrap_or_else(|| "unknown".into())
    }
}

/// The caller address set by the trusted reverse proxy: `X-Real-IP`
/// (nginx overwrites it with `$remote_addr`), else the **last**
/// `X-Forwarded-For` hop (the one the proxy appended — earlier hops are
/// client-controlled and never trusted).
pub fn forwarded_ip(h: &axum::http::HeaderMap) -> Option<String> {
    let valid = |s: &str| s.parse::<std::net::IpAddr>().ok().map(|ip| ip.to_string());
    if let Some(v) = h.get("x-real-ip").and_then(|v| v.to_str().ok())
        && let Some(ip) = valid(v.trim())
    {
        return Some(ip);
    }
    h.get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .last()
        .and_then(valid)
}

/// The route class of a request path under `/idp/{realm}/…`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Token,
    Interactive,
    Introspect,
    Revoke,
    Other,
}

fn classify(method: &Method, path: &str) -> Option<(String, Class)> {
    let rest = path.strip_prefix("/idp/")?;
    let (realm, tail) = rest.split_once('/').unwrap_or((rest, ""));
    let class = match (method, tail) {
        (&Method::POST, "token") => Class::Token,
        (&Method::POST, "introspect") => Class::Introspect,
        (&Method::POST, "revoke") => Class::Revoke,
        (_, "authorize") => Class::Interactive,
        (&Method::POST, "login" | "consent" | "select-patient" | "logout") => Class::Interactive,
        _ => Class::Other,
    };
    Some((realm.to_string(), class))
}

/// The client id of a token-endpoint request when it can be read without
/// consuming the body: HTTP Basic credentials. (Form-posted `client_id`
/// counts under the IP bucket only; Basic is what confidential clients use.)
fn basic_client(req: &Request) -> Option<String> {
    let v = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, cred) = v.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(cred.trim())
        .ok()?;
    let s = String::from_utf8(raw).ok()?;
    let id = s.split_once(':').map(|(a, _)| a).unwrap_or(&s);
    Some(id.chars().take(200).collect())
}

fn too_many(retry_after: u64) -> Response {
    let mut r = (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::CONTENT_TYPE, "application/json")],
        Body::from(
            r#"{"error":"slow_down","error_description":"Too many requests; retry later."}"#,
        ),
    )
        .into_response();
    if let Ok(v) = HeaderValue::from_str(&retry_after.to_string()) {
        r.headers_mut().insert(header::RETRY_AFTER, v);
    }
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// The middleware: `axum::middleware::from_fn_with_state(limiter, layer)`.
pub async fn layer(
    State(limiter): State<Arc<RateLimiter>>,
    mut req: Request,
    next: Next,
) -> Response {
    let ip = limiter.client_ip(&req);
    req.extensions_mut().insert(CallerIp(ip.clone()));
    let Some((realm, class)) = classify(req.method(), req.uri().path()) else {
        return next.run(req).await;
    };
    let now = Instant::now();
    let c = limiter.config;
    let mut checks: Vec<(String, u32)> = Vec::new();
    match class {
        Class::Token => {
            checks.push((format!("{realm}|token|ip|{ip}"), c.token_per_ip));
            if let Some(client) = basic_client(&req) {
                checks.push((format!("{realm}|token|client|{client}"), c.token_per_client));
            }
        }
        Class::Interactive => checks.push((format!("{realm}|ui|ip|{ip}"), c.interactive_per_ip)),
        Class::Introspect => {
            let who = basic_client(&req).unwrap_or_else(|| format!("ip:{ip}"));
            checks.push((format!("{realm}|introspect|{who}"), c.introspect_per_client));
        }
        Class::Revoke => {
            let who = basic_client(&req).unwrap_or_else(|| format!("ip:{ip}"));
            checks.push((format!("{realm}|revoke|{who}"), c.revoke_per_client));
        }
        Class::Other => {}
    }
    for (key, limit) in checks {
        if let Err(after) = limiter.take(&key, limit, now) {
            tracing::warn!(realm = %realm, key = %key, "idp rate limit hit");
            return too_many(after);
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn bucket_refills_over_time() {
        let l = RateLimiter::new(LimitConfig::default(), false);
        let t0 = Instant::now();
        for _ in 0..20 {
            assert!(l.take("k", 20, t0).is_ok());
        }
        let wait = l.take("k", 20, t0).unwrap_err();
        assert_eq!(wait, 3, "20/min ⇒ one token every 3 s");
        assert!(l.take("k", 20, t0 + Duration::from_secs(3)).is_ok());
        assert!(l.take("other", 20, t0).is_ok(), "separate keys");
        assert!(l.take("unlimited", 0, t0).is_ok());
    }

    #[test]
    fn forwarded_address_cannot_be_forged() {
        use axum::http::HeaderMap;
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "6.6.6.6, 203.0.113.5".parse().unwrap());
        assert_eq!(forwarded_ip(&h).as_deref(), Some("203.0.113.5"));
        h.insert("x-real-ip", "198.51.100.1".parse().unwrap());
        assert_eq!(forwarded_ip(&h).as_deref(), Some("198.51.100.1"));
        h.insert("x-real-ip", "not-an-ip".parse().unwrap());
        assert_eq!(forwarded_ip(&h).as_deref(), Some("203.0.113.5"));
        assert_eq!(forwarded_ip(&HeaderMap::new()), None);
    }

    #[test]
    fn classes() {
        assert_eq!(
            classify(&Method::POST, "/idp/clinic/token"),
            Some(("clinic".into(), Class::Token))
        );
        assert_eq!(
            classify(&Method::GET, "/idp/clinic/authorize").map(|x| x.1),
            Some(Class::Interactive)
        );
        assert_eq!(
            classify(&Method::POST, "/idp/clinic/login").map(|x| x.1),
            Some(Class::Interactive)
        );
        assert_eq!(
            classify(&Method::GET, "/idp/clinic/jwks").map(|x| x.1),
            Some(Class::Other)
        );
        assert_eq!(classify(&Method::GET, "/fhir/x"), None);
    }
}
