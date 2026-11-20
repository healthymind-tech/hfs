//! Secrets at rest: AES-256-GCM envelopes under an operator-held key ring.
//!
//! Every credential the manager stores in PostgreSQL — the console's OIDC
//! client secret, an endpoint's terminology token, a sync target's client
//! secret / private key / password — is a [`SealedSecret`]:
//!
//! ```text
//! hmsec:v1:<key-id>:<base64url nonce (12 bytes)>:<base64url ciphertext+tag>
//! ```
//!
//! sealed with the active key of a [`SecretKeyRing`] and bound by AAD to the
//! place it is stored (`endpoint:{id}:terminology.token`,
//! `sync-job:{id}:target.auth.clientSecret`, `console:oidc.clientSecret`), so
//! an envelope copied into another row or field does not open.
//!
//! The key ring comes from the environment, never from the database:
//! `HFS_SECRET_KEYS` (or `HFS_SECRET_KEYS_FILE`) = `id:base64key[,id:base64key…]`,
//! 32-byte keys, the **first** is active (new seals), the others only open
//! (rotation: prepend a new key, restart, re-seal with the admin
//! `POST /admin/secrets/$rewrap`, then drop the old key). Without a key ring
//! the server runs, but every API call that would store a secret answers
//! `409` "configure HFS_SECRET_KEYS" — fail closed, never plaintext.
//!
//! The admin API never returns secrets: responses carry `{"set": true}`
//! ([`redact_sealed`]); requests carry a [`SecretInput`].

use std::fmt;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::{Deserialize, Serialize};

/// Envelope prefix (format version 1).
pub const ENVELOPE_PREFIX: &str = "hmsec:v1:";

/// Environment variable holding the key ring.
pub const KEYS_ENV: &str = "HFS_SECRET_KEYS";
/// Environment variable naming a file that holds the key ring (wins).
pub const KEYS_FILE_ENV: &str = "HFS_SECRET_KEYS_FILE";

/// Why sealing or opening failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    /// No key ring configured.
    #[error("no secret key configured (set {KEYS_ENV} or {KEYS_FILE_ENV})")]
    NoKeyRing,
    /// The key ring specification is malformed.
    #[error("invalid secret key ring: {0}")]
    BadKeyRing(String),
    /// The envelope is not `hmsec:v1:…`.
    #[error("not a sealed secret envelope")]
    BadEnvelope,
    /// The envelope names a key the ring does not hold.
    #[error("sealed with unknown key id {0:?}")]
    UnknownKey(String),
    /// Authentication failed: wrong key, wrong AAD, or tampering.
    #[error("sealed secret does not open (wrong key or context, or tampered)")]
    Open,
}

/// A sealed secret envelope as stored (serialized as its envelope string).
///
/// `Debug` and `Display` never print the ciphertext.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SealedSecret(String);

impl SealedSecret {
    /// Wraps a stored envelope after a syntax check.
    pub fn from_envelope(s: impl Into<String>) -> Result<Self, SecretError> {
        let s = s.into();
        parse_envelope(&s)?;
        Ok(Self(s))
    }

    /// The envelope string (for storage only).
    pub fn envelope(&self) -> &str {
        &self.0
    }

    /// Key id this envelope was sealed with.
    pub fn key_id(&self) -> &str {
        parse_envelope(&self.0).map(|p| p.0).unwrap_or("")
    }
}

impl TryFrom<String> for SealedSecret {
    type Error = SecretError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        SealedSecret::from_envelope(s)
    }
}

impl From<SealedSecret> for String {
    fn from(s: SealedSecret) -> String {
        s.0
    }
}

impl fmt::Debug for SealedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SealedSecret(key={})", self.key_id())
    }
}

fn parse_envelope(s: &str) -> Result<(&str, &str, &str), SecretError> {
    let rest = s
        .strip_prefix(ENVELOPE_PREFIX)
        .ok_or(SecretError::BadEnvelope)?;
    let mut it = rest.split(':');
    let (Some(kid), Some(nonce), Some(ct), None) = (it.next(), it.next(), it.next(), it.next())
    else {
        return Err(SecretError::BadEnvelope);
    };
    if kid.is_empty() || nonce.is_empty() || ct.is_empty() {
        return Err(SecretError::BadEnvelope);
    }
    Ok((kid, nonce, ct))
}

