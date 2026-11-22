//! `POST /idp/{realm}/token` (RFC 6749 §3.2, §4.1.3, §4.4, §6; RFC 7636
//! §4.5; RFC 7523; SMART "Obtain access token", "Refresh access token",
//! Backend Services).
//!
//! Grants: `authorization_code`, `refresh_token`, `client_credentials`.
//! Everything else — notably `password` (RFC 9700 §2.4 "MUST NOT") — is
//! `unsupported_grant_type`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chrono::{DateTime, Duration, Utc};
use helios_idp::client::{ClientAuth, ClientSpec, GrantType};
use helios_idp::events::IdpEvent;
use helios_idp::launch::LaunchContext;
use helios_idp::runtime::RealmRuntime;
use helios_idp::scope::{self, Scope, ScopeContext};
use helios_idp::store::{CodeTake, GrantRecord};
use helios_idp::token::{
    AccessTokenClaims, AccessTokenInput, HandleKind, IdTokenClaims, RefreshDecision, RefreshFamily,
    at_hash, evaluate_refresh, handle_hash, mint_handle,
};
use helios_idp::util::random_b64;
use helios_idp::{OAuthError, OAuthErrorCode, pkce};
use serde_json::{Value, json};

use crate::IdpServer;
use crate::client_auth::{self, AuthenticatedClient};
use crate::http::{self, Params};

fn server_error(e: impl std::fmt::Display) -> OAuthError {
    tracing::error!(error = %e, "idp token endpoint failure");
    OAuthError::new(OAuthErrorCode::ServerError, "temporary failure")
}

pub(crate) async fn token(
    State(s): State<IdpServer>,
    Path(realm_name): Path<String>,
    headers: HeaderMap,
    ext: axum::http::Extensions,
    body: Bytes,
) -> Response {
    let Some(realm) = http::realm(&s, &realm_name) else {
        return http::not_found();
    };
    let ip = http::client_ip(&ext);
    match handle(&s, &realm, &headers, &body, &ip).await {
        Ok(v) => http::json_no_store(v),
        Err(e) => http::oauth_error(&e, realm.issuer()),
    }
}

async fn handle(
    s: &IdpServer,
    realm: &Arc<RealmRuntime>,
    headers: &HeaderMap,
    body: &[u8],
    ip: &Option<String>,
) -> Result<Value, OAuthError> {
    if !http::is_form(headers) {
        return Err(OAuthError::invalid_request(
            "the token request must be application/x-www-form-urlencoded",
        ));
    }
    let p = Params::parse(body);
    if p.has("access_token") {
        return Err(OAuthError::invalid_request(
            "unexpected access_token parameter",
        ));
    }
    let client = client_auth::authenticate(s, realm, headers, &p, &realm.urls.token, ip).await?;
    if let Some(d) = p.first_dup() {
        return Err(OAuthError::invalid_request(format!(
            "parameter {d} is repeated"
        )));
    }
    let gt = p
        .get("grant_type")
        .ok_or_else(|| OAuthError::invalid_request("grant_type is required"))?;
    let (grant, gt_name) = match gt {
        "authorization_code" => (GrantType::AuthorizationCode, gt),
        "refresh_token" => (GrantType::RefreshToken, gt),
        "client_credentials" => (GrantType::ClientCredentials, gt),
        _ => {
            return Err(OAuthError::new(
                OAuthErrorCode::UnsupportedGrantType,
                format!("grant_type {gt:?} is not supported"),
            ));
        }
    };
    if !client.spec.grant_types.contains(&grant) {
        return Err(OAuthError::new(
            OAuthErrorCode::UnauthorizedClient,
            format!("the client may not use {gt_name}"),
        ));
    }
    if realm.access_token_signer().is_none() {
        tracing::error!(realm = %realm.spec.name.as_str(), problem = ?realm.key_problem, "no signing key");
        return Err(OAuthError::new(
            OAuthErrorCode::TemporarilyUnavailable,
            "the authorization server has no usable signing key",
        ));
    }
    match grant {
        GrantType::AuthorizationCode => authorization_code(s, realm, &client, &p, ip).await,
        GrantType::RefreshToken => refresh(s, realm, &client, &p, ip).await,
        _ => client_credentials(s, realm, &client, &p, ip).await,
    }
}

