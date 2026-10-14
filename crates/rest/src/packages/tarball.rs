//! Reading FHIR npm packages (`.tgz`) as untrusted data.
//!
//! A package is a gzip-compressed tar archive with a `package/` folder:
//! `package/package.json` (the manifest), one JSON file per conformance
//! resource directly under `package/`, examples under `package/example/`,
//! and assorted tooling output (`package/other/`, `package/xml/`,
//! `package/openapi/`, `.index.json`) that is not FHIR content.
//!
//! Nothing here touches the filesystem: entries are read from memory and
//! never extracted, so a hostile archive cannot write anywhere. It is still
//! rejected outright when an entry path tries to escape the archive root
//! (`..`, an absolute path) — such a package is not a tooling accident — and
//! every size is bounded: the compressed archive, each entry, the running
//! unpacked total (counted from tar headers, so a gzip bomb is stopped
//! before it is inflated past the limit), and the number of entries. Links
//! and other non-regular entries are skipped.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Component, Path};

use flate2::read::GzDecoder;
use serde::Deserialize;
use serde_json::Value;
use sha2::Digest;

/// Size bounds applied while reading a package.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Largest accepted archive (compressed bytes).
    pub max_compressed: u64,
    /// Largest total of all entry sizes.
    pub max_unpacked: u64,
    /// Largest single entry.
    pub max_entry: u64,
    /// Most entries.
    pub max_entries: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_compressed: 256 * 1024 * 1024,
            max_unpacked: 1024 * 1024 * 1024,
            max_entry: 64 * 1024 * 1024,
            max_entries: 100_000,
        }
    }
}

/// Why a package could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PackageError {
    /// The archive is larger than [`Limits::max_compressed`].
    #[error("package is {size} bytes; the limit is {limit} bytes")]
    TooLarge {
        /// Archive size.
        size: u64,
        /// Configured limit.
        limit: u64,
    },
    /// The unpacked content exceeds [`Limits::max_unpacked`].
    #[error("package unpacks to more than {limit} bytes")]
    UnpackedTooLarge {
        /// Configured limit.
        limit: u64,
    },
    /// One entry exceeds [`Limits::max_entry`].
    #[error("package entry '{path}' is {size} bytes; the limit is {limit} bytes")]
    EntryTooLarge {
        /// Entry path.
        path: String,
        /// Declared size.
        size: u64,
        /// Configured limit.
        limit: u64,
    },
    /// More entries than [`Limits::max_entries`].
    #[error("package has more than {limit} entries")]
    TooManyEntries {
        /// Configured limit.
        limit: usize,
    },
    /// An entry path escapes the archive root.
    #[error("package entry '{0}' has an unsafe path (absolute or containing '..')")]
    UnsafePath(String),
    /// Not a gzip/tar archive, or a corrupt one.
    #[error("not a readable .tgz package: {0}")]
    Archive(String),
    /// `package/package.json` is missing.
    #[error("package has no package/package.json manifest")]
    MissingManifest,
    /// The manifest is not usable.
    #[error("invalid package.json: {0}")]
    BadManifest(String),
}

/// The parts of `package.json` the manager uses.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    /// Package name (validated by [`valid_package_name`]).
    pub name: String,
    /// Package version (validated by [`valid_package_version`]).
    pub version: String,
    /// `fhirVersions` (or the legacy `fhir-version-list` / `fhirVersion`).
    pub fhir_versions: Vec<String>,
    /// `dependencies`: package name → version or range.
    pub dependencies: BTreeMap<String, String>,
    /// `title`, when given.
    pub title: Option<String>,
    /// `description`, when given.
    pub description: Option<String>,
    /// `canonical`, when given.
    pub canonical: Option<String>,
    /// `url` (the IG's home page), when given.
    pub url: Option<String>,
    /// `type` (`IG`, `Core`, `fhir.ig`, …), when given.
    pub kind: Option<String>,
    /// `date`, when given.
    pub date: Option<String>,
    /// `author`, when given as a string.
    pub author: Option<String>,
    /// `license`, when given.
    pub license: Option<String>,
}

