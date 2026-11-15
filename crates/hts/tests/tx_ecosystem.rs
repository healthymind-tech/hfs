//! Regressions found by running the HL7 tx-ecosystem test suite
//! (HL7/fhir-tx-ecosystem-ig `tests/`, `validator_cli.jar txTests`) against
//! HTS. Each test names the suite/test it mirrors. Runs on SQLite by default
//! and on PostgreSQL with `HTS_TEST_BACKEND=postgres`.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};

const TX_ISSUE_TYPE: &str = "http://hl7.org/fhir/tools/CodeSystem/tx-issue-type";

/// A versionless, flat CodeSystem whose hierarchy is stated through
/// `subsumedBy` properties (the shape of THO act-class), plus filtered
/// ValueSets over it.
fn act_bundle() -> String {
    json!({
      "resourceType": "Bundle", "type": "collection",
      "entry": [
        {"resource": {
          "resourceType": "CodeSystem", "id": "act", "url": "http://hts.test/act",
          "status": "active", "content": "complete", "hierarchyMeaning": "is-a",
          "property": [
            {"code": "subsumedBy", "uri": "http://hl7.org/fhir/concept-properties#parent", "type": "code"},
            {"code": "status", "uri": "http://hl7.org/fhir/concept-properties#status", "type": "code"}
          ],
          "concept": [
            {"code": "ACT", "display": "act"},
            {"code": "ORG", "display": "organizer",
             "property": [{"code": "subsumedBy", "valueCode": "ACT"}]},
            {"code": "FOLDER", "display": "folder",
             "property": [{"code": "subsumedBy", "valueCode": "ORG"}]},
            {"code": "OBS", "display": "observation",
             "property": [{"code": "subsumedBy", "valueCode": "ACT"}]},
            {"code": "OLD", "display": "old",
             "property": [{"code": "subsumedBy", "valueCode": "ACT"},
                          {"code": "status", "valueCode": "retired"}]}
          ]
        }},
        {"resource": {
          "resourceType": "ValueSet", "id": "act-isa", "url": "http://hts.test/vs/act-isa",
          "version": "0.1.0", "status": "active",
          "compose": {"include": [{"system": "http://hts.test/act",
            "filter": [{"property": "concept", "op": "is-a", "value": "ORG"}]}]}
        }}
      ]
    })
    .to_string()
}

fn param<'a>(body: &'a Value, name: &str) -> Option<&'a Value> {
    body["parameter"]
        .as_array()?
        .iter()
        .find(|p| p["name"] == name)
}

fn issues(body: &Value) -> Vec<Value> {
    param(body, "issues")
        .and_then(|p| p["resource"]["issue"].as_array().cloned())
        .unwrap_or_default()
}

fn tx_codes(body: &Value) -> Vec<String> {
    let mut v: Vec<String> = issues(body)
        .iter()
        .filter_map(|i| {
            i["details"]["coding"]
                .as_array()?
                .iter()
                .find(|c| c["system"] == TX_ISSUE_TYPE)
                .and_then(|c| c["code"].as_str().map(str::to_owned))
        })
        .collect();
    v.sort();
    v
}

async fn vs_validate(app: &TestApp, params: Value) -> Value {
    let (st, body) = app
        .post_fhir("/ValueSet/$validate-code", params.to_string())
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    body
}

/// tho/validate-isa-parent, tho/validate-isa-unknown-code: a code outside a
/// filtered ValueSet over a versionless CodeSystem still echoes the
/// CodeSystem display, and a code missing from the CodeSystem gets the
/// invalid-code issue next to not-in-vs.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn not_in_vs_against_versionless_code_system() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&act_bundle()).await;

    let body = vs_validate(
        &app,
        json!({"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": "http://hts.test/vs/act-isa"},
            {"name": "system", "valueUri": "http://hts.test/act"},
            {"name": "code", "valueCode": "ACT"}
        ]}),
    )
    .await;
    assert_eq!(param(&body, "result").unwrap()["valueBoolean"], false);
    assert_eq!(param(&body, "display").unwrap()["valueString"], "act");
    assert_eq!(tx_codes(&body), ["not-in-vs"]);

    let body = vs_validate(
        &app,
        json!({"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": "http://hts.test/vs/act-isa"},
            {"name": "system", "valueUri": "http://hts.test/act"},
            {"name": "code", "valueCode": "NOPE"}
        ]}),
    )
    .await;
    assert_eq!(param(&body, "result").unwrap()["valueBoolean"], false);
    assert!(param(&body, "display").is_none(), "{body}");
    assert_eq!(tx_codes(&body), ["invalid-code", "not-in-vs"]);
}

/// tho/validate-isa-other-branch: an inactive code that the ValueSet leaves
/// out for hierarchy reasons (not because it excludes inactive codes) gets
/// not-in-vs plus the inactive warning, but not the "valid but not active"
/// error; the warning is about the `code` parameter.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn inactive_code_outside_filtered_vs_is_not_a_status_error() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&act_bundle()).await;

    let body = vs_validate(
        &app,
        json!({"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": "http://hts.test/vs/act-isa"},
            {"name": "system", "valueUri": "http://hts.test/act"},
            {"name": "code", "valueCode": "OLD"}
        ]}),
    )
    .await;
    assert_eq!(param(&body, "result").unwrap()["valueBoolean"], false);
    assert_eq!(param(&body, "inactive").unwrap()["valueBoolean"], true);
    assert_eq!(tx_codes(&body), ["code-comment", "not-in-vs"], "{body}");
    let warning = issues(&body)
        .into_iter()
        .find(|i| i["severity"] == "warning")
        .unwrap();
    assert_eq!(warning["expression"], json!(["code"]));
}

