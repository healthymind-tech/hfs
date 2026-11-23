//! Management pages of a realm: list, overview, clients, users, keys
//! (rotation, emergency rotation), sessions & grants (revoke), settings
//! (capabilities, lifetimes, security, endpoint binding, enable/disable,
//! delete) and the diagnostic ("Test").

use std::collections::HashMap;

use askama::Template;
use axum::Extension;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use helios_endpoints::ConsolePrincipal;
use helios_idp::client::{ClientAuth, ClientSpec};
use helios_idp::keys::KeyStatus;
use helios_idp::metadata::RealmUrls;
use helios_idp::realm::UserSource;
use helios_idp::store::{GrantFilter, RealmRecord};
use helios_idp::user::RealmUser;
use helios_idp_admin::binding::BindMode;
use helios_idp_admin::diagnostic::AppProbe;
use helios_idp_admin::service::{
    DIAGNOSTIC_PREFIX, DiagnoseOptions, IdpConsole, RealmSummary, WizardState,
};
use serde::Deserialize;

use super::verify::{self, VerifyView};
use super::wizard::{self, NumField, Opt, ToggleGroup};
use super::{Errors, actor, console, flash_key, labels, see_other, values};
use crate::i18n::{I18n, RequestLocale};
use crate::manager::forms::Form;
use crate::{RequestTenant, RequestVersion, WebState, current_status, render};

#[derive(Deserialize, Default)]
pub(crate) struct Done {
    done: Option<String>,
}

/// Header + tabs shared by every realm page.
pub(crate) struct RealmHead {
    pub name: String,
    pub display_name: String,
    pub issuer: String,
    pub enabled: bool,
    pub draft_step: Option<u8>,
    /// Active tab id.
    pub tab: &'static str,
}

pub(crate) const TABS: [(&str, &str, &str); 7] = [
    ("overview", "idpc-tab-overview", ""),
    ("clients", "idpc-tab-clients", "/clients"),
    ("users", "idpc-tab-users", "/users"),
    ("keys", "idpc-tab-keys", "/keys"),
    ("sessions", "idpc-tab-sessions", "/sessions"),
    ("settings", "idpc-tab-settings", "/settings"),
    ("test", "idpc-tab-test", "/test"),
];

fn head(c: &IdpConsole, rec: &RealmRecord, tab: &'static str) -> RealmHead {
    RealmHead {
        name: rec.spec.name.as_str().to_string(),
        display_name: rec.spec.display_name.clone(),
        issuer: c.issuer(&rec.spec),
        enabled: rec.spec.enabled,
        draft_step: WizardState::of(&rec.spec).map(|w| w.step),
        tab,
    }
}

macro_rules! ctx {
    ($c:ident, $realm:expr) => {{
        let $c = match console() {
            Ok(c) => c,
            Err(r) => return r,
        };
        match $c.realm(&$realm).await {
            Ok(rec) => ($c, rec),
            Err(e) => return (super::op_status(&e), e.message).into_response(),
        }
    }};
}

// ---------------------------------------------------------------- list

#[derive(Template)]
#[template(path = "pages/idp/list.html")]
pub(crate) struct ListPage {
    pub status: crate::Status,
    pub i18n: I18n,
    pub active_page: &'static str,
    pub rows: Vec<RealmSummary>,
    pub keys_available: bool,
    pub flash: Option<String>,
    pub error: Option<String>,
}

impl ListPage {
    pub fn status_look(&self, r: &RealmSummary) -> (&'static str, &'static str, String) {
        if let Some(d) = &r.draft {
            return (
                "accent",
                "pencil",
                self.i18n.t_arg("idpc-status-draft", "step", d.step as i64),
            );
        }
        if !r.enabled {
            return ("muted", "power", self.i18n.t("idpc-status-disabled"));
        }
        match r.last_status {
            Some(s) => {
                let (tone, icon, key) = verify::look(s);
                (tone, icon, self.i18n.t(&format!("{key}-short")))
            }
            None => ("info", "info", self.i18n.t("idpc-status-not-tested")),
        }
    }
}

pub(crate) async fn list_page(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Query(q): Query<Done>,
) -> Response {
    let c = match console() {
        Ok(c) => c,
        Err(r) => return r,
    };
    let (rows, error) = match c.summaries().await {
        Ok(r) => (r, None),
        Err(e) => (vec![], Some(e.message)),
    };
    render(ListPage {
        status: current_status(&st, rv.0, &rt),
        i18n: I18n::new(locale),
        active_page: "idp-realms",
        rows,
        keys_available: c.keys_available(),
        flash: flash_key(q.done.as_deref()),
        error,
    })
}

// ---------------------------------------------------------------- overview

pub(crate) struct LinkRow {
    pub label_key: &'static str,
    pub url: String,
}

#[derive(Template)]
#[template(path = "pages/idp/overview.html")]
pub(crate) struct OverviewPage {
    pub status: crate::Status,
    pub i18n: I18n,
    pub active_page: &'static str,
    pub head: RealmHead,
    pub links: Vec<LinkRow>,
    pub endpoints: Vec<(String, String)>,
    pub clients: usize,
    pub users: usize,
    pub active_keys: usize,
    pub sessions: usize,
    pub capabilities: Vec<String>,
    pub verify: Option<VerifyView>,
    pub flash: Option<String>,
}

