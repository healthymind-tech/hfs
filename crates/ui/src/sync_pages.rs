//! Sync console pages (FHIR server manager P6, design §10.1/§10.3):
//! `/ui/sync` (job list), `/ui/sync/new` (create form), `/ui/sync/{job}`
//! (definition, run history, Run / Cancel / Retry / Delete) and
//! `/ui/sync/runs/{run}` (live counters polled by htmx every 2 s while the
//! run is active, log tail, conflicts with overwrite / keep actions).
//!
//! Server-rendered over [`helios_sync::SyncService`] (no self-calls). Reads
//! are viewer pages; every POST here is a write and therefore behind the
//! console's administrator guard (`admin_login::protects` refuses
//! non-read-only `/ui` requests without a session). Credentials typed into
//! the form go straight into the service's sealing path and are never
//! rendered back.

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::{
    Extension, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use helios_endpoints::sync::{
    LogLevel, SyncConflict, SyncJob, SyncLogLine, SyncPeer, SyncRun, SyncRunState, SyncTrigger,
};
use helios_sync::{SyncError, SyncService};
use serde_json::{Value, json};

use crate::i18n::{I18n, RequestLocale};
use crate::{RequestTenant, RequestVersion, WebState, current_status, render};

/// The sync pages (merged into the console router).
pub(crate) fn routes() -> Router<WebState> {
    crate::manager::nav::register(crate::manager::nav::NavItem {
        section: crate::manager::nav::NavSection::Server,
        id: "sync",
        label_key: "sync-title",
        href: "/ui/sync",
        icon: "sync",
        min_role: helios_endpoints::ConsoleRole::Viewer,
        order: 10,
    });
    Router::new()
        .route("/ui/sync", get(list_page))
        .route("/ui/sync/new", get(new_page).post(create))
        .route("/ui/sync/runs/{run}", get(run_page))
        .route("/ui/sync/runs/{run}/status", get(run_status))
        .route(
            "/ui/sync/runs/{run}/conflicts/{rtype}/{rid}/{action}",
            post(resolve),
        )
        .route("/ui/sync/{job}", get(job_page))
        .route("/ui/sync/{job}/{action}", post(job_action))
}

fn service() -> Option<Arc<SyncService>> {
    helios_sync::global()
}

/// One job row of the list.
pub(crate) struct JobRow {
    pub id: String,
    pub name: String,
    pub source: String,
    pub target: String,
    pub trigger: String,
    pub enabled: bool,
    pub state: String,
    pub progress: String,
    pub percent: Option<u8>,
    pub run_id: String,
    pub next_run: String,
}

/// Pill tone and icon of a run state (P5 `pill_raw`).
fn state_look(state: &str) -> (&'static str, &'static str) {
    match state {
        "succeeded" => ("ok", "check-circle"),
        "failed" => ("danger", "alert"),
        "running" => ("info", "activity"),
        "queued" => ("accent", "history"),
        "cancelled" => ("muted", "stop-circle"),
        _ => ("muted", "info"),
    }
}

impl JobRow {
    pub fn tone(&self) -> &'static str {
        state_look(&self.state).0
    }
    pub fn icon(&self) -> &'static str {
        state_look(&self.state).1
    }
}

/// Progress percent: estimated when known, full when succeeded,
/// indeterminate (`None`) while active without an estimate.
fn run_percent(r: &SyncRun) -> Option<u8> {
    let p = &r.progress;
    match r.state {
        SyncRunState::Succeeded => Some(100),
        SyncRunState::Failed | SyncRunState::Cancelled => Some(0),
        _ => p
            .estimated_total
            .filter(|t| *t > 0)
            .map(|t| ((p.read * 100 / t).min(100)) as u8),
    }
}

fn peer_label(p: &SyncPeer) -> String {
    match p {
        SyncPeer::Endpoint { name } => format!("endpoint {name}"),
        SyncPeer::External(x) => x.base_url.clone(),
    }
}

