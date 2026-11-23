//! Terminology explorers (HealthyMind): one visual explorer per kind of
//! code system, at `GET /ui/terminology/explore?system=…&version=…`.
//!
//! | Kind | Systems | Shape |
//! |---|---|---|
//! | `snomed` | SNOMED CT | is-a tree + focus graph (parents above, children below, attribute groups beside) |
//! | `icd` | ICD-10-CM / PCS / ICD-9-CM, the Taiwan editions and zh-Hant supplements | chapter overview sized by code count, chapter → block → category → code tree, English and Chinese side by side |
//! | `loinc` | LOINC | clusters by axis (class type, class, component, system, …) with drill-down and part chips |
//! | `ndc` | NDC | product type → labeler → product → package cluster cards |
//! | `ucum` | UCUM | units grouped by dimension |
//! | `tree` / `flat` / `auto` | everything else | lazy tree when the system has a hierarchy, else a grouped list |
//!
//! The page is a shell; the browser script (`term-explorer.js`) reads two
//! JSON routes of this module: `explore/data` (a whitelisted pass-through to
//! the terminology server's `$hm-explore`, which pages and caps every read)
//! and `explore/concept` (`$lookup` and the `$translate` mappings of one
//! concept, for the side panel). Nothing loads a whole system.
//!
//! The kind follows the system URL ([`kind_for`]); `HTS_UI_EXPLORER_KINDS`
//! (`url=kind;url=kind`) adds or overrides entries, and `?kind=` overrides
//! one page.

use std::sync::Arc;

use askama::Template;
use axum::{
    Router,
    extract::{RawQuery, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};

use crate::i18n::{I18n, RequestLocale};
use crate::upstream::{ConceptRef, MappingDirection, OutcomeView, UpstreamError};
use crate::{Chrome, HtsUiState};

pub(crate) fn routes() -> Router<Arc<HtsUiState>> {
    Router::new()
        .route("/hts/explore", get(explore_page))
        .route("/hts/explore/data", get(explore_data))
        .route("/hts/explore/concept", get(explore_concept))
}

// ── Kind selection ──────────────────────────────────────────────────────

/// The explorer a system gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Snomed,
    Icd,
    Loinc,
    Ndc,
    Ucum,
    Tree,
    Flat,
    Auto,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "snomed" => Self::Snomed,
            "icd" => Self::Icd,
            "loinc" => Self::Loinc,
            "ndc" => Self::Ndc,
            "ucum" => Self::Ucum,
            "tree" => Self::Tree,
            "flat" => Self::Flat,
            "auto" => Self::Auto,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Snomed => "snomed",
            Self::Icd => "icd",
            Self::Loinc => "loinc",
            Self::Ndc => "ndc",
            Self::Ucum => "ucum",
            Self::Tree => "tree",
            Self::Flat => "flat",
            Self::Auto => "auto",
        }
    }
}

/// How one system is explored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KindConfig {
    pub kind: Kind,
    /// The system whose hierarchy shapes the tree, when not the explored one
    /// (a Taiwan ICD edition is flat; its codes hang in the ICD-10-CM tree).
    pub structure: Option<String>,
    /// Companion labels for the same codes (`url[|version]`): the Chinese
    /// edition next to the English one, or the explored edition itself when
    /// the tree comes from `structure`.
    pub alt: Option<String>,
}

pub const SNOMED: &str = "http://snomed.info/sct";
pub const LOINC: &str = "http://loinc.org";
pub const NDC: &str = "http://hl7.org/fhir/sid/ndc";
pub const UCUM: &str = "http://unitsofmeasure.org";
pub const ICD10CM: &str = "http://hl7.org/fhir/sid/icd-10-cm";
pub const ICD10PCS: &str = "http://www.cms.gov/Medicare/Coding/ICD10";
pub const ICD9CM: &str = "http://hl7.org/fhir/sid/icd-9-cm";
const TW_IG: &str = "https://twcore.mohw.gov.tw/ig/twcore/CodeSystem/";
const HM_CS: &str = "https://terminology.healthymind.tw/CodeSystem/";

