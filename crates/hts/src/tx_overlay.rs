//! Request-scoped terminology for `tx-resource` parameters.
//!
//! A client may send the CodeSystems, ValueSets and ConceptMaps an operation
//! needs along with the request (`tx-resource`), instead of relying on the
//! server to hold them: the HL7 validator and the IG Publisher do this for
//! the content of the IG being validated, and the HL7 tx-ecosystem test
//! runner sends every suite's setup resources this way.
//!
//! The backends answer from their stores, so a request whose `tx-resource`s
//! are not all held by the store, or are held with different content, is
//! answered from an *overlay*: an in-memory SQLite terminology store holding
//! those resources plus copies of the stored resources they (and the request)
//! depend on. Overlays are cached by their content and dropped whenever the
//! main store changes. When a dependency cannot be copied — a large
//! terminology imported from its distribution files (SNOMED CT, LOINC, …)
//! whose stored resource carries no concepts — the request is answered from
//! the main store as before.
//!
//! The overlay never writes to the main store: what one client sends is
//! never visible to another.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use helios_persistence::tenant::TenantContext;
use serde_json::{Value, json};

use crate::backends::SqliteTerminologyBackend;
use crate::error::HtsError;
use crate::import::BundleImportBackend;
use crate::state::AppState;
use crate::traits::{CodeSystemOperations, TerminologyBackend, ValueSetOperations};

/// Most overlays kept at once (least recently built evicted first).
const OVERLAY_CACHE_MAX: usize = 32;
/// Most concepts copied from the main store into one overlay.
const MAX_COPIED_CONCEPTS: usize = 100_000;
/// How deep ValueSet → ValueSet references are followed for dependencies.
const MAX_DEPTH: usize = 8;

/// Cache of overlays by content key; `None` remembers "answer from the main
/// store" for a key.
#[derive(Default)]
pub struct TxOverlayCache {
    inner: Mutex<(
        HashMap<u64, Option<Arc<AppState<SqliteTerminologyBackend>>>>,
        VecDeque<u64>,
    )>,
}

impl TxOverlayCache {
    /// Drop every overlay (the main store changed).
    pub fn clear(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.0.clear();
            g.1.clear();
        }
    }

    fn get(&self, key: u64) -> Option<Option<Arc<AppState<SqliteTerminologyBackend>>>> {
        self.inner.lock().ok()?.0.get(&key).cloned()
    }

    fn put(&self, key: u64, value: Option<Arc<AppState<SqliteTerminologyBackend>>>) {
        if let Ok(mut g) = self.inner.lock() {
            if g.0.insert(key, value).is_none() {
                g.1.push_back(key);
            }
            while g.1.len() > OVERLAY_CACHE_MAX {
                if let Some(old) = g.1.pop_front() {
                    g.0.remove(&old);
                }
            }
        }
    }
}

fn tx_resources(params: &[Value]) -> Vec<&Value> {
    params
        .iter()
        .filter(|p| p.get("name").and_then(Value::as_str) == Some("tx-resource"))
        .filter_map(|p| p.get("resource"))
        .filter(|r| {
            matches!(
                r.get("resourceType").and_then(Value::as_str),
                Some("CodeSystem" | "ValueSet" | "ConceptMap")
            )
        })
        .collect()
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// `url` of a canonical `url|version`.
fn bare(canonical: &str) -> &str {
    canonical.split('|').next().unwrap_or(canonical)
}

/// A resource without the parts a store adds or rewrites on import.
fn comparable(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(o) = v.as_object_mut() {
        for k in ["meta", "text", "id"] {
            o.remove(k);
        }
    }
    v
}

/// The canonicals a request names directly: code systems (`system`, Coding
/// systems, CodeSystem `url`, `useSupplement`, version pins) and value sets
/// (`url`, the compose of an inline `valueSet`).
fn request_canonicals(params: &[Value], out: &mut BTreeSet<String>) {
    fn coding_systems(v: &Value, out: &mut BTreeSet<String>) {
        if let Some(s) = str_of(v, "system") {
            out.insert(bare(s).to_string());
        }
        for c in v
            .get("coding")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(s) = str_of(c, "system") {
                out.insert(bare(s).to_string());
            }
        }
    }
    for p in params {
        let name = p.get("name").and_then(Value::as_str).unwrap_or_default();
        if name == "tx-resource" {
            continue;
        }
        for key in ["valueUri", "valueCanonical", "valueString", "valueUrl"] {
            if let Some(s) = str_of(p, key) {
                if s.contains("://") || s.starts_with("urn:") {
                    out.insert(bare(s).to_string());
                }
            }
        }
        for key in ["valueCoding", "valueCodeableConcept"] {
            if let Some(v) = p.get(key) {
                coding_systems(v, out);
            }
        }
        if let Some(r) = p.get("resource") {
            compose_canonicals(r, out);
        }
        for part in p
            .get("part")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(v) = part.get("valueCoding") {
                coding_systems(v, out);
            }
        }
    }
}

