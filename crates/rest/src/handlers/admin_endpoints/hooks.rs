//! Extension points the endpoint admin API calls but other packages own.
//!
//! | Trait | Implemented by | Until then |
//! |---|---|---|
//! | [`DuplicateService`] | P6 (sync jobs, `StorageCopy`) | [`UnavailableDuplicate`] → `501` |
//! | [`EndpointAuthProbe`] | P2 (issuer discovery + JWKS through the outbound policy): [`P2Probe`] | [`UnavailableProbe`] → `501` |
//! | [`TenantOps`] | here, over the server's storage ([`StorageTenantOps`]) | — |
//! | [`AuditSearch`] | here, over the server's storage ([`StorageAuditSearch`]) | — |
//!
//! The API validates requests itself (names, uniqueness, SSRF) before it
//! hands them to a hook, so implementations receive well-formed input.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use helios_auth::OutboundGuard;
use helios_endpoints::model::EndpointAuth;
use helios_endpoints::sync::DuplicateRequest;
use helios_endpoints::{Actor, Endpoint, ManagedBy, OutboundPolicy};
use helios_persistence::core::{
    ErasedScope, ResourceStorage, SearchProvider, WriteEvent, WriteObserver, WriteObservers,
};
use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
use helios_persistence::types::{SearchQuery, SortDirection, SortDirective};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::error::ApiError;

// ------------------------------------------------------------------ duplicate

/// A duplicate that was started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateStarted {
    /// Job id (sync run / job of P6).
    pub job_id: String,
    /// Where the job's status is polled (relative to the server base), e.g.
    /// `admin/sync/runs/{id}`.
    pub status_url: String,
    /// The new endpoint (status `provisioning`), when already created.
    #[serde(default)]
    pub endpoint: Option<Value>,
}

/// Starts `POST /admin/endpoints/{name}/$duplicate` (design §9.3).
#[async_trait]
pub trait DuplicateService: Send + Sync {
    /// Creates the copy (status `provisioning`) and the job that fills it.
    /// `request` is already validated: the name/path are well-formed and free.
    async fn start(
        &self,
        source: &Endpoint,
        request: DuplicateRequest,
        actor: &Actor,
    ) -> Result<DuplicateStarted, ApiError>;
}

/// Placeholder until the sync package is installed.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableDuplicate;

#[async_trait]
impl DuplicateService for UnavailableDuplicate {
    async fn start(
        &self,
        _source: &Endpoint,
        _request: DuplicateRequest,
        _actor: &Actor,
    ) -> Result<DuplicateStarted, ApiError> {
        Err(ApiError::not_implemented(
            "duplicating endpoints needs the sync service, which is not installed in this server",
        ))
    }
}

// ------------------------------------------------------------------ auth probe

pub use helios_auth::{ProbeReport, ProbeStep};

/// What `$test-auth` answers: P2's [`ProbeReport`] (mode, issuer discovery
/// step, JWKS step, warnings) plus, when a token was sent, a `token` step
/// (signature, issuer, audience, expiry checked as the endpoint would).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthTestReport {
    /// The probe; its `ok` covers the token step too.
    #[serde(flatten)]
    pub report: ProbeReport,
    /// The token check, when a token was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<ProbeStep>,
}

/// Probes an endpoint's auth section (P2 implements it; [`P2Probe`]).
#[async_trait]
pub trait EndpointAuthProbe: Send + Sync {
    /// Probes `auth` of an endpoint managed by `managed_by` (env-managed
    /// endpoints are operator-trusted; console-edited ones go through the
    /// outbound policy), validating `token` (a bare JWT) when given.
    async fn test(
        &self,
        auth: &EndpointAuth,
        managed_by: ManagedBy,
        token: Option<&str>,
    ) -> Result<AuthTestReport, ApiError>;
}

/// Placeholder: `501`.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableProbe;

#[async_trait]
impl EndpointAuthProbe for UnavailableProbe {
    async fn test(
        &self,
        _auth: &EndpointAuth,
        _managed_by: ManagedBy,
        _token: Option<&str>,
    ) -> Result<AuthTestReport, ApiError> {
        Err(ApiError::not_implemented(
            "the auth probe is not installed in this server",
        ))
    }
}

