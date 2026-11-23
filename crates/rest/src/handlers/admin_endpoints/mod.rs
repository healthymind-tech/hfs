//! Admin REST API for FHIR endpoints: `/admin/endpoints` (package P4 of
//! `docs/design/fhir-server-manager.md`, §8). OpenAPI:
//! `docs/api/admin-endpoints.yaml` (kept in step with the router by the
//! `admin_endpoints_openapi` test).
//!
//! | Method & path | |
//! |---|---|
//! | `GET /admin/endpoints[?include=deleted,stats]` | list (redacted) |
//! | `POST /admin/endpoints` | create → `201`, `Location`, `ETag` |
//! | `GET /admin/endpoints/{name}` | one (redacted), `ETag` (`If-None-Match` → `304`) |
//! | `PUT /admin/endpoints/{name}` | replace (`If-Match` required) |
//! | `PATCH /admin/endpoints/{name}` | JSON merge patch (`If-Match` required) |
//! | `DELETE /admin/endpoints/{name}?confirm={name}[&purge=true]` | tombstone (+ tenant purge) |
//! | `POST /admin/endpoints/{name}/$enable`, `$disable` | status (`If-Match` optional) |
//! | `GET /admin/endpoints/{name}/history` | revisions (redacted) |
//! | `POST /admin/endpoints/{name}/$duplicate` | hand-off to the sync service → `202` |
//! | `POST /admin/endpoints/{name}/$test-auth` | issuer discovery + JWKS probe |
//! | `POST /admin/endpoints/{name}/$verify-auth`, `POST /admin/endpoints/$verify-auth` | step-by-step auth verification (`VerifyReport`) |
//! | `POST /admin/endpoints/$verify-terminology` | terminology server verification (`VerifyReport`) |
//! | `GET /admin/endpoints/{name}/stats` | counts by type, last write, traffic |
//! | `GET\|POST /admin/endpoints/{name}/packages`, `GET\|DELETE …/packages/{pkg}/{ver}` | package API in the endpoint's tenant |
//! | `GET /admin/endpoints/{name}/audit` | AuditEvents of the endpoint's tenant (Bundle) |
//! | `POST /admin/secrets/$rewrap` | re-seal every secret under the active key |
//!
//! Concurrency is optimistic: every endpoint response carries
//! `ETag: "<revision>"`; `PUT`/`PATCH` must send it back as `If-Match`
//! (`428` without, `412` when stale), and a write that loses a race in the
//! store answers `409` with the current revision. Every successful write
//! reloads this instance's routing snapshot before answering (other
//! instances follow through the config epoch, design §5.3), so the change is
//! visible on the next request. Writes are audited ([`audit`]); secrets are
//! write-only ([`spec_input`], [`secrets`]); URLs the server fetches pass the
//! SSRF policy ([`outbound`]); errors are OperationOutcomes ([`error`]);
//! authorization is pluggable ([`authz`]).
//!
//! Every rule lives in [`service`] (methods on [`AdminEndpointsState`]); the
//! HTTP handlers in [`endpoints`] only parse and render, so the manager
//! console can call the same service and get the same validation, errors
//! ([`error::ErrorKind`]) and audit trail.

pub mod audit;
pub mod authz;
pub mod endpoints;
pub mod error;
pub mod hooks;
pub mod outbound;
pub mod packages;
pub mod secrets;
pub mod service;
pub mod spec_input;

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::{HeaderMap, header};
use helios_audit::AuditSink;
use helios_endpoints::{Actor, EndpointRegistry, EndpointStore, OutboundPolicy, SecretKeyRing};
use helios_persistence::core::{ResourceStorage, SearchProvider, WriteObservers};
use serde::{Deserialize, Serialize};

use self::audit::AdminAudit;
use self::authz::{AdminAuthorizer, AdminCredentialAuthorizer};
use self::error::{ApiError, ApiResult};
use self::hooks::{
    AuditSearch, DuplicateService, EndpointAuthProbe, StorageAuditSearch, StorageTenantOps,
    TenantOps, UnavailableDuplicate, UnavailableProbe,
};
use self::outbound::{HostResolver, SystemResolver};
use crate::packages::PackageManager;

/// Most endpoints one deployment may have (design §5.2, metrics label bound).
pub const DEFAULT_MAX_ENDPOINTS: usize = 200;

/// The `ETag` of a revision.
pub fn etag(revision: u64) -> String {
    format!("\"{revision}\"")
}

