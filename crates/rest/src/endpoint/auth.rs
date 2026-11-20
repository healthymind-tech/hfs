//! Per-endpoint FHIR authentication (FHIR server manager, package P2;
//! `docs/design/fhir-server-manager.md` §6, D-8, D-10).
//!
//! One always-installed layer group ([`auth_layer`]) replaces the
//! conditional `auth_middleware` + `authz_middleware` pair. For every request
//! it works out the [`EffectiveAuth`] of the endpoint the request addresses
//! (the [`ResolvedEndpoint`] the dispatcher stored in the request extensions)
//! and runs the existing authentication / authorization functions
//! ([`authenticate`], [`authorize`]) with that endpoint's state:
//!
//! | Request | Effective auth |
//! |---|---|
//! | no `ResolvedEndpoint` (manager off) | the environment (`HFS_AUTH_*`) exactly as before |
//! | endpoint with `auth: null` | the environment, exactly as before (the endpoint's rate limit and console access still apply) |
//! | endpoint with `auth.mode = off` | pass-through: no credential needed, tokens ignored |
//! | endpoint with `write-protect` / `full` | the endpoint's issuer, JWKS, audience, algorithms, scope policy, admin-credential toggle |
//!
//! Verifiers are shared through [`VerifierCache`]: endpoints naming the same
//! issuer and JWKS share one fetcher, each checks its own audience. Compiled
//! states are cached per `(endpoint id, revision)`; a configuration change
//! (new snapshot) therefore takes effect on the next request, and
//! [`AuthHook::watch`] prunes fetchers, rate-limit buckets and discovery
//! entries of endpoints that are gone.
//!
//! Credentials, in order: `Authorization: Bearer <JWT>` → the endpoint's
//! verifier; Basic / non-JWT bearer → the administrator credential when the
//! endpoint accepts it; no `Authorization` + a console session cookie → the
//! console principal ([`ConsoleSessionResolver`], scopes from
//! [`ConsoleRole::data_scopes`] and the endpoint's `consoleAccess`; writes
//! must be same-origin); else anonymous (write-protect reads only).
//!
//! [`ConsoleRole::data_scopes`]: helios_endpoints::ConsoleRole::data_scopes

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use helios_audit::{AuditAction, AuditEventBuilder, AuditSink, ExclusionFilter};
use helios_auth::{
    AuthConfig, AuthMode, IssuerDiscovery, OutboundGuard, SmartConfiguration, VerifierCache,
    VerifierKey,
};
use helios_endpoints::model::{ConsoleAccess, ManagedBy, ScopePolicy};
use helios_endpoints::{
    ConsoleSessionResolver, Endpoint, EndpointRegistry, EndpointSnapshot, OutboundPolicy,
    ResolvedEndpoint,
};
use tracing::{debug, warn};

use crate::audit::AttributeToPrincipal;
use crate::middleware::auth::{
    AuthMiddlewareState, EndpointScopePolicy, authenticate_request, authorize_request,
    console_principal, forbidden_response, run_authorized,
};
use crate::middleware::client_rate_limit::{
    self as rate_limit, ClientRateLimiter, RateBudget, RateLimitConfig,
};

/// What every per-endpoint auth state shares with the environment's: the
/// audit sink and its settings, the administrator gate, the tenant routing
/// mode.
#[derive(Clone)]
pub struct AuthBase {
    /// Where authentication events go.
    pub audit_sink: Arc<dyn AuditSink>,
    /// `AuditEvent.source.observer`.
    pub audit_source_observer: String,
    /// Paths not audited.
    pub audit_exclusion_filter: ExclusionFilter,
    /// The administrator sign-in (`HFS_ADMIN_PASSWORD`), when configured.
    pub admin: Option<helios_admin_auth::AdminGate>,
    /// URL-path tenant routing (`url_path`/`both`).
    pub tenant_url_routing: bool,
}

impl AuthBase {
    /// The parts of an existing (environment) state.
    pub fn from_state(state: &AuthMiddlewareState) -> Self {
        Self {
            audit_sink: Arc::clone(&state.audit_sink),
            audit_source_observer: state.audit_source_observer.clone(),
            audit_exclusion_filter: state.audit_exclusion_filter.clone(),
            admin: state.admin.clone(),
            tenant_url_routing: state.tenant_url_routing,
        }
    }

