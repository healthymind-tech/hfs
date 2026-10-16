//! Import format registry and archive inspection.
//!
//! The registry is derived from [`ImportFormat::value_variants`], so a format
//! added to the CLI (`--format …`) shows up in the admin API and console with
//! no change here — it just gets the generic defaults below until someone
//! gives it a growth factor or a licence note. That is deliberate: Phase 1C
//! adds `icd10-pcs` on another branch and it must appear on merge without a
//! second edit in this file.

use std::io::Read;
use std::path::Path;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::config::{ImportFormat, detect_format};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// How the source data may be obtained — drives the console's licence note.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Licence {
    /// Licensed content the operator must obtain and supply (SNOMED CT, LOINC,
    /// RxNorm …). HTS never downloads it.
    UserSupplied,
    /// Public-domain / freely redistributable files (CMS ICD-10, UCUM …).
    Public,
    /// FHIR packages and Bundles: licence travels with the package.
    Package,
}

/// Static facts about one import format.
#[derive(Debug, Clone, Serialize)]
pub struct FormatInfo {
    /// CLI value (`snomed-rf2`, `loinc`, …).
    pub id: String,
    pub licence: Licence,
    /// The archive yields one CodeSystem, so "the version" is unambiguous and
    /// may be overridden.
    pub single_system: bool,
    /// Honours the `languages` parameter.
    pub languages: bool,
    /// Honours the `properties` parameter (concept property selection).
    pub properties: bool,
    /// Minimum database growth to budget for, in bytes.
    pub growth_floor: u64,
    /// Database growth per source byte (applied to the file size).
    pub growth_per_byte: f64,
    /// Measured or typical import duration / memory note shown to operators.
    pub note: Option<&'static str>,
}

impl FormatInfo {
    /// Estimated database growth for a source of `size` bytes.
    pub fn estimate_growth(&self, size: u64) -> u64 {
        let scaled = (size as f64 * self.growth_per_byte) as u64;
        scaled.max(self.growth_floor)
    }
}

/// Facts for `format`. Unknown (newly added) formats fall through to the
/// generic defaults.
pub fn info(format: ImportFormat) -> FormatInfo {
    let id = format.to_string();
    let base = FormatInfo {
        id,
        licence: Licence::Public,
        single_system: true,
        languages: false,
        properties: false,
        growth_floor: 64 * MIB,
        growth_per_byte: 10.0,
        note: None,
    };
    match format {
        // Measured (Phase 1C): SNOMED INT 20250601 grew the DB by 3–4 GB.
        ImportFormat::SnomedRf2 => FormatInfo {
            licence: Licence::UserSupplied,
            languages: true,
            growth_floor: 4 * GIB,
            growth_per_byte: 3.5,
            note: Some("snomed"),
            ..base
        },
        // Measured (Phase 1C): LOINC 2.80 grew the DB 5.0 → 7.2 GB (176 668
        // concepts, 2 h 13 min, 2.5 GB RAM peak).
        ImportFormat::Loinc => FormatInfo {
            licence: Licence::UserSupplied,
            languages: true,
            properties: true,
            growth_floor: 5 * GIB / 2,
            growth_per_byte: 4.0,
            note: Some("loinc"),
            ..base
        },
        ImportFormat::Rxnorm => FormatInfo {
            licence: Licence::UserSupplied,
            growth_floor: GIB,
            growth_per_byte: 3.0,
            ..base
        },
        ImportFormat::Icd10Cm => FormatInfo {
            growth_floor: 256 * MIB,
            growth_per_byte: 20.0,
            ..base
        },
        ImportFormat::Hl7Npm => FormatInfo {
            licence: Licence::Package,
            single_system: false,
            growth_floor: 64 * MIB,
            growth_per_byte: 20.0,
            ..base
        },
        ImportFormat::FhirBundle => FormatInfo {
            licence: Licence::Package,
            single_system: false,
            growth_floor: 16 * MIB,
            growth_per_byte: 4.0,
            ..base
        },
        ImportFormat::NciThesaurus | ImportFormat::Mesh => FormatInfo {
            growth_floor: 512 * MIB,
            growth_per_byte: 4.0,
            ..base
        },
        _ => base,
    }
}

