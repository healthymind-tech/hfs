//! Semantic ordering of FHIR resource `version` strings.
//!
//! FHIR leaves `CodeSystem.version` / `ValueSet.version` / `ConceptMap.version`
//! as free text. In practice three families show up in the wild:
//!
//! 1. **Numeric / semver-like** — `4.0.1`, `10.0.0`, `2.80`, `6.2.0-ballot`,
//!    `20250601`. Compared segment by segment as integers; a pre-release
//!    suffix (`-ballot`, `-draft`) sorts *below* the bare release.
//! 2. **Dates** — `2018-08-12`, `2024-01-01T00:00:00Z`. Compared
//!    chronologically.
//! 3. **Anything else** — compared as plain text.
//!
//! "Latest" means: the newest numeric version wins over any date, which wins
//! over any plain-text version. The numeric-over-date rule is deliberate:
//! terminology.hl7.org re-versioned its code systems from dates (`2018-08-12`)
//! to semver (`9.0.0`, `10.0.0`), and the semver rows are the newer ones.
//!
//! The ordering is materialised as a *sort key* — a string that sorts
//! correctly under plain byte-wise comparison — so SQL `ORDER BY` can use it
//! without a custom collation. The key is stored in the `version_sort` column
//! of `code_systems` / `value_sets` (both backends) and is recomputed on every
//! write; [`compare_versions`] gives the same order in Rust.
//!
//! Key layout (`<class>:<body>`; classes sort descending by class digit):
//!
//! ```text
//! 2:0000000004.0000000000.0000000001.0000000000~            4.0.1
//! 2:0000000006.0000000002.0000000000.0000000000-ballot      6.2.0-ballot
//! 1:20180812                                                2018-08-12
//! 0:http://snomed.info/sct/900000000000207008/version/...   free text
//! ```

use std::cmp::Ordering;

/// Width of each zero-padded numeric segment. Wide enough for a `YYYYMMDD`
/// stamp used as a single segment (SNOMED-style `20250601`).
const SEGMENT_WIDTH: usize = 10;
/// Number of segments every numeric key is padded to, so `1.0` and `1.0.0`
/// compare as equal-prefix rather than by length.
const SEGMENT_COUNT: usize = 4;

/// Compute the sortable key for a version string. `None` (no version) maps
/// to the empty string, which sorts below every real version.
pub fn version_sort_key(version: Option<&str>) -> String {
    let Some(raw) = version else {
        return String::new();
    };
    let v = raw.trim();
    if v.is_empty() {
        return String::new();
    }
    if let Some(date) = date_key(v) {
        return format!("1:{date}");
    }
    if let Some(numeric) = numeric_key(v) {
        return format!("2:{numeric}");
    }
    format!("0:{}", v.to_ascii_lowercase())
}

/// Compare two optional version strings semantically. `None` sorts lowest.
pub fn compare_versions(a: Option<&str>, b: Option<&str>) -> Ordering {
    version_sort_key(a).cmp(&version_sort_key(b))
}

/// `YYYY-MM-DD` optionally followed by a time (`THH:MM[:SS][Z|±HH:MM]`), or
/// a bare `YYYY-MM` / `YYYY-MM-DD` partial date. Returns a digits-only key.
fn date_key(v: &str) -> Option<String> {
    let bytes = v.as_bytes();
    // YYYY-MM is the minimum shape that is unambiguously a date.
    if bytes.len() < 7 || !bytes[..4].iter().all(u8::is_ascii_digit) || bytes[4] != b'-' {
        return None;
    }
    if !bytes[5..7].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut key = String::with_capacity(16);
    key.push_str(&v[..4]);
    key.push_str(&v[5..7]);
    let rest = &v[7..];
    if rest.is_empty() {
        key.push_str("00");
        return Some(key);
    }
    let rest = rest.strip_prefix('-')?;
    if rest.len() < 2 || !rest.as_bytes()[..2].iter().all(u8::is_ascii_digit) {
        return None;
    }
    key.push_str(&rest[..2]);
    let time = &rest[2..];
    if time.is_empty() {
        return Some(key);
    }
    // Time part: keep digits only, so `T10:30:00Z` → `103000`. Anything that
    // is not a recognisable time marker makes this not-a-date.
    let time = time.strip_prefix('T')?;
    let digits: String = time.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    key.push_str(&digits);
    Some(key)
}

