//! Errors: the OAuth/OIDC wire error codes and the crate's internal errors.

use serde::Serialize;

/// Error codes of the authorization and token endpoints (RFC 6749 §4.1.2.1,
/// §5.2; RFC 6750 §3.1; OIDC Core §3.1.2.6; RFC 8628 §3.5; RFC 9126 §2.3;
/// RFC 9449 §5, §12.2; RFC 7591 §3.2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthErrorCode {
    InvalidRequest,
    InvalidClient,
    InvalidGrant,
    UnauthorizedClient,
    UnsupportedGrantType,
    UnsupportedResponseType,
    InvalidScope,
    AccessDenied,
    ServerError,
    TemporarilyUnavailable,
    InvalidToken,
    InsufficientScope,
    // OpenID Connect Core §3.1.2.6
    InteractionRequired,
    LoginRequired,
    AccountSelectionRequired,
    ConsentRequired,
    InvalidRequestUri,
    InvalidRequestObject,
    RequestNotSupported,
    RequestUriNotSupported,
    RegistrationNotSupported,
    // RFC 8628 §3.5
    AuthorizationPending,
    SlowDown,
    ExpiredToken,
    // RFC 8707 §2
    InvalidTarget,
    // RFC 9449 §5 / §8
    InvalidDpopProof,
    UseDpopNonce,
    // RFC 7591 §3.2.2
    InvalidRedirectUri,
    InvalidClientMetadata,
    InvalidSoftwareStatement,
    UnapprovedSoftwareStatement,
}

impl OAuthErrorCode {
    /// The wire string.
    pub fn as_str(self) -> &'static str {
        use OAuthErrorCode::*;
        match self {
            InvalidRequest => "invalid_request",
            InvalidClient => "invalid_client",
            InvalidGrant => "invalid_grant",
            UnauthorizedClient => "unauthorized_client",
            UnsupportedGrantType => "unsupported_grant_type",
            UnsupportedResponseType => "unsupported_response_type",
            InvalidScope => "invalid_scope",
            AccessDenied => "access_denied",
            ServerError => "server_error",
            TemporarilyUnavailable => "temporarily_unavailable",
            InvalidToken => "invalid_token",
            InsufficientScope => "insufficient_scope",
            InteractionRequired => "interaction_required",
            LoginRequired => "login_required",
            AccountSelectionRequired => "account_selection_required",
            ConsentRequired => "consent_required",
            InvalidRequestUri => "invalid_request_uri",
            InvalidRequestObject => "invalid_request_object",
            RequestNotSupported => "request_not_supported",
            RequestUriNotSupported => "request_uri_not_supported",
            RegistrationNotSupported => "registration_not_supported",
            AuthorizationPending => "authorization_pending",
            SlowDown => "slow_down",
            ExpiredToken => "expired_token",
            InvalidTarget => "invalid_target",
            InvalidDpopProof => "invalid_dpop_proof",
            UseDpopNonce => "use_dpop_nonce",
            InvalidRedirectUri => "invalid_redirect_uri",
            InvalidClientMetadata => "invalid_client_metadata",
            InvalidSoftwareStatement => "invalid_software_statement",
            UnapprovedSoftwareStatement => "unapproved_software_statement",
        }
    }

    /// HTTP status of a token-endpoint error response (RFC 6749 §5.2:
    /// 400, except `invalid_client` which is 401 when the client tried HTTP
    /// authentication — the caller decides that case and the header).
    pub fn token_endpoint_status(self) -> u16 {
        match self {
            OAuthErrorCode::InvalidClient => 401,
            OAuthErrorCode::ServerError => 500,
            OAuthErrorCode::TemporarilyUnavailable => 503,
            _ => 400,
        }
    }
}

/// A protocol error as sent to a client (`error`, `error_description`).
///
/// `description` is shown to client developers: it must never carry secrets,
/// internal identifiers or whether a user account exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("{}: {description}", code.as_str())]
pub struct OAuthError {
    #[serde(rename = "error")]
    pub code: OAuthErrorCode,
    #[serde(rename = "error_description")]
    pub description: String,
}

impl OAuthError {
    pub fn new(code: OAuthErrorCode, description: impl Into<String>) -> Self {
        Self {
            code,
            description: description.into(),
        }
    }

    pub fn invalid_request(d: impl Into<String>) -> Self {
        Self::new(OAuthErrorCode::InvalidRequest, d)
    }

    pub fn invalid_grant(d: impl Into<String>) -> Self {
        Self::new(OAuthErrorCode::InvalidGrant, d)
    }

    pub fn invalid_scope(d: impl Into<String>) -> Self {
        Self::new(OAuthErrorCode::InvalidScope, d)
    }

    pub fn invalid_client(d: impl Into<String>) -> Self {
        Self::new(OAuthErrorCode::InvalidClient, d)
    }
}

/// Internal errors of the core (configuration, key material, storage).
#[derive(Debug, thiserror::Error)]
pub enum IdpError {
    /// A realm or client specification is invalid (field path, message).
    #[error("invalid {0}: {1}")]
    Invalid(String, String),
    /// Key generation, parsing or signing failed.
    #[error("key error: {0}")]
    Key(String),
    /// Sealing or opening a private key failed.
    #[error(transparent)]
    Secret(#[from] helios_endpoints::secret::SecretError),
    /// The store failed.
    #[error("store error: {0}")]
    Store(String),
    /// Optimistic concurrency conflict (revision mismatch).
    #[error("conflict: {0}")]
    Conflict(String),
    /// Not found.
    #[error("not found: {0}")]
    NotFound(String),
}

impl IdpError {
    pub(crate) fn invalid(field: &str, msg: impl Into<String>) -> Self {
        IdpError::Invalid(field.to_string(), msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_strings_match_serde() {
        for c in [
            OAuthErrorCode::InvalidGrant,
            OAuthErrorCode::UseDpopNonce,
            OAuthErrorCode::AuthorizationPending,
            OAuthErrorCode::InvalidSoftwareStatement,
        ] {
            assert_eq!(
                serde_json::to_value(c).unwrap(),
                serde_json::Value::String(c.as_str().into())
            );
        }
    }

    #[test]
    fn error_body_shape() {
        let e = OAuthError::invalid_grant("code expired");
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["error"], "invalid_grant");
        assert_eq!(v["error_description"], "code expired");
        assert_eq!(OAuthErrorCode::InvalidClient.token_endpoint_status(), 401);
    }
}
