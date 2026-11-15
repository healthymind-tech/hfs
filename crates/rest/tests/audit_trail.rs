//! End to end: every REST interaction leaves a BALP `AuditEvent` in the tenant
//! it addressed, the events are searchable through the FHIR API, and clients
//! cannot alter them.
//!
//! The audit store is the same SQLite backend the API serves (the production
//! "shared" database mode), written through `DatabaseSink` exactly as `hfs`
//! does it.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum_test::TestServer;
use helios_audit::sinks::database::AuditStorage;
use helios_audit::{AuditConfig, AuditMiddlewareState, DatabaseSink, ExclusionFilter, balp};
use helios_auth::{AuthConfig, AuthError, AuthProvider, Principal, ScopeSet};
use helios_fhir::FhirVersion;
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use helios_persistence::core::ResourceStorage;
use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
use helios_rest::{AuthMiddlewareState, ServerConfig};
use serde_json::{Value, json};

const X_TENANT_ID: HeaderName = HeaderName::from_static("x-tenant-id");
const AUTHORIZATION: HeaderName = HeaderName::from_static("authorization");
const BASE: &str = "http://fhir.test/fhir";

/// The `hfs` adapter: request tenant, else the server's default tenant.
struct Adapter(SqliteBackend);

#[async_trait]
impl AuditStorage for Adapter {
    async fn create_resource(
        &self,
        tenant: Option<&str>,
        resource_type: &str,
        resource: Value,
        fhir_version: FhirVersion,
    ) -> Result<(), String> {
        let ctx = TenantContext::new(
            TenantId::new(tenant.unwrap_or(&ServerConfig::for_testing().default_tenant)),
            TenantPermissions::full_access(),
        );
        self.0
            .create(&ctx, resource_type, resource, fhir_version)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Accepts `Bearer reader` (read-only on Observation) and `Bearer writer`.
struct StubProvider;

#[async_trait]
impl AuthProvider for StubProvider {
    async fn authenticate(&self, header: &str) -> Result<Principal, AuthError> {
        let scopes = match header {
            "Bearer writer" => "user/*.cruds",
            "Bearer reader" => "user/Observation.rs",
            _ => return Err(AuthError::InvalidSignature),
        };
        let mut claims = serde_json::Map::new();
        claims.insert("fhirUser".into(), "Practitioner/pr1".into());
        claims.insert("name".into(), "Dr. Lin".into());
        claims.insert("azp".into(), "ward-app".into());
        Ok(Principal {
            subject: "u-lin".into(),
            issuer: "https://idp.test".into(),
            tenant_id: None,
            scopes: ScopeSet::parse(scopes),
            jti: None,
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
            custom_claims: claims,
        })
    }

    fn name(&self) -> &str {
        "stub"
    }
}

fn backend() -> SqliteBackend {
    let data_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("data"))
        .unwrap();
    let backend = SqliteBackend::with_config(
        ":memory:",
        SqliteBackendConfig {
            data_dir: Some(data_dir),
            ..Default::default()
        },
    )
    .expect("sqlite");
    backend.init_schema().expect("schema");
    backend
}

fn server(with_auth: bool) -> TestServer {
    let backend = backend();
    let sink = Arc::new(DatabaseSink::new(
        Arc::new(Adapter(backend.clone())),
        FhirVersion::default(),
    ));
    let audit =
        Arc::new(AuditMiddlewareState::new(sink, AuditConfig::default()).with_server_address(BASE));
    let auth = with_auth.then(|| {
        Arc::new(AuthMiddlewareState {
            provider: Arc::new(StubProvider),
            config: Arc::new(AuthConfig::default()),
            audit_sink: Arc::new(helios_audit::NullSink),
            audit_source_observer: "Device/hfs".into(),
            audit_exclusion_filter: ExclusionFilter::new(vec![]),
            tenant_url_routing: false,
            sessions: None,
            admin: None,
        })
    });
    let config = ServerConfig {
        base_url: BASE.to_string(),
        seed_conformance: false,
        ..ServerConfig::for_testing()
    };
    let app = helios_rest::create_app_with_auth(
        backend,
        config,
        AuthConfig::default(),
        auth,
        Some(audit),
    );
    TestServer::new(app).expect("server")
}

/// All AuditEvents of a tenant, in recording order (read straight from the
/// API, which is itself audited — callers filter what they look for).
async fn events(server: &TestServer, tenant: &str, bearer: Option<&str>) -> Vec<Value> {
    let mut req = server.get("/AuditEvent?_count=1000");
    if !tenant.is_empty() {
        req = req.add_header(X_TENANT_ID, HeaderValue::from_str(tenant).unwrap());
    }
    if let Some(b) = bearer {
        req = req.add_header(AUTHORIZATION, HeaderValue::from_str(b).unwrap());
    }
    let resp = req.await;
    resp.assert_status_ok();
    let mut out: Vec<Value> = resp.json::<Value>()["entry"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|e| e["resource"].clone())
        .collect();
    out.sort_by(|a, b| a["recorded"].as_str().cmp(&b["recorded"].as_str()));
    out
}

fn subtype(e: &Value) -> &str {
    e["subtype"][0]["code"].as_str().unwrap_or("")
}

fn profile(e: &Value) -> &str {
    e["meta"]["profile"][0].as_str().unwrap_or("")
}

fn entity_refs(e: &Value) -> Vec<String> {
    e["entity"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|en| en["what"]["reference"].as_str().map(str::to_string))
        .collect()
}

fn find(events: &[Value], pred: impl Fn(&Value) -> bool) -> &Value {
    events
        .iter()
        .find(|e| pred(e))
        .unwrap_or_else(|| panic!("no matching event in {events:#?}"))
}

async fn search_count(server: &TestServer, query: &str) -> usize {
    let resp = server
        .get(&format!("/AuditEvent?{query}&_count=1000"))
        .add_header(X_TENANT_ID, HeaderValue::from_static("default"))
        .await;
    resp.assert_status_ok();
    resp.json::<Value>()["entry"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0)
}

#[tokio::test]
async fn every_interaction_is_recorded_as_balp() {
    let server = server(false);
    let t = || HeaderValue::from_static("default");

    server
        .put("/Patient/p1")
        .add_header(X_TENANT_ID, t())
        .json(&json!({"resourceType": "Patient", "id": "p1", "active": true}))
        .await
        .assert_status(StatusCode::CREATED);
    let created = server
        .post("/Observation")
        .add_header(X_TENANT_ID, t())
        .json(&json!({
            "resourceType": "Observation", "status": "final",
            "code": {"text": "pulse"}, "subject": {"reference": "Patient/p1"}
        }))
        .await;
    created.assert_status(StatusCode::CREATED);
    let obs_id = created.json::<Value>()["id"].as_str().unwrap().to_string();

    server
        .get(&format!("/Observation/{obs_id}"))
        .add_header(X_TENANT_ID, t())
        .await
        .assert_status_ok();
    server
        .get(&format!("/Observation/{obs_id}/_history/1"))
        .add_header(X_TENANT_ID, t())
        .await
        .assert_status_ok();
    server
        .get("/Observation?patient=p1&code:text=pulse")
        .add_header(X_TENANT_ID, t())
        .await
        .assert_status_ok();
    server
        .patch("/Patient/p1")
        .add_header(X_TENANT_ID, t())
        .add_header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json-patch+json"),
        )
        .bytes(
            br#"[{"op":"replace","path":"/active","value":false}]"#
                .to_vec()
                .into(),
        )
        .await
        .assert_status_ok();
    server
        .get("/Patient/p1/_history")
        .add_header(X_TENANT_ID, t())
        .await
        .assert_status_ok();
    server
        .get("/Patient/p1/$everything")
        .add_header(X_TENANT_ID, t())
        .await
        .assert_status_ok();
    server
        .get("/Basic/missing")
        .add_header(X_TENANT_ID, t())
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .delete(&format!("/Observation/{obs_id}"))
        .add_header(X_TENANT_ID, t())
        .await;

