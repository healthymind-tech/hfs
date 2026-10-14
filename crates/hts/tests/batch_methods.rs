//! `POST /` batch Bundles honour `entry.request.method` (fork task: batch
//! support for GET operations and PUT/POST/DELETE resource writes).

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};

const CS_URL: &str = "http://hts.test/batch/cs";
const VS_URL: &str = "http://hts.test/batch/vs";

fn code_system() -> Value {
    json!({
      "resourceType": "CodeSystem", "id": "batch-cs", "url": CS_URL, "version": "1.0",
      "status": "active", "content": "complete", "hierarchyMeaning": "is-a",
      "concept": [{"code": "root", "display": "Root", "concept": [{"code": "kid", "display": "Kid"}]}]
    })
}

fn value_set() -> Value {
    json!({
      "resourceType": "ValueSet", "id": "batch-vs", "url": VS_URL, "status": "active",
      "compose": {"include": [{"system": CS_URL, "filter": [{"property": "concept", "op": "is-a", "value": "root"}]}]}
    })
}

fn bundle(entries: Vec<Value>) -> String {
    json!({"resourceType": "Bundle", "type": "batch", "entry": entries}).to_string()
}

fn statuses(body: &Value) -> Vec<String> {
    body["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["response"]["status"].as_str().unwrap_or("").to_string())
        .collect()
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn batch_creates_reads_operates_updates_and_deletes() {
    let app = TestApp::with_resource_store();

    // 1. Create both resources through the batch endpoint.
    let (st, body) = app
        .post_fhir(
            "/",
            bundle(vec![
                json!({"request": {"method": "POST", "url": "CodeSystem"}, "resource": code_system()}),
                json!({"request": {"method": "POST", "url": "ValueSet"}, "resource": value_set()}),
            ]),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["type"], "batch-response");
    assert_eq!(statuses(&body), ["201", "201"], "{body}");
    assert_eq!(body["entry"][0]["resource"]["resourceType"], "CodeSystem");

    // 2. Mixed GET / POST operations plus a read by id.
    let (st, body) = app
        .post_fhir(
            "/",
            bundle(vec![
                json!({"request": {"method": "GET", "url": format!("CodeSystem/$lookup?system={CS_URL}&code=kid")}}),
                json!({"request": {"method": "GET", "url": format!("ValueSet/$expand?url={VS_URL}&excludeNested=true")}}),
                json!({"request": {"method": "GET", "url": format!("CodeSystem/$subsumes?system={CS_URL}&codeA=root&codeB=kid")}}),
                json!({"request": {"method": "POST", "url": "ValueSet/$validate-code"},
                       "resource": {"resourceType": "Parameters", "parameter": [
                           {"name": "url", "valueUri": VS_URL}, {"name": "system", "valueUri": CS_URL}, {"name": "code", "valueCode": "kid"}]}}),
                json!({"request": {"method": "GET", "url": "ValueSet/batch-vs"}}),
                json!({"request": {"method": "GET", "url": "ValueSet/batch-vs/$expand?excludeNested=true"}}),
                json!({"request": {"method": "GET", "url": "CodeSystem/does-not-exist"}}),
            ]),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        statuses(&body),
        ["200", "200", "200", "200", "200", "200", "404"],
        "{body}"
    );
    let entries = body["entry"].as_array().unwrap();
    assert_eq!(entries[0]["resource"]["resourceType"], "Parameters");
    assert_eq!(
        entries[1]["resource"]["expansion"]["contains"]
            .as_array()
            .map(Vec::len),
        Some(2),
        "{body}"
    );
    let outcome = entries[2]["resource"]["parameter"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "outcome")
        .map(|p| p["valueCode"].clone());
    assert_eq!(outcome, Some(json!("subsumes")));
    let result = entries[3]["resource"]["parameter"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "result")
        .map(|p| p["valueBoolean"].clone());
    assert_eq!(result, Some(json!(true)));
    assert_eq!(entries[4]["resource"]["url"], VS_URL);
    assert_eq!(
        entries[5]["resource"]["expansion"]["contains"]
            .as_array()
            .map(Vec::len),
        Some(2),
        "{body}"
    );
    assert_eq!(entries[6]["resource"]["resourceType"], "OperationOutcome");

    // 3. PUT (update) then DELETE, then the deleted read is a 404.
    let mut updated = code_system();
    updated["concept"][0]["display"] = json!("Root renamed");
    let (st, body) = app
        .post_fhir(
            "/",
            bundle(vec![
                json!({"request": {"method": "PUT", "url": "CodeSystem/batch-cs"}, "resource": updated}),
                json!({"request": {"method": "DELETE", "url": "ValueSet/batch-vs"}}),
                json!({"request": {"method": "GET", "url": "ValueSet/batch-vs"}}),
                json!({"request": {"method": "PUT", "url": "ValueSet/created-by-put"},
                       "resource": {"resourceType": "ValueSet", "url": "http://hts.test/batch/vs2", "status": "active",
                                    "compose": {"include": [{"system": CS_URL, "concept": [{"code": "kid"}]}]}}}),
            ]),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(statuses(&body), ["200", "204", "404", "201"], "{body}");
    assert!(body["entry"][1].get("resource").is_none(), "{body}");

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
    assert_eq!(display, Some("Root renamed"));
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn batch_entry_without_method_defaults_to_post_and_bad_entries_are_isolated() {
    let app = TestApp::with_resource_store();
    app.post_fhir("/CodeSystem", code_system().to_string())
        .await;

    let (st, body) = app
        .post_fhir(
            "/",
            bundle(vec![
                json!({"request": {"url": "CodeSystem/$lookup"},
                       "resource": {"resourceType": "Parameters", "parameter": [
                           {"name": "system", "valueUri": CS_URL}, {"name": "code", "valueCode": "root"}]}}),
                json!({"request": {"method": "PATCH", "url": "CodeSystem/batch-cs"}}),
                json!({"request": {"method": "POST", "url": "Patient"}, "resource": {"resourceType": "Patient"}}),
                json!({"request": {"method": "POST", "url": "CodeSystem"}, "resource": {"resourceType": "ValueSet"}}),
            ]),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(statuses(&body), ["200", "400", "400", "400"], "{body}");
}
