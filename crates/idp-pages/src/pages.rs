//! The hosted pages and their handlers (design §8.2–§8.6, package I4).
//!
//! Flow of one authorization (`/authorize` → these pages → code):
//!
//! ```text
//! /authorize ──303──► GET /login?ix=hmix_…   (or the SSO session is reused)
//!                     POST /login             → SSO cookie, progress.user
//!                 ──► GET/POST /select-patient (patient picker, if needed)
//!                 ──► GET/POST /consent        (unless trusted / remembered)
//!                 ──► AuthorizationCompleter::complete → 303 to the app
//!                                                (or the form_post page)
//! ```
//!
//! Security rules every step follows:
//! * the interaction is looked up by the digest of its handle, in the
//!   realm of the URL (another realm's handle is unknown here);
//! * after sign-in, every step requires the **same browser's SSO session**
//!   (`progress.sessionHash` = digest of the cookie): a leaked `ix` cannot
//!   be finished in another browser;
//! * every POST passes the CSRF token (bound to browser, form and
//!   interaction) and the Origin check;
//! * nothing the browser sends selects a redirect target: redirects come
//!   from the stored interaction (validated at `/authorize`) or from
//!   `check_logout` (registered URIs only), so there is no open redirect;
//! * errors before the client is trusted (unknown or expired interaction)
//!   render an error page and never redirect.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::extract::{ConnectInfo, Form, FromRequestParts, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, Duration, Utc};
use helios_idp::error::{OAuthError, OAuthErrorCode};
use helios_idp::events::IdpEvent;
use helios_idp::interaction::{
    AuthorizationCompleter, AuthorizationResponse, CompleteError, Decision, PendingAuthorization,
    Prompt, ResponseMode,
};
use helios_idp::launch::LaunchContext;
use helios_idp::logout::{LogoutRequest, check_logout};
use helios_idp::runtime::{IdpRuntime, RealmRuntime};
use helios_idp::store::{Consent, GrantFilter, IdpStore, SsoSession};
use helios_idp::token::{HandleKind, handle_hash, mint_handle};
use helios_idp::user::{RealmUser, UserKind};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::csrf::{self, CsrfKey};
use crate::directory::{PatientDirectory, PatientSummary};
use crate::i18n::I18n;
use crate::lockout::{Attempt, Lockout};
use crate::scopes::{self, ScopeLine};
use crate::security;

/// Static settings of the pages.
#[derive(Debug, Clone)]
pub struct PagesConfig {
    /// Public origin of the IdP (`https://host[:port]`): the Origin check.
    pub public_origin: String,
    /// `Secure` + `__Host-` cookies (on unless the origin is plain http,
    /// e.g. a loopback test stack).
    pub secure_cookies: bool,
    /// Take the client address from `X-Forwarded-For` (behind nginx).
    pub trust_forwarded: bool,
}

/// Everything the handlers need.
#[derive(Clone)]
pub struct Pages {
    pub runtime: Arc<IdpRuntime>,
    pub completer: Arc<dyn AuthorizationCompleter>,
    pub directory: Arc<dyn PatientDirectory>,
    pub lockout: Lockout,
    pub csrf: CsrfKey,
    pub config: PagesConfig,
    hashing: Arc<Semaphore>,
}

impl Pages {
    pub fn new(
        runtime: Arc<IdpRuntime>,
        completer: Arc<dyn AuthorizationCompleter>,
        directory: Arc<dyn PatientDirectory>,
        lockout: Lockout,
        csrf: CsrfKey,
        config: PagesConfig,
    ) -> Self {
        Self {
            runtime,
            completer,
            directory,
            lockout,
            csrf,
            config,
            // Bounds concurrent argon2 work (design §8.6: DoS).
            hashing: Arc::new(Semaphore::new(4)),
        }
    }

    fn store(&self) -> &Arc<dyn IdpStore> {
        self.runtime.store()
    }
}

/// The router: `/idp/{realm}/{login,select-patient,consent,logout,
/// account/apps}` and `/idp/assets/idp.css`.
pub fn router(pages: Pages) -> Router {
    Router::new()
        .route("/idp/assets/idp.css", get(stylesheet))
        .route("/idp/{realm}/login", get(login_page).post(login_submit))
        .route(
            "/idp/{realm}/select-patient",
            get(picker_page).post(picker_submit),
        )
        .route(
            "/idp/{realm}/consent",
            get(consent_page).post(consent_submit),
        )
        .route("/idp/{realm}/logout", get(logout_page).post(logout_submit))
        .route(
            "/idp/{realm}/account/apps",
            get(apps_page).post(apps_revoke),
        )
        .route(
            "/idp/{realm}/account/login",
            axum::routing::post(account_login),
        )
        .with_state(pages)
}

async fn stylesheet() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        include_str!("../assets/idp.css"),
    )
        .into_response()
}

// ------------------------------------------------------------------ shell

/// The shared frame of every page (see `templates/idp/layout.html`).
pub struct Shell {
    pub i18n: I18n,
    pub nonce: String,
    pub theme_attrs: &'static str,
    pub brand_head: String,
    pub theme_boot: String,
    pub toolbar: String,
    pub brand: String,
    pub footer: String,
    pub realm_display: String,
}

const ASSETS: &str = "/ui/assets";

impl Shell {
    fn new(i18n: I18n, headers: &HeaderMap, realm_display: &str, lang_suffix: &str) -> Self {
        let nonce = security::nonce();
        let get = |n: header::HeaderName| headers.get(n).and_then(|v| v.to_str().ok());
        let theme_attrs = helios_ui_chrome::theme_attributes(
            get(header::COOKIE),
            headers
                .get("sec-ch-prefers-color-scheme")
                .and_then(|v| v.to_str().ok()),
        );
        let langs = helios_ui_chrome::lang_links(&i18n.lang(), lang_suffix);
        Self {
            brand_head: helios_ui_chrome::brand_head(ASSETS).unwrap_or_default(),
            theme_boot: security::with_nonce(
                &helios_ui_chrome::theme_boot().unwrap_or_default(),
                &nonce,
            ),
            toolbar: helios_ui_chrome::auth_toolbar(&i18n, &langs).unwrap_or_default(),
            brand: helios_ui_chrome::auth_brand(
                &i18n,
                helios_ui_chrome::AuthBrand {
                    home: "#auth-main",
                    assets: ASSETS,
                    product_key: "idp-product",
                },
            )
            .unwrap_or_default(),
            footer: helios_ui_chrome::auth_footer(&i18n, concat!("v", env!("CARGO_PKG_VERSION")))
                .unwrap_or_default(),
            theme_attrs,
            nonce,
            i18n,
            realm_display: realm_display.to_string(),
        }
    }
}

/// A banner on a page.
pub struct Banner {
    pub kind: &'static str,
    pub text: String,
    pub id: &'static str,
}

impl Banner {
    fn danger(text: String, id: &'static str) -> Self {
        Self {
            kind: "danger",
            text,
            id,
        }
    }
    fn ok(text: String, id: &'static str) -> Self {
        Self {
            kind: "ok",
            text,
            id,
        }
    }
    pub fn role(&self) -> &'static str {
        if matches!(self.kind, "ok" | "info") {
            "status"
        } else {
            "alert"
        }
    }
}

#[derive(Template)]
#[template(path = "idp/login.html")]
struct LoginTemplate {
    shell: Shell,
    lede: String,
    action: String,
    ix: String,
    csrf: String,
    username: String,
    error: Option<Banner>,
    reveal: String,
}

/// One picker row.
pub struct PickerRow {
    pub id: String,
    pub name: String,
    pub meta: String,
}

#[derive(Template)]
#[template(path = "idp/picker.html")]
struct PickerTemplate {
    shell: Shell,
    lede: String,
    action: String,
    ix: String,
    csrf: String,
    searchable: bool,
    query: String,
    patients: Vec<PickerRow>,
    empty_text: String,
    error: Option<Banner>,
}

#[derive(Template)]
#[template(path = "idp/consent.html")]
struct ConsentTemplate {
    shell: Shell,
    title: String,
    lede: String,
    action: String,
    ix: String,
    csrf: String,
    signed_in: String,
    patient: Option<String>,
    scopes: Vec<ScopeLine>,
    has_optional: bool,
    error: Option<Banner>,
}

