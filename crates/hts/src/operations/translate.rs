//! Handler for `POST /ConceptMap/$translate`.
//!
//! Accepts a FHIR Parameters resource naming the source concept — as `code` +
//! `system`, a `coding`, or a `codeableConcept` (R5: the `source*` spellings, and
//! the `target*` ones for reverse translation) — plus optional `url` (ConceptMap
//! URL) and `reverse`. Returns a FHIR Parameters resource with a `result` boolean
//! and zero or more `match` parts.
//!
//! # FHIR specification
//! <https://hl7.org/fhir/conceptmap-operation-translate.html>

use axum::{
    Json,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, header},
    response::Response,
};
use helios_persistence::tenant::TenantContext;
use serde_json::{Value, json};

use crate::error::HtsError;
use crate::state::AppState;
use crate::traits::{ConceptMapOperations, TerminologyBackend};
use crate::types::TranslateRequest;

use super::format::{fhir_respond, negotiate_format};
use super::params::{
    extract_codeable_concept, extract_coding, extract_parameter_array, find_str_param,
    normalize_canonical_params, parse_query_string, query_params_to_fhir_params,
};

/// Resolve a `(system, code)` pair from the Coding/CodeableConcept spellings of a
/// `$translate` input parameter.
///
/// The spec defines the source concept three ways — `code` + `system`, `coding`, or
/// `codeableConcept` (R5: `sourceCoding` / `sourceCodeableConcept`, and the `target*`
/// forms for reverse translation) — but this handler only ever read the scalar pair, so
/// a spec-legal `coding` request was rejected as "Missing required parameter". See #287.
///
/// A CodeableConcept may carry several codings; the first is used, matching how
/// `$validate-code` treats the same input.
fn coding_pair(params: &[Value], coding_name: &str, cc_name: &str) -> Option<(String, String)> {
    if let Some((system, code, _display)) = extract_coding(params, coding_name) {
        return Some((system, code));
    }
    extract_codeable_concept(params, cc_name)?
        .into_iter()
        .next()
}

