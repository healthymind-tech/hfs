//! The contract between the protocol endpoints (`helios-idp-server`, package
//! I1) and the hosted pages (`helios-idp-pages`, package I4).
//!
//! 1. `GET|POST /idp/{realm}/authorize` validates the request completely
//!    (client, exact redirect URI, PKCE, `aud`, scopes, launch handle) and
//!    stores a [`PendingAuthorization`] keyed by the digest of an opaque
//!    `hmix_…` handle. It then answers `303 /idp/{realm}/login?ix={handle}`.
//! 2. The pages authenticate the user (SSO cookie `__Host-hmidp-{realm}`),
//!    run the patient picker and the consent screen, recording progress with
//!    [`crate::store::IdpStore::update_interaction`].
//! 3. They finish with [`AuthorizationCompleter::complete`] (or `deny`). The
//!    completer consumes the interaction once, re-checks the decision against
//!    what was requested, issues the code and returns the
//!    [`AuthorizationResponse`] (redirect with `code`, `state`, `iss`; or a
//!    form_post), which the pages send to the browser.
//!
//! Nothing in a pending authorization is trusted from the browser after
//! step 1: the page only carries the handle.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{OAuthError, OAuthErrorCode};
use crate::launch::LaunchContext;

/// `response_mode` (OAuth 2.0 Multiple Response Types §2.1, Form Post
/// Response Mode). `fragment` is refused (no front-channel tokens; and a
/// code in a fragment is pointless for code-only servers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResponseMode {
    #[default]
    Query,
    FormPost,
}

/// OIDC `prompt` values (OIDC Core §3.1.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Prompt {
    None,
    Login,
    Consent,
    SelectAccount,
}

/// What the pages must still do, derived by `/authorize`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractionNeeds {
    /// `launch/patient` granted and no patient from an EHR launch: show the
    /// patient picker (standalone launch).
    pub patient_picker: bool,
    /// `launch/encounter` granted and no encounter from an EHR launch.
    pub encounter_picker: bool,
    /// Consent screen required (client not trusted, or `prompt=consent`).
    /// The pages may still skip it when a stored consent covers every
    /// requested scope for (user, client, endpoint) and `prompt` does not
    /// contain `consent`.
    pub consent: bool,
}

/// An authorization request that passed every protocol check and waits for
/// the user (`hfs_idp_interactions`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingAuthorization {
    /// SHA-256 of the `hmix_…` handle.
    pub id_hash: String,
    pub realm_id: String,
    pub realm_name: String,
    pub client_id: String,
    /// Display name of the client (consent screen).
    pub client_name: String,
    /// Exact redirect URI from the request (already matched).
    pub redirect_uri: String,
    pub response_mode: ResponseMode,
    pub state: Option<String>,
    pub nonce: Option<String>,
    /// S256 challenge.
    pub code_challenge: String,
    /// Requested scopes already intersected with the client policy, in
    /// request order. The user may grant a subset.
    pub scopes: Vec<String>,
    /// Requested scopes the client may not have (shown to nobody; logged).
    #[serde(default)]
    pub dropped_scopes: Vec<String>,
    /// The bound FHIR endpoint (name) the token is for, and its `aud` URL.
    pub endpoint: String,
    pub audience: String,
    #[serde(default)]
    pub prompt: Vec<Prompt>,
    pub max_age: Option<u64>,
    pub login_hint: Option<String>,
    #[serde(default)]
    pub ui_locales: Vec<String>,
    /// Context fixed by an EHR launch handle (patient, encounter,
    /// fhirContext, need_patient_banner, intent, smart_style_url, tenant).
    /// The pages cannot override what is set here.
    #[serde(default)]
    pub launch_context: LaunchContext,
    /// Whether the request carried a `launch` handle (EHR launch).
    #[serde(default)]
    pub ehr_launch: bool,
    /// The console user who created the launch handle (console-source realms).
    #[serde(default)]
    pub launched_by: Option<String>,
    pub needs: InteractionNeeds,
    /// Progress recorded by the pages (not trusted for anything security
    /// relevant: `complete` re-validates the decision).
    #[serde(default)]
    pub progress: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl PendingAuthorization {
    pub fn has_prompt(&self, p: Prompt) -> bool {
        self.prompt.contains(&p)
    }
}