/// The built-in choice for `url` (before `HTS_UI_EXPLORER_KINDS`).
pub fn builtin_kind(url: &str) -> KindConfig {
    let plain = |kind| KindConfig {
        kind,
        structure: None,
        alt: None,
    };
    let edition_of = |base: &str| KindConfig {
        kind: Kind::Icd,
        structure: Some(base.to_string()),
        alt: Some(url.to_string()),
    };
    match url {
        SNOMED => plain(Kind::Snomed),
        LOINC => plain(Kind::Loinc),
        NDC => plain(Kind::Ndc),
        UCUM => plain(Kind::Ucum),
        ICD10CM => KindConfig {
            kind: Kind::Icd,
            structure: None,
            alt: Some(format!("{TW_IG}icd-10-cm-2023-tw")),
        },
        ICD10PCS => KindConfig {
            kind: Kind::Icd,
            structure: None,
            alt: Some(format!("{TW_IG}icd-10-pcs-2023-tw")),
        },
        ICD9CM => plain(Kind::Icd),
        _ => {
            let tail = url
                .strip_prefix(TW_IG)
                .or_else(|| url.strip_prefix(HM_CS))
                .unwrap_or("");
            if tail.starts_with("icd-10-cm-") {
                edition_of(ICD10CM)
            } else if tail.starts_with("icd-10-pcs-") {
                edition_of(ICD10PCS)
            } else {
                plain(Kind::Auto)
            }
        }
    }
}

/// `HTS_UI_EXPLORER_KINDS` entries: `url=kind` separated by `;` or newlines.
pub fn parse_overrides(raw: &str) -> Vec<(String, Kind)> {
    raw.split([';', '\n'])
        .filter_map(|entry| {
            let (url, kind) = entry.trim().rsplit_once('=')?;
            Some((url.trim().to_string(), Kind::parse(kind)?))
        })
        .filter(|(url, _)| !url.is_empty())
        .collect()
}

fn overrides() -> &'static [(String, Kind)] {
    static O: std::sync::OnceLock<Vec<(String, Kind)>> = std::sync::OnceLock::new();
    O.get_or_init(|| {
        std::env::var("HTS_UI_EXPLORER_KINDS")
            .map(|v| parse_overrides(&v))
            .unwrap_or_default()
    })
}

/// The explorer for `url`: an explicit `requested` kind, then
/// `HTS_UI_EXPLORER_KINDS`, then [`builtin_kind`].
pub fn kind_for(url: &str, requested: Option<Kind>, table: &[(String, Kind)]) -> KindConfig {
    let mut cfg = builtin_kind(url);
    let chosen = requested.or_else(|| table.iter().find(|(u, _)| u == url).map(|(_, k)| *k));
    if let Some(kind) = chosen
        && kind != cfg.kind
    {
        cfg = KindConfig {
            kind,
            structure: None,
            alt: None,
        };
    }
    cfg
}

// ── Query plumbing ──────────────────────────────────────────────────────

fn pairs(raw: Option<&str>) -> Vec<(String, String)> {
    form_urlencoded::parse(raw.unwrap_or("").as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn first<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
}

/// Keys `explore/data` forwards to `$hm-explore`.
const FORWARDED: &[&str] = &[
    "mode",
    "system",
    "version",
    "code",
    "prefix",
    "sep",
    "property",
    "filter",
    "show",
    "q",
    "count",
    "offset",
    "order",
    "descendants",
    "props",
    "alt",
    "lang",
];

/// The forwarded subset of `pairs` (at most 40 pairs, values ≤ 512 chars).
pub fn forwarded(pairs: &[(String, String)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .filter(|(k, v)| FORWARDED.contains(&k.as_str()) && v.len() <= 512)
        .take(40)
        .cloned()
        .collect()
}

fn json_response(status: StatusCode, body: Value) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "private, max-age=60"),
        ],
        body.to_string(),
    )
        .into_response()
}

fn error_json(err: &UpstreamError) -> Response {
    let (status, message) = match err {
        UpstreamError::Outcome {
            status, outcome, ..
        } => (*status, outcome.diagnostics.clone()),
        UpstreamError::HttpStatus { status, .. } => (*status, format!("HTTP {status}")),
        other => (502, other.to_string()),
    };
    let status = StatusCode::from_u16(status)
        .ok()
        .filter(|s| s.is_client_error() || s.is_server_error())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    json_response(status, json!({"error": message}))
}

// ── Routes ──────────────────────────────────────────────────────────────

/// `GET explore/data` — `$hm-explore`, whitelisted.
async fn explore_data(State(state): State<Arc<HtsUiState>>, RawQuery(raw): RawQuery) -> Response {
    let query = forwarded(&pairs(raw.as_deref()));
    if first(&query, "system").is_none() {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({"error": "system is required"}),
        );
    }
    match state.upstream.explore_json(&query).await {
        Ok(body) => json_response(StatusCode::OK, body),
        Err(e) => error_json(&e),
    }
}