/// The P2 probe (`helios_auth::EndpointAuthProbe`) behind [`EndpointAuthProbe`]:
/// issuer discovery and JWKS fetched through the outbound policy
/// (`HFS_OUTBOUND_ALLOW`) for console-edited endpoints, trusted for
/// env-managed ones; a token is verified with the verifier P2 builds for the
/// endpoint (`AuthConfig::from_endpoint`).
#[derive(Debug, Clone)]
pub struct P2Probe {
    policy: OutboundGuard,
}

impl P2Probe {
    /// Console-edited endpoints are fetched under `policy`.
    pub fn new(policy: Arc<OutboundPolicy>) -> Self {
        Self {
            policy: OutboundGuard::Policy(policy),
        }
    }

    fn guard(&self, managed_by: ManagedBy) -> OutboundGuard {
        match managed_by {
            ManagedBy::Env => OutboundGuard::Trusted,
            ManagedBy::Ui => self.policy.clone(),
        }
    }

    async fn token_step(&self, auth: &EndpointAuth, guard: &OutboundGuard, jwt: &str) -> ProbeStep {
        let started = std::time::Instant::now();
        let step = |ok: bool, detail: String| ProbeStep {
            ok,
            url: None,
            detail,
            key_count: None,
            elapsed_ms: started.elapsed().as_millis() as u64,
        };
        if auth.mode == helios_endpoints::model::AuthMode::Off {
            return step(
                false,
                "auth is off for this endpoint: tokens are not checked".into(),
            );
        }
        let config =
            helios_auth::AuthConfig::from_endpoint(auth, &helios_auth::AuthConfig::default());
        let provider = helios_auth::VerifierCache::new().provider(&config, guard);
        let jwt = jwt.trim();
        let jwt = jwt.strip_prefix("Bearer ").unwrap_or(jwt);
        match provider.authenticate(&format!("Bearer {jwt}")).await {
            Ok(p) => step(
                true,
                format!(
                    "valid token: sub {}, issuer {}, {} scope(s)",
                    p.subject,
                    p.issuer,
                    p.scopes.scopes().len()
                ),
            ),
            Err(e) => step(false, format!("token refused: {e}")),
        }
    }
}

#[async_trait]
impl EndpointAuthProbe for P2Probe {
    async fn test(
        &self,
        auth: &EndpointAuth,
        managed_by: ManagedBy,
        token: Option<&str>,
    ) -> Result<AuthTestReport, ApiError> {
        let guard = self.guard(managed_by);
        let mut report = helios_auth::EndpointAuthProbe::new(guard.clone())
            .test(auth)
            .await;
        let token = match token {
            Some(jwt) => Some(self.token_step(auth, &guard, jwt).await),
            None => None,
        };
        if token.as_ref().is_some_and(|t| !t.ok) {
            report.ok = false;
        }
        Ok(AuthTestReport { report, token })
    }
}

// ------------------------------------------------------------------ tenant ops

/// What an endpoint's tenant holds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantStats {
    /// Current resources by type.
    pub resources: BTreeMap<String, u64>,
    /// Sum of `resources`.
    pub total: u64,
    /// Newest write, when the backend tracks it.
    pub last_write: Option<DateTime<Utc>>,
}

/// Storage operations on an endpoint's tenant.
#[async_trait]
pub trait TenantOps: Send + Sync {
    /// Counts and last write.
    async fn stats(&self, tenant: &str) -> Result<TenantStats, ApiError>;
    /// Permanently removes the tenant's data (resources, history, search
    /// index) and its registration. Returns current resources removed.
    async fn purge(&self, tenant: &str) -> Result<u64, ApiError>;
}

fn ctx(tenant: &str) -> TenantContext {
    TenantContext::new(TenantId::new(tenant), TenantPermissions::full_access())
}

fn storage_error(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "transient",
        format!("storage error: {e}"),
    )
}

/// [`TenantOps`] over the server's storage, reusing the tenant purge of
/// `DELETE /admin/tenants/{id}?purge=true`.
#[derive(Clone)]
pub struct StorageTenantOps {
    storage: Arc<dyn ResourceStorage>,
    observers: Option<Arc<WriteObservers>>,
}

