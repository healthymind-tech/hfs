//! `_summary=count` / `_total=accurate` from `search_index` alone (fork
//! migration `hm-0040-search-index-liveness`, `index_liveness.rs`): the count
//! must equal the `resources`-joined count after every kind of write, per
//! tenant, and must stay on the joined form while orphan index rows exist.
//!
//! Runs against an existing PostgreSQL named by `HFS_TEST_PG_HOST` /
//! `HFS_TEST_PG_PORT` (user, password `postgres`), each test in a database of
//! its own; without those variables the tests are skipped (HealthyMind CI sets
//! them, see docs/ops/ci.md).

#![cfg(feature = "postgres")]

use std::path::PathBuf;
use std::time::Duration;

use helios_fhir::FhirVersion;
use helios_persistence::backends::postgres::{PostgresBackend, PostgresConfig};
use helios_persistence::core::{
    PurgableStorage, ResourceStorage, SearchProvider, Transaction, TransactionOptions,
    TransactionProvider,
};
use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
use helios_persistence::types::{
    SearchParamType, SearchParameter, SearchQuery, SearchValue, TotalMode,
};
use serde_json::json;

/// A backend over a fresh database of its own.
async fn backend(name: &str) -> Option<PostgresBackend> {
    let host = std::env::var("HFS_TEST_PG_HOST").ok()?;
    let port: u16 = std::env::var("HFS_TEST_PG_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5432);
    let admin = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let db = format!("hm_count_{}_{name}", std::process::id());
    let (client, conn) = tokio_postgres::connect(&admin, tokio_postgres::NoTls)
        .await
        .expect("connect to HFS_TEST_PG_HOST");
    let driver = tokio::spawn(conn);
    for sql in [
        format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"),
        format!("CREATE DATABASE {db}"),
    ] {
        client
            .batch_execute(&sql)
            .await
            .expect("create test database");
    }
    drop(client);
    let _ = driver.await;
    let data_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data");
    let backend = PostgresBackend::new(PostgresConfig {
        host,
        port,
        dbname: db,
        user: "postgres".into(),
        password: Some("postgres".into()),
        max_connections: 4,
        data_dir: Some(data_dir),
        ..Default::default()
    })
    .await
    .expect("backend");
    backend.init_schema().await.expect("schema");
    Some(backend)
}

macro_rules! backend_or_skip {
    ($name:expr) => {
        match backend($name).await {
            Some(b) => b,
            None => {
                eprintln!("skipped: HFS_TEST_PG_HOST is not set");
                return;
            }
        }
    };
}

fn tenant(id: &str) -> TenantContext {
    TenantContext::new(TenantId::new(id), TenantPermissions::full_access())
}

fn observation(id: &str, code: &str) -> serde_json::Value {
    json!({
        "resourceType": "Observation",
        "id": id,
        "status": "final",
        "code": {"coding": [{"system": "http://loinc.org", "code": code}]},
    })
}

fn code_query(code: &str) -> SearchQuery {
    SearchQuery::new("Observation").with_parameter(SearchParameter {
        name: "code".to_string(),
        param_type: SearchParamType::Token,
        modifier: None,
        values: vec![SearchValue::eq(code)],
        chain: vec![],
        components: vec![],
    })
}

async fn put(backend: &PostgresBackend, tenant: &TenantContext, id: &str, code: &str) {
    backend
        .create_or_update(
            tenant,
            "Observation",
            id,
            observation(id, code),
            FhirVersion::default(),
        )
        .await
        .expect("put");
}

/// The count the general statement gives: live resources that have a matching
/// index row.
async fn joined_count(backend: &PostgresBackend, tenant: &TenantContext, code: &str) -> u64 {
    let client = backend.get_client().await.unwrap();
    let n: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM resources r \
             WHERE r.tenant_id = $1 AND r.resource_type = 'Observation' AND r.is_deleted = FALSE \
               AND r.id IN (SELECT resource_id FROM search_index \
                            WHERE tenant_id = $1 AND resource_type = 'Observation' \
                              AND param_name = 'code' AND value_token_code = $2)",
            &[&tenant.tenant_id().as_str(), &code],
        )
        .await
        .unwrap()
        .get(0);
    n as u64
}

/// Counts this backend answered from `search_index` alone.
async fn fast_count_statements(backend: &PostgresBackend) -> u64 {
    backend.index_only_count_total()
}

/// `search_count`, `_total=accurate` and the joined count agree, and equal
/// `expected`.
async fn assert_counts(
    backend: &PostgresBackend,
    tenant: &TenantContext,
    code: &str,
    expected: u64,
    step: &str,
) {
    let counted = backend
        .search_count(tenant, &code_query(code))
        .await
        .unwrap();
    let mut with_total = code_query(code);
    with_total.total = Some(TotalMode::Accurate);
    let total = backend.search(tenant, &with_total).await.unwrap().total;
    let joined = joined_count(backend, tenant, code).await;
    assert_eq!(
        (counted, total, joined),
        (expected, Some(expected), expected),
        "{step}: tenant {} code {code}",
        tenant.tenant_id().as_str()
    );
}

