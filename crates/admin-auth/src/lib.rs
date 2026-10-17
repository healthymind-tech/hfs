//! Password sign-in for the administrative surfaces of HFS and HTS.
//!
//! Both servers can expose an administrative console and API
//! (`HFS_ADMIN_UI`, `HTS_ADMIN_UI`): IG package installs, tenant
//! maintenance, terminology imports and deletes, job lists that name who
//! asked from where. Until SMART/OIDC admin scopes land, this crate is what
//! keeps a stranger out of them:
//!
//! * one administrator per server, `<PREFIX>_ADMIN_USER` (default `admin`)
//!   with `<PREFIX>_ADMIN_PASSWORD` or `<PREFIX>_ADMIN_PASSWORD_FILE`
//!   ([`AdminSettings`]);
//! * **fail closed**: with administration enabled and no password, every
//!   admin endpoint answers `503` ([`AdminGate::Locked`]) — never open;
//! * browsers sign in on a form and get a session cookie — a random 256-bit
//!   token, `HttpOnly; SameSite=Strict`, `Secure` over HTTPS, idle expiry —
//!   and state-changing requests made with it must carry a same-site
//!   `Origin`/`Referer` (CSRF);
//! * scripts send `Authorization: Basic` (`curl -u admin:…`) or
//!   `Bearer <password>`;
//! * five failures from one client within 15 minutes lock that client out
//!   for 15 minutes ([`auth::MAX_FAILURES`]); failures are logged at `warn`
//!   with the client address only.
//!
//! [`guard`] is the axum middleware that applies a [`AdminGate`] to the
//! paths a product classifies as administrative; the product renders its own
//! sign-in page and calls [`AdminAuth::login`] / [`AdminAuth::logout`].

pub mod auth;
pub mod client;
pub mod settings;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, OriginalUri, Request, State};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

pub use auth::{AdminAuth, CookieSpec, Denial, Granted, LoginOutcome, Via};
pub use client::IpNet;
pub use settings::{AdminSettings, CookieSecure, DEFAULT_USER, Secret};

/// Whether, and how, administrative endpoints admit a request.
#[derive(Clone, Debug)]
pub enum AdminGate {
    /// No admin protection: administration is disabled, or another layer
    /// (SMART/OIDC authentication) is responsible for it.
    Open,
    /// Administration is enabled but cannot be signed in to (no password, or
    /// a broken password file): every admin endpoint refuses with `503`.
    Locked(Arc<str>),
    /// Password sign-in.
    Password(Arc<AdminAuth>),
}

impl AdminGate {
    /// Builds the gate for a server from its settings and logs the outcome
    /// once. `prefix` is the environment-variable prefix (`HFS`, `HTS`);
    /// `admin_enabled` is the product's admin switch (`*_ADMIN_UI`).
    ///
    /// A configured password always enables the gate. Without one, an
    /// enabled admin surface is [`Locked`](AdminGate::Locked) and a disabled
    /// one [`Open`](AdminGate::Open) (there is then nothing to protect, or
    /// the product's own switch already refuses).
    pub fn configure(
        prefix: &'static str,
        settings: AdminSettings,
        admin_enabled: bool,
        cookie: CookieSpec,
    ) -> Self {
        if let Some(error) = &settings.error {
            tracing::error!(
                "{prefix} admin sign-in is misconfigured ({error}); administrative endpoints \
                 refuse every request until it is fixed"
            );
            return Self::Locked(Arc::from(format!("administration is locked: {error}")));
        }
        match settings.password.clone() {
            Some(password) => {
                if password.expose().chars().count() < 12 {
                    tracing::warn!(
                        "{prefix}_ADMIN_PASSWORD is shorter than 12 characters; use a long random \
                         password"
                    );
                }
                tracing::info!(
                    user = %settings.user,
                    idle_minutes = settings.session_idle.as_secs() / 60,
                    "{prefix} admin sign-in enabled"
                );
                Self::Password(Arc::new(AdminAuth::new(prefix, settings, password, cookie)))
            }
            None if admin_enabled => {
                tracing::warn!(
                    "{prefix}_ADMIN_UI=true but neither {prefix}_ADMIN_PASSWORD nor \
                     {prefix}_ADMIN_PASSWORD_FILE is set: administrative endpoints refuse every \
                     request (503) until one is"
                );
                Self::Locked(Arc::from(format!(
                    "administration is locked: set {prefix}_ADMIN_PASSWORD or \
                     {prefix}_ADMIN_PASSWORD_FILE to enable sign-in"
                )))
            }
            None => Self::Open,
        }
    }

