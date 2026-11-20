//! The two sides of a sync: [`SourceReader`] and [`TargetWriter`], each
//! implemented for a local endpoint (in-process storage, [`crate::local`])
//! and an external FHIR server (HTTP, [`crate::external`]).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::change::Change;
use crate::conflict::TargetInfo;

/// A failure talking to one side.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct PeerError(pub String);

impl From<crate::http::HttpError> for PeerError {
    fn from(e: crate::http::HttpError) -> Self {
        PeerError(e.to_string())
    }
}

/// One page of the initial load.
#[derive(Debug, Clone, Default)]
pub struct InitialPage {
    /// Changes (current versions).
    pub changes: Vec<Change>,
    /// Opaque state for the next page; `None` = type finished.
    pub next: Option<String>,
}

/// One page of an incremental window.
///
/// `changes` advance the high-water mark once applied. A source that pages
/// in ascending time order (a local endpoint) returns pages with `next`; a
/// source that can only page newest-first (FHIR `_history`) returns the
/// whole window at once. `neutral` changes (a local source's deletes, read
/// from history separately) are applied first but never advance the mark:
/// they are re-read by later passes and recognised by the item state.
#[derive(Debug, Clone, Default)]
pub struct ChangeWindow {
    /// Changes at or after the requested instant (any order).
    pub changes: Vec<Change>,
    /// Changes applied without moving the high-water mark.
    pub neutral: Vec<Change>,
    /// Continuation of this window; `None` = window complete.
    pub next: Option<String>,
}

/// Reads at the source.
#[async_trait]
pub trait SourceReader: Send + Sync {
    /// Types available for an initial load (a local endpoint answers
    /// [`crate::cursor::ALL_TYPES`]: one ordered stream over every type).
    async fn initial_types(&self) -> Result<Vec<String>, PeerError>;

    /// One page of current versions of `resource_type` (or all types),
    /// optionally only those changed since `since`.
    async fn initial_page(
        &self,
        resource_type: &str,
        page: Option<&str>,
        since: Option<DateTime<Utc>>,
        count: u32,
    ) -> Result<InitialPage, PeerError>;

    /// Changes (versions and deletes) at or after `since`; `page` continues
    /// a window a previous call returned `next` for.
    async fn changes_since(
        &self,
        since: DateTime<Utc>,
        page: Option<&str>,
        count: u32,
    ) -> Result<ChangeWindow, PeerError>;

    /// The current state of one resource (`None` = absent or deleted).
    async fn read(&self, resource_type: &str, id: &str) -> Result<Option<Change>, PeerError>;
}

/// One write at the target.
#[derive(Debug, Clone, PartialEq)]
pub enum WriteOp {
    /// `PUT [type]/[id]`.
    Put {
        /// Type.
        resource_type: String,
        /// Id.
        id: String,
        /// Content.
        resource: Value,
        /// Expected current target version.
        if_match: Option<String>,
    },
    /// `DELETE [type]/[id]`.
    Delete {
        /// Type.
        resource_type: String,
        /// Id.
        id: String,
        /// Expected current target version.
        if_match: Option<String>,
    },
}

/// The outcome of one write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteResult {
    /// Created with this target version.
    Created(Option<String>),
    /// Updated to this target version.
    Updated(Option<String>),
    /// Deleted (or already absent).
    Deleted,
    /// Precondition failed (target changed concurrently).
    Conflict(String),
    /// Refused by the target endpoint's write validation (`enforce`).
    Invalid(String),
    /// Refused (permission, storage…).
    Failed(String),
}

/// Writes at the target.
#[async_trait]
pub trait TargetWriter: Send + Sync {
    /// The target's current state of one resource.
    async fn current(&self, resource_type: &str, id: &str)
    -> Result<Option<TargetInfo>, PeerError>;

    /// Applies writes; one result per op, in order. An `Err` means nothing
    /// usable came back (the whole batch failed and may be retried).
    async fn apply(&self, ops: Vec<WriteOp>) -> Result<Vec<WriteResult>, PeerError>;

    /// Validation findings of written resources since the last call
    /// (`log` mode of a local target), for the run log.
    fn take_findings(&self) -> Vec<String> {
        Vec::new()
    }
}

impl WriteOp {
    /// `type/id` of the op.
    pub fn key(&self) -> String {
        match self {
            WriteOp::Put {
                resource_type, id, ..
            }
            | WriteOp::Delete {
                resource_type, id, ..
            } => {
                format!("{resource_type}/{id}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_keys() {
        let p = WriteOp::Put {
            resource_type: "Patient".into(),
            id: "1".into(),
            resource: serde_json::json!({}),
            if_match: None,
        };
        assert_eq!(p.key(), "Patient/1");
        let d = WriteOp::Delete {
            resource_type: "Observation".into(),
            id: "o".into(),
            if_match: Some("2".into()),
        };
        assert_eq!(d.key(), "Observation/o");
        let e: PeerError = crate::http::HttpError::TooLarge("x".into()).into();
        assert!(e.to_string().contains("size limit"));
    }
}
