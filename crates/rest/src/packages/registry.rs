//! Package sources: the online registries, the marketplace index and the
//! on-disk package cache.
//!
//! - **Registries** are npm-compatible: `GET {base}/{name}` lists versions
//!   (`versions.{v}.dist.{tarball,shasum}`, `dist-tags.latest`) and
//!   `GET {base}/{name}/{version}` serves the archive. packages.fhir.org is
//!   tried first and packages2.fhir.org second, or the operator's mirrors
//!   instead (`HFS_PACKAGE_REGISTRY`). Every download is checked against
//!   the registry's SHA-1 `shasum` when it publishes one.
//! - **Catalog search** uses each registry's `/catalog?name=&fhirversion=`.
//! - **Marketplace**: FHIR/ig-registry's `fhir-ig-list.json` (category,
//!   country, authority, editions), cached in memory and on disk for a day.
//! - **Cache**: `<cache dir>/<name>#<version>.tgz`, written atomically. It
//!   is also where uploads land and where an offline hospital can drop
//!   packages by hand; with the registry `off` it is the only source.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use tracing::{debug, warn};

use super::resolver::{PackageSource, VersionList};
use super::tarball::{
    Limits, Manifest, read_manifest, sha1_hex, valid_package_name, valid_package_version,
};
use super::version::compare;
use crate::config::RegistryMode;

/// How long a registry version list is reused.
const DOC_TTL: Duration = Duration::from_secs(300);
/// How long the marketplace index is reused.
const MARKET_TTL: Duration = Duration::from_secs(24 * 3600);

/// One published version as a registry lists it.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryVersion {
    /// Version.
    pub version: String,
    /// FHIR version, when listed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fhir_version: Option<String>,
    /// Publication date, when listed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// Tarball URL.
    #[serde(skip)]
    pub tarball: Option<String>,
    /// SHA-1 of the tarball.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shasum: Option<String>,
}

/// A package's registry document.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryDoc {
    /// Package name.
    pub name: String,
    /// Description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `dist-tags.latest`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// Versions, newest first.
    pub versions: Vec<RegistryVersion>,
    /// The registry that answered.
    pub source: String,
}

/// One catalog search hit.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    /// Package name.
    pub name: String,
    /// Description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// FHIR version label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fhir_version: Option<String>,
    /// Latest version, when the catalog reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Canonical, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical: Option<String>,
}

/// One edition of a marketplace IG.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketEdition {
    /// Edition name ("STU 1.0.0").
    pub name: String,
    /// IG version.
    pub version: String,
    /// `name#version`.
    pub package: String,
    /// FHIR versions.
    pub fhir_versions: Vec<String>,
}

/// One marketplace IG (an `fhir-ig-list.json` guide).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketEntry {
    /// npm package name.
    pub name: String,
    /// Human title.
    pub title: String,
    /// Category ("National Base", …).
    pub category: String,
    /// ISO country code (`tw`, `us`, `uv`, …).
    pub country: String,
    /// Publishing authority ("MOHW (TW)", "HL7", …).
    pub authority: String,
    /// Description.
    pub description: String,
    /// Canonical.
    pub canonical: String,
    /// Editions, newest last as published.
    pub editions: Vec<MarketEdition>,
}

