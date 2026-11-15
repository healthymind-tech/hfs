//! Resource-specific rules the HL7 validator checks beyond what any profile
//! states: defining canonical URLs, `Identifier` values under the RFC 3986
//! system, and the authoring rules for CodeSystem and ValueSet resources
//! (`CodeSystemValidator` / `ValueSetValidator` in the HL7 Java validator).
//!
//! Only the rules that need no terminology server are here: the ones that
//! look a code up (supplement concepts, filter values, ECL) need the
//! terminology service and are not implemented. Messages are the HL7
//! validator's, so issues line up with its output.

use super::errors::{ErrorKind, ValidationError};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

/// Every resource of the instance (root, contained, Bundle entries, ...)
/// with its dotted path, and whether it is a contained resource.
fn resources<'a>(
    value: &'a Value,
    path: String,
    contained: bool,
    out: &mut Vec<(String, &'a Value, bool)>,
) {
    match value {
        Value::Object(map) => {
            if map.get("resourceType").is_some_and(Value::is_string) {
                out.push((path.clone(), value, contained));
            }
            for (key, child) in map {
                if child.is_object() || child.is_array() {
                    resources(child, format!("{path}.{key}"), key == "contained", out);
                }
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                resources(item, format!("{path}.{index}"), contained, out);
            }
        }
        _ => {}
    }
}

/// The R4/R5 resource types whose `url` element is their defining
/// canonical URL.
const CANONICAL_RESOURCES: &[&str] = &[
    "ActivityDefinition",
    "ActorDefinition",
    "CapabilityStatement",
    "ChargeItemDefinition",
    "CodeSystem",
    "CompartmentDefinition",
    "ConceptMap",
    "ConditionDefinition",
    "EffectEvidenceSynthesis",
    "EventDefinition",
    "Evidence",
    "EvidenceReport",
    "EvidenceVariable",
    "ExampleScenario",
    "GraphDefinition",
    "ImplementationGuide",
    "Library",
    "Measure",
    "MessageDefinition",
    "NamingSystem",
    "ObservationDefinition",
    "OperationDefinition",
    "PlanDefinition",
    "Questionnaire",
    "Requirements",
    "ResearchDefinition",
    "ResearchElementDefinition",
    "RiskEvidenceSynthesis",
    "SearchParameter",
    "SpecimenDefinition",
    "StructureDefinition",
    "StructureMap",
    "SubscriptionTopic",
    "TerminologyCapabilities",
    "TestPlan",
    "TestScript",
    "ValueSet",
];

/// `Utilities.isToken`: a non-empty run of letters, digits, `-`, `_`,
/// `.`, `+` starting with a letter.
fn is_token(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
}

/// `Utilities.isAbsoluteUrl` of the HL7 validator.
pub(crate) fn is_absolute_url(url: &str) -> bool {
    let Some((scheme, details)) = url.split_once(':') else {
        return false;
    };
    let scheme_ok = matches!(scheme, "http" | "https" | "urn")
        || (is_token(scheme) && scheme == scheme.to_lowercase());
    scheme_ok && !details.is_empty() && !details.contains(' ')
}

/// `InstanceValidator.isAbsolute` (Identifier values): a token scheme
/// before the first `:` (`file:` needs a path or a drive).
fn is_absolute(uri: &str) -> bool {
    let Some((protocol, tail)) = uri.split_once(':') else {
        return false;
    };
    if !is_token(protocol) {
        return false;
    }
    protocol != "file" || tail.starts_with('/') || tail.contains(':')
}

/// `UUIDUtilities.isValidUUID`: lowercase 8-4-4-4-12 hex.
fn is_valid_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(p, n)| p.len() == n && p.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')))
}

fn err(errors: &mut Vec<ValidationError>, path: String, message: String) {
    errors.push(ValidationError::new(ErrorKind::ResourceRule, path, message));
}

