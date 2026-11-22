//! PKCE (RFC 7636). Only `S256`: SMART App Launch 2.2 ("SHALL NOT support
//! the plain method"), OAuth 2.1 §4.1.1 and RFC 9700 §2.1.1.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use crate::error::OAuthError;

/// The only supported `code_challenge_method`.
pub const METHOD_S256: &str = "S256";

/// Checks the `code_challenge` / `code_challenge_method` pair at the
/// authorization endpoint. A missing method is refused rather than read as
/// `plain` (RFC 7636 §4.3 default), because `plain` is not supported.
pub fn check_challenge(challenge: Option<&str>, method: Option<&str>) -> Result<(), OAuthError> {
    let challenge =
        challenge.ok_or_else(|| OAuthError::invalid_request("code_challenge is required"))?;
    match method {
        Some(METHOD_S256) => {}
        Some(_) | None => {
            return Err(OAuthError::invalid_request(
                "code_challenge_method must be S256",
            ));
        }
    }
    // base64url(SHA-256) without padding is exactly 43 characters.
    if challenge.len() != 43 || URL_SAFE_NO_PAD.decode(challenge).is_err() {
        return Err(OAuthError::invalid_request(
            "code_challenge is not a base64url SHA-256 digest",
        ));
    }
    Ok(())
}

/// RFC 7636 §4.1: 43..128 characters of `[A-Z] / [a-z] / [0-9] / "-" / "." / "_" / "~"`.
pub fn verifier_syntax_ok(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// The S256 challenge of a verifier.
pub fn s256_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Verifies `code_verifier` against the stored S256 challenge at the token
/// endpoint (constant-time comparison). Failure is `invalid_grant`.
pub fn verify(verifier: Option<&str>, stored_challenge: &str) -> Result<(), OAuthError> {
    let verifier =
        verifier.ok_or_else(|| OAuthError::invalid_grant("code_verifier is required"))?;
    if !verifier_syntax_ok(verifier) {
        return Err(OAuthError::invalid_grant("code_verifier is malformed"));
    }
    let computed = s256_challenge(verifier);
    if crate::util::ct_eq(computed.as_bytes(), stored_challenge.as_bytes()) {
        Ok(())
    } else {
        Err(OAuthError::invalid_grant("code_verifier does not match"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 7636 Appendix B.
    const V: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const C: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    #[test]
    fn rfc7636_appendix_b() {
        assert_eq!(s256_challenge(V), C);
        assert!(verify(Some(V), C).is_ok());
    }

    #[test]
    fn plain_and_missing_method_refused() {
        assert!(check_challenge(Some(C), Some("S256")).is_ok());
        assert!(check_challenge(Some(C), Some("plain")).is_err());
        assert!(check_challenge(Some(C), None).is_err());
        assert!(check_challenge(None, Some("S256")).is_err());
        assert!(check_challenge(Some("short"), Some("S256")).is_err());
    }

    #[test]
    fn verifier_rules() {
        assert!(verify(None, C).is_err());
        assert!(verify(Some("a"), C).is_err());
        assert!(verify(Some(&"a".repeat(129)), C).is_err());
        assert!(verify(Some(&format!("{V}+")), C).is_err());
        let other = "x".repeat(43);
        assert_eq!(
            verify(Some(&other), C).unwrap_err().code,
            crate::error::OAuthErrorCode::InvalidGrant
        );
    }
}
