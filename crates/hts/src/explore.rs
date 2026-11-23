//! `GET /CodeSystem/$hm-explore` — the paged, capped reads behind the
//! console's terminology explorers (HealthyMind extension).
//!
//! `$lookup` returns every child of a concept (201 048 under the NDC root)
//! and `$expand` cannot group, so the explorers ask this operation instead.
//! Every mode is bounded: pages are capped at [`MAX_COUNT`], counts beyond
//! [`COUNT_CAP`] are reported as the cap, and nothing reads a whole system.
//!
//! | `mode` | Answer |
//! |---|---|
//! | `stats` | the stored version: concept count, hierarchy, metadata (`props=1`: property usage, small systems only) |
//! | `children` | one page of the children of `code` (no `code`: the hierarchy roots), each with its child count |
//! | `buckets` | the children of `code` grouped by the code segment before `sep` (NDC labelers) |
//! | `facets` | the values of `property` with concept counts, within `filter=PROP=value` |
//! | `members` | one page of the concepts within the filters, with the `show` properties |
//! | `search` | code (exact, prefix) and display (all words) matches |
//! | `node` | a concept with its parents, path to a root and child count (SNOMED CT: attributes too) |
//! | `mappings` | the stored ConceptMap elements that map `code` (as source) or map to it (as target), straight from the element index |
//!
//! SNOMED CT `children`, `node` and `search` are answered from the in-memory
//! concept model (active concepts only, semantic tags from the FSN). The JSON
//! is plain (not FHIR): this is an operation for the console, not a FHIR
//! interaction.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::{
    extract::{RawQuery, State},
    http::header,
    response::{IntoResponse, Response},
};
use helios_persistence::tenant::TenantContext;
use serde_json::{Value, json};

use crate::error::HtsError;
use crate::state::AppState;
use crate::traits::TerminologyBackend;

/// Largest page any mode returns.
pub const MAX_COUNT: u32 = 500;
/// Counts (children, matches) stop at this value and are reported as `capped`.
pub const COUNT_CAP: i64 = 100_000;
/// Systems with more concepts than this skip the property census of `stats`.
pub const PROPS_CENSUS_MAX: i64 = 60_000;
/// Longest path walked to a root.
pub const MAX_DEPTH: usize = 64;

/// What the request asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExploreMode {
    Stats,
    Children,
    Buckets,
    Facets,
    Members,
    Search,
    Node,
    Mappings,
}

impl ExploreMode {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "stats" => Self::Stats,
            "children" => Self::Children,
            "buckets" => Self::Buckets,
            "facets" => Self::Facets,
            "members" => Self::Members,
            "search" => Self::Search,
            "node" => Self::Node,
            "mappings" => Self::Mappings,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stats => "stats",
            Self::Children => "children",
            Self::Buckets => "buckets",
            Self::Facets => "facets",
            Self::Members => "members",
            Self::Search => "search",
            Self::Node => "node",
            Self::Mappings => "mappings",
        }
    }

    fn default_count(self) -> u32 {
        match self {
            Self::Children => 100,
            Self::Buckets => 60,
            Self::Facets => 24,
            Self::Members => 25,
            Self::Search => 20,
            Self::Stats | Self::Node | Self::Mappings => 50,
        }
    }

    fn max_count(self) -> u32 {
        match self {
            Self::Children => MAX_COUNT,
            Self::Buckets => 200,
            Self::Facets | Self::Members | Self::Node | Self::Mappings => 100,
            Self::Search => 50,
            Self::Stats => 1,
        }
    }
}

