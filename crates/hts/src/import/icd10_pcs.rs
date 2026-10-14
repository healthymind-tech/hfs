//! ICD-10-PCS importer.
//!
//! Parses the CMS ICD-10-PCS *order file* (`icd10pcs_codes_YYYY.txt`: one
//! `CODE TITLE` line per valid 7-character procedure code) and imports every
//! code plus a four-level grouping hierarchy into the HTS normalized schema.
//!
//! ```text
//! ICD-10-PCS                         (virtual root)
//! └── 0        Medical and Surgical  (axis 1 — section)
//!     └── 0D   Gastrointestinal System (axis 2 — body system)
//!         └── 0DT  Resection         (axis 3 — root operation)
//!             └── 0DTJ0ZZ Resection of Appendix, Open Approach
//! ```
//!
//! The order file carries only the leaf titles. Axis labels for the grouping
//! nodes come from the CMS *tables* XML (`icd10pcs_tables_YYYY.xml`) when it
//! is present next to the codes file (same ZIP or same directory); otherwise
//! the importer falls back to a built-in table of section names, the
//! Medical-and-Surgical body-system names, and a root-operation label derived
//! from the common leading words of the child titles ("Resection of Appendix…",
//! "Resection of Stomach…" → "Resection").
//!
//! # Canonical URL
//!
//! HL7 THO registers ICD-10-PCS as `http://www.cms.gov/Medicare/Coding/ICD10`
//! (CodeSystem `icd10PCS`, "ICD-10 Procedure Codes"); HAPI FHIR and the US Core
//! `us-core-procedure-code` ValueSet use the same URL. The importer claims that
//! URL as the authoritative (`content: complete`) copy, so it takes precedence
//! over the `content: not-present` stub shipped in the THO package.
//!
//! # No license required
//!
//! ICD-10-PCS is maintained by CMS and distributed free of charge:
//! <https://www.cms.gov/medicare/coding-billing/icd-10-codes>

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::Path;

use helios_persistence::tenant::TenantContext;

use crate::error::HtsError;
use crate::import::BundleImportBackend;
use crate::import::ImportStats;
use crate::import::bundle_builder::{BuilderConcept, CodeSystemMeta, build_code_system_bundle};

// ── ICD-10-PCS constants ──────────────────────────────────────────────────────

const ICD10PCS_URL: &str = "http://www.cms.gov/Medicare/Coding/ICD10";
const ICD10PCS_ID: &str = "icd10pcs";
const ICD10PCS_NAME: &str = "ICD10PCS";
const ICD10PCS_TITLE: &str =
    "ICD-10-PCS (International Classification of Diseases, 10th Revision, Procedure Coding System)";
const ROOT_CODE: &str = "ICD-10-PCS";

/// Axis-1 section names (stable since the 2016 release).
const SECTION_NAMES: &[(char, &str)] = &[
    ('0', "Medical and Surgical"),
    ('1', "Obstetrics"),
    ('2', "Placement"),
    ('3', "Administration"),
    ('4', "Measurement and Monitoring"),
    ('5', "Extracorporeal or Systemic Assistance and Performance"),
    ('6', "Extracorporeal or Systemic Therapies"),
    ('7', "Osteopathic"),
    ('8', "Other Procedures"),
    ('9', "Chiropractic"),
    ('B', "Imaging"),
    ('C', "Nuclear Medicine"),
    ('D', "Radiation Therapy"),
    ('F', "Physical Rehabilitation and Diagnostic Audiology"),
    ('G', "Mental Health"),
    ('H', "Substance Abuse Treatment"),
    ('X', "New Technology"),
];