#[derive(Template)]
#[template(path = "idp/message.html")]
struct MessageTemplate {
    shell: Shell,
    title: String,
    text: String,
    banner: Option<Banner>,
}

#[derive(Template)]
#[template(path = "idp/logout.html")]
struct LogoutTemplate {
    shell: Shell,
    lede: String,
    app_line: String,
    action: String,
    csrf: String,
    carry: Vec<(String, String)>,
    cancel_href: String,
}

/// One connected app.
pub struct AppRow {
    pub client_id: String,
    pub endpoint: String,
    pub name: String,
    pub meta: String,
    pub scopes: String,
}

#[derive(Template)]
#[template(path = "idp/apps.html")]
struct AppsTemplate {
    shell: Shell,
    lede: String,
    action: String,
    csrf: String,
    apps: Vec<AppRow>,
    notice: Option<Banner>,
    logout_href: String,
}

// ---------------------------------------------------------------- cookies

const CSRF_COOKIE: &str = "hmidp-csrf";

fn cookie_name(cfg: &PagesConfig, base: &str) -> String {
    if cfg.secure_cookies {
        format!("__Host-{base}")
    } else {
        base.to_string()
    }
}

fn session_cookie_name(cfg: &PagesConfig, realm: &str) -> String {
    cookie_name(cfg, &format!("hmidp-{realm}"))
}

fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| {
            let (k, v) = kv.trim().split_once('=')?;
            (k == name).then(|| v.trim().to_string())
        })
}

fn set_cookie(
    cfg: &PagesConfig,
    name: &str,
    value: &str,
    same_site: &str,
    max_age: Option<i64>,
) -> String {
    let mut c = format!("{name}={value}; Path=/; HttpOnly; SameSite={same_site}");
    if cfg.secure_cookies {
        c.push_str("; Secure");
    }
    if let Some(m) = max_age {
        c.push_str(&format!("; Max-Age={m}"));
    }
    c
}

/// A response under construction: body + cookies + headers.
struct Page {
    status: StatusCode,
    html: String,
    cookies: Vec<String>,
    nonce: String,
    frame_ancestors: Vec<String>,
}

impl Page {
    fn new(html: String, nonce: String, realm: Option<&RealmRuntime>) -> Self {
        Self {
            status: StatusCode::OK,
            html,
            cookies: vec![],
            nonce,
            frame_ancestors: realm
                .map(|r| security::frame_ancestors(&r.spec.branding))
                .unwrap_or_default(),
        }
    }
    fn status(mut self, s: StatusCode) -> Self {
        self.status = s;
        self
    }
    fn cookies(mut self, c: Vec<String>) -> Self {
        self.cookies.extend(c);
        self
    }
}

impl IntoResponse for Page {
    fn into_response(self) -> Response {
        let mut r = (
            self.status,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            self.html,
        )
            .into_response();
        security::apply(r.headers_mut(), &self.nonce, &self.frame_ancestors, None);
        for c in self.cookies {
            if let Ok(v) = HeaderValue::from_str(&c) {
                r.headers_mut().append(header::SET_COOKIE, v);
            }
        }
        r
    }
}

fn redirect(location: &str, cookies: Vec<String>) -> Response {
    let mut r = StatusCode::SEE_OTHER.into_response();
    if let Ok(v) = HeaderValue::from_str(location) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    security::apply(r.headers_mut(), &security::nonce(), &[], None);
    for c in cookies {
        if let Ok(v) = HeaderValue::from_str(&c) {
            r.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    r
}

// ------------------------------------------------------------- request ctx

/// The caller: the address the rate limiter resolved ([`crate::limits::CallerIp`],
/// trusted proxy aware) or the socket peer.
pub struct Peer(Option<String>);

impl<S: Send + Sync> FromRequestParts<S> for Peer {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _: &S,
    ) -> Result<Self, Self::Rejection> {
        if let Some(crate::limits::CallerIp(ip)) = parts.extensions.get::<crate::limits::CallerIp>()
        {
            return Ok(Peer(Some(ip.clone())));
        }
        Ok(Peer(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip().to_string()),
        ))
    }
}

fn client_ip(cfg: &PagesConfig, headers: &HeaderMap, peer: Option<String>) -> String {
    if let Some(p) = &peer {
        return p.clone();
    }
    if cfg.trust_forwarded
        && let Some(ip) = crate::limits::forwarded_ip(headers)
    {
        return ip;
    }
    "unknown".into()
}

/// What the pages record on the interaction between steps. Not trusted for
/// anything security relevant on its own: `complete` re-validates, and the
/// user binding is checked against the browser's session cookie.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
struct Progress {
    user_id: Option<String>,
    session_hash: Option<String>,
    sid: Option<String>,
    auth_time: Option<i64>,
    amr: Vec<String>,
    patient: Option<String>,
}