/// What an `If-Match` header asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IfMatch {
    /// No header.
    Absent,
    /// `*`.
    Any,
    /// A revision.
    Revision(u64),
    /// Something that is not a revision ETag (never matches).
    Other(String),
}

/// Parses `If-Match` (`"3"`, `W/"3"`, `3`, `*`).
pub fn if_match(headers: &HeaderMap) -> IfMatch {
    let Some(raw) = headers.get(header::IF_MATCH).and_then(|v| v.to_str().ok()) else {
        return IfMatch::Absent;
    };
    let raw = raw.trim();
    if raw == "*" {
        return IfMatch::Any;
    }
    let bare = raw.strip_prefix("W/").unwrap_or(raw).trim_matches('"');
    match bare.parse::<u64>() {
        Ok(r) => IfMatch::Revision(r),
        Err(_) => IfMatch::Other(raw.to_string()),
    }
}

/// The revision a write may replace: `current` when `If-Match` matches it
/// (or is `*`, or absent and not `required`).
pub fn expected_revision(headers: &HeaderMap, current: u64, required: bool) -> ApiResult<u64> {
    match if_match(headers) {
        IfMatch::Absent if required => Err(ApiError::precondition_required()),
        IfMatch::Absent | IfMatch::Any => Ok(current),
        IfMatch::Revision(r) if r == current => Ok(current),
        IfMatch::Revision(r) => Err(ApiError::precondition_failed(current, &etag(r))),
        IfMatch::Other(s) => Err(ApiError::precondition_failed(current, &s)),
    }
}

/// Outcome of re-sealing one scope of secrets (`$rewrap`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RewrapReport {
    /// What was re-sealed (`endpoints`, `console`, `sync`).
    pub scope: String,
    /// Secrets re-sealed under the active key.
    pub rewrapped: usize,
    /// Secrets already under the active key.
    pub current: usize,
    /// Failures (`object/field: reason`).
    pub failed: Vec<String>,
}

/// Another owner of sealed secrets that `POST /admin/secrets/$rewrap` should
/// re-seal (console settings — P3, sync jobs — P6).
#[async_trait]
pub trait RewrapParticipant: Send + Sync {
    /// Re-seals every secret not under `ring`'s active key.
    async fn rewrap(&self, ring: &SecretKeyRing, actor: &Actor) -> RewrapReport;
}

/// Shared state of the endpoint admin routes.
#[derive(Clone)]
pub struct AdminEndpointsState {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) store: Arc<dyn EndpointStore>,
    pub(crate) registry: Arc<EndpointRegistry>,
    pub(crate) key_ring: Option<Arc<SecretKeyRing>>,
    pub(crate) outbound: OutboundPolicy,
    pub(crate) resolver: Arc<dyn HostResolver>,
    pub(crate) authorizer: Arc<dyn AdminAuthorizer>,
    pub(crate) packages: Option<Arc<PackageManager>>,
    pub(crate) package_upload_limit: usize,
    pub(crate) tenant_ops: Option<Arc<dyn TenantOps>>,
    pub(crate) audit_search: Option<Arc<dyn AuditSearch>>,
    pub(crate) audit: AdminAudit,
    pub(crate) duplicate: Arc<dyn DuplicateService>,
    pub(crate) auth_probe: Arc<dyn EndpointAuthProbe>,
    pub(crate) rewrap_participants: Vec<Arc<dyn RewrapParticipant>>,
    pub(crate) max_endpoints: usize,
    pub(crate) server_base: String,
}

impl AdminEndpointsState {
    /// Starts a builder over the endpoint store and the routing registry.
    pub fn builder(
        store: Arc<dyn EndpointStore>,
        registry: Arc<EndpointRegistry>,
    ) -> AdminEndpointsStateBuilder {
        AdminEndpointsStateBuilder {
            inner: Inner {
                store,
                registry,
                key_ring: None,
                outbound: OutboundPolicy::strict(),
                resolver: Arc::new(SystemResolver),
                authorizer: Arc::new(AdminCredentialAuthorizer::new(None)),
                packages: None,
                package_upload_limit: 256 * 1024 * 1024,
                tenant_ops: None,
                audit_search: None,
                audit: AdminAudit::new(None, "Device/hfs", "default"),
                duplicate: Arc::new(UnavailableDuplicate),
                auth_probe: Arc::new(UnavailableProbe),
                rewrap_participants: Vec::new(),
                max_endpoints: DEFAULT_MAX_ENDPOINTS,
                server_base: String::new(),
            },
        }
    }

