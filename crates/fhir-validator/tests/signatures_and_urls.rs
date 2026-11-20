//! Line V2 features: `Bundle.signature` verification, unresolvable
//! HL7-namespace uri values, `htmlChecks()` on rendering-xhtml values, and
//! the empty-invariant switch.
//!
//! The signed fixtures under `tests/fixtures/signatures/` come from
//! FHIR/fhir-test-cases (`validator/`, Apache License 2.0) and the Da Vinci
//! CDex 2.1.0 package; the HL7 Java validator's verdict on each is noted on
//! its test.
//!
//! Run with: `cargo test -p helios-fhir-validator --features fhirpath --test signatures_and_urls`

#![cfg(all(feature = "fhirpath", feature = "R4"))]

use helios_fhir::FhirVersion;
use helios_fhir_validator::fhirpath_effects::FhirPathConstraintEvaluator;
use helios_fhir_validator::packs::core_registry;
use helios_fhir_validator::signature::{SignatureOptions, SignatureTrust, parse_key_file};
use helios_fhir_validator::{
    EffectHandlers, ErrorKind, SchemaResolver, Severity, ValidationError, ValidationOptions, Validator,
};
use serde_json::{Value, json};

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/signatures/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

async fn validate(resource: &Value, sig: Option<&SignatureOptions>, empty_fails: bool) -> Vec<ValidationError> {
    let validator = Validator::new(core_registry(FhirVersion::R4));
    let evaluator = FhirPathConstraintEvaluator::new().with_empty_fails(empty_fails);
    let suppress = vec!["dom-6".to_string()];
    let handlers = EffectHandlers {
        constraints: Some(&evaluator),
        suppress_constraints: &suppress,
        signatures: sig,
        ..Default::default()
    };
    let opts = ValidationOptions {
        hl7_urls: Some(Severity::Error),
        ..Default::default()
    };
    validator
        .validate(resource, FhirVersion::R4, &opts, &handlers)
        .await
}

fn of_kind(issues: &[ValidationError], kind: ErrorKind) -> Vec<(Severity, String)> {
    issues
        .iter()
        .filter(|e| e.kind == kind)
        .map(|e| (e.severity, e.message.clone()))
        .collect()
}

fn signature_errors(issues: &[ValidationError]) -> Vec<String> {
    of_kind(issues, ErrorKind::Signature)
        .into_iter()
        .filter(|(s, _)| *s == Severity::Error)
        .map(|(_, m)| m)
        .collect()
}

fn has_message(issues: &[ValidationError], text: &str) -> bool {
    issues.iter().any(|e| e.message.contains(text))
}

/// fhir-test-cases `bundle-signed-good` (Java: "The signature verified OK"):
/// detached JWS, RS256, `x5c`, `canonicalization=…/json`.
#[tokio::test]
async fn detached_jws_over_canonical_json_verifies() {
    let bundle = fixture("bundle-signed-good.json");
    let issues = validate(&bundle, Some(&SignatureOptions::default()), false).await;
    assert!(signature_errors(&issues).is_empty(), "{issues:#?}");
    assert!(has_message(&issues, "The signature verified OK"), "{issues:#?}");
    assert!(has_message(
        &issues,
        "The content was signed by OU=IG Publisher,L=Ann Arbor,CN=hl7.org,O=HL7,ST=Missouri,C=us (and this has been verified by the signature)"
    ));
}

/// Changing one signed value breaks the signature.
#[tokio::test]
async fn tampered_content_fails() {
    let mut bundle = fixture("bundle-signed-good.json");
    bundle["timestamp"] = json!("2001-01-01T00:00:00Z");
    let issues = validate(&bundle, Some(&SignatureOptions::default()), false).await;
    assert_eq!(
        signature_errors(&issues),
        vec!["The signature did not verify against the provided certificate".to_string()],
        "{issues:#?}"
    );
    // ... unless signatures are not checked.
    let off = validate(&bundle, None, false).await;
    assert!(of_kind(&off, ErrorKind::Signature).is_empty());
}

/// fhir-test-cases `bundle-sig-2` (Java: verified OK, payload warning):
/// JWS with an embedded payload, `#document` canonicalization (Bundle.id and
/// Bundle.meta excluded).
#[tokio::test]
async fn document_canonicalization_with_embedded_payload_verifies() {
    let bundle = fixture("bundle-sig-2.json");
    let issues = validate(&bundle, Some(&SignatureOptions::default()), false).await;
    assert!(signature_errors(&issues).is_empty(), "{issues:#?}");
    assert!(has_message(&issues, "The signature verified OK"));
    assert!(of_kind(&issues, ErrorKind::Signature).iter().any(|(s, m)| *s == Severity::Warning
        && m == "The signature has a payload, but the signature should be a detached signature with no payload"));
}