    /// A base with this audit sink, no exclusions, no administrator gate.
    pub fn new(audit_sink: Arc<dyn AuditSink>, audit_source_observer: impl Into<String>) -> Self {
        Self {
            audit_sink,
            audit_source_observer: audit_source_observer.into(),
            audit_exclusion_filter: ExclusionFilter::new(vec![]),
            admin: None,
            tenant_url_routing: false,
        }
    }
}

impl std::fmt::Debug for AuthBase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthBase")
            .field("audit_source_observer", &self.audit_source_observer)
            .field("admin", &self.admin.is_some())
            .field("tenant_url_routing", &self.tenant_url_routing)
            .finish()
    }
}

/// Process-wide console session resolver, installed by the console sign-in
/// wiring (package P3) when it starts; hooks without their own use it.
static CONSOLE_RESOLVER: OnceLock<Arc<dyn ConsoleSessionResolver>> = OnceLock::new();
/// Process-wide administrator gate for deployments where the environment's
/// auth is off (so no environment state carries it) but endpoints turn auth
/// on and accept the administrator credential.
static ADMIN_GATE: OnceLock<helios_admin_auth::AdminGate> = OnceLock::new();

/// Installs the console session resolver every [`AuthHook`] falls back to.
/// Returns `false` when one was installed already.
pub fn install_console_resolver(resolver: Arc<dyn ConsoleSessionResolver>) -> bool {
    CONSOLE_RESOLVER.set(resolver).is_ok()
}

/// Installs the administrator gate endpoints accepting the administrator
/// credential use when the environment state has none. Returns `false` when
/// one was installed already.
pub fn install_admin_gate(gate: helios_admin_auth::AdminGate) -> bool {
    gate.auth().is_some() && ADMIN_GATE.set(gate).is_ok()
}

/// The auth in force for one request (see the module documentation).
pub struct EffectiveAuth {
    /// The endpoint, `None` without the manager (no `ResolvedEndpoint`).
    pub endpoint: Option<Arc<Endpoint>>,
    /// The environment's auth applies (`auth: null`, or no endpoint).
    pub inherited: bool,
    /// The mode in force ([`AuthMode::Off`] = no auth).
    pub mode: AuthMode,
    /// The middleware state the existing auth functions run with; `None`
    /// when the mode is off.
    pub state: Option<Arc<AuthMiddlewareState>>,
    /// The configuration (the environment's when inherited).
    pub config: Arc<AuthConfig>,
    /// The endpoint's scope policy (`None` when inherited: no filtering).
    pub scope_policy: Option<ScopePolicy>,
    /// Console session as a credential here.
    pub console_access: ConsoleAccess,
    /// Rate limits.
    pub rate: RateBudget,
    /// How the endpoint's IdP URLs may be fetched.
    pub guard: OutboundGuard,
    /// The shared verifier key (endpoint-configured auth only).
    pub verifier_key: Option<VerifierKey>,
    discovery: Arc<IssuerDiscovery>,
    revision: (u64, i64),
}

impl std::fmt::Debug for EffectiveAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EffectiveAuth")
            .field("endpoint", &self.endpoint_name())
            .field("inherited", &self.inherited)
            .field("mode", &self.mode)
            .field("console_access", &self.console_access)
            .field("rate", &self.rate)
            .finish()
    }
}

impl EffectiveAuth {
    /// The effective auth a request carries (stored by [`auth_layer`]).
    pub fn of(extensions: &axum::http::Extensions) -> Option<Arc<EffectiveAuth>> {
        extensions.get::<Arc<EffectiveAuth>>().cloned()
    }

    /// Endpoint name (`default` without the manager).
    pub fn endpoint_name(&self) -> &str {
        self.endpoint
            .as_deref()
            .map(Endpoint::name)
            .unwrap_or(helios_endpoints::DEFAULT_ENDPOINT_NAME)
    }

