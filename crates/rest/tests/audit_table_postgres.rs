//! End to end with the dedicated audit store (`HFS_AUDIT_STORE=table`):
//! every REST interaction leaves a BALP `AuditEvent` in the audit tables, the
//! FHIR API serves `AuditEvent` read / vread / search from them, the events
//! written before the tables existed stay visible, clients cannot alter the
//! trail, and compartment walks do not return it unless asked.
//!
//! Runs against the PostgreSQL named by `HFS_TEST_PG_HOST` /
//! `HFS_TEST_PG_PORT` (user, password, database `postgres`); skipped without
//! them. Each test uses tenants of its own.

#![cfg(feature = "postgres")]

use std::path::PathBuf;
use std::sync::Arc;

use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum_test::TestServer;
use helios_audit::{AuditConfig, AuditMiddlewareState, DatabaseSink, balp};
use helios_auth::AuthConfig;
use helios_fhir::FhirVersion;
use helios_persistence::audit_store::TableAuditStore;
use helios_persistence::backends::postgres::{PostgresBackend, PostgresConfig};
use helios_persistence::core::ResourceStorage;
use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
use helios_rest::ServerConfig;
use serde_json::{Value, json};

const X_TENANT_ID: HeaderName = HeaderName::from_static("x-tenant-id");
const PREFER: HeaderName = HeaderName::from_static("prefer");
const BASE: &str = "http://fhir.test/fhir";

struct Fixture {
    server: TestServer,
    backend: Arc<PostgresBackend>,
    /// Tenant of this test.
    tenant: String,
}

async fn backend() -> Option<Arc<PostgresBackend>> {
    let host = std::env::var("HFS_TEST_PG_HOST").ok()?;
    let port: u16 = std::env::var("HFS_TEST_PG_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5432);
    let data_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("data"))
        .unwrap();
    let backend = PostgresBackend::new(PostgresConfig {
        host,
        port,
        dbname: "postgres".to_string(),
        user: "postgres".to_string(),
        password: Some("postgres".to_string()),
        max_connections: 5,
        data_dir: Some(data_dir),
        ..Default::default()
    })
    .await
    .expect("backend");
    backend.init_schema().await.expect("schema");
    Some(Arc::new(backend))
}

/// A server over PostgreSQL. `table`: audit events go to the audit tables and
/// are served from them; otherwise they are stored as resources (the layout
/// before the tables, `HFS_AUDIT_STORE=resource`).
async fn fixture(name: &str, table: bool) -> Option<Fixture> {
    let backend = backend().await?;
    let tenant = format!("audit_{name}_{}", uuid::Uuid::new_v4().simple());
    let config = ServerConfig {
        base_url: BASE.to_string(),
        default_tenant: tenant.clone(),
        seed_conformance: false,
        ..ServerConfig::for_testing()
    };
    let store = Arc::new(TableAuditStore::new(
        Arc::clone(&backend),
        None,
        tenant.clone(),
    ));
    let audit = if table {
        let sink = Arc::new(DatabaseSink::new(store.clone(), FhirVersion::default()));
        AuditMiddlewareState::new(sink, AuditConfig::default())
            .with_server_address(BASE)
            .with_reader(Arc::new(store.table().clone()))
    } else {
        let legacy = Arc::new(helios_persistence::audit_store::TxAuditStore::new(
            Arc::clone(&backend),
            None,
            tenant.clone(),
        ));
        let sink = Arc::new(DatabaseSink::new(legacy, FhirVersion::default()));
        AuditMiddlewareState::new(sink, AuditConfig::default()).with_server_address(BASE)
    };
    // The app owns its backend; the audit store has a pool of its own, as in
    // `hfs`.
    let Ok(app_backend) = Arc::try_unwrap(self::backend().await?) else {
        panic!("the app backend has a single owner");
    };
    let app = helios_rest::create_app_with_auth(
        app_backend,
        config,
        AuthConfig::default(),
        None,
        Some(Arc::new(audit)),
    );
    Some(Fixture {
        server: TestServer::new(app).expect("server"),
        backend,
        tenant,
    })
}

macro_rules! fixture_or_skip {
    ($name:expr, $table:expr) => {
        match fixture($name, $table).await {
            Some(f) => f,
            None => {
                eprintln!("skipped: HFS_TEST_PG_HOST is not set");
                return;
            }
        }
    };
}

