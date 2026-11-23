//! Realm signing keys: generation, JWS signing, public JWKs, sealing, and
//! the rotation state machine (design §6).
//!
//! * Algorithms: `RS256 RS384 PS256 PS384 ES256 ES384`. A realm normally holds
//!   one active RSA key (`RS256`, the OIDC default for id_tokens, OIDC Core
//!   §3.1.3.7 / Discovery §3) and one active EC key (`ES256`, compact tokens).
//!   `HS*` (shared secrets) and `none` are never used to sign tokens.
//! * Private keys exist in plaintext only in memory (`ring` key pairs; the
//!   PKCS#8 bytes are wiped on drop). At rest they are [`SealedSecret`]s under
//!   `HFS_SECRET_KEYS`, AAD-bound to `idp-realm:{realm_id}:signing-key:{kid}`.
//! * `kid` = RFC 7638 JWK thumbprint (SHA-256, base64url) of the public key.
//! * Rotation: `next` (published, not signing) → `active` (signing) →
//!   `retiring` (published, verifying tokens still alive) → `retired`
//!   (unpublished, kept for audit). Publishing `next` at least one JWKS cache
//!   lifetime before it signs lets resource servers and clients pick it up.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Duration, Utc};
use helios_endpoints::secret::{Plaintext, SealedSecret, SecretKeyRing};
use ring::rand::SystemRandom;
use ring::signature::{self, EcdsaKeyPair, KeyPair, RsaKeyPair};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::error::IdpError;
use crate::util::{b64, sha256_b64};

/// A JWS signing algorithm the server issues tokens with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SigningAlg {
    RS256,
    RS384,
    PS256,
    PS384,
    ES256,
    ES384,
}

impl SigningAlg {
    pub const ALL: [SigningAlg; 6] = [
        SigningAlg::RS256,
        SigningAlg::RS384,
        SigningAlg::PS256,
        SigningAlg::PS384,
        SigningAlg::ES256,
        SigningAlg::ES384,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SigningAlg::RS256 => "RS256",
            SigningAlg::RS384 => "RS384",
            SigningAlg::PS256 => "PS256",
            SigningAlg::PS384 => "PS384",
            SigningAlg::ES256 => "ES256",
            SigningAlg::ES384 => "ES384",
        }
    }

    /// JWK `kty`.
    pub fn kty(self) -> &'static str {
        if self.is_rsa() { "RSA" } else { "EC" }
    }

    pub fn is_rsa(self) -> bool {
        matches!(
            self,
            SigningAlg::RS256 | SigningAlg::RS384 | SigningAlg::PS256 | SigningAlg::PS384
        )
    }

    fn ecdsa_signing(self) -> Option<&'static signature::EcdsaSigningAlgorithm> {
        match self {
            SigningAlg::ES256 => Some(&signature::ECDSA_P256_SHA256_FIXED_SIGNING),
            SigningAlg::ES384 => Some(&signature::ECDSA_P384_SHA384_FIXED_SIGNING),
            _ => None,
        }
    }

    fn rsa_encoding(self) -> Option<&'static dyn signature::RsaEncoding> {
        match self {
            SigningAlg::RS256 => Some(&signature::RSA_PKCS1_SHA256),
            SigningAlg::RS384 => Some(&signature::RSA_PKCS1_SHA384),
            SigningAlg::PS256 => Some(&signature::RSA_PSS_SHA256),
            SigningAlg::PS384 => Some(&signature::RSA_PSS_SHA384),
            _ => None,
        }
    }

    /// The verification algorithm (for self-checks and in-process
    /// verification of our own tokens).
    pub fn verification(self) -> &'static dyn signature::VerificationAlgorithm {
        match self {
            SigningAlg::RS256 => &signature::RSA_PKCS1_2048_8192_SHA256,
            SigningAlg::RS384 => &signature::RSA_PKCS1_2048_8192_SHA384,
            SigningAlg::PS256 => &signature::RSA_PSS_2048_8192_SHA256,
            SigningAlg::PS384 => &signature::RSA_PSS_2048_8192_SHA384,
            SigningAlg::ES256 => &signature::ECDSA_P256_SHA256_FIXED,
            SigningAlg::ES384 => &signature::ECDSA_P384_SHA384_FIXED,
        }
    }

    fn crv(self) -> Option<&'static str> {
        match self {
            SigningAlg::ES256 => Some("P-256"),
            SigningAlg::ES384 => Some("P-384"),
            _ => None,
        }
    }
}

