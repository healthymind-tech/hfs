//! Who may call the endpoint admin API.
//!
//! The API is **admin only** (design §7.2). Authorization is a trait,
//! [`AdminAuthorizer`], so the console-role resolver of package P3 (console
//! sessions with role `admin`, per-endpoint bindings) plugs in without
//! touching the handlers. Until then [`AdminCredentialAuthorizer`] admits:
//!
//! 1. a request the server's administrator guard already admitted
//!    ([`helios_admin_auth::AdminUser`] extension, set by the
//!    `HFS_ADMIN_PASSWORD` guard in front of `/admin`);
//! 2. a [`Principal`] produced by the FHIR auth layer: the administrator
//!    credential principal (issuer [`ADMIN_ISSUER`]), or a bearer token
//!    holding the admin token scope (`HFS_ADMIN_TOKEN_SCOPE`, default
//!    `system/*.cruds` — stricter than the `system/*.r` of the older
//!    `/admin/packages` and `/admin/tenants`, design §6);
//! 3. the administrator password itself (`Authorization: Basic` / bearer
//!    password / admin session cookie, same-site writes only) when the
//!    authorizer holds the [`AdminGate`] — so the API is protected even when
//!    it is mounted without the outer guard;
//! 4. nobody else. Anonymous administration is refused (`401`) unless
//!    explicitly allowed for development (`HFS_CONSOLE_ALLOW_ANONYMOUS_ADMIN=true`):
//!    unlike the dev-mode-open legacy admin tier, this API can switch an
//!    endpoint's authentication off.

use std::net::IpAddr;

use async_trait::async_trait;
use axum::http::{Extensions, HeaderMap, Method, StatusCode};
use helios_admin_auth::{AdminGate, AdminUser, Denial};
use helios_auth::{Principal, SmartPermissions};
use helios_endpoints::Actor;

use super::error::ApiError;
use crate::middleware::auth::ADMIN_ISSUER;

/// Environment variable naming the scope an admin bearer token needs.
pub const ADMIN_TOKEN_SCOPE_ENV: &str = "HFS_ADMIN_TOKEN_SCOPE";
/// Default admin token scope.
pub const DEFAULT_ADMIN_TOKEN_SCOPE: &str = "system/*.cruds";
/// Development switch admitting unauthenticated administration.
pub const ALLOW_ANONYMOUS_ENV: &str = "HFS_CONSOLE_ALLOW_ANONYMOUS_ADMIN";

/// What the authorizer sees of a request (borrowed, no body).
#[derive(Debug, Clone, Copy)]
pub struct AdminRequest<'a> {
    /// HTTP method.
    pub method: &'a Method,
    /// Request path (`/admin/endpoints/...`).
    pub path: &'a str,
    /// Headers.
    pub headers: &'a HeaderMap,
    /// Request extensions (principal, admin user, console session…).
    pub extensions: &'a Extensions,
    /// Peer address, when known.
    pub peer: Option<IpAddr>,
}

impl AdminRequest<'_> {
    /// The endpoint the request is about (`/admin/endpoints/{name}/…`), for
    /// per-endpoint role bindings.
    pub fn endpoint(&self) -> Option<&str> {
        let rest = self.path.strip_prefix("/admin/endpoints/")?;
        let name = rest.split('/').next().unwrap_or("");
        (!name.is_empty() && !name.starts_with('$')).then_some(name)
    }

    /// Whether the method changes state.
    pub fn is_write(&self) -> bool {
        !matches!(
            *self.method,
            Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
        )
    }
}

