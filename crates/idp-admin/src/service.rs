//! The console's service over [`IdpAdmin`]: realm templates, the creation
//! wizard's server-side drafts, endpoint binding, and the diagnostic with
//! throw-away fixtures and a per-realm history. The console pages
//! (`crates/ui` `idp`) and the admin API ([`crate::api`]) both call this.
//!
//! **Drafts.** A wizard draft is a realm row with `enabled = false` and a
//! `branding.wizard` object (`{"draft": true, "step": n, "template": …,
//! "endpoints": […], "bindMode": …}`), so a draft survives restarts and is
//! visible on every instance, needs no extra table, and is committed by
//! clearing the marker and enabling the realm.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use chrono::Utc;
use helios_endpoints::Actor;
use helios_idp::IdpError;
use helios_idp::admin::{CreatedClient, IdpAdmin};
use helios_idp::client::{ClientAuth, ClientSpec, GrantType, LaunchType};
use helios_idp::keys::{RsaBits, SigningAlg, SigningKey};
use helios_idp::realm::{RealmFeatures, RealmName, RealmSpec, UserSource};
use helios_idp::scope::ScopePolicy;
use helios_idp::store::RealmRecord;
use helios_idp::user::{FhirLink, RealmUser, UserKind};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use crate::binding::{BindDiff, BindMode, EndpointBinder};
use crate::diagnostic::{
    self, AppProbe, EndpointView, Fixture, PreflightInput, Report, RunConfig, Status,
};

/// Redirect URI of the diagnostic's throw-away public client. Never
/// fetched: the runner reads the authorization response from `Location`.
pub const DIAGNOSTIC_REDIRECT: &str = "https://diagnostic.invalid/callback";
/// Prefix of throw-away clients and users (hidden from the lists).
pub const DIAGNOSTIC_PREFIX: &str = "hm-diagnostic-";
const HISTORY: usize = 20;

/// The wizard's starting points (step 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Template {
    /// Patients sign in to standalone apps (patient portal apps).
    Patient,
    /// Clinicians launch apps from the console (EHR launch + standalone).
    Clinician,
    /// System-to-system (Backend Services) only.
    Backend,
    /// Every phase-1 SMART capability.
    Everything,
    Custom,
}

impl Template {
    pub const ALL: [Template; 5] = [
        Template::Patient,
        Template::Clinician,
        Template::Backend,
        Template::Everything,
        Template::Custom,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Template::Patient => "patient",
            Template::Clinician => "clinician",
            Template::Backend => "backend",
            Template::Everything => "everything",
            Template::Custom => "custom",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == s)
    }

    /// The realm features this template presets (steps 3–5).
    pub fn apply(self, spec: &mut RealmSpec) {
        let mut f = RealmFeatures::default();
        match self {
            Template::Patient => {
                f.launch_ehr = false;
                f.client_confidential_asymmetric = false;
                f.backend_services = false;
                f.permission_user = false;
                spec.user_source = UserSource::Local;
            }
            Template::Clinician => {
                f.backend_services = false;
                f.client_confidential_asymmetric = false;
                spec.user_source = UserSource::Local;
            }
            Template::Backend => {
                f.launch_ehr = false;
                f.launch_standalone = false;
                f.client_public = false;
                f.client_confidential_symmetric = false;
                f.openid_connect = false;
                f.offline_access = false;
                f.permission_patient = false;
                f.permission_user = false;
                f.context_banner = false;
                spec.signing_algs = vec![SigningAlg::ES256, SigningAlg::RS256];
            }
            Template::Everything | Template::Custom => {}
        }
        spec.features = f;
    }
}

/// `branding.wizard` of a draft realm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WizardState {
    pub draft: bool,
    pub step: u8,
    pub template: Option<Template>,
    /// Endpoints chosen in step 6 (bound at the end, after the checks).
    #[serde(default)]
    pub endpoints: Vec<String>,
    #[serde(default)]
    pub bind_mode: BindMode,
    /// Patient id the test user links to (patient-scope checks).
    #[serde(default)]
    pub test_patient: Option<String>,
}

