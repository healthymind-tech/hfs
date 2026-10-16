//! FHIR package (Implementation Guide) manager.
//!
//! Installs npm-style FHIR packages from the online registries
//! (packages.fhir.org, packages2.fhir.org or a mirror), from uploaded
//! `.tgz` archives, or from the local package cache; resolves their
//! dependencies; writes conformance resources into this server (registering
//! StructureDefinitions with the validator) and terminology into the
//! configured terminology server; records which package owns which
//! resource; and removes exactly what a package owns on uninstall.
//!
//! - [`tarball`] reads archives as untrusted data (path and size checks).
//! - [`version`] orders versions and matches dependency ranges.
//! - [`resolver`] builds the install plan.
//! - [`registry`] talks to the registries and the marketplace index, and
//!   owns the cache.
//! - [`hts`] is the terminology-server client.
//! - [`jobs`] is the background job table.
//! - [`manager`] ties it together; [`store`] is the file-backed registry for
//!   backends without registry tables.
//!
//! The operator guide is `docs/ops/ig-packages.md`.

pub mod hts;
pub mod jobs;
pub mod manager;
pub mod registry;
pub mod resolver;
pub mod store;
pub mod tarball;
pub mod version;

pub use jobs::{Job, JobKind, JobStatus, JobView};
pub use manager::{
    CoreTerminologyReport, InstallOptions, InstallRequest, ManagerDeps, ManagerError, PackageManager, TerminologyTarget,
};
pub use resolver::{InstallMode, NodeAction, Plan};
pub use version::DependencyPolicy;
