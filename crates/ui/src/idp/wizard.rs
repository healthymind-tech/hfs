//! The creation wizard (design §11.2): 1 basics → 2 signing keys → 3 users
//! / identity source → 4 clients → 5 SMART capabilities & scopes → 6
//! endpoints → 7 review & test. Step 1 creates a disabled draft realm;
//! every later step saves to it, so the operator can leave and resume
//! (`/ui/idp` lists drafts). Step 7 enables the realm, runs the diagnostic,
//! binds the endpoints only if the realm itself passed, and shows the
//! per-step report.

use std::collections::HashMap;

use askama::Template;
use axum::Extension;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use helios_endpoints::ConsolePrincipal;
use helios_idp::client::{ClientAuth, ClientSpec, GrantType, LaunchType};
use helios_idp::keys::{RsaBits, SigningAlg};
use helios_idp::metadata::{RealmUrls, smart_configuration};
use helios_idp::realm::{RealmSpec, UserSource};
use helios_idp::scope::ScopePolicy;
use helios_idp::user::{FhirLink, RealmUser, UserKind};
use helios_idp_admin::binding::BindMode;
use helios_idp_admin::service::{
    DIAGNOSTIC_PREFIX, FinishOutcome, IdpConsole, OpError, Template as RealmTemplate, WizardState,
};

use super::verify::{self, VerifyView};
use super::{Errors, actor, console, labels, values};
use crate::i18n::{I18n, RequestLocale};
use crate::manager::forms::Form;
use crate::{RequestTenant, RequestVersion, WebState, current_status, render};

pub(crate) const STEPS: [(&str, &str); 7] = [
    ("basics", "idpc-step-basics"),
    ("keys", "idpc-step-keys"),
    ("users", "idpc-step-users"),
    ("clients", "idpc-step-clients"),
    ("capabilities", "idpc-step-capabilities"),
    ("endpoints", "idpc-step-endpoints"),
    ("review", "idpc-step-review"),
];

pub(crate) struct StepNav {
    pub n: u8,
    pub label_key: &'static str,
    /// `done` | `current` | `todo`.
    pub state: &'static str,
    pub href: String,
}

pub(crate) struct Opt {
    pub value: String,
    pub label_key: String,
    pub hint_key: String,
    pub checked: bool,
}

pub(crate) struct KeyRow {
    pub kid: String,
    pub alg: String,
    pub status_key: &'static str,
    pub tone: &'static str,
    pub created: String,
}

pub(crate) struct UserRow {
    pub username: String,
    pub kind_key: &'static str,
    pub fhir_user: String,
}

pub(crate) struct ClientRow {
    pub client_id: String,
    pub name: String,
    pub kind_key: &'static str,
    pub redirects: String,
    pub disabled: bool,
}

pub(crate) struct Toggle {
    pub name: &'static str,
    pub label_key: &'static str,
    pub hint_key: &'static str,
    pub checked: bool,
}

pub(crate) struct ToggleGroup {
    pub title_key: &'static str,
    pub items: Vec<Toggle>,
}

pub(crate) struct NumField {
    pub name: &'static str,
    pub label_key: &'static str,
    pub hint_key: &'static str,
    pub value: String,
}

pub(crate) struct EpChoice {
    pub name: String,
    pub base_url: String,
    pub state_key: &'static str,
    pub state_detail: String,
    pub chosen: bool,
    /// Switching it changes how it authenticates today (warn + diff).
    pub changes: bool,
    pub mode_before: String,
}

pub(crate) struct ReviewRow {
    pub label_key: &'static str,
    pub value: String,
    pub edit_step: u8,
}

#[derive(Template)]
#[template(path = "pages/idp/wizard.html")]
pub(crate) struct WizardPage {
    pub status: crate::Status,
    pub i18n: I18n,
    pub active_page: &'static str,
    pub step: u8,
    pub realm: String,
    pub display_name: String,
    pub public_base: String,
    pub issuer: String,
    pub keys_available: bool,
    pub nav: Vec<StepNav>,
    pub errors: Errors,
    pub form: HashMap<String, String>,
    pub templates: Vec<Opt>,
    // step 2
    pub algs: Vec<Opt>,
    pub rsa_bits: String,
    pub rotate_days: String,
    pub keys: Vec<KeyRow>,
    pub oidc_on: bool,
    // step 3
    pub user_source: String,
    pub password_min: String,
    pub lockout: String,
    pub users: Vec<UserRow>,
    pub endpoint_names: Vec<String>,
    // step 4
    pub clients: Vec<ClientRow>,
    pub new_secret: Option<(String, String)>,
    pub client_kinds: Vec<Opt>,
    pub sb: super::scopes::ScopeBuilder,
    // step 5
    pub groups: Vec<ToggleGroup>,
    pub lifetimes: Vec<NumField>,
    pub smart_preview: String,
    // step 6
    pub endpoints: Vec<EpChoice>,
    pub bind_mode: String,
    pub test_patient: String,
    // step 7
    pub review: Vec<ReviewRow>,
    pub verify: Option<VerifyView>,
    pub finish: Option<FinishView>,
}

