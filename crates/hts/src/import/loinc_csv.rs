//! LOINC CSV importer.
//!
//! Reads a LOINC distribution ZIP and imports LOINC codes, preferred display
//! names, status, and the multi-axial hierarchy into the HTS normalized schema.
//!
//! ## Linguistic variants (language translations)
//!
//! Official LOINC releases ship language translations under
//! `AccessoryFiles/LinguisticVariants/`: an index file `LinguisticVariants.csv`
//! (mapping a numeric variant id → ISO language / country / display name) plus
//! one CSV per language named like `frFR28LinguisticVariant.csv`. Each such file
//! carries translated `LONG_COMMON_NAME` / `SHORTNAME` columns keyed by
//! `LOINC_NUM`. When present, those translations are imported as FHIR
//! `concept.designation` entries tagged with the BCP-47 language (e.g. `fr-FR`,
//! `de-DE`). The English `display` is left untouched. Because both the `hts
//! import` CLI and the server bootstrap funnel LOINC ZIPs through
//! [`import_loinc_csv`], language variants are picked up automatically by both
//! paths — no extra flag or file is required. The imported language set can
//! optionally be restricted via [`LanguageFilter`] (`HTS_IMPORT_LANGUAGES` /
//! `--languages`); excluded variant files are skipped without being parsed.
//!
//! ## Parts and answer lists
//!
//! The main table and the hierarchy give the LOINC terms and the parts that
//! appear in the multiaxial hierarchy. Official releases also ship
//! `AccessoryFiles/PartFile/Part.csv` (every part, about 42 000 of them in
//! no hierarchy) and `AccessoryFiles/AnswerFile/AnswerList.csv` (answer
//! lists `LL…` and their answers `LA…`). When present they are imported
//! too, unless `HTS_LOINC_PARTS_AND_ANSWERS=false`:
//!
//! * every part, answer list and answer becomes a concept of
//!   `http://loinc.org` (for 2.80: 243 550 concepts in all, the same count
//!   HAPI's loader reports, instead of 176 668);
//! * each answer list with answers becomes a ValueSet
//!   `http://loinc.org/vs/<LL…>` enumerating its answers in sequence order;
//! * `LoincAnswerListLink.csv` gives each linked term an `answer-list`
//!   property.
//!
//! ## Properties
//!
//! Following the LOINC FHIR terminology guidance (<https://loinc.org/fhir/>,
//! property codes as in the official LOINC `CodeSystem` resource), each LOINC
//! term carries
//!
//! * the `string` properties of `Loinc.csv` listed in
//!   [`LOINC_STRING_PROPERTIES`] (`STATUS`, `CLASSTYPE`, `ORDER_OBS`, …);
//! * one `Coding` property per row of `LoincPartLink_Primary.csv` and
//!   `LoincPartLink_Supplementary.csv` — `COMPONENT`, `PROPERTY`,
//!   `TIME_ASPCT`, `SYSTEM`, `SCALE_TYP`, `METHOD_TYP`, `CLASS`, `analyte`,
//!   `category`, `document-kind`, `rad-modality-type`, … — whose value is the
//!   part code (`LP…`). The `search` links (278 000 rows that no filter uses)
//!   are left out.
//!
//! A ValueSet filter on one of the part properties may name the part either
//! by code (`SCALE_TYP = LP32888-7`) or by name (`SCALE_TYP = DOC`, matched
//! case-insensitively against the parts' names; see
//! [`crate::loinc_filters`]). Each part also gets its `PartName` and
//! `PartDisplayName` as designations, so `Bld` is a valid display of
//! `LP7057-5` (display `Blood`), and each term its `SHORTNAME`
//! (`Calcium SerPl-mCnc` for 17861-6); both are `en-US` designations.
//! `HTS_LOINC_PROPERTIES=false` skips the properties (the designations stay:
//! they are displays). An existing import gets the properties and the
//! designations without re-importing its concepts through
//! `hts import --format loinc-properties <zip>` ([`backfill_loinc_properties`]).
//!
//! # ⚠️  LICENSE REQUIRED
//!
//! Real LOINC data requires a free license from the Regenstrief Institute.
//! Register at <https://loinc.org/license/> (takes ~5 minutes, no approval wait).
//! This parser was written and tested using **synthetic fixture data only**.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use helios_persistence::tenant::TenantContext;
use zip::ZipArchive;

use crate::error::HtsError;
use crate::import::BundleImportBackend;
use crate::import::ImportStats;
use crate::import::LanguageFilter;
use crate::import::PropertyRow;
use crate::import::bundle_builder::{
    BuilderConcept, BuilderDesignation, BuilderProperty, CodeSystemMeta, build_parsed_code_system,
};

// ── LOINC constants ───────────────────────────────────────────────────────────

const LOINC_URL: &str = "http://loinc.org";
const LOINC_ID: &str = "loinc";
const LOINC_NAME: &str = "LOINC";
const LOINC_TITLE: &str = "Logical Observation Identifiers Names and Codes (LOINC)";

// ── Data types ────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct LoincConcept {
    display: String,
    /// Raw STATUS value from LoincTable: ACTIVE, DEPRECATED, DISCOURAGED, TRIAL.
    status: String,
    /// Non-empty [`LOINC_STRING_PROPERTIES`] values, in that order.
    props: Vec<(&'static str, String)>,
    /// The term's SHORTNAME when it differs from `display`. tx.fhir.org
    /// accepts it as a valid display (DisplayName and CONSUMER_NAME it does
    /// not: 'Physical findings Narrative' for 29545-1 is wrong), so it
    /// becomes an `en-US` designation.
    alternates: Vec<String>,
}

/// The `en-US` designations for a concept's alternate English names.
fn english_designations(alternates: &[String]) -> Vec<OwnedDesignation> {
    alternates
        .iter()
        .map(|value| OwnedDesignation {
            language: "en-US".to_string(),
            value: value.clone(),
        })
        .collect()
}

/// `Loinc.csv` columns stored as `string` properties, under the same codes
/// the official LOINC FHIR CodeSystem uses.
pub(crate) const LOINC_STRING_PROPERTIES: &[&str] = &[
    "STATUS",
    "CLASSTYPE",
    "ORDER_OBS",
    "VersionFirstReleased",
    "VersionLastChanged",
    "CHNG_TYPE",
    "UNITSREQUIRED",
    "EXAMPLE_UCUM_UNITS",
    "EXAMPLE_UNITS",
    "PanelType",
];

/// Part-link properties not stored: the `search` links are 278 000 rows of
/// search keywords that no ValueSet filter uses.
const SKIPPED_PART_LINK_PROPERTIES: &[&str] = &["search"];

