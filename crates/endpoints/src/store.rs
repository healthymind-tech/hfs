//! Persistence contract for endpoint configuration.
//!
//! [`EndpointStore`] is implemented by [`MemoryEndpointStore`] (tests,
//! non-PostgreSQL single-instance deployments, `HFS_ENDPOINTS_FILE`) and by
//! the PostgreSQL store over `hfs_endpoints` / `hfs_endpoint_revisions` /
//! `hfs_config_epochs` (fork migration `hm-0008-endpoints`; package P1,
//! `crates/endpoints/src/pg.rs`, feature `postgres`). Both must pass the same
//! behavioural tests ([`conformance`]).
//!
//! Semantics every implementation guarantees:
//!
//! * `create` validates the spec ([`EndpointSpec::validate`]), refuses a
//!   second default endpoint, a non-default endpoint named `default`, and
//!   any name / path / tenant already used by a non-deleted endpoint
//!   ([`StoreError::Duplicate`]); assigns a UUID and revision 1.
//! * `update` is compare-and-set on `expected_revision`
//!   ([`StoreError::Conflict`] carries the current revision); `name` and
//!   `storage` are immutable; a `managed_by = env` endpoint is refused unless
//!   the actor is the bootstrap ([`Actor::bootstrap`]).
//! * every successful write appends one revision record (full redacted spec)
//!   and advances [`EndpointStore::epoch`] — the number every instance polls
//!   to know its routing snapshot is stale.
//! * deletion is `set_status(Deleted)`: the row stays, its name, path and
//!   tenant become reusable.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{Endpoint, EndpointSpec, EndpointStatus, ManagedBy, ValidationErrors};

/// Who made a change (recorded in revisions, `updated_by`, audit).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Actor {
    /// Stable subject: `local:{username}`, `oidc:{iss}|{sub}`, `admin`
    /// (break-glass password), `token:{iss}|{sub}`, `system:bootstrap`.
    pub subject: String,
    /// Display name, if known.
    pub display: Option<String>,
    /// How the actor authenticated: `console-session`, `admin-password`,
    /// `bearer`, `bootstrap`.
    pub via: String,
}

impl Actor {
    /// The startup bootstrap (default endpoint creation, `HFS_ENDPOINTS_FILE`).
    pub fn bootstrap() -> Self {
        Self {
            subject: "system:bootstrap".into(),
            display: None,
            via: "bootstrap".into(),
        }
    }

    /// Whether this is the bootstrap actor.
    pub fn is_bootstrap(&self) -> bool {
        self.via == "bootstrap" && self.subject == "system:bootstrap"
    }
}

/// Kind of change a revision records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    /// Created.
    Create,
    /// Spec changed.
    Update,
    /// Status changed (enable / disable / delete).
    Status,
}

/// One entry of an endpoint's change history (`hfs_endpoint_revisions`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointRevision {
    /// Endpoint id.
    pub endpoint_id: String,
    /// Revision this change produced.
    pub revision: u64,
    /// Kind.
    pub change: ChangeKind,
    /// The spec after the change, secrets redacted.
    pub spec: serde_json::Value,
    /// When.
    pub changed_at: DateTime<Utc>,
    /// Actor subject.
    pub changed_by: Option<String>,
}

/// Store failures, mapped by the admin API to HTTP statuses.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum StoreError {
    /// `404`.
    #[error("endpoint {0} not found")]
    NotFound(String),
    /// `409` with the current revision (client re-reads and retries).
    #[error("revision conflict: current revision is {current}")]
    Conflict {
        /// The stored revision.
        current: u64,
    },
    /// `409`: a unique field is taken.
    #[error("{field} {value:?} is already used by endpoint {by}")]
    Duplicate {
        /// `name`, `path` or `storage.tenantId`.
        field: &'static str,
        /// The value.
        value: String,
        /// Name of the endpoint holding it.
        by: String,
    },
    /// `422`.
    #[error(transparent)]
    Invalid(#[from] ValidationErrors),
    /// `409`: change not allowed (immutable field, env-managed, default
    /// endpoint deletion…).
    #[error("{0}")]
    Forbidden(String),
    /// `503`.
    #[error("endpoint store unavailable: {0}")]
    Unavailable(String),
}

