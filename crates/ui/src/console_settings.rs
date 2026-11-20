//! Settings → Authentication (`/ui/settings/auth`, admin; package P3).
//!
//! The console sign-in mode, OIDC client, local policy, session lifetimes
//! and break-glass switch, saved through
//! [`ConsoleAuth::save_settings`] — the same rules as the admin REST API
//! (`PUT /admin/console/settings`): env-pinned fields cannot change, a
//! switch to `local` needs an enabled admin account and a switch to `oidc`
//! a passed "Test sign-in" of the saved OIDC client in this admin's session.

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::Extension;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use helios_admin_auth::console::settings::field;
use helios_admin_auth::console::{ConsoleAuth, ConsoleError, OidcSwitchCheck, Resolved};
use helios_endpoints::SecretInput;
use helios_endpoints::console::{
    ConsoleAuthMode, ConsoleAuthSettings, ConsoleRole, OidcClientSettings, RoleMapping,
};
use serde::Deserialize;

use crate::account::error_text;
use crate::csrf::CsrfField;
use crate::{I18n, RequestLocale, RequestTenant, RequestVersion, Status, WebState};

/// Form values as strings.
#[derive(Debug, Default, Clone)]
pub(crate) struct FormView {
    pub(crate) mode: String,
    pub(crate) anonymous_role: String,
    pub(crate) break_glass: bool,
    pub(crate) default_role: String,
    pub(crate) allowed_origins: String,
}

