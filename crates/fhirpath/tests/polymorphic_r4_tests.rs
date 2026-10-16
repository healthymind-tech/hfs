use helios_fhir::r4;
use helios_fhirpath::{EvaluationContext, evaluate_expression};
use helios_fhirpath_support::EvaluationResult;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

/// Tests for polymorphic access to FHIR choice elements in R4 resources
/// This tests that expressions like "value.unit" correctly resolve to "valueQuantity.unit"
/// in observations with a valueQuantity element.
#[test]
fn test_polymorphic_value_unit() {
    // Load the Observation example
    let context = load_test_resource("observation-example.json").expect("Failed to load resource");

    // First verify simple path access works
    let expr_direct = "value.unit";
    let result_direct = run_expression(&context, expr_direct);
    assert_eq!(result_direct, EvaluationResult::string("lbs".to_string()));

    // For now, we use %context to work around the resource type prefix issue
    let expr_context = "%context.value.unit";
    let result_context = run_expression(&context, expr_context);
    assert_eq!(result_context, EvaluationResult::string("lbs".to_string()));
}

/// Tests that the 'is' operator correctly identifies choice element types
#[test]
fn test_polymorphic_is_quantity() {
    // Load the Observation example
    let context = load_test_resource("observation-example.json").expect("Failed to load resource");

    // Test using context to access value - use operator syntax
    let expr = "%context.value is Quantity";
    let result = run_expression(&context, expr);
    assert_eq!(result, EvaluationResult::boolean(true));

    // Also test negative case
    let expr_neg = "%context.value is Period";
    let result_neg = run_expression(&context, expr_neg);
    assert_eq!(result_neg, EvaluationResult::boolean(false));
}

/// Tests that the 'as' operator correctly filters choice elements by type
#[test]
fn test_polymorphic_as_quantity() {
    // Load the Observation example
    let context = load_test_resource("observation-example.json").expect("Failed to load resource");

    // Test with context
    let expr = "%context.value.as(Quantity).unit";
    let result = run_expression(&context, expr);
    assert_eq!(result, EvaluationResult::string("lbs".to_string()));
}

/// Evaluates a FHIRPath expression and returns the result
fn run_expression(context: &EvaluationContext, expression: &str) -> EvaluationResult {
    evaluate_expression(expression, context).expect("Failed to evaluate expression")
}

/// Loads a FHIR R4 resource from a JSON test file and creates an evaluation context
fn load_test_resource(json_filename: &str) -> Result<EvaluationContext, String> {
    // Get the path to the JSON file
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push(format!("tests/data/r4/input/{}", json_filename));

    // Load the JSON file
    let mut file =
        File::open(&path).map_err(|e| format!("Could not open JSON resource file: {:?}", e))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|e| format!("Failed to read JSON resource file: {:?}", e))?;

    // Parse the JSON into a FHIR resource
    let resource: r4::Resource =
        serde_json::from_str(&contents).map_err(|e| format!("Failed to parse JSON: {:?}", e))?;

    // Create an evaluation context with the resource
    let context = EvaluationContext::new(vec![helios_fhir::FhirResource::R4(Box::new(resource))]);
    Ok(context)
}

fn context_for(json: serde_json::Value) -> EvaluationContext {
    let resource: r4::Resource = serde_json::from_value(json).expect("valid R4 resource");
    EvaluationContext::new(vec![helios_fhir::FhirResource::R4(Box::new(resource))])
}

/// `Timing.repeat.periodUnit` is a sibling of `period`, not a `period[x]`
/// variant: core invariant tim-5 (`period.exists() implies period >= 0`)
/// must hold for a repeat with only `periodUnit` (IPS bundle-minimal), not
/// fail to evaluate comparing the unit code with 0.
#[test]
fn timing_period_unit_is_not_a_period_variant() {
    let context = context_for(serde_json::json!({
        "resourceType": "MedicationStatement",
        "status": "active",
        "medicationCodeableConcept": { "text": "x" },
        "subject": { "reference": "Patient/p" },
        "dosage": [{ "timing": { "repeat": { "count": 1, "periodUnit": "d", "durationMax": 2 } } }]
    }));
    let repeat = "MedicationStatement.dosage.timing.repeat";
    assert_eq!(
        run_expression(&context, &format!("{repeat}.period.exists()")),
        EvaluationResult::boolean(false)
    );
    assert_eq!(
        run_expression(
            &context,
            &format!("{repeat}.all(period.exists() implies period >= 0)")
        ),
        EvaluationResult::boolean(true)
    );
    assert_eq!(
        run_expression(&context, &format!("{repeat}.periodUnit")),
        EvaluationResult::string("d".to_string())
    );
}

/// Core invariant rng-2 (`low.empty() or high.empty() or (low <= high)`)
/// compares two Quantity objects, also with a UCUM code the converter does
/// not know (`/[HPF]`, IPS urine-wbc-range).
#[test]
fn range_low_high_quantities_compare() {
    let range = |low: i64, high: i64, code: &str| {
        context_for(serde_json::json!({
            "resourceType": "Observation",
            "status": "final",
            "code": { "text": "x" },
            "valueRange": {
                "low": { "value": low, "system": "http://unitsofmeasure.org", "code": code },
                "high": { "value": high, "system": "http://unitsofmeasure.org", "code": code }
            }
        }))
    };
    let rng2 = "Observation.value.all(low.empty() or high.empty() or (low <= high))";
    assert_eq!(
        run_expression(&range(10, 20, "/[HPF]"), rng2),
        EvaluationResult::boolean(true)
    );
    assert_eq!(
        run_expression(&range(30, 20, "/[HPF]"), rng2),
        EvaluationResult::boolean(false)
    );
    assert_eq!(
        run_expression(&range(10, 20, "mg"), rng2),
        EvaluationResult::boolean(true)
    );
}

/// FHIRPath singleton evaluation: a single element object in a Boolean
/// operator is true (US Core us-core-13 `practitioner or organization or
/// healthcareService or location` on PractitionerRole).
#[test]
fn element_objects_are_true_in_boolean_operators() {
    let role = context_for(serde_json::json!({
        "resourceType": "PractitionerRole",
        "practitioner": { "reference": "Practitioner/p" }
    }));
    let us_core_13 =
        "PractitionerRole.all(practitioner or organization or healthcareService or location)";
    assert_eq!(
        run_expression(&role, us_core_13),
        EvaluationResult::boolean(true)
    );
    assert_eq!(
        run_expression(
            &role,
            "PractitionerRole.all(practitioner implies practitioner)"
        ),
        EvaluationResult::boolean(true)
    );
    let empty = context_for(serde_json::json!({ "resourceType": "PractitionerRole" }));
    assert_eq!(
        run_expression(
            &empty,
            "PractitionerRole.select(practitioner or organization).empty()"
        ),
        EvaluationResult::boolean(true)
    );
}
