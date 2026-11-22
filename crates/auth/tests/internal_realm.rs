//! Internal-realm verification (docs/design/authorization-server.md §5.2,
//! package I5): tokens minted by the IdP core's signer are accepted by an
//! endpoint bound to that realm only, revoked grants are refused on the
//! next request, and the launch patient reaches the principal so patient
//! confinement applies.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use helios_auth::{
    AuthError, AuthProvider, InternalRealmSource, InternalVerifier, MemoryRealmSource, RealmTrust,
    TrustedKey,
};
use helios_idp::keys::{RsaBits, SigningAlg, SigningKey};
use helios_idp::launch::LaunchContext;
use helios_idp::token::{AccessTokenClaims, AccessTokenInput};

const ISS_A: &str = "https://hfs.test/idp/clinic";
const ISS_B: &str = "https://hfs.test/idp/other";
const AUD_LAB: &str = "https://hfs.test/fhir/lab";
const AUD_ICU: &str = "https://hfs.test/fhir/icu";

struct Realm {
    keys: Vec<SigningKey>,
}

impl Realm {
    fn new() -> Self {
        Self {
            keys: vec![
                SigningKey::generate(SigningAlg::ES256, RsaBits::default()).unwrap(),
                SigningKey::generate(SigningAlg::RS256, RsaBits::default()).unwrap(),
            ],
        }
    }

    fn trust(&self, issuer: &str, bound: &[(&str, &str)]) -> RealmTrust {
        let mut keys = HashMap::new();
        for k in &self.keys {
            let (kid, key) = TrustedKey::from_public_jwk(k.public_jwk()).unwrap();
            assert_eq!(kid, k.kid());
            keys.insert(kid, key);
        }
        RealmTrust {
            issuer: issuer.to_string(),
            keys,
            audiences: bound
                .iter()
                .map(|(e, a)| (e.to_string(), a.to_string()))
                .collect(),
            smart_configuration: HashMap::new(),
        }
    }
}

struct Mint<'a> {
    issuer: &'a str,
    aud: &'a str,
    gid: &'a str,
    scope: &'a str,
    patient: Option<&'a str>,
    ttl: u32,
    typ: &'a str,
}

impl Default for Mint<'_> {
    fn default() -> Self {
        Self {
            issuer: ISS_A,
            aud: AUD_LAB,
            gid: "g1",
            scope: "patient/Observation.rs launch/patient openid fhirUser",
            patient: Some("p1"),
            ttl: 300,
            typ: "at+jwt",
        }
    }
}

fn mint(key: &SigningKey, m: Mint<'_>) -> String {
    let claims = AccessTokenClaims::build(
        AccessTokenInput {
            issuer: m.issuer,
            subject: "user-1",
            audience: vec![m.aud.to_string()],
            client_id: "app",
            scope: m.scope,
            ttl_secs: m.ttl,
            auth_time: Some(Utc::now()),
            fhir_user: Some(format!("{AUD_LAB}/Patient/p1")),
            context: LaunchContext {
                patient: m.patient.map(String::from),
                need_patient_banner: Some(true),
                ..Default::default()
            },
            cnf: None,
            grant_id: m.gid,
        },
        Utc::now(),
    );
    key.sign_jws(m.typ, &claims).unwrap()
}

fn verifier(source: &Arc<MemoryRealmSource>, realm: &str, endpoint: &str) -> InternalVerifier {
    let src: Arc<dyn InternalRealmSource> = source.clone();
    InternalVerifier::new(Some(src), realm, endpoint, None, 0)
}

fn setup() -> (Realm, Realm, Arc<MemoryRealmSource>) {
    let a = Realm::new();
    let b = Realm::new();
    let source = Arc::new(MemoryRealmSource::new());
    source.set_realm(
        "clinic",
        a.trust(ISS_A, &[("lab", AUD_LAB), ("icu", AUD_ICU)]),
    );
    source.set_realm(
        "other",
        b.trust(ISS_B, &[("ward", "https://hfs.test/fhir/ward")]),
    );
    (a, b, source)
}

