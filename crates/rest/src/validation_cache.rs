//! Persisted compiled-profile cache for the validation service.
//!
//! Startup rehydration used to read every stored StructureDefinition twice
//! (an export page for the ids, then `read_batch` for the content) and
//! convert each one to a FHIR Schema before any `$validate` could proceed:
//! 985 SDs (14 IGs) took 4.6–5.1 s on the 48-core test server and 7.4 s on
//! the production host, every restart, with validation blocked meanwhile.
//!
//! The service now keeps an index of the stored StructureDefinitions it has
//! folded into its registries — tenant, id, `meta.versionId`,
//! `meta.lastUpdated`, and the compiled schema — and writes it to
//! `<dir>/profiles.json.gz`. On the next start the file is loaded first
//! (registries are complete within a few hundred milliseconds, validation
//! stops waiting), then the database is reconciled in the background: rows
//! whose stamp still matches are kept as they are, changed or new rows are
//! read and converted (in parallel), rows that disappeared are dropped.
//!
//! The file is only an accelerator, never a source of truth:
//! - it is keyed by a build fingerprint (package version, git commit, and
//!   the size and mtime of the running executable) plus the
//!   `HFS_VALIDATION_SHADOW_CORE` setting, so a new build — whose converter
//!   or embedded core pack may differ — ignores an old file and rebuilds it;
//! - every start reconciles against the database, so a stale entry lives at
//!   most until that pass reaches its row;
//! - any read or parse failure falls back to a full rehydration.

use helios_fhir::FhirVersion;
use helios_fhir_validator::FhirSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Bumped whenever the on-disk layout changes.
const FORMAT: u32 = 1;
/// File name inside the cache directory.
const FILE_NAME: &str = "profiles.json.gz";

/// The row identity a stored StructureDefinition is matched on: its id and
/// the `meta.versionId` / `meta.lastUpdated` every backend stamps on write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RowStamp {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_updated: Option<String>,
}

impl RowStamp {
    /// The stamp of a resource body; `None` without an `id`.
    pub fn of(content: &Value) -> Option<Self> {
        let id = content.get("id")?.as_str()?.to_string();
        let meta = content.get("meta");
        let field = |key: &str| {
            meta.and_then(|m| m.get(key))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        Some(Self {
            id,
            version_id: field("versionId"),
            last_updated: field("lastUpdated"),
        })
    }

    /// The stamp of one exported NDJSON line, without building a full
    /// `Value` tree (unknown fields are skipped by the deserializer).
    pub fn of_line(line: &str) -> Option<Self> {
        #[derive(Deserialize)]
        struct Meta {
            #[serde(rename = "versionId")]
            version_id: Option<String>,
            #[serde(rename = "lastUpdated")]
            last_updated: Option<String>,
        }
        #[derive(Deserialize)]
        struct Light {
            id: String,
            meta: Option<Meta>,
        }
        let light: Light = serde_json::from_str(line).ok()?;
        let (version_id, last_updated) = match light.meta {
            Some(m) => (m.version_id, m.last_updated),
            None => (None, None),
        };
        Some(Self {
            id: light.id,
            version_id,
            last_updated,
        })
    }
}

/// One stored StructureDefinition as the service knows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredProfile {
    pub tenant: String,
    pub fhir_version: FhirVersion,
    pub stamp: RowStamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The compiled schema; `None` for a definition that was read but not
    /// registered (conversion failure, or a core canonical while core
    /// shadowing is off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<FhirSchema>,
    /// Registration order (later wins a shared URL); persisted as the
    /// order of the file's entries.
    #[serde(skip)]
    pub seq: u64,
    /// Seen in the database by the running reconciliation pass.
    #[serde(skip)]
    pub confirmed: bool,
    /// Written through the REST/package write path since this process
    /// started: newer than anything a reconciliation pass could read.
    #[serde(skip)]
    pub live: bool,
}

/// The index of stored StructureDefinitions, keyed by `(tenant, id)`.
#[derive(Debug, Default)]
pub(crate) struct StoredIndex {
    pub entries: HashMap<(String, String), StoredProfile>,
    next_seq: u64,
}

