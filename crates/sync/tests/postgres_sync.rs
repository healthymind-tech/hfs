//! PostgreSQL integration (feature `postgres`): the `PgSyncStore` passes the
//! store conformance suite; the set-based tenant copy (resources, history,
//! search index, package registry); storage-copy runs, duplication with
//! history, and two sync runtimes on two backend handles sharing one
//! database (cluster leasing, fencing, shared item state).
//!
//! Uses `HFS_TEST_PG_HOST` / `HFS_TEST_PG_PORT` when set (CI service
//! container), else a `postgres:16-alpine` testcontainer.
#![cfg(feature = "postgres")]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use helios_endpoints::pg_sync::PgSyncStore;
use helios_endpoints::sync::{SyncProgress, SyncRunState, SyncStore, SyncStoreError};
use helios_fhir::FhirVersion;
use helios_persistence::backends::postgres::{PostgresBackend, PostgresConfig, TenantCopyOptions};
use helios_persistence::core::{JobKind, ResourceStorage, WorkerId};
use helios_sync::copier::TenantCopier;
use helios_sync::local::{LocalStore, tenant_context};
use helios_sync::{SyncDeps, SyncService};
use serde_json::{Value, json};
use tokio::sync::{Mutex, OnceCell};

/// `claim_next` drains one shared queue: serialize the tests of this file.
static LOCK: Mutex<()> = Mutex::const_new(());

struct SharedPg {
    host: String,
    port: u16,
    _container: Option<testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>>,
}

static SHARED: OnceCell<SharedPg> = OnceCell::const_new();

fn pg_config(host: &str, port: u16) -> PostgresConfig {
    PostgresConfig {
        host: host.to_string(),
        port,
        dbname: "postgres".to_string(),
        user: "postgres".to_string(),
        password: Some("postgres".to_string()),
        max_connections: 8,
        data_dir: Some(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(|p| p.parent())
                .map(|p| p.join("data"))
                .unwrap_or_else(|| PathBuf::from("data")),
        ),
        ..Default::default()
    }
}

async fn shared() -> &'static SharedPg {
    SHARED
        .get_or_init(|| async {
            use testcontainers::ImageExt;
            use testcontainers::runners::AsyncRunner;
            let (host, port, container) = if let Ok(host) = std::env::var("HFS_TEST_PG_HOST") {
                let port = std::env::var("HFS_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(5432);
                (host, port, None)
            } else {
                let c = testcontainers_modules::postgres::Postgres::default()
                    .with_tag("16-alpine")
                    .with_label("github.run_id", std::env::var("GITHUB_RUN_ID").unwrap_or_default())
                    .start()
                    .await
                    .expect("postgres container");
                let port = c.get_host_port_ipv4(5432).await.unwrap();
                let host = c.get_host().await.unwrap().to_string();
                (host, port, Some(c))
            };
            PostgresBackend::new(pg_config(&host, port))
                .await
                .expect("backend")
                .init_schema()
                .await
                .expect("schema");
            SharedPg { host, port, _container: container }
        })
        .await
}

/// A fresh handle on the shared database (two calls = two instances).
async fn backend() -> Arc<PostgresBackend> {
    let s = shared().await;
    Arc::new(PostgresBackend::new(pg_config(&s.host, s.port)).await.expect("backend"))
}

fn uniq(prefix: &str) -> String {
    format!("{prefix}{}", &uuid::Uuid::new_v4().simple().to_string()[..10])
}

async fn put(b: &PostgresBackend, tenant: &str, rt: &str, id: &str, body: Value) {
    let mut r = body;
    r["resourceType"] = json!(rt);
    r["id"] = json!(id);
    ResourceStorage::create_or_update(b, &tenant_context(tenant), rt, id, r, FhirVersion::R4).await.unwrap();
}

async fn count(b: &PostgresBackend, table: &str, tenant: &str) -> i64 {
    let c = b.get_client().await.unwrap();
    c.query_one(&format!("SELECT COUNT(*) FROM {table} WHERE tenant_id = $1"), &[&tenant])
        .await
        .unwrap()
        .get(0)
}

/// One instance's sync service over a backend handle.
async fn instance(
    b: Arc<PostgresBackend>,
    endpoints: Arc<dyn helios_endpoints::store::EndpointStore>,
    registry: Arc<helios_endpoints::registry::EndpointRegistry>,
    name: &str,
) -> Arc<SyncService> {
    let (svc, _) = helios_sync::build(
        &common::config(&[]),
        SyncDeps {
            store: Arc::new(PgSyncStore::new(b.shared_pool())),
            cluster: b.cluster_job_store().expect("cluster jobs"),
            registry,
            endpoints: Some(endpoints),
            local: Some(b.clone() as Arc<dyn LocalStore>),
            copier: Some(b.clone() as Arc<dyn TenantCopier>),
            key_ring: Some(common::key_ring()),
            observer: None,
            audit: None,
            instance: name.into(),
        },
    );
    svc
}

