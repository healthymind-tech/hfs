//! Integration tests of the protocol endpoints (design §11.3 steps 1–4,
//! 6–10, §13 compliance matrix). Every grant, every error response.

mod common;

use axum::http::StatusCode;
use common::*;
use helios_idp::events::IdpEvent;
use helios_idp::interaction::CompleteError;
use helios_idp::keys::{RsaBits, SigningAlg, SigningKey};
use helios_idp::{OAuthError, OAuthErrorCode};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::Jwk};
use serde_json::{Value, json};

fn verify_jwt(jwks: &Value, token: &str, aud: &str) -> Value {
    let h = decode_header(token).unwrap();
    let kid = h.kid.clone().unwrap();
    let jwk = jwks["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["kid"] == kid.as_str())
        .unwrap();
    let jwk: Jwk = serde_json::from_value(jwk.clone()).unwrap();
    let mut v = Validation::new(h.alg);
    v.set_audience(&[aud]);
    v.set_issuer(&[ISS]);
    decode::<Value>(token, &DecodingKey::from_jwk(&jwk).unwrap(), &v)
        .unwrap()
        .claims
}

// ------------------------------------------------------------- discovery

#[tokio::test(flavor = "multi_thread")]
async fn discovery_documents_and_jwks() {
    let h = Harness::new().await;
    let a = h.get("/idp/clinic/.well-known/openid-configuration").await;
    assert_eq!(a.status, StatusCode::OK);
    let m = a.json();
    assert_eq!(m["issuer"], ISS);
    assert_eq!(m["authorization_endpoint"], format!("{ISS}/authorize"));
    assert_eq!(m["token_endpoint"], format!("{ISS}/token"));
    assert_eq!(m["jwks_uri"], format!("{ISS}/jwks"));
    assert_eq!(m["userinfo_endpoint"], format!("{ISS}/userinfo"));
    assert_eq!(m["introspection_endpoint"], format!("{ISS}/introspect"));
    assert_eq!(m["revocation_endpoint"], format!("{ISS}/revoke"));
    assert_eq!(m["end_session_endpoint"], format!("{ISS}/logout"));
    assert_eq!(m["response_types_supported"], json!(["code"]));
    assert_eq!(m["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(m["subject_types_supported"], json!(["public"]));
    assert_eq!(m["id_token_signing_alg_values_supported"], json!(["RS256"]));
    assert_eq!(m["authorization_response_iss_parameter_supported"], true);
    let grants = m["grant_types_supported"].as_array().unwrap();
    assert!(!grants.contains(&json!("password")) && !grants.contains(&json!("implicit")));
    // RFC 8414 path insertion: same document.
    let b = h
        .get("/.well-known/oauth-authorization-server/idp/clinic")
        .await;
    assert_eq!(b.json(), m);
    assert!(a.header("cache-control").contains("max-age"));

    // SMART configuration of the bound endpoint (SMART 2.2 "Conformance").
    let rt = h.rt.realm("clinic").unwrap();
    let s = helios_idp_server::smart_configuration(&rt);
    for k in [
        "issuer",
        "jwks_uri",
        "authorization_endpoint",
        "token_endpoint",
        "grant_types_supported",
        "token_endpoint_auth_methods_supported",
        "token_endpoint_auth_signing_alg_values_supported",
        "scopes_supported",
        "response_types_supported",
        "capabilities",
        "code_challenge_methods_supported",
        "introspection_endpoint",
        "revocation_endpoint",
        "management_endpoint",
    ] {
        assert!(s.get(k).is_some(), "smart-configuration lacks {k}");
    }
    assert_eq!(s["code_challenge_methods_supported"], json!(["S256"]));
    let caps = s["capabilities"].as_array().unwrap();
    for c in [
        "launch-ehr",
        "launch-standalone",
        "authorize-post",
        "client-public",
        "client-confidential-symmetric",
        "client-confidential-asymmetric",
        "sso-openid-connect",
        "context-banner",
        "context-ehr-patient",
        "context-ehr-encounter",
        "context-standalone-patient",
        "context-standalone-encounter",
        "permission-offline",
        "permission-patient",
        "permission-user",
        "permission-v1",
        "permission-v2",
    ] {
        assert!(caps.contains(&json!(c)), "capability {c}");
    }
    let algs = s["token_endpoint_auth_signing_alg_values_supported"]
        .as_array()
        .unwrap();
    assert!(algs.contains(&json!("RS384")) && algs.contains(&json!("ES384")));

    // JWKS: public members only, kid = RFC 7638 thumbprint, RS256 + ES256.
    let j = h.get("/idp/clinic/jwks").await;
    assert_eq!(j.status, StatusCode::OK);
    assert_eq!(j.header("content-type"), "application/jwk-set+json");
    assert!(j.header("cache-control").contains("max-age=300"));
    let keys = j.json()["keys"].as_array().unwrap().clone();
    assert_eq!(keys.len(), 2);
    for k in &keys {
        for private in ["d", "p", "q", "dp", "dq", "qi"] {
            assert!(k.get(private).is_none(), "private member {private}");
        }
        assert_eq!(k["kid"].as_str().unwrap(), helios_idp::keys::thumbprint(k));
        assert_eq!(k["use"], "sig");
    }
    let algs: Vec<&str> = keys.iter().map(|k| k["alg"].as_str().unwrap()).collect();
    assert!(algs.contains(&"RS256") && algs.contains(&"ES256"));
    // CORS for browser apps.
    assert_eq!(j.header("access-control-allow-origin"), "*");

    // Unknown realm.
    assert_eq!(
        h.get("/idp/nope/.well-known/openid-configuration")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(h.get("/idp/nope/jwks").await.status, StatusCode::NOT_FOUND);
}

// ------------------------------------------------------------- standalone launch

#[tokio::test(flavor = "multi_thread")]
async fn standalone_launch_full_flow_with_openid_and_patient() {
    let h = Harness::new().await;
    let scope = "openid fhirUser profile launch/patient offline_access patient/*.rs";
    let ix = h.start(&h.authz_params("app", scope)).await;
    // The pending authorization the pages see.
    let rt = h.rt.realm("clinic").unwrap();
    let hash =
        helios_idp::token::handle_hash(helios_idp::token::HandleKind::Interaction, &ix).unwrap();
    let pending =
        h.rt.store()
            .get_interaction(&rt.id, &hash, chrono::Utc::now())
            .await
            .unwrap()
            .unwrap();
    assert!(pending.needs.patient_picker && pending.needs.consent);
    assert_eq!(pending.audience, AUD);
    assert_eq!(pending.endpoint, "clinic");

    let scopes: Vec<&str> = scope.split(' ').collect();
    let resp = h
        .complete(&ix, h.decision("alice", &scopes, Some("p1")))
        .await;
    // RFC 9207 iss and state echo.
    assert_eq!(resp.param("state"), Some("st-123"));
    assert_eq!(resp.param("iss"), Some(ISS));
    let url = url::Url::parse(&resp.redirect_url()).unwrap();
    assert_eq!(
        format!(
            "{}://{}{}",
            url.scheme(),
            url.host_str().unwrap(),
            url.path()
        ),
        REDIRECT
    );
    let code = resp.param("code").unwrap().to_string();
    assert!(code.starts_with("hmac_"));
    // Interaction is single use.
    assert_eq!(
        h.server
            .completer()
            .complete("clinic", &ix, h.decision("alice", &scopes, Some("p1")))
            .await
            .unwrap_err(),
        CompleteError::UnknownInteraction
    );

    let r = h
        .token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT),
                ("code_verifier", VERIFIER),
                ("client_id", "app"),
            ],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(r.header("cache-control"), "no-store");
    assert_eq!(r.header("pragma"), "no-cache");
    let t = r.json();
    assert_eq!(t["token_type"], "Bearer");
    assert_eq!(t["expires_in"], 3600);
    assert_eq!(t["scope"], scope);
    assert_eq!(t["patient"], "p1");
    assert_eq!(t["need_patient_banner"], true);
    assert!(t["refresh_token"].as_str().unwrap().starts_with("hmrt_"));

    let jwks = h.get("/idp/clinic/jwks").await.json();
    // RFC 9068 access token, verified by the resource server's library.
    let at = t["access_token"].as_str().unwrap();
    assert_eq!(decode_header(at).unwrap().typ.as_deref(), Some("at+jwt"));
    let c = verify_jwt(&jwks, at, AUD);
    assert_eq!(c["sub"], "alice");
    assert_eq!(c["client_id"], "app");
    assert_eq!(c["patient"], "p1");
    assert_eq!(c["fhirUser"], format!("{AUD}/Patient/p1"));
    assert!(c["gid"].as_str().unwrap().starts_with("g_"));
    assert!(c["jti"].is_string());

    // id_token: RS256, nonce, at_hash, fhirUser, name.
    let id = t["id_token"].as_str().unwrap();
    assert_eq!(decode_header(id).unwrap().alg, Algorithm::RS256);
    let ic = verify_jwt(&jwks, id, "app");
    assert_eq!(ic["nonce"], "n-456");
    assert_eq!(ic["sub"], "alice");
    assert_eq!(ic["sid"], "sid-1");
    assert_eq!(ic["fhirUser"], format!("{AUD}/Patient/p1"));
    assert_eq!(ic["name"], "Alice Example");
    assert_eq!(
        ic["at_hash"],
        helios_idp::token::at_hash(at, SigningAlg::RS256)
    );
    assert!(ic["auth_time"].is_i64());

    let events = h.events.take();
    assert!(events.iter().any(|e| matches!(e, IdpEvent::TokenIssued { grant_type, .. } if grant_type == "authorization_code")));
}

#[tokio::test(flavor = "multi_thread")]
async fn authorize_post_and_form_post_response_mode() {
    let h = Harness::new().await;
    let mut p = h.authz_params("app", "launch/patient patient/*.rs");
    p.push(("response_mode".into(), "form_post".into()));
    let body: Vec<(&str, &str)> = p.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let r = h.post("/idp/clinic/authorize", &body, &[]).await;
    assert_eq!(
        r.status,
        StatusCode::SEE_OTHER,
        "authorize-post: {}",
        r.text()
    );
    let ix = url::Url::parse(r.location())
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "ix")
        .unwrap()
        .1
        .into_owned();
    let resp = h
        .complete(
            &ix,
            h.decision("alice", &["launch/patient", "patient/*.rs"], Some("child1")),
        )
        .await;
    assert_eq!(resp.mode, helios_idp::interaction::ResponseMode::FormPost);
    let html = resp.form_post_html("nonce123");
    assert!(html.contains("action=\"https://app.example/cb\""));
    assert!(html.contains("name=\"code\""));
    assert!(html.contains("name=\"iss\""));

    // authorize-post needs a form body.
    let r = h
        .send(
            axum::http::Request::post("/idp/clinic/authorize")
                .header("content-type", "application/json")
                .body(axum::body::Body::from("{}"))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    // Errors in form_post mode are posted back, with state and iss.
    let mut bad = h.authz_params("app", "patient/*.rs");
    bad.push(("response_mode".into(), "form_post".into()));
    bad.retain(|(k, _)| k != "code_challenge");
    let r = h.authorize(&bad).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.header("content-security-policy")
            .contains("form-action https://app.example")
    );
    let t = r.text();
    assert!(t.contains("value=\"invalid_request\"") && t.contains("value=\"st-123\""));
}

// ------------------------------------------------------------- authorize errors

#[tokio::test(flavor = "multi_thread")]
async fn authorize_errors_before_redirect_are_pages() {
    let h = Harness::new().await;
    // Unknown client: page, no redirect.
    let r = h.authorize(&h.authz_params("nobody", "patient/*.rs")).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.location().is_empty());
    assert!(r.text().contains("unknown client"));
    assert!(
        r.header("content-security-policy")
            .contains("default-src 'none'")
    );
    // Unregistered redirect URI (also a near miss): page, no redirect.
    for uri in [
        "https://evil.example/cb",
        "https://app.example/cb/",
        "https://app.example/cb?x=1",
    ] {
        let mut p = h.authz_params("app", "patient/*.rs");
        p.iter_mut().find(|(k, _)| k == "redirect_uri").unwrap().1 = uri.into();
        let r = h.authorize(&p).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(r.location().is_empty(), "{uri}");
    }
    // Missing redirect_uri, duplicate client_id.
    let mut p = h.authz_params("app", "patient/*.rs");
    p.retain(|(k, _)| k != "redirect_uri");
    assert!(h.authorize(&p).await.location().is_empty());
    let mut p = h.authz_params("app", "patient/*.rs");
    p.push(("client_id".into(), "app".into()));
    assert_eq!(h.authorize(&p).await.status, StatusCode::BAD_REQUEST);
    // Loopback redirect with any port is fine (RFC 8252 §7.3).
    let mut p = h.authz_params("app", "patient/*.rs");
    p.iter_mut().find(|(k, _)| k == "redirect_uri").unwrap().1 = "http://127.0.0.1:53111/cb".into();
    assert_eq!(h.authorize(&p).await.status, StatusCode::SEE_OTHER);
    // Unknown realm.
    let r = h.get("/idp/nope/authorize?client_id=app").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

async fn redirect_error(
    h: &Harness,
    p: &[(String, String)],
) -> (String, std::collections::HashMap<String, String>) {
    let r = h.authorize(p).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.text());
    let q = r.redirect_params();
    assert!(r.location().starts_with(REDIRECT));
    assert_eq!(
        q.get("state").map(String::as_str),
        Some("st-123"),
        "state echo"
    );
    assert_eq!(q.get("iss").map(String::as_str), Some(ISS), "RFC 9207 iss");
    assert!(q.get("code").is_none());
    (q.get("error").cloned().unwrap_or_default(), q)
}

