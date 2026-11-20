//! FHIR server manager runtime: many FHIR endpoints on one HFS (package P1 of
//! `docs/design/fhir-server-manager.md`).
//!
//! When an [`EndpointRuntime`] is handed to the app builder
//! ([`crate::OperationsBundle::endpoints`], set by the server only with
//! `HFS_MANAGER=on`), the FHIR stack is wrapped by the
//! [`dispatch`]er: every FHIR request is resolved against the registry's
//! current [`EndpointSnapshot`], a named endpoint's `/{path}` prefix is
//! stripped, and the [`ResolvedEndpoint`] is inserted as a request extension
//! (and made the task-local [`current`] endpoint for code that has no access
//! to the request, such as the validation service). Everything downstream —
//! tenant extractor (one door per tenant, D-7), public URLs, the
//! CapabilityStatement, the effective FHIR version, validation, search
//! terminology, CORS — reads it instead of global configuration.
//!
//! Without a runtime nothing here is installed and no request carries a
//! `ResolvedEndpoint`, so every reader falls back to today's behaviour.

pub mod auth;
pub mod base_url;
pub mod cors;
pub mod dispatch;
pub mod effective;

use std::future::Future;
use std::sync::Arc;

use helios_endpoints::{
    EndpointRegistry, EndpointSnapshot, EndpointStore, FhirRelease, ResolvedEndpoint,
};
use helios_fhir::FhirVersion;

pub use auth::{AuthBase, AuthHook, EffectiveAuth, auth_layer};
pub use base_url::{BaseUrlResolver, SnapshotBaseUrlResolver};
pub use effective::EffectiveConfig;

/// The process-wide endpoint registry and the store it reloads from.
#[derive(Clone)]
pub struct EndpointRuntime {
    /// Hot-swappable routing snapshot.
    pub registry: Arc<EndpointRegistry>,
    /// Where endpoint configuration is persisted (PostgreSQL in production,
    /// the memory store for `HFS_ENDPOINTS_FILE` on other backends and tests).
    pub store: Arc<dyn EndpointStore>,
    /// The endpoint admin service (package P4) the app builds for this
    /// runtime, shared with the manager console so both write through one
    /// state (audit sink, storage statistics, audit search). Filled by
    /// `build_app`; clones of the runtime share the slot.
    pub admin_api: AdminApiSlot,
}

/// Where `build_app` publishes the endpoint admin service it built.
pub type AdminApiSlot =
    Arc<std::sync::OnceLock<crate::handlers::admin_endpoints::AdminEndpointsState>>;

impl std::fmt::Debug for EndpointRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointRuntime")
            .field("registry", &self.registry)
            .finish_non_exhaustive()
    }
}

impl EndpointRuntime {
    /// A runtime over `registry` and `store`.
    pub fn new(registry: Arc<EndpointRegistry>, store: Arc<dyn EndpointStore>) -> Self {
        Self {
            registry,
            store,
            admin_api: AdminApiSlot::default(),
        }
    }

    /// Shares `slot` (the process's manager context keeps one, so every
    /// runtime it hands out publishes the admin service to the same place).
    pub fn with_admin_api_slot(mut self, slot: AdminApiSlot) -> Self {
        self.admin_api = slot;
        self
    }

    /// The endpoint admin service built by the app, once it is built.
    pub fn admin_api(&self) -> Option<&crate::handlers::admin_endpoints::AdminEndpointsState> {
        self.admin_api.get()
    }

    /// Reloads the registry from the store if the store moved (the admin API
    /// calls this before answering, so a writer reads its own write).
    pub async fn reload_if_stale(&self) -> Result<bool, helios_endpoints::ReloadError> {
        self.registry.reload_if_stale(self.store.as_ref()).await
    }
}

/// Request extension: the snapshot the request was resolved on, so every
/// layer of one request sees the same endpoint set (the tenant extractor uses
/// it for the one-door rule).
#[derive(Debug, Clone)]
pub struct RequestSnapshot(pub Arc<EndpointSnapshot>);