/// One stored property of a LOINC term: (code, FHIR value key, value).
type OwnedProperty = (std::sync::Arc<str>, &'static str, String);

/// Merged concept map: code → (display, optional definition, optional parent).
type ConceptEntries = HashMap<String, MergedConcept>;

#[derive(Debug, Default)]
struct MergedConcept {
    display: String,
    definition: Option<String>,
    parent: Option<String>,
    /// Translated display names from LOINC linguistic-variant files. Owned so
    /// the borrowed [`BuilderDesignation`] slice can be assembled per batch.
    designations: Vec<OwnedDesignation>,
    /// `answer-list` codes from `LoincAnswerListLink.csv`.
    answer_lists: Vec<String>,
    /// String and part-link properties (see the module docs).
    properties: Vec<OwnedProperty>,
}

/// One answer list from `AnswerList.csv`.
#[derive(Debug, Default)]
struct AnswerList {
    name: String,
    oid: Option<String>,
    /// `(sequence, LA code, display)`, sorted by sequence.
    answers: Vec<(u32, String, String)>,
}

/// One translated display name, owned by its [`MergedConcept`].
#[derive(Debug, Clone)]
struct OwnedDesignation {
    /// BCP-47 language tag, e.g. `fr-FR`, `de-DE`, `zh-CN`.
    language: String,
    value: String,
}

#[derive(Debug, Default)]
struct LoincParseResult {
    concepts: ConceptEntries,
    loinc_count: usize,
    lp_count: usize,
    edge_count: usize,
    designation_count: usize,
    /// Per-language designation counts, sorted for deterministic reporting.
    designations_by_language: Vec<(String, usize)>,
    parse_errors: Vec<String>,
    /// Release version parsed from a `Loinc_<ver>/` ZIP path segment, if present.
    release_version: Option<String>,
    /// Parts from `Part.csv` that the hierarchy did not already provide.
    extra_part_count: usize,
    /// Answer lists keyed by `LL…` code, in code order.
    answer_lists: std::collections::BTreeMap<String, AnswerList>,
    answer_count: usize,
    /// Properties attached to terms (string + part links).
    property_count: usize,
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Import a LOINC distribution ZIP through the given backend.
///
/// `languages` restricts which linguistic-variant translations are ingested
/// as designations (see [`LanguageFilter`]); excluded variant files are never
/// parsed. The English main-table content is unaffected.
pub async fn import_loinc_csv(
    backend: &dyn BundleImportBackend,
    ctx: &TenantContext,
    path: &Path,
    batch_size: usize,
    dry_run: bool,
    languages: &LanguageFilter,
) -> Result<ImportStats, HtsError> {
    const FORMAT: &str = "loinc";
    let batch_size = batch_size.max(1);

    let path_owned = path.to_path_buf();
    let languages = languages.clone();
    let parsed = tokio::task::spawn_blocking(move || -> Result<LoincParseResult, HtsError> {
        let paths = find_loinc_paths(&path_owned)?;
        let loinc_table_path = &paths.loinc_table;
        let hierarchy_path = &paths.hierarchy;

        tracing::info!(
            loinc_table = %loinc_table_path,
            hierarchy = %hierarchy_path,
            linguistic_variants = paths.lv_variants.len(),
            "Located LOINC CSV files in archive"
        );

        let mut parse_errors: Vec<String> = Vec::new();

        let loinc_concepts = {
            let mut zip = open_zip(&path_owned)?;
            let entry = zip
                .by_name(loinc_table_path)
                .map_err(|e| HtsError::InvalidRequest(format!("Cannot open LoincTable: {e}")))?;
            parse_loinc_table(entry, &mut parse_errors)?
        };

        let (hierarchy_concepts, edges) = {
            let mut zip = open_zip(&path_owned)?;
            let entry = zip.by_name(hierarchy_path).map_err(|e| {
                HtsError::InvalidRequest(format!("Cannot open MultiAxialHierarchy: {e}"))
            })?;
            parse_hierarchy(entry, &mut parse_errors)?
        };

        // Linguistic variants (language translations). The index file maps a
        // numeric variant id → ISO language/country; per-language files carry
        // the translated LONG_COMMON_NAME keyed by LOINC_NUM. Each parsed file
        // yields (loinc_code, language_tag, translated_display) tuples.
        let lv_index = match &paths.lv_index {
            Some(idx_path) => {
                let mut zip = open_zip(&path_owned)?;
                match zip.by_name(idx_path) {
                    Ok(entry) => parse_linguistic_index(entry, &mut parse_errors),
                    Err(e) => {
                        parse_errors.push(format!(
                            "Cannot open {idx_path}: {e} — \
                             language tags will be derived from filenames"
                        ));
                        HashMap::new()
                    }
                }
            }
            None => HashMap::new(),
        };

        // First pass: resolve every linguistic-variant file's language tag so
        // the language filter can make a *best-tier* decision over the whole
        // candidate set (e.g. keep `es-ES` and drop `es-AR`/`es-MX`), rather
        // than admitting every tag that loosely matches.
        let mut variant_langs: Vec<(&String, String, String)> = Vec::new();
        for variant_path in &paths.lv_variants {
            let filename = variant_path
                .rsplit('/')
                .next()
                .unwrap_or(variant_path)
                .to_string();
            match derive_language_tag(&filename, &lv_index) {
                Some(language) => variant_langs.push((variant_path, filename, language)),
                None => parse_errors.push(format!(
                    "Cannot determine language for linguistic-variant file '{filename}' — skipped"
                )),
            }
        }
        // Best-tier retained set across all candidate languages. English
        // designations are always kept (consistent with SNOMED), independent of
        // the filter.
        let retained = languages.resolve_retained(variant_langs.iter().map(|(_, _, l)| l.as_str()));

        // code → (language → translated display). Last writer wins per language.
        let mut translations: HashMap<String, HashMap<String, String>> = HashMap::new();
        for (variant_path, filename, language) in &variant_langs {
            if !crate::language::lang_matches("en", language) && !retained.contains(language) {
                tracing::info!(
                    file = %filename,
                    language = %language,
                    "skipping LOINC linguistic variant excluded by language filter"
                );
                continue;
            }
            let mut zip = open_zip(&path_owned)?;
            let entry = match zip.by_name(variant_path.as_str()) {
                Ok(e) => e,
                Err(e) => {
                    parse_errors.push(format!("Cannot open {variant_path}: {e} — skipped"));
                    continue;
                }
            };
            let rows = parse_linguistic_variant(entry, filename, &mut parse_errors)?;
            for (code, value) in rows {
                translations
                    .entry(code)
                    .or_default()
                    .insert(language.clone(), value);
            }
        }

        // Build parent map from the edges (child → parent).
        let mut parent_of: HashMap<String, String> = HashMap::new();
        for (child, parent) in &edges {
            // A code may appear under multiple parents; keep the first edge
            // seen to match the old behaviour where the `concept_hierarchy`
            // table stored only one incoming edge per child/parent pair.
            parent_of
                .entry(child.clone())
                .or_insert_with(|| parent.clone());
        }

        let mut all_concepts: ConceptEntries = HashMap::new();
        for (code, display) in &hierarchy_concepts {
            all_concepts.insert(
                code.clone(),
                MergedConcept {
                    display: display.clone(),
                    definition: None,
                    parent: parent_of.get(code).cloned(),
                    ..Default::default()
                },
            );
        }
        let with_properties = loinc_properties_enabled();
        let mut interner = PropertyInterner::default();
        for (code, concept) in &loinc_concepts {
            let definition = if concept.status != "ACTIVE" {
                Some(format!("STATUS:{}", concept.status))
            } else {
                None
            };
            let properties = if with_properties {
                concept
                    .props
                    .iter()
                    .map(|(p, v)| (interner.get(p), "valueString", v.clone()))
                    .collect()
            } else {
                Vec::new()
            };
            all_concepts.insert(
                code.clone(),
                MergedConcept {
                    display: concept.display.clone(),
                    definition,
                    parent: parent_of.get(code).cloned(),
                    properties,
                    designations: english_designations(&concept.alternates),
                    ..Default::default()
                },
            );
        }

        // Parts, answer lists and answers (see the module docs).
        let (extra_part_count, answer_lists, answer_count) = if parts_and_answers_enabled() {
            load_parts_and_answers(&path_owned, &paths, &mut all_concepts, &mut parse_errors)?
        } else {
            Default::default()
        };

        // Part links: one `Coding` property per (term, property, part).
        if with_properties {
            for name in &paths.part_links {
                let bytes = read_entry(&path_owned, name)?;
                for (loinc, property, part) in parse_part_links(bytes.as_slice(), name, &mut parse_errors)? {
                    if let Some(concept) = all_concepts.get_mut(&loinc) {
                        let property = interner.get(&property);
                        if !concept
                            .properties
                            .iter()
                            .any(|(p, _, v)| *p == property && *v == part)
                        {
                            concept.properties.push((property, "valueCoding", part));
                        }
                    }
                }
            }
        }
        let property_count = all_concepts.values().map(|c| c.properties.len()).sum();

        // Attach translated display names as designations on the concepts that
        // exist in this CodeSystem. Translations for unknown codes are recorded
        // as parse errors rather than creating orphan concepts.
        let mut designation_count: usize = 0;
        let mut per_language: HashMap<String, usize> = HashMap::new();
        for (code, langs) in translations {
            let Some(concept) = all_concepts.get_mut(&code) else {
                parse_errors.push(format!(
                    "Linguistic variant for unknown LOINC code '{code}' — skipped"
                ));
                continue;
            };
            // Sort by language for deterministic designation ordering.
            let mut langs: Vec<(String, String)> = langs.into_iter().collect();
            langs.sort_by(|a, b| a.0.cmp(&b.0));
            for (language, value) in langs {
                *per_language.entry(language.clone()).or_default() += 1;
                designation_count += 1;
                concept
                    .designations
                    .push(OwnedDesignation { language, value });
            }
        }

        let mut designations_by_language: Vec<(String, usize)> = per_language.into_iter().collect();
        designations_by_language.sort_by(|a, b| a.0.cmp(&b.0));

        Ok(LoincParseResult {
            concepts: all_concepts,
            loinc_count: loinc_concepts.len(),
            lp_count: hierarchy_concepts.len(),
            edge_count: edges.len(),
            designation_count,
            designations_by_language,
            parse_errors,
            release_version: paths.release_version,
            extra_part_count,
            answer_lists,
            answer_count,
            property_count,
        })
    })
    .await
    .map_err(|e| HtsError::Internal(format!("LOINC parser panicked: {e}")))??;

    let total_concepts = parsed.concepts.len() as u32;

    let mut stats = ImportStats {
        code_systems: 1,
        errors: parsed.parse_errors,
        ..Default::default()
    };

    let lang_summary = format_language_summary(&parsed.designations_by_language);

    if dry_run {
        stats.concepts = total_concepts;
        eprintln!(
            "[{FORMAT}] dry-run — would import {total_concepts} concepts \
             ({} LOINC codes, {} LP category nodes, {} further parts, {} answer lists, \
             {} answers), {} hierarchy edges, {} properties, {} translated designations{lang_summary}",
            parsed.loinc_count,
            parsed.lp_count,
            parsed.extra_part_count,
            parsed.answer_lists.len(),
            parsed.answer_count,
            parsed.edge_count,
            parsed.property_count,
            parsed.designation_count,
        );
        stats.value_sets = answer_list_value_sets(&parsed.answer_lists) as u32;
        return Ok(stats);
    }

    let release_version = parsed.release_version.clone();
    let meta = CodeSystemMeta {
        id: LOINC_ID,
        url: LOINC_URL,
        version: release_version.as_deref(),
        name: Some(LOINC_NAME),
        title: Some(LOINC_TITLE),
        status: "active",
        content: "complete",
    };

    // Probe once before loading: if the system has no concepts yet, every
    // concept in every batch is brand-new, so the per-concept
    // delete-before-reinsert can be skipped across the whole load. On a
    // re-import (system already populated) we leave it off so replacement
    // semantics still apply. Probe failure falls back to the safe path.
    let fresh_load = !backend
        .code_system_has_concepts(ctx, LOINC_URL)
        .await
        .unwrap_or(true);

    // Seed: empty CodeSystem to upsert metadata.
    let seed = build_parsed_code_system(&meta, &[]);
    let seed_stats = backend.import_parsed(ctx, seed).await?;
    stats.code_systems = seed_stats.code_systems;
    stats.errors.extend(seed_stats.errors);

    // Collect concepts into a stable order so chunks across retries are
    // deterministic.
    let concept_list: Vec<(&String, &MergedConcept)> = parsed.concepts.iter().collect();
    let num_batches = concept_list.len().div_ceil(batch_size).max(1);

    for (i, chunk) in concept_list.chunks(batch_size).enumerate() {
        // Build the borrowed designation slices first so they outlive the
        // `BuilderConcept`s that reference them within this iteration.
        let desig_sets: Vec<Vec<BuilderDesignation<'_>>> = chunk
            .iter()
            .map(|(_, entry)| {
                entry
                    .designations
                    .iter()
                    .map(|d| BuilderDesignation {
                        language: Some(d.language.as_str()),
                        use_system: None,
                        use_code: None,
                        value: d.value.as_str(),
                    })
                    .collect()
            })
            .collect();

        let prop_sets: Vec<Vec<BuilderProperty<'_>>> = chunk
            .iter()
            .map(|(_, entry)| {
                entry
                    .answer_lists
                    .iter()
                    .map(|ll| BuilderProperty {
                        code: "answer-list",
                        value_key: "valueCode",
                        value: ll.as_str(),
                    })
                    .chain(entry.properties.iter().map(|(code, key, value)| BuilderProperty {
                        code,
                        value_key: key,
                        value: value.as_str(),
                    }))
                    .collect()
            })
            .collect();

        let builder: Vec<BuilderConcept<'_>> = chunk
            .iter()
            .zip(desig_sets.iter().zip(&prop_sets))
            .map(|((code, entry), (desigs, props))| BuilderConcept {
                code: code.as_str(),
                display: Some(entry.display.as_str()).filter(|s| !s.is_empty()),
                definition: entry.definition.as_deref(),
                parent_code: entry.parent.as_deref(),
                designations: desigs.as_slice(),
                extra_properties: props.as_slice(),
            })
            .collect();

        let mut parsed = build_parsed_code_system(&meta, &builder);
        parsed.fresh_load = fresh_load;
        parsed.chunked = true;
        let chunk_stats = backend.import_parsed(ctx, parsed).await?;
        stats.errors.extend(chunk_stats.errors);
        stats.concepts += chunk.len() as u32;

        crate::import::progress::report_batch(i + 1, num_batches);
        eprintln!(
            "[{FORMAT}] concept batch {}/{num_batches} — +{} concepts (total: {})",
            i + 1,
            chunk.len(),
            stats.concepts,
        );
    }

    if parsed.designation_count > 0 {
        eprintln!(
            "[{FORMAT}] imported {} translated designations{lang_summary}",
            parsed.designation_count,
        );
    }

    // One ValueSet per answer list, `http://loinc.org/vs/<LL…>`.
    let lists: Vec<(&String, &AnswerList)> = parsed
        .answer_lists
        .iter()
        .filter(|(_, l)| !l.answers.is_empty())
        .collect();
    for chunk in lists.chunks(500) {
        let bundle = answer_list_bundle(chunk, release_version.as_deref());
        let vs_stats = backend.import_bundle(ctx, &bundle).await?;
        stats.value_sets += vs_stats.value_sets;
        stats.errors.extend(vs_stats.errors);
    }
    if !lists.is_empty() {
        eprintln!(
            "[{FORMAT}] imported {} answer lists as ValueSets ({} further parts, {} answers as concepts)",
            lists.len(),
            parsed.extra_part_count,
            parsed.answer_count,
        );
    }

    Ok(stats)
}

/// Adds the LOINC properties, part-name and SHORTNAME designations (see the module
/// docs) to an existing LOINC import, without re-importing its concepts:
/// `hts import --format loinc-properties <Loinc_x.yz.zip>`.
///
/// Reads `Loinc.csv`, the `LoincPartLink_*.csv` files and `Part.csv` from
/// the distribution, replaces every stored value of the properties it
/// writes (so a rerun does not duplicate them) and adds the part names and
/// term short names that are missing (a designation equal, ignoring case, to
/// the display or to an existing designation is skipped, so reruns and
/// databases backfilled by an earlier version get no duplicates). Targets the CodeSystem `http://loinc.org` of the archive's
/// release version, else the only stored version. Concepts and hierarchy
/// stay as they are; cached expansions over LOINC are dropped. A server
/// running against the same database keeps in-memory caches until it is
/// restarted (run it as an admin import job to avoid that).
pub async fn backfill_loinc_properties(
    backend: &dyn BundleImportBackend,
    ctx: &TenantContext,
    path: &Path,
    batch_size: usize,
    dry_run: bool,
) -> Result<ImportStats, HtsError> {
    const FORMAT: &str = "loinc-properties";
    // Property rows per backend call: large batches, as each is a few
    // UNNEST / prepared-statement inserts.
    let batch_size = batch_size.max(1).saturating_mul(10).max(10_000);
    let path_owned = path.to_path_buf();
    #[allow(clippy::type_complexity)]
    let (version, rows, designations, errors) = tokio::task::spawn_blocking(
        move || -> Result<(Option<String>, Vec<PropertyRow>, Vec<(String, String, String)>, Vec<String>), HtsError> {
            let paths = find_loinc_paths(&path_owned)?;
            let mut errors = Vec::new();
            let table = parse_loinc_table(
                read_entry(&path_owned, &paths.loinc_table)?.as_slice(),
                &mut errors,
            )?;
            let mut rows: Vec<PropertyRow> = Vec::new();
            let mut codes: Vec<&String> = table.keys().collect();
            codes.sort();
            for code in codes {
                for (p, v) in &table[code].props {
                    rows.push(PropertyRow {
                        code: code.clone(),
                        property: (*p).to_string(),
                        value_type: "string".into(),
                        value: v.clone(),
                    });
                }
            }
            let mut designations = Vec::new();
            for code in table.keys() {
                for alternate in &table[code].alternates {
                    designations.push((
                        code.clone(),
                        PART_NAME_LANGUAGE.to_string(),
                        alternate.clone(),
                    ));
                }
            }
            let mut seen: HashSet<(String, String, String)> = HashSet::new();
            for name in &paths.part_links {
                for (loinc, property, part) in
                    parse_part_links(read_entry(&path_owned, name)?.as_slice(), name, &mut errors)?
                {
                    if !table.contains_key(&loinc) {
                        continue;
                    }
                    let key = (loinc, property, part);
                    if seen.contains(&key) {
                        continue;
                    }
                    seen.insert(key.clone());
                    let (code, property, value) = key;
                    rows.push(PropertyRow {
                        code,
                        property,
                        value_type: "coding".into(),
                        value,
                    });
                }
            }
            if paths.part_links.is_empty() {
                errors.push(
                    "no LoincPartLink_*.csv in the archive: only the Loinc.csv properties are written"
                        .into(),
                );
            }
            if let Some(name) = &paths.parts {
                for part in parse_parts(read_entry(&path_owned, name)?.as_slice(), &mut errors)? {
                    let mut values = vec![part.display];
                    if !values[0].eq_ignore_ascii_case(&part.name) && !part.name.is_empty() {
                        values.push(part.name);
                    }
                    for value in values.into_iter().filter(|v| !v.is_empty()) {
                        designations.push((
                            part.code.clone(),
                            PART_NAME_LANGUAGE.to_string(),
                            value,
                        ));
                    }
                }
            }
            Ok((paths.release_version, rows, designations, errors))
        },
    )
    .await
    .map_err(|e| HtsError::Internal(format!("LOINC parser panicked: {e}")))??;

    let mut stats = ImportStats {
        errors,
        ..Default::default()
    };
    let mut clear: Vec<String> = rows.iter().map(|r| r.property.clone()).collect();
    clear.sort();
    clear.dedup();
    if dry_run {
        eprintln!(
            "[{FORMAT}] dry-run — would write {} properties ({} property codes) and up to {} \
             part-name/short-name designations",
            rows.len(),
            clear.len(),
            designations.len()
        );
        return Ok(stats);
    }
    eprintln!(
        "[{FORMAT}] writing {} properties ({} property codes) and up to {} part-name/short-name designations",
        rows.len(),
        clear.len(),
        designations.len()
    );
    let version = match version {
        Some(v)
            if backend
                .write_concept_properties(ctx, LOINC_URL, Some(&v), &[], vec![], vec![])
                .await
                .is_ok() =>
        {
            Some(v)
        }
        _ => None,
    };
    let num_batches = rows.len().div_ceil(batch_size).max(1);
    let mut rows = rows.into_iter().peekable();
    let mut designations = Some(designations);
    let (mut props_written, mut desig_written) = (0u64, 0u64);
    for i in 0..num_batches {
        let chunk: Vec<PropertyRow> = rows.by_ref().take(batch_size).collect();
        let first = i == 0;
        let (p, d) = backend
            .write_concept_properties(
                ctx,
                LOINC_URL,
                version.as_deref(),
                if first { &clear } else { &[] },
                chunk,
                if first {
                    designations.take().unwrap_or_default()
                } else {
                    Vec::new()
                },
            )
            .await?;
        props_written += p;
        desig_written += d;
        crate::import::progress::report_batch(i + 1, num_batches);
        eprintln!("[{FORMAT}] batch {}/{num_batches} — {props_written} properties", i + 1);
    }
    debug_assert!(rows.peek().is_none());
    eprintln!(
        "[{FORMAT}] wrote {props_written} properties and {desig_written} part-name/short-name designations"
    );
    stats.code_systems = 1;
    Ok(stats)
}

/// `HTS_LOINC_PARTS_AND_ANSWERS` (default on): import `Part.csv` and the
/// answer files.
fn parts_and_answers_enabled() -> bool {
    std::env::var("HTS_LOINC_PARTS_AND_ANSWERS").map_or(true, |v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

/// Read `Part.csv`, `AnswerList.csv` and `LoincAnswerListLink.csv` (each when
/// present) into `concepts`: parts the hierarchy did not provide, answer
/// lists and answers become concepts, and linked terms get their
/// `answer-list` codes. Returns `(parts added, answer lists, answers added)`.
#[allow(clippy::type_complexity)]
fn load_parts_and_answers(
    zip_path: &Path,
    paths: &LoincArchivePaths,
    concepts: &mut ConceptEntries,
    errors: &mut Vec<String>,
) -> Result<(usize, std::collections::BTreeMap<String, AnswerList>, usize), HtsError> {
    let open = |name: &str| -> Result<Vec<u8>, HtsError> {
        use std::io::Read;
        let mut zip = open_zip(zip_path)?;
        let mut entry = zip
            .by_name(name)
            .map_err(|e| HtsError::InvalidRequest(format!("Cannot open {name}: {e}")))?;
        let mut buf = Vec::new();
        entry
            .read_to_end(&mut buf)
            .map_err(|e| HtsError::InvalidRequest(format!("Cannot read {name}: {e}")))?;
        Ok(buf)
    };
    let status_definition = |status: &str| {
        (!status.is_empty() && status != "ACTIVE").then(|| format!("STATUS:{status}"))
    };

    let mut extra_parts = 0;
    if let Some(name) = &paths.parts {
        for part in parse_parts(open(name)?.as_slice(), errors)? {
            let concept = match concepts.entry(part.code) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    extra_parts += 1;
                    slot.insert(MergedConcept {
                        display: part.display.clone(),
                        definition: status_definition(&part.status),
                        ..Default::default()
                    })
                }
                std::collections::hash_map::Entry::Occupied(slot) => slot.into_mut(),
            };
            for value in [part.name, part.display] {
                add_part_name(concept, value);
            }
        }
    }

    let mut lists = std::collections::BTreeMap::new();
    let mut answers = 0;
    if let Some(name) = &paths.answer_lists {
        lists = parse_answer_lists(open(name)?.as_slice(), errors)?;
        for (ll, list) in &lists {
            concepts.entry(ll.clone()).or_insert_with(|| MergedConcept {
                display: list.name.clone(),
                ..Default::default()
            });
            for (_, la, display) in &list.answers {
                if let std::collections::hash_map::Entry::Vacant(slot) = concepts.entry(la.clone())
                {
                    slot.insert(MergedConcept {
                        display: display.clone(),
                        ..Default::default()
                    });
                    answers += 1;
                }
            }
        }
    }

    if let Some(name) = &paths.answer_list_links {
        for (loinc, ll) in parse_answer_list_links(open(name)?.as_slice(), errors)? {
            match concepts.get_mut(&loinc) {
                Some(c) if !c.answer_lists.contains(&ll) => c.answer_lists.push(ll),
                Some(_) => {}
                None => errors.push(format!(
                    "Answer list link for unknown LOINC code '{loinc}' — skipped"
                )),
            }
        }
    }
    Ok((extra_parts, lists, answers))
}

/// Language of the part-name and short-name designations (LOINC is
/// published in en-US).
const PART_NAME_LANGUAGE: &str = "en-US";

/// Adds `value` as an English designation of a part unless it is empty or
/// already the display or a designation (case-insensitively: display checks
/// ignore case anyway).
fn add_part_name(concept: &mut MergedConcept, value: String) {
    if value.is_empty()
        || concept.display.eq_ignore_ascii_case(&value)
        || concept
            .designations
            .iter()
            .any(|d| d.value.eq_ignore_ascii_case(&value))
    {
        return;
    }
    concept.designations.push(OwnedDesignation {
        language: PART_NAME_LANGUAGE.to_string(),
        value,
    });
}

/// `HTS_LOINC_PROPERTIES` (default on): store the `Loinc.csv` string
/// properties and the part links.
fn loinc_properties_enabled() -> bool {
    std::env::var("HTS_LOINC_PROPERTIES").map_or(true, |v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

/// Shares one allocation per property code across the ~1.6 million stored
/// part links.
#[derive(Default)]
struct PropertyInterner(HashMap<String, std::sync::Arc<str>>);

impl PropertyInterner {
    fn get(&mut self, code: &str) -> std::sync::Arc<str> {
        if let Some(a) = self.0.get(code) {
            return a.clone();
        }
        let a: std::sync::Arc<str> = std::sync::Arc::from(code);
        self.0.insert(code.to_string(), a.clone());
        a
    }
}

/// Reads one ZIP entry fully.
fn read_entry(zip_path: &Path, name: &str) -> Result<Vec<u8>, HtsError> {
    use std::io::Read;
    let mut zip = open_zip(zip_path)?;
    let mut entry = zip
        .by_name(name)
        .map_err(|e| HtsError::InvalidRequest(format!("Cannot open {name}: {e}")))?;
    let mut buf = Vec::new();
    entry
        .read_to_end(&mut buf)
        .map_err(|e| HtsError::InvalidRequest(format!("Cannot read {name}: {e}")))?;
    Ok(buf)
}

/// How many answer lists become ValueSets (those with answers; externally
/// defined lists have none in the file).
fn answer_list_value_sets(lists: &std::collections::BTreeMap<String, AnswerList>) -> usize {
    lists.values().filter(|l| !l.answers.is_empty()).count()
}

/// A Bundle of ValueSets, one per answer list, each enumerating its answers
/// in sequence order.
fn answer_list_bundle(lists: &[(&String, &AnswerList)], version: Option<&str>) -> Vec<u8> {
    use serde_json::json;
    let entries: Vec<serde_json::Value> = lists
        .iter()
        .map(|(ll, list)| {
            let mut vs = json!({
                "resourceType": "ValueSet",
                "id": ll,
                "url": format!("{}/{ll}", crate::implicit_vs::LOINC_VS_URL),
                "name": format!("LOINC_Answer_List_{}", ll.replace('-', "_")),
                "title": list.name,
                "status": "active",
                "compose": {"include": [{
                    "system": LOINC_URL,
                    "concept": list.answers.iter()
                        .map(|(_, la, display)| json!({"code": la, "display": display}))
                        .collect::<Vec<_>>(),
                }]},
            });
            if let Some(v) = version {
                vs["version"] = json!(v);
            }
            if let Some(oid) = &list.oid {
                vs["identifier"] =
                    json!([{"system": "urn:ietf:rfc:3986", "value": format!("urn:oid:{oid}")}]);
            }
            json!({"resource": vs})
        })
        .collect();
    serde_json::to_vec(&json!({
        "resourceType": "Bundle",
        "type": "collection",
        "entry": entries,
    }))
    .unwrap_or_default()
}

// ── ZIP helpers ───────────────────────────────────────────────────────────────

fn open_zip(path: &Path) -> Result<ZipArchive<std::fs::File>, HtsError> {
    let file = std::fs::File::open(path)
        .map_err(|e| HtsError::InvalidRequest(format!("Cannot open {}: {e}", path.display())))?;
    ZipArchive::new(file)
        .map_err(|e| HtsError::InvalidRequest(format!("Not a valid ZIP archive: {e}")))
}

/// Locations of the CSV files HTS reads from a LOINC distribution ZIP.
#[derive(Debug, Default)]
struct LoincArchivePaths {
    loinc_table: String,
    hierarchy: String,
    /// `AccessoryFiles/LinguisticVariants/LinguisticVariants.csv`, if present.
    lv_index: Option<String>,
    /// Per-language `*LinguisticVariant.csv` files (excludes the index), sorted
    /// for deterministic processing order.
    lv_variants: Vec<String>,
    /// Parsed from a top-level `Loinc_<ver>/` directory in the ZIP, e.g. `2.82`.
    release_version: Option<String>,
    /// `AccessoryFiles/PartFile/Part.csv`, if present.
    parts: Option<String>,
    /// `AnswerList.csv` (the `AnswerFile/` copy preferred), if present.
    answer_lists: Option<String>,
    /// `LoincAnswerListLink.csv` (the `AnswerFile/` copy preferred), if present.
    answer_list_links: Option<String>,
    /// `LoincPartLink_Primary.csv` / `LoincPartLink_Supplementary.csv`
    /// (`LoincPartLink.csv` before 2.67), sorted.
    part_links: Vec<String>,
}

/// Extract LOINC release version from a ZIP entry path such as
/// `Loinc_2.82/LoincTable/Loinc.csv`.
///
/// Stored on `CodeSystem.version` at import time so unpinned `$validate-code`
/// and `version=current` resolution prefer the full release over empty HL7 or
/// consult-minimal stub rows. See `docs/ig-publisher-compatibility.md`.
pub(crate) fn loinc_release_version_from_entry(entry_path: &str) -> Option<String> {
    for segment in entry_path.split('/') {
        let lower = segment.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("loinc_") {
            // Accept the archive file name too (`Loinc_2.80.zip`).
            let ver = rest.trim().trim_end_matches(".zip");
            if !ver.is_empty() && ver.chars().all(|c| c.is_ascii_digit() || c == '.') {
                return Some(ver.to_string());
            }
        }
    }
    None
}

fn find_loinc_paths(path: &Path) -> Result<LoincArchivePaths, HtsError> {
    let mut zip = open_zip(path)?;

    let mut loinc_path: Option<String> = None;
    let mut hierarchy_path: Option<String> = None;
    let mut lv_index: Option<String> = None;
    let mut lv_variants: Vec<String> = Vec::new();
    let mut release_version: Option<String> = None;
    let mut parts: Option<String> = None;
    let mut answer_lists: Option<String> = None;
    let mut answer_list_links: Option<String> = None;
    let mut part_links: Vec<String> = Vec::new();
    // `PanelsAndForms/` carries copies of the answer files; `AnswerFile/` wins.
    let prefer_answer_file = |slot: &mut Option<String>, name: &str| {
        if slot.is_none() || name.to_lowercase().contains("answerfile/") {
            *slot = Some(name.to_string());
        }
    };

    for i in 0..zip.len() {
        let entry = zip
            .by_index(i)
            .map_err(|e| HtsError::InvalidRequest(format!("ZIP entry error: {e}")))?;
        let name = entry.name().to_string();
        let lower = name.to_lowercase();
        let filename = lower.rsplit('/').next().unwrap_or(&lower);

        if filename.ends_with(".csv") {
            // The main table is always exactly `Loinc.csv` or `LoincTable.csv`
            // (flat or under `Loinc_<ver>/` or `LoincTable/`). Accessory files
            // like `LoincPartLink_Primary.csv`, `LoincParts.csv`, or
            // `LoincUniversalLabOrdersValueSet.csv` would be false positives
            // for a looser prefix match.
            if filename == "loinc.csv" || filename == "loinctable.csv" {
                // Official ZIPs (≥ 2.74) also ship a copy of the main table as
                // `AccessoryFiles/PanelsAndForms/Loinc.csv`, and it sorts
                // *before* `LoincTable/Loinc.csv`. Prefer the entry living
                // under a `LoincTable/` directory; otherwise first match wins.
                let under_loinc_table = lower.contains("loinctable/");
                let replace = match &loinc_path {
                    None => true,
                    Some(current) => {
                        under_loinc_table && !current.to_lowercase().contains("loinctable/")
                    }
                };
                if replace {
                    release_version = loinc_release_version_from_entry(&name);
                    loinc_path = Some(name);
                }
            } else if filename.contains("multiaxial") || filename.contains("componenthierarchy") {
                hierarchy_path = Some(name);
            } else if filename == "part.csv" {
                parts = Some(name);
            } else if filename.starts_with("loincpartlink") {
                part_links.push(name);
            } else if filename == "answerlist.csv" {
                prefer_answer_file(&mut answer_lists, &name);
            } else if filename == "loincanswerlistlink.csv" {
                prefer_answer_file(&mut answer_list_links, &name);
            } else if filename == "linguisticvariants.csv" {
                // The translation index (note trailing 's').
                lv_index = Some(name);
            } else if filename.ends_with("linguisticvariant.csv") {
                // A per-language translation file, e.g. `frFR28LinguisticVariant.csv`.
                lv_variants.push(name);
            }
        }
    }

    lv_variants.sort();
    part_links.sort();

    // Current official ZIPs have no `Loinc_<ver>/` top-level folder — the
    // version is only in the archive name (`Loinc_2.80.zip`). Fall back to it
    // so `CodeSystem.version` is never left empty.
    if release_version.is_none() {
        release_version = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(loinc_release_version_from_entry);
    }

    Ok(LoincArchivePaths {
        loinc_table: loinc_path.ok_or_else(|| {
            HtsError::InvalidRequest(
                "No LoincTable CSV found. Expected a file whose name starts with 'loinc' or contains 'loinctable'.".into(),
            )
        })?,
        hierarchy: hierarchy_path.ok_or_else(|| {
            HtsError::InvalidRequest(
                "No hierarchy CSV found. Expected 'MultiAxialHierarchy.csv' (LOINC ≤ 2.73) or 'ComponentHierarchyBySystem.csv' (LOINC ≥ 2.74).".into(),
            )
        })?,
        lv_index,
        lv_variants,
        release_version,
        parts,
        answer_lists,
        answer_list_links,
        part_links,
    })
}

// ── Linguistic-variant helpers ─────────────────────────────────────────────────

/// Parse `LinguisticVariants.csv` into `variant_id → (iso_language, iso_country)`.
///
/// Columns: `ID, ISO_LANGUAGE, ISO_COUNTRY, LANGUAGE_NAME` (the documented
/// `LinguisticVariantId`/`IsoLanguage`/`IsoCountry` spellings are also
/// accepted). A missing or malformed index is non-fatal — callers fall back to
/// deriving the language tag from the per-variant filename.
fn parse_linguistic_index(
    reader: impl std::io::Read,
    errors: &mut Vec<String>,
) -> HashMap<String, (String, String)> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(reader);

    let headers = match rdr.headers() {
        Ok(h) => h.clone(),
        Err(e) => {
            errors.push(format!("Cannot read LinguisticVariants.csv headers: {e}"));
            return HashMap::new();
        }
    };

    // Real LOINC releases name these columns `ID`, `ISO_LANGUAGE`,
    // `ISO_COUNTRY`; accept the documented `LinguisticVariantId`/`IsoLanguage`/
    // `IsoCountry` spellings too so either form parses.
    let find_any = |names: &[&str]| names.iter().find_map(|n| find_col(&headers, n).ok());
    let id_idx = find_any(&["ID", "LinguisticVariantId"]);
    let lang_idx = find_any(&["ISO_LANGUAGE", "IsoLanguage"]);
    let country_idx = find_any(&["ISO_COUNTRY", "IsoCountry"]);

    let (Some(id_idx), Some(lang_idx)) = (id_idx, lang_idx) else {
        errors.push(
            "LinguisticVariants.csv missing ID/ISO_LANGUAGE columns — \
             language tags will be derived from filenames"
                .into(),
        );
        return HashMap::new();
    };

    let mut map: HashMap<String, (String, String)> = HashMap::new();
    for result in rdr.records() {
        let Ok(record) = result else { continue };
        let id = record.get(id_idx).unwrap_or("").trim().to_string();
        if id.is_empty() {
            continue;
        }
        let lang = record.get(lang_idx).unwrap_or("").trim().to_lowercase();
        let country = country_idx
            .and_then(|i| record.get(i))
            .unwrap_or("")
            .trim()
            .to_uppercase();
        if !lang.is_empty() {
            map.insert(id, (lang, country));
        }
    }
    map
}

/// Derive the BCP-47 language tag for a per-variant file.
///
/// Prefers the index (matched by the numeric id embedded in the filename),
/// falling back to the `<lang><COUNTRY>` letters that prefix the filename.
/// `filename` is the lowercased basename, e.g. `frfr28linguisticvariant.csv`.
fn derive_language_tag(
    filename: &str,
    index: &HashMap<String, (String, String)>,
) -> Option<String> {
    let lower = filename.to_lowercase();
    let stem = lower.strip_suffix("linguisticvariant.csv")?;

    // The numeric id (if any) sits between the language/country letters and the
    // `LinguisticVariant` suffix, e.g. `frfr28` → "28".
    let id: String = stem.chars().filter(|c| c.is_ascii_digit()).collect();
    if !id.is_empty() {
        if let Some((lang, country)) = index.get(&id) {
            return Some(format_lang_tag(lang, country));
        }
    }

    // Fallback: leading letters encode `<lang><country>` (e.g. `frfr`).
    let letters: String = stem
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if letters.len() >= 2 {
        let lang = &letters[0..2];
        let country = if letters.len() >= 4 {
            &letters[2..4]
        } else {
            ""
        };
        return Some(format_lang_tag(lang, country));
    }
    None
}

fn format_lang_tag(lang: &str, country: &str) -> String {
    let lang = lang.to_lowercase();
    if country.is_empty() {
        lang
    } else {
        format!("{lang}-{}", country.to_uppercase())
    }
}

/// LOINC axis columns, in fully-specified-name order. Used to synthesize a
/// translated display when a linguistic-variant file leaves the combined-name
/// columns empty (e.g. the es-ES and it-IT translations only populate the
/// individual axes).
const LOINC_AXIS_COLUMNS: [&str; 6] = [
    "COMPONENT",
    "PROPERTY",
    "TIME_ASPCT",
    "SYSTEM",
    "SCALE_TYP",
    "METHOD_TYP",
];

/// Parse one `*LinguisticVariant.csv` into `(loinc_code, translated_display)`
/// rows. The translated display prefers `LONG_COMMON_NAME`, then `SHORTNAME`,
/// then `LinguisticVariantDisplayName`. When none of those combined-name
/// columns carry text (as in the es-ES and it-IT variants, which translate only
/// the individual axes), the display is synthesized as a LOINC fully-specified
/// name — the populated `COMPONENT:PROPERTY:TIME_ASPCT:SYSTEM:SCALE_TYP:METHOD_TYP`
/// axes joined with `:`. Rows without a code or any translated text are skipped.
fn parse_linguistic_variant(
    reader: impl std::io::Read,
    filename: &str,
    errors: &mut Vec<String>,
) -> Result<Vec<(String, String)>, HtsError> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(reader);

    let headers = rdr
        .headers()
        .map_err(|e| HtsError::InvalidRequest(format!("Cannot read {filename} headers: {e}")))?;
    let headers = headers.clone();

    let code_idx = find_col(&headers, "LOINC_NUM")?;
    let long_idx = find_col(&headers, "LONG_COMMON_NAME").ok();
    let short_idx = find_col(&headers, "SHORTNAME").ok();
    let display_idx = find_col(&headers, "LinguisticVariantDisplayName").ok();
    let axis_idxs: Vec<usize> = LOINC_AXIS_COLUMNS
        .iter()
        .filter_map(|c| find_col(&headers, c).ok())
        .collect();

    if long_idx.is_none() && short_idx.is_none() && display_idx.is_none() && axis_idxs.is_empty() {
        errors.push(format!(
            "{filename}: no LONG_COMMON_NAME/SHORTNAME/LinguisticVariantDisplayName or axis \
             columns — no translations imported"
        ));
        return Ok(Vec::new());
    }

    let mut rows: Vec<(String, String)> = Vec::new();
    for result in rdr.records() {
        let record = result
            .map_err(|e| HtsError::InvalidRequest(format!("{filename} record error: {e}")))?;

        let code = record.get(code_idx).unwrap_or("").trim().to_string();
        if code.is_empty() {
            continue;
        }
        let pick = |idx: Option<usize>| {
            idx.and_then(|i| record.get(i))
                .map(str::trim)
                .filter(|s| !s.is_empty())
        };
        let value: String = match pick(long_idx)
            .or_else(|| pick(short_idx))
            .or_else(|| pick(display_idx))
        {
            Some(v) => v.to_string(),
            // No combined name — synthesize the LOINC fully-specified name from
            // the translated axis columns (e.g. es-ES, it-IT). Trailing empty
            // axes are dropped so a partly-populated row doesn't render dangling
            // `:` separators; interior gaps are kept to preserve axis position.
            None => {
                let mut axes: Vec<&str> = axis_idxs
                    .iter()
                    .map(|&i| record.get(i).unwrap_or("").trim())
                    .collect();
                while axes.last().is_some_and(|s| s.is_empty()) {
                    axes.pop();
                }
                axes.join(":")
            }
        };
        // A synthesized FSN of only empty axes collapses to ":::::" — treat
        // anything without an alphanumeric character as no translation.
        if !value.chars().any(|c| c.is_alphanumeric()) {
            continue;
        }
        rows.push((code, value));
    }
    Ok(rows)
}

/// Render a `" (fr-FR: 3, de-DE: 2)"`-style suffix for import logging, or an
/// empty string when there are no translations.
fn format_language_summary(by_language: &[(String, usize)]) -> String {
    if by_language.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = by_language
        .iter()
        .map(|(lang, n)| format!("{lang}: {n}"))
        .collect();
    format!(" ({})", parts.join(", "))
}

// ── CSV parsers ───────────────────────────────────────────────────────────────

fn find_col(headers: &csv::StringRecord, name: &str) -> Result<usize, HtsError> {
    headers
        .iter()
        .position(|h| h.trim() == name)
        .ok_or_else(|| {
            HtsError::InvalidRequest(format!("Required column '{name}' not found in CSV headers"))
        })
}

fn parse_loinc_table(
    reader: impl std::io::Read,
    errors: &mut Vec<String>,
) -> Result<HashMap<String, LoincConcept>, HtsError> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(reader);

    let headers = rdr
        .headers()
        .map_err(|e| HtsError::InvalidRequest(format!("Cannot read CSV headers: {e}")))?
        .clone();

    let code_idx = find_col(&headers, "LOINC_NUM")?;
    let long_idx = find_col(&headers, "LONG_COMMON_NAME").ok();
    // `LoincTable/Loinc.csv` spells it SHORTNAME; older/core tables ShortName.
    let short_idx = find_col(&headers, "SHORTNAME")
        .or_else(|_| find_col(&headers, "ShortName"))
        .ok();
    let status_idx = find_col(&headers, "STATUS").ok();
    let prop_idxs: Vec<(&'static str, usize)> = LOINC_STRING_PROPERTIES
        .iter()
        .filter_map(|p| find_col(&headers, p).ok().map(|i| (*p, i)))
        .collect();

    let mut concepts: HashMap<String, LoincConcept> = HashMap::new();
    let mut record_no: usize = 0;

    for result in rdr.records() {
        record_no += 1;
        let record =
            result.map_err(|e| HtsError::InvalidRequest(format!("CSV record error: {e}")))?;

        let code = record.get(code_idx).unwrap_or("").trim().to_string();
        if code.is_empty() {
            errors.push(format!(
                "LoincTable record {record_no}: LOINC_NUM is empty — skipped"
            ));
            continue;
        }

        let display = long_idx
            .and_then(|i| record.get(i))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                short_idx
                    .and_then(|i| record.get(i))
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or("")
            .to_string();

        let status = status_idx
            .and_then(|i| record.get(i))
            .map(str::trim)
            .unwrap_or("ACTIVE")
            .to_string();

        let props = prop_idxs
            .iter()
            .filter_map(|(p, i)| {
                let v = record.get(*i).map(str::trim).unwrap_or("");
                (!v.is_empty()).then(|| (*p, v.to_string()))
            })
            .collect();

        let alternates: Vec<String> = short_idx
            .and_then(|i| record.get(i))
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case(&display))
            .map(str::to_string)
            .into_iter()
            .collect();

        concepts.insert(
            code,
            LoincConcept {
                display,
                status,
                props,
                alternates,
            },
        );
    }

    Ok(concepts)
}

type HierarchyResult = (HashMap<String, String>, Vec<(String, String)>);

fn parse_hierarchy(
    reader: impl std::io::Read,
    errors: &mut Vec<String>,
) -> Result<HierarchyResult, HtsError> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(reader);

    let headers = rdr
        .headers()
        .map_err(|e| HtsError::InvalidRequest(format!("Cannot read hierarchy CSV headers: {e}")))?
        .clone();

    let code_idx = find_col(&headers, "CODE")?;
    let parent_idx = find_col(&headers, "IMMEDIATE_PARENT")?;
    let text_idx = find_col(&headers, "CODE_TEXT").ok();

    let mut concepts: HashMap<String, String> = HashMap::new();
    let mut edges: Vec<(String, String)> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut record_no: usize = 0;

    for result in rdr.records() {
        record_no += 1;
        let record = result
            .map_err(|e| HtsError::InvalidRequest(format!("Hierarchy CSV record error: {e}")))?;

        let code = record.get(code_idx).unwrap_or("").trim().to_string();
        if code.is_empty() {
            errors.push(format!(
                "MultiAxialHierarchy record {record_no}: CODE is empty — skipped"
            ));
            continue;
        }

        let text = text_idx
            .and_then(|i| record.get(i))
            .unwrap_or("")
            .trim()
            .to_string();

        concepts.entry(code.clone()).or_insert(text);

        let parent = record.get(parent_idx).unwrap_or("").trim().to_string();
        if !parent.is_empty() {
            concepts.entry(parent.clone()).or_default();
            let edge = (code, parent);
            if seen.insert(edge.clone()) {
                edges.push(edge);
            }
        }
    }

    Ok((concepts, edges))
}