#[tokio::test(flavor = "multi_thread")]
async fn authorize_errors_after_redirect_check_are_redirected() {
    let h = Harness::new().await;
    let base = h.authz_params("app", "launch/patient patient/*.rs");
    let with = |k: &str, v: Option<&str>| {
        let mut p = base.clone();
        p.retain(|(a, _)| a != k);
        if let Some(v) = v {
            p.push((k.into(), v.into()));
        }
        p
    };
    let cases: Vec<(Vec<(String, String)>, &str)> = vec![
        // Implicit / hybrid refused (OAuth 2.1 §10.1, SMART 2).
        (
            with("response_type", Some("token")),
            "unsupported_response_type",
        ),
        (
            with("response_type", Some("code id_token")),
            "unsupported_response_type",
        ),
        (with("response_type", None), "invalid_request"),
        // PKCE S256 required; plain refused (SMART 2.2).
        (with("code_challenge", None), "invalid_request"),
        (
            with("code_challenge_method", Some("plain")),
            "invalid_request",
        ),
        (with("code_challenge_method", None), "invalid_request"),
        // aud required and must be a bound endpoint.
        (with("aud", None), "invalid_request"),
        (
            with("aud", Some("https://evil.example/fhir")),
            "invalid_request",
        ),
        (
            with("aud", Some("https://hfs.test/fhir/other")),
            "invalid_request",
        ),
        // scope.
        (with("scope", None), "invalid_scope"),
        (with("scope", Some("system/*.rs junk")), "invalid_scope"),
        // Unsupported request forms.
        (with("response_mode", Some("fragment")), "invalid_request"),
        (with("request", Some("eyJ.x.y")), "request_not_supported"),
        (
            with("request_uri", Some("https://x")),
            "request_uri_not_supported",
        ),
        (with("prompt", Some("none login")), "invalid_request"),
        (with("prompt", Some("bogus")), "invalid_request"),
        (with("max_age", Some("soon")), "invalid_request"),
        // Bad launch handle.
        (
            {
                let mut p = with("scope", Some("launch patient/*.rs"));
                p.push((
                    "launch".into(),
                    "hmlh_doesnotexistdoesnotexistdoesnotexist12".into(),
                ));
                p
            },
            "invalid_request",
        ),
    ];
    for (p, want) in cases {
        let (err, q) = redirect_error(&h, &p).await;
        assert_eq!(err, want, "{p:?} → {q:?}");
    }
    // Repeated parameter.
    let mut p = base.clone();
    p.push(("scope".into(), "openid".into()));
    assert_eq!(redirect_error(&h, &p).await.0, "invalid_request");
    // A client without the code grant.
    let mut p = h.authz_params("backend", "system/*.rs");
    p.retain(|(k, _)| k != "redirect_uri");
    assert!(
        h.authorize(&p).await.location().is_empty(),
        "no redirect URI registered → page"
    );
    // resource is a synonym of aud.
    let p = with("aud", None)
        .into_iter()
        .chain([("resource".to_string(), AUD.to_string())])
        .collect::<Vec<_>>();
    assert_eq!(h.authorize(&p).await.status, StatusCode::SEE_OTHER);
}

