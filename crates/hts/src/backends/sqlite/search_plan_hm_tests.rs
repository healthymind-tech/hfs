//! The exact-match resource searches read `<table>` through its url index.
//!
//! `$expand`, `$validate-code` and the resolvers look resources up by
//! canonical URL on every uncached request; with LOINC's answer lists loaded
//! `value_sets` holds thousands of rows, and a scan of it per lookup cost
//! about 12 ms.

use super::*;

/// The `detail` column of `EXPLAIN QUERY PLAN` for `sql`, one step per line.
fn plan(conn: &rusqlite::Connection, sql: &str, params: Vec<rusqlite::types::Value>) -> String {
    let mut statement = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap_or_else(|e| panic!("{e}: {sql}"));
    let steps: Vec<String> = statement
        .query_map(rusqlite::params_from_iter(params), |row| {
            row.get::<_, String>(3)
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    steps.join("\n")
}

fn by_url(version: Option<&str>) -> ResourceSearchQuery {
    ResourceSearchQuery {
        url: Some("http://hts.test/plan".into()),
        version: version.map(str::to_string),
        count: Some(1),
        ..Default::default()
    }
}

fn assert_reads_the_url_index(table: &str, plan: &str) {
    assert!(
        plan.contains(&format!("SEARCH {table} USING")) && plan.contains("url=?"),
        "{table}: expected an index search on url, got:\n{plan}"
    );
    assert!(
        !plan.contains(&format!("SCAN {table}")),
        "{table}: full scan:\n{plan}"
    );
}

#[test]
fn a_search_by_url_reads_the_url_index() {
    let backend = SqliteTerminologyBackend::in_memory().unwrap();
    let conn = backend.pool().get().unwrap();
    for table in ["value_sets", "code_systems", "concept_maps"] {
        for columns in [SEARCH_META_COLUMNS, SEARCH_FULL_COLUMNS] {
            for version in [None, Some("1.0")] {
                let (sql, params) = exact_search(table, columns, &by_url(version));
                assert_reads_the_url_index(table, &plan(&conn, &sql, params));
            }
        }
    }
}

#[test]
fn a_count_and_a_string_search_by_url_read_the_url_index() {
    let backend = SqliteTerminologyBackend::in_memory().unwrap();
    let conn = backend.pool().get().unwrap();
    for table in ["value_sets", "code_systems", "concept_maps"] {
        let filter = ExactFilter::url_version_status('?', &by_url(None));
        let params = || {
            filter
                .values
                .iter()
                .cloned()
                .map(rusqlite::types::Value::Text)
                .collect::<Vec<_>>()
        };
        let count = format!("SELECT COUNT(*) FROM {table} WHERE {}", filter.sql);
        assert_reads_the_url_index(table, &plan(&conn, &count, params()));
        let rows = format!(
            "SELECT id, url FROM {table} WHERE {} ORDER BY {}",
            filter.sql,
            crate::backends::search_order_by(table, &by_url(None))
        );
        assert_reads_the_url_index(table, &plan(&conn, &rows, params()));
    }
}

/// The statement binds exactly the values it names, then the page.
#[test]
fn the_parameters_follow_the_named_columns() {
    let query = ResourceSearchQuery {
        url: Some("http://hts.test/plan".into()),
        status: Some("active".into()),
        count: Some(7),
        offset: Some(3),
        ..Default::default()
    };
    let (sql, params) = exact_search("value_sets", SEARCH_META_COLUMNS, &query);
    assert!(sql.contains("WHERE url = ?1 AND status = ?2"), "{sql}");
    assert!(sql.contains("LIMIT ?3 OFFSET ?4"), "{sql}");
    assert!(!sql.contains("IS NULL OR"), "{sql}");
    use rusqlite::types::Value::{Integer, Text};
    assert_eq!(
        params,
        [
            Text("http://hts.test/plan".into()),
            Text("active".into()),
            Integer(7),
            Integer(3)
        ]
    );

    let (sql, params) = exact_search(
        "value_sets",
        SEARCH_META_COLUMNS,
        &ResourceSearchQuery::default(),
    );
    assert!(sql.contains("WHERE 1 = 1"), "{sql}");
    assert!(sql.contains("LIMIT ?1 OFFSET ?2"), "{sql}");
    assert_eq!(params, [Integer(20), Integer(0)]);
}

/// The rows and their order are what the fixed statement returned: a
/// version-less search by url lists the versions by precedence, and each
/// combination of conditions selects the same rows.
#[tokio::test]
async fn searches_return_the_matching_rows_in_precedence_order() {
    use crate::import::BundleImportBackend;
    use crate::traits::ValueSetOperations;
    let backend = SqliteTerminologyBackend::in_memory().unwrap();
    let vs = |id: &str, url: &str, version: &str, status: &str| {
        serde_json::json!({"resource": {"resourceType": "ValueSet", "id": id, "url": url,
            "version": version, "status": status, "name": format!("N{id}"),
            "compose": {"include": [{"system": "http://hts.test/plan/cs"}]}}})
    };
    let bundle = serde_json::json!({"resourceType": "Bundle", "type": "collection", "entry": [
        vs("a1", "http://hts.test/plan/a", "1.0", "active"),
        vs("a2", "http://hts.test/plan/a", "1.10", "draft"),
        vs("a3", "http://hts.test/plan/a", "1.9", "active"),
        vs("b1", "http://hts.test/plan/b", "1.0", "active"),
    ]});
    let ctx = TenantContext::system();
    backend
        .import_bundle(&ctx, bundle.to_string().as_bytes())
        .await
        .unwrap();
    let versions = |query: ResourceSearchQuery| {
        let backend = backend.clone();
        let ctx = ctx.clone();
        async move {
            ValueSetOperations::search(&backend, &ctx, query)
                .await
                .unwrap()
                .iter()
                .map(|r| {
                    format!(
                        "{}|{}",
                        r["url"].as_str().unwrap().rsplit('/').next().unwrap(),
                        r["version"].as_str().unwrap()
                    )
                })
                .collect::<Vec<_>>()
        }
    };
    let a = || Some("http://hts.test/plan/a".to_string());
    assert_eq!(
        versions(ResourceSearchQuery {
            url: a(),
            ..Default::default()
        })
        .await,
        ["a|1.10", "a|1.9", "a|1.0"]
    );
    assert_eq!(
        versions(ResourceSearchQuery {
            url: a(),
            count: Some(1),
            ..Default::default()
        })
        .await,
        ["a|1.10"]
    );
    assert_eq!(
        versions(ResourceSearchQuery {
            url: a(),
            version: Some("1.9".into()),
            ..Default::default()
        })
        .await,
        ["a|1.9"]
    );
    assert_eq!(
        versions(ResourceSearchQuery {
            url: a(),
            status: Some("active".into()),
            ..Default::default()
        })
        .await,
        ["a|1.9", "a|1.0"]
    );
    let mut active = versions(ResourceSearchQuery {
        status: Some("active".into()),
        ..Default::default()
    })
    .await;
    active.sort();
    assert_eq!(active, ["a|1.0", "a|1.9", "b|1.0"]);
    assert_eq!(
        versions(ResourceSearchQuery {
            url: Some("http://hts.test/plan/none".into()),
            ..Default::default()
        })
        .await,
        Vec::<String>::new()
    );
}
