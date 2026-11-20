//! Router for the endpoint admin API (`/admin/endpoints`, `/admin/secrets`).
//!
//! Every route sits behind [`authorize`], which asks the state's
//! [`AdminAuthorizer`](crate::handlers::admin_endpoints::authz::AdminAuthorizer)
//! (admin credential / admin token today, console roles once P3 lands) and
//! inserts the acting [`Actor`] for the handlers. Refused and failed writes
//! are audited there too. [`ROUTES`] lists every `(method, path)` the router
//! serves; the `admin_endpoints_openapi` test checks it against
//! `docs/api/admin-endpoints.yaml` and against the router itself.

use axum::{
    Router,
    extract::{ConnectInfo, DefaultBodyLimit, Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use helios_audit::AuditAction;
use helios_endpoints::Actor;

use std::sync::Arc;

use helios_persistence::core::{ResourceStorage, SearchProvider};

use crate::handlers::admin_endpoints::audit::AdminEvent;
use crate::handlers::admin_endpoints::authz::AdminRequest;
use crate::handlers::admin_endpoints::{AdminEndpointsState, endpoints as h, packages as p};
use crate::packages::PackageManager;
use crate::state::AppState;

/// Every route of this router: `(method, path template)`.
pub const ROUTES: &[(&str, &str)] = &[
    ("GET", "/admin/endpoints"),
    ("POST", "/admin/endpoints"),
    ("GET", "/admin/endpoints/{name}"),
    ("PUT", "/admin/endpoints/{name}"),
    ("PATCH", "/admin/endpoints/{name}"),
    ("DELETE", "/admin/endpoints/{name}"),
    ("POST", "/admin/endpoints/{name}/$enable"),
    ("POST", "/admin/endpoints/{name}/$disable"),
    ("GET", "/admin/endpoints/{name}/history"),
    ("POST", "/admin/endpoints/{name}/$duplicate"),
    ("POST", "/admin/endpoints/{name}/$test-auth"),
    ("GET", "/admin/endpoints/{name}/stats"),
    ("GET", "/admin/endpoints/{name}/audit"),
    ("GET", "/admin/endpoints/{name}/packages"),
    ("POST", "/admin/endpoints/{name}/packages"),
    (
        "GET",
        "/admin/endpoints/{name}/packages/{package}/{version}",
    ),
    (
        "DELETE",
        "/admin/endpoints/{name}/packages/{package}/{version}",
    ),
    ("POST", "/admin/secrets/$rewrap"),
];

/// The endpoint admin routes of the application (`build_app`): mounted only
/// when the FHIR server manager is on (`HFS_MANAGER=on`, the app state holds
/// the endpoint runtime); an empty router otherwise. Key ring, outbound
/// policy and authorizer come from the environment
/// ([`AdminEndpointsStateBuilder::env_defaults`](crate::handlers::admin_endpoints::AdminEndpointsStateBuilder::env_defaults));
/// the administrator credential reaches the authorizer through the server's
/// admin guard (`AdminUser`) or the admin tier's auth layer (`Principal`).
pub fn app_routes<S>(
    state: &AppState<S>,
    packages: &Arc<PackageManager>,
    package_upload_limit: usize,
) -> Router
where
    S: ResourceStorage + SearchProvider + Send + Sync + 'static,
{
    let Some(runtime) = state.endpoint_runtime() else {
        return Router::new();
    };
    let admin =
        AdminEndpointsState::builder(Arc::clone(&runtime.store), Arc::clone(&runtime.registry))
            .env_defaults(None)
            .packages(Arc::clone(packages), package_upload_limit)
            .storage(
                state.storage_arc(),
                Some(Arc::clone(state.write_observer())),
            )
            .audit(
                state.audit_sink().cloned(),
                state.audit_source_observer(),
                state.default_tenant(),
            )
            .server_base(state.base_url())
            .build();
    routes(admin)
}

/// The endpoint admin routes over `state`.
pub fn routes(state: AdminEndpointsState) -> Router {
    let upload = state.package_upload_limit();
    Router::new()
        .route(
            "/admin/endpoints",
            get(h::list_handler).post(h::create_handler),
        )
        .route(
            "/admin/endpoints/{name}",
            get(h::get_handler)
                .put(h::put_handler)
                .patch(h::patch_handler)
                .delete(h::delete_handler),
        )
        .route("/admin/endpoints/{name}/$enable", post(h::enable_handler))
        .route("/admin/endpoints/{name}/$disable", post(h::disable_handler))
        .route("/admin/endpoints/{name}/history", get(h::history_handler))
        .route(
            "/admin/endpoints/{name}/$duplicate",
            post(h::duplicate_handler),
        )
        .route(
            "/admin/endpoints/{name}/$test-auth",
            post(h::test_auth_handler),
        )
        .route("/admin/endpoints/{name}/stats", get(h::stats_handler))
        .route("/admin/endpoints/{name}/audit", get(h::audit_handler))
        .route(
            "/admin/endpoints/{name}/packages",
            get(p::list_handler)
                .merge(post(p::install_handler).layer(DefaultBodyLimit::max(upload))),
        )
        .route(
            "/admin/endpoints/{name}/packages/{package}/{version}",
            get(p::detail_handler).delete(p::uninstall_handler),
        )
        .route("/admin/secrets/$rewrap", post(h::rewrap_handler))
        // `route_layer`: only matched routes are authorized, so an unknown
        // path still falls through to the app's 404.
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            authorize,
        ))
        .with_state(state)
}

/// Authorizes the request, inserts the [`Actor`], and audits refused or
/// failed writes.
pub async fn authorize(
    State(state): State<AdminEndpointsState>,
    mut request: Request,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip());
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let decision = {
        let req = AdminRequest {
            method: &method,
            path: &path,
            headers: request.headers(),
            extensions: request.extensions(),
            peer,
        };
        let write = req.is_write();
        let endpoint = req.endpoint().map(str::to_string);
        (
            state.inner().authorizer.authorize(req).await,
            write,
            endpoint,
        )
    };
    let (result, write, endpoint) = decision;
    let actor = match result {
        Ok(actor) => actor,
        Err(e) => {
            if write {
                let anonymous = Actor {
                    subject: "unauthenticated".into(),
                    display: None,
                    via: "none".into(),
                };
                state
                    .inner()
                    .audit
                    .record(
                        &anonymous,
                        AdminEvent {
                            action: action_of(&method),
                            outcome: "4",
                            endpoint: endpoint.as_deref(),
                            description: format!("{method} {path} refused: {e}"),
                            details: vec![("change", "refused".into())],
                        },
                    )
                    .await;
            }
            return e.into_response();
        }
    };
    request.extensions_mut().insert(actor.clone());
    let response = next.run(request).await;
    let status = response.status();
    if write && (status.is_client_error() || status.is_server_error()) {
        state
            .inner()
            .audit
            .record(
                &actor,
                AdminEvent {
                    action: action_of(&method),
                    outcome: if status.is_server_error() { "8" } else { "4" },
                    endpoint: endpoint.as_deref(),
                    description: format!("{method} {path} failed with {}", status.as_u16()),
                    details: vec![("status", status.as_u16().to_string())],
                },
            )
            .await;
    }
    response
}

fn action_of(method: &axum::http::Method) -> AuditAction {
    match *method {
        axum::http::Method::POST => AuditAction::Create,
        axum::http::Method::DELETE => AuditAction::Delete,
        axum::http::Method::GET | axum::http::Method::HEAD => AuditAction::Read,
        _ => AuditAction::Update,
    }
}
