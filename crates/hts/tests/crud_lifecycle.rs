//! REST lifecycle of terminology resources (fork issue T5):
//!
//! * a CodeSystem created or replaced through REST must have its concept
//!   closure built so `$subsumes` and is-a filters work immediately,
//! * resources that reached the store only through `POST /import` must be
//!   readable, replaceable and deletable by id,
//! * `DELETE` answers 204 / 404 (never 500), `PUT` to a new id creates (201).

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};

const CS_URL: &str = "http://hts.test/crud/cs";
const VS_URL: &str = "http://hts.test/crud/vs";
const CM_URL: &str = "http://hts.test/crud/cm";

fn code_system(id: &str, version: &str) -> Value {
    json!({
      "resourceType": "CodeSystem", "id": id, "url": CS_URL, "version": version,
      "name": "CrudCS", "status": "active", "content": "complete", "hierarchyMeaning": "is-a",
      "concept": [
        {"code": "root", "display": format!("Root {version}"), "concept": [
          {"code": "mid", "display": "Mid", "concept": [{"code": "leaf", "display": "Leaf"}]}
        ]}
      ]
    })
}

fn import_bundle() -> String {
    json!({
      "resourceType": "Bundle", "type": "collection",
      "entry": [
        {"resource": code_system("crud-cs", "1.0")},
        {"resource": {"resourceType": "ValueSet", "id": "crud-vs", "url": VS_URL, "version": "1.0",
          "status": "active", "compose": {"include": [{"system": CS_URL, "concept": [{"code": "leaf"}]}]}}},
        {"resource": {"resourceType": "ValueSet", "id": "crud-vs-multi", "url": format!("{VS_URL}-multi"), "version": "1.0",
          "status": "active", "compose": {"include": [{"system": CS_URL, "concept": [{"code": "leaf"}]}]}}},
        {"resource": {"resourceType": "ValueSet", "id": "crud-vs-multi", "url": format!("{VS_URL}-multi"), "version": "2.0",
          "status": "active", "compose": {"include": [{"system": CS_URL, "concept": [{"code": "mid"}]}]}}},
        {"resource": {"resourceType": "ConceptMap", "id": "crud-cm", "url": CM_URL, "status": "active",
          "sourceUri": VS_URL, "targetUri": "http://snomed.info/sct",
          "group": [{"source": CS_URL, "target": "http://snomed.info/sct",
            "element": [{"code": "leaf", "target": [{"code": "1", "equivalence": "equivalent"}]}]}]}}
      ]
    })
    .to_string()
}

fn outcome(body: &Value) -> Option<&str> {
    body["parameter"]
        .as_array()?
        .iter()
        .find(|p| p["name"] == "outcome")
        .and_then(|p| p["valueCode"].as_str())
}

async fn subsumes(app: &TestApp, a: &str, b: &str) -> (StatusCode, Value) {
    app.get_fhir(&format!(
        "/CodeSystem/$subsumes?system={CS_URL}&codeA={a}&codeB={b}"
    ))
    .await
}

// ── T5a: closure after REST writes ────────────────────────────────────────────

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn posted_code_system_is_immediately_subsumable_and_filterable() {
    let app = TestApp::with_resource_store();

    let (st, body) = app
        .post_fhir("/CodeSystem", code_system("crud-cs", "1.0").to_string())
        .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");

    let (st, body) = subsumes(&app, "root", "leaf").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(outcome(&body), Some("subsumes"), "{body}");

    let inline = json!({"resourceType": "Parameters", "parameter": [{"name": "valueSet", "resource": {
        "resourceType": "ValueSet", "status": "active",
        "compose": {"include": [{"system": CS_URL, "filter": [{"property": "concept", "op": "is-a", "value": "mid"}]}]}
    }}]});
    let (st, body) = app
        .post_fhir("/ValueSet/$expand?excludeNested=true", inline.to_string())
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    // The expansion may come back nested; collect codes at every level.
    let mut codes: Vec<&str> = Vec::new();
    for c in body["expansion"]["contains"].as_array().unwrap() {
        codes.push(c["code"].as_str().unwrap());
        for n in c["contains"].as_array().into_iter().flatten() {
            codes.push(n["code"].as_str().unwrap());
        }
    }
    codes.sort();
    assert_eq!(codes, ["leaf", "mid"], "{body}");
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn updated_code_system_rebuilds_its_closure() {
    let app = TestApp::with_resource_store();
    let (st, _) = app
        .post_fhir("/CodeSystem", code_system("crud-cs", "1.0").to_string())
        .await;
    assert_eq!(st, StatusCode::CREATED);

    // Re-parent `leaf` directly under `root` and drop `mid`.
    let replaced = json!({
      "resourceType": "CodeSystem", "id": "crud-cs", "url": CS_URL, "version": "1.0",
      "name": "CrudCS", "status": "active", "content": "complete", "hierarchyMeaning": "is-a",
      "concept": [{"code": "root", "display": "Root", "concept": [{"code": "leaf", "display": "Leaf"}]}]
    });
    let (st, body) = app
        .put_fhir("/CodeSystem/crud-cs", replaced.to_string())
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");

    let (st, body) = subsumes(&app, "root", "leaf").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(outcome(&body), Some("subsumes"), "{body}");
    let (st, body) = subsumes(&app, "mid", "leaf").await;
    assert_ne!(st, StatusCode::OK, "mid was removed: {body}");
}

// ── T5b: imported resources are addressable by id ─────────────────────────────

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn imported_resources_are_readable_by_id() {
    let app = TestApp::with_resource_store();
    app.import_bundle_ok(&import_bundle()).await;

    let (st, body) = app.get_fhir("/CodeSystem/crud-cs").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["url"], CS_URL);

    let (st, body) = app.get_fhir("/ValueSet/crud-vs").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["url"], VS_URL);

    // Two versions share the FHIR id: the newest wins.
    let (st, body) = app.get_fhir("/ValueSet/crud-vs-multi").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["version"], "2.0");

    let (st, body) = app.get_fhir("/ConceptMap/crud-cm").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["url"], CM_URL);

    let (st, _) = app.get_fhir("/ValueSet/does-not-exist").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

