//! SNOMED CT version URIs in requests.
//!
//! FHIR identifies a SNOMED CT edition and release by URI
//! (<https://terminology.hl7.org/SNOMEDCT.html>):
//! `http://snomed.info/sct/<module>` is the latest release of an edition,
//! `http://snomed.info/sct/<module>/version/<YYYYMMDD>` one release. HTS's
//! RF2 importer stores the release date (`20250601`) as the CodeSystem
//! version, so every request naming the edition by URI — even the
//! International edition that is loaded — used to fail with "A definition for
//! CodeSystem http://snomed.info/sct could not be found".
//!
//! Before `$validate-code` and `$lookup` run, a SNOMED version URI is
//! rewritten to the stored version it names:
//!
//! * `…/<module>/version/<date>` → `<date>` when that release is stored;
//! * `…/900000000000207008` (International edition) → the latest stored
//!   release (a plain-date release is taken to be International unless a
//!   version URI says otherwise);
//! * `…/<module>` of an edition stored with URI versions → its latest.
//!
//! An edition that is not loaded (e.g. the US edition `731000124108` when
//! only International is) is left alone and answered as tx.fhir.org answers
//! an edition it lacks: the version is unknown, the code cannot be
//! validated, and the message lists the valid versions.

use helios_persistence::tenant::TenantContext;
use serde_json::Value;

use crate::traits::{CodeSystemOperations, TerminologyBackend};

/// The SNOMED CT code system.
pub const SNOMED_URL: &str = "http://snomed.info/sct";
/// The module (edition) id of the International edition.
pub const INTERNATIONAL_MODULE: &str = "900000000000207008";

/// `(module, release date)` of a SNOMED version URI.
pub fn parse_version_uri(version: &str) -> Option<(&str, Option<&str>)> {
    let rest = version.strip_prefix("http://snomed.info/sct/")?;
    let (module, date) = match rest.split_once("/version/") {
        Some((m, d)) => (m, Some(d)),
        None => (rest.trim_end_matches('/'), None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (digits(module) && date.is_none_or(digits)).then_some((module, date))
}

/// The stored version `requested` names, when it is a SNOMED version URI
/// that differs from it (see the module docs). `None` leaves the request
/// as it is.
pub fn map_version(requested: &str, stored: &[String]) -> Option<String> {
    if stored.iter().any(|s| s == requested) {
        return None;
    }
    let (module, date) = parse_version_uri(requested)?;
    let latest = |candidates: Vec<&String>| candidates.into_iter().max().cloned();
    match date {
        Some(date) => stored.iter().find(|s| *s == date).cloned(),
        None => {
            let prefix = format!("http://snomed.info/sct/{module}/version/");
            let as_uri: Vec<&String> = stored.iter().filter(|s| s.starts_with(&prefix)).collect();
            if !as_uri.is_empty() {
                return latest(as_uri);
            }
            if module == INTERNATIONAL_MODULE {
                latest(
                    stored
                        .iter()
                        .filter(|s| s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit()))
                        .collect(),
                )
            } else {
                None
            }
        }
    }
}

fn param_str<'a>(p: &'a Value, name: &str) -> Option<&'a str> {
    (p.get("name")?.as_str()? == name)
        .then(|| {
            p.as_object()?
                .iter()
                .find(|(k, _)| k.starts_with("value"))
                .and_then(|(_, v)| v.as_str())
        })
        .flatten()
}

