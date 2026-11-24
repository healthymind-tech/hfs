//! The console's OpenID Connect relying party: discovery, the HTTP client
//! (pinned to SSRF-checked addresses for console-typed URLs, design D-13),
//! and role mapping from token claims.

use std::net::SocketAddr;
use std::time::Duration;

use helios_auth::{LoginConfig, OidcCodeClient, OidcTokens};
use helios_endpoints::OutboundPolicy;
use helios_endpoints::console::{ConsoleRole, OidcClientSettings, claim_values};
use sha2::{Digest, Sha256};

/// HTTP timeout of discovery and token calls.
const TIMEOUT: Duration = Duration::from_secs(10);

/// A ready OIDC client for one configuration.
#[derive(Debug, Clone)]
pub struct OidcRuntime {
    /// The code-flow client.
    pub client: OidcCodeClient,
    /// Issuer as discovered (what `iss` must equal).
    pub issuer: String,
    /// The configuration it was built from.
    pub settings: OidcClientSettings,
    /// [`fingerprint`] of `settings` (+ secret presence).
    pub fingerprint: String,
}

/// Identifies an OIDC configuration (a passed "Test sign-in" is bound to it).
pub fn fingerprint(settings: &OidcClientSettings, env_secret: bool) -> String {
    let mut h = Sha256::new();
    h.update(serde_json::to_vec(settings).unwrap_or_default());
    h.update([u8::from(env_secret)]);
    h.finalize()
        .iter()
        .take(12)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Resolves `url`'s host, checks every address against `policy`, and
/// returns the pin `(host, addr)`; `None` for an IP-literal host (already
/// checked by `check_url`).
async fn pin(policy: &OutboundPolicy, url: &str) -> Result<Option<(String, SocketAddr)>, String> {
    let u = policy.check_url(url).map_err(|e| format!("{url}: {e}"))?;
    let host = u.host_str().unwrap_or_default().to_string();
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.parse::<std::net::IpAddr>().is_ok() {
        return Ok(None);
    }
    let port = u.port_or_known_default().unwrap_or(443);
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((bare, port))
        .await
        .map_err(|e| format!("{host}: cannot resolve ({e})"))?
        .collect();
    let first = *addrs.first().ok_or_else(|| format!("{host}: no address"))?;
    for a in &addrs {
        policy
            .check_ip(bare, a.ip())
            .map_err(|e| format!("{url}: {e}"))?;
    }
    Ok(Some((bare.to_string(), first)))
}

/// An HTTP client for `urls`: unpinned (operator-trusted, from the
/// environment) when `policy` is `None`; otherwise every URL is checked and
/// the connection pinned to the checked address, redirects off.
pub async fn http_client(
    policy: Option<&OutboundPolicy>,
    urls: &[&str],
) -> Result<reqwest::Client, String> {
    let mut b = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none());
    if let Some(policy) = policy {
        for url in urls {
            if let Some((host, addr)) = pin(policy, url).await? {
                b = b.resolve(&host, addr);
            }
        }
    }
    b.build().map_err(|e| e.to_string())
}

/// Builds the runtime: discovery (through the policy when given), then a
/// client for the token endpoint.
pub async fn build(
    settings: &OidcClientSettings,
    client_secret: Option<String>,
    env_secret: bool,
    redirect_uri: &str,
    cookie_secure: bool,
    policy: Option<&OutboundPolicy>,
) -> Result<OidcRuntime, String> {
    let discovery = format!(
        "{}/.well-known/openid-configuration",
        settings.issuer.trim_end_matches('/')
    );
    let http = http_client(policy, &[&discovery]).await?;
    let endpoints = helios_auth::discover_with(
        &http,
        &settings.issuer,
        settings.authorization_endpoint.clone(),
        settings.token_endpoint.clone(),
        settings.end_session_endpoint.clone(),
    )
    .await
    .map_err(|e| e.to_string())?;
    if policy.is_some() {
        // Browser redirects need only syntax checks; the token endpoint is
        // called by the server and is pinned below.
        for u in [&endpoints.authorization_endpoint]
            .into_iter()
            .chain(endpoints.end_session_endpoint.as_ref())
        {
            url::Url::parse(u)
                .ok()
                .filter(|u| matches!(u.scheme(), "https" | "http"))
                .ok_or_else(|| format!("{u}: not an http(s) URL"))?;
        }
    }
    let token_http = http_client(policy, &[&endpoints.token_endpoint]).await?;
    let config = LoginConfig {
        client_id: settings.client_id.clone(),
        client_secret,
        redirect_uri: redirect_uri.to_string(),
        scopes: settings.scopes.clone(),
        authorization_endpoint: endpoints.authorization_endpoint,
        token_endpoint: endpoints.token_endpoint,
        end_session_endpoint: endpoints.end_session_endpoint,
        cookie_secure,
    };
    Ok(OidcRuntime {
        client: OidcCodeClient::new(config, token_http),
        issuer: endpoints.issuer,
        settings: settings.clone(),
        fingerprint: fingerprint(settings, env_secret),
    })
}