    /// The `/.well-known/smart-configuration` document of an endpoint with
    /// its own auth section: `None` when that section turns auth off (the
    /// endpoint answers `404`). Unset SMART endpoints are filled from the
    /// issuer's OpenID configuration (cached, outbound-policy guarded) when
    /// `smart.discoverFromIssuer` is on. Not meant for inherited auth (the
    /// caller keeps the environment's document).
    pub async fn smart_configuration(&self) -> Option<SmartConfiguration> {
        if self.mode == AuthMode::Off {
            return None;
        }
        let smart = self
            .endpoint
            .as_ref()
            .and_then(|e| e.spec.auth.as_ref())
            .map(|a| &a.smart);
        let discover = smart.is_none_or(|s| s.discover_from_issuer);
        let incomplete = self.config.smart_authorize_endpoint.is_none()
            || self.config.smart_token_endpoint.is_none()
            || self.config.smart_jwks_url.is_none();
        let discovered = match (&self.config.expected_issuer, discover && incomplete) {
            (Some(issuer), true) => self.discovery.get(issuer, &self.guard).await.ok(),
            _ => None,
        };
        Some(SmartConfiguration::for_endpoint(
            &self.config,
            discovered.as_deref(),
            self.config.smart_end_session_endpoint.as_deref(),
        ))
    }

    /// The CapabilityStatement `rest.security` of an endpoint with its own
    /// auth section: the SMART-on-FHIR service coding and `oauth-uris` when
    /// auth is on and the authorize and token endpoints are known (configured
    /// or discovered), else `cors` only — the same rule the environment's
    /// statement follows (#1441).
    pub async fn capability_security(&self, cors: bool) -> serde_json::Value {
        let mut security = serde_json::json!({
            "cors": cors,
            "description": "This server supports CORS for cross-origin requests"
        });
        let Some(doc) = self.smart_configuration().await else {
            return security;
        };
        let (Some(authorize), Some(token)) = (&doc.authorization_endpoint, &doc.token_endpoint)
        else {
            return security;
        };
        security["service"] = serde_json::json!([{
            "coding": [{
                "system": "http://terminology.hl7.org/CodeSystem/restful-security-service",
                "code": "SMART-on-FHIR",
                "display": "SMART-on-FHIR"
            }],
            "text": "OAuth2 using SMART-on-FHIR profile (see http://docs.smarthealthit.org)"
        }]);
        let mut uris = vec![
            serde_json::json!({"url": "authorize", "valueUri": authorize}),
            serde_json::json!({"url": "token", "valueUri": token}),
        ];
        for (name, value) in [
            ("introspect", &doc.introspection_endpoint),
            ("revoke", &doc.revocation_endpoint),
            ("register", &doc.registration_endpoint),
            ("manage", &doc.management_endpoint),
        ] {
            if let Some(uri) = value {
                uris.push(serde_json::json!({"url": name, "valueUri": uri}));
            }
        }
        security["extension"] = serde_json::json!([{
            "url": "http://fhir-registry.smarthealthit.org/StructureDefinition/oauth-uris",
            "extension": uris
        }]);
        security
    }
}

/// The per-endpoint auth runtime: the environment's state, the shared
/// verifier cache, the compiled per-endpoint states, the rate limiter, the
/// issuer discovery cache and the console resolver.
pub struct AuthHook {
    env_config: Arc<AuthConfig>,
    env_state: Option<Arc<AuthMiddlewareState>>,
    base: AuthBase,
    env_rate: RateLimitConfig,
    limiter: Arc<ClientRateLimiter>,
    verifiers: VerifierCache,
    discovery: Arc<IssuerDiscovery>,
    outbound: Arc<OutboundPolicy>,
    console: RwLock<Option<Arc<dyn ConsoleSessionResolver>>>,
    allowed_origins: Vec<String>,
    compiled: Mutex<HashMap<String, Arc<EffectiveAuth>>>,
    env_effective: Arc<EffectiveAuth>,
}

impl std::fmt::Debug for AuthHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthHook")
            .field("env_mode", &self.env_config.effective_mode())
            .field("verifiers", &self.verifiers)
            .field("compiled", &self.compiled_count())
            .finish()
    }
}