fn trigger_label(t: &SyncTrigger) -> String {
    match t {
        SyncTrigger::Manual => "manual".into(),
        SyncTrigger::Interval { every_secs } => format!("every {every_secs} s"),
        SyncTrigger::Continuous { poll_secs } => format!("continuous ({poll_secs} s)"),
        SyncTrigger::Subscription {
            fallback_poll_secs, ..
        } => {
            format!("subscription (poll {fallback_poll_secs} s)")
        }
    }
}

fn state_label(s: SyncRunState) -> &'static str {
    match s {
        SyncRunState::Queued => "queued",
        SyncRunState::Running => "running",
        SyncRunState::Succeeded => "succeeded",
        SyncRunState::Failed => "failed",
        SyncRunState::Cancelled => "cancelled",
    }
}

fn progress_label(r: &SyncRun) -> String {
    let p = &r.progress;
    format!(
        "{} read · {} created · {} updated · {} deleted · {} skipped · {} conflicts · {} failed",
        p.read, p.created, p.updated, p.deleted, p.skipped, p.conflicts, p.failed
    )
}

fn job_row(job: &SyncJob, last: Option<&SyncRun>) -> JobRow {
    JobRow {
        id: job.id.clone(),
        name: job.spec.name.clone(),
        source: peer_label(&job.spec.source),
        target: peer_label(&job.spec.target),
        trigger: trigger_label(&job.spec.trigger),
        enabled: job.spec.enabled,
        state: last
            .map(|r| state_label(r.state))
            .unwrap_or("never run")
            .to_string(),
        progress: last.map(progress_label).unwrap_or_default(),
        percent: last.and_then(run_percent),
        run_id: last.map(|r| r.id.clone()).unwrap_or_default(),
        next_run: job
            .next_run_at
            .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_default(),
    }
}

#[derive(Template)]
#[template(path = "pages/sync-jobs.html")]
pub(crate) struct SyncJobsPage {
    pub status: crate::Status,
    pub i18n: I18n,
    pub active_page: &'static str,
    pub available: bool,
    pub rows: Vec<JobRow>,
    pub error: Option<String>,
}

async fn list_page(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
) -> Response {
    let i18n = I18n::new(locale);
    let mut page = SyncJobsPage {
        status: current_status(&state, rv.0, &rt),
        i18n,
        active_page: "sync",
        available: false,
        rows: vec![],
        error: None,
    };
    if let Some(s) = service() {
        page.available = true;
        match s.list().await {
            Ok(list) => {
                for item in list {
                    if let Ok(job) = serde_json::from_value::<SyncJob>(item.job.clone())
                        .or_else(|_| redacted_to_job(&item.job))
                    {
                        page.rows.push(job_row(&job, item.last_run.as_ref()));
                    }
                }
            }
            Err(e) => page.error = Some(e.to_string()),
        }
    }
    render(page)
}

/// A redacted job (secrets as `{"set": true}`) reshaped so it parses back
/// into a [`SyncJob`] for display (secret fields replaced by a dummy
/// envelope that is never shown).
fn redacted_to_job(v: &Value) -> Result<SyncJob, serde_json::Error> {
    fn fill(v: &mut Value) {
        match v {
            Value::Object(m) if m.len() == 1 && m.get("set") == Some(&Value::Bool(true)) => {
                *v = Value::String("hmsec:v1:x:AAAAAAAAAAAAAAAA:AA".into());
            }
            Value::Object(m) => m.values_mut().for_each(fill),
            Value::Array(a) => a.iter_mut().for_each(fill),
            _ => {}
        }
    }
    let mut c = v.clone();
    fill(&mut c);
    serde_json::from_value(c)
}

/// An endpoint option of the form.
pub(crate) struct EndpointOption {
    pub name: String,
    pub label: String,
}

#[derive(Template)]
#[template(path = "pages/sync-new.html")]
pub(crate) struct SyncNewPage {
    pub status: crate::Status,
    pub i18n: I18n,
    pub active_page: &'static str,
    pub available: bool,
    pub endpoints: Vec<EndpointOption>,
    pub errors: Vec<String>,
    pub form: HashMap<String, String>,
}

