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
}

/// Inspect `path` and propose a format. Never fails: unreadable or unknown
/// input yields a detection with `format: None`.
pub fn detect(path: &Path) -> Detection {
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
            ImportFormat::Loinc => d.version = loinc_release(&entries),
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

fn evidence_for(format: ImportFormat, name: &str, entries: &[String]) -> String {
    let needle: &[&str] = match format {
        ImportFormat::SnomedRf2 => &["concept_full", "concept_snapshot", "description_full"],
        ImportFormat::Loinc => &["loinc.csv", "loinctable.csv"],
        ImportFormat::Rxnorm => &["rxnconso.rrf"],
        ImportFormat::Icd10Cm => &["tabular"],
        _ => &[],
    };
    for e in entries {
        let l = e.to_ascii_lowercase();
        if needle.iter().any(|n| l.contains(n)) {
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

/// `Loinc_2.80/LoincTable/Loinc.csv` → `2.80`.
fn loinc_release(entries: &[String]) -> Option<String> {
    entries.iter().find_map(|e| {
        e.split('/').find_map(|seg| {
            let l = seg.to_ascii_lowercase();
            let v = l.strip_prefix("loinc_")?;
            (v.chars().next()?.is_ascii_digit()).then(|| v.to_string())
        })
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
}
