//! LOINC part-property filters that name the part instead of coding it.
//!
//! The LOINC FHIR CodeSystem (<https://loinc.org/fhir/>) types the axis and
//! part-link properties (`COMPONENT`, `SCALE_TYP`, `CLASS`, `document-kind`,
//! `rad-modality-type`, …) as `Coding`: their values are part codes, and HTS
//! stores them so (see [`crate::import::loinc_csv`]). ValueSets in the field
//! use both spellings:
//!
//! * `SCALE_TYP = LP32888-7` (NHI CI `loinc-report-type`) — the part code;
//! * `SCALE_TYP = DOC` (US Core `us-core-documentreference-type`) — the
//!   `Loinc.csv` column value, i.e. the part's name, in any case.
//!
//! tx.fhir.org and the HL7 validator accept both. A filter value on a part
//! property that is not itself a part code therefore also matches the parts
//! whose display or a designation (the importer stores `PartName` and
//! `PartDisplayName` as designations) equals it case-insensitively. The
//! backends call [`wants_name_lookup`] and resolve the names with one query
//! per (system, value), memoised here until the next terminology write.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// The LOINC code system.
pub const LOINC_URL: &str = "http://loinc.org";

/// `true` for the LOINC properties whose values are parts (`Coding`).
pub fn is_part_property(property: &str) -> bool {
    matches!(
        property,
        "COMPONENT"
            | "PROPERTY"
            | "TIME_ASPCT"
            | "SYSTEM"
            | "SCALE_TYP"
            | "METHOD_TYP"
            | "CLASS"
            | "category"
            | "super-system"
            | "system-core"
            | "time-core"
            | "time-modifier"
            | "challenge"
            | "adjustment"
            | "count"
    ) || property.starts_with("analyte")
        || property.starts_with("document-")
        || property.starts_with("rad-")
}

/// `true` for a LOINC part code (`LP` + digits + `-` + check digit).
pub fn is_part_code(value: &str) -> bool {
    let Some((num, check)) = value.strip_prefix("LP").and_then(|r| r.split_once('-')) else {
        return false;
    };
    !num.is_empty()
        && num.bytes().all(|b| b.is_ascii_digit())
        && check.len() == 1
        && check.bytes().all(|b| b.is_ascii_digit())
}

/// `true` when a filter `value` on LOINC `property` should also match the
/// parts it names.
pub fn wants_name_lookup(property: &str, value: &str) -> bool {
    is_part_property(property) && !value.trim().is_empty() && !is_part_code(value.trim())
}

type Cache = Mutex<HashMap<(String, String), Vec<String>>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn key(system_id: &str, value: &str) -> (String, String) {
    (system_id.to_string(), value.trim().to_lowercase())
}

/// The part codes `value` was resolved to in `system_id`, if memoised.
pub fn cached(system_id: &str, value: &str) -> Option<Vec<String>> {
    cache().lock().ok()?.get(&key(system_id, value)).cloned()
}

/// Memoises the part codes `value` names in `system_id`.
pub fn remember(system_id: &str, value: &str, codes: &[String]) {
    if let Ok(mut c) = cache().lock() {
        // Bounded: a pathological client cannot grow it without limit.
        if c.len() > 10_000 {
            c.clear();
        }
        c.insert(key(system_id, value), codes.to_vec());
    }
}

/// Forgets every memoised resolution (any terminology write may change
/// part names).
pub fn clear() {
    if let Ok(mut c) = cache().lock() {
        c.clear();
    }
}

/// `value` followed by the part codes it names, without duplicates.
pub fn with_codes(value: &str, codes: Vec<String>) -> Vec<String> {
    let mut out = vec![value.to_string()];
    for c in codes {
        if !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_codes_are_recognised() {
        assert!(is_part_code("LP32888-7"));
        assert!(is_part_code("LP7057-5"));
        assert!(!is_part_code("LP32888"));
        assert!(!is_part_code("DOC"));
        assert!(!is_part_code("34133-9"));
        assert!(!is_part_code("LP12-34"));
    }

    #[test]
    fn only_part_properties_with_names_need_a_lookup() {
        assert!(wants_name_lookup("SCALE_TYP", "DOC"));
        assert!(wants_name_lookup("CLASS", "DOC.ONTOLOGY"));
        assert!(wants_name_lookup("document-kind", "Note"));
        assert!(wants_name_lookup("rad-modality-type", "CT"));
        assert!(!wants_name_lookup("SCALE_TYP", "LP32888-7"));
        assert!(!wants_name_lookup("STATUS", "ACTIVE"));
        assert!(!wants_name_lookup("CLASSTYPE", "2"));
        assert!(!wants_name_lookup("SCALE_TYP", " "));
    }

    #[test]
    fn memo_is_case_insensitive_and_clearable() {
        remember("loinc-test-memo", "Doc", &["LP32888-7".to_string()]);
        assert_eq!(
            cached("loinc-test-memo", " DOC "),
            Some(vec!["LP32888-7".to_string()])
        );
        assert_eq!(
            with_codes("DOC", vec!["LP1-1".into(), "LP1-1".into()]),
            vec!["DOC".to_string(), "LP1-1".to_string()]
        );
        clear();
        assert_eq!(cached("loinc-test-memo", "doc"), None);
    }
}
