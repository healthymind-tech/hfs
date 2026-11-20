//! Unit tests for the REST audit event helpers: patient references,
//! percent-decoding, patient search parameters, outcome codes, truncation.

use super::*;

#[test]
fn normalize_patient_ref_forms() {
    assert_eq!(normalize_patient_ref("Patient/1").as_deref(), Some("Patient/1"));
    assert_eq!(
        normalize_patient_ref("http://h/fhir/Patient/abc").as_deref(),
        Some("Patient/abc")
    );
    assert_eq!(
        normalize_patient_ref("Patient/abc/_history/3").as_deref(),
        Some("Patient/abc")
    );
    assert_eq!(normalize_patient_ref("//Patient//x//").as_deref(), Some("Patient/x"));
    // Not a patient instance.
    assert_eq!(normalize_patient_ref("Group/1"), None);
    assert_eq!(normalize_patient_ref("Patient"), None);
    assert_eq!(normalize_patient_ref("Patient/$everything"), None);
    assert_eq!(normalize_patient_ref("Patient/_search"), None);
    assert_eq!(normalize_patient_ref(""), None);
    // The right-most `Patient` segment wins.
    assert_eq!(
        normalize_patient_ref("Patient/a/Patient/b").as_deref(),
        Some("Patient/b")
    );
}

#[test]
fn percent_decode_edge_cases() {
    assert_eq!(percent_decode("a+b%20c"), "a b c");
    assert_eq!(percent_decode("%2F%2f"), "//");
    // Truncated or invalid escapes are kept literally.
    assert_eq!(percent_decode("100%"), "100%");
    assert_eq!(percent_decode("%4"), "%4");
    assert_eq!(percent_decode("%zz1"), "%zz1");
    // Multi-byte UTF-8 sequences decode; invalid bytes are replaced.
    assert_eq!(percent_decode("%E5%BF%83"), "心");
    assert_eq!(percent_decode("%FF"), "\u{FFFD}");
    assert_eq!(percent_decode(""), "");
}

#[test]
fn patient_parameters_in_queries() {
    let p = |rt: Option<&str>, q: &str| patients_in_params(rt, q);
    assert_eq!(p(Some("Observation"), "patient=1"), vec!["Patient/1"]);
    assert_eq!(
        p(Some("Observation"), "patient=1,2&code=x"),
        vec!["Patient/1", "Patient/2"]
    );
    assert_eq!(
        p(Some("Observation"), "patient=Patient%2F7"),
        vec!["Patient/7"]
    );
    assert_eq!(
        p(Some("Observation"), "patient:Patient=9"),
        vec!["Patient/9"]
    );
    // Other modifiers and chains do not name an id.
    assert!(p(Some("Observation"), "patient:missing=true").is_empty());
    assert!(p(Some("Observation"), "patient.name=smith").is_empty());
    // subject only when it points at a Patient.
    assert_eq!(p(Some("Observation"), "subject=Patient/3"), vec!["Patient/3"]);
    assert!(p(Some("Observation"), "subject=Group/3").is_empty());
    assert!(p(Some("Observation"), "subject=3").is_empty());
    assert_eq!(p(Some("Observation"), "subject:Patient=4"), vec!["Patient/4"]);
    assert_eq!(
        p(Some("Observation"), "subject:Patient=http://h/Patient/5"),
        vec!["Patient/5"]
    );
    // _id names patients only on a Patient search.
    assert_eq!(p(Some("Patient"), "_id=a,b"), vec!["Patient/a", "Patient/b"]);
    assert!(p(Some("Observation"), "_id=a").is_empty());
    assert!(p(None, "_id=a").is_empty());
    // Empty values, empty items and bare keys are ignored.
    assert!(p(Some("Observation"), "patient=&patient&patient=,").is_empty());
    assert_eq!(p(Some("Observation"), "patient= 1 , ,2"), vec!["Patient/1", "Patient/2"]);
    // A versioned reference names the patient, not the version.
    assert_eq!(
        p(Some("Observation"), "patient=Patient/8/_history/2"),
        vec!["Patient/8"]
    );
}

#[test]
fn outcome_codes_follow_the_status_class() {
    assert_eq!(outcome_for_status(0), "0");
    assert_eq!(outcome_for_status(200), "0");
    assert_eq!(outcome_for_status(304), "0");
    assert_eq!(outcome_for_status(399), "0");
    assert_eq!(outcome_for_status(400), "4");
    assert_eq!(outcome_for_status(499), "4");
    assert_eq!(outcome_for_status(500), "8");
    assert_eq!(outcome_for_status(599), "8");
    assert_eq!(outcome_for_status(999), "8");
}

#[test]
fn truncate_keeps_char_boundaries() {
    assert_eq!(truncate("abcdef".into(), 3), "abc");
    assert_eq!(truncate("abc".into(), 10), "abc");
    // "心" is three bytes: cutting inside it backs off to the boundary.
    assert_eq!(truncate("a心b".into(), 2), "a");
    assert_eq!(truncate("a心b".into(), 4), "a心");
    assert_eq!(truncate(String::new(), 0), "");
}

#[test]
fn subject_patient_modifier_with_a_versioned_reference() {
    // `subject:Patient` must name the same patient as `patient=` does for a
    // versioned or absolute reference, not the version number.
    assert_eq!(
        patients_in_params(Some("Observation"), "subject:Patient=Patient/8/_history/2"),
        vec!["Patient/8"]
    );
    assert_eq!(
        patients_in_params(
            Some("Observation"),
            "subject:Patient=https://h/fhir/Patient/9/_history/1"
        ),
        vec!["Patient/9"]
    );
}