/// The code systems and value sets a ValueSet's compose (and its contained
/// value sets) refers to; a CodeSystem supplement's base.
fn compose_canonicals(r: &Value, out: &mut BTreeSet<String>) {
    if let Some(s) = str_of(r, "supplements") {
        out.insert(bare(s).to_string());
    }
    for c in r
        .get("contained")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        compose_canonicals(c, out);
    }
    // A ConceptMap: its scopes, the code systems it maps between, and the
    // maps its `unmapped` rules defer to.
    if r.get("resourceType").and_then(Value::as_str) == Some("ConceptMap") {
        for key in [
            "sourceScopeUri",
            "sourceScopeCanonical",
            "targetScopeUri",
            "targetScopeCanonical",
            "sourceUri",
            "sourceCanonical",
            "targetUri",
            "targetCanonical",
        ] {
            if let Some(s) = str_of(r, key) {
                out.insert(bare(s).to_string());
            }
        }
        for g in r
            .get("group")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for key in ["source", "target"] {
                if let Some(s) = str_of(g, key) {
                    out.insert(bare(s).to_string());
                }
            }
            if let Some(u) = g.get("unmapped") {
                for key in ["otherMap", "url", "valueSet"] {
                    if let Some(s) = str_of(u, key) {
                        out.insert(bare(s).to_string());
                    }
                }
            }
        }
        return;
    }
    let Some(compose) = r.get("compose") else {
        return;
    };
    for key in ["include", "exclude"] {
        for inc in compose
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(s) = str_of(inc, "system") {
                out.insert(bare(s).to_string());
            }
            for vs in inc
                .get("valueSet")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(s) = vs.as_str().filter(|s| !s.starts_with('#')) {
                    out.insert(bare(s).to_string());
                }
            }
        }
    }
}

async fn stored_code_systems<B: TerminologyBackend>(
    backend: &B,
    ctx: &TenantContext,
    url: &str,
) -> Vec<Value> {
    CodeSystemOperations::search(
        backend,
        ctx,
        crate::types::ResourceSearchQuery {
            url: Some(url.to_string()),
            count: Some(50),
            ..Default::default()
        },
    )
    .await
    .unwrap_or_default()
}

async fn stored_value_sets<B: TerminologyBackend>(
    backend: &B,
    ctx: &TenantContext,
    url: &str,
) -> Vec<Value> {
    ValueSetOperations::search(
        backend,
        ctx,
        crate::types::ResourceSearchQuery {
            url: Some(url.to_string()),
            count: Some(50),
            ..Default::default()
        },
    )
    .await
    .unwrap_or_default()
}

async fn stored_concept_maps<B: TerminologyBackend>(
    backend: &B,
    ctx: &TenantContext,
    url: &str,
) -> Vec<Value> {
    crate::traits::ConceptMapOperations::search(
        backend,
        ctx,
        crate::types::ResourceSearchQuery {
            url: Some(url.to_string()),
            count: Some(50),
            ..Default::default()
        },
    )
    .await
    .unwrap_or_default()
}

fn concept_count(cs: &Value) -> usize {
    fn walk(list: Option<&Vec<Value>>) -> usize {
        list.into_iter()
            .flatten()
            .map(|c| 1 + walk(c.get("concept").and_then(Value::as_array)))
            .sum()
    }
    walk(cs.get("concept").and_then(Value::as_array))
}

/// Whether the store holds `r` (same url and version) with the same content.
async fn held_by_store<B: TerminologyBackend>(backend: &B, ctx: &TenantContext, r: &Value) -> bool {
    let Some(url) = str_of(r, "url") else {
        return false;
    };
    let version = str_of(r, "version");
    let query = crate::types::ResourceSearchQuery {
        url: Some(url.to_string()),
        version: version.map(str::to_string),
        count: Some(1),
        ..Default::default()
    };
    let hits = match r.get("resourceType").and_then(Value::as_str) {
        Some("CodeSystem") => CodeSystemOperations::search(backend, ctx, query).await,
        Some("ValueSet") => ValueSetOperations::search(backend, ctx, query).await,
        Some("ConceptMap") => {
            crate::traits::ConceptMapOperations::search(backend, ctx, query).await
        }
        _ => return false,
    }
    .unwrap_or_default();
    hits.first()
        .is_some_and(|stored| comparable(stored) == comparable(r))
}