#[derive(Template)]
#[template(path = "pages/settings-auth.html")]
struct SettingsAuthPage {
    status: Status,
    i18n: I18n,
    active_page: &'static str,
    csrf_html: String,
    form: FormView,
    modes: Vec<(&'static str, &'static str)>,
    oidc_fields: Vec<(&'static str, &'static str, &'static str, String)>,
    number_fields: Vec<(&'static str, &'static str, &'static str, String)>,
    pinned_fields: Vec<&'static str>,
    revision: u64,
    secret_set: bool,
    key_ring: bool,
    oidc_configured: bool,
    test_passed: bool,
    break_glass_available: bool,
    persistent: bool,
    notice: Option<String>,
    error: Option<String>,
    field_errors: Vec<(String, String)>,
}

impl SettingsAuthPage {
    fn pinned(&self, f: &str) -> bool {
        self.pinned_fields.contains(&f)
    }
}

fn role_str(r: Option<ConsoleRole>) -> String {
    r.map(|r| r.as_str().to_string())
        .unwrap_or_else(|| "none".into())
}

fn mode_str(m: ConsoleAuthMode) -> &'static str {
    match m {
        ConsoleAuthMode::None => "none",
        ConsoleAuthMode::Oidc => "oidc",
        ConsoleAuthMode::Local => "local",
    }
}

struct PageInput {
    settings: ConsoleAuthSettings,
    notice: Option<String>,
    error: Option<String>,
    field_errors: Vec<(String, String)>,
    status: StatusCode,
}

#[allow(clippy::too_many_arguments)]
fn render(
    state: &WebState,
    locale: RequestLocale,
    rv: &RequestVersion,
    rt: &RequestTenant,
    console: &ConsoleAuth,
    resolved: Option<&Resolved>,
    csrf: &CsrfField,
    input: PageInput,
) -> Response {
    let s = &input.settings;
    let o = s.oidc.clone();
    let get = |f: fn(&OidcClientSettings) -> String| o.as_ref().map(f).unwrap_or_default();
    let (_, revision) = console.stored_settings();
    let fingerprint = console.oidc_fingerprint();
    let page = SettingsAuthPage {
        status: crate::current_status(state, rv.0, rt),
        i18n: I18n::new(locale),
        active_page: "settings-auth",
        csrf_html: csrf.html(),
        form: FormView {
            mode: mode_str(s.mode).into(),
            anonymous_role: role_str(s.anonymous_role),
            break_glass: s.break_glass,
            default_role: role_str(o.as_ref().and_then(|o| o.default_role)),
            allowed_origins: s.allowed_origins.join(", "),
        },
        modes: vec![
            ("none", "console-mode-none"),
            ("oidc", "console-mode-oidc"),
            ("local", "console-mode-local"),
        ],
        oidc_fields: vec![
            (field::OIDC_ISSUER, "console-settings-oidc-issuer", "HFS_CONSOLE_OIDC_ISSUER", get(|o| o.issuer.clone())),
            (field::OIDC_CLIENT_ID, "console-settings-oidc-client-id", "HFS_CONSOLE_OIDC_CLIENT_ID", get(|o| o.client_id.clone())),
            (field::OIDC_SCOPES, "console-settings-oidc-scopes", "HFS_CONSOLE_OIDC_SCOPES", o.as_ref().map(|o| o.scopes.clone()).unwrap_or_else(|| "openid profile email".into())),
            (field::OIDC_ROLE_CLAIM, "console-settings-oidc-role-claim", "HFS_CONSOLE_OIDC_ROLE_CLAIM", o.as_ref().map(|o| o.role_claim.clone()).unwrap_or_else(|| "roles".into())),
            (field::OIDC_ADMIN_VALUES, "console-settings-oidc-admin-values", "HFS_CONSOLE_OIDC_ADMIN_VALUES", get(|o| o.role_mapping.admin.join(", "))),
            (field::OIDC_EDITOR_VALUES, "console-settings-oidc-editor-values", "HFS_CONSOLE_OIDC_EDITOR_VALUES", get(|o| o.role_mapping.editor.join(", "))),
            (field::OIDC_VIEWER_VALUES, "console-settings-oidc-viewer-values", "HFS_CONSOLE_OIDC_VIEWER_VALUES", get(|o| o.role_mapping.viewer.join(", "))),
        ],
        number_fields: vec![
            (field::MIN_PASSWORD_LENGTH, "console-settings-min-length", "HFS_CONSOLE_PASSWORD_MIN_LENGTH", s.local.min_password_length.to_string()),
            (field::LOCKOUT_FAILURES, "console-settings-lockout-failures", "HFS_CONSOLE_LOCKOUT_FAILURES", s.local.lockout_failures.to_string()),
            (field::LOCKOUT_MINUTES, "console-settings-lockout-minutes", "HFS_CONSOLE_LOCKOUT_MINUTES", s.local.lockout_minutes.to_string()),
            (field::SESSION_IDLE, "console-settings-session-idle", "HFS_CONSOLE_SESSION_IDLE_MINUTES", s.session_idle_minutes.to_string()),
            (field::SESSION_ABSOLUTE, "console-settings-session-absolute", "HFS_CONSOLE_SESSION_ABSOLUTE_HOURS", s.session_absolute_hours.to_string()),
        ],
        pinned_fields: console.env().pinned.iter().copied().collect(),
        revision,
        secret_set: console.env().client_secret.is_some()
            || o.as_ref().is_some_and(|o| o.client_secret.is_some()),
        key_ring: console.options().key_ring.is_some(),
        oidc_configured: console.settings().oidc.is_some(),
        test_passed: fingerprint.is_some()
            && resolved.and_then(|r| r.session.oidc_test_passed.clone()) == fingerprint,
        break_glass_available: console.break_glass_available(),
        persistent: console.options().persistent,
        notice: input.notice,
        error: input.error,
        field_errors: input.field_errors,
    };
    let mut response = crate::render(page);
    if response.status() == StatusCode::OK {
        *response.status_mut() = input.status;
    }
    response
}

#[derive(Deserialize, Default)]
pub(crate) struct SettingsQuery {
    #[serde(default)]
    saved: Option<String>,
    #[serde(default)]
    tested: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    role: Option<String>,
}

/// `GET /ui/settings/auth`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn page(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    console: Option<Extension<Arc<ConsoleAuth>>>,
    resolved: Option<Extension<Resolved>>,
    csrf: CsrfField,
    Query(q): Query<SettingsQuery>,
) -> Response {
    let Some(Extension(console)) = console else {
        return crate::login::console_not_installed();
    };
    console.reload().await;
    let i18n = I18n::new(locale);
    let user = q.user.clone().unwrap_or_default();
    let notice = match (q.saved.as_deref(), q.tested.as_deref()) {
        (Some(_), _) => Some(i18n.t("console-settings-saved")),
        (_, Some("ok")) => Some(i18n.t_arg2(
            "console-settings-test-ok",
            "user",
            user,
            "role",
            q.role.clone().unwrap_or_default(),
        )),
        (_, Some(_)) => Some(i18n.t_arg("console-settings-test-no-role", "user", user)),
        _ => None,
    };
    // Show the stored values for unpinned fields (what a save would keep).
    let (stored, _) = console.stored_settings();
    let settings = console.env().apply(stored.as_ref());
    render(
        &state,
        locale,
        &rv,
        &rt,
        &console,
        resolved.as_ref().map(|r| &r.0),
        &csrf,
        PageInput {
            settings,
            notice,
            error: None,
            field_errors: Vec::new(),
            status: StatusCode::OK,
        },
    )
}

