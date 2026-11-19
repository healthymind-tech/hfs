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
        errors
            .iter()
            .any(|e| e.kind == ErrorKind::FhirpathConstraint
                && e.extra.get("constraint") == Some(&json!("obs-7"))
                && e.severity == helios_fhir_validator::Severity::Error),
        "{errors:#?}"
    );
    obs["component"][0]["code"] =
        json!({"coding": [{"system": "http://loinc.org", "code": "8462-4"}]});
    let errors = validate(&obs, &[]).await;
    assert!(
        !errors
            .iter()
            .any(|e| e.extra.get("constraint") == Some(&json!("obs-7"))),
        "{errors:#?}"
    );
}

/// A wrong display on a required-bound coding warns (HAPI / HL7 validator
/// with display issues as warnings); the right display, a designation-free
/// case difference, or no display at all stays silent.
#[tokio::test]
async fn wrong_display_on_a_required_binding_warns() {
    let condition = |display: Option<&str>| {
        let mut coding = json!({
            "system": "http://terminology.hl7.org/CodeSystem/condition-clinical", "code": "active"
        });
        if let Some(d) = display {
            coding["display"] = json!(d);
        }
        json!({"resourceType": "Condition", "subject": {"reference": "Patient/p"},
               "clinicalStatus": {"coding": [coding]}})
    };
    let validator = Validator::new(core_registry(FhirVersion::R4));
    let tx = helios_fhir_validator::core_terminology(FhirVersion::R4);
    let handlers = EffectHandlers {
        terminology: Some(tx.as_ref()),
        ..Default::default()
    };
    let check = |resource: Value| {
        let validator = &validator;
        let handlers = &handlers;
        async move {
            validator
                .validate(
                    &resource,
                    FhirVersion::R4,
                    &ValidationOptions::default(),
                    handlers,
                )
                .await
        }
    };
    let issues = check(condition(Some("Not Active At All"))).await;
    let wrong: Vec<_> = issues
        .iter()
        .filter(|e| e.kind == ErrorKind::DisplayMismatch)
        .collect();
    assert_eq!(wrong.len(), 1, "{issues:#?}");
    assert_eq!(wrong[0].severity, helios_fhir_validator::Severity::Warning);
    assert_eq!(wrong[0].path, "Condition.clinicalStatus.coding.0.display");
    assert!(
        wrong[0].message.contains("Valid display is 'Active'"),
        "{wrong:?}"
    );
    assert!(
        !issues
            .iter()
            .any(|e| e.severity == helios_fhir_validator::Severity::Error),
        "{issues:#?}"
    );
    for fine in [Some("Active"), Some("active"), None] {
        let issues = check(condition(fine)).await;
        assert!(issues.is_empty(), "{fine:?}: {issues:#?}");
    }
}

// ---------------------------------------------------------------------------
// Converted profiles layered on the core pack
// ---------------------------------------------------------------------------

use helios_fhir_validator::{CompositeResolver, SchemaRegistry, SchemaResolver};
use std::sync::Arc;

fn ed(id: &str, extra: Value) -> Value {
    let path: String = id
        .split('.')
        .map(|s| s.split(':').next().unwrap())
        .collect::<Vec<_>>()
        .join(".");
    let mut e = json!({"id": id, "path": path});
    for (k, v) in extra.as_object().unwrap() {
        e[k] = v.clone();
    }
    e
}

fn sd(url: &str, ty: &str, base: &str, kind: &str, elements: Vec<Value>) -> Value {
    json!({
        "resourceType": "StructureDefinition", "url": url, "name": url.rsplit('/').next().unwrap(),
        "status": "active", "kind": kind, "abstract": false, "type": ty,
        "baseDefinition": base, "derivation": "constraint",
        "snapshot": {"element": elements}
    })
}

fn resolver(sds: &[Value]) -> Arc<dyn SchemaResolver> {
    let mut registry = SchemaRegistry::new();
    for s in sds {
        let conversion = helios_fhir_validator::converter::convert(s).expect("converts");
        registry.insert_by_url(conversion.schema);
    }
    Arc::new(CompositeResolver::new(vec![
        Arc::new(registry),
        core_registry(FhirVersion::R4),
    ]))
}

fn structural(
    resolver: &Arc<dyn SchemaResolver>,
    resource: &Value,
    profiles: &[&str],
) -> Vec<ValidationError> {
    let opts = ValidationOptions {
        profiles: profiles.iter().map(|p| p.to_string()).collect(),
        check_extension_context: true,
        ..Default::default()
    };
    Validator::new(Arc::clone(resolver))
        .validate_sync(resource, &opts)
        .errors
}

