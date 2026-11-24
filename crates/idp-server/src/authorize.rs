//! `GET|POST /idp/{realm}/authorize` (RFC 6749 §4.1.1, SMART App Launch
//! 2.2 "Obtain authorization code" incl. `authorize-post`, OIDC Core
//! §3.1.2.1).
//!
//! Order of checks (RFC 6749 §4.1.2.1): until the client and the exact
//! redirect URI are verified, errors are shown on an error page and never
//! redirected. After that, errors go back to the client with `error`,
//! `state` and `iss` (RFC 9207).
//!
//! Refused on purpose (design D-15): `response_type` other than `code`
//! (implicit / hybrid → `unsupported_response_type`), PKCE `plain` or no
//! PKCE, `response_mode=fragment`, request objects (`request`,
//! `request_uri` → `request_not_supported` / `request_uri_not_supported`).

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{Duration, Utc};
use helios_idp::client::{GrantType, LaunchType};
use helios_idp::interaction::{
    AuthorizationResponse, InteractionNeeds, PendingAuthorization, Prompt, ResponseMode,
};
use helios_idp::launch::LaunchContext;
use helios_idp::runtime::RealmRuntime;
use helios_idp::scope;
use helios_idp::token::{HandleKind, handle_hash, mint_handle};
use helios_idp::{OAuthError, OAuthErrorCode, pkce};

use crate::IdpServer;
use crate::http::{self, Params};

pub(crate) async fn authorize_get(
    State(s): State<IdpServer>,
    Path(realm): Path<String>,
    RawQuery(q): RawQuery,
) -> Response {
    let p = Params::parse(q.unwrap_or_default().as_bytes());
    authorize(s, realm, p).await
}

pub(crate) async fn authorize_post(
    State(s): State<IdpServer>,
    Path(realm): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !http::is_form(&headers) {
        return http::error_page(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            &OAuthError::invalid_request(
                "authorize-post requires application/x-www-form-urlencoded",
            ),
        );
    }
    authorize(s, realm, Params::parse(&body)).await
}

/// Response to the user agent for an error after the redirect URI is trusted.
pub(crate) fn respond(resp: &AuthorizationResponse) -> Response {
    match resp.mode {
        ResponseMode::Query => {
            let mut r = Redirect::to(&resp.redirect_url()).into_response();
            r.headers_mut()
                .insert(header::CACHE_CONTROL, "no-store".parse().expect("static"));
            r
        }
        ResponseMode::FormPost => {
            let nonce = helios_idp::util::random_b64(16);
            let mut r = (StatusCode::OK, resp.form_post_html(&nonce)).into_response();
            let h = r.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                "text/html; charset=utf-8".parse().expect("static"),
            );
            if let Ok(v) = resp.form_post_csp(&nonce).parse() {
                h.insert(header::CONTENT_SECURITY_POLICY, v);
            }
            h.insert(header::CACHE_CONTROL, "no-store".parse().expect("static"));
            r
        }
    }
}

/// Drops scopes the realm does not offer (the server may grant less than
/// requested; SMART "scope … may differ").
fn realm_allows(realm: &RealmRuntime, tok: &str) -> bool {
    // system/ scopes only through client_credentials.
    helios_idp::scope_catalog::realm_offers(&realm.spec.features, tok, false)
}

fn parse_prompt(v: Option<&str>) -> Result<Vec<Prompt>, OAuthError> {
    let mut out = vec![];
    for t in v.unwrap_or("").split(' ').filter(|t| !t.is_empty()) {
        let p = match t {
            "none" => Prompt::None,
            "login" => Prompt::Login,
            "consent" => Prompt::Consent,
            "select_account" => Prompt::SelectAccount,
            _ => {
                return Err(OAuthError::invalid_request(format!(
                    "unsupported prompt value {t:?}"
                )));
            }
        };
        if !out.contains(&p) {
            out.push(p);
        }
    }
    if out.contains(&Prompt::None) && out.len() > 1 {
        return Err(OAuthError::invalid_request(
            "prompt=none cannot be combined",
        ));
    }
    Ok(out)
}