pub(crate) struct FinishView {
    pub bound: Vec<String>,
    pub not_bound: Vec<String>,
    pub ok: bool,
}

impl WizardPage {
    pub fn finished_ok(&self) -> bool {
        self.finish.as_ref().is_some_and(|f| f.not_bound.is_empty())
    }
    pub fn is_v(&self, k: &str, val: &str) -> bool {
        self.v(k) == val
    }
    pub fn v(&self, k: &str) -> &str {
        self.form.get(k).map(String::as_str).unwrap_or("")
    }
    pub fn err(&self, k: &str) -> String {
        self.errors.get(k)
    }
    pub fn step_label(&self) -> String {
        self.i18n.t(STEPS[(self.step - 1) as usize].1)
    }
}

fn nav(realm: &str, current: u8, reached: u8) -> Vec<StepNav> {
    STEPS
        .iter()
        .enumerate()
        .map(|(i, (_, key))| {
            let n = i as u8 + 1;
            StepNav {
                n,
                label_key: key,
                state: if n == current {
                    "current"
                } else if n < reached.max(current) {
                    "done"
                } else {
                    "todo"
                },
                href: if realm.is_empty() || n == 1 {
                    String::new()
                } else {
                    format!("/ui/idp/new/{realm}/{n}")
                },
            }
        })
        .collect()
}

fn base_page(
    st: &WebState,
    locale: RequestLocale,
    rv: &RequestVersion,
    rt: &RequestTenant,
    c: &IdpConsole,
    step: u8,
) -> WizardPage {
    let i18n = I18n::new(locale);
    WizardPage {
        status: current_status(st, rv.0, rt),
        i18n,
        active_page: "idp-realms",
        step,
        realm: String::new(),
        display_name: String::new(),
        public_base: c.public_base().as_str().trim_end_matches('/').to_string(),
        issuer: String::new(),
        keys_available: c.keys_available(),
        nav: nav("", step, step),
        errors: Errors::default(),
        form: HashMap::new(),
        templates: RealmTemplate::ALL
            .iter()
            .map(|t| Opt {
                value: t.as_str().into(),
                label_key: format!("idpc-template-{}", t.as_str()),
                hint_key: format!("idpc-template-{}-hint", t.as_str()),
                checked: false,
            })
            .collect(),
        algs: vec![],
        rsa_bits: "2048".into(),
        rotate_days: "90".into(),
        keys: vec![],
        oidc_on: true,
        user_source: "local".into(),
        password_min: "12".into(),
        lockout: "5".into(),
        users: vec![],
        endpoint_names: vec![],
        clients: vec![],
        new_secret: None,
        client_kinds: ["public", "secret", "key", "backend"]
            .iter()
            .map(|k| Opt {
                value: k.to_string(),
                label_key: format!("idpc-client-kind-{k}"),
                hint_key: format!("idpc-client-kind-{k}-hint"),
                checked: *k == "public",
            })
            .collect(),
        sb: Default::default(),
        groups: vec![],
        lifetimes: vec![],
        smart_preview: String::new(),
        endpoints: vec![],
        bind_mode: "keep".into(),
        test_patient: String::new(),
        review: vec![],
        verify: None,
        finish: None,
    }
}

// ---------------------------------------------------------------- step 1

pub(crate) async fn basics_page(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
) -> Response {
    let c = match console() {
        Ok(c) => c,
        Err(r) => return r,
    };
    let mut p = base_page(&st, locale, &rv, &rt, &c, 1);
    p.form.insert("template".into(), "everything".into());
    for t in &mut p.templates {
        t.checked = t.value == "everything";
    }
    render(p)
}

pub(crate) async fn basics_submit(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    body: Bytes,
) -> Response {
    let c = match console() {
        Ok(c) => c,
        Err(r) => return r,
    };
    let f = Form::parse(&body);
    let template = RealmTemplate::parse(f.get("template")).unwrap_or(RealmTemplate::Everything);
    match c
        .create_draft(f.get("name"), f.get("display_name"), template)
        .await
    {
        Ok(rec) => super::see_other(&format!("/ui/idp/new/{}/2", rec.spec.name.as_str())),
        Err(e) => {
            let mut p = base_page(&st, locale, &rv, &rt, &c, 1);
            for k in ["name", "display_name", "template"] {
                p.form.insert(k.into(), f.get(k).to_string());
            }
            for t in &mut p.templates {
                t.checked = t.value == template.as_str();
            }
            p.errors = Errors::from_op(&e);
            if let Some(m) = p.errors.fields.remove("displayName") {
                p.errors.fields.insert("display_name".into(), m);
            }
            let mut r = render(p);
            *r.status_mut() = super::op_status(&e);
            r
        }
    }
}

