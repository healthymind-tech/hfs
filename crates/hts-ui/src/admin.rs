//! Terminology management pages: inventory (`/hts/terminology`), the delete
//! confirmation, the jobs list and job detail (`/hts/jobs`), and the form
//! endpoints the extended Import page posts to.
//!
//! Like every other page in this crate, these render from the HTS HTTP
//! surface — here the admin API (`/admin/...`, `docs/hts-terminology-admin.md`)
//! — through [`crate::upstream::UpstreamClient::admin_call`]. A `404` from
//! `GET /admin` means the server runs with `HTS_ADMIN_UI` off; the pages then
//! show how to turn it on instead of failing.
//!
//! Every action is a plain HTML form with a `303 See Other` back to a page,
//! so the pages work without JavaScript. htmx only adds polling: the jobs
//! list and job detail re-fetch their fragment every two seconds while a job
//! is active (the fragment drops its `hx-trigger` once everything is
//! terminal, which stops the polling). The chunked upload is the one piece
//! that needs script (`assets/hts-admin.js`); it talks to the same admin API
//! mounted under `/ui/hts/api/admin`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::{
    Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use axum_htmx::HxRequest;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::i18n::{I18n, RequestLocale};
use crate::upstream::UpstreamError;
use crate::{Chrome, HtsUiState, render_page};

/// Browser-side base of the admin API (same origin as the console).
pub(crate) const BROWSER_API: &str = "/ui/hts/api/admin";

const SLOW: Duration = Duration::from_secs(25);
const FAST: Duration = Duration::from_secs(8);

pub(crate) fn routes() -> Router<Arc<HtsUiState>> {
    Router::new()
        .route("/hts/terminology", get(inventory_page))
        .route("/hts/terminology/current", post(set_current))
        .route(
            "/hts/terminology/delete",
            get(delete_confirm_page).post(delete_submit),
        )
        .route("/hts/terminology/delete-resource", post(delete_resource))
        .route("/hts/terminology/compact", post(compact))
        .route("/hts/import/job", post(start_import))
        .route("/hts/import/registry", get(registry_search))
        .route("/hts/jobs", get(jobs_page))
        .route("/hts/jobs/{id}", get(job_page))
        .route("/hts/jobs/{id}/cancel", post(cancel_job))
}

// ── Shared helpers ────────────────────────────────────────────────────────────

/// Who is behind this console request, forwarded to the admin API so its
/// audit trail names the operator rather than the console's loopback client.
pub(crate) struct Fwd(Vec<(&'static str, String)>);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for Fwd {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let mut out = Vec::new();
        for name in [
            "x-forwarded-for",
            "x-forwarded-user",
            "x-remote-user",
            "user-agent",
        ] {
            if let Some(v) = parts.headers.get(name).and_then(|v| v.to_str().ok()) {
                out.push((name, v.to_string()));
            }
        }
        if !out.iter().any(|(n, _)| *n == "x-forwarded-for")
            && let Some(axum::extract::ConnectInfo(peer)) =
                parts
                    .extensions
                    .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        {
            out.push(("x-forwarded-for", peer.ip().to_string()));
        }
        Ok(Fwd(out))
    }
}

/// Why an admin page cannot show data.
#[derive(Clone, Debug)]
pub(crate) enum Unavailable {
    /// `HTS_ADMIN_UI` is off upstream.
    Disabled,
    /// Transport failure — reuse the shared degraded banner.
    Degraded(&'static str),
    /// The admin API answered with an error.
    Error(String),
}

impl Unavailable {
    fn from_upstream(e: &UpstreamError) -> Self {
        Unavailable::Degraded(e.degraded_reason())
    }
    pub(crate) fn is_disabled(&self) -> bool {
        matches!(self, Unavailable::Disabled)
    }
    pub(crate) fn degraded_reason(&self) -> Option<&'static str> {
        match self {
            Unavailable::Degraded(r) => Some(r),
            _ => None,
        }
    }
    pub(crate) fn error_message(&self) -> Option<&str> {
        match self {
            Unavailable::Error(m) => Some(m),
            _ => None,
        }
    }
}

fn message_of(v: &Value) -> String {
    v.get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("request failed")
        .to_string()
}

/// `GET /admin{path}` → JSON, mapping 404-on-root to "disabled".
pub(crate) async fn admin_get(
    state: &HtsUiState,
    path: &str,
    timeout: Duration,
) -> Result<Value, Unavailable> {
    match state
        .upstream
        .admin_call(reqwest::Method::GET, path, None, timeout, &[])
        .await
    {
        Ok((200, v)) => Ok(v),
        Ok((404, v)) if v.get("error").is_none() => Err(Unavailable::Disabled),
        Ok((_, v)) => Err(Unavailable::Error(message_of(&v))),
        Err(e) => Err(Unavailable::from_upstream(&e)),
    }
}

fn chrome<'a>(state: &'a HtsUiState, locale: RequestLocale, active: &'static str) -> Chrome<'a> {
    Chrome {
        i18n: I18n::new(locale),
        active_page: active,
        fhir_version: state.fhir_version,
        version: state.version,
    }
}

fn parse_form(body: &[u8]) -> HashMap<String, String> {
    form_urlencoded::parse(body)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn field<'a>(f: &'a HashMap<String, String>, k: &str) -> Option<&'a str> {
    f.get(k).map(|s| s.trim()).filter(|s| !s.is_empty())
}

