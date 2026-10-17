//! The administrator sign-in in front of the terminology management pages
//! (`HTS_ADMIN_PASSWORD`): the page guard, the sign-in form, the session
//! cookie, Sign out in the account menu, and the console's own credential on
//! its calls to the admin API.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, StatusCode, header},
    routing::get,
};
use helios_admin_auth::{AdminGate, AdminSettings};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

const PASSWORD: &str = "hts-ui-test-password";

/// A mock admin API that answers only callers presenting `expected`.
async fn start_mock(expected: String) -> String {
    let check = move |headers: HeaderMap| {
        let ok = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            == Some(expected.as_str());
        async move { ok }
    };
    let c1 = check.clone();
    let c2 = check.clone();
    let router = Router::new()
        .route(
            "/admin",
            get(move |h: HeaderMap| async move {
                if c1(h).await {
                    (
                        StatusCode::OK,
                        axum::Json(json!({"enabled": true, "backend": "sqlite"})),
                    )
                } else {
                    (
                        StatusCode::UNAUTHORIZED,
                        axum::Json(
                            json!({"error": "login", "message": "administrator sign-in required"}),
                        ),
                    )
                }
            }),
        )
        .route(
            "/admin/jobs",
            get(move |h: HeaderMap| async move {
                if c2(h).await {
                    (StatusCode::OK, axum::Json(json!({"jobs": []})))
                } else {
                    (
                        StatusCode::UNAUTHORIZED,
                        axum::Json(
                            json!({"error": "login", "message": "administrator sign-in required"}),
                        ),
                    )
                }
            }),
        )
        .route(
            "/health",
            get(|| async { axum::Json(json!({"status": "ok"})) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{addr}")
}

async fn app_with(gate: AdminGate) -> Router {
    let mut upstream = helios_hts_ui::UpstreamClient::new_with_timeouts(
        start_mock(
            gate.auth()
                .map(|a| a.service_authorization().to_str().unwrap().to_string())
                .unwrap_or_default(),
        )
        .await,
        Duration::from_secs(5),
        Duration::from_secs(2),
    )
    .unwrap();
    if let Some(auth) = gate.auth() {
        upstream = upstream.with_admin_authorization(auth.service_authorization());
    }
    let state = Arc::new(helios_hts_ui::HtsUiState {
        fhir_version: "R4",
        version: "9.9.9-test",
        upstream,
        bundled_data_bytes: None,
        metrics_ring: Default::default(),
    });
    Router::new().nest("/ui", helios_hts_ui::router_with_admin(state, None, gate))
}

fn password_gate() -> AdminGate {
    AdminGate::configure(
        "HTS",
        AdminSettings::default().with_password(PASSWORD),
        true,
        helios_hts_ui::ADMIN_COOKIE,
    )
}

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (StatusCode, HeaderMap, String) {
    let mut req = axum::http::Request::builder().method(method).uri(uri);
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

async fn sign_in(app: &Router) -> String {
    let (status, headers, _) = send(
        app,
        "POST",
        "/ui/hts/login",
        &[
            ("host", "tx.example"),
            ("origin", "https://tx.example"),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        &format!("username=admin&password={PASSWORD}&next=%2Fui%2Fhts%2Fjobs"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn management_pages_need_the_administrator_and_browsing_does_not() {
    let app = app_with(password_gate()).await;
    for uri in ["/ui/hts/terminology", "/ui/hts/jobs", "/ui/hts/import"] {
        let (status, headers, _) = send(&app, "GET", uri, &[], "").await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{uri}");
        assert_eq!(
            headers[header::LOCATION],
            format!("/ui/hts/login?next={uri}"),
            "{uri}"
        );
    }
    // htmx polling of a fragment navigates the window to the sign-in page.
    let (status, headers, _) =
        send(&app, "GET", "/ui/hts/jobs", &[("hx-request", "true")], "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.contains_key("hx-redirect"));
    // A management form posted without a session does nothing.
    let (status, _, _) = send(
        &app,
        "POST",
        "/ui/hts/terminology/compact",
        &[("content-type", "application/x-www-form-urlencoded")],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Browsing stays public, and shows no Sign out.
    let (status, _, html) = send(&app, "GET", "/ui/hts", &[], "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!html.contains("/ui/hts/logout"));
}

#[tokio::test]
async fn signed_in_pages_reach_the_admin_api_and_offer_sign_out() {
    let app = app_with(password_gate()).await;
    let cookie = sign_in(&app).await;
    let (status, _, html) = send(&app, "GET", "/ui/hts/jobs", &[("cookie", &cookie)], "").await;
    assert_eq!(status, StatusCode::OK);
    // The mock admin API answered (it refuses calls without the console's
    // credential, which would surface as an error banner).
    assert!(!html.contains("administrator sign-in required"), "{html}");
    assert!(html.contains(r#"action="/ui/hts/logout""#), "{html}");
    assert!(html.contains("Administrator"), "{html}");

    // Signed in, the sign-in page just forwards.
    let (status, headers, _) = send(
        &app,
        "GET",
        "/ui/hts/login?next=/ui/hts/terminology",
        &[("cookie", &cookie)],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/ui/hts/terminology");
}

#[tokio::test]
async fn sign_in_page_is_translated_and_wrong_passwords_are_throttled() {
    let app = app_with(password_gate()).await;
    let (status, _, html) = send(
        &app,
        "GET",
        "/ui/hts/login",
        &[("accept-language", "zh-TW")],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("管理員登入"), "{html}");
    assert!(html.contains(r#"type="password""#));

    let form = [
        ("host", "tx.example"),
        ("origin", "https://tx.example"),
        ("content-type", "application/x-www-form-urlencoded"),
    ];
    for _ in 0..4 {
        let (status, headers, html) = send(
            &app,
            "POST",
            "/ui/hts/login",
            &form,
            "username=admin&password=nope",
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(!headers.contains_key(header::SET_COOKIE));
        assert!(html.contains("Wrong user name or password"), "{html}");
        // The password is never echoed back into the page.
        assert!(!html.contains("nope"), "{html}");
    }
    let (status, headers, _) = send(
        &app,
        "POST",
        "/ui/hts/login",
        &form,
        "username=admin&password=nope",
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.contains_key(header::RETRY_AFTER));
}

#[tokio::test]
async fn locked_gate_explains_and_open_gate_needs_no_sign_in() {
    let locked = AdminGate::configure(
        "HTS",
        AdminSettings::default(),
        true,
        helios_hts_ui::ADMIN_COOKIE,
    );
    let app = app_with(locked).await;
    let (status, _, _) = send(&app, "GET", "/ui/hts/jobs", &[], "").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _, html) = send(&app, "GET", "/ui/hts/login", &[], "").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(html.contains("HTS_ADMIN_PASSWORD"), "{html}");
    assert!(!html.contains(r#"type="password""#));

    let open = app_with(AdminGate::Open).await;
    let (status, _, _) = send(&open, "GET", "/ui/hts/jobs", &[], "").await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers, _) = send(&open, "GET", "/ui/hts/login?next=/ui/hts/jobs", &[], "").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/ui/hts/jobs");
}
