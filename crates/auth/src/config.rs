use std::env;
use std::sync::Arc;

use crate::outbound::{OutboundAuthProvider, provider_from_token};

/// How much of the FHIR API needs a credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthMode {
    /// No authentication: every request is anonymous and allowed.
    Off,
    /// Anonymous reads, authenticated writes. A request without credentials
    /// may read and search; anything that changes data needs a valid token
    /// (or the administrator credential). A request that *does* present a
    /// token is validated and scope-checked exactly as in [`AuthMode::Full`].
    WriteProtect,
    /// Every request except discovery and health needs a valid token.
    #[default]
    Full,
}

impl AuthMode {
    /// Parses `off`, `write-protect` (also `write_protect`, `writes`) or
    /// `full` (also `on`, `true`), case-insensitively.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "off" | "false" | "none" | "disabled" => Some(Self::Off),
            "write-protect" | "writes" | "write" => Some(Self::WriteProtect),
            "full" | "on" | "true" | "required" => Some(Self::Full),
            _ => None,
        }
    }

    /// The canonical spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::WriteProtect => "write-protect",
            Self::Full => "full",
        }
    }
}

/// What a `patient/` scope may read outside the Patient compartment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NonCompartmentPolicy {
    /// Types that are not members of the Patient compartment at all
    /// (Medication, Practitioner, Organization, Location, …) may be read and
    /// searched under a `patient/` scope that names them (or `*`). They carry
    /// no patient data by definition, and patient-facing apps (US Core) need
    /// them to render medications and care teams. Writes stay refused.
    #[default]
    Allow,
    /// `patient/` scopes reach nothing outside the compartment.
    Deny,
}

/// Configuration for the authentication and authorization subsystem.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// Master switch — when false, all auth is bypassed.
    pub enabled: bool,
    /// What needs a credential when [`enabled`](Self::enabled) is true
    /// (`HFS_AUTH_MODE`). See [`AuthConfig::effective_mode`].
    pub mode: AuthMode,
    /// Allowed clock skew, in seconds, when checking `exp` and `nbf`
    /// (`HFS_AUTH_CLOCK_SKEW_SECONDS`, default 60).
    pub clock_skew_secs: u64,
    /// What `patient/` scopes may read outside the Patient compartment
    /// (`HFS_AUTH_PATIENT_NON_COMPARTMENT`, `allow` or `deny`).
    pub patient_non_compartment: NonCompartmentPolicy,
    /// `capabilities` advertised in `/.well-known/smart-configuration`
    /// (`HFS_SMART_CAPABILITIES`, comma-separated). `None` uses the default.
    pub smart_capabilities: Option<Vec<String>>,
    /// `scopes_supported` advertised in discovery (`HFS_SMART_SCOPES_SUPPORTED`).
    pub smart_scopes_supported: Option<Vec<String>>,
    /// `token_endpoint_auth_methods_supported` advertised in discovery
    /// (`HFS_SMART_TOKEN_AUTH_METHODS`).
    pub smart_token_auth_methods: Option<Vec<String>>,
    /// `grant_types_supported` advertised in discovery (`HFS_SMART_GRANT_TYPES`).
    pub smart_grant_types: Option<Vec<String>>,
    /// JWKS endpoint URL. Required when `enabled` is true.
    pub jwks_url: Option<String>,
    /// Expected JWT issuer (`iss` claim). Validated if set.
    pub expected_issuer: Option<String>,
    /// Expected JWT audience (`aud` claim). Validated if set.
    ///
    /// **Recommended for production.** Without audience validation, any valid
    /// token from the same issuer is accepted — even tokens intended for a
    /// different service. Set `HFS_AUTH_AUDIENCE` to restrict accepted tokens
    /// to those explicitly issued for this server.
    pub expected_audience: Option<String>,
    /// JWT claim name used to extract the tenant ID.
    pub tenant_claim: String,
    /// Comma-separated list of allowed JWT signing algorithms.
    pub allowed_algorithms: Vec<String>,
    /// Minimum interval (seconds) between JWKS refreshes.
    pub jwks_min_refresh_interval: u64,
    /// Longest time (seconds) a fetched JWKS is trusted before it is fetched
    /// again (`HFS_AUTH_JWKS_MAX_AGE_SECONDS`, default 300). Bounds how long a
    /// key the IdP retired keeps validating tokens.
    pub jwks_max_age_secs: u64,

    // SMART discovery endpoint fields
    /// Token endpoint URL for `/.well-known/smart-configuration`.
    pub smart_token_endpoint: Option<String>,
    /// Authorization endpoint URL.
    pub smart_authorize_endpoint: Option<String>,
    /// JWKS URL for the discovery document (may differ from `jwks_url`).
    pub smart_jwks_url: Option<String>,
    /// Introspection endpoint URL.
    pub smart_introspection_endpoint: Option<String>,
    /// Management endpoint URL.
    pub smart_management_endpoint: Option<String>,
    /// Registration endpoint URL.
    pub smart_registration_endpoint: Option<String>,
    /// Revocation endpoint URL.
    pub smart_revocation_endpoint: Option<String>,

    /// Static bearer token attached to outbound server-to-server requests
    /// (e.g., subscription notification dispatch). When set, an
    /// `Authorization: Bearer <token>` header is added to outbound calls.
    /// Subscription-supplied headers take precedence.
    pub outbound_bearer_token: Option<String>,

    // Interactive browser login for the web UI (issue #1449). Only consulted
    // when `enabled` is true; the flow is off unless `web_client_id` is set.
    /// OAuth client id registered at the IdP for the web UI's Authorization
    /// Code + PKCE login (e.g. `hfs-web`). Unset disables the browser login.
    pub web_client_id: Option<String>,
    /// Client secret for a confidential web client. A public client (PKCE
    /// only, the default) leaves this unset.
    pub web_client_secret: Option<String>,
    /// The `redirect_uri` registered at the IdP. Unset means
    /// `{HFS_BASE_URL}/ui/callback`, which the server fills in at startup.
    pub web_redirect_uri: Option<String>,
    /// Scopes requested at authorization. Defaults to `openid profile email`.
    pub web_scopes: String,
    /// Whether the session cookie carries the `Secure` attribute. Defaults to
    /// true; turn off only for plain-HTTP local development.
    pub web_cookie_secure: bool,
    /// The IdP's end-session (RP-initiated logout) endpoint. Discovered from
    /// `.well-known/openid-configuration` when unset.
    pub smart_end_session_endpoint: Option<String>,
}

