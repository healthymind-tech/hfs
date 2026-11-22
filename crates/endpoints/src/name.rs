//! Endpoint names and URL path segments.
//!
//! A FHIR endpoint is addressed by one path segment under the server's FHIR
//! base: with `HFS_BASE_URL=https://host/fhir`, the endpoint whose path is
//! `twcore` serves `https://host/fhir/twcore/Patient/1`, and the server sees
//! `/twcore/Patient/1` (the reverse proxy strips `/fhir`). The default
//! endpoint has the empty path and keeps serving `https://host/fhir/...`.
//!
//! Names and paths share one grammar, `^[a-z][a-z0-9-]{0,62}$` without a
//! trailing `-`: lower case only, so a segment can never be a FHIR resource
//! type (those start upper case), an `_`-parameter (`_history`, `_search`), an
//! `$operation` or `.well-known`. Every such segment is also a valid tenant id
//! (`helios_persistence::tenant::TenantId::parse`), so a new endpoint can use
//! its name as its tenant.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Name of the default endpoint (the one with the empty path).
pub const DEFAULT_ENDPOINT_NAME: &str = "default";

/// Longest name or path segment, in bytes.
pub const MAX_SEGMENT_LEN: usize = 63;

/// Segments no endpoint may take as its path or name, because the server (or
/// the reference reverse proxy in front of it) already routes them. Lower
/// case; compared after the grammar check, which already refuses upper case.
///
/// Keep in step with `RESERVED_SYSTEM_PATHS` in
/// `crates/rest/src/middleware/tenant_prefix.rs` and the top-level routes the
/// `hfs` binary mounts (`/ui`, `/admin`, `/console`, `/metrics`, `/health`).
pub const RESERVED_SEGMENTS: &[&str] = &[
    // Mounted by HFS itself.
    "ui",
    "admin",
    "console",
    "metrics",
    "health",
    "metadata",
    "ws",
    "export",
    "export-status",
    "export-file",
    "bulk-submit-status",
    "bulk-submit-file",
    "api",
    "v1",
    "v2",
    "fhir",
    // Routed by the reference nginx / reserved for the manager.
    "auth",
    "tx",
    "static",
    "assets",
    "login",
    "logout",
    "callback",
    "oauth",
    "oidc",
    // Built-in authorization server realms (`/idp/{realm}`,
    // docs/design/authorization-server.md D-2).
    "idp",
    "well-known",
    "system",
    "internal",
    "manager",
    "endpoints",
    "sync",
];

/// Why a name or path segment was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SegmentError {
    /// Empty where a segment is required.
    #[error("must not be empty")]
    Empty,
    /// Longer than [`MAX_SEGMENT_LEN`].
    #[error("must be at most {MAX_SEGMENT_LEN} characters (got {0})")]
    TooLong(usize),
    /// Does not start with `a`-`z`.
    #[error("must start with a lower-case letter a-z")]
    BadStart,
    /// Ends with `-`.
    #[error("must not end with '-'")]
    BadEnd,
    /// A character outside `a-z 0-9 -`.
    #[error("may only contain a-z, 0-9 and '-' (found {0:?})")]
    BadChar(char),
    /// One of [`RESERVED_SEGMENTS`].
    #[error("'{0}' is reserved by the server")]
    Reserved(String),
}

/// Checks the shared name / path grammar, without the reserved list.
pub fn check_segment_grammar(s: &str) -> Result<(), SegmentError> {
    if s.is_empty() {
        return Err(SegmentError::Empty);
    }
    if s.len() > MAX_SEGMENT_LEN {
        return Err(SegmentError::TooLong(s.len()));
    }
    let first = s.chars().next().expect("non-empty");
    if !first.is_ascii_lowercase() {
        return Err(SegmentError::BadStart);
    }
    if let Some(c) = s
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
    {
        return Err(SegmentError::BadChar(c));
    }
    if s.ends_with('-') {
        return Err(SegmentError::BadEnd);
    }
    Ok(())
}

/// Checks a segment a client asks for: grammar plus the reserved list.
pub fn check_segment(s: &str) -> Result<(), SegmentError> {
    check_segment_grammar(s)?;
    if RESERVED_SEGMENTS.contains(&s) {
        return Err(SegmentError::Reserved(s.to_string()));
    }
    Ok(())
}

/// A validated endpoint name: the immutable identifier of an endpoint in the
/// admin API (`/admin/endpoints/{name}`), the console (`/ui/e/{name}`), logs,
/// metrics labels and audit records.
///
/// [`DEFAULT_ENDPOINT_NAME`] is valid only for the default endpoint; the
/// store enforces that (it is not in [`RESERVED_SEGMENTS`] because the default
/// endpoint itself carries it).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EndpointName(String);

