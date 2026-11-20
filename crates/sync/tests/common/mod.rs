//! Shared fixtures of the sync integration tests.
#![allow(dead_code)]

pub mod hapi;
pub mod hfs;

use std::sync::Arc;

use helios_endpoints::model::EndpointSpec;
use helios_endpoints::name::EndpointName;
use helios_endpoints::registry::EndpointRegistry;
use helios_endpoints::secret::SecretKeyRing;
use helios_endpoints::store::{Actor, EndpointStore, MemoryEndpointStore, ensure_default_endpoint};
use helios_endpoints::sync::SyncStore;
use helios_fhir::FhirVersion;
use helios_persistence::backends::sqlite::SqliteBackend;
use helios_persistence::core::cluster_job_store::testing::InMemoryClusterJobStore;
use helios_persistence::core::{ClusterJobStore, ResourceStorage, WorkerId};
use helios_sync::local::{LocalStore, tenant_context};
use helios_sync::memory::MemorySyncStore;
use helios_sync::{SyncConfig, SyncDeps, SyncService};
use serde_json::{Value, json};

/// One server's sync stack over SQLite.
pub struct Fixture {
    pub backend: Arc<SqliteBackend>,
    pub svc: Arc<SyncService>,
    pub endpoints: Arc<dyn EndpointStore>,
    pub registry: Arc<EndpointRegistry>,
    pub store: Arc<dyn SyncStore>,
    pub cluster: Arc<dyn ClusterJobStore>,
    pub handles: Vec<tokio::task::JoinHandle<()>>,
}

pub fn config(extra: &[(&str, &str)]) -> SyncConfig {
    let mut env: Vec<(String, String)> = vec![
        ("HFS_SYNC_SAFETY_LAG_MS".into(), "0".into()),
        ("HFS_OUTBOUND_ALLOW".into(), "127.0.0.1".into()),
        ("HFS_SYNC_ENABLED".into(), "false".into()),
    ];
    for (k, v) in extra {
        env.retain(|(a, _)| a != k);
        env.push((k.to_string(), v.to_string()));
    }
    SyncConfig::from_lookup(&move |k| env.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone()))
}

pub fn key_ring() -> Arc<SecretKeyRing> {
    Arc::new(SecretKeyRing::parse(&SecretKeyRing::generate_spec("test")).unwrap())
}

pub async fn endpoints_with(names: &[&str]) -> (Arc<dyn EndpointStore>, Arc<EndpointRegistry>) {
    let store: Arc<dyn EndpointStore> = Arc::new(MemoryEndpointStore::new());
    ensure_default_endpoint(store.as_ref(), "default", helios_endpoints::FhirRelease::R4)
        .await
        .unwrap();
    for n in names {
        let spec = EndpointSpec::named(EndpointName::parse(n).unwrap(), n.to_string());
        store.create(spec, &Actor::bootstrap()).await.unwrap();
    }
    let registry = Arc::new(EndpointRegistry::load(store.as_ref()).await.unwrap());
    (store, registry)
}

pub async fn fixture(cfg: SyncConfig) -> Fixture {
    let backend = Arc::new(SqliteBackend::in_memory().unwrap());
    backend.init_schema().unwrap();
    fixture_over(backend, cfg, &["a", "b", "c"]).await
}

pub async fn fixture_over(backend: Arc<SqliteBackend>, cfg: SyncConfig, names: &[&str]) -> Fixture {
    let (endpoints, registry) = endpoints_with(names).await;
    let store: Arc<dyn SyncStore> = Arc::new(MemorySyncStore::new());
    let cluster: Arc<dyn ClusterJobStore> = Arc::new(InMemoryClusterJobStore::new());
    let (svc, handles) = helios_sync::build(
        &cfg,
        SyncDeps {
            store: store.clone(),
            cluster: cluster.clone(),
            registry: registry.clone(),
            endpoints: Some(endpoints.clone()),
            local: Some(backend.clone() as Arc<dyn LocalStore>),
            copier: None,
            key_ring: Some(key_ring()),
            observer: None,
            audit: None,
            instance: "test".into(),
        },
    );
    Fixture {
        backend,
        svc,
        endpoints,
        registry,
        store,
        cluster,
        handles,
    }
}

impl Fixture {
    pub async fn put(&self, tenant: &str, rt: &str, id: &str, body: Value) {
        let mut r = body;
        r["resourceType"] = json!(rt);
        r["id"] = json!(id);
        ResourceStorage::create_or_update(self.backend.as_ref(), &tenant_context(tenant), rt, id, r, FhirVersion::R4)
            .await
            .unwrap();
    }

    pub async fn delete(&self, tenant: &str, rt: &str, id: &str) {
        ResourceStorage::delete(self.backend.as_ref(), &tenant_context(tenant), rt, id).await.unwrap();
    }

    pub async fn get(&self, tenant: &str, rt: &str, id: &str) -> Option<(String, Value)> {
        match ResourceStorage::read(self.backend.as_ref(), &tenant_context(tenant), rt, id).await {
            Ok(Some(s)) if !s.is_deleted() => Some((s.version_id().to_string(), s.content().clone())),
            _ => None,
        }
    }

    /// Works queued runs on this instance until none is claimable.
    pub async fn drain(&self) -> usize {
        let w = WorkerId::new("test-worker");
        let mut n = 0;
        while self.svc.runtime().work_next(&w).await.unwrap() {
            n += 1;
        }
        n
    }

    pub async fn create_job(&self, body: Value) -> String {
        let created = self.svc.create(body, "admin:test").await.unwrap();
        created.job["id"].as_str().unwrap().to_string()
    }

    pub async fn run_and_wait(&self, job: &str) -> helios_endpoints::sync::SyncRun {
        let run = self.svc.run_now(job, "admin:test").await.unwrap();
        self.drain().await;
        self.svc.run(&run.id).await.unwrap()
    }
}

pub fn local_job(name: &str, src: &str, dst: &str) -> Value {
    json!({
        "name": name,
        "source": {"kind": "endpoint", "name": src},
        "target": {"kind": "endpoint", "name": dst},
        "batchSize": 2,
    })
}
