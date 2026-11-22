//! The console service's diagnostic against the REAL protocol endpoints of
//! `helios-idp-server` (IdP A), over HTTP on a loopback port, with:
//!
//! * a minimal stand-in for IdP B's hosted pages (`/login`, `/consent`,
//!   `/logout`) that finishes interactions through the real
//!   `AuthorizationCompleter`, with a CSRF token and an SSO cookie;
//! * a FHIR resource-server stub that verifies tokens with the realm
//!   runtime (signature, iss, aud = endpoint base, exp, revoked grants) and
//!   serves SMART discovery only for endpoints bound to the realm.
//!
//! The admin service creates the realm, keys, an app and a user; the
//! endpoint is bound through the endpoint store. A healthy realm passes;
//! wrong redirect URI, disabled client, expired key, endpoint not bound
//! and clock skew are pinpointed.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Form, Json, Router};
use chrono::Utc;
use helios_endpoints::{Actor, EndpointName, EndpointSpec, EndpointStore, MemoryEndpointStore, SecretKeyRing};
use helios_idp::admin::IdpAdmin;
use helios_idp::client::{ClientAuth, ClientSpec, GrantType, LaunchType};
use helios_idp::interaction::{AuthorizationCompleter, Decision, Prompt};
use helios_idp::memory::MemoryStore;
use helios_idp::runtime::{ConventionalEndpointUrls, IdpRuntime};
use helios_idp::scope::ScopePolicy;
use helios_idp::token::{HandleKind, handle_hash};
use helios_idp::{OAuthError, OAuthErrorCode};
use helios_idp_admin::binding::{BindMode, StoreBinder};
use helios_idp_admin::diagnostic::{AppProbe, Status};
use helios_idp_admin::service::{DiagnoseOptions, IdpConsole};
use helios_idp_server::{IdpServer, IdpServerOptions};
use serde_json::{Value, json};
use url::Url;

#[derive(Clone)]
struct Pages {
    rt: Arc<IdpRuntime>,
    completer: Arc<dyn AuthorizationCompleter>,
    eps: Arc<dyn EndpointStore>,
    csrf: Arc<Mutex<HashSet<String>>>,
    /// session cookie → user id
    sessions: Arc<Mutex<HashMap<String, String>>>,
}

fn cookie(h: &HeaderMap) -> Option<String> {
    h.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|c| {
        let (n, v) = c.trim().split_once('=')?;
        (n == "stub_sso").then(|| v.to_string())
    })
}

fn rid() -> String {
    helios_idp::util::random_b64(16)
}

impl Pages {
    async fn finish(&self, realm: &str, ix: &str, user: &str) -> Response {
        let rt = self.rt.realm(realm).unwrap();
        let Ok(Some(p)) = self.rt.store().get_interaction(&rt.id, &handle_hash(HandleKind::Interaction, ix).unwrap(), Utc::now()).await else {
            return (StatusCode::BAD_REQUEST, Html("<main>expired</main>")).into_response();
        };
        let d = Decision {
            subject: user.to_string(),
            sid: None,
            session_id_hash: None,
            auth_time: Utc::now(),
            amr: vec!["pwd".into()],
            acr: None,
            granted_scopes: p.scopes.clone(),
            context: Default::default(),
            fhir_user: None,
        };
        match self.completer.complete(realm, ix, d).await {
            Ok(r) => Redirect::to(&r.redirect_url()).into_response(),
            Err(e) => (StatusCode::BAD_REQUEST, Html(format!("<main>{e:?}</main>"))).into_response(),
        }
    }

    fn login_form(&self, realm: &str, ix: &str, msg: &str) -> Response {
        let csrf = rid();
        self.csrf.lock().unwrap().insert(csrf.clone());
        Html(format!(
            r#"<main><h1>Sign in</h1><p>{msg}</p><form method="post" action="/idp/{realm}/login"><input type="hidden" name="_csrf" value="{csrf}"><input type="hidden" name="ix" value="{ix}"><input type="text" name="username"><input type="password" name="password"><button type="submit">Sign in</button></form></main>"#
        ))
        .into_response()
    }
}