impl AuthConfig {
    /// Load configuration from environment variables.
    pub fn from_env() -> Self {
        let legacy_enabled = env::var("HFS_AUTH_ENABLED")
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
            .unwrap_or(false);
        // `HFS_AUTH_MODE` wins; without it `HFS_AUTH_ENABLED=true` means full.
        let mode = match env::var("HFS_AUTH_MODE").ok().filter(|v| !v.trim().is_empty()) {
            Some(raw) => AuthMode::parse(&raw).unwrap_or_else(|| {
                tracing::error!(
                    value = %raw,
                    "HFS_AUTH_MODE is not one of off, write-protect, full; using full"
                );
                AuthMode::Full
            }),
            None if legacy_enabled => AuthMode::Full,
            None => AuthMode::Off,
        };
        Self {
            enabled: mode != AuthMode::Off,
            mode: if mode == AuthMode::Off {
                AuthMode::Full
            } else {
                mode
            },
            clock_skew_secs: env::var("HFS_AUTH_CLOCK_SKEW_SECONDS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(60),
            patient_non_compartment: match env::var("HFS_AUTH_PATIENT_NON_COMPARTMENT")
                .map(|v| v.trim().to_ascii_lowercase())
                .as_deref()
            {
                Ok("deny") => NonCompartmentPolicy::Deny,
                _ => NonCompartmentPolicy::Allow,
            },
            smart_capabilities: list_env("HFS_SMART_CAPABILITIES"),
            smart_scopes_supported: list_env("HFS_SMART_SCOPES_SUPPORTED"),
            smart_token_auth_methods: list_env("HFS_SMART_TOKEN_AUTH_METHODS"),
            smart_grant_types: list_env("HFS_SMART_GRANT_TYPES"),
            jwks_url: env::var("HFS_AUTH_JWKS_URL").ok(),
            expected_issuer: env::var("HFS_AUTH_ISSUER").ok(),
            expected_audience: env::var("HFS_AUTH_AUDIENCE").ok(),
            tenant_claim: env::var("HFS_AUTH_TENANT_CLAIM")
                .unwrap_or_else(|_| "tenant_id".to_string()),
            allowed_algorithms: env::var("HFS_AUTH_ALGORITHMS")
                .unwrap_or_else(|_| "RS256,RS384,ES256,ES384".to_string())
                .split(',')
                .map(|s| s.trim().to_string())
                .collect(),
            jwks_min_refresh_interval: env::var("HFS_AUTH_JWKS_MIN_REFRESH_INTERVAL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
            jwks_max_age_secs: env::var("HFS_AUTH_JWKS_MAX_AGE_SECONDS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(300),
            smart_token_endpoint: env::var("HFS_SMART_TOKEN_ENDPOINT").ok(),
            smart_authorize_endpoint: env::var("HFS_SMART_AUTHORIZE_ENDPOINT").ok(),
            smart_jwks_url: env::var("HFS_SMART_JWKS_URL").ok(),
            smart_introspection_endpoint: env::var("HFS_SMART_INTROSPECTION_ENDPOINT").ok(),
            smart_management_endpoint: env::var("HFS_SMART_MANAGEMENT_ENDPOINT").ok(),
            smart_registration_endpoint: env::var("HFS_SMART_REGISTRATION_ENDPOINT").ok(),
            smart_revocation_endpoint: env::var("HFS_SMART_REVOCATION_ENDPOINT").ok(),
            outbound_bearer_token: env::var("HFS_OUTBOUND_BEARER_TOKEN").ok(),
            web_client_id: env::var("HFS_UI_LOGIN_CLIENT_ID")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            web_client_secret: env::var("HFS_UI_LOGIN_CLIENT_SECRET")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            web_redirect_uri: env::var("HFS_UI_LOGIN_REDIRECT_URI")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            web_scopes: env::var("HFS_UI_LOGIN_SCOPES")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| "openid profile email".to_string()),
            web_cookie_secure: env::var("HFS_UI_LOGIN_COOKIE_SECURE")
                .map(|v| !(v.eq_ignore_ascii_case("false") || v == "0"))
                .unwrap_or(true),
            smart_end_session_endpoint: env::var("HFS_SMART_END_SESSION_ENDPOINT").ok(),
        }
    }

    /// The mode in force: [`AuthMode::Off`] when disabled, else
    /// [`mode`](Self::mode).
    pub fn effective_mode(&self) -> AuthMode {
        if self.enabled { self.mode } else { AuthMode::Off }
    }

    /// Build an outbound auth provider from this config.
    ///
    /// Returns a [`StaticBearerOutboundAuthProvider`](crate::StaticBearerOutboundAuthProvider)
    /// when [`outbound_bearer_token`](Self::outbound_bearer_token) is set,
    /// otherwise a [`NoOpOutboundAuthProvider`](crate::NoOpOutboundAuthProvider).
    pub fn outbound_provider(&self) -> Arc<dyn OutboundAuthProvider> {
        provider_from_token(self.outbound_bearer_token.as_deref())
    }

    /// Validates that an enabled configuration is internally coherent,
    /// accumulating every problem rather than reporting only the first.
    ///
    /// A disabled configuration is always valid — nothing below is consulted
    /// when auth is bypassed.
    ///
    /// These invariants previously lived only in the `hfs` binary's startup
    /// path, which meant any *other* embedder of the library crates — the
    /// integration harnesses, downstream users — could build an enabled
    /// `AuthConfig` with no pinned issuer and never be told. Since the issuer is
    /// what qualifies a subject into a per-user identity (see
    /// `helios_rest::extractors::UserKey`), an unpinned issuer makes that
    /// identity depend on an unvalidated token claim. Keeping the check on the
    /// type means it holds for every caller.
    ///
    /// Audience is deliberately *not* required: an open demo deployment
    /// legitimately accepts any token from its issuer. Callers should warn.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        if !self.enabled {
            return Ok(());
        }

        let mut errors = Vec::new();
        if self.jwks_url.as_deref().unwrap_or("").trim().is_empty() {
            errors.push("HFS_AUTH_JWKS_URL is required when HFS_AUTH_ENABLED=true".to_string());
        }
        if self
            .expected_issuer
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            errors.push(
                "HFS_AUTH_ISSUER is required when HFS_AUTH_ENABLED=true (it pins the `iss` \
                 claim, which qualifies every per-user identity)"
                    .to_string(),
            );
        }
        if self.allowed_algorithms.is_empty() {
            errors.push("HFS_AUTH_ALGORITHMS must list at least one signing algorithm".to_string());
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: AuthMode::Full,
            clock_skew_secs: 60,
            patient_non_compartment: NonCompartmentPolicy::Allow,
            smart_capabilities: None,
            smart_scopes_supported: None,
            smart_token_auth_methods: None,
            smart_grant_types: None,
            jwks_url: None,
            expected_issuer: None,
            expected_audience: None,
            tenant_claim: "tenant_id".to_string(),
            allowed_algorithms: vec![
                "RS256".to_string(),
                "RS384".to_string(),
                "ES256".to_string(),
                "ES384".to_string(),
            ],
            jwks_min_refresh_interval: 10,
            jwks_max_age_secs: 300,
            smart_token_endpoint: None,
            smart_authorize_endpoint: None,
            smart_jwks_url: None,
            smart_introspection_endpoint: None,
            smart_management_endpoint: None,
            smart_registration_endpoint: None,
            smart_revocation_endpoint: None,
            outbound_bearer_token: None,
            web_client_id: None,
            web_client_secret: None,
            web_redirect_uri: None,
            web_scopes: "openid profile email".to_string(),
            web_cookie_secure: true,
            smart_end_session_endpoint: None,
        }
    }
}