/// fhir-test-cases `sig-time-example` (Java: "The signature did not verify
/// against the provided certificate").
#[tokio::test]
async fn kanta_sig_time_example_does_not_verify() {
    let bundle = fixture("sig-time-example.json");
    let issues = validate(&bundle, Some(&SignatureOptions::default()), false).await;
    assert_eq!(
        signature_errors(&issues),
        vec!["The signature did not verify against the provided certificate".to_string()],
        "{issues:#?}"
    );
}

/// Da Vinci CDex 2.1.0 `Bundle-cdex-searchbundle-digital-sig-example`
/// (Java 7.0.1 difftest: BUNDLE_SIGNATURE_SIG_FAIL_CERT).
#[tokio::test]
async fn cdex_search_bundle_example_does_not_verify() {
    let bundle = fixture("cdex-searchbundle-digital-sig-example.json");
    let issues = validate(&bundle, Some(&SignatureOptions::default()), false).await;
    assert_eq!(
        signature_errors(&issues),
        vec!["The signature did not verify against the provided certificate".to_string()],
        "{issues:#?}"
    );
    // On the Bundle, also when the Bundle is nested in Parameters.
    let params = json!({"resourceType": "Parameters", "parameter": [{"name": "b", "resource": bundle}]});
    let issues = validate(&params, Some(&SignatureOptions::default()), false).await;
    assert!(
        issues.iter().any(|e| e.kind == ErrorKind::Signature
            && e.severity == Severity::Error
            && e.path == "Parameters.parameter.0.resource"),
        "{issues:#?}"
    );
}

/// `trust = listed`: a verified signature by a certificate nobody configured
/// is reported; configuring that certificate (here as a JWK set carrying the
/// key) clears it, and a `kid` is looked up in the configured keys.
#[tokio::test]
async fn listed_trust_and_kid_lookup() {
    let bundle = fixture("bundle-signed-good.json");
    let untrusted = SignatureOptions {
        trust: SignatureTrust::Listed,
        untrusted_severity: Severity::Error,
        ..Default::default()
    };
    let issues = validate(&bundle, Some(&untrusted), false).await;
    assert!(
        signature_errors(&issues)
            .iter()
            .any(|m| m.starts_with("The signing certificate is not trusted by this server")),
        "{issues:#?}"
    );

    // The x5c certificate as a PEM trust anchor.
    let header_b64 = {
        use base64::Engine as _;
        let data = bundle["signature"]["data"].as_str().unwrap();
        let jws = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(data).unwrap()).unwrap();
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(jws.split('.').next().unwrap())
            .unwrap();
        let header: Value = serde_json::from_slice(&header).unwrap();
        header["x5c"][0].as_str().unwrap().to_string()
    };
    let pem = format!("-----BEGIN CERTIFICATE-----\n{header_b64}\n-----END CERTIFICATE-----\n");
    let trusted = SignatureOptions {
        keys: parse_key_file(pem.as_bytes()).unwrap(),
        trust: SignatureTrust::Listed,
        untrusted_severity: Severity::Error,
    };
    let issues = validate(&bundle, Some(&trusted), false).await;
    assert!(signature_errors(&issues).is_empty(), "{issues:#?}");
}