    let ev = events(&server, "default", None).await;

    let e = find(&ev, |e| {
        subtype(e) == "create" && entity_refs(e).iter().any(|r| r == "Patient/p1")
    });
    assert_eq!(
        profile(e),
        balp::profiles::PATIENT_CREATE,
        "update-as-create"
    );
    assert_eq!(e["action"], "C");

    let obs_ref = format!("Observation/{obs_id}");
    let e = find(&ev, |e| {
        subtype(e) == "create" && entity_refs(e).contains(&obs_ref)
    });
    assert_eq!(profile(e), balp::profiles::PATIENT_CREATE);
    assert!(entity_refs(e).contains(&"Patient/p1".to_string()));

    let e = find(&ev, |e| subtype(e) == "read");
    assert_eq!(
        profile(e),
        balp::profiles::PATIENT_READ,
        "read is patient-centric"
    );
    assert_eq!(
        entity_refs(e),
        vec![obs_ref.clone(), "Patient/p1".to_string()]
    );
    assert!(
        e["entity"][2]["what"]["identifier"]["value"].is_string(),
        "X-Request-Id entity"
    );

    let e = find(&ev, |e| subtype(e) == "vread");
    assert_eq!(profile(e), balp::profiles::PATIENT_READ);
    assert_eq!(entity_refs(e)[0], format!("{obs_ref}/_history/1"));

