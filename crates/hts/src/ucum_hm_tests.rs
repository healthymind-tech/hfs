//! Unit tests for the UCUM grammar evaluator: exponents, parentheses,
//! powers of ten, annotations, descriptions, and UCUM value sets
//! (whole grammar, enumerations, `canonical =` filters, excludes).

use super::*;
use SubsumptionOutcome::*;
use serde_json::json;

fn close(a: f64, b: f64) -> bool {
    same_magnitude(a, b)
}

#[test]
fn exponents_and_powers_of_ten() {
    assert_eq!(split_exponent("m2"), ("m", 2));
    assert_eq!(split_exponent("s-1"), ("s", -1));
    assert_eq!(split_exponent("m+3"), ("m", 3));
    assert_eq!(split_exponent("10*3"), ("10*", 3));
    assert_eq!(split_exponent("10*-3"), ("10*", -3));
    // A symbol that is itself an atom is not split.
    assert_eq!(split_exponent("mg"), ("mg", 1));
    // All digits / no digits: unchanged.
    assert_eq!(split_exponent("123"), ("123", 1));
    assert_eq!(split_exponent("abc"), ("abc", 1));

    let k = canonical("10*3").unwrap();
    assert!(close(k.factor, 1000.0));
    assert_eq!(canonical_unit(&k), "1");
    let per_ul = canonical("10*3/uL").unwrap();
    assert_eq!(canonical_unit(&per_ul), "m-3");
    assert!(close(per_ul.factor, 1e3 / 1e-9));
    assert!(close(canonical("10*-3").unwrap().factor, 1e-3));
}

#[test]
fn products_quotients_and_parentheses() {
    let n = canonical("N").unwrap();
    let p = canonical("(kg.m)/s2").unwrap();
    assert_eq!(n.dims, p.dims);
    assert!(close(n.factor, p.factor));
    // A leading division.
    assert_eq!(
        canonical("/s").unwrap().dims,
        canonical("s-1").unwrap().dims
    );
    // An exponent on a parenthesised term.
    let sq = canonical("(m/s)2").unwrap();
    assert_eq!(canonical_unit(&sq), "m2.s-2");
    let inv = canonical("(m/s)-1").unwrap();
    assert_eq!(canonical_unit(&inv), "m-1.s");
    // Unbalanced parentheses and trailing operators do not reduce.
    assert!(canonical("(m/s").is_none());
    assert!(canonical("m/").is_none());
    assert!(canonical("m..s").is_none());
    // A plain number is a dimensionless factor.
    let c = canonical("100").unwrap();
    assert!(close(c.factor, 100.0));
    assert_eq!(canonical_unit(&c), "1");
}

#[test]
fn prefixes_scale_metric_units_only() {
    assert!(close(canonical("mg").unwrap().factor, 1e-3));
    assert!(close(canonical("kg").unwrap().factor, 1e3));
    assert!(close(canonical("dL").unwrap().factor, 1e-4));
    // `[in_i]` is not metric, so a prefix on it is not a unit.
    assert!(canonical("k[in_i]").is_none());
    assert!(canonical("[in_i]").is_some());
}

#[test]
fn special_and_arbitrary_units() {
    // Special units (offset / log scales) do not reduce.
    assert!(canonical("Cel").is_none());
    assert!(canonical("[pH]").is_none());
    // Arbitrary units carry their own name through products and powers.
    let iu = canonical("[IU]").unwrap();
    assert_eq!(iu.arbitrary, vec!["[IU]".to_string()]);
    let per_l = canonical("[IU]/L").unwrap();
    assert_eq!(per_l.arbitrary, vec!["[IU]".to_string()]);
    let inv = canonical("/[IU]").unwrap();
    assert_eq!(inv.arbitrary, vec!["/[IU]".to_string()]);
    let sq = canonical("([IU])2").unwrap();
    assert_eq!(sq.arbitrary, vec!["[IU]^2".to_string()]);
}

#[test]
fn annotations_are_dropped_and_stand_alone_ones_are_one() {
    assert_eq!(without_annotations("mg{total}"), "mg");
    assert_eq!(without_annotations("{tbl}"), "1");
    assert_eq!(without_annotations("/{HPF}"), "/1");
    assert_eq!(without_annotations("{rbc}/uL"), "1/uL");
    assert_eq!(without_annotations("(m.{x})"), "(m.1)");
    assert_eq!(canonical_unit(&canonical("{tbl}").unwrap()), "1");
    assert_eq!(subsumes("{tbl}", "{capsule}"), Ok(Equivalent));
    assert_eq!(subsumes("10*3/uL", "10*9/L"), Ok(Equivalent));
    assert_eq!(subsumes("[IU]", "[IU]/L"), Ok(NotSubsumed));
    assert_eq!(subsumes("mg", "nope{x}"), Err("nope{x}"));
}