impl AuthHook {
    /// A hook over the environment's auth: `env_config` (`AuthConfig::from_env`
    /// plus discovered endpoints) and its middleware state (`None` when the
    /// environment's auth is off). `base` supplies the audit sink and the
    /// administrator gate to endpoint states. The rate limits default to
    /// `HFS_RATE_LIMIT_*` and the outbound policy to `HFS_OUTBOUND_ALLOW`.
    pub fn new(
        env_config: AuthConfig,
        env_state: Option<Arc<AuthMiddlewareState>>,
        base: AuthBase,
    ) -> Self {
        let env_config = Arc::new(env_config);
        let env_rate = RateLimitConfig::from_env();
        let discovery = Arc::new(IssuerDiscovery::new());
        let mut hook = Self {
            env_effective: Arc::new(EffectiveAuth {
                endpoint: None,
                inherited: true,
                mode: AuthMode::Off,
                state: None,
                config: Arc::clone(&env_config),
                scope_policy: None,
                console_access: ConsoleAccess::RoleBased,
                rate: RateBudget::of(&env_rate),
                guard: OutboundGuard::Trusted,
                verifier_key: None,
                discovery: Arc::clone(&discovery),
                revision: (0, 0),
            }),
            limiter: ClientRateLimiter::new(env_rate.clone()),
            env_rate,
            env_config,
            env_state,
            base,
            verifiers: VerifierCache::new(),
            discovery,
            outbound: Arc::new(OutboundPolicy::from_env()),
            console: RwLock::new(None),
            allowed_origins: Vec::new(),
            compiled: Mutex::new(HashMap::new()),
        };
        hook.rebuild_env_effective();
        hook
    }

    fn rebuild_env_effective(&mut self) {
        self.env_effective = Arc::new(EffectiveAuth {
            endpoint: None,
            inherited: true,
            mode: if self.env_state.is_some() {
                self.env_config.effective_mode()
            } else {
                AuthMode::Off
            },
            state: self.env_state.clone(),
            config: Arc::clone(&self.env_config),
            scope_policy: None,
            console_access: ConsoleAccess::RoleBased,
            rate: RateBudget::of(&self.env_rate),
            guard: OutboundGuard::Trusted,
            verifier_key: None,
            discovery: Arc::clone(&self.discovery),
            revision: (0, 0),
        });
    }

    /// Uses `config` as the environment's rate limits (instead of
    /// `HFS_RATE_LIMIT_*`).
    pub fn with_rate_limit(mut self, config: RateLimitConfig) -> Self {
        self.limiter = ClientRateLimiter::new(config.clone());
        self.env_rate = config;
        self.rebuild_env_effective();
        self
    }

    /// Uses `policy` for console-edited endpoints' IdP URLs (instead of
    /// `HFS_OUTBOUND_ALLOW`).
    pub fn with_outbound(mut self, policy: OutboundPolicy) -> Self {
        self.outbound = Arc::new(policy);
        self
    }

    /// Accepts `origin`'s scheme://host[:port] (e.g. `HFS_BASE_URL`) on
    /// cookie-authenticated (console) writes, besides the request's `Host`.
    pub fn with_allowed_origin(mut self, url: &str) -> Self {
        if let Some(origin) = origin_of(url) {
            self.allowed_origins.push(origin);
        }
        self
    }

    /// Uses `resolver` for console sessions (instead of the process-wide one).
    pub fn with_console_resolver(self, resolver: Arc<dyn ConsoleSessionResolver>) -> Self {
        self.set_console_resolver(resolver);
        self
    }

    /// Sets the console session resolver at runtime.
    pub fn set_console_resolver(&self, resolver: Arc<dyn ConsoleSessionResolver>) {
        *self.console.write().unwrap_or_else(|p| p.into_inner()) = Some(resolver);
    }

    fn console_resolver(&self) -> Option<Arc<dyn ConsoleSessionResolver>> {
        self.console
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .or_else(|| CONSOLE_RESOLVER.get().cloned())
    }

    fn admin_gate(&self) -> Option<helios_admin_auth::AdminGate> {
        self.base
            .admin
            .clone()
            .filter(|g| g.auth().is_some())
            .or_else(|| ADMIN_GATE.get().cloned())
    }

    /// The shared verifier cache.
    pub fn verifiers(&self) -> &VerifierCache {
        &self.verifiers
    }

    /// The rate limiter.
    pub fn limiter(&self) -> &Arc<ClientRateLimiter> {
        &self.limiter
    }

