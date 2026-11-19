//! Minimal FHIR package registry client (packages.fhir.org protocol).
//!
//! Only what terminology installation needs: catalog search, version listing
//! and a verified tarball download. The HFS package manager (plan stream E)
//! needs the same calls; this module is self-contained so it can move to a
//! shared crate without changes.
//!
//! The registry is configurable (`HTS_PACKAGE_REGISTRY`, an internal mirror
//! URL) and can be switched off entirely (`off`) for hospitals without
//! outbound internet — upload and the server import directory keep working.

use std::path::Path;

use serde::Serialize;
use serde_json::Value;
use sha1::{Digest, Sha1};
use tokio::io::AsyncWriteExt;

use super::AdminError;

/// One catalog hit.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CatalogEntry {
    pub name: String,
    pub description: Option<String>,
    pub fhir_version: Option<String>,
}

/// Resolved package version.
#[derive(Debug, Clone, Serialize)]
pub struct PackageVersion {
    pub name: String,
    pub version: String,
    pub tarball: String,
    pub shasum: Option<String>,
    pub fhir_version: Option<String>,
}

/// Registry client. `base = None` means the registry is disabled.
#[derive(Clone)]
pub struct Registry {
    base: Option<String>,
    http: reqwest::Client,
}

fn upstream(e: impl std::fmt::Display) -> AdminError {
    AdminError::Upstream(e.to_string())
}

/// Package names are npm-style: lowercase letters, digits, `.`, `-`, `_`.
pub fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 214
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_')
        })
}

impl Registry {
    pub fn new(base: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("helios-hts/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self {
            base: base.map(|b| b.trim_end_matches('/').to_string()),
            http,
        }
    }

    pub fn base(&self) -> Option<&str> {
        self.base.as_deref()
    }

    fn require(&self) -> Result<&str, AdminError> {
        self.base.as_deref().ok_or_else(|| {
            AdminError::Disabled("package registry is disabled (HTS_PACKAGE_REGISTRY=off)".into())
        })
    }

    /// `GET /catalog?name=…&fhirversion=…`.
    pub async fn search(
        &self,
        query: &str,
        fhir_version: Option<&str>,
    ) -> Result<Vec<CatalogEntry>, AdminError> {
        let base = self.require()?;
        let mut req = self
            .http
            .get(format!("{base}/catalog"))
            .query(&[("name", query)])
            .timeout(std::time::Duration::from_secs(30));
        if let Some(v) = fhir_version {
            req = req.query(&[("fhirversion", v)]);
        }
        let v: Value = req
            .send()
            .await
            .map_err(upstream)?
            .error_for_status()
            .map_err(upstream)?
            .json()
            .await
            .map_err(upstream)?;
        Ok(parse_catalog(&v))
    }

    /// Resolve `version` (or `dist-tags.latest`) of `name`.
    pub async fn resolve(
        &self,
        name: &str,
        version: Option<&str>,
    ) -> Result<PackageVersion, AdminError> {
        if !valid_package_name(name) {
            return Err(AdminError::BadRequest(format!(
                "invalid package name {name:?}"
            )));
        }
        let base = self.require()?;
        let resp = self
            .http
            .get(format!("{base}/{name}"))
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(upstream)?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(AdminError::NotFound(format!("package {name}")));
        }
        let v: Value = resp
            .error_for_status()
            .map_err(upstream)?
            .json()
            .await
            .map_err(upstream)?;
        parse_manifest(&v, name, version)
    }

    /// Download the tarball to `dest`, verifying the registry's SHA-1
    /// `shasum` when it publishes one. `on_bytes` receives the running total.
    pub async fn download(
        &self,
        pkg: &PackageVersion,
        dest: &Path,
        mut on_bytes: impl FnMut(u64, Option<u64>),
    ) -> Result<String, AdminError> {
        self.require()?;
        let mut resp = self
            .http
            .get(&pkg.tarball)
            .timeout(std::time::Duration::from_secs(1800))
            .send()
            .await
            .map_err(upstream)?
            .error_for_status()
            .map_err(upstream)?;
        let total = resp.content_length();
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| AdminError::Internal(e.to_string()))?;
        }
        let tmp = dest.with_extension("part");
        let mut file = tokio::fs::File::create(&tmp)
            .await
            .map_err(|e| AdminError::Internal(e.to_string()))?;
        let mut hasher = Sha1::new();
        let mut done = 0u64;
        while let Some(chunk) = resp.chunk().await.map_err(upstream)? {
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|e| AdminError::Internal(e.to_string()))?;
            done += chunk.len() as u64;
            on_bytes(done, total);
        }
        file.flush()
            .await
            .map_err(|e| AdminError::Internal(e.to_string()))?;
        drop(file);
        let digest = hasher.finalize();
        let sha: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        if let Some(expected) = pkg.shasum.as_deref()
            && !expected.eq_ignore_ascii_case(&sha)
        {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(AdminError::Upstream(format!(
                "shasum mismatch for {}#{}: registry {expected}, downloaded {sha}",
                pkg.name, pkg.version
            )));
        }
        tokio::fs::rename(&tmp, dest)
            .await
            .map_err(|e| AdminError::Internal(e.to_string()))?;
        Ok(sha)
    }
}

