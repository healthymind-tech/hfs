//! The subscriptions engine as a post-commit [`WriteObserver`] (#1078).
//!
//! The server's write paths report every committed write once, to one
//! [`WriteObservers`](helios_persistence::core::WriteObservers) fan-out; this
//! adapter subscribes the engine to it. It turns each resource write that
//! carries a [`WriteNotice`] into the [`ResourceEvent`] the handlers used to
//! build by hand, and hands it to
//! [`SubscriptionEngine::on_resource_event`] on a spawned task.
//!
//! A write the engine could do nothing with — the tenant has no topic that
//! triggers on the resource type and interaction, and the resource is not a
//! Subscription or topic itself — is dropped here, before the event (a deep
//! copy of the resource) is built and a task is spawned for it: on a server
//! without subscriptions that is every write. The check is the one
//! `on_resource_event` would make first. Topic and Subscription writes are
//! applied on spawned tasks, so while one of those is still pending nothing
//! is dropped: a write that follows a topic's create is matched exactly as
//! it was before.
//!
//! What gets announced is decided by the write path, not here: a write whose
//! notice is `None` is deliberately silent (conditional writes are not such a
//! case: they announce like their unconditional counterparts, upstream #862),
//! and aggregate [`WriteEvent::Counts`] and purge [`WriteEvent::Erased`] events
//! never produce a notification.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use helios_persistence::core::{WriteEvent, WriteKind, WriteObserver};
use tracing::debug;

use crate::engine::SubscriptionEngine;
use crate::event::{ResourceEvent, ResourceEventType};

/// Feeds committed resource writes to a [`SubscriptionEngine`].
pub struct SubscriptionWriteObserver {
    engine: Arc<SubscriptionEngine>,
    /// Lifecycle writes (Subscription, topic) handed to a task that has not
    /// finished applying them yet.
    pending_lifecycle: Arc<AtomicUsize>,
    /// Writes handed to the engine so far.
    dispatched: AtomicUsize,
}

impl SubscriptionWriteObserver {
    /// Wraps the engine that should receive resource notifications.
    pub fn new(engine: Arc<SubscriptionEngine>) -> Self {
        Self {
            engine,
            pending_lifecycle: Arc::new(AtomicUsize::new(0)),
            dispatched: AtomicUsize::new(0),
        }
    }

    /// How many writes were handed to the engine (the rest were silent, or
    /// nothing the engine could act on).
    pub fn dispatched_events(&self) -> usize {
        self.dispatched.load(Ordering::Relaxed)
    }
}

/// Decrements the pending-lifecycle count when the task ends, however it
/// ends (completion, panic, runtime shutdown).
struct PendingLifecycle(Arc<AtomicUsize>);

impl Drop for PendingLifecycle {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn event_type(kind: WriteKind) -> ResourceEventType {
    match kind {
        WriteKind::Create => ResourceEventType::Create,
        WriteKind::Update => ResourceEventType::Update,
        WriteKind::Delete => ResourceEventType::Delete,
    }
}

impl std::fmt::Debug for SubscriptionWriteObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubscriptionWriteObserver").finish()
    }
}

/// The engine event a write announces, or `None` when it announces nothing:
/// an aggregate or purge event, a write without a notice, or a notice with no
/// resource id.
fn resource_event(event: &WriteEvent) -> Option<ResourceEvent> {
    let WriteEvent::Resource(write) = event else {
        return None;
    };
    let notice = write.notice.as_ref()?;
    if notice.resource_id.is_empty() {
        return None;
    }
    Some(ResourceEvent {
        tenant_id: write.tenant.clone(),
        fhir_version: write.fhir_version,
        resource_type: write.resource_type.clone(),
        resource_id: notice.resource_id.clone(),
        version_id: notice.version_id.clone(),
        event_type: event_type(notice.kind),
        resource: notice.resource.clone(),
        previous_resource: notice.previous.clone(),
        timestamp: write.at,
    })
}