/// Core translation logic shared by the POST and GET handlers.
///
/// Extracts the source concept (required — see [`coding_pair`] for the accepted
/// spellings) and optional `url` (ConceptMap URL), `source`, `target`,
/// `targetSystem`, `reverse`, and `date` from `params`, delegates to
/// [`ConceptMapOperations::translate`], and assembles the FHIR `Parameters`
/// response.
///
/// `reverse` is parsed from `"true"` / `"false"` strings so it works for both
/// GET (query-string) and POST (JSON boolean) inputs.
///
/// ## Returns
///
/// A FHIR `Parameters` resource with a `result` boolean and zero or more
/// `match` parts.  Each `match` part contains `equivalence` and
/// `relationship` codes, a `concept` (`valueCoding`) for the target side
/// of the matched ConceptMap element, an `originMap` canonical reference,
/// and (in reverse responses) a `source` (`valueCoding`) for the source
/// side of the matched element.
///
/// ## Errors
///
/// Returns [`HtsError::InvalidRequest`] when no source concept is supplied in any
/// of the accepted spellings.
pub(crate) async fn process_translate<B: TerminologyBackend>(
    state: &AppState<B>,
    params: Vec<Value>,
) -> Result<Value, HtsError> {
    // Canonical `X|V` spellings: the ConceptMap `url` promotes its version to
    // `conceptMapVersion`, the source `system` to `version`. The ValueSet /
    // target-system canonicals have no companion version parameter in the
    // operation definition, so their version suffix is dropped.
    let mut params = params;
    normalize_canonical_params(&mut params, "url", Some("conceptMapVersion"))?;
    normalize_canonical_params(&mut params, "system", Some("version"))?;
    normalize_canonical_params(&mut params, "sourceSystem", Some("version"))?;
    for name in ["source", "target", "targetSystem", "targetsystem"] {
        normalize_canonical_params(&mut params, name, None)?;
    }
    // R4 names: `code`, `system`. R5 names: `sourceCode`, `sourceSystem`,
    // `targetCode`, `targetSystem`. Accept either form.
    let mut source_code =
        find_str_param(&params, "sourceCode").or_else(|| find_str_param(&params, "code"));
    // R4 has no target-code input; `targetcode` is accepted as a spelling of
    // the R5 `targetCode` (IG `translate/translate-reverse-r4-b`).
    let mut target_code =
        find_str_param(&params, "targetCode").or_else(|| find_str_param(&params, "targetcode"));
    let mut source_system =
        find_str_param(&params, "sourceSystem").or_else(|| find_str_param(&params, "system"));
    let mut target_system =
        find_str_param(&params, "targetSystem").or_else(|| find_str_param(&params, "targetsystem"));

    // The scalar pair wins when present; Coding/CodeableConcept only fill the gaps, so a
    // request mixing the spellings keeps the explicit `code`/`system` the caller named.
    if source_code.is_none() {
        if let Some((system, code)) = coding_pair(&params, "coding", "codeableConcept")
            .or_else(|| coding_pair(&params, "sourceCoding", "sourceCodeableConcept"))
        {
            source_code = Some(code);
            source_system = source_system.or(Some(system).filter(|s| !s.is_empty()));
        }
    }
    if target_code.is_none() {
        if let Some((system, code)) = coding_pair(&params, "targetCoding", "targetCodeableConcept")
        {
            target_code = Some(code);
            target_system = target_system.or(Some(system).filter(|s| !s.is_empty()));
        }
    }

    // Need at least one of source code (forward) or target code (reverse).
    // Name every accepted spelling so a client that sent a valid-but-unread
    // shape (e.g. the R5 `sourceCoding`) can self-correct — the old message
    // listed only the scalar forms and gave no hint the Coding/CodeableConcept
    // spellings this handler accepts even exist. See #288.
    if source_code.is_none() && target_code.is_none() {
        return Err(HtsError::InvalidRequest(
            "Missing source concept: provide code (with system), coding, or codeableConcept \
             (R5 aliases sourceCode/sourceSystem, sourceCoding, sourceCodeableConcept accepted); \
             for a reverse translation supply targetCode, targetCoding, or targetCodeableConcept"
                .into(),
        ));
    }

    // `reverse` arrives as valueBoolean (POST) or plain string "true"/"false" (GET).
    let reverse_flag = find_str_param(&params, "reverse")
        .map(|s| s == "true")
        .unwrap_or(false);
    // The request is reverse-mode if the caller asked for it explicitly
    // (`reverse=true`) or supplied `targetCode` instead of `sourceCode`.
    let is_reverse = reverse_flag || target_code.is_some();

    let req = TranslateRequest {
        url: find_str_param(&params, "url"),
        system: source_system,
        // `code` is the forward-mode lookup. Empty string when only
        // `targetCode` is supplied (reverse mode keyed on `target_code`).
        code: source_code.unwrap_or_default(),
        source: find_str_param(&params, "source"),
        target: find_str_param(&params, "target"),
        target_system,
        target_code,
        reverse: reverse_flag,
        date: find_str_param(&params, "date"),
        map_version: find_str_param(&params, "conceptMapVersion"),
    };

    let ctx = TenantContext::system();
    let tx_maps: Vec<Value> = super::params::collect_resource_params(&params, "tx-resource")
        .into_iter()
        .filter(|r| r.get("resourceType").and_then(|t| t.as_str()) == Some("ConceptMap"))
        .collect();
    // A map named by `url` that came with the request is not looked up.
    let url_from_request = req
        .url
        .as_deref()
        .is_some_and(|u| tx_maps.iter().any(|m| str_field(m, &["url"]) == Some(u)));

    // `conceptMapVersion` (explicit or promoted from `url=X|V`): a version
    // that is not stored is a 404 rather than a silent answer from a
    // different version; a stored one is the map the backend translates with.
    if let (Some(url), Some(want), false) = (
        req.url.as_deref(),
        find_str_param(&params, "conceptMapVersion"),
        url_from_request,
    ) {
        let hits = ConceptMapOperations::search(
            state.backend(),
            &ctx,
            crate::types::ResourceSearchQuery {
                url: Some(url.to_string()),
                version: Some(want.clone()),
                count: Some(1),
                ..Default::default()
            },
        )
        .await?;
        if hits.is_empty() {
            return Err(HtsError::NotFound(format!(
                "ConceptMap not found: {url}|{want}"
            )));
        }
    }

    // The lookup side: the source concept, or for a reverse translation the
    // target concept (`reverse=true` names it with `code`/`system`, the R5
    // form with `targetCode`/`targetSystem`); `other` restricts the far side.
    let (lookup_system, lookup_code, other_system) = if req.target_code.is_some() {
        (
            req.target_system.clone(),
            req.target_code.clone().unwrap_or_default(),
            req.system.clone(),
        )
    } else {
        (
            req.system.clone(),
            req.code.clone(),
            req.target_system.clone(),
        )
    };
    let source_vs = req.source.clone();
    let target_vs = req.target.clone();
    let url = req.url.clone();
    let map_version = req.map_version.clone();
    let resp = if url_from_request {
        crate::types::TranslateResponse {
            result: false,
            message: None,
            matches: Vec::new(),
        }
    } else {
        ConceptMapOperations::translate(state.backend(), &ctx, req).await?
    };

    // ── Candidate maps ─────────────────────────────────────────────────────
    // The element index (the backend translate above) finds the maps with a
    // row for this code; the maps of the lookup system are added so their
    // noMap / unmapped rules apply to codes they have no row for.
    let mut candidates: Vec<(String, Option<String>)> = Vec::new();
    let mut push_candidate = |u: String, v: Option<String>| {
        if !candidates.iter().any(|(cu, _)| *cu == u) {
            candidates.push((u, v));
        }
    };
    match url.as_deref() {
        Some(u) => {
            let v = map_version.clone().or_else(|| {
                resp.matches
                    .iter()
                    .find(|m| m.source.as_deref() == Some(u))
                    .and_then(|m| m.map_version.clone())
            });
            push_candidate(u.to_string(), v);
        }
        None => {
            for m in &resp.matches {
                if let Some(u) = m.source.clone() {
                    push_candidate(u, m.map_version.clone());
                }
            }
            if let Some(sys) = lookup_system.as_deref() {
                for (u, v) in ConceptMapOperations::concept_maps_for_system(
                    state.backend(),
                    &ctx,
                    sys,
                    is_reverse,
                )
                .await
                .unwrap_or_default()
                {
                    push_candidate(u, v);
                }
            }
        }
    }
    // Request-supplied maps of the lookup system (or the one named by `url`).
    for m in &tx_maps {
        let Some(u) = str_field(m, &["url"]) else {
            continue;
        };
        let wanted = match url.as_deref() {
            Some(want) => want == u,
            None => map_has_system(m, lookup_system.as_deref(), is_reverse),
        };
        if wanted {
            push_candidate(
                u.to_string(),
                str_field(m, &["version"]).map(str::to_string),
            );
        }
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0));

    let query = MapQuery {
        system: lookup_system.as_deref(),
        code: &lookup_code,
        other_system: other_system.as_deref(),
        reverse: is_reverse,
        source_vs: source_vs.as_deref(),
        target_vs: target_vs.as_deref(),
        tx_maps: &tx_maps,
    };
    let mut matches: Vec<MapMatch> = Vec::new();
    let mut used_maps: Vec<String> = Vec::new();
    for (u, v) in &candidates {
        let map = load_concept_map(state, &ctx, &tx_maps, u, v.as_deref()).await;
        match map.as_ref().filter(|m| m.get("group").is_some()) {
            Some(map) => {
                evaluate_map(state, &ctx, map, &query, 0, &mut matches, &mut used_maps).await;
            }
            // No stored definition to read the rules from: the element rows.
            None => {
                for m in resp
                    .matches
                    .iter()
                    .filter(|m| m.source.as_deref() == Some(u.as_str()))
                {
                    matches.push(MapMatch {
                        concept: Some(coding(
                            &m.concept_system,
                            &m.concept_code,
                            m.concept_display.as_deref(),
                        )),
                        relationship: to_relationship(&m.equivalence),
                        equivalence: r4_equivalence_code(&m.equivalence).map(str::to_string),
                        no_map: false,
                        origin: canonical(u, m.map_version.as_deref().or(v.as_deref())),
                        source_concept: coding(
                            m.source_system
                                .as_deref()
                                .or(lookup_system.as_deref())
                                .unwrap_or_default(),
                            m.source_code.as_deref().unwrap_or(&lookup_code),
                            None,
                        ),
                        source_comment: None,
                        target_comment: None,
                    });
                }
            }
        }
    }
    // The same answer reached through two maps is one answer (the first).
    let mut seen: Vec<(Option<Value>, String, bool)> = Vec::new();
    matches.retain(|m| {
        let key = (m.concept.clone(), m.relationship.clone(), m.no_map);
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });

    // ── Build FHIR Parameters response ─────────────────────────────────────
    let mut parameter: Vec<Value> = Vec::with_capacity(matches.len() + 2);
    let result = matches
        .iter()
        .any(|m| !m.no_map && m.relationship != "not-related-to");
    for m in &matches {
        parameter.push(json!({"name": "match", "part": match_parts(m)}));
    }
    parameter.push(json!({"name": "result", "valueBoolean": result}));
    if matches.is_empty() {
        let sys = lookup_system.as_deref().unwrap_or("(no system)");
        let to = other_system
            .as_deref()
            .map(|t| format!(" to {t}"))
            .unwrap_or_default();
        parameter.push(json!({
            "name": "message",
            "valueString": format!("No mapping found for the code '{lookup_code}' in {sys}{to}"),
        }));
    }
    for u in &used_maps {
        parameter.push(json!({"name": "used-conceptmap", "valueUri": u}));
    }

    Ok(json!({
        "resourceType": "Parameters",
        "parameter": parameter
    }))
}

