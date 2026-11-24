//! The diagnostic against a mock realm + FHIR endpoint that speaks the
//! protocol the way design §5–§8 specifies (hosted login form with CSRF,
//! consent, PKCE S256, code single use with replay revocation, refresh
//! rotation with reuse detection, private_key_jwt with jti replay cache,
//! introspection, revocation, userinfo, RP-initiated logout, a resource
//! server checking iss/aud/signature/revocation). A healthy realm must pass;
//! each injected fault must be reported at the right step with the right
//! code and field.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use helios_idp::keys::{RsaBits, SigningAlg, SigningKey, thumbprint};
use helios_idp_admin::diagnostic::{
    self, AppProbe, EndpointProvider, EndpointView, Fixture, PreflightInput, RunConfig, Status,
};
use serde_json::{Value, json};

const CB: &str = "https://diagnostic.invalid/cb";
const USER: &str = "diag-user";
const PASS: &str = "correct horse battery staple";

#[derive(Default, Clone)]
struct Faults {
    /// Sign tokens with a key that is not published (expired/retired key).
    unpublished_signer: bool,
    /// The endpoint trusts another issuer (not bound).
    endpoint_other_issuer: bool,
    /// Server clock offset in seconds (Date header and token iat/exp).
    clock_offset: i64,
    /// The app client is disabled.
    app_disabled: bool,
    /// The first N /authorize requests answer 429 Retry-After: 1.
    rate_limit_first: usize,
}

struct Mock {
    base: String,
    faults: Faults,
    es: SigningKey,
    rs: SigningKey,
    stray: SigningKey,
    backend_jwk: Value,
    csrf: HashSet<String>,
    txs: HashMap<String, HashMap<String, String>>,
    sessions: HashSet<String>,
    codes: HashMap<String, (HashMap<String, String>, String)>,
    used_codes: HashMap<String, String>,
    revoked_grants: HashSet<String>,
    /// refresh token → (family, generation)
    refresh: HashMap<String, (String, u32)>,
    families: HashMap<String, (u32, bool, String)>,
    jtis: HashSet<String>,
    clients: HashMap<String, (Vec<String>, bool)>,
}

type S = Arc<Mutex<Mock>>;

impl Mock {
    fn issuer(&self) -> String {
        format!("{}/idp/r", self.base)
    }
    fn fhir(&self) -> String {
        format!("{}/fhir/ep1", self.base)
    }
    fn now(&self) -> i64 {
        chrono::Utc::now().timestamp() + self.faults.clock_offset
    }
    fn signer(&self, alg: SigningAlg) -> &SigningKey {
        if self.faults.unpublished_signer {
            return &self.stray;
        }
        if alg == SigningAlg::RS256 {
            &self.rs
        } else {
            &self.es
        }
    }
    fn rid() -> String {
        helios_idp::util::random_b64(16)
    }
}

fn date(s: &Mock) -> (header::HeaderName, String) {
    let t = chrono::Utc::now() + chrono::Duration::seconds(s.faults.clock_offset);
    (header::DATE, t.to_rfc2822().replace("+0000", "GMT"))
}

fn json_resp(s: &Mock, status: StatusCode, v: Value) -> Response {
    (
        status,
        [date(s), (header::CACHE_CONTROL, "no-store".into())],
        Json(v),
    )
        .into_response()
}

fn error_page(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Html(format!(
            "<html><body><main><h1>Error</h1><p>{msg}</p></main></body></html>"
        )),
    )
        .into_response()
}

async fn oidc(State(st): State<S>) -> Response {
    let s = st.lock().unwrap();
    let i = s.issuer();
    json_resp(
        &s,
        StatusCode::OK,
        json!({
            "issuer": i,
            "authorization_endpoint": format!("{i}/authorize"),
            "token_endpoint": format!("{i}/token"),
            "jwks_uri": format!("{i}/jwks"),
            "userinfo_endpoint": format!("{i}/userinfo"),
            "introspection_endpoint": format!("{i}/introspect"),
            "revocation_endpoint": format!("{i}/revoke"),
            "end_session_endpoint": format!("{i}/logout"),
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256", "ES256"],
            "code_challenge_methods_supported": ["S256"],
        }),
    )
}

