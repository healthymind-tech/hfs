//! Unique logical ids for stored CodeSystems, ValueSets and ConceptMaps.
//!
//! Terminology packages and importers reuse one FHIR `id` for every version
//! of a resource: LOINC 2.80 and 2.83 both declare `"id": "loinc"`, each
//! LOINC answer-list ValueSet keeps its `LL…` id across releases, and
//! `hl7.fhir.r4.core` / THO ship hundreds of CodeSystems under the same id at
//! different versions. Storage keys rows by a synthetic `<id>|<version>`
//! storage id, so the rows themselves never collided — but every one of them
//! reported the declared id, so `GET /CodeSystem?url=http://loinc.org` listed
//! two resources with id `loinc`, and `GET /CodeSystem/loinc` could only
//! guess which one was meant. (`_summary` listings leaked the storage id
//! `loinc|2.80`, which is not a valid FHIR id at all.)
//!
//! # Id scheme
//!
//! Rows are grouped by their *declared* id (`resource_json.id`, never
//! rewritten). Each row also carries a reported id in the `fhir_id` column:
//!
//! * a group of one row reports the declared id unchanged — the common case,
//!   so nothing changes for single-version resources;
//! * in a group of several rows the **default version** (the row the
//!   versionless resolvers pick: the backend's precedence order) reports the
//!   declared id, and every other row reports `<declared id>-<version>`
//!   ([`versioned_id`]), e.g. `loinc-2.80` next to `loinc` (2.83).
//!
//! So `CodeSystem/loinc` keeps resolving to the default version, and every
//! version is addressable by a unique, valid id. Reads also accept
//! `<declared id>-<version>` for the default version itself (see
//! [`versioned_id_candidates`]), so the versioned form of an id is a stable
//! address no matter which version is currently the default. When a newer
//! version becomes the default, the plain id moves to it and the previous
//! default starts reporting its versioned id.
//!
//! The assignment is recomputed for the affected groups after every write and
//! repaired at startup ([`assign`] is the single implementation both backends
//! use), so existing databases holding duplicates are fixed in place. Only
//! the `fhir_id` column changes: storage ids, and with them the concept rows
//! keyed by them, are untouched.

use std::collections::HashSet;

/// FHIR `id` length limit.
const MAX_ID_LEN: usize = 64;

/// One stored row of a declared-id group, in the backend's precedence order
/// (the default version first).
#[derive(Debug, Clone)]
pub(crate) struct GroupRow {
    pub storage_id: String,
    pub declared: String,
    pub version: Option<String>,
    /// The currently stored `fhir_id`.
    pub current: Option<String>,
}

/// `raw` as a valid FHIR id: characters outside `[A-Za-z0-9.-]` become `-`,
/// and an id longer than 64 characters is shortened, keeping it unique with a
/// hash of the full text.
pub(crate) fn sanitize_id(raw: &str) -> String {
    let mapped: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mapped = if mapped.is_empty() {
        "-".to_owned()
    } else {
        mapped
    };
    if mapped.len() <= MAX_ID_LEN {
        return mapped;
    }
    let hash = fnv1a(raw.as_bytes());
    format!("{}-{hash:08x}", &mapped[..MAX_ID_LEN - 9])
}