pub(crate) async fn overview_page(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path(realm): Path<String>,
    Query(q): Query<Done>,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    if let Some(w) = WizardState::of(&rec.spec) {
        return see_other(&format!("/ui/idp/new/{realm}/{}", w.step.max(2)));
    }
    let i18n = I18n::new(locale);
    let issuer = c.issuer(&rec.spec);
    let urls = RealmUrls::new(&issuer);
    let origin = reqwest::Url::parse(&issuer)
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_default();
    let views = c.endpoint_views().await;
    let endpoints = rec
        .spec
        .endpoints
        .iter()
        .map(|e| {
            (
                e.clone(),
                views
                    .iter()
                    .find(|v| &v.name == e)
                    .map(|v| v.base_url.clone())
                    .unwrap_or_default(),
            )
        })
        .collect();
    let keys = c.admin.list_keys(&rec.id).await.unwrap_or_default();
    let clients = c
        .admin
        .list_clients(&rec.id)
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| !r.spec.client_id.starts_with(DIAGNOSTIC_PREFIX))
        .count();
    let users = c
        .admin
        .list_users(&rec.id)
        .await
        .unwrap_or_default()
        .iter()
        .filter(|u| !u.username.starts_with(DIAGNOSTIC_PREFIX))
        .count();
    let now = Utc::now();
    let sessions = c
        .admin
        .list_sessions(&rec.id, None)
        .await
        .unwrap_or_default()
        .iter()
        .filter(|s| s.alive(rec.spec.lifetimes.sso_session_idle, now))
        .count();
    let verify = c
        .last_report(&realm)
        .map(|r| verify::view(&i18n, &r, false));
    render(OverviewPage {
        status: current_status(&st, rv.0, &rt),
        i18n,
        active_page: "idp-realms",
        head: head(&c, &rec, "overview"),
        links: vec![
            LinkRow {
                label_key: "idpc-link-issuer",
                url: issuer.clone(),
            },
            LinkRow {
                label_key: "idpc-link-oidc",
                url: format!("{issuer}/.well-known/openid-configuration"),
            },
            LinkRow {
                label_key: "idpc-link-rfc8414",
                url: format!(
                    "{origin}/.well-known/oauth-authorization-server{}",
                    reqwest::Url::parse(&issuer)
                        .map(|u| u.path().to_string())
                        .unwrap_or_default()
                ),
            },
            LinkRow {
                label_key: "idpc-link-jwks",
                url: urls.jwks.clone(),
            },
            LinkRow {
                label_key: "idpc-link-authorize",
                url: urls.authorization.clone(),
            },
            LinkRow {
                label_key: "idpc-link-token",
                url: urls.token.clone(),
            },
        ],
        endpoints,
        clients,
        users,
        active_keys: keys
            .iter()
            .filter(|k| k.status == KeyStatus::Active)
            .count(),
        sessions,
        capabilities: helios_idp::metadata::smart_capabilities(&rec.spec)
            .into_iter()
            .map(String::from)
            .collect(),
        verify,
        flash: flash_key(q.done.as_deref()),
    })
}

// ---------------------------------------------------------------- clients

#[derive(Template)]
#[template(path = "pages/idp/clients.html")]
pub(crate) struct ClientsPage {
    pub status: crate::Status,
    pub i18n: I18n,
    pub active_page: &'static str,
    pub head: RealmHead,
    pub rows: Vec<wizard::ClientRow>,
    pub client_kinds: Vec<Opt>,
    pub scope_presets: Vec<Opt>,
    pub errors: Errors,
    pub form: HashMap<String, String>,
    pub new_secret: Option<(String, String)>,
    pub flash: Option<String>,
    /// The add form is open (after an error).
    pub open_form: bool,
}

impl ClientsPage {
    pub fn v(&self, k: &str) -> &str {
        self.form.get(k).map(String::as_str).unwrap_or("")
    }
    pub fn err(&self, k: &str) -> String {
        self.errors.get(k)
    }
}

async fn client_rows(c: &IdpConsole, realm_id: &str) -> Vec<wizard::ClientRow> {
    c.admin
        .list_clients(realm_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.spec)
        .filter(|s| !s.client_id.starts_with(DIAGNOSTIC_PREFIX))
        .map(|s| wizard::ClientRow {
            kind_key: labels::client_kind(&s),
            redirects: s.redirect_uris.join("\n"),
            disabled: s.disabled,
            client_id: s.client_id,
            name: s.name,
        })
        .collect()
}

pub(crate) async fn clients_page(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path(realm): Path<String>,
    Query(q): Query<Done>,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    let i18n = I18n::new(locale);
    render(ClientsPage {
        status: current_status(&st, rv.0, &rt),
        i18n,
        active_page: "idp-realms",
        head: head(&c, &rec, "clients"),
        rows: client_rows(&c, &rec.id).await,
        client_kinds: kinds("public"),
        scope_presets: wizard::scope_presets(&rec.spec, &[]),
        errors: Errors::default(),
        form: HashMap::new(),
        new_secret: None,
        flash: flash_key(q.done.as_deref()),
        open_form: false,
    })
}

fn kinds(sel: &str) -> Vec<Opt> {
    ["public", "secret", "key", "backend"]
        .iter()
        .map(|k| Opt {
            value: k.to_string(),
            label_key: format!("idpc-client-kind-{k}"),
            hint_key: format!("idpc-client-kind-{k}-hint"),
            checked: *k == sel,
        })
        .collect()
}

pub(crate) async fn client_create(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path(realm): Path<String>,
    body: Bytes,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    let f = Form::parse(&body);
    let mut errs = Errors::default();
    let mut secret = None;
    if let Some(spec) = wizard::client_from_form(&body, &f, &mut errs) {
        match c.admin.create_client(&rec.id, spec).await {
            Ok(created) => {
                secret = created
                    .client_secret
                    .map(|s| (created.record.spec.client_id.clone(), s));
                if secret.is_none() {
                    return see_other(&format!("/ui/idp/{realm}/clients?done=client-created"));
                }
            }
            Err(e) => {
                let mut e = helios_idp_admin::service::OpError::from(e);
                e.field = e.field.map(|x| wizard::client_field(&x));
                errs.fields.extend(Errors::from_op(&e).fields);
                if errs.fields.is_empty() {
                    errs.form = Some(e.message);
                }
            }
        }
    }
    let failed = !errs.is_empty();
    let mut form = HashMap::new();
    if failed {
        for (k, v) in form_urlencoded::parse(&body) {
            form.insert(k.into_owned(), v.into_owned());
        }
    }
    let i18n = I18n::new(locale);
    let mut r = render(ClientsPage {
        status: current_status(&st, rv.0, &rt),
        i18n,
        active_page: "idp-realms",
        head: head(&c, &rec, "clients"),
        rows: client_rows(&c, &rec.id).await,
        client_kinds: kinds(if failed {
            f.get("client_kind")
        } else {
            "public"
        }),
        scope_presets: wizard::scope_presets(
            &rec.spec,
            &if failed {
                values(&body, "scope")
            } else {
                vec![]
            },
        ),
        errors: errs,
        form,
        new_secret: secret,
        flash: None,
        open_form: failed,
    });
    if failed {
        *r.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
    }
    r
}