/// All resource-rule errors of `resource` (and the resources nested in it).
pub(crate) fn resource_rule_errors(resource: &Value) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let root = resource
        .get("resourceType")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    identifier_values(resource, root.clone(), &mut errors);
    let mut found = Vec::new();
    resources(resource, root, false, &mut found);
    for (path, res, contained) in found {
        let Some(rt) = res.get("resourceType").and_then(Value::as_str) else {
            continue;
        };
        if CANONICAL_RESOURCES.contains(&rt) {
            canonical_url(res, &path, contained, &mut errors);
        }
        match rt {
            "CodeSystem" => code_system(res, &path, &mut errors),
            "ValueSet" => value_set(res, &path, &mut errors),
            _ => {}
        }
    }
    errors
}

/// `Identifier.system = urn:ietf:rfc:3986` requires a full URI as value.
/// An Identifier is recognised as an object with a string `system` and a
/// string `value` (Coding has `code`, Quantity a numeric `value`).
fn identifier_values(value: &Value, path: String, errors: &mut Vec<ValidationError>) {
    match value {
        Value::Object(map) => {
            if map.get("system").and_then(Value::as_str) == Some("urn:ietf:rfc:3986")
                && let Some(v) = map.get("value").and_then(Value::as_str)
                && !map.contains_key("code")
                && !is_absolute(v)
            {
                err(
                    errors,
                    path.clone(),
                    format!(
                        "if identifier.system is 'urn:ietf:rfc:3986', then the identifier.value must be a full URI (e.g. start with a scheme), not '{v}'"
                    ),
                );
            }
            for (key, child) in map {
                if child.is_object() || child.is_array() {
                    identifier_values(child, format!("{path}.{key}"), errors);
                }
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                identifier_values(item, format!("{path}.{index}"), errors);
            }
        }
        _ => {}
    }
}

/// The defining `url` of a canonical resource must be absolute (a
/// `urn:uuid:` one a valid lowercase UUID).
fn canonical_url(res: &Value, path: &str, contained: bool, errors: &mut Vec<ValidationError>) {
    let Some(url) = res.get("url").and_then(Value::as_str) else {
        return;
    };
    let at = format!("{path}.url");
    if let Some(rest) = url.strip_prefix("urn:uuid:") {
        let uuid = rest.split('#').next().unwrap_or(rest);
        let uuid = uuid.rsplit_once('|').map_or(uuid, |(u, _)| u);
        if !is_valid_uuid(uuid) {
            err(
                errors,
                at.clone(),
                format!("UUIDs must be valid and lowercase ({uuid})"),
            );
        }
    }
    if !is_absolute_url(url) {
        let message = if contained {
            format!(
                "Canonical URLs in contained resources must be absolute URLs if present ({url})"
            )
        } else {
            format!(
                "Canonical URLs must be absolute URLs if they are not fragment references ({url})"
            )
        };
        err(errors, at, message);
    }
}

fn str_of<'a>(map: &'a Value, key: &str) -> Option<&'a str> {
    map.get(key).and_then(Value::as_str)
}

fn array_of<'a>(map: &'a Value, key: &str) -> &'a [Value] {
    map.get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Concepts at every nesting level.
fn count_concepts(concepts: &[Value]) -> usize {
    concepts
        .iter()
        .map(|c| 1 + count_concepts(array_of(c, "concept")))
        .sum()
}

/// The properties the HL7 validator knows by URI and code
/// (`CodeSystemValidator.KnownProperty`), with their types.
const KNOWN_PROPERTIES: &[(&str, Option<&str>)] = &[
    ("status", Some("code")),
    ("inactive", Some("boolean")),
    ("effectiveDate", Some("dateTime")),
    ("deprecationDate", Some("dateTime")),
    ("retirementDate", Some("dateTime")),
    ("notSelectable", Some("boolean")),
    ("parent", Some("code")),
    ("child", Some("code")),
    ("partOf", Some("code")),
    ("synonym", Some("code")),
    ("comment", None),
    ("itemWeight", Some("decimal")),
];

const CONCEPT_PROPERTIES: &str = "http://hl7.org/fhir/concept-properties#";

fn known_by_code(code: &str) -> Option<(&'static str, Option<&'static str>)> {
    KNOWN_PROPERTIES.iter().copied().find(|(c, _)| *c == code)
}

