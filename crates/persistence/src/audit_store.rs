//! Audit-event stores over this crate's backends, shared by `hfs` and `hts`.
//!
//! [`TxAuditStore`] writes a batch of `AuditEvent`s in one transaction on the
//! transactional backends (SQLite, PostgreSQL); [`ResourceAuditStore`] writes
//! per event through any [`ResourceStorage`]. Both implement
//! [`helios_audit::sinks::database::AuditStorage`], so a
//! [`helios_audit::DatabaseSink`] can sit on top. [`retention`] is the
//! age-based purge job over [`crate::core::RetentionStorage`].

use std::sync::Arc;

use crate::core::ResourceStorage;
use crate::tenant::TenantContext;

pub mod retention;

/// The tenant an audit event is stored in: the configured audit tenant, else
/// the tenant of the audited request, else the default tenant — so that the
/// events are searchable through the REST API of that tenant.
pub fn audit_tenant(fixed: Option<&str>, request: Option<&str>, default: &str) -> TenantContext {
    TenantContext::new(
        crate::tenant::TenantId::new(fixed.or(request).unwrap_or(default)),
        crate::tenant::TenantPermissions::full_access(),
    )
}

/// How a transactional backend opens the transaction for a batch of audit
/// events (creates under ids this server just generated).
#[async_trait::async_trait]
pub trait AuditBatchTransaction: crate::core::TransactionProvider {
    /// Opens the transaction for a batch creating `ids`.
    async fn begin_audit_batch(
        &self,
        tenant: &TenantContext,
        options: crate::core::TransactionOptions,
        ids: Vec<(String, String)>,
    ) -> crate::error::StorageResult<Self::Transaction>;
}

#[cfg(feature = "postgres")]
#[async_trait::async_trait]
impl AuditBatchTransaction for crate::backends::postgres::PostgresBackend {
    async fn begin_audit_batch(
        &self,
        tenant: &TenantContext,
        options: crate::core::TransactionOptions,
        ids: Vec<(String, String)>,
    ) -> crate::error::StorageResult<Self::Transaction> {
        // Shared tenant lock only: a plain transaction would hold the tenant's
        // exclusive write gate and stall every clinical write meanwhile.
        self.begin_fresh_create_transaction(tenant, options, ids)
            .await
    }
}

#[cfg(feature = "sqlite")]
#[async_trait::async_trait]
impl AuditBatchTransaction for crate::backends::sqlite::SqliteBackend {
    async fn begin_audit_batch(
        &self,
        tenant: &TenantContext,
        options: crate::core::TransactionOptions,
        _ids: Vec<(String, String)>,
    ) -> crate::error::StorageResult<Self::Transaction> {
        // SQLite has one writer at a time either way.
        crate::core::TransactionProvider::begin_transaction(self, tenant, options).await
    }
}

/// Audit store on a transactional backend (SQLite, PostgreSQL): a batch of
/// events is written in **one** transaction, so the commit (and its WAL
/// flush) is paid once per batch rather than once per event.
pub struct TxAuditStore<B> {
    backend: Arc<B>,
    fixed_tenant: Option<String>,
    default_tenant: String,
}

impl<B> TxAuditStore<B> {
    /// `fixed_tenant` (`*_AUDIT_TENANT`) receives every event when set;
    /// otherwise events go to their request's tenant, else `default_tenant`.
    pub fn new(backend: Arc<B>, fixed_tenant: Option<String>, default_tenant: String) -> Self {
        Self {
            backend,
            fixed_tenant,
            default_tenant,
        }
    }

    fn tenant_for(&self, request_tenant: Option<&str>) -> TenantContext {
        audit_tenant(
            self.fixed_tenant.as_deref(),
            request_tenant,
            &self.default_tenant,
        )
    }
}