impl Progress {
    fn of(p: &PendingAuthorization) -> Self {
        serde_json::from_value(p.progress.clone()).unwrap_or_default()
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct IxQuery {
    #[serde(default)]
    ix: String,
    #[serde(default)]
    lang: Option<String>,
    #[serde(default)]
    q: Option<String>,
}

/// The loaded state of one step.
struct Step {
    realm: Arc<RealmRuntime>,
    pending: PendingAuthorization,
    ix: String,
    i18n: I18n,
    lang_cookie: Option<String>,
    binding: String,
    new_binding: bool,
}

impl Step {
    fn csrf(&self, pages: &Pages, purpose: &str) -> String {
        pages.csrf.token(&self.binding, purpose, &self.ix)
    }
    fn cookies(&self, pages: &Pages) -> Vec<String> {
        let mut c = vec![];
        if self.new_binding {
            c.push(set_cookie(
                &pages.config,
                &cookie_name(&pages.config, CSRF_COOKIE),
                &self.binding,
                "Strict",
                None,
            ));
        }
        if let Some(l) = &self.lang_cookie {
            c.push(l.clone());
        }
        c
    }
    fn shell(&self, headers: &HeaderMap) -> Shell {
        Shell::new(
            self.i18n,
            headers,
            &self.realm.spec.display_name,
            &format!("&ix={}", self.ix),
        )
    }
    fn path(&self, page: &str) -> String {
        format!(
            "/idp/{}/{page}?ix={}",
            self.realm.spec.name.as_str(),
            self.ix
        )
    }
}

fn realm_or_404(pages: &Pages, name: &str) -> Option<Arc<RealmRuntime>> {
    pages.runtime.realm(name).filter(|r| r.spec.enabled)
}

/// An error page (never a redirect).
fn error_page(
    headers: &HeaderMap,
    realm: Option<&RealmRuntime>,
    i18n: I18n,
    status: StatusCode,
    text: String,
) -> Response {
    let display = realm
        .map(|r| r.spec.display_name.clone())
        .unwrap_or_default();
    let shell = Shell::new(i18n, headers, &display, "");
    let nonce = shell.nonce.clone();
    let t = MessageTemplate {
        title: i18n.t("idp-error-title"),
        text: String::new(),
        banner: Some(Banner::danger(text, "idp-error")),
        shell,
    };
    Page::new(t.render().unwrap_or_default(), nonce, realm)
        .status(status)
        .into_response()
}

async fn load_step(
    pages: &Pages,
    realm_name: &str,
    ix: &str,
    lang: Option<&str>,
    headers: &HeaderMap,
) -> Result<Step, Response> {
    let (i18n0, _) = I18n::negotiate(lang, headers, &[]);
    let Some(realm) = realm_or_404(pages, realm_name) else {
        return Err(error_page(
            headers,
            None,
            i18n0,
            StatusCode::NOT_FOUND,
            i18n0.t("idp-error-realm"),
        ));
    };
    let expired = |i: I18n| {
        error_page(
            headers,
            Some(&realm),
            i,
            StatusCode::BAD_REQUEST,
            i.t("idp-error-expired"),
        )
    };
    let Some(hash) = handle_hash(HandleKind::Interaction, ix) else {
        return Err(expired(i18n0));
    };
    let pending = match pages
        .store()
        .get_interaction(&realm.id, &hash, Utc::now())
        .await
    {
        Ok(Some(p)) if p.realm_id == realm.id => p,
        Ok(_) => return Err(expired(i18n0)),
        Err(e) => {
            tracing::error!(error = %e, "idp: interaction lookup failed");
            return Err(error_page(
                headers,
                Some(&realm),
                i18n0,
                StatusCode::SERVICE_UNAVAILABLE,
                i18n0.t("idp-error-generic"),
            ));
        }
    };
    let (i18n, explicit) = I18n::negotiate(lang, headers, &pending.ui_locales);
    let cookie = read_cookie(headers, &cookie_name(&pages.config, CSRF_COOKIE))
        .filter(|v| v.len() >= 32 && v.len() <= 64);
    let (binding, new_binding) = match cookie {
        Some(b) => (b, false),
        None => (csrf::new_binding(), true),
    };
    Ok(Step {
        realm,
        pending,
        ix: ix.to_string(),
        i18n,
        lang_cookie: explicit.then(|| i18n.cookie()),
        binding,
        new_binding,
    })
}

/// CSRF + Origin check of a POST. `Err` = an error page.
fn check_post(
    pages: &Pages,
    step: &Step,
    headers: &HeaderMap,
    purpose: &str,
    token: &str,
) -> Result<(), Response> {
    let ok = !step.new_binding
        && csrf::same_origin(headers, &pages.config.public_origin)
        && pages.csrf.verify(&step.binding, purpose, &step.ix, token);
    if ok {
        Ok(())
    } else {
        tracing::warn!(realm = %step.realm.spec.name.as_str(), purpose, "idp: CSRF check failed");
        Err(error_page(
            headers,
            Some(&step.realm),
            step.i18n,
            StatusCode::FORBIDDEN,
            step.i18n.t("idp-csrf"),
        ))
    }
}

/// The live SSO session of this browser in `realm`.
async fn current_session(
    pages: &Pages,
    realm: &RealmRuntime,
    headers: &HeaderMap,
) -> Option<SsoSession> {
    let value = read_cookie(
        headers,
        &session_cookie_name(&pages.config, realm.spec.name.as_str()),
    )?;
    let hash = handle_hash(HandleKind::SsoSession, &value)?;
    let s = pages.store().get_session(&realm.id, &hash).await.ok()??;
    let now = Utc::now();
    if s.realm_id != realm.id || !s.alive(realm.spec.lifetimes.sso_session_idle, now) {
        return None;
    }
    let _ = pages.store().touch_session(&realm.id, &hash, now).await;
    Some(s)
}

/// The signed-in user of a step: progress bound to this browser's session.
async fn bound_user(
    pages: &Pages,
    step: &Step,
    headers: &HeaderMap,
) -> Option<(RealmUser, SsoSession, Progress)> {
    let progress = Progress::of(&step.pending);
    let user_id = progress.user_id.clone()?;
    let session = current_session(pages, &step.realm, headers).await?;
    if Some(&session.id_hash) != progress.session_hash.as_ref() || session.user_id != user_id {
        return None;
    }
    let user = pages
        .store()
        .get_user(&step.realm.id, &user_id)
        .await
        .ok()??;
    if user.disabled {
        return None;
    }
    Some((user, session, progress))
}

// ------------------------------------------------------------------ login

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    #[serde(default)]
    ix: String,
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

fn login_lede(step: &Step) -> String {
    step.i18n.t_arg2(
        "idp-login-lede",
        "realm",
        step.realm.spec.display_name.clone(),
        "app",
        step.pending.client_name.clone(),
    )
}

fn render_login(
    pages: &Pages,
    step: &Step,
    headers: &HeaderMap,
    username: &str,
    error: Option<Banner>,
    status: StatusCode,
) -> Response {
    let shell = step.shell(headers);
    let nonce = shell.nonce.clone();
    let reveal = helios_ui_chrome::password_toggle(&step.i18n).unwrap_or_default();
    let t = LoginTemplate {
        lede: login_lede(step),
        action: format!("/idp/{}/login", step.realm.spec.name.as_str()),
        ix: step.ix.clone(),
        csrf: step.csrf(pages, "login"),
        username: username.to_string(),
        error,
        reveal,
        shell,
    };
    Page::new(t.render().unwrap_or_default(), nonce, Some(&step.realm))
        .status(status)
        .cookies(step.cookies(pages))
        .into_response()
}

async fn login_page(
    State(pages): State<Pages>,
    Path(realm): Path<String>,
    Query(q): Query<IxQuery>,
    headers: HeaderMap,
) -> Response {
    let step = match load_step(&pages, &realm, &q.ix, q.lang.as_deref(), &headers).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let p = &step.pending;
    let force_login = p.has_prompt(Prompt::Login) || p.has_prompt(Prompt::SelectAccount);
    if !force_login && let Some(session) = current_session(&pages, &step.realm, &headers).await {
        let fresh_enough = p.max_age.is_none_or(|max| {
            Utc::now() - session.auth_time <= Duration::seconds(max.min(i64::MAX as u64) as i64)
        });
        if fresh_enough {
            // SSO: reuse the session (OIDC Core §3.1.2.3).
            let progress = Progress {
                user_id: Some(session.user_id.clone()),
                session_hash: Some(session.id_hash.clone()),
                sid: Some(session.sid.clone()),
                auth_time: Some(session.auth_time.timestamp()),
                amr: session.amr.clone(),
                patient: None,
            };
            return match save_progress(&pages, &step, progress).await {
                Ok(step) => advance(&pages, step, &headers, vec![]).await,
                Err(r) => r,
            };
        }
    }
    if p.has_prompt(Prompt::None) {
        return deny(
            &pages,
            &step,
            &headers,
            OAuthErrorCode::LoginRequired,
            "login required",
        )
        .await;
    }
    let hint = p.login_hint.clone().unwrap_or_default();
    render_login(&pages, &step, &headers, &hint, None, StatusCode::OK)
}

async fn save_progress(pages: &Pages, step: &Step, progress: Progress) -> Result<Step, Response> {
    let mut pending = step.pending.clone();
    pending.progress = serde_json::to_value(&progress).unwrap_or_default();
    match pages.store().update_interaction(pending.clone()).await {
        Ok(true) => Ok(Step {
            pending,
            realm: step.realm.clone(),
            ix: step.ix.clone(),
            i18n: step.i18n,
            lang_cookie: step.lang_cookie.clone(),
            binding: step.binding.clone(),
            new_binding: step.new_binding,
        }),
        _ => Err(error_page(
            &HeaderMap::new(),
            Some(&step.realm),
            step.i18n,
            StatusCode::BAD_REQUEST,
            step.i18n.t("idp-error-expired"),
        )),
    }
}

/// Verifies a password with bounded parallelism; `None` user ⇒ dummy
/// verification (same cost, so timing does not reveal unknown names).
async fn verify_password(pages: &Pages, phc: Option<String>, password: String) -> bool {
    let Ok(_permit) = pages.hashing.clone().acquire_owned().await else {
        return false;
    };
    tokio::task::spawn_blocking(move || match phc {
        Some(h) => helios_idp::password::verify(&password, &h),
        None => {
            let _ = helios_idp::password::verify(&password, helios_idp::password::dummy_hash());
            false
        }
    })
    .await
    .unwrap_or(false)
}

/// Authenticates a username/password in `realm`; on success creates a new
/// SSO session (fixation: always a fresh handle; the previous one is
/// revoked) and returns (user, session, cookie).
#[allow(clippy::too_many_arguments)]
async fn authenticate(
    pages: &Pages,
    realm: &RealmRuntime,
    headers: &HeaderMap,
    ip: &str,
    username: &str,
    password: &str,
    client_id: Option<&str>,
    i18n: I18n,
) -> Result<(RealmUser, SsoSession, String), Banner> {
    let store = pages.store();
    let realm_name = realm.spec.name.as_str().to_string();
    let username = username.trim();
    let user = if username.is_empty() || username.len() > 256 {
        None
    } else {
        store
            .find_user_by_username(&realm.id, username)
            .await
            .ok()
            .flatten()
    };
    let attempt = Attempt::new(
        &realm.id,
        user.as_ref().map(|u| u.id.as_str()),
        username,
        ip,
    );
    let policy = &realm.spec.security;
    let fail = |reason: &str| IdpEvent::LoginFailed {
        realm: realm_name.clone(),
        reason: reason.to_string(),
        username: Some(username.chars().take(256).collect()),
        client_id: client_id.map(String::from),
        ip: Some(ip.to_string()),
    };
    let locked = |until: i64| IdpEvent::LockedOut {
        realm: realm_name.clone(),
        username: Some(username.chars().take(256).collect()),
        until_unix: until,
        ip: Some(ip.to_string()),
    };
    if let Some(until) = pages.lockout.locked_until(&attempt).await {
        pages.runtime.emit(fail("locked out"));
        pages.runtime.emit(locked(until));
        let minutes = ((until - Utc::now().timestamp()).max(60) + 59) / 60;
        return Err(Banner::danger(
            i18n.t_arg("idp-locked", "minutes", minutes.to_string()),
            "idp-locked",
        ));
    }
    let phc = match &user {
        Some(u) if !u.disabled => store
            .get_password_hash(&realm.id, &u.id)
            .await
            .ok()
            .flatten()
            .map(|(h, _)| h),
        _ => None,
    };
    let has_hash = phc.is_some();
    let ok = verify_password(pages, phc, password.chars().take(256).collect()).await
        && password.len() <= helios_idp::password::MAX_LENGTH;
    let Some(user) = user.filter(|u| ok && has_hash && !u.disabled) else {
        let reason = if !has_hash {
            "unknown user, disabled or no password"
        } else {
            "wrong password"
        };
        pages.runtime.emit(fail(reason));
        if let Some(until) = pages.lockout.failed(&attempt, policy).await {
            pages.runtime.emit(locked(until));
            let minutes = ((until - Utc::now().timestamp()).max(60) + 59) / 60;
            return Err(Banner::danger(
                i18n.t_arg("idp-locked", "minutes", minutes.to_string()),
                "idp-locked",
            ));
        }
        return Err(Banner::danger(i18n.t("idp-bad-credentials"), "idp-error"));
    };
    pages.lockout.succeeded(&attempt).await;
    // Session fixation: revoke whatever session this browser had.
    if let Some(old) = read_cookie(headers, &session_cookie_name(&pages.config, &realm_name))
        .and_then(|v| handle_hash(HandleKind::SsoSession, &v))
    {
        let _ = store.revoke_session(&realm.id, &old, Utc::now()).await;
    }
    let now = Utc::now();
    let handle = mint_handle(HandleKind::SsoSession);
    let session = SsoSession {
        id_hash: handle.hash.clone(),
        realm_id: realm.id.clone(),
        user_id: user.id.clone(),
        sid: mint_handle(HandleKind::SsoSession).hash,
        auth_time: now,
        amr: vec!["pwd".into()],
        acr: None,
        created_at: now,
        last_seen_at: now,
        expires_at: now + Duration::seconds(realm.spec.lifetimes.sso_session_max as i64),
        revoked_at: None,
    };
    if let Err(e) = store.put_session(session.clone()).await {
        tracing::error!(error = %e, "idp: cannot store the SSO session");
        return Err(Banner::danger(i18n.t("idp-error-generic"), "idp-error"));
    }
    let _ = store.record_login(&realm.id, &user.id, now).await;
    pages.runtime.emit(IdpEvent::LoginSucceeded {
        realm: realm_name.clone(),
        user_id: user.id.clone(),
        client_id: client_id.map(String::from),
        amr: session.amr.clone(),
        ip: Some(ip.to_string()),
    });
    let cookie = set_cookie(
        &pages.config,
        &session_cookie_name(&pages.config, &realm_name),
        &handle.value,
        "Lax",
        None,
    );
    Ok((user, session, cookie))
}

async fn login_submit(
    State(pages): State<Pages>,
    Path(realm): Path<String>,
    peer: Peer,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let step = match load_step(&pages, &realm, &form.ix, None, &headers).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    if let Err(r) = check_post(&pages, &step, &headers, "login", &form.csrf) {
        return r;
    }
    let ip = client_ip(&pages.config, &headers, peer.0);
    match authenticate(
        &pages,
        &step.realm,
        &headers,
        &ip,
        &form.username,
        &form.password,
        Some(&step.pending.client_id),
        step.i18n,
    )
    .await
    {
        Ok((user, session, cookie)) => {
            let progress = Progress {
                user_id: Some(user.id.clone()),
                session_hash: Some(session.id_hash.clone()),
                sid: Some(session.sid.clone()),
                auth_time: Some(session.auth_time.timestamp()),
                amr: session.amr.clone(),
                patient: None,
            };
            match save_progress(&pages, &step, progress).await {
                Ok(step) => {
                    // The session cookie is set on this response; the next
                    // steps see it on the following requests. Advance with an
                    // explicit session so this request needs no cookie.
                    advance_as(&pages, step, &headers, vec![cookie], Some((user, session))).await
                }
                Err(r) => r,
            }
        }
        Err(banner) => render_login(
            &pages,
            &step,
            &headers,
            form.username.trim(),
            Some(banner),
            StatusCode::UNAUTHORIZED,
        ),
    }
}

// ---------------------------------------------------------------- advance

async fn advance(pages: &Pages, step: Step, headers: &HeaderMap, cookies: Vec<String>) -> Response {
    advance_as(pages, step, headers, cookies, None).await
}

/// Moves the interaction to its next step: picker, consent, or completion.
async fn advance_as(
    pages: &Pages,
    step: Step,
    headers: &HeaderMap,
    mut cookies: Vec<String>,
    fresh: Option<(RealmUser, SsoSession)>,
) -> Response {
    let (user, session, progress) = match fresh {
        Some((u, s)) => (u, s, Progress::of(&step.pending)),
        None => match bound_user(pages, &step, headers).await {
            Some(x) => x,
            None => return redirect(&step.path("login"), cookies),
        },
    };
    cookies.extend(step.cookies(pages));
    let p = &step.pending;
    let prompt_none = p.has_prompt(Prompt::None);

    // 1. Patient picker.
    if p.needs.patient_picker && progress.patient.is_none() && p.launch_context.patient.is_none() {
        let selectable = user.selectable_patients(&p.endpoint);
        if user.kind != UserKind::Practitioner && selectable.len() == 1 {
            let mut pr = progress.clone();
            pr.patient = Some(selectable[0].clone());
            return match save_progress(pages, &step, pr).await {
                Ok(step) => {
                    Box::pin(advance_as(
                        pages,
                        step,
                        headers,
                        cookies,
                        Some((user, session)),
                    ))
                    .await
                }
                Err(r) => r,
            };
        }
        if prompt_none {
            return deny(
                pages,
                &step,
                headers,
                OAuthErrorCode::InteractionRequired,
                "patient selection required",
            )
            .await;
        }
        return redirect(&step.path("select-patient"), cookies);
    }

    // 2. Consent.
    let client = pages
        .store()
        .get_client(&step.realm.id, &p.client_id)
        .await
        .ok()
        .flatten();
    let trusted = client.as_ref().is_some_and(|c| c.trusted);
    let needs_consent = if p.has_prompt(Prompt::Consent) {
        true
    } else if !p.needs.consent || trusted {
        false
    } else {
        let stored = pages
            .store()
            .get_consent(&step.realm.id, &user.id, &p.client_id, &p.endpoint)
            .await
            .ok()
            .flatten()
            .filter(|c| c.expires_at.is_none_or(|e| e > Utc::now()));
        !stored.is_some_and(|c| scopes::covered(&c.scopes, &p.scopes))
    };
    if needs_consent {
        if prompt_none {
            return deny(
                pages,
                &step,
                headers,
                OAuthErrorCode::ConsentRequired,
                "consent required",
            )
            .await;
        }
        return redirect(&step.path("consent"), cookies);
    }
    finish(
        pages,
        &step,
        headers,
        &user,
        &session,
        &progress,
        p.scopes.clone(),
        cookies,
    )
    .await
}

/// Completes the interaction with the granted scopes.
#[allow(clippy::too_many_arguments)]
async fn finish(
    pages: &Pages,
    step: &Step,
    headers: &HeaderMap,
    user: &RealmUser,
    session: &SsoSession,
    progress: &Progress,
    granted: Vec<String>,
    cookies: Vec<String>,
) -> Response {
    let p = &step.pending;
    let endpoint_base = step.realm.endpoint_base(&p.endpoint).unwrap_or(&p.audience);
    let patient = progress
        .patient
        .clone()
        .or_else(|| p.launch_context.patient.clone());
    let context = LaunchContext {
        patient: patient.clone(),
        need_patient_banner: (!p.ehr_launch && patient.is_some()).then_some(true),
        ..Default::default()
    };
    let decision = Decision {
        subject: user.id.clone(),
        sid: Some(session.sid.clone()),
        session_id_hash: Some(session.id_hash.clone()),
        auth_time: progress
            .auth_time
            .and_then(|t| DateTime::from_timestamp(t, 0))
            .unwrap_or(session.auth_time),
        amr: if progress.amr.is_empty() {
            session.amr.clone()
        } else {
            progress.amr.clone()
        },
        acr: session.acr.clone(),
        granted_scopes: granted,
        context,
        fhir_user: user.fhir_user(&p.endpoint, endpoint_base),
    };
    match pages
        .completer
        .complete(step.realm.spec.name.as_str(), &step.ix, decision)
        .await
    {
        Ok(resp) => respond(pages, step, headers, resp, cookies),
        Err(e) => complete_error(step, headers, e),
    }
}

fn complete_error(step: &Step, headers: &HeaderMap, e: CompleteError) -> Response {
    tracing::warn!(error = %e, "idp: completing the interaction failed");
    let text = match e {
        CompleteError::UnknownInteraction => step.i18n.t("idp-error-expired"),
        // The completer's message says what was inconsistent (e.g. a
        // patient outside the user's set); it carries no secrets.
        CompleteError::InvalidDecision(m) => format!("{} ({m})", step.i18n.t("idp-error-generic")),
        CompleteError::Internal(_) => step.i18n.t("idp-error-generic"),
    };
    error_page(
        headers,
        Some(&step.realm),
        step.i18n,
        StatusCode::BAD_REQUEST,
        text,
    )
}

/// Sends the authorization response: a 303 for `query`, the auto-submitting
/// form (own CSP: `form-action` = the client's origin) for `form_post`.
fn respond(
    pages: &Pages,
    step: &Step,
    _headers: &HeaderMap,
    resp: AuthorizationResponse,
    cookies: Vec<String>,
) -> Response {
    match resp.mode {
        ResponseMode::Query => redirect(&resp.redirect_url(), cookies),
        ResponseMode::FormPost => {
            let nonce = security::nonce();
            let mut r = (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                resp.form_post_html(&nonce),
            )
                .into_response();
            security::apply(r.headers_mut(), &nonce, &[], None);
            if let Ok(v) = HeaderValue::from_str(&resp.form_post_csp(&nonce)) {
                r.headers_mut().insert(header::CONTENT_SECURITY_POLICY, v);
            }
            for c in cookies {
                if let Ok(v) = HeaderValue::from_str(&c) {
                    r.headers_mut().append(header::SET_COOKIE, v);
                }
            }
            let _ = (pages, step);
            r
        }
    }
}

async fn deny(
    pages: &Pages,
    step: &Step,
    headers: &HeaderMap,
    code: OAuthErrorCode,
    description: &str,
) -> Response {
    match pages
        .completer
        .deny(
            step.realm.spec.name.as_str(),
            &step.ix,
            OAuthError::new(code, description),
        )
        .await
    {
        Ok(resp) => respond(pages, step, headers, resp, step.cookies(pages)),
        Err(e) => complete_error(step, headers, e),
    }
}

// ----------------------------------------------------------------- picker

#[derive(Debug, Deserialize)]
pub struct PickerForm {
    #[serde(default)]
    ix: String,
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    patient: String,
}

fn rows(i18n: &I18n, list: Vec<PatientSummary>) -> Vec<PickerRow> {
    list.into_iter()
        .map(|p| {
            let mut meta = vec![];
            if let Some(d) = &p.birth_date {
                meta.push(i18n.t_arg("idp-picker-born", "date", d.clone()));
            }
            if let Some(g) = &p.gender {
                meta.push(g.clone());
            }
            meta.push(format!("Patient/{}", p.id));
            PickerRow {
                id: p.id,
                name: p.name,
                meta: meta.join(" · "),
            }
        })
        .collect()
}

async fn picker_candidates(
    pages: &Pages,
    step: &Step,
    user: &RealmUser,
    query: &str,
) -> (Vec<PatientSummary>, bool) {
    let endpoint = &step.pending.endpoint;
    if user.kind == UserKind::Practitioner {
        let found = pages
            .directory
            .search(&step.realm, endpoint, user, query)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "idp: patient search failed");
                vec![]
            });
        return (found, true);
    }
    let mut out = vec![];
    for id in user.selectable_patients(endpoint) {
        match pages.directory.get(&step.realm, endpoint, user, &id).await {
            Ok(Some(p)) => out.push(p),
            // Linked but not readable (deleted, or the endpoint refuses):
            // still offered by id; `complete` re-validates the link.
            Ok(None) | Err(_) => out.push(PatientSummary {
                id: id.clone(),
                name: format!("Patient/{id}"),
                birth_date: None,
                gender: None,
            }),
        }
    }
    (out, false)
}

