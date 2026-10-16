//! UCUM CodeSystem validation helpers for `http://unitsofmeasure.org`.
//!
//! The UCUM `ucum-essence.xml` import stores atomic prefixes, base units, and
//! named units — not every composed expression (`mg` = milli + gram). tx.fhir.org
//! validates UCUM structurally; HTS mirrors that when a direct concept lookup
//! misses but the code is a well-formed UCUM unit expression.

use octofhir_ucum::validate as validate_ucum_expression;

use crate::types::{ExpansionContains, ValidateCodeResponse};

pub const UCUM_URL: &str = "http://unitsofmeasure.org";

/// True when `url` is the UCUM CodeSystem canonical (optional `|version` suffix).
pub fn is_ucum_url(url: &str) -> bool {
    url.split_once('|').map(|(u, _)| u).unwrap_or(url) == UCUM_URL
}

/// Structural UCUM syntax check (prefix + unit composition, etc.).
///
/// Annotations (`{tbl}`, `{pack-years}`, `10*3{RBCs}`) are handled here:
/// UCUM §6 makes an annotation standing alone (or right after an operator)
/// the unity `1`, and one following a unit a no-op; annotation text is ASCII
/// 33–126 without braces. The expression parser rejects the stand-alone form.
pub fn is_valid_ucum_code(code: &str) -> bool {
    if code.is_empty() {
        return false;
    }
    if validate_ucum_expression(code).is_ok() {
        return true;
    }
    match strip_annotations(code) {
        Some(stripped) if stripped != code => validate_ucum_expression(&stripped).is_ok(),
        _ => false,
    }
}

/// `code` with every `{annotation}` replaced by `1` (stand-alone / after an
/// operator) or removed (after a unit); `None` for a malformed annotation.
fn strip_annotations(code: &str) -> Option<String> {
    let mut out = String::with_capacity(code.len());
    let mut chars = code.chars();
    while let Some(c) = chars.next() {
        if c != '{' {
            if c == '}' {
                return None;
            }
            out.push(c);
            continue;
        }
        let mut closed = false;
        for a in chars.by_ref() {
            if a == '}' {
                closed = true;
                break;
            }
            if a == '{' || !('!'..='~').contains(&a) {
                return None;
            }
        }
        if !closed {
            return None;
        }
        if out.is_empty() || out.ends_with(['/', '.', '(']) {
            out.push('1');
        }
    }
    Some(out)
}

/// Success response for a structurally valid composed UCUM code.
pub fn composed_code_success(code: &str, cs_version: Option<String>) -> ValidateCodeResponse {
    ValidateCodeResponse {
        result: true,
        message: None,
        display: Some(code.to_string()),
        system: Some(UCUM_URL.into()),
        cs_version,
        inactive: None,
        issues: vec![],
        caused_by_unknown_system: None,
        concept_status: None,
        normalized_code: None,
    }
}