async fn verify(backend: &PostgresBackend, tenant: &TenantContext) -> u64 {
    backend
        .verify_search_index_liveness(tenant.tenant_id().as_str(), "Observation")
        .await
        .unwrap()
}

#[tokio::test]
async fn index_count_equals_joined_count_after_every_kind_of_write() {
    let backend = backend_or_skip!("writes");
    let (t1, t2) = (tenant("count-one"), tenant("count-two"));
    for n in 0..30 {
        put(&backend, &t1, &format!("a{n}"), "AAA").await;
    }
    for n in 0..10 {
        put(&backend, &t1, &format!("b{n}"), "BBB").await;
    }
    for n in 0..7 {
        // Same ids as the first tenant: tenants must not see each other.
        put(&backend, &t2, &format!("a{n}"), "AAA").await;
    }
    assert_eq!(verify(&backend, &t1).await, 0);
    assert_eq!(verify(&backend, &t2).await, 0);
    assert_eq!(fast_count_statements(&backend).await, 0);

    assert_counts(&backend, &t1, "AAA", 30, "initial").await;
    assert_counts(&backend, &t1, "BBB", 10, "initial").await;
    assert_counts(&backend, &t2, "AAA", 7, "initial").await;
    assert_counts(&backend, &t2, "BBB", 0, "initial").await;
    assert!(
        fast_count_statements(&backend).await >= 1,
        "a verified type is counted from search_index alone"
    );

    // Update: the old version's value lives on in history only.
    put(&backend, &t1, "a0", "BBB").await;
    assert_counts(&backend, &t1, "AAA", 29, "update").await;
    assert_counts(&backend, &t1, "BBB", 11, "update").await;

    // Several versions of one resource are still one match.
    put(&backend, &t1, "a1", "AAA").await;
    put(&backend, &t1, "a1", "AAA").await;
    assert_counts(&backend, &t1, "AAA", 29, "re-put").await;

    // Soft delete.
    backend.delete(&t1, "Observation", "a2").await.unwrap();
    assert_counts(&backend, &t1, "AAA", 28, "delete").await;

    // PUT to a deleted id brings it back, once.
    put(&backend, &t1, "a2", "AAA").await;
    assert_counts(&backend, &t1, "AAA", 29, "undelete").await;

    // Transaction: create, update and delete together.
    let mut tx = backend
        .begin_transaction(&t1, TransactionOptions::new())
        .await
        .unwrap();
    tx.create("Observation", observation("tx-new", "AAA"))
        .await
        .unwrap();
    tx.delete("Observation", "a3").await.unwrap();
    tx.delete("Observation", "b0").await.unwrap();
    Box::new(tx).commit().await.unwrap();
    assert_counts(&backend, &t1, "AAA", 29, "transaction").await;
    assert_counts(&backend, &t1, "BBB", 10, "transaction").await;

    // A rolled-back transaction changes nothing.
    let mut tx = backend
        .begin_transaction(&t1, TransactionOptions::new())
        .await
        .unwrap();
    tx.create("Observation", observation("tx-gone", "AAA"))
        .await
        .unwrap();
    tx.delete("Observation", "a4").await.unwrap();
    Box::new(tx).rollback().await.unwrap();
    assert_counts(&backend, &t1, "AAA", 29, "rollback").await;

    // Purge of one resource, then of a deleted one.
    backend.purge(&t1, "Observation", "a5").await.unwrap();
    assert_counts(&backend, &t1, "AAA", 28, "purge").await;
    backend.delete(&t1, "Observation", "a6").await.unwrap();
    backend.purge(&t1, "Observation", "a6").await.unwrap();
    assert_counts(&backend, &t1, "AAA", 27, "purge deleted").await;

    // The other tenant never moved.
    assert_counts(&backend, &t2, "AAA", 7, "other tenant").await;
    backend.delete(&t2, "Observation", "a0").await.unwrap();
    assert_counts(&backend, &t2, "AAA", 6, "other tenant delete").await;
    assert_counts(
        &backend,
        &t1,
        "AAA",
        27,
        "first tenant after the other's delete",
    )
    .await;

    // Purging a whole type empties it.
    backend.purge_all(&t2, "Observation").await.unwrap();
    assert_counts(&backend, &t2, "AAA", 0, "purge_all").await;

    // Nothing above left an index row behind a dead resource.
    assert_eq!(verify(&backend, &t1).await, 0);
    assert_eq!(verify(&backend, &t2).await, 0);
}

