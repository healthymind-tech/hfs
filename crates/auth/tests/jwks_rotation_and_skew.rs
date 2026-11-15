//! Key rotation, clock skew and SMART launch context against a mock JWKS.
//!
//! An IdP rotates its signing key by publishing a new `kid` (Keycloak keeps the
//! old key as passive until it is removed). HFS must pick up a token signed
//! with a key it has not seen yet by refreshing the JWKS on the unknown `kid`,
//! and must stop accepting a key once the IdP stops publishing it.

use std::sync::Arc;

use helios_auth::{AuthConfig, AuthError, AuthProvider, JwksBearerAuthProvider, JwksCache};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ISSUER: &str = "https://idp.example.com";
const KID_1: &str = "key-1";
const KID_2: &str = "key-2";
const N_1: &str = "zbeWVBfqrPsdF1phb2EB5hxjz1EMb5WT4MnhMatFnNFquEZIcgXG1DBAUICEZLQM8gRj9kZ83YA1cFhUtkgUx1qMio8YkKweLV6r8RAqnuxrS5Khm4a25M85qB5MhGoCLZgGtla9lJ79mjVN5XBqD7sPL0ENN1GPcnus1I6WF-E-5Uiih-QWsQVlGeUqMH81vTBLemDxvMQv-TiKZP3LHH_T41G1FfyF-rlbkj7WXGkbqG_WvR4FKeRm1X2_RRVw87h2j1lfK4AiUKs4h_DqwqZju3fgOH_0MHj2RKCu0wo0cTx8mQGbLCNSb-sRoWRnWGpGPphI9xVS8znuPB1JYQ";
const N_2: &str = "uF_UcxtWC3Z2CJfQzMGUN7VfjqhynYYPL4ftRXnC4Oo5wfB0DG1rg8PEslctm6XyShwP4PBzLyzqsHVWfV1S03mLsEz3LHzr4CF0aiaaZVPV2uKcKMAlA6pD3MARAzXF0Q038XTy_JCcr0jZn98OvxFv9chaz2q38rLn-vyptITbDqfxThLrEbjp4e3Pd-utrTYbGq_BQ9UEpbQ-ONrLBkLvWJMJTbYi6omOrmqxh9IrPuMGavUpCbK_YxkI2P9PQ2dc4iNASK1jq6g8zafscMErY2hxbF20iTN2my7laL0c0GpNVy-822GU8Ooefn2ZfG736Pys9b5-HfQF-YQZEw";

fn now() -> i64 {
    jsonwebtoken::get_current_timestamp() as i64
}

fn jwk(kid: &str, n: &str) -> Value {
    json!({"kty": "RSA", "kid": kid, "use": "sig", "alg": "RS256", "n": n, "e": "AQAB"})
}

fn sign_with(kid: &str, claims: &Value) -> String {
    let pem: &[u8] = if kid == KID_1 {
        include_bytes!("data/test_rsa_key.pem")
    } else {
        include_bytes!("data/test_rsa_key_2.pem")
    };
    let key = EncodingKey::from_rsa_pem(pem).expect("valid test RSA key");
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.to_string());
    encode(&header, claims, &key).expect("token encodes")
}

fn claims() -> Value {
    json!({"sub": "app-1", "iss": ISSUER, "exp": now() + 600, "scope": "patient/*.rs"})
}

async fn serve_keys(server: &MockServer, keys: Vec<Value>) {
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "keys": keys })))
        .mount(server)
        .await;
}

async fn provider(server: &MockServer, skew: u64) -> JwksBearerAuthProvider {
    // A zero refresh interval: every unknown `kid` may refresh.
    let cache = Arc::new(JwksCache::new(&format!("{}/jwks", server.uri()), 0));
    cache.initial_fetch().await.expect("initial JWKS fetch");
    let config = AuthConfig {
        enabled: true,
        expected_issuer: Some(ISSUER.to_string()),
        clock_skew_secs: skew,
        ..AuthConfig::default()
    };
    JwksBearerAuthProvider::new(cache, &config)
}

