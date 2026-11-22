//! Browser-shaped flows through the real protocol server and these pages
//! (docs/design/authorization-server.md §8.2–§8.6, §10.4; packages I4/I8):
//! `/authorize` → sign-in → patient picker → consent → code → `/token`,
//! with a hand-rolled cookie jar per "browser" so leaked-handle and CSRF
//! cases can use a second browser against the same server.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum_test::{TestResponse, TestServer};
use helios_endpoints::SecretKeyRing;
use helios_idp::admin::{IdpAdmin, LaunchRequest};
use helios_idp::client::ClientSpec;
use helios_idp::events::{IdpEvent, MemoryEventSink};
use helios_idp::launch::LaunchContext;
use helios_idp::memory::MemoryStore;
use helios_idp::realm::{RealmName, RealmSpec};
use helios_idp::runtime::{ConventionalEndpointUrls, IdpRuntime};
use helios_idp::store::IdpStore;
use helios_idp::user::RealmUser;
use helios_idp_pages::csrf::CsrfKey;
use helios_idp_pages::directory::{MemoryPatientDirectory, PatientSummary};
use helios_idp_pages::lockout::Lockout;
use helios_idp_pages::{Pages, PagesConfig};
use helios_idp_server::{IdpServer, IdpServerOptions};
use serde_json::{Value, json};

const ORIGIN: &str = "http://localhost";
const CB: &str = "https://app.example/cb";
const AUD: &str = "http://localhost/fhir/lab";

struct Env {
    server: TestServer,
    rt: Arc<IdpRuntime>,
    realm_id: String,
    events: Arc<MemoryEventSink>,
}

fn patient(id: &str, name: &str, born: &str) -> PatientSummary {
    PatientSummary {
        id: id.into(),
        name: name.into(),
        birth_date: Some(born.into()),
        gender: None,
    }
}

async fn env_with(branding: Value) -> Env {
    let store: Arc<dyn IdpStore> = Arc::new(MemoryStore::new());
    let ring = Arc::new(SecretKeyRing::parse(&SecretKeyRing::generate_spec("k1")).unwrap());
    let rt = IdpRuntime::new(
        store,
        Some(ring),
        url::Url::parse(ORIGIN).unwrap(),
        Arc::new(ConventionalEndpointUrls {
            public_base: ORIGIN.into(),
        }),
    );
    let events = Arc::new(MemoryEventSink::default());
    rt.set_event_sink(events.clone());
    let admin = IdpAdmin::new(rt.clone());
    let mut spec = RealmSpec::new(RealmName::parse("clinic").unwrap(), "Riverside Clinic");
    spec.endpoints = vec!["lab".into()];
    spec.branding = branding;
    spec.security.lockout_threshold = 3;
    let realm = admin.create_realm(spec).await.unwrap();
    let client: ClientSpec = serde_json::from_value(json!({
        "clientId": "app", "name": "Growth Chart",
        "auth": {"method": "none"},
        "grantTypes": ["authorization_code", "refresh_token"],
        "redirectUris": [CB],
        "postLogoutRedirectUris": ["https://app.example/bye"],
        "launchTypes": ["standalone", "ehr"],
        "scopes": {"allowed": ["openid", "fhirUser", "profile", "launch", "launch/patient",
                    "patient/*.rs", "user/*.rs", "offline_access"], "acceptV1": true},
        "trusted": false
    }))
    .unwrap();
    admin.create_client(&realm.id, client).await.unwrap();
    let user = |id: &str, name: &str, kind: &str, links: Value| -> RealmUser {
        serde_json::from_value(json!({
            "id": id, "username": name, "kind": kind, "displayName": name,
            "email": null, "emailVerified": false, "links": links,
            "externalSubject": null, "disabled": false
        }))
        .unwrap()
    };
    for (u, pw) in [
        (
            user(
                "u-alice",
                "alice",
                "patient",
                json!([{"endpoint": "lab", "reference": "Patient/p1", "isFhirUser": true}]),
            ),
            "alice-password-1",
        ),
        (
            user(
                "u-carol",
                "carol",
                "related-person",
                json!([{"endpoint": "lab", "reference": "RelatedPerson/rp1", "isFhirUser": true,
                        "accessiblePatients": ["p1", "p2"]}]),
            ),
            "carol-password-1",
        ),
        (
            user(
                "u-bob",
                "drbob",
                "practitioner",
                json!([{"endpoint": "lab", "reference": "Practitioner/dr1", "isFhirUser": true}]),
            ),
            "drbob-password-1",
        ),
    ] {
        admin
            .create_user(&realm.id, u, Some(pw), false)
            .await
            .unwrap();
    }
    let server = IdpServer::new(rt.clone(), IdpServerOptions::default());
    let mut patients = HashMap::new();
    patients.insert(
        "lab".to_string(),
        vec![
            patient("p1", "Amy Shaw", "1980-04-01"),
            patient("p2", "Ben Shaw", "2015-09-12"),
            patient("p3", "Cleo Ortiz", "1955-01-30"),
        ],
    );
    let pages = Pages::new(
        rt.clone(),
        server.completer(),
        Arc::new(MemoryPatientDirectory { patients }),
        Lockout::new(Arc::new(
            helios_admin_auth::store::testing::MemoryStore::default(),
        )),
        CsrfKey::random(),
        PagesConfig {
            public_origin: ORIGIN.into(),
            secure_cookies: false,
            trust_forwarded: false,
        },
    );
    let app = server.router().merge(helios_idp_pages::router(pages));
    Env {
        server: TestServer::new(app).unwrap(),
        rt,
        realm_id: realm.id,
        events,
    }
}

