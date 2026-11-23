//! Authorization servers console (built-in IdP, I6) through the mounted UI
//! router over the memory store: hidden until installed; wizard with
//! field-anchored errors, server-side draft, secret shown once, endpoint
//! choice, finish that refuses to bind when the checks fail; management
//! pages (keys rotate / emergency with typed confirmation, client delete,
//! sessions, settings bind/unbind, delete realm); zh-Hant strings.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use helios_endpoints::{Actor, EndpointName, EndpointSpec, EndpointStore, MemoryEndpointStore, SecretKeyRing};
use helios_idp::admin::IdpAdmin;
use helios_idp::memory::MemoryStore;
use helios_idp::runtime::{ConventionalEndpointUrls, IdpRuntime};
use helios_idp_admin::binding::StoreBinder;
use helios_idp_admin::service::{self, IdpConsole};
use http_body_util::BodyExt;
use tower::ServiceExt;
use reqwest::Url;

fn app() -> Router {
    helios_ui::mount_with_conformance_source(
        Router::new(),
        "9.9.9",
        Some(std::path::PathBuf::from("../../data")),
        helios_ui::NlSearch::default(),
        None,
        None,
        "default".to_string(),
        Arc::new(helios_ui::StaticConformanceSource::from_data_dir(std::path::Path::new("../../data"))),
        helios_fhir::FhirVersion::R4,
        None,
        "http://localhost:8080/fhir".to_string(),
        None,
    )
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn get_lang(uri: &str, lang: &str) -> Request<Body> {
    Request::builder().uri(uri).header(header::ACCEPT_LANGUAGE, lang).body(Body::empty()).unwrap()
}

fn post(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::ORIGIN, "http://localhost")
        .header(header::HOST, "localhost")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn location(h: &axum::http::HeaderMap) -> String {
    h.get(header::LOCATION).map(|v| v.to_str().unwrap().to_string()).unwrap_or_default()
}

async fn install() -> (Arc<IdpConsole>, Arc<dyn EndpointStore>) {
    let base = Url::parse("http://127.0.0.1:9").unwrap();
    let ring = Arc::new(SecretKeyRing::parse(&SecretKeyRing::generate_spec("t1")).unwrap());
    let rt = IdpRuntime::new(Arc::new(MemoryStore::new()), Some(ring), base.clone(), Arc::new(ConventionalEndpointUrls { public_base: base.to_string() }));
    let eps: Arc<dyn EndpointStore> = Arc::new(MemoryEndpointStore::new());
    eps.create(EndpointSpec::named(EndpointName::parse("clinic").unwrap(), "Clinic"), &Actor::bootstrap()).await.unwrap();
    let rt2 = rt.clone();
    let binder = StoreBinder {
        store: eps.clone(),
        server_base: format!("{base}fhir"),
        public_base: base,
        jwks_via: None,
        realms: Arc::new(move || rt2.realms().iter().map(|r| r.spec.clone()).collect()),
    };
    let mut c = IdpConsole::new(IdpAdmin::new(rt), Some(Arc::new(binder)));
    c.timeout = std::time::Duration::from_secs(2);
    let c = Arc::new(c);
    service::install(c.clone());
    helios_ui::idp::register_nav();
    (c, eps)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idp_console_end_to_end() {
    let app = app();
    // Hidden until the server installs the service (HFS_IDP=on).
    let (s, _, _) = send(&app, get("/ui/idp")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (c, eps) = install().await;
    let (s, _, html) = send(&app, get("/ui/idp")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("No authorization servers yet"), "{html}");
    assert!(html.contains("href=\"/ui/idp/new\""));

    // --- Wizard step 1: field-anchored errors, no raw keys.
    let (s, _, html) = send(&app, get("/ui/idp/new")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("data-idp-issuer-preview"));
    assert!(html.contains("Patient-facing apps"));
    let (s, _, html) = send(&app, post("/ui/idp/new", "display_name=Clinic+apps&name=Bad+Name&template=everything")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(html.contains("id=\"f-name\"") && html.contains("aria-invalid=\"true\""), "{html}");
    assert!(html.contains("href=\"#f-name\""), "error summary links to the field");
    assert!(html.contains("value=\"Clinic apps\""), "typed values kept");
    let (s, h, _) = send(&app, post("/ui/idp/new", "display_name=Clinic+apps&name=clinic-apps&template=everything")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(location(&h), "/ui/idp/new/clinic-apps/2");

    // Draft listed with "Resume".
    let (_, _, html) = send(&app, get("/ui/idp")).await;
    assert!(html.contains("/ui/idp/new/clinic-apps/2") && html.contains("Resume"), "{html}");

    // --- Step 2: keys generated, shown with human state labels.
    let (s, _, html) = send(&app, get("/ui/idp/new/clinic-apps/2")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("Active (signing)"), "{html}");
    let (s, _, html) = send(&app, post("/ui/idp/new/clinic-apps/2", "alg=ES256&rsa_bits=2048&rotation.rotateAfterDays=90&action=next")).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "OIDC needs RS256");
    assert!(html.contains("href=\"#f-signingAlgs\""), "{html}");
    let (s, h, _) = send(&app, post("/ui/idp/new/clinic-apps/2", "alg=RS256&alg=ES256&rsa_bits=2048&rotation.rotateAfterDays=90&action=next")).await;
    assert_eq!((s, location(&h).as_str()), (StatusCode::SEE_OTHER, "/ui/idp/new/clinic-apps/3"));

    // --- Step 3: add a user (password policy error anchored), then next.
    let (s, _, html) = send(&app, post("/ui/idp/new/clinic-apps/3", "user_source=local&security.passwordMinLength=12&security.lockoutThreshold=5&user_username=alice&user_password=short&user_kind=patient&action=add_user")).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("href=\"#f-user_password\""), "{html}");
    assert!(!html.contains("value=\"short\""), "passwords are never echoed");
    let (s, h, _) = send(&app, post("/ui/idp/new/clinic-apps/3", "user_source=local&security.passwordMinLength=12&security.lockoutThreshold=5&user_username=alice&user_password=a+long+enough+password&user_kind=patient&user_endpoint=clinic&user_reference=Patient%2Fp1&action=add_user")).await;
    assert_eq!((s, location(&h).as_str()), (StatusCode::SEE_OTHER, "/ui/idp/new/clinic-apps/3"));
    let (_, _, html) = send(&app, get("/ui/idp/new/clinic-apps/3")).await;
    assert!(html.contains("alice") && html.contains("Patient/p1"), "{html}");
    let (s, _, _) = send(&app, post("/ui/idp/new/clinic-apps/3", "user_source=local&security.passwordMinLength=12&security.lockoutThreshold=5&action=next")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);

    // --- Step 4: app with a bad redirect URI → anchored; a secret app → secret shown once.
    let (s, _, html) = send(&app, post("/ui/idp/new/clinic-apps/4", "action=add_client&client_kind=public&client_name=Portal&redirect_uris=http%3A%2F%2Fportal.example.org%2Fcb&launch=standalone&scope=openid&scope=patient%2F*.rs")).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("href=\"#f-redirect_uris\""), "{html}");
    let (s, _, html) = send(&app, post("/ui/idp/new/clinic-apps/4", "action=add_client&client_kind=secret&client_name=Portal&redirect_uris=https%3A%2F%2Fportal.example.org%2Fcb&launch=standalone&scope=openid&scope=patient%2F*.rs")).await;
    assert_eq!(s, StatusCode::OK, "{html}");
    assert!(html.contains("Client secret for portal"), "{html}");
    let (_, _, again) = send(&app, get("/ui/idp/new/clinic-apps/4")).await;
    assert!(!again.contains("Client secret for portal"), "shown once");
    let (s, _, _) = send(&app, post("/ui/idp/new/clinic-apps/4", "action=next")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);

    // --- Step 5: capabilities; preview reflects the form; invalid combination anchored.
    let (_, _, html) = send(&app, get("/ui/idp/new/clinic-apps/5")).await;
    assert!(html.contains("launch-standalone") && html.contains("hx-post=\"/ui/idp/new/clinic-apps/preview\""));
    let (_, _, frag) = send(&app, post("/ui/idp/new/clinic-apps/preview", "launch_standalone=on&client_public=on&permission_patient=on&openid_connect=on")).await;
    assert!(frag.contains("launch-standalone") && !frag.contains("launch-ehr"), "{frag}");
    let (s, _, html) = send(&app, post("/ui/idp/new/clinic-apps/5", "launch_standalone=on&backend_services=on&client_public=on&permission_patient=on&openid_connect=on&lifetimes.accessToken=3600&action=next")).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("href=\"#f-features.backend_services\""), "{html}");
    let (s, _, _) = send(&app, post("/ui/idp/new/clinic-apps/5", "launch_standalone=on&launch_ehr=on&client_public=on&client_confidential_symmetric=on&permission_patient=on&permission_user=on&openid_connect=on&offline_access=on&introspection=on&revocation=on&lifetimes.accessToken=3600&action=next")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);

    // --- Step 6: endpoints with today's state.
    let (_, _, html) = send(&app, get("/ui/idp/new/clinic-apps/6")).await;
    assert!(html.contains("id=\"ep-clinic\"") && html.contains("server-wide settings"), "{html}");
    let (s, _, _) = send(&app, post("/ui/idp/new/clinic-apps/6", "endpoint=clinic&bind_mode=full&action=next")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);

    // --- Step 7: review + configuration check; finish runs the test. The
    // public URL (127.0.0.1:9) is unreachable, so the endpoint is NOT bound.
    let (_, _, html) = send(&app, get("/ui/idp/new/clinic-apps/7")).await;
    assert!(html.contains("Configuration check") && html.contains("idp-verify"), "{html}");
    let (s, _, html) = send(&app, post("/ui/idp/new/clinic-apps/7", "")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("Not switched yet"), "{html}");
    assert!(html.contains("Fix this first"), "{html}");
    assert!(html.contains("HFS_IDP_DIAGNOSTIC_VIA"), "connect failure explains the fix");
    assert!(eps.get("clinic").await.unwrap().unwrap().spec.auth.is_none(), "endpoint untouched");
    let r = c.realm("clinic-apps").await.unwrap();
    assert!(r.spec.enabled);

    // --- Management pages on a realm created outside the wizard.
    let mut spec = helios_idp::realm::RealmSpec::new(helios_idp::RealmName::parse("staff").unwrap(), "Staff apps");
    spec.endpoints = vec![];
    let rec = c.admin.create_realm(spec).await.unwrap();
    for (path, needle) in [
        ("/ui/idp/staff", "OpenID configuration"),
        ("/ui/idp/staff/clients", "Add an app"),
        ("/ui/idp/staff/users", "Add a user"),
        ("/ui/idp/staff/keys", "Rotate now"),
        ("/ui/idp/staff/sessions", "No one is signed in."),
        ("/ui/idp/staff/settings", "Danger zone"),
        ("/ui/idp/staff/test", "Test this authorization server"),
    ] {
        let (s, _, html) = send(&app, get(path)).await;
        assert_eq!(s, StatusCode::OK, "{path}");
        assert!(html.contains(needle), "{path}: {needle}");
        assert!(html.contains("aria-current=\"page\""), "{path}: tab marked");
    }
    // Rotate keys → next keys published.
    let (s, h, _) = send(&app, post("/ui/idp/staff/keys/rotate", "")).await;
    assert_eq!((s, location(&h).as_str()), (StatusCode::SEE_OTHER, "/ui/idp/staff/keys?done=keys-rotated"));
    let (_, _, html) = send(&app, get("/ui/idp/staff/keys?done=keys-rotated")).await;
    assert!(html.contains("Next (published, not signing yet)") && html.contains("New keys published"), "{html}");
    // Emergency rotation needs the typed name.
    let (_, _, html) = send(&app, post("/ui/idp/staff/keys/emergency", "confirm=nope")).await;
    assert!(html.contains("Type the authorization server&#x27;s name") || html.contains("Type the authorization server"), "{html}");
    let before: Vec<String> = c.admin.list_keys(&rec.id).await.unwrap().iter().filter(|k| k.status == helios_idp::keys::KeyStatus::Active).map(|k| k.kid.clone()).collect();
    let (s, _, _) = send(&app, post("/ui/idp/staff/keys/emergency", "confirm=staff")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let after: Vec<String> = c.admin.list_keys(&rec.id).await.unwrap().iter().filter(|k| k.status == helios_idp::keys::KeyStatus::Active).map(|k| k.kid.clone()).collect();
    assert_eq!(after.len(), 2);
    assert!(after.iter().all(|k| !before.contains(k)));

    // Clients: add (public), open, delete with confirmation.
    let (s, h, _) = send(&app, post("/ui/idp/staff/clients", "client_kind=public&client_name=Chart+viewer&client_id=chart&redirect_uris=https%3A%2F%2Fchart.example.org%2Fcb&launch=ehr&scope=openid&scope=launch")).await;
    assert_eq!((s, location(&h).as_str()), (StatusCode::SEE_OTHER, "/ui/idp/staff/clients?done=client-created"));
    let (_, _, html) = send(&app, get("/ui/idp/staff/clients/chart")).await;
    assert!(html.contains("data-confirm=\"Delete app chart?"), "{html}");
    let (s, h, _) = send(&app, post("/ui/idp/staff/clients/chart/delete", "")).await;
    assert_eq!((s, location(&h).as_str()), (StatusCode::SEE_OTHER, "/ui/idp/staff/clients?done=client-deleted"));
    assert!(c.admin.list_clients(&rec.id).await.unwrap().is_empty());

    // Settings: bind / unbind the endpoint.
    let (s, h, _) = send(&app, post("/ui/idp/staff/settings/bind", "endpoint=clinic&bind_mode=write-protect")).await;
    assert_eq!((s, location(&h).as_str()), (StatusCode::SEE_OTHER, "/ui/idp/staff/settings?done=endpoint-bound"));
    let auth = eps.get("clinic").await.unwrap().unwrap().spec.auth.unwrap();
    assert_eq!(auth.issuer.as_deref(), Some("http://127.0.0.1:9/idp/staff"));
    let (_, _, html) = send(&app, get("/ui/idp/staff/settings")).await;
    assert!(html.contains("internal authorization server") && html.contains("Unbind"), "{html}");
    let (s, _, _) = send(&app, post("/ui/idp/staff/settings/unbind", "endpoint=clinic")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(eps.get("clinic").await.unwrap().unwrap().spec.auth.is_none());
    // Settings save keeps field-anchored errors.
    let (s, _, html) = send(&app, post("/ui/idp/staff/settings", "display_name=Staff&lifetimes.accessToken=99999&openid_connect=on&permission_user=on&launch_standalone=on&client_public=on")).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("href=\"#f-lifetimes.accessToken\""), "{html}");

    // zh-Hant.
    let (_, _, html) = send(&app, get_lang("/ui/idp", "zh-Hant")).await;
    assert!(html.contains("驗證伺服器") && html.contains("新增驗證伺服器"), "{html}");
    let (_, _, html) = send(&app, get_lang("/ui/idp/staff/keys", "zh-Hant")).await;
    assert!(html.contains("立即輪替") && html.contains("緊急輪替"));

    // Delete with typed confirmation.
    let (s, _, html) = send(&app, post("/ui/idp/staff/settings/delete", "confirm=wrong")).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("Type the name exactly"));
    let (s, h, _) = send(&app, post("/ui/idp/staff/settings/delete", "confirm=staff")).await;
    assert_eq!((s, location(&h).as_str()), (StatusCode::SEE_OTHER, "/ui/idp?done=realm-deleted"));
    assert!(c.realm("staff").await.is_err());

    // Discard the wizard draft (still a draft after the failed finish).
    let (s, h, _) = send(&app, post("/ui/idp/new/clinic-apps/discard", "")).await;
    assert_eq!((s, location(&h).as_str()), (StatusCode::SEE_OTHER, "/ui/idp?done=draft-discarded"));
    service::uninstall();
}

#[test]
fn idp_catalogs_in_parity() {
    use std::collections::BTreeSet;
    let keys = |locale: &str| -> BTreeSet<String> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../locales").join(locale).join("idp-console.ftl");
        let src = std::fs::read_to_string(&path).unwrap();
        let res = fluent_syntax::parser::parse(src.as_str()).unwrap_or_else(|(_, e)| panic!("{locale}: {e:?}"));
        res.body
            .iter()
            .filter_map(|e| match e {
                fluent_syntax::ast::Entry::Message(m) => Some(m.id.name.to_string()),
                _ => None,
            })
            .collect()
    };
    let en = keys("en");
    assert!(en.len() > 400, "{}", en.len());
    assert_eq!(en, keys("zh-Hant"));
}