fn enc(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// `303` to `to`, with an optional `notice=`/`error=` flag the page renders.
fn see_other(to: &str) -> Response {
    Redirect::to(to).into_response()
}

fn with_param(base: &str, key: &str, value: &str) -> String {
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}{key}={}", enc(value))
}

/// Submit a write and redirect to the job it created, or back to `back`
/// with the error.
async fn submit_job(
    state: &HtsUiState,
    fwd: &Fwd,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
    back: &str,
) -> Response {
    match state
        .upstream
        .admin_call(method, path, body.as_ref(), FAST, &fwd.0)
        .await
    {
        Ok((202, job)) => match job.get("id").and_then(|i| i.as_str()) {
            Some(id) => see_other(&format!("/ui/hts/jobs/{}", enc(id))),
            None => see_other(&with_param(back, "error", "no job id")),
        },
        Ok((_, v)) => see_other(&with_param(back, "error", &message_of(&v))),
        Err(e) => see_other(&with_param(back, "error", &e.to_string())),
    }
}

/// Human byte size (`1.2 GB`), locale-aware decimal separator.
pub(crate) fn bytes_label(i18n: &I18n, n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{} B", i18n.num(n))
    } else {
        format!("{} {}", i18n.dec(v, 1), UNITS[u])
    }
}

/// `2026-10-14T10:15:00.123+00:00` → `2026-10-14 10:15 UTC`.
pub(crate) fn when(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|t| {
            t.with_timezone(&chrono::Utc)
                .format("%Y-%m-%d %H:%M UTC")
                .to_string()
        })
        .unwrap_or_else(|_| ts.to_string())
}

#[derive(Debug, Deserialize, Default)]
struct Flash {
    notice: Option<String>,
    error: Option<String>,
}

// ── Inventory ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default, Clone)]
pub(crate) struct InvStats {
    #[serde(default)]
    pub concepts: u64,
    #[serde(default)]
    pub est_bytes: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct InvVersion {
    pub version: Option<String>,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub current: bool,
    pub stats: Option<InvStats>,
    pub imported_at: Option<String>,
    pub source: Option<String>,
    pub sha256: Option<String>,
    pub job_id: Option<String>,
    pub db_growth_bytes: Option<i64>,
}

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct InvSystem {
    pub url: String,
    pub title: Option<String>,
    pub versions: Vec<InvVersion>,
}

#[derive(Debug, Deserialize, Default)]
struct InvDb {
    #[serde(default)]
    bytes: u64,
    #[serde(default)]
    reclaimable_bytes: u64,
}

#[derive(Debug, Deserialize)]
struct Inventory {
    #[serde(default)]
    stats_pending: bool,
    stats_computed_at: Option<String>,
    database: Option<InvDb>,
    #[serde(default)]
    value_sets: u64,
    #[serde(default)]
    concept_maps: u64,
    #[serde(default)]
    code_system_count: u64,
    total_concepts: Option<u64>,
    #[serde(default)]
    systems: Vec<InvSystem>,
}

/// One table row: a system's version, flattened for the template.
pub(crate) struct InvRow {
    pub url: String,
    pub title: String,
    pub first_of_system: bool,
    pub version_count: usize,
    pub version: String,
    pub version_param: String,
    pub content: String,
    pub is_supplement: bool,
    pub concepts: Option<String>,
    pub size: Option<String>,
    pub size_measured: bool,
    pub imported_at: Option<String>,
    pub source: Option<String>,
    pub sha_short: Option<String>,
    pub job_id: Option<String>,
    pub pinned: bool,
    pub current: bool,
    pub status: String,
}

#[derive(Debug, Deserialize, Default)]
struct InvQuery {
    q: Option<String>,
    /// Number of code systems to show (default 50).
    limit: Option<usize>,
    sort: Option<String>,
    all: Option<String>,
    notice: Option<String>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "pages/terminology.html")]
struct InventoryPage<'a> {
    chrome: Chrome<'a>,
    unavailable: Option<Unavailable>,
    q: String,
    sort_size: bool,
    show_all: bool,
    rows: Vec<InvRow>,
    stats_pending: bool,
    stats_at: Option<String>,
    db_size: Option<String>,
    db_reclaimable: Option<String>,
    systems: String,
    concepts: Option<String>,
    value_sets: String,
    concept_maps: String,
    notice: Option<String>,
    error: Option<String>,
    refresh_url: String,
    back: String,
    /// "Showing x of y" when the list was truncated, with the link to all.
    more: Option<(String, String)>,
}

