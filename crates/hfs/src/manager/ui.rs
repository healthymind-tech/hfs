//! P5 hook: installs the manager console (`helios_ui::manager`) so `/ui`
//! becomes the overview of every endpoint and `/ui/e/{endpoint}/…` its
//! pages. Runs only with `HFS_MANAGER=on`; without it the console keeps its
//! single-endpoint mode.
//!
//! The console's data port is [`AdminApiBackend`]: an adapter over package
//! P4's admin service ([`AdminEndpointsState`]), so a change made in the
//! console gets exactly the rules of `PUT /admin/endpoints/{name}` —
//! validation, SSRF checks of URLs, secret sealing, the endpoint cap,
//! revision checks, registry reload (read-your-write) and the admin audit
//! event. (`helios-ui` cannot depend on `helios-rest`, hence the adapter
//! lives here, in the wiring.)

use std::sync::Arc;

use async_trait::async_trait;
use helios_endpoints::store::EndpointRevision;
use helios_endpoints::sync::DuplicateRequest;
use helios_endpoints::{Actor, Endpoint, EndpointAuth, EndpointSpec, EndpointStatus, SecretInput};
use helios_rest::handlers::admin_endpoints::AdminEndpointsState;
use helios_rest::handlers::admin_endpoints::error::{ApiError, ErrorKind};
use helios_rest::handlers::admin_endpoints::spec_input::SecretInputs;
use helios_ui::manager::{
    BackendError, DuplicateOutcome, EndpointRouting, EndpointStats, ManagerBackend, ManagerRuntime,
    ProbeLine, SecretChange, SecretChanges, ServerHealth, StoreBackend, set_manager,
};

use super::ManagerContext;

/// Installs the console runtime for this process.
pub fn install_ui(ctx: &ManagerContext) {
    let admin = AdminEndpointsState::builder(ctx.store.clone(), ctx.registry.clone())
        .env_defaults(None)
        .key_ring(ctx.key_ring.clone())
        .server_base(ctx.config.base_url.clone())
        .build();
    let backend = AdminApiBackend {
        admin,
        store: StoreBackend::new(ctx.store.clone()).with_registry(ctx.registry.clone()),
        key_ring: ctx.key_ring.is_some(),
    };
    set_manager(
        ManagerRuntime::new(Arc::new(backend), ctx.registry.clone())
            .with_routing(EndpointRouting::Dispatcher),
    );
}

/// [`ManagerBackend`] over P4's admin service.
pub struct AdminApiBackend {
    admin: AdminEndpointsState,
    /// For the health probe only (store round trip).
    store: StoreBackend,
    key_ring: bool,
}

/// Maps a P4 error onto the console's error (1:1, see P4's report).
fn map(e: ApiError) -> BackendError {
    match e.kind() {
        ErrorKind::NotFound => BackendError::NotFound,
        ErrorKind::Conflict { current } => BackendError::Conflict { current },
        ErrorKind::Duplicate { field, value, by } => BackendError::Duplicate { field, value, by },
        ErrorKind::Invalid => BackendError::Invalid(e.field_errors()),
        ErrorKind::Forbidden | ErrorKind::Unauthorized => BackendError::Forbidden(e.to_string()),
        ErrorKind::NotImplemented => BackendError::NotImplemented,
        ErrorKind::Unavailable | ErrorKind::Internal => BackendError::Unavailable(e.to_string()),
    }
}

fn inputs(changes: SecretChanges) -> SecretInputs {
    changes
        .into_iter()
        .map(|(field, change)| {
            let input = match change {
                SecretChange::Set(v) => SecretInput::Set(v),
                SecretChange::Clear => SecretInput::Clear,
            };
            (field, input)
        })
        .collect()
}

#[async_trait]
impl ManagerBackend for AdminApiBackend {
    async fn list(&self) -> Result<Vec<Endpoint>, BackendError> {
        self.admin.list_endpoints(false).await.map_err(map)
    }

    async fn get(&self, name: &str) -> Result<Option<Endpoint>, BackendError> {
        match self.admin.get_endpoint(name).await {
            Ok(e) => Ok(Some(e)),
            Err(e) if matches!(e.kind(), ErrorKind::NotFound) => Ok(None),
            Err(e) => Err(map(e)),
        }
    }

    async fn create(
        &self,
        spec: EndpointSpec,
        secrets: SecretChanges,
        actor: &Actor,
    ) -> Result<Endpoint, BackendError> {
        self.admin
            .create_endpoint(spec, inputs(secrets), actor)
            .await
            .map(|w| w.endpoint)
            .map_err(map)
    }

    async fn update(
        &self,
        name: &str,
        expected_revision: u64,
        spec: EndpointSpec,
        secrets: SecretChanges,
        actor: &Actor,
    ) -> Result<Endpoint, BackendError> {
        self.admin
            .update_endpoint(
                name,
                expected_revision,
                spec,
                inputs(secrets),
                actor,
                "edited in the console",
            )
            .await
            .map(|w| w.endpoint)
            .map_err(map)
    }