/// Decides whether a request may use the admin API and who it acts as.
#[async_trait]
pub trait AdminAuthorizer: Send + Sync {
    /// `Ok(actor)` admits the request; `Err` is the response to send.
    async fn authorize(&self, request: AdminRequest<'_>) -> Result<Actor, ApiError>;
}

/// The pre-P3 authorizer: administrator credential or admin-scoped token.
#[derive(Clone)]
pub struct AdminCredentialAuthorizer {
    gate: Option<AdminGate>,
    token_permissions: SmartPermissions,
    allow_anonymous: bool,
}

impl std::fmt::Debug for AdminCredentialAuthorizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminCredentialAuthorizer")
            .field("gate", &self.gate.as_ref().map(|g| g.auth().is_some()))
            .field("token_permissions", &self.token_permissions)
            .field("allow_anonymous", &self.allow_anonymous)
            .finish()
    }
}

/// Parses `system/*.<perms>` into the permissions it requires; anything else
/// falls back to the default (`cruds`) with a warning.
pub fn parse_admin_scope(spec: &str) -> SmartPermissions {
    let perms = spec
        .trim()
        .strip_prefix("system/*.")
        .and_then(SmartPermissions::from_permission_str)
        .filter(|p| !p.is_empty());
    match perms {
        Some(p) => p,
        None => {
            tracing::warn!(
                value = spec,
                "{ADMIN_TOKEN_SCOPE_ENV} must look like system/*.cruds; using {DEFAULT_ADMIN_TOKEN_SCOPE}"
            );
            SmartPermissions::all()
        }
    }
}

impl AdminCredentialAuthorizer {
    /// An authorizer checking `gate` (when it signs people in), tokens for
    /// `system/*.cruds`, and refusing anonymous callers.
    pub fn new(gate: Option<AdminGate>) -> Self {
        Self {
            gate,
            token_permissions: SmartPermissions::all(),
            allow_anonymous: false,
        }
    }

    /// From the environment: [`ADMIN_TOKEN_SCOPE_ENV`], [`ALLOW_ANONYMOUS_ENV`].
    pub fn from_env(gate: Option<AdminGate>) -> Self {
        let scope = std::env::var(ADMIN_TOKEN_SCOPE_ENV)
            .ok()
            .filter(|s| !s.trim().is_empty());
        let allow = std::env::var(ALLOW_ANONYMOUS_ENV)
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        if allow {
            tracing::warn!(
                "{ALLOW_ANONYMOUS_ENV}=true: the endpoint admin API admits unauthenticated callers (development only)"
            );
        }
        Self {
            token_permissions: scope
                .as_deref()
                .map(parse_admin_scope)
                .unwrap_or(SmartPermissions::all()),
            allow_anonymous: allow,
            ..Self::new(gate)
        }
    }

    /// Sets the permissions an admin token needs (`system/*.<perms>`).
    pub fn with_token_permissions(mut self, permissions: SmartPermissions) -> Self {
        self.token_permissions = permissions;
        self
    }

    /// Admits unauthenticated callers (development only).
    pub fn allow_anonymous(mut self, allow: bool) -> Self {
        self.allow_anonymous = allow;
        self
    }
}

fn admin_actor(user: &str, via: &str) -> Actor {
    Actor {
        subject: "admin".into(),
        display: Some(user.to_string()),
        via: via.into(),
    }
}

fn denied(status: StatusCode, code: &'static str, message: &str) -> ApiError {
    ApiError::new(status, code, message)
}

