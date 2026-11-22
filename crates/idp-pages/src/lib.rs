//! Hosted pages of the built-in authorization server
//! (docs/design/authorization-server.md, packages I4 and I8).
//!
//! Between `/idp/{realm}/authorize` (protocol, `crates/idp-server`) and the
//! redirect back to the app sit the interactive steps a person sees: sign
//! in, pick a patient, approve the requested access. This crate serves them
//! (`/idp/{realm}/login`, `/select-patient`, `/consent`, `/logout`,
//! `/account/apps`), with the console's design system, and the controls
//! around them: CSRF ([`csrf`]), CSP and clickjacking headers
//! ([`security`]), lockout, rate limits ([`limits`]) and BALP AuditEvents
//! ([`audit`]).

pub mod audit;
pub mod csrf;
pub mod limits;
pub mod security;
pub mod directory;
pub mod i18n;
pub mod lockout;
pub mod pages;
pub mod scopes;

pub use pages::{Pages, PagesConfig, router};