#[tokio::test]
async fn orphan_index_rows_keep_the_joined_count_until_removed() {
    let backend = backend_or_skip!("orphans");
    let t = tenant("count-orphans");
    for n in 0..5 {
        put(&backend, &t, &format!("o{n}"), "AAA").await;
    }
    // Rows no write path creates: an index row for a resource that is not
    // there (a partial restore, hand-written SQL).
    {
        let client = backend.get_client().await.unwrap();
        client
            .execute(
                "INSERT INTO search_index \
                   (tenant_id, resource_type, resource_id, param_name, value_token_system, \
                    value_token_code, last_updated) \
                 VALUES ($1, 'Observation', 'ghost', 'code', 'http://loinc.org', 'AAA', now())",
                &[&t.tenant_id().as_str()],
            )
            .await
            .unwrap();
    }
    assert_eq!(verify(&backend, &t).await, 1);
    assert_counts(&backend, &t, "AAA", 5, "with an orphan row").await;
    assert_eq!(
        fast_count_statements(&backend).await,
        0,
        "a type with orphan rows is never counted from the index alone"
    );

    {
        let client = backend.get_client().await.unwrap();
        client
            .execute(
                "DELETE FROM search_index WHERE tenant_id = $1 AND resource_id = 'ghost'",
                &[&t.tenant_id().as_str()],
            )
            .await
            .unwrap();
    }
    assert_eq!(verify(&backend, &t).await, 0);
    assert_counts(&backend, &t, "AAA", 5, "orphan removed").await;
    assert!(fast_count_statements(&backend).await >= 1);
}

#[tokio::test]
async fn an_unverified_type_is_checked_in_the_background() {
    let backend = backend_or_skip!("lazy");
    let t = tenant("count-lazy");
    for n in 0..4 {
        put(&backend, &t, &format!("l{n}"), "AAA").await;
    }
    // The first count has no verified fact: exact, through `resources`.
    assert_counts(&backend, &t, "AAA", 4, "before verification").await;
    // It started the check; wait for the marker.
    let mut recorded = false;
    for _ in 0..100 {
        let client = backend.get_client().await.unwrap();
        recorded = client
            .query_opt(
                "SELECT 1 FROM hfs_search_index_liveness \
                 WHERE tenant_id = $1 AND resource_type = 'Observation' AND orphan_resources = 0",
                &[&t.tenant_id().as_str()],
            )
            .await
            .unwrap()
            .is_some();
        drop(client);
        if recorded {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(recorded, "the background check records a clean type");
    // The cache is filled just after the marker commits.
    let mut fast = 0;
    for _ in 0..50 {
        assert_counts(&backend, &t, "AAA", 4, "after verification").await;
        fast = fast_count_statements(&backend).await;
        if fast > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(fast >= 1, "later counts use the index alone");
}

#[tokio::test]
async fn deleting_a_resource_row_removes_its_index_rows() {
    let backend = backend_or_skip!("triggers");
    let t = tenant("count-triggers");
    let tenant_id = t.tenant_id().as_str().to_string();
    for id in ["soft", "hard", "kept"] {
        put(&backend, &t, id, "AAA").await;
    }
    assert_eq!(verify(&backend, &t).await, 0);
    let client = backend.get_client().await.unwrap();
    let rows_of = |id: &'static str| {
        let client = &client;
        let tenant_id = tenant_id.clone();
        async move {
            client
                .query_one(
                    "SELECT COUNT(*) FROM search_index \
                     WHERE tenant_id = $1 AND resource_type = 'Observation' AND resource_id = $2",
                    &[&tenant_id, &id],
                )
                .await
                .unwrap()
                .get::<_, i64>(0)
        }
    };
    assert!(rows_of("soft").await > 0 && rows_of("hard").await > 0);

    // Maintenance SQL that knows nothing about search_index.
    client
        .execute(
            "UPDATE resources SET is_deleted = TRUE WHERE tenant_id = $1 AND id = 'soft'",
            &[&tenant_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "DELETE FROM resources WHERE tenant_id = $1 AND id = 'hard'",
            &[&tenant_id],
        )
        .await
        .unwrap();
    assert_eq!(rows_of("soft").await, 0);
    assert_eq!(rows_of("hard").await, 0);
    assert!(rows_of("kept").await > 0);
    // An update that does not delete leaves the rows alone.
    client
        .execute(
            "UPDATE resources SET is_deleted = FALSE, last_updated = now() \
             WHERE tenant_id = $1 AND id = 'kept'",
            &[&tenant_id],
        )
        .await
        .unwrap();
    assert!(rows_of("kept").await > 0);
    drop(client);
    assert_counts(&backend, &t, "AAA", 1, "after raw deletes").await;
    assert_eq!(verify(&backend, &t).await, 0);

    // TRUNCATE has no rows to follow: every type must be verified again.
    let client = backend.get_client().await.unwrap();
    client.batch_execute("TRUNCATE resources").await.unwrap();
    let markers: i64 = client
        .query_one("SELECT COUNT(*) FROM hfs_search_index_liveness", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(markers, 0);
}
