//! Interactive browser login for the web UI (issue #1449).
//!
//! Three routes and one middleware:
//!
//! - `GET /ui/login` starts an Authorization Code + PKCE login: it remembers a
//!   pending login (CSRF `state` + PKCE verifier) under a short-lived cookie and
//!   redirects the browser to the IdP.
//! - `GET /ui/callback?code&state` completes it: checks `state`, exchanges the
//!   code, establishes the server-side session, sets the session cookie and
//!   lands on the page the user originally asked for.
//! - `POST /ui/logout` ends the session, clears the cookie and, when the IdP
//!   has an end-session endpoint, sends the browser there so the IdP session
//!   ends too.
//! - [`require_session`] runs on every UI page: a request with a valid session
//!   gets its [`helios_auth::Principal`] stamped (so per-user settings key on
//!   the signed-in identity); one without is sent to `/ui/login`.
//!
//! The tokens never reach the browser. The FHIR API authenticates the pages'
//! own browser-originated calls because the REST auth middleware turns the
//! same session cookie into the session's bearer (`AuthMiddlewareState::sessions`).
//!
//! The runtime is **process-wide**, installed once at startup with
//! [`set_interactive_login`] — the same shape as the dashboard refresh
//! cadences — because the session store is shared with the REST auth layer,
//! which is built before the UI is mounted. When nothing is installed the UI
//! behaves exactly as before: no login, no gate.

use std::sync::{Arc, RwLock};

use std::net::SocketAddr;

use askama::Template;
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use chrono::Utc;
use helios_admin_auth::console::guard::{self as console_guard, GateOutcome};
use helios_admin_auth::console::{ConsoleAuth, LoginResult, OidcOutcome, Resolved};
use helios_auth::{AccessOutcome, PENDING_COOKIE, SESSION_COOKIE, SessionStore};
use helios_endpoints::console::{CONSOLE_ISSUER, ConsoleAuthMode, ConsolePrincipal};
use serde::Deserialize;

use crate::csrf::CsrfField;
use crate::{I18n, RequestLocale, WebState};

/// What the UI needs to drive the login flow.
#[derive(Clone)]
pub struct LoginRuntime {
    /// The session store shared with the REST auth middleware.
    pub sessions: Arc<SessionStore>,
}

impl std::fmt::Debug for LoginRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginRuntime")
            .field("sessions", &self.sessions)
            .finish()
    }
}

static INTERACTIVE_LOGIN: RwLock<Option<LoginRuntime>> = RwLock::new(None);

/// Install (or replace) the process-wide interactive login. Called once from
/// the server's startup, after auth is initialized and before the UI is
/// mounted; the mount reads it into `WebState`.
pub fn set_interactive_login(runtime: LoginRuntime) {
    match INTERACTIVE_LOGIN.write() {
        Ok(mut guard) => *guard = Some(runtime),
        Err(poisoned) => *poisoned.into_inner() = Some(runtime),
    }
}

/// The installed runtime, if any.
pub(crate) fn installed() -> Option<Arc<LoginRuntime>> {
    INTERACTIVE_LOGIN
        .read()
        .ok()
        .and_then(|guard| guard.clone())
        .map(Arc::new)
}

/// Paths under `/ui` that must stay reachable without a session: the login
/// flow itself and the static assets the login-adjacent pages need.
fn is_open_path(path: &str) -> bool {
    matches!(path, "/ui/login" | "/ui/callback" | "/ui/logout") || path.starts_with("/ui/assets/")
}

/// Middleware: with a login installed, a UI page request must carry a valid
/// session. One that does gets the signed-in [`helios_auth::Principal`] in its
/// extensions; one that does not is redirected to `/ui/login` with the
/// requested path as `next`. Without a login installed this is a no-op.
pub(crate) async fn require_session(
    State(state): State<WebState>,
    mut request: Request,
    next: Next,
) -> Response {
    // Console sign-in (P3) installed: it decides every `/ui` request.
    if let Some(console) = request.extensions().get::<Arc<ConsoleAuth>>().cloned() {
        return console_gate(console, request, next).await;
    }
    let Some(login) = state.login.as_ref() else {
        return next.run(request).await;
    };
    let path = request.uri().path().to_string();
    if !path.starts_with("/ui") || is_open_path(&path) {
        return next.run(request).await;
    }

    // Resolve the session the way the FHIR layer will a moment later: through
    // `access_token`, which refreshes a token about to lapse and drops a
    // session whose refresh is dead. Gating on a bare lookup let a page render
    // once with a session the API had just discarded — one stale page showing
    // a raw 401 before the next navigation redirected — so the gate now settles
    // the session's fate before anything renders.
    let session = match helios_auth::cookie_value(request.headers(), SESSION_COOKIE) {
        Some(id) => match login.sessions.access_token(&id).await {
            AccessOutcome::Token(_) => login.sessions.get(&id),
            AccessOutcome::NoSession => None,
        },
        None => None,
    };
    match session {
        Some(session) => {
            request.extensions_mut().insert(session_principal(&session));
            request.extensions_mut().insert(SignedIn(session.principal));
            next.run(request).await
        }
        None => {
            let next_path = request
                .uri()
                .path_and_query()
                .map(|pq| pq.as_str().to_string())
                .unwrap_or_else(|| "/ui".to_string());
            let target = format!(
                "/ui/login?{}",
                form_urlencoded::Serializer::new(String::new())
                    .append_pair("next", &next_path)
                    .finish()
            );
            // htmx fragment requests cannot follow a redirect into a full-page
            // login; tell htmx to navigate the whole window there instead.
            if request.headers().contains_key("hx-request") {
                return (
                    StatusCode::OK,
                    [(axum::http::HeaderName::from_static("hx-redirect"), target)],
                )
                    .into_response();
            }
            Redirect::to(&target).into_response()
        }
    }
}

/// The signed-in user, stamped on the request by [`require_session`] for
/// templates and handlers that show identity (the user menu, #738).
#[derive(Debug, Clone)]
pub struct SignedIn(pub helios_auth::SessionPrincipal);