const BP_LIKE: &str = "http://example.org/StructureDefinition/bp-like";

/// A blood-pressure-like profile in the shape the core `bp` profile and TW
/// Core use: components sliced by `code.coding.code`/`code.coding.system`
/// whose constants sit inside a nested `coding` slice, and fixed
/// `value[x].system`/`value[x].code` under each component.
fn bp_like() -> Value {
    let component = |slice: &str, code: &str| {
        let p = format!("Observation.component:{slice}");
        vec![
            ed(
                &p,
                json!({"sliceName": slice, "min": 1, "max": "1", "type": [{"code": "BackboneElement"}]}),
            ),
            ed(
                &format!("{p}.code"),
                json!({"min": 1, "max": "1", "type": [{"code": "CodeableConcept"}]}),
            ),
            ed(
                &format!("{p}.code.coding"),
                json!({"min": 1, "max": "*", "base": {"path": "CodeableConcept.coding", "max": "*"},
                "type": [{"code": "Coding"}],
                "slicing": {"discriminator": [{"type": "value", "path": "code"}, {"type": "value", "path": "system"}], "rules": "open"}}),
            ),
            ed(
                &format!("{p}.code.coding:{slice}Code"),
                json!({"sliceName": format!("{slice}Code"), "min": 1, "max": "1", "type": [{"code": "Coding"}]}),
            ),
            ed(
                &format!("{p}.code.coding:{slice}Code.system"),
                json!({"min": 1, "max": "1", "type": [{"code": "uri"}], "fixedUri": "http://loinc.org"}),
            ),
            ed(
                &format!("{p}.code.coding:{slice}Code.code"),
                json!({"min": 1, "max": "1", "type": [{"code": "code"}], "fixedCode": code}),
            ),
            ed(
                &format!("{p}.value[x]"),
                json!({"min": 0, "max": "1", "type": [{"code": "Quantity"}]}),
            ),
            ed(
                &format!("{p}.value[x].value"),
                json!({"min": 1, "max": "1", "type": [{"code": "decimal"}]}),
            ),
            ed(
                &format!("{p}.value[x].system"),
                json!({"min": 1, "max": "1", "type": [{"code": "uri"}], "fixedUri": "http://unitsofmeasure.org"}),
            ),
            ed(
                &format!("{p}.value[x].code"),
                json!({"min": 1, "max": "1", "type": [{"code": "code"}], "fixedCode": "mm[Hg]"}),
            ),
        ]
    };
    let mut elements = vec![
        ed("Observation", json!({"min": 0, "max": "*"})),
        ed(
            "Observation.component",
            json!({"min": 2, "max": "*", "base": {"path": "Observation.component", "max": "*"},
            "type": [{"code": "BackboneElement"}],
            "slicing": {"discriminator": [{"type": "value", "path": "code.coding.code"}, {"type": "value", "path": "code.coding.system"}],
                        "ordered": false, "rules": "open"}}),
        ),
    ];
    elements.extend(component("Sys", "8480-6"));
    elements.extend(component("Dia", "8462-4"));
    sd(
        BP_LIKE,
        "Observation",
        "http://hl7.org/fhir/StructureDefinition/Observation",
        "resource",
        elements,
    )
}

fn bp_observation() -> Value {
    let comp = |code: &str, v: i64| {
        json!({
            "code": {"coding": [{"system": "http://loinc.org", "code": code}]},
            "valueQuantity": {"value": v, "unit": "mmHg", "system": "http://unitsofmeasure.org", "code": "mm[Hg]"}
        })
    };
    json!({"resourceType": "Observation", "status": "final", "code": {"text": "BP"},
           "component": [comp("8480-6", 120), comp("8462-4", 80)]})
}