impl Manifest {
    /// Parse and validate a manifest document.
    pub fn from_json(raw: &[u8]) -> Result<Self, PackageError> {
        let doc: Value =
            serde_json::from_slice(raw).map_err(|e| PackageError::BadManifest(e.to_string()))?;
        let text = |key: &str| doc.get(key).and_then(Value::as_str).map(str::to_string);
        let name = text("name").ok_or_else(|| PackageError::BadManifest("no name".into()))?;
        let version =
            text("version").ok_or_else(|| PackageError::BadManifest("no version".into()))?;
        if !valid_package_name(&name) {
            return Err(PackageError::BadManifest(format!(
                "unsupported package name '{name}'"
            )));
        }
        if !valid_package_version(&version) {
            return Err(PackageError::BadManifest(format!(
                "unsupported package version '{version}'"
            )));
        }
        let mut fhir_versions: Vec<String> = Vec::new();
        for key in ["fhirVersions", "fhir-version-list"] {
            if let Some(list) = doc.get(key).and_then(Value::as_array) {
                fhir_versions.extend(list.iter().filter_map(Value::as_str).map(str::to_string));
            }
        }
        if fhir_versions.is_empty()
            && let Some(v) = text("fhirVersion")
        {
            fhir_versions.push(v);
        }
        let mut dependencies = BTreeMap::new();
        if let Some(map) = doc.get("dependencies").and_then(Value::as_object) {
            for (dep, range) in map {
                let Some(range) = range.as_str() else {
                    continue;
                };
                if valid_package_name(dep) {
                    dependencies.insert(dep.clone(), range.trim().to_string());
                }
            }
        }
        Ok(Self {
            name,
            version,
            fhir_versions,
            dependencies,
            title: text("title"),
            description: text("description"),
            canonical: text("canonical"),
            url: text("url"),
            kind: text("type"),
            date: text("date"),
            author: text("author"),
            license: text("license"),
        })
    }

    /// `name#version`.
    pub fn key(&self) -> String {
        format!("{}#{}", self.name, self.version)
    }
}

/// npm-style package names as FHIR uses them: ASCII letters, digits, `.`,
/// `-`, `_`, starting with a letter or digit, at most 214 characters. These
/// names become file names in the package cache, so nothing else is allowed.
pub fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 214
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// Package versions: ASCII letters, digits, `.`, `-`, `+`, `_`, at most 64
/// characters, starting with a letter or digit (also used in file names).
pub fn valid_package_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 64
        && version.as_bytes()[0].is_ascii_alphanumeric()
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+' | b'_'))
}

/// One FHIR resource found in a package.
#[derive(Debug, Clone)]
pub struct PackageEntry {
    /// Path inside the archive.
    pub path: String,
    /// `resourceType`.
    pub resource_type: String,
    /// `id`, when present.
    pub id: Option<String>,
    /// `url`, when present.
    pub url: Option<String>,
    /// `version`, when present.
    pub version: Option<String>,
    /// From an example folder, or declared an example by the package's
    /// ImplementationGuide.
    pub example: bool,
    /// The raw JSON (parsed on demand, so a large terminology package is not
    /// held in memory twice).
    pub bytes: Vec<u8>,
}

impl PackageEntry {
    /// The resource as JSON.
    pub fn json(&self) -> Result<Value, String> {
        serde_json::from_slice(&self.bytes).map_err(|e| e.to_string())
    }
}

/// A read package.
#[derive(Debug, Clone)]
pub struct ParsedPackage {
    /// The manifest.
    pub manifest: Manifest,
    /// Every FHIR resource, in archive order.
    pub entries: Vec<PackageEntry>,
    /// Files that looked like resources but could not be read (path, reason).
    pub skipped: Vec<(String, String)>,
    /// Lower-case hex SHA-1 of the archive (the npm `shasum`).
    pub sha1: String,
    /// Lower-case hex SHA-256 of the archive.
    pub sha256: String,
    /// Archive size in bytes.
    pub size: u64,
}