async fn login_get(State(p): State<Pages>, Path(realm): Path<String>, h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    let ix = q.get("ix").cloned().unwrap_or_default();
    let user = cookie(&h).and_then(|c| p.sessions.lock().unwrap().get(&c).cloned());
    let rt = p.rt.realm(&realm).unwrap();
    let pending = p.rt.store().get_interaction(&rt.id, &handle_hash(HandleKind::Interaction, &ix).unwrap_or_default(), Utc::now()).await.ok().flatten();
    if let Some(u) = user {
        return p.finish(&realm, &ix, &u).await;
    }
    if pending.as_ref().is_some_and(|x| x.has_prompt(Prompt::None)) {
        return match p.completer.deny(&realm, &ix, OAuthError::new(OAuthErrorCode::LoginRequired, "no session")).await {
            Ok(r) => Redirect::to(&r.redirect_url()).into_response(),
            Err(e) => (StatusCode::BAD_REQUEST, format!("{e:?}")).into_response(),
        };
    }
    p.login_form(&realm, &ix, "")
}

async fn login_post(State(p): State<Pages>, Path(realm): Path<String>, Form(f): Form<HashMap<String, String>>) -> Response {
    if !p.csrf.lock().unwrap().remove(f.get("_csrf").map(String::as_str).unwrap_or("")) {
        return (StatusCode::FORBIDDEN, "csrf").into_response();
    }
    let ix = f.get("ix").cloned().unwrap_or_default();
    let rt = p.rt.realm(&realm).unwrap();
    let store = p.rt.store();
    let user = store.find_user_by_username(&rt.id, f.get("username").map(String::as_str).unwrap_or("")).await.ok().flatten();
    let ok = match &user {
        Some(u) => store
            .get_password_hash(&rt.id, &u.id)
            .await
            .ok()
            .flatten()
            .is_some_and(|(h, _)| helios_idp::password::verify(f.get("password").map(String::as_str).unwrap_or(""), &h)),
        None => false,
    };
    if !ok {
        return p.login_form(&realm, &ix, "Username or password is incorrect.");
    }
    let u = user.unwrap();
    let sid = rid();
    p.sessions.lock().unwrap().insert(sid.clone(), u.id.clone());
    let csrf = rid();
    p.csrf.lock().unwrap().insert(csrf.clone());
    (
        [(header::SET_COOKIE, format!("stub_sso={sid}; Path=/; HttpOnly"))],
        Html(format!(
            r#"<main><h1>Allow?</h1><form method="post" action="/idp/{realm}/consent"><input type="hidden" name="_csrf" value="{csrf}"><input type="hidden" name="ix" value="{ix}"><input type="hidden" name="user" value="{}"><button name="decision" value="deny">Deny</button><button name="decision" value="approve">Allow</button></form></main>"#,
            u.id
        )),
    )
        .into_response()
}

async fn consent_post(State(p): State<Pages>, Path(realm): Path<String>, Form(f): Form<HashMap<String, String>>) -> Response {
    if !p.csrf.lock().unwrap().remove(f.get("_csrf").map(String::as_str).unwrap_or("")) || f.get("decision").map(String::as_str) != Some("approve") {
        return (StatusCode::FORBIDDEN, "denied").into_response();
    }
    p.finish(&realm, &f["ix"], &f["user"]).await
}

async fn logout(State(p): State<Pages>, h: HeaderMap) -> Response {
    if let Some(c) = cookie(&h) {
        p.sessions.lock().unwrap().remove(&c);
    }
    ([(header::SET_COOKIE, "stub_sso=; Path=/; Max-Age=0".to_string())], Html("<main>Signed out</main>")).into_response()
}

/// Which realm (if any) the endpoint trusts, from its stored auth section.
async fn trusted_realm(p: &Pages, ep: &str) -> Option<Arc<helios_idp::runtime::RealmRuntime>> {
    let e = p.eps.get(ep).await.ok().flatten()?;
    let iss = e.spec.auth.as_ref()?.issuer.clone()?;
    p.rt.realms().into_iter().find(|r| r.issuer() == iss)
}

async fn smart(State(p): State<Pages>, Path(ep): Path<String>) -> Response {
    match trusted_realm(&p, &ep).await {
        Some(r) => Json(helios_idp_server::smart_configuration(&r)).into_response(),
        None => Json(json!({"issuer": "https://keycloak.example/realms/other", "authorization_endpoint": "https://keycloak.example/realms/other/auth", "token_endpoint": "x", "capabilities": []})).into_response(),
    }
}