    /// Number of compiled endpoint states.
    pub fn compiled_count(&self) -> usize {
        self.compiled
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    /// The `$test-auth` probe for an endpoint (package P4): fetches through
    /// the outbound policy unless the endpoint is operator-managed.
    pub fn probe(&self, managed_by: ManagedBy) -> helios_auth::EndpointAuthProbe {
        helios_auth::EndpointAuthProbe::new(self.guard_for(managed_by))
    }

    fn guard_for(&self, managed_by: ManagedBy) -> OutboundGuard {
        match managed_by {
            ManagedBy::Env => OutboundGuard::Trusted,
            ManagedBy::Ui => OutboundGuard::Policy(Arc::clone(&self.outbound)),
        }
    }

    /// The effective auth of a request addressing `resolved` (`None` = no
    /// manager: the environment's auth).
    pub fn effective(&self, resolved: Option<&ResolvedEndpoint>) -> Arc<EffectiveAuth> {
        match resolved {
            None => Arc::clone(&self.env_effective),
            Some(r) => self.effective_for(&r.endpoint),
        }
    }

    /// The effective auth of `endpoint`, compiled once per revision.
    pub fn effective_for(&self, endpoint: &Arc<Endpoint>) -> Arc<EffectiveAuth> {
        let stamp = (endpoint.revision, endpoint.updated_at.timestamp_micros());
        if let Some(hit) = self
            .compiled
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&endpoint.id)
            .filter(|c| c.revision == stamp)
        {
            return Arc::clone(hit);
        }
        let compiled = Arc::new(self.compile(endpoint, stamp));
        let replaced = {
            let mut map = self.compiled.lock().unwrap_or_else(|p| p.into_inner());
            map.insert(endpoint.id.clone(), Arc::clone(&compiled))
                .is_some()
        };
        if replaced {
            // A changed endpoint may have left a fetcher nobody uses.
            self.retain_verifiers_of_compiled();
        }
        compiled
    }

    fn compile(&self, endpoint: &Arc<Endpoint>, stamp: (u64, i64)) -> EffectiveAuth {
        let spec = &endpoint.spec;
        let prefix = RateBudget::endpoint_prefix(&endpoint.id);
        let rate = match &spec.rate_limit {
            Some(r) => RateBudget {
                key_prefix: prefix,
                anonymous_per_minute: r.anonymous_per_minute,
                authenticated_per_minute: r.authenticated_per_minute,
                burst_seconds: if r.burst_seconds == 0 {
                    10
                } else {
                    r.burst_seconds
                },
            },
            None => RateBudget {
                key_prefix: prefix,
                ..RateBudget::of(&self.env_rate)
            },
        };
        let guard = self.guard_for(spec.managed_by);
        match &spec.auth {
            None => EffectiveAuth {
                endpoint: Some(Arc::clone(endpoint)),
                inherited: true,
                mode: self.env_effective.mode,
                state: self.env_state.clone(),
                config: Arc::clone(&self.env_config),
                scope_policy: None,
                console_access: spec.console_access,
                rate,
                guard: OutboundGuard::Trusted,
                verifier_key: None,
                discovery: Arc::clone(&self.discovery),
                revision: stamp,
            },
            Some(auth) => {
                let config = Arc::new(AuthConfig::from_endpoint(auth, &self.env_config));
                let mode = config.effective_mode();
                let state = config.enabled.then(|| {
                    Arc::new(AuthMiddlewareState {
                        provider: self.verifiers.provider(&config, &guard),
                        config: Arc::clone(&config),
                        audit_sink: Arc::clone(&self.base.audit_sink),
                        audit_source_observer: self.base.audit_source_observer.clone(),
                        audit_exclusion_filter: self.base.audit_exclusion_filter.clone(),
                        tenant_url_routing: self.base.tenant_url_routing,
                        // The web UI login (#1449) holds tokens of the
                        // environment's IdP; it is not a credential here.
                        sessions: None,
                        admin: if auth.accept_admin_credential {
                            self.admin_gate()
                        } else {
                            None
                        },
                    })
                });
                debug!(
                    endpoint = endpoint.name(),
                    mode = mode.as_str(),
                    "compiled endpoint auth"
                );
                EffectiveAuth {
                    endpoint: Some(Arc::clone(endpoint)),
                    inherited: false,
                    mode,
                    verifier_key: VerifierKey::of(&config, &guard),
                    state,
                    config,
                    scope_policy: Some(auth.scopes.clone()),
                    console_access: spec.console_access,
                    rate,
                    guard,
                    discovery: Arc::clone(&self.discovery),
                    revision: stamp,
                }
            }
        }
    }

    fn retain_verifiers_of_compiled(&self) {
        let live: HashSet<VerifierKey> = self
            .compiled
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .filter_map(|c| c.verifier_key.clone())
            .collect();
        self.verifiers.retain(&live);
    }

