//! The console gate for `/ui` and the console-aware admin API check.
//!
//! [`gate`] decides one `/ui` request: who it is ([`ConsoleAuth::resolve`],
//! else the anonymous visitor of mode `none`), what the route needs
//! ([`access::requirement`]), and — for state-changing cookie-authenticated
//! requests — the CSRF checks (same-site `Origin`/`Referer` and the session's
//! synchronizer token). The UI crate renders the refusals in its own pages.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{OriginalUri, Request};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use helios_endpoints::console::{ConsolePrincipal, ConsoleRole, ConsoleVia};

use super::access::{self, Requirement};
use super::{CSRF_FIELD, CSRF_HEADER, ConsoleAuth, Resolved};
use crate::{AdminUser, is_read_only, login_location};

/// Largest form body read to find a `_csrf` field.
const MAX_FORM: usize = 2 * 1024 * 1024;

/// The CSRF token of the request's session, for templates
/// (`<input type="hidden" name="_csrf">`).
#[derive(Clone, Debug)]
pub struct CsrfToken(pub String);

/// What [`gate`] learned about an admitted request.
#[derive(Clone, Debug, Default)]
pub struct GateInfo {
    /// The principal (a session's, or the anonymous visitor's).
    pub principal: Option<ConsolePrincipal>,
    /// The session, when there is one.
    pub resolved: Option<Resolved>,
}

/// The decision on one `/ui` request.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // one per request, moved once
pub enum GateOutcome {
    /// Go on (the request is returned, its body possibly re-buffered).
    Pass(Box<Request>, GateInfo),
    /// Sign in first: `target` is the sign-in URL with `next`.
    SignIn {
        /// Where to send the browser.
        target: String,
        /// Answer with `HX-Redirect` (htmx) rather than `303`.
        htmx: bool,
        /// A form post without a session: refuse (`401`) rather than redirect.
        refuse: bool,
    },
    /// Signed in, but the role is too low.
    Forbidden {
        /// Who.
        principal: ConsolePrincipal,
        /// What was needed.
        need: ConsoleRole,
    },
    /// The account must change its password first.
    ChangePassword {
        /// htmx request.
        htmx: bool,
    },
    /// CSRF check failed.
    Csrf(&'static str),
}

/// Path of the forced password change.
pub const CHANGE_PASSWORD_PATH: &str = "/ui/account/password";

fn original_path_and_query(request: &Request) -> String {
    request
        .extensions()
        .get::<OriginalUri>()
        .map(|u| u.0.clone())
        .unwrap_or_else(|| request.uri().clone())
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/ui".into())
}

fn is_form(request: &Request) -> bool {
    request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"))
}

/// Finds the CSRF token in the header, else (for urlencoded forms) the
/// `_csrf` field, buffering the body and putting it back.
async fn presented_token(request: Request) -> (Request, Option<String>) {
    if let Some(v) = request
        .headers()
        .get(CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        let v = v.to_string();
        return (request, Some(v));
    }
    if !is_form(&request) {
        return (request, None);
    }
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_FORM).await {
        Ok(b) => b,
        Err(_) => return (Request::from_parts(parts, Body::empty()), None),
    };
    let token = form_urlencoded::parse(&bytes)
        .find(|(k, _)| k == CSRF_FIELD)
        .map(|(_, v)| v.into_owned());
    (Request::from_parts(parts, Body::from(bytes)), token)
}

