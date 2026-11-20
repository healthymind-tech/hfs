//! Router of the sync admin API (`/admin/sync/**`, FHIR server manager P6)
//! and the public notification receiver (`/sync/notify/{job}`).
//!
//! `build_app` merges [`routes`] into the admin tier (system-scope token /
//! administrator sign-in, like `/admin/tenants`) and [`notify_routes`]
//! outside it: a FHIR server delivering rest-hook notifications has no
//! admin credential, it proves itself with the job's shared secret.

use axum::{
    Router,
    routing::{get, post},
};

use crate::handlers::admin_sync::{self as h, SyncApi};

/// Admin routes over the process-global sync service.
pub fn routes() -> Router {
    routes_with(SyncApi::Global)
}

/// Admin routes over a given service (tests).
pub fn routes_with(api: SyncApi) -> Router {
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
        .with_state(api)
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