impl StoredIndex {
    /// Insert or replace an entry, assigning it the next registration slot.
    pub fn put(&mut self, mut profile: StoredProfile) {
        profile.seq = self.next_seq;
        self.next_seq += 1;
        self.entries
            .insert((profile.tenant.clone(), profile.stamp.id.clone()), profile);
    }

    /// Entries in registration order (what the cache file stores).
    pub fn ordered(&self) -> Vec<StoredProfile> {
        let mut all: Vec<StoredProfile> = self.entries.values().cloned().collect();
        all.sort_by_key(|p| p.seq);
        all
    }
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    format: u32,
    fingerprint: String,
    entries: Vec<StoredProfile>,
}

/// Where the compiled-profile cache lives and which build it belongs to.
#[derive(Debug, Clone)]
pub struct ProfileCache {
    path: PathBuf,
    fingerprint: String,
}

impl ProfileCache {
    /// A cache under `dir` for this build and `shadow_core` setting. `None`
    /// when the build cannot be fingerprinted (no readable executable).
    pub fn new(dir: &Path, shadow_core: bool) -> Option<Self> {
        let exe = std::env::current_exe().ok()?;
        let meta = std::fs::metadata(&exe).ok()?;
        let mtime = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos();
        let fingerprint = format!(
            "{}|{}|{}|{}|shadow-core={}",
            crate::build_info::PKG_VERSION,
            crate::build_info::git_sha().unwrap_or("-"),
            meta.len(),
            mtime,
            shadow_core
        );
        Some(Self::with_fingerprint(dir, fingerprint))
    }

    /// A cache with an explicit fingerprint (tests).
    pub fn with_fingerprint(dir: &Path, fingerprint: impl Into<String>) -> Self {
        Self {
            path: dir.join(FILE_NAME),
            fingerprint: fingerprint.into(),
        }
    }

