//! Tenant → public FHIR base, for code that builds URLs without a request
//! (subscription notification bundles, `SubscriptionStatus` references).
//!
//! With the manager on, a tenant bound to a named endpoint is published under
//! that endpoint's base (`{HFS_BASE_URL}/{path}`); every other tenant keeps
//! the base it had before (the `fallback`, which encodes URL-tenant routing).

use std::sync::Arc;

use helios_endpoints::EndpointRegistry;

/// Maps a tenant to the public base URL its resources are served under.
pub trait BaseUrlResolver: Send + Sync {
    /// The public base for `tenant` (no trailing slash).
    fn public_base_for_tenant(&self, tenant: &str) -> String;
}

impl<F> BaseUrlResolver for F
where
    F: Fn(&str) -> String + Send + Sync,
{
    fn public_base_for_tenant(&self, tenant: &str) -> String {
        self(tenant)
    }
}

/// [`BaseUrlResolver`] reading the registry's current snapshot.
pub struct SnapshotBaseUrlResolver {
    registry: Arc<EndpointRegistry>,
    server_base: String,
    fallback: Arc<dyn Fn(&str) -> String + Send + Sync>,
}

impl SnapshotBaseUrlResolver {
    /// `server_base` is the normalized `HFS_BASE_URL`; `fallback` answers for
    /// tenants of the default endpoint and unbound tenants.
    pub fn new(
        registry: Arc<EndpointRegistry>,
        server_base: impl Into<String>,
        fallback: Arc<dyn Fn(&str) -> String + Send + Sync>,
    ) -> Self {
        Self {
            registry,
            server_base: server_base.into(),
            fallback,
        }
    }
}

impl BaseUrlResolver for SnapshotBaseUrlResolver {
    fn public_base_for_tenant(&self, tenant: &str) -> String {
        let snapshot = self.registry.snapshot();
        match snapshot.tenant_bound_to_named(tenant) {
            Some(e) => format!(
                "{}{}",
                self.server_base.trim_end_matches('/'),
                e.spec.path.prefix()
            ),
            None => (self.fallback)(tenant),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_endpoints::{
        Actor, EndpointName, EndpointPath, EndpointSpec, EndpointStore, FhirRelease,
        MemoryEndpointStore, ensure_default_endpoint,
    };

    #[tokio::test]
    async fn bound_tenants_use_their_endpoint() {
        let store = MemoryEndpointStore::new();
        ensure_default_endpoint(&store, "default", FhirRelease::R4)
            .await
            .unwrap();
        let mut spec = EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab");
        spec.path = EndpointPath::parse("laboratory").unwrap();
        store.create(spec, &Actor::bootstrap()).await.unwrap();
        let reg = Arc::new(EndpointRegistry::load(&store).await.unwrap());
        let r = SnapshotBaseUrlResolver::new(
            reg,
            "https://h/fhir",
            Arc::new(|t: &str| format!("https://h/fhir#{t}")),
        );
        assert_eq!(r.public_base_for_tenant("lab"), "https://h/fhir/laboratory");
        assert_eq!(
            r.public_base_for_tenant("default"),
            "https://h/fhir#default"
        );
        assert_eq!(r.public_base_for_tenant("other"), "https://h/fhir#other");
        let f = |t: &str| t.to_uppercase();
        assert_eq!(f.public_base_for_tenant("x"), "X");
    }
}
