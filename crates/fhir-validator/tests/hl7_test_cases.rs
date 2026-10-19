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

/// Core pack, FHIRPath constraints and the embedded core terminology.
async fn validate_with_tx(resource: &Value) -> Vec<ValidationError> {
    let validator = Validator::new(core_registry(FhirVersion::R4));
    let evaluator = FhirPathConstraintEvaluator::new();
    let tx = helios_fhir_validator::core_terminology(FhirVersion::R4);
    let suppress = vec!["dom-6".to_string()];
    let handlers = EffectHandlers {
        constraints: Some(&evaluator),
        terminology: Some(tx.as_ref()),
        suppress_constraints: &suppress,
        ..Default::default()
    };
    validator
        .validate(
            resource,
            FhirVersion::R4,
            &ValidationOptions::default(),
            &handlers,
        )
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

fn constraint_failures<'a>(issues: &'a [ValidationError], id: &str) -> Vec<&'a ValidationError> {
    issues
        .iter()
        .filter(|e| e.extra.get("constraint") == Some(&json!(id)))
        .collect()
}

/// `contained-invariant`: `%context`, `%resource` and `%rootResource` on
/// invariants a profile puts on `contained`, inside a contained resource and
/// on a plain element.
#[tokio::test]
async fn invariant_variables_in_contained_resources() {
    let check = "%context.type().name = '{ctx}' and %resource.type().name = '{res}' \
                 and %rootResource.type().name = 'Patient'";
    let c = |key: &str, path: &str, ctx: &str, res: &str| {
        json!({"id": path, "path": path, "constraint": [{
            "key": key, "severity": "error", "human": "Check context variables are set correctly",
            "expression": check.replace("{ctx}", ctx).replace("{res}", res)}]})
    };
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/contained-invariant",
        "name": "ContainedInvariant", "status": "active", "kind": "resource", "abstract": false,
        "type": "Patient", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
        "differential": {"element": [
            c("cont-1", "Patient.contained", "Practitioner", "Patient"),
            c("cont-2", "Patient.contained.name", "HumanName", "Practitioner"),
            c("cont-3", "Patient.generalPractitioner", "Reference", "Patient")
        ]}
    });
    let patient = json!({
        "resourceType": "Patient",
        "meta": {"profile": ["http://example.org/StructureDefinition/contained-invariant"]},
        "contained": [{"resourceType": "Practitioner", "id": "gp", "name": [{"family": "Doe"}]}],
        "generalPractitioner": [{"reference": "#gp"}]
    });
    let issues = validate_with(&patient, &[sd], &[]).await;
    for id in ["cont-1", "cont-2", "cont-3", "dom-3", "ref-1"] {
        assert!(
            constraint_failures(&issues, id).is_empty(),
            "{id}: {issues:#?}"
        );
    }
}

/// `bundle-invariant`: an invariant a Bundle profile puts on
/// `Bundle.entry.resource` sees the entry as `%context` and the Bundle as
/// `%resource` / `%rootResource`.
#[tokio::test]
async fn invariant_variables_on_bundle_entry_resources() {
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/bundle-invariant",
        "name": "BundleInvariant", "status": "active", "kind": "resource", "abstract": false,
        "type": "Bundle", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Bundle",
        "differential": {"element": [{"id": "Bundle.entry.resource", "path": "Bundle.entry.resource",
            "constraint": [{"key": "variables-test", "severity": "error", "human": "variables",
              "expression": "%context.type().name = 'Patient' and %resource.type().name = 'Bundle' \
                             and %rootResource.type().name = 'Bundle'"}]}]}
    });
    let bundle = json!({
        "resourceType": "Bundle", "type": "collection",
        "meta": {"profile": ["http://example.org/StructureDefinition/bundle-invariant"]},
        "entry": [{"fullUrl": "http://example.org/fhir/Patient/a",
                   "resource": {"resourceType": "Patient", "id": "a"}}]
    });
    let issues = validate_with(&bundle, &[sd], &[]).await;
    assert!(
        constraint_failures(&issues, "variables-test").is_empty(),
        "{issues:#?}"
    );
}

/// `bundle-resolve-deep`, `bundle-with-contained`: ref-1 and dom-3 on an
/// entry resource look at that resource's own contained resources, not the
/// Bundle's.
#[tokio::test]
async fn contained_references_inside_bundle_entries() {
    let entry = |device_ref: &str| {
        json!({"fullUrl": "http://example.org/fhir/Observation/o",
               "resource": {"resourceType": "Observation", "id": "o", "status": "final",
                 "code": {"text": "x"},
                 "contained": [{"resourceType": "Device", "id": "d1"}],
                 "device": {"reference": device_ref}}})
    };
    let bundle = |entries: Vec<Value>| json!({"resourceType": "Bundle", "type": "collection", "entry": entries});
    let ok = validate_with(&bundle(vec![entry("#d1")]), &[], &[]).await;
    assert!(constraint_failures(&ok, "ref-1").is_empty(), "{ok:#?}");
    assert!(constraint_failures(&ok, "dom-3").is_empty(), "{ok:#?}");
    // A contained resource referenced only from another entry is not
    // referenced from its own container.
    let mut other = entry("#d1");
    other["fullUrl"] = json!("http://example.org/fhir/Observation/o2");
    other["resource"]["id"] = json!("o2");
    other["resource"]["contained"] = json!([]);
    other["resource"]
        .as_object_mut()
        .unwrap()
        .remove("contained");
    let mut first = entry("#d1");
    first["resource"].as_object_mut().unwrap().remove("device");
    let bad = validate_with(&bundle(vec![first, other]), &[], &[]).await;
    assert_eq!(constraint_failures(&bad, "dom-3").len(), 1, "{bad:#?}");
}

/// `allergy`, `us-docref`: a required-bound code present only as a
/// data-absent-reason sidecar has no code; a bound code with a value and a
/// sidecar, or an unbound primitive (Attachment.data) without value, is fine.
#[tokio::test]
async fn required_bound_code_without_value_is_an_error() {
    let dar = json!({"extension": [{
        "url": "http://hl7.org/fhir/StructureDefinition/data-absent-reason", "valueCode": "unknown"}]});
    let allergy = json!({
        "resourceType": "AllergyIntolerance", "patient": {"reference": "Patient/p"},
        "clinicalStatus": {"coding": [{
            "system": "http://terminology.hl7.org/CodeSystem/allergyintolerance-clinical",
            "code": "active"}]},
        "category": ["food", null], "_category": [dar.clone(), dar.clone()]
    });
    let issues = validate_with(&allergy, &[], &[]).await;
    let errs = errors(&issues);
    assert_eq!(errs.len(), 1, "{issues:#?}");
    assert_eq!(errs[0].path, "AllergyIntolerance.category.1");
    let docref = json!({
        "resourceType": "DocumentReference", "_status": dar.clone(),
        "content": [{"attachment": {"contentType": "text/plain", "_data": dar}}]
    });
    let issues = validate_with(&docref, &[], &[]).await;
    let errs = errors(&issues);
    assert_eq!(errs.len(), 1, "{issues:#?}");
    assert_eq!(errs[0].path, "DocumentReference.status");
}

/// Diagnostic, not a check: invariant failures per constraint id (and
/// unknown codes in the embedded core code systems) over the R4
/// spec examples (`crates/fhir/tests/data/json/R4`), to measure how a change
/// to constraint evaluation moves false positives. Run with
/// `cargo test -p helios-fhir-validator --features fhirpath --test hl7_test_cases
///  constraint_sweep -- --ignored --nocapture`.
#[tokio::test]
#[ignore]
async fn constraint_sweep_r4_spec_examples() {
    let dir =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fhir/tests/data/json/R4");
    let mut counts: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    let mut files = 0;
    let mut entries: Vec<_> = std::fs::read_dir(&dir).expect("corpus").flatten().collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(resource) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if resource.get("resourceType").is_none() {
            continue;
        }
        files += 1;
        for e in validate_with_tx(&resource).await {
            if e.severity != Severity::Error {
                continue;
            }
            let key = match e.extra.get("constraint").and_then(Value::as_str) {
                Some(id) => id.to_string(),
                None if e.message.starts_with("Unknown code") => "unknown-code".to_string(),
                None => continue,
            };
            counts.entry(key).or_default().push(format!(
                "{} {}",
                entry.file_name().to_string_lossy(),
                e.path
            ));
        }
    }
    eprintln!("{files} resources");
    for (id, hits) in &counts {
        eprintln!(
            "{id}: {} e.g. {}",
            hits.len(),
            hits.iter().take(3).cloned().collect::<Vec<_>>().join(" | ")
        );
    }
}

/// `obs-de` (KBV): a profile requiring `coding.display.extension` judges the
/// `_display` sidecar, not the display string.
#[tokio::test]
async fn profile_children_of_a_primitive_apply_to_its_sidecar() {
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/display-translated",
        "name": "DisplayTranslated", "status": "active", "kind": "resource", "abstract": false,
        "type": "Observation", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Observation",
        "differential": {"element": [
            {"id": "Observation.code.coding.display.extension",
             "path": "Observation.code.coding.display.extension", "min": 1},
            {"id": "Observation.code.coding.display.id",
             "path": "Observation.code.coding.display.id", "max": "0"}
        ]}
    });
    let url = "http://example.org/StructureDefinition/display-translated";
    let obs = |coding: Value| {
        json!({"resourceType": "Observation", "status": "final",
               "code": {"coding": [coding]}, "subject": {"reference": "Patient/p"}})
    };
    let translated = json!({"system": "http://example.org/cs", "code": "x", "display": "X",
        "_display": {"extension": [{"url": "http://example.org/ext", "valueString": "x"}]}});
    let issues = validate_with(&obs(translated), &[sd.clone()], &[url]).await;
    assert!(errors(&issues).is_empty(), "{issues:#?}");

    let plain = json!({"system": "http://example.org/cs", "code": "x", "display": "X"});
    let issues = validate_with(&obs(plain), &[sd.clone()], &[url]).await;
    let errs = errors(&issues);
    assert_eq!(errs.len(), 1, "{issues:#?}");
    assert_eq!(errs[0].path, "Observation.code.coding.0.display.extension");

    let with_id = json!({"system": "http://example.org/cs", "code": "x", "display": "X",
        "_display": {"id": "d", "extension": [{"url": "http://example.org/ext", "valueString": "x"}]}});
    let issues = validate_with(&obs(with_id), &[sd], &[url]).await;
    assert_eq!(errors(&issues).len(), 1, "{issues:#?}");
}

