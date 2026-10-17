//! Several versions of a ConceptMap are stored side by side, and every
//! versionless use of the map URL — `GET /ConceptMap?url=…` and `$translate`
//! with or without `url` — picks the default (semantically newest) version,
//! the rule CodeSystems and ValueSets follow.
//!
//! Before, `concept_maps.url` was unique: importing version 1.9 after 1.10
//! replaced 1.10, so whatever was imported last won.
//!
//! Runs on SQLite by default and on PostgreSQL with
//! `HTS_TEST_BACKEND=postgres` (feature `postgres`).

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};

const CM_URL: &str = "http://hts.test/cm-versions/map";
const SRC: &str = "http://hts.test/cm-versions/src";
const TGT: &str = "http://hts.test/cm-versions/tgt";

fn cm(version: &str, target: &str) -> Value {
    json!({
        "resourceType": "ConceptMap", "id": "map", "url": CM_URL, "version": version,
        "name": "Map", "status": "active",
        "group": [{"source": SRC, "target": TGT, "element": [
            {"code": "A", "target": [{"code": target, "equivalence": "equivalent"}]}
        ]}]
    })
}

fn bundle(resources: Vec<Value>) -> String {
    json!({
        "resourceType": "Bundle", "type": "collection",
        "entry": resources.into_iter().map(|r| json!({"resource": r})).collect::<Vec<_>>()
    })
    .to_string()
}

fn enc(s: &str) -> String {
    s.replace(':', "%3A")
        .replace('/', "%2F")
        .replace('|', "%7C")
}

/// Target codes of the `match` parts.
fn targets(body: &Value) -> Vec<String> {
    let mut out: Vec<String> = body["parameter"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["name"] == "match")
        .filter_map(|m| {
            m["part"]
                .as_array()?
                .iter()
                .find(|p| p["name"] == "concept")
                .and_then(|p| p["valueCoding"]["code"].as_str())
                .map(str::to_string)
        })
        .collect();
    out.sort();
    out
}

async fn translate(app: &TestApp, query: &str) -> Vec<String> {
    let (status, body) = app
        .get_fhir(&format!("/ConceptMap/$translate?{query}"))
        .await;
    assert_eq!(status, StatusCode::OK, "{query}: {body}");
    targets(&body)
}

/// Newest first by semantics (1.10 > 1.9), oldest imported last.
async fn app() -> TestApp {
    let app = TestApp::with_resource_store().await;
    app.import_bundle_ok(&bundle(vec![cm("1.10", "NEW")])).await;
    app.import_bundle_ok(&bundle(vec![cm("1.9", "OLD")])).await;
    app
}

#[tokio::test]
async fn versionless_search_lists_the_default_version_first() {
    let app = app().await;
    let (status, body) = app
        .get_fhir(&format!("/ConceptMap?url={}", enc(CM_URL)))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let got: Vec<(String, String)> = body["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["resource"]["version"].as_str().unwrap().to_string(),
                e["resource"]["id"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("1.10".to_string(), "map".to_string()),
            ("1.9".to_string(), "map-1.9".to_string())
        ]
    );
    assert_eq!(body["total"], 2);

    let (_, one) = app
        .get_fhir(&format!("/ConceptMap?url={}&_count=1", enc(CM_URL)))
        .await;
    assert_eq!(one["entry"][0]["resource"]["version"], "1.10", "{one}");

    for (id, version) in [("map", "1.10"), ("map-1.9", "1.9")] {
        let (status, body) = app.get_fhir(&format!("/ConceptMap/{id}")).await;
        assert_eq!(status, StatusCode::OK, "{id}: {body}");
        assert_eq!(body["version"], version, "{id}");
        assert_eq!(body["id"], id, "{id}");
    }
}