async fn fhir(State(p): State<Pages>, Path(ep): Path<String>, method: axum::http::Method, h: HeaderMap) -> Response {
    let token = h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    let unauthorized = || (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer error=\"invalid_token\"")]).into_response();
    if token.is_empty() {
        return unauthorized();
    }
    let Some(r) = trusted_realm(&p, &ep).await else { return unauthorized() };
    let Ok(v) = r.verify_own(token, Some("at+jwt")) else { return unauthorized() };
    let base = format!("{}fhir/{ep}", p.rt.public_base());
    let aud_ok = match v.claims.get("aud") {
        Some(Value::Array(a)) => a.iter().any(|x| x.as_str() == Some(base.as_str())),
        Some(Value::String(s)) => *s == base,
        _ => false,
    };
    let live = v.claims.get("exp").and_then(Value::as_i64).is_some_and(|e| e > Utc::now().timestamp());
    let revoked = v.claims.get("gid").and_then(Value::as_str).is_some_and(|g| r.grant_revoked(g));
    if !aud_ok || !live || revoked {
        return unauthorized();
    }
    if method == axum::http::Method::POST {
        return StatusCode::FORBIDDEN.into_response();
    }
    Json(json!({"resourceType": "Bundle", "type": "searchset", "total": 0})).into_response()
}

struct Stack {
    console: Arc<IdpConsole>,
    eps: Arc<dyn EndpointStore>,
    realm_id: String,
}

