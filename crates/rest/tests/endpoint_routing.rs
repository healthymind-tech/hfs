//! FHIR server manager — endpoint runtime (package P1) router integration.
//!
//! The full app (`build_app` through `create_app_with_auth_bulk_settings_and_ops`)
//! over SQLite with an `EndpointRuntime` on a memory store: the default
//! endpoint plus named endpoints served at `{base}/{path}`. Checks URL
//! correctness (Location, bundle links, fullUrl, CapabilityStatement,
//! history, transaction, `$everything`, bulk export), the one-door rule,
//! 503/404, CORS and validation per endpoint, and hot reload.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum_test::TestServer;
use helios_endpoints::model::{ProfileRequirement, TerminologyValidation};
use helios_endpoints::{
    Actor, CorsSettings, EndpointName, EndpointPath, EndpointRegistry, EndpointSpec,
    EndpointStatus, EndpointStore, FhirRelease, MemoryEndpointStore, ValidationMode,
    ValidationSettings, ensure_default_endpoint,
};
use helios_fhir::FhirVersion;
use helios_persistence::backends::local_fs::LocalFsOutputStore;
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use helios_persistence::core::{
    BulkExportJobStore, DefaultExportWorker, ExportClaimStrategy, ExportOutputStore,
    ResourceStorage, WorkerId,
};
use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
use helios_rest::endpoint::EndpointRuntime;
use helios_rest::{BulkExportBundle, OperationsBundle, ServerConfig};
use serde_json::{Value, json};

const BASE: &str = "https://hfs.example/fhir";

struct Harness {
    server: TestServer,
    backend: Arc<SqliteBackend>,
    output: Arc<LocalFsOutputStore>,
    runtime: Option<EndpointRuntime>,
    store: Arc<MemoryEndpointStore>,
    _tmp: tempfile::TempDir,
}

fn named(name: &str) -> EndpointSpec {
    EndpointSpec::named(EndpointName::parse(name).unwrap(), name)
}

/// The standard endpoint set: `default`; `lab` served at `/laboratory`
/// (path ≠ tenant on purpose); `strict` with enforced validation and a
/// required profile; `app` with its own CORS; `off` disabled.
async fn standard_store() -> Arc<MemoryEndpointStore> {
    let store = Arc::new(MemoryEndpointStore::new());
    ensure_default_endpoint(store.as_ref(), "default", FhirRelease::R4)
        .await
        .unwrap();
    let a = Actor::bootstrap();
    let mut lab = named("lab");
    lab.path = EndpointPath::parse("laboratory").unwrap();
    store.create(lab, &a).await.unwrap();
    let mut strict = named("strict");
    strict.validation = Some(ValidationSettings {
        mode: ValidationMode::Enforce,
        terminology: TerminologyValidation::Off,
        stored_profiles: true,
        required_profiles: vec![ProfileRequirement {
            resource_type: "Observation".into(),
            profile: "http://hl7.org/fhir/StructureDefinition/vitalsigns".into(),
        }],
    });
    store.create(strict, &a).await.unwrap();
    let mut app = named("app");
    app.cors = Some(CorsSettings {
        enabled: true,
        allowed_origins: vec!["https://app.example".into()],
        allow_credentials: false,
        extra_headers: vec![],
    });
    store.create(app, &a).await.unwrap();
    let mut off = named("off");
    off.status = EndpointStatus::Disabled;
    store.create(off, &a).await.unwrap();
    store
}