/// A versioned CodeSystem and a ValueSet including all of it.
fn simple_bundle() -> String {
    json!({
      "resourceType": "Bundle", "type": "collection",
      "entry": [
        {"resource": {
          "resourceType": "CodeSystem", "id": "simple", "url": "http://hts.test/simple",
          "version": "0.1.0", "status": "active", "content": "complete",
          "concept": [{"code": "code1", "display": "Display 1"}, {"code": "code2", "display": "Display 2"}]
        }},
        {"resource": {
          "resourceType": "ValueSet", "id": "simple-all", "url": "http://hts.test/vs/simple-all",
          "version": "5.0.0", "status": "active",
          "compose": {"include": [{"system": "http://hts.test/simple"}]}
        }}
      ]
    })
    .to_string()
}

fn vs_params(extra: Value) -> Value {
    let mut p = vec![json!({"name": "url", "valueUri": "http://hts.test/vs/simple-all"})];
    p.extend(extra.as_array().unwrap().iter().cloned());
    json!({"resourceType": "Parameters", "parameter": p})
}

/// validation/validation-missing-*: nothing to validate is a 400
/// invalid / invalid-data; an incomplete Coding is result=false with a
/// warning against that Coding.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn incomplete_validate_code_input() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&simple_bundle()).await;

    // Request errors.
    for (path, body) in [
        ("/ValueSet/$validate-code", vs_params(json!([]))),
        // A bare code with no system and no inferSystem.
        (
            "/ValueSet/$validate-code",
            vs_params(json!([{"name": "code", "valueCode": "code1"}])),
        ),
        (
            "/CodeSystem/$validate-code",
            json!({"resourceType": "Parameters", "parameter": [{"name": "code", "valueCode": "code1"}]}),
        ),
    ] {
        let (st, oo) = app.post_fhir(path, body.to_string()).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{oo}");
        assert_eq!(oo["issue"][0]["code"], "invalid");
        assert_eq!(
            oo["issue"][0]["details"]["coding"][0]["code"],
            "invalid-data"
        );
        assert!(oo["issue"][0].get("diagnostics").is_none(), "{oo}");
    }

    // inferSystem lets a bare code through.
    let body = vs_validate(
        &app,
        vs_params(json!([
            {"name": "code", "valueCode": "code1"},
            {"name": "inferSystem", "valueBoolean": true}
        ])),
    )
    .await;
    assert_eq!(param(&body, "result").unwrap()["valueBoolean"], true);

    // A Coding with a system but no code.
    let body = vs_validate(
        &app,
        vs_params(json!([{"name": "coding", "valueCoding": {"system": "http://hts.test/simple"}}])),
    )
    .await;
    assert_eq!(
        param(&body, "result").unwrap()["valueBoolean"],
        false,
        "{body}"
    );
    assert_eq!(tx_codes(&body), ["invalid-data"]);
    assert_eq!(issues(&body)[0]["severity"], "warning");
    assert_eq!(issues(&body)[0]["expression"], json!(["Coding"]));
    assert!(param(&body, "message").is_some());

    // A CodeableConcept with text only: fails as a whole.
    let body = vs_validate(
        &app,
        vs_params(json!([{"name": "codeableConcept", "valueCodeableConcept": {"text": "x"}}])),
    )
    .await;
    assert_eq!(param(&body, "result").unwrap()["valueBoolean"], false);
    assert_eq!(tx_codes(&body), ["not-in-vs"]);
    let msg = param(&body, "message").unwrap()["valueString"]
        .as_str()
        .unwrap();
    assert!(msg.contains("http://hts.test/vs/simple-all|5.0.0"), "{msg}");

    // A systemless coding next to a good one: the good one carries the
    // result, the systemless one is reported against its own index.
    let body = vs_validate(
        &app,
        vs_params(
            json!([{"name": "codeableConcept", "valueCodeableConcept": {"coding": [
                {"code": "code1"},
                {"system": "http://hts.test/simple", "code": "code1"}
            ]}}]),
        ),
    )
    .await;
    assert_eq!(
        param(&body, "result").unwrap()["valueBoolean"],
        true,
        "{body}"
    );
    assert_eq!(tx_codes(&body), ["invalid-data", "this-code-not-in-vs"]);
    let warning = issues(&body)
        .into_iter()
        .find(|i| i["severity"] == "warning")
        .unwrap();
    assert_eq!(warning["expression"], json!(["CodeableConcept.coding[0]"]));

    // CodeSystem/$validate-code: a CodeableConcept whose coding has no code.
    let (st, body) = app
        .post_fhir(
            "/CodeSystem/$validate-code",
            json!({"resourceType": "Parameters", "parameter": [
                {"name": "codeableConcept", "valueCodeableConcept": {"coding": [{"system": "http://hts.test/simple"}]}}
            ]})
            .to_string(),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(param(&body, "result").unwrap()["valueBoolean"], false);
    assert_eq!(tx_codes(&body), ["invalid-data"]);
}

/// deprecated/draft-validate: a draft CodeSystem used by an active ValueSet
/// is noted as such; a normative one is not noted at all.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn status_check_notes() {
    let app = TestApp::new().await;
    app.import_bundle_ok(
        &json!({"resourceType": "Bundle", "type": "collection", "entry": [
            {"resource": {"resourceType": "CodeSystem", "id": "draft", "url": "http://hts.test/draft",
              "version": "0.1.0", "status": "draft", "content": "complete",
              "concept": [{"code": "a", "display": "A"}]}},
            {"resource": {"resourceType": "CodeSystem", "id": "norm", "url": "http://hts.test/norm",
              "version": "1.0", "status": "active", "content": "complete",
              "extension": [{"url": "http://hl7.org/fhir/StructureDefinition/structuredefinition-standards-status",
                             "valueCode": "normative"}],
              "concept": [{"code": "b", "display": "B"}]}},
            {"resource": {"resourceType": "ValueSet", "id": "both", "url": "http://hts.test/vs/both",
              "version": "1.0", "status": "active",
              "compose": {"include": [{"system": "http://hts.test/draft"}, {"system": "http://hts.test/norm"}]}}}
        ]})
        .to_string(),
    )
    .await;

    let coding = |system: &str, code: &str| {
        json!({"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": "http://hts.test/vs/both"},
            {"name": "coding", "valueCoding": {"system": system, "code": code}}
        ]})
    };
    let body = vs_validate(&app, coding("http://hts.test/draft", "a")).await;
    let notes = issues(&body);
    assert_eq!(notes.len(), 1, "{body}");
    assert_eq!(
        notes[0]["details"]["text"],
        "Reference to draft CodeSystem http://hts.test/draft|0.1.0 from active ValueSet http://hts.test/vs/both|1.0"
    );

    let body = vs_validate(&app, coding("http://hts.test/norm", "b")).await;
    assert!(issues(&body).is_empty(), "{body}");
}