/// A parsed `$hm-explore` request.
#[derive(Clone, Debug)]
pub struct ExploreRequest {
    pub mode: ExploreMode,
    pub system: String,
    pub version: Option<String>,
    /// `children` / `buckets`: the parent (none: the roots); `node`: the focus.
    pub code: Option<String>,
    /// `children`: only codes starting with this.
    pub prefix: Option<String>,
    /// `buckets`: the separator (default `-`).
    pub sep: String,
    /// `facets`: the property counted.
    pub property: Option<String>,
    /// `facets` / `members`: `(property, value)` pairs every concept must have.
    pub filters: Vec<(String, String)>,
    /// `members`: properties returned per concept.
    pub show: Vec<String>,
    /// `search` / `facets` / `members`: free text.
    pub q: Option<String>,
    pub count: u32,
    pub offset: u32,
    /// `children`: order by display rather than code.
    pub order_display: bool,
    /// `children`: codes in descending order (`order=code-desc`).
    pub order_desc: bool,
    /// `children`: also count descendants (closure), for pages of up to 60.
    pub descendants: bool,
    /// `stats`: include the property census.
    pub props: bool,
    /// Labels from another stored system with the same codes (`url[|version]`):
    /// its display, or its first designation when the display is empty.
    pub alt: Option<(String, Option<String>)>,
    /// A designation language (prefix, e.g. `zh`) returned as `designation`.
    pub lang: Option<String>,
}

impl ExploreRequest {
    /// Parse the query pairs; unknown keys are ignored.
    pub fn from_pairs(pairs: &[(String, String)]) -> Result<Self, HtsError> {
        let get = |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let mode_raw = get("mode").unwrap_or_else(|| "stats".into());
        let mode = ExploreMode::parse(&mode_raw).ok_or_else(|| {
            HtsError::InvalidRequest(format!(
                "$hm-explore: unknown mode '{mode_raw}' (stats, children, buckets, facets, members, search, node, mappings)"
            ))
        })?;
        let system = get("system").ok_or_else(|| {
            HtsError::InvalidRequest("$hm-explore: the system parameter is required".into())
        })?;
        let count = get("count")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or_else(|| mode.default_count())
            .clamp(1, mode.max_count());
        let offset = get("offset")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0)
            .min(10_000_000);
        let mut filters = Vec::new();
        for (k, v) in pairs {
            if k != "filter" {
                continue;
            }
            let Some((p, val)) = v.split_once('=') else {
                return Err(HtsError::InvalidRequest(format!(
                    "$hm-explore: filter '{v}' is not PROPERTY=value"
                )));
            };
            if p.trim().is_empty() {
                return Err(HtsError::InvalidRequest(format!(
                    "$hm-explore: filter '{v}' names no property"
                )));
            }
            filters.push((p.trim().to_string(), val.trim().to_string()));
        }
        if filters.len() > 8 {
            return Err(HtsError::InvalidRequest(
                "$hm-explore: at most 8 filters".into(),
            ));
        }
        let show: Vec<String> = get("show")
            .map(|s| {
                s.split(',')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .take(12)
                    .collect()
            })
            .unwrap_or_default();
        let flag = |k: &str| get(k).is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"));
        let alt = get("alt").map(|a| match a.split_once('|') {
            Some((u, v)) if !v.is_empty() => (u.to_string(), Some(v.to_string())),
            Some((u, _)) => (u.to_string(), None),
            None => (a, None),
        });
        let lang = get("lang").map(|l| l.to_ascii_lowercase());
        if let Some(l) = &lang
            && (l.len() > 16 || !l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        {
            return Err(HtsError::InvalidRequest(format!(
                "$hm-explore: lang '{l}' is not a language tag"
            )));
        }
        let req = ExploreRequest {
            mode,
            system,
            version: get("version"),
            code: get("code"),
            prefix: get("prefix"),
            sep: get("sep").unwrap_or_else(|| "-".into()),
            property: get("property"),
            filters,
            show,
            q: get("q").map(|q| q.chars().take(100).collect()),
            count,
            offset,
            order_display: get("order").as_deref() == Some("display"),
            order_desc: get("order").as_deref() == Some("code-desc"),
            descendants: flag("descendants"),
            props: flag("props"),
            alt,
            lang,
        };
        match req.mode {
            ExploreMode::Node | ExploreMode::Mappings if req.code.is_none() => {
                Err(HtsError::InvalidRequest(format!(
                    "$hm-explore: mode={} needs a code",
                    req.mode.as_str()
                )))
            }
            ExploreMode::Buckets if req.code.is_none() => Err(HtsError::InvalidRequest(
                "$hm-explore: mode=buckets needs the parent code".into(),
            )),
            ExploreMode::Facets if req.property.is_none() => Err(HtsError::InvalidRequest(
                "$hm-explore: mode=facets needs a property".into(),
            )),
            ExploreMode::Search if req.q.is_none() => Err(HtsError::InvalidRequest(
                "$hm-explore: mode=search needs q".into(),
            )),
            _ if req.sep.chars().count() != 1 => Err(HtsError::InvalidRequest(
                "$hm-explore: sep must be one character".into(),
            )),
            _ => Ok(req),
        }
    }

