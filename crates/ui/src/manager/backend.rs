//! The data port of the manager console.
//!
//! Every manager page reads and writes endpoints through [`ManagerBackend`],
//! never through a concrete store, so the console does not care whether the
//! data comes from:
//!
//! * the admin REST API of package P4 (`/admin/endpoints`, design §8) — the
//!   production adapter the `hfs` wiring installs once P4 lands, so writes
//!   are audited and secret inputs are handled in one place;
//! * [`StoreBackend`], a direct adapter over the P0 contracts
//!   ([`EndpointStore`], [`EndpointRegistry`], [`SyncStore`]) — what the
//!   console uses until then, in the HFS_MANAGER=off "default endpoint only"
//!   view, and in tests.
//!
//! The trait is shaped after the §8 routes (one method per route the console
//! needs) and speaks the P0 types, so an HTTP adapter is a thin translation.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use helios_endpoints::console::ConsoleAuthMode;
use helios_endpoints::model::FieldError;
use helios_endpoints::store::EndpointRevision;
use helios_endpoints::sync::{DuplicateRequest, SyncJob, SyncRun};
use helios_endpoints::{
    Actor, Endpoint, EndpointAuth, EndpointRegistry, EndpointSnapshot, EndpointSpec,
    EndpointStatus, EndpointStore, SecretKeyRing, StoreError, SyncStore,
};

/// Why a backend call failed, mapped onto what the console shows.
#[derive(Debug, Clone, PartialEq)]
pub enum BackendError {
    /// The endpoint (or job) does not exist.
    NotFound,
    /// Someone else saved first; re-read and retry (`409` / `412`).
    Conflict {
        /// The revision now stored.
        current: u64,
    },
    /// Field-level problems (`422`), one per field path (`auth.issuer`).
    Invalid(Vec<FieldError>),
    /// A unique field is taken.
    Duplicate {
        /// `name`, `path` or `storage.tenantId`.
        field: String,
        /// The value.
        value: String,
        /// The endpoint holding it.
        by: String,
    },
    /// Not allowed (env-managed, immutable field, default endpoint delete).
    Forbidden(String),
    /// The store or API is down.
    Unavailable(String),
    /// The operation is not available in this deployment yet (e.g.
    /// duplicate before package P6 lands).
    NotImplemented,
}

impl From<StoreError> for BackendError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound(_) => BackendError::NotFound,
            StoreError::Conflict { current } => BackendError::Conflict { current },
            StoreError::Duplicate { field, value, by } => BackendError::Duplicate {
                field: field.to_string(),
                value,
                by,
            },
            StoreError::Invalid(errs) => BackendError::Invalid(errs.0),
            StoreError::Forbidden(m) => BackendError::Forbidden(m),
            StoreError::Unavailable(m) => BackendError::Unavailable(m),
        }
    }
}

impl BackendError {
    /// Field errors keyed by field path, for mapping onto form inputs. A
    /// duplicate becomes an error on its field; everything else is a form-level
    /// error under the empty key.
    pub fn field_errors(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        match self {
            BackendError::Invalid(errs) => {
                for e in errs {
                    out.entry(e.field.clone())
                        .and_modify(|m: &mut String| {
                            m.push_str("; ");
                            m.push_str(&e.message);
                        })
                        .or_insert_with(|| e.message.clone());
                }
            }
            BackendError::Duplicate { field, value, by } => {
                out.insert(
                    field.clone(),
                    format!("{value:?} is already used by endpoint {by}"),
                );
            }
            BackendError::Forbidden(m) | BackendError::Unavailable(m) => {
                out.insert(String::new(), m.clone());
            }
            BackendError::Conflict { current } => {
                out.insert(
                    String::new(),
                    format!("changed by someone else (now revision {current})"),
                );
            }
            BackendError::NotFound => {
                out.insert(String::new(), "not found".into());
            }
            BackendError::NotImplemented => {
                out.insert(String::new(), "not available in this deployment".into());
            }
        }
        out
    }
}