    /// The cache file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the file. `Ok(None)` when there is none, or it belongs to
    /// another build or format; `Err` when it is unreadable.
    pub(crate) fn load(&self) -> Result<Option<Vec<StoredProfile>>, String> {
        let file = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", self.path.display())),
        };
        let mut json = Vec::new();
        flate2::read::GzDecoder::new(std::io::BufReader::new(file))
            .read_to_end(&mut json)
            .map_err(|e| format!("{}: {e}", self.path.display()))?;
        #[derive(Deserialize)]
        struct Header {
            format: u32,
            fingerprint: String,
        }
        // Check the header before paying for the entries.
        let header: Header =
            serde_json::from_slice(&json).map_err(|e| format!("{}: {e}", self.path.display()))?;
        if header.format != FORMAT || header.fingerprint != self.fingerprint {
            return Ok(None);
        }
        let file: CacheFile =
            serde_json::from_slice(&json).map_err(|e| format!("{}: {e}", self.path.display()))?;
        Ok(Some(file.entries))
    }

    /// Write the file atomically (temporary file + rename).
    pub(crate) fn save(&self, entries: Vec<StoredProfile>) -> std::io::Result<usize> {
        let count = entries.len();
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
            remove_stale_temporaries(dir);
        }
        let tmp = self.path.with_extension(format!("tmp.{}", std::process::id()));
        let result = (|| {
            let file = std::fs::File::create(&tmp)?;
            let mut gz = flate2::write::GzEncoder::new(
                std::io::BufWriter::new(file),
                flate2::Compression::fast(),
            );
            serde_json::to_writer(
                &mut gz,
                &CacheFile {
                    format: FORMAT,
                    fingerprint: self.fingerprint.clone(),
                    entries,
                },
            )
            .map_err(std::io::Error::other)?;
            gz.finish()?.flush()?;
            std::fs::rename(&tmp, &self.path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.map(|()| count)
    }
}

/// Drop temporary files a killed writer left behind (`profiles.json.tmp.<pid>`
/// older than a minute; a concurrent writer's fresh one is left alone).
fn remove_stale_temporaries(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("profiles.json.tmp.") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > std::time::Duration::from_secs(60));
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The cache directory for a server configuration
/// (`HFS_VALIDATION_PROFILE_CACHE`): `off`/`false`/empty disables it, a path
/// selects it; unset, it sits next to the package cache — beside a SQLite
/// database file, or under `HFS_DATA_DIR` / the `/data` volume — and is off
/// when there is no such place (in-memory databases, tests).
pub fn cache_dir(config: &crate::config::ServerConfig) -> Option<PathBuf> {
    match config.validation.profile_cache.as_deref().map(str::trim) {
        Some("off" | "false" | "0" | "") => return None,
        Some(path) => return Some(PathBuf::from(path)),
        None => {}
    }
    let sqlite_file = config.storage_backend.starts_with("sqlite")
        && config
            .database_url
            .as_deref()
            .is_some_and(|url| !url.contains("://") && url != ":memory:" && !url.is_empty());
    let has_volume = config.packages.cache_dir.is_some()
        || config.data_dir.is_some()
        || (!config.storage_backend.starts_with("sqlite") && Path::new("/data").is_dir());
    if !sqlite_file && !has_volume {
        return None;
    }
    let packages = crate::packages::manager::cache_dir(config);
    Some(
        packages
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("validation-cache"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn profile(id: &str, version: &str) -> StoredProfile {
        let sd = json!({
            "resourceType": "StructureDefinition",
            "url": format!("http://example.org/StructureDefinition/{id}"),
            "name": id,
            "type": "Patient",
            "kind": "resource",
            "derivation": "constraint",
            "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
            "differential": {"element": [
                {"id": "Patient", "path": "Patient"},
                {"id": "Patient.birthDate", "path": "Patient.birthDate", "min": 1}
            ]}
        });
        StoredProfile {
            tenant: "t".into(),
            fhir_version: FhirVersion::R4,
            stamp: RowStamp {
                id: id.into(),
                version_id: Some(version.into()),
                last_updated: Some("2026-10-16T00:00:00Z".into()),
            },
            url: sd["url"].as_str().map(str::to_string),
            schema: Some(
                helios_fhir_validator::converter::convert(&sd)
                    .expect("converts")
                    .schema,
            ),
            seq: 0,
            confirmed: false,
            live: false,
        }
    }

    #[test]
    fn stamps_come_from_id_and_meta() {
        let body = json!({"resourceType": "StructureDefinition", "id": "a",
            "meta": {"versionId": "3", "lastUpdated": "2026-10-16T01:02:03Z"}, "url": "x"});
        let stamp = RowStamp::of(&body).expect("stamp");
        assert_eq!(stamp.version_id.as_deref(), Some("3"));
        assert_eq!(RowStamp::of_line(&body.to_string()), Some(stamp));
        assert_eq!(RowStamp::of(&json!({"resourceType": "X"})), None);
        assert_eq!(
            RowStamp::of_line(r#"{"id":"b"}"#).map(|s| s.version_id),
            Some(None)
        );
    }

    #[test]
    fn round_trips_schemas_in_registration_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ProfileCache::with_fingerprint(dir.path(), "build-1");
        assert!(cache.load().expect("no file").is_none());

        let mut index = StoredIndex::default();
        index.put(profile("b", "1"));
        index.put(profile("a", "2"));
        let written = index.ordered();
        cache.save(written.clone()).expect("save");

        let read = cache.load().expect("read").expect("same build");
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].stamp.id, "b", "registration order is kept");
        for (r, w) in read.iter().zip(&written) {
            assert_eq!(r.stamp, w.stamp);
            assert_eq!(r.schema, w.schema, "schemas round-trip losslessly");
        }

        // Another build ignores the file.
        let other = ProfileCache::with_fingerprint(dir.path(), "build-2");
        assert!(other.load().expect("readable").is_none());
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ProfileCache::with_fingerprint(dir.path(), "b");
        std::fs::write(cache.path(), b"not gzip").expect("write");
        assert!(cache.load().is_err());
    }
}