// ── ConceptMap evaluation ─────────────────────────────────────────────────────

/// What to translate, as seen from the ConceptMap: the concept on the lookup
/// side (the source side, or the target side of a reverse translation) and
/// the restrictions on the other side.
struct MapQuery<'a> {
    system: Option<&'a str>,
    code: &'a str,
    other_system: Option<&'a str>,
    reverse: bool,
    /// `source` / `target` value sets the maps must be scoped to.
    source_vs: Option<&'a str>,
    target_vs: Option<&'a str>,
    /// ConceptMaps supplied with the request (`tx-resource`); they win over
    /// stored maps with the same url.
    tx_maps: &'a [Value],
}

/// One `match` of a `$translate` response.
struct MapMatch {
    /// The concept translated to (none for `noMap`).
    concept: Option<Value>,
    /// R5 `relationship` code.
    relationship: String,
    /// R4 `equivalence` code when the map states one (else derived).
    equivalence: Option<String>,
    no_map: bool,
    /// `originMap` canonical.
    origin: String,
    source_concept: Value,
    source_comment: Option<String>,
    target_comment: Option<String>,
}

fn canonical(url: &str, version: Option<&str>) -> String {
    match version.filter(|v| !v.is_empty()) {
        Some(v) => format!("{url}|{v}"),
        None => url.to_string(),
    }
}

fn coding(system: &str, code: &str, display: Option<&str>) -> Value {
    let mut c = serde_json::Map::new();
    c.insert("system".into(), json!(system));
    c.insert("code".into(), json!(code));
    if let Some(d) = display.filter(|d| !d.is_empty()) {
        c.insert("display".into(), json!(d));
    }
    Value::Object(c)
}

/// The R5 `relationship` code for an R5 relationship or R4 equivalence code.
fn to_relationship(code: &str) -> String {
    match code {
        "equal" | "equivalent" => "equivalent",
        "wider" | "subsumes" => "source-is-narrower-than-target",
        "narrower" | "specializes" => "source-is-broader-than-target",
        "relatedto" | "inexact" => "related-to",
        "disjoint" | "unmatched" => "not-related-to",
        other => other,
    }
    .to_string()
}

/// `code` when it is an R4 `ConceptMapEquivalence` code.
fn r4_equivalence_code(code: &str) -> Option<&str> {
    matches!(
        code,
        "relatedto"
            | "equivalent"
            | "equal"
            | "wider"
            | "subsumes"
            | "narrower"
            | "specializes"
            | "inexact"
            | "unmatched"
            | "disjoint"
    )
    .then_some(code)
}

/// The R4 `equivalence` code for an R5 relationship.
fn relationship_to_equivalence(rel: &str) -> &'static str {
    match rel {
        "equivalent" => "equivalent",
        "source-is-narrower-than-target" => "wider",
        "source-is-broader-than-target" => "narrower",
        "not-related-to" => "disjoint",
        _ => "relatedto",
    }
}

fn match_parts(m: &MapMatch) -> Vec<Value> {
    let mut parts: Vec<Value> = Vec::new();
    if m.no_map {
        parts.push(json!({"name": "noMap", "valueBoolean": true}));
    } else {
        if let Some(c) = &m.concept {
            parts.push(json!({"name": "concept", "valueCoding": c}));
        }
        #[cfg(any(feature = "R5", feature = "R6"))]
        parts.push(json!({"name": "relationship", "valueCode": m.relationship}));
        #[cfg(not(any(feature = "R5", feature = "R6")))]
        {
            let eq = m
                .equivalence
                .clone()
                .unwrap_or_else(|| relationship_to_equivalence(&m.relationship).to_string());
            parts.push(json!({"name": "equivalence", "valueCode": eq}));
        }
    }
    parts.push(json!({"name": "originMap", "valueCanonical": m.origin}));
    // R4 names the map a mapping comes from `source`.
    #[cfg(not(any(feature = "R5", feature = "R6")))]
    parts.push(json!({"name": "source", "valueCanonical": m.origin}));
    if let Some(c) = &m.source_comment {
        parts.push(json!({"name": "sourceComment", "valueString": c}));
    }
    parts.push(json!({"name": "sourceConcept", "valueCoding": m.source_concept}));
    if let Some(c) = &m.target_comment {
        parts.push(json!({"name": "targetComment", "valueString": c}));
    }
    parts
}

/// Whether a ConceptMap has a group mapping from `system` (into it when
/// `reverse`); any map when `system` is unknown.
fn map_has_system(map: &Value, system: Option<&str>, reverse: bool) -> bool {
    let Some(system) = system else {
        return true;
    };
    let side = if reverse { "target" } else { "source" };
    map.get("group")
        .and_then(|g| g.as_array())
        .is_some_and(|groups| {
            groups
                .iter()
                .any(|g| str_field(g, &[side]).map(bare) == Some(system))
        })
}

