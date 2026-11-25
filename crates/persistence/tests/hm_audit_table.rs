//! The dedicated audit tables on PostgreSQL (`hm-0030-audit-event-table`):
//! append, search semantics, immutability, tenant isolation, partition
//! creation / rollover / retention, the legacy backfill and the union read.
//!
//! Runs against an existing PostgreSQL named by `HFS_TEST_PG_HOST` /
//! `HFS_TEST_PG_PORT` (user, password `postgres`), in a database of its own
//! created for this run; without those variables the tests are skipped
//! (HealthyMind CI sets them, see docs/ops/ci.md).

#![cfg(feature = "postgres")]

use std::sync::Arc;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use helios_audit::sinks::database::AuditStorage;
use helios_audit::store::{AuditEventReader, AuditQuery, AuditRow, matches, parse_search};
use helios_audit::{AuditSink, RetentionConfig};
use helios_fhir::FhirVersion;
use helios_persistence::audit_store::retention::run_table_once;
use helios_persistence::audit_store::{AuditEventSearch, TableAuditStore};
use helios_persistence::backends::postgres::PostgresBackend;
use helios_persistence::backends::postgres::audit_table::{
    FORK_AUDIT_BACKFILL, FORK_AUDIT_EVENT_TABLE, PgAuditTable, month_start,
};
use helios_persistence::core::ResourceStorage;
use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
use serde_json::{Value, json};

/// A backend over a fresh database of its own (see `hm_postgres_coordination`).
async fn backend(name: &str) -> Option<Arc<PostgresBackend>> {
    let host = std::env::var("HFS_TEST_PG_HOST").ok()?;
    let port: u16 = std::env::var("HFS_TEST_PG_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5432);
    let admin = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let db = format!("hm_audit_{}_{name}", std::process::id());
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
    let mut config = PostgresBackend::config_from_connection_string(&format!(
        "postgres://postgres:postgres@{host}:{port}/{db}"
    ))
    .expect("config");
    // The search parameter definitions: the legacy `AuditEvent` resources are
    // searched (and sorted by `date`) through the general search index.
    config.data_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("data"));
    let backend = PostgresBackend::new(config).await.expect("backend");
    backend.init_schema().await.expect("schema");
    Some(Arc::new(backend))
}

macro_rules! table_or_skip {
    ($name:expr) => {
        match backend($name).await {
            Some(b) => (PgAuditTable::new(Arc::clone(&b)), b),
            None => {
                eprintln!("skipped: HFS_TEST_PG_HOST is not set");
                return;
            }
        }
    };
}

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

#[allow(clippy::too_many_arguments)]
fn event(
    id: &str,
    recorded: DateTime<Utc>,
    action: &str,
    subtype: &str,
    outcome: &str,
    entity: &str,
    patient: Option<&str>,
    user: &str,
) -> Value {
    let mut entities = vec![json!({"what": {"reference": entity}})];
    if let Some(p) = patient {
        entities.push(json!({"what": {"reference": format!("Patient/{p}")}}));
    }
    json!({
        "resourceType": "AuditEvent", "id": id,
        "type": {"system": "http://terminology.hl7.org/CodeSystem/audit-event-type", "code": "rest"},
        "subtype": [{"system": "http://hl7.org/fhir/restful-interaction", "code": subtype}],
        "action": action,
        "recorded": recorded.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "outcome": outcome,
        "agent": [
            {"requestor": true, "who": {"identifier": {"value": user}}},
            {"requestor": false, "who": {"reference": "Device/hfs"}}
        ],
        "source": {"observer": {"reference": "Device/hfs"}},
        "entity": entities
    })
}

fn db_message(e: &tokio_postgres::Error) -> String {
    e.as_db_error()
        .map(|d| d.message().to_string())
        .unwrap_or_default()
}

