//! Built-in authorization server of the HealthyMind FHIR Server Manager
//! (docs/design/authorization-server.md).
//!
//! HFS can host any number of **realms** — independent OAuth 2.0 / OpenID
//! Connect / SMART App Launch 2.2 authorization servers — each at
//! `{public_base}/idp/{realm}`. A FHIR endpoint's auth section then names
//! either an external issuer (Keycloak, Entra, Epic, …) or an internal realm.
//!
//! This crate is package 0: protocol-independent core types and rules with
//! no HTTP and no database driver.
//!
//! | module | contents |
//! |---|---|
//! | [`realm`] | realm names, issuer URLs, features, lifetimes, policies |
//! | [`client`] | client registrations, auth methods, redirect URI rules |
//! | [`scope`] | scope grammar, grant intersection, refresh narrowing |
//! | [`pkce`] | RFC 7636 S256 |
//! | [`keys`] | signing keys, JWS, JWK thumbprints, sealing, rotation planner |
//! | [`token`] | RFC 9068 access token / id_token claims, opaque handles, refresh rotation |
//! | [`launch`] | SMART launch context and EHR launch handles |
//! | [`user`] | realm users and their FHIR resource links |
//! | [`access`] | a user's links as a flat list; copy/paste formats, import plans |
//! | [`metadata`] | OIDC / RFC 8414 metadata and SMART configuration |
//! | [`store`] | the store contract ([`memory`] in-memory, `pg` PostgreSQL) |
//! | [`schema`] | fork migration SQL (`hm-0020-idp-core`) |
//! | [`interaction`] | contract between `/authorize` and the hosted pages |
//! | [`runtime`] | hot per-instance snapshot (keys, JWKS, deny set) |
//! | [`admin`] | administrative service used by the console and `/admin/idp` |
//! | [`jws`] | JWS verification (client assertions, own tokens) |
//! | [`password`] | argon2id for user passwords and client secrets |
//! | [`logout`] | RP-initiated logout validation |
//!
//! Everything is inert unless `HFS_IDP=on` ([`FLAG_ENV`]).

pub mod access;
pub mod admin;
pub mod client;
pub mod error;
pub mod events;
pub mod interaction;
pub mod jws;
pub mod keys;
pub mod launch;
pub mod logout;
pub mod memory;
pub mod metadata;
pub mod password;
#[cfg(feature = "postgres")]
pub mod pg;
pub mod pkce;
pub mod realm;
pub mod runtime;
pub mod schema;
pub mod scope;
pub mod store;
pub mod token;
pub mod user;
pub mod util;

pub use error::{IdpError, OAuthError, OAuthErrorCode};
pub use realm::{IDP_PATH_PREFIX, RealmName, RealmSpec};

/// Feature flag: `HFS_IDP=on` mounts `/idp/*`, the console pages and the
/// internal verifier. Default off (schema is still created).
pub const FLAG_ENV: &str = "HFS_IDP";

/// Whether the flag value turns the IdP on (`on`, `true`, `1`, `yes`).
pub fn flag_enabled(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("on" | "true" | "1" | "yes")
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn flag() {
        assert!(super::flag_enabled(Some("on")));
        assert!(super::flag_enabled(Some(" TRUE ")));
        assert!(!super::flag_enabled(Some("off")));
        assert!(!super::flag_enabled(None));
    }
}