#[test]
fn nested_slice_constants_discriminate_components() {
    let r = resolver(&[bp_like()]);
    let ok = structural(&r, &bp_observation(), &[BP_LIKE]);
    assert!(ok.is_empty(), "{ok:#?}");

    // Systolic twice: slice max 1, attributed to the profile.
    let mut dup = bp_observation();
    let first = dup["component"][0].clone();
    dup["component"].as_array_mut().unwrap().push(first);
    let errors = structural(&r, &dup, &[BP_LIKE]);
    let max: Vec<_> = errors
        .iter()
        .filter(|e| e.kind == ErrorKind::SliceCardinality && e.path == "Observation.component")
        .collect();
    assert_eq!(max.len(), 1, "{errors:#?}");
    assert!(max[0].message.contains("max cardinality: '1'"), "{max:?}");
    assert_eq!(max[0].source.as_deref(), Some(BP_LIKE));
    assert_eq!(
        max[0].hl7_message.as_deref(),
        Some("Observation.component:Sys: max allowed = 1, but found 2")
    );

    // Diastolic missing: slice min 1, and the element minimum of 2 in the
    // HL7 wording, attributed to the profile.
    let mut missing = bp_observation();
    missing["component"].as_array_mut().unwrap().pop();
    let errors = structural(&r, &missing, &[BP_LIKE]);
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::Min
            && e.hl7_message.as_deref()
                == Some("Observation.component: minimum required = 2, but only found 1")
            && e.source.as_deref() == Some(BP_LIKE)),
        "{errors:#?}"
    );
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::SliceCardinality
            && e.message
                .contains("min cardinality: '1', actual cardinality: '0'")),
        "{errors:#?}"
    );

    // Neither component matches: both required slices are reported, each
    // under its own name (the pinned message alone is identical).
    let mut neither = bp_observation();
    for c in neither["component"].as_array_mut().unwrap() {
        c["code"] = json!({"coding": [{"system": "http://loinc.org", "code": "0000-0"}]});
    }
    let errors = structural(&r, &neither, &[BP_LIKE]);
    let mut missing: Vec<_> = errors
        .iter()
        .filter(|e| e.kind == ErrorKind::SliceCardinality && e.path == "Observation.component")
        .filter_map(|e| e.hl7_message.as_deref())
        .collect();
    missing.sort();
    assert_eq!(
        missing,
        vec![
            "Slice 'Observation.component:Dia': a matching slice is required, but not found",
            "Slice 'Observation.component:Sys': a matching slice is required, but not found",
        ],
        "{errors:#?}"
    );
}

#[test]
fn choice_children_constrain_the_concrete_branch() {
    let r = resolver(&[bp_like()]);
    let mut obs = bp_observation();
    obs["component"][0]["valueQuantity"]["system"] = json!("http://example.org/units");
    let errors = structural(&r, &obs, &[BP_LIKE]);
    let fixed: Vec<_> = errors
        .iter()
        .filter(|e| e.kind == ErrorKind::FixedValue)
        .collect();
    assert_eq!(fixed.len(), 1, "{errors:#?}");
    assert_eq!(
        fixed[0].path,
        "Observation.component.0.valueQuantity.system"
    );

    // value[x].value is required inside the slice.
    let mut obs = bp_observation();
    obs["component"][1]["valueQuantity"]
        .as_object_mut()
        .unwrap()
        .remove("value");
    let errors = structural(&r, &obs, &[BP_LIKE]);
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::Required
            && e.path == "Observation.component.1.valueQuantity.value"),
        "{errors:#?}"
    );
}

const STRICT_PATIENT: &str = "http://example.org/StructureDefinition/strict-patient";
const DOC_BUNDLE: &str = "http://example.org/StructureDefinition/doc-bundle";

/// Bundle entries sliced by `type` and by `profile` of `resource`, as TW
/// Core's document/message Bundles and the IPS Bundle do.
fn doc_bundle_sds() -> Vec<Value> {
    let strict = sd(
        STRICT_PATIENT,
        "Patient",
        "http://hl7.org/fhir/StructureDefinition/Patient",
        "resource",
        vec![
            ed("Patient", json!({"min": 0, "max": "*"})),
            ed(
                "Patient.gender",
                json!({"min": 1, "max": "1", "type": [{"code": "code"}]}),
            ),
        ],
    );
    let bundle = sd(
        DOC_BUNDLE,
        "Bundle",
        "http://hl7.org/fhir/StructureDefinition/Bundle",
        "resource",
        vec![
            ed("Bundle", json!({"min": 0, "max": "*"})),
            ed(
                "Bundle.entry",
                json!({"min": 0, "max": "*", "base": {"path": "Bundle.entry", "max": "*"},
            "type": [{"code": "BackboneElement"}],
            "slicing": {"discriminator": [{"type": "type", "path": "resource"}, {"type": "profile", "path": "resource"}], "rules": "open"}}),
            ),
            ed(
                "Bundle.entry:patient",
                json!({"sliceName": "patient", "min": 1, "max": "1", "type": [{"code": "BackboneElement"}]}),
            ),
            ed(
                "Bundle.entry:patient.resource",
                json!({"min": 1, "max": "1",
            "type": [{"code": "Patient", "profile": [STRICT_PATIENT]}]}),
            ),
        ],
    );
    vec![strict, bundle]
}

