//! [`TableAuditStore`]: the audit store over the dedicated PostgreSQL audit
//! tables (`HFS_AUDIT_STORE=table`).
//!
//! Writes go to `audit_event` / `audit_event_ref`
//! ([`crate::backends::postgres::audit_table`]), a whole batch per statement.
//! Reads for servers without a general FHIR search API (HTS) merge the table
//! with the `AuditEvent` resources written before the table existed, so the
//! trail stays complete during the transition.

use std::sync::Arc;

use helios_audit::store::{AuditEventReader, AuditReadError, parse_search};
use serde_json::Value;

use super::{AuditEventSearch, AuditSearchError, TxAuditStore};
use crate::backends::postgres::PostgresBackend;
use crate::backends::postgres::audit_table::PgAuditTable;

/// Audit store writing to the dedicated audit tables.
pub struct TableAuditStore {
    table: PgAuditTable,
    /// The pre-table store, for the tenant resolution it shares and for
    /// reading the events it wrote.
    legacy: TxAuditStore<PostgresBackend>,
}

impl TableAuditStore {
    /// `fixed_tenant` (`*_AUDIT_TENANT`) receives every event when set;
    /// otherwise events go to their request's tenant, else `default_tenant`.
    pub fn new(
        backend: Arc<PostgresBackend>,
        fixed_tenant: Option<String>,
        default_tenant: String,
    ) -> Self {
        Self {
            table: PgAuditTable::new(Arc::clone(&backend)),
            legacy: TxAuditStore::new(backend, fixed_tenant, default_tenant),
        }
    }

    /// The tables.
    pub fn table(&self) -> &PgAuditTable {
        &self.table
    }

    fn tenant_id(&self, request_tenant: Option<&str>) -> String {
        self.legacy
            .tenant_for(request_tenant)
            .tenant_id()
            .as_str()
            .to_string()
    }
}

#[async_trait::async_trait]
impl helios_audit::sinks::database::AuditStorage for TableAuditStore {
    async fn create_resource(
        &self,
        tenant: Option<&str>,
        _resource_type: &str,
        resource: Value,
        _fhir_version: helios_fhir::FhirVersion,
    ) -> Result<(), String> {
        self.table
            .append(vec![(self.tenant_id(tenant), resource)])
            .await
    }

    async fn append_events(
        &self,
        events: Vec<(Option<String>, Value)>,
    ) -> Option<Result<(), String>> {
        let rows = events
            .into_iter()
            .map(|(tenant, event)| (self.tenant_id(tenant.as_deref()), event))
            .collect();
        Some(self.table.append(rows).await)
    }

    fn appends(&self) -> bool {
        true
    }
}

fn search_error(e: AuditReadError) -> AuditSearchError {
    match e {
        AuditReadError::BadRequest(m) => AuditSearchError::BadRequest(m),
        AuditReadError::Storage(m) => AuditSearchError::Storage(m),
    }
}

fn recorded_of(event: &Value) -> &str {
    event.get("recorded").and_then(Value::as_str).unwrap_or("")
}

#[async_trait::async_trait]
impl AuditEventSearch for TableAuditStore {
    async fn search_audit_events(
        &self,
        params: &[(String, String)],
    ) -> Result<Vec<Value>, AuditSearchError> {
        // The legacy builder validates the parameter names and `_count`.
        let legacy_query = super::audit_search_query(params)?;
        let count = legacy_query.count.unwrap_or(100) as usize;
        let ascending = legacy_query
            .sort
            .first()
            .is_some_and(|s| s.direction == crate::types::SortDirection::Ascending);
        let criteria: Vec<(String, String)> = params
            .iter()
            .filter(|(name, _)| !matches!(name.as_str(), "_count" | "_sort" | "_format"))
            .cloned()
            .collect();
        let mut query = parse_search(&criteria).map_err(search_error)?.query;
        query.limit = count;
        query.ascending = ascending;
        let tenant = self.tenant_id(None);
        let mut events: Vec<Value> = self
            .table
            .search(&tenant, &query)
            .await
            .map_err(search_error)?
            .into_iter()
            .map(|e| e.event)
            .collect();
        let has_legacy = self
            .table
            .has_legacy_events(&tenant)
            .await
            .map_err(AuditSearchError::Storage)?;
        if has_legacy {
            events.extend(self.legacy.search_audit_events(params).await?);
            // Stable: on equal instants the table's events stay first.
            events.sort_by(|a, b| {
                let order = recorded_instant(a).cmp(&recorded_instant(b));
                if ascending { order } else { order.reverse() }
            });
            events.truncate(count);
        }
        Ok(events)
    }

    async fn read_audit_event(&self, id: &str) -> Result<Option<Value>, AuditSearchError> {
        let tenant = self.tenant_id(None);
        match self.table.read(&tenant, id).await.map_err(search_error)? {
            Some(e) => Ok(Some(e.event)),
            None => self.legacy.read_audit_event(id).await,
        }
    }
}

fn recorded_instant(event: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(recorded_of(event))
        .ok()
        .map(|d| d.with_timezone(&chrono::Utc))
}