fn ttl(realm: &RealmRuntime, client: &ClientSpec, backend: bool) -> u32 {
    let cap = if backend {
        realm.spec.lifetimes.backend_access_token
    } else {
        realm.spec.lifetimes.access_token
    };
    client
        .access_token_ttl_secs
        .map_or(cap, |t| t.clamp(1, cap))
}

struct Issued {
    access_token: String,
    jti: String,
    expires_in: u32,
}

#[allow(clippy::too_many_arguments)]
fn sign_access_token(
    realm: &RealmRuntime,
    client: &ClientSpec,
    subject: &str,
    audience: Vec<String>,
    scope: &str,
    ttl_secs: u32,
    auth_time: Option<DateTime<Utc>>,
    fhir_user: Option<String>,
    context: LaunchContext,
    gid: &str,
    now: DateTime<Utc>,
) -> Result<Issued, OAuthError> {
    let key = realm
        .access_token_signer()
        .ok_or_else(|| server_error("no signing key"))?;
    let claims = AccessTokenClaims::build(
        AccessTokenInput {
            issuer: realm.issuer(),
            subject,
            audience,
            client_id: &client.client_id,
            scope,
            ttl_secs,
            auth_time,
            fhir_user,
            context,
            cnf: None,
            grant_id: gid,
        },
        now,
    );
    let jti = claims.jti.clone();
    let access_token = key.sign_jws("at+jwt", &claims).map_err(server_error)?;
    Ok(Issued {
        access_token,
        jti,
        expires_in: ttl_secs,
    })
}

#[allow(clippy::too_many_arguments)]
fn sign_id_token(
    realm: &RealmRuntime,
    client_id: &str,
    subject: &str,
    auth_time: DateTime<Utc>,
    nonce: Option<String>,
    sid: Option<String>,
    amr: Vec<String>,
    acr: Option<String>,
    fhir_user: Option<String>,
    profile: (Option<String>, Option<String>),
    access_token: &str,
    now: DateTime<Utc>,
) -> Result<String, OAuthError> {
    let key = realm
        .id_token_signer()
        .ok_or_else(|| server_error("no RS256 key for id_tokens"))?;
    let c = IdTokenClaims {
        iss: realm.issuer().to_string(),
        sub: subject.to_string(),
        aud: client_id.to_string(),
        exp: (now + Duration::seconds(realm.spec.lifetimes.id_token as i64)).timestamp(),
        iat: now.timestamp(),
        auth_time: auth_time.timestamp(),
        nonce,
        at_hash: Some(at_hash(access_token, key.alg())),
        sid,
        acr,
        amr: (!amr.is_empty()).then_some(amr),
        fhir_user,
        name: profile.0,
        email: profile.1,
    };
    key.sign_jws("JWT", &c).map_err(server_error)
}

fn base_response(
    issued: &Issued,
    scope: &str,
    ctx: &LaunchContext,
) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("access_token".into(), json!(issued.access_token));
    m.insert("token_type".into(), json!("Bearer"));
    m.insert("expires_in".into(), json!(issued.expires_in));
    m.insert("scope".into(), json!(scope));
    // SMART: launch context at the top level of the token response.
    ctx.write_into(&mut m);
    m
}

async fn profile_claims(
    s: &IdpServer,
    realm: &RealmRuntime,
    subject: &str,
    scopes: &[String],
) -> (Option<String>, Option<String>) {
    let want_name = scopes.iter().any(|x| x == "profile");
    let want_email = scopes.iter().any(|x| x == "email");
    if !want_name && !want_email {
        return (None, None);
    }
    match s.inner.rt.store().get_user(&realm.id, subject).await {
        Ok(Some(u)) => (
            want_name.then(|| u.display_name.clone().unwrap_or(u.username.clone())),
            if want_email { u.email.clone() } else { None },
        ),
        _ => (None, None),
    }
}

