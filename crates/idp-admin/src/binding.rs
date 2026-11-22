//! Binding FHIR endpoints to a realm (design §5.2, wizard step 6).
//!
//! A bound endpoint's auth section gets `provider = Internal{realm}` (I5):
//! the resource server verifies with the realm's in-process keys and
//! revoked-grant set, and SMART discovery is generated from the realm. The
//! issuer, JWKS, audience and SMART URLs are filled in as well, so the
//! Security page shows where tokens come from and readers that predate the
//! provider field still see a consistent External-style binding.
//!
//! A binding is recognised by the provider; an endpoint whose issuer equals
//! `{public_base}/idp/{realm}` (written before the provider existed) counts
//! as bound too.

use std::sync::Arc;

use async_trait::async_trait;
use helios_endpoints::{
    Actor, AuthMode, AuthProviderRef, Endpoint, EndpointAuth, EndpointSpec, EndpointStore, StoreError,
};
use helios_idp::metadata::{RealmUrls, smart_capabilities, smart_configuration};
use helios_idp::realm::RealmSpec;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::diagnostic::{EndpointProvider, EndpointView};

/// Where the server fetches a realm's JWKS when verifying tokens on a
/// bound endpoint (`HFS_IDP_JWKS_VIA`: the in-network origin of this same
/// server, so verification does not hairpin through the public proxy).
#[derive(Debug, Clone, Default)]
pub struct BindingUrls {
    pub public_base: Option<Url>,
    pub jwks_via: Option<Url>,
}

/// The auth mode a binding leaves the endpoint in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum BindMode {
    /// Keep the endpoint's mode; `off` becomes `full`.
    #[default]
    Keep,
    WriteProtect,
    Full,
}

impl BindMode {
    pub fn parse(s: &str) -> Self {
        match s {
            "write-protect" => BindMode::WriteProtect,
            "full" => BindMode::Full,
            _ => BindMode::Keep,
        }
    }
}

pub fn mode_str(m: AuthMode) -> &'static str {
    match m {
        AuthMode::Off => "off",
        AuthMode::WriteProtect => "write-protect",
        AuthMode::Full => "full",
    }
}

/// Rewrites an endpoint's auth section to trust `realm`.
pub fn apply(
    auth: &mut EndpointAuth,
    realm: &RealmSpec,
    public_base: &Url,
    jwks_via: Option<&Url>,
    endpoint_base: &str,
    mode: BindMode,
) {
    let issuer = realm.issuer(public_base);
    let urls = RealmUrls::new(&issuer);
    auth.provider = AuthProviderRef::Internal { realm: realm.name.as_str().to_string() };
    auth.mode = match (mode, auth.mode) {
        (BindMode::WriteProtect, _) => AuthMode::WriteProtect,
        (BindMode::Full, _) | (BindMode::Keep, AuthMode::Off) => AuthMode::Full,
        (BindMode::Keep, m) => m,
    };
    auth.issuer = Some(issuer.clone());
    auth.jwks_url = Some(match jwks_via {
        Some(via) => {
            let path = Url::parse(&urls.jwks).map(|u| u.path().to_string()).unwrap_or_default();
            format!("{}{}", via.as_str().trim_end_matches('/'), path)
        }
        None => urls.jwks.clone(),
    });
    auth.audience = Some(endpoint_base.trim_end_matches('/').to_string());
    auth.algorithms = realm.signing_algs.iter().map(|a| a.as_str().to_string()).collect();
    let sc = smart_configuration(realm, &urls);
    let s = &mut auth.smart;
    s.discover_from_issuer = false;
    s.authorization_endpoint = Some(urls.authorization.clone());
    s.token_endpoint = Some(urls.token.clone());
    s.jwks_uri = Some(urls.jwks.clone());
    s.introspection_endpoint = realm.features.introspection.then(|| urls.introspection.clone());
    s.revocation_endpoint = realm.features.revocation.then(|| urls.revocation.clone());
    s.management_endpoint = Some(urls.management.clone());
    s.end_session_endpoint = realm.features.openid_connect.then(|| urls.end_session.clone());
    s.registration_endpoint = None;
    s.capabilities = Some(smart_capabilities(realm).into_iter().map(String::from).collect());
    s.scopes_supported = sc
        .get("scopes_supported")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect());
    // SMART scope families the realm can issue.
    auth.scopes.allow_patient = realm.features.permission_patient;
    auth.scopes.allow_user = realm.features.permission_user;
    auth.scopes.allow_system = realm.features.backend_services;
    auth.scopes.accept_v1 = realm.features.permission_v1;
}

