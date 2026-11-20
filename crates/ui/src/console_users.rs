//! Settings → Users (`/ui/settings/users`, admin; package P3): built-in
//! accounts, per-endpoint role bindings and live console sessions. Every
//! action goes through [`ConsoleAuth`] — the same rules as the admin REST
//! API (`/admin/console/users`, `/bindings`, `/sessions`).

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::Extension;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use helios_admin_auth::console::{ConsoleAuth, ConsoleError, NewUser, Resolved, UserPatch};
use helios_endpoints::console::{ConsoleAuthMode, ConsoleRole, RoleBinding};

use crate::account::{error_text, via_label};
use crate::csrf::CsrfField;
use crate::{I18n, RequestLocale, RequestTenant, RequestVersion, Status, WebState};

pub(crate) struct UserRow {
    pub(crate) id: String,
    pub(crate) username: String,
    pub(crate) display: String,
    pub(crate) role: String,
    pub(crate) disabled: bool,
    pub(crate) revision: u64,
    pub(crate) last_login: String,
}

pub(crate) struct SessionRow {
    pub(crate) key: String,
    pub(crate) subject: String,
    pub(crate) display: String,
    pub(crate) role: &'static str,
    pub(crate) via: &'static str,
    pub(crate) client: String,
    pub(crate) last_seen: String,
    pub(crate) current: bool,
}

#[derive(Template)]
#[template(path = "pages/settings-users.html")]
struct UsersPage {
    status: Status,
    i18n: I18n,
    active_page: &'static str,
    csrf_html: String,
    users: Vec<UserRow>,
    bindings: Vec<RoleBinding>,
    bindings_error: Option<String>,
    sessions: Vec<SessionRow>,
    local_mode: bool,
    min_password_length: u8,
    notice: Option<String>,
    one_time_password: Option<String>,
    error: Option<String>,
}

fn unix_text(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default()
}

/// What a page render needs besides the runtime.
pub(crate) struct Ctx {
    state: WebState,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    console: Arc<ConsoleAuth>,
    resolved: Option<Resolved>,
    csrf: CsrfField,
}

async fn render(
    ctx: &Ctx,
    notice: Option<String>,
    one_time_password: Option<String>,
    error: Option<String>,
    status: StatusCode,
) -> Response {
    let c = &ctx.console;
    let i18n = I18n::new(ctx.locale);
    let (users, mut error) = match c.list_users().await {
        Ok(u) => (u, error),
        Err(e) => (Vec::new(), error.or_else(|| Some(error_text(&i18n, &e)))),
    };
    let (bindings, bindings_error) = match c.store().list_role_bindings().await {
        Ok(b) => (b, None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    };
    let current = ctx.resolved.as_ref().map(|r| r.key.clone());
    let sessions = match c.list_sessions().await {
        Ok(list) => list
            .into_iter()
            .map(|(key, s)| SessionRow {
                current: current.as_deref() == Some(key.as_str()),
                key,
                subject: s.subject,
                display: s.display_name,
                role: s.role.as_str(),
                via: via_label(s.via),
                client: s.client.unwrap_or_default(),
                last_seen: unix_text(s.last_seen_unix),
            })
            .collect(),
        Err(e) => {
            error = error.or_else(|| Some(error_text(&i18n, &e)));
            Vec::new()
        }
    };
    let page = UsersPage {
        status: crate::current_status(&ctx.state, ctx.rv.0, &ctx.rt),
        i18n,
        active_page: "settings-users",
        csrf_html: ctx.csrf.html(),
        users: users
            .into_iter()
            .map(|u| UserRow {
                display: u.display_name.clone().unwrap_or_default(),
                role: u.role.as_str().into(),
                disabled: u.disabled,
                revision: u.revision,
                last_login: u
                    .last_login_at
                    .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_default(),
                id: u.id,
                username: u.username,
            })
            .collect(),
        bindings,
        bindings_error,
        sessions,
        local_mode: c.mode() == ConsoleAuthMode::Local,
        min_password_length: c.settings().local.min_password_length,
        notice,
        one_time_password,
        error,
    };
    let mut response = crate::render(page);
    if response.status() == StatusCode::OK {
        *response.status_mut() = status;
    }
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

fn actor(ctx: &Ctx) -> String {
    ctx.resolved
        .as_ref()
        .map(|r| r.principal.subject.clone())
        .unwrap_or_else(|| "console".into())
}

macro_rules! ctx {
    ($state:ident, $locale:ident, $rv:ident, $rt:ident, $console:ident, $resolved:ident, $csrf:ident) => {{
        let Some(Extension(console)) = $console else {
            return crate::login::console_not_installed();
        };
        Ctx {
            state: $state,
            locale: $locale,
            rv: $rv,
            rt: $rt,
            console,
            resolved: $resolved.map(|r| r.0),
            csrf: $csrf,
        }
    }};
}

fn form(body: &[u8]) -> HashMap<String, String> {
    form_urlencoded::parse(body).into_owned().collect()
}

async fn outcome(ctx: &Ctx, result: Result<String, ConsoleError>, otp: Option<String>) -> Response {
    let i18n = I18n::new(ctx.locale);
    match result {
        Ok(notice) => render(ctx, Some(i18n.t(&notice)), otp, None, StatusCode::OK).await,
        Err(e) => {
            let status = match e {
                ConsoleError::NotFound => StatusCode::NOT_FOUND,
                ConsoleError::Invalid(_) | ConsoleError::Policy(_) => {
                    StatusCode::UNPROCESSABLE_ENTITY
                }
                ConsoleError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::CONFLICT,
            };
            render(ctx, None, None, Some(error_text(&i18n, &e)), status).await
        }
    }
}

type Console = Option<Extension<Arc<ConsoleAuth>>>;
type MaybeResolved = Option<Extension<Resolved>>;

/// `GET /ui/settings/users`.
pub(crate) async fn page(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    console: Console,
    resolved: MaybeResolved,
    csrf: CsrfField,
) -> Response {
    let ctx = ctx!(state, locale, rv, rt, console, resolved, csrf);
    render(&ctx, None, None, None, StatusCode::OK).await
}

/// `POST /ui/settings/users` — create an account.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn create(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    console: Console,
    resolved: MaybeResolved,
    csrf: CsrfField,
    body: axum::body::Bytes,
) -> Response {
    let ctx = ctx!(state, locale, rv, rt, console, resolved, csrf);
    let f = form(&body);
    let new = NewUser {
        username: f.get("username").cloned().unwrap_or_default(),
        display_name: f.get("displayName").cloned(),
        email: f.get("email").cloned(),
        role: f.get("role").and_then(|r| ConsoleRole::parse(r)),
        password: f.get("password").cloned().filter(|p| !p.is_empty()),
    };
    match ctx.console.create_user(&actor(&ctx), new).await {
        Ok((_, otp)) => outcome(&ctx, Ok("console-users-created".into()), otp).await,
        Err(e) => outcome(&ctx, Err(e), None).await,
    }
}

/// `POST /ui/settings/users/{id}` — role and disabled flag.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn update(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    console: Console,
    resolved: MaybeResolved,
    csrf: CsrfField,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let ctx = ctx!(state, locale, rv, rt, console, resolved, csrf);
    let f = form(&body);
    let revision = f.get("revision").and_then(|r| r.parse().ok()).unwrap_or(0);
    let patch = UserPatch {
        role: f.get("role").and_then(|r| ConsoleRole::parse(r)),
        disabled: Some(f.get("disabled").is_some_and(|v| v == "true")),
        ..Default::default()
    };
    let result = ctx
        .console
        .update_user(&actor(&ctx), &id, patch, revision)
        .await
        .map(|_| "console-users-updated".to_string());
    outcome(&ctx, result, None).await
}