async fn render_picker(
    pages: &Pages,
    step: &Step,
    headers: &HeaderMap,
    user: &RealmUser,
    query: &str,
    error: Option<Banner>,
) -> Response {
    let (list, searchable) = picker_candidates(pages, step, user, query).await;
    let shell = step.shell(headers);
    let nonce = shell.nonce.clone();
    let i = step.i18n;
    let empty_text = if searchable {
        i.t("idp-picker-none")
    } else {
        i.t("idp-picker-empty")
    };
    let t = PickerTemplate {
        lede: i.t_arg("idp-picker-lede", "app", step.pending.client_name.clone()),
        action: format!("/idp/{}/select-patient", step.realm.spec.name.as_str()),
        ix: step.ix.clone(),
        csrf: step.csrf(pages, "select-patient"),
        searchable,
        query: query.to_string(),
        patients: rows(&i, list),
        empty_text,
        error,
        shell,
    };
    Page::new(t.render().unwrap_or_default(), nonce, Some(&step.realm))
        .cookies(step.cookies(pages))
        .into_response()
}

async fn picker_page(
    State(pages): State<Pages>,
    Path(realm): Path<String>,
    Query(q): Query<IxQuery>,
    headers: HeaderMap,
) -> Response {
    let step = match load_step(&pages, &realm, &q.ix, q.lang.as_deref(), &headers).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some((user, _, _)) = bound_user(&pages, &step, &headers).await else {
        return redirect(&step.path("login"), step.cookies(&pages));
    };
    if !step.pending.needs.patient_picker {
        return advance(&pages, step, &headers, vec![]).await;
    }
    let query: String = q.q.unwrap_or_default().chars().take(100).collect();
    render_picker(&pages, &step, &headers, &user, query.trim(), None).await
}