async fn harness_with(store: Option<Arc<MemoryEndpointStore>>) -> Harness {
    let data_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("data"));
    let backend = Arc::new(
        SqliteBackend::with_config(
            ":memory:",
            SqliteBackendConfig {
                data_dir,
                ..Default::default()
            },
        )
        .unwrap(),
    );
    backend.init_schema().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let output = Arc::new(LocalFsOutputStore::new(
        tmp.path(),
        "https://internal.example",
    ));
    let config = ServerConfig {
        base_url: BASE.to_string(),
        default_tenant: "default".to_string(),
        enable_cors: true,
        cors_origins: "*".to_string(),
        ..ServerConfig::for_testing()
    };
    let runtime = match &store {
        Some(store) => {
            let registry = Arc::new(EndpointRegistry::load(store.as_ref()).await.unwrap());
            Some(EndpointRuntime::new(
                registry,
                Arc::clone(store) as Arc<dyn EndpointStore>,
            ))
        }
        None => None,
    };
    let app = helios_rest::create_app_with_auth_bulk_settings_and_ops(
        Arc::clone(&backend),
        config,
        helios_auth::AuthConfig::default(),
        None,
        None,
        Some(BulkExportBundle {
            jobs: backend.clone() as Arc<dyn BulkExportJobStore>,
            output: output.clone() as Arc<dyn ExportOutputStore>,
            file_auth: Arc::new(helios_rest::bulk_export_auth::BearerScopeAuth),
        }),
        None,
        None,
        OperationsBundle {
            endpoints: runtime.clone(),
            ..Default::default()
        },
    );
    Harness {
        server: TestServer::new(app).unwrap(),
        backend,
        output,
        runtime,
        store: store.unwrap_or_default(),
        _tmp: tmp,
    }
}

async fn harness() -> Harness {
    harness_with(Some(standard_store().await)).await
}

fn tenant(id: &str) -> TenantContext {
    TenantContext::new(TenantId::new(id), TenantPermissions::full_access())
}