/// The ConceptMap `url` (`version`, or the default one): a request-supplied
/// one, else the stored one.
async fn load_concept_map<B: TerminologyBackend>(
    state: &AppState<B>,
    ctx: &TenantContext,
    tx_maps: &[Value],
    url: &str,
    version: Option<&str>,
) -> Option<Value> {
    if let Some(m) = tx_maps.iter().find(|m| {
        str_field(m, &["url"]) == Some(url)
            && version.is_none_or(|v| str_field(m, &["version"]) == Some(v))
    }) {
        return Some(m.clone());
    }
    let mut hits = ConceptMapOperations::search(
        state.backend(),
        ctx,
        crate::types::ResourceSearchQuery {
            url: Some(url.to_string()),
            version: version.map(str::to_string),
            count: Some(1),
            ..Default::default()
        },
    )
    .await
    .ok()?;
    hits.pop()
}

fn str_field<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(|x| x.as_str()))
        .filter(|s| !s.is_empty())
}

/// `url` of a canonical `url|version`.
fn bare(canonical: &str) -> &str {
    canonical.split('|').next().unwrap_or(canonical)
}

/// The comment on a ConceptMap element (R6 `comment`, or its cross-version
/// extension in R4/R5).
fn element_comment(el: &Value) -> Option<String> {
    if let Some(c) = str_field(el, &["comment"]) {
        return Some(c.to_string());
    }
    el.get("extension")
        .and_then(|e| e.as_array())?
        .iter()
        .find(|e| {
            e.get("url")
                .and_then(|u| u.as_str())
                .is_some_and(|u| u.ends_with("extension-ConceptMap.group.element.comment"))
        })
        .and_then(|e| e.get("valueString").and_then(|v| v.as_str()))
        .map(str::to_string)
}

/// Whether the map applies to the lookup concept: a map is limited to its
/// scope (`sourceScope` — R4 `source` — or, for a reverse translation,
/// `targetScope`), which also has to be the value set the caller named.
async fn map_in_scope<B: TerminologyBackend>(
    state: &AppState<B>,
    ctx: &TenantContext,
    map: &Value,
    q: &MapQuery<'_>,
) -> bool {
    let source_scope = str_field(
        map,
        &[
            "sourceScopeUri",
            "sourceScopeCanonical",
            "sourceUri",
            "sourceCanonical",
        ],
    );
    let target_scope = str_field(
        map,
        &[
            "targetScopeUri",
            "targetScopeCanonical",
            "targetUri",
            "targetCanonical",
        ],
    );
    for (wanted, scope) in [(q.source_vs, source_scope), (q.target_vs, target_scope)] {
        if let (Some(w), Some(s)) = (wanted, scope) {
            if bare(w) != bare(s) {
                return false;
            }
        }
    }
    let scope = if q.reverse {
        target_scope
    } else {
        source_scope
    };
    let (Some(scope), Some(system)) = (scope, q.system) else {
        return true;
    };
    let scope = bare(scope);
    // A scope value set this server does not have cannot be checked.
    let known = crate::traits::ValueSetOperations::search(
        state.backend(),
        ctx,
        crate::types::ResourceSearchQuery {
            url: Some(scope.to_string()),
            count: Some(1),
            ..Default::default()
        },
    )
    .await
    .map(|h| !h.is_empty())
    .unwrap_or(false);
    if !known {
        return true;
    }
    let req = crate::types::ValidateCodeRequest {
        url: Some(scope.to_string()),
        value_set_version: None,
        system: Some(system.to_string()),
        code: q.code.to_string(),
        version: None,
        display: None,
        date: None,
        include_abstract: None,
        input_form: None,
        lenient_display_validation: None,
        default_value_set_versions: std::collections::HashMap::new(),
    };
    crate::traits::ValueSetOperations::validate_code(state.backend(), ctx, req)
        .await
        .map(|r| r.result)
        .unwrap_or(true)
}

