//! SMART on FHIR discovery endpoint.
//!
//! Serves the `/.well-known/smart-configuration` document — per FHIR endpoint
//! when the FHIR server manager is on (`docs/design/fhir-server-manager.md`
//! §5.2, §6): an endpoint with its own auth section describes its own IdP
//! (configured SMART endpoints, the rest discovered from its issuer) and
//! answers `404` when that section turns auth off; an endpoint that inherits
//! the environment (and every request without the manager) gets the
//! environment's document exactly as before.

use axum::{
    Extension, Json,
    extract::State,
    response::{IntoResponse, Response},
};
use helios_auth::SmartConfiguration;
use helios_persistence::core::ResourceStorage;
use std::sync::Arc;

use crate::endpoint::EffectiveAuth;
use crate::state::AppState;

/// Handler for `GET /.well-known/smart-configuration`.
///
/// Returns a SMART configuration document built from the auth configuration
/// in force for the endpoint. If auth is not configured in the environment,
/// returns a minimal document (unchanged behaviour).
pub async fn smart_configuration_handler<S>(
    State(state): State<AppState<S>>,
    effective: Option<Extension<Arc<EffectiveAuth>>>,
) -> Response
where
    S: ResourceStorage + Send + Sync,
{
    if let Some(Extension(effective)) = effective
        && !effective.inherited
    {
        return match effective.smart_configuration().await {
            Some(doc) => Json(doc.to_json()).into_response(),
            None => crate::endpoint::auth::smart_discovery_not_found(effective.endpoint_name()),
        };
    }
    let config = state.auth_config();
    let smart = SmartConfiguration::from_config(config);
    Json(smart.to_json()).into_response()
}