/// A secret field change sent with a create/update (design §8 `SecretInput`):
/// absent = keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretChange {
    /// Replace with this plaintext (sealed by the backend).
    Set(String),
    /// Remove.
    Clear,
}

/// Secret changes by field path (`terminology.token`).
pub type SecretChanges = BTreeMap<String, SecretChange>;

/// Traffic figures of one endpoint (`GET /admin/endpoints/{name}/stats`).
/// Every field is optional: a deployment without request metrics shows "—".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EndpointStats {
    /// Requests per minute over the last 5 minutes.
    pub requests_per_minute: Option<f64>,
    /// Share of 5xx responses over the last hour (0..=1).
    pub error_rate: Option<f64>,
    /// 95th percentile latency over the last hour, milliseconds.
    pub p95_ms: Option<f64>,
    /// Last successful write.
    pub last_write: Option<DateTime<Utc>>,
    /// Requests per minute for the last hour, oldest first (sparkline).
    pub sparkline: Vec<u64>,
}

/// State of one dependency in the overview health strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HealthState {
    /// Working.
    Ok,
    /// Working, with a problem worth a look.
    Degraded,
    /// Not working.
    Down,
    /// Not configured / not reported.
    #[default]
    Unknown,
}

impl HealthState {
    /// CSS / i18n slug.
    pub fn slug(self) -> &'static str {
        match self {
            HealthState::Ok => "ok",
            HealthState::Degraded => "degraded",
            HealthState::Down => "down",
            HealthState::Unknown => "unknown",
        }
    }
}

/// Probe a terminology server's `/health` for the overview's health strip.
///
/// Before UIF the tile always read "Not reported" even with a healthy HTS
/// configured, because nothing ever filled [`ServerHealth::terminology`].
/// `2xx` is healthy, any other answer degraded, no answer (or a timeout of
/// three seconds) down.
pub async fn probe_terminology(base_url: &str) -> HealthState {
    let url = format!("{}/health", base_url.trim_end_matches('/'));
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    else {
        return HealthState::Unknown;
    };
    match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => HealthState::Ok,
        Ok(_) => HealthState::Degraded,
        Err(_) => HealthState::Down,
    }
}

/// The overview's health strip.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServerHealth {
    /// Primary database.
    pub database: HealthState,
    /// Round trip of the last health probe.
    pub database_latency_ms: Option<u64>,
    /// Terminology server.
    pub terminology: HealthState,
    /// Live instances in the cluster (heartbeat), when known.
    pub instances: Option<u32>,
    /// Whether `HFS_SECRET_KEYS` is configured.
    pub key_ring: bool,
    /// Console sign-in mode.
    pub console_mode: ConsoleAuthMode,
}

/// Outcome of a duplicate kick-off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateOutcome {
    /// The new endpoint's name.
    pub endpoint: String,
    /// The copy job, when data is being copied.
    pub job_id: Option<String>,
}

/// One line of a `$test-auth` probe report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeLine {
    /// Passed.
    pub ok: bool,
    /// What was checked and what came back.
    pub message: String,
}

/// A sync job with its most recent run, for the sync entry points.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncJobView {
    /// The job.
    pub job: SyncJob,
    /// Its newest run.
    pub last_run: Option<SyncRun>,
}

/// The console's data port. See the module docs.
#[async_trait]
pub trait ManagerBackend: Send + Sync {
    /// `GET /admin/endpoints` — every non-deleted endpoint, default first.
    async fn list(&self) -> Result<Vec<Endpoint>, BackendError>;

    /// `GET /admin/endpoints/{name}`.
    async fn get(&self, name: &str) -> Result<Option<Endpoint>, BackendError>;

    /// `POST /admin/endpoints`.
    async fn create(
        &self,
        spec: EndpointSpec,
        secrets: SecretChanges,
        actor: &Actor,
    ) -> Result<Endpoint, BackendError>;

