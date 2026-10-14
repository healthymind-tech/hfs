//! End-to-end FHIRPath invariant evaluation (feature `fhirpath`): real
//! spec constraints from the embedded R4 pack, evaluated by helios-fhirpath
//! through the deferred-effects pipeline.
//!
//! Run with: `cargo test -p helios-fhir-validator --features fhirpath`
//!
//! ## MSVC 14.44 linker caveat
//!
//! This binary links helios-fhirpath's full dependency tree and can trip the
//! MSVC 14.44 (`link.exe` 14.44.35207) `LNK1318: Unexpected PDB error;
//! LIMIT (12)` defect on Windows. If that happens, the same scenarios are
//! covered end-to-end by `crates/rest/tests/validate_operation_tests.rs`
//! (`validate_evaluates_real_fhirpath_invariants`), whose test binary links
//! fine; alternatively add `-C link-arg=/PDBPAGESIZE:8192` to the MSVC
//! target rustflags or use a non-affected MSVC toolchain.

#![cfg(all(feature = "fhirpath", feature = "R4"))]

use helios_fhir::FhirVersion;
use helios_fhir_validator::fhirpath_effects::FhirPathConstraintEvaluator;
use helios_fhir_validator::packs::core_registry;
use helios_fhir_validator::{EffectHandlers, ErrorKind, ValidationOptions, Validator};
use serde_json::json;

fn handlers(evaluator: &FhirPathConstraintEvaluator) -> EffectHandlers<'_> {
    EffectHandlers {
        constraints: Some(evaluator),
        ..Default::default()
    }
}

/// dom-6 (narrative present) is a warning-severity invariant that fires on
/// almost every test resource; suppress it like the server default does.
fn suppress() -> Vec<String> {
    vec!["dom-6".to_string()]
}

#[tokio::test]
async fn real_pat1_invariant_fires_on_detail_less_contact() {
    let validator = Validator::new(core_registry(FhirVersion::R4));
    let evaluator = FhirPathConstraintEvaluator::new();
    let suppress = suppress();
    let mut h = handlers(&evaluator);
    h.suppress_constraints = &suppress;

    // pat-1: "SHALL at least contain a contact's details or a reference to
    // an organization" — a contact with only a gender violates it.
    let violating = json!({
        "resourceType": "Patient",
        "contact": [{ "gender": "male" }]
    });
    let errors = validator
        .validate(
            &violating,
            FhirVersion::R4,
            &ValidationOptions::default(),
            &h,
        )
        .await;
    let pat1: Vec<_> = errors
        .iter()
        .filter(|e| e.kind == ErrorKind::FhirpathConstraint && e.path == "Patient.contact.0")
        .collect();
    assert_eq!(
        pat1.len(),
        1,
        "pat-1 must fire exactly once at the contact node; all issues: {}",
        serde_json::to_string_pretty(&errors).unwrap()
    );
    assert_eq!(
        pat1[0].extra.get("constraint"),
        Some(&json!("pat-1")),
        "constraint id carried in extras"
    );

    let satisfied = json!({
        "resourceType": "Patient",
        "contact": [{ "gender": "male", "name": { "family": "Smith" } }]
    });
    let errors = validator
        .validate(
            &satisfied,
            FhirVersion::R4,
            &ValidationOptions::default(),
            &h,
        )
        .await;
    assert!(
        !errors
            .iter()
            .any(|e| e.kind == ErrorKind::FhirpathConstraint
                && e.extra.get("constraint") == Some(&json!("pat-1"))),
        "pat-1 must pass when the contact has a name; got: {}",
        serde_json::to_string_pretty(&errors).unwrap()
    );
}

#[tokio::test]
async fn absent_nodes_pass_invariants() {
    // FHIR invariant semantics: a constraint over an absent element is
    // vacuously true (empty → pass).
    let validator = Validator::new(core_registry(FhirVersion::R4));
    let evaluator = FhirPathConstraintEvaluator::new();
    let suppress = suppress();
    let mut h = handlers(&evaluator);
    h.suppress_constraints = &suppress;

    let minimal = json!({ "resourceType": "Patient" });
    let errors = validator
        .validate(&minimal, FhirVersion::R4, &ValidationOptions::default(), &h)
        .await;
    let hard_failures: Vec<_> = errors
        .iter()
        .filter(|e| e.severity == helios_fhir_validator::Severity::Error)
        .collect();
    assert!(
        hard_failures.is_empty(),
        "a minimal Patient must have no error-severity issues, got: {}",
        serde_json::to_string_pretty(&errors).unwrap()
    );
}

/// Constraint ids reported for the narrative at `Patient.text.div`.
async fn narrative_issues(div: &str) -> Vec<(String, helios_fhir_validator::Severity)> {
    let validator = Validator::new(core_registry(FhirVersion::R4));
    let evaluator = FhirPathConstraintEvaluator::new();
    let suppress = suppress();
    let mut h = handlers(&evaluator);
    h.suppress_constraints = &suppress;
    let patient = json!({
        "resourceType": "Patient",
        "text": { "status": "generated", "div": div }
    });
    validator
        .validate(&patient, FhirVersion::R4, &ValidationOptions::default(), &h)
        .await
        .into_iter()
        .filter(|e| e.path == "Patient.text.div")
        .map(|e| {
            (
                e.extra
                    .get("constraint")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default()
                    .to_string(),
                e.severity,
            )
        })
        .collect()
}