fn wants_refresh(client: &ClientSpec, scopes: &[String]) -> bool {
    client.grant_types.contains(&GrantType::RefreshToken)
        && scopes
            .iter()
            .any(|s| s == "offline_access" || s == "online_access")
}

async fn authorization_code(
    s: &IdpServer,
    realm: &Arc<RealmRuntime>,
    client: &AuthenticatedClient,
    p: &Params,
    ip: &Option<String>,
) -> Result<Value, OAuthError> {
    let store = s.inner.rt.store();
    let now = Utc::now();
    let code = p
        .get("code")
        .ok_or_else(|| OAuthError::invalid_request("code is required"))?;
    let redirect_uri = p
        .get("redirect_uri")
        .ok_or_else(|| OAuthError::invalid_request("redirect_uri is required"))?;
    let hash = handle_hash(HandleKind::AuthorizationCode, code)
        .ok_or_else(|| OAuthError::invalid_grant("invalid authorization code"))?;
    let c = match store
        .take_code(&realm.id, &hash, now)
        .await
        .map_err(server_error)?
    {
        CodeTake::Fresh(c) => *c,
        CodeTake::Replayed { grant_id } => {
            // RFC 6749 §4.1.2 / RFC 9700 §4.2.4: revoke what the code yielded.
            store
                .revoke_grant(&realm.id, &grant_id, "authorization code replay", now)
                .await
                .map_err(server_error)?;
            s.inner.rt.mark_revoked(realm.spec.name.as_str(), &grant_id);
            s.inner.rt.emit(IdpEvent::CodeReplay {
                realm: realm.spec.name.as_str().to_string(),
                client_id: client.spec.client_id.clone(),
                ip: ip.clone(),
            });
            return Err(OAuthError::invalid_grant("authorization code already used"));
        }
        CodeTake::Unknown => {
            return Err(OAuthError::invalid_grant(
                "invalid or expired authorization code",
            ));
        }
    };
    let code_gid = c.grant_id.clone();
    let revoke = |why: &'static str| {
        let gid = code_gid.clone();
        async move {
            let _ = store.revoke_grant(&realm.id, &gid, why, now).await;
            s.inner.rt.mark_revoked(realm.spec.name.as_str(), &gid);
        }
    };
    if c.client_id != client.spec.client_id {
        revoke("code presented by another client").await;
        return Err(OAuthError::invalid_grant(
            "the code was issued to another client",
        ));
    }
    if c.redirect_uri != redirect_uri {
        revoke("redirect_uri mismatch").await;
        return Err(OAuthError::invalid_grant(
            "redirect_uri does not match the authorization request",
        ));
    }
    if let Err(e) = pkce::verify(p.get("code_verifier"), &c.code_challenge) {
        revoke("PKCE verification failed").await;
        return Err(e);
    }
    let grant = store
        .get_grant(&realm.id, &c.grant_id)
        .await
        .map_err(server_error)?
        .filter(|g| g.revoked_at.is_none())
        .ok_or_else(|| OAuthError::invalid_grant("the authorization was revoked"))?;
    let scope = c.scopes.join(" ");
    let ttl_secs = ttl(realm, &client.spec, false);
    let issued = sign_access_token(
        realm,
        &client.spec,
        &c.subject,
        c.audience.clone(),
        &scope,
        ttl_secs,
        Some(c.auth_time),
        c.fhir_user.clone(),
        c.context.clone(),
        &c.grant_id,
        now,
    )?;
    let mut resp = base_response(&issued, &scope, &c.context);
    let mut grant_expiry = now + Duration::seconds(ttl_secs as i64);
    if wants_refresh(&client.spec, &c.scopes) {
        let rt = mint_handle(HandleKind::RefreshToken);
        let lt = &realm.spec.lifetimes;
        let online_only = !c.scopes.iter().any(|x| x == "offline_access");
        let fam = RefreshFamily {
            family_id: format!("f_{}", random_b64(16)),
            grant_id: c.grant_id.clone(),
            client_id: client.spec.client_id.clone(),
            subject: c.subject.clone(),
            scopes: c.scopes.clone(),
            audience: c.audience.clone(),
            endpoint: c.endpoint.clone(),
            fhir_user: c.fhir_user.clone(),
            auth_time: Some(c.auth_time),
            context: c.context.clone(),
            current_generation: 0,
            created_at: now,
            last_used_at: now,
            absolute_expiry: now + Duration::seconds(lt.refresh_token_absolute as i64),
            // online_access: the family dies with the SSO session.
            sso_session_id: if online_only {
                c.sso_session_id.clone()
            } else {
                None
            },
            revoked_at: None,
            revoked_reason: None,
        };
        grant_expiry = fam.absolute_expiry;
        store
            .create_family(&realm.id, fam, &rt.hash)
            .await
            .map_err(server_error)?;
        resp.insert("refresh_token".into(), json!(rt.value));
    }
    if c.scopes.iter().any(|x| x == "openid") {
        let profile = profile_claims(s, realm, &c.subject, &c.scopes).await;
        let id = sign_id_token(
            realm,
            &client.spec.client_id,
            &c.subject,
            c.auth_time,
            c.nonce.clone(),
            c.sid.clone(),
            c.amr.clone(),
            c.acr.clone(),
            c.fhir_user.clone(),
            profile,
            &issued.access_token,
            now,
        )?;
        resp.insert("id_token".into(), json!(id));
    }
    store
        .extend_grant(&realm.id, &grant.id, grant_expiry)
        .await
        .map_err(server_error)?;
    s.inner.rt.emit(IdpEvent::TokenIssued {
        realm: realm.spec.name.as_str().to_string(),
        client_id: client.spec.client_id.clone(),
        subject: c.subject.clone(),
        scope,
        aud: c.audience.clone(),
        gid: c.grant_id.clone(),
        jti: issued.jti.clone(),
        grant_type: "authorization_code".into(),
        patient: c.context.patient.clone(),
        ip: ip.clone(),
    });
    Ok(Value::Object(resp))
}