/// `pat-dob-ext`, `uuid-extension`, `obs-vs-1`: with the HL7 validator's
/// policy an extension nobody defines is an error — except example urls,
/// HL7 tooling and cross-version extensions; the lenient default keeps
/// non-HL7 unknown extensions silent.
#[tokio::test]
async fn unknown_extension_policy() {
    use helios_fhir_validator::UnknownExtensionPolicy;
    let patient = |url: &str| {
        json!({"resourceType": "Patient",
               "_birthDate": {"extension": [{"url": url, "valueString": "x"}]},
               "birthDate": "2000-01-01",
               "extension": [{"url": url, "valueString": "x"}]})
    };
    let run = |policy: UnknownExtensionPolicy, url: &'static str| async move {
        let validator = Validator::new(core_registry(FhirVersion::R4));
        let opts = ValidationOptions {
            unknown_extensions: policy,
            ..Default::default()
        };
        let outcome = validator.validate_sync(&patient(url), &opts);
        outcome
            .errors
            .into_iter()
            .filter(|e| e.severity == Severity::Error)
            .count()
    };
    let local = "http://validitron.unimelb.edu.au/fhir/StructureDefinition/age";
    assert_eq!(run(UnknownExtensionPolicy::Error, local).await, 2);
    assert_eq!(run(UnknownExtensionPolicy::Lenient, local).await, 0);
    for allowed in [
        "http://example.org/fhir/StructureDefinition/x",
        "http://hl7.org/fhir/5.0/StructureDefinition/extension-Patient.birthDate",
        "http://hl7.org/fhir/tools/StructureDefinition/x",
    ] {
        assert_eq!(
            run(UnknownExtensionPolicy::Error, allowed).await,
            0,
            "{allowed}"
        );
    }
}

fn structural_errors(resource: &Value) -> Vec<(String, String)> {
    let validator = Validator::new(core_registry(FhirVersion::R4));
    validator
        .validate_sync(resource, &ValidationOptions::default())
        .errors
        .into_iter()
        .filter(|e| e.severity == Severity::Error)
        .map(|e| (e.path, e.message))
        .collect()
}

/// `patient-hs-1`, `patient-id-extensions`, `empty-array`,
/// `contained-resource-duplicate`: JSON-representation rules.
#[test]
fn json_representation_rules() {
    // A primitive array position with neither value nor sidecar.
    let errs = structural_errors(&json!({"resourceType": "Patient",
        "name": [{"given": ["a"], "_given": [{"id": "test"}, null]}]}));
    assert_eq!(errs.len(), 1, "{errs:#?}");
    assert_eq!(errs[0].0, "Patient.name.0.given");
    assert!(errs[0].1.contains("invalid at index 1"), "{errs:#?}");
    let errs = structural_errors(&json!({"resourceType": "Patient",
        "name": [{"given": ["a", null], "_given": [null, {"extension": [{
            "url": "http://hl7.org/fhir/StructureDefinition/data-absent-reason", "valueCode": "unknown"}]}]}]}));
    assert!(errs.is_empty(), "{errs:#?}");

    // Resource.id carries no extensions.
    let errs = structural_errors(&json!({"resourceType": "Patient", "id": "p",
        "_id": {"extension": [{"url": "http://hl7.org/fhir/StructureDefinition/data-absent-reason",
                               "valueCode": "masked"}]}}));
    assert_eq!(errs.len(), 1, "{errs:#?}");
    assert_eq!(errs[0].0, "Patient.id");

    // Empty arrays.
    let errs = structural_errors(&json!({"resourceType": "Patient", "name": []}));
    assert_eq!(errs.len(), 1, "{errs:#?}");
    assert!(errs[0].1.starts_with("Array cannot be empty"), "{errs:#?}");

    // Contained ids are unique within the resource.
    let errs = structural_errors(&json!({"resourceType": "Condition",
        "subject": {"reference": "Patient/p"},
        "recorder": {"reference": "#23"}, "asserter": {"reference": "#23"},
        "contained": [{"resourceType": "Practitioner", "id": "23"},
                      {"resourceType": "Practitioner", "id": "23"}]}));
    assert_eq!(errs.len(), 1, "{errs:#?}");
    assert_eq!(errs[0].0, "Condition.contained.1");
}

/// `obs-value-min`, `obs-value-min-g`, `pat-minvalue-date`, `param-mintime`:
/// a profile's `minValue[x]` / `maxValue[x]` bound the data.
#[tokio::test]
async fn min_and_max_values() {
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/bounded",
        "name": "Bounded", "status": "active", "kind": "resource", "abstract": false,
        "type": "Observation", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Observation",
        "differential": {"element": [
            {"id": "Observation.value[x]", "path": "Observation.value[x]",
             "type": [{"code": "Quantity"}],
             "minValueQuantity": {"value": 0, "system": "http://unitsofmeasure.org", "code": "kg"},
             "maxValueQuantity": {"value": 10, "system": "http://unitsofmeasure.org", "code": "kg"}},
            {"id": "Observation.effective[x]", "path": "Observation.effective[x]",
             "type": [{"code": "dateTime"}], "minValueDateTime": "2025-01-01"}
        ]}
    });
    let url = "http://example.org/StructureDefinition/bounded";
    let obs = |value: f64, unit: &str, effective: &str| {
        json!({"resourceType": "Observation", "status": "final", "code": {"text": "weight"},
               "effectiveDateTime": effective,
               "valueQuantity": {"value": value, "system": "http://unitsofmeasure.org", "code": unit}})
    };
    let count = |issues: Vec<ValidationError>| {
        issues
            .iter()
            .filter(|e| e.severity == Severity::Error && e.message.contains("than the specified"))
            .count()
    };
    assert_eq!(
        count(validate_with(&obs(5.0, "kg", "2025-06-01"), &[sd.clone()], &[url]).await),
        0
    );
    assert_eq!(
        count(validate_with(&obs(-1.0, "g", "2025-06-01"), &[sd.clone()], &[url]).await),
        1
    );
    assert_eq!(
        count(validate_with(&obs(11000.0, "g", "2025-06-01"), &[sd.clone()], &[url]).await),
        1
    );
    assert_eq!(
        count(validate_with(&obs(5.0, "kg", "2024-12-31"), &[sd], &[url]).await),
        1
    );
}

/// `base64-ws-*`, `attachment-with-wrong-size`: Attachment.data must be
/// valid base64 (whitespace tolerated) and match a stated size and hash.
#[test]
fn attachment_data_size_and_hash() {
    let text = "This is the content of a test attachment. It is long enough that its base64 form is wrapped onto more than one line.";
    let b64 = "VGhpcyBpcyB0aGUgY29udGVudCBvZiBhIHRlc3QgYXR0YWNobWVudC4gSXQgaXMgbG9uZyBlbm91Z2ggdGhhdCBpdHMgYmFzZTY0IGZvcm0gaXMgd3JhcHBlZCBvbnRvIG1vcmUgdGhhbiBvbmUgbGluZS4=";
    let params = |data: &str, size: usize, hash: &str| {
        json!({"resourceType": "Parameters", "parameter": [{"name": "a", "valueAttachment": {
            "contentType": "text/plain", "data": data, "size": size, "hash": hash}}]})
    };
    let hash = "0TUjD0LsI0HVJR71SRAloD71d+o=";
    let wrapped = format!("{}\r\n{}", &b64[..76], &b64[76..]);
    assert!(structural_errors(&params(&wrapped, text.len(), hash)).is_empty());
    let errs = structural_errors(&params(b64, 999, hash));
    assert_eq!(errs.len(), 1, "{errs:#?}");
    assert!(
        errs[0].1.starts_with("Stated Attachment Size 999"),
        "{errs:#?}"
    );
    let errs = structural_errors(&params(b64, text.len(), "LEZwlbCg5nvlH2vQD4DLIRiEbdw="));
    assert_eq!(errs.len(), 1, "{errs:#?}");
    assert!(
        errs[0].1.starts_with("The hash of the data did not match"),
        "{errs:#?}"
    );
    // Padding in the middle of the data (base64-ws/r4-bad-padding.json).
    let bad = "VGhpcyBpcyB0aGUgY29udGVudCBvZiBhIHRlc3QgYXR0YWNobWVudA==\r\nLiBJdCBpcyBsb25nIGVub3VnaCB0aGF0IGl0cyBiYXNlNjQgZm9ybSBpcyB3cmFwcGVkIG9udG8g\r\nbW9yZSB0aGFuIG9uZSBsaW5lLg==";
    let errs = structural_errors(&params(bad, text.len(), hash));
    assert_eq!(errs.len(), 1, "{errs:#?}");
    assert!(errs[0].1.contains("not valid base64"), "{errs:#?}");
}

/// `hakan-se-nofetch`: a `#id` reference naming no contained resource is
/// unresolvable (besides ref-1); `#` (the container) and references between
/// a container's contained resources resolve.
#[test]
fn unresolvable_contained_references() {
    let unresolved = |r: &Value| {
        structural_errors(r)
            .into_iter()
            .filter(|(_, m)| m.starts_with("Unable to resolve resource"))
            .count()
    };
    let rx = |requester: &str| {
        json!({"resourceType": "MedicationRequest", "status": "active", "intent": "order",
               "medicationCodeableConcept": {"text": "x"}, "subject": {"reference": "Patient/p"},
               "requester": {"reference": requester},
               "contained": [{"resourceType": "Practitioner", "id": "dr"},
                             {"resourceType": "PractitionerRole", "id": "role",
                              "practitioner": {"reference": "#dr"}}],
               "performer": {"reference": "#role"}})
    };
    assert_eq!(unresolved(&rx("#dr")), 0);
    assert_eq!(unresolved(&rx("#nobody")), 1);
}