/// Hrex `Parameters-member-match-in` / `Bundle-subscription-notification-*`
/// (Java: Type_Specific_Checks_DT_URL_Resolve): a uri in the HL7 FHIR
/// namespace with no resource-type segment names nothing.
#[tokio::test]
async fn hl7_namespace_uris_that_name_nothing_are_errors() {
    let consent = json!({
        "resourceType": "Consent", "status": "active",
        "scope": {"coding": [{"system": "http://terminology.hl7.org/CodeSystem/consentscope", "code": "patient-privacy"}]},
        "category": [{"coding": [{"system": "http://loinc.org", "code": "64292-6"}]}],
        "policy": [
            {"uri": "http://hl7.org/fhir/us/davinci-hrex/StructureDefinition-hrex-consent.html#regular"},
            {"uri": "http://hl7.org/fhir/us/davinci-hrex/StructureDefinition/hrex-consent"},
            {"uri": "http://example.org/policy"}
        ]
    });
    let issues = validate(&consent, None, false).await;
    let urls = of_kind(&issues, ErrorKind::UrlUnresolved);
    assert_eq!(
        urls,
        vec![(
            Severity::Error,
            "No definition could be found for URL value 'http://hl7.org/fhir/us/davinci-hrex/StructureDefinition-hrex-consent.html#regular'".to_string()
        )],
        "{issues:#?}"
    );
    let params = json!({"resourceType": "Parameters", "parameter": [
        {"name": "topic", "valueUri": "http://hl7.org/fhir/us/davinci-hrex/SubscriptionTopic/Task"},
        {"name": "sid", "valueUri": "http://hl7.org/fhir/sid/us-npi"},
        {"name": "sp", "valueUri": "http://hl7.org/fhir/SearchParameter/Patient-name"},
        {"name": "mapping", "valueUri": "http://hl7.org/fhir/w5"},
        {"name": "cs", "valueUri": "http://hl7.org/fhir/concept-properties#itemWeight"},
        {"name": "ig", "valueUri": "http://hl7.org/fhir/us/core"}
    ]});
    let issues = validate(&params, None, false).await;
    let urls = of_kind(&issues, ErrorKind::UrlUnresolved);
    assert_eq!(urls.len(), 1, "{issues:#?}");
    assert!(urls[0].1.contains("SubscriptionTopic/Task"));
}

/// SDC `Questionnaire-questionnaire-sdc-profile-example-image-options` (Java:
/// xhtml-ext-1 failed). Invariants below a primitive's `_name` sidecar were
/// evaluated on nothing (the FHIRPath focus `_display` does not exist), so
/// they always passed. Also, the R4 core definition of rendering-xhtml
/// (embedded pack, 4.0.1) simply has no xhtml-ext-1 — that invariant comes
/// with hl7.fhir.uv.extensions (5.x), which the Java validator loads and HFS
/// only lets replace a core definition with `HFS_VALIDATION_SHADOW_CORE`.
/// Here the 5.x-style definition is layered over the core pack.
#[tokio::test]
async fn rendering_xhtml_on_a_primitive_sidecar_runs_html_checks() {
    use helios_fhir_validator::{CompositeResolver, SchemaRegistry};
    let sd = json!({
        "resourceType": "StructureDefinition",
        "url": "http://hl7.org/fhir/StructureDefinition/rendering-xhtml", "version": "5.3.0",
        "name": "RenderingXhtml", "status": "active", "kind": "complex-type", "abstract": false,
        "context": [{"type": "element", "expression": "string"}],
        "type": "Extension", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Extension",
        "differential": {"element": [
            {"id": "Extension", "path": "Extension", "max": "1"},
            {"id": "Extension.extension", "path": "Extension.extension", "max": "0"},
            {"id": "Extension.url", "path": "Extension.url",
             "fixedUri": "http://hl7.org/fhir/StructureDefinition/rendering-xhtml"},
            {"id": "Extension.value[x]", "path": "Extension.value[x]", "min": 1,
             "type": [{"code": "string"}],
             "constraint": [{"key": "xhtml-ext-1", "severity": "error",
                "human": "The XHTML contained here SHALL abide by the same constraints as narrative",
                "expression": "htmlChecks()"}]}
        ]}
    });
    let mut overlay = SchemaRegistry::new();
    assert!(overlay.insert(helios_fhir_validator::converter::convert(&sd).unwrap().schema));
    let resolver: std::sync::Arc<dyn SchemaResolver> = std::sync::Arc::new(CompositeResolver::new(vec![
        std::sync::Arc::new(overlay),
        core_registry(FhirVersion::R4),
    ]));
    let validator = Validator::new(resolver);
    let evaluator = FhirPathConstraintEvaluator::new();
    let handlers = EffectHandlers {
        constraints: Some(&evaluator),
        ..Default::default()
    };
    let q = |xhtml: &str| json!({
        "resourceType": "Questionnaire", "status": "draft",
        "item": [{"linkId": "1", "type": "choice", "answerOption": [{"valueCoding": {
            "system": "http://example.org", "code": "sad", "display": "Sad",
            "_display": {"extension": [{
                "url": "http://hl7.org/fhir/StructureDefinition/rendering-xhtml",
                "valueString": xhtml
            }]}
        }}]}]
    });
    let opts = ValidationOptions::default();
    let bad = validator
        .validate(&q("<img src='data:image/png;base64,AAAA'/>"), FhirVersion::R4, &opts, &handlers)
        .await;
    assert!(
        bad.iter().any(|e| e.severity == Severity::Error && e.message.contains("xhtml-ext-1")),
        "{bad:#?}"
    );
    let good = validator
        .validate(
            &q("<div xmlns=\"http://www.w3.org/1999/xhtml\"><b>Sad</b></div>"),
            FhirVersion::R4,
            &opts,
            &handlers,
        )
        .await;
    assert!(!good.iter().any(|e| e.message.contains("xhtml-ext-1")), "{good:#?}");
}