#[tokio::test(flavor = "multi_thread")]
async fn scope_policy_and_realm_features_narrow_the_request() {
    let h = Harness::new().await;
    // system/ never at /authorize; unknown and unallowed scopes are dropped.
    let ix = h
        .start(&h.authz_params(
            "app",
            "patient/Observation.rs?category=laboratory patient/Patient.read system/*.rs junk launch",
        ))
        .await;
    let rt = h.rt.realm("clinic").unwrap();
    let hash =
        helios_idp::token::handle_hash(helios_idp::token::HandleKind::Interaction, &ix).unwrap();
    let p =
        h.rt.store()
            .get_interaction(&rt.id, &hash, chrono::Utc::now())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        p.scopes,
        vec![
            "patient/Observation.rs?category=laboratory",
            "patient/Patient.read"
        ]
    );
    assert!(p.dropped_scopes.contains(&"system/*.rs".to_string()));
    assert!(
        p.dropped_scopes.contains(&"launch".to_string()),
        "launch without a launch parameter"
    );
    // The decision cannot add scopes.
    let err = h
        .server
        .completer()
        .complete(
            "clinic",
            &ix,
            h.decision("alice", &["patient/*.cruds"], None),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CompleteError::InvalidDecision(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn completer_validates_patient_and_denies_with_redirect() {
    let h = Harness::new().await;
    // Patient user may only pick from their own set.
    let ix = h
        .start(&h.authz_params("app", "launch/patient patient/*.rs"))
        .await;
    let err = h
        .server
        .completer()
        .complete(
            "clinic",
            &ix,
            h.decision(
                "alice",
                &["launch/patient", "patient/*.rs"],
                Some("stranger"),
            ),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CompleteError::InvalidDecision(_)));
    // launch/patient approved but no patient.
    let ix = h
        .start(&h.authz_params("app", "launch/patient patient/*.rs"))
        .await;
    assert!(matches!(
        h.server
            .completer()
            .complete(
                "clinic",
                &ix,
                h.decision("alice", &["launch/patient", "patient/*.rs"], None)
            )
            .await,
        Err(CompleteError::InvalidDecision(_))
    ));
    // prompt=none without a session → the pages deny with login_required.
    let mut p = h.authz_params("app", "patient/*.rs");
    p.push(("prompt".into(), "none".into()));
    let ix = h.start(&p).await;
    let resp = h
        .server
        .completer()
        .deny(
            "clinic",
            &ix,
            OAuthError::new(OAuthErrorCode::LoginRequired, "no session"),
        )
        .await
        .unwrap();
    assert_eq!(resp.param("error"), Some("login_required"));
    assert_eq!(resp.param("state"), Some("st-123"));
    assert_eq!(resp.param("iss"), Some(ISS));
    // User declines.
    let ix = h.start(&h.authz_params("app", "patient/*.rs")).await;
    let resp = h
        .server
        .completer()
        .deny("clinic", &ix, OAuthError::invalid_client("weird"))
        .await
        .unwrap();
    assert_eq!(
        resp.param("error"),
        Some("access_denied"),
        "only interaction errors pass through"
    );
    // max_age.
    let mut p = h.authz_params("app", "patient/*.rs");
    p.push(("max_age".into(), "60".into()));
    let ix = h.start(&p).await;
    let mut d = h.decision("alice", &["patient/*.rs"], None);
    d.auth_time = chrono::Utc::now() - chrono::Duration::seconds(600);
    assert!(matches!(
        h.server.completer().complete("clinic", &ix, d).await,
        Err(CompleteError::InvalidDecision(_))
    ));
    // Unknown handle.
    assert_eq!(
        h.server
            .completer()
            .complete(
                "clinic",
                "hmix_x",
                h.decision("alice", &["patient/*.rs"], None)
            )
            .await
            .unwrap_err(),
        CompleteError::UnknownInteraction
    );
}

// ------------------------------------------------------------- token errors

#[tokio::test(flavor = "multi_thread")]
async fn token_code_errors_pkce_replay_and_binding() {
    let h = Harness::new().await;
    async fn code_for(h: &Harness) -> String {
        let ix = h
            .start(&h.authz_params("app", "patient/*.rs offline_access"))
            .await;
        h.complete(
            &ix,
            h.decision("alice", &["patient/*.rs", "offline_access"], None),
        )
        .await
        .param("code")
        .unwrap()
        .to_string()
    }
    // Wrong PKCE verifier → invalid_grant and the code's grant is revoked.
    let code = code_for(&h).await;
    let r = h
        .token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT),
                ("code_verifier", &"x".repeat(43)),
                ("client_id", "app"),
            ],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["error"], "invalid_grant");
    assert_eq!(r.header("cache-control"), "no-store");
    // Missing verifier.
    let code = code_for(&h).await;
    let r = h
        .token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT),
                ("client_id", "app"),
            ],
            None,
        )
        .await;
    assert_eq!(r.json()["error"], "invalid_grant");
    // redirect_uri mismatch / missing.
    let code = code_for(&h).await;
    let r = h
        .token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", "http://127.0.0.1/cb"),
                ("code_verifier", VERIFIER),
                ("client_id", "app"),
            ],
            None,
        )
        .await;
    assert_eq!(r.json()["error"], "invalid_grant");
    let r = h
        .token(
            &[
                ("grant_type", "authorization_code"),
                ("code", "hmac_x"),
                ("code_verifier", VERIFIER),
                ("client_id", "app"),
            ],
            None,
        )
        .await;
    assert_eq!(r.json()["error"], "invalid_request");
    // Code issued to app, redeemed by conf.
    let code = code_for(&h).await;
    let r = h
        .token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT),
                ("code_verifier", VERIFIER),
            ],
            Some(("conf", &h.secret)),
        )
        .await;
    assert_eq!(r.json()["error"], "invalid_grant");
    // Garbage code.
    let r = h
        .token(
            &[
                ("grant_type", "authorization_code"),
                ("code", "not-a-code"),
                ("redirect_uri", REDIRECT),
                ("code_verifier", VERIFIER),
                ("client_id", "app"),
            ],
            None,
        )
        .await;
    assert_eq!(r.json()["error"], "invalid_grant");

    // Replay: second redemption fails and revokes the tokens of the first.
    let code = code_for(&h).await;
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", REDIRECT),
        ("code_verifier", VERIFIER),
        ("client_id", "app"),
    ];
    let first = h.token(&form, None).await.json();
    let at = first["access_token"].as_str().unwrap().to_string();
    let rt_tok = first["refresh_token"].as_str().unwrap().to_string();
    assert_eq!(h.introspect(&at).await["active"], true);
    let r = h.token(&form, None).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["error"], "invalid_grant");
    assert_eq!(
        h.introspect(&at).await["active"],
        false,
        "replay revoked the grant"
    );
    let r = h
        .token(
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &rt_tok),
                ("client_id", "app"),
            ],
            None,
        )
        .await;
    assert_eq!(
        r.json()["error"],
        "invalid_grant",
        "refresh family revoked too"
    );
    let gid = verify_jwt(&h.get("/idp/clinic/jwks").await.json(), &at, AUD)["gid"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        h.rt.realm("clinic").unwrap().grant_revoked(&gid),
        "deny set updated at once"
    );
    assert!(
        h.events
            .take()
            .iter()
            .any(|e| matches!(e, IdpEvent::CodeReplay { .. }))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn token_endpoint_error_codes() {
    let h = Harness::new().await;
    // Password and implicit-style grants are refused.
    for gt in [
        "password",
        "implicit",
        "urn:ietf:params:oauth:grant-type:device_code",
        "bogus",
    ] {
        let r = h
            .token(
                &[
                    ("grant_type", gt),
                    ("username", "alice"),
                    ("password", "x"),
                    ("client_id", "app"),
                ],
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{gt}");
        assert_eq!(r.json()["error"], "unsupported_grant_type", "{gt}");
    }
    let r = h.token(&[("client_id", "app")], None).await;
    assert_eq!(r.json()["error"], "invalid_request");
    // Grant not registered for the client.
    let r = h
        .token(
            &[
                ("grant_type", "client_credentials"),
                ("scope", "system/*.rs"),
                ("client_id", "app"),
            ],
            None,
        )
        .await;
    assert_eq!(r.json()["error"], "unauthorized_client");
    // Wrong content type.
    let r = h
        .send(
            axum::http::Request::post("/idp/clinic/token")
                .header("content-type", "application/json")
                .body(axum::body::Body::from("{\"grant_type\":\"x\"}"))
                .unwrap(),
        )
        .await;
    assert_eq!(r.json()["error"], "invalid_request");
    // invalid_client: 401 + WWW-Authenticate.
    let r = h
        .token(
            &[("grant_type", "authorization_code"), ("code", "x")],
            Some(("conf", "wrong")),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json()["error"], "invalid_client");
    assert!(r.header("www-authenticate").starts_with("Basic realm="));
    let r = h
        .token(
            &[("grant_type", "authorization_code"), ("code", "x")],
            Some(("nobody", "x")),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // Confidential client without credentials; public client with a secret.
    let r = h
        .token(
            &[("grant_type", "authorization_code"), ("client_id", "conf")],
            None,
        )
        .await;
    assert_eq!(r.json()["error"], "invalid_client");
    let r = h
        .token(
            &[
                ("grant_type", "authorization_code"),
                ("client_id", "app"),
                ("client_secret", "x"),
            ],
            None,
        )
        .await;
    assert_eq!(r.json()["error"], "invalid_client");
    // No client identification at all.
    let r = h.token(&[("grant_type", "authorization_code")], None).await;
    assert_eq!(r.json()["error"], "invalid_client");
    // Two authentication methods.
    let r = h
        .token(
            &[
                ("grant_type", "authorization_code"),
                ("client_secret", &h.secret),
                ("client_id", "conf"),
            ],
            Some(("conf", &h.secret)),
        )
        .await;
    assert_eq!(r.json()["error"], "invalid_request");
    // Repeated parameter.
    let r = h
        .token(
            &[
                ("grant_type", "authorization_code"),
                ("grant_type", "authorization_code"),
                ("client_id", "app"),
            ],
            None,
        )
        .await;
    assert_eq!(r.json()["error"], "invalid_request");
    assert!(
        h.events
            .take()
            .iter()
            .any(|e| matches!(e, IdpEvent::ClientAuthFailed { .. }))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn confidential_symmetric_client_basic_and_post() {
    let h = Harness::new().await;
    for basic in [true, false] {
        let ix = h
            .start(&h.authz_params("conf", "openid patient/*.rs"))
            .await;
        let code = h
            .complete(&ix, h.decision("alice", &["openid", "patient/*.rs"], None))
            .await
            .param("code")
            .unwrap()
            .to_string();
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT),
            ("code_verifier", VERIFIER),
        ];
        let r = if basic {
            h.token(&form, Some(("conf", &h.secret))).await
        } else {
            form.push(("client_id", "conf"));
            form.push(("client_secret", &h.secret));
            h.token(&form, None).await
        };
        assert_eq!(r.status, StatusCode::OK, "{}", r.text());
        assert!(r.json()["id_token"].is_string());
    }
}

// ------------------------------------------------------------- refresh

#[tokio::test(flavor = "multi_thread")]
async fn refresh_rotation_reuse_detection_and_narrowing() {
    let h = Harness::new().await;
    let t = h
        .code_flow(
            "openid launch/patient offline_access patient/*.rs",
            Some("p1"),
        )
        .await;
    let rt0 = t["refresh_token"].as_str().unwrap().to_string();
    let refresh = |tok: String, scope: Option<&'static str>, client: &'static str| {
        let h = &h;
        async move {
            let mut f = vec![
                ("grant_type", "refresh_token".to_string()),
                ("refresh_token", tok),
                ("client_id", client.to_string()),
            ];
            if let Some(s) = scope {
                f.push(("scope", s.to_string()));
            }
            let f: Vec<(&str, &str)> = f.iter().map(|(a, b)| (*a, b.as_str())).collect();
            h.token(&f, None).await
        }
    };
    // Rotation: new refresh token, same context, new access token.
    let r1 = refresh(rt0.clone(), None, "app").await;
    assert_eq!(r1.status, StatusCode::OK, "{}", r1.text());
    let j1 = r1.json();
    let rt1 = j1["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(rt1, rt0);
    assert_eq!(j1["patient"], "p1");
    assert_eq!(r1.header("cache-control"), "no-store");
    // Narrowing works, widening is invalid_scope.
    let r2 = refresh(rt1.clone(), Some("patient/Observation.rs"), "app").await;
    assert_eq!(r2.status, StatusCode::OK);
    assert_eq!(r2.json()["scope"], "patient/Observation.rs");
    let rt2 = r2.json()["refresh_token"].as_str().unwrap().to_string();
    let r = refresh(rt2.clone(), Some("patient/*.cruds"), "app").await;
    assert_eq!(r.json()["error"], "invalid_scope");
    // Bound to the client.
    let r = h
        .token(
            &[("grant_type", "refresh_token"), ("refresh_token", &rt2)],
            Some(("conf", &h.secret)),
        )
        .await;
    assert_eq!(r.json()["error"], "invalid_grant");
    // Reuse of an old generation → family + grant revoked (RFC 9700 §4.14.2).
    let at2 = r2.json()["access_token"].as_str().unwrap().to_string();
    let r = refresh(rt0.clone(), None, "app").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["error"], "invalid_grant");
    let r = refresh(rt2.clone(), None, "app").await;
    assert_eq!(
        r.json()["error"],
        "invalid_grant",
        "the current token died with the family"
    );
    assert_eq!(h.introspect(&at2).await["active"], false);
    assert!(
        h.events
            .take()
            .iter()
            .any(|e| matches!(e, IdpEvent::RefreshReuse { .. }))
    );
    // Garbage.
    let r = refresh("hmrt_nope".into(), None, "app").await;
    assert_eq!(r.json()["error"], "invalid_grant");
}

#[tokio::test(flavor = "multi_thread")]
async fn no_refresh_token_without_offline_access() {
    let h = Harness::new().await;
    let t = h.code_flow("patient/*.rs", None).await;
    assert!(t.get("refresh_token").is_none());
    assert!(t.get("id_token").is_none());
    assert!(t.get("patient").is_none());
}

// ------------------------------------------------------------- introspection / revocation

#[tokio::test(flavor = "multi_thread")]
async fn introspection_active_inactive_and_access_control() {
    let h = Harness::new().await;
    let t = h
        .code_flow(
            "openid fhirUser launch/patient offline_access patient/*.rs",
            Some("p1"),
        )
        .await;
    let at = t["access_token"].as_str().unwrap();
    let i = h.introspect(at).await;
    assert_eq!(i["active"], true);
    assert_eq!(i["client_id"], "app");
    assert_eq!(i["sub"], "alice");
    assert_eq!(i["iss"], ISS);
    assert_eq!(i["patient"], "p1");
    assert_eq!(i["fhirUser"], format!("{AUD}/Patient/p1"));
    assert_eq!(i["token_type"], "Bearer");
    assert!(i["exp"].is_i64() && i["scope"].as_str().unwrap().contains("patient/*.rs"));
    // Inactive: garbage, tampered, foreign issuer.
    for bad in ["garbage", "eyJhbGciOiJub25lIn0.e30.", &format!("{at}x")] {
        let i = h.introspect(bad).await;
        assert_eq!(i, json!({"active": false}), "{bad}");
    }
    let foreign = SigningKey::generate(SigningAlg::ES256, RsaBits::B2048).unwrap();
    let forged = foreign
        .sign_jws(
            "at+jwt",
            &json!({"iss": ISS, "exp": 9999999999i64, "gid": "g"}),
        )
        .unwrap();
    assert_eq!(h.introspect(&forged).await["active"], false);
    // Refresh token: only for its own client.
    let rt = t["refresh_token"].as_str().unwrap();
    assert_eq!(
        h.introspect(rt).await["active"],
        false,
        "rs may not see app's refresh token"
    );
    let r = h
        .post(
            "/idp/clinic/introspect",
            &[("token", rt), ("client_id", "app")],
            &[],
        )
        .await;
    let j = r.json();
    assert_eq!(j["active"], true);
    assert_eq!(j["token_type"], "refresh_token");
    assert_eq!(j["patient"], "p1");
    // Unauthenticated / bad credentials.
    let r = h
        .post("/idp/clinic/introspect", &[("token", at)], &[])
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json()["error"], "invalid_client");
    let r = h
        .post(
            "/idp/clinic/introspect",
            &[("token", at)],
            &[("authorization", &basic("rs", "nope"))],
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // Bearer without the introspect scope.
    let r = h
        .post(
            "/idp/clinic/introspect",
            &[("token", at)],
            &[("authorization", &format!("Bearer {at}"))],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.header("www-authenticate").contains("insufficient_scope"));
    // Missing token parameter.
    let r = h
        .post(
            "/idp/clinic/introspect",
            &[],
            &[("authorization", &basic("rs", &h.rs_secret))],
        )
        .await;
    assert_eq!(r.json()["error"], "invalid_request");
}

#[tokio::test(flavor = "multi_thread")]
async fn revocation_of_refresh_and_access_tokens() {
    let h = Harness::new().await;
    // Refresh token revocation kills the family and the access token.
    let t = h.code_flow("offline_access patient/*.rs", None).await;
    let at = t["access_token"].as_str().unwrap().to_string();
    let rt = t["refresh_token"].as_str().unwrap().to_string();
    // Another client cannot revoke it (no effect, still 200).
    let r = h
        .post(
            "/idp/clinic/revoke",
            &[("token", &rt)],
            &[("authorization", &basic("conf", &h.secret))],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(h.introspect(&at).await["active"], true);
    let r = h
        .post(
            "/idp/clinic/revoke",
            &[
                ("token", &rt),
                ("token_type_hint", "refresh_token"),
                ("client_id", "app"),
            ],
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(h.introspect(&at).await["active"], false);
    let r = h
        .token(
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &rt),
                ("client_id", "app"),
            ],
            None,
        )
        .await;
    assert_eq!(r.json()["error"], "invalid_grant");
    // Access token revocation.
    let t = h.code_flow("offline_access patient/*.rs", None).await;
    let at = t["access_token"].as_str().unwrap().to_string();
    let gid = verify_jwt(&h.get("/idp/clinic/jwks").await.json(), &at, AUD)["gid"]
        .as_str()
        .unwrap()
        .to_string();
    let r = h
        .post(
            "/idp/clinic/revoke",
            &[("token", &at), ("client_id", "app")],
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(h.introspect(&at).await["active"], false);
    assert!(h.rt.realm("clinic").unwrap().grant_revoked(&gid));
    assert!(
        h.events
            .take()
            .iter()
            .any(|e| matches!(e, IdpEvent::TokenRevoked { .. }))
    );
    // Unknown token: 200 (RFC 7009 §2.2). Missing token: invalid_request.
    let r = h
        .post(
            "/idp/clinic/revoke",
            &[("token", "whatever"), ("client_id", "app")],
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = h
        .post("/idp/clinic/revoke", &[("client_id", "app")], &[])
        .await;
    assert_eq!(r.json()["error"], "invalid_request");
    // Unauthenticated confidential client.
    let r = h
        .post(
            "/idp/clinic/revoke",
            &[("token", "x"), ("client_id", "conf")],
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

// ------------------------------------------------------------- backend services / private_key_jwt

async fn backend_token(h: &Harness, _client: &str, assertion: &str, scope: &str) -> Resp {
    h.token(
        &[
            ("grant_type", "client_credentials"),
            ("scope", scope),
            (
                "client_assertion_type",
                "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
            ),
            ("client_assertion", assertion),
        ],
        None,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn backend_services_private_key_jwt_success_and_claims() {
    let h = Harness::new().await;
    // RS384 via jwks_uri.
    let a = h.assertion(&h.backend_key, "backend", json!({}));
    let r = backend_token(
        &h,
        "backend",
        &a,
        "system/Observation.rs system/Patient.rs patient/*.rs",
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let t = r.json();
    assert_eq!(
        t["scope"], "system/Observation.rs system/Patient.rs",
        "pre-authorized system scopes only (patient/ dropped)"
    );
    assert!(t["expires_in"].as_u64().unwrap() <= 300);
    assert!(
        t.get("refresh_token").is_none(),
        "Backend Services: no refresh token"
    );
    let c = verify_jwt(
        &h.get("/idp/clinic/jwks").await.json(),
        t["access_token"].as_str().unwrap(),
        AUD,
    );
    assert_eq!(c["sub"], "backend", "RFC 9068: sub = client_id");
    // ES384 with inline JWKS.
    let a = h.assertion(&h.asym_key, "asym", json!({}));
    let r = backend_token(&h, "asym", &a, "system/*.rs").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    // aud = issuer also accepted.
    let a = h.assertion(&h.asym_key, "asym", json!({"aud": ISS}));
    assert_eq!(
        backend_token(&h, "asym", &a, "system/*.rs").await.status,
        StatusCode::OK
    );
    // Nothing pre-authorized.
    let a = h.assertion(&h.asym_key, "asym", json!({}));
    let r = backend_token(&h, "asym", &a, "system/Observation.cruds").await;
    assert_eq!(r.json()["error"], "invalid_scope");
    let a = h.assertion(&h.asym_key, "asym", json!({}));
    let r = h
        .token(
            &[
                ("grant_type", "client_credentials"),
                (
                    "client_assertion_type",
                    "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
                ),
                ("client_assertion", &a),
            ],
            None,
        )
        .await;
    assert_eq!(r.json()["error"], "invalid_scope", "scope required");
}

#[tokio::test(flavor = "multi_thread")]
async fn private_key_jwt_negative_cases_are_invalid_client() {
    let h = Harness::new().await;
    let now = chrono::Utc::now().timestamp();
    let k = &h.asym_key;
    let other = SigningKey::generate(SigningAlg::ES384, RsaBits::B2048).unwrap();
    let mut forged_kid = other.public_jwk().clone();
    forged_kid["kid"] = k.public_jwk()["kid"].clone();
    let _ = forged_kid;
    let cases: Vec<(&str, String)> = vec![
        (
            "exp > 5 min",
            h.assertion(k, "asym", json!({"exp": now + 900})),
        ),
        ("expired", h.assertion(k, "asym", json!({"exp": now - 120}))),
        ("no exp", h.assertion(k, "asym", json!({"exp": null}))),
        ("no jti", h.assertion(k, "asym", json!({"jti": null}))),
        (
            "wrong aud",
            h.assertion(k, "asym", json!({"aud": "https://other/token"})),
        ),
        (
            "multi aud",
            h.assertion(
                k,
                "asym",
                json!({"aud": [format!("{ISS}/token"), "https://x"]}),
            ),
        ),
        (
            "iss != sub",
            h.assertion(k, "asym", json!({"sub": "backend"})),
        ),
        ("unknown client", h.assertion(k, "ghost", json!({}))),
        ("wrong key", h.assertion(&other, "asym", json!({}))),
        (
            "future nbf",
            h.assertion(k, "asym", json!({"nbf": now + 600})),
        ),
        ("secret client", h.assertion(k, "conf", json!({}))),
    ];
    for (name, a) in cases {
        let r = backend_token(&h, "asym", &a, "system/*.rs").await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{name}: {}", r.text());
        assert_eq!(r.json()["error"], "invalid_client", "{name}");
    }
    // Signature with a forged header (alg confusion / none / HS256 / jku).
    let good = h.assertion(k, "asym", json!({}));
    let parts: Vec<&str> = good.split('.').collect();
    let kid = k.public_jwk()["kid"].as_str().unwrap();
    for hdr in [
        json!({"alg": "none", "kid": kid}),
        json!({"alg": "HS256", "kid": kid}),
        json!({"alg": "RS384", "kid": kid}),
        json!({"alg": "ES384", "kid": kid, "jku": "https://evil.example/jwks"}),
        json!({"alg": "ES384", "kid": kid, "jwk": k.public_jwk()}),
        json!({"alg": "ES384"}),
    ] {
        let h64 = helios_idp::util::b64(hdr.to_string().as_bytes());
        let forged = format!("{h64}.{}.{}", parts[1], parts[2]);
        let r = backend_token(&h, "asym", &forged, "system/*.rs").await;
        assert_eq!(r.json()["error"], "invalid_client", "{hdr}");
    }
    // Replay of the same jti.
    let a = h.assertion(k, "asym", json!({}));
    assert_eq!(
        backend_token(&h, "asym", &a, "system/*.rs").await.status,
        StatusCode::OK
    );
    let r = backend_token(&h, "asym", &a, "system/*.rs").await;
    assert_eq!(r.json()["error"], "invalid_client", "jti replay");
    // Wrong assertion type.
    let a = h.assertion(k, "asym", json!({}));
    let r = h
        .token(
            &[
                ("grant_type", "client_credentials"),
                ("scope", "system/*.rs"),
                ("client_assertion_type", "urn:x"),
                ("client_assertion", &a),
            ],
            None,
        )
        .await;
    assert_eq!(r.json()["error"], "invalid_client");
    // Assertion plus client_secret → invalid_request.
    let a = h.assertion(k, "asym", json!({}));
    let r = h
        .token(
            &[
                ("grant_type", "client_credentials"),
                ("scope", "system/*.rs"),
                (
                    "client_assertion_type",
                    "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
                ),
                ("client_assertion", &a),
                ("client_secret", "x"),
            ],
            None,
        )
        .await;
    assert_eq!(r.json()["error"], "invalid_request");
}

#[tokio::test(flavor = "multi_thread")]
async fn private_key_jwt_with_rotated_client_keys() {
    let h = Harness::new().await;
    let a = h.assertion(&h.backend_key, "backend", json!({}));
    assert_eq!(
        backend_token(&h, "backend", &a, "system/*.rs").await.status,
        StatusCode::OK
    );
    let fetches = *h.jwks.fetches.lock().unwrap();
    // The client rotates: new key published, old one removed.
    let new_key = SigningKey::generate(SigningAlg::ES384, RsaBits::B2048).unwrap();
    h.jwks.sets.lock().unwrap().insert(
        "https://backend.example/jwks.json".into(),
        json!({"keys": [new_key.public_jwk()]}),
    );
    // Cached set is used for the old kid…
    let a = h.assertion(&h.backend_key, "backend", json!({}));
    assert_eq!(
        backend_token(&h, "backend", &a, "system/*.rs").await.status,
        StatusCode::OK
    );
    // …an unknown kid forces a refetch and succeeds with the new key.
    let a = h.assertion(&new_key, "backend", json!({}));
    let r = backend_token(&h, "backend", &a, "system/*.rs").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert!(*h.jwks.fetches.lock().unwrap() > fetches);
    // After the refetch the retired key no longer works.
    let a = h.assertion(&h.backend_key, "backend", json!({}));
    assert_eq!(
        backend_token(&h, "backend", &a, "system/*.rs").await.json()["error"],
        "invalid_client"
    );
    // jku equal to the registered jwks_uri is accepted.
    let a2 = {
        let now = chrono::Utc::now().timestamp();
        let c = json!({"iss":"backend","sub":"backend","aud":format!("{ISS}/token"),"exp":now+200,"jti":"jku-ok"});
        let hdr = json!({"alg":"ES384","kid":new_key.kid(),"typ":"JWT","jku":"https://backend.example/jwks.json"});
        let input = format!(
            "{}.{}",
            helios_idp::util::b64(hdr.to_string().as_bytes()),
            helios_idp::util::b64(c.to_string().as_bytes())
        );
        format!(
            "{input}.{}",
            helios_idp::util::b64(&new_key.sign(input.as_bytes()).unwrap())
        )
    };
    assert_eq!(
        backend_token(&h, "backend", &a2, "system/*.rs")
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn private_key_jwt_for_authorization_code_and_introspection() {
    let h = Harness::new().await;
    let ix = h
        .start(&h.authz_params("asym", "openid patient/*.rs"))
        .await;
    let code = h
        .complete(&ix, h.decision("alice", &["openid", "patient/*.rs"], None))
        .await
        .param("code")
        .unwrap()
        .to_string();
    let a = h.assertion(&h.asym_key, "asym", json!({}));
    let r = h
        .token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT),
                ("code_verifier", VERIFIER),
                (
                    "client_assertion_type",
                    "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
                ),
                ("client_assertion", &a),
            ],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    // Introspection with private_key_jwt, aud = introspection endpoint.
    let at = r.json()["access_token"].as_str().unwrap().to_string();
    let a = h.assertion(
        &h.asym_key,
        "asym",
        json!({"aud": format!("{ISS}/introspect")}),
    );
    let r = h
        .post(
            "/idp/clinic/introspect",
            &[
                ("token", &at),
                (
                    "client_assertion_type",
                    "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
                ),
                ("client_assertion", &a),
            ],
            &[],
        )
        .await;
    assert_eq!(r.json()["active"], true);
}

// ------------------------------------------------------------- EHR launch

#[tokio::test(flavor = "multi_thread")]
async fn ehr_launch_context_from_launch_handle() {
    let h = Harness::new().await;
    let launch = h
        .admin
        .create_launch(
            &h.realm_id,
            helios_idp::admin::LaunchRequest {
                client_id: Some("app".into()),
                endpoint: "clinic".into(),
                launched_by: Some("console:dr".into()),
                context: helios_idp::launch::LaunchContext {
                    patient: Some("p9".into()),
                    encounter: Some("e1".into()),
                    fhir_context: vec![helios_idp::launch::FhirContextItem {
                        reference: Some("Appointment/a1".into()),
                        canonical: None,
                        identifier: None,
                        resource_type: None,
                        role: None,
                    }],
                    intent: Some("reconcile-medications".into()),
                    tenant: Some("t1".into()),
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(launch.iss, AUD);
    let mut p = h.authz_params("app", "launch openid fhirUser patient/*.rs");
    p.push(("launch".into(), launch.launch.clone()));
    let ix = h.start(&p).await;
    let resp = h
        .complete(
            &ix,
            // The pages cannot change the EHR's patient.
            h.decision(
                "bob",
                &["launch", "openid", "fhirUser", "patient/*.rs"],
                Some("p1"),
            ),
        )
        .await;
    let code = resp.param("code").unwrap().to_string();
    let t = h
        .token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT),
                ("code_verifier", VERIFIER),
                ("client_id", "app"),
            ],
            None,
        )
        .await
        .json();
    assert_eq!(t["patient"], "p9");
    assert_eq!(t["encounter"], "e1");
    assert_eq!(t["fhirContext"][0]["reference"], "Appointment/a1");
    assert_eq!(t["intent"], "reconcile-medications");
    assert_eq!(t["tenant"], "t1");
    assert_eq!(t["need_patient_banner"], true);
    let ic = verify_jwt(
        &h.get("/idp/clinic/jwks").await.json(),
        t["id_token"].as_str().unwrap(),
        "app",
    );
    assert_eq!(ic["fhirUser"], format!("{AUD}/Practitioner/dr1"));
    // The handle is single use.
    let r = h.authorize(&p).await;
    assert_eq!(
        r.redirect_params().get("error").map(String::as_str),
        Some("invalid_request")
    );
    // A handle for another client, and launch without the launch scope.
    let other = h
        .admin
        .create_launch(
            &h.realm_id,
            helios_idp::admin::LaunchRequest {
                client_id: Some("conf".into()),
                endpoint: "clinic".into(),
                launched_by: None,
                context: Default::default(),
            },
        )
        .await
        .unwrap();
    let mut p = h.authz_params("app", "launch patient/*.rs");
    p.push(("launch".into(), other.launch));
    assert_eq!(
        h.authorize(&p).await.redirect_params()["error"],
        "invalid_request"
    );
    let fresh = h
        .admin
        .create_launch(
            &h.realm_id,
            helios_idp::admin::LaunchRequest {
                client_id: None,
                endpoint: "clinic".into(),
                launched_by: None,
                context: Default::default(),
            },
        )
        .await
        .unwrap();
    let mut p = h.authz_params("app", "patient/*.rs");
    p.push(("launch".into(), fresh.launch));
    assert_eq!(
        h.authorize(&p).await.redirect_params()["error"],
        "invalid_scope"
    );
}

// ------------------------------------------------------------- userinfo

#[tokio::test(flavor = "multi_thread")]
async fn userinfo_bearer_rules() {
    let h = Harness::new().await;
    let t = h
        .code_flow("openid fhirUser profile email patient/*.rs", None)
        .await;
    let at = t["access_token"].as_str().unwrap();
    let r = h
        .send(
            axum::http::Request::get("/idp/clinic/userinfo")
                .header("authorization", format!("Bearer {at}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let u = r.json();
    assert_eq!(u["sub"], "alice");
    assert_eq!(u["fhirUser"], format!("{AUD}/Patient/p1"));
    assert_eq!(u["name"], "Alice Example");
    assert_eq!(u["email"], "alice@example.org");
    // POST with the token in the form body.
    let r = h
        .post("/idp/clinic/userinfo", &[("access_token", at)], &[])
        .await;
    assert_eq!(r.status, StatusCode::OK);
    // Query string refused.
    let r = h
        .get(&format!("/idp/clinic/userinfo?access_token={at}"))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    // No token / bad token.
    let r = h.get("/idp/clinic/userinfo").await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(r.header("www-authenticate").starts_with("Bearer realm="));
    let r = h
        .send(
            axum::http::Request::get("/idp/clinic/userinfo")
                .header("authorization", "Bearer nope")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(
        r.header("www-authenticate")
            .contains("error=\"invalid_token\"")
    );
    // Without openid.
    let t = h.code_flow("patient/*.rs", None).await;
    let r = h
        .send(
            axum::http::Request::get("/idp/clinic/userinfo")
                .header(
                    "authorization",
                    format!("Bearer {}", t["access_token"].as_str().unwrap()),
                )
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

// ------------------------------------------------------------- keys, logout, misc

#[tokio::test(flavor = "multi_thread")]
async fn emergency_key_rotation_kills_tokens() {
    let h = Harness::new().await;
    let t = h.code_flow("offline_access patient/*.rs", None).await;
    let at = t["access_token"].as_str().unwrap().to_string();
    let old_kids: Vec<Value> = h.get("/idp/clinic/jwks").await.json()["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["kid"].clone())
        .collect();
    h.admin.emergency_rotate(&h.realm_id).await.unwrap();
    let new: Vec<Value> = h.get("/idp/clinic/jwks").await.json()["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["kid"].clone())
        .collect();
    assert!(
        new.iter().all(|k| !old_kids.contains(k)),
        "compromised keys unpublished"
    );
    assert_eq!(h.introspect(&at).await["active"], false);
    let r = h
        .token(
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", t["refresh_token"].as_str().unwrap()),
                ("client_id", "app"),
            ],
            None,
        )
        .await;
    assert_eq!(r.json()["error"], "invalid_grant");
    // New tokens verify with the new keys.
    let t = h.code_flow("patient/*.rs", None).await;
    verify_jwt(
        &h.get("/idp/clinic/jwks").await.json(),
        t["access_token"].as_str().unwrap(),
        AUD,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn planned_rotation_prepublishes_then_promotes() {
    let h = Harness::new().await;
    let created = h.admin.rotate_keys(&h.realm_id).await.unwrap();
    assert_eq!(created.len(), 2);
    let jwks = h.get("/idp/clinic/jwks").await.json();
    assert_eq!(
        jwks["keys"].as_array().unwrap().len(),
        4,
        "next keys published early"
    );
    // Tokens still signed by the old active key.
    let t = h.code_flow("patient/*.rs", None).await;
    let kid = decode_header(t["access_token"].as_str().unwrap())
        .unwrap()
        .kid
        .unwrap();
    assert!(created.iter().all(|k| k.kid != kid));
    // After the pre-publication period the planner promotes.
    let later = chrono::Utc::now() + chrono::Duration::days(91);
    let acts = h.admin.run_rotation(&h.realm_id, later).await.unwrap();
    assert!(
        acts.iter()
            .any(|a| matches!(a, helios_idp::keys::RotationAction::Promote { .. }))
    );
    let t2 = h.code_flow("patient/*.rs", None).await;
    let kid2 = decode_header(t2["access_token"].as_str().unwrap())
        .unwrap()
        .kid
        .unwrap();
    assert!(created.iter().any(|k| k.kid == kid2), "new key signs");
    // Old tokens still verify (retiring keys stay published).
    assert_eq!(
        h.introspect(t["access_token"].as_str().unwrap()).await["active"],
        true
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rp_initiated_logout_validation() {
    let h = Harness::new().await;
    let t = h.code_flow("openid patient/*.rs", None).await;
    let id = t["id_token"].as_str().unwrap().to_string();
    let rt = h.rt.realm("clinic").unwrap();
    let store = h.rt.store().clone();
    let plan = helios_idp::logout::check_logout(
        &rt,
        store.as_ref(),
        &helios_idp::logout::LogoutRequest {
            id_token_hint: Some(id.clone()),
            client_id: None,
            post_logout_redirect_uri: Some("https://app.example/bye".into()),
            state: Some("s1".into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(plan.client_id.as_deref(), Some("app"));
    assert_eq!(plan.subject.as_deref(), Some("alice"));
    assert_eq!(
        plan.redirect.as_deref(),
        Some("https://app.example/bye?state=s1")
    );
    // Unregistered post-logout URI, mismatched client, access token as hint.
    for req in [
        helios_idp::logout::LogoutRequest {
            id_token_hint: Some(id.clone()),
            post_logout_redirect_uri: Some("https://evil.example/".into()),
            ..Default::default()
        },
        helios_idp::logout::LogoutRequest {
            id_token_hint: Some(id.clone()),
            client_id: Some("conf".into()),
            ..Default::default()
        },
        helios_idp::logout::LogoutRequest {
            id_token_hint: Some(t["access_token"].as_str().unwrap().into()),
            ..Default::default()
        },
        helios_idp::logout::LogoutRequest {
            post_logout_redirect_uri: Some("https://app.example/bye".into()),
            ..Default::default()
        },
    ] {
        assert!(
            helios_idp::logout::check_logout(&rt, store.as_ref(), &req)
                .await
                .is_err()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cors_preflight_and_security_headers() {
    let h = Harness::new().await;
    let r = h
        .send(
            axum::http::Request::builder()
                .method("OPTIONS")
                .uri("/idp/clinic/token")
                .header("origin", "https://app.example")
                .header("access-control-request-method", "POST")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(r.header("access-control-allow-origin"), "*");
    let r = h.get("/idp/clinic/authorize?client_id=nobody").await;
    assert_eq!(r.header("x-frame-options"), "DENY");
    assert_eq!(r.header("referrer-policy"), "no-referrer");
    assert!(
        r.header("access-control-allow-origin").is_empty(),
        "no CORS on /authorize"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_client_and_realm() {
    let h = Harness::new().await;
    let t = h.code_flow("patient/*.rs", None).await;
    let rec =
        h.rt.store()
            .get_client_record(&h.realm_id, "app")
            .await
            .unwrap()
            .unwrap();
    let mut spec = rec.spec.clone();
    spec.disabled = true;
    h.admin
        .update_client(&h.realm_id, spec, rec.revision)
        .await
        .unwrap();
    let r = h.authorize(&h.authz_params("app", "patient/*.rs")).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.location().is_empty());
    let _ = t;
    let realm = h.rt.store().get_realm(&h.realm_id).await.unwrap().unwrap();
    let mut s = realm.spec.clone();
    s.enabled = false;
    h.admin
        .update_realm(&h.realm_id, s, realm.revision)
        .await
        .unwrap();
    assert_eq!(
        h.get("/idp/clinic/.well-known/openid-configuration")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}