/// `resource-invalid-eid-2`: a string longer than 1 MB is invalid; base64
/// content is not capped.
#[test]
fn strings_are_capped_at_one_megabyte() {
    let big = "x".repeat(1024 * 1024 + 1);
    let errs = structural_errors(&json!({"resourceType": "Patient", "name": [{"text": big}]}));
    assert_eq!(
        errs.len(),
        1,
        "{:?}",
        errs.iter().map(|e| &e.0).collect::<Vec<_>>()
    );
    assert!(
        errs[0]
            .1
            .starts_with("value is longer than permitted maximum length")
    );
    let data = "QUJD".repeat(300_000);
    let errs = structural_errors(
        &json!({"resourceType": "Binary", "contentType": "text/plain",
        "data": data}),
    );
    assert!(
        errs.is_empty(),
        "{:?}",
        errs.iter().map(|e| &e.0).collect::<Vec<_>>()
    );
}

/// `ext-ctxt-*`: an extension whose contexts are element paths, another
/// extension's url, and a FHIRPath expression is judged against each kind.
#[tokio::test]
async fn extension_contexts_of_every_kind() {
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://hl7.org/fhir/test/StructureDefinition/ext-ctxt-defn",
        "name": "ExtCtxt", "status": "active", "kind": "complex-type", "abstract": false,
        "type": "Extension", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Extension",
        "context": [
            {"type": "element", "expression": "Patient"},
            {"type": "element", "expression": "Patient.name"},
            {"type": "extension", "expression": "http://hl7.org/fhir/StructureDefinition/patient-interpreterRequired"},
            {"type": "fhirpath", "expression": "Patient.address.where(use = 'home')"}
        ],
        "differential": {"element": [
            {"id": "Extension.extension", "path": "Extension.extension", "max": "0"},
            {"id": "Extension.url", "path": "Extension.url",
             "fixedUri": "http://hl7.org/fhir/test/StructureDefinition/ext-ctxt-defn"},
            {"id": "Extension.value[x]", "path": "Extension.value[x]", "type": [{"code": "boolean"}]}
        ]}
    });
    let ext = json!({"url": "http://hl7.org/fhir/test/StructureDefinition/ext-ctxt-defn",
                     "valueBoolean": true});
    let count = |issues: Vec<ValidationError>| {
        issues
            .iter()
            .filter(|e| e.kind == helios_fhir_validator::ErrorKind::ExtensionContext)
            .count()
    };
    let run = |resource: Value| {
        let sd = sd.clone();
        async move {
            let core = core_registry(FhirVersion::R4);
            let mut overlay = SchemaRegistry::new();
            overlay.insert(convert(&sd).unwrap().schema);
            let resolver: Arc<dyn SchemaResolver> =
                Arc::new(CompositeResolver::new(vec![Arc::new(overlay), core]));
            let opts = ValidationOptions {
                check_extension_context: true,
                ..Default::default()
            };
            Validator::new(resolver)
                .validate_sync(&resource, &opts)
                .errors
        }
    };
    let patient = |extra: Value| {
        let mut p = json!({"resourceType": "Patient"});
        p.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        p
    };
    // allowed: root, name, inside interpreterRequired
    assert_eq!(
        count(run(patient(json!({"extension": [ext.clone()]}))).await),
        0
    );
    assert_eq!(
        count(
            run(patient(
                json!({"name": [{"extension": [ext.clone()], "family": "x"}]})
            ))
            .await
        ),
        0
    );
    let nested = json!({"url": "http://hl7.org/fhir/StructureDefinition/patient-interpreterRequired",
                        "extension": [ext.clone()]});
    let issues = run(patient(json!({"extension": [nested]}))).await;
    let shown = format!("{issues:#?}");
    assert_eq!(count(issues), 0, "{shown}");
    // not allowed: on a primitive, on another resource type
    assert_eq!(
        count(
            run(patient(
                json!({"active": true, "_active": {"extension": [ext.clone()]}})
            ))
            .await
        ),
        1
    );
    assert_eq!(
        count(run(json!({"resourceType": "Organization", "extension": [ext.clone()]})).await),
        1
    );
}

/// `patient-extension-bad*`: an extension needs an absolute url (relative
/// urls are for a complex extension's own sub-extensions), and FHIR JSON has
/// no empty string values.
#[test]
fn extension_urls_and_empty_values() {
    let count = |r: Value, prefix: &str| {
        structural_errors(&r)
            .into_iter()
            .filter(|(_, m)| m.starts_with(prefix))
            .count()
    };
    let patient = |ext: Value| json!({"resourceType": "Patient", "extension": [ext]});
    assert_eq!(
        count(
            patient(json!({"valueString": "x"})),
            "Extension.url is required"
        ),
        1
    );
    assert_eq!(
        count(
            patient(json!({"url": "", "valueString": "x"})),
            "Extension.url is required"
        ),
        1
    );
    assert_eq!(
        count(
            patient(json!({"url": "", "valueString": "x"})),
            "value cannot be empty"
        ),
        1
    );
    assert_eq!(
        count(
            patient(json!({"url": "something", "valueString": "x"})),
            "Extension.url must be an absolute URL"
        ),
        1
    );
    let complex = json!({"url": "http://hl7.org/fhir/StructureDefinition/patient-animal",
        "extension": [{"url": "species", "valueCodeableConcept": {"text": "dog"}}]});
    assert_eq!(
        count(patient(complex), "Extension.url must be an absolute URL"),
        0
    );
}

/// `slicing-types-by-string`: in a differential without element ids, the
/// elements after a `sliceName` element constrain that slice.
#[tokio::test]
async fn idless_differentials_constrain_the_open_slice() {
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/payload-slices",
        "name": "PayloadSlices", "status": "active", "kind": "resource", "abstract": false,
        "type": "Communication", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Communication",
        "differential": {"element": [
            {"path": "Communication"},
            {"path": "Communication.payload", "slicing": {
                "discriminator": [{"type": "type", "path": "content"}], "rules": "open"}},
            {"path": "Communication.payload", "sliceName": "string"},
            {"path": "Communication.payload.contentString", "min": 1, "type": [{"code": "string"}]},
            {"path": "Communication.payload", "sliceName": "attachment", "max": "10"},
            {"path": "Communication.payload.contentAttachment", "type": [{"code": "Attachment"}]}
        ]}
    });
    let comm = json!({"resourceType": "Communication", "status": "in-progress", "payload": [
        {"contentString": "test"},
        {"contentAttachment": {"title": "some title"}},
        {"contentAttachment": {"title": "some other title"}}]});
    let issues = validate_with(
        &comm,
        &[sd],
        &["http://example.org/StructureDefinition/payload-slices"],
    )
    .await;
    assert!(errors(&issues).is_empty(), "{issues:#?}");
}

/// `profile-choice-1`, `no/Person-test-bad`, `bundle-ihe-bad`: an element's
/// `type.profile` binds its values — one profile layers on, several must
/// match at least one, and a resource profile requires its resource type.
#[tokio::test]
async fn type_profiles_bind_element_values() {
    let address = |url: &str, required: &str| {
        json!({
            "resourceType": "StructureDefinition", "url": url, "name": "A", "status": "active",
            "kind": "complex-type", "abstract": false, "type": "Address", "derivation": "constraint",
            "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Address",
            "differential": {"element": [{"id": format!("Address.{required}"),
                                          "path": format!("Address.{required}"), "min": 1}]}
        })
    };
    let a1 = "http://example.org/StructureDefinition/address-use";
    let a2 = "http://example.org/StructureDefinition/address-type";
    let patient_profile = |profiles: &[&str]| {
        json!({
            "resourceType": "StructureDefinition",
            "url": "http://example.org/StructureDefinition/patient-address",
            "name": "P", "status": "active", "kind": "resource", "abstract": false,
            "type": "Patient", "derivation": "constraint",
            "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
            "differential": {"element": [{"id": "Patient.address", "path": "Patient.address",
                "type": [{"code": "Address", "profile": profiles}]}]}
        })
    };
    let p = "http://example.org/StructureDefinition/patient-address";
    let patient = |addr: Value| json!({"resourceType": "Patient", "address": [addr]});
    let sds = |profiles: &[&str]| {
        vec![
            address(a1, "use"),
            address(a2, "type"),
            patient_profile(profiles),
        ]
    };
    let count = |issues: Vec<ValidationError>| errors(&issues).len();
    // two profiles: matching either one is enough
    assert_eq!(
        count(validate_with(&patient(json!({"use": "home"})), &sds(&[a1, a2]), &[p]).await),
        0
    );
    assert_eq!(
        count(validate_with(&patient(json!({"type": "postal"})), &sds(&[a1, a2]), &[p]).await),
        0
    );
    // neither: the no-match error plus each candidate's own failure
    let issues = validate_with(&patient(json!({"city": "x"})), &sds(&[a1, a2]), &[p]).await;
    let errs: Vec<String> = errors(&issues).iter().map(|e| e.path.clone()).collect();
    assert_eq!(
        errs,
        vec![
            "Patient.address.0".to_string(),
            "Patient.address.0.use".to_string(),
            "Patient.address.0.type".to_string()
        ]
    );
    // one profile: its rules apply directly
    let issues = validate_with(&patient(json!({"city": "x"})), &sds(&[a1]), &[p]).await;
    let errs: Vec<String> = errors(&issues).iter().map(|e| e.path.clone()).collect();
    assert_eq!(errs, vec!["Patient.address.0.use".to_string()]);
}

