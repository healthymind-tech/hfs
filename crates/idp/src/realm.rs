//! Realms: independent authorization servers hosted by one HFS, each at
//! `{public_base}/idp/{name}` with its own issuer, keys, clients, users and
//! policy (design §5).

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::IdpError;
use crate::keys::{RotationPolicy, RsaBits, SigningAlg};

/// First path segment under which every realm is served (design D-2). Not
/// `/auth` — the reference deployment routes that to Keycloak.
pub const IDP_PATH_PREFIX: &str = "idp";

/// Realm names that are not allowed (they would collide with realm-level
/// routes or be confusing in URLs).
pub const RESERVED_REALM_NAMES: &[&str] = &["well-known", "admin", "static", "assets", "api"];

/// A validated realm name: same grammar as endpoint names
/// (`^[a-z][a-z0-9-]{0,62}$`, not ending in `-`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RealmName(String);

impl RealmName {
    pub fn parse(s: &str) -> Result<Self, IdpError> {
        helios_endpoints::name::check_segment_grammar(s)
            .map_err(|e| IdpError::invalid("name", e.to_string()))?;
        if RESERVED_REALM_NAMES.contains(&s) {
            return Err(IdpError::invalid("name", format!("'{s}' is reserved")));
        }
        Ok(Self(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RealmName {
    type Error = IdpError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        RealmName::parse(&s)
    }
}

impl From<RealmName> for String {
    fn from(n: RealmName) -> String {
        n.0
    }
}

/// Where the realm's end users come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum UserSource {
    /// Accounts stored in the realm (`hfs_idp_users`, argon2id).
    Local,
    /// The console's accounts (local console users and/or the console's
    /// OIDC IdP) — for practitioner/staff realms; no second password.
    Console,
    /// Upstream OIDC identity provider (brokered login, phase 2; UDAP
    /// Tiered OAuth uses the same mechanism, phase 3).
    UpstreamOidc {
        issuer: String,
        client_id: String,
        /// Sealed under `idp-realm:{id}:upstream.clientSecret`.
        client_secret: Option<helios_endpoints::SealedSecret>,
        /// Claim mapped to the local subject link.
        subject_claim: String,
    },
}

/// Token lifetimes (seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TokenLifetimes {
    /// Authorization code (RFC 6749 §4.1.2 recommends ≤ 10 min).
    pub authorization_code: u32,
    /// Access tokens for user launches (SMART: SHOULD be ≤ 1 h).
    pub access_token: u32,
    /// Access tokens from client_credentials (SMART Backend: SHOULD ≤ 300).
    pub backend_access_token: u32,
    pub id_token: u32,
    /// Absolute lifetime of an `offline_access` refresh token family.
    pub refresh_token_absolute: u32,
    /// Idle timeout of a refresh token family.
    pub refresh_token_idle: u32,
    /// Browser SSO session at the realm.
    pub sso_session_idle: u32,
    pub sso_session_max: u32,
    /// EHR launch handle (`launch` parameter) validity.
    pub launch_handle: u32,
}

impl Default for TokenLifetimes {
    fn default() -> Self {
        Self {
            authorization_code: 60,
            access_token: 3600,
            backend_access_token: 300,
            id_token: 3600,
            refresh_token_absolute: 90 * 86400,
            refresh_token_idle: 30 * 86400,
            sso_session_idle: 30 * 60,
            sso_session_max: 10 * 3600,
            launch_handle: 300,
        }
    }
}

/// Protocol features a realm turns on (each maps to discovery metadata and
/// SMART capabilities; see `metadata`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RealmFeatures {
    pub launch_ehr: bool,
    pub launch_standalone: bool,
    pub client_public: bool,
    pub client_confidential_symmetric: bool,
    pub client_confidential_asymmetric: bool,
    pub backend_services: bool,
    pub openid_connect: bool,
    pub offline_access: bool,
    pub online_access: bool,
    pub permission_patient: bool,
    pub permission_user: bool,
    pub permission_v1: bool,
    pub context_banner: bool,
    pub context_style: bool,
    pub introspection: bool,
    pub revocation: bool,
    /// RFC 7591/7592 open or software-statement registration (phase 2).
    pub dynamic_registration: DcrPolicy,
    /// RFC 9126 (phase 2).
    pub par: bool,
    /// RFC 9449 (phase 2).
    pub dpop: bool,
    /// RFC 8628 (phase 2).
    pub device_authorization: bool,
    /// RFC 8693 (phase 2).
    pub token_exchange: bool,
    /// UDAP (HL7 FAST Security IG), phase 3.
    pub udap: bool,
}

