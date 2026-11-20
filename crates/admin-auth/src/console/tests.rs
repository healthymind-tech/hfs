//! Console sign-in tests: login flows per mode, sessions, CSRF, lockout,
//! expiry, account changes reaching sessions, role gating through the gate
//! middleware, the admin API, and OIDC against an in-process mock IdP.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use helios_endpoints::console::{ConsoleAuthMode, ConsoleRole, ConsoleStore};
use tower::ServiceExt;

use super::guard::{GateOutcome, gate};
use super::testing::{env, runtime};
use super::*;

const BG: &str = "break-glass-password-1";
const HOST: &str = "hfs.test";
const ORIGIN: &str = "http://hfs.test";

fn opts() -> ConsoleOptions {
    ConsoleOptions {
        public_base_url: "http://hfs.test".into(),
        recheck: Duration::from_secs(0),
        cookie_secure: CookieSecure::Never,
        ..Default::default()
    }
}

fn site_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(header::HOST, HeaderValue::from_static(HOST));
    h.insert(header::ORIGIN, HeaderValue::from_static(ORIGIN));
    h
}

/// `Cookie` header from `Set-Cookie` values (name=value pairs, non-empty).
fn cookie_header(set: &[HeaderValue]) -> String {
    set.iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .filter(|kv| !kv.ends_with('='))
        .collect::<Vec<_>>()
        .join("; ")
}

fn csrf_of(set: &[HeaderValue]) -> String {
    set.iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| v.strip_prefix("hfs_csrf="))
        .map(|v| v.split(';').next().unwrap().to_string())
        .unwrap()
}

async fn local_rt(pairs: &[(&str, &str)]) -> (Arc<ConsoleAuth>, Arc<memory::MemoryConsoleStore>) {
    let mut all = vec![("HFS_CONSOLE_AUTH_MODE", "local")];
    all.extend_from_slice(pairs);
    runtime(env(&all), Some(BG), opts()).await
}