#[derive(Template)]
#[template(path = "pages/idp/client.html")]
pub(crate) struct ClientPage {
    pub status: crate::Status,
    pub i18n: I18n,
    pub active_page: &'static str,
    pub head: RealmHead,
    pub client_id: String,
    pub kind_key: &'static str,
    pub confidential_secret: bool,
    pub uses_key: bool,
    pub form: HashMap<String, String>,
    pub launch_ehr: bool,
    pub launch_standalone: bool,
    pub scope_presets: Vec<Opt>,
    pub revision: i64,
    pub errors: Errors,
    pub new_secret: Option<String>,
    pub flash: Option<String>,
}

impl ClientPage {
    pub fn v(&self, k: &str) -> &str {
        self.form.get(k).map(String::as_str).unwrap_or("")
    }
    pub fn err(&self, k: &str) -> String {
        self.errors.get(k)
    }
}

fn client_form(spec: &ClientSpec) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("name".into(), spec.name.clone());
    m.insert("redirectUris".into(), spec.redirect_uris.join("\n"));
    let presets = [
        "openid",
        "fhirUser",
        "offline_access",
        "launch",
        "launch/patient",
        "patient/*.rs",
        "patient/*.cruds",
        "user/*.rs",
        "user/*.cruds",
        "system/*.rs",
    ];
    m.insert(
        "scopes_custom".into(),
        spec.scopes
            .allowed
            .iter()
            .filter(|s| !presets.contains(&s.as_str()))
            .cloned()
            .collect::<Vec<_>>()
            .join(" "),
    );
    if let ClientAuth::PrivateKeyJwt { jwks_uri, jwks, .. } = &spec.auth {
        m.insert("jwks_uri".into(), jwks_uri.clone().unwrap_or_default());
        m.insert(
            "jwks".into(),
            jwks.as_ref()
                .map(|j| serde_json::to_string_pretty(j).unwrap_or_default())
                .unwrap_or_default(),
        );
    }
    if spec.disabled {
        m.insert("disabled".into(), "on".into());
    }
    if spec.trusted {
        m.insert("trusted".into(), "on".into());
    }
    m
}

async fn render_client(
    st: &WebState,
    locale: RequestLocale,
    rv: &RequestVersion,
    rt: &RequestTenant,
    c: &IdpConsole,
    rec: &RealmRecord,
    client: &str,
    errors: Errors,
    form_override: Option<HashMap<String, String>>,
    secret: Option<String>,
    flash: Option<String>,
) -> Response {
    let Some(cr) = c
        .admin
        .list_clients(&rec.id)
        .await
        .unwrap_or_default()
        .into_iter()
        .find(|r| r.spec.client_id == client)
    else {
        return (StatusCode::NOT_FOUND, "no such client").into_response();
    };
    let s = &cr.spec;
    let failed = !errors.is_empty();
    let mut r = render(ClientPage {
        status: current_status(st, rv.0, rt),
        i18n: I18n::new(locale),
        active_page: "idp-realms",
        head: head(c, rec, "clients"),
        client_id: s.client_id.clone(),
        kind_key: labels::client_kind(s),
        confidential_secret: matches!(s.auth, ClientAuth::ClientSecret { .. }),
        uses_key: matches!(s.auth, ClientAuth::PrivateKeyJwt { .. }),
        form: form_override.unwrap_or_else(|| client_form(s)),
        launch_ehr: s
            .launch_types
            .contains(&helios_idp::client::LaunchType::Ehr),
        launch_standalone: s
            .launch_types
            .contains(&helios_idp::client::LaunchType::Standalone),
        scope_presets: wizard::scope_presets(&rec.spec, &s.scopes.allowed),
        revision: cr.revision,
        errors,
        new_secret: secret,
        flash,
    });
    if failed {
        *r.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
    }
    r
}

pub(crate) async fn client_page(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path((realm, client)): Path<(String, String)>,
    Query(q): Query<Done>,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    render_client(
        &st,
        locale,
        &rv,
        &rt,
        &c,
        &rec,
        &client,
        Errors::default(),
        None,
        None,
        flash_key(q.done.as_deref()),
    )
    .await
}