/// The realm an auth section trusts, if its issuer is one of `realms`.
pub fn bound_realm<'a>(
    auth: Option<&EndpointAuth>,
    realms: &'a [RealmSpec],
    public_base: &Url,
) -> Option<&'a RealmSpec> {
    let auth = auth?;
    if let Some(name) = auth.provider.internal_realm() {
        return realms.iter().find(|r| r.name.as_str() == name);
    }
    let issuer = auth.issuer.as_deref()?.trim_end_matches('/');
    realms.iter().find(|r| r.issuer(public_base) == issuer)
}

/// A change an endpoint would undergo (the wizard's diff preview).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BindDiff {
    pub endpoint: String,
    /// `off` / `external` / `internal:{realm}` before.
    pub before: String,
    pub before_issuer: Option<String>,
    pub mode_before: String,
    pub mode_after: String,
    pub audience: String,
    /// Inherits server-wide auth today (binding gives it its own section).
    pub inherited: bool,
}

/// Endpoint operations the admin service needs.
#[async_trait]
pub trait EndpointBinder: Send + Sync {
    /// Every endpoint as the diagnostic sees it.
    async fn views(&self) -> Result<Vec<EndpointView>, String>;
    /// What [`EndpointBinder::bind`] would change.
    async fn diff(&self, endpoint: &str, realm: &RealmSpec, mode: BindMode) -> Result<BindDiff, String>;
    async fn bind(&self, endpoint: &str, realm: &RealmSpec, mode: BindMode, actor: &Actor) -> Result<(), String>;
    /// Removes the binding: the endpoint goes back to the server-wide auth
    /// settings (inherit), never silently open.
    async fn unbind(&self, endpoint: &str, realm: &RealmSpec, actor: &Actor) -> Result<(), String>;
}

/// [`EndpointBinder`] over the endpoint store.
pub struct StoreBinder {
    pub store: Arc<dyn EndpointStore>,
    /// `HFS_BASE_URL` (FHIR server base).
    pub server_base: String,
    pub public_base: Url,
    pub jwks_via: Option<Url>,
    /// Realm specs, to recognise existing bindings.
    pub realms: Arc<dyn Fn() -> Vec<RealmSpec> + Send + Sync>,
}

fn store_err(e: StoreError) -> String {
    e.to_string()
}

impl StoreBinder {
    fn view(&self, e: &Endpoint, realms: &[RealmSpec]) -> EndpointView {
        let auth = e.spec.auth.as_ref();
        EndpointView {
            name: e.name().to_string(),
            base_url: e.public_base(&self.server_base),
            auth_mode: auth.map(|a| mode_str(a.mode)).unwrap_or("inherit").to_string(),
            provider: match bound_realm(auth, realms, &self.public_base) {
                Some(r) => EndpointProvider::Internal {
                    realm: r.name.as_str().to_string(),
                },
                None => EndpointProvider::External {
                    issuer: auth.and_then(|a| a.issuer.clone()),
                },
            },
        }
    }

    async fn get(&self, name: &str) -> Result<Endpoint, String> {
        self.store
            .get(name)
            .await
            .map_err(store_err)?
            .ok_or_else(|| format!("endpoint {name} does not exist"))
    }
}

pub fn describe(spec: &EndpointSpec, realms: &[RealmSpec], public_base: &Url) -> (String, Option<String>, String) {
    match spec.auth.as_ref() {
        None => ("inherit".into(), None, "inherit".into()),
        Some(a) => {
            let what = match bound_realm(Some(a), realms, public_base) {
                Some(r) => format!("internal:{}", r.name.as_str()),
                None if a.mode == AuthMode::Off => "off".into(),
                None => "external".into(),
            };
            (what, a.issuer.clone(), mode_str(a.mode).into())
        }
    }
}

