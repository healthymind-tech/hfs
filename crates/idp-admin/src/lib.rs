//! Admin API (`/admin/idp/**`) and the "test this auth server" diagnostic
//! of the built-in authorization server (docs/design/authorization-server.md
//! packages I6/I7).
//!
//! * [`diagnostic`] — configuration preflight + the ten live steps against
//!   a realm's public URLs, reported in the console's Verify-panel shape.

pub mod binding;
pub mod diagnostic;