    /// The words of `q` (lower-cased, at most 6), for display matching.
    pub fn words(&self) -> Vec<String> {
        self.q
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .map(|w| w.to_lowercase())
            .filter(|w| !w.is_empty())
            .take(6)
            .collect()
    }

    fn cache_key(&self) -> String {
        format!(
            "{}|{}|{:?}|{:?}|{:?}|{}|{:?}|{:?}|{:?}|{:?}|{}|{}|{}|{}|{}|{}|{:?}|{:?}",
            self.mode.as_str(),
            self.system,
            self.version,
            self.code,
            self.prefix,
            self.sep,
            self.property,
            self.filters,
            self.show,
            self.q,
            self.count,
            self.offset,
            self.order_display,
            self.order_desc,
            self.descendants,
            self.props,
            self.alt,
            self.lang
        )
    }
}

/// One stored ConceptMap element of `mode=mappings`, from the point of view
/// of the looked-up concept (`system`/`code` are the other side).
#[derive(Clone, Debug, Default)]
pub struct MapRow {
    pub direction: String,
    pub map: String,
    pub map_version: Option<String>,
    pub map_title: Option<String>,
    pub system: String,
    pub code: String,
    pub relationship: String,
    pub display: String,
}

/// Drop rows that map the concept to itself and repeats from other stored
/// versions of the same map; keeps the first of each.
pub fn dedupe_maps(rows: Vec<MapRow>, system: &str, code: &str) -> Vec<MapRow> {
    let mut seen = std::collections::HashSet::new();
    rows.into_iter()
        .filter(|m| !(m.system == system && m.code == code))
        .filter(|m| {
            seen.insert((
                m.direction.clone(),
                m.map.clone(),
                m.system.clone(),
                m.code.clone(),
            ))
        })
        .collect()
}

/// The `mode=mappings` answer.
pub fn map_rows_json(code: &str, rows: Vec<MapRow>) -> Value {
    json!({
        "code": code,
        "items": rows.into_iter().map(|m| json!({
            "direction": m.direction,
            "map": match &m.map_version {
                Some(v) if !v.is_empty() => format!("{}|{v}", m.map),
                _ => m.map.clone(),
            },
            "mapTitle": m.map_title,
            "system": m.system,
            "code": m.code,
            "display": m.display,
            "relationship": m.relationship,
        })).collect::<Vec<_>>(),
    })
}

