//! Backend-agnostic FHIR Bundle parser.
//!
//! [`parse_bundle`] deserialises a raw FHIR Bundle JSON payload into a
//! [`ParsedBundle`] intermediate representation.  Both the SQLite and
//! PostgreSQL write layers consume this representation, so all JSON-walking
//! logic lives here exactly once.
//!
//! ## What this module does NOT do
//!
//! It does not touch any database.  All DB writes happen in the backend-specific
//! modules (`import/fhir_bundle.rs` for SQLite,
//! `backend/postgres/mod.rs` for PostgreSQL).

use serde_json::Value;
use uuid::Uuid;

use crate::error::HtsError;

// ── Public intermediate types ─────────────────────────────────────────────────

/// A FHIR Bundle parsed into backend-agnostic Rust structs.
///
/// Resources are partitioned in import-dependency order:
/// CodeSystems first, then ValueSets, then ConceptMaps.
///
/// `parse_errors` accumulates non-fatal parse failures (e.g. a CodeSystem
/// missing its `url`).  Write layers should propagate these into
/// [`super::ImportStats::errors`] so callers can return HTTP 207 Multi-Status.
#[derive(Debug, Default)]
pub struct ParsedBundle {
    pub code_systems: Vec<ParsedCodeSystem>,
    pub value_sets: Vec<ParsedValueSet>,
    pub concept_maps: Vec<ParsedConceptMap>,
    /// Non-fatal parse errors.  Resources with errors are skipped; the rest
    /// of the bundle continues to be imported.
    pub parse_errors: Vec<String>,
    /// Optimization hint: when `true`, every code system in this bundle is
    /// known to have had **no concepts** in the database before this import
    /// session began, so the per-concept delete-before-reinsert of
    /// properties/designations is pointless and can be skipped. Set by the
    /// chunked filesystem importers (SNOMED RF2, LOINC) after a single
    /// pre-import probe; it stays `true` across every batch of a fresh load.
    /// Defaults to `false` (always replace), which is always correct — the flag
    /// is purely a fast-path for first-time bulk loads. Importers MUST leave it
    /// `false` whenever the target may already contain concepts (re-import).
    pub fresh_load: bool,
    /// `true` when this bundle is one concept batch of a chunked bulk load
    /// (SNOMED RF2, LOINC). The importer's caller builds the closure once
    /// when the whole load is in (`hts import`, the admin import job, the
    /// bootstrap), so the write layer does not rebuild it after every batch —
    /// which, for a hierarchy below the inline-rebuild limit, meant one full
    /// closure build per batch.
    pub chunked: bool,
    /// When `true`, the write layer does not rebuild `concept_closure` for code
    /// systems that already held concepts before this bundle. Rebuilding after
    /// every batch of a chunked bulk load (SNOMED RF2, LOINC, RxNorm, …) is
    /// O(n²), so those importers set this on each batch and leave the single
    /// rebuild to `finalize_after_bootstrap` / `migrate_concept_closure*`.
    /// Systems that were empty before the bundle are still rebuilt (cheap).
    ///
    /// Defaults to `false`: every code system the bundle writes gets its
    /// closure rebuilt before the import returns, so a plain re-import never
    /// leaves `$subsumes` / `is-a` answering from a wiped closure (#2128).
    pub defer_closure: bool,
}

/// Authority rank for a resource we have POSITIVE evidence is authoritative:
/// the package that shipped it declares a `canonical` base that covers the
/// resource's URL, or it came from a native importer loading a publisher's own
/// distribution (SNOMED RF2, LOINC, RxNorm, …). Lower wins.
pub const AUTHORITY_OWNER: i32 = 0;

/// Authority rank for a resource whose provenance we cannot vouch for.
///
/// This is the DEFAULT, and the distinction from [`AUTHORITY_OWNER`] is the
/// whole point: we only ever *promote* on positive evidence, never *demote* on
/// the absence of it. A row is `UNKNOWN` when it arrived outside any package
/// (REST write, `$import-bundle`, CLI bundle) or when the package that shipped
/// it does not declare a canonical base covering the URL.
///
/// That second case deliberately conflates two situations we cannot tell apart:
///
/// * `hl7.fhir.r4.core` re-publishing `http://terminology.hl7.org/...` — a
///   genuine stale copy, which must lose to THO's original. This is issue #200.
/// * `us.nlm.vsac` shipping `http://cts.nlm.nih.gov/...` while declaring the
///   fhir.org *registry* base `http://fhir.org/packages/us.nlm.vsac` — a genuine
///   ORIGINAL whose manifest simply doesn't describe its content's authority.
///   14,850 of its ValueSets look identical to a "copy" under this rule.
///
/// Conflating them is safe precisely because rank is only ever a TIEBREAK among
/// rows sharing one canonical URL. Being `UNKNOWN` costs a row nothing unless a
/// demonstrable owner of the same URL exists — VSAC's rows have no rival, so they
/// still win their URLs outright. An earlier draft ranked these as a definite
/// "foreign copy" (2) and treated non-package writes as owners (0); that was
/// wrong twice over. It asserted a fact about VSAC we had not established, and —
/// because `vs_precedence_order_by` has no content/has-concepts tier to absorb
/// the error — it let any REST upload outrank the real VSAC definition outright.
pub const AUTHORITY_UNKNOWN: i32 = 1;

/// Sentinel meaning "no source has claimed this row yet", used by the import
/// upsert's `MIN(COALESCE(authority_rank, …), ?)` / `LEAST(...)` so that a row
/// which predates the column (NULL) is overwritten by the first source to claim
/// it, rather than reading as an authoritative 0 and pinning a stale copy.
///
/// It must stay strictly greater than every real rank. Note the deliberate
/// asymmetry: on WRITE, NULL is the *weakest* value (this sentinel); on READ it
/// coalesces to 0, the *strongest*, so that a database which has not yet
/// re-imported behaves exactly as it did before the column existed.
pub const AUTHORITY_UNCLAIMED: i32 = 9;

/// Classify a resource against the canonical base declared by the package that
/// shipped it (`package.json` → `canonical`).
///
/// Returns [`AUTHORITY_OWNER`] only on positive evidence — the package declares a
/// canonical base and the resource's URL sits under it. Everything else, including
/// a resource that arrived with no package context at all, is [`AUTHORITY_UNKNOWN`].
///
/// The asymmetry is deliberate and load-bearing. Defaulting an unattributed write
/// to OWNER would mean an operator (or, on a server without auth, anyone) replaying
/// `hl7.fhir.r4.core`'s truncated copy through `$import-bundle` would stamp it rank
/// 0 — and since the upsert's `MIN` only ever lowers a rank, that copy would
/// outrank THO's original permanently and reinstate issue #200 with no way back.
pub fn authority_rank_for(url: &str, package_canonical: Option<&str>) -> i32 {
    match package_canonical {
        Some(base) if !base.is_empty() && url_is_under(url, base) => AUTHORITY_OWNER,
        _ => AUTHORITY_UNKNOWN,
    }
}

