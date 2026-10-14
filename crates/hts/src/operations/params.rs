//! Helpers for parsing and building FHIR Parameters resources.
//!
//! All FHIR operation endpoints accept and return `application/fhir+json`
//! bodies whose `resourceType` is `"Parameters"`. These helpers encapsulate
//! the common patterns so each handler stays focused on business logic.
//!
//! ## IG Publisher compatibility
//!
//! [HL7 FHIR IG Publisher](https://github.com/HL7/fhir-ig-publisher) often uses
//! parameter **names** that match the value type (`valueCoding`, `valueCodeableConcept`)
//! rather than the FHIR operation definition names (`coding`, `codeableConcept`).
//! [`extract_coding_full`] and [`extract_codeable_concept`] accept both. See
//! [`coding_entries_from_codeable_concept`] for single-object vs array `coding`
//! payloads. Full notes: `docs/ig-publisher-compatibility.md`.

use serde_json::{Value, json};

use crate::error::HtsError;

// ── Parsing helpers ────────────────────────────────────────────────────────────

/// Validate that `body` is a FHIR Parameters resource and return its
/// `parameter` array (empty vec if the field is absent).
pub fn extract_parameter_array(body: &Value) -> Result<Vec<Value>, HtsError> {
    match body.get("resourceType").and_then(|v| v.as_str()) {
        Some("Parameters") => {}
        _ => {
            return Err(HtsError::InvalidRequest(
                "Request body must be a FHIR Parameters resource \
                 (\"resourceType\": \"Parameters\")"
                    .into(),
            ));
        }
    }

    let arr = body
        .get("parameter")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    Ok(arr)
}

/// Return the string value of the first parameter named `name`, or `None`.
pub fn find_str_param(params: &[Value], name: &str) -> Option<String> {
    params
        .iter()
        .find(|p| p.get("name").and_then(|v| v.as_str()) == Some(name))
        .and_then(extract_any_string_value)
}