/// `encounter-period` (Java: per-1 failed): mixed-precision start/end make
/// the comparison empty; the Java validator treats an empty invariant result
/// as a failure, HFS only with `empty_fails`.
#[tokio::test]
async fn empty_invariant_results_fail_only_when_asked() {
    let enc = json!({
        "resourceType": "Encounter", "status": "finished",
        "class": {"system": "http://terminology.hl7.org/CodeSystem/v3-ActCode", "code": "AMB"},
        "period": {"start": "2021-01-01T10:00:00Z", "end": "2021-01-01"}
    });
    let pass = validate(&enc, None, false).await;
    assert!(!pass.iter().any(|e| e.message.contains("per-1")), "{pass:#?}");
    let fail = validate(&enc, None, true).await;
    assert!(
        fail.iter().any(|e| e.severity == Severity::Error && e.message.contains("per-1")),
        "{fail:#?}"
    );
}


/// mCODE / genomics bundles (Java: Reference_REF_CantMatchChoice at every
/// reference to an entry that fails its profile on an invariant): the entry's
/// own failure is repeated at the references once the effects pass found it.
#[tokio::test]
async fn references_to_entries_failing_their_profile_on_invariants_are_reported() {
    use helios_fhir_validator::{CompositeResolver, SchemaRegistry};
    let pat = json!({
        "resourceType": "StructureDefinition", "url": "http://example.org/StructureDefinition/pat-p",
        "name": "PatP", "status": "active", "kind": "resource", "abstract": false, "type": "Patient",
        "derivation": "constraint", "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
        "differential": {"element": [{"id": "Patient", "path": "Patient",
            "constraint": [{"key": "pat-p-1", "severity": "error", "human": "a name", "expression": "name.exists()"}]}]}
    });
    let obs = json!({
        "resourceType": "StructureDefinition", "url": "http://example.org/StructureDefinition/obs-p",
        "name": "ObsP", "status": "active", "kind": "resource", "abstract": false, "type": "Observation",
        "derivation": "constraint", "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Observation",
        "differential": {"element": [{"id": "Observation.subject", "path": "Observation.subject",
            "type": [{"code": "Reference", "targetProfile": ["http://example.org/StructureDefinition/pat-p"]}]}]}
    });
    let mut overlay = SchemaRegistry::new();
    for sd in [&pat, &obs] {
        assert!(overlay.insert(helios_fhir_validator::converter::convert(sd).unwrap().schema));
    }
    let resolver: std::sync::Arc<dyn SchemaResolver> = std::sync::Arc::new(CompositeResolver::new(vec![
        std::sync::Arc::new(overlay),
        core_registry(FhirVersion::R4),
    ]));
    let validator = Validator::new(resolver);
    let evaluator = FhirPathConstraintEvaluator::new();
    let handlers = EffectHandlers {
        constraints: Some(&evaluator),
        ..Default::default()
    };
    let bundle = |name: Value| {
        let mut patient = json!({"resourceType": "Patient", "id": "1",
            "meta": {"profile": ["http://example.org/StructureDefinition/pat-p"]}});
        if !name.is_null() {
            patient["name"] = name;
        }
        json!({"resourceType": "Bundle", "type": "collection", "entry": [
            {"fullUrl": "http://example.org/fhir/Observation/1", "resource": {
                "resourceType": "Observation", "id": "1", "status": "final", "code": {"text": "x"},
                "meta": {"profile": ["http://example.org/StructureDefinition/obs-p"]},
                "subject": {"reference": "Patient/1"}}},
            {"fullUrl": "http://example.org/fhir/Patient/1", "resource": patient}
        ]})
    };
    let opts = ValidationOptions::default();
    let bad = validator.validate(&bundle(Value::Null), FhirVersion::R4, &opts, &handlers).await;
    assert!(bad.iter().any(|e| e.message.contains("pat-p-1")), "{bad:#?}");
    assert!(
        bad.iter().any(|e| e.path == "Bundle.entry.0.resource.subject"
            && e.message == "Unable to find a profile match for Patient/1 among choices: http://example.org/StructureDefinition/pat-p"),
        "{bad:#?}"
    );
    let good = validator.validate(&bundle(json!([{"family": "X"}])), FhirVersion::R4, &opts, &handlers).await;
    assert!(!good.iter().any(|e| e.message.contains("Unable to find a profile match")), "{good:#?}");
}

