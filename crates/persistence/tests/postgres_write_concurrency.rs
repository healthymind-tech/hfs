//! PostgreSQL write-path concurrency and SearchParameter-overlay regressions.
//!
//! Requires Docker (testcontainers, PostgreSQL 16).
//!
//! Run with:
//! `cargo test -p helios-persistence --features postgres --test postgres_write_concurrency`

#![cfg(feature = "postgres")]

#[path = "common/container_cleanup.rs"]
mod container_cleanup;

use std::path::PathBuf;

use helios_fhir::FhirVersion;
use helios_persistence::backends::postgres::{PostgresBackend, PostgresConfig};
use helios_persistence::core::{
    ResourceStorage, Transaction, TransactionOptions, TransactionProvider,
};
use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
use serde_json::{Value, json};
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio::sync::OnceCell;

struct SharedPg {
    host: String,
    port: u16,
    _container: testcontainers::ContainerAsync<Postgres>,
}

static SHARED_PG: OnceCell<SharedPg> = OnceCell::const_new();

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("data"))
        .unwrap_or_else(|| PathBuf::from("data"))
}

async fn shared_pg() -> &'static SharedPg {
    SHARED_PG
        .get_or_init(|| async {
            let run_id = std::env::var("GITHUB_RUN_ID").unwrap_or_default();
            let container = container_cleanup::with_cleanup_label(
                Postgres::default()
                    .with_tag("16-alpine")
                    .with_label("github.run_id", &run_id),
            )
            .start()
            .await
            .expect("start PostgreSQL 16 testcontainer");
            let pg = SharedPg {
                host: container.get_host().await.expect("host").to_string(),
                port: container.get_host_port_ipv4(5432).await.expect("port"),
                _container: container,
            };
            backend_at(&pg, 4)
                .await
                .init_schema()
                .await
                .expect("initialize schema");
            pg
        })
        .await
}

async fn backend_at(pg: &SharedPg, max_connections: usize) -> PostgresBackend {
    PostgresBackend::new(PostgresConfig {
        host: pg.host.clone(),
        port: pg.port,
        dbname: "postgres".into(),
        user: "postgres".into(),
        password: Some("postgres".into()),
        max_connections,
        data_dir: Some(data_dir()),
        ..Default::default()
    })
    .await
    .expect("backend")
}

/// A backend on the shared database. Every call is a separate instance with
/// its own pool and caches, standing in for a separate HFS process.
async fn backend(max_connections: usize) -> PostgresBackend {
    let backend = backend_at(shared_pg().await, max_connections).await;
    // Schema initialization also reads the layout marker this instance uses.
    backend.init_schema().await.expect("init schema");
    backend
}

fn tenant(prefix: &str) -> TenantContext {
    TenantContext::new(
        TenantId::new(format!("{prefix}_{}", uuid::Uuid::new_v4().simple())),
        TenantPermissions::full_access(),
    )
}

fn custom_search_parameter(code: &str) -> Value {
    json!({
        "resourceType": "SearchParameter",
        "id": code,
        "url": format!("http://example.org/fhir/SearchParameter/{code}"),
        "name": code,
        "status": "active",
        "code": code,
        "base": ["Patient"],
        "type": "string",
        "expression": "Patient.name.given",
        "description": "stream G overlay probe"
    })
}

async fn index_rows(
    backend: &PostgresBackend,
    tenant: &TenantContext,
    id: &str,
    param: &str,
) -> i64 {
    backend
        .get_client()
        .await
        .unwrap()
        .query_one(
            "SELECT COUNT(*) FROM search_index
             WHERE tenant_id = $1 AND resource_type = 'Patient' AND resource_id = $2
               AND param_name = $3",
            &[&tenant.tenant_id().as_str(), &id, &param],
        )
        .await
        .unwrap()
        .get(0)
}

async fn create_patient(backend: &PostgresBackend, tenant: &TenantContext, id: &str) {
    backend
        .create(
            tenant,
            "Patient",
            json!({"resourceType": "Patient", "id": id, "name": [{"family": "Probe", "given": ["Gee"]}]}),
            FhirVersion::default(),
        )
        .await
        .unwrap();
}

/// A write must index with the persisted overlay even when another process
/// changed it. The writer below has already cached the tenant's registry
/// (its first create), then a second instance stores a custom
/// SearchParameter; the first instance's next create must index it.
#[tokio::test]
async fn cached_write_registry_follows_other_instance_search_parameter() {
    let writer = backend(4).await;
    let other = backend(4).await;
    let tenant = tenant("spepoch");
    let code = format!("g{}", uuid::Uuid::new_v4().simple());

    create_patient(&writer, &tenant, "before").await;
    assert_eq!(index_rows(&writer, &tenant, "before", &code).await, 0);

    other
        .create(
            &tenant,
            "SearchParameter",
            custom_search_parameter(&code),
            FhirVersion::default(),
        )
        .await
        .unwrap();

    create_patient(&writer, &tenant, "after").await;
    assert_eq!(
        index_rows(&writer, &tenant, "after", &code).await,
        1,
        "the cached registry must be refreshed by the other instance's SearchParameter"
    );

    // And deleting it on the other instance stops the writer indexing it.
    other
        .delete(&tenant, "SearchParameter", &code)
        .await
        .unwrap();
    create_patient(&writer, &tenant, "deleted").await;
    assert_eq!(index_rows(&writer, &tenant, "deleted", &code).await, 0);
}

/// A transaction that writes a SearchParameter reads its own uncommitted
/// overlay. That registry must not reach the shared cache: once the
/// transaction rolls back, later writes must not index the parameter.
#[tokio::test]
async fn rolled_back_search_parameter_does_not_poison_write_cache() {
    let backend = backend(4).await;
    let tenant = tenant("sprollback");
    let code = format!("g{}", uuid::Uuid::new_v4().simple());

    create_patient(&backend, &tenant, "warm").await;

    let mut tx = backend
        .begin_transaction(&tenant, TransactionOptions::new())
        .await
        .unwrap();
    tx.create("SearchParameter", custom_search_parameter(&code))
        .await
        .unwrap();
    tx.create(
        "Patient",
        json!({"resourceType": "Patient", "id": "inside", "name": [{"given": ["Gee"]}]}),
    )
    .await
    .unwrap();
    Box::new(tx).rollback().await.unwrap();

    create_patient(&backend, &tenant, "after").await;
    assert_eq!(
        index_rows(&backend, &tenant, "after", &code).await,
        0,
        "a rolled-back SearchParameter must not stay in the write cache"
    );
}
