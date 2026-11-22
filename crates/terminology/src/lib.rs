//! # helios-terminology — unified server foundation
//!
//! Package 0 of `docs/design/unified-server.md`: HFS and HTS become one
//! process, one database, one console and one sign-in. This crate holds the
//! contracts every other package codes against:
//!
//! * [`service::TerminologyService`] — the one way HFS subsystems
//!   (validation, `:in` search, SQL-on-FHIR, FHIRPath, package manager,
//!   console) reach terminology; implemented in-process
//!   ([`loopback::LoopbackTerminology`]) and remotely
//!   ([`remote::RemoteTerminology`]), or switched off
//!   ([`service::DisabledTerminology`]);
//! * [`config`] — `HFS_TERMINOLOGY_*` settings, the `HTS_*` alias table and
//!   deprecation reporting;
//! * [`schema`] — the one-database plan (schema `terminology`, search path,
//!   fork-migration block, SQLite sibling file);
//! * [`access`] — console role rules for terminology pages and writes;
//! * [`routes`] — the URL namespace (`/tx`, `/admin/terminology`,
//!   `/ui/terminology`, reserved `/idp`) and `/ui/hts` redirects.
//!
//! No axum, no helios-hts, no FHIR models: helios-rest, helios-ui,
//! helios-hts-ui and the hfs wiring all depend on it.

pub mod access;
pub mod config;
pub mod loopback;
pub mod remote;
pub mod routes;
pub mod schema;
pub mod service;

pub use config::{EnvView, TerminologyMode, TerminologySettings};
pub use service::{
    DisabledTerminology, ServiceKind, TerminologyHealth, TerminologyService, TxError, TxPrincipal,
    TxRequest, TxResponse,
};