fn doc_bundle(patient: Value) -> Value {
    json!({"resourceType": "Bundle", "type": "collection", "entry": [
        {"fullUrl": "urn:uuid:11111111-1111-1111-1111-111111111111", "resource": patient},
        {"fullUrl": "urn:uuid:22222222-2222-2222-2222-222222222222",
         "resource": {"resourceType": "Observation", "status": "final", "code": {"text": "x"}}}
    ]})
}

#[test]
fn type_and_profile_discriminators_select_entries() {
    let r = resolver(&doc_bundle_sds());
    let slice_errors = |b: &Value| {
        structural(&r, b, &[DOC_BUNDLE])
            .into_iter()
            .filter(|e| e.kind == ErrorKind::SliceCardinality)
            .collect::<Vec<_>>()
    };
    // A Patient meeting the entry profile fills the slice.
    let ok = doc_bundle(json!({"resourceType": "Patient", "gender": "male"}));
    assert!(slice_errors(&ok).is_empty(), "{:#?}", slice_errors(&ok));
    // A Patient that does not conform matches no slice: required slice missing.
    let bad = doc_bundle(json!({"resourceType": "Patient"}));
    let errors = slice_errors(&bad);
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(errors[0].path, "Bundle.entry");
    // Claiming the profile is enough to be sorted into the slice (the claim
    // itself is then checked through meta.profile).
    let claimed =
        doc_bundle(json!({"resourceType": "Patient", "meta": {"profile": [STRICT_PATIENT]}}));
    assert!(slice_errors(&claimed).is_empty());
    // Two conforming Patients overflow the slice.
    let mut two = ok.clone();
    let p = two["entry"][0].clone();
    two["entry"].as_array_mut().unwrap().push(p);
    let errors = slice_errors(&two);
    assert!(
        errors.iter().any(|e| e.message.contains("max cardinality")),
        "{errors:#?}"
    );
}

const RACE_LIKE: &str = "http://example.org/StructureDefinition/race-like";
const RACE_PATIENT: &str = "http://example.org/StructureDefinition/race-patient";

/// A complex extension (US Core race shape) with a required `text`
/// sub-extension, and a Patient profile declaring it.
fn race_sds() -> Vec<Value> {
    let mut ext = sd(
        RACE_LIKE,
        "Extension",
        "http://hl7.org/fhir/StructureDefinition/Extension",
        "complex-type",
        vec![
            ed("Extension", json!({"min": 0, "max": "*"})),
            ed(
                "Extension.extension",
                json!({"min": 1, "max": "*", "base": {"path": "Element.extension", "max": "*"},
            "type": [{"code": "Extension"}],
            "slicing": {"discriminator": [{"type": "value", "path": "url"}], "rules": "open"}}),
            ),
            ed(
                "Extension.extension:text",
                json!({"sliceName": "text", "min": 1, "max": "1", "type": [{"code": "Extension"}]}),
            ),
            ed(
                "Extension.extension:text.url",
                json!({"min": 1, "max": "1", "type": [{"code": "uri"}], "fixedUri": "text"}),
            ),
            ed(
                "Extension.extension:text.value[x]",
                json!({"min": 1, "max": "1", "type": [{"code": "string"}]}),
            ),
            ed(
                "Extension.url",
                json!({"min": 1, "max": "1", "type": [{"code": "uri"}], "fixedUri": RACE_LIKE}),
            ),
            ed(
                "Extension.value[x]",
                json!({"min": 0, "max": "0", "type": [{"code": "string"}]}),
            ),
        ],
    );
    ext["context"] = json!([{"type": "element", "expression": "Patient"}]);
    let patient = sd(
        RACE_PATIENT,
        "Patient",
        "http://hl7.org/fhir/StructureDefinition/Patient",
        "resource",
        vec![
            ed("Patient", json!({"min": 0, "max": "*"})),
            ed(
                "Patient.extension",
                json!({"min": 0, "max": "*", "base": {"path": "DomainResource.extension", "max": "*"},
            "type": [{"code": "Extension"}],
            "slicing": {"discriminator": [{"type": "value", "path": "url"}], "rules": "open"}}),
            ),
            ed(
                "Patient.extension:race",
                json!({"sliceName": "race", "min": 0, "max": "1",
            "type": [{"code": "Extension", "profile": [RACE_LIKE]}]}),
            ),
        ],
    );
    vec![ext, patient]
}