/// inactive/inactive-4-validate, inactive-5-validate,
/// validation-simple-codeableconcept-unknown-system: in a CodeableConcept,
/// the codings that do not carry the result still get their notes (an
/// inactive code, an unchecked unknown code system), at their own index,
/// and the result stays true.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn codeable_concept_notes_for_the_other_codings() {
    let app = TestApp::new().await;
    app.import_bundle_ok(
        &json!({"resourceType": "Bundle", "type": "collection", "entry": [
            {"resource": {"resourceType": "CodeSystem", "id": "inact", "url": "http://hts.test/inact",
              "version": "0.1.0", "status": "active", "content": "complete",
              "property": [{"code": "status", "uri": "http://hl7.org/fhir/concept-properties#status", "type": "code"}],
              "concept": [
                {"code": "on", "display": "Active"},
                {"code": "off", "display": "Inactive", "property": [{"code": "status", "valueCode": "inactive"}]}
              ]}},
            {"resource": {"resourceType": "ValueSet", "id": "inact-all", "url": "http://hts.test/vs/inact-all",
              "version": "1.0", "status": "active",
              "compose": {"include": [{"system": "http://hts.test/inact"}]}}}
        ]})
        .to_string(),
    )
    .await;
    let cc = |codings: Value| {
        json!({"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": "http://hts.test/vs/inact-all"},
            {"name": "codeableConcept", "valueCodeableConcept": {"coding": codings}}
        ]})
    };
    let warning_at = |body: &Value| -> Vec<Value> {
        issues(body)
            .into_iter()
            .filter(|i| i["severity"] == "warning")
            .map(|i| i["expression"][0].clone())
            .collect()
    };

    // Active first, inactive second: the warning names coding[1].
    let body = vs_validate(
        &app,
        cc(json!([
            {"system": "http://hts.test/inact", "code": "on"},
            {"system": "http://hts.test/inact", "code": "off"}
        ])),
    )
    .await;
    assert_eq!(
        param(&body, "result").unwrap()["valueBoolean"],
        true,
        "{body}"
    );
    assert_eq!(warning_at(&body), [json!("CodeableConcept.coding[1]")]);

    // An unknown code system next to a good coding.
    let body = vs_validate(
        &app,
        cc(json!([
            {"system": "http://hts.test/unknown", "code": "x"},
            {"system": "http://hts.test/inact", "code": "on"}
        ])),
    )
    .await;
    assert_eq!(
        param(&body, "result").unwrap()["valueBoolean"],
        true,
        "{body}"
    );
    assert_eq!(tx_codes(&body), ["not-found", "this-code-not-in-vs"]);
    assert_eq!(
        warning_at(&body),
        [json!("CodeableConcept.coding[0].system")]
    );
}

fn props_bundle() -> String {
    json!({"resourceType": "Bundle", "type": "collection", "entry": [
        {"resource": {"resourceType": "CodeSystem", "id": "props", "url": "http://hts.test/props",
          "version": "0.1.0", "status": "active", "content": "complete",
          "property": [
            {"code": "colour", "uri": "http://hts.test/props#colour", "type": "string"},
            {"code": "link", "type": "Coding"},
            {"code": "inactive", "uri": "http://hl7.org/fhir/concept-properties#inactive", "type": "boolean"}
          ],
          "concept": [
            {"code": "a", "display": "A", "property": [{"code": "colour", "valueString": "red"}]},
            {"code": "b", "display": "B", "property": [
              {"code": "link", "valueCoding": {"system": "http://elsewhere.test", "code": "x", "display": "X"}}]},
            {"code": "c", "display": "C", "property": [{"code": "inactive", "valueBoolean": true}]}
          ]}},
        {"resource": {"resourceType": "ValueSet", "id": "props-all", "url": "http://hts.test/vs/props-all",
          "version": "1.0", "status": "active",
          "compose": {"include": [{"system": "http://hts.test/props"}], "property": ["*"]}}}
    ]})
    .to_string()
}

/// `property` entries of an R5 `expansion.contains`, or the R5 cross-version
/// extensions an R4 build renders them as, flattened to `{code, value[x]}`.
fn r5_props(holder: &Value, ext_url: &str) -> Vec<Value> {
    if let Some(props) = holder["property"].as_array() {
        return props.clone();
    }
    holder["extension"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["url"] == ext_url)
        .map(|e| {
            let mut out = serde_json::Map::new();
            for part in e["extension"].as_array().into_iter().flatten() {
                let name = part["url"].as_str().unwrap_or_default();
                if let Some((k, v)) = part
                    .as_object()
                    .unwrap()
                    .iter()
                    .find(|(k, _)| k.starts_with("value"))
                {
                    let key = if name == "value" {
                        k.clone()
                    } else {
                        name.to_string()
                    };
                    out.insert(key, v.clone());
                }
            }
            Value::Object(out)
        })
        .collect()
}

fn contains_props(body: &Value, code: &str) -> Vec<Value> {
    let c = body["expansion"]["contains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["code"] == code)
        .cloned()
        .unwrap_or_default();
    r5_props(
        &c,
        "http://hl7.org/fhir/5.0/StructureDefinition/extension-ValueSet.expansion.contains.property",
    )
}

/// inactive/inactive-expand: a concept made inactive by its `inactive`
/// property carries the status `inactive` in an expansion.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn expansion_status_of_an_inactive_concept() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&props_bundle()).await;
    let (st, body) = app
        .get_fhir("/ValueSet/$expand?url=http://hts.test/vs/props-all&property=status")
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        contains_props(&body, "c").contains(&json!({"code": "status", "valueCode": "inactive"})),
        "{body}"
    );
}

