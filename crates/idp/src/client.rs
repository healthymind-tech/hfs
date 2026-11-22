//! OAuth clients of a realm: authentication method, grant types, redirect
//! URIs, SMART launch types and the scope policy.

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::IdpError;
use crate::keys::SigningAlg;
use crate::scope::ScopePolicy;

/// How a client authenticates at the token, introspection, revocation and
/// PAR endpoints (RFC 7591 §2 `token_endpoint_auth_method`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum ClientAuth {
    /// Public client (`none`): native or browser app; PKCE is its only proof.
    /// SMART `client-public`.
    None,
    /// `client_secret_basic` / `client_secret_post`. SMART
    /// `client-confidential-symmetric`. The secret is stored as an argon2id
    /// hash (never recoverable, shown once at creation).
    ClientSecret { secret_hash: String },
    /// `private_key_jwt` (RFC 7523 §2.2, SMART `client-confidential-asymmetric`,
    /// Backend Services). Keys by URL (preferred, rotatable) or inline.
    PrivateKeyJwt {
        jwks_uri: Option<String>,
        jwks: Option<serde_json::Value>,
        /// Accepted assertion algorithms; empty = `RS384 ES384` (SMART
        /// requires at least one of the two) plus `RS256 ES256`.
        algs: Vec<SigningAlg>,
    },
    /// `tls_client_auth` / `self_signed_tls_client_auth` (RFC 8705), phase 3.
    TlsClientAuth {
        subject_dn: Option<String>,
        san_uri: Option<String>,
    },
}

impl ClientAuth {
    pub fn method_name(&self) -> &'static str {
        match self {
            ClientAuth::None => "none",
            ClientAuth::ClientSecret { .. } => "client_secret_basic",
            ClientAuth::PrivateKeyJwt { .. } => "private_key_jwt",
            ClientAuth::TlsClientAuth { .. } => "tls_client_auth",
        }
    }

    pub fn is_confidential(&self) -> bool {
        !matches!(self, ClientAuth::None)
    }
}

/// Grant types (RFC 7591 §2 `grant_types`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GrantType {
    #[serde(rename = "authorization_code")]
    AuthorizationCode,
    #[serde(rename = "refresh_token")]
    RefreshToken,
    #[serde(rename = "client_credentials")]
    ClientCredentials,
    /// RFC 8628, phase 2.
    #[serde(rename = "urn:ietf:params:oauth:grant-type:device_code")]
    DeviceCode,
    /// RFC 8693, phase 2.
    #[serde(rename = "urn:ietf:params:oauth:grant-type:token-exchange")]
    TokenExchange,
    /// RFC 7523 §2.1 JWT bearer authorization grant, phase 3.
    #[serde(rename = "urn:ietf:params:oauth:grant-type:jwt-bearer")]
    JwtBearer,
}

/// SMART launch types a client may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LaunchType {
    /// SMART `launch-ehr` (launch parameter from the console's EHR launcher).
    Ehr,
    /// SMART `launch-standalone`.
    Standalone,
    /// Backend Services (client_credentials, no user).
    Backend,
}

/// A registered client (the `hfs_idp_clients.spec` JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientSpec {
    pub client_id: String,
    pub name: String,
    pub auth: ClientAuth,
    pub grant_types: Vec<GrantType>,
    pub redirect_uris: Vec<String>,
    #[serde(default)]
    pub post_logout_redirect_uris: Vec<String>,
    #[serde(default)]
    pub launch_types: Vec<LaunchType>,
    pub scopes: ScopePolicy,
    /// Skip the consent screen (first-party / trusted clients only; set by
    /// an admin, never by dynamic registration).
    #[serde(default)]
    pub trusted: bool,
    /// Require DPoP-bound tokens (RFC 9449), phase 2.
    #[serde(default)]
    pub require_dpop: bool,
    /// Require PAR (RFC 9126 §6 `require_pushed_authorization_requests`), phase 2.
    #[serde(default)]
    pub require_par: bool,
    /// OIDC back-channel logout URI, phase 2.
    #[serde(default)]
    pub backchannel_logout_uri: Option<String>,
    /// Access token lifetime override (seconds; capped by the realm).
    #[serde(default)]
    pub access_token_ttl_secs: Option<u32>,
    #[serde(default)]
    pub disabled: bool,
}