impl MarketEntry {
    fn from_json(guide: &Value) -> Option<Self> {
        let s = |key: &str| {
            guide
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let name = s("npm-name");
        if !valid_package_name(&name) {
            return None;
        }
        let editions = guide
            .get("editions")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|e| {
                        let package = e.get("package").and_then(Value::as_str)?.to_string();
                        let version = e
                            .get("ig-version")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .or_else(|| package.split_once('#').map(|(_, v)| v.to_string()))?;
                        Some(MarketEdition {
                            name: e
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            version,
                            package,
                            fhir_versions: e
                                .get("fhir-version")
                                .and_then(Value::as_array)
                                .map(|v| {
                                    v.iter()
                                        .filter_map(Value::as_str)
                                        .map(str::to_string)
                                        .collect()
                                })
                                .unwrap_or_default(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            name,
            title: s("name"),
            category: s("category"),
            country: s("country").to_ascii_lowercase(),
            authority: s("authority"),
            description: s("description"),
            canonical: s("canonical"),
            editions,
        })
    }

    /// The newest edition's version.
    pub fn latest(&self) -> Option<&str> {
        self.editions
            .iter()
            .map(|e| e.version.as_str())
            .max_by(|a, b| compare(a, b))
    }
}

/// Parses a `fhir-ig-list.json` document.
pub fn parse_marketplace(doc: &Value) -> Vec<MarketEntry> {
    doc.get("guides")
        .and_then(Value::as_array)
        .map(|guides| guides.iter().filter_map(MarketEntry::from_json).collect())
        .unwrap_or_default()
}

/// Provenance of a fetched archive.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchInfo {
    /// `cache`, or the URL the archive came from.
    pub source: String,
    /// Whether a registry shasum was checked (and matched).
    pub verified: bool,
}

/// The registry client and package cache.
pub struct Registry {
    mode: RegistryMode,
    marketplace_url: Option<String>,
    cache_dir: PathBuf,
    limits: Limits,
    http: reqwest::Client,
    docs: Mutex<HashMap<String, (Instant, RegistryDoc)>>,
    manifests: Mutex<HashMap<String, Manifest>>,
    market: Mutex<Option<(Instant, Arc<Vec<MarketEntry>>)>>,
}

impl Registry {
    /// A client for `mode`, caching under `cache_dir`.
    pub fn new(
        mode: RegistryMode,
        marketplace_url: Option<String>,
        cache_dir: PathBuf,
        limits: Limits,
    ) -> Self {
        let http = reqwest::Client::builder()
            .use_rustls_tls()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .user_agent(concat!("helios-hfs/", env!("CARGO_PKG_VERSION")))
            // An archive must arrive byte-for-byte so its shasum checks.
            .no_gzip()
            .build()
            .unwrap_or_default();
        Self {
            mode,
            marketplace_url,
            cache_dir,
            limits,
            http,
            docs: Mutex::new(HashMap::new()),
            manifests: Mutex::new(HashMap::new()),
            market: Mutex::new(None),
        }
    }

    /// Registry mode.
    pub fn mode(&self) -> &RegistryMode {
        &self.mode
    }

    /// The cache directory.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Size limits.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Whether a marketplace index is configured and allowed.
    pub fn marketplace_enabled(&self) -> bool {
        self.marketplace_url.is_some()
    }

    fn cache_file(&self, name: &str, version: &str) -> Option<PathBuf> {
        (valid_package_name(name) && valid_package_version(version))
            .then(|| self.cache_dir.join(format!("{name}#{version}.tgz")))
    }

    /// Versions of `name` present in the cache.
    pub fn cached_versions(&self, name: &str) -> Vec<String> {
        let prefix = format!("{name}#");
        let Ok(dir) = std::fs::read_dir(&self.cache_dir) else {
            return Vec::new();
        };
        dir.filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|f| {
                f.strip_prefix(&prefix)
                    .and_then(|rest| rest.strip_suffix(".tgz"))
                    .filter(|v| valid_package_version(v))
                    .map(str::to_string)
            })
            .collect()
    }

    /// Every cached `name#version`.
    pub fn cached_packages(&self) -> Vec<(String, String)> {
        let Ok(dir) = std::fs::read_dir(&self.cache_dir) else {
            return Vec::new();
        };
        let mut out: Vec<(String, String)> = dir
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|f| {
                let stem = f.strip_suffix(".tgz")?;
                let (n, v) = stem.split_once('#')?;
                (valid_package_name(n) && valid_package_version(v))
                    .then(|| (n.to_string(), v.to_string()))
            })
            .collect();
        out.sort();
        out
    }

    /// Reads a cached archive.
    pub fn read_cached(&self, name: &str, version: &str) -> Option<Vec<u8>> {
        std::fs::read(self.cache_file(name, version)?).ok()
    }