/// A comma- or space-separated list from the environment; `None` when unset
/// or blank.
fn list_env(name: &str) -> Option<Vec<String>> {
    let raw = env::var(name).ok()?;
    let items: Vec<String> = raw
        .split([',', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    (!items.is_empty()).then_some(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    const AUTH_ENV_KEYS: &[&str] = &[
        "HFS_AUTH_ENABLED",
        "HFS_AUTH_MODE",
        "HFS_AUTH_CLOCK_SKEW_SECONDS",
        "HFS_AUTH_PATIENT_NON_COMPARTMENT",
        "HFS_SMART_CAPABILITIES",
        "HFS_AUTH_JWKS_URL",
        "HFS_AUTH_ISSUER",
        "HFS_AUTH_AUDIENCE",
        "HFS_AUTH_TENANT_CLAIM",
        "HFS_AUTH_ALGORITHMS",
        "HFS_AUTH_JWKS_MIN_REFRESH_INTERVAL",
        "HFS_SMART_TOKEN_ENDPOINT",
        "HFS_SMART_AUTHORIZE_ENDPOINT",
        "HFS_SMART_JWKS_URL",
        "HFS_SMART_INTROSPECTION_ENDPOINT",
        "HFS_SMART_MANAGEMENT_ENDPOINT",
        "HFS_SMART_REGISTRATION_ENDPOINT",
        "HFS_SMART_REVOCATION_ENDPOINT",
        "HFS_OUTBOUND_BEARER_TOKEN",
        "HFS_SMART_SCOPES_SUPPORTED",
        "HFS_SMART_TOKEN_AUTH_METHODS",
        "HFS_SMART_GRANT_TYPES",
    ];

    fn clear_auth_env() {
        for key in AUTH_ENV_KEYS {
            unsafe { env::remove_var(key) };
        }
    }

    #[test]
    fn test_from_env_defaults() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        clear_auth_env();

        let config = AuthConfig::from_env();
        assert!(!config.enabled);
        assert_eq!(config.tenant_claim, "tenant_id");
        assert_eq!(config.jwks_min_refresh_interval, 10);
        assert_eq!(
            config.allowed_algorithms,
            vec!["RS256", "RS384", "ES256", "ES384"]
        );
    }

    #[test]
    fn mode_defaults_to_off_and_legacy_enabled_means_full() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        clear_auth_env();
        assert_eq!(AuthConfig::from_env().effective_mode(), AuthMode::Off);
        unsafe { env::set_var("HFS_AUTH_ENABLED", "true") };
        let config = AuthConfig::from_env();
        assert!(config.enabled);
        assert_eq!(config.effective_mode(), AuthMode::Full);
        clear_auth_env();
    }

    #[test]
    fn mode_write_protect_enables_auth() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        clear_auth_env();
        unsafe { env::set_var("HFS_AUTH_MODE", "write-protect") };
        let config = AuthConfig::from_env();
        assert!(config.enabled);
        assert_eq!(config.effective_mode(), AuthMode::WriteProtect);
        // An explicit mode wins over the legacy switch.
        unsafe { env::set_var("HFS_AUTH_ENABLED", "true") };
        unsafe { env::set_var("HFS_AUTH_MODE", "off") };
        let config = AuthConfig::from_env();
        assert!(!config.enabled);
        assert_eq!(config.effective_mode(), AuthMode::Off);
        clear_auth_env();
    }

    #[test]
    fn clock_skew_and_lists_parse() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        clear_auth_env();
        assert_eq!(AuthConfig::from_env().clock_skew_secs, 60);
        unsafe { env::set_var("HFS_AUTH_CLOCK_SKEW_SECONDS", "15") };
        unsafe { env::set_var("HFS_SMART_CAPABILITIES", "launch-standalone, permission-v2") };
        let config = AuthConfig::from_env();
        assert_eq!(config.clock_skew_secs, 15);
        assert_eq!(
            config.smart_capabilities,
            Some(vec!["launch-standalone".to_string(), "permission-v2".to_string()])
        );
        clear_auth_env();
    }

    #[test]
    fn auth_mode_parses_spellings() {
        assert_eq!(AuthMode::parse("WRITE_PROTECT"), Some(AuthMode::WriteProtect));
        assert_eq!(AuthMode::parse("full"), Some(AuthMode::Full));
        assert_eq!(AuthMode::parse("off"), Some(AuthMode::Off));
        assert_eq!(AuthMode::parse("maybe"), None);
    }

    #[test]
    fn test_default_config() {
        let config = AuthConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.tenant_claim, "tenant_id");
        assert_eq!(config.jwks_min_refresh_interval, 10);
        assert_eq!(config.allowed_algorithms.len(), 4);
    }

    fn enabled_config() -> AuthConfig {
        AuthConfig {
            enabled: true,
            jwks_url: Some("https://idp.example.com/jwks".to_string()),
            expected_issuer: Some("https://idp.example.com".to_string()),
            ..AuthConfig::default()
        }
    }

    #[test]
    fn validate_accepts_a_complete_enabled_config() {
        assert!(enabled_config().validate().is_ok());
    }

    /// A disabled config never consults the other fields.
    #[test]
    fn validate_ignores_a_disabled_config() {
        let config = AuthConfig::default();
        assert!(!config.enabled);
        assert!(config.jwks_url.is_none());
        assert!(config.validate().is_ok());
    }

    /// The issuer pins `iss`, which qualifies every per-user identity — an
    /// enabled config without one must not start (#270).
    #[test]
    fn validate_requires_an_issuer_when_enabled() {
        let config = AuthConfig {
            expected_issuer: None,
            ..enabled_config()
        };
        let errors = config.validate().unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("HFS_AUTH_ISSUER")),
            "expected an issuer error, got {errors:?}"
        );
    }

    /// A blank value is as absent as a missing one.
    #[test]
    fn validate_rejects_whitespace_only_values() {
        let config = AuthConfig {
            expected_issuer: Some("   ".to_string()),
            jwks_url: Some("".to_string()),
            ..enabled_config()
        };
        let errors = config.validate().unwrap_err();
        assert_eq!(errors.len(), 2, "got {errors:?}");
    }

    /// Every problem is reported at once, not just the first.
    #[test]
    fn validate_accumulates_all_errors() {
        let config = AuthConfig {
            enabled: true,
            jwks_url: None,
            expected_issuer: None,
            allowed_algorithms: vec![],
            ..AuthConfig::default()
        };
        let errors = config.validate().unwrap_err();
        assert_eq!(errors.len(), 3, "got {errors:?}");
    }

    /// Audience stays optional by design — an open demo deployment accepts any
    /// token from its issuer. Callers warn rather than refuse.
    #[test]
    fn validate_does_not_require_an_audience() {
        let config = AuthConfig {
            expected_audience: None,
            ..enabled_config()
        };
        assert!(config.validate().is_ok());
    }
}
