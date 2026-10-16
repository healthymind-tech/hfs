//! Profile-rule correctness against the HL7 Java validator's behaviour:
//! slicing discriminators, extension definitions and contexts, invariants
//! and bindings, on the embedded R4 pack.
//!
//! Run with: `cargo test -p helios-fhir-validator --features fhirpath --test profile_rules`

#![cfg(all(feature = "fhirpath", feature = "R4"))]

use helios_fhir::FhirVersion;
use helios_fhir_validator::fhirpath_effects::FhirPathConstraintEvaluator;
use helios_fhir_validator::packs::core_registry;
use helios_fhir_validator::{
    EffectHandlers, ErrorKind, ValidationError, ValidationOptions, Validator,
};
use serde_json::{Value, json};

async fn validate(resource: &Value, profiles: &[&str]) -> Vec<ValidationError> {
    let validator = Validator::new(core_registry(FhirVersion::R4));
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

/// obs-7: a component carrying the Observation's own code must not sit
/// next to an Observation-level value.
#[tokio::test]
async fn obs7_fires_when_a_component_repeats_the_code_with_a_value() {
    let code = json!({"coding": [{"system": "http://loinc.org", "code": "8480-6"}]});
    let mut obs = json!({
        "resourceType": "Observation", "status": "final", "code": code,
        "valueQuantity": {"value": 72, "system": "http://unitsofmeasure.org", "code": "/min"},
        "component": [{"code": code,
            "valueQuantity": {"value": 120, "system": "http://unitsofmeasure.org", "code": "mm[Hg]"}}]
    });
    let errors = validate(&obs, &[]).await;
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::FhirpathConstraint
            && e.extra.get("constraint") == Some(&json!("obs-7"))
            && e.severity == helios_fhir_validator::Severity::Error),
        "{errors:#?}"
    );
    obs["component"][0]["code"] = json!({"coding": [{"system": "http://loinc.org", "code": "8462-4"}]});
    let errors = validate(&obs, &[]).await;
    assert!(
        !errors
            .iter()
            .any(|e| e.extra.get("constraint") == Some(&json!("obs-7"))),
        "{errors:#?}"
    );
}