impl Fixture {
    fn t(&self) -> HeaderValue {
        HeaderValue::from_str(&self.tenant).unwrap()
    }

    async fn get(&self, path: &str) -> axum_test::TestResponse {
        self.server
            .get(path)
            .add_header(X_TENANT_ID, self.t())
            .await
    }

    async fn bundle(&self, path: &str) -> Value {
        let resp = self.get(path).await;
        resp.assert_status_ok();
        resp.json()
    }

    async fn sql_count(&self, sql: &str) -> i64 {
        self.backend
            .get_client()
            .await
            .unwrap()
            .query_one(sql, &[&self.tenant])
            .await
            .unwrap()
            .get(0)
    }

    async fn seed_clinical(&self) {
        for (path, body) in [
            (
                "/Patient/p1",
                json!({"resourceType": "Patient", "id": "p1"}),
            ),
            (
                "/Patient/p2",
                json!({"resourceType": "Patient", "id": "p2"}),
            ),
            (
                "/Observation/o1",
                json!({"resourceType": "Observation", "id": "o1", "status": "final",
                       "code": {"text": "x"}, "subject": {"reference": "Patient/p1"}}),
            ),
            (
                "/Observation/o2",
                json!({"resourceType": "Observation", "id": "o2", "status": "final",
                       "code": {"text": "x"}, "subject": {"reference": "Patient/p2"}}),
            ),
        ] {
            let resp = self
                .server
                .put(path)
                .add_header(X_TENANT_ID, self.t())
                .json(&body)
                .await;
            assert!(resp.status_code().is_success(), "{path}: {}", resp.text());
        }
    }
}

fn entries(bundle: &Value) -> Vec<&Value> {
    bundle["entry"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["search"]["mode"] != "outcome")
        .map(|e| &e["resource"])
        .collect()
}

fn next_link(bundle: &Value) -> Option<String> {
    bundle["link"]
        .as_array()?
        .iter()
        .find(|l| l["relation"] == "next")
        .and_then(|l| l["url"].as_str())
        .map(|u| u.strip_prefix(BASE).unwrap_or(u).to_string())
}

fn subtype(e: &Value) -> &str {
    e["subtype"][0]["code"].as_str().unwrap_or("")
}

