//! Token formats (design §7): JWT access tokens (RFC 9068) with SMART
//! claims, id_tokens (OIDC Core §2), opaque handles (codes, refresh tokens,
//! launch handles, device codes) stored only as SHA-256 digests, and the
//! refresh-token rotation / reuse-detection rules (RFC 9700 §4.14.2).

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::launch::LaunchContext;
use crate::util::{random_b64, sha256_b64};

/// Kinds of opaque handles. The prefix makes leaked values recognisable by
/// secret scanners and lets the server reject a handle of the wrong kind
/// before any lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandleKind {
    AuthorizationCode,
    RefreshToken,
    LaunchHandle,
    DeviceCode,
    ParRequestUri,
    RegistrationAccessToken,
    InitialAccessToken,
}

impl HandleKind {
    pub fn prefix(self) -> &'static str {
        match self {
            HandleKind::AuthorizationCode => "hmac_",
            HandleKind::RefreshToken => "hmrt_",
            HandleKind::LaunchHandle => "hmlh_",
            HandleKind::DeviceCode => "hmdc_",
            // RFC 9126 §2.2: the request_uri is a URN.
            HandleKind::ParRequestUri => "urn:ietf:params:oauth:request_uri:hmpar_",
            HandleKind::RegistrationAccessToken => "hmrat_",
            HandleKind::InitialAccessToken => "hmiat_",
        }
    }
}

/// A freshly minted opaque handle: `value` goes to the client once, `hash`
/// is what the store keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpaqueHandle {
    pub value: String,
    pub hash: String,
}

/// Mints a handle with 256 bits of entropy.
pub fn mint_handle(kind: HandleKind) -> OpaqueHandle {
    let value = format!("{}{}", kind.prefix(), random_b64(32));
    let hash = sha256_b64(value.as_bytes());
    OpaqueHandle { value, hash }
}

/// Digest of a presented handle, or `None` if it is not of `kind` or is
/// malformed (so the store is never queried with junk).
pub fn handle_hash(kind: HandleKind, presented: &str) -> Option<String> {
    let body = presented.strip_prefix(kind.prefix())?;
    if body.len() != 43
        || !body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(sha256_b64(presented.as_bytes()))
}

/// Claims of a JWT access token (RFC 9068 §2.2 plus SMART / HFS claims).
/// Header: `typ: at+jwt`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccessTokenClaims {
    pub iss: String,
    /// The user (pairwise-stable id in the realm) or, for client_credentials,
    /// the client id (RFC 9068 §2.2).
    pub sub: String,
    /// The FHIR endpoint base URL(s) (RFC 8707 / SMART `aud`).
    pub aud: Vec<String>,
    pub exp: i64,
    pub iat: i64,
    pub jti: String,
    pub client_id: String,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_time: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amr: Option<Vec<String>>,
    /// SMART `fhirUser` (absolute URL of Patient/Practitioner/…).
    #[serde(rename = "fhirUser", skip_serializing_if = "Option::is_none")]
    pub fhir_user: Option<String>,
    /// Launch context, flattened (`patient`, `encounter`, `fhirContext`, …),
    /// so the resource server can confine patient scopes without a lookup.
    #[serde(flatten)]
    pub context: LaunchContext,
    /// Sender constraint (RFC 9449 §6.1 `jkt`, RFC 8705 §3.1 `x5t#S256`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cnf: Option<serde_json::Value>,
    /// Grant id: links the token to its grant for revocation/introspection.
    pub gid: String,
}

/// Inputs for building access token claims.
#[derive(Debug, Clone)]
pub struct AccessTokenInput<'a> {
    pub issuer: &'a str,
    pub subject: &'a str,
    pub audience: Vec<String>,
    pub client_id: &'a str,
    pub scope: &'a str,
    pub ttl_secs: u32,
    pub auth_time: Option<DateTime<Utc>>,
    pub fhir_user: Option<String>,
    pub context: LaunchContext,
    pub cnf: Option<serde_json::Value>,
    pub grant_id: &'a str,
}

impl AccessTokenClaims {
    pub fn build(i: AccessTokenInput<'_>, now: DateTime<Utc>) -> Self {
        Self {
            iss: i.issuer.to_string(),
            sub: i.subject.to_string(),
            aud: i.audience,
            exp: (now + Duration::seconds(i.ttl_secs as i64)).timestamp(),
            iat: now.timestamp(),
            jti: random_b64(16),
            client_id: i.client_id.to_string(),
            scope: i.scope.to_string(),
            auth_time: i.auth_time.map(|t| t.timestamp()),
            acr: None,
            amr: None,
            fhir_user: i.fhir_user,
            context: i.context,
            cnf: i.cnf,
            gid: i.grant_id.to_string(),
        }
    }
}

/// id_token claims (OIDC Core §2, §3.1.3.6 `at_hash`; SMART `fhirUser`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IdTokenClaims {
    pub iss: String,
    pub sub: String,
    pub aud: String,
    pub exp: i64,
    pub iat: i64,
    pub auth_time: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amr: Option<Vec<String>>,
    #[serde(rename = "fhirUser", skip_serializing_if = "Option::is_none")]
    pub fhir_user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// `at_hash` (OIDC Core §3.1.3.6): left half of the hash of the ASCII access
/// token, the hash being the one of the id_token's `alg`.
pub fn at_hash(access_token: &str, alg: crate::keys::SigningAlg) -> String {
    use crate::keys::SigningAlg::*;
    let digest: Vec<u8> = match alg {
        RS256 | PS256 | ES256 => {
            ring::digest::digest(&ring::digest::SHA256, access_token.as_bytes())
                .as_ref()
                .to_vec()
        }
        RS384 | PS384 | ES384 => {
            ring::digest::digest(&ring::digest::SHA384, access_token.as_bytes())
                .as_ref()
                .to_vec()
        }
    };
    crate::util::b64(&digest[..digest.len() / 2])
}