/// `obs-temp-bad`, `obs-vs-1`, `obs-quantity-us`: a vital-signs
/// Observation is held to the core vital signs profile its code maps to.
#[test]
fn vital_signs_observations_meet_their_core_profile() {
    let obs = |system: &str, code: &str, unit: &str| {
        json!({"resourceType": "Observation", "status": "final",
               "category": [{"coding": [{"system": "http://terminology.hl7.org/CodeSystem/observation-category",
                                         "code": "vital-signs"}]}],
               "code": {"coding": [{"system": system, "code": code}]},
               "subject": {"reference": "Patient/p"}, "effectiveDateTime": "2020-06-30",
               "valueQuantity": {"value": 36.0, "system": "http://unitsofmeasure.org", "code": unit, "unit": unit}})
    };
    // SNOMED body temperature: the bodytemp profile's LOINC code slice fails
    let errs = structural_errors(&obs("http://snomed.info/sct", "276885007", "Cel"));
    assert!(!errs.is_empty(), "bodytemp applies");
    // with the LOINC code and a body temperature unit it conforms
    let errs = structural_errors(&obs("http://loinc.org", "8310-5", "Cel"));
    assert!(errs.is_empty(), "{errs:#?}");
    // a systolic pressure is a blood pressure (components required)
    let errs = structural_errors(&obs("http://loinc.org", "8480-6", "mm[Hg]"));
    assert!(errs.len() >= 2, "{errs:#?}");
    // not a vital-signs code: untouched
    assert!(structural_errors(&obs("http://loinc.org", "1234-5", "zzz")).is_empty());
    // the code decides, not the category (the bp profile then also wants
    // the vital-signs category)
    let mut lab = obs("http://loinc.org", "8480-6", "mm[Hg]");
    lab["category"][0]["coding"][0]["code"] = json!("laboratory");
    assert!(structural_errors(&lab).len() > errs.len(), "{errs:#?}");
}

/// `ext-ctxt-good-ext`: an extension whose context is another extension may
/// sit on that extension's primitive value.
#[tokio::test]
async fn extension_context_on_an_extension_value() {
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/on-interpreter",
        "name": "OnInterpreter", "status": "active", "kind": "complex-type", "abstract": false,
        "type": "Extension", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Extension",
        "context": [{"type": "extension",
                     "expression": "http://hl7.org/fhir/StructureDefinition/patient-interpreterRequired"}],
        "differential": {"element": [
            {"id": "Extension.url", "path": "Extension.url",
             "fixedUri": "http://example.org/StructureDefinition/on-interpreter"},
            {"id": "Extension.value[x]", "path": "Extension.value[x]", "type": [{"code": "boolean"}]}
        ]}
    });
    let patient = json!({"resourceType": "Patient", "extension": [{
        "url": "http://hl7.org/fhir/StructureDefinition/patient-interpreterRequired",
        "valueBoolean": true,
        "_valueBoolean": {"extension": [{"url": "http://example.org/StructureDefinition/on-interpreter",
                                         "valueBoolean": true}]}}]});
    let core = core_registry(FhirVersion::R4);
    let mut overlay = SchemaRegistry::new();
    overlay.insert(convert(&sd).unwrap().schema);
    let resolver: Arc<dyn SchemaResolver> =
        Arc::new(CompositeResolver::new(vec![Arc::new(overlay), core]));
    let opts = ValidationOptions {
        check_extension_context: true,
        ..Default::default()
    };
    let issues = Validator::new(resolver)
        .validate_sync(&patient, &opts)
        .errors;
    assert!(
        !issues
            .iter()
            .any(|e| e.kind == helios_fhir_validator::ErrorKind::ExtensionContext),
        "{issues:#?}"
    );
}

/// `binary-ref-internal`: a contained resource referenced only from the
/// narrative (`<img src="#imageResource">`) satisfies dom-3.
#[tokio::test]
async fn contained_resources_referenced_from_narrative() {
    let composition = |src: &str| {
        json!({"resourceType": "Composition", "status": "final", "type": {"text": "x"},
               "date": "2021-02-01", "author": [{"display": "y"}], "title": "z",
               "contained": [{"resourceType": "Binary", "id": "imageResource",
                              "contentType": "image/png", "data": "iVBORw0KGgo="}],
               "section": [{"text": {"status": "additional",
                   "div": format!("<div xmlns=\"http://www.w3.org/1999/xhtml\"><img src=\"{src}\" alt=\"an image\"/></div>")}}]})
    };
    let issues = validate_with(&composition("#imageResource"), &[], &[]).await;
    assert!(
        constraint_failures(&issues, "dom-3").is_empty(),
        "{issues:#?}"
    );
    let issues = validate_with(&composition("#other"), &[], &[]).await;
    assert_eq!(
        constraint_failures(&issues, "dom-3").len(),
        1,
        "{issues:#?}"
    );
}

/// `document-bad-no-sections`: slices with a minimum (and required
/// extensions) are missing when their element is absent altogether.
#[tokio::test]
async fn slice_minimums_on_absent_elements() {
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/sections",
        "name": "Sections", "status": "active", "kind": "resource", "abstract": false,
        "type": "Composition", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Composition",
        "differential": {"element": [
            {"id": "Composition.section", "path": "Composition.section",
             "slicing": {"discriminator": [{"type": "pattern", "path": "code"}], "rules": "open"}},
            {"id": "Composition.section:code-A", "path": "Composition.section", "sliceName": "code-A", "min": 1},
            {"id": "Composition.section:code-A.code", "path": "Composition.section.code",
             "patternCodeableConcept": {"coding": [{"system": "http://example.org/cs", "code": "A"}]}},
            {"id": "Composition.extension", "path": "Composition.extension", "slicing": {
                "discriminator": [{"type": "value", "path": "url"}], "rules": "open"}},
            {"id": "Composition.extension:version", "path": "Composition.extension", "sliceName": "version",
             "min": 1, "type": [{"code": "Extension",
                "profile": ["http://hl7.org/fhir/StructureDefinition/composition-clinicaldocument-versionNumber"]}]}
        ]}
    });
    let composition = json!({"resourceType": "Composition", "status": "final", "type": {"text": "x"},
        "date": "2021-02-01", "author": [{"display": "y"}], "title": "z"});
    let issues = validate_with(
        &composition,
        &[sd],
        &["http://example.org/StructureDefinition/sections"],
    )
    .await;
    let slices: Vec<&str> = issues
        .iter()
        .filter(|e| e.kind == helios_fhir_validator::ErrorKind::SliceCardinality)
        .map(|e| e.path.as_str())
        .collect();
    assert_eq!(
        slices,
        vec!["Composition.section", "Composition.extension"],
        "{issues:#?}"
    );
}

/// `patient-extension-complex-bad2`, `maiden-name-extension`: a complex
/// extension's undefined sub-extension, and an extension whose context is
/// a datatype element (`HumanName.family`) placed elsewhere.
#[tokio::test]
async fn complex_extensions_and_datatype_element_contexts() {
    let animal = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/animal",
        "name": "Animal", "status": "active", "kind": "complex-type", "abstract": false,
        "type": "Extension", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Extension",
        "context": [{"type": "element", "expression": "Patient"}],
        "differential": {"element": [
            {"id": "Extension.extension", "path": "Extension.extension",
             "slicing": {"discriminator": [{"type": "value", "path": "url"}], "rules": "open"}},
            {"id": "Extension.extension:species", "path": "Extension.extension", "sliceName": "species", "min": 1},
            {"id": "Extension.extension:species.url", "path": "Extension.extension.url", "fixedUri": "species"},
            {"id": "Extension.extension:species.value[x]", "path": "Extension.extension.value[x]",
             "type": [{"code": "CodeableConcept"}]},
            {"id": "Extension.url", "path": "Extension.url", "fixedUri": "http://example.org/StructureDefinition/animal"},
            {"id": "Extension.value[x]", "path": "Extension.value[x]", "max": "0"}
        ]}
    });
    let family = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/mothers-family",
        "name": "MothersFamily", "status": "active", "kind": "complex-type", "abstract": false,
        "type": "Extension", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Extension",
        "context": [{"type": "element", "expression": "HumanName.family"}],
        "differential": {"element": [
            {"id": "Extension.url", "path": "Extension.url",
             "fixedUri": "http://example.org/StructureDefinition/mothers-family"},
            {"id": "Extension.value[x]", "path": "Extension.value[x]", "type": [{"code": "string"}]}
        ]}
    });
    let run = |resource: Value| {
        let sds = [animal.clone(), family.clone()];
        async move {
            let core = core_registry(FhirVersion::R4);
            let mut overlay = SchemaRegistry::new();
            for sd in &sds {
                overlay.insert(convert(sd).unwrap().schema);
            }
            let resolver: Arc<dyn SchemaResolver> =
                Arc::new(CompositeResolver::new(vec![Arc::new(overlay), core]));
            let opts = ValidationOptions {
                check_extension_context: true,
                ..Default::default()
            };
            Validator::new(resolver)
                .validate_sync(&resource, &opts)
                .errors
                .into_iter()
                .filter(|e| e.severity == Severity::Error)
                .map(|e| e.message)
                .collect::<Vec<_>>()
        }
    };
    let species = json!({"url": "species", "valueCodeableConcept": {"text": "dog"}});
    let ok = json!({"resourceType": "Patient", "extension": [{
        "url": "http://example.org/StructureDefinition/animal", "extension": [species.clone()]}]});
    assert!(run(ok).await.is_empty());
    let bad = json!({"resourceType": "Patient", "extension": [{
        "url": "http://example.org/StructureDefinition/animal",
        "extension": [species, {"url": "species-x", "valueString": "x"}]}]});
    let errs = run(bad).await;
    assert!(
        errs.iter()
            .any(|m| m.starts_with("Sub-extension url 'species-x'")),
        "{errs:#?}"
    );

    let mf =
        json!({"url": "http://example.org/StructureDefinition/mothers-family", "valueString": "x"});
    let on_family = json!({"resourceType": "Patient", "name": [{"family": "Doe",
        "_family": {"extension": [mf.clone()]}}]});
    assert!(run(on_family).await.is_empty());
    let on_name =
        json!({"resourceType": "Patient", "name": [{"family": "Doe", "extension": [mf]}]});
    let errs = run(on_name).await;
    assert!(
        errs.iter()
            .any(|m| m.contains("is not allowed to be used at this point")),
        "{errs:#?}"
    );
}