    /// Stores an archive in the cache under its manifest's `name#version`
    /// (atomic rename) and returns the manifest.
    pub fn store(&self, bytes: &[u8]) -> Result<Manifest, String> {
        let manifest = read_manifest(bytes, &self.limits).map_err(|e| e.to_string())?;
        let path = self
            .cache_file(&manifest.name, &manifest.version)
            .ok_or_else(|| "invalid package name or version".to_string())?;
        std::fs::create_dir_all(&self.cache_dir)
            .map_err(|e| format!("cannot create {}: {e}", self.cache_dir.display()))?;
        let tmp = path.with_extension(format!("tgz.{}.tmp", uuid::Uuid::new_v4().simple()));
        std::fs::write(&tmp, bytes).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("cannot store {}: {e}", path.display())
        })?;
        self.manifests
            .lock()
            .expect("manifest memo")
            .insert(manifest.key(), manifest.clone());
        Ok(manifest)
    }

    /// The archive for `name#version`: from the cache, else downloaded
    /// (shasum-checked) and cached.
    pub async fn fetch(&self, name: &str, version: &str) -> Result<(Vec<u8>, FetchInfo), String> {
        if let Some(bytes) = self.read_cached(name, version) {
            return Ok((
                bytes,
                FetchInfo {
                    source: "cache".into(),
                    verified: false,
                },
            ));
        }
        if !self.mode.is_online() {
            return Err(format!(
                "{name}#{version} is not in the package cache and the registry is off \
                 (HFS_PACKAGE_REGISTRY=off): upload the .tgz"
            ));
        }
        if self.cache_file(name, version).is_none() {
            return Err("invalid package name or version".into());
        }
        let mut errors = Vec::new();
        for base in self.mode.sources() {
            let doc = self.doc_from(&base, name).await;
            let listed = doc
                .as_ref()
                .ok()
                .and_then(|d| d.versions.iter().find(|v| v.version == version).cloned());
            let mut urls = Vec::new();
            if let Some(t) = listed.as_ref().and_then(|v| v.tarball.clone()) {
                urls.push(t);
            }
            let direct = format!("{base}/{name}/{version}");
            if !urls.contains(&direct) {
                urls.push(direct);
            }
            let shasum = listed.as_ref().and_then(|v| v.shasum.clone());
            for url in urls {
                match self.download(&url).await {
                    Ok(bytes) => {
                        let actual = sha1_hex(&bytes);
                        if let Some(expected) = &shasum
                            && !expected.eq_ignore_ascii_case(&actual)
                        {
                            errors.push(format!(
                                "{url}: shasum mismatch (registry {expected}, got {actual})"
                            ));
                            continue;
                        }
                        let manifest = read_manifest(&bytes, &self.limits)
                            .map_err(|e| format!("{url}: {e}"))?;
                        if manifest.name != name || compare(&manifest.version, version).is_ne() {
                            errors.push(format!(
                                "{url}: archive is {} not {name}#{version}",
                                manifest.key()
                            ));
                            continue;
                        }
                        self.store(&bytes)?;
                        return Ok((
                            bytes,
                            FetchInfo {
                                source: url,
                                verified: shasum.is_some(),
                            },
                        ));
                    }
                    Err(e) => errors.push(format!("{url}: {e}")),
                }
            }
        }
        Err(format!(
            "could not download {name}#{version}: {}",
            errors.join("; ")
        ))
    }

    async fn download(&self, url: &str) -> Result<Vec<u8>, String> {
        let mut response = self.http.get(url).send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        let limit = self.limits.max_compressed;
        if response.content_length().is_some_and(|len| len > limit) {
            return Err(format!("larger than the {limit}-byte package limit"));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            body.extend_from_slice(&chunk);
            if body.len() as u64 > limit {
                return Err(format!("larger than the {limit}-byte package limit"));
            }
        }
        Ok(body)
    }

    async fn get_json(&self, url: &str) -> Result<Value, String> {
        let response = self
            .http
            .get(url)
            .header("Accept", "application/json")
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err("response too large".into());
        }
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())
    }

    async fn doc_from(&self, base: &str, name: &str) -> Result<RegistryDoc, String> {
        let memo_key = format!("{base}\n{name}");
        if let Some((at, doc)) = self.docs.lock().expect("doc memo").get(&memo_key)
            && at.elapsed() < DOC_TTL
        {
            return Ok(doc.clone());
        }
        let raw = self.get_json(&format!("{base}/{name}")).await?;
        let doc = parse_registry_doc(name, base, &raw)?;
        self.docs
            .lock()
            .expect("doc memo")
            .insert(memo_key, (Instant::now(), doc.clone()));
        Ok(doc)
    }

    /// The registry document for `name` from the first source that knows
    /// it, with cached versions merged in (cache only when offline).
    pub async fn doc(&self, name: &str) -> Result<RegistryDoc, String> {
        let cached = self.cached_versions(name);
        let mut errors = Vec::new();
        let mut doc = None;
        for base in self.mode.sources() {
            match self.doc_from(&base, name).await {
                Ok(d) => {
                    doc = Some(d);
                    break;
                }
                Err(e) => errors.push(format!("{base}: {e}")),
            }
        }
        let mut doc = match doc {
            Some(d) => d,
            None if !cached.is_empty() => RegistryDoc {
                name: name.to_string(),
                source: "cache".into(),
                ..RegistryDoc::default()
            },
            None if !self.mode.is_online() => {
                return Err(format!(
                    "{name} is not in the package cache and the registry is off"
                ));
            }
            None => return Err(format!("{name} not found: {}", errors.join("; "))),
        };
        for v in cached {
            if !doc.versions.iter().any(|r| r.version == v) {
                doc.versions.push(RegistryVersion {
                    version: v,
                    ..RegistryVersion::default()
                });
            }
        }
        doc.versions.sort_by(|a, b| compare(&b.version, &a.version));
        if doc.latest.is_none() {
            doc.latest = doc.versions.first().map(|v| v.version.clone());
        }
        Ok(doc)
    }

    /// Catalog search across the configured registries (first that answers).
    pub async fn search(
        &self,
        query: &str,
        fhir_version: Option<&str>,
    ) -> Result<Vec<CatalogEntry>, String> {
        if !self.mode.is_online() {
            return Err("the package registry is off (HFS_PACKAGE_REGISTRY=off)".into());
        }
        let mut errors = Vec::new();
        for base in self.mode.sources() {
            let mut url = match reqwest::Url::parse(&format!("{base}/catalog")) {
                Ok(u) => u,
                Err(e) => {
                    errors.push(format!("{base}: {e}"));
                    continue;
                }
            };
            url.query_pairs_mut().append_pair("name", query);
            if let Some(fv) = fhir_version {
                url.query_pairs_mut().append_pair("fhirversion", fv);
            }
            match self.get_json(url.as_str()).await {
                Ok(Value::Array(items)) => return Ok(parse_catalog(&items)),
                Ok(_) => errors.push(format!("{base}: unexpected catalog shape")),
                Err(e) => errors.push(format!("{base}: {e}")),
            }
        }
        Err(errors.join("; "))
    }

    fn market_file(&self) -> PathBuf {
        self.cache_dir.join("fhir-ig-list.json")
    }

    /// The marketplace index: memory (a day), then the on-disk copy (a day),
    /// then the network; a stale disk copy is still served when the network
    /// fails, and an offline server serves whatever is on disk.
    pub async fn marketplace(&self) -> Result<Arc<Vec<MarketEntry>>, String> {
        let Some(url) = self.marketplace_url.clone() else {
            return Err("the marketplace index is off (HFS_PACKAGE_MARKETPLACE=off)".into());
        };
        if let Some((at, list)) = self.market.lock().expect("market memo").as_ref()
            && at.elapsed() < MARKET_TTL
        {
            return Ok(Arc::clone(list));
        }
        let file = self.market_file();
        let disk_age = std::fs::metadata(&file)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok());
        let from_disk = || -> Option<Arc<Vec<MarketEntry>>> {
            let raw = std::fs::read(&file).ok()?;
            let doc: Value = serde_json::from_slice(&raw).ok()?;
            Some(Arc::new(parse_marketplace(&doc)))
        };
        if disk_age.is_some_and(|age| age < MARKET_TTL) || !self.mode.is_online() {
            if let Some(list) = from_disk() {
                *self.market.lock().expect("market memo") =
                    Some((Instant::now(), Arc::clone(&list)));
                return Ok(list);
            }
            if !self.mode.is_online() {
                return Err(format!(
                    "the registry is off and no marketplace index is cached at {}",
                    file.display()
                ));
            }
        }
        match self.get_json(&url).await {
            Ok(doc) => {
                let list = Arc::new(parse_marketplace(&doc));
                if list.is_empty() {
                    return Err(format!("{url}: no guides in the marketplace index"));
                }
                if std::fs::create_dir_all(&self.cache_dir).is_ok()
                    && let Ok(raw) = serde_json::to_vec(&doc)
                    && let Err(e) = std::fs::write(&file, raw)
                {
                    warn!(error = %e, "could not cache the marketplace index");
                }
                *self.market.lock().expect("market memo") =
                    Some((Instant::now(), Arc::clone(&list)));
                Ok(list)
            }
            Err(e) => {
                debug!(error = %e, "marketplace fetch failed; trying the disk copy");
                from_disk().ok_or_else(|| format!("{url}: {e}"))
            }
        }
    }
}

