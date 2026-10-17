//! Administrator sign-in for the terminology management pages
//! (`HTS_ADMIN_PASSWORD`, crate `helios-admin-auth`).
//!
//! * `GET /hts/login?next=` renders the form; `POST /hts/login` checks the
//!   credentials and sets the `hts_admin` session cookie (`Path=/ui/hts`);
//!   `POST /hts/logout` ends the session.
//! * The guard ([`protects`]) requires a session for everything that manages
//!   terminology — `/hts/terminology`, `/hts/jobs`, `/hts/import` — and for
//!   any write outside the read-only workbenches. Browsing CodeSystems,
//!   ValueSets and ConceptMaps and running `$lookup`/`$expand`/… stays public
//!   like the terminology API it fronts.
//! * `/hts/api/admin` (the admin API the browser code calls) is not covered
//!   here: the server mounts it already wrapped in the API guard, which also
//!   accepts this session cookie.
//!
//! The signed-in user rides a task-local for the request, so every page's
//! account menu can offer Sign out without each page threading it through.

use std::net::SocketAddr;
use std::sync::Arc;

use askama::Template;
use axum::{
    Extension, Router,
    body::Bytes,
    extract::{ConnectInfo, Query, Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use helios_admin_auth::{AdminGate, AdminUser, LoginOutcome, is_read_only, safe_next, under};
use serde::Deserialize;

use crate::i18n::{I18n, RequestLocale};
use crate::{Chrome, HtsUiState, render_page};

/// Browser path of the sign-in page.
pub(crate) const LOGIN_PATH: &str = "/ui/hts/login";
/// Browser path of the sign-out endpoint.
pub(crate) const LOGOUT_PATH: &str = "/ui/hts/logout";
/// The session cookie: its own name, so it never collides with the HFS
/// console's on a shared origin, and scoped to the console's prefix.
pub const COOKIE: helios_admin_auth::CookieSpec = helios_admin_auth::CookieSpec {
    name: "hts_admin",
    path: "/ui/hts",
};

tokio::task_local! {
    static SIGNED_IN: Option<String>;
}

/// The administrator signed in on the current request, if any.
pub(crate) fn signed_in() -> Option<String> {
    SIGNED_IN.try_with(|user| user.clone()).ok().flatten()
}

/// Paths (inside the `/ui` mount) that need the administrator.
pub(crate) fn protects(method: &Method, path: &str) -> bool {
    if under(path, "/hts/api/admin") {
        return false;
    }
    if ["/hts/terminology", "/hts/jobs", "/hts/import"]
        .iter()
        .any(|prefix| under(path, prefix))
    {
        return true;
    }
    if is_read_only(method) {
        return false;
    }
    // A write anywhere else is administrative unless it is one of the
    // read-only workbench forms (lookup, validate, expand, translate, the
    // JSON folds) or the sign-in itself.
    ![
        "/hts/code-systems",
        "/hts/value-sets",
        "/hts/concept-maps",
        "/hts/concepts",
        "/hts/capability-statement",
        "/hts/login",
        "/hts/logout",
    ]
    .iter()
    .any(|prefix| under(path, prefix))
}

/// Middleware: makes the guard's [`AdminUser`] visible to page rendering.
pub(crate) async fn scope_signed_in(request: Request, next: Next) -> Response {
    let user = request.extensions().get::<AdminUser>().map(|u| u.0.clone());
    SIGNED_IN.scope(user, next.run(request)).await
}

pub(crate) fn routes() -> Router<Arc<HtsUiState>> {
    Router::new()
        .route("/hts/login", get(login_page).post(login_submit))
        .route("/hts/logout", post(logout))
}

#[derive(Template)]
#[template(path = "pages/login.html")]
struct LoginPage<'a> {
    chrome: Chrome<'a>,
    next: String,
    user: String,
    error: Option<String>,
    locked: bool,
}

#[derive(Deserialize)]
struct NextQuery {
    next: Option<String>,
}

fn target(next: Option<&str>) -> String {
    safe_next(next, "/ui/hts", "/ui/hts")
}

fn page(
    state: &HtsUiState,
    locale: RequestLocale,
    status: StatusCode,
    next: String,
    user: String,
    error: Option<String>,
    locked: bool,
) -> Response {
    let page = LoginPage {
        chrome: Chrome {
            i18n: I18n::new(locale),
            active_page: "",
            fhir_version: state.fhir_version,
            version: state.version,
        },
        next,
        user,
        error,
        locked,
    };
    let mut response = render_page(page.render());
    if response.status() == StatusCode::OK {
        *response.status_mut() = status;
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().expect("static"));
    response
}

/// `GET /hts/login`.
async fn login_page(
    State(state): State<Arc<HtsUiState>>,
    Extension(gate): Extension<AdminGate>,
    locale: RequestLocale,
    headers: HeaderMap,
    Query(query): Query<NextQuery>,
) -> Response {
    let next = target(query.next.as_deref());
    match &gate {
        AdminGate::Open => Redirect::to(&next).into_response(),
        AdminGate::Locked(_) => page(
            &state,
            locale,
            StatusCode::SERVICE_UNAVAILABLE,
            next,
            String::new(),
            None,
            true,
        ),
        AdminGate::Password(auth) => {
            if gate.session_user(&headers).is_some() {
                return Redirect::to(&next).into_response();
            }
            page(
                &state,
                locale,
                StatusCode::OK,
                next,
                auth.user().to_string(),
                None,
                false,
            )
        }
    }
}

/// `POST /hts/login` (form: `username`, `password`, `next`).
async fn login_submit(
    State(state): State<Arc<HtsUiState>>,
    Extension(gate): Extension<AdminGate>,
    locale: RequestLocale,
    connect: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
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
    match gate.login(&headers, peer, user.trim(), &password) {
        LoginOutcome::Success { set_cookie } => {
            let mut response = Redirect::to(&next).into_response();
            response
                .headers_mut()
                .append(header::SET_COOKIE, set_cookie);
            response
        }
        LoginOutcome::Invalid => page(
            &state,
            locale,
            StatusCode::UNAUTHORIZED,
            next,
            user,
            Some(i18n.t("admin-login-invalid")),
            false,
        ),
        LoginOutcome::RateLimited(left) => {
            let minutes = left.as_secs().div_ceil(60).max(1);
            let mut response = page(
                &state,
                locale,
                StatusCode::TOO_MANY_REQUESTS,
                next,
                user,
                Some(i18n.t_arg("admin-login-throttled", "minutes", minutes)),
                false,
            );
            if let Ok(v) = left.as_secs().max(1).to_string().parse() {
                response.headers_mut().insert(header::RETRY_AFTER, v);
            }
            response
        }
        LoginOutcome::CrossOrigin => page(
            &state,
            locale,
            StatusCode::FORBIDDEN,
            next,
            user,
            Some(i18n.t("admin-login-cross-site")),
            false,
        ),
        LoginOutcome::NotConfigured => match gate {
            AdminGate::Open => Redirect::to(&next).into_response(),
            _ => page(
                &state,
                locale,
                StatusCode::SERVICE_UNAVAILABLE,
                next,
                String::new(),
                None,
                true,
            ),
        },
    }
}

/// `POST /hts/logout`.
async fn logout(Extension(gate): Extension<AdminGate>, headers: HeaderMap) -> Response {
    let Some(auth) = gate.auth() else {
        return Redirect::to("/ui/hts").into_response();
    };
    // Signing someone out cross-site is only a nuisance, but there is no
    // reason to allow it either.
    if !auth.same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "cross-site request refused").into_response();
    }
    let mut response = Redirect::to("/ui/hts").into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, auth.logout(&headers));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn management_pages_and_writes_are_protected() {
        let get = Method::GET;
        let post = Method::POST;
        for path in [
            "/hts/terminology",
            "/hts/terminology/delete",
            "/hts/jobs",
            "/hts/jobs/abc",
            "/hts/import",
            "/hts/import/registry",
        ] {
            assert!(protects(&get, path), "{path}");
        }
        for path in [
            "/hts/terminology/compact",
            "/hts/jobs/x/cancel",
            "/hts/import/job",
            "/hts/whatever",
        ] {
            assert!(protects(&post, path), "{path}");
        }
        for path in [
            "/hts",
            "/hts/code-systems",
            "/hts/value-sets/x",
            "/hts/concepts",
            "/hts/login",
        ] {
            assert!(!protects(&get, path), "{path}");
        }
        for path in [
            "/hts/code-systems/x/lookup",
            "/hts/value-sets/x/expand",
            "/hts/concept-maps/x/translate",
            "/hts/capability-statement/json-expand",
            "/hts/login",
            "/hts/logout",
            "/hts/api/admin/imports",
        ] {
            assert!(!protects(&post, path), "{path}");
        }
    }
}