impl ParsedPackage {
    /// Resource counts by type, examples counted separately.
    pub fn counts(&self) -> (BTreeMap<String, u64>, BTreeMap<String, u64>) {
        let mut resources = BTreeMap::new();
        let mut examples = BTreeMap::new();
        for e in &self.entries {
            let map = if e.example {
                &mut examples
            } else {
                &mut resources
            };
            *map.entry(e.resource_type.clone()).or_insert(0) += 1;
        }
        (resources, examples)
    }
}

/// Lower-case hex SHA-1 of `bytes`.
pub fn sha1_hex(bytes: &[u8]) -> String {
    use sha1::Digest as _;
    hex(&sha1::Sha1::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Just enough of a resource to classify it.
#[derive(Deserialize)]
struct Head {
    #[serde(rename = "resourceType")]
    resource_type: Option<String>,
    id: Option<Value>,
    url: Option<Value>,
    version: Option<Value>,
}

/// Checks a tar entry path and returns it as a `/`-joined string, rejecting
/// anything that could address outside the archive root.
fn safe_path(path: &Path) -> Result<String, PackageError> {
    let shown = path.to_string_lossy().to_string();
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(PackageError::UnsafePath(shown));
            }
        }
    }
    let joined = parts.join("/");
    if joined.contains('\\') || joined.contains('\0') {
        return Err(PackageError::UnsafePath(shown));
    }
    Ok(joined)
}

/// Where a file sits in the package, for classification.
#[derive(Debug, PartialEq, Eq)]
enum Placement {
    Manifest,
    Resource,
    Example,
    Ignored,
}

fn placement(path: &str) -> Placement {
    let Some(rest) = path.strip_prefix("package/") else {
        return Placement::Ignored;
    };
    if rest == "package.json" {
        return Placement::Manifest;
    }
    let (dir, file) = match rest.rsplit_once('/') {
        Some((dir, file)) => (Some(dir), file),
        None => (None, rest),
    };
    if !file.ends_with(".json") || file.starts_with('.') {
        return Placement::Ignored;
    }
    if matches!(
        file,
        "package.json" | "validation-summary.json" | "validation-oo.json" | "spec.internals"
    ) {
        return Placement::Ignored;
    }
    match dir {
        None => Placement::Resource,
        Some("example") | Some("examples") => Placement::Example,
        Some(_) => Placement::Ignored,
    }
}

