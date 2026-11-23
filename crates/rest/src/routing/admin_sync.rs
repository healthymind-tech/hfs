//! Router of the sync admin API (`/admin/sync/**`, FHIR server manager P6)
//! and the public notification receiver (`/sync/notify/{job}`).
//!
//! `build_app` merges [`routes`] into the admin tier (system-scope token /
//! administrator sign-in, like `/admin/tenants`) and [`notify_routes`]
//! outside it: a FHIR server delivering rest-hook notifications has no
//! admin credential, it proves itself with the job's shared secret.
//!
//! The admin routes additionally require P2's [`AdminTokenScope`]
//! (`HFS_ADMIN_TOKEN_SCOPE`, default `system/*.cruds`) of a JWT principal;
//! the administrator credential and requests without a principal (auth off,
//! the admin guard decides) pass.

use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post},
};

use crate::handlers::admin_sync::{self as h, SyncApi};
use crate::middleware::auth::{AdminTokenScope, admin_token_scope_middleware};

/// Admin routes over the process-global sync service.
pub fn routes() -> Router {
    routes_with(SyncApi::Global)
}

/// [`routes`], and installs the server's write path for local sync targets
/// (`EndpointWriteHooks` over this app's validation service and endpoint
/// registry) into the process-global sync service.
pub fn routes_for<S>(state: &crate::state::AppState<S>) -> Router
where
    S: helios_persistence::core::ResourceStorage + Send + Sync + 'static,
{
    if let (Some(svc), Some(runtime)) = (helios_sync::global(), state.endpoint_runtime()) {
        svc.set_local_write_hooks(Arc::new(h::EndpointWriteHooks::new(
            state.validation_shared(),
            Arc::clone(&runtime.registry),
            state.config().default_fhir_version,
        )));
    }
    routes()
}

/// Admin routes over a given service (tests).
pub fn routes_with(api: SyncApi) -> Router {
    routes_with_scope(api, AdminTokenScope::from_env())
}

/// Admin routes over a given service and admin token scope.
pub fn routes_with_scope(api: SyncApi, scope: AdminTokenScope) -> Router {
    Router::new()
        .route("/admin/sync/jobs", get(h::list_jobs).post(h::create_job))
        .route("/admin/sync/jobs/$test", post(h::test_connection))
        .route(
            "/admin/sync/jobs/{id}",
            get(h::get_job).put(h::put_job).delete(h::delete_job),
        )
        .route("/admin/sync/jobs/{id}/$run", post(h::run_job))
        .route("/admin/sync/jobs/{id}/$cancel", post(h::cancel_job))
        .route("/admin/sync/jobs/{id}/$retry", post(h::retry_job))
        .route("/admin/sync/jobs/{id}/runs", get(h::job_runs))
        .route("/admin/sync/runs/{run}", get(h::get_run))
        .route("/admin/sync/runs/{run}/log", get(h::run_log))
        .route("/admin/sync/runs/{run}/conflicts", get(h::run_conflicts))
        .route(
            "/admin/sync/runs/{run}/conflicts/{rtype}/{rid}/$resolve",
            post(h::resolve_conflict),
        )
        .route("/admin/sync/duplicates", post(h::duplicate))
        .route("/admin/sync/$rewrap", post(h::rewrap))
        .route("/admin/sync/$verify-peer", post(h::verify_peer))
        .with_state(api)
        // `route_layer`: only these routes (a `layer` would also wrap the
        // fallback this router carries into the app).
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::new(scope),
            admin_token_scope_middleware,
        ))
}

/// The notification receiver over the process-global service.
pub fn notify_routes() -> Router {
    notify_routes_with(SyncApi::Global)
}

/// The notification receiver over a given service (tests).
pub fn notify_routes_with(api: SyncApi) -> Router {
    Router::new()
        .route("/sync/notify/{id}", post(h::notify))
        .with_state(api)
}