#[tokio::test]
async fn translate_uses_the_default_version_unless_one_is_named() {
    let app = app().await;
    let code = format!("system={}&code=A", enc(SRC));
    let url = enc(CM_URL);
    assert_eq!(translate(&app, &format!("url={url}&{code}")).await, ["NEW"]);
    // Without `url`, each map URL still contributes only its default version.
    assert_eq!(translate(&app, &code).await, ["NEW"]);
    assert_eq!(
        translate(&app, &format!("url={url}&conceptMapVersion=1.9&{code}")).await,
        ["OLD"]
    );
    assert_eq!(
        translate(
            &app,
            &format!("url={}&{code}", enc(&format!("{CM_URL}|1.9")))
        )
        .await,
        ["OLD"]
    );
    let (status, _) = app
        .get_fhir(&format!(
            "/ConceptMap/$translate?url={url}&conceptMapVersion=3&{code}"
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Instance-level: the id names the version.
    for (id, want) in [("map", "NEW"), ("map-1.9", "OLD")] {
        let (status, body) = app
            .get_fhir(&format!("/ConceptMap/{id}/$translate?{code}"))
            .await;
        assert_eq!(status, StatusCode::OK, "{id}: {body}");
        assert_eq!(targets(&body), [want], "{id}: {body}");
    }
}

#[tokio::test]
async fn reimporting_a_version_replaces_only_that_version() {
    let app = app().await;
    app.import_bundle_ok(&bundle(vec![cm("1.9", "OLDER")]))
        .await;
    let code = format!("system={}&code=A", enc(SRC));
    let url = enc(CM_URL);
    assert_eq!(
        translate(&app, &format!("url={url}&conceptMapVersion=1.9&{code}")).await,
        ["OLDER"]
    );
    assert_eq!(translate(&app, &format!("url={url}&{code}")).await, ["NEW"]);

    // Deleting the default version hands the URL (and the plain id) to 1.9.
    assert_eq!(
        app.delete_fhir("/ConceptMap/map").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        translate(&app, &format!("url={url}&{code}")).await,
        ["OLDER"]
    );
    let (status, body) = app.get_fhir("/ConceptMap/map").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version"], "1.9");
}

/// A database created when `concept_maps.url` was `UNIQUE` is rebuilt on
/// open without losing its maps or their elements.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn legacy_unique_url_table_is_rebuilt_with_its_elements() {
    use helios_hts::backends::SqliteTerminologyBackend;
    use helios_hts::traits::ConceptMapOperations;
    use helios_hts::types::TranslateRequest;
    use helios_persistence::tenant::TenantContext;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hts.db");
    let path = path.to_str().unwrap();
    {
        let backend = SqliteTerminologyBackend::new(path).unwrap();
        let conn = backend.pool().get().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF;
             DROP TABLE concept_maps;
             CREATE TABLE concept_maps (
                 id TEXT PRIMARY KEY, url TEXT NOT NULL UNIQUE, version TEXT,
                 source_uri TEXT, target_uri TEXT, status TEXT NOT NULL DEFAULT 'active',
                 created_at TEXT NOT NULL, name TEXT, title TEXT, resource_json TEXT);
             INSERT INTO concept_maps (id, url, version, status, created_at)
                 VALUES ('legacy', 'http://hts.test/cm-versions/legacy', '1', 'active', 'x');
             INSERT INTO concept_map_elements
                 (map_id, source_system, source_code, target_system, target_code, equivalence)
                 VALUES ('legacy', 'http://s', 'A', 'http://t', 'X', 'equivalent');
             PRAGMA foreign_keys=ON;",
        )
        .unwrap();
    }
    let backend = SqliteTerminologyBackend::new(path).unwrap();
    let unique_url: bool = backend
        .pool()
        .get()
        .unwrap()
        .query_row(
            "SELECT sql LIKE '%url%TEXT%NOT NULL%UNIQUE%' FROM sqlite_master \
             WHERE type = 'table' AND name = 'concept_maps'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!unique_url, "the legacy UNIQUE(url) must be gone");
    let resp = backend
        .translate(
            &TenantContext::system(),
            TranslateRequest {
                url: Some("http://hts.test/cm-versions/legacy".into()),
                system: Some("http://s".into()),
                code: "A".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(resp.matches.len(), 1, "elements survive the rebuild");
}