/// Every registered format, in CLI order.
pub fn registry() -> Vec<FormatInfo> {
    ImportFormat::value_variants()
        .iter()
        .copied()
        .map(info)
        .collect()
}

/// Parse a CLI format id (`snomed-rf2`).
pub fn parse_format(id: &str) -> Option<ImportFormat> {
    ImportFormat::from_str(id.trim(), true).ok()
}

/// Result of inspecting a source file.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Detection {
    /// Detected format id, `None` when nothing matched (operator must choose).
    pub format: Option<String>,
    /// What the decision rests on, for the operator (`zip entry …`, `name`).
    pub evidence: Option<String>,
    /// Release/version found inside the archive, when it carries one.
    pub version: Option<String>,
    /// FHIR package `name` (hl7-npm).
    pub package_name: Option<String>,
    pub size_bytes: u64,
    /// Budgeted database growth for the detected (or overridden) format.
    pub growth_estimate_bytes: Option<u64>,
    pub licence: Option<Licence>,
    /// Up to 12 archive entries, so the operator can see what is inside.
    pub sample_entries: Vec<String>,
    /// For a directory: the file or sub-directory inside it that the importer
    /// reads (`Loinc_2.80.zip`, `rrf`), relative to the directory. `None`
    /// when the directory itself is the import source (RxNorm RRF folder) or
    /// when no single source could be chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_path: Option<String>,
}

/// Inspect `path` and propose a format. Never fails: unreadable or unknown
/// input yields a detection with `format: None`.
pub fn detect(path: &Path) -> Detection {
    if path.is_dir() {
        return detect_dir(path);
    }
    let size_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut d = Detection {
        size_bytes,
        ..Default::default()
    };
    let entries = if name.ends_with(".zip") {
        zip_entries(path)
    } else {
        Vec::new()
    };
    d.sample_entries = entries.iter().take(12).cloned().collect();

    let format = detect_format(path).or_else(|| {
        // ICD-10-PCS ships as a zip with `icd10pcs_*` files; recognise it by
        // content when this build has the importer (Phase 1C adds it).
        entries
            .iter()
            .any(|e| e.to_ascii_lowercase().contains("icd10pcs"))
            .then(|| parse_format("icd10-pcs"))
            .flatten()
    });
    if let Some(f) = format {
        let fi = info(f);
        d.format = Some(fi.id.clone());
        d.licence = Some(fi.licence);
        d.growth_estimate_bytes = Some(fi.estimate_growth(size_bytes));
        d.evidence = Some(evidence_for(f, &name, &entries));
        match f {
            ImportFormat::SnomedRf2 => d.version = snomed_release(&entries),
            // Same rule as the importer: the ZIP name (`Loinc_2.80.zip`),
            // else a `Loinc_<ver>/` folder.
            ImportFormat::Loinc => {
                d.version = crate::import::loinc_csv::loinc_release_version_from_entry(&name)
                    .or_else(|| {
                        entries.iter().find_map(|e| {
                            crate::import::loinc_csv::loinc_release_version_from_entry(e)
                        })
                    })
            }
            ImportFormat::Hl7Npm => {
                if let Some((n, v)) = npm_name_version(path) {
                    d.package_name = Some(n);
                    d.version = Some(v);
                }
            }
            _ => d.version = year_in(&name),
        }
    }
    d
}

/// Deepest level and most files a directory scan looks at: release folders
/// are shallow (`Loinc_2.80/LoincTable/Loinc.csv`, `rrf/RXNCONSO.RRF`).
const DIR_SCAN_DEPTH: usize = 4;
const DIR_SCAN_LIMIT: usize = 5000;