/// `GET explore/concept?system&code[&version][&lang]` — the side panel:
/// `$lookup` (every property) and the forward and reverse `$translate`
/// matches across all stored ConceptMaps.
async fn explore_concept(
    State(state): State<Arc<HtsUiState>>,
    RawQuery(raw): RawQuery,
) -> Response {
    let p = pairs(raw.as_deref());
    let (Some(system), Some(code)) = (first(&p, "system"), first(&p, "code")) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({"error": "system and code are required"}),
        );
    };
    let reference = ConceptRef {
        system: system.to_string(),
        code: code.to_string(),
        version: first(&p, "version").map(str::to_string),
        display_language: first(&p, "lang").map(str::to_string),
    };
    let (identity, forward, reverse) = tokio::join!(
        state.upstream.concept_identity(&reference),
        state
            .upstream
            .concept_mappings(&reference, MappingDirection::Forward),
        state
            .upstream
            .concept_mappings(&reference, MappingDirection::Reverse),
    );
    let identity = match identity {
        Ok(i) => i,
        Err(e) => return error_json(&e),
    };
    let rel = |r: &crate::upstream::ConceptRelative| json!({"code": r.code, "display": r.display});
    let mut mappings = Vec::new();
    for (dir, result) in [("forward", forward), ("reverse", reverse)] {
        if let Ok(m) = result {
            for g in &m.groups {
                for row in &g.rows {
                    mappings.push(json!({
                        "direction": dir,
                        "map": g.origin,
                        "system": row.system,
                        "code": row.code,
                        "display": row.display,
                        "relationship": row.mapping_value,
                    }));
                }
            }
        }
    }
    mappings.truncate(60);
    let body = json!({
        "system": identity.system,
        "code": identity.code,
        "version": identity.version,
        "name": identity.name,
        "display": identity.display,
        "definition": identity.definition,
        "abstract": identity.is_abstract,
        "inactive": identity.inactive,
        "parents": identity.parents.iter().take(30).map(rel).collect::<Vec<_>>(),
        "children": identity.children.iter().take(30).map(rel).collect::<Vec<_>>(),
        "childCount": identity.children.len(),
        "designations": identity.designations.iter().take(40).map(|d| json!({
            "language": d.language, "use": d.use_code, "value": d.value,
        })).collect::<Vec<_>>(),
        "properties": identity.properties.iter().take(60).map(|pr| json!({
            "code": pr.code, "value": pr.value,
        })).collect::<Vec<_>>(),
        "supplements": identity.used_supplements,
        "mappings": mappings,
        "permalink": reference.permalink(),
    });
    json_response(StatusCode::OK, body)
}

/// The explorer page.
#[derive(Template)]
#[template(path = "pages/explore.html")]
struct ExplorePage<'a> {
    chrome: Chrome<'a>,
    system: String,
    version: String,
    kind: &'static str,
    title: String,
    concepts: Option<i64>,
    hierarchy_meaning: String,
    content: String,
    cs_id: String,
    /// `<script type=application/json>` payload for the browser script.
    config_json: String,
    outcome: Option<OutcomeView>,
    degraded: Option<&'static str>,
}

impl ExplorePage<'_> {
    fn kind_label_key(&self) -> String {
        format!("tx-explore-kind-{}", self.kind)
    }
}