/// properties/expand-with-valueset.compose.property-*: the ValueSet's
/// compose.property (here `*`) selects the properties of the expansion,
/// and a Coding property keeps its own system.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn expansion_properties_from_compose_property() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&props_bundle()).await;
    let (st, body) = app
        .get_fhir("/ValueSet/$expand?url=http://hts.test/vs/props-all")
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let declared: Vec<String> = r5_props(
        &body["expansion"],
        "http://hl7.org/fhir/5.0/StructureDefinition/extension-ValueSet.expansion.property",
    )
    .iter()
    .filter_map(|p| p["code"].as_str().map(str::to_owned))
    .collect();
    assert!(
        declared.iter().any(|c| c == "colour") && declared.iter().any(|c| c == "link"),
        "{body}"
    );
    assert!(contains_props(&body, "a").contains(&json!({"code": "colour", "valueString": "red"})));
    let link = contains_props(&body, "b")
        .into_iter()
        .find(|p| p["code"] == "link")
        .unwrap();
    assert_eq!(link["valueCoding"]["system"], "http://elsewhere.test");
    assert_eq!(link["valueCoding"]["code"], "x");
}

/// simple/simple-lookup-1, simple-lookup-zxx: `$lookup` reports `abstract`
/// (and the notSelectable property) for property=*, and for an abstract
/// concept whatever was asked; a property filter also limits designations;
/// a display language the code system has nothing in gets a warning.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn lookup_abstract_property_filter_and_language_warning() {
    let app = TestApp::new().await;
    app.import_bundle_ok(
        &json!({"resourceType": "Bundle", "type": "collection", "entry": [
            {"resource": {"resourceType": "CodeSystem", "id": "lk", "url": "http://hts.test/lk",
              "version": "1", "status": "active", "content": "complete", "language": "en",
              "property": [{"code": "notSelectable", "uri": "http://hl7.org/fhir/concept-properties#notSelectable", "type": "boolean"}],
              "concept": [
                {"code": "top", "display": "Top", "definition": "Abstract top",
                 "property": [{"code": "notSelectable", "valueBoolean": true}],
                 "concept": [{"code": "leaf", "display": "Leaf", "definition": "A leaf"}]}
              ]}}
        ]})
        .to_string(),
    )
    .await;
    let names = |body: &Value| -> Vec<String> {
        body["parameter"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let prop_codes = |body: &Value| -> Vec<String> {
        body["parameter"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["name"] == "property")
            .map(|p| p["part"][0]["valueCode"].as_str().unwrap().to_owned())
            .collect()
    };

    let (_, body) = app
        .get_fhir("/CodeSystem/$lookup?system=http://hts.test/lk&code=leaf&property=*")
        .await;
    assert_eq!(
        param(&body, "abstract").unwrap()["valueBoolean"],
        false,
        "{body}"
    );
    assert!(
        prop_codes(&body).contains(&"notSelectable".to_string()),
        "{body}"
    );

    let (_, body) = app
        .get_fhir(
            "/CodeSystem/$lookup?system=http://hts.test/lk&code=top&property=definition&displayLanguage=zxx",
        )
        .await;
    assert_eq!(
        param(&body, "abstract").unwrap()["valueBoolean"],
        true,
        "{body}"
    );
    assert_eq!(prop_codes(&body), ["notSelectable"]);
    assert!(!names(&body).contains(&"designation".to_string()), "{body}");
    let warning = param(&body, "warning").unwrap()["valueString"]
        .as_str()
        .unwrap();
    assert!(warning.contains("zxx"), "{warning}");

    // No property parameter: no abstract for a concept that is not abstract.
    let (_, body) = app
        .get_fhir("/CodeSystem/$lookup?system=http://hts.test/lk&code=leaf")
        .await;
    assert!(param(&body, "abstract").is_none(), "{body}");
    assert!(param(&body, "warning").is_none(), "{body}");
}

/// simple/simple-expand-contained-circular: a value set that includes
/// itself through contained value sets has no expansion — 4xx
/// VALUESET_CIRCULAR_REFERENCE, not an expansion with a warning.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn circular_include_is_an_error() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&simple_bundle()).await;
    let (st, oo) = app
        .post_fhir(
            "/ValueSet/$expand",
            json!({"resourceType": "Parameters", "parameter": [{"name": "valueSet", "resource": {
            "resourceType": "ValueSet", "url": "http://hts.test/vs/circular", "status": "active",
            "compose": {"include": [{"valueSet": ["#vs1"]}]},
            "contained": [
                {"resourceType": "ValueSet", "id": "vs1", "status": "active",
                 "compose": {"include": [{"valueSet": ["#vs2"]}]}},
                {"resourceType": "ValueSet", "id": "vs2", "status": "active",
                 "compose": {"include": [
                    {"system": "http://hts.test/simple", "concept": [{"code": "code1"}]},
                    {"valueSet": ["#vs1"]}]}}
            ]}}]})
            .to_string(),
        )
        .await;
    assert!(st.is_client_error(), "{st} {oo}");
    assert_eq!(oo["issue"][0]["code"], "processing");
    assert_eq!(oo["issue"][0]["details"]["coding"][0]["code"], "vs-invalid");
    assert!(
        oo["issue"][0]["details"]["text"]
            .as_str()
            .unwrap()
            .contains("vs1")
    );
}