/// Endpoint configuration persistence. Object-safe; held as
/// `Arc<dyn EndpointStore>`.
#[async_trait]
pub trait EndpointStore: Send + Sync {
    /// Monotonic change counter (`hfs_config_epochs.epoch` for scope
    /// `endpoints`). Polled by every instance; cheap.
    async fn epoch(&self) -> Result<u64, StoreError>;

    /// Every endpoint (deleted ones only when asked), default first then by name.
    async fn list(&self, include_deleted: bool) -> Result<Vec<Endpoint>, StoreError>;

    /// One non-deleted endpoint by name.
    async fn get(&self, name: &str) -> Result<Option<Endpoint>, StoreError>;

    /// Creates an endpoint.
    async fn create(&self, spec: EndpointSpec, actor: &Actor) -> Result<Endpoint, StoreError>;

    /// Replaces the spec of `name` if its revision is `expected_revision`.
    async fn update(
        &self,
        name: &str,
        expected_revision: u64,
        spec: EndpointSpec,
        actor: &Actor,
    ) -> Result<Endpoint, StoreError>;

    /// Changes only the status (enable / disable / provisioning / delete).
    async fn set_status(
        &self,
        name: &str,
        expected_revision: u64,
        status: EndpointStatus,
        actor: &Actor,
    ) -> Result<Endpoint, StoreError>;

    /// Change history of an endpoint by id, newest first.
    async fn history(
        &self,
        endpoint_id: &str,
        limit: usize,
    ) -> Result<Vec<EndpointRevision>, StoreError>;
}

/// Shared create/update checks over the current set of endpoints, so every
/// store implementation enforces the same rules. `existing` excludes deleted
/// endpoints; `replacing` is the id being updated (skipped in uniqueness).
pub fn check_write(
    spec: &EndpointSpec,
    existing: &[&Endpoint],
    replacing: Option<&Endpoint>,
    actor: &Actor,
) -> Result<(), StoreError> {
    spec.validate()?;
    if let Some(cur) = replacing {
        if cur.spec.name != spec.name {
            return Err(StoreError::Forbidden("name is immutable".into()));
        }
        if cur.spec.storage != spec.storage {
            return Err(StoreError::Forbidden(
                "storage is immutable; duplicate the endpoint to move its data".into(),
            ));
        }
        if cur.spec.managed_by == ManagedBy::Env && !actor.is_bootstrap() {
            return Err(StoreError::Forbidden(
                "endpoint is managed by HFS_ENDPOINTS_FILE; edit the file and restart".into(),
            ));
        }
        if cur.spec.is_default() && spec.status == EndpointStatus::Deleted {
            return Err(StoreError::Forbidden(
                "the default endpoint cannot be deleted (disable it instead)".into(),
            ));
        }
    } else if spec.managed_by == ManagedBy::Env && !actor.is_bootstrap() {
        return Err(StoreError::Forbidden(
            "managedBy=env is reserved for HFS_ENDPOINTS_FILE".into(),
        ));
    }
    if spec.status == EndpointStatus::Deleted && replacing.is_none() {
        return Err(StoreError::Forbidden(
            "cannot create a deleted endpoint".into(),
        ));
    }
    for e in existing {
        if replacing.is_some_and(|r| r.id == e.id) {
            continue;
        }
        if e.spec.name == spec.name {
            return Err(StoreError::Duplicate {
                field: "name",
                value: spec.name.to_string(),
                by: e.name().into(),
            });
        }
        if e.spec.path == spec.path {
            return Err(StoreError::Duplicate {
                field: "path",
                value: spec.path.to_string(),
                by: e.name().into(),
            });
        }
        if e.tenant_id() == spec.storage.tenant_id() {
            return Err(StoreError::Duplicate {
                field: "storage.tenantId",
                value: spec.storage.tenant_id().into(),
                by: e.name().into(),
            });
        }
    }
    Ok(())
}