/// Axis-2 body-system names for the Medical and Surgical section (`0`), which
/// holds ~87 % of all codes. Other sections fall back to a generic label
/// unless the tables XML is available.
const MED_SURG_BODY_SYSTEMS: &[(char, &str)] = &[
    ('0', "Central Nervous System and Cranial Nerves"),
    ('1', "Peripheral Nervous System"),
    ('2', "Heart and Great Vessels"),
    ('3', "Upper Arteries"),
    ('4', "Lower Arteries"),
    ('5', "Upper Veins"),
    ('6', "Lower Veins"),
    ('7', "Lymphatic and Hemic Systems"),
    ('8', "Eye"),
    ('9', "Ear, Nose, Sinus"),
    ('B', "Respiratory System"),
    ('C', "Mouth and Throat"),
    ('D', "Gastrointestinal System"),
    ('F', "Hepatobiliary System and Pancreas"),
    ('G', "Endocrine System"),
    ('H', "Skin and Breast"),
    ('J', "Subcutaneous Tissue and Fascia"),
    ('K', "Muscles"),
    ('L', "Tendons"),
    ('M', "Bursae and Ligaments"),
    ('N', "Head and Facial Bones"),
    ('P', "Upper Bones"),
    ('Q', "Lower Bones"),
    ('R', "Upper Joints"),
    ('S', "Lower Joints"),
    ('T', "Urinary System"),
    ('U', "Female Reproductive System"),
    ('V', "Male Reproductive System"),
    ('W', "Anatomical Regions, General"),
    ('X', "Anatomical Regions, Upper Extremities"),
    ('Y', "Anatomical Regions, Lower Extremities"),
];

/// Connector words stripped from the tail of a derived root-operation label.
const TRAILING_CONNECTORS: &[&str] = &[
    "of", "to", "in", "into", "from", "with", "using", "for", "by", "on", "and", "the", "or", "at",
];

// ── Data types ────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct PcsConcept {
    code: String,
    display: String,
    /// `true` for the 7-character procedure codes; `false` for grouping nodes.
    leaf: bool,
    /// Parent code, or `None` for the virtual root.
    parent: Option<String>,
}

/// Axis labels harvested from the CMS tables XML, keyed by code prefix
/// (1, 2 or 3 characters).
#[derive(Debug, Default)]
struct AxisLabels {
    labels: HashMap<String, String>,
    version: Option<String>,
}