pub(crate) async fn client_save(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path((realm, client)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    let f = Form::parse(&body);
    let Some(cur) = c
        .admin
        .list_clients(&rec.id)
        .await
        .unwrap_or_default()
        .into_iter()
        .find(|r| r.spec.client_id == client)
    else {
        return (StatusCode::NOT_FOUND, "no such client").into_response();
    };
    let mut spec = cur.spec.clone();
    spec.name = f.get("name").to_string();
    spec.redirect_uris = f.list("redirectUris");
    let launch = values(&body, "launch");
    if !spec
        .launch_types
        .contains(&helios_idp::client::LaunchType::Backend)
    {
        spec.launch_types = launch
            .iter()
            .filter_map(|l| match l.as_str() {
                "ehr" => Some(helios_idp::client::LaunchType::Ehr),
                "standalone" => Some(helios_idp::client::LaunchType::Standalone),
                _ => None,
            })
            .collect();
    }
    let mut scopes = values(&body, "scope");
    scopes.extend(
        f.list("scopes_custom")
            .into_iter()
            .flat_map(|l| l.split_whitespace().map(String::from).collect::<Vec<_>>()),
    );
    scopes.sort();
    scopes.dedup();
    spec.scopes.allowed = scopes;
    spec.trusted = f.checked("trusted");
    spec.disabled = f.checked("disabled");
    let mut errs = Errors::default();
    if let ClientAuth::PrivateKeyJwt { jwks_uri, jwks, .. } = &mut spec.auth {
        *jwks_uri = f.opt("jwks_uri");
        *jwks = match f.opt("jwks").map(|j| serde_json::from_str(&j)) {
            Some(Ok(v)) => Some(v),
            Some(Err(_)) => {
                errs.fields.insert("jwks".into(), "not valid JSON".into());
                None
            }
            None => None,
        };
    }
    let rev = f.get("revision").parse().unwrap_or(cur.revision);
    if errs.is_empty() {
        match c.admin.update_client(&rec.id, spec, rev).await {
            Ok(_) => return see_other(&format!("/ui/idp/{realm}/clients/{client}?done=saved")),
            Err(e) => errs = Errors::from_op(&helios_idp_admin::service::OpError::from(e)),
        }
    }
    let mut form = HashMap::new();
    for (k, v) in form_urlencoded::parse(&body) {
        form.insert(k.into_owned(), v.into_owned());
    }
    render_client(
        &st,
        locale,
        &rv,
        &rt,
        &c,
        &rec,
        &client,
        errs,
        Some(form),
        None,
        None,
    )
    .await
}

pub(crate) async fn client_action(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path((realm, client, action)): Path<(String, String, String)>,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    match action.as_str() {
        "delete" => match c.admin.delete_client(&rec.id, &client).await {
            Ok(_) => see_other(&format!("/ui/idp/{realm}/clients?done=client-deleted")),
            Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        },
        "reset-secret" => match c.admin.reset_client_secret(&rec.id, &client).await {
            Ok(created) => {
                render_client(
                    &st,
                    locale,
                    &rv,
                    &rt,
                    &c,
                    &rec,
                    &client,
                    Errors::default(),
                    None,
                    created.client_secret,
                    None,
                )
                .await
            }
            Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        },
        _ => (StatusCode::NOT_FOUND, "unknown action").into_response(),
    }
}

// ---------------------------------------------------------------- users

pub(crate) struct UserRowFull {
    pub id: String,
    pub username: String,
    pub display: String,
    pub kind_key: &'static str,
    pub fhir_user: String,
    pub disabled: bool,
}

#[derive(Template)]
#[template(path = "pages/idp/users.html")]
pub(crate) struct UsersPage {
    pub status: crate::Status,
    pub i18n: I18n,
    pub active_page: &'static str,
    pub head: RealmHead,
    pub source_key: &'static str,
    pub local: bool,
    pub rows: Vec<UserRowFull>,
    pub endpoint_names: Vec<String>,
    pub errors: Errors,
    pub form: HashMap<String, String>,
    pub flash: Option<String>,
    pub open_form: bool,
    pub password_min: u32,
}

impl UsersPage {
    pub fn is_v(&self, k: &str, val: &str) -> bool {
        self.v(k) == val
    }
    pub fn v(&self, k: &str) -> &str {
        self.form.get(k).map(String::as_str).unwrap_or("")
    }
    pub fn err(&self, k: &str) -> String {
        self.errors.get(k)
    }
}

fn source_key(s: &UserSource) -> &'static str {
    match s {
        UserSource::Local => "idpc-source-local",
        UserSource::Console => "idpc-source-console",
        UserSource::UpstreamOidc { .. } => "idpc-source-upstream",
    }
}

fn user_row(u: RealmUser) -> UserRowFull {
    UserRowFull {
        fhir_user: u
            .links
            .iter()
            .find(|l| l.is_fhir_user)
            .map(|l| format!("{} · {}", l.endpoint, l.reference))
            .unwrap_or_default(),
        display: u.display_name.clone().unwrap_or_default(),
        kind_key: labels::user_kind(u.kind),
        disabled: u.disabled,
        id: u.id,
        username: u.username,
    }
}

async fn users_render(
    st: &WebState,
    locale: RequestLocale,
    rv: &RequestVersion,
    rt: &RequestTenant,
    c: &IdpConsole,
    rec: &RealmRecord,
    errors: Errors,
    form: HashMap<String, String>,
    flash: Option<String>,
) -> Response {
    let failed = !errors.is_empty();
    let rows = c
        .admin
        .list_users(&rec.id)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|u| !u.username.starts_with(DIAGNOSTIC_PREFIX))
        .map(user_row)
        .collect();
    let mut r = render(UsersPage {
        status: current_status(st, rv.0, rt),
        i18n: I18n::new(locale),
        active_page: "idp-realms",
        head: head(c, rec, "users"),
        source_key: source_key(&rec.spec.user_source),
        local: matches!(rec.spec.user_source, UserSource::Local),
        rows,
        endpoint_names: c
            .endpoint_views()
            .await
            .into_iter()
            .map(|e| e.name)
            .collect(),
        errors,
        form,
        flash,
        open_form: failed,
        password_min: rec.spec.security.password_min_length,
    });
    if failed {
        *r.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
    }
    r
}

pub(crate) async fn users_page(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path(realm): Path<String>,
    Query(q): Query<Done>,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    users_render(
        &st,
        locale,
        &rv,
        &rt,
        &c,
        &rec,
        Errors::default(),
        HashMap::new(),
        flash_key(q.done.as_deref()),
    )
    .await
}

pub(crate) async fn user_create(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path(realm): Path<String>,
    body: Bytes,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    let f = Form::parse(&body);
    match c
        .admin
        .create_user(
            &rec.id,
            wizard::user_from_form(&f),
            f.opt("user_password").as_deref(),
            f.checked("user_must_change"),
        )
        .await
    {
        Ok(_) => see_other(&format!("/ui/idp/{realm}/users?done=user-created")),
        Err(e) => {
            let mut e = helios_idp_admin::service::OpError::from(e);
            e.field = e.field.map(|x| wizard::user_field(&x));
            let mut form = HashMap::new();
            for (k, v) in form_urlencoded::parse(&body) {
                if !k.contains("password") {
                    form.insert(k.into_owned(), v.into_owned());
                }
            }
            users_render(
                &st,
                locale,
                &rv,
                &rt,
                &c,
                &rec,
                Errors::from_op(&e),
                form,
                None,
            )
            .await
        }
    }
}

