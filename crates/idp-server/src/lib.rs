//! HTTP protocol endpoints of the built-in authorization server (packages
//! I1 + I3 of docs/design/authorization-server.md).
//!
//! | route | spec |
//! |---|---|
//! | `GET/POST /idp/{realm}/authorize` | RFC 6749 §4.1, SMART "authorize-post", OIDC Core §3.1.2 |
//! | `POST /idp/{realm}/token` | RFC 6749 §4.1.3/§4.4/§6, RFC 7636, RFC 7523, SMART Backend Services |
//! | `POST /idp/{realm}/introspect` | RFC 7662, SMART "Token Introspection" |
//! | `POST /idp/{realm}/revoke` | RFC 7009 |
//! | `GET/POST /idp/{realm}/userinfo` | OIDC Core §5.3 |
//! | `GET /idp/{realm}/jwks` | RFC 7517 |
//! | `GET /idp/{realm}/.well-known/openid-configuration` | OIDC Discovery §4 |
//! | `GET /.well-known/oauth-authorization-server/idp/{realm}` | RFC 8414 §3 |
//!
//! The interactive steps (login, patient picker, consent, logout) are the
//! hosted pages of `helios-idp-pages`; `/authorize` hands them a
//! [`helios_idp::interaction::PendingAuthorization`] and they finish through
//! [`IdpServer::completer`].
//!
//! Mount: `app.merge(server.router())` (behind `HFS_IDP=on`). Rate limits
//! and audit are layered by the composition root.

mod authorize;
pub mod client_auth;
mod completer;
mod discovery;
mod http;
mod introspect;
pub mod jwks_fetch;
mod token;
mod userinfo;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use helios_endpoints::OutboundPolicy;
use helios_idp::interaction::AuthorizationCompleter;
use helios_idp::runtime::{IdpRuntime, RealmRuntime};
use serde_json::Value;

pub use http::ClientIp;
pub use jwks_fetch::{HttpJwksFetcher, JwksFetcher};

/// Construction options.
pub struct IdpServerOptions {
    /// Outbound SSRF policy for client `jwks_uri` fetches.
    pub outbound: Arc<OutboundPolicy>,
    /// Replaces the HTTP JWKS fetcher (tests).
    pub jwks_fetcher: Option<Arc<dyn JwksFetcher>>,
    /// Concurrent argon2id verifications (client secrets), DoS bound.
    pub argon2_permits: usize,
    /// Pending authorization lifetime (seconds).
    pub interaction_ttl_secs: u32,
    /// Minimum interval between forced client JWKS refetches (unknown kid).
    pub jwks_min_refetch: std::time::Duration,
}

impl Default for IdpServerOptions {
    fn default() -> Self {
        Self {
            outbound: Arc::new(OutboundPolicy::strict()),
            jwks_fetcher: None,
            argon2_permits: 4,
            interaction_ttl_secs: 900,
            jwks_min_refetch: jwks_fetch::MIN_REFETCH,
        }
    }
}

pub(crate) struct Inner {
    pub rt: Arc<IdpRuntime>,
    pub jwks: jwks_fetch::JwksCache,
    pub argon2: tokio::sync::Semaphore,
    pub interaction_ttl_secs: u32,
}

/// The protocol server (cheap to clone).
#[derive(Clone)]
pub struct IdpServer {
    pub(crate) inner: Arc<Inner>,
}

impl IdpServer {
    pub fn new(rt: Arc<IdpRuntime>, opts: IdpServerOptions) -> Self {
        let fetcher = opts
            .jwks_fetcher
            .unwrap_or_else(|| Arc::new(HttpJwksFetcher::new(opts.outbound.clone())));
        Self {
            inner: Arc::new(Inner {
                rt,
                jwks: jwks_fetch::JwksCache::new(fetcher, opts.jwks_min_refetch),
                argon2: tokio::sync::Semaphore::new(opts.argon2_permits.max(1)),
                interaction_ttl_secs: opts.interaction_ttl_secs.clamp(60, 3600),
            }),
        }
    }

    pub fn runtime(&self) -> &Arc<IdpRuntime> {
        &self.inner.rt
    }

    /// The completer the hosted pages finish interactions with.
    pub fn completer(&self) -> Arc<dyn AuthorizationCompleter> {
        Arc::new(self.clone())
    }

    /// The protocol routes.
    pub fn router(&self) -> Router {
        let api = Router::new()
            .route("/idp/{realm}/token", post(token::token))
            .route("/idp/{realm}/introspect", post(introspect::introspect))
            .route("/idp/{realm}/revoke", post(introspect::revoke))
            .route(
                "/idp/{realm}/userinfo",
                get(userinfo::userinfo_get).post(userinfo::userinfo_post),
            )
            .route("/idp/{realm}/jwks", get(discovery::jwks))
            .route(
                "/idp/{realm}/.well-known/openid-configuration",
                get(discovery::openid_configuration),
            )
            .route(
                "/idp/{realm}/.well-known/oauth-authorization-server",
                get(discovery::openid_configuration),
            )
            .route(
                "/.well-known/oauth-authorization-server/idp/{realm}",
                get(discovery::openid_configuration),
            )
            .route(
                "/.well-known/openid-configuration/idp/{realm}",
                get(discovery::openid_configuration),
            )
            // Browser apps call these cross-origin (no cookies involved).
            .layer(axum::middleware::from_fn(http::cors))
            .layer(axum::extract::DefaultBodyLimit::max(64 * 1024));
        Router::new()
            .route(
                "/idp/{realm}/authorize",
                get(authorize::authorize_get).post(authorize::authorize_post),
            )
            .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
            .merge(api)
            .layer(axum::middleware::from_fn(http::security_headers))
            .with_state(self.clone())
    }
}

/// `{fhir-base}/.well-known/smart-configuration` for an endpoint bound to
/// this realm (SMART App Launch 2.2 "Conformance").
pub fn smart_configuration(realm: &RealmRuntime) -> Value {
    helios_idp::metadata::smart_configuration(&realm.spec, &realm.urls)
}