/// `%`, `_` and `\` escaped for a `LIKE … ESCAPE '\'` pattern.
pub fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The semantic tag of a SNOMED CT FSN (`Clinical finding (finding)` →
/// `finding`), from the newline-separated stored terms.
pub fn semantic_tag(terms: &[String]) -> Option<(String, String)> {
    terms.iter().find_map(|t| {
        let t = t.trim();
        let body = t.strip_suffix(')')?;
        let open = body.rfind('(')?;
        let tag = body[open + 1..].trim();
        if tag.is_empty() || tag.len() > 40 || open == 0 {
            return None;
        }
        Some((t.to_string(), tag.to_string()))
    })
}

/// The UCUM code system URL.
pub const UCUM_URL: &str = "http://unitsofmeasure.org";

/// UCUM concepts carry their canonical unit (`m`, `g.m-3`, `1`; empty for
/// special units such as `Cel`), which the explorer groups by dimension.
fn annotate_ucum(value: &mut Value) {
    fn one(it: &mut Value) {
        if let Some(code) = it["code"].as_str() {
            let c = crate::ucum::canonical(code)
                .map(|c| crate::ucum::canonical_unit(&c))
                .unwrap_or_default();
            it["canonical"] = json!(c);
        }
    }
    for key in ["items", "parents", "path"] {
        if let Some(a) = value.get_mut(key).and_then(Value::as_array_mut) {
            a.iter_mut().for_each(one);
        }
    }
    if let Some(a) = value
        .get_mut("children")
        .and_then(|c| c.get_mut("items"))
        .and_then(Value::as_array_mut)
    {
        a.iter_mut().for_each(one);
    }
}

// ── Response cache ───────────────────────────────────────────────────────────

/// Answers that took long (root scans, unfiltered facets over LOINC) are
/// kept for a while; imports of a new version get a new system id, and a
/// re-import of the same one is visible after [`CACHE_TTL`].
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const CACHE_SLOW: Duration = Duration::from_millis(120);
const CACHE_MAX: usize = 512;

type Cache = Mutex<HashMap<String, (Instant, Duration, Value)>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Overviews — `stats` (concept counts, metadata) and the facet counts with
/// at most one filter (the first two levels of a LOINC drill-down) — change
/// only with an import and are what every visitor opens first: kept longer.
const OVERVIEW_TTL: Duration = Duration::from_secs(6 * 60 * 60);

fn ttl_of(req: &ExploreRequest) -> Duration {
    match req.mode {
        ExploreMode::Stats => OVERVIEW_TTL,
        ExploreMode::Facets if req.filters.len() <= 1 && req.q.is_none() => OVERVIEW_TTL,
        _ => CACHE_TTL,
    }
}

fn cache_get(key: &str) -> Option<Value> {
    let guard = cache().lock().ok()?;
    let (at, ttl, v) = guard.get(key)?;
    (at.elapsed() < *ttl).then(|| v.clone())
}

fn cache_put(key: String, ttl: Duration, value: &Value) {
    if let Ok(mut guard) = cache().lock() {
        if guard.len() >= CACHE_MAX {
            guard.retain(|_, (at, ttl, _)| at.elapsed() < *ttl);
            if guard.len() >= CACHE_MAX {
                guard.clear();
            }
        }
        guard.insert(key, (Instant::now(), ttl, value.clone()));
    }
}

// ── Warm-up ──────────────────────────────────────────────────────────────────

/// Warm what the explorers open first, in the background, so the first
/// visitor after a restart does not wait: the SNOMED CT concept model (built
/// on first use, 5–15 s on a full edition), the page headers of the large
/// systems (exact concept counts) and the LOINC overview counts
/// (class types, and the classes of each class type). Repeated every five
/// hours, inside the six-hour life of those cache entries. Never blocks
/// start-up; `HTS_EXPLORE_WARMUP=false` turns it off.
pub fn spawn_warmup<B: TerminologyBackend + Clone + 'static>(state: AppState<B>) {
    if std::env::var("HTS_EXPLORE_WARMUP").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off"
        )
    }) {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        loop {
            warm(&state).await;
            tokio::time::sleep(Duration::from_secs(5 * 60 * 60)).await;
        }
    });
}

fn request(pairs: &[(&str, &str)]) -> Option<ExploreRequest> {
    let owned: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    ExploreRequest::from_pairs(&owned).ok()
}

