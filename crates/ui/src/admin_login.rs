//! Administrator sign-in for the console's administrative pages
//! (`HFS_ADMIN_PASSWORD`, crate `helios-admin-auth`).
//!
//! * `GET /ui/admin/login?next=` renders the form; `POST /ui/admin/login`
//!   checks the credentials and sets the `hfs_admin` session cookie;
//!   `POST /ui/admin/logout` ends the session.
//! * The guard ([`protects`]) requires a session for the pages that act on
//!   the server as a whole — IG Packages, Tenants, Bulk Import, Bulk Export,
//!   SQL Export — and for every other state-changing request under `/ui`
//!   except the read-only previews (lint, run, render, diff, lookups) and the
//!   per-browser preferences (FHIR version, tenant). Browsing pages stay
//!   public, like the FHIR API they read.
//! * The cookie is `Path=/` because the IG Packages page's script calls the
//!   package API at `[base]/admin/packages`; the `hfs` binary guards that API
//!   with the same gate, which accepts the cookie as well as Basic/Bearer.
//!
//! The gate is process-wide, installed by the server before the UI is mounted
//! ([`crate::set_admin_gate`]), like the interactive OIDC login. Without one
//! installed the UI behaves as before: no admin sign-in.

use std::net::SocketAddr;

use askama::Template;
use axum::{
    Extension,
    body::Bytes,
    extract::{ConnectInfo, Query, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use helios_admin_auth::{AdminGate, LoginOutcome, is_read_only, safe_next, under};
use serde::Deserialize;

use crate::auth_shell::AuthShell;
use crate::{I18n, RequestLocale, RequestTenant, RequestVersion, WebState};

/// Browser path of the sign-in page.
pub(crate) const LOGIN_PATH: &str = "/ui/admin/login";
/// Browser path of the sign-out endpoint.
pub(crate) const LOGOUT_PATH: &str = "/ui/admin/logout";

/// The session cookie: its own name, so it never collides with the HTS
/// console's on a shared origin. `Path=/` so it reaches both `/ui` and the
/// package API under the FHIR base (`/admin/packages`, `/fhir/admin/...`).
pub const ADMIN_COOKIE: helios_admin_auth::CookieSpec = helios_admin_auth::CookieSpec {
    name: "hfs_admin",
    path: "/",
};

/// Pages that administer the server as a whole.
const ADMIN_PAGES: &[&str] = &[
    "/ui/packages",
    "/ui/tenants",
    "/ui/bulk-import",
    "/ui/bulk-export",
    "/ui/sql/export",
];

/// State-changing requests that change nothing on the server: previews the
/// public browsing pages compute, and this browser's own preferences.
const OPEN_WRITES: &[&str] = &[
    "/ui/capability-statement/json-expand",
    "/ui/sql/view-definitions/lint",
    "/ui/sql/view-definitions/complete",
    "/ui/sql/view-definitions/run",
    "/ui/sql/queries/run",
    "/ui/sql/queries/document",
    "/ui/sql/views/run",
    "/ui/sql/views/document",
    "/ui/lookup/patient-options",
    "/ui/lookup/group-options",
    "/ui/lookup/table-options",
    "/ui/editor/render",
    "/ui/json-view/render",
    "/ui/history/diff",
    "/ui/version",
    "/ui/tenant",
    // The OIDC login's own sign-out, and this sign-in (which checks the
    // request's origin itself).
    "/ui/logout",
    LOGIN_PATH,
    LOGOUT_PATH,
];

/// Whether `(method, path)` needs the administrator.
pub(crate) fn protects(method: &Method, path: &str) -> bool {
    if !under(path, "/ui") {
        return false;
    }
    if ADMIN_PAGES.iter().any(|prefix| under(path, prefix)) {
        return true;
    }
    !is_read_only(method) && !OPEN_WRITES.contains(&path)
}

/// The sign-in page.
#[derive(Template)]
#[template(path = "pages/admin-login.html")]
struct AdminLoginPage {
    i18n: I18n,
    auth: AuthShell,
    next: String,
    user: String,
    error: Option<String>,
    locked: bool,
}

#[derive(Deserialize)]
pub(crate) struct NextQuery {
    next: Option<String>,
}

fn target(next: Option<&str>) -> String {
    safe_next(next, "/ui", "/ui")
}

struct PageInput {
    status: StatusCode,
    next: String,
    user: String,
    error: Option<String>,
    locked: bool,
}

fn page(locale: RequestLocale, headers: &HeaderMap, input: PageInput) -> Response {
    let i18n = I18n::new(locale);
    let suffix = format!(
        "&{}",
        form_urlencoded::Serializer::new(String::new())
            .append_pair("next", &input.next)
            .finish()
    );
    let mut response = crate::render(AdminLoginPage {
        auth: AuthShell::new(&i18n, Some(headers), &suffix),
        i18n,
        next: input.next,
        user: input.user,
        error: input.error,
        locked: input.locked,
    });
    if response.status() == StatusCode::OK {
        *response.status_mut() = input.status;
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `GET /ui/admin/login`.
pub(crate) async fn login_page(
    State(state): State<WebState>,
    locale: RequestLocale,
    _rv: RequestVersion,
    _rt: RequestTenant,
    console: Option<Extension<std::sync::Arc<helios_admin_auth::console::ConsoleAuth>>>,
    headers: HeaderMap,
    Query(query): Query<NextQuery>,
) -> Response {
    let next = target(query.next.as_deref());
    // With console sign-in (P3) the break-glass administrator signs in on
    // the console's own page.
    if console.is_some() {
        return Redirect::to(&helios_admin_auth::console::guard::sign_in_target(
            &next, true,
        ))
        .into_response();
    }
    let gate = &state.admin_gate;
    match gate {
        AdminGate::Open => Redirect::to(&next).into_response(),
        AdminGate::Locked(_) => page(
            locale,
            &headers,
            PageInput {
                status: StatusCode::SERVICE_UNAVAILABLE,
                next,
                user: String::new(),
                error: None,
                locked: true,
            },
        ),
        AdminGate::Password(auth) => {
            if gate.session_user(&headers).is_some() {
                return Redirect::to(&next).into_response();
            }
            let user = auth.user().to_string();
            page(
                locale,
                &headers,
                PageInput {
                    status: StatusCode::OK,
                    next,
                    user,
                    error: None,
                    locked: false,
                },
            )
        }
    }
}

/// `POST /ui/admin/login` (form: `username`, `password`, `next`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn login_submit(
    State(state): State<WebState>,
    locale: RequestLocale,
    _rv: RequestVersion,
    _rt: RequestTenant,
    connect: Option<Extension<ConnectInfo<SocketAddr>>>,
    console: Option<Extension<std::sync::Arc<helios_admin_auth::console::ConsoleAuth>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if console.is_some() {
        return crate::login::login_local_submit(locale, console, connect, headers, body).await;
    }
    let mut user = String::new();
    let mut password = String::new();
    let mut next = None;
    for (key, value) in form_urlencoded::parse(&body) {
        match key.as_ref() {
            "username" => user = value.into_owned(),
            "password" => password = value.into_owned(),
            "next" => next = Some(value.into_owned()),
            _ => {}
        }
    }
    let next = target(next.as_deref());
    let peer = connect.map(|c| c.0.0.ip());
    let i18n = I18n::new(locale);
    let failed = |status, error: Option<String>, user: String, locked| {
        page(
            locale,
            &headers,
            PageInput {
                status,
                next: next.clone(),
                user,
                error,
                locked,
            },
        )
    };
    match state
        .admin_gate
        .login_shared(&headers, peer, user.trim(), &password)
        .await
    {
        LoginOutcome::Success { set_cookie } => {
            let mut response = Redirect::to(&next).into_response();
            response
                .headers_mut()
                .append(header::SET_COOKIE, set_cookie);
            response
        }
        LoginOutcome::Invalid => failed(
            StatusCode::UNAUTHORIZED,
            Some(i18n.t("admin-login-invalid")),
            user,
            false,
        ),
        LoginOutcome::RateLimited(left) => {
            let minutes = left.as_secs().div_ceil(60).max(1);
            let mut response = failed(
                StatusCode::TOO_MANY_REQUESTS,
                Some(i18n.t_arg("admin-login-throttled", "minutes", minutes)),
                user,
                false,
            );
            if let Ok(v) = HeaderValue::from_str(&left.as_secs().max(1).to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, v);
            }
            response
        }
        LoginOutcome::CrossOrigin => failed(
            StatusCode::FORBIDDEN,
            Some(i18n.t("admin-login-cross-site")),
            user,
            false,
        ),
        LoginOutcome::NotConfigured => match state.admin_gate {
            AdminGate::Open => Redirect::to(&next).into_response(),
            _ => failed(StatusCode::SERVICE_UNAVAILABLE, None, String::new(), true),
        },
    }
}

/// `POST /ui/admin/logout`.
pub(crate) async fn logout(
    State(state): State<WebState>,
    console: Option<Extension<std::sync::Arc<helios_admin_auth::console::ConsoleAuth>>>,
    headers: HeaderMap,
) -> Response {
    if let Some(Extension(console)) = console {
        return crate::login::console_logout(&console, &headers).await;
    }
    let Some(auth) = state.admin_gate.auth() else {
        return Redirect::to("/ui").into_response();
    };
    if !auth.same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "cross-site request refused").into_response();
    }
    let mut response = Redirect::to("/ui").into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, auth.logout_shared(&headers).await);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_pages_and_server_writes_are_protected() {
        let get = Method::GET;
        let post = Method::POST;
        for path in [
            "/ui/packages",
            "/ui/packages/hl7.fhir.us.core/6.1.0",
            "/ui/tenants",
            "/ui/tenants/rows",
            "/ui/bulk-import",
            "/ui/bulk-import/keys",
            "/ui/bulk-export",
            "/ui/bulk-export/active/x/download",
            "/ui/sql/export",
            "/ui/sql/export/new",
        ] {
            assert!(protects(&get, path), "GET {path}");
        }
        for path in [
            "/ui/tenants",
            "/ui/bulk-import",
            "/ui/bulk-import/test-auth",
            "/ui/bulk-export",
            "/ui/sql/export",
            "/ui/sql/view-definitions",
            "/ui/sql/queries",
            "/ui/sql/views",
            "/ui/something-new",
        ] {
            assert!(protects(&post, path), "POST {path}");
        }
        assert!(protects(&Method::DELETE, "/ui/tenants/acme"));
        for path in [
            "/ui",
            "/ui/resources",
            "/ui/sql/view-definitions",
            "/ui/subscriptions",
            "/ui/editor",
            "/ui/admin/login",
            "/ui/tenant/options",
            "/Patient",
            "/admin/packages",
        ] {
            assert!(!protects(&get, path), "GET {path}");
        }
        for path in OPEN_WRITES {
            assert!(!protects(&post, path), "POST {path}");
        }
        // Outside /ui is the API's business (the hfs binary guards /admin).
        assert!(!protects(&post, "/Patient"));
    }
}