#[tokio::test]
async fn accepts_tokens_of_its_realm_with_es256_and_rs256() {
    let (a, _b, source) = setup();
    let v = verifier(&source, "clinic", "lab");
    for key in &a.keys {
        let t = mint(key, Mint::default());
        let p = v.authenticate(&format!("Bearer {t}")).await.unwrap();
        assert_eq!(p.subject, "user-1");
        assert_eq!(p.issuer, ISS_A);
        assert_eq!(p.client_id(), Some("app"));
        assert_eq!(p.launch_patient(), Some("p1"), "patient confinement input");
        assert!(p.scopes.raw().iter().any(|s| s == "patient/Observation.rs"));
        assert_eq!(
            p.launch_context.as_ref().unwrap().fhir_user.as_deref(),
            Some("https://hfs.test/fhir/lab/Patient/p1")
        );
    }
    // Scheme is case-insensitive (RFC 7235 §2.1).
    let t = mint(&a.keys[0], Mint::default());
    assert!(v.authenticate(&format!("bearer {t}")).await.is_ok());
}

#[tokio::test]
async fn refuses_tokens_of_another_realm() {
    let (_a, b, source) = setup();
    let v = verifier(&source, "clinic", "lab");
    // Signed by realm B, claiming realm A's issuer: unknown kid.
    let forged = mint(&b.keys[0], Mint::default());
    assert!(matches!(
        v.authenticate(&format!("Bearer {forged}")).await,
        Err(AuthError::UnknownKid { .. })
    ));
    // Honest realm-B token: unknown kid as well.
    let honest = mint(
        &b.keys[0],
        Mint {
            issuer: ISS_B,
            ..Default::default()
        },
    );
    assert!(v.authenticate(&format!("Bearer {honest}")).await.is_err());
}

#[tokio::test]
async fn refuses_tokens_for_another_endpoint_of_the_same_realm() {
    let (a, _b, source) = setup();
    let icu = mint(
        &a.keys[0],
        Mint {
            aud: AUD_ICU,
            ..Default::default()
        },
    );
    let lab = verifier(&source, "clinic", "lab");
    match lab.authenticate(&format!("Bearer {icu}")).await {
        Err(AuthError::ValidationError(m)) => assert!(m.contains("audience"), "{m}"),
        other => panic!("expected audience refusal, got {other:?}"),
    }
    let icu_v = verifier(&source, "clinic", "icu");
    assert!(icu_v.authenticate(&format!("Bearer {icu}")).await.is_ok());
}

#[tokio::test]
async fn wrong_issuer_with_the_right_key_is_refused() {
    let (a, _b, source) = setup();
    let t = mint(
        &a.keys[0],
        Mint {
            issuer: "https://evil/idp/clinic",
            ..Default::default()
        },
    );
    let v = verifier(&source, "clinic", "lab");
    assert!(matches!(
        v.authenticate(&format!("Bearer {t}")).await,
        Err(AuthError::ValidationError(m)) if m.contains("issuer")
    ));
}

#[tokio::test]
async fn revocation_is_immediate() {
    let (a, _b, source) = setup();
    let v = verifier(&source, "clinic", "lab");
    let t = mint(&a.keys[0], Mint::default());
    let other = mint(
        &a.keys[0],
        Mint {
            gid: "g2",
            ..Default::default()
        },
    );
    assert!(v.authenticate(&format!("Bearer {t}")).await.is_ok());
    source.revoke_grant("clinic", "g1");
    match v.authenticate(&format!("Bearer {t}")).await {
        Err(AuthError::ValidationError(m)) => assert_eq!(m, "Token revoked"),
        other => panic!("expected revoked, got {other:?}"),
    }
    assert!(
        v.authenticate(&format!("Bearer {other}")).await.is_ok(),
        "only that grant"
    );
    // Same gid in another realm is a different grant.
    assert!(!source.is_grant_revoked("other", "g1"));
    // Epoch reload replaces the set.
    source.replace_revoked("clinic", ["g2".to_string()]);
    assert!(v.authenticate(&format!("Bearer {t}")).await.is_ok());
    assert!(v.authenticate(&format!("Bearer {other}")).await.is_err());
}