async fn drain(svc: &SyncService, worker: &str) -> usize {
    let w = WorkerId::new(worker);
    let mut n = 0;
    while svc.runtime().work_next(&w).await.unwrap() {
        n += 1;
    }
    n
}

#[tokio::test]
async fn postgres_integration_sync_store_conformance() {
    let _g = LOCK.lock().await;
    let b = backend().await;
    helios_sync::memory::conformance::run_all(&PgSyncStore::new(b.shared_pool())).await;
}

#[tokio::test]
async fn postgres_integration_tenant_copy() {
    let _g = LOCK.lock().await;
    let b = backend().await;
    let (src, dst, dst2) = (uniq("cs"), uniq("cd"), uniq("ch"));
    for i in 0..10 {
        put(&b, &src, "Patient", &format!("p{i}"), json!({"name": [{"family": format!("Fam{i}")}], "gender": "female"})).await;
    }
    put(&b, &src, "Patient", "p0", json!({"name": [{"family": "Changed"}], "gender": "male"})).await;
    put(&b, &src, "Observation", "o1", json!({"status": "final", "code": {"coding": [{"system": "http://loinc.org", "code": "1-1"}]},
        "subject": {"reference": "Patient/p1"}})).await;
    ResourceStorage::delete(b.as_ref(), &tenant_context(&src), "Patient", "p9").await.unwrap();
    // A package registry row to carry along.
    let c = b.get_client().await.unwrap();
    c.batch_execute(
        "CREATE TABLE IF NOT EXISTS hfs_packages (tenant_id TEXT NOT NULL, name TEXT NOT NULL, version TEXT NOT NULL,
            status TEXT NOT NULL, installed_at TIMESTAMPTZ NOT NULL, document JSONB NOT NULL,
            PRIMARY KEY (tenant_id, name, version))",
    )
    .await
    .unwrap();
    c.execute(
        "INSERT INTO hfs_packages (tenant_id, name, version, status, installed_at, document) VALUES ($1, 'x.ig', '1.0.0', 'installed', NOW(), '{}')",
        &[&src],
    )
    .await
    .unwrap();

    let report = b
        .copy_tenant(&src, &dst, &TenantCopyOptions { copy_packages: true, require_empty_target: true, ..Default::default() })
        .await
        .unwrap();
    assert_eq!(report.resources(), 11, "{report:?}"); // 10 live + 1 tombstone
    assert_eq!(count(&b, "resources", &dst).await, count(&b, "resources", &src).await);
    assert_eq!(count(&b, "search_index", &dst).await, count(&b, "search_index", &src).await);
    assert_eq!(count(&b, "resource_history", &dst).await, 11, "current versions only");
    assert_eq!(count(&b, "hfs_packages", &dst).await, 1);
    // The copy serves reads and searches.
    let p0 = ResourceStorage::read(b.as_ref(), &tenant_context(&dst), "Patient", "p0").await.unwrap().unwrap();
    assert_eq!(p0.version_id(), "2");
    assert_eq!(p0.content()["name"][0]["family"], "Changed");
    let gone = ResourceStorage::read(b.as_ref(), &tenant_context(&dst), "Patient", "p9").await;
    assert!(!matches!(gone, Ok(Some(ref r)) if !r.is_deleted()), "tombstone copied");
    let males: i64 = c
        .query_one(
            "SELECT COUNT(DISTINCT resource_id) FROM search_index WHERE tenant_id = $1 AND resource_type = 'Patient'
               AND param_name = 'gender' AND value_token_code = 'male'",
            &[&dst],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(males, 1);
    // Isolation: the source is unchanged and the copy is independent.
    put(&b, &dst, "Patient", "p1", json!({"gender": "other"})).await;
    assert_eq!(ResourceStorage::read(b.as_ref(), &tenant_context(&src), "Patient", "p1").await.unwrap().unwrap().content()["gender"], "female");

    // History copy; non-empty targets are refused.
    let full = b
        .copy_tenant(&src, &dst2, &TenantCopyOptions { include_history: true, require_empty_target: true, ..Default::default() })
        .await
        .unwrap();
    assert_eq!(full.rows.get("resource_history").copied(), Some(count(&b, "resource_history", &src).await as u64));
    assert!(b.copy_tenant(&src, &dst, &TenantCopyOptions { require_empty_target: true, ..Default::default() }).await.is_err());
    assert!(b.copy_tenant(&src, &src, &TenantCopyOptions::default()).await.is_err());
    // Type filter.
    let only = uniq("co");
    let r = b
        .copy_tenant(&src, &only, &TenantCopyOptions { resource_types: Some(vec!["Observation".into()]), ..Default::default() })
        .await
        .unwrap();
    assert_eq!(r.resources(), 1);
}