/// Inspect a directory: classify each file it holds (release archives by
/// their entries, unpacked release trees by their marker files) and propose
/// a format only when exactly one format is present.
///
/// The usual layouts:
/// - a folder holding the release ZIP (`loinc/2.80/Loinc_2.80.zip` next to
///   `loincupload.properties`, `snomed/SnomedCT_….zip`) → that format, with
///   [`Detection::import_path`] pointing at the archive;
/// - an unpacked RxNorm release (`rrf/RXNCONSO.RRF`) → `rxnorm`, importing
///   the folder that holds the RRF files;
/// - an unpacked ICD-10-CM / ICD-10-PCS release → the tabular XML /
///   `icd10pcs_*.txt` file;
/// - an unpacked LOINC (`LoincTable/Loinc.csv`, `AccessoryFiles/PartFile/`)
///   or SNOMED RF2 (`sct2_*`, `der2_*`) tree → the format, without an import
///   path: those importers read the release ZIP.
///
/// macOS AppleDouble files (`._*`), `__MACOSX/` and other dot files are
/// ignored.
fn detect_dir(path: &Path) -> Detection {
    let files = dir_files(path);
    let mut d = Detection {
        sample_entries: files.iter().take(12).cloned().collect(),
        ..Default::default()
    };

    // (format, relative path, importable source?)
    let mut found: Vec<(ImportFormat, String, bool)> = Vec::new();
    for rel in &files {
        let lower = rel.to_ascii_lowercase();
        let file = lower.rsplit('/').next().unwrap_or(&lower);
        if file == "rxnconso.rrf" {
            let parent = rel.rsplit_once('/').map(|(dir, _)| dir.to_string());
            found.push((ImportFormat::Rxnorm, parent.unwrap_or_default(), true));
        } else if (file.starts_with("sct2_") || file.starts_with("der2_")) && file.ends_with(".txt")
        {
            found.push((ImportFormat::SnomedRf2, rel.clone(), false));
        } else if is_unpacked_loinc_marker(&lower, file) {
            found.push((ImportFormat::Loinc, rel.clone(), false));
        } else if file.ends_with(".rrf") {
            // Other RxNorm tables: covered by RXNCONSO.RRF above.
        } else if let Some(f) = detect(&path.join(rel)).format.as_deref().and_then(parse_format)
        {
            found.push((f, rel.clone(), true));
        }
    }

    let mut formats: Vec<ImportFormat> = Vec::new();
    for (f, _, _) in &found {
        if !formats.contains(f) {
            formats.push(*f);
        }
    }
    match formats.as_slice() {
        [] => {
            d.evidence = Some("no terminology release recognised in this directory".into());
            d
        }
        [format] => {
            let format = *format;
            let sources: Vec<&String> = found
                .iter()
                .filter(|(_, _, importable)| *importable)
                .map(|(_, rel, _)| rel)
                .collect();
            // One importable source: describe it as if it had been chosen.
            let chosen = match sources.as_slice() {
                [one] => Some((*one).clone()),
                many => {
                    // Several files of one format: a single archive wins
                    // (`icd10cm-table-index-2025.zip` next to its PDFs).
                    let zips: Vec<&&String> = many
                        .iter()
                        .filter(|r| r.to_ascii_lowercase().ends_with(".zip"))
                        .collect();
                    match zips.as_slice() {
                        [one] => Some((**one).clone()),
                        _ => None,
                    }
                }
            };
            let mut out = match &chosen {
                Some(rel) if !rel.is_empty() && format != ImportFormat::Rxnorm => {
                    let mut inner = detect(&path.join(rel));
                    inner.evidence = Some(format!(
                        "{rel}: {}",
                        inner.evidence.as_deref().unwrap_or("recognised")
                    ));
                    inner.import_path = Some(rel.clone());
                    inner
                }
                _ => {
                    let fi = info(format);
                    let size = files
                        .iter()
                        .filter_map(|r| std::fs::metadata(path.join(r)).ok())
                        .map(|m| m.len())
                        .sum();
                    let mut inner = Detection {
                        size_bytes: size,
                        licence: Some(fi.licence),
                        growth_estimate_bytes: Some(fi.estimate_growth(size)),
                        ..Default::default()
                    };
                    let first = found.iter().find(|(f, _, _)| *f == format).map(|(_, r, _)| r);
                    inner.evidence = Some(match (format, &chosen) {
                        (ImportFormat::Rxnorm, Some(dir)) if !dir.is_empty() => {
                            inner.import_path = Some(dir.clone());
                            format!("RXNCONSO.RRF in {dir}/")
                        }
                        (ImportFormat::Rxnorm, _) => "RXNCONSO.RRF".to_string(),
                        (_, _) if sources.is_empty() => format!(
                            "unpacked release ({}); import the release ZIP file",
                            first.map(String::as_str).unwrap_or("")
                        ),
                        (_, _) => format!(
                            "several {format} sources ({}); choose one file",
                            sources
                                .iter()
                                .take(4)
                                .map(|s| s.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    });
                    inner
                }
            };
            out.format = Some(info(format).id);
            if out.version.is_none() && format == ImportFormat::Loinc {
                out.version = loinc_upload_version(path);
            }
            out.sample_entries = d.sample_entries;
            out
        }
        several => {
            let listed: Vec<String> = several
                .iter()
                .map(|f| {
                    let rel = found
                        .iter()
                        .find(|(g, _, _)| g == f)
                        .map(|(_, r, _)| r.as_str())
                        .unwrap_or("");
                    format!("{f} ({rel})")
                })
                .collect();
            d.evidence = Some(format!(
                "directory holds several formats: {}; choose a sub-directory or file",
                listed.join(", ")
            ));
            d
        }
    }
}

/// Unpacked LOINC release markers: the main table (`LoincTable/Loinc.csv`,
/// or a top-level `Loinc.csv` / `LoincTable.csv`) or the Part file.
/// `AccessoryFiles/PanelsAndForms/Loinc.csv` alone is not the main table, but
/// any `AccessoryFiles/` tree is still LOINC's.
fn is_unpacked_loinc_marker(lower: &str, file: &str) -> bool {
    lower.ends_with("loinctable/loinc.csv")
        || file == "loinctable.csv"
        || (file == "loinc.csv" && !lower.contains('/'))
        || lower.contains("accessoryfiles/partfile/")
        || lower.contains("accessoryfiles/componenthierarchybysystem/")
}

/// `loinc.codesystem.version` from a HAPI-style `loincupload.properties`.
fn loinc_upload_version(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("loincupload.properties")).ok()?;
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "loinc.codesystem.version")
            .then(|| value.trim().to_string())
            .filter(|v| !v.is_empty())
    })
}