// ---------------------------------------------------------------- steps 2-7

async fn load(
    st: &WebState,
    locale: RequestLocale,
    rv: &RequestVersion,
    rt: &RequestTenant,
    c: &IdpConsole,
    realm: &str,
    step: u8,
) -> Result<WizardPage, Response> {
    let rec = c
        .realm(realm)
        .await
        .map_err(|e| (super::op_status(&e), e.message).into_response())?;
    let w = WizardState::of(&rec.spec);
    let reached = w.as_ref().map(|w| w.step).unwrap_or(7);
    let mut p = base_page(st, locale, rv, rt, c, step);
    p.realm = realm.to_string();
    p.display_name = rec.spec.display_name.clone();
    p.issuer = c.issuer(&rec.spec);
    p.nav = nav(realm, step, reached);
    fill(&mut p, c, &rec.id, &rec.spec, w.as_ref()).await;
    if step == 4 {
        let shape = super::scopes::shape_for("public", &["standalone".into()], false);
        let i18n = I18n::new(locale);
        p.sb = super::scopes::builder(
            c,
            &rec,
            &i18n,
            &super::scopes::default_scopes(&rec.spec.features),
            false,
            "",
            &shape,
            String::new(),
        )
        .await;
    }
    Ok(p)
}

pub(crate) fn feature_groups(s: &RealmSpec) -> Vec<ToggleGroup> {
    let f = &s.features;
    let t = |name, label_key, hint_key, checked| Toggle {
        name,
        label_key,
        hint_key,
        checked,
    };
    vec![
        ToggleGroup {
            title_key: "idpc-cap-group-launch",
            items: vec![
                t(
                    "launch_standalone",
                    "idpc-cap-launch-standalone",
                    "idpc-cap-launch-standalone-hint",
                    f.launch_standalone,
                ),
                t(
                    "launch_ehr",
                    "idpc-cap-launch-ehr",
                    "idpc-cap-launch-ehr-hint",
                    f.launch_ehr,
                ),
                t(
                    "backend_services",
                    "idpc-cap-backend",
                    "idpc-cap-backend-hint",
                    f.backend_services,
                ),
            ],
        },
        ToggleGroup {
            title_key: "idpc-cap-group-clients",
            items: vec![
                t(
                    "client_public",
                    "idpc-cap-client-public",
                    "idpc-cap-client-public-hint",
                    f.client_public,
                ),
                t(
                    "client_confidential_symmetric",
                    "idpc-cap-client-secret",
                    "idpc-cap-client-secret-hint",
                    f.client_confidential_symmetric,
                ),
                t(
                    "client_confidential_asymmetric",
                    "idpc-cap-client-key",
                    "idpc-cap-client-key-hint",
                    f.client_confidential_asymmetric,
                ),
            ],
        },
        ToggleGroup {
            title_key: "idpc-cap-group-sso",
            items: vec![
                t(
                    "openid_connect",
                    "idpc-cap-oidc",
                    "idpc-cap-oidc-hint",
                    f.openid_connect,
                ),
                t(
                    "offline_access",
                    "idpc-cap-offline",
                    "idpc-cap-offline-hint",
                    f.offline_access,
                ),
            ],
        },
        ToggleGroup {
            title_key: "idpc-cap-group-permissions",
            items: vec![
                t(
                    "permission_patient",
                    "idpc-cap-perm-patient",
                    "idpc-cap-perm-patient-hint",
                    f.permission_patient,
                ),
                t(
                    "permission_user",
                    "idpc-cap-perm-user",
                    "idpc-cap-perm-user-hint",
                    f.permission_user,
                ),
                t(
                    "permission_v1",
                    "idpc-cap-perm-v1",
                    "idpc-cap-perm-v1-hint",
                    f.permission_v1,
                ),
                t(
                    "context_banner",
                    "idpc-cap-banner",
                    "idpc-cap-banner-hint",
                    f.context_banner,
                ),
            ],
        },
        ToggleGroup {
            title_key: "idpc-cap-group-tokens",
            items: vec![
                t(
                    "introspection",
                    "idpc-cap-introspection",
                    "idpc-cap-introspection-hint",
                    f.introspection,
                ),
                t(
                    "revocation",
                    "idpc-cap-revocation",
                    "idpc-cap-revocation-hint",
                    f.revocation,
                ),
            ],
        },
    ]
}

