//! Console sign-in (P3) roles on the endpoint admin API, through
//! `ConsoleAdminAuthorizer` with the console runtime on every request (as the
//! hfs wiring puts it): viewer = read-only; an admin binding on one endpoint
//! = writes to that endpoint only; global admin = everything (create,
//! delete, `$rewrap` including the console's own secret); no session = the
//! credential authorizer (401 without a credential). Cookie writes need a
//! same-site `Origin` and the session's `X-CSRF-Token`.

#[path = "admin_endpoints_suite/mod.rs"]
mod suite;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderName, HeaderValue, Method, StatusCode, header};
use helios_admin_auth::CookieSecure;
use helios_admin_auth::console::testing::{env, runtime, session};
use helios_admin_auth::console::{ConsoleAuth, ConsoleOptions, NewUser};
use helios_endpoints::console::{ConsoleRole, ConsoleVia};
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use serde_json::{Value, json};
use suite::{Harness, Options};

struct Who {
    cookie: String,
    csrf: String,
}

async fn console() -> Arc<ConsoleAuth> {
    let (rt, _) = runtime(
        env(&[("HFS_CONSOLE_AUTH_MODE", "local")]),
        Some("break-glass-password-1"),
        ConsoleOptions {
            public_base_url: "http://hfs.test".into(),
            recheck: Duration::from_secs(0),
            cookie_secure: CookieSecure::Never,
            ..Default::default()
        },
    )
    .await;
    rt
}

/// A local account `name` with `role`, signed in.
async fn who(rt: &ConsoleAuth, name: &str, role: ConsoleRole) -> Who {
    let (mut user, _) = rt
        .create_user(
            "test",
            NewUser {
                username: name.into(),
                role: Some(role),
                password: Some("a-long-enough-test-password-42".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    user.must_change_password = false;
    let rev = user.revision;
    let user = rt.store().update_user(user, rev).await.unwrap();
    let (cookie, csrf) = session(
        rt,
        &format!("local:{name}"),
        role,
        ConsoleVia::Local,
        Some(user.id.clone()),
    )
    .await;
    Who { cookie, csrf }
}

async fn harness(rt: Arc<ConsoleAuth>) -> Harness {
    let backend = SqliteBackend::with_config(
        ":memory:",
        SqliteBackendConfig {
            data_dir: Some(suite::data_dir()),
            ..Default::default()
        },
    )
    .unwrap();
    backend.init_schema().unwrap();
    Harness::memory(
        Arc::new(backend),
        Options {
            console: Some(rt),
            ..Options::default()
        },
    )
    .await
}

/// `csrf`: send the session's token (false = forget it).
async fn call(
    h: &Harness,
    who: Option<&Who>,
    method: Method,
    path: &str,
    body: Option<Value>,
    csrf: bool,
) -> (StatusCode, Value) {
    let mut r = h
        .server
        .method(method, path)
        .add_header(header::HOST, HeaderValue::from_static("hfs.test"))
        .add_header(header::ORIGIN, HeaderValue::from_static("http://hfs.test"));
    if let Some(w) = who {
        r = r.add_header(header::COOKIE, HeaderValue::from_str(&w.cookie).unwrap());
        if csrf {
            r = r.add_header(
                HeaderName::from_static("x-csrf-token"),
                HeaderValue::from_str(&w.csrf).unwrap(),
            );
        }
    }
    // Writes here do not exercise concurrency: accept any revision.
    r = r.add_header(header::IF_MATCH, HeaderValue::from_static("*"));
    if let Some(b) = body {
        r = r.json(&b);
    }
    suite::scan(&r.await)
}

#[tokio::test]
async fn console_roles_on_the_admin_api() {
    let rt = console().await;
    let h = harness(rt.clone()).await;
    // Fixture endpoints created by the break-glass credential (no session).
    assert_eq!(
        h.create(json!({"name": "lab"})).await.0,
        StatusCode::CREATED
    );
    assert_eq!(
        h.create(json!({"name": "ward"})).await.0,
        StatusCode::CREATED
    );

    // Anonymous: no session, no credential → the credential authorizer says 401.
    let (s, o) = call(&h, None, Method::GET, "/admin/endpoints", None, false).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{o}");

    // Viewer: read-only.
    let viewer = who(&rt, "vera", ConsoleRole::Viewer).await;
    let (s, o) = call(
        &h,
        Some(&viewer),
        Method::GET,
        "/admin/endpoints",
        None,
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{o}");
    let (s, _) = call(
        &h,
        Some(&viewer),
        Method::GET,
        "/admin/endpoints/lab/history",
        None,
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, o) = call(
        &h,
        Some(&viewer),
        Method::PATCH,
        "/admin/endpoints/lab",
        Some(json!({"displayName": "x"})),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{o}");
    assert!(o.to_string().contains("admin role on endpoint lab"), "{o}");
    let (s, _) = call(
        &h,
        Some(&viewer),
        Method::POST,
        "/admin/endpoints",
        Some(json!({"name": "v"})),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Per-endpoint admin: a viewer bound as admin on `lab`.
    rt.store()
        .set_role_binding("local:eddy", "lab", ConsoleRole::Admin, "test")
        .await
        .unwrap();
    let eddy = who(&rt, "eddy", ConsoleRole::Viewer).await;
    let (s, o) = call(
        &h,
        Some(&eddy),
        Method::PATCH,
        "/admin/endpoints/lab",
        Some(json!({"displayName": "Lab by Eddy"})),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{o}");
    assert!(
        h.audited("local:eddy"),
        "the console subject is the audited actor"
    );
    // Same write without the CSRF token: refused.
    let (s, o) = call(
        &h,
        Some(&eddy),
        Method::PATCH,
        "/admin/endpoints/lab",
        Some(json!({"displayName": "again"})),
        false,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{o}");
    let (s, _) = call(
        &h,
        Some(&eddy),
        Method::POST,
        "/admin/endpoints/lab/$disable",
        None,
        true,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // Other endpoints, delete and create need more.
    let (s, _) = call(
        &h,
        Some(&eddy),
        Method::PATCH,
        "/admin/endpoints/ward",
        Some(json!({"displayName": "x"})),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(
        &h,
        Some(&eddy),
        Method::DELETE,
        "/admin/endpoints/lab?confirm=lab",
        None,
        true,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(
        &h,
        Some(&eddy),
        Method::POST,
        "/admin/endpoints",
        Some(json!({"name": "e"})),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Global admin: everything.
    let ada = who(&rt, "ada", ConsoleRole::Admin).await;
    let (s, o) = call(
        &h,
        Some(&ada),
        Method::POST,
        "/admin/endpoints",
        Some(json!({"name": "new"})),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{o}");
    let (s, o) = call(
        &h,
        Some(&ada),
        Method::DELETE,
        "/admin/endpoints/new?confirm=new",
        None,
        true,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{o}");
    let (s, o) = call(
        &h,
        Some(&ada),
        Method::POST,
        "/admin/secrets/$rewrap",
        None,
        true,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{o}");
    let scopes: Vec<&str> = o["reports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["scope"].as_str().unwrap())
        .collect();
    assert_eq!(scopes, vec!["endpoints", "console"], "{o}");

    // A stale or unknown cookie is not a session: back to the credential path.
    let ghost = Who {
        cookie: "hfs_console=nope".into(),
        csrf: String::new(),
    };
    let (s, _) = call(
        &h,
        Some(&ghost),
        Method::GET,
        "/admin/endpoints",
        None,
        false,
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}