/// Raw inputs located for one import: the mandatory order file and the
/// optional tables XML.
struct PcsSources {
    codes_name: String,
    codes_bytes: Vec<u8>,
    tables_xml: Option<Vec<u8>>,
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Import an ICD-10-PCS order file (`icd10pcs_codes_YYYY.txt`, or a CMS ZIP
/// containing one) through the given backend.
pub async fn import_icd10_pcs(
    backend: &dyn BundleImportBackend,
    ctx: &TenantContext,
    path: &Path,
    batch_size: usize,
    dry_run: bool,
) -> Result<ImportStats, HtsError> {
    let batch_size = batch_size.max(1);

    let path_owned = path.to_path_buf();
    let (concepts, version, errors) = tokio::task::spawn_blocking(
        move || -> Result<(Vec<PcsConcept>, String, Vec<String>), HtsError> {
            let sources = read_sources(&path_owned)?;
            let mut errors = Vec::new();
            let axis = match &sources.tables_xml {
                Some(xml) => parse_tables_xml(xml, &mut errors),
                None => AxisLabels::default(),
            };
            let version = axis
                .version
                .clone()
                .or_else(|| release_year_from_name(&sources.codes_name))
                .unwrap_or_else(|| "current".to_string());
            let concepts = parse_codes_file(&sources.codes_bytes, &axis, &mut errors)?;
            Ok((concepts, version, errors))
        },
    )
    .await
    .map_err(|e| HtsError::Internal(format!("ICD-10-PCS parser panicked: {e}")))??;

    let mut stats = ImportStats {
        errors,
        ..Default::default()
    };

    if dry_run {
        stats.code_systems = 1;
        stats.concepts = concepts.len() as u32;
        eprintln!(
            "[icd10-pcs] dry-run — {} concepts parsed (version {version}), no DB writes",
            concepts.len()
        );
        return Ok(stats);
    }

    let meta = CodeSystemMeta {
        id: ICD10PCS_ID,
        url: ICD10PCS_URL,
        version: Some(&version),
        name: Some(ICD10PCS_NAME),
        title: Some(ICD10PCS_TITLE),
        status: "active",
        content: "complete",
    };

    // Seed with an empty CodeSystem to register metadata.
    let seed_bytes = build_code_system_bundle(&meta, &[]);
    let seed_stats = backend.import_bundle(ctx, &seed_bytes).await?;
    stats.code_systems = seed_stats.code_systems;
    stats.errors.extend(seed_stats.errors);

    let total = concepts.len();
    let total_batches = total.div_ceil(batch_size).max(1);

    for (batch_idx, batch) in concepts.chunks(batch_size).enumerate() {
        let builder: Vec<BuilderConcept<'_>> = batch
            .iter()
            .map(|c| BuilderConcept {
                code: &c.code,
                display: Some(c.display.as_str()).filter(|s| !s.is_empty()),
                definition: if c.leaf { None } else { Some("header") },
                parent_code: c.parent.as_deref(),
                ..Default::default()
            })
            .collect();
        let bytes = build_code_system_bundle(&meta, &builder);
        let chunk = backend.import_bundle(ctx, &bytes).await?;
        stats.errors.extend(chunk.errors);

        crate::import::progress::report_batch(batch_idx + 1, total_batches);
        eprintln!(
            "[icd10-pcs] batch {}/{total_batches} — +{} concepts (total: {})",
            batch_idx + 1,
            batch.len(),
            ((batch_idx + 1) * batch_size).min(total),
        );
    }

    stats.concepts = total as u32;
    Ok(stats)
}

// ── Source discovery ──────────────────────────────────────────────────────────

fn is_codes_file(name: &str) -> bool {
    let n = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
    n.ends_with(".txt") && n.contains("icd10pcs") && !n.contains("addenda")
}

fn is_tables_xml(name: &str) -> bool {
    let n = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
    n.ends_with(".xml") && n.contains("icd10pcs") && n.contains("table")
}

/// Locate the order file (and optional tables XML) behind `path`, which may be
/// the `.txt` itself or a CMS ZIP containing it.
fn read_sources(path: &Path) -> Result<PcsSources, HtsError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if ext == "zip" {
        let file = std::fs::File::open(path).map_err(|e| {
            HtsError::InvalidRequest(format!("Cannot open ZIP '{p}': {e}", p = path.display()))
        })?;
        let mut archive = zip::ZipArchive::new(file).map_err(|e| {
            HtsError::InvalidRequest(format!("Invalid ZIP '{p}': {e}", p = path.display()))
        })?;

        let names: Vec<String> = (0..archive.len())
            .filter_map(|i| archive.by_index(i).ok().map(|e| e.name().to_string()))
            .collect();
        let codes_name = names
            .iter()
            .find(|n| is_codes_file(n))
            .cloned()
            .ok_or_else(|| {
                HtsError::InvalidRequest(format!(
                    "No 'icd10pcs_codes_*.txt' order file found inside ZIP '{}'",
                    path.display()
                ))
            })?;
        let tables_name = names.iter().find(|n| is_tables_xml(n)).cloned();

        let codes_bytes = read_zip_entry(&mut archive, &codes_name)?;
        let tables_xml = match tables_name {
            Some(n) => Some(read_zip_entry(&mut archive, &n)?),
            None => None,
        };
        Ok(PcsSources {
            codes_name,
            codes_bytes,
            tables_xml,
        })
    } else {
        let codes_bytes = std::fs::read(path).map_err(|e| {
            HtsError::InvalidRequest(format!("Cannot read file '{}': {e}", path.display()))
        })?;
        let codes_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        // A tables XML sitting next to the order file is picked up automatically.
        let tables_xml = path
            .parent()
            .and_then(|dir| std::fs::read_dir(dir).ok())
            .and_then(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .find(|e| is_tables_xml(&e.file_name().to_string_lossy()))
                    .and_then(|e| std::fs::read(e.path()).ok())
            });
        Ok(PcsSources {
            codes_name,
            codes_bytes,
            tables_xml,
        })
    }
}

fn read_zip_entry<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, HtsError> {
    let mut entry = archive
        .by_name(name)
        .map_err(|e| HtsError::InvalidRequest(format!("Cannot read ZIP entry '{name}': {e}")))?;
    let mut buf = Vec::with_capacity(entry.size() as usize);
    entry
        .read_to_end(&mut buf)
        .map_err(|e| HtsError::InvalidRequest(format!("Cannot read ZIP entry '{name}': {e}")))?;
    Ok(buf)
}

/// `icd10pcs_codes_2025.txt` → `Some("2025")`. CMS names every file in the
/// release after its fiscal year.
fn release_year_from_name(name: &str) -> Option<String> {
    let base = name.rsplit('/').next().unwrap_or(name);
    let digits: Vec<&str> = base
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| s.len() == 4 && s.starts_with("20"))
        .collect();
    digits.last().map(|s| s.to_string())
}

