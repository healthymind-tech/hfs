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
