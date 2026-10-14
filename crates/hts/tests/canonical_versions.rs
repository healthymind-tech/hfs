//! Canonical `url|version` handling must be identical across every operation
//! and the `?url=` search endpoints (fork issue T1).
//!
//! Each test imports two versions of a CodeSystem / ValueSet / ConceptMap and
//! checks that the piped spelling (`url=X|V`) selects the same row as the
//! explicit `valueSetVersion` / `version` parameter, and that disagreeing
//! versions are rejected with a 400 `OperationOutcome`.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::Value;

const CS_URL: &str = "http://hts.test/versioned/cs";
const VS_URL: &str = "http://hts.test/versioned/vs";
const CM_URL: &str = "http://hts.test/versioned/cm";

fn versioned_bundle() -> &'static str {
    r#"{
  "resourceType": "Bundle",
  "type": "collection",
  "entry": [
    {"resource": {
      "resourceType": "CodeSystem", "id": "vcs", "url": "http://hts.test/versioned/cs",
      "version": "1.0", "name": "VersionedCS", "status": "active", "content": "complete",
      "hierarchyMeaning": "is-a",
      "concept": [
        {"code": "root", "display": "Root v1", "concept": [
          {"code": "a", "display": "A v1"}
        ]}
      ]
    }},
    {"resource": {
      "resourceType": "CodeSystem", "id": "vcs", "url": "http://hts.test/versioned/cs",
      "version": "2.0", "name": "VersionedCS", "status": "active", "content": "complete",
      "hierarchyMeaning": "is-a",
      "concept": [
        {"code": "root", "display": "Root v2", "concept": [
          {"code": "a", "display": "A v2"},
          {"code": "b", "display": "B v2"}
        ]}
      ]
    }},
    {"resource": {
      "resourceType": "ValueSet", "id": "vvs", "url": "http://hts.test/versioned/vs",
      "version": "1.0", "name": "VersionedVS", "status": "active",
      "compose": {"include": [{"system": "http://hts.test/versioned/cs", "concept": [{"code": "a"}]}]}
    }},
    {"resource": {
      "resourceType": "ValueSet", "id": "vvs", "url": "http://hts.test/versioned/vs",
      "version": "2.0", "name": "VersionedVS", "status": "active",
      "compose": {"include": [{"system": "http://hts.test/versioned/cs", "concept": [{"code": "a"}, {"code": "b"}]}]}
    }},
    {"resource": {
      "resourceType": "ConceptMap", "id": "vcm", "url": "http://hts.test/versioned/cm",
      "version": "3.1", "name": "VersionedCM", "status": "active",
      "sourceUri": "http://hts.test/versioned/vs", "targetUri": "http://snomed.info/sct",
      "group": [{"source": "http://hts.test/versioned/cs", "target": "http://snomed.info/sct",
        "element": [{"code": "a", "target": [{"code": "40983000", "equivalence": "equivalent"}]}]}]
    }}
  ]
}"#
}

fn param<'a>(body: &'a Value, name: &str) -> Option<&'a Value> {
    body["parameter"]
        .as_array()?
        .iter()
        .find(|p| p["name"] == name)
}

fn result_bool(body: &Value) -> Option<bool> {
    param(body, "result").and_then(|p| p["valueBoolean"].as_bool())
}

