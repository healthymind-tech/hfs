//! Storage backend implementations for the Helios Terminology Server.
//!
//! Each sub-module provides a concrete implementation of [`TerminologyBackend`]
//! gated by the corresponding feature flag.
//!
//! | Module     | Feature    | Type                          |
//! |------------|------------|-------------------------------|
//! | `sqlite`   | `sqlite`   | `SqliteTerminologyBackend`    |
//! | `postgres` | `postgres` | `PostgresTerminologyBackend`  |
//!
//! [`TerminologyBackend`]: crate::traits::TerminologyBackend

use std::borrow::Cow;
use std::sync::LazyLock;

#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "sqlite")]
pub use sqlite::SqliteTerminologyBackend;

#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "postgres")]
pub use postgres::PostgresTerminologyBackend;

/// Precedence among `code_systems` rows sharing one canonical URL.
///
/// This is the single definition of "which row wins when the caller did not pin
/// a version". Both backends embed it verbatim, so SQLite and Postgres cannot
/// drift apart. It assumes the candidate table is named/aliased `code_systems`.
///
/// Tiers, in order:
///
/// 1. **`content`** — a usable definition beats a `fragment`/`example`, which
///    beats a `not-present` stub. Load-bearing: VSAC and the FHIR core packages
///    ship empty `not-present` stubs for SNOMED/LOINC whose `version` string
///    (`"current"`) text-sorts *above* a real edition (`"20260501"`), so this
///    tier is the only thing keeping `version=current` resolving to the real
///    import. See `docs/ig-publisher-compatibility.md`.
/// 2. **has concepts** — a populated row beats an empty one, for the same reason.
/// 3. **`current_pin`** — a version an operator explicitly marked "current"
///    through the admin API beats every other populated row. It sits below
///    tiers 1–2 so pinning an empty stub cannot hide the real edition, and
///    above provenance and version order because it is an explicit decision.
///    Unpinned (the default), the semantic newest version wins as before.
/// 4. **`authority_rank`** — the original beats a re-published copy. This is the
///    tier that fixes the `terminology.hl7.org` collisions: `hl7.fhir.r4.core`
///    re-ships 746 THO CodeSystems stamped with the *FHIR release* version
///    (`4.0.1`), which outsorts the code system's own version (`1.0.0`) under any
///    ordering — lexicographic or semver — even though the copy is truncated.
///    Provenance is the only thing that separates those rows; nothing intrinsic
///    to them does. `NULL` (a row imported before the column existed) coalesces
///    to 0, so an un-re-imported database keeps its previous behaviour.
/// 5. **`version`** — highest version string wins, as before.
/// 6. **`id`** — a stable, deterministic final tiebreak.
///
/// Tier 4 sits *below* 1 and 2 deliberately: if a package that owns a canonical
/// ships only an empty stub of it while some other package ships a populated
/// copy, the populated row should still win — being authoritative is worthless
/// if the row has no concepts to validate against.
///
/// `alias` is how the `code_systems` table is named in the enclosing query
/// (`"code_systems"` when unaliased, or e.g. `"s"` / `"cs"`).
///
/// Every query that has to choose one row for a canonical URL must order by
/// this. When they disagree, the server becomes internally incoherent — e.g.
/// `$validate-code` resolving against the authoritative row while `$lookup`
/// reads `resource_json` from the truncated copy. That incoherence is what
/// let this bug class persist.
pub(crate) fn cs_precedence_order_by(alias: &str) -> Cow<'static, str> {
    // This clause is spliced into a `format!`-built SQL string at ~40 hot-path
    // resolver call sites, once (or more) per request. Only three aliases are
    // ever passed (`"code_systems"`, `"s"`, `"value_sets"` is vs-only), so cache
    // their rendered form and rebuild only for an unexpected alias. The cached
    // value is produced by the same `build_cs_precedence_order_by` as the
    // fallback, so the SQL text is identical whichever branch is taken.
    match alias {
        "code_systems" => Cow::Borrowed(CS_ORDER_BY_CODE_SYSTEMS.as_str()),
        "s" => Cow::Borrowed(CS_ORDER_BY_S.as_str()),
        other => Cow::Owned(build_cs_precedence_order_by(other)),
    }
}

