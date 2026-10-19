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

/// Diagnostic, not a check: invariant failures per constraint id over the R4
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
        for e in validate_with(&resource, &[], &[]).await {
            if let Some(id) = e.extra.get("constraint").and_then(Value::as_str)
                && e.severity == Severity::Error
            {
                counts.entry(id.to_string()).or_default().push(format!(
                    "{} {}",
                    entry.file_name().to_string_lossy(),
                    e.path
                ));
            }
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