/// `POST /ui/settings/users/{id}/reset-password`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn reset_password(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    console: Console,
    resolved: MaybeResolved,
    csrf: CsrfField,
    Path(id): Path<String>,
) -> Response {
    let ctx = ctx!(state, locale, rv, rt, console, resolved, csrf);
    match ctx.console.reset_password(&actor(&ctx), &id).await {
        Ok(p) => outcome(&ctx, Ok("console-users-reset-done".into()), Some(p)).await,
        Err(e) => outcome(&ctx, Err(e), None).await,
    }
}

/// `POST /ui/settings/users/{id}/delete`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn delete(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    console: Console,
    resolved: MaybeResolved,
    csrf: CsrfField,
    Path(id): Path<String>,
) -> Response {
    let ctx = ctx!(state, locale, rv, rt, console, resolved, csrf);
    let result = ctx
        .console
        .delete_user(&actor(&ctx), &id)
        .await
        .map(|_| "console-users-deleted".to_string());
    outcome(&ctx, result, None).await
}

/// `POST /ui/settings/sessions/{key}/revoke`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn revoke_session(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    console: Console,
    resolved: MaybeResolved,
    csrf: CsrfField,
    Path(key): Path<String>,
) -> Response {
    let ctx = ctx!(state, locale, rv, rt, console, resolved, csrf);
    let result = ctx
        .console
        .revoke_session(&actor(&ctx), &key)
        .await
        .map(|_| "console-sessions-revoked".to_string());
    outcome(&ctx, result, None).await
}

/// `POST /ui/settings/bindings` (form: `subject`, `endpoint`, `role`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn bind(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    console: Console,
    resolved: MaybeResolved,
    csrf: CsrfField,
    body: axum::body::Bytes,
) -> Response {
    let ctx = ctx!(state, locale, rv, rt, console, resolved, csrf);
    let f = form(&body);
    let subject = f
        .get("subject")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let endpoint = f
        .get("endpoint")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let Some(role) = f.get("role").and_then(|r| ConsoleRole::parse(r)) else {
        return outcome(&ctx, Err(ConsoleError::NotFound), None).await;
    };
    let result = ctx
        .console
        .store()
        .set_role_binding(&subject, &endpoint, role, &actor(&ctx))
        .await
        .map_err(ConsoleError::from)
        .map(|_| {
            ctx.console.invalidate_subject(&subject);
            "console-bindings-saved".to_string()
        });
    outcome(&ctx, result, None).await
}

/// `POST /ui/settings/bindings/delete` (form: `subject`, `endpoint`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn unbind(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    console: Console,
    resolved: MaybeResolved,
    csrf: CsrfField,
    body: axum::body::Bytes,
) -> Response {
    let ctx = ctx!(state, locale, rv, rt, console, resolved, csrf);
    let f = form(&body);
    let subject = f.get("subject").cloned().unwrap_or_default();
    let endpoint = f.get("endpoint").cloned().unwrap_or_default();
    let result = ctx
        .console
        .store()
        .delete_role_binding(&subject, &endpoint)
        .await
        .map_err(ConsoleError::from)
        .map(|_| {
            ctx.console.invalidate_subject(&subject);
            "console-bindings-saved".to_string()
        });
    outcome(&ctx, result, None).await
}
