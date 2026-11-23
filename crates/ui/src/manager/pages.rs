//! Handlers of the manager pages. Thin: read through the backend, shape view
//! models ([`super::view`]), render a template.

use std::sync::Arc;

use askama::Template;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Extension, Router, routing::get, routing::post};
use chrono::Utc;
use helios_endpoints::{
    Actor, AuthMode, ConsolePrincipal, ConsoleRole, Endpoint, EndpointAuth, EndpointSpec,
    EndpointStatus, FhirRelease, ManagedBy,
};
use serde::Deserialize;

use super::backend::{BackendError, HealthState, ManagerBackend, ServerHealth, SyncJobView};
use super::forms::{self, FieldErrors, Form, NewEndpoint, Section, release_label};
use super::view::{
    self, Badge, DiffRow, EndpointCard, EnvDefaults, SyncRow, auth_mode_value,
    terminology_validation_value, validation_mode_value,
};
use super::{EndpointScope, ManagerRuntime};
use crate::i18n::{I18n, RequestLocale};
use crate::{RequestTenant, RequestVersion, Status, WebState, current_status, render};

/// The manager's routes (registered only with a runtime installed).
pub(crate) fn routes() -> Router<WebState> {
    Router::new()
        .route("/ui", get(overview))
        .route("/ui/endpoints/new", get(new_page))
        .route("/ui/endpoints", post(create))
        .route("/ui/endpoints/check", get(check_name))
        .route("/ui/endpoints/preview", post(preview_new))
        .route("/ui/e/{ep}", get(home))
        .route("/ui/e/{ep}/status", post(set_status))
        .route("/ui/e/{ep}/duplicate", get(duplicate_page).post(duplicate))
        .route("/ui/e/{ep}/sync", get(sync_page))
        .route("/ui/e/{ep}/audit", get(audit_page))
        .route("/ui/e/{ep}/history", get(history_page))
        .route("/ui/e/{ep}/subscriptions", get(to_data))
        .route("/ui/e/{ep}/bulk", get(to_data))
        .route("/ui/e/{ep}/{section}/preview", post(preview_section))
        .route("/ui/e/{ep}/{section}", get(section_page).post(save_section))
        .route("/ui/about", get(about))
}

// ---------------------------------------------------------------------------
// Shared context
// ---------------------------------------------------------------------------

fn runtime(state: &WebState) -> &Arc<ManagerRuntime> {
    state
        .manager
        .as_ref()
        .expect("manager routes are only registered with a runtime")
}

fn env_defaults() -> EnvDefaults {
    EnvDefaults::from_env()
}

/// Who is acting, for revisions and audit.
fn actor(rt: &RequestTenant, principal: Option<&ConsolePrincipal>) -> Actor {
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

/// The header every endpoint page shows.
#[derive(Debug, Clone)]
pub(crate) struct EpHeader {
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub base_url: String,
    pub fhir_version: &'static str,
    pub status: Badge,
    pub auth: Badge,
    pub validation: Badge,
    pub is_default: bool,
    pub env_managed: bool,
    pub enabled: bool,
    pub revision: u64,
    pub tenant: String,
    pub updated: String,
    pub updated_by: String,
}

impl EpHeader {
    fn new(e: &Endpoint, server_base: &str, env: &EnvDefaults) -> Self {
        Self {
            name: e.name().to_string(),
            display_name: e.spec.display_name.clone(),
            description: e.spec.description.clone().unwrap_or_default(),
            base_url: e.public_base(server_base),
            fhir_version: release_label(e.spec.fhir_version),
            status: view::status_badge(e.spec.status),
            auth: view::auth_badge(e.spec.auth.as_ref(), env),
            validation: view::validation_badge(e.spec.validation.as_ref(), env),
            is_default: e.spec.is_default(),
            env_managed: e.spec.managed_by == ManagedBy::Env,
            enabled: e.spec.status == EndpointStatus::Enabled,
            revision: e.revision,
            tenant: e.tenant_id().to_string(),
            updated: e.updated_at.format("%Y-%m-%d %H:%M UTC").to_string(),
            updated_by: e.updated_by.clone().unwrap_or_default(),
        }
    }
}

/// Per-request bits every manager page needs.
struct Ctx {
    status: Status,
    i18n: I18n,
    role: ConsoleRole,
    runtime: Arc<ManagerRuntime>,
    env: EnvDefaults,
    server_base: String,
}

fn ctx(state: &WebState, locale: RequestLocale, rv: RequestVersion, rt: &RequestTenant) -> Ctx {
    Ctx {
        status: current_status(state, rv.0, rt),
        i18n: I18n::new(locale),
        role: rt.role,
        runtime: runtime(state).clone(),
        env: env_defaults(),
        server_base: state.public_base_url.clone(),
    }
}

impl Ctx {
    fn can_admin(&self) -> bool {
        self.role >= ConsoleRole::Admin && self.runtime.backend.writable()
    }
}

/// The error page of the manager (404 endpoint, 403 role, 503 backend).
#[derive(Template)]
#[template(path = "pages/manager/error.html")]
struct ErrorPage {
    status: Status,
    i18n: I18n,
    active_page: &'static str,
    code: u16,
    title_key: &'static str,
    text_key: &'static str,
    detail: String,
    back_href: String,
    back_key: &'static str,
}

fn error_page(
    c: Ctx,
    code: StatusCode,
    title_key: &'static str,
    text_key: &'static str,
    detail: String,
) -> Response {
    let mut response = render(ErrorPage {
        status: c.status,
        i18n: c.i18n,
        active_page: "",
        code: code.as_u16(),
        title_key,
        text_key,
        detail,
        back_href: "/ui".into(),
        back_key: "mgr-back-overview",
    });
    *response.status_mut() = code;
    response
}

fn forbidden(c: Ctx) -> Response {
    error_page(
        c,
        StatusCode::FORBIDDEN,
        "mgr-error-forbidden-title",
        "mgr-error-forbidden-text",
        String::new(),
    )
}

fn unavailable(c: Ctx, e: &BackendError) -> Response {
    error_page(
        c,
        StatusCode::SERVICE_UNAVAILABLE,
        "mgr-error-unavailable-title",
        "mgr-error-unavailable-text",
        format!("{e:?}"),
    )
}

fn not_found(c: Ctx) -> Response {
    error_page(
        c,
        StatusCode::NOT_FOUND,
        "mgr-error-endpoint-title",
        "mgr-error-endpoint-text",
        String::new(),
    )
}

/// Loads the endpoint of `/ui/e/{ep}`, or the error response.
#[allow(clippy::result_large_err)]
async fn load(c: Ctx, name: &str) -> Result<(Ctx, Endpoint), Response> {
    match c.runtime.backend.get(name).await {
        Ok(Some(e)) => Ok((c, e)),
        Ok(None) => Err(not_found(c)),
        Err(e) => Err(unavailable(c, &e)),
    }
}

// ---------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub(crate) struct OverviewQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    changed: Option<String>,
    #[serde(default)]
    deleted: Option<String>,
    #[serde(default)]
    enabled: Option<String>,
    #[serde(default)]
    disabled: Option<String>,
    #[serde(default)]
    tenant: Option<String>,
}

/// One tile of the health strip.
pub(crate) struct HealthTile {
    pub label_key: &'static str,
    pub value: String,
    pub value_key: &'static str,
    pub tone: &'static str,
}

fn health_tiles(h: &ServerHealth) -> Vec<HealthTile> {
    let state = |s: HealthState| -> (&'static str, &'static str) {
        match s {
            HealthState::Ok => ("ok", "mgr-health-ok"),
            HealthState::Degraded => ("warn", "mgr-health-degraded"),
            HealthState::Down => ("danger", "mgr-health-down"),
            HealthState::Unknown => ("muted", "mgr-health-unknown"),
        }
    };
    let (db_tone, db_key) = state(h.database);
    let (tx_tone, tx_key) = state(h.terminology);
    vec![
        HealthTile {
            label_key: "mgr-health-database",
            value: h
                .database_latency_ms
                .map(|ms| format!("{ms} ms"))
                .unwrap_or_default(),
            value_key: db_key,
            tone: db_tone,
        },
        HealthTile {
            label_key: "mgr-health-terminology",
            value: String::new(),
            value_key: tx_key,
            tone: tx_tone,
        },
        HealthTile {
            label_key: "mgr-health-instances",
            value: h.instances.map(|n| n.to_string()).unwrap_or_default(),
            value_key: if h.instances.is_some() {
                ""
            } else {
                "mgr-health-unknown"
            },
            tone: if h.instances.is_some() { "ok" } else { "muted" },
        },
        HealthTile {
            label_key: "mgr-health-keyring",
            value: String::new(),
            value_key: if h.key_ring {
                "mgr-health-keyring-set"
            } else {
                "mgr-health-keyring-unset"
            },
            tone: if h.key_ring { "ok" } else { "warn" },
        },
        HealthTile {
            label_key: "mgr-health-console",
            value: String::new(),
            value_key: match h.console_mode {
                helios_endpoints::ConsoleAuthMode::None => "mgr-console-none",
                helios_endpoints::ConsoleAuthMode::Oidc => "mgr-console-oidc",
                helios_endpoints::ConsoleAuthMode::Local => "mgr-console-local",
            },
            tone: "info",
        },
    ]
}