async fn add_user(
    rt: &ConsoleAuth,
    name: &str,
    role: ConsoleRole,
    password: &str,
) -> helios_endpoints::console::ConsoleUser {
    let (mut u, _) = rt
        .create_user(
            "test",
            NewUser {
                username: name.into(),
                role: Some(role),
                password: Some(password.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // Clear the forced change for tests that are not about it.
    u.must_change_password = false;
    u.password_hash = None;
    let rev = u.revision;
    rt.store().update_user(u, rev).await.unwrap()
}

async fn sign_in(rt: &ConsoleAuth, user: &str, pw: &str) -> Vec<HeaderValue> {
    match rt.login_password(&site_headers(), None, user, pw).await {
        LoginResult::Success { cookies, .. } => cookies,
        other => panic!("sign-in failed: {other:?}"),
    }
}

/// An app with a route per path, behind the console gate (plain refusals).
fn app(rt: Arc<ConsoleAuth>) -> Router {
    async fn gate_mw(request: Request, next: Next) -> Response {
        let rt = request
            .extensions()
            .get::<Arc<ConsoleAuth>>()
            .cloned()
            .unwrap();
        match gate(&rt, request).await {
            GateOutcome::Pass(req, _) => next.run(*req).await,
            other => other.into_plain_response(),
        }
    }
    Router::new()
        .route("/ui/{*rest}", any(|| async { "page" }))
        .route("/ui", get(|| async { "home" }))
        .route(
            "/admin/{*rest}",
            any(|r: Request| async move {
                r.extensions()
                    .get::<crate::AdminUser>()
                    .map(|u| u.0.clone())
                    .unwrap_or_default()
            }),
        )
        .layer(axum::middleware::from_fn(gate_mw))
        .layer(axum::middleware::from_fn_with_state(
            crate::Guard {
                gate: rt.break_glass_gate().clone(),
                protects: |_, p| crate::under(p, "/admin"),
                style: crate::Style::Api { fhir: true },
            },
            crate::guard,
        ))
        .layer(axum::Extension(rt))
}

async fn send(
    app: &Router,
    method: Method,
    path: &str,
    cookies: &str,
    extra: &[(&str, &str)],
) -> Response {
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST);
    if !cookies.is_empty() {
        b = b.header(header::COOKIE, cookies);
    }
    for (k, v) in extra {
        b = b.header(*k, *v);
    }
    app.clone()
        .oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn location(r: &Response) -> String {
    r.headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

// ------------------------------------------------------------------ local

#[tokio::test]
async fn local_mode_requires_sign_in_on_every_page() {
    let (rt, _) = local_rt(&[]).await;
    let app = app(rt.clone());
    for path in [
        "/ui",
        "/ui/resources",
        "/ui/settings/users",
        "/ui/e/lab/data",
    ] {
        let r = send(&app, Method::GET, path, "", &[]).await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER, "{path}");
        assert!(
            location(&r).starts_with("/ui/login?next="),
            "{path}: {}",
            location(&r)
        );
    }
    // htmx gets HX-Redirect; a form post without a session gets 401.
    let r = send(
        &app,
        Method::GET,
        "/ui/resources",
        "",
        &[("hx-request", "true")],
    )
    .await;
    assert!(r.headers().contains_key("hx-redirect"));
    let r = send(&app, Method::POST, "/ui/anything", "", &[]).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    // Public paths stay reachable.
    for path in ["/ui/login", "/ui/assets/app.css", "/ui/callback"] {
        assert_eq!(
            send(&app, Method::GET, path, "", &[]).await.status(),
            StatusCode::OK,
            "{path}"
        );
    }
}

#[tokio::test]
async fn local_sign_in_session_roles_and_logout() {
    let (rt, _) = local_rt(&[]).await;
    add_user(&rt, "vera", ConsoleRole::Viewer, "tidy-Lantern-41-orbit").await;
    add_user(&rt, "eddie", ConsoleRole::Editor, "tidy-Lantern-41-orbit").await;
    add_user(&rt, "ada", ConsoleRole::Admin, "tidy-Lantern-41-orbit").await;
    let app = app(rt.clone());

    let viewer = sign_in(&rt, "VERA", "tidy-Lantern-41-orbit").await;
    let set: Vec<String> = viewer
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    assert!(set.iter().any(|c| c.starts_with("hfs_console=")
        && c.contains("HttpOnly")
        && c.contains("SameSite=Strict")));
    assert!(
        set.iter()
            .any(|c| c.starts_with("hfs_csrf=") && !c.contains("HttpOnly"))
    );
    let vc = cookie_header(&viewer);
    assert_eq!(
        send(&app, Method::GET, "/ui/resources", &vc, &[])
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        send(&app, Method::GET, "/ui/settings/users", &vc, &[])
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let csrf = csrf_of(&viewer);
    let r = send(
        &app,
        Method::POST,
        "/ui/sql/view-definitions",
        &vc,
        &[("origin", ORIGIN), ("x-csrf-token", &csrf)],
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "viewer cannot write");

    let editor = sign_in(&rt, "eddie", "tidy-Lantern-41-orbit").await;
    let ec = cookie_header(&editor);
    let et = csrf_of(&editor);
    let r = send(
        &app,
        Method::POST,
        "/ui/sql/view-definitions",
        &ec,
        &[("origin", ORIGIN), ("x-csrf-token", &et)],
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        send(&app, Method::GET, "/ui/packages", &ec, &[])
            .await
            .status(),
        StatusCode::FORBIDDEN
    );

    let admin = sign_in(&rt, "ada", "tidy-Lantern-41-orbit").await;
    let ac = cookie_header(&admin);
    assert_eq!(
        send(&app, Method::GET, "/ui/settings/users", &ac, &[])
            .await
            .status(),
        StatusCode::OK
    );

    // Sign-out ends the session.
    let mut h = site_headers();
    h.insert(header::COOKIE, HeaderValue::from_str(&vc).unwrap());
    let out = rt.logout(&h).await;
    assert_eq!(out.location, "/ui/login?signed_out=1");
    assert!(
        out.cookies
            .iter()
            .any(|c| c.to_str().unwrap().contains("Max-Age=0"))
    );
    assert_eq!(
        send(&app, Method::GET, "/ui/resources", &vc, &[])
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn csrf_needs_same_origin_and_the_session_token() {
    let (rt, _) = local_rt(&[]).await;
    add_user(&rt, "eddie", ConsoleRole::Editor, "tidy-Lantern-41-orbit").await;
    let app = app(rt.clone());
    let set = sign_in(&rt, "eddie", "tidy-Lantern-41-orbit").await;
    let c = cookie_header(&set);
    let t = csrf_of(&set);
    // No token.
    let r = send(&app, Method::POST, "/ui/x", &c, &[("origin", ORIGIN)]).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    // Wrong token.
    let r = send(
        &app,
        Method::POST,
        "/ui/x",
        &c,
        &[("origin", ORIGIN), ("x-csrf-token", "nope")],
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    // Right token, foreign origin.
    let r = send(
        &app,
        Method::POST,
        "/ui/x",
        &c,
        &[("origin", "https://evil.example"), ("x-csrf-token", &t)],
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    // Right token, no Origin/Referer at all.
    let r = send(&app, Method::POST, "/ui/x", &c, &[("x-csrf-token", &t)]).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    // Referer is accepted in place of Origin.
    let r = send(
        &app,
        Method::POST,
        "/ui/x",
        &c,
        &[("referer", "http://hfs.test/ui/page"), ("x-csrf-token", &t)],
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    // Form field `_csrf` works for plain forms, and the body still reaches
    // the handler.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/ui/x")
        .header(header::HOST, HOST)
        .header(header::COOKIE, &c)
        .header(header::ORIGIN, ORIGIN)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!("a=1&_csrf={t}")))
        .unwrap();
    let echo = Router::new()
        .route(
            "/ui/x",
            axum::routing::post(|body: String| async move { body }),
        )
        .layer(axum::middleware::from_fn(
            |request: Request, next: Next| async move {
                let rt = request
                    .extensions()
                    .get::<Arc<ConsoleAuth>>()
                    .cloned()
                    .unwrap();
                match gate(&rt, request).await {
                    GateOutcome::Pass(req, _) => next.run(*req).await,
                    other => other.into_plain_response(),
                }
            },
        ))
        .layer(axum::Extension(rt.clone()));
    let r = echo.oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    assert!(String::from_utf8_lossy(&body).starts_with("a=1&_csrf="));
    // Reads need no token.
    assert_eq!(
        send(&app, Method::GET, "/ui/x", &c, &[]).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn lockout_per_account_and_generic_errors() {
    let (rt, _) = local_rt(&[("HFS_CONSOLE_LOCKOUT_FAILURES", "3")]).await;
    add_user(&rt, "bob", ConsoleRole::Viewer, "tidy-Lantern-41-orbit").await;
    for _ in 0..2 {
        assert!(matches!(
            rt.login_password(&site_headers(), None, "bob", "wrong")
                .await,
            LoginResult::Invalid
        ));
    }
    // Third failure locks; then even the right password is refused.
    assert!(matches!(
        rt.login_password(&site_headers(), None, "bob", "wrong")
            .await,
        LoginResult::RateLimited(_)
    ));
    assert!(matches!(
        rt.login_password(&site_headers(), None, "bob", "tidy-Lantern-41-orbit")
            .await,
        LoginResult::RateLimited(_)
    ));
    // Unknown users get the same generic answer.
    let (rt2, _) = local_rt(&[]).await;
    assert!(matches!(
        rt2.login_password(&site_headers(), None, "ghost", "whatever")
            .await,
        LoginResult::Invalid
    ));
    // Cross-site posts are refused before anything else.
    let mut h = site_headers();
    h.insert(
        header::ORIGIN,
        HeaderValue::from_static("https://evil.example"),
    );
    assert!(matches!(
        rt2.login_password(&h, None, "ghost", "x").await,
        LoginResult::CrossOrigin
    ));
}

#[tokio::test]
async fn idle_and_absolute_expiry() {
    let (rt, store) = local_rt(&[]).await;
    add_user(&rt, "bob", ConsoleRole::Viewer, "tidy-Lantern-41-orbit").await;
    let set = sign_in(&rt, "bob", "tidy-Lantern-41-orbit").await;
    let mut h = HeaderMap::new();
    h.insert(
        header::COOKIE,
        HeaderValue::from_str(&cookie_header(&set)).unwrap(),
    );
    let r = rt.resolve(&h).await.unwrap();
    // Age the stored session past the idle lifetime.
    let mut s = r.session.clone();
    s.last_seen_unix -= 481 * 60;
    rt.put_session(&r.key, &s).await.unwrap();
    rt.cache_lock().clear();
    assert!(rt.resolve(&h).await.is_none(), "idle expiry");
    // Absolute lifetime.
    let set = sign_in(&rt, "bob", "tidy-Lantern-41-orbit").await;
    h.insert(
        header::COOKIE,
        HeaderValue::from_str(&cookie_header(&set)).unwrap(),
    );
    let r = rt.resolve(&h).await.unwrap();
    let mut s = r.session.clone();
    s.created_unix -= 25 * 3600;
    rt.put_session(&r.key, &s).await.unwrap();
    rt.cache_lock().clear();
    assert!(rt.resolve(&h).await.is_none(), "absolute expiry");
    let _ = store;
}

#[tokio::test]
async fn disable_role_change_and_mode_switch_reach_sessions() {
    let (rt, _) = local_rt(&[]).await;
    add_user(&rt, "ada", ConsoleRole::Admin, "tidy-Lantern-41-orbit").await;
    let bob = add_user(&rt, "bob", ConsoleRole::Editor, "tidy-Lantern-41-orbit").await;
    let set = sign_in(&rt, "bob", "tidy-Lantern-41-orbit").await;
    let mut h = HeaderMap::new();
    h.insert(
        header::COOKIE,
        HeaderValue::from_str(&cookie_header(&set)).unwrap(),
    );
    assert_eq!(
        rt.resolve(&h).await.unwrap().principal.role,
        ConsoleRole::Editor
    );
    let bob = rt
        .update_user(
            "ada",
            &bob.id,
            UserPatch {
                role: Some(ConsoleRole::Viewer),
                ..Default::default()
            },
            bob.revision,
        )
        .await
        .unwrap();
    assert_eq!(
        rt.resolve(&h).await.unwrap().principal.role,
        ConsoleRole::Viewer,
        "downgraded"
    );
    rt.update_user(
        "ada",
        &bob.id,
        UserPatch {
            disabled: Some(true),
            ..Default::default()
        },
        bob.revision,
    )
    .await
    .unwrap();
    assert!(rt.resolve(&h).await.is_none(), "disabled ends the session");
    assert!(matches!(
        rt.login_password(&site_headers(), None, "bob", "tidy-Lantern-41-orbit")
            .await,
        LoginResult::Invalid
    ));
}

#[tokio::test]
async fn forced_password_change_and_policy() {
    let (rt, _) = local_rt(&[]).await;
    add_user(&rt, "ada", ConsoleRole::Admin, "tidy-Lantern-41-orbit").await;
    let (u, otp) = rt
        .create_user(
            "ada",
            NewUser {
                username: "newbie".into(),
                role: Some(ConsoleRole::Editor),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(u.must_change_password);
    let otp = otp.expect("generated one-time password");
    let set = match rt
        .login_password(&site_headers(), None, "newbie", &otp)
        .await
    {
        LoginResult::Success {
            cookies,
            must_change_password,
        } => {
            assert!(must_change_password);
            cookies
        }
        other => panic!("{other:?}"),
    };
    let app = app(rt.clone());
    let c = cookie_header(&set);
    let r = send(&app, Method::GET, "/ui/resources", &c, &[]).await;
    assert_eq!(location(&r), "/ui/account/password");
    assert_eq!(
        send(&app, Method::GET, "/ui/account/password", &c, &[])
            .await
            .status(),
        StatusCode::OK
    );
    let mut h = HeaderMap::new();
    h.insert(header::COOKIE, HeaderValue::from_str(&c).unwrap());
    let resolved = rt.resolve(&h).await.unwrap();
    assert!(matches!(
        rt.change_own_password(&resolved, &otp, "short").await,
        Err(ConsoleError::Policy(PolicyViolation::TooShort(12)))
    ));
    assert!(matches!(
        rt.change_own_password(&resolved, "wrong", "a-Much-better-pass-9")
            .await,
        Err(ConsoleError::Invalid(_))
    ));
    rt.change_own_password(&resolved, &otp, "a-Much-better-pass-9")
        .await
        .unwrap();
    assert_eq!(
        send(&app, Method::GET, "/ui/resources", &c, &[])
            .await
            .status(),
        StatusCode::OK
    );
    // Reset: one-time password again, sessions end.
    let otp2 = rt.reset_password("ada", &u.id).await.unwrap();
    assert_ne!(otp, otp2);
    assert_eq!(
        send(&app, Method::GET, "/ui/resources", &c, &[])
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    // Usernames: reserved and malformed are refused; policy on given passwords.
    for bad in ["admin", "ADMIN", "", "has space", "anonymous"] {
        assert!(
            matches!(
                rt.create_user(
                    "ada",
                    NewUser {
                        username: bad.into(),
                        ..Default::default()
                    }
                )
                .await,
                Err(ConsoleError::Invalid(_))
            ),
            "{bad}"
        );
    }
    assert!(matches!(
        rt.create_user(
            "ada",
            NewUser {
                username: "x1".into(),
                password: Some("unbelievable".into()),
                ..Default::default()
            }
        )
        .await,
        Err(ConsoleError::Policy(PolicyViolation::Common))
    ));
}

// -------------------------------------------------------------- break-glass

#[tokio::test]
async fn break_glass_works_in_every_mode_and_can_be_disabled() {
    for mode in ["none", "local", "oidc"] {
        let mut pairs = vec![("HFS_CONSOLE_AUTH_MODE", mode)];
        if mode == "oidc" {
            pairs.push(("HFS_CONSOLE_OIDC_ISSUER", "https://idp.example"));
            pairs.push(("HFS_CONSOLE_OIDC_CLIENT_ID", "hfs-web"));
        }
        let (rt, _) = runtime(env(&pairs), Some(BG), opts()).await;
        let set = sign_in(&rt, "admin", BG).await;
        assert!(
            set.iter()
                .any(|c| c.to_str().unwrap().starts_with("hfs_admin=")),
            "{mode}: legacy admin session too"
        );
        let app = app(rt.clone());
        let c = cookie_header(&set);
        assert_eq!(
            send(&app, Method::GET, "/ui/settings/auth", &c, &[])
                .await
                .status(),
            StatusCode::OK,
            "{mode}"
        );
        assert!(matches!(
            rt.login_password(&site_headers(), None, "admin", "wrong")
                .await,
            LoginResult::Invalid
        ));
    }
    let (rt, _) = runtime(
        env(&[
            ("HFS_CONSOLE_AUTH_MODE", "local"),
            ("HFS_CONSOLE_BREAK_GLASS", "false"),
        ]),
        Some(BG),
        opts(),
    )
    .await;
    assert!(!rt.break_glass_available());
    assert!(matches!(
        rt.login_password(&site_headers(), None, "admin", BG).await,
        LoginResult::Invalid
    ));
    // No password configured: no break-glass.
    let (rt, _) = runtime(env(&[("HFS_CONSOLE_AUTH_MODE", "local")]), None, opts()).await;
    assert!(!rt.break_glass_available());
}

// ------------------------------------------------------------------- none

#[tokio::test]
async fn none_mode_is_todays_console() {
    let (rt, _) = runtime(env(&[]), Some(BG), opts()).await;
    assert_eq!(rt.mode(), ConsoleAuthMode::None);
    let app = app(rt.clone());
    assert_eq!(
        send(&app, Method::GET, "/ui", "", &[]).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        send(&app, Method::GET, "/ui/resources", "", &[])
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        send(&app, Method::POST, "/ui/editor/render", "", &[])
            .await
            .status(),
        StatusCode::OK,
        "previews open"
    );
    let r = send(&app, Method::GET, "/ui/packages", "", &[]).await;
    assert_eq!(r.status(), StatusCode::SEE_OTHER);
    assert!(
        location(&r).starts_with("/ui/login?local=1&next="),
        "{}",
        location(&r)
    );
    assert_eq!(
        send(&app, Method::POST, "/ui/sql/queries", "", &[])
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    // Local accounts cannot sign in outside mode local.
    assert!(matches!(
        rt.login_password(&site_headers(), None, "bob", "tidy-Lantern-41-orbit")
            .await,
        LoginResult::Invalid
    ));
    // anonymousRole none = sign-in required even in mode none.
    let (rt, _) = runtime(
        env(&[("HFS_CONSOLE_ANONYMOUS_ROLE", "none")]),
        Some(BG),
        opts(),
    )
    .await;
    let r = send(&app_of(rt), Method::GET, "/ui", "", &[]).await;
    assert_eq!(r.status(), StatusCode::SEE_OTHER);
}

fn app_of(rt: Arc<ConsoleAuth>) -> Router {
    app(rt)
}

#[tokio::test]
async fn endpoint_bindings_raise_or_lower_the_role_per_endpoint() {
    let (rt, store) = local_rt(&[]).await;
    add_user(&rt, "bob", ConsoleRole::Viewer, "tidy-Lantern-41-orbit").await;
    store
        .set_role_binding("local:bob", "lab", ConsoleRole::Editor, "t")
        .await
        .unwrap();
    let app = app(rt.clone());
    let set = sign_in(&rt, "bob", "tidy-Lantern-41-orbit").await;
    let c = cookie_header(&set);
    let t = csrf_of(&set);
    let w = [("origin", ORIGIN), ("x-csrf-token", t.as_str())];
    assert_eq!(
        send(&app, Method::POST, "/ui/e/lab/data/Patient", &c, &w)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        send(&app, Method::POST, "/ui/e/other/data/Patient", &c, &w)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&app, Method::POST, "/ui/e/lab/security", &c, &w)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
}

// -------------------------------------------------------------- admin API

#[tokio::test]
async fn admin_api_accepts_console_admins_with_csrf_and_break_glass_basic() {
    let (rt, _) = local_rt(&[]).await;
    add_user(&rt, "ada", ConsoleRole::Admin, "tidy-Lantern-41-orbit").await;
    add_user(&rt, "eddie", ConsoleRole::Editor, "tidy-Lantern-41-orbit").await;
    let app = app(rt.clone());
    assert_eq!(
        send(&app, Method::GET, "/admin/packages", "", &[])
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let ed = cookie_header(&sign_in(&rt, "eddie", "tidy-Lantern-41-orbit").await);
    assert_eq!(
        send(&app, Method::GET, "/admin/packages", &ed, &[])
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let set = sign_in(&rt, "ada", "tidy-Lantern-41-orbit").await;
    let ac = cookie_header(&set);
    let r = send(&app, Method::GET, "/admin/packages", &ac, &[]).await;
    assert_eq!(r.status(), StatusCode::OK);
    let body = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
    assert_eq!(
        &body[..],
        b"local:ada",
        "AdminUser stamped for the audit agent"
    );
    assert_eq!(
        send(
            &app,
            Method::POST,
            "/admin/packages",
            &ac,
            &[("origin", ORIGIN)]
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let t = csrf_of(&set);
    assert_eq!(
        send(
            &app,
            Method::POST,
            "/admin/packages",
            &ac,
            &[("origin", ORIGIN), ("x-csrf-token", &t)]
        )
        .await
        .status(),
        StatusCode::OK
    );
    // Break-glass credentials for scripts.
    let basic = format!("Basic {}", STANDARD.encode(format!("admin:{BG}")));
    assert_eq!(
        send(
            &app,
            Method::POST,
            "/admin/packages",
            "",
            &[("authorization", &basic)]
        )
        .await
        .status(),
        StatusCode::OK
    );
    let bad = format!("Basic {}", STANDARD.encode("admin:nope"));
    assert_eq!(
        send(
            &app,
            Method::GET,
            "/admin/packages",
            "",
            &[("authorization", &bad)]
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    // The legacy admin cookie alone is not a console credential.
    assert_eq!(
        send(
            &app,
            Method::GET,
            "/admin/packages",
            "hfs_admin=whatever",
            &[]
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
}

async fn api_call(
    app: &Router,
    method: Method,
    path: &str,
    body: Option<serde_json::Value>,
    if_match: Option<u64>,
) -> (StatusCode, HeaderMap, serde_json::Value) {
    let basic = format!("Basic {}", STANDARD.encode(format!("admin:{BG}")));
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST)
        .header(header::AUTHORIZATION, basic);
    if let Some(r) = if_match {
        b = b.header(header::IF_MATCH, format!("\"{r}\""));
    }
    let req = match body {
        Some(v) => b
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let r = app.clone().oneshot(req).await.unwrap();
    let status = r.status();
    let headers = r.headers().clone();
    let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    let v = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, headers, v)
}

fn api_app(rt: Arc<ConsoleAuth>) -> Router {
    api::router(rt.clone())
        .layer(axum::middleware::from_fn_with_state(
            crate::Guard {
                gate: rt.break_glass_gate().clone(),
                protects: |_, p| crate::under(p, "/admin"),
                style: crate::Style::Api { fhir: true },
            },
            crate::guard,
        ))
        .layer(axum::Extension(rt))
}

#[tokio::test]
async fn admin_api_settings_and_users() {
    let (rt, _) = runtime(
        env(&[("HFS_CONSOLE_SESSION_IDLE_MINUTES", "60")]),
        Some(BG),
        opts(),
    )
    .await;
    let app = api_app(rt.clone());
    // Unauthenticated: refused by the guard.
    let r = app
        .clone()
        .oneshot(
            Request::get("/admin/console/settings")
                .header(header::HOST, HOST)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    // The router itself refuses a request no guard admitted.
    let bare = api::router(rt.clone());
    let r = bare
        .oneshot(
            Request::get("/admin/console/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    let (s, h, v) = api_call(&app, Method::GET, "/admin/console/settings", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h[header::ETAG], "\"0\"");
    assert_eq!(v["effective"]["mode"], "none");
    assert_eq!(v["effective"]["sessionIdleMinutes"], 60);
    assert!(
        v["pinned"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("sessionIdleMinutes"))
    );

    let mut body = v["effective"].clone();
    // No If-Match: 428.
    assert_eq!(
        api_call(
            &app,
            Method::PUT,
            "/admin/console/settings",
            Some(body.clone()),
            None
        )
        .await
        .0,
        StatusCode::PRECONDITION_REQUIRED
    );
    // Pinned field changed: 422 naming it.
    body["sessionIdleMinutes"] = serde_json::json!(30);
    let (s, _, v) = api_call(
        &app,
        Method::PUT,
        "/admin/console/settings",
        Some(body.clone()),
        Some(0),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["issue"][0]["expression"][0], "sessionIdleMinutes");
    // Switching to local without an admin account: refused.
    body["sessionIdleMinutes"] = serde_json::json!(60);
    body["mode"] = serde_json::json!("local");
    let (s, _, _) = api_call(
        &app,
        Method::PUT,
        "/admin/console/settings",
        Some(body.clone()),
        Some(0),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    // Create an admin through the API, then switch.
    let (s, h, v) = api_call(
        &app,
        Method::POST,
        "/admin/console/users",
        Some(serde_json::json!({"username": "ada", "role": "admin"})),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert!(h.contains_key(header::ETAG));
    let otp = v["oneTimePassword"].as_str().unwrap().to_string();
    assert!(v["user"].get("passwordHash").is_none());
    let id = v["user"]["id"].as_str().unwrap().to_string();
    let (s, h, _) = api_call(
        &app,
        Method::PUT,
        "/admin/console/settings",
        Some(body.clone()),
        Some(0),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h[header::ETAG], "\"1\"");
    assert_eq!(rt.mode(), ConsoleAuthMode::Local);
    // Stale revision: 412.
    assert_eq!(
        api_call(
            &app,
            Method::PUT,
            "/admin/console/settings",
            Some(body.clone()),
            Some(0)
        )
        .await
        .0,
        StatusCode::PRECONDITION_FAILED
    );
    // A client secret cannot be stored without a key ring.
    let mut with_oidc = body.clone();
    with_oidc["oidc"] = serde_json::json!({"issuer": "https://idp.example", "clientId": "c"});
    with_oidc["secrets"] = serde_json::json!({"oidc.clientSecret": {"set": "x"}});
    let (s, _, v) = api_call(
        &app,
        Method::PUT,
        "/admin/console/settings",
        Some(with_oidc),
        Some(1),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");

    // Users: get, update with If-Match, last-admin rule, reset, delete.
    let (s, h, _) = api_call(
        &app,
        Method::GET,
        &format!("/admin/console/users/{id}"),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let rev: u64 = h[header::ETAG]
        .to_str()
        .unwrap()
        .trim_matches('"')
        .parse()
        .unwrap();
    let (s, _, _) = api_call(
        &app,
        Method::PUT,
        &format!("/admin/console/users/{id}"),
        Some(serde_json::json!({"role": "viewer"})),
        Some(rev),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "last admin");
    let (s, _, v) = api_call(
        &app,
        Method::PUT,
        &format!("/admin/console/users/{id}"),
        Some(serde_json::json!({"displayName": "Ada L."})),
        Some(rev),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["displayName"], "Ada L.");
    let (s, _, v) = api_call(
        &app,
        Method::POST,
        &format!("/admin/console/users/{id}/$reset-password"),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_ne!(v["oneTimePassword"].as_str().unwrap(), otp);
    let (s, _, v) = api_call(
        &app,
        Method::POST,
        "/admin/console/users",
        Some(serde_json::json!({"username": "Ada"})),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(
        api_call(
            &app,
            Method::DELETE,
            &format!("/admin/console/users/{id}"),
            None,
            None
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (_, _, v) = api_call(&app, Method::GET, "/admin/console/users", None, None).await;
    assert_eq!(v["users"].as_array().unwrap().len(), 1);
    // Bindings and sessions.
    let (s, _, _) = api_call(
        &app,
        Method::PUT,
        "/admin/console/bindings/local%3Aada/lab",
        Some(serde_json::json!({"role": "editor"})),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_, _, v) = api_call(&app, Method::GET, "/admin/console/bindings", None, None).await;
    assert_eq!(v["bindings"][0]["subject"], "local:ada");
    assert_eq!(
        api_call(
            &app,
            Method::DELETE,
            "/admin/console/bindings/local%3Aada/lab",
            None,
            None
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let _ = sign_in(&rt, "admin", BG).await;
    let (_, _, v) = api_call(&app, Method::GET, "/admin/console/sessions", None, None).await;
    let key = v["sessions"][0]["key"].as_str().unwrap().to_string();
    assert_eq!(
        api_call(
            &app,
            Method::DELETE,
            &format!("/admin/console/sessions/{key}"),
            None,
            None
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        api_call(
            &app,
            Method::DELETE,
            &format!("/admin/console/sessions/{key}"),
            None,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn mode_none_refuses_break_glass_off_and_anonymous_admin() {
    let (rt, _) = runtime(env(&[]), Some(BG), opts()).await;
    let mut s = rt.settings();
    s.break_glass = false;
    assert!(matches!(
        rt.save_settings("t", s, SecretInput::Keep, 0, OidcSwitchCheck::Probe)
            .await,
        Err(ConsoleError::Invalid(_))
    ));
    let mut s = rt.settings();
    s.anonymous_role = Some(ConsoleRole::Admin);
    assert!(matches!(
        rt.save_settings("t", s, SecretInput::Keep, 0, OidcSwitchCheck::Probe)
            .await,
        Err(ConsoleError::Invalid(_))
    ));
}

// ------------------------------------------------------------------- OIDC

/// An issued code: PKCE challenge, nonce, roles.
type IssuedCode = (String, String, Vec<String>);

#[derive(Default)]
struct IdpState {
    /// code → issued code
    codes: Mutex<std::collections::HashMap<String, IssuedCode>>,
    refreshes: Mutex<u32>,
    refresh_roles: Mutex<Vec<String>>,
}

fn jwt(claims: serde_json::Value) -> String {
    format!(
        "{}.{}.sig",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    )
}

/// An in-process OpenID provider: discovery, token endpoint (code with PKCE
/// check, refresh), end-session. Returns its base URL.
async fn mock_idp(state: Arc<IdpState>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let issuer = base.clone();
    let app = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get({
                let base = base.clone();
                move || async move {
                    axum::Json(serde_json::json!({
                        "issuer": base,
                        "authorization_endpoint": format!("{base}/authorize"),
                        "token_endpoint": format!("{base}/token"),
                        "end_session_endpoint": format!("{base}/logout"),
                    }))
                }
            }),
        )
        .route(
            "/token",
            axum::routing::post({
                let state = state.clone();
                move |form: axum::Form<std::collections::HashMap<String, String>>| {
                    let state = state.clone();
                    let issuer = issuer.clone();
                    async move {
                        let now = unix_now();
                        let f = form.0;
                        let (sub_roles, nonce) = match f.get("grant_type").map(String::as_str) {
                            Some("authorization_code") => {
                                let code = f.get("code").cloned().unwrap_or_default();
                                let Some((challenge, nonce, roles)) = state.codes.lock().unwrap().remove(&code) else {
                                    return (StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({"error": "invalid_grant"}))).into_response();
                                };
                                let verifier = f.get("code_verifier").cloned().unwrap_or_default();
                                if helios_auth::code_challenge_s256(&verifier) != challenge {
                                    return (StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({"error": "invalid_grant", "error_description": "PKCE"}))).into_response();
                                }
                                (roles, Some(nonce))
                            }
                            Some("refresh_token") => {
                                *state.refreshes.lock().unwrap() += 1;
                                if f.get("refresh_token").map(String::as_str) != Some("rt-1") {
                                    return (StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({"error": "invalid_grant"}))).into_response();
                                }
                                (state.refresh_roles.lock().unwrap().clone(), None)
                            }
                            _ => return StatusCode::BAD_REQUEST.into_response(),
                        };
                        let mut id = serde_json::json!({
                            "iss": issuer, "aud": "hfs-web", "sub": "user-1", "exp": now + 300,
                            "name": "Olive IdP", "groups": sub_roles,
                        });
                        if let Some(n) = nonce {
                            id["nonce"] = serde_json::json!(n);
                        }
                        axum::Json(serde_json::json!({
                            "access_token": jwt(serde_json::json!({"sub": "user-1", "realm_access": {"roles": sub_roles}})),
                            "id_token": jwt(id),
                            "refresh_token": "rt-1",
                            "expires_in": 1,
                        }))
                        .into_response()
                    }
                }
            }),
        );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    base
}

/// The browser's trip to the IdP: parse the authorize URL and issue a code.
fn authorize(state: &IdpState, url: &str, roles: &[&str]) -> (String, String) {
    let u = url::Url::parse(url).unwrap();
    let q: std::collections::HashMap<String, String> = u.query_pairs().into_owned().collect();
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["redirect_uri"], "http://hfs.test/ui/callback");
    let code = format!("code-{}", helios_auth::random_token(8));
    state.codes.lock().unwrap().insert(
        code.clone(),
        (
            q["code_challenge"].clone(),
            q["nonce"].clone(),
            roles.iter().map(|s| s.to_string()).collect(),
        ),
    );
    (code, q["state"].clone())
}

async fn oidc_rt(idp: &str, extra: &[(&str, &str)]) -> Arc<ConsoleAuth> {
    let mut pairs = vec![
        ("HFS_CONSOLE_AUTH_MODE", "oidc"),
        ("HFS_CONSOLE_OIDC_ISSUER", idp),
        ("HFS_CONSOLE_OIDC_CLIENT_ID", "hfs-web"),
        ("HFS_CONSOLE_OIDC_ROLE_CLAIM", "groups"),
        ("HFS_CONSOLE_OIDC_ADMIN_VALUES", "hfs-admin"),
        ("HFS_CONSOLE_OIDC_EDITOR_VALUES", "hfs-editor"),
    ];
    pairs.extend_from_slice(extra);
    runtime(env(&pairs), Some(BG), opts()).await.0
}

fn with_cookie(set: &HeaderValue) -> HeaderMap {
    let mut h = site_headers();
    h.insert(
        header::COOKIE,
        HeaderValue::from_str(set.to_str().unwrap().split(';').next().unwrap()).unwrap(),
    );
    h
}

#[tokio::test(flavor = "multi_thread")]
async fn oidc_code_pkce_flow_role_mapping_refresh_and_logout() {
    let idp_state = Arc::new(IdpState::default());
    let idp = mock_idp(idp_state.clone()).await;
    let rt = oidc_rt(&idp, &[]).await;
    // The mock's access tokens live one second, so every resolve refreshes.
    *idp_state.refresh_roles.lock().unwrap() = vec!["hfs-editor".into()];

    let (pending, url) = rt
        .begin_oidc(&site_headers(), "/ui/resources", None)
        .await
        .unwrap();
    let pc = pending.to_str().unwrap().to_string();
    assert!(
        pc.starts_with("hfs_console_login=")
            && pc.contains("SameSite=Lax")
            && pc.contains("HttpOnly")
    );
    let (code, state) = authorize(&idp_state, &url, &["hfs-editor"]);
    // Wrong state is refused (and consumes the pending login).
    let h = with_cookie(&pending);
    assert!(matches!(
        rt.complete_oidc(&h, None, Some(&code), Some("forged"), None)
            .await,
        OidcOutcome::Failed(_)
    ));
    // Start again: success.
    let (pending, url) = rt
        .begin_oidc(&site_headers(), "/ui/resources", None)
        .await
        .unwrap();
    let (code, state2) = authorize(&idp_state, &url, &["hfs-editor"]);
    assert_ne!(state, state2);
    let h = with_cookie(&pending);
    let cookies = match rt
        .complete_oidc(&h, None, Some(&code), Some(&state2), None)
        .await
    {
        OidcOutcome::Success { cookies, next } => {
            assert_eq!(next, "/ui/resources");
            cookies
        }
        other => panic!("{other:?}"),
    };
    // Replay of the same callback is refused.
    assert!(matches!(
        rt.complete_oidc(&h, None, Some(&code), Some(&state2), None)
            .await,
        OidcOutcome::Failed(_)
    ));
    let mut sh = HeaderMap::new();
    sh.insert(
        header::COOKIE,
        HeaderValue::from_str(&cookie_header(&cookies)).unwrap(),
    );
    let r = rt.resolve(&sh).await.unwrap();
    assert_eq!(r.principal.role, ConsoleRole::Editor);
    assert_eq!(r.principal.display_name, "Olive IdP");
    assert_eq!(r.principal.subject, format!("oidc:{idp}|user-1"));

    // The access token expired (expires_in 1): the next resolve refreshes and
    // re-maps the role from the new tokens.
    *idp_state.refresh_roles.lock().unwrap() = vec!["hfs-admin".into()];
    tokio::time::sleep(Duration::from_millis(50)).await;
    let r = rt.resolve(&sh).await.unwrap();
    assert!(*idp_state.refreshes.lock().unwrap() >= 1);
    assert_eq!(r.principal.role, ConsoleRole::Admin);
    // A refresh whose tokens map to no role ends the session.
    *idp_state.refresh_roles.lock().unwrap() = vec!["nobody".into()];
    let mut s = r.session.clone();
    s.oidc.as_mut().unwrap().access_expires_unix = 0;
    rt.put_session(&r.key, &s).await.unwrap();
    rt.cache_lock().clear();
    assert!(rt.resolve(&sh).await.is_none());

    // Sign-out sends the browser to the IdP's end-session endpoint.
    *idp_state.refresh_roles.lock().unwrap() = vec!["hfs-admin".into()];
    let (pending, url) = rt.begin_oidc(&site_headers(), "/ui", None).await.unwrap();
    let (code, st) = authorize(&idp_state, &url, &["hfs-admin"]);
    let OidcOutcome::Success { cookies, .. } = rt
        .complete_oidc(&with_cookie(&pending), None, Some(&code), Some(&st), None)
        .await
    else {
        panic!()
    };
    let mut lh = site_headers();
    lh.insert(
        header::COOKIE,
        HeaderValue::from_str(&cookie_header(&cookies)).unwrap(),
    );
    let out = rt.logout(&lh).await;
    assert!(
        out.location.starts_with(&format!("{idp}/logout?")),
        "{}",
        out.location
    );
    assert!(out.location.contains("id_token_hint="));
}

#[tokio::test(flavor = "multi_thread")]
async fn oidc_without_a_matching_role_gets_no_access_unless_default_role() {
    let idp_state = Arc::new(IdpState::default());
    let idp = mock_idp(idp_state.clone()).await;
    let rt = oidc_rt(&idp, &[]).await;
    let (pending, url) = rt.begin_oidc(&site_headers(), "/ui", None).await.unwrap();
    let (code, st) = authorize(&idp_state, &url, &["staff"]);
    assert!(matches!(
        rt.complete_oidc(&with_cookie(&pending), None, Some(&code), Some(&st), None)
            .await,
        OidcOutcome::NoAccess { .. }
    ));
    let rt = oidc_rt(&idp, &[("HFS_CONSOLE_OIDC_DEFAULT_ROLE", "viewer")]).await;
    let (pending, url) = rt.begin_oidc(&site_headers(), "/ui", None).await.unwrap();
    let (code, st) = authorize(&idp_state, &url, &["staff"]);
    assert!(matches!(
        rt.complete_oidc(&with_cookie(&pending), None, Some(&code), Some(&st), None)
            .await,
        OidcOutcome::Success { .. }
    ));
    // IdP error and missing pending cookie.
    assert!(matches!(
        rt.complete_oidc(&site_headers(), None, None, None, Some("access_denied"))
            .await,
        OidcOutcome::Failed(_)
    ));
    assert!(matches!(
        rt.complete_oidc(&site_headers(), None, Some("c"), Some("s"), None)
            .await,
        OidcOutcome::Failed(_)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn switching_to_oidc_in_the_console_needs_a_passed_test_sign_in() {
    let idp_state = Arc::new(IdpState::default());
    let idp = mock_idp(idp_state.clone()).await;
    // Console-typed issuer: the outbound policy must allow the loopback IdP.
    let mut o = opts();
    o.outbound = helios_endpoints::OutboundPolicy::parse("127.0.0.1/32");
    let (rt, _) = runtime(env(&[]), Some(BG), o).await;
    let admin = sign_in(&rt, "admin", BG).await;
    let mut ah = site_headers();
    ah.insert(
        header::COOKIE,
        HeaderValue::from_str(&cookie_header(&admin)).unwrap(),
    );
    let admin_session = rt.resolve(&ah).await.unwrap();

    // Save the OIDC client while staying in mode none.
    let mut s = rt.settings();
    s.oidc = Some(helios_endpoints::console::OidcClientSettings {
        issuer: idp.clone(),
        client_id: "hfs-web".into(),
        client_secret: None,
        scopes: "openid profile".into(),
        authorization_endpoint: None,
        token_endpoint: None,
        end_session_endpoint: None,
        role_claim: "groups".into(),
        role_mapping: helios_endpoints::console::RoleMapping {
            admin: vec!["hfs-admin".into()],
            ..Default::default()
        },
        default_role: None,
    });
    let rev = rt
        .save_settings(
            "admin",
            s.clone(),
            SecretInput::Keep,
            0,
            OidcSwitchCheck::Tested(None),
        )
        .await
        .unwrap();
    // Switch without a test: refused.
    let mut sw = s.clone();
    sw.mode = ConsoleAuthMode::Oidc;
    assert!(matches!(
        rt.save_settings(
            "admin",
            sw.clone(),
            SecretInput::Keep,
            rev,
            OidcSwitchCheck::Tested(None)
        )
        .await,
        Err(ConsoleError::Forbidden(_))
    ));
    // Test sign-in round trip in the admin's session.
    let (pending, url) = rt
        .begin_oidc(&ah, "/ui/settings/auth", Some(&admin_session.key))
        .await
        .unwrap();
    let (code, st) = authorize(&idp_state, &url, &["hfs-admin"]);
    match rt
        .complete_oidc(&with_cookie(&pending), None, Some(&code), Some(&st), None)
        .await
    {
        OidcOutcome::Tested { role, .. } => assert_eq!(role, Some(ConsoleRole::Admin)),
        other => panic!("{other:?}"),
    }
    let passed = rt.resolve(&ah).await.unwrap().session.oidc_test_passed;
    assert_eq!(passed, rt.oidc_fingerprint());
    rt.save_settings(
        "admin",
        sw,
        SecretInput::Keep,
        rev,
        OidcSwitchCheck::Tested(passed),
    )
    .await
    .unwrap();
    assert_eq!(rt.mode(), ConsoleAuthMode::Oidc);
    // The break-glass session survives the switch.
    assert!(rt.resolve(&ah).await.is_some());

    // A strict policy refuses the loopback IdP for console-typed issuers.
    let (strict, _) = runtime(env(&[]), Some(BG), opts()).await;
    let o = s.oidc.clone().unwrap();
    assert!(strict.probe_oidc(&o).await.is_err());
}

#[tokio::test]
async fn resolver_trait_reads_the_cookie_header() {
    let (rt, _) = local_rt(&[]).await;
    add_user(&rt, "bob", ConsoleRole::Editor, "tidy-Lantern-41-orbit").await;
    let set = sign_in(&rt, "bob", "tidy-Lantern-41-orbit").await;
    let resolver: Arc<dyn helios_endpoints::console::ConsoleSessionResolver> = rt.clone();
    let p = resolver.resolve(&cookie_header(&set)).await.unwrap();
    assert_eq!(p.subject, "local:bob");
    assert_eq!(p.role, ConsoleRole::Editor);
    assert!(resolver.resolve("hfs_console=bogus").await.is_none());
    assert!(resolver.resolve("").await.is_none());
}

#[tokio::test]
async fn client_secret_is_sealed_and_rewrapped_under_the_active_key() {
    let k1 = helios_endpoints::SecretKeyRing::generate_spec("k1");
    let k2 = helios_endpoints::SecretKeyRing::generate_spec("k2");
    let ring1 = Arc::new(helios_endpoints::SecretKeyRing::parse(&k1).unwrap());
    let ring2 = Arc::new(helios_endpoints::SecretKeyRing::parse(&format!("{k2},{k1}")).unwrap());
    let store = Arc::new(memory::MemoryConsoleStore::new());
    let make = |ring: Arc<helios_endpoints::SecretKeyRing>| {
        let store = store.clone();
        async move {
            ConsoleAuth::new(
                env(&[]),
                store,
                Arc::new(MemorySessionStore::default()),
                Arc::new(crate::store::testing::MemoryStore::default()),
                AdminGate::Open,
                ConsoleOptions {
                    key_ring: Some(ring),
                    ..opts()
                },
            )
            .await
        }
    };
    let a = make(ring1).await;
    let mut s = a.settings();
    s.oidc = Some(helios_endpoints::console::OidcClientSettings {
        issuer: "https://idp.example".into(),
        client_id: "c".into(),
        client_secret: None,
        scopes: "openid".into(),
        authorization_endpoint: None,
        token_endpoint: None,
        end_session_endpoint: None,
        role_claim: "roles".into(),
        role_mapping: Default::default(),
        default_role: None,
    });
    a.save_settings(
        "t",
        s,
        SecretInput::Set("top-secret".into()),
        0,
        OidcSwitchCheck::Probe,
    )
    .await
    .unwrap();
    let sealed = a
        .stored_settings()
        .0
        .unwrap()
        .oidc
        .unwrap()
        .client_secret
        .unwrap();
    assert_eq!(sealed.key_id(), "k1");
    assert!(!sealed.envelope().contains("top-secret"));
    // The API never shows it.
    let shown = api::redacted_settings(&a.settings()).to_string();
    assert!(
        !shown.contains(sealed.envelope()) && !shown.contains("top-secret"),
        "{shown}"
    );
    assert_eq!(
        a.rewrap_secrets("t").await.unwrap(),
        0,
        "already under the active key"
    );
    // Rotate: k2 active, k1 still readable.
    let b = make(ring2.clone()).await;
    assert_eq!(b.rewrap_secrets("t").await.unwrap(), 1);
    let resealed = b
        .stored_settings()
        .0
        .unwrap()
        .oidc
        .unwrap()
        .client_secret
        .unwrap();
    assert_eq!(resealed.key_id(), "k2");
    assert_eq!(
        ring2
            .open(&resealed, settings::CLIENT_SECRET_AAD)
            .unwrap()
            .expose_str(),
        Some("top-secret")
    );
    // Keep leaves it, Clear removes it.
    let s = b.stored_settings().0.unwrap();
    let rev = b.stored_settings().1;
    b.save_settings(
        "t",
        s.clone(),
        SecretInput::Keep,
        rev,
        OidcSwitchCheck::Probe,
    )
    .await
    .unwrap();
    assert!(
        b.stored_settings()
            .0
            .unwrap()
            .oidc
            .unwrap()
            .client_secret
            .is_some()
    );
    b.save_settings("t", s, SecretInput::Clear, rev + 1, OidcSwitchCheck::Probe)
        .await
        .unwrap();
    assert!(
        b.stored_settings()
            .0
            .unwrap()
            .oidc
            .unwrap()
            .client_secret
            .is_none()
    );
}

#[tokio::test]
async fn admin_api_viewers_read_endpoints_only() {
    let (rt, store) = local_rt(&[]).await;
    let v = add_user(&rt, "vera", ConsoleRole::Viewer, "tidy-Lantern-41-orbit").await;
    store
        .set_role_binding("local:vera", "lab", ConsoleRole::Admin, "t")
        .await
        .unwrap();
    let app = app(rt.clone());
    let set = sign_in(&rt, "vera", "tidy-Lantern-41-orbit").await;
    let c = cookie_header(&set);
    let t = csrf_of(&set);
    let w = [("origin", ORIGIN), ("x-csrf-token", t.as_str())];
    assert_eq!(
        send(&app, Method::GET, "/admin/endpoints", &c, &[])
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        send(&app, Method::PUT, "/admin/endpoints/lab", &c, &w)
            .await
            .status(),
        StatusCode::OK,
        "endpoint admin binding"
    );
    assert_eq!(
        send(&app, Method::DELETE, "/admin/endpoints/lab", &c, &w)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&app, Method::GET, "/admin/console/users", &c, &[])
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let mut h = HeaderMap::new();
    h.insert(header::COOKIE, HeaderValue::from_str(&c).unwrap());
    assert!(matches!(
        rt.authorize_admin_request(&Method::GET, "/admin/endpoints/lab", &h)
            .await,
        Some(Ok(_))
    ));
    assert!(matches!(
        rt.authorize_admin_request(&Method::POST, "/admin/endpoints", &h)
            .await,
        Some(Err(_))
    ));
    assert!(
        rt.authorize_admin_request(&Method::GET, "/admin/endpoints", &HeaderMap::new())
            .await
            .is_none()
    );
    let _ = v;
}
