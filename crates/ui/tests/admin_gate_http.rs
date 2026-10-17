//! The administrator sign-in (`HFS_ADMIN_PASSWORD`) over the mounted UI
//! router: the admin pages' guard, `/ui/admin/login`, the `hfs_admin` session
//! cookie, Sign out, the lockout, the fail-closed gate — and that the FHIR
//! routes behind the UI are never asked for it.
//!
//! The gate is process-wide and captured at mount, so these tests run in
//! their own process and serialize on [`SERIAL`].

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::routing::get;
use helios_admin_auth::{AdminGate, AdminSettings};
use http_body_util::BodyExt;
use tower::ServiceExt;

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const PASSWORD: &str = "hfs-ui-test-password";

fn mount(gate: AdminGate) -> Router {
    helios_ui::set_admin_gate(gate);
    // Stand-in for the FHIR router the UI falls back to.
    let fhir = Router::new().route("/Patient", get(|| async { "fhir-search" }));
    helios_ui::mount_with_conformance_source(
        fhir,
        "9.9.9",
        Some(std::path::PathBuf::from("../../data")),
        helios_ui::NlSearch {
            enabled: false,
            configured: false,
            model: "test-model".to_string(),
        },
        None,
        None,
        "default".to_string(),
        Arc::new(helios_ui::StaticConformanceSource::from_data_dir(
            std::path::Path::new("../../data"),
        )),
        helios_fhir::FhirVersion::R4,
        None,
        "https://hfs.example/fhir".to_string(),
        None,
    )
}