/// A plaintext secret in memory. `Debug` redacts; the bytes are wiped on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct Plaintext(Vec<u8>);

impl Plaintext {
    /// Wraps bytes.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    /// The bytes.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// The bytes as UTF-8, if they are.
    pub fn expose_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }
}

impl Drop for Plaintext {
    fn drop(&mut self) {
        self.0.fill(0);
        // Keep the wipe from being optimized away as a dead store.
        std::hint::black_box(&self.0);
    }
}

impl fmt::Debug for Plaintext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Plaintext([redacted])")
    }
}

/// The operator's keys. The first key seals; all of them open.
#[derive(Clone)]
pub struct SecretKeyRing {
    keys: Vec<(String, [u8; 32])>,
}

impl fmt::Debug for SecretKeyRing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ids: Vec<&str> = self.keys.iter().map(|(id, _)| id.as_str()).collect();
        f.debug_struct("SecretKeyRing")
            .field("key_ids", &ids)
            .finish()
    }
}

impl SecretKeyRing {
    /// Parses `id:base64key[,id:base64key…]` (standard or URL-safe base64,
    /// 32 decoded bytes, ids `[A-Za-z0-9_-]{1,32}`, unique).
    pub fn parse(spec: &str) -> Result<Self, SecretError> {
        let mut keys: Vec<(String, [u8; 32])> = Vec::new();
        for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (id, b64) = part
                .split_once(':')
                .ok_or_else(|| SecretError::BadKeyRing("expected id:base64key".into()))?;
            if id.is_empty()
                || id.len() > 32
                || !id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(SecretError::BadKeyRing(format!("bad key id {id:?}")));
            }
            if keys.iter().any(|(k, _)| k == id) {
                return Err(SecretError::BadKeyRing(format!("duplicate key id {id:?}")));
            }
            let raw = STANDARD
                .decode(b64.trim())
                .or_else(|_| URL_SAFE_NO_PAD.decode(b64.trim().trim_end_matches('=')))
                .map_err(|_| SecretError::BadKeyRing(format!("key {id:?} is not base64")))?;
            let key: [u8; 32] = raw.try_into().map_err(|_| {
                SecretError::BadKeyRing(format!("key {id:?} must decode to 32 bytes"))
            })?;
            keys.push((id.to_string(), key));
        }
        if keys.is_empty() {
            return Err(SecretError::BadKeyRing("no keys".into()));
        }
        Ok(Self { keys })
    }

    /// Reads [`KEYS_FILE_ENV`] (wins) or [`KEYS_ENV`]. `Ok(None)` when
    /// neither is set; an unreadable file or a bad spec is an error (the
    /// server should refuse to start rather than silently lose secrets).
    pub fn from_env() -> Result<Option<Self>, SecretError> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// [`from_env`](Self::from_env) over an injectable lookup (tests).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, SecretError> {
        if let Some(path) = get(KEYS_FILE_ENV).filter(|p| !p.trim().is_empty()) {
            let spec = std::fs::read_to_string(path.trim())
                .map_err(|e| SecretError::BadKeyRing(format!("{KEYS_FILE_ENV}: {e}")))?;
            return Self::parse(spec.trim()).map(Some);
        }
        match get(KEYS_ENV).filter(|s| !s.trim().is_empty()) {
            Some(spec) => Self::parse(&spec).map(Some),
            None => Ok(None),
        }
    }

    /// Generates a fresh random key spec `id:base64` (for docs / tooling).
    pub fn generate_spec(id: &str) -> String {
        let mut key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        format!("{id}:{}", STANDARD.encode(key))
    }

    /// Id of the active (sealing) key.
    pub fn active_key_id(&self) -> &str {
        &self.keys[0].0
    }

    /// Seals `plaintext` bound to `aad`.
    pub fn seal(&self, plaintext: &[u8], aad: &str) -> SealedSecret {
        let (kid, key) = &self.keys[0];
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
        let mut nonce = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let ct = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: aad.as_bytes(),
                },
            )
            .expect("AES-GCM encryption of an in-memory buffer cannot fail");
        SealedSecret(format!(
            "{ENVELOPE_PREFIX}{kid}:{}:{}",
            URL_SAFE_NO_PAD.encode(nonce),
            URL_SAFE_NO_PAD.encode(ct)
        ))
    }

    /// Seals a UTF-8 string.
    pub fn seal_str(&self, plaintext: &str, aad: &str) -> SealedSecret {
        self.seal(plaintext.as_bytes(), aad)
    }

    /// Opens an envelope sealed with `aad`.
    pub fn open(&self, sealed: &SealedSecret, aad: &str) -> Result<Plaintext, SecretError> {
        let (kid, nonce_b64, ct_b64) = parse_envelope(&sealed.0)?;
        let key = self
            .keys
            .iter()
            .find(|(id, _)| id == kid)
            .map(|(_, k)| k)
            .ok_or_else(|| SecretError::UnknownKey(kid.to_string()))?;
        let nonce = URL_SAFE_NO_PAD
            .decode(nonce_b64)
            .map_err(|_| SecretError::BadEnvelope)?;
        if nonce.len() != 12 {
            return Err(SecretError::BadEnvelope);
        }
        let ct = URL_SAFE_NO_PAD
            .decode(ct_b64)
            .map_err(|_| SecretError::BadEnvelope)?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
        cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &ct,
                    aad: aad.as_bytes(),
                },
            )
            .map(Plaintext)
            .map_err(|_| SecretError::Open)
    }

    /// Whether `sealed` should be re-sealed under the active key.
    pub fn needs_rewrap(&self, sealed: &SealedSecret) -> bool {
        sealed.key_id() != self.active_key_id()
    }

    /// Re-seals under the active key (same AAD).
    pub fn rewrap(&self, sealed: &SealedSecret, aad: &str) -> Result<SealedSecret, SecretError> {
        let plain = self.open(sealed, aad)?;
        Ok(self.seal(plain.expose(), aad))
    }
}

