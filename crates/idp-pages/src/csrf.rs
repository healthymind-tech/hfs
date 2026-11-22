//! CSRF protection of the hosted forms (design §8.5, §10.4).
//!
//! Two independent checks, both required on every POST:
//!
//! 1. **Synchronizer token bound to the browser and the step.** The first
//!    page of an interaction sets a random browser-binding cookie
//!    (`__Host-hmidp-csrf`, `HttpOnly; Secure; SameSite=Strict; Path=/`).
//!    Each form carries `csrf = HMAC-SHA256(key, cookie ‖ purpose ‖ ix)`,
//!    where `purpose` names the form (`login`, `consent`, …) and `ix` the
//!    interaction. A token from another browser, another interaction or
//!    another form does not verify; comparison is constant time. Nothing is
//!    stored server side.
//! 2. **Origin check.** When the browser sends `Origin` it must be the
//!    public origin of the IdP; when it sends `Sec-Fetch-Site` it must be
//!    `same-origin`. (Browsers send at least one of them on form POSTs; a
//!    client sending neither still needs the token.)

use axum::http::{HeaderMap, header};
use base64::Engine;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Server-side CSRF key (per process unless configured; tokens only need to
/// survive one interaction, and every instance of a cluster must share it —
/// the `hfs` wiring derives it from `HFS_SECRET_KEYS` when present).
#[derive(Clone)]
pub struct CsrfKey([u8; 32]);

impl std::fmt::Debug for CsrfKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CsrfKey(..)")
    }
}

impl CsrfKey {
    /// A random key.
    pub fn random() -> Self {
        let mut k = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut k);
        Self(k)
    }

    /// A key derived from operator key material (domain-separated).
    pub fn derive(material: &[u8]) -> Self {
        let mut mac = HmacSha256::new_from_slice(material).expect("any key length");
        mac.update(b"hfs-idp-pages/csrf/v1");
        let out = mac.finalize().into_bytes();
        let mut k = [0u8; 32];
        k.copy_from_slice(&out);
        Self(k)
    }

    /// The token of `(binding cookie, purpose, interaction)`.
    pub fn token(&self, binding: &str, purpose: &str, ix: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.0).expect("32-byte key");
        for part in [binding, purpose, ix] {
            mac.update(&(part.len() as u64).to_be_bytes());
            mac.update(part.as_bytes());
        }
        B64.encode(mac.finalize().into_bytes())
    }

    /// Constant-time verification of a submitted token.
    pub fn verify(&self, binding: &str, purpose: &str, ix: &str, submitted: &str) -> bool {
        if binding.len() < 32 {
            return false;
        }
        let expected = self.token(binding, purpose, ix);
        expected.as_bytes().ct_eq(submitted.as_bytes()).into()
    }
}

/// A new browser-binding value (256 bits).
pub fn new_binding() -> String {
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    B64.encode(b)
}

/// Whether the request's `Origin` / `Sec-Fetch-Site` allow a state change
/// for an IdP served at `public_origin` (`https://host[:port]`).
pub fn same_origin(headers: &HeaderMap, public_origin: &str) -> bool {
    if let Some(origin) = headers.get(header::ORIGIN) {
        match origin.to_str() {
            Ok(o) if o.eq_ignore_ascii_case(public_origin) => {}
            _ => return false,
        }
    }
    if let Some(site) = headers.get("sec-fetch-site")
        && site.to_str().map(|s| s != "same-origin").unwrap_or(true)
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn token_is_bound_to_browser_form_and_interaction() {
        let k = CsrfKey::random();
        let b = new_binding();
        let t = k.token(&b, "login", "ix1");
        assert!(k.verify(&b, "login", "ix1", &t));
        assert!(
            !k.verify(&new_binding(), "login", "ix1", &t),
            "other browser"
        );
        assert!(!k.verify(&b, "consent", "ix1", &t), "other form");
        assert!(!k.verify(&b, "login", "ix2", &t), "other interaction");
        assert!(!k.verify(&b, "login", "ix1", ""), "missing");
        assert!(!k.verify("short", "login", "ix1", &k.token("short", "login", "ix1")));
        assert!(
            !CsrfKey::random().verify(&b, "login", "ix1", &t),
            "other key"
        );
        // Length-prefixing: ("ab","c") ≠ ("a","bc").
        assert_ne!(k.token(&b, "ab", "c"), k.token(&b, "a", "bc"));
        assert_eq!(
            CsrfKey::derive(b"m").token(&b, "x", "y"),
            CsrfKey::derive(b"m").token(&b, "x", "y")
        );
    }

    #[test]
    fn origin_rules() {
        let o = "https://hfs.example";
        let mut h = HeaderMap::new();
        assert!(same_origin(&h, o), "no headers: token decides");
        h.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://hfs.example"),
        );
        assert!(same_origin(&h, o));
        h.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        assert!(!same_origin(&h, o));
        h.insert(header::ORIGIN, HeaderValue::from_static("null"));
        assert!(!same_origin(&h, o));
        let mut h = HeaderMap::new();
        h.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!same_origin(&h, o));
        h.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        assert!(same_origin(&h, o));
    }
}