fn build_cs_precedence_order_by(alias: &str) -> String {
    format!(
        "(CASE COALESCE({alias}.content, 'complete') \
              WHEN 'complete'    THEN 0 \
              WHEN 'supplement'  THEN 0 \
              WHEN 'fragment'    THEN 1 \
              WHEN 'example'     THEN 1 \
              WHEN 'not-present' THEN 2 \
              ELSE 1 END), \
         (CASE WHEN EXISTS \
             (SELECT 1 FROM concepts hc WHERE hc.system_id = {alias}.id) \
             THEN 0 ELSE 1 END), \
         (CASE WHEN {alias}.current_pin = 1 THEN 0 ELSE 1 END), \
         COALESCE({alias}.authority_rank, 0), \
         COALESCE({alias}.version_sort, {alias}.version, '') DESC, \
         {alias}.id",
    )
}

static CS_ORDER_BY_CODE_SYSTEMS: LazyLock<String> =
    LazyLock::new(|| build_cs_precedence_order_by("code_systems"));
static CS_ORDER_BY_S: LazyLock<String> = LazyLock::new(|| build_cs_precedence_order_by("s"));

/// `ORDER BY` for a resource search (`GET /CodeSystem?url=…`, and every
/// operation that resolves "the" CodeSystem for a URL with
/// `search(url, count: 1)`).
///
/// A search narrowed to one canonical URL without a version lists the rows in
/// [`cs_precedence_order_by`] order, so its first hit is the row every other
/// versionless resolver picks. Before, it listed them by `created_at`: with
/// the THO `v3-loinc` stub (`content=not-present`, version `3.1.0`) imported
/// before the real LOINC 2.83, `$expand` expanded 2.83 but reported
/// `used-codesystem = http://loinc.org|3.1.0` from the stub. Other searches
/// keep insertion order.
pub(crate) fn search_order_by(table: &str, url_without_version: bool) -> Cow<'static, str> {
    match (table, url_without_version) {
        ("code_systems", true) => cs_precedence_order_by("code_systems"),
        _ => Cow::Borrowed("created_at"),
    }
}

/// Precedence among `value_sets` rows sharing one canonical URL.
///
/// The ValueSet analogue of [`cs_precedence_order_by`]. `value_sets` has no
/// `content` column and no concept rows, so only the provenance and version
/// tiers apply: original beats re-published copy, then highest version, then a
/// stable `id`.
///
/// This matters for the same reason: `hl7.fhir.r4.core` re-ships 603 THO
/// ValueSets under canonical URLs it does not own.
pub(crate) fn vs_precedence_order_by(alias: &str) -> Cow<'static, str> {
    // Only `"value_sets"` is ever passed; cache it and rebuild for any other.
    // Same builder feeds cache and fallback, so the SQL text is identical.
    match alias {
        "value_sets" => Cow::Borrowed(VS_ORDER_BY_VALUE_SETS.as_str()),
        other => Cow::Owned(build_vs_precedence_order_by(other)),
    }
}

fn build_vs_precedence_order_by(alias: &str) -> String {
    format!(
        "COALESCE({alias}.authority_rank, 0), \
         COALESCE({alias}.version_sort, {alias}.version, '') DESC, \
         {alias}.id",
    )
}

static VS_ORDER_BY_VALUE_SETS: LazyLock<String> =
    LazyLock::new(|| build_vs_precedence_order_by("value_sets"));

/// `expansion.contains` entries built straight from `compose.include.concept[]`
/// (code + embedded display), skipping codes already in `skip`.
pub(crate) fn enumerated_from_compose(
    system_url: &str,
    version: Option<&str>,
    explicit_codes: &[serde_json::Value],
    skip: &std::collections::HashSet<String>,
) -> Vec<crate::types::ExpansionContains> {
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for entry in explicit_codes {
        let Some(code) = entry["code"].as_str() else {
            continue;
        };
        if skip.contains(code) || !seen.insert(code) {
            continue;
        }
        out.push(crate::types::ExpansionContains {
            system: system_url.to_owned(),
            version: version.map(str::to_owned),
            code: code.to_owned(),
            display: entry["display"].as_str().map(str::to_owned),
            is_abstract: None,
            inactive: None,
            designations: vec![],
            properties: vec![],
            extensions: vec![],
            contains: vec![],
        });
    }
    out
}