async fn inventory_page(
    State(state): State<Arc<HtsUiState>>,
    // Taken for its side effect: `AutoVaryLayer` adds `Vary: HX-Request`
    // only for handlers that read the header. The stats-pending refresh
    // swaps a selection of the full page, so both requests get the page.
    HxRequest(_is_htmx): HxRequest,
    locale: RequestLocale,
    Query(q): Query<InvQuery>,
) -> Response {
    let chrome = chrome(&state, locale, "terminology");
    let i18n = chrome.i18n;
    let query = q.q.clone().unwrap_or_default();
    let sort_size = q.sort.as_deref() == Some("size");
    let show_all = q.all.is_some();
    let limit = q.limit.unwrap_or(50).clamp(1, 5000);
    let mut path = format!("/terminology?limit={limit}&q={}", enc(&query));
    if sort_size {
        path.push_str("&sort=size");
    }
    if !show_all {
        path.push_str("&populated=true");
    }
    let mut back = format!("/ui/hts/terminology?q={}", enc(&query));
    if sort_size {
        back.push_str("&sort=size");
    }
    if show_all {
        back.push_str("&all=1");
    }
    let result = admin_get(&state, &path, SLOW).await.and_then(|v| {
        serde_json::from_value::<Inventory>(v).map_err(|e| Unavailable::Error(e.to_string()))
    });
    let mut page = InventoryPage {
        chrome,
        unavailable: None,
        q: query,
        sort_size,
        show_all,
        rows: Vec::new(),
        stats_pending: false,
        stats_at: None,
        db_size: None,
        db_reclaimable: None,
        systems: String::new(),
        concepts: None,
        value_sets: String::new(),
        concept_maps: String::new(),
        notice: q.notice,
        error: q.error,
        refresh_url: back.clone(),
        more: None,
        back,
    };
    match result {
        Err(u) => page.unavailable = Some(u),
        Ok(inv) => {
            page.stats_pending = inv.stats_pending;
            page.stats_at = inv.stats_computed_at.as_deref().map(when);
            if let Some(db) = &inv.database {
                page.db_size = Some(bytes_label(&i18n, db.bytes));
                page.db_reclaimable = Some(bytes_label(&i18n, db.reclaimable_bytes));
            }
            page.systems = i18n.num(inv.code_system_count);
            page.concepts = inv.total_concepts.map(|n| i18n.num(n));
            page.value_sets = i18n.num(inv.value_sets);
            page.concept_maps = i18n.num(inv.concept_maps);
            let shown = inv.systems.len() as u64;
            if shown < inv.code_system_count {
                page.more = Some((
                    i18n.t_arg2(
                        "hts-term-showing",
                        "shown",
                        i18n.num(shown),
                        "total",
                        i18n.num(inv.code_system_count),
                    ),
                    format!("{}&limit=5000", page.back),
                ));
            }
            page.rows = flatten(&i18n, inv.systems);
        }
    }
    render_page(page.render())
}

pub(crate) fn flatten(i18n: &I18n, systems: Vec<InvSystem>) -> Vec<InvRow> {
    let mut rows = Vec::new();
    for s in systems {
        let n = s.versions.len();
        let title = s.title.clone().unwrap_or_default();
        for (i, v) in s.versions.into_iter().enumerate() {
            let measured = v.db_growth_bytes.filter(|b| *b > 0);
            rows.push(InvRow {
                url: s.url.clone(),
                title: title.clone(),
                first_of_system: i == 0,
                version_count: n,
                version: v.version.clone().unwrap_or_else(|| "—".into()),
                version_param: v.version.clone().unwrap_or_default(),
                is_supplement: v.content == "supplement",
                content: v.content,
                concepts: v.stats.as_ref().map(|st| i18n.num(st.concepts)),
                size: match measured {
                    Some(b) => Some(bytes_label(i18n, b as u64)),
                    None => v.stats.as_ref().map(|st| bytes_label(i18n, st.est_bytes)),
                },
                size_measured: measured.is_some(),
                imported_at: v.imported_at.as_deref().map(when),
                source: v.source,
                sha_short: v.sha256.as_deref().map(|s| s.chars().take(12).collect()),
                job_id: v.job_id,
                pinned: v.pinned,
                current: v.current,
                status: v.status,
            });
        }
    }
    rows
}

async fn set_current(State(state): State<Arc<HtsUiState>>, fwd: Fwd, body: Bytes) -> Response {
    let f = parse_form(&body);
    let back = field(&f, "back")
        .filter(|b| b.starts_with("/ui/hts/"))
        .unwrap_or("/ui/hts/terminology")
        .to_string();
    let Some(url) = field(&f, "url") else {
        return see_other(&with_param(&back, "error", "url is required"));
    };
    let clear = field(&f, "action") == Some("clear");
    let version = field(&f, "version");
    let body = if clear {
        json!({ "url": url, "version": null })
    } else {
        match version {
            Some(v) => json!({ "url": url, "version": v }),
            None => json!({ "url": url, "unversioned": true }),
        }
    };
    match state
        .upstream
        .admin_call(
            reqwest::Method::PUT,
            "/code-systems/current",
            Some(&body),
            FAST,
            &fwd.0,
        )
        .await
    {
        Ok((200, _)) => see_other(&with_param(
            &back,
            "notice",
            if clear { "unpinned" } else { "pinned" },
        )),
        Ok((_, v)) => see_other(&with_param(&back, "error", &message_of(&v))),
        Err(e) => see_other(&with_param(&back, "error", &e.to_string())),
    }
}

// ── Delete confirmation ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
struct DeleteQuery {
    url: Option<String>,
    version: Option<String>,
    error: Option<String>,
}

pub(crate) struct DepRow {
    pub kind: String,
    pub url: String,
    pub version: String,
    pub pins: bool,
}