    /// `PUT /admin/endpoints/{name}` with `If-Match: "{expected_revision}"`.
    async fn update(
        &self,
        name: &str,
        expected_revision: u64,
        spec: EndpointSpec,
        secrets: SecretChanges,
        actor: &Actor,
    ) -> Result<Endpoint, BackendError>;

    /// `POST $enable` / `$disable`, and `DELETE` (status `deleted`).
    async fn set_status(
        &self,
        name: &str,
        expected_revision: u64,
        status: EndpointStatus,
        actor: &Actor,
    ) -> Result<Endpoint, BackendError>;

    /// `GET /admin/endpoints/{name}/history`, newest first.
    async fn history(&self, name: &str) -> Result<Vec<EndpointRevision>, BackendError>;

    /// `POST /admin/endpoints/{name}/$duplicate`.
    async fn duplicate(
        &self,
        _source: &str,
        _request: DuplicateRequest,
        _actor: &Actor,
    ) -> Result<DuplicateOutcome, BackendError> {
        Err(BackendError::NotImplemented)
    }

    /// `POST /admin/endpoints/{name}/$test-auth` for an unsaved auth section.
    async fn test_auth(&self, _auth: &EndpointAuth) -> Result<Vec<ProbeLine>, BackendError> {
        Err(BackendError::NotImplemented)
    }

    /// `GET /admin/endpoints/{name}/stats`; `None` when not reported.
    async fn stats(&self, _name: &str) -> Option<EndpointStats> {
        None
    }

    /// Health strip of the overview.
    async fn health(&self) -> ServerHealth {
        ServerHealth::default()
    }

    /// `GET /admin/sync/jobs` (package P6), each with its newest run. Empty
    /// when sync is not available.
    async fn sync_jobs(&self) -> Result<Vec<SyncJobView>, BackendError> {
        Ok(Vec::new())
    }

    /// Whether sync is available at all (P6 installed).
    fn sync_available(&self) -> bool {
        false
    }

    /// Whether endpoint configuration can be written here (`false` for the
    /// read-only fallback and env-only deployments).
    fn writable(&self) -> bool {
        true
    }
}

/// [`ManagerBackend`] over the P0 store contracts, used until the P4 API
/// adapter is wired, and in tests. Writes go through the store's own
/// `check_write` rules; the registry is reloaded after every write so the
/// console (and, in-process, the dispatcher) sees the change at once.
pub struct StoreBackend {
    store: Arc<dyn EndpointStore>,
    registry: Option<Arc<EndpointRegistry>>,
    sync: Option<Arc<dyn SyncStore>>,
    key_ring: Option<Arc<SecretKeyRing>>,
    health: ServerHealth,
}

impl StoreBackend {
    /// A backend over `store`.
    pub fn new(store: Arc<dyn EndpointStore>) -> Self {
        Self {
            store,
            registry: None,
            sync: None,
            key_ring: None,
            health: ServerHealth::default(),
        }
    }