fn text(v: &Option<Value>) -> Option<String> {
    v.as_ref()
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Reads a package archive. Accepts a gzip-compressed tar (`.tgz`) or, for
/// a transport that already inflated it, a plain tar.
pub fn parse_package(bytes: &[u8], limits: &Limits) -> Result<ParsedPackage, PackageError> {
    let size = bytes.len() as u64;
    if size > limits.max_compressed {
        return Err(PackageError::TooLarge {
            size,
            limit: limits.max_compressed,
        });
    }
    let sha1 = sha1_hex(bytes);
    let sha256 = hex(&sha2::Sha256::digest(bytes));

    let gzip = bytes.starts_with(&[0x1f, 0x8b]);
    let reader: Box<dyn Read + '_> = if gzip {
        Box::new(GzDecoder::new(bytes))
    } else if bytes.len() > 262 && &bytes[257..262] == b"ustar" {
        Box::new(bytes)
    } else {
        return Err(PackageError::Archive("not a gzip or tar file".into()));
    };
    let mut archive = tar::Archive::new(reader);
    let entries = archive
        .entries()
        .map_err(|e| PackageError::Archive(e.to_string()))?;

    let mut manifest_raw: Option<Vec<u8>> = None;
    let mut found: Vec<(Placement, PackageEntry)> = Vec::new();
    let mut skipped = Vec::new();
    let mut total: u64 = 0;
    let mut count = 0usize;

    for entry in entries {
        let mut entry = entry.map_err(|e| PackageError::Archive(e.to_string()))?;
        count += 1;
        if count > limits.max_entries {
            return Err(PackageError::TooManyEntries {
                limit: limits.max_entries,
            });
        }
        let raw_path = entry
            .path()
            .map_err(|e| PackageError::Archive(e.to_string()))?
            .into_owned();
        let path = safe_path(&raw_path)?;
        let declared = entry.header().size().unwrap_or(0);
        total = total.saturating_add(declared);
        if total > limits.max_unpacked {
            return Err(PackageError::UnpackedTooLarge {
                limit: limits.max_unpacked,
            });
        }
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let place = placement(&path);
        if place == Placement::Ignored {
            continue;
        }
        if declared > limits.max_entry {
            return Err(PackageError::EntryTooLarge {
                path,
                size: declared,
                limit: limits.max_entry,
            });
        }
        let mut buf = Vec::with_capacity(declared as usize);
        (&mut entry)
            .take(limits.max_entry + 1)
            .read_to_end(&mut buf)
            .map_err(|e| PackageError::Archive(e.to_string()))?;
        if buf.len() as u64 > limits.max_entry {
            return Err(PackageError::EntryTooLarge {
                path,
                size: buf.len() as u64,
                limit: limits.max_entry,
            });
        }
        if place == Placement::Manifest {
            manifest_raw = Some(buf);
            continue;
        }
        let head: Head = match serde_json::from_slice(&buf) {
            Ok(head) => head,
            Err(e) => {
                skipped.push((path, format!("not JSON: {e}")));
                continue;
            }
        };
        let Some(resource_type) = head
            .resource_type
            .filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_alphanumeric()))
        else {
            skipped.push((path, "no resourceType".into()));
            continue;
        };
        found.push((
            place,
            PackageEntry {
                path,
                resource_type,
                id: text(&head.id),
                url: text(&head.url),
                version: text(&head.version),
                example: false,
                bytes: buf,
            },
        ));
    }

    let manifest = Manifest::from_json(&manifest_raw.ok_or(PackageError::MissingManifest)?)?;

    // Examples the ImplementationGuide declares (exampleBoolean /
    // exampleCanonical) count as examples wherever they sit.
    let mut declared_examples: HashSet<String> = HashSet::new();
    for (_, e) in found
        .iter()
        .filter(|(_, e)| e.resource_type == "ImplementationGuide")
    {
        let Ok(ig) = e.json() else { continue };
        let resources = ig
            .pointer("/definition/resource")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for r in resources {
            let is_example = r.get("exampleBoolean").and_then(Value::as_bool) == Some(true)
                || r.get("exampleCanonical").is_some();
            if let (true, Some(reference)) = (
                is_example,
                r.pointer("/reference/reference").and_then(Value::as_str),
            ) {
                declared_examples.insert(reference.to_string());
            }
        }
    }

    let entries = found
        .into_iter()
        .map(|(place, mut e)| {
            let reference =
                e.id.as_ref()
                    .map(|id| format!("{}/{}", e.resource_type, id));
            e.example = place == Placement::Example
                || reference.is_some_and(|r| declared_examples.contains(&r));
            e
        })
        .collect();

    Ok(ParsedPackage {
        manifest,
        entries,
        skipped,
        sha1,
        sha256,
        size,
    })
}