/// Relative paths (`/`-separated, sorted) of the files under `root`, at most
/// [`DIR_SCAN_DEPTH`] levels deep and [`DIR_SCAN_LIMIT`] files, skipping
/// dot files (incl. AppleDouble `._*`) and `__MACOSX/`.
fn dir_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack: Vec<(std::path::PathBuf, String, usize)> =
        vec![(root.to_path_buf(), String::new(), 0)];
    while let Some((dir, prefix, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if name.starts_with('.') || name == "__MACOSX" {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            // Symlinked directories could loop; release folders do not use them.
            if kind.is_symlink() && entry.path().is_dir() {
                continue;
            }
            if kind.is_dir() {
                if depth + 1 < DIR_SCAN_DEPTH {
                    stack.push((entry.path(), rel, depth + 1));
                }
            } else if out.len() < DIR_SCAN_LIMIT {
                out.push(rel);
            }
        }
    }
    out.sort();
    out
}

fn evidence_for(format: ImportFormat, name: &str, entries: &[String]) -> String {
    // Most specific first: LOINC archives also carry accessory `Loinc.csv`
    // copies (PanelsAndForms) that are not the main table.
    let needle: &[&str] = match format {
        ImportFormat::SnomedRf2 => &["concept_full", "concept_snapshot", "description_full"],
        ImportFormat::Loinc => &["loinctable/loinc.csv", "loinctable.csv", "loinc.csv"],
        ImportFormat::Rxnorm => &["rxnconso.rrf"],
        ImportFormat::Icd10Cm => &["tabular-", "tabular_"],
        ImportFormat::Icd10Pcs => &["icd10pcs_codes", "icd10pcs"],
        _ => &[],
    };
    // The data file over its PDF/XSD companions (`icd-10-cm-tabular-2025.pdf`).
    let data = |e: &&String| {
        let l = e.to_ascii_lowercase();
        !(l.ends_with(".pdf") || l.ends_with(".xsd"))
    };
    for n in needle {
        if let Some(e) = entries
            .iter()
            .filter(data)
            .find(|e| e.to_ascii_lowercase().contains(n))
        {
            return format!("archive entry {e}");
        }
    }
    format!("file name {name}")
}

