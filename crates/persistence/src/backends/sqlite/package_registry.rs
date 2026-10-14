//! SQLite implementation of the installed-package registry
//! ([`PackageStore`]).
//!
//! Two tables in the server's own database, created on first use (see the
//! module docs of [`crate::core::package_registry`] for why they sit outside
//! the versioned schema): `hfs_packages`, one row per installed
//! `(tenant, name, version)` with the package manager's JSON document, and
//! `hfs_package_resources`, one row per resource a package wrote.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::core::package_registry::{
    OwnedPackageResource, PackageRecord, PackageResourceRecord, PackageStore,
};
use crate::error::{BackendError, StorageError, StorageResult};

use super::SqliteBackend;

const DDL: &str = "
CREATE TABLE IF NOT EXISTS hfs_packages (
    tenant_id    TEXT NOT NULL,
    name         TEXT NOT NULL,
    version      TEXT NOT NULL,
    status       TEXT NOT NULL,
    installed_at TEXT NOT NULL,
    document     TEXT NOT NULL,
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
    preexisting   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, name, version, target, resource_type, resource_id)
);
CREATE INDEX IF NOT EXISTS idx_hfs_package_resources_key
    ON hfs_package_resources (tenant_id, target, resource_type, resource_id);
";

fn backend_err(message: String) -> StorageError {
    StorageError::Backend(BackendError::Internal {
        backend_name: "sqlite".to_string(),
        message,
        source: None,
    })
}

fn ensure_tables(conn: &Connection) -> StorageResult<()> {
    conn.execute_batch(DDL)
        .map_err(|e| backend_err(format!("create package registry tables: {e}")))
}