/// In-memory [`EndpointStore`].
#[derive(Default)]
pub struct MemoryEndpointStore {
    inner: Mutex<MemInner>,
}

#[derive(Default)]
struct MemInner {
    epoch: u64,
    endpoints: Vec<Endpoint>,
    revisions: HashMap<String, Vec<EndpointRevision>>,
}

impl MemoryEndpointStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

fn redacted_spec(spec: &EndpointSpec) -> serde_json::Value {
    let mut v = serde_json::to_value(spec).expect("spec serializes");
    crate::secret::redact_sealed(&mut v);
    v
}

impl MemInner {
    fn live(&self) -> Vec<&Endpoint> {
        self.endpoints
            .iter()
            .filter(|e| e.spec.status != EndpointStatus::Deleted)
            .collect()
    }

    fn record(&mut self, e: &Endpoint, change: ChangeKind, actor: &Actor) {
        self.epoch += 1;
        self.revisions
            .entry(e.id.clone())
            .or_default()
            .push(EndpointRevision {
                endpoint_id: e.id.clone(),
                revision: e.revision,
                change,
                spec: redacted_spec(&e.spec),
                changed_at: e.updated_at,
                changed_by: Some(actor.subject.clone()),
            });
    }

    fn live_index(&self, name: &str) -> Option<usize> {
        self.endpoints
            .iter()
            .position(|e| e.name() == name && e.spec.status != EndpointStatus::Deleted)
    }
}

fn sorted(mut v: Vec<Endpoint>) -> Vec<Endpoint> {
    v.sort_by(|a, b| {
        (!a.spec.is_default(), a.name(), a.created_at).cmp(&(
            !b.spec.is_default(),
            b.name(),
            b.created_at,
        ))
    });
    v
}

#[async_trait]
impl EndpointStore for MemoryEndpointStore {
    async fn epoch(&self) -> Result<u64, StoreError> {
        Ok(self.inner.lock().expect("store lock").epoch)
    }

    async fn list(&self, include_deleted: bool) -> Result<Vec<Endpoint>, StoreError> {
        let g = self.inner.lock().expect("store lock");
        Ok(sorted(
            g.endpoints
                .iter()
                .filter(|e| include_deleted || e.spec.status != EndpointStatus::Deleted)
                .cloned()
                .collect(),
        ))
    }

    async fn get(&self, name: &str) -> Result<Option<Endpoint>, StoreError> {
        let g = self.inner.lock().expect("store lock");
        Ok(g.live_index(name).map(|i| g.endpoints[i].clone()))
    }

    async fn create(&self, spec: EndpointSpec, actor: &Actor) -> Result<Endpoint, StoreError> {
        let mut g = self.inner.lock().expect("store lock");
        check_write(&spec, &g.live(), None, actor)?;
        let now = Utc::now();
        let e = Endpoint {
            id: uuid::Uuid::new_v4().to_string(),
            revision: 1,
            spec,
            created_at: now,
            updated_at: now,
            created_by: Some(actor.subject.clone()),
            updated_by: Some(actor.subject.clone()),
        };
        g.record(&e, ChangeKind::Create, actor);
        g.endpoints.push(e.clone());
        Ok(e)
    }

    async fn update(
        &self,
        name: &str,
        expected_revision: u64,
        spec: EndpointSpec,
        actor: &Actor,
    ) -> Result<Endpoint, StoreError> {
        let mut g = self.inner.lock().expect("store lock");
        let i = g
            .live_index(name)
            .ok_or_else(|| StoreError::NotFound(name.into()))?;
        let cur = g.endpoints[i].clone();
        if cur.revision != expected_revision {
            return Err(StoreError::Conflict {
                current: cur.revision,
            });
        }
        check_write(&spec, &g.live(), Some(&cur), actor)?;
        let e = Endpoint {
            revision: cur.revision + 1,
            spec,
            updated_at: Utc::now(),
            updated_by: Some(actor.subject.clone()),
            ..cur
        };
        g.record(&e, ChangeKind::Update, actor);
        g.endpoints[i] = e.clone();
        Ok(e)
    }

