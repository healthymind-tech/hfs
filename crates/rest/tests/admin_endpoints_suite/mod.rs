//! Shared integration suite of the endpoint admin API (`/admin/endpoints`),
//! run over in-memory SQLite (`admin_endpoints.rs`) and PostgreSQL
//! (`admin_endpoints_postgres.rs`). Every response body is scanned: a sealed
//! envelope (`hmsec:`) or a plaintext secret must never be echoed.

#![allow(dead_code)]

pub mod mock_idp;

use std::collections::HashMap;
use std::io::Write;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum_test::{TestResponse, TestServer};
use base64::Engine as _;
use flate2::{Compression, write::GzEncoder};
use helios_audit::{AuditEvent, AuditSink};
use helios_endpoints::model::EndpointAuth;
use helios_endpoints::store::EndpointRevision;
use helios_endpoints::sync::DuplicateRequest;
use helios_endpoints::{
    Actor, Endpoint, EndpointRegistry, EndpointSpec, EndpointStatus, EndpointStore, ManagedBy,
    MemoryEndpointStore, OutboundPolicy, Resolution, SealedSecret, SecretKeyRing, StoreError,
    ensure_default_endpoint,
};
use helios_fhir::FhirVersion;
use helios_persistence::core::{ResourceStorage, SearchProvider, WriteObservers};
use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
use helios_rest::ServerConfig;
use helios_rest::config::{PackageConfig, RegistryMode};
use helios_rest::handlers::admin_endpoints::AdminEndpointsState;
use helios_rest::handlers::admin_endpoints::authz::{
    AdminCredentialAuthorizer, ConsoleAdminAuthorizer,
};
use helios_rest::handlers::admin_endpoints::error::ApiError;
use helios_rest::handlers::admin_endpoints::hooks::{
    DuplicateService, DuplicateStarted, EndpointAuthProbe, P2Probe,
};
use helios_rest::handlers::admin_endpoints::outbound::HostResolver;
use helios_rest::packages::{ManagerDeps, PackageManager};
use helios_rest::routing::admin_endpoints::routes;
use helios_rest::validation::ValidationService;
use serde_json::{Value, json};

pub const PASSWORD: &str = "correct horse battery staple";
pub const SECRET: &str = "plaintext-terminology-token-42";

// ------------------------------------------------------------------ fakes

/// Keeps every audit event with its tenant.
#[derive(Default)]
pub struct CaptureSink(pub Mutex<Vec<(Option<String>, Value)>>);

#[async_trait]
impl AuditSink for CaptureSink {
    async fn record(&self, event: AuditEvent) {
        self.record_in(None, event).await;
    }
    async fn record_in(&self, tenant: Option<&str>, event: AuditEvent) {
        self.0.lock().unwrap().push((
            tenant.map(str::to_string),
            serde_json::to_value(&event).unwrap(),
        ));
    }
    async fn flush(&self) {}
    fn name(&self) -> &str {
        "capture"
    }
}

impl CaptureSink {
    pub fn descriptions(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(_, e)| {
                e.get("outcomeDesc")
                    .and_then(Value::as_str)
                    .or_else(|| e.pointer("/outcome/detail/text").and_then(Value::as_str))
                    .map(str::to_string)
                    .unwrap_or_else(|| e.to_string())
            })
            .collect()
    }
}

/// Fixed DNS answers; unknown hosts do not resolve.
pub struct FakeResolver(pub HashMap<String, Vec<IpAddr>>);

impl Default for FakeResolver {
    fn default() -> Self {
        let mut m = HashMap::new();
        m.insert(
            "idp.example.org".to_string(),
            vec!["93.184.216.34".parse().unwrap()],
        );
        m.insert(
            "tx.example.org".to_string(),
            vec!["93.184.216.35".parse().unwrap()],
        );
        m.insert(
            "internal.example.org".to_string(),
            vec!["10.1.2.3".parse().unwrap()],
        );
        m.insert("keycloak".to_string(), vec!["172.18.0.4".parse().unwrap()]);
        Self(m)
    }
}

#[async_trait]
impl HostResolver for FakeResolver {
    async fn resolve(&self, host: &str, _port: u16) -> Result<Vec<IpAddr>, String> {
        self.0.get(host).cloned().ok_or_else(|| "NXDOMAIN".into())
    }
}

/// Records duplicate requests and answers with a job.
#[derive(Default)]
pub struct FakeDuplicate(pub Mutex<Vec<(String, DuplicateRequest)>>);

#[async_trait]
impl DuplicateService for FakeDuplicate {
    async fn start(
        &self,
        source: &Endpoint,
        request: DuplicateRequest,
        _actor: &Actor,
    ) -> Result<DuplicateStarted, ApiError> {
        self.0
            .lock()
            .unwrap()
            .push((source.name().to_string(), request));
        Ok(DuplicateStarted {
            job_id: "job-1".into(),
            status_url: "admin/sync/runs/job-1".into(),
            endpoint: None,
        })
    }
}

/// The real P2 probe, fetching under `allow` (the mock IdP is on loopback).
pub fn p2_probe(allow: &str) -> Arc<dyn EndpointAuthProbe> {
    Arc::new(P2Probe::new(Arc::new(OutboundPolicy::parse(allow))))
}

/// A store whose next `update` loses a race to a concurrent writer.
pub struct RacyStore {
    pub inner: Arc<dyn EndpointStore>,
    pub race_next: Mutex<bool>,
}

#[async_trait]
impl EndpointStore for RacyStore {
    async fn epoch(&self) -> Result<u64, StoreError> {
        self.inner.epoch().await
    }
    async fn list(&self, d: bool) -> Result<Vec<Endpoint>, StoreError> {
        self.inner.list(d).await
    }
    async fn get(&self, name: &str) -> Result<Option<Endpoint>, StoreError> {
        self.inner.get(name).await
    }
    async fn create(&self, spec: EndpointSpec, actor: &Actor) -> Result<Endpoint, StoreError> {
        self.inner.create(spec, actor).await
    }
    async fn update(
        &self,
        name: &str,
        expected: u64,
        spec: EndpointSpec,
        actor: &Actor,
    ) -> Result<Endpoint, StoreError> {
        let race = std::mem::replace(&mut *self.race_next.lock().unwrap(), false);
        if race {
            let cur = self.inner.get(name).await?.unwrap();
            let mut other = cur.spec.clone();
            other.description = Some("concurrent edit".into());
            self.inner
                .update(name, cur.revision, other, &Actor::bootstrap())
                .await
                .ok();
        }
        self.inner.update(name, expected, spec, actor).await
    }
    async fn set_status(
        &self,
        name: &str,
        expected: u64,
        status: EndpointStatus,
        actor: &Actor,
    ) -> Result<Endpoint, StoreError> {
        self.inner.set_status(name, expected, status, actor).await
    }
    async fn history(&self, id: &str, limit: usize) -> Result<Vec<EndpointRevision>, StoreError> {
        self.inner.history(id, limit).await
    }
}

