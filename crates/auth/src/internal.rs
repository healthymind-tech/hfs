//! Token verification for FHIR endpoints bound to an **internal** realm of
//! the built-in authorization server (docs/design/authorization-server.md
//! §5.2, D-3, package I5).
//!
//! An endpoint whose auth section says `provider: {kind: internal, realm}`
//! does not fetch a JWKS. Its [`InternalVerifier`] asks an
//! [`InternalRealmSource`] — the IdP runtime of the same process — for the
//! realm's *public* keys and issuer, and for every token whether its grant
//! (`gid`) has been revoked. The source answers from memory, so:
//!
//! * there is no network fetch and no JWKS cache window: a key the realm
//!   retires is refused on the next request;
//! * revocation is immediate on this instance (the IdP's revoke handler
//!   updates the deny set before it answers) and within the cluster sync
//!   interval on the others (the deny set reloads on the `idp` epoch);
//! * when no source is installed (`HFS_IDP=off`) or the realm is unknown or
//!   disabled, every token is answered with [`AuthError::JwksFetchError`],
//!   which the REST layer turns into `503`: an internal endpoint fails
//!   closed, never open.
//!
//! Checks, in order: `Bearer` scheme; JWS header `typ` = `at+jwt` (RFC 9068
//! §2.1 — an id_token or a client assertion signed by the same key is not
//! an access token); `kid` names a published key of the realm and `alg`
//! equals that key's algorithm (no algorithm confusion; `none`/`HS*` never
//! match); signature; `iss` = the realm issuer; `aud` contains the
//! endpoint's audience; `exp`/`nbf` with leeway; non-empty `sub`,
//! `client_id` and `gid`; `gid` not revoked. Scope enforcement and patient
//! confinement are unchanged: they consume the [`Principal`] built here.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, RwLock};

use async_trait::async_trait;
use jsonwebtoken::jwk::Jwk;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};

use crate::error::AuthError;
use crate::principal::{LaunchContext, Principal};
use crate::provider::AuthProvider;
use crate::scope::ScopeSet;

/// A public verification key of a realm.
#[derive(Clone)]
pub struct TrustedKey {
    /// The only algorithm this key verifies.
    pub alg: Algorithm,
    /// The key.
    pub key: DecodingKey,
}

impl std::fmt::Debug for TrustedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrustedKey")
            .field("alg", &self.alg)
            .finish()
    }
}

impl TrustedKey {
    /// A key from a public JWK (`kty`, `alg` and the public members).
    /// Refuses symmetric keys, `none`, unknown algorithms, private members
    /// and a `kty` that does not fit the `alg`.
    pub fn from_public_jwk(jwk: &serde_json::Value) -> Result<(String, Self), String> {
        let obj = jwk.as_object().ok_or("a JWK is a JSON object")?;
        for private in ["d", "p", "q", "dp", "dq", "qi", "k"] {
            if obj.contains_key(private) {
                return Err(format!(
                    "JWK carries the private/symmetric member '{private}'"
                ));
            }
        }
        let kid = obj
            .get("kid")
            .and_then(|v| v.as_str())
            .filter(|k| !k.is_empty())
            .ok_or("JWK without 'kid'")?
            .to_string();
        let alg = match obj.get("alg").and_then(|v| v.as_str()) {
            Some("RS256") => Algorithm::RS256,
            Some("RS384") => Algorithm::RS384,
            Some("RS512") => Algorithm::RS512,
            Some("PS256") => Algorithm::PS256,
            Some("PS384") => Algorithm::PS384,
            Some("PS512") => Algorithm::PS512,
            Some("ES256") => Algorithm::ES256,
            Some("ES384") => Algorithm::ES384,
            other => return Err(format!("unsupported JWK alg {other:?}")),
        };
        let kty = obj.get("kty").and_then(|v| v.as_str()).unwrap_or("");
        let fits = match alg {
            Algorithm::ES256 | Algorithm::ES384 => kty == "EC",
            _ => kty == "RSA",
        };
        if !fits {
            return Err(format!("kty '{kty}' does not fit alg {alg:?}"));
        }
        let parsed: Jwk = serde_json::from_value(jwk.clone()).map_err(|e| e.to_string())?;
        let key = DecodingKey::from_jwk(&parsed).map_err(|e| e.to_string())?;
        Ok((kid, Self { alg, key }))
    }
}