impl SyncNewPage {
    /// A form value (empty when absent).
    pub fn v(&self, key: &str) -> &str {
        self.form.get(key).map(String::as_str).unwrap_or("")
    }

    /// Whether a select / radio value is chosen (`default` when absent).
    pub fn is(&self, key: &str, value: &str, default: &str) -> bool {
        self.form.get(key).map(String::as_str).unwrap_or(default) == value
    }

    /// `{side}_{field}` value.
    pub fn fv(&self, side: &str, field: &str) -> &str {
        self.v(&format!("{side}_{field}"))
    }

    /// `{side}_{field}` choice.
    pub fn fis(&self, side: &str, field: &str, value: &str, default: &str) -> bool {
        self.is(&format!("{side}_{field}"), value, default)
    }

    /// Localized fieldset title of a side.
    pub fn side_title(&self, side: &str) -> String {
        if side == "source" {
            self.i18n.t("sync-source")
        } else {
            self.i18n.t("sync-target")
        }
    }

    /// `{side}_{field}` as a form field name.
    pub fn side_field(&self, side: &str, field: &str) -> String {
        format!("{side}_{field}")
    }

    /// A value with a default when absent / empty.
    pub fn v_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        match self.v(key) {
            "" => default,
            v => v,
        }
    }

    /// A checkbox: `default` on a fresh form, else whether it was sent.
    pub fn checked_or(&self, key: &str, default: bool) -> bool {
        if self.form.is_empty() {
            default
        } else {
            self.v(key) == "on"
        }
    }
}

fn endpoint_options(s: &SyncService) -> Vec<EndpointOption> {
    s.runtime()
        .env
        .registry
        .snapshot()
        .endpoints()
        .iter()
        .map(|e| EndpointOption {
            name: e.name().to_string(),
            label: format!(
                "{} — {} ({:?})",
                e.name(),
                e.spec.display_name,
                e.spec.fhir_version
            ),
        })
        .collect()
}

async fn new_page(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let s = service();
    // `/ui/sync/new?source=<endpoint>` (the endpoint's sync page) preselects
    // the source.
    let mut form = HashMap::new();
    if let Some(src) = q.get("source").filter(|v| !v.is_empty()) {
        form.insert("source_kind".to_string(), "endpoint".to_string());
        form.insert("source_endpoint".to_string(), src.clone());
    }
    render(SyncNewPage {
        status: current_status(&state, rv.0, &rt),
        i18n: I18n::new(locale),
        active_page: "sync",
        available: s.is_some(),
        endpoints: s.as_deref().map(endpoint_options).unwrap_or_default(),
        errors: vec![],
        form,
    })
}

