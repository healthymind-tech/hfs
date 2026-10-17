//! SNOMED CT releases are reported in the FHIR edition URI form
//! (`http://snomed.info/sct/900000000000207008/version/20250601`, as
//! tx.fhir.org reports them) while storage keeps the RF2 release date; a
//! request naming the release by date, by full URI or by the edition URI
//! (`http://snomed.info/sct/900000000000207008`) resolves everywhere a
//! version can be given.
//!
//! Runs on SQLite, or on PostgreSQL with `HTS_TEST_BACKEND=postgres`.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};

const SCT: &str = "http://snomed.info/sct";
const INT: &str = "http://snomed.info/sct/900000000000207008";
const URI: &str = "http://snomed.info/sct/900000000000207008/version/20250601";
const US: &str = "http://snomed.info/sct/731000124108";

fn vs(id: &str, include_version: Option<&str>) -> Value {
    let mut include = json!({"system": SCT, "concept": [{"code": "763875007"}]});
    if let Some(v) = include_version {
        include["version"] = json!(v);
    }
    json!({"resourceType": "ValueSet", "id": id, "url": format!("http://example.org/vs/{id}"),
        "status": "active", "compose": {"include": [include]}})
}

async fn app() -> TestApp {
    let app = TestApp::with_resource_store().await;
    let bundle = json!({"resourceType": "Bundle", "type": "collection", "entry": [
        {"resource": {"resourceType": "CodeSystem", "id": "sct", "url": SCT, "version": "20250601",
            "status": "active", "content": "complete", "hierarchyMeaning": "is-a",
            "concept": [{"code": "763875007", "display": "Sulfonamide-containing product",
                "concept": [{"code": "111", "display": "Child product"}]}]}},
        {"resource": vs("plain", None)},
        {"resource": vs("edition", Some(INT))},
        {"resource": vs("release", Some(URI))},
        {"resource": vs("dated", Some("20250601"))}
    ]});
    app.import_bundle_ok(&bundle.to_string()).await;
    app
}

fn enc(s: &str) -> String {
    s.replace(':', "%3A")
        .replace('/', "%2F")
        .replace('|', "%7C")
}

fn param<'a>(out: &'a Value, name: &str) -> Option<&'a Value> {
    out["parameter"]
        .as_array()?
        .iter()
        .find(|p| p["name"] == name)
}

fn param_str<'a>(out: &'a Value, name: &str) -> Option<&'a str> {
    let p = param(out, name)?;
    p.as_object()?
        .iter()
        .find(|(k, _)| k.starts_with("value"))
        .and_then(|(_, v)| v.as_str())
}

