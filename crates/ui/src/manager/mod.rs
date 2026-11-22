//! The FHIR server manager console (design `docs/design/fhir-server-manager.md`
//! §10, package P5).
//!
//! With a [`ManagerRuntime`] installed ([`set_manager`], called by the `hfs`
//! wiring when `HFS_MANAGER=on`), the console becomes a manager of many FHIR
//! endpoints:
//!
//! * `/ui` — overview of every endpoint ([`pages::overview`]);
//! * `/ui/endpoints/new` — create an endpoint;
//! * `/ui/e/{endpoint}` — one endpoint's home, and its configuration pages
//!   (`config`, `security`, `validation`, `terminology`), `sync`, `audit`,
//!   `history`, `duplicate`;
//! * `/ui/e/{endpoint}/data/…` — every pre-existing data page (resources,
//!   editor, batch, `$validate`, bulk, SQL on FHIR, conformance…), scoped to
//!   the endpoint by [`scope`], which rewrites the address onto the
//!   unchanged handler and stamps the endpoint's tenant, FHIR release and
//!   API base on the request;
//! * `/ui/about` — product, version and attribution.
//!
//! Old addresses (`/ui/resources`, …) answer `302` to the endpoint the
//! visitor came from, else the default endpoint.
//!
//! Without a runtime the console renders exactly as before (D-18).

pub mod backend;
pub(crate) mod forms;
pub mod nav;
pub(crate) mod pages;
pub(crate) mod scope;
pub(crate) mod view;

use std::sync::{Arc, RwLock};

use helios_endpoints::{
    ConsolePrincipal, ConsoleRole, Endpoint, EndpointRegistry, EndpointSnapshot, EndpointSpec,
    FhirRelease,
};

pub use backend::{
    BackendError, DuplicateOutcome, EndpointStats, HealthState, ManagerBackend, ProbeLine,
    SecretChange, SecretChanges, ServerHealth, SnapshotBackend, StoreBackend, SyncJobView,
    probe_terminology,
};
pub use scope::EndpointScope;

/// The P0 contracts the manager speaks, re-exported so a host wiring the
/// console (the `hfs` binary) needs no direct dependency.
pub use helios_endpoints;

/// How browser and self-calls reach a named endpoint's FHIR API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EndpointRouting {
    /// The P1 dispatcher serves `{HFS_BASE_URL}/{path}/…` (`HFS_MANAGER=on`
    /// with package P1): calls go to the endpoint's own base.
    #[default]
    Dispatcher,
    /// No dispatcher: calls go to the server base with `X-Tenant-ID` set to
    /// the endpoint's tenant (development and the pre-P1 integration).
    TenantHeader,
}

/// Everything the manager console needs, installed once at startup.
#[derive(Clone)]
pub struct ManagerRuntime {
    /// Data port.
    pub backend: Arc<dyn ManagerBackend>,
    /// Live routing snapshot: resolves `/ui/e/{endpoint}` synchronously.
    pub registry: Arc<EndpointRegistry>,
    /// How endpoint APIs are addressed.
    pub routing: EndpointRouting,
}

impl ManagerRuntime {
    /// A runtime over a backend and the registry it keeps current.
    pub fn new(backend: Arc<dyn ManagerBackend>, registry: Arc<EndpointRegistry>) -> Self {
        Self {
            backend,
            registry,
            routing: EndpointRouting::Dispatcher,
        }
    }

    /// Selects how endpoint APIs are addressed.
    pub fn with_routing(mut self, routing: EndpointRouting) -> Self {
        self.routing = routing;
        self
    }

    /// The read-only "default endpoint only" runtime of a deployment without
    /// an endpoint store.
    pub fn default_only(default_tenant: &str, fhir_version: FhirRelease) -> Self {
        let endpoint = Endpoint {
            id: "default".into(),
            revision: 1,
            spec: EndpointSpec::default_endpoint(default_tenant, fhir_version),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            created_by: None,
            updated_by: None,
        };
        let snapshot =
            EndpointSnapshot::new(1, vec![endpoint]).expect("the default endpoint alone is valid");
        let registry = Arc::new(EndpointRegistry::new(snapshot));
        let backend = Arc::new(SnapshotBackend::new(registry.snapshot()));
        Self::new(backend, registry).with_routing(EndpointRouting::TenantHeader)
    }
}