/// Builds the API body (with `secrets`) from the form fields.
pub(crate) fn body_from_form(f: &HashMap<String, String>) -> Value {
    let get = |k: &str| f.get(k).map(|s| s.trim()).filter(|s| !s.is_empty());
    let mut secrets = serde_json::Map::new();
    let mut peer = |side: &str| -> Value {
        if get(&format!("{side}_kind")) == Some("external") {
            let auth_type = get(&format!("{side}_auth")).unwrap_or("none");
            let mut auth = json!({"type": auth_type});
            let secret = get(&format!("{side}_secret"));
            match auth_type {
                "bearer" => {
                    if let Some(s) = secret {
                        secrets.insert(format!("{side}.auth.token"), json!({"set": s}));
                    }
                }
                "basic" => {
                    auth["username"] = json!(get(&format!("{side}_username")).unwrap_or(""));
                    if let Some(s) = secret {
                        secrets.insert(format!("{side}.auth.password"), json!({"set": s}));
                    }
                }
                "client-credentials" => {
                    auth["tokenUrl"] = json!(get(&format!("{side}_token_url")).unwrap_or(""));
                    auth["clientId"] = json!(get(&format!("{side}_client_id")).unwrap_or(""));
                    if let Some(sc) = get(&format!("{side}_scope")) {
                        auth["scope"] = json!(sc);
                    }
                    if let Some(s) = secret {
                        secrets.insert(format!("{side}.auth.clientSecret"), json!({"set": s}));
                    }
                }
                _ => {}
            }
            json!({"kind": "external", "baseUrl": get(&format!("{side}_url")).unwrap_or(""), "auth": auth})
        } else {
            json!({"kind": "endpoint", "name": get(&format!("{side}_endpoint")).unwrap_or("")})
        }
    };
    let source = peer("source");
    let target = peer("target");
    let num = |k: &str, d: u32| get(k).and_then(|v| v.parse::<u32>().ok()).unwrap_or(d);
    let trigger = match get("trigger").unwrap_or("manual") {
        "interval" => json!({"type": "interval", "everySecs": num("every_secs", 3600)}),
        "continuous" => json!({"type": "continuous", "pollSecs": num("poll_secs", 30)}),
        "subscription" => {
            json!({"type": "subscription", "fallbackPollSecs": num("poll_secs", 300)})
        }
        _ => json!({"type": "manual"}),
    };
    let types: Vec<String> = get("resource_types")
        .map(|s| {
            s.split([',', ' '])
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    json!({
        "name": get("name").unwrap_or(""),
        "description": get("description"),
        "source": source,
        "target": target,
        "resourceTypes": types,
        "initialLoad": get("initial_load").unwrap_or("paged-read"),
        "includeHistory": get("include_history") == Some("on"),
        "trigger": trigger,
        "conflict": get("conflict").unwrap_or("source-wins"),
        "propagateDeletes": get("propagate_deletes") == Some("on"),
        "batchSize": num("batch_size", 200),
        "enabled": get("enabled") == Some("on"),
        "secrets": Value::Object(secrets),
    })
}

/// Parses an `application/x-www-form-urlencoded` body.
pub(crate) fn parse_form(body: &[u8]) -> HashMap<String, String> {
    form_urlencoded::parse(body).into_owned().collect()
}

fn error_lines(e: &SyncError) -> Vec<String> {
    match e {
        SyncError::Invalid(v) => {
            v.0.iter()
                .map(|f| {
                    if f.field.is_empty() {
                        f.message.clone()
                    } else {
                        format!("{}: {}", f.field, f.message)
                    }
                })
                .collect()
        }
        other => vec![other.to_string()],
    }
}

fn ui_actor(admin: Option<&helios_admin_auth::AdminUser>) -> String {
    admin
        .map(|a| format!("admin:{}", a.0))
        .unwrap_or_else(|| "console".into())
}

async fn create(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    admin: Option<Extension<helios_admin_auth::AdminUser>>,
    body: Bytes,
) -> Response {
    let Some(s) = service() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "sync is not enabled").into_response();
    };
    let mut form = parse_form(&body);
    match s
        .create(
            body_from_form(&form),
            &ui_actor(admin.as_ref().map(|e| &e.0)),
        )
        .await
    {
        Ok(created) => {
            let id = created
                .job
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Redirect::to(&format!("/ui/sync/{id}")).into_response()
        }
        Err(e) => {
            // Never echo credentials back into the form.
            form.retain(|k, _| !k.ends_with("_secret"));
            let page = SyncNewPage {
                status: current_status(&state, rv.0, &rt),
                i18n: I18n::new(locale),
                active_page: "sync",
                available: true,
                endpoints: endpoint_options(&s),
                errors: error_lines(&e),
                form,
            };
            (StatusCode::UNPROCESSABLE_ENTITY, render(page)).into_response()
        }
    }
}

/// A run row.
pub(crate) struct RunRow {
    pub id: String,
    pub state: String,
    pub reason: String,
    pub created: String,
    pub progress: String,
    pub error: String,
}

impl RunRow {
    pub fn tone(&self) -> &'static str {
        state_look(&self.state).0
    }
    pub fn icon(&self) -> &'static str {
        state_look(&self.state).1
    }
}

fn run_row(r: &SyncRun) -> RunRow {
    RunRow {
        id: r.id.clone(),
        state: state_label(r.state).into(),
        reason: format!("{:?}", r.reason).to_lowercase(),
        created: r.created_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        progress: progress_label(r),
        error: r.error.clone().unwrap_or_default(),
    }
}

