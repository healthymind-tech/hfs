//! `/ui/account` — the signed-in console user's identity, role and (for
//! built-in accounts) password change, which is forced on the first sign-in
//! of an account an admin created (package P3).

use std::sync::Arc;

use askama::Template;
use axum::Extension;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use helios_admin_auth::console::{ConsoleAuth, ConsoleError, Resolved};
use helios_endpoints::console::ConsoleVia;
use serde::Deserialize;

use crate::auth_shell::{AuthShell, Banner};
use crate::csrf::CsrfField;
use crate::{I18n, RequestLocale, RequestTenant, RequestVersion, Status, WebState};

#[derive(Template)]
#[template(path = "pages/account.html")]
struct AccountPage {
    status: Status,
    i18n: I18n,
    active_page: &'static str,
    csrf_html: String,
    display_name: String,
    subject: String,
    role: &'static str,
    via: &'static str,
    endpoint_roles: Vec<(String, &'static str)>,
    local: bool,
    must_change: bool,
    min_password_length: u8,
    notice: Option<String>,
    error: Option<String>,
    /// The shared "show password" button.
    reveal_html: String,
}

/// The forced password change on the first sign-in of an account an admin
/// created: a standalone page (the shell's navigation would lead nowhere
/// until the password is changed — the gate sends every page here).
#[derive(Template)]
#[template(path = "pages/password-change.html")]
struct PasswordChangePage {
    i18n: I18n,
    auth: AuthShell,
    csrf_html: String,
    username: String,
    min_password_length: u8,
    /// A problem not tied to one field.
    error: Option<Banner>,
    /// The field an error belongs to (`current`, `new`, `confirm`) and its text.
    field: &'static str,
    field_text: String,
}

/// Which field of the password form an error belongs to, and its text.
fn classify(i18n: &I18n, e: &ConsoleError) -> (&'static str, String) {
    match e {
        ConsoleError::Policy(_) => ("new", error_text(i18n, e)),
        ConsoleError::Invalid(errors) => match errors.first().map(|e| e.field) {
            Some("currentPassword") => ("current", i18n.t("auth-current-wrong")),
            Some("newPassword") => ("new", i18n.t("auth-password-same")),
            _ => ("", error_text(i18n, e)),
        },
        _ => ("", error_text(i18n, e)),
    }
}

/// `via` as shown.
pub(crate) fn via_label(via: ConsoleVia) -> &'static str {
    match via {
        ConsoleVia::Anonymous => "anonymous",
        ConsoleVia::Local => "local account",
        ConsoleVia::Oidc => "OpenID Connect",
        ConsoleVia::BreakGlass => "break-glass (HFS_ADMIN_PASSWORD)",
    }
}

/// Maps a console error to a localized message.
pub(crate) fn error_text(i18n: &I18n, e: &ConsoleError) -> String {
    match e {
        ConsoleError::Policy(p) => match p {
            helios_admin_auth::console::PolicyViolation::TooShort(n) => {
                i18n.t_arg(p.message_id(), "min", *n as i64)
            }
            _ => i18n.t(p.message_id()),
        },
        ConsoleError::Conflict(_) => i18n.t("console-error-conflict"),
        ConsoleError::Unavailable(d) => {
            i18n.t_arg("console-error-unavailable", "detail", d.clone())
        }
        ConsoleError::Invalid(errors) => match errors.first().map(|e| e.field) {
            Some("currentPassword") => i18n.t("auth-current-wrong"),
            Some("newPassword") => i18n.t("auth-password-same"),
            _ => e.to_string(),
        },
        other => other.to_string(),
    }
}

#[derive(Deserialize, Default)]
pub(crate) struct AccountQuery {
    #[serde(default)]
    changed: Option<String>,
}

struct Ctx<'a> {
    state: &'a WebState,
    locale: RequestLocale,
    rv: &'a RequestVersion,
    rt: &'a RequestTenant,
    headers: &'a HeaderMap,
}

/// What went wrong on a password change: the field (if any) and the text.
struct Problem {
    field: &'static str,
    text: String,
}

