//! Thread-stack use of a full validation (structural walk, FHIRPath
//! invariants, embedded terminology) of a large Bundle.
//!
//! Tokio worker threads have 2 MiB of stack, and `$validate` runs the
//! validator on one. In debug builds a full validation needed 1310 KiB of
//! the caller's stack (walk ~260 KiB, FHIRPath invariants the rest), so the
//! whole `$validate` request needed ~1.86 MiB. The recursive parts now run on
//! their own large-stack thread ([`Validator::validate`]); the guard below
//! validates a 200-entry Bundle on a caller thread with `VALIDATOR_STACK_KIB`
//! (default 512) KiB of stack (the harness itself — runtime, pack loading — needs ~260 KiB): a validator that recursed on the caller's
//! stack again aborts the test process with a stack overflow.
//! `eval/scripts/p5-validator/stackprobe.sh` bisects the smallest stack.

#![cfg(all(feature = "fhirpath", feature = "R4"))]

use helios_fhir::FhirVersion;
use helios_fhir_validator::fhirpath_effects::FhirPathConstraintEvaluator;
use helios_fhir_validator::packs::core_registry;
use helios_fhir_validator::{EffectHandlers, ValidationOptions, Validator};
use serde_json::{Value, json};

fn bundle(entries: usize) -> Value {
    let mut items = Vec::new();
    for i in 0..entries {
        let (full_url, resource) = if i % 2 == 0 {
            (
                format!("urn:uuid:00000000-0000-0000-0000-{i:012}"),
                json!({"resourceType": "Patient", "id": format!("p{i}"),
                    "text": {"status": "generated", "div": "<div xmlns=\"http://www.w3.org/1999/xhtml\"><p>x</p></div>"},
                    "name": [{"family": "F", "given": ["G"]}], "gender": "female", "birthDate": "1970-01-01",
                    "contained": [{"resourceType": "Organization", "id": "o", "name": "O"}],
                    "managingOrganization": {"reference": "#o"}}),
            )
        } else {
            (
                format!("urn:uuid:00000000-0000-0000-0000-{i:012}"),
                json!({"resourceType": "Observation", "id": format!("o{i}"), "status": "final",
                    "category": [{"coding": [{"system": "http://terminology.hl7.org/CodeSystem/observation-category", "code": "vital-signs"}]}],
                    "code": {"coding": [{"system": "http://loinc.org", "code": "29463-7"}]},
                    "subject": {"reference": format!("urn:uuid:00000000-0000-0000-0000-{:012}", i - 1)},
                    "effectiveDateTime": "2020-01-01",
                    "valueQuantity": {"value": 70.5, "unit": "kg", "system": "http://unitsofmeasure.org", "code": "kg"}}),
            )
        };
        items.push(json!({"fullUrl": full_url, "resource": resource,
            "request": {"method": "POST", "url": resource["resourceType"]}}));
    }
    json!({"resourceType": "Bundle", "type": "transaction", "entry": items})
}

#[test]
fn validating_a_large_bundle_fits_a_small_stack() {
    let kib: usize = std::env::var("VALIDATOR_STACK_KIB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(512);
    let entries: usize = std::env::var("VALIDATOR_STACK_ENTRIES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let resource = bundle(entries);
    let issues = std::thread::Builder::new()
        .stack_size(kib * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
            runtime.block_on(async {
                let validator = Validator::new(core_registry(FhirVersion::R4));
                let evaluator = FhirPathConstraintEvaluator::new();
                let tx = helios_fhir_validator::core_terminology(FhirVersion::R4);
                let suppress = vec!["dom-6".to_string()];
                let handlers = EffectHandlers {
                    constraints: Some(&evaluator),
                    terminology: Some(tx.as_ref()),
                    suppress_constraints: &suppress,
                    check_extensible: true,
                    narrative_links: true,
                    resource_rules: true,
                    best_practice: true,
                    ..Default::default()
                };
                // VALIDATOR_STACK_PHASE=walk: the structural walk alone.
                if std::env::var("VALIDATOR_STACK_PHASE").as_deref() == Ok("walk") {
                    return validator
                        .validate_sync(&resource, &ValidationOptions::default())
                        .errors;
                }
                validator
                    .validate(&resource, FhirVersion::R4, &ValidationOptions::default(), &handlers)
                    .await
            })
        })
        .unwrap()
        .join()
        .unwrap();
    assert!(
        issues.iter().all(|e| e.severity != helios_fhir_validator::Severity::Error),
        "{issues:#?}"
    );
}
