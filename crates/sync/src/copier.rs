//! Set-based tenant copy (the `storage-copy` initial load, endpoint
//! duplication with data). Implemented for PostgreSQL by
//! `PostgresBackend::copy_tenant` (persistence `tenant_copy.rs`).

use async_trait::async_trait;

/// What to copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyRequest {
    /// From.
    pub source_tenant: String,
    /// Into (must hold no resources).
    pub target_tenant: String,
    /// Every history version, not only current ones.
    pub include_history: bool,
    /// Only these types.
    pub resource_types: Option<Vec<String>>,
    /// Package registry rows too.
    pub copy_packages: bool,
}

/// What was copied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyReport {
    /// Resources (current versions) copied.
    pub resources: u64,
    /// Rows per table, for the log.
    pub summary: String,
}

/// A backend that can copy a tenant inside the database.
#[async_trait]
pub trait TenantCopier: Send + Sync {
    /// Copies; the target must be empty.
    async fn copy(&self, req: CopyRequest) -> Result<CopyReport, String>;
}

#[cfg(feature = "postgres")]
#[async_trait]
impl TenantCopier for helios_persistence::backends::postgres::PostgresBackend {
    async fn copy(&self, req: CopyRequest) -> Result<CopyReport, String> {
        let opts = helios_persistence::backends::postgres::TenantCopyOptions {
            include_history: req.include_history,
            resource_types: req.resource_types,
            copy_packages: req.copy_packages,
            require_empty_target: true,
        };
        let r = self
            .copy_tenant(&req.source_tenant, &req.target_tenant, &opts)
            .await
            .map_err(|e| e.to_string())?;
        Ok(CopyReport {
            resources: r.resources(),
            summary: r
                .rows
                .iter()
                .map(|(t, n)| format!("{t}={n}"))
                .collect::<Vec<_>>()
                .join(", "),
        })
    }
}