/// The claim values of `role_claim` in the ID token, then the access token
/// (Keycloak puts `realm_access.roles` only in the access token by default).
pub fn role_values(tokens: &OidcTokens, role_claim: &str) -> Vec<String> {
    let mut v = claim_values(&tokens.id_claims, role_claim);
    for x in claim_values(&tokens.access_claims, role_claim) {
        if !v.contains(&x) {
            v.push(x);
        }
    }
    v
}

/// Fingerprint of what decides a role (claim path, mapping, default role):
/// a session mapped under another fingerprint is re-mapped.
pub fn mapping_fingerprint(settings: &OidcClientSettings) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!(
        "{}|{:?}|{:?}",
        settings.role_claim, settings.role_mapping, settings.default_role
    )
    .hash(&mut h);
    format!("{:016x}", h.finish())
}

/// The role these tokens map to under `settings` (`None` = no access).
pub fn map_role(tokens: &OidcTokens, settings: &OidcClientSettings) -> Option<ConsoleRole> {
    settings.role_mapping.resolve(
        &role_values(tokens, &settings.role_claim),
        settings.default_role,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_endpoints::console::RoleMapping;

    fn settings() -> OidcClientSettings {
        OidcClientSettings {
            issuer: "https://idp.example/realms/r".into(),
            client_id: "hfs-web".into(),
            client_secret: None,
            scopes: "openid".into(),
            authorization_endpoint: None,
            token_endpoint: None,
            end_session_endpoint: None,
            role_claim: "realm_access.roles".into(),
            role_mapping: RoleMapping {
                admin: vec!["hfs-admin".into()],
                editor: vec!["hfs-editor".into()],
                viewer: vec![],
            },
            default_role: None,
        }
    }

    fn tokens(id: serde_json::Value, access: serde_json::Value) -> OidcTokens {
        OidcTokens {
            access_token: String::new(),
            refresh_token: None,
            id_token: None,
            expires_in: None,
            id_claims: id,
            access_claims: access,
        }
    }

    #[test]
    fn roles_from_id_then_access_token() {
        let s = settings();
        let t = tokens(
            serde_json::json!({"sub": "u"}),
            serde_json::json!({"realm_access": {"roles": ["hfs-editor"]}}),
        );
        assert_eq!(map_role(&t, &s), Some(ConsoleRole::Editor));
        let t = tokens(
            serde_json::json!({"realm_access": {"roles": ["hfs-admin"]}}),
            serde_json::json!({"realm_access": {"roles": ["hfs-editor"]}}),
        );
        assert_eq!(
            role_values(&t, &s.role_claim),
            vec!["hfs-admin", "hfs-editor"]
        );
        assert_eq!(map_role(&t, &s), Some(ConsoleRole::Admin));
        let none = tokens(serde_json::json!({}), serde_json::Value::Null);
        assert_eq!(map_role(&none, &s), None);
        let mut d = s.clone();
        d.default_role = Some(ConsoleRole::Viewer);
        assert_eq!(map_role(&none, &d), Some(ConsoleRole::Viewer));
    }

    #[test]
    fn fingerprint_tracks_configuration() {
        let a = fingerprint(&settings(), false);
        assert_eq!(a, fingerprint(&settings(), false));
        assert_ne!(a, fingerprint(&settings(), true));
        let mut s = settings();
        s.client_id = "other".into();
        assert_ne!(a, fingerprint(&s, false));
    }

    #[tokio::test]
    async fn policy_refuses_internal_urls() {
        let strict = OutboundPolicy::strict();
        assert!(
            http_client(Some(&strict), &["http://127.0.0.1:8080/x"])
                .await
                .is_err()
        );
        assert!(
            http_client(Some(&strict), &["https://169.254.169.254/latest"])
                .await
                .is_err()
        );
        let allow = OutboundPolicy::parse("127.0.0.1/32");
        assert!(
            http_client(Some(&allow), &["http://127.0.0.1:8080/x"])
                .await
                .is_ok()
        );
        assert!(http_client(None, &["http://127.0.0.1:1/"]).await.is_ok());
    }
}
