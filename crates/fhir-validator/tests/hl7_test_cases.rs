//! Regressions from the official HL7 validator test cases
//! (FHIR/fhir-test-cases `validator/manifest.json`), reduced to the rule each
//! case exercises. The case name is in each test's doc comment.
//!
//! Run with: `cargo test -p helios-fhir-validator --features fhirpath --test hl7_test_cases`

#![cfg(all(feature = "fhirpath", feature = "R4"))]

use helios_fhir::FhirVersion;
use helios_fhir_validator::converter::convert;
use helios_fhir_validator::fhirpath_effects::FhirPathConstraintEvaluator;
use helios_fhir_validator::packs::core_registry;
use helios_fhir_validator::{
    CompositeResolver, EffectHandlers, SchemaRegistry, SchemaResolver, Severity, ValidationError,
    ValidationOptions, Validator,
};
use serde_json::{Value, json};
use std::sync::Arc;

/// Validate against the R4 core pack overlaid with `sds` (converted like
/// stored profiles), with FHIRPath constraints on.
async fn validate_with(resource: &Value, sds: &[Value], profiles: &[&str]) -> Vec<ValidationError> {
    let core = core_registry(FhirVersion::R4);
    let resolver: Arc<dyn SchemaResolver> = if sds.is_empty() {
        core
    } else {
        let mut overlay = SchemaRegistry::new();
        for sd in sds {
            assert!(overlay.insert(convert(sd).expect("profile converts").schema));
        }
        Arc::new(CompositeResolver::new(vec![Arc::new(overlay), core]))
    };
    let validator = Validator::new(resolver);
    let evaluator = FhirPathConstraintEvaluator::new();
    let suppress = vec!["dom-6".to_string()];
    let handlers = EffectHandlers {
        constraints: Some(&evaluator),
        suppress_constraints: &suppress,
        ..Default::default()
    };
    let opts = ValidationOptions {
        profiles: profiles.iter().map(|p| p.to_string()).collect(),
        ..Default::default()
    };
    validator
        .validate(resource, FhirVersion::R4, &opts, &handlers)
        .await
}

fn errors(issues: &[ValidationError]) -> Vec<&ValidationError> {
    issues
        .iter()
        .filter(|e| e.severity == Severity::Error)
        .collect()
}

/// `obs-percent#profile`, `medstmt-ips#profile`: a differential `value[x]`
/// that only adds a binding keeps the base's types.
#[tokio::test]
async fn typeless_choice_element_keeps_the_base_types() {
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/obs-bound-value",
        "name": "ObsBoundValue", "status": "active", "kind": "resource", "abstract": false,
        "type": "Observation", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Observation",
        "differential": {"element": [
            {"id": "Observation.effective[x]", "path": "Observation.effective[x]", "min": 1},
            {"id": "Observation.value[x]", "path": "Observation.value[x]",
             "patternQuantity": {"system": "http://unitsofmeasure.org"}}
        ]}
    });
    let obs = |value: Value| {
        let mut o = json!({
            "resourceType": "Observation", "status": "final",
            "code": {"text": "oxygen saturation"},
            "effectivePeriod": {"start": "2020-10-11"}
        });
        o["valueQuantity"] = value;
        o
    };
    let url = "http://example.org/StructureDefinition/obs-bound-value";
    let ok = validate_with(
        &obs(json!({"value": 98, "system": "http://unitsofmeasure.org", "code": "%"})),
        &[sd.clone()],
        &[url],
    )
    .await;
    assert!(errors(&ok).is_empty(), "{ok:#?}");
    // The declarer's keywords still apply to the branch in use.
    let bad = validate_with(
        &obs(json!({"value": 98, "system": "http://example.org/units", "code": "%"})),
        &[sd],
        &[url],
    )
    .await;
    assert_eq!(errors(&bad).len(), 1, "{bad:#?}");
}