fn password_gate() -> AdminGate {
    AdminGate::configure(
        "HFS",
        AdminSettings::default()
            .with_password(PASSWORD)
            .allow_origin_of("https://hfs.example/fhir"),
        true,
        helios_ui::ADMIN_COOKIE,
    )
}

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (StatusCode, HeaderMap, String) {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

const FORM: [(&str, &str); 3] = [
    ("host", "hfs.example"),
    ("origin", "https://hfs.example"),
    ("content-type", "application/x-www-form-urlencoded"),
];

async fn sign_in(app: &Router, next: &str) -> (StatusCode, HeaderMap) {
    let body = format!(
        "username=admin&password={PASSWORD}&next={}",
        next.replace('/', "%2F")
    );
    let (status, headers, _) = send(app, "POST", "/ui/admin/login", &FORM, &body).await;
    (status, headers)
}

fn cookie_of(headers: &HeaderMap) -> String {
    headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn admin_pages_send_the_browser_to_sign_in_and_browsing_stays_public() {
    let _serial = SERIAL.lock().await;
    let app = mount(password_gate());

    for uri in [
        "/ui/packages",
        "/ui/packages/hl7.fhir.us.core/6.1.0",
        "/ui/tenants",
        "/ui/bulk-import",
        "/ui/bulk-export",
        "/ui/sql/export",
    ] {
        let (status, headers, _) = send(&app, "GET", uri, &[], "").await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{uri}");
        assert_eq!(
            headers[header::LOCATION],
            format!("/ui/admin/login?next={uri}"),
            "{uri}"
        );
    }
    // htmx fragments navigate the whole window to the sign-in page.
    let (status, headers, _) = send(
        &app,
        "GET",
        "/ui/tenants/rows",
        &[("hx-request", "true")],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers["hx-redirect"]
            .to_str()
            .unwrap()
            .starts_with("/ui/admin/login?next=")
    );
    // A server-changing form without a session does nothing.
    let (status, _, _) = send(
        &app,
        "POST",
        "/ui/tenants",
        &[("content-type", "application/x-www-form-urlencoded")],
        "id=evil",
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = send(&app, "DELETE", "/ui/tenants/default", &[], "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Browsing pages and the FHIR API behind the UI never ask.
    for uri in ["/ui", "/ui/resources", "/Patient"] {
        let (status, _, body) = send(&app, "GET", uri, &[], "").await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(!body.contains("/ui/admin/logout"), "{uri}");
    }
    // Per-browser preferences are not administration.
    let (status, _, _) = send(
        &app,
        "POST",
        "/ui/version",
        &[("content-type", "application/x-www-form-urlencoded")],
        "version=R4",
    )
    .await;
    assert_ne!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn sign_in_opens_the_admin_pages_until_sign_out() {
    let _serial = SERIAL.lock().await;
    let app = mount(password_gate());

    let (status, _, html) = send(&app, "GET", "/ui/admin/login?next=/ui/packages", &[], "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains(r#"action="/ui/admin/login""#), "{html}");
    assert!(
        html.contains(r#"name="next" value="/ui/packages""#),
        "{html}"
    );
    assert!(html.contains(r#"type="password""#));

    let (status, headers) = sign_in(&app, "/ui/packages").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/ui/packages");
    let set_cookie = headers[header::SET_COOKIE].to_str().unwrap().to_string();
    assert!(set_cookie.starts_with("hfs_admin="), "{set_cookie}");
    for attr in ["Path=/;", "HttpOnly", "SameSite=Strict", "Secure"] {
        assert!(set_cookie.contains(attr), "{attr}: {set_cookie}");
    }
    let cookie = cookie_of(&headers);

    let (status, _, html) = send(&app, "GET", "/ui/packages", &[("cookie", &cookie)], "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains(r#"action="/ui/admin/logout""#), "{html}");
    assert!(html.contains("Administrator"), "{html}");
    // Public pages show the signed-in administrator too.
    let (_, _, html) = send(&app, "GET", "/ui", &[("cookie", &cookie)], "").await;
    assert!(html.contains(r#"action="/ui/admin/logout""#));

    // A write with the cookie but without a same-site Origin is CSRF.
    let (status, _, _) = send(
        &app,
        "POST",
        "/ui/tenants",
        &[
            ("cookie", &cookie),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        "id=acme",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = send(
        &app,
        "POST",
        "/ui/tenants",
        &[
            ("cookie", &cookie),
            ("content-type", "application/x-www-form-urlencoded"),
            ("host", "hfs.example"),
            ("origin", "https://evil.example"),
        ],
        "id=acme",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Signed in, the sign-in page forwards; `next` cannot leave the console.
    let (status, headers, _) = send(
        &app,
        "GET",
        "/ui/admin/login?next=//evil.example/",
        &[("cookie", &cookie)],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/ui");

    // Sign out.
    let (status, headers, _) = send(
        &app,
        "POST",
        "/ui/admin/logout",
        &[("cookie", &cookie), FORM[0], FORM[1]],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(
        headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    let (status, _, _) = send(&app, "GET", "/ui/packages", &[("cookie", &cookie)], "").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn wrong_passwords_are_refused_then_throttled_and_never_echoed() {
    let _serial = SERIAL.lock().await;
    let app = mount(password_gate());
    for _ in 0..4 {
        let (status, headers, html) = send(
            &app,
            "POST",
            "/ui/admin/login",
            &FORM,
            "username=admin&password=wrong-guess&next=%2Fui%2Fpackages",
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(!headers.contains_key(header::SET_COOKIE));
        assert!(html.contains("Wrong user name or password"), "{html}");
        assert!(!html.contains("wrong-guess"));
    }
    let (status, headers, _) = send(
        &app,
        "POST",
        "/ui/admin/login",
        &FORM,
        "username=admin&password=wrong-guess",
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.contains_key(header::RETRY_AFTER));
    // Locked out: even the right password is refused for now.
    let (status, _) = sign_in(&app, "/ui").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // A form posted from another site is refused outright.
    let (status, _, _) = send(
        &app,
        "POST",
        "/ui/admin/login",
        &[
            ("host", "hfs.example"),
            ("origin", "https://evil.example"),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        &format!("username=admin&password={PASSWORD}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_sign_in_page_speaks_traditional_chinese() {
    let _serial = SERIAL.lock().await;
    let app = mount(password_gate());
    let (status, _, html) = send(
        &app,
        "GET",
        "/ui/admin/login",
        &[("accept-language", "zh-TW,zh;q=0.9")],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("管理員登入"), "{html}");
    assert!(html.contains("密碼"), "{html}");
}

#[tokio::test]
async fn without_a_password_the_admin_pages_are_locked_not_open() {
    let _serial = SERIAL.lock().await;
    let app = mount(AdminGate::configure(
        "HFS",
        AdminSettings::default(),
        true,
        helios_ui::ADMIN_COOKIE,
    ));
    let (status, _, _) = send(&app, "GET", "/ui/packages", &[], "").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _, _) = send(
        &app,
        "POST",
        "/ui/tenants",
        &[("content-type", "application/x-www-form-urlencoded")],
        "id=acme",
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let (status, _, html) = send(&app, "GET", "/ui/admin/login", &[], "").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(html.contains("HFS_ADMIN_PASSWORD"), "{html}");
    assert!(!html.contains(r#"type="password""#));
    // Browsing is unaffected.
    let (status, _, _) = send(&app, "GET", "/ui", &[], "").await;
    assert_eq!(status, StatusCode::OK);
}