async fn warm<B: TerminologyBackend>(state: &AppState<B>) {
    let started = Instant::now();
    let ctx = TenantContext::system();
    let snomed = matches!(
        state
            .backend()
            .snomed_model(&ctx, crate::snomed::SNOMED_URL, None)
            .await,
        Ok(Some(_))
    );
    // The explorer page header (`stats`, exact concept counts) of the large
    // systems, as the page asks for it (no version).
    let mut headers = 0usize;
    for system in [
        crate::snomed::SNOMED_URL,
        "http://hl7.org/fhir/sid/icd-10-cm",
        "https://twcore.mohw.gov.tw/ig/twcore/CodeSystem/icd-10-cm-2023-tw",
        "http://hl7.org/fhir/sid/ndc",
        UCUM_URL,
    ] {
        if let Some(req) = request(&[("mode", "stats"), ("system", system)])
            && explore(state, &req).await.is_ok()
        {
            headers += 1;
        }
    }
    // LOINC, as the explorer asks for it (the version it shows is pinned).
    let mut loinc = 0usize;
    if let Some(req) = request(&[("mode", "stats"), ("system", "http://loinc.org")])
        && let Ok(stats) = explore(state, &req).await
        && let Some(version) = stats["version"].as_str().map(str::to_string)
    {
        let base = [
            ("mode", "facets"),
            ("system", "http://loinc.org"),
            ("version", version.as_str()),
            ("count", "32"),
        ];
        let mut top = base.to_vec();
        top.push(("property", "CLASSTYPE"));
        if let Some(req) = request(&top)
            && let Ok(types) = explore(state, &req).await
        {
            loinc += 1;
            let values: Vec<String> = types["values"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v["value"].as_str().map(|s| format!("CLASSTYPE={s}")))
                        .collect()
                })
                .unwrap_or_default();
            for filter in values {
                let mut q = base.to_vec();
                q.push(("property", "CLASS"));
                q.push(("filter", filter.as_str()));
                if let Some(req) = request(&q)
                    && explore(state, &req).await.is_ok()
                {
                    loinc += 1;
                }
            }
        }
    }
    tracing::info!(
        snomed_model = snomed,
        page_headers = headers,
        loinc_overviews = loinc,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "terminology explorer warm-up done"
    );
}

// ── Handler ──────────────────────────────────────────────────────────────────

/// `GET /CodeSystem/$hm-explore`.
pub async fn explore_handler<B: TerminologyBackend>(
    State(state): State<AppState<B>>,
    RawQuery(raw): RawQuery,
) -> Result<Response, HtsError> {
    let pairs = crate::operations::params::parse_query_string(raw.as_deref().unwrap_or(""));
    let req = ExploreRequest::from_pairs(&pairs)?;
    let value = explore(&state, &req).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "private, max-age=60"),
        ],
        value.to_string(),
    )
        .into_response())
}

/// Answer one request (cache, SNOMED CT model, else the backend).
pub async fn explore<B: TerminologyBackend>(
    state: &AppState<B>,
    req: &ExploreRequest,
) -> Result<Value, HtsError> {
    let key = req.cache_key();
    if let Some(hit) = cache_get(&key) {
        return Ok(hit);
    }
    let started = Instant::now();
    let ctx = TenantContext::system();
    let backend = state.backend();
    let mut value = None;
    if crate::snomed::is_snomed(&req.system)
        && matches!(
            req.mode,
            ExploreMode::Children | ExploreMode::Node | ExploreMode::Search
        )
        && let Some(model) = backend
            .snomed_model(&ctx, &req.system, req.version.as_deref())
            .await?
    {
        value = Some(snomed::answer(backend, &ctx, &model, req).await?);
    }
    let mut value = match value {
        Some(v) => v,
        None => backend.explore(&ctx, req).await?,
    };
    if req.system == UCUM_URL {
        annotate_ucum(&mut value);
    }
    if started.elapsed() >= CACHE_SLOW {
        cache_put(key, ttl_of(req), &value);
    }
    Ok(value)
}

/// SNOMED CT answers from the in-memory concept model.
mod snomed {
    use super::*;
    use crate::snomed::model::{RelValue, SnomedModel};

    const ROOT: &str = "138875005";

    fn active_children(model: &SnomedModel, i: u32) -> Vec<u32> {
        model.children[i as usize]
            .iter()
            .copied()
            .filter(|c| model.active[*c as usize])
            .collect()
    }

    fn display(model: &SnomedModel, i: u32) -> String {
        model.display_of(i).unwrap_or_default().to_string()
    }

    fn sort_by_display(model: &SnomedModel, v: &mut [u32]) {
        v.sort_by_cached_key(|i| (display(model, *i).to_lowercase(), model.codes[*i as usize]));
    }