/// Warning issue for a code accepted through ValueSet enumeration while the
/// CodeSystem has no loaded content to verify it against. Shared by both
/// storage backends.
pub(crate) fn push_unverified_code_system_warning(
    response: &mut crate::types::ValidateCodeResponse,
    system_url: &str,
    value_set_url: &str,
) {
    if response
        .issues
        .iter()
        .any(|i| i.message_id.as_deref() == Some("UNKNOWN_CODESYSTEM"))
    {
        return;
    }
    response.issues.push(crate::types::ValidationIssue {
        severity: "warning".into(),
        fhir_code: "not-found".into(),
        tx_code: "not-found".into(),
        text: format!(
            "A definition for CodeSystem '{system_url}' could not be found, so the code could not \
             be checked against it; it was accepted because the ValueSet '{value_set_url}' \
             enumerates it"
        ),
        expression: None,
        location: None,
        message_id: Some("UNKNOWN_CODESYSTEM".into()),
    });
}

/// True when `ver` is the sentinel `current` (case-insensitive).
///
/// IG Publisher, VSAC stubs, and CQL-generated Library `dataRequirement` entries
/// validate SNOMED with `version=current`. HTS resolves that to the best loaded
/// edition (complete row with concepts), not a literal version string match.
/// See `docs/ig-publisher-compatibility.md` and `fetch_versions` ordering in
/// `sqlite/code_system.rs` / `postgres/code_system.rs`.
pub(super) fn code_system_version_is_current(ver: &str) -> bool {
    ver.eq_ignore_ascii_case("current")
}

/// Re-rank typeahead hits for clinician-facing search (exact / short terms first).
///
/// Shared by both backends: SQLite ranks text-filtered expansions with it, and
/// so does PostgreSQL, so a filtered page lists the same codes in the same order.
/// Aligns with SNOMED Search and Data Entry Guide: prefer exact/preferred terms
/// and short common phrases over long FSNs.
pub(crate) fn sort_typeahead_candidates(
    candidates: &mut [crate::types::ExpansionContains],
    query: &str,
) {
    candidates.sort_by(|a, b| {
        typeahead_match_score(query, a)
            .cmp(&typeahead_match_score(query, b))
            .then_with(|| {
                a.display
                    .as_deref()
                    .unwrap_or("")
                    .len()
                    .cmp(&b.display.as_deref().unwrap_or("").len())
            })
            .then_with(|| a.code.cmp(&b.code))
    });
}

fn typeahead_match_score(query: &str, hit: &crate::types::ExpansionContains) -> i32 {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return i32::MAX;
    }
    let display = hit.display.as_deref().unwrap_or("").to_lowercase();
    let code = hit.code.to_lowercase();

    if display == q {
        return 0;
    }
    if code == q {
        return 1;
    }
    if typeahead_word_match(&display, &q) {
        return 20 + display.len() as i32;
    }
    if display.starts_with(&q) {
        return 10 + display.len() as i32;
    }
    if let Some(pos) = display.find(&q) {
        return 30 + pos as i32 + display.len() as i32;
    }
    i32::MAX
}

fn typeahead_word_match(display: &str, query: &str) -> bool {
    display
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word == query)
}