/// Decides a `/ui` request (see the module docs). Requests outside `/ui`
/// pass untouched.
pub async fn gate(console: &ConsoleAuth, request: Request) -> GateOutcome {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let scoped = request
        .extensions()
        .get::<access::ScopedEndpoint>()
        .map(|s| s.0.clone());
    let Some(requirement) = access::scoped_requirement(&method, &path, scoped.as_deref()) else {
        return GateOutcome::Pass(Box::new(request), GateInfo::default());
    };
    console.refresh_if_stale().await;
    let htmx = request.headers().contains_key("hx-request");
    let resolved = console.resolve(request.headers()).await;
    let principal = resolved
        .as_ref()
        .map(|r| r.principal.clone())
        .or_else(|| console.anonymous());
    let info = GateInfo {
        principal: principal.clone(),
        resolved: resolved.clone(),
    };

    // A forced password change comes before anything but the sign-in flow.
    if let Some(r) = &resolved
        && r.session.must_change_password
        && requirement != Requirement::Public
        && path != CHANGE_PASSWORD_PATH
    {
        return GateOutcome::ChangePassword { htmx };
    }

    let sign_in = |anonymous_visitor: bool| {
        let next = original_path_and_query(&request);
        let local =
            anonymous_visitor || console.mode() == helios_endpoints::console::ConsoleAuthMode::None;
        let target = sign_in_target(&next, local);
        GateOutcome::SignIn {
            target,
            htmx,
            refuse: !is_read_only(&method) && !htmx,
        }
    };

    let need = match &requirement {
        Requirement::Public => None,
        Requirement::SignedIn => {
            if resolved.is_none() {
                return sign_in(principal.is_some());
            }
            None
        }
        Requirement::Role(r) => Some((*r, None)),
        Requirement::EndpointRole(ep, r) => Some((*r, Some(ep.clone()))),
    };
    if let Some((need, endpoint)) = need {
        let Some(p) = &principal else {
            return sign_in(false);
        };
        let have = match &endpoint {
            Some(ep) => p.role_for(ep),
            None => p.role,
        };
        if have < need {
            if p.via == ConsoleVia::Anonymous {
                return sign_in(true);
            }
            return GateOutcome::Forbidden {
                principal: p.clone(),
                need,
            };
        }
    }

    // CSRF: state-changing requests carrying the session cookie. (Anonymous
    // requests carry no credential to forge; the sign-in and sign-out forms
    // check the origin themselves.)
    let mut request = request;
    if !is_read_only(&method)
        && let Some(r) = &resolved
        && requirement != Requirement::Public
    {
        if !console.same_origin(request.headers()) {
            tracing::warn!(subject = %r.session.subject, path, "console write refused: cross-site Origin/Referer");
            return GateOutcome::Csrf("cross-site request refused");
        }
        let (req, token) = presented_token(request).await;
        request = req;
        if !ConsoleAuth::csrf_ok(&r.session, token.as_deref()) {
            tracing::warn!(subject = %r.session.subject, path, "console write refused: missing or wrong CSRF token");
            return GateOutcome::Csrf("missing or invalid CSRF token; reload the page");
        }
    }

    if let Some(r) = &resolved {
        request
            .extensions_mut()
            .insert(CsrfToken(r.session.csrf.clone()));
        request.extensions_mut().insert(r.clone());
    }
    if let Some(p) = &principal {
        request.extensions_mut().insert(p.clone());
    }
    GateOutcome::Pass(Box::new(request), info)
}

impl GateOutcome {
    /// A plain response for a refusal (products may render their own).
    pub fn into_plain_response(self) -> Response {
        let no_store = |mut r: Response| {
            r.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            r
        };
        match self {
            GateOutcome::Pass(..) => StatusCode::OK.into_response(),
            GateOutcome::SignIn {
                target,
                htmx,
                refuse,
            } => no_store(redirect(&target, htmx, refuse)),
            GateOutcome::ChangePassword { htmx } => {
                no_store(redirect(CHANGE_PASSWORD_PATH, htmx, false))
            }
            GateOutcome::Forbidden { need, .. } => no_store(
                (
                    StatusCode::FORBIDDEN,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    format!("this page needs the {} role", need.as_str()),
                )
                    .into_response(),
            ),
            GateOutcome::Csrf(m) => no_store(
                (
                    StatusCode::FORBIDDEN,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    m,
                )
                    .into_response(),
            ),
        }
    }
}

/// `/ui/login[?local=1][&next=…]` (`local` = the password form, for the
/// break-glass administrator or a visitor who needs more than anonymous).
pub fn sign_in_target(next: &str, local: bool) -> String {
    let base = login_location("/ui/login", next);
    match (local, base.contains('?')) {
        (false, _) => base,
        (true, true) => base.replacen("/ui/login?", "/ui/login?local=1&", 1),
        (true, false) => "/ui/login?local=1".into(),
    }
}