fn zip_entries(path: &Path) -> Vec<String> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let Ok(mut zip) = zip::ZipArchive::new(file) else {
        return Vec::new();
    };
    (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|e| e.name().to_string()))
        .collect()
}

/// `sct2_Concept_Snapshot_INT_20250601.txt` → `20250601`.
fn snomed_release(entries: &[String]) -> Option<String> {
    entries
        .iter()
        .find(|e| {
            let l = e.to_ascii_lowercase();
            l.contains("sct2_concept_") && l.ends_with(".txt")
        })
        .and_then(|e| {
            let stem = e.rsplit('/').next()?.trim_end_matches(".txt");
            let tail = stem.rsplit('_').next()?;
            (tail.len() >= 8 && tail[..8].bytes().all(|b| b.is_ascii_digit()))
                .then(|| tail[..8].to_string())
        })
}

/// First four-digit year 19xx/20xx in a file name (`icd10cm_tabular_2025.xml`).
fn year_in(name: &str) -> Option<String> {
    let b = name.as_bytes();
    (0..b.len().saturating_sub(3)).find_map(|i| {
        let w = &b[i..i + 4];
        let digits = w.iter().all(u8::is_ascii_digit);
        let bounded = (i == 0 || !b[i - 1].is_ascii_digit())
            && (i + 4 == b.len() || !b[i + 4].is_ascii_digit());
        (digits && bounded && (w.starts_with(b"19") || w.starts_with(b"20")))
            .then(|| String::from_utf8_lossy(w).into_owned())
    })
}