// ------------------------------------------------------------------ packages

fn tar_bytes(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (path, body) in members {
        let mut h = [0u8; 512];
        h[..path.len()].copy_from_slice(path.as_bytes());
        h[100..108].copy_from_slice(b"0000644\0");
        h[108..116].copy_from_slice(b"0000000\0");
        h[116..124].copy_from_slice(b"0000000\0");
        h[124..136].copy_from_slice(format!("{:011o}\0", body.len()).as_bytes());
        h[136..148].copy_from_slice(b"00000000000\0");
        h[156] = b'0';
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        out.extend_from_slice(&h);
        out.extend_from_slice(body);
        out.extend(std::iter::repeat_n(0u8, (512 - body.len() % 512) % 512));
    }
    out.extend(std::iter::repeat_n(0u8, 1024));
    out
}

/// A small IG: one profile.
pub fn package_tgz(name: &str, version: &str) -> Vec<u8> {
    let manifest = json!({
        "name": name, "version": version, "fhirVersions": ["4.0.1"],
        "title": format!("{name} title"), "canonical": "http://acme.org",
        "dependencies": {"hl7.fhir.r4.core": "4.0.1"},
    })
    .to_string();
    let sd = json!({
        "resourceType": "StructureDefinition", "id": "acme-patient",
        "url": "http://acme.org/StructureDefinition/acme-patient", "version": version,
        "name": "AcmePatient", "status": "active", "kind": "resource", "abstract": false,
        "type": "Patient", "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
        "derivation": "constraint",
        "differential": {"element": [
            {"id": "Patient", "path": "Patient"},
            {"id": "Patient.birthDate", "path": "Patient.birthDate", "min": 1}
        ]}
    })
    .to_string();
    let tar = tar_bytes(&[
        ("package/package.json", manifest.as_bytes()),
        (
            "package/StructureDefinition-acme-patient.json",
            sd.as_bytes(),
        ),
    ]);
    let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

// ------------------------------------------------------------------ harness

/// Options of a harness.
pub struct Options {
    pub key_ring: Option<Arc<SecretKeyRing>>,
    pub admin_ui: bool,
    pub duplicate: Option<Arc<dyn DuplicateService>>,
    pub probe: Option<Arc<dyn EndpointAuthProbe>>,
    pub outbound: OutboundPolicy,
    pub max_endpoints: usize,
    pub racy: bool,
    pub token_layer: Option<&'static str>,
    /// Console sign-in runtime: put on every request (as the hfs wiring
    /// does) and decided by `ConsoleAdminAuthorizer`.
    pub console: Option<Arc<helios_admin_auth::console::ConsoleAuth>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            key_ring: Some(Arc::new(
                SecretKeyRing::parse(&SecretKeyRing::generate_spec("k1")).unwrap(),
            )),
            admin_ui: true,
            duplicate: None,
            probe: None,
            outbound: OutboundPolicy::parse("keycloak"),
            max_endpoints: 200,
            racy: false,
            token_layer: None,
            console: None,
        }
    }
}

pub struct Harness {
    pub server: TestServer,
    pub store: Arc<dyn EndpointStore>,
    pub racy: Option<Arc<RacyStore>>,
    pub registry: Arc<EndpointRegistry>,
    pub sink: Arc<CaptureSink>,
    pub storage: Arc<dyn ResourceStorage>,
    pub key_ring: Option<Arc<SecretKeyRing>>,
    _cache: tempfile::TempDir,
}

pub fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("data"))
        .unwrap_or_else(|| PathBuf::from("data"))
}

impl Harness {
    /// A harness over `storage` and an endpoint `store` (empty or holding
    /// only the default endpoint).
    pub async fn new<S>(storage: Arc<S>, store: Arc<dyn EndpointStore>, opts: Options) -> Self
    where
        S: ResourceStorage + SearchProvider + Send + Sync + 'static,
    {
        ensure_default_endpoint(store.as_ref(), "default", helios_endpoints::FhirRelease::R4)
            .await
            .unwrap();
        let racy = opts.racy.then(|| {
            Arc::new(RacyStore {
                inner: store.clone(),
                race_next: Mutex::new(false),
            })
        });
        let api_store: Arc<dyn EndpointStore> = match &racy {
            Some(r) => r.clone(),
            None => store.clone(),
        };
        let registry = Arc::new(EndpointRegistry::load(store.as_ref()).await.unwrap());
        let cache = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            seed_conformance: false,
            packages: PackageConfig {
                admin_ui: opts.admin_ui,
                registry: RegistryMode::Off,
                cache_dir: Some(cache.path().to_path_buf()),
                ..PackageConfig::default()
            },
            ..ServerConfig::for_testing()
        };
        let dyn_storage: Arc<dyn ResourceStorage> = storage.clone();
        let package_store = dyn_storage.package_store().unwrap_or_else(|| {
            Arc::new(helios_rest::packages::store::FilePackageStore::at(
                cache.path().join("registry.json"),
            ))
        });
        let observers = Arc::new(WriteObservers::new());
        let manager = Arc::new(PackageManager::new(
            &config,
            ManagerDeps {
                storage: dyn_storage.clone(),
                store: package_store,
                validation: Arc::new(ValidationService::new()),
                observers: observers.clone(),
                audit: None,
                audit_source: "Device/hfs".into(),
            },
        ));
        let sink = Arc::new(CaptureSink::default());
        let gate = helios_admin_auth::AdminGate::configure(
            "HFS",
            helios_admin_auth::AdminSettings::from_lookup("HFS", |_| None).with_password(PASSWORD),
            true,
            helios_admin_auth::CookieSpec {
                name: "hfs_admin",
                path: "/",
            },
        );
        let mut builder = AdminEndpointsState::builder(api_store, registry.clone())
            .key_ring(opts.key_ring.clone())
            .outbound(opts.outbound)
            .resolver(Arc::new(FakeResolver::default()))
            .authorizer(match &opts.console {
                Some(_) => Arc::new(ConsoleAdminAuthorizer::new(
                    None,
                    AdminCredentialAuthorizer::new(Some(gate)),
                )),
                None => Arc::new(AdminCredentialAuthorizer::new(Some(gate))),
            })
            .packages(manager, 16 * 1024 * 1024)
            .storage(storage, Some(observers))
            .audit(Some(sink.clone()), "Device/hfs", "default")
            .max_endpoints(opts.max_endpoints)
            .server_base("https://hfs.example.org/fhir");
        if let Some(d) = opts.duplicate {
            builder = builder.duplicate(d);
        }
        if let Some(p) = opts.probe {
            builder = builder.auth_probe(p);
        }
        let mut app = routes(builder.build());
        if let Some(rt) = &opts.console {
            app = app.layer(axum::Extension(rt.clone()));
        }
        if let Some(scopes) = opts.token_layer {
            // Stands in for the FHIR auth layer: every request carries a
            // validated bearer principal with these scopes.
            app = app.layer(axum::middleware::from_fn(
                move |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
                    req.extensions_mut().insert(helios_auth::Principal {
                        subject: "svc".into(),
                        issuer: "https://idp.example.org".into(),
                        tenant_id: None,
                        scopes: helios_auth::ScopeSet::parse(scopes),
                        jti: None,
                        expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
                        custom_claims: Default::default(),
                        launch_context: None,
                    });
                    next.run(req).await
                },
            ));
        }
        Self {
            server: TestServer::new(app).unwrap(),
            store,
            racy,
            registry,
            sink,
            storage: dyn_storage,
            key_ring: opts.key_ring,
            _cache: cache,
        }
    }

    /// Over a fresh in-memory endpoint store.
    pub async fn memory<S>(storage: Arc<S>, opts: Options) -> Self
    where
        S: ResourceStorage + SearchProvider + Send + Sync + 'static,
    {
        Self::new(storage, Arc::new(MemoryEndpointStore::new()), opts).await
    }
}