    /// The authenticator, when the gate signs people in.
    pub fn auth(&self) -> Option<&Arc<AdminAuth>> {
        match self {
            Self::Password(auth) => Some(auth),
            _ => None,
        }
    }

    /// Whether admin endpoints are refusing everything.
    pub fn is_locked(&self) -> bool {
        matches!(self, Self::Locked(_))
    }

    /// The signed-in admin of a request's session cookie.
    pub fn session_user(&self, headers: &axum::http::HeaderMap) -> Option<String> {
        self.auth().and_then(|auth| auth.session_user(headers))
    }

    /// Handles a sign-in form submission.
    pub fn login(
        &self,
        headers: &axum::http::HeaderMap,
        peer: Option<IpAddr>,
        user: &str,
        password: &str,
    ) -> LoginOutcome {
        match self {
            Self::Password(auth) => auth.login(headers, peer, user, password),
            _ => LoginOutcome::NotConfigured,
        }
    }
}

/// The signed-in administrator, stamped on the request extensions by
/// [`guard`] (on every request it sees that carries a live session, not only
/// protected ones) so pages can offer Sign out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminUser(pub String);

/// How refusals are worded.
#[derive(Clone, Copy, Debug)]
pub enum Style {
    /// A JSON API: `401`/`403`/`429`/`503` with a JSON body — a FHIR
    /// `OperationOutcome` when `fhir`, else `{"error", "message"}`.
    Api {
        /// Emit an `OperationOutcome`.
        fhir: bool,
    },
    /// Browser pages: a page load without a session is sent (`303`) to
    /// `login_path?next=…`; htmx requests get `HX-Redirect` instead.
    Browser {
        /// Absolute path of the sign-in page.
        login_path: &'static str,
    },
}

/// State of the [`guard`] middleware.
#[derive(Clone)]
pub struct Guard {
    /// The gate to apply.
    pub gate: AdminGate,
    /// Whether `(method, path)` is administrative. `path` is the path the
    /// router layer sees (after any `nest` prefix was stripped).
    pub protects: fn(&Method, &str) -> bool,
    /// How refusals look.
    pub style: Style,
}

/// axum middleware applying a [`Guard`]:
/// `axum::middleware::from_fn_with_state(guard_state, helios_admin_auth::guard)`.
///
/// On an admitted request the client-supplied `X-Forwarded-User` is dropped
/// and `X-Remote-User` set to the admin user, so audit trails that read those
/// headers name the authenticated operator rather than whatever the client
/// claimed.
pub async fn guard(State(guard): State<Guard>, mut request: Request, next: Next) -> Response {
    let protected = (guard.protects)(request.method(), request.uri().path());
    match &guard.gate {
        AdminGate::Open => next.run(request).await,
        AdminGate::Locked(message) => {
            if protected {
                refuse(guard.style, &request, Denial::NotConfigured, message)
            } else {
                next.run(request).await
            }
        }
        AdminGate::Password(auth) => {
            if !protected {
                if let Some(user) = auth.session_user(request.headers()) {
                    request.extensions_mut().insert(AdminUser(user));
                }
                return next.run(request).await;
            }
            let peer = request
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|info| info.0.ip());
            match auth.authorize(request.method(), request.headers(), peer) {
                Ok(granted) => {
                    let headers = request.headers_mut();
                    headers.remove("x-forwarded-user");
                    if let Ok(value) = HeaderValue::from_str(&granted.user) {
                        headers.insert(HeaderName::from_static("x-remote-user"), value);
                    }
                    request.extensions_mut().insert(AdminUser(granted.user));
                    next.run(request).await
                }
                Err(denial) => refuse(guard.style, &request, denial, ""),
            }
        }
    }
}

