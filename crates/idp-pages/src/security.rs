//! Response hardening of every hosted page (design §10.4: clickjacking,
//! XSS, token leakage).
//!
//! * **CSP** `default-src 'self'`, scripts only from `'self'` plus a
//!   per-response nonce (the inline theme boot script and the form_post
//!   auto-submit), no `unsafe-inline`, `object-src 'none'`, `base-uri
//!   'none'`, `form-action` limited to `'self'` plus the origin a submission
//!   may be redirected to (the client's registered redirect URI on the
//!   sign-in, picker and consent pages; the registered post-logout URI on
//!   the sign-out page) — browsers apply `form-action` to redirects too.
//! * **Clickjacking**: `frame-ancestors 'none'` and `X-Frame-Options: DENY`
//!   by default. A realm may allow EHR embedding for listed `https` origins
//!   (`branding.frameAncestors`); then `frame-ancestors` names exactly those
//!   origins and `X-Frame-Options` is omitted (it cannot express a list).
//! * `Referrer-Policy: same-origin` (nothing — not even the `ix` handle in
//!   the URL — leaves for another origin; same-origin form posts keep a real
//!   `Origin` header, which `no-referrer` would turn into `Origin: null`),
//!   `Cache-Control: no-store`,
//!   `X-Content-Type-Options: nosniff`, `Cross-Origin-Opener-Policy:
//!   same-origin`, a closed `Permissions-Policy`.

use axum::http::{HeaderMap, HeaderValue, header};
use base64::Engine;
use rand::RngCore;

/// A fresh CSP nonce (128 bits, base64url).
pub fn nonce() -> String {
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// Origins a realm allows to frame its pages: `branding.frameAncestors`,
/// keeping only well-formed `https://host[:port]` origins (anything else —
/// `*`, paths, `http:` — is ignored, so a typo can only make framing
/// stricter).
pub fn frame_ancestors(branding: &serde_json::Value) -> Vec<String> {
    branding
        .get("frameAncestors")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str())
                .filter_map(normalize_origin)
                .collect()
        })
        .unwrap_or_default()
}

fn normalize_origin(s: &str) -> Option<String> {
    let u = url::Url::parse(s.trim()).ok()?;
    if u.scheme() != "https" || u.host_str().is_none() {
        return None;
    }
    if !(u.path() == "/" || u.path().is_empty()) || u.query().is_some() || u.fragment().is_some() {
        return None;
    }
    if !u.username().is_empty() || u.password().is_some() {
        return None;
    }
    Some(u.origin().ascii_serialization())
}

/// The Content-Security-Policy of a hosted page.
pub fn csp(nonce: &str, frame_ancestors: &[String], extra_form_action: Option<&str>) -> String {
    let fa = if frame_ancestors.is_empty() {
        "'none'".to_string()
    } else {
        frame_ancestors.join(" ")
    };
    let form_action = match extra_form_action {
        Some(o) => format!("'self' {o}"),
        None => "'self'".to_string(),
    };
    format!(
        "default-src 'self'; script-src 'self' 'nonce-{nonce}'; style-src 'self'; \
         img-src 'self' https: data:; font-src 'self'; connect-src 'self'; \
         object-src 'none'; base-uri 'none'; form-action {form_action}; \
         frame-ancestors {fa}"
    )
}

/// Applies the hardening headers to a response.
pub fn apply(
    headers: &mut HeaderMap,
    nonce: &str,
    frame_ancestors: &[String],
    extra_form_action: Option<&str>,
) {
    let set = |h: &mut HeaderMap, name: header::HeaderName, value: String| {
        if let Ok(v) = HeaderValue::from_str(&value) {
            h.insert(name, v);
        }
    };
    set(
        headers,
        header::CONTENT_SECURITY_POLICY,
        csp(nonce, frame_ancestors, extra_form_action),
    );
    if frame_ancestors.is_empty() {
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    } else {
        headers.remove(header::X_FRAME_OPTIONS);
    }
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "cross-origin-opener-policy",
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=(), payment=()"),
    );
}

/// Stamps `nonce` on every `<script` start tag of a chrome fragment (the
/// shared theme boot script is inline by design: it must run before first
/// paint).
pub fn with_nonce(fragment: &str, nonce: &str) -> String {
    fragment.replace("<script>", &format!("<script nonce=\"{nonce}\">"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_denies_framing_and_inline_script() {
        let c = csp("abc", &[], None);
        assert!(c.contains("frame-ancestors 'none'"));
        assert!(c.contains("script-src 'self' 'nonce-abc'"));
        assert!(!c.contains("unsafe-inline"));
        assert!(c.contains("form-action 'self';"));
        let mut h = HeaderMap::new();
        apply(&mut h, "abc", &[], None);
        assert_eq!(h[header::X_FRAME_OPTIONS], "DENY");
        assert_eq!(h[header::REFERRER_POLICY], "same-origin");
        assert_eq!(h[header::CACHE_CONTROL], "no-store");
    }

    #[test]
    fn frame_ancestor_allow_list_is_https_origins_only() {
        let b = serde_json::json!({"frameAncestors": [
            "https://ehr.example", "https://ehr2.example:8443/", "*", "http://plain.example",
            "https://x.example/path", "javascript:alert(1)", "https://u:p@y.example", 7
        ]});
        assert_eq!(
            frame_ancestors(&b),
            vec!["https://ehr.example", "https://ehr2.example:8443"]
        );
        let mut h = HeaderMap::new();
        apply(&mut h, "n", &frame_ancestors(&b), None);
        assert!(h.get(header::X_FRAME_OPTIONS).is_none());
        let c = h[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .to_string();
        assert!(c.ends_with("frame-ancestors https://ehr.example https://ehr2.example:8443"));
        assert!(frame_ancestors(&serde_json::Value::Null).is_empty());
    }

    #[test]
    fn nonce_is_stamped_on_inline_scripts() {
        assert_eq!(
            with_nonce("<script>x</script>", "N"),
            "<script nonce=\"N\">x</script>"
        );
        assert_ne!(nonce(), nonce());
    }
}