fn list(raw: &str) -> Vec<String> {
    raw.split([',', ' ', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_role(raw: Option<&String>) -> Option<ConsoleRole> {
    raw.and_then(|r| ConsoleRole::parse(r.trim()))
}

/// Builds the proposed settings from the form; pinned (disabled, so
/// absent) fields keep their effective value.
pub(crate) fn settings_from_form(
    form: &HashMap<String, String>,
    current: &ConsoleAuthSettings,
) -> (ConsoleAuthSettings, SecretInput, Vec<(String, String)>) {
    let mut s = current.clone();
    let mut errors = Vec::new();
    if let Some(m) = form.get(field::MODE) {
        s.mode = match m.as_str() {
            "oidc" => ConsoleAuthMode::Oidc,
            "local" => ConsoleAuthMode::Local,
            _ => ConsoleAuthMode::None,
        };
    }
    if let Some(r) = form.get(field::ANONYMOUS_ROLE) {
        s.anonymous_role = ConsoleRole::parse(r);
    }
    // A checkbox is absent when unchecked; it is also absent when pinned
    // (disabled), so only read it when the form carried the mode radio
    // group or anything else from the same form.
    if form.contains_key("revision") {
        s.break_glass = form.get(field::BREAK_GLASS).is_some_and(|v| v == "true");
    }
    let issuer = form.get(field::OIDC_ISSUER).map(|v| v.trim().to_string());
    let client = form.get(field::OIDC_CLIENT_ID).map(|v| v.trim().to_string());
    let has_oidc_input = issuer.as_deref().is_some_and(|v| !v.is_empty())
        || client.as_deref().is_some_and(|v| !v.is_empty())
        || current.oidc.is_some();
    if has_oidc_input {
        let mut o = current.oidc.clone().unwrap_or_else(|| OidcClientSettings {
            issuer: String::new(),
            client_id: String::new(),
            client_secret: None,
            scopes: "openid profile email".into(),
            authorization_endpoint: None,
            token_endpoint: None,
            end_session_endpoint: None,
            role_claim: "roles".into(),
            role_mapping: RoleMapping::default(),
            default_role: None,
        });
        if let Some(v) = issuer {
            o.issuer = v;
        }
        if let Some(v) = client {
            o.client_id = v;
        }
        if let Some(v) = form.get(field::OIDC_SCOPES) {
            o.scopes = v.trim().to_string();
        }
        if let Some(v) = form.get(field::OIDC_ROLE_CLAIM) {
            o.role_claim = v.trim().to_string();
        }
        if let Some(v) = form.get(field::OIDC_ADMIN_VALUES) {
            o.role_mapping.admin = list(v);
        }
        if let Some(v) = form.get(field::OIDC_EDITOR_VALUES) {
            o.role_mapping.editor = list(v);
        }
        if let Some(v) = form.get(field::OIDC_VIEWER_VALUES) {
            o.role_mapping.viewer = list(v);
        }
        if form.contains_key(field::OIDC_DEFAULT_ROLE) {
            o.default_role = parse_role(form.get(field::OIDC_DEFAULT_ROLE));
        }
        // An emptied issuer and client id remove the OIDC client.
        if o.issuer.is_empty() && o.client_id.is_empty() {
            s.oidc = None;
        } else {
            s.oidc = Some(o);
        }
    }
    macro_rules! num {
        ($f:expr, $slot:expr) => {
            if let Some(v) = form.get($f) {
                match v.trim().parse() {
                    Ok(n) => $slot = n,
                    Err(_) => errors.push(($f.to_string(), "not a number".to_string())),
                }
            }
        };
    }
    num!(field::MIN_PASSWORD_LENGTH, s.local.min_password_length);
    num!(field::LOCKOUT_FAILURES, s.local.lockout_failures);
    num!(field::LOCKOUT_MINUTES, s.local.lockout_minutes);
    num!(field::SESSION_IDLE, s.session_idle_minutes);
    num!(field::SESSION_ABSOLUTE, s.session_absolute_hours);
    if let Some(v) = form.get(field::ALLOWED_ORIGINS) {
        s.allowed_origins = list(v);
    }
    let secret = match (
        form.get(field::OIDC_CLIENT_SECRET).filter(|v| !v.is_empty()),
        form.get("oidc.clientSecret.clear").is_some_and(|v| v == "true"),
    ) {
        (Some(v), _) => SecretInput::Set(v.clone()),
        (None, true) => SecretInput::Clear,
        (None, false) => SecretInput::Keep,
    };
    (s, secret, errors)
}

/// `POST /ui/settings/auth`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn save(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    console: Option<Extension<Arc<ConsoleAuth>>>,
    resolved: Option<Extension<Resolved>>,
    csrf: CsrfField,
    body: axum::body::Bytes,
) -> Response {
    let Some(Extension(console)) = console else {
        return crate::login::console_not_installed();
    };
    let form: HashMap<String, String> = form_urlencoded::parse(&body).into_owned().collect();
    console.reload().await;
    let (stored, _) = console.stored_settings();
    let current = console.env().apply(stored.as_ref());
    let (proposed, secret, mut field_errors) = settings_from_form(&form, &current);
    let i18n = I18n::new(locale);
    let actor = resolved
        .as_ref()
        .map(|r| r.0.principal.subject.clone())
        .unwrap_or_else(|| "console".into());
    let expected: u64 = form
        .get("revision")
        .and_then(|r| r.parse().ok())
        .unwrap_or(0);
    let tested = resolved
        .as_ref()
        .and_then(|r| r.0.session.oidc_test_passed.clone());
    let fail = |error: Option<String>, field_errors: Vec<(String, String)>, settings| {
        render(
            &state,
            locale,
            &rv,
            &rt,
            &console,
            resolved.as_ref().map(|r| &r.0),
            &csrf,
            PageInput {
                settings,
                notice: None,
                error,
                field_errors,
                status: StatusCode::UNPROCESSABLE_ENTITY,
            },
        )
    };
    if !field_errors.is_empty() {
        return fail(None, field_errors, proposed);
    }
    match console
        .save_settings(
            &actor,
            proposed.clone(),
            secret,
            expected,
            OidcSwitchCheck::Tested(tested),
        )
        .await
    {
        Ok(_) => Redirect::to("/ui/settings/auth?saved=1").into_response(),
        Err(ConsoleError::Invalid(errors)) => {
            field_errors.extend(
                errors
                    .into_iter()
                    .map(|e| (e.field.to_string(), e.message)),
            );
            fail(None, field_errors, proposed)
        }
        Err(e) => fail(Some(error_text(&i18n, &e)), Vec::new(), proposed),
    }
}

/// `POST /ui/settings/auth/test` — a "Test sign-in" round trip with the
/// saved OIDC client, in this browser; the result is recorded in the
/// admin's session.
pub(crate) async fn test_sign_in(
    console: Option<Extension<Arc<ConsoleAuth>>>,
    resolved: Option<Extension<Resolved>>,
    headers: HeaderMap,
) -> Response {
    let Some(Extension(console)) = console else {
        return crate::login::console_not_installed();
    };
    let Some(Extension(resolved)) = resolved else {
        return Redirect::to("/ui/login?local=1").into_response();
    };
    match console
        .begin_oidc(&headers, "/ui/settings/auth", Some(&resolved.key))
        .await
    {
        Ok((cookie, url)) => {
            let mut r = Redirect::to(&url).into_response();
            r.headers_mut().append(axum::http::header::SET_COOKIE, cookie);
            r
        }
        Err(e) => {
            let q = form_urlencoded::Serializer::new(String::new())
                .append_pair("tested", "error")
                .append_pair("user", &e)
                .finish();
            Redirect::to(&format!("/ui/settings/auth?{q}")).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn form_maps_to_settings() {
        let current = ConsoleAuthSettings::default();
        let (s, secret, errors) = settings_from_form(
            &form(&[
                ("revision", "3"),
                ("mode", "oidc"),
                ("anonymousRole", "none"),
                ("oidc.issuer", " https://idp.example/realms/r "),
                ("oidc.clientId", "hfs-web"),
                ("oidc.scopes", "openid profile"),
                ("oidc.roleClaim", "realm_access.roles"),
                ("oidc.roleMapping.admin", "hfs-admin, ops"),
                ("oidc.defaultRole", "viewer"),
                ("oidc.clientSecret", "s3"),
                ("sessionIdleMinutes", "60"),
                ("allowedOrigins", "https://a.example, https://b.example"),
            ]),
            &current,
        );
        assert!(errors.is_empty());
        assert_eq!(s.mode, ConsoleAuthMode::Oidc);
        assert_eq!(s.anonymous_role, None);
        assert!(!s.break_glass, "unchecked box");
        let o = s.oidc.unwrap();
        assert_eq!(o.issuer, "https://idp.example/realms/r");
        assert_eq!(o.role_mapping.admin, vec!["hfs-admin", "ops"]);
        assert_eq!(o.default_role, Some(ConsoleRole::Viewer));
        assert_eq!(s.session_idle_minutes, 60);
        assert_eq!(s.allowed_origins.len(), 2);
        assert_eq!(secret, SecretInput::Set("s3".into()));

        // Pinned fields are disabled and so absent: the current value stays.
        let (s2, secret, _) = settings_from_form(&form(&[("revision", "1"), ("breakGlass", "true")]), &s_with(&current));
        assert_eq!(s2.mode, ConsoleAuthMode::Local);
        assert!(s2.break_glass);
        assert_eq!(secret, SecretInput::Keep);
        let (_, secret, errors) = settings_from_form(
            &form(&[("oidc.clientSecret.clear", "true"), ("sessionIdleMinutes", "x")]),
            &current,
        );
        assert_eq!(secret, SecretInput::Clear);
        assert_eq!(errors[0].0, "sessionIdleMinutes");
    }

    fn s_with(c: &ConsoleAuthSettings) -> ConsoleAuthSettings {
        let mut s = c.clone();
        s.mode = ConsoleAuthMode::Local;
        s
    }
}
