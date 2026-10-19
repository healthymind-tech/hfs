//! CodeSystem supplements are applied automatically to `$lookup` and `$expand`
//! (fork task 7): designations contributed by a stored `content=supplement`
//! CodeSystem show up, and `displayLanguage` can select them.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};

const CS_URL: &str = "http://hts.test/supp/cs";
const SUP_URL: &str = "http://hts.test/supp/cs-zh-hant";
const VS_URL: &str = "http://hts.test/supp/vs";

fn bundle() -> String {
    json!({
      "resourceType": "Bundle", "type": "collection",
      "entry": [
        {"resource": {
          "resourceType": "CodeSystem", "id": "supp-cs", "url": CS_URL, "version": "2025",
          "status": "active", "content": "complete", "language": "en",
          "concept": [
            {"code": "E11.9", "display": "Type 2 diabetes mellitus without complications",
             "designation": [{"language": "de", "value": "Diabetes mellitus Typ 2 ohne Komplikationen"}]},
            {"code": "I10", "display": "Essential (primary) hypertension"}
          ]
        }},
        {"resource": {
          "resourceType": "CodeSystem", "id": "supp-zh", "url": SUP_URL, "version": "2023",
          "status": "active", "content": "supplement", "supplements": CS_URL,
          "concept": [
            {"code": "E11.9", "designation": [{"language": "zh-Hant", "value": "第2型糖尿病，無併發症"}]},
            {"code": "I10", "designation": [{"language": "zh-Hant", "value": "本態性（原發性）高血壓"}]}
          ]
        }},
        {"resource": {
          "resourceType": "ValueSet", "id": "supp-vs", "url": VS_URL, "status": "active",
          "compose": {"include": [{"system": CS_URL}]}
        }}
      ]
    })
    .to_string()
}

fn params(body: &Value) -> &Vec<Value> {
    body["parameter"].as_array().unwrap()
}

fn display(body: &Value) -> Option<&str> {
    params(body)
        .iter()
        .find(|p| p["name"] == "display")
        .and_then(|p| p["valueString"].as_str())
}

fn designation_values(body: &Value, lang: &str) -> Vec<String> {
    params(body)
        .iter()
        .filter(|p| p["name"] == "designation")
        .filter(|p| {
            p["part"]
                .as_array()
                .unwrap()
                .iter()
                .any(|q| q["name"] == "language" && q["valueCode"] == lang)
        })
        .filter_map(|p| {
            p["part"]
                .as_array()
                .unwrap()
                .iter()
                .find(|q| q["name"] == "value")
                .and_then(|q| q["valueString"].as_str())
                .map(str::to_owned)
        })
        .collect()
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn lookup_returns_supplement_designations_without_use_supplement() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&bundle()).await;

    let (st, body) = app
        .get_fhir(&format!("/CodeSystem/$lookup?system={CS_URL}&code=E11.9"))
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        designation_values(&body, "zh-Hant"),
        ["第2型糖尿病，無併發症"],
        "{body}"
    );
    assert_eq!(
        designation_values(&body, "de"),
        ["Diabetes mellitus Typ 2 ohne Komplikationen"],
        "{body}"
    );
    // Base display untouched when no displayLanguage was requested.
    assert_eq!(
        display(&body),
        Some("Type 2 diabetes mellitus without complications")
    );
    // The applied supplement is echoed.
    assert!(
        params(&body).iter().any(|p| p["name"] == "used-supplement"
            && p["valueCanonical"].as_str() == Some(&format!("{SUP_URL}|2023"))),
        "{body}"
    );
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn lookup_display_language_selects_supplement_translation() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&bundle()).await;

    let (st, body) = app
        .get_fhir(&format!(
            "/CodeSystem/$lookup?system={CS_URL}&code=I10&displayLanguage=zh-Hant"
        ))
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(display(&body), Some("本態性（原發性）高血壓"), "{body}");
    assert_eq!(
        designation_values(&body, "zh-Hant"),
        ["本態性（原發性）高血壓"]
    );
    // Other languages are filtered out when displayLanguage is set.
    assert!(designation_values(&body, "de").is_empty(), "{body}");

    // A base-CS designation in the requested language still wins over the supplement.
    let (st, body) = app
        .get_fhir(&format!(
            "/CodeSystem/$lookup?system={CS_URL}&code=E11.9&displayLanguage=de"
        ))
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        display(&body),
        Some("Diabetes mellitus Typ 2 ohne Komplikationen"),
        "{body}"
    );
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn expand_display_language_and_designations_use_supplement() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&bundle()).await;

    let (st, body) = app
        .get_fhir(&format!(
            "/ValueSet/$expand?url={VS_URL}&displayLanguage=zh-Hant&includeDesignations=true"
        ))
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let contains = body["expansion"]["contains"].as_array().unwrap();
    let i10 = contains
        .iter()
        .find(|c| c["code"] == "I10")
        .unwrap_or_else(|| panic!("{body}"));
    assert_eq!(i10["display"], "本態性（原發性）高血壓", "{body}");
    let e119 = contains.iter().find(|c| c["code"] == "E11.9").unwrap();
    assert_eq!(e119["display"], "第2型糖尿病，無併發症", "{body}");
    // The English display is rotated into the designations (preferredForLanguage).
    let langs: Vec<&str> = e119["designation"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["language"].as_str())
        .collect();
    assert!(langs.contains(&"en"), "{body}");

    // Without displayLanguage the base display stays and supplement
    // designations are listed when designations are requested.
    let (st, body) = app
        .get_fhir(&format!(
            "/ValueSet/$expand?url={VS_URL}&includeDesignations=true"
        ))
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let i10 = body["expansion"]["contains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["code"] == "I10")
        .unwrap();
    assert_eq!(i10["display"], "Essential (primary) hypertension");
    assert!(
        i10["designation"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["language"] == "zh-Hant" && d["value"] == "本態性（原發性）高血壓"),
        "{body}"
    );
}

/// A display from a stored translation supplement validates when the
/// request asks for that language (`zh-TW` is served by `zh-Hant`), as the
/// HL7 validator accepts any designation in the requested language — the
/// same display `$lookup ... displayLanguage=zh-TW` hands out. Without a
/// `displayLanguage` an unrequested supplement does not count (tx.fhir.org:
/// `parameters-validate-supplement-none`), and another language does not
/// pick it up either.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn validate_code_accepts_supplement_display_in_the_requested_language() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&bundle()).await;
    let display = "%E6%9C%AC%E6%85%8B%E6%80%A7%EF%BC%88%E5%8E%9F%E7%99%BC%E6%80%A7%EF%BC%89%E9%AB%98%E8%A1%80%E5%A3%93";
    let result = |body: &Value| {
        params(body)
            .iter()
            .find(|p| p["name"] == "result")
            .and_then(|p| p["valueBoolean"].as_bool())
    };
    for (lang, expected) in [
        ("&displayLanguage=zh-TW", true),
        ("&displayLanguage=zh-Hant", true),
        ("", false),
        ("&displayLanguage=de", false),
    ] {
        let (status, body) = app
            .get_fhir(&format!(
                "/CodeSystem/$validate-code?url={CS_URL}&code=I10&display={display}{lang}"
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{lang}: {body}");
        assert_eq!(result(&body), Some(expected), "{lang}: {body}");
        let (_, body) = app
            .get_fhir(&format!(
                "/ValueSet/$validate-code?url={VS_URL}&system={CS_URL}&code=I10&display={display}{lang}"
            ))
            .await;
        assert_eq!(result(&body), Some(expected), "VS {lang}: {body}");
    }
}