#[derive(Template)]
#[template(path = "pages/manager/overview.html")]
struct OverviewPage {
    status: Status,
    i18n: I18n,
    active_page: &'static str,
    cards: Vec<EndpointCard>,
    shown: usize,
    total: usize,
    enabled: usize,
    disabled: usize,
    q: String,
    status_filter: String,
    health: Vec<HealthTile>,
    can_admin: bool,
    flash_key: &'static str,
    flash_arg: String,
    flash_tenant: String,
}

/// Resource totals of a tenant from the dashboard provider, when known.
async fn counts(tenant: String) -> Option<(u64, usize)> {
    let snap = helios_observability::dashboard::snapshot(
        helios_observability::dashboard::DashboardWindow::default(),
        &tenant,
        &[],
        false,
    )
    .await?;
    snap.figures
        .is_known()
        .then_some((snap.total_resources, snap.distinct_types))
}

pub(crate) async fn overview(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Query(q): Query<OverviewQuery>,
) -> Response {
    let c = ctx(&state, locale, rv, &rt);
    let backend = c.runtime.backend.clone();
    let endpoints = match backend.list().await {
        Ok(list) => list,
        Err(e) => return unavailable(c, &e),
    };
    let jobs = sync_jobs_of(&backend).await.unwrap_or_default();
    let health = backend.health().await;
    // Counts and stats for every endpoint concurrently: one slow tenant must
    // not serialize the page.
    let mut tasks = Vec::with_capacity(endpoints.len());
    for e in &endpoints {
        let tenant = e.tenant_id().to_string();
        let name = e.name().to_string();
        let backend = backend.clone();
        tasks.push(tokio::spawn(async move {
            let (c, s) = futures_lite::future::zip(counts(tenant), backend.stats(&name)).await;
            (c, s)
        }));
    }
    let now = Utc::now();
    let mut cards = Vec::with_capacity(endpoints.len());
    for (e, task) in endpoints.iter().zip(tasks) {
        let (counts, stats) = task.await.unwrap_or((None, None));
        cards.push(EndpointCard::new(
            e,
            &c.server_base,
            &c.env,
            counts,
            stats,
            &jobs,
            now,
        ));
    }
    let total = cards.len();
    let enabled = cards.iter().filter(|c| c.status_slug == "enabled").count();
    let disabled = cards.iter().filter(|c| c.status_slug == "disabled").count();
    cards.retain(|card| card.matches(&q.q, &q.status));
    // Each outcome says what happened (UX-14).
    let (flash_key, flash_arg) = if let Some(n) = &q.created {
        ("mgr-flash-created", n.clone())
    } else if let Some(n) = &q.deleted {
        ("mgr-flash-deleted", n.clone())
    } else if let Some(n) = &q.disabled {
        ("mgr-flash-disabled", n.clone())
    } else if let Some(n) = &q.enabled {
        ("mgr-flash-enabled", n.clone())
    } else if let Some(n) = &q.changed {
        ("mgr-flash-status", n.clone())
    } else {
        ("", String::new())
    };
    let flash_tenant = q.tenant.clone().unwrap_or_default();
    let can_admin = c.can_admin();
    render(OverviewPage {
        status: c.status,
        i18n: c.i18n,
        active_page: "overview",
        shown: cards.len(),
        cards,
        total,
        enabled,
        disabled,
        q: q.q,
        status_filter: q.status,
        health: health_tiles(&health),
        can_admin,
        flash_key,
        flash_arg,
        flash_tenant,
    })
}

// ---------------------------------------------------------------------------
// Endpoint home
// ---------------------------------------------------------------------------

/// A configuration summary row of the endpoint home.
pub(crate) struct SummaryRow {
    pub label_key: &'static str,
    pub value: String,
    pub value_key: &'static str,
    pub inherited: bool,
    pub href: String,
}

/// A row of the top resource types.
pub(crate) struct TypeRow {
    pub name: String,
    pub total: u64,
    pub percent: u8,
    pub href: String,
}

/// An AuditEvent row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuditRow {
    pub at: String,
    pub action: String,
    pub outcome_ok: bool,
    pub outcome: String,
    pub agent: String,
    pub what: String,
    pub kind: String,
}

/// A config revision row.
pub(crate) struct RevisionRow {
    pub revision: u64,
    pub change_key: &'static str,
    pub at: String,
    pub by: String,
    pub diff: Vec<DiffRow>,
}

#[derive(Template)]
#[template(path = "pages/manager/endpoint-home.html")]
struct HomePage {
    status: Status,
    i18n: I18n,
    active_page: &'static str,
    head: EpHeader,
    can_admin: bool,
    resources: Option<u64>,
    types: Option<usize>,
    export_jobs: Option<u64>,
    import_jobs: Option<u64>,
    top_types: Vec<TypeRow>,
    summary: Vec<SummaryRow>,
    revisions: Vec<RevisionRow>,
    audit: Vec<AuditRow>,
    audit_error: bool,
    sync_rows: Vec<SyncRow>,
    sync_available: bool,
    warnings: Vec<&'static str>,
    data_href: String,
}

fn summary(e: &Endpoint, env: &EnvDefaults) -> Vec<SummaryRow> {
    let spec = &e.spec;
    let href = |s: &str| format!("/ui/e/{}/{s}", e.name());
    let auth_mode = spec.auth.as_ref().map(|a| a.mode).unwrap_or(env.auth_mode);
    let issuer = spec
        .auth
        .as_ref()
        .and_then(|a| a.issuer.clone())
        .or_else(|| env.auth_issuer.clone())
        .unwrap_or_default();
    let v = spec.validation.as_ref();
    let term = spec
        .terminology
        .as_ref()
        .and_then(|t| t.server_url.clone())
        .or_else(|| env.terminology_server.clone());
    vec![
        SummaryRow {
            label_key: "mgr-field-auth-mode",
            value: if auth_mode == AuthMode::Off {
                String::new()
            } else {
                issuer
            },
            value_key: view::auth_mode_key(auth_mode),
            inherited: spec.auth.is_none(),
            href: href("security"),
        },
        SummaryRow {
            label_key: "mgr-field-validation-mode",
            value: String::new(),
            value_key: view::validation_badge(v, env).key,
            inherited: v.is_none(),
            href: href("validation"),
        },
        SummaryRow {
            label_key: "mgr-field-terminology-server",
            value: term.clone().unwrap_or_default(),
            value_key: if term.is_some() { "" } else { "mgr-none" },
            inherited: spec.terminology.is_none(),
            href: href("terminology"),
        },
        SummaryRow {
            label_key: "mgr-field-cors",
            value: spec
                .cors
                .as_ref()
                .map(|c| c.allowed_origins.join(", "))
                .or_else(|| env.cors_origins.clone())
                .unwrap_or_default(),
            value_key: "",
            inherited: spec.cors.is_none(),
            href: href("security"),
        },
        SummaryRow {
            label_key: "mgr-field-rate-limit",
            value: spec
                .rate_limit
                .as_ref()
                .map(|r| {
                    format!(
                        "{} / {}",
                        r.anonymous_per_minute, r.authenticated_per_minute
                    )
                })
                .unwrap_or_default(),
            value_key: "",
            inherited: spec.rate_limit.is_none(),
            href: href("security"),
        },
        SummaryRow {
            label_key: "mgr-field-tenant",
            value: e.tenant_id().to_string(),
            value_key: "",
            inherited: false,
            href: href("config"),
        },
    ]
}

/// Revisions with a diff against the previous one.
fn revision_rows(revs: Vec<helios_endpoints::store::EndpointRevision>) -> Vec<RevisionRow> {
    // Newest first; the diff of each is against the next (older) entry.
    let mut out = Vec::with_capacity(revs.len());
    for (i, r) in revs.iter().enumerate() {
        let diff = match revs.get(i + 1) {
            Some(prev) => view::diff_rows(&prev.spec, &r.spec),
            None => Vec::new(),
        };
        out.push(RevisionRow {
            revision: r.revision,
            change_key: match r.change {
                helios_endpoints::store::ChangeKind::Create => "mgr-change-create",
                helios_endpoints::store::ChangeKind::Update => "mgr-change-update",
                helios_endpoints::store::ChangeKind::Status => "mgr-change-status",
            },
            at: r.changed_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
            by: r.changed_by.clone().unwrap_or_default(),
            diff,
        });
    }
    out
}

