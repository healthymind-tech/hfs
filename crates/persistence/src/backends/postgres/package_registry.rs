//! PostgreSQL implementation of the installed-package registry
//! ([`PackageStore`]).
//!
//! Same two tables as the SQLite backend (`hfs_packages`,
//! `hfs_package_resources`), created on first use. Creation checks
//! `to_regclass` first and only then takes a transaction-scoped advisory lock
//! around the `CREATE TABLE IF NOT EXISTS`, so two instances starting against
//! one database cannot race on the catalog (`IF NOT EXISTS` alone is not safe
//! under concurrency in PostgreSQL).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::core::package_registry::{
    OwnedPackageResource, PackageRecord, PackageResourceRecord, PackageStore,
};
use crate::error::{BackendError, StorageError, StorageResult};

use super::PostgresBackend;

/// Advisory-lock key for the lazy DDL ("HPKG"); see the user-settings module
/// for the namespacing convention of two-part advisory keys.
const PACKAGE_DDL_LOCK_NAMESPACE: i32 = 0x4850_4B47;

const DDL: &str = "
CREATE TABLE IF NOT EXISTS hfs_packages (
    tenant_id    TEXT NOT NULL,
    name         TEXT NOT NULL,
    version      TEXT NOT NULL,
    status       TEXT NOT NULL,
    installed_at TIMESTAMPTZ NOT NULL,
    document     JSONB NOT NULL,
    PRIMARY KEY (tenant_id, name, version)
);
CREATE TABLE IF NOT EXISTS hfs_package_resources (
    tenant_id     TEXT NOT NULL,
    name          TEXT NOT NULL,
    version       TEXT NOT NULL,
    target        TEXT NOT NULL,
    resource_type TEXT NOT NULL,
    resource_id   TEXT NOT NULL,
    url           TEXT,
    res_version   TEXT,
    preexisting   BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (tenant_id, name, version, target, resource_type, resource_id)
);
CREATE INDEX IF NOT EXISTS idx_hfs_package_resources_key
    ON hfs_package_resources (tenant_id, target, resource_type, resource_id);
";

fn backend_err(message: String) -> StorageError {
    StorageError::Backend(BackendError::Internal {
        backend_name: "postgres".to_string(),
        message,
        source: None,
    })
}

impl PostgresBackend {
    async fn package_client(&self) -> StorageResult<deadpool_postgres::Client> {
        let mut client = self.get_client().await?;
        let present: bool = client
            .query_one(
                "SELECT to_regclass('hfs_package_resources') IS NOT NULL",
                &[],
            )
            .await
            .map_err(|e| backend_err(format!("probe package registry tables: {e}")))?
            .get(0);
        if !present {
            let txn = client
                .transaction()
                .await
                .map_err(|e| backend_err(format!("begin package registry DDL: {e}")))?;
            txn.execute(
                "SELECT pg_advisory_xact_lock($1, 0)",
                &[&PACKAGE_DDL_LOCK_NAMESPACE],
            )
            .await
            .map_err(|e| backend_err(format!("lock package registry DDL: {e}")))?;
            txn.batch_execute(DDL)
                .await
                .map_err(|e| backend_err(format!("create package registry tables: {e}")))?;
            txn.commit()
                .await
                .map_err(|e| backend_err(format!("commit package registry DDL: {e}")))?;
        }
        Ok(client)
    }
}

fn to_record(row: &tokio_postgres::Row) -> PackageRecord {
    PackageRecord {
        name: row.get(0),
        version: row.get(1),
        status: row.get(2),
        installed_at: row.get::<_, DateTime<Utc>>(3),
        document: row.get::<_, Value>(4),
    }
}

#[async_trait]
impl PackageStore for PostgresBackend {
    async fn list_packages(&self, tenant: &str) -> StorageResult<Vec<PackageRecord>> {
        let client = self.package_client().await?;
        let rows = client
            .query(
                "SELECT name, version, status, installed_at, document FROM hfs_packages
                 WHERE tenant_id = $1 ORDER BY name, version",
                &[&tenant],
            )
            .await
            .map_err(|e| backend_err(format!("list packages: {e}")))?;
        Ok(rows.iter().map(to_record).collect())
    }

    async fn get_package(
        &self,
        tenant: &str,
        name: &str,
        version: &str,
    ) -> StorageResult<Option<PackageRecord>> {
        let client = self.package_client().await?;
        let row = client
            .query_opt(
                "SELECT name, version, status, installed_at, document FROM hfs_packages
                 WHERE tenant_id = $1 AND name = $2 AND version = $3",
                &[&tenant, &name, &version],
            )
            .await
            .map_err(|e| backend_err(format!("read package: {e}")))?;
        Ok(row.as_ref().map(to_record))
    }