async fn jwks(State(st): State<S>) -> Response {
    let s = st.lock().unwrap();
    (
        StatusCode::OK,
        [date(&s), (header::CACHE_CONTROL, "max-age=300".into())],
        Json(json!({"keys": [s.rs.public_jwk(), s.es.public_jwk()]})),
    )
        .into_response()
}

async fn smart(State(st): State<S>) -> Response {
    let s = st.lock().unwrap();
    let i = if s.faults.endpoint_other_issuer {
        "https://keycloak.example/realms/x".to_string()
    } else {
        s.issuer()
    };
    json_resp(
        &s,
        StatusCode::OK,
        json!({
            "issuer": i,
            "authorization_endpoint": format!("{i}/authorize"),
            "token_endpoint": format!("{i}/token"),
            "capabilities": ["launch-standalone", "launch-ehr", "client-public", "sso-openid-connect"],
        }),
    )
}

fn cookie(h: &HeaderMap, name: &str) -> Option<String> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| {
            let (n, v) = c.trim().split_once('=')?;
            (n == name).then(|| v.to_string())
        })
}

fn redirect_with(uri: &str, params: &[(&str, &str)]) -> Response {
    let mut u = url::Url::parse(uri).unwrap();
    for (k, v) in params {
        u.query_pairs_mut().append_pair(k, v);
    }
    Redirect::to(u.as_str()).into_response()
}

fn issue_code(s: &mut Mock, q: &HashMap<String, String>) -> Response {
    let code = Mock::rid();
    s.codes.insert(code.clone(), (q.clone(), Mock::rid()));
    let iss = s.issuer();
    redirect_with(
        &q["redirect_uri"],
        &[
            ("code", &code),
            ("state", q.get("state").map(String::as_str).unwrap_or("")),
            ("iss", &iss),
        ],
    )
}

