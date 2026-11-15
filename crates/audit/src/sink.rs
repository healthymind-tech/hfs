//! Core audit sink trait.
//!
//! All audit backends implement [`AuditSink`]. The trait is intentionally
//! infallible — failures are logged via `tracing` and never propagated to
//! callers, preserving the fire-and-forget contract.

use crate::fhir_model::AuditEvent;
use async_trait::async_trait;

/// An audit event together with the tenant it belongs to.
#[derive(Debug, Clone)]
pub struct AuditRecord {
    /// Tenant of the audited request, if any.
    pub tenant: Option<String>,
    /// The event.
    pub event: AuditEvent,
}

/// Pluggable backend for recording FHIR `AuditEvent` resources.
///
/// Implementations must be `Send + Sync + 'static` so they can be shared
/// across Axum handlers via `Arc`.
#[async_trait]
pub trait AuditSink: Send + Sync + 'static {
    /// Record a single audit event.
    ///
    /// This method **must not** return errors to the caller. Failures are
    /// logged internally via `tracing::error!`.
    async fn record(&self, event: AuditEvent);

    /// Record a batch of audit events.
    ///
    /// The default implementation calls [`record`](Self::record) for each
    /// event sequentially. Backends that support batch writes can override
    /// this for better throughput.
    async fn record_batch(&self, events: Vec<AuditEvent>) {
        for event in events {
            self.record(event).await;
        }
    }

    /// Record an event that belongs to a tenant.
    ///
    /// `tenant` is the tenant the audited request was resolved to; `None` for
    /// events outside any tenant (server lifecycle, a request rejected before
    /// its tenant was known). Sinks that do not partition by tenant ignore it.
    async fn record_in(&self, tenant: Option<&str>, event: AuditEvent) {
        let _ = tenant;
        self.record(event).await;
    }

    /// Record a batch of tenant-attributed events.
    ///
    /// The default implementation calls [`record_in`](Self::record_in) for
    /// each record sequentially.
    async fn record_records(&self, records: Vec<AuditRecord>) {
        for r in records {
            self.record_in(r.tenant.as_deref(), r.event).await;
        }
    }

    /// Flush any buffered events to the underlying store.
    async fn flush(&self);

    /// Human-readable name for this sink (e.g. `"null"`, `"file"`, `"database"`).
    fn name(&self) -> &str;
}