/// extensions/validate-code-inactive-display-*: a display matching a
/// withdrawn designation is wrong — an error (warning when lenient) naming
/// the designation's own status and the correct display.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn inactive_display_is_an_error_unless_lenient() {
    let app = TestApp::new().await;
    app.import_bundle_ok(
        &json!({"resourceType": "Bundle", "type": "collection", "entry": [
            {"resource": {"resourceType": "CodeSystem", "id": "disp", "url": "http://hts.test/disp",
              "version": "1", "status": "active", "content": "complete",
              "concept": [{"code": "c2", "display": "Display 2", "designation": [
                {"language": "de", "value": "2nd Code", "extension": [{
                  "url": "http://hl7.org/fhir/StructureDefinition/structuredefinition-standards-status",
                  "valueCode": "withdrawn"}]}]}]}}
        ]})
        .to_string(),
    )
    .await;
    for (lenient, result, severity) in [(false, false, "error"), (true, true, "warning")] {
        let mut p = vec![
            json!({"name": "url", "valueUri": "http://hts.test/disp"}),
            json!({"name": "code", "valueCode": "c2"}),
            json!({"name": "display", "valueString": "2nd Code"}),
        ];
        if lenient {
            p.push(json!({"name": "lenient-display-validation", "valueBoolean": true}));
        }
        let (st, body) = app
            .post_fhir(
                "/CodeSystem/$validate-code",
                json!({"resourceType": "Parameters", "parameter": p}).to_string(),
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(
            param(&body, "result").unwrap()["valueBoolean"],
            result,
            "{body}"
        );
        let issue = &issues(&body)[0];
        assert_eq!(issue["severity"], severity);
        assert_eq!(
            issue["details"]["text"],
            "'2nd Code' is no longer considered a correct display for code 'c2' (status = withdrawn). The correct display is 'Display 2'"
        );
        assert!(param(&body, "message").is_some());
    }
}

/// exclude/exclude-valueset: a value set used by the expansion that came
/// with the request is reported as used with its own version.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn used_valueset_from_the_request_keeps_its_version() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&simple_bundle()).await;
    let (st, body) = app
        .post_fhir(
            "/ValueSet/$expand",
            json!({"resourceType": "Parameters", "parameter": [
                {"name": "url", "valueUri": "http://hts.test/vs/minus"},
                {"name": "tx-resource", "resource": {"resourceType": "ValueSet",
                  "url": "http://hts.test/vs/minus", "status": "active",
                  "compose": {"include": [{"system": "http://hts.test/simple"}],
                              "exclude": [{"valueSet": ["http://hts.test/vs/two"]}]}}},
                {"name": "tx-resource", "resource": {"resourceType": "ValueSet",
                  "url": "http://hts.test/vs/two", "version": "0.3", "status": "active",
                  "compose": {"include": [{"system": "http://hts.test/simple", "concept": [{"code": "code2"}]}]}}}
            ]})
            .to_string(),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let used: Vec<&str> = body["expansion"]["parameter"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["name"] == "used-valueset")
        .filter_map(|p| p["valueUri"].as_str())
        .collect();
    assert_eq!(used, ["http://hts.test/vs/two|0.3"], "{body}");

    // default-valueset-version/indirect-expand-zero: of several supplied
    // versions, the latest.
    let (st, body) = app
        .post_fhir(
            "/ValueSet/$expand",
            json!({"resourceType": "Parameters", "parameter": [
                {"name": "url", "valueUri": "http://hts.test/vs/minus"},
                {"name": "tx-resource", "resource": {"resourceType": "ValueSet",
                  "url": "http://hts.test/vs/minus", "status": "active",
                  "compose": {"include": [{"system": "http://hts.test/simple"}],
                              "exclude": [{"valueSet": ["http://hts.test/vs/two"]}]}}},
                {"name": "tx-resource", "resource": {"resourceType": "ValueSet",
                  "url": "http://hts.test/vs/two", "version": "0.3", "status": "active",
                  "compose": {"include": [{"system": "http://hts.test/simple", "concept": [{"code": "code2"}]}]}}},
                {"name": "tx-resource", "resource": {"resourceType": "ValueSet",
                  "url": "http://hts.test/vs/two", "version": "0.10", "status": "active",
                  "compose": {"include": [{"system": "http://hts.test/simple", "concept": [{"code": "code2"}]}]}}}
            ]})
            .to_string(),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let used: Vec<&str> = body["expansion"]["parameter"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["name"] == "used-valueset")
        .filter_map(|p| p["valueUri"].as_str())
        .collect();
    assert_eq!(used, ["http://hts.test/vs/two|0.10"], "{body}");
}

fn translate_bundle() -> String {
    json!({"resourceType": "Bundle", "type": "collection", "entry": [
        {"resource": {"resourceType": "CodeSystem", "id": "tsrc", "url": "http://hts.test/tsrc",
          "version": "1", "status": "active", "content": "complete",
          "concept": [{"code": "s1"}, {"code": "s2"}, {"code": "s3"}, {"code": "s4"}, {"code": "s5"}, {"code": "s6"}]}},
        {"resource": {"resourceType": "CodeSystem", "id": "ttgt", "url": "http://hts.test/ttgt",
          "version": "1", "status": "active", "content": "complete",
          "concept": [{"code": "t1"}, {"code": "t2"}, {"code": "t9", "display": "Nine"}]}},
        {"resource": {"resourceType": "ValueSet", "id": "tscope", "url": "http://hts.test/vs/tscope",
          "version": "1", "status": "active",
          "compose": {"include": [{"system": "http://hts.test/tsrc",
            "concept": [{"code": "s1"}, {"code": "s2"}, {"code": "s3"}, {"code": "s4"}, {"code": "s5"}]}]}}},
        {"resource": {"resourceType": "ConceptMap", "id": "tmap", "url": "http://hts.test/cm/main",
          "version": "0.1", "status": "active", "sourceScopeUri": "http://hts.test/vs/tscope",
          "group": [{"source": "http://hts.test/tsrc", "target": "http://hts.test/ttgt",
            "element": [
              {"code": "s1", "target": [{"code": "t1", "relationship": "equivalent", "comment": "c1"}]},
              {"code": "s2", "target": [{"code": "t2", "relationship": "not-related-to"}]},
              {"code": "s3", "noMap": true}
            ],
            "unmapped": {"mode": "fixed", "code": "t9", "display": "Nine", "relationship": "related-to"}}]}}
    ]})
    .to_string()
}

async fn translate(app: &TestApp, params: Value) -> Value {
    let (st, body) = app
        .post_fhir(
            "/ConceptMap/$translate",
            json!({"resourceType": "Parameters", "parameter": params}).to_string(),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    body
}

fn matches(body: &Value) -> Vec<Vec<Value>> {
    body["parameter"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["name"] == "match")
        .map(|p| p["part"].as_array().unwrap().clone())
        .collect()
}

fn part<'a>(parts: &'a [Value], name: &str) -> Option<&'a Value> {
    parts.iter().find(|p| p["name"] == name)
}