/// One `Part.csv` row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PartRow {
    code: String,
    /// `PartDisplayName`, else `PartName`.
    display: String,
    /// `PartName` (e.g. `Bld` for display `Blood`).
    name: String,
    status: String,
}

/// `Part.csv` rows.
fn parse_parts(
    reader: impl std::io::Read,
    errors: &mut Vec<String>,
) -> Result<Vec<PartRow>, HtsError> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(reader);
    let headers = rdr
        .headers()
        .map_err(|e| HtsError::InvalidRequest(format!("Cannot read Part.csv headers: {e}")))?
        .clone();
    let code_idx = find_col(&headers, "PartNumber")?;
    let name_idx = find_col(&headers, "PartName").ok();
    let display_idx = find_col(&headers, "PartDisplayName").ok();
    let status_idx = find_col(&headers, "Status").ok();
    let field = |r: &csv::StringRecord, i: Option<usize>| {
        i.and_then(|i| r.get(i))
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let mut out = Vec::new();
    for (n, result) in rdr.records().enumerate() {
        let record =
            result.map_err(|e| HtsError::InvalidRequest(format!("Part.csv record error: {e}")))?;
        let code = field(&record, Some(code_idx));
        if code.is_empty() {
            errors.push(format!(
                "Part.csv record {}: PartNumber is empty — skipped",
                n + 1
            ));
            continue;
        }
        let name = field(&record, name_idx);
        let mut display = field(&record, display_idx);
        if display.is_empty() {
            display = name.clone();
        }
        out.push(PartRow {
            code,
            display,
            name,
            status: field(&record, status_idx),
        });
    }
    Ok(out)
}