/// The message keys the browser script uses (sent once, as JSON).
const CLIENT_KEYS: &[&str] = &[
    "tx-explore-search-placeholder",
    "tx-explore-searching",
    "tx-explore-no-results",
    "tx-explore-error",
    "tx-explore-retry",
    "tx-explore-load-more",
    "tx-explore-more-remaining",
    "tx-explore-loading",
    "tx-explore-children",
    "tx-explore-parents",
    "tx-explore-attributes",
    "tx-explore-group",
    "tx-explore-path",
    "tx-explore-root",
    "tx-explore-back",
    "tx-explore-forward",
    "tx-explore-home",
    "tx-explore-hierarchy",
    "tx-explore-focus",
    "tx-explore-details",
    "tx-explore-close",
    "tx-explore-copy-code",
    "tx-explore-copy-coding",
    "tx-explore-copied",
    "tx-explore-open-concept",
    "tx-explore-designations",
    "tx-explore-properties",
    "tx-explore-mappings",
    "tx-explore-mappings-none",
    "tx-explore-maps-to",
    "tx-explore-mapped-from",
    "tx-explore-definition",
    "tx-explore-inactive",
    "tx-explore-active",
    "tx-explore-abstract",
    "tx-explore-primitive",
    "tx-explore-defined",
    "tx-explore-codes",
    "tx-explore-concepts",
    "tx-explore-not-in-edition",
    "tx-explore-chapters",
    "tx-explore-chapter-hint",
    "tx-explore-all-chapters",
    "tx-explore-english",
    "tx-explore-chinese",
    "tx-explore-group-by",
    "tx-explore-filters",
    "tx-explore-clear-filters",
    "tx-explore-terms",
    "tx-explore-clusters",
    "tx-explore-top-values",
    "tx-explore-values-truncated",
    "tx-explore-filter-values",
    "tx-explore-axis-CLASSTYPE",
    "tx-explore-axis-CLASS",
    "tx-explore-axis-COMPONENT",
    "tx-explore-axis-PROPERTY",
    "tx-explore-axis-TIME_ASPCT",
    "tx-explore-axis-SYSTEM",
    "tx-explore-axis-SCALE_TYP",
    "tx-explore-axis-METHOD_TYP",
    "tx-explore-loinc-classtype-1",
    "tx-explore-loinc-classtype-2",
    "tx-explore-loinc-classtype-3",
    "tx-explore-loinc-classtype-4",
    "tx-explore-ndc-types",
    "tx-explore-ndc-labelers",
    "tx-explore-ndc-labeler",
    "tx-explore-ndc-products",
    "tx-explore-ndc-packages",
    "tx-explore-ndc-unclassified",
    "tx-explore-ndc-sample",
    "tx-explore-ucum-dimension",
    "tx-explore-ucum-dimensionless",
    "tx-explore-ucum-units",
    "tx-explore-groups",
    "tx-explore-all",
    "tx-explore-list",
    "tx-explore-tree",
    "tx-explore-grouped-by",
    "tx-explore-ungrouped",
    "tx-explore-roots-truncated",
    "tx-explore-select-hint",
    "tx-explore-descendants",
    "tx-explore-pick-cluster",
];