/// translate/*, translate2/*: $translate follows the ConceptMap definition —
/// comments, noMap, unmapped, sourceScope, not-related-to, reverse, and
/// maps supplied with the request.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn translate_follows_the_concept_map_definition() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&translate_bundle()).await;
    let fwd = |code: &str| {
        json!([
            {"name": "system", "valueUri": "http://hts.test/tsrc"},
            {"name": "code", "valueCode": code},
            {"name": "targetsystem", "valueUri": "http://hts.test/ttgt"}
        ])
    };

    // A mapping, with its target comment and the source concept.
    let body = translate(&app, fwd("s1")).await;
    assert_eq!(param(&body, "result").unwrap()["valueBoolean"], true);
    let m = &matches(&body)[0];
    assert_eq!(part(m, "concept").unwrap()["valueCoding"]["code"], "t1");
    assert_eq!(
        part(m, "originMap").unwrap()["valueCanonical"],
        "http://hts.test/cm/main|0.1"
    );
    assert_eq!(
        part(m, "sourceConcept").unwrap()["valueCoding"]["code"],
        "s1"
    );
    assert_eq!(part(m, "targetComment").unwrap()["valueString"], "c1");

    // not-related-to is an answer, but not a translation.
    let body = translate(&app, fwd("s2")).await;
    assert_eq!(param(&body, "result").unwrap()["valueBoolean"], false);
    assert_eq!(matches(&body).len(), 1);

    // noMap.
    let body = translate(&app, fwd("s3")).await;
    assert_eq!(param(&body, "result").unwrap()["valueBoolean"], false);
    assert_eq!(
        part(&matches(&body)[0], "noMap").unwrap()["valueBoolean"],
        true
    );

    // No element: the group's unmapped rule.
    let body = translate(&app, fwd("s4")).await;
    assert_eq!(param(&body, "result").unwrap()["valueBoolean"], true);
    let m = &matches(&body)[0];
    assert_eq!(part(m, "concept").unwrap()["valueCoding"]["code"], "t9");
    assert_eq!(
        part(m, "concept").unwrap()["valueCoding"]["display"],
        "Nine"
    );

    // Outside the map's sourceScope: the map does not apply at all.
    let body = translate(&app, fwd("s6")).await;
    assert_eq!(param(&body, "result").unwrap()["valueBoolean"], false);
    assert!(matches(&body).is_empty());
    let msg = param(&body, "message").unwrap()["valueString"]
        .as_str()
        .unwrap();
    assert!(msg.contains("s6"), "{msg}");

    // Reverse (R4 `reverse=true`): the target concept, back to the source.
    let body = translate(
        &app,
        json!([
            {"name": "system", "valueUri": "http://hts.test/ttgt"},
            {"name": "code", "valueCode": "t1"},
            {"name": "reverse", "valueBoolean": true},
            {"name": "targetsystem", "valueUri": "http://hts.test/tsrc"}
        ]),
    )
    .await;
    assert_eq!(
        param(&body, "result").unwrap()["valueBoolean"],
        true,
        "{body}"
    );
    assert_eq!(
        part(&matches(&body)[0], "sourceConcept").unwrap()["valueCoding"]["code"],
        "s1"
    );

    // A map supplied with the request, following other-map.
    let body = translate(
        &app,
        json!([
            {"name": "url", "valueUri": "http://hts.test/cm/req"},
            {"name": "system", "valueUri": "http://hts.test/tsrc"},
            {"name": "code", "valueCode": "s5"},
            {"name": "tx-resource", "resource": {"resourceType": "ConceptMap", "url": "http://hts.test/cm/req",
              "version": "2", "status": "active",
              "group": [{"source": "http://hts.test/tsrc", "target": "http://hts.test/ttgt",
                "element": [{"code": "s1", "target": [{"code": "t1", "relationship": "equivalent"}]}],
                "unmapped": {"mode": "other-map", "otherMap": "http://hts.test/cm/main"}}]}}
        ]),
    )
    .await;
    assert_eq!(
        param(&body, "result").unwrap()["valueBoolean"],
        true,
        "{body}"
    );
    let m = &matches(&body)[0];
    assert_eq!(part(m, "concept").unwrap()["valueCoding"]["code"], "t9");
    assert_eq!(
        part(m, "originMap").unwrap()["valueCanonical"],
        "http://hts.test/cm/req|2"
    );
    assert_eq!(
        param(&body, "used-conceptmap").unwrap()["valueUri"],
        "http://hts.test/cm/main|0.1"
    );
}

