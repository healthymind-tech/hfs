//! Patient-compartment membership decided from a resource's payload.
//!
//! Bulk export reads the primary store's payload rather than the search
//! index, so it keeps working when search is offloaded to a secondary backend
//! and the local index is empty. Compartment *search*, on the other hand,
//! decides membership from the index with the parameter set of the bundled
//! `CompartmentDefinition` (`helios_fhir::get_compartment_params`). This
//! matcher gives export the same parameter set, evaluated on the payload with
//! the search-parameter extractor, so `Patient/$export` and
//! `GET /Patient/{id}/*` agree on what belongs to a patient (#1122).

use std::collections::HashSet;
use std::sync::Arc;

use helios_fhir::FhirVersion;
use parking_lot::RwLock;
use serde_json::Value;

use crate::search::{
    IndexValue, SearchParameterDefinition, SearchParameterExtractor, SearchParameterRegistry,
};
use crate::types::strip_reference_version;

/// Membership test for one resource type in the Patient compartment.
pub struct PatientCompartmentMatcher {
    resource_type: String,
    params: Vec<Arc<SearchParameterDefinition>>,
    /// Per parameter, the element paths when every one of its expression
    /// members for this type is a plain path (optionally
    /// `.where(resolve() is Patient)`): membership through it is then decided
    /// by reading those elements' `reference` strings, without FHIRPath.
    plain_paths: Vec<Option<Vec<Vec<String>>>>,
    extractor: SearchParameterExtractor,
}

impl PatientCompartmentMatcher {
    /// Resolves the type's compartment parameters against `registry`.
    ///
    /// A parameter the registry does not know (or that is not a reference
    /// parameter) contributes nothing; a type with no resolvable parameter is
    /// [`is_empty`](Self::is_empty) and never a member.
    pub fn new(
        registry: Arc<RwLock<SearchParameterRegistry>>,
        version: FhirVersion,
        resource_type: &str,
    ) -> Self {
        let codes = helios_fhir::get_compartment_params(version, "Patient", resource_type);
        let params: Vec<Arc<SearchParameterDefinition>> = {
            let registry = registry.read();
            codes
                .iter()
                .filter_map(|code| registry.get_param(resource_type, code))
                .filter(|param| param.param_type == crate::types::SearchParamType::Reference)
                .collect()
        };
        let plain_paths = params
            .iter()
            .map(|param| plain_member_paths(&param.expression, resource_type))
            .collect();
        Self {
            resource_type: resource_type.to_string(),
            params,
            plain_paths,
            extractor: SearchParameterExtractor::new(registry),
        }
    }

    /// True when no compartment parameter resolved for the type.
    pub fn is_empty(&self) -> bool {
        self.params.is_empty()
    }

    /// The parameter codes membership is decided on, in compartment order.
    /// Membership decided by FHIRPath alone, for checking the plain-path
    /// shortcut against it.
    #[cfg(test)]
    fn is_member_by_fhirpath(&self, resource: &Value, patient_refs: &HashSet<String>) -> bool {
        self.params.iter().any(|param| {
            self.extractor
                .extract_for_param(resource, param)
                .ok()
                .into_iter()
                .flatten()
                .any(|extracted| match &extracted.value {
                    IndexValue::Reference { reference, .. } => {
                        patient_refs.contains(strip_reference_version(reference))
                    }
                    _ => false,
                })
        })
    }

    pub fn param_codes(&self) -> impl Iterator<Item = &str> {
        self.params.iter().map(|param| param.code.as_str())
    }

    /// Whether `resource` references one of `patient_refs` (`Patient/{id}`)
    /// through any compartment parameter. A versioned reference
    /// (`Patient/{id}/_history/{vid}`) counts as its base.
    pub fn is_member(&self, resource: &Value, patient_refs: &HashSet<String>) -> bool {
        self.params
            .iter()
            .zip(&self.plain_paths)
            .any(|(param, plain)| {
                // A plain-path parameter is answered from the payload directly:
                // exact for relative references, and the export paths call this
                // once per candidate row, where FHIRPath was most of the cost.
                // An absolute or `urn:` reference at the path is left to FHIRPath.
                if let Some(paths) = plain {
                    if let Some(answer) = plain_paths_member(resource, paths, patient_refs) {
                        return answer;
                    }
                }
                self.extractor
                    .extract_for_param(resource, param)
                    .ok()
                    .into_iter()
                    .flatten()
                    .any(|extracted| match &extracted.value {
                        IndexValue::Reference { reference, .. } => {
                            patient_refs.contains(strip_reference_version(reference))
                        }
                        _ => false,
                    })
            })
    }

