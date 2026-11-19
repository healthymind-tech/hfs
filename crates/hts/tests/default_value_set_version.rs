//! The default (versionless) version of a ValueSet is the one the backend
//! expands — the highest version — and every message that names the
//! ValueSet must name that version, not the oldest-imported row.
//!
//! Runs on SQLite by default and on PostgreSQL with
//! `HTS_TEST_BACKEND=postgres` (feature `postgres`).

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};

const CS_URL: &str = "http://hts.test/default-vs-version/cs";
const VS_URL: &str = "http://hts.test/default-vs-version/vs";

fn bundle() -> String {
    let vs = |version: &str, codes: &[&str]| {
        json!({
            "resourceType": "ValueSet", "id": format!("vs-{version}"), "url": VS_URL,
            "version": version, "name": "DefaultVS", "status": "active",
            "compose": {"include": [{"system": CS_URL,
                "concept": codes.iter().map(|c| json!({"code": c})).collect::<Vec<_>>()}]}
        })
    };
    json!({
        "resourceType": "Bundle", "type": "collection",
        "entry": [
            {"resource": {
                "resourceType": "CodeSystem", "id": "dvs-cs", "url": CS_URL,
                "version": "1", "name": "DefaultVSCS", "status": "active",
                "content": "complete",
                "concept": [{"code": "a", "display": "A"}, {"code": "b", "display": "B"},
                            {"code": "c", "display": "C"}]
            }},
            // Oldest row first, and `1.9` < `1.10` only under semantic ordering.
            {"resource": vs("1.9", &["a"])},
            {"resource": vs("1.10", &["a", "b"])}
        ]
    })
    .to_string()
}

fn out_param<'a>(body: &'a Value, name: &str) -> Option<&'a Value> {
    body["parameter"]
        .as_array()?
        .iter()
        .find(|p| p["name"] == name)
}

#[tokio::test(flavor = "multi_thread")]
async fn versionless_url_search_lists_the_default_version_first() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&bundle()).await;
    let (st, all) = app.get_fhir(&format!("/ValueSet?url={VS_URL}")).await;
    assert_eq!(st, StatusCode::OK, "{all}");
    assert_eq!(all["total"], 2, "{all}");
    assert_eq!(all["entry"][0]["resource"]["version"], "1.10", "{all}");
}

#[tokio::test(flavor = "multi_thread")]
async fn versionless_validate_code_names_the_version_it_validated_against() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&bundle()).await;

    // `b` is only in 1.10: the default version is the one validated against.
    let (st, ok) = app
        .get_fhir(&format!(
            "/ValueSet/$validate-code?url={VS_URL}&system={CS_URL}&code=b"
        ))
        .await;
    assert_eq!(st, StatusCode::OK, "{ok}");
    assert_eq!(
        out_param(&ok, "result").unwrap()["valueBoolean"],
        true,
        "{ok}"
    );

    // `c` is in neither: the not-in-value-set message names `|1.10`.
    let (st, bad) = app
        .get_fhir(&format!(
            "/ValueSet/$validate-code?url={VS_URL}&system={CS_URL}&code=c"
        ))
        .await;
    assert_eq!(st, StatusCode::OK, "{bad}");
    assert_eq!(
        out_param(&bad, "result").unwrap()["valueBoolean"],
        false,
        "{bad}"
    );
    let text = bad.to_string();
    assert!(text.contains(&format!("{VS_URL}|1.10")), "{bad}");
    assert!(!text.contains(&format!("{VS_URL}|1.9")), "{bad}");
}

#[tokio::test(flavor = "multi_thread")]
async fn versionless_coding_validate_code_names_the_default_version() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&bundle()).await;
    for coding in [
        json!({"system": CS_URL, "code": "c"}),
        // No system: the not-in-value-set issue still names the ValueSet.
        json!({"code": "c"}),
    ] {
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "url", "valueUri": VS_URL},
                {"name": "coding", "valueCoding": coding}
            ]
        });
        let (st, out) = app
            .post_fhir("/ValueSet/$validate-code", body.to_string())
            .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(
            out_param(&out, "result").unwrap()["valueBoolean"],
            false,
            "{out}"
        );
        let text = out.to_string();
        assert!(text.contains(&format!("{VS_URL}|1.10")), "{out}");
        assert!(!text.contains(&format!("{VS_URL}|1.9")), "{out}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn versionless_expand_expands_the_default_version() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&bundle()).await;
    let (st, out) = app
        .get_fhir(&format!("/ValueSet/$expand?url={VS_URL}"))
        .await;
    assert_eq!(st, StatusCode::OK, "{out}");
    assert_eq!(out["version"], "1.10", "{out}");
    assert_eq!(out["expansion"]["total"], 2, "{out}");
}