/// Translate with one ConceptMap's own definition: its elements, `noMap`
/// and `group.unmapped` rules (following `other-map` up to a few maps deep,
/// the matches credited to `map` and the other map reported as used).
fn evaluate_map<'a, B: TerminologyBackend>(
    state: &'a AppState<B>,
    ctx: &'a TenantContext,
    map: &'a Value,
    q: &'a MapQuery<'a>,
    depth: u8,
    out: &'a mut Vec<MapMatch>,
    used_maps: &'a mut Vec<String>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        if depth > 4 || !map_in_scope(state, ctx, map, q).await {
            return;
        }
        let url = str_field(map, &["url"]).unwrap_or_default();
        let origin = canonical(url, str_field(map, &["version"]));
        let Some(groups) = map.get("group").and_then(|g| g.as_array()) else {
            return;
        };
        for group in groups {
            let g_source = str_field(group, &["source"]).map(bare);
            let g_target = str_field(group, &["target"]).map(bare);
            let (lookup_side, other_side) = if q.reverse {
                (g_target, g_source)
            } else {
                (g_source, g_target)
            };
            if q.system.is_some_and(|s| lookup_side != Some(s))
                || q.other_system.is_some_and(|s| other_side != Some(s))
            {
                continue;
            }
            let g_source = g_source.unwrap_or_default();
            let g_target = g_target.unwrap_or_default();
            let elements: Vec<&Value> = group
                .get("element")
                .and_then(|e| e.as_array())
                .map(|a| a.iter().collect())
                .unwrap_or_default();
            let mut found = false;
            for el in &elements {
                let el_code = str_field(el, &["code"]).unwrap_or_default();
                let targets: Vec<&Value> = el
                    .get("target")
                    .and_then(|t| t.as_array())
                    .map(|a| a.iter().collect())
                    .unwrap_or_default();
                if q.reverse {
                    for t in &targets {
                        if str_field(t, &["code"]) != Some(q.code) {
                            continue;
                        }
                        found = true;
                        out.push(target_match(
                            t,
                            g_target,
                            &origin,
                            coding(g_source, el_code, str_field(el, &["display"])),
                            None,
                        ));
                    }
                    continue;
                }
                if el_code != q.code {
                    continue;
                }
                found = true;
                let source_concept = coding(g_source, el_code, None);
                let source_comment = element_comment(el);
                // R5 `noMap`; R4 states it as a codeless `unmatched` target.
                let no_map = el.get("noMap").and_then(|v| v.as_bool()) == Some(true)
                    || (!targets.is_empty()
                        && targets.iter().all(|t| {
                            str_field(t, &["code"]).is_none()
                                && str_field(t, &["equivalence"]) == Some("unmatched")
                        }));
                if no_map {
                    out.push(MapMatch {
                        concept: None,
                        relationship: "not-related-to".into(),
                        equivalence: None,
                        no_map: true,
                        origin: origin.clone(),
                        source_concept,
                        source_comment,
                        target_comment: None,
                    });
                    continue;
                }
                for t in targets.iter().filter(|t| str_field(t, &["code"]).is_some()) {
                    out.push(target_match(
                        t,
                        g_target,
                        &origin,
                        source_concept.clone(),
                        source_comment.clone(),
                    ));
                }
            }
            if found || q.reverse {
                continue;
            }
            // No element for the code: the group's `unmapped` rule.
            let Some(unmapped) = group.get("unmapped") else {
                continue;
            };
            let source_concept = coding(g_source, q.code, None);
            let relationship = str_field(unmapped, &["relationship", "equivalence"])
                .map(to_relationship)
                .unwrap_or_else(|| "related-to".into());
            let fixed = |code: &str, display: Option<&str>| MapMatch {
                concept: Some(coding(g_target, code, display)),
                relationship: relationship.clone(),
                equivalence: str_field(unmapped, &["equivalence"])
                    .and_then(r4_equivalence_code)
                    .map(str::to_string),
                no_map: false,
                origin: origin.clone(),
                source_concept: source_concept.clone(),
                source_comment: None,
                target_comment: None,
            };
            match str_field(unmapped, &["mode"]) {
                Some("fixed") => {
                    if let Some(code) = str_field(unmapped, &["code"]) {
                        out.push(fixed(code, str_field(unmapped, &["display"])));
                    }
                }
                Some("provided") | Some("use-source-code") => out.push(fixed(q.code, None)),
                Some("other-map") => {
                    let Some(other) = str_field(unmapped, &["otherMap", "url"]) else {
                        continue;
                    };
                    let (other_url, other_ver) = match other.split_once('|') {
                        Some((u, v)) => (u, Some(v)),
                        None => (other, None),
                    };
                    let Some(other_map) =
                        load_concept_map(state, ctx, q.tx_maps, other_url, other_ver).await
                    else {
                        continue;
                    };
                    let used = canonical(other_url, str_field(&other_map, &["version"]));
                    if !used_maps.contains(&used) {
                        used_maps.push(used);
                    }
                    let mut via: Vec<MapMatch> = Vec::new();
                    evaluate_map(state, ctx, &other_map, q, depth + 1, &mut via, used_maps).await;
                    for mut m in via {
                        m.origin = origin.clone();
                        out.push(m);
                    }
                }
                _ => {}
            }
        }
    })
}

/// A match on one `element.target`.
fn target_match(
    t: &Value,
    g_target: &str,
    origin: &str,
    source_concept: Value,
    source_comment: Option<String>,
) -> MapMatch {
    let raw = str_field(t, &["relationship", "equivalence"]).unwrap_or("equivalent");
    MapMatch {
        concept: Some(coding(
            g_target,
            str_field(t, &["code"]).unwrap_or_default(),
            str_field(t, &["display"]),
        )),
        relationship: to_relationship(raw),
        equivalence: str_field(t, &["equivalence"])
            .and_then(r4_equivalence_code)
            .map(str::to_string),
        no_map: false,
        origin: origin.to_string(),
        source_concept,
        source_comment,
        target_comment: str_field(t, &["comment"]).map(str::to_string),
    }
}

