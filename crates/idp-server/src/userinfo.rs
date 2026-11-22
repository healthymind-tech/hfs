//! `GET|POST /idp/{realm}/userinfo` (OIDC Core §5.3). The access token comes
//! in the `Authorization: Bearer` header (RFC 6750 §2.1) or, for POST, the
//! form body (§2.2). Tokens in the query string are refused (RFC 6750 §2.3,
//! RFC 9700 §4.3.2).

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use helios_idp::OAuthErrorCode;
use serde_json::{Value, json};

use crate::IdpServer;
use crate::http::{self, Params};
use crate::introspect::active_access_token;

fn header_token(headers: &HeaderMap) -> Result<Option<String>, ()> {
    let Some(v) = headers.get(header::AUTHORIZATION) else {
        return Ok(None);
    };
    let v = v.to_str().map_err(|_| ())?;
    let (scheme, tok) = v.split_once(' ').ok_or(())?;
    if !scheme.eq_ignore_ascii_case("bearer") || tok.trim().is_empty() {
        return Err(());
    }
    Ok(Some(tok.trim().to_string()))
}

pub(crate) async fn userinfo_get(
    State(s): State<IdpServer>,
    Path(realm): Path<String>,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
) -> Response {
    let qp = Params::parse(q.unwrap_or_default().as_bytes());
    if qp.has("access_token") {
        return refuse_query(&s, &realm);
    }
    userinfo(s, realm, header_token(&headers), None).await
}

pub(crate) async fn userinfo_post(
    State(s): State<IdpServer>,
    Path(realm): Path<String>,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
    body: Bytes,
) -> Response {
    let qp = Params::parse(q.unwrap_or_default().as_bytes());
    if qp.has("access_token") {
        return refuse_query(&s, &realm);
    }
    let form = if http::is_form(&headers) {
        Params::parse(&body).get("access_token").map(str::to_string)
    } else {
        None
    };
    userinfo(s, realm, header_token(&headers), form).await
}

fn refuse_query(s: &IdpServer, realm: &str) -> Response {
    let iss = http::realm(s, realm)
        .map(|r| r.issuer().to_string())
        .unwrap_or_default();
    http::bearer_error(
        StatusCode::BAD_REQUEST,
        Some(OAuthErrorCode::InvalidRequest),
        "access tokens are not accepted in the query string",
        &iss,
    )
}

async fn userinfo(
    s: IdpServer,
    realm_name: String,
    header: Result<Option<String>, ()>,
    form: Option<String>,
) -> Response {
    let Some(realm) = http::realm(&s, &realm_name) else {
        return http::not_found();
    };
    if !realm.spec.features.openid_connect {
        return http::not_found();
    }
    let iss = realm.issuer().to_string();
    let token = match (header, form) {
        (Err(()), _) => {
            return http::bearer_error(
                StatusCode::BAD_REQUEST,
                Some(OAuthErrorCode::InvalidRequest),
                "malformed Authorization header",
                &iss,
            );
        }
        (Ok(Some(_)), Some(_)) => {
            return http::bearer_error(
                StatusCode::BAD_REQUEST,
                Some(OAuthErrorCode::InvalidRequest),
                "use one way of sending the access token",
                &iss,
            );
        }
        (Ok(Some(t)), None) | (Ok(None), Some(t)) => t,
        (Ok(None), None) => {
            // RFC 6750 §3.1: no error code when no credentials were sent.
            return http::bearer_error(StatusCode::UNAUTHORIZED, None, "", &iss);
        }
    };
    let Some(claims) = active_access_token(&s, &realm, &token).await else {
        return http::bearer_error(
            StatusCode::UNAUTHORIZED,
            Some(OAuthErrorCode::InvalidToken),
            "the access token is invalid, expired or revoked",
            &iss,
        );
    };
    let scopes: Vec<&str> = claims
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("")
        .split(' ')
        .collect();
    if !scopes.contains(&"openid") {
        return http::bearer_error(
            StatusCode::FORBIDDEN,
            Some(OAuthErrorCode::InsufficientScope),
            "the openid scope is required",
            &iss,
        );
    }
    let sub = claims.get("sub").and_then(Value::as_str).unwrap_or("");
    let mut out = serde_json::Map::new();
    out.insert("sub".into(), json!(sub));
    if scopes.contains(&"fhirUser")
        && let Some(fu) = claims.get("fhirUser")
    {
        out.insert("fhirUser".into(), fu.clone());
    }
    if (scopes.contains(&"profile") || scopes.contains(&"email"))
        && let Ok(Some(u)) = s.inner.rt.store().get_user(&realm.id, sub).await
    {
        if scopes.contains(&"profile") {
            out.insert(
                "name".into(),
                json!(u.display_name.clone().unwrap_or(u.username.clone())),
            );
            out.insert("preferred_username".into(), json!(u.username));
        }
        if scopes.contains(&"email")
            && let Some(e) = u.email
        {
            out.insert("email".into(), json!(e));
            out.insert("email_verified".into(), json!(u.email_verified));
        }
    }
    http::json_no_store(Value::Object(out))
}
