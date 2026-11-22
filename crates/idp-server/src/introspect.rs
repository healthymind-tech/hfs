//! `POST /idp/{realm}/introspect` (RFC 7662, SMART "Token Introspection")
//! and `POST /idp/{realm}/revoke` (RFC 7009).
//!
//! Introspection callers authenticate as a client of the realm (any
//! registered method), or present a bearer access token of the realm whose
//! scope contains `introspect` (design §7.6). Unknown, malformed, expired,
//! revoked and foreign-realm tokens are all `{"active": false}`. A refresh
//! token is only described to the client it was issued to.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use helios_idp::events::IdpEvent;
use helios_idp::launch::LaunchContext;
use helios_idp::runtime::RealmRuntime;
use helios_idp::token::{HandleKind, handle_hash};
use helios_idp::{OAuthError, OAuthErrorCode};
use serde_json::{Value, json};

use crate::IdpServer;
use crate::client_auth;
use crate::http::{self, Params};

/// Who asked.
enum Caller {
    Client(String),
    Bearer,
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, tok) = v.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| tok.trim())
        .filter(|t| !t.is_empty())
}

/// Claims of an active access token of this realm (signature, `typ`,
/// `iss`, expiry, grant not revoked), or `None`.
pub(crate) async fn active_access_token(
    s: &IdpServer,
    realm: &RealmRuntime,
    token: &str,
) -> Option<serde_json::Map<String, Value>> {
    if !token.starts_with("ey") {
        return None;
    }
    let v = realm.verify_own(token, Some("at+jwt")).ok()?;
    let now = Utc::now().timestamp();
    let exp = v.claims.get("exp")?.as_i64()?;
    if exp <= now {
        return None;
    }
    let gid = v.claims.get("gid")?.as_str()?;
    if realm.grant_revoked(gid) {
        return None;
    }
    let grant = s
        .inner
        .rt
        .store()
        .get_grant(&realm.id, gid)
        .await
        .ok()
        .flatten()?;
    if grant.revoked_at.is_some() {
        return None;
    }
    Some(v.claims)
}

fn inactive() -> Response {
    http::json_no_store(json!({"active": false}))
}

