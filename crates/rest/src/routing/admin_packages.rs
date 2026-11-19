//! Router for the FHIR package (IG) admin API (`/admin/packages`).
//!
//! Merged into the console **admin** tier by `build_app`, next to
//! `/admin/tenants`: bearer authentication plus a system-context scope when
//! auth is on. On top of that every route but `config` requires
//! `HFS_ADMIN_UI=true` (see [`crate::handlers::admin_packages`]).

use std::sync::Arc;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{get, post},
};

use crate::handlers::admin_packages as h;
use crate::packages::PackageManager;

/// Package-administration routes. `max_upload` bounds the `.tgz` upload
/// body (`HFS_PACKAGE_MAX_SIZE`) independently of `HFS_MAX_BODY_SIZE`.
pub fn routes(manager: Arc<PackageManager>, max_upload: usize) -> Router {
    Router::new()
        .route(
            "/admin/packages",
            get(h::list_handler)
                .merge(post(h::create_handler).layer(DefaultBodyLimit::max(max_upload))),
        )
        .route("/admin/packages/config", get(h::config_handler))
        .route("/admin/packages/cache", get(h::cache_handler))
        .route("/admin/packages/jobs", get(h::jobs_handler))
        .route(
            "/admin/packages/jobs/{id}",
            get(h::job_handler).delete(h::cancel_handler),
        )
        .route("/admin/packages/registry/search", get(h::search_handler))
        .route(
            "/admin/packages/registry/marketplace",
            get(h::marketplace_handler),
        )
        .route(
            "/admin/packages/registry/versions/{name}",
            get(h::versions_handler),
        )
        .route(
            "/admin/packages/{name}/{version}",
            get(h::detail_handler).delete(h::delete_handler),
        )
        .with_state(manager)
}