/// The tx-ecosystem runner sends every suite's setup resources as
/// `tx-resource` parameters instead of loading them into the server: an
/// operation must be answerable from request-supplied CodeSystems and
/// ValueSets (with stored ones they depend on), without storing them.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn operations_on_request_supplied_resources() {
    let app = TestApp::new().await;
    // Stored: a code system the supplied value set also draws on.
    app.import_bundle_ok(&simple_bundle()).await;
    let cs = json!({"resourceType": "CodeSystem", "url": "http://hts.test/req-cs", "version": "1",
        "status": "active", "content": "complete",
        "concept": [{"code": "r1", "display": "Req one"}, {"code": "r2", "display": "Req two"}]});
    let vs = json!({"resourceType": "ValueSet", "url": "http://hts.test/vs/req", "version": "1",
        "status": "active", "compose": {"include": [
            {"system": "http://hts.test/req-cs"},
            {"system": "http://hts.test/simple", "concept": [{"code": "code1"}]}]}});
    let with_tx = |mut p: Vec<Value>| {
        p.push(json!({"name": "tx-resource", "resource": cs.clone()}));
        p.push(json!({"name": "tx-resource", "resource": vs.clone()}));
        json!({"resourceType": "Parameters", "parameter": p}).to_string()
    };

    let (st, body) = app
        .post_fhir(
            "/ValueSet/$expand",
            with_tx(vec![
                json!({"name": "url", "valueUri": "http://hts.test/vs/req"}),
            ]),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let mut codes: Vec<&str> = body["expansion"]["contains"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["code"].as_str())
        .collect();
    codes.sort();
    assert_eq!(codes, ["code1", "r1", "r2"]);

    let (st, body) = app
        .post_fhir(
            "/ValueSet/$validate-code",
            with_tx(vec![
                json!({"name": "url", "valueUri": "http://hts.test/vs/req"}),
                json!({"name": "coding", "valueCoding": {"system": "http://hts.test/req-cs", "code": "r2"}}),
            ]),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        param(&body, "result").unwrap()["valueBoolean"],
        true,
        "{body}"
    );
    assert_eq!(param(&body, "display").unwrap()["valueString"], "Req two");

    let (st, body) = app
        .post_fhir(
            "/CodeSystem/$lookup",
            with_tx(vec![
                json!({"name": "system", "valueUri": "http://hts.test/req-cs"}),
                json!({"name": "code", "valueCode": "r1"}),
            ]),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(param(&body, "display").unwrap()["valueString"], "Req one");

    // Nothing the request supplied was stored.
    let (_, bundle) = app.get_fhir("/CodeSystem?url=http://hts.test/req-cs").await;
    assert_eq!(bundle["total"], 0, "{bundle}");
    let (st, _) = app
        .post_fhir(
            "/CodeSystem/$lookup",
            json!({"resourceType": "Parameters", "parameter": [
                {"name": "system", "valueUri": "http://hts.test/req-cs"},
                {"name": "code", "valueCode": "r1"}]})
            .to_string(),
        )
        .await;
    assert!(st.is_client_error(), "{st}");
}

/// parameters/parameters-*-supplement-none: a supplement that came with the
/// request is applied when asked for (`useSupplement`), never automatically;
/// the operator's stored supplements still are (HTS_AUTO_SUPPLEMENTS).
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn request_supplements_are_not_applied_automatically() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&simple_bundle()).await;
    let supplement = json!({"resourceType": "CodeSystem", "url": "http://hts.test/supp", "version": "1",
        "status": "active", "content": "supplement", "supplements": "http://hts.test/simple",
        "concept": [{"code": "code1", "designation": [{"language": "nl", "value": "een"}]}]});
    let lookup = |extra: Vec<Value>| {
        let mut p = vec![
            json!({"name": "system", "valueUri": "http://hts.test/simple"}),
            json!({"name": "code", "valueCode": "code1"}),
            json!({"name": "tx-resource", "resource": supplement.clone()}),
        ];
        p.extend(extra);
        json!({"resourceType": "Parameters", "parameter": p}).to_string()
    };
    let used = |body: &Value| param(body, "used-supplement").is_some();

    let (st, body) = app.post_fhir("/CodeSystem/$lookup", lookup(vec![])).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(!used(&body), "{body}");

    let (st, body) = app
        .post_fhir(
            "/CodeSystem/$lookup",
            lookup(vec![
                json!({"name": "useSupplement", "valueCanonical": "http://hts.test/supp"}),
            ]),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(used(&body), "{body}");
}

/// mimetypes/*: media types (urn:ietf:bcp:13) are a grammar — value sets
/// over them, $validate-code, $expand and $subsumes work without stored
/// content.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn media_type_code_system() {
    let app = TestApp::new().await;
    let base = json!({"resourceType": "ValueSet", "url": "http://hts.test/vs/mt-text", "version": "1",
        "status": "active", "compose": {"include": [{"system": "urn:ietf:bcp:13",
            "filter": [{"property": "base", "op": "=", "value": "text"}]}]}});
    let narrow = json!({"resourceType": "ValueSet", "url": "http://hts.test/vs/mt-reg", "version": "1",
        "status": "active", "compose": {"include": [{"system": "urn:ietf:bcp:13", "filter": [
            {"property": "registered", "op": "=", "value": "true"},
            {"property": "base", "op": "=", "value": "text/plain"}]}]}});
    let validate = |code: &str| {
        json!({"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": "http://hts.test/vs/mt-text"},
            {"name": "system", "valueUri": "urn:ietf:bcp:13"},
            {"name": "code", "valueCode": code},
            {"name": "tx-resource", "resource": base.clone()}]})
    };
    let body = vs_validate(&app, validate("text/html; charset=utf-8")).await;
    assert_eq!(
        param(&body, "result").unwrap()["valueBoolean"],
        true,
        "{body}"
    );
    let body = vs_validate(&app, validate("application/json")).await;
    assert_eq!(
        param(&body, "result").unwrap()["valueBoolean"],
        false,
        "{body}"
    );
    assert_eq!(tx_codes(&body), ["not-in-vs"]);

    // A base filter cannot be expanded; registered=true can, unclosed.
    let expand = |vs: &Value| {
        json!({"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": vs["url"]},
            {"name": "tx-resource", "resource": vs}]})
        .to_string()
    };
    let (st, _) = app.post_fhir("/ValueSet/$expand", expand(&base)).await;
    assert!(st.is_client_error(), "{st}");
    let (st, body) = app.post_fhir("/ValueSet/$expand", expand(&narrow)).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["expansion"]["contains"][0]["code"], "text/plain");
    assert_eq!(body["expansion"]["total"], 1);

    let subsumes = |a: &str, b: &str| {
        json!({"resourceType": "Parameters", "parameter": [
            {"name": "system", "valueUri": "urn:ietf:bcp:13"},
            {"name": "codeA", "valueCode": a}, {"name": "codeB", "valueCode": b}]})
        .to_string()
    };
    let (_, body) = app
        .post_fhir(
            "/CodeSystem/$subsumes",
            subsumes("application/xml", "application/xml; charset=utf-8"),
        )
        .await;
    assert_eq!(
        param(&body, "outcome").unwrap()["valueCode"],
        "subsumes",
        "{body}"
    );
    let (st, oo) = app
        .post_fhir(
            "/CodeSystem/$subsumes",
            subsumes("text/plain", "text/plain; foo=bar"),
        )
        .await;
    assert!(st.is_client_error(), "{st}");
    assert_eq!(
        oo["issue"][0]["details"]["coding"][0]["code"],
        "cannot-determine"
    );
}