#[derive(Template)]
#[template(path = "pages/sync-job.html")]
pub(crate) struct SyncJobPage {
    pub status: crate::Status,
    pub i18n: I18n,
    pub active_page: &'static str,
    pub lede: String,
    pub row: JobRow,
    pub definition: String,
    pub runs: Vec<RunRow>,
    pub active: bool,
    pub retryable: bool,
}

async fn job_page(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path(id): Path<String>,
) -> Response {
    let i18n = I18n::new(locale);
    let status = current_status(&state, rv.0, &rt);
    let Some(s) = service() else {
        return crate::render_not_found(status, i18n, "sync", "/ui/sync", "Sync");
    };
    let Ok(job) = s.job(&id).await else {
        return crate::render_not_found(status, i18n, "sync", "/ui/sync", "Sync");
    };
    let runs = s.runs(&id, 50).await.unwrap_or_default();
    let definition =
        serde_json::to_string_pretty(&helios_sync::secrets::redacted_job(&job)["spec"])
            .unwrap_or_default();
    let last = runs.first();
    let row = job_row(&job, last);
    let mut lede = format!("{} → {} · {}", row.source, row.target, row.trigger);
    if !row.enabled {
        lede.push_str(&format!(" · {}", i18n.t("sync-disabled")));
    }
    render(SyncJobPage {
        status,
        i18n,
        active_page: "sync",
        lede,
        row,
        definition,
        active: last.is_some_and(|r| !r.state.is_terminal()),
        retryable: last
            .is_some_and(|r| matches!(r.state, SyncRunState::Failed | SyncRunState::Cancelled)),
        runs: runs.iter().map(run_row).collect(),
    })
}