fn used_codesystems(expansion: &Value) -> Vec<String> {
    expansion["expansion"]["parameter"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["name"] == "used-codesystem")
        .filter_map(|p| p["valueUri"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn lookup_and_validate_code_report_the_edition_uri() {
    let app = app().await;
    for version in ["", "20250601", INT, URI] {
        let v = if version.is_empty() {
            String::new()
        } else {
            format!("&version={}", enc(version))
        };
        let (status, out) = app
            .get_fhir(&format!(
                "/CodeSystem/$lookup?system={}&code=763875007{v}",
                enc(SCT)
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{version}: {out}");
        assert_eq!(param_str(&out, "version"), Some(URI), "{version}: {out}");

        let (_, out) = app
            .get_fhir(&format!(
                "/CodeSystem/$validate-code?url={}&code=763875007{v}",
                enc(SCT)
            ))
            .await;
        assert_eq!(
            param(&out, "result").unwrap()["valueBoolean"],
            true,
            "{version}: {out}"
        );
        assert_eq!(param_str(&out, "version"), Some(URI), "{version}: {out}");
    }

    // ValueSet $validate-code with a coding naming the release by URI.
    let body = json!({"resourceType": "Parameters", "parameter": [
        {"name": "url", "valueUri": "http://example.org/vs/plain"},
        {"name": "coding", "valueCoding": {"system": SCT, "version": URI, "code": "763875007"}}
    ]});
    let (_, out) = app
        .post_fhir("/ValueSet/$validate-code", body.to_string())
        .await;
    assert_eq!(
        param(&out, "result").unwrap()["valueBoolean"],
        true,
        "{out}"
    );
    assert_eq!(param_str(&out, "version"), Some(URI), "{out}");

    // An unknown code names the release by URI in the message too.
    let (_, out) = app
        .get_fhir(&format!(
            "/CodeSystem/$validate-code?url={}&code=999",
            enc(SCT)
        ))
        .await;
    let message = param_str(&out, "message").unwrap_or_default();
    assert!(!message.contains("'20250601'"), "{out}");

    // An edition that is not loaded stays unknown.
    let (_, out) = app
        .get_fhir(&format!(
            "/CodeSystem/$validate-code?url={}&code=763875007&version={}",
            enc(SCT),
            enc(US)
        ))
        .await;
    assert_eq!(
        param(&out, "result").unwrap()["valueBoolean"],
        false,
        "{out}"
    );
}

#[tokio::test]
async fn expand_accepts_and_reports_edition_uris() {
    let app = app().await;
    // Stored ValueSets pinning the release by edition URI, release URI or date.
    for id in ["plain", "edition", "release", "dated"] {
        let (status, out) = app
            .get_fhir(&format!(
                "/ValueSet/$expand?url={}",
                enc(&format!("http://example.org/vs/{id}"))
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{id}: {out}");
        assert_eq!(
            out["expansion"]["contains"][0]["code"], "763875007",
            "{id}: {out}"
        );
        assert_eq!(
            used_codesystems(&out),
            [format!("{SCT}|{URI}")],
            "{id}: {out}"
        );
    }
    // Inline ValueSet with an edition URI include, and a `system-version` pin.
    for (vs, pin) in [
        (vs("inline", Some(INT)), None),
        (vs("inline2", None), Some(format!("{SCT}|{URI}"))),
        (vs("inline3", None), Some(format!("{SCT}|{INT}"))),
    ] {
        let mut params = vec![json!({"name": "valueSet", "resource": vs})];
        if let Some(pin) = &pin {
            params.push(json!({"name": "system-version", "valueUri": pin}));
        }
        let body = json!({"resourceType": "Parameters", "parameter": params});
        let (status, out) = app.post_fhir("/ValueSet/$expand", body.to_string()).await;
        assert_eq!(status, StatusCode::OK, "{pin:?}: {out}");
        assert_eq!(
            out["expansion"]["contains"][0]["code"], "763875007",
            "{out}"
        );
        assert_eq!(used_codesystems(&out), [format!("{SCT}|{URI}")], "{out}");
    }
    // An edition that is not loaded is an unknown version.
    let body = json!({"resourceType": "Parameters", "parameter": [
        {"name": "valueSet", "resource": vs("us", Some(US))}
    ]});
    let (status, _) = app.post_fhir("/ValueSet/$expand", body.to_string()).await;
    assert!(status.is_client_error(), "{status}");
}

#[tokio::test]
async fn code_system_resources_report_the_edition_uri() {
    let app = app().await;
    let (status, cs) = app.get_fhir("/CodeSystem/sct").await;
    assert_eq!(status, StatusCode::OK, "{cs}");
    assert_eq!(cs["version"], URI);
    for version in ["", "20250601", INT, URI] {
        let v = if version.is_empty() {
            String::new()
        } else {
            format!("&version={}", enc(version))
        };
        let (status, bundle) = app
            .get_fhir(&format!("/CodeSystem?url={}{v}", enc(SCT)))
            .await;
        assert_eq!(status, StatusCode::OK, "{bundle}");
        assert_eq!(bundle["total"], 1, "{version}: {bundle}");
        assert_eq!(
            bundle["entry"][0]["resource"]["version"], URI,
            "{version}: {bundle}"
        );
    }
    let (_, bundle) = app
        .get_fhir(&format!("/CodeSystem?url={}&version={}", enc(SCT), enc(US)))
        .await;
    assert_eq!(bundle["total"], 0, "{bundle}");
}

#[tokio::test]
async fn subsumes_accepts_the_edition_uri() {
    let app = app().await;
    for version in ["20250601", INT, URI] {
        let (status, out) = app
            .get_fhir(&format!(
                "/CodeSystem/$subsumes?system={}&codeA=763875007&codeB=111&version={}",
                enc(SCT),
                enc(version)
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{version}: {out}");
        assert_eq!(
            param_str(&out, "outcome"),
            Some("subsumes"),
            "{version}: {out}"
        );
    }
}