fn refuse(style: Style, request: &Request, denial: Denial, locked_message: &str) -> Response {
    let (status, code, message): (StatusCode, &str, String) = match &denial {
        Denial::Unauthenticated => (
            StatusCode::UNAUTHORIZED,
            "login",
            "administrator sign-in required".into(),
        ),
        Denial::BadCredentials => (
            StatusCode::UNAUTHORIZED,
            "login",
            "invalid administrator credentials".into(),
        ),
        Denial::RateLimited(_) => (
            StatusCode::TOO_MANY_REQUESTS,
            "throttled",
            "too many failed sign-in attempts; try again later".into(),
        ),
        Denial::CrossOrigin => (
            StatusCode::FORBIDDEN,
            "forbidden",
            "cross-site request refused".into(),
        ),
        Denial::NotConfigured => (
            StatusCode::SERVICE_UNAVAILABLE,
            "forbidden",
            if locked_message.is_empty() {
                "administration is locked".into()
            } else {
                locked_message.to_string()
            },
        ),
    };

    let mut response = match style {
        Style::Browser { login_path } => {
            let browser_sign_in = matches!(denial, Denial::Unauthenticated | Denial::NotConfigured);
            let page_load = matches!(*request.method(), Method::GET | Method::HEAD);
            let htmx = request.headers().contains_key("hx-request");
            if browser_sign_in && (page_load || htmx) {
                let next = request
                    .extensions()
                    .get::<OriginalUri>()
                    .map(|uri| uri.0.clone())
                    .unwrap_or_else(|| request.uri().clone());
                let next = next
                    .path_and_query()
                    .map(|pq| pq.as_str().to_string())
                    .unwrap_or_default();
                let target = login_location(login_path, &next);
                if htmx {
                    // htmx cannot follow a redirect into a full page; ask it
                    // to navigate the window instead.
                    let mut r = StatusCode::OK.into_response();
                    if let Ok(v) = HeaderValue::from_str(&target) {
                        r.headers_mut().insert("hx-redirect", v);
                    }
                    r
                } else {
                    let mut r = StatusCode::SEE_OTHER.into_response();
                    if let Ok(v) = HeaderValue::from_str(&target) {
                        r.headers_mut().insert(header::LOCATION, v);
                    }
                    r
                }
            } else {
                (
                    status,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    message,
                )
                    .into_response()
            }
        }
        Style::Api { fhir } => {
            let body = if fhir {
                serde_json::json!({
                    "resourceType": "OperationOutcome",
                    "issue": [{"severity": "error", "code": code, "diagnostics": message}],
                })
            } else {
                serde_json::json!({"error": code, "message": message})
            };
            let content_type = if fhir {
                "application/fhir+json"
            } else {
                "application/json"
            };
            (
                status,
                [(header::CONTENT_TYPE, content_type)],
                body.to_string(),
            )
                .into_response()
        }
    };
    if status == StatusCode::UNAUTHORIZED {
        // `Bearer`, not `Basic`: a Basic challenge makes browsers pop up a
        // native dialog on every expired-session fetch, and cache whatever
        // is typed into it. `curl -u` sends Basic without being asked.
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"admin\""),
        );
    }
    if let Denial::RateLimited(left) = denial
        && let Ok(v) = HeaderValue::from_str(&left.as_secs().max(1).to_string())
    {
        response.headers_mut().insert(header::RETRY_AFTER, v);
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `login_path?next=<next>`.
pub fn login_location(login_path: &str, next: &str) -> String {
    if next.is_empty() {
        return login_path.to_string();
    }
    let encoded: String = next
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("{login_path}?next={encoded}")
}

/// A post-sign-in redirect target from a client-supplied `next`: kept only
/// when it is a local absolute path under `prefix` (no scheme, no `//`
/// network-path, no backslash), else `fallback`. Stops the sign-in page from
/// being an open redirect.
pub fn safe_next(next: Option<&str>, prefix: &str, fallback: &str) -> String {
    match next {
        Some(n)
            if n.starts_with('/')
                && !n.starts_with("//")
                && !n.contains('\\')
                && !n.chars().any(char::is_control)
                && (n == prefix
                    || n.starts_with(&format!("{prefix}/"))
                    || n.starts_with(&format!("{prefix}?"))) =>
        {
            n.to_string()
        }
        _ => fallback.to_string(),
    }
}

/// Whether `path` is `prefix` or below it (`/ui/packages`,
/// `/ui/packages/x`, but not `/ui/packagesx`).
pub fn under(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether `method` cannot change state.
pub fn is_read_only(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use base64::Engine;
    use tower::ServiceExt;

    const SPEC: CookieSpec = CookieSpec {
        name: "hts_admin",
        path: "/",
    };

    fn protects(_: &Method, path: &str) -> bool {
        under(path, "/admin")
    }

    fn app(gate: AdminGate, style: Style) -> Router {
        Router::new()
            .route(
                "/admin/jobs",
                get(|| async { "jobs" }).post(|| async { "started" }),
            )
            .route("/ValueSet/$expand", get(|| async { "public" }))
            .layer(axum::middleware::from_fn_with_state(
                Guard {
                    gate,
                    protects,
                    style,
                },
                guard,
            ))
    }

    fn password_gate() -> AdminGate {
        AdminGate::configure(
            "HTS",
            AdminSettings::default().with_password("test-password-123"),
            true,
            SPEC,
        )
    }

    async fn status(
        app: &Router,
        req: axum::http::Request<Body>,
    ) -> (StatusCode, axum::http::HeaderMap) {
        let r = app.clone().oneshot(req).await.unwrap();
        (r.status(), r.headers().clone())
    }

    #[tokio::test]
    async fn api_style_refuses_without_credentials_and_admits_basic() {
        let app = app(password_gate(), Style::Api { fhir: false });
        let (s, h) = status(
            &app,
            axum::http::Request::get("/admin/jobs")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert!(h.contains_key(header::WWW_AUTHENTICATE));

        let basic = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("admin:test-password-123")
        );
        let (s, _) = status(
            &app,
            axum::http::Request::post("/admin/jobs")
                .header("authorization", &basic)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::OK);

        // Public routes never ask.
        let (s, _) = status(
            &app,
            axum::http::Request::get("/ValueSet/$expand")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn locked_gate_refuses_admin_and_leaves_public_open() {
        let gate = AdminGate::configure("HTS", AdminSettings::default(), true, SPEC);
        assert!(gate.is_locked());
        let app = app(gate, Style::Api { fhir: true });
        let r = app
            .clone()
            .oneshot(
                axum::http::Request::get("/admin/jobs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = http_body_util::BodyExt::collect(r.into_body())
            .await
            .unwrap()
            .to_bytes();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("HTS_ADMIN_PASSWORD"), "{text}");
        assert!(text.contains("OperationOutcome"), "{text}");
        // Even "credentials" do not open a locked gate.
        let (s, _) = status(
            &app,
            axum::http::Request::get("/admin/jobs")
                .header("authorization", "Bearer anything")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        let (s, _) = status(
            &app,
            axum::http::Request::get("/ValueSet/$expand")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn disabled_admin_without_password_is_open() {
        let gate = AdminGate::configure("HTS", AdminSettings::default(), false, SPEC);
        assert!(matches!(gate, AdminGate::Open));
    }

    #[tokio::test]
    async fn browser_style_redirects_page_loads_and_htmx_to_the_login() {
        let app = app(
            password_gate(),
            Style::Browser {
                login_path: "/ui/hts/login",
            },
        );
        let (s, h) = status(
            &app,
            axum::http::Request::get("/admin/jobs?x=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        assert_eq!(
            h[header::LOCATION],
            "/ui/hts/login?next=/admin/jobs%3Fx%3D1"
        );
        let (s, h) = status(
            &app,
            axum::http::Request::post("/admin/jobs")
                .header("hx-request", "true")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(h.contains_key("hx-redirect"));
        // A plain form POST without a session is refused, not redirected.
        let (s, _) = status(
            &app,
            axum::http::Request::post("/admin/jobs")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn safe_next_rejects_open_redirects() {
        let f = "/ui";
        assert_eq!(
            safe_next(Some("/ui/packages?x=1"), "/ui", f),
            "/ui/packages?x=1"
        );
        assert_eq!(safe_next(Some("/ui"), "/ui", f), "/ui");
        assert_eq!(safe_next(Some("//evil.example/ui"), "/ui", f), f);
        assert_eq!(safe_next(Some("https://evil.example"), "/ui", f), f);
        assert_eq!(safe_next(Some("/\\evil"), "/", f), f);
        assert_eq!(safe_next(Some("/uix"), "/ui", f), f);
        assert_eq!(safe_next(Some("/fhir/Patient"), "/ui", f), f);
        assert_eq!(safe_next(None, "/ui", f), f);
    }

    #[test]
    fn under_matches_whole_segments() {
        assert!(under("/ui/packages", "/ui/packages"));
        assert!(under("/ui/packages/a/1", "/ui/packages"));
        assert!(!under("/ui/packagesx", "/ui/packages"));
        assert!(!under("/ui/tenant", "/ui/tenants"));
    }
}