pub(crate) fn lifetime_fields(s: &RealmSpec) -> Vec<NumField> {
    let l = &s.lifetimes;
    let n = |name, label_key, hint_key, v: u32| NumField {
        name,
        label_key,
        hint_key,
        value: v.to_string(),
    };
    vec![
        n(
            "lifetimes.accessToken",
            "idpc-life-access",
            "idpc-life-access-hint",
            l.access_token,
        ),
        n(
            "lifetimes.backendAccessToken",
            "idpc-life-backend",
            "idpc-life-backend-hint",
            l.backend_access_token,
        ),
        n(
            "lifetimes.authorizationCode",
            "idpc-life-code",
            "idpc-life-code-hint",
            l.authorization_code,
        ),
        n(
            "lifetimes.refreshTokenIdle",
            "idpc-life-refresh-idle",
            "idpc-life-refresh-idle-hint",
            l.refresh_token_idle / 86400,
        ),
        n(
            "lifetimes.refreshTokenAbsolute",
            "idpc-life-refresh-abs",
            "idpc-life-refresh-abs-hint",
            l.refresh_token_absolute / 86400,
        ),
        n(
            "lifetimes.ssoSessionIdle",
            "idpc-life-sso-idle",
            "idpc-life-sso-idle-hint",
            l.sso_session_idle / 60,
        ),
    ]
}

pub(crate) fn smart_json(c: &IdpConsole, s: &RealmSpec) -> String {
    let urls = RealmUrls::new(&c.issuer(s));
    serde_json::to_string_pretty(&smart_configuration(s, &urls)).unwrap_or_default()
}