/// AuditEvent rows from a search Bundle.
pub(crate) fn audit_rows(bundle: &serde_json::Value) -> Vec<AuditRow> {
    let s = |v: Option<&serde_json::Value>| v.and_then(|v| v.as_str()).unwrap_or("").to_string();
    bundle["entry"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| e.get("resource"))
                .map(|r| {
                    let outcome = s(r.get("outcome"));
                    let agent = r["agent"]
                        .as_array()
                        .and_then(|a| a.first())
                        .map(|a| {
                            let who = s(a["who"].get("display"));
                            if who.is_empty() {
                                let name = s(a.get("name"));
                                if name.is_empty() {
                                    s(a["who"].get("reference"))
                                } else {
                                    name
                                }
                            } else {
                                who
                            }
                        })
                        .unwrap_or_default();
                    let what = r["entity"]
                        .as_array()
                        .and_then(|a| a.first())
                        .map(|e| {
                            let w = s(e["what"].get("reference"));
                            if w.is_empty() {
                                s(e.get("description"))
                            } else {
                                w
                            }
                        })
                        .unwrap_or_default();
                    let kind = {
                        let sub = r["subtype"]
                            .as_array()
                            .and_then(|a| a.first())
                            .map(|c| {
                                let d = s(c.get("display"));
                                if d.is_empty() { s(c.get("code")) } else { d }
                            })
                            .unwrap_or_default();
                        if sub.is_empty() {
                            let t = &r["type"];
                            let d = s(t.get("display"));
                            if d.is_empty() { s(t.get("code")) } else { d }
                        } else {
                            sub
                        }
                    };
                    AuditRow {
                        at: s(r.get("recorded")).replace('T', " "),
                        action: s(r.get("action")),
                        outcome_ok: outcome.is_empty() || outcome == "0",
                        outcome,
                        agent,
                        what,
                        kind,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One FHIR read through the UI's loopback self-call, as the requesting
/// user (their credential when in scope, else the outbound service one).
async fn fhir_get(
    state: &WebState,
    scope: &EndpointScope,
    path_and_query: &str,
) -> Result<serde_json::Value, String> {
    let url = format!(
        "{}{}{}",
        state.self_base_url,
        scope.self_call_prefix(),
        path_and_query
    );
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| e.to_string())?;
    let mut request = client
        .get(&url)
        .header("Accept", "application/fhir+json")
        .header("X-Tenant-ID", scope.endpoint.tenant_id());
    request = match crate::conformance::current_request_authorization() {
        Some(a) => request.header("Authorization", a),
        None => state
            .outbound_auth
            .authorize(request, &state.self_base_url)
            .await
            .map_err(|e| e.to_string())?,
    };
    let response = request.send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status().as_u16()));
    }
    response.json().await.map_err(|e| e.to_string())
}

fn scope_for(rt: &RequestTenant, runtime: &ManagerRuntime, e: &Endpoint) -> EndpointScope {
    rt.scope
        .as_deref()
        .cloned()
        .unwrap_or_else(|| EndpointScope {
            endpoint: Arc::new(e.clone()),
            routing: runtime.routing,
            explicit: true,
        })
}

pub(crate) async fn home(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path(ep): Path<String>,
) -> Response {
    let c = ctx(&state, locale, rv, &rt);
    let (c, e) = match load(c, &ep).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let backend = c.runtime.backend.clone();
    let scope = scope_for(&rt, &c.runtime, &e);
    let tenant = e.tenant_id().to_string();
    let (snap, (revs, (audit, jobs))) = futures_lite::future::zip(
        helios_observability::dashboard::snapshot(
            helios_observability::dashboard::DashboardWindow::default(),
            &tenant,
            &[],
            false,
        ),
        futures_lite::future::zip(
            backend.history(&ep),
            futures_lite::future::zip(
                fhir_get(&state, &scope, "/AuditEvent?_sort=-date&_count=6"),
                sync_jobs_of(&backend),
            ),
        ),
    )
    .await;
    let known = snap.as_ref().filter(|s| s.figures.is_known());
    let total = known.map(|s| s.total_resources).unwrap_or(0).max(1);
    let top_types = known
        .map(|s| {
            s.available
                .iter()
                .take(6)
                .map(|t| TypeRow {
                    name: t.resource_type.clone(),
                    total: t.total,
                    percent: ((t.total * 100) / total).min(100) as u8,
                    href: format!("/ui/e/{ep}/data/resources?type={}", t.resource_type),
                })
                .collect()
        })
        .unwrap_or_default();
    let others: Vec<Endpoint> = backend.list().await.unwrap_or_default();
    let other_specs: Vec<&EndpointSpec> = others.iter().map(|o| &o.spec).collect();
    let jobs = jobs.unwrap_or_default();
    let mut revisions = revision_rows(revs.unwrap_or_default());
    revisions.truncate(5);
    let (audit, audit_error) = match audit {
        Ok(b) => (audit_rows(&b), false),
        Err(_) => (Vec::new(), true),
    };
    let can_admin = c.can_admin();
    render(HomePage {
        head: EpHeader::new(&e, &c.server_base, &c.env),
        resources: known.map(|s| s.total_resources),
        types: known.map(|s| s.distinct_types),
        export_jobs: known.and_then(|s| s.export_jobs.map(|j| j.running + j.queued)),
        import_jobs: known.and_then(|s| s.import_jobs_active),
        top_types,
        summary: summary(&e, &c.env),
        revisions,
        audit,
        audit_error,
        sync_rows: view::sync_rows(&ep, &jobs),
        sync_available: sync_on(backend.as_ref()),
        warnings: forms::warnings(&e.spec, &other_specs),
        data_href: format!("/ui/e/{ep}/data"),
        can_admin,
        status: c.status,
        i18n: c.i18n,
        active_page: "ep-home",
    })
}

// ---------------------------------------------------------------------------
// Configuration sections
// ---------------------------------------------------------------------------

/// Every form value of the four configuration sections, as strings/bools.
#[derive(Debug, Clone, Default)]
pub(crate) struct FormValues {
    // general
    pub display_name: String,
    pub description: String,
    pub path: String,
    pub fhir_version: &'static str,
    pub console_access: &'static str,
    pub legacy_tenant_header: bool,
    pub labels: String,
    // security
    pub auth_inherit: bool,
    pub auth_mode: &'static str,
    pub auth_issuer: String,
    pub auth_jwks_url: String,
    pub auth_audience: String,
    pub auth_algorithms: String,
    pub auth_clock_skew: String,
    pub auth_jwks_max_age: String,
    pub scopes_accept_v1: bool,
    pub scopes_allow_patient: bool,
    pub scopes_allow_user: bool,
    pub scopes_allow_system: bool,
    pub scopes_non_compartment_deny: bool,
    pub smart_discover: bool,
    pub smart_authorization_endpoint: String,
    pub smart_token_endpoint: String,
    pub smart_jwks_uri: String,
    pub accept_admin: bool,
    pub cors_inherit: bool,
    pub cors_enabled: bool,
    pub cors_origins: String,
    pub cors_credentials: bool,
    pub cors_headers: String,
    pub rl_inherit: bool,
    pub rl_anonymous: String,
    pub rl_authenticated: String,
    pub rl_burst: String,
    // validation
    pub val_inherit: bool,
    pub val_mode: &'static str,
    pub val_terminology: &'static str,
    pub val_stored_profiles: bool,
    pub val_required: String,
    // terminology
    pub term_inherit: bool,
    pub term_server_url: String,
    pub term_public_url: String,
    pub term_timeout: String,
    pub term_token_set: bool,
}

fn opt_num(n: Option<u32>) -> String {
    n.map(|n| n.to_string()).unwrap_or_default()
}

impl FormValues {
    /// Show the server's effective values in the (disabled) fields of every
    /// section that inherits them.
    ///
    /// Before UIF an inheriting section showed the spec type's defaults —
    /// validation "Off" and terminology "Embedded value sets only" under a
    /// toggle reading "Server setting: Validation: log" — so the greyed form
    /// contradicted the line above it. Switching the toggle off now starts
    /// from what the endpoint actually does today.
    pub(crate) fn with_inherited(mut self, env: &view::EnvDefaults) -> Self {
        if self.auth_inherit {
            self.auth_mode = auth_mode_value(env.auth_mode);
            self.auth_issuer = env.auth_issuer.clone().unwrap_or_default();
            self.auth_audience = env.auth_audience.clone().unwrap_or_default();
        }
        if self.val_inherit {
            self.val_mode = validation_mode_value(env.validation_mode);
            self.val_terminology = terminology_validation_value(env.validation_terminology);
        }
        if self.term_inherit {
            self.term_server_url = env.terminology_server.clone().unwrap_or_default();
        }
        self
    }

