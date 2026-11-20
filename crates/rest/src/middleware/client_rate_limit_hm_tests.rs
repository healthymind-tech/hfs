//! Unit tests for the per-client rate limiter: configuration parsing,
//! address selection through proxies, bucket arithmetic, and the
//! middleware's decisions (who is billed, what is exempt, the 429 shape).

use super::*;
use axum::{Router, body::Body, routing::get};
use std::time::Duration;
use tower::ServiceExt;

fn cfg(anon: u32, authd: u32) -> RateLimitConfig {
    RateLimitConfig {
        anonymous_per_minute: anon,
        authenticated_per_minute: authd,
        burst_seconds: 1,
        trusted_proxies: IpNet::default_trusted(),
    }
}

fn app(limiter: Arc<ClientRateLimiter>) -> Router {
    Router::new()
        .route("/Patient", get(|| async { "ok" }))
        .route("/health", get(|| async { "up" }))
        .route(
            "/.well-known/smart-configuration",
            get(|| async { "{}" }),
        )
        .layer(axum::middleware::from_fn_with_state(
            limiter,
            client_rate_limit_middleware,
        ))
}

fn req(path: &str, peer: &str, principal: Option<Principal>) -> Request {
    let mut r = Request::builder().uri(path).body(Body::empty()).unwrap();
    r.extensions_mut()
        .insert(ConnectInfo::<SocketAddr>(peer.parse().unwrap()));
    if let Some(p) = principal {
        r.extensions_mut().insert(p);
    }
    r
}

fn principal(issuer: &str, subject: &str, client_id: Option<&str>) -> Principal {
    let mut p = Principal {
        issuer: issuer.into(),
        subject: subject.into(),
        ..Default::default()
    };
    if let Some(c) = client_id {
        p.custom_claims
            .insert("client_id".into(), serde_json::Value::String(c.into()));
    }
    p
}

async fn status(app: &Router, r: Request) -> (StatusCode, HeaderMap, Vec<u8>) {
    let resp = app.clone().oneshot(r).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, headers, body)
}

#[test]
fn config_defaults_and_bad_values() {
    let c = RateLimitConfig::from_lookup(|_| None);
    assert_eq!(c.anonymous_per_minute, 0);
    assert_eq!(c.authenticated_per_minute, 0);
    assert_eq!(c.burst_seconds, 10);
    assert_eq!(c.trusted_proxies.len(), IpNet::default_trusted().len());
    assert!(!c.enabled());

    // Unparsable numbers fall back to the default; burst is at least 1.
    let c = RateLimitConfig::from_lookup(|k| match k {
        "HFS_RATE_LIMIT_ANONYMOUS" => Some("lots".into()),
        "HFS_RATE_LIMIT_AUTHENTICATED" => Some(" 30 ".into()),
        "HFS_RATE_LIMIT_BURST_SECONDS" => Some("0".into()),
        _ => None,
    });
    assert_eq!(c.anonymous_per_minute, 0);
    assert_eq!(c.authenticated_per_minute, 30);
    assert_eq!(c.burst_seconds, 1);
    assert!(c.enabled());
}

#[test]
fn trusted_proxies_list_parsing() {
    // Comma- and space-separated; invalid entries are dropped.
    let c = RateLimitConfig::from_lookup(|k| {
        (k == "HFS_RATE_LIMIT_TRUSTED_PROXIES").then(|| "192.0.2.0/24, bogus 2001:db8::/32".into())
    });
    assert_eq!(c.trusted_proxies.len(), 2);
    assert!(c.trusted_proxies[0].contains(&"192.0.2.77".parse().unwrap()));
    assert!(c.trusted_proxies[1].contains(&"2001:db8::1".parse().unwrap()));
    // `NONE` in any case disables forwarding trust; blank means the default.
    let none = RateLimitConfig::from_lookup(|_| Some(" NONE ".into()));
    assert!(none.trusted_proxies.is_empty());
    let blank = RateLimitConfig::from_lookup(|k| {
        (k == "HFS_RATE_LIMIT_TRUSTED_PROXIES").then(|| "  ".into())
    });
    assert_eq!(
        blank.trusted_proxies.len(),
        IpNet::default_trusted().len()
    );
}

#[test]
fn client_ip_edge_cases() {
    let trusted = IpNet::default_trusted();
    let empty = HeaderMap::new();
    // No peer: unknown.
    assert_eq!(client_ip(&empty, None, &trusted), None);
    // Trusted peer without the header: the peer.
    assert_eq!(
        client_ip(&empty, Some("127.0.0.1".parse().unwrap()), &trusted),
        Some("127.0.0.1".parse().unwrap())
    );
    // Every hop trusted: the left-most (original) one.
    let mut h = HeaderMap::new();
    h.insert("x-forwarded-for", "10.1.1.1, 10.2.2.2".parse().unwrap());
    assert_eq!(
        client_ip(&h, Some("10.0.0.2".parse().unwrap()), &trusted),
        Some("10.1.1.1".parse().unwrap())
    );
    // Several header lines and garbage entries: garbage ignored, right-most
    // untrusted hop wins across lines.
    let mut h = HeaderMap::new();
    h.append("x-forwarded-for", "203.0.113.1, nonsense".parse().unwrap());
    h.append("x-forwarded-for", "198.51.100.7, 10.9.9.9".parse().unwrap());
    assert_eq!(
        client_ip(&h, Some("10.0.0.2".parse().unwrap()), &trusted),
        Some("198.51.100.7".parse().unwrap())
    );
    // Nothing trusted: the header is never believed.
    assert_eq!(
        client_ip(&h, Some("10.0.0.2".parse().unwrap()), &[]),
        Some("10.0.0.2".parse().unwrap())
    );
}

