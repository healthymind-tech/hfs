//! # FHIRPath `resolve()` Function
//!
//! Implements the FHIR-flavored FHIRPath `resolve()` function, which dereferences
//! a `Reference` (or a reference/canonical string) to the resource it points to.
//!
//! Resolution is **in-scope only** — it searches resources already available to the
//! evaluation context, with no external/database I/O:
//!
//! 1. `contained` resources of the resources in [`EvaluationContext::resources`]
//!    (matched by fragment `#id` or bare `id`).
//! 2. The context resources themselves and their contained resources, matched by
//!    `ResourceType/id` (relative) or by the trailing `Type/id` of an absolute URL.
//!
//! When a `Type/id`-shaped (or absolute-URL-with-`Type/id`) reference cannot be
//! resolved, a *typed stub* object `{ resourceType: Type }` is returned so that
//! type filters such as `resolve() is Observation` still behave correctly. Bare or
//! fragment references that cannot be resolved produce [`EvaluationResult::Empty`].

use crate::evaluator::EvaluationContext;
use helios_fhirpath_support::{
    EvaluationError, EvaluationResult, IntoEvaluationResult, TypeInfoResult,
};
use std::collections::HashMap;

/// Implementation of the FHIRPath `resolve()` function.
///
/// Operates on the invocation base, which may be a single `Reference`/string or a
/// collection of them. Each item is resolved independently and the results are
/// flattened, mirroring FHIRPath collection semantics.
pub fn resolve_function(
    invocation_base: &EvaluationResult,
    context: &EvaluationContext,
) -> Result<EvaluationResult, EvaluationError> {
    // Build the candidate pool once and reuse it for every reference being resolved.
    let candidates = build_candidate_pool(context);

    match invocation_base {
        EvaluationResult::Empty => Ok(EvaluationResult::Empty),
        EvaluationResult::Collection { items, .. } => {
            let mut resolved = Vec::new();
            for item in items {
                match resolve_one(item, &candidates) {
                    EvaluationResult::Empty => {}
                    other => resolved.push(other),
                }
            }
            match resolved.len() {
                0 => Ok(EvaluationResult::Empty),
                1 => Ok(resolved.into_iter().next().unwrap()),
                _ => Ok(EvaluationResult::Collection {
                    items: resolved,
                    has_undefined_order: false,
                    type_info: None,
                }),
            }
        }
        single => Ok(resolve_one(single, &candidates)),
    }
}

/// A single candidate resource available for resolution.
struct Candidate {
    resource_type: Option<String>,
    id: Option<String>,
    /// Whether this candidate came from a `contained` array (eligible for
    /// fragment / bare-id matching).
    is_contained: bool,
    /// `Bundle.entry.fullUrl` when the candidate is a Bundle entry's resource
    /// (`urn:uuid:…` and absolute references match it literally).
    full_url: Option<String>,
    value: EvaluationResult,
}

/// Flattens the context resources and their `contained` children into a pool of
/// candidates that can be matched against a reference.
///
/// `context.resources` is shared via `Arc` and survives cloning, so lambda bodies
/// (`where()`, `select()`, …) — which run in child contexts — still see the root
/// resources here. Without that sharing, `reference.where(resolve() is X)` would
/// resolve to nothing.
fn build_candidate_pool(context: &EvaluationContext) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for resource in context.resources.iter() {
        let result = resource.to_evaluation_result();
        collect_candidates(&result, false, None, &mut candidates);
    }
    candidates
}

/// Adds `value` (if it is a resource object), its `contained` children and —
/// for a Bundle — its entries' resources to the pool. Bundle entries are how
/// document/message invariants reach their siblings
/// (`Composition.author.resolve()` in a document Bundle).
fn collect_candidates(
    value: &EvaluationResult,
    is_contained: bool,
    full_url: Option<String>,
    out: &mut Vec<Candidate>,
) {
    if let EvaluationResult::Object { map, .. } = value {
        let resource_type = string_field(map, "resourceType");
        let id = string_field(map, "id");
        // Only treat objects that look like resources (have a resourceType) as candidates.
        if resource_type.is_some() {
            out.push(Candidate {
                resource_type: resource_type.clone(),
                id: id.clone(),
                is_contained,
                full_url,
                value: value.clone(),
            });
        }

        if resource_type.as_deref() == Some("Bundle") {
            let entries = match map.get("entry") {
                Some(EvaluationResult::Collection { items, .. }) => items.iter().collect(),
                Some(entry @ EvaluationResult::Object { .. }) => vec![entry],
                _ => Vec::new(),
            };
            for entry in entries {
                if let EvaluationResult::Object { map: entry, .. } = entry
                    && let Some(resource) = entry.get("resource")
                {
                    collect_candidates(resource, false, string_field(entry, "fullUrl"), out);
                }
            }
        }

        // Recurse into contained resources (one level is all FHIR allows, but
        // iterating defensively is cheap).
        if let Some(contained) = map.get("contained") {
            match contained {
                EvaluationResult::Collection { items, .. } => {
                    for item in items {
                        collect_candidates(item, true, None, out);
                    }
                }
                obj @ EvaluationResult::Object { .. } => {
                    collect_candidates(obj, true, None, out);
                }
                _ => {}
            }
        }
    }
}

