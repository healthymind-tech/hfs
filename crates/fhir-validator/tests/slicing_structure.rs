//! Slicing and profile-structure correctness against the HL7 Java
//! validator (Phase 2 stream Q), on converted profiles layered over the
//! embedded R4 pack. Each case reduces a disagreement found by the
//! differential test over the official IG examples.
//!
//! The `real_examples_*` tests replay those official examples when
//! `HFS_IG_PACKAGES` names a directory of extracted IG packages
//! (`<dir>/<name>-<version>/package/StructureDefinition-*.json`) and
//! `HFS_IG_CORPUS` the example corpus (`<dir>/<ig>/<file>.json`); they skip
//! silently otherwise.
//!
//! Run with: `cargo test -p helios-fhir-validator --test slicing_structure`

#![cfg(feature = "R4")]

use helios_fhir::FhirVersion;
use helios_fhir_validator::packs::core_registry;
use helios_fhir_validator::{
    CompositeResolver, ErrorKind, SchemaRegistry, SchemaResolver, Severity, ValidationError,
    ValidationOptions, Validator,
};
use serde_json::{Value, json};
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

fn sd(url: &str, ty: &str, elements: Vec<Value>) -> Value {
    json!({
        "resourceType": "StructureDefinition", "url": url, "name": url.rsplit('/').next().unwrap(),
        "status": "active", "kind": "resource", "abstract": false, "type": ty,
        "baseDefinition": format!("http://hl7.org/fhir/StructureDefinition/{ty}"),
        "derivation": "constraint",
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

fn validate(resolver: &Arc<dyn SchemaResolver>, resource: &Value, profiles: &[&str]) -> Vec<ValidationError> {
    let opts = ValidationOptions {
        profiles: profiles.iter().map(|p| p.to_string()).collect(),
        check_extension_context: true,
        ..Default::default()
    };
    Validator::new(Arc::clone(resolver))
        .validate_sync(resource, &opts)
        .errors
}

fn errors_of(errors: &[ValidationError], kind: ErrorKind) -> Vec<String> {
    errors
        .iter()
        .filter(|e| e.kind == kind && e.severity == Severity::Error)
        .map(|e| format!("{}: {}", e.path, e.hl7_message.as_deref().unwrap_or(&e.message)))
        .collect()
}

const PRACTITIONER: &str = "http://example.org/StructureDefinition/loose-practitioner";
const PATIENT: &str = "http://example.org/StructureDefinition/loose-patient";
const ENTRY_BUNDLE: &str = "http://example.org/StructureDefinition/entry-bundle";

/// Bundle.entry sliced by `profile` of `resource` only (no `type`
/// discriminator), with profiles that constrain nothing — the EMPD/NGS
/// Bundle shape. A Patient shares every element a Practitioner profile
/// checks, so only the profile's type tells them apart.
fn entry_bundle_sds() -> Vec<Value> {
    let loose = |url: &str, ty: &str| sd(url, ty, vec![ed(ty, json!({"min": 0, "max": "*"}))]);
    let bundle = sd(
        ENTRY_BUNDLE,
        "Bundle",
        vec![
            ed("Bundle", json!({"min": 0, "max": "*"})),
            ed(
                "Bundle.entry",
                json!({"min": 0, "max": "*", "type": [{"code": "BackboneElement"}],
                       "slicing": {"discriminator": [{"type": "profile", "path": "resource"}], "rules": "open"}}),
            ),
            ed(
                "Bundle.entry:Practitioner",
                json!({"sliceName": "Practitioner", "min": 1, "max": "1", "type": [{"code": "BackboneElement"}]}),
            ),
            ed(
                "Bundle.entry:Practitioner.resource",
                json!({"min": 1, "max": "1", "type": [{"code": "Practitioner", "profile": [PRACTITIONER]}]}),
            ),
            ed(
                "Bundle.entry:Patient",
                json!({"sliceName": "Patient", "min": 1, "max": "1", "type": [{"code": "BackboneElement"}]}),
            ),
            ed(
                "Bundle.entry:Patient.resource",
                json!({"min": 1, "max": "1", "type": [{"code": "Patient", "profile": [PATIENT]}]}),
            ),
        ],
    );
    vec![loose(PRACTITIONER, "Practitioner"), loose(PATIENT, "Patient"), bundle]
}

/// Regression (stream L): EMPD `Bundle-bun-01-ep` reported
/// "Bundle.entry:Practitioner: max allowed = 1, but found 2" with one
/// Practitioner — the Patient validated cleanly against the Practitioner
/// profile.
#[test]
fn profile_discriminators_do_not_count_resources_of_another_type() {
    let r = resolver(&entry_bundle_sds());
    let bundle = json!({"resourceType": "Bundle", "type": "collection", "entry": [
        {"fullUrl": "urn:uuid:1", "resource": {"resourceType": "Patient", "id": "p",
            "name": [{"text": "A"}], "gender": "male", "birthDate": "1970-01-01"}},
        {"fullUrl": "urn:uuid:2", "resource": {"resourceType": "Practitioner", "id": "d",
            "name": [{"text": "B"}], "gender": "female"}}
    ]});
    let errors = validate(&r, &bundle, &[ENTRY_BUNDLE]);
    assert!(errors_of(&errors, ErrorKind::SliceCardinality).is_empty(), "{errors:#?}");

    // Without the Practitioner the slice minimum fails, and the Patient
    // does not stand in for it.
    let mut no_doctor = bundle.clone();
    no_doctor["entry"].as_array_mut().unwrap().pop();
    let slices = errors_of(&validate(&r, &no_doctor, &[ENTRY_BUNDLE]), ErrorKind::SliceCardinality);
    assert_eq!(slices.len(), 1, "{slices:#?}");
    assert!(slices[0].contains("Bundle.entry:Practitioner"), "{slices:#?}");
}

/// A resource claiming a profile of another resource type is rejected
/// (HL7 `VALIDATION_VAL_PROFILE_WRONGTYPE`) instead of being judged by the
/// other type's elements.
#[test]
fn a_profile_of_another_resource_type_is_rejected() {
    let r = resolver(&entry_bundle_sds());
    let patient = json!({"resourceType": "Patient", "meta": {"profile": [PRACTITIONER]}});
    let errors = validate(&r, &patient, &[]);
    let wrong = errors_of(&errors, ErrorKind::Type);
    assert_eq!(wrong.len(), 1, "{errors:#?}");
    assert!(wrong[0].contains("Specified profile type was 'Practitioner'"), "{wrong:#?}");
    // The right type is fine.
    let ok = json!({"resourceType": "Practitioner", "meta": {"profile": [PRACTITIONER]}});
    assert!(validate(&r, &ok, &[]).iter().all(|e| e.severity != Severity::Error));
}

const OCCUPATION: &str = "http://example.org/StructureDefinition/occupation";

/// EMR `Observation-PMR-Obs6`: `code.coding` sliced by `type@$this` — every
/// Coding is of the sliced element's type, so the coding with the fixed
/// LOINC code fills the required slice.
#[test]
fn type_discriminator_on_this_matches_non_resource_items() {
    let r = resolver(&[sd(
        OCCUPATION,
        "Observation",
        vec![
            ed("Observation", json!({"min": 0, "max": "*"})),
            ed(
                "Observation.code.coding",
                json!({"min": 1, "max": "*", "type": [{"code": "Coding"}],
                       "slicing": {"discriminator": [{"type": "type", "path": "$this"}], "rules": "open"}}),
            ),
            ed(
                "Observation.code.coding:UsualOccupation",
                json!({"sliceName": "UsualOccupation", "min": 1, "max": "1", "type": [{"code": "Coding"}]}),
            ),
            ed(
                "Observation.code.coding:UsualOccupation.system",
                json!({"min": 1, "max": "1", "type": [{"code": "uri"}], "fixedUri": "http://loinc.org"}),
            ),
            ed(
                "Observation.code.coding:UsualOccupation.code",
                json!({"min": 1, "max": "1", "type": [{"code": "code"}], "fixedCode": "21847-9"}),
            ),
        ],
    )]);
    let obs = |code: &str| {
        json!({"resourceType": "Observation", "status": "final",
               "code": {"coding": [{"system": "http://loinc.org", "code": code}]}})
    };
    let errors = validate(&r, &obs("21847-9"), &[OCCUPATION]);
    assert!(errors_of(&errors, ErrorKind::SliceCardinality).is_empty(), "{errors:#?}");
    // A coding of the type with the wrong code joins the slice and fails
    // its fixed code (as in the Java validator).
    let errors = validate(&r, &obs("11111-1"), &[OCCUPATION]);
    assert!(!errors_of(&errors, ErrorKind::FixedValue).is_empty(), "{errors:#?}");
}

const TREAT: &str = "http://example.org/StructureDefinition/treat";

/// NHI PAS `MedicationRequest-treat-twpas`: `coding` closed-sliced by
/// `pattern@$this` whose slices carry only required bindings. The Java
/// validator decides membership by ValueSet; we cannot here, so the closed
/// rule must not reject the coding.
#[test]
fn closed_slicing_with_undecidable_slices_rejects_nothing() {
    let r = resolver(&[sd(
        TREAT,
        "MedicationRequest",
        vec![
            ed("MedicationRequest", json!({"min": 0, "max": "*"})),
            ed(
                "MedicationRequest.medication[x]",
                json!({"min": 1, "max": "1", "type": [{"code": "CodeableConcept"}]}),
            ),
            ed(
                "MedicationRequest.medication[x].coding",
                json!({"min": 1, "max": "1", "type": [{"code": "Coding"}],
                       "slicing": {"discriminator": [{"type": "pattern", "path": "$this"}], "rules": "closed"}}),
            ),
            ed(
                "MedicationRequest.medication[x].coding:nhi",
                json!({"sliceName": "nhi", "min": 0, "max": "1", "type": [{"code": "Coding"}],
                       "binding": {"strength": "required", "valueSet": "http://example.org/ValueSet/nhi"}}),
            ),
        ],
    )]);
    let req = json!({"resourceType": "MedicationRequest", "status": "active", "intent": "order",
        "subject": {"reference": "Patient/p"},
        "medicationCodeableConcept": {"coding": [{"system": "http://example.org/nhi", "code": "A1"}]}});
    let errors = validate(&r, &req, &[TREAT]);
    assert!(errors_of(&errors, ErrorKind::SliceUnmatched).is_empty(), "{errors:#?}");
}

const STRICT_OBS: &str = "http://example.org/StructureDefinition/strict-obs";
const DOC: &str = "http://example.org/StructureDefinition/doc-composition";

/// A Composition whose section entries are sliced by `profile@resolve()`
/// (IPS, TW Core EMR/EMPD/NGS Compositions), with a required slice.
fn doc_sds() -> Vec<Value> {
    let strict = sd(
        STRICT_OBS,
        "Observation",
        vec![
            ed("Observation", json!({"min": 0, "max": "*"})),
            ed(
                "Observation.valueQuantity",
                json!({"min": 1, "max": "1", "type": [{"code": "Quantity"}]}),
            ),
        ],
    );
    let doc = sd(
        DOC,
        "Composition",
        vec![
            ed("Composition", json!({"min": 0, "max": "*"})),
            ed(
                "Composition.section.entry",
                json!({"min": 0, "max": "*", "type": [{"code": "Reference"}],
                       "slicing": {"discriminator": [{"type": "profile", "path": "resolve()"}], "rules": "open"}}),
            ),
            ed(
                "Composition.section.entry:result",
                json!({"sliceName": "result", "min": 1, "max": "*",
                       "type": [{"code": "Reference", "targetProfile": [STRICT_OBS]}]}),
            ),
        ],
    );
    vec![strict, doc]
}

fn composition(entry_ref: &str) -> Value {
    json!({"resourceType": "Composition", "id": "c", "status": "final",
        "type": {"text": "doc"}, "date": "2024-01-01", "title": "t",
        "author": [{"display": "x"}],
        "section": [{"title": "s", "entry": [{"reference": entry_ref}]}]})
}

/// Standalone, the reference resolves to nothing: the required slice is
/// missing, as the Java validator reports for the standalone Composition
/// examples (20 resources wrongly accepted before).
#[test]
fn resolve_discriminators_need_a_resolvable_conforming_target() {
    let r = resolver(&doc_sds());
    let errors = validate(&r, &composition("Observation/o"), &[DOC]);
    let slices = errors_of(&errors, ErrorKind::SliceCardinality);
    assert_eq!(slices.len(), 1, "{errors:#?}");
    assert!(slices[0].contains("Composition.section.entry:result"), "{slices:#?}");

    // Inside a Bundle, the target entry is found and judged.
    let bundle = |value: Value| {
        json!({"resourceType": "Bundle", "type": "collection", "entry": [
            {"fullUrl": "urn:uuid:c", "resource": composition("urn:uuid:o")},
            {"fullUrl": "urn:uuid:o", "resource": {"resourceType": "Observation", "id": "o",
                "status": "final", "code": {"text": "x"}, "value": value}}
        ]})
    };
    let mut good = bundle(json!(null));
    let obs = good["entry"][1]["resource"].as_object_mut().unwrap();
    obs.remove("value");
    obs.insert("valueQuantity".into(), json!({"value": 1}));
    good["entry"][0]["resource"]["meta"] = json!({"profile": [DOC]});
    let errors = validate(&r, &good, &[]);
    assert!(errors_of(&errors, ErrorKind::SliceCardinality).is_empty(), "{errors:#?}");

    // A target that does not conform to the target profile fills nothing.
    let mut bad = good.clone();
    let obs = bad["entry"][1]["resource"].as_object_mut().unwrap();
    obs.remove("valueQuantity");
    obs.insert("valueString".into(), json!("x"));
    let slices = errors_of(&validate(&r, &bad, &[]), ErrorKind::SliceCardinality);
    assert_eq!(slices.len(), 1, "{slices:#?}");
}

const EXISTS: &str = "http://example.org/StructureDefinition/exists-patient";

/// `exists` discriminator: the slice requiring the element takes the items
/// that have it.
#[test]
fn exists_discriminators_split_by_presence() {
    let r = resolver(&[sd(
        EXISTS,
        "Patient",
        vec![
            ed("Patient", json!({"min": 0, "max": "*"})),
            ed(
                "Patient.identifier",
                json!({"min": 0, "max": "*", "type": [{"code": "Identifier"}],
                       "slicing": {"discriminator": [{"type": "exists", "path": "period"}], "rules": "open"}}),
            ),
            ed(
                "Patient.identifier:dated",
                json!({"sliceName": "dated", "min": 1, "max": "1", "type": [{"code": "Identifier"}]}),
            ),
            ed(
                "Patient.identifier:dated.period",
                json!({"min": 1, "max": "1", "type": [{"code": "Period"}]}),
            ),
        ],
    )]);
    let patient = |ids: Value| json!({"resourceType": "Patient", "identifier": ids});
    let none = validate(&r, &patient(json!([{"value": "1"}])), &[EXISTS]);
    assert_eq!(errors_of(&none, ErrorKind::SliceCardinality).len(), 1, "{none:#?}");
    let one = validate(
        &r,
        &patient(json!([{"value": "1"}, {"value": "2", "period": {"start": "2020"}}])),
        &[EXISTS],
    );
    assert!(errors_of(&one, ErrorKind::SliceCardinality).is_empty(), "{one:#?}");
    let two = validate(
        &r,
        &patient(json!([{"value": "1", "period": {"start": "2020"}}, {"value": "2", "period": {"start": "2021"}}])),
        &[EXISTS],
    );
    assert_eq!(errors_of(&two, ErrorKind::SliceCardinality).len(), 1, "{two:#?}");
}

/// IPS `Bundle-bundle-minimal`: entries with `urn:uuid:` fullUrls whose
/// references are relative (`Patient/…`) resolve nowhere. The Composition's
/// references are errors, every other entry is an orphan.
#[test]
fn document_bundles_follow_the_fullurl_reference_rules() {
    let r = resolver(&[]);
    let bundle = |subject: &str| {
        json!({"resourceType": "Bundle", "type": "document",
            "identifier": {"system": "urn:ietf:rfc:3986", "value": "urn:uuid:b"},
            "timestamp": "2024-01-01T00:00:00Z", "entry": [
            {"fullUrl": "urn:uuid:c", "resource": {"resourceType": "Composition", "id": "c",
                "status": "final", "type": {"text": "doc"}, "date": "2024-01-01", "title": "t",
                "subject": {"reference": subject}, "author": [{"reference": subject}]}},
            {"fullUrl": "urn:uuid:p", "resource": {"resourceType": "Patient", "id": "p"}}
        ]})
    };
    let ok = validate(&r, &bundle("urn:uuid:p"), &[]);
    assert!(ok.iter().all(|e| e.severity != Severity::Error), "{ok:#?}");

    let bad = validate(&r, &bundle("Patient/p"), &[]);
    let unresolved = errors_of(&bad, ErrorKind::ReferenceUnresolved);
    assert_eq!(unresolved.len(), 2, "{bad:#?}");
    assert!(unresolved[0].starts_with("Bundle.entry.0.resource.subject: Can't find 'Patient/p'"));
    let orphans = errors_of(&bad, ErrorKind::BundleEntryMismatch);
    assert_eq!(orphans, vec!["Bundle.entry.1: Entry 'urn:uuid:p' isn't reachable by traversing links (forward or backward) from the Composition".to_string()]);
}

// ---------------------------------------------------------------------------
// The official examples themselves (optional, see the module docs)
// ---------------------------------------------------------------------------

fn ig_env() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let packages = std::env::var_os("HFS_IG_PACKAGES")?;
    let corpus = std::env::var_os("HFS_IG_CORPUS")?;
    Some((packages.into(), corpus.into()))
}

/// Every StructureDefinition of every extracted package under `dir`
/// (except r4.core, which the embedded pack provides).
fn ig_resolver(dir: &std::path::Path) -> Arc<dyn SchemaResolver> {
    let mut registry = SchemaRegistry::new();
    for pkg in std::fs::read_dir(dir).expect("packages dir").flatten() {
        if pkg.file_name().to_string_lossy().starts_with("hl7.fhir.r4.core") {
            continue;
        }
        let Ok(files) = std::fs::read_dir(pkg.path().join("package")) else {
            continue;
        };
        for f in files.flatten() {
            let name = f.file_name().to_string_lossy().to_string();
            if !name.starts_with("StructureDefinition-") || !name.ends_with(".json") {
                continue;
            }
            let sd: Value = serde_json::from_slice(&std::fs::read(f.path()).unwrap()).unwrap();
            if let Ok(conversion) = helios_fhir_validator::converter::convert(&sd) {
                registry.insert_by_url(conversion.schema);
            }
        }
    }
    Arc::new(CompositeResolver::new(vec![
        Arc::new(registry),
        core_registry(FhirVersion::R4),
    ]))
}

fn example(corpus: &std::path::Path, file: &str) -> Value {
    serde_json::from_slice(&std::fs::read(corpus.join(file)).expect("example")).unwrap()
}

/// The structural errors HFS reports on one example, as `path: message`.
fn example_errors(r: &Arc<dyn SchemaResolver>, corpus: &std::path::Path, file: &str) -> Vec<String> {
    let errors = validate(r, &example(corpus, file), &[]);
    errors
        .iter()
        .filter(|e| e.severity == Severity::Error)
        .map(|e| format!("{}: {}", e.path, e.hl7_message.as_deref().unwrap_or(&e.message)))
        .collect()
}

#[test]
fn real_examples_agree_with_the_java_validator() {
    let Some((packages, corpus)) = ig_env() else {
        return;
    };
    let r = ig_resolver(&packages);
    let slicing = |errors: &[String]| -> Vec<String> {
        errors
            .iter()
            .filter(|e| e.contains("Slice") || e.contains("slice") || e.contains(".entry:"))
            .cloned()
            .collect()
    };
    // Java: no errors (warnings only).
    for file in [
        "tw.gov.mohw.nhi.empd/Bundle-bun-01-ep.json",
        "tw.gov.mohw.nhi.empd/Bundle-bun-05-ep.json",
        "tw.gov.mohw.nhi.ngs/Bundle-bun-document-min.json",
        "tw.gov.mohw.nhi.pas/MedicationRequest-medReq-treat.json",
        "tw.gov.mohw.nhi.pas/Bundle-bun-1.json",
        "tw.gov.mohw.emr/Observation-PMR-Obs6.json",
    ] {
        let errors = example_errors(&r, &corpus, file);
        assert!(slicing(&errors).is_empty(), "{file}: {errors:#?}");
    }
    // Java: required resolve()-sliced section entries are missing.
    for (file, slice) in [
        ("tw.gov.mohw.emr/Composition-com-ep.json", "entry:MedicationRequest"),
        ("tw.gov.mohw.nhi.ngs/Composition-com-min.json", "entry:diagnosticReport"),
        ("tw.gov.mohw.nhi.empd/Composition-com-01-ep.json", "entry:MedicationRequest"),
        ("hl7.fhir.uv.ips/Composition-composition-minimal.json", "entry:problem"),
    ] {
        let errors = example_errors(&r, &corpus, file);
        assert!(errors.iter().any(|e| e.contains(slice)), "{file}: {errors:#?}");
    }
    // Java: the IPS Composition entry does not conform, references written
    // relative to urn:uuid fullUrls resolve nowhere, entries are orphans.
    let errors = example_errors(&r, &corpus, "hl7.fhir.uv.ips/Bundle-bundle-minimal.json");
    assert!(errors.iter().any(|e| e.contains("Bundle.entry:composition")), "{errors:#?}");
    assert!(errors.iter().any(|e| e.contains("isn't reachable")), "{errors:#?}");
    assert!(errors.iter().any(|e| e.contains("Can't find 'Patient/")), "{errors:#?}");
}

/// Structural errors on every corpus example, written as JSON
/// (`{file: ["path: message", ...]}`) to `HFS_IG_SWEEP_OUT` — an offline
/// view of what the differential test will see from the structural walk.
#[test]
#[ignore = "corpus sweep; needs HFS_IG_PACKAGES, HFS_IG_CORPUS and HFS_IG_SWEEP_OUT"]
fn real_examples_sweep() {
    let (Some((packages, corpus)), Some(out)) = (ig_env(), std::env::var_os("HFS_IG_SWEEP_OUT"))
    else {
        return;
    };
    let r = ig_resolver(&packages);
    let mut report = serde_json::Map::new();
    let mut igs: Vec<_> = std::fs::read_dir(&corpus).unwrap().flatten().collect();
    igs.sort_by_key(|e| e.file_name());
    for ig in igs.into_iter().filter(|e| e.path().is_dir()) {
        let mut files: Vec<_> = std::fs::read_dir(ig.path()).unwrap().flatten().collect();
        files.sort_by_key(|e| e.file_name());
        for f in files {
            let rel = format!(
                "{}/{}",
                ig.file_name().to_string_lossy(),
                f.file_name().to_string_lossy()
            );
            if !rel.ends_with(".json") {
                continue;
            }
            let errors = example_errors(&r, &corpus, &rel);
            report.insert(rel, json!(errors));
        }
    }
    std::fs::write(out, serde_json::to_vec_pretty(&Value::Object(report)).unwrap()).unwrap();
}