async fn picker_submit(
    State(pages): State<Pages>,
    Path(realm): Path<String>,
    headers: HeaderMap,
    Form(form): Form<PickerForm>,
) -> Response {
    let step = match load_step(&pages, &realm, &form.ix, None, &headers).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    if let Err(r) = check_post(&pages, &step, &headers, "select-patient", &form.csrf) {
        return r;
    }
    let Some((user, _, mut progress)) = bound_user(&pages, &step, &headers).await else {
        return redirect(&step.path("login"), step.cookies(&pages));
    };
    let id = form.patient.trim().to_string();
    // The patient must be in the user's selectable set: linked patients for
    // patients / caregivers, readable with their own permissions for
    // practitioners (design §10.4 "patient-context escalation").
    let allowed = match user.kind {
        UserKind::Practitioner => matches!(
            pages
                .directory
                .get(&step.realm, &step.pending.endpoint, &user, &id)
                .await,
            Ok(Some(_))
        ),
        _ => user
            .selectable_patients(&step.pending.endpoint)
            .iter()
            .any(|p| *p == id),
    };
    if !allowed {
        tracing::warn!(realm = %realm, user = %user.id, "idp: patient outside the selectable set refused");
        let b = Banner::danger(step.i18n.t("idp-picker-invalid"), "idp-error");
        return render_picker(&pages, &step, &headers, &user, "", Some(b)).await;
    }
    progress.patient = Some(id);
    match save_progress(&pages, &step, progress).await {
        Ok(step) => advance(&pages, step, &headers, vec![]).await,
        Err(r) => r,
    }
}