#[tokio::test]
async fn interactions_land_in_the_audit_tables_and_are_served_from_them() {
    let f = fixture_or_skip!("served", true);
    f.seed_clinical().await;
    f.get("/Observation/o1").await.assert_status_ok();
    f.get("/Observation?subject=Patient/p1")
        .await
        .assert_status_ok();
    f.get("/Observation/missing")
        .await
        .assert_status(StatusCode::NOT_FOUND);

    // Nothing of the audit trail is in the resource tables.
    assert_eq!(
        f.sql_count(
            "SELECT count(*) FROM resources WHERE tenant_id = $1 AND resource_type = 'AuditEvent'"
        )
        .await,
        0
    );
    assert_eq!(
        f.sql_count("SELECT count(*) FROM search_index WHERE tenant_id = $1 AND resource_type = 'AuditEvent'").await,
        0
    );
    assert_eq!(
        f.sql_count("SELECT count(*) FROM audit_event WHERE tenant_id = $1")
            .await,
        7
    );

    let all = f.bundle("/AuditEvent?_count=100").await;
    assert_eq!(all["resourceType"], "Bundle");
    assert_eq!(all["type"], "searchset");
    let events = entries(&all);
    // 4 writes, 2 reads and 1 failed read so far; this search is recorded
    // after it answered.
    assert_eq!(events.len(), 7);
    assert!(all.get("total").is_none(), "no total unless asked");
    // Newest first, each a complete BALP event with server meta.
    assert_eq!(events[0]["outcome"], "4");
    assert_eq!(subtype(events[6]), "create", "a PUT to a new id creates");
    for e in &events {
        assert_eq!(e["resourceType"], "AuditEvent");
        assert_eq!(e["meta"]["versionId"], "1");
        assert!(e["meta"]["lastUpdated"].is_string());
        assert_eq!(e["source"]["observer"]["reference"], "Device/hfs");
    }
    let create = events
        .iter()
        .find(|e| e["entity"].to_string().contains("Observation/o1") && e["action"] != "R")
        .unwrap();
    assert!(
        create["meta"]["profile"]
            .to_string()
            .contains("IHE.BasicAudit"),
        "the BALP profile claim survives storage: {create}"
    );
    let full_urls: Vec<&str> = all["entry"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["fullUrl"].as_str())
        .collect();
    assert!(
        full_urls[0].starts_with(&format!("{BASE}/AuditEvent/")),
        "{full_urls:?}"
    );

    // Criteria of the fixed parameter set.
    let count = |v: &Value| entries(v).len();
    assert_eq!(
        count(&f.bundle("/AuditEvent?patient=Patient/p1&_count=100").await),
        4
    );
    assert_eq!(count(&f.bundle("/AuditEvent?patient=p2").await), 2);
    assert_eq!(
        count(&f.bundle("/AuditEvent?entity=Observation/o1").await),
        2
    );
    assert_eq!(
        count(&f.bundle("/AuditEvent?entity=Observation/o1&action=R").await),
        1
    );
    assert_eq!(count(&f.bundle("/AuditEvent?outcome=4").await), 1);
    assert_eq!(count(&f.bundle("/AuditEvent?outcome:not=0").await), 1);
    // The Observation search, and every AuditEvent search made so far.
    assert_eq!(
        count(&f.bundle("/AuditEvent?subtype=search-type&_count=100").await),
        8
    );
    assert_eq!(
        count(
            &f.bundle("/AuditEvent?type=rest&date=ge2020&_count=100")
                .await
        ),
        15
    );
    assert_eq!(count(&f.bundle("/AuditEvent?date=lt2020").await), 0);
    let by_id = f
        .bundle(&format!(
            "/AuditEvent?_id={}",
            create["id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(entries(&by_id)[0]["id"], create["id"]);

    // POST _search answers the same.
    let resp = f
        .server
        .post("/AuditEvent/_search")
        .add_header(X_TENANT_ID, f.t())
        .form(&[("patient", "Patient/p1"), ("action", "R")])
        .await;
    resp.assert_status_ok();
    assert!(!entries(&resp.json::<Value>()).is_empty());

    // Counts.
    let n = f
        .sql_count("SELECT count(*) FROM audit_event WHERE tenant_id = $1")
        .await;
    let summary = f.bundle("/AuditEvent?_summary=count").await;
    assert_eq!(summary["total"], n);
    assert!(summary.get("entry").is_none());
    let with_total = f.bundle("/AuditEvent?outcome=4&_total=accurate").await;
    assert_eq!(with_total["total"], 1);
    assert_eq!(entries(&with_total).len(), 1);
}

#[tokio::test]
async fn search_pages_through_the_whole_trail_in_both_orders() {
    let f = fixture_or_skip!("paging", true);
    f.seed_clinical().await;
    for _ in 0..12 {
        f.get("/Observation/o1").await.assert_status_ok();
    }
    for sort in ["-date", "date"] {
        let mut seen: Vec<String> = Vec::new();
        let mut url = Some(format!(
            "/AuditEvent?entity=Observation/o1&_sort={sort}&_count=5"
        ));
        let mut pages = 0;
        while let Some(u) = url {
            let page = f.bundle(&u).await;
            let ids: Vec<String> = entries(&page)
                .iter()
                .map(|e| e["id"].as_str().unwrap().to_string())
                .collect();
            assert!(ids.len() <= 5);
            seen.extend(ids);
            url = next_link(&page);
            pages += 1;
            assert!(pages < 10, "paging does not terminate");
        }
        assert_eq!(seen.len(), 13, "one create and twelve reads ({sort})");
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 13, "no event twice ({sort})");
        let first = f.bundle(&format!("/AuditEvent/{}", seen[0])).await;
        let last = f.bundle(&format!("/AuditEvent/{}", seen[12])).await;
        let (a, b) = (
            first["recorded"].as_str().unwrap(),
            last["recorded"].as_str().unwrap(),
        );
        if sort == "date" {
            assert!(a <= b);
        } else {
            assert!(a >= b);
        }
    }
    // A cursor of another search type is refused, not silently restarted.
    f.get("/AuditEvent?_cursor=bogus")
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn unsupported_parameters_are_reported_or_refused() {
    let f = fixture_or_skip!("params", true);
    f.seed_clinical().await;
    let lenient = f
        .bundle("/AuditEvent?entity-name=x&_content=y&action=C&_count=50")
        .await;
    assert_eq!(
        entries(&lenient).len(),
        4,
        "the supported criterion still applies"
    );
    let outcome = lenient["entry"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["search"]["mode"] == "outcome")
        .expect("a warning entry");
    let text = outcome["resource"].to_string();
    assert!(
        text.contains("entity-name") && text.contains("_content"),
        "{text}"
    );
    let self_link = lenient["link"][0]["url"].as_str().unwrap();
    assert!(
        self_link.contains("action=C") && !self_link.contains("entity-name"),
        "{self_link}"
    );

    let strict = f
        .server
        .get("/AuditEvent?entity-name=x")
        .add_header(X_TENANT_ID, f.t())
        .add_header(PREFER, HeaderValue::from_static("handling=strict"))
        .await;
    strict.assert_status(StatusCode::BAD_REQUEST);
    f.get("/AuditEvent?date=whenever")
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    f.get("/AuditEvent?patient:identifier=x")
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let sorted = f.bundle("/AuditEvent?_sort=outcome").await;
    assert!(
        sorted
            .to_string()
            .contains("_sort value 'outcome' is not supported")
    );
    f.server
        .get("/AuditEvent?_sort=outcome")
        .add_header(X_TENANT_ID, f.t())
        .add_header(PREFER, HeaderValue::from_static("handling=strict"))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn events_are_readable_by_id_and_immutable_through_the_api() {
    let f = fixture_or_skip!("immutable", true);
    f.seed_clinical().await;
    let all = f.bundle("/AuditEvent?entity=Observation/o1").await;
    let event = entries(&all)[0].clone();
    let id = event["id"].as_str().unwrap();

    let read = f.get(&format!("/AuditEvent/{id}")).await;
    read.assert_status_ok();
    assert_eq!(read.header("etag").to_str().unwrap(), "W/\"1\"");
    assert_eq!(read.json::<Value>(), event);
    let vread = f.get(&format!("/AuditEvent/{id}/_history/1")).await;
    vread.assert_status_ok();
    assert_eq!(vread.json::<Value>()["id"], id);
    f.get(&format!("/AuditEvent/{id}/_history/2"))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    f.get("/AuditEvent/no-such-event")
        .await
        .assert_status(StatusCode::NOT_FOUND);

    let mut tampered = event.clone();
    tampered["outcome"] = json!("8");
    for resp in [
        f.server
            .put(&format!("/AuditEvent/{id}"))
            .add_header(X_TENANT_ID, f.t())
            .json(&tampered)
            .await,
        f.server
            .delete(&format!("/AuditEvent/{id}"))
            .add_header(X_TENANT_ID, f.t())
            .await,
        f.server
            .post("/AuditEvent")
            .add_header(X_TENANT_ID, f.t())
            .json(&tampered)
            .await,
    ] {
        assert!(
            resp.status_code().is_client_error(),
            "the trail is append-only: {}",
            resp.status_code()
        );
    }
    let again = f.get(&format!("/AuditEvent/{id}")).await.json::<Value>();
    assert_eq!(again, event);
    assert_eq!(
        f.sql_count(
            "SELECT count(*) FROM resources WHERE tenant_id = $1 AND resource_type = 'AuditEvent'"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn a_tenant_sees_only_its_own_trail() {
    let f = fixture_or_skip!("tenants", true);
    f.seed_clinical().await;
    let other = format!("{}_other", f.tenant);
    let other_h = || HeaderValue::from_str(&other).unwrap();
    f.server
        .put("/Patient/p1")
        .add_header(X_TENANT_ID, other_h())
        .json(&json!({"resourceType": "Patient", "id": "p1"}))
        .await;
    let mine = f.bundle("/AuditEvent?patient=p1&_count=100").await;
    let theirs: Value = f
        .server
        .get("/AuditEvent?patient=p1&_count=100")
        .add_header(X_TENANT_ID, other_h())
        .await
        .json();
    assert_eq!(entries(&mine).len(), 2);
    assert_eq!(entries(&theirs).len(), 1);
    let their_id = entries(&theirs)[0]["id"].as_str().unwrap().to_string();
    f.get(&format!("/AuditEvent/{their_id}"))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    f.server
        .get(&format!("/AuditEvent/{their_id}"))
        .add_header(X_TENANT_ID, other_h())
        .await
        .assert_status_ok();
}

#[tokio::test]
async fn events_written_before_the_tables_stay_visible_in_one_trail() {
    let f = fixture_or_skip!("union", true);
    let ctx = TenantContext::new(TenantId::new(&f.tenant), TenantPermissions::full_access());
    // The trail as the previous layout left it: AuditEvent resources.
    for i in 0..9 {
        let recorded =
            chrono::Utc::now() - chrono::Duration::days(30) + chrono::Duration::minutes(i);
        f.backend
            .create(
                &ctx,
                "AuditEvent",
                json!({
                    "resourceType": "AuditEvent", "id": format!("old-{i}"),
                    "type": {"system": "http://terminology.hl7.org/CodeSystem/audit-event-type", "code": "rest"},
                    "subtype": [{"system": "http://hl7.org/fhir/restful-interaction", "code": "read"}],
                    "action": "R", "outcome": "0",
                    "recorded": recorded.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    "agent": [{"requestor": true, "who": {"display": "old"}}],
                    "source": {"observer": {"reference": "Device/hfs"}},
                    "entity": [{"what": {"reference": "Patient/p1"}}]
                }),
                FhirVersion::default(),
            )
            .await
            .unwrap();
    }
    f.seed_clinical().await; // 4 new events, 2 of them about Patient/p1

    let summary = f
        .bundle("/AuditEvent?patient=Patient/p1&_summary=count")
        .await;
    assert_eq!(summary["total"], 11);

    let mut seen = Vec::new();
    let mut url = Some("/AuditEvent?patient=Patient/p1&_count=4".to_string());
    while let Some(u) = url {
        let page = f.bundle(&u).await;
        seen.extend(
            entries(&page)
                .iter()
                .map(|e| e["id"].as_str().unwrap().to_string()),
        );
        url = next_link(&page);
        assert!(seen.len() <= 12, "paging repeats events: {seen:?}");
    }
    // The two writes about the patient and the count search above (a search
    // by patient is itself an event about that patient); the page requests
    // themselves are newer than the first page and so never appear.
    assert_eq!(seen.len(), 12);
    assert!(
        seen[..3].iter().all(|id| !id.starts_with("old-")),
        "new events first: {seen:?}"
    );
    assert_eq!(
        &seen[3..],
        [
            "old-8", "old-7", "old-6", "old-5", "old-4", "old-3", "old-2", "old-1", "old-0"
        ]
    );

    let asc = f
        .bundle("/AuditEvent?patient=Patient/p1&_sort=date&_count=3")
        .await;
    let ids: Vec<&str> = entries(&asc)
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["old-0", "old-1", "old-2"]);

    // Legacy events are still read by id, and still cannot be changed.
    f.get("/AuditEvent/old-3").await.assert_status_ok();
    assert!(
        f.server
            .delete("/AuditEvent/old-3")
            .add_header(X_TENANT_ID, f.t())
            .await
            .status_code()
            .is_client_error()
    );
}

#[tokio::test]
async fn compartment_walks_leave_the_audit_trail_out_unless_asked() {
    for table in [true, false] {
        let f = fixture_or_skip!("everything", table);
        f.seed_clinical().await;
        f.get("/Observation/o1").await.assert_status_ok();

        let types = |bundle: &Value| -> Vec<String> {
            let mut t: Vec<String> = entries(bundle)
                .iter()
                .map(|e| e["resourceType"].as_str().unwrap().to_string())
                .collect();
            t.sort();
            t.dedup();
            t
        };
        let everything = f.bundle("/Patient/p1/$everything").await;
        assert_eq!(
            types(&everything),
            ["Observation", "Patient"],
            "table={table}"
        );
        let wildcard = f.bundle("/Patient/p1/*").await;
        assert_eq!(types(&wildcard), ["Observation"], "table={table}");

        // Naming the type is the opt-in.
        let explicit = f.bundle("/Patient/p1/AuditEvent?_count=100").await;
        let events = entries(&explicit);
        assert!(events.len() >= 3, "table={table}: {explicit}");
        assert!(events.iter().all(|e| e["resourceType"] == "AuditEvent"));
        assert!(
            events
                .iter()
                .all(|e| e["entity"].to_string().contains("Patient/p1")),
            "only this patient's events (table={table})"
        );
    }
}

#[tokio::test]
async fn the_resource_store_escape_hatch_keeps_the_previous_behaviour() {
    let f = fixture_or_skip!("resource", false);
    f.seed_clinical().await;
    assert_eq!(
        f.sql_count("SELECT count(*) FROM audit_event WHERE tenant_id = $1")
            .await,
        0
    );
    assert_eq!(
        f.sql_count(
            "SELECT count(*) FROM resources WHERE tenant_id = $1 AND resource_type = 'AuditEvent'"
        )
        .await,
        4
    );
    let all = f.bundle("/AuditEvent?_count=100").await;
    assert_eq!(entries(&all).len(), 4);
    assert!(
        entries(&all)[0]["meta"]["profile"]
            .to_string()
            .contains(balp::profiles::PATIENT_UPDATE)
            || entries(&all)[0]["meta"]["profile"]
                .to_string()
                .contains("IHE.BasicAudit")
    );
}

#[tokio::test]
async fn history_capabilities_and_counts_reflect_the_audit_store() {
    let f = fixture_or_skip!("surface", true);
    f.seed_clinical().await;
    f.get("/Observation/o1").await.assert_status_ok();

    // Reference + another criterion (the lookup-driven plan with a probe filter).
    let reads = f
        .bundle("/AuditEvent?patient=Patient/p1&action=R&_count=50")
        .await;
    assert_eq!(entries(&reads).len(), 1);
    let writes = f
        .bundle("/AuditEvent?patient=Patient/p1&action=C&outcome=0&subtype=create&_total=accurate")
        .await;
    assert_eq!(entries(&writes).len(), 2);
    assert_eq!(writes["total"], 2);
    assert_eq!(
        entries(&f.bundle("/AuditEvent?outcome:not=0").await).len(),
        0
    );
    f.get("/Observation/none")
        .await
        .assert_status(StatusCode::NOT_FOUND);
    assert_eq!(
        entries(&f.bundle("/AuditEvent?outcome:not=0").await).len(),
        1
    );

    // Instance history: the one version of an immutable event.
    let id = entries(&reads)[0]["id"].as_str().unwrap().to_string();
    let history = f.bundle(&format!("/AuditEvent/{id}/_history")).await;
    assert_eq!(history["type"], "history");
    let versions = history["entry"].as_array().unwrap();
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0]["resource"]["id"], id.as_str());
    assert_eq!(versions[0]["resource"]["meta"]["versionId"], "1");
    assert_eq!(versions[0]["request"]["method"], "POST");
    f.get("/AuditEvent/nope/_history")
        .await
        .assert_status(StatusCode::NOT_FOUND);

    // The CapabilityStatement advertises what the store answers, no more.
    let metadata = f.bundle("/metadata").await;
    let audit = metadata["rest"][0]["resource"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["type"] == "AuditEvent")
        .expect("AuditEvent entry");
    let mut names: Vec<&str> = audit["searchParam"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "_id",
            "_lastUpdated",
            "action",
            "agent",
            "date",
            "entity",
            "outcome",
            "patient",
            "subtype",
            "type"
        ]
    );
    assert!(audit.get("searchRevInclude").is_none() && audit.get("searchInclude").is_none());
    let codes: Vec<&str> = audit["interaction"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, ["read", "vread", "history-instance", "search-type"]);
    let patient = metadata["rest"][0]["resource"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["type"] == "Patient")
        .unwrap();
    assert!(
        patient["searchParam"].as_array().unwrap().len() > 10,
        "other types are untouched"
    );

    // The console's per-type totals count the store's events.
    let n = f
        .sql_count("SELECT count(*) FROM audit_event WHERE tenant_id = $1")
        .await;
    let counts = f
        .bundle("/console/metrics/resource-counts?types=AuditEvent,Patient")
        .await;
    let series = counts["series"].as_array().unwrap();
    let total = |t: &str| series.iter().find(|s| s["resource_type"] == t).unwrap()["total"].clone();
    assert!(total("AuditEvent").as_i64().unwrap() >= n, "{counts}");
    assert_eq!(total("Patient"), 2);
}
