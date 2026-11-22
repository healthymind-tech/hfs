//! Compact JWS verification with `ring` (client assertions, RFC 7523; our own
//! access and id tokens at introspection, userinfo and logout).
//!
//! Defences against the classic JWT pitfalls (§10.4 "Algorithm confusion"):
//! * `alg` must be one of the caller's allowed asymmetric algorithms;
//!   `none` and `HS*` can never be allowed (they are not [`SigningAlg`]s).
//! * The verification algorithm comes from `alg`, and the key's `kty` (and
//!   `crv`, and `alg` when the JWK carries one) must agree with it, so an RSA
//!   key can never be used as an HMAC secret or vice versa.
//! * `kid` is required and must select exactly one key.
//! * `crit` is refused (we understand no extensions), as are oversized tokens.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{self, RsaPublicKeyComponents, UnparsedPublicKey};
use serde_json::Value;

use crate::keys::SigningAlg;

/// Largest compact JWS accepted (bytes).
pub const MAX_JWS_LEN: usize = 16 * 1024;

/// Why a JWS was refused. The messages are safe to show to client developers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JwsError {
    #[error("malformed JWT: {0}")]
    Malformed(&'static str),
    #[error("JWT algorithm {0:?} is not accepted")]
    Algorithm(String),
    #[error("JWT header has no kid")]
    MissingKid,
    #[error("no unique key matches the JWT kid")]
    NoKey,
    #[error("the key does not fit the JWT algorithm")]
    KeyMismatch,
    #[error("JWT signature is invalid")]
    Signature,
    #[error("JWT uses unsupported critical extensions")]
    Critical,
}

/// A verified JWS.
#[derive(Debug, Clone, PartialEq)]
pub struct Verified {
    pub header: Value,
    pub claims: serde_json::Map<String, Value>,
    pub alg: SigningAlg,
    pub kid: String,
}

fn b64d(s: &str) -> Result<Vec<u8>, JwsError> {
    URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|_| JwsError::Malformed("bad base64url"))
}

/// Splits and decodes the header without verifying anything (used to pick
/// the client or key set before verification). Never trust its content.
pub fn peek(token: &str) -> Result<(Value, serde_json::Map<String, Value>), JwsError> {
    if token.len() > MAX_JWS_LEN {
        return Err(JwsError::Malformed("too large"));
    }
    let mut it = token.split('.');
    let (Some(h), Some(p), Some(_s), None) = (it.next(), it.next(), it.next(), it.next()) else {
        return Err(JwsError::Malformed("not a compact JWS"));
    };
    let header: Value =
        serde_json::from_slice(&b64d(h)?).map_err(|_| JwsError::Malformed("header is not JSON"))?;
    if !header.is_object() {
        return Err(JwsError::Malformed("header is not an object"));
    }
    let claims: Value = serde_json::from_slice(&b64d(p)?)
        .map_err(|_| JwsError::Malformed("payload is not JSON"))?;
    let Value::Object(claims) = claims else {
        return Err(JwsError::Malformed("payload is not an object"));
    };
    Ok((header, claims))
}

/// Verifies `token` against the keys of a JWK Set (`keys` array members).
pub fn verify(token: &str, keys: &[Value], allowed: &[SigningAlg]) -> Result<Verified, JwsError> {
    let (header, claims) = peek(token)?;
    let alg_s = header
        .get("alg")
        .and_then(Value::as_str)
        .ok_or(JwsError::Malformed("header has no alg"))?;
    let alg: SigningAlg = alg_s
        .parse()
        .map_err(|_| JwsError::Algorithm(alg_s.to_string()))?;
    if !allowed.contains(&alg) {
        return Err(JwsError::Algorithm(alg_s.to_string()));
    }
    if header.get("crit").is_some() {
        return Err(JwsError::Critical);
    }
    let kid = header
        .get("kid")
        .and_then(Value::as_str)
        .ok_or(JwsError::MissingKid)?
        .to_string();
    let mut matching = keys
        .iter()
        .filter(|k| k.get("kid").and_then(Value::as_str) == Some(kid.as_str()));
    let key = matching.next().ok_or(JwsError::NoKey)?;
    if matching.next().is_some() {
        return Err(JwsError::NoKey);
    }
    check_key_fits(key, alg)?;
    let (signing_input, sig_b64) = token.rsplit_once('.').expect("three parts checked");
    let sig = b64d(sig_b64)?;
    verify_raw(key, alg, signing_input.as_bytes(), &sig)?;
    Ok(Verified {
        header,
        claims,
        alg,
        kid,
    })
}