fn pairs(p: &[(&str, &str)]) -> Vec<(String, String)> {
    p.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

fn query(p: &[(&str, &str)], limit: usize) -> AuditQuery {
    let mut q = parse_search(&pairs(p)).unwrap().query;
    q.limit = limit;
    q
}

async fn sql_count(backend: &PostgresBackend, sql: &str) -> i64 {
    backend
        .get_client()
        .await
        .unwrap()
        .query_one(sql, &[])
        .await
        .unwrap()
        .get(0)
}

/// A varied set of events over the current month.
fn sample(now: DateTime<Utc>, n: usize) -> Vec<Value> {
    let base = month_start(now).and_hms_opt(1, 0, 0).unwrap().and_utc();
    (0..n)
        .map(|i| {
            let (action, subtype) = [("C", "create"), ("R", "read"), ("U", "update"), ("E", "search-type")][i % 4];
            event(
                &format!("ev-{i:04}"),
                // Several events share an instant, to exercise the id tie-break.
                base + Duration::seconds((i / 3) as i64),
                action,
                subtype,
                if i % 7 == 0 { "4" } else { "0" },
                &format!("Observation/o{}", i % 5),
                (i % 2 == 0).then_some(if i % 4 == 0 { "p1" } else { "p2" }),
                if i % 3 == 0 { "alice" } else { "bob" },
            )
        })
        .collect()
}

#[tokio::test]
async fn the_migration_creates_partitioned_tables_and_is_idempotent() {
    let (table, backend) = table_or_skip!("migration");
    assert_eq!(
        sql_count(
            &backend,
            &format!("SELECT count(*) FROM hm_schema_migrations WHERE name = '{FORK_AUDIT_EVENT_TABLE}'")
        )
        .await,
        1
    );
    // Never applied at start-up.
    assert_eq!(
        sql_count(
            &backend,
            &format!("SELECT count(*) FROM hm_schema_migrations WHERE name = '{FORK_AUDIT_BACKFILL}'")
        )
        .await,
        0
    );
    let now = Utc::now();
    let months: Vec<NaiveDate> = table.partitions().await.unwrap().iter().map(|p| p.month).collect();
    assert_eq!(months.len(), 3, "this month and the next two: {months:?}");
    assert_eq!(months[0], month_start(now));
    for t in ["audit_event_default", "audit_event_ref_default"] {
        assert_eq!(
            sql_count(&backend, &format!("SELECT count(*) FROM pg_class WHERE relname = '{t}'")).await,
            1
        );
    }
    // Running the migration SQL again, and ensuring existing partitions, changes nothing.
    backend.init_schema().await.unwrap();
    backend
        .get_client()
        .await
        .unwrap()
        .batch_execute(helios_persistence::backends::postgres::audit_table::FORK_AUDIT_EVENT_TABLE_SQL)
        .await
        .unwrap();
    assert_eq!(table.ensure_partitions(now, 2).await.unwrap(), 0);
    assert_eq!(table.partitions().await.unwrap().len(), 3);
    // The upstream ladder's version table is not touched by the fork migration.
    assert_eq!(sql_count(&backend, "SELECT count(*) FROM schema_version").await, 1);
}

#[tokio::test]
async fn appended_events_are_searchable_with_fhir_semantics() {
    let (table, backend) = table_or_skip!("search");
    let now = Utc::now();
    let events = sample(now, 60);
    table
        .append(events.iter().cloned().map(|e| ("t1".to_string(), e)).collect())
        .await
        .unwrap();
    // No row of the general resource tables was written.
    for t in ["resources", "resource_history", "search_index"] {
        assert_eq!(
            sql_count(&backend, &format!("SELECT count(*) FROM {t} WHERE resource_type = 'AuditEvent'")).await,
            0,
            "{t}"
        );
    }
    assert_eq!(sql_count(&backend, "SELECT count(*) FROM audit_event").await, 60);

    let rows: Vec<AuditRow> = events.iter().cloned().map(|e| AuditRow::from_event(e, now)).collect();
    let month = month_start(now).format("%Y-%m").to_string();
    let day = month_start(now).format("%Y-%m-%d").to_string();
    let criteria: Vec<Vec<(&str, &str)>> = vec![
        vec![],
        vec![("patient", "p1")],
        vec![("patient", "Patient/p2")],
        vec![("patient", "nobody")],
        vec![("entity", "Observation/o3")],
        vec![("entity", "o3")],
        vec![("entity", "Encounter/o3")],
        vec![("entity", "Observation/o1,Observation/o2")],
        vec![("agent", "alice")],
        vec![("agent", "Device/hfs")],
        vec![("action", "C")],
        vec![("action", "C,U")],
        vec![("action:not", "R")],
        vec![("outcome", "4")],
        vec![("outcome:not", "0")],
        vec![("type", "rest")],
        vec![("type", "http://terminology.hl7.org/CodeSystem/audit-event-type|rest")],
        vec![("type", "http://other|rest")],
        vec![("subtype", "create")],
        vec![("subtype", "http://hl7.org/fhir/restful-interaction|search-type")],
        vec![("subtype", "http://hl7.org/fhir/restful-interaction|")],
        vec![("subtype:not", "read")],
        vec![("date", &month)],
        vec![("date", &day)],
        vec![("date", "lt2000-01-01")],
        vec![("_lastUpdated", "ge2000")],
        vec![("_id", "ev-0003,ev-0004,nope")],
        vec![("patient", "p1"), ("action", "C")],
        vec![("patient", "p1"), ("entity", "Observation/o0")],
        vec![("patient", "p1"), ("entity", "Observation/o0"), ("outcome", "0"), ("subtype", "create")],
        vec![("agent", "bob"), ("date", &month), ("action", "R,E")],
    ];
    for c in &criteria {
        for ascending in [false, true] {
            let mut q = query(c, 1000);
            q.ascending = ascending;
            let mut expected: Vec<&AuditRow> = rows.iter().filter(|r| matches(&q, r)).collect();
            expected.sort_by(|a, b| (a.recorded, &a.id).cmp(&(b.recorded, &b.id)));
            if !ascending {
                expected.reverse();
            }
            let expected: Vec<&str> = expected.iter().map(|r| r.id.as_str()).collect();
            let got = table.search("t1", &q).await.unwrap();
            let got_ids: Vec<&str> = got.iter().map(|e| e.id.as_str()).collect();
            assert_eq!(got_ids, expected, "criteria {c:?} ascending {ascending}");
            assert_eq!(table.count("t1", &q).await.unwrap() as usize, expected.len(), "count {c:?}");

            // Keyset paging returns the same list, without gaps or repeats.
            let mut paged = Vec::new();
            let mut after = None;
            loop {
                let mut page_q = q.clone();
                page_q.limit = 7;
                page_q.after = after.clone();
                let page = table.search("t1", &page_q).await.unwrap();
                let Some(last) = page.last() else { break };
                after = Some((last.recorded, last.id.clone()));
                paged.extend(page.iter().map(|e| e.id.clone()));
                if page.len() < 7 {
                    break;
                }
            }
            assert_eq!(paged, expected, "paged {c:?} ascending {ascending}");
        }
    }

    // The JSON comes back unchanged apart from the server-side meta.
    let one = table.read("t1", "ev-0005").await.unwrap().unwrap();
    let mut body = one.event.clone();
    assert_eq!(body["meta"]["versionId"], "1");
    body.as_object_mut().unwrap().remove("meta");
    assert_eq!(body, events[5]);
    assert!(table.read("t1", "missing").await.unwrap().is_none());

    // An id generated by the event builder (UUIDv7) is read from its month.
    let built = helios_audit::AuditEventBuilder::new("Device/hfs").build();
    let mut value = serde_json::to_value(&built).unwrap();
    value["resourceType"] = json!("AuditEvent");
    let id = value["id"].as_str().unwrap().to_string();
    table.append(vec![("t1".to_string(), value)]).await.unwrap();
    assert_eq!(table.read("t1", &id).await.unwrap().unwrap().id, id);
}

#[tokio::test]
async fn tenants_do_not_see_each_others_events() {
    let (table, _backend) = table_or_skip!("tenants");
    let now = Utc::now();
    table
        .append(vec![
            ("a".to_string(), event("e1", now, "C", "create", "0", "Observation/1", Some("p"), "u")),
            ("b".to_string(), event("e2", now, "C", "create", "0", "Observation/1", Some("p"), "u")),
        ])
        .await
        .unwrap();
    for (tenant, own, other) in [("a", "e1", "e2"), ("b", "e2", "e1")] {
        for c in [vec![], vec![("patient", "p")], vec![("entity", "Observation/1"), ("action", "C")]] {
            let got = table.search(tenant, &query(&c, 10)).await.unwrap();
            assert_eq!(got.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec![own], "{tenant} {c:?}");
            assert_eq!(table.count(tenant, &query(&c, 10)).await.unwrap(), 1);
        }
        assert!(table.read(tenant, own).await.unwrap().is_some());
        assert!(table.read(tenant, other).await.unwrap().is_none());
    }
    assert!(table.search("c", &query(&[], 10)).await.unwrap().is_empty());
}

#[tokio::test]
async fn stored_events_cannot_be_updated_or_deleted() {
    let (table, backend) = table_or_skip!("immutable");
    let now = Utc::now();
    table
        .append(vec![("t".to_string(), event("e1", now, "C", "create", "0", "Observation/1", Some("p"), "u"))])
        .await
        .unwrap();
    let client = backend.get_client().await.unwrap();
    for sql in [
        "UPDATE audit_event SET outcome = '8'",
        "UPDATE audit_event SET event = '{}'::jsonb WHERE id = 'e1'",
        "DELETE FROM audit_event",
        "UPDATE audit_event_ref SET ref_id = 'x'",
        "DELETE FROM audit_event_ref",
    ] {
        let err = client.batch_execute(sql).await.expect_err(sql);
        assert!(db_message(&err).contains("append-only"), "{sql}: {err:?}");
    }
    // A setting left on by mistake still never allows an update.
    let err = client
        .batch_execute("BEGIN; SET LOCAL hfs.audit_maintenance = 'on'; UPDATE audit_event SET outcome = '8'; COMMIT;")
        .await
        .expect_err("update under maintenance");
    assert!(db_message(&err).contains("append-only"), "{err:?}");
    client.batch_execute("ROLLBACK").await.unwrap();
    assert_eq!(table.count("t", &query(&[], 1)).await.unwrap(), 1);
    assert_eq!(table.read("t", "e1").await.unwrap().unwrap().event["outcome"], "0");

    // Writing the same event again (a replayed batch) neither fails nor duplicates.
    let mut changed = event("e1", now, "D", "delete", "8", "Observation/1", Some("p"), "u");
    changed["recorded"] = table.read("t", "e1").await.unwrap().unwrap().event["recorded"].clone();
    table.append(vec![("t".to_string(), changed)]).await.unwrap();
    assert_eq!(sql_count(&backend, "SELECT count(*) FROM audit_event").await, 1);
    assert_eq!(table.read("t", "e1").await.unwrap().unwrap().event["action"], "C");
    assert_eq!(sql_count(&backend, "SELECT count(*) FROM audit_event_ref").await, 4);
}

#[tokio::test]
async fn a_tenant_purge_removes_its_audit_trail_only() {
    let (table, backend) = table_or_skip!("purge");
    let now = Utc::now();
    table
        .append(vec![
            ("gone".to_string(), event("e1", now, "C", "create", "0", "Observation/1", Some("p"), "u")),
            ("kept".to_string(), event("e2", now, "C", "create", "0", "Observation/1", Some("p"), "u")),
        ])
        .await
        .unwrap();
    backend.purge_tenant_data("gone").await.unwrap();
    assert_eq!(table.count("gone", &query(&[], 1)).await.unwrap(), 0);
    assert_eq!(table.count("kept", &query(&[], 1)).await.unwrap(), 1);
    assert_eq!(
        sql_count(&backend, "SELECT count(*) FROM audit_event_ref WHERE tenant_id = 'gone'").await,
        0
    );
}

#[tokio::test]
async fn months_without_a_partition_use_the_default_and_roll_over() {
    let (table, backend) = table_or_skip!("rollover");
    let now = Utc::now();
    // Six months ahead: no partition yet (the clock of another instance, or
    // maintenance that did not run).
    let future = now + Duration::days(183);
    let events: Vec<(String, Value)> = (0..5)
        .map(|i| {
            (
                "t".to_string(),
                event(&format!("f{i}"), future + Duration::seconds(i), "C", "create", "0", "Observation/1", Some("p"), "u"),
            )
        })
        .collect();
    table.append(events).await.unwrap();
    assert_eq!(sql_count(&backend, "SELECT count(*) FROM audit_event_default").await, 5);
    assert_eq!(table.count("t", &query(&[("patient", "p")], 1)).await.unwrap(), 5);

    // The partition appears: the rows move into it and stay searchable.
    assert!(table.ensure_partition(month_start(future)).await.unwrap());
    assert!(!table.ensure_partition(month_start(future)).await.unwrap());
    assert_eq!(sql_count(&backend, "SELECT count(*) FROM audit_event_default").await, 0);
    assert_eq!(sql_count(&backend, "SELECT count(*) FROM audit_event_ref_default").await, 0);
    let name = format!("audit_event_p{}", month_start(future).format("%Y_%m"));
    assert_eq!(sql_count(&backend, &format!("SELECT count(*) FROM {name}")).await, 5);
    let found = table.search("t", &query(&[("patient", "p")], 10)).await.unwrap();
    assert_eq!(found.len(), 5);
    assert_eq!(found[0].id, "f4");
    // New rows of that month go straight to the partition, and it is immutable too.
    table
        .append(vec![("t".to_string(), event("f9", future, "C", "create", "0", "Observation/1", None, "u"))])
        .await
        .unwrap();
    assert_eq!(sql_count(&backend, &format!("SELECT count(*) FROM {name}")).await, 6);
    let err = backend
        .get_client()
        .await
        .unwrap()
        .batch_execute(&format!("DELETE FROM {name}"))
        .await
        .expect_err("partition rows are immutable");
    assert!(db_message(&err).contains("append-only"), "{err:?}");

    // Many instances creating the same months at once: each is created once.
    let month = month_start(now + Duration::days(400));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let table = table.clone();
        tasks.push(tokio::spawn(async move { table.ensure_partition(month).await }));
    }
    let mut created = 0;
    for t in tasks {
        if t.await.unwrap().unwrap() {
            created += 1;
        }
    }
    assert_eq!(created, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_archives_then_drops_whole_expired_months() {
    let (table, backend) = table_or_skip!("retention");
    let now = at("2026-11-25T12:00:00Z");
    // Retention of 60 days: cutoff 2026-09-26. August is entirely before it,
    // September straddles it (kept: at most one extra month), October is kept.
    for month in ["2026-08-01", "2026-09-01", "2026-10-01"] {
        table
            .ensure_partition(NaiveDate::parse_from_str(month, "%Y-%m-%d").unwrap())
            .await
            .unwrap();
    }
    let mut events = Vec::new();
    for (i, when) in [
        "2026-08-03T00:00:00Z",
        "2026-08-30T23:59:59Z",
        "2026-09-10T00:00:00Z",
        "2026-09-29T00:00:00Z",
        "2026-10-05T00:00:00Z",
        // No partition for these months: default partition.
        "2025-01-15T00:00:00Z",
        "2025-02-15T00:00:00Z",
    ]
    .iter()
    .enumerate()
    {
        events.push((
            if i % 2 == 0 { "a" } else { "b" }.to_string(),
            event(&format!("r{i}"), at(when), "C", "create", "0", "Observation/1", Some("p"), "u"),
        ));
    }
    table.append(events).await.unwrap();
    assert_eq!(sql_count(&backend, "SELECT count(*) FROM audit_event_default").await, 2);

    let dir = tempfile::tempdir().unwrap();
    let config = RetentionConfig {
        days: Some(60),
        interval_hours: 24,
        batch: 1,
        archive_dir: Some(dir.path().to_string_lossy().to_string()),
    };

    // Another instance holds the maintenance lock: this run does nothing.
    let lock = table.maintenance_lock().await.unwrap().expect("free lock");
    let skipped = run_table_once(&table, &config, now).await.unwrap();
    assert_eq!(skipped.purged, 0);
    lock.release().await;

    let report = run_table_once(&table, &config, now).await.unwrap();
    assert_eq!(report.purged, 4, "two August events and two of the default partition");
    let names: Vec<String> = table.partitions().await.unwrap().into_iter().map(|p| p.name).collect();
    assert!(!names.contains(&"audit_event_p2026_08".to_string()), "{names:?}");
    assert!(names.contains(&"audit_event_p2026_09".to_string()), "{names:?}");
    assert_eq!(
        sql_count(&backend, "SELECT count(*) FROM pg_class WHERE relname = 'audit_event_ref_p2026_08'").await,
        0
    );
    assert_eq!(sql_count(&backend, "SELECT count(*) FROM audit_event").await, 3);
    assert_eq!(sql_count(&backend, "SELECT count(*) FROM audit_event_default").await, 0);
    assert_eq!(
        sql_count(&backend, "SELECT count(*) FROM audit_event_ref WHERE recorded < '2026-09-01'").await,
        0
    );

    // Everything that was removed is in the archive.
    let archive = report.archive.expect("archive written");
    let mut text = String::new();
    std::io::Read::read_to_string(
        &mut flate2::read::GzDecoder::new(std::fs::File::open(&archive).unwrap()),
        &mut text,
    )
    .unwrap();
    let mut archived: Vec<String> = text
        .lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            assert_eq!(v["resource"]["resourceType"], "AuditEvent");
            format!("{}/{}", v["tenant"].as_str().unwrap(), v["resource"]["id"].as_str().unwrap())
        })
        .collect();
    archived.sort();
    assert_eq!(archived, vec!["a/r0", "a/r6", "b/r1", "b/r5"]);

    // Nothing more to do; an unwritable archive stops a run before it drops anything.
    assert_eq!(run_table_once(&table, &config, now).await.unwrap().purged, 0);
    let blocked = RetentionConfig {
        days: Some(1),
        archive_dir: Some("/proc/hfs-no-such-dir/archive".to_string()),
        ..config.clone()
    };
    assert!(run_table_once(&table, &blocked, now).await.is_err());
    assert_eq!(sql_count(&backend, "SELECT count(*) FROM audit_event").await, 3);
    // No retention configured: a no-op.
    let off = RetentionConfig { days: None, ..config };
    assert_eq!(run_table_once(&table, &off, now).await.unwrap().purged, 0);
}

