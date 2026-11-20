//! The immutable routing snapshot and request-path resolution.
//!
//! The REST layer's endpoint dispatcher (package P1) holds the current
//! [`EndpointSnapshot`] through [`crate::EndpointRegistry`] and calls
//! [`EndpointSnapshot::resolve`] once per FHIR request on the server-relative
//! path (after the reverse proxy stripped `/fhir`). On a match it rewrites the
//! request URI to [`ResolvedEndpoint::remaining_path`] and inserts the
//! [`ResolvedEndpoint`] as a request extension, which every downstream layer
//! (audit, CORS, rate limit, authentication, authorization, tenant extractor,
//! public-URL builder, validation) reads instead of global configuration.

use std::collections::HashMap;
use std::sync::Arc;

use crate::model::{Endpoint, EndpointStatus};

/// Why a set of endpoints cannot form a snapshot (the store prevents all of
/// these; a snapshot refuses them anyway so routing is never ambiguous).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotError {
    /// Two routed endpoints share a name.
    #[error("duplicate endpoint name {0}")]
    DuplicateName(String),
    /// Two routed endpoints share a path.
    #[error("duplicate endpoint path {0:?}")]
    DuplicatePath(String),
    /// Two routed endpoints share a tenant (v1 is one endpoint per tenant).
    #[error("tenant {0} is bound to more than one endpoint")]
    DuplicateTenant(String),
}

/// The endpoints the router knows at one store epoch. Cheap to clone (Arcs).
#[derive(Debug, Clone, Default)]
pub struct EndpointSnapshot {
    epoch: u64,
    endpoints: Vec<Arc<Endpoint>>,
    by_name: HashMap<String, Arc<Endpoint>>,
    by_path: HashMap<String, Arc<Endpoint>>,
    by_tenant: HashMap<String, Arc<Endpoint>>,
    default: Option<Arc<Endpoint>>,
}

/// The endpoint a request addresses, as inserted into request extensions.
#[derive(Debug, Clone)]
pub struct ResolvedEndpoint {
    /// The endpoint (its config as of the snapshot the request saw).
    pub endpoint: Arc<Endpoint>,
    /// Server-relative prefix that was stripped (`""` for the default).
    pub prefix: String,
    /// The path the FHIR router routes (`/Patient/1`, `/` for the base).
    pub remaining_path: String,
    /// The path as received (`/twcore/Patient/1`).
    pub original_path: String,
    /// Snapshot epoch, for caches keyed per configuration (CapabilityStatement).
    pub epoch: u64,
}

impl ResolvedEndpoint {
    /// Whether this is the default (root) endpoint.
    pub fn is_default(&self) -> bool {
        self.endpoint.spec.is_default()
    }

    /// Public base URL given `HFS_BASE_URL`.
    pub fn public_base(&self, server_base: &str) -> String {
        format!("{}{}", server_base.trim_end_matches('/'), self.prefix)
    }
}

/// Outcome of resolving a request path.
#[derive(Debug, Clone)]
pub enum Resolution {
    /// Route to this endpoint.
    Serve(ResolvedEndpoint),
    /// The path names an endpoint that is disabled or provisioning: answer
    /// `503` + OperationOutcome (`transient`) naming it.
    Unavailable(ResolvedEndpoint),
    /// No endpoint (no default endpoint in the snapshot either): `404`.
    NotFound,
}

impl EndpointSnapshot {
    /// Builds a snapshot from stored endpoints; deleted ones are skipped.
    pub fn new(epoch: u64, endpoints: Vec<Endpoint>) -> Result<Self, SnapshotError> {
        let mut snap = EndpointSnapshot {
            epoch,
            ..Default::default()
        };
        for e in endpoints {
            if !e.spec.status.is_routed() {
                continue;
            }
            let e = Arc::new(e);
            let name = e.name().to_string();
            let path = e.spec.path.as_str().to_string();
            let tenant = e.tenant_id().to_string();
            if snap.by_name.insert(name.clone(), Arc::clone(&e)).is_some() {
                return Err(SnapshotError::DuplicateName(name));
            }
            if snap.by_path.insert(path.clone(), Arc::clone(&e)).is_some() {
                return Err(SnapshotError::DuplicatePath(path));
            }
            if snap
                .by_tenant
                .insert(tenant.clone(), Arc::clone(&e))
                .is_some()
            {
                return Err(SnapshotError::DuplicateTenant(tenant));
            }
            if e.spec.is_default() {
                snap.default = Some(Arc::clone(&e));
            }
            snap.endpoints.push(e);
        }
        snap.endpoints.sort_by(|a, b| {
            (!a.spec.is_default(), a.name()).cmp(&(!b.spec.is_default(), b.name()))
        });
        Ok(snap)
    }

    /// Store epoch this snapshot reflects.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Routed endpoints, default first, then by name.
    pub fn endpoints(&self) -> &[Arc<Endpoint>] {
        &self.endpoints
    }

    /// By name.
    pub fn by_name(&self, name: &str) -> Option<&Arc<Endpoint>> {
        self.by_name.get(name)
    }

    /// The endpoint bound to a tenant (subscriptions, bulk jobs and audit use
    /// this to find the public base of a tenant's events).
    pub fn by_tenant(&self, tenant: &str) -> Option<&Arc<Endpoint>> {
        self.by_tenant.get(tenant)
    }

    /// The default endpoint.
    pub fn default_endpoint(&self) -> Option<&Arc<Endpoint>> {
        self.default.as_ref()
    }

    /// Whether `tenant` is bound to a named (non-default) endpoint: the
    /// default endpoint must then refuse `X-Tenant-ID: tenant` (`403`,
    /// "served at /fhir/{path}") so the tenant has exactly one door.
    pub fn tenant_bound_to_named(&self, tenant: &str) -> Option<&Arc<Endpoint>> {
        self.by_tenant.get(tenant).filter(|e| !e.spec.is_default())
    }