/// The user's decision, produced by the hosted pages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    /// Realm user id (becomes `sub`).
    pub subject: String,
    /// OIDC `sid` of the SSO session used.
    pub sid: Option<String>,
    /// Digest of the SSO session cookie (binds `online_access` families).
    pub session_id_hash: Option<String>,
    pub auth_time: DateTime<Utc>,
    #[serde(default)]
    pub amr: Vec<String>,
    pub acr: Option<String>,
    /// Scopes the user approved: must be a subset of
    /// [`PendingAuthorization::scopes`].
    pub granted_scopes: Vec<String>,
    /// Context chosen in the pages (patient picker, encounter picker).
    /// Fields already set by an EHR launch are ignored here.
    #[serde(default)]
    pub context: LaunchContext,
    /// SMART `fhirUser` (absolute URL) for the audience endpoint, if any.
    pub fhir_user: Option<String>,
}

/// The authorization response to send to the user agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationResponse {
    pub redirect_uri: String,
    pub mode: ResponseMode,
    /// `code`/`error`, `error_description`, `state`, `iss` (RFC 9207).
    pub params: Vec<(String, String)>,
}

impl AuthorizationResponse {
    /// An error response for a request whose client and redirect URI were
    /// already verified (RFC 6749 §4.1.2.1). Never call this with an
    /// unverified redirect URI.
    pub fn error(
        redirect_uri: &str,
        mode: ResponseMode,
        err: &OAuthError,
        state: Option<&str>,
        issuer: &str,
    ) -> Self {
        let mut params = vec![("error".to_string(), err.code.as_str().to_string())];
        if !err.description.is_empty() {
            params.push(("error_description".into(), err.description.clone()));
        }
        if let Some(s) = state {
            params.push(("state".into(), s.to_string()));
        }
        params.push(("iss".into(), issuer.to_string()));
        Self {
            redirect_uri: redirect_uri.to_string(),
            mode,
            params,
        }
    }

    /// The URL for a `303` (query mode): parameters appended to any query
    /// the registered redirect URI already has (RFC 6749 §3.1.2).
    pub fn redirect_url(&self) -> String {
        match Url::parse(&self.redirect_uri) {
            Ok(mut u) => {
                {
                    let mut q = u.query_pairs_mut();
                    for (k, v) in &self.params {
                        q.append_pair(k, v);
                    }
                }
                u.to_string()
            }
            // Registered URIs are validated absolute URLs; unreachable.
            Err(_) => self.redirect_uri.clone(),
        }
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn html_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

impl AuthorizationResponse {
    /// The auto-submitting page of the Form Post Response Mode. Serve it
    /// with `Content-Security-Policy: default-src 'none'; script-src
    /// 'nonce-{nonce}'; form-action {redirect origin}` (see
    /// [`AuthorizationResponse::form_post_csp`]) and `Cache-Control: no-store`.
    /// The nonce must be fresh per response (≥ 128 bits).
    pub fn form_post_html(&self, nonce: &str) -> String {
        let mut inputs = String::new();
        for (k, v) in &self.params {
            inputs.push_str(&format!(
                "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
                html_escape(k),
                html_escape(v)
            ));
        }
        format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>Submitting…</title></head>\
             <body><form method=\"post\" action=\"{action}\">{inputs}\
             <noscript><button type=\"submit\">Continue</button></noscript></form>\
             <script nonce=\"{nonce}\">document.forms[0].submit();</script></body></html>",
            action = html_escape(&self.redirect_uri),
            nonce = html_escape(nonce),
        )
    }

    /// CSP for [`AuthorizationResponse::form_post_html`].
    pub fn form_post_csp(&self, nonce: &str) -> String {
        let target = Url::parse(&self.redirect_uri)
            .ok()
            .map(|u| match u.scheme() {
                "http" | "https" => u.origin().ascii_serialization(),
                s => format!("{s}:"),
            })
            .unwrap_or_else(|| "'none'".into());
        format!(
            "default-src 'none'; script-src 'nonce-{nonce}'; form-action {target}; frame-ancestors 'none'; base-uri 'none'"
        )
    }
}

/// Why [`AuthorizationCompleter`] refused (the pages show an error page;
/// nothing is redirected because the interaction may not be trusted).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompleteError {
    /// Unknown, expired or already completed interaction, or another realm.
    #[error("the sign-in request has expired or was already used; start again from the app")]
    UnknownInteraction,
    /// The decision is inconsistent with the request (e.g. scopes that were
    /// not requested, a patient outside the user's set). The interaction is
    /// consumed and the client receives `server_error`-free denial is not
    /// possible; the page shows this message.
    #[error("invalid decision: {0}")]
    InvalidDecision(String),
    #[error("internal error: {0}")]
    Internal(String),
}