/// The overlay to answer `params` from, or `None` for the main store.
pub async fn overlay_for<B: TerminologyBackend>(
    state: &AppState<B>,
    params: &[Value],
) -> Option<Arc<AppState<SqliteTerminologyBackend>>> {
    if state.is_tx_overlay {
        return None;
    }
    let supplied = tx_resources(params);
    if supplied.is_empty() {
        return None;
    }
    let mut direct: BTreeSet<String> = BTreeSet::new();
    request_canonicals(params, &mut direct);
    let key = {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for r in &supplied {
            r.to_string().hash(&mut h);
        }
        direct.hash(&mut h);
        h.finish()
    };
    if let Some(cached) = state.tx_overlays.get(key) {
        return cached;
    }
    let overlay = match build_overlay(state, &supplied, direct).await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, "tx-resource overlay could not be built; using the main store");
            None
        }
    };
    state.tx_overlays.put(key, overlay.clone());
    overlay
}

async fn build_overlay<B: TerminologyBackend>(
    state: &AppState<B>,
    supplied: &[&Value],
    direct: BTreeSet<String>,
) -> Result<Option<Arc<AppState<SqliteTerminologyBackend>>>, HtsError> {
    let ctx = TenantContext::system();
    let backend = state.backend();

    // Nothing to do when the store already holds every supplied resource.
    let mut all_held = true;
    for r in supplied {
        if !held_by_store(backend, &ctx, r).await {
            all_held = false;
            break;
        }
    }
    if all_held {
        return Ok(None);
    }

    let supplied_urls: BTreeSet<String> = supplied
        .iter()
        .filter_map(|r| str_of(r, "url").map(str::to_string))
        .collect();
    let mut entries: Vec<Value> = supplied
        .iter()
        .map(|r| json!({"resource": (*r).clone()}))
        .collect();

    // Copy the stored resources the request and the supplied resources
    // depend on, following value set references.
    let mut pending: Vec<(String, usize)> = direct.into_iter().map(|c| (c, 0)).collect();
    for r in supplied {
        let mut refs = BTreeSet::new();
        compose_canonicals(r, &mut refs);
        pending.extend(refs.into_iter().map(|c| (c, 0)));
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut copied_concepts = 0usize;
    while let Some((canonical, depth)) = pending.pop() {
        if depth > MAX_DEPTH || !seen.insert(canonical.clone()) {
            continue;
        }
        if supplied_urls.contains(&canonical) {
            continue;
        }
        let code_systems = stored_code_systems(backend, &ctx, &canonical).await;
        for cs in &code_systems {
            let content = str_of(cs, "content").unwrap_or("complete");
            let concepts = concept_count(cs);
            if concepts == 0 && matches!(content, "complete" | "fragment") {
                // Stored from its distribution files: the resource has no
                // concepts to copy. Such a request stays on the main store.
                tracing::debug!(system = %canonical, "tx-resource overlay: dependency not copyable");
                return Ok(None);
            }
            copied_concepts += concepts;
            if copied_concepts > MAX_COPIED_CONCEPTS {
                return Ok(None);
            }
            entries.push(json!({"resource": cs}));
        }
        if !code_systems.is_empty() {
            // Its stored supplements, for automatic and named use alike.
            let supplements = CodeSystemOperations::search(
                backend,
                &ctx,
                crate::types::ResourceSearchQuery {
                    supplements: Some(canonical.clone()),
                    count: Some(50),
                    ..Default::default()
                },
            )
            .await
            .unwrap_or_default();
            for s in supplements {
                if str_of(&s, "url").is_some_and(|u| !supplied_urls.contains(u)) {
                    entries.push(json!({"resource": s}));
                }
            }
            continue;
        }
        let value_sets = stored_value_sets(backend, &ctx, &canonical).await;
        let maps = if value_sets.is_empty() {
            stored_concept_maps(backend, &ctx, &canonical).await
        } else {
            Vec::new()
        };
        for r in value_sets.into_iter().chain(maps) {
            let mut refs = BTreeSet::new();
            compose_canonicals(&r, &mut refs);
            pending.extend(refs.into_iter().map(|c| (c, depth + 1)));
            entries.push(json!({"resource": r}));
        }
    }

    let overlay_backend = SqliteTerminologyBackend::in_memory()?;
    let bundle = json!({"resourceType": "Bundle", "type": "collection", "entry": entries});
    overlay_backend
        .import_bundle(&ctx, bundle.to_string().as_bytes())
        .await?;
    let mut overlay = AppState::new(overlay_backend)
        .with_max_expansion_size(state.max_expansion_size)
        .with_auto_supplements(state.auto_supplements);
    overlay.is_tx_overlay = true;
    overlay.auto_supplements_skip = Arc::new(
        supplied
            .iter()
            .filter(|r| str_of(r, "content") == Some("supplement"))
            .filter_map(|r| str_of(r, "url").map(str::to_string))
            .collect(),
    );
    Ok(Some(Arc::new(overlay)))
}