/// `true` when `url` sits under the canonical `base`, respecting path
/// boundaries: `http://hl7.org/fhir` owns `http://hl7.org/fhir/CodeSystem/x`
/// but NOT `http://hl7.org/fhirpath/x`. A bare `starts_with` would claim the
/// latter and wrongly mark a genuine original as unattributed.
fn url_is_under(url: &str, base: &str) -> bool {
    let base = base.trim_end_matches('/');
    match url.strip_prefix(base) {
        Some("") => true,
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
}

/// A single FHIR CodeSystem resource extracted from a Bundle entry.
#[derive(Debug)]
pub struct ParsedCodeSystem {
    pub id: String,
    pub url: String,
    pub version: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub status: String,
    pub content: String,
    /// Flat list of all concepts in the system (including nested descendants).
    /// Each entry carries `parent_code` so the hierarchy can be reconstructed.
    pub concepts: Vec<ParsedConcept>,
    /// The full original JSON resource (stored in `resource_json` column).
    pub resource_json: Value,
    /// Provenance precedence among rows sharing this canonical URL.
    /// See [`authority_rank_for`].
    pub authority_rank: i32,
}

/// One concept row, already flattened from the potentially nested FHIR tree.
///
/// `parent_code` is `Some` when the concept was either nested under another
/// concept **or** carried a `property[{code:"parent", valueCode:"X"}]` entry.
#[derive(Debug)]
pub struct ParsedConcept {
    pub code: String,
    pub display: Option<String>,
    pub definition: Option<String>,
    pub properties: Vec<ParsedProperty>,
    pub designations: Vec<ParsedDesignation>,
    /// The direct parent code, if any.  Used to populate `concept_hierarchy`.
    pub parent_code: Option<String>,
}

/// A single FHIR concept property (e.g. `inactive`, `parent`).
#[derive(Debug)]
pub struct ParsedProperty {
    pub code: String,
    pub value_type: String,
    pub value: String,
    /// Set to `true` when this property encodes a hierarchy edge (`code == "parent"`).
    pub is_parent_edge: bool,
    /// The parent code for the hierarchy edge (only meaningful when `is_parent_edge`).
    pub parent_code_value: Option<String>,
}

/// A single FHIR concept designation (alternate name or translation).
#[derive(Debug)]
pub struct ParsedDesignation {
    pub language: Option<String>,
    pub use_system: Option<String>,
    pub use_code: Option<String>,
    pub value: String,
}

/// A single FHIR ValueSet resource extracted from a Bundle entry.
#[derive(Debug)]
pub struct ParsedValueSet {
    pub id: String,
    pub url: String,
    pub version: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub status: String,
    /// The `compose` element serialised back to JSON, stored verbatim for
    /// lazy expansion.  `None` when the ValueSet has no `compose`.
    pub compose_json: Option<String>,
    pub resource_json: Value,
    /// Provenance precedence among rows sharing this canonical URL.
    /// See [`authority_rank_for`].
    pub authority_rank: i32,
}

/// A single FHIR ConceptMap resource extracted from a Bundle entry.
#[derive(Debug)]
pub struct ParsedConceptMap {
    pub id: String,
    pub url: String,
    pub version: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub source_uri: Option<String>,
    pub target_uri: Option<String>,
    pub status: String,
    /// Flat list of all source→target mappings across all groups.
    pub elements: Vec<ParsedMapElement>,
    pub resource_json: Value,
}

/// One row in `concept_map_elements`.
#[derive(Debug)]
pub struct ParsedMapElement {
    pub source_system: String,
    pub source_code: String,
    pub target_system: String,
    pub target_code: String,
    pub equivalence: String,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Parse a raw FHIR Bundle JSON payload into a [`ParsedBundle`].
///
/// Validates that the root `resourceType` is `"Bundle"`.  Partitions entries
/// into CodeSystems, ValueSets, and ConceptMaps (in that order, to match
/// import-dependency order).  Unknown resource types are logged at `debug`
/// level and skipped.
///
/// # Errors
///
/// Returns [`HtsError::InvalidRequest`] when the bytes are not valid JSON or
/// the root resource is not a Bundle.
pub fn parse_bundle(data: &[u8]) -> Result<ParsedBundle, HtsError> {
    // No package context: REST writes, `$import-bundle`, and CLI bundles are
    // treated as authoritative for whatever canonical they declare.
    parse_bundle_from_package(data, None)
}

/// Parse a Bundle that arrived inside a FHIR NPM package, whose `package.json`
/// declared `package_canonical` as the canonical base it owns.
///
/// Provenance is an explicit parameter, NOT a field on the payload. An earlier
/// draft smuggled it through a non-FHIR `_htsSourceCanonical` key on the Bundle
/// wrapper, which meant an untrusted `POST /import` body could set the value
/// that decides which definition of a canonical URL is authoritative. Trust
/// input must come from the caller, never from the bytes being parsed.
pub fn parse_bundle_from_package(
    data: &[u8],
    package_canonical: Option<&str>,
) -> Result<ParsedBundle, HtsError> {
    let root: Value = serde_json::from_slice(data)
        .map_err(|e| HtsError::InvalidRequest(format!("Invalid JSON: {e}")))?;

    let resource_type = root["resourceType"].as_str().unwrap_or("");
    if resource_type != "Bundle" {
        return Err(HtsError::InvalidRequest(format!(
            "Expected a FHIR Bundle resource, got '{resource_type}'"
        )));
    }

    let mut bundle = ParsedBundle::default();

    let entries = root["entry"].as_array().cloned().unwrap_or_default();
    for entry in &entries {
        let resource = &entry["resource"];
        if resource.is_null() {
            continue;
        }
        let rid = resource["id"].as_str().unwrap_or("<no-id>");
        match resource["resourceType"].as_str() {
            Some("CodeSystem") => match parse_code_system(resource) {
                Some(mut cs) => {
                    cs.authority_rank = authority_rank_for(&cs.url, package_canonical);
                    bundle.code_systems.push(cs);
                }
                None => bundle.parse_errors.push(format!(
                    "CodeSystem '{rid}' skipped: missing required field 'url'"
                )),
            },
            Some("ValueSet") => match parse_value_set(resource) {
                Some(mut vs) => {
                    vs.authority_rank = authority_rank_for(&vs.url, package_canonical);
                    bundle.value_sets.push(vs);
                }
                None => bundle.parse_errors.push(format!(
                    "ValueSet '{rid}' skipped: missing required field 'url'"
                )),
            },
            Some("ConceptMap") => match parse_concept_map(resource) {
                Some(cm) => bundle.concept_maps.push(cm),
                None => bundle.parse_errors.push(format!(
                    "ConceptMap '{rid}' skipped: missing required field 'url'"
                )),
            },
            Some(rt) => {
                tracing::debug!(
                    resource_type = rt,
                    "Skipping unsupported resource type in Bundle"
                );
            }
            None => {}
        }
    }

    Ok(bundle)
}

// ── CodeSystem parsing ────────────────────────────────────────────────────────

/// Parse a single CodeSystem JSON value (e.g. from a CRUD handler).
///
/// Returns `None` when `url` is absent.
pub fn parse_code_system_value(cs: &Value) -> Option<ParsedCodeSystem> {
    parse_code_system(cs)
}

/// Parse a single ValueSet JSON value.
pub fn parse_value_set_value(vs: &Value) -> Option<ParsedValueSet> {
    parse_value_set(vs)
}

/// Parse a single ConceptMap JSON value.
pub fn parse_concept_map_value(cm: &Value) -> Option<ParsedConceptMap> {
    parse_concept_map(cm)
}

fn parse_code_system(cs: &Value) -> Option<ParsedCodeSystem> {
    let url = cs["url"].as_str()?;

    let id = cs["id"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let mut concepts: Vec<ParsedConcept> = Vec::new();

    let edges = HierarchyProperties::from_code_system(cs);
    if let Some(top_level) = cs["concept"].as_array() {
        flatten_concepts(top_level, None, &edges, &mut concepts);
    }
    edges.apply_child_edges(&mut concepts);
    fill_known_hierarchy(url, &mut concepts);

    Some(ParsedCodeSystem {
        id,
        url: url.to_owned(),
        version: cs["version"].as_str().map(str::to_owned),
        name: cs["name"].as_str().map(str::to_owned),
        title: cs["title"].as_str().map(str::to_owned),
        status: cs["status"].as_str().unwrap_or("active").to_owned(),
        content: cs["content"].as_str().unwrap_or("complete").to_owned(),
        concepts,
        resource_json: cs.clone(),
        authority_rank: AUTHORITY_UNKNOWN,
    })
}

/// Same canonical `url|version`, different content: the replacement policy.
///
/// Returns the warning to record when the incoming CodeSystem must NOT be
/// written over the stored row with the same `url|version`, `None` when it may.
///
/// A CodeSystem version is immutable content, so two different copies under
/// one `url|version` mean one of them is wrong. The common wrong copy is a
/// truncated sample: TW Core 1.0.0 ships every Taiwan ICD-10-CM/PCS code
/// system with 5–18 example codes (still labelled `content=complete`) under
/// the same `url|version` as TW Core 0.3.2's full tables (91–97 k codes).
/// HTS merges concepts on re-import, but the smaller copy's metadata
/// (`content`, `count`, resource JSON) replaced the stored one's, and on a
/// server that only ever saw the sample the full table could not be loaded
/// without being "replaced back" by a later package install.
///
/// Policy: when the stored row is `complete` and the incoming copy has fewer
/// concepts, all of which the stored row already defines, the incoming copy
/// is a subset — a fragment, sample or stub — and is skipped with a warning.
/// A larger copy, or one with codes the stored row lacks, is written as
/// before (concepts merge). Chunked bulk loads (one batch = a subset by
/// design) never reach this check.
pub fn truncated_copy_warning(
    cs: &ParsedCodeSystem,
    stored_content: &str,
    stored_concepts: i64,
    incoming_missing_from_stored: i64,
) -> Option<String> {
    let incoming = cs.concepts.len() as i64;
    if stored_content != "complete" || incoming >= stored_concepts || incoming_missing_from_stored > 0
    {
        return None;
    }
    Some(format!(
        "CodeSystem {}|{} not replaced: the stored copy is complete with {stored_concepts} \
         concepts; the incoming copy (content={}) has {incoming}, all already present",
        cs.url,
        cs.version.as_deref().unwrap_or(""),
        cs.content
    ))
}

/// Code systems HTS loads from their publisher's current release under the
/// version `current` (native importers: NUCC, NDC, RxNorm, ICD-10-CM), whose
/// codes are stable across releases.
///
/// Value sets (VSAC above all) pin the release they were authored against —
/// `{"system": "http://nucc.org/provider-taxonomy", "version": "23.1"}` —
/// and HTS answered UNKNOWN_CODESYSTEM_VERSION for every code because only
/// `current` is stored (CARIN's `EOB-careteam-qualification` invariant
/// failed under remote terminology). For these systems a compose
/// `include.version` / `exclude.version` is advisory: it is dropped from the
/// stored compose (the resource JSON keeps it), so the include resolves to the
/// stored release, as tx.fhir.org effectively does for these systems.
pub const ADVISORY_VERSION_SYSTEMS: [&str; 4] = [
    "http://nucc.org/provider-taxonomy",
    "http://hl7.org/fhir/sid/ndc",
    "http://www.nlm.nih.gov/research/umls/rxnorm",
    "http://hl7.org/fhir/sid/icd-10-cm",
];

/// Drop the advisory `version` of compose includes / excludes over
/// [`ADVISORY_VERSION_SYSTEMS`]. Returns whether anything changed.
pub fn strip_advisory_versions(compose: &mut Value) -> bool {
    let mut changed = false;
    for key in ["include", "exclude"] {
        if let Some(items) = compose.get_mut(key).and_then(Value::as_array_mut) {
            for inc in items {
                let advisory = inc
                    .get("system")
                    .and_then(Value::as_str)
                    .is_some_and(|s| ADVISORY_VERSION_SYSTEMS.contains(&s));
                if advisory
                    && let Some(obj) = inc.as_object_mut()
                    && obj.remove("version").is_some()
                {
                    changed = true;
                }
            }
        }
    }
    changed
}

/// CDC Race & Ethnicity (CDCREC).
pub const CDCREC_URL: &str = "urn:oid:2.16.840.1.113883.6.238";

/// `<child>\t<parent>` edges of the CDCREC race and ethnicity trees.
const CDCREC_HIERARCHY: &str = include_str!("../data/cdcrec-hierarchy.tsv");

/// Supply the hierarchy of a code system known to be distributed flat.
///
/// The CDCREC CodeSystem that reaches HTS (VSAC / PHIN VADS 1.2, 966 codes;
/// THO's own copy is `not-present`) lists its concepts without nesting or
/// parent properties, so `is-a` / `descendent-of 1000-9` filters (US Core
/// `detailed-race`, `detailed-ethnicity`) expanded to nothing and every
/// detailed race code failed validation. The CDCREC trees are the same as
/// THO's `v3-Race` and `v3-Ethnicity` code systems under the abstract roots
/// `1000-9` (Race) and `2133-7` (Ethnicity); the edges are embedded from them.
///
/// Only applies when the incoming content carries no hierarchy at all — a
/// CDCREC with its own nesting or parent properties is kept as published —
/// and only links codes the code system itself defines.
/// The embedded CDCREC `(child, parent)` edges.
pub fn cdcrec_edges() -> Vec<(&'static str, &'static str)> {
    CDCREC_HIERARCHY
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('\t'))
        .collect()
}

/// One-time repair marker (`bootstrap_imports` sentinel row) for databases
/// written before the fixes of the p25-data line: flat CDCREC rows get their
/// hierarchy, and cached expansions over CDCREC and UCUM (computed when
/// enumerated UCUM units and CDCREC descendants were dropped) are cleared.
pub const P25_DATA_MIGRATION_MARKER: &str = "#migration:cdcrec-hierarchy-ucum-enum-p25data";

/// One-time repair marker of the p25-data2 fixes: stored composes lose the
/// advisory versions of [`ADVISORY_VERSION_SYSTEMS`], and cached expansions
/// get their `value_set_dependencies` rows.
pub const P25_DATA2_MIGRATION_MARKER: &str = "#migration:advisory-versions-vs-dependencies-p25data2";

/// URLs whose cached expansions the p25-data migration clears.
pub const P25_DATA_SYSTEMS: [&str; 2] = [CDCREC_URL, "http://unitsofmeasure.org"];

fn fill_known_hierarchy(url: &str, concepts: &mut [ParsedConcept]) {
    if url != CDCREC_URL || concepts.is_empty() || concepts.iter().any(|c| c.parent_code.is_some())
    {
        return;
    }
    let present: std::collections::HashSet<String> =
        concepts.iter().map(|c| c.code.clone()).collect();
    let parents: std::collections::HashMap<&str, &str> = cdcrec_edges().into_iter().collect();
    let mut linked = 0usize;
    for c in concepts.iter_mut() {
        if let Some(parent) = parents.get(c.code.as_str())
            && present.contains(*parent)
        {
            c.parent_code = Some((*parent).to_owned());
            linked += 1;
        }
    }
    tracing::info!(
        url,
        linked,
        concepts = concepts.len(),
        "flat CDCREC CodeSystem: supplied the race/ethnicity hierarchy"
    );
}

/// Which concept properties of a CodeSystem encode hierarchy edges.
///
/// FHIR defines the standard properties `parent` and `child`
/// (`http://hl7.org/fhir/concept-properties#parent` / `#child`); a CodeSystem
/// may declare them under any local code via `CodeSystem.property[].uri`. The
/// HL7 v3 code systems on terminology.hl7.org (v3-ActCode, v3-RoleCode, …)
/// use a flat `concept[]` list with a `subsumedBy` property for the parent
/// link, and nothing else — without recognising it, `is-a` filters and
/// `$subsumes` see no hierarchy at all (fork issue T3, `_ActEncounterCode`).
#[derive(Debug, Default)]
pub(crate) struct HierarchyProperties {
    /// Property codes whose `valueCode` names the concept's parent.
    parent_codes: Vec<String>,
    /// Property codes whose `valueCode` names a child of the concept.
    child_codes: Vec<String>,
}

impl HierarchyProperties {
    pub(crate) fn from_code_system(cs: &Value) -> Self {
        let mut me = Self {
            parent_codes: vec!["parent".into(), "subsumedBy".into()],
            child_codes: vec!["child".into()],
        };
        if let Some(defs) = cs["property"].as_array() {
            for def in defs {
                let (Some(code), Some(uri)) = (def["code"].as_str(), def["uri"].as_str()) else {
                    continue;
                };
                match uri {
                    "http://hl7.org/fhir/concept-properties#parent"
                        if !me.parent_codes.iter().any(|c| c == code) =>
                    {
                        me.parent_codes.push(code.to_owned());
                    }
                    "http://hl7.org/fhir/concept-properties#child"
                        if !me.child_codes.iter().any(|c| c == code) =>
                    {
                        me.child_codes.push(code.to_owned());
                    }
                    _ => {}
                }
            }
        }
        me
    }

    fn is_parent(&self, code: &str) -> bool {
        self.parent_codes.iter().any(|c| c == code)
    }

    fn is_child(&self, code: &str) -> bool {
        self.child_codes.iter().any(|c| c == code)
    }

    /// `child` properties point the other way: `X child Y` means Y's parent
    /// is X. Fill in `parent_code` for children that have no parent yet.
    fn apply_child_edges(&self, concepts: &mut [ParsedConcept]) {
        if self.child_codes.is_empty() {
            return;
        }
        let mut parent_of: Vec<(String, String)> = Vec::new();
        for c in concepts.iter() {
            for p in &c.properties {
                if p.value_type == "code" && self.is_child(&p.code) {
                    parent_of.push((p.value.clone(), c.code.clone()));
                }
            }
        }
        if parent_of.is_empty() {
            return;
        }
        for (child, parent) in parent_of {
            if let Some(c) = concepts.iter_mut().find(|c| c.code == child)
                && c.parent_code.is_none()
            {
                c.parent_code = Some(parent);
            }
        }
    }
}

/// Recursively flatten a nested concept tree into a flat `Vec<ParsedConcept>`.
///
/// `parent_code` is propagated to each child via the nesting relationship.
/// A parent-edge property on a concept (`parent`, `subsumedBy`, or any
/// property declared with the `concept-properties#parent` URI — see
/// [`HierarchyProperties`]) also generates a hierarchy edge to the property's
/// `valueCode` (in addition to or instead of the nesting edge).
fn flatten_concepts(
    concepts: &[Value],
    nesting_parent: Option<&str>,
    edges: &HierarchyProperties,
    out: &mut Vec<ParsedConcept>,
) {
    for concept_val in concepts {
        let code = match concept_val["code"].as_str() {
            Some(c) => c.to_owned(),
            None => continue, // skip malformed
        };

        let display = concept_val["display"].as_str().map(str::to_owned);
        let definition = concept_val["definition"].as_str().map(str::to_owned);

        // Parse properties; collect any additional parent-edge codes.
        let mut properties: Vec<ParsedProperty> = Vec::new();
        let mut property_parent: Option<String> = None;

        if let Some(props) = concept_val["property"].as_array() {
            for prop in props {
                if let Some(p) = parse_property(prop, edges) {
                    if p.is_parent_edge {
                        if let Some(ref pv) = p.parent_code_value {
                            property_parent = Some(pv.clone());
                        }
                    }
                    properties.push(p);
                }
            }
        }

        // Parse designations.
        let mut designations: Vec<ParsedDesignation> = Vec::new();
        if let Some(desigs) = concept_val["designation"].as_array() {
            for desig in desigs {
                if let Some(d) = parse_designation(desig) {
                    designations.push(d);
                }
            }
        }

        // Determine effective parent code: nesting takes precedence; property
        // parent is used when there is no nesting parent.
        let parent_code = nesting_parent.map(str::to_owned).or(property_parent);

        out.push(ParsedConcept {
            code: code.clone(),
            display,
            definition,
            properties,
            designations,
            parent_code,
        });

        // Recurse into nested children.
        if let Some(children) = concept_val["concept"].as_array() {
            flatten_concepts(children, Some(&code), edges, out);
        }
    }
}

fn parse_property(prop: &Value, edges: &HierarchyProperties) -> Option<ParsedProperty> {
    let code = prop["code"].as_str()?.to_owned();
    let (value_type, value) = extract_property_value(prop);
    if value.is_empty() {
        return None;
    }

    let is_parent_edge = edges.is_parent(&code) && value_type == "code";
    let parent_code_value = if is_parent_edge {
        Some(value.clone())
    } else {
        None
    };

    Some(ParsedProperty {
        code,
        value_type,
        value,
        is_parent_edge,
        parent_code_value,
    })
}

fn parse_designation(desig: &Value) -> Option<ParsedDesignation> {
    let value = desig["value"].as_str()?.to_owned();
    Some(ParsedDesignation {
        language: desig["language"].as_str().map(str::to_owned),
        use_system: desig["use"]["system"].as_str().map(str::to_owned),
        use_code: desig["use"]["code"].as_str().map(str::to_owned),
        value,
    })
}

/// Detect the FHIR property value type from the `valueXxx` fields present
/// in the JSON object and return `(value_type, serialised_value)`.
///
/// Returns `("string", "")` when no recognised value field is found.
pub fn extract_property_value(prop: &Value) -> (String, String) {
    if let Some(v) = prop["valueCode"].as_str() {
        return ("code".into(), v.into());
    }
    if let Some(v) = prop["valueBoolean"].as_bool() {
        return ("boolean".into(), v.to_string());
    }
    if let Some(v) = prop["valueInteger"].as_i64() {
        return ("integer".into(), v.to_string());
    }
    if let Some(v) = prop["valueDecimal"].as_f64() {
        return ("decimal".into(), v.to_string());
    }
    if let Some(v) = prop["valueString"].as_str() {
        return ("string".into(), v.into());
    }
    if let Some(v) = prop["valueDateTime"].as_str() {
        return ("dateTime".into(), v.into());
    }
    if let Some(v) = prop["value"].as_str() {
        return ("string".into(), v.into());
    }
    // A Coding property is stored as its code (filters compare codes).
    if let Some(v) = prop["valueCoding"]["code"].as_str() {
        return ("coding".into(), v.into());
    }
    if prop["valueCoding"].is_object() {
        return ("Coding".into(), prop["valueCoding"].to_string());
    }
    ("string".into(), String::new())
}

// ── ValueSet parsing ──────────────────────────────────────────────────────────

fn parse_value_set(vs: &Value) -> Option<ParsedValueSet> {
    let url = vs["url"].as_str()?;

    let id = vs["id"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let compose_json = vs["compose"]
        .as_object()
        .map(|_| {
            let mut compose = vs["compose"].clone();
            strip_advisory_versions(&mut compose);
            serde_json::to_string(&compose).unwrap_or_default()
        })
        .or_else(|| compose_from_expansion(vs).map(|c| c.to_string()));

    Some(ParsedValueSet {
        id,
        url: url.to_owned(),
        version: vs["version"].as_str().map(str::to_owned),
        name: vs["name"].as_str().map(str::to_owned),
        title: vs["title"].as_str().map(str::to_owned),
        status: vs["status"].as_str().unwrap_or("active").to_owned(),
        compose_json,
        resource_json: vs.clone(),
        authority_rank: AUTHORITY_UNKNOWN,
    })
}

/// The membership of a ValueSet published with only an `expansion` (VSAC /
/// eCQM content, cqf-tooling output) as an enumerated compose: one include
/// per system, in expansion order, nested `contains` flattened, abstract
/// groupers left out. The expansion's code system versions are not pinned:
/// the stored expansion already names its members, and pinning e.g. a SNOMED
/// US edition or LOINC 2.72 would make the ValueSet unexpandable on a server
/// that loaded other editions. `None` when there is no expansion to use.
fn compose_from_expansion(vs: &Value) -> Option<Value> {
    fn collect(contains: &[Value], includes: &mut Vec<(String, Vec<Value>)>) {
        for c in contains {
            if let (Some(system), Some(code)) = (c["system"].as_str(), c["code"].as_str()) {
                if c["abstract"].as_bool() != Some(true) {
                    let mut concept = serde_json::json!({ "code": code });
                    if let Some(display) = c["display"].as_str() {
                        concept["display"] = Value::String(display.to_owned());
                    }
                    match includes.iter_mut().find(|(s, _)| s == system) {
                        Some((_, concepts)) => concepts.push(concept),
                        None => includes.push((system.to_owned(), vec![concept])),
                    }
                }
            }
            if let Some(nested) = c["contains"].as_array() {
                collect(nested, includes);
            }
        }
    }
    let contains = vs["expansion"]["contains"].as_array()?;
    let mut includes: Vec<(String, Vec<Value>)> = Vec::new();
    collect(contains, &mut includes);
    if includes.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "include": includes
            .into_iter()
            .map(|(system, concept)| {
                serde_json::json!({
                    "extension": [{
                        "url": crate::backends::STORED_EXPANSION_INCLUDE_EXT,
                        "valueBoolean": true
                    }],
                    "system": system,
                    "concept": concept
                })
            })
            .collect::<Vec<_>>()
    }))
}

