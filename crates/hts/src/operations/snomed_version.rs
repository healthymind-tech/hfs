//! SNOMED CT version URIs.
//!
//! FHIR identifies a SNOMED CT edition and release by URI
//! (<https://terminology.hl7.org/SNOMEDCT.html>):
//! `http://snomed.info/sct/<module>` is the latest release of an edition,
//! `http://snomed.info/sct/<module>/version/<YYYYMMDD>` one release. HTS's
//! RF2 importer stores the release date (`20250601`) as the CodeSystem
//! version.
//!
//! **Stored form stays the date; the URI is translated at the boundary.**
//! The stored version is load-bearing far beyond the `code_systems` row —
//! expansion caches, implicit-ValueSet caches, refset membership,
//! `concept_maps`, admin pins and every `version = ?` resolver compare it —
//! so rewriting it in place would need a migration of all of those plus every
//! importer, with a production SNOMED load (638k concepts) on the line. The
//! boundary is two small, total functions instead:
//!
//! * **Requests** ([`normalize_snomed_versions`], and the compose resolvers
//!   via [`map_version`]): a SNOMED version URI is rewritten to the stored
//!   version it names —
//!   * `…/<module>/version/<date>` → `<date>` when that release is stored;
//!   * `…/900000000000207008` (International edition) → the latest stored
//!     release (a plain-date release is taken to be International unless a
//!     version URI says otherwise);
//!   * `…/<module>` of an edition stored with URI versions → its latest.
//!
//!   A plain date keeps working as before.
//! * **Responses** ([`report_edition_versions`]): a stored plain-date
//!   release is reported as `…/900000000000207008/version/<date>` — the form
//!   tx.fhir.org reports — in `version` / `used-codesystem` /
//!   `system-version`, codings, `expansion.contains`, CodeSystem resources
//!   and messages.
//!
//! An edition that is not loaded (e.g. the US edition `731000124108` when
//! only International is) is left alone and answered as tx.fhir.org answers
//! an edition it lacks: the version is unknown, the code cannot be
//! validated, and the message lists the valid versions. Releases are
//! compared by their date within an edition (`map_version`); a release of
//! another edition is a different version.
//!
//! Known limit: the RF2 importer does not record which edition it loaded, so
//! a plain-date release is always reported as the International edition. An
//! extension edition imported from RF2 (e.g. the US edition) would need its
//! module recorded at import to be reported correctly.

use helios_persistence::tenant::TenantContext;
use serde_json::Value;

use crate::traits::{CodeSystemOperations, TerminologyBackend};

/// The SNOMED CT code system.
pub const SNOMED_URL: &str = "http://snomed.info/sct";
/// The module (edition) id of the International edition.
pub const INTERNATIONAL_MODULE: &str = "900000000000207008";

/// Whether `v` is a bare release date (`YYYYMMDD`), the form the RF2
/// importer stores.
fn is_release_date(v: &str) -> bool {
    v.len() == 8 && v.bytes().all(|b| b.is_ascii_digit())
}

/// The FHIR version URI a stored release is reported as: a plain release
/// date is an International edition release. Other versions (already URIs,
/// a THO stub's `3.2.1`, …) are reported as stored.
pub fn edition_uri(stored: &str) -> Option<String> {
    is_release_date(stored).then(|| format!("{SNOMED_URL}/{INTERNATIONAL_MODULE}/version/{stored}"))
}

