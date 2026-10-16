//! `EvaluationContext::enable_root_memo`: `%resource` chains are memoized
//! without changing any result.

use helios_fhir::FhirResource;
use helios_fhirpath::{EvaluationContext, EvaluationResult, evaluator::evaluate, parse_expression};

fn patient() -> FhirResource {
    let json = serde_json::json!({
        "resourceType": "Patient",
        "id": "p",
        "name": [{"family": "A", "given": ["x", "y"]}, {"family": "B"}]
    });
    FhirResource::R4(Box::new(serde_json::from_value(json).unwrap()))
}

#[test]
fn memoized_root_chains_give_the_same_results() {
    let exprs = [
        "name.all(family.exists() and %resource.name.first().family = 'A')",
        "name.select(%resource.name.count())",
        "%resource.name.first().given.count()",
        "%rootResource.name.last().family",
        "name.where(family = %resource.name.tail().first().family).count()",
        "%resource.id & '-' & name.first().family",
    ];
    for text in exprs {
        let ast = parse_expression(text).unwrap();
        let plain = EvaluationContext::new(vec![patient()]);
        let mut memo = EvaluationContext::new(vec![patient()]);
        memo.enable_root_memo();
        let expected = evaluate(&ast, &plain, None).unwrap();
        // Twice: the second run is served from the memo.
        for _ in 0..2 {
            let got = evaluate(&ast, &memo, None).unwrap();
            assert_eq!(got, expected, "{text}");
        }
    }
    let ast = parse_expression("%resource.name.count()").unwrap();
    let mut memo = EvaluationContext::new(vec![patient()]);
    memo.enable_root_memo();
    assert_eq!(
        evaluate(&ast, &memo, None).unwrap(),
        EvaluationResult::integer(2)
    );
    assert!(!memo.root_memo.as_ref().unwrap().lock().is_empty(), "memo used");
}
