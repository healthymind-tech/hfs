//! Rate limits around the `/idp` router (design §10.2): 429 + Retry-After,
//! per class and key, and the caller address cannot be forged.

use axum::Router;
use axum::http::{HeaderValue, StatusCode, header};
use axum::routing::{get, post};
use axum_test::TestServer;
use helios_idp_pages::limits::{CallerIp, LimitConfig, RateLimiter, layer};

fn app(cfg: LimitConfig, trust_forwarded: bool) -> TestServer {
    let limiter = RateLimiter::new(cfg, trust_forwarded);
    let echo = |req: axum::extract::Request| async move {
        req.extensions()
            .get::<CallerIp>()
            .map(|c| c.0.clone())
            .unwrap_or_default()
    };
    let r = Router::new()
        .route("/idp/{realm}/login", post(echo))
        .route("/idp/{realm}/token", post(echo))
        .route("/idp/{realm}/jwks", get(echo))
        .layer(axum::middleware::from_fn_with_state(limiter, layer));
    TestServer::new(r).unwrap()
}

#[tokio::test]
async fn interactive_posts_get_429_with_retry_after() {
    let s = app(
        LimitConfig {
            interactive_per_ip: 3,
            ..LimitConfig::default()
        },
        false,
    );
    for _ in 0..3 {
        assert_eq!(s.post("/idp/r/login").await.status_code(), StatusCode::OK);
    }
    let r = s.post("/idp/r/login").await;
    assert_eq!(r.status_code(), StatusCode::TOO_MANY_REQUESTS);
    let after: u64 = r.header(header::RETRY_AFTER).to_str().unwrap().parse().unwrap();
    assert!((1..=20).contains(&after), "{after}");
    assert!(r.text().contains("slow_down"));
    // Other realms and other classes have their own buckets.
    assert_eq!(s.post("/idp/other/login").await.status_code(), StatusCode::OK);
    assert_eq!(s.post("/idp/r/token").await.status_code(), StatusCode::OK);
    // Discovery / JWKS are not limited by this layer.
    for _ in 0..10 {
        assert_eq!(s.get("/idp/r/jwks").await.status_code(), StatusCode::OK);
    }
}

#[tokio::test]
async fn token_endpoint_limits_per_client() {
    let s = app(
        LimitConfig {
            token_per_client: 2,
            token_per_ip: 100,
            ..LimitConfig::default()
        },
        false,
    );
    let basic = |id: &str| {
        use base64::Engine;
        HeaderValue::from_str(&format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{id}:secret"))
        ))
        .unwrap()
    };
    for _ in 0..2 {
        let r = s.post("/idp/r/token").add_header(header::AUTHORIZATION, basic("a")).await;
        assert_eq!(r.status_code(), StatusCode::OK);
    }
    let r = s.post("/idp/r/token").add_header(header::AUTHORIZATION, basic("a")).await;
    assert_eq!(r.status_code(), StatusCode::TOO_MANY_REQUESTS);
    let r = s.post("/idp/r/token").add_header(header::AUTHORIZATION, basic("b")).await;
    assert_eq!(r.status_code(), StatusCode::OK, "another client");
}

#[tokio::test]
async fn a_forged_forwarded_for_does_not_reset_the_bucket() {
    let s = app(
        LimitConfig {
            interactive_per_ip: 2,
            ..LimitConfig::default()
        },
        true,
    );
    // The proxy appends the real address as the last hop; the first hop is
    // whatever the client sent.
    for fake in ["1.1.1.1", "2.2.2.2"] {
        let r = s
            .post("/idp/r/login")
            .add_header("x-forwarded-for", HeaderValue::from_str(&format!("{fake}, 203.0.113.7")).unwrap())
            .await;
        assert_eq!(r.text(), "203.0.113.7");
    }
    let r = s
        .post("/idp/r/login")
        .add_header("x-forwarded-for", HeaderValue::from_static("9.9.9.9, 203.0.113.7"))
        .await;
    assert_eq!(r.status_code(), StatusCode::TOO_MANY_REQUESTS);
}
