//! `children()` / `descendants()` on R4 primitives with `_field` metadata:
//! a primitive's children are its `id` and `extension` (FHIRPath on FHIR
//! element model). dom-3 relies on it to see `#id` canonicals carried in
//! a primitive's extensions (HL7 validator test case vs-canonical-good).

use helios_fhir::r4;
use helios_fhirpath::{EvaluationContext, evaluate_expression};
use helios_fhirpath_support::EvaluationResult;

fn eval(resource: serde_json::Value, expr: &str) -> EvaluationResult {
    let r: r4::Resource = serde_json::from_value(resource).expect("parses");
    let ctx = EvaluationContext::new(vec![helios_fhir::FhirResource::R4(Box::new(r))]);
    evaluate_expression(expr, &ctx).expect("evaluates")
}

fn value_set() -> serde_json::Value {
    serde_json::json!({
        "resourceType": "ValueSet", "status": "active",
        "contained": [{"resourceType": "CodeSystem", "id": "c1", "status": "active",
                       "content": "complete", "url": "http://example.org/c1"}],
        "compose": {"include": [{
            "system": "http://example.org/c1",
            "_system": {"id": "s1", "extension": [{
                "url": "http://hl7.org/fhir/StructureDefinition/valueset-system",
                "valueCanonical": "#c1"}]}
        }]}
    })
}

#[test]
fn primitive_children_are_its_id_and_extensions() {
    assert_eq!(
        eval(value_set(), "compose.include.system.children().count()"),
        EvaluationResult::integer(2)
    );
    assert_eq!(
        eval(value_set(), "status.children().count()"),
        EvaluationResult::integer(0)
    );
}

#[test]
fn descendants_reach_into_primitive_extensions() {
    assert_eq!(
        eval(
            value_set(),
            "descendants().ofType(canonical).where($this = '#c1').exists()"
        ),
        EvaluationResult::boolean(true)
    );
}

#[test]
fn ele1_holds_for_a_primitive_with_a_value_and_an_id() {
    // The element-level rule is unchanged for primitives with a value.
    let p = serde_json::json!({"resourceType": "Patient", "birthDate": "1980-01-01",
        "_birthDate": {"id": "b"}});
    assert_eq!(
        eval(
            p,
            "birthDate.hasValue() or (birthDate.children().count() > birthDate.id.count())"
        ),
        EvaluationResult::boolean(true)
    );
}
