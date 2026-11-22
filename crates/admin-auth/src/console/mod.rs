//! Web console sign-in for the FHIR server manager (design
//! `docs/design/fhir-server-manager.md` §7, package P3; feature `console`).
//!
//! Three modes ([`ConsoleAuthMode`]): `none` (today's open console; visitors
//! get `anonymousRole`), `oidc` (Authorization Code + PKCE against any
//! OpenID Connect IdP, roles mapped from token claims) and `local` (built-in
//! accounts with argon2id passwords). In `oidc` and `local` **every** `/ui`
//! page needs a session. Roles are `viewer < editor < admin`, global with
//! optional per-endpoint bindings. The `HFS_ADMIN_PASSWORD` administrator is
//! the break-glass account in every mode (`/ui/login?local=1`) unless
//! `breakGlass` is off.
//!
//! [`ConsoleAuth`] is the single runtime: settings (database + environment
//! pins), server-side sessions (`login_sessions`, `kind='console'`), the
//! lockout (`admin_login_failures`, scope `HFS-CONSOLE`), local accounts and
//! the OIDC client. The products install it as a request extension
//! (`Extension<Arc<ConsoleAuth>>`, outermost) — then the `/ui` gate
//! ([`guard::gate`]), the admin API guard ([`crate::guard`]) and the console
//! admin API ([`api`]) all use it. Without the extension every layer behaves
//! exactly as before (no console sign-in).
//!
//! CSRF (design §7.3): the session cookie is `SameSite=Strict; HttpOnly`;
//! every state-changing cookie-authenticated request must carry a same-site
//! `Origin`/`Referer` **and** the session's synchronizer token — header
//! `X-CSRF-Token` (htmx, `fetch`) or form field `_csrf`. The token is also
//! readable by the page's own script from the `hfs_csrf` cookie
//! (`SameSite=Strict`, not `HttpOnly`); the server compares against the
//! session's stored token, never against the cookie.

pub mod access;
pub mod api;
pub mod audit;
pub mod guard;
pub mod memory;
pub mod oidc;
pub mod password;
pub mod sessions;
pub mod settings;

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, HeaderValue};
use chrono::Utc;
use helios_endpoints::console::{
    ConsoleAuthMode, ConsoleAuthSettings, ConsolePrincipal, ConsoleRole, ConsoleSessionResolver,
    ConsoleStore, ConsoleStoreError, ConsoleUser, ConsoleVia,
};
use helios_endpoints::{OutboundPolicy, SecretInput, SecretKeyRing};
use sha2::{Digest, Sha256};

use crate::auth::{request_is_https, same_origin_with};
use crate::client::{IpNet, client_ip};
use crate::settings::CookieSecure;
use crate::store::AdminSessionStore;
use crate::{AdminGate, LoginOutcome};

pub use access::Requirement;
pub use audit::{ConsoleAuditAction, ConsoleAuditEvent, ConsoleAuditHook};
pub use password::PolicyViolation;
pub use sessions::{ConsoleSession, ConsoleSessionStore, MemorySessionStore};
pub use settings::{EnvBootstrap, SettingsError};

/// Session cookie (`HttpOnly; SameSite=Strict`).
pub const SESSION_COOKIE: &str = "hfs_console";
/// The session's CSRF token for the page's own script (not `HttpOnly`).
pub const CSRF_COOKIE: &str = "hfs_csrf";
/// Pending OIDC login (`SameSite=Lax`: the IdP returns cross-site).
pub const PENDING_COOKIE: &str = "hfs_console_login";
/// Request header carrying the CSRF token.
pub const CSRF_HEADER: &str = "x-csrf-token";
/// Form field carrying the CSRF token.
pub const CSRF_FIELD: &str = "_csrf";
/// Lockout scope in `admin_login_failures`.
pub const LOCKOUT_SCOPE: &str = "HFS-CONSOLE";
/// How long a pending OIDC login may wait for the callback.
pub const PENDING_TTL_SECS: i64 = 600;
/// How often a used session reports its last use to the store.
const TOUCH_EVERY: Duration = Duration::from_secs(30);

pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Hex SHA-256 of a token: the only form tokens are stored in.
pub fn digest_hex(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Constant-time string equality.
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    std::hint::black_box(a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y))) == 0
}

/// A cookie value from the request.
pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    helios_auth::cookie_value(headers, name).filter(|v| !v.is_empty())
}

/// Static options of the runtime.
#[derive(Clone, Debug)]
pub struct ConsoleOptions {
    /// Public base URL of the server (the OIDC `redirect_uri` is
    /// `{base}/ui/callback`; its origin is accepted on writes).
    pub public_base_url: String,
    /// `HFS_ADMIN_COOKIE_SECURE`.
    pub cookie_secure: CookieSecure,
    /// `HFS_ADMIN_TRUSTED_PROXIES` (client address for the lockout).
    pub trusted_proxies: Vec<IpNet>,
    /// Origins always accepted on writes (base URL, `HFS_ADMIN_ALLOWED_ORIGINS`).
    pub allowed_origins: Vec<String>,
    /// SSRF policy for console-typed OIDC URLs.
    pub outbound: OutboundPolicy,
    /// `HFS_SECRET_KEYS` (sealing the OIDC client secret).
    pub key_ring: Option<Arc<SecretKeyRing>>,
    /// How long a session/settings snapshot is trusted before the stores
    /// are asked again (cluster revocation bound). Default 10 s.
    pub recheck: Duration,
    /// Whether the stores are shared and durable (PostgreSQL).
    pub persistent: bool,
}

impl Default for ConsoleOptions {
    fn default() -> Self {
        Self {
            public_base_url: String::new(),
            cookie_secure: CookieSecure::Auto,
            trusted_proxies: IpNet::default_trusted(),
            allowed_origins: Vec::new(),
            outbound: OutboundPolicy::strict(),
            key_ring: None,
            recheck: Duration::from_secs(10),
            persistent: false,
        }
    }
}

/// Failures of console operations (mapped to HTTP by the API and pages).
#[derive(Debug, Clone, PartialEq)]
pub enum ConsoleError {
    /// `422`: one or more fields.
    Invalid(Vec<SettingsError>),
    /// `422`: the password policy.
    Policy(PolicyViolation),
    /// `409`/`412`: revision conflict.
    Conflict(u64),
    /// `404`.
    NotFound,
    /// `409`.
    Duplicate(String),
    /// `409`: refused by a rule (last admin, mode switch precondition).
    Forbidden(String),
    /// `503`.
    Unavailable(String),
}

impl std::fmt::Display for ConsoleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(errors) => {
                let parts: Vec<String> = errors
                    .iter()
                    .map(|e| format!("{}: {}", e.field, e.message))
                    .collect();
                f.write_str(&parts.join("; "))
            }
            Self::Policy(p) => write!(f, "{p}"),
            Self::Conflict(r) => write!(f, "changed meanwhile (current revision {r})"),
            Self::NotFound => f.write_str("not found"),
            Self::Duplicate(n) => write!(f, "user name {n:?} is taken"),
            Self::Forbidden(m) | Self::Unavailable(m) => f.write_str(m),
        }
    }
}

impl From<ConsoleStoreError> for ConsoleError {
    fn from(e: ConsoleStoreError) -> Self {
        match e {
            ConsoleStoreError::NotFound => Self::NotFound,
            ConsoleStoreError::Conflict { current } => Self::Conflict(current),
            ConsoleStoreError::DuplicateUsername(n) => Self::Duplicate(n),
            ConsoleStoreError::Forbidden(m) => Self::Forbidden(m),
            ConsoleStoreError::Unavailable(m) => Self::Unavailable(m),
        }
    }
}

fn invalid(field: &'static str, message: impl Into<String>) -> ConsoleError {
    ConsoleError::Invalid(vec![SettingsError {
        field,
        message: message.into(),
    }])
}

/// Outcome of a password sign-in.
#[derive(Debug)]
pub enum LoginResult {
    /// Signed in: send these `Set-Cookie`s; go to the password change first
    /// when `must_change_password`.
    Success {
        /// Cookies.
        cookies: Vec<HeaderValue>,
        /// Forced password change pending.
        must_change_password: bool,
    },
    /// Wrong user name or password (generic on purpose).
    Invalid,
    /// Locked out for this long.
    RateLimited(Duration),
    /// Not posted from this site.
    CrossOrigin,
    /// A store is down.
    Unavailable(String),
}