/// Insert `(key, value)` into a bounded cache, evicting one existing entry when
/// the map is already at `max` capacity.
///
/// The response and concept-flag caches are read under a shared (`read`) lock
/// on the `$lookup` / `$validate-code` hot paths, so we deliberately do **not**
/// track per-entry recency: true LRU would require taking a write lock on every
/// read to bump a recency counter, serializing the very lookups the cache
/// exists to speed up. Instead, when the map is full we evict a single
/// arbitrary entry (std `HashMap` iteration order is randomized per process, so
/// this is effectively random replacement) and admit the new key.
///
/// This replaces the previous "insert only while `len < max`" policy, which
/// *froze* each cache once it filled: the first `max` distinct keys held their
/// slots permanently and every later key missed forever, making the cache
/// useless for any working set larger than `max` (e.g. diverse-code `$lookup`
/// traffic against a 600 K-concept SNOMED system). Random replacement keeps the
/// same memory ceiling while letting hot keys re-enter the cache.
pub(crate) fn bounded_cache_insert<K, V, S>(
    map: &mut std::collections::HashMap<K, V, S>,
    key: K,
    value: V,
    max: usize,
) where
    K: std::hash::Hash + Eq + Clone,
    S: std::hash::BuildHasher,
{
    if max == 0 {
        return;
    }
    // Only evict when inserting a genuinely new key would exceed the bound;
    // overwriting an existing key does not grow the map.
    if map.len() >= max && !map.contains_key(&key) {
        // Clone one key to end the immutable borrow before removing. The keys
        // here are small (`String` / `(String, String)`) and this only runs on
        // a cache miss, so the clone is negligible next to the SQLite work that
        // produced `value`.
        if let Some(evict) = map.keys().next().cloned() {
            map.remove(&evict);
        }
    }
    map.insert(key, value);
}

#[cfg(test)]
mod precedence_tests {
    use super::*;

    // Locks the exact ORDER BY text. The precedence order is the single arbiter
    // of which same-canonical-URL row wins (content > has-concepts > authority >
    // version > id); a silent change to this string would re-open the
    // resolution-incoherence bug class #200 closed. These literals are the
    // clause as it shipped, so both the cached fast path and the owned fallback
    // are asserted byte-for-byte.
    const CS_EXPECTED_CODE_SYSTEMS: &str = "(CASE COALESCE(code_systems.content, 'complete') \
              WHEN 'complete'    THEN 0 \
              WHEN 'supplement'  THEN 0 \
              WHEN 'fragment'    THEN 1 \
              WHEN 'example'     THEN 1 \
              WHEN 'not-present' THEN 2 \
              ELSE 1 END), \
         (CASE WHEN EXISTS \
             (SELECT 1 FROM concepts hc WHERE hc.system_id = code_systems.id) \
             THEN 0 ELSE 1 END), \
         (CASE WHEN code_systems.current_pin = 1 THEN 0 ELSE 1 END), \
         COALESCE(code_systems.authority_rank, 0), \
         COALESCE(code_systems.version_sort, code_systems.version, '') DESC, \
         code_systems.id";

    #[test]
    fn cs_precedence_cached_and_fallback_match_shipped_sql() {
        // Cached fast paths (the only aliases used in the codebase).
        assert_eq!(
            &*cs_precedence_order_by("code_systems"),
            CS_EXPECTED_CODE_SYSTEMS
        );
        assert_eq!(
            &*cs_precedence_order_by("s"),
            CS_EXPECTED_CODE_SYSTEMS
                .replace("code_systems", "s")
                .as_str()
        );
        // Fallback (unexpected alias) renders through the same builder, so it is
        // byte-identical to the cached form with the alias substituted.
        assert_eq!(
            &*cs_precedence_order_by("cs"),
            CS_EXPECTED_CODE_SYSTEMS
                .replace("code_systems", "cs")
                .as_str()
        );
        // The cached value equals a fresh build — no drift between the two paths.
        assert_eq!(
            &*cs_precedence_order_by("code_systems"),
            build_cs_precedence_order_by("code_systems").as_str()
        );
    }

    #[test]
    fn vs_precedence_cached_and_fallback_match_shipped_sql() {
        const VS_EXPECTED: &str = "COALESCE(value_sets.authority_rank, 0), \
         COALESCE(value_sets.version_sort, value_sets.version, '') DESC, \
         value_sets.id";
        assert_eq!(&*vs_precedence_order_by("value_sets"), VS_EXPECTED);
        assert_eq!(
            &*vs_precedence_order_by("s"),
            VS_EXPECTED.replace("value_sets", "s").as_str()
        );
        assert_eq!(
            &*vs_precedence_order_by("value_sets"),
            build_vs_precedence_order_by("value_sets").as_str()
        );
    }
}