/// Rewrite `http://snomed.info/sct|<date>` and
/// `'http://snomed.info/sct' version '<date>'` in free text.
fn rewrite_text(text: &str) -> Option<String> {
    if !text.contains(SNOMED_URL) {
        return None;
    }
    let mut out = String::with_capacity(text.len() + 48);
    let mut rest = text;
    let mut changed = false;
    while let Some(pos) = rest.find(SNOMED_URL) {
        let (before, at) = rest.split_at(pos);
        out.push_str(before);
        out.push_str(SNOMED_URL);
        let after = &at[SNOMED_URL.len()..];
        let (sep, tail) = if let Some(t) = after.strip_prefix('|') {
            ("|", t)
        } else if let Some(t) = after.strip_prefix("' version '") {
            ("' version '", t)
        } else {
            rest = after;
            continue;
        };
        let date_end = tail.bytes().take_while(u8::is_ascii_digit).count();
        let date = &tail[..date_end];
        let closed = sep == "|" || tail[date_end..].starts_with('\'');
        match edition_uri(date).filter(|_| closed) {
            Some(uri) => {
                out.push_str(sep);
                out.push_str(&uri);
                rest = &tail[date_end..];
                changed = true;
            }
            None => rest = after,
        }
    }
    out.push_str(rest);
    changed.then_some(out)
}

fn rewrite_version(slot: &mut Value) {
    if let Some(uri) = slot.as_str().and_then(edition_uri) {
        *slot = Value::String(uri);
    }
}

/// The `value[x]` string of a `Parameters.parameter` entry.
fn param_value_str(p: &Value) -> Option<&str> {
    p.as_object()?
        .iter()
        .find(|(k, _)| k.starts_with("value"))
        .and_then(|(_, v)| v.as_str())
}