/// Outcome of the OIDC callback.
#[derive(Debug)]
pub enum OidcOutcome {
    /// Signed in.
    Success {
        /// Cookies (session, CSRF, pending cleared).
        cookies: Vec<HeaderValue>,
        /// Where to go.
        next: String,
    },
    /// A "Test sign-in" round trip completed; the result is recorded in the
    /// admin's session.
    Tested {
        /// Mapped role (`None` = this user would get no access).
        role: Option<ConsoleRole>,
        /// The tested user.
        display: String,
        /// Cookies (pending cleared).
        cookies: Vec<HeaderValue>,
    },
    /// Authenticated, but no role maps (and no default role).
    NoAccess {
        /// Who.
        display: String,
        /// Cookies (pending cleared).
        cookies: Vec<HeaderValue>,
        /// IdP logout, when available.
        end_session: Option<String>,
    },
    /// The flow failed.
    Failed(String),
}

/// What a sign-out needs to send.
#[derive(Debug)]
pub struct LogoutResult {
    /// Cookies clearing the session (and the break-glass session).
    pub cookies: Vec<HeaderValue>,
    /// Where to send the browser (IdP end-session or the sign-in page).
    pub location: String,
}

/// A request's live session.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The principal.
    pub principal: ConsolePrincipal,
    /// The session document.
    pub session: ConsoleSession,
    /// The session key (token digest).
    pub key: String,
}

/// A user an admin creates.
#[derive(Debug, Clone, Default)]
pub struct NewUser {
    /// User name.
    pub username: String,
    /// Display name.
    pub display_name: Option<String>,
    /// E-mail.
    pub email: Option<String>,
    /// Role.
    pub role: Option<ConsoleRole>,
    /// Initial password; `None` = generate a one-time password.
    pub password: Option<String>,
}

/// Changes to a user (fields left `None` keep their value).
#[derive(Debug, Clone, Default)]
pub struct UserPatch {
    /// Display name (`Some(None)` clears).
    pub display_name: Option<Option<String>>,
    /// E-mail (`Some(None)` clears).
    pub email: Option<Option<String>>,
    /// Role.
    pub role: Option<ConsoleRole>,
    /// Disabled.
    pub disabled: Option<bool>,
}

/// How a switch to mode `oidc` proves the configuration works.
#[derive(Debug, Clone)]
pub enum OidcSwitchCheck {
    /// The fingerprint whose "Test sign-in" passed in the admin's session.
    Tested(Option<String>),
    /// Fetch the discovery document now (admin REST API).
    Probe,
}

#[derive(Clone)]
struct Snapshot {
    stored: Option<ConsoleAuthSettings>,
    effective: ConsoleAuthSettings,
    revision: u64,
    loaded: Instant,
}

struct Cached {
    session: ConsoleSession,
    endpoint_roles: BTreeMap<String, ConsoleRole>,
    checked: Instant,
    touched: Instant,
}

/// The console sign-in runtime. See the module docs.
pub struct ConsoleAuth {
    env: EnvBootstrap,
    options: ConsoleOptions,
    store: Arc<dyn ConsoleStore>,
    sessions: Arc<dyn ConsoleSessionStore>,
    lockout: Arc<dyn AdminSessionStore>,
    break_glass: AdminGate,
    snapshot: RwLock<Snapshot>,
    reloading: tokio::sync::Mutex<()>,
    cache: Mutex<HashMap<String, Cached>>,
    oidc: tokio::sync::Mutex<Option<Arc<oidc::OidcRuntime>>>,
    audit_hook: std::sync::OnceLock<audit::SharedAuditHook>,
}

impl std::fmt::Debug for ConsoleAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsoleAuth")
            .field("mode", &self.mode())
            .finish_non_exhaustive()
    }
}

fn read<T>(l: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|p| p.into_inner())
}

impl ConsoleAuth {
    /// Builds the runtime and loads the settings (a store failure leaves
    /// the environment's view in force and is retried on every recheck).
    pub async fn new(
        env: EnvBootstrap,
        store: Arc<dyn ConsoleStore>,
        sessions: Arc<dyn ConsoleSessionStore>,
        lockout: Arc<dyn AdminSessionStore>,
        break_glass: AdminGate,
        options: ConsoleOptions,
    ) -> Arc<Self> {
        for w in &env.warnings {
            tracing::warn!("console sign-in: {w}");
        }
        let effective = env.apply(None);
        let this = Arc::new(Self {
            snapshot: RwLock::new(Snapshot {
                stored: None,
                effective,
                revision: 0,
                loaded: Instant::now() - Duration::from_secs(3600),
            }),
            env,
            options,
            store,
            sessions,
            lockout,
            break_glass,
            reloading: tokio::sync::Mutex::new(()),
            cache: Mutex::new(HashMap::new()),
            oidc: tokio::sync::Mutex::new(None),
            audit_hook: std::sync::OnceLock::new(),
        });
        this.reload().await;
        let s = this.settings();
        tracing::info!(
            mode = ?s.mode,
            break_glass = this.break_glass_available(),
            persistent = this.options.persistent,
            pinned = ?this.env.pinned,
            "console sign-in installed"
        );
        if s.mode != ConsoleAuthMode::None && !this.break_glass_available() {
            tracing::warn!(
                "console sign-in: no break-glass administrator (HFS_ADMIN_PASSWORD unset or \
                 breakGlass off): a broken configuration can only be recovered from the \
                 environment"
            );
        }
        this
    }

    // ------------------------------------------------------------ settings

    /// The environment bootstrap (pins).
    pub fn env(&self) -> &EnvBootstrap {
        &self.env
    }

    /// The static options.
    pub fn options(&self) -> &ConsoleOptions {
        &self.options
    }

    /// The account store.
    pub fn store(&self) -> &Arc<dyn ConsoleStore> {
        &self.store
    }

    /// The break-glass gate (`HFS_ADMIN_PASSWORD`).
    pub fn break_glass_gate(&self) -> &AdminGate {
        &self.break_glass
    }

    /// The effective settings.
    pub fn settings(&self) -> ConsoleAuthSettings {
        read(&self.snapshot).effective.clone()
    }

    /// The stored settings (`None` before the first save) and revision.
    pub fn stored_settings(&self) -> (Option<ConsoleAuthSettings>, u64) {
        let s = read(&self.snapshot);
        (s.stored.clone(), s.revision)
    }

    /// The effective mode.
    pub fn mode(&self) -> ConsoleAuthMode {
        read(&self.snapshot).effective.mode
    }

    /// Whether the break-glass administrator can sign in now.
    pub fn break_glass_available(&self) -> bool {
        self.settings().break_glass && self.break_glass.auth().is_some()
    }

    /// The break-glass user name, when a password is configured.
    pub fn break_glass_user(&self) -> Option<String> {
        self.break_glass.auth().map(|a| a.user().to_string())
    }

    /// Re-reads the settings when the snapshot is older than the recheck
    /// interval (one reader at a time; others keep the current snapshot).
    pub async fn refresh_if_stale(&self) {
        if read(&self.snapshot).loaded.elapsed() < self.options.recheck {
            return;
        }
        if let Ok(_g) = self.reloading.try_lock() {
            self.load_settings().await;
        }
    }

    /// Re-reads the settings now.
    pub async fn reload(&self) {
        let _g = self.reloading.lock().await;
        self.load_settings().await;
    }

    async fn load_settings(&self) {
        match self.store.settings().await {
            Ok(stored) => {
                let (stored, revision) = match stored {
                    Some((s, r)) => (Some(s), r),
                    None => (None, 0),
                };
                let effective = self.env.apply(stored.as_ref());
                let mut snap = self.snapshot.write().unwrap_or_else(|p| p.into_inner());
                *snap = Snapshot {
                    stored,
                    effective,
                    revision,
                    loaded: Instant::now(),
                };
            }
            Err(e) => {
                tracing::warn!(error = %e, "console settings unavailable; keeping the last known");
                let mut snap = self.snapshot.write().unwrap_or_else(|p| p.into_inner());
                snap.loaded = Instant::now();
            }
        }
    }

    /// Origins accepted on cookie-authenticated writes.
    fn allowed_origins(&self) -> Vec<String> {
        let mut v = self.options.allowed_origins.clone();
        v.extend(self.settings().allowed_origins);
        v
    }

    /// Whether the request's `Origin`/`Referer` is this site.
    pub fn same_origin(&self, headers: &HeaderMap) -> bool {
        same_origin_with(headers, &self.allowed_origins())
    }

