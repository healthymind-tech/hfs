//! The endpoint Security form's "Authorization server: External /
//! Internal" choice (design §5.2, §11.1). A small, delimited integration
//! with the manager's Security section (owned by the console line):
//!
//! * `templates/partials/idp/security-provider.html` — included once in
//!   `partials/manager/section-security.html`; renders nothing unless the
//!   IdP is installed;
//! * [`apply_form`] — called once from `manager::forms::apply_section`
//!   (Security): with `auth_provider=internal` the issuer, JWKS, audience
//!   and SMART fields are generated from the chosen realm, never typed;
//! * [`after_save`] — called once from `manager::pages::save_section`
//!   after a successful save: keeps the realm's bound-endpoint list (the
//!   `aud` values it issues) in step with the endpoint.

use helios_endpoints::{EndpointAuth, EndpointSpec};
use helios_idp_admin::binding::{self, BindMode};
use helios_idp_admin::service;

use crate::manager::forms::{FieldErrors, Form};

/// A realm the form offers.
pub struct RealmChoice {
    pub name: String,
    pub display: String,
    pub issuer: String,
}

/// Realms for the select (empty when the IdP is not installed).
pub fn realm_choices() -> Vec<RealmChoice> {
    let Some(c) = service::global() else { return vec![] };
    let mut v: Vec<RealmChoice> = c
        .admin
        .runtime()
        .realms()
        .iter()
        .filter(|r| r.spec.enabled)
        .map(|r| RealmChoice { name: r.spec.name.as_str().to_string(), display: r.spec.display_name.clone(), issuer: c.issuer(&r.spec) })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// Whether the IdP console is installed (the partial renders only then).
pub fn available() -> bool {
    service::global().is_some()
}

/// The realm whose issuer the auth section names, if any.
pub fn current_realm(issuer: &str) -> String {
    let issuer = issuer.trim_end_matches('/');
    realm_choices().into_iter().find(|r| r.issuer == issuer).map(|r| r.name).unwrap_or_default()
}

/// The realm an endpoint is bound to: its `provider = Internal{realm}`, or
/// (older rows) an issuer equal to a realm's issuer.
pub fn endpoint_realm(endpoint: &str, issuer: &str) -> String {
    let provider = crate::manager::installed().and_then(|m| {
        m.registry
            .snapshot()
            .endpoints()
            .iter()
            .find(|e| e.name() == endpoint)
            .and_then(|e| e.spec.auth.as_ref().and_then(|a| a.provider.internal_realm().map(String::from)))
    });
    provider.unwrap_or_else(|| current_realm(issuer))
}

/// Applies `auth_provider=internal` + `auth_realm` to the parsed auth section.
pub fn apply_form(form: &Form, spec_name: &str, auth: &mut EndpointAuth, errs: &mut FieldErrors) {
    if form.get("auth_provider") == "external" {
        auth.provider = helios_endpoints::AuthProviderRef::External;
        return;
    }
    if form.get("auth_provider") != "internal" {
        return;
    }
    let Some(c) = service::global() else {
        errs.insert("auth.provider".into(), "the built-in authorization server is not enabled (HFS_IDP=on)".into());
        return;
    };
    let name = form.get("auth_realm");
    let Some(rt) = c.admin.runtime().realm(name) else {
        errs.insert("auth.realm".into(), "choose an authorization server".into());
        return;
    };
    let Some(base) = c.admin.runtime().endpoint_urls().base_url(spec_name) else {
        errs.insert("auth.realm".into(), "the endpoint's public URL is unknown".into());
        return;
    };
    let keep_mode = auth.mode;
    binding::apply(auth, &rt.spec, c.public_base(), None, &base, BindMode::Keep);
    // The mode radio on the same form wins over the binding default.
    if form.has("auth_mode") {
        auth.mode = keep_mode;
    }
}

/// After the endpoint was saved: add it to the trusted realm's endpoints,
/// remove it from any other realm.
pub async fn after_save(endpoint: &str, spec: &EndpointSpec) {
    let Some(c) = service::global() else { return };
    let trusted = match spec.auth.as_ref() {
        Some(a) => match a.provider.internal_realm() {
            Some(r) => r.to_string(),
            None => a.issuer.as_deref().map(current_realm).unwrap_or_default(),
        },
        None => String::new(),
    };
    let Ok(realms) = c.realms().await else { return };
    for r in realms {
        let name = r.spec.name.as_str().to_string();
        let listed = r.spec.endpoints.iter().any(|e| e == endpoint);
        let want = name == trusted;
        if listed != want {
            let ep = endpoint.to_string();
            let _ = c
                .update(&name, None, move |s| {
                    if want {
                        s.endpoints.push(ep);
                    } else {
                        s.endpoints.retain(|e| *e != ep);
                    }
                    Ok(())
                })
                .await;
        }
    }
}