/// Finishes an interaction (implemented by `helios-idp-server`).
#[async_trait]
pub trait AuthorizationCompleter: Send + Sync + 'static {
    /// Issues the authorization code for an approved interaction.
    async fn complete(
        &self,
        realm: &str,
        interaction: &str,
        decision: Decision,
    ) -> Result<AuthorizationResponse, CompleteError>;

    /// Ends an interaction with an error for the client, e.g.
    /// `access_denied` (user declined), `login_required` /
    /// `consent_required` / `interaction_required` (for `prompt=none`).
    async fn deny(
        &self,
        realm: &str,
        interaction: &str,
        error: OAuthError,
    ) -> Result<AuthorizationResponse, CompleteError>;
}

/// The errors `deny` accepts (anything else becomes `access_denied`).
pub fn deny_code_allowed(code: OAuthErrorCode) -> bool {
    matches!(
        code,
        OAuthErrorCode::AccessDenied
            | OAuthErrorCode::LoginRequired
            | OAuthErrorCode::ConsentRequired
            | OAuthErrorCode::InteractionRequired
            | OAuthErrorCode::AccountSelectionRequired
            | OAuthErrorCode::TemporarilyUnavailable
            | OAuthErrorCode::ServerError
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_keeps_registered_query() {
        let r = AuthorizationResponse {
            redirect_uri: "https://app.example/cb?x=1".into(),
            mode: ResponseMode::Query,
            params: vec![
                ("code".into(), "hmac_a b".into()),
                ("state".into(), "s&t".into()),
                ("iss".into(), "https://h/idp/r".into()),
            ],
        };
        let u = Url::parse(&r.redirect_url()).unwrap();
        let q: Vec<(String, String)> = u.query_pairs().into_owned().collect();
        assert_eq!(q[0], ("x".into(), "1".into()));
        assert_eq!(q[1].1, "hmac_a b");
        assert_eq!(q[2].1, "s&t");
        assert_eq!(q[3].1, "https://h/idp/r");
    }

    #[test]
    fn error_response_echoes_state_and_iss() {
        let e = OAuthError::new(OAuthErrorCode::AccessDenied, "user declined");
        let r = AuthorizationResponse::error(
            "https://a/cb",
            ResponseMode::Query,
            &e,
            Some("xyz"),
            "https://h/idp/r",
        );
        assert_eq!(r.param("error"), Some("access_denied"));
        assert_eq!(r.param("state"), Some("xyz"));
        assert_eq!(r.param("iss"), Some("https://h/idp/r"));
        assert!(deny_code_allowed(OAuthErrorCode::LoginRequired));
        assert!(!deny_code_allowed(OAuthErrorCode::InvalidClient));
    }

    #[test]
    fn form_post_escapes() {
        let r = AuthorizationResponse {
            redirect_uri: "https://app.example/cb".into(),
            mode: ResponseMode::FormPost,
            params: vec![("state".into(), "\"><script>x</script>".into())],
        };
        let h = r.form_post_html("n0nce");
        assert!(!h.contains("<script>x"));
        assert!(h.contains("&quot;&gt;&lt;script&gt;"));
        assert!(h.contains("nonce=\"n0nce\""));
        assert!(
            r.form_post_csp("n0nce")
                .contains("form-action https://app.example")
        );
    }
}
