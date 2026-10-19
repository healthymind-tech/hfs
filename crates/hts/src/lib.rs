//! # Helios Terminology Server (HTS)
//!
//! HTS is a FHIR R4 Terminology Server implementing the operations defined in
//! the FHIR specification for CodeSystem, ValueSet, and ConceptMap resources.
//! It runs as a standalone HTTP server and can optionally be integrated with
//! the main Helios FHIR Server (`hfs`) to power `$expand`-based search and
//! FHIRPath `memberOf()` / `subsumes()` calls.
//!
//! ## Core operations
//!
//! | Operation | Endpoint |
//! |-----------|----------|
//! | `$lookup` | `POST /CodeSystem/$lookup` |
//! | `$validate-code` | `POST /CodeSystem/$validate-code`, `POST /ValueSet/$validate-code` |
//! | `$subsumes` | `POST /CodeSystem/$subsumes` |
//! | `$expand` | `POST /ValueSet/$expand` |
//! | `$translate` | `POST /ConceptMap/$translate` |
//! | `$closure` | `POST /ConceptMap/$closure` |
//! | Bundle import | `POST /import` |
//! | CRUD | `GET/POST/PUT/DELETE /CodeSystem`, `/ValueSet`, `/ConceptMap` |
//!
//! ## Key types
//!
//! - [`state::AppState`] — shared application state injected into every handler.
//! - [`traits::TerminologyBackend`] — supertrait implemented by all backends.
//! - [`backends::SqliteTerminologyBackend`] — SQLite backend (default).
//! - [`error::HtsError`] — unified error type with `IntoResponse`.
//! - [`server::create_app`] — assembles the Axum router with all routes.
//!
//! ## Library vs binary
//!
//! This crate is both a library and a binary.  All modules are `pub` so that
//! integration tests can construct a test [`AppState`] and router without
//! replicating the binary's bootstrap logic.
//!
//! [`AppState`]: state::AppState
//! [`create_app`]: server::create_app

/// Run the operation `$f(&state, params)` against the request-scoped
/// `tx-resource` overlay when the request needs one (see [`tx_overlay`]),
/// else against `$state`.
macro_rules! dispatch_tx {
    ($state:expr, $params:expr, $f:path) => {{
        #[cfg(feature = "sqlite")]
        {
            match $crate::tx_overlay::overlay_for(&$state, &$params).await {
                Some(overlay) => $f(&*overlay, $params).await,
                None => $f(&$state, $params).await,
            }
        }
        #[cfg(not(feature = "sqlite"))]
        {
            $f(&$state, $params).await
        }
    }};
}

pub mod admin;
pub mod backends;
pub(crate) mod bcp13;
pub mod config;
pub mod ecl;
pub mod error;
pub(crate) mod implicit_vs;
pub mod import;
pub(crate) mod language;
pub mod loinc_filters;
#[cfg(all(feature = "sqlite", feature = "postgres"))]
pub mod migrate;
pub(crate) mod mimetypes;
pub mod operations;
pub(crate) mod resource_ids;
pub mod server;
pub mod state;
pub mod string_search;
pub mod traits;
#[cfg(feature = "sqlite")]
pub mod tx_overlay;
pub mod types;
pub(crate) mod ucum;
pub(crate) mod ucum_validate;
pub mod version_order;
pub mod write_guard;