/// Report stored SNOMED CT releases in their FHIR edition URI form throughout
/// a response (see the module docs). Idempotent; leaves every other system's
/// versions alone.
pub fn report_edition_versions(value: &mut Value) {
    match value {
        Value::Object(obj) => {
            let is_sct = |k: &str| obj.get(k).and_then(Value::as_str) == Some(SNOMED_URL);
            // Codings, expansion.contains, compose includes, and the SNOMED
            // CodeSystem resource itself.
            let versioned = is_sct("system")
                || (is_sct("url")
                    && obj.get("resourceType").and_then(Value::as_str) == Some("CodeSystem"));
            // TerminologyCapabilities.codeSystem: `{uri, version: [{code}]}`.
            let capability = is_sct("uri");
            // Parameters naming SNOMED as their `system` / `url` ($lookup,
            // $validate-code): their `version` parameter is SNOMED's.
            let sct_params = obj
                .get("parameter")
                .and_then(Value::as_array)
                .is_some_and(|ps| {
                    ps.iter().any(|p| {
                        matches!(
                            p.get("name").and_then(Value::as_str),
                            Some("system" | "url")
                        ) && param_value_str(p) == Some(SNOMED_URL)
                    })
                });
            if versioned && let Some(v) = obj.get_mut("version") {
                rewrite_version(v);
            }
            if capability && let Some(Value::Array(versions)) = obj.get_mut("version") {
                for v in versions {
                    if let Some(code) = v.get_mut("code") {
                        rewrite_version(code);
                    }
                }
            }
            if sct_params && let Some(Value::Array(ps)) = obj.get_mut("parameter") {
                for p in ps {
                    if p.get("name").and_then(Value::as_str) != Some("version") {
                        continue;
                    }
                    if let Some(obj) = p.as_object_mut()
                        && let Some((_, v)) = obj.iter_mut().find(|(k, _)| k.starts_with("value"))
                    {
                        rewrite_version(v);
                    }
                }
            }
            for v in obj.values_mut() {
                report_edition_versions(v);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(report_edition_versions),
        Value::String(s) => {
            if let Some(rewritten) = rewrite_text(s) {
                *s = rewritten;
            }
        }
        _ => {}
    }
}

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
    let system_is_snomed = params.iter().any(|p| {
        param_str(p, "url") == Some(SNOMED_URL) || param_str(p, "system") == Some(SNOMED_URL)
    });
    let mut slots = Vec::new();
    for p in params.iter_mut() {
        let name = p
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let Some(obj) = p.as_object_mut() else {
            continue;
        };
        match name.as_str() {
            "version" | "systemVersion" if system_is_snomed => {
                if let Some((_, v)) = obj.iter_mut().find(|(k, _)| k.starts_with("value")) {
                    slots.push(v);
                }
            }
            "valueSet" => {
                // An inline ValueSet's SNOMED includes / excludes.
                if let Some(compose) = obj
                    .get_mut("resource")
                    .and_then(|vs| vs.get_mut("compose"))
                    .and_then(Value::as_object_mut)
                {
                    for (key, list) in compose.iter_mut() {
                        if key != "include" && key != "exclude" {
                            continue;
                        }
                        for inc in list.as_array_mut().into_iter().flatten() {
                            if inc.get("system").and_then(Value::as_str) == Some(SNOMED_URL)
                                && let Some(v) = inc.get_mut("version")
                            {
                                slots.push(v);
                            }
                        }
                    }
                }
            }
            "coding" | "codingA" | "codingB" => {
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

/// `$expand` version pins (`system-version`, `force-system-version`,
/// `check-system-version`) on SNOMED: their `http://snomed.info/sct|<v>`
/// value slots.
fn snomed_pin_slots(params: &mut [Value]) -> Vec<&mut Value> {
    let mut slots = Vec::new();
    for p in params.iter_mut() {
        let pin = matches!(
            p.get("name").and_then(Value::as_str),
            Some("system-version" | "force-system-version" | "check-system-version")
        );
        if !pin {
            continue;
        }
        if let Some(obj) = p.as_object_mut()
            && let Some((_, v)) = obj.iter_mut().find(|(k, _)| k.starts_with("value"))
            && v.as_str()
                .is_some_and(|s| s.starts_with(&format!("{SNOMED_URL}|")))
        {
            slots.push(v);
        }
    }
    slots
}

/// Rewrites the SNOMED version URIs in `params` to the stored versions they
/// name (see the module docs): `version` / `systemVersion`, coding versions,
/// an inline `valueSet`'s SNOMED includes and `$expand`'s `*-system-version`
/// pins. Looks the stored versions up only when there is a version URI to
/// map.
pub async fn normalize_snomed_versions<B: TerminologyBackend>(
    backend: &B,
    ctx: &TenantContext,
    params: &mut [Value],
) {
    let pin_suffix = |v: &Value| {
        v.as_str()
            .and_then(|s| s.strip_prefix(SNOMED_URL)?.strip_prefix('|'))
            .map(str::to_owned)
    };
    let needs = snomed_version_slots(params)
        .iter()
        .any(|v| v.as_str().is_some_and(|s| parse_version_uri(s).is_some()))
        || snomed_pin_slots(params)
            .iter()
            .any(|v| pin_suffix(v).is_some_and(|s| parse_version_uri(&s).is_some()));
    if !needs {
        return;
    }
    let Some(stored) = stored_versions(backend, ctx).await else {
        return;
    };
    for slot in snomed_version_slots(params) {
        if let Some(mapped) = slot.as_str().and_then(|s| map_version(s, &stored)) {
            *slot = Value::String(mapped);
        }
    }
    for slot in snomed_pin_slots(params) {
        if let Some(mapped) = pin_suffix(slot).and_then(|s| map_version(&s, &stored)) {
            *slot = Value::String(format!("{SNOMED_URL}|{mapped}"));
        }
    }
}

/// The stored version a compose include's `version` names for `url`: the
/// release a SNOMED CT edition URI names (see [`map_version`]); `None` for
/// any other system or version. For the backends' compose resolvers, which
/// see stored ValueSets the request-side normalization never touches.
pub fn map_compose_version<'a>(
    url: &str,
    requested: Option<&str>,
    stored: impl Iterator<Item = &'a str>,
) -> Option<String> {
    let requested = requested.filter(|v| url == SNOMED_URL && parse_version_uri(v).is_some())?;
    let stored: Vec<String> = stored.map(str::to_owned).collect();
    map_version(requested, &stored)
}

/// The SNOMED CT CodeSystem versions that are stored.
async fn stored_versions<B: TerminologyBackend>(
    backend: &B,
    ctx: &TenantContext,
) -> Option<Vec<String>> {
    match CodeSystemOperations::search(
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
        Ok(hits) => Some(
            hits.iter()
                .filter_map(|cs| {
                    cs.get("version")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect(),
        ),
        Err(_) => None,
    }
}

/// The stored version a CodeSystem search's `version=` names, when the
/// search is on SNOMED CT and the version is an edition URI.
pub async fn normalize_search_version<B: TerminologyBackend>(
    backend: &B,
    ctx: &TenantContext,
    query: &mut crate::types::ResourceSearchQuery,
) {
    let on_snomed =
        query.url.as_deref() == Some(SNOMED_URL) || query.system.as_deref() == Some(SNOMED_URL);
    let Some(requested) = query
        .version
        .clone()
        .filter(|v| parse_version_uri(v).is_some())
    else {
        return;
    };
    if !on_snomed {
        return;
    }
    if let Some(stored) = stored_versions(backend, ctx).await
        && let Some(mapped) = map_version(&requested, &stored)
    {
        query.version = Some(mapped);
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
        assert_eq!(
            parse_version_uri("http://snomed.info/sct/1/version/x"),
            None
        );
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

    #[test]
    fn responses_report_releases_as_edition_uris() {
        let uri = "http://snomed.info/sct/900000000000207008/version/20250601";
        let mut out = json!({"resourceType": "Parameters", "parameter": [
            {"name": "system", "valueUri": SNOMED_URL},
            {"name": "version", "valueString": "20250601"},
            {"name": "message", "valueString":
                "Unknown code 'x' in the CodeSystem 'http://snomed.info/sct' version '20250601'"},
            {"name": "codeableConcept", "valueCodeableConcept": {"coding": [
                {"system": SNOMED_URL, "version": "20250601", "code": "1"},
                {"system": "http://loinc.org", "version": "2.80", "code": "2"}]}}
        ]});
        report_edition_versions(&mut out);
        assert_eq!(out["parameter"][1]["valueString"], uri);
        assert_eq!(
            out["parameter"][2]["valueString"],
            format!("Unknown code 'x' in the CodeSystem 'http://snomed.info/sct' version '{uri}'")
        );
        assert_eq!(
            out["parameter"][3]["valueCodeableConcept"]["coding"][0]["version"],
            uri
        );
        assert_eq!(
            out["parameter"][3]["valueCodeableConcept"]["coding"][1]["version"],
            "2.80"
        );
        // Idempotent.
        let once = out.clone();
        report_edition_versions(&mut out);
        assert_eq!(out, once);

        let mut expansion = json!({"expansion": {"parameter": [
            {"name": "used-codesystem", "valueUri": "http://snomed.info/sct|20250601"},
            {"name": "used-codesystem", "valueUri": "http://loinc.org|2.80"}
        ], "contains": [{"system": SNOMED_URL, "version": "20250601", "code": "1"}]}});
        report_edition_versions(&mut expansion);
        assert_eq!(
            expansion["expansion"]["parameter"][0]["valueUri"],
            format!("http://snomed.info/sct|{uri}")
        );
        assert_eq!(
            expansion["expansion"]["parameter"][1]["valueUri"],
            "http://loinc.org|2.80"
        );
        assert_eq!(expansion["expansion"]["contains"][0]["version"], uri);

        // Not a release date: a THO stub's version, a LOINC `version`, a
        // longer number.
        let mut other = json!([
            {"resourceType": "CodeSystem", "url": SNOMED_URL, "version": "3.2.1"},
            {"resourceType": "Parameters", "parameter": [
                {"name": "system", "valueUri": "http://loinc.org"},
                {"name": "version", "valueString": "20250601"}]},
            "http://snomed.info/sct|202506011"
        ]);
        let before = other.clone();
        report_edition_versions(&mut other);
        assert_eq!(other, before);
    }
}