impl WizardState {
    pub fn of(spec: &RealmSpec) -> Option<WizardState> {
        spec.branding
            .get("wizard")
            .and_then(|w| serde_json::from_value::<WizardState>(w.clone()).ok())
            .filter(|w| w.draft)
    }

    pub fn store(&self, spec: &mut RealmSpec) {
        if !spec.branding.is_object() {
            spec.branding = json!({});
        }
        spec.branding["wizard"] = serde_json::to_value(self).expect("serializes");
    }

    pub fn clear(spec: &mut RealmSpec) {
        if let Some(o) = spec.branding.as_object_mut() {
            o.remove("wizard");
        }
    }
}

/// Error of a console operation: a field-anchored validation error, or
/// another failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpError {
    /// HTTP-ish status: 400 invalid, 404, 409 conflict, 503 unavailable.
    pub status: u16,
    /// Console field (`name`, `lifetimes.accessToken`, `redirectUris`…).
    pub field: Option<String>,
    pub message: String,
}

impl From<IdpError> for OpError {
    fn from(e: IdpError) -> Self {
        match e {
            IdpError::Invalid(field, message) => OpError { status: 400, field: Some(field), message },
            IdpError::NotFound(m) => OpError { status: 404, field: None, message: format!("not found: {m}") },
            IdpError::Conflict(m) => OpError { status: 409, field: None, message: m },
            other => OpError { status: 500, field: None, message: other.to_string() },
        }
    }
}

impl OpError {
    pub fn invalid(field: &str, message: impl Into<String>) -> Self {
        OpError { status: 400, field: Some(field.to_string()), message: message.into() }
    }
    pub fn other(status: u16, message: impl Into<String>) -> Self {
        OpError { status, field: None, message: message.into() }
    }
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.field {
            Some(fl) => write!(f, "{fl}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

pub type OpResult<T> = Result<T, OpError>;

/// Outcome of the wizard's last step.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishOutcome {
    pub realm: String,
    /// Endpoints switched to this realm.
    pub bound: Vec<String>,
    /// Endpoints left as they were because the checks failed first.
    pub not_bound: Vec<String>,
    pub report: Report,
}

/// Options of a diagnostic run.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnoseOptions {
    /// Also check this registered app (client id + redirect URI).
    pub app: Option<AppProbe>,
    /// Link the test user to this Patient on the first endpoint and use
    /// patient scopes.
    pub patient: Option<String>,
    /// Run against these endpoints instead of the realm's bound ones
    /// (wizard: before binding).
    #[serde(skip)]
    pub endpoints_override: Option<Vec<String>>,
    /// Use the endpoints as `aud` only (they are not bound yet).
    #[serde(skip)]
    pub audience_only: bool,
}

/// Installed once by the server (`HFS_IDP=on`).
pub struct IdpConsole {
    pub admin: IdpAdmin,
    pub binder: Option<Arc<dyn EndpointBinder>>,
    /// In-network origin for the diagnostic (`HFS_IDP_DIAGNOSTIC_VIA`).
    pub fetch_via: Option<Url>,
    pub timeout: Duration,
    history: Mutex<HashMap<String, VecDeque<Report>>>,
}

static GLOBAL: RwLock<Option<Arc<IdpConsole>>> = RwLock::new(None);

/// Installs the console service (the most recent call wins).
pub fn install(c: Arc<IdpConsole>) {
    *GLOBAL.write().unwrap_or_else(|p| p.into_inner()) = Some(c);
}

/// Removes it (tests).
pub fn uninstall() {
    *GLOBAL.write().unwrap_or_else(|p| p.into_inner()) = None;
}

pub fn global() -> Option<Arc<IdpConsole>> {
    GLOBAL.read().unwrap_or_else(|p| p.into_inner()).clone()
}

fn rand_suffix() -> String {
    helios_idp::util::random_b64(9)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { 'x' })
        .collect()
}