impl Default for RealmFeatures {
    fn default() -> Self {
        Self {
            launch_ehr: true,
            launch_standalone: true,
            client_public: true,
            client_confidential_symmetric: true,
            client_confidential_asymmetric: true,
            backend_services: true,
            openid_connect: true,
            offline_access: true,
            online_access: false,
            permission_patient: true,
            permission_user: true,
            permission_v1: true,
            context_banner: true,
            context_style: false,
            introspection: true,
            revocation: true,
            dynamic_registration: DcrPolicy::Off,
            par: false,
            dpop: false,
            device_authorization: false,
            token_exchange: false,
            udap: false,
        }
    }
}

/// Dynamic client registration policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DcrPolicy {
    #[default]
    Off,
    /// Requires an initial access token minted in the console (RFC 7591 §3).
    InitialAccessToken,
    /// Open registration of *public* patient-facing apps only, rate limited,
    /// patient scopes only, consent always shown.
    OpenPublic,
    /// Signed software statements from trusted issuers (RFC 7591 §2.3;
    /// UDAP trust communities in phase 3).
    SoftwareStatement,
}

/// Login-screen and account security policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SecurityPolicy {
    /// Failed sign-ins before a temporary lock (per account and per IP).
    pub lockout_threshold: u32,
    pub lockout_secs: u32,
    pub password_min_length: u32,
    /// TOTP (phase 2) / WebAuthn (phase 3) required for these roles.
    pub mfa: MfaPolicy,
}