    /// The element paths under the resource root the compartment parameters
    /// read, for stores that filter on the payload with a path expression
    /// (dotted, e.g. `link.other` for `Patient.link.other`).
    ///
    /// Each union member of a parameter's FHIRPath expression that starts with
    /// the resource type is reduced to its leading element path; anything
    /// after a function call or a type operator is dropped, so the paths are a
    /// superset filter — a candidate found through them still has to pass
    /// [`is_member`](Self::is_member). Returns `None` when some parameter
    /// yields no usable path, in which case the caller cannot prefilter and
    /// must test every resource of the type.
    pub fn payload_paths(&self) -> Option<Vec<String>> {
        let mut paths = Vec::new();
        for param in &self.params {
            let mut found = false;
            for member in param.expression.split('|') {
                if let Some(path) = leading_element_path(member, &self.resource_type) {
                    found = true;
                    if !paths.contains(&path) {
                        paths.push(path);
                    }
                }
            }
            if !found {
                return None;
            }
        }
        Some(paths)
    }
}

/// The element paths of `expression`'s members for `resource_type`, when
/// every such member is `Type.a.b` or `Type.a.b.where(resolve() is Patient)`
/// (the `where` only narrows to Patient targets, which is all a patient
/// reference set can match anyway). Anything else — `as`, `ofType`,
/// `extension(...)`, other filters — returns `None` and stays on FHIRPath.
fn plain_member_paths(expression: &str, resource_type: &str) -> Option<Vec<Vec<String>>> {
    let mut paths = Vec::new();
    for member in expression.split('|') {
        let member = member.trim();
        let Some(rest) = member
            .strip_prefix(resource_type)
            .and_then(|r| r.strip_prefix('.'))
        else {
            // A member for another type (shared multi-type parameters), or a
            // parenthesized form: only the former is safe to skip.
            if member.starts_with('(') && member.contains(resource_type) {
                return None;
            }
            continue;
        };
        let path = rest
            .strip_suffix(".where(resolve() is Patient)")
            .unwrap_or(rest);
        if path.is_empty()
            || !path.split('.').all(|seg| {
                !seg.is_empty() && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
        {
            return None;
        }
        paths.push(path.split('.').map(str::to_string).collect());
    }
    if paths.is_empty() { None } else { Some(paths) }
}

/// `Some(true)` when a relative reference at one of `paths` names a patient
/// in `patient_refs`; `Some(false)` when every reference there is relative and
/// none does; `None` when one is absolute or a URN (the caller falls back).
fn plain_paths_member(
    resource: &Value,
    paths: &[Vec<String>],
    patient_refs: &HashSet<String>,
) -> Option<bool> {
    fn walk<'a>(value: &'a Value, path: &[String], out: &mut Vec<&'a Value>) {
        match value {
            Value::Array(items) => items.iter().for_each(|v| walk(v, path, out)),
            _ if path.is_empty() => out.push(value),
            Value::Object(map) => {
                if let Some(next) = map.get(&path[0]) {
                    walk(next, &path[1..], out);
                }
            }
            _ => {}
        }
    }
    let mut undecided = false;
    for path in paths {
        let mut found = Vec::new();
        walk(resource, path, &mut found);
        for value in found {
            let Some(reference) = value.get("reference").and_then(Value::as_str) else {
                continue;
            };
            if reference.contains("://") || reference.starts_with("urn:") {
                undecided = true;
                continue;
            }
            if patient_refs.contains(strip_reference_version(reference)) {
                return Some(true);
            }
        }
    }
    if undecided { None } else { Some(false) }
}

/// `Observation.subject.where(resolve() is Patient)` → `subject`;
/// `(Patient.link.other as Reference)` → `link.other`; a member rooted in
/// another type, or one with no element after the type, → `None`.
fn leading_element_path(member: &str, resource_type: &str) -> Option<String> {
    let member = member.trim().trim_start_matches('(');
    let rest = member.strip_prefix(resource_type)?.strip_prefix('.')?;
    let mut segments: Vec<&str> = Vec::new();
    for segment in rest.split('.') {
        let ident_end = segment
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(segment.len());
        let ident = &segment[..ident_end];
        // A segment that opens a call is a function, not an element: stop
        // before it. Any other cut (a space, a paren) ends the path after it.
        if segment[ident_end..].starts_with('(') {
            break;
        }
        if !ident.is_empty() {
            segments.push(ident);
        }
        if ident_end < segment.len() {
            break;
        }
    }
    if segments.is_empty() {
        return None;
    }
    Some(segments.join("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::SearchParameterLoader;
    use serde_json::json;

    /// The embedded fallback plus the spec file under the workspace `data/`
    /// directory, the same two sources a backend loads on startup.
    fn registry() -> Arc<RwLock<SearchParameterRegistry>> {
        let loader = SearchParameterLoader::new(FhirVersion::default());
        let mut registry = SearchParameterRegistry::new();
        for param in loader.load_embedded().unwrap() {
            let _ = registry.register(param);
        }
        let data_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data");
        for param in loader.load_from_spec_file(&data_dir).unwrap() {
            let _ = registry.register(param);
        }
        Arc::new(RwLock::new(registry))
    }

    fn refs(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| format!("Patient/{id}")).collect()
    }

    #[test]
    fn a_type_outside_the_compartment_has_no_parameters() {
        let matcher =
            PatientCompartmentMatcher::new(registry(), FhirVersion::default(), "Organization");
        assert!(matcher.is_empty());
        assert!(!matcher.is_member(&json!({"resourceType": "Organization"}), &refs(&["p1"])));
    }

    #[test]
    fn membership_follows_every_compartment_parameter_not_only_subject() {
        let matcher = PatientCompartmentMatcher::new(
            registry(),
            FhirVersion::default(),
            "AllergyIntolerance",
        );
        let codes: Vec<&str> = matcher.param_codes().collect();
        assert_eq!(codes, ["patient", "recorder", "asserter"]);
        let recorded = json!({
            "resourceType": "AllergyIntolerance",
            "patient": {"reference": "Patient/other"},
            "recorder": {"reference": "Patient/p1"}
        });
        assert!(matcher.is_member(&recorded, &refs(&["p1"])));
        assert!(!matcher.is_member(&recorded, &refs(&["p2"])));
    }

    #[test]
    fn a_versioned_reference_counts_as_its_base() {
        let matcher =
            PatientCompartmentMatcher::new(registry(), FhirVersion::default(), "Observation");
        let observation = json!({
            "resourceType": "Observation",
            "status": "final",
            "code": {"text": "x"},
            "performer": [{"reference": "Patient/p1/_history/3"}]
        });
        assert!(matcher.is_member(&observation, &refs(&["p1"])));
    }

    #[test]
    fn a_patient_belongs_through_link() {
        let matcher = PatientCompartmentMatcher::new(registry(), FhirVersion::default(), "Patient");
        let linked = json!({
            "resourceType": "Patient",
            "id": "p9",
            "link": [{"other": {"reference": "Patient/p1"}, "type": "seealso"}]
        });
        assert!(matcher.is_member(&linked, &refs(&["p1"])));
        assert!(!matcher.is_member(
            &json!({"resourceType": "Patient", "id": "p1"}),
            &refs(&["p1"])
        ));
    }

    /// The common clinical types are answered from plain paths, and the
    /// shortcut agrees with FHIRPath on every shape of reference.
    #[test]
    fn plain_paths_agree_with_fhirpath() {
        let reg = registry();
        let obs =
            PatientCompartmentMatcher::new(reg.clone(), FhirVersion::default(), "Observation");
        assert!(
            obs.plain_paths.iter().all(Option::is_some),
            "Observation compartment params should all be plain paths: {:?}",
            obs.plain_paths
        );
        let cases = [
            (
                "Observation",
                json!({"resourceType": "Observation", "status": "final", "code": {"text": "x"},
                "subject": {"reference": "Patient/p1"}}),
            ),
            (
                "Observation",
                json!({"resourceType": "Observation", "status": "final", "code": {"text": "x"},
                "subject": {"reference": "Patient/p2"}, "performer": [{"reference": "Practitioner/d"}, {"reference": "Patient/p1/_history/2"}]}),
            ),
            (
                "Observation",
                json!({"resourceType": "Observation", "status": "final", "code": {"text": "x"},
                "subject": {"reference": "Patient/p2"}, "focus": [{"reference": "Patient/p1"}]}),
            ),
            (
                "Observation",
                json!({"resourceType": "Observation", "status": "final", "code": {"text": "x"},
                "subject": {"reference": "Group/g1"}}),
            ),
            (
                "Observation",
                json!({"resourceType": "Observation", "status": "final", "code": {"text": "x"},
                "subject": {"reference": "http://elsewhere.example/fhir/Patient/p1"}}),
            ),
            (
                "Observation",
                json!({"resourceType": "Observation", "status": "final", "code": {"text": "x"},
                "contained": [{"resourceType": "Patient", "id": "c"}], "subject": {"reference": "#c"}}),
            ),
            (
                "Encounter",
                json!({"resourceType": "Encounter", "status": "finished", "class": {"code": "AMB"},
                "subject": {"reference": "Patient/p2"}, "participant": [{"individual": {"reference": "Practitioner/d"}}]}),
            ),
            (
                "Encounter",
                json!({"resourceType": "Encounter", "status": "finished", "class": {"code": "AMB"},
                "subject": {"reference": "Patient/p1"}}),
            ),
            (
                "AllergyIntolerance",
                json!({"resourceType": "AllergyIntolerance",
                "patient": {"reference": "Patient/other"}, "recorder": {"reference": "Patient/p1"}}),
            ),
            (
                "ExplanationOfBenefit",
                json!({"resourceType": "ExplanationOfBenefit", "status": "active",
                "patient": {"reference": "Patient/p1"}, "payee": {"party": {"reference": "Organization/o"}}}),
            ),
            (
                "Condition",
                json!({"resourceType": "Condition", "subject": {"reference": "Patient/p3"},
                "asserter": {"reference": "Patient/p1"}}),
            ),
            (
                "Patient",
                json!({"resourceType": "Patient", "id": "p9",
                "link": [{"other": {"reference": "Patient/p1"}, "type": "seealso"}]}),
            ),
        ];
        let wanted = refs(&["p1"]);
        for (rt, resource) in cases {
            let matcher = PatientCompartmentMatcher::new(reg.clone(), FhirVersion::default(), rt);
            assert_eq!(
                matcher.is_member(&resource, &wanted),
                matcher.is_member_by_fhirpath(&resource, &wanted),
                "{rt}: {resource}"
            );
        }
    }

    #[test]
    fn plain_member_paths_accept_only_paths_and_patient_filters() {
        assert_eq!(
            plain_member_paths(
                "AllergyIntolerance.patient | Observation.subject.where(resolve() is Patient) | Observation.performer",
                "Observation"
            ),
            Some(vec![
                vec!["subject".to_string()],
                vec!["performer".to_string()]
            ])
        );
        assert_eq!(
            plain_member_paths("Encounter.participant.individual", "Encounter"),
            Some(vec![vec![
                "participant".to_string(),
                "individual".to_string()
            ]])
        );
        assert_eq!(
            plain_member_paths("(Patient.link.other as Reference)", "Patient"),
            None
        );
        assert_eq!(
            plain_member_paths(
                "Observation.subject.where(resolve() is Group)",
                "Observation"
            ),
            None
        );
        assert_eq!(plain_member_paths("Claim.patient", "ClaimResponse"), None);
    }

    #[test]
    fn payload_paths_reduce_each_expression_to_its_element_path() {
        let matcher =
            PatientCompartmentMatcher::new(registry(), FhirVersion::default(), "Observation");
        let paths = matcher.payload_paths().unwrap();
        assert!(paths.contains(&"subject".to_string()), "{paths:?}");
        assert!(paths.contains(&"performer".to_string()), "{paths:?}");
        let patient = PatientCompartmentMatcher::new(registry(), FhirVersion::default(), "Patient");
        assert_eq!(patient.payload_paths().unwrap(), ["link.other"]);
    }

    #[test]
    fn leading_path_stops_at_functions_and_operators() {
        assert_eq!(
            leading_element_path(
                " Observation.subject.where(resolve() is Patient) ",
                "Observation"
            )
            .as_deref(),
            Some("subject")
        );
        assert_eq!(
            leading_element_path("(Patient.link.other as Reference)", "Patient").as_deref(),
            Some("link.other")
        );
        assert_eq!(
            leading_element_path("Condition.subject", "Observation"),
            None
        );
        assert_eq!(leading_element_path("Observation", "Observation"), None);
    }
}