#[async_trait]
impl AdminAuthorizer for AdminCredentialAuthorizer {
    async fn authorize(&self, request: AdminRequest<'_>) -> Result<Actor, ApiError> {
        if let Some(AdminUser(user)) = request.extensions.get::<AdminUser>() {
            return Ok(admin_actor(user, "admin-password"));
        }
        if let Some(p) = request.extensions.get::<Principal>() {
            if p.issuer == ADMIN_ISSUER {
                return Ok(admin_actor(&p.subject, "admin-password"));
            }
            if p.scopes.has_system_scope(self.token_permissions) {
                return Ok(Actor {
                    subject: format!("token:{}|{}", p.issuer, p.subject),
                    display: None,
                    via: "bearer".into(),
                });
            }
            return Err(denied(
                StatusCode::FORBIDDEN,
                "forbidden",
                "the endpoint admin API needs an administrator: a system/* token with the admin scope (HFS_ADMIN_TOKEN_SCOPE) or the administrator credential",
            ));
        }
        match &self.gate {
            Some(AdminGate::Password(auth)) => {
                match auth.authorize(request.method, request.headers, request.peer) {
                    Ok(granted) => {
                        let via = match granted.via {
                            helios_admin_auth::Via::Session => "admin-session",
                            helios_admin_auth::Via::Credentials => "admin-password",
                        };
                        return Ok(admin_actor(&granted.user, via));
                    }
                    Err(Denial::Unauthenticated) if self.allow_anonymous => {}
                    Err(Denial::Unauthenticated | Denial::BadCredentials) => {
                        return Err(denied(
                            StatusCode::UNAUTHORIZED,
                            "login",
                            "administrator sign-in required",
                        ));
                    }
                    Err(Denial::RateLimited(left)) => {
                        let mut e = denied(
                            StatusCode::TOO_MANY_REQUESTS,
                            "throttled",
                            "too many failed sign-in attempts",
                        );
                        e.retry_after = Some(left.as_secs().max(1));
                        return Err(e);
                    }
                    Err(Denial::CrossOrigin) => {
                        return Err(denied(
                            StatusCode::FORBIDDEN,
                            "forbidden",
                            "cross-site request refused",
                        ));
                    }
                    Err(Denial::NotConfigured) => {
                        return Err(denied(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "transient",
                            "administration is locked: no administrator password is configured",
                        ));
                    }
                }
            }
            Some(AdminGate::Locked(message)) => {
                return Err(denied(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "transient",
                    message,
                ));
            }
            Some(AdminGate::Open) | None => {}
        }
        if self.allow_anonymous {
            return Ok(Actor {
                subject: "anonymous".into(),
                display: None,
                via: "anonymous".into(),
            });
        }
        Err(denied(
            StatusCode::UNAUTHORIZED,
            "login",
            "the endpoint admin API needs an administrator (HFS_ADMIN_PASSWORD credential, an admin console session, or an admin-scoped token)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_admin_auth::{AdminSettings, CookieSpec};

    fn req<'a>(
        method: &'a Method,
        path: &'a str,
        headers: &'a HeaderMap,
        ext: &'a Extensions,
    ) -> AdminRequest<'a> {
        AdminRequest {
            method,
            path,
            headers,
            extensions: ext,
            peer: None,
        }
    }

    fn principal(issuer: &str, scopes: &str) -> Principal {
        Principal {
            subject: "svc".into(),
            issuer: issuer.into(),
            tenant_id: None,
            scopes: helios_auth::ScopeSet::parse(scopes),
            jti: None,
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
            custom_claims: Default::default(),
            launch_context: None,
        }
    }

    fn password_gate() -> AdminGate {
        let settings =
            AdminSettings::from_lookup("HFS", |_| None).with_password("correct horse battery");
        AdminGate::configure(
            "HFS",
            settings,
            true,
            CookieSpec {
                name: "hfs_admin",
                path: "/",
            },
        )
    }

    #[test]
    fn endpoint_of_path() {
        let h = HeaderMap::new();
        let e = Extensions::new();
        let m = Method::GET;
        assert_eq!(
            req(&m, "/admin/endpoints/tw/packages", &h, &e).endpoint(),
            Some("tw")
        );
        assert_eq!(req(&m, "/admin/endpoints", &h, &e).endpoint(), None);
        assert_eq!(req(&m, "/admin/endpoints/$rewrap", &h, &e).endpoint(), None);
        assert!(!req(&m, "/admin/endpoints", &h, &e).is_write());
        assert!(req(&Method::PATCH, "/admin/endpoints", &h, &e).is_write());
    }

    #[test]
    fn admin_scope_parsing() {
        assert_eq!(parse_admin_scope("system/*.cruds"), SmartPermissions::all());
        assert_eq!(parse_admin_scope("system/*.r"), SmartPermissions::READ);
        assert_eq!(parse_admin_scope("user/*.r"), SmartPermissions::all());
        assert_eq!(parse_admin_scope("system/*.x"), SmartPermissions::all());
    }

    #[tokio::test]
    async fn anonymous_is_refused_unless_allowed() {
        let h = HeaderMap::new();
        let e = Extensions::new();
        let a = AdminCredentialAuthorizer::new(None);
        let err = a
            .authorize(req(&Method::GET, "/admin/endpoints", &h, &e))
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
        let a = a.allow_anonymous(true);
        let actor = a
            .authorize(req(&Method::GET, "/admin/endpoints", &h, &e))
            .await
            .unwrap();
        assert_eq!(actor.via, "anonymous");
    }

    #[tokio::test]
    async fn admin_user_extension_and_admin_principal_are_admitted() {
        let h = HeaderMap::new();
        let mut e = Extensions::new();
        e.insert(AdminUser("root".into()));
        let a = AdminCredentialAuthorizer::new(None);
        let actor = a
            .authorize(req(&Method::POST, "/admin/endpoints", &h, &e))
            .await
            .unwrap();
        assert_eq!(actor.subject, "admin");
        assert_eq!(actor.display.as_deref(), Some("root"));

        let mut e = Extensions::new();
        e.insert(principal(ADMIN_ISSUER, ""));
        assert!(
            a.authorize(req(&Method::POST, "/admin/endpoints", &h, &e))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn tokens_need_the_admin_scope() {
        let h = HeaderMap::new();
        let a = AdminCredentialAuthorizer::new(None);
        let mut e = Extensions::new();
        e.insert(principal("https://idp", "system/*.r"));
        let err = a
            .authorize(req(&Method::GET, "/admin/endpoints", &h, &e))
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        let mut e = Extensions::new();
        e.insert(principal("https://idp", "system/*.cruds"));
        let actor = a
            .authorize(req(&Method::GET, "/admin/endpoints", &h, &e))
            .await
            .unwrap();
        assert_eq!(actor.subject, "token:https://idp|svc");
        assert_eq!(actor.via, "bearer");
        // A relaxed scope setting admits a read-only token.
        let a = a.with_token_permissions(SmartPermissions::READ);
        let mut e = Extensions::new();
        e.insert(principal("https://idp", "system/*.r"));
        assert!(
            a.authorize(req(&Method::GET, "/admin/endpoints", &h, &e))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn password_gate_checks_credentials() {
        use axum::http::{HeaderValue, header};
        use base64::Engine as _;
        let a = AdminCredentialAuthorizer::new(Some(password_gate()));
        let e = Extensions::new();
        let mut h = HeaderMap::new();
        let err = a
            .authorize(req(&Method::GET, "/admin/endpoints", &h, &e))
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
        let good = base64::engine::general_purpose::STANDARD.encode("admin:correct horse battery");
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {good}")).unwrap(),
        );
        let actor = a
            .authorize(req(&Method::POST, "/admin/endpoints", &h, &e))
            .await
            .unwrap();
        assert_eq!(actor.via, "admin-password");
        let bad = base64::engine::general_purpose::STANDARD.encode("admin:wrong");
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {bad}")).unwrap(),
        );
        let err = a
            .authorize(req(&Method::GET, "/admin/endpoints", &h, &e))
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn locked_gate_answers_503() {
        let a = AdminCredentialAuthorizer::new(Some(AdminGate::Locked("locked".into())));
        let h = HeaderMap::new();
        let e = Extensions::new();
        let err = a
            .authorize(req(&Method::GET, "/admin/endpoints", &h, &e))
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
        // Open gate behaves like no gate.
        let a = AdminCredentialAuthorizer::new(Some(AdminGate::Open));
        let err = a
            .authorize(req(&Method::GET, "/admin/endpoints", &h, &e))
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
        assert!(format!("{a:?}").contains("AdminCredentialAuthorizer"));
    }
}