/// A defined property: its type and whether its `code` values must be
/// codes of this code system (`INTERNAL_CODE`: parent, child, partOf,
/// synonym by URI).
struct PropertyDef {
    ty: Option<String>,
    internal_codes: bool,
}

fn code_system(cs: &Value, path: &str, errors: &mut Vec<ValidationError>) {
    let url = str_of(cs, "url");
    let content = str_of(cs, "content");
    let supplements = str_of(cs, "supplements").filter(|s| !s.is_empty());
    let concepts = array_of(cs, "concept");
    let count = count_concepts(concepts);

    // metaChecks
    if supplements.is_some() {
        if content != Some("supplement") {
            err(
                errors,
                format!("{path}.content"),
                "CodeSystem Supplements SHALL have a content value of 'supplement'".to_string(),
            );
        }
    } else if content == Some("supplement") {
        err(
            errors,
            format!("{path}.content"),
            "CodeSystem Supplements with a content value of 'supplement' SHALL have a supplements element that specifies which code system is being supplemented".to_string(),
        );
    }
    if content == Some("complete")
        && let Some(stated) = cs.get("count").and_then(Value::as_i64)
        && stated >= 0
        && stated as usize != count
    {
        err(
            errors,
            format!("{path}.count"),
            format!(
                "The code system is complete, but the number of concepts ({count}) does not match the stated total number ({stated})"
            ),
        );
    }
    if content == Some("not-present") && !concepts.is_empty() {
        err(
            errors,
            format!("{path}.concept.0"),
            "The code system says it has no content present, but concepts are found".to_string(),
        );
    }

    // property definitions
    let mut properties: HashMap<String, PropertyDef> = HashMap::new();
    for (i, property) in array_of(cs, "property").iter().enumerate() {
        check_property_definition(
            cs,
            url,
            property,
            &format!("{path}.property.{i}"),
            &mut properties,
            errors,
        );
    }

    // concepts: duplicate codes, then property values
    let mut codes: HashSet<String> = HashSet::new();
    check_concept_codes(concepts, &format!("{path}.concept"), &mut codes, errors);
    check_concept_properties(
        concepts,
        &format!("{path}.concept"),
        &properties,
        &codes,
        supplements.is_some(),
        errors,
    );
}

fn check_property_definition(
    cs: &Value,
    url: Option<&str>,
    property: &Value,
    at: &str,
    properties: &mut HashMap<String, PropertyDef>,
    errors: &mut Vec<ValidationError>,
) {
    let uri = str_of(property, "uri");
    let code = str_of(property, "code");
    let ty = str_of(property, "type");
    let mut by_uri: Option<(&str, Option<&str>)> = None;
    let mut internal_codes = false;
    let _ = cs;
    if let Some(uri) = uri {
        if !is_absolute_url(uri) {
            err(
                errors,
                at.to_string(),
                format!("A property URI must be an absolute URI, but '{uri}' is not"),
            );
        } else {
            if properties.contains_key(uri) {
                err(
                    errors,
                    at.to_string(),
                    format!("A property is already defined with the URI '{uri}'"),
                );
            } else {
                properties.insert(
                    uri.to_string(),
                    PropertyDef {
                        ty: ty.map(str::to_string),
                        internal_codes: false,
                    },
                );
            }
            if uri.contains("://hl7.org/fhir") {
                by_uri = uri.strip_prefix(CONCEPT_PROPERTIES).and_then(known_by_code);
                if by_uri.is_none() {
                    let base_spec = url.is_some_and(|u| {
                        u.strip_prefix("http://hl7.org/fhir/")
                            .is_some_and(|rest| !rest.contains('/'))
                    });
                    let self_ref = url.is_some_and(|u| uri.starts_with(u));
                    if !base_spec && !self_ref {
                        err(
                            errors,
                            at.to_string(),
                            format!(
                                "Unknown CodeSystem Property '{uri}'. If you are creating your own property, do not create it in the HL7 namespace"
                            ),
                        );
                    }
                }
            }
        }
    }
    if let Some(code) = code {
        if properties.contains_key(code) {
            err(
                errors,
                at.to_string(),
                format!("A property is already defined with the code '{code}'"),
            );
        } else {
            properties.insert(
                code.to_string(),
                PropertyDef {
                    ty: ty.map(str::to_string),
                    internal_codes: false,
                },
            );
        }
    }
    if let Some((known_code, known_type)) = by_uri {
        let uri = uri.unwrap_or_default();
        if let Some(code) = code
            && known_by_code(code).is_some()
            && code != known_code
        {
            err(
                errors,
                at.to_string(),
                format!(
                    "The URI '{uri}' is normally assigned the code '{known_code}'. Using the code '{code}' will usually create confusion in ValueSet filters etc"
                ),
            );
        }
        if let Some(ty) = ty
            && Some(ty) != known_type
        {
            err(
                errors,
                at.to_string(),
                format!(
                    "Wrong type '{ty}': The URI '{uri}' identifies a property that has the type '{}'",
                    known_type.unwrap_or("null")
                ),
            );
        }
        internal_codes = matches!(known_code, "parent" | "child" | "partOf" | "synonym");
    }
    if internal_codes && let Some(code) = code {
        // The definition the concept properties are judged by is the one
        // registered under the code (the first one with that code).
        if let Some(def) = properties.get_mut(code)
            && def.ty.as_deref() == ty
        {
            def.internal_codes = true;
        }
    }
}