#[derive(Template)]
#[template(path = "pages/terminology-delete.html")]
struct DeletePage<'a> {
    chrome: Chrome<'a>,
    unavailable: Option<Unavailable>,
    url: String,
    version: String,
    version_label: String,
    concepts: Option<String>,
    size: Option<String>,
    is_pinned: bool,
    deps: Vec<DepRow>,
    current_after: Option<String>,
    nothing_left: bool,
    error: Option<String>,
}

async fn delete_confirm_page(
    State(state): State<Arc<HtsUiState>>,
    HxRequest(_is_htmx): HxRequest,
    locale: RequestLocale,
    Query(q): Query<DeleteQuery>,
) -> Response {
    let chrome = chrome(&state, locale, "terminology");
    let i18n = chrome.i18n;
    let url = q.url.clone().unwrap_or_default();
    let version = q.version.clone().unwrap_or_default();
    let mut page = DeletePage {
        chrome,
        unavailable: None,
        version_label: if version.is_empty() {
            "—".into()
        } else {
            version.clone()
        },
        url: url.clone(),
        version: version.clone(),
        concepts: None,
        size: None,
        is_pinned: false,
        deps: Vec::new(),
        current_after: None,
        nothing_left: false,
        error: q.error,
    };
    let path = format!(
        "/code-systems/dependents?url={}&version={}",
        enc(&url),
        enc(&version)
    );
    match admin_get(&state, &path, SLOW).await {
        Err(u) => page.unavailable = Some(u),
        Ok(v) => {
            page.is_pinned =
                v.pointer("/code_system/pinned").and_then(|p| p.as_bool()) == Some(true);
            if let Some(st) = v.get("stats").filter(|s| !s.is_null()) {
                page.concepts = st
                    .get("concepts")
                    .and_then(|c| c.as_u64())
                    .map(|n| i18n.num(n));
                page.size = st
                    .get("est_bytes")
                    .and_then(|c| c.as_u64())
                    .map(|n| bytes_label(&i18n, n));
            }
            let d = &v["dependents"];
            for (key, kind) in [
                ("value_sets", "ValueSet"),
                ("supplements", "CodeSystem (supplement)"),
                ("concept_maps", "ConceptMap"),
            ] {
                for r in d[key].as_array().into_iter().flatten() {
                    page.deps.push(DepRow {
                        kind: kind.into(),
                        url: r["url"].as_str().unwrap_or("").into(),
                        version: r["version"].as_str().unwrap_or("").into(),
                        pins: r["pins_version"].as_bool().unwrap_or(false),
                    });
                }
            }
            match d.get("current_after_delete") {
                None | Some(Value::Null) => page.nothing_left = true,
                Some(v) => {
                    page.current_after = Some(v.as_str().unwrap_or("—").to_string());
                }
            }
        }
    }
    render_page(page.render())
}

async fn delete_submit(State(state): State<Arc<HtsUiState>>, fwd: Fwd, body: Bytes) -> Response {
    let f = parse_form(&body);
    let url = field(&f, "url").unwrap_or("").to_string();
    let version = field(&f, "version").unwrap_or("").to_string();
    let back = format!(
        "/ui/hts/terminology/delete?url={}&version={}",
        enc(&url),
        enc(&version)
    );
    if field(&f, "confirm") != Some(url.as_str()) {
        return see_other(&with_param(&back, "error", "confirm"));
    }
    submit_job(
        &state,
        &fwd,
        reqwest::Method::POST,
        "/code-systems/delete",
        Some(json!({ "url": url, "version": version })),
        &back,
    )
    .await
}

async fn delete_resource(State(state): State<Arc<HtsUiState>>, fwd: Fwd, body: Bytes) -> Response {
    let f = parse_form(&body);
    let back = "/ui/hts/terminology";
    let Some(url) = field(&f, "url") else {
        return see_other(&with_param(back, "error", "url is required"));
    };
    let path = match field(&f, "type") {
        Some("ConceptMap") => format!("/concept-maps?url={}", enc(url)),
        _ => format!(
            "/value-sets?url={}&version={}",
            enc(url),
            enc(field(&f, "version").unwrap_or(""))
        ),
    };
    submit_job(&state, &fwd, reqwest::Method::DELETE, &path, None, back).await
}

async fn compact(State(state): State<Arc<HtsUiState>>, fwd: Fwd) -> Response {
    submit_job(
        &state,
        &fwd,
        reqwest::Method::POST,
        "/maintenance/compact",
        None,
        "/ui/hts/terminology",
    )
    .await
}

// ── Import page support ──────────────────────────────────────────────────────

/// Admin-side data the Import page needs (sources other than Bundle paste).
pub(crate) struct ImportAdmin {
    pub chunk_size: u64,
    pub max_upload: String,
    pub formats: Vec<FormatOpt>,
    pub import_dir: Option<String>,
    pub dir_entries: Vec<DirEntry>,
    pub dir_error: Option<String>,
    pub registry: Option<String>,
    pub incomplete: Vec<IncompleteUpload>,
    pub completed: Vec<CompletedUpload>,
    pub api: &'static str,
}

pub(crate) struct FormatOpt {
    pub id: String,
    pub user_supplied: bool,
}

pub(crate) struct DirEntry {
    pub path: String,
    pub size: String,
    pub modified: String,
    pub hint: String,
}

pub(crate) struct IncompleteUpload {
    pub filename: String,
    pub percent: u64,
    pub size: String,
    pub updated: String,
}

