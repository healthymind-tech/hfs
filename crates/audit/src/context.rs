//! Request-scoped audit context.
//!
//! The audit middleware wraps every audited request in a task-local
//! [`RequestAudit`]. Code that runs *inside* the request — the tenant
//! extractor, the authentication layer, the batch handler — reports what only
//! it knows (the resolved tenant, the authenticated user, the per-entry results
//! of a bundle) into that context, and the middleware turns it into
//! `AuditEvent`s once the response is known.
//!
//! Everything here is a no-op outside an audited request (audit disabled, an
//! excluded path, a unit test that drives a handler directly), so callers never
//! need to know whether audit is on.

use std::future::Future;
use std::sync::{Arc, Mutex};

use crate::rest_event::{AuditUser, RestEventSpec};

tokio::task_local! {
    static CURRENT: RequestAudit;
}

/// What the request reported about itself while it ran.
#[derive(Debug, Default, Clone)]
pub struct Captured {
    /// Tenant the request was resolved to.
    pub tenant: Option<String>,
    /// Whether that tenant came from a URL path prefix (which the middleware
    /// then strips before classifying the path).
    pub tenant_from_url: bool,
    /// The authenticated user, when authentication is enabled.
    pub user: Option<AuditUser>,
    /// `batch` or `transaction`, reported by the bundle handler.
    pub bundle_type: Option<String>,
    /// One audit description per processed bundle entry.
    pub entries: Vec<RestEventSpec>,
    /// Patients the handler found the request to involve, beyond those the
    /// request path and parameters name.
    pub patients: Vec<String>,
}

/// Handle on the audit context of the request being processed.
#[derive(Debug, Default, Clone)]
pub struct RequestAudit(Arc<Mutex<Captured>>);

impl RequestAudit {
    /// A fresh, empty context.
    pub fn new() -> Self {
        Self::default()
    }

    fn with<R>(&self, f: impl FnOnce(&mut Captured) -> R) -> R {
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut guard)
    }

    /// Takes everything captured so far, leaving the context empty.
    pub fn take(&self) -> Captured {
        self.with(std::mem::take)
    }

    /// A copy of everything captured so far.
    pub fn snapshot(&self) -> Captured {
        self.with(|c| c.clone())
    }

    /// Runs `fut` with this context as the current one.
    pub async fn scope<F: Future>(self, fut: F) -> F::Output {
        CURRENT.scope(self, fut).await
    }
}

/// The context of the request being processed, if it is being audited.
pub fn current() -> Option<RequestAudit> {
    CURRENT.try_with(|c| c.clone()).ok()
}

fn update(f: impl FnOnce(&mut Captured)) -> bool {
    match current() {
        Some(ctx) => {
            ctx.with(f);
            true
        }
        None => false,
    }
}

/// Reports the tenant the request was resolved to.
pub fn set_tenant(tenant: &str, from_url: bool) {
    update(|c| {
        c.tenant = Some(tenant.to_string());
        c.tenant_from_url = from_url;
    });
}

/// Reports the authenticated user of the request.
pub fn set_user(user: AuditUser) {
    update(|c| c.user = Some(user));
}

/// Reports whether a bundle request is a `batch` or a `transaction`.
pub fn set_bundle_type(bundle_type: &str) {
    update(|c| c.bundle_type = Some(bundle_type.to_string()));
}

/// Adds a patient the request involves.
pub fn add_patient(patient_ref: &str) {
    update(|c| {
        if !c.patients.iter().any(|p| p == patient_ref) {
            c.patients.push(patient_ref.to_string());
        }
    });
}

/// Records the audit description of one processed bundle entry.
///
/// Returns `false` when no audited request is in progress, so the caller can
/// fall back to recording the event itself.
pub fn push_entry(entry: RestEventSpec) -> bool {
    update(|c| c.entries.push(entry))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_reach_the_scoped_context() {
        let ctx = RequestAudit::new();
        ctx.clone()
            .scope(async {
                set_tenant("acme", true);
                set_bundle_type("transaction");
                add_patient("Patient/1");
                add_patient("Patient/1");
            })
            .await;
        let c = ctx.take();
        assert_eq!(c.tenant.as_deref(), Some("acme"));
        assert!(c.tenant_from_url);
        assert_eq!(c.bundle_type.as_deref(), Some("transaction"));
        assert_eq!(c.patients, vec!["Patient/1".to_string()]);
    }

    #[tokio::test]
    async fn reports_outside_a_request_are_ignored() {
        set_tenant("acme", false);
        assert!(!push_entry(RestEventSpec::default()));
        assert!(current().is_none());
    }
}