    async fn set_status(
        &self,
        name: &str,
        expected_revision: u64,
        status: EndpointStatus,
        actor: &Actor,
    ) -> Result<Endpoint, StoreError> {
        let mut g = self.inner.lock().expect("store lock");
        let i = g
            .live_index(name)
            .ok_or_else(|| StoreError::NotFound(name.into()))?;
        let cur = g.endpoints[i].clone();
        if cur.revision != expected_revision {
            return Err(StoreError::Conflict {
                current: cur.revision,
            });
        }
        let mut spec = cur.spec.clone();
        spec.status = status;
        check_write(&spec, &g.live(), Some(&cur), actor)?;
        let e = Endpoint {
            revision: cur.revision + 1,
            spec,
            updated_at: Utc::now(),
            updated_by: Some(actor.subject.clone()),
            ..cur
        };
        g.record(&e, ChangeKind::Status, actor);
        g.endpoints[i] = e.clone();
        Ok(e)
    }

    async fn history(
        &self,
        endpoint_id: &str,
        limit: usize,
    ) -> Result<Vec<EndpointRevision>, StoreError> {
        let g = self.inner.lock().expect("store lock");
        Ok(g.revisions
            .get(endpoint_id)
            .map(|v| v.iter().rev().take(limit).cloned().collect())
            .unwrap_or_default())
    }
}

/// Creates the default endpoint if the store has none (startup, every
/// instance; the unique indexes make concurrent attempts safe — a
/// `Duplicate` from a racing instance is success).
pub async fn ensure_default_endpoint(
    store: &dyn EndpointStore,
    default_tenant: &str,
    fhir_version: crate::model::FhirRelease,
) -> Result<Endpoint, StoreError> {
    if let Some(e) = store.get(crate::name::DEFAULT_ENDPOINT_NAME).await? {
        return Ok(e);
    }
    match store
        .create(
            EndpointSpec::default_endpoint(default_tenant, fhir_version),
            &Actor::bootstrap(),
        )
        .await
    {
        Ok(e) => Ok(e),
        Err(StoreError::Duplicate { .. }) => store
            .get(crate::name::DEFAULT_ENDPOINT_NAME)
            .await?
            .ok_or_else(|| StoreError::Unavailable("default endpoint vanished".into())),
        Err(e) => Err(e),
    }
}

/// Behavioural tests every [`EndpointStore`] implementation must pass. The
/// PostgreSQL store calls these from its `postgres_integration_*` tests.
pub mod conformance {
    use super::*;
    use crate::model::{EndpointAuth, FhirRelease};
    use crate::name::{EndpointName, EndpointPath};

    fn admin() -> Actor {
        Actor {
            subject: "local:alice".into(),
            display: Some("Alice".into()),
            via: "console-session".into(),
        }
    }

    /// Runs every check against an **empty** store.
    pub async fn run_all(store: &dyn EndpointStore) {
        lifecycle(store).await;
    }