async fn env() -> Env {
    env_with(Value::Null).await
}

/// One browser: a cookie jar.
#[derive(Default, Clone)]
struct Browser {
    jar: BTreeMap<String, String>,
}

impl Browser {
    fn cookie_header(&self) -> Option<HeaderValue> {
        if self.jar.is_empty() {
            return None;
        }
        let s: Vec<String> = self.jar.iter().map(|(k, v)| format!("{k}={v}")).collect();
        HeaderValue::from_str(&s.join("; ")).ok()
    }

    fn absorb(&mut self, r: &TestResponse) {
        for v in r.headers().get_all(header::SET_COOKIE) {
            let s = v.to_str().unwrap();
            let first = s.split(';').next().unwrap();
            let (k, val) = first.split_once('=').unwrap();
            if val.is_empty() || s.contains("Max-Age=0") {
                self.jar.remove(k);
            } else {
                self.jar.insert(k.to_string(), val.to_string());
            }
        }
    }

    async fn get(&mut self, s: &TestServer, path: &str) -> TestResponse {
        let mut req = s.get(path);
        if let Some(c) = self.cookie_header() {
            req = req.add_header(header::COOKIE, c);
        }
        let r = req.await;
        self.absorb(&r);
        r
    }

    async fn post(&mut self, s: &TestServer, path: &str, body: &[(&str, &str)]) -> TestResponse {
        self.post_origin(s, path, body, Some(ORIGIN)).await
    }

    async fn post_origin(
        &mut self,
        s: &TestServer,
        path: &str,
        body: &[(&str, &str)],
        origin: Option<&str>,
    ) -> TestResponse {
        let encoded: String = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(body.iter())
            .finish();
        let mut req = s
            .post(path)
            .text(encoded)
            .content_type("application/x-www-form-urlencoded");
        if let Some(o) = origin {
            req = req.add_header(header::ORIGIN, HeaderValue::from_str(o).unwrap());
        }
        if let Some(c) = self.cookie_header() {
            req = req.add_header(header::COOKIE, c);
        }
        let r = req.await;
        self.absorb(&r);
        r
    }
}