fn check_concept_codes(
    concepts: &[Value],
    at: &str,
    codes: &mut HashSet<String>,
    errors: &mut Vec<ValidationError>,
) {
    for (i, concept) in concepts.iter().enumerate() {
        let here = format!("{at}.{i}");
        if let Some(code) = str_of(concept, "code")
            && !codes.insert(code.to_string())
        {
            err(
                errors,
                here.clone(),
                format!("The code '{code}' has already been defined"),
            );
        }
        check_concept_codes(
            array_of(concept, "concept"),
            &format!("{here}.concept"),
            codes,
            errors,
        );
    }
}

const PRIMITIVE_TYPES: &[&str] = &[
    "base64Binary",
    "boolean",
    "canonical",
    "code",
    "date",
    "dateTime",
    "decimal",
    "id",
    "instant",
    "integer",
    "integer64",
    "markdown",
    "oid",
    "positiveInt",
    "string",
    "time",
    "unsignedInt",
    "uri",
    "url",
    "uuid",
];

/// The FHIR type of a `value[x]` key (`valueDateTime` → `dateTime`).
fn value_type(map: &Map<String, Value>) -> Option<(String, &Value)> {
    map.iter().find_map(|(k, v)| {
        let suffix = k.strip_prefix("value")?;
        let mut chars = suffix.chars();
        let first = chars.next()?;
        if !first.is_ascii_uppercase() {
            return None;
        }
        // Primitive type names start lowercase (`valueDateTime` → dateTime);
        // complex ones keep their name (`valueCoding` → Coding).
        let lowered = format!("{}{}", first.to_ascii_lowercase(), chars.as_str());
        Some(if PRIMITIVE_TYPES.contains(&lowered.as_str()) {
            (lowered, v)
        } else {
            (suffix.to_string(), v)
        })
    })
}

fn check_concept_properties(
    concepts: &[Value],
    at: &str,
    properties: &HashMap<String, PropertyDef>,
    codes: &HashSet<String>,
    is_supplement: bool,
    errors: &mut Vec<ValidationError>,
) {
    for (i, concept) in concepts.iter().enumerate() {
        let here = format!("{at}.{i}");
        for (j, property) in array_of(concept, "property").iter().enumerate() {
            let p_at = format!("{here}.property.{j}");
            let Some(code) = str_of(property, "code") else {
                continue;
            };
            let Some(map) = property.as_object() else {
                continue;
            };
            let Some((vtype, value)) = value_type(map) else {
                err(
                    errors,
                    p_at,
                    format!("The property '{code}' has no value, and cannot be understood"),
                );
                continue;
            };
            let Some(def) = properties.get(code) else {
                continue;
            };
            let Some(dtype) = def.ty.as_deref() else {
                continue;
            };
            if vtype != dtype {
                err(
                    errors,
                    p_at,
                    format!(
                        "The property '{code}' has the invalid type '{vtype}', when it is defined to have the type '{dtype}'"
                    ),
                );
                continue;
            }
            if vtype == "code"
                && def.internal_codes
                && !is_supplement
                && let Some(v) = value.as_str()
                && !codes.contains(v)
            {
                err(
                    errors,
                    p_at,
                    format!("The code '{v}' is not a valid code in this code system"),
                );
            }
        }
        check_concept_properties(
            array_of(concept, "concept"),
            &format!("{here}.concept"),
            properties,
            codes,
            is_supplement,
            errors,
        );
    }
}