    async fn put_package(&self, tenant: &str, record: &PackageRecord) -> StorageResult<()> {
        let client = self.package_client().await?;
        client
            .execute(
                "INSERT INTO hfs_packages (tenant_id, name, version, status, installed_at, document)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (tenant_id, name, version) DO UPDATE SET
                     status = EXCLUDED.status, installed_at = EXCLUDED.installed_at,
                     document = EXCLUDED.document",
                &[
                    &tenant,
                    &record.name,
                    &record.version,
                    &record.status,
                    &record.installed_at,
                    &record.document,
                ],
            )
            .await
            .map_err(|e| backend_err(format!("write package: {e}")))?;
        Ok(())
    }

    async fn delete_package(&self, tenant: &str, name: &str, version: &str) -> StorageResult<()> {
        let mut client = self.package_client().await?;
        let txn = client
            .transaction()
            .await
            .map_err(|e| backend_err(format!("begin package delete: {e}")))?;
        txn.execute(
            "DELETE FROM hfs_package_resources WHERE tenant_id = $1 AND name = $2 AND version = $3",
            &[&tenant, &name, &version],
        )
        .await
        .map_err(|e| backend_err(format!("delete package resources: {e}")))?;
        txn.execute(
            "DELETE FROM hfs_packages WHERE tenant_id = $1 AND name = $2 AND version = $3",
            &[&tenant, &name, &version],
        )
        .await
        .map_err(|e| backend_err(format!("delete package: {e}")))?;
        txn.commit()
            .await
            .map_err(|e| backend_err(format!("commit package delete: {e}")))
    }

    async fn add_package_resources(
        &self,
        tenant: &str,
        name: &str,
        version: &str,
        resources: &[PackageResourceRecord],
    ) -> StorageResult<()> {
        if resources.is_empty() {
            return Ok(());
        }
        let mut client = self.package_client().await?;
        let txn = client
            .transaction()
            .await
            .map_err(|e| backend_err(format!("begin package resources write: {e}")))?;
        let stmt = txn
            .prepare(
                "INSERT INTO hfs_package_resources
                 (tenant_id, name, version, target, resource_type, resource_id, url,
                  res_version, preexisting)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                 ON CONFLICT (tenant_id, name, version, target, resource_type, resource_id)
                 DO UPDATE SET url = EXCLUDED.url, res_version = EXCLUDED.res_version,
                               preexisting = EXCLUDED.preexisting",
            )
            .await
            .map_err(|e| backend_err(format!("prepare package resources write: {e}")))?;
        for r in resources {
            txn.execute(
                &stmt,
                &[
                    &tenant,
                    &name,
                    &version,
                    &r.target,
                    &r.resource_type,
                    &r.resource_id,
                    &r.url,
                    &r.version,
                    &r.preexisting,
                ],
            )
            .await
            .map_err(|e| backend_err(format!("write package resource: {e}")))?;
        }
        txn.commit()
            .await
            .map_err(|e| backend_err(format!("commit package resources: {e}")))
    }

    async fn package_resources(
        &self,
        tenant: &str,
        package: Option<(&str, &str)>,
    ) -> StorageResult<Vec<OwnedPackageResource>> {
        let client = self.package_client().await?;
        let rows = match package {
            Some((name, version)) => client
                .query(
                    "SELECT name, version, target, resource_type, resource_id, url, res_version,
                            preexisting
                     FROM hfs_package_resources
                     WHERE tenant_id = $1 AND name = $2 AND version = $3
                     ORDER BY resource_type, resource_id",
                    &[&tenant, &name, &version],
                )
                .await,
            None => client
                .query(
                    "SELECT name, version, target, resource_type, resource_id, url, res_version,
                            preexisting
                     FROM hfs_package_resources
                     WHERE tenant_id = $1
                     ORDER BY name, version, resource_type, resource_id",
                    &[&tenant],
                )
                .await,
        }
        .map_err(|e| backend_err(format!("list package resources: {e}")))?;
        Ok(rows
            .iter()
            .map(|row| OwnedPackageResource {
                package_name: row.get(0),
                package_version: row.get(1),
                resource: PackageResourceRecord {
                    target: row.get(2),
                    resource_type: row.get(3),
                    resource_id: row.get(4),
                    url: row.get(5),
                    version: row.get(6),
                    preexisting: row.get(7),
                },
            })
            .collect())
    }
}
