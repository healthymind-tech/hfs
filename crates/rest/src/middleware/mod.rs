//! HTTP middleware for the FHIR REST API.
//!
//! This module contains Axum middleware components:
//!
//! - [`tenant`] - Tenant identification and extraction
//! - [`tenant_prefix`] - URL prefix stripping for tenant-in-URL routing
//! - [`content_type`] - Content negotiation
//! - [`conditional`] - Conditional request headers (If-Match, etc.)
//! - [`prefer`] - Prefer header handling
//! - [`resource_type`] - Refuses paths whose type segment is not a resource type
//! - [`smart_access`] - Patient-compartment, granular-scope and bundle filtering for SMART tokens

pub mod audit;
pub mod auth;
pub mod client_rate_limit;
pub mod conditional;
pub mod content_type;
pub mod prefer;
pub mod resource_type;
pub mod smart_access;
pub mod tenant;
pub mod tenant_prefix;

pub use conditional::ConditionalHeaders;
pub use prefer::PreferHeader;
pub use tenant_prefix::{ExtractedTenantFromUrl, OriginalPath};