/// How an API request changes a secret field.
///
/// JSON: `{"set": "<plaintext>"}`, `"keep"` (default when the field is
/// omitted), `"clear"`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SecretInput {
    /// Store this new value.
    Set(String),
    /// Leave the stored value as it is.
    #[default]
    Keep,
    /// Remove the stored value.
    Clear,
}

impl fmt::Debug for SecretInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretInput::Set(_) => f.write_str("Set([redacted])"),
            SecretInput::Keep => f.write_str("Keep"),
            SecretInput::Clear => f.write_str("Clear"),
        }
    }
}

impl SecretInput {
    /// Applies the input to the currently stored value.
    pub fn apply(
        &self,
        current: Option<SealedSecret>,
        ring: Option<&SecretKeyRing>,
        aad: &str,
    ) -> Result<Option<SealedSecret>, SecretError> {
        match self {
            SecretInput::Keep => Ok(current),
            SecretInput::Clear => Ok(None),
            SecretInput::Set(v) => Ok(Some(ring.ok_or(SecretError::NoKeyRing)?.seal_str(v, aad))),
        }
    }
}

/// Replaces every sealed envelope string in a JSON tree with `{"set": true}`.
pub fn redact_sealed(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::String(s) if s.starts_with(ENVELOPE_PREFIX) => {
            *v = serde_json::json!({ "set": true });
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(redact_sealed),
        serde_json::Value::Object(o) => o.values_mut().for_each(redact_sealed),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring() -> SecretKeyRing {
        SecretKeyRing::parse(&SecretKeyRing::generate_spec("k1")).unwrap()
    }

    #[test]
    fn seal_open_roundtrip_and_aad_binding() {
        let r = ring();
        let s = r.seal_str("s3cret", "endpoint:abc:terminology.token");
        assert!(s.envelope().starts_with("hmsec:v1:k1:"));
        assert!(!s.envelope().contains("s3cret"));
        assert_eq!(
            r.open(&s, "endpoint:abc:terminology.token")
                .unwrap()
                .expose_str(),
            Some("s3cret")
        );
        assert_eq!(
            r.open(&s, "endpoint:xyz:terminology.token"),
            Err(SecretError::Open)
        );
        // Two seals of the same value differ (random nonce).
        assert_ne!(r.seal_str("x", "a"), r.seal_str("x", "a"));
    }

    #[test]
    fn tamper_and_wrong_key() {
        let r = ring();
        let s = r.seal_str("v", "a");
        let mut env = s.envelope().to_string();
        let last = env.pop().unwrap();
        env.push(if last == 'A' { 'B' } else { 'A' });
        let t = SealedSecret::from_envelope(env).unwrap();
        assert_eq!(r.open(&t, "a"), Err(SecretError::Open));
        let other = SecretKeyRing::parse(&SecretKeyRing::generate_spec("k1")).unwrap();
        assert_eq!(other.open(&s, "a"), Err(SecretError::Open));
        let k2 = SecretKeyRing::parse(&SecretKeyRing::generate_spec("k2")).unwrap();
        assert_eq!(k2.open(&s, "a"), Err(SecretError::UnknownKey("k1".into())));
    }

    #[test]
    fn rotation() {
        let old = SecretKeyRing::generate_spec("old");
        let new = SecretKeyRing::generate_spec("new");
        let before = SecretKeyRing::parse(&old).unwrap();
        let s = before.seal_str("v", "a");
        let after = SecretKeyRing::parse(&format!("{new},{old}")).unwrap();
        assert_eq!(after.active_key_id(), "new");
        assert!(after.needs_rewrap(&s));
        let re = after.rewrap(&s, "a").unwrap();
        assert!(!after.needs_rewrap(&re));
        assert_eq!(after.open(&re, "a").unwrap().expose_str(), Some("v"));
    }

    #[test]
    fn keyring_parse_errors() {
        assert!(SecretKeyRing::parse("").is_err());
        assert!(SecretKeyRing::parse("k1").is_err());
        assert!(SecretKeyRing::parse("k1:aGVsbG8=").is_err()); // 5 bytes
        let one = SecretKeyRing::generate_spec("k1");
        assert!(SecretKeyRing::parse(&format!("{one},{one}")).is_err());
        assert!(SecretKeyRing::from_lookup(|_| None).unwrap().is_none());
        let got = SecretKeyRing::from_lookup(|k| (k == KEYS_ENV).then(|| one.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(got.active_key_id(), "k1");
        assert!(!format!("{got:?}").contains(one.split(':').nth(1).unwrap()));
    }

    #[test]
    fn serde_and_redaction() {
        let r = ring();
        let s = r.seal_str("v", "a");
        let json = serde_json::to_value(&s).unwrap();
        assert!(json.as_str().unwrap().starts_with(ENVELOPE_PREFIX));
        let back: SealedSecret = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(back, s);
        assert!(serde_json::from_str::<SealedSecret>("\"plain\"").is_err());
        let mut tree = serde_json::json!({"a": {"token": json}, "b": ["x"]});
        redact_sealed(&mut tree);
        assert_eq!(tree["a"]["token"], serde_json::json!({"set": true}));
        assert!(!format!("{s:?}").contains(s.envelope()));
    }

    #[test]
    fn secret_input() {
        let r = ring();
        let cur = Some(r.seal_str("old", "a"));
        assert_eq!(
            SecretInput::Keep.apply(cur.clone(), None, "a").unwrap(),
            cur
        );
        assert_eq!(
            SecretInput::Clear.apply(cur.clone(), None, "a").unwrap(),
            None
        );
        assert_eq!(
            SecretInput::Set("n".into()).apply(cur.clone(), None, "a"),
            Err(SecretError::NoKeyRing)
        );
        let n = SecretInput::Set("n".into())
            .apply(cur, Some(&r), "a")
            .unwrap()
            .unwrap();
        assert_eq!(r.open(&n, "a").unwrap().expose_str(), Some("n"));
        let parsed: SecretInput = serde_json::from_str(r#"{"set":"x"}"#).unwrap();
        assert_eq!(parsed, SecretInput::Set("x".into()));
        let keep: SecretInput = serde_json::from_str(r#""keep""#).unwrap();
        assert_eq!(keep, SecretInput::Keep);
        assert_eq!(format!("{parsed:?}"), "Set([redacted])");
    }
}