pub(crate) struct SessionRow {
    pub id: String,
    pub user: String,
    pub started: String,
    pub last_seen: String,
    pub alive: bool,
}

pub(crate) struct GrantRow {
    pub id: String,
    pub client: String,
    pub user: String,
    pub scopes: String,
    pub created: String,
    pub expires: String,
    pub active: bool,
}

#[derive(Template)]
#[template(path = "pages/idp/user.html")]
pub(crate) struct UserPage {
    pub status: crate::Status,
    pub i18n: I18n,
    pub active_page: &'static str,
    pub head: RealmHead,
    pub user: UserRowFull,
    pub form: HashMap<String, String>,
    pub endpoint_names: Vec<String>,
    pub sessions: Vec<SessionRow>,
    pub grants: Vec<GrantRow>,
    pub errors: Errors,
    pub flash: Option<String>,
    pub password_min: u32,
}

impl UserPage {
    pub fn is_v(&self, k: &str, val: &str) -> bool {
        self.v(k) == val
    }
    pub fn back(&self) -> String {
        format!("/ui/idp/{}/users/{}", self.head.name, self.user.id)
    }
    pub fn v(&self, k: &str) -> &str {
        self.form.get(k).map(String::as_str).unwrap_or("")
    }
    pub fn err(&self, k: &str) -> String {
        self.errors.get(k)
    }
}

async fn sessions_of(
    c: &IdpConsole,
    rec: &RealmRecord,
    user: Option<&str>,
) -> (Vec<SessionRow>, Vec<GrantRow>) {
    let now = Utc::now();
    let users: HashMap<String, String> = c
        .admin
        .list_users(&rec.id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|u| (u.id, u.username))
        .collect();
    let name = |id: &str| users.get(id).cloned().unwrap_or_else(|| id.to_string());
    let sessions = c
        .admin
        .list_sessions(&rec.id, user)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.revoked_at.is_none() && s.expires_at > now)
        .filter(|s| !name(&s.user_id).starts_with(DIAGNOSTIC_PREFIX))
        .map(|s| SessionRow {
            alive: s.alive(rec.spec.lifetimes.sso_session_idle, now),
            id: s.id_hash.clone(),
            user: name(&s.user_id),
            started: s.created_at.format("%Y-%m-%d %H:%M UTC").to_string(),
            last_seen: s.last_seen_at.format("%Y-%m-%d %H:%M UTC").to_string(),
        })
        .collect();
    let f = GrantFilter {
        subject: user.map(String::from),
        client_id: None,
        include_revoked: false,
        limit: Some(500),
    };
    let grants = c
        .admin
        .list_grants(&rec.id, &f)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|g| !g.client_id.starts_with(DIAGNOSTIC_PREFIX))
        .map(|g| GrantRow {
            active: g.active(now),
            id: g.id.clone(),
            client: g.client_id.clone(),
            user: name(&g.subject),
            scopes: g.scopes.join(" "),
            created: g.created_at.format("%Y-%m-%d %H:%M UTC").to_string(),
            expires: g.expires_at.format("%Y-%m-%d %H:%M UTC").to_string(),
        })
        .collect();
    (sessions, grants)
}

async fn render_user(
    st: &WebState,
    locale: RequestLocale,
    rv: &RequestVersion,
    rt: &RequestTenant,
    c: &IdpConsole,
    rec: &RealmRecord,
    user_id: &str,
    errors: Errors,
    flash: Option<String>,
) -> Response {
    let Ok(Some(u)) = c.admin.runtime().store().get_user(&rec.id, user_id).await else {
        return (StatusCode::NOT_FOUND, "no such user").into_response();
    };
    let mut form = HashMap::new();
    form.insert(
        "user_display".into(),
        u.display_name.clone().unwrap_or_default(),
    );
    form.insert("user_email".into(), u.email.clone().unwrap_or_default());
    form.insert(
        "user_kind".into(),
        serde_json::to_value(u.kind)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default(),
    );
    if let Some(l) = u.links.iter().find(|l| l.is_fhir_user) {
        form.insert("user_endpoint".into(), l.endpoint.clone());
        form.insert("user_reference".into(), l.reference.clone());
    }
    let (sessions, grants) = sessions_of(c, rec, Some(user_id)).await;
    let failed = !errors.is_empty();
    let mut r = render(UserPage {
        status: current_status(st, rv.0, rt),
        i18n: I18n::new(locale),
        active_page: "idp-realms",
        head: head(c, rec, "users"),
        user: user_row(u),
        form,
        endpoint_names: c
            .endpoint_views()
            .await
            .into_iter()
            .map(|e| e.name)
            .collect(),
        sessions,
        grants,
        errors,
        flash,
        password_min: rec.spec.security.password_min_length,
    });
    if failed {
        *r.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
    }
    r
}

pub(crate) async fn user_page(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path((realm, user)): Path<(String, String)>,
    Query(q): Query<Done>,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    render_user(
        &st,
        locale,
        &rv,
        &rt,
        &c,
        &rec,
        &user,
        Errors::default(),
        flash_key(q.done.as_deref()),
    )
    .await
}