async fn stack() -> Stack {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let ring = Arc::new(SecretKeyRing::parse(&SecretKeyRing::generate_spec("t1")).unwrap());
    let rt = IdpRuntime::new(Arc::new(MemoryStore::new()), Some(ring), base.clone(), Arc::new(ConventionalEndpointUrls { public_base: base.to_string() }));
    let server = IdpServer::new(rt.clone(), IdpServerOptions::default());
    let eps: Arc<dyn EndpointStore> = Arc::new(MemoryEndpointStore::new());
    eps.create(EndpointSpec::named(EndpointName::parse("clinic").unwrap(), "Clinic"), &Actor::bootstrap()).await.unwrap();
    let pages = Pages { rt: rt.clone(), completer: server.completer(), eps: eps.clone(), csrf: Default::default(), sessions: Default::default() };
    let app = server
        .router()
        .merge(
            Router::new()
                .route("/idp/{realm}/login", get(login_get).post(login_post))
                .route("/idp/{realm}/consent", axum::routing::post(consent_post))
                .route("/idp/{realm}/logout", get(logout))
                .route("/fhir/{ep}/.well-known/smart-configuration", get(smart))
                .route("/fhir/{ep}/Patient", get(fhir).post(fhir))
                .with_state(pages),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let rt2 = rt.clone();
    let binder = StoreBinder {
        store: eps.clone(),
        server_base: format!("{base}fhir"),
        public_base: base.clone(),
        jwks_via: None,
        realms: Arc::new(move || rt2.realms().iter().map(|r| r.spec.clone()).collect()),
    };
    let console = Arc::new(IdpConsole::new(IdpAdmin::new(rt.clone()), Some(Arc::new(binder))));
    // The wizard path: draft → steps → finish (check, then bind).
    console.create_draft("clinic-apps", "Clinic apps", helios_idp_admin::service::Template::Everything).await.unwrap();
    let rec = console.realm("clinic-apps").await.unwrap();
    console
        .admin
        .create_client(
            &rec.id,
            ClientSpec {
                client_id: "portal".into(),
                name: "Portal".into(),
                auth: ClientAuth::None,
                grant_types: vec![GrantType::AuthorizationCode],
                redirect_uris: vec!["https://portal.example.org/callback".into()],
                post_logout_redirect_uris: vec![],
                launch_types: vec![LaunchType::Standalone],
                scopes: ScopePolicy { allowed: vec!["openid".into(), "user/*.rs".into()], accept_v1: false },
                trusted: false,
                require_dpop: false,
                require_par: false,
                backchannel_logout_uri: None,
                access_token_ttl_secs: None,
                disabled: false,
            },
        )
        .await
        .unwrap();
    console
        .save_step("clinic-apps", 7, |_, w| {
            w.endpoints = vec!["clinic".into()];
            w.bind_mode = BindMode::Full;
            Ok(())
        })
        .await
        .unwrap();
    let out = console.finish("clinic-apps", &Actor::bootstrap()).await.unwrap();
    assert_eq!(out.bound, vec!["clinic"], "{:#?}", out.report.problems());
    assert!(out.report.problems().is_empty(), "{:#?}", out.report.problems());
    Stack { realm_id: rec.id, console, eps }
}

fn app() -> DiagnoseOptions {
    DiagnoseOptions { app: Some(AppProbe { client_id: "portal".into(), redirect_uri: "https://portal.example.org/callback".into() }), ..Default::default() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn healthy_realm_passes_and_faults_are_pinpointed() {
    let s = stack().await;
    let c = &s.console;

    // Healthy (endpoint bound by the wizard's finish).
    let r = c.diagnose("clinic-apps", app()).await.unwrap();
    assert!(r.problems().is_empty(), "{:#?}", r.problems());
    for id in ["discovery.smart.clinic", "authorization-code.flow", "authorization-code.app.portal", "fhir.clinic.read", "refresh.reuse", "backend.token", "introspection.active", "revocation.fhir", "userinfo-logout.userinfo"] {
        assert_eq!(r.check(id).map(|c| c.status), Some(Status::Pass), "{id}: {:#?}", r.check(id));
    }
    // The throw-away identities are gone.
    let clients = c.admin.list_clients(&s.realm_id).await.unwrap();
    assert_eq!(clients.len(), 1, "only the portal app remains");

    // 1. Wrong redirect URI.
    let mut o = app();
    o.app.as_mut().unwrap().redirect_uri = "https://portal.example.org/cb".into();
    let r = c.diagnose("clinic-apps", o).await.unwrap();
    let first = r.problems()[0];
    assert_eq!((first.code.as_str(), first.field.as_deref()), ("app-redirect-unregistered", Some("client.portal.redirectUris")));
    assert_eq!(r.check("authorization-code.app.portal").unwrap().code, "app-authorize-refused");

    // 2. Disabled client.
    let mut portal = c.admin.list_clients(&s.realm_id).await.unwrap().into_iter().find(|x| x.spec.client_id == "portal").unwrap();
    portal.spec.disabled = true;
    c.admin.update_client(&s.realm_id, portal.spec.clone(), portal.revision).await.unwrap();
    let r = c.diagnose("clinic-apps", app()).await.unwrap();
    assert_eq!(r.problems()[0].code, "app-disabled");
    assert_eq!(r.check("authorization-code.app.portal").unwrap().status, Status::Fail);
    let portal = c.admin.list_clients(&s.realm_id).await.unwrap().into_iter().find(|x| x.spec.client_id == "portal").unwrap();
    let mut spec = portal.spec.clone();
    spec.disabled = false;
    c.admin.update_client(&s.realm_id, spec, portal.revision).await.unwrap();

    // 3. Endpoint not bound (its Security settings point elsewhere).
    let e = s.eps.get("clinic").await.unwrap().unwrap();
    let mut es = e.spec.clone();
    es.auth.as_mut().unwrap().issuer = Some("https://keycloak.example/realms/other".into());
    s.eps.update("clinic", e.revision, es, &Actor::bootstrap()).await.unwrap();
    let r = c.diagnose("clinic-apps", DiagnoseOptions::default()).await.unwrap();
    assert_eq!((r.problems()[0].code.as_str(), r.problems()[0].field.as_deref()), ("endpoint-not-bound", Some("endpoints.clinic")));
    assert_eq!(r.check("discovery.smart.clinic").unwrap().code, "endpoint-smart-other-issuer");
    assert_eq!(r.check("fhir.clinic.read").unwrap().code, "fhir-token-refused");
    c.bind("clinic-apps", "clinic", BindMode::Full, &Actor::bootstrap()).await.unwrap();
    assert!(c.diagnose("clinic-apps", DiagnoseOptions::default()).await.unwrap().problems().is_empty());

    // 4. Expired key (the active ES256 key past its expiry: the rotation
    // job did not run).
    let k = c.admin.list_keys(&s.realm_id).await.unwrap().into_iter().find(|k| k.alg == helios_idp::keys::SigningAlg::ES256 && k.status == helios_idp::keys::KeyStatus::Active).unwrap();
    let past = Utc::now() - chrono::Duration::hours(1);
    let changed = c.admin.runtime().store().set_key_status(&s.realm_id, &k.kid, helios_idp::keys::KeyStatus::Active, helios_idp::keys::KeyStatus::Active, past, Some(past)).await.unwrap();
    assert!(changed);
    let r = c.diagnose("clinic-apps", DiagnoseOptions::default()).await.unwrap();
    assert_eq!((r.problems()[0].code.as_str(), r.problems()[0].field.as_deref()), ("key-expired", Some("keys")));
    c.admin.rotate_keys(&s.realm_id).await.unwrap();
    c.admin.run_rotation(&s.realm_id, Utc::now() + chrono::Duration::days(400)).await.unwrap();

    // 5. Clock skew.
    let mut skewed = IdpConsole::new(c.admin.clone(), c.binder.clone());
    skewed.clock_offset_secs = 900;
    let r = skewed.diagnose("clinic-apps", DiagnoseOptions::default()).await.unwrap();
    assert_eq!(r.check("discovery.clock").map(|c| c.code.as_str()), Some("clock-skew"));
    assert_eq!(r.problems()[0].code, "clock-skew");
}