async fn refresh(
    s: &IdpServer,
    realm: &Arc<RealmRuntime>,
    client: &AuthenticatedClient,
    p: &Params,
    ip: &Option<String>,
) -> Result<Value, OAuthError> {
    let store = s.inner.rt.store();
    let now = Utc::now();
    let presented = p
        .get("refresh_token")
        .ok_or_else(|| OAuthError::invalid_request("refresh_token is required"))?;
    let hash = handle_hash(HandleKind::RefreshToken, presented)
        .ok_or_else(|| OAuthError::invalid_grant("invalid refresh token"))?;
    let (fam, generation) = store
        .get_family_by_token(&realm.id, &hash)
        .await
        .map_err(server_error)?
        .ok_or_else(|| OAuthError::invalid_grant("invalid refresh token"))?;
    // SMART: "SHALL be bound to the same client_id".
    if fam.client_id != client.spec.client_id {
        return Err(OAuthError::invalid_grant(
            "the refresh token was issued to another client",
        ));
    }
    let session_alive = match &fam.sso_session_id {
        None => true,
        Some(sh) => store
            .get_session(&realm.id, sh)
            .await
            .map_err(server_error)?
            .is_some_and(|ss| ss.alive(realm.spec.lifetimes.sso_session_idle, now)),
    };
    let reuse = |fam: RefreshFamily| async move {
        let _ = store
            .revoke_grant(&realm.id, &fam.grant_id, "refresh token reuse", now)
            .await;
        let _ = store
            .revoke_family(&realm.id, &fam.family_id, "refresh token reuse", now)
            .await;
        s.inner
            .rt
            .mark_revoked(realm.spec.name.as_str(), &fam.grant_id);
        s.inner.rt.emit(IdpEvent::RefreshReuse {
            realm: realm.spec.name.as_str().to_string(),
            client_id: fam.client_id.clone(),
            family: fam.family_id.clone(),
            ip: ip.clone(),
        });
        OAuthError::invalid_grant("refresh token already used; the authorization was revoked")
    };
    let next = match evaluate_refresh(
        &fam,
        generation,
        realm.spec.lifetimes.refresh_token_idle,
        session_alive,
        now,
    ) {
        RefreshDecision::Rotate { next_generation } => next_generation,
        RefreshDecision::ReuseDetected => return Err(reuse(fam).await),
        RefreshDecision::Revoked => return Err(OAuthError::invalid_grant("refresh token revoked")),
        RefreshDecision::Expired => return Err(OAuthError::invalid_grant("refresh token expired")),
        RefreshDecision::SessionEnded => {
            return Err(OAuthError::invalid_grant(
                "the session of this refresh token ended",
            ));
        }
    };
    let grant = store
        .get_grant(&realm.id, &fam.grant_id)
        .await
        .map_err(server_error)?
        .filter(|g| g.revoked_at.is_none())
        .ok_or_else(|| OAuthError::invalid_grant("the authorization was revoked"))?;
    let scopes = match p.get("scope") {
        None => fam.scopes.clone(),
        Some(req) => {
            let req = scope::split_scopes(req);
            if req.is_empty() || !scope::is_narrowing(&req, &fam.scopes) {
                return Err(OAuthError::invalid_scope(
                    "the requested scope exceeds the original grant",
                ));
            }
            req
        }
    };
    let new_rt = mint_handle(HandleKind::RefreshToken);
    let rotated = store
        .rotate_family(&realm.id, &fam.family_id, generation, &new_rt.hash, now)
        .await
        .map_err(server_error)?;
    if !rotated {
        // Lost a concurrent race with the same token: that is reuse.
        return Err(reuse(fam).await);
    }
    debug_assert_eq!(next, generation + 1);
    let scope = scopes.join(" ");
    let ttl_secs = ttl(realm, &client.spec, false);
    let issued = sign_access_token(
        realm,
        &client.spec,
        &fam.subject,
        fam.audience.clone(),
        &scope,
        ttl_secs,
        fam.auth_time,
        if scopes.iter().any(|x| x == "fhirUser") {
            fam.fhir_user.clone()
        } else {
            None
        },
        fam.context.clone(),
        &fam.grant_id,
        now,
    )?;
    let mut resp = base_response(&issued, &scope, &fam.context);
    resp.insert("refresh_token".into(), json!(new_rt.value));
    if scopes.iter().any(|x| x == "openid") {
        let profile = profile_claims(s, realm, &fam.subject, &scopes).await;
        let id = sign_id_token(
            realm,
            &client.spec.client_id,
            &fam.subject,
            fam.auth_time.unwrap_or(grant.created_at),
            None,
            None,
            vec![],
            None,
            if scopes.iter().any(|x| x == "fhirUser") {
                fam.fhir_user.clone()
            } else {
                None
            },
            profile,
            &issued.access_token,
            now,
        )?;
        resp.insert("id_token".into(), json!(id));
    }
    let new_expiry = (now + Duration::seconds(realm.spec.lifetimes.refresh_token_idle as i64))
        .min(fam.absolute_expiry)
        .max(now + Duration::seconds(ttl_secs as i64));
    store
        .extend_grant(&realm.id, &grant.id, new_expiry)
        .await
        .map_err(server_error)?;
    s.inner.rt.emit(IdpEvent::TokenRefreshed {
        realm: realm.spec.name.as_str().to_string(),
        client_id: client.spec.client_id.clone(),
        subject: fam.subject.clone(),
        scope,
        gid: fam.grant_id.clone(),
        jti: issued.jti,
        ip: ip.clone(),
    });
    Ok(Value::Object(resp))
}