/// `LoincPartLink_*.csv`: `(LoincNumber, property code, PartNumber)` per
/// row. The property code is the last segment of the `Property` column
/// (`http://loinc.org/property/SCALE_TYP` → `SCALE_TYP`). Rows linking to
/// another code system than LOINC, and [`SKIPPED_PART_LINK_PROPERTIES`],
/// are left out.
fn parse_part_links(
    reader: impl std::io::Read,
    name: &str,
    errors: &mut Vec<String>,
) -> Result<Vec<(String, String, String)>, HtsError> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(reader);
    let headers = rdr
        .headers()
        .map_err(|e| HtsError::InvalidRequest(format!("Cannot read {name} headers: {e}")))?
        .clone();
    // A file without the expected columns (a subset or an older layout) is
    // skipped, not fatal: the terms import without its links.
    let (Ok(loinc_idx), Ok(part_idx), Ok(property_idx)) = (
        find_col(&headers, "LoincNumber"),
        find_col(&headers, "PartNumber"),
        find_col(&headers, "Property"),
    ) else {
        errors.push(format!(
            "{name}: no LoincNumber/PartNumber/Property columns — part links not imported"
        ));
        return Ok(Vec::new());
    };
    let system_idx = find_col(&headers, "PartCodeSystem").ok();
    let mut out = Vec::new();
    for (n, result) in rdr.records().enumerate() {
        let record =
            result.map_err(|e| HtsError::InvalidRequest(format!("{name} record error: {e}")))?;
        let loinc = record.get(loinc_idx).unwrap_or("").trim();
        let part = record.get(part_idx).unwrap_or("").trim();
        let property = record
            .get(property_idx)
            .unwrap_or("")
            .trim()
            .rsplit('/')
            .next()
            .unwrap_or("");
        if loinc.is_empty() || part.is_empty() || property.is_empty() {
            errors.push(format!(
                "{name} record {}: empty LoincNumber, PartNumber or Property — skipped",
                n + 1
            ));
            continue;
        }
        let system = system_idx.and_then(|i| record.get(i)).unwrap_or("").trim();
        if (!system.is_empty() && system != LOINC_URL)
            || SKIPPED_PART_LINK_PROPERTIES.contains(&property)
        {
            continue;
        }
        out.push((loinc.to_string(), property.to_string(), part.to_string()));
    }
    Ok(out)
}