// ---------------------------------------------------------------- consent

/// The consent form; `scope` repeats (one per ticked checkbox), so it is
/// parsed from the raw body.
#[derive(Debug, Default)]
pub struct ConsentForm {
    ix: String,
    csrf: String,
    decision: String,
    remember: bool,
    scopes: Vec<String>,
}

impl ConsentForm {
    fn parse(body: &[u8]) -> Self {
        let mut f = ConsentForm::default();
        for (k, v) in form_urlencoded::parse(body) {
            match k.as_ref() {
                "ix" => f.ix = v.into_owned(),
                "csrf" => f.csrf = v.into_owned(),
                "decision" => f.decision = v.into_owned(),
                "remember" => f.remember = v == "1",
                "scope" if f.scopes.len() < 200 => f.scopes.push(v.into_owned()),
                _ => {}
            }
        }
        f
    }
}

fn redirect_host(uri: &str) -> String {
    url::Url::parse(uri)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_else(|| uri.to_string())
}

async fn render_consent(
    pages: &Pages,
    step: &Step,
    headers: &HeaderMap,
    user: &RealmUser,
    progress: &Progress,
    error: Option<Banner>,
) -> Response {
    let i = step.i18n;
    let p = &step.pending;
    let lines: Vec<ScopeLine> = p.scopes.iter().map(|s| scopes::describe(&i, s)).collect();
    let patient_id = progress
        .patient
        .clone()
        .or_else(|| p.launch_context.patient.clone());
    let patient = match patient_id {
        Some(id) => {
            let name = pages
                .directory
                .get(&step.realm, &p.endpoint, user, &id)
                .await
                .ok()
                .flatten()
                .map(|s| s.name)
                .unwrap_or_else(|| format!("Patient/{id}"));
            Some(i.t_arg("idp-consent-patient", "name", name))
        }
        None => None,
    };
    let shell = step.shell(headers);
    let nonce = shell.nonce.clone();
    let who = user
        .display_name
        .clone()
        .unwrap_or_else(|| user.username.clone());
    let t = ConsentTemplate {
        title: i.t_arg("idp-consent-title", "app", p.client_name.clone()),
        lede: i.t_args(
            "idp-consent-lede",
            &[
                ("app", p.client_name.clone()),
                ("host", redirect_host(&p.redirect_uri)),
                ("realm", step.realm.spec.display_name.clone()),
            ],
        ),
        action: format!("/idp/{}/consent", step.realm.spec.name.as_str()),
        ix: step.ix.clone(),
        csrf: step.csrf(pages, "consent"),
        signed_in: i.t_arg("idp-consent-signed-in", "user", who),
        patient,
        has_optional: lines.iter().any(|l| l.optional),
        scopes: lines,
        error,
        shell,
    };
    Page::new(t.render().unwrap_or_default(), nonce, Some(&step.realm))
        .cookies(step.cookies(pages))
        .into_response()
}

async fn consent_page(
    State(pages): State<Pages>,
    Path(realm): Path<String>,
    Query(q): Query<IxQuery>,
    headers: HeaderMap,
) -> Response {
    let step = match load_step(&pages, &realm, &q.ix, q.lang.as_deref(), &headers).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some((user, _, progress)) = bound_user(&pages, &step, &headers).await else {
        return redirect(&step.path("login"), step.cookies(&pages));
    };
    render_consent(&pages, &step, &headers, &user, &progress, None).await
}

async fn consent_submit(
    State(pages): State<Pages>,
    Path(realm): Path<String>,
    peer: Peer,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let form = ConsentForm::parse(&body);
    let step = match load_step(&pages, &realm, &form.ix, None, &headers).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    if let Err(r) = check_post(&pages, &step, &headers, "consent", &form.csrf) {
        return r;
    }
    let Some((user, session, progress)) = bound_user(&pages, &step, &headers).await else {
        return redirect(&step.path("login"), step.cookies(&pages));
    };
    let ip = client_ip(&pages.config, &headers, peer.0);
    let p = step.pending.clone();
    if form.decision != "allow" {
        pages.runtime.emit(IdpEvent::ConsentDenied {
            realm: realm.clone(),
            user_id: user.id.clone(),
            client_id: p.client_id.clone(),
            endpoint: p.endpoint.clone(),
            ip: Some(ip),
        });
        return deny(
            &pages,
            &step,
            &headers,
            OAuthErrorCode::AccessDenied,
            "the user denied the request",
        )
        .await;
    }
    let granted = scopes::granted(&p.scopes, &form.scopes);
    let any_data = granted.iter().any(|s| {
        !matches!(
            helios_idp::scope::parse_scope(s),
            helios_idp::scope::Scope::OpenId
                | helios_idp::scope::Scope::FhirUser
                | helios_idp::scope::Scope::Launch
                | helios_idp::scope::Scope::LaunchContext { .. }
        )
    });
    let requested_data = p.scopes.len() != scopes::granted(&p.scopes, &[]).len();
    if requested_data && !any_data {
        let b = Banner::danger(step.i18n.t("idp-consent-nothing"), "idp-error");
        return render_consent(&pages, &step, &headers, &user, &progress, Some(b)).await;
    }
    if form.remember {
        let c = Consent {
            realm_id: step.realm.id.clone(),
            user_id: user.id.clone(),
            client_id: p.client_id.clone(),
            endpoint: p.endpoint.clone(),
            scopes: granted.clone(),
            granted_at: Utc::now(),
            expires_at: None,
        };
        if let Err(e) = pages.store().put_consent(c).await {
            tracing::warn!(error = %e, "idp: cannot remember consent");
        }
    }
    let patient = progress
        .patient
        .clone()
        .or_else(|| p.launch_context.patient.clone());
    pages.runtime.emit(IdpEvent::ConsentGranted {
        realm: realm.clone(),
        user_id: user.id.clone(),
        client_id: p.client_id.clone(),
        endpoint: p.endpoint.clone(),
        scopes: granted.clone(),
        patient,
        remembered: form.remember,
        ip: Some(ip),
    });
    let cookies = step.cookies(&pages);
    finish(
        &pages, &step, &headers, &user, &session, &progress, granted, cookies,
    )
    .await
}

// ----------------------------------------------------------------- logout

#[derive(Debug, Deserialize, Default)]
pub struct LogoutQuery {
    #[serde(default)]
    id_token_hint: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    post_logout_redirect_uri: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    ui_locales: Option<String>,
    #[serde(default)]
    lang: Option<String>,
    #[serde(default)]
    csrf: Option<String>,
}

impl LogoutQuery {
    fn request(&self) -> LogoutRequest {
        let t = |v: &Option<String>| v.clone().filter(|s| !s.is_empty() && s.len() < 8192);
        LogoutRequest {
            id_token_hint: t(&self.id_token_hint),
            client_id: t(&self.client_id),
            post_logout_redirect_uri: t(&self.post_logout_redirect_uri),
            state: t(&self.state),
        }
    }
    fn carry(&self) -> Vec<(String, String)> {
        let mut v = vec![];
        for (k, val) in [
            ("id_token_hint", &self.id_token_hint),
            ("client_id", &self.client_id),
            ("post_logout_redirect_uri", &self.post_logout_redirect_uri),
            ("state", &self.state),
        ] {
            if let Some(x) = val.as_ref().filter(|s| !s.is_empty()) {
                v.push((k.to_string(), x.clone()));
            }
        }
        v
    }
}

