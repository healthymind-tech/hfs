//! Stack-depth regression guard for the FHIR server manager dispatcher.
//!
//! Tokio worker threads get 2 MiB of stack. The heaviest request path —
//! `$validate` of a large Bundle with the manager on (dispatcher + endpoint
//! CORS), audit and authentication all installed — must complete on a thread
//! with exactly that much stack in a debug build (the deepest frames). The
//! request is driven on a dedicated 2 MiB thread with a current-thread
//! runtime, so the whole service stack is polled on that thread.
//!
//! Measurement aids (`hfs-eval/eval/scripts/manager-p1/stackprobe.sh`
//! bisects with them; a too-small stack aborts the process):
//! `HFS_TEST_STACK_KIB` sets the stack size, `HFS_TEST_STACK_MANAGER=off`
//! builds the same app without the manager, `HFS_TEST_STACK_GET=/path`
//! probes one GET instead, `HFS_TEST_STACK_ENTRIES` sizes the Bundle.
//!
//! Measured when written (debug, `debug=0`, KiB; manager off → on):
//! `$validate` of a Bundle 1864 → 1887 (independent of the Bundle size: the
//! depth is the validator's), `GET /Patient` 639 → 662, `GET /metadata`
//! 600 → 624. The dispatcher first cost +155 KiB on a search; most of it was
//! the audit `hfs-endpoint` extension built through serde, the rest the two
//! `from_fn` middlewares, now hand-written services.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::http::StatusCode;
use axum_test::TestServer;
use helios_audit::sinks::database::AuditStorage;
use helios_audit::{AuditConfig, AuditMiddlewareState, DatabaseSink, ExclusionFilter};
use helios_auth::{AuthConfig, AuthError, AuthProvider, Principal, ScopeSet};
use helios_endpoints::{
    Actor, EndpointName, EndpointRegistry, EndpointSpec, EndpointStore, FhirRelease,
    MemoryEndpointStore, ensure_default_endpoint,
};
use helios_fhir::FhirVersion;
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use helios_persistence::core::ResourceStorage;
use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
use helios_rest::endpoint::EndpointRuntime;
use helios_rest::{AuthMiddlewareState, OperationsBundle, ServerConfig};
use serde_json::{Value, json};

/// Tokio's default worker thread stack.
const WORKER_STACK: usize = 2 * 1024 * 1024;

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
            TenantId::new(tenant.unwrap_or("default")),
            TenantPermissions::full_access(),
        );
        self.0
            .create(&ctx, resource_type, resource, fhir_version)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

struct StubProvider;