fn check_key_fits(key: &Value, alg: SigningAlg) -> Result<(), JwsError> {
    let kty = key.get("kty").and_then(Value::as_str);
    if kty != Some(alg.kty()) {
        return Err(JwsError::KeyMismatch);
    }
    if let Some(a) = key.get("alg").and_then(Value::as_str)
        && a != alg.as_str()
    {
        return Err(JwsError::KeyMismatch);
    }
    if let Some(u) = key.get("use").and_then(Value::as_str)
        && u != "sig"
    {
        return Err(JwsError::KeyMismatch);
    }
    if let Some(ops) = key.get("key_ops").and_then(Value::as_array)
        && !ops.iter().any(|o| o == "verify")
    {
        return Err(JwsError::KeyMismatch);
    }
    if !alg.is_rsa() {
        let want = match alg {
            SigningAlg::ES256 => "P-256",
            _ => "P-384",
        };
        if key.get("crv").and_then(Value::as_str) != Some(want) {
            return Err(JwsError::KeyMismatch);
        }
    }
    Ok(())
}

fn member(key: &Value, name: &str) -> Result<Vec<u8>, JwsError> {
    let s = key
        .get(name)
        .and_then(Value::as_str)
        .ok_or(JwsError::KeyMismatch)?;
    URL_SAFE_NO_PAD.decode(s).map_err(|_| JwsError::KeyMismatch)
}

fn verify_raw(key: &Value, alg: SigningAlg, msg: &[u8], sig: &[u8]) -> Result<(), JwsError> {
    if alg.is_rsa() {
        let n = member(key, "n")?;
        let e = member(key, "e")?;
        RsaPublicKeyComponents { n: &n, e: &e }
            .verify(rsa_params(alg), msg, sig)
            .map_err(|_| JwsError::Signature)
    } else {
        let x = member(key, "x")?;
        let y = member(key, "y")?;
        let len = if alg == SigningAlg::ES256 { 32 } else { 48 };
        if x.len() != len || y.len() != len || sig.len() != 2 * len {
            return Err(JwsError::KeyMismatch);
        }
        let mut point = Vec::with_capacity(1 + 2 * len);
        point.push(4u8);
        point.extend_from_slice(&x);
        point.extend_from_slice(&y);
        UnparsedPublicKey::new(alg.verification(), &point)
            .verify(msg, sig)
            .map_err(|_| JwsError::Signature)
    }
}

