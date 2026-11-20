//! Integration tests of the endpoint admin API (`/admin/endpoints`) over
//! in-memory SQLite storage and the in-memory endpoint store
//! (`helios_endpoints::MemoryEndpointStore`). The PostgreSQL variant is
//! `admin_endpoints_postgres.rs`; both run the shared suite in
//! `admin_endpoints_suite/`.

#[path = "admin_endpoints_suite/mod.rs"]
mod suite;

use std::sync::Arc;

use axum::http::StatusCode;
use helios_endpoints::SecretKeyRing;
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use suite::{FakeDuplicate, Harness, OkProbe, Options};

fn sqlite() -> Arc<SqliteBackend> {
    let backend = SqliteBackend::with_config(
        ":memory:",
        SqliteBackendConfig {
            data_dir: Some(suite::data_dir()),
            ..Default::default()
        },
    )
    .expect("sqlite");
    backend.init_schema().expect("schema");
    Arc::new(backend)
}

async fn harness(opts: Options) -> Harness {
    Harness::memory(sqlite(), opts).await
}

#[tokio::test]
async fn crud_lifecycle() {
    suite::crud_lifecycle(&harness(Options::default()).await).await;
}

#[tokio::test]
async fn validation_and_conflicts() {
    suite::validation_and_conflicts(&harness(Options::default()).await).await;
}

#[tokio::test]
async fn secrets_are_write_only_and_sealed() {
    suite::secrets(&harness(Options::default()).await).await;
}

#[tokio::test]
async fn secrets_need_a_key_ring() {
    suite::no_key_ring(
        &harness(Options {
            key_ring: None,
            ..Options::default()
        })
        .await,
    )
    .await;
}

#[tokio::test]
async fn rewrap_reseals_under_the_active_key() {
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
async fn outbound_urls_pass_the_ssrf_policy() {
    suite::ssrf(&harness(Options::default()).await).await;
}

#[tokio::test]
async fn store_race_is_a_409_with_the_revision() {
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
async fn env_managed_endpoints_are_read_only() {
    suite::env_managed(&harness(Options::default()).await).await;
}

#[tokio::test]
async fn unauthenticated_and_bad_credentials_are_refused() {
    suite::authorization(&harness(Options::default()).await).await;
}

#[tokio::test]
async fn tokens_need_the_admin_scope() {
    suite::token_scope(
        &harness(Options {
            token_layer: Some("system/*.r"),
            ..Options::default()
        })
        .await,
        StatusCode::FORBIDDEN,
    )
    .await;
    suite::token_scope(
        &harness(Options {
            token_layer: Some("user/*.cruds"),
            ..Options::default()
        })
        .await,
        StatusCode::FORBIDDEN,
    )
    .await;
    suite::token_scope(
        &harness(Options {
            token_layer: Some("system/*.cruds"),
            ..Options::default()
        })
        .await,
        StatusCode::OK,
    )
    .await;
}

#[tokio::test]
async fn endpoint_scoped_packages() {
    suite::packages(&harness(Options::default()).await).await;
}

#[tokio::test]
async fn endpoint_packages_need_admin_ui() {
    suite::packages_disabled(
        &harness(Options {
            admin_ui: false,
            ..Options::default()
        })
        .await,
    )
    .await;
}

#[tokio::test]
async fn duplicate_without_sync_service_is_501() {
    suite::duplicate_unavailable(&harness(Options::default()).await).await;
}

#[tokio::test]
async fn duplicate_hands_off_to_the_sync_service() {
    let fake = Arc::new(FakeDuplicate::default());
    let h = harness(Options {
        duplicate: Some(fake.clone()),
        ..Options::default()
    })
    .await;
    suite::duplicate_handoff(&h, &fake).await;
}

#[tokio::test]
async fn test_auth_without_probe_is_501() {
    suite::test_auth_unavailable(&harness(Options::default()).await).await;
}

#[tokio::test]
async fn test_auth_runs_the_probe() {
    suite::test_auth_probe(
        &harness(Options {
            probe: Some(Arc::new(OkProbe)),
            ..Options::default()
        })
        .await,
    )
    .await;
}

#[tokio::test]
async fn stats_audit_search_and_purge() {
    suite::stats_audit_purge(&harness(Options::default()).await).await;
}

#[tokio::test]
async fn endpoint_cap() {
    suite::max_endpoints(
        &harness(Options {
            max_endpoints: 2,
            ..Options::default()
        })
        .await,
    )
    .await;
}
