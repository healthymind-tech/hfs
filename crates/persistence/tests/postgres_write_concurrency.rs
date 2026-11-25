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
use std::sync::Arc;

use helios_fhir::FhirVersion;
use helios_persistence::backends::postgres::{PostgresBackend, PostgresConfig};
use helios_persistence::core::{
    BundleEntry, BundleMethod, BundleProvider, ConditionalCreateResult, ConditionalStorage,
    ConditionalUpdateResult, EntityTagPrecondition, ResourceStorage, Transaction,
    TransactionOptions, TransactionProvider,
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

fn post_patient_entry(family: &str, n: usize) -> BundleEntry {
    BundleEntry {
        method: BundleMethod::Post,
        url: "Patient".to_string(),
        resource: Some(
            json!({"resourceType": "Patient", "name": [{"family": family, "given": [format!("g{n}")]}]}),
        ),
        full_url: Some(format!("urn:uuid:{}", uuid::Uuid::new_v4())),
        ..Default::default()
    }
}

/// Sixteen import-shaped bundles at once must all commit. They used to take
/// the exclusive tenant gate one after another, and whichever waited past
/// `statement_timeout` came back 500 "rolled back".
#[tokio::test]
async fn concurrent_transaction_bundles_all_commit() {
    // A short statement timeout makes queueing behind a global lock fail
    // fast, the way the benchmark's 30 s did under 16 clients: serialised,
    // the last of sixteen 400-entry bundles waits well past 1.5 s.
    let pg = shared_pg().await;
    let backend = Arc::new(
        PostgresBackend::new(PostgresConfig {
            host: pg.host.clone(),
            port: pg.port,
            dbname: "postgres".into(),
            user: "postgres".into(),
            password: Some("postgres".into()),
            max_connections: 16,
            statement_timeout_ms: 1_500,
            data_dir: Some(data_dir()),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    backend.init_schema().await.unwrap();
    let tenant = tenant("bundles");

    let mut tasks = Vec::new();
    for client in 0..16 {
        let backend = backend.clone();
        let tenant = tenant.clone();
        tasks.push(tokio::spawn(async move {
            for round in 0..2 {
                let entries: Vec<BundleEntry> = (0..400)
                    .map(|n| post_patient_entry(&format!("c{client}r{round}"), n))
                    .collect();
                backend
                    .process_transaction(&tenant, entries, FhirVersion::default())
                    .await
                    .map_err(|e| format!("client {client} round {round}: {e}"))?;
            }
            Ok::<(), String>(())
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(
        backend.count(&tenant, Some("Patient")).await.unwrap(),
        16 * 2 * 400
    );
}

/// Concurrent bundles that update the same resource serialise on it: every
/// one commits, versions are gapless, and history has one row per version.
#[tokio::test]
async fn concurrent_bundles_updating_one_resource_serialise() {
    let backend = Arc::new(backend(16).await);
    let tenant = tenant("hotspot");
    create_patient(&backend, &tenant, "hot").await;

    let mut tasks = Vec::new();
    for client in 0..12 {
        let backend = backend.clone();
        let tenant = tenant.clone();
        tasks.push(tokio::spawn(async move {
            let entries = vec![
                post_patient_entry("side", client),
                BundleEntry {
                    method: BundleMethod::Put,
                    url: "Patient/hot".to_string(),
                    resource: Some(json!({"resourceType": "Patient", "id": "hot", "name": [{"family": format!("v{client}")}]})),
                    ..Default::default()
                },
            ];
            backend
                .process_transaction(&tenant, entries, FhirVersion::default())
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    let current = backend
        .read(&tenant, "Patient", "hot")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.version_id(), "13");
    let client = backend.get_client().await.unwrap();
    let versions: Vec<String> = client
        .query(
            "SELECT version_id FROM resource_history
             WHERE tenant_id = $1 AND resource_type = 'Patient' AND id = 'hot'
             ORDER BY version_id::int",
            &[&tenant.tenant_id().as_str()],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(
        versions,
        (1..=13).map(|v| v.to_string()).collect::<Vec<_>>()
    );
}

/// `ifNoneExist` bundles racing on the same criteria still create exactly one
/// resource: a conditional bundle cannot plan its keys, so it keeps the
/// exclusive tenant gate.
#[tokio::test]
async fn concurrent_if_none_exist_bundles_create_once() {
    let backend = Arc::new(backend(16).await);
    let tenant = tenant("ifnone");
    let mut tasks = Vec::new();
    for client in 0..8 {
        let backend = backend.clone();
        let tenant = tenant.clone();
        tasks.push(tokio::spawn(async move {
            let entries = vec![
                BundleEntry {
                    method: BundleMethod::Post,
                    url: "Patient".to_string(),
                    resource: Some(json!({
                        "resourceType": "Patient",
                        "identifier": [{"system": "http://example.org/mrn", "value": "RACE-1"}],
                        "name": [{"family": format!("c{client}")}]
                    })),
                    if_none_exist: Some("identifier=http://example.org/mrn|RACE-1".to_string()),
                    full_url: Some(format!("urn:uuid:{}", uuid::Uuid::new_v4())),
                    ..Default::default()
                },
                post_patient_entry("other", client),
            ];
            backend
                .process_transaction(&tenant, entries, FhirVersion::default())
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    let client = backend.get_client().await.unwrap();
    let matching: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM resources
             WHERE tenant_id = $1 AND resource_type = 'Patient'
               AND data @> '{\"identifier\": [{\"value\": \"RACE-1\"}]}'",
            &[&tenant.tenant_id().as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(matching, 1);
}

/// A POST whose resource names its own id keeps it, and that client-chosen
/// id is locked rather than treated as fresh: a second bundle creating the
/// same id fails with a conflict, not a lock-plan violation.
#[tokio::test]
async fn bundle_post_with_client_id_conflicts_cleanly() {
    let backend = backend(4).await;
    let tenant = tenant("postid");
    let entry = || BundleEntry {
        method: BundleMethod::Post,
        url: "Patient".to_string(),
        resource: Some(json!({"resourceType": "Patient", "id": "chosen"})),
        ..Default::default()
    };
    backend
        .process_transaction(&tenant, vec![entry()], FhirVersion::default())
        .await
        .unwrap();
    let error = backend
        .process_transaction(&tenant, vec![entry()], FhirVersion::default())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("already exists"), "{error}");
}

/// A bundle's full-text rows are written a group at a time. A group holding
/// a resource whose text overflows PostgreSQL's 1 MB `tsvector` is replayed
/// per resource, so the oversized one is indexed truncated and its
/// neighbours are indexed whole, and the bundle still commits.
#[tokio::test]
async fn bundle_full_text_survives_an_oversized_resource() {
    let backend = backend(4).await;
    let tenant = tenant("ftsgroup");
    let text = (0..100_000)
        .map(|index| format!("lexeme{index:08x}"))
        .collect::<Vec<_>>()
        .join(" ");
    let patient = |id: &str, div: String| BundleEntry {
        method: BundleMethod::Put,
        url: format!("Patient/{id}"),
        resource: Some(json!({
            "resourceType": "Patient",
            "id": id,
            "text": {"status": "generated", "div": format!("<div>{div}</div>")}
        })),
        ..Default::default()
    };
    let entries = vec![
        patient("small-a", "alphaword".to_string()),
        patient("huge", text),
        patient("small-b", "betaword".to_string()),
    ];
    backend
        .process_transaction(&tenant, entries, FhirVersion::default())
        .await
        .unwrap();

    let client = backend.get_client().await.unwrap();
    let matches = |word: &'static str| {
        let client = &client;
        let tenant_id = tenant.tenant_id().as_str().to_string();
        async move {
            client
                .query(
                    "SELECT resource_id FROM resource_fts
                     WHERE tenant_id = $1 AND content_tsvector @@ plainto_tsquery('english', $2)
                     ORDER BY resource_id",
                    &[&tenant_id, &word],
                )
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.get::<_, String>(0))
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(matches("alphaword").await, vec!["small-a"]);
    assert_eq!(matches("betaword").await, vec!["small-b"]);
    assert_eq!(matches("lexeme00000000").await, vec!["huge"]);
}

/// Bundles that must take the exclusive gate (here: one `ifNoneExist` entry
/// each) run one at a time. Waiting their turn is not an error: the wait is
/// bounded separately from `statement_timeout`, so with a 1.5 s statement
/// timeout eight serialised bundles still all commit.
#[tokio::test]
async fn exclusive_gate_waits_are_not_cut_off_by_statement_timeout() {
    let pg = shared_pg().await;
    let backend = Arc::new(
        PostgresBackend::new(PostgresConfig {
            host: pg.host.clone(),
            port: pg.port,
            dbname: "postgres".into(),
            user: "postgres".into(),
            password: Some("postgres".into()),
            max_connections: 16,
            statement_timeout_ms: 1_500,
            data_dir: Some(data_dir()),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    backend.init_schema().await.unwrap();
    let tenant = tenant("gatewait");

    let mut tasks = Vec::new();
    for client in 0..8 {
        let backend = backend.clone();
        let tenant = tenant.clone();
        tasks.push(tokio::spawn(async move {
            let mut entries: Vec<BundleEntry> = (0..800)
                .map(|n| post_patient_entry(&format!("q{client}"), n))
                .collect();
            entries[0].if_none_exist = Some(format!("identifier=http://example.org/q|{client}"));
            entries[0].resource.as_mut().unwrap()["identifier"] =
                json!([{"system": "http://example.org/q", "value": client.to_string()}]);
            backend
                .process_transaction(&tenant, entries, FhirVersion::default())
                .await
                .map(|_| ())
                .map_err(|e| format!("client {client}: {e}"))
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(
        backend.count(&tenant, Some("Patient")).await.unwrap(),
        8 * 800
    );
}

/// The `resource_history` versions of one Patient, in order.
async fn history_versions(
    backend: &PostgresBackend,
    tenant: &TenantContext,
    id: &str,
) -> Vec<String> {
    backend
        .get_client()
        .await
        .unwrap()
        .query(
            "SELECT version_id FROM resource_history
             WHERE tenant_id = $1 AND resource_type = 'Patient' AND id = $2
             ORDER BY version_id::int",
            &[&tenant.tenant_id().as_str(), &id],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect()
}

fn patient(id: &str, family: &str) -> Value {
    json!({"resourceType": "Patient", "id": id, "name": [{"family": family}]})
}

fn identified_patient(value: &str) -> Value {
    json!({
        "resourceType": "Patient",
        "identifier": [{"system": "urn:hm:mrn", "value": value}],
        "name": [{"family": "Cond"}]
    })
}

/// More PUTs in flight than the pool has connections: each one needs exactly
/// one connection, so they queue for the pool and all succeed. A PUT used to
/// hold a session-lock connection while it waited for a second one to write
/// with; at pool size they starved each other until the pool's wait timeout
/// (10 s) and failed 503.
#[tokio::test]
async fn puts_beyond_the_pool_size_need_one_connection_each() {
    let backend = Arc::new(backend(3).await);
    let tenant = tenant("putpool");
    let started = std::time::Instant::now();
    for round in 0..2 {
        let mut tasks = Vec::new();
        for n in 0..36 {
            let backend = backend.clone();
            let tenant = tenant.clone();
            tasks.push(tokio::spawn(async move {
                let id = format!("p{n}");
                backend
                    .create_or_update(
                        &tenant,
                        "Patient",
                        &id,
                        patient(&id, &format!("r{round}")),
                        FhirVersion::default(),
                    )
                    .await
                    .map(|(stored, created)| (stored.version_id().to_string(), created))
                    .map_err(|e| e.to_string())
            }));
        }
        for task in tasks {
            let (version, created) = task.await.unwrap().expect("every PUT succeeds");
            assert_eq!(version, (round + 1).to_string());
            assert_eq!(created, round == 0);
        }
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(8),
        "PUTs must not wait out the pool timeout: {:?}",
        started.elapsed()
    );
    assert_eq!(history_versions(&backend, &tenant, "p7").await, ["1", "2"]);
}

/// Thirty PUTs of one id from three instances with small pools: every one
/// succeeds, each is its own version, history is gapless, and the search
/// index holds the rows of the last version only.
#[tokio::test]
async fn put_storm_on_one_id_keeps_every_version_with_small_pools() {
    let instances = [
        Arc::new(backend(2).await),
        Arc::new(backend(2).await),
        Arc::new(backend(2).await),
    ];
    let tenant = tenant("putstorm");
    let mut tasks = Vec::new();
    for n in 0..30 {
        let backend = instances[n % 3].clone();
        let tenant = tenant.clone();
        tasks.push(tokio::spawn(async move {
            backend
                .create_or_update(
                    &tenant,
                    "Patient",
                    "storm",
                    patient("storm", &format!("f{n}")),
                    FhirVersion::default(),
                )
                .await
                .map(|(stored, created)| (stored.version_id().to_string(), created))
                .map_err(|e| e.to_string())
        }));
    }
    let mut versions = Vec::new();
    let mut creates = 0;
    for task in tasks {
        let (version, created) = task.await.unwrap().expect("every PUT succeeds");
        versions.push(version.parse::<u32>().unwrap());
        creates += usize::from(created);
    }
    versions.sort_unstable();
    assert_eq!(versions, (1..=30).collect::<Vec<_>>());
    assert_eq!(creates, 1, "exactly one PUT created the resource");
    let expected: Vec<String> = (1..=30).map(|v| v.to_string()).collect();
    assert_eq!(history_versions(&instances[0], &tenant, "storm").await, expected);
    let current = instances[0]
        .read(&tenant, "Patient", "storm")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.version_id(), "30");
    assert_eq!(
        index_rows(&instances[0], &tenant, "storm", "family").await,
        1,
        "only the current version is indexed"
    );
}

/// PUTs racing onto a deleted id: one restores it, the others update it, and
/// the version chain continues past the deletion without a gap.
#[tokio::test]
async fn puts_racing_onto_a_deleted_id_restore_it_once() {
    let backend = Arc::new(backend(3).await);
    let tenant = tenant("putgone");
    create_patient(&backend, &tenant, "gone").await;
    backend.delete(&tenant, "Patient", "gone").await.unwrap();
    assert_eq!(index_rows(&backend, &tenant, "gone", "family").await, 0);

    let mut tasks = Vec::new();
    for n in 0..10 {
        let backend = backend.clone();
        let tenant = tenant.clone();
        tasks.push(tokio::spawn(async move {
            backend
                .create_or_update(
                    &tenant,
                    "Patient",
                    "gone",
                    patient("gone", &format!("back{n}")),
                    FhirVersion::default(),
                )
                .await
                .map(|(stored, created)| (stored.version_id().to_string(), created))
                .map_err(|e| e.to_string())
        }));
    }
    let mut restored = 0;
    for task in tasks {
        let (_, created) = task.await.unwrap().expect("every PUT succeeds");
        restored += usize::from(created);
    }
    assert_eq!(restored, 1, "exactly one PUT restored the tombstone");
    // v1 create, v2 delete, v3..v12 the ten PUTs.
    let expected: Vec<String> = (1..=12).map(|v| v.to_string()).collect();
    assert_eq!(history_versions(&backend, &tenant, "gone").await, expected);
    assert_eq!(index_rows(&backend, &tenant, "gone", "family").await, 1);
}

/// Conditional creates beyond the pool size: distinct criteria all create
/// (one connection each), and the same criteria from three instances create
/// exactly once.
#[tokio::test]
async fn conditional_creates_use_one_connection_and_create_once() {
    let instances = [
        Arc::new(backend(2).await),
        Arc::new(backend(2).await),
        Arc::new(backend(2).await),
    ];
    let tenant = tenant("condpool");
    let started = std::time::Instant::now();
    let mut tasks = Vec::new();
    for n in 0..24 {
        let backend = instances[n % 3].clone();
        let tenant = tenant.clone();
        tasks.push(tokio::spawn(async move {
            let value = format!("distinct-{n}");
            backend
                .conditional_create(
                    &tenant,
                    "Patient",
                    identified_patient(&value),
                    &format!("identifier=urn:hm:mrn|{value}"),
                    FhirVersion::default(),
                )
                .await
                .map(|result| matches!(result, ConditionalCreateResult::Created(_)))
                .map_err(|e| e.to_string())
        }));
    }
    for task in tasks {
        assert!(task.await.unwrap().expect("conditional create succeeds"));
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(8),
        "conditional creates must not wait out the pool timeout: {:?}",
        started.elapsed()
    );

    let mut tasks = Vec::new();
    for n in 0..18 {
        let backend = instances[n % 3].clone();
        let tenant = tenant.clone();
        tasks.push(tokio::spawn(async move {
            backend
                .conditional_create(
                    &tenant,
                    "Patient",
                    identified_patient("same"),
                    "identifier=urn:hm:mrn|same",
                    FhirVersion::default(),
                )
                .await
                .map_err(|e| e.to_string())
        }));
    }
    let (mut created, mut exists) = (0, 0);
    for task in tasks {
        match task.await.unwrap().expect("conditional create succeeds") {
            ConditionalCreateResult::Created(_) => created += 1,
            ConditionalCreateResult::Exists(_) => exists += 1,
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
    assert_eq!((created, exists), (1, 17));
    let stored: i64 = instances[0]
        .get_client()
        .await
        .unwrap()
        .query_one(
            "SELECT COUNT(*) FROM resources WHERE tenant_id = $1 AND resource_type = 'Patient'",
            &[&tenant.tenant_id().as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(stored, 25);
}

/// Conditional upserts on one criteria from three instances: one creates,
/// the rest update what it created, and nothing is duplicated.
#[tokio::test]
async fn conditional_upserts_on_one_criteria_create_once_then_update() {
    let instances = [
        Arc::new(backend(2).await),
        Arc::new(backend(2).await),
        Arc::new(backend(2).await),
    ];
    let tenant = tenant("condupsert");
    let mut tasks = Vec::new();
    for n in 0..12 {
        let backend = instances[n % 3].clone();
        let tenant = tenant.clone();
        tasks.push(tokio::spawn(async move {
            backend
                .conditional_update(
                    &tenant,
                    "Patient",
                    identified_patient("upsert"),
                    "identifier=urn:hm:mrn|upsert",
                    true,
                    FhirVersion::default(),
                    &EntityTagPrecondition::default(),
                )
                .await
                .map_err(|e| e.to_string())
        }));
    }
    let (mut created, mut updated) = (0, 0);
    let mut versions = Vec::new();
    for task in tasks {
        match task.await.unwrap().expect("conditional upsert succeeds") {
            ConditionalUpdateResult::Created(stored) => {
                created += 1;
                versions.push(stored.version_id().parse::<u32>().unwrap());
            }
            ConditionalUpdateResult::Updated(stored) => {
                updated += 1;
                versions.push(stored.version_id().parse::<u32>().unwrap());
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
    assert_eq!((created, updated), (1, 11));
    versions.sort_unstable();
    assert_eq!(versions, (1..=12).collect::<Vec<_>>());
    let stored: i64 = instances[0]
        .get_client()
        .await
        .unwrap()
        .query_one(
            "SELECT COUNT(*) FROM resources WHERE tenant_id = $1 AND resource_type = 'Patient'",
            &[&tenant.tenant_id().as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(stored, 1);
}

/// A resource whose text is far over the size that can be indexed without a
/// savepoint still stores, and a small one written on the same connection
/// afterwards is indexed for full text (the unguarded statement left no
/// aborted transaction behind).
#[tokio::test]
async fn single_writes_index_full_text_with_and_without_the_savepoint() {
    let backend = backend(2).await;
    let tenant = tenant("ftsguard");
    let huge: String = (0..400_000)
        .map(|n| format!("w{n} "))
        .collect::<Vec<_>>()
        .concat();
    backend
        .create(
            &tenant,
            "Patient",
            json!({"resourceType": "Patient", "id": "huge", "name": [{"family": "Big", "text": huge}]}),
            FhirVersion::default(),
        )
        .await
        .expect("an oversized resource is stored");
    create_patient(&backend, &tenant, "small").await;
    backend
        .create_or_update(
            &tenant,
            "Patient",
            "small",
            patient("small", "Renamed"),
            FhirVersion::default(),
        )
        .await
        .expect("update");
    let client = backend.get_client().await.unwrap();
    let fts: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM resource_fts
             WHERE tenant_id = $1 AND resource_type = 'Patient'
               AND content_tsvector @@ plainto_tsquery('english', 'Renamed')",
            &[&tenant.tenant_id().as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(fts, 1, "the updated small resource is in the full-text index");
    let rows: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM resource_fts WHERE tenant_id = $1 AND resource_type = 'Patient'",
            &[&tenant.tenant_id().as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(rows, 2);
}