impl fmt::Display for SigningAlg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SigningAlg {
    type Err = IdpError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        SigningAlg::ALL
            .into_iter()
            .find(|a| a.as_str() == s)
            .ok_or_else(|| IdpError::invalid("alg", format!("unsupported signing algorithm {s:?}")))
    }
}

/// RSA modulus size for generated keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RsaBits {
    #[default]
    #[serde(rename = "2048")]
    B2048,
    #[serde(rename = "3072")]
    B3072,
    #[serde(rename = "4096")]
    B4096,
}

enum Pair {
    Rsa(RsaKeyPair),
    Ec(EcdsaKeyPair),
}

/// A private signing key in memory.
pub struct SigningKey {
    kid: String,
    alg: SigningAlg,
    pkcs8: Plaintext,
    pair: Pair,
    public_jwk: Value,
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SigningKey")
            .field("kid", &self.kid)
            .field("alg", &self.alg)
            .finish_non_exhaustive()
    }
}

impl SigningKey {
    /// Generates a fresh key from the OS CSPRNG.
    pub fn generate(alg: SigningAlg, rsa_bits: RsaBits) -> Result<Self, IdpError> {
        let der = if alg.is_rsa() {
            generate_rsa_pkcs8(rsa_bits)?
        } else {
            let rng = SystemRandom::new();
            let doc = EcdsaKeyPair::generate_pkcs8(alg.ecdsa_signing().expect("ec"), &rng)
                .map_err(|_| IdpError::Key("EC key generation failed".into()))?;
            doc.as_ref().to_vec()
        };
        Self::from_pkcs8(alg, &der)
    }

    /// Loads a PKCS#8 v1/v2 DER private key (generated here, or imported by
    /// an operator) for `alg`.
    pub fn from_pkcs8(alg: SigningAlg, der: &[u8]) -> Result<Self, IdpError> {
        let pair = if alg.is_rsa() {
            let kp = RsaKeyPair::from_pkcs8(der)
                .map_err(|e| IdpError::Key(format!("RSA key rejected: {e}")))?;
            if kp.public().modulus_len() < 256 {
                return Err(IdpError::Key("RSA keys must be at least 2048 bits".into()));
            }
            Pair::Rsa(kp)
        } else {
            let rng = SystemRandom::new();
            let kp = EcdsaKeyPair::from_pkcs8(alg.ecdsa_signing().expect("ec"), der, &rng)
                .map_err(|e| IdpError::Key(format!("EC key rejected for {alg}: {e}")))?;
            Pair::Ec(kp)
        };
        let mut public_jwk = public_jwk_members(alg, &pair);
        let kid = thumbprint(&public_jwk);
        let obj = public_jwk.as_object_mut().expect("object");
        obj.insert("kid".into(), Value::String(kid.clone()));
        obj.insert("use".into(), Value::String("sig".into()));
        obj.insert("alg".into(), Value::String(alg.as_str().into()));
        Ok(Self {
            kid,
            alg,
            pkcs8: Plaintext::new(der.to_vec()),
            pair,
            public_jwk,
        })
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }

    pub fn alg(&self) -> SigningAlg {
        self.alg
    }

    /// The public JWK (`kty`, `kid`, `use=sig`, `alg`, and `n`/`e` or
    /// `crv`/`x`/`y`). Never contains private members.
    pub fn public_jwk(&self) -> &Value {
        &self.public_jwk
    }

    /// Raw signature over `msg` in JWS form (RSA: PKCS#1 v1.5 / PSS;
    /// EC: fixed-width `r || s`, RFC 7518 §3.4).
    pub fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, IdpError> {
        let rng = SystemRandom::new();
        match &self.pair {
            Pair::Rsa(kp) => {
                let mut sig = vec![0u8; kp.public().modulus_len()];
                kp.sign(self.alg.rsa_encoding().expect("rsa"), &rng, msg, &mut sig)
                    .map_err(|_| IdpError::Key("RSA signing failed".into()))?;
                Ok(sig)
            }
            Pair::Ec(kp) => kp
                .sign(&rng, msg)
                .map(|s| s.as_ref().to_vec())
                .map_err(|_| IdpError::Key("ECDSA signing failed".into())),
        }
    }