// ── Tables XML (optional axis labels) ────────────────────────────────────────

/// Harvest axis 1–3 labels (section, body system, root operation) from the CMS
/// `icd10pcs_tables_YYYY.xml`. Each `<pcsTable>` carries one `<axis pos="1|2|3">`
/// with a single `<label code="…">`; the labels are keyed by the accumulated
/// code prefix. Malformed XML is non-fatal: the error is recorded and the
/// built-in fallback labels are used instead.
fn parse_tables_xml(xml: &[u8], errors: &mut Vec<String>) -> AxisLabels {
    let mut out = AxisLabels::default();
    let text = match std::str::from_utf8(xml) {
        Ok(t) => t,
        Err(e) => {
            errors.push(format!(
                "tables XML is not valid UTF-8 — axis labels skipped: {e}"
            ));
            return out;
        }
    };
    let doc = match roxmltree::Document::parse(text) {
        Ok(d) => d,
        Err(e) => {
            errors.push(format!("tables XML parse error — axis labels skipped: {e}"));
            return out;
        }
    };
    let root = doc.root_element();
    out.version = root
        .children()
        .find(|n| n.has_tag_name("version"))
        .and_then(|n| n.text())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());

    for table in root.children().filter(|n| n.has_tag_name("pcsTable")) {
        let mut prefix = String::new();
        for pos in 1..=3u8 {
            let Some(axis) = table
                .children()
                .find(|n| n.has_tag_name("axis") && n.attribute("pos") == Some(&pos.to_string()))
            else {
                break;
            };
            let Some(label) = axis.children().find(|n| n.has_tag_name("label")) else {
                break;
            };
            let (Some(code), Some(text)) = (label.attribute("code"), label.text()) else {
                break;
            };
            prefix.push_str(code.trim());
            let text = text.trim();
            if !text.is_empty() {
                out.labels
                    .entry(prefix.clone())
                    .or_insert_with(|| text.to_string());
            }
        }
    }
    out
}

// ── Order-file parser ─────────────────────────────────────────────────────────

fn is_valid_pcs_code(code: &str) -> bool {
    code.len() == 7
        && code
            .chars()
            .all(|c| c.is_ascii_digit() || (c.is_ascii_uppercase() && c != 'I' && c != 'O'))
}

fn parse_codes_file(
    bytes: &[u8],
    axis: &AxisLabels,
    errors: &mut Vec<String>,
) -> Result<Vec<PcsConcept>, HtsError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| HtsError::InvalidRequest(format!("Order file is not valid UTF-8: {e}")))?;

    // code → title, in file order (BTreeMap gives a stable, sorted hierarchy).
    let mut leaves: BTreeMap<String, String> = BTreeMap::new();
    for (line_no, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }
        let (code, title) = match line.split_once(char::is_whitespace) {
            Some((c, t)) => (c.trim(), t.trim()),
            None => (line, ""),
        };
        if !is_valid_pcs_code(code) {
            errors.push(format!(
                "line {}: '{code}' is not a 7-character ICD-10-PCS code — skipped",
                line_no + 1
            ));
            continue;
        }
        if title.is_empty() {
            errors.push(format!(
                "line {}: code {code} has no title — imported with empty display",
                line_no + 1
            ));
        }
        if leaves.insert(code.to_string(), title.to_string()).is_some() {
            errors.push(format!(
                "line {}: duplicate code {code} — last title wins",
                line_no + 1
            ));
        }
    }

    if leaves.is_empty() {
        return Err(HtsError::InvalidRequest(
            "Order file contains no ICD-10-PCS codes".into(),
        ));
    }

    let mut concepts: Vec<PcsConcept> = Vec::with_capacity(leaves.len() + 1200);
    concepts.push(PcsConcept {
        code: ROOT_CODE.to_string(),
        display: ICD10PCS_TITLE.to_string(),
        leaf: false,
        parent: None,
    });

    // Group leaves by 3-character prefix so the root-operation label can be
    // derived from the child titles when the tables XML is absent.
    let mut by_operation: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for (code, title) in &leaves {
        by_operation
            .entry(code[..3].to_string())
            .or_default()
            .push(title.as_str());
    }

    let mut emitted_sections: BTreeMap<String, ()> = BTreeMap::new();
    let mut emitted_systems: BTreeMap<String, ()> = BTreeMap::new();

    for (op, titles) in &by_operation {
        let section = &op[..1];
        let system = &op[..2];
        let section_name = axis
            .labels
            .get(section)
            .cloned()
            .or_else(|| builtin_section_name(section))
            .unwrap_or_else(|| format!("Section {section}"));

        if emitted_sections.insert(section.to_string(), ()).is_none() {
            concepts.push(PcsConcept {
                code: section.to_string(),
                display: section_name.clone(),
                leaf: false,
                parent: Some(ROOT_CODE.to_string()),
            });
        }
        if emitted_systems.insert(system.to_string(), ()).is_none() {
            let display = axis
                .labels
                .get(system)
                .cloned()
                .or_else(|| builtin_body_system_name(system))
                .unwrap_or_else(|| format!("{section_name}, body system {}", &system[1..]));
            concepts.push(PcsConcept {
                code: system.to_string(),
                display,
                leaf: false,
                parent: Some(section.to_string()),
            });
        }
        let op_display = axis
            .labels
            .get(op)
            .cloned()
            .or_else(|| derive_common_label(titles))
            .unwrap_or_else(|| format!("{section_name}, {op}"));
        concepts.push(PcsConcept {
            code: op.clone(),
            display: op_display,
            leaf: false,
            parent: Some(system.to_string()),
        });
    }

    for (code, title) in leaves {
        let parent = code[..3].to_string();
        concepts.push(PcsConcept {
            code,
            display: title,
            leaf: true,
            parent: Some(parent),
        });
    }

    Ok(concepts)
}

