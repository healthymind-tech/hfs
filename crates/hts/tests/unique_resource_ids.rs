//! Every stored version of a CodeSystem / ValueSet reports its own logical id.
//!
//! Production reproduction: LOINC 2.83 imported (admin job, `format: loinc`)
//! on top of 2.80 left `GET /CodeSystem?url=http://loinc.org` listing two
//! resources with id `loinc`, and `GET /CodeSystem/loinc` ambiguous. Versions
//! sharing a declared id now report it on the default version only and
//! `<id>-<version>` on the others; the startup repair fixes databases that
//! already hold the duplicates, without touching concepts.
//!
//! Runs on SQLite by default and on PostgreSQL with
//! `HTS_TEST_BACKEND=postgres` (feature `postgres`).

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};

const URL: &str = "http://hts.test/unique-ids/loinc";

fn cs(version: &str) -> Value {
    json!({
        "resourceType": "CodeSystem", "id": "loinc", "url": URL,
        "version": version, "name": "LOINC", "status": "active", "content": "complete",
        "concept": [{"code": "1-1", "display": format!("Note {version}")}]
    })
}

/// The LOINC answer-list ValueSet shape: same id in every release.
fn vs(version: &str) -> Value {
    json!({
        "resourceType": "ValueSet", "id": "LL1-1", "url": "http://hts.test/unique-ids/vs/LL1-1",
        "version": version, "status": "active",
        "compose": {"include": [{"system": URL, "concept": [{"code": "1-1"}]}]}
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
    s.replace(':', "%3A").replace('/', "%2F")
}

/// `(id, version)` of every entry, sorted by version.
fn ids(body: &Value) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = body["entry"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| {
                    (
                        e["resource"]["id"].as_str().unwrap_or_default().to_string(),
                        e["resource"]["version"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

async fn search(app: &TestApp, path: &str) -> Vec<(String, String)> {
    let (status, body) = app.get_fhir(path).await;
    assert_eq!(status, StatusCode::OK, "{path}: {body}");
    ids(&body)
}

#[tokio::test]
async fn versions_sharing_an_id_report_unique_ids() {
    let app = TestApp::with_resource_store().await;
    // Older release imported last: the default does not depend on order.
    app.import_bundle_ok(&bundle(vec![cs("2.83"), vs("2.83")]))
        .await;
    app.import_bundle_ok(&bundle(vec![cs("2.80"), vs("2.80")]))
        .await;

    let want = pairs(&[("loinc-2.80", "2.80"), ("loinc", "2.83")]);
    for path in [
        format!("/CodeSystem?url={}", enc(URL)),
        format!("/CodeSystem?url={}&_summary=true", enc(URL)),
        format!("/CodeSystem?url={}&name=LOINC", enc(URL)),
        format!("/CodeSystem?url={}&_elements=id,version", enc(URL)),
    ] {
        assert_eq!(search(&app, &path).await, want, "{path}");
    }
    let vs_url = enc("http://hts.test/unique-ids/vs/LL1-1");
    assert_eq!(
        search(&app, &format!("/ValueSet?url={vs_url}")).await,
        pairs(&[("LL1-1-2.80", "2.80"), ("LL1-1", "2.83")])
    );
    assert_eq!(
        search(&app, &format!("/ValueSet?url={vs_url}&_summary=true")).await,
        pairs(&[("LL1-1-2.80", "2.80"), ("LL1-1", "2.83")])
    );

    // `_id` names one resource.
    assert_eq!(
        search(&app, "/CodeSystem?_id=loinc-2.80").await,
        pairs(&[("loinc-2.80", "2.80")])
    );
    assert_eq!(
        search(&app, "/CodeSystem?_id=loinc").await,
        pairs(&[("loinc", "2.83")])
    );

    // Reads are deterministic: the plain id is the default version, every
    // version is addressable, and `<id>-<version>` also names the default.
    for (path, id, version) in [
        ("/CodeSystem/loinc", "loinc", "2.83"),
        ("/CodeSystem/loinc-2.80", "loinc-2.80", "2.80"),
        ("/CodeSystem/loinc-2.83", "loinc-2.83", "2.83"),
        ("/ValueSet/LL1-1", "LL1-1", "2.83"),
        ("/ValueSet/LL1-1-2.80", "LL1-1-2.80", "2.80"),
    ] {
        let (status, body) = app.get_fhir(path).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert_eq!(body["id"], id, "{path}");
        assert_eq!(body["version"], version, "{path}");
    }
    let (status, _) = app.get_fhir("/CodeSystem/loinc-2.81").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn instance_operations_run_on_the_version_the_id_names() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&bundle(vec![
        cs("2.80"),
        cs("2.83"),
        vs("2.80"),
        vs("2.83"),
    ]))
    .await;

    for (id, display) in [
        ("loinc", "Note 2.83"),
        ("loinc-2.83", "Note 2.83"),
        ("loinc-2.80", "Note 2.80"),
    ] {
        let (status, body) = app
            .get_fhir(&format!("/CodeSystem/{id}/$lookup?code=1-1"))
            .await;
        assert_eq!(status, StatusCode::OK, "{id}: {body}");
        let got = body["parameter"]
            .as_array()
            .and_then(|a| a.iter().find(|p| p["name"] == "display"))
            .and_then(|p| p["valueString"].as_str());
        assert_eq!(got, Some(display), "{id}: {body}");
    }
    for (id, version) in [("LL1-1", "2.83"), ("LL1-1-2.80", "2.80")] {
        let (status, body) = app.get_fhir(&format!("/ValueSet/{id}/$expand")).await;
        assert_eq!(status, StatusCode::OK, "{id}: {body}");
        assert_eq!(body["version"], version, "{id}: {body}");
        assert_eq!(body["id"], id, "{id}: {body}");
    }
}

#[tokio::test]
async fn a_newer_version_takes_over_the_plain_id() {
    let app = TestApp::new().await;
    app.import_bundle_ok(&bundle(vec![cs("2.80")])).await;
    assert_eq!(
        search(&app, &format!("/CodeSystem?url={}", enc(URL))).await,
        pairs(&[("loinc", "2.80")])
    );
    app.import_bundle_ok(&bundle(vec![cs("2.83")])).await;
    assert_eq!(
        search(&app, &format!("/CodeSystem?url={}", enc(URL))).await,
        pairs(&[("loinc-2.80", "2.80"), ("loinc", "2.83")])
    );
    app.import_bundle_ok(&bundle(vec![cs("2.84")])).await;
    assert_eq!(
        search(&app, &format!("/CodeSystem?url={}", enc(URL))).await,
        pairs(&[
            ("loinc-2.80", "2.80"),
            ("loinc-2.83", "2.83"),
            ("loinc", "2.84")
        ])
    );
    // A re-import of an existing version changes nothing.
    app.import_bundle_ok(&bundle(vec![cs("2.80")])).await;
    assert_eq!(
        search(&app, &format!("/CodeSystem?url={}", enc(URL))).await,
        pairs(&[
            ("loinc-2.80", "2.80"),
            ("loinc-2.83", "2.83"),
            ("loinc", "2.84")
        ])
    );
}

#[tokio::test]
async fn deleting_by_a_unique_id_removes_only_that_version() {
    let app = TestApp::with_resource_store().await;
    app.import_bundle_ok(&bundle(vec![cs("2.80"), cs("2.83")]))
        .await;

    assert_eq!(
        app.delete_fhir("/CodeSystem/loinc-2.80").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        search(&app, &format!("/CodeSystem?url={}", enc(URL))).await,
        pairs(&[("loinc", "2.83")])
    );

    app.import_bundle_ok(&bundle(vec![cs("2.80")])).await;
    // Deleting the default version hands the plain id to the next one.
    assert_eq!(
        app.delete_fhir("/CodeSystem/loinc").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        search(&app, &format!("/CodeSystem?url={}", enc(URL))).await,
        pairs(&[("loinc", "2.80")])
    );
}

// ── Startup repair of databases that already hold duplicate ids ────────────────

/// Rewind the database to what an older binary left behind: no `fhir_id`
/// on one table, every version reporting the same id on the other.
const LEGACY_SQL: &str = "UPDATE code_systems SET fhir_id = 'loinc'; \
                          UPDATE value_sets SET fhir_id = NULL;";

fn repaired_ids(rows: Vec<(Option<String>, Option<String>)>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = rows
        .into_iter()
        .map(|(v, f)| (f.unwrap_or_default(), v.unwrap_or_default()))
        .collect();
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn startup_repairs_duplicate_ids_sqlite() {
    use helios_hts::backends::SqliteTerminologyBackend;
    use helios_hts::import::BundleImportBackend;
    use helios_persistence::tenant::TenantContext;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hts.db");
    let path = path.to_str().unwrap();
    let concepts = {
        let backend = SqliteTerminologyBackend::new(path).unwrap();
        backend
            .import_bundle(
                &TenantContext::system(),
                bundle(vec![cs("2.80"), cs("2.83"), vs("2.80"), vs("2.83")]).as_bytes(),
            )
            .await
            .unwrap();
        let conn = backend.pool().get().unwrap();
        conn.execute_batch(LEGACY_SQL).unwrap();
        conn.query_row("SELECT COUNT(*) FROM concepts", [], |r| r.get::<_, i64>(0))
            .unwrap()
    };

    let backend = SqliteTerminologyBackend::new(path).unwrap();
    let conn = backend.pool().get().unwrap();
    let rows = |table: &str| {
        let mut stmt = conn
            .prepare(&format!("SELECT version, fhir_id FROM {table}"))
            .unwrap();
        repaired_ids(
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect(),
        )
    };
    assert_eq!(
        rows("code_systems"),
        pairs(&[("loinc-2.80", "2.80"), ("loinc", "2.83")])
    );
    assert_eq!(
        rows("value_sets"),
        pairs(&[("LL1-1-2.80", "2.80"), ("LL1-1", "2.83")])
    );
    let after: i64 = conn
        .query_row("SELECT COUNT(*) FROM concepts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(after, concepts, "the repair must not touch concepts");
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn startup_repairs_duplicate_ids_postgres() {
    use helios_hts::backends::PostgresTerminologyBackend;
    use helios_hts::import::BundleImportBackend;
    use helios_persistence::tenant::TenantContext;

    if !common::pg_mode() {
        return;
    }
    let url = common::fresh_pg_database().await;
    let (client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    let concepts: i64 = {
        let backend = PostgresTerminologyBackend::new(&url).await.unwrap();
        backend
            .import_bundle(
                &TenantContext::system(),
                bundle(vec![cs("2.80"), cs("2.83"), vs("2.80"), vs("2.83")]).as_bytes(),
            )
            .await
            .unwrap();
        backend.pool().close();
        client.batch_execute(LEGACY_SQL).await.unwrap();
        client
            .query_one("SELECT COUNT(*) FROM concepts", &[])
            .await
            .unwrap()
            .get(0)
    };

    let _backend = PostgresTerminologyBackend::new(&url).await.unwrap();
    for (table, want) in [
        (
            "code_systems",
            pairs(&[("loinc-2.80", "2.80"), ("loinc", "2.83")]),
        ),
        (
            "value_sets",
            pairs(&[("LL1-1-2.80", "2.80"), ("LL1-1", "2.83")]),
        ),
    ] {
        let rows = client
            .query(&format!("SELECT version, fhir_id FROM {table}"), &[])
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.get(0), r.get(1)))
            .collect();
        assert_eq!(repaired_ids(rows), want, "{table}");
    }
    let after: i64 = client
        .query_one("SELECT COUNT(*) FROM concepts", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(after, concepts, "the repair must not touch concepts");
}