fn parse_time(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn package_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(String, String, String, String, String)> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

fn to_record(
    (name, version, status, installed_at, document): (String, String, String, String, String),
) -> StorageResult<PackageRecord> {
    let document = serde_json::from_str(&document)
        .map_err(|e| backend_err(format!("decode package {name}#{version}: {e}")))?;
    Ok(PackageRecord {
        name,
        version,
        status,
        installed_at: parse_time(&installed_at),
        document,
    })
}

#[async_trait]
impl PackageStore for SqliteBackend {
    async fn list_packages(&self, tenant: &str) -> StorageResult<Vec<PackageRecord>> {
        let conn = self.get_connection()?;
        ensure_tables(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT name, version, status, installed_at, document FROM hfs_packages
                 WHERE tenant_id = ?1 ORDER BY name, version",
            )
            .map_err(|e| backend_err(format!("list packages: {e}")))?;
        let rows = stmt
            .query_map(params![tenant], package_from_row)
            .map_err(|e| backend_err(format!("list packages: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(to_record(
                row.map_err(|e| backend_err(format!("list packages: {e}")))?,
            )?);
        }
        Ok(out)
    }

    async fn get_package(
        &self,
        tenant: &str,
        name: &str,
        version: &str,
    ) -> StorageResult<Option<PackageRecord>> {
        let conn = self.get_connection()?;
        ensure_tables(&conn)?;
        let row = conn
            .query_row(
                "SELECT name, version, status, installed_at, document FROM hfs_packages
                 WHERE tenant_id = ?1 AND name = ?2 AND version = ?3",
                params![tenant, name, version],
                package_from_row,
            )
            .optional()
            .map_err(|e| backend_err(format!("read package: {e}")))?;
        row.map(to_record).transpose()
    }

    async fn put_package(&self, tenant: &str, record: &PackageRecord) -> StorageResult<()> {
        let conn = self.get_connection()?;
        ensure_tables(&conn)?;
        let document = serde_json::to_string(&record.document)
            .map_err(|e| backend_err(format!("encode package: {e}")))?;
        conn.execute(
            "INSERT INTO hfs_packages (tenant_id, name, version, status, installed_at, document)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(tenant_id, name, version) DO UPDATE SET
                 status = ?4, installed_at = ?5, document = ?6",
            params![
                tenant,
                record.name,
                record.version,
                record.status,
                record.installed_at.to_rfc3339(),
                document
            ],
        )
        .map_err(|e| backend_err(format!("write package: {e}")))?;
        Ok(())
    }

    async fn delete_package(&self, tenant: &str, name: &str, version: &str) -> StorageResult<()> {
        let mut conn = self.get_connection()?;
        ensure_tables(&conn)?;
        let txn = conn
            .transaction()
            .map_err(|e| backend_err(format!("begin package delete: {e}")))?;
        txn.execute(
            "DELETE FROM hfs_package_resources WHERE tenant_id = ?1 AND name = ?2 AND version = ?3",
            params![tenant, name, version],
        )
        .map_err(|e| backend_err(format!("delete package resources: {e}")))?;
        txn.execute(
            "DELETE FROM hfs_packages WHERE tenant_id = ?1 AND name = ?2 AND version = ?3",
            params![tenant, name, version],
        )
        .map_err(|e| backend_err(format!("delete package: {e}")))?;
        txn.commit()
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
        let mut conn = self.get_connection()?;
        ensure_tables(&conn)?;
        let txn = conn
            .transaction()
            .map_err(|e| backend_err(format!("begin package resources write: {e}")))?;
        {
            let mut stmt = txn
                .prepare(
                    "INSERT OR REPLACE INTO hfs_package_resources
                     (tenant_id, name, version, target, resource_type, resource_id, url,
                      res_version, preexisting)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )
                .map_err(|e| backend_err(format!("prepare package resources write: {e}")))?;
            for r in resources {
                stmt.execute(params![
                    tenant,
                    name,
                    version,
                    r.target,
                    r.resource_type,
                    r.resource_id,
                    r.url,
                    r.version,
                    i64::from(r.preexisting)
                ])
                .map_err(|e| backend_err(format!("write package resource: {e}")))?;
            }
        }
        txn.commit()
            .map_err(|e| backend_err(format!("commit package resources: {e}")))
    }

    async fn package_resources(
        &self,
        tenant: &str,
        package: Option<(&str, &str)>,
    ) -> StorageResult<Vec<OwnedPackageResource>> {
        let conn = self.get_connection()?;
        ensure_tables(&conn)?;
        let (sql, name, version) = match package {
            Some((n, v)) => (
                "SELECT name, version, target, resource_type, resource_id, url, res_version,
                        preexisting
                 FROM hfs_package_resources
                 WHERE tenant_id = ?1 AND name = ?2 AND version = ?3
                 ORDER BY resource_type, resource_id",
                n.to_string(),
                v.to_string(),
            ),
            None => (
                "SELECT name, version, target, resource_type, resource_id, url, res_version,
                        preexisting
                 FROM hfs_package_resources
                 WHERE tenant_id = ?1 AND ?2 = ?2 AND ?3 = ?3
                 ORDER BY name, version, resource_type, resource_id",
                String::new(),
                String::new(),
            ),
        };
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| backend_err(format!("list package resources: {e}")))?;
        let rows = stmt
            .query_map(params![tenant, name, version], |row| {
                Ok(OwnedPackageResource {
                    package_name: row.get(0)?,
                    package_version: row.get(1)?,
                    resource: PackageResourceRecord {
                        target: row.get(2)?,
                        resource_type: row.get(3)?,
                        resource_id: row.get(4)?,
                        url: row.get(5)?,
                        version: row.get(6)?,
                        preexisting: row.get::<_, i64>(7)? != 0,
                    },
                })
            })
            .map_err(|e| backend_err(format!("list package resources: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| backend_err(format!("list package resources: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(name: &str, version: &str) -> PackageRecord {
        PackageRecord {
            name: name.into(),
            version: version.into(),
            status: "installed".into(),
            installed_at: Utc::now(),
            document: json!({"title": name}),
        }
    }

    fn res(id: &str) -> PackageResourceRecord {
        PackageResourceRecord {
            target: "hfs".into(),
            resource_type: "StructureDefinition".into(),
            resource_id: id.into(),
            url: Some(format!("http://example.org/{id}")),
            version: Some("1.0.0".into()),
            preexisting: false,
        }
    }

    #[tokio::test]
    async fn package_rows_round_trip_and_delete_cascades_to_resources() {
        let db = SqliteBackend::in_memory().unwrap();
        db.put_package("default", &record("a.pkg", "1.0.0"))
            .await
            .unwrap();
        db.put_package("default", &record("b.pkg", "2.0.0"))
            .await
            .unwrap();
        db.put_package("other", &record("a.pkg", "1.0.0"))
            .await
            .unwrap();
        db.add_package_resources("default", "a.pkg", "1.0.0", &[res("x"), res("y")])
            .await
            .unwrap();
        db.add_package_resources("default", "b.pkg", "2.0.0", &[res("y")])
            .await
            .unwrap();

        let listed = db.list_packages("default").await.unwrap();
        assert_eq!(
            listed.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["a.pkg", "b.pkg"]
        );
        assert_eq!(listed[0].document["title"], "a.pkg");
        assert!(
            db.get_package("default", "a.pkg", "9.9.9")
                .await
                .unwrap()
                .is_none()
        );

        let all = db.package_resources("default", None).await.unwrap();
        assert_eq!(all.len(), 3);
        let only_a = db
            .package_resources("default", Some(("a.pkg", "1.0.0")))
            .await
            .unwrap();
        assert_eq!(only_a.len(), 2);

        // Re-adding the same key replaces rather than duplicates.
        let mut again = res("x");
        again.preexisting = true;
        db.add_package_resources("default", "a.pkg", "1.0.0", &[again])
            .await
            .unwrap();
        let only_a = db
            .package_resources("default", Some(("a.pkg", "1.0.0")))
            .await
            .unwrap();
        assert_eq!(only_a.len(), 2);
        assert!(only_a.iter().any(|r| r.resource.preexisting));

        db.delete_package("default", "a.pkg", "1.0.0")
            .await
            .unwrap();
        assert!(
            db.get_package("default", "a.pkg", "1.0.0")
                .await
                .unwrap()
                .is_none()
        );
        let all = db.package_resources("default", None).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].package_name, "b.pkg");
        // Other tenants are untouched.
        assert_eq!(db.list_packages("other").await.unwrap().len(), 1);
    }
}