impl IdpConsole {
    pub fn new(admin: IdpAdmin, binder: Option<Arc<dyn EndpointBinder>>) -> Self {
        Self {
            admin,
            binder,
            fetch_via: None,
            timeout: Duration::from_secs(10),
            history: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_fetch_via(mut self, via: Option<Url>) -> Self {
        self.fetch_via = via;
        self
    }

    pub fn public_base(&self) -> &Url {
        self.admin.runtime().public_base()
    }

    pub fn issuer(&self, spec: &RealmSpec) -> String {
        spec.issuer(self.public_base())
    }

    /// Whether signing keys can be created (`HFS_SECRET_KEYS`).
    pub fn keys_available(&self) -> bool {
        self.admin.runtime().key_ring().is_some()
    }

    pub async fn realms(&self) -> OpResult<Vec<RealmRecord>> {
        let mut v = self.admin.list_realms().await?;
        v.sort_by(|a, b| a.spec.name.as_str().cmp(b.spec.name.as_str()));
        Ok(v)
    }

    pub async fn realm(&self, name: &str) -> OpResult<RealmRecord> {
        self.admin
            .runtime()
            .store()
            .get_realm_by_name(name)
            .await?
            .ok_or_else(|| OpError::other(404, format!("no authorization server named {name}")))
    }

    // ------------------------------------------------------------ wizard

    /// Step 1: validates the basics and creates the draft (keys are
    /// generated now — step 2 shows them and may change algorithms).
    pub async fn create_draft(&self, name: &str, display_name: &str, template: Template) -> OpResult<RealmRecord> {
        let name = RealmName::parse(name.trim()).map_err(|e| match e {
            IdpError::Invalid(_, m) => OpError::invalid("name", m),
            other => other.into(),
        })?;
        if display_name.trim().is_empty() {
            return Err(OpError::invalid("displayName", "enter a display name"));
        }
        if self.admin.runtime().store().get_realm_by_name(name.as_str()).await?.is_some() {
            return Err(OpError::invalid("name", format!("an authorization server named {} already exists", name.as_str())));
        }
        if !self.keys_available() {
            return Err(OpError::other(409, "configure HFS_SECRET_KEYS before creating an authorization server (signing keys are sealed with it)"));
        }
        let mut spec = RealmSpec::new(name, display_name.trim());
        template.apply(&mut spec);
        spec.enabled = false;
        WizardState { draft: true, step: 2, template: Some(template), ..Default::default() }.store(&mut spec);
        Ok(self.admin.create_realm(spec).await?)
    }

    /// Saves a step: `edit` changes the spec; the wizard step advances to
    /// `next_step`.
    pub async fn save_step(
        &self,
        name: &str,
        next_step: u8,
        edit: impl FnOnce(&mut RealmSpec, &mut WizardState) -> OpResult<()>,
    ) -> OpResult<RealmRecord> {
        let rec = self.realm(name).await?;
        let mut spec = rec.spec.clone();
        let mut w = WizardState::of(&spec).unwrap_or(WizardState { draft: true, step: 1, ..Default::default() });
        edit(&mut spec, &mut w)?;
        w.step = w.step.max(next_step);
        w.store(&mut spec);
        Ok(self.admin.update_realm(&rec.id, spec, rec.revision).await?)
    }

    /// Updates a realm's settings outside the wizard.
    pub async fn update(&self, name: &str, revision: Option<i64>, edit: impl FnOnce(&mut RealmSpec) -> OpResult<()>) -> OpResult<RealmRecord> {
        let rec = self.realm(name).await?;
        if let Some(r) = revision
            && r != rec.revision
        {
            return Err(OpError::other(409, format!("the settings were changed by someone else (revision {} → {}); reload and try again", r, rec.revision)));
        }
        let mut spec = rec.spec.clone();
        edit(&mut spec)?;
        Ok(self.admin.update_realm(&rec.id, spec, rec.revision).await?)
    }

    /// Step 7: enables the realm, runs the checks against the chosen
    /// endpoints *before* switching them, binds them only if nothing
    /// failed, then re-runs the endpoint-facing checks.
    pub async fn finish(&self, name: &str, actor: &Actor) -> OpResult<FinishOutcome> {
        let rec = self.realm(name).await?;
        let w = WizardState::of(&rec.spec).unwrap_or_default();
        let mut spec = rec.spec.clone();
        spec.enabled = true;
        // aud for the chosen endpoints must be issuable during the checks.
        for e in &w.endpoints {
            if !spec.endpoints.contains(e) {
                spec.endpoints.push(e.clone());
            }
        }
        WizardState::clear(&mut spec);
        let mut keep = w.clone();
        keep.draft = false;
        let rec = self.admin.update_realm(&rec.id, spec, rec.revision).await?;
        let opts = DiagnoseOptions { patient: w.test_patient.clone(), endpoints_override: Some(w.endpoints.clone()), audience_only: true, ..Default::default() };
        let pre = self.diagnose(name, opts).await?;
        let mut bound = vec![];
        let mut not_bound = vec![];
        let realm_ok = pre.steps.iter().filter(|s| s.id != "preflight").all(|s| s.status != Status::Fail);
        if realm_ok && let Some(b) = &self.binder {
            for e in &w.endpoints {
                match b.bind(e, &rec.spec, w.bind_mode, actor).await {
                    Ok(()) => bound.push(e.clone()),
                    Err(_) => not_bound.push(e.clone()),
                }
            }
        } else {
            not_bound = w.endpoints.clone();
        }
        let report = if bound.is_empty() && !w.endpoints.is_empty() {
            pre
        } else {
            self.diagnose(name, DiagnoseOptions { patient: w.test_patient.clone(), ..Default::default() }).await?
        };
        Ok(FinishOutcome { realm: name.to_string(), bound, not_bound, report })
    }

    /// Abandons a draft (deletes the realm row and its keys).
    pub async fn discard_draft(&self, name: &str) -> OpResult<()> {
        let rec = self.realm(name).await?;
        if WizardState::of(&rec.spec).is_none() {
            return Err(OpError::other(409, "only unfinished drafts can be discarded here; delete the authorization server from its settings"));
        }
        self.admin.delete_realm(&rec.id).await?;
        Ok(())
    }

    // ------------------------------------------------------------ endpoints

    pub async fn endpoint_views(&self) -> Vec<EndpointView> {
        match &self.binder {
            Some(b) => b.views().await.unwrap_or_default(),
            None => vec![],
        }
    }

    pub async fn bind_diff(&self, name: &str, endpoint: &str, mode: BindMode) -> OpResult<BindDiff> {
        let rec = self.realm(name).await?;
        let b = self.binder.as_ref().ok_or_else(|| OpError::other(503, "endpoint management is not available"))?;
        b.diff(endpoint, &rec.spec, mode).await.map_err(|m| OpError::invalid("endpoints", m))
    }

    pub async fn bind(&self, name: &str, endpoint: &str, mode: BindMode, actor: &Actor) -> OpResult<()> {
        let b = self.binder.clone().ok_or_else(|| OpError::other(503, "endpoint management is not available"))?;
        self.update(name, None, |s| {
            if !s.endpoints.iter().any(|e| e == endpoint) {
                s.endpoints.push(endpoint.to_string());
            }
            Ok(())
        })
        .await?;
        let rec = self.realm(name).await?;
        b.bind(endpoint, &rec.spec, mode, actor).await.map_err(|m| OpError::invalid("endpoints", m))
    }

    pub async fn unbind(&self, name: &str, endpoint: &str, actor: &Actor) -> OpResult<()> {
        let rec = self.realm(name).await?;
        if let Some(b) = &self.binder {
            b.unbind(endpoint, &rec.spec, actor).await.map_err(|m| OpError::invalid("endpoints", m))?;
        }
        self.update(name, None, |s| {
            s.endpoints.retain(|e| e != endpoint);
            Ok(())
        })
        .await?;
        Ok(())
    }

    // ------------------------------------------------------------ diagnostic

    pub fn history(&self, name: &str) -> Vec<Report> {
        self.history
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(name)
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn last_report(&self, name: &str) -> Option<Report> {
        self.history.lock().unwrap_or_else(|p| p.into_inner()).get(name).and_then(|q| q.front().cloned())
    }

    fn remember(&self, name: &str, r: &Report) {
        let mut h = self.history.lock().unwrap_or_else(|p| p.into_inner());
        let q = h.entry(name.to_string()).or_default();
        q.push_front(r.clone());
        q.truncate(HISTORY);
    }

    /// The configuration checks only (instant).
    pub async fn preflight(&self, name: &str, app: Option<&AppProbe>) -> OpResult<Report> {
        let rec = self.realm(name).await?;
        let clients = self.visible_clients(&rec.id).await?;
        let keys = self.admin.list_keys(&rec.id).await?;
        let eps = self.endpoint_views().await;
        let pre = PreflightInput {
            realm: &rec.spec,
            public_base: self.public_base(),
            clients: &clients,
            keys: &keys,
            endpoints: &eps,
            app,
            now: Utc::now(),
        };
        Ok(diagnostic::preflight_only(&pre))
    }

    async fn visible_clients(&self, realm_id: &str) -> OpResult<Vec<ClientSpec>> {
        Ok(self
            .admin
            .list_clients(realm_id)
            .await?
            .into_iter()
            .map(|c| c.spec)
            .filter(|c| !c.client_id.starts_with(DIAGNOSTIC_PREFIX))
            .collect())
    }

    /// Full run: preflight, throw-away fixture, steps 1–10, cleanup.
    pub async fn diagnose(&self, name: &str, opts: DiagnoseOptions) -> OpResult<Report> {
        let rec = self.realm(name).await?;
        let realm = &rec.spec;
        let clients = self.visible_clients(&rec.id).await?;
        let keys = self.admin.list_keys(&rec.id).await?;
        let all_eps = self.endpoint_views().await;
        let pre = PreflightInput {
            realm,
            public_base: self.public_base(),
            clients: &clients,
            keys: &keys,
            endpoints: &all_eps,
            app: opts.app.as_ref(),
            now: Utc::now(),
        };
        let issuer = self.issuer(realm);
        let wanted: Vec<String> = opts.endpoints_override.clone().unwrap_or_else(|| realm.endpoints.clone());
        let endpoints: Vec<EndpointView> = wanted
            .iter()
            .filter_map(|n| all_eps.iter().find(|e| &e.name == n).cloned())
            .collect();
        let fixture = self.create_fixture(&rec, &endpoints, opts.patient.as_deref()).await;
        let report = match fixture {
            Ok((fx, cleanup)) => {
                let mut cfg = RunConfig::new(realm.name.as_str(), &issuer, fx);
                cfg.fetch_via = self.fetch_via.clone();
                cfg.endpoints = endpoints;
                cfg.features = realm.features.clone();
                cfg.app = opts.app.clone();
                cfg.timeout = self.timeout;
                cfg.audience_only = opts.audience_only;
                let r = diagnostic::diagnose(&pre, cfg).await;
                self.cleanup_fixture(&rec.id, cleanup).await;
                r
            }
            Err(e) => {
                let mut r = diagnostic::preflight_only(&pre);
                let mut s = diagnostic::Step::new(1, "fixture", "Test identities");
                s.push(
                    diagnostic::Check::fail("fixture.create", "fixture-failed", format!("Could not create the throw-away test app/user: {e}"))
                        .param("detail", e.to_string())
                        .field(e.field.clone().map(|f| format!("realm.{f}")).unwrap_or_else(|| "realm".into()))
                        .fix("The realm's capabilities do not allow a test app (public PKCE client or Backend Services). Enable at least one launch mode and client type."),
                );
                s.finish();
                r.steps.push(s);
                r.finish();
                r
            }
        };
        self.remember(name, &report);
        Ok(report)
    }

    async fn create_fixture(&self, rec: &RealmRecord, endpoints: &[EndpointView], patient: Option<&str>) -> OpResult<(Fixture, (Vec<String>, Option<String>))> {
        let f = &rec.spec.features;
        let tag = rand_suffix();
        let mut clients = vec![];
        let user_scopes = patient.is_some();
        let mut scopes: Vec<&str> = vec![];
        if f.openid_connect {
            scopes.extend(["openid", "fhirUser"]);
        }
        if f.offline_access {
            scopes.push("offline_access");
        }
        if user_scopes && f.permission_patient {
            scopes.extend(["launch/patient", "patient/*.rs"]);
        } else if f.permission_user {
            scopes.push("user/*.rs");
        } else if f.permission_patient {
            scopes.extend(["launch/patient", "patient/*.rs"]);
        }
        let public_id = format!("{DIAGNOSTIC_PREFIX}app-{tag}");
        let interactive = (f.launch_standalone || f.launch_ehr) && f.client_public;
        if interactive {
            let mut grants = vec![GrantType::AuthorizationCode];
            if f.offline_access {
                grants.push(GrantType::RefreshToken);
            }
            let spec = ClientSpec {
                client_id: public_id.clone(),
                name: "Diagnostic (temporary)".into(),
                auth: ClientAuth::None,
                grant_types: grants,
                redirect_uris: vec![DIAGNOSTIC_REDIRECT.into()],
                post_logout_redirect_uris: vec![],
                launch_types: vec![if f.launch_standalone { LaunchType::Standalone } else { LaunchType::Ehr }],
                scopes: ScopePolicy { allowed: scopes.iter().map(|s| s.to_string()).collect(), accept_v1: false },
                trusted: false,
                require_dpop: false,
                require_par: false,
                backchannel_logout_uri: None,
                access_token_ttl_secs: None,
                disabled: false,
            };
            let _: CreatedClient = self.admin.create_client(&rec.id, spec).await?;
            clients.push(public_id.clone());
        }
        let mut backend = None;
        if f.backend_services && f.client_confidential_asymmetric {
            let key = SigningKey::generate(SigningAlg::ES256, RsaBits::B2048)?;
            let id = format!("{DIAGNOSTIC_PREFIX}backend-{tag}");
            let spec = ClientSpec {
                client_id: id.clone(),
                name: "Diagnostic backend (temporary)".into(),
                auth: ClientAuth::PrivateKeyJwt { jwks_uri: None, jwks: Some(json!({"keys": [key.public_jwk()]})), algs: vec![SigningAlg::ES256] },
                grant_types: vec![GrantType::ClientCredentials],
                redirect_uris: vec![],
                post_logout_redirect_uris: vec![],
                launch_types: vec![LaunchType::Backend],
                scopes: ScopePolicy { allowed: vec!["system/*.rs".into()], accept_v1: false },
                trusted: false,
                require_dpop: false,
                require_par: false,
                backchannel_logout_uri: None,
                access_token_ttl_secs: None,
                disabled: false,
            };
            if let Err(e) = self.admin.create_client(&rec.id, spec).await {
                for c in &clients {
                    let _ = self.admin.delete_client(&rec.id, c).await;
                }
                return Err(e.into());
            }
            clients.push(id.clone());
            backend = Some((id, Arc::new(key)));
        }
        if clients.is_empty() {
            return Err(OpError::invalid("features", "no client type the diagnostic can use is enabled"));
        }
        let username = format!("{DIAGNOSTIC_PREFIX}{tag}");
        let password = format!("Diag-{}-{}", helios_idp::util::random_b64(18), rand_suffix());
        let mut user_id = None;
        if interactive {
            let links = match (patient, endpoints.first()) {
                (Some(p), Some(e)) => vec![FhirLink { endpoint: e.name.clone(), reference: format!("Patient/{p}"), is_fhir_user: true, accessible_patients: vec![] }],
                _ => vec![],
            };
            let user = RealmUser {
                id: String::new(),
                username: username.clone(),
                kind: if patient.is_some() { UserKind::Patient } else { UserKind::Practitioner },
                display_name: Some("Diagnostic user (temporary)".into()),
                email: None,
                email_verified: false,
                links,
                external_subject: None,
                disabled: false,
            };
            match self.admin.create_user(&rec.id, user, Some(&password), false).await {
                Ok(u) => user_id = Some(u.id),
                Err(e) => {
                    for c in &clients {
                        let _ = self.admin.delete_client(&rec.id, c).await;
                    }
                    return Err(e.into());
                }
            }
        }
        let fx = Fixture {
            public_client_id: if interactive { public_id } else { String::new() },
            redirect_uri: DIAGNOSTIC_REDIRECT.into(),
            backend_client_id: backend.as_ref().map(|b| b.0.clone()),
            backend_key: backend.map(|b| b.1),
            backend_scope: "system/*.rs".into(),
            username,
            password,
            patient: patient.map(String::from),
            scopes: scopes.join(" "),
        };
        Ok((fx, (clients, user_id)))
    }

    async fn cleanup_fixture(&self, realm_id: &str, (clients, user): (Vec<String>, Option<String>)) {
        if let Some(u) = user
            && let Err(e) = self.admin.delete_user(realm_id, &u).await
        {
            tracing::warn!(error = %e, "idp diagnostic: could not delete the test user");
        }
        for c in clients {
            if let Err(e) = self.admin.delete_client(realm_id, &c).await {
                tracing::warn!(error = %e, client = %c, "idp diagnostic: could not delete the test client");
            }
        }
    }
}

/// Summary row of the realm list.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RealmSummary {
    pub name: String,
    pub display_name: String,
    pub issuer: String,
    pub enabled: bool,
    pub draft: Option<WizardState>,
    pub endpoints: Vec<String>,
    pub clients: usize,
    pub users: usize,
    /// Age in days of the oldest active key.
    pub key_age_days: Option<i64>,
    pub last_status: Option<Status>,
}

impl IdpConsole {
    pub async fn summaries(&self) -> OpResult<Vec<RealmSummary>> {
        let mut out = vec![];
        for r in self.realms().await? {
            let clients = self.visible_clients(&r.id).await.map(|c| c.len()).unwrap_or(0);
            let users = self
                .admin
                .list_users(&r.id)
                .await
                .map(|u| u.iter().filter(|u| !u.username.starts_with(DIAGNOSTIC_PREFIX)).count())
                .unwrap_or(0);
            let now = Utc::now();
            let key_age_days = self
                .admin
                .list_keys(&r.id)
                .await
                .ok()
                .and_then(|ks| {
                    ks.iter()
                        .filter(|k| k.status == helios_idp::keys::KeyStatus::Active)
                        .map(|k| (now - k.activate_at.unwrap_or(k.created_at)).num_days())
                        .max()
                });
            out.push(RealmSummary {
                name: r.spec.name.as_str().to_string(),
                display_name: r.spec.display_name.clone(),
                issuer: self.issuer(&r.spec),
                enabled: r.spec.enabled,
                draft: WizardState::of(&r.spec),
                endpoints: r.spec.endpoints.clone(),
                clients,
                users,
                key_age_days,
                last_status: self.last_report(r.spec.name.as_str()).map(|x| x.status),
            });
        }
        Ok(out)
    }
}

/// JSON helper for API errors.
pub fn error_json(e: &OpError) -> Value {
    json!({"error": e.message, "field": e.field, "status": e.status})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_produce_valid_realms() {
        for t in Template::ALL {
            let mut s = RealmSpec::new(RealmName::parse("x").unwrap(), "X");
            t.apply(&mut s);
            s.validate().unwrap_or_else(|e| panic!("{t:?}: {e}"));
        }
        let mut s = RealmSpec::new(RealmName::parse("x").unwrap(), "X");
        Template::Backend.apply(&mut s);
        assert!(s.features.backend_services && !s.features.launch_standalone);
    }

    #[test]
    fn wizard_state_roundtrips_in_branding() {
        let mut s = RealmSpec::new(RealmName::parse("x").unwrap(), "X");
        assert!(WizardState::of(&s).is_none());
        let w = WizardState { draft: true, step: 3, template: Some(Template::Patient), endpoints: vec!["ep".into()], ..Default::default() };
        w.store(&mut s);
        assert_eq!(WizardState::of(&s), Some(w));
        WizardState::clear(&mut s);
        assert!(WizardState::of(&s).is_none());
    }
}
