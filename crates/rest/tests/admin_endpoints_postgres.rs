//! The endpoint admin API suite over PostgreSQL: the endpoint configuration
//! lives in `hfs_endpoints` / `hfs_endpoint_revisions` through
//! `helios_endpoints::pg::PgEndpointStore` (CAS revisions, epochs, history),
//! and the package registry (per tenant), tenant statistics, AuditEvent
//! search and tenant purge run on the same database (one fresh database per
//! test).
//!
//! PostgreSQL: `HFS_TEST_PG_HOST`/`HFS_TEST_PG_PORT` (user, password and
//! database `postgres`), as in HealthyMind CI; otherwise a `postgres:16`
//! testcontainer.

#![cfg(feature = "postgres")]

#[path = "admin_endpoints_suite/mod.rs"]
mod suite;

#[path = "common/container_cleanup.rs"]
mod container_cleanup;

use std::sync::Arc;

use helios_endpoints::pg::PgEndpointStore;
use helios_endpoints::{EndpointStore, SecretKeyRing};
use helios_persistence::backends::postgres::{PostgresBackend, PostgresConfig};
use suite::{FakeDuplicate, Harness, Options};
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio::sync::OnceCell;

struct SharedPg {
    host: String,
    port: u16,
    _container: Option<testcontainers::ContainerAsync<Postgres>>,
}

static SHARED_PG: OnceCell<SharedPg> = OnceCell::const_new();

async fn shared_pg() -> &'static SharedPg {
    SHARED_PG
        .get_or_init(|| async {
            if std::env::var("HFS_TEST_PG_HOST").is_ok()
                || std::env::var("HFS_TEST_PG_PORT").is_ok()
            {
                return SharedPg {
                    host: std::env::var("HFS_TEST_PG_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
                    port: std::env::var("HFS_TEST_PG_PORT")
                        .ok()
                        .map(|p| p.parse().expect("HFS_TEST_PG_PORT must be a TCP port"))
                        .unwrap_or(5432),
                    _container: None,
                };
            }
            let run_id = std::env::var("GITHUB_RUN_ID").unwrap_or_default();
            let container = container_cleanup::with_cleanup_label(
                Postgres::default()
                    .with_tag("16-alpine")
                    .with_label("github.run_id", &run_id),
            )
            .start()
            .await
            .expect("failed to start PostgreSQL container");
            SharedPg {
                host: container.get_host().await.expect("host").to_string(),
                port: container.get_host_port_ipv4(5432).await.expect("port"),
                _container: Some(container),
            }
        })
        .await
}

fn pg_config(pg: &SharedPg, dbname: &str) -> PostgresConfig {
    PostgresConfig {
        host: pg.host.clone(),
        port: pg.port,
        dbname: dbname.to_string(),
        user: "postgres".to_string(),
        password: Some("postgres".to_string()),
        max_connections: 4,
        data_dir: Some(suite::data_dir()),
        ..Default::default()
    }
}

/// A fresh, schema-initialized database.
async fn postgres() -> Arc<PostgresBackend> {
    let pg = shared_pg().await;
    let dbname = format!("hm_p4_{}", uuid::Uuid::new_v4().simple());
    PostgresBackend::new(pg_config(pg, "postgres"))
        .await
        .expect("connect to postgres")
        .get_client()
        .await
        .expect("checkout")
        .batch_execute(&format!("CREATE DATABASE {dbname}"))
        .await
        .expect("create database");
    let backend = PostgresBackend::new(pg_config(pg, &dbname))
        .await
        .expect("connect");
    backend.init_schema().await.expect("init schema");
    Arc::new(backend)
}

async fn harness(opts: Options) -> Harness {
    let backend = postgres().await;
    let store: Arc<dyn EndpointStore> = Arc::new(PgEndpointStore::new(backend.shared_pool()));
    Harness::new(backend, store, opts).await
}

#[tokio::test]
async fn postgres_integration_admin_endpoints_crud_lifecycle() {
    suite::crud_lifecycle(&harness(Options::default()).await).await;
}

#[tokio::test]
async fn postgres_integration_admin_endpoints_validation_and_conflicts() {
    suite::validation_and_conflicts(&harness(Options::default()).await).await;
}

#[tokio::test]
async fn postgres_integration_admin_endpoints_secrets() {
    suite::secrets(&harness(Options::default()).await).await;
    let old = SecretKeyRing::generate_spec("old");
    let ring =
        SecretKeyRing::parse(&format!("{},{old}", SecretKeyRing::generate_spec("new"))).unwrap();
    suite::rewrap_rotation(
        &harness(Options {
            key_ring: Some(Arc::new(ring)),
            ..Options::default()
        })
        .await,
        &old,
    )
    .await;
}

#[tokio::test]
async fn postgres_integration_admin_endpoints_packages_per_tenant() {
    suite::packages(&harness(Options::default()).await).await;
}

#[tokio::test]
async fn postgres_integration_admin_endpoints_stats_audit_purge() {
    suite::stats_audit_purge(&harness(Options::default()).await).await;
}

#[tokio::test]
async fn postgres_integration_admin_endpoints_env_managed_and_race() {
    suite::env_managed(&harness(Options::default()).await).await;
    suite::store_race(
        &harness(Options {
            racy: true,
            ..Options::default()
        })
        .await,
    )
    .await;
}

#[tokio::test]
async fn postgres_integration_admin_endpoints_duplicate_handoff() {
    let fake = Arc::new(FakeDuplicate::default());
    let h = harness(Options {
        duplicate: Some(fake.clone()),
        ..Options::default()
    })
    .await;
    suite::duplicate_handoff(&h, &fake).await;
}