    pub(crate) fn inner(&self) -> &Inner {
        &self.inner
    }

    /// Package upload body limit (for the router).
    pub fn package_upload_limit(&self) -> usize {
        self.inner.package_upload_limit
    }
}

/// Builder of [`AdminEndpointsState`].
pub struct AdminEndpointsStateBuilder {
    inner: Inner,
}

impl AdminEndpointsStateBuilder {
    /// The secret key ring (`HFS_SECRET_KEYS`); without one, setting a secret
    /// answers `409`.
    pub fn key_ring(mut self, ring: Option<Arc<SecretKeyRing>>) -> Self {
        self.inner.key_ring = ring;
        self
    }

    /// Server defaults from the environment: the key ring
    /// (`HFS_SECRET_KEYS[_FILE]`; a broken one is logged and treated as
    /// absent, so secrets are refused rather than stored), the outbound
    /// policy (`HFS_OUTBOUND_ALLOW`), the P2 auth probe under that policy and
    /// the authorizer: console sign-in roles (P3, from the request's
    /// `Extension<Arc<ConsoleAuth>>`) in front of the admin-credential authorizer
    /// (`HFS_ADMIN_TOKEN_SCOPE`, `HFS_CONSOLE_ALLOW_ANONYMOUS_ADMIN`) over
    /// `gate`, the server's administrator sign-in.
    pub fn env_defaults(mut self, gate: Option<helios_admin_auth::AdminGate>) -> Self {
        self.inner.key_ring = match SecretKeyRing::from_env() {
            Ok(ring) => ring.map(Arc::new),
            Err(e) => {
                tracing::error!(error = %e, "HFS_SECRET_KEYS is invalid; storing endpoint secrets is refused until it is fixed");
                None
            }
        };
        self.inner.outbound = OutboundPolicy::from_env();
        self.inner.authorizer = Arc::new(authz::ConsoleAdminAuthorizer::new(
            None,
            AdminCredentialAuthorizer::from_env(gate),
        ));
        self.inner.auth_probe =
            Arc::new(hooks::P2Probe::new(Arc::new(self.inner.outbound.clone())));
        self
    }

    /// The outbound (SSRF) policy (`HFS_OUTBOUND_ALLOW`).
    pub fn outbound(mut self, policy: OutboundPolicy) -> Self {
        self.inner.outbound = policy;
        self
    }

    /// DNS resolver for the SSRF check (tests inject a fake).
    pub fn resolver(mut self, resolver: Arc<dyn HostResolver>) -> Self {
        self.inner.resolver = resolver;
        self
    }

    /// Who may call the API (P3 plugs its console-role authorizer in here).
    pub fn authorizer(mut self, authorizer: Arc<dyn AdminAuthorizer>) -> Self {
        self.inner.authorizer = authorizer;
        self
    }

    /// The package manager, for the endpoint-scoped package routes, and the
    /// `.tgz` upload limit (`HFS_PACKAGE_MAX_SIZE`).
    pub fn packages(mut self, manager: Arc<PackageManager>, upload_limit: usize) -> Self {
        self.inner.packages = Some(manager);
        self.inner.package_upload_limit = upload_limit;
        self
    }

    /// The server's storage: stats, tenant purge and audit search run on it.
    pub fn storage<S>(mut self, storage: Arc<S>, observers: Option<Arc<WriteObservers>>) -> Self
    where
        S: ResourceStorage + SearchProvider + Send + Sync + 'static,
    {
        let dyn_storage: Arc<dyn ResourceStorage> = storage.clone();
        self.inner.tenant_ops = Some(Arc::new(StorageTenantOps::new(dyn_storage, observers)));
        self.inner.audit_search = Some(Arc::new(StorageAuditSearch::new(storage)));
        self
    }

    /// Overrides the tenant operations.
    pub fn tenant_ops(mut self, ops: Arc<dyn TenantOps>) -> Self {
        self.inner.tenant_ops = Some(ops);
        self
    }

    /// Overrides the audit search.
    pub fn audit_search(mut self, search: Arc<dyn AuditSearch>) -> Self {
        self.inner.audit_search = Some(search);
        self
    }