impl ClientSpec {
    /// Static consistency checks (run on create/update and on DCR).
    pub fn validate(&self) -> Result<(), IdpError> {
        if self.client_id.is_empty()
            || self.client_id.len() > 128
            || !self.client_id.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(IdpError::invalid(
                "clientId",
                "1-128 visible ASCII characters",
            ));
        }
        let uses_code = self.grant_types.contains(&GrantType::AuthorizationCode);
        if uses_code && self.redirect_uris.is_empty() {
            return Err(IdpError::invalid(
                "redirectUris",
                "required for authorization_code",
            ));
        }
        for u in &self.redirect_uris {
            validate_redirect_uri(u)?;
        }
        if self.grant_types.contains(&GrantType::ClientCredentials) && !self.auth.is_confidential()
        {
            return Err(IdpError::invalid(
                "grantTypes",
                "client_credentials needs a confidential client",
            ));
        }
        if self.launch_types.contains(&LaunchType::Backend)
            && !matches!(self.auth, ClientAuth::PrivateKeyJwt { .. })
        {
            return Err(IdpError::invalid(
                "auth",
                "SMART Backend Services clients SHALL use private_key_jwt",
            ));
        }
        if let ClientAuth::PrivateKeyJwt { jwks_uri, jwks, .. } = &self.auth {
            match (jwks_uri, jwks) {
                (Some(_), Some(_)) | (None, None) => {
                    return Err(IdpError::invalid("auth", "exactly one of jwksUri / jwks"));
                }
                (Some(u), None) => {
                    let url = Url::parse(u)
                        .map_err(|_| IdpError::invalid("auth.jwksUri", "not a URL"))?;
                    if url.scheme() != "https" {
                        return Err(IdpError::invalid("auth.jwksUri", "must be https"));
                    }
                }
                (None, Some(j)) => {
                    if !j.get("keys").is_some_and(|k| k.is_array()) {
                        return Err(IdpError::invalid("auth.jwks", "must be a JWK Set"));
                    }
                }
            }
        }
        Ok(())
    }

    /// Exact redirect URI matching (RFC 9700 §4.1.3, OAuth 2.1 §2.3.1), with
    /// the single exception of the port of a loopback IP redirect for
    /// native apps (RFC 8252 §7.3).
    pub fn redirect_uri_allowed(&self, presented: &str) -> bool {
        self.redirect_uris
            .iter()
            .any(|r| redirect_matches(r, presented))
    }
}

fn is_loopback_ip_host(u: &Url) -> bool {
    matches!(u.host_str(), Some("127.0.0.1") | Some("[::1]"))
}

/// Registration-time redirect URI rules: absolute, no fragment (RFC 6749
/// §3.1.2), https — except loopback http (RFC 8252 §7.3, not `localhost`,
/// §8.3) and private-use schemes with a dot (RFC 8252 §7.1) — no wildcards.
pub fn validate_redirect_uri(u: &str) -> Result<(), IdpError> {
    let url = Url::parse(u)
        .map_err(|_| IdpError::invalid("redirectUris", format!("{u:?} is not an absolute URL")))?;
    if url.fragment().is_some() {
        return Err(IdpError::invalid(
            "redirectUris",
            "must not contain a fragment",
        ));
    }
    if u.contains('*') {
        return Err(IdpError::invalid(
            "redirectUris",
            "wildcards are not allowed",
        ));
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback_ip_host(&url) => Ok(()),
        "http" => Err(IdpError::invalid(
            "redirectUris",
            "http is only allowed for 127.0.0.1 / [::1] loopback redirects",
        )),
        s if s.contains('.') => Ok(()),
        _ => Err(IdpError::invalid(
            "redirectUris",
            "custom schemes must be reverse-domain (RFC 8252 §7.1)",
        )),
    }
}

