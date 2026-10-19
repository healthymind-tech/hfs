//! Display, code-system, extensible-binding, narrative and best-practice
//! checks, aligned with the HL7 Java validator (stream P2-R), on the
//! embedded R4 pack.
//!
//! Run with: `cargo test -p helios-fhir-validator --features fhirpath --test terminology_checks`

#![cfg(all(feature = "fhirpath", feature = "R4"))]

use async_trait::async_trait;
use helios_fhir::FhirVersion;
use helios_fhir_validator::fhirpath_effects::FhirPathConstraintEvaluator;
use helios_fhir_validator::packs::core_registry;
use helios_fhir_validator::{
    CodedValue, EffectHandlers, ErrorKind, Severity, TerminologyError, TerminologyProvider,
    TxCheck, TxVerdict, ValidationError, ValidationOptions, Validator, core_terminology,
};
use serde_json::{Value, json};
use std::sync::Mutex;

async fn run(resource: &Value, handlers: &EffectHandlers<'_>) -> Vec<ValidationError> {
    Validator::new(core_registry(FhirVersion::R4))
        .validate(
            resource,
            FhirVersion::R4,
            &ValidationOptions::default(),
            handlers,
        )
        .await
}

fn of_kind(issues: &[ValidationError], kind: ErrorKind) -> Vec<&ValidationError> {
    issues.iter().filter(|e| e.kind == kind).collect()
}

fn observation(category_display: &str) -> Value {
    json!({
        "resourceType": "Observation", "status": "final",
        "category": [{"coding": [{
            "system": "http://terminology.hl7.org/CodeSystem/observation-category",
            "code": "vital-signs", "display": category_display}]}],
        "code": {"text": "x"},
        "subject": {"reference": "Patient/p"},
        "performer": [{"reference": "Practitioner/x"}],
        "effectiveDateTime": "2024-01-01"
    })
}

/// A wrong display on any coding (here an unbound-strength one) is a
/// display issue whose severity follows the setting; the right display, in
/// any case, is fine.
#[tokio::test]
async fn wrong_display_on_any_coding_follows_the_display_setting() {
    let tx = core_terminology(FhirVersion::R4);
    let mut handlers = EffectHandlers {
        terminology: Some(tx.as_ref()),
        ..Default::default()
    };
    let issues = run(&observation("Vital Signs Wrong"), &handlers).await;
    let wrong = of_kind(&issues, ErrorKind::DisplayMismatch);
    assert_eq!(wrong.len(), 1, "{issues:#?}");
    assert_eq!(wrong[0].severity, Severity::Warning);
    assert_eq!(wrong[0].path, "Observation.category.0.coding.0.display");
    assert!(
        wrong[0]
            .message
            .starts_with("Wrong Display Name 'Vital Signs Wrong'")
    );

    handlers.display_issues = Severity::Error;
    let issues = run(&observation("Vital Signs Wrong"), &handlers).await;
    let wrong = of_kind(&issues, ErrorKind::DisplayMismatch);
    assert_eq!(wrong.len(), 1, "{issues:#?}");
    assert_eq!(wrong[0].severity, Severity::Error);

    for fine in ["Vital Signs", "vital  signs"] {
        let issues = run(&observation(fine), &handlers).await;
        assert!(issues.is_empty(), "{fine}: {issues:#?}");
    }
}

/// Answers every check from a script and records what it was asked.
struct Scripted {
    calls: Mutex<usize>,
    asked: Mutex<Vec<String>>,
    /// Value sets (substring of the canonical) whose membership checks fail.
    reject_value_sets: &'static [&'static str],
    /// Code systems reported unknown.
    unknown_systems: &'static [&'static str],
    /// A display reported wrong on every coding check that has one.
    wrong_display: bool,
}

impl Scripted {
    fn new() -> Self {
        Self {
            calls: Mutex::new(0),
            asked: Mutex::new(Vec::new()),
            reject_value_sets: &[],
            unknown_systems: &[],
            wrong_display: false,
        }
    }
}

#[async_trait]
impl TerminologyProvider for Scripted {
    async fn validate_code(&self, _: &str, _: &CodedValue) -> Result<bool, TerminologyError> {
        panic!("the validator must ask through check()");
    }