fn session_principal(session: &helios_auth::Session) -> helios_auth::Principal {
    helios_auth::Principal {
        subject: session.principal.subject.clone(),
        issuer: session.principal.issuer.clone(),
        tenant_id: None,
        scopes: helios_auth::ScopeSet::empty(),
        jti: None,
        // The UI never authorizes by scope or expiry itself — the FHIR layer
        // does, on the bearer the session stands for — so this is only the
        // identity that keys per-user settings.
        expires_at: Utc::now() + chrono::Duration::hours(1),
        custom_claims: serde_json::Map::new(),
        ..Default::default()
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct LoginQuery {
    #[serde(default)]
    next: Option<String>,
    /// Console sign-in: show the password form (break-glass) in any mode.
    #[serde(default)]
    local: Option<String>,
    /// Console sign-in: just signed out.
    #[serde(default)]
    signed_out: Option<String>,
}

/// `GET /ui/login` — with console sign-in, the sign-in page; otherwise
/// start the #1449 login and send the browser to the IdP.
pub(crate) async fn login(
    State(state): State<WebState>,
    locale: RequestLocale,
    console: Option<Extension<Arc<ConsoleAuth>>>,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Response {
    if let Some(Extension(console)) = console {
        return console_login_page(&console, locale, &headers, query).await;
    }
    let Some(login) = state.login.as_ref() else {
        return not_configured();
    };
    let next = query.next.as_deref().unwrap_or("/ui");
    let (pending_id, authorize_url) = login.sessions.begin(next).await;
    tracing::info!(next = %next, "web login started; redirecting to the identity provider");
    let secure = login.sessions.config().cookie_secure;
    let mut response = Redirect::to(&authorize_url).into_response();
    append_cookie(
        &mut response,
        &cookie(PENDING_COOKIE, &pending_id, Some(600), secure),
    );
    response
}

#[derive(Debug, Deserialize)]
pub(crate) struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// `GET /ui/callback` — the IdP sends the browser back here with the code.
pub(crate) async fn callback(
    State(state): State<WebState>,
    Query(query): Query<CallbackQuery>,
    request: Request,
) -> Response {
    if let Some(console) = request.extensions().get::<Arc<ConsoleAuth>>().cloned() {
        return console_callback(&console, query, request).await;
    }
    let Some(login) = state.login.as_ref() else {
        return not_configured();
    };
    let secure = login.sessions.config().cookie_secure;

    if let Some(error) = query.error {
        let detail = query.error_description.unwrap_or_default();
        return login_failed(
            StatusCode::BAD_REQUEST,
            &format!("the identity provider refused the login: {error} {detail}"),
            secure,
        );
    }
    let (Some(code), Some(state_param)) = (query.code, query.state) else {
        return login_failed(
            StatusCode::BAD_REQUEST,
            "the callback carried no code or state",
            secure,
        );
    };
    let Some(pending_id) = helios_auth::cookie_value(request.headers(), PENDING_COOKIE) else {
        return login_failed(
            StatusCode::BAD_REQUEST,
            "no login is pending in this browser — start again from /ui/login",
            secure,
        );
    };

    match login
        .sessions
        .complete(&pending_id, &state_param, &code)
        .await
    {
        Ok((session, next)) => {
            tracing::info!(
                subject = %session.principal.subject,
                user = %session.principal.display(),
                "web login completed"
            );
            let mut response = Redirect::to(&next).into_response();
            append_cookie(
                &mut response,
                &cookie(SESSION_COOKIE, &session.id, None, secure),
            );
            append_cookie(&mut response, &cookie(PENDING_COOKIE, "", Some(0), secure));
            response
        }
        Err(err) => login_failed(StatusCode::BAD_REQUEST, &err.to_string(), secure),
    }
}

/// `POST /ui/logout` — end the session; then the IdP's session too, when it
/// offers RP-initiated logout.
pub(crate) async fn logout(State(state): State<WebState>, request: Request) -> Response {
    if let Some(console) = request.extensions().get::<Arc<ConsoleAuth>>().cloned() {
        return console_logout(&console, request.headers()).await;
    }
    let Some(login) = state.login.as_ref() else {
        return not_configured();
    };
    let secure = login.sessions.config().cookie_secure;
    let session_id = helios_auth::cookie_value(request.headers(), SESSION_COOKIE);
    let end_session = match session_id.as_deref() {
        Some(id) => login.sessions.logout(id).await,
        None => None,
    };

    let post_logout = format!("{}/ui", state.public_base_url.trim_end_matches('/'));
    let end_session_requested = end_session.is_some();
    let target = match end_session {
        Some((endpoint, id_token_hint)) => {
            let mut query = form_urlencoded::Serializer::new(String::new());
            query.append_pair("post_logout_redirect_uri", &post_logout);
            query.append_pair("client_id", &login.sessions.config().client_id);
            if let Some(hint) = id_token_hint {
                query.append_pair("id_token_hint", &hint);
            }
            let separator = if endpoint.contains('?') { '&' } else { '?' };
            format!("{endpoint}{separator}{}", query.finish())
        }
        None => "/ui/login".to_string(),
    };
    tracing::info!(
        idp_logout = end_session_requested,
        "web logout; session cleared"
    );
    let mut response = Redirect::to(&target).into_response();
    append_cookie(&mut response, &cookie(SESSION_COOKIE, "", Some(0), secure));
    response
}

fn not_configured() -> Response {
    (
        StatusCode::NOT_FOUND,
        "interactive login is not configured on this server",
    )
        .into_response()
}

fn login_failed(status: StatusCode, detail: &str, secure: bool) -> Response {
    let mut response = (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!("Login failed: {detail}"),
    )
        .into_response();
    append_cookie(&mut response, &cookie(PENDING_COOKIE, "", Some(0), secure));
    response
}

/// A `Set-Cookie` value. `max_age` of `Some(0)` deletes the cookie; `None`
/// makes it a session cookie (cleared when the browser closes).
fn cookie(name: &str, value: &str, max_age: Option<u64>, secure: bool) -> String {
    let mut parts = vec![
        format!("{name}={value}"),
        "Path=/".to_string(),
        "HttpOnly".to_string(),
        "SameSite=Lax".to_string(),
    ];
    if secure {
        parts.push("Secure".to_string());
    }
    if let Some(max_age) = max_age {
        parts.push(format!("Max-Age={max_age}"));
    }
    parts.join("; ")
}

fn append_cookie(response: &mut Response, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

// ===========================================================================
// Console sign-in (FHIR server manager package P3, design §7)
//
// Installed when the server puts `Arc<ConsoleAuth>` in the request
// extensions (`crates/hfs/src/manager/console_auth.rs`). Then:
// * [`require_session`] runs the console gate on every `/ui` request;
// * `GET /ui/login` is the sign-in page (OIDC button, password form, or
//   both via `?local=1` for the break-glass administrator);
// * `POST /ui/login/local` signs in with a password, `GET /ui/login/oidc`
//   starts Authorization Code + PKCE, `GET /ui/callback` completes it;
// * `POST /ui/logout` ends the session (and the IdP session).
// Without the extension every handler here behaves as before.
// ===========================================================================

/// The console's own routes (sign-in, account, settings); merged into the
/// UI router by `lib.rs`. Each answers `404` without console sign-in.
pub(crate) fn console_routes() -> Router<WebState> {
    Router::new()
        .route("/ui/login/local", post(login_local_submit))
        .route("/ui/login/oidc", get(login_oidc))
        .route("/ui/console/denied", get(denied))
        .route("/ui/account", get(crate::account::page))
        .route(
            "/ui/account/password",
            get(crate::account::page).post(crate::account::change_password),
        )
        .route(
            "/ui/settings/auth",
            get(crate::console_settings::page).post(crate::console_settings::save),
        )
        .route(
            "/ui/settings/auth/test",
            post(crate::console_settings::test_sign_in),
        )
        .route(
            "/ui/settings/users",
            get(crate::console_users::page).post(crate::console_users::create),
        )
        .route(
            "/ui/settings/users/{id}",
            post(crate::console_users::update),
        )
        .route(
            "/ui/settings/users/{id}/reset-password",
            post(crate::console_users::reset_password),
        )
        .route(
            "/ui/settings/users/{id}/delete",
            post(crate::console_users::delete),
        )
        .route(
            "/ui/settings/sessions/{key}/revoke",
            post(crate::console_users::revoke_session),
        )
        .route("/ui/settings/bindings", post(crate::console_users::bind))
        .route(
            "/ui/settings/bindings/delete",
            post(crate::console_users::unbind),
        )
}

/// `404` for a console route on a server without console sign-in.
pub(crate) fn console_not_installed() -> Response {
    (
        StatusCode::NOT_FOUND,
        "console sign-in is not enabled on this server",
    )
        .into_response()
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn redirect_with(location: &str, cookies: Vec<HeaderValue>) -> Response {
    let mut response = Redirect::to(location).into_response();
    for c in cookies {
        response.headers_mut().append(header::SET_COOKIE, c);
    }
    no_store(response)
}

/// The identity the rest of the UI sees for a console session: the
/// account menu (`SignedIn`), per-user settings (`Principal`), and the
/// direct-storage handlers' "is someone signed in" check (`SessionPrincipal`).
fn stamp_session(request: &mut Request, resolved: &Resolved) {
    let p = &resolved.principal;
    let session_principal = helios_auth::SessionPrincipal {
        subject: p.subject.clone(),
        issuer: CONSOLE_ISSUER.to_string(),
        name: Some(p.display_name.clone()),
        preferred_username: resolved
            .session
            .subject
            .strip_prefix("local:")
            .map(str::to_string),
        email: None,
        picture: None,
    };
    request.extensions_mut().insert(helios_auth::Principal {
        subject: p.subject.clone(),
        issuer: CONSOLE_ISSUER.to_string(),
        tenant_id: None,
        scopes: helios_auth::ScopeSet::empty(),
        jti: None,
        expires_at: Utc::now() + chrono::Duration::hours(1),
        custom_claims: serde_json::Map::new(),
        ..Default::default()
    });
    request.extensions_mut().insert(session_principal.clone());
    request.extensions_mut().insert(SignedIn(session_principal));
}

/// The console gate as the UI's outermost layer. Refusals that need a page
/// (role too low, CSRF) render the localized `GET /ui/console/denied` page
/// (a route matched before this layer cannot be re-routed, so the page is
/// served by a small router of its own behind the locale negotiation).
async fn console_gate(console: Arc<ConsoleAuth>, request: Request, next: Next) -> Response {
    let headers = request.headers().clone();
    let htmx = headers.contains_key("hx-request");
    match console_guard::gate(&console, request).await {
        GateOutcome::Pass(request, info) => {
            let mut request = *request;
            if let Some(resolved) = &info.resolved {
                stamp_session(&mut request, resolved);
            }
            next.run(request).await
        }
        GateOutcome::Forbidden { principal, need } => {
            let query = format!("kind=role&need={}", need.as_str());
            let resolved = console.resolve(&headers).await;
            denied_dispatch(&console, headers, &query, Some(principal), resolved, htmx).await
        }
        GateOutcome::Csrf(_) => {
            let resolved = console.resolve(&headers).await;
            let principal = resolved.as_ref().map(|r| r.principal.clone());
            denied_dispatch(&console, headers, "kind=csrf", principal, resolved, htmx).await
        }
        other => other.into_plain_response(),
    }
}

async fn denied_dispatch(
    console: &Arc<ConsoleAuth>,
    headers: HeaderMap,
    query: &str,
    principal: Option<ConsolePrincipal>,
    resolved: Option<Resolved>,
    htmx: bool,
) -> Response {
    if htmx {
        // A fragment request cannot show a page; tell htmx to go there.
        let mut r = StatusCode::FORBIDDEN.into_response();
        if let Ok(v) = HeaderValue::from_str(&format!("/ui/console/denied?{query}")) {
            r.headers_mut().insert("hx-redirect", v);
        }
        return no_store(r);
    }
    let mut request = Request::builder()
        .method(axum::http::Method::GET)
        .uri(format!("/ui/console/denied?{query}"))
        .body(axum::body::Body::empty())
        .expect("static request parts");
    let keep = [
        header::HOST,
        header::COOKIE,
        header::ACCEPT_LANGUAGE,
        header::USER_AGENT,
    ];
    for name in keep {
        if let Some(v) = headers.get(&name) {
            request.headers_mut().insert(name, v.clone());
        }
    }
    request.extensions_mut().insert(console.clone());
    if let Some(p) = principal {
        request.extensions_mut().insert(p);
    }
    if let Some(r) = resolved {
        request
            .extensions_mut()
            .insert(console_guard::CsrfToken(r.session.csrf.clone()));
        stamp_session(&mut request, &r);
        request.extensions_mut().insert(r);
    }
    let page = Router::new()
        .route("/ui/console/denied", get(denied))
        .layer(axum::middleware::from_fn(crate::i18n::negotiate_locale));
    let mut response = match tower::ServiceExt::oneshot(page, request).await {
        Ok(r) => r,
        Err(never) => match never {},
    };
    *response.status_mut() = StatusCode::FORBIDDEN;
    no_store(response)
}

/// Sign-in, "not allowed" and failure messages.
#[derive(Template)]
#[template(path = "pages/console-message.html")]
pub(crate) struct ConsoleMessagePage {
    pub(crate) i18n: I18n,
    pub(crate) title: String,
    pub(crate) text: String,
    pub(crate) back_href: Option<String>,
    pub(crate) show_sign_out: bool,
    pub(crate) csrf_html: String,
    pub(crate) end_session_href: Option<String>,
}

pub(crate) fn message_page(status: StatusCode, page: ConsoleMessagePage) -> Response {
    let mut response = crate::render(page);
    if response.status() == StatusCode::OK {
        *response.status_mut() = status;
    }
    no_store(response)
}

#[derive(Debug, Deserialize)]
pub(crate) struct DeniedQuery {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    need: Option<String>,
}

/// `GET /ui/console/denied` — what the gate's refusals render.
async fn denied(
    locale: RequestLocale,
    csrf: CsrfField,
    resolved: Option<Extension<Resolved>>,
    Query(q): Query<DeniedQuery>,
) -> Response {
    let i18n = I18n::new(locale);
    let (title, text) = match q.kind.as_deref() {
        Some("csrf") => (i18n.t("console-csrf-title"), i18n.t("console-csrf-text")),
        _ => (
            i18n.t("console-forbidden-title"),
            i18n.t_arg(
                "console-forbidden-text",
                "role",
                q.need.unwrap_or_else(|| "admin".into()),
            ),
        ),
    };
    message_page(
        StatusCode::FORBIDDEN,
        ConsoleMessagePage {
            i18n,
            title,
            text,
            back_href: Some("/ui".into()),
            show_sign_out: resolved.is_some(),
            csrf_html: csrf.html(),
            end_session_href: None,
        },
    )
}

/// The console sign-in page.
#[derive(Template)]
#[template(path = "pages/login.html")]
struct ConsoleLoginPage {
    i18n: I18n,
    lede: String,
    notice: Option<String>,
    error: Option<String>,
    show_idp: bool,
    idp_href: String,
    show_form: bool,
    next: String,
    user: String,
    show_break_glass_link: bool,
    break_glass_href: String,
    show_idp_link: bool,
    idp_page_href: String,
    lang_suffix: String,
}

fn query_pair(key: &str, value: &str) -> String {
    form_urlencoded::Serializer::new(String::new())
        .append_pair(key, value)
        .finish()
}

struct LoginView {
    local: bool,
    next: String,
    user: String,
    notice: Option<String>,
    error: Option<String>,
}

/// Renders the sign-in page for the current mode, or `None` when there is
/// nothing to sign in to (mode none with an anonymous role, no `?local=1`).
fn login_view(console: &ConsoleAuth, i18n: I18n, v: LoginView) -> Option<ConsoleLoginPage> {
    let mode = console.mode();
    let bg = console.break_glass_available();
    let (show_idp, show_form, lede) = match mode {
        ConsoleAuthMode::Oidc if !v.local => (true, false, "console-login-lede-oidc"),
        ConsoleAuthMode::Local => (false, true, "console-login-lede-local"),
        ConsoleAuthMode::None if !v.local && console.anonymous().is_some() => return None,
        _ => (false, true, "console-login-lede-break-glass"),
    };
    let next_q = query_pair("next", &v.next);
    let mut error = v.error;
    if show_form && !bg && mode != ConsoleAuthMode::Local && error.is_none() {
        error = Some(i18n.t_arg("admin-login-locked", "var", "HFS_ADMIN_PASSWORD"));
    }
    Some(ConsoleLoginPage {
        lede: i18n.t(lede),
        notice: v.notice,
        error,
        show_idp,
        idp_href: format!("/ui/login/oidc?{next_q}"),
        show_form: show_form && (bg || mode == ConsoleAuthMode::Local),
        next: v.next,
        user: v.user,
        show_break_glass_link: show_idp && bg,
        break_glass_href: format!("/ui/login?local=1&{next_q}"),
        show_idp_link: mode == ConsoleAuthMode::Oidc && v.local,
        idp_page_href: format!("/ui/login?{next_q}"),
        lang_suffix: format!("&{next_q}{}", if v.local { "&local=1" } else { "" }),
        i18n,
    })
}

async fn console_login_page(
    console: &ConsoleAuth,
    locale: RequestLocale,
    headers: &HeaderMap,
    query: LoginQuery,
) -> Response {
    console.refresh_if_stale().await;
    let next = helios_admin_auth::safe_next(query.next.as_deref(), "/ui", "/ui");
    let signed_out = query.signed_out.is_some();
    if !signed_out && console.resolve(headers).await.is_some() {
        return no_store(Redirect::to(&next).into_response());
    }
    let i18n = I18n::new(locale);
    let view = LoginView {
        local: query.local.is_some(),
        next: next.clone(),
        user: String::new(),
        notice: signed_out.then(|| i18n.t("console-login-signed-out")),
        error: None,
    };
    match login_view(console, i18n, view) {
        Some(page) => no_store(crate::render(page)),
        None => no_store(Redirect::to(&next).into_response()),
    }
}

/// `POST /ui/login/local` (form: `username`, `password`, `next`).
pub(crate) async fn login_local_submit(
    locale: RequestLocale,
    console: Option<Extension<Arc<ConsoleAuth>>>,
    connect: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Some(Extension(console)) = console else {
        return console_not_installed();
    };
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
    let next = helios_admin_auth::safe_next(next.as_deref(), "/ui", "/ui");
    let peer = connect.map(|c| c.0.0.ip());
    let i18n = I18n::new(locale);
    let failed = |status: StatusCode, error: String| {
        let view = LoginView {
            local: true,
            next: next.clone(),
            user: user.clone(),
            notice: None,
            error: Some(error),
        };
        let page = login_view(&console, i18n, view)
            .map(crate::render)
            .unwrap_or_else(|| status.into_response());
        let mut response = no_store(page);
        *response.status_mut() = status;
        response
    };
    match console
        .login_password(&headers, peer, user.trim(), &password)
        .await
    {
        LoginResult::Success {
            cookies,
            must_change_password,
        } => {
            let target = if must_change_password {
                console_guard::CHANGE_PASSWORD_PATH.to_string()
            } else {
                next.clone()
            };
            redirect_with(&target, cookies)
        }
        LoginResult::Invalid => failed(StatusCode::UNAUTHORIZED, i18n.t("admin-login-invalid")),
        LoginResult::RateLimited(left) => {
            let minutes = left.as_secs().div_ceil(60).max(1);
            let mut response = failed(
                StatusCode::TOO_MANY_REQUESTS,
                i18n.t_arg("admin-login-throttled", "minutes", minutes),
            );
            if let Ok(v) = HeaderValue::from_str(&left.as_secs().max(1).to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, v);
            }
            response
        }
        LoginResult::CrossOrigin => failed(StatusCode::FORBIDDEN, i18n.t("admin-login-cross-site")),
        LoginResult::Unavailable(detail) => failed(
            StatusCode::SERVICE_UNAVAILABLE,
            i18n.t_arg("console-error-unavailable", "detail", detail),
        ),
    }
}

/// `GET /ui/login/oidc?next=` — start an OIDC sign-in.
async fn login_oidc(
    locale: RequestLocale,
    console: Option<Extension<Arc<ConsoleAuth>>>,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Response {
    let Some(Extension(console)) = console else {
        return console_not_installed();
    };
    let next = helios_admin_auth::safe_next(query.next.as_deref(), "/ui", "/ui");
    match console.begin_oidc(&headers, &next, None).await {
        Ok((cookie, url)) => redirect_with(&url, vec![cookie]),
        Err(reason) => {
            tracing::warn!(error = %reason, "console OIDC sign-in could not start");
            let i18n = I18n::new(locale);
            let view = LoginView {
                local: true,
                next,
                user: String::new(),
                notice: None,
                error: Some(i18n.t_arg("console-login-no-oidc", "reason", reason)),
            };
            match login_view(&console, i18n, view) {
                Some(page) => {
                    let mut r = no_store(crate::render(page));
                    *r.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
                    r
                }
                None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            }
        }
    }
}

async fn console_callback(
    console: &ConsoleAuth,
    query: CallbackQuery,
    request: Request,
) -> Response {
    let locale = request
        .extensions()
        .get::<RequestLocale>()
        .copied()
        .unwrap_or_default();
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let i18n = I18n::new(locale);
    let outcome = console
        .complete_oidc(
            request.headers(),
            peer,
            query.code.as_deref(),
            query.state.as_deref(),
            query.error.as_deref(),
        )
        .await;
    match outcome {
        OidcOutcome::Success { cookies, next } => redirect_with(&next, cookies),
        OidcOutcome::Tested {
            role,
            display,
            cookies,
        } => {
            let q = form_urlencoded::Serializer::new(String::new())
                .append_pair("tested", if role.is_some() { "ok" } else { "norole" })
                .append_pair("user", &display)
                .append_pair("role", role.map(|r| r.as_str()).unwrap_or(""))
                .finish();
            redirect_with(&format!("/ui/settings/auth?{q}"), cookies)
        }
        OidcOutcome::NoAccess {
            display,
            cookies,
            end_session,
        } => {
            let mut r = message_page(
                StatusCode::FORBIDDEN,
                ConsoleMessagePage {
                    title: i18n.t("console-noaccess-title"),
                    text: i18n.t_arg("console-noaccess-text", "user", display),
                    back_href: None,
                    show_sign_out: false,
                    csrf_html: String::new(),
                    end_session_href: end_session,
                    i18n,
                },
            );
            for c in cookies {
                r.headers_mut().append(header::SET_COOKIE, c);
            }
            r
        }
        OidcOutcome::Failed(detail) => {
            tracing::warn!(error = %detail, "console OIDC sign-in failed");
            message_page(
                StatusCode::BAD_REQUEST,
                ConsoleMessagePage {
                    title: i18n.t("console-login-failed-title"),
                    text: detail,
                    back_href: Some("/ui/login".into()),
                    show_sign_out: false,
                    csrf_html: String::new(),
                    end_session_href: None,
                    i18n,
                },
            )
        }
    }
}

/// Console sign-out (same-site posts only).
pub(crate) async fn console_logout(console: &ConsoleAuth, headers: &HeaderMap) -> Response {
    if !console.same_origin(headers) {
        return (StatusCode::FORBIDDEN, "cross-site request refused").into_response();
    }
    let out = console.logout(headers).await;
    redirect_with(&out.location, out.cookies)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_paths_bypass_the_session_gate() {
        assert!(is_open_path("/ui/login"));
        assert!(is_open_path("/ui/callback"));
        assert!(is_open_path("/ui/logout"));
        assert!(is_open_path("/ui/assets/app.css"));
        assert!(!is_open_path("/ui"));
        assert!(!is_open_path("/ui/resources"));
    }

    #[test]
    fn session_cookie_is_httponly_lax_and_secure_by_default() {
        let c = cookie(SESSION_COOKIE, "abc", None, true);
        assert_eq!(c, "hfs_session=abc; Path=/; HttpOnly; SameSite=Lax; Secure");
        let dev = cookie(SESSION_COOKIE, "abc", None, false);
        assert!(!dev.contains("Secure"));
        let cleared = cookie(SESSION_COOKIE, "", Some(0), true);
        assert!(cleared.ends_with("Max-Age=0"));
    }
}

/// The `Authorization` value a self-call made on this request's behalf should
/// carry when the browser sent none of its own: the signed-in user's session
/// bearer (issue #1480). Resolved through the same `access_token` path the
/// REST middleware uses — a token about to lapse is refreshed, a session whose
/// refresh is dead is dropped — so a page's server-side call runs with exactly
/// the credential its browser-side calls run with. `None` when no login is
/// installed or the request carries no valid session; the caller then falls
/// back to the process's outbound service credential as before.
pub(crate) async fn session_authorization(
    state: &WebState,
    headers: &axum::http::HeaderMap,
) -> Option<String> {
    let login = state.login.as_ref()?;
    let id = helios_auth::cookie_value(headers, SESSION_COOKIE)?;
    match login.sessions.access_token(&id).await {
        AccessOutcome::Token(token) => Some(format!("Bearer {token}")),
        AccessOutcome::NoSession => None,
    }
}

/// A [`crate::Caller`] for this request: the browser's own `Authorization`
/// when it sent one, else the signed-in session's bearer (#1480), else none —
/// in which case the conformance source applies the outbound service
/// credential. The `$sql-export` self-calls run as the user who clicked.
pub(crate) async fn caller_for(
    state: &WebState,
    headers: &axum::http::HeaderMap,
    tenant: &str,
) -> crate::Caller {
    let mut caller = crate::Caller::from_request(headers, tenant);
    if caller.authorization.is_none() {
        caller.authorization = session_authorization(state, headers).await;
    }
    caller
}

/// The credential a page's own server-side self-call carries under the
/// interactive login (#1480): the browser's `Authorization` when it sent one,
/// else the signed-in session's bearer, else nothing (the outbound service
/// credential applies downstream).
#[cfg(test)]
mod selfcall_tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use axum::http::{HeaderMap, HeaderValue, header};
    use helios_auth::{LoginConfig, Session, SessionPrincipal, SessionStore};

    use super::*;

    const TOKEN: &str = "access-token-1";

    fn store() -> Arc<SessionStore> {
        let store = Arc::new(SessionStore::new(LoginConfig {
            client_id: "hfs-web".to_string(),
            client_secret: None,
            redirect_uri: "http://localhost:8080/ui/callback".to_string(),
            scopes: "openid".to_string(),
            authorization_endpoint: "https://idp.example.com/auth".to_string(),
            token_endpoint: "https://idp.example.com/token".to_string(),
            end_session_endpoint: None,
            cookie_secure: true,
        }));
        store.insert(Session {
            id: "sess-1".to_string(),
            principal: SessionPrincipal {
                subject: "demo-sub".to_string(),
                issuer: "https://idp.example.com".to_string(),
                name: Some("Demo User".to_string()),
                preferred_username: None,
                email: None,
                picture: None,
            },
            access_token: TOKEN.to_string(),
            access_expires_at: Instant::now() + Duration::from_secs(300),
            refresh_token: None,
            id_token: None,
            last_seen: Instant::now(),
            created_at: Utc::now(),
        });
        store
    }

    /// A real `WebState`, with or without a login installed.
    fn web_state(sessions: Option<Arc<SessionStore>>) -> WebState {
        let source: Arc<dyn crate::ConformanceSource> = Arc::new(
            crate::StaticConformanceSource::from_data_dir(std::path::Path::new("../../data")),
        );
        WebState {
            version: "9.9.9",
            sp_catalog: Arc::new(crate::search_params::SpCatalog::new(source.clone())),
            nl: Arc::new(crate::NlSearch::default()),
            compartments: Arc::new(crate::compartments::CompartmentCatalog::new(source.clone())),
            conformance: source,
            tenants: None,
            tenant_inventory: None,
            provisioning: Default::default(),
            data_dir: None,
            public_base_url: "http://localhost:8080".to_string(),
            self_base_url: "http://localhost:8080".to_string(),
            outbound_auth: Arc::new(helios_auth::outbound::NoOpOutboundAuthProvider),
            tenant_path_routing: false,
            fhir_version: helios_fhir::FhirVersion::R4,
            default_tenant: "default".to_string(),
            terminology: None,
            terminology_public_url: None,
            settings: None,
            bulk_provider: None,
            write_observer: None,
            patient_name_search: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            login: sessions.map(|sessions| Arc::new(LoginRuntime { sessions })),
            admin_gate: helios_admin_auth::AdminGate::Open,
            manager: None,
        }
    }

    fn cookie(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[tokio::test]
    async fn session_authorization_is_the_session_bearer_only_for_a_valid_cookie() {
        let state = web_state(Some(store()));
        assert_eq!(
            session_authorization(&state, &cookie("hfs_lang=es; hfs_session=sess-1")).await,
            Some(format!("Bearer {TOKEN}"))
        );
        assert_eq!(session_authorization(&state, &HeaderMap::new()).await, None);
        assert_eq!(
            session_authorization(&state, &cookie("hfs_session=nope")).await,
            None
        );
        // No login installed: the cookie means nothing.
        let no_login = web_state(None);
        assert_eq!(
            session_authorization(&no_login, &cookie("hfs_session=sess-1")).await,
            None
        );
    }

    #[tokio::test]
    async fn caller_for_prefers_the_browser_header_then_the_session_then_nothing() {
        let state = web_state(Some(store()));

        let mut both = cookie("hfs_session=sess-1");
        both.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer browser-token"),
        );
        let caller = caller_for(&state, &both, "clinic-a").await;
        assert_eq!(
            caller.authorization.as_deref(),
            Some("Bearer browser-token")
        );
        assert_eq!(caller.tenant, "clinic-a");

        let caller = caller_for(&state, &cookie("hfs_session=sess-1"), "clinic-a").await;
        assert_eq!(
            caller.authorization.as_deref(),
            Some(format!("Bearer {TOKEN}").as_str())
        );

        let caller = caller_for(&state, &HeaderMap::new(), "clinic-a").await;
        assert_eq!(caller.authorization, None);
    }

    async fn built_headers(state: &WebState, headers: &HeaderMap) -> reqwest::header::HeaderMap {
        let request = reqwest::Client::new().get("http://127.0.0.1:1/Patient/$export");
        crate::bulk_export::forward_identity(state, request, headers, "clinic-a", "aud")
            .await
            .expect("credential resolved")
            .build()
            .expect("request builds")
            .headers()
            .clone()
    }

    #[tokio::test]
    async fn forward_identity_runs_the_self_call_as_the_signed_in_user() {
        let state = web_state(Some(store()));

        // Session cookie, no browser header: the session's bearer goes on.
        let sent = built_headers(&state, &cookie("hfs_session=sess-1")).await;
        assert_eq!(
            sent.get("authorization").and_then(|v| v.to_str().ok()),
            Some(format!("Bearer {TOKEN}").as_str())
        );
        assert_eq!(
            sent.get("x-tenant-id").and_then(|v| v.to_str().ok()),
            Some("clinic-a")
        );

        // The browser's own header still wins, verbatim.
        let mut both = cookie("hfs_session=sess-1");
        both.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer browser-token"),
        );
        let sent = built_headers(&state, &both).await;
        assert_eq!(
            sent.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer browser-token")
        );

        // No session: the outbound provider (a no-op here) is all there is.
        let sent = built_headers(&state, &HeaderMap::new()).await;
        assert!(sent.get("authorization").is_none());
    }

    async fn import_headers(
        state: &WebState,
        submission: &crate::bulk_import::Submission,
        headers: &HeaderMap,
    ) -> reqwest::header::HeaderMap {
        let request = reqwest::Client::new().post("http://localhost:8080/$bulk-submit");
        crate::bulk_import::authorize_self_call(state, submission, headers, request, "aud")
            .await
            .expect("credential resolved")
            .build()
            .expect("request builds")
            .headers()
            .clone()
    }

    #[tokio::test]
    async fn the_import_page_submits_to_this_server_as_the_signed_in_user_too() {
        let state = web_state(Some(store()));
        let to_self = crate::bulk_import::Submission {
            auth: "none".to_string(),
            recipient_base_url: "http://localhost:8080".to_string(),
            ..Default::default()
        };

        // Same order as every other page: the session's bearer goes on.
        let sent = import_headers(&state, &to_self, &cookie("hfs_session=sess-1")).await;
        assert_eq!(
            sent.get("authorization").and_then(|v| v.to_str().ok()),
            Some(format!("Bearer {TOKEN}").as_str())
        );

        // The browser's own header still wins.
        let mut both = cookie("hfs_session=sess-1");
        both.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer browser-token"),
        );
        let sent = import_headers(&state, &to_self, &both).await;
        assert_eq!(
            sent.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer browser-token")
        );

        // Another server never sees this server's user or service token.
        let elsewhere = crate::bulk_import::Submission {
            auth: "none".to_string(),
            recipient_base_url: "https://recipient.example".to_string(),
            ..Default::default()
        };
        let sent = import_headers(&state, &elsewhere, &cookie("hfs_session=sess-1")).await;
        assert!(sent.get("authorization").is_none());
    }
}

/// Console sign-in through the real UI router (`crate::mount`): sign-in
/// pages per mode, the role gate over **every** `/ui` route of `lib.rs` in
/// each mode for anonymous/viewer/editor/admin, CSRF on the settings forms,
/// account/settings pages, and the en/zh-Hant catalogs.
#[cfg(test)]
mod console_tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use helios_admin_auth::console::access::{Requirement, requirement};
    use helios_admin_auth::console::testing::{env, runtime, session};
    use helios_admin_auth::console::{ConsoleAuth, ConsoleOptions, NewUser};
    use helios_endpoints::console::{ConsoleRole, ConsoleVia};
    use tower::ServiceExt;

    const BG: &str = "break-glass-password-1";
    const ORIGIN: &str = "http://hfs.test";

    fn opts() -> ConsoleOptions {
        ConsoleOptions {
            public_base_url: ORIGIN.into(),
            recheck: std::time::Duration::from_secs(0),
            cookie_secure: helios_admin_auth::CookieSecure::Never,
            ..Default::default()
        }
    }

    fn app(console: Arc<ConsoleAuth>) -> Router {
        crate::mount(
            Router::new(),
            "9.9.9",
            None,
            crate::NlSearch::default(),
            None,
            None,
            "default".into(),
            // Self-calls fail fast instead of resolving a test host.
            "http://127.0.0.1:1".into(),
            Arc::new(helios_auth::outbound::NoOpOutboundAuthProvider),
            helios_fhir::FhirVersion::R4,
            None,
            ORIGIN.into(),
            None,
        )
        .layer(axum::Extension(console))
    }

    struct Reply {
        status: StatusCode,
        location: String,
        cookies: Vec<String>,
        body: String,
    }

    async fn call(
        app: &Router,
        method: Method,
        path: &str,
        cookie: &str,
        csrf: &str,
        form: Option<&str>,
    ) -> Reply {
        let mut b = Request::builder()
            .method(method.clone())
            .uri(path)
            .header(header::HOST, "hfs.test");
        if !cookie.is_empty() {
            b = b.header(header::COOKIE, cookie);
        }
        if method != Method::GET {
            b = b.header(header::ORIGIN, ORIGIN);
            if !csrf.is_empty() {
                b = b.header("x-csrf-token", csrf);
            }
        }
        let req = match form {
            Some(f) => b
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(f.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let r = app.clone().oneshot(req).await.unwrap();
        let status = r.status();
        let location = r
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let cookies = r
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_string)
            .collect();
        let bytes = axum::body::to_bytes(r.into_body(), 8 << 20).await.unwrap();
        Reply {
            status,
            location,
            cookies,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    fn cookie_of(reply: &Reply) -> (String, String) {
        let pairs: Vec<&str> = reply
            .cookies
            .iter()
            .filter_map(|c| c.split(';').next())
            .filter(|kv| !kv.ends_with('='))
            .collect();
        let csrf = pairs
            .iter()
            .find_map(|p| p.strip_prefix("hfs_csrf="))
            .unwrap_or_default()
            .to_string();
        (pairs.join("; "), csrf)
    }

    /// Every concrete `/ui` route registered in `lib.rs` (and the console's
    /// own), path parameters filled with `x`.
    fn all_ui_routes() -> Vec<String> {
        let mut src = String::from(include_str!("lib.rs"));
        src.push_str(include_str!("login.rs"));
        let mut routes: Vec<String> = Vec::new();
        for (i, _) in src.match_indices(".route(") {
            let rest = &src[i + 7..];
            let rest = rest.trim_start();
            let Some(rest) = rest.strip_prefix('"') else {
                continue;
            };
            let Some(end) = rest.find('"') else { continue };
            let path = &rest[..end];
            if !path.starts_with("/ui") {
                continue;
            }
            let filled: String = path
                .split('/')
                .map(|seg| if seg.starts_with('{') { "x" } else { seg })
                .collect::<Vec<_>>()
                .join("/");
            if !routes.contains(&filled) {
                routes.push(filled);
            }
        }
        for p in ["/ui/admin/login", "/ui/admin/logout"] {
            routes.push(p.into());
        }
        routes
    }

    fn signed_in_redirect(r: &Reply) -> bool {
        (r.status == StatusCode::SEE_OTHER && r.location.starts_with("/ui/login"))
            || r.status == StatusCode::UNAUTHORIZED
    }

    fn console_forbidden(r: &Reply) -> bool {
        r.status == StatusCode::FORBIDDEN && r.body.contains("console-message")
    }

    #[test]
    fn route_inventory_is_complete_enough() {
        let routes = all_ui_routes();
        assert!(routes.len() > 60, "{}", routes.len());
        for p in [
            "/ui",
            "/ui/resources",
            "/ui/packages",
            "/ui/settings/users",
            "/ui/login/local",
        ] {
            assert!(routes.iter().any(|r| r == p), "{p}");
        }
    }

    /// The gate matrix: every route × GET (and POST for the writing ones) ×
    /// {anonymous, viewer, editor, admin} × {none, local, oidc}.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_ui_route_is_gated_by_role_in_every_mode() {
        let routes = all_ui_routes();
        for mode in ["none", "local", "oidc"] {
            let mut pairs = vec![("HFS_CONSOLE_AUTH_MODE", mode)];
            if mode == "oidc" {
                // Unreachable on purpose: "Test sign-in" fails fast.
                pairs.push(("HFS_CONSOLE_OIDC_ISSUER", "http://127.0.0.1:1"));
                pairs.push(("HFS_CONSOLE_OIDC_CLIENT_ID", "hfs-web"));
            }
            let (rt, _) = runtime(env(&pairs), Some(BG), opts()).await;
            let app = app(rt.clone());
            let via = match mode {
                "local" => ConsoleVia::Local,
                "oidc" => ConsoleVia::Oidc,
                _ => ConsoleVia::BreakGlass,
            };
            let mut principals: Vec<(&str, Option<ConsoleRole>, String, String)> =
                vec![("anonymous", None, String::new(), String::new())];
            for role in [ConsoleRole::Viewer, ConsoleRole::Editor, ConsoleRole::Admin] {
                // In mode none only the break-glass administrator signs in.
                if mode == "none" && role != ConsoleRole::Admin {
                    continue;
                }
                let user_id = if via == ConsoleVia::Local {
                    let (u, _) = rt
                        .create_user(
                            "t",
                            NewUser {
                                username: format!("u-{}", role.as_str()),
                                role: Some(role),
                                password: Some("tidy-Lantern-41-orbit".into()),
                                ..Default::default()
                            },
                        )
                        .await
                        .unwrap();
                    let mut u2 = u.clone();
                    u2.must_change_password = false;
                    u2.password_hash = None;
                    let u2 = rt.store().update_user(u2, u.revision).await.unwrap();
                    Some(u2.id)
                } else {
                    None
                };
                let subject = format!("{}:{}", mode, role.as_str());
                let (c, t) = session(&rt, &subject, role, via, user_id).await;
                principals.push((role.as_str(), Some(role), c, t));
            }
            let anonymous_role = rt.anonymous().map(|p| p.role);
            for path in &routes {
                for method in [Method::GET, Method::POST] {
                    let Some(req) = requirement(&method, path) else {
                        continue;
                    };
                    // The refusal page itself always answers 403.
                    if req == Requirement::Public || path == "/ui/console/denied" {
                        continue;
                    }
                    // POST only where a write exists or the rule differs.
                    if method == Method::POST
                        && requirement(&Method::GET, path) == Some(req.clone())
                        && !path.contains("settings")
                    {
                        continue;
                    }
                    for (who, role, cookie, csrf) in &principals {
                        let effective = role.or(anonymous_role);
                        let allowed = match &req {
                            Requirement::Public => true,
                            Requirement::SignedIn => role.is_some(),
                            Requirement::Role(r) | Requirement::EndpointRole(_, r) => {
                                effective.is_some_and(|e| e >= *r)
                            }
                        };
                        let r = call(&app, method.clone(), path, cookie, csrf, None).await;
                        let ctx = format!(
                            "mode={mode} who={who} {method} {path} -> {} {}",
                            r.status, r.location
                        );
                        if allowed {
                            assert!(
                                !signed_in_redirect(&r) && !console_forbidden(&r),
                                "allowed but refused: {ctx}"
                            );
                        } else if role.is_none() {
                            assert!(
                                signed_in_redirect(&r),
                                "anonymous not sent to sign-in: {ctx}"
                            );
                        } else {
                            assert!(console_forbidden(&r), "role too low not refused: {ctx}");
                        }
                    }
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn local_sign_in_page_flow_settings_and_logout() {
        let (rt, _) = runtime(env(&[("HFS_CONSOLE_AUTH_MODE", "local")]), Some(BG), opts()).await;
        let app = app(rt.clone());
        // Entry: sign-in required.
        let r = call(&app, Method::GET, "/ui/resources", "", "", None).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER);
        assert_eq!(r.location, "/ui/login?next=/ui/resources");
        let r = call(
            &app,
            Method::GET,
            "/ui/login?next=/ui/resources",
            "",
            "",
            None,
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        assert!(r.body.contains(r#"id="signin-form""#), "{}", r.body);
        assert!(r.body.contains("HealthyMind FHIR Server Manager"));
        assert!(!r.body.contains("signin-idp"));
        let zh = call(&app, Method::GET, "/ui/login?lang=zh-Hant", "", "", None).await;
        assert!(zh.body.contains("療心智能"), "zh-Hant title");

        // Wrong password: 401 and the page again.
        let r = call(
            &app,
            Method::POST,
            "/ui/login/local",
            "",
            "",
            Some("username=admin&password=nope&next=/ui/resources"),
        )
        .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        assert!(r.body.contains(r#"role="alert""#));
        // Break-glass sign-in.
        let r = call(
            &app,
            Method::POST,
            "/ui/login/local",
            "",
            "",
            Some(&format!("username=admin&password={BG}&next=/ui/resources")),
        )
        .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
        assert_eq!(r.location, "/ui/resources");
        let (cookie, csrf) = cookie_of(&r);
        assert!(cookie.contains("hfs_console=") && cookie.contains("hfs_admin="));

        // Users page; create an account through the form (token in the form).
        let r = call(&app, Method::GET, "/ui/settings/users", &cookie, "", None).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        assert!(r.body.contains(r#"name="_csrf""#));
        let r = call(
            &app,
            Method::POST,
            "/ui/settings/users",
            &cookie,
            "",
            Some(&format!("_csrf={csrf}&username=ed&role=editor")),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        assert!(r.body.contains(r#"id="one-time-password""#));
        // Without the token: refused, rendered as the localized message.
        let r = call(
            &app,
            Method::POST,
            "/ui/settings/users",
            &cookie,
            "",
            Some("username=ed2"),
        )
        .await;
        assert!(console_forbidden(&r), "{} {}", r.status, r.body);
        assert!(rt.store().user_by_name("ed2").await.unwrap().is_none());

        // Settings page renders with the env pin on the mode.
        let r = call(&app, Method::GET, "/ui/settings/auth", &cookie, "", None).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        assert!(r.body.contains("HFS_CONSOLE_AUTH_MODE"));
        assert!(r.body.contains(r#"id="settings-auth-form""#));
        // Saving: the pinned mode stays; an unpinned field changes.
        let rev = rt.stored_settings().1;
        let r = call(&app, Method::POST, "/ui/settings/auth", &cookie, &csrf, Some(&format!("revision={rev}&breakGlass=true&anonymousRole=viewer&sessionIdleMinutes=120&sessionAbsoluteHours=24&local.minPasswordLength=14&local.lockoutFailures=5&local.lockoutMinutes=15&allowedOrigins=&oidc.issuer=&oidc.clientId="))).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
        assert_eq!(rt.settings().session_idle_minutes, 120);
        assert_eq!(rt.settings().local.min_password_length, 14);

        // Account page, then sign-out.
        let r = call(&app, Method::GET, "/ui/account", &cookie, "", None).await;
        assert_eq!(r.status, StatusCode::OK);
        assert!(r.body.contains("break-glass"));
        let r = call(&app, Method::POST, "/ui/logout", &cookie, &csrf, None).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER);
        assert_eq!(r.location, "/ui/login?signed_out=1");
        let r = call(&app, Method::GET, "/ui/resources", &cookie, "", None).await;
        assert!(signed_in_redirect(&r));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn forced_password_change_through_the_pages() {
        let (rt, _) = runtime(env(&[("HFS_CONSOLE_AUTH_MODE", "local")]), Some(BG), opts()).await;
        let (_, otp) = rt
            .create_user(
                "t",
                NewUser {
                    username: "newbie".into(),
                    role: Some(ConsoleRole::Viewer),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let otp = otp.unwrap();
        let app = app(rt.clone());
        let r = call(
            &app,
            Method::POST,
            "/ui/login/local",
            "",
            "",
            Some(&format!("username=newbie&password={otp}&next=/ui")),
        )
        .await;
        assert_eq!(r.location, "/ui/account/password");
        let (cookie, csrf) = cookie_of(&r);
        let r = call(&app, Method::GET, "/ui/resources", &cookie, "", None).await;
        assert_eq!(r.location, "/ui/account/password");
        let r = call(&app, Method::GET, "/ui/account/password", &cookie, "", None).await;
        assert!(r.body.contains(r#"id="must-change""#));
        let r = call(
            &app,
            Method::POST,
            "/ui/account/password",
            &cookie,
            &csrf,
            Some(&format!("current={otp}&new=a&confirm=a")),
        )
        .await;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
        let r = call(
            &app,
            Method::POST,
            "/ui/account/password",
            &cookie,
            &csrf,
            Some(&format!(
                "current={otp}&new=calm-River-77-stone&confirm=calm-River-77-stone"
            )),
        )
        .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
        let r = call(&app, Method::GET, "/ui", &cookie, "", None).await;
        assert!(!signed_in_redirect(&r) && r.location != "/ui/account/password");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oidc_sign_in_page_and_none_mode() {
        let (rt, _) = runtime(
            env(&[
                ("HFS_CONSOLE_AUTH_MODE", "oidc"),
                ("HFS_CONSOLE_OIDC_ISSUER", "http://127.0.0.1:1/realms/r"),
                ("HFS_CONSOLE_OIDC_CLIENT_ID", "hfs-web"),
            ]),
            Some(BG),
            opts(),
        )
        .await;
        let oidc_app = app(rt);
        let r = call(
            &oidc_app,
            Method::GET,
            "/ui/login?next=/ui/resources",
            "",
            "",
            None,
        )
        .await;
        assert!(r.body.contains(r#"id="signin-idp""#));
        assert!(r.body.contains("/ui/login/oidc?next=%2Fui%2Fresources"));
        assert!(r.body.contains(r#"id="signin-break-glass""#));
        assert!(!r.body.contains(r#"id="signin-form""#));
        let r = call(&oidc_app, Method::GET, "/ui/login?local=1", "", "", None).await;
        assert!(r.body.contains(r#"id="signin-form""#));
        // The IdP is down: the start fails with a message, not a crash.
        let r = call(
            &oidc_app,
            Method::GET,
            "/ui/login/oidc?next=/ui",
            "",
            "",
            None,
        )
        .await;
        assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);

        let (rt, _) = runtime(env(&[]), Some(BG), opts()).await;
        let none_app = app(rt);
        let app = none_app;
        let r = call(&app, Method::GET, "/ui/resources", "", "", None).await;
        assert_ne!(r.status, StatusCode::SEE_OTHER, "mode none: open console");
        let r = call(&app, Method::GET, "/ui/packages", "", "", None).await;
        assert!(
            r.location.starts_with("/ui/login?local=1"),
            "{}",
            r.location
        );
        let r = call(&app, Method::GET, "/ui/login", "", "", None).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "nothing to sign in to");
        let r = call(
            &app,
            Method::GET,
            "/ui/admin/login?next=/ui/packages",
            "",
            "",
            None,
        )
        .await;
        assert_eq!(r.location, "/ui/login?local=1&next=/ui/packages");
    }

    #[test]
    fn console_catalogs_share_keys_and_cover_the_templates() {
        use std::collections::BTreeSet;
        let keys = |locale: &str| -> BTreeSet<String> {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../locales")
                .join(locale)
                .join("console-auth.ftl");
            let src = std::fs::read_to_string(&path).unwrap();
            let res = fluent_syntax::parser::parse(src).unwrap();
            res.body
                .iter()
                .filter_map(|e| match e {
                    fluent_syntax::ast::Entry::Message(m) => Some(m.id.name.clone()),
                    _ => None,
                })
                .collect()
        };
        let en = keys("en");
        assert_eq!(
            en,
            keys("zh-Hant"),
            "zh-Hant/console-auth.ftl must match en"
        );
        let main = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../locales/en/main.ftl"),
        )
        .unwrap();
        for tpl in [
            include_str!("../templates/pages/login.html"),
            include_str!("../templates/pages/console-message.html"),
            include_str!("../templates/pages/account.html"),
            include_str!("../templates/pages/settings-auth.html"),
            include_str!("../templates/pages/settings-users.html"),
        ] {
            for (i, _) in tpl.match_indices("(\"console-") {
                let key: String = tpl[i + 2..].chars().take_while(|c| *c != '"').collect();
                assert!(en.contains(&key), "missing {key}");
            }
            for (i, _) in tpl.match_indices("(\"admin-login-") {
                let key: String = tpl[i + 2..].chars().take_while(|c| *c != '"').collect();
                assert!(main.contains(&format!("{key} =")), "missing {key}");
            }
        }
    }
}
