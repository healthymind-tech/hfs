//! Client authentication at the token, introspection and revocation
//! endpoints (package I3; RFC 6749 §2.3, RFC 7521 §4.2, RFC 7523 §2.2/§3,
//! SMART "Client Authentication: Asymmetric/Symmetric", Backend Services).
//!
//! * Exactly one method per request (RFC 6749 §2.3: "MUST NOT use more than
//!   one authentication method") — otherwise `invalid_request`.
//! * The presented method must be the client's registered method; a public
//!   client never presents a secret, a confidential client never
//!   authenticates with `client_id` alone.
//! * `client_secret_basic` / `client_secret_post`: argon2id verification
//!   (bounded concurrency, dummy verification for unknown clients).
//! * `private_key_jwt`: `iss` = `sub` = `client_id`; single `aud` equal to
//!   the token endpoint (or the issuer, or the endpoint being called);
//!   `exp` required and at most 5 minutes ahead; `jti` required and unseen
//!   (replay cache until `exp`); `jku`, if present, must equal the
//!   registered `jwks_uri`; `kid` selects exactly one key; RS384/ES384
//!   (+RS256/ES256 by default); `none`/`HS*` impossible.
//! * Every failure is `invalid_client` (SMART Backend Services), with the
//!   reason only in the security event.

use axum::http::{HeaderMap, header};
use base64::Engine as _;
use chrono::{DateTime, Duration, TimeZone, Utc};
use helios_idp::client::{ClientAuth, ClientSpec};
use helios_idp::events::IdpEvent;
use helios_idp::jws::{self, JwsError};
use helios_idp::keys::SigningAlg;
use helios_idp::runtime::RealmRuntime;
use helios_idp::{OAuthError, password};
use serde_json::Value;

use crate::IdpServer;
use crate::http::Params;

/// `client_assertion_type` for JWT client authentication (RFC 7523 §2.2).
pub const JWT_BEARER_ASSERTION: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";
/// Longest accepted assertion lifetime from now (SMART: "no more than five
/// minutes in the future").
pub const MAX_ASSERTION_LIFETIME_SECS: i64 = 300;
/// Clock skew tolerated on `exp`/`nbf`/`iat`.
pub const SKEW_SECS: i64 = 30;
/// Default accepted assertion algorithms when a client lists none.
pub const DEFAULT_ASSERTION_ALGS: [SigningAlg; 4] = [
    SigningAlg::RS384,
    SigningAlg::ES384,
    SigningAlg::RS256,
    SigningAlg::ES256,
];

/// How the client authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    None,
    ClientSecretBasic,
    ClientSecretPost,
    PrivateKeyJwt,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::None => "none",
            Method::ClientSecretBasic => "client_secret_basic",
            Method::ClientSecretPost => "client_secret_post",
            Method::PrivateKeyJwt => "private_key_jwt",
        }
    }
}

/// An authenticated (or, for public clients, identified) client.
#[derive(Debug, Clone)]
pub struct AuthenticatedClient {
    pub spec: ClientSpec,
    pub method: Method,
}

fn fail(
    server: &IdpServer,
    realm: &RealmRuntime,
    client_id: Option<&str>,
    reason: &str,
    ip: &Option<String>,
) -> OAuthError {
    server.inner.rt.emit(IdpEvent::ClientAuthFailed {
        realm: realm.spec.name.as_str().to_string(),
        client_id: client_id.map(str::to_string),
        reason: reason.to_string(),
        ip: ip.clone(),
    });
    OAuthError::invalid_client("client authentication failed")
}