static MANAGER: RwLock<Option<ManagerRuntime>> = RwLock::new(None);

/// Installs the manager console. Called once by the server before the UI is
/// mounted (the mount captures it); the most recent call wins.
pub fn set_manager(runtime: ManagerRuntime) {
    *MANAGER.write().unwrap_or_else(|p| p.into_inner()) = Some(runtime);
}

/// Removes an installed manager (tests).
#[doc(hidden)]
pub fn clear_manager() {
    *MANAGER.write().unwrap_or_else(|p| p.into_inner()) = None;
}

pub(crate) fn installed() -> Option<Arc<ManagerRuntime>> {
    MANAGER
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .map(Arc::new)
}

/// The console role of a request: the P3 [`ConsolePrincipal`] when the
/// console guard stamped one; otherwise (until P3 lands, and in `none` mode)
/// `admin` when no administrator password gate is configured or an
/// administrator is signed in, else `viewer`.
pub(crate) fn role_of(
    parts: &axum::http::Extensions,
    gate_open: bool,
    admin_signed_in: bool,
    endpoint: Option<&Endpoint>,
) -> ConsoleRole {
    if let Some(principal) = parts.get::<ConsolePrincipal>() {
        return match endpoint {
            Some(e) => principal.role_for(e.name()),
            None => principal.role,
        };
    }
    if gate_open || admin_signed_in {
        ConsoleRole::Admin
    } else {
        ConsoleRole::Viewer
    }
}

/// The console session's CSRF synchronizer token (design §7.3): P3's type,
/// stamped by the console gate, which accepts it as `X-CSRF-Token` or form
/// field `_csrf`. The layout emits `<meta name="csrf-token">`, htmx sends it
/// as `X-CSRF-Token` and every manager form carries it as `_csrf`.
pub use helios_admin_auth::console::guard::CsrfToken;

/// An endpoint as the sidebar's switcher shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EndpointChip {
    pub name: String,
    pub display: String,
    pub initials: String,
    pub tone: &'static str,
    pub status_key: &'static str,
    pub href: String,
    /// The endpoint's URL path under the FHIR base (`/` for the default).
    pub path: String,
    pub serving: bool,
}

impl EndpointChip {
    fn new(e: &Endpoint) -> Self {
        let badge = view::status_badge(e.spec.status);
        let display = e.spec.display_name.clone();
        let initials: String = display
            .split([' ', '-', '_'])
            .filter(|w| !w.is_empty())
            .take(2)
            .filter_map(|w| w.chars().next())
            .collect::<String>()
            .to_uppercase();
        Self {
            name: e.name().to_string(),
            initials: if initials.is_empty() {
                "?".into()
            } else {
                initials
            },
            display,
            tone: badge.tone,
            status_key: badge.key,
            href: format!("/ui/e/{}", e.name()),
            path: if e.spec.path.is_root() {
                "/".to_string()
            } else {
                format!("/{}", e.spec.path.as_str())
            },
            serving: e.spec.status.is_serving(),
        }
    }
}

/// What the layout needs from the manager on every page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagerChrome {
    /// Sidebar sections.
    pub nav: Vec<nav::NavGroup>,
    /// The endpoint in scope.
    pub endpoint: Option<EndpointChip>,
    /// Every endpoint, for the switcher.
    pub endpoints: Vec<EndpointChip>,
    /// The requester's role.
    pub role: ConsoleRole,
}

impl ManagerChrome {
    pub(crate) fn new(
        runtime: &ManagerRuntime,
        scope: Option<&EndpointScope>,
        role: ConsoleRole,
    ) -> Self {
        let snapshot = runtime.registry.snapshot();
        Self {
            nav: nav::build(scope.map(|s| s.name()), role),
            endpoint: scope.map(|s| EndpointChip::new(&s.endpoint)),
            endpoints: snapshot
                .endpoints()
                .iter()
                .map(|e| EndpointChip::new(e))
                .collect(),
            role,
        }
    }
}