fn builtin_section_name(section: &str) -> Option<String> {
    let c = section.chars().next()?;
    SECTION_NAMES
        .iter()
        .find(|(k, _)| *k == c)
        .map(|(_, v)| (*v).to_string())
}

fn builtin_body_system_name(system: &str) -> Option<String> {
    let mut chars = system.chars();
    let section = chars.next()?;
    let body = chars.next()?;
    if section != '0' {
        return None;
    }
    MED_SURG_BODY_SYSTEMS
        .iter()
        .find(|(k, _)| *k == body)
        .map(|(_, v)| (*v).to_string())
}

/// Longest common leading word sequence of `titles`, with trailing connector
/// words and punctuation trimmed. `None` when the titles share no leading word.
fn derive_common_label(titles: &[&str]) -> Option<String> {
    let mut common: Vec<&str> = titles.first()?.split_whitespace().collect();
    for t in &titles[1..] {
        let words: Vec<&str> = t.split_whitespace().collect();
        let n = common
            .iter()
            .zip(words.iter())
            .take_while(|(a, b)| a == b)
            .count();
        common.truncate(n);
        if common.is_empty() {
            return None;
        }
    }
    // Cut at the first connector word so a lone child ("Excision of Stomach,
    // Open Approach") still yields the operation ("Excision"), then trim any
    // trailing connectors ("Resection of" → "Resection").
    if let Some(pos) = common.iter().position(|w| {
        let bare = w.trim_matches(|c: char| !c.is_alphanumeric());
        TRAILING_CONNECTORS.contains(&bare.to_ascii_lowercase().as_str())
    }) {
        common.truncate(pos);
    }
    while let Some(last) = common.last() {
        let bare = last.trim_matches(|c: char| !c.is_alphanumeric());
        if bare.is_empty() || TRAILING_CONNECTORS.contains(&bare.to_ascii_lowercase().as_str()) {
            common.pop();
        } else {
            break;
        }
    }
    if common.is_empty() {
        return None;
    }
    let joined = common.join(" ");
    let trimmed = joined.trim_end_matches([',', ';', ':', '-']).trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::backends::SqliteTerminologyBackend;

    /// Six leaves across three sections / four body systems / five operations,
    /// CRLF-terminated like the CMS file.
    const CODES_TXT: &str = "0DTJ0ZZ Resection of Appendix, Open Approach\r\n\
0DTJ4ZZ Resection of Appendix, Percutaneous Endoscopic Approach\r\n\
0DT60ZZ Resection of Stomach, Open Approach\r\n\
0DB64Z3 Excision of Stomach, Percutaneous Endoscopic Approach, Vertical\r\n\
0016070 Bypass Cerebral Ventricle to Nasopharynx with Autologous Tissue Substitute, Open Approach\r\n\
B020ZZZ Computerized Tomography (CT Scan) of Brain\r\n\
GZ10ZZZ Psychological Tests, Developmental\r\n\
GZ11ZZZ Psychological Tests, Personality and Behavioral\r\n";

    const TABLES_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ICD10PCS.tabular>
  <version>2025</version>
  <pcsTable>
    <axis pos="1" values="1"><title>Section</title><label code="0">Medical and Surgical</label></axis>
    <axis pos="2" values="1"><title>Body System</title><label code="D">Gastrointestinal System</label></axis>
    <axis pos="3" values="1"><title>Operation</title><label code="T">Resection</label><definition>Cutting out or off, without replacement, all of a body part</definition></axis>
    <pcsRow codes="2">
      <axis pos="4" values="2"><title>Body Part</title><label code="6">Stomach</label><label code="J">Appendix</label></axis>
    </pcsRow>
  </pcsTable>
  <pcsTable>
    <axis pos="1" values="1"><title>Section</title><label code="G">Mental Health</label></axis>
    <axis pos="2" values="1"><title>Body System</title><label code="Z">None</label></axis>
    <axis pos="3" values="1"><title>Type</title><label code="1">Psychological Tests</label></axis>
  </pcsTable>
</ICD10PCS.tabular>"#;

    fn count_rows(backend: &SqliteTerminologyBackend, table: &str) -> i64 {
        let conn = backend.pool().get().unwrap();
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    fn parse(txt: &str) -> (Vec<PcsConcept>, Vec<String>) {
        let mut errors = Vec::new();
        let concepts =
            parse_codes_file(txt.as_bytes(), &AxisLabels::default(), &mut errors).unwrap();
        (concepts, errors)
    }

    fn find<'a>(concepts: &'a [PcsConcept], code: &str) -> &'a PcsConcept {
        concepts
            .iter()
            .find(|c| c.code == code)
            .unwrap_or_else(|| panic!("missing concept {code}"))
    }

    // ── parser unit tests ─────────────────────────────────────────────────

    #[test]
    fn parse_builds_four_level_hierarchy() {
        let (concepts, errors) = parse(CODES_TXT);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        // 1 root + 3 sections (0, B, G) + 4 systems (00, 0D, B0, GZ)
        // + 5 operations (001, 0DB, 0DT, B02, GZ1) + 8 leaves
        assert_eq!(concepts.len(), 1 + 3 + 4 + 5 + 8);

        let root = find(&concepts, ROOT_CODE);
        assert!(root.parent.is_none());
        assert!(!root.leaf);

        assert_eq!(find(&concepts, "0").parent.as_deref(), Some(ROOT_CODE));
        assert_eq!(find(&concepts, "0D").parent.as_deref(), Some("0"));
        assert_eq!(find(&concepts, "0DT").parent.as_deref(), Some("0D"));
        let leaf = find(&concepts, "0DTJ0ZZ");
        assert_eq!(leaf.parent.as_deref(), Some("0DT"));
        assert!(leaf.leaf);
        assert_eq!(leaf.display, "Resection of Appendix, Open Approach");
    }

    #[test]
    fn parse_uses_builtin_labels_without_tables_xml() {
        let (concepts, _) = parse(CODES_TXT);
        assert_eq!(find(&concepts, "0").display, "Medical and Surgical");
        assert_eq!(find(&concepts, "B").display, "Imaging");
        assert_eq!(find(&concepts, "0D").display, "Gastrointestinal System");
        assert_eq!(
            find(&concepts, "00").display,
            "Central Nervous System and Cranial Nerves"
        );
        // Non-Medical/Surgical body systems get a generic label.
        assert_eq!(
            find(&concepts, "GZ").display,
            "Mental Health, body system Z"
        );
    }

    #[test]
    fn parse_derives_operation_label_from_child_titles() {
        let (concepts, _) = parse(CODES_TXT);
        assert_eq!(find(&concepts, "0DT").display, "Resection");
        assert_eq!(find(&concepts, "0DB").display, "Excision");
        assert_eq!(find(&concepts, "001").display, "Bypass Cerebral Ventricle");
        assert_eq!(
            find(&concepts, "B02").display,
            "Computerized Tomography (CT Scan)"
        );
        assert_eq!(find(&concepts, "GZ1").display, "Psychological Tests");
    }

    #[test]
    fn derive_common_label_handles_edge_cases() {
        assert_eq!(derive_common_label(&[]), None);
        assert_eq!(derive_common_label(&["Alpha of X", "Beta of X"]), None);
        assert_eq!(derive_common_label(&["of the", "of the"]), None);
        assert_eq!(
            derive_common_label(&["Excision of Stomach, Open Approach"]).as_deref(),
            Some("Excision")
        );
        assert_eq!(
            derive_common_label(&["Drainage of Brain, Open", "Drainage of Spine"]).as_deref(),
            Some("Drainage")
        );
    }

    #[test]
    fn parse_tables_xml_overrides_labels_and_version() {
        let mut errors = Vec::new();
        let axis = parse_tables_xml(TABLES_XML.as_bytes(), &mut errors);
        assert!(errors.is_empty());
        assert_eq!(axis.version.as_deref(), Some("2025"));
        assert_eq!(
            axis.labels.get("0").map(String::as_str),
            Some("Medical and Surgical")
        );
        assert_eq!(
            axis.labels.get("0D").map(String::as_str),
            Some("Gastrointestinal System")
        );
        assert_eq!(
            axis.labels.get("0DT").map(String::as_str),
            Some("Resection")
        );
        assert_eq!(axis.labels.get("GZ").map(String::as_str), Some("None"));
        assert_eq!(
            axis.labels.get("GZ1").map(String::as_str),
            Some("Psychological Tests")
        );
        // Axis 4+ labels are never keyed.
        assert!(!axis.labels.contains_key("0DTJ"));

        let concepts = parse_codes_file(CODES_TXT.as_bytes(), &axis, &mut errors).unwrap();
        assert_eq!(find(&concepts, "GZ").display, "None");
        assert_eq!(find(&concepts, "0DT").display, "Resection");
    }

    #[test]
    fn parse_tables_xml_malformed_is_non_fatal() {
        let mut errors = Vec::new();
        let axis = parse_tables_xml(b"<not-closed", &mut errors);
        assert!(axis.labels.is_empty());
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn parse_rejects_bad_codes_and_reports_duplicates() {
        let txt = "0DTJ0ZZ Resection of Appendix, Open Approach\n\
                   0DTJ0Z Too short\n\
                   0DTI0ZZ Letter I is never used\n\
                   0DTJ0ZZ Resection of Appendix, Open Approach (dup)\n\
                   0DTJ4ZZ\n";
        let (concepts, errors) = parse(txt);
        assert_eq!(errors.len(), 4, "{errors:?}");
        assert!(errors[0].contains("0DTJ0Z"));
        assert!(errors[1].contains("0DTI0ZZ"));
        assert!(errors[2].contains("duplicate"));
        assert!(errors[3].contains("no title"));
        assert_eq!(concepts.iter().filter(|c| c.leaf).count(), 2);
        assert_eq!(
            find(&concepts, "0DTJ0ZZ").display,
            "Resection of Appendix, Open Approach (dup)"
        );
    }

    #[test]
    fn parse_empty_file_is_an_error() {
        let mut errors = Vec::new();
        assert!(parse_codes_file(b"\r\n\r\n", &AxisLabels::default(), &mut errors).is_err());
        assert!(parse_codes_file(b"\xFF\xFE", &AxisLabels::default(), &mut errors).is_err());
    }

    #[test]
    fn release_year_is_taken_from_the_file_name() {
        assert_eq!(
            release_year_from_name("icd10pcs_codes_2025.txt").as_deref(),
            Some("2025")
        );
        assert_eq!(
            release_year_from_name("x/icd10pcs_tables_2026.zip").as_deref(),
            Some("2026")
        );
        assert_eq!(release_year_from_name("codes.txt"), None);
    }

    #[test]
    fn file_name_detection() {
        assert!(is_codes_file("icd10pcs_codes_2025.txt"));
        assert!(is_codes_file("ICD10PCS_Codes_2025.TXT"));
        assert!(!is_codes_file("codes_addenda_2025.txt"));
        assert!(!is_codes_file("icd10pcsCodesFile.pdf"));
        assert!(is_tables_xml("icd10pcs_tables_2025.xml"));
        assert!(!is_tables_xml("icd10pcs_index_2025.xml"));
    }

    // ── end-to-end against SQLite ─────────────────────────────────────────

    fn make_txt_file() -> tempfile::NamedTempFile {
        use std::io::Write;
        let dir = std::env::temp_dir();
        let mut f = tempfile::Builder::new()
            .prefix("icd10pcs_codes_2025-")
            .suffix(".txt")
            .tempfile_in(dir)
            .unwrap();
        f.write_all(CODES_TXT.as_bytes()).unwrap();
        f
    }

    #[tokio::test]
    async fn import_dry_run_counts_without_writing() {
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        let f = make_txt_file();

        let stats = import_icd10_pcs(&backend, &ctx, f.path(), 500, true)
            .await
            .unwrap();
        assert_eq!(stats.code_systems, 1);
        assert_eq!(stats.concepts, 21);
        assert!(stats.errors.is_empty());
        assert_eq!(count_rows(&backend, "code_systems"), 0);
        assert_eq!(count_rows(&backend, "concepts"), 0);
    }

    #[tokio::test]
    async fn import_live_writes_concepts_hierarchy_and_version() {
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        let f = make_txt_file();

        let stats = import_icd10_pcs(&backend, &ctx, f.path(), 3, false)
            .await
            .unwrap();
        assert_eq!(stats.code_systems, 1);
        assert_eq!(stats.concepts, 21);
        assert_eq!(count_rows(&backend, "code_systems"), 1);
        assert_eq!(count_rows(&backend, "concepts"), 21);
        assert_eq!(count_rows(&backend, "concept_hierarchy"), 20);

        let conn = backend.pool().get().unwrap();
        let (url, version): (String, Option<String>) = conn
            .query_row("SELECT url, version FROM code_systems", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(url, ICD10PCS_URL);
        assert_eq!(version.as_deref(), Some("2025"));

        let (display, definition): (String, Option<String>) = conn
            .query_row(
                "SELECT display, definition FROM concepts WHERE code = ?1",
                rusqlite::params!["0DTJ0ZZ"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(display, "Resection of Appendix, Open Approach");
        assert!(definition.is_none());

        let header_def: Option<String> = conn
            .query_row(
                "SELECT definition FROM concepts WHERE code = ?1",
                rusqlite::params!["0DT"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(header_def.as_deref(), Some("header"));
    }

    #[tokio::test]
    async fn import_is_idempotent() {
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        let f = make_txt_file();

        import_icd10_pcs(&backend, &ctx, f.path(), 500, false)
            .await
            .unwrap();
        import_icd10_pcs(&backend, &ctx, f.path(), 500, false)
            .await
            .unwrap();
        assert_eq!(count_rows(&backend, "code_systems"), 1);
        assert_eq!(count_rows(&backend, "concepts"), 21);
        assert_eq!(count_rows(&backend, "concept_hierarchy"), 20);
    }

    #[tokio::test]
    async fn import_from_cms_zip_with_tables_xml() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("icd10pcs_tables_2025.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::FileOptions::default();
            zip.start_file("icd10pcsCodesFile.pdf", opts).unwrap();
            zip.write_all(b"%PDF-1.4 not really").unwrap();
            zip.start_file("codes_addenda_2025.txt", opts).unwrap();
            zip.write_all(b"Add: 0DTJ0ZZ ...").unwrap();
            zip.start_file("icd10pcs_codes_2025.txt", opts).unwrap();
            zip.write_all(CODES_TXT.as_bytes()).unwrap();
            zip.start_file("icd10pcs_tables_2025.xml", opts).unwrap();
            zip.write_all(TABLES_XML.as_bytes()).unwrap();
            zip.finish().unwrap();
        }

        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        let stats = import_icd10_pcs(&backend, &ctx, &zip_path, 500, false)
            .await
            .unwrap();
        assert_eq!(stats.concepts, 21);
        assert!(stats.errors.is_empty(), "{:?}", stats.errors);

        let conn = backend.pool().get().unwrap();
        let gz: String = conn
            .query_row(
                "SELECT display FROM concepts WHERE code = ?1",
                rusqlite::params!["GZ"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(gz, "None");
    }

    #[tokio::test]
    async fn import_missing_file_returns_error() {
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        let result = import_icd10_pcs(
            &backend,
            &ctx,
            Path::new("/nonexistent/icd10pcs_codes_2025.txt"),
            500,
            false,
        )
        .await;
        assert!(result.is_err());
    }
}
