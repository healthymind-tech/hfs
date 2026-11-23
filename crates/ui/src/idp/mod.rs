//! Authorization servers console (built-in IdP, design §11, package I6).
//!
//! * `/ui/idp` — every realm, drafts with "Resume";
//! * `/ui/idp/new` + `/ui/idp/new/{realm}/{step}` — the 7-step wizard
//!   ([`wizard`]), drafts saved server-side after every step;
//! * `/ui/idp/{realm}` — overview; `/clients`, `/users`, `/keys`,
//!   `/sessions`, `/settings`, `/test` — management ([`manage`]).
//!
//! Server-rendered over [`helios_idp_admin::service::IdpConsole`] (no
//! self-calls). Hidden (404, no nav entry) unless the server installed the
//! service (`HFS_IDP=on`). Every POST is a write behind the console's
//! administrator guard and CSRF check.

// Page handlers return `Result<_, Response>` (both arms are responses).
#![allow(clippy::result_large_err)]
// Page renderers take the request context pieces explicitly.
#![allow(clippy::too_many_arguments)]

pub(crate) mod manage;
pub(crate) mod security;
pub(crate) mod verify;
pub(crate) mod wizard;

use std::sync::Arc;

use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use helios_endpoints::{Actor, ConsolePrincipal};
use helios_idp_admin::service::{self, IdpConsole, OpError};

use crate::{RequestTenant, WebState};

/// The console pages (merged into the console router).
pub(crate) fn routes() -> Router<WebState> {
    Router::new()
        .route("/ui/idp", get(manage::list_page))
        .route(
            "/ui/idp/new",
            get(wizard::basics_page).post(wizard::basics_submit),
        )
        .route(
            "/ui/idp/new/{realm}/{step}",
            get(wizard::step_page).post(wizard::step_submit),
        )
        .route("/ui/idp/new/{realm}/preview", post(wizard::smart_preview))
        .route("/ui/idp/new/{realm}/discard", post(wizard::discard))
        .route("/ui/idp/{realm}", get(manage::overview_page))
        .route(
            "/ui/idp/{realm}/clients",
            get(manage::clients_page).post(manage::client_create),
        )
        .route(
            "/ui/idp/{realm}/clients/{client}",
            get(manage::client_page).post(manage::client_save),
        )
        .route(
            "/ui/idp/{realm}/clients/{client}/{action}",
            post(manage::client_action),
        )
        .route(
            "/ui/idp/{realm}/users",
            get(manage::users_page).post(manage::user_create),
        )
        .route(
            "/ui/idp/{realm}/users/{user}",
            get(manage::user_page).post(manage::user_save),
        )
        .route(
            "/ui/idp/{realm}/users/{user}/{action}",
            post(manage::user_action),
        )
        .route("/ui/idp/{realm}/keys", get(manage::keys_page))
        .route("/ui/idp/{realm}/keys/{action}", post(manage::keys_action))
        .route("/ui/idp/{realm}/sessions", get(manage::sessions_page))
        .route(
            "/ui/idp/{realm}/sessions/{kind}/{id}/revoke",
            post(manage::session_revoke),
        )
        .route(
            "/ui/idp/{realm}/settings",
            get(manage::settings_page).post(manage::settings_save),
        )
        .route(
            "/ui/idp/{realm}/settings/{action}",
            post(manage::settings_action),
        )
        .route(
            "/ui/idp/{realm}/test",
            get(manage::test_page).post(manage::test_run),
        )
        .route(
            "/ui/idp/{realm}/test/report.json",
            get(manage::test_report_json),
        )
}

/// Registers the sidebar section (called by the server when it installs
/// the service, i.e. only with `HFS_IDP=on`).
pub fn register_nav() {
    // One registry (ui-chrome, re-exported by the manager's nav module)
    // feeds the console sidebar.
    use crate::manager::nav::{NavItem, NavRole, NavSection, register};
    register(
        NavItem::new(
            NavSection::AuthorizationServers,
            "idp-realms",
            "idpc-nav-realms",
            "/ui/idp",
        )
        .icon("shield")
        .min_role(NavRole::Admin)
        .order(10),
    );
}

/// The installed service, or the "not enabled" page.
pub(crate) fn console() -> Result<Arc<IdpConsole>, Response> {
    service::global().ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            "The built-in authorization server is not enabled on this server (HFS_IDP=on).",
        )
            .into_response()
    })
}

/// The acting console user (audit / endpoint history).
pub(crate) fn actor(rt: &RequestTenant, principal: Option<&ConsolePrincipal>) -> Actor {
    if let Some(p) = principal {
        return Actor {
            subject: p.subject.clone(),
            display: Some(p.display_name.clone()),
            via: "console-session".into(),
        };
    }
    if let Some(user) = &rt.admin_user {
        return Actor {
            subject: "admin".into(),
            display: Some(user.clone()),
            via: "admin-password".into(),
        };
    }
    if let Some(s) = &rt.signed_in {
        return Actor {
            subject: format!("oidc:{}", s.subject),
            display: Some(s.display().to_string()),
            via: "console-session".into(),
        };
    }
    Actor {
        subject: "anonymous".into(),
        display: None,
        via: "console-session".into(),
    }
}