/// Every place in `params` that carries a SNOMED version: the `version` /
/// `systemVersion` parameters when `url`/`system` is SNOMED, and the
/// `version` of SNOMED codings (`coding`, `codeableConcept.coding[]`).
fn snomed_version_slots(params: &mut [Value]) -> Vec<&mut Value> {
    let system_is_snomed = params
        .iter()
        .any(|p| param_str(p, "url") == Some(SNOMED_URL) || param_str(p, "system") == Some(SNOMED_URL));
    let mut slots = Vec::new();
    for p in params.iter_mut() {
        let name = p.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        let Some(obj) = p.as_object_mut() else {
            continue;
        };
        match name.as_str() {
            "version" | "systemVersion" if system_is_snomed => {
                if let Some((_, v)) = obj.iter_mut().find(|(k, _)| k.starts_with("value")) {
                    slots.push(v);
                }
            }
            "coding" => {
                if let Some(c) = obj.get_mut("valueCoding") {
                    if c.get("system").and_then(Value::as_str) == Some(SNOMED_URL)
                        && let Some(v) = c.get_mut("version")
                    {
                        slots.push(v);
                    }
                }
            }
            "codeableConcept" => {
                if let Some(codings) = obj
                    .get_mut("valueCodeableConcept")
                    .and_then(|cc| cc.get_mut("coding"))
                    .and_then(Value::as_array_mut)
                {
                    for c in codings {
                        if c.get("system").and_then(Value::as_str) == Some(SNOMED_URL)
                            && let Some(v) = c.get_mut("version")
                        {
                            slots.push(v);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    slots
}

/// Rewrites the SNOMED version URIs in `params` to the stored versions they
/// name (see the module docs). Looks the stored versions up only when there
/// is a version URI to map.
pub async fn normalize_snomed_versions<B: TerminologyBackend>(
    backend: &B,
    ctx: &TenantContext,
    params: &mut [Value],
) {
    let needs = snomed_version_slots(params)
        .iter()
        .any(|v| v.as_str().is_some_and(|s| parse_version_uri(s).is_some()));
    if !needs {
        return;
    }
    let stored: Vec<String> = match CodeSystemOperations::search(
        backend,
        ctx,
        crate::types::ResourceSearchQuery {
            url: Some(SNOMED_URL.to_string()),
            count: Some(100),
            summary: Some("true".into()),
            ..Default::default()
        },
    )
    .await
    {
        Ok(hits) => hits
            .iter()
            .filter_map(|cs| cs.get("version").and_then(Value::as_str).map(str::to_string))
            .collect(),
        Err(_) => return,
    };
    for slot in snomed_version_slots(params) {
        if let Some(mapped) = slot.as_str().and_then(|s| map_version(s, &stored)) {
            *slot = Value::String(mapped);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stored() -> Vec<String> {
        vec!["20240101".into(), "20250601".into()]
    }

    #[test]
    fn version_uris_parse() {
        assert_eq!(
            parse_version_uri("http://snomed.info/sct/900000000000207008/version/20250601"),
            Some(("900000000000207008", Some("20250601")))
        );
        assert_eq!(
            parse_version_uri("http://snomed.info/sct/731000124108"),
            Some(("731000124108", None))
        );
        assert_eq!(parse_version_uri("20250601"), None);
        assert_eq!(parse_version_uri("http://snomed.info/sct/abc"), None);
        assert_eq!(parse_version_uri("http://snomed.info/sct/1/version/x"), None);
    }

    #[test]
    fn international_uris_map_to_stored_releases() {
        let s = stored();
        let int = "http://snomed.info/sct/900000000000207008";
        assert_eq!(map_version(int, &s).as_deref(), Some("20250601"));
        assert_eq!(
            map_version(&format!("{int}/version/20240101"), &s).as_deref(),
            Some("20240101")
        );
        // A release that is not stored stays unknown.
        assert_eq!(map_version(&format!("{int}/version/20190731"), &s), None);
        // Plain versions and stored URIs are left alone.
        assert_eq!(map_version("20250601", &s), None);
        assert_eq!(map_version(int, &[int.to_string()]), None);
    }

    #[test]
    fn editions_that_are_not_loaded_stay_unknown() {
        let s = stored();
        assert_eq!(map_version("http://snomed.info/sct/731000124108", &s), None);
        // …unless that edition is stored with URI versions.
        let us = "http://snomed.info/sct/731000124108/version/20250301".to_string();
        assert_eq!(
            map_version("http://snomed.info/sct/731000124108", &[us.clone()]),
            Some(us)
        );
    }

    #[test]
    fn slots_cover_version_params_and_codings() {
        let mut params = vec![
            json!({"name": "url", "valueUri": SNOMED_URL}),
            json!({"name": "version", "valueString": "v1"}),
            json!({"name": "coding", "valueCoding": {"system": SNOMED_URL, "version": "v2"}}),
            json!({"name": "coding", "valueCoding": {"system": "http://loinc.org", "version": "x"}}),
            json!({"name": "codeableConcept", "valueCodeableConcept": {"coding": [
                {"system": SNOMED_URL, "version": "v3"}, {"system": "http://other", "version": "y"}]}}),
        ];
        let found: Vec<String> = snomed_version_slots(&mut params)
            .into_iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(found, ["v1", "v2", "v3"]);
        // `version` of another system is not SNOMED's.
        let mut other = vec![
            json!({"name": "url", "valueUri": "http://loinc.org"}),
            json!({"name": "version", "valueString": "2.80"}),
        ];
        assert!(snomed_version_slots(&mut other).is_empty());
    }
}
