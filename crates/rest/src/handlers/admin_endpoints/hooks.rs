//! Extension points the endpoint admin API calls but other packages own.
//!
//! | Trait | Implemented by | Until then |
//! |---|---|---|
//! | [`DuplicateService`] | P6 (sync jobs, `StorageCopy`) | [`UnavailableDuplicate`] → `501` |
//! | [`EndpointAuthProbe`] | P2 (issuer discovery + JWKS through the outbound policy) | [`UnavailableProbe`] → `501` |
//! | [`TenantOps`] | here, over the server's storage ([`StorageTenantOps`]) | — |
//! | [`AuditSearch`] | here, over the server's storage ([`StorageAuditSearch`]) | — |
//!
//! The API validates requests itself (names, uniqueness, SSRF) before it
//! hands them to a hook, so implementations receive well-formed input.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use helios_endpoints::model::EndpointAuth;
use helios_endpoints::sync::DuplicateRequest;
use helios_endpoints::{Actor, Endpoint};
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

/// One check of an auth probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeCheck {
    /// What was checked (`issuer-discovery`, `jwks`, `algorithms`…).
    pub name: String,
    /// Whether it passed.
    pub ok: bool,
    /// Details or the error.
    #[serde(default)]
    pub detail: Option<String>,
}

/// Result of `POST /admin/endpoints/{name}/$test-auth`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeReport {
    /// All checks passed.
    pub ok: bool,
    /// The checks.
    pub checks: Vec<ProbeCheck>,
}

/// Fetches an endpoint's issuer discovery and JWKS (through the outbound
/// policy) and reports what works.
#[async_trait]
pub trait EndpointAuthProbe: Send + Sync {
    /// Probes `auth`.
    async fn test(&self, auth: &EndpointAuth) -> ProbeReport;
}

/// Placeholder until the per-endpoint auth package is installed.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableProbe;

#[async_trait]
impl EndpointAuthProbe for UnavailableProbe {
    async fn test(&self, _auth: &EndpointAuth) -> ProbeReport {
        ProbeReport {
            ok: false,
            checks: vec![ProbeCheck {
                name: "probe".into(),
                ok: false,
                detail: Some("the auth probe is not installed in this server".into()),
            }],
        }
    }
}

impl UnavailableProbe {
    /// Whether `report` is the placeholder's (so the API can answer `501`).
    pub fn is_placeholder(report: &ProbeReport) -> bool {
        report.checks.len() == 1
            && report.checks[0].name == "probe"
            && report.checks[0]
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("not installed"))
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
        let r = UnavailableProbe.test(&EndpointAuth::default()).await;
        assert!(!r.ok);
        assert!(UnavailableProbe::is_placeholder(&r));
        assert!(!UnavailableProbe::is_placeholder(&ProbeReport {
            ok: true,
            checks: vec![]
        }));
    }
}