/// A refresh token family: one per authorization grant with
/// `offline_access`/`online_access` (design §7.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshFamily {
    pub family_id: String,
    pub client_id: String,
    pub subject: String,
    pub scopes: Vec<String>,
    pub context: LaunchContext,
    /// Generation of the only refresh token currently valid.
    pub current_generation: u32,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    pub absolute_expiry: DateTime<Utc>,
    /// `online_access`: valid only while this SSO session lives.
    pub sso_session_id: Option<String>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub revoked_reason: Option<String>,
}

/// What to do with a presented refresh token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshDecision {
    /// Issue a new access token and the refresh token of `next_generation`.
    Rotate { next_generation: u32 },
    /// An already-rotated token came back: revoke the whole family
    /// (RFC 9700 §4.14.2) and audit it as a possible theft.
    ReuseDetected,
    /// The family is revoked.
    Revoked,
    /// Absolute or idle lifetime exceeded.
    Expired,
    /// `online_access` and the SSO session is gone.
    SessionEnded,
}

/// Decides a refresh (pure; the store applies it with a compare-and-set on
/// `current_generation`, so two concurrent refreshes with the same token
/// cannot both rotate — the loser sees `ReuseDetected`).
pub fn evaluate_refresh(
    family: &RefreshFamily,
    presented_generation: u32,
    idle_secs: u32,
    sso_session_alive: bool,
    now: DateTime<Utc>,
) -> RefreshDecision {
    if family.revoked_at.is_some() {
        return RefreshDecision::Revoked;
    }
    if presented_generation != family.current_generation {
        return RefreshDecision::ReuseDetected;
    }
    if now >= family.absolute_expiry
        || now >= family.last_used_at + Duration::seconds(idle_secs as i64)
    {
        return RefreshDecision::Expired;
    }
    if family.sso_session_id.is_some() && !sso_session_alive {
        return RefreshDecision::SessionEnded;
    }
    RefreshDecision::Rotate {
        next_generation: family.current_generation + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::SigningAlg;

    #[test]
    fn handles() {
        let h = mint_handle(HandleKind::RefreshToken);
        assert!(h.value.starts_with("hmrt_"));
        assert_eq!(
            handle_hash(HandleKind::RefreshToken, &h.value).as_deref(),
            Some(h.hash.as_str())
        );
        assert_eq!(handle_hash(HandleKind::AuthorizationCode, &h.value), None);
        assert_eq!(handle_hash(HandleKind::RefreshToken, "hmrt_short"), None);
        assert_ne!(mint_handle(HandleKind::RefreshToken).value, h.value);
        let p = mint_handle(HandleKind::ParRequestUri);
        assert!(p.value.starts_with("urn:ietf:params:oauth:request_uri:"));
    }

    #[test]
    fn at_hash_oidc_example() {
        // OIDC Core Appendix A.3 (RS256 example values).
        assert_eq!(
            at_hash(
                "jHkWEdUXMU1BwAsC4vtUsZwnNvTIxEl0z9K3vx5KF0Y",
                SigningAlg::RS256
            ),
            "77QmUPtjPfzWtF2AnpK9RQ"
        );
    }

    #[test]
    fn access_token_claims_shape() {
        let now = Utc::now();
        let c = AccessTokenClaims::build(
            AccessTokenInput {
                issuer: "https://h/idp/p",
                subject: "u1",
                audience: vec!["https://h/fhir".into()],
                client_id: "app",
                scope: "patient/*.rs",
                ttl_secs: 300,
                auth_time: Some(now),
                fhir_user: Some("https://h/fhir/Patient/1".into()),
                context: LaunchContext {
                    patient: Some("1".into()),
                    ..Default::default()
                },
                cnf: None,
                grant_id: "g1",
            },
            now,
        );
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["patient"], "1");
        assert_eq!(v["fhirUser"], "https://h/fhir/Patient/1");
        assert_eq!(v["exp"].as_i64().unwrap() - v["iat"].as_i64().unwrap(), 300);
        assert!(v.get("cnf").is_none());
        let back: AccessTokenClaims = serde_json::from_value(v).unwrap();
        assert_eq!(back.context.patient.as_deref(), Some("1"));
    }

    fn family(now: DateTime<Utc>) -> RefreshFamily {
        RefreshFamily {
            family_id: "f".into(),
            client_id: "app".into(),
            subject: "u".into(),
            scopes: vec!["offline_access".into()],
            context: LaunchContext::default(),
            current_generation: 3,
            created_at: now - Duration::days(1),
            last_used_at: now - Duration::hours(1),
            absolute_expiry: now + Duration::days(10),
            sso_session_id: None,
            revoked_at: None,
            revoked_reason: None,
        }
    }

    #[test]
    fn refresh_rotation_rules() {
        let now = Utc::now();
        let f = family(now);
        assert_eq!(
            evaluate_refresh(&f, 3, 86400, true, now),
            RefreshDecision::Rotate { next_generation: 4 }
        );
        assert_eq!(
            evaluate_refresh(&f, 2, 86400, true, now),
            RefreshDecision::ReuseDetected
        );
        assert_eq!(
            evaluate_refresh(&f, 3, 60, true, now),
            RefreshDecision::Expired
        );
        let mut online = f.clone();
        online.sso_session_id = Some("s".into());
        assert_eq!(
            evaluate_refresh(&online, 3, 86400, false, now),
            RefreshDecision::SessionEnded
        );
        let mut r = f;
        r.revoked_at = Some(now);
        assert_eq!(
            evaluate_refresh(&r, 3, 86400, true, now),
            RefreshDecision::Revoked
        );
    }
}