/// The invariants decided natively (ele-1, ext-1, dom-2/4/5, bdl-1..5/7/8)
/// give the same verdicts as their FHIRPath.
#[tokio::test]
async fn natively_decided_invariants_agree_with_fhirpath() {
    use helios_fhir_validator::{ConstraintEvaluator, ConstraintOutcome, Deferred, DeferredConstraint};
    let cases = [
        json!({"resourceType": "Bundle", "type": "collection", "total": 3, "entry": [
            {"fullUrl": "http://x/Patient/1", "resource": {"resourceType": "Patient", "id": "1",
                "contained": [{"resourceType": "Organization", "id": "o", "meta": {"versionId": "2"},
                    "contained": [{"resourceType": "Organization", "id": "p"}]}],
                "managingOrganization": {"reference": "#o"},
                "text": {"status": "generated", "div": "<div xmlns=\"http://www.w3.org/1999/xhtml\"><p>ok</p></div>"},
                "generalPractitioner": [{"reference": "#missing"}, {"identifier": {"value": "x"}}, {"reference": "Practitioner/1"}],
                "name": [{"given": ["A", null], "_given": [null, {"extension": [{"url": "http://x/e", "valueString": "x"}]}]}],
                "_birthDate": {"id": "only"},
                "extension": [{"url": "http://x/e", "valueString": "x", "extension": [{"url": "a", "valueBoolean": true}]}]}},
            {"fullUrl": "http://x/Patient/1", "request": {"method": "POST", "url": "Patient"},
             "search": {"mode": "match"}, "response": {"status": "201"}},
            {"fullUrl": "http://x/Patient/2/_history/1"},
            {}
        ]}),
        json!({"resourceType": "Bundle", "type": "transaction", "entry": [
            {"fullUrl": "urn:uuid:1", "resource": {"resourceType": "Patient", "active": true},
             "request": {"method": "POST", "url": "Patient"}},
            {"fullUrl": "urn:uuid:2", "resource": {"resourceType": "Patient", "active": true,
                "text": {"status": "generated", "div": "<div xmlns=\"http://www.w3.org/1999/xhtml\"><script>x</script></div>"},
                "contained": [{"resourceType": "Organization", "id": "o2", "partOf": {"reference": "#o3"}},
                              {"resourceType": "Organization", "id": "o3",
                               "text": {"status": "generated", "div": "<div xmlns=\"http://www.w3.org/1999/xhtml\"> </div>"}}],
                "managingOrganization": {"reference": "#o2"}}}
        ]}),
    ];
    let validator = Validator::new(core_registry(FhirVersion::R4));
    let evaluator = FhirPathConstraintEvaluator::new();
    for resource in &cases {
        let outcome = validator.validate_sync(resource, &ValidationOptions::default());
        let native_ids = ["ele-1", "ext-1", "dom-2", "dom-4", "dom-5", "bdl-1", "bdl-2", "bdl-3", "bdl-4", "bdl-5", "bdl-7", "bdl-8", "ref-1", "txt-1", "txt-2"];
        let constraints: Vec<DeferredConstraint<'_>> = outcome
            .deferred
            .iter()
            .filter_map(|d| match d {
                Deferred::Constraint { path, id, expression, own_resource, .. }
                    if native_ids.contains(&id.as_str()) =>
                {
                    Some(DeferredConstraint { path, id, expression, own_resource: *own_resource })
                }
                _ => None,
            })
            .collect();
        assert!(constraints.len() > 10);
        let by_fhirpath = evaluator.evaluate_all(resource, FhirVersion::R4, &constraints);
        // The effects pass (native) reports a failure exactly where FHIRPath fails.
        let handlers = EffectHandlers { constraints: Some(&evaluator), ..Default::default() };
        let issues = validator.validate(resource, FhirVersion::R4, &ValidationOptions::default(), &handlers).await;
        for (c, outcome) in constraints.iter().zip(&by_fhirpath) {
            let reported = issues.iter().any(|e| {
                e.path == c.path && e.extra.get("constraint").and_then(Value::as_str) == Some(c.id)
            });
            // `_birthDate` with only an id: the walk reports ele-1 itself
            // (the typed model never sees it), FHIRPath passes it.
            if c.path.ends_with("_birthDate") {
                continue;
            }
            assert_eq!(
                reported,
                matches!(outcome, ConstraintOutcome::Failed),
                "{} at {}: fhirpath {outcome:?}, native reported {reported}",
                c.id,
                c.path
            );
        }
    }
}

