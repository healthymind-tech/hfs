//! `hasValue()` on R4 resources: true only for primitives with a value.

use helios_fhir::r4;
use helios_fhirpath::{EvaluationContext, evaluate_expression};
use helios_fhirpath_support::EvaluationResult;

fn eval(resource: serde_json::Value, expr: &str) -> EvaluationResult {
    let r: r4::Resource = serde_json::from_value(resource).expect("parses");
    let ctx = EvaluationContext::new(vec![helios_fhir::FhirResource::R4(Box::new(r))]);
    evaluate_expression(expr, &ctx).expect("evaluates")
}

#[test]
fn complex_elements_have_no_value() {
    let p = serde_json::json!({"resourceType": "Patient", "name": [{"id": "a"}]});
    assert_eq!(
        eval(p.clone(), "name[0].hasValue()"),
        EvaluationResult::boolean(false)
    );
    // ele-1 on an element carrying only an id fails.
    assert_eq!(
        eval(
            p,
            "name[0].hasValue() or (name[0].children().count() > name[0].id.count())"
        ),
        EvaluationResult::boolean(false)
    );
}

#[test]
fn primitives_have_a_value_unless_only_extended() {
    let p = serde_json::json!({"resourceType": "Patient", "birthDate": "1980-01-01",
        "_birthDate": {"id": "b"}, "gender": "male"});
    assert_eq!(
        eval(p.clone(), "birthDate.hasValue()"),
        EvaluationResult::boolean(true)
    );
    assert_eq!(
        eval(p, "gender.hasValue()"),
        EvaluationResult::boolean(true)
    );
    let only_ext = serde_json::json!({"resourceType": "Patient", "_birthDate": {
        "extension": [{"url": "http://hl7.org/fhir/StructureDefinition/data-absent-reason", "valueCode": "unknown"}]}});
    assert_eq!(
        eval(only_ext, "birthDate.hasValue()"),
        EvaluationResult::boolean(false)
    );
}