#[tokio::test]
async fn non_access_tokens_and_expired_tokens_are_refused() {
    let (a, _b, source) = setup();
    let v = verifier(&source, "clinic", "lab");
    // An id_token (typ JWT) signed by the same key is not an access token.
    let id = mint(
        &a.keys[1],
        Mint {
            typ: "JWT",
            ..Default::default()
        },
    );
    assert!(matches!(
        v.authenticate(&format!("Bearer {id}")).await,
        Err(AuthError::InvalidTokenFormat(m)) if m.contains("at+jwt")
    ));
    let expired = {
        let mut claims: serde_json::Value = serde_json::json!({
            "iss": ISS_A, "sub": "u", "aud": [AUD_LAB], "exp": Utc::now().timestamp() - 120,
            "iat": Utc::now().timestamp() - 400, "jti": "j", "client_id": "app",
            "scope": "patient/*.rs", "gid": "g9"
        });
        claims["patient"] = "p1".into();
        a.keys[0].sign_jws("at+jwt", &claims).unwrap()
    };
    assert!(matches!(
        v.authenticate(&format!("Bearer {expired}")).await,
        Err(AuthError::TokenExpired)
    ));
    // Missing gid → refused (cannot be revoked, so never accepted).
    let no_gid = a.keys[0]
        .sign_jws(
            "at+jwt",
            &serde_json::json!({"iss": ISS_A, "sub": "u", "aud": AUD_LAB,
            "exp": Utc::now().timestamp() + 60, "client_id": "app", "scope": "x"}),
        )
        .unwrap();
    assert!(matches!(
        v.authenticate(&format!("Bearer {no_gid}")).await,
        Err(AuthError::ValidationError(m)) if m.contains("gid")
    ));
}

#[tokio::test]
async fn algorithm_confusion_is_refused() {
    let (a, _b, source) = setup();
    let v = verifier(&source, "clinic", "lab");
    // Header claims HS256 with the RSA key's kid; payload signed with the
    // public JWK bytes as an HMAC secret would be the classic confusion.
    let t = mint(&a.keys[1], Mint::default());
    let mut parts: Vec<String> = t.split('.').map(String::from).collect();
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = serde_json::json!({"alg": "HS256", "kid": a.keys[1].kid(), "typ": "at+jwt"});
    parts[0] = b64.encode(serde_json::to_vec(&header).unwrap());
    let forged = parts.join(".");
    assert!(v.authenticate(&format!("Bearer {forged}")).await.is_err());
    // alg none
    let header = serde_json::json!({"alg": "none", "kid": a.keys[1].kid(), "typ": "at+jwt"});
    parts[0] = b64.encode(serde_json::to_vec(&header).unwrap());
    parts[2] = String::new();
    assert!(
        v.authenticate(&format!("Bearer {}", parts.join(".")))
            .await
            .is_err()
    );
    // ES256 key's kid with an RS256 header.
    let t = mint(&a.keys[0], Mint::default());
    let mut parts: Vec<String> = t.split('.').map(String::from).collect();
    let header = serde_json::json!({"alg": "RS256", "kid": a.keys[0].kid(), "typ": "at+jwt"});
    parts[0] = b64.encode(serde_json::to_vec(&header).unwrap());
    assert!(matches!(
        v.authenticate(&format!("Bearer {}", parts.join("."))).await,
        Err(AuthError::UnsupportedAlgorithm { .. })
    ));
}

#[tokio::test]
async fn retired_keys_stop_verifying_on_the_next_request() {
    let (a, _b, source) = setup();
    let v = verifier(&source, "clinic", "lab");
    let t = mint(&a.keys[0], Mint::default());
    assert!(v.authenticate(&format!("Bearer {t}")).await.is_ok());
    let mut trust = a.trust(ISS_A, &[("lab", AUD_LAB)]);
    trust.keys.remove(a.keys[0].kid());
    source.set_realm("clinic", trust);
    assert!(matches!(
        v.authenticate(&format!("Bearer {t}")).await,
        Err(AuthError::UnknownKid { .. })
    ));
}

#[tokio::test]
async fn fails_closed_when_the_idp_is_off_or_the_realm_is_gone() {
    let (a, _b, source) = setup();
    let t = mint(&a.keys[0], Mint::default());
    let off = InternalVerifier::new(None, "clinic", "lab", None, 0);
    match off.authenticate(&format!("Bearer {t}")).await {
        Err(AuthError::JwksFetchError(m)) => assert!(m.contains("HFS_IDP"), "{m}"),
        other => panic!("expected 503-class refusal, got {other:?}"),
    }
    let unbound = verifier(&source, "clinic", "not-bound");
    assert!(matches!(
        unbound.authenticate(&format!("Bearer {t}")).await,
        Err(AuthError::JwksFetchError(_))
    ));
    source.remove_realm("clinic");
    let v = verifier(&source, "clinic", "lab");
    assert!(matches!(
        v.authenticate(&format!("Bearer {t}")).await,
        Err(AuthError::JwksFetchError(_))
    ));
}