pub fn basic() -> HeaderValue {
    let v = base64::engine::general_purpose::STANDARD.encode(format!("admin:{PASSWORD}"));
    HeaderValue::from_str(&format!("Basic {v}")).unwrap()
}

/// Checks a response never carries a secret and returns `(status, body)`.
pub fn scan(r: &TestResponse) -> (StatusCode, Value) {
    let text = r.text();
    assert!(!text.contains("hmsec:"), "sealed secret echoed: {text}");
    assert!(!text.contains(SECRET), "plaintext secret echoed: {text}");
    for (_, v) in r.headers() {
        let v = v.to_str().unwrap_or_default();
        assert!(!v.contains("hmsec:") && !v.contains(SECRET));
    }
    let body = if text.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    (r.status_code(), body)
}

pub fn etag_of(r: &TestResponse) -> String {
    r.header(header::ETAG).to_str().unwrap().to_string()
}

pub fn fields(outcome: &Value) -> Vec<String> {
    outcome["issue"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|i| i["expression"][0].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

const IF_MATCH: HeaderName = header::IF_MATCH;

impl Harness {
    pub async fn get(&self, path: &str) -> TestResponse {
        self.server
            .get(path)
            .add_header(header::AUTHORIZATION, basic())
            .await
    }
    pub async fn post(&self, path: &str, body: Value) -> TestResponse {
        self.server
            .post(path)
            .add_header(header::AUTHORIZATION, basic())
            .json(&body)
            .await
    }
    pub async fn post_empty(&self, path: &str) -> TestResponse {
        self.server
            .post(path)
            .add_header(header::AUTHORIZATION, basic())
            .await
    }
    pub async fn put(&self, path: &str, etag: Option<&str>, body: Value) -> TestResponse {
        let mut r = self
            .server
            .put(path)
            .add_header(header::AUTHORIZATION, basic())
            .json(&body);
        if let Some(e) = etag {
            r = r.add_header(IF_MATCH, HeaderValue::from_str(e).unwrap());
        }
        r.await
    }
    pub async fn patch(&self, path: &str, etag: Option<&str>, body: Value) -> TestResponse {
        let mut r = self
            .server
            .patch(path)
            .add_header(header::AUTHORIZATION, basic())
            .json(&body);
        if let Some(e) = etag {
            r = r.add_header(IF_MATCH, HeaderValue::from_str(e).unwrap());
        }
        r.await
    }
    pub async fn delete(&self, path: &str) -> TestResponse {
        self.server
            .delete(path)
            .add_header(header::AUTHORIZATION, basic())
            .await
    }

    pub async fn create(&self, body: Value) -> (StatusCode, Value, String) {
        let r = self.post("/admin/endpoints", body).await;
        let (s, b) = scan(&r);
        let etag = r
            .maybe_header(header::ETAG)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        (s, b, etag)
    }

    pub async fn seed(&self, tenant: &str, resource_type: &str, n: usize) {
        let ctx = TenantContext::new(TenantId::new(tenant), TenantPermissions::full_access());
        for i in 0..n {
            self.storage
                .create(
                    &ctx,
                    resource_type,
                    json!({"resourceType": resource_type, "id": format!("{resource_type}-{i}")}),
                    FhirVersion::R4,
                )
                .await
                .unwrap();
        }
    }

    /// Which endpoint this instance's routing snapshot sends `path` to.
    pub fn route(&self, path: &str) -> String {
        match self.registry.snapshot().resolve(path) {
            Resolution::Serve(r) => r.endpoint.name().to_string(),
            Resolution::Unavailable(r) => format!("unavailable:{}", r.endpoint.name()),
            Resolution::NotFound => "not-found".into(),
        }
    }

    pub fn audited(&self, needle: &str) -> bool {
        self.sink
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|(t, e)| t.as_deref() == Some("default") && e.to_string().contains(needle))
    }
}

// ------------------------------------------------------------------ the suite

/// Create, read, list, replace, patch, status, delete, history; ETags and
/// hot reload.
pub async fn crud_lifecycle(h: &Harness) {
    let (s, body, etag) = h
        .create(json!({"name": "twcore", "displayName": "TW Core", "labels": {"env": "test"}}))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    assert_eq!(etag, "\"1\"");
    assert_eq!(body["path"], "twcore");
    assert_eq!(body["storage"]["tenantId"], "twcore");
    assert_eq!(body["publicBaseUrl"], "https://hfs.example.org/fhir/twcore");
    // Hot reload: the instance's snapshot already routes it.
    assert_eq!(h.route("/twcore/Patient"), "twcore");

    let r = h.get("/admin/endpoints/twcore").await;
    let (s, got) = scan(&r);
    assert_eq!(s, StatusCode::OK);
    assert_eq!(etag_of(&r), "\"1\"");
    assert_eq!(got["displayName"], "TW Core");
    let r = h
        .server
        .get("/admin/endpoints/twcore")
        .add_header(header::AUTHORIZATION, basic())
        .add_header(header::IF_NONE_MATCH, HeaderValue::from_static("\"1\""))
        .await;
    assert_eq!(r.status_code(), StatusCode::NOT_MODIFIED);

    let (s, list) = scan(&h.get("/admin/endpoints").await);
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list["total"], 2);
    assert_eq!(list["endpoints"][0]["name"], "default");
    assert_eq!(list["endpoints"][1]["name"], "twcore");
    let (s, _) = scan(&h.get("/admin/endpoints?include=bogus").await);
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // PUT back what GET returned, with one change.
    let mut edited = got.clone();
    edited["displayName"] = json!("Taiwan Core");
    let (s, o) = scan(&h.put("/admin/endpoints/twcore", None, edited.clone()).await);
    assert_eq!(s, StatusCode::PRECONDITION_REQUIRED, "{o}");
    let r = h
        .put("/admin/endpoints/twcore", Some("\"1\""), edited.clone())
        .await;
    let (s, put) = scan(&r);
    assert_eq!(s, StatusCode::OK, "{put}");
    assert_eq!(etag_of(&r), "\"2\"");
    assert_eq!(put["displayName"], "Taiwan Core");
    // Stale If-Match.
    let r = h
        .put("/admin/endpoints/twcore", Some("\"1\""), edited)
        .await;
    let (s, o) = scan(&r);
    assert_eq!(s, StatusCode::PRECONDITION_FAILED);
    assert_eq!(o["extension"][0]["valueInteger"], 2);
    assert_eq!(etag_of(&r), "\"2\"");

    // PATCH: merge patch; null removes; path rename.
    let r = h
        .patch(
            "/admin/endpoints/twcore",
            Some("\"2\""),
            json!({"path": "tw", "labels": {"env": null, "team": "ig"},
                   "rateLimit": {"anonymousPerMinute": 60, "authenticatedPerMinute": 600, "burstSeconds": 10}}),
        )
        .await;
    let (s, p) = scan(&r);
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!(p["path"], "tw");
    assert_eq!(p["labels"], json!({"team": "ig"}));
    assert_eq!(p["rateLimit"]["anonymousPerMinute"], 60);
    assert_eq!(h.route("/tw/Patient"), "twcore");
    // The old path is no longer an endpoint: it falls to the default endpoint.
    assert_eq!(h.route("/twcore/Patient"), "default");
    let r = h
        .server
        .patch("/admin/endpoints/twcore")
        .add_header(header::AUTHORIZATION, basic())
        .add_header(IF_MATCH, HeaderValue::from_static("*"))
        .add_header(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"))
        .bytes("{}".into())
        .await;
    assert_eq!(r.status_code(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    // Disable / enable (idempotent), hot reload each time.
    let (s, d) = scan(&h.post_empty("/admin/endpoints/twcore/$disable").await);
    assert_eq!(s, StatusCode::OK);
    assert_eq!(d["status"], "disabled");
    assert_eq!(h.route("/tw/Patient"), "unavailable:twcore");
    let (s, d2) = scan(&h.post_empty("/admin/endpoints/twcore/$disable").await);
    assert_eq!(s, StatusCode::OK);
    assert_eq!(d2["revision"], d["revision"]);
    let r = h
        .server
        .post("/admin/endpoints/twcore/$enable")
        .add_header(header::AUTHORIZATION, basic())
        .add_header(IF_MATCH, HeaderValue::from_static("\"1\""))
        .await;
    assert_eq!(r.status_code(), StatusCode::PRECONDITION_FAILED);
    let (s, e) = scan(&h.post_empty("/admin/endpoints/twcore/$enable").await);
    assert_eq!(s, StatusCode::OK);
    assert_eq!(e["status"], "enabled");

    // Delete needs ?confirm=name.
    let (s, o) = scan(&h.delete("/admin/endpoints/twcore").await);
    assert_eq!(s, StatusCode::BAD_REQUEST, "{o}");
    let (s, _) = scan(&h.delete("/admin/endpoints/twcore?confirm=other").await);
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, gone) = scan(&h.delete("/admin/endpoints/twcore?confirm=twcore").await);
    assert_eq!(s, StatusCode::OK, "{gone}");
    assert_eq!(gone["endpoint"]["status"], "deleted");
    assert_eq!(gone["purged"], false);
    assert_eq!(h.route("/tw/Patient"), "default");
    let (s, _) = scan(&h.get("/admin/endpoints/twcore").await);
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (_, list) = scan(&h.get("/admin/endpoints?include=deleted").await);
    assert_eq!(list["total"], 2);

    // History of the deleted endpoint, newest first.
    let (s, hist) = scan(&h.get("/admin/endpoints/twcore/history?limit=10").await);
    assert_eq!(s, StatusCode::OK, "{hist}");
    let revs: Vec<u64> = hist["revisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["revision"].as_u64().unwrap())
        .collect();
    assert_eq!(revs.first(), Some(&6));
    assert_eq!(revs.last(), Some(&1));
    assert_eq!(hist["revisions"][0]["change"], "status");
    let (s, _) = scan(&h.get("/admin/endpoints/nope/history").await);
    assert_eq!(s, StatusCode::NOT_FOUND);

    // The name is reusable.
    let (s, again, _) = h.create(json!({"name": "twcore"})).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_ne!(again["id"], got["id"]);

    // Every write was audited in the default tenant.
    for needle in [
        "created endpoint twcore",
        "replaced endpoint twcore",
        "patched endpoint twcore",
        "enabled → disabled",
        "deleted endpoint twcore",
    ] {
        assert!(
            h.audited(needle),
            "missing audit {needle:?}: {:?}",
            h.sink.descriptions()
        );
    }
    assert!(h.audited("Endpoint/twcore"));
}

/// Validation: one issue per field, reserved names, duplicates, immutable
/// fields, status changes outside the status routes, the default endpoint.
pub async fn validation_and_conflicts(h: &Harness) {
    let (s, o, _) = h
        .create(json!({"name": "admin", "path": "Bad_Path", "displayName": " ", "rateLimits": {}}))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let f = fields(&o);
    for want in ["name", "path", "displayName", "rateLimits"] {
        assert!(f.contains(&want.to_string()), "missing {want} in {f:?}");
    }
    assert!(o.to_string().contains("reserved"));

    let (s, o, _) = h
        .create(
            json!({"name": "lab", "cors": {"allowedOrigins": ["*"], "allowCredentials": true},
                       "auth": {"mode": "full"}, "legacyTenantHeader": true}),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let f = fields(&o);
    for want in [
        "cors.allowCredentials",
        "auth.issuer",
        "auth.jwksUrl",
        "legacyTenantHeader",
    ] {
        assert!(f.contains(&want.to_string()), "missing {want} in {f:?}");
    }
    let (s, _, _) = h
        .create(json!({"name": "lab", "status": "provisioning"}))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, o) = scan(
        &h.server
            .post("/admin/endpoints")
            .add_header(header::AUTHORIZATION, basic())
            .text("{not json")
            .await,
    );
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(o["resourceType"], "OperationOutcome");

    #[cfg(not(feature = "R6"))]
    {
        let (s, o, _) = h.create(json!({"name": "six", "fhirVersion": "R6"})).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(fields(&o), vec!["fhirVersion"]);
    }

    let (s, lab, _) = h.create(json!({"name": "lab", "status": "disabled"})).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(lab["status"], "disabled");
    // Duplicates.
    let (s, o, _) = h.create(json!({"name": "lab"})).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(o["issue"][0]["code"], "duplicate");
    let (s, o, _) = h.create(json!({"name": "lab2", "path": "lab"})).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(fields(&o), vec!["path"]);
    let (s, o, _) = h
        .create(json!({"name": "lab3", "storage": {"kind": "tenant", "tenantId": "lab"}}))
        .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(fields(&o), vec!["storage.tenantId"]);

    // Immutable storage, status only through the status routes.
    let rev = format!("\"{}\"", lab["revision"]);
    let (s, o) = scan(
        &h.patch(
            "/admin/endpoints/lab",
            Some(&rev),
            json!({"storage": {"kind": "tenant", "tenantId": "moved"}}),
        )
        .await,
    );
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(o.to_string().contains("immutable"));
    let (s, o) = scan(
        &h.patch(
            "/admin/endpoints/lab",
            Some(&rev),
            json!({"status": "enabled"}),
        )
        .await,
    );
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(fields(&o), vec!["status"]);
    let (s, _) = scan(
        &h.patch(
            "/admin/endpoints/lab",
            Some(&rev),
            json!({"name": "renamed"}),
        )
        .await,
    );
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = scan(&h.patch("/admin/endpoints/nope", Some("*"), json!({})).await);
    assert_eq!(s, StatusCode::NOT_FOUND);

    // The default endpoint cannot be deleted; a named endpoint cannot take the root path.
    let (s, o) = scan(&h.delete("/admin/endpoints/default?confirm=default").await);
    assert_eq!(s, StatusCode::CONFLICT, "{o}");
    let (s, o) = scan(
        &h.patch("/admin/endpoints/lab", Some("*"), json!({"path": ""}))
            .await,
    );
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{o}");

    // Refused writes are audited.
    assert!(h.audited("failed with 409"), "{:?}", h.sink.descriptions());
}

/// Secrets: write-only, sealed under the key ring bound to the endpoint id,
/// kept across a GET → PUT round trip, cleared explicitly, re-sealed by
/// `$rewrap`.
pub async fn secrets(h: &Harness) {
    let ring = h.key_ring.clone().expect("harness with a key ring");
    let (s, o, _) = h
        .create(
            json!({"name": "term", "terminology": {"serverUrl": "https://tx.example.org/fhir",
                       "token": SECRET}}),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{o}");
    assert_eq!(o["issue"][0]["code"], "security", "{o}");
    let (s, o, _) = h
        .create(json!({"name": "term", "secrets": {"terminology.token": {"set": SECRET}}}))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{o}");
    assert_eq!(fields(&o), vec!["secrets.terminology.token"]);
    // Nothing half-created.
    assert!(h.store.get("term").await.unwrap().is_none());

    let (s, tx, etag) = h
        .create(
            json!({"name": "term", "terminology": {"serverUrl": "https://tx.example.org/fhir"},
                       "secrets": {"terminology.token": {"set": SECRET}}}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{tx}");
    assert_eq!(tx["terminology"]["token"], json!({"set": true}));
    assert_eq!(etag, "\"2\"");
    let stored = h.store.get("term").await.unwrap().unwrap();
    let sealed: SealedSecret = stored.spec.terminology.clone().unwrap().token.unwrap();
    let id = stored.id.clone();
    assert_eq!(
        ring.open(&sealed, &format!("endpoint:{id}:terminology.token"))
            .unwrap()
            .expose_str(),
        Some(SECRET)
    );

    // GET → PUT keeps the secret.
    let r = h.get("/admin/endpoints/term").await;
    let (_, got) = scan(&r);
    let mut edited = got.clone();
    edited["description"] = json!("terminology test");
    let (s, put) = scan(
        &h.put("/admin/endpoints/term", Some(&etag_of(&r)), edited)
            .await,
    );
    assert_eq!(s, StatusCode::OK, "{put}");
    let after = h.store.get("term").await.unwrap().unwrap();
    assert_eq!(after.spec.terminology.unwrap().token.unwrap(), sealed);

    // History is redacted too.
    let (_, hist) = scan(&h.get("/admin/endpoints/term/history").await);
    assert!(hist.to_string().contains("\"set\":true"));

    // Re-seal under a rotated ring: the harness ring holds k1 only, so rotate
    // through the store directly and check $rewrap reports the old key as current.
    let (s, rw) = scan(&h.post_empty("/admin/secrets/$rewrap").await);
    assert_eq!(s, StatusCode::OK, "{rw}");
    assert_eq!(rw["activeKey"], ring.active_key_id());
    assert_eq!(rw["reports"][0]["current"], 1);
    assert_eq!(rw["rewrapped"], 0);

    // Clear.
    let (s, cleared) = scan(
        &h.patch(
            "/admin/endpoints/term",
            Some("*"),
            json!({"secrets": {"terminology.token": "clear"}}),
        )
        .await,
    );
    assert_eq!(s, StatusCode::OK, "{cleared}");
    assert_eq!(cleared["terminology"]["token"], Value::Null);
    assert!(h.audited("terminology.token:clear"));
}

/// Without `HFS_SECRET_KEYS` storing a secret is refused (`409`), the rest works.
pub async fn no_key_ring(h: &Harness) {
    let (s, o, _) = h
        .create(
            json!({"name": "term", "terminology": {"serverUrl": "https://tx.example.org/fhir"},
                       "secrets": {"terminology.token": {"set": SECRET}}}),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{o}");
    assert!(o.to_string().contains("HFS_SECRET_KEYS"));
    assert!(h.store.get("term").await.unwrap().is_none());
    let (s, _, _) = h
        .create(
            json!({"name": "term", "terminology": {"serverUrl": "https://tx.example.org/fhir"}}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, _) = scan(&h.post_empty("/admin/secrets/$rewrap").await);
    assert_eq!(s, StatusCode::CONFLICT);
}

/// `$rewrap` re-seals a secret sealed under an older key.
pub async fn rewrap_rotation(h: &Harness, old_spec: &str) {
    let old = SecretKeyRing::parse(old_spec).unwrap();
    // An endpoint whose token was sealed under the old key (written as the
    // bootstrap would, straight into the store).
    let mut spec =
        EndpointSpec::named(helios_endpoints::EndpointName::parse("rot").unwrap(), "Rot");
    spec.terminology = Some(helios_endpoints::TerminologyBinding {
        server_url: Some("https://tx.example.org/fhir".into()),
        ..Default::default()
    });
    let created = h.store.create(spec, &Actor::bootstrap()).await.unwrap();
    let mut with_token = created.spec.clone();
    with_token.terminology.as_mut().unwrap().token = Some(old.seal_str(
        SECRET,
        &format!("endpoint:{}:terminology.token", created.id),
    ));
    h.store
        .update("rot", created.revision, with_token, &Actor::bootstrap())
        .await
        .unwrap();
    let (s, rw) = scan(&h.post_empty("/admin/secrets/$rewrap").await);
    assert_eq!(s, StatusCode::OK, "{rw}");
    assert_eq!(rw["rewrapped"], 1);
    assert_eq!(rw["failed"], 0);
    let ring = h.key_ring.clone().unwrap();
    let now = h.store.get("rot").await.unwrap().unwrap();
    let sealed = now.spec.terminology.unwrap().token.unwrap();
    assert_eq!(sealed.key_id(), ring.active_key_id());
    assert_eq!(
        ring.open(&sealed, &format!("endpoint:{}:terminology.token", now.id))
            .unwrap()
            .expose_str(),
        Some(SECRET)
    );
}

/// URLs the server fetches pass the SSRF policy when they change.
pub async fn ssrf(h: &Harness) {
    let (s, o, _) = h
        .create(json!({"name": "sec", "auth": {"mode": "full",
            "issuer": "https://internal.example.org/realms/a",
            "jwksUrl": "https://169.254.169.254/latest/meta-data"},
            "terminology": {"serverUrl": "https://metadata.google.internal/x"}}))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{o}");
    assert_eq!(
        fields(&o),
        vec!["auth.issuer", "auth.jwksUrl", "terminology.serverUrl"]
    );
    assert!(
        o["issue"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["code"] == "security")
    );
    // Allow-listed internal host (HFS_OUTBOUND_ALLOW=keycloak), public https.
    let (s, o, _) = h
        .create(json!({"name": "sec", "auth": {"mode": "write-protect",
            "issuer": "https://idp.example.org/realms/a",
            "jwksUrl": "http://keycloak:8080/realms/a/protocol/openid-connect/certs",
            "audience": "sec-api"}}))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{o}");
    // Unresolvable host: accepted with a warning.
    let (s, o) = scan(
        &h.patch(
            "/admin/endpoints/sec",
            Some("*"),
            json!({"terminology": {"serverUrl": "https://nowhere.invalid/fhir"}}),
        )
        .await,
    );
    assert_eq!(s, StatusCode::OK, "{o}");
    assert!(
        o["warnings"][0]
            .as_str()
            .unwrap()
            .contains("did not resolve")
    );
    // A changed URL into an internal range is refused.
    let (s, o) = scan(
        &h.patch(
            "/admin/endpoints/sec",
            Some("*"),
            json!({"auth": {"issuer": "http://127.0.0.1:8080/realms/a"}}),
        )
        .await,
    );
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{o}");
}

/// A write that loses a race in the store answers `409` with the revision.
pub async fn store_race(h: &Harness) {
    let (s, _, etag) = h.create(json!({"name": "race"})).await;
    assert_eq!(s, StatusCode::CREATED);
    *h.racy
        .as_ref()
        .expect("racy harness")
        .race_next
        .lock()
        .unwrap() = true;
    let r = h
        .patch(
            "/admin/endpoints/race",
            Some(&etag),
            json!({"displayName": "mine"}),
        )
        .await;
    let (s, o) = scan(&r);
    assert_eq!(s, StatusCode::CONFLICT, "{o}");
    assert_eq!(o["issue"][0]["code"], "conflict");
    assert_eq!(o["extension"][0]["valueInteger"], 2);
    assert_eq!(etag_of(&r), "\"2\"");
    // Re-read and retry.
    let (s, _) = scan(
        &h.patch(
            "/admin/endpoints/race",
            Some("\"2\""),
            json!({"displayName": "mine"}),
        )
        .await,
    );
    assert_eq!(s, StatusCode::OK);
}

/// `HFS_ENDPOINTS_FILE` endpoints are read-only in the API.
pub async fn env_managed(h: &Harness) {
    let mut spec = EndpointSpec::named(
        helios_endpoints::EndpointName::parse("pinned").unwrap(),
        "Pinned",
    );
    spec.managed_by = ManagedBy::Env;
    h.store.create(spec, &Actor::bootstrap()).await.unwrap();
    h.registry.reload_if_stale(h.store.as_ref()).await.unwrap();
    let (s, o) = scan(
        &h.patch(
            "/admin/endpoints/pinned",
            Some("*"),
            json!({"displayName": "x"}),
        )
        .await,
    );
    assert_eq!(s, StatusCode::CONFLICT, "{o}");
    assert!(o.to_string().contains("HFS_ENDPOINTS_FILE"));
    let (s, _) = scan(&h.post_empty("/admin/endpoints/pinned/$disable").await);
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = scan(
        &h.delete("/admin/endpoints/pinned?confirm=pinned&purge=true")
            .await,
    );
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _, _) = h.create(json!({"name": "mine", "managedBy": "env"})).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // Still enabled and routed.
    assert_eq!(h.route("/pinned/metadata"), "pinned");
}

/// Authorization: no credential, a wrong password, cross-site cookie-less
/// writes are refused and refused writes audited.
pub async fn authorization(h: &Harness) {
    let r = h.server.get("/admin/endpoints").await;
    let (s, o) = scan(&r);
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(o["resourceType"], "OperationOutcome");
    let bad = base64::engine::general_purpose::STANDARD.encode("admin:wrong");
    let r = h
        .server
        .post("/admin/endpoints")
        .add_header(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {bad}")).unwrap(),
        )
        .json(&json!({"name": "x"}))
        .await;
    assert_eq!(r.status_code(), StatusCode::UNAUTHORIZED);
    assert!(h.audited("refused"), "{:?}", h.sink.descriptions());
    assert!(h.store.get("x").await.unwrap().is_none());
    // Unknown routes fall through (404 from the router, not an auth error).
    let r = h.server.get("/admin/nothing-here").await;
    assert_eq!(r.status_code(), StatusCode::NOT_FOUND);
}

/// Token principals need the admin scope (`system/*.cruds`).
pub async fn token_scope(h: &Harness, expect: StatusCode) {
    let r = h.server.get("/admin/endpoints").await;
    assert_eq!(r.status_code(), expect, "{}", r.text());
}

/// Package install / list / detail / uninstall in an endpoint's tenant.
pub async fn packages(h: &Harness) {
    let (s, _, _) = h.create(json!({"name": "ig"})).await;
    assert_eq!(s, StatusCode::CREATED);
    let r = h
        .server
        .post("/admin/endpoints/ig/packages?wait=30&dryRun=false")
        .add_header(header::AUTHORIZATION, basic())
        .add_header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/gzip"),
        )
        .bytes(package_tgz("acme.base", "1.0.0").into())
        .await;
    let (s, job) = scan(&r);
    assert!(
        s == StatusCode::OK || s == StatusCode::ACCEPTED,
        "{s} {job}"
    );
    assert_eq!(job["status"], "succeeded", "{job}");
    assert_eq!(job["tenant"], "ig");
    assert!(
        r.header(header::LOCATION)
            .to_str()
            .unwrap()
            .starts_with("admin/packages/jobs/")
    );

    let (s, list) = scan(&h.get("/admin/endpoints/ig/packages").await);
    assert_eq!(s, StatusCode::OK, "{list}");
    assert_eq!(list["tenant"], "ig");
    assert_eq!(list["packages"][0]["name"], "acme.base");
    let (_, dflt) = scan(&h.get("/admin/endpoints/default/packages").await);
    assert_eq!(dflt["packages"].as_array().unwrap().len(), 0, "{dflt}");

    let (s, detail) = scan(&h.get("/admin/endpoints/ig/packages/acme.base/1.0.0").await);
    assert_eq!(s, StatusCode::OK, "{detail}");
    let (s, _) = scan(
        &h.get("/admin/endpoints/default/packages/acme.base/1.0.0")
            .await,
    );
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (s, job) = scan(
        &h.delete("/admin/endpoints/ig/packages/acme.base/1.0.0?wait=30")
            .await,
    );
    assert!(s == StatusCode::OK || s == StatusCode::ACCEPTED, "{job}");
    assert_eq!(job["status"], "succeeded", "{job}");
    let (_, list) = scan(&h.get("/admin/endpoints/ig/packages").await);
    assert_eq!(list["packages"].as_array().unwrap().len(), 0);

    let (s, _) = scan(&h.get("/admin/endpoints/nope/packages").await);
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, o) = scan(
        &h.server
            .post("/admin/endpoints/ig/packages")
            .add_header(header::AUTHORIZATION, basic())
            .text("not json")
            .await,
    );
    assert_eq!(s, StatusCode::BAD_REQUEST, "{o}");
}

/// With `HFS_ADMIN_UI=false` the package routes answer `403`.
pub async fn packages_disabled(h: &Harness) {
    let (s, o) = scan(&h.get("/admin/endpoints/default/packages").await);
    assert_eq!(s, StatusCode::FORBIDDEN, "{o}");
}

/// `$duplicate` validates, then hands off (`501` without the sync service).
pub async fn duplicate_unavailable(h: &Harness) {
    let (s, o) = scan(
        &h.post(
            "/admin/endpoints/default/$duplicate",
            json!({"name": "copy"}),
        )
        .await,
    );
    assert_eq!(s, StatusCode::NOT_IMPLEMENTED, "{o}");
    assert_eq!(o["issue"][0]["code"], "not-supported");
}

pub async fn duplicate_handoff(h: &Harness, fake: &FakeDuplicate) {
    let (s, o) = scan(
        &h.post("/admin/endpoints/default/$duplicate", json!({"name": "ui"}))
            .await,
    );
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{o}");
    assert_eq!(fields(&o), vec!["name", "path"]);
    let (s, o) = scan(
        &h.post(
            "/admin/endpoints/default/$duplicate",
            json!({"name": "default"}),
        )
        .await,
    );
    assert_eq!(s, StatusCode::CONFLICT, "{o}");
    let (s, _) = scan(
        &h.post("/admin/endpoints/default/$duplicate", json!({"nom": 1}))
            .await,
    );
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) = scan(
        &h.post("/admin/endpoints/nope/$duplicate", json!({"name": "x"}))
            .await,
    );
    assert_eq!(s, StatusCode::NOT_FOUND);
    let r = h
        .post(
            "/admin/endpoints/default/$duplicate",
            json!({"name": "copy", "data": "current", "keepInSync": true}),
        )
        .await;
    let (s, o) = scan(&r);
    assert_eq!(s, StatusCode::ACCEPTED, "{o}");
    assert_eq!(o["jobId"], "job-1");
    assert_eq!(r.header(header::LOCATION), "admin/sync/runs/job-1");
    let calls = fake.0.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "default");
    assert_eq!(calls[0].1.name, "copy");
    assert!(calls[0].1.keep_in_sync);
    drop(calls);
    assert!(h.audited("duplicate of endpoint default as copy"));
}

/// `$test-auth`: `501` without the probe; SSRF before probing; inherited auth.
pub async fn test_auth_unavailable(h: &Harness) {
    let (s, o) = scan(&h.post_empty("/admin/endpoints/default/$test-auth").await);
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{o}");
    let (s, _) = scan(
        &h.post(
            "/admin/endpoints/default/$test-auth",
            json!({"mode": "full", "issuer": "https://idp.example.org/realms/a",
                   "jwksUrl": "https://idp.example.org/certs"}),
        )
        .await,
    );
    assert_eq!(s, StatusCode::NOT_IMPLEMENTED);
}

pub async fn test_auth_probe(h: &Harness) {
    // A healthy IdP: discovery and JWKS pass, token checked.
    let idp = mock_idp::start(true, true).await;
    let auth = json!({"mode": "full", "issuer": idp.issuer, "jwksUrl": idp.jwks_url,
                      "audience": "lab-api"});
    let (s, o) = scan(
        &h.post("/admin/endpoints/default/$test-auth", auth.clone())
            .await,
    );
    assert_eq!(s, StatusCode::OK, "{o}");
    assert_eq!(o["ok"], true, "{o}");
    assert_eq!(o["mode"], "full");
    assert_eq!(o["issuer"]["ok"], true, "{o}");
    assert_eq!(o["jwks"]["ok"], true, "{o}");
    assert_eq!(o["jwks"]["keyCount"], 1);
    assert!(o.get("token").is_none());

    let good = mock_idp::token(&idp.issuer, "lab-api", 300);
    let (s, o) = scan(
        &h.post(
            "/admin/endpoints/default/$test-auth",
            json!({"auth": auth, "token": good}),
        )
        .await,
    );
    assert_eq!(s, StatusCode::OK, "{o}");
    assert_eq!(o["token"]["ok"], true, "{o}");
    assert!(
        o["token"]["detail"]
            .as_str()
            .unwrap()
            .contains("svc-client")
    );
    assert_eq!(o["ok"], true);

    let expired = mock_idp::token(&idp.issuer, "lab-api", -3600);
    let (_, o) = scan(
        &h.post(
            "/admin/endpoints/default/$test-auth",
            json!({"auth": auth, "token": expired}),
        )
        .await,
    );
    assert_eq!(o["token"]["ok"], false, "{o}");
    assert_eq!(o["ok"], false);
    let other_aud = mock_idp::token(&idp.issuer, "someone-else", 300);
    let (_, o) = scan(
        &h.post(
            "/admin/endpoints/default/$test-auth",
            json!({"auth": auth, "token": other_aud}),
        )
        .await,
    );
    assert_eq!(o["token"]["ok"], false, "{o}");
    let (s, _) = scan(
        &h.post(
            "/admin/endpoints/default/$test-auth",
            json!({"auth": auth, "token": 5}),
        )
        .await,
    );
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    // Discovery down / JWKS down.
    let down = mock_idp::start(false, true).await;
    let (s, o) = scan(
        &h.post(
            "/admin/endpoints/default/$test-auth",
            json!({"mode": "full", "issuer": down.issuer, "jwksUrl": down.jwks_url}),
        )
        .await,
    );
    assert_eq!(s, StatusCode::OK);
    assert_eq!(o["ok"], false, "{o}");
    assert_eq!(o["issuer"]["ok"], false);
    assert_eq!(o["jwks"]["ok"], true);
    assert!(o["warnings"].to_string().contains("audience"), "{o}");
    let nokeys = mock_idp::start(true, false).await;
    let (_, o) = scan(
        &h.post(
            "/admin/endpoints/default/$test-auth",
            json!({"mode": "full", "issuer": nokeys.issuer, "jwksUrl": nokeys.jwks_url}),
        )
        .await,
    );
    assert_eq!(o["issuer"]["ok"], true, "{o}");
    assert_eq!(o["jwks"]["ok"], false, "{o}");

    // Auth off: nothing fetched, a token cannot be checked.
    let (_, o) = scan(
        &h.post(
            "/admin/endpoints/default/$test-auth",
            json!({"auth": {"mode": "off"}, "token": "x.y.z"}),
        )
        .await,
    );
    assert_eq!(o["mode"], "off", "{o}");
    assert_eq!(o["token"]["ok"], false);

    // SSRF before probing; issuer required.
    let (s, o) = scan(
        &h.post(
            "/admin/endpoints/default/$test-auth",
            json!({"mode": "full", "issuer": "https://internal.example.org/realms/a",
                   "jwksUrl": "https://idp.example.org/certs"}),
        )
        .await,
    );
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{o}");
    assert_eq!(fields(&o), vec!["auth.issuer"]);
    let (s, _) = scan(
        &h.post(
            "/admin/endpoints/default/$test-auth",
            json!({"mode": "full"}),
        )
        .await,
    );
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) = scan(&h.post_empty("/admin/endpoints/default/$test-auth").await);
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    // The stored auth section of an endpoint.
    let (s, o, _) = h
        .create(json!({"name": "authd", "auth": {"mode": "full",
            "issuer": idp.issuer, "jwksUrl": idp.jwks_url, "audience": "authd-api"}}))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{o}");
    let (s, o) = scan(&h.post_empty("/admin/endpoints/authd/$test-auth").await);
    assert_eq!(s, StatusCode::OK, "{o}");
    assert_eq!(o["ok"], true, "{o}");
}

/// Env-managed endpoints are operator-trusted: their stored URLs are probed
/// without the outbound policy; console-edited ones are not.
pub async fn probe_guards() {
    let idp = mock_idp::start(true, true).await;
    let auth: EndpointAuth = serde_json::from_value(json!({"mode": "full",
        "issuer": idp.issuer, "jwksUrl": idp.jwks_url, "audience": "a"}))
    .unwrap();
    let strict = p2_probe("");
    let r = strict.test(&auth, ManagedBy::Env, None).await.unwrap();
    assert!(r.report.ok, "{r:?}");
    let r = strict.test(&auth, ManagedBy::Ui, None).await.unwrap();
    assert!(
        !r.report.ok,
        "loopback must be refused for console-edited endpoints: {r:?}"
    );
}

/// Stats, audit search and delete with purge over real storage.
pub async fn stats_audit_purge(h: &Harness) {
    let (s, _, _) = h.create(json!({"name": "data"})).await;
    assert_eq!(s, StatusCode::CREATED);
    h.seed("data", "Patient", 3).await;
    h.seed("data", "Observation", 2).await;
    h.seed("data", "AuditEvent", 3).await;
    h.seed("default", "Patient", 1).await;

    let (s, st) = scan(&h.get("/admin/endpoints/data/stats?window=600").await);
    assert_eq!(s, StatusCode::OK, "{st}");
    assert_eq!(st["resources"]["Patient"], 3);
    assert_eq!(st["resources"]["Observation"], 2);
    assert_eq!(st["total"], 8);
    assert_eq!(st["traffic"]["windowSeconds"], 600);
    let (_, list) = scan(&h.get("/admin/endpoints?include=stats").await);
    let data = list["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "data")
        .unwrap()
        .clone();
    assert_eq!(data["stats"]["total"], 8);

    let (s, b) = scan(&h.get("/admin/endpoints/data/audit?_count=2").await);
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["resourceType"], "Bundle");
    assert_eq!(b["entry"].as_array().unwrap().len(), 2);
    let next = b["link"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["relation"] == "next")
        .map(|l| l["url"].as_str().unwrap().to_string())
        .expect("next link");
    let (s, b2) = scan(&h.get(&format!("/{next}")).await);
    assert_eq!(s, StatusCode::OK, "{b2}");
    assert_eq!(b2["entry"].as_array().unwrap().len(), 1);

    // Delete + purge: the tenant's data is gone, the default tenant's stays.
    let (s, gone) = scan(
        &h.delete("/admin/endpoints/data?confirm=data&purge=true")
            .await,
    );
    assert_eq!(s, StatusCode::OK, "{gone}");
    assert_eq!(gone["purged"], true);
    assert_eq!(gone["resourcesRemoved"], 8);
    let ctx = |t: &str| TenantContext::new(TenantId::new(t), TenantPermissions::full_access());
    assert!(!h.storage.tenant_has_resources(&ctx("data")).await.unwrap());
    assert!(
        h.storage
            .tenant_has_resources(&ctx("default"))
            .await
            .unwrap()
    );
    assert!(h.audited("purged tenant data (8 resources)"));
}

/// The endpoint cap.
pub async fn max_endpoints(h: &Harness) {
    let (s, _, _) = h.create(json!({"name": "one"})).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, o, _) = h.create(json!({"name": "two"})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{o}");
    assert!(o.to_string().contains("limit 2"));
}
