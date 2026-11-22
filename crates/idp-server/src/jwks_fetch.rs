//! Fetching client JWK Sets (`private_key_jwt` with `jwks_uri`, SMART
//! Backend Services) through the outbound SSRF guard (design §8.4, §10.4):
//! https only (unless allow-listed), resolved addresses checked against
//! internal ranges and pinned for the connection (no DNS rebinding),
//! no redirects, 5 s timeout, 64 KiB cap, Cache-Control-bounded caching.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use helios_endpoints::OutboundPolicy;
use serde_json::Value;

/// Largest JWKS document accepted.
pub const MAX_JWKS_BYTES: usize = 64 * 1024;
/// Default and maximum cache lifetime.
pub const DEFAULT_TTL: Duration = Duration::from_secs(300);
pub const MAX_TTL: Duration = Duration::from_secs(3600);
/// A forced refetch (unknown `kid`, client key rotation) happens at most
/// this often per URL.
pub const MIN_REFETCH: Duration = Duration::from_secs(10);

/// A fetched JWKS and how long it may be cached.
#[derive(Debug, Clone)]
pub struct FetchedJwks {
    pub jwks: Value,
    pub ttl: Duration,
}

/// Fetches a JWK Set (the HTTP implementation, or a test double).
#[async_trait]
pub trait JwksFetcher: Send + Sync + 'static {
    async fn fetch(&self, url: &str) -> Result<FetchedJwks, String>;
}

/// HTTP fetcher with the outbound guard.
pub struct HttpJwksFetcher {
    policy: Arc<OutboundPolicy>,
}

impl HttpJwksFetcher {
    pub fn new(policy: Arc<OutboundPolicy>) -> Self {
        Self { policy }
    }
}

fn max_age(h: &reqwest::header::HeaderMap) -> Duration {
    let Some(cc) = h
        .get(reqwest::header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
    else {
        return DEFAULT_TTL;
    };
    let mut ttl = DEFAULT_TTL;
    for d in cc.split(',').map(str::trim) {
        let dl = d.to_ascii_lowercase();
        if dl == "no-store" || dl == "no-cache" {
            return Duration::ZERO;
        }
        if let Some(v) = dl.strip_prefix("max-age=")
            && let Ok(n) = v.trim_matches('"').parse::<u64>()
        {
            ttl = Duration::from_secs(n);
        }
    }
    ttl.min(MAX_TTL)
}

#[async_trait]
impl JwksFetcher for HttpJwksFetcher {
    async fn fetch(&self, raw: &str) -> Result<FetchedJwks, String> {
        let url = self.policy.check_url(raw).map_err(|e| e.to_string())?;
        let host = url.host_str().ok_or("no host")?.to_string();
        let port = url.port_or_known_default().ok_or("no port")?;
        let bare = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        // Resolve once, check every address, pin the first allowed one.
        let addrs: Vec<std::net::SocketAddr> = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::lookup_host((bare.as_str(), port)),
        )
        .await
        .map_err(|_| "DNS timeout".to_string())?
        .map_err(|e| format!("DNS: {e}"))?
        .collect();
        if addrs.is_empty() {
            return Err("host does not resolve".into());
        }
        for a in &addrs {
            self.policy
                .check_ip(&bare, a.ip())
                .map_err(|e| e.to_string())?;
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(3))
            .resolve(&bare, addrs[0])
            .build()
            .map_err(|e| e.to_string())?;
        let mut resp = client
            .get(url)
            .header(
                reqwest::header::ACCEPT,
                "application/json, application/jwk-set+json",
            )
            .send()
            .await
            .map_err(|e| format!("fetch failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let ttl = max_age(resp.headers());
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_JWKS_BYTES {
                return Err("JWKS too large".into());
            }
        }
        let jwks: Value = serde_json::from_slice(&body).map_err(|_| "JWKS is not JSON")?;
        if !jwks.get("keys").is_some_and(Value::is_array) {
            return Err("not a JWK Set".into());
        }
        Ok(FetchedJwks { jwks, ttl })
    }
}

/// Per-URL cache in front of a fetcher.
pub(crate) struct JwksCache {
    fetcher: Arc<dyn JwksFetcher>,
    min_refetch: Duration,
    cache: Mutex<HashMap<String, (Value, Instant, Duration)>>,
}

impl JwksCache {
    pub fn new(fetcher: Arc<dyn JwksFetcher>, min_refetch: Duration) -> Self {
        Self {
            fetcher,
            min_refetch,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The keys at `url`. `refresh` asks for a refetch (unknown `kid`),
    /// honoured once per [`MIN_REFETCH`].
    pub async fn keys(&self, url: &str, refresh: bool) -> Result<Vec<Value>, String> {
        {
            let c = self.cache.lock().expect("poisoned");
            if let Some((v, at, ttl)) = c.get(url) {
                let fresh = at.elapsed() < *ttl;
                let may_refetch = at.elapsed() >= self.min_refetch;
                if fresh && !(refresh && may_refetch) {
                    return Ok(keys_of(v));
                }
                if refresh && !may_refetch {
                    return Ok(keys_of(v));
                }
            }
        }
        let f = self.fetcher.fetch(url).await?;
        let keys = keys_of(&f.jwks);
        let mut c = self.cache.lock().expect("poisoned");
        if c.len() > 1000 {
            c.clear();
        }
        c.insert(url.to_string(), (f.jwks, Instant::now(), f.ttl));
        Ok(keys)
    }
}

fn keys_of(v: &Value) -> Vec<Value> {
    v.get("keys")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_control_bounds() {
        let mut h = reqwest::header::HeaderMap::new();
        assert_eq!(max_age(&h), DEFAULT_TTL);
        h.insert(
            reqwest::header::CACHE_CONTROL,
            "public, max-age=60".parse().unwrap(),
        );
        assert_eq!(max_age(&h), Duration::from_secs(60));
        h.insert(
            reqwest::header::CACHE_CONTROL,
            "max-age=999999".parse().unwrap(),
        );
        assert_eq!(max_age(&h), MAX_TTL);
        h.insert(reqwest::header::CACHE_CONTROL, "no-store".parse().unwrap());
        assert_eq!(max_age(&h), Duration::ZERO);
    }

    #[tokio::test]
    async fn guard_refuses_internal_and_http() {
        let f = HttpJwksFetcher::new(Arc::new(OutboundPolicy::strict()));
        assert!(f.fetch("http://example.org/jwks").await.is_err());
        assert!(f.fetch("https://127.0.0.1/jwks").await.is_err());
        assert!(f.fetch("https://169.254.169.254/latest").await.is_err());
        assert!(f.fetch("https://metadata.google.internal/x").await.is_err());
        assert!(f.fetch("https://user:pw@example.org/jwks").await.is_err());
        assert!(f.fetch("https://localhost/jwks").await.is_err());
    }
}