/// The catalog uses PascalCase keys on packages.fhir.org and camelCase on
/// some mirrors; accept both.
pub fn parse_catalog(v: &Value) -> Vec<CatalogEntry> {
    let get = |o: &Value, keys: &[&str]| {
        keys.iter()
            .find_map(|k| o.get(*k).and_then(|x| x.as_str()).map(String::from))
    };
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|o| {
                    Some(CatalogEntry {
                        name: get(o, &["Name", "name"])?,
                        description: get(o, &["Description", "description"]),
                        fhir_version: get(o, &["FhirVersion", "fhirVersion", "fhirversion"]),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Pick `version` (default `dist-tags.latest`) out of an npm manifest.
pub fn parse_manifest(
    v: &Value,
    name: &str,
    version: Option<&str>,
) -> Result<PackageVersion, AdminError> {
    let version = match version {
        Some(v) => v.to_string(),
        None => v
            .pointer("/dist-tags/latest")
            .and_then(|x| x.as_str())
            .ok_or_else(|| AdminError::Upstream(format!("{name}: no dist-tags.latest")))?
            .to_string(),
    };
    let entry = v
        .get("versions")
        .and_then(|vs| vs.get(&version))
        .ok_or_else(|| AdminError::NotFound(format!("package {name}#{version}")))?;
    let tarball = entry
        .pointer("/dist/tarball")
        .and_then(|x| x.as_str())
        .ok_or_else(|| AdminError::Upstream(format!("{name}#{version}: no dist.tarball")))?
        .to_string();
    Ok(PackageVersion {
        name: name.to_string(),
        version,
        tarball,
        shasum: entry
            .pointer("/dist/shasum")
            .and_then(|x| x.as_str())
            .map(String::from),
        fhir_version: entry
            .get("fhirVersion")
            .and_then(|x| x.as_str())
            .map(String::from),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_catalog_in_both_casings() {
        let v = json!([
            {"Name": "hl7.terminology.r4", "Description": "THO", "FhirVersion": "R4"},
            {"name": "x.y", "fhirVersion": "R5"},
            {"Description": "nameless"}
        ]);
        let c = parse_catalog(&v);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].name, "hl7.terminology.r4");
        assert_eq!(c[1].fhir_version.as_deref(), Some("R5"));
    }

    #[test]
    fn resolves_latest_and_explicit_versions() {
        let v = json!({
            "dist-tags": {"latest": "7.4.0"},
            "versions": {
                "7.4.0": {"dist": {"tarball": "https://r/7.4.0", "shasum": "ab"}, "fhirVersion": "R4"},
                "6.0.2": {"dist": {"tarball": "https://r/6.0.2"}}
            }
        });
        let p = parse_manifest(&v, "hl7.terminology.r4", None).unwrap();
        assert_eq!(
            (p.version.as_str(), p.shasum.as_deref()),
            ("7.4.0", Some("ab"))
        );
        let p = parse_manifest(&v, "hl7.terminology.r4", Some("6.0.2")).unwrap();
        assert_eq!(p.tarball, "https://r/6.0.2");
        assert!(parse_manifest(&v, "hl7.terminology.r4", Some("1.0.0")).is_err());
    }

    #[test]
    fn package_name_validation() {
        assert!(valid_package_name("hl7.terminology.r4"));
        assert!(!valid_package_name("../etc"));
        assert!(!valid_package_name("Has Space"));
    }

    #[tokio::test]
    async fn disabled_registry_refuses() {
        let r = Registry::new(None);
        assert!(matches!(
            r.search("x", None).await,
            Err(AdminError::Disabled(_))
        ));
    }
}