/// CSRF "interaction" of the logout form: bound to the browser's session.
fn logout_ix(session: Option<&SsoSession>) -> String {
    format!("logout:{}", session.map(|s| s.sid.as_str()).unwrap_or("-"))
}

fn binding_of(pages: &Pages, headers: &HeaderMap) -> (String, bool) {
    match read_cookie(headers, &cookie_name(&pages.config, CSRF_COOKIE))
        .filter(|v| v.len() >= 32 && v.len() <= 64)
    {
        Some(b) => (b, false),
        None => (csrf::new_binding(), true),
    }
}

async fn end_session(
    pages: &Pages,
    realm: &RealmRuntime,
    session: Option<&SsoSession>,
    client_id: Option<String>,
    ip: String,
) -> String {
    if let Some(s) = session {
        let _ = pages
            .store()
            .revoke_session(&realm.id, &s.id_hash, Utc::now())
            .await;
        pages.runtime.emit(IdpEvent::Logout {
            realm: realm.spec.name.as_str().to_string(),
            user_id: Some(s.user_id.clone()),
            client_id,
            ip: Some(ip),
        });
    }
    // Expire the cookie.
    set_cookie(
        &pages.config,
        &session_cookie_name(&pages.config, realm.spec.name.as_str()),
        "",
        "Lax",
        Some(0),
    )
}

fn logged_out_page(
    headers: &HeaderMap,
    realm: &RealmRuntime,
    i18n: I18n,
    cookies: Vec<String>,
) -> Response {
    let shell = Shell::new(i18n, headers, &realm.spec.display_name, "");
    let nonce = shell.nonce.clone();
    let t = MessageTemplate {
        title: i18n.t("idp-logout-done-title"),
        text: i18n.t_arg("idp-logout-done", "realm", realm.spec.display_name.clone()),
        banner: None,
        shell,
    };
    Page::new(t.render().unwrap_or_default(), nonce, Some(realm))
        .cookies(cookies)
        .into_response()
}

async fn logout_page(
    State(pages): State<Pages>,
    Path(realm_name): Path<String>,
    peer: Peer,
    Query(q): Query<LogoutQuery>,
    headers: HeaderMap,
) -> Response {
    let locales: Vec<String> = q
        .ui_locales
        .as_deref()
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default();
    let (i18n, explicit) = I18n::negotiate(q.lang.as_deref(), &headers, &locales);
    let Some(realm) = realm_or_404(&pages, &realm_name) else {
        return error_page(
            &headers,
            None,
            i18n,
            StatusCode::NOT_FOUND,
            i18n.t("idp-error-realm"),
        );
    };
    let plan = match check_logout(&realm, pages.store().as_ref(), &q.request()).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e.description, "idp: invalid logout request");
            return error_page(
                &headers,
                Some(&realm),
                i18n,
                StatusCode::BAD_REQUEST,
                i18n.t("idp-logout-invalid"),
            );
        }
    };
    let session = current_session(&pages, &realm, &headers).await;
    let ip = client_ip(&pages.config, &headers, peer.0);
    let mut cookies = vec![];
    if explicit {
        cookies.push(i18n.cookie());
    }
    // No session: nothing to end. A valid hint naming this very session:
    // the RP proved it holds the id_token; end it without asking (OIDC
    // RP-Initiated Logout §2 leaves the confirmation to the OP).
    let auto = session.is_none()
        || (plan.hint_valid
            && plan.sid.is_some()
            && session.as_ref().map(|s| &s.sid) == plan.sid.as_ref());
    if auto {
        cookies
            .push(end_session(&pages, &realm, session.as_ref(), plan.client_id.clone(), ip).await);
        return match plan.redirect {
            Some(to) => redirect(&to, cookies),
            None => logged_out_page(&headers, &realm, i18n, cookies),
        };
    }
    let session = session.expect("checked above");
    let (binding, new_binding) = binding_of(&pages, &headers);
    if new_binding {
        cookies.push(set_cookie(
            &pages.config,
            &cookie_name(&pages.config, CSRF_COOKIE),
            &binding,
            "Strict",
            None,
        ));
    }
    let user = pages
        .store()
        .get_user(&realm.id, &session.user_id)
        .await
        .ok()
        .flatten();
    let who = user
        .map(|u| u.display_name.unwrap_or(u.username))
        .unwrap_or_default();
    let app_line = match &plan.client_id {
        Some(cid) => {
            let name = pages
                .store()
                .get_client(&realm.id, cid)
                .await
                .ok()
                .flatten()
                .map(|c| c.name)
                .unwrap_or_else(|| cid.clone());
            i18n.t_arg("idp-logout-app", "app", name)
        }
        None => String::new(),
    };
    let shell = Shell::new(i18n, &headers, &realm.spec.display_name, "");
    let nonce = shell.nonce.clone();
    let t = LogoutTemplate {
        lede: i18n.t_arg2(
            "idp-logout-lede",
            "realm",
            realm.spec.display_name.clone(),
            "user",
            who,
        ),
        app_line,
        action: format!("/idp/{}/logout", realm.spec.name.as_str()),
        csrf: pages
            .csrf
            .token(&binding, "logout", &logout_ix(Some(&session))),
        carry: q.carry(),
        cancel_href: format!("/idp/{}/account/apps", realm.spec.name.as_str()),
        shell,
    };
    Page::new(t.render().unwrap_or_default(), nonce, Some(&realm))
        .cookies(cookies)
        .into_response()
}

async fn logout_submit(
    State(pages): State<Pages>,
    Path(realm_name): Path<String>,
    peer: Peer,
    headers: HeaderMap,
    Form(q): Form<LogoutQuery>,
) -> Response {
    let (i18n, _) = I18n::negotiate(None, &headers, &[]);
    let Some(realm) = realm_or_404(&pages, &realm_name) else {
        return error_page(
            &headers,
            None,
            i18n,
            StatusCode::NOT_FOUND,
            i18n.t("idp-error-realm"),
        );
    };
    let session = current_session(&pages, &realm, &headers).await;
    let (binding, new_binding) = binding_of(&pages, &headers);
    let token_ok = !new_binding
        && csrf::same_origin(&headers, &pages.config.public_origin)
        && pages.csrf.verify(
            &binding,
            "logout",
            &logout_ix(session.as_ref()),
            q.csrf.as_deref().unwrap_or(""),
        );
    if !token_ok {
        return error_page(
            &headers,
            Some(&realm),
            i18n,
            StatusCode::FORBIDDEN,
            i18n.t("idp-csrf"),
        );
    }
    let plan = match check_logout(&realm, pages.store().as_ref(), &q.request()).await {
        Ok(p) => p,
        Err(_) => {
            return error_page(
                &headers,
                Some(&realm),
                i18n,
                StatusCode::BAD_REQUEST,
                i18n.t("idp-logout-invalid"),
            );
        }
    };
    let ip = client_ip(&pages.config, &headers, peer.0);
    let cookie = end_session(&pages, &realm, session.as_ref(), plan.client_id.clone(), ip).await;
    match plan.redirect {
        Some(to) => redirect(&to, vec![cookie]),
        None => logged_out_page(&headers, &realm, i18n, vec![cookie]),
    }
}

// ----------------------------------------------------- account / apps page

#[derive(Debug, Deserialize, Default)]
pub struct AppsQuery {
    #[serde(default)]
    lang: Option<String>,
    #[serde(default)]
    revoked: Option<String>,
}

const ACCOUNT_IX: &str = "account";

async fn render_account_login(
    pages: &Pages,
    realm: &RealmRuntime,
    headers: &HeaderMap,
    i18n: I18n,
    lang_cookie: Option<String>,
    username: &str,
    error: Option<Banner>,
    status: StatusCode,
) -> Response {
    let (binding, new_binding) = binding_of(pages, headers);
    let mut cookies = vec![];
    if new_binding {
        cookies.push(set_cookie(
            &pages.config,
            &cookie_name(&pages.config, CSRF_COOKIE),
            &binding,
            "Strict",
            None,
        ));
    }
    cookies.extend(lang_cookie);
    let shell = Shell::new(i18n, headers, &realm.spec.display_name, "");
    let nonce = shell.nonce.clone();
    let t = LoginTemplate {
        lede: i18n.t_arg(
            "idp-login-lede-account",
            "realm",
            realm.spec.display_name.clone(),
        ),
        action: format!("/idp/{}/account/login", realm.spec.name.as_str()),
        ix: ACCOUNT_IX.into(),
        csrf: pages.csrf.token(&binding, "account-login", ACCOUNT_IX),
        username: username.to_string(),
        error,
        reveal: helios_ui_chrome::password_toggle(&i18n).unwrap_or_default(),
        shell,
    };
    Page::new(t.render().unwrap_or_default(), nonce, Some(realm))
        .status(status)
        .cookies(cookies)
        .into_response()
}