/// UCUM/*: UCUM is a grammar — $subsumes compares canonical form and
/// magnitude, value sets filter on the canonical unit, $lookup describes
/// any expression.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn ucum_grammar_operations() {
    let app = TestApp::new().await;
    let subsumes = |a: &str, b: &str| {
        json!({"resourceType": "Parameters", "parameter": [
            {"name": "system", "valueUri": "http://unitsofmeasure.org"},
            {"name": "codeA", "valueCode": a}, {"name": "codeB", "valueCode": b}]})
        .to_string()
    };
    for (a, b, outcome) in [
        ("1/min", "min-1", "equivalent"),
        ("N", "kg.m/s2", "equivalent"),
        ("mg/dL", "g/L", "not-subsumed"),
        ("Cel", "K", "not-subsumed"),
    ] {
        let (st, body) = app.post_fhir("/CodeSystem/$subsumes", subsumes(a, b)).await;
        assert_eq!(st, StatusCode::OK, "{a} {b}: {body}");
        assert_eq!(
            param(&body, "outcome").unwrap()["valueCode"],
            outcome,
            "{a} {b}"
        );
    }
    let (st, _) = app
        .post_fhir("/CodeSystem/$subsumes", subsumes("not-a-unit", "mg"))
        .await;
    assert!(st.is_client_error());

    let canonical = json!({"resourceType": "ValueSet", "url": "http://hts.test/vs/mass", "status": "active",
        "compose": {"include": [{"system": "http://unitsofmeasure.org",
            "filter": [{"property": "canonical", "op": "=", "value": "g"}]}]}});
    let validate = |code: &str| {
        json!({"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": "http://hts.test/vs/mass"},
            {"name": "system", "valueUri": "http://unitsofmeasure.org"},
            {"name": "code", "valueCode": code},
            {"name": "tx-resource", "resource": canonical.clone()}]})
    };
    let body = vs_validate(&app, validate("kg")).await;
    assert_eq!(
        param(&body, "result").unwrap()["valueBoolean"],
        true,
        "{body}"
    );
    let body = vs_validate(&app, validate("mL")).await;
    assert_eq!(
        param(&body, "result").unwrap()["valueBoolean"],
        false,
        "{body}"
    );
    let msg = param(&body, "message").unwrap()["valueString"]
        .as_str()
        .unwrap();
    assert!(msg.contains("m3"), "{msg}");

    let (st, body) = app
        .get_fhir("/CodeSystem/$lookup?system=http://unitsofmeasure.org&code=mg/dL")
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(param(&body, "display").unwrap()["valueString"], "mg/dL");
    assert!(
        body.to_string().contains("(milligram) / (deciliter)"),
        "{body}"
    );
}

/// A code system whose content is not present cannot rule a code out: as
/// tx.fhir.org, an unknown code is a warning (it may exist), not an error.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn unknown_code_in_a_not_present_code_system_is_a_warning() {
    let app = TestApp::new().await;
    app.import_bundle_ok(
        &json!({"resourceType": "Bundle", "type": "collection", "entry": [
            {"resource": {"resourceType": "CodeSystem", "url": "http://hts.test/np", "version": "1",
              "status": "active", "content": "not-present"}}]})
        .to_string(),
    )
    .await;
    let (st, body) = app
        .get_fhir("/CodeSystem/$validate-code?url=http://hts.test/np&code=anything")
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        param(&body, "result").unwrap()["valueBoolean"],
        true,
        "{body}"
    );
    assert_eq!(issues(&body)[0]["severity"], "warning");
}

/// A bare `json` is not a media type: as tx.fhir.org, an unknown code of
/// urn:ietf:bcp:13 (not an unknown code system), against the core mimetypes
/// value set and the code system alike.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn a_bare_subtype_is_not_a_media_type() {
    let app = TestApp::new().await;
    let (_, body) = app
        .get_fhir("/ValueSet/$validate-code?url=http://hl7.org/fhir/ValueSet/mimetypes&system=urn:ietf:bcp:13&code=json")
        .await;
    assert_eq!(
        param(&body, "result").unwrap()["valueBoolean"],
        false,
        "{body}"
    );
    assert_eq!(tx_codes(&body), ["invalid-code", "not-in-vs"]);
    assert!(
        param(&body, "x-caused-by-unknown-system").is_none(),
        "{body}"
    );
    let (_, body) = app
        .get_fhir("/CodeSystem/$validate-code?url=urn:ietf:bcp:13&code=json")
        .await;
    assert_eq!(
        param(&body, "result").unwrap()["valueBoolean"],
        false,
        "{body}"
    );
    assert_eq!(tx_codes(&body), ["invalid-code"]);
    let (_, body) = app
        .get_fhir("/CodeSystem/$validate-code?url=urn:ietf:bcp:13&code=application/json")
        .await;
    assert_eq!(
        param(&body, "result").unwrap()["valueBoolean"],
        true,
        "{body}"
    );
}