fn form_decode(s: &str) -> Option<String> {
    // RFC 6749 §2.3.1: id and secret are application/x-www-form-urlencoded
    // before being placed in the Basic credentials.
    let wrapped = format!("v={s}");
    url::form_urlencoded::parse(wrapped.as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
}

fn basic_credentials(headers: &HeaderMap) -> Result<Option<(String, String)>, ()> {
    let Some(v) = headers.get(header::AUTHORIZATION) else {
        return Ok(None);
    };
    let v = v.to_str().map_err(|_| ())?;
    let Some((scheme, rest)) = v.split_once(' ') else {
        return Err(());
    };
    if !scheme.eq_ignore_ascii_case("basic") {
        // A Bearer header is not client authentication (introspection
        // handles it separately); anything else is malformed here.
        return if scheme.eq_ignore_ascii_case("bearer") {
            Ok(None)
        } else {
            Err(())
        };
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(rest.trim())
        .map_err(|_| ())?;
    let s = String::from_utf8(raw).map_err(|_| ())?;
    let (id, secret) = s.split_once(':').ok_or(())?;
    Ok(Some((
        form_decode(id).ok_or(())?,
        form_decode(secret).ok_or(())?,
    )))
}

async fn verify_secret(server: &IdpServer, secret: String, phc: String) -> bool {
    let Ok(_permit) = server.inner.argon2.acquire().await else {
        return false;
    };
    tokio::task::spawn_blocking(move || password::verify(&secret, &phc))
        .await
        .unwrap_or(false)
}

/// Authenticates the client of a token / introspection / revocation request.
/// `endpoint_url` is the URL being called (accepted as assertion `aud`).
pub(crate) async fn authenticate(
    server: &IdpServer,
    realm: &RealmRuntime,
    headers: &HeaderMap,
    params: &Params,
    endpoint_url: &str,
    ip: &Option<String>,
) -> Result<AuthenticatedClient, OAuthError> {
    let basic = basic_credentials(headers)
        .map_err(|_| fail(server, realm, None, "malformed Authorization header", ip))?;
    let has_assertion = params.has("client_assertion") || params.has("client_assertion_type");
    let has_post_secret = params.has("client_secret");
    let n_methods = basic.is_some() as u8 + has_assertion as u8 + has_post_secret as u8;
    if n_methods > 1 {
        return Err(OAuthError::invalid_request(
            "use exactly one client authentication method",
        ));
    }
    for p in [
        "client_id",
        "client_secret",
        "client_assertion",
        "client_assertion_type",
    ] {
        if params.is_dup(p) {
            return Err(OAuthError::invalid_request(format!("{p} is repeated")));
        }
    }
    let body_client_id = params.get("client_id");

    if let Some((id, secret)) = basic {
        if body_client_id.is_some_and(|b| b != id) {
            return Err(fail(
                server,
                realm,
                Some(&id),
                "client_id differs from Basic credentials",
                ip,
            ));
        }
        return secret_auth(server, realm, &id, secret, Method::ClientSecretBasic, ip).await;
    }
    if has_post_secret {
        let Some(id) = body_client_id else {
            return Err(fail(
                server,
                realm,
                None,
                "client_secret without client_id",
                ip,
            ));
        };
        let secret = params.get("client_secret").unwrap_or("").to_string();
        return secret_auth(server, realm, id, secret, Method::ClientSecretPost, ip).await;
    }
    if has_assertion {
        return jwt_auth(server, realm, params, body_client_id, endpoint_url, ip).await;
    }
    // Public client: identified by client_id only.
    let Some(id) = body_client_id else {
        return Err(fail(server, realm, None, "no client identification", ip));
    };
    let client = load(server, realm, id).await?;
    match client {
        Some(c) if matches!(c.auth, ClientAuth::None) => Ok(AuthenticatedClient {
            spec: c,
            method: Method::None,
        }),
        Some(_) => Err(fail(
            server,
            realm,
            Some(id),
            "confidential client without credentials",
            ip,
        )),
        None => Err(fail(server, realm, Some(id), "unknown client", ip)),
    }
}

async fn load(
    server: &IdpServer,
    realm: &RealmRuntime,
    client_id: &str,
) -> Result<Option<ClientSpec>, OAuthError> {
    if client_id.len() > 128 {
        return Ok(None);
    }
    server
        .inner
        .rt
        .store()
        .get_client(&realm.id, client_id)
        .await
        .map(|c| c.filter(|c| !c.disabled))
        .map_err(|e| {
            tracing::error!(error = %e, "idp store error");
            OAuthError::new(helios_idp::OAuthErrorCode::ServerError, "temporary failure")
        })
}

async fn secret_auth(
    server: &IdpServer,
    realm: &RealmRuntime,
    id: &str,
    secret: String,
    method: Method,
    ip: &Option<String>,
) -> Result<AuthenticatedClient, OAuthError> {
    let client = load(server, realm, id).await?;
    let phc = match &client {
        Some(ClientSpec {
            auth: ClientAuth::ClientSecret { secret_hash },
            ..
        }) => secret_hash.clone(),
        // Same cost as a real verification (no client enumeration by timing).
        _ => password::dummy_hash().to_string(),
    };
    let ok = verify_secret(server, secret, phc).await;
    match client {
        Some(c) if ok && matches!(c.auth, ClientAuth::ClientSecret { .. }) => {
            Ok(AuthenticatedClient { spec: c, method })
        }
        Some(c) if !matches!(c.auth, ClientAuth::ClientSecret { .. }) => Err(fail(
            server,
            realm,
            Some(id),
            &format!("client is registered for {}", c.auth.method_name()),
            ip,
        )),
        Some(_) => Err(fail(server, realm, Some(id), "wrong client secret", ip)),
        None => Err(fail(server, realm, Some(id), "unknown client", ip)),
    }
}

fn str_claim<'a>(claims: &'a serde_json::Map<String, Value>, k: &str) -> Option<&'a str> {
    claims.get(k).and_then(Value::as_str)
}