fn tenant(id: &str) -> TenantContext {
    TenantContext::new(TenantId::new(id), TenantPermissions::full_access())
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_events_stay_visible_and_the_backfill_moves_them() {
    let (table, backend) = table_or_skip!("backfill");
    let now = Utc::now();
    // Events written by the previous layout: AuditEvent resources.
    for i in 0..25 {
        let when = now - Duration::days(400) + Duration::minutes(i);
        backend
            .create(
                &tenant("default"),
                "AuditEvent",
                event(&format!("old-{i:02}"), when, "R", "read", "0", "Observation/1", Some("p1"), "u"),
                FhirVersion::default(),
            )
            .await
            .unwrap();
    }
    let store = TableAuditStore::new(Arc::clone(&backend), None, "default".to_string());
    // New events go to the table (through the sink's storage contract).
    let fresh: Vec<(Option<String>, Value)> = (0..5)
        .map(|i| {
            (
                None,
                event(&format!("new-{i}"), now - Duration::seconds(10 - i), "C", "create", "0", "Observation/2", Some("p1"), "u"),
            )
        })
        .collect();
    assert!(store.appends());
    store.append_events(fresh).await.unwrap().unwrap();
    assert_eq!(sql_count(&backend, "SELECT count(*) FROM resources WHERE resource_type = 'AuditEvent'").await, 25);
    assert!(table.has_legacy_events("default").await.unwrap());

    // The union read (HTS's /AuditEvent) sees both sources, newest first.
    let all = store.search_audit_events(&pairs(&[("_count", "1000")])).await.unwrap();
    assert_eq!(all.len(), 30);
    assert_eq!(all[0]["id"], "new-4");
    assert_eq!(all[4]["id"], "new-0");
    assert_eq!(all[5]["id"], "old-24");
    assert_eq!(all[29]["id"], "old-00");
    let top = store.search_audit_events(&pairs(&[("_count", "7")])).await.unwrap();
    assert_eq!(top.len(), 7);
    assert_eq!(top[6]["id"], "old-23", "{:?}", top.iter().map(|e| e["id"].clone()).collect::<Vec<_>>());
    let by_patient = store
        .search_audit_events(&pairs(&[("patient", "Patient/p1"), ("subtype", "read"), ("_count", "100")]))
        .await
        .unwrap();
    assert_eq!(by_patient.len(), 25);
    assert!(store.read_audit_event("old-03").await.unwrap().is_some());
    assert!(store.read_audit_event("new-3").await.unwrap().is_some());
    assert!(matches!(
        store.search_audit_events(&pairs(&[("name", "x")])).await,
        Err(helios_persistence::audit_store::AuditSearchError::BadRequest(_))
    ));

    // The backfill, interrupted after one batch and then resumed.
    let first = table.backfill_legacy(10, Some(1)).await.unwrap();
    assert_eq!((first.copied, first.purged, first.finished), (10, 10, false));
    assert_eq!(store.search_audit_events(&pairs(&[("_count", "1000")])).await.unwrap().len(), 30);
    let rest = table.backfill_legacy(10, None).await.unwrap();
    assert_eq!((rest.copied, rest.purged, rest.finished), (15, 15, true));
    for t in ["resources", "resource_history", "search_index"] {
        assert_eq!(
            sql_count(&backend, &format!("SELECT count(*) FROM {t} WHERE resource_type = 'AuditEvent'")).await,
            0,
            "{t}"
        );
    }
    assert!(!table.has_legacy_events("default").await.unwrap());
    assert_eq!(table.count("default", &query(&[], 1)).await.unwrap(), 30);
    assert_eq!(
        sql_count(&backend, &format!("SELECT count(*) FROM hm_schema_migrations WHERE name = '{FORK_AUDIT_BACKFILL}'")).await,
        1
    );
    assert_eq!(sql_count(&backend, "SELECT copied FROM hm_audit_backfill_state").await, 25);
    // Old months got partitions of their own rather than the default one.
    assert_eq!(sql_count(&backend, "SELECT count(*) FROM audit_event_default").await, 0);
    let all = store.search_audit_events(&pairs(&[("_count", "1000")])).await.unwrap();
    assert_eq!(all.len(), 30);
    assert_eq!(all[5]["id"], "old-24");
    let moved = table.read("default", "old-07").await.unwrap().unwrap();
    assert_eq!(moved.event["subtype"][0]["code"], "read");
    assert_eq!(moved.event["meta"]["versionId"], "1");
    // Running it again is a no-op.
    let again = table.backfill_legacy(10, None).await.unwrap();
    assert_eq!((again.copied, again.finished), (0, true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_and_the_queued_sink_lose_nothing() {
    let (table, backend) = table_or_skip!("writers");
    let store: Arc<dyn AuditStorage> =
        Arc::new(TableAuditStore::new(Arc::clone(&backend), Some("audit".to_string()), "default".to_string()));
    let sink = Arc::new(helios_audit::QueuedSink::new(
        Arc::new(helios_audit::DatabaseSink::new(store, FhirVersion::default())),
        helios_audit::QueueConfig {
            capacity: 64,
            batch_size: 50,
            workers: 4,
            ..Default::default()
        },
    ));
    // 8 producers x 250 events through a queue much smaller than the load:
    // producers wait for room, nothing is dropped.
    let mut tasks = Vec::new();
    for p in 0..8 {
        let sink = Arc::clone(&sink);
        tasks.push(tokio::spawn(async move {
            for i in 0..250 {
                let e = helios_audit::AuditEventBuilder::new("Device/hfs")
                    .action(helios_audit::AuditAction::Create)
                    .outcome("0")
                    .detail("n", format!("{p}-{i}"))
                    .build();
                sink.record_in(Some("ignored-because-fixed"), e).await;
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    sink.flush().await;
    assert_eq!(sink.pending(), 0);
    assert_eq!(table.count("audit", &query(&[], 1)).await.unwrap(), 2000);
    assert_eq!(sql_count(&backend, "SELECT count(DISTINCT id) FROM audit_event").await, 2000);
    assert_eq!(sql_count(&backend, "SELECT count(*) FROM resources WHERE resource_type = 'AuditEvent'").await, 0);
    // The writers took no advisory locks and left no transaction open.
    assert_eq!(
        sql_count(&backend, "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND database = (SELECT oid FROM pg_database WHERE datname = current_database())").await,
        0
    );
}