pub(crate) async fn introspect(
    State(s): State<IdpServer>,
    Path(realm_name): Path<String>,
    headers: HeaderMap,
    ext: axum::http::Extensions,
    body: Bytes,
) -> Response {
    let Some(realm) = http::realm(&s, &realm_name) else {
        return http::not_found();
    };
    if !realm.spec.features.introspection {
        return http::not_found();
    }
    let ip = http::client_ip(&ext);
    if !http::is_form(&headers) {
        return http::oauth_error(
            &OAuthError::invalid_request("application/x-www-form-urlencoded required"),
            realm.issuer(),
        );
    }
    let p = Params::parse(&body);
    let caller = if let Some(bt) = bearer_token(&headers) {
        match active_access_token(&s, &realm, bt).await {
            Some(c)
                if c.get("scope")
                    .and_then(Value::as_str)
                    .is_some_and(|sc| sc.split(' ').any(|t| t == "introspect")) =>
            {
                Caller::Bearer
            }
            Some(_) => {
                return http::bearer_error(
                    StatusCode::FORBIDDEN,
                    Some(OAuthErrorCode::InsufficientScope),
                    "the token lacks the introspect scope",
                    realm.issuer(),
                );
            }
            None => {
                return http::bearer_error(
                    StatusCode::UNAUTHORIZED,
                    Some(OAuthErrorCode::InvalidToken),
                    "invalid bearer token",
                    realm.issuer(),
                );
            }
        }
    } else {
        match client_auth::authenticate(&s, &realm, &headers, &p, &realm.urls.introspection, &ip)
            .await
        {
            Ok(c) => Caller::Client(c.spec.client_id),
            Err(e) => return http::oauth_error(&e, realm.issuer()),
        }
    };
    if let Some(d) = p.first_dup() {
        return http::oauth_error(
            &OAuthError::invalid_request(format!("parameter {d} is repeated")),
            realm.issuer(),
        );
    }
    let Some(token) = p.get("token") else {
        return http::oauth_error(
            &OAuthError::invalid_request("token is required"),
            realm.issuer(),
        );
    };

    if token.starts_with(HandleKind::RefreshToken.prefix()) {
        let Caller::Client(caller_id) = caller else {
            return inactive();
        };
        return introspect_refresh(&s, &realm, token, &caller_id).await;
    }
    let Some(c) = active_access_token(&s, &realm, token).await else {
        return inactive();
    };
    let mut out = serde_json::Map::new();
    out.insert("active".into(), json!(true));
    for k in [
        "scope",
        "client_id",
        "sub",
        "exp",
        "iat",
        "iss",
        "aud",
        "jti",
        "fhirUser",
    ] {
        if let Some(v) = c.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    out.insert("token_type".into(), json!("Bearer"));
    // SMART: the launch context of the original token response.
    for k in [
        "patient",
        "encounter",
        "fhirContext",
        "need_patient_banner",
        "smart_style_url",
        "intent",
        "tenant",
    ] {
        if let Some(v) = c.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    http::json_no_store(Value::Object(out))
}

async fn introspect_refresh(
    s: &IdpServer,
    realm: &RealmRuntime,
    token: &str,
    caller: &str,
) -> Response {
    let Some(hash) = handle_hash(HandleKind::RefreshToken, token) else {
        return inactive();
    };
    let store = s.inner.rt.store();
    let Ok(Some((fam, generation))) = store.get_family_by_token(&realm.id, &hash).await else {
        return inactive();
    };
    let now = Utc::now();
    let idle = chrono::Duration::seconds(realm.spec.lifetimes.refresh_token_idle as i64);
    if fam.client_id != caller
        || fam.revoked_at.is_some()
        || generation != fam.current_generation
        || now >= fam.absolute_expiry
        || now >= fam.last_used_at + idle
    {
        return inactive();
    }
    match store.get_grant(&realm.id, &fam.grant_id).await {
        Ok(Some(g)) if g.revoked_at.is_none() => {}
        _ => return inactive(),
    }
    let mut out = serde_json::Map::new();
    out.insert("active".into(), json!(true));
    out.insert("scope".into(), json!(fam.scopes.join(" ")));
    out.insert("client_id".into(), json!(fam.client_id));
    out.insert("sub".into(), json!(fam.subject));
    out.insert(
        "exp".into(),
        json!(
            (fam.last_used_at + idle)
                .min(fam.absolute_expiry)
                .timestamp()
        ),
    );
    out.insert("iat".into(), json!(fam.last_used_at.timestamp()));
    out.insert("iss".into(), json!(realm.issuer()));
    out.insert("token_type".into(), json!("refresh_token"));
    if let Some(fu) = &fam.fhir_user {
        out.insert("fhirUser".into(), json!(fu));
    }
    let ctx: &LaunchContext = &fam.context;
    ctx.write_into(&mut out);
    http::json_no_store(Value::Object(out))
}

pub(crate) async fn revoke(
    State(s): State<IdpServer>,
    Path(realm_name): Path<String>,
    headers: HeaderMap,
    ext: axum::http::Extensions,
    body: Bytes,
) -> Response {
    let Some(realm) = http::realm(&s, &realm_name) else {
        return http::not_found();
    };
    if !realm.spec.features.revocation {
        return http::not_found();
    }
    let ip = http::client_ip(&ext);
    if !http::is_form(&headers) {
        return http::oauth_error(
            &OAuthError::invalid_request("application/x-www-form-urlencoded required"),
            realm.issuer(),
        );
    }
    let p = Params::parse(&body);
    let client = match client_auth::authenticate(
        &s,
        &realm,
        &headers,
        &p,
        &realm.urls.revocation,
        &ip,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => return http::oauth_error(&e, realm.issuer()),
    };
    if let Some(d) = p.first_dup() {
        return http::oauth_error(
            &OAuthError::invalid_request(format!("parameter {d} is repeated")),
            realm.issuer(),
        );
    }
    let Some(token) = p.get("token") else {
        return http::oauth_error(
            &OAuthError::invalid_request("token is required"),
            realm.issuer(),
        );
    };
    let store = s.inner.rt.store();
    let now = Utc::now();
    // Which grant does the token belong to — only if this client owns it.
    let gid: Option<String> = if token.starts_with(HandleKind::RefreshToken.prefix()) {
        match handle_hash(HandleKind::RefreshToken, token) {
            Some(h) => match store.get_family_by_token(&realm.id, &h).await {
                Ok(Some((f, _))) if f.client_id == client.spec.client_id => Some(f.grant_id),
                _ => None,
            },
            None => None,
        }
    } else {
        // Expired access tokens may still be revoked (their grant may live on).
        match realm.verify_own(token, Some("at+jwt")) {
            Ok(v)
                if v.claims.get("client_id").and_then(Value::as_str)
                    == Some(client.spec.client_id.as_str()) =>
            {
                v.claims
                    .get("gid")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }
            _ => None,
        }
    };
    if let Some(gid) = gid {
        match store
            .revoke_grant(&realm.id, &gid, "revoked by the client", now)
            .await
        {
            Ok(_) => {
                s.inner.rt.mark_revoked(realm.spec.name.as_str(), &gid);
                s.inner.rt.emit(IdpEvent::TokenRevoked {
                    realm: realm.spec.name.as_str().to_string(),
                    client_id: client.spec.client_id.clone(),
                    gid,
                    ip,
                });
            }
            Err(e) => {
                tracing::error!(error = %e, "idp revoke failed");
                return http::oauth_error(
                    &OAuthError::new(OAuthErrorCode::ServerError, "temporary failure"),
                    realm.issuer(),
                );
            }
        }
    }
    // RFC 7009 §2.2: 200 whether or not the token was valid.
    let mut r = StatusCode::OK.into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().expect("static"));
    r
}