fn header(res: &axum_test::TestResponse, name: &str) -> String {
    res.headers()
        .get(name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .to_str()
        .unwrap()
        .to_string()
}

async fn create_patient(h: &Harness, prefix: &str) -> (String, String) {
    let res = h
        .server
        .post(&format!("{prefix}/Patient"))
        .json(&json!({"resourceType": "Patient", "name": [{"family": "Lin"}]}))
        .await;
    assert_eq!(res.status_code(), StatusCode::CREATED, "{}", res.text());
    let body: Value = res.json();
    (
        body["id"].as_str().unwrap().to_string(),
        header(&res, "location"),
    )
}

#[tokio::test]
async fn metadata_per_endpoint() {
    let h = harness().await;
    let d: Value = h.server.get("/metadata").await.json();
    assert_eq!(d["implementation"]["url"], BASE);
    assert_eq!(d["fhirVersion"], "4.0.1");
    let l: Value = h.server.get("/laboratory/metadata").await.json();
    assert_eq!(l["implementation"]["url"], format!("{BASE}/laboratory"));
    assert_eq!(l["fhirVersion"], "4.0.1");
    assert_eq!(l["rest"][0]["security"]["cors"], true);
    // The endpoint is addressed by its path, never its name.
    let by_name = h.server.get("/lab/metadata").await;
    assert_eq!(by_name.status_code(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn crud_location_and_isolation() {
    let h = harness().await;
    let (id, location) = create_patient(&h, "/laboratory").await;
    assert_eq!(
        location,
        format!("{BASE}/laboratory/Patient/{id}/_history/1")
    );
    let read = h.server.get(&format!("/laboratory/Patient/{id}")).await;
    assert_eq!(read.status_code(), StatusCode::OK);
    // Stored in the endpoint's tenant, invisible through the root door.
    assert!(
        h.backend
            .read(&tenant("lab"), "Patient", &id)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        h.server.get(&format!("/Patient/{id}")).await.status_code(),
        StatusCode::NOT_FOUND
    );
    // Update and delete through the endpoint.
    let upd = h
        .server
        .put(&format!("/laboratory/Patient/{id}"))
        .json(&json!({"resourceType": "Patient", "id": id, "active": true}))
        .await;
    assert_eq!(upd.status_code(), StatusCode::OK, "{}", upd.text());
    assert_eq!(
        h.server
            .delete(&format!("/laboratory/Patient/{id}"))
            .await
            .status_code(),
        StatusCode::NO_CONTENT
    );
    // The default endpoint keeps its unprefixed URLs.
    let (did, dloc) = create_patient(&h, "").await;
    assert_eq!(dloc, format!("{BASE}/Patient/{did}/_history/1"));
}

#[tokio::test]
async fn search_links_full_urls_and_history() {
    let h = harness().await;
    for _ in 0..3 {
        create_patient(&h, "/laboratory").await;
    }
    let res = h.server.get("/laboratory/Patient?_count=2").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let b: Value = res.json();
    let links = b["link"].as_array().unwrap();
    for l in links {
        assert!(
            l["url"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{BASE}/laboratory/Patient")),
            "{l}"
        );
    }
    assert!(links.iter().any(|l| l["relation"] == "next"));
    for e in b["entry"].as_array().unwrap() {
        assert!(
            e["fullUrl"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{BASE}/laboratory/Patient/"))
        );
    }
    // The next link is followable through the endpoint.
    let next = links.iter().find(|l| l["relation"] == "next").unwrap()["url"]
        .as_str()
        .unwrap()
        .strip_prefix(BASE)
        .unwrap()
        .to_string();
    assert_eq!(h.server.get(&next).await.status_code(), StatusCode::OK);

    let hist: Value = h.server.get("/laboratory/Patient/_history").await.json();
    assert_eq!(hist["type"], "history");
    for e in hist["entry"].as_array().unwrap() {
        assert!(
            e["fullUrl"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{BASE}/laboratory/"))
        );
    }
    let sys: Value = h.server.get("/laboratory/_history").await.json();
    assert!(
        sys["link"][0]["url"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{BASE}/laboratory/_history"))
    );
    // The default endpoint's history does not contain the lab's writes.
    let root: Value = h.server.get("/_history").await.json();
    assert_eq!(root["entry"].as_array().map_or(0, Vec::len), 0);
}

#[tokio::test]
async fn transaction_and_everything() {
    let h = harness().await;
    let bundle = json!({
        "resourceType": "Bundle",
        "type": "transaction",
        "entry": [
            {"fullUrl": "urn:uuid:11111111-1111-1111-1111-111111111111",
             "resource": {"resourceType": "Patient", "name": [{"family": "Wu"}]},
             "request": {"method": "POST", "url": "Patient"}},
            {"resource": {"resourceType": "Observation", "status": "final",
                          "code": {"text": "note"},
                          "subject": {"reference": "urn:uuid:11111111-1111-1111-1111-111111111111"}},
             "request": {"method": "POST", "url": "Observation"}}
        ]
    });
    let res = h.server.post("/laboratory").json(&bundle).await;
    assert_eq!(res.status_code(), StatusCode::OK, "{}", res.text());
    let b: Value = res.json();
    let mut patient_id = String::new();
    for e in b["entry"].as_array().unwrap() {
        // Entry locations are relative (`Patient/1/_history/1`, as without
        // the manager), so they resolve against the endpoint's base; an
        // absolute one must carry the endpoint prefix.
        let loc = e["response"]["location"].as_str().unwrap();
        assert!(
            !loc.starts_with("http") || loc.starts_with(&format!("{BASE}/laboratory/")),
            "{loc}"
        );
        if loc.contains("Patient/") {
            patient_id = loc.split('/').rev().nth(2).unwrap().to_string();
        }
    }
    let ev = h
        .server
        .get(&format!("/laboratory/Patient/{patient_id}/$everything"))
        .await;
    assert_eq!(ev.status_code(), StatusCode::OK, "{}", ev.text());
    let ev: Value = ev.json();
    let entries = ev["entry"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    for e in entries {
        assert!(
            e["fullUrl"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{BASE}/laboratory/"))
        );
    }
}

#[tokio::test]
async fn one_door_per_tenant() {
    let h = harness().await;
    // Root door, header naming a tenant bound to a named endpoint: refused.
    let r = h
        .server
        .get("/Patient")
        .add_header("x-tenant-id", "lab")
        .await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN);
    assert!(r.text().contains("/laboratory"), "{}", r.text());
    // Unbound tenant through the header: still allowed (legacyTenantHeader).
    let r = h
        .server
        .get("/Patient")
        .add_header("x-tenant-id", "legacy-t1")
        .await;
    assert_eq!(r.status_code(), StatusCode::OK, "{}", r.text());
    // Named endpoint: a header naming another tenant is refused, its own is fine.
    let r = h
        .server
        .get("/laboratory/Patient")
        .add_header("x-tenant-id", "default")
        .await;
    assert_eq!(r.status_code(), StatusCode::BAD_REQUEST);
    let r = h
        .server
        .get("/laboratory/Patient")
        .add_header("x-tenant-id", "lab")
        .await;
    assert_eq!(r.status_code(), StatusCode::OK);

    // Turning legacyTenantHeader off closes the header door for unbound tenants.
    let d = h.store.get("default").await.unwrap().unwrap();
    let mut spec = d.spec.clone();
    spec.legacy_tenant_header = false;
    h.store
        .update("default", d.revision, spec, &Actor::bootstrap())
        .await
        .unwrap();
    h.runtime.as_ref().unwrap().reload_if_stale().await.unwrap();
    let r = h
        .server
        .get("/Patient")
        .add_header("x-tenant-id", "legacy-t1")
        .await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN);
    assert_eq!(h.server.get("/Patient").await.status_code(), StatusCode::OK);
}

#[tokio::test]
async fn disabled_unknown_and_hot_reload() {
    let h = harness().await;
    let r = h.server.get("/off/Patient").await;
    assert_eq!(r.status_code(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(r.text().contains("transient"));
    // Unknown first segments belong to the default endpoint (404 as a type).
    assert_eq!(
        h.server.get("/nope/Patient").await.status_code(),
        StatusCode::NOT_FOUND
    );

    // Create an endpoint at runtime: served after the reload, no restart.
    h.store
        .create(named("fresh"), &Actor::bootstrap())
        .await
        .unwrap();
    h.runtime.as_ref().unwrap().reload_if_stale().await.unwrap();
    let (id, loc) = create_patient(&h, "/fresh").await;
    assert_eq!(loc, format!("{BASE}/fresh/Patient/{id}/_history/1"));
    // Enable the disabled one; disable the fresh one.
    let off = h.store.get("off").await.unwrap().unwrap();
    h.store
        .set_status(
            "off",
            off.revision,
            EndpointStatus::Enabled,
            &Actor::bootstrap(),
        )
        .await
        .unwrap();
    let fresh = h.store.get("fresh").await.unwrap().unwrap();
    h.store
        .set_status(
            "fresh",
            fresh.revision,
            EndpointStatus::Disabled,
            &Actor::bootstrap(),
        )
        .await
        .unwrap();
    h.runtime.as_ref().unwrap().reload_if_stale().await.unwrap();
    assert_eq!(
        h.server.get("/off/Patient").await.status_code(),
        StatusCode::OK
    );
    assert_eq!(
        h.server.get("/fresh/Patient").await.status_code(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    // Renaming a path moves the endpoint; data stays with the tenant.
    let lab = h.store.get("lab").await.unwrap().unwrap();
    let mut spec = lab.spec.clone();
    spec.path = EndpointPath::parse("lab2").unwrap();
    h.store
        .update("lab", lab.revision, spec, &Actor::bootstrap())
        .await
        .unwrap();
    h.runtime.as_ref().unwrap().reload_if_stale().await.unwrap();
    let (id, loc) = create_patient(&h, "/lab2").await;
    assert_eq!(loc, format!("{BASE}/lab2/Patient/{id}/_history/1"));
    assert!(
        h.backend
            .read(&tenant("lab"), "Patient", &id)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn watcher_reloads_without_explicit_call() {
    let h = harness().await;
    let rt = h.runtime.clone().unwrap();
    let task = tokio::spawn(helios_endpoints::run_watcher(
        Arc::clone(&rt.registry),
        Arc::clone(&rt.store),
        Duration::from_millis(50),
    ));
    h.store
        .create(named("later"), &Actor::bootstrap())
        .await
        .unwrap();
    let mut served = false;
    for _ in 0..40 {
        if h.server.get("/later/metadata").await.status_code() == StatusCode::OK
            && rt.registry.snapshot().by_name("later").is_some()
        {
            served = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    task.abort();
    assert!(served, "endpoint created in the store was not picked up");
}

#[tokio::test]
async fn validation_per_endpoint() {
    let h = harness().await;
    let bad = json!({"resourceType": "Patient", "notAnElement": true});
    // Default endpoint inherits HFS_VALIDATION_MODE (off in tests).
    assert_eq!(
        h.server.post("/Patient").json(&bad).await.status_code(),
        StatusCode::CREATED
    );
    // `strict` enforces.
    let r = h.server.post("/strict/Patient").json(&bad).await;
    assert_eq!(
        r.status_code(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        r.text()
    );
    let ok = json!({"resourceType": "Patient", "active": true});
    assert_eq!(
        h.server
            .post("/strict/Patient")
            .json(&ok)
            .await
            .status_code(),
        StatusCode::CREATED
    );
    // Required profile: an Observation that is not a vital sign is refused on
    // `strict` only.
    let obs = json!({"resourceType": "Observation", "status": "final", "code": {"text": "x"}});
    let r = h.server.post("/strict/Observation").json(&obs).await;
    assert_eq!(
        r.status_code(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        r.text()
    );
    assert_eq!(
        h.server
            .post("/laboratory/Observation")
            .json(&obs)
            .await
            .status_code(),
        StatusCode::CREATED
    );
    // `$validate` runs on every endpoint.
    let r = h
        .server
        .post("/strict/Patient/$validate")
        .json(&json!({"resourceType": "Parameters", "parameter": [{"name": "resource", "resource": bad}]}))
        .await;
    assert_eq!(r.status_code(), StatusCode::OK, "{}", r.text());
    assert!(r.text().contains("OperationOutcome"));
}

#[tokio::test]
async fn cors_per_endpoint() {
    let h = harness().await;
    let get = |path: &'static str, origin: &'static str| {
        let s = &h.server;
        async move {
            s.get(path)
                .add_header("origin", origin)
                .await
                .headers()
                .get("access-control-allow-origin")
                .map(|v| v.to_str().unwrap().to_string())
        }
    };
    assert_eq!(
        get("/metadata", "https://x.example").await.as_deref(),
        Some("*")
    );
    assert_eq!(
        get("/laboratory/metadata", "https://x.example")
            .await
            .as_deref(),
        Some("*")
    );
    assert_eq!(
        get("/app/metadata", "https://app.example").await.as_deref(),
        Some("https://app.example")
    );
    assert_eq!(get("/app/metadata", "https://x.example").await, None);
}

#[tokio::test]
async fn bulk_export_urls() {
    let h = harness().await;
    h.backend
        .create(
            &tenant("lab"),
            "Patient",
            json!({"resourceType": "Patient", "id": "p1"}),
            FhirVersion::default(),
        )
        .await
        .unwrap();
    let kickoff = h
        .server
        .get("/laboratory/$export")
        .add_header("prefer", "respond-async")
        .add_query_param("_type", "Patient")
        .await;
    assert_eq!(
        kickoff.status_code(),
        StatusCode::ACCEPTED,
        "{}",
        kickoff.text()
    );
    let status_url = header(&kickoff, "content-location");
    assert!(
        status_url.starts_with(&format!("{BASE}/laboratory/export-status/")),
        "{status_url}"
    );
    let worker_id = WorkerId::new("test-worker");
    let worker = DefaultExportWorker::new(
        h.backend.clone(),
        h.backend.clone(),
        h.output.clone(),
        worker_id.clone(),
    );
    while let Some(lease) = h
        .backend
        .claim_next(&worker_id, Duration::from_secs(60), 3)
        .await
        .unwrap()
    {
        worker.run_job(lease).await.unwrap();
    }
    let done = h.server.get(status_url.strip_prefix(BASE).unwrap()).await;
    assert_eq!(done.status_code(), StatusCode::OK, "{}", done.text());
    let manifest: Value = done.json();
    assert_eq!(
        manifest["request"],
        format!("{BASE}/laboratory/$export?_type=Patient")
    );
    let file = manifest["output"][0]["url"].as_str().unwrap().to_string();
    assert!(
        file.starts_with(&format!("{BASE}/laboratory/export-file/")),
        "{file}"
    );
    let ndjson = h.server.get(file.strip_prefix(BASE).unwrap()).await;
    assert_eq!(ndjson.status_code(), StatusCode::OK);
    assert!(ndjson.text().contains("\"p1\""));
    // The job belongs to the lab tenant: the root door cannot read it.
    let root = h
        .server
        .get(
            status_url
                .strip_prefix(&format!("{BASE}/laboratory"))
                .unwrap(),
        )
        .await;
    assert_ne!(root.status_code(), StatusCode::OK);
}

#[tokio::test]
async fn default_only_matches_manager_off() {
    // Manager on with only the default endpoint behaves like manager off.
    let store = Arc::new(MemoryEndpointStore::new());
    ensure_default_endpoint(store.as_ref(), "default", FhirRelease::R4)
        .await
        .unwrap();
    let on = harness_with(Some(store)).await;
    let off = harness_with(None).await;
    for h in [&on, &off] {
        let put = h
            .server
            .put("/Patient/same")
            .json(&json!({"resourceType": "Patient", "id": "same", "active": true}))
            .await;
        assert_eq!(put.status_code(), StatusCode::CREATED);
        assert_eq!(
            header(&put, "location"),
            format!("{BASE}/Patient/same/_history/1")
        );
    }
    // Search parameters come from a hash-ordered registry, so element order
    // differs between any two backends; compare order-insensitively.
    fn canonical(v: &mut Value) {
        match v {
            Value::Array(items) => {
                items.iter_mut().for_each(canonical);
                items.sort_by_key(|i| i.to_string());
            }
            Value::Object(map) => map.values_mut().for_each(canonical),
            _ => {}
        }
    }
    let strip = |mut v: Value| {
        v.as_object_mut().unwrap().remove("date");
        if let Some(m) = v.get_mut("meta") {
            m.as_object_mut().unwrap().remove("lastUpdated");
        }
        canonical(&mut v);
        v
    };
    // The startup tasks `build_app` spawns (package manager, profile
    // rehydration) settle the default tenant's registry asynchronously, with
    // or without the manager; compare once both statements are stable.
    let mut equal = false;
    for _ in 0..50 {
        let a = strip(on.server.get("/metadata").await.json());
        let b = strip(off.server.get("/metadata").await.json());
        if a == b {
            equal = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(equal, "CapabilityStatements differ with the manager on");
    let ra: Value = on.server.get("/Patient/same").await.json();
    let rb: Value = off.server.get("/Patient/same").await.json();
    assert_eq!(ra["id"], rb["id"]);
    assert_eq!(ra["meta"]["versionId"], rb["meta"]["versionId"]);
    let sa: Value = on.server.get("/Patient?_id=same").await.json();
    let sb: Value = off.server.get("/Patient?_id=same").await.json();
    assert_eq!(sa["link"], sb["link"]);
    assert_eq!(sa["entry"][0]["fullUrl"], sb["entry"][0]["fullUrl"]);
    // X-Tenant-ID selects unbound tenants exactly as before.
    for h in [&on, &off] {
        let r = h
            .server
            .get("/Patient")
            .add_header("x-tenant-id", "t9")
            .await;
        assert_eq!(r.status_code(), StatusCode::OK);
    }
}

#[tokio::test]
async fn no_default_endpoint_is_404() {
    let store = Arc::new(MemoryEndpointStore::new());
    store
        .create(named("only"), &Actor::bootstrap())
        .await
        .unwrap();
    let h = harness_with(Some(store)).await;
    let r = h.server.get("/Patient").await;
    assert_eq!(r.status_code(), StatusCode::NOT_FOUND);
    assert!(r.text().contains("no FHIR endpoint"));
    assert_eq!(
        h.server.get("/only/metadata").await.status_code(),
        StatusCode::OK
    );
}