/// What a resource server needs to know about one realm, as of now.
#[derive(Debug, Clone, Default)]
pub struct RealmTrust {
    /// Exact `iss` (`{public base}/idp/{realm}`).
    pub issuer: String,
    /// Published keys (`next`, `active`, `retiring`) by `kid`.
    pub keys: HashMap<String, TrustedKey>,
    /// Endpoints bound to the realm → the `aud` their tokens carry (the
    /// endpoint's public FHIR base URL).
    pub audiences: HashMap<String, String>,
    /// The SMART configuration each bound endpoint advertises
    /// (`/.well-known/smart-configuration`), generated from the realm.
    pub smart_configuration: HashMap<String, serde_json::Value>,
}

/// The in-process view of the built-in authorization server's realms.
///
/// Implemented by the IdP runtime (an adapter over its snapshot) and by
/// [`MemoryRealmSource`] (tests, single node).
pub trait InternalRealmSource: Send + Sync + 'static {
    /// The realm's trust data; `None` when the realm does not exist, is
    /// disabled or the IdP is off.
    fn realm(&self, realm: &str) -> Option<Arc<RealmTrust>>;

    /// Whether grant `gid` of `realm` has been revoked. Called on every
    /// request, so it must answer from memory.
    fn is_grant_revoked(&self, realm: &str, gid: &str) -> bool;
}

/// A simple [`InternalRealmSource`]: realms and revoked grants in memory.
/// Revocation through [`revoke_grant`](Self::revoke_grant) is visible to
/// the next request.
#[derive(Default)]
pub struct MemoryRealmSource {
    realms: RwLock<HashMap<String, Arc<RealmTrust>>>,
    revoked: RwLock<HashSet<(String, String)>>,
}

impl MemoryRealmSource {
    /// An empty source.
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes (or replaces) a realm.
    pub fn set_realm(&self, realm: &str, trust: RealmTrust) {
        self.realms
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(realm.to_string(), Arc::new(trust));
    }

    /// Withdraws a realm (disabled or deleted).
    pub fn remove_realm(&self, realm: &str) {
        self.realms
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(realm);
    }

    /// Revokes a grant: every access token carrying `gid` is refused from
    /// now on.
    pub fn revoke_grant(&self, realm: &str, gid: &str) {
        self.revoked
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert((realm.to_string(), gid.to_string()));
    }

    /// Replaces the revoked set of `realm` (epoch reload from the store).
    pub fn replace_revoked(&self, realm: &str, gids: impl IntoIterator<Item = String>) {
        let mut set = self.revoked.write().unwrap_or_else(|p| p.into_inner());
        set.retain(|(r, _)| r != realm);
        set.extend(gids.into_iter().map(|g| (realm.to_string(), g)));
    }
}

impl InternalRealmSource for MemoryRealmSource {
    fn realm(&self, realm: &str) -> Option<Arc<RealmTrust>> {
        self.realms
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(realm)
            .cloned()
    }

    fn is_grant_revoked(&self, realm: &str, gid: &str) -> bool {
        self.revoked
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&(realm.to_string(), gid.to_string()))
    }
}

static INTERNAL_REALMS: OnceLock<Arc<dyn InternalRealmSource>> = OnceLock::new();

/// Installs the process-wide realm source (the `hfs` binary does this when
/// `HFS_IDP=on`). Returns `false` when one was installed already.
pub fn install_internal_realms(source: Arc<dyn InternalRealmSource>) -> bool {
    INTERNAL_REALMS.set(source).is_ok()
}

/// The process-wide realm source, `None` while the IdP is off.
pub fn internal_realms() -> Option<Arc<dyn InternalRealmSource>> {
    INTERNAL_REALMS.get().cloned()
}