    /// The client address the lockout keys on.
    pub fn client_ip(&self, headers: &HeaderMap, peer: Option<IpAddr>) -> IpAddr {
        client_ip(headers, peer, &self.options.trusted_proxies)
    }

    fn secure(&self, headers: &HeaderMap) -> bool {
        match self.options.cookie_secure {
            CookieSecure::Always => true,
            CookieSecure::Never => false,
            CookieSecure::Auto => {
                request_is_https(headers) || self.options.public_base_url.starts_with("https://")
            }
        }
    }

    fn cookie_header(name: &str, value: &str, attrs: &str, secure: bool) -> HeaderValue {
        let mut c = format!("{name}={value}; {attrs}");
        if secure {
            c.push_str("; Secure");
        }
        let mut h =
            HeaderValue::from_str(&c).unwrap_or_else(|_| HeaderValue::from_static("x=; Max-Age=0"));
        h.set_sensitive(true);
        h
    }

    fn session_cookies(&self, headers: &HeaderMap, token: &str, csrf: &str) -> Vec<HeaderValue> {
        let secure = self.secure(headers);
        vec![
            Self::cookie_header(
                SESSION_COOKIE,
                token,
                "Path=/; HttpOnly; SameSite=Strict",
                secure,
            ),
            Self::cookie_header(CSRF_COOKIE, csrf, "Path=/; SameSite=Strict", secure),
        ]
    }

    fn clear_cookies(&self, headers: &HeaderMap) -> Vec<HeaderValue> {
        let secure = self.secure(headers);
        vec![
            Self::cookie_header(
                SESSION_COOKIE,
                "",
                "Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
                secure,
            ),
            Self::cookie_header(
                CSRF_COOKIE,
                "",
                "Path=/; SameSite=Strict; Max-Age=0",
                secure,
            ),
        ]
    }

    fn clear_pending_cookie(&self, headers: &HeaderMap) -> HeaderValue {
        Self::cookie_header(
            PENDING_COOKIE,
            "",
            "Path=/ui; HttpOnly; SameSite=Lax; Max-Age=0",
            self.secure(headers),
        )
    }

    // --------------------------------------------------------------- audit

    /// Installs the hook console actions are reported to (FHIR AuditEvents
    /// in HFS). Returns `false` when one was installed already.
    pub fn set_audit_hook(&self, hook: audit::SharedAuditHook) -> bool {
        self.audit_hook.set(hook).is_ok()
    }

    /// Reports a console action to the audit hook (no hook: nothing; the
    /// action is logged by its caller).
    pub async fn audit(&self, event: ConsoleAuditEvent) {
        if let Some(hook) = self.audit_hook.get() {
            hook.record(event).await;
        }
    }

    /// Reports `event` as done by the console session `resolved`.
    pub async fn audit_by(&self, resolved: &Resolved, event: ConsoleAuditEvent) {
        self.audit(
            event
                .via(via_str(resolved.session.via))
                .display(resolved.session.display_name.clone()),
        )
        .await;
    }

    // ------------------------------------------------------------ sessions

    fn expires(&self, s: &ConsoleSession) -> i64 {
        let settings = self.settings();
        (s.last_seen_unix + i64::from(settings.session_idle_minutes) * 60)
            .min(s.created_unix + i64::from(settings.session_absolute_hours) * 3600)
    }

    async fn put_session(&self, key: &str, s: &ConsoleSession) -> Result<(), String> {
        let doc = serde_json::to_value(s).map_err(|e| e.to_string())?;
        self.sessions
            .put(key, sessions::KIND_SESSION, &doc, self.expires(s))
            .await
    }

    async fn create_session(
        &self,
        headers: &HeaderMap,
        peer: Option<IpAddr>,
        mut session: ConsoleSession,
    ) -> Result<(String, Vec<HeaderValue>), String> {
        let token = helios_auth::random_token(32);
        let key = digest_hex(&token);
        let now = unix_now();
        session.csrf = helios_auth::random_token(16);
        session.created_unix = now;
        session.last_seen_unix = now;
        session.client = Some(self.client_ip(headers, peer).to_string());
        self.put_session(&key, &session).await?;
        let cookies = self.session_cookies(headers, &token, &session.csrf);
        tracing::info!(
            subject = %session.subject,
            via = ?session.via,
            role = session.role.as_str(),
            client = session.client.as_deref().unwrap_or(""),
            "console sign-in"
        );
        self.audit(
            ConsoleAuditEvent::new(
                session.subject.clone(),
                ConsoleAuditAction::Execute,
                "console sign-in",
            )
            .via(via_str(session.via))
            .display(session.display_name.clone())
            .detail("role", session.role.as_str())
            .detail("client", session.client.clone().unwrap_or_default()),
        )
        .await;
        Ok((key, cookies))
    }

    async fn end_session(&self, key: &str) {
        self.cache_lock().remove(key);
        if let Err(e) = self.sessions.delete(key, sessions::KIND_SESSION).await {
            tracing::warn!(error = %e, "console session delete failed");
        }
    }