/// `obs-fio2`, `obs-percent`: the vital signs profiles bind
/// `component.value[x]` (Quantity|CodeableConcept|...) to their unit value
/// set; the HL7 validator does not hold a Quantity of that multi-type element
/// to it (`fixed-quantity-binding-observation`: a Quantity-only element is).
#[test]
fn multi_type_value_bindings_skip_quantities() {
    let obs = json!({"resourceType": "Observation", "status": "final",
        "category": [{"coding": [{"system": "http://terminology.hl7.org/CodeSystem/observation-category",
                                  "code": "vital-signs"}]}],
        "code": {"coding": [{"system": "http://loinc.org", "code": "2708-6"}]},
        "subject": {"reference": "Patient/p"}, "effectiveDateTime": "2020-10-11",
        "valueQuantity": {"value": 98, "system": "http://unitsofmeasure.org", "code": "%", "unit": "%"},
        "component": [{"code": {"coding": [{"system": "http://loinc.org", "code": "3151-8"}]},
            "valueQuantity": {"value": 2, "system": "http://unitsofmeasure.org", "code": "L/min"}}]});
    let validator = Validator::new(core_registry(FhirVersion::R4));
    let outcome = validator.validate_sync(&obs, &ValidationOptions::default());
    let deferred_on_component_value = outcome.deferred.iter().any(|d| {
        matches!(d, helios_fhir_validator::Deferred::Binding { path, .. }
            if path == "Observation.component.0.valueQuantity")
    });
    assert!(!deferred_on_component_value, "{:?}", outcome.deferred);
}

/// `ext-ctxt-bad-address`: a FHIRPath extension context
/// (`Patient.address.where(use = 'home')`) is evaluated on the extension's
/// parent.
#[tokio::test]
async fn fhirpath_extension_contexts_are_evaluated() {
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/home-only",
        "name": "HomeOnly", "status": "active", "kind": "complex-type", "abstract": false,
        "type": "Extension", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Extension",
        "context": [{"type": "fhirpath", "expression": "Patient.address.where(use = 'home')"}],
        "differential": {"element": [
            {"id": "Extension.url", "path": "Extension.url",
             "fixedUri": "http://example.org/StructureDefinition/home-only"},
            {"id": "Extension.value[x]", "path": "Extension.value[x]", "type": [{"code": "boolean"}]}
        ]}
    });
    let run = |address_use: &'static str| {
        let sd = sd.clone();
        async move {
            let core = core_registry(FhirVersion::R4);
            let mut overlay = SchemaRegistry::new();
            overlay.insert(convert(&sd).unwrap().schema);
            let resolver: Arc<dyn SchemaResolver> =
                Arc::new(CompositeResolver::new(vec![Arc::new(overlay), core]));
            let evaluator = FhirPathConstraintEvaluator::new();
            let handlers = EffectHandlers {
                constraints: Some(&evaluator),
                ..Default::default()
            };
            let opts = ValidationOptions {
                check_extension_context: true,
                ..Default::default()
            };
            let patient = json!({"resourceType": "Patient", "address": [{"use": address_use, "city": "x",
                "extension": [{"url": "http://example.org/StructureDefinition/home-only", "valueBoolean": true}]}]});
            Validator::new(resolver)
                .validate(&patient, FhirVersion::R4, &opts, &handlers)
                .await
                .into_iter()
                .filter(|e| e.severity == Severity::Error)
                .count()
        }
    };
    assert_eq!(run("home").await, 0);
    assert_eq!(run("work").await, 1);
}

/// `exta-ctxt-bad-name`, `extb-ctxt-bad`: abstract contexts (DomainResource,
/// DomainResource.text, BackboneElement) and a contextInvariant.
#[tokio::test]
async fn abstract_contexts_and_context_invariants() {
    let ext = |url: &str, contexts: Value, invariant: Option<&str>| {
        let mut sd = json!({
            "resourceType": "StructureDefinition", "url": url, "name": "X", "status": "active",
            "kind": "complex-type", "abstract": false, "type": "Extension", "derivation": "constraint",
            "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Extension",
            "context": contexts,
            "differential": {"element": [
                {"id": "Extension.url", "path": "Extension.url", "fixedUri": url},
                {"id": "Extension.value[x]", "path": "Extension.value[x]", "type": [{"code": "boolean"}]}]}});
        if let Some(i) = invariant {
            sd["contextInvariant"] = json!([i]);
        }
        sd
    };
    let a = "http://example.org/StructureDefinition/exta";
    let b = "http://example.org/StructureDefinition/extb";
    let sds = vec![
        ext(
            a,
            json!([{"type": "element", "expression": "DomainResource"},
                      {"type": "element", "expression": "DomainResource.text"},
                      {"type": "element", "expression": "BackboneElement"}]),
            None,
        ),
        ext(
            b,
            json!([{"type": "element", "expression": "Patient"}]),
            Some("Patient.active.not()"),
        ),
    ];
    let run = |patient: Value| {
        let sds = sds.clone();
        async move {
            let core = core_registry(FhirVersion::R4);
            let mut overlay = SchemaRegistry::new();
            for sd in &sds {
                overlay.insert(convert(sd).unwrap().schema);
            }
            let resolver: Arc<dyn SchemaResolver> =
                Arc::new(CompositeResolver::new(vec![Arc::new(overlay), core]));
            let evaluator = FhirPathConstraintEvaluator::new();
            let handlers = EffectHandlers {
                constraints: Some(&evaluator),
                ..Default::default()
            };
            let opts = ValidationOptions {
                check_extension_context: true,
                ..Default::default()
            };
            Validator::new(resolver)
                .validate(&patient, FhirVersion::R4, &opts, &handlers)
                .await
                .into_iter()
                .filter(|e| e.severity == Severity::Error)
                .count()
        }
    };
    let e = |url: &str| json!([{"url": url, "valueBoolean": true}]);
    assert_eq!(
        run(json!({"resourceType": "Patient", "extension": e(a)})).await,
        0
    );
    assert_eq!(run(json!({"resourceType": "Patient",
        "text": {"status": "generated", "div": "<div xmlns=\"http://www.w3.org/1999/xhtml\">x</div>",
                 "extension": e(a)}})).await, 0);
    assert_eq!(run(json!({"resourceType": "Patient", "contact": [{"extension": e(a), "name": {"family": "y"}}]})).await, 0);
    assert_eq!(
        run(json!({"resourceType": "Patient", "name": [{"family": "x", "extension": e(a)}]})).await,
        1
    );
    assert_eq!(
        run(json!({"resourceType": "Patient", "active": false, "extension": e(b)})).await,
        0
    );
    assert_eq!(
        run(json!({"resourceType": "Patient", "active": true, "extension": e(b)})).await,
        1
    );
}

/// `patient-min-length`: ElementDefinition.maxLength and the minLength
/// extension bound string lengths.
#[tokio::test]
async fn string_length_bounds() {
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/lengths",
        "name": "Lengths", "status": "active", "kind": "resource", "abstract": false,
        "type": "Patient", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
        "differential": {"element": [
            {"id": "Patient.id", "path": "Patient.id", "extension": [{
                "url": "http://hl7.org/fhir/StructureDefinition/minLength", "valueInteger": 3}]},
            {"id": "Patient.name.family", "path": "Patient.name.family", "maxLength": 5}
        ]}
    });
    let url = "http://example.org/StructureDefinition/lengths";
    let count = |issues: Vec<ValidationError>| errors(&issues).len();
    let p = |id: &str, family: &str| json!({"resourceType": "Patient", "id": id, "name": [{"family": family}]});
    assert_eq!(
        count(validate_with(&p("abc", "Smith"), &[sd.clone()], &[url]).await),
        0
    );
    assert_eq!(
        count(validate_with(&p("ab", "Smith"), &[sd.clone()], &[url]).await),
        1
    );
    assert_eq!(
        count(validate_with(&p("abc", "Smithers"), &[sd], &[url]).await),
        1
    );
}

/// `patient-id-only`: a primitive with only an id (no value, no
/// extension) breaks ele-1.
#[test]
fn primitive_with_only_an_id() {
    let errs =
        structural_errors(&json!({"resourceType": "Patient", "_implicitRules": {"id": "i1"}}));
    assert_eq!(errs.len(), 1, "{errs:#?}");
    assert_eq!(errs[0].0, "Patient.implicitRules");
    assert!(
        structural_errors(
            &json!({"resourceType": "Patient", "implicitRules": "http://x.org",
        "_implicitRules": {"id": "i1"}})
        )
        .is_empty()
    );
}

/// `bnd-slicing-cgm`: an item matching two slices of one slicing.
#[tokio::test]
async fn an_item_matching_two_slices() {
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/overlap",
        "name": "Overlap", "status": "active", "kind": "resource", "abstract": false,
        "type": "Observation", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Observation",
        "differential": {"element": [
            {"id": "Observation.category", "path": "Observation.category", "slicing": {
                "discriminator": [{"type": "pattern", "path": "$this"}], "rules": "open"}},
            {"id": "Observation.category:a", "path": "Observation.category", "sliceName": "a",
             "patternCodeableConcept": {"coding": [{"code": "x"}]}},
            {"id": "Observation.category:b", "path": "Observation.category", "sliceName": "b",
             "patternCodeableConcept": {"coding": [{"system": "http://example.org/cs"}]}}
        ]}
    });
    let obs = |coding: Value| {
        json!({"resourceType": "Observation", "status": "final",
        "code": {"text": "t"}, "category": [{"coding": [coding]}]})
    };
    let url = "http://example.org/StructureDefinition/overlap";
    let both = validate_with(
        &obs(json!({"system": "http://example.org/cs", "code": "x"})),
        &[sd.clone()],
        &[url],
    )
    .await;
    assert!(
        errors(&both)
            .iter()
            .any(|e| e.message.contains("matches more than one slice")),
        "{both:#?}"
    );
    let one = validate_with(
        &obs(json!({"system": "http://example.org/cs", "code": "y"})),
        &[sd],
        &[url],
    )
    .await;
    assert!(errors(&one).is_empty(), "{one:#?}");
}