    let e = find(&ev, |e| subtype(e) == "search-type");
    assert_eq!(profile(e), balp::profiles::PATIENT_QUERY);
    assert_eq!(
        e["entity"][0]["description"],
        format!("GET {BASE}/Observation?patient=p1&code:text=pulse")
    );
    assert!(e["entity"][0]["query"].is_string());

    let e = find(&ev, |e| subtype(e) == "patch");
    assert_eq!(profile(e), balp::profiles::PATIENT_UPDATE);
    assert_eq!(e["action"], "U");

    let e = find(&ev, |e| subtype(e) == "history-instance");
    assert_eq!(profile(e), balp::profiles::PATIENT_QUERY);

    let e = find(&ev, |e| subtype(e) == "operation");
    assert_eq!(profile(e), balp::profiles::PATIENT_QUERY, "$everything");
    assert!(
        e["entity"][0]["description"]
            .as_str()
            .unwrap()
            .ends_with("/$everything")
    );

    let e = find(&ev, |e| subtype(e) == "delete");
    assert_eq!(profile(e), balp::profiles::PATIENT_DELETE);
    assert_eq!(e["action"], "D");

    let e = find(&ev, |e| e["outcome"] == "4");
    assert_eq!(subtype(e), "read");
    assert!(e.get("meta").is_none() || e["meta"].get("profile").is_none());
    assert!(
        e["outcomeDesc"].as_str().unwrap().starts_with("HTTP 404"),
        "{e}"
    );

    // Every event names the client and the server.
    for e in &ev {
        assert!(e["agent"].as_array().unwrap().len() >= 2, "{e}");
        assert_eq!(e["agent"][1]["network"]["address"], BASE);
        assert_eq!(e["source"]["observer"]["reference"], "Device/hfs");
    }
}