/// Dotted numeric segments with an optional pre-release / build suffix.
/// `1`, `1.0`, `4.0.1`, `2.80`, `6.2.0-ballot`, `1.0.0+build.5`.
fn numeric_key(v: &str) -> Option<String> {
    // Split off a pre-release suffix at the first `-` or `+`.
    let (core, suffix) = match v.find(['-', '+']) {
        Some(pos) => (&v[..pos], Some(&v[pos..])),
        None => (v, None),
    };
    if core.is_empty() {
        return None;
    }
    let mut segments: Vec<u64> = Vec::with_capacity(SEGMENT_COUNT);
    for seg in core.split('.') {
        if seg.is_empty() || !seg.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        // Segments wider than the key width cannot be ordered correctly;
        // treat the whole string as text instead of silently truncating.
        if seg.len() > SEGMENT_WIDTH {
            return None;
        }
        segments.push(seg.parse().ok()?);
    }
    let mut key = String::with_capacity(SEGMENT_COUNT * (SEGMENT_WIDTH + 1) + 8);
    let shown = segments.len().max(SEGMENT_COUNT);
    for i in 0..shown {
        if i > 0 {
            key.push('.');
        }
        let n = segments.get(i).copied().unwrap_or(0);
        key.push_str(&format!("{n:0SEGMENT_WIDTH$}"));
    }
    match suffix {
        // `~` (0x7E) sorts above `-`/`+` and every printable suffix, so a bare
        // release beats its own pre-releases.
        None => key.push('~'),
        Some(s) => key.push_str(&s.to_ascii_lowercase()),
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn newest<'a>(versions: &[&'a str]) -> &'a str {
        versions
            .iter()
            .copied()
            .max_by(|a, b| compare_versions(Some(a), Some(b)))
            .unwrap()
    }

    #[test]
    fn numeric_segments_compare_as_integers_not_text() {
        assert_eq!(newest(&["4.0.1", "10.0.0", "9.0.0"]), "10.0.0");
        assert_eq!(newest(&["2.9", "2.80", "2.10"]), "2.80");
        assert_eq!(newest(&["1", "1.0.1"]), "1.0.1");
        assert_eq!(
            compare_versions(Some("1.0"), Some("1.0.0")),
            Ordering::Equal
        );
    }

    #[test]
    fn tho_date_versions_sort_below_semver() {
        // terminology.hl7.org v3-ActCode: 2018-08-12 → 9.0.0 → 10.0.0.
        assert_eq!(newest(&["2018-08-12", "9.0.0", "10.0.0"]), "10.0.0");
        assert_eq!(newest(&["2018-08-12", "2.0.0"]), "2.0.0");
    }

    #[test]
    fn dates_sort_chronologically() {
        assert_eq!(
            newest(&["2018-08-12", "2019-01-01", "2018-12-31"]),
            "2019-01-01"
        );
        assert_eq!(
            newest(&["2024-01-01T10:00:00Z", "2024-01-01T09:00:00Z"]),
            "2024-01-01T10:00:00Z"
        );
        assert_eq!(newest(&["2024-01", "2023-12-31"]), "2024-01");
    }

    #[test]
    fn prerelease_sorts_below_release() {
        assert_eq!(newest(&["6.2.0-ballot", "6.2.0"]), "6.2.0");
        assert_eq!(newest(&["6.2.0-ballot", "6.1.0"]), "6.2.0-ballot");
        assert_eq!(newest(&["1.0.0+build.5", "1.0.0"]), "1.0.0");
    }

    #[test]
    fn snomed_style_date_stamp_is_numeric() {
        assert_eq!(newest(&["20240601", "20250601", "20241201"]), "20250601");
    }

    #[test]
    fn free_text_sorts_below_everything_else_and_textually() {
        assert_eq!(newest(&["draft", "1.0"]), "1.0");
        assert_eq!(newest(&["draft", "2018-08-12"]), "2018-08-12");
        assert_eq!(newest(&["abc", "abd"]), "abd");
        assert_eq!(
            newest(&[
                "http://snomed.info/sct/900000000000207008/version/20240101",
                "http://snomed.info/sct/900000000000207008/version/20250601"
            ]),
            "http://snomed.info/sct/900000000000207008/version/20250601"
        );
    }

    #[test]
    fn missing_version_sorts_lowest() {
        assert_eq!(version_sort_key(None), "");
        assert_eq!(version_sort_key(Some("  ")), "");
        assert_eq!(compare_versions(None, Some("0.0.1")), Ordering::Less);
        assert_eq!(compare_versions(None, Some("x")), Ordering::Less);
    }

    #[test]
    fn keys_are_byte_sortable() {
        // The SQL side orders by the raw key bytes; make sure that agrees
        // with the Rust comparator for a representative mix.
        let mut versions = vec![
            "4.0.1",
            "10.0.0",
            "9.0.0",
            "2018-08-12",
            "6.2.0-ballot",
            "6.2.0",
            "draft",
            "2.80",
            "20250601",
        ];
        versions.sort_by(|a, b| compare_versions(Some(a), Some(b)));
        let mut by_key = versions.clone();
        by_key.sort_by_key(|v| version_sort_key(Some(v)));
        assert_eq!(versions, by_key);
        assert_eq!(
            versions,
            vec![
                "draft",
                "2018-08-12",
                "2.80",
                "4.0.1",
                "6.2.0-ballot",
                "6.2.0",
                "9.0.0",
                "10.0.0",
                "20250601"
            ]
        );
    }

    #[test]
    fn oversized_segments_fall_back_to_text() {
        assert_eq!(version_sort_key(Some("123456789012.0")), "0:123456789012.0");
    }
}