    fn cache_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Cached>> {
        self.cache.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Forgets every cached session of `subject` on this instance, so the
    /// next request re-reads the account (role change, disable).
    pub fn invalidate_subject(&self, subject: &str) {
        self.cache_lock()
            .retain(|_, c| c.session.subject != subject);
    }

    /// Ends every session of `subject` everywhere.
    pub async fn end_sessions_of(&self, subject: &str) {
        self.invalidate_subject(subject);
        if let Err(e) = self.sessions.delete_subject(subject).await {
            tracing::warn!(error = %e, "console session revocation failed");
        }
    }

    fn principal_of(
        session: &ConsoleSession,
        roles: &BTreeMap<String, ConsoleRole>,
    ) -> ConsolePrincipal {
        ConsolePrincipal {
            subject: session.subject.clone(),
            display_name: session.display_name.clone(),
            role: session.role,
            endpoint_roles: roles.clone(),
            via: session.via,
        }
    }

    /// The anonymous visitor of mode `none` (`None` when sign-in is needed).
    pub fn anonymous(&self) -> Option<ConsolePrincipal> {
        let s = self.settings();
        if s.mode != ConsoleAuthMode::None {
            return None;
        }
        s.anonymous_role.map(|role| ConsolePrincipal {
            subject: "anonymous".into(),
            display_name: String::new(),
            role,
            endpoint_roles: BTreeMap::new(),
            via: ConsoleVia::Anonymous,
        })
    }

    /// The live session of a request (refreshing, re-reading the account
    /// and ending it as the rules require).
    pub async fn resolve(&self, headers: &HeaderMap) -> Option<Resolved> {
        let token = cookie(headers, SESSION_COOKIE)?;
        self.resolve_token(&token).await
    }

    async fn resolve_token(&self, token: &str) -> Option<Resolved> {
        let key = digest_hex(token);
        let now = unix_now();
        let settings = self.settings();
        let fresh = {
            let cache = self.cache_lock();
            cache
                .get(&key)
                .filter(|c| c.checked.elapsed() < self.options.recheck)
                .map(|c| (c.session.clone(), c.endpoint_roles.clone(), c.touched))
        };
        let (mut session, roles, touched) = match fresh {
            Some(x) => x,
            None => {
                let doc = match self.sessions.get(&key, sessions::KIND_SESSION).await {
                    Ok(Some(doc)) => doc,
                    Ok(None) => {
                        self.cache_lock().remove(&key);
                        return None;
                    }
                    Err(e) => {
                        // Availability first: keep what this instance knows.
                        tracing::warn!(error = %e, "console session store unavailable");
                        let cache = self.cache_lock();
                        let c = cache.get(&key)?;
                        return Some(Resolved {
                            principal: Self::principal_of(&c.session, &c.endpoint_roles),
                            session: c.session.clone(),
                            key,
                        });
                    }
                };
                let mut session: ConsoleSession = serde_json::from_value(doc).ok()?;
                let cached_touch = self.cache_lock().get(&key).map(|c| c.touched);
                if !self.revalidate(&key, &mut session).await {
                    return None;
                }
                let roles = if session.via == ConsoleVia::BreakGlass {
                    BTreeMap::new()
                } else {
                    self.store
                        .endpoint_roles(&session.subject)
                        .await
                        .unwrap_or_default()
                };
                let touched = cached_touch.unwrap_or_else(Instant::now);
                self.cache_lock().insert(
                    key.clone(),
                    Cached {
                        session: session.clone(),
                        endpoint_roles: roles.clone(),
                        checked: Instant::now(),
                        touched,
                    },
                );
                (session, roles, touched)
            }
        };
        // Lifetimes and mode consistency.
        let idle_ok = now - session.last_seen_unix < i64::from(settings.session_idle_minutes) * 60;
        let abs_ok = now - session.created_unix < i64::from(settings.session_absolute_hours) * 3600;
        let mode_ok = match session.via {
            ConsoleVia::Local => settings.mode == ConsoleAuthMode::Local,
            ConsoleVia::Oidc => settings.mode == ConsoleAuthMode::Oidc,
            ConsoleVia::BreakGlass => self.break_glass_available(),
            ConsoleVia::Anonymous => false,
        };
        if !(idle_ok && abs_ok && mode_ok) {
            self.end_session(&key).await;
            return None;
        }
        session.last_seen_unix = now;
        let touch_due = touched.elapsed() >= TOUCH_EVERY;
        {
            let mut cache = self.cache_lock();
            if let Some(c) = cache.get_mut(&key) {
                c.session.last_seen_unix = now;
                if touch_due {
                    c.touched = Instant::now();
                }
            }
        }
        if touch_due && let Err(e) = self.put_session(&key, &session).await {
            tracing::debug!(error = %e, "console session touch failed");
        }
        Some(Resolved {
            principal: Self::principal_of(&session, &roles),
            session,
            key,
        })
    }

    /// Re-reads what a session depends on: the local account, the IdP
    /// tokens. `false` = the session ended.
    async fn revalidate(&self, key: &str, session: &mut ConsoleSession) -> bool {
        match session.via {
            ConsoleVia::Local => {
                let Some(id) = session.user_id.clone() else {
                    self.end_session(key).await;
                    return false;
                };
                match self.store.user_by_id(&id).await {
                    Ok(Some(u)) if !u.disabled => {
                        if u.revision != session.user_revision {
                            session.role = u.role;
                            session.display_name = display_of(&u);
                            session.must_change_password = u.must_change_password;
                            session.user_revision = u.revision;
                            let _ = self.put_session(key, session).await;
                        }
                        true
                    }
                    Ok(_) => {
                        self.end_session(key).await;
                        false
                    }
                    // Store down: keep the session as it was.
                    Err(_) => true,
                }
            }
            ConsoleVia::Oidc => {
                let Some(tokens) = session.oidc.clone() else {
                    return true;
                };
                if tokens.access_expires_unix > unix_now() + 30 {
                    return true;
                }
                let Some(refresh) = tokens.refresh_token.clone() else {
                    // Nothing to refresh with: the console lifetimes rule.
                    return true;
                };
                let runtime = match self.oidc_runtime().await {
                    Ok(r) => r,
                    Err(_) => return true,
                };
                match runtime.client.refresh(&refresh).await {
                    Ok(t) => {
                        let role = if t.id_claims.is_null() && t.access_claims.is_null() {
                            Some(session.role)
                        } else {
                            oidc::map_role(&t, &runtime.settings)
                        };
                        let Some(role) = role else {
                            self.end_session(key).await;
                            return false;
                        };
                        session.role = role;
                        session.oidc = Some(sessions::OidcSessionTokens {
                            id_token: t.id_token.or(tokens.id_token),
                            refresh_token: t.refresh_token.or(Some(refresh)),
                            access_expires_unix: unix_now() + t.expires_in.unwrap_or(300) as i64,
                        });
                        let _ = self.put_session(key, session).await;
                        true
                    }
                    Err(e) => {
                        tracing::info!(error = %e, subject = %session.subject, "console OIDC refresh failed; session ended");
                        self.end_session(key).await;
                        false
                    }
                }
            }
            _ => true,
        }
    }

    /// Whether `presented` is the session's CSRF token.
    pub fn csrf_ok(session: &ConsoleSession, presented: Option<&str>) -> bool {
        presented.is_some_and(|p| !p.is_empty() && ct_eq(p, &session.csrf))
    }

    // ------------------------------------------------------- password sign-in

    /// The form sign-in: the break-glass administrator in every mode, local
    /// accounts in mode `local`.
    pub async fn login_password(
        &self,
        headers: &HeaderMap,
        peer: Option<IpAddr>,
        username: &str,
        password: &str,
    ) -> LoginResult {
        if !self.same_origin(headers) {
            return LoginResult::CrossOrigin;
        }
        let username = username.trim();
        if let Some(bg) = self.break_glass_user()
            && username == bg
        {
            if !self.break_glass_available() {
                return LoginResult::Invalid;
            }
            return match self
                .break_glass
                .login_shared(headers, peer, username, password)
                .await
            {
                LoginOutcome::Success { set_cookie } => {
                    let session = ConsoleSession {
                        subject: "admin".into(),
                        display_name: username.to_string(),
                        role: ConsoleRole::Admin,
                        via: ConsoleVia::BreakGlass,
                        user_id: None,
                        user_revision: 0,
                        csrf: String::new(),
                        created_unix: 0,
                        last_seen_unix: 0,
                        must_change_password: false,
                        oidc: None,
                        client: None,
                        oidc_test_passed: None,
                    };
                    match self.create_session(headers, peer, session).await {
                        Ok((_, mut cookies)) => {
                            // The legacy admin session too: the FHIR and admin
                            // layers that predate the console accept it.
                            cookies.push(set_cookie);
                            LoginResult::Success {
                                cookies,
                                must_change_password: false,
                            }
                        }
                        Err(e) => LoginResult::Unavailable(e),
                    }
                }
                LoginOutcome::RateLimited(d) => {
                    self.audit(
                        ConsoleAuditEvent::new(
                            username.to_string(),
                            ConsoleAuditAction::Execute,
                            "break-glass sign-in refused: locked out",
                        )
                        .via("break-glass")
                        .failed(),
                    )
                    .await;
                    LoginResult::RateLimited(d)
                }
                LoginOutcome::CrossOrigin => LoginResult::CrossOrigin,
                LoginOutcome::Invalid | LoginOutcome::NotConfigured => {
                    self.audit(
                        ConsoleAuditEvent::new(
                            username.to_string(),
                            ConsoleAuditAction::Execute,
                            "break-glass sign-in failed",
                        )
                        .via("break-glass")
                        .failed(),
                    )
                    .await;
                    LoginResult::Invalid
                }
            };
        }
        if self.mode() != ConsoleAuthMode::Local {
            return LoginResult::Invalid;
        }
        self.login_local(headers, peer, username, password).await
    }

    async fn login_local(
        &self,
        headers: &HeaderMap,
        peer: Option<IpAddr>,
        username: &str,
        password: &str,
    ) -> LoginResult {
        let policy = self.settings().local;
        let ip_key = format!("ip:{}", self.client_ip(headers, peer));
        let user_key = format!("user:{}", username.to_lowercase());
        for k in [&ip_key, &user_key] {
            match self.lockout.locked_until(k).await {
                Ok(Some(until)) => {
                    tracing::warn!(key = %k, "console sign-in refused: locked out");
                    self.audit(
                        ConsoleAuditEvent::new(
                            username.to_string(),
                            ConsoleAuditAction::Execute,
                            "console sign-in refused: locked out",
                        )
                        .via("local")
                        .failed()
                        .detail("client", ip_key.clone()),
                    )
                    .await;
                    return LoginResult::RateLimited(Duration::from_secs(
                        (until - unix_now()).max(1) as u64,
                    ));
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "console lockout store unavailable"),
            }
        }
        let user = match self.store.user_by_name(username).await {
            Ok(u) => u,
            Err(e) => return LoginResult::Unavailable(e.to_string()),
        };
        let hash = user
            .as_ref()
            .and_then(|u| u.password_hash.clone())
            .unwrap_or_else(|| password::dummy_hash().to_string());
        let pw = password.to_string();
        let matches = tokio::task::spawn_blocking(move || password::verify_password(&pw, &hash))
            .await
            .unwrap_or(false);
        let ok_user = user.filter(|u| matches && !u.disabled && u.password_hash.is_some());
        let Some(user) = ok_user else {
            let window = i64::from(policy.lockout_minutes) * 60;
            let max = u32::from(policy.lockout_failures);
            let mut locked = None;
            for k in [&ip_key, &user_key] {
                if let Ok(Some(until)) = self.lockout.record_failure(k, window, max, window).await {
                    locked = Some(until);
                }
            }
            tracing::warn!(client = %ip_key, locked_out = locked.is_some(), "console sign-in failed");
            self.audit(
                ConsoleAuditEvent::new(
                    username.to_string(),
                    ConsoleAuditAction::Execute,
                    "console sign-in failed",
                )
                .via("local")
                .failed()
                .detail("client", ip_key.clone())
                .detail("lockedOut", locked.is_some().to_string()),
            )
            .await;
            return match locked {
                Some(until) => LoginResult::RateLimited(Duration::from_secs(
                    (until - unix_now()).max(1) as u64,
                )),
                None => LoginResult::Invalid,
            };
        };
        let _ = self.lockout.clear_failures(&ip_key).await;
        let _ = self.lockout.clear_failures(&user_key).await;
        let _ = self.store.record_login(&user.id, Utc::now()).await;
        let session = ConsoleSession {
            subject: local_subject(&user.username),
            display_name: display_of(&user),
            role: user.role,
            via: ConsoleVia::Local,
            user_id: Some(user.id.clone()),
            user_revision: user.revision,
            csrf: String::new(),
            created_unix: 0,
            last_seen_unix: 0,
            must_change_password: user.must_change_password,
            oidc: None,
            client: None,
            oidc_test_passed: None,
        };
        match self.create_session(headers, peer, session).await {
            Ok((_, cookies)) => LoginResult::Success {
                cookies,
                must_change_password: user.must_change_password,
            },
            Err(e) => LoginResult::Unavailable(e),
        }
    }

    /// Signs out: ends the console session (and a break-glass session), and
    /// for OIDC sends the browser to the IdP's end-session endpoint.
    pub async fn logout(&self, headers: &HeaderMap) -> LogoutResult {
        let mut cookies = self.clear_cookies(headers);
        let mut location = "/ui/login?signed_out=1".to_string();
        if let Some(resolved) = self.resolve(headers).await {
            self.end_session(&resolved.key).await;
            tracing::info!(subject = %resolved.session.subject, "console sign-out");
            self.audit_by(
                &resolved,
                ConsoleAuditEvent::new(
                    resolved.session.subject.clone(),
                    ConsoleAuditAction::Execute,
                    "console sign-out",
                ),
            )
            .await;
            if let Some(tokens) = &resolved.session.oidc
                && let Ok(runtime) = self.oidc_runtime().await
                && let Some(url) = runtime.client.end_session_url(
                    &format!(
                        "{}/ui/login?signed_out=1",
                        self.options.public_base_url.trim_end_matches('/')
                    ),
                    tokens.id_token.as_deref(),
                )
            {
                location = url;
            }
        } else if let Some(token) = cookie(headers, SESSION_COOKIE) {
            self.end_session(&digest_hex(&token)).await;
        }
        if let Some(auth) = self.break_glass.auth()
            && auth.session_user(headers).is_some()
        {
            cookies.push(auth.logout_shared(headers).await);
        }
        LogoutResult { cookies, location }
    }

    // ---------------------------------------------------------------- OIDC

    /// The OIDC client for the effective configuration (built on first use
    /// and whenever the configuration changes).
    pub async fn oidc_runtime(&self) -> Result<Arc<oidc::OidcRuntime>, String> {
        let settings = self.settings();
        let o = settings
            .oidc
            .clone()
            .ok_or_else(|| "no OIDC client is configured".to_string())?;
        let env_secret = self.env.client_secret.is_some();
        let fp = oidc::fingerprint(&o, env_secret);
        let mut slot = self.oidc.lock().await;
        if let Some(r) = slot.as_ref()
            && r.fingerprint == fp
        {
            return Ok(r.clone());
        }
        let secret = match (&self.env.client_secret, &o.client_secret) {
            (Some(s), _) => Some(s.expose().to_string()),
            (None, Some(sealed)) => {
                let ring = self
                    .options
                    .key_ring
                    .as_ref()
                    .ok_or("the OIDC client secret is sealed but HFS_SECRET_KEYS is not set")?;
                let plain = ring
                    .open(sealed, settings::CLIENT_SECRET_AAD)
                    .map_err(|e| format!("cannot open the OIDC client secret: {e}"))?;
                Some(
                    plain
                        .expose_str()
                        .ok_or("the OIDC client secret is not text")?
                        .to_string(),
                )
            }
            (None, None) => None,
        };
        // URLs from the environment are operator-trusted; console-typed ones
        // pass the outbound policy (D-13).
        let policy = if self.env.is_pinned(settings::field::OIDC_ISSUER) {
            None
        } else {
            Some(&self.options.outbound)
        };
        let redirect = format!(
            "{}/ui/callback",
            self.options.public_base_url.trim_end_matches('/')
        );
        let secure = self.options.public_base_url.starts_with("https://")
            || matches!(self.options.cookie_secure, CookieSecure::Always);
        let runtime =
            Arc::new(oidc::build(&o, secret, env_secret, &redirect, secure, policy).await?);
        *slot = Some(runtime.clone());
        Ok(runtime)
    }

    /// The fingerprint of the effective OIDC configuration.
    pub fn oidc_fingerprint(&self) -> Option<String> {
        self.settings()
            .oidc
            .map(|o| oidc::fingerprint(&o, self.env.client_secret.is_some()))
    }

    /// Starts an OIDC login (or, with `test_session`, a "Test sign-in" of the
    /// current OIDC configuration for that admin session). Returns the
    /// pending cookie and the IdP authorize URL.
    pub async fn begin_oidc(
        &self,
        headers: &HeaderMap,
        next: &str,
        test_session: Option<&str>,
    ) -> Result<(HeaderValue, String), String> {
        if test_session.is_none() && self.mode() != ConsoleAuthMode::Oidc {
            return Err("OIDC sign-in is not enabled".into());
        }
        let runtime = self.oidc_runtime().await?;
        let pending_id = helios_auth::random_token(32);
        let pending = sessions::PendingLogin {
            state: helios_auth::random_token(32),
            verifier: helios_auth::random_token(64),
            nonce: helios_auth::random_token(32),
            next: crate::safe_next(Some(next), "/ui", "/ui"),
            started_unix: unix_now(),
            test_of: test_session.map(|_| runtime.fingerprint.clone()),
            test_session: test_session.map(str::to_string),
        };
        let url = runtime.client.authorize_url(
            &pending.state,
            &helios_auth::code_challenge_s256(&pending.verifier),
            &pending.nonce,
        );
        let doc = serde_json::to_value(&pending).map_err(|e| e.to_string())?;
        self.sessions
            .put(
                &digest_hex(&pending_id),
                sessions::KIND_PENDING,
                &doc,
                unix_now() + PENDING_TTL_SECS,
            )
            .await?;
        let cookie = Self::cookie_header(
            PENDING_COOKIE,
            &pending_id,
            &format!("Path=/ui; HttpOnly; SameSite=Lax; Max-Age={PENDING_TTL_SECS}"),
            self.secure(headers),
        );
        Ok((cookie, url))
    }

    /// Completes an OIDC login from the callback query.
    pub async fn complete_oidc(
        &self,
        headers: &HeaderMap,
        peer: Option<IpAddr>,
        code: Option<&str>,
        state: Option<&str>,
        error: Option<&str>,
    ) -> OidcOutcome {
        let clear = self.clear_pending_cookie(headers);
        if let Some(e) = error {
            return OidcOutcome::Failed(format!("the identity provider refused the sign-in: {e}"));
        }
        let (Some(code), Some(state)) = (code, state) else {
            return OidcOutcome::Failed("the callback carried no code or state".into());
        };
        let Some(pending_id) = cookie(headers, PENDING_COOKIE) else {
            return OidcOutcome::Failed(
                "no sign-in is pending in this browser; start again".into(),
            );
        };
        let key = digest_hex(&pending_id);
        let doc = match self.sessions.get(&key, sessions::KIND_PENDING).await {
            Ok(Some(d)) => d,
            Ok(None) => return OidcOutcome::Failed("the sign-in expired; start again".into()),
            Err(e) => return OidcOutcome::Failed(format!("session store unavailable: {e}")),
        };
        // Exactly once: the callback whose delete removed the row proceeds.
        match self.sessions.delete(&key, sessions::KIND_PENDING).await {
            Ok(true) => {}
            _ => return OidcOutcome::Failed("this sign-in was already completed".into()),
        }
        let Ok(pending) = serde_json::from_value::<sessions::PendingLogin>(doc) else {
            return OidcOutcome::Failed("unreadable pending sign-in".into());
        };
        if unix_now() - pending.started_unix > PENDING_TTL_SECS {
            return OidcOutcome::Failed("the sign-in expired; start again".into());
        }
        if !ct_eq(&pending.state, state) {
            return OidcOutcome::Failed("state mismatch".into());
        }
        let runtime = match self.oidc_runtime().await {
            Ok(r) => r,
            Err(e) => return OidcOutcome::Failed(e),
        };
        if let Some(fp) = &pending.test_of
            && *fp != runtime.fingerprint
        {
            return OidcOutcome::Failed(
                "the OIDC configuration changed during the test; test again".into(),
            );
        }
        let tokens = match runtime.client.exchange_code(code, &pending.verifier).await {
            Ok(t) => t,
            Err(e) => return OidcOutcome::Failed(e.to_string()),
        };
        if let Err(e) = helios_auth::check_id_claims(
            &tokens.id_claims,
            &runtime.issuer,
            &runtime.settings.client_id,
            &pending.nonce,
            unix_now(),
            60,
        ) {
            return OidcOutcome::Failed(e.to_string());
        }
        let principal = match tokens.principal() {
            Ok(p) => p,
            Err(e) => return OidcOutcome::Failed(e.to_string()),
        };
        let role = oidc::map_role(&tokens, &runtime.settings);
        let who = principal.display().to_string();
        if let (Some(fp), Some(admin_key)) = (&pending.test_of, &pending.test_session) {
            if let Ok(Some(doc)) = self.sessions.get(admin_key, sessions::KIND_SESSION).await
                && let Ok(mut s) = serde_json::from_value::<ConsoleSession>(doc)
            {
                s.oidc_test_passed = Some(fp.clone());
                let _ = self.put_session(admin_key, &s).await;
                if let Some(c) = self.cache_lock().get_mut(admin_key) {
                    c.session.oidc_test_passed = Some(fp.clone());
                }
            }
            tracing::info!(user = %who, role = ?role, "console OIDC test sign-in passed");
            return OidcOutcome::Tested {
                role,
                display: who,
                cookies: vec![clear],
            };
        }
        if self.mode() != ConsoleAuthMode::Oidc {
            return OidcOutcome::Failed("OIDC sign-in is not enabled".into());
        }
        let end_session = runtime.client.end_session_url(
            &format!(
                "{}/ui/login?signed_out=1",
                self.options.public_base_url.trim_end_matches('/')
            ),
            tokens.id_token.as_deref(),
        );
        let Some(role) = role else {
            tracing::warn!(user = %who, "console OIDC sign-in: no role maps; refused");
            self.audit(
                ConsoleAuditEvent::new(
                    who.clone(),
                    ConsoleAuditAction::Execute,
                    "console OIDC sign-in refused: no role maps",
                )
                .via("oidc")
                .failed(),
            )
            .await;
            return OidcOutcome::NoAccess {
                display: who,
                cookies: vec![clear],
                end_session,
            };
        };
        let session = ConsoleSession {
            subject: format!("oidc:{}|{}", principal.issuer, principal.subject),
            display_name: who,
            role,
            via: ConsoleVia::Oidc,
            user_id: None,
            user_revision: 0,
            csrf: String::new(),
            created_unix: 0,
            last_seen_unix: 0,
            must_change_password: false,
            oidc: Some(sessions::OidcSessionTokens {
                id_token: tokens.id_token.clone(),
                refresh_token: tokens.refresh_token.clone(),
                access_expires_unix: unix_now() + tokens.expires_in.unwrap_or(300) as i64,
            }),
            client: None,
            oidc_test_passed: None,
        };
        match self.create_session(headers, peer, session).await {
            Ok((_, mut cookies)) => {
                cookies.push(clear);
                OidcOutcome::Success {
                    cookies,
                    next: pending.next,
                }
            }
            Err(e) => OidcOutcome::Failed(e),
        }
    }

    // ------------------------------------------------------------ accounts

    fn check_username(&self, username: &str) -> Result<(), ConsoleError> {
        let ok = !username.is_empty()
            && username.len() <= 64
            && username
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'));
        if !ok {
            return Err(invalid(
                "username",
                "1–64 characters: letters, digits, . _ - @",
            ));
        }
        let reserved = self
            .break_glass_user()
            .unwrap_or_else(|| crate::DEFAULT_USER.to_string());
        if username.eq_ignore_ascii_case(&reserved) || username.eq_ignore_ascii_case("anonymous") {
            return Err(invalid(
                "username",
                "this name is reserved for the break-glass administrator",
            ));
        }
        Ok(())
    }

    /// Lists accounts.
    pub async fn list_users(&self) -> Result<Vec<ConsoleUser>, ConsoleError> {
        Ok(self.store.list_users().await?)
    }

    /// Creates an account. Returns it and the generated one-time password
    /// when none was given. Accounts created by an admin must change their
    /// password at first sign-in.
    pub async fn create_user(
        &self,
        actor: &str,
        new: NewUser,
    ) -> Result<(ConsoleUser, Option<String>), ConsoleError> {
        let username = new.username.trim().to_string();
        self.check_username(&username)?;
        let min = self.settings().local.min_password_length;
        let (pw, generated) = match new.password.filter(|p| !p.is_empty()) {
            Some(p) => {
                password::check_policy(&p, &username, min).map_err(ConsoleError::Policy)?;
                (p, None)
            }
            None => {
                let g = password::generate_password();
                (g.clone(), Some(g))
            }
        };
        let hash = hash_blocking(pw).await;
        let now = Utc::now();
        let user = self
            .store
            .create_user(ConsoleUser {
                id: String::new(),
                username,
                display_name: clean(new.display_name),
                email: clean(new.email),
                role: new.role.unwrap_or(ConsoleRole::Viewer),
                disabled: false,
                must_change_password: true,
                password_hash: Some(hash),
                revision: 0,
                created_at: now,
                updated_at: now,
                last_login_at: None,
            })
            .await?;
        tracing::info!(actor, user = %user.username, role = user.role.as_str(), "console user created");
        self.audit(
            ConsoleAuditEvent::new(actor, ConsoleAuditAction::Create, "console user created")
                .entity("ConsoleUser", user.username.clone())
                .detail("role", user.role.as_str()),
        )
        .await;
        Ok((user, generated))
    }

    /// Changes an account's profile, role or disabled flag (CAS). Disabling
    /// ends its sessions; a role change reaches its sessions on their next
    /// request here and within the recheck interval elsewhere.
    pub async fn update_user(
        &self,
        actor: &str,
        id: &str,
        patch: UserPatch,
        expected_revision: u64,
    ) -> Result<ConsoleUser, ConsoleError> {
        let mut user = self
            .store
            .user_by_id(id)
            .await?
            .ok_or(ConsoleError::NotFound)?;
        if let Some(v) = patch.display_name {
            user.display_name = clean(v);
        }
        if let Some(v) = patch.email {
            user.email = clean(v);
        }
        if let Some(r) = patch.role {
            user.role = r;
        }
        if let Some(d) = patch.disabled {
            user.disabled = d;
        }
        user.password_hash = None;
        let updated = self.store.update_user(user, expected_revision).await?;
        let subject = local_subject(&updated.username);
        if updated.disabled {
            self.end_sessions_of(&subject).await;
        } else {
            self.invalidate_subject(&subject);
        }
        tracing::info!(actor, user = %updated.username, role = updated.role.as_str(), disabled = updated.disabled, "console user updated");
        self.audit(
            ConsoleAuditEvent::new(actor, ConsoleAuditAction::Update, "console user updated")
                .entity("ConsoleUser", updated.username.clone())
                .detail("role", updated.role.as_str())
                .detail("disabled", updated.disabled.to_string())
                .detail("revision", updated.revision.to_string()),
        )
        .await;
        Ok(updated)
    }

    /// Sets a new one-time password (shown once to the admin), forces a
    /// change at next sign-in and ends the account's sessions.
    pub async fn reset_password(&self, actor: &str, id: &str) -> Result<String, ConsoleError> {
        let mut user = self
            .store
            .user_by_id(id)
            .await?
            .ok_or(ConsoleError::NotFound)?;
        let generated = password::generate_password();
        let revision = user.revision;
        user.password_hash = Some(hash_blocking(generated.clone()).await);
        user.must_change_password = true;
        let updated = self.store.update_user(user, revision).await?;
        self.end_sessions_of(&local_subject(&updated.username))
            .await;
        tracing::info!(actor, user = %updated.username, "console user password reset");
        self.audit(
            ConsoleAuditEvent::new(
                actor,
                ConsoleAuditAction::Update,
                "console user password reset",
            )
            .entity("ConsoleUser", updated.username.clone()),
        )
        .await;
        Ok(generated)
    }

    /// A signed-in local user changes their own password.
    pub async fn change_own_password(
        &self,
        resolved: &Resolved,
        current: &str,
        new: &str,
    ) -> Result<(), ConsoleError> {
        let id = resolved.session.user_id.clone().ok_or_else(|| {
            ConsoleError::Forbidden("only local accounts have a password here".into())
        })?;
        let mut user = self
            .store
            .user_by_id(&id)
            .await?
            .ok_or(ConsoleError::NotFound)?;
        let hash = user.password_hash.clone().unwrap_or_default();
        let cur = current.to_string();
        if !tokio::task::spawn_blocking(move || password::verify_password(&cur, &hash))
            .await
            .unwrap_or(false)
        {
            return Err(invalid("currentPassword", "the current password is wrong"));
        }
        if current == new {
            return Err(invalid("newPassword", "choose a different password"));
        }
        password::check_policy(
            new,
            &user.username,
            self.settings().local.min_password_length,
        )
        .map_err(ConsoleError::Policy)?;
        let revision = user.revision;
        user.password_hash = Some(hash_blocking(new.to_string()).await);
        user.must_change_password = false;
        let updated = self.store.update_user(user, revision).await?;
        // Other sessions of the account end; this one continues.
        let subject = local_subject(&updated.username);
        if let Ok(all) = self.sessions.list(sessions::KIND_SESSION).await {
            for (key, doc) in all {
                if key != resolved.key
                    && doc.get("subject").and_then(|s| s.as_str()) == Some(subject.as_str())
                {
                    self.end_session(&key).await;
                }
            }
        }
        let mut s = resolved.session.clone();
        s.must_change_password = false;
        s.user_revision = updated.revision;
        let _ = self.put_session(&resolved.key, &s).await;
        self.cache_lock().remove(&resolved.key);
        tracing::info!(user = %updated.username, "console user changed their password");
        self.audit_by(
            resolved,
            ConsoleAuditEvent::new(
                resolved.session.subject.clone(),
                ConsoleAuditAction::Update,
                "console user changed their password",
            )
            .entity("ConsoleUser", updated.username.clone()),
        )
        .await;
        Ok(())
    }

    /// Deletes an account (last-admin rule) and ends its sessions.
    pub async fn delete_user(&self, actor: &str, id: &str) -> Result<(), ConsoleError> {
        let user = self
            .store
            .user_by_id(id)
            .await?
            .ok_or(ConsoleError::NotFound)?;
        self.store.delete_user(id).await?;
        self.end_sessions_of(&local_subject(&user.username)).await;
        tracing::info!(actor, user = %user.username, "console user deleted");
        self.audit(
            ConsoleAuditEvent::new(actor, ConsoleAuditAction::Delete, "console user deleted")
                .entity("ConsoleUser", user.username.clone()),
        )
        .await;
        Ok(())
    }

    /// Sets a per-endpoint role binding (audited; the subject's cached
    /// sessions pick it up on their next request).
    pub async fn set_binding(
        &self,
        actor: &str,
        subject: &str,
        endpoint: &str,
        role: ConsoleRole,
    ) -> Result<(), ConsoleStoreError> {
        self.store
            .set_role_binding(subject, endpoint, role, actor)
            .await?;
        self.invalidate_subject(subject);
        tracing::info!(
            actor,
            subject,
            endpoint,
            role = role.as_str(),
            "console role binding set"
        );
        self.audit(
            ConsoleAuditEvent::new(
                actor,
                ConsoleAuditAction::Update,
                "console role binding set",
            )
            .entity("Endpoint", endpoint)
            .detail("subject", subject)
            .detail("role", role.as_str()),
        )
        .await;
        Ok(())
    }

    /// Removes a per-endpoint role binding (audited).
    pub async fn remove_binding(
        &self,
        actor: &str,
        subject: &str,
        endpoint: &str,
    ) -> Result<(), ConsoleStoreError> {
        self.store.delete_role_binding(subject, endpoint).await?;
        self.invalidate_subject(subject);
        tracing::info!(actor, subject, endpoint, "console role binding removed");
        self.audit(
            ConsoleAuditEvent::new(
                actor,
                ConsoleAuditAction::Delete,
                "console role binding removed",
            )
            .entity("Endpoint", endpoint)
            .detail("subject", subject),
        )
        .await;
        Ok(())
    }

    /// Live sessions `(key, session)`, newest first.
    pub async fn list_sessions(&self) -> Result<Vec<(String, ConsoleSession)>, ConsoleError> {
        let mut v: Vec<(String, ConsoleSession)> = self
            .sessions
            .list(sessions::KIND_SESSION)
            .await
            .map_err(ConsoleError::Unavailable)?
            .into_iter()
            .filter_map(|(k, d)| serde_json::from_value(d).ok().map(|s| (k, s)))
            .collect();
        v.sort_by_key(|(_, s)| std::cmp::Reverse(s.last_seen_unix));
        Ok(v)
    }

    /// Ends one session by key.
    pub async fn revoke_session(&self, actor: &str, key: &str) -> Result<(), ConsoleError> {
        let existed = self
            .sessions
            .delete(key, sessions::KIND_SESSION)
            .await
            .map_err(ConsoleError::Unavailable)?;
        self.cache_lock().remove(key);
        if !existed {
            return Err(ConsoleError::NotFound);
        }
        tracing::info!(actor, "console session revoked");
        self.audit(ConsoleAuditEvent::new(
            actor,
            ConsoleAuditAction::Delete,
            "console session revoked",
        ))
        .await;
        Ok(())
    }

    // ------------------------------------------------------------ settings

    /// Saves console settings (admin). Refuses changes to env-pinned fields,
    /// invalid values, and a mode switch whose precondition fails: `local`
    /// needs an enabled admin account; `oidc` a passed test sign-in (console)
    /// or a reachable discovery document (API).
    pub async fn save_settings(
        &self,
        actor: &str,
        mut proposed: ConsoleAuthSettings,
        client_secret: SecretInput,
        expected_revision: u64,
        check: OidcSwitchCheck,
    ) -> Result<u64, ConsoleError> {
        let pinned = self.env.pinned_changes(&proposed);
        if !pinned.is_empty() {
            return Err(ConsoleError::Invalid(
                pinned
                    .into_iter()
                    .map(|field| SettingsError {
                        field,
                        message: "set by the environment; change it there".into(),
                    })
                    .collect(),
            ));
        }
        let (stored, _) = self.stored_settings();
        let current_secret = stored
            .as_ref()
            .and_then(|s| s.oidc.as_ref())
            .and_then(|o| o.client_secret.clone());
        if let Some(o) = proposed.oidc.as_mut() {
            o.client_secret = client_secret
                .apply(
                    current_secret,
                    self.options.key_ring.as_deref(),
                    settings::CLIENT_SECRET_AAD,
                )
                .map_err(|_| {
                    invalid(
                        settings::field::OIDC_CLIENT_SECRET,
                        "storing a client secret needs HFS_SECRET_KEYS",
                    )
                })?;
        } else if matches!(client_secret, SecretInput::Set(_)) {
            return Err(invalid(
                settings::field::OIDC_CLIENT_SECRET,
                "configure the OIDC client first",
            ));
        }
        let effective = self.env.apply(Some(&proposed));
        let errors = settings::validate(&effective, self.env.allow_anonymous_admin);
        if !errors.is_empty() {
            return Err(ConsoleError::Invalid(errors));
        }
        if effective.mode == ConsoleAuthMode::None && !effective.break_glass {
            return Err(invalid(
                settings::field::BREAK_GLASS,
                "in mode none administrators sign in with the break-glass account; keep it on",
            ));
        }
        let current_mode = self.mode();
        if effective.mode != current_mode && !self.env.is_pinned(settings::field::MODE) {
            match effective.mode {
                ConsoleAuthMode::Local => {
                    let admins = self
                        .store
                        .list_users()
                        .await?
                        .into_iter()
                        .filter(|u| u.role == ConsoleRole::Admin && !u.disabled)
                        .count();
                    if admins == 0 {
                        return Err(ConsoleError::Forbidden(
                            "create an enabled admin account before switching to local sign-in"
                                .into(),
                        ));
                    }
                }
                ConsoleAuthMode::Oidc => {
                    let o = effective.oidc.clone().ok_or_else(|| {
                        invalid(
                            settings::field::OIDC_ISSUER,
                            "configure the OIDC client first",
                        )
                    })?;
                    let fp = oidc::fingerprint(&o, self.env.client_secret.is_some());
                    match check {
                        OidcSwitchCheck::Tested(passed) => {
                            if passed.as_deref() != Some(fp.as_str()) {
                                return Err(ConsoleError::Forbidden(
                                    "save the OIDC settings, run \"Test sign-in\" successfully, \
                                     then switch the mode"
                                        .into(),
                                ));
                            }
                        }
                        OidcSwitchCheck::Probe => {
                            self.probe_oidc(&o).await.map_err(|e| {
                                ConsoleError::Forbidden(format!("OIDC discovery failed: {e}"))
                            })?;
                        }
                    }
                }
                ConsoleAuthMode::None => {}
            }
        }
        let rev = self
            .store
            .save_settings(&proposed, expected_revision, actor)
            .await?;
        self.reload().await;
        tracing::info!(actor, mode = ?self.mode(), revision = rev, "console settings saved");
        self.audit(
            ConsoleAuditEvent::new(actor, ConsoleAuditAction::Update, "console settings saved")
                .entity("ConsoleSettings", "console")
                .detail("mode", format!("{:?}", self.mode()).to_lowercase())
                .detail("revision", rev.to_string()),
        )
        .await;
        Ok(rev)
    }

    /// Re-seals the stored OIDC client secret under the active key of
    /// `HFS_SECRET_KEYS` (for `POST /admin/secrets/$rewrap`, P4's
    /// `RewrapParticipant`). Returns how many secrets were re-sealed.
    pub async fn rewrap_secrets(&self, actor: &str) -> Result<usize, ConsoleError> {
        let Some(ring) = self.options.key_ring.clone() else {
            return Ok(0);
        };
        self.reload().await;
        let (stored, revision) = self.stored_settings();
        let Some(mut stored) = stored else {
            return Ok(0);
        };
        let Some(sealed) = stored.oidc.as_ref().and_then(|o| o.client_secret.clone()) else {
            return Ok(0);
        };
        if !ring.needs_rewrap(&sealed) {
            return Ok(0);
        }
        let rewrapped = ring
            .rewrap(&sealed, settings::CLIENT_SECRET_AAD)
            .map_err(|e| {
                ConsoleError::Unavailable(format!("cannot re-seal the OIDC client secret: {e}"))
            })?;
        if let Some(o) = stored.oidc.as_mut() {
            o.client_secret = Some(rewrapped);
        }
        self.store.save_settings(&stored, revision, actor).await?;
        self.reload().await;
        tracing::info!(actor, "console OIDC client secret re-sealed");
        self.audit(
            ConsoleAuditEvent::new(
                actor,
                ConsoleAuditAction::Update,
                "console OIDC client secret re-sealed",
            )
            .entity("ConsoleSettings", "console"),
        )
        .await;
        Ok(1)
    }

    /// The console principal of an admitted admin API request and whether
    /// it may make it — what P4's `AdminAuthorizer` delegates to: `None` when
    /// the request carries no console session (other credentials decide).
    pub async fn authorize_admin_request(
        &self,
        method: &axum::http::Method,
        path: &str,
        headers: &HeaderMap,
    ) -> Option<Result<ConsolePrincipal, ConsolePrincipal>> {
        let resolved = self.resolve(headers).await?;
        let p = resolved.principal;
        Some(if access::admin_api_allows(&p, method, path) {
            Ok(p)
        } else {
            Err(p)
        })
    }

    /// Fetches the discovery document of `o` (through the outbound policy
    /// unless the issuer is env-pinned). Returns the issuer it states.
    pub async fn probe_oidc(
        &self,
        o: &helios_endpoints::console::OidcClientSettings,
    ) -> Result<String, String> {
        let policy = if self.env.is_pinned(settings::field::OIDC_ISSUER) {
            None
        } else {
            Some(&self.options.outbound)
        };
        let discovery = format!(
            "{}/.well-known/openid-configuration",
            o.issuer.trim_end_matches('/')
        );
        let http = oidc::http_client(policy, &[&discovery]).await?;
        let e = helios_auth::discover_with(
            &http,
            &o.issuer,
            o.authorization_endpoint.clone(),
            o.token_endpoint.clone(),
            o.end_session_endpoint.clone(),
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(e.issuer)
    }
}

fn clean(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// `local:{lower-cased username}`.
pub fn local_subject(username: &str) -> String {
    format!("local:{}", username.to_lowercase())
}

/// `ConsoleVia` as the audit `via` detail.
fn via_str(via: ConsoleVia) -> &'static str {
    match via {
        ConsoleVia::Anonymous => "anonymous",
        ConsoleVia::Local => "local",
        ConsoleVia::Oidc => "oidc",
        ConsoleVia::BreakGlass => "break-glass",
    }
}

fn display_of(u: &ConsoleUser) -> String {
    u.display_name.clone().unwrap_or_else(|| u.username.clone())
}

async fn hash_blocking(pw: String) -> String {
    tokio::task::spawn_blocking(move || password::hash_password(&pw))
        .await
        .unwrap_or_else(|_| password::hash_password("unreachable"))
}

#[async_trait::async_trait]
impl ConsoleSessionResolver for ConsoleAuth {
    async fn resolve(&self, cookie_header: &str) -> Option<ConsolePrincipal> {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_str(cookie_header).ok()?,
        );
        ConsoleAuth::resolve(self, &headers)
            .await
            .map(|r| r.principal)
    }

    async fn resolve_with_csrf(
        &self,
        cookie_header: &str,
    ) -> Option<(ConsolePrincipal, Option<String>)> {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_str(cookie_header).ok()?,
        );
        ConsoleAuth::resolve(self, &headers)
            .await
            .map(|r| (r.principal, Some(r.session.csrf)))
    }
}