async fn authorize(
    State(st): State<S>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let mut s = st.lock().unwrap();
    if s.faults.rate_limit_first > 0 {
        s.faults.rate_limit_first -= 1;
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "1".to_string())],
            Json(json!({"error": "slow_down"})),
        )
            .into_response();
    }
    let cid = q.get("client_id").cloned().unwrap_or_default();
    let Some((redirects, enabled)) = s.clients.get(&cid).cloned() else {
        return error_page("Unknown client.");
    };
    if !enabled {
        return error_page(&format!("The application {cid} is disabled."));
    }
    let ru = q.get("redirect_uri").cloned().unwrap_or_default();
    if !redirects.contains(&ru) {
        return error_page(&format!(
            "The redirect URI {ru} is not registered for {cid}."
        ));
    }
    if q.get("code_challenge_method").map(String::as_str) != Some("S256") {
        return redirect_with(
            &ru,
            &[
                ("error", "invalid_request"),
                ("error_description", "PKCE S256 required"),
            ],
        );
    }
    let session = cookie(&h, "__Host-hmidp-r").filter(|c| s.sessions.contains(c));
    if q.get("prompt").map(String::as_str) == Some("none") && session.is_none() {
        return redirect_with(
            &ru,
            &[
                ("error", "login_required"),
                ("state", q.get("state").map(String::as_str).unwrap_or("")),
            ],
        );
    }
    if session.is_some() {
        return issue_code(&mut s, &q);
    }
    let tx = Mock::rid();
    let csrf = Mock::rid();
    s.txs.insert(tx.clone(), q.clone());
    s.csrf.insert(csrf.clone());
    Html(format!(r#"<html><body><main><h1>Sign in</h1><form method="post" action="/idp/r/login">
      <input type="hidden" name="_csrf" value="{csrf}"><input type="hidden" name="tx" value="{tx}">
      <label>User <input type="text" name="username"></label><label>Password <input type="password" name="password"></label>
      <button type="submit">Sign in</button></form></main></body></html>"#)).into_response()
}

async fn login(State(st): State<S>, Form(f): Form<HashMap<String, String>>) -> Response {
    let mut s = st.lock().unwrap();
    if !s
        .csrf
        .remove(f.get("_csrf").map(String::as_str).unwrap_or(""))
    {
        return error_page("CSRF");
    }
    if f.get("username").map(String::as_str) != Some(USER)
        || f.get("password").map(String::as_str) != Some(PASS)
    {
        let csrf = Mock::rid();
        s.csrf.insert(csrf.clone());
        return Html(format!(r#"<form method="post" action="/idp/r/login"><p>Username or password is incorrect.</p><input type="hidden" name="_csrf" value="{csrf}"><input type="hidden" name="tx" value="{}"><input name="username"><input type="password" name="password"><button>Sign in</button></form>"#, f["tx"])).into_response();
    }
    let sid = Mock::rid();
    s.sessions.insert(sid.clone());
    let csrf = Mock::rid();
    s.csrf.insert(csrf.clone());
    (
        [(header::SET_COOKIE, format!("__Host-hmidp-r={sid}; Path=/; HttpOnly; SameSite=Lax"))],
        Html(format!(r#"<main><h1>Allow app?</h1><form method="post" action="consent"><input type="hidden" name="_csrf" value="{csrf}"><input type="hidden" name="tx" value="{}">
        <input type="checkbox" name="scope" value="openid" checked><button name="decision" value="deny">Deny</button><button name="decision" value="approve">Allow</button></form></main>"#, f["tx"])),
    ).into_response()
}

async fn consent(State(st): State<S>, Form(f): Form<Vec<(String, String)>>) -> Response {
    let mut s = st.lock().unwrap();
    let get = |k: &str| {
        f.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    if !s.csrf.remove(&get("_csrf")) || get("decision") != "approve" {
        return error_page("denied");
    }
    let Some(q) = s.txs.remove(&get("tx")) else {
        return error_page("tx");
    };
    issue_code(&mut s, &q)
}

fn tokens(
    s: &mut Mock,
    client: &str,
    scope: &str,
    gid: &str,
    nonce: Option<&str>,
    aud: &str,
    with_refresh: Option<(String, u32)>,
) -> Value {
    let now = s.now();
    let i = s.issuer();
    let at = s.signer(SigningAlg::ES256).sign_jws("at+jwt", &json!({
        "iss": i, "sub": "u1", "aud": [aud], "exp": now + 3600, "iat": now, "jti": Mock::rid(),
        "client_id": client, "scope": scope, "gid": gid,
    })).unwrap();
    let mut body =
        json!({"access_token": at, "token_type": "Bearer", "expires_in": 3600, "scope": scope});
    if scope.split(' ').any(|t| t == "openid") {
        let id = s.signer(SigningAlg::RS256).sign_jws("JWT", &json!({
            "iss": i, "sub": "u1", "aud": client, "exp": now + 3600, "iat": now, "auth_time": now,
            "nonce": nonce, "at_hash": helios_idp::token::at_hash(&at, SigningAlg::RS256),
        })).unwrap();
        body["id_token"] = json!(id);
    }
    if let Some((fam, generation)) = with_refresh {
        let rt = format!("hmrt_{}", Mock::rid());
        s.refresh.insert(rt.clone(), (fam, generation));
        body["refresh_token"] = json!(rt);
    }
    body
}

fn verify_assertion(s: &mut Mock, f: &HashMap<String, String>) -> bool {
    let Some(a) = f.get("client_assertion") else {
        return false;
    };
    let jwk: jsonwebtoken::jwk::Jwk = serde_json::from_value(s.backend_jwk.clone()).unwrap();
    let key = jsonwebtoken::DecodingKey::from_jwk(&jwk).unwrap();
    let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
    v.set_audience(&[format!("{}/token", s.issuer())]);
    let Ok(d) = jsonwebtoken::decode::<Value>(a, &key, &v) else {
        return false;
    };
    let c = d.claims;
    let now = s.now();
    if c["exp"].as_i64().unwrap_or(0) > now + 300 {
        return false;
    }
    s.jtis.insert(c["jti"].as_str().unwrap_or("").to_string())
}

async fn token(State(st): State<S>, Form(f): Form<HashMap<String, String>>) -> Response {
    let mut s = st.lock().unwrap();
    let err = |s: &Mock, st: StatusCode, e: &str| {
        json_resp(s, st, json!({"error": e, "error_description": e}))
    };
    match f.get("grant_type").map(String::as_str) {
        Some("authorization_code") => {
            let code = f.get("code").cloned().unwrap_or_default();
            let Some((q, gid)) = s.codes.remove(&code) else {
                if let Some(gid) = s.used_codes.get(&code).cloned() {
                    s.revoked_grants.insert(gid);
                }
                return err(&s, StatusCode::BAD_REQUEST, "invalid_grant");
            };
            s.used_codes.insert(code, gid.clone());
            if helios_idp::pkce::verify(
                f.get("code_verifier").map(String::as_str),
                &q["code_challenge"],
            )
            .is_err()
            {
                return err(&s, StatusCode::BAD_REQUEST, "invalid_grant");
            }
            let scope = q["scope"].clone();
            let refresh = scope.contains("offline_access").then(|| {
                let fam = Mock::rid();
                s.families.insert(fam.clone(), (0, false, gid.clone()));
                (fam, 0)
            });
            let aud = q.get("aud").cloned().unwrap_or_default();
            let body = tokens(
                &mut s,
                &q["client_id"],
                &scope,
                &gid,
                q.get("nonce").map(String::as_str),
                &aud,
                refresh,
            );
            json_resp(&s, StatusCode::OK, body)
        }
        Some("refresh_token") => {
            let rt = f.get("refresh_token").cloned().unwrap_or_default();
            let Some((fam, generation)) = s.refresh.get(&rt).cloned() else {
                return err(&s, StatusCode::BAD_REQUEST, "invalid_grant");
            };
            let (cur, revoked, gid) = s.families[&fam].clone();
            if revoked || cur != generation {
                s.families.get_mut(&fam).unwrap().1 = true;
                return err(&s, StatusCode::BAD_REQUEST, "invalid_grant");
            }
            s.families.get_mut(&fam).unwrap().0 = generation + 1;
            let fhir = s.fhir();
            let body = tokens(
                &mut s,
                "diag-public",
                "openid offline_access user/*.rs",
                &gid,
                None,
                &fhir,
                Some((fam, generation + 1)),
            );
            json_resp(&s, StatusCode::OK, body)
        }
        Some("client_credentials") => {
            if !verify_assertion(&mut s, &f) {
                return err(&s, StatusCode::UNAUTHORIZED, "invalid_client");
            }
            let now = s.now();
            let i = s.issuer();
            let fhir = s.fhir();
            let at = s.signer(SigningAlg::ES256).sign_jws("at+jwt", &json!({"iss": i, "sub": "diag-backend", "aud": [fhir], "exp": now + 300, "iat": now, "jti": Mock::rid(), "client_id": "diag-backend", "scope": f["scope"], "gid": Mock::rid()})).unwrap();
            json_resp(
                &s,
                StatusCode::OK,
                json!({"access_token": at, "token_type": "Bearer", "expires_in": 300, "scope": f["scope"]}),
            )
        }
        _ => err(&s, StatusCode::BAD_REQUEST, "unsupported_grant_type"),
    }
}

fn claims_of(s: &Mock, t: &str) -> Option<Value> {
    let p = t.split('.').nth(1)?;
    let c: Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(p)
            .ok()?,
    )
    .ok()?;
    let kid = {
        let h: Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(t.split('.').next()?)
                .ok()?,
        )
        .ok()?;
        h["kid"].as_str()?.to_string()
    };
    if kid != s.es.kid() && kid != s.rs.kid() {
        return None;
    }
    if s.revoked_grants.contains(c["gid"].as_str().unwrap_or("")) {
        return None;
    }
    Some(c)
}

async fn introspect(State(st): State<S>, Form(f): Form<HashMap<String, String>>) -> Response {
    let s = st.lock().unwrap();
    match claims_of(&s, f.get("token").map(String::as_str).unwrap_or("")) {
        Some(c) => json_resp(
            &s,
            StatusCode::OK,
            json!({"active": true, "scope": c["scope"], "client_id": c["client_id"], "exp": c["exp"], "sub": c["sub"]}),
        ),
        None => json_resp(&s, StatusCode::OK, json!({"active": false})),
    }
}

async fn revoke(State(st): State<S>, Form(f): Form<HashMap<String, String>>) -> Response {
    let mut s = st.lock().unwrap();
    if let Some(c) = claims_of(&s, f.get("token").map(String::as_str).unwrap_or("")) {
        s.revoked_grants
            .insert(c["gid"].as_str().unwrap_or("").to_string());
    }
    StatusCode::OK.into_response()
}

fn bearer(h: &HeaderMap) -> String {
    h.get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string()
}

async fn userinfo(State(st): State<S>, h: HeaderMap) -> Response {
    let s = st.lock().unwrap();
    match claims_of(&s, &bearer(&h)) {
        Some(c) => json_resp(&s, StatusCode::OK, json!({"sub": c["sub"]})),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

async fn logout(State(st): State<S>, h: HeaderMap) -> Response {
    let mut s = st.lock().unwrap();
    if let Some(c) = cookie(&h, "__Host-hmidp-r") {
        s.sessions.remove(&c);
    }
    (
        [(
            header::SET_COOKIE,
            "__Host-hmidp-r=; Path=/; Max-Age=0".to_string(),
        )],
        Html("<p>Signed out</p>"),
    )
        .into_response()
}

async fn fhir_read(State(st): State<S>, h: HeaderMap) -> Response {
    let s = st.lock().unwrap();
    let t = bearer(&h);
    if t.is_empty() {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer".to_string())],
        )
            .into_response();
    }
    match claims_of(&s, &t) {
        Some(c) if !s.faults.endpoint_other_issuer && c["iss"] == json!(s.issuer()) => {
            json_resp(&s, StatusCode::OK, json!({"resourceType": "Bundle"}))
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                "Bearer error=\"invalid_token\", error_description=\"untrusted issuer\""
                    .to_string(),
            )],
        )
            .into_response(),
    }
}

async fn fhir_write(State(st): State<S>, h: HeaderMap) -> Response {
    let s = st.lock().unwrap();
    match claims_of(&s, &bearer(&h)) {
        Some(_) => StatusCode::FORBIDDEN.into_response(),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

async fn spawn(faults: Faults) -> (String, S, SigningKey) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let backend = SigningKey::generate(SigningAlg::ES256, RsaBits::B2048).unwrap();
    let mut clients = HashMap::new();
    clients.insert("diag-public".to_string(), (vec![CB.to_string()], true));
    clients.insert(
        "app".to_string(),
        (
            vec!["https://app.example.org/cb".to_string()],
            !faults.app_disabled,
        ),
    );
    let st: S = Arc::new(Mutex::new(Mock {
        base: base.clone(),
        faults,
        es: SigningKey::generate(SigningAlg::ES256, RsaBits::B2048).unwrap(),
        rs: SigningKey::generate(SigningAlg::RS256, RsaBits::B2048).unwrap(),
        stray: SigningKey::generate(SigningAlg::ES256, RsaBits::B2048).unwrap(),
        backend_jwk: backend.public_jwk().clone(),
        csrf: HashSet::new(),
        txs: HashMap::new(),
        sessions: HashSet::new(),
        codes: HashMap::new(),
        used_codes: HashMap::new(),
        revoked_grants: HashSet::new(),
        refresh: HashMap::new(),
        families: HashMap::new(),
        jtis: HashSet::new(),
        clients,
    }));
    let app = Router::new()
        .route("/idp/r/.well-known/openid-configuration", get(oidc))
        .route("/.well-known/oauth-authorization-server/idp/r", get(oidc))
        .route("/idp/r/jwks", get(jwks))
        .route("/idp/r/authorize", get(authorize))
        .route("/idp/r/login", post(login))
        .route("/idp/r/consent", post(consent))
        .route("/idp/r/token", post(token))
        .route("/idp/r/introspect", post(introspect))
        .route("/idp/r/revoke", post(revoke))
        .route("/idp/r/userinfo", get(userinfo))
        .route("/idp/r/logout", get(logout))
        .route("/fhir/ep1/.well-known/smart-configuration", get(smart))
        .route("/fhir/ep1/Patient", get(fhir_read).post(fhir_write))
        .route(
            "/fhir/ep1/Patient/{id}",
            get(|s: State<S>, h: HeaderMap, _p: Path<String>| fhir_read(s, h)),
        )
        .with_state(st.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, st, backend)
}

fn config(base: &str, backend: SigningKey) -> RunConfig {
    let fixture = Fixture {
        public_client_id: "diag-public".into(),
        redirect_uri: CB.into(),
        backend_client_id: Some("diag-backend".into()),
        backend_key: Some(Arc::new(backend)),
        backend_scope: "system/*.rs".into(),
        username: USER.into(),
        password: PASS.into(),
        patient: None,
        scopes: "openid fhirUser offline_access user/*.rs".into(),
    };
    let mut cfg = RunConfig::new("r", &format!("{base}/idp/r"), fixture);
    cfg.endpoints = vec![EndpointView {
        name: "ep1".into(),
        base_url: format!("{base}/fhir/ep1"),
        auth_mode: "full".into(),
        provider: EndpointProvider::Internal { realm: "r".into() },
        fhir_version: "R4".into(),
    }];
    cfg.revocation_wait = std::time::Duration::from_millis(500);
    cfg
}

async fn run(faults: Faults, app: Option<AppProbe>) -> diagnostic::Report {
    let (base, _st, backend) = spawn(faults).await;
    let mut cfg = config(&base, backend);
    cfg.app = app;
    let mut report = diagnostic::Report::new("r", &cfg.issuer);
    diagnostic::runner::run(cfg, &mut report).await;
    report.finish();
    report
}

fn fails(r: &diagnostic::Report) -> Vec<(String, String)> {
    r.problems()
        .iter()
        .map(|c| (c.id.clone(), c.code.clone()))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn healthy_realm_passes_every_step() {
    let r = run(
        Faults::default(),
        Some(AppProbe {
            client_id: "app".into(),
            redirect_uri: "https://app.example.org/cb".into(),
        }),
    )
    .await;
    let ids: Vec<&str> = r.steps.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "discovery",
            "jwks",
            "authorization-code",
            "negative",
            "fhir",
            "refresh",
            "backend",
            "introspection",
            "revocation",
            "userinfo-logout"
        ]
    );
    assert!(fails(&r).is_empty(), "{:#?}", r.problems());
    assert!(r.warnings().is_empty(), "{:#?}", r.warnings());
    assert_eq!(r.status, Status::Pass);
    for id in [
        "negative.replay-revokes",
        "refresh.family",
        "backend.replay",
        "backend.exp",
        "revocation.fhir",
        "userinfo-logout.logout",
    ] {
        assert_eq!(r.check(id).map(|c| c.status), Some(Status::Pass), "{id}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_redirect_uri_is_pinpointed() {
    let r = run(
        Faults::default(),
        Some(AppProbe {
            client_id: "app".into(),
            redirect_uri: "https://app.example.org/callback".into(),
        }),
    )
    .await;
    let c = r.check("authorization-code.app.app").unwrap();
    assert_eq!(c.status, Status::Fail);
    assert_eq!(c.code, "app-authorize-refused");
    assert_eq!(c.field.as_deref(), Some("client.app.redirectUris"));
    assert!(c.message.contains("not registered"), "{}", c.message);
    // Only that check fails.
    assert_eq!(fails(&r).len(), 1, "{:#?}", r.problems());
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_client_is_pinpointed() {
    let r = run(
        Faults {
            app_disabled: true,
            ..Default::default()
        },
        Some(AppProbe {
            client_id: "app".into(),
            redirect_uri: "https://app.example.org/cb".into(),
        }),
    )
    .await;
    let c = r.check("authorization-code.app.app").unwrap();
    assert_eq!(c.code, "app-authorize-refused");
    assert!(c.message.contains("disabled"), "{}", c.message);
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_key_is_pinpointed() {
    let r = run(
        Faults {
            unpublished_signer: true,
            ..Default::default()
        },
        None,
    )
    .await;
    let c = r.check("authorization-code.access-token").unwrap();
    assert_eq!(c.code, "access-token-signature");
    assert_eq!(c.field.as_deref(), Some("keys"));
    assert!(
        c.message.contains("not in the published JWKS"),
        "{}",
        c.message
    );
    assert_eq!(
        r.check("authorization-code.id-token").unwrap().code,
        "id-token-signature"
    );
    // The resource server refuses these tokens too.
    assert_eq!(r.check("fhir.ep1.read").unwrap().code, "fhir-token-refused");
}

#[tokio::test(flavor = "multi_thread")]
async fn endpoint_not_bound_is_pinpointed() {
    let r = run(
        Faults {
            endpoint_other_issuer: true,
            ..Default::default()
        },
        None,
    )
    .await;
    let c = r.check("discovery.smart.ep1").unwrap();
    assert_eq!(c.code, "endpoint-smart-other-issuer");
    assert_eq!(c.field.as_deref(), Some("endpoints.ep1"));
    let f = r.check("fhir.ep1.read").unwrap();
    assert_eq!(f.code, "fhir-token-refused");
    assert_eq!(f.field.as_deref(), Some("endpoints.ep1"));
    // The first problem listed is the cause.
    assert_eq!(r.problems()[0].code, "endpoint-smart-other-issuer");
}

#[tokio::test(flavor = "multi_thread")]
async fn clock_skew_is_pinpointed() {
    let r = run(
        Faults {
            clock_offset: 600,
            ..Default::default()
        },
        None,
    )
    .await;
    let c = r.check("discovery.clock").unwrap();
    assert_eq!(c.code, "clock-skew");
    assert_eq!(c.status, Status::Fail);
    assert!(c.params["skew"].parse::<i64>().unwrap() >= 590);
    assert_eq!(
        r.check("authorization-code.clock").unwrap().code,
        "clock-skew-token"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_server_skips_dependants() {
    let fixture_key = SigningKey::generate(SigningAlg::ES256, RsaBits::B2048).unwrap();
    let cfg = config("http://127.0.0.1:9", fixture_key);
    let mut report = diagnostic::Report::new("r", &cfg.issuer);
    diagnostic::runner::run(cfg, &mut report).await;
    report.finish();
    assert_eq!(report.status, Status::Fail);
    assert_eq!(report.check("discovery.oidc").unwrap().code, "net-connect");
    assert!(
        report.steps[1..]
            .iter()
            .all(|s| s.status == Status::Skipped)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn preflight_and_live_together() {
    use helios_idp::realm::{RealmName, RealmSpec};
    let (base, _st, backend) = spawn(Faults::default()).await;
    let cfg = config(&base, backend);
    let mut realm = RealmSpec::new(RealmName::parse("r").unwrap(), "R");
    realm.endpoints = vec!["ep1".into()];
    let url = url::Url::parse(&base).unwrap();
    let eps = cfg.endpoints.clone();
    let pre = PreflightInput {
        realm: &realm,
        public_base: &url,
        clients: &[],
        keys: &[],
        endpoints: &eps,
        app: None,
        now: chrono::Utc::now(),
    };
    let r = diagnostic::diagnose(&pre, cfg).await;
    assert_eq!(r.steps[0].id, "preflight");
    // No keys in the stored config → pinpointed in step 0 even though the
    // live mock signs fine.
    assert!(r.problems().iter().any(|c| c.code == "key-missing"));
    let j = serde_json::to_value(&r).unwrap();
    assert_eq!(j["steps"].as_array().unwrap().len(), 11);
    let _ = thumbprint(&json!({}));
}

#[tokio::test(flavor = "multi_thread")]
async fn rate_limits_are_waited_out_and_reported() {
    let r = run(
        Faults {
            rate_limit_first: 2,
            ..Default::default()
        },
        None,
    )
    .await;
    assert!(fails(&r).is_empty(), "{:#?}", r.problems());
    assert_eq!(
        r.check("authorization-code.rate").map(|c| c.code.as_str()),
        Some("rate-waited")
    );
}
