//! Authentication hook of the FHIR stack (design §6, package P2 fills it).
//!
//! [`auth_layer`] installs authentication, the principal → audit bridge,
//! authorization and the per-client rate limiter around the FHIR router. P1
//! ships the pass-through below, which reproduces exactly the global layering
//! `build_app` used before the manager (one `AuthMiddlewareState`, layered
//! only when `HFS_AUTH_MODE` is not `off`). Package P2 replaces the body with
//! the always-installed per-endpoint `endpoint_auth` layer, reading the
//! `ResolvedEndpoint` extension the dispatcher inserts (the dispatcher wraps
//! this stack, so the request path seen here is already endpoint-relative).

use std::sync::Arc;

use axum::Router;

use crate::middleware;

/// What the FHIR stack's authentication is built from.
#[derive(Clone, Default)]
pub struct AuthHook {
    /// The server-wide auth state (`HFS_AUTH_*`); `None` when auth is off.
    pub auth_state: Option<Arc<middleware::auth::AuthMiddlewareState>>,
    /// The per-client rate limiter (`HFS_RATE_LIMIT_*`), when enabled.
    pub rate_limiter: Option<Arc<middleware::client_rate_limit::ClientRateLimiter>>,
}

/// Wraps the FHIR router in authentication, authorization and rate limiting.
///
/// Order, innermost first: authorization, principal capture (between
/// authentication and authorization, so a request authorization refuses is
/// still attributed to its user), rate limit (inside authentication, so a
/// request is billed to its OAuth client), authentication.
pub fn auth_layer(router: Router, hook: AuthHook) -> Router {
    let AuthHook {
        auth_state,
        rate_limiter,
    } = hook;
    if let Some(auth) = auth_state {
        let router = router
            .layer(axum::middleware::from_fn_with_state(
                auth.clone(),
                middleware::auth::authz_middleware,
            ))
            .layer(axum::middleware::from_fn(
                middleware::audit::capture_principal,
            ));
        let router = match rate_limiter {
            Some(limiter) => router.layer(axum::middleware::from_fn_with_state(
                limiter,
                middleware::client_rate_limit::client_rate_limit_middleware,
            )),
            None => router,
        };
        router.layer(axum::middleware::from_fn_with_state(
            auth,
            middleware::auth::auth_middleware,
        ))
    } else {
        match rate_limiter {
            Some(limiter) => router.layer(axum::middleware::from_fn_with_state(
                limiter,
                middleware::client_rate_limit::client_rate_limit_middleware,
            )),
            None => router,
        }
    }
}