    pub(crate) fn from_spec(spec: &EndpointSpec) -> Self {
        let auth = spec.auth.clone().unwrap_or_default();
        let cors = spec.cors.clone().unwrap_or_default();
        let rl = spec.rate_limit.clone().unwrap_or_default();
        let val = spec.validation.clone().unwrap_or_default();
        let term = spec.terminology.clone().unwrap_or_default();
        Self {
            display_name: spec.display_name.clone(),
            description: spec.description.clone().unwrap_or_default(),
            path: spec.path.as_str().to_string(),
            fhir_version: release_label(spec.fhir_version),
            console_access: match spec.console_access {
                helios_endpoints::ConsoleAccess::Off => "off",
                helios_endpoints::ConsoleAccess::ReadOnly => "read-only",
                helios_endpoints::ConsoleAccess::RoleBased => "role-based",
            },
            legacy_tenant_header: spec.legacy_tenant_header,
            labels: spec
                .labels
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("\n"),
            auth_inherit: spec.auth.is_none(),
            auth_mode: auth_mode_value(auth.mode),
            auth_issuer: auth.issuer.unwrap_or_default(),
            auth_jwks_url: auth.jwks_url.unwrap_or_default(),
            auth_audience: auth.audience.unwrap_or_default(),
            auth_algorithms: auth.algorithms.join(", "),
            auth_clock_skew: opt_num(auth.clock_skew_secs),
            auth_jwks_max_age: opt_num(auth.jwks_max_age_secs),
            scopes_accept_v1: auth.scopes.accept_v1,
            scopes_allow_patient: auth.scopes.allow_patient,
            scopes_allow_user: auth.scopes.allow_user,
            scopes_allow_system: auth.scopes.allow_system,
            scopes_non_compartment_deny: auth.scopes.patient_non_compartment
                == helios_endpoints::model::NonCompartmentPolicy::Deny,
            smart_discover: auth.smart.discover_from_issuer,
            smart_authorization_endpoint: auth.smart.authorization_endpoint.unwrap_or_default(),
            smart_token_endpoint: auth.smart.token_endpoint.unwrap_or_default(),
            smart_jwks_uri: auth.smart.jwks_uri.unwrap_or_default(),
            accept_admin: auth.accept_admin_credential,
            cors_inherit: spec.cors.is_none(),
            cors_enabled: cors.enabled,
            cors_origins: cors.allowed_origins.join("\n"),
            cors_credentials: cors.allow_credentials,
            cors_headers: cors.extra_headers.join(", "),
            rl_inherit: spec.rate_limit.is_none(),
            rl_anonymous: rl.anonymous_per_minute.to_string(),
            rl_authenticated: rl.authenticated_per_minute.to_string(),
            rl_burst: rl.burst_seconds.to_string(),
            val_inherit: spec.validation.is_none(),
            val_mode: validation_mode_value(val.mode),
            val_terminology: terminology_validation_value(val.terminology),
            val_stored_profiles: val.stored_profiles,
            val_required: val
                .required_profiles
                .iter()
                .map(|r| format!("{} {}", r.resource_type, r.profile))
                .collect::<Vec<_>>()
                .join("\n"),
            term_inherit: spec.terminology.is_none(),
            term_server_url: term.server_url.unwrap_or_default(),
            term_public_url: term.public_url.unwrap_or_default(),
            term_timeout: opt_num(term.timeout_secs),
            term_token_set: term.token.is_some(),
        }
    }
}

/// Inherited server values shown next to each "Inherit" toggle.
pub(crate) struct EnvView {
    pub auth_mode_key: &'static str,
    pub auth_issuer: String,
    pub auth_audience: String,
    pub auth_jwks: String,
    pub validation_key: &'static str,
    pub terminology_server: String,
    pub cors_origins: String,
    pub rate_limit: String,
}

impl EnvView {
    fn new(env: &EnvDefaults) -> Self {
        Self {
            auth_mode_key: view::auth_mode_key(env.auth_mode),
            auth_issuer: env.auth_issuer.clone().unwrap_or_default(),
            auth_audience: env.auth_audience.clone().unwrap_or_default(),
            auth_jwks: env.auth_jwks_url.clone().unwrap_or_default(),
            validation_key: view::validation_badge(None, env).key,
            terminology_server: env
                .terminology_server
                .clone()
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| {
                    // Embedded terminology has no URL: say what inheriting
                    // means (the server's own /tx).
                    if crate::editor::terminology_is_embedded() {
                        "/tx".into()
                    } else {
                        String::new()
                    }
                }),
            cors_origins: env.cors_origins.clone().unwrap_or_else(|| "*".into()),
            rate_limit: format!(
                "{} / {}",
                env.rate_limit.0.clone().unwrap_or_else(|| "0".into()),
                env.rate_limit.1.clone().unwrap_or_else(|| "0".into())
            ),
        }
    }
}

#[derive(Template)]
#[template(path = "pages/manager/endpoint-section.html")]
struct SectionPage {
    status: Status,
    i18n: I18n,
    active_page: &'static str,
    title_key: &'static str,
    section: &'static str,
    head: EpHeader,
    v: FormValues,
    errors: FieldErrors,
    env: EnvView,
    can_edit: bool,
    locked_key: &'static str,
    saved: bool,
    conflict: Option<u64>,
    revision: u64,
    warnings: Vec<&'static str>,
    versions: Vec<&'static str>,
    data_href: String,
    /// The failed verification that stopped a Save (UX-06).
    verify_block: Option<super::verify::ReportView>,
}

impl SectionPage {
    /// The error of a field path, or `""`.
    fn err(&self, path: &str) -> String {
        self.errors
            .get(path)
            .map(|m| forms::error_message(&self.i18n, m))
            .unwrap_or_default()
    }

    /// The error summary: each problem with its field label and a link that
    /// focuses the field (UX-21, UX-03).
    fn error_links(&self) -> Vec<forms::ErrorLink> {
        forms::error_links(&self.i18n, &self.errors)
    }

    /// Whether the field has an error (for `aria-invalid`).
    fn bad(&self, path: &str) -> bool {
        self.errors.contains_key(path)
    }
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct SavedQuery {
    #[serde(default)]
    saved: Option<String>,
}

fn locked_key(c: &Ctx, e: &Endpoint) -> &'static str {
    if e.spec.managed_by == ManagedBy::Env {
        "mgr-locked-env"
    } else if !c.runtime.backend.writable() {
        "mgr-locked-readonly"
    } else if c.role < ConsoleRole::Admin {
        "mgr-locked-role"
    } else {
        ""
    }
}

fn enabled_versions() -> Vec<&'static str> {
    crate::search_params::enabled_versions()
        .into_iter()
        .map(|v| v.as_str())
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn section_response(
    c: Ctx,
    section: Section,
    e: &Endpoint,
    spec: &EndpointSpec,
    errors: FieldErrors,
    saved: bool,
    conflict: Option<u64>,
    others: &[Endpoint],
) -> Response {
    section_response_full(c, section, e, spec, errors, saved, conflict, others, None)
}

/// Runs the auth verification a Save needs (UX-06, UX-03): fills an empty
/// JWKS URL from discovery, and answers the report when it has failures
/// and the user did not choose "Save anyway". `None` = go ahead.
async fn verify_before_save(
    c: &Ctx,
    endpoint: Option<&str>,
    auth: Option<&mut EndpointAuth>,
    overridden: bool,
) -> Option<super::verify::ReportView> {
    let auth = auth?;
    if auth.mode == helios_endpoints::AuthMode::Off {
        return None;
    }
    let report = c
        .runtime
        .backend
        .verify_auth(endpoint, auth, None)
        .await
        .ok()?;
    if auth.jwks_url.as_deref().is_none_or(|u| u.trim().is_empty())
        && let Some(jwks) = report.suggestions.get("jwksUrl")
    {
        auth.jwks_url = Some(jwks.clone());
    }
    if report.failures() == 0 || overridden {
        return None;
    }
    Some(super::verify::view(
        &c.i18n,
        &report,
        super::verify::AUTH_FIELDS,
    ))
}

#[allow(clippy::too_many_arguments)]
fn section_response_full(
    c: Ctx,
    section: Section,
    e: &Endpoint,
    spec: &EndpointSpec,
    errors: FieldErrors,
    saved: bool,
    conflict: Option<u64>,
    others: &[Endpoint],
    verify_block: Option<super::verify::ReportView>,
) -> Response {
    let locked = locked_key(&c, e);
    let other_specs: Vec<&EndpointSpec> = others.iter().map(|o| &o.spec).collect();
    let code = if !errors.is_empty() || verify_block.is_some() {
        StatusCode::UNPROCESSABLE_ENTITY
    } else if conflict.is_some() {
        StatusCode::CONFLICT
    } else {
        StatusCode::OK
    };
    let mut response = render(SectionPage {
        head: EpHeader::new(e, &c.server_base, &c.env),
        section: section.slug(),
        v: FormValues::from_spec(spec).with_inherited(&c.env),
        errors,
        env: EnvView::new(&c.env),
        can_edit: locked.is_empty(),
        locked_key: locked,
        saved,
        conflict,
        revision: e.revision,
        // Each warning on the section it is about (UX-15), not on all four.
        warnings: forms::warnings(spec, &other_specs)
            .into_iter()
            .filter(|w| forms::warning_section(w) == section)
            .collect(),
        versions: enabled_versions(),
        data_href: format!("/ui/e/{}/data", e.name()),
        verify_block,
        title_key: match section {
            Section::General => "mgr-nav-config",
            Section::Security => "mgr-nav-security",
            Section::Validation => "mgr-nav-validation",
            Section::Terminology => "mgr-nav-terminology",
        },
        active_page: match section {
            Section::General => "ep-config",
            Section::Security => "ep-security",
            Section::Validation => "ep-validation",
            Section::Terminology => "ep-terminology",
        },
        status: c.status,
        i18n: c.i18n,
    });
    *response.status_mut() = code;
    response
}