/// `AnswerList.csv`: one row per (list, answer); a list without answers
/// (externally defined) has one row with an empty `AnswerStringId`.
fn parse_answer_lists(
    reader: impl std::io::Read,
    errors: &mut Vec<String>,
) -> Result<std::collections::BTreeMap<String, AnswerList>, HtsError> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(reader);
    let headers = rdr
        .headers()
        .map_err(|e| HtsError::InvalidRequest(format!("Cannot read AnswerList.csv headers: {e}")))?
        .clone();
    let ll_idx = find_col(&headers, "AnswerListId")?;
    let name_idx = find_col(&headers, "AnswerListName").ok();
    let oid_idx = find_col(&headers, "AnswerListOID").ok();
    let la_idx = find_col(&headers, "AnswerStringId").ok();
    let seq_idx = find_col(&headers, "SequenceNumber").ok();
    let display_idx = find_col(&headers, "DisplayText").ok();
    let field = |r: &csv::StringRecord, i: Option<usize>| {
        i.and_then(|i| r.get(i))
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let mut lists: std::collections::BTreeMap<String, AnswerList> = Default::default();
    for (n, result) in rdr.records().enumerate() {
        let record = result
            .map_err(|e| HtsError::InvalidRequest(format!("AnswerList.csv record error: {e}")))?;
        let ll = field(&record, Some(ll_idx));
        if ll.is_empty() {
            errors.push(format!(
                "AnswerList.csv record {}: AnswerListId is empty — skipped",
                n + 1
            ));
            continue;
        }
        let list = lists.entry(ll).or_default();
        if list.name.is_empty() {
            list.name = field(&record, name_idx);
        }
        if list.oid.is_none() {
            list.oid = Some(field(&record, oid_idx)).filter(|s| !s.is_empty());
        }
        let la = field(&record, la_idx);
        if !la.is_empty() {
            let seq = field(&record, seq_idx).parse().unwrap_or(u32::MAX);
            list.answers.push((seq, la, field(&record, display_idx)));
        }
    }
    for list in lists.values_mut() {
        list.answers.sort();
        list.answers.dedup_by(|a, b| a.1 == b.1);
    }
    Ok(lists)
}