fn fnv1a(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in bytes {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// The reported id of a non-default version: `<declared>-<version>`.
pub(crate) fn versioned_id(declared: &str, version: Option<&str>) -> String {
    match version.filter(|v| !v.is_empty()) {
        Some(v) => sanitize_id(&format!("{declared}-{v}")),
        None => sanitize_id(&format!("{declared}-unversioned")),
    }
}

/// The id a row without an assigned `fhir_id` reports: its storage id, made
/// valid (`loinc|2.80` → `loinc-2.80`).
pub(crate) fn fallback_id(storage_id: &str) -> String {
    sanitize_id(storage_id)
}

/// `(declared id, version suffix)` splits of a requested id that could be a
/// [`versioned_id`]: every split at a `-`, the longest declared id first.
/// Used by reads to resolve `<declared>-<version>` when no row reports that
/// id (the default version, which reports the plain declared id).
pub(crate) fn versioned_id_candidates(requested: &str) -> Vec<(&str, &str)> {
    let mut out: Vec<(&str, &str)> = requested
        .match_indices('-')
        .map(|(i, _)| (&requested[..i], &requested[i + 1..]))
        .filter(|(d, v)| !d.is_empty() && !v.is_empty())
        .collect();
    out.reverse();
    out
}

/// Compute the reported ids of `rows` (grouped by declared id, each group in
/// precedence order). `reserved` holds ids already reported by rows outside
/// these groups; versioned ids avoid them (and each other) with a numeric
/// suffix. Returns `(storage id, new fhir_id)` for the rows whose id changes.
pub(crate) fn assign(rows: &[GroupRow], reserved: &HashSet<String>) -> Vec<(String, String)> {
    let mut used: HashSet<String> = HashSet::new();
    let mut planned: Vec<(usize, String)> = Vec::with_capacity(rows.len());
    // Pass 1: every group's default version claims its declared id.
    let mut i = 0;
    while i < rows.len() {
        let declared = &rows[i].declared;
        let mut j = i;
        while j < rows.len() && rows[j].declared == *declared {
            j += 1;
        }
        let primary = sanitize_id(declared);
        used.insert(primary.clone());
        planned.push((i, primary));
        i = j;
    }
    // Pass 2: the other versions get `<declared>-<version>`.
    let mut i = 0;
    while i < rows.len() {
        let declared = &rows[i].declared;
        let mut j = i + 1;
        while j < rows.len() && rows[j].declared == *declared {
            let base = versioned_id(declared, rows[j].version.as_deref());
            let mut candidate = base.clone();
            let mut n = 2;
            while used.contains(&candidate) || reserved.contains(&candidate) {
                candidate = sanitize_id(&format!("{base}-{n}"));
                n += 1;
            }
            used.insert(candidate.clone());
            planned.push((j, candidate));
            j += 1;
        }
        i = j;
    }
    planned
        .into_iter()
        .filter(|(idx, id)| rows[*idx].current.as_deref() != Some(id.as_str()))
        .map(|(idx, id)| (rows[idx].storage_id.clone(), id))
        .collect()
}

/// Set the reported `id` of a stored resource.
pub(crate) fn apply_id(resource: &mut serde_json::Value, fhir_id: &str) {
    if let Some(obj) = resource.as_object_mut() {
        obj.insert("id".into(), serde_json::Value::String(fhir_id.to_owned()));
    }
}

/// The id a row reports: its `fhir_id`, or the fallback from its storage id.
pub(crate) fn reported_id(storage_id: &str, fhir_id: Option<&str>) -> String {
    fhir_id.map_or_else(|| fallback_id(storage_id), str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        storage: &str,
        declared: &str,
        version: Option<&str>,
        current: Option<&str>,
    ) -> GroupRow {
        GroupRow {
            storage_id: storage.into(),
            declared: declared.into(),
            version: version.map(Into::into),
            current: current.map(Into::into),
        }
    }

    #[test]
    fn default_version_keeps_the_declared_id() {
        let rows = vec![
            row("loinc|2.83", "loinc", Some("2.83"), Some("loinc")),
            row("loinc|2.80", "loinc", Some("2.80"), Some("loinc")),
            row("v3-loinc|3.1.0", "v3-loinc", Some("3.1.0"), None),
        ];
        let changes = assign(&rows, &HashSet::new());
        assert_eq!(
            changes,
            vec![
                ("v3-loinc|3.1.0".to_string(), "v3-loinc".to_string()),
                ("loinc|2.80".to_string(), "loinc-2.80".to_string()),
            ]
        );
        // Idempotent once applied.
        let applied = vec![
            row("loinc|2.83", "loinc", Some("2.83"), Some("loinc")),
            row("loinc|2.80", "loinc", Some("2.80"), Some("loinc-2.80")),
        ];
        assert!(assign(&applied, &HashSet::new()).is_empty());
    }

    #[test]
    fn versioned_ids_avoid_taken_ids() {
        let rows = vec![
            row("a|2", "a", Some("2"), None),
            row("a|1", "a", Some("1"), None),
            row("x", "a", Some("1"), None),
        ];
        let reserved: HashSet<String> = ["a-1".to_string()].into();
        let changes = assign(&rows, &reserved);
        assert_eq!(changes[0].1, "a");
        assert_eq!(changes[1].1, "a-1-2");
        assert_eq!(changes[2].1, "a-1-3");
    }

    #[test]
    fn ids_are_valid_fhir_ids() {
        assert_eq!(sanitize_id("loinc|2.80"), "loinc-2.80");
        assert_eq!(
            versioned_id(
                "sct",
                Some("http://snomed.info/sct/900000000000207008/version/20250601")
            ),
            "sct-http---snomed.info-sct-900000000000207008-version-20250601"
        );
        let long = sanitize_id(&"x".repeat(100));
        assert_eq!(long.len(), 64);
        assert_ne!(long, sanitize_id(&"x".repeat(101)));
    }

    #[test]
    fn versioned_candidates_split_at_dashes() {
        assert_eq!(
            versioned_id_candidates("v3-EntityRisk-2018-08-12"),
            vec![
                ("v3-EntityRisk-2018-08", "12"),
                ("v3-EntityRisk-2018", "08-12"),
                ("v3-EntityRisk", "2018-08-12"),
                ("v3", "EntityRisk-2018-08-12"),
            ]
        );
        assert_eq!(
            versioned_id("v3-EntityRisk", Some("2018-08-12")),
            "v3-EntityRisk-2018-08-12"
        );
    }
}
