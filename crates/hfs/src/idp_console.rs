//! Console pages and admin API of the built-in authorization server in the
//! `hfs` binary (docs/design/authorization-server.md I6/I7).
//!
//! * [`install`] — called by `idp::attach` once the runtime exists
//!   (`HFS_IDP=on`): installs the console service (`/ui/idp/**`, the
//!   wizard, management pages, diagnostic) and its sidebar section;
//! * [`admin_routes`] — `/admin/idp/**`, mounted inside the admin guard
//!   (console admin session + CSRF, the administrator credential, or an
//!   admin-scoped token, via the endpoint admin API's authorizer).
//!
//! | variable | default | meaning |
//! |---|---|---|
//! | `HFS_IDP_DIAGNOSTIC_VIA` | unset | send the diagnostic's requests for the public origin to this in-network origin instead (Host/X-Forwarded-* keep the public names) |
//! | `HFS_IDP_DIAGNOSTIC_TIMEOUT_SECS` | `10` | per-request timeout of the diagnostic |

use std::sync::{Arc, OnceLock};

use axum::Router;
use helios_endpoints::EndpointStore;
use helios_idp::runtime::IdpRuntime;
use helios_idp_admin::api::{self, AuthResult};
use helios_idp_admin::binding::StoreBinder;
use helios_idp_admin::service::{self, IdpConsole};
use helios_rest::handlers::admin_endpoints::authz::{
    AdminAuthorizer, AdminCredentialAuthorizer, AdminRequest, ConsoleAdminAuthorizer,
};
use tracing::info;

/// The endpoint store, recorded by `manager::attach` (endpoint binding).
pub static ENDPOINT_STORE: OnceLock<Arc<dyn EndpointStore>> = OnceLock::new();

/// Installs the console service over the runtime.
pub fn install(rt: &Arc<IdpRuntime>, server_base: &str) {
    let rt2 = rt.clone();
    let binder = ENDPOINT_STORE.get().map(|store| {
        Arc::new(StoreBinder {
            store: store.clone(),
            server_base: server_base.to_string(),
            public_base: rt.public_base().clone(),
            jwks_via: None,
            realms: Arc::new(move || rt2.realms().iter().map(|r| r.spec.clone()).collect()),
        }) as Arc<dyn helios_idp_admin::binding::EndpointBinder>
    });
    let via = std::env::var("HFS_IDP_DIAGNOSTIC_VIA")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .and_then(|v| url::Url::parse(v.trim()).ok());
    let mut console = IdpConsole::new(helios_idp::admin::IdpAdmin::new(rt.clone()), binder)
        .with_fetch_via(via)
        .with_type_provider(Some(Arc::new(resource_types)));
    if let Some(secs) = std::env::var("HFS_IDP_DIAGNOSTIC_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        console.timeout = std::time::Duration::from_secs(secs.clamp(1, 120));
    }
    service::install(Arc::new(console));
    #[cfg(feature = "ui")]
    helios_ui::idp::register_nav();
    info!(
        endpoint_binding = ENDPOINT_STORE.get().is_some(),
        "authorization server console on (/ui/idp, /admin/idp)"
    );
}

/// The resource types an endpoint of `release` serves (the generated
/// models of this build; its CapabilityStatement lists the same), for the
/// SMART scope builder. A release this build lacks serves nothing.
fn resource_types(release: &str) -> Vec<String> {
    use helios_fhir::FhirResourceTypeProvider;
    let names: Vec<&'static str> = match release {
        #[cfg(feature = "R4")]
        "R4" => <helios_fhir::r4::Resource as FhirResourceTypeProvider>::get_resource_type_names(),
        #[cfg(feature = "R4B")]
        "R4B" => {
            <helios_fhir::r4b::Resource as FhirResourceTypeProvider>::get_resource_type_names()
        }
        #[cfg(feature = "R5")]
        "R5" => <helios_fhir::r5::Resource as FhirResourceTypeProvider>::get_resource_type_names(),
        #[cfg(feature = "R6")]
        "R6" => <helios_fhir::r6::Resource as FhirResourceTypeProvider>::get_resource_type_names(),
        _ => vec![],
    };
    names.into_iter().map(str::to_string).collect()
}

/// `/admin/idp/**` (answers 503 until [`install`] ran).
pub fn admin_routes(app: Router, gate: helios_admin_auth::AdminGate) -> Router {
    if !crate::idp::enabled() {
        return app;
    }
    let authorizer: Arc<dyn AdminAuthorizer> = Arc::new(ConsoleAdminAuthorizer::new(
        None,
        AdminCredentialAuthorizer::from_env(Some(gate)),
    ));
    let auth: api::Authorize = Arc::new(move |parts| {
        let authorizer = authorizer.clone();
        Box::pin(async move {
            let peer = parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|c| c.0.ip());
            let req = AdminRequest {
                method: &parts.method,
                path: parts.uri.path(),
                headers: &parts.headers,
                extensions: &parts.extensions,
                peer,
            };
            let r: AuthResult = authorizer.authorize(req).await.map_err(|e| {
                (
                    e.status,
                    e.issues
                        .first()
                        .map(|i| i.message.clone())
                        .unwrap_or_default(),
                )
            });
            r
        })
    });
    app.merge(api::router(Some(auth)))
}