/// `LoincAnswerListLink.csv`: `(LoincNumber, AnswerListId)` pairs.
fn parse_answer_list_links(
    reader: impl std::io::Read,
    errors: &mut Vec<String>,
) -> Result<Vec<(String, String)>, HtsError> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(reader);
    let headers = rdr
        .headers()
        .map_err(|e| {
            HtsError::InvalidRequest(format!("Cannot read LoincAnswerListLink.csv headers: {e}"))
        })?
        .clone();
    let loinc_idx = find_col(&headers, "LoincNumber")?;
    let ll_idx = find_col(&headers, "AnswerListId")?;
    let mut out = Vec::new();
    for (n, result) in rdr.records().enumerate() {
        let record = result.map_err(|e| {
            HtsError::InvalidRequest(format!("LoincAnswerListLink.csv record error: {e}"))
        })?;
        let loinc = record.get(loinc_idx).unwrap_or("").trim();
        let ll = record.get(ll_idx).unwrap_or("").trim();
        if loinc.is_empty() || ll.is_empty() {
            errors.push(format!(
                "LoincAnswerListLink.csv record {}: empty LoincNumber or AnswerListId — skipped",
                n + 1
            ));
            continue;
        }
        out.push((loinc.to_string(), ll.to_string()));
    }
    Ok(out)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::backends::SqliteTerminologyBackend;
    use std::io::Write;
    use tempfile::NamedTempFile;

    const LOINC_TABLE_CSV: &str = "\
LOINC_NUM,LONG_COMMON_NAME,ShortName,STATUS\r\n\
2160-0,Creatinine [Mass/volume] in Serum or Plasma,Creat SerPl-mCnc,ACTIVE\r\n\
718-7,Hemoglobin [Mass/volume] in Blood,Hgb Bld-mCnc,ACTIVE\r\n\
99999-9,Old deprecated test,Old test,DEPRECATED\r\n";

    const HIERARCHY_CSV: &str = "\
PATH_TO_ROOT,SEQUENCE,IMMEDIATE_PARENT,CODE,CODE_TEXT\r\n\
LP7786-3,1,,LP7786-3,Laboratory\r\n\
LP7786-3.LP29693-6,2,LP7786-3,LP29693-6,Chemistry\r\n\
LP7786-3.LP29693-6.2160-0,3,LP29693-6,2160-0,Creatinine\r\n\
LP7786-3.LP10156-0,2,LP7786-3,LP10156-0,Hematology\r\n\
LP7786-3.LP10156-0.718-7,3,LP10156-0,718-7,Hemoglobin\r\n";

    fn make_test_loinc_zip() -> NamedTempFile {
        let tmp = NamedTempFile::with_suffix(".zip").unwrap();
        {
            let mut zip = zip::ZipWriter::new(tmp.reopen().unwrap());
            let opts = zip::write::FileOptions::default();

            zip.start_file("LoincTable.csv", opts).unwrap();
            zip.write_all(LOINC_TABLE_CSV.as_bytes()).unwrap();

            zip.start_file("MultiAxialHierarchy.csv", opts).unwrap();
            zip.write_all(HIERARCHY_CSV.as_bytes()).unwrap();

            zip.finish().unwrap();
        }
        tmp
    }

    // Linguistic-variant fixtures. The index maps numeric ids → language/country;
    // the per-variant files carry translated LONG_COMMON_NAME values.
    // Uses the real LOINC release column names (`ID`, `ISO_LANGUAGE`, …); the
    // documented `LinguisticVariantId`/`IsoLanguage` spellings are accepted too.
    const LV_INDEX_CSV: &str = "\
ID,ISO_LANGUAGE,ISO_COUNTRY,LANGUAGE_NAME,PRODUCER\r\n\
28,fr,FR,French (France),Some Producer\r\n\
8,de,DE,German (Germany),Some Producer\r\n";

    // es-ES / it-IT style: only the individual axes are translated; the
    // combined-name columns (LONG_COMMON_NAME/SHORTNAME/LinguisticVariantDisplayName)
    // are empty. The display must be synthesized from the axes.
    const LV_ES_AXES_CSV: &str = "\
LOINC_NUM,COMPONENT,PROPERTY,TIME_ASPCT,SYSTEM,SCALE_TYP,METHOD_TYP,CLASS,SHORTNAME,LONG_COMMON_NAME,RELATEDNAMES2,LinguisticVariantDisplayName\r\n\
2160-0,Creatinina,MCnc,Punto temporal,Suero o Plasma,Cuant,,,,,,\r\n\
718-7,Hemoglobina,MCnc,Punto temporal,Sangre,Cuant,,,,,,\r\n";

    const LV_FR_CSV: &str = "\
LOINC_NUM,COMPONENT,LONG_COMMON_NAME,SHORTNAME\r\n\
2160-0,Créatinine,Créatinine [Masse/volume] dans le sérum ou le plasma,Créat SerPl\r\n\
718-7,Hémoglobine,Hémoglobine [Masse/volume] dans le sang,Hb Sang\r\n";

    const LV_DE_CSV: &str = "\
LOINC_NUM,COMPONENT,LONG_COMMON_NAME,SHORTNAME\r\n\
2160-0,Kreatinin,Kreatinin [Masse/Volumen] in Serum oder Plasma,Krea SerPl\r\n";

    fn make_test_loinc_zip_with_variants() -> NamedTempFile {
        let tmp = NamedTempFile::with_suffix(".zip").unwrap();
        {
            let mut zip = zip::ZipWriter::new(tmp.reopen().unwrap());
            let opts = zip::write::FileOptions::default();

            zip.start_file("LoincTable/Loinc.csv", opts).unwrap();
            zip.write_all(LOINC_TABLE_CSV.as_bytes()).unwrap();

            zip.start_file(
                "AccessoryFiles/MultiAxialHierarchy/MultiAxialHierarchy.csv",
                opts,
            )
            .unwrap();
            zip.write_all(HIERARCHY_CSV.as_bytes()).unwrap();

            zip.start_file(
                "AccessoryFiles/LinguisticVariants/LinguisticVariants.csv",
                opts,
            )
            .unwrap();
            zip.write_all(LV_INDEX_CSV.as_bytes()).unwrap();

            zip.start_file(
                "AccessoryFiles/LinguisticVariants/frFR28LinguisticVariant.csv",
                opts,
            )
            .unwrap();
            zip.write_all(LV_FR_CSV.as_bytes()).unwrap();

            zip.start_file(
                "AccessoryFiles/LinguisticVariants/deDE8LinguisticVariant.csv",
                opts,
            )
            .unwrap();
            zip.write_all(LV_DE_CSV.as_bytes()).unwrap();

            zip.finish().unwrap();
        }
        tmp
    }

    /// Build a ZIP carrying the regional variant set from the user-reported
    /// case: de-AT, de-DE, es-AR, es-ES, es-MX. The numeric ids (15/91/92/93)
    /// are absent from `LV_INDEX_CSV` so `derive_language_tag` resolves those
    /// tags from the filename letters (id 8 maps to de-DE via the index).
    fn make_test_loinc_zip_with_regional_variants() -> NamedTempFile {
        let tmp = NamedTempFile::with_suffix(".zip").unwrap();
        {
            let mut zip = zip::ZipWriter::new(tmp.reopen().unwrap());
            let opts = zip::write::FileOptions::default();

            zip.start_file("LoincTable/Loinc.csv", opts).unwrap();
            zip.write_all(LOINC_TABLE_CSV.as_bytes()).unwrap();
            zip.start_file(
                "AccessoryFiles/MultiAxialHierarchy/MultiAxialHierarchy.csv",
                opts,
            )
            .unwrap();
            zip.write_all(HIERARCHY_CSV.as_bytes()).unwrap();
            zip.start_file(
                "AccessoryFiles/LinguisticVariants/LinguisticVariants.csv",
                opts,
            )
            .unwrap();
            zip.write_all(LV_INDEX_CSV.as_bytes()).unwrap();

            // German variants (LONG_COMMON_NAME-style, 1 row each): de-AT, de-DE.
            for fname in ["deAT15LinguisticVariant.csv", "deDE8LinguisticVariant.csv"] {
                zip.start_file(format!("AccessoryFiles/LinguisticVariants/{fname}"), opts)
                    .unwrap();
                zip.write_all(LV_DE_CSV.as_bytes()).unwrap();
            }
            // Spanish variants (axes-synthesized, 2 rows each): es-AR, es-ES, es-MX.
            for fname in [
                "esAR92LinguisticVariant.csv",
                "esES91LinguisticVariant.csv",
                "esMX93LinguisticVariant.csv",
            ] {
                zip.start_file(format!("AccessoryFiles/LinguisticVariants/{fname}"), opts)
                    .unwrap();
                zip.write_all(LV_ES_AXES_CSV.as_bytes()).unwrap();
            }

            zip.finish().unwrap();
        }
        tmp
    }

    fn count_rows(backend: &SqliteTerminologyBackend, table: &str) -> i64 {
        let conn = backend.pool().get().unwrap();
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap_or(0)
    }

    /// Translated designations only (the `en-US` alternate English names —
    /// SHORTNAME etc. — are counted separately).
    fn count_translations(backend: &SqliteTerminologyBackend) -> i64 {
        let conn = backend.pool().get().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM concept_designations WHERE language IS NOT 'en-US'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0)
    }

    fn count_english_alternates(backend: &SqliteTerminologyBackend) -> i64 {
        count_rows(backend, "concept_designations") - count_translations(backend)
    }

    // ── Parser unit tests ─────────────────────────────────────────────────────

    #[test]
    fn parse_loinc_table_returns_all_statuses() {
        let mut errors = Vec::new();
        let concepts = parse_loinc_table(LOINC_TABLE_CSV.as_bytes(), &mut errors).unwrap();
        assert_eq!(concepts.len(), 3, "all three rows should be parsed");
        assert_eq!(
            concepts["2160-0"].display,
            "Creatinine [Mass/volume] in Serum or Plasma"
        );
        assert_eq!(concepts["2160-0"].status, "ACTIVE");
        assert_eq!(concepts["99999-9"].status, "DEPRECATED");
        assert!(errors.is_empty());
    }

    #[test]
    fn parse_loinc_table_falls_back_to_short_name() {
        let csv = "LOINC_NUM,LONG_COMMON_NAME,ShortName,STATUS\r\n\
                   1234-5,,Short only,ACTIVE\r\n";
        let mut errors = Vec::new();
        let concepts = parse_loinc_table(csv.as_bytes(), &mut errors).unwrap();
        assert_eq!(concepts["1234-5"].display, "Short only");
    }

    /// SHORTNAME is an alternate English display (tx.fhir.org accepts
    /// `Calcium SerPl-mCnc` for 17861-6), DisplayName/CONSUMER_NAME are not;
    /// the real Loinc.csv spells the short name SHORTNAME.
    #[test]
    fn parse_loinc_table_keeps_alternate_english_names() {
        let csv = "\"LOINC_NUM\",\"CONSUMER_NAME\",\"SHORTNAME\",\"LONG_COMMON_NAME\",\"STATUS\",\"DisplayName\"\r\n\
                   \"17861-6\",\"\",\"Calcium SerPl-mCnc\",\"Calcium [Mass/volume] in Serum or Plasma\",\"ACTIVE\",\"Calcium [Mass/Vol]\"\r\n\
                   \"1-8\",\"Same\",\"Same\",\"Same\",\"ACTIVE\",\"\"\r\n";
        let mut errors = Vec::new();
        let concepts = parse_loinc_table(csv.as_bytes(), &mut errors).unwrap();
        let calcium = &concepts["17861-6"];
        assert_eq!(calcium.display, "Calcium [Mass/volume] in Serum or Plasma");
        assert_eq!(
            calcium.alternates,
            vec!["Calcium SerPl-mCnc".to_string()]
        );
        assert!(concepts["1-8"].alternates.is_empty(), "a short name equal to the display is dropped");
        let designations = english_designations(&calcium.alternates);
        assert!(designations.iter().all(|d| d.language == "en-US"));
    }

    #[test]
    fn parse_parts_returns_the_part_name() {
        let csv = "\"PartNumber\",\"PartTypeName\",\"PartName\",\"PartDisplayName\",\"Status\"\r\n\
                   \"LP7057-5\",\"SYSTEM\",\"Bld\",\"Blood\",\"ACTIVE\"\r\n";
        let mut errors = Vec::new();
        let parts = parse_parts(csv.as_bytes(), &mut errors).unwrap();
        assert_eq!(
            parts,
            vec![PartRow {
                code: "LP7057-5".to_string(),
                display: "Blood".to_string(),
                name: "Bld".to_string(),
                status: "ACTIVE".to_string(),
            }]
        );
    }

    #[test]
    fn parse_hierarchy_returns_lp_codes_and_edges() {
        let mut errors = Vec::new();
        let (concepts, edges) = parse_hierarchy(HIERARCHY_CSV.as_bytes(), &mut errors).unwrap();
        assert_eq!(concepts.len(), 5);
        assert_eq!(concepts["LP7786-3"], "Laboratory");
        assert_eq!(edges.len(), 4);
    }

    #[test]
    fn parse_loinc_empty_code_recorded_in_errors() {
        let csv = "LOINC_NUM,LONG_COMMON_NAME,ShortName,STATUS\r\n\
                   2160-0,Creatinine,Creat,ACTIVE\r\n\
                   ,Missing code,Bad,ACTIVE\r\n";
        let mut errors = Vec::new();
        let concepts = parse_loinc_table(csv.as_bytes(), &mut errors).unwrap();
        assert_eq!(concepts.len(), 1);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("LOINC_NUM is empty"));
    }

    #[tokio::test]
    async fn deprecated_status_stored_in_definition() {
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        let zip_file = make_test_loinc_zip();

        import_loinc_csv(
            &backend,
            &ctx,
            zip_file.path(),
            500,
            false,
            &LanguageFilter::default(),
        )
        .await
        .unwrap();

        let conn = backend.pool().get().unwrap();
        let definition: Option<String> = conn
            .query_row(
                "SELECT definition FROM concepts WHERE code = '99999-9'",
                [],
                |row| row.get(0),
            )
            .ok();
        assert_eq!(definition.as_deref(), Some("STATUS:DEPRECATED"));
    }

    #[tokio::test]
    async fn import_loinc_csv_dry_run_does_not_write_to_db() {
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        let zip_file = make_test_loinc_zip();

        let stats = import_loinc_csv(
            &backend,
            &ctx,
            zip_file.path(),
            500,
            true,
            &LanguageFilter::default(),
        )
        .await
        .expect("dry-run should succeed");

        assert_eq!(stats.code_systems, 1);
        assert!(stats.concepts > 0);

        assert_eq!(count_rows(&backend, "code_systems"), 0);
        assert_eq!(count_rows(&backend, "concepts"), 0);
    }

    #[tokio::test]
    async fn import_loinc_csv_live_writes_concepts_and_hierarchy() {
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        let zip_file = make_test_loinc_zip();

        let stats = import_loinc_csv(
            &backend,
            &ctx,
            zip_file.path(),
            500,
            false,
            &LanguageFilter::default(),
        )
        .await
        .expect("live import should succeed");

        assert_eq!(stats.code_systems, 1);
        // 3 LOINC codes + 3 LP codes = 6 total concepts
        assert_eq!(stats.concepts, 6);

        assert_eq!(count_rows(&backend, "code_systems"), 1);
        assert_eq!(count_rows(&backend, "concepts"), 6);
        assert_eq!(count_rows(&backend, "concept_hierarchy"), 4);
    }

    // Accessory files: one part already in the hierarchy (LP29693-6), one
    // that is not (LP999-1, deprecated); answer list LL1-1 with two answers
    // linked to 2160-0; LL2-2 is externally defined (no answers).
    const PART_CSV: &str = "\
\"PartNumber\",\"PartTypeName\",\"PartName\",\"PartDisplayName\",\"Status\"\r\n\
\"LP29693-6\",\"CLASS\",\"Chem\",\"Chemistry (part file)\",\"ACTIVE\"\r\n\
\"LP999-1\",\"COMPONENT\",\"Oldthing\",\"\",\"DEPRECATED\"\r\n";
    const ANSWER_LIST_CSV: &str = "\
\"AnswerListId\",\"AnswerListName\",\"AnswerListOID\",\"ExtDefinedYN\",\"ExtDefinedAnswerListCodeSystem\",\"ExtDefinedAnswerListLink\",\"AnswerStringId\",\"LocalAnswerCode\",\"LocalAnswerCodeSystem\",\"SequenceNumber\",\"DisplayText\"\r\n\
\"LL1-1\",\"Yes/No\",\"1.2.3\",\"N\",\"\",\"\",\"LA33-6\",\"\",\"\",\"1\",\"Yes\"\r\n\
\"LL1-1\",\"Yes/No\",\"1.2.3\",\"N\",\"\",\"\",\"LA32-8\",\"\",\"\",\"2\",\"No\"\r\n\
\"LL2-2\",\"External list\",\"\",\"Y\",\"http://example.org\",\"\",\"\",\"\",\"\",\"\",\"\"\r\n";
    const ANSWER_LINK_CSV: &str = "\
\"LoincNumber\",\"LongCommonName\",\"AnswerListId\",\"AnswerListName\",\"AnswerListLinkType\",\"ApplicableContext\"\r\n\
\"2160-0\",\"Creatinine\",\"LL1-1\",\"Yes/No\",\"NORMATIVE\",\"\"\r\n";

    fn make_loinc_zip_with_accessory_files() -> NamedTempFile {
        let tmp = make_test_loinc_zip();
        {
            let mut zip = zip::ZipWriter::new_append(tmp.reopen().unwrap()).unwrap();
            let opts = zip::write::FileOptions::default();
            for (name, body) in [
                ("AccessoryFiles/PartFile/Part.csv", PART_CSV),
                // The PanelsAndForms copy must lose to AnswerFile/.
                (
                    "AccessoryFiles/PanelsAndForms/AnswerList.csv",
                    "AnswerListId\r\n",
                ),
                ("AccessoryFiles/AnswerFile/AnswerList.csv", ANSWER_LIST_CSV),
                (
                    "AccessoryFiles/AnswerFile/LoincAnswerListLink.csv",
                    ANSWER_LINK_CSV,
                ),
            ] {
                zip.start_file(name, opts).unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            }
            zip.finish().unwrap();
        }
        tmp
    }

    /// Parts outside the hierarchy, answer lists and answers become concepts;
    /// an answer list with answers becomes `http://loinc.org/vs/<LL…>`; a
    /// linked term carries `answer-list`.
    #[tokio::test]
    async fn import_loinc_csv_imports_parts_and_answer_lists() {
        use crate::traits::{CodeSystemOperations, ValueSetOperations};
        use crate::types::{ExpandRequest, LookupRequest, ValidateCodeRequest};
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        let zip_file = make_loinc_zip_with_accessory_files();

        let dry = import_loinc_csv(
            &backend,
            &ctx,
            zip_file.path(),
            500,
            true,
            &LanguageFilter::default(),
        )
        .await
        .unwrap();
        // 6 + LP999-1 + LL1-1 + LL2-2 + LA33-6 + LA32-8.
        assert_eq!((dry.concepts, dry.value_sets), (11, 1));

        let stats = import_loinc_csv(
            &backend,
            &ctx,
            zip_file.path(),
            500,
            false,
            &LanguageFilter::default(),
        )
        .await
        .expect("import");
        assert_eq!((stats.concepts, stats.value_sets), (11, 1));
        assert_eq!(count_rows(&backend, "concepts"), 11);

        let lookup = |code: &str| LookupRequest {
            system: "http://loinc.org".into(),
            code: code.into(),
            ..Default::default()
        };
        let part = backend.lookup(&ctx, lookup("LP999-1")).await.unwrap();
        assert_eq!(part.display.as_deref(), Some("Oldthing"));
        // The hierarchy's display stays for a part it already has.
        let chem = backend.lookup(&ctx, lookup("LP29693-6")).await.unwrap();
        assert_eq!(chem.display.as_deref(), Some("Chemistry"));
        let term = backend.lookup(&ctx, lookup("2160-0")).await.unwrap();
        assert!(
            term.properties
                .iter()
                .any(|p| p.code == "answer-list" && p.value == "LL1-1"),
            "{:?}",
            term.properties
        );

        let vs = backend
            .expand(
                &ctx,
                ExpandRequest {
                    url: Some("http://loinc.org/vs/LL1-1".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("answer list ValueSet");
        let mut answers: Vec<&str> = vs.contains.iter().map(|c| c.code.as_str()).collect();
        answers.sort_unstable();
        assert_eq!(answers, ["LA32-8", "LA33-6"]);
        let valid = ValueSetOperations::validate_code(
            &backend,
            &ctx,
            ValidateCodeRequest {
                url: Some("http://loinc.org/vs/LL1-1".into()),
                system: Some("http://loinc.org".into()),
                code: "LA32-8".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(valid.result);
        assert!(
            backend
                .expand(
                    &ctx,
                    ExpandRequest {
                        url: Some("http://loinc.org/vs/LL2-2".into()),
                        ..Default::default()
                    },
                )
                .await
                .is_err(),
            "an externally defined list has no ValueSet"
        );
    }

    #[tokio::test]
    async fn import_loinc_csv_idempotent_reimport() {
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        let zip_file = make_test_loinc_zip();

        import_loinc_csv(
            &backend,
            &ctx,
            zip_file.path(),
            500,
            false,
            &LanguageFilter::default(),
        )
        .await
        .unwrap();
        import_loinc_csv(
            &backend,
            &ctx,
            zip_file.path(),
            500,
            false,
            &LanguageFilter::default(),
        )
        .await
        .unwrap();

        assert_eq!(count_rows(&backend, "code_systems"), 1);
        assert_eq!(count_rows(&backend, "concepts"), 6);
        assert_eq!(count_rows(&backend, "concept_hierarchy"), 4);
    }

    #[tokio::test]
    async fn import_loinc_csv_batching_preserves_all_concepts() {
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        let zip_file = make_test_loinc_zip();

        let stats = import_loinc_csv(
            &backend,
            &ctx,
            zip_file.path(),
            2,
            false,
            &LanguageFilter::default(),
        )
        .await
        .unwrap();
        assert_eq!(stats.concepts, 6);
        assert_eq!(count_rows(&backend, "concepts"), 6);
    }

    #[tokio::test]
    async fn import_loinc_csv_missing_file_returns_error() {
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();

        let result = import_loinc_csv(
            &backend,
            &ctx,
            Path::new("/nonexistent/loinc.zip"),
            500,
            false,
            &LanguageFilter::default(),
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn import_loinc_ignores_accessory_loinc_files() {
        // Reproduces tx-ecosystem subset layout: real table at
        // `LoincTable/Loinc.csv`, with `AccessoryFiles/PartFile/LoincPartLink_Primary.csv`
        // alongside. The accessory file's name starts with "loinc" but isn't
        // the main table — picking it would fail with "LOINC_NUM not found".
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();

        let tmp = NamedTempFile::with_suffix(".zip").unwrap();
        {
            let mut zip = zip::ZipWriter::new(tmp.reopen().unwrap());
            let opts = zip::write::FileOptions::default();

            zip.start_file("AccessoryFiles/PartFile/LoincPartLink_Primary.csv", opts)
                .unwrap();
            zip.write_all(b"LinkTypeName,LoincNumber,PartNumber\r\n")
                .unwrap();

            zip.start_file("LoincTable/Loinc.csv", opts).unwrap();
            zip.write_all(LOINC_TABLE_CSV.as_bytes()).unwrap();

            zip.start_file(
                "AccessoryFiles/ComponentHierarchyBySystem/ComponentHierarchyBySystem.csv",
                opts,
            )
            .unwrap();
            zip.write_all(HIERARCHY_CSV.as_bytes()).unwrap();

            zip.finish().unwrap();
        }

        let stats = import_loinc_csv(
            &backend,
            &ctx,
            tmp.path(),
            500,
            false,
            &LanguageFilter::default(),
        )
        .await
        .expect("should pick LoincTable/Loinc.csv, not the accessory file");
        assert_eq!(stats.concepts, 6);
        assert_eq!(count_rows(&backend, "concepts"), 6);
    }

    // ── Linguistic-variant tests ──────────────────────────────────────────────

    #[test]
    fn parse_linguistic_index_maps_ids_to_lang_country() {
        let mut errors = Vec::new();
        let index = parse_linguistic_index(LV_INDEX_CSV.as_bytes(), &mut errors);
        assert_eq!(index["28"], ("fr".to_string(), "FR".to_string()));
        assert_eq!(index["8"], ("de".to_string(), "DE".to_string()));
        assert!(errors.is_empty());
    }

    #[test]
    fn derive_language_tag_prefers_index() {
        let mut errors = Vec::new();
        let index = parse_linguistic_index(LV_INDEX_CSV.as_bytes(), &mut errors);
        assert_eq!(
            derive_language_tag("frFR28LinguisticVariant.csv", &index).as_deref(),
            Some("fr-FR")
        );
        assert_eq!(
            derive_language_tag("deDE8LinguisticVariant.csv", &index).as_deref(),
            Some("de-DE")
        );
    }

    #[test]
    fn derive_language_tag_falls_back_to_filename() {
        let empty = HashMap::new();
        // No index → derive `zh-CN` from the leading letters.
        assert_eq!(
            derive_language_tag("zhCN5LinguisticVariant.csv", &empty).as_deref(),
            Some("zh-CN")
        );
        // Language only, no country letters.
        assert_eq!(
            derive_language_tag("fr99LinguisticVariant.csv", &empty).as_deref(),
            Some("fr")
        );
        // Not a variant file.
        assert_eq!(derive_language_tag("LoincTable.csv", &empty), None);
    }

    #[test]
    fn parse_linguistic_variant_extracts_translations() {
        let mut errors = Vec::new();
        let rows = parse_linguistic_variant(
            LV_FR_CSV.as_bytes(),
            "frFR28LinguisticVariant.csv",
            &mut errors,
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "2160-0");
        assert_eq!(
            rows[0].1,
            "Créatinine [Masse/volume] dans le sérum ou le plasma"
        );
        assert!(errors.is_empty());
    }

    #[test]
    fn parse_linguistic_variant_synthesizes_display_from_axes() {
        // es-ES / it-IT only populate the axis columns; the display must be the
        // fully-specified name built from the populated axes (regression: these
        // variants used to import zero designations).
        let mut errors = Vec::new();
        let rows = parse_linguistic_variant(
            LV_ES_AXES_CSV.as_bytes(),
            "esES12LinguisticVariant.csv",
            &mut errors,
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "2160-0");
        // Trailing empty axis (METHOD_TYP) is dropped — no dangling separator.
        assert_eq!(
            rows[0].1,
            "Creatinina:MCnc:Punto temporal:Suero o Plasma:Cuant"
        );
        assert!(errors.is_empty());
    }

    #[test]
    fn parse_linguistic_index_accepts_real_loinc_columns() {
        // Real LOINC releases use ID/ISO_LANGUAGE/ISO_COUNTRY rather than the
        // documented camelCase spellings — both must parse without error.
        let mut errors = Vec::new();
        let index = parse_linguistic_index(LV_INDEX_CSV.as_bytes(), &mut errors);
        assert_eq!(index["28"], ("fr".to_string(), "FR".to_string()));
        assert!(errors.is_empty());
    }

    #[test]
    fn find_loinc_paths_locates_linguistic_variants() {
        let zip_file = make_test_loinc_zip_with_variants();
        let paths = find_loinc_paths(zip_file.path()).unwrap();
        assert!(paths.lv_index.is_some());
        assert_eq!(paths.lv_variants.len(), 2);
        // The index (`LinguisticVariants.csv`, plural) must not be collected as
        // a per-variant file.
        assert!(
            paths
                .lv_variants
                .iter()
                .all(|p| !p.to_lowercase().ends_with("linguisticvariants.csv"))
        );
    }

    #[tokio::test]
    async fn import_loinc_csv_writes_designations() {
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        let zip_file = make_test_loinc_zip_with_variants();

        import_loinc_csv(
            &backend,
            &ctx,
            zip_file.path(),
            500,
            false,
            &LanguageFilter::default(),
        )
        .await
        .expect("import with linguistic variants should succeed");

        // 2 French + 1 German translation = 3 designation rows.
        assert_eq!(count_translations(&backend), 3);

        let conn = backend.pool().get().unwrap();
        let (lang, value): (String, String) = conn
            .query_row(
                "SELECT d.language, d.value FROM concept_designations d \
                 JOIN concepts c ON c.id = d.concept_id \
                 WHERE c.code = '2160-0' AND d.language = 'de-DE'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("German designation for 2160-0 should exist");
        assert_eq!(lang, "de-DE");
        assert_eq!(value, "Kreatinin [Masse/Volumen] in Serum oder Plasma");

        // The English display is left untouched.
        let display: String = conn
            .query_row(
                "SELECT display FROM concepts WHERE code = '2160-0'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(display, "Creatinine [Mass/volume] in Serum or Plasma");
    }

    #[tokio::test]
    async fn import_loinc_csv_language_filter_skips_excluded_variants() {
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        let zip_file = make_test_loinc_zip_with_variants();

        import_loinc_csv(
            &backend,
            &ctx,
            zip_file.path(),
            500,
            false,
            &LanguageFilter::parse("fr"),
        )
        .await
        .expect("import with language filter should succeed");

        // Only the 2 French translations survive; the German variant file is
        // skipped without being parsed.
        assert_eq!(count_translations(&backend), 2);

        let conn = backend.pool().get().unwrap();
        let de_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM concept_designations WHERE language = 'de-DE'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(de_count, 0);

        // The English display is unaffected by the filter.
        let display: String = conn
            .query_row(
                "SELECT display FROM concepts WHERE code = '2160-0'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(display, "Creatinine [Mass/volume] in Serum or Plasma");
    }

    #[tokio::test]
    async fn import_loinc_best_tier_excludes_regional_siblings() {
        // The user-reported case: filter `de,es-ES` against de-AT, de-DE,
        // es-AR, es-ES, es-MX. `es-ES` is an exact match, so es-AR/es-MX are
        // dropped; `de` only reaches the regional-sibling tier, so both German
        // variants are kept.
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        let zip_file = make_test_loinc_zip_with_regional_variants();

        import_loinc_csv(
            &backend,
            &ctx,
            zip_file.path(),
            500,
            false,
            &LanguageFilter::parse("de,es-ES"),
        )
        .await
        .expect("import with language filter should succeed");

        let conn = backend.pool().get().unwrap();
        let count = |lang: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM concept_designations WHERE language = ?1",
                [lang],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(count("de-AT"), 1, "de-AT kept (de* sibling tier)");
        assert_eq!(count("de-DE"), 1, "de-DE kept (de* sibling tier)");
        assert_eq!(count("es-ES"), 2, "es-ES kept (exact match)");
        assert_eq!(count("es-AR"), 0, "es-AR dropped (es-ES exact outranks)");
        assert_eq!(count("es-MX"), 0, "es-MX dropped (es-ES exact outranks)");
        // 2 German + 2 Spanish designations, nothing else.
        assert_eq!(count_translations(&backend), 4);
    }

    #[tokio::test]
    async fn loinc_fresh_load_skip_then_narrowed_reimport_replaces() {
        // First import is a fresh load (system empty → fresh_load path skips the
        // delete-before-reinsert). Both French and German designations land.
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        let zip_file = make_test_loinc_zip_with_variants();
        import_loinc_csv(
            &backend,
            &ctx,
            zip_file.path(),
            500,
            false,
            &LanguageFilter::default(),
        )
        .await
        .unwrap();
        assert_eq!(count_translations(&backend), 3); // fr×2 + de×1

        // Re-import with a narrower filter. The system now has concepts, so the
        // probe must flip fresh_load OFF and the per-concept replacement must
        // run — dropping the German designation. This is the interaction that
        // would silently leave stale rows if the skip path were taken on a
        // re-import.
        import_loinc_csv(
            &backend,
            &ctx,
            zip_file.path(),
            500,
            false,
            &LanguageFilter::parse("fr"),
        )
        .await
        .unwrap();
        let conn = backend.pool().get().unwrap();
        let de: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM concept_designations WHERE language = 'de-DE'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            de, 0,
            "stale German designations must be removed on re-import"
        );
        assert_eq!(count_translations(&backend), 2); // fr×2 only
    }

    #[tokio::test]
    async fn import_loinc_csv_dry_run_counts_designations() {
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        let zip_file = make_test_loinc_zip_with_variants();

        import_loinc_csv(
            &backend,
            &ctx,
            zip_file.path(),
            500,
            true,
            &LanguageFilter::default(),
        )
        .await
        .expect("dry-run should succeed");

        // Dry-run writes nothing.
        assert_eq!(count_translations(&backend), 0);
    }

    #[tokio::test]
    async fn import_loinc_csv_without_variants_still_works() {
        // The original (no-variant) ZIP must import cleanly with zero designations.
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        let zip_file = make_test_loinc_zip();

        import_loinc_csv(
            &backend,
            &ctx,
            zip_file.path(),
            500,
            false,
            &LanguageFilter::default(),
        )
        .await
        .unwrap();
        assert_eq!(count_translations(&backend), 0);
        // The three short names of the fixture are alternate displays.
        assert_eq!(count_english_alternates(&backend), 3);
    }

    /// The backfill's designation dedupe is per language: a translation that
    /// happens to spell the short name does not stop the `en-US` designation,
    /// and a second write adds nothing.
    #[tokio::test]
    async fn designation_backfill_dedupes_per_language() {
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();
        let zip_file = make_test_loinc_zip();
        import_loinc_csv(&backend, &ctx, zip_file.path(), 500, false, &LanguageFilter::default())
            .await
            .unwrap();
        {
            let conn = backend.pool().get().unwrap();
            conn.execute("DELETE FROM concept_designations", []).unwrap();
            conn.execute(
                "INSERT INTO concept_designations (concept_id, language, value)
                 SELECT id, 'fr-FR', 'HGB BLD-MCNC' FROM concepts WHERE code = '718-7'",
                [],
            )
            .unwrap();
        }
        let rows = || {
            vec![
                ("718-7".to_string(), "en-US".to_string(), "Hgb Bld-mCnc".to_string()),
                ("718-7".to_string(), "en-US".to_string(), "hgb bld-mcnc".to_string()),
                // Equal to the display, ignoring case: not a designation.
                (
                    "718-7".to_string(),
                    "en-US".to_string(),
                    "hemoglobin [mass/volume] in blood".to_string(),
                ),
            ]
        };
        let (_, first) = backend
            .write_concept_properties(&ctx, LOINC_URL, None, &[], vec![], rows())
            .await
            .unwrap();
        assert_eq!(first, 1, "one en-US short name beside the fr-FR row");
        let (_, second) = backend
            .write_concept_properties(&ctx, LOINC_URL, None, &[], vec![], rows())
            .await
            .unwrap();
        assert_eq!(second, 0, "a rerun adds nothing");
        assert_eq!(count_english_alternates(&backend), 1);
        assert_eq!(count_translations(&backend), 1);
    }

    #[test]
    fn loinc_release_version_from_zip_file_name() {
        assert_eq!(
            super::loinc_release_version_from_entry("Loinc_2.80.zip").as_deref(),
            Some("2.80")
        );
        assert_eq!(super::loinc_release_version_from_entry("Loinc_.zip"), None);
    }

    #[test]
    fn find_loinc_paths_prefers_loinc_table_dir_and_zip_name_version() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("Loinc_2.80.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::FileOptions::default();
            // Sorted before LoincTable/ exactly like the official archive.
            zip.start_file("AccessoryFiles/PanelsAndForms/Loinc.csv", opts)
                .unwrap();
            zip.write_all(b"\"LOINC_NUM\"\n").unwrap();
            zip.start_file(
                "AccessoryFiles/ComponentHierarchyBySystem/ComponentHierarchyBySystem.csv",
                opts,
            )
            .unwrap();
            zip.write_all(b"PATH_TO_ROOT,SEQUENCE,IMMEDIATE_PARENT,CODE,CODE_TEXT\n")
                .unwrap();
            zip.start_file("LoincTable/Loinc.csv", opts).unwrap();
            zip.write_all(b"\"LOINC_NUM\"\n").unwrap();
            zip.finish().unwrap();
        }
        let paths = super::find_loinc_paths(&zip_path).unwrap();
        assert_eq!(paths.loinc_table, "LoincTable/Loinc.csv");
        assert_eq!(paths.release_version.as_deref(), Some("2.80"));
    }

    #[test]
    fn loinc_release_version_from_entry_parses_top_level_folder() {
        assert_eq!(
            super::loinc_release_version_from_entry("Loinc_2.82/LoincTable/Loinc.csv").as_deref(),
            Some("2.82")
        );
        assert_eq!(
            super::loinc_release_version_from_entry("LoincTable/Loinc.csv"),
            None
        );
    }

    #[tokio::test]
    async fn import_loinc_nested_zip_layout() {
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        let ctx = TenantContext::system();

        let tmp = NamedTempFile::with_suffix(".zip").unwrap();
        {
            let mut zip = zip::ZipWriter::new(tmp.reopen().unwrap());
            let opts = zip::write::FileOptions::default();

            zip.start_file("Loinc_2.77/LoincTable.csv", opts).unwrap();
            zip.write_all(LOINC_TABLE_CSV.as_bytes()).unwrap();

            zip.start_file(
                "Loinc_2.77/AccessoryFiles/MultiAxialHierarchy/MultiAxialHierarchy.csv",
                opts,
            )
            .unwrap();
            zip.write_all(HIERARCHY_CSV.as_bytes()).unwrap();

            zip.finish().unwrap();
        }

        let stats = import_loinc_csv(
            &backend,
            &ctx,
            tmp.path(),
            500,
            false,
            &LanguageFilter::default(),
        )
        .await
        .unwrap();

        assert!(stats.concepts > 0, "got 0 concepts");
        assert!(
            stats.errors.is_empty(),
            "unexpected errors: {:?}",
            stats.errors
        );

        let conn = backend.pool().get().unwrap();
        let version: Option<String> = conn
            .query_row(
                "SELECT version FROM code_systems WHERE url = 'http://loinc.org'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version.as_deref(), Some("2.77"));
    }
}