/// Resolves a single reference item against the candidate pool.
fn resolve_one(item: &EvaluationResult, candidates: &[Candidate]) -> EvaluationResult {
    let Some(reference) = extract_reference_string(item) else {
        return EvaluationResult::Empty;
    };

    // A Bundle entry whose fullUrl is the reference (urn:uuid:, absolute URL).
    if let Some(found) = candidates
        .iter()
        .find(|c| c.full_url.as_deref() == Some(reference.as_str()))
    {
        return found.value.clone();
    }

    let parsed = ParsedReference::parse(&reference);

    if let Some(found) = parsed.find(candidates) {
        return found.clone();
    }

    // Unresolved fallback: a typed stub for Type/id-shaped references so that
    // `resolve() is Type` continues to work.
    if let Some(resource_type) = parsed.target_type() {
        return typed_stub(resource_type);
    }

    EvaluationResult::Empty
}

/// Extracts the reference string from a `Reference` object (its `reference` field)
/// or from a bare string (canonical / already-a-reference-string).
fn extract_reference_string(item: &EvaluationResult) -> Option<String> {
    match item {
        EvaluationResult::String(s, _, _) => Some(s.clone()),
        EvaluationResult::Object { map, .. } => match map.get("reference") {
            Some(EvaluationResult::String(s, _, _)) => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// A parsed FHIR reference, classified by form.
enum ParsedReference {
    /// `#id` fragment or bare `id` (no slash) — resolves only against contained resources.
    Fragment(String),
    /// `ResourceType/id` (relative) or an absolute URL whose tail is `Type/id`.
    Typed { resource_type: String, id: String },
    /// Anything else we can't classify (e.g. a `urn:uuid:` with no type) — best-effort
    /// match by trailing id only.
    Other(String),
}

impl ParsedReference {
    fn parse(reference: &str) -> ParsedReference {
        if let Some(fragment) = reference.strip_prefix('#') {
            return ParsedReference::Fragment(fragment.to_string());
        }

        // Find the last `Type/id` pair. For absolute URLs this naturally takes the
        // final two path segments (e.g. ".../Patient/123").
        if let Some((resource_type, id)) = split_type_id(reference) {
            return ParsedReference::Typed { resource_type, id };
        }

        // No slash at all → a bare local id (contained reference, as in the FHIR
        // patient-container example where `reference` is just "1").
        if !reference.contains('/') {
            return ParsedReference::Fragment(reference.to_string());
        }

        ParsedReference::Other(reference.to_string())
    }

    /// The resource type implied by this reference, if any (used for the typed stub).
    fn target_type(&self) -> Option<&str> {
        match self {
            ParsedReference::Typed { resource_type, .. } => Some(resource_type),
            _ => None,
        }
    }

    /// Finds the matching candidate, if any.
    fn find<'a>(&self, candidates: &'a [Candidate]) -> Option<&'a EvaluationResult> {
        match self {
            ParsedReference::Fragment(id) => candidates
                .iter()
                .find(|c| c.is_contained && c.id.as_deref() == Some(id.as_str()))
                .map(|c| &c.value),
            ParsedReference::Typed { resource_type, id } => candidates
                .iter()
                .find(|c| {
                    c.resource_type.as_deref() == Some(resource_type.as_str())
                        && c.id.as_deref() == Some(id.as_str())
                })
                .map(|c| &c.value),
            ParsedReference::Other(reference) => {
                // Best-effort: match a candidate whose id is the trailing segment.
                let tail = reference.rsplit('/').next().unwrap_or(reference);
                candidates
                    .iter()
                    .find(|c| c.id.as_deref() == Some(tail))
                    .map(|c| &c.value)
            }
        }
    }
}

/// Splits a `ResourceType/id` reference (taking the final two segments of a path)
/// into `(ResourceType, id)`. Returns `None` unless the type segment looks like a
/// FHIR resource type (starts uppercase).
fn split_type_id(reference: &str) -> Option<(String, String)> {
    let mut segments = reference.rsplitn(3, '/');
    let id = segments.next()?;
    let resource_type = segments.next()?;
    if id.is_empty() || resource_type.is_empty() {
        return None;
    }
    // A FHIR resource type is a capitalized token without scheme separators.
    if !resource_type
        .chars()
        .next()
        .map(|c| c.is_ascii_uppercase())
        .unwrap_or(false)
    {
        return None;
    }
    Some((resource_type.to_string(), id.to_string()))
}

/// Builds a typed stub object `{ resourceType: Type }` carrying FHIR type info so
/// that `resolve() is Type` matches.
fn typed_stub(resource_type: &str) -> EvaluationResult {
    let mut map = HashMap::new();
    map.insert(
        "resourceType".to_string(),
        EvaluationResult::String(resource_type.to_string(), None, None),
    );
    EvaluationResult::Object {
        map,
        type_info: Some(TypeInfoResult::new("FHIR", resource_type)),
    }
}

/// Reads a string field from an object map.
fn string_field(map: &HashMap<String, EvaluationResult>, key: &str) -> Option<String> {
    match map.get(key) {
        Some(EvaluationResult::String(s, _, _)) => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(s: &str) -> EvaluationResult {
        EvaluationResult::String(s.to_string(), None, None)
    }

    fn reference(r: &str) -> EvaluationResult {
        let mut map = HashMap::new();
        map.insert("reference".to_string(), string(r));
        EvaluationResult::Object {
            map,
            type_info: None,
        }
    }

    fn obj(pairs: &[(&str, EvaluationResult)]) -> EvaluationResult {
        let map = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        EvaluationResult::Object {
            map,
            type_info: None,
        }
    }

    /// Builds a candidate pool directly from EvaluationResult objects (bypassing
    /// FhirResource conversion) so the matching logic can be unit-tested in isolation.
    fn pool(roots: &[EvaluationResult]) -> Vec<Candidate> {
        let mut out = Vec::new();
        for root in roots {
            collect_candidates(root, false, None, &mut out);
        }
        out
    }

    #[test]
    fn resolves_bare_id_against_contained() {
        // Patient with a contained Organization/1, referenced by bare "1".
        let patient = obj(&[
            ("resourceType", string("Patient")),
            ("id", string("example-container")),
            (
                "contained",
                EvaluationResult::Collection {
                    items: vec![obj(&[
                        ("resourceType", string("Organization")),
                        ("id", string("1")),
                    ])],
                    has_undefined_order: false,
                    type_info: None,
                },
            ),
        ]);
        let candidates = pool(&[patient]);
        let resolved = resolve_one(&reference("1"), &candidates);
        match resolved {
            EvaluationResult::Object { map, .. } => {
                assert_eq!(
                    string_field(&map, "resourceType").as_deref(),
                    Some("Organization")
                );
                assert_eq!(string_field(&map, "id").as_deref(), Some("1"));
            }
            other => panic!("expected Organization object, got {other:?}"),
        }
    }

    fn collection(items: Vec<EvaluationResult>) -> EvaluationResult {
        EvaluationResult::Collection {
            items,
            has_undefined_order: false,
            type_info: None,
        }
    }

    fn id_of(resolved: &EvaluationResult) -> Option<String> {
        match resolved {
            EvaluationResult::Object { map, .. } => string_field(map, "id"),
            _ => None,
        }
    }

    /// Bundle entries are resolvable: by relative `Type/id`, by `fullUrl`
    /// (`urn:uuid:` and absolute), and an entry's own contained resources stay
    /// fragment-only. (The NHI EMPD document invariant `empd-medreq-1` reads
    /// `Composition.author.resolve().ofType(Practitioner).identifier`.)
    #[test]
    fn resolves_bundle_entries() {
        let practitioner = obj(&[
            ("resourceType", string("Practitioner")),
            ("id", string("pra-02")),
            (
                "identifier",
                collection(vec![obj(&[("value", string("DC001"))])]),
            ),
        ]);
        let patient = obj(&[
            ("resourceType", string("Patient")),
            ("id", string("p1")),
            (
                "contained",
                obj(&[
                    ("resourceType", string("Organization")),
                    ("id", string("o1")),
                ]),
            ),
        ]);
        let bundle = obj(&[
            ("resourceType", string("Bundle")),
            (
                "entry",
                collection(vec![
                    obj(&[
                        (
                            "fullUrl",
                            string("https://example.org/fhir/Practitioner/pra-02"),
                        ),
                        ("resource", practitioner),
                    ]),
                    obj(&[
                        (
                            "fullUrl",
                            string("urn:uuid:2b90dd2b-2dab-4c75-9bb9-a355e07401e8"),
                        ),
                        ("resource", patient),
                    ]),
                ]),
            ),
        ]);
        let candidates = pool(&[bundle]);
        let resolved = resolve_one(&reference("Practitioner/pra-02"), &candidates);
        assert_eq!(id_of(&resolved).as_deref(), Some("pra-02"));
        match &resolved {
            EvaluationResult::Object { map, .. } => {
                assert!(
                    map.contains_key("identifier"),
                    "the real entry, not a typed stub"
                )
            }
            other => panic!("expected Practitioner, got {other:?}"),
        }
        let resolved = resolve_one(
            &reference("urn:uuid:2b90dd2b-2dab-4c75-9bb9-a355e07401e8"),
            &candidates,
        );
        assert_eq!(id_of(&resolved).as_deref(), Some("p1"));
        let resolved = resolve_one(
            &reference("https://example.org/fhir/Practitioner/pra-02"),
            &candidates,
        );
        assert_eq!(id_of(&resolved).as_deref(), Some("pra-02"));
        assert_eq!(
            id_of(&resolve_one(&reference("#o1"), &candidates)).as_deref(),
            Some("o1")
        );
        assert_eq!(
            id_of(&resolve_one(&reference("p1"), &candidates)),
            None,
            "a bare id never matches a Bundle entry (only contained resources)"
        );
    }

    #[test]
    fn resolves_fragment_against_contained() {
        let patient = obj(&[
            ("resourceType", string("Patient")),
            ("id", string("p")),
            (
                "contained",
                obj(&[
                    ("resourceType", string("Practitioner")),
                    ("id", string("pr1")),
                ]),
            ),
        ]);
        let candidates = pool(&[patient]);
        let resolved = resolve_one(&reference("#pr1"), &candidates);
        assert!(matches!(resolved, EvaluationResult::Object { .. }));
    }

    #[test]
    fn resolves_relative_reference_in_context() {
        let observation = obj(&[
            ("resourceType", string("Observation")),
            ("id", string("obs-1")),
        ]);
        let candidates = pool(&[observation]);
        let resolved = resolve_one(&reference("Observation/obs-1"), &candidates);
        match resolved {
            EvaluationResult::Object { map, .. } => {
                assert_eq!(string_field(&map, "id").as_deref(), Some("obs-1"));
            }
            other => panic!("expected Observation object, got {other:?}"),
        }
    }

    #[test]
    fn resolves_absolute_url_by_trailing_type_id() {
        let observation = obj(&[
            ("resourceType", string("Observation")),
            ("id", string("obs-1")),
        ]);
        let candidates = pool(&[observation]);
        let resolved = resolve_one(
            &reference("http://example.org/fhir/Observation/obs-1"),
            &candidates,
        );
        assert!(matches!(resolved, EvaluationResult::Object { .. }));
    }

    #[test]
    fn typed_stub_when_unresolved() {
        let candidates: Vec<Candidate> = Vec::new();
        let resolved = resolve_one(&reference("Observation/missing"), &candidates);
        match &resolved {
            EvaluationResult::Object { map, type_info } => {
                assert_eq!(
                    string_field(map, "resourceType").as_deref(),
                    Some("Observation")
                );
                assert_eq!(
                    type_info.as_ref().map(|t| t.name.as_str()),
                    Some("Observation")
                );
            }
            other => panic!("expected typed stub, got {other:?}"),
        }
        // The stub carries FHIR/Observation type info and a resourceType field, both
        // of which `is_of_type_with_context` consults — so `resolve() is Observation`
        // is satisfied (verified end-to-end via the CLI integration check).
    }

    #[test]
    fn unresolvable_bare_reference_is_empty() {
        let candidates: Vec<Candidate> = Vec::new();
        let resolved = resolve_one(&reference("nope"), &candidates);
        assert!(matches!(resolved, EvaluationResult::Empty));
    }

    #[test]
    fn resolves_collection_of_references() {
        let patient = obj(&[
            ("resourceType", string("Patient")),
            ("id", string("p")),
            (
                "contained",
                EvaluationResult::Collection {
                    items: vec![
                        obj(&[
                            ("resourceType", string("Organization")),
                            ("id", string("1")),
                        ]),
                        obj(&[
                            ("resourceType", string("Organization")),
                            ("id", string("2")),
                        ]),
                    ],
                    has_undefined_order: false,
                    type_info: None,
                },
            ),
        ]);
        let candidates = pool(&[patient]);
        // Map resolve over each reference (the same flattening resolve_function performs).
        let out: Vec<EvaluationResult> = [reference("1"), reference("2")]
            .iter()
            .map(|r| resolve_one(r, &candidates))
            .collect();
        assert_eq!(out.len(), 2);
        assert!(
            out.iter()
                .all(|r| matches!(r, EvaluationResult::Object { .. }))
        );
        assert_eq!(
            out.iter()
                .filter_map(|r| match r {
                    EvaluationResult::Object { map, .. } => string_field(map, "id"),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["1".to_string(), "2".to_string()]
        );
    }
}