    /// A compact JWS: protected header `{alg, kid, typ}` + JSON payload.
    /// `typ` is `at+jwt` for access tokens (RFC 9068 §2.1), `JWT` for
    /// id_tokens, `logout+jwt` for back-channel logout tokens.
    pub fn sign_jws<T: Serialize>(&self, typ: &str, payload: &T) -> Result<String, IdpError> {
        let header = json!({ "alg": self.alg.as_str(), "kid": self.kid, "typ": typ });
        let h = b64(&serde_json::to_vec(&header).map_err(|e| IdpError::Key(e.to_string()))?);
        let p = b64(&serde_json::to_vec(payload).map_err(|e| IdpError::Key(e.to_string()))?);
        let signing_input = format!("{h}.{p}");
        let sig = self.sign(signing_input.as_bytes())?;
        Ok(format!("{signing_input}.{}", b64(&sig)))
    }

    /// Seals the private key for storage.
    pub fn seal(&self, ring: &SecretKeyRing, realm_id: &str) -> SealedSecret {
        ring.seal(self.pkcs8.expose(), &key_aad(realm_id, &self.kid))
    }

    /// Opens a stored key and checks it is the key the row says it is.
    pub fn open(
        ring: &SecretKeyRing,
        sealed: &SealedSecret,
        realm_id: &str,
        kid: &str,
        alg: SigningAlg,
    ) -> Result<Self, IdpError> {
        let der = ring.open(sealed, &key_aad(realm_id, kid))?;
        let key = Self::from_pkcs8(alg, der.expose())?;
        if key.kid != kid {
            return Err(IdpError::Key(format!(
                "stored key thumbprint {} does not match kid {kid}",
                key.kid
            )));
        }
        Ok(key)
    }
}

/// AAD binding a sealed private key to its realm and kid.
pub fn key_aad(realm_id: &str, kid: &str) -> String {
    format!("idp-realm:{realm_id}:signing-key:{kid}")
}

fn generate_rsa_pkcs8(bits: RsaBits) -> Result<Vec<u8>, IdpError> {
    use aws_lc_rs::encoding::{AsDer, Pkcs8V1Der};
    use aws_lc_rs::rsa::{KeyPair as AwsRsa, KeySize};
    let size = match bits {
        RsaBits::B2048 => KeySize::Rsa2048,
        RsaBits::B3072 => KeySize::Rsa3072,
        RsaBits::B4096 => KeySize::Rsa4096,
    };
    let kp =
        AwsRsa::generate(size).map_err(|_| IdpError::Key("RSA key generation failed".into()))?;
    let der: Pkcs8V1Der<'static> = kp
        .as_der()
        .map_err(|_| IdpError::Key("RSA PKCS#8 encoding failed".into()))?;
    Ok(der.as_ref().to_vec())
}

/// The RFC 7638 "required members" of the public JWK.
fn public_jwk_members(alg: SigningAlg, pair: &Pair) -> Value {
    match pair {
        Pair::Rsa(kp) => {
            let c: ring::rsa::PublicKeyComponents<Vec<u8>> = kp.public().into();
            json!({ "kty": "RSA", "n": b64(&c.n), "e": b64(&c.e) })
        }
        Pair::Ec(kp) => {
            // Uncompressed SEC1 point: 0x04 || X || Y.
            let point = kp.public_key().as_ref();
            let n = (point.len() - 1) / 2;
            json!({
                "kty": "EC",
                "crv": alg.crv().expect("ec"),
                "x": b64(&point[1..1 + n]),
                "y": b64(&point[1 + n..]),
            })
        }
    }
}