async fn authorize(s: IdpServer, realm_name: String, p: Params) -> Response {
    let Some(realm) = http::realm(&s, &realm_name) else {
        return http::error_page(
            StatusCode::NOT_FOUND,
            &OAuthError::invalid_request("unknown authorization server"),
        );
    };
    let issuer = realm.issuer().to_string();

    // ---- Phase 1: client and redirect URI (never redirect on failure).
    if p.is_dup("client_id") || p.is_dup("redirect_uri") {
        return http::error_page(
            StatusCode::BAD_REQUEST,
            &OAuthError::invalid_request("client_id and redirect_uri must appear once"),
        );
    }
    let Some(client_id) = p.get("client_id") else {
        return http::error_page(
            StatusCode::BAD_REQUEST,
            &OAuthError::invalid_request("client_id is required"),
        );
    };
    let client = match s.inner.rt.store().get_client(&realm.id, client_id).await {
        Ok(Some(c)) if !c.disabled && client_id.len() <= 128 => c,
        Ok(_) => {
            return http::error_page(
                StatusCode::BAD_REQUEST,
                &OAuthError::invalid_request("unknown client"),
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "idp store error");
            return http::error_page(
                StatusCode::SERVICE_UNAVAILABLE,
                &OAuthError::new(OAuthErrorCode::TemporarilyUnavailable, "try again later"),
            );
        }
    };
    let Some(redirect_uri) = p.get("redirect_uri") else {
        return http::error_page(
            StatusCode::BAD_REQUEST,
            &OAuthError::invalid_request("redirect_uri is required"),
        );
    };
    if !client.redirect_uri_allowed(redirect_uri) {
        return http::error_page(
            StatusCode::BAD_REQUEST,
            &OAuthError::invalid_request("redirect_uri is not registered for this client"),
        );
    }

    // ---- Phase 2: errors are redirected to the verified redirect URI.
    let state = p.get("state").map(str::to_string);
    let (mode, mode_err) = match p.get("response_mode") {
        None | Some("query") => (ResponseMode::Query, None),
        Some("form_post") => (ResponseMode::FormPost, None),
        Some(other) => (
            ResponseMode::Query,
            Some(OAuthError::invalid_request(format!(
                "response_mode {other:?} is not supported (query, form_post)"
            ))),
        ),
    };
    let fail = |e: OAuthError| {
        respond(&AuthorizationResponse::error(
            redirect_uri,
            mode,
            &e,
            state.as_deref(),
            &issuer,
        ))
    };
    if let Some(e) = mode_err {
        return fail(e);
    }
    if let Some(d) = p.first_dup() {
        return fail(OAuthError::invalid_request(format!(
            "parameter {d} is repeated"
        )));
    }
    if p.has("request") {
        return fail(OAuthError::new(
            OAuthErrorCode::RequestNotSupported,
            "request objects are not supported",
        ));
    }
    if p.has("request_uri") {
        return fail(OAuthError::new(
            OAuthErrorCode::RequestUriNotSupported,
            "request_uri is not supported",
        ));
    }
    match p.get("response_type") {
        None => return fail(OAuthError::invalid_request("response_type is required")),
        Some("code") => {}
        Some(_) => {
            return fail(OAuthError::new(
                OAuthErrorCode::UnsupportedResponseType,
                "only response_type=code is supported",
            ));
        }
    }
    if !client.grant_types.contains(&GrantType::AuthorizationCode) {
        return fail(OAuthError::new(
            OAuthErrorCode::UnauthorizedClient,
            "the client may not use the authorization code grant",
        ));
    }
    if let Err(e) = pkce::check_challenge(p.get("code_challenge"), p.get("code_challenge_method")) {
        return fail(e);
    }
    let prompt = match parse_prompt(p.get("prompt")) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let max_age = match p.get("max_age").map(str::parse::<u64>) {
        None => None,
        Some(Ok(v)) => Some(v),
        Some(Err(_)) => {
            return fail(OAuthError::invalid_request(
                "max_age must be a number of seconds",
            ));
        }
    };
    if p.get("nonce").is_some_and(|n| n.len() > 512)
        || p.get("state").is_some_and(|n| n.len() > 2048)
        || p.get("login_hint").is_some_and(|n| n.len() > 256)
    {
        return fail(OAuthError::invalid_request("parameter too long"));
    }

    // Audience: SMART `aud` (required for SMART launches); RFC 8707
    // `resource` as a synonym. A pure OpenID Connect sign-in (only
    // identity scopes: openid, profile, email, offline_access) needs no FHIR
    // audience; its access token is then only good for userinfo.
    let aud = match (p.get("aud"), p.get("resource")) {
        (Some(a), Some(r)) if a.trim_end_matches('/') != r.trim_end_matches('/') => {
            return fail(OAuthError::invalid_request("aud and resource differ"));
        }
        (Some(a), _) => Some(a),
        (None, Some(r)) => Some(r),
        (None, None) => None,
    };
    let identity_only = p.get("scope").is_some_and(|sc| {
        let toks = scope::split_scopes(sc);
        toks.iter().any(|t| t == "openid")
            && toks.iter().all(|t| {
                matches!(
                    t.as_str(),
                    "openid" | "profile" | "email" | "offline_access"
                )
            })
    });
    let (endpoint, aud_url) = match aud {
        Some(a) => match realm.endpoint_for_aud(a).cloned() {
            Some(e) => e,
            None => {
                return fail(OAuthError::invalid_request(
                    "aud is not a FHIR endpoint served by this authorization server",
                ));
            }
        },
        None if identity_only && p.get("launch").is_none() => (String::new(), issuer.clone()),
        None => return fail(OAuthError::invalid_request("aud is required")),
    };

    // Scopes: requested ∩ realm features ∩ client policy.
    let Some(scope_param) = p.get("scope") else {
        return fail(OAuthError::invalid_scope("scope is required"));
    };
    let requested = scope::split_scopes(scope_param);
    let offered: Vec<String> = requested
        .iter()
        .filter(|t| realm_allows(&realm, t))
        .cloned()
        .collect();
    let mut g = scope::grant(&offered, &client.scopes);
    g.dropped
        .extend(requested.iter().filter(|t| !offered.contains(t)).cloned());

    // EHR launch: `launch` handle, consumed once, bound to this client/aud.
    let mut launch_context = LaunchContext::default();
    let mut launched_by = None;
    let ehr = p.get("launch");
    if let Some(handle) = ehr {
        if !realm.spec.features.launch_ehr || !client.launch_types.contains(&LaunchType::Ehr) {
            return fail(OAuthError::new(
                OAuthErrorCode::UnauthorizedClient,
                "the client may not use EHR launch",
            ));
        }
        if !g.has("launch") {
            return fail(OAuthError::invalid_scope(
                "an EHR launch requires the launch scope",
            ));
        }
        let Some(hash) = handle_hash(HandleKind::LaunchHandle, handle) else {
            return fail(OAuthError::invalid_request("launch is invalid or expired"));
        };
        let h = match s
            .inner
            .rt
            .store()
            .take_launch(&realm.id, &hash, Utc::now())
            .await
        {
            Ok(Some(h)) => h,
            Ok(None) => return fail(OAuthError::invalid_request("launch is invalid or expired")),
            Err(e) => {
                tracing::error!(error = %e, "idp store error");
                return fail(OAuthError::new(
                    OAuthErrorCode::TemporarilyUnavailable,
                    "try again later",
                ));
            }
        };
        if h.client_id
            .as_deref()
            .is_some_and(|c| c != client.client_id)
        {
            return fail(OAuthError::invalid_request(
                "launch was created for another app",
            ));
        }
        if h.endpoint != endpoint {
            return fail(OAuthError::invalid_request(
                "launch is for another FHIR endpoint",
            ));
        }
        launch_context = h.context;
        launched_by = h.launched_by;
    } else {
        let standalone_ok =
            client.launch_types.is_empty() || client.launch_types.contains(&LaunchType::Standalone);
        if !realm.spec.features.launch_standalone || !standalone_ok {
            return fail(OAuthError::new(
                OAuthErrorCode::UnauthorizedClient,
                "the client may not use standalone launch",
            ));
        }
        // `launch` without a launch parameter means nothing: not granted.
        if g.has("launch") {
            g.granted.retain(|t| t != "launch");
            g.dropped.push("launch".into());
        }
    }
    if g.granted.is_empty() {
        return fail(OAuthError::invalid_scope(
            "none of the requested scopes may be granted to this client",
        ));
    }

    let needs = InteractionNeeds {
        patient_picker: g.has("launch/patient") && launch_context.patient.is_none(),
        encounter_picker: g.has("launch/encounter") && launch_context.encounter.is_none(),
        consent: !client.trusted || prompt.contains(&Prompt::Consent),
    };
    let handle = mint_handle(HandleKind::Interaction);
    let now = Utc::now();
    let pending = PendingAuthorization {
        id_hash: handle.hash.clone(),
        realm_id: realm.id.clone(),
        realm_name: realm.spec.name.as_str().to_string(),
        client_id: client.client_id.clone(),
        client_name: client.name.clone(),
        redirect_uri: redirect_uri.to_string(),
        response_mode: mode,
        state: state.clone(),
        nonce: p.get("nonce").map(str::to_string),
        code_challenge: p.get("code_challenge").unwrap_or_default().to_string(),
        scopes: g.granted.clone(),
        dropped_scopes: g.dropped.clone(),
        endpoint,
        audience: aud_url,
        prompt,
        max_age,
        login_hint: p.get("login_hint").map(str::to_string),
        ui_locales: p
            .get("ui_locales")
            .map(|v| {
                v.split(' ')
                    .filter(|t| !t.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        launch_context,
        ehr_launch: ehr.is_some(),
        launched_by,
        needs,
        progress: serde_json::json!({}),
        created_at: now,
        expires_at: now + Duration::seconds(s.inner.interaction_ttl_secs as i64),
    };
    if let Err(e) = s.inner.rt.store().put_interaction(pending).await {
        tracing::error!(error = %e, "idp store error");
        return fail(OAuthError::new(
            OAuthErrorCode::TemporarilyUnavailable,
            "try again later",
        ));
    }
    let mut login = url::Url::parse(&format!("{issuer}/login")).expect("issuer is a URL");
    login.query_pairs_mut().append_pair("ix", &handle.value);
    let mut r = Redirect::to(login.as_str()).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().expect("static"));
    r
}