/// Token verifier of one endpoint bound to one internal realm.
pub struct InternalVerifier {
    source: Option<Arc<dyn InternalRealmSource>>,
    realm: String,
    endpoint: String,
    audience_override: Option<String>,
    leeway_secs: u64,
}

impl InternalVerifier {
    /// The verifier of `endpoint` bound to `realm`. `source` is `None` when
    /// the IdP is off (every token → `503`). `audience_override` replaces
    /// the audience the realm derives for the endpoint (its public base URL).
    pub fn new(
        source: Option<Arc<dyn InternalRealmSource>>,
        realm: impl Into<String>,
        endpoint: impl Into<String>,
        audience_override: Option<String>,
        leeway_secs: u64,
    ) -> Self {
        Self {
            source,
            realm: realm.into(),
            endpoint: endpoint.into(),
            audience_override: audience_override.filter(|a| !a.trim().is_empty()),
            leeway_secs,
        }
    }

    fn unavailable(&self, why: &str) -> AuthError {
        AuthError::JwksFetchError(format!(
            "internal authorization server realm '{}' {why}",
            self.realm
        ))
    }
}

/// Whether a JWS `typ` names an RFC 9068 access token.
fn is_at_jwt(typ: Option<&str>) -> bool {
    typ.is_some_and(|t| {
        let t = t.trim().to_ascii_lowercase();
        t == "at+jwt" || t == "application/at+jwt"
    })
}

#[async_trait]
impl AuthProvider for InternalVerifier {
    async fn authenticate(&self, authorization_header: &str) -> Result<Principal, AuthError> {
        let source = self.source.as_ref().ok_or_else(|| {
            self.unavailable("is unavailable: the built-in authorization server is off (HFS_IDP)")
        })?;
        let trust = source
            .realm(&self.realm)
            .ok_or_else(|| self.unavailable("does not exist or is disabled"))?;
        let audience = match &self.audience_override {
            Some(a) => a.clone(),
            None => trust
                .audiences
                .get(&self.endpoint)
                .cloned()
                .ok_or_else(|| self.unavailable("is not bound to this endpoint"))?,
        };

        let (scheme, token) = authorization_header
            .split_once(' ')
            .ok_or_else(|| AuthError::InvalidTokenFormat("expected 'Bearer <token>'".into()))?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return Err(AuthError::InvalidTokenFormat(
                "Authorization header must use the Bearer scheme".into(),
            ));
        }
        let token = token.trim();
        if token.is_empty() {
            return Err(AuthError::InvalidTokenFormat("Empty token".into()));
        }

        let header = decode_header(token)
            .map_err(|e| AuthError::InvalidTokenFormat(format!("JWS header: {e}")))?;
        if !is_at_jwt(header.typ.as_deref()) {
            return Err(AuthError::InvalidTokenFormat(
                "not an access token (typ must be at+jwt, RFC 9068)".into(),
            ));
        }
        let kid = header
            .kid
            .as_deref()
            .ok_or_else(|| AuthError::InvalidTokenFormat("JWT header missing 'kid'".into()))?;
        let key = trust.keys.get(kid).ok_or_else(|| AuthError::UnknownKid {
            kid: kid.to_string(),
        })?;
        if header.alg != key.alg {
            return Err(AuthError::UnsupportedAlgorithm {
                alg: format!("{:?}", header.alg),
            });
        }