/// POST /ConceptMap/$translate
pub async fn translate_handler<B: TerminologyBackend>(
    State(state): State<AppState<B>>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, HtsError> {
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    let format = negotiate_format(raw.as_deref(), accept);
    let params = extract_parameter_array(&body)?;
    Ok(fhir_respond(
        dispatch_tx!(state, params, process_translate)?,
        format,
    ))
}

/// GET /ConceptMap/$translate?code=...&system=...
pub async fn get_translate_handler<B: TerminologyBackend>(
    State(state): State<AppState<B>>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Response, HtsError> {
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    let format = negotiate_format(raw.as_deref(), accept);
    let pairs = parse_query_string(raw.as_deref().unwrap_or(""));
    let params = query_params_to_fhir_params(pairs);
    Ok(fhir_respond(
        process_translate(&state, params).await?,
        format,
    ))
}

// ── Instance-level: /ConceptMap/{id}/$translate ───────────────────────────────

/// Inject (or replace) the `url` parameter in a params list.
fn inject_url(mut params: Vec<Value>, url: String) -> Vec<Value> {
    params.retain(|p| p.get("name").and_then(|v| v.as_str()) != Some("url"));
    let mut with_url = vec![json!({"name": "url", "valueUri": url})];
    with_url.append(&mut params);
    with_url
}

/// POST /ConceptMap/{id}/$translate
///
/// Resolves the ConceptMap canonical URL from its FHIR `id`, then delegates to
/// the same translate logic used by the system-level endpoint.
pub async fn translate_by_id_post<B: TerminologyBackend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Result<Response, HtsError> {
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    let format = negotiate_format(raw.as_deref(), accept);
    let url = state
        .backend()
        .resource_url_by_id("ConceptMap", &id)
        .ok_or_else(|| HtsError::NotFound(format!("ConceptMap/{id}")))?;

    let raw_params = body
        .and_then(|Json(v)| extract_parameter_array(&v).ok())
        .unwrap_or_default();
    Ok(fhir_respond(
        process_translate(&state, inject_url(raw_params, url)).await?,
        format,
    ))
}

/// GET /ConceptMap/{id}/$translate?code=...&system=...
pub async fn get_translate_by_id<B: TerminologyBackend>(
    State(state): State<AppState<B>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Response, HtsError> {
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    let format = negotiate_format(raw.as_deref(), accept);
    let url = state
        .backend()
        .resource_url_by_id("ConceptMap", &id)
        .ok_or_else(|| HtsError::NotFound(format!("ConceptMap/{id}")))?;

    let pairs = parse_query_string(raw.as_deref().unwrap_or(""));
    let params = query_params_to_fhir_params(pairs);
    Ok(fhir_respond(
        process_translate(&state, inject_url(params, url)).await?,
        format,
    ))
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, http::Request, routing::post};
    use tower::ServiceExt;

    use crate::backends::sqlite::SqliteTerminologyBackend;
    use crate::state::AppState;

    fn make_app() -> Router {
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        {
            let conn = backend.pool().get().unwrap();
            conn.execute_batch(
                "INSERT INTO code_systems
                     (id, url, version, name, status, content, created_at, updated_at)
                 VALUES ('cs-src', 'http://example.org/src', '1.0', 'Source CS',
                         'active', 'complete', '2024-01-01', '2024-01-01'),
                        ('cs-tgt', 'http://example.org/tgt', '1.0', 'Target CS',
                         'active', 'complete', '2024-01-01', '2024-01-01');

                 INSERT INTO concepts (id, system_id, code, display)
                 VALUES (1, 'cs-src', 'A', 'Alpha'),
                        (2, 'cs-src', 'B', 'Beta'),
                        (10, 'cs-tgt', 'X', 'X-Ray'),
                        (11, 'cs-tgt', 'Y', 'Yankee');

                 INSERT INTO concept_maps
                     (id, url, version, source_uri, target_uri, status, created_at)
                 VALUES ('cm1', 'http://example.org/cm', '1.0',
                         'http://example.org/src', 'http://example.org/tgt',
                         'active', '2024-01-01');

                 INSERT INTO concept_map_elements
                     (map_id, source_system, source_code, target_system, target_code, equivalence)
                 VALUES ('cm1', 'http://example.org/src', 'A',
                         'http://example.org/tgt', 'X', 'equivalent'),
                        ('cm1', 'http://example.org/src', 'B',
                         'http://example.org/tgt', 'Y', 'wider');",
            )
            .unwrap();
        }
        let state = AppState::new(backend);
        Router::new()
            .route(
                "/ConceptMap/$translate",
                post(translate_handler::<SqliteTerminologyBackend>),
            )
            .with_state(state)
    }

    async fn post_json(app: Router, uri: &str, body: Value) -> axum::response::Response {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    async fn body_json(response: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // ── Happy path ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn translate_returns_parameters_resource() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "code", "valueCode": "A"},
                {"name": "system", "valueUri": "http://example.org/src"}
            ]
        });

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        assert_eq!(resp.status(), 200);

        let json = body_json(resp).await;
        assert_eq!(json["resourceType"], "Parameters");
    }

    #[tokio::test]
    async fn translate_result_true_on_match() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "code", "valueCode": "A"},
                {"name": "system", "valueUri": "http://example.org/src"}
            ]
        });

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        let json = body_json(resp).await;

        let params = json["parameter"].as_array().unwrap();
        let result_param = params.iter().find(|p| p["name"] == "result").unwrap();
        assert_eq!(result_param["valueBoolean"], true);
    }

    #[tokio::test]
    async fn translate_match_contains_concept_coding() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "code", "valueCode": "A"},
                {"name": "system", "valueUri": "http://example.org/src"}
            ]
        });

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        let json = body_json(resp).await;

        let params = json["parameter"].as_array().unwrap();
        let match_param = params.iter().find(|p| p["name"] == "match").unwrap();
        let parts = match_param["part"].as_array().unwrap();

        let concept = parts.iter().find(|p| p["name"] == "concept").unwrap();
        assert_eq!(concept["valueCoding"]["code"], "X");
        assert_eq!(concept["valueCoding"]["system"], "http://example.org/tgt");
        // The IG translate fixtures expect bare {system, code} Codings —
        // display is intentionally omitted from the output.
        assert!(concept["valueCoding"].get("display").is_none());
    }

    #[tokio::test]
    async fn translate_match_contains_equivalence() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "code", "valueCode": "A"},
                {"name": "system", "valueUri": "http://example.org/src"}
            ]
        });

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        let json = body_json(resp).await;

        let params = json["parameter"].as_array().unwrap();
        let match_param = params.iter().find(|p| p["name"] == "match").unwrap();
        let parts = match_param["part"].as_array().unwrap();

        // The build emits `equivalence` for R4/R4B and `relationship` for R5/R6;
        // either name carries the same valueCode.
        let key = if cfg!(any(feature = "R5", feature = "R6")) {
            "relationship"
        } else {
            "equivalence"
        };
        let equiv = parts.iter().find(|p| p["name"] == key).unwrap();
        assert_eq!(equiv["valueCode"], "equivalent");
    }

    #[tokio::test]
    async fn translate_result_false_on_no_match() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "code", "valueCode": "UNKNOWN"}
            ]
        });

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        assert_eq!(resp.status(), 200);

        let json = body_json(resp).await;
        let params = json["parameter"].as_array().unwrap();
        let result_param = params.iter().find(|p| p["name"] == "result").unwrap();
        assert_eq!(result_param["valueBoolean"], false);
    }

    #[tokio::test]
    async fn translate_reverse_finds_source_code() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "code", "valueCode": "X"},
                {"name": "system", "valueUri": "http://example.org/tgt"},
                {"name": "reverse", "valueBoolean": true}
            ]
        });

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        assert_eq!(resp.status(), 200);

        let json = body_json(resp).await;
        let params = json["parameter"].as_array().unwrap();
        let result_param = params.iter().find(|p| p["name"] == "result").unwrap();
        assert_eq!(result_param["valueBoolean"], true);

        let match_param = params.iter().find(|p| p["name"] == "match").unwrap();
        let parts = match_param["part"].as_array().unwrap();
        // Reverse output: `concept` carries the supplied target Coding (X in tgt CS);
        // `source` carries the resolved source Coding (A in src CS).
        let concept = parts.iter().find(|p| p["name"] == "concept").unwrap();
        assert_eq!(concept["valueCoding"]["code"], "X");
        assert_eq!(concept["valueCoding"]["system"], "http://example.org/tgt");
        let source = parts.iter().find(|p| p["name"] == "sourceConcept").unwrap();
        assert_eq!(source["valueCoding"]["code"], "A");
        assert_eq!(source["valueCoding"]["system"], "http://example.org/src");
    }

    // ── Coding / CodeableConcept source spellings (#287) ───────────────────────

    /// The spec allows the source concept as a `coding`, but the handler only read the
    /// scalar `code` + `system` pair, so this shape 400'd as "Missing required
    /// parameter". Regression test for #287.
    #[tokio::test]
    async fn translate_accepts_coding_param() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "coding", "valueCoding": {
                    "system": "http://example.org/src",
                    "code": "A"
                }},
                {"name": "targetSystem", "valueUri": "http://example.org/tgt"}
            ]
        });

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        assert_eq!(resp.status(), 200);
        let json = body_json(resp).await;
        let params = json["parameter"].as_array().unwrap();

        let result = params.iter().find(|p| p["name"] == "result").unwrap();
        assert_eq!(result["valueBoolean"], true);

        let m = params.iter().find(|p| p["name"] == "match").unwrap();
        let concept = m["part"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "concept")
            .unwrap();
        assert_eq!(concept["valueCoding"]["code"], "X");
    }

    /// R5 spelling of the same input.
    #[tokio::test]
    async fn translate_accepts_source_coding_param() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "sourceCoding", "valueCoding": {
                    "system": "http://example.org/src",
                    "code": "A"
                }},
                {"name": "targetSystem", "valueUri": "http://example.org/tgt"}
            ]
        });

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        assert_eq!(resp.status(), 200);
        let json = body_json(resp).await;
        let result = json["parameter"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "result")
            .unwrap();
        assert_eq!(result["valueBoolean"], true);
    }

    /// A `codeableConcept` translates on its first coding.
    #[tokio::test]
    async fn translate_accepts_codeable_concept_param() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "codeableConcept", "valueCodeableConcept": {
                    "coding": [{"system": "http://example.org/src", "code": "A"}]
                }},
                {"name": "targetSystem", "valueUri": "http://example.org/tgt"}
            ]
        });

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        assert_eq!(resp.status(), 200);
        let json = body_json(resp).await;
        let result = json["parameter"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "result")
            .unwrap();
        assert_eq!(result["valueBoolean"], true);
    }

    /// `targetCoding` drives reverse mode the same way a bare `targetCode` does.
    #[tokio::test]
    async fn translate_accepts_target_coding_param_for_reverse() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "sourceSystem", "valueUri": "http://example.org/src"},
                {"name": "targetCoding", "valueCoding": {
                    "system": "http://example.org/tgt",
                    "code": "X"
                }}
            ]
        });

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        assert_eq!(resp.status(), 200);
        let json = body_json(resp).await;
        let params = json["parameter"].as_array().unwrap();

        let result = params.iter().find(|p| p["name"] == "result").unwrap();
        assert_eq!(result["valueBoolean"], true);

        let m = params.iter().find(|p| p["name"] == "match").unwrap();
        let source = m["part"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "sourceConcept")
            .unwrap();
        assert_eq!(source["valueCoding"]["code"], "A");
    }

    /// An explicit `code` outranks a `coding` sent alongside it, so a request mixing the
    /// spellings translates what the caller named rather than silently preferring one.
    #[tokio::test]
    async fn translate_scalar_code_wins_over_coding() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "code",   "valueCode": "A"},
                {"name": "system", "valueUri": "http://example.org/src"},
                {"name": "coding", "valueCoding": {
                    "system": "http://example.org/src",
                    "code": "no-such-code"
                }},
                {"name": "targetSystem", "valueUri": "http://example.org/tgt"}
            ]
        });

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        assert_eq!(resp.status(), 200);
        let json = body_json(resp).await;
        let params = json["parameter"].as_array().unwrap();

        let result = params.iter().find(|p| p["name"] == "result").unwrap();
        assert_eq!(result["valueBoolean"], true);

        let m = params.iter().find(|p| p["name"] == "match").unwrap();
        let concept = m["part"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "concept")
            .unwrap();
        assert_eq!(concept["valueCoding"]["code"], "X");
    }

    // ── Error cases ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn translate_missing_code_returns_400() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "system", "valueUri": "http://example.org/src"}
            ]
        });

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        assert_eq!(resp.status(), 400);
    }

    /// When no source concept is supplied, the 400 OperationOutcome must name
    /// every accepted spelling — including the Coding/CodeableConcept ones — so a
    /// client that sent a spec-legal-but-unread shape (e.g. R5 `sourceCoding`) has
    /// a hint about what HTS accepts. Regression test for #288: the old message
    /// listed only `code`/`sourceCode`/`targetCode`. Assert substring-contains per
    /// spelling (not exact-equality) so rewording the prose doesn't spuriously
    /// break CI while still guaranteeing each alias stays discoverable.
    #[tokio::test]
    async fn translate_missing_code_diagnostics_names_accepted_spellings() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "system", "valueUri": "http://example.org/src"}
            ]
        });

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        assert_eq!(resp.status(), 400);

        let json = body_json(resp).await;
        assert_eq!(json["resourceType"], "OperationOutcome");
        assert_eq!(json["issue"][0]["code"], "invalid");
        let diag = json["issue"][0]["details"]["text"]
            .as_str()
            .expect("details.text string");
        for spelling in [
            "sourceCoding",
            "coding",
            "sourceCodeableConcept",
            "codeableConcept",
            "sourceCode",
            "targetCoding",
            "targetCode",
        ] {
            assert!(
                diag.contains(spelling),
                "diagnostics must name the accepted `{spelling}` spelling, got: {diag}"
            );
        }
    }

    #[tokio::test]
    async fn translate_wrong_resource_type_returns_400() {
        let app = make_app();
        let body = json!({"resourceType": "ConceptMap", "parameter": []});

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        assert_eq!(resp.status(), 400);
    }

    // ── R5 parameter names + no-URL translation (tx-ecosystem IG) ──────────────

    /// `sourceCode` + `sourceSystem` + `targetSystem` (no `url`) — R5 names.
    /// Mirrors the IG `translate/translate-1` fixture shape.
    #[tokio::test]
    async fn translate_r5_param_names_without_url_finds_match() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "sourceSystem", "valueUri": "http://example.org/src"},
                {"name": "sourceCode",   "valueCode": "A"},
                {"name": "targetSystem", "valueUri": "http://example.org/tgt"}
            ]
        });

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        assert_eq!(resp.status(), 200);
        let json = body_json(resp).await;
        let params = json["parameter"].as_array().unwrap();

        let result = params.iter().find(|p| p["name"] == "result").unwrap();
        assert_eq!(result["valueBoolean"], true);

        let m = params.iter().find(|p| p["name"] == "match").unwrap();
        let parts = m["part"].as_array().unwrap();
        let concept = parts.iter().find(|p| p["name"] == "concept").unwrap();
        assert_eq!(concept["valueCoding"]["code"], "X");
        assert_eq!(concept["valueCoding"]["system"], "http://example.org/tgt");
    }

    /// Reverse mode driven by `targetCode` + `sourceSystem` (no `reverse=true`).
    /// Mirrors `translate/translate-reverse`.
    #[tokio::test]
    async fn translate_reverse_via_target_code_emits_source_coding() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "sourceSystem", "valueUri": "http://example.org/src"},
                {"name": "targetCode",   "valueCode": "X"},
                {"name": "targetSystem", "valueUri": "http://example.org/tgt"}
            ]
        });

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        assert_eq!(resp.status(), 200);
        let json = body_json(resp).await;
        let params = json["parameter"].as_array().unwrap();

        let m = params.iter().find(|p| p["name"] == "match").unwrap();
        let parts = m["part"].as_array().unwrap();

        // Reverse output: `concept` carries the *target* side of the
        // matched element (i.e. the supplied targetCode), and `source`
        // carries the *source* side (the resolved code). This matches
        // the IG `translate/translate-reverse` fixture exactly.
        let concept = parts.iter().find(|p| p["name"] == "concept").unwrap();
        assert_eq!(concept["valueCoding"]["code"], "X");
        assert_eq!(concept["valueCoding"]["system"], "http://example.org/tgt");

        let source = parts.iter().find(|p| p["name"] == "sourceConcept").unwrap();
        assert_eq!(source["valueCoding"]["code"], "A");
        assert_eq!(source["valueCoding"]["system"], "http://example.org/src");
    }

    /// IG `translate/translate-reverse` fixture pins the part ordering. The
    /// validator's TxTesterSorters alphabetises before comparison, so we just
    /// need the right SET of parts (one of equivalence/relationship per the
    /// build's FHIR version). originMap is suppressed in reverse mode.
    #[tokio::test]
    async fn translate_reverse_part_ordering_matches_ig_fixture() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "sourceSystem", "valueUri": "http://example.org/src"},
                {"name": "targetCode",   "valueCode": "X"},
                {"name": "targetSystem", "valueUri": "http://example.org/tgt"}
            ]
        });
        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        assert_eq!(resp.status(), 200);
        let json = body_json(resp).await;
        let params = json["parameter"].as_array().unwrap();
        let m = params.iter().find(|p| p["name"] == "match").unwrap();
        let parts = m["part"].as_array().unwrap();

        let names: Vec<&str> = parts.iter().filter_map(|p| p["name"].as_str()).collect();
        let equiv_or_rel = if cfg!(any(feature = "R5", feature = "R6")) {
            "relationship"
        } else {
            "equivalence"
        };
        let expected: Vec<&str> = if cfg!(any(feature = "R5", feature = "R6")) {
            vec!["concept", equiv_or_rel, "originMap", "sourceConcept"]
        } else {
            vec![
                "concept",
                equiv_or_rel,
                "originMap",
                "source",
                "sourceConcept",
            ]
        };
        assert_eq!(
            names, expected,
            "reverse-mode parts must be concept/<equivalence|relationship>/originMap/[source]/sourceConcept"
        );
    }

    /// `originMap` is emitted as `url|version` when the ConceptMap has a version.
    #[tokio::test]
    async fn translate_emits_origin_map_canonical_with_version() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "sourceSystem", "valueUri": "http://example.org/src"},
                {"name": "sourceCode",   "valueCode": "A"}
            ]
        });

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        let json = body_json(resp).await;
        let params = json["parameter"].as_array().unwrap();

        let m = params.iter().find(|p| p["name"] == "match").unwrap();
        let parts = m["part"].as_array().unwrap();
        let origin = parts.iter().find(|p| p["name"] == "originMap").unwrap();
        assert_eq!(origin["valueCanonical"], "http://example.org/cm|1.0");
    }

    /// Forward translation emits the version-appropriate name only —
    /// `equivalence` in R4/R4B, `relationship` in R5/R6 — so the validator's
    /// TxTesterSorters-alphabetised actual matches the version-filtered
    /// expected at every position.
    #[tokio::test]
    async fn translate_emits_version_appropriate_relationship_name() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "sourceSystem", "valueUri": "http://example.org/src"},
                {"name": "sourceCode",   "valueCode": "A"}
            ]
        });

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        let json = body_json(resp).await;
        let parts = json["parameter"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "match")
            .unwrap()["part"]
            .as_array()
            .unwrap()
            .clone();

        if cfg!(any(feature = "R5", feature = "R6")) {
            let rel = parts.iter().find(|p| p["name"] == "relationship").unwrap();
            assert_eq!(rel["valueCode"], "equivalent");
            assert!(parts.iter().all(|p| p["name"] != "equivalence"));
        } else {
            let equiv = parts.iter().find(|p| p["name"] == "equivalence").unwrap();
            assert_eq!(equiv["valueCode"], "equivalent");
            assert!(parts.iter().all(|p| p["name"] != "relationship"));
        }
    }

    /// Forward responses do *not* include a `source` Coding — the caller
    /// already knows the source code they sent.
    #[tokio::test]
    async fn translate_forward_omits_source_coding_part() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "sourceSystem", "valueUri": "http://example.org/src"},
                {"name": "sourceCode",   "valueCode": "A"}
            ]
        });

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        let json = body_json(resp).await;
        let parts = json["parameter"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "match")
            .unwrap()["part"]
            .as_array()
            .unwrap()
            .clone();
        // `source` (R4) is the canonical of the map, never a Coding.
        assert!(
            parts
                .iter()
                .filter(|p| p["name"] == "source")
                .all(|p| p.get("valueCoding").is_none() && p.get("valueCanonical").is_some()),
            "`source` must be the map canonical"
        );
    }

    /// Neither `code` nor `targetCode` → 400.
    #[tokio::test]
    async fn translate_missing_both_code_and_target_code_returns_400() {
        let app = make_app();
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "sourceSystem", "valueUri": "http://example.org/src"},
                {"name": "targetSystem", "valueUri": "http://example.org/tgt"}
            ]
        });

        let resp = post_json(app, "/ConceptMap/$translate", body).await;
        assert_eq!(resp.status(), 400);
    }
}