impl WriteObserver for SubscriptionWriteObserver {
    fn on_write(&self, event: &WriteEvent) {
        // Before anything is copied: could the engine act on this write?
        let lifecycle = match event {
            WriteEvent::Resource(write) => {
                let Some(notice) = write.notice.as_ref() else {
                    return;
                };
                let lifecycle = SubscriptionEngine::is_lifecycle_type(&write.resource_type);
                if !lifecycle
                    && self.pending_lifecycle.load(Ordering::Acquire) == 0
                    && !self.engine.may_handle(
                        write.tenant.as_ref(),
                        &write.resource_type,
                        event_type(notice.kind),
                    )
                {
                    return;
                }
                lifecycle
            }
            _ => return,
        };
        let Some(event) = resource_event(event) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            debug!(
                resource_type = %event.resource_type,
                resource_id = %event.resource_id,
                "No Tokio runtime; subscription event not dispatched"
            );
            return;
        };
        debug!(
            resource_type = %event.resource_type,
            resource_id = %event.resource_id,
            event_type = %event.event_type,
            "Emitting subscription event"
        );
        let engine = Arc::clone(&self.engine);
        let pending = lifecycle.then(|| {
            self.pending_lifecycle.fetch_add(1, Ordering::AcqRel);
            PendingLifecycle(Arc::clone(&self.pending_lifecycle))
        });
        self.dispatched.fetch_add(1, Ordering::Relaxed);
        runtime.spawn(async move {
            let _pending = pending;
            engine.on_resource_event(event).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use helios_fhir::FhirVersion;
    use helios_persistence::core::{ErasedScope, ResourceWrite, WriteNotice, WriteOrigin};
    use helios_persistence::tenant::TenantId;
    use serde_json::json;

    use crate::config::SubscriptionConfig;

    fn write(notice: Option<WriteNotice>) -> WriteEvent {
        WriteEvent::Resource(ResourceWrite {
            tenant: TenantId::new("acme".to_string()),
            fhir_version: FhirVersion::default(),
            resource_type: "Patient".to_string(),
            live_delta: 1,
            notice,
            at: Utc::now(),
        })
    }

    fn notice(kind: WriteKind, id: &str) -> WriteNotice {
        WriteNotice {
            kind,
            resource_id: id.to_string(),
            version_id: "2".to_string(),
            resource: Some(json!({"resourceType": "Patient", "id": id})),
            previous: Some(json!({"resourceType": "Patient", "id": id, "active": true})),
        }
    }

    #[test]
    fn a_notice_becomes_the_same_engine_event_the_handlers_built() {
        let event = write(Some(notice(WriteKind::Update, "p1")));
        let WriteEvent::Resource(source) = &event else {
            unreachable!()
        };
        let built = resource_event(&event).expect("announced");
        assert_eq!(built.tenant_id.as_str(), "acme");
        assert_eq!(built.fhir_version, FhirVersion::default());
        assert_eq!(built.resource_type, "Patient");
        assert_eq!(built.resource_id, "p1");
        assert_eq!(built.version_id, "2");
        assert_eq!(built.event_type, ResourceEventType::Update);
        assert_eq!(
            built.resource,
            Some(json!({"resourceType": "Patient", "id": "p1"}))
        );
        assert_eq!(
            built.previous_resource,
            Some(json!({"resourceType": "Patient", "id": "p1", "active": true}))
        );
        assert_eq!(built.timestamp, source.at);

        for (kind, expected) in [
            (WriteKind::Create, ResourceEventType::Create),
            (WriteKind::Delete, ResourceEventType::Delete),
        ] {
            let built = resource_event(&write(Some(notice(kind, "p1")))).expect("announced");
            assert_eq!(built.event_type, expected);
        }
    }

    #[test]
    fn silent_writes_counts_and_purges_announce_nothing() {
        assert!(resource_event(&write(None)).is_none(), "no notice");
        assert!(
            resource_event(&write(Some(notice(WriteKind::Create, "")))).is_none(),
            "empty resource id"
        );
        assert!(
            resource_event(&WriteEvent::Counts {
                tenant: TenantId::new("acme".to_string()),
                resource_type: "Patient".to_string(),
                created: 3,
                updated: 1,
                deleted: 1,
                origin: WriteOrigin::BulkSubmit,
                at: Utc::now(),
            })
            .is_none()
        );
        assert!(
            resource_event(&WriteEvent::Erased {
                tenant: TenantId::new("acme".to_string()),
                scope: ErasedScope::Tenant,
            })
            .is_none()
        );
    }

    fn engine() -> Arc<SubscriptionEngine> {
        Arc::new(SubscriptionEngine::new(
            SubscriptionConfig::default(),
            "http://localhost:8080".to_string(),
        ))
    }

    #[test]
    fn without_a_runtime_the_event_is_dropped_not_panicked() {
        let observer = SubscriptionWriteObserver::new(engine());
        observer.on_write(&write(Some(notice(WriteKind::Create, "p1"))));
    }

    /// End to end through the engine: a Subscription create notice registers
    /// it, a delete notice deregisters it, and a notice-less write does
    /// neither.
    #[tokio::test]
    async fn announced_writes_reach_the_engine_and_silent_ones_do_not() {
        use crate::topics::{ResourceTrigger, TopicDefinition};
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let hook = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&hook)
            .await;

        let engine = engine();
        let topic_url = "http://example.org/topic/patient-create";
        engine.topic_registry().add_topic(
            "acme",
            TopicDefinition {
                canonical_url: topic_url.to_string(),
                title: None,
                resource_triggers: vec![ResourceTrigger {
                    resource_type: "Patient".to_string(),
                    interactions: vec![ResourceEventType::Create],
                    fhirpath_criteria: None,
                }],
                can_filter_by: vec![],
                notification_shape: vec![],
            },
        );
        let subscription = crate::manager::tests::build_subscription_json(
            topic_url,
            "rest-hook",
            Some(&format!("{}/hook", hook.uri())),
        );
        let observer = SubscriptionWriteObserver::new(Arc::clone(&engine));
        let subscription_write = |kind: WriteKind, announced: bool| {
            WriteEvent::Resource(ResourceWrite {
                tenant: TenantId::new("acme".to_string()),
                fhir_version: FhirVersion::default(),
                resource_type: "Subscription".to_string(),
                live_delta: 0,
                notice: announced.then(|| WriteNotice {
                    kind,
                    resource_id: "sub-1".to_string(),
                    version_id: "1".to_string(),
                    resource: (kind != WriteKind::Delete).then(|| subscription.clone()),
                    previous: None,
                }),
                at: Utc::now(),
            })
        };
        let registered = || engine.manager().get_subscription("acme", "sub-1").is_some();
        async fn settle(check: impl Fn() -> bool) -> bool {
            for _ in 0..200 {
                if check() {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            false
        }

        observer.on_write(&subscription_write(WriteKind::Create, false));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!registered(), "a write without a notice is silent");

        observer.on_write(&subscription_write(WriteKind::Create, true));
        assert!(settle(registered).await, "the create notice registered it");

        observer.on_write(&subscription_write(WriteKind::Delete, true));
        assert!(
            settle(|| !registered()).await,
            "the delete notice deregistered it"
        );
    }
    fn patient_write(tenant: &str, kind: WriteKind) -> WriteEvent {
        WriteEvent::Resource(ResourceWrite {
            tenant: TenantId::new(tenant.to_string()),
            fhir_version: FhirVersion::default(),
            resource_type: "Patient".to_string(),
            live_delta: 1,
            notice: Some(notice(kind, "p1")),
            at: Utc::now(),
        })
    }

    fn patient_create_topic(url: &str) -> crate::topics::TopicDefinition {
        crate::topics::TopicDefinition {
            canonical_url: url.to_string(),
            title: None,
            resource_triggers: vec![crate::topics::ResourceTrigger {
                resource_type: "Patient".to_string(),
                interactions: vec![ResourceEventType::Create],
                fhirpath_criteria: None,
            }],
            can_filter_by: vec![],
            notification_shape: vec![],
        }
    }

    /// A write no topic of the tenant triggers on never becomes an engine
    /// event; once a topic for it exists, the next write does.
    #[tokio::test]
    async fn writes_no_topic_triggers_on_are_dropped_until_a_topic_exists() {
        let engine = engine();
        let observer = SubscriptionWriteObserver::new(Arc::clone(&engine));

        for _ in 0..3 {
            observer.on_write(&patient_write("acme", WriteKind::Create));
        }
        assert_eq!(observer.dispatched_events(), 0, "no topics at all");

        engine
            .topic_registry()
            .add_topic("acme", patient_create_topic("http://example.org/topic/p"));
        observer.on_write(&patient_write("acme", WriteKind::Create));
        assert_eq!(observer.dispatched_events(), 1, "the topic's trigger");

        observer.on_write(&patient_write("acme", WriteKind::Delete));
        observer.on_write(&patient_write("other-tenant", WriteKind::Create));
        assert_eq!(
            observer.dispatched_events(),
            1,
            "another interaction, another tenant: still nothing to do"
        );
    }

    /// A topic write is applied on a spawned task. A resource write reported
    /// right behind it — before that task ran — is still handed to the
    /// engine, and once the topic is registered the following writes are
    /// matched by the registry check.
    #[tokio::test]
    async fn a_write_right_behind_a_topic_write_is_not_dropped() {
        let engine = engine();
        let observer = SubscriptionWriteObserver::new(Arc::clone(&engine));
        let topic_write = WriteEvent::Resource(ResourceWrite {
            tenant: TenantId::new("acme".to_string()),
            fhir_version: FhirVersion::default(),
            resource_type: "SubscriptionTopic".to_string(),
            live_delta: 1,
            notice: Some(WriteNotice {
                kind: WriteKind::Create,
                resource_id: "t1".to_string(),
                version_id: "1".to_string(),
                resource: Some(json!({
                    "resourceType": "SubscriptionTopic",
                    "id": "t1",
                    "url": "http://example.org/topic/patient-create",
                    "status": "active",
                    "resourceTrigger": [{
                        "resource": "Patient",
                        "supportedInteraction": ["create"]
                    }]
                })),
                previous: None,
            }),
            at: Utc::now(),
        });

        // Current-thread runtime: nothing spawned runs before the next await.
        observer.on_write(&topic_write);
        assert_eq!(observer.dispatched_events(), 1);
        assert!(
            !engine.may_handle("acme", "Patient", ResourceEventType::Create),
            "the topic is not registered yet"
        );
        observer.on_write(&patient_write("acme", WriteKind::Create));
        assert_eq!(
            observer.dispatched_events(),
            2,
            "a pending topic write keeps the following write"
        );

        for _ in 0..200 {
            if observer.pending_lifecycle.load(Ordering::Acquire) == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(observer.pending_lifecycle.load(Ordering::Acquire), 0);
        assert!(
            engine.may_handle("acme", "Patient", ResourceEventType::Create),
            "the topic write registered the topic"
        );
        observer.on_write(&patient_write("acme", WriteKind::Create));
        assert_eq!(observer.dispatched_events(), 3, "matched by the registry");
        observer.on_write(&patient_write("acme", WriteKind::Update));
        assert_eq!(
            observer.dispatched_events(),
            3,
            "not a trigger of the topic"
        );
    }
}