pub(crate) async fn section_page(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path((ep, slug)): Path<(String, String)>,
    Query(q): Query<SavedQuery>,
) -> Response {
    let c = ctx(&state, locale, rv, &rt);
    let Some(section) = Section::from_slug(&slug) else {
        return not_found(c);
    };
    let (c, e) = match load(c, &ep).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let others = c.runtime.backend.list().await.unwrap_or_default();
    let spec = e.spec.clone();
    section_response(
        c,
        section,
        &e,
        &spec,
        FieldErrors::new(),
        q.saved.is_some(),
        None,
        &others,
    )
}

pub(crate) async fn save_section(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    principal: Option<Extension<ConsolePrincipal>>,
    Path((ep, slug)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let c = ctx(&state, locale, rv, &rt);
    let Some(section) = Section::from_slug(&slug) else {
        return not_found(c);
    };
    let (c, e) = match load(c, &ep).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if !locked_key(&c, &e).is_empty() {
        return forbidden(c);
    }
    let form = Form::parse(&body);
    let mut errors = FieldErrors::new();
    let mut spec = e.spec.clone();
    let secrets = forms::apply_section(section, &form, &mut spec, &mut errors);
    let others = c.runtime.backend.list().await.unwrap_or_default();
    if !errors.is_empty() {
        return section_response(c, section, &e, &spec, errors, false, None, &others);
    }
    // Configure-then-verify on Save (UX-06): a token configuration that
    // fails verification is not saved silently; the JWKS URL left empty is
    // the one the issuer publishes (UX-03).
    let blocked = if section == Section::Security {
        verify_before_save(
            &c,
            Some(&ep),
            spec.auth.as_mut(),
            form.checked("verify_override"),
        )
        .await
    } else {
        None
    };
    // Field errors come first: they say exactly what to fill in.
    if let Err(v) = spec.validate() {
        for fe in v.0 {
            errors.entry(fe.field).or_insert(fe.message);
        }
        return section_response(c, section, &e, &spec, errors, false, None, &others);
    }
    if let Some(blocked) = blocked {
        return section_response_full(
            c,
            section,
            &e,
            &spec,
            FieldErrors::new(),
            false,
            None,
            &others,
            Some(blocked),
        );
    }
    // "Overwrite" after a conflict posts the revision the user was shown.
    let expected = form.revision().unwrap_or(e.revision);
    let actor = actor(&rt, principal.as_ref().map(|p| &p.0));
    match c
        .runtime
        .backend
        .update(&ep, expected, spec.clone(), secrets, &actor)
        .await
    {
        Ok(_) => {
            if section == Section::Security {
                crate::idp::security::after_save(&ep, &spec).await;
            }
            Redirect::to(&format!("/ui/e/{ep}/{}?saved=1", section.slug())).into_response()
        }
        Err(BackendError::Conflict { current }) => {
            section_response(c, section, &e, &spec, errors, false, Some(current), &others)
        }
        Err(
            err @ (BackendError::Invalid(_)
            | BackendError::Duplicate { .. }
            | BackendError::Forbidden(_)),
        ) => section_response(
            c,
            section,
            &e,
            &spec,
            err.field_errors(),
            false,
            None,
            &others,
        ),
        Err(err) => unavailable(c, &err),
    }
}

#[derive(Template)]
#[template(path = "partials/manager/diff-preview.html")]
struct DiffPreview {
    i18n: I18n,
    rows: Vec<DiffRow>,
    errors: FieldErrors,
    warnings: Vec<&'static str>,
}

/// `POST /ui/e/{ep}/{section}/preview` — the save bar's "Review changes".
pub(crate) async fn preview_section(
    State(state): State<WebState>,
    locale: RequestLocale,
    Path((ep, slug)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let i18n = I18n::new(locale);
    let Some(section) = Section::from_slug(&slug) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let rt = runtime(&state);
    let Ok(Some(e)) = rt.backend.get(&ep).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let form = Form::parse(&body);
    let mut errors = FieldErrors::new();
    let mut spec = e.spec.clone();
    forms::apply_section(section, &form, &mut spec, &mut errors);
    if let Err(v) = spec.validate() {
        for fe in v.0 {
            errors.entry(fe.field).or_insert(fe.message);
        }
    }
    let before = redacted_spec(&e.spec);
    let after = redacted_spec(&spec);
    let others = rt.backend.list().await.unwrap_or_default();
    let other_specs: Vec<&EndpointSpec> = others.iter().map(|o| &o.spec).collect();
    render(DiffPreview {
        i18n,
        rows: view::diff_rows(&before, &after),
        errors,
        warnings: forms::warnings(&spec, &other_specs),
    })
}

fn redacted_spec(spec: &EndpointSpec) -> serde_json::Value {
    let mut v = serde_json::to_value(spec).unwrap_or_default();
    helios_endpoints::secret::redact_sealed(&mut v);
    v
}

#[derive(Deserialize)]
pub(crate) struct StatusForm {
    action: String,
    #[serde(default)]
    revision: Option<u64>,
    #[serde(default)]
    confirm: String,
    #[serde(default)]
    back: String,
}

/// `POST /ui/e/{ep}/status` — enable / disable / delete.
pub(crate) async fn set_status(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    principal: Option<Extension<ConsolePrincipal>>,
    Path(ep): Path<String>,
    axum::Form(form): axum::Form<StatusForm>,
) -> Response {
    let c = ctx(&state, locale, rv, &rt);
    let (c, e) = match load(c, &ep).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if !locked_key(&c, &e).is_empty() || e.spec.is_default() {
        return forbidden(c);
    }
    let status = match form.action.as_str() {
        "enable" => EndpointStatus::Enabled,
        "disable" => EndpointStatus::Disabled,
        "delete" => {
            if form.confirm.trim() != ep {
                let others = c.runtime.backend.list().await.unwrap_or_default();
                let mut errs = FieldErrors::new();
                errs.insert("confirm".into(), c.i18n.t("mgr-danger-confirm-mismatch"));
                let spec = e.spec.clone();
                return section_response(
                    c,
                    Section::General,
                    &e,
                    &spec,
                    errs,
                    false,
                    None,
                    &others,
                );
            }
            EndpointStatus::Deleted
        }
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    let actor = actor(&rt, principal.as_ref().map(|p| &p.0));
    match c
        .runtime
        .backend
        .set_status(&ep, form.revision.unwrap_or(e.revision), status, &actor)
        .await
    {
        Ok(_) if status == EndpointStatus::Deleted => Redirect::to(&format!(
            "/ui?deleted={ep}&tenant={}",
            form_urlencoded::byte_serialize(e.tenant_id().as_bytes()).collect::<String>()
        ))
        .into_response(),
        Ok(_) => {
            let back = crate::navigation::safe_ui_return(Some(&form.back))
                .unwrap_or_else(|| format!("/ui/e/{ep}"));
            let action = if status == EndpointStatus::Enabled {
                "enabled"
            } else {
                "disabled"
            };
            let back = back.replace(&format!("changed={ep}"), &format!("{action}={ep}"));
            Redirect::to(&back).into_response()
        }
        Err(BackendError::Conflict { current }) => {
            let others = c.runtime.backend.list().await.unwrap_or_default();
            let spec = e.spec.clone();
            section_response(
                c,
                Section::General,
                &e,
                &spec,
                FieldErrors::new(),
                false,
                Some(current),
                &others,
            )
        }
        Err(err) => unavailable(c, &err),
    }
}

// ---------------------------------------------------------------------------
// Sync, audit, history
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "pages/manager/endpoint-sync.html")]
struct SyncPage {
    status: Status,
    i18n: I18n,
    active_page: &'static str,
    head: EpHeader,
    sync_rows: Vec<SyncRow>,
    available: bool,
    can_admin: bool,
}

pub(crate) async fn sync_page(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path(ep): Path<String>,
) -> Response {
    let c = ctx(&state, locale, rv, &rt);
    let (c, e) = match load(c, &ep).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let jobs = sync_jobs_of(&c.runtime.backend).await.unwrap_or_default();
    let can_admin = c.can_admin();
    render(SyncPage {
        head: EpHeader::new(&e, &c.server_base, &c.env),
        sync_rows: view::sync_rows(&ep, &jobs),
        available: sync_on(c.runtime.backend.as_ref()),
        can_admin,
        status: c.status,
        i18n: c.i18n,
        active_page: "ep-sync",
    })
}

#[derive(Template)]
#[template(path = "pages/manager/endpoint-audit.html")]
struct AuditPage {
    status: Status,
    i18n: I18n,
    active_page: &'static str,
    head: EpHeader,
    rows: Vec<AuditRow>,
    error: String,
    count: u32,
    has_more: bool,
    more_href: String,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct AuditQuery {
    #[serde(default)]
    count: Option<u32>,
}

pub(crate) async fn audit_page(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path(ep): Path<String>,
    Query(q): Query<AuditQuery>,
) -> Response {
    let c = ctx(&state, locale, rv, &rt);
    let (c, e) = match load(c, &ep).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let count = q.count.unwrap_or(50).clamp(10, 500);
    let scope = scope_for(&rt, &c.runtime, &e);
    let (rows, error) = match fhir_get(
        &state,
        &scope,
        &format!("/AuditEvent?_sort=-date&_count={count}"),
    )
    .await
    {
        Ok(b) => (audit_rows(&b), String::new()),
        Err(err) => (Vec::new(), err),
    };
    render(AuditPage {
        head: EpHeader::new(&e, &c.server_base, &c.env),
        more_href: format!("/ui/e/{ep}/audit?count={}", (count * 2).min(500)),
        has_more: rows.len() as u32 >= count && count < 500,
        rows,
        error,
        count,
        status: c.status,
        i18n: c.i18n,
        active_page: "ep-audit",
    })
}

#[derive(Template)]
#[template(path = "pages/manager/endpoint-history.html")]
struct HistoryPage {
    status: Status,
    i18n: I18n,
    active_page: &'static str,
    head: EpHeader,
    rows: Vec<RevisionRow>,
    error: String,
}

pub(crate) async fn history_page(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path(ep): Path<String>,
) -> Response {
    let c = ctx(&state, locale, rv, &rt);
    let (c, e) = match load(c, &ep).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let (rows, error) = match c.runtime.backend.history(&ep).await {
        Ok(revs) => (revision_rows(revs), String::new()),
        Err(err) => (Vec::new(), format!("{err:?}")),
    };
    render(HistoryPage {
        head: EpHeader::new(&e, &c.server_base, &c.env),
        rows,
        error,
        status: c.status,
        i18n: c.i18n,
        active_page: "ep-history",
    })
}

/// `/ui/e/{ep}/subscriptions` and `/bulk` live with the data pages.
pub(crate) async fn to_data(uri: axum::http::Uri) -> Response {
    let path = uri.path();
    let target = if let Some(base) = path.strip_suffix("/bulk") {
        format!("{base}/data/bulk-export")
    } else if let Some(base) = path.strip_suffix("/subscriptions") {
        format!("{base}/data/subscriptions")
    } else {
        "/ui".to_string()
    };
    Redirect::to(&target).into_response()
}

// ---------------------------------------------------------------------------
// New endpoint and duplicate
// ---------------------------------------------------------------------------

/// Values of the new-endpoint form (re-rendered on errors).
#[derive(Debug, Clone)]
pub(crate) struct NewValues {
    pub display_name: String,
    pub name: String,
    pub path: String,
    pub description: String,
    pub fhir_version: String,
    pub data_source: String,
    pub adopt_tenant: String,
    pub duplicate_from: String,
    pub duplicate_data: String,
    pub auth_choice: String,
    pub v: FormValues,
    pub val_choice: String,
    pub term_choice: String,
    pub labels: String,
}

impl NewValues {
    fn empty(default_version: &'static str) -> Self {
        let mut spec = EndpointSpec::named(
            helios_endpoints::EndpointName::parse("new").expect("valid"),
            "",
        );
        spec.auth = Some(Default::default());
        spec.validation = Some(Default::default());
        Self {
            display_name: String::new(),
            name: String::new(),
            path: String::new(),
            description: String::new(),
            fhir_version: default_version.to_string(),
            data_source: "empty".into(),
            adopt_tenant: String::new(),
            duplicate_from: String::new(),
            duplicate_data: "none".into(),
            auth_choice: "inherit".into(),
            v: FormValues::from_spec(&spec),
            val_choice: "inherit".into(),
            term_choice: "inherit".into(),
            labels: String::new(),
        }
    }

    fn from_form(form: &Form, default_version: &'static str) -> Self {
        let mut v = Self::empty(default_version);
        let mut spec = EndpointSpec::named(
            helios_endpoints::EndpointName::parse("new").expect("valid"),
            "",
        );
        let mut errs = FieldErrors::new();
        spec.auth = Some(forms::parse_auth(form, None, &mut errs));
        spec.validation = Some(forms::parse_validation(form, &mut errs));
        let mut secrets = super::backend::SecretChanges::new();
        spec.terminology = Some(forms::parse_terminology(
            form,
            None,
            &mut errs,
            &mut secrets,
        ));
        v.v = FormValues::from_spec(&spec);
        v.display_name = form.get("display_name").into();
        v.name = form.get("name").into();
        v.path = form.get("path").into();
        v.description = form.get("description").into();
        if !form.get("fhir_version").is_empty() {
            v.fhir_version = form.get("fhir_version").into();
        }
        for (field, value) in [
            (&mut v.data_source, "data_source"),
            (&mut v.adopt_tenant, "adopt_tenant"),
            (&mut v.duplicate_from, "duplicate_from"),
            (&mut v.duplicate_data, "duplicate_data"),
            (&mut v.auth_choice, "auth_choice"),
            (&mut v.val_choice, "val_choice"),
            (&mut v.term_choice, "term_choice"),
        ] {
            if !form.get(value).is_empty() {
                *field = form.get(value).to_string();
            }
        }
        v.labels = form.get("labels").into();
        v
    }
}

/// An endpoint choice in a select.
pub(crate) struct Choice {
    pub value: String,
    pub label: String,
}

#[derive(Template)]
#[template(path = "pages/manager/endpoint-new.html")]
struct NewPage {
    status: Status,
    i18n: I18n,
    active_page: &'static str,
    nv: NewValues,
    errors: FieldErrors,
    endpoints: Vec<Choice>,
    tenants: Vec<String>,
    env: EnvView,
    versions: Vec<&'static str>,
    server_base: String,
    /// The failed verification that stopped Create (UX-06).
    verify_block: Option<super::verify::ReportView>,
}

impl NewPage {
    fn err(&self, path: &str) -> String {
        self.errors
            .get(path)
            .map(|m| forms::error_message(&self.i18n, m))
            .unwrap_or_default()
    }

    /// The error summary: each problem with its field label and a link that
    /// focuses the field (UX-21, UX-03).
    fn error_links(&self) -> Vec<forms::ErrorLink> {
        forms::error_links(&self.i18n, &self.errors)
    }

    fn bad(&self, path: &str) -> bool {
        self.errors.contains_key(path)
    }
}

/// Tenants of the store not bound to any endpoint ("adopt existing").
async fn unbound_tenants(state: &WebState, endpoints: &[Endpoint]) -> Vec<String> {
    let Some(registry) = &state.tenants else {
        return Vec::new();
    };
    let bound: std::collections::BTreeSet<&str> = endpoints.iter().map(|e| e.tenant_id()).collect();
    registry
        .list_tenants()
        .await
        .map(|records| {
            records
                .into_iter()
                .map(|r| r.id)
                .filter(|id| !bound.contains(id.as_str()))
                .collect()
        })
        .unwrap_or_default()
}

fn default_version_label(state: &WebState) -> &'static str {
    state.fhir_version.as_str()
}

async fn new_response(state: &WebState, c: Ctx, nv: NewValues, errors: FieldErrors) -> Response {
    new_response_full(state, c, nv, errors, None).await
}

async fn new_response_full(
    state: &WebState,
    c: Ctx,
    nv: NewValues,
    errors: FieldErrors,
    verify_block: Option<super::verify::ReportView>,
) -> Response {
    let endpoints = c.runtime.backend.list().await.unwrap_or_default();
    let tenants = unbound_tenants(state, &endpoints).await;
    let code = if errors.is_empty() && verify_block.is_none() {
        StatusCode::OK
    } else {
        StatusCode::UNPROCESSABLE_ENTITY
    };
    let mut response = render(NewPage {
        nv,
        errors,
        verify_block,
        endpoints: endpoints
            .iter()
            .map(|e| Choice {
                value: e.name().to_string(),
                label: format!("{} ({})", e.spec.display_name, e.name()),
            })
            .collect(),
        tenants,
        env: EnvView::new(&c.env),
        versions: enabled_versions(),
        server_base: c.server_base.clone(),
        status: c.status,
        i18n: c.i18n,
        active_page: "overview",
    });
    *response.status_mut() = code;
    response
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct NewQuery {
    #[serde(default)]
    from: Option<String>,
}

pub(crate) async fn new_page(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Query(q): Query<NewQuery>,
) -> Response {
    let c = ctx(&state, locale, rv, &rt);
    if !c.can_admin() {
        return forbidden(c);
    }
    let mut nv = NewValues::empty(default_version_label(&state));
    if let Some(from) = q.from {
        nv.data_source = "duplicate".into();
        nv.duplicate_from = from;
    }
    new_response(&state, c, nv, FieldErrors::new()).await
}

pub(crate) async fn create(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    principal: Option<Extension<ConsolePrincipal>>,
    body: Bytes,
) -> Response {
    let c = ctx(&state, locale, rv, &rt);
    if !c.can_admin() {
        return forbidden(c);
    }
    let mut form = Form::parse(&body);
    // Configure-then-verify on Create (UX-02/03/06): the JWKS URL comes from
    // discovery when left empty; a failing token configuration needs an
    // explicit "Create anyway".
    if form.get("auth_choice") == "custom" && !form.get("auth_issuer").is_empty() {
        let mut errs = FieldErrors::new();
        let mut auth = forms::parse_auth(&form, None, &mut errs);
        let blocked =
            verify_before_save(&c, None, Some(&mut auth), form.checked("verify_override")).await;
        if let Some(jwks) = auth.jwks_url.as_deref()
            && form.get("auth_jwks_url").is_empty()
        {
            form.set("auth_jwks_url", jwks);
        }
        if let Some(block) = blocked {
            let nv = NewValues::from_form(&form, default_version_label(&state));
            if let Err(errors) = forms::parse_new(&form) {
                return new_response(&state, c, nv, errors).await;
            }
            return new_response_full(&state, c, nv, FieldErrors::new(), Some(block)).await;
        }
    }
    let nv = NewValues::from_form(&form, default_version_label(&state));
    let actor = actor(&rt, principal.as_ref().map(|p| &p.0));
    let parsed = match forms::parse_new(&form) {
        Ok(p) => p,
        Err(errors) => return new_response(&state, c, nv, errors).await,
    };
    let result = match parsed {
        NewEndpoint::Create(spec, secrets) => c
            .runtime
            .backend
            .create(*spec, secrets, &actor)
            .await
            .map(|e| e.name().to_string()),
        NewEndpoint::Duplicate { source, request } => c
            .runtime
            .backend
            .duplicate(&source, request, &actor)
            .await
            .map(|o| o.endpoint),
    };
    match result {
        Ok(name) => Redirect::to(&format!("/ui/e/{name}?created=1")).into_response(),
        Err(BackendError::NotImplemented) => {
            let mut errors = FieldErrors::new();
            errors.insert("dataSource".into(), c.i18n.t("mgr-duplicate-unavailable"));
            new_response(&state, c, nv, errors).await
        }
        Err(err @ BackendError::Unavailable(_)) => unavailable(c, &err),
        Err(err) => {
            let errors = err.field_errors();
            new_response(&state, c, nv, errors).await
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct CheckQuery {
    #[serde(default)]
    name: String,
    #[serde(default)]
    path: String,
}

#[derive(Template)]
#[template(path = "partials/manager/availability.html")]
struct Availability {
    ok: bool,
    message: String,
    url: String,
}

/// `GET /ui/endpoints/check?name=&path=` — live availability of a name/path.
pub(crate) async fn check_name(
    State(state): State<WebState>,
    locale: RequestLocale,
    Query(q): Query<CheckQuery>,
) -> Response {
    let i18n = I18n::new(locale);
    let name = q.name.trim();
    let path = if q.path.trim().is_empty() {
        name
    } else {
        q.path.trim()
    };
    let endpoints = runtime(&state).backend.list().await.unwrap_or_default();
    let (ok, message) = if name.is_empty() {
        (false, i18n.t("mgr-check-empty"))
    } else if let Err(e) = helios_endpoints::EndpointName::parse(name) {
        (false, e.to_string())
    } else if name == helios_endpoints::DEFAULT_ENDPOINT_NAME {
        (false, i18n.t("mgr-check-default"))
    } else if let Err(e) = helios_endpoints::EndpointPath::parse(path) {
        (false, e.to_string())
    } else if let Some(by) = endpoints
        .iter()
        .find(|e| e.name() == name || e.spec.path.as_str() == path || e.tenant_id() == name)
    {
        (
            false,
            i18n.t_arg("mgr-check-taken", "endpoint", by.name().to_string()),
        )
    } else {
        (true, i18n.t("mgr-check-available"))
    };
    render(Availability {
        ok,
        message,
        url: format!("{}/{path}", state.public_base_url.trim_end_matches('/')),
    })
}

#[derive(Template)]
#[template(path = "partials/manager/new-preview.html")]
struct NewPreview {
    i18n: I18n,
    json: String,
    errors: FieldErrors,
    warnings: Vec<&'static str>,
    duplicate: bool,
    summary: Vec<ReviewRow>,
}

impl NewPreview {
    fn error_links(&self) -> Vec<forms::ErrorLink> {
        forms::error_links(&self.i18n, &self.errors)
    }
}

/// `POST /ui/endpoints/preview` — the wizard's review step.
pub(crate) async fn preview_new(
    State(state): State<WebState>,
    locale: RequestLocale,
    body: Bytes,
) -> Response {
    let i18n = I18n::new(locale);
    let mut form = Form::parse(&body);
    // As Create does (UX-03): an empty JWKS URL is the one discovery names.
    if form.get("auth_choice") == "custom"
        && !form.get("auth_issuer").is_empty()
        && form.get("auth_jwks_url").is_empty()
    {
        let mut errs = FieldErrors::new();
        let auth = forms::parse_auth(&form, None, &mut errs);
        if let Ok(report) = runtime(&state).backend.verify_auth(None, &auth, None).await
            && let Some(jwks) = report.suggestions.get("jwksUrl")
        {
            form.set("auth_jwks_url", jwks);
        }
    }
    let others = runtime(&state).backend.list().await.unwrap_or_default();
    let other_specs: Vec<&EndpointSpec> = others.iter().map(|o| &o.spec).collect();
    let mut summary = Vec::new();
    let (json, errors, warnings, duplicate) = match forms::parse_new(&form) {
        Ok(NewEndpoint::Create(spec, _)) => {
            summary = review_rows(&i18n, &spec);
            (
                serde_json::to_string_pretty(&redacted_spec(&spec)).unwrap_or_default(),
                FieldErrors::new(),
                forms::warnings(&spec, &other_specs),
                false,
            )
        }
        Ok(NewEndpoint::Duplicate { source, request }) => (
            serde_json::to_string_pretty(&serde_json::json!({
                "duplicateOf": source,
                "request": request,
            }))
            .unwrap_or_default(),
            FieldErrors::new(),
            Vec::new(),
            true,
        ),
        Err(errors) => (String::new(), errors, Vec::new(), false),
    };
    render(NewPreview {
        i18n,
        json,
        errors,
        warnings,
        duplicate,
        summary,
    })
}

/// One line of the wizard's readable review (UX-22): step, label, value.
pub(crate) struct ReviewRow {
    pub step: usize,
    pub label: String,
    pub value: String,
}

fn review_rows(i18n: &I18n, spec: &EndpointSpec) -> Vec<ReviewRow> {
    let row = |step: usize, key: &str, value: String| ReviewRow {
        step,
        label: i18n.t(key),
        value,
    };
    let inherited = i18n.t("mgr-inherit-label");
    let mut rows = vec![
        row(0, "mgr-field-display-name", spec.display_name.clone()),
        row(0, "mgr-field-name", spec.name.to_string()),
        row(
            0,
            "mgr-field-path",
            if spec.path.is_root() {
                "/".into()
            } else {
                format!("/{}", spec.path.as_str())
            },
        ),
        row(
            1,
            "mgr-field-fhir-version",
            release_label(spec.fhir_version).to_string(),
        ),
    ];
    match &spec.auth {
        None => rows.push(row(2, "mgr-field-auth-mode", inherited.clone())),
        Some(a) => {
            rows.push(row(
                2,
                "mgr-field-auth-mode",
                i18n.t(view::auth_mode_key(a.mode)),
            ));
            if a.mode != AuthMode::Off {
                rows.push(row(
                    2,
                    "mgr-field-issuer",
                    a.issuer.clone().unwrap_or_default(),
                ));
                rows.push(row(
                    2,
                    "mgr-field-audience",
                    a.audience.clone().unwrap_or_else(|| "—".into()),
                ));
                rows.push(row(
                    2,
                    "mgr-field-jwks",
                    a.jwks_url.clone().unwrap_or_default(),
                ));
            }
        }
    }
    rows.push(row(
        3,
        "mgr-field-validation-mode",
        match &spec.validation {
            None => inherited.clone(),
            Some(v) => format!("{:?}", v.mode).to_lowercase(),
        },
    ));
    rows.push(row(
        3,
        "mgr-term-server",
        match &spec.terminology {
            None => inherited,
            Some(t) => t.server_url.clone().unwrap_or_default(),
        },
    ));
    rows
}

#[derive(Template)]
#[template(path = "pages/manager/endpoint-duplicate.html")]
struct DuplicatePage {
    status: Status,
    i18n: I18n,
    active_page: &'static str,
    head: EpHeader,
    suggested: String,
    suggested_display: String,
    errors: FieldErrors,
    can_admin: bool,
}

impl DuplicatePage {
    fn err(&self, path: &str) -> String {
        self.errors
            .get(path)
            .map(|m| forms::error_message(&self.i18n, m))
            .unwrap_or_default()
    }
}

pub(crate) async fn duplicate_page(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path(ep): Path<String>,
) -> Response {
    let c = ctx(&state, locale, rv, &rt);
    let (c, e) = match load(c, &ep).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let can_admin = c.can_admin();
    let suggested = if e.spec.is_default() {
        "default-copy".to_string()
    } else {
        format!("{}-copy", e.name())
    };
    render(DuplicatePage {
        head: EpHeader::new(&e, &c.server_base, &c.env),
        suggested_display: c
            .i18n
            .t_arg("mgr-copy-of", "name", e.spec.display_name.clone()),
        suggested,
        errors: FieldErrors::new(),
        can_admin,
        status: c.status,
        i18n: c.i18n,
        active_page: "ep-home",
    })
}

pub(crate) async fn duplicate(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    principal: Option<Extension<ConsolePrincipal>>,
    Path(ep): Path<String>,
    body: Bytes,
) -> Response {
    let c = ctx(&state, locale, rv, &rt);
    let (c, e) = match load(c, &ep).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if !c.can_admin() {
        return forbidden(c);
    }
    let form = Form::parse(&body);
    // Reuse the new-endpoint parser with the source fixed.
    let mut pairs: Vec<(String, String)> = form_urlencoded::parse(&body)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    pairs.push(("data_source".into(), "duplicate".into()));
    pairs.push(("duplicate_from".into(), ep.clone()));
    let encoded = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    let actor = actor(&rt, principal.as_ref().map(|p| &p.0));
    let errors = match forms::parse_new(&Form::parse(encoded.as_bytes())) {
        Ok(NewEndpoint::Duplicate { source, request }) => {
            match c.runtime.backend.duplicate(&source, request, &actor).await {
                Ok(o) => {
                    return Redirect::to(&format!("/ui/e/{}?created=1", o.endpoint))
                        .into_response();
                }
                Err(BackendError::NotImplemented) => {
                    let mut m = FieldErrors::new();
                    m.insert(String::new(), c.i18n.t("mgr-duplicate-unavailable"));
                    m
                }
                Err(err) => err.field_errors(),
            }
        }
        Ok(_) => FieldErrors::new(),
        Err(errs) => errs,
    };
    let can_admin = c.can_admin();
    let mut response = render(DuplicatePage {
        head: EpHeader::new(&e, &c.server_base, &c.env),
        suggested: form.get("name").to_string(),
        suggested_display: form.get("display_name").to_string(),
        errors,
        can_admin,
        status: c.status,
        i18n: c.i18n,
        active_page: "ep-home",
    });
    *response.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
    response
}

// ---------------------------------------------------------------------------
// About
// ---------------------------------------------------------------------------

/// A third-party component listed on the About page.
pub(crate) struct License {
    pub name: &'static str,
    pub license: &'static str,
    pub url: &'static str,
}

const LICENSES: &[License] = &[
    License {
        name: "Helios FHIR Server",
        license: "MIT",
        url: "https://github.com/HeliosSoftware/hfs",
    },
    License {
        name: "htmx 2.0.4",
        license: "0BSD",
        url: "https://htmx.org",
    },
    License {
        name: "CodeMirror 6",
        license: "MIT",
        url: "https://codemirror.net",
    },
    License {
        name: "Figtree",
        license: "SIL OFL 1.1",
        url: "https://github.com/erikdkennedy/figtree",
    },
    License {
        name: "axum",
        license: "MIT",
        url: "https://github.com/tokio-rs/axum",
    },
    License {
        name: "Askama",
        license: "MIT / Apache-2.0",
        url: "https://github.com/askama-rs/askama",
    },
    License {
        name: "Project Fluent",
        license: "MIT / Apache-2.0",
        url: "https://projectfluent.org",
    },
];

#[derive(Template)]
#[template(path = "pages/manager/about.html")]
struct AboutPage {
    status: Status,
    i18n: I18n,
    active_page: &'static str,
    licenses: &'static [License],
    endpoints: usize,
    default_version: &'static str,
}

pub(crate) async fn about(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
) -> Response {
    let endpoints = match &state.manager {
        Some(m) => m.registry.snapshot().endpoints().len(),
        None => 1,
    };
    render(AboutPage {
        status: current_status(&state, rv.0, &rt),
        i18n: I18n::new(locale),
        active_page: "about",
        licenses: LICENSES,
        endpoints,
        default_version: release_label(match state.fhir_version.as_str() {
            "R4B" => FhirRelease::R4B,
            "R5" => FhirRelease::R5,
            "R6" => FhirRelease::R6,
            _ => FhirRelease::R4,
        }),
    })
}

/// Sync jobs for the manager pages: the backend's own, else those of the
/// installed sync service.
pub(crate) async fn sync_jobs_of(
    backend: &Arc<dyn ManagerBackend>,
) -> Result<Vec<SyncJobView>, BackendError> {
    if !backend.sync_available()
        && let Some(views) = crate::sync_pages::service_job_views().await
    {
        return Ok(views);
    }
    backend.sync_jobs().await
}

/// Whether sync is available to the manager pages (backend or service).
fn sync_on(backend: &dyn ManagerBackend) -> bool {
    backend.sync_available() || crate::sync_pages::service_available()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_rows_read_the_useful_fields() {
        let bundle = serde_json::json!({"entry": [
            {"resource": {"resourceType": "AuditEvent", "recorded": "2026-11-20T01:02:03Z",
              "type": {"code": "rest", "display": "RESTful Operation"},
              "subtype": [{"code": "create"}], "action": "C", "outcome": "0",
              "agent": [{"who": {"display": "admin"}}],
              "entity": [{"what": {"reference": "Patient/1"}}]}},
            {"resource": {"resourceType": "AuditEvent", "recorded": "2026-11-20T01:02:04Z",
              "type": {"code": "rest"}, "outcome": "8",
              "agent": [{"name": "svc"}], "entity": [{"description": "Endpoint/lab"}]}}
        ]});
        let rows = audit_rows(&bundle);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, "create");
        assert_eq!(rows[0].at, "2026-11-20 01:02:03Z");
        assert!(rows[0].outcome_ok);
        assert_eq!(rows[0].what, "Patient/1");
        assert_eq!(rows[1].kind, "rest");
        assert!(!rows[1].outcome_ok);
        assert_eq!(rows[1].agent, "svc");
        assert_eq!(rows[1].what, "Endpoint/lab");
        assert!(audit_rows(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn form_values_round_trip_through_forms() {
        let mut spec =
            EndpointSpec::named(helios_endpoints::EndpointName::parse("lab").unwrap(), "Lab");
        spec.auth = Some(helios_endpoints::EndpointAuth {
            mode: AuthMode::Full,
            issuer: Some("https://i.example".into()),
            jwks_url: Some("https://i.example/certs".into()),
            audience: Some("aud".into()),
            ..Default::default()
        });
        let v = FormValues::from_spec(&spec);
        assert!(!v.auth_inherit && v.cors_inherit && v.val_inherit);
        // Inherited sections show the server's values, explicit ones keep theirs.
        let env = view::EnvDefaults::from_lookup(|k| match k {
            "HFS_VALIDATION_MODE" => Some("log".into()),
            "HFS_VALIDATION_TERMINOLOGY" => Some("remote".into()),
            "HFS_AUTH_MODE" => Some("write-protect".into()),
            "HFS_TERMINOLOGY_SERVER" => Some("http://hts:8090".into()),
            _ => None,
        });
        let w = FormValues::from_spec(&spec).with_inherited(&env);
        assert_eq!((w.val_mode, w.val_terminology), ("log", "remote"));
        assert_eq!(w.term_server_url, "http://hts:8090");
        assert_eq!(
            w.auth_mode, "full",
            "an explicit auth section is not overridden"
        );
        assert_eq!(v.auth_mode, "full");
        assert_eq!(v.path, "lab");
        let health = health_tiles(&ServerHealth::default());
        assert_eq!(health.len(), 5);
        assert!(health.iter().all(|t| !t.tone.is_empty()));
    }
}