#[async_trait::async_trait]
impl<B> helios_audit::sinks::database::AuditStorage for TxAuditStore<B>
where
    B: AuditBatchTransaction + Send + Sync + 'static,
{
    async fn create_resource(
        &self,
        tenant: Option<&str>,
        resource_type: &str,
        resource: serde_json::Value,
        fhir_version: helios_fhir::FhirVersion,
    ) -> Result<(), String> {
        self.backend
            .create(
                &self.tenant_for(tenant),
                resource_type,
                resource,
                fhir_version,
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn create_resources(
        &self,
        tenant: Option<&str>,
        resource_type: &str,
        resources: Vec<serde_json::Value>,
        fhir_version: helios_fhir::FhirVersion,
    ) -> Vec<Result<(), String>> {
        use crate::core::{Transaction, TransactionOptions};
        let n = resources.len();
        let fail = |e: String| (0..n).map(|_| Err(e.clone())).collect::<Vec<_>>();
        let mut options = TransactionOptions::new();
        options.fhir_version = Some(fhir_version);
        let ids = resources
            .iter()
            .filter_map(|r| r.get("id").and_then(|v| v.as_str()))
            .map(|id| (resource_type.to_string(), id.to_string()))
            .collect();
        let mut tx = match self
            .backend
            .begin_audit_batch(&self.tenant_for(tenant), options, ids)
            .await
        {
            Ok(tx) => tx,
            Err(e) => return fail(e.to_string()),
        };
        for r in resources {
            if let Err(e) = tx.create(resource_type, r).await {
                let _ = Box::new(tx).rollback().await;
                // Every event of the batch is retried on its own.
                return fail(e.to_string());
            }
        }
        match Box::new(tx).commit().await {
            Ok(()) => (0..n).map(|_| Ok(())).collect(),
            Err(e) => fail(e.to_string()),
        }
    }
}

/// Audit store writing per event through any [`ResourceStorage`].
pub struct ResourceAuditStore {
    storage: Arc<dyn ResourceStorage>,
    /// `HFS_AUDIT_TENANT`: every event goes to this tenant.
    fixed_tenant: Option<String>,
    /// Tenant of events outside any request (the server's default tenant).
    default_tenant: String,
}

impl ResourceAuditStore {
    /// Per-event store over any backend (MongoDB, S3, ...).
    pub fn new(
        storage: Arc<dyn ResourceStorage>,
        fixed_tenant: Option<String>,
        default_tenant: String,
    ) -> Self {
        Self {
            storage,
            fixed_tenant,
            default_tenant,
        }
    }

    /// The tenant an event is stored in: the configured audit tenant, else
    /// the tenant of the audited request, else the default tenant — so that
    /// the events are searchable through the REST API of that tenant.
    fn tenant_for(&self, request_tenant: Option<&str>) -> TenantContext {
        audit_tenant(
            self.fixed_tenant.as_deref(),
            request_tenant,
            &self.default_tenant,
        )
    }
}

#[async_trait::async_trait]
impl helios_audit::sinks::database::AuditStorage for ResourceAuditStore {
    async fn create_resource(
        &self,
        tenant: Option<&str>,
        resource_type: &str,
        resource: serde_json::Value,
        fhir_version: helios_fhir::FhirVersion,
    ) -> Result<(), String> {
        self.storage
            .create(
                &self.tenant_for(tenant),
                resource_type,
                resource,
                fhir_version,
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn create_resources(
        &self,
        tenant: Option<&str>,
        resource_type: &str,
        resources: Vec<serde_json::Value>,
        fhir_version: helios_fhir::FhirVersion,
    ) -> Vec<Result<(), String>> {
        // `create_many` writes with the backend's bulk concurrency (or its
        // native batch write) instead of one round trip at a time.
        self.storage
            .create_many(
                &self.tenant_for(tenant),
                resource_type,
                resources,
                fhir_version,
            )
            .await
            .into_iter()
            .map(|r| r.map(|_| ()).map_err(|e| e.to_string()))
            .collect()
    }
}

/// Searching the stored audit trail, for servers that have no general FHIR
/// search API of their own (HTS). Supports the AuditEvent parameters an
/// auditor needs: `date` (with prefixes, repeatable), `patient`, `entity`,
/// `agent` (references), `type`, `subtype`, `action`, `outcome` (tokens),
/// `_count` (default 100, max 1000) and `_sort` (default `-date`).
#[async_trait::async_trait]
pub trait AuditEventSearch: Send + Sync {
    /// Matching events, newest first by default. `Err` carries a message for
    /// a `400` (unknown parameter) or the storage failure.
    async fn search_audit_events(
        &self,
        params: &[(String, String)],
    ) -> Result<Vec<serde_json::Value>, AuditSearchError>;

    /// One event by id.
    async fn read_audit_event(
        &self,
        id: &str,
    ) -> Result<Option<serde_json::Value>, AuditSearchError>;
}

/// Why an audit search failed.
#[derive(Debug)]
pub enum AuditSearchError {
    /// The request named something this search does not support.
    BadRequest(String),
    /// The store failed.
    Storage(String),
}

impl std::fmt::Display for AuditSearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(m) | Self::Storage(m) => f.write_str(m),
        }
    }
}

/// Builds the [`crate::types::SearchQuery`] for [`AuditEventSearch`].
pub fn audit_search_query(
    params: &[(String, String)],
) -> Result<crate::types::SearchQuery, AuditSearchError> {
    use crate::types::{SearchParamType, SearchParameter, SearchQuery, SearchValue, SortDirective};
    let mut query = SearchQuery::new("AuditEvent");
    let mut count = 100u32;
    let mut sort = "-date".to_string();
    for (name, value) in params {
        let param_type = match name.as_str() {
            "date" => SearchParamType::Date,
            "patient" | "entity" | "agent" => SearchParamType::Reference,
            "type" | "subtype" | "action" | "outcome" => SearchParamType::Token,
            "_count" => {
                count = value
                    .parse::<u32>()
                    .map_err(|_| AuditSearchError::BadRequest(format!("invalid _count {value:?}")))?
                    .clamp(1, 1000);
                continue;
            }
            "_sort" => {
                sort = value.clone();
                continue;
            }
            "_format" => continue,
            other => {
                return Err(AuditSearchError::BadRequest(format!(
                    "unsupported AuditEvent search parameter {other:?} (supported: date, patient, \
                     entity, agent, type, subtype, action, outcome, _count, _sort)"
                )));
            }
        };
        query = query.with_parameter(SearchParameter {
            name: name.clone(),
            param_type,
            values: value
                .split(',')
                .filter(|v| !v.is_empty())
                .map(|v| SearchValue::parse_for_type(v, param_type))
                .collect(),
            ..Default::default()
        });
    }
    let directive = SortDirective::parse(&sort);
    let param_type = (directive.parameter == "date").then_some(SearchParamType::Date);
    Ok(query
        .with_sort(directive.with_param_type(param_type))
        .with_count(count))
}

#[async_trait::async_trait]
impl<B> AuditEventSearch for TxAuditStore<B>
where
    B: crate::core::SearchProvider + Send + Sync + 'static,
{
    async fn search_audit_events(
        &self,
        params: &[(String, String)],
    ) -> Result<Vec<serde_json::Value>, AuditSearchError> {
        let query = audit_search_query(params)?;
        let result = self
            .backend
            .search(&self.tenant_for(None), &query)
            .await
            .map_err(|e| AuditSearchError::Storage(e.to_string()))?;
        Ok(result
            .resources
            .items
            .into_iter()
            .map(|r| r.content_with_meta())
            .collect())
    }

    async fn read_audit_event(
        &self,
        id: &str,
    ) -> Result<Option<serde_json::Value>, AuditSearchError> {
        self.backend
            .read(&self.tenant_for(None), "AuditEvent", id)
            .await
            .map(|r| r.map(|r| r.content_with_meta()))
            .map_err(|e| AuditSearchError::Storage(e.to_string()))
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::backends::sqlite::SqliteBackend;
    use helios_audit::sinks::database::AuditStorage;
    use helios_fhir::FhirVersion;

    fn event(
        id: &str,
        subtype: &str,
        outcome: &str,
        entity: &str,
        recorded: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "resourceType": "AuditEvent", "id": id,
            "type": {"system": "http://terminology.hl7.org/CodeSystem/audit-event-type", "code": "rest"},
            "subtype": [{"system": "http://hl7.org/fhir/restful-interaction", "code": subtype}],
            "action": "C", "recorded": recorded, "outcome": outcome,
            "agent": [{"requestor": true, "who": {"display": "admin"}}],
            "source": {"observer": {"reference": "Device/hts"}},
            "entity": [{"what": {"reference": entity}}]
        })
    }

    fn store() -> TxAuditStore<SqliteBackend> {
        let data_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("data"))
            .unwrap();
        let backend = SqliteBackend::with_config(
            ":memory:",
            crate::backends::sqlite::SqliteBackendConfig {
                data_dir: Some(data_dir),
                ..Default::default()
            },
        )
        .unwrap();
        backend.init_schema().unwrap();
        TxAuditStore::new(Arc::new(backend), None, "default".into())
    }

    #[tokio::test]
    async fn batches_are_stored_and_searchable() {
        let store = store();
        let results = store
            .create_resources(
                None,
                "AuditEvent",
                vec![
                    event(
                        "a1",
                        "create",
                        "0",
                        "CodeSystem/cs1",
                        "2026-11-01T10:00:00Z",
                    ),
                    event(
                        "a2",
                        "delete",
                        "0",
                        "CodeSystem/cs1",
                        "2026-11-02T10:00:00Z",
                    ),
                    event("a3", "update", "4", "ValueSet/vs1", "2026-11-03T10:00:00Z"),
                ],
                FhirVersion::default(),
            )
            .await;
        assert!(results.iter().all(Result::is_ok), "{results:?}");

        let ids = |v: Vec<serde_json::Value>| {
            v.into_iter()
                .map(|e| e["id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        let p = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect::<Vec<_>>()
        };
        let all = store.search_audit_events(&[]).await.unwrap();
        assert_eq!(ids(all), vec!["a3", "a2", "a1"], "newest first");
        let cs = store
            .search_audit_events(&p(&[("entity", "CodeSystem/cs1")]))
            .await
            .unwrap();
        assert_eq!(cs.len(), 2);
        let failed = store
            .search_audit_events(&p(&[("outcome", "4")]))
            .await
            .unwrap();
        assert_eq!(ids(failed), vec!["a3"]);
        let deletes = store
            .search_audit_events(&p(&[("subtype", "delete")]))
            .await
            .unwrap();
        assert_eq!(ids(deletes), vec!["a2"]);
        let recent = store
            .search_audit_events(&p(&[("date", "ge2026-11-02")]))
            .await
            .unwrap();
        assert_eq!(recent.len(), 2);
        let one = store
            .search_audit_events(&p(&[("_count", "1")]))
            .await
            .unwrap();
        assert_eq!(one.len(), 1);
        assert!(store.read_audit_event("a2").await.unwrap().is_some());
        assert!(matches!(
            store.search_audit_events(&p(&[("name", "x")])).await,
            Err(AuditSearchError::BadRequest(_))
        ));
    }
}