// ── ConceptMap parsing ────────────────────────────────────────────────────────

fn parse_concept_map(cm: &Value) -> Option<ParsedConceptMap> {
    let url = cm["url"].as_str()?;

    let id = cm["id"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let source_uri = cm["sourceUri"]
        .as_str()
        .or_else(|| cm["sourceScope"].as_str())
        .map(str::to_owned);

    let target_uri = cm["targetUri"]
        .as_str()
        .or_else(|| cm["targetScope"].as_str())
        .map(str::to_owned);

    let mut elements: Vec<ParsedMapElement> = Vec::new();
    let empty = vec![];
    let groups = cm["group"].as_array().unwrap_or(&empty);

    for group in groups {
        let source_system = group["source"].as_str().unwrap_or("").to_owned();
        let target_system = group["target"].as_str().unwrap_or("").to_owned();
        let group_elements = group["element"].as_array().unwrap_or(&empty);

        for element in group_elements {
            let raw_source_code = element["code"].as_str().unwrap_or("").to_owned();
            let targets = element["target"].as_array().unwrap_or(&empty);

            // HL7 terminology package sometimes encodes multiple source codes as a
            // comma-separated string (e.g. "unconfirmed, provisional"). Split them
            // so each code gets its own row in concept_map_elements.
            let source_codes: Vec<String> = raw_source_code
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();

            for target in targets {
                let target_code = target["code"].as_str().unwrap_or("").to_owned();
                // R4 uses `equivalence`; R5 uses `relationship`. Accept either so
                // ConceptMap fixtures from either FHIR version import correctly.
                let equivalence = target["equivalence"]
                    .as_str()
                    .or_else(|| target["relationship"].as_str())
                    .unwrap_or("equivalent")
                    .to_owned();

                if raw_source_code.is_empty() || target_code.is_empty() {
                    continue;
                }

                for source_code in &source_codes {
                    elements.push(ParsedMapElement {
                        source_system: source_system.clone(),
                        source_code: source_code.clone(),
                        target_system: target_system.clone(),
                        target_code: target_code.clone(),
                        equivalence: equivalence.clone(),
                    });
                }
            }
        }
    }

    Some(ParsedConceptMap {
        id,
        url: url.to_owned(),
        version: cm["version"].as_str().map(str::to_owned),
        name: cm["name"].as_str().map(str::to_owned),
        title: cm["title"].as_str().map(str::to_owned),
        source_uri,
        target_uri,
        status: cm["status"].as_str().unwrap_or("active").to_owned(),
        elements,
        resource_json: cm.clone(),
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parent_of<'a>(cs: &'a ParsedCodeSystem, code: &str) -> Option<&'a str> {
        cs.concepts
            .iter()
            .find(|c| c.code == code)
            .and_then(|c| c.parent_code.as_deref())
    }

    #[test]
    fn flat_cdcrec_gets_the_race_and_ethnicity_hierarchy() {
        let cs = parse_code_system(&serde_json::json!({
            "resourceType": "CodeSystem", "url": CDCREC_URL, "version": "1.2",
            "content": "complete",
            "concept": [
                {"code": "1000-9", "display": "Race"},
                {"code": "1002-5", "display": "American Indian or Alaska Native"},
                {"code": "1004-1", "display": "American Indian"},
                {"code": "2028-9", "display": "Asian"},
                {"code": "2039-6", "display": "Japanese"},
                {"code": "2133-7", "display": "Ethnicity"},
                {"code": "2135-2", "display": "Hispanic or Latino"},
                {"code": "2148-5", "display": "Mexican"}
            ]
        }))
        .unwrap();
        assert_eq!(parent_of(&cs, "1000-9"), None);
        assert_eq!(parent_of(&cs, "1002-5"), Some("1000-9"));
        assert_eq!(parent_of(&cs, "1004-1"), Some("1002-5"));
        assert_eq!(parent_of(&cs, "2039-6"), Some("2028-9"));
        assert_eq!(parent_of(&cs, "2133-7"), None);
        assert_eq!(parent_of(&cs, "2135-2"), Some("2133-7"));
        assert_eq!(parent_of(&cs, "2148-5"), Some("2135-2"));
    }

    #[test]
    fn advisory_versions_are_dropped_from_the_stored_compose_only() {
        let vs = serde_json::json!({
            "resourceType": "ValueSet", "url": "http://cts/vs/1066", "compose": {
                "include": [
                    {"system": "http://nucc.org/provider-taxonomy", "version": "23.1"},
                    {"system": "http://loinc.org", "version": "2.77", "concept": [{"code": "1-8"}]}
                ],
                "exclude": [{"system": "http://hl7.org/fhir/sid/ndc", "version": "x",
                             "concept": [{"code": "0777-3105-02"}]}]
            }
        });
        let parsed = parse_value_set(&vs).unwrap();
        let compose: Value = serde_json::from_str(parsed.compose_json.as_deref().unwrap()).unwrap();
        assert!(compose["include"][0].get("version").is_none());
        assert_eq!(compose["include"][1]["version"], "2.77", "other systems keep their pin");
        assert!(compose["exclude"][0].get("version").is_none());
        assert_eq!(parsed.resource_json["compose"]["include"][0]["version"], "23.1");
    }

    #[test]
    fn cdcrec_table_covers_all_966_codes_in_two_trees() {
        let edges: Vec<(&str, &str)> = CDCREC_HIERARCHY
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.split_once('\t'))
            .collect();
        assert_eq!(edges.len(), 964, "966 codes minus the two roots");
        let parents: std::collections::HashMap<&str, &str> = edges.iter().copied().collect();
        for (child, _) in &edges {
            let mut cur = *child;
            let mut steps = 0;
            while let Some(p) = parents.get(cur) {
                cur = p;
                steps += 1;
                assert!(steps < 10, "cycle at {child}");
            }
            assert!(cur == "1000-9" || cur == "2133-7", "{child} roots at {cur}");
        }
    }

    #[test]
    fn cdcrec_with_its_own_hierarchy_and_other_systems_are_untouched() {
        let nested = parse_code_system(&serde_json::json!({
            "url": CDCREC_URL, "concept": [
                {"code": "1000-9", "concept": [{"code": "2028-9"}]},
                {"code": "1002-5"}
            ]
        }))
        .unwrap();
        assert_eq!(parent_of(&nested, "2028-9"), Some("1000-9"));
        assert_eq!(parent_of(&nested, "1002-5"), None);
        let other = parse_code_system(&serde_json::json!({
            "url": "http://example.org/cs", "concept": [{"code": "1000-9"}, {"code": "1002-5"}]
        }))
        .unwrap();
        assert_eq!(parent_of(&other, "1002-5"), None);
    }

    fn minimal_bundle() -> &'static [u8] {
        br#"{
            "resourceType": "Bundle",
            "type": "collection",
            "entry": [
                {
                    "resource": {
                        "resourceType": "CodeSystem",
                        "id": "cs1",
                        "url": "http://example.org/cs",
                        "name": "TestCS",
                        "status": "active",
                        "content": "complete",
                        "concept": [
                            {
                                "code": "A", "display": "Alpha",
                                "concept": [
                                    { "code": "B", "display": "Beta",
                                      "concept": [{ "code": "C", "display": "Gamma" }]
                                    }
                                ]
                            }
                        ]
                    }
                },
                {
                    "resource": {
                        "resourceType": "ValueSet",
                        "id": "vs1",
                        "url": "http://example.org/vs",
                        "status": "active",
                        "compose": { "include": [{ "system": "http://example.org/cs" }] }
                    }
                },
                {
                    "resource": {
                        "resourceType": "ConceptMap",
                        "id": "cm1",
                        "url": "http://example.org/cm",
                        "status": "active",
                        "group": [{
                            "source": "http://example.org/src",
                            "target": "http://example.org/tgt",
                            "element": [{
                                "code": "A",
                                "target": [{ "code": "X", "equivalence": "equivalent" }]
                            }]
                        }]
                    }
                }
            ]
        }"#
    }

    #[test]
    fn parse_bundle_partitions_resources_correctly() {
        let parsed = parse_bundle(minimal_bundle()).unwrap();
        assert_eq!(parsed.code_systems.len(), 1);
        assert_eq!(parsed.value_sets.len(), 1);
        assert_eq!(parsed.concept_maps.len(), 1);
    }

    #[test]
    fn code_system_concepts_flattened_with_parents() {
        let parsed = parse_bundle(minimal_bundle()).unwrap();
        let cs = &parsed.code_systems[0];
        // A, B, C — three concepts total
        assert_eq!(cs.concepts.len(), 3);

        let a = cs.concepts.iter().find(|c| c.code == "A").unwrap();
        let b = cs.concepts.iter().find(|c| c.code == "B").unwrap();
        let c = cs.concepts.iter().find(|c| c.code == "C").unwrap();

        assert!(a.parent_code.is_none(), "A is a root concept");
        assert_eq!(b.parent_code.as_deref(), Some("A"));
        assert_eq!(c.parent_code.as_deref(), Some("B"));
    }

    #[test]
    fn value_set_compose_json_extracted() {
        let parsed = parse_bundle(minimal_bundle()).unwrap();
        let vs = &parsed.value_sets[0];
        assert!(vs.compose_json.is_some());
        let compose: serde_json::Value =
            serde_json::from_str(vs.compose_json.as_ref().unwrap()).unwrap();
        assert!(compose["include"].is_array());
    }

    /// VSAC-style ValueSets (the eCQM content, cqf tooling output) carry
    /// only an `expansion` — no `compose`. Their membership is that stored
    /// expansion; without a compose it used to expand to nothing, so a CQL
    /// retrieve `[Condition: "Diabetes"]` (via HFS `code:in`) found nothing.
    #[test]
    fn value_set_expansion_only_derives_enumerated_compose() {
        let vs = serde_json::json!({
            "resourceType": "ValueSet",
            "url": "http://cts.nlm.nih.gov/fhir/ValueSet/2.16.840.1.113883.3.464.1003.103.12.1001",
            "version": "20190315",
            "expansion": {
                "timestamp": "2019-03-15T00:00:00Z",
                "contains": [
                    {"system": "http://hl7.org/fhir/sid/icd-10-cm", "version": "2019",
                     "code": "E11.9", "display": "Type 2 diabetes mellitus without complications"},
                    {"system": "http://snomed.info/sct",
                     "version": "http://snomed.info/sct/731000124108/version/20190301",
                     "code": "44054006", "display": "Diabetes mellitus type 2"},
                    {"system": "http://hl7.org/fhir/sid/icd-10-cm", "code": "E10", "abstract": true,
                     "contains": [{"system": "http://hl7.org/fhir/sid/icd-10-cm", "code": "E10.9"}]}
                ]
            }
        });
        let parsed = parse_value_set_value(&vs).unwrap();
        let compose: Value = serde_json::from_str(parsed.compose_json.as_deref().unwrap()).unwrap();
        let includes = compose["include"].as_array().unwrap();
        assert_eq!(includes.len(), 2, "one include per system: {compose}");
        let icd = includes
            .iter()
            .find(|i| i["system"] == "http://hl7.org/fhir/sid/icd-10-cm")
            .unwrap();
        let codes: Vec<&str> = icd["concept"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["code"].as_str().unwrap())
            .collect();
        // nested contains flattened; the abstract grouper is not a member
        assert_eq!(codes, vec!["E11.9", "E10.9"]);
        // the expansion's code system versions are not pinned: the stored
        // expansion already names its members, whatever editions are loaded
        assert!(includes.iter().all(|i| i.get("version").is_none()));
        assert_eq!(icd["concept"][0]["display"], "Type 2 diabetes mellitus without complications");
        // the stored resource itself is untouched
        assert!(parsed.resource_json.get("compose").is_none());
    }

    /// A ValueSet with a compose keeps it, even when it also has an expansion.
    #[test]
    fn value_set_compose_wins_over_expansion() {
        let vs = serde_json::json!({
            "resourceType": "ValueSet", "url": "http://example.org/vs",
            "compose": {"include": [{"system": "http://example.org/cs", "concept": [{"code": "A"}]}]},
            "expansion": {"contains": [{"system": "http://example.org/cs", "code": "B"}]}
        });
        let parsed = parse_value_set_value(&vs).unwrap();
        assert!(parsed.compose_json.unwrap().contains("\"A\""));
        let empty = serde_json::json!({"resourceType": "ValueSet", "url": "http://example.org/e"});
        assert!(parse_value_set_value(&empty).unwrap().compose_json.is_none());
    }

    #[test]
    fn concept_map_elements_flattened() {
        let parsed = parse_bundle(minimal_bundle()).unwrap();
        let cm = &parsed.concept_maps[0];
        assert_eq!(cm.elements.len(), 1);
        assert_eq!(cm.elements[0].source_code, "A");
        assert_eq!(cm.elements[0].target_code, "X");
        assert_eq!(cm.elements[0].equivalence, "equivalent");
    }

    /// R5 ConceptMap targets use `relationship` instead of R4's `equivalence`.
    /// The parser should accept either form so tx-ecosystem fixtures import
    /// regardless of which FHIR version they were authored against.
    #[test]
    fn concept_map_relationship_field_imports_as_equivalence() {
        let bundle = br#"{
            "resourceType": "Bundle",
            "type": "collection",
            "entry": [
                {
                    "resource": {
                        "resourceType": "ConceptMap",
                        "url": "http://example.org/cm-r5",
                        "status": "active",
                        "group": [
                            {
                                "source": "http://example.org/src",
                                "target": "http://example.org/tgt",
                                "element": [
                                    {
                                        "code": "code-1",
                                        "target": [{
                                            "code": "code1",
                                            "relationship": "source-is-narrower-than-target"
                                        }]
                                    }
                                ]
                            }
                        ]
                    }
                }
            ]
        }"#;
        let parsed = parse_bundle(bundle).unwrap();
        assert_eq!(parsed.concept_maps.len(), 1);
        let elem = &parsed.concept_maps[0].elements[0];
        assert_eq!(elem.source_code, "code-1");
        assert_eq!(elem.target_code, "code1");
        assert_eq!(elem.equivalence, "source-is-narrower-than-target");
    }

    #[test]
    fn non_bundle_resource_returns_error() {
        let data = br#"{"resourceType":"Patient","id":"p1"}"#;
        let err = parse_bundle(data).unwrap_err();
        assert!(matches!(err, HtsError::InvalidRequest(_)));
    }

    #[test]
    fn invalid_json_returns_error() {
        let err = parse_bundle(b"not json").unwrap_err();
        assert!(matches!(err, HtsError::InvalidRequest(_)));
    }

    #[test]
    fn unknown_resource_types_are_skipped() {
        let data = br#"{
            "resourceType": "Bundle", "type": "collection",
            "entry": [{ "resource": { "resourceType": "Patient", "id": "p1" } }]
        }"#;
        let parsed = parse_bundle(data).unwrap();
        assert!(parsed.code_systems.is_empty());
        assert!(parsed.value_sets.is_empty());
        assert!(parsed.concept_maps.is_empty());
    }

    #[test]
    fn property_parent_edge_detected() {
        let data = br#"{
            "resourceType": "Bundle", "type": "collection",
            "entry": [{
                "resource": {
                    "resourceType": "CodeSystem",
                    "url": "http://example.org/cs",
                    "status": "active",
                    "content": "complete",
                    "concept": [{
                        "code": "CHILD",
                        "display": "Child",
                        "property": [{ "code": "parent", "valueCode": "ROOT" }]
                    }]
                }
            }]
        }"#;
        let parsed = parse_bundle(data).unwrap();
        let cs = &parsed.code_systems[0];
        let child = cs.concepts.iter().find(|c| c.code == "CHILD").unwrap();
        assert_eq!(child.parent_code.as_deref(), Some("ROOT"));
        let parent_prop = child
            .properties
            .iter()
            .find(|p| p.code == "parent")
            .unwrap();
        assert!(parent_prop.is_parent_edge);
    }

    #[test]
    fn extract_property_value_handles_all_types() {
        let cases = [
            (serde_json::json!({"valueCode": "ABC"}), "code", "ABC"),
            (serde_json::json!({"valueBoolean": true}), "boolean", "true"),
            (serde_json::json!({"valueInteger": 42}), "integer", "42"),
            (
                serde_json::json!({"valueString": "hello"}),
                "string",
                "hello",
            ),
            (
                serde_json::json!({"valueDateTime": "2024-01-01"}),
                "dateTime",
                "2024-01-01",
            ),
        ];
        for (prop, expected_type, expected_value) in &cases {
            let (vt, v) = extract_property_value(prop);
            assert_eq!(vt, *expected_type);
            assert_eq!(v, *expected_value);
        }
    }

    #[test]
    fn scalar_values_keep_precedence_when_coding_is_also_present() {
        let cases = [
            ("valueCode", serde_json::json!("ABC"), "code", "ABC"),
            ("valueBoolean", serde_json::json!(false), "boolean", "false"),
            ("valueInteger", serde_json::json!(0), "integer", "0"),
            ("valueDecimal", serde_json::json!(1.25), "decimal", "1.25"),
            ("valueString", serde_json::json!("text"), "string", "text"),
            (
                "valueDateTime",
                serde_json::json!("2026-01-01"),
                "dateTime",
                "2026-01-01",
            ),
            ("value", serde_json::json!("text"), "string", "text"),
        ];
        for (field, value, expected_type, expected_value) in cases {
            let mut property = serde_json::json!({"valueCoding": {"code": "A"}});
            property[field] = value;
            assert_eq!(
                extract_property_value(&property),
                (expected_type.to_owned(), expected_value.to_owned()),
                "{field} must retain its existing precedence"
            );
        }
    }

    #[test]
    fn coding_property_preserves_the_complete_object() {
        // Fork: a Coding with a `code` is stored as that code (filters compare
        // codes; `$expand` restores the full Coding from the stored resource).
        // Only a Coding without a code keeps the whole object (upstream #2095).
        let coding = serde_json::json!({
            "system": "http://example.org/target",
            "version": "2026",
            "code": "A",
            "display": "Alpha",
            "userSelected": true,
            "extension": [{"url": "http://example.org/note", "valueString": "detail"}]
        });
        let edges = HierarchyProperties::default();
        let property = parse_property(
            &serde_json::json!({"code": "maps-to", "valueCoding": coding}),
            &edges,
        )
        .unwrap();
        assert_eq!(property.value_type, "coding");
        assert_eq!(property.value, "A");
        assert!(!property.is_parent_edge);

        let codeless = serde_json::json!({
            "system": "http://example.org/target",
            "display": "Alpha"
        });
        let property = parse_property(
            &serde_json::json!({"code": "maps-to", "valueCoding": codeless}),
            &edges,
        )
        .unwrap();
        assert_eq!(property.value_type, "Coding");
        assert_eq!(
            serde_json::from_str::<Value>(&property.value).unwrap(),
            codeless
        );
    }

    #[test]
    fn non_object_coding_properties_are_skipped() {
        for value in [
            Value::Null,
            serde_json::json!("A"),
            serde_json::json!(42),
            serde_json::json!(true),
            serde_json::json!([]),
        ] {
            assert!(
                parse_property(
                    &serde_json::json!({"code": "maps-to", "valueCoding": value}),
                    &HierarchyProperties::default(),
                )
                .is_none()
            );
        }
    }
}