impl Default for SecurityPolicy {
    fn default() -> Self {
        Self {
            lockout_threshold: 5,
            lockout_secs: 900,
            password_min_length: 12,
            mfa: MfaPolicy::Optional,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MfaPolicy {
    Off,
    #[default]
    Optional,
    Required,
}

/// A realm's full configuration (`hfs_idp_realms.spec`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RealmSpec {
    pub name: RealmName,
    pub display_name: String,
    pub enabled: bool,
    pub user_source: UserSource,
    /// Algorithms with an active key; the first signs access tokens.
    pub signing_algs: Vec<SigningAlg>,
    #[serde(default)]
    pub rsa_bits: RsaBits,
    #[serde(default)]
    pub rotation: RotationPolicy,
    #[serde(default)]
    pub lifetimes: TokenLifetimes,
    #[serde(default)]
    pub features: RealmFeatures,
    #[serde(default)]
    pub security: SecurityPolicy,
    /// FHIR endpoints whose tokens this realm issues (endpoint names). Each
    /// becomes an allowed `aud` value (its public FHIR base URL).
    #[serde(default)]
    pub endpoints: Vec<String>,
    /// Login page branding (logo URL, colors), applied to the hosted pages.
    #[serde(default)]
    pub branding: serde_json::Value,
}

impl RealmSpec {
    /// A spec with the defaults the creation wizard starts from.
    pub fn new(name: RealmName, display_name: impl Into<String>) -> Self {
        Self {
            name,
            display_name: display_name.into(),
            enabled: true,
            user_source: UserSource::Local,
            signing_algs: vec![SigningAlg::RS256, SigningAlg::ES256],
            rsa_bits: RsaBits::default(),
            rotation: RotationPolicy::default(),
            lifetimes: TokenLifetimes::default(),
            features: RealmFeatures::default(),
            security: SecurityPolicy::default(),
            endpoints: Vec::new(),
            branding: serde_json::Value::Null,
        }
    }

    /// The issuer: `{public_base}/idp/{name}` with no trailing slash
    /// (OIDC Discovery §4.3 / RFC 8414 §3.3 require an exact match).
    pub fn issuer(&self, public_base: &Url) -> String {
        let base = public_base.as_str().trim_end_matches('/');
        format!("{base}/{IDP_PATH_PREFIX}/{}", self.name.as_str())
    }

    pub fn validate(&self) -> Result<(), IdpError> {
        if self.signing_algs.is_empty() {
            return Err(IdpError::invalid("signingAlgs", "at least one algorithm"));
        }
        if self.features.openid_connect && !self.signing_algs.contains(&SigningAlg::RS256) {
            // OIDC Core §3.1.3.7 / Discovery §3: RS256 is the default
            // id_token algorithm and MUST be supported by OPs.
            return Err(IdpError::invalid(
                "signingAlgs",
                "OpenID Connect requires RS256 among the signing algorithms",
            ));
        }
        let l = &self.lifetimes;
        if l.authorization_code == 0 || l.authorization_code > 600 {
            return Err(IdpError::invalid(
                "lifetimes.authorizationCode",
                "1..600 seconds",
            ));
        }
        if l.access_token == 0 || l.access_token > 3600 {
            return Err(IdpError::invalid(
                "lifetimes.accessToken",
                "1..3600 seconds (SMART: ≤ 1 h)",
            ));
        }
        if l.backend_access_token == 0 || l.backend_access_token > 3600 {
            return Err(IdpError::invalid(
                "lifetimes.backendAccessToken",
                "1..3600 seconds",
            ));
        }
        if l.refresh_token_idle > l.refresh_token_absolute {
            return Err(IdpError::invalid(
                "lifetimes.refreshTokenIdle",
                "must not exceed the absolute lifetime",
            ));
        }
        if self.rotation.retain_hours as u64 * 3600 < l.access_token.max(l.id_token) as u64 {
            return Err(IdpError::invalid(
                "rotation.retainHours",
                "must cover the longest access/id token lifetime",
            ));
        }
        if self.features.backend_services && !self.features.client_confidential_asymmetric {
            return Err(IdpError::invalid(
                "features.backendServices",
                "needs client-confidential-asymmetric",
            ));
        }
        if !self.features.permission_patient
            && !self.features.permission_user
            && !self.features.backend_services
        {
            return Err(IdpError::invalid(
                "features",
                "enable patient, user or system access",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(RealmName::parse("clinic-a").is_ok());
        assert!(RealmName::parse("Clinic").is_err());
        assert!(RealmName::parse("well-known").is_err());
        assert!(RealmName::parse("a-").is_err());
        assert!(serde_json::from_str::<RealmName>("\"x_y\"").is_err());
    }

    #[test]
    fn issuer_shape() {
        let r = RealmSpec::new(RealmName::parse("patients").unwrap(), "Patients");
        let base = Url::parse("https://hfs.example.org/").unwrap();
        assert_eq!(r.issuer(&base), "https://hfs.example.org/idp/patients");
        let base = Url::parse("https://example.org/hm").unwrap();
        assert_eq!(r.issuer(&base), "https://example.org/hm/idp/patients");
    }

    #[test]
    fn defaults_validate_and_rules_hold() {
        let mut r = RealmSpec::new(RealmName::parse("p").unwrap(), "P");
        r.validate().unwrap();
        r.signing_algs = vec![SigningAlg::ES256];
        assert!(r.validate().is_err(), "OIDC needs RS256");
        r.features.openid_connect = false;
        r.validate().unwrap();
        r.lifetimes.access_token = 7200;
        assert!(r.validate().is_err());
        r.lifetimes.access_token = 3600;
        r.features.client_confidential_asymmetric = false;
        assert!(r.validate().is_err());
    }

    #[test]
    fn spec_roundtrips() {
        let r = RealmSpec::new(RealmName::parse("p").unwrap(), "P");
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["userSource"]["kind"], "local");
        let back: RealmSpec = serde_json::from_value(j).unwrap();
        assert_eq!(back, r);
    }
}