    async fn check(&self, checks: &[TxCheck<'_>]) -> Vec<Result<TxVerdict, TerminologyError>> {
        *self.calls.lock().unwrap() += 1;
        checks
            .iter()
            .map(|c| {
                Ok(match c {
                    TxCheck::InValueSet(vs, _) => {
                        self.asked.lock().unwrap().push(format!("vs {vs}"));
                        TxVerdict::member(!self.reject_value_sets.iter().any(|r| vs.contains(r)))
                    }
                    TxCheck::Coding(coding) => {
                        let system = coding["system"].as_str().unwrap_or_default();
                        self.asked.lock().unwrap().push(format!("cs {system}"));
                        // `system|version` entries: only that version is unknown.
                        let versioned = coding["version"]
                            .as_str()
                            .map(|v| format!("{system}|{v}"))
                            .unwrap_or_default();
                        TxVerdict {
                            valid: true,
                            wrong_display: (self.wrong_display && coding.get("display").is_some())
                                .then(|| "Wrong Display Name 'x'".to_string()),
                            unknown_system: self.unknown_systems.contains(&system)
                                || self.unknown_systems.contains(&versioned.as_str()),
                            unknown_code: None,
                        }
                    }
                })
            })
            .collect()
    }
}

/// A wrong display never makes the code invalid: the binding passes and
/// only a display issue remains — NGS specimen types `Bld`/`Tiss`.
#[tokio::test]
async fn wrong_display_is_not_a_binding_failure() {
    let mut tx = Scripted::new();
    tx.wrong_display = true;
    let handlers = EffectHandlers {
        terminology: Some(&tx),
        ..Default::default()
    };
    let specimen = json!({"resourceType": "Specimen",
        "type": {"coding": [{"system": "http://loinc.org", "code": "LP7057-5", "display": "Bld"}]}});
    let issues = run(&specimen, &handlers).await;
    assert_eq!(issues.len(), 1, "{issues:#?}");
    assert_eq!(issues[0].kind, ErrorKind::DisplayMismatch);
    assert_eq!(issues[0].path, "Specimen.type.coding.0.display");
}

/// Bindings and codings of one resource — a whole Bundle — cost one
/// provider call; every coding with a system and code is asked about (its
/// code system must define the code, HL7 validator test case
/// contract-binding-test).
#[tokio::test]
async fn one_terminology_call_per_resource() {
    let tx = Scripted::new();
    let handlers = EffectHandlers {
        terminology: Some(&tx),
        check_extensible: true,
        ..Default::default()
    };
    let loinc = |code: &str, display: Option<&str>| {
        let mut c = json!({"system": "http://loinc.org", "code": code});
        if let Some(d) = display {
            c["display"] = json!(d);
        }
        c
    };
    let bundle = json!({"resourceType": "Bundle", "type": "collection", "entry": [
        {"fullUrl": "urn:uuid:1", "resource": {"resourceType": "Observation", "status": "final",
            "code": {"coding": [loinc("2339-0", Some("Glucose"))]},
            "subject": {"reference": "Patient/p"}, "performer": [{"reference": "Practitioner/x"}]}},
        {"fullUrl": "urn:uuid:2", "resource": {"resourceType": "Observation", "status": "final",
            "code": {"coding": [loinc("718-7", None)]},
            "interpretation": [{"coding": [{
                "system": "http://terminology.hl7.org/CodeSystem/v3-ObservationInterpretation",
                "code": "N"}]}],
            "subject": {"reference": "Patient/p"}, "performer": [{"reference": "Practitioner/x"}]}}
    ]});
    let issues = run(&bundle, &handlers).await;
    assert!(issues.is_empty(), "{issues:#?}");
    assert_eq!(*tx.calls.lock().unwrap(), 1);
    let asked = tx.asked.lock().unwrap();
    assert!(
        asked.contains(&"cs http://loinc.org".to_string()),
        "{asked:?}"
    );
    assert_eq!(
        asked.iter().filter(|a| *a == "cs http://loinc.org").count(),
        2,
        "the display-less LOINC coding is asked too: {asked:?}"
    );
    assert!(
        asked
            .iter()
            .any(|a| a.contains("v3-ObservationInterpretation")),
        "an HL7-namespace system is always asked: {asked:?}"
    );
}

/// An unresolvable code system in the HL7 namespaces is an error on
/// `Coding.system` (HL7 validator Type_Specific_Checks_DT_URL_Resolve); a
/// `system|version` there never resolves; elsewhere an unknown system says
/// nothing.
#[tokio::test]
async fn unknown_hl7_code_systems_are_errors() {
    let mut tx = Scripted::new();
    tx.unknown_systems = &[
        "http://terminology.hl7.org/CodeSystem/v3-ActEncounterCode",
        "http://example.org/cs",
    ];
    let handlers = EffectHandlers {
        terminology: Some(&tx),
        ..Default::default()
    };
    let encounter = json!({"resourceType": "Encounter", "status": "finished",
        "class": {"system": "http://terminology.hl7.org/CodeSystem/v3-ActEncounterCode", "code": "AMB"},
        "type": [{"coding": [{"system": "http://example.org/cs", "code": "x", "display": "X"}]}]});
    let issues = run(&encounter, &handlers).await;
    let unknown = of_kind(&issues, ErrorKind::UnknownCodeSystem);
    assert_eq!(unknown.len(), 1, "{issues:#?}");
    assert_eq!(unknown[0].severity, Severity::Error);
    assert_eq!(unknown[0].path, "Encounter.class.system");
    assert_eq!(
        unknown[0].message,
        "No definition could be found for URL value 'http://terminology.hl7.org/CodeSystem/v3-ActEncounterCode'"
    );

    let practitioner = json!({"resourceType": "Practitioner", "qualification": [{"code": {"coding": [{
        "system": "http://terminology.hl7.org/CodeSystem/v2-0360|2.7", "code": "MD", "display": "Doctor of Medicine"}]}}]});
    let tx = Scripted::new();
    let handlers = EffectHandlers {
        terminology: Some(&tx),
        ..Default::default()
    };
    let issues = run(&practitioner, &handlers).await;
    let unknown = of_kind(&issues, ErrorKind::UnknownCodeSystem);
    assert_eq!(unknown.len(), 1, "{issues:#?}");
    assert_eq!(
        unknown[0].path,
        "Practitioner.qualification.0.code.coding.0.system"
    );
    assert!(
        !tx.asked
            .lock()
            .unwrap()
            .iter()
            .any(|a| a.contains("v2-0360")),
        "a versioned system URL is judged without asking"
    );
}

/// `extensible` bindings warn when enabled and stay silent otherwise; a
/// text-only concept satisfies them.
#[tokio::test]
async fn extensible_bindings_warn_when_enabled() {
    let mut tx = Scripted::new();
    tx.reject_value_sets = &["v3-ActEncounterCode", "administrative-gender"];
    // Condition.code: extensible-free (example) in core; Encounter.class is
    // extensible to v3-ActEncounterCode in R4.
    let encounter = json!({"resourceType": "Encounter", "status": "finished",
        "class": {"system": "http://example.org/casetype", "code": "01"}});
    let off = EffectHandlers {
        terminology: Some(&tx),
        ..Default::default()
    };
    let issues = run(&encounter, &off).await;
    assert!(issues.is_empty(), "{issues:#?}");

    let on = EffectHandlers {
        terminology: Some(&tx),
        check_extensible: true,
        ..Default::default()
    };
    let issues = run(&encounter, &on).await;
    assert_eq!(issues.len(), 1, "{issues:#?}");
    assert_eq!(issues[0].kind, ErrorKind::TerminologyBinding);
    assert_eq!(issues[0].severity, Severity::Warning);
    assert_eq!(issues[0].path, "Encounter.class");
    assert!(
        issues[0].message.starts_with(
            "The Coding provided (http://example.org/casetype#01) was not found in the value set"
        ),
        "{}",
        issues[0].message
    );

    // A required binding failing is still an error, never a warning (a
    // bare code also draws "System URI could not be determined").
    let gender = json!({"resourceType": "Patient", "gender": "male"});
    let issues = run(&gender, &on).await;
    assert_eq!(issues.len(), 2, "{issues:#?}");
    assert!(issues.iter().all(|i| i.severity == Severity::Error));
}

/// Narrative links and best-practice warnings run when the handlers ask for
/// them.
#[tokio::test]
async fn narrative_links_and_best_practice_are_opt_in() {
    let obs = json!({"resourceType": "Observation", "status": "final", "code": {"text": "x"},
        "subject": {"reference": "Patient/p"},
        "text": {"status": "generated",
            "div": "<div xmlns=\"http://www.w3.org/1999/xhtml\"><p><a href=\"#Patient_p\">See above</a></p></div>"}});
    let issues = run(&obs, &EffectHandlers::default()).await;
    assert!(issues.is_empty(), "{issues:#?}");

    let handlers = EffectHandlers {
        narrative_links: true,
        best_practice: true,
        ..Default::default()
    };
    let issues = run(&obs, &handlers).await;
    let narrative = of_kind(&issues, ErrorKind::Narrative);
    assert_eq!(narrative.len(), 1, "{issues:#?}");
    assert_eq!(narrative[0].severity, Severity::Error);
    assert_eq!(narrative[0].path, "Observation.text.div");
    let bp = of_kind(&issues, ErrorKind::BestPractice);
    assert_eq!(bp.len(), 1, "{issues:#?}");
    assert_eq!(bp[0].severity, Severity::Warning);
    assert!(bp[0].message.contains("should have a performer"));
}

/// cpb-16 on a `kind = requirements` CapabilityStatement looks at
/// `implementation`, not `implementationGuide` (stream M saw it fire).
#[tokio::test]
async fn cpb16_does_not_confuse_implementation_with_implementation_guide() {
    let evaluator = FhirPathConstraintEvaluator::new();
    let suppress = vec!["dom-6".to_string()];
    let handlers = EffectHandlers {
        constraints: Some(&evaluator),
        suppress_constraints: &suppress,
        ..Default::default()
    };
    let mut cs = json!({
        "resourceType": "CapabilityStatement", "status": "active", "date": "2024-01-01",
        "kind": "requirements", "fhirVersion": "4.0.1", "format": ["json"],
        "implementationGuide": ["http://example.org/ImplementationGuide/x"],
        "rest": [{"mode": "server"}]
    });
    let issues = run(&cs, &handlers).await;
    let cpb16 = |issues: &[ValidationError]| {
        issues
            .iter()
            .any(|e| e.extra.get("constraint") == Some(&json!("cpb-16")))
    };
    assert!(!cpb16(&issues), "{issues:#?}");

    cs["implementation"] = json!({"description": "x"});
    let issues = run(&cs, &handlers).await;
    assert!(cpb16(&issues), "{issues:#?}");
}

/// `pat-ms-3.0.0` (HL7 validator test cases): an unknown version of a known
/// HL7 code system is a warning about the version, not an unresolvable
/// system URL; a versioned Coding of an unknown HL7 system is still an
/// error.
#[tokio::test]
async fn unknown_version_of_a_known_code_system_warns() {
    let patient = |system: &str| {
        json!({"resourceType": "Patient", "maritalStatus": {"coding": [{
            "system": system, "version": "3.0.0", "code": "M"}]}})
    };
    let mut tx = Scripted::new();
    tx.unknown_systems = &[
        "http://terminology.hl7.org/CodeSystem/v3-MaritalStatus|3.0.0",
        "http://terminology.hl7.org/CodeSystem/nothing-here",
    ];
    let handlers = EffectHandlers {
        terminology: Some(&tx),
        ..Default::default()
    };
    let issues = run(
        &patient("http://terminology.hl7.org/CodeSystem/v3-MaritalStatus"),
        &handlers,
    )
    .await;
    let unknown = of_kind(&issues, ErrorKind::UnknownCodeSystem);
    assert_eq!(unknown.len(), 1, "{issues:#?}");
    assert_eq!(unknown[0].severity, Severity::Warning);
    assert_eq!(unknown[0].path, "Patient.maritalStatus.coding.0.version");

    let issues = run(
        &patient("http://terminology.hl7.org/CodeSystem/nothing-here"),
        &handlers,
    )
    .await;
    let unknown = of_kind(&issues, ErrorKind::UnknownCodeSystem);
    assert_eq!(unknown.len(), 1, "{issues:#?}");
    assert_eq!(unknown[0].severity, Severity::Error);
}

/// `contract-binding-test` (HL7 validator test cases): a code its (complete,
/// embedded) code system does not define is an error, bound or not.
#[tokio::test]
async fn unknown_codes_in_known_code_systems_are_errors() {
    let tx = core_terminology(FhirVersion::R4);
    let handlers = EffectHandlers {
        terminology: Some(tx.as_ref()),
        ..Default::default()
    };
    let contract = json!({"resourceType": "Contract",
        "legalState": {"coding": [{"system": "http://hl7.org/fhir/contract-legalstate",
                                   "code": "invalid-code-test"}]}});
    let issues = run(&contract, &handlers).await;
    let unknown: Vec<_> = issues
        .iter()
        .filter(|e| e.message.starts_with("Unknown code 'invalid-code-test'"))
        .collect();
    assert_eq!(unknown.len(), 1, "{issues:#?}");
    assert_eq!(unknown[0].severity, Severity::Error);
    assert_eq!(unknown[0].path, "Contract.legalState.coding.0.code");

    let good = json!({"resourceType": "Contract",
        "legalState": {"coding": [{"system": "http://hl7.org/fhir/contract-legalstate",
                                   "code": "amended"}]}});
    let issues = run(&good, &handlers).await;
    assert!(
        !issues.iter().any(|e| e.message.starts_with("Unknown code")),
        "{issues:#?}"
    );
}

/// `capabilitystatement-measure-processor` (HL7 validator test cases):
/// CapabilityStatement.format accepts the spec's short forms xml/json/ttl;
/// anything else still has to be a mime type from the bound value set.
#[tokio::test]
async fn capability_statement_format_short_forms() {
    let mut tx = Scripted::new();
    tx.reject_value_sets = &["mimetypes"];
    let handlers = EffectHandlers {
        terminology: Some(&tx),
        ..Default::default()
    };
    let cs = |format: &str| {
        json!({"resourceType": "CapabilityStatement", "status": "active", "date": "2024-01-01",
               "kind": "instance", "fhirVersion": "4.0.1", "format": ["xml", format]})
    };
    let issues = run(&cs("json"), &handlers).await;
    assert!(
        of_kind(&issues, ErrorKind::TerminologyBinding).is_empty(),
        "{issues:#?}"
    );
    let issues = run(&cs("jason"), &handlers).await;
    assert!(
        !of_kind(&issues, ErrorKind::TerminologyBinding).is_empty(),
        "{issues:#?}"
    );
}