pub(crate) struct CompletedUpload {
    pub id: String,
    pub filename: String,
    pub size: String,
    pub format: String,
    pub version: String,
}

/// Load what the Import page shows for admin sources; `Err` when admin is
/// off or unreachable (the page then offers only the Bundle source).
pub(crate) async fn import_admin(
    state: &HtsUiState,
    i18n: &I18n,
) -> Result<ImportAdmin, Unavailable> {
    let status = admin_get(state, "", FAST).await?;
    let formats = admin_get(state, "/formats", FAST).await?;
    let uploads = admin_get(state, "/uploads", FAST)
        .await
        .unwrap_or(Value::Null);
    let import_dir = status
        .get("import_dir")
        .and_then(|d| d.as_str())
        .map(String::from);
    let (dir_entries, dir_error) = if import_dir.is_some() {
        match admin_get(state, "/import-dir", FAST).await {
            Ok(v) => (
                v["entries"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|e| DirEntry {
                        path: e["path"].as_str().unwrap_or("").into(),
                        size: if e["is_dir"].as_bool() == Some(true) {
                            "—".into()
                        } else {
                            bytes_label(i18n, e["size"].as_u64().unwrap_or(0))
                        },
                        modified: e["modified"].as_str().map(when).unwrap_or_default(),
                        hint: e["format_hint"].as_str().unwrap_or("").into(),
                    })
                    .collect(),
                None,
            ),
            Err(u) => (
                Vec::new(),
                Some(u.error_message().unwrap_or("unavailable").to_string()),
            ),
        }
    } else {
        (Vec::new(), None)
    };
    let mut incomplete = Vec::new();
    let mut completed = Vec::new();
    for u in uploads["uploads"].as_array().into_iter().flatten() {
        let size = u["size"].as_u64().unwrap_or(0);
        if u["completed"].as_bool() == Some(true) {
            completed.push(CompletedUpload {
                id: u["id"].as_str().unwrap_or("").into(),
                filename: u["filename"].as_str().unwrap_or("").into(),
                size: bytes_label(i18n, size),
                format: u
                    .pointer("/detection/format")
                    .and_then(|f| f.as_str())
                    .unwrap_or("")
                    .into(),
                version: u
                    .pointer("/detection/version")
                    .and_then(|f| f.as_str())
                    .unwrap_or("")
                    .into(),
            });
        } else {
            let got = u["received_bytes"].as_u64().unwrap_or(0);
            incomplete.push(IncompleteUpload {
                filename: u["filename"].as_str().unwrap_or("").into(),
                percent: (got * 100).checked_div(size).unwrap_or(0),
                size: bytes_label(i18n, size),
                updated: u["updated_at"].as_str().map(when).unwrap_or_default(),
            });
        }
    }
    Ok(ImportAdmin {
        chunk_size: status["chunk_size"].as_u64().unwrap_or(8 << 20),
        max_upload: bytes_label(i18n, status["max_upload_size"].as_u64().unwrap_or(0)),
        formats: formats["formats"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|f| FormatOpt {
                id: f["id"].as_str().unwrap_or("").into(),
                user_supplied: f["licence"].as_str() == Some("user-supplied"),
            })
            .collect(),
        import_dir,
        dir_entries,
        dir_error,
        registry: status
            .get("registry")
            .and_then(|r| r.as_str())
            .map(String::from),
        incomplete,
        completed,
        api: BROWSER_API,
    })
}

async fn start_import(State(state): State<Arc<HtsUiState>>, fwd: Fwd, body: Bytes) -> Response {
    let f = parse_form(&body);
    let back = "/ui/hts/import";
    let source = match field(&f, "admin_source") {
        Some("upload") => match field(&f, "upload_id") {
            Some(id) => json!({ "type": "upload", "upload_id": id }),
            None => return see_other(&with_param(back, "error", "choose an uploaded file")),
        },
        Some("import-dir") => match field(&f, "path") {
            Some(p) => json!({ "type": "import-dir", "path": p }),
            None => return see_other(&with_param(back, "error", "choose a file")),
        },
        Some("registry") => match field(&f, "package") {
            Some(p) => json!({
                "type": "registry",
                "package": p,
                "version": field(&f, "package_version"),
            }),
            None => return see_other(&with_param(back, "error", "enter a package name")),
        },
        _ => return see_other(&with_param(back, "error", "unknown source")),
    };
    let format = field(&f, "format").filter(|v| *v != "auto");
    let req = json!({
        "source": source,
        "format": format,
        "version": field(&f, "version"),
        "languages": field(&f, "languages"),
        "properties": field(&f, "properties"),
    });
    submit_job(
        &state,
        &fwd,
        reqwest::Method::POST,
        "/imports",
        Some(req),
        back,
    )
    .await
}

#[derive(Debug, Deserialize, Default)]
struct RegistryQuery {
    #[serde(alias = "registry_q")]
    q: Option<String>,
}

#[derive(Template)]
#[template(path = "partials/hts-registry-results.html")]
struct RegistryResults<'a> {
    chrome: Chrome<'a>,
    packages: Vec<(String, String, String)>,
    error: Option<String>,
    searched: bool,
}