fn redirect_matches(registered: &str, presented: &str) -> bool {
    if registered == presented {
        return true;
    }
    let (Ok(r), Ok(p)) = (Url::parse(registered), Url::parse(presented)) else {
        return false;
    };
    if r.scheme() == "http" && is_loopback_ip_host(&r) && r.port().is_none() {
        // Any port, everything else identical.
        let mut p2 = p.clone();
        if p2.set_port(None).is_err() {
            return false;
        }
        return p2.as_str() == r.as_str() && p.scheme() == "http";
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public_client() -> ClientSpec {
        ClientSpec {
            client_id: "app".into(),
            name: "App".into(),
            auth: ClientAuth::None,
            grant_types: vec![GrantType::AuthorizationCode],
            redirect_uris: vec![
                "https://app.example.org/cb".into(),
                "http://127.0.0.1/cb".into(),
                "org.example.app:/cb".into(),
            ],
            post_logout_redirect_uris: vec![],
            launch_types: vec![LaunchType::Standalone],
            scopes: ScopePolicy::default(),
            trusted: false,
            require_dpop: false,
            require_par: false,
            backchannel_logout_uri: None,
            access_token_ttl_secs: None,
            disabled: false,
        }
    }

    #[test]
    fn exact_match_only() {
        let c = public_client();
        c.validate().unwrap();
        assert!(c.redirect_uri_allowed("https://app.example.org/cb"));
        assert!(!c.redirect_uri_allowed("https://app.example.org/cb/"));
        assert!(!c.redirect_uri_allowed("https://app.example.org/cb?x=1"));
        assert!(!c.redirect_uri_allowed("https://app.example.org.evil/cb"));
        assert!(!c.redirect_uri_allowed("https://APP.example.org/cb2"));
        assert!(c.redirect_uri_allowed("org.example.app:/cb"));
    }

    #[test]
    fn loopback_port_varies() {
        let c = public_client();
        assert!(c.redirect_uri_allowed("http://127.0.0.1:51234/cb"));
        assert!(!c.redirect_uri_allowed("http://127.0.0.1:51234/other"));
        assert!(!c.redirect_uri_allowed("https://127.0.0.1:51234/cb"));
    }

    #[test]
    fn registration_rules() {
        assert!(validate_redirect_uri("https://a.example/cb").is_ok());
        assert!(validate_redirect_uri("https://a.example/cb#x").is_err());
        assert!(validate_redirect_uri("http://a.example/cb").is_err());
        assert!(validate_redirect_uri("http://localhost/cb").is_err());
        assert!(validate_redirect_uri("myapp:/cb").is_err());
        assert!(validate_redirect_uri("https://*.example/cb").is_err());
        assert!(validate_redirect_uri("/relative").is_err());
    }

    #[test]
    fn backend_clients_need_private_key_jwt() {
        let mut c = public_client();
        c.grant_types = vec![GrantType::ClientCredentials];
        assert!(c.validate().is_err());
        c.launch_types = vec![LaunchType::Backend];
        c.auth = ClientAuth::ClientSecret {
            secret_hash: "$argon2id$...".into(),
        };
        assert!(c.validate().is_err());
        c.auth = ClientAuth::PrivateKeyJwt {
            jwks_uri: Some("https://app.example.org/jwks.json".into()),
            jwks: None,
            algs: vec![],
        };
        c.validate().unwrap();
        c.auth = ClientAuth::PrivateKeyJwt {
            jwks_uri: Some("http://app.example.org/jwks.json".into()),
            jwks: None,
            algs: vec![],
        };
        assert!(c.validate().is_err());
    }
}