impl EndpointName {
    /// Parses a name (grammar + reserved list).
    pub fn parse(s: &str) -> Result<Self, SegmentError> {
        check_segment(s)?;
        Ok(Self(s.to_string()))
    }

    /// The default endpoint's name.
    pub fn default_endpoint() -> Self {
        Self(DEFAULT_ENDPOINT_NAME.to_string())
    }

    /// Whether this is the default endpoint's name.
    pub fn is_default(&self) -> bool {
        self.0 == DEFAULT_ENDPOINT_NAME
    }

    /// The name as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EndpointName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for EndpointName {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for EndpointName {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        EndpointName::parse(&raw).map_err(|e| serde::de::Error::custom(format!("name: {e}")))
    }
}

/// The URL path segment an endpoint is served at, or the empty path of the
/// default endpoint. Unlike the name it may change (a rename of the public
/// URL); the routing snapshot picks the change up on its next reload.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EndpointPath(String);

impl EndpointPath {
    /// Parses a path segment (grammar + reserved list). The empty string is
    /// the root path, valid only for the default endpoint.
    pub fn parse(s: &str) -> Result<Self, SegmentError> {
        if s.is_empty() {
            return Ok(Self::root());
        }
        check_segment(s)?;
        Ok(Self(s.to_string()))
    }

    /// The root (empty) path of the default endpoint.
    pub fn root() -> Self {
        Self(String::new())
    }

    /// Whether this is the root path.
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The bare segment (`""` for the root).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The server-relative prefix: `""` for the root, `"/{segment}"` else.
    pub fn prefix(&self) -> String {
        if self.0.is_empty() {
            String::new()
        } else {
            format!("/{}", self.0)
        }
    }
}

impl fmt::Display for EndpointPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for EndpointPath {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for EndpointPath {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        EndpointPath::parse(&raw).map_err(|e| serde::de::Error::custom(format!("path: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar() {
        assert!(EndpointName::parse("twcore").is_ok());
        assert!(EndpointName::parse("tw-core-2").is_ok());
        assert!(EndpointName::parse("a").is_ok());
        assert_eq!(EndpointName::parse(""), Err(SegmentError::Empty));
        assert_eq!(EndpointName::parse("Patient"), Err(SegmentError::BadStart));
        assert_eq!(EndpointName::parse("_history"), Err(SegmentError::BadStart));
        assert_eq!(EndpointName::parse("9lives"), Err(SegmentError::BadStart));
        assert_eq!(
            EndpointName::parse("tw_core"),
            Err(SegmentError::BadChar('_'))
        );
        assert_eq!(
            EndpointName::parse("twCore"),
            Err(SegmentError::BadChar('C'))
        );
        assert_eq!(
            EndpointName::parse("tw.core"),
            Err(SegmentError::BadChar('.'))
        );
        assert_eq!(
            EndpointName::parse("tw/core"),
            Err(SegmentError::BadChar('/'))
        );
        assert_eq!(EndpointName::parse("tw-"), Err(SegmentError::BadEnd));
        assert_eq!(
            EndpointName::parse(&"a".repeat(64)),
            Err(SegmentError::TooLong(64))
        );
        assert!(EndpointName::parse(&"a".repeat(63)).is_ok());
    }

    #[test]
    fn reserved() {
        for r in [
            "ui",
            "admin",
            "metrics",
            "fhir",
            "tx",
            "auth",
            "export-status",
        ] {
            assert_eq!(
                EndpointName::parse(r),
                Err(SegmentError::Reserved(r.into()))
            );
            assert!(EndpointPath::parse(r).is_err());
        }
        // `default` is the default endpoint's own name, not reserved.
        assert!(EndpointName::parse("default").unwrap().is_default());
    }

    #[test]
    fn path_prefix() {
        assert_eq!(EndpointPath::root().prefix(), "");
        assert!(EndpointPath::parse("").unwrap().is_root());
        assert_eq!(EndpointPath::parse("twcore").unwrap().prefix(), "/twcore");
    }

    #[test]
    fn serde_validates() {
        let ok: EndpointName = serde_json::from_str("\"lab\"").unwrap();
        assert_eq!(ok.as_str(), "lab");
        assert!(serde_json::from_str::<EndpointName>("\"Lab\"").is_err());
        assert!(serde_json::from_str::<EndpointPath>("\"ui\"").is_err());
        let root: EndpointPath = serde_json::from_str("\"\"").unwrap();
        assert!(root.is_root());
    }
}