    async fn lifecycle(store: &dyn EndpointStore) {
        let e0 = store.epoch().await.unwrap();
        let d = ensure_default_endpoint(store, "default", FhirRelease::R4)
            .await
            .unwrap();
        assert!(d.spec.is_default());
        assert_eq!(d.revision, 1);
        assert!(store.epoch().await.unwrap() > e0);
        // Idempotent.
        assert_eq!(
            ensure_default_endpoint(store, "default", FhirRelease::R4)
                .await
                .unwrap()
                .id,
            d.id
        );
        // A second default is refused.
        let dup = store
            .create(
                EndpointSpec::default_endpoint("other", FhirRelease::R4),
                &admin(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(dup, StoreError::Duplicate { field: "name", .. }),
            "{dup:?}"
        );

        let tw = store
            .create(
                EndpointSpec::named(EndpointName::parse("twcore").unwrap(), "TW Core"),
                &admin(),
            )
            .await
            .unwrap();
        assert_eq!(tw.created_by.as_deref(), Some("local:alice"));

        // Path and tenant uniqueness.
        let mut clash = EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab");
        clash.path = EndpointPath::parse("twcore").unwrap();
        assert!(matches!(
            store.create(clash, &admin()).await.unwrap_err(),
            StoreError::Duplicate { field: "path", .. }
        ));
        let mut clash = EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab");
        clash.storage = tw.spec.storage.clone();
        assert!(matches!(
            store.create(clash, &admin()).await.unwrap_err(),
            StoreError::Duplicate {
                field: "storage.tenantId",
                ..
            }
        ));
        // Invalid spec.
        let mut bad = EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab");
        bad.auth = Some(EndpointAuth {
            mode: crate::model::AuthMode::Full,
            ..Default::default()
        });
        assert!(matches!(
            store.create(bad, &admin()).await.unwrap_err(),
            StoreError::Invalid(_)
        ));

        // CAS update.
        let mut spec = tw.spec.clone();
        spec.display_name = "Taiwan Core".into();
        let tw2 = store
            .update("twcore", 1, spec.clone(), &admin())
            .await
            .unwrap();
        assert_eq!(tw2.revision, 2);
        assert_eq!(tw2.id, tw.id);
        assert_eq!(
            store
                .update("twcore", 1, spec.clone(), &admin())
                .await
                .unwrap_err(),
            StoreError::Conflict { current: 2 }
        );
        // Immutable fields.
        let mut moved = tw2.spec.clone();
        moved.storage = crate::model::StorageBinding::Tenant {
            tenant_id: "elsewhere".into(),
        };
        assert!(matches!(
            store
                .update("twcore", 2, moved, &admin())
                .await
                .unwrap_err(),
            StoreError::Forbidden(_)
        ));
        // Path rename is allowed.
        let mut renamed = tw2.spec.clone();
        renamed.path = EndpointPath::parse("tw").unwrap();
        let tw3 = store.update("twcore", 2, renamed, &admin()).await.unwrap();
        assert_eq!(tw3.spec.path.as_str(), "tw");

        // Default cannot be deleted.
        assert!(matches!(
            store
                .set_status("default", d.revision, EndpointStatus::Deleted, &admin())
                .await
                .unwrap_err(),
            StoreError::Forbidden(_)
        ));
        // Delete frees name, path and tenant.
        let gone = store
            .set_status("twcore", 3, EndpointStatus::Deleted, &admin())
            .await
            .unwrap();
        assert_eq!(gone.spec.status, EndpointStatus::Deleted);
        assert!(store.get("twcore").await.unwrap().is_none());
        assert_eq!(store.list(false).await.unwrap().len(), 1);
        assert_eq!(store.list(true).await.unwrap().len(), 2);
        let again = store
            .create(
                EndpointSpec::named(EndpointName::parse("twcore").unwrap(), "TW Core"),
                &admin(),
            )
            .await
            .unwrap();
        assert_ne!(again.id, tw.id);

        // History, newest first, secrets redacted.
        let h = store.history(&tw.id, 10).await.unwrap();
        assert_eq!(
            h.iter().map(|r| r.revision).collect::<Vec<_>>(),
            vec![4, 3, 2, 1]
        );
        assert_eq!(h[0].change, ChangeKind::Status);
        assert_eq!(h[3].change, ChangeKind::Create);

        // Env-managed endpoints are read-only for everyone but the bootstrap.
        let mut env = EndpointSpec::named(EndpointName::parse("pinned").unwrap(), "Pinned");
        env.managed_by = ManagedBy::Env;
        assert!(matches!(
            store.create(env.clone(), &admin()).await.unwrap_err(),
            StoreError::Forbidden(_)
        ));
        let pinned = store
            .create(env.clone(), &Actor::bootstrap())
            .await
            .unwrap();
        env.display_name = "Changed".into();
        assert!(matches!(
            store
                .update("pinned", pinned.revision, env, &admin())
                .await
                .unwrap_err(),
            StoreError::Forbidden(_)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_store_conformance() {
        conformance::run_all(&MemoryEndpointStore::new()).await;
    }
}