#[test]
fn canonical_unit_lists_every_base_dimension() {
    let c = Canonical {
        factor: 1.0,
        dims: [1, 2, -1, 0, 1, -2, 3],
        arbitrary: vec![],
    };
    assert_eq!(canonical_unit(&c), "g.m2.s-1.K.C-2.cd3");
}

#[test]
fn descriptions_cover_numbers_exponents_and_groups() {
    assert_eq!(description("m2"), "(meter) ^ 2");
    assert_eq!(
        description("kg.m/s2"),
        "(kilogram) * (meter) / (second) ^ 2"
    );
    assert_eq!(description("10*3/uL"), "10*3 / (microliter)");
    assert_eq!(description("{tbl}"), "1");
    assert_eq!(description("(m/s)"), "((meter) / (second))");
    // Unknown symbols are echoed.
    assert_eq!(description("zz"), "zz");
}

#[test]
fn error_messages_locate_the_unknown_unit() {
    assert_eq!(
        error_message("mg/qq"),
        "Error processing unit 'mg/qq': The unit 'qq' is unknown at character 4"
    );
    assert_eq!(
        error_message("mg/"),
        "Error processing unit 'mg/': The unit is not a valid UCUM expression"
    );
}

fn vs(include: serde_json::Value, exclude: Option<serde_json::Value>) -> serde_json::Value {
    let mut compose = json!({ "include": include });
    if let Some(e) = exclude {
        compose["exclude"] = e;
    }
    json!({ "resourceType": "ValueSet", "compose": compose })
}

#[test]
fn grammar_value_set_classification() {
    let whole = vs(json!([{ "system": UCUM_URL }]), None);
    assert!(is_ucum_grammar_value_set(&whole));
    assert!(takes_whole_grammar(&whole));

    let filtered = vs(
        json!([{ "system": UCUM_URL, "filter": [{ "property": "canonical", "op": "=", "value": "g" }] }]),
        None,
    );
    assert!(is_ucum_grammar_value_set(&filtered));
    assert!(!takes_whole_grammar(&filtered));

    // Enumerated only: an ordinary list.
    let listed = vs(
        json!([{ "system": UCUM_URL, "concept": [{ "code": "mg" }] }]),
        None,
    );
    assert!(!is_ucum_grammar_value_set(&listed));
    assert!(!takes_whole_grammar(&listed));

    // Another system, a value set import, or no compose: not a UCUM grammar.
    assert!(!is_ucum_grammar_value_set(&vs(
        json!([{ "system": UCUM_URL }, { "system": "http://loinc.org" }]),
        None
    )));
    assert!(!is_ucum_grammar_value_set(&vs(
        json!([{ "system": UCUM_URL, "valueSet": ["http://x/vs"] }]),
        None
    )));
    assert!(!is_ucum_grammar_value_set(
        &json!({ "resourceType": "ValueSet" })
    ));
    // An exclude from another system disqualifies it too.
    assert!(!is_ucum_grammar_value_set(&vs(
        json!([{ "system": UCUM_URL }]),
        Some(json!([{ "system": "http://loinc.org" }]))
    )));
}

#[test]
fn membership_in_ucum_value_sets() {
    let whole = vs(json!([{ "system": UCUM_URL }]), None);
    assert!(in_value_set(&whole, "mg/dL"));
    assert!(!in_value_set(&whole, "not-a-unit"));

    let mass = vs(
        json!([{ "system": UCUM_URL, "filter": [{ "property": "canonical", "op": "=", "value": "g" }] }]),
        None,
    );
    assert!(in_value_set(&mass, "mg"));
    assert!(in_value_set(&mass, "[lb_av]"));
    assert!(!in_value_set(&mass, "mL"));
    // A special unit does not reduce, so it matches no canonical filter.
    assert!(!in_value_set(&mass, "Cel"));

    // Unknown filter properties / operators never match.
    let odd = vs(
        json!([{ "system": UCUM_URL, "filter": [{ "property": "canonical", "op": "is-a", "value": "g" }] }]),
        None,
    );
    assert!(!in_value_set(&odd, "mg"));

    let listed = vs(
        json!([{ "system": UCUM_URL, "concept": [{ "code": "mg" }, { "code": "g" }] }]),
        Some(json!([{ "system": UCUM_URL, "concept": [{ "code": "g" }] }])),
    );
    assert!(in_value_set(&listed, "mg"));
    assert!(!in_value_set(&listed, "g"));
    assert!(!in_value_set(&listed, "kg"));

    // Whole grammar minus a canonical class.
    let not_volume = vs(
        json!([{ "system": UCUM_URL }]),
        Some(
            json!([{ "system": UCUM_URL, "filter": [{ "property": "canonical", "op": "=", "value": "L" }] }]),
        ),
    );
    assert!(in_value_set(&not_volume, "mg"));
    assert!(!in_value_set(&not_volume, "mL"));
}