/// fhir-test-cases `patient-value-present-*` / `patient-value-alternative-*`
/// (R5 value controls via the 5.0 cross-version extensions).
#[tokio::test]
async fn must_have_value_and_value_alternatives() {
    use helios_fhir_validator::{CompositeResolver, SchemaRegistry};
    let sd = |url: &str, ext: Value| json!({
        "resourceType": "StructureDefinition", "url": url, "name": "P", "status": "active",
        "kind": "resource", "abstract": false, "type": "Patient", "derivation": "constraint",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
        "differential": {"element": [{"id": "Patient.active", "path": "Patient.active", "min": 1, "extension": [ext]}]}
    });
    let present = sd("http://example.org/vp", json!({"url": "http://hl7.org/fhir/5.0/StructureDefinition/extension-ElementDefinition.mustHaveValue", "valueBoolean": true}));
    let alternative = sd("http://example.org/va", json!({"url": "http://hl7.org/fhir/5.0/StructureDefinition/extension-ElementDefinition.valueAlternatives", "valueCanonical": "http://hl7.org/fhir/StructureDefinition/data-absent-reason"}));
    let mut overlay = SchemaRegistry::new();
    for s in [&present, &alternative] {
        assert!(overlay.insert(helios_fhir_validator::converter::convert(s).unwrap().schema));
    }
    let resolver: std::sync::Arc<dyn SchemaResolver> = std::sync::Arc::new(CompositeResolver::new(vec![
        std::sync::Arc::new(overlay),
        core_registry(FhirVersion::R4),
    ]));
    let validator = Validator::new(resolver);
    let run = |ext_url: &str, profile: &str| {
        let patient = json!({"resourceType": "Patient", "_active": {"extension": [{"url": ext_url, "valueCode": "unknown"}]}});
        let opts = ValidationOptions { profiles: vec![profile.to_string()], ..Default::default() };
        validator.validate_sync(&patient, &opts).errors
    };
    let dar = "http://hl7.org/fhir/StructureDefinition/data-absent-reason";
    let nf = "http://hl7.org/fhir/StructureDefinition/iso21090-nullFlavor";
    let has = |errs: &[ValidationError], text: &str| errs.iter().any(|e| e.path == "Patient.active" && e.message.contains(text));
    assert!(has(&run(dar, "http://example.org/vp"), "requires that a value be present"));
    assert!(!has(&run(dar, "http://example.org/va"), "if a value is not present"));
    assert!(has(&run(nf, "http://example.org/va"), "if a value is not present"));
    let valued = json!({"resourceType": "Patient", "active": true});
    let opts = ValidationOptions { profiles: vec!["http://example.org/vp".into()], ..Default::default() };
    assert!(validator.validate_sync(&valued, &opts).errors.iter().all(|e| !e.message.contains("requires that")));
}

/// subscriptions-backport `Bundle-r4b-notification-*` (Java: one "does not
/// appear to be a FHIR resource" error): an entry of a resource type this
/// FHIR version does not have is reported once, its members are not walked.
#[test]
fn unknown_nested_resource_types_are_reported_once() {
    let bundle = json!({"resourceType": "Bundle", "type": "history", "entry": [{
        "fullUrl": "urn:uuid:1",
        "resource": {"resourceType": "SubscriptionStatus", "status": "active", "type": "heartbeat",
            "topic": "http://example.org/t"},
        "request": {"method": "GET", "url": "x"}, "response": {"status": "200"}}]});
    let validator = Validator::new(core_registry(FhirVersion::R4));
    let errors = validator.validate_sync(&bundle, &ValidationOptions::default()).errors;
    let under: Vec<_> = errors.iter().filter(|e| e.path.starts_with("Bundle.entry.0.resource")).collect();
    assert_eq!(under.len(), 1, "{errors:#?}");
    assert_eq!(
        under[0].hl7_message.as_deref(),
        Some("Contained resource does not appear to be a FHIR resource (unknown name 'SubscriptionStatus')")
    );
}
