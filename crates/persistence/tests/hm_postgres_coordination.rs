//! PostgreSQL package-job coordination (`hfs_package_jobs` + the advisory
//! install lock) — the cluster side of the IG package manager.
//!
//! Runs against an existing PostgreSQL named by `HFS_TEST_PG_HOST` /
//! `HFS_TEST_PG_PORT` (user, password `postgres`), in a database of its own
//! created for this run; without those variables the tests are skipped
//! (HealthyMind CI sets them, see docs/ops/ci.md).

#![cfg(feature = "postgres")]

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use helios_persistence::backends::postgres::PostgresBackend;
use helios_persistence::core::ResourceStorage;
use helios_persistence::core::package_jobs::PackageJobRecord;
use serde_json::json;

/// A backend over a fresh database of its own. Not shared between tests:
/// each `#[tokio::test]` has its own runtime, and pooled connections die
/// with the runtime that opened them.
async fn backend(name: &str) -> Option<Arc<PostgresBackend>> {
    let host = std::env::var("HFS_TEST_PG_HOST").ok()?;
    let port: u16 = std::env::var("HFS_TEST_PG_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5432);
    let admin = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let db = format!("hm_coord_{}_{name}", std::process::id());
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
    let backend = PostgresBackend::from_connection_string(&format!(
        "postgres://postgres:postgres@{host}:{port}/{db}"
    ))
    .await
    .expect("backend");
    backend.init_schema().await.expect("schema");
    Some(Arc::new(backend))
}

macro_rules! coordinator_or_skip {
    ($name:expr) => {
        match backend($name).await {
            Some(b) => b
                .package_job_coordinator()
                .expect("PostgreSQL backs a package job coordinator"),
            None => {
                eprintln!("skipped: HFS_TEST_PG_HOST is not set");
                return;
            }
        }
    };
}

fn job(id: &str, status: &str, terminal: bool) -> PackageJobRecord {
    PackageJobRecord {
        id: id.into(),
        tenant: "default".into(),
        kind: "install".into(),
        status: status.into(),
        terminal,
        view: json!({"id": id, "status": status}),
        cancel_requested: false,
        owner: "instance-a".into(),
        updated_at: Utc::now(),
    }
}

#[tokio::test]
async fn jobs_round_trip_and_update_in_place() {
    let c = coordinator_or_skip!("rt");
    let id = format!("job-rt-{}", uuid::Uuid::new_v4());
    assert!(c.load_job(&id).await.unwrap().is_none());

    c.save_job(&job(&id, "running", false)).await.unwrap();
    let loaded = c.load_job(&id).await.unwrap().expect("saved");
    assert_eq!(loaded.status, "running");
    assert_eq!(loaded.kind, "install");
    assert_eq!(loaded.tenant, "default");
    assert!(!loaded.terminal);
    assert!(!loaded.cancel_requested);
    assert_eq!(loaded.view["status"], "running");

    let mut done = job(&id, "completed", true);
    done.owner = "instance-b".into();
    c.save_job(&done).await.unwrap();
    let loaded = c.load_job(&id).await.unwrap().unwrap();
    assert_eq!(loaded.status, "completed");
    assert!(loaded.terminal);
    assert_eq!(loaded.owner, "instance-b");
    assert_eq!(loaded.view["status"], "completed");
}

#[tokio::test]
async fn cancel_is_requested_only_for_running_jobs() {
    let c = coordinator_or_skip!("cancel");
    let running = format!("job-c1-{}", uuid::Uuid::new_v4());
    let finished = format!("job-c2-{}", uuid::Uuid::new_v4());
    c.save_job(&job(&running, "running", false)).await.unwrap();
    c.save_job(&job(&finished, "completed", true))
        .await
        .unwrap();

    assert!(!c.cancel_requested(&running).await.unwrap());
    assert!(c.request_cancel(&running).await.unwrap());
    assert!(c.cancel_requested(&running).await.unwrap());
    assert!(
        c.load_job(&running)
            .await
            .unwrap()
            .unwrap()
            .cancel_requested
    );
    // A later progress save keeps the request.
    c.save_job(&job(&running, "running", false)).await.unwrap();
    assert!(c.cancel_requested(&running).await.unwrap());

    assert!(
        !c.request_cancel(&finished).await.unwrap(),
        "already terminal"
    );
    assert!(!c.cancel_requested(&finished).await.unwrap());
    assert!(!c.request_cancel("no-such-job").await.unwrap());
    assert!(!c.cancel_requested("no-such-job").await.unwrap());
}

#[tokio::test]
async fn jobs_are_listed_newest_first_and_limited() {
    let c = coordinator_or_skip!("list");
    let tag = uuid::Uuid::new_v4();
    let ids: Vec<String> = (0..3).map(|i| format!("job-l{i}-{tag}")).collect();
    for id in &ids {
        c.save_job(&job(id, "queued", false)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let listed = c.list_jobs(1000).await.unwrap();
    let ours: Vec<&str> = listed
        .iter()
        .map(|j| j.id.as_str())
        .filter(|id| id.ends_with(&tag.to_string()))
        .collect();
    assert_eq!(ours, [ids[2].as_str(), ids[1].as_str(), ids[0].as_str()]);
    assert_eq!(c.list_jobs(2).await.unwrap().len(), 2);
}

#[tokio::test]
async fn the_install_lock_is_exclusive_and_released_on_drop() {
    let c = coordinator_or_skip!("lock");
    let key = 4242;
    let guard = c.lock_installs(key).await.unwrap();

    // A second holder waits while the first holds the lock.
    let c2 = Arc::clone(&c);
    let waiter = tokio::spawn(async move { c2.lock_installs(key).await.map(|_g| ()) });
    tokio::time::sleep(Duration::from_millis(300)).await;
    if waiter.is_finished() {
        let r = waiter.await.unwrap();
        panic!("the second taker must block, got {:?}", r.err());
    }

    drop(guard);
    tokio::time::timeout(Duration::from_secs(10), waiter)
        .await
        .expect("released after the guard is dropped")
        .unwrap()
        .unwrap();

    // Another key is independent.
    let a = c.lock_installs(1).await.unwrap();
    let b = tokio::time::timeout(Duration::from_secs(5), c.lock_installs(2))
        .await
        .expect("a different key does not wait")
        .unwrap();
    drop((a, b));
}
