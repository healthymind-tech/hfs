//! Search parameters beyond url/version/name/title/status on the terminology
//! resource endpoints: `supplements`, `system`, `content-mode` (CodeSystem),
//! `publisher`, `_id` and `_lastUpdated` (all three types).
//!
//! The HTTP tests run on SQLite, or on PostgreSQL with
//! `HTS_TEST_BACKEND=postgres`. The upgrade tests check that rows stored by a
//! build without the search columns become searchable once the new schema is
//! applied, on both backends.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use helios_hts::import::BundleImportBackend;
use helios_hts::traits::CodeSystemOperations;
use helios_hts::types::ResourceSearchQuery;
use helios_persistence::tenant::TenantContext;
use serde_json::Value;

const BUNDLE: &str = r#"{
  "resourceType": "Bundle",
  "type": "collection",
  "entry": [
    {"resource": {
      "resourceType": "CodeSystem", "id": "p2o-base",
      "url": "http://hts.test/p2o/cs/base", "version": "1.0",
      "name": "Base", "status": "active", "publisher": "Acme Health",
      "content": "complete", "concept": [{"code": "a", "display": "A"}]
    }},
    {"resource": {
      "resourceType": "CodeSystem", "id": "p2o-sup-bare",
      "url": "http://hts.test/p2o/cs/sup-bare", "version": "1",
      "name": "SupBare", "status": "active", "publisher": "Taiwan MOHW",
      "content": "supplement", "supplements": "http://hts.test/p2o/cs/base",
      "concept": [{"code": "a", "designation": [{"language": "zh-TW", "value": "甲"}]}]
    }},
    {"resource": {
      "resourceType": "CodeSystem", "id": "p2o-sup-versioned",
      "url": "http://hts.test/p2o/cs/sup-versioned", "version": "1",
      "name": "SupVersioned", "status": "draft",
      "content": "supplement", "supplements": "http://hts.test/p2o/cs/base|1.0",
      "concept": [{"code": "a", "designation": [{"language": "ja", "value": "甲"}]}]
    }},
    {"resource": {
      "resourceType": "CodeSystem", "id": "p2o-sup-other",
      "url": "http://hts.test/p2o/cs/sup-other", "version": "1",
      "name": "SupOther", "status": "active",
      "content": "supplement", "supplements": "http://hts.test/p2o/cs/other",
      "concept": [{"code": "x", "designation": [{"language": "de", "value": "X"}]}]
    }},
    {"resource": {
      "resourceType": "CodeSystem", "id": "p2o-fragment",
      "url": "http://hts.test/p2o/cs/fragment", "version": "1",
      "name": "Fragment", "status": "active", "publisher": "acme labs",
      "content": "fragment", "concept": [{"code": "f", "display": "F"}]
    }},
    {"resource": {
      "resourceType": "ValueSet", "id": "p2o-vs",
      "url": "http://hts.test/p2o/vs/one", "version": "1",
      "name": "VsOne", "status": "active", "publisher": "Acme Health",
      "compose": {"include": [{"system": "http://hts.test/p2o/cs/base"}]}
    }},
    {"resource": {
      "resourceType": "ValueSet", "id": "p2o-vs2",
      "url": "http://hts.test/p2o/vs/two", "version": "1",
      "name": "VsTwo", "status": "active",
      "compose": {"include": [{"system": "http://hts.test/p2o/cs/base"}]}
    }},
    {"resource": {
      "resourceType": "ConceptMap", "id": "p2o-cm",
      "url": "http://hts.test/p2o/cm/one", "version": "1",
      "name": "CmOne", "status": "active", "publisher": "Other Org",
      "sourceUri": "http://hts.test/p2o/vs/one", "targetUri": "http://hts.test/p2o/vs/two",
      "group": []
    }}
  ]
}"#;