async fn fill(
    p: &mut WizardPage,
    c: &IdpConsole,
    realm_id: &str,
    s: &RealmSpec,
    w: Option<&WizardState>,
) {
    p.oidc_on = s.features.openid_connect;
    p.algs = SigningAlg::ALL
        .iter()
        .map(|a| Opt {
            value: a.as_str().into(),
            label_key: format!("idpc-alg-{}", a.as_str().to_ascii_lowercase()),
            hint_key: match a {
                SigningAlg::RS256 => "idpc-alg-rs256-hint".into(),
                SigningAlg::ES256 => "idpc-alg-es256-hint".into(),
                _ => "idpc-alg-optional-hint".into(),
            },
            checked: s.signing_algs.contains(a),
        })
        .collect();
    p.rsa_bits = serde_json::to_value(s.rsa_bits)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "2048".into());
    p.rotate_days = s.rotation.rotate_after_days.to_string();
    p.keys = c
        .admin
        .list_keys(realm_id)
        .await
        .unwrap_or_default()
        .iter()
        .filter(|k| k.status.published())
        .map(|k| {
            let (status_key, tone) = labels::key_status(k.status);
            KeyRow {
                kid: k.kid.chars().take(16).collect(),
                alg: k.alg.as_str().into(),
                status_key,
                tone,
                created: k.created_at.format("%Y-%m-%d %H:%M UTC").to_string(),
            }
        })
        .collect();
    p.user_source = match s.user_source {
        UserSource::Console => "console".into(),
        UserSource::UpstreamOidc { .. } => "upstream".into(),
        UserSource::Local => "local".into(),
    };
    p.password_min = s.security.password_min_length.to_string();
    p.lockout = s.security.lockout_threshold.to_string();
    p.users = c
        .admin
        .list_users(realm_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|u| !u.username.starts_with(DIAGNOSTIC_PREFIX))
        .map(|u| UserRow {
            fhir_user: u
                .links
                .iter()
                .find(|l| l.is_fhir_user)
                .map(|l| format!("{} · {}", l.endpoint, l.reference))
                .unwrap_or_default(),
            username: u.username,
            kind_key: labels::user_kind(u.kind),
        })
        .collect();
    let views = c.endpoint_views().await;
    p.endpoint_names = views.iter().map(|e| e.name.clone()).collect();
    p.clients = c
        .admin
        .list_clients(realm_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.spec)
        .filter(|c| !c.client_id.starts_with(DIAGNOSTIC_PREFIX))
        .map(|c| ClientRow {
            kind_key: labels::client_kind(&c),
            redirects: c.redirect_uris.join("\n"),
            disabled: c.disabled,
            client_id: c.client_id,
            name: c.name,
        })
        .collect();
    p.groups = feature_groups(s);
    p.lifetimes = lifetime_fields(s);
    p.smart_preview = smart_json(c, s);
    let chosen: Vec<String> = w
        .map(|w| w.endpoints.clone())
        .unwrap_or_else(|| s.endpoints.clone());
    p.bind_mode = serde_json::to_value(w.map(|w| w.bind_mode).unwrap_or_default())
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "keep".into());
    p.test_patient = w.and_then(|w| w.test_patient.clone()).unwrap_or_default();
    let mut eps = vec![];
    for e in &views {
        let diff = c
            .bind_diff(s.name.as_str(), &e.name, BindMode::Keep)
            .await
            .ok();
        let (before, detail, mode_before) = match &diff {
            Some(d) => (
                d.before.clone(),
                d.before_issuer.clone().unwrap_or_default(),
                d.mode_before.clone(),
            ),
            None => ("inherit".into(), String::new(), "inherit".into()),
        };
        let mine = before == format!("internal:{}", s.name.as_str());
        eps.push(EpChoice {
            name: e.name.clone(),
            base_url: e.base_url.clone(),
            state_key: labels::endpoint_state(&before),
            state_detail: if before.starts_with("internal:") {
                before.trim_start_matches("internal:").to_string()
            } else {
                detail
            },
            chosen: chosen.contains(&e.name) || mine,
            changes: !mine && before != "inherit" && before != "off",
            mode_before,
        });
    }
    p.endpoints = eps;
    let src_key = match s.user_source {
        UserSource::Console => "idpc-source-console",
        UserSource::UpstreamOidc { .. } => "idpc-source-upstream",
        UserSource::Local => "idpc-source-local",
    };
    p.review = vec![
        ReviewRow {
            label_key: "idpc-review-name",
            value: format!("{} — {}", s.name.as_str(), s.display_name),
            edit_step: 1,
        },
        ReviewRow {
            label_key: "idpc-review-issuer",
            value: p.issuer.clone(),
            edit_step: 1,
        },
        ReviewRow {
            label_key: "idpc-review-keys",
            value: format!(
                "{} ({} key(s))",
                s.signing_algs
                    .iter()
                    .map(|a| a.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                p.keys.len()
            ),
            edit_step: 2,
        },
        ReviewRow {
            label_key: "idpc-review-users",
            value: format!("{} · {}", p.i18n.t(src_key), p.users.len()),
            edit_step: 3,
        },
        ReviewRow {
            label_key: "idpc-review-clients",
            value: if p.clients.is_empty() {
                "—".into()
            } else {
                p.clients
                    .iter()
                    .map(|c| c.client_id.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            edit_step: 4,
        },
        ReviewRow {
            label_key: "idpc-review-capabilities",
            value: helios_idp::metadata::smart_capabilities(s).join(", "),
            edit_step: 5,
        },
        ReviewRow {
            label_key: "idpc-review-endpoints",
            value: if chosen.is_empty() {
                "—".into()
            } else {
                chosen.join(", ")
            },
            edit_step: 6,
        },
    ];
}

pub(crate) async fn step_page(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path((realm, step)): Path<(String, String)>,
) -> Response {
    let c = match console() {
        Ok(c) => c,
        Err(r) => return r,
    };
    let Ok(step) = step.parse::<u8>() else {
        return (axum::http::StatusCode::NOT_FOUND, "no such step").into_response();
    };
    if !(2..=7).contains(&step) {
        return super::see_other("/ui/idp/new");
    }
    let mut p = match load(&st, locale, &rv, &rt, &c, &realm, step).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    if step == 7
        && let Ok(r) = c.preflight(&realm, None).await
    {
        p.verify = Some(verify::view(&p.i18n, &r, true));
    }
    render(p)
}

fn num(f: &Form, name: &str, errs: &mut Errors) -> Option<u32> {
    let v = f.get(name);
    if v.is_empty() {
        return None;
    }
    match v.parse::<u32>() {
        Ok(n) => Some(n),
        Err(_) => {
            errs.fields
                .insert(name.to_string(), "enter a whole number".into());
            None
        }
    }
}

/// Applies step 5's form to a spec (also used by the live preview and the
/// settings page).
pub(crate) fn apply_capabilities(s: &mut RealmSpec, f: &Form, errs: &mut Errors) {
    let ft = &mut s.features;
    ft.launch_standalone = f.checked("launch_standalone");
    ft.launch_ehr = f.checked("launch_ehr");
    ft.backend_services = f.checked("backend_services");
    ft.client_public = f.checked("client_public");
    ft.client_confidential_symmetric = f.checked("client_confidential_symmetric");
    ft.client_confidential_asymmetric = f.checked("client_confidential_asymmetric");
    ft.openid_connect = f.checked("openid_connect");
    ft.offline_access = f.checked("offline_access");
    ft.permission_patient = f.checked("permission_patient");
    ft.permission_user = f.checked("permission_user");
    ft.permission_v1 = f.checked("permission_v1");
    ft.context_banner = f.checked("context_banner");
    ft.introspection = f.checked("introspection");
    ft.revocation = f.checked("revocation");
    let l = &mut s.lifetimes;
    if let Some(n) = num(f, "lifetimes.accessToken", errs) {
        l.access_token = n;
    }
    if let Some(n) = num(f, "lifetimes.backendAccessToken", errs) {
        l.backend_access_token = n;
    }
    if let Some(n) = num(f, "lifetimes.authorizationCode", errs) {
        l.authorization_code = n;
    }
    if let Some(n) = num(f, "lifetimes.refreshTokenIdle", errs) {
        l.refresh_token_idle = n.saturating_mul(86400);
    }
    if let Some(n) = num(f, "lifetimes.refreshTokenAbsolute", errs) {
        l.refresh_token_absolute = n.saturating_mul(86400);
    }
    if let Some(n) = num(f, "lifetimes.ssoSessionIdle", errs) {
        l.sso_session_idle = n.saturating_mul(60);
    }
    // Keep OIDC's RS256 requirement satisfiable without a trip to step 2.
    if s.features.openid_connect && !s.signing_algs.contains(&SigningAlg::RS256) {
        s.signing_algs.insert(0, SigningAlg::RS256);
    }
}

/// Builds a client from the add-app form; `errs` gets field errors.
pub(crate) fn client_from_form(body: &[u8], f: &Form, errs: &mut Errors) -> Option<ClientSpec> {
    let kind = f.get("client_kind");
    let name = f.get("client_name").to_string();
    if name.is_empty() {
        errs.fields
            .insert("client_name".into(), "enter the app's name".into());
    }
    let id = match f.opt("client_id") {
        Some(i) => i,
        None => name
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .trim_matches('-')
            .to_string(),
    };
    let redirects = f.list("redirect_uris");
    let launch: Vec<String> = values(body, "launch");
    let scopes = super::scopes::scopes_from_form(body);
    let jwks_source = || -> Option<ClientAuth> {
        let uri = f.opt("jwks_uri");
        let jwks = f
            .opt("jwks")
            .map(|j| serde_json::from_str::<serde_json::Value>(&j));
        let jwks = match jwks {
            Some(Ok(v)) => Some(v),
            Some(Err(_)) => {
                return None;
            }
            None => None,
        };
        Some(ClientAuth::PrivateKeyJwt {
            jwks_uri: uri,
            jwks,
            algs: vec![
                SigningAlg::ES384,
                SigningAlg::RS384,
                SigningAlg::ES256,
                SigningAlg::RS256,
            ],
        })
    };
    let (auth, grants, launch_types) = match kind {
        "secret" => (
            ClientAuth::ClientSecret {
                secret_hash: String::new(),
            },
            vec![GrantType::AuthorizationCode, GrantType::RefreshToken],
            launch.iter().filter_map(|l| launch_type(l)).collect(),
        ),
        "key" => match jwks_source() {
            Some(a) => (
                a,
                vec![GrantType::AuthorizationCode, GrantType::RefreshToken],
                launch.iter().filter_map(|l| launch_type(l)).collect(),
            ),
            None => {
                errs.fields.insert("jwks".into(), "not valid JSON".into());
                return None;
            }
        },
        "backend" => match jwks_source() {
            Some(a) => (
                a,
                vec![GrantType::ClientCredentials],
                vec![LaunchType::Backend],
            ),
            None => {
                errs.fields.insert("jwks".into(), "not valid JSON".into());
                return None;
            }
        },
        _ => (
            ClientAuth::None,
            vec![GrantType::AuthorizationCode, GrantType::RefreshToken],
            launch
                .iter()
                .filter_map(|l| launch_type(l))
                .collect::<Vec<_>>(),
        ),
    };
    if errs.fields.contains_key("client_name") {
        return None;
    }
    let grants = if scopes.iter().any(|s| s == "offline_access") || kind == "backend" {
        grants
    } else {
        grants
            .into_iter()
            .filter(|g| *g != GrantType::RefreshToken)
            .collect()
    };
    Some(ClientSpec {
        client_id: id,
        name,
        auth,
        grant_types: grants,
        redirect_uris: redirects,
        post_logout_redirect_uris: vec![],
        launch_types,
        scopes: ScopePolicy {
            allowed: scopes,
            accept_v1: f.checked("accept_v1"),
        },
        trusted: f.checked("trusted"),
        require_dpop: false,
        require_par: false,
        backchannel_logout_uri: None,
        access_token_ttl_secs: None,
        disabled: false,
    })
}

fn launch_type(s: &str) -> Option<LaunchType> {
    match s {
        "ehr" => Some(LaunchType::Ehr),
        "standalone" => Some(LaunchType::Standalone),
        _ => None,
    }
}

/// Maps the service's client field names onto the form's inputs.
pub(crate) fn client_field(f: &str) -> String {
    match f {
        "clientId" => "client_id".into(),
        "redirectUris" => "redirect_uris".into(),
        "auth" | "auth.jwksUri" => "jwks_uri".into(),
        "auth.jwks" => "jwks".into(),
        "launchTypes" => "launch".into(),
        "grantTypes" | "scopes" => "scopes_text".into(),
        other => other.to_string(),
    }
}

pub(crate) fn user_from_form(f: &Form) -> RealmUser {
    let kind = match f.get("user_kind") {
        "practitioner" => UserKind::Practitioner,
        "related-person" => UserKind::RelatedPerson,
        "staff" => UserKind::Staff,
        _ => UserKind::Patient,
    };
    let mut links = vec![];
    if let (Some(ep), Some(reference)) = (f.opt("user_endpoint"), f.opt("user_reference")) {
        links.push(FhirLink {
            endpoint: ep,
            reference,
            is_fhir_user: true,
            accessible_patients: vec![],
        });
    }
    RealmUser {
        id: String::new(),
        username: f.get("user_username").to_string(),
        kind,
        display_name: f.opt("user_display"),
        email: f.opt("user_email"),
        email_verified: false,
        links,
        external_subject: None,
        disabled: false,
    }
}

pub(crate) fn user_field(f: &str) -> String {
    match f {
        "username" => "user_username".into(),
        "password" => "user_password".into(),
        "links" | "links.reference" => "user_reference".into(),
        other => format!("user_{other}"),
    }
}

pub(crate) async fn step_submit(
    State(st): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    principal: Option<Extension<ConsolePrincipal>>,
    Path((realm, step)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let c = match console() {
        Ok(c) => c,
        Err(r) => return r,
    };
    let Ok(step) = step.parse::<u8>() else {
        return (axum::http::StatusCode::NOT_FOUND, "no such step").into_response();
    };
    let f = Form::parse(&body);
    let action = f.get("action").to_string();
    let next = |n: u8| super::see_other(&format!("/ui/idp/new/{realm}/{n}"));
    let back = action == "back";
    let mut errs = Errors::default();
    let mut new_secret = None;
    let result: Result<(), OpError> = match step {
        2 => {
            let algs: Vec<SigningAlg> = values(&body, "alg")
                .iter()
                .filter_map(|a| a.parse().ok())
                .collect();
            let bits = match f.get("rsa_bits") {
                "3072" => RsaBits::B3072,
                "4096" => RsaBits::B4096,
                _ => RsaBits::B2048,
            };
            let days = num(&f, "rotation.rotateAfterDays", &mut errs);
            if !errs.is_empty() {
                Err(OpError::invalid(
                    "rotation.rotateAfterDays",
                    "enter a whole number",
                ))
            } else {
                c.save_step(&realm, 3, |s, _| {
                    s.signing_algs = algs;
                    s.rsa_bits = bits;
                    if let Some(d) = days {
                        s.rotation.rotate_after_days = d;
                    }
                    Ok(())
                })
                .await
                .map(|_| ())
            }
        }
        3 => {
            let src = match f.get("user_source") {
                "console" => UserSource::Console,
                _ => UserSource::Local,
            };
            let pmin = num(&f, "security.passwordMinLength", &mut errs);
            let lock = num(&f, "security.lockoutThreshold", &mut errs);
            let mut r = c
                .save_step(&realm, if action == "add_user" { 3 } else { 4 }, |s, _| {
                    s.user_source = src;
                    if let Some(n) = pmin {
                        s.security.password_min_length = n;
                    }
                    if let Some(n) = lock {
                        s.security.lockout_threshold = n;
                    }
                    Ok(())
                })
                .await
                .map(|_| ());
            if r.is_ok() && !f.get("user_username").is_empty() {
                let rec = c.realm(&realm).await;
                r = match rec {
                    Ok(rec) => c
                        .admin
                        .create_user(
                            &rec.id,
                            user_from_form(&f),
                            f.opt("user_password").as_deref(),
                            false,
                        )
                        .await
                        .map(|_| ())
                        .map_err(OpError::from)
                        .map_err(|mut e| {
                            e.field = e.field.map(|x| user_field(&x));
                            e
                        }),
                    Err(e) => Err(e),
                };
            }
            r
        }
        4 => {
            if action == "add_client" {
                match client_from_form(&body, &f, &mut errs) {
                    Some(spec) => match c.realm(&realm).await {
                        Ok(rec) => match c.admin.create_client(&rec.id, spec).await {
                            Ok(created) => {
                                new_secret = created
                                    .client_secret
                                    .map(|s| (created.record.spec.client_id.clone(), s));
                                Ok(())
                            }
                            Err(e) => {
                                let mut e = OpError::from(e);
                                e.field = e.field.map(|x| client_field(&x));
                                Err(e)
                            }
                        },
                        Err(e) => Err(e),
                    },
                    None => Err(OpError::other(400, "")),
                }
            } else if let Some(id) = action.strip_prefix("delete_client:") {
                match c.realm(&realm).await {
                    Ok(rec) => c
                        .admin
                        .delete_client(&rec.id, id)
                        .await
                        .map(|_| ())
                        .map_err(OpError::from),
                    Err(e) => Err(e),
                }
            } else {
                c.save_step(&realm, 5, |_, _| Ok(())).await.map(|_| ())
            }
        }
        5 => {
            let mut local = Errors::default();
            let r = c
                .save_step(&realm, 6, |s, _| {
                    apply_capabilities(s, &f, &mut local);
                    Ok(())
                })
                .await
                .map(|_| ());
            errs.fields.extend(local.fields);
            if errs.is_empty() {
                r
            } else {
                Err(OpError::other(400, ""))
            }
        }
        6 => {
            let eps = values(&body, "endpoint");
            let mode = BindMode::parse(f.get("bind_mode"));
            let patient = f.opt("test_patient");
            c.save_step(&realm, 7, |_, w| {
                w.endpoints = eps;
                w.bind_mode = mode;
                w.test_patient = patient;
                Ok(())
            })
            .await
            .map(|_| ())
        }
        7 => {
            let who = actor(&rt, principal.as_ref().map(|p| &p.0));
            match c.finish(&realm, &who).await {
                Ok(out) => return render_finish(&st, locale, &rv, &rt, &c, &realm, out).await,
                Err(e) => Err(e),
            }
        }
        _ => return super::see_other("/ui/idp/new"),
    };
    match result {
        Ok(())
            if new_secret.is_some()
                || action == "add_user"
                || action.starts_with("delete_client:")
                || (action == "add_client" && step == 4) =>
        {
            let mut p = match load(&st, locale, &rv, &rt, &c, &realm, step).await {
                Ok(p) => p,
                Err(r) => return r,
            };
            p.new_secret = new_secret;
            if action == "add_user" {
                return next(3);
            }
            render(p)
        }
        Ok(()) if back => next(step.saturating_sub(1).max(2)),
        Ok(()) => next(step + 1),
        Err(e) => {
            let mut p = match load(&st, locale, &rv, &rt, &c, &realm, step).await {
                Ok(p) => p,
                Err(r) => return r,
            };
            let mut all = if e.message.is_empty() {
                Errors::default()
            } else {
                Errors::from_op(&e)
            };
            all.fields.extend(errs.fields);
            // Keep what was typed.
            for (k, v) in form_urlencoded::parse(&body) {
                if !k.contains("password") {
                    p.form.insert(k.into_owned(), v.into_owned());
                }
            }
            if step == 4 {
                if let Ok(rec) = c.realm(&realm).await {
                    let i18n = I18n::new(locale);
                    let launch = values(&body, "launch");
                    let accept_v1 = f.checked("accept_v1");
                    let shape = super::scopes::shape_for(f.get("client_kind"), &launch, accept_v1);
                    p.sb = super::scopes::builder(
                        &c,
                        &rec,
                        &i18n,
                        &super::scopes::scopes_from_form(&body),
                        accept_v1,
                        "",
                        &shape,
                        all.get("scopes_text"),
                    )
                    .await;
                }
                for k in &mut p.client_kinds {
                    k.checked = k.value == f.get("client_kind");
                }
            }
            p.errors = all;
            let mut r = render(p);
            *r.status_mut() = axum::http::StatusCode::UNPROCESSABLE_ENTITY;
            r
        }
    }
}

async fn render_finish(
    st: &WebState,
    locale: RequestLocale,
    rv: &RequestVersion,
    rt: &RequestTenant,
    c: &IdpConsole,
    realm: &str,
    out: FinishOutcome,
) -> Response {
    let mut p = match load(st, locale, rv, rt, c, realm, 7).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let still_draft = c
        .realm(realm)
        .await
        .ok()
        .and_then(|r| WizardState::of(&r.spec))
        .is_some();
    p.verify = Some(verify::view(&p.i18n, &out.report, still_draft));
    p.finish = Some(FinishView {
        ok: out.not_bound.is_empty() && out.report.problems().is_empty(),
        bound: out.bound,
        not_bound: out.not_bound,
    });
    render(p)
}

/// Live `smart-configuration` preview of step 5 (htmx, no save).
pub(crate) async fn smart_preview(Path(realm): Path<String>, body: Bytes) -> Response {
    let c = match console() {
        Ok(c) => c,
        Err(r) => return r,
    };
    let Ok(rec) = c.realm(&realm).await else {
        return Html(String::new()).into_response();
    };
    let mut s = rec.spec.clone();
    let mut errs = Errors::default();
    apply_capabilities(&mut s, &Form::parse(&body), &mut errs);
    let problem = s
        .validate()
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    let json = smart_json(&c, &s);
    let esc = |t: &str| {
        t.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let warn = if problem.is_empty() {
        String::new()
    } else {
        format!(
            "<p class=\"mfield__error\" role=\"alert\">{}</p>",
            esc(&problem)
        )
    };
    Html(format!(
        "{warn}<pre class=\"json-preview\"><code>{}</code></pre>",
        esc(&json)
    ))
    .into_response()
}

pub(crate) async fn discard(Path(realm): Path<String>) -> Response {
    let c = match console() {
        Ok(c) => c,
        Err(r) => return r,
    };
    match c.discard_draft(&realm).await {
        Ok(()) => super::see_other("/ui/idp?done=draft-discarded"),
        Err(e) => (super::op_status(&e), e.message).into_response(),
    }
}