#[test]
fn exempt_paths() {
    for p in [
        "/health",
        "/health/",
        "/_liveness",
        "/_readiness",
        "/.well-known/smart-configuration",
    ] {
        assert!(exempt(p), "{p} should be exempt");
    }
    for p in ["/", "/Patient", "/healthz", "/metadata", "/tenant/health"] {
        assert!(!exempt(p), "{p} should be limited");
    }
}

#[test]
fn bucket_rate_and_retry_after() {
    // 6 per minute = one token every 10 s, burst 1 s → capacity floored at 1.
    let l = ClientRateLimiter::new(cfg(6, 6));
    let t0 = Instant::now();
    assert!(l.take("k", 6, t0).is_ok());
    // Empty bucket: a whole token needs 10 s.
    assert_eq!(l.take("k", 6, t0), Err(10));
    // Half-way: 5 s left.
    assert_eq!(l.take("k", 6, t0 + Duration::from_secs(5)), Err(5));
    assert!(l.take("k", 6, t0 + Duration::from_secs(10)).is_ok());
    // A long idle period does not overfill the bucket beyond capacity.
    let later = t0 + Duration::from_secs(3600);
    assert!(l.take("k", 6, later).is_ok());
    assert!(l.take("k", 6, later).is_err());
}

#[test]
fn bucket_capacity_scales_with_burst_seconds() {
    let l = ClientRateLimiter::new(RateLimitConfig {
        burst_seconds: 30,
        ..cfg(120, 0)
    });
    // 120/min × 30 s = 60 tokens.
    let t0 = Instant::now();
    for i in 0..60 {
        assert!(l.take("k", 120, t0).is_ok(), "token {i}");
    }
    // Retry-After never reports less than one second.
    assert_eq!(l.take("k", 120, t0), Err(1));
}

#[tokio::test]
async fn anonymous_clients_are_limited_per_address_with_a_429() {
    let app = app(ClientRateLimiter::new(cfg(60, 0)));
    let (s, _, _) = status(&app, req("/Patient", "198.51.100.1:5000", None)).await;
    assert_eq!(s, StatusCode::OK);
    let (s, h, body) = status(&app, req("/Patient", "198.51.100.1:5001", None)).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(h.get(header::RETRY_AFTER).unwrap(), "1");
    let oo: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(oo["resourceType"], "OperationOutcome");
    assert_eq!(oo["issue"][0]["code"], "throttled");
    assert!(
        oo["issue"][0]["diagnostics"]
            .as_str()
            .unwrap()
            .contains("60 requests per minute")
    );
    // Another address has its own bucket.
    let (s, _, _) = status(&app, req("/Patient", "198.51.100.2:5000", None)).await;
    assert_eq!(s, StatusCode::OK);
    // Health and SMART discovery are never limited.
    for p in ["/health", "/.well-known/smart-configuration"] {
        let (s, _, _) = status(&app, req(p, "198.51.100.1:5002", None)).await;
        assert_eq!(s, StatusCode::OK, "{p}");
    }
}

#[tokio::test]
async fn forwarded_clients_behind_a_trusted_proxy_get_separate_buckets() {
    let app = app(ClientRateLimiter::new(cfg(60, 0)));
    let with_xff = |xff: &str| {
        let mut r = req("/Patient", "10.0.0.5:443", None);
        r.headers_mut()
            .insert("x-forwarded-for", xff.parse().unwrap());
        r
    };
    assert_eq!(status(&app, with_xff("203.0.113.1")).await.0, StatusCode::OK);
    assert_eq!(status(&app, with_xff("203.0.113.2")).await.0, StatusCode::OK);
    assert_eq!(
        status(&app, with_xff("203.0.113.1")).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    // Prepending a fake hop does not escape the bucket.
    assert_eq!(
        status(&app, with_xff("1.2.3.4, 203.0.113.1")).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn authenticated_clients_are_billed_to_their_oauth_client() {
    let app = app(ClientRateLimiter::new(cfg(0, 60)));
    let a1 = principal("https://idp", "alice", Some("app-1"));
    let b1 = principal("https://idp", "bob", Some("app-1"));
    let a2 = principal("https://idp", "alice", Some("app-2"));
    assert_eq!(
        status(&app, req("/Patient", "198.51.100.1:1", Some(a1))).await.0,
        StatusCode::OK
    );
    // Same client_id, different user, different address: same bucket.
    assert_eq!(
        status(&app, req("/Patient", "198.51.100.9:1", Some(b1))).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        status(&app, req("/Patient", "198.51.100.1:1", Some(a2))).await.0,
        StatusCode::OK
    );
    // Without client_id/azp the issuer + subject is the key.
    let s1 = principal("https://idp", "svc", None);
    let s2 = principal("https://other", "svc", None);
    assert_eq!(
        status(&app, req("/Patient", "198.51.100.1:1", Some(s1.clone())))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        status(&app, req("/Patient", "198.51.100.1:1", Some(s2))).await.0,
        StatusCode::OK
    );
    assert_eq!(
        status(&app, req("/Patient", "198.51.100.1:1", Some(s1))).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    // Anonymous traffic is unlimited when only the authenticated limit is on.
    for _ in 0..5 {
        assert_eq!(
            status(&app, req("/Patient", "198.51.100.1:1", None)).await.0,
            StatusCode::OK
        );
    }
}

#[tokio::test]
async fn the_administrator_credential_is_never_limited() {
    let app = app(ClientRateLimiter::new(cfg(60, 60)));
    let admin = principal(crate::middleware::auth::ADMIN_ISSUER, "admin", None);
    for _ in 0..5 {
        assert_eq!(
            status(&app, req("/Patient", "198.51.100.1:1", Some(admin.clone())))
                .await
                .0,
            StatusCode::OK
        );
    }
}