    async fn set_status(
        &self,
        name: &str,
        expected_revision: u64,
        status: EndpointStatus,
        actor: &Actor,
    ) -> Result<Endpoint, BackendError> {
        if status == EndpointStatus::Deleted {
            return self
                .admin
                .delete_endpoint(name, Some(expected_revision), false, actor)
                .await
                .map(|d| d.endpoint)
                .map_err(map);
        }
        self.admin
            .set_endpoint_status(name, Some(expected_revision), status, actor)
            .await
            .map_err(map)
    }

    async fn history(&self, name: &str) -> Result<Vec<EndpointRevision>, BackendError> {
        self.admin
            .endpoint_history(name, 50)
            .await
            .map(|(_, revs)| revs)
            .map_err(map)
    }

    async fn duplicate(
        &self,
        source: &str,
        request: DuplicateRequest,
        actor: &Actor,
    ) -> Result<DuplicateOutcome, BackendError> {
        let name = request.name.clone();
        self.admin
            .duplicate_endpoint(source, request, actor)
            .await
            .map(|started| DuplicateOutcome {
                endpoint: name,
                job_id: Some(started.job_id),
            })
            .map_err(map)
    }

    async fn test_auth(&self, auth: &EndpointAuth) -> Result<Vec<ProbeLine>, BackendError> {
        self.admin
            .test_endpoint_auth(None, Some(auth.clone()))
            .await
            .map(|report| {
                report
                    .checks
                    .into_iter()
                    .map(|c| ProbeLine {
                        ok: c.ok,
                        message: match c.detail {
                            Some(d) => format!("{}: {d}", c.name),
                            None => c.name,
                        },
                    })
                    .collect()
            })
            .map_err(map)
    }

    async fn stats(&self, name: &str) -> Option<EndpointStats> {
        let view = self.admin.endpoint_stats(name, 300).await.ok()?;
        let has_traffic = view.traffic.samples > 0;
        Some(EndpointStats {
            requests_per_minute: has_traffic.then_some(view.traffic.requests_per_minute),
            error_rate: has_traffic.then_some(view.traffic.error_rate),
            p95_ms: has_traffic.then_some(view.traffic.p95_ms),
            last_write: view.storage.last_write,
            sparkline: Vec::new(),
        })
    }

    async fn health(&self) -> ServerHealth {
        let mut health = self.store.health().await;
        health.key_ring = self.key_ring;
        health
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_endpoints::{EndpointName, EndpointRegistry, FhirRelease, MemoryEndpointStore};

    fn admin() -> Actor {
        Actor {
            subject: "admin".into(),
            display: None,
            via: "admin-password".into(),
        }
    }

    async fn backend() -> AdminApiBackend {
        let store: Arc<dyn helios_endpoints::EndpointStore> = Arc::new(MemoryEndpointStore::new());
        store
            .create(
                EndpointSpec::default_endpoint("default", FhirRelease::R4),
                &Actor::bootstrap(),
            )
            .await
            .unwrap();
        let registry = Arc::new(EndpointRegistry::load(store.as_ref()).await.unwrap());
        AdminApiBackend {
            admin: AdminEndpointsState::builder(store.clone(), registry.clone()).build(),
            store: StoreBackend::new(store).with_registry(registry),
            key_ring: false,
        }
    }

    #[tokio::test]
    async fn console_writes_go_through_the_admin_service() {
        let b = backend().await;
        let spec = EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab");
        let created = b
            .create(spec.clone(), SecretChanges::new(), &admin())
            .await
            .unwrap();
        assert_eq!(created.name(), "lab");
        // Same name again: the service's duplicate error, mapped.
        let err = b
            .create(spec, SecretChanges::new(), &admin())
            .await
            .unwrap_err();
        assert!(matches!(err, BackendError::Duplicate { .. }), "{err:?}");
        // Stale revision: conflict with the stored one.
        let mut s = created.spec.clone();
        s.display_name = "Lab 2".into();
        b.update("lab", 1, s.clone(), SecretChanges::new(), &admin())
            .await
            .unwrap();
        assert_eq!(
            b.update("lab", 1, s, SecretChanges::new(), &admin())
                .await
                .unwrap_err(),
            BackendError::Conflict { current: 2 }
        );
        // A secret without a key ring is refused before anything changes.
        let mut s = b.get("lab").await.unwrap().unwrap().spec;
        s.terminology = Some(Default::default());
        let mut secrets = SecretChanges::new();
        secrets.insert("terminology.token".into(), SecretChange::Set("x".into()));
        assert!(b.update("lab", 2, s, secrets, &admin()).await.is_err());
        // Disable then delete.
        b.set_status("lab", 2, EndpointStatus::Disabled, &admin())
            .await
            .unwrap();
        b.set_status("lab", 3, EndpointStatus::Deleted, &admin())
            .await
            .unwrap();
        assert!(b.get("lab").await.unwrap().is_none());
        assert!(!b.history("default").await.unwrap().is_empty());
        assert_eq!(
            b.duplicate(
                "default",
                serde_json::from_str(r#"{"name":"copy"}"#).unwrap(),
                &admin()
            )
            .await
            .unwrap_err(),
            BackendError::NotImplemented
        );
    }
}
