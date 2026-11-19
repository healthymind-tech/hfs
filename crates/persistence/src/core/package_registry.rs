//! Registry of installed FHIR packages (Implementation Guides).
//!
//! The package manager in `helios-rest` installs npm-style FHIR packages
//! (`name#version` `.tgz` archives) and has to remember, across restarts,
//! which packages are installed and which stored resources each one brought
//! in, so that an uninstall removes exactly what the package owns — never a
//! resource another package (or an operator) still relies on.
//!
//! This module defines that state as a small, backend-neutral store. Like
//! [`SettingsStore`](crate::core::SettingsStore) it is deliberately *not*
//! part of the FHIR [`ResourceStorage`](crate::core::ResourceStorage)
//! hierarchy: the registry is server bookkeeping, not FHIR data, and must not
//! surface in search, `_history` or `$export`.
//!
//! # Shape
//!
//! - One [`PackageRecord`] per installed `(tenant, name, version)`. Its
//!   `document` is an opaque JSON object owned by the package manager
//!   (title, canonical, FHIR version, source, shasum, dependencies, per-type
//!   counts, …) so new metadata needs no schema change.
//! - One [`PackageResourceRecord`] per resource the package wrote, keyed by
//!   `(target, resource_type, resource_id)` where `target` is `hfs` (this
//!   server's storage) or `hts` (the configured terminology server). Two
//!   packages may own the same key; a resource is only deleted when no other
//!   package owns it.
//!
//! # Backends
//!
//! SQLite and PostgreSQL implement it in two tables (`hfs_packages`,
//! `hfs_package_resources`) created on first use with `CREATE TABLE IF NOT
//! EXISTS` rather than through the versioned schema migrations: the tables
//! are independent of every resource table, so a lazily-created table never
//! has to be ordered against the migration chain. Other backends return
//! `None` from [`ResourceStorage::package_store`](crate::core::ResourceStorage::package_store)
//! and the REST layer falls back to a JSON file in the data directory.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::StorageResult;

/// Where a package resource was written.
pub const TARGET_HFS: &str = "hfs";
/// Written to the terminology server (`HFS_TERMINOLOGY_SERVER`).
pub const TARGET_HTS: &str = "hts";

/// One installed package version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackageRecord {
    /// npm package name, e.g. `tw.gov.mohw.twcore`.
    pub name: String,
    /// Package version, e.g. `1.0.0`.
    pub version: String,
    /// Lifecycle state: `installing`, `installed`, `failed` or `cancelled`.
    pub status: String,
    /// When the install started (first write) or last changed state.
    pub installed_at: DateTime<Utc>,
    /// Package-manager metadata (opaque to this layer; always an object).
    pub document: Value,
}

/// One resource a package wrote.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PackageResourceRecord {
    /// [`TARGET_HFS`] or [`TARGET_HTS`].
    pub target: String,
    /// FHIR resource type.
    pub resource_type: String,
    /// The id the resource is stored under (HFS id; for HTS the package's id,
    /// the server may have chosen another — HTS rows are addressed by
    /// `url` + `version`).
    pub resource_id: String,
    /// Canonical URL, when the resource has one.
    pub url: Option<String>,
    /// Business version (`Resource.version`), when present.
    pub version: Option<String>,
    /// The key already existed before this package wrote it (an operator's
    /// own resource, or terminology loaded some other way). Pre-existing rows
    /// are never deleted by an uninstall.
    pub preexisting: bool,
}

impl PackageResourceRecord {
    /// The `(target, type, id)` identity two packages can share.
    pub fn key(&self) -> (String, String, String) {
        (
            self.target.clone(),
            self.resource_type.clone(),
            self.resource_id.clone(),
        )
    }
}

/// A [`PackageResourceRecord`] together with the package that owns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedPackageResource {
    /// Owning package name.
    pub package_name: String,
    /// Owning package version.
    pub package_version: String,
    /// The resource.
    pub resource: PackageResourceRecord,
}

/// Persistence for the installed-package registry. All methods are
/// tenant-scoped: a package is installed into one tenant's storage.
#[async_trait]
pub trait PackageStore: Send + Sync {
    /// Every package installed in `tenant`, ordered by name then version.
    async fn list_packages(&self, tenant: &str) -> StorageResult<Vec<PackageRecord>>;

    /// One package, or `None` when it is not installed.
    async fn get_package(
        &self,
        tenant: &str,
        name: &str,
        version: &str,
    ) -> StorageResult<Option<PackageRecord>>;

    /// Insert or replace a package row (its resources are left untouched).
    async fn put_package(&self, tenant: &str, record: &PackageRecord) -> StorageResult<()>;

    /// Remove a package row and every resource row it owns.
    async fn delete_package(&self, tenant: &str, name: &str, version: &str) -> StorageResult<()>;

    /// Record resources as owned by a package (insert or replace by key).
    async fn add_package_resources(
        &self,
        tenant: &str,
        name: &str,
        version: &str,
        resources: &[PackageResourceRecord],
    ) -> StorageResult<()>;

    /// Resource rows, optionally limited to one package (`Some((name,
    /// version))`); `None` returns every package's rows (used to decide
    /// shared ownership).
    async fn package_resources(
        &self,
        tenant: &str,
        package: Option<(&str, &str)>,
    ) -> StorageResult<Vec<OwnedPackageResource>>;
}