    async fn tags<B: TerminologyBackend>(
        backend: &B,
        ctx: &TenantContext,
        model: &SnomedModel,
        req: &ExploreRequest,
        idx: &[u32],
    ) -> HashMap<String, (String, String)> {
        let codes: Vec<String> = idx.iter().map(|i| model.code(*i)).collect();
        if codes.is_empty() {
            return HashMap::new();
        }
        match backend
            .snomed_concepts(ctx, &req.system, req.version.as_deref(), &codes)
            .await
        {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|(code, info)| semantic_tag(&info.row.terms).map(|t| (code, t)))
                .collect(),
            Err(_) => HashMap::new(),
        }
    }

    fn item(model: &SnomedModel, i: u32, tags: &HashMap<String, (String, String)>) -> Value {
        let code = model.code(i);
        let mut v = json!({
            "code": code,
            "display": display(model, i),
            "children": active_children(model, i).len(),
            "active": model.active[i as usize],
        });
        if let Some((_, tag)) = tags.get(&code) {
            v["tag"] = json!(tag);
        }
        v
    }

    fn path_to_root(model: &SnomedModel, i: u32) -> Vec<u32> {
        let mut path = vec![i];
        let mut cur = i;
        while path.len() < MAX_DEPTH {
            let mut parents: Vec<u32> = model.parents[cur as usize].clone();
            if parents.is_empty() {
                break;
            }
            // Prefer the parent closest to the root (fewest ancestors walked
            // is costly; the lowest code is a stable, cheap tie-breaker).
            parents.sort_by_key(|p| model.codes[*p as usize]);
            cur = parents[0];
            if path.contains(&cur) {
                break;
            }
            path.push(cur);
        }
        path.reverse();
        path
    }

    pub(super) async fn answer<B: TerminologyBackend>(
        backend: &B,
        ctx: &TenantContext,
        model: &SnomedModel,
        req: &ExploreRequest,
    ) -> Result<Value, HtsError> {
        let version = model.version.clone();
        match req.mode {
            ExploreMode::Children => {
                let kids: Vec<u32> = match req.code.as_deref() {
                    None => model.idx(ROOT).into_iter().collect(),
                    Some(code) => {
                        let i = model.idx(code).ok_or_else(|| {
                            HtsError::NotFound(format!("SNOMED CT concept {code} is not stored"))
                        })?;
                        let mut v = active_children(model, i);
                        sort_by_display(model, &mut v);
                        v
                    }
                };
                let total = kids.len();
                let page: Vec<u32> = kids
                    .into_iter()
                    .skip(req.offset as usize)
                    .take(req.count as usize)
                    .collect();
                let tags = tags(backend, ctx, model, req, &page).await;
                Ok(json!({
                    "system": req.system,
                    "version": version,
                    "parent": req.code,
                    "total": total,
                    "offset": req.offset,
                    "items": page.iter().map(|i| item(model, *i, &tags)).collect::<Vec<_>>(),
                }))
            }
            ExploreMode::Node => {
                let code = req.code.as_deref().unwrap_or_default();
                let i = model.idx(code).ok_or_else(|| {
                    HtsError::NotFound(format!("SNOMED CT concept {code} is not stored"))
                })?;
                let mut parents = model.parents[i as usize].clone();
                sort_by_display(model, &mut parents);
                let mut kids = active_children(model, i);
                sort_by_display(model, &mut kids);
                let child_total = kids.len();
                kids.truncate(req.count as usize);
                let path = path_to_root(model, i);
                let rels = &model.rels[i as usize];
                let mut wanted: Vec<u32> = vec![i];
                wanted.extend(parents.iter().copied());
                wanted.extend(kids.iter().copied());
                wanted.extend(path.iter().copied());
                for r in rels {
                    if let RelValue::Concept(v) = r.value {
                        wanted.push(v);
                    }
                }
                wanted.sort_unstable();
                wanted.dedup();
                let tags = tags(backend, ctx, model, req, &wanted).await;
                let attributes: Vec<Value> = rels
                    .iter()
                    .map(|r| {
                        let mut a = json!({
                            "group": r.group,
                            "type": model.code(r.type_id),
                            "typeDisplay": display(model, r.type_id),
                        });
                        match &r.value {
                            RelValue::Concept(v) => {
                                a["value"] = json!(model.code(*v));
                                a["valueDisplay"] = json!(display(model, *v));
                                if let Some((_, tag)) = tags.get(&model.code(*v)) {
                                    a["valueTag"] = json!(tag);
                                }
                            }
                            RelValue::Concrete(c) => {
                                a["value"] = json!(c.render());
                                a["concrete"] = json!(true);
                            }
                        }
                        a
                    })
                    .collect();
                let fsn = tags.get(code).map(|(f, _)| f.clone());
                let tag = tags.get(code).map(|(_, t)| t.clone());
                Ok(json!({
                    "system": req.system,
                    "version": version,
                    "code": code,
                    "display": display(model, i),
                    "fsn": fsn,
                    "tag": tag,
                    "active": model.active[i as usize],
                    "primitive": model.primitive[i as usize],
                    "parents": parents.iter().map(|p| item(model, *p, &tags)).collect::<Vec<_>>(),
                    "children": {
                        "total": child_total,
                        "items": kids.iter().map(|c| item(model, *c, &tags)).collect::<Vec<_>>(),
                    },
                    "attributes": attributes,
                    "path": path.iter().map(|p| item(model, *p, &tags)).collect::<Vec<_>>(),
                }))
            }
            _ => {
                // Search: the stored displays (SQL), annotated from the model.
                let mut base = backend.explore(ctx, req).await?;
                let codes: Vec<u32> = base["items"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|it| it["code"].as_str().and_then(|c| model.idx(c)))
                            .collect()
                    })
                    .unwrap_or_default();
                let tags = tags(backend, ctx, model, req, &codes).await;
                if let Some(items) = base["items"].as_array_mut() {
                    for it in items.iter_mut() {
                        let Some(i) = it["code"].as_str().and_then(|c| model.idx(c)) else {
                            continue;
                        };
                        let fresh = item(model, i, &tags);
                        it["children"] = fresh["children"].clone();
                        it["active"] = fresh["active"].clone();
                        if let Some(tag) = fresh.get("tag") {
                            it["tag"] = tag.clone();
                        }
                    }
                    // Active concepts first; the SQL order within.
                    items.sort_by_key(|it| !it["active"].as_bool().unwrap_or(true));
                }
                Ok(base)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(q: &str) -> Vec<(String, String)> {
        crate::operations::params::parse_query_string(q)
    }

    #[test]
    fn parses_and_caps() {
        let r = ExploreRequest::from_pairs(&pairs(
            "mode=children&system=http://x&count=9999&filter=CLASS%3DCHEM&show=A,B&alt=http://y%7C2",
        ))
        .unwrap();
        assert_eq!(r.mode, ExploreMode::Children);
        assert_eq!(r.count, MAX_COUNT);
        assert_eq!(r.filters, vec![("CLASS".to_string(), "CHEM".to_string())]);
        assert_eq!(r.show, vec!["A".to_string(), "B".to_string()]);
        assert_eq!(r.alt, Some(("http://y".into(), Some("2".into()))));
        assert!(ExploreRequest::from_pairs(&pairs("mode=nope&system=x")).is_err());
        assert!(ExploreRequest::from_pairs(&pairs("mode=node&system=x")).is_err());
        assert!(ExploreRequest::from_pairs(&pairs("mode=facets&system=x")).is_err());
        assert!(ExploreRequest::from_pairs(&pairs("mode=children")).is_err());
        assert!(ExploreRequest::from_pairs(&pairs("mode=search&system=x&q=a&lang=x;y")).is_err());
    }

    #[test]
    fn semantic_tags_come_from_the_fsn() {
        let terms = vec![
            "Type 2 diabetes mellitus".to_string(),
            "Diabetes mellitus type 2 (disorder)".to_string(),
        ];
        assert_eq!(
            semantic_tag(&terms).map(|t| t.1).as_deref(),
            Some("disorder")
        );
        assert_eq!(semantic_tag(&["(x)".to_string()]), None);
        assert_eq!(like_escape("a_b%c\\"), "a\\_b\\%c\\\\");
    }
}