/// ValueSet membership for "all of UCUM": a value set whose compose includes
/// `http://unitsofmeasure.org` without `concept`, `filter` or `valueSet`
/// (`v3-UnitsOfMeasureCaseSensitive`, TW PAS `apply-quantity-unit`, …), and
/// excludes nothing from UCUM, contains every well-formed UCUM expression —
/// UCUM is a grammar, so no expansion enumerates `mg/m2` or `{tbl}`. Returns
/// the matched concept for `code` (system taken from the request, or the
/// value set's only include), `None` when the rule does not apply.
pub fn whole_ucum_member(
    compose_json: Option<&str>,
    system: Option<&str>,
    code: &str,
) -> Option<ExpansionContains> {
    if system.is_some_and(|s| !is_ucum_url(s)) || !is_valid_ucum_code(code) {
        return None;
    }
    let compose: serde_json::Value = serde_json::from_str(compose_json?).ok()?;
    let is_ucum = |inc: &serde_json::Value| {
        inc.get("system")
            .and_then(|s| s.as_str())
            .is_some_and(is_ucum_url)
    };
    let includes = compose.get("include")?.as_array()?;
    if system.is_none() && includes.len() != 1 {
        return None; // a bare code must not be claimed for UCUM by guesswork
    }
    let whole = includes.iter().any(|inc| {
        is_ucum(inc)
            && ["concept", "filter", "valueSet"].iter().all(|k| {
                inc.get(*k)
                    .and_then(|v| v.as_array())
                    .is_none_or(|a| a.is_empty())
            })
    });
    let excludes_ucum = compose
        .get("exclude")
        .and_then(|e| e.as_array())
        .is_some_and(|ex| ex.iter().any(is_ucum));
    (whole && !excludes_ucum).then(|| ExpansionContains {
        system: UCUM_URL.into(),
        version: None,
        code: code.to_string(),
        display: None,
        is_abstract: None,
        inactive: None,
        designations: vec![],
        properties: vec![],
        extensions: vec![],
        contains: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotations_are_unity_or_no_ops() {
        for code in [
            "{tbl}",
            "{pack-years}",
            "10*3{RBCs}",
            "mg/{total}",
            "/{HPF}",
            "{a}.mg",
        ] {
            assert!(is_valid_ucum_code(code), "{code}");
        }
        for code in ["{tbl", "tbl}", "{a{b}}", "{a b}"] {
            assert_eq!(strip_annotations(code), None, "{code}");
        }
        assert_eq!(strip_annotations("{tbl}").as_deref(), Some("1"));
        assert_eq!(strip_annotations("mg/{total}").as_deref(), Some("mg/1"));
        assert_eq!(strip_annotations("10*3{RBCs}").as_deref(), Some("10*3"));
        assert!(!is_valid_ucum_code("nope{x}"));
    }

    #[test]
    fn whole_ucum_value_sets_contain_every_well_formed_unit() {
        let all = r#"{"include":[{"system":"http://snomed.info/sct","concept":[{"code":"733006000"}]},{"system":"http://unitsofmeasure.org"}]}"#;
        for code in ["mg/m2", "{tbl}", "{pack-years}", "mm[Hg]", "kg"] {
            let hit = whole_ucum_member(Some(all), Some(UCUM_URL), code);
            assert_eq!(hit.map(|c| c.code), Some(code.to_string()), "{code}");
        }
        assert!(whole_ucum_member(Some(all), Some(UCUM_URL), "not a unit").is_none());
        assert!(
            whole_ucum_member(Some(all), Some("http://snomed.info/sct"), "mg").is_none(),
            "only UCUM codings"
        );
        assert!(
            whole_ucum_member(Some(all), None, "mg").is_none(),
            "a bare code with several includes is not guessed"
        );
        let only = r#"{"include":[{"system":"http://unitsofmeasure.org"}]}"#;
        assert!(whole_ucum_member(Some(only), None, "mg").is_some());

        // Enumerated, filtered or excluding UCUM value sets are real lists.
        for compose in [
            r#"{"include":[{"system":"http://unitsofmeasure.org","concept":[{"code":"kg"}]}]}"#,
            r#"{"include":[{"system":"http://unitsofmeasure.org","filter":[{"property":"canonical","op":"=","value":"g"}]}]}"#,
            r#"{"include":[{"system":"http://unitsofmeasure.org"}],"exclude":[{"system":"http://unitsofmeasure.org","concept":[{"code":"mg"}]}]}"#,
        ] {
            assert!(
                whole_ucum_member(Some(compose), Some(UCUM_URL), "mg").is_none(),
                "{compose}"
            );
        }
        assert!(whole_ucum_member(None, Some(UCUM_URL), "mg").is_none());
    }

    #[test]
    fn accepts_composed_mg() {
        assert!(is_valid_ucum_code("mg"));
    }

    #[test]
    fn accepts_atomic_g_from_essence() {
        assert!(is_valid_ucum_code("g"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(!is_valid_ucum_code("not-a-unit"));
    }
}