    /// Resolves a server-relative request path (no query string).
    ///
    /// The first segment is compared exactly (case-sensitive) with the named
    /// endpoints' paths; anything else belongs to the default endpoint with
    /// the path unchanged.
    pub fn resolve(&self, path: &str) -> Resolution {
        let trimmed = path.strip_prefix('/').unwrap_or(path);
        let (first, rest) = match trimmed.find('/') {
            Some(i) => (&trimmed[..i], &trimmed[i..]),
            None => (trimmed, ""),
        };
        if !first.is_empty()
            && let Some(e) = self.by_path.get(first)
            && !e.spec.path.is_root()
        {
            let resolved = ResolvedEndpoint {
                endpoint: Arc::clone(e),
                prefix: format!("/{first}"),
                remaining_path: if rest.is_empty() {
                    "/".to_string()
                } else {
                    rest.to_string()
                },
                original_path: path.to_string(),
                epoch: self.epoch,
            };
            return if e.spec.status == EndpointStatus::Enabled {
                Resolution::Serve(resolved)
            } else {
                Resolution::Unavailable(resolved)
            };
        }
        match &self.default {
            Some(d) => {
                let resolved = ResolvedEndpoint {
                    endpoint: Arc::clone(d),
                    prefix: String::new(),
                    remaining_path: if path.is_empty() {
                        "/".into()
                    } else {
                        path.to_string()
                    },
                    original_path: path.to_string(),
                    epoch: self.epoch,
                };
                if d.spec.status == EndpointStatus::Enabled {
                    Resolution::Serve(resolved)
                } else {
                    Resolution::Unavailable(resolved)
                }
            }
            None => Resolution::NotFound,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{EndpointSpec, FhirRelease};
    use crate::name::EndpointName;
    use chrono::Utc;

    pub(crate) fn endpoint(spec: EndpointSpec) -> Endpoint {
        Endpoint {
            id: uuid::Uuid::new_v4().to_string(),
            revision: 1,
            spec,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            created_by: None,
            updated_by: None,
        }
    }

    fn named(n: &str) -> Endpoint {
        endpoint(EndpointSpec::named(EndpointName::parse(n).unwrap(), n))
    }

    fn snap() -> EndpointSnapshot {
        let mut disabled = named("old");
        disabled.spec.status = EndpointStatus::Disabled;
        let mut deleted = named("gone");
        deleted.spec.status = EndpointStatus::Deleted;
        EndpointSnapshot::new(
            7,
            vec![
                named("twcore"),
                endpoint(EndpointSpec::default_endpoint("default", FhirRelease::R4)),
                disabled,
                deleted,
            ],
        )
        .unwrap()
    }

    fn serve(r: Resolution) -> ResolvedEndpoint {
        match r {
            Resolution::Serve(r) => r,
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    #[test]
    fn named_paths() {
        let s = snap();
        let r = serve(s.resolve("/twcore/Patient/1"));
        assert_eq!(r.endpoint.name(), "twcore");
        assert_eq!(r.prefix, "/twcore");
        assert_eq!(r.remaining_path, "/Patient/1");
        assert_eq!(r.original_path, "/twcore/Patient/1");
        assert_eq!(r.epoch, 7);
        assert_eq!(r.public_base("https://h/fhir/"), "https://h/fhir/twcore");
        assert_eq!(serve(s.resolve("/twcore")).remaining_path, "/");
        assert_eq!(serve(s.resolve("/twcore/")).remaining_path, "/");
        assert_eq!(
            serve(s.resolve("/twcore/.well-known/smart-configuration")).remaining_path,
            "/.well-known/smart-configuration"
        );
    }

    #[test]
    fn everything_else_is_default() {
        let s = snap();
        for p in [
            "/Patient/1",
            "/metadata",
            "/",
            "",
            "/twcorex/Patient",
            "/TWCORE/Patient",
            "/gone/Patient",
        ] {
            let r = serve(s.resolve(p));
            assert!(r.is_default(), "{p}");
            assert_eq!(r.prefix, "");
        }
        assert_eq!(serve(s.resolve("/Patient/1")).remaining_path, "/Patient/1");
    }

    #[test]
    fn disabled_is_unavailable_deleted_is_not_routed() {
        let s = snap();
        assert!(matches!(
            s.resolve("/old/Patient"),
            Resolution::Unavailable(_)
        ));
        assert!(s.by_name("gone").is_none());
        assert_eq!(s.endpoints().len(), 3);
        assert!(s.endpoints()[0].spec.is_default());
    }

    #[test]
    fn tenant_binding() {
        let s = snap();
        assert_eq!(s.tenant_bound_to_named("twcore").unwrap().name(), "twcore");
        assert!(s.tenant_bound_to_named("default").is_none());
        assert!(s.tenant_bound_to_named("eval-t1").is_none());
    }

    #[test]
    fn no_default_means_not_found() {
        let s = EndpointSnapshot::new(1, vec![named("a")]).unwrap();
        assert!(matches!(s.resolve("/Patient"), Resolution::NotFound));
        assert!(matches!(s.resolve("/a/Patient"), Resolution::Serve(_)));
    }

    #[test]
    fn duplicates_refused() {
        let a = named("a");
        let mut b = named("b");
        b.spec.storage = a.spec.storage.clone();
        assert_eq!(
            EndpointSnapshot::new(1, vec![a.clone(), b]).unwrap_err(),
            SnapshotError::DuplicateTenant("a".into())
        );
        assert!(matches!(
            EndpointSnapshot::new(1, vec![a.clone(), a]).unwrap_err(),
            SnapshotError::DuplicateName(_)
        ));
    }
}