fn time_claim(claims: &serde_json::Map<String, Value>, k: &str) -> Option<i64> {
    claims.get(k).and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))
    })
}

async fn jwt_auth(
    server: &IdpServer,
    realm: &RealmRuntime,
    params: &Params,
    body_client_id: Option<&str>,
    endpoint_url: &str,
    ip: &Option<String>,
) -> Result<AuthenticatedClient, OAuthError> {
    let hint = body_client_id;
    if params.get("client_assertion_type") != Some(JWT_BEARER_ASSERTION) {
        return Err(fail(
            server,
            realm,
            hint,
            "unsupported client_assertion_type",
            ip,
        ));
    }
    let Some(assertion) = params.get("client_assertion") else {
        return Err(fail(server, realm, hint, "client_assertion missing", ip));
    };
    let (header, claims) = jws::peek(assertion)
        .map_err(|e| fail(server, realm, hint, &format!("assertion: {e}"), ip))?;
    let iss = str_claim(&claims, "iss");
    let sub = str_claim(&claims, "sub");
    let (Some(iss), Some(sub)) = (iss, sub) else {
        return Err(fail(server, realm, hint, "assertion lacks iss/sub", ip));
    };
    if iss != sub {
        return Err(fail(server, realm, Some(iss), "assertion iss != sub", ip));
    }
    if body_client_id.is_some_and(|b| b != iss) {
        return Err(fail(
            server,
            realm,
            Some(iss),
            "client_id differs from assertion iss",
            ip,
        ));
    }
    let Some(client) = load(server, realm, iss).await? else {
        return Err(fail(server, realm, Some(iss), "unknown client", ip));
    };
    let ClientAuth::PrivateKeyJwt {
        jwks_uri,
        jwks,
        algs,
    } = &client.auth
    else {
        return Err(fail(
            server,
            realm,
            Some(iss),
            &format!("client is registered for {}", client.auth.method_name()),
            ip,
        ));
    };
    // jku, when present, must be the registered jwks_uri (never followed otherwise).
    if let Some(jku) = header.get("jku") {
        match (jwks_uri.as_deref(), jku.as_str()) {
            (Some(registered), Some(presented)) if registered == presented => {}
            _ => {
                return Err(fail(
                    server,
                    realm,
                    Some(iss),
                    "jku is not the registered jwks_uri",
                    ip,
                ));
            }
        }
    }
    if header.get("x5u").is_some() || header.get("jwk").is_some() || header.get("x5c").is_some() {
        return Err(fail(
            server,
            realm,
            Some(iss),
            "embedded/remote key headers are not accepted",
            ip,
        ));
    }
    let allowed: Vec<SigningAlg> = if algs.is_empty() {
        DEFAULT_ASSERTION_ALGS.to_vec()
    } else {
        algs.clone()
    };
    let verified =
        match (jwks, jwks_uri) {
            (Some(set), _) => {
                let keys = set
                    .get("keys")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                jws::verify(assertion, &keys, &allowed)
            }
            (None, Some(uri)) => {
                let keys =
                    server.inner.jwks.keys(uri, false).await.map_err(|e| {
                        fail(server, realm, Some(iss), &format!("jwks_uri: {e}"), ip)
                    })?;
                match jws::verify(assertion, &keys, &allowed) {
                    // Key rotation at the client: refetch once.
                    Err(JwsError::NoKey) => {
                        let keys = server.inner.jwks.keys(uri, true).await.map_err(|e| {
                            fail(server, realm, Some(iss), &format!("jwks_uri: {e}"), ip)
                        })?;
                        jws::verify(assertion, &keys, &allowed)
                    }
                    other => other,
                }
            }
            (None, None) => Err(JwsError::NoKey),
        }
        .map_err(|e| fail(server, realm, Some(iss), &format!("assertion: {e}"), ip))?;
    let claims = verified.claims;

    // aud: exactly one value, one of our URLs.
    let aud: Option<&str> = match claims.get("aud") {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(Value::Array(a)) if a.len() == 1 => a[0].as_str(),
        _ => None,
    };
    let acceptable = [realm.urls.token.as_str(), realm.issuer(), endpoint_url];
    match aud {
        Some(a) if acceptable.contains(&a) => {}
        _ => {
            return Err(fail(
                server,
                realm,
                Some(iss),
                "assertion aud is not this server",
                ip,
            ));
        }
    }
    let now = Utc::now();
    let ts = now.timestamp();
    let Some(exp) = time_claim(&claims, "exp") else {
        return Err(fail(server, realm, Some(iss), "assertion has no exp", ip));
    };
    if exp + SKEW_SECS <= ts {
        return Err(fail(server, realm, Some(iss), "assertion expired", ip));
    }
    if exp > ts + MAX_ASSERTION_LIFETIME_SECS + SKEW_SECS {
        return Err(fail(
            server,
            realm,
            Some(iss),
            "assertion exp more than 5 minutes ahead",
            ip,
        ));
    }
    if time_claim(&claims, "nbf").is_some_and(|nbf| nbf > ts + SKEW_SECS) {
        return Err(fail(
            server,
            realm,
            Some(iss),
            "assertion not yet valid",
            ip,
        ));
    }
    if time_claim(&claims, "iat").is_some_and(|iat| iat > ts + SKEW_SECS) {
        return Err(fail(
            server,
            realm,
            Some(iss),
            "assertion issued in the future",
            ip,
        ));
    }
    let Some(jti) = str_claim(&claims, "jti").filter(|j| !j.is_empty() && j.len() <= 256) else {
        return Err(fail(
            server,
            realm,
            Some(iss),
            "assertion jti missing or too long",
            ip,
        ));
    };
    let keep_until: DateTime<Utc> = Utc
        .timestamp_opt(exp + SKEW_SECS, 0)
        .single()
        .unwrap_or(now + Duration::seconds(MAX_ASSERTION_LIFETIME_SECS));
    let fresh = server
        .inner
        .rt
        .store()
        .record_jti(&realm.id, iss, jti, keep_until, now)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "idp store error");
            OAuthError::new(helios_idp::OAuthErrorCode::ServerError, "temporary failure")
        })?;
    if !fresh {
        return Err(fail(server, realm, Some(iss), "assertion jti replayed", ip));
    }
    Ok(AuthenticatedClient {
        spec: client,
        method: Method::PrivateKeyJwt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_header_parsing() {
        let mut h = HeaderMap::new();
        let enc = base64::engine::general_purpose::STANDARD.encode("my%20app:s%3Acret+x");
        h.insert(
            header::AUTHORIZATION,
            format!("Basic {enc}").parse().unwrap(),
        );
        assert_eq!(
            basic_credentials(&h).unwrap(),
            Some(("my app".into(), "s:cret x".into()))
        );
        h.insert(header::AUTHORIZATION, "Basic !!!".parse().unwrap());
        assert!(basic_credentials(&h).is_err());
        h.insert(header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(basic_credentials(&h).unwrap(), None);
        h.insert(header::AUTHORIZATION, "Digest x".parse().unwrap());
        assert!(basic_credentials(&h).is_err());
    }
}