/// Return the boolean value of the first parameter named `name`, accepting
/// both the POST encoding (`valueBoolean`) and the GET encoding produced by
/// [`query_params_to_fhir_params`] (`valueString` / `valueCode` of `"true"` /
/// `"false"`). `None` when absent or not a recognisable boolean.
pub fn find_bool_param(params: &[Value], name: &str) -> Option<bool> {
    let p = params
        .iter()
        .find(|p| p.get("name").and_then(|v| v.as_str()) == Some(name))?;
    if let Some(b) = p.get("valueBoolean").and_then(|v| v.as_bool()) {
        return Some(b);
    }
    match extract_any_string_value(p)?.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Collect the string values of **all** parameters named `name` (handles
/// repeated parameters such as `property` in `$lookup`).
pub fn collect_str_params(params: &[Value], name: &str) -> Vec<String> {
    params
        .iter()
        .filter(|p| p.get("name").and_then(|v| v.as_str()) == Some(name))
        .filter_map(extract_any_string_value)
        .collect()
}

/// Collect canonical `url|version` parameters by name into `(system_url,
/// version_pattern)` pairs. Used by the IG-style version-pin parameters
/// (`system-version`, `force-system-version`, `check-system-version`) which
/// each carry a `valueCanonical` of the form `"http://...|1.0.x"`.
///
/// Accepts `valueCanonical` / `valueUri` / `valueString` / `valueUrl` so
/// any reasonable encoding from the IG test fixtures is honoured. Entries
/// without a `|` separator (or with empty url/version sides) are skipped.
pub fn collect_canonical_params(params: &[Value], name: &str) -> Vec<(String, String)> {
    params
        .iter()
        .filter(|p| p.get("name").and_then(|v| v.as_str()) == Some(name))
        .filter_map(|p| {
            for key in ["valueCanonical", "valueUri", "valueString", "valueUrl"] {
                if let Some(s) = p.get(key).and_then(|v| v.as_str()) {
                    return Some(s.to_string());
                }
            }
            None
        })
        .filter_map(|c| {
            c.split_once('|').and_then(|(u, v)| {
                if u.is_empty() || v.is_empty() {
                    None
                } else {
                    Some((u.to_string(), v.to_string()))
                }
            })
        })
        .collect()
}

/// Collect every `useSupplement` value, as given (`url` or `url|version`).
///
/// Accepts `valueCanonical`, `valueUri`, `valueUrl` and `valueString`: the GET
/// forms of the operations turn every query parameter into a `valueString`
/// (see [`query_params_to_fhir_params`]), so a reader that only knows the
/// canonical types silently ignores `?useSupplement=` on GET.
pub fn collect_use_supplements(params: &[Value]) -> Vec<String> {
    params
        .iter()
        .filter(|p| p.get("name").and_then(|v| v.as_str()) == Some("useSupplement"))
        .filter_map(|p| {
            ["valueCanonical", "valueUri", "valueUrl", "valueString"]
                .iter()
                .find_map(|k| p.get(*k).and_then(|v| v.as_str()))
                .map(str::to_string)
        })
        .collect()
}

/// Extract a string-typed value from a FHIR parameter object, checking the
/// most common `valueXxx` fields.
fn extract_any_string_value(param: &Value) -> Option<String> {
    const STRING_KEYS: &[&str] = &[
        "valueUri",
        "valueCode",
        "valueString",
        "valueUrl",
        "valueOid",
        "valueId",
        "valueUuid",
        "valueMarkdown",
        "valueCanonical",
        "valueDateTime",
        "valueDate",
    ];

    for key in STRING_KEYS {
        if let Some(s) = param.get(key).and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }

    // For non-string primitives (boolean, integer, decimal) convert to string.
    for key in &["valueBoolean", "valueInteger", "valueDecimal"] {
        if let Some(v) = param.get(key) {
            return Some(v.to_string());
        }
    }

    None
}

/// Extract a `valueCoding` parameter, returning `(system, code, display)`.
///
/// Looks for the first parameter named `name` that carries a `valueCoding`
/// object and returns the `system`, `code`, and optional `display` from it.
/// Returns `None` if the parameter is absent or incomplete.
pub fn extract_coding(params: &[Value], name: &str) -> Option<(String, String, Option<String>)> {
    let (s, c, d, _) = extract_coding_full(params, name)?;
    Some((s, c, d))
}

/// Like [`extract_coding`] but also returns `Coding.version` as the 4th element.
///
/// Accepts parameter name `name` or, when `name` is `"coding"`, the IG
/// Publisher alias `"valueCoding"` (same `valueCoding` payload).
pub fn extract_coding_full(
    params: &[Value],
    name: &str,
) -> Option<(String, String, Option<String>, Option<String>)> {
    extract_coding_full_named(params, name).or_else(|| {
        if name == "coding" {
            extract_coding_full_named(params, "valueCoding")
        } else {
            None
        }
    })
}

/// Resolve a Coding payload from a single Parameters entry.
///
/// Accepts `valueCoding`, a nested `part` array (IG Publisher / Java HAPI
/// client encoding), or an embedded `resource` with `resourceType: Coding`.
fn coding_value_from_parameter(param: &Value) -> Option<Value> {
    if let Some(v) = param.get("valueCoding") {
        return Some(v.clone());
    }
    if let Some(res) = param.get("resource") {
        if res.get("resourceType").and_then(|v| v.as_str()) == Some("Coding") {
            return Some(res.clone());
        }
    }
    let parts = param.get("part")?.as_array()?;
    assemble_coding_from_parts(parts)
}

/// Build a Coding object from a FHIR Parameters `part` list (`system`, `code`, …).
fn assemble_coding_from_parts(parts: &[Value]) -> Option<Value> {
    let mut obj = serde_json::Map::new();
    for part in parts {
        let name = part.get("name").and_then(|v| v.as_str())?;
        if let Some(nested) = part.get("part").and_then(|p| p.as_array()) {
            if name == "coding" {
                if let Some(inner) = assemble_coding_from_parts(nested) {
                    obj.insert(name.to_string(), inner);
                }
            } else if let Some(inner) = assemble_coding_from_parts(nested) {
                obj.insert(name.to_string(), inner);
            }
            continue;
        }
        if let Some(vc) = part.get("valueCoding") {
            obj.insert(name.to_string(), vc.clone());
            continue;
        }
        if let Some(s) = extract_any_string_value(part) {
            obj.insert(name.to_string(), Value::String(s));
        } else if let Some(v) = part.get("valueBoolean") {
            obj.insert(name.to_string(), v.clone());
        }
    }
    if obj.get("code").is_some() {
        Some(Value::Object(obj))
    } else {
        None
    }
}

/// Resolve a CodeableConcept payload from a single Parameters entry.
fn codeable_concept_from_parameter(param: &Value) -> Option<Value> {
    if let Some(v) = param.get("valueCodeableConcept") {
        return Some(v.clone());
    }
    if let Some(res) = param.get("resource") {
        if res.get("resourceType").and_then(|v| v.as_str()) == Some("CodeableConcept") {
            return Some(res.clone());
        }
    }
    let parts = param.get("part")?.as_array()?;
    let mut codings: Vec<Value> = Vec::new();
    for part in parts {
        if part.get("name").and_then(|v| v.as_str()) == Some("coding") {
            if let Some(c) = coding_value_from_parameter(part) {
                codings.push(c);
            }
        }
    }
    if !codings.is_empty() {
        return Some(json!({"coding": codings}));
    }
    assemble_coding_from_parts(parts).map(|c| json!({"coding": [c]}))
}

fn extract_coding_full_named(
    params: &[Value],
    name: &str,
) -> Option<(String, String, Option<String>, Option<String>)> {
    let param = params
        .iter()
        .find(|p| p.get("name").and_then(|v| v.as_str()) == Some(name))?;
    let coding = coding_value_from_parameter(param)?;
    // FHIR ValueSet/$validate-code allows a Coding without `system` (validate
    // by code alone, scoped by VS membership). Fall back to an empty string
    // so downstream paths can detect "no system" without rejecting the
    // request as malformed.
    let system = coding
        .get("system")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let code = coding.get("code").and_then(|v| v.as_str())?.to_string();
    let display = coding
        .get("display")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let version = coding
        .get("version")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Some((system, code, display, version))
}

/// Extract a `resource`-typed parameter by name, returning the resource JSON.
///
/// FHIR Parameters can carry a full resource as `{"name": "...", "resource": {...}}`.
/// This is used by operations like `$expand` which accept an inline `ValueSet`.
pub fn find_resource_param(params: &[Value], name: &str) -> Option<Value> {
    params
        .iter()
        .find(|p| p.get("name").and_then(|v| v.as_str()) == Some(name))?
        .get("resource")
        .cloned()
}

/// Collect every `resource`-typed parameter named `name` (handles repeated
/// `tx-resource` entries that supply ad-hoc terminology only valid for the
/// current request).
pub fn collect_resource_params(params: &[Value], name: &str) -> Vec<Value> {
    params
        .iter()
        .filter(|p| p.get("name").and_then(|v| v.as_str()) == Some(name))
        .filter_map(|p| p.get("resource").cloned())
        .collect()
}

/// Extract a `valueCodeableConcept` parameter, returning all `(system, code)` pairs.
///
/// Accepts parameter name `name` or, when `name` is `"codeableConcept"`, the IG
/// Publisher alias `"valueCodeableConcept"`.
pub fn extract_codeable_concept(params: &[Value], name: &str) -> Option<Vec<(String, String)>> {
    extract_codeable_concept_named(params, name).or_else(|| {
        if name == "codeableConcept" {
            extract_codeable_concept_named(params, "valueCodeableConcept")
        } else {
            None
        }
    })
}

/// Return the `valueCodeableConcept` payload for `codeableConcept` or IG
/// Publisher alias `valueCodeableConcept`.
pub fn find_codeable_concept_param(params: &[Value]) -> Option<Value> {
    for name in ["codeableConcept", "valueCodeableConcept"] {
        if let Some(param) = params
            .iter()
            .find(|p| p.get("name").and_then(|v| v.as_str()) == Some(name))
        {
            if let Some(cc) = codeable_concept_from_parameter(param) {
                return Some(cc);
            }
        }
    }
    None
}

/// Normalize `CodeableConcept.coding` to a vec of Coding objects.
///
/// Accepts a JSON array (FHIR spec) or a single object (IG Publisher / tooling).
pub(crate) fn coding_entries_from_codeable_concept(cc: &Value) -> Option<Vec<Value>> {
    let coding = cc.get("coding")?;
    if let Some(arr) = coding.as_array() {
        Some(arr.clone())
    } else if coding.is_object() {
        Some(vec![coding.clone()])
    } else {
        None
    }
}

fn extract_codeable_concept_named(params: &[Value], name: &str) -> Option<Vec<(String, String)>> {
    let param = params
        .iter()
        .find(|p| p.get("name").and_then(|v| v.as_str()) == Some(name))?;
    let cc = codeable_concept_from_parameter(param)?;
    let codings = coding_entries_from_codeable_concept(&cc)?;
    let pairs = codings
        .iter()
        .filter_map(|c| {
            let system = c.get("system").and_then(|v| v.as_str())?.to_string();
            let code = c.get("code").and_then(|v| v.as_str())?.to_string();
            Some((system, code))
        })
        .collect();
    Some(pairs)
}

// ── Canonical `url|version` helpers ────────────────────────────────────────────

/// Split a FHIR canonical reference of the form `url|version` into its two
/// halves. Returns `(url, None)` when there is no `|` or the version side is
/// empty, so callers can treat the result uniformly.
///
/// The pipe character is reserved in canonical URLs (FHIR R4 §2.3.0.3), so the
/// first `|` is always the version separator.
pub fn split_canonical(raw: &str) -> (&str, Option<&str>) {
    match raw.split_once('|') {
        Some((url, ver)) if !ver.is_empty() => (url, Some(ver)),
        Some((url, _)) => (url, None),
        None => (raw, None),
    }
}

/// Normalise a canonical parameter so that every operation sees the same
/// shape regardless of whether the caller wrote `url=X|V` or
/// `url=X&<version_name>=V`.
///
/// For every parameter named `url_name` whose value is `X|V`:
/// - the parameter value is rewritten to the bare `X` (the `valueXxx` key is
///   preserved), and
/// - the version `V` is promoted into a parameter named `version_name` when
///   the request did not already carry one.
///
/// When the request already carries `version_name` and it disagrees with the
/// piped version, the request is ambiguous and is rejected with
/// [`HtsError::InvalidRequest`] (rendered as a 400 `OperationOutcome`). When
/// `version_name` is `None`, the version suffix is dropped (used for canonical
/// parameters that have no companion version parameter in the operation
/// definition).
///
/// This is the single place that teaches HTS about piped canonicals; `$expand`,
/// `$validate-code`, `$lookup`, `$subsumes`, `$translate` and `?url=` search all
/// call it on entry so the operations behave identically.
pub fn normalize_canonical_params(
    params: &mut Vec<Value>,
    url_name: &str,
    version_name: Option<&str>,
) -> Result<(), HtsError> {
    let mut promoted: Option<String> = None;
    for param in params.iter_mut() {
        if param.get("name").and_then(|v| v.as_str()) != Some(url_name) {
            continue;
        }
        let Some(obj) = param.as_object_mut() else {
            continue;
        };
        let Some((key, raw)) = obj.iter().find_map(|(k, v)| {
            if k.starts_with("value") {
                v.as_str().map(|s| (k.clone(), s.to_string()))
            } else {
                None
            }
        }) else {
            continue;
        };
        if !raw.contains('|') {
            continue;
        }
        let (bare, ver) = split_canonical(&raw);
        let bare = bare.to_string();
        let ver = ver.map(str::to_string);
        obj.insert(key, Value::String(bare));
        if let Some(v) = ver {
            if let Some(prev) = promoted.as_deref()
                && prev != v
            {
                return Err(HtsError::InvalidRequest(format!(
                    "Parameter `{url_name}` was supplied with conflicting versions \
                     ({prev} vs {v})"
                )));
            }
            promoted = Some(v);
        }
    }
    let (Some(ver), Some(version_name)) = (promoted, version_name) else {
        return Ok(());
    };
    match find_str_param(params, version_name) {
        Some(explicit) if explicit != ver => Err(HtsError::InvalidRequest(format!(
            "Parameter `{url_name}` carries version `{ver}` but `{version_name}` is `{explicit}`; \
             supply one or make them agree"
        ))),
        Some(_) => Ok(()),
        None => {
            params.push(json!({"name": version_name, "valueString": ver}));
            Ok(())
        }
    }
}

// ── GET / query-string helpers ─────────────────────────────────────────────────

/// Parse a raw URL query string (e.g. `"system=http%3A...&code=ABC"`) into a
/// list of `(name, value)` pairs, with percent-decoding applied.
///
/// Repeated parameters (`?property=parent&property=child`) each produce a
/// separate entry, so `collect_str_params` works correctly for GET requests.
pub fn parse_query_string(raw: &str) -> Vec<(String, String)> {
    form_urlencoded::parse(raw.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// Convert `(name, value)` pairs (from a GET query string) into a FHIR
/// `parameter` array compatible with `find_str_param` / `collect_str_params`.
///
/// Each pair becomes `{"name": name, "valueString": value}`.
pub fn query_params_to_fhir_params(pairs: Vec<(String, String)>) -> Vec<Value> {
    pairs
        .into_iter()
        .map(|(name, value)| json!({"name": name, "valueString": value}))
        .collect()
}

// ── Building helpers ───────────────────────────────────────────────────────────

/// Map a FHIR property `value_type` string to the correct `valueXxx` key
/// used in a FHIR Parameters `part` entry.
pub fn property_value_part(value_type: &str, raw_value: &str) -> Value {
    match value_type {
        "code" => serde_json::json!({"name": "value", "valueCode": raw_value}),
        "boolean" => {
            let b = raw_value.parse::<bool>().unwrap_or(false);
            serde_json::json!({"name": "value", "valueBoolean": b})
        }
        "integer" => {
            let n = raw_value.parse::<i64>().unwrap_or(0);
            serde_json::json!({"name": "value", "valueInteger": n})
        }
        "decimal" => {
            let f = raw_value.parse::<f64>().unwrap_or(0.0);
            serde_json::json!({"name": "value", "valueDecimal": f})
        }
        "dateTime" => serde_json::json!({"name": "value", "valueDateTime": raw_value}),
        _ => serde_json::json!({"name": "value", "valueString": raw_value}),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn find_bool_param_accepts_boolean_and_string_forms() {
        let params = vec![
            json!({"name": "a", "valueBoolean": true}),
            json!({"name": "b", "valueString": "false"}),
            json!({"name": "c", "valueCode": "true"}),
            json!({"name": "d", "valueString": "yes"}),
        ];
        assert_eq!(find_bool_param(&params, "a"), Some(true));
        assert_eq!(find_bool_param(&params, "b"), Some(false));
        assert_eq!(find_bool_param(&params, "c"), Some(true));
        assert_eq!(find_bool_param(&params, "d"), None);
        assert_eq!(find_bool_param(&params, "e"), None);
    }

    #[test]
    fn split_canonical_handles_plain_piped_and_empty_version() {
        assert_eq!(split_canonical("http://x/vs"), ("http://x/vs", None));
        assert_eq!(
            split_canonical("http://x/vs|2.0"),
            ("http://x/vs", Some("2.0"))
        );
        assert_eq!(split_canonical("http://x/vs|"), ("http://x/vs", None));
        // Only the first pipe separates url from version.
        assert_eq!(
            split_canonical("http://x/vs|1|2"),
            ("http://x/vs", Some("1|2"))
        );
    }

    #[test]
    fn normalize_canonical_promotes_piped_version() {
        let mut params = vec![json!({"name": "url", "valueUri": "http://x/vs|2.0"})];
        normalize_canonical_params(&mut params, "url", Some("valueSetVersion")).unwrap();
        assert_eq!(params[0]["valueUri"], "http://x/vs");
        assert_eq!(
            find_str_param(&params, "valueSetVersion").as_deref(),
            Some("2.0")
        );
    }

    #[test]
    fn normalize_canonical_is_a_no_op_without_pipe() {
        let mut params = vec![
            json!({"name": "url", "valueUri": "http://x/vs"}),
            json!({"name": "valueSetVersion", "valueString": "1.0"}),
        ];
        let before = params.clone();
        normalize_canonical_params(&mut params, "url", Some("valueSetVersion")).unwrap();
        assert_eq!(params, before);
    }

    #[test]
    fn normalize_canonical_accepts_agreeing_explicit_version() {
        let mut params = vec![
            json!({"name": "url", "valueString": "http://x/vs|1.0"}),
            json!({"name": "valueSetVersion", "valueString": "1.0"}),
        ];
        normalize_canonical_params(&mut params, "url", Some("valueSetVersion")).unwrap();
        assert_eq!(params.len(), 2);
        assert_eq!(params[0]["valueString"], "http://x/vs");
    }

    #[test]
    fn normalize_canonical_rejects_conflicting_versions() {
        let mut params = vec![
            json!({"name": "url", "valueUri": "http://x/vs|1.0"}),
            json!({"name": "valueSetVersion", "valueString": "2.0"}),
        ];
        let err =
            normalize_canonical_params(&mut params, "url", Some("valueSetVersion")).unwrap_err();
        assert!(matches!(err, HtsError::InvalidRequest(_)), "{err:?}");
    }

    #[test]
    fn normalize_canonical_drops_version_when_no_slot() {
        let mut params = vec![json!({"name": "source", "valueUri": "http://x/vs|3"})];
        normalize_canonical_params(&mut params, "source", None).unwrap();
        assert_eq!(params.len(), 1);
        assert_eq!(params[0]["valueUri"], "http://x/vs");
    }

    #[test]
    fn extract_parameter_array_ok() {
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [{"name": "code", "valueCode": "ABC"}]
        });
        let params = extract_parameter_array(&body).unwrap();
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn extract_parameter_array_empty_when_no_parameter_key() {
        let body = json!({"resourceType": "Parameters"});
        let params = extract_parameter_array(&body).unwrap();
        assert!(params.is_empty());
    }

    #[test]
    fn extract_parameter_array_rejects_non_parameters() {
        let body = json!({"resourceType": "Patient"});
        let err = extract_parameter_array(&body).unwrap_err();
        assert!(matches!(err, crate::error::HtsError::InvalidRequest(_)));
    }

    #[test]
    fn find_str_param_value_uri() {
        let params = vec![json!({"name": "system", "valueUri": "http://example.org"})];
        assert_eq!(
            find_str_param(&params, "system"),
            Some("http://example.org".into())
        );
    }

    #[test]
    fn find_str_param_value_code() {
        let params = vec![json!({"name": "code", "valueCode": "ABC"})];
        assert_eq!(find_str_param(&params, "code"), Some("ABC".into()));
    }

    #[test]
    fn find_str_param_missing_returns_none() {
        let params: Vec<Value> = vec![];
        assert!(find_str_param(&params, "system").is_none());
    }

    #[test]
    fn collect_str_params_repeated() {
        let params = vec![
            json!({"name": "property", "valueCode": "parent"}),
            json!({"name": "property", "valueCode": "inactive"}),
        ];
        let vals = collect_str_params(&params, "property");
        assert_eq!(vals, vec!["parent", "inactive"]);
    }

    #[test]
    fn property_value_part_code() {
        let part = property_value_part("code", "ROOT");
        assert_eq!(part["valueCode"], "ROOT");
    }

    #[test]
    fn property_value_part_boolean() {
        let part = property_value_part("boolean", "true");
        assert_eq!(part["valueBoolean"], true);
    }

    #[test]
    fn property_value_part_integer() {
        let part = property_value_part("integer", "42");
        assert_eq!(part["valueInteger"], 42);
    }

    #[test]
    fn property_value_part_decimal() {
        let part = property_value_part("decimal", "2.5");
        let v = part["valueDecimal"].as_f64().unwrap();
        assert!((v - 2.5_f64).abs() < 1e-9);
    }

    #[test]
    fn property_value_part_unknown_type_falls_back_to_string() {
        let part = property_value_part("custom", "hello");
        assert_eq!(part["valueString"], "hello");
    }

    // ── extract_coding ─────────────────────────────────────────────────────────

    #[test]
    fn extract_coding_full_fields() {
        let params = vec![json!({
            "name": "coding",
            "valueCoding": {"system": "http://cs.org", "code": "A", "display": "Alpha"}
        })];
        let result = extract_coding(&params, "coding").unwrap();
        assert_eq!(result.0, "http://cs.org");
        assert_eq!(result.1, "A");
        assert_eq!(result.2, Some("Alpha".into()));
    }

    #[test]
    fn extract_coding_without_display() {
        let params = vec![json!({
            "name": "coding",
            "valueCoding": {"system": "http://cs.org", "code": "B"}
        })];
        let result = extract_coding(&params, "coding").unwrap();
        assert_eq!(result.1, "B");
        assert!(result.2.is_none());
    }

    #[test]
    fn extract_coding_missing_param_returns_none() {
        let params: Vec<Value> = vec![];
        assert!(extract_coding(&params, "coding").is_none());
    }

    #[test]
    fn extract_coding_wrong_name_returns_none() {
        let params = vec![json!({
            "name": "other",
            "valueCoding": {"system": "http://cs.org", "code": "A"}
        })];
        assert!(extract_coding(&params, "coding").is_none());
    }

    #[test]
    fn extract_coding_accepts_parameters_part_encoding() {
        let params = vec![json!({
            "name": "coding",
            "part": [
                {"name": "system", "valueUri": "http://snomed.info/sct"},
                {"name": "code", "valueCode": "371530004"},
                {"name": "display", "valueString": "Clinical consultation report"}
            ]
        })];
        let result = extract_coding_full(&params, "coding").unwrap();
        assert_eq!(result.0, "http://snomed.info/sct");
        assert_eq!(result.1, "371530004");
        assert_eq!(result.2.as_deref(), Some("Clinical consultation report"));
    }

    #[test]
    fn extract_coding_accepts_embedded_coding_resource() {
        let params = vec![json!({
            "name": "coding",
            "resource": {
                "resourceType": "Coding",
                "system": "http://loinc.org",
                "code": "98181-1"
            }
        })];
        let result = extract_coding(&params, "coding").unwrap();
        assert_eq!(result.0, "http://loinc.org");
        assert_eq!(result.1, "98181-1");
    }

    #[test]
    fn extract_coding_accepts_value_coding_alias() {
        let params = vec![json!({
            "name": "valueCoding",
            "valueCoding": {"system": "http://cs.org", "code": "A", "version": "current"}
        })];
        let result = extract_coding_full(&params, "coding").unwrap();
        assert_eq!(result.0, "http://cs.org");
        assert_eq!(result.1, "A");
        assert_eq!(result.3.as_deref(), Some("current"));
    }

    // ── extract_codeable_concept ───────────────────────────────────────────────

    #[test]
    fn extract_codeable_concept_multiple_codings() {
        let params = vec![json!({
            "name": "codeableConcept",
            "valueCodeableConcept": {
                "coding": [
                    {"system": "http://cs1.org", "code": "A"},
                    {"system": "http://cs2.org", "code": "B"}
                ]
            }
        })];
        let codings = extract_codeable_concept(&params, "codeableConcept").unwrap();
        assert_eq!(codings.len(), 2);
        assert_eq!(codings[0], ("http://cs1.org".into(), "A".into()));
        assert_eq!(codings[1], ("http://cs2.org".into(), "B".into()));
    }

    #[test]
    fn extract_codeable_concept_skips_incomplete_entries() {
        let params = vec![json!({
            "name": "codeableConcept",
            "valueCodeableConcept": {
                "coding": [
                    {"system": "http://cs.org"},         // missing code
                    {"code": "B"},                       // missing system
                    {"system": "http://cs.org", "code": "C"}
                ]
            }
        })];
        let codings = extract_codeable_concept(&params, "codeableConcept").unwrap();
        assert_eq!(codings.len(), 1);
        assert_eq!(codings[0].1, "C");
    }

    #[test]
    fn extract_codeable_concept_missing_param_returns_none() {
        let params: Vec<Value> = vec![];
        assert!(extract_codeable_concept(&params, "codeableConcept").is_none());
    }

    #[test]
    fn extract_codeable_concept_accepts_parameters_part_encoding() {
        let params = vec![json!({
            "name": "codeableConcept",
            "part": [{
                "name": "coding",
                "part": [
                    {"name": "system", "valueUri": "http://snomed.info/sct"},
                    {"name": "code", "valueCode": "371530004"},
                    {"name": "display", "valueString": "Clinical consultation report"}
                ]
            }]
        })];
        let codings = extract_codeable_concept(&params, "codeableConcept").unwrap();
        assert_eq!(codings.len(), 1);
        assert_eq!(
            codings[0],
            ("http://snomed.info/sct".into(), "371530004".into())
        );
        let cc = find_codeable_concept_param(&params).unwrap();
        assert!(coding_entries_from_codeable_concept(&cc).is_some());
    }

    #[test]
    fn extract_codeable_concept_accepts_value_codeable_concept_alias() {
        let params = vec![json!({
            "name": "valueCodeableConcept",
            "valueCodeableConcept": {
                "coding": [{"system": "http://cs.org", "code": "C"}]
            }
        })];
        let codings = extract_codeable_concept(&params, "codeableConcept").unwrap();
        assert_eq!(codings, vec![("http://cs.org".into(), "C".into())]);
    }

    #[test]
    fn extract_codeable_concept_accepts_single_coding_object() {
        let params = vec![json!({
            "name": "codeableConcept",
            "valueCodeableConcept": {
                "coding": {"system": "http://cs.org", "code": "C"}
            }
        })];
        let codings = extract_codeable_concept(&params, "codeableConcept").unwrap();
        assert_eq!(codings, vec![("http://cs.org".into(), "C".into())]);
    }
}
