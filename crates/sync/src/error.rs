//! Errors of the sync service, mapped by the admin API to HTTP statuses.

use helios_endpoints::model::ValidationErrors;
use helios_endpoints::secret::SecretError;
use helios_endpoints::sync::SyncStoreError;

/// A service-level failure.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SyncError {
    /// `422` with one issue per field.
    #[error(transparent)]
    Invalid(#[from] ValidationErrors),
    /// `404`.
    #[error("{0} not found")]
    NotFound(String),
    /// `409`.
    #[error("{0}")]
    Conflict(String),
    /// `409` stale revision (`If-Match`).
    #[error("revision conflict: current revision is {current}")]
    Revision {
        /// Stored revision.
        current: u64,
    },
    /// `409`: storing a secret needs `HFS_SECRET_KEYS`.
    #[error("storing credentials requires a secret key ring: configure HFS_SECRET_KEYS")]
    NoKeyRing,
    /// `403`: refused by policy (`HFS_SYNC_EXTERNAL=off`, SSRF policy).
    #[error("{0}")]
    Forbidden(String),
    /// `503`.
    #[error("{0}")]
    Unavailable(String),
}

impl From<SyncStoreError> for SyncError {
    fn from(e: SyncStoreError) -> Self {
        match e {
            SyncStoreError::NotFound => SyncError::NotFound("sync job or run".into()),
            SyncStoreError::Conflict { current } => SyncError::Revision { current },
            SyncStoreError::DuplicateName(n) => {
                SyncError::Conflict(format!("a sync job named {n:?} already exists"))
            }
            SyncStoreError::ActiveRun(j) => {
                SyncError::Conflict(format!("job {j} already has a queued or running run"))
            }
            SyncStoreError::LeaseLost => SyncError::Conflict("lease lost".into()),
            SyncStoreError::Unavailable(m) => SyncError::Unavailable(m),
        }
    }
}

impl From<SecretError> for SyncError {
    fn from(e: SecretError) -> Self {
        match e {
            SecretError::NoKeyRing => SyncError::NoKeyRing,
            other => SyncError::Unavailable(format!("stored credential: {other}")),
        }
    }
}

/// Builds a one-field validation error.
pub fn invalid(field: &str, message: impl Into<String>) -> SyncError {
    let mut e = ValidationErrors::default();
    e.push(field, message);
    SyncError::Invalid(e)
}

/// Why an engine run failed.
#[derive(Debug, Clone, thiserror::Error)]
pub enum EngineError {
    /// The run was cancelled or its lease was lost: stop without writing.
    #[error("stopped: {0}")]
    Stopped(String),
    /// A failure that ends the run as `failed` (resumable).
    #[error("{0}")]
    Failed(String),
}

impl From<SyncStoreError> for EngineError {
    fn from(e: SyncStoreError) -> Self {
        match e {
            SyncStoreError::LeaseLost => EngineError::Stopped("lease lost or run cancelled".into()),
            other => EngineError::Failed(format!("sync store: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_errors_map() {
        assert!(matches!(
            SyncError::from(SyncStoreError::Conflict { current: 3 }),
            SyncError::Revision { current: 3 }
        ));
        assert!(matches!(
            SyncError::from(SyncStoreError::ActiveRun("j".into())),
            SyncError::Conflict(_)
        ));
        assert!(matches!(
            SyncError::from(SyncStoreError::DuplicateName("x".into())),
            SyncError::Conflict(m) if m.contains("x")
        ));
        assert!(matches!(SyncError::from(SyncStoreError::NotFound), SyncError::NotFound(_)));
        assert!(matches!(
            SyncError::from(SyncStoreError::LeaseLost),
            SyncError::Conflict(_)
        ));
        assert!(matches!(
            SyncError::from(SyncStoreError::Unavailable("db".into())),
            SyncError::Unavailable(_)
        ));
        assert!(matches!(SyncError::from(SecretError::NoKeyRing), SyncError::NoKeyRing));
        assert!(matches!(SyncError::from(SecretError::Open), SyncError::Unavailable(_)));
        assert!(matches!(invalid("name", "bad"), SyncError::Invalid(e) if e.0[0].field == "name"));
        assert!(matches!(
            EngineError::from(SyncStoreError::LeaseLost),
            EngineError::Stopped(_)
        ));
        assert!(matches!(
            EngineError::from(SyncStoreError::NotFound),
            EngineError::Failed(_)
        ));
    }
}