fn render(
    ctx: Ctx<'_>,
    console: &ConsoleAuth,
    resolved: &Resolved,
    csrf: &CsrfField,
    notice: Option<String>,
    error: Option<Problem>,
    status: StatusCode,
) -> Response {
    let p = &resolved.principal;
    if resolved.session.must_change_password {
        let i18n = I18n::new(ctx.locale);
        let (field, field_text, banner) = match error {
            Some(Problem { field: "", text }) => ("", String::new(), Some(Banner::danger(text))),
            Some(Problem { field, text }) => (field, text, None),
            None => ("", String::new(), None),
        };
        let page = PasswordChangePage {
            auth: AuthShell::new(&i18n, Some(ctx.headers), ""),
            csrf_html: csrf.html(),
            username: resolved
                .session
                .subject
                .strip_prefix("local:")
                .unwrap_or(&p.display_name)
                .to_string(),
            min_password_length: console.settings().local.min_password_length,
            error: banner,
            field,
            field_text,
            i18n,
        };
        let mut response = crate::render(page);
        if response.status() == StatusCode::OK {
            *response.status_mut() = status;
        }
        return response;
    }
    let error = error.map(|e| e.text);
    let page = AccountPage {
        status: crate::current_status(ctx.state, ctx.rv.0, ctx.rt),
        i18n: I18n::new(ctx.locale),
        active_page: "account",
        csrf_html: csrf.html(),
        display_name: p.display_name.clone(),
        subject: p.subject.clone(),
        role: p.role.as_str(),
        via: via_label(p.via),
        endpoint_roles: p
            .endpoint_roles
            .iter()
            .map(|(e, r)| (e.clone(), r.as_str()))
            .collect(),
        local: p.via == ConsoleVia::Local,
        must_change: resolved.session.must_change_password,
        min_password_length: console.settings().local.min_password_length,
        notice,
        error,
        reveal_html: helios_ui_chrome::password_toggle(&I18n::new(ctx.locale)).unwrap_or_default(),
    };
    let mut response = crate::render(page);
    if response.status() == StatusCode::OK {
        *response.status_mut() = status;
    }
    response
}

/// `GET /ui/account` and `GET /ui/account/password`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn page(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    console: Option<Extension<Arc<ConsoleAuth>>>,
    resolved: Option<Extension<Resolved>>,
    csrf: CsrfField,
    headers: HeaderMap,
    Query(q): Query<AccountQuery>,
) -> Response {
    let Some(Extension(console)) = console else {
        return crate::login::console_not_installed();
    };
    let Some(Extension(resolved)) = resolved else {
        return Redirect::to("/ui/login").into_response();
    };
    let i18n = I18n::new(locale);
    let notice = q.changed.map(|_| i18n.t("console-account-changed"));
    render(
        Ctx {
            state: &state,
            locale,
            rv: &rv,
            rt: &rt,
            headers: &headers,
        },
        &console,
        &resolved,
        &csrf,
        notice,
        None,
        StatusCode::OK,
    )
}

/// `POST /ui/account/password` (form: `current`, `new`, `confirm`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn change_password(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    console: Option<Extension<Arc<ConsoleAuth>>>,
    resolved: Option<Extension<Resolved>>,
    csrf: CsrfField,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Some(Extension(console)) = console else {
        return crate::login::console_not_installed();
    };
    let Some(Extension(resolved)) = resolved else {
        return Redirect::to("/ui/login").into_response();
    };
    let mut current = String::new();
    let mut new = String::new();
    let mut confirm = String::new();
    for (k, v) in form_urlencoded::parse(&body) {
        match k.as_ref() {
            "current" => current = v.into_owned(),
            "new" => new = v.into_owned(),
            "confirm" => confirm = v.into_owned(),
            _ => {}
        }
    }
    let i18n = I18n::new(locale);
    let ctx = Ctx {
        state: &state,
        locale,
        rv: &rv,
        rt: &rt,
        headers: &headers,
    };
    if new != confirm {
        return render(
            ctx,
            &console,
            &resolved,
            &csrf,
            None,
            Some(Problem {
                field: "confirm",
                text: i18n.t("console-account-mismatch"),
            }),
            StatusCode::UNPROCESSABLE_ENTITY,
        );
    }
    match console.change_own_password(&resolved, &current, &new).await {
        Ok(()) => Redirect::to("/ui/account?changed=1").into_response(),
        Err(e) => {
            let (field, text) = classify(&i18n, &e);
            render(
                ctx,
                &console,
                &resolved,
                &csrf,
                None,
                Some(Problem { field, text }),
                StatusCode::UNPROCESSABLE_ENTITY,
            )
        }
    }
}