#[tokio::test]
async fn audit_events_are_searchable() {
    let server = server(false);
    let t = || HeaderValue::from_static("default");
    server
        .put("/Patient/p2")
        .add_header(X_TENANT_ID, t())
        .json(&json!({"resourceType": "Patient", "id": "p2"}))
        .await;
    server.get("/Patient/p2").add_header(X_TENANT_ID, t()).await;
    server
        .get("/Patient/nope")
        .add_header(X_TENANT_ID, t())
        .await;

    // (Searching the trail by patient is itself a patient query, so the
    // interaction filter keeps the earlier searches out of the counts.)
    assert_eq!(
        search_count(&server, "patient=Patient/p2&action=C,R").await,
        2
    );
    assert_eq!(
        search_count(&server, "entity=Patient/p2&action=C,R").await,
        2
    );
    assert!(search_count(&server, "patient=Patient/p2&subtype=search-type").await >= 1);
    assert_eq!(search_count(&server, "subtype=read").await, 2);
    assert_eq!(search_count(&server, "action=R").await, 2);
    assert_eq!(search_count(&server, "outcome=4").await, 1);
    assert!(search_count(&server, "type=rest").await >= 3);
    assert!(search_count(&server, "date=ge2020-01-01").await >= 3);
    assert_eq!(search_count(&server, "date=lt2020-01-01").await, 0);
    assert_eq!(
        search_count(
            &server,
            "subtype=http://hl7.org/fhir/restful-interaction|create"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn audit_events_are_per_tenant() {
    let server = server(false);
    server
        .put("/Basic/b1")
        .add_header(X_TENANT_ID, HeaderValue::from_static("hospital-a"))
        .json(&json!({"resourceType": "Basic", "id": "b1", "code": {"text": "x"}}))
        .await;
    let a = events(&server, "hospital-a", None).await;
    assert!(
        a.iter()
            .any(|e| entity_refs(e).contains(&"Basic/b1".to_string()))
    );
    let b = events(&server, "hospital-b", None).await;
    assert!(
        !b.iter()
            .any(|e| entity_refs(e).contains(&"Basic/b1".to_string())),
        "another tenant never sees the trail"
    );
}

#[tokio::test]
async fn clients_cannot_alter_the_trail() {
    let server = server(false);
    let t = || HeaderValue::from_static("default");
    server.get("/Patient/x").add_header(X_TENANT_ID, t()).await;
    let ev = events(&server, "default", None).await;
    let id = ev[0]["id"].as_str().unwrap().to_string();
    let mut changed = ev[0].clone();
    changed["outcome"] = json!("0");

    server
        .put(&format!("/AuditEvent/{id}"))
        .add_header(X_TENANT_ID, t())
        .json(&changed)
        .await
        .assert_status(StatusCode::METHOD_NOT_ALLOWED);
    server
        .delete(&format!("/AuditEvent/{id}"))
        .add_header(X_TENANT_ID, t())
        .await
        .assert_status(StatusCode::METHOD_NOT_ALLOWED);
    let patch = server
        .patch(&format!("/AuditEvent/{id}"))
        .add_header(X_TENANT_ID, t())
        .add_header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json-patch+json"),
        )
        .bytes(
            br#"[{"op":"replace","path":"/outcome","value":"0"}]"#
                .to_vec()
                .into(),
        )
        .await;
    assert!(
        patch.status_code().is_client_error(),
        "{}",
        patch.status_code()
    );
    let purge = server
        .delete(&format!("/AuditEvent/{id}/$purge"))
        .add_header(X_TENANT_ID, t())
        .await;
    assert!(purge.status_code().is_client_error() || purge.status_code().is_server_error());
    // A transaction cannot do it either.
    server
        .post("/")
        .add_header(X_TENANT_ID, t())
        .json(&json!({
            "resourceType": "Bundle", "type": "transaction",
            "entry": [{"request": {"method": "DELETE", "url": format!("AuditEvent/{id}")}}]
        }))
        .await;

    let still = server
        .get(&format!("/AuditEvent/{id}"))
        .add_header(X_TENANT_ID, t())
        .await;
    still.assert_status_ok();
    assert_eq!(still.json::<Value>()["outcome"], ev[0]["outcome"]);
}

#[tokio::test]
async fn bundles_are_recorded_per_entry() {
    let server = server(false);
    server
        .post("/")
        .add_header(X_TENANT_ID, HeaderValue::from_static("default"))
        .json(&json!({
            "resourceType": "Bundle", "type": "transaction",
            "entry": [
                {"fullUrl": "urn:uuid:1", "resource": {"resourceType": "Patient"},
                 "request": {"method": "POST", "url": "Patient"}},
                {"resource": {"resourceType": "Basic", "id": "bb", "code": {"text": "y"}},
                 "request": {"method": "PUT", "url": "Basic/bb"}}
            ]
        }))
        .await
        .assert_status_ok();
    let ev = events(&server, "default", None).await;
    let entries: Vec<&Value> = ev
        .iter()
        .filter(|e| {
            e["entity"].as_array().unwrap().iter().any(|en| {
                en["detail"]
                    .as_array()
                    .is_some_and(|d| d.iter().any(|x| x["valueString"] == "transaction"))
            })
        })
        .collect();
    assert_eq!(entries.len(), 2, "{ev:#?}");
    assert!(entries.iter().all(|e| subtype(e) == "create"));
    assert!(entries.iter().all(|e| !profile(e).is_empty()));
    assert!(
        !ev.iter().any(|e| subtype(e) == "transaction"),
        "no envelope on success"
    );
}

#[tokio::test]
async fn refused_requests_and_the_user_are_recorded() {
    let server = server(true);
    server
        .get("/Observation")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .get("/Patient/p1")
        .add_header(AUTHORIZATION, HeaderValue::from_static("Bearer reader"))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .get("/Observation?patient=p1")
        .add_header(AUTHORIZATION, HeaderValue::from_static("Bearer reader"))
        .await
        .assert_status_ok();

    let ev = events(&server, "", Some("Bearer writer")).await;
    let unauthorized = find(&ev, |e| {
        e["outcomeDesc"]
            .as_str()
            .is_some_and(|d| d.starts_with("HTTP 401"))
    });
    assert_eq!(unauthorized["outcome"], "4");
    assert_eq!(subtype(unauthorized), "search-type");
    assert_eq!(
        unauthorized["agent"].as_array().unwrap().len(),
        2,
        "no user"
    );

    let forbidden = find(&ev, |e| {
        e["outcomeDesc"]
            .as_str()
            .is_some_and(|d| d.starts_with("HTTP 403"))
    });
    assert_eq!(forbidden["outcome"], "4");
    assert_eq!(forbidden["agent"][2]["who"]["identifier"]["value"], "u-lin");
    assert!(entity_refs(forbidden).contains(&"Patient/p1".to_string()));

    let ok = find(&ev, |e| e["outcome"] == "0" && subtype(e) == "search-type");
    assert_eq!(profile(ok), balp::profiles::PATIENT_QUERY);
    let user = &ok["agent"][2];
    assert_eq!(user["type"]["coding"][0]["code"], "IRCP");
    assert_eq!(user["who"]["reference"], "Practitioner/pr1");
    assert_eq!(user["name"], "Dr. Lin");
    assert_eq!(ok["agent"][0]["who"]["identifier"]["value"], "ward-app");
    assert_eq!(ok["agent"][0]["requestor"], false);

    // The user is searchable as the agent.
    let resp = server
        .get("/AuditEvent?agent=Practitioner/pr1&_count=100")
        .add_header(AUTHORIZATION, HeaderValue::from_static("Bearer writer"))
        .await;
    resp.assert_status_ok();
    assert!(
        resp.json::<Value>()["entry"]
            .as_array()
            .is_some_and(|e| e.len() >= 2)
    );
}
