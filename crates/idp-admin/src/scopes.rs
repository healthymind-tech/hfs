//! The scope builder's data: which resource types the realm's endpoints
//! serve (grouped), the presets and the identity/launch scopes
//! ([`IdpConsole::scope_catalog`], `GET /admin/idp/scopes/catalog`), and
//! the analysis of a scope list for a client ([`IdpConsole::analyze_scopes`]).
//!
//! The endpoints are this server's own: every resource type of an
//! endpoint's FHIR release is served (its CapabilityStatement lists exactly
//! these), so the types come from the generated models through
//! [`TypeProvider`] rather than from an HTTP round trip.

use std::collections::BTreeSet;
use std::sync::Arc;

use helios_idp::realm::RealmSpec;
use helios_idp::scope_catalog::{
    self, Analysis, ClientShape, NON_RESOURCE, PRESETS, TypeGroup, group_types,
};
use serde::Serialize;

use crate::diagnostic::{EndpointProvider, EndpointView};
use crate::service::{IdpConsole, OpResult, WizardState};

/// `"R4"` → the resource type names that release serves.
pub type TypeProvider = Arc<dyn Fn(&str) -> Vec<String> + Send + Sync>;

/// One endpoint in the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEndpoint {
    pub name: String,
    pub base_url: String,
    pub fhir_version: String,
    /// `off` | `write-protect` | `full` | `inherit`.
    pub auth_mode: String,
    /// Bound to the realm (tokens of the realm are accepted there).
    pub bound: bool,
    pub resource_types: usize,
}

/// One group of resource types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogGroup {
    pub id: &'static str,
    pub types: Vec<String>,
}

/// A preset as the catalog lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogPreset {
    pub id: &'static str,
    pub scopes: Vec<&'static str>,
    pub backend: bool,
}

/// Where the types of the catalog come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TypeSource {
    /// The endpoints bound to the realm.
    Bound,
    /// The endpoints chosen in the wizard (not bound yet).
    Chosen,
    /// No endpoint is bound or chosen: every endpoint of the server.
    All,
    /// No realm given: every endpoint of the server.
    Server,
}

/// Everything the builder lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    pub source: TypeSource,
    pub endpoints: Vec<CatalogEndpoint>,
    /// Union of the endpoints' types, grouped (`*` is implied).
    pub groups: Vec<CatalogGroup>,
    pub presets: Vec<CatalogPreset>,
    pub non_resource: Vec<&'static str>,
    pub contexts: Vec<&'static str>,
    /// v2 permission letters in canonical order.
    pub permissions: Vec<&'static str>,
}

fn bound_to(v: &EndpointView, realm: &str) -> bool {
    matches!(&v.provider, EndpointProvider::Internal { realm: r } if r == realm)
}

impl IdpConsole {
    /// Sets [`IdpConsole::type_provider`].
    pub fn with_type_provider(mut self, p: Option<TypeProvider>) -> Self {
        self.type_provider = p;
        self
    }

    fn types_of(&self, release: &str) -> Vec<String> {
        match &self.type_provider {
            Some(p) => p(if release.is_empty() { "R4" } else { release }),
            None => vec![],
        }
    }

    /// The endpoints whose types the builder lists for `realm` (bound →
    /// chosen in the wizard → every endpoint), with where they came from.
    async fn scope_endpoints(
        &self,
        realm: Option<&RealmSpec>,
    ) -> (TypeSource, Vec<EndpointView>, Vec<EndpointView>) {
        let all = self.endpoint_views().await;
        let Some(spec) = realm else {
            return (TypeSource::Server, all.clone(), all);
        };
        let name = spec.name.as_str();
        let bound: Vec<EndpointView> = all.iter().filter(|v| bound_to(v, name)).cloned().collect();
        if !bound.is_empty() {
            return (TypeSource::Bound, bound, all);
        }
        let chosen: Vec<EndpointView> = WizardState::of(spec)
            .map(|w| {
                all.iter()
                    .filter(|v| w.endpoints.iter().any(|e| e == &v.name))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if !chosen.is_empty() {
            return (TypeSource::Chosen, chosen, all);
        }
        (TypeSource::All, all.clone(), all)
    }

    /// The builder's catalog for `realm` (by name), or for the whole
    /// server.
    pub async fn scope_catalog(&self, realm: Option<&str>) -> OpResult<Catalog> {
        let rec = match realm {
            Some(r) => Some(self.realm(r).await?),
            None => None,
        };
        let (source, eps, all) = self.scope_endpoints(rec.as_ref().map(|r| &r.spec)).await;
        let mut union = BTreeSet::new();
        let realm_name = rec.as_ref().map(|r| r.spec.name.as_str().to_string());
        let mut endpoints = vec![];
        for v in &all {
            let listed = eps.iter().any(|e| e.name == v.name);
            let types = self.types_of(&v.fhir_version);
            if listed {
                union.extend(types.iter().cloned());
            }
            if listed || source == TypeSource::Server {
                endpoints.push(CatalogEndpoint {
                    name: v.name.clone(),
                    base_url: v.base_url.clone(),
                    fhir_version: v.fhir_version.clone(),
                    auth_mode: v.auth_mode.clone(),
                    bound: realm_name.as_deref().is_some_and(|r| bound_to(v, r)),
                    resource_types: types.len(),
                });
            }
        }
        if union.is_empty() {
            // No endpoint (or no binder): the server's default release.
            union.extend(self.types_of("R4"));
        }
        let union: Vec<String> = union.into_iter().collect();
        Ok(Catalog {
            source,
            endpoints,
            groups: group_types(&union)
                .into_iter()
                .map(|(g, types)| CatalogGroup {
                    id: g.as_str(),
                    types,
                })
                .collect(),
            presets: PRESETS
                .iter()
                .map(|p| CatalogPreset {
                    id: p.id,
                    scopes: p.scopes.to_vec(),
                    backend: p.backend,
                })
                .collect(),
            non_resource: NON_RESOURCE.to_vec(),
            contexts: vec!["patient", "user", "system"],
            permissions: vec!["c", "r", "u", "d", "s"],
        })
    }

    /// Analyses `scopes` for a client of `shape` in `realm`: syntax,
    /// redundancy and the combinations that cannot work there.
    pub async fn analyze_scopes(
        &self,
        realm: &str,
        scopes: &[String],
        shape: &ClientShape,
    ) -> OpResult<Analysis> {
        let rec = self.realm(realm).await?;
        let (source, eps, _) = self.scope_endpoints(Some(&rec.spec)).await;
        let write_protected: Vec<String> = if source == TypeSource::Bound {
            eps.iter()
                .filter(|v| v.auth_mode == "write-protect")
                .map(|v| v.name.clone())
                .collect()
        } else {
            vec![]
        };
        Ok(scope_catalog::analyze(
            scopes,
            shape,
            Some(&rec.spec.features),
            &write_protected,
        ))
    }
}

/// Group ids in display order (for clients that render empty groups).
pub fn group_ids() -> Vec<&'static str> {
    TypeGroup::ALL.iter().map(|g| g.as_str()).collect()
}
