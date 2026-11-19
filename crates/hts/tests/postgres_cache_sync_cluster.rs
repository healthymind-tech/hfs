//! Cluster cache coherence for HTS on PostgreSQL.
//!
//! Two independently constructed apps (own pools, own response caches) share
//! one database, the way two `hts` replicas behind a load balancer do. A
//! CodeSystem change written through replica A must reach replica B's
//! `$lookup` / `$validate-code` answers through the shared
//! `terminology_epoch` poller — and, as the control, must *not* reach a
//! replica that runs without it (proving the test exercises the poller and
//! not some accidental cache miss).
//!
//! Run with:
//!   `cargo test -p helios-hts --features postgres --test postgres_cache_sync_cluster`

#![cfg(feature = "postgres")]

mod common;

use std::time::{Duration, Instant};

use axum::http::StatusCode;
use common::TestAppPg;
use serde_json::{Value, json};

const CS_URL: &str = "http://cluster-sync.example/cs";

fn code_system(display: &str, id: Option<&str>) -> String {
    let mut cs = json!({
        "resourceType": "CodeSystem",
        "url": CS_URL,
        "version": "1",
        "name": "ClusterSync",
        "status": "active",
        "content": "complete",
        "concept": [{"code": "a", "display": display}]
    });
    if let Some(id) = id {
        cs["id"] = json!(id);
    }
    cs.to_string()
}

async fn lookup_display(app: &TestAppPg) -> Option<String> {
    let (status, body) = app
        .get_fhir(&format!("/CodeSystem/$lookup?system={CS_URL}&code=a"))
        .await;
    if status != StatusCode::OK {
        return None;
    }
    body["parameter"]
        .as_array()?
        .iter()
        .find(|p| p["name"] == "display")
        .and_then(|p| p["valueString"].as_str())
        .map(str::to_string)
}

async fn validate_code_result(app: &TestAppPg, code: &str) -> Option<bool> {
    let (status, body): (StatusCode, Value) = app
        .get_fhir(&format!(
            "/CodeSystem/$validate-code?url={CS_URL}&code={code}"
        ))
        .await;
    if status != StatusCode::OK {
        return None;
    }
    body["parameter"]
        .as_array()?
        .iter()
        .find(|p| p["name"] == "result")
        .and_then(|p| p["valueBoolean"].as_bool())
}

async fn eventually<F, Fut>(timeout: Duration, mut probe: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if probe().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// An update through A reaches B's warmed `$lookup` cache within the poll
/// window; a replica without the poller keeps serving the stale answer.
#[tokio::test(flavor = "multi_thread")]
async fn update_via_one_replica_reaches_the_other_replicas_lookup() {
    let a = TestAppPg::new().await;
    let b = TestAppPg::replica(a.database_url(), Some(Duration::from_millis(50))).await;
    let control = TestAppPg::replica(a.database_url(), None).await;

    let (status, body) = a.post_fhir("/CodeSystem", code_system("Old", None)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_string();

    // Warm both caches on the old content.
    assert_eq!(lookup_display(&b).await.as_deref(), Some("Old"));
    assert_eq!(lookup_display(&control).await.as_deref(), Some("Old"));

    let (status, body) = a
        .put_fhir(
            &format!("/CodeSystem/{id}"),
            code_system("New", Some(&id)),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(lookup_display(&a).await.as_deref(), Some("New"));

    assert!(
        eventually(Duration::from_secs(5), || async {
            lookup_display(&b).await.as_deref() == Some("New")
        })
        .await,
        "replica B must drop its cached $lookup after A's update"
    );
    // The control has no poller: its cache still answers the old display,
    // which is exactly the per-process staleness the poller removes.
    assert_eq!(lookup_display(&control).await.as_deref(), Some("Old"));
}

/// A delete through A turns B's cached positive `$validate-code` negative.
#[tokio::test(flavor = "multi_thread")]
async fn delete_via_one_replica_reaches_the_other_replicas_validate_code() {
    let a = TestAppPg::new().await;
    let b = TestAppPg::replica(a.database_url(), Some(Duration::from_millis(50))).await;

    let (status, body) = a.post_fhir("/CodeSystem", code_system("Alpha", None)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_string();
    assert_eq!(validate_code_result(&b, "a").await, Some(true));

    let status = a.delete_fhir(&format!("/CodeSystem/{id}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    assert!(
        eventually(Duration::from_secs(5), || async {
            validate_code_result(&b, "a").await != Some(true)
        })
        .await,
        "replica B must stop validating a code whose CodeSystem A deleted"
    );
}

/// A write that bypasses every server (hand-written SQL, another tool) still
/// moves the epoch through the header-table trigger.
#[tokio::test(flavor = "multi_thread")]
async fn out_of_band_sql_write_moves_the_epoch() {
    let a = TestAppPg::new().await;
    let backend = common::pg_hts_backend(a.database_url()).await.unwrap();
    let (status, body) = a.post_fhir("/CodeSystem", code_system("X", None)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let before = backend.terminology_epoch().await.unwrap();
    let client = backend.pool().get().await.unwrap();
    client
        .execute(
            "UPDATE code_systems SET name = 'Renamed' WHERE url = $1",
            &[&CS_URL],
        )
        .await
        .unwrap();
    let after = backend.terminology_epoch().await.unwrap();
    assert!(
        after > before,
        "trigger must bump the epoch ({before} -> {after})"
    );
}