pub(crate) async fn user_save(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path((realm, user)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    let f = Form::parse(&body);
    let Ok(Some(mut u)) = c.admin.runtime().store().get_user(&rec.id, &user).await else {
        return (StatusCode::NOT_FOUND, "no such user").into_response();
    };
    let edited = wizard::user_from_form(&f);
    u.display_name = edited.display_name;
    u.email = edited.email;
    u.kind = edited.kind;
    u.links.retain(|l| !l.is_fhir_user);
    u.links.extend(edited.links);
    u.disabled = f.checked("disabled");
    match c.admin.update_user(&rec.id, u).await {
        Ok(_) => see_other(&format!("/ui/idp/{realm}/users/{user}?done=saved")),
        Err(e) => {
            let mut e = helios_idp_admin::service::OpError::from(e);
            e.field = e.field.map(|x| wizard::user_field(&x));
            render_user(
                &st,
                locale,
                &rv,
                &rt,
                &c,
                &rec,
                &user,
                Errors::from_op(&e),
                None,
            )
            .await
        }
    }
}

pub(crate) async fn user_action(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path((realm, user, action)): Path<(String, String, String)>,
    body: Bytes,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    let f = Form::parse(&body);
    match action.as_str() {
        "password" => match c
            .admin
            .set_password(
                &rec.id,
                &user,
                f.get("user_password"),
                f.checked("user_must_change"),
            )
            .await
        {
            Ok(()) => see_other(&format!("/ui/idp/{realm}/users/{user}?done=password-set")),
            Err(e) => {
                let mut e = helios_idp_admin::service::OpError::from(e);
                e.field = e.field.map(|x| wizard::user_field(&x));
                render_user(
                    &st,
                    locale,
                    &rv,
                    &rt,
                    &c,
                    &rec,
                    &user,
                    Errors::from_op(&e),
                    None,
                )
                .await
            }
        },
        "delete" => match c.admin.delete_user(&rec.id, &user).await {
            Ok(_) => see_other(&format!("/ui/idp/{realm}/users?done=user-deleted")),
            Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        },
        _ => (StatusCode::NOT_FOUND, "unknown action").into_response(),
    }
}

// ---------------------------------------------------------------- keys

pub(crate) struct KeyRowFull {
    pub kid: String,
    pub alg: String,
    pub status_key: &'static str,
    pub tone: &'static str,
    pub created: String,
    pub activated: String,
    pub until: String,
    pub age_days: i64,
}

#[derive(Template)]
#[template(path = "pages/idp/keys.html")]
pub(crate) struct KeysPage {
    pub status: crate::Status,
    pub i18n: I18n,
    pub active_page: &'static str,
    pub head: RealmHead,
    pub rows: Vec<KeyRowFull>,
    pub retired: usize,
    pub rotate_days: u32,
    pub prepublish_hours: u32,
    pub flash: Option<String>,
    pub error: Option<String>,
}

pub(crate) async fn keys_page(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path(realm): Path<String>,
    Query(q): Query<Done>,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    keys_render(
        &st,
        locale,
        &rv,
        &rt,
        &c,
        &rec,
        flash_key(q.done.as_deref()),
        None,
    )
    .await
}

async fn keys_render(
    st: &WebState,
    locale: RequestLocale,
    rv: &RequestVersion,
    rt: &RequestTenant,
    c: &IdpConsole,
    rec: &RealmRecord,
    flash: Option<String>,
    error: Option<String>,
) -> Response {
    let keys = c.admin.list_keys(&rec.id).await.unwrap_or_default();
    let now = Utc::now();
    let fmt = |t: Option<chrono::DateTime<Utc>>| {
        t.map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "—".into())
    };
    let mut rows: Vec<KeyRowFull> = keys
        .iter()
        .filter(|k| k.status.published())
        .map(|k| {
            let (status_key, tone) = labels::key_status(k.status);
            KeyRowFull {
                kid: k.kid.clone(),
                alg: k.alg.as_str().into(),
                status_key,
                tone,
                created: fmt(Some(k.created_at)),
                activated: fmt(k.activate_at),
                until: fmt(k.expire_at),
                age_days: (now - k.activate_at.unwrap_or(k.created_at)).num_days(),
            }
        })
        .collect();
    rows.sort_by(|a, b| a.alg.cmp(&b.alg).then(a.status_key.cmp(b.status_key)));
    render(KeysPage {
        status: current_status(st, rv.0, rt),
        i18n: I18n::new(locale),
        active_page: "idp-realms",
        head: head(c, rec, "keys"),
        rows,
        retired: keys
            .iter()
            .filter(|k| k.status == KeyStatus::Retired)
            .count(),
        rotate_days: rec.spec.rotation.rotate_after_days,
        prepublish_hours: rec.spec.rotation.prepublish_hours,
        flash,
        error,
    })
}