#[tokio::test]
async fn a_rotated_key_is_fetched_on_first_sight_and_a_retired_one_refused() {
    let server = MockServer::start().await;
    serve_keys(&server, vec![jwk(KID_1, N_1)]).await;
    let provider = provider(&server, 60).await;

    let old = sign_with(KID_1, &claims());
    provider
        .authenticate(&format!("Bearer {old}"))
        .await
        .expect("token signed with the current key");

    // The IdP rotates: key-2 is active, key-1 still published as passive.
    serve_keys(&server, vec![jwk(KID_2, N_2), jwk(KID_1, N_1)]).await;
    let new = sign_with(KID_2, &claims());
    provider
        .authenticate(&format!("Bearer {new}"))
        .await
        .expect("token signed with the new key, refreshed on its unknown kid");
    provider
        .authenticate(&format!("Bearer {old}"))
        .await
        .expect("the passive key still validates");

    // The IdP retires key-1. A refresh (triggered by any unknown kid) drops it.
    serve_keys(&server, vec![jwk(KID_2, N_2)]).await;
    let unknown = {
        let mut c = claims();
        c["jti"] = json!("x");
        let pem = include_bytes!("data/test_rsa_key_2.pem");
        let key = EncodingKey::from_rsa_pem(pem).unwrap();
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("key-3".to_string());
        encode(&header, &c, &key).unwrap()
    };
    let err = provider
        .authenticate(&format!("Bearer {unknown}"))
        .await
        .expect_err("a kid the IdP never published");
    assert!(matches!(err, AuthError::UnknownKid { .. }), "got {err}");
    let err = provider
        .authenticate(&format!("Bearer {old}"))
        .await
        .expect_err("the retired key no longer validates");
    assert!(matches!(err, AuthError::UnknownKid { .. }), "got {err}");
}

#[tokio::test]
async fn expiry_is_judged_with_the_configured_clock_skew() {
    let server = MockServer::start().await;
    serve_keys(&server, vec![jwk(KID_1, N_1)]).await;

    let mut c = claims();
    c["exp"] = json!(now() - 30);
    let token = format!("Bearer {}", sign_with(KID_1, &c));

    provider(&server, 60)
        .await
        .authenticate(&token)
        .await
        .expect("30 s past exp is inside a 60 s skew");
    let err = provider(&server, 5)
        .await
        .authenticate(&token)
        .await
        .expect_err("30 s past exp is outside a 5 s skew");
    assert!(matches!(err, AuthError::TokenExpired), "got {err}");
}

#[tokio::test]
async fn not_before_is_judged_with_the_configured_clock_skew() {
    let server = MockServer::start().await;
    serve_keys(&server, vec![jwk(KID_1, N_1)]).await;

    let mut c = claims();
    c["nbf"] = json!(now() + 20);
    let token = format!("Bearer {}", sign_with(KID_1, &c));
    provider(&server, 60)
        .await
        .authenticate(&token)
        .await
        .expect("a token minted on a clock 20 s ahead is inside a 60 s skew");
    provider(&server, 0)
        .await
        .authenticate(&token)
        .await
        .expect_err("but not inside a zero skew");
}

#[tokio::test]
async fn launch_patient_and_client_come_from_the_token() {
    let server = MockServer::start().await;
    serve_keys(&server, vec![jwk(KID_1, N_1)]).await;
    let mut c = claims();
    c["patient"] = json!("pat-1");
    c["azp"] = json!("growth-chart");
    let principal = provider(&server, 60)
        .await
        .authenticate(&format!("Bearer {}", sign_with(KID_1, &c)))
        .await
        .expect("valid");
    assert_eq!(principal.launch_patient(), Some("pat-1"));
    assert_eq!(principal.client_id(), Some("growth-chart"));
    assert!(principal.scopes.has_patient_scopes());
}