#[test]
fn complex_extension_sub_extension_minimum_names_the_extension() {
    let r = resolver(&race_sds());
    let without_text = json!({"resourceType": "Patient", "extension": [{
        "url": RACE_LIKE, "extension": [{"url": "other", "valueString": "x"}]
    }]});
    for profiles in [&[RACE_PATIENT][..], &[][..]] {
        let errors = structural(&r, &without_text, profiles);
        let missing: Vec<_> = errors
            .iter()
            .filter(|e| e.kind == ErrorKind::SliceCardinality)
            .collect();
        assert_eq!(missing.len(), 1, "{profiles:?}: {errors:#?}");
        assert_eq!(missing[0].path, "Patient.extension.0.extension");
        assert_eq!(
            missing[0].source.as_deref(),
            Some(RACE_LIKE),
            "{profiles:?}"
        );
    }
    let with_text = json!({"resourceType": "Patient", "extension": [{
        "url": RACE_LIKE, "extension": [{"url": "text", "valueString": "x"}]
    }]});
    let errors = structural(&r, &with_text, &[RACE_PATIENT]);
    assert!(errors.is_empty(), "{errors:#?}");

    // The extension's context is Patient: on an Observation it is an error.
    let obs = json!({"resourceType": "Observation", "status": "final", "code": {"text": "x"},
        "extension": [{"url": RACE_LIKE, "extension": [{"url": "text", "valueString": "x"}]}]});
    let errors = structural(&r, &obs, &[]);
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::ExtensionContext
            && e.severity == helios_fhir_validator::Severity::Error),
        "{errors:#?}"
    );
}

/// An extension allowed by a FHIRPath context is not misplaced just
/// because its element contexts do not match.
#[test]
fn fhirpath_contexts_keep_the_context_check_quiet() {
    let mut sds = race_sds();
    sds[0]["context"] = json!([
        {"type": "element", "expression": "Patient"},
        {"type": "fhirpath", "expression": "Observation"}
    ]);
    let r = resolver(&sds);
    let obs = json!({"resourceType": "Observation", "status": "final", "code": {"text": "x"},
        "extension": [{"url": RACE_LIKE, "extension": [{"url": "text", "valueString": "x"}]}]});
    let errors = structural(&r, &obs, &[]);
    assert!(
        !errors.iter().any(|e| e.kind == ErrorKind::ExtensionContext),
        "{errors:#?}"
    );
}

/// ele-1: an element carrying nothing but an `id` has neither a value nor
/// children.
#[tokio::test]
async fn ele1_fires_on_an_id_only_element() {
    let errors = validate(
        &json!({"resourceType": "Patient", "name": [{"id": "a"}]}),
        &[],
    )
    .await;
    assert!(
        errors
            .iter()
            .any(|e| e.extra.get("constraint") == Some(&json!("ele-1"))
                && e.path == "Patient.name.0"
                && e.severity == helios_fhir_validator::Severity::Error),
        "{errors:#?}"
    );
    let fine = validate(
        &json!({"resourceType": "Patient", "name": [{"id": "a", "family": "x"}],
                "birthDate": "1980-01-01", "_birthDate": {"id": "b"}}),
        &[],
    )
    .await;
    assert!(fine.is_empty(), "{fine:#?}");
}

/// A resource the typed model cannot read (gender as an array) reports its
/// structural error plus one note that invariants could not run — not one
/// warning per invariant.
#[tokio::test]
async fn unparseable_resources_report_unevaluated_invariants_once() {
    let errors = validate(
        &json!({"resourceType": "Patient", "gender": ["male", "female"],
                "identifier": [{"system": "http://x", "value": "1"}]}),
        &[],
    )
    .await;
    let not_evaluated: Vec<_> = errors
        .iter()
        .filter(|e| e.kind == ErrorKind::FhirpathConstraint)
        .collect();
    assert_eq!(not_evaluated.len(), 1, "{errors:#?}");
    assert_eq!(not_evaluated[0].path, "Patient");
    assert_eq!(
        not_evaluated[0].severity,
        helios_fhir_validator::Severity::Warning
    );
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::NotSingular),
        "{errors:#?}"
    );
}