async fn job_action(
    Path((id, action)): Path<(String, String)>,
    admin: Option<Extension<helios_admin_auth::AdminUser>>,
) -> Response {
    let Some(s) = service() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "sync is not enabled").into_response();
    };
    let actor = ui_actor(admin.as_ref().map(|e| &e.0));
    let r: Result<Option<String>, SyncError> = match action.as_str() {
        "run" => s.run_now(&id, &actor).await.map(|r| Some(r.id)),
        "retry" => s.retry(&id, &actor).await.map(|r| Some(r.id)),
        "cancel" => s.cancel(&id, &actor).await.map(|_| None),
        "delete" => {
            return match s.delete(&id, &actor).await {
                Ok(()) => Redirect::to("/ui/sync").into_response(),
                Err(e) => (StatusCode::CONFLICT, e.to_string()).into_response(),
            };
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    match r {
        Ok(Some(run)) => Redirect::to(&format!("/ui/sync/runs/{run}")).into_response(),
        Ok(None) => Redirect::to(&format!("/ui/sync/{id}")).into_response(),
        Err(e) => (
            StatusCode::CONFLICT,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            e.to_string(),
        )
            .into_response(),
    }
}

/// A log line view.
pub(crate) struct LogView {
    pub at: String,
    pub level: &'static str,
    pub message: String,
}

fn log_view(l: &SyncLogLine) -> LogView {
    LogView {
        at: l.at.format("%H:%M:%S").to_string(),
        level: match l.level {
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        },
        message: l.message.clone(),
    }
}

/// The live part of the run page.
#[derive(Template)]
#[template(path = "partials/sync_run_status.html")]
pub(crate) struct RunStatusFragment {
    pub i18n: I18n,
    pub run: RunRow,
    pub counters: Vec<(&'static str, u64)>,
    pub phase: String,
    pub current_type: String,
    pub live: bool,
    pub percent: Option<u8>,
    pub progress_label: String,
    pub csrf: Option<String>,
    pub log: Vec<LogView>,
    pub conflicts: Vec<SyncConflict>,
}

#[derive(Template)]
#[template(path = "pages/sync-run.html")]
pub(crate) struct SyncRunPage {
    pub status: crate::Status,
    pub i18n: I18n,
    pub active_page: &'static str,
    pub job_id: String,
    pub job_name: String,
    pub fragment_html: String,
}

async fn fragment(
    s: &SyncService,
    run: &SyncRun,
    i18n: I18n,
    csrf: Option<String>,
) -> RunStatusFragment {
    let mut log = s.log(&run.id, 0, 1000).await.unwrap_or_default();
    let tail = log.len().saturating_sub(100);
    let log: Vec<LogView> = log.drain(tail..).rev().map(|l| log_view(&l)).collect();
    let p = &run.progress;
    RunStatusFragment {
        i18n,
        run: run_row(run),
        counters: vec![
            ("read", p.read),
            ("created", p.created),
            ("updated", p.updated),
            ("deleted", p.deleted),
            ("skipped", p.skipped),
            ("conflicts", p.conflicts),
            ("failed", p.failed),
        ],
        phase: run.cursor.phase.clone(),
        current_type: run.cursor.current_type.clone().unwrap_or_default(),
        live: !run.state.is_terminal(),
        percent: run_percent(run),
        progress_label: progress_label(run),
        csrf,
        log,
        conflicts: s.conflicts(&run.id, 200).await.unwrap_or_default(),
    }
}

async fn run_page(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path(run_id): Path<String>,
) -> Response {
    let status = current_status(&state, rv.0, &rt);
    let Some(s) = service() else {
        return crate::render_not_found(status, I18n::new(locale), "sync", "/ui/sync", "Sync");
    };
    let Ok(run) = s.run(&run_id).await else {
        return crate::render_not_found(status, I18n::new(locale), "sync", "/ui/sync", "Sync");
    };
    let job_name = s
        .job(&run.job_id)
        .await
        .map(|j| j.spec.name)
        .unwrap_or_default();
    let csrf = status.csrf_token().map(str::to_string);
    render(SyncRunPage {
        status,
        i18n: I18n::new(locale.clone()),
        active_page: "sync",
        job_id: run.job_id.clone(),
        job_name,
        fragment_html: fragment(&s, &run, I18n::new(locale), csrf)
            .await
            .render()
            .unwrap_or_default(),
    })
}

async fn run_status(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Path(run_id): Path<String>,
) -> Response {
    let Some(s) = service() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let csrf = current_status(&state, rv.0, &rt)
        .csrf_token()
        .map(str::to_string);
    match s.run(&run_id).await {
        Ok(run) => render(fragment(&s, &run, I18n::new(locale), csrf).await),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn resolve(
    Path((run, rtype, rid, action)): Path<(String, String, String, String)>,
    admin: Option<Extension<helios_admin_auth::AdminUser>>,
) -> Response {
    let Some(s) = service() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match s
        .resolve_conflict(
            &run,
            &rtype,
            &rid,
            &action,
            &ui_actor(admin.as_ref().map(|e| &e.0)),
        )
        .await
    {
        Ok(_) => Redirect::to(&format!("/ui/sync/runs/{run}")).into_response(),
        Err(e) => (StatusCode::CONFLICT, e.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_to_body() {
        let f = parse_form(
            b"name=push&source_kind=endpoint&source_endpoint=default&target_kind=external\
              &target_url=https%3A%2F%2Fhapi.example.org%2Ffhir&target_auth=bearer&target_secret=t0k\
              &trigger=interval&every_secs=600&resource_types=Patient%2C+Observation\
              &initial_load=paged-read&conflict=fail&propagate_deletes=on&enabled=on",
        );
        let b = body_from_form(&f);
        assert_eq!(b["source"], json!({"kind": "endpoint", "name": "default"}));
        assert_eq!(b["target"]["baseUrl"], "https://hapi.example.org/fhir");
        assert_eq!(b["target"]["auth"]["type"], "bearer");
        assert!(
            b["target"]["auth"].get("token").is_none(),
            "secrets travel in `secrets` only"
        );
        assert_eq!(b["secrets"]["target.auth.token"], json!({"set": "t0k"}));
        assert_eq!(b["trigger"], json!({"type": "interval", "everySecs": 600}));
        assert_eq!(b["resourceTypes"], json!(["Patient", "Observation"]));
        assert_eq!(b["conflict"], "fail");
        assert_eq!(b["propagateDeletes"], true);
        assert_eq!(b["enabled"], true);
        let spec: helios_endpoints::sync::SyncJobSpec = {
            let mut v = b.clone();
            v.as_object_mut().unwrap().remove("secrets");
            v["target"]["auth"]["token"] = json!("hmsec:v1:k:AAAAAAAAAAAAAAAA:AA");
            serde_json::from_value(v).unwrap()
        };
        assert_eq!(spec.batch_size, 200);

        let f = parse_form(
            b"name=pull&source_kind=external&source_url=https%3A%2F%2Fs.example.org&source_auth=client-credentials\
              &source_token_url=https%3A%2F%2Fs.example.org%2Ftoken&source_client_id=c&source_scope=system%2F*.rs\
              &source_secret=cs&target_kind=endpoint&target_endpoint=lab&trigger=continuous&poll_secs=15",
        );
        let b = body_from_form(&f);
        assert_eq!(b["source"]["auth"]["clientId"], "c");
        assert_eq!(b["source"]["auth"]["scope"], "system/*.rs");
        assert_eq!(
            b["secrets"]["source.auth.clientSecret"],
            json!({"set": "cs"})
        );
        assert_eq!(b["trigger"], json!({"type": "continuous", "pollSecs": 15}));
        assert_eq!(b["enabled"], false);
        let f = parse_form(b"source_kind=external&source_auth=basic&source_username=u&source_secret=p&trigger=subscription");
        let b = body_from_form(&f);
        assert_eq!(b["source"]["auth"]["username"], "u");
        assert_eq!(b["secrets"]["source.auth.password"], json!({"set": "p"}));
        assert_eq!(b["trigger"]["type"], "subscription");
    }

    #[test]
    fn labels() {
        assert_eq!(
            trigger_label(&SyncTrigger::Interval { every_secs: 60 }),
            "every 60 s"
        );
        assert_eq!(trigger_label(&SyncTrigger::Manual), "manual");
        assert!(trigger_label(&SyncTrigger::Continuous { poll_secs: 5 }).contains('5'));
        assert!(
            trigger_label(&SyncTrigger::Subscription {
                fallback_poll_secs: 9,
                notify_secret: None
            })
            .contains('9')
        );
        assert_eq!(state_label(SyncRunState::Cancelled), "cancelled");
        assert_eq!(
            peer_label(&SyncPeer::Endpoint { name: "a".into() }),
            "endpoint a"
        );
        let job = redacted_to_job(&json!({
            "id": "j", "revision": 1, "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
            "spec": {"name": "n", "source": {"kind": "endpoint", "name": "a"},
                     "target": {"kind": "external", "baseUrl": "https://x.example.org",
                                "auth": {"type": "bearer", "token": {"set": true}}}}
        }))
        .unwrap();
        let row = job_row(&job, None);
        assert_eq!(row.state, "never run");
        assert_eq!(row.target, "https://x.example.org");
    }

    #[test]
    fn errors_are_listed() {
        let mut v = helios_endpoints::model::ValidationErrors::default();
        v.push("name", "bad");
        v.push("", "whole body");
        assert_eq!(
            error_lines(&SyncError::Invalid(v)),
            vec!["name: bad", "whole body"]
        );
        assert_eq!(error_lines(&SyncError::NoKeyRing).len(), 1);
    }

    #[test]
    fn catalogs_define_the_same_sync_keys() {
        use std::collections::BTreeSet;
        let keys = |locale: &str| -> BTreeSet<String> {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../locales")
                .join(locale)
                .join("sync.ftl");
            std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
                .lines()
                .filter_map(|l| l.split_once(" = ").map(|(k, _)| k.trim().to_string()))
                .filter(|k| !k.starts_with('#') && !k.is_empty())
                .collect()
        };
        let en = keys("en");
        assert!(en.len() > 10);
        assert_eq!(en, keys("zh-Hant"));
    }
}