    /// Reload this registry after each write.
    pub fn with_registry(mut self, registry: Arc<EndpointRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Read sync jobs from this store.
    pub fn with_sync(mut self, sync: Arc<dyn SyncStore>) -> Self {
        self.sync = Some(sync);
        self
    }

    /// Seal secret inputs with this key ring (without one, setting a secret
    /// is refused, design D-12).
    pub fn with_key_ring(mut self, key_ring: Arc<SecretKeyRing>) -> Self {
        self.health.key_ring = true;
        self.key_ring = Some(key_ring);
        self
    }

    /// The health strip this backend reports.
    pub fn with_health(mut self, health: ServerHealth) -> Self {
        let key_ring = self.health.key_ring;
        self.health = health;
        self.health.key_ring = self.health.key_ring || key_ring;
        self
    }

    async fn reload(&self) {
        if let Some(registry) = &self.registry
            && let Err(error) = registry.reload_if_stale(self.store.as_ref()).await
        {
            tracing::warn!(%error, "manager console: registry reload after write failed");
        }
    }

    /// Applies secret changes to `spec` for endpoint `id`.
    fn apply_secrets(
        &self,
        id: &str,
        spec: &mut EndpointSpec,
        current: Option<&EndpointSpec>,
        secrets: &SecretChanges,
    ) -> Result<(), BackendError> {
        // Keep what is stored unless told otherwise: a form never round-trips
        // a sealed value.
        if let (Some(term), Some(cur)) = (
            spec.terminology.as_mut(),
            current.and_then(|c| c.terminology.as_ref()),
        ) && term.token.is_none()
        {
            term.token = cur.token.clone();
        }
        for (field, change) in secrets {
            match field.as_str() {
                "terminology.token" => {
                    let Some(term) = spec.terminology.as_mut() else {
                        continue;
                    };
                    match change {
                        SecretChange::Clear => term.token = None,
                        SecretChange::Set(plain) => {
                            let Some(ring) = &self.key_ring else {
                                return Err(BackendError::Invalid(vec![FieldError {
                                    field: field.clone(),
                                    message:
                                        "cannot store a secret: HFS_SECRET_KEYS is not configured"
                                            .into(),
                                }]));
                            };
                            term.token =
                                Some(ring.seal_str(plain, &format!("endpoint:{id}:{field}")));
                        }
                    }
                }
                other => {
                    return Err(BackendError::Invalid(vec![FieldError {
                        field: other.to_string(),
                        message: "unknown secret field".into(),
                    }]));
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl ManagerBackend for StoreBackend {
    async fn list(&self) -> Result<Vec<Endpoint>, BackendError> {
        Ok(self.store.list(false).await?)
    }

    async fn get(&self, name: &str) -> Result<Option<Endpoint>, BackendError> {
        Ok(self.store.get(name).await?)
    }

    async fn create(
        &self,
        spec: EndpointSpec,
        secrets: SecretChanges,
        actor: &Actor,
    ) -> Result<Endpoint, BackendError> {
        let wants_secret = secrets.values().any(|c| matches!(c, SecretChange::Set(_)));
        if wants_secret && self.key_ring.is_none() {
            // Refuse before creating anything (D-12).
            let mut probe = spec.clone();
            self.apply_secrets("", &mut probe, None, &secrets)?;
        }
        let created = self.store.create(spec, actor).await?;
        let created = if secrets.is_empty() {
            created
        } else {
            // Secrets are sealed with the endpoint id as AAD, known only now.
            let mut spec = created.spec.clone();
            self.apply_secrets(&created.id, &mut spec, None, &secrets)?;
            self.store
                .update(created.name(), created.revision, spec, actor)
                .await?
        };
        self.reload().await;
        Ok(created)
    }

    async fn update(
        &self,
        name: &str,
        expected_revision: u64,
        mut spec: EndpointSpec,
        secrets: SecretChanges,
        actor: &Actor,
    ) -> Result<Endpoint, BackendError> {
        let current = self.store.get(name).await?.ok_or(BackendError::NotFound)?;
        self.apply_secrets(&current.id, &mut spec, Some(&current.spec), &secrets)?;
        let updated = self
            .store
            .update(name, expected_revision, spec, actor)
            .await?;
        self.reload().await;
        Ok(updated)
    }

    async fn set_status(
        &self,
        name: &str,
        expected_revision: u64,
        status: EndpointStatus,
        actor: &Actor,
    ) -> Result<Endpoint, BackendError> {
        let updated = self
            .store
            .set_status(name, expected_revision, status, actor)
            .await?;
        self.reload().await;
        Ok(updated)
    }

    async fn history(&self, name: &str) -> Result<Vec<EndpointRevision>, BackendError> {
        let endpoint = self.store.get(name).await?.ok_or(BackendError::NotFound)?;
        Ok(self.store.history(&endpoint.id, 50).await?)
    }

    async fn health(&self) -> ServerHealth {
        let started = std::time::Instant::now();
        let mut health = self.health.clone();
        match self.store.epoch().await {
            Ok(_) => {
                if health.database == HealthState::Unknown {
                    health.database = HealthState::Ok;
                }
                health.database_latency_ms =
                    Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
            }
            Err(_) => health.database = HealthState::Down,
        }
        health
    }

    async fn sync_jobs(&self) -> Result<Vec<SyncJobView>, BackendError> {
        let Some(sync) = &self.sync else {
            return Ok(Vec::new());
        };
        let jobs = sync
            .jobs()
            .await
            .map_err(|e| BackendError::Unavailable(e.to_string()))?;
        let mut out = Vec::with_capacity(jobs.len());
        for job in jobs {
            let last_run = sync
                .runs(&job.id, 1)
                .await
                .ok()
                .and_then(|mut runs| (!runs.is_empty()).then(|| runs.remove(0)));
            out.push(SyncJobView { job, last_run });
        }
        Ok(out)
    }

    fn sync_available(&self) -> bool {
        self.sync.is_some()
    }
}

/// Read-only [`ManagerBackend`] over a fixed snapshot: the "default endpoint
/// only" view of a deployment without an endpoint store.
pub struct SnapshotBackend {
    snapshot: Arc<EndpointSnapshot>,
}

impl SnapshotBackend {
    /// Serves `snapshot`.
    pub fn new(snapshot: Arc<EndpointSnapshot>) -> Self {
        Self { snapshot }
    }
}

#[async_trait]
impl ManagerBackend for SnapshotBackend {
    async fn list(&self) -> Result<Vec<Endpoint>, BackendError> {
        Ok(self
            .snapshot
            .endpoints()
            .iter()
            .map(|e| e.as_ref().clone())
            .collect())
    }

    async fn get(&self, name: &str) -> Result<Option<Endpoint>, BackendError> {
        Ok(self.snapshot.by_name(name).map(|e| e.as_ref().clone()))
    }

    async fn create(
        &self,
        _spec: EndpointSpec,
        _secrets: SecretChanges,
        _actor: &Actor,
    ) -> Result<Endpoint, BackendError> {
        Err(BackendError::Forbidden(
            "endpoints are read-only in this deployment".into(),
        ))
    }

    async fn update(
        &self,
        _name: &str,
        _expected_revision: u64,
        _spec: EndpointSpec,
        _secrets: SecretChanges,
        _actor: &Actor,
    ) -> Result<Endpoint, BackendError> {
        Err(BackendError::Forbidden(
            "endpoints are read-only in this deployment".into(),
        ))
    }

    async fn set_status(
        &self,
        _name: &str,
        _expected_revision: u64,
        _status: EndpointStatus,
        _actor: &Actor,
    ) -> Result<Endpoint, BackendError> {
        Err(BackendError::Forbidden(
            "endpoints are read-only in this deployment".into(),
        ))
    }

    async fn history(&self, _name: &str) -> Result<Vec<EndpointRevision>, BackendError> {
        Ok(Vec::new())
    }

    fn writable(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_endpoints::{EndpointName, FhirRelease, MemoryEndpointStore};

    async fn serve(status: u16) -> String {
        let app = axum::Router::new().route(
            "/health",
            axum::routing::get(
                move || async move { axum::http::StatusCode::from_u16(status).unwrap() },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/")
    }

    #[tokio::test]
    async fn terminology_probe_reports_ok_degraded_and_down() {
        assert_eq!(probe_terminology(&serve(200).await).await, HealthState::Ok);
        assert_eq!(
            probe_terminology(&serve(503).await).await,
            HealthState::Degraded
        );
        assert_eq!(
            probe_terminology("http://127.0.0.1:1").await,
            HealthState::Down
        );
    }

    fn admin() -> Actor {
        Actor {
            subject: "admin".into(),
            display: None,
            via: "admin-password".into(),
        }
    }

    async fn backend() -> StoreBackend {
        let store: Arc<dyn EndpointStore> = Arc::new(MemoryEndpointStore::new());
        store
            .create(
                EndpointSpec::default_endpoint("default", FhirRelease::R4),
                &Actor::bootstrap(),
            )
            .await
            .unwrap();
        StoreBackend::new(store)
    }

    #[tokio::test]
    async fn create_update_status_and_history() {
        let b = backend().await;
        let spec = EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab");
        let created = b
            .create(spec, SecretChanges::new(), &admin())
            .await
            .unwrap();
        assert_eq!(created.revision, 1);
        let mut spec = created.spec.clone();
        spec.display_name = "Lab 2".into();
        let updated = b
            .update("lab", 1, spec.clone(), SecretChanges::new(), &admin())
            .await
            .unwrap();
        assert_eq!(updated.spec.display_name, "Lab 2");
        // Stale revision -> conflict carrying the current one.
        let err = b
            .update("lab", 1, spec, SecretChanges::new(), &admin())
            .await
            .unwrap_err();
        assert_eq!(err, BackendError::Conflict { current: 2 });
        assert!(err.field_errors().contains_key(""));
        let disabled = b
            .set_status("lab", 2, EndpointStatus::Disabled, &admin())
            .await
            .unwrap();
        assert_eq!(disabled.spec.status, EndpointStatus::Disabled);
        assert_eq!(b.history("lab").await.unwrap().len(), 3);
        assert_eq!(b.list().await.unwrap().len(), 2);
        assert!(b.health().await.database == HealthState::Ok);
    }

    #[tokio::test]
    async fn duplicate_names_map_to_a_field_error() {
        let b = backend().await;
        let spec = EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab");
        b.create(spec.clone(), SecretChanges::new(), &admin())
            .await
            .unwrap();
        let err = b
            .create(spec, SecretChanges::new(), &admin())
            .await
            .unwrap_err();
        let fields = err.field_errors();
        assert!(fields.contains_key("name"), "{fields:?}");
    }

    #[tokio::test]
    async fn secrets_need_a_key_ring_and_are_sealed() {
        let b = backend().await;
        let mut spec = EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab");
        spec.terminology = Some(Default::default());
        let mut secrets = SecretChanges::new();
        secrets.insert(
            "terminology.token".into(),
            SecretChange::Set("s3cret".into()),
        );
        let err = b
            .create(spec.clone(), secrets.clone(), &admin())
            .await
            .unwrap_err();
        assert!(err.field_errors().contains_key("terminology.token"));
        assert!(b.get("lab").await.unwrap().is_none(), "nothing created");

        let ring = SecretKeyRing::parse(&SecretKeyRing::generate_spec("k1")).unwrap();
        let b = backend().await.with_key_ring(Arc::new(ring));
        let created = b.create(spec, secrets, &admin()).await.unwrap();
        let token = created.spec.terminology.unwrap().token.unwrap();
        assert!(!token.envelope().contains("s3cret"));
        // A later save without a secret change keeps the sealed value.
        let mut spec = b.get("lab").await.unwrap().unwrap().spec;
        spec.terminology.as_mut().unwrap().token = None;
        let kept = b
            .update("lab", 2, spec, SecretChanges::new(), &admin())
            .await
            .unwrap();
        assert!(kept.spec.terminology.unwrap().token.is_some());
    }

    #[tokio::test]
    async fn snapshot_backend_is_read_only() {
        let default = Endpoint {
            id: "d".into(),
            revision: 1,
            spec: EndpointSpec::default_endpoint("default", FhirRelease::R4),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            created_by: None,
            updated_by: None,
        };
        let snap = Arc::new(EndpointSnapshot::new(1, vec![default]).unwrap());
        let b = SnapshotBackend::new(snap);
        assert!(!b.writable());
        assert_eq!(b.list().await.unwrap().len(), 1);
        assert!(b.get("default").await.unwrap().is_some());
        assert!(matches!(
            b.set_status("default", 1, EndpointStatus::Disabled, &admin())
                .await,
            Err(BackendError::Forbidden(_))
        ));
        assert_eq!(
            b.duplicate(
                "default",
                serde_json::from_str(r#"{"name":"x"}"#).unwrap(),
                &admin()
            )
            .await,
            Err(BackendError::NotImplemented)
        );
    }
}