fn codes(body: &Value) -> Vec<String> {
    body["expansion"]["contains"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c["code"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn is_operation_outcome_400(status: StatusCode, body: &Value) -> bool {
    status == StatusCode::BAD_REQUEST && body["resourceType"] == "OperationOutcome"
}

// ── $expand ───────────────────────────────────────────────────────────────────

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn expand_piped_version_matches_explicit_value_set_version() {
    let app = TestApp::new();
    app.import_bundle_ok(versioned_bundle()).await;

    for (ver, expected) in [("1.0", vec!["a"]), ("2.0", vec!["a", "b"])] {
        let (st, piped) = app
            .get_fhir(&format!("/ValueSet/$expand?url={VS_URL}%7C{ver}"))
            .await;
        assert_eq!(st, StatusCode::OK, "{piped}");
        let (st, explicit) = app
            .get_fhir(&format!(
                "/ValueSet/$expand?url={VS_URL}&valueSetVersion={ver}"
            ))
            .await;
        assert_eq!(st, StatusCode::OK, "{explicit}");

        let mut a = codes(&piped);
        let mut b = codes(&explicit);
        a.sort();
        b.sort();
        assert_eq!(a, expected, "piped {ver}: {piped}");
        assert_eq!(a, b, "piped and explicit {ver} must agree");
        assert_eq!(piped["version"], ver, "{piped}");
    }

    // Unpinned → newest version.
    let (st, latest) = app
        .get_fhir(&format!("/ValueSet/$expand?url={VS_URL}"))
        .await;
    assert_eq!(st, StatusCode::OK, "{latest}");
    assert_eq!(latest["version"], "2.0", "{latest}");
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn expand_piped_version_not_found_does_not_poison_other_versions() {
    let app = TestApp::new();
    app.import_bundle_ok(versioned_bundle()).await;

    let (st, _) = app
        .get_fhir(&format!("/ValueSet/$expand?url={VS_URL}%7C9.9"))
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let (st, body) = app
        .get_fhir(&format!("/ValueSet/$expand?url={VS_URL}"))
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = app
        .get_fhir(&format!("/ValueSet/$expand?url={VS_URL}%7C1.0"))
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn expand_conflicting_versions_is_400() {
    let app = TestApp::new();
    app.import_bundle_ok(versioned_bundle()).await;

    let (st, body) = app
        .get_fhir(&format!(
            "/ValueSet/$expand?url={VS_URL}%7C1.0&valueSetVersion=2.0"
        ))
        .await;
    assert!(is_operation_outcome_400(st, &body), "{st} {body}");

    // POST form, agreeing versions are fine.
    let req = TestApp::params(&[
        ("url", "valueUri", &format!("{VS_URL}|2.0")),
        ("valueSetVersion", "valueString", "2.0"),
    ]);
    let (st, body) = app.post_fhir("/ValueSet/$expand", req).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["version"], "2.0");
}

// ── ValueSet/$validate-code ───────────────────────────────────────────────────

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn vs_validate_code_piped_version_selects_that_version() {
    let app = TestApp::new();
    app.import_bundle_ok(versioned_bundle()).await;

    // `b` is only in version 2.0.
    let (st, v1) = app
        .get_fhir(&format!(
            "/ValueSet/$validate-code?url={VS_URL}%7C1.0&system={CS_URL}&code=b"
        ))
        .await;
    assert_eq!(st, StatusCode::OK, "{v1}");
    assert_eq!(result_bool(&v1), Some(false), "{v1}");

    let (st, v2) = app
        .get_fhir(&format!(
            "/ValueSet/$validate-code?url={VS_URL}%7C2.0&system={CS_URL}&code=b"
        ))
        .await;
    assert_eq!(st, StatusCode::OK, "{v2}");
    assert_eq!(result_bool(&v2), Some(true), "{v2}");

    // Explicit parameter must agree with the piped form.
    let (st, explicit) = app
        .get_fhir(&format!(
            "/ValueSet/$validate-code?url={VS_URL}&valueSetVersion=2.0&system={CS_URL}&code=b"
        ))
        .await;
    assert_eq!(st, StatusCode::OK, "{explicit}");
    assert_eq!(result_bool(&explicit), Some(true));

    // Conflict → 400.
    let (st, body) = app
        .get_fhir(&format!(
            "/ValueSet/$validate-code?url={VS_URL}%7C1.0&valueSetVersion=2.0&system={CS_URL}&code=b"
        ))
        .await;
    assert!(is_operation_outcome_400(st, &body), "{st} {body}");
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn vs_validate_code_piped_system_version_is_honoured() {
    let app = TestApp::new();
    app.import_bundle_ok(versioned_bundle()).await;

    // Code `b` exists only in CS 2.0; pin the system to 1.0 via the pipe.
    let (st, body) = app
        .get_fhir(&format!(
            "/ValueSet/$validate-code?url={VS_URL}&system={CS_URL}%7C1.0&code=b"
        ))
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(result_bool(&body), Some(false), "{body}");

    let (st, body) = app
        .get_fhir(&format!(
            "/ValueSet/$validate-code?url={VS_URL}&system={CS_URL}%7C2.0&code=b"
        ))
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(result_bool(&body), Some(true), "{body}");
}

// ── CodeSystem/$validate-code ─────────────────────────────────────────────────

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn cs_validate_code_piped_version_selects_that_version() {
    let app = TestApp::new();
    app.import_bundle_ok(versioned_bundle()).await;

    let (st, v1) = app
        .get_fhir(&format!(
            "/CodeSystem/$validate-code?url={CS_URL}%7C1.0&code=b"
        ))
        .await;
    assert_eq!(st, StatusCode::OK, "{v1}");
    assert_eq!(result_bool(&v1), Some(false), "{v1}");

    let (st, v2) = app
        .get_fhir(&format!(
            "/CodeSystem/$validate-code?url={CS_URL}%7C2.0&code=b"
        ))
        .await;
    assert_eq!(st, StatusCode::OK, "{v2}");
    assert_eq!(result_bool(&v2), Some(true), "{v2}");
    assert_eq!(
        param(&v2, "version").and_then(|p| p["valueString"].as_str()),
        Some("2.0"),
        "{v2}"
    );

    let (st, body) = app
        .get_fhir(&format!(
            "/CodeSystem/$validate-code?url={CS_URL}%7C2.0&version=1.0&code=b"
        ))
        .await;
    assert!(is_operation_outcome_400(st, &body), "{st} {body}");
}

// ── $lookup ───────────────────────────────────────────────────────────────────

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn lookup_piped_system_version_matches_explicit_version() {
    let app = TestApp::new();
    app.import_bundle_ok(versioned_bundle()).await;

    for (ver, display) in [("1.0", "A v1"), ("2.0", "A v2")] {
        let (st, piped) = app
            .get_fhir(&format!(
                "/CodeSystem/$lookup?system={CS_URL}%7C{ver}&code=a"
            ))
            .await;
        assert_eq!(st, StatusCode::OK, "{piped}");
        assert_eq!(
            param(&piped, "display").and_then(|p| p["valueString"].as_str()),
            Some(display),
            "{piped}"
        );
        assert_eq!(
            param(&piped, "version").and_then(|p| p["valueString"].as_str()),
            Some(ver),
            "{piped}"
        );

        let (st, explicit) = app
            .get_fhir(&format!(
                "/CodeSystem/$lookup?system={CS_URL}&version={ver}&code=a"
            ))
            .await;
        assert_eq!(st, StatusCode::OK, "{explicit}");
        assert_eq!(
            param(&explicit, "display"),
            param(&piped, "display"),
            "piped and explicit {ver} disagree"
        );
    }

    let (st, body) = app
        .get_fhir(&format!(
            "/CodeSystem/$lookup?system={CS_URL}%7C1.0&version=2.0&code=a"
        ))
        .await;
    assert!(is_operation_outcome_400(st, &body), "{st} {body}");
}

// ── $subsumes ─────────────────────────────────────────────────────────────────

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn subsumes_piped_system_version_is_honoured() {
    let app = TestApp::new();
    app.import_bundle_ok(versioned_bundle()).await;

    // `b` exists only in 2.0 → in 1.0 it is unknown (404), in 2.0 it is subsumed by root.
    let (st, body) = app
        .get_fhir(&format!(
            "/CodeSystem/$subsumes?system={CS_URL}%7C2.0&codeA=root&codeB=b"
        ))
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        param(&body, "outcome").and_then(|p| p["valueCode"].as_str()),
        Some("subsumes"),
        "{body}"
    );

    let (st, body) = app
        .get_fhir(&format!(
            "/CodeSystem/$subsumes?system={CS_URL}%7C1.0&codeA=root&codeB=b"
        ))
        .await;
    assert_ne!(st, StatusCode::OK, "code b must be unknown in 1.0: {body}");
}

// ── $translate ────────────────────────────────────────────────────────────────

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn translate_piped_concept_map_version() {
    let app = TestApp::new();
    app.import_bundle_ok(versioned_bundle()).await;

    let (st, ok) = app
        .get_fhir(&format!(
            "/ConceptMap/$translate?url={CM_URL}%7C3.1&system={CS_URL}&code=a"
        ))
        .await;
    assert_eq!(st, StatusCode::OK, "{ok}");
    assert_eq!(result_bool(&ok), Some(true), "{ok}");

    let (st, explicit) = app
        .get_fhir(&format!(
            "/ConceptMap/$translate?url={CM_URL}&conceptMapVersion=3.1&system={CS_URL}&code=a"
        ))
        .await;
    assert_eq!(st, StatusCode::OK, "{explicit}");
    assert_eq!(result_bool(&explicit), Some(true));

    let (st, missing) = app
        .get_fhir(&format!(
            "/ConceptMap/$translate?url={CM_URL}%7C9.9&system={CS_URL}&code=a"
        ))
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{missing}");

    let (st, body) = app
        .get_fhir(&format!(
            "/ConceptMap/$translate?url={CM_URL}%7C3.1&conceptMapVersion=1.0&system={CS_URL}&code=a"
        ))
        .await;
    assert!(is_operation_outcome_400(st, &body), "{st} {body}");
}

// ── Search ────────────────────────────────────────────────────────────────────

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn search_url_pipe_version_matches_explicit_version_param() {
    let app = TestApp::new();
    app.import_bundle_ok(versioned_bundle()).await;

    for resource in ["CodeSystem", "ValueSet"] {
        let url = if resource == "CodeSystem" {
            CS_URL
        } else {
            VS_URL
        };
        let (st, all) = app.get_fhir(&format!("/{resource}?url={url}")).await;
        assert_eq!(st, StatusCode::OK, "{all}");
        assert_eq!(all["total"], 2, "{all}");

        let (st, piped) = app.get_fhir(&format!("/{resource}?url={url}%7C1.0")).await;
        assert_eq!(st, StatusCode::OK, "{piped}");
        assert_eq!(piped["total"], 1, "{piped}");
        assert_eq!(piped["entry"][0]["resource"]["version"], "1.0", "{piped}");

        let (st, explicit) = app
            .get_fhir(&format!("/{resource}?url={url}&version=1.0"))
            .await;
        assert_eq!(st, StatusCode::OK, "{explicit}");
        assert_eq!(explicit["total"], 1);

        let (st, body) = app
            .get_fhir(&format!("/{resource}?url={url}%7C1.0&version=2.0"))
            .await;
        assert!(is_operation_outcome_400(st, &body), "{st} {body}");
    }

    let (st, cm) = app
        .get_fhir(&format!("/ConceptMap?url={CM_URL}%7C3.1"))
        .await;
    assert_eq!(st, StatusCode::OK, "{cm}");
    assert_eq!(cm["total"], 1, "{cm}");
    let (st, cm_none) = app
        .get_fhir(&format!("/ConceptMap?url={CM_URL}%7C0.1"))
        .await;
    assert_eq!(st, StatusCode::OK, "{cm_none}");
    assert_eq!(cm_none["total"], 0, "{cm_none}");
}
