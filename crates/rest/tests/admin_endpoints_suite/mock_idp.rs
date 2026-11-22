//! A mock OpenID provider for auth-probe tests (wiremock): discovery,
//! JWKS, and RS256 tokens signed with a fixed test key.

#![allow(dead_code)]

use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub const KID: &str = "test-key-1";

/// Test-only RSA key (the one of crates/auth/tests/bearer_token_reuse.rs).
pub const TEST_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCSroIOUaCnmnAz
wXfTUJd7PeHkKBFyL1cuAz3bDgzCy6LyhSeBaytYLgzc43v2MD9fIMpD1sh2Uua5
mM56uypO2i3FKVsCJQzRs7gc1cz8dBPCDG6jK9wwmQ+eE3TdpxPfqDwVNxsNQu8B
5qByfgwO6vjDPDFvdl1JaGGLBwAK2aMNa/fL2isD358iIzrnydkXgUYD8FnfKY2u
3gqeTy4yrJIClBiLH7KH73/CpU08w2xQssUyZPOIZ+sWUgKb/HKAl6ldBKM6gdOw
+fScdT/Qg5eZWizxJoohI7gCDLUra+xVi4Q6IA1h2+IJY523fG3lXpusO8ni8vPP
TtfSc90TAgMBAAECggEABnkE7DTV7g92nBIRg5Wu2ZVlfnf2LR/BrRofhKceEQqD
akhN8fwUsZN2pdi0A60lXsFHq66yseX+oHoJwoi32Trvgh/NEE8qPaa8nSkiHpHQ
vWNnDnRFBy/57HPXWGCjE9+MpzMDUpZ9jYvr0KGXTqE30QW6+Lw0aaTdiREKA3B+
EPe7laG5BfccznNt2ESaqfqyVLE/LRQ3ZxC32ruxoFWwaC9vqs0PWgCII72rqi5f
YcXBdQd9q5x4mPzX9CiR/E9TkTUNL14HFpc1T1XxQMHFtjwIRzisjE9VMjcEUpuF
YIGSSXEaoV8otETkyUWYKGNqlDu7EI39IqSQGmGBIQKBgQDIL7aYftFuDMq3Zgo6
3i5FL1SzzH6cupzVuI3/n2eR5HDKxE2Y1UKwXv4epfBQ5jIJYF+n0wBmfcYZEpq6
yf/golBDVrWBIXAmt0JU2yeaBJkyhUKv/PeukyWPWB5/zCWpAQ1yn66S+RBaIrL3
bXNkjfXsfmbkOS4BHvef5FOq5QKBgQC7k+aKYL81YUswLsx+He12/2xHNKYng9nd
jukZ6mh2xUPbQonJg9+miUNWHIv3CkKkbkbKRXYEF4skCDEANRUqZiErMzj37ODz
B1K4UMWg9F315Y2HSclBy/em7vUQwhOhg1L6UkPLoO92mhE4VSe3BPIoqNjaqVJu
NPhvnUDQlwKBgQCdGhTKiHwDSbatRz8wA718PjDTCeEzTqBWeYe23HqDXCvIdVYQ
Ywz7LRFxK/j1BDKweRmYs1bVGE+mzZrwjCZrO/aRYjL/LCa/u4Iq5fKmRIWVyE8V
ngki0Afh/t2wnZ3QjCrpkbeHUD8s/Z5F33d3qpEdD/XflaAs8QiUSrP+oQKBgFVM
WmfOuuShS7mrbl5jaZrVZ/2xWWVatfXkiOe6CqsH5WWNim7Swx9OCArejF9YkRmI
9DQDBjmyIxnNh8raWLehHbAxaNSFKX4adGlQga5BsYCiVIuS6Cw9fm6w90wZlSe7
Oj6OrjmpA3vhb7c4Mgkt/Ji0v3gfy1ZGTDslPVYdAoGBAKbfGrUtJQr6bpJmCRyp
zUu2oQ8paQXazIx+CscfgoWheDte2STTScGcKj1QuDAozcbBPYUpodSgkK4O5Wrs
qqPm2u7Kvz/kcDNIAIE1MUYNpgiZx/C0ca4yDHACefzwWD9IerNFRVr1CPGtFFzw
uLlPD/tbsTbUAc5x9+dCkqEp
-----END PRIVATE KEY-----";

/// Its public modulus.
pub const TEST_JWK_N: &str = "kq6CDlGgp5pwM8F301CXez3h5CgRci9XLgM92w4Mwsui8oUngWsrWC4M3ON79jA_XyDKQ9bIdlLmuZjOersqTtotxSlbAiUM0bO4HNXM_HQTwgxuoyvcMJkPnhN03acT36g8FTcbDULvAeagcn4MDur4wzwxb3ZdSWhhiwcACtmjDWv3y9orA9-fIiM658nZF4FGA_BZ3ymNrt4Knk8uMqySApQYix-yh-9_wqVNPMNsULLFMmTziGfrFlICm_xygJepXQSjOoHTsPn0nHU_0IOXmVos8SaKISO4Agy1K2vsVYuEOiANYdviCWOdt3xt5V6brDvJ4vLzz07X0nPdEw";

/// What the mock serves.
pub struct Idp {
    pub server: MockServer,
    pub issuer: String,
    pub jwks_url: String,
}

/// `discovery` / `jwks`: serve them (200) or answer 404 / 500.
pub async fn start(discovery: bool, jwks: bool) -> Idp {
    let server = MockServer::start().await;
    let issuer = format!("{}/realms/t", server.uri());
    let jwks_url = format!("{issuer}/certs");
    let d = if discovery {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/auth"),
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": jwks_url,
        }))
    } else {
        ResponseTemplate::new(404)
    };
    Mock::given(method("GET"))
        .and(path("/realms/t/.well-known/openid-configuration"))
        .respond_with(d)
        .mount(&server)
        .await;
    let j = if jwks {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "keys": [{"kid": KID, "kty": "RSA", "use": "sig", "alg": "RS256",
                       "n": TEST_JWK_N, "e": "AQAB"}]
        }))
    } else {
        ResponseTemplate::new(500)
    };
    Mock::given(method("GET"))
        .and(path("/realms/t/certs"))
        .respond_with(j)
        .mount(&server)
        .await;
    Idp {
        server,
        issuer,
        jwks_url,
    }
}

/// An RS256 token of `issuer` for `audience`, expiring in `exp_in` seconds
/// (negative: already expired).
pub fn token(issuer: &str, audience: &str, exp_in: i64) -> String {
    let now = chrono::Utc::now().timestamp();
    let claims = serde_json::json!({
        "sub": "svc-client", "iss": issuer, "aud": audience,
        "iat": now - 60, "exp": now + exp_in, "scope": "system/*.rs",
    });
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(KID.to_string());
    let key = EncodingKey::from_rsa_pem(TEST_PRIVATE_KEY_PEM.as_bytes()).expect("test key");
    encode(&header, &claims, &key).expect("sign")
}