/// Test helpers shared by this crate's and the UI's tests.
pub mod testing {
    use super::*;

    /// A runtime over memory stores with break-glass password `bg_password`
    /// (or none), the given environment, and options suitable for tests
    /// (`recheck` zero so every request re-reads the stores).
    pub async fn runtime(
        env: EnvBootstrap,
        bg_password: Option<&str>,
        options: ConsoleOptions,
    ) -> (Arc<ConsoleAuth>, Arc<memory::MemoryConsoleStore>) {
        let store = Arc::new(memory::MemoryConsoleStore::new());
        let mut admin = crate::AdminSettings::default();
        if let Some(p) = bg_password {
            admin = admin.with_password(p);
        }
        let gate = AdminGate::configure(
            "HFS",
            admin,
            true,
            crate::CookieSpec {
                name: "hfs_admin",
                path: "/",
            },
        );
        let rt = ConsoleAuth::new(
            env,
            store.clone(),
            Arc::new(MemorySessionStore::default()),
            Arc::new(crate::store::testing::MemoryStore::default()),
            gate,
            options,
        )
        .await;
        (rt, store)
    }

    /// A live session for `subject` with `role`, as if signed in `via`
    /// (local sessions name a user id that must exist for them to survive a
    /// recheck: pass it as `user_id`). Returns the `Cookie` header value and
    /// the CSRF token.
    pub async fn session(
        rt: &ConsoleAuth,
        subject: &str,
        role: ConsoleRole,
        via: ConsoleVia,
        user_id: Option<String>,
    ) -> (String, String) {
        let user_revision = match &user_id {
            Some(id) => rt
                .store
                .user_by_id(id)
                .await
                .ok()
                .flatten()
                .map(|u| u.revision)
                .unwrap_or(0),
            None => 0,
        };
        let session = ConsoleSession {
            subject: subject.into(),
            display_name: subject.into(),
            role,
            via,
            user_id,
            user_revision,
            csrf: String::new(),
            created_unix: 0,
            last_seen_unix: 0,
            must_change_password: false,
            oidc: None,
            client: None,
            oidc_test_passed: None,
        };
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::HOST,
            HeaderValue::from_static("hfs.test"),
        );
        let (_, cookies) = rt
            .create_session(&h, None, session)
            .await
            .expect("memory store");
        let pairs: Vec<String> = cookies
            .iter()
            .filter_map(|c| c.to_str().ok())
            .filter_map(|c| c.split(';').next())
            .map(str::to_string)
            .collect();
        let csrf = pairs
            .iter()
            .find_map(|p| p.strip_prefix(&format!("{CSRF_COOKIE}=")))
            .unwrap_or_default()
            .to_string();
        (pairs.join("; "), csrf)
    }

    /// An environment from pairs.
    pub fn env(pairs: &[(&str, &str)]) -> EnvBootstrap {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        EnvBootstrap::from_lookup(|k| map.get(k).cloned())
    }
}

#[cfg(test)]
mod tests;
