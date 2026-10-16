//! Display, code-system and extensible-binding checks, aligned with the HL7 Java validator (stream P2-R), on the
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
    assert!(wrong[0].message.starts_with("Wrong Display Name 'Vital Signs Wrong'"));

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
                        TxVerdict {
                            valid: true,
                            wrong_display: (self.wrong_display && coding.get("display").is_some())
                                .then(|| "Wrong Display Name 'x'".to_string()),
                            unknown_system: self.unknown_systems.contains(&system),
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
/// provider call; codings without a display outside the HL7 namespaces are
/// not asked about.
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
        {"resource": {"resourceType": "Observation", "status": "final",
            "code": {"coding": [loinc("8867-4", Some("Heart rate"))]},
            "subject": {"reference": "Patient/p"}, "performer": [{"reference": "Practitioner/x"}]}},
        {"resource": {"resourceType": "Observation", "status": "final",
            "code": {"coding": [loinc("8310-5", None)]},
            "interpretation": [{"coding": [{
                "system": "http://terminology.hl7.org/CodeSystem/v3-ObservationInterpretation",
                "code": "N"}]}],
            "subject": {"reference": "Patient/p"}, "performer": [{"reference": "Practitioner/x"}]}}
    ]});
    let issues = run(&bundle, &handlers).await;
    assert!(issues.is_empty(), "{issues:#?}");
    assert_eq!(*tx.calls.lock().unwrap(), 1);
    let asked = tx.asked.lock().unwrap();
    assert!(asked.contains(&"cs http://loinc.org".to_string()), "{asked:?}");
    assert_eq!(
        asked.iter().filter(|a| *a == "cs http://loinc.org").count(),
        1,
        "the display-less LOINC coding is not asked: {asked:?}"
    );
    assert!(
        asked.iter().any(|a| a.contains("v3-ObservationInterpretation")),
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
        !tx.asked.lock().unwrap().iter().any(|a| a.contains("v2-0360")),
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
        issues[0]
            .message
            .starts_with("The Coding provided (http://example.org/casetype#01) was not found in the value set"),
        "{}",
        issues[0].message
    );

    // A required binding failing is still an error, never a warning.
    let gender = json!({"resourceType": "Patient", "gender": "male"});
    let issues = run(&gender, &on).await;
    assert_eq!(issues.len(), 1, "{issues:#?}");
    assert_eq!(issues[0].severity, Severity::Error);
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