pub(crate) async fn keys_action(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path((realm, action)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    let f = Form::parse(&body);
    let r = match action.as_str() {
        "rotate" => c.admin.rotate_keys(&rec.id).await.map(|_| "keys-rotated"),
        "promote" => c
            .admin
            .run_rotation(
                &rec.id,
                Utc::now() + chrono::Duration::hours(rec.spec.rotation.prepublish_hours as i64 + 1),
            )
            .await
            .map(|_| "keys-promoted"),
        "emergency" => {
            if f.get("confirm") != rec.spec.name.as_str() {
                return keys_render(
                    &st,
                    locale,
                    &rv,
                    &rt,
                    &c,
                    &rec,
                    None,
                    Some(I18n::new(locale).t("idpc-keys-emergency-confirm-mismatch")),
                )
                .await;
            }
            c.admin
                .emergency_rotate(&rec.id)
                .await
                .map(|_| "keys-emergency")
        }
        _ => return (StatusCode::NOT_FOUND, "unknown action").into_response(),
    };
    match r {
        Ok(k) => see_other(&format!("/ui/idp/{realm}/keys?done={k}")),
        Err(e) => keys_render(&st, locale, &rv, &rt, &c, &rec, None, Some(e.to_string())).await,
    }
}

// ---------------------------------------------------------------- sessions & grants

#[derive(Template)]
#[template(path = "pages/idp/sessions.html")]
pub(crate) struct SessionsPage {
    pub status: crate::Status,
    pub i18n: I18n,
    pub active_page: &'static str,
    pub head: RealmHead,
    pub sessions: Vec<SessionRow>,
    pub grants: Vec<GrantRow>,
    pub flash: Option<String>,
}

impl SessionsPage {
    pub fn back(&self) -> String {
        format!("/ui/idp/{}/sessions", self.head.name)
    }
}

pub(crate) async fn sessions_page(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path(realm): Path<String>,
    Query(q): Query<Done>,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    let (sessions, grants) = sessions_of(&c, &rec, None).await;
    render(SessionsPage {
        status: current_status(&st, rv.0, &rt),
        i18n: I18n::new(locale),
        active_page: "idp-realms",
        head: head(&c, &rec, "sessions"),
        sessions,
        grants,
        flash: flash_key(q.done.as_deref()),
    })
}

pub(crate) async fn session_revoke(
    Path((realm, kind, id)): Path<(String, String, String)>,
    body: Bytes,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    let back = Form::parse(&body)
        .opt("back")
        .filter(|b| b.starts_with("/ui/idp/"))
        .unwrap_or_else(|| format!("/ui/idp/{realm}/sessions"));
    let r = match kind.as_str() {
        "session" => c
            .admin
            .revoke_session(&rec.id, &id)
            .await
            .map(|_| "session-revoked"),
        "grant" => c
            .admin
            .revoke_grant(&rec.id, &id)
            .await
            .map(|_| "grant-revoked"),
        "all-sessions" => {
            let now = Utc::now();
            let mut n = Ok("sessions-revoked");
            for s in c
                .admin
                .list_sessions(&rec.id, None)
                .await
                .unwrap_or_default()
            {
                if s.revoked_at.is_none()
                    && s.expires_at > now
                    && let Err(e) = c.admin.revoke_session(&rec.id, &s.id_hash).await
                {
                    n = Err(e);
                }
            }
            n
        }
        _ => return (StatusCode::NOT_FOUND, "unknown").into_response(),
    };
    match r {
        Ok(k) => {
            let sep = if back.contains('?') { '&' } else { '?' };
            see_other(&format!("{back}{sep}done={k}"))
        }
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------- settings

pub(crate) struct BoundRow {
    pub name: String,
    pub base_url: String,
    pub state_key: &'static str,
    pub state_detail: String,
    pub bound_here: bool,
    pub listed: bool,
}

#[derive(Template)]
#[template(path = "pages/idp/settings.html")]
pub(crate) struct SettingsPage {
    pub status: crate::Status,
    pub i18n: I18n,
    pub active_page: &'static str,
    pub head: RealmHead,
    pub revision: i64,
    pub display_name: String,
    pub groups: Vec<ToggleGroup>,
    pub lifetimes: Vec<NumField>,
    pub password_min: String,
    pub lockout: String,
    pub rotate_days: String,
    pub endpoints: Vec<BoundRow>,
    pub errors: Errors,
    pub flash: Option<String>,
}

impl SettingsPage {
    pub fn err(&self, k: &str) -> String {
        self.errors.get(k)
    }
}

async fn settings_render(
    st: &WebState,
    locale: RequestLocale,
    rv: &RequestVersion,
    rt: &RequestTenant,
    c: &IdpConsole,
    rec: &RealmRecord,
    errors: Errors,
    flash: Option<String>,
) -> Response {
    let s = &rec.spec;
    let mut eps = vec![];
    for v in c.endpoint_views().await {
        let d = c
            .bind_diff(s.name.as_str(), &v.name, BindMode::Keep)
            .await
            .ok();
        let before = d
            .as_ref()
            .map(|d| d.before.clone())
            .unwrap_or_else(|| "inherit".into());
        eps.push(BoundRow {
            bound_here: before == format!("internal:{}", s.name.as_str()),
            listed: s.endpoints.contains(&v.name),
            state_key: labels::endpoint_state(&before),
            state_detail: if before.starts_with("internal:") {
                before.trim_start_matches("internal:").to_string()
            } else {
                d.and_then(|d| d.before_issuer).unwrap_or_default()
            },
            name: v.name,
            base_url: v.base_url,
        });
    }
    let failed = !errors.is_empty();
    let mut r = render(SettingsPage {
        status: current_status(st, rv.0, rt),
        i18n: I18n::new(locale),
        active_page: "idp-realms",
        head: head(c, rec, "settings"),
        revision: rec.revision,
        display_name: s.display_name.clone(),
        groups: wizard::feature_groups(s),
        lifetimes: wizard::lifetime_fields(s),
        password_min: s.security.password_min_length.to_string(),
        lockout: s.security.lockout_threshold.to_string(),
        rotate_days: s.rotation.rotate_after_days.to_string(),
        endpoints: eps,
        errors,
        flash,
    });
    if failed {
        *r.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
    }
    r
}

pub(crate) async fn settings_page(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path(realm): Path<String>,
    Query(q): Query<Done>,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    settings_render(
        &st,
        locale,
        &rv,
        &rt,
        &c,
        &rec,
        Errors::default(),
        flash_key(q.done.as_deref()),
    )
    .await
}

pub(crate) async fn settings_save(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path(realm): Path<String>,
    body: Bytes,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    let f = Form::parse(&body);
    let mut errs = Errors::default();
    let mut spec = rec.spec.clone();
    spec.display_name = f.get("display_name").to_string();
    if spec.display_name.is_empty() {
        errs.fields
            .insert("display_name".into(), "enter a display name".into());
    }
    wizard::apply_capabilities(&mut spec, &f, &mut errs);
    for (field, target) in [
        ("security.passwordMinLength", 0u8),
        ("security.lockoutThreshold", 1),
        ("rotation.rotateAfterDays", 2),
    ] {
        let v = f.get(field);
        if v.is_empty() {
            continue;
        }
        match v.parse::<u32>() {
            Ok(n) => match target {
                0 => spec.security.password_min_length = n,
                1 => spec.security.lockout_threshold = n,
                _ => spec.rotation.rotate_after_days = n,
            },
            Err(_) => {
                errs.fields
                    .insert(field.into(), "enter a whole number".into());
            }
        }
    }
    let rev: Option<i64> = f.get("revision").parse().ok();
    if errs.is_empty() {
        match c
            .update(&realm, rev, move |s| {
                *s = spec;
                Ok(())
            })
            .await
        {
            Ok(_) => return see_other(&format!("/ui/idp/{realm}/settings?done=saved")),
            Err(e) => errs = Errors::from_op(&e),
        }
    }
    let rec = c.realm(&realm).await.unwrap_or(rec);
    settings_render(&st, locale, &rv, &rt, &c, &rec, errs, None).await
}

pub(crate) async fn settings_action(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    principal: Option<Extension<ConsolePrincipal>>,
    Path((realm, action)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    let f = Form::parse(&body);
    let who = actor(&rt, principal.as_ref().map(|p| &p.0));
    let r: Result<&str, helios_idp_admin::service::OpError> = match action.as_str() {
        "bind" => c
            .bind(
                &realm,
                f.get("endpoint"),
                BindMode::parse(f.get("bind_mode")),
                &who,
            )
            .await
            .map(|_| "endpoint-bound"),
        "unbind" => c
            .unbind(&realm, f.get("endpoint"), &who)
            .await
            .map(|_| "endpoint-unbound"),
        "enable" | "disable" => {
            let on = action == "enable";
            c.update(&realm, None, |s| {
                s.enabled = on;
                Ok(())
            })
            .await
            .map(|_| if on { "enabled" } else { "disabled" })
        }
        "delete" => {
            if f.get("confirm") != realm {
                let e = Errors::default().field(
                    "confirm",
                    I18n::new(locale).t("idpc-delete-confirm-mismatch"),
                );
                return settings_render(&st, locale, &rv, &rt, &c, &rec, e, None).await;
            }
            return match c.delete_realm(&realm, &who).await {
                Ok(()) => see_other("/ui/idp?done=realm-deleted"),
                Err(e) => {
                    settings_render(&st, locale, &rv, &rt, &c, &rec, Errors::from_op(&e), None)
                        .await
                }
            };
        }
        _ => return (StatusCode::NOT_FOUND, "unknown action").into_response(),
    };
    match r {
        Ok(k) => see_other(&format!("/ui/idp/{realm}/settings?done={k}")),
        Err(e) => {
            let rec = c.realm(&realm).await.unwrap_or(rec);
            settings_render(&st, locale, &rv, &rt, &c, &rec, Errors::from_op(&e), None).await
        }
    }
}

// ---------------------------------------------------------------- test

pub(crate) struct HistoryRow {
    pub when: String,
    pub tone: &'static str,
    pub icon: &'static str,
    pub label: String,
    pub problems: usize,
}

#[derive(Template)]
#[template(path = "pages/idp/test.html")]
pub(crate) struct TestPage {
    pub status: crate::Status,
    pub i18n: I18n,
    pub active_page: &'static str,
    pub head: RealmHead,
    pub verify: Option<VerifyView>,
    pub history: Vec<HistoryRow>,
    pub clients: Vec<(String, String)>,
    pub form: HashMap<String, String>,
}

impl TestPage {
    pub fn v(&self, k: &str) -> &str {
        self.form.get(k).map(String::as_str).unwrap_or("")
    }
}

#[derive(Template)]
#[template(path = "partials/idp/verify.html")]
pub(crate) struct VerifyFragment {
    pub i18n: I18n,
    pub verify: Option<VerifyView>,
}

async fn test_render(
    st: &WebState,
    locale: RequestLocale,
    rv: &RequestVersion,
    rt: &RequestTenant,
    c: &IdpConsole,
    rec: &RealmRecord,
    form: HashMap<String, String>,
) -> Response {
    let i18n = I18n::new(locale);
    let name = rec.spec.name.as_str();
    let history = c
        .history(name)
        .iter()
        .map(|r| {
            let (tone, icon, key) = verify::look(r.status);
            HistoryRow {
                when: r.finished_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
                tone,
                icon,
                label: i18n.t(&format!("{key}-short")),
                problems: r.problems().len(),
            }
        })
        .collect();
    let clients = c
        .admin
        .list_clients(&rec.id)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            !r.spec.client_id.starts_with(DIAGNOSTIC_PREFIX) && !r.spec.redirect_uris.is_empty()
        })
        .map(|r| {
            (
                r.spec.client_id.clone(),
                r.spec.redirect_uris.first().cloned().unwrap_or_default(),
            )
        })
        .collect();
    render(TestPage {
        status: current_status(st, rv.0, rt),
        verify: c.last_report(name).map(|r| verify::view(&i18n, &r, false)),
        i18n,
        active_page: "idp-realms",
        head: head(c, rec, "test"),
        history,
        clients,
        form,
    })
}

pub(crate) async fn test_page(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path(realm): Path<String>,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    test_render(&st, locale, &rv, &rt, &c, &rec, HashMap::new()).await
}

/// Runs the diagnostic. htmx → the panel fragment; plain POST → the page.
pub(crate) async fn test_run(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    hx: axum_htmx::HxRequest,
    Path(realm): Path<String>,
    body: Bytes,
) -> Response {
    let (c, rec) = ctx!(c, realm);
    let f = Form::parse(&body);
    let app = match (f.opt("app_client"), f.opt("app_redirect")) {
        (Some(client_id), Some(redirect_uri)) => Some(AppProbe {
            client_id,
            redirect_uri,
        }),
        _ => None,
    };
    let opts = DiagnoseOptions {
        app,
        patient: f.opt("patient"),
        ..Default::default()
    };
    let report = match c.diagnose(&realm, opts).await {
        Ok(r) => r,
        Err(e) => return (super::op_status(&e), e.message).into_response(),
    };
    if hx.0 {
        let i18n = I18n::new(locale);
        return render(VerifyFragment {
            verify: Some(verify::view(&i18n, &report, false)),
            i18n,
        });
    }
    let mut form = HashMap::new();
    for (k, v) in form_urlencoded::parse(&body) {
        form.insert(k.into_owned(), v.into_owned());
    }
    test_render(&st, locale, &rv, &rt, &c, &rec, form).await
}

pub(crate) async fn test_report_json(Path(realm): Path<String>) -> Response {
    let c = match console() {
        Ok(c) => c,
        Err(r) => return r,
    };
    match c.last_report(&realm) {
        Some(r) => super::json_download(&format!("idp-{realm}-diagnostic.json"), &r),
        None => (StatusCode::NOT_FOUND, "no diagnostic has run yet").into_response(),
    }
}