#[async_trait]
impl EndpointBinder for StoreBinder {
    async fn views(&self) -> Result<Vec<EndpointView>, String> {
        let realms = (self.realms)();
        Ok(self
            .store
            .list(false)
            .await
            .map_err(store_err)?
            .iter()
            .map(|e| self.view(e, &realms))
            .collect())
    }

    async fn diff(&self, endpoint: &str, realm: &RealmSpec, mode: BindMode) -> Result<BindDiff, String> {
        let e = self.get(endpoint).await?;
        let realms = (self.realms)();
        let (before, before_issuer, mode_before) = describe(&e.spec, &realms, &self.public_base);
        let base = e.public_base(&self.server_base);
        let mut auth = e.spec.auth.clone().unwrap_or_default();
        apply(&mut auth, realm, &self.public_base, self.jwks_via.as_ref(), &base, mode);
        Ok(BindDiff {
            endpoint: endpoint.to_string(),
            before,
            before_issuer,
            mode_before,
            mode_after: mode_str(auth.mode).into(),
            audience: base,
            inherited: e.spec.auth.is_none(),
        })
    }

    async fn bind(&self, endpoint: &str, realm: &RealmSpec, mode: BindMode, actor: &Actor) -> Result<(), String> {
        let e = self.get(endpoint).await?;
        let base = e.public_base(&self.server_base);
        let mut spec = e.spec.clone();
        let mut auth = spec.auth.clone().unwrap_or_default();
        apply(&mut auth, realm, &self.public_base, self.jwks_via.as_ref(), &base, mode);
        spec.auth = Some(auth);
        self.store
            .update(endpoint, e.revision, spec, actor)
            .await
            .map_err(store_err)?;
        Ok(())
    }

    async fn unbind(&self, endpoint: &str, realm: &RealmSpec, actor: &Actor) -> Result<(), String> {
        let e = self.get(endpoint).await?;
        let realms = vec![realm.clone()];
        if bound_realm(e.spec.auth.as_ref(), &realms, &self.public_base).is_none() {
            return Ok(());
        }
        let mut spec = e.spec.clone();
        spec.auth = None;
        self.store
            .update(endpoint, e.revision, spec, actor)
            .await
            .map_err(store_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_idp::realm::RealmName;

    #[test]
    fn apply_points_the_endpoint_at_the_realm() {
        let mut realm = RealmSpec::new(RealmName::parse("clinic").unwrap(), "Clinic");
        realm.features.introspection = true;
        let base = Url::parse("https://hfs.example.org").unwrap();
        let mut auth = EndpointAuth::default();
        apply(&mut auth, &realm, &base, None, "https://hfs.example.org/fhir/ep1/", BindMode::Keep);
        assert_eq!(auth.mode, AuthMode::Full);
        assert_eq!(auth.issuer.as_deref(), Some("https://hfs.example.org/idp/clinic"));
        assert_eq!(auth.jwks_url.as_deref(), Some("https://hfs.example.org/idp/clinic/jwks"));
        assert_eq!(auth.audience.as_deref(), Some("https://hfs.example.org/fhir/ep1"));
        assert_eq!(auth.smart.token_endpoint.as_deref(), Some("https://hfs.example.org/idp/clinic/token"));
        assert!(!auth.smart.discover_from_issuer);
        assert!(auth.smart.capabilities.as_ref().unwrap().iter().any(|c| c == "launch-standalone"));
        assert!(bound_realm(Some(&auth), std::slice::from_ref(&realm), &base).is_some());

        let via = Url::parse("http://127.0.0.1:8080").unwrap();
        let mut auth = EndpointAuth { mode: AuthMode::WriteProtect, ..Default::default() };
        apply(&mut auth, &realm, &base, Some(&via), "https://h/fhir/x", BindMode::Keep);
        assert_eq!(auth.mode, AuthMode::WriteProtect);
        assert_eq!(auth.jwks_url.as_deref(), Some("http://127.0.0.1:8080/idp/clinic/jwks"));
    }
}