// ── T5c: DELETE status codes ──────────────────────────────────────────────────

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn delete_returns_204_then_404_for_imported_and_rest_resources() {
    let app = TestApp::with_resource_store();
    app.import_bundle_ok(&import_bundle()).await;

    // Imported-only ValueSet.
    assert_eq!(
        app.delete_fhir("/ValueSet/crud-vs").await,
        StatusCode::NO_CONTENT
    );
    let (st, _) = app.get_fhir("/ValueSet/crud-vs").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, body) = app.get_fhir(&format!("/ValueSet?url={VS_URL}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["total"], 0, "{body}");
    assert_eq!(
        app.delete_fhir("/ValueSet/crud-vs").await,
        StatusCode::NOT_FOUND
    );

    // Never existed.
    assert_eq!(
        app.delete_fhir("/CodeSystem/nope").await,
        StatusCode::NOT_FOUND
    );

    // REST-created resource: 204 then 404.
    let (st, _) = app
        .post_fhir(
            "/ValueSet",
            json!({"resourceType": "ValueSet", "id": "rest-vs", "url": "http://hts.test/crud/rest-vs",
                   "status": "active", "compose": {"include": [{"system": CS_URL, "concept": [{"code": "leaf"}]}]}})
            .to_string(),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(
        app.delete_fhir("/ValueSet/rest-vs").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.delete_fhir("/ValueSet/rest-vs").await,
        StatusCode::NOT_FOUND
    );
}

// ── T5d: PUT update-as-create ─────────────────────────────────────────────────

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn put_to_unknown_id_creates_then_updates() {
    let app = TestApp::with_resource_store();

    let (st, body) = app
        .put_fhir(
            "/CodeSystem/brand-new",
            code_system("brand-new", "1.0").to_string(),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["id"], "brand-new");

    let (st, body) = app.get_fhir("/CodeSystem/brand-new").await;
    assert_eq!(st, StatusCode::OK, "{body}");

    let (st, body) = app
        .put_fhir(
            "/CodeSystem/brand-new",
            code_system("brand-new", "1.0").to_string(),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");

    // Body id must agree with the URL id.
    let (st, body) = app
        .put_fhir(
            "/CodeSystem/other-id",
            code_system("brand-new", "1.0").to_string(),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");

    // $subsumes works on the PUT-created CodeSystem too.
    let (st, body) = subsumes(&app, "root", "leaf").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(outcome(&body), Some("subsumes"), "{body}");
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn put_over_imported_resource_replaces_it() {
    let app = TestApp::with_resource_store();
    app.import_bundle_ok(&import_bundle()).await;

    let mut replaced = code_system("crud-cs", "1.0");
    replaced["concept"][0]["display"] = json!("Root replaced");
    let (st, body) = app
        .put_fhir("/CodeSystem/crud-cs", replaced.to_string())
        .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");

    let (st, body) = app
        .get_fhir(&format!("/CodeSystem/$lookup?system={CS_URL}&code=root"))
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let display = body["parameter"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "display")
        .and_then(|p| p["valueString"].as_str());
    assert_eq!(display, Some("Root replaced"), "{body}");

    // Still exactly one row for the (url, version).
    let (st, body) = app.get_fhir(&format!("/CodeSystem?url={CS_URL}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["total"], 1, "{body}");
}