/// Every value of a repeated form field (checkbox groups).
pub(crate) fn values(body: &[u8], name: &str) -> Vec<String> {
    form_urlencoded::parse(body)
        .filter(|(k, _)| k == name)
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

/// `303 See Other` after a successful POST.
pub(crate) fn see_other(to: &str) -> Response {
    Redirect::to(to).into_response()
}

/// A flash message carried in the query string (`?done=key`), rendered as
/// a status line; keys only, never free text from the URL.
pub(crate) fn flash_key(done: Option<&str>) -> Option<String> {
    let k = done?;
    k.chars()
        .all(|c| c.is_ascii_lowercase() || c == '-')
        .then(|| format!("idpc-done-{k}"))
}

/// Errors of an operation as the form shows them: per field, plus a
/// form-level message.
#[derive(Debug, Default, Clone)]
pub(crate) struct Errors {
    pub fields: std::collections::BTreeMap<String, String>,
    pub form: Option<String>,
}

impl Errors {
    pub fn from_op(e: &OpError) -> Self {
        let mut s = Self::default();
        match &e.field {
            Some(f) if !f.is_empty() => {
                s.fields.insert(form_field(f), e.message.clone());
            }
            _ => s.form = Some(e.message.clone()),
        }
        s
    }
    pub fn field(mut self, f: &str, m: impl Into<String>) -> Self {
        self.fields.insert(f.to_string(), m.into());
        self
    }
    pub fn get(&self, f: &str) -> String {
        self.fields.get(f).cloned().unwrap_or_default()
    }
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty() && self.form.is_none()
    }
    /// Error-summary entries: (field id, message).
    pub fn summary(&self) -> Vec<(String, String)> {
        self.fields
            .iter()
            .map(|(k, v)| (format!("f-{k}"), v.clone()))
            .collect()
    }
}

/// Service field path → the form input's name (`features.backendServices`
/// → `features.backend_services`, the toggle's id `f-features.backend_services`).
pub(crate) fn form_field(f: &str) -> String {
    match f.strip_prefix("features.") {
        Some(rest) => {
            let mut out = String::from("features.");
            for ch in rest.chars() {
                if ch.is_ascii_uppercase() {
                    out.push('_');
                    out.push(ch.to_ascii_lowercase());
                } else {
                    out.push(ch);
                }
            }
            out
        }
        None => f.to_string(),
    }
}

/// A rendered page or a plain error.
pub(crate) fn op_status(e: &OpError) -> StatusCode {
    StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// `Content-Disposition` download of JSON.
pub(crate) fn json_download(name: &str, v: &impl serde::Serialize) -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/json".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        serde_json::to_string_pretty(v).unwrap_or_default(),
    )
        .into_response()
}

/// Human labels (Fluent keys) for enum values shown in the console — never
/// raw keys (UX audit).
pub(crate) mod labels {
    use helios_idp::client::ClientAuth;
    use helios_idp::keys::KeyStatus;
    use helios_idp::user::UserKind;

    pub fn client_kind(c: &helios_idp::client::ClientSpec) -> &'static str {
        use helios_idp::client::LaunchType;
        if c.launch_types.contains(&LaunchType::Backend) {
            return "idpc-client-kind-backend";
        }
        match c.auth {
            ClientAuth::None => "idpc-client-kind-public",
            ClientAuth::ClientSecret { .. } => "idpc-client-kind-secret",
            ClientAuth::PrivateKeyJwt { .. } => "idpc-client-kind-key",
            ClientAuth::TlsClientAuth { .. } => "idpc-client-kind-mtls",
        }
    }

    pub fn user_kind(k: UserKind) -> &'static str {
        match k {
            UserKind::Patient => "idpc-user-kind-patient",
            UserKind::Practitioner => "idpc-user-kind-practitioner",
            UserKind::RelatedPerson => "idpc-user-kind-related-person",
            UserKind::Staff => "idpc-user-kind-staff",
        }
    }

    pub fn key_status(s: KeyStatus) -> (&'static str, &'static str) {
        match s {
            KeyStatus::Next => ("idpc-key-status-next", "info"),
            KeyStatus::Active => ("idpc-key-status-active", "ok"),
            KeyStatus::Retiring => ("idpc-key-status-retiring", "warn"),
            KeyStatus::Retired => ("idpc-key-status-retired", "muted"),
        }
    }

    pub fn endpoint_state(before: &str) -> &'static str {
        match before {
            "inherit" => "idpc-ep-state-inherit",
            "off" => "idpc-ep-state-off",
            "external" => "idpc-ep-state-external",
            _ if before.starts_with("internal:") => "idpc-ep-state-internal",
            _ => "idpc-ep-state-external",
        }
    }
}