/// `ad-practitioner-resource`: a fixed complex value is judged per child.
#[tokio::test]
async fn fixed_complex_values_per_child() {
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/home-telecom",
        "name": "HomeTelecom", "status": "active", "kind": "resource", "abstract": false,
        "type": "Practitioner", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Practitioner",
        "differential": {"element": [{"id": "Practitioner.telecom", "path": "Practitioner.telecom",
            "fixedContactPoint": {"use": "home"}}]}
    });
    let url = "http://example.org/StructureDefinition/home-telecom";
    let p = |t: Value| json!({"resourceType": "Practitioner", "telecom": [t]});
    let issues = validate_with(
        &p(json!({"system": "phone", "value": "555", "use": "home"})),
        &[sd.clone()],
        &[url],
    )
    .await;
    let paths: Vec<String> = errors(&issues).iter().map(|e| e.path.clone()).collect();
    assert_eq!(
        paths,
        vec![
            "Practitioner.telecom.0.system",
            "Practitioner.telecom.0.value"
        ],
        "{issues:#?}"
    );
    assert!(errors(&validate_with(&p(json!({"use": "home"})), &[sd], &[url]).await).is_empty());
}

/// `member-of-CC-*`: `memberOf()` in an invariant is answered by the
/// validator's terminology.
#[tokio::test]
async fn member_of_in_invariants() {
    struct Vs;
    #[async_trait::async_trait]
    impl helios_fhir_validator::TerminologyProvider for Vs {
        async fn validate_code(
            &self,
            value_set: &str,
            coded: &helios_fhir_validator::CodedValue,
        ) -> Result<bool, helios_fhir_validator::TerminologyError> {
            let helios_fhir_validator::CodedValue::CodeableConcept(cc) = coded else {
                return Ok(false);
            };
            Ok(value_set == "http://example.org/ValueSet/vs" && cc["coding"][0]["code"] == "good")
        }
    }
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/member-of",
        "name": "MemberOf", "status": "active", "kind": "resource", "abstract": false,
        "type": "Basic", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Basic",
        "differential": {"element": [{"id": "Basic.code", "path": "Basic.code",
            "constraint": [{"key": "bc-mm-1", "severity": "error", "human": "must be in a value set",
                "expression": "memberOf('http://example.org/ValueSet/vs')"}]}]}
    });
    let mut overlay = SchemaRegistry::new();
    assert!(overlay.insert(convert(&sd).expect("profile converts").schema));
    let resolver: Arc<dyn SchemaResolver> = Arc::new(CompositeResolver::new(vec![
        Arc::new(overlay),
        core_registry(FhirVersion::R4),
    ]));
    let validator = Validator::new(resolver);
    let evaluator = FhirPathConstraintEvaluator::new();
    let suppress = vec!["dom-6".to_string()];
    let handlers = EffectHandlers {
        constraints: Some(&evaluator),
        terminology: Some(&Vs),
        suppress_constraints: &suppress,
        ..Default::default()
    };
    let opts = ValidationOptions {
        profiles: vec!["http://example.org/StructureDefinition/member-of".into()],
        ..Default::default()
    };
    let basic = |code: &str| {
        json!({"resourceType": "Basic",
        "code": {"coding": [{"system": "http://example.org/cs", "code": code}]}})
    };
    let bad = validator
        .validate(&basic("bad"), FhirVersion::R4, &opts, &handlers)
        .await;
    let failed: Vec<_> = errors(&bad)
        .into_iter()
        .filter(|e| e.message.contains("bc-mm-1"))
        .collect();
    assert_eq!(failed.len(), 1, "{bad:#?}");
    assert_eq!(failed[0].path, "Basic.code");
    let good = validator
        .validate(&basic("good"), FhirVersion::R4, &opts, &handlers)
        .await;
    assert!(
        !good.iter().any(|e| e.message.contains("bc-mm-1")),
        "{good:#?}"
    );
}

/// `toplevel-minvalueduration-*`, `toplevel-maxvalueduration-*`: a Duration
/// minValue/maxValue on a date bounds it relative to now.
#[tokio::test]
async fn duration_relative_bounds() {
    let sd = |key: &str, years: u32| {
        json!({
            "resourceType": "StructureDefinition",
            "url": format!("http://example.org/StructureDefinition/{key}"),
            "name": "Bound", "status": "active", "kind": "resource", "abstract": false,
            "type": "Patient", "derivation": "constraint",
            "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
            "differential": {"element": [{"id": "Patient.birthDate", "path": "Patient.birthDate",
                key: {"value": years, "unit": "a", "system": "http://unitsofmeasure.org", "code": "a"}}]}
        })
    };
    let p = |d: &str| json!({"resourceType": "Patient", "birthDate": d});
    for (key, years, bad, good) in [
        ("minValueDuration", 100, "1850-01-01", "1950-01-01"),
        ("maxValueDuration", 10, "2100-01-01", "2020-01-01"),
    ] {
        let url = format!("http://example.org/StructureDefinition/{key}");
        let issues = validate_with(&p(bad), &[sd(key, years)], &[&url]).await;
        let errs = errors(&issues);
        assert_eq!(errs.len(), 1, "{issues:#?}");
        assert!(errs[0].message.contains("is not legal"), "{issues:#?}");
        assert!(errors(&validate_with(&p(good), &[sd(key, years)], &[&url]).await).is_empty());
    }
}

/// `ips-htmlrefs-backwards`: an extension value of a type the definition
/// does not allow is reported on the extension, naming the allowed types.
#[tokio::test]
async fn extension_value_of_a_disallowed_type() {
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/link",
        "name": "Link", "status": "active", "kind": "complex-type", "abstract": false,
        "context": [{"type": "element", "expression": "Element"}],
        "type": "Extension", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Extension",
        "differential": {"element": [
            {"id": "Extension", "path": "Extension"},
            {"id": "Extension.url", "path": "Extension.url", "fixedUri": "http://example.org/StructureDefinition/link"},
            {"id": "Extension.value[x]", "path": "Extension.value[x]", "type": [{"code": "url"}]}]}
    });
    let patient = json!({"resourceType": "Patient", "extension": [
        {"url": "http://example.org/StructureDefinition/link", "valueUri": "#a"}]});
    let issues = validate_with(&patient, &[sd], &[]).await;
    let errs = errors(&issues);
    assert!(
        errs.iter().any(|e| e.path == "Patient.extension.0"
            && e.message == "The Extension 'http://example.org/StructureDefinition/link' definition allows for the types [url] but found type uri"),
        "{issues:#?}"
    );
}

/// `inv-checks#profile`: a profile labelled `specialization` of its own
/// base type is still a profile.
#[tokio::test]
async fn specialization_of_the_same_type_is_a_profile() {
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/named",
        "name": "Named", "status": "active", "kind": "resource", "abstract": false,
        "type": "Patient", "derivation": "specialization",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
        "differential": {"element": [{"id": "Patient.name", "path": "Patient.name", "min": 1}]}
    });
    let issues = validate_with(
        &json!({"resourceType": "Patient"}),
        &[sd],
        &["http://example.org/StructureDefinition/named"],
    )
    .await;
    assert_eq!(errors(&issues).len(), 1, "{issues:#?}");
}

/// `document-bad-no-sections#profile`: a slice typed by an element of a
/// library profile (`elementdefinition-profile-element`) matches what that
/// element's slice matches, and keeps its minimum.
#[tokio::test]
async fn slices_from_a_library_profile() {
    let library = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/library",
        "name": "Library", "status": "active", "kind": "resource", "abstract": true,
        "type": "Composition", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Composition",
        "differential": {"element": [
            {"path": "Composition"},
            {"path": "Composition.section", "slicing": {"discriminator": [{"type": "pattern", "path": "code"}], "rules": "closed"}},
            {"path": "Composition.section", "sliceName": "codeA"},
            {"path": "Composition.section.code", "min": 1,
                "patternCodeableConcept": {"coding": [{"system": "http://example.org/cs", "code": "a"}]}}]}
    });
    let structure = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/structure",
        "name": "Structure", "status": "active", "kind": "resource", "abstract": false,
        "type": "Composition", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Composition",
        "differential": {"element": [
            {"path": "Composition"},
            {"path": "Composition.section", "slicing": {"discriminator": [{"type": "pattern", "path": "code"}], "rules": "open"}},
            {"path": "Composition.section", "sliceName": "code-A", "min": 1,
                "type": [{"code": "BackboneElement",
                    "profile": ["http://example.org/StructureDefinition/library"],
                    "_profile": [{"extension": [{"url": "http://hl7.org/fhir/StructureDefinition/elementdefinition-profile-element",
                        "valueString": "Composition.section:codeA"}]}]}]}]}
    });
    let composition = |code: &str| {
        json!({
            "resourceType": "Composition", "status": "final",
            "type": {"text": "doc"}, "date": "2024-01-01", "title": "t", "author": [{"display": "a"}],
            "section": [{"title": "s", "code": {"coding": [{"system": "http://example.org/cs", "code": code}]}, "text": {"status": "generated", "div": "<div xmlns=\"http://www.w3.org/1999/xhtml\">x</div>"}}]
        })
    };
    let url = "http://example.org/StructureDefinition/structure";
    let sds = [library, structure];
    let bad = validate_with(&composition("b"), &sds, &[url]).await;
    assert!(
        errors(&bad).iter().any(|e| e
            .hl7_message
            .as_deref()
            .is_some_and(|m| m.contains("code-A"))),
        "{bad:#?}"
    );
    let good = validate_with(&composition("a"), &sds, &[url]).await;
    assert!(errors(&good).is_empty(), "{good:#?}");
}

/// `patient-extension-bad2`: an empty primitive (`url=""`) has neither a
/// value nor children, so ele-1 fails as well.
#[test]
fn empty_primitives_break_ele1() {
    let errs = structural_errors(&json!({"resourceType": "Patient", "extension": [
        {"url": "", "valueString": "x"}]}));
    let at_url: Vec<&str> = errs
        .iter()
        .filter(|e| e.0 == "Patient.extension.0.url")
        .map(|e| e.1.as_str())
        .collect();
    assert_eq!(
        at_url,
        vec![
            "value cannot be empty",
            "Constraint failed: ele-1: 'All FHIR elements must have a @value or children'"
        ],
        "{errs:#?}"
    );
}