fn rsa_params(alg: SigningAlg) -> &'static signature::RsaParameters {
    match alg {
        SigningAlg::RS256 => &signature::RSA_PKCS1_2048_8192_SHA256,
        SigningAlg::RS384 => &signature::RSA_PKCS1_2048_8192_SHA384,
        SigningAlg::PS256 => &signature::RSA_PSS_2048_8192_SHA256,
        _ => &signature::RSA_PSS_2048_8192_SHA384,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{RsaBits, SigningKey};
    use crate::util::b64;
    use serde_json::json;

    fn tok(key: &SigningKey) -> String {
        key.sign_jws("JWT", &json!({"iss":"a","sub":"a"})).unwrap()
    }

    #[test]
    fn verifies_every_alg() {
        for alg in SigningAlg::ALL {
            let k = SigningKey::generate(alg, RsaBits::B2048).unwrap();
            let v = verify(&tok(&k), &[k.public_jwk().clone()], &SigningAlg::ALL).unwrap();
            assert_eq!(v.alg, alg);
            assert_eq!(v.claims["iss"], "a");
        }
    }

    #[test]
    fn refusals() {
        let k = SigningKey::generate(SigningAlg::ES384, RsaBits::B2048).unwrap();
        let other = SigningKey::generate(SigningAlg::ES384, RsaBits::B2048).unwrap();
        let t = tok(&k);
        let jwk = k.public_jwk().clone();
        // Not allowed alg.
        assert!(matches!(
            verify(&t, &[jwk.clone()], &[SigningAlg::RS384]),
            Err(JwsError::Algorithm(_))
        ));
        // Wrong key under the same kid.
        let mut forged = other.public_jwk().clone();
        forged["kid"] = jwk["kid"].clone();
        assert_eq!(
            verify(&t, &[forged], &SigningAlg::ALL),
            Err(JwsError::Signature)
        );
        // Duplicate kid is ambiguous.
        assert_eq!(
            verify(&t, &[jwk.clone(), jwk.clone()], &SigningAlg::ALL),
            Err(JwsError::NoKey)
        );
        // Tampered payload.
        let parts: Vec<&str> = t.split('.').collect();
        let p2 = b64(br#"{"iss":"b","sub":"a"}"#);
        let tampered = format!("{}.{}.{}", parts[0], p2, parts[2]);
        assert_eq!(
            verify(&tampered, &[jwk.clone()], &SigningAlg::ALL),
            Err(JwsError::Signature)
        );
        // alg none / HS256 headers.
        for alg in ["none", "HS256", "HS384"] {
            let h = b64(json!({"alg": alg, "kid": jwk["kid"]})
                .to_string()
                .as_bytes());
            let forged = format!("{h}.{}.{}", parts[1], parts[2]);
            assert!(matches!(
                verify(&forged, &[jwk.clone()], &SigningAlg::ALL),
                Err(JwsError::Algorithm(_))
            ));
        }
        // RSA alg against an EC key (alg confusion).
        let h = b64(json!({"alg": "RS384", "kid": jwk["kid"]})
            .to_string()
            .as_bytes());
        let confused = format!("{h}.{}.{}", parts[1], parts[2]);
        assert_eq!(
            verify(&confused, &[jwk.clone()], &SigningAlg::ALL),
            Err(JwsError::KeyMismatch)
        );
        // Missing kid.
        let h = b64(json!({"alg": "ES384"}).to_string().as_bytes());
        assert_eq!(
            verify(
                &format!("{h}.{}.{}", parts[1], parts[2]),
                &[jwk.clone()],
                &SigningAlg::ALL
            ),
            Err(JwsError::MissingKid)
        );
        // crit.
        let h = b64(json!({"alg": "ES384", "kid": jwk["kid"], "crit": ["exp"]})
            .to_string()
            .as_bytes());
        assert_eq!(
            verify(
                &format!("{h}.{}.{}", parts[1], parts[2]),
                &[jwk.clone()],
                &SigningAlg::ALL
            ),
            Err(JwsError::Critical)
        );
        // Junk.
        assert!(verify("a.b", &[jwk.clone()], &SigningAlg::ALL).is_err());
        assert!(verify(&"a".repeat(20000), &[jwk], &SigningAlg::ALL).is_err());
    }

    #[test]
    fn key_alg_member_must_agree() {
        let k = SigningKey::generate(SigningAlg::ES256, RsaBits::B2048).unwrap();
        let mut jwk = k.public_jwk().clone();
        jwk["alg"] = json!("ES384");
        assert_eq!(
            verify(&tok(&k), &[jwk], &SigningAlg::ALL),
            Err(JwsError::KeyMismatch)
        );
    }
}