async fn client_credentials(
    s: &IdpServer,
    realm: &Arc<RealmRuntime>,
    client: &AuthenticatedClient,
    p: &Params,
    ip: &Option<String>,
) -> Result<Value, OAuthError> {
    if !realm.spec.features.backend_services {
        return Err(OAuthError::new(
            OAuthErrorCode::UnauthorizedClient,
            "client_credentials is disabled in this realm",
        ));
    }
    if matches!(client.spec.auth, ClientAuth::None) {
        return Err(OAuthError::invalid_client(
            "public clients cannot use client_credentials",
        ));
    }
    let requested = scope::split_scopes(
        p.get("scope")
            .ok_or_else(|| OAuthError::invalid_scope("scope is required"))?,
    );
    // Only system/ resource scopes make sense without a user.
    let system_only: Vec<String> = requested
        .iter()
        .filter(|t| {
            matches!(scope::parse_scope(t), Scope::Resource(r) if r.context == ScopeContext::System
                && (!r.v1 || realm.spec.features.permission_v1))
        })
        .cloned()
        .collect();
    // Backend Services: requested scope must fall within the pre-authorized scope.
    let g = scope::grant(&system_only, &client.spec.scopes);
    if g.granted.is_empty() {
        return Err(OAuthError::invalid_scope(
            "none of the requested scopes is pre-authorized for this client",
        ));
    }
    let audience: Vec<String> = match p.get("aud").or(p.get("resource")) {
        Some(a) => vec![
            realm
                .endpoint_for_aud(a)
                .map(|(_, b)| b.clone())
                .ok_or_else(|| {
                    OAuthError::new(
                        OAuthErrorCode::InvalidTarget,
                        "aud is not a FHIR endpoint served by this authorization server",
                    )
                })?,
        ],
        None => realm.endpoints.iter().map(|(_, b)| b.clone()).collect(),
    };
    if audience.is_empty() {
        return Err(OAuthError::new(
            OAuthErrorCode::InvalidTarget,
            "no FHIR endpoint is bound to this authorization server",
        ));
    }
    let now = Utc::now();
    let ttl_secs = ttl(realm, &client.spec, true);
    let scope = g.scope_string();
    let gid = format!("g_{}", random_b64(16));
    let store = s.inner.rt.store();
    store
        .put_grant(GrantRecord {
            id: gid.clone(),
            realm_id: realm.id.clone(),
            client_id: client.spec.client_id.clone(),
            subject: client.spec.client_id.clone(),
            scopes: g.granted.clone(),
            audience: audience.clone(),
            context: LaunchContext::default(),
            fhir_user: None,
            sso_session_id: None,
            auth_time: None,
            created_at: now,
            expires_at: now + Duration::seconds(ttl_secs as i64),
            revoked_at: None,
            revoked_reason: None,
        })
        .await
        .map_err(server_error)?;
    // RFC 9068 §2.2: sub = client_id for client_credentials.
    let issued = sign_access_token(
        realm,
        &client.spec,
        &client.spec.client_id,
        audience.clone(),
        &scope,
        ttl_secs,
        None,
        None,
        LaunchContext::default(),
        &gid,
        now,
    )?;
    let resp = base_response(&issued, &scope, &LaunchContext::default());
    s.inner.rt.emit(IdpEvent::TokenIssued {
        realm: realm.spec.name.as_str().to_string(),
        client_id: client.spec.client_id.clone(),
        subject: client.spec.client_id.clone(),
        scope,
        aud: audience,
        gid,
        jti: issued.jti,
        grant_type: "client_credentials".into(),
        patient: None,
        ip: ip.clone(),
    });
    Ok(Value::Object(resp))
}