async fn registry_search(
    State(state): State<Arc<HtsUiState>>,
    HxRequest(_is_htmx): HxRequest,
    locale: RequestLocale,
    Query(q): Query<RegistryQuery>,
) -> Response {
    let chrome = chrome(&state, locale, "import");
    let query = q.q.unwrap_or_default();
    let mut view = RegistryResults {
        chrome,
        packages: Vec::new(),
        error: None,
        searched: !query.trim().is_empty(),
    };
    if view.searched {
        match admin_get(
            &state,
            &format!("/registry/search?q={}", enc(query.trim())),
            SLOW,
        )
        .await
        {
            Ok(v) => {
                view.packages = v["packages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(30)
                    .map(|p| {
                        (
                            p["name"].as_str().unwrap_or("").to_string(),
                            p["fhir_version"].as_str().unwrap_or("").to_string(),
                            p["description"].as_str().unwrap_or("").to_string(),
                        )
                    })
                    .collect();
            }
            Err(u) => {
                view.error = Some(
                    u.error_message()
                        .map(String::from)
                        .unwrap_or_else(|| "registry unavailable".into()),
                )
            }
        }
    }
    render_page(view.render())
}

// ── Jobs ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone, Default)]
pub(crate) struct JobProgress {
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub done: u64,
    pub total: Option<u64>,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub concepts: u64,
    pub percent: Option<f64>,
}

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct JobJson {
    pub id: String,
    pub label: String,
    pub status: String,
    #[serde(default)]
    pub requested_by: Value,
    #[serde(default)]
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    #[serde(default)]
    pub phases: Vec<String>,
    #[serde(default)]
    pub progress: JobProgress,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub queue_position: Option<u64>,
    #[serde(default)]
    pub spec: Value,
}

/// A job, pre-formatted for the templates.
pub(crate) struct JobView {
    pub id: String,
    pub label: String,
    pub status: String,
    /// `hts-job-status-<status>` (askama does not rewrite field names
    /// inside `format!`, so keys are composed here).
    pub status_key: String,
    pub phase_key: String,
    pub active: bool,
    pub cancellable: bool,
    pub tag_class: &'static str,
    pub progress_class: &'static str,
    pub percent: u64,
    pub phase: String,
    pub counter: Option<String>,
    pub concepts: Option<String>,
    pub queue_position: Option<u64>,
    pub who: String,
    pub created: String,
    pub started: Option<String>,
    pub finished: Option<String>,
    pub duration: Option<String>,
    pub error: Option<String>,
    pub phases: Vec<PhaseView>,
    pub result_json: Option<String>,
    pub result_summary: Vec<(String, String)>,
}

pub(crate) struct PhaseView {
    pub key: String,
    /// `done`, `current`, `pending`.
    pub state: &'static str,
}

fn job_view(i18n: &I18n, j: JobJson) -> JobView {
    let active = matches!(j.status.as_str(), "queued" | "running");
    let (tag_class, progress_class) = match j.status.as_str() {
        "succeeded" => ("tag--complete", "progress--complete"),
        "failed" | "interrupted" => ("tag--failed", "progress--failed"),
        "cancelled" => ("tag--cancelled", "progress--cancelled"),
        _ => ("tag--in-progress", "progress--in-progress"),
    };
    let is_import = j.spec.get("kind").and_then(|k| k.as_str()) == Some("import");
    let cancellable = j.status == "queued" || (j.status == "running" && is_import);
    let p = &j.progress;
    let counter = match (p.unit.as_str(), p.total) {
        ("batches", Some(t)) if j.status == "running" => Some(i18n.t_arg2(
            "hts-job-batches",
            "done",
            i18n.num(p.done),
            "total",
            i18n.num(t),
        )),
        ("batches", None) if j.status == "running" && p.done > 0 => {
            Some(i18n.t_arg("hts-job-batches-open", "done", i18n.num(p.done)))
        }
        ("bytes", Some(t)) if j.status == "running" => Some(format!(
            "{} / {}",
            bytes_label(i18n, p.done),
            bytes_label(i18n, t)
        )),
        ("rows", Some(t)) if j.status == "running" && t > 0 => Some(i18n.t_arg2(
            "hts-job-rows",
            "done",
            i18n.num(p.done),
            "total",
            i18n.num(t),
        )),
        _ => None,
    };
    let phase_idx = j.phases.iter().position(|ph| *ph == p.phase);
    let phases = j
        .phases
        .iter()
        .enumerate()
        .map(|(i, name)| PhaseView {
            key: format!("hts-job-phase-{name}"),
            state: if j.status == "succeeded" {
                "done"
            } else {
                match phase_idx {
                    Some(c) if i < c => "done",
                    Some(c) if i == c && active => "current",
                    _ => "pending",
                }
            },
        })
        .collect();
    let duration = match (&j.started_at, &j.finished_at) {
        (Some(s), Some(f)) => {
            let s = chrono::DateTime::parse_from_rfc3339(s).ok();
            let f = chrono::DateTime::parse_from_rfc3339(f).ok();
            s.zip(f).map(|(s, f)| {
                let secs = (f - s).num_seconds().max(0);
                format!("{}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
            })
        }
        _ => None,
    };
    let mut result_summary = Vec::new();
    if let Some(r) = &j.result {
        if let Some(st) = r.get("stats") {
            for k in ["code_systems", "value_sets", "concept_maps", "concepts"] {
                if let Some(n) = st.get(k).and_then(|n| n.as_u64()) {
                    result_summary.push((
                        format!("hts-job-result-{}", k.replace('_', "-")),
                        i18n.num(n),
                    ));
                }
            }
        }
        if let Some(rows) = r.get("rows")
            && let Some(n) = rows.get("concepts").and_then(|n| n.as_u64())
        {
            result_summary.push(("hts-job-result-deleted-concepts".into(), i18n.num(n)));
        }
        if let Some(g) = r.get("db_growth_bytes").and_then(|g| g.as_i64()) {
            result_summary.push((
                "hts-job-result-growth".into(),
                if g >= 0 {
                    bytes_label(i18n, g as u64)
                } else {
                    format!("−{}", bytes_label(i18n, g.unsigned_abs()))
                },
            ));
        }
        if let Some(g) = r.get("reclaimed_bytes").and_then(|g| g.as_i64()) {
            result_summary.push((
                "hts-job-result-reclaimed".into(),
                bytes_label(i18n, g.max(0) as u64),
            ));
        }
        if let Some(s) = r.get("elapsed_seconds").and_then(|s| s.as_f64()) {
            result_summary.push(("hts-job-result-elapsed".into(), format!("{s:.1} s")));
        }
    }
    let phase = if p.phase.is_empty() {
        "prepare".to_string()
    } else {
        p.phase.clone()
    };
    JobView {
        status_key: format!("hts-job-status-{}", j.status),
        phase_key: format!("hts-job-phase-{phase}"),
        percent: p.percent.unwrap_or(0.0).round().clamp(0.0, 100.0) as u64,
        phase,
        counter,
        concepts: (p.concepts > 0).then(|| i18n.num(p.concepts)),
        queue_position: j.queue_position,
        who: j
            .requested_by
            .get("user")
            .and_then(|u| u.as_str())
            .or_else(|| j.requested_by.get("address").and_then(|u| u.as_str()))
            .unwrap_or("—")
            .to_string(),
        created: when(&j.created_at),
        started: j.started_at.as_deref().map(when),
        finished: j.finished_at.as_deref().map(when),
        duration,
        error: j.error.clone(),
        phases,
        result_json: j
            .result
            .as_ref()
            .and_then(|r| serde_json::to_string_pretty(r).ok()),
        result_summary,
        id: j.id,
        label: j.label,
        status: j.status,
        active,
        cancellable,
        tag_class,
        progress_class,
    }
}

#[derive(Debug, Deserialize, Default)]
struct FragmentQuery {
    fragment: Option<String>,
    notice: Option<String>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "pages/jobs.html")]
struct JobsPage<'a> {
    chrome: Chrome<'a>,
    unavailable: Option<Unavailable>,
    jobs: Vec<JobView>,
    any_active: bool,
    flash: Flash,
}