async fn apps_page(
    State(pages): State<Pages>,
    Path(realm_name): Path<String>,
    Query(q): Query<AppsQuery>,
    headers: HeaderMap,
) -> Response {
    let (i18n, explicit) = I18n::negotiate(q.lang.as_deref(), &headers, &[]);
    let lang_cookie = explicit.then(|| i18n.cookie());
    let Some(realm) = realm_or_404(&pages, &realm_name) else {
        return error_page(
            &headers,
            None,
            i18n,
            StatusCode::NOT_FOUND,
            i18n.t("idp-error-realm"),
        );
    };
    let Some(session) = current_session(&pages, &realm, &headers).await else {
        return render_account_login(
            &pages,
            &realm,
            &headers,
            i18n,
            lang_cookie,
            "",
            None,
            StatusCode::OK,
        )
        .await;
    };
    let store = pages.store();
    let consents = store
        .list_consents(&realm.id, &session.user_id)
        .await
        .unwrap_or_default();
    let mut apps = vec![];
    for c in consents {
        let name = store
            .get_client(&realm.id, &c.client_id)
            .await
            .ok()
            .flatten()
            .map(|x| x.name)
            .unwrap_or_else(|| c.client_id.clone());
        apps.push(AppRow {
            meta: format!(
                "{} · {}",
                i18n.t_arg(
                    "idp-apps-granted",
                    "date",
                    c.granted_at.format("%Y-%m-%d").to_string()
                ),
                c.endpoint
            ),
            scopes: c.scopes.join(" "),
            client_id: c.client_id,
            endpoint: c.endpoint,
            name,
        });
    }
    let (binding, new_binding) = binding_of(&pages, &headers);
    let mut cookies: Vec<String> = lang_cookie.into_iter().collect();
    if new_binding {
        cookies.push(set_cookie(
            &pages.config,
            &cookie_name(&pages.config, CSRF_COOKIE),
            &binding,
            "Strict",
            None,
        ));
    }
    let notice = q
        .revoked
        .filter(|r| r.len() <= 200)
        .map(|r| Banner::ok(i18n.t_arg("idp-apps-revoked", "app", r), "idp-apps-notice"));
    let shell = Shell::new(i18n, &headers, &realm.spec.display_name, "");
    let nonce = shell.nonce.clone();
    let t = AppsTemplate {
        lede: i18n.t_arg("idp-apps-lede", "realm", realm.spec.display_name.clone()),
        action: format!("/idp/{}/account/apps", realm.spec.name.as_str()),
        csrf: pages
            .csrf
            .token(&binding, "apps", &logout_ix(Some(&session))),
        apps,
        notice,
        logout_href: format!("/idp/{}/logout", realm.spec.name.as_str()),
        shell,
    };
    Page::new(t.render().unwrap_or_default(), nonce, Some(&realm))
        .cookies(cookies)
        .into_response()
}

async fn account_login(
    State(pages): State<Pages>,
    Path(realm_name): Path<String>,
    peer: Peer,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let (i18n, _) = I18n::negotiate(None, &headers, &[]);
    let Some(realm) = realm_or_404(&pages, &realm_name) else {
        return error_page(
            &headers,
            None,
            i18n,
            StatusCode::NOT_FOUND,
            i18n.t("idp-error-realm"),
        );
    };
    let (binding, new_binding) = binding_of(&pages, &headers);
    if new_binding
        || !csrf::same_origin(&headers, &pages.config.public_origin)
        || !pages
            .csrf
            .verify(&binding, "account-login", ACCOUNT_IX, &form.csrf)
    {
        return error_page(
            &headers,
            Some(&realm),
            i18n,
            StatusCode::FORBIDDEN,
            i18n.t("idp-csrf"),
        );
    }
    let ip = client_ip(&pages.config, &headers, peer.0);
    match authenticate(
        &pages,
        &realm,
        &headers,
        &ip,
        &form.username,
        &form.password,
        None,
        i18n,
    )
    .await
    {
        Ok((_, _, cookie)) => redirect(
            &format!("/idp/{}/account/apps", realm.spec.name.as_str()),
            vec![cookie],
        ),
        Err(b) => {
            render_account_login(
                &pages,
                &realm,
                &headers,
                i18n,
                None,
                form.username.trim(),
                Some(b),
                StatusCode::UNAUTHORIZED,
            )
            .await
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RevokeForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    endpoint: String,
}

async fn apps_revoke(
    State(pages): State<Pages>,
    Path(realm_name): Path<String>,
    peer: Peer,
    headers: HeaderMap,
    Form(form): Form<RevokeForm>,
) -> Response {
    let (i18n, _) = I18n::negotiate(None, &headers, &[]);
    let Some(realm) = realm_or_404(&pages, &realm_name) else {
        return error_page(
            &headers,
            None,
            i18n,
            StatusCode::NOT_FOUND,
            i18n.t("idp-error-realm"),
        );
    };
    let Some(session) = current_session(&pages, &realm, &headers).await else {
        return redirect(&format!("/idp/{realm_name}/account/apps"), vec![]);
    };
    let (binding, new_binding) = binding_of(&pages, &headers);
    if new_binding
        || !csrf::same_origin(&headers, &pages.config.public_origin)
        || !pages
            .csrf
            .verify(&binding, "apps", &logout_ix(Some(&session)), &form.csrf)
    {
        return error_page(
            &headers,
            Some(&realm),
            i18n,
            StatusCode::FORBIDDEN,
            i18n.t("idp-csrf"),
        );
    }
    let store = pages.store();
    let ip = client_ip(&pages.config, &headers, peer.0);
    let user = session.user_id.clone();
    let _ = store
        .delete_consent(&realm.id, &user, &form.client_id, &form.endpoint)
        .await;
    // Withdrawing consent revokes the app's grants for this user at once
    // (internal endpoints refuse its tokens on the next request).
    let grants = store
        .list_grants(
            &realm.id,
            &GrantFilter {
                subject: Some(user.clone()),
                client_id: Some(form.client_id.clone()),
                include_revoked: false,
                limit: Some(1000),
            },
        )
        .await
        .unwrap_or_default();
    let now = Utc::now();
    let aud = realm.endpoint_base(&form.endpoint).map(str::to_string);
    for g in grants.into_iter().filter(|g| {
        aud.as_ref()
            .is_none_or(|a| g.audience.iter().any(|x| x.trim_end_matches('/') == a))
    }) {
        if let Ok(true) = store
            .revoke_grant(&realm.id, &g.id, "consent withdrawn by the user", now)
            .await
        {
            pages.runtime.mark_revoked(&realm_name, &g.id);
            pages.runtime.emit(IdpEvent::TokenRevoked {
                realm: realm_name.clone(),
                client_id: g.client_id.clone(),
                gid: g.id.clone(),
                ip: Some(ip.clone()),
            });
        }
    }
    pages.runtime.emit(IdpEvent::ConsentWithdrawn {
        realm: realm_name.clone(),
        user_id: user,
        client_id: form.client_id.clone(),
        endpoint: Some(form.endpoint.clone()),
        ip: Some(ip),
    });
    let name = store
        .get_client(&realm.id, &form.client_id)
        .await
        .ok()
        .flatten()
        .map(|c| c.name)
        .unwrap_or(form.client_id);
    let mut q = BTreeMap::new();
    q.insert("revoked", name);
    let qs: String = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(q)
        .finish();
    redirect(&format!("/idp/{realm_name}/account/apps?{qs}"), vec![])
}