        let mut validation = Validation::new(key.alg);
        validation.set_issuer(&[trust.issuer.as_str()]);
        validation.set_audience(&[audience.as_str()]);
        validation.leeway = self.leeway_secs;
        validation.validate_exp = true;
        validation.validate_nbf = true;
        for claim in ["exp", "iss", "aud", "sub"] {
            validation.required_spec_claims.insert(claim.to_string());
        }
        let data = decode::<serde_json::Value>(token, &key.key, &validation).map_err(|e| {
            use jsonwebtoken::errors::ErrorKind as K;
            match e.kind() {
                K::ExpiredSignature => AuthError::TokenExpired,
                K::InvalidSignature => AuthError::InvalidSignature,
                K::ImmatureSignature => {
                    AuthError::ValidationError("Token not yet valid (nbf)".into())
                }
                K::InvalidAudience => AuthError::ValidationError("Invalid audience".into()),
                K::InvalidIssuer => AuthError::ValidationError("Invalid issuer".into()),
                K::MissingRequiredClaim(c) => {
                    AuthError::ValidationError(format!("Missing required claim: {c}"))
                }
                _ => AuthError::ValidationError(format!("Token validation failed: {e}")),
            }
        })?;
        let claims = data.claims;
        let map = claims
            .as_object()
            .ok_or_else(|| AuthError::ValidationError("claims are not an object".into()))?;
        let text = |name: &str| {
            map.get(name)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        };
        let subject = text("sub")
            .ok_or_else(|| AuthError::ValidationError("Missing 'sub' claim".into()))?
            .to_string();
        text("client_id")
            .ok_or_else(|| AuthError::ValidationError("Missing 'client_id' claim".into()))?;
        let gid =
            text("gid").ok_or_else(|| AuthError::ValidationError("Missing 'gid' claim".into()))?;
        if source.is_grant_revoked(&self.realm, gid) {
            return Err(AuthError::ValidationError("Token revoked".into()));
        }
        let exp = map
            .get("exp")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| AuthError::ValidationError("Missing 'exp' claim".into()))?;
        let expires_at = chrono::DateTime::from_timestamp(exp, 0)
            .ok_or_else(|| AuthError::ValidationError("Invalid 'exp' timestamp".into()))?;
        let scopes = match map.get("scope") {
            Some(serde_json::Value::String(s)) => {
                ScopeSet::parse_array(&s.split_whitespace().map(String::from).collect::<Vec<_>>())
            }
            _ => ScopeSet::parse_array(&[]),
        };
        let launch_context = LaunchContext::from_claims(map, "patient", "encounter", "fhirUser");
        let jti = text("jti").map(String::from);
        let standard = ["sub", "iss", "exp", "iat", "nbf", "aud", "jti", "scope"];
        let custom_claims = map
            .iter()
            .filter(|(k, _)| !standard.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Ok(Principal {
            subject,
            issuer: trust.issuer.clone(),
            tenant_id: None,
            scopes,
            jti,
            expires_at,
            custom_claims,
            launch_context,
        })
    }

    fn name(&self) -> &str {
        "internal-realm"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typ_must_be_at_jwt() {
        assert!(is_at_jwt(Some("at+jwt")));
        assert!(is_at_jwt(Some("AT+JWT")));
        assert!(is_at_jwt(Some("application/at+jwt")));
        assert!(!is_at_jwt(Some("JWT")));
        assert!(!is_at_jwt(None));
    }

    #[test]
    fn jwk_refusals() {
        let sym = serde_json::json!({"kty":"oct","kid":"k","alg":"HS256","k":"AAAA"});
        assert!(TrustedKey::from_public_jwk(&sym).is_err());
        let none = serde_json::json!({"kty":"RSA","kid":"k","alg":"none","n":"AQAB","e":"AQAB"});
        assert!(TrustedKey::from_public_jwk(&none).is_err());
        let mismatch =
            serde_json::json!({"kty":"RSA","kid":"k","alg":"ES256","n":"AQAB","e":"AQAB"});
        assert!(TrustedKey::from_public_jwk(&mismatch).is_err());
        let private = serde_json::json!({"kty":"EC","kid":"k","alg":"ES256","crv":"P-256","x":"a","y":"b","d":"c"});
        assert!(
            TrustedKey::from_public_jwk(&private)
                .unwrap_err()
                .contains("private")
        );
        let no_kid = serde_json::json!({"kty":"EC","alg":"ES256","crv":"P-256","x":"a","y":"b"});
        assert!(TrustedKey::from_public_jwk(&no_kid).is_err());
    }
}