/// RFC 7638 §3: SHA-256 over the required members, lexicographic order, no
/// whitespace. `serde_json::Map` is ordered (BTreeMap) without the
/// `preserve_order` feature; we build the object explicitly to not depend on it.
pub fn thumbprint(jwk: &Value) -> String {
    let required: &[&str] = match jwk.get("kty").and_then(Value::as_str) {
        Some("RSA") => &["e", "kty", "n"],
        Some("EC") => &["crv", "kty", "x", "y"],
        Some("OKP") => &["crv", "kty", "x"],
        _ => &[],
    };
    let mut s = String::from("{");
    for (i, k) in required.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let v = jwk.get(*k).and_then(Value::as_str).unwrap_or("");
        s.push_str(&serde_json::to_string(k).expect("str"));
        s.push(':');
        s.push_str(&serde_json::to_string(v).expect("str"));
    }
    s.push('}');
    sha256_b64(s.as_bytes())
}

/// Lifecycle state of a stored signing key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyStatus {
    /// Published in the JWKS, not yet signing.
    Next,
    /// Signing new tokens (one per algorithm family per realm).
    Active,
    /// Published, no longer signing; tokens it signed may still be alive.
    Retiring,
    /// Unpublished.
    Retired,
}

impl KeyStatus {
    pub fn published(self) -> bool {
        matches!(
            self,
            KeyStatus::Next | KeyStatus::Active | KeyStatus::Retiring
        )
    }
}

/// Metadata of a stored key (the `hfs_idp_signing_keys` row without the
/// sealed private part).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyRecord {
    pub kid: String,
    pub alg: SigningAlg,
    pub status: KeyStatus,
    pub public_jwk: Value,
    pub created_at: DateTime<Utc>,
    /// When it became (or becomes) `active`.
    pub activate_at: Option<DateTime<Utc>>,
    /// When it became `retiring`.
    pub retire_at: Option<DateTime<Utc>>,
    /// Unpublish after this instant (`retire_at` + longest token lifetime +
    /// JWKS cache allowance).
    pub expire_at: Option<DateTime<Utc>>,
}

/// Rotation policy of a realm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RotationPolicy {
    /// Rotate the active key after this many days (0 = manual only).
    pub rotate_after_days: u32,
    /// Publish the successor this long before it signs (≥ the longest JWKS
    /// cache lifetime of relying parties; HFS's own default is 300 s).
    pub prepublish_hours: u32,
    /// Keep a retiring key published this long after it stops signing
    /// (≥ the longest access/id token lifetime).
    pub retain_hours: u32,
}

impl Default for RotationPolicy {
    fn default() -> Self {
        Self {
            rotate_after_days: 90,
            prepublish_hours: 24,
            retain_hours: 48,
        }
    }
}

/// One step the rotation job should take now for one algorithm family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RotationAction {
    /// Generate and store a `next` key for this algorithm.
    CreateNext(SigningAlg),
    /// Promote `next` (kid) to `active` and demote the current active to `retiring`.
    Promote {
        next: String,
        demote: Option<String>,
    },
    /// Move a `retiring` key to `retired`.
    Retire(String),
}

/// The pure rotation planner (run by one instance under a cluster lease).
/// Deterministic for a given key set and instant, so two instances planning
/// at once produce the same actions (the store applies them with revision
/// checks).
pub fn plan_rotation(
    keys: &[KeyRecord],
    algs: &[SigningAlg],
    policy: &RotationPolicy,
    now: DateTime<Utc>,
) -> Vec<RotationAction> {
    let mut out = Vec::new();
    for k in keys {
        if k.status == KeyStatus::Retiring && k.expire_at.is_some_and(|e| e <= now) {
            out.push(RotationAction::Retire(k.kid.clone()));
        }
    }
    for &alg in algs {
        let active = keys
            .iter()
            .find(|k| k.alg == alg && k.status == KeyStatus::Active);
        let next = keys
            .iter()
            .find(|k| k.alg == alg && k.status == KeyStatus::Next);
        match (active, next) {
            (None, None) => out.push(RotationAction::CreateNext(alg)),
            (None, Some(n)) => out.push(RotationAction::Promote {
                next: n.kid.clone(),
                demote: None,
            }),
            (Some(a), None) => {
                if policy.rotate_after_days > 0 {
                    let since = a.activate_at.unwrap_or(a.created_at);
                    let due = since + Duration::days(policy.rotate_after_days as i64)
                        - Duration::hours(policy.prepublish_hours as i64);
                    if now >= due {
                        out.push(RotationAction::CreateNext(alg));
                    }
                }
            }
            (Some(a), Some(n)) => {
                let ready = n.created_at + Duration::hours(policy.prepublish_hours as i64);
                let since = a.activate_at.unwrap_or(a.created_at);
                let due = since + Duration::days(policy.rotate_after_days.max(1) as i64);
                if now >= ready && (policy.rotate_after_days == 0 || now >= due) {
                    // Manual mode: the next key was created on request, so
                    // promote it once it has been published long enough.
                    out.push(RotationAction::Promote {
                        next: n.kid.clone(),
                        demote: Some(a.kid.clone()),
                    });
                }
            }
        }
    }
    out
}

