//! Fallback [`PackageStore`] for backends that keep no registry tables
//! (MongoDB, S3, …): one JSON document in the package cache directory,
//! rewritten atomically after every change. Also the in-memory store used
//! by tests (no path).

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use helios_persistence::core::{
    OwnedPackageResource, PackageRecord, PackageResourceRecord, PackageStore,
};
use helios_persistence::error::{BackendError, StorageError, StorageResult};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

#[derive(Default, Serialize, Deserialize)]
struct TenantPackages {
    packages: BTreeMap<String, PackageRecord>,
    resources: BTreeMap<String, Vec<PackageResourceRecord>>,
}

#[derive(Default, Serialize, Deserialize)]
struct Document {
    tenants: BTreeMap<String, TenantPackages>,
}

/// JSON-file (or in-memory) package registry.
pub struct FilePackageStore {
    path: Option<PathBuf>,
    doc: Mutex<Document>,
}

fn key(name: &str, version: &str) -> String {
    format!("{name}#{version}")
}

fn io_err(message: String) -> StorageError {
    StorageError::Backend(BackendError::Internal {
        backend_name: "package-registry-file".to_string(),
        message,
        source: None,
    })
}

impl FilePackageStore {
    /// A store persisted at `path` (loaded now when it exists).
    pub fn at(path: PathBuf) -> Self {
        let doc = std::fs::read(&path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        Self {
            path: Some(path),
            doc: Mutex::new(doc),
        }
    }

    /// A store that lives only in memory.
    pub fn in_memory() -> Self {
        Self {
            path: None,
            doc: Mutex::new(Document::default()),
        }
    }

    fn save(&self, doc: &Document) -> StorageResult<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| io_err(format!("{}: {e}", dir.display())))?;
        }
        let raw = serde_json::to_vec(doc).map_err(|e| io_err(e.to_string()))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, raw).map_err(|e| io_err(format!("{}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, path).map_err(|e| io_err(format!("{}: {e}", path.display())))
    }
}

#[async_trait]
impl PackageStore for FilePackageStore {
    async fn list_packages(&self, tenant: &str) -> StorageResult<Vec<PackageRecord>> {
        let doc = self.doc.lock().await;
        Ok(doc
            .tenants
            .get(tenant)
            .map(|t| t.packages.values().cloned().collect())
            .unwrap_or_default())
    }

    async fn get_package(
        &self,
        tenant: &str,
        name: &str,
        version: &str,
    ) -> StorageResult<Option<PackageRecord>> {
        let doc = self.doc.lock().await;
        Ok(doc
            .tenants
            .get(tenant)
            .and_then(|t| t.packages.get(&key(name, version)).cloned()))
    }

    async fn put_package(&self, tenant: &str, record: &PackageRecord) -> StorageResult<()> {
        let mut doc = self.doc.lock().await;
        doc.tenants
            .entry(tenant.to_string())
            .or_default()
            .packages
            .insert(key(&record.name, &record.version), record.clone());
        self.save(&doc)
    }

    async fn delete_package(&self, tenant: &str, name: &str, version: &str) -> StorageResult<()> {
        let mut doc = self.doc.lock().await;
        if let Some(t) = doc.tenants.get_mut(tenant) {
            t.packages.remove(&key(name, version));
            t.resources.remove(&key(name, version));
        }
        self.save(&doc)
    }

    async fn add_package_resources(
        &self,
        tenant: &str,
        name: &str,
        version: &str,
        resources: &[PackageResourceRecord],
    ) -> StorageResult<()> {
        let mut doc = self.doc.lock().await;
        let rows = doc
            .tenants
            .entry(tenant.to_string())
            .or_default()
            .resources
            .entry(key(name, version))
            .or_default();
        for r in resources {
            match rows.iter_mut().find(|row| row.key() == r.key()) {
                Some(row) => *row = r.clone(),
                None => rows.push(r.clone()),
            }
        }
        self.save(&doc)
    }

    async fn package_resources(
        &self,
        tenant: &str,
        package: Option<(&str, &str)>,
    ) -> StorageResult<Vec<OwnedPackageResource>> {
        let doc = self.doc.lock().await;
        let Some(t) = doc.tenants.get(tenant) else {
            return Ok(Vec::new());
        };
        let wanted = package.map(|(n, v)| key(n, v));
        let mut out = Vec::new();
        for (pkg, rows) in &t.resources {
            if wanted.as_ref().is_some_and(|w| w != pkg) {
                continue;
            }
            let (name, version) = pkg.split_once('#').unwrap_or((pkg.as_str(), ""));
            out.extend(rows.iter().map(|r| OwnedPackageResource {
                package_name: name.to_string(),
                package_version: version.to_string(),
                resource: r.clone(),
            }));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    #[tokio::test]
    async fn file_store_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        let store = FilePackageStore::at(path.clone());
        store
            .put_package(
                "default",
                &PackageRecord {
                    name: "a".into(),
                    version: "1".into(),
                    status: "installed".into(),
                    installed_at: Utc::now(),
                    document: json!({}),
                },
            )
            .await
            .unwrap();
        let row = PackageResourceRecord {
            target: "hfs".into(),
            resource_type: "StructureDefinition".into(),
            resource_id: "x".into(),
            url: None,
            version: None,
            preexisting: false,
        };
        store
            .add_package_resources("default", "a", "1", &[row.clone(), row])
            .await
            .unwrap();
        drop(store);
        let store = FilePackageStore::at(path);
        assert_eq!(store.list_packages("default").await.unwrap().len(), 1);
        assert_eq!(
            store
                .package_resources("default", None)
                .await
                .unwrap()
                .len(),
            1
        );
        store.delete_package("default", "a", "1").await.unwrap();
        assert!(
            store
                .package_resources("default", None)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