/// Reads only `package/package.json` from an archive, stopping at it (the
/// manifest is normally the first member). Same path and size checks as
/// [`parse_package`] for everything read up to that point.
pub fn read_manifest(bytes: &[u8], limits: &Limits) -> Result<Manifest, PackageError> {
    if bytes.len() as u64 > limits.max_compressed {
        return Err(PackageError::TooLarge {
            size: bytes.len() as u64,
            limit: limits.max_compressed,
        });
    }
    let reader: Box<dyn Read + '_> = if bytes.starts_with(&[0x1f, 0x8b]) {
        Box::new(GzDecoder::new(bytes))
    } else if bytes.len() > 262 && &bytes[257..262] == b"ustar" {
        Box::new(bytes)
    } else {
        return Err(PackageError::Archive("not a gzip or tar file".into()));
    };
    let mut archive = tar::Archive::new(reader);
    let entries = archive
        .entries()
        .map_err(|e| PackageError::Archive(e.to_string()))?;
    let mut total: u64 = 0;
    for (count, entry) in entries.enumerate() {
        let mut entry = entry.map_err(|e| PackageError::Archive(e.to_string()))?;
        if count >= limits.max_entries {
            return Err(PackageError::TooManyEntries {
                limit: limits.max_entries,
            });
        }
        let path = safe_path(
            &entry
                .path()
                .map_err(|e| PackageError::Archive(e.to_string()))?,
        )?;
        let declared = entry.header().size().unwrap_or(0);
        total = total.saturating_add(declared);
        if total > limits.max_unpacked {
            return Err(PackageError::UnpackedTooLarge {
                limit: limits.max_unpacked,
            });
        }
        if path != "package/package.json" || !entry.header().entry_type().is_file() {
            continue;
        }
        if declared > limits.max_entry {
            return Err(PackageError::EntryTooLarge {
                path,
                size: declared,
                limit: limits.max_entry,
            });
        }
        let mut buf = Vec::new();
        (&mut entry)
            .take(limits.max_entry)
            .read_to_end(&mut buf)
            .map_err(|e| PackageError::Archive(e.to_string()))?;
        return Manifest::from_json(&buf);
    }
    Err(PackageError::MissingManifest)
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Builds `.tgz` archives in memory for tests.
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;

    /// One archive member: path, content, tar entry type.
    pub struct Member<'a> {
        pub path: &'a str,
        pub body: &'a [u8],
        pub kind: tar::EntryType,
    }

    impl<'a> Member<'a> {
        pub fn file(path: &'a str, body: &'a [u8]) -> Self {
            Self {
                path,
                body,
                kind: tar::EntryType::Regular,
            }
        }
    }

    /// Writes members into a tar, bypassing the `tar` crate's own path
    /// sanitising so hostile names (`../x`) can be produced.
    pub fn tar_bytes(members: &[Member<'_>]) -> Vec<u8> {
        let mut out = Vec::new();
        for m in members {
            let mut header = [0u8; 512];
            let name = m.path.as_bytes();
            header[..name.len()].copy_from_slice(name);
            header[100..108].copy_from_slice(b"0000644\0");
            header[108..116].copy_from_slice(b"0000000\0");
            header[116..124].copy_from_slice(b"0000000\0");
            let size = format!("{:011o}\0", m.body.len());
            header[124..136].copy_from_slice(size.as_bytes());
            header[136..148].copy_from_slice(b"00000000000\0");
            header[156] = m.kind.as_byte();
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            header[148..156].copy_from_slice(b"        ");
            let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
            let chk = format!("{sum:06o}\0 ");
            header[148..156].copy_from_slice(chk.as_bytes());
            out.extend_from_slice(&header);
            out.extend_from_slice(m.body);
            let pad = (512 - m.body.len() % 512) % 512;
            out.extend(std::iter::repeat_n(0u8, pad));
        }
        out.extend(std::iter::repeat_n(0u8, 1024));
        out
    }

    pub fn tgz(members: &[Member<'_>]) -> Vec<u8> {
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        gz.write_all(&tar_bytes(members)).unwrap();
        gz.finish().unwrap()
    }

    /// A minimal valid package with the given manifest dependencies and
    /// resources (`(file name under package/, json)`).
    pub fn package(
        name: &str,
        version: &str,
        deps: &[(&str, &str)],
        resources: &[(&str, &str)],
    ) -> Vec<u8> {
        let deps: serde_json::Map<String, serde_json::Value> = deps
            .iter()
            .map(|(n, v)| (n.to_string(), serde_json::Value::from(*v)))
            .collect();
        let manifest = serde_json::json!({
            "name": name, "version": version, "fhirVersions": ["4.0.1"],
            "dependencies": deps, "title": format!("{name} title"),
            "canonical": format!("http://example.org/{name}")
        })
        .to_string();
        let mut members = vec![Member::file("package/package.json", manifest.as_bytes())];
        let paths: Vec<String> = resources
            .iter()
            .map(|(file, _)| format!("package/{file}"))
            .collect();
        for (path, (_, body)) in paths.iter().zip(resources) {
            members.push(Member::file(path, body.as_bytes()));
        }
        tgz(&members)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    const SD: &str =
        r#"{"resourceType":"StructureDefinition","id":"p","url":"http://x/p","version":"1.0.0"}"#;

    #[test]
    fn reads_manifest_resources_and_examples() {
        let ig = r#"{"resourceType":"ImplementationGuide","id":"ig","definition":{"resource":[
            {"reference":{"reference":"Patient/declared"},"exampleBoolean":true}]}}"#;
        let bytes = package(
            "acme.core",
            "1.2.0",
            &[("hl7.fhir.r4.core", "4.0.1")],
            &[
                ("StructureDefinition-p.json", SD),
                ("ImplementationGuide-ig.json", ig),
                (
                    "Patient-declared.json",
                    r#"{"resourceType":"Patient","id":"declared"}"#,
                ),
                (
                    "example/Patient-ex.json",
                    r#"{"resourceType":"Patient","id":"ex"}"#,
                ),
                ("other/thing.json", r#"{"resourceType":"Basic","id":"no"}"#),
                ("xml/StructureDefinition-p.json", SD),
                (".index.json", r#"{"files":[]}"#),
                ("broken.json", "{not json"),
                ("notes.txt", "hello"),
            ],
        );
        let p = parse_package(&bytes, &Limits::default()).unwrap();
        assert_eq!(p.manifest.name, "acme.core");
        assert_eq!(p.manifest.version, "1.2.0");
        assert_eq!(p.manifest.fhir_versions, ["4.0.1"]);
        assert_eq!(p.manifest.dependencies["hl7.fhir.r4.core"], "4.0.1");
        let names: Vec<_> = p.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            names,
            [
                "package/StructureDefinition-p.json",
                "package/ImplementationGuide-ig.json",
                "package/Patient-declared.json",
                "package/example/Patient-ex.json"
            ]
        );
        let examples: Vec<_> = p
            .entries
            .iter()
            .filter(|e| e.example)
            .map(|e| e.id.clone().unwrap())
            .collect();
        assert_eq!(examples, ["declared", "ex"]);
        assert_eq!(p.entries[0].url.as_deref(), Some("http://x/p"));
        assert_eq!(p.skipped.len(), 1, "{:?}", p.skipped);
        assert_eq!(p.sha1, sha1_hex(&bytes));
        let (res, ex) = p.counts();
        assert_eq!(res["StructureDefinition"], 1);
        assert_eq!(ex["Patient"], 2);
    }

    #[test]
    fn rejects_parent_directory_traversal() {
        let bytes = tgz(&[
            Member::file("package/package.json", br#"{"name":"a","version":"1"}"#),
            Member::file("package/../../etc/passwd.json", b"{}"),
        ]);
        let err = parse_package(&bytes, &Limits::default()).unwrap_err();
        assert!(matches!(err, PackageError::UnsafePath(p) if p.contains("..")));
    }

    #[test]
    fn rejects_absolute_paths() {
        let bytes = tgz(&[Member::file("/package/package.json", b"{}")]);
        assert!(matches!(
            parse_package(&bytes, &Limits::default()),
            Err(PackageError::UnsafePath(_))
        ));
    }

    #[test]
    fn skips_links_instead_of_following_them() {
        let bytes = tgz(&[
            Member::file("package/package.json", br#"{"name":"a","version":"1"}"#),
            Member {
                path: "package/Link.json",
                body: b"",
                kind: tar::EntryType::Symlink,
            },
        ]);
        let p = parse_package(&bytes, &Limits::default()).unwrap();
        assert!(p.entries.is_empty());
    }

    #[test]
    fn rejects_oversize_archive_entry_and_total() {
        let bytes = package("a", "1", &[], &[("StructureDefinition-p.json", SD)]);
        let tight = Limits {
            max_compressed: 10,
            ..Limits::default()
        };
        assert!(matches!(
            parse_package(&bytes, &tight),
            Err(PackageError::TooLarge { .. })
        ));
        let tight = Limits {
            max_entry: 20,
            ..Limits::default()
        };
        assert!(matches!(
            parse_package(&bytes, &tight),
            Err(PackageError::EntryTooLarge { .. })
        ));
        let tight = Limits {
            max_unpacked: 100,
            ..Limits::default()
        };
        assert!(matches!(
            parse_package(&bytes, &tight),
            Err(PackageError::UnpackedTooLarge { .. })
        ));
        let tight = Limits {
            max_entries: 1,
            ..Limits::default()
        };
        assert!(matches!(
            parse_package(&bytes, &tight),
            Err(PackageError::TooManyEntries { .. })
        ));
    }

    #[test]
    fn gzip_bomb_is_stopped_by_declared_sizes() {
        // 8 MiB of zeros compresses to a few KiB; the declared size alone
        // trips the unpacked limit before the body is inflated.
        let zeros = vec![0u8; 8 * 1024 * 1024];
        let bytes = tgz(&[
            Member::file("package/package.json", br#"{"name":"a","version":"1"}"#),
            Member::file("package/other/blob.bin", &zeros),
        ]);
        assert!(bytes.len() < 512 * 1024, "{}", bytes.len());
        let tight = Limits {
            max_unpacked: 1024 * 1024,
            ..Limits::default()
        };
        assert!(matches!(
            parse_package(&bytes, &tight),
            Err(PackageError::UnpackedTooLarge { .. })
        ));
    }

    #[test]
    fn requires_a_valid_manifest() {
        let bytes = tgz(&[Member::file(
            "package/StructureDefinition-p.json",
            SD.as_bytes(),
        )]);
        assert_eq!(
            parse_package(&bytes, &Limits::default()).unwrap_err(),
            PackageError::MissingManifest
        );
        for manifest in [
            r#"{"version":"1"}"#,
            r#"{"name":"../evil","version":"1"}"#,
            r#"{"name":"ok","version":"1/2"}"#,
            "not json",
        ] {
            let bytes = tgz(&[Member::file("package/package.json", manifest.as_bytes())]);
            assert!(
                matches!(
                    parse_package(&bytes, &Limits::default()),
                    Err(PackageError::BadManifest(_))
                ),
                "{manifest}"
            );
        }
    }

    #[test]
    fn rejects_non_archives_and_accepts_plain_tar() {
        assert!(matches!(
            parse_package(b"hello world", &Limits::default()),
            Err(PackageError::Archive(_))
        ));
        let tar = tar_bytes(&[Member::file(
            "package/package.json",
            br#"{"name":"a","version":"1"}"#,
        )]);
        assert_eq!(
            parse_package(&tar, &Limits::default())
                .unwrap()
                .manifest
                .name,
            "a"
        );
    }

    #[test]
    fn manifest_peek_matches_full_parse_and_checks_paths() {
        let bytes = package(
            "a.b",
            "2.0.0",
            &[("c", "1.0.0")],
            &[("StructureDefinition-p.json", SD)],
        );
        let m = read_manifest(&bytes, &Limits::default()).unwrap();
        assert_eq!(
            m,
            parse_package(&bytes, &Limits::default()).unwrap().manifest
        );
        let evil = tgz(&[Member::file("../package/package.json", b"{}")]);
        assert!(matches!(
            read_manifest(&evil, &Limits::default()),
            Err(PackageError::UnsafePath(_))
        ));
    }

    #[test]
    fn package_names_and_versions_are_file_name_safe() {
        for ok in [
            "tw.gov.mohw.twcore",
            "hl7.fhir.r4.core",
            "KBV.Basis",
            "a_b-c",
        ] {
            assert!(valid_package_name(ok), "{ok}");
        }
        for bad in ["", ".hidden", "a/b", "a#b", "..", "a b", "é"] {
            assert!(!valid_package_name(bad), "{bad}");
        }
        for ok in [
            "1.0.0",
            "2022.4.20221006",
            "1.0.0-ballot",
            "current",
            "1.0.0+build",
        ] {
            assert!(valid_package_version(ok), "{ok}");
        }
        for bad in ["", "1/2", "..", "-1", "1 0"] {
            assert!(!valid_package_version(bad), "{bad}");
        }
    }
}