    /// Brings the caches in line with `snapshot`: compiles every endpoint
    /// (so new IdPs' keys are fetched before the first request), forgets
    /// endpoints that are gone, and drops their JWKS fetchers, rate-limit
    /// buckets and discovery entries.
    pub fn sync(&self, snapshot: &EndpointSnapshot) {
        let live_ids: HashSet<String> = snapshot.endpoints().iter().map(|e| e.id.clone()).collect();
        self.compiled
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|id, _| live_ids.contains(id));
        let mut issuers = HashSet::new();
        for endpoint in snapshot.endpoints() {
            let effective = self.effective_for(endpoint);
            if let Some(issuer) = &effective.config.expected_issuer
                && !effective.inherited
            {
                issuers.insert(issuer.clone());
            }
        }
        self.retain_verifiers_of_compiled();
        self.limiter.retain_endpoints(&live_ids);
        self.discovery.retain_issuers(&issuers);
    }

    /// Keeps the caches in line with `registry` (call once at startup, with
    /// the manager on): syncs now and on every snapshot change.
    pub fn watch(self: &Arc<Self>, registry: Arc<EndpointRegistry>) {
        let hook = Arc::clone(self);
        let mut changes = registry.subscribe();
        hook.sync(&registry.snapshot());
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                while changes.changed().await.is_ok() {
                    hook.sync(&registry.snapshot());
                }
            });
        }
    }

    /// Whether a cookie-authenticated write comes from this site: `Origin`
    /// (else `Referer`) equal to an allowed origin or naming the request's
    /// `Host`.
    fn same_origin(&self, headers: &HeaderMap) -> bool {
        let origin = headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .filter(|v| *v != "null")
            .and_then(origin_of)
            .or_else(|| {
                headers
                    .get(header::REFERER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(origin_of)
            });
        let Some(origin) = origin else {
            return false;
        };
        if self.allowed_origins.contains(&origin) {
            return true;
        }
        let authority = origin.split_once("://").map(|(_, a)| a).unwrap_or("");
        headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|host| host.trim().eq_ignore_ascii_case(authority))
    }
}

/// `scheme://host[:port]` of a URL, lower-cased, default ports dropped.
fn origin_of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url.trim()).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    Some(match parsed.port() {
        Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
        None => format!("{}://{host}", parsed.scheme()),
    })
}

