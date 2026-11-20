//! Audit of endpoint administration.
//!
//! Every write of the endpoint admin API is recorded as an `AuditEvent` in
//! the **default** tenant (design §5.2 / §8): `entity.what` = `Endpoint/{name}`,
//! the agent is the [`Actor`] (subject, display, how it authenticated), and
//! details carry the revision and change. Successful writes are recorded by
//! the handlers (they know the revision); refused writes (`4xx`/`5xx`) are
//! recorded by the authorization middleware with the method and path.
//!
//! Without an audit sink the event is logged at `info` instead.

use std::sync::Arc;

use helios_audit::{AuditAction, AuditEventBuilder, AuditSink};
use helios_endpoints::Actor;

/// DICOM "Application Activity" (the package manager's admin events use it too).
const EVENT_SYSTEM: &str = "http://dicom.nema.org/resources/ontology/DCM";
const EVENT_CODE: &str = "110100";

/// Where admin events go.
#[derive(Clone)]
pub struct AdminAudit {
    sink: Option<Arc<dyn AuditSink>>,
    source: String,
    default_tenant: String,
}

impl std::fmt::Debug for AdminAudit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminAudit")
            .field("sink", &self.sink.as_ref().map(|s| s.name().to_string()))
            .field("source", &self.source)
            .field("default_tenant", &self.default_tenant)
            .finish()
    }
}

/// One admin event.
#[derive(Debug, Clone)]
pub struct AdminEvent<'a> {
    /// FHIR audit action.
    pub action: AuditAction,
    /// `0` success, `4` minor failure (refused), `8` serious failure.
    pub outcome: &'static str,
    /// Endpoint name, when the event is about one.
    pub endpoint: Option<&'a str>,
    /// Description (`outcomeDesc`).
    pub description: String,
    /// Extra `entity.detail` pairs.
    pub details: Vec<(&'static str, String)>,
}

impl AdminAudit {
    /// Events into `sink` (or the log), stamped with `source` as observer and
    /// tagged with `default_tenant`.
    pub fn new(
        sink: Option<Arc<dyn AuditSink>>,
        source: impl Into<String>,
        default_tenant: impl Into<String>,
    ) -> Self {
        Self {
            sink,
            source: source.into(),
            default_tenant: default_tenant.into(),
        }
    }

    /// Records `event` by `actor`.
    pub async fn record(&self, actor: &Actor, event: AdminEvent<'_>) {
        let mut b = AuditEventBuilder::new(self.source.clone())
            .action(event.action)
            .outcome(event.outcome)
            .outcome_desc(event.description.clone())
            .event_type(EVENT_SYSTEM, EVENT_CODE)
            .agent(&actor.subject, actor.display.clone(), true)
            .detail("via", actor.via.clone());
        if let Some(name) = event.endpoint {
            b = b.resource("Endpoint", name);
        }
        for (k, v) in &event.details {
            b = b.detail(k, v.clone());
        }
        let built = b.tenant(self.default_tenant.clone()).build();
        match &self.sink {
            Some(sink) => sink.record_in(Some(&self.default_tenant), built).await,
            None => tracing::info!(
                actor = %actor.subject,
                endpoint = event.endpoint.unwrap_or(""),
                outcome = event.outcome,
                "endpoint admin: {}",
                event.description
            ),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use async_trait::async_trait;
    use helios_audit::AuditEvent;
    use std::sync::Mutex;

    /// Keeps every event with its tenant.
    #[derive(Default)]
    pub struct CaptureSink(pub Mutex<Vec<(Option<String>, serde_json::Value)>>);

    #[async_trait]
    impl AuditSink for CaptureSink {
        async fn record(&self, event: AuditEvent) {
            self.record_in(None, event).await;
        }
        async fn record_in(&self, tenant: Option<&str>, event: AuditEvent) {
            self.0.lock().unwrap().push((
                tenant.map(str::to_string),
                serde_json::to_value(&event).unwrap(),
            ));
        }
        async fn flush(&self) {}
        fn name(&self) -> &str {
            "capture"
        }
    }

    #[tokio::test]
    async fn events_go_to_the_default_tenant_with_the_endpoint_entity() {
        let sink = Arc::new(CaptureSink::default());
        let audit = AdminAudit::new(Some(sink.clone()), "Device/hfs", "default");
        assert!(format!("{audit:?}").contains("capture"));
        let actor = Actor {
            subject: "admin".into(),
            display: Some("root".into()),
            via: "admin-password".into(),
        };
        audit
            .record(
                &actor,
                AdminEvent {
                    action: AuditAction::Update,
                    outcome: "0",
                    endpoint: Some("twcore"),
                    description: "updated".into(),
                    details: vec![("revision", "2".into())],
                },
            )
            .await;
        {
            let events = sink.0.lock().unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].0.as_deref(), Some("default"));
            let text = events[0].1.to_string();
            assert!(text.contains("Endpoint/twcore"), "{text}");
            assert!(text.contains("revision"));
            assert!(text.contains("admin-password"));
        }
        // No sink: logged, no panic.
        AdminAudit::new(None, "Device/hfs", "default")
            .record(
                &actor,
                AdminEvent {
                    action: AuditAction::Delete,
                    outcome: "4",
                    endpoint: None,
                    description: "x".into(),
                    details: vec![],
                },
            )
            .await;
    }
}