#[async_trait]
impl AuthProvider for StubProvider {
    async fn authenticate(&self, header: &str) -> Result<Principal, AuthError> {
        if header != "Bearer writer" {
            return Err(AuthError::InvalidSignature);
        }
        Ok(Principal {
            subject: "u-1".into(),
            issuer: "https://idp.test".into(),
            tenant_id: None,
            scopes: ScopeSet::parse("system/*.cruds"),
            jti: None,
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
            custom_claims: serde_json::Map::new(),
            launch_context: None,
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
    .unwrap();
    backend.init_schema().unwrap();
    backend
}

async fn server() -> TestServer {
    let backend = backend();
    let sink = Arc::new(DatabaseSink::new(
        Arc::new(Adapter(backend.clone())),
        FhirVersion::default(),
    ));
    let audit = Arc::new(AuditMiddlewareState::new(sink, AuditConfig::default()));
    let auth = Arc::new(AuthMiddlewareState {
        provider: Arc::new(StubProvider),
        config: Arc::new(AuthConfig::default()),
        audit_sink: Arc::new(helios_audit::NullSink),
        audit_source_observer: "Device/hfs".into(),
        audit_exclusion_filter: ExclusionFilter::new(vec![]),
        tenant_url_routing: false,
        sessions: None,
        admin: None,
    });
    let store = Arc::new(MemoryEndpointStore::new());
    ensure_default_endpoint(store.as_ref(), "default", FhirRelease::R4)
        .await
        .unwrap();
    let mut lab = EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab");
    lab.validation = Some(Default::default());
    lab.cors = Some(Default::default());
    store.create(lab, &Actor::bootstrap()).await.unwrap();
    let registry = Arc::new(EndpointRegistry::load(store.as_ref()).await.unwrap());
    let app = helios_rest::create_app_with_auth_bulk_settings_and_ops(
        Arc::new(backend),
        ServerConfig {
            base_url: "https://hfs.example/fhir".into(),
            seed_conformance: false,
            ..ServerConfig::for_testing()
        },
        AuthConfig::default(),
        Some(auth),
        Some(audit),
        None,
        None,
        None,
        OperationsBundle {
            endpoints: (std::env::var("HFS_TEST_STACK_MANAGER").as_deref() != Ok("off"))
                .then(|| EndpointRuntime::new(registry, store as Arc<dyn EndpointStore>)),
            ..Default::default()
        },
    );
    // The request instrumentation `hfs` puts around everything.
    let app = if std::env::var("HFS_TEST_STACK_TRACK").as_deref() == Ok("off") {
        app
    } else {
        app.layer(axum::middleware::from_fn(
            helios_observability::middleware::track,
        ))
    };
    TestServer::new(app).unwrap()
}

/// A transaction-shaped Bundle of `n` Patients and Observations.
fn large_bundle(n: usize) -> Value {
    let mut entries = Vec::new();
    for i in 0..n {
        entries.push(json!({
            "fullUrl": format!("urn:uuid:00000000-0000-0000-0000-{i:012}"),
            "resource": {"resourceType": "Patient", "name": [{"family": format!("P{i}"), "given": ["A", "B"]}],
                         "gender": "female", "birthDate": "1980-01-01",
                         "telecom": [{"system": "phone", "value": "555"}]},
            "request": {"method": "POST", "url": "Patient"}
        }));
        entries.push(json!({
            "resource": {"resourceType": "Observation", "status": "final",
                         "code": {"coding": [{"system": "http://loinc.org", "code": "8867-4"}]},
                         "subject": {"reference": format!("urn:uuid:00000000-0000-0000-0000-{i:012}")},
                         "valueQuantity": {"value": 72, "unit": "/min", "system": "http://unitsofmeasure.org", "code": "/min"}},
            "request": {"method": "POST", "url": "Observation"}
        }));
    }
    json!({"resourceType": "Bundle", "type": "transaction", "entry": entries})
}

fn on_worker_sized_stack<F, Fut>(stack: usize, f: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()>,
{
    std::thread::Builder::new()
        .name("worker-2mib".into())
        .stack_size(
            std::env::var("HFS_TEST_STACK_KIB")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .map_or(stack, |kib| kib * 1024),
        )
        .spawn(move || {
            // The request is polled on this thread (`block_on`); the app's
            // background tasks (package manager, rehydration, dashboard)
            // run on the runtime's own workers, so they do not count.
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(16 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                // Spawned tasks live on the heap in a server; so does this one.
                .block_on(Box::pin(f()));
        })
        .unwrap()
        .join()
        .expect("request path panicked (a stack overflow aborts the process instead)");
}

#[test]
fn validate_large_bundle_fits_a_worker_stack_with_manager_audit_and_auth() {
    on_worker_sized_stack(WORKER_STACK, heaviest_path);
}

async fn heaviest_path() {
    {
        let server = server().await;
        let entries = std::env::var("HFS_TEST_STACK_ENTRIES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100);
        let body = large_bundle(entries);
        let manager = std::env::var("HFS_TEST_STACK_MANAGER").as_deref() != Ok("off");
        // Measurement aid: `HFS_TEST_STACK_GET=/path` probes one GET instead.
        if let Ok(path) = std::env::var("HFS_TEST_STACK_GET") {
            let res = server
                .get(&path)
                .add_header("authorization", "Bearer writer")
                .await;
            assert!(res.status_code().as_u16() < 500, "{}", res.text());
            return;
        }
        let paths: &[&str] = if manager {
            &["/Bundle/$validate", "/lab/Bundle/$validate"]
        } else {
            &["/Bundle/$validate"]
        };
        for path in paths.iter().copied() {
            let res = server
                .post(path)
                .add_header("authorization", "Bearer writer")
                .add_header("origin", "https://app.example")
                .json(&body)
                .await;
            assert_eq!(res.status_code(), StatusCode::OK, "{path}: {}", res.text());
        }
        if !manager {
            return;
        }
        // A write and a transaction on the named endpoint through the same stack.
        let res = server
            .post("/lab/Patient/$validate")
            .add_header("authorization", "Bearer writer")
            .json(&json!({"resourceType": "Patient", "active": true}))
            .await;
        assert_eq!(res.status_code(), StatusCode::OK, "{}", res.text());
        let res = server
            .post("/lab")
            .add_header("authorization", "Bearer writer")
            .json(&large_bundle(20))
            .await;
        assert_eq!(res.status_code(), StatusCode::OK, "{}", res.text());
    }
}