/// `GET /explore?system=…[&version=…][&kind=…]`.
async fn explore_page(
    State(state): State<Arc<HtsUiState>>,
    RawQuery(raw): RawQuery,
    locale: RequestLocale,
) -> Response {
    let i18n = I18n::new(locale);
    let chrome = Chrome {
        i18n,
        active_page: "code-systems",
        fhir_version: state.fhir_version,
        version: state.version,
    };
    let p = pairs(raw.as_deref());
    let system = first(&p, "system").unwrap_or_default().to_string();
    let version = first(&p, "version").unwrap_or_default().to_string();
    let requested = first(&p, "kind").and_then(Kind::parse);
    let cfg = kind_for(&system, requested, overrides());
    let mut page = ExplorePage {
        chrome,
        system: system.clone(),
        version: version.clone(),
        kind: cfg.kind.as_str(),
        title: system.clone(),
        concepts: None,
        hierarchy_meaning: String::new(),
        content: String::new(),
        cs_id: String::new(),
        config_json: String::new(),
        outcome: None,
        degraded: None,
    };
    if system.is_empty() {
        page.outcome = Some(OutcomeView::invalid_input(
            "system is required (explore?system=…&version=…)",
        ));
        let mut r = crate::render_page(page.render());
        *r.status_mut() = StatusCode::BAD_REQUEST;
        return r;
    }
    let mut q = vec![
        ("mode".to_string(), "stats".to_string()),
        ("system".to_string(), system.clone()),
    ];
    if !version.is_empty() {
        q.push(("version".into(), version.clone()));
    }
    if matches!(cfg.kind, Kind::Auto | Kind::Flat | Kind::Tree) {
        q.push(("props".into(), "1".into()));
    }
    let stats = match state.upstream.explore_json(&q).await {
        Ok(s) => s,
        Err(e) => {
            let status = match &e {
                UpstreamError::Outcome {
                    status, outcome, ..
                } => {
                    page.outcome = Some((**outcome).clone());
                    *status
                }
                UpstreamError::HttpStatus { status, .. } => {
                    page.outcome = Some(OutcomeView::invalid_input(e.to_string()));
                    *status
                }
                other => {
                    page.degraded = Some(other.degraded_reason());
                    502
                }
            };
            let mut r = crate::render_page(page.render());
            *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            return r;
        }
    };
    let s = |k: &str| stats[k].as_str().unwrap_or_default().to_string();
    page.title = [s("title"), s("name")]
        .into_iter()
        .find(|t| !t.is_empty())
        .unwrap_or_else(|| system.clone());
    page.concepts = stats["concepts"].as_i64();
    page.hierarchy_meaning = s("hierarchyMeaning");
    page.content = s("content");
    page.cs_id = s("id");
    page.version = s("version");
    let mut kind = cfg.kind;
    if kind == Kind::Auto {
        kind = if stats["hierarchy"].as_bool().unwrap_or(false) {
            Kind::Tree
        } else {
            Kind::Flat
        };
    }
    page.kind = kind.as_str();
    let strings: serde_json::Map<String, Value> = CLIENT_KEYS
        .iter()
        .map(|k| {
            // Messages with a count carry a `__N__` marker the script fills.
            let text = if *k == "tx-explore-more-remaining" {
                page.chrome.i18n.t_arg(k, "count", "__N__".to_string())
            } else {
                page.chrome.i18n.t(k)
            };
            (k.to_string(), json!(text))
        })
        .collect();
    let ui = crate::paths::ui();
    let config = json!({
        "system": system,
        "version": page.version,
        "kind": kind.as_str(),
        "structure": cfg.structure,
        "alt": cfg.alt,
        "title": page.title,
        "concepts": page.concepts,
        "hierarchy": stats["hierarchy"],
        "closure": stats["closure"],
        "content": page.content,
        "properties": stats.get("properties").cloned().unwrap_or(Value::Null),
        "designationLanguages": stats.get("designationLanguages").cloned().unwrap_or(Value::Null),
        "propertyDefs": stats.get("propertyDefs").cloned().unwrap_or(Value::Null),
        "dataUrl": format!("{ui}/explore/data"),
        "conceptUrl": format!("{ui}/explore/concept"),
        "conceptPage": format!("{ui}/concepts"),
        "lang": page.chrome.i18n.lang(),
        "strings": strings,
    });
    // `</` must not end the <script> element early.
    page.config_json = config.to_string().replace("</", "<\\/");
    crate::render_page(page.render())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_follow_the_system_url() {
        assert_eq!(builtin_kind(SNOMED).kind, Kind::Snomed);
        assert_eq!(builtin_kind(LOINC).kind, Kind::Loinc);
        assert_eq!(builtin_kind(NDC).kind, Kind::Ndc);
        assert_eq!(builtin_kind(UCUM).kind, Kind::Ucum);
        let cm = builtin_kind(ICD10CM);
        assert_eq!(cm.kind, Kind::Icd);
        assert_eq!(cm.structure, None);
        assert!(cm.alt.unwrap().ends_with("icd-10-cm-2023-tw"));
        let tw = builtin_kind("https://twcore.mohw.gov.tw/ig/twcore/CodeSystem/icd-10-cm-2014-tw");
        assert_eq!(tw.kind, Kind::Icd);
        assert_eq!(tw.structure.as_deref(), Some(ICD10CM));
        assert_eq!(
            tw.alt.as_deref(),
            Some("https://twcore.mohw.gov.tw/ig/twcore/CodeSystem/icd-10-cm-2014-tw")
        );
        let zh = builtin_kind("https://terminology.healthymind.tw/CodeSystem/icd-10-pcs-zh-hant");
        assert_eq!(zh.structure.as_deref(), Some(ICD10PCS));
        assert_eq!(builtin_kind("http://example.org/cs").kind, Kind::Auto);
        assert_eq!(
            builtin_kind("https://twcore.mohw.gov.tw/ig/twcore/CodeSystem/marital-status-tw").kind,
            Kind::Auto
        );
    }

    #[test]
    fn overrides_and_requests_win() {
        let table =
            parse_overrides(" http://a.example/cs = tree ;http://b=nope\nhttp://loinc.org=flat");
        assert_eq!(
            table,
            vec![
                ("http://a.example/cs".to_string(), Kind::Tree),
                ("http://loinc.org".to_string(), Kind::Flat)
            ]
        );
        assert_eq!(
            kind_for("http://a.example/cs", None, &table).kind,
            Kind::Tree
        );
        assert_eq!(kind_for(LOINC, None, &table).kind, Kind::Flat);
        assert_eq!(kind_for(LOINC, Some(Kind::Loinc), &table).kind, Kind::Loinc);
        // Asking for the built-in kind keeps its companion settings.
        let cm = kind_for(ICD10CM, Some(Kind::Icd), &table);
        assert!(cm.alt.is_some());
        assert_eq!(kind_for(SNOMED, Some(Kind::Tree), &[]).alt, None);
    }

    #[test]
    fn only_whitelisted_keys_are_forwarded() {
        let p = pairs(Some(
            "mode=children&system=x&filter=A%3D1&filter=B%3D2&evil=1&_format=xml&q=a%20b",
        ));
        let f = forwarded(&p);
        let keys: Vec<&str> = f.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["mode", "system", "filter", "filter", "q"]);
        assert_eq!(f[4].1, "a b");
    }
}