#[tokio::test]
async fn postgres_integration_two_instances_storage_copy_incremental_and_fencing() {
    let _g = LOCK.lock().await;
    let (a, b) = (backend().await, backend().await);
    let (src, dst) = (uniq("s"), uniq("d"));
    let (endpoints, registry) = common::endpoints_with(&[&src, &dst]).await;
    for i in 0..6 {
        put(&a, &src, "Patient", &format!("p{i}"), json!({"active": true})).await;
    }
    let svc_a = instance(a.clone(), endpoints.clone(), registry.clone(), "inst-a").await;
    let svc_b = instance(b.clone(), endpoints.clone(), registry.clone(), "inst-b").await;

    // Created through A, run by B (the queue is shared).
    let job = svc_a
        .create(
            json!({"name": uniq("copy job "), "source": {"kind": "endpoint", "name": src},
                   "target": {"kind": "endpoint", "name": dst}, "initialLoad": "storage-copy", "batchSize": 2}),
            "t",
        )
        .await
        .unwrap()
        .job["id"]
        .as_str()
        .unwrap()
        .to_string();
    let run = svc_a.run_now(&job, "t").await.unwrap();
    assert_eq!(drain(&svc_b, "b-worker").await, 1);
    assert_eq!(drain(&svc_a, "a-worker").await, 0, "claimed exactly once");
    let done = svc_a.run(&run.id).await.unwrap();
    assert_eq!(done.state, SyncRunState::Succeeded, "{:?}", done.error);
    assert_eq!(done.progress.created, 6);
    assert_eq!(count(&a, "resources", &dst).await, 6);

    // Incremental through the other instance continues from B's cursor.
    tokio::time::sleep(Duration::from_millis(20)).await;
    put(&a, &src, "Patient", "p1", json!({"active": false})).await;
    put(&a, &src, "Patient", "p7", json!({})).await;
    let run2 = svc_b.run_now(&job, "t").await.unwrap();
    drain(&svc_a, "a-worker").await;
    let done2 = svc_b.run(&run2.id).await.unwrap();
    assert_eq!(done2.state, SyncRunState::Succeeded, "{:?}", done2.error);
    assert_eq!((done2.progress.created, done2.progress.updated), (1, 1), "{:?}", done2.progress);
    let p1 = ResourceStorage::read(a.as_ref(), &tenant_context(&dst), "Patient", "p1").await.unwrap().unwrap();
    assert_eq!(p1.content()["active"], json!(false));

    // Fencing across instances: A takes a short lease and stalls; B reclaims.
    let run3 = svc_a.run_now(&job, "t").await.unwrap();
    let cluster_a = a.cluster_job_store().unwrap();
    let (lease_a, _) = cluster_a
        .claim_next(JobKind::EndpointSync, &WorkerId::new("stalled"), Duration::from_millis(200))
        .await
        .unwrap()
        .unwrap();
    let store_a = PgSyncStore::new(a.shared_pool());
    let token_a = lease_a.fencing_token as i64;
    store_a.claim_run(&run3.id, lease_a.job_id.as_str(), token_a).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert_eq!(drain(&svc_b, "b-worker").await, 1, "expired lease reclaimed");
    let r3 = svc_b.run(&run3.id).await.unwrap();
    assert_eq!(r3.state, SyncRunState::Succeeded, "{:?}", r3.error);
    assert!(matches!(
        store_a.checkpoint(&run3.id, token_a, SyncRunState::Running, &r3.cursor, &SyncProgress::default(), None).await,
        Err(SyncStoreError::LeaseLost)
    ));
    svc_a.delete(&job, "t").await.unwrap();
}

#[tokio::test]
async fn postgres_integration_duplicate_with_history() {
    let _g = LOCK.lock().await;
    let b = backend().await;
    let src = uniq("e");
    let (endpoints, registry) = common::endpoints_with(&[&src]).await;
    put(&b, &src, "Patient", "p1", json!({"active": true})).await;
    put(&b, &src, "Patient", "p1", json!({"active": false})).await;
    let svc = instance(b.clone(), endpoints.clone(), registry, "inst").await;
    let copy = uniq("f");
    let out = svc
        .duplicate(
            &src,
            serde_json::from_value(json!({"name": copy, "data": "history"})).unwrap(),
            &helios_endpoints::store::Actor::bootstrap(),
        )
        .await
        .unwrap();
    assert_eq!(drain(&svc, "w").await, 1);
    let run = svc.run(out.run_id.as_deref().unwrap()).await.unwrap();
    assert_eq!(run.state, SyncRunState::Succeeded, "{:?}", run.error);
    assert_eq!(count(&b, "resource_history", &copy).await, 2, "history copied");
    let ep = endpoints.get(&copy).await.unwrap().unwrap();
    assert_eq!(ep.spec.status, helios_endpoints::model::EndpointStatus::Enabled);
    let log = svc.log(&run.id, 0, 50).await.unwrap();
    assert!(log.iter().any(|l| l.message.contains("storage copy finished")));
}