/// `303` to `target`, or `HX-Redirect` for htmx, or `401` when `refuse`.
pub fn redirect(target: &str, htmx: bool, refuse: bool) -> Response {
    if htmx {
        let mut r = StatusCode::OK.into_response();
        if let Ok(v) = HeaderValue::from_str(target) {
            r.headers_mut().insert("hx-redirect", v);
        }
        return r;
    }
    if refuse {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "sign-in required",
        )
            .into_response();
    }
    let mut r = StatusCode::SEE_OTHER.into_response();
    if let Ok(v) = HeaderValue::from_str(target) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    r
}

fn api_refusal(status: StatusCode, code: &str, message: &str) -> Response {
    let body = serde_json::json!({
        "resourceType": "OperationOutcome",
        "issue": [{"severity": "error", "code": code, "diagnostics": message}],
    });
    let mut r = (
        status,
        [(header::CONTENT_TYPE, "application/fhir+json")],
        body.to_string(),
    )
        .into_response();
    if status == StatusCode::UNAUTHORIZED {
        r.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"admin\""),
        );
    }
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// The console's rule for a protected admin API request (called by
/// [`crate::guard`] when a console runtime is installed):
///
/// * with `Authorization` (scripts): `Err(None)` — the password gate decides,
///   unless break-glass is off (then `401`);
/// * else a console session with the **global admin** role is required, and
///   a state-changing request must be same-site and carry `X-CSRF-Token`.
///
/// Admitted requests get [`AdminUser`] and the principal stamped.
pub async fn admin_api(console: &Arc<ConsoleAuth>, mut request: Request) -> AdminApiDecision {
    if request.headers().contains_key(header::AUTHORIZATION) {
        if console.break_glass_available() {
            return AdminApiDecision::PasswordGate(request);
        }
        return AdminApiDecision::Refuse(api_refusal(
            StatusCode::UNAUTHORIZED,
            "login",
            "password credentials are disabled (breakGlass off)",
        ));
    }
    console.refresh_if_stale().await;
    let Some(resolved) = console.resolve(request.headers()).await else {
        return AdminApiDecision::Refuse(api_refusal(
            StatusCode::UNAUTHORIZED,
            "login",
            "administrator sign-in required",
        ));
    };
    let path = request.uri().path().to_string();
    if !access::admin_api_allows(&resolved.principal, request.method(), &path) {
        return AdminApiDecision::Refuse(api_refusal(
            StatusCode::FORBIDDEN,
            "forbidden",
            "your console role does not allow this request",
        ));
    }
    if !is_read_only(request.method()) {
        let token = request
            .headers()
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok());
        if !console.same_origin(request.headers())
            || !ConsoleAuth::csrf_ok(&resolved.session, token)
        {
            return AdminApiDecision::Refuse(api_refusal(
                StatusCode::FORBIDDEN,
                "forbidden",
                "cross-site request refused (Origin and X-CSRF-Token required)",
            ));
        }
    }
    let user = if resolved.session.via == ConsoleVia::BreakGlass {
        resolved.session.display_name.clone()
    } else {
        resolved.session.subject.clone()
    };
    let headers = request.headers_mut();
    headers.remove("x-forwarded-user");
    if let Ok(v) = HeaderValue::from_str(&user) {
        headers.insert(HeaderName::from_static("x-remote-user"), v);
    }
    request.extensions_mut().insert(AdminUser(user));
    request.extensions_mut().insert(resolved.principal.clone());
    request.extensions_mut().insert(resolved);
    AdminApiDecision::Admit(request)
}

/// [`admin_api`]'s decision.
#[derive(Debug)]
pub enum AdminApiDecision {
    /// Admitted.
    Admit(Request),
    /// Refused with this response.
    Refuse(Response),
    /// Password credentials: let the break-glass gate decide.
    PasswordGate(Request),
}

/// Whether a method changes state (for callers outside this crate).
pub fn is_write(method: &Method) -> bool {
    !is_read_only(method)
}