/// txt-1 and txt-2 are evaluated (no "htmlChecks is not implemented"
/// warning), each judging its own rule.
#[tokio::test]
async fn narrative_invariants_are_evaluated() {
    use helios_fhir_validator::Severity::Error;
    let ok = r#"<div xmlns="http://www.w3.org/1999/xhtml"><p>John <b>Doe</b>&nbsp;1980</p></div>"#;
    assert_eq!(narrative_issues(ok).await, vec![]);

    let script =
        r#"<div xmlns="http://www.w3.org/1999/xhtml"><p>x</p><script>alert(1)</script></div>"#;
    assert_eq!(
        narrative_issues(script).await,
        vec![("txt-1".to_string(), Error)]
    );

    let onclick = r#"<div xmlns="http://www.w3.org/1999/xhtml"><p onclick="x()">x</p></div>"#;
    assert_eq!(
        narrative_issues(onclick).await,
        vec![("txt-1".to_string(), Error)]
    );

    let no_ns = "<div><p>x</p></div>";
    assert_eq!(
        narrative_issues(no_ns).await,
        vec![("txt-1".to_string(), Error)]
    );

    let empty = r#"<div xmlns="http://www.w3.org/1999/xhtml">  <p/> </div>"#;
    assert_eq!(
        narrative_issues(empty).await,
        vec![("txt-2".to_string(), Error)]
    );
}

/// Every `text.div` string in `value`, with nothing else.
fn narratives<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a str>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(div) = map
                .get("text")
                .and_then(|t| t.get("div"))
                .and_then(|d| d.as_str())
            {
                out.push(div);
            }
            map.values().for_each(|v| narratives(v, out));
        }
        serde_json::Value::Array(items) => items.iter().for_each(|v| narratives(v, out)),
        _ => {}
    }
}

/// False-positive guard: every narrative in the official R4 example corpus
/// passes txt-1, and txt-2 unless it is genuinely empty. Each distinct div
/// is judged on a minimal Patient so the sweep stays fast.
#[test]
fn spec_example_narratives_pass_html_checks() {
    use helios_fhir_validator::{ConstraintEvaluator, ConstraintOutcome, DeferredConstraint};
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../fhir/tests/data/json/R4");
    let Ok(entries) = std::fs::read_dir(dir) else {
        eprintln!("R4 example corpus not present at {dir}; skipping");
        return;
    };
    let mut divs: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !text.contains("\"div\"") {
            continue;
        }
        let Ok(resource) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let mut found = Vec::new();
        narratives(&resource, &mut found);
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        for div in found {
            divs.entry(div.to_string()).or_default().push(name.clone());
        }
    }
    assert!(
        divs.len() > 1000,
        "only {} distinct narratives found",
        divs.len()
    );

    let evaluator = FhirPathConstraintEvaluator::new();
    let constraints = ["txt-1", "txt-2"].map(|id| DeferredConstraint {
        path: "Patient.text.div",
        id,
        expression: "htmlChecks()",
    });
    let mut failures = Vec::new();
    for (div, files) in &divs {
        let patient = json!({
            "resourceType": "Patient",
            "text": { "status": "generated", "div": div }
        });
        let outcomes = evaluator.evaluate_all(&patient, FhirVersion::R4, &constraints);
        for (c, outcome) in constraints.iter().zip(outcomes) {
            if outcome != ConstraintOutcome::Passed {
                for file in files {
                    failures.push(format!("{file} {} {outcome:?}", c.id));
                }
            }
        }
    }
    failures.sort();
    failures.dedup();
    // The corpus's only failures: four narratives with no content at all
    // (whitespace, or an empty <pre>), which txt-2 forbids.
    assert_eq!(
        failures,
        vec![
            "activitydefinition-servicerequest-example.json txt-2 Failed",
            "activitydefinition-supplyrequest-example.json txt-2 Failed",
            "eventdefinition-example.json txt-2 Failed",
            "questionnaire-zika-virus-exposure-assessment.json txt-2 Failed",
        ],
        "{} distinct narratives checked",
        divs.len()
    );
}

/// dom-3 (contained resources must be referenced) runs: it applies the
/// `as()` function to a collection.
#[tokio::test]
async fn dom_3_flags_unreferenced_contained_resources() {
    let validator = Validator::new(core_registry(FhirVersion::R4));
    let evaluator = FhirPathConstraintEvaluator::new();
    let suppress = suppress();
    let mut h = handlers(&evaluator);
    h.suppress_constraints = &suppress;
    let dom3 = |errors: &[helios_fhir_validator::ValidationError]| {
        errors
            .iter()
            .filter(|e| e.extra.get("constraint") == Some(&json!("dom-3")))
            .map(|e| (e.severity, e.message.clone()))
            .collect::<Vec<_>>()
    };

    let unreferenced = json!({
        "resourceType": "Patient",
        "contained": [{ "resourceType": "Organization", "id": "org1", "name": "Unref" }]
    });
    let errors = validator
        .validate(
            &unreferenced,
            FhirVersion::R4,
            &ValidationOptions::default(),
            &h,
        )
        .await;
    let found = dom3(&errors);
    assert_eq!(found.len(), 1, "{errors:#?}");
    assert_eq!(found[0].0, helios_fhir_validator::Severity::Error);

    let referenced = json!({
        "resourceType": "Patient",
        "contained": [{ "resourceType": "Organization", "id": "org1", "name": "Ref" }],
        "managingOrganization": { "reference": "#org1" }
    });
    let errors = validator
        .validate(
            &referenced,
            FhirVersion::R4,
            &ValidationOptions::default(),
            &h,
        )
        .await;
    assert!(dom3(&errors).is_empty(), "{errors:#?}");
}