#[derive(Template)]
#[template(path = "partials/hts-jobs-list.html")]
struct JobsList<'a> {
    chrome: Chrome<'a>,
    unavailable: Option<Unavailable>,
    jobs: Vec<JobView>,
    any_active: bool,
}

async fn jobs_page(
    State(state): State<Arc<HtsUiState>>,
    locale: RequestLocale,
    HxRequest(is_htmx): HxRequest,
    Query(q): Query<FragmentQuery>,
) -> Response {
    let chrome = chrome(&state, locale, "jobs");
    let i18n = chrome.i18n;
    let (unavailable, jobs) = match admin_get(&state, "/jobs?limit=100", FAST).await {
        Err(u) => (Some(u), Vec::new()),
        Ok(v) => {
            let list: Vec<JobJson> = serde_json::from_value(v["jobs"].clone()).unwrap_or_default();
            (None, list.into_iter().map(|j| job_view(&i18n, j)).collect())
        }
    };
    let any_active = jobs.iter().any(|j: &JobView| j.active);
    if is_htmx || q.fragment.is_some() {
        return render_page(
            JobsList {
                chrome,
                unavailable,
                jobs,
                any_active,
            }
            .render(),
        );
    }
    render_page(
        JobsPage {
            chrome,
            unavailable,
            jobs,
            any_active,
            flash: Flash {
                notice: q.notice,
                error: q.error,
            },
        }
        .render(),
    )
}

#[derive(Template)]
#[template(path = "pages/job.html")]
struct JobPage<'a> {
    chrome: Chrome<'a>,
    unavailable: Option<Unavailable>,
    job_opt: Option<JobView>,
    log: Vec<String>,
    flash: Flash,
}

#[derive(Template)]
#[template(path = "partials/hts-job-detail.html")]
struct JobDetail<'a> {
    chrome: Chrome<'a>,
    job: JobView,
    log: Vec<String>,
}