impl StorageTenantOps {
    /// Over `storage`; `observers` hear about purged tenants (dashboard counters).
    pub fn new(storage: Arc<dyn ResourceStorage>, observers: Option<Arc<WriteObservers>>) -> Self {
        Self { storage, observers }
    }
}

#[async_trait]
impl TenantOps for StorageTenantOps {
    async fn stats(&self, tenant: &str) -> Result<TenantStats, ApiError> {
        let c = ctx(tenant);
        let counts = self
            .storage
            .count_all_types(&c)
            .await
            .map_err(storage_error)?;
        let resources: BTreeMap<String, u64> = counts.into_iter().filter(|(_, n)| *n > 0).collect();
        let total = resources.values().sum();
        let last_write = self
            .storage
            .latest_write_marker(&c, None)
            .await
            .ok()
            .flatten()
            .and_then(|m| m.latest);
        Ok(TenantStats {
            resources,
            total,
            last_write,
        })
    }

    async fn purge(&self, tenant: &str) -> Result<u64, ApiError> {
        let removed = self
            .storage
            .purge_tenant_data(tenant)
            .await
            .map_err(storage_error)?;
        // Not every backend has a tenant registry; a missing one is fine.
        let _ = self.storage.deregister_tenant(tenant).await;
        if let Some(obs) = &self.observers {
            obs.on_write(&WriteEvent::Erased {
                tenant: TenantId::new(tenant),
                scope: ErasedScope::Tenant,
            });
            obs.on_write(&WriteEvent::TenantRemoved {
                tenant: TenantId::new(tenant),
            });
        }
        Ok(removed)
    }
}

// ------------------------------------------------------------------ audit search

/// One page of AuditEvents.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditPage {
    /// The events, newest first.
    pub events: Vec<Value>,
    /// Cursor of the next page.
    pub next: Option<String>,
}

/// AuditEvent search in an endpoint's tenant.
#[async_trait]
pub trait AuditSearch: Send + Sync {
    /// Newest first, `count` per page, continuing at `cursor`.
    async fn search(
        &self,
        tenant: &str,
        count: u32,
        cursor: Option<String>,
    ) -> Result<AuditPage, ApiError>;
}

/// [`AuditSearch`] over a search-capable storage.
pub struct StorageAuditSearch<S> {
    storage: Arc<S>,
}

impl<S> StorageAuditSearch<S> {
    /// Over `storage`.
    pub fn new(storage: Arc<S>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl<S> AuditSearch for StorageAuditSearch<S>
where
    S: SearchProvider + Send + Sync + 'static,
{
    async fn search(
        &self,
        tenant: &str,
        count: u32,
        cursor: Option<String>,
    ) -> Result<AuditPage, ApiError> {
        let mut query = SearchQuery::new("AuditEvent")
            .with_sort(SortDirective {
                parameter: "_lastUpdated".into(),
                direction: SortDirection::Descending,
                param_type: None,
            })
            .with_count(count);
        if let Some(c) = cursor {
            query = query.with_cursor(c);
        }
        let result = self
            .storage
            .search(&ctx(tenant), &query)
            .await
            .map_err(|e| match e {
                helios_persistence::error::StorageError::Validation(_) => {
                    ApiError::bad_request(format!("bad cursor: {e}"))
                }
                other => storage_error(other),
            })?;
        let next = result.resources.page_info.next_cursor.clone();
        Ok(AuditPage {
            events: result
                .resources
                .items
                .into_iter()
                .map(|r| r.into_content_with_meta())
                .collect(),
            next,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_endpoints::{EndpointName, EndpointSpec};

    fn endpoint() -> Endpoint {
        Endpoint {
            id: "id".into(),
            revision: 1,
            spec: EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            created_by: None,
            updated_by: None,
        }
    }

    #[tokio::test]
    async fn placeholders_answer_not_implemented() {
        let req: DuplicateRequest =
            serde_json::from_value(serde_json::json!({"name": "copy"})).unwrap();
        let e = UnavailableDuplicate
            .start(&endpoint(), req, &Actor::bootstrap())
            .await
            .unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::NOT_IMPLEMENTED);
        let e = UnavailableProbe
            .test(&EndpointAuth::default(), ManagedBy::Ui, None)
            .await
            .unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::NOT_IMPLEMENTED);
    }
}