fn is_safe(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Wraps the FHIR router in the per-endpoint auth layer. One middleware runs,
/// in order: effective-auth resolution → authentication (endpoint verifier,
/// admin credential, console session) → rate limit (per endpoint and client)
/// → principal capture for audit → authorization (scopes, patient
/// compartment). Each auth step is a pass-through when the endpoint's auth is
/// off. (One layer instead of five keeps the request future chain shallow:
/// debug builds overflowed a worker's 2 MiB stack under the dispatcher, the
/// audit layer and the validator with one middleware per step.)
pub fn auth_layer<S>(router: Router<S>, hook: Arc<AuthHook>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.layer(axum::middleware::from_fn_with_state(hook, endpoint_auth))
}

async fn endpoint_auth(
    State(hook): State<Arc<AuthHook>>,
    mut request: Request,
    next: Next,
) -> Response {
    // The request's EffectiveAuth, RateBudget and (for an endpoint with its
    // own auth) EndpointScopePolicy.
    let effective = hook.effective(request.extensions().get::<ResolvedEndpoint>());
    {
        let ext = request.extensions_mut();
        ext.insert(effective.rate.clone());
        if let Some(policy) = &effective.scope_policy {
            ext.insert(EndpointScopePolicy(policy.clone()));
        }
        ext.insert(Arc::clone(&effective));
    }
    let state = effective.state.clone();

    // Authentication.
    if let Some(state) = &state {
        request = match authenticate_endpoint(&hook, &effective, state, request).await {
            Ok(request) => request,
            Err(response) => return response,
        };
    }

    // Rate limit (inside authentication: billed to the OAuth client).
    if let Some(refusal) = rate_limit::check(&hook.limiter, &effective.rate, &request) {
        return refusal;
    }

    // Audit attribution, before authorization so a 403 names its user.
    if let Some(principal) = request.extensions().get::<helios_auth::Principal>() {
        helios_audit::context::set_user(crate::middleware::audit::audit_user(principal));
    }

    // Authorization.
    match &state {
        Some(state) => match authorize_request(state, request).await {
            Ok((request, shared_types)) => run_authorized(request, shared_types, next).await,
            Err(response) => response,
        },
        None => next.run(request).await,
    }
}

/// Authentication of one request: the console session principal when the
/// request carries a console cookie and no `Authorization`, else the
/// endpoint's [`authenticate_request`].
// The `Err` is the response that ends the request, returned at once.
#[allow(clippy::result_large_err)]
async fn authenticate_endpoint(
    hook: &AuthHook,
    effective: &EffectiveAuth,
    state: &AuthMiddlewareState,
    mut request: Request,
) -> Result<Request, Response> {
    if !request.headers().contains_key(header::AUTHORIZATION)
        && effective.console_access != ConsoleAccess::Off
        && !helios_auth::is_cross_site(request.headers())
        && let Some(resolver) = hook.console_resolver()
        && let Some(cookie) = request
            .headers()
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
        && let Some(console) = resolver.resolve(&cookie).await
        && let Some(scopes) = console
            .role_for(effective.endpoint_name())
            .data_scopes(effective.console_access)
    {
        if !is_safe(request.method()) && !hook.same_origin(request.headers()) {
            warn!(sub = %console.subject, "cross-site console write refused");
            return Err(forbidden_response("cross-site request refused"));
        }
        let principal = console_principal(&console, scopes);
        let path = request.uri().path().to_string();
        if !state
            .audit_exclusion_filter
            .is_excluded(&path, request.method().as_str())
        {
            let event = AuditEventBuilder::new(&state.audit_source_observer)
                .action(AuditAction::Execute)
                .outcome("0")
                .outcome_desc("Authenticated with a console session".to_string())
                .principal(&principal)
                .build();
            state.audit_sink.record(event).await;
        }
        debug!(
            sub = %principal.subject,
            endpoint = effective.endpoint_name(),
            "console session principal"
        );
        request
            .extensions_mut()
            .insert(crate::audit::audit_agent(&principal));
        request.extensions_mut().insert(principal);
        return Ok(request);
    }
    authenticate_request(state, request).await
}

/// `404` for the SMART discovery document of an endpoint whose auth is off.
pub(crate) fn smart_discovery_not_found(endpoint: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "resourceType": "OperationOutcome",
            "issue": [{
                "severity": "error",
                "code": "not-found",
                "diagnostics": format!(
                    "endpoint {endpoint} does not use SMART on FHIR authorization"
                )
            }]
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_endpoints::model::{AuthMode as EpMode, EndpointAuth, RateLimitSettings};
    use helios_endpoints::{EndpointName, EndpointSpec};

    fn endpoint(name: &str, auth: Option<EndpointAuth>, revision: u64) -> Arc<Endpoint> {
        let mut spec = EndpointSpec::named(EndpointName::parse(name).unwrap(), name);
        spec.auth = auth;
        Arc::new(Endpoint {
            id: format!("id-{name}"),
            revision,
            spec,
            created_at: chrono::Utc::now(),
            updated_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            created_by: None,
            updated_by: None,
        })
    }

    fn sso(issuer: &str, audience: &str, mode: EpMode) -> EndpointAuth {
        EndpointAuth {
            mode,
            issuer: Some(issuer.into()),
            jwks_url: Some(format!("{issuer}/certs")),
            audience: Some(audience.into()),
            ..EndpointAuth::default()
        }
    }

    fn hook() -> AuthHook {
        AuthHook::new(
            AuthConfig::default(),
            None,
            AuthBase::new(Arc::new(helios_audit::sinks::NullSink), "Device/test"),
        )
        .with_rate_limit(RateLimitConfig::default())
        .with_outbound(OutboundPolicy::parse("127.0.0.1"))
    }

    #[tokio::test]
    async fn inherited_and_off_and_endpoint_auth_compile_as_designed() {
        let h = hook();
        assert_eq!(h.effective(None).mode, AuthMode::Off);
        assert!(h.effective(None).inherited);

        let inherited = h.effective_for(&endpoint("lab", None, 1));
        assert!(inherited.inherited);
        assert!(inherited.state.is_none(), "env auth off: no state");
        assert!(inherited.scope_policy.is_none());

        let off = h.effective_for(&endpoint("open", Some(EndpointAuth::default()), 1));
        assert!(!off.inherited);
        assert_eq!(off.mode, AuthMode::Off);
        assert!(off.state.is_none());
        assert!(off.smart_configuration().await.is_none(), "404 when off");

        let wp = h.effective_for(&endpoint(
            "a",
            Some(sso(
                "http://127.0.0.1:9/realms/a",
                "api-a",
                EpMode::WriteProtect,
            )),
            1,
        ));
        assert_eq!(wp.mode, AuthMode::WriteProtect);
        let state = wp.state.as_ref().unwrap();
        assert_eq!(state.config.expected_audience.as_deref(), Some("api-a"));
        assert!(state.sessions.is_none());
        assert!(wp.scope_policy.is_some());
        assert!(wp.rate.key_prefix.starts_with("ep:id-a|"));
    }

    #[tokio::test]
    async fn endpoints_sharing_an_issuer_share_a_fetcher_and_revisions_recompile() {
        let h = hook();
        let issuer = "http://127.0.0.1:9/realms/shared";
        let a = h.effective_for(&endpoint("a", Some(sso(issuer, "api-a", EpMode::Full)), 1));
        let b = h.effective_for(&endpoint("b", Some(sso(issuer, "api-b", EpMode::Full)), 1));
        assert_eq!(h.verifiers().fetcher_count(), 1);
        assert_eq!(a.verifier_key, b.verifier_key);
        // Cached per revision.
        let again = h.effective_for(&endpoint("a", Some(sso(issuer, "api-a", EpMode::Full)), 1));
        assert!(Arc::ptr_eq(&a, &again));
        // A new revision on another IdP: recompiled, and once no endpoint
        // uses the shared IdP's fetcher any more it goes.
        let other = "http://127.0.0.1:9/realms/other";
        h.effective_for(&endpoint("a", Some(sso(other, "api-a", EpMode::Full)), 2));
        assert_eq!(
            h.verifiers().fetcher_count(),
            2,
            "b still uses the shared one"
        );
        h.effective_for(&endpoint("b", Some(sso(other, "api-b", EpMode::Full)), 2));
        assert_eq!(h.verifiers().fetcher_count(), 1);
    }

    #[tokio::test]
    async fn sync_forgets_endpoints_that_are_gone() {
        let h = hook();
        let mut a = (*endpoint(
            "a",
            Some(sso("http://127.0.0.1:9/realms/a", "x", EpMode::Full)),
            1,
        ))
        .clone();
        a.spec.rate_limit = Some(RateLimitSettings {
            anonymous_per_minute: 1,
            authenticated_per_minute: 0,
            burst_seconds: 0,
        });
        let snap = EndpointSnapshot::new(1, vec![a.clone()]).unwrap();
        h.sync(&snap);
        assert_eq!(h.compiled_count(), 1);
        assert_eq!(h.verifiers().fetcher_count(), 1);
        let eff = h.effective_for(&Arc::new(a));
        assert_eq!(eff.rate.anonymous_per_minute, 1);
        assert_eq!(eff.rate.burst_seconds, 10);
        h.sync(&EndpointSnapshot::new(2, vec![]).unwrap());
        assert_eq!(h.compiled_count(), 0);
        assert_eq!(h.verifiers().fetcher_count(), 0);
    }

    #[test]
    fn origins_compare_by_scheme_host_and_port() {
        assert_eq!(
            origin_of("https://HFS.example/fhir/x").as_deref(),
            Some("https://hfs.example")
        );
        assert_eq!(
            origin_of("http://localhost:8080/").as_deref(),
            Some("http://localhost:8080")
        );
        let h = hook().with_allowed_origin("https://public.example/fhir");
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "internal:8080".parse().unwrap());
        headers.insert(header::ORIGIN, "https://public.example".parse().unwrap());
        assert!(h.same_origin(&headers));
        headers.insert(header::ORIGIN, "http://internal:8080".parse().unwrap());
        assert!(h.same_origin(&headers));
        headers.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert!(!h.same_origin(&headers));
        headers.remove(header::ORIGIN);
        assert!(!h.same_origin(&headers), "neither Origin nor Referer");
        headers.insert(
            header::REFERER,
            "http://internal:8080/ui/x".parse().unwrap(),
        );
        assert!(h.same_origin(&headers));
    }
}