tokio::task_local! {
    static CURRENT: ResolvedEndpoint;
}

/// The endpoint of the request being processed on this task, if the request
/// came through the dispatcher.
pub fn current() -> Option<ResolvedEndpoint> {
    CURRENT.try_with(Clone::clone).ok()
}

/// Runs `f` on the current endpoint without cloning it.
pub fn with_current<R>(f: impl FnOnce(&ResolvedEndpoint) -> R) -> Option<R> {
    CURRENT.try_with(f).ok()
}

/// Runs `fut` with `endpoint` as the [`current`] endpoint.
pub async fn scope<F: Future>(endpoint: ResolvedEndpoint, fut: F) -> F::Output {
    CURRENT.scope(endpoint, fut).await
}

/// The `helios_fhir` version of an endpoint release, `None` when this binary
/// was built without it (the admin API refuses such endpoints; a stale row
/// falls back to the server default).
pub fn fhir_version_of(release: FhirRelease) -> Option<FhirVersion> {
    match release {
        #[cfg(feature = "R4")]
        FhirRelease::R4 => Some(FhirVersion::R4),
        #[cfg(feature = "R4B")]
        FhirRelease::R4B => Some(FhirVersion::R4B),
        #[cfg(feature = "R5")]
        FhirRelease::R5 => Some(FhirVersion::R5),
        #[cfg(feature = "R6")]
        FhirRelease::R6 => Some(FhirVersion::R6),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

/// The release of a `helios_fhir` version (the inverse of [`fhir_version_of`]).
pub fn release_of(version: FhirVersion) -> FhirRelease {
    match version {
        #[cfg(feature = "R4")]
        FhirVersion::R4 => FhirRelease::R4,
        #[cfg(feature = "R4B")]
        FhirVersion::R4B => FhirRelease::R4B,
        #[cfg(feature = "R5")]
        FhirVersion::R5 => FhirRelease::R5,
        #[cfg(feature = "R6")]
        FhirVersion::R6 => FhirRelease::R6,
        #[allow(unreachable_patterns)]
        _ => FhirRelease::R4,
    }
}

/// The effective default FHIR version of a request: its endpoint's release
/// when it came through the dispatcher (and the binary has it), else
/// `server_default`.
pub fn effective_fhir_version(
    endpoint: Option<&ResolvedEndpoint>,
    server_default: FhirVersion,
) -> FhirVersion {
    endpoint
        .and_then(|r| fhir_version_of(r.endpoint.spec.fhir_version))
        .unwrap_or(server_default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_endpoints::{Endpoint, EndpointSpec};

    fn resolved(spec: EndpointSpec) -> ResolvedEndpoint {
        let snap = EndpointSnapshot::new(
            1,
            vec![Endpoint {
                id: "e".into(),
                revision: 1,
                spec,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                created_by: None,
                updated_by: None,
            }],
        )
        .unwrap();
        match snap.resolve("/Patient") {
            helios_endpoints::Resolution::Serve(r) => r,
            other => panic!("{other:?}"),
        }
    }

    #[cfg(feature = "R4")]
    #[test]
    fn versions_round_trip() {
        assert_eq!(fhir_version_of(FhirRelease::R4), Some(FhirVersion::R4));
        assert_eq!(release_of(FhirVersion::R4), FhirRelease::R4);
        let r = resolved(EndpointSpec::default_endpoint("default", FhirRelease::R4));
        assert_eq!(
            effective_fhir_version(Some(&r), FhirVersion::R4),
            FhirVersion::R4
        );
        assert_eq!(
            effective_fhir_version(None, FhirVersion::R4),
            FhirVersion::R4
        );
    }

    #[tokio::test]
    async fn current_is_scoped() {
        assert!(current().is_none());
        let r = resolved(EndpointSpec::default_endpoint("default", FhirRelease::R4));
        let name = scope(r, async {
            current().map(|r| r.endpoint.name().to_string())
        })
        .await;
        assert_eq!(name.as_deref(), Some("default"));
        assert!(with_current(|r| r.prefix.clone()).is_none());
    }
}