fn value_set(vs: &Value, path: &str, errors: &mut Vec<ValidationError>) {
    let Some(compose) = vs.get("compose") else {
        return;
    };
    for part in ["include", "exclude"] {
        for (i, inc) in array_of(compose, part).iter().enumerate() {
            let Some(system) = str_of(inc, "system") else {
                continue;
            };
            if !is_absolute_url(system) {
                let message = if system.starts_with('#') {
                    "URI values in ValueSet.compose.include.system must be absolute. To reference a contained code system, use the full CodeSystem URL and reference it using the http://hl7.org/fhir/StructureDefinition/valueset-system extension"
                } else {
                    "URI values in ValueSet.compose.include.system must be absolute"
                };
                err(
                    errors,
                    format!("{path}.compose.{part}.{i}"),
                    message.to_string(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn messages(resource: Value) -> Vec<String> {
        resource_rule_errors(&resource)
            .into_iter()
            .map(|e| format!("{} {}", e.path, e.message))
            .collect()
    }

    #[test]
    fn absolute_urls() {
        assert!(is_absolute_url("http://example.org/x"));
        assert!(is_absolute_url(
            "urn:uuid:6a2ee390-978e-42c6-8f88-c17dff3bd8a3"
        ));
        assert!(is_absolute_url("urn:oid:1.2.3"));
        assert!(!is_absolute_url("#c1"));
        assert!(!is_absolute_url("c1"));
        assert!(!is_absolute_url("ValueSet/x"));
        assert!(!is_absolute_url("http:"));
    }

    /// `vs-canonical-bad`, `vs-canonical-urn-bad`, `vs-canonical-urn`.
    #[test]
    fn canonical_urls_and_include_systems() {
        let errs = messages(
            json!({"resourceType": "ValueSet", "url": "ValueSet/vs-canonical-bad",
            "contained": [{"resourceType": "CodeSystem", "id": "c1", "url": "c1", "content": "complete"}],
            "compose": {"include": [{"system": "#c1"}]}}),
        );
        assert_eq!(errs.len(), 3, "{errs:#?}");
        assert!(
            errs.iter()
                .any(|e| e.starts_with("ValueSet.contained.0.url Canonical URLs in contained"))
        );
        assert!(
            errs.iter()
                .any(|e| e.starts_with("ValueSet.url Canonical URLs must be absolute"))
        );
        assert!(
            errs.iter()
                .any(|e| e.starts_with("ValueSet.compose.include.0 URI values"))
        );

        let errs = messages(json!({"resourceType": "ValueSet",
            "url": "urn:uuid:6a2ee390-978e-42c6-8f88-c17dff3bd8a",
            "compose": {"include": [{"system": "http://loinc.org"}]}}));
        assert_eq!(
            errs,
            vec![
                "ValueSet.url UUIDs must be valid and lowercase (6a2ee390-978e-42c6-8f88-c17dff3bd8a)"
            ]
        );

        assert!(
            messages(json!({"resourceType": "ValueSet",
            "url": "urn:uuid:6a2ee390-978e-42c6-8f88-c17dff3bd8a3"}))
            .is_empty()
        );
        // Not a canonical resource: its url is not checked.
        assert!(messages(json!({"resourceType": "Endpoint", "url": "x"})).is_empty());
    }

    /// `cs-bad-oid`.
    #[test]
    fn rfc3986_identifier_values_and_not_present_content() {
        let errs = messages(
            json!({"resourceType": "CodeSystem", "url": "http://example.org/cs",
            "identifier": [{"system": "urn:ietf:rfc:3986", "value": "2.16.840.1.113883.6.57"}],
            "content": "not-present", "concept": [{"code": "a"}]}),
        );
        assert_eq!(errs.len(), 2, "{errs:#?}");
        assert!(errs[0].starts_with("CodeSystem.identifier.0 if identifier.system"));
        assert!(errs[1].starts_with("CodeSystem.concept.0 The code system says"));
        assert!(
            messages(json!({"resourceType": "Patient",
            "identifier": [{"system": "urn:ietf:rfc:3986", "value": "urn:oid:1.2.3"}]}))
            .is_empty()
        );
    }

    /// `cs-meta-profile` (count), `supplement-2` (content).
    #[test]
    fn count_and_supplement_content() {
        let errs = messages(
            json!({"resourceType": "CodeSystem", "url": "http://example.org/cs",
            "content": "complete", "count": 3, "concept": [{"code": "a", "concept": [{"code": "b"}]}]}),
        );
        assert_eq!(
            errs,
            vec![
                "CodeSystem.count The code system is complete, but the number of concepts (2) does not match the stated total number (3)"
            ]
        );
        let errs = messages(
            json!({"resourceType": "CodeSystem", "url": "http://example.org/s",
            "content": "complete", "supplements": "http://loinc.org"}),
        );
        assert_eq!(
            errs,
            vec![
                "CodeSystem.content CodeSystem Supplements SHALL have a content value of 'supplement'"
            ]
        );
    }

    /// `cs-bad-props`: the 7 errors the HL7 validator reports.
    #[test]
    fn property_definitions_and_values() {
        let cp = |c: &str| format!("http://hl7.org/fhir/concept-properties#{c}");
        let errs = messages(
            json!({"resourceType": "CodeSystem", "url": "http://hl7.org/fhir/test/CodeSystem/cs-bad-props",
            "content": "complete", "caseSensitive": true,
            "property": [
                {"code": "notSelectable", "uri": cp("notSelectable"), "type": "dateTime"},
                {"code": "notSelectable", "type": "boolean"},
                {"code": "notSelectable2", "uri": cp("notSelectable"), "type": "boolean"},
                {"code": "synonym", "type": "decimal"},
                {"code": "inactive", "uri": cp("deprecationDate"), "type": "code"}
            ],
            "concept": [{"code": "test", "display": "Test Code", "property": [
                {"code": "not-a-code", "valueCode": "x"},
                {"code": "notSelectable", "valueBoolean": true},
                {"code": "synonym", "valueInteger": 12}
            ]}]}),
        );
        assert_eq!(errs.len(), 7, "{errs:#?}");
        // `cs-order-prop-r4`: an unknown property in the HL7 namespace.
        let errs = messages(
            json!({"resourceType": "CodeSystem", "url": "http://hl7.org/fhir/test/CodeSystem/order",
            "content": "complete", "property": [{"code": "order", "uri": cp("order"), "type": "decimal"}]}),
        );
        assert_eq!(errs.len(), 1, "{errs:#?}");
        assert!(errs[0].contains("Unknown CodeSystem Property"), "{errs:#?}");
        // `cs-filter`: a Coding property holds valueCoding.
        let errs = messages(
            json!({"resourceType": "CodeSystem", "url": "http://example.org/cs",
            "content": "complete", "property": [{"code": "p", "type": "Coding"}],
            "concept": [{"code": "a", "property": [{"code": "p",
                "valueCoding": {"system": "http://example.org/x", "code": "y"}}]}]}),
        );
        assert!(errs.is_empty(), "{errs:#?}");
        // Internal code references.
        let errs = messages(
            json!({"resourceType": "CodeSystem", "url": "http://example.org/cs",
            "content": "complete", "property": [{"code": "parent", "uri": cp("parent"), "type": "code"}],
            "concept": [{"code": "a"}, {"code": "b", "property": [{"code": "parent", "valueCode": "zz"}]},
                        {"code": "a"}]}),
        );
        assert_eq!(errs.len(), 2, "{errs:#?}");
    }
}