fn location(r: &TestResponse) -> String {
    r.maybe_header(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

/// Path + query of an absolute URL on this origin.
fn local(url: &str) -> String {
    url.strip_prefix(ORIGIN).unwrap_or(url).to_string()
}

fn field(html: &str, name: &str) -> String {
    let pat = format!("name=\"{name}\" value=\"");
    let start = html
        .find(&pat)
        .unwrap_or_else(|| panic!("no {name} in {html}"))
        + pat.len();
    html[start..start + html[start..].find('"').unwrap()]
        .replace("&#x2b;", "+")
        .replace("&#43;", "+")
}

fn query_of(url: &str) -> HashMap<String, String> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk-long-enough-verifier";

fn authorize_url(scope: &str, extra: &[(&str, &str)]) -> String {
    let challenge = helios_idp::pkce::s256_challenge(VERIFIER);
    let mut q = form_urlencoded::Serializer::new(String::new());
    q.extend_pairs([
        ("response_type", "code"),
        ("client_id", "app"),
        ("redirect_uri", CB),
        ("scope", scope),
        ("state", "st-123"),
        ("aud", AUD),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
    ]);
    q.extend_pairs(extra.iter());
    format!("/idp/clinic/authorize?{}", q.finish())
}

/// `/authorize` → the login page URL (path + query).
async fn start(e: &Env, b: &mut Browser, scope: &str, extra: &[(&str, &str)]) -> String {
    let r = b.get(&e.server, &authorize_url(scope, extra)).await;
    assert_eq!(r.status_code(), StatusCode::SEE_OTHER, "{}", r.text());
    local(&location(&r))
}

async fn login(e: &Env, b: &mut Browser, login_path: &str, user: &str, pw: &str) -> TestResponse {
    let page = b.get(&e.server, login_path).await;
    assert_eq!(page.status_code(), StatusCode::OK, "{}", page.text());
    let html = page.text();
    let ix = field(&html, "ix");
    let csrf = field(&html, "csrf");
    b.post(
        &e.server,
        "/idp/clinic/login",
        &[
            ("ix", &ix),
            ("csrf", &csrf),
            ("username", user),
            ("password", pw),
        ],
    )
    .await
}

async fn token(e: &Env, code: &str) -> Value {
    let r = e
        .server
        .post("/idp/clinic/token")
        .text(
            form_urlencoded::Serializer::new(String::new())
                .extend_pairs([
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    ("redirect_uri", CB),
                    ("client_id", "app"),
                    ("code_verifier", VERIFIER),
                ])
                .finish(),
        )
        .content_type("application/x-www-form-urlencoded")
        .await;
    assert_eq!(r.status_code(), StatusCode::OK, "{}", r.text());
    r.json()
}

fn claims(jwt: &str) -> Value {
    use base64::Engine;
    let p = jwt.split('.').nth(1).unwrap();
    serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(p)
            .unwrap(),
    )
    .unwrap()
}

/// Consent page → allow (all boxes ticked unless `scopes` given).
async fn allow(e: &Env, b: &mut Browser, consent_path: &str, remember: bool) -> TestResponse {
    let page = b.get(&e.server, consent_path).await;
    assert_eq!(page.status_code(), StatusCode::OK, "{}", page.text());
    let html = page.text();
    let ix = field(&html, "ix");
    let csrf = field(&html, "csrf");
    let mut body: Vec<(String, String)> = vec![
        ("ix".into(), ix),
        ("csrf".into(), csrf),
        ("decision".into(), "allow".into()),
    ];
    for part in html.split("name=\"scope\"").skip(1) {
        let v = &part[part.find("value=\"").unwrap() + 7..];
        body.push(("scope".into(), v[..v.find('"').unwrap()].to_string()));
    }
    if remember {
        body.push(("remember".into(), "1".into()));
    }
    let refs: Vec<(&str, &str)> = body.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    b.post(&e.server, "/idp/clinic/consent", &refs).await
}

const PATIENT_SCOPE: &str = "openid fhirUser launch/patient patient/*.rs offline_access";

// ---------------------------------------------------------------- flows

#[tokio::test]
async fn standalone_patient_launch_with_consent_issues_tokens_with_context() {
    let e = env().await;
    let mut b = Browser::default();
    let login_path = start(&e, &mut b, PATIENT_SCOPE, &[("nonce", "n-1")]).await;
    assert!(
        login_path.starts_with("/idp/clinic/login?ix=hmix_"),
        "{login_path}"
    );
    let r = login(&e, &mut b, &login_path, "alice", "alice-password-1").await;
    // One linked patient: no picker, straight to consent.
    assert_eq!(r.status_code(), StatusCode::SEE_OTHER, "{}", r.text());
    let consent = location(&r);
    assert!(consent.starts_with("/idp/clinic/consent?ix="), "{consent}");
    let page = b.get(&e.server, &consent).await;
    let html = page.text();
    assert!(
        html.contains("Allow Growth Chart to access your data?"),
        "{html}"
    );
    assert!(html.contains("app.example"));
    assert!(html.contains("Patient: Amy Shaw"));
    assert!(html.contains("Read all data"));
    let r = allow(&e, &mut b, &consent, false).await;
    assert_eq!(r.status_code(), StatusCode::SEE_OTHER, "{}", r.text());
    let cb = location(&r);
    assert!(cb.starts_with(CB), "{cb}");
    let q = query_of(&cb);
    assert_eq!(q["state"], "st-123");
    assert_eq!(q["iss"], "http://localhost/idp/clinic");
    let t = token(&e, &q["code"]).await;
    assert_eq!(t["patient"], "p1");
    assert_eq!(t["need_patient_banner"], true);
    assert!(t["refresh_token"].is_string());
    let at = claims(t["access_token"].as_str().unwrap());
    assert_eq!(at["sub"], "u-alice");
    assert_eq!(at["patient"], "p1");
    assert_eq!(at["aud"], json!([AUD]));
    assert_eq!(at["fhirUser"], "http://localhost/fhir/lab/Patient/p1");
    let id = claims(t["id_token"].as_str().unwrap());
    assert_eq!(id["nonce"], "n-1");
    assert_eq!(id["fhirUser"], "http://localhost/fhir/lab/Patient/p1");

    let ev = e.events.take();
    assert!(
        ev.iter()
            .any(|x| matches!(x, IdpEvent::LoginSucceeded { user_id, .. } if user_id == "u-alice"))
    );
    assert!(
        ev.iter()
            .any(|x| matches!(x, IdpEvent::ConsentGranted { client_id, .. } if client_id == "app"))
    );
    assert!(
        ev.iter()
            .any(|x| matches!(x, IdpEvent::TokenIssued { subject, .. } if subject == "u-alice"))
    );
}

#[tokio::test]
async fn sso_reuses_the_session_and_remembered_consent_skips_the_screen_until_revoked() {
    let e = env().await;
    let mut b = Browser::default();
    let lp = start(&e, &mut b, PATIENT_SCOPE, &[]).await;
    let r = login(&e, &mut b, &lp, "alice", "alice-password-1").await;
    let r = allow(&e, &mut b, &location(&r), true).await;
    let code1 = query_of(&location(&r))["code"].clone();
    let t1 = token(&e, &code1).await;
    let gid = claims(t1["access_token"].as_str().unwrap())["gid"]
        .as_str()
        .unwrap()
        .to_string();

    // Second authorization in the same browser: no form, no consent.
    let lp = start(&e, &mut b, PATIENT_SCOPE, &[]).await;
    let r = b.get(&e.server, &lp).await;
    assert_eq!(r.status_code(), StatusCode::SEE_OTHER, "{}", r.text());
    assert!(
        location(&r).starts_with(CB),
        "consent remembered: {}",
        location(&r)
    );

    // Connected apps: listed; removing access deletes the consent and
    // revokes the grant at once.
    let apps = b.get(&e.server, "/idp/clinic/account/apps").await;
    let html = apps.text();
    assert!(html.contains("Growth Chart"), "{html}");
    let csrf = field(&html, "csrf");
    let r = b
        .post(
            &e.server,
            "/idp/clinic/account/apps",
            &[("csrf", &csrf), ("client_id", "app"), ("endpoint", "lab")],
        )
        .await;
    assert_eq!(r.status_code(), StatusCode::SEE_OTHER);
    let after = b.get(&e.server, &location(&r)).await.text();
    assert!(
        after.contains("Access for Growth Chart was removed."),
        "{after}"
    );
    assert!(after.contains("You have not connected any apps."));
    assert!(
        e.rt.realm("clinic").unwrap().grant_revoked(&gid),
        "immediate revocation"
    );
    let store = e.rt.store();
    assert!(
        store
            .get_consent(&e.realm_id, "u-alice", "app", "lab")
            .await
            .unwrap()
            .is_none()
    );

    // Consent is asked again.
    let lp = start(&e, &mut b, PATIENT_SCOPE, &[]).await;
    let r = b.get(&e.server, &lp).await;
    assert!(
        location(&r).starts_with("/idp/clinic/consent"),
        "{}",
        location(&r)
    );
    let ev = e.events.take();
    assert!(
        ev.iter()
            .any(|x| matches!(x, IdpEvent::ConsentWithdrawn { .. }))
    );
    assert!(
        ev.iter()
            .any(|x| matches!(x, IdpEvent::TokenRevoked { gid: g, .. } if *g == gid))
    );
}

#[tokio::test]
async fn unticked_scopes_are_not_granted_and_deny_returns_access_denied() {
    let e = env().await;
    let mut b = Browser::default();
    let lp = start(&e, &mut b, PATIENT_SCOPE, &[]).await;
    let r = login(&e, &mut b, &lp, "alice", "alice-password-1").await;
    let consent = location(&r);
    let html = b.get(&e.server, &consent).await.text();
    let (ix, csrf) = (field(&html, "ix"), field(&html, "csrf"));
    // Only patient/*.rs ticked (offline_access unticked).
    let r = b
        .post(
            &e.server,
            "/idp/clinic/consent",
            &[
                ("ix", &ix),
                ("csrf", &csrf),
                ("decision", "allow"),
                ("scope", "patient/*.rs"),
                ("scope", "system/*.cruds"),
            ],
        )
        .await;
    let t = token(&e, &query_of(&location(&r))["code"]).await;
    let granted: Vec<&str> = t["scope"].as_str().unwrap().split(' ').collect();
    assert!(granted.contains(&"patient/*.rs"));
    assert!(!granted.contains(&"offline_access"), "{granted:?}");
    assert!(
        !granted.contains(&"system/*.cruds"),
        "never more than requested"
    );
    assert!(t.get("refresh_token").is_none());

    let lp = start(&e, &mut b, PATIENT_SCOPE, &[]).await;
    let r = b.get(&e.server, &lp).await;
    let consent = location(&r);
    let html = b.get(&e.server, &consent).await.text();
    let r = b
        .post(
            &e.server,
            "/idp/clinic/consent",
            &[
                ("ix", &field(&html, "ix")),
                ("csrf", &field(&html, "csrf")),
                ("decision", "deny"),
            ],
        )
        .await;
    let q = query_of(&location(&r));
    assert_eq!(q["error"], "access_denied");
    assert_eq!(q["state"], "st-123");
    assert!(!q.contains_key("code"));
}

#[tokio::test]
async fn caregiver_picks_from_linked_patients_only() {
    let e = env().await;
    let mut b = Browser::default();
    let lp = start(&e, &mut b, PATIENT_SCOPE, &[]).await;
    let r = login(&e, &mut b, &lp, "carol", "carol-password-1").await;
    let picker = location(&r);
    assert!(
        picker.starts_with("/idp/clinic/select-patient?ix="),
        "{picker}"
    );
    let html = b.get(&e.server, &picker).await.text();
    assert!(
        html.contains("Amy Shaw") && html.contains("Ben Shaw"),
        "{html}"
    );
    assert!(!html.contains("Cleo Ortiz"), "unlinked patient not offered");
    assert!(!html.contains("idp-q"), "caregivers do not search");
    let (ix, csrf) = (field(&html, "ix"), field(&html, "csrf"));
    // A patient outside the linked set is refused.
    let r = b
        .post(
            &e.server,
            "/idp/clinic/select-patient",
            &[("ix", &ix), ("csrf", &csrf), ("patient", "p3")],
        )
        .await;
    assert_eq!(r.status_code(), StatusCode::OK);
    assert!(r.text().contains("Choose one of the patients listed."));
    let r = b
        .post(
            &e.server,
            "/idp/clinic/select-patient",
            &[("ix", &ix), ("csrf", &csrf), ("patient", "p2")],
        )
        .await;
    let consent = location(&r);
    assert!(consent.starts_with("/idp/clinic/consent"), "{consent}");
    assert!(
        b.get(&e.server, &consent)
            .await
            .text()
            .contains("Patient: Ben Shaw")
    );
    let r = allow(&e, &mut b, &consent, false).await;
    let t = token(&e, &query_of(&location(&r))["code"]).await;
    assert_eq!(t["patient"], "p2");
    assert_eq!(
        claims(t["access_token"].as_str().unwrap())["fhirUser"],
        "http://localhost/fhir/lab/RelatedPerson/rp1"
    );
}

#[tokio::test]
async fn practitioner_searches_patients_and_gets_fhir_user() {
    let e = env().await;
    let mut b = Browser::default();
    let lp = start(&e, &mut b, "openid fhirUser launch/patient user/*.rs", &[]).await;
    let r = login(&e, &mut b, &lp, "drbob", "drbob-password-1").await;
    let picker = location(&r);
    let html = b.get(&e.server, &format!("{picker}&q=Ortiz")).await.text();
    assert!(
        html.contains("Cleo Ortiz") && !html.contains("Amy Shaw"),
        "{html}"
    );
    let html = b
        .get(&e.server, &format!("{picker}&q=1980-04-01"))
        .await
        .text();
    assert!(html.contains("Amy Shaw") && !html.contains("Cleo Ortiz"));
    let (ix, csrf) = (field(&html, "ix"), field(&html, "csrf"));
    let r = b
        .post(
            &e.server,
            "/idp/clinic/select-patient",
            &[("ix", &ix), ("csrf", &csrf), ("patient", "nope")],
        )
        .await;
    assert!(r.text().contains("Choose one of the patients listed."));
    let r = b
        .post(
            &e.server,
            "/idp/clinic/select-patient",
            &[("ix", &ix), ("csrf", &csrf), ("patient", "p3")],
        )
        .await;
    let r = allow(&e, &mut b, &location(&r), false).await;
    let t = token(&e, &query_of(&location(&r))["code"]).await;
    assert_eq!(t["patient"], "p3");
    assert_eq!(
        claims(t["id_token"].as_str().unwrap())["fhirUser"],
        "http://localhost/fhir/lab/Practitioner/dr1"
    );
}

#[tokio::test]
async fn ehr_launch_carries_the_launch_context_without_a_picker() {
    let e = env().await;
    let admin = IdpAdmin::new(e.rt.clone());
    let launch = admin
        .create_launch(
            &e.realm_id,
            LaunchRequest {
                client_id: Some("app".into()),
                endpoint: "lab".into(),
                launched_by: None,
                context: LaunchContext {
                    patient: Some("p3".into()),
                    encounter: Some("enc-9".into()),
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(launch.iss, AUD);
    let mut b = Browser::default();
    let lp = start(
        &e,
        &mut b,
        "openid fhirUser launch user/*.rs",
        &[("launch", &launch.launch)],
    )
    .await;
    let r = login(&e, &mut b, &lp, "drbob", "drbob-password-1").await;
    let next = location(&r);
    assert!(next.starts_with("/idp/clinic/consent"), "no picker: {next}");
    let r = allow(&e, &mut b, &next, false).await;
    let t = token(&e, &query_of(&location(&r))["code"]).await;
    assert_eq!(t["patient"], "p3");
    assert_eq!(t["encounter"], "enc-9");
    // The handle is single use.
    let r = b
        .get(
            &e.server,
            &authorize_url("openid launch user/*.rs", &[("launch", &launch.launch)]),
        )
        .await;
    assert!(
        location(&r).contains("error=invalid_request"),
        "{}",
        location(&r)
    );
}

#[tokio::test]
async fn prompt_none_without_a_session_redirects_login_required() {
    let e = env().await;
    let mut b = Browser::default();
    let lp = start(&e, &mut b, PATIENT_SCOPE, &[("prompt", "none")]).await;
    let r = b.get(&e.server, &lp).await;
    let q = query_of(&location(&r));
    assert_eq!(q["error"], "login_required");
    assert_eq!(q["state"], "st-123");
}

#[tokio::test]
async fn wrong_passwords_are_generic_and_lock_the_account() {
    let e = env().await;
    let mut b = Browser::default();
    let lp = start(&e, &mut b, PATIENT_SCOPE, &[]).await;
    let ghost = login(&e, &mut b, &lp, "nobody", "whatever-password").await;
    let wrong = login(&e, &mut b, &lp, "alice", "wrong-password").await;
    assert_eq!(ghost.status_code(), StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.status_code(), StatusCode::UNAUTHORIZED);
    let msg = "The user name or password is incorrect.";
    assert!(ghost.text().contains(msg) && wrong.text().contains(msg));
    login(&e, &mut b, &lp, "alice", "wrong-password").await;
    let third = login(&e, &mut b, &lp, "alice", "wrong-password").await;
    assert!(
        third.text().contains("Too many failed attempts"),
        "{}",
        third.text()
    );
    // Locked: even the right password is refused.
    let right = login(&e, &mut b, &lp, "alice", "alice-password-1").await;
    assert_eq!(right.status_code(), StatusCode::UNAUTHORIZED);
    assert!(right.text().contains("Too many failed attempts"));
    // Another account is unaffected.
    let mut b2 = Browser::default();
    let lp2 = start(&e, &mut b2, PATIENT_SCOPE, &[]).await;
    let ok = login(&e, &mut b2, &lp2, "carol", "carol-password-1").await;
    assert_eq!(ok.status_code(), StatusCode::SEE_OTHER);
    let ev = e.events.take();
    let failed = ev
        .iter()
        .filter(|x| matches!(x, IdpEvent::LoginFailed { .. }))
        .count();
    assert!(failed >= 5, "{ev:?}");
    assert!(ev.iter().any(
        |x| matches!(x, IdpEvent::LoginFailed { reason, .. } if reason.starts_with("locked"))
    ));
    assert!(
        !format!("{ev:?}").contains("wrong-password"),
        "no password in events"
    );
}

#[tokio::test]
async fn csrf_and_origin_are_enforced() {
    let e = env().await;
    let mut b = Browser::default();
    let lp = start(&e, &mut b, PATIENT_SCOPE, &[]).await;
    let html = b.get(&e.server, &lp).await.text();
    let (ix, csrf) = (field(&html, "ix"), field(&html, "csrf"));
    fn with<'a>(ix: &'a str, c: &'a str) -> Vec<(&'a str, &'a str)> {
        vec![
            ("ix", ix),
            ("csrf", c),
            ("username", "alice"),
            ("password", "alice-password-1"),
        ]
    }
    // Missing token.
    let r = b.post(&e.server, "/idp/clinic/login", &with(&ix, "")).await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN);
    // Cross-site Origin with a valid token.
    let r = b
        .post_origin(
            &e.server,
            "/idp/clinic/login",
            &with(&ix, &csrf),
            Some("https://evil.example"),
        )
        .await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN);
    // Another browser (no binding cookie) replaying the token.
    let mut attacker = Browser::default();
    let r = attacker
        .post(&e.server, "/idp/clinic/login", &with(&ix, &csrf))
        .await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN);
    // Token of another interaction.
    let lp2 = start(&e, &mut b, PATIENT_SCOPE, &[]).await;
    let other_csrf = field(&b.get(&e.server, &lp2).await.text(), "csrf");
    let r = b
        .post(&e.server, "/idp/clinic/login", &with(&ix, &other_csrf))
        .await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN);
    // The real thing works.
    let r = b
        .post(&e.server, "/idp/clinic/login", &with(&ix, &csrf))
        .await;
    assert_eq!(r.status_code(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn a_leaked_interaction_cannot_be_finished_in_another_browser() {
    let e = env().await;
    let mut victim = Browser::default();
    let lp = start(&e, &mut victim, PATIENT_SCOPE, &[]).await;
    let r = login(&e, &mut victim, &lp, "alice", "alice-password-1").await;
    let consent = location(&r);
    let mut attacker = Browser::default();
    let r = attacker.get(&e.server, &consent).await;
    assert_eq!(r.status_code(), StatusCode::SEE_OTHER);
    assert!(
        location(&r).starts_with("/idp/clinic/login"),
        "{}",
        location(&r)
    );
    // Even with the victim's CSRF token.
    let html = victim.get(&e.server, &consent).await.text();
    let r = attacker
        .post(
            &e.server,
            "/idp/clinic/consent",
            &[
                ("ix", &field(&html, "ix")),
                ("csrf", &field(&html, "csrf")),
                ("decision", "allow"),
            ],
        )
        .await;
    assert_ne!(r.status_code(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn unknown_or_foreign_interactions_render_an_error_page_without_redirect() {
    let e = env().await;
    let mut b = Browser::default();
    for path in [
        "/idp/clinic/login?ix=hmix_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "/idp/clinic/login?ix=garbage",
        "/idp/clinic/consent?ix=",
    ] {
        let r = b.get(&e.server, path).await;
        assert_eq!(r.status_code(), StatusCode::BAD_REQUEST, "{path}");
        assert!(r.maybe_header(header::LOCATION).is_none());
        assert!(r.text().contains("expired or was already used"));
    }
    let r = b.get(&e.server, "/idp/nope/login?ix=x").await;
    assert_eq!(r.status_code(), StatusCode::NOT_FOUND);
    // A redirect_uri that is not registered: error page at /authorize, never a redirect.
    let bad = authorize_url(PATIENT_SCOPE, &[]).replace(
        &form_urlencoded::byte_serialize(CB.as_bytes()).collect::<String>(),
        &form_urlencoded::byte_serialize(b"https://evil.example/cb").collect::<String>(),
    );
    let r = b.get(&e.server, &bad).await;
    assert!(r.maybe_header(header::LOCATION).is_none(), "open redirect");
    assert!(r.status_code().is_client_error());
}

#[tokio::test]
async fn hosted_pages_refuse_framing_and_inline_script() {
    let e = env().await;
    let mut b = Browser::default();
    let lp = start(&e, &mut b, PATIENT_SCOPE, &[]).await;
    let r = b.get(&e.server, &lp).await;
    let csp = r
        .header(header::CONTENT_SECURITY_POLICY)
        .to_str()
        .unwrap()
        .to_string();
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert!(!csp.contains("unsafe-inline"));
    assert_eq!(r.header(header::X_FRAME_OPTIONS), "DENY");
    assert_eq!(r.header(header::CACHE_CONTROL), "no-store");
    assert_eq!(r.header(header::REFERRER_POLICY), "no-referrer");
    let html = r.text();
    // Every inline script carries this response's nonce.
    let nonce = csp
        .split("'nonce-")
        .nth(1)
        .unwrap()
        .split('\'')
        .next()
        .unwrap();
    for part in html.split("<script").skip(1) {
        assert!(
            part.starts_with(" src=") || part.starts_with(&format!(" nonce=\"{nonce}\"")),
            "{part:.80}"
        );
    }
    // Brand: English, no CJK in the card's brand line.
    assert!(html.contains("HealthyMind"));

    // A realm allowing an EHR to embed it.
    let e = env_with(json!({"frameAncestors": ["https://ehr.example", "*"]})).await;
    let mut b = Browser::default();
    let lp = start(&e, &mut b, PATIENT_SCOPE, &[]).await;
    let r = b.get(&e.server, &lp).await;
    let csp = r
        .header(header::CONTENT_SECURITY_POLICY)
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        csp.ends_with("frame-ancestors https://ehr.example"),
        "{csp}"
    );
    assert!(r.maybe_header(header::X_FRAME_OPTIONS).is_none());
}

#[tokio::test]
async fn rp_initiated_logout_confirms_and_only_redirects_to_registered_uris() {
    let e = env().await;
    let mut b = Browser::default();
    let lp = start(&e, &mut b, PATIENT_SCOPE, &[]).await;
    let r = login(&e, &mut b, &lp, "alice", "alice-password-1").await;
    let r = allow(&e, &mut b, &location(&r), false).await;
    let t = token(&e, &query_of(&location(&r))["code"]).await;
    let id_token = t["id_token"].as_str().unwrap().to_string();

    // Unregistered post-logout URI: error page, no redirect, session kept.
    let bad = format!(
        "/idp/clinic/logout?client_id=app&post_logout_redirect_uri={}",
        form_urlencoded::byte_serialize(b"https://evil.example/").collect::<String>()
    );
    let r = b.get(&e.server, &bad).await;
    assert_eq!(r.status_code(), StatusCode::BAD_REQUEST);
    assert!(r.maybe_header(header::LOCATION).is_none());

    // Without a hint: confirmation page, then POST ends the session.
    let r = b
        .get(&e.server, "/idp/clinic/logout?client_id=app&post_logout_redirect_uri=https%3A%2F%2Fapp.example%2Fbye&state=s9")
        .await;
    let html = r.text();
    assert!(html.contains("Sign out?"), "{html}");
    let mut body: Vec<(String, String)> = vec![("csrf".into(), field(&html, "csrf"))];
    for k in ["client_id", "post_logout_redirect_uri", "state"] {
        body.push((k.into(), field(&html, k)));
    }
    let refs: Vec<(&str, &str)> = body.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let r = b.post(&e.server, "/idp/clinic/logout", &refs).await;
    assert_eq!(r.status_code(), StatusCode::SEE_OTHER);
    assert_eq!(location(&r), "https://app.example/bye?state=s9");
    let ev = e.events.take();
    assert!(
        ev.iter()
            .any(|x| matches!(x, IdpEvent::Logout { user_id: Some(u), .. } if u == "u-alice"))
    );
    // The session is gone: the next authorize asks for the password again.
    let lp = start(&e, &mut b, PATIENT_SCOPE, &[]).await;
    assert_eq!(b.get(&e.server, &lp).await.status_code(), StatusCode::OK);

    // With a valid id_token_hint of this session: no confirmation.
    let r = login(&e, &mut b, &lp, "alice", "alice-password-1").await;
    let r = allow(&e, &mut b, &location(&r), false).await;
    let t = token(&e, &query_of(&location(&r))["code"]).await;
    let hint = t["id_token"].as_str().unwrap();
    let r = b
        .get(&e.server, &format!("/idp/clinic/logout?id_token_hint={hint}&post_logout_redirect_uri=https%3A%2F%2Fapp.example%2Fbye"))
        .await;
    assert_eq!(location(&r), "https://app.example/bye");
    let _ = id_token;
}

#[tokio::test]
async fn logout_and_revoke_forms_need_csrf() {
    let e = env().await;
    let mut b = Browser::default();
    let lp = start(&e, &mut b, PATIENT_SCOPE, &[]).await;
    login(&e, &mut b, &lp, "alice", "alice-password-1").await;
    let r = b
        .post(&e.server, "/idp/clinic/logout", &[("csrf", "x")])
        .await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN);
    let r = b
        .post(
            &e.server,
            "/idp/clinic/account/apps",
            &[("csrf", "x"), ("client_id", "app"), ("endpoint", "lab")],
        )
        .await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN);
    let _ = HeaderName::from_static("x");
}

#[tokio::test]
async fn pages_speak_traditional_chinese_with_an_english_brand() {
    let e = env().await;
    let mut b = Browser::default();
    let lp = start(&e, &mut b, PATIENT_SCOPE, &[]).await;
    let r = b.get(&e.server, &format!("{lp}&lang=zh-Hant")).await;
    let html = r.text();
    assert!(html.contains("lang=\"zh-Hant\""));
    assert!(
        html.contains("登入 Riverside Clinic 以繼續使用 Growth Chart。"),
        "{html}"
    );
    assert!(html.contains("auth-logo__name\">安全登入<"));
    assert!(html.contains("HealthyMind Tech"), "brand stays English");
    assert!(!html.contains("療心"), "no Chinese brand");
    assert!(b.jar.get("hfs_lang").is_some_and(|v| v == "zh-Hant"));
}