async fn job_page(
    State(state): State<Arc<HtsUiState>>,
    locale: RequestLocale,
    HxRequest(is_htmx): HxRequest,
    Path(id): Path<String>,
    Query(q): Query<FragmentQuery>,
) -> Response {
    let chrome = chrome(&state, locale, "jobs");
    let i18n = chrome.i18n;
    let id_enc = enc(&id);
    let (unavailable, job) = match admin_get(&state, &format!("/jobs/{id_enc}"), FAST).await {
        Err(u) => (Some(u), None),
        Ok(v) => match serde_json::from_value::<JobJson>(v) {
            Ok(j) => (None, Some(job_view(&i18n, j))),
            Err(e) => (Some(Unavailable::Error(e.to_string())), None),
        },
    };
    let log = admin_get(&state, &format!("/jobs/{id_enc}/log?tail=200"), FAST)
        .await
        .ok()
        .and_then(|v| serde_json::from_value::<Vec<String>>(v["lines"].clone()).ok())
        .unwrap_or_default();
    if (is_htmx || q.fragment.is_some())
        && let Some(job) = job
    {
        return render_page(JobDetail { chrome, job, log }.render());
    }
    let status = if job.is_none() && unavailable.is_none() {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::OK
    };
    let mut resp = render_page(
        JobPage {
            chrome,
            unavailable,
            job_opt: job,
            log,
            flash: Flash {
                notice: q.notice,
                error: q.error,
            },
        }
        .render(),
    );
    if status != StatusCode::OK {
        *resp.status_mut() = status;
    }
    resp
}

async fn cancel_job(
    State(state): State<Arc<HtsUiState>>,
    fwd: Fwd,
    Path(id): Path<String>,
) -> Response {
    let back = format!("/ui/hts/jobs/{}", enc(&id));
    match state
        .upstream
        .admin_call(
            reqwest::Method::POST,
            &format!("/jobs/{}/cancel", enc(&id)),
            None,
            FAST,
            &fwd.0,
        )
        .await
    {
        Ok((200, _)) => see_other(&with_param(&back, "notice", "cancel-requested")),
        Ok((_, v)) => see_other(&with_param(&back, "error", &message_of(&v))),
        Err(e) => see_other(&with_param(&back, "error", &e.to_string())),
    }
}

/// Plain-text response helper kept for symmetry with other modules.
#[allow(dead_code)]
fn text(status: StatusCode, body: &'static str) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain")], body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_view_maps_running_import_progress() {
        let i18n = I18n::new(RequestLocale::default());
        let j: JobJson = serde_json::from_value(json!({
            "id": "j1", "label": "import snomed-rf2", "status": "running",
            "spec": {"kind": "import"},
            "created_at": "2026-10-14T10:00:00+00:00",
            "started_at": "2026-10-14T10:00:01+00:00",
            "phases": ["prepare", "parse", "write", "closure"],
            "progress": {"phase": "write", "done": 3, "total": 74, "unit": "batches", "concepts": 15000, "percent": 41.2}
        }))
        .unwrap();
        let v = job_view(&i18n, j);
        assert!(v.active && v.cancellable);
        assert_eq!(v.percent, 41);
        assert_eq!(
            v.phases.iter().map(|p| p.state).collect::<Vec<_>>(),
            vec!["done", "done", "current", "pending"]
        );
        assert!(v.counter.unwrap().contains("74"));
        assert_eq!(v.concepts.as_deref(), Some("15,000"));
    }

    #[test]
    fn running_delete_is_not_cancellable_and_failed_maps_classes() {
        let i18n = I18n::new(RequestLocale::default());
        let j: JobJson = serde_json::from_value(json!({
            "id": "j2", "label": "delete", "status": "running",
            "spec": {"kind": "delete-code-system"}, "progress": {"phase": "concepts"}
        }))
        .unwrap();
        assert!(!job_view(&i18n, j).cancellable);
        let j: JobJson = serde_json::from_value(json!({
            "id": "j3", "label": "x", "status": "failed", "error": "boom"
        }))
        .unwrap();
        let v = job_view(&i18n, j);
        assert_eq!(
            (v.tag_class, v.progress_class),
            ("tag--failed", "progress--failed")
        );
    }

    #[test]
    fn flatten_prefers_measured_growth_and_marks_first_row() {
        let i18n = I18n::new(RequestLocale::default());
        let systems: Vec<InvSystem> = serde_json::from_value(json!([{
            "url": "http://snomed.info/sct", "title": "SNOMED CT",
            "versions": [
                {"version": "20250601", "content": "complete", "status": "active", "current": true,
                 "stats": {"concepts": 369000, "est_bytes": 2500000000u64}, "db_growth_bytes": 3600000000i64,
                 "sha256": "0123456789abcdef0123", "source": "upload:snomed.zip"},
                {"version": "20240101", "content": "complete", "status": "active",
                 "stats": {"concepts": 360000, "est_bytes": 2400000000u64}}
            ]
        }]))
        .unwrap();
        let rows = flatten(&i18n, systems);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].first_of_system && !rows[1].first_of_system);
        assert!(rows[0].size_measured);
        assert_eq!(rows[0].size.as_deref(), Some("3.6 GB"));
        assert_eq!(rows[0].sha_short.as_deref(), Some("0123456789ab"));
        assert_eq!(rows[1].size.as_deref(), Some("2.4 GB"));
    }

    #[test]
    fn byte_labels_and_timestamps() {
        let i18n = I18n::new(RequestLocale::default());
        assert_eq!(bytes_label(&i18n, 999), "999 B");
        assert_eq!(bytes_label(&i18n, 1_500_000), "1.5 MB");
        assert_eq!(when("2026-10-14T10:15:00.5+00:00"), "2026-10-14 10:15 UTC");
        assert_eq!(with_param("/a?x=1", "error", "a b"), "/a?x=1&error=a+b");
    }
}