    /// Where admin AuditEvents go: `sink` (or the log), observer `source`,
    /// tenant `default_tenant`.
    pub fn audit(
        mut self,
        sink: Option<Arc<dyn AuditSink>>,
        source: impl Into<String>,
        default_tenant: impl Into<String>,
    ) -> Self {
        self.inner.audit = AdminAudit::new(sink, source, default_tenant);
        self
    }

    /// The duplicate hand-off (P6).
    pub fn duplicate(mut self, service: Arc<dyn DuplicateService>) -> Self {
        self.inner.duplicate = service;
        self
    }

    /// The auth probe (P2).
    pub fn auth_probe(mut self, probe: Arc<dyn EndpointAuthProbe>) -> Self {
        self.inner.auth_probe = probe;
        self
    }

    /// Adds an owner of secrets for `$rewrap`.
    pub fn rewrap_participant(mut self, participant: Arc<dyn RewrapParticipant>) -> Self {
        self.inner.rewrap_participants.push(participant);
        self
    }

    /// Endpoint cap (default [`DEFAULT_MAX_ENDPOINTS`]).
    pub fn max_endpoints(mut self, max: usize) -> Self {
        self.inner.max_endpoints = max.max(1);
        self
    }

    /// Public FHIR base (`HFS_BASE_URL`), for `publicBaseUrl` in responses.
    pub fn server_base(mut self, base: impl Into<String>) -> Self {
        self.inner.server_base = base.into();
        self
    }

    /// The state.
    pub fn build(self) -> AdminEndpointsState {
        AdminEndpointsState {
            inner: Arc::new(self.inner),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn h(v: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert(header::IF_MATCH, HeaderValue::from_str(v).unwrap());
        m
    }

    #[test]
    fn if_match_forms() {
        assert_eq!(if_match(&HeaderMap::new()), IfMatch::Absent);
        assert_eq!(if_match(&h("*")), IfMatch::Any);
        assert_eq!(if_match(&h("\"3\"")), IfMatch::Revision(3));
        assert_eq!(if_match(&h("W/\"4\"")), IfMatch::Revision(4));
        assert_eq!(if_match(&h("5")), IfMatch::Revision(5));
        assert!(matches!(if_match(&h("\"abc\"")), IfMatch::Other(_)));
    }

    #[test]
    fn expected_revision_rules() {
        assert_eq!(expected_revision(&HeaderMap::new(), 2, false).unwrap(), 2);
        assert_eq!(
            expected_revision(&HeaderMap::new(), 2, true)
                .unwrap_err()
                .status,
            axum::http::StatusCode::PRECONDITION_REQUIRED
        );
        assert_eq!(expected_revision(&h("*"), 2, true).unwrap(), 2);
        assert_eq!(expected_revision(&h("\"2\""), 2, true).unwrap(), 2);
        let e = expected_revision(&h("\"1\""), 2, true).unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::PRECONDITION_FAILED);
        assert_eq!(e.current_revision, Some(2));
        assert_eq!(
            expected_revision(&h("xyz"), 2, false).unwrap_err().status,
            axum::http::StatusCode::PRECONDITION_FAILED
        );
        assert_eq!(etag(9), "\"9\"");
    }

    #[tokio::test]
    async fn builder_defaults() {
        let store = Arc::new(helios_endpoints::MemoryEndpointStore::new());
        let registry = Arc::new(EndpointRegistry::load(store.as_ref()).await.unwrap());
        let s = AdminEndpointsState::builder(store, registry)
            .env_defaults(None)
            .max_endpoints(0)
            .rewrap_participant(Arc::new(NoSecrets))
            .build();
        assert_eq!(s.inner().max_endpoints, 1);
        assert_eq!(s.package_upload_limit(), 256 * 1024 * 1024);
        assert_eq!(s.inner().rewrap_participants.len(), 1);
        let ring = SecretKeyRing::parse(&SecretKeyRing::generate_spec("k")).unwrap();
        let r = s.inner().rewrap_participants[0]
            .rewrap(&ring, &Actor::bootstrap())
            .await;
        assert_eq!(r.scope, "none");
    }

    struct NoSecrets;

    #[async_trait]
    impl RewrapParticipant for NoSecrets {
        async fn rewrap(&self, _ring: &SecretKeyRing, _actor: &Actor) -> RewrapReport {
            RewrapReport {
                scope: "none".into(),
                ..RewrapReport::default()
            }
        }
    }
}
