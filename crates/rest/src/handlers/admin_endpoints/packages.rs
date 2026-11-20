//! Endpoint-scoped package (IG) administration:
//! `/admin/endpoints/{name}/packages[/{package}/{version}]`.
//!
//! The same API as `/admin/packages` (design §5.2 "IG packages") with the
//! tenant fixed to the endpoint's — there is no `X-Tenant-ID` / `?tenant=`
//! here, so a package can never land in another endpoint's tenant. Job
//! progress stays at `/admin/packages/jobs/{id}` (the `Location` of a `202`).
//! `HFS_ADMIN_UI=true` is required, as for `/admin/packages`.
//!
//! Gap (tracked in the P4 report): HTS imports still use the server's
//! terminology server, not the endpoint's `terminology.serverUrl`.

use axum::{
    Extension,
    body::Bytes,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use helios_endpoints::Actor;
use serde::Deserialize;

use super::AdminEndpointsState;
use super::endpoints::load;
use super::error::{ApiError, ApiResult};
use crate::handlers::admin_packages as pk;
use crate::packages::PackageManager;

/// Query of the endpoint package routes.
#[derive(Debug, Default, Deserialize)]
pub struct PackageQuery {
    /// Hold the response until the job ends (≤ 60 s).
    #[serde(default)]
    wait: Option<u64>,
    /// Plan only (`POST`; default `true` for uploads, request body otherwise).
    #[serde(default, rename = "dryRun", alias = "dry_run")]
    dry_run: Option<bool>,
    /// Include the resource list (`GET …/{package}/{version}`, default true).
    #[serde(default)]
    resources: Option<bool>,
}

fn manager(state: &AdminEndpointsState) -> ApiResult<&std::sync::Arc<PackageManager>> {
    state.inner().packages.as_ref().ok_or_else(|| {
        ApiError::not_implemented("the package manager is not installed in this server")
    })
}

/// The endpoint's tenant and the package manager, with the admin gate.
async fn scope(
    state: &AdminEndpointsState,
    name: &str,
) -> Result<(std::sync::Arc<PackageManager>, String), Box<Response>> {
    let m = manager(state).map_err(|e| Box::new(e.into_response()))?;
    if let Some(r) = pk::gate(m) {
        return Err(Box::new(r));
    }
    let e = load(state, name)
        .await
        .map_err(|e| Box::new(e.into_response()))?;
    Ok((m.clone(), e.tenant_id().to_string()))
}

/// `GET /admin/endpoints/{name}/packages`
pub async fn list_handler(
    State(state): State<AdminEndpointsState>,
    Path(name): Path<String>,
) -> Response {
    match scope(&state, &name).await {
        Ok((m, tenant)) => pk::list_in(&m, &tenant).await,
        Err(r) => *r,
    }
}

/// `POST /admin/endpoints/{name}/packages` (JSON install request or `.tgz`)
pub async fn install_handler(
    State(state): State<AdminEndpointsState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    Query(q): Query<PackageQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match scope(&state, &name).await {
        Ok((m, tenant)) => {
            pk::install_in(
                &m,
                &tenant,
                &headers,
                q.dry_run,
                q.wait,
                Some(actor.subject),
                body,
            )
            .await
        }
        Err(r) => *r,
    }
}

/// `GET /admin/endpoints/{name}/packages/{package}/{version}`
pub async fn detail_handler(
    State(state): State<AdminEndpointsState>,
    Path((name, package, version)): Path<(String, String, String)>,
    Query(q): Query<PackageQuery>,
) -> Response {
    match scope(&state, &name).await {
        Ok((m, tenant)) => {
            pk::detail_in(&m, &tenant, &package, &version, q.resources.unwrap_or(true)).await
        }
        Err(r) => *r,
    }
}

/// `DELETE /admin/endpoints/{name}/packages/{package}/{version}`
pub async fn uninstall_handler(
    State(state): State<AdminEndpointsState>,
    Extension(actor): Extension<Actor>,
    Path((name, package, version)): Path<(String, String, String)>,
    Query(q): Query<PackageQuery>,
) -> Response {
    match scope(&state, &name).await {
        Ok((m, tenant)) => {
            pk::uninstall_in(&m, &tenant, &package, &version, q.wait, Some(actor.subject)).await
        }
        Err(r) => *r,
    }
}
