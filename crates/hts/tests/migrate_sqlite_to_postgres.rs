//! `hts migrate`: an SQLite database copied into PostgreSQL answers like the
//! original.

#![cfg(all(feature = "sqlite", feature = "postgres"))]

mod common;

use helios_hts::backends::{PostgresTerminologyBackend, SqliteTerminologyBackend};
use helios_hts::import::BundleImportBackend;
use helios_hts::migrate::{MigrateOptions, migrate_sqlite_to_postgres};
use helios_hts::traits::{CodeSystemOperations, ConceptMapOperations, ValueSetOperations};
use helios_hts::types::{ExpandRequest, LookupRequest, TranslateRequest, ValidateCodeRequest};
use helios_persistence::ResourceStorage;
use helios_persistence::tenant::TenantContext;
use serde_json::json;

fn ctx() -> TenantContext {
    TenantContext::system()
}

fn fixture() -> serde_json::Value {
    json!({
        "resourceType": "Bundle", "type": "collection",
        "entry": [
            {"resource": {
                "resourceType": "CodeSystem", "id": "mig", "url": "http://mig.test/cs",
                "version": "1.0.0", "status": "active", "content": "complete",
                "concept": [{"code": "root", "display": "Root", "designation": [{"language": "de", "value": "Wurzel"}],
                             "concept": [{"code": "a", "display": "Alpha"}, {"code": "b", "display": "Beta",
                                          "property": [{"code": "colour", "valueCode": "red"}]}]}]
            }},
            {"resource": {
                "resourceType": "ValueSet", "id": "mig-vs", "url": "http://mig.test/vs",
                "status": "active", "compose": {"include": [{"system": "http://mig.test/cs",
                    "filter": [{"property": "concept", "op": "is-a", "value": "root"}]}]}
            }},
            {"resource": {
                "resourceType": "ConceptMap", "id": "mig-cm", "url": "http://mig.test/cm", "status": "active",
                "group": [{"source": "http://mig.test/cs", "target": "http://mig.test/t",
                           "element": [{"code": "a", "target": [{"code": "T-a", "equivalence": "equivalent"}]}]}]
            }}
        ]
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn migrated_database_answers_like_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("hts.db");
    let sqlite = SqliteTerminologyBackend::new(db.to_str().unwrap()).unwrap();
    sqlite
        .import_bundle(&ctx(), &serde_json::to_vec(&fixture()).unwrap())
        .await
        .unwrap();
    sqlite
        .import_refset_members(
            &ctx(),
            "http://mig.test/cs",
            Some("1.0.0"),
            vec![("rs".into(), "a".into())],
        )
        .await
        .unwrap();
    // One REST-created resource in the persistence store of the same file.
    let store =
        helios_persistence::backends::sqlite::SqliteBackend::open(db.to_str().unwrap()).unwrap();
    store.init_schema().unwrap();
    store
        .create(
            &ctx(),
            "CodeSystem",
            json!({"resourceType": "CodeSystem", "id": "rest-cs", "url": "http://mig.test/rest",
                   "status": "active", "content": "complete", "concept": [{"code": "x"}]}),
            helios_fhir::FhirVersion::default_enabled(),
        )
        .await
        .unwrap();
    // Expand once so the persistent expansion cache has rows to copy.
    sqlite
        .expand(
            &ctx(),
            ExpandRequest {
                url: Some("http://mig.test/vs".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    drop(store);

    let url = common::fresh_pg_database().await;
    let report = migrate_sqlite_to_postgres(&MigrateOptions {
        from: db.clone(),
        to: url.clone(),
        replace: false,
        skip_resources: false,
        jobs: 4,
    })
    .await
    .unwrap();
    assert!(report.mismatches().is_empty(), "{:?}", report.tables);
    assert_eq!(report.resources, 1);
    let rows = |t: &str| {
        report
            .tables
            .iter()
            .find(|r| r.table == t)
            .map(|r| r.copied_rows)
            .unwrap()
    };
    assert_eq!(rows("concepts"), 3);
    assert_eq!(rows("refset_members"), 1);
    assert!(rows("concept_closure") >= 5);

    // A second run refuses a populated target unless asked to replace it.
    let again = migrate_sqlite_to_postgres(&MigrateOptions {
        from: db.clone(),
        to: url.clone(),
        replace: false,
        skip_resources: true,
        jobs: 2,
    })
    .await;
    assert!(again.is_err());
    let replaced = migrate_sqlite_to_postgres(&MigrateOptions {
        from: db.clone(),
        to: url.clone(),
        replace: true,
        skip_resources: true,
        jobs: 2,
    })
    .await
    .unwrap();
    assert!(replaced.mismatches().is_empty());

    let pg = PostgresTerminologyBackend::new(&url).await.unwrap();
    let expand = ExpandRequest {
        url: Some("http://mig.test/vs".into()),
        ..Default::default()
    };
    let codes = |r: helios_hts::types::ExpandResponse| {
        r.contains.into_iter().map(|c| c.code).collect::<Vec<_>>()
    };
    assert_eq!(
        codes(sqlite.expand(&ctx(), expand.clone()).await.unwrap()),
        codes(pg.expand(&ctx(), expand).await.unwrap())
    );
    let ecl = ExpandRequest {
        url: Some("http://mig.test/cs?fhir_vs=refset/rs".into()),
        ..Default::default()
    };
    assert_eq!(codes(pg.expand(&ctx(), ecl).await.unwrap()), ["a"]);
    let lookup = LookupRequest {
        system: "http://mig.test/cs".into(),
        code: "root".into(),
        ..Default::default()
    };
    let (sl, pl) = (
        sqlite.lookup(&ctx(), lookup.clone()).await.unwrap(),
        pg.lookup(&ctx(), lookup).await.unwrap(),
    );
    assert_eq!(
        serde_json::to_value(&sl).unwrap(),
        serde_json::to_value(&pl).unwrap()
    );
    let vc = ValidateCodeRequest {
        url: Some("http://mig.test/vs".into()),
        system: Some("http://mig.test/cs".into()),
        code: "b".into(),
        ..Default::default()
    };
    assert!(
        ValueSetOperations::validate_code(&pg, &ctx(), vc)
            .await
            .unwrap()
            .result
    );
    let tr = pg
        .translate(
            &ctx(),
            TranslateRequest {
                url: Some("http://mig.test/cm".into()),
                system: Some("http://mig.test/cs".into()),
                code: "a".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(tr.matches[0].concept_code, "T-a");

    // The REST resource is in the PostgreSQL persistence store.
    let pstore =
        helios_persistence::backends::postgres::PostgresBackend::from_connection_string(&url)
            .await
            .unwrap();
    let read = pstore.read(&ctx(), "CodeSystem", "rest-cs").await.unwrap();
    assert!(read.is_some());
}
