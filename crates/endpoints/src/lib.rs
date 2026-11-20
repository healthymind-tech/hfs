//! # helios-endpoints — the FHIR server manager foundation
//!
//! Package 0 of `docs/design/fhir-server-manager.md`. It defines the shared
//! contracts the parallel implementation packages code against:
//!
//! | Module | Contract | Implemented / consumed by |
//! |---|---|---|
//! | [`name`] | endpoint name / path grammar, reserved segments | everyone |
//! | [`model`] | [`Endpoint`], [`EndpointSpec`] and its policy sections | P1 routing, P2 auth, P4 admin API, P5 UI |
//! | [`snapshot`] | [`EndpointSnapshot::resolve`], [`ResolvedEndpoint`] request extension | P1 dispatcher, every per-request consumer |
//! | [`registry`] | [`EndpointRegistry`] (ArcSwap + watch), [`run_watcher`] | P1 (wiring), P2 (verifier cache invalidation), P5 |
//! | [`store`] | [`EndpointStore`], [`MemoryEndpointStore`], conformance tests | P1 (PostgreSQL store), P4 |
//! | [`secret`] | [`SecretKeyRing`], [`SealedSecret`], [`SecretInput`] | P3, P4, P6 |
//! | [`console`] | console auth modes, roles, [`ConsoleSessionResolver`], [`ConsoleStore`] | P3 (impl), P2 + P5 (consumers) |
//! | [`sync`] | sync job / run model, [`SyncStore`] | P6 (impl), P4/P5 (API/UI) |
//! | [`outbound`] | [`OutboundPolicy`] SSRF guard | P2, P3, P6 |
//! | `pg` (feature `postgres`) | PostgreSQL [`EndpointStore`] + `LISTEN hfs_config` task | P1 |
//!
//! The crate deliberately has no HTTP framework, FHIR model or persistence
//! dependency so that every other crate can depend on it.

pub mod console;
pub mod model;
pub mod name;
pub mod outbound;
#[cfg(feature = "postgres")]
pub mod pg;
pub mod registry;
pub mod secret;
pub mod snapshot;
pub mod store;
pub mod sync;

pub use console::{
    CONSOLE_ISSUER, ConsoleAuthMode, ConsoleAuthSettings, ConsolePrincipal, ConsoleRole,
    ConsoleSessionResolver, ConsoleStore, ConsoleUser,
};
pub use model::{
    AuthMode, ConsoleAccess, CorsSettings, Endpoint, EndpointAuth, EndpointSpec, EndpointStatus,
    FhirRelease, ManagedBy, RateLimitSettings, StorageBinding, TerminologyBinding,
    ValidationErrors, ValidationMode, ValidationSettings,
};
pub use name::{DEFAULT_ENDPOINT_NAME, EndpointName, EndpointPath, RESERVED_SEGMENTS};
pub use outbound::OutboundPolicy;
pub use registry::{EndpointRegistry, ReloadError, run_watcher};
pub use secret::{SealedSecret, SecretInput, SecretKeyRing};
pub use snapshot::{EndpointSnapshot, Resolution, ResolvedEndpoint};
pub use store::{Actor, EndpointStore, MemoryEndpointStore, StoreError, ensure_default_endpoint};
pub use sync::{SyncJob, SyncJobSpec, SyncRun, SyncStore};