/// `type-subtype-slicing3#profile`: a slice that sets only one of two
/// pattern discriminators matches on that one.
#[tokio::test]
async fn slice_setting_one_of_two_discriminators() {
    let pattern =
        |code: &str| json!({"coding": [{"system": "http://example.org/cs", "code": code}]});
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/ranges",
        "name": "Ranges", "status": "active", "kind": "resource", "abstract": false,
        "type": "Observation", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Observation",
        "differential": {"element": [
            {"id": "Observation.referenceRange", "path": "Observation.referenceRange", "slicing": {
                "discriminator": [{"type": "pattern", "path": "type"}, {"type": "pattern", "path": "appliesTo"}],
                "rules": "open"}},
            {"id": "Observation.referenceRange:s3", "path": "Observation.referenceRange", "sliceName": "s3", "min": 1, "max": "1"},
            {"id": "Observation.referenceRange:s3.type", "path": "Observation.referenceRange.type", "patternCodeableConcept": pattern("t")}
        ]}
    });
    let range = |a: &str| json!({"type": pattern("t"), "appliesTo": [pattern(a)], "text": "x"});
    let obs = json!({"resourceType": "Observation", "status": "final", "code": {"text": "c"},
        "referenceRange": [range("a"), range("b")]});
    let issues = validate_with(
        &obs,
        &[sd],
        &["http://example.org/StructureDefinition/ranges"],
    )
    .await;
    let errs = errors(&issues);
    assert_eq!(errs.len(), 1, "{issues:#?}");
    assert!(
        errs[0]
            .hl7_message
            .as_deref()
            .is_some_and(|m| m.contains("max allowed = 1, but found 2")),
        "{issues:#?}"
    );
}

/// `fixed-quantity-binding-observation#profile`: a profile that narrows
/// value[x] to Quantity and binds it judges the Quantity's units.
#[test]
fn narrowed_quantity_value_binding_applies() {
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/aged",
        "name": "Aged", "status": "active", "kind": "resource", "abstract": false,
        "type": "Observation", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Observation",
        "differential": {"element": [{"id": "Observation.value[x]", "path": "Observation.value[x]",
            "type": [{"code": "Quantity"}],
            "binding": {"strength": "required", "valueSet": "http://hl7.org/fhir/ValueSet/age-units"}}]}
    });
    let mut overlay = SchemaRegistry::new();
    assert!(overlay.insert(convert(&sd).expect("profile converts").schema));
    let resolver: Arc<dyn SchemaResolver> = Arc::new(CompositeResolver::new(vec![
        Arc::new(overlay),
        core_registry(FhirVersion::R4),
    ]));
    let obs = json!({"resourceType": "Observation", "status": "final", "code": {"text": "age"},
        "valueQuantity": {"value": 3, "system": "http://unitsofmeasure.org", "code": "m"}});
    let opts = ValidationOptions {
        profiles: vec!["http://example.org/StructureDefinition/aged".into()],
        ..Default::default()
    };
    let outcome = Validator::new(resolver).validate_sync(&obs, &opts);
    assert!(
        outcome.deferred.iter().any(
            |d| matches!(d, helios_fhir_validator::Deferred::Binding { path, .. }
            if path == "Observation.valueQuantity")
        ),
        "{:?}",
        outcome.deferred
    );
}

/// `additional-bindings-observation-2#profile`: a required additional
/// binding is checked.
#[test]
fn required_additional_binding() {
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/additional",
        "name": "Additional", "status": "active", "kind": "resource", "abstract": false,
        "type": "Observation", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Observation",
        "differential": {"element": [{"id": "Observation.code", "path": "Observation.code",
            "binding": {"strength": "example", "valueSet": "http://hl7.org/fhir/ValueSet/observation-codes",
                "extension": [{"url": "http://hl7.org/fhir/5.0/StructureDefinition/extension-ElementDefinition.binding.additional",
                    "extension": [{"url": "purpose", "valueCode": "required"},
                                  {"url": "valueSet", "valueCanonical": "http://example.org/ValueSet/vs1"}]}]}}]}
    });
    let schema = convert(&sd).expect("profile converts").schema;
    let code = &schema.elements.as_ref().unwrap()["code"];
    let binding = code.binding.as_ref().expect("binding");
    assert_eq!(binding.value_set, "http://example.org/ValueSet/vs1");
    assert_eq!(binding.strength.as_deref(), Some("required"));
}

/// IPS Bundle entries: slices matched by profile conformance may overlap
/// (structural matching is looser than the HL7 validator's full check), so
/// overlap among them is not reported.
#[tokio::test]
async fn profile_matched_slices_may_overlap() {
    let obs_profile = |name: &str| {
        json!({
            "resourceType": "StructureDefinition",
            "url": format!("http://example.org/StructureDefinition/{name}"),
            "name": name, "status": "active", "kind": "resource", "abstract": false,
            "type": "Observation", "derivation": "constraint",
            "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Observation",
            "differential": {"element": [{"id": "Observation.status", "path": "Observation.status", "mustSupport": true}]}
        })
    };
    let slice = |name: &str| {
        vec![
            json!({"id": format!("Bundle.entry:{name}"), "path": "Bundle.entry", "sliceName": name}),
            json!({"id": format!("Bundle.entry:{name}.resource"), "path": "Bundle.entry.resource",
            "type": [{"code": "Observation", "profile": [format!("http://example.org/StructureDefinition/{name}")]}]}),
        ]
    };
    let mut elements = vec![json!({"id": "Bundle.entry", "path": "Bundle.entry",
        "slicing": {"discriminator": [{"type": "type", "path": "resource"}, {"type": "profile", "path": "resource"}], "rules": "open"}})];
    elements.extend(slice("a"));
    elements.extend(slice("b"));
    let bundle_sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/bundle",
        "name": "B", "status": "active", "kind": "resource", "abstract": false,
        "type": "Bundle", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Bundle",
        "differential": {"element": elements}
    });
    let bundle = json!({"resourceType": "Bundle", "type": "collection", "entry": [
        {"fullUrl": "urn:uuid:7f1c0b9e-1d7b-4c3a-9a51-2f3b8e6d4c10",
         "resource": {"resourceType": "Observation", "status": "final", "code": {"text": "x"}}}]});
    let issues = validate_with(
        &bundle,
        &[obs_profile("a"), obs_profile("b"), bundle_sd],
        &["http://example.org/StructureDefinition/bundle"],
    )
    .await;
    assert!(
        !issues
            .iter()
            .any(|e| e.message.contains("more than one slice")),
        "{issues:#?}"
    );
}

/// `ch/mixed-valueset-dr#profile`: the maxSize extension caps an
/// Attachment's data.
#[tokio::test]
async fn attachment_max_size() {
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/small",
        "name": "Small", "status": "active", "kind": "resource", "abstract": false,
        "type": "DocumentReference", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/DocumentReference",
        "differential": {"element": [{"id": "DocumentReference.content.attachment",
            "path": "DocumentReference.content.attachment",
            "extension": [{"url": "http://hl7.org/fhir/StructureDefinition/maxSize", "valueDecimal": 15}]}]}
    });
    // "Hello World Data!" = 17 bytes
    let doc = json!({"resourceType": "DocumentReference", "status": "current",
        "content": [{"attachment": {"contentType": "text/plain", "data": "SGVsbG8gV29ybGQgRGF0YSE="}}]});
    let issues = validate_with(
        &doc,
        &[sd],
        &["http://example.org/StructureDefinition/small"],
    )
    .await;
    let errs = errors(&issues);
    assert!(
        errs.iter().any(|e| e.message
            == "Attachment size is 17 bytes which exceeds the stated limit of 15 bytes"),
        "{issues:#?}"
    );
}

/// `versioned-extension`: an extension url carrying a version is an error.
#[test]
fn versioned_extension_urls() {
    let errs = structural_errors(&json!({"resourceType": "Patient", "extension": [
        {"url": "http://hl7.org/fhir/StructureDefinition/patient-congregation|4.0.0", "valueString": "x"}]}));
    assert!(
        errs.iter().any(|e| e.0 == "Patient.extension.0"
            && e.1 == "The extension URL must not contain a version"),
        "{errs:#?}"
    );
}

/// `bundle-conformsto`, difftest `Bundle-example-PMR`: a Bundle-internal
/// reference target must conform to one of the Reference's target
/// profiles.
#[tokio::test]
async fn bundled_reference_targets_meet_target_profiles() {
    let patient_profile = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/identified-patient",
        "name": "IdentifiedPatient", "status": "active", "kind": "resource", "abstract": false,
        "type": "Patient", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
        "differential": {"element": [{"id": "Patient.identifier", "path": "Patient.identifier", "min": 1}]}
    });
    let obs_profile = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/obs",
        "name": "Obs", "status": "active", "kind": "resource", "abstract": false,
        "type": "Observation", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Observation",
        "differential": {"element": [{"id": "Observation.subject", "path": "Observation.subject",
            "type": [{"code": "Reference", "targetProfile": ["http://example.org/StructureDefinition/identified-patient"]}]}]}
    });
    let bundle = |patient: Value| {
        json!({"resourceType": "Bundle", "type": "collection", "entry": [
        {"fullUrl": "http://example.org/fhir/Observation/o", "resource": {"resourceType": "Observation", "id": "o",
            "meta": {"profile": ["http://example.org/StructureDefinition/obs"]},
            "status": "final", "code": {"text": "x"}, "subject": {"reference": "Patient/p"}}},
        {"fullUrl": "http://example.org/fhir/Patient/p", "resource": patient}]})
    };
    let sds = [patient_profile, obs_profile];
    let bad = validate_with(
        &bundle(json!({"resourceType": "Patient", "id": "p"})),
        &sds,
        &[],
    )
    .await;
    assert!(
        errors(&bad).iter().any(|e| e
            .message
            .starts_with("Unable to find a profile match for Patient/p")),
        "{bad:#?}"
    );
    let good = validate_with(
        &bundle(json!({"resourceType": "Patient", "id": "p", "identifier": [{"value": "1"}]})),
        &sds,
        &[],
    )
    .await;
    assert!(errors(&good).is_empty(), "{good:#?}");
}