/// Read `package/package.json` from an NPM tarball.
pub fn npm_name_version(path: &Path) -> Option<(String, String)> {
    let file = std::fs::File::open(path).ok()?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);
    for entry in tar.entries().ok()? {
        let mut entry = entry.ok()?;
        let p = entry.path().ok()?.to_string_lossy().into_owned();
        if p == "package/package.json" || p == "package.json" {
            let mut s = String::new();
            entry.read_to_string(&mut s).ok()?;
            let v: serde_json::Value = serde_json::from_str(&s).ok()?;
            return Some((
                v.get("name")?.as_str()?.to_string(),
                v.get("version")?.as_str()?.to_string(),
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_with(name: &str, entries: &[&str]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        for e in entries {
            zip.start_file(*e, zip::write::FileOptions::default())
                .unwrap();
            zip.write_all(b"x").unwrap();
        }
        zip.finish().unwrap();
        (dir, path)
    }

    #[test]
    fn registry_covers_every_cli_format() {
        let ids: Vec<String> = registry().into_iter().map(|f| f.id).collect();
        assert!(ids.contains(&"snomed-rf2".to_string()));
        assert!(ids.contains(&"fhir-bundle".to_string()));
        assert_eq!(ids.len(), ImportFormat::value_variants().len());
        assert_eq!(parse_format("LOINC"), Some(ImportFormat::Loinc));
        assert_eq!(parse_format("nope"), None);
    }

    #[test]
    fn detects_snomed_by_content_and_reads_release() {
        let (_d, p) = zip_with(
            "upload.zip",
            &[
                "SnomedCT_InternationalRF2_PRODUCTION_20250601T120000Z/Full/Terminology/sct2_Concept_Full_INT_20250601.txt",
            ],
        );
        let d = detect(&p);
        assert_eq!(d.format.as_deref(), Some("snomed-rf2"));
        assert_eq!(d.version.as_deref(), Some("20250601"));
        assert_eq!(d.licence, Some(Licence::UserSupplied));
        assert!(d.evidence.unwrap().contains("sct2_Concept_Full"));
        assert!(d.growth_estimate_bytes.unwrap() >= 4 * GIB);
    }

    #[test]
    fn detects_loinc_and_version() {
        let (_d, p) = zip_with("x.zip", &["Loinc_2.80/LoincTable/Loinc.csv"]);
        let d = detect(&p);
        assert_eq!(d.format.as_deref(), Some("loinc"));
        assert_eq!(d.version.as_deref(), Some("2.80"));
        // The 2.80 distribution: flat layout, version only in the ZIP name,
        // and a decoy `Loinc_2.80_DifferenceReport.pdf` plus accessory copy.
        let (_d, p) = zip_with(
            "Loinc_2.80.zip",
            &[
                "Loinc_2.80_DifferenceReport.pdf",
                "AccessoryFiles/PanelsAndForms/Loinc.csv",
                "LoincTable/Loinc.csv",
            ],
        );
        let d = detect(&p);
        assert_eq!(d.version.as_deref(), Some("2.80"));
        assert_eq!(
            d.evidence.as_deref(),
            Some("archive entry LoincTable/Loinc.csv")
        );
    }

    #[test]
    fn unknown_zip_has_no_format_but_lists_entries() {
        let (_d, p) = zip_with("x.zip", &["readme.txt", "data/a.bin"]);
        let d = detect(&p);
        assert_eq!(d.format, None);
        assert_eq!(d.sample_entries.len(), 2);
    }

    #[test]
    fn detects_npm_package_name_and_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pkg.tgz");
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut tar = tar::Builder::new(gz);
        let body = br#"{"name":"hl7.terminology.r4","version":"6.5.0"}"#;
        let mut h = tar::Header::new_gnu();
        h.set_size(body.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append_data(&mut h, "package/package.json", &body[..])
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        let d = detect(&path);
        assert_eq!(d.format.as_deref(), Some("hl7-npm"));
        assert_eq!(d.package_name.as_deref(), Some("hl7.terminology.r4"));
        assert_eq!(d.version.as_deref(), Some("6.5.0"));
    }

    #[test]
    fn year_extraction() {
        assert_eq!(year_in("icd10cm_tabular_2025.xml").as_deref(), Some("2025"));
        assert_eq!(year_in("abc123456.xml"), None);
    }

    #[test]
    fn growth_uses_floor_and_scale() {
        let f = info(ImportFormat::Loinc);
        assert_eq!(f.estimate_growth(1), 5 * GIB / 2);
        assert_eq!(f.estimate_growth(GIB), 4 * GIB);
    }

    /// Lay out `files` under a fresh directory; `*.zip` paths become archives
    /// holding the `(zip path, entries)` given in `zips`.
    fn tree(files: &[&str], zips: &[(&str, &[&str])]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for f in files {
            let p = dir.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"x").unwrap();
        }
        for (z, entries) in zips {
            let p = dir.path().join(z);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&p).unwrap());
            for e in *entries {
                zip.start_file(*e, zip::write::FileOptions::default())
                    .unwrap();
                zip.write_all(b"x").unwrap();
            }
            zip.finish().unwrap();
        }
        dir
    }

    const LOINC_ZIP: &[&str] = &[
        "AccessoryFiles/AnswerFile/AnswerList.csv",
        "AccessoryFiles/ComponentHierarchyBySystem/ComponentHierarchyBySystem.csv",
        "AccessoryFiles/PanelsAndForms/Loinc.csv",
        "AccessoryFiles/PartFile/Part.csv",
        "LoincTable/Loinc.csv",
    ];

    /// The real LOINC 2.80 layout (`test_datas/loinc/2.80`): the release ZIP
    /// next to HAPI's `loincupload.properties`, plus macOS `._*` files.
    #[test]
    fn loinc_release_directory_is_loinc_not_rxnorm() {
        let dir = tree(
            &["2.80/loincupload.properties", "2.80/._Loinc_2.80.zip", "2.80/._loincupload.properties"],
            &[("2.80/Loinc_2.80.zip", LOINC_ZIP)],
        );
        std::fs::write(
            dir.path().join("2.80/loincupload.properties"),
            "loinc.hierarchy.file=AccessoryFiles/ComponentHierarchyBySystem/ComponentHierarchyBySystem.csv\nloinc.codesystem.version=2.80\n",
        )
        .unwrap();
        let d = detect(&dir.path().join("2.80"));
        assert_eq!(d.format.as_deref(), Some("loinc"), "{d:?}");
        assert_eq!(d.import_path.as_deref(), Some("Loinc_2.80.zip"));
        assert_eq!(d.version.as_deref(), Some("2.80"));
        assert!(d.evidence.unwrap().contains("LoincTable/Loinc.csv"));

        // The parent `loinc/` folder (with the Taiwan mapping CSVs) too.
        std::fs::write(dir.path().join("taiwan_mapping.csv"), "a,b").unwrap();
        std::fs::write(dir.path().join("lab_reference_ranges.csv"), "a,b").unwrap();
        let d = detect(dir.path());
        assert_eq!(d.format.as_deref(), Some("loinc"), "{d:?}");
        assert_eq!(d.import_path.as_deref(), Some("2.80/Loinc_2.80.zip"));
    }

    #[test]
    fn unpacked_loinc_tree_is_loinc_without_import_path() {
        let dir = tree(
            &[
                "Loinc_2.80/LoincTable/Loinc.csv",
                "Loinc_2.80/AccessoryFiles/PartFile/Part.csv",
                "Loinc_2.80/AccessoryFiles/PanelsAndForms/Loinc.csv",
            ],
            &[],
        );
        let d = detect(dir.path());
        assert_eq!(d.format.as_deref(), Some("loinc"), "{d:?}");
        assert_eq!(d.import_path, None);
        assert!(d.evidence.unwrap().contains("release ZIP"));
        // Only the Part file present still says LOINC.
        let dir = tree(&["AccessoryFiles/PartFile/Part.csv"], &[]);
        assert_eq!(detect(dir.path()).format.as_deref(), Some("loinc"));
    }

    #[test]
    fn rxnorm_directories() {
        // Files at the top level: the directory itself is the source.
        let dir = tree(&["RXNCONSO.RRF", "RXNREL.RRF", "RXNSAT.RRF"], &[]);
        let d = detect(dir.path());
        assert_eq!(d.format.as_deref(), Some("rxnorm"), "{d:?}");
        assert_eq!(d.import_path, None);
        // The full release: `RxNorm_full_10062025/rrf/RXNCONSO.RRF`.
        let dir = tree(
            &[
                "rrf/RXNCONSO.RRF",
                "rrf/RXNREL.RRF",
                "scripts/mysql/Load_scripts_mysql_rxn_unix.sh",
                "Readme_full_10062025.txt",
            ],
            &[],
        );
        let d = detect(dir.path());
        assert_eq!(d.format.as_deref(), Some("rxnorm"), "{d:?}");
        assert_eq!(d.import_path.as_deref(), Some("rrf"));
    }

    #[test]
    fn snomed_directories() {
        let dir = tree(
            &["._SnomedCT_InternationalRF2_PRODUCTION_20250601T120000Z.zip"],
            &[(
                "SnomedCT_InternationalRF2_PRODUCTION_20250601T120000Z.zip",
                &[
                    "SnomedCT_InternationalRF2_PRODUCTION_20250601T120000Z/Full/Terminology/sct2_Concept_Full_INT_20250601.txt",
                    "SnomedCT_InternationalRF2_PRODUCTION_20250601T120000Z/Snapshot/Terminology/sct2_Concept_Snapshot_INT_20250601.txt",
                ],
            )],
        );
        let d = detect(dir.path());
        assert_eq!(d.format.as_deref(), Some("snomed-rf2"), "{d:?}");
        assert_eq!(d.version.as_deref(), Some("20250601"));
        assert_eq!(
            d.import_path.as_deref(),
            Some("SnomedCT_InternationalRF2_PRODUCTION_20250601T120000Z.zip")
        );

        let dir = tree(
            &[
                "Snapshot/Terminology/sct2_Concept_Snapshot_INT_20250601.txt",
                "Snapshot/Refset/Language/der2_cRefset_LanguageSnapshot-en_INT_20250601.txt",
                "release_package_information.json",
            ],
            &[],
        );
        let d = detect(dir.path());
        assert_eq!(d.format.as_deref(), Some("snomed-rf2"), "{d:?}");
        assert_eq!(d.import_path, None);
    }

    /// The CMS layouts in `test_datas/icd/10/{icd10cm,icd10pcs}`.
    #[test]
    fn icd10_directories() {
        let dir = tree(
            &[],
            &[(
                "icd10cm-table-index-2025.zip",
                &[
                    "icd-10-cm-tabular-2025.pdf",
                    "icd-10-cm-index-2025.xml",
                    "icd-10-cm-tabular-2025.xml",
                ],
            )],
        );
        let d = detect(dir.path());
        assert_eq!(d.format.as_deref(), Some("icd10-cm"), "{d:?}");
        assert_eq!(d.import_path.as_deref(), Some("icd10cm-table-index-2025.zip"));
        assert!(d.evidence.unwrap().ends_with("icd-10-cm-tabular-2025.xml"));

        let dir = tree(
            &[],
            &[(
                "icd10pcs_tables_2025.zip",
                &["icd10pcsCodesFile.pdf", "codes_addenda_2025.txt", "icd10pcs_codes_2025.txt"],
            )],
        );
        let d = detect(dir.path());
        assert_eq!(d.format.as_deref(), Some("icd10-pcs"), "{d:?}");
        assert_eq!(d.import_path.as_deref(), Some("icd10pcs_tables_2025.zip"));
        assert!(d.evidence.unwrap().ends_with("icd10pcs_codes_2025.txt"));

        // Unpacked: the tabular XML / the codes file is the source.
        let dir = tree(
            &["icd10cm_tabular_2025.xml", "icd10cm_index_2025.xml", "icd10cm_tabular_2025.pdf"],
            &[],
        );
        let d = detect(dir.path());
        assert_eq!(d.format.as_deref(), Some("icd10-cm"), "{d:?}");
        assert_eq!(d.import_path.as_deref(), Some("icd10cm_tabular_2025.xml"));
        let dir = tree(&["icd10pcs_codes_2025.txt", "codes_addenda_2025.txt"], &[]);
        let d = detect(dir.path());
        assert_eq!(d.format.as_deref(), Some("icd10-pcs"), "{d:?}");
        assert_eq!(d.import_path.as_deref(), Some("icd10pcs_codes_2025.txt"));
    }

    /// `test_datas/` itself holds LOINC, SNOMED and ICD releases: no guess.
    #[test]
    fn mixed_and_empty_directories_have_no_format() {
        let dir = tree(
            &["loinc/2.80/loincupload.properties", "generated/README"],
            &[
                ("loinc/2.80/Loinc_2.80.zip", LOINC_ZIP),
                (
                    "snomed/SnomedCT_X_20250601.zip",
                    &["X/Snapshot/Terminology/sct2_Concept_Snapshot_INT_20250601.txt"],
                ),
            ],
        );
        let d = detect(dir.path());
        assert_eq!(d.format, None, "{d:?}");
        let evidence = d.evidence.unwrap();
        assert!(evidence.contains("loinc") && evidence.contains("snomed-rf2"), "{evidence}");

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(detect(empty.path()).format, None);
    }
}
