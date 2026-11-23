//! CSRF helpers for console pages (design §7.3; package P3).
//!
//! With console sign-in installed, the gate
//! ([`helios_admin_auth::console::guard::gate`]) refuses every
//! state-changing cookie-authenticated `/ui` request that is not same-site
//! **and** does not carry the session's synchronizer token — header
//! `X-CSRF-Token` or form field `_csrf`. Pages get the token two ways:
//!
//! * the layout loads `assets/csrf.js`, which reads the `hfs_csrf` cookie and
//!   adds the token to htmx requests, same-origin `fetch` writes and plain
//!   post forms — so existing pages need no change;
//! * templates that render their own forms (the console's sign-in, account
//!   and settings pages) put [`CsrfField::html`] in the form, which works
//!   without JavaScript.

use axum::http::request::Parts;
use helios_admin_auth::console::guard::CsrfToken;

/// The token of the request's console session, if any.
#[derive(Clone, Debug, Default)]
pub struct CsrfField(pub Option<String>);

impl CsrfField {
    /// From the request extensions the gate stamped.
    pub fn from_parts(parts: &Parts) -> Self {
        Self(parts.extensions.get::<CsrfToken>().map(|t| t.0.clone()))
    }

    /// `<input type="hidden" name="_csrf" value="…">`, or nothing without a
    /// session. The token is base64url, so it needs no escaping.
    pub fn html(&self) -> String {
        match &self.0 {
            Some(t)
                if t.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
            {
                format!(r#"<input type="hidden" name="_csrf" value="{t}">"#)
            }
            _ => String::new(),
        }
    }
}

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for CsrfField {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self::from_parts(parts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_field_only_for_safe_tokens() {
        assert_eq!(CsrfField(None).html(), "");
        assert_eq!(
            CsrfField(Some("aB-_9".into())).html(),
            r#"<input type="hidden" name="_csrf" value="aB-_9">"#
        );
        assert_eq!(CsrfField(Some("<x>".into())).html(), "");
    }

    #[test]
    fn csrf_script_is_embedded_and_layout_loads_it() {
        let js = crate::Assets::get("csrf.js").expect("assets/csrf.js embedded");
        let js = String::from_utf8_lossy(&js.data);
        for needle in ["hfs_csrf", "X-CSRF-Token", "htmx:configRequest", "_csrf"] {
            assert!(js.contains(needle), "{needle}");
        }
        let layout = include_str!("../templates/layouts/base.html");
        assert!(layout.contains("/csrf.js\" defer"));
    }
}