/// `patient-bad-gender#profile`: FHIRPath `conformsTo()` in an invariant.
#[tokio::test]
async fn conforms_to_in_invariants() {
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/conform",
        "name": "Conform", "status": "active", "kind": "resource", "abstract": false,
        "type": "Patient", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
        "differential": {"element": [{"id": "Patient", "path": "Patient",
            "constraint": [{"key": "ck-1", "severity": "error", "human": "conformsToCheck",
                "expression": "conformsTo('http://hl7.org/fhir/StructureDefinition/Patient')"}]}]}
    });
    let url = "http://example.org/StructureDefinition/conform";
    let ck1 = |issues: &[ValidationError]| issues.iter().any(|e| e.message.contains("ck-1"));
    let bad = validate_with(
        &json!({"resourceType": "Patient", "gender": "bad"}),
        &[sd.clone()],
        &[url],
    )
    .await;
    assert!(ck1(&bad), "{bad:#?}");
    let good = validate_with(
        &json!({"resourceType": "Patient", "gender": "male"}),
        &[sd],
        &[url],
    )
    .await;
    assert!(!ck1(&good), "{good:#?}");
}

/// `bnd-slicing-cgm#profile`: a discriminator value defined only in the
/// slices' type profiles (`resource.ofType(Observation).code`).
#[tokio::test]
async fn discriminator_values_from_type_profiles() {
    let obs_profile = |name: &str, code: &str| {
        json!({
            "resourceType": "StructureDefinition",
            "url": format!("http://example.org/StructureDefinition/{name}"),
            "name": name, "status": "active", "kind": "resource", "abstract": false,
            "type": "Observation", "derivation": "constraint",
            "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Observation",
            "differential": {"element": [{"id": "Observation.code", "path": "Observation.code",
                "patternCodeableConcept": {"coding": [{"system": "http://example.org/cs", "code": code}]}}]}
        })
    };
    let slice = |name: &str| {
        vec![
            json!({"id": format!("Bundle.entry:{name}"), "path": "Bundle.entry", "sliceName": name, "min": 1}),
            json!({"id": format!("Bundle.entry:{name}.resource"), "path": "Bundle.entry.resource",
            "type": [{"code": "Observation", "profile": [format!("http://example.org/StructureDefinition/{name}")]}]}),
        ]
    };
    let mut elements = vec![json!({"id": "Bundle.entry", "path": "Bundle.entry",
        "slicing": {"discriminator": [{"type": "type", "path": "resource"},
                                      {"type": "value", "path": "resource.ofType(Observation).code"}], "rules": "open"}})];
    elements.extend(slice("a"));
    elements.extend(slice("b"));
    let bundle_sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/obs-bundle",
        "name": "ObsBundle", "status": "active", "kind": "resource", "abstract": false,
        "type": "Bundle", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Bundle",
        "differential": {"element": elements}
    });
    let bundle = json!({"resourceType": "Bundle", "type": "collection", "entry": [
        {"fullUrl": "urn:uuid:7f1c0b9e-1d7b-4c3a-9a51-2f3b8e6d4c11",
         "resource": {"resourceType": "Observation", "status": "final",
            "code": {"coding": [{"system": "http://example.org/cs", "code": "a"}]}}}]});
    let issues = validate_with(
        &bundle,
        &[obs_profile("a", "a"), obs_profile("b", "b"), bundle_sd],
        &["http://example.org/StructureDefinition/obs-bundle"],
    )
    .await;
    let missing: Vec<String> = errors(&issues)
        .iter()
        .filter_map(|e| e.hl7_message.clone())
        .filter(|m| m.contains("a matching slice is required"))
        .collect();
    assert_eq!(missing.len(), 1, "{issues:#?}");
    assert!(missing[0].contains("Bundle.entry:b"), "{missing:?}");
}

/// `inv-checks#profile`: invariants decide between type profiles too.
#[tokio::test]
async fn candidate_invariants_decide_type_profiles() {
    let name_profile = |n: &str| {
        json!({
            "resourceType": "StructureDefinition",
            "url": format!("http://example.org/StructureDefinition/{n}"),
            "name": n, "status": "active", "kind": "complex-type", "abstract": false,
            "type": "HumanName", "derivation": "constraint",
            "baseDefinition": "http://hl7.org/fhir/StructureDefinition/HumanName",
            "differential": {"element": [{"id": "HumanName.given", "path": "HumanName.given",
                "constraint": [{"key": "test1", "severity": "error", "human": "Always fail", "expression": "false"}]}]}
        })
    };
    let patient_profile = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/inv-patient",
        "name": "InvPatient", "status": "active", "kind": "resource", "abstract": false,
        "type": "Patient", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
        "differential": {"element": [{"id": "Patient.name", "path": "Patient.name",
            "type": [{"code": "HumanName", "profile": ["http://example.org/StructureDefinition/n1",
                                                        "http://example.org/StructureDefinition/n2"]}]}]}
    });
    let patient =
        json!({"resourceType": "Patient", "name": [{"family": "Doe", "given": ["Jane"]}]});
    let issues = validate_with(
        &patient,
        &[name_profile("n1"), name_profile("n2"), patient_profile],
        &["http://example.org/StructureDefinition/inv-patient"],
    )
    .await;
    let errs = errors(&issues);
    assert!(
        errs.iter().any(|e| e
            .message
            .starts_with("Unable to find a match for the specified profile")),
        "{issues:#?}"
    );
    assert_eq!(
        errs.iter().filter(|e| e.message.contains("test1")).count(),
        1,
        "{issues:#?}"
    );
}

/// `no/Person-test-bad#profile`: the same cardinality failure under two
/// candidate type profiles is listed for each.
#[tokio::test]
async fn candidate_failures_are_listed_per_profile() {
    let address_profile = |n: &str| {
        json!({
            "resourceType": "StructureDefinition",
            "url": format!("http://example.org/StructureDefinition/{n}"),
            "name": n, "status": "active", "kind": "complex-type", "abstract": false,
            "type": "Address", "derivation": "constraint",
            "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Address",
            "differential": {"element": [{"id": "Address.city", "path": "Address.city", "max": "0"}]}
        })
    };
    let person_profile = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/person",
        "name": "P", "status": "active", "kind": "resource", "abstract": false,
        "type": "Person", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Person",
        "differential": {"element": [{"id": "Person.address", "path": "Person.address",
            "type": [{"code": "Address", "profile": ["http://example.org/StructureDefinition/a1",
                                                      "http://example.org/StructureDefinition/a2"]}]}]}
    });
    let person = json!({"resourceType": "Person", "address": [{"city": "Oslo"}]});
    let issues = validate_with(
        &person,
        &[address_profile("a1"), address_profile("a2"), person_profile],
        &["http://example.org/StructureDefinition/person"],
    )
    .await;
    assert_eq!(errors(&issues).len(), 3, "{issues:#?}");
}

/// `bnd-slicing-cgm#profile` (cgm-summary.hasMember): a `resolve().code`
/// value discriminator takes its value from the slices' target profiles.
#[tokio::test]
async fn resolved_discriminator_values_from_target_profiles() {
    let obs_profile = |name: &str, code: &str| {
        json!({
            "resourceType": "StructureDefinition",
            "url": format!("http://example.org/StructureDefinition/{name}"),
            "name": name, "status": "active", "kind": "resource", "abstract": false,
            "type": "Observation", "derivation": "constraint",
            "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Observation",
            "differential": {"element": [{"id": "Observation.code", "path": "Observation.code",
                "patternCodeableConcept": {"coding": [{"system": "http://example.org/cs", "code": code}]}}]}
        })
    };
    let summary = json!({
        "resourceType": "StructureDefinition",
        "url": "http://example.org/StructureDefinition/summary",
        "name": "Summary", "status": "active", "kind": "resource", "abstract": false,
        "type": "Observation", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Observation",
        "differential": {"element": [
            {"id": "Observation.hasMember", "path": "Observation.hasMember",
             "slicing": {"discriminator": [{"type": "value", "path": "resolve().code"}], "rules": "open"}},
            {"id": "Observation.hasMember:a", "path": "Observation.hasMember", "sliceName": "a", "min": 1,
             "type": [{"code": "Reference", "targetProfile": ["http://example.org/StructureDefinition/a"]}]},
            {"id": "Observation.hasMember:b", "path": "Observation.hasMember", "sliceName": "b", "min": 1,
             "type": [{"code": "Reference", "targetProfile": ["http://example.org/StructureDefinition/b"]}]}
        ]}
    });
    let obs = |id: &str, code: &str| {
        json!({"fullUrl": format!("http://example.org/fhir/Observation/{id}"),
        "resource": {"resourceType": "Observation", "id": id, "status": "final",
            "code": {"coding": [{"system": "http://example.org/cs", "code": code}]}}})
    };
    let mut s = obs("s", "s");
    s["resource"]["meta"] = json!({"profile": ["http://example.org/StructureDefinition/summary"]});
    s["resource"]["hasMember"] = json!([{"reference": "Observation/m"}]);
    let bundle =
        json!({"resourceType": "Bundle", "type": "collection", "entry": [s, obs("m", "a")]});
    let issues = validate_with(
        &bundle,
        &[obs_profile("a", "a"), obs_profile("b", "b"), summary],
        &[],
    )
    .await;
    let missing: Vec<String> = errors(&issues)
        .iter()
        .filter_map(|e| e.hl7_message.clone())
        .filter(|m| m.contains("a matching slice is required"))
        .collect();
    assert_eq!(missing.len(), 1, "{issues:#?}");
    assert!(missing[0].contains("hasMember:b"), "{missing:?}");
}