fn ids(body: &Value) -> Vec<String> {
    let mut out: Vec<String> = body["entry"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|e| {
                    let id = e["resource"]["id"].as_str().unwrap_or_default();
                    // Multi-version imports may store `<id>|<version>`.
                    id.split('|').next().unwrap().to_owned()
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

async fn search(app: &TestApp, path: &str) -> Vec<String> {
    let (status, body) = app.get_fhir(path).await;
    assert_eq!(status, StatusCode::OK, "{path}: {body}");
    let found = ids(&body);
    assert_eq!(
        body["total"].as_u64(),
        Some(found.len() as u64),
        "{path}: total must count the filtered matches: {body}"
    );
    found
}

#[tokio::test(flavor = "multi_thread")]
async fn code_system_search_parameters_filter() {
    let app = TestApp::new().await;
    app.import_bundle_ok(BUNDLE).await;

    let cases: &[(&str, &[&str])] = &[
        // supplements: a bare canonical matches versioned references too.
        (
            "/CodeSystem?supplements=http://hts.test/p2o/cs/base",
            &["p2o-sup-bare", "p2o-sup-versioned"],
        ),
        (
            "/CodeSystem?supplements=http://hts.test/p2o/cs/base%7C1.0",
            &["p2o-sup-versioned"],
        ),
        ("/CodeSystem?supplements=http://hts.test/p2o/cs/nothing", &[]),
        (
            "/CodeSystem?supplements=http://hts.test/p2o/cs/base&status=active",
            &["p2o-sup-bare"],
        ),
        // system == url
        ("/CodeSystem?system=http://hts.test/p2o/cs/base", &["p2o-base"]),
        (
            "/CodeSystem?system=http://hts.test/p2o/cs/base&url=http://hts.test/p2o/cs/fragment",
            &[],
        ),
        ("/CodeSystem?content-mode=fragment", &["p2o-fragment"]),
        (
            "/CodeSystem?content-mode=supplement&name=SupO",
            &["p2o-sup-other"],
        ),
        // publisher: FHIR string semantics (prefix, case-insensitive).
        ("/CodeSystem?publisher=acme", &["p2o-base", "p2o-fragment"]),
        ("/CodeSystem?publisher:exact=Acme%20Health", &["p2o-base"]),
        ("/CodeSystem?publisher:contains=MOHW", &["p2o-sup-bare"]),
        ("/CodeSystem?_id=p2o-fragment", &["p2o-fragment"]),
        ("/CodeSystem?_id=p2o", &[]),
        ("/CodeSystem?_lastUpdated=lt2000-01-01&name=Base", &[]),
        (
            "/CodeSystem?_lastUpdated=gt2000&_lastUpdated=lt2999-12-31T23:59:59Z&name=Base",
            &["p2o-base"],
        ),
        ("/ValueSet?publisher=acme", &["p2o-vs"]),
        ("/ValueSet?_id=p2o-vs2", &["p2o-vs2"]),
        ("/ValueSet?_lastUpdated=ge2000-01&name=VsTwo", &["p2o-vs2"]),
        ("/ConceptMap?publisher=other", &["p2o-cm"]),
        ("/ConceptMap?publisher=acme", &[]),
        ("/ConceptMap?_lastUpdated=gt2000-01-01T00:00:00%2B08:00", &["p2o-cm"]),
    ];
    for (path, expected) in cases {
        let got = search(&app, path).await;
        assert_eq!(got, *expected, "{path}");
    }

    // `_summary=count` reports the filtered total, not the table size.
    let (status, body) = app
        .get_fhir("/CodeSystem?supplements=http://hts.test/p2o/cs/base&_summary=count")
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 2, "{body}");

    // Paging applies after the filter.
    let (_, page) = app.get_fhir("/CodeSystem?content-mode=supplement&_count=2").await;
    assert_eq!(page["total"], 3, "{page}");
    assert_eq!(page["entry"].as_array().unwrap().len(), 2, "{page}");

    // The stored resource is returned, not a synthetic summary.
    let (_, full) = app.get_fhir("/CodeSystem?supplements=http://hts.test/p2o/cs/other").await;
    assert_eq!(
        full["entry"][0]["resource"]["supplements"],
        "http://hts.test/p2o/cs/other",
        "{full}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_search_values_are_rejected() {
    let app = TestApp::new().await;
    for path in [
        "/CodeSystem?_lastUpdated=yesterday",
        "/CodeSystem?_lastUpdated=xx2020",
        "/CodeSystem?supplements:missing=true",
        "/ValueSet?publisher:below=x",
    ] {
        let (status, body) = app.get_fhir(path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
    }
}

fn supplements_query(canonical: &str) -> ResourceSearchQuery {
    ResourceSearchQuery {
        supplements: Some(canonical.to_owned()),
        ..ResourceSearchQuery::default()
    }
}

fn search_ids(found: &[Value]) -> Vec<String> {
    let mut out: Vec<String> = found
        .iter()
        .map(|r| r["id"].as_str().unwrap().split('|').next().unwrap().to_owned())
        .collect();
    out.sort();
    out
}

/// A database written before the search columns existed: the columns are
/// dropped again after the import, and reopening the backend must make the
/// stored rows searchable without re-importing them.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn sqlite_existing_rows_become_searchable_after_upgrade() {
    use helios_hts::backends::SqliteTerminologyBackend;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hts.db");
    let path = path.to_str().unwrap();
    {
        let backend = SqliteTerminologyBackend::new(path).unwrap();
        backend
            .import_bundle(&TenantContext::system(), BUNDLE.as_bytes())
            .await
            .unwrap();
    }
    {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "ALTER TABLE code_systems DROP COLUMN supplements;
             ALTER TABLE code_systems DROP COLUMN publisher;
             ALTER TABLE value_sets DROP COLUMN publisher;
             ALTER TABLE concept_maps DROP COLUMN publisher;",
        )
        .unwrap();
    }
    let backend = SqliteTerminologyBackend::new(path).unwrap();
    let found = CodeSystemOperations::search(
        &backend,
        &TenantContext::system(),
        supplements_query("http://hts.test/p2o/cs/base"),
    )
    .await
    .unwrap();
    assert_eq!(search_ids(&found), ["p2o-sup-bare", "p2o-sup-versioned"]);
    // A second open is a no-op.
    drop(backend);
    let backend = SqliteTerminologyBackend::new(path).unwrap();
    let total = CodeSystemOperations::count(
        &backend,
        &TenantContext::system(),
        supplements_query("http://hts.test/p2o/cs/base"),
    )
    .await
    .unwrap();
    assert_eq!(total, 2);
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn postgres_existing_rows_become_searchable_after_upgrade() {
    use helios_hts::backends::PostgresTerminologyBackend;
    let url = common::fresh_pg_database().await;
    {
        let backend = PostgresTerminologyBackend::new(&url).await.unwrap();
        backend
            .import_bundle(&TenantContext::system(), BUNDLE.as_bytes())
            .await
            .unwrap();
        backend.pool().close();
    }
    {
        let (client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        let driver = tokio::spawn(conn);
        client
            .batch_execute(
                "ALTER TABLE code_systems DROP COLUMN supplements;
                 ALTER TABLE code_systems DROP COLUMN publisher;
                 DELETE FROM hts_schema_meta WHERE key = 'schema_fingerprint';",
            )
            .await
            .unwrap();
        drop(client);
        let _ = driver.await;
    }
    let backend = PostgresTerminologyBackend::new(&url).await.unwrap();
    let found = CodeSystemOperations::search(
        &backend,
        &TenantContext::system(),
        supplements_query("http://hts.test/p2o/cs/base"),
    )
    .await
    .unwrap();
    assert_eq!(search_ids(&found), ["p2o-sup-bare", "p2o-sup-versioned"]);
    let total = CodeSystemOperations::count(
        &backend,
        &TenantContext::system(),
        supplements_query("http://hts.test/p2o/cs/base"),
    )
    .await
    .unwrap();
    assert_eq!(total, 2);
    backend.pool().close();
}