/// The realm's JWKS document: every published key's public JWK.
pub fn jwks_document(keys: &[KeyRecord]) -> Value {
    let list: Vec<Value> = keys
        .iter()
        .filter(|k| k.status.published())
        .map(|k| k.public_jwk.clone())
        .collect();
    let mut m = Map::new();
    m.insert("keys".into(), Value::Array(list));
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, jwk::Jwk};

    fn verify_with_jsonwebtoken(key: &SigningKey, token: &str, alg: Algorithm) -> Value {
        let jwk: Jwk = serde_json::from_value(key.public_jwk().clone()).unwrap();
        let dk = DecodingKey::from_jwk(&jwk).unwrap();
        let mut v = Validation::new(alg);
        v.set_audience(&["https://example.org/fhir"]);
        v.set_issuer(&["https://example.org/idp/test"]);
        decode::<Value>(token, &dk, &v).unwrap().claims
    }

    fn claims() -> Value {
        json!({
            "iss": "https://example.org/idp/test",
            "aud": "https://example.org/fhir",
            "sub": "u1",
            "exp": Utc::now().timestamp() + 300,
            "iat": Utc::now().timestamp(),
        })
    }

    #[test]
    fn ec_keys_sign_tokens_the_resource_server_accepts() {
        for (alg, jalg) in [
            (SigningAlg::ES256, Algorithm::ES256),
            (SigningAlg::ES384, Algorithm::ES384),
        ] {
            let k = SigningKey::generate(alg, RsaBits::B2048).unwrap();
            let t = k.sign_jws("at+jwt", &claims()).unwrap();
            let c = verify_with_jsonwebtoken(&k, &t, jalg);
            assert_eq!(c["sub"], "u1");
            let jwk = k.public_jwk();
            assert!(jwk.get("d").is_none(), "no private member in public JWK");
            assert_eq!(jwk["kid"], k.kid());
        }
    }

    #[test]
    fn rsa_keys_sign_tokens_the_resource_server_accepts() {
        let k = SigningKey::generate(SigningAlg::RS256, RsaBits::B2048).unwrap();
        assert_eq!(k.public_jwk()["kty"], "RSA");
        assert!(k.public_jwk().get("d").is_none());
        for (alg, jalg) in [
            (SigningAlg::RS256, Algorithm::RS256),
            (SigningAlg::RS384, Algorithm::RS384),
            (SigningAlg::PS256, Algorithm::PS256),
            (SigningAlg::PS384, Algorithm::PS384),
        ] {
            // Same key material, other RSA algorithm.
            let k2 = SigningKey::from_pkcs8(alg, k.pkcs8.expose()).unwrap();
            assert_eq!(k2.kid(), k.kid(), "kid is the key thumbprint, not per-alg");
            let t = k2.sign_jws("JWT", &claims()).unwrap();
            verify_with_jsonwebtoken(&k2, &t, jalg);
        }
    }

    #[test]
    fn rfc7638_thumbprint_example() {
        // RFC 7638 §3.1.
        let jwk = json!({
            "kty": "RSA",
            "n": "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgw",
            "e": "AQAB",
            "alg": "RS256",
            "kid": "2011-04-29"
        });
        assert_eq!(
            thumbprint(&jwk),
            "NzbLsXh8uDCcd-6MNwXF4W_7noWXFZAfHkxZsRGC9Xs"
        );
    }

    #[test]
    fn sealed_keys_are_bound_to_realm_and_kid() {
        let ring = SecretKeyRing::parse(&SecretKeyRing::generate_spec("k1")).unwrap();
        let k = SigningKey::generate(SigningAlg::ES256, RsaBits::B2048).unwrap();
        let sealed = k.seal(&ring, "realm-1");
        let back = SigningKey::open(&ring, &sealed, "realm-1", k.kid(), SigningAlg::ES256).unwrap();
        assert_eq!(back.public_jwk(), k.public_jwk());
        assert!(SigningKey::open(&ring, &sealed, "realm-2", k.kid(), SigningAlg::ES256).is_err());
        assert!(SigningKey::open(&ring, &sealed, "realm-1", "other", SigningAlg::ES256).is_err());
        assert!(!format!("{k:?}").contains("pkcs8"));
    }

    #[test]
    fn small_rsa_keys_refused() {
        assert!(SigningKey::from_pkcs8(SigningAlg::RS256, b"not a key").is_err());
        assert!("HS256".parse::<SigningAlg>().is_err());
        assert!("none".parse::<SigningAlg>().is_err());
    }

    fn rec(kid: &str, alg: SigningAlg, status: KeyStatus, created: DateTime<Utc>) -> KeyRecord {
        KeyRecord {
            kid: kid.into(),
            alg,
            status,
            public_jwk: json!({"kid": kid}),
            created_at: created,
            activate_at: Some(created),
            retire_at: None,
            expire_at: None,
        }
    }

    #[test]
    fn rotation_plan() {
        let now = Utc::now();
        let p = RotationPolicy::default();
        // Empty realm: create a next key per algorithm.
        assert_eq!(
            plan_rotation(&[], &[SigningAlg::RS256], &p, now),
            vec![RotationAction::CreateNext(SigningAlg::RS256)]
        );
        // Only next: promote immediately (initial bootstrap).
        let n = rec("n", SigningAlg::RS256, KeyStatus::Next, now);
        assert_eq!(
            plan_rotation(std::slice::from_ref(&n), &[SigningAlg::RS256], &p, now),
            vec![RotationAction::Promote {
                next: "n".into(),
                demote: None
            }]
        );
        // Young active key: nothing to do.
        let a = rec(
            "a",
            SigningAlg::RS256,
            KeyStatus::Active,
            now - Duration::days(1),
        );
        assert!(plan_rotation(std::slice::from_ref(&a), &[SigningAlg::RS256], &p, now).is_empty());
        // Old active key: pre-publish a successor.
        let old = rec(
            "a",
            SigningAlg::RS256,
            KeyStatus::Active,
            now - Duration::days(90),
        );
        assert_eq!(
            plan_rotation(std::slice::from_ref(&old), &[SigningAlg::RS256], &p, now),
            vec![RotationAction::CreateNext(SigningAlg::RS256)]
        );
        // Successor published long enough: promote and demote.
        let n_old = rec(
            "n",
            SigningAlg::RS256,
            KeyStatus::Next,
            now - Duration::hours(25),
        );
        assert_eq!(
            plan_rotation(&[old.clone(), n_old], &[SigningAlg::RS256], &p, now),
            vec![RotationAction::Promote {
                next: "n".into(),
                demote: Some("a".into())
            }]
        );
        // Successor too fresh: wait.
        let n_new = rec(
            "n",
            SigningAlg::RS256,
            KeyStatus::Next,
            now - Duration::hours(1),
        );
        assert!(plan_rotation(&[old, n_new], &[SigningAlg::RS256], &p, now).is_empty());
        // Expired retiring key: retire.
        let mut r = rec(
            "r",
            SigningAlg::ES256,
            KeyStatus::Retiring,
            now - Duration::days(100),
        );
        r.expire_at = Some(now - Duration::hours(1));
        assert_eq!(
            plan_rotation(&[r], &[], &p, now),
            vec![RotationAction::Retire("r".into())]
        );
    }

    #[test]
    fn jwks_publishes_only_published_states() {
        let now = Utc::now();
        let keys = vec![
            rec("n", SigningAlg::ES256, KeyStatus::Next, now),
            rec("a", SigningAlg::ES256, KeyStatus::Active, now),
            rec("r", SigningAlg::ES256, KeyStatus::Retiring, now),
            rec("x", SigningAlg::ES256, KeyStatus::Retired, now),
        ];
        let doc = jwks_document(&keys);
        let kids: Vec<&str> = doc["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k["kid"].as_str().unwrap())
            .collect();
        assert_eq!(kids, ["n", "a", "r"]);
    }
}