fn parse_registry_doc(name: &str, base: &str, raw: &Value) -> Result<RegistryDoc, String> {
    let versions = raw
        .get("versions")
        .and_then(Value::as_object)
        .ok_or_else(|| "no versions in registry document".to_string())?;
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    let mut list: Vec<RegistryVersion> = versions
        .iter()
        .filter(|(v, _)| valid_package_version(v))
        .map(|(v, info)| RegistryVersion {
            version: v.clone(),
            fhir_version: text(info, "fhirVersion"),
            date: text(info, "date"),
            tarball: info
                .pointer("/dist/tarball")
                .and_then(Value::as_str)
                .map(str::to_string),
            shasum: info
                .pointer("/dist/shasum")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
        .collect();
    list.sort_by(|a, b| compare(&b.version, &a.version));
    Ok(RegistryDoc {
        name: name.to_string(),
        description: text(raw, "description"),
        latest: raw
            .pointer("/dist-tags/latest")
            .and_then(Value::as_str)
            .map(str::to_string),
        versions: list,
        source: base.to_string(),
    })
}

/// Parses both catalog shapes: packages.fhir.org (`Name`, `Description`,
/// `FhirVersion`) and packages2 (`name`, `version`, `fhirVersion`, …).
pub fn parse_catalog(items: &[Value]) -> Vec<CatalogEntry> {
    let mut by_name: BTreeMap<String, CatalogEntry> = BTreeMap::new();
    for item in items {
        let get = |keys: &[&str]| {
            keys.iter()
                .find_map(|k| item.get(*k).and_then(Value::as_str))
                .map(str::to_string)
        };
        let Some(name) = get(&["Name", "name"]).filter(|n| valid_package_name(n)) else {
            continue;
        };
        let entry = CatalogEntry {
            name: name.clone(),
            description: get(&["Description", "description"]),
            fhir_version: get(&["FhirVersion", "fhirVersion"]),
            version: get(&["Version", "version"]),
            canonical: get(&["Canonical", "canonical"]),
        };
        by_name
            .entry(name)
            .and_modify(|e| {
                if let (Some(new), Some(old)) = (&entry.version, &e.version)
                    && compare(new, old).is_gt()
                {
                    *e = entry.clone();
                }
            })
            .or_insert(entry);
    }
    by_name.into_values().collect()
}

/// [`PackageSource`] over the registry and cache.
pub struct RegistrySource<'a>(pub &'a Registry);

#[async_trait]
impl PackageSource for RegistrySource<'_> {
    async fn versions(&self, name: &str) -> Result<VersionList, String> {
        let doc = self.0.doc(name).await?;
        Ok(VersionList {
            versions: doc.versions.into_iter().map(|v| v.version).collect(),
            latest: doc.latest,
        })
    }

    async fn manifest(&self, name: &str, version: &str) -> Result<Manifest, String> {
        let key = format!("{name}#{version}");
        if let Some(m) = self.0.manifests.lock().expect("manifest memo").get(&key) {
            return Ok(m.clone());
        }
        let (bytes, _) = self.0.fetch(name, version).await?;
        let manifest = read_manifest(&bytes, &self.0.limits).map_err(|e| e.to_string())?;
        self.0
            .manifests
            .lock()
            .expect("manifest memo")
            .insert(key, manifest.clone());
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_registry_documents_newest_first() {
        let raw = json!({
            "name": "tw.gov.mohw.twcore",
            "dist-tags": {"latest": "1.0.0"},
            "versions": {
                "0.3.2": {"dist": {"shasum": "aa", "tarball": "https://x/0.3.2"}, "fhirVersion": "R4"},
                "1.0.0": {"dist": {"shasum": "bb", "tarball": "https://x/1.0.0"}, "fhirVersion": "R4"},
                "../evil": {}
            }
        });
        let doc = parse_registry_doc("tw.gov.mohw.twcore", "https://r", &raw).unwrap();
        assert_eq!(doc.latest.as_deref(), Some("1.0.0"));
        assert_eq!(
            doc.versions
                .iter()
                .map(|v| v.version.as_str())
                .collect::<Vec<_>>(),
            ["1.0.0", "0.3.2"]
        );
        assert_eq!(doc.versions[0].shasum.as_deref(), Some("bb"));
        assert!(parse_registry_doc("x", "b", &json!({})).is_err());
    }

    #[test]
    fn parses_both_catalog_shapes() {
        let items = vec![
            json!({"Name": "tw.gov.mohw.twcore", "Description": "TW Core", "FhirVersion": "R4"}),
            json!({"name": "tw.gov.mohw.emr", "version": "0.1.0", "fhirVersion": "4.0.1"}),
            json!({"name": "tw.gov.mohw.emr", "version": "0.2.0", "fhirVersion": "4.0.1",
                   "canonical": "https://twcore.mohw.gov.tw/ig/emr"}),
            json!({"name": "bad name"}),
        ];
        let entries = parse_catalog(&items);
        assert_eq!(entries.len(), 2);
        let emr = entries
            .iter()
            .find(|e| e.name == "tw.gov.mohw.emr")
            .unwrap();
        assert_eq!(emr.version.as_deref(), Some("0.2.0"));
        assert_eq!(
            emr.canonical.as_deref(),
            Some("https://twcore.mohw.gov.tw/ig/emr")
        );
    }

    #[test]
    fn parses_the_ig_registry_list() {
        let doc = json!({"guides": [
            {"name": "TW Core", "category": "National Base", "npm-name": "tw.gov.mohw.twcore",
             "description": "Taiwan Core", "authority": "MOHW (TW)", "country": "tw",
             "canonical": "https://twcore.mohw.gov.tw/ig/twcore",
             "editions": [
                {"name": "STU 0.3", "ig-version": "0.3.2", "package": "tw.gov.mohw.twcore#0.3.2", "fhir-version": ["4.0.1"]},
                {"name": "STU 1.0.0", "ig-version": "1.0.0", "package": "tw.gov.mohw.twcore#1.0.0", "fhir-version": ["4.0.1"]}
             ]},
            {"name": "no package"}
        ]});
        let list = parse_marketplace(&doc);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].country, "tw");
        assert_eq!(list[0].latest(), Some("1.0.0"));
        assert_eq!(list[0].editions[1].fhir_versions, ["4.0.1"]);
    }

    #[tokio::test]
    async fn cache_round_trip_and_offline_behaviour() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(
            RegistryMode::Off,
            None,
            dir.path().to_path_buf(),
            Limits::default(),
        );
        let bytes = super::super::tarball::test_support::package("acme.ig", "1.0.0", &[], &[]);
        let manifest = registry.store(&bytes).unwrap();
        assert_eq!(manifest.key(), "acme.ig#1.0.0");
        assert_eq!(registry.cached_versions("acme.ig"), ["1.0.0"]);
        assert_eq!(
            registry.cached_packages(),
            [("acme.ig".to_string(), "1.0.0".to_string())]
        );
        let (fetched, info) = registry.fetch("acme.ig", "1.0.0").await.unwrap();
        assert_eq!(fetched, bytes);
        assert_eq!(info.source, "cache");
        let err = registry.fetch("acme.ig", "2.0.0").await.unwrap_err();
        assert!(err.contains("registry is off"), "{err}");
        let doc = registry.doc("acme.ig").await.unwrap();
        assert_eq!(doc.latest.as_deref(), Some("1.0.0"));
        assert!(registry.doc("unknown.pkg").await.is_err());
        assert!(registry.search("tw", None).await.is_err());
        let source = RegistrySource(&registry);
        assert_eq!(
            source.manifest("acme.ig", "1.0.0").await.unwrap().name,
            "acme.ig"
        );
        // Names that are not file-name safe never reach the filesystem.
        assert!(registry.read_cached("../x", "1").is_none());
    }
}
