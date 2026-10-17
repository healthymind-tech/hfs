//! PostgreSQL-backed terminology service backend.
//!
//! Gated by the `postgres` feature flag. Uses `deadpool-postgres` for async
//! connection pooling and `tokio-postgres` for query execution.

pub mod admin;
pub mod schema;

mod code_system;
mod concept_map;
pub(crate) mod fhir_ids;
mod value_set;

use async_trait::async_trait;
use deadpool_postgres::{Config, GenericClient, Pool, Runtime};

/// A pooled connection: a `tokio_postgres::Client` with a per-connection
/// prepared-statement cache.
pub(crate) type PgClient = deadpool_postgres::ClientWrapper;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use tokio_postgres::NoTls;
use tracing::info;

use crate::error::HtsError;
use crate::import::bundle_parser::{
    self, ParsedCodeSystem, ParsedConcept, ParsedConceptMap, ParsedValueSet,
};
use crate::import::{BundleImportBackend, ImportStats};
use crate::string_search::{ResourceSearchRow, ResourceStringSearch, filter_rows};
use crate::traits::{TerminologyCaches, TerminologyMetadata};
use crate::types::{LookupResponse, ResourceSearchQuery, SubsumesResponse, TranslateResponse};
use crate::version_order::version_sort_key;
use helios_persistence::tenant::TenantContext;

/// Per-compose in-memory expansion index. Mirrors SQLite's
/// `inline_compose_index` (backends/sqlite/value_set.rs:296+).
///
/// Key = fnv64 of canonicalised compose body + system-version pins. Value =
/// a `OnceCell` that holds the full, sorted, unfiltered list of expansion
/// concepts. Slicing (filter / count / offset) happens in Rust on the
/// cached vec, so once a compose is warm a request avoids all DB I/O.
///
/// The `OnceCell` provides **single-flight** semantics: when 50 VUs
/// simultaneously miss the same key, only the first runs `compute_expansion`
/// and `populate_cache`; the other 49 await the cell's value and clone its
/// `Arc` in O(1). Without this, the bench's cold-miss stampede burned
/// 50× redundant 5,975-row INSERTs on a single VSAC VS (EX04). On error the
/// cell stays empty so the next caller retries cleanly.
pub(crate) type ComposeExpansionCache =
    Arc<RwLock<HashMap<u64, Arc<tokio::sync::OnceCell<Arc<value_set::CachedExpansion>>>>>>;

/// `$lookup` response memo. Mirrors SQLite's `lookup_response_cache`
/// (backends/sqlite/code_system.rs:240+).
///
/// Key = canonicalised request string (`system|code|version|lang|date|props`).
/// Skipped when `useSupplement` is non-empty since the merged response depends
/// on per-request supplement resolution.
pub type LookupResponseCache = Arc<RwLock<HashMap<String, Arc<LookupResponse>>>>;

/// `(url, version) → (system_id, name, version)` resolver cache. Mirrors
/// SQLite's `cs_resolved_meta_cache` (backends/sqlite/code_system.rs:1686+).
/// Populated on the `date.is_none()` fast path; bypassed when a point-in-time
/// `date` filter is active.
pub type ResolvedMetaCache =
    Arc<RwLock<HashMap<(String, Option<String>), (String, String, Option<String>)>>>;

/// `$subsumes` response memo (SS01 hot path).
/// Key = `"system|version|codeA|codeB"`. Both ancestor-check directions
/// (subsumes / subsumed-by) share the same key — the outcome captures the
/// resolved direction.
pub type SubsumesResponseCache = Arc<RwLock<HashMap<String, Arc<SubsumesResponse>>>>;

/// Small per-backend memos of CodeSystem / ValueSet metadata that the
/// validate-code and expansion paths look up for every request — the
/// PostgreSQL counterpart of the SQLite backend's `cs_id_cache`,
/// `cs_version_for_msg`, `cs_content`, property-alias and concept-flag
/// caches. Cleared with every other cache on any write.
#[derive(Default)]
pub(crate) struct PgMeta {
    pub(super) system_id: RwLock<HashMap<String, Option<String>>>,
    pub(super) cs_version: RwLock<HashMap<String, Option<String>>>,
    pub(super) cs_content: RwLock<HashMap<String, Option<String>>>,
    pub(super) case_insensitive: RwLock<HashMap<String, bool>>,
    pub(super) prop_codes: RwLock<HashMap<(String, String), Arc<Vec<String>>>>,
    /// `(0 = inactive | 1 = abstract, system url, code)` → flag.
    pub(super) concept_flags: RwLock<HashMap<(u8, String, String), bool>>,
    pub(super) vs_version: RwLock<HashMap<String, Option<String>>>,
    /// `supplements_for_system` by base CodeSystem URL: the query reads
    /// `supplements` out of every supplement's `resource_json` (megabytes
    /// for an ICD-10 translation supplement) — ~140 ms uncached.
    pub(super) supplements: RwLock<HashMap<String, Vec<crate::traits::SupplementInfo>>>,
    /// `code_system_language` by CodeSystem URL.
    pub(super) cs_language: RwLock<HashMap<String, Option<String>>>,
    /// Supplement CodeSystem URL → `(storage id, "url|version")` rows.
    pub(super) supplement_rows: RwLock<HashMap<String, Vec<(String, String)>>>,
    /// CodeSystem URL → storage ids of its rows.
    pub(super) url_system_ids: RwLock<HashMap<String, Vec<String>>>,
    /// Supplement storage id → the codes it defines.
    pub(super) supplement_codes: RwLock<HashMap<String, Arc<HashSet<String>>>>,
    /// `code_system_version_for_url` by URL.
    pub(super) latest_version: RwLock<HashMap<String, Option<String>>>,
    /// `supplement_target` by supplement URL.
    pub(super) supplement_target: RwLock<HashMap<String, Option<crate::traits::SupplementInfo>>>,
    /// CodeSystem URL → codes of the `concept[]` tree in its stored JSON.
    pub(super) json_concept_codes: RwLock<HashMap<String, Arc<HashSet<String>>>>,
}

impl PgMeta {
    fn clear(&self) {
        macro_rules! clear {
            ($($m:ident),*) => { $(if let Ok(mut g) = self.$m.write() { g.clear(); })* };
        }
        clear!(
            system_id,
            cs_version,
            cs_content,
            case_insensitive,
            prop_codes,
            concept_flags,
            vs_version,
            supplements,
            cs_language,
            supplement_rows,
            url_system_ids,
            supplement_codes,
            latest_version,
            supplement_target,
            json_concept_codes
        );
    }
}

/// Bound of each [`PgMeta`] map (random eviction beyond it).
pub(super) const PG_META_CACHE_MAX: usize = 50_000;

/// Memoise `compute` in `map` under `key` (bounded, random eviction).
pub(super) async fn memo<K, V, F>(map: &RwLock<HashMap<K, V>>, key: K, compute: F) -> V
where
    K: std::hash::Hash + Eq + Clone,
    V: Clone,
    F: std::future::Future<Output = V>,
{
    if let Some(v) = map.read().ok().and_then(|g| g.get(&key).cloned()) {
        return v;
    }
    let v = compute.await;
    if let Ok(mut w) = map.write() {
        crate::backends::bounded_cache_insert(&mut *w, key, v.clone(), PG_META_CACHE_MAX);
    }
    v
}

/// `ValueSet/$validate-code` response memo, keyed on every request field
/// that shapes the answer.
pub type ValidateCodeResponseCache =
    Arc<RwLock<HashMap<String, Arc<crate::types::ValidateCodeResponse>>>>;

/// Bound of [`ValidateCodeResponseCache`] (SQLite's
/// `VALIDATE_CODE_RESPONSE_CACHE_MAX`).
pub(super) const PG_VALIDATE_CODE_RESPONSE_CACHE_MAX: usize = 4096;

/// `$translate` response memo (CM01/CM02 hot path).
/// Key = `"url|system|code|source|target|target_system|target_code|reverse|date"`.
pub type TranslateResponseCache = Arc<RwLock<HashMap<String, Arc<TranslateResponse>>>>;

/// Soft cap on cache entries. Once full, new entries are dropped silently
/// (warm-set wins). 16384 mirrors SQLite's iter-9 cap-raise: each LK*/VC*
/// pool holds 2000 distinct codes across 3-5 systems = 6-10K keys; the
/// previous 4096 was being filled by the first two systems' keys, leaving
/// later systems (LK04 RxNorm, etc.) entirely uncached.
pub const PG_COMPOSE_CACHE_MAX: usize = 16384;

/// Soft cap on `lookup_response_cache` entries — mirrors SQLite's
/// `lookup_response_cache_max()` (backends/sqlite/code_system.rs:131-135).
/// Sized to hold all LK*/VC*/SS01 keys simultaneously (~10K).
pub const PG_LOOKUP_RESPONSE_CACHE_MAX: usize = 16384;

/// Soft cap on `subsumes_response_cache` entries.
pub const PG_SUBSUMES_RESPONSE_CACHE_MAX: usize = 16384;

/// Soft cap on `translate_response_cache` entries.
pub const PG_TRANSLATE_RESPONSE_CACHE_MAX: usize = 16384;

/// Count the resources in `table` matching `query`, ignoring `_count` and
/// `_offset`, so a searchset Bundle can report the full match count.
///
/// Uses the same predicates as the search paths: exact `url`/`version`/
/// `status` in SQL and, when present, FHIR string matching of `name`/`title`
/// over the metadata rows.
async fn count_resources<C>(
    client: &C,
    table: &str,
    query: &ResourceSearchQuery,
) -> Result<u64, HtsError>
where
    C: GenericClient + Sync,
{
    let search = ResourceStringSearch::new(query);
    let filter = "WHERE ($1::text IS NULL OR url = $1)
           AND ($2::text IS NULL OR version = $2)
           AND ($3::text IS NULL OR status = $3)";
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 3] =
        [&query.url, &query.version, &query.status];

    if search.is_empty() {
        let count: i64 = client
            .query_one(&format!("SELECT COUNT(*) FROM {table} {filter}"), &params)
            .await
            .map_err(|error| HtsError::StorageError(error_chain(&error)))?
            .get(0);
        return Ok(count.max(0) as u64);
    }

    let rows = client
        .query(
            &format!(
                "SELECT {columns} FROM {table} {filter}",
                columns = crate::string_search::metadata_select_list(table, &search, true)
            ),
            &params,
        )
        .await
        .map_err(|error| HtsError::StorageError(error_chain(&error)))?;
    Ok(rows
        .iter()
        .map(search_row)
        .filter(|row| search.matches_row(row))
        .count() as u64)
}

/// Map a row of [`crate::string_search::metadata_select_list`].
fn search_row(row: &tokio_postgres::Row) -> ResourceSearchRow {
    ResourceSearchRow {
        id: row.get(0),
        url: row.get(1),
        version: row.get(2),
        name: row.get(3),
        title: row.get(4),
        status: row.get(5),
        content: row.get(6),
        supplements: row.get(7),
        publisher: row.get(8),
        updated_at: row.get(9),
        fhir_id: row.get(10),
    }
}

impl PostgresTerminologyBackend {
    /// Pooled wrapper around [`count_resources`] for the per-type search traits.
    pub(super) async fn count_search_matches(
        &self,
        table: &str,
        query: ResourceSearchQuery,
    ) -> Result<u64, HtsError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| HtsError::StorageError(format!("Pool error: {}", error_chain(&e))))?;
        count_resources(&client, table, &query).await
    }
}

/// `GET /<ResourceType>` for `table`, mirroring the SQLite search contract:
/// FHIR string matching on `name`/`title` when present; otherwise exact
/// predicates in SQL. With `synthetic_summary` (CodeSystem, ValueSet),
/// `_summary=true` — and an unfiltered listing — return synthetic metadata
/// resources without reading `resource_json`; the stored resource is returned
/// as stored (its `id` is not rewritten).
async fn search_table(
    client: &mut deadpool_postgres::Object,
    table: &str,
    resource_type: &str,
    mut query: ResourceSearchQuery,
    synthetic_summary: bool,
) -> Result<Vec<serde_json::Value>, HtsError> {
    let string_search = ResourceStringSearch::new(&query);
    if string_search.is_empty() {
        query.name = None;
        query.title = None;
    } else {
        return search_resources(
            client,
            table,
            resource_type,
            query,
            string_search,
            synthetic_summary,
        )
        .await;
    }
    let limit = i64::from(query.count.unwrap_or(20));
    let offset = i64::from(query.offset.unwrap_or(0));
    let metadata_only = synthetic_summary
        && (query.summary.as_deref() == Some("true")
            || query.url.is_none()
                && query.version.is_none()
                && query.name.is_none()
                && query.title.is_none()
                && query.status.is_none());
    let order_by =
        crate::backends::search_order_by(table, query.url.is_some() && query.version.is_none());
    let has_fhir_id = fhir_ids::TABLES.contains(&table);
    let sql = format!(
        "SELECT id, url, version, name, title, status, {fhir_id}{json}
         FROM {table}
         WHERE ($1::text IS NULL OR url = $1)
           AND ($2::text IS NULL OR version = $2)
           AND ($3::text IS NULL OR name = $3)
           AND ($4::text IS NULL OR title = $4)
           AND ($5::text IS NULL OR status = $5)
         ORDER BY {order_by}
         LIMIT $6 OFFSET $7",
        json = if metadata_only { "" } else { ", resource_json" },
        fhir_id = if has_fhir_id { "fhir_id" } else { "NULL::text" }
    );
    let rows = (**client)
        .cq(
            &sql,
            &[
                &query.url,
                &query.version,
                &query.name,
                &query.title,
                &query.status,
                &limit,
                &offset,
            ],
        )
        .await
        .map_err(|e| HtsError::StorageError(error_chain(&e)))?;
    let mut results = Vec::with_capacity(rows.len());
    for row in rows {
        let stored: Option<serde_json::Value> = if metadata_only { None } else { row.get(7) };
        let mut resource = stored.unwrap_or_else(|| {
            code_system::build_synthetic_resource(
                resource_type,
                &row.get::<_, String>(0),
                &row.get::<_, String>(1),
                row.get::<_, Option<String>>(2).as_deref(),
                row.get::<_, Option<String>>(3).as_deref(),
                row.get::<_, Option<String>>(4).as_deref(),
                &row.get::<_, String>(5),
            )
        });
        if has_fhir_id {
            let fhir_id: Option<String> = row.get(6);
            crate::resource_ids::apply_id(
                &mut resource,
                &crate::resource_ids::reported_id(&row.get::<_, String>(0), fhir_id.as_deref()),
            );
        }
        results.push(resource);
    }
    Ok(results)
}

/// Search metadata using backend-neutral FHIR string matching, then hydrate
/// only the selected page (synthetic resources for `_summary=true` when
/// `synthetic_summary`, as SQLite does).
async fn search_resources(
    client: &mut deadpool_postgres::Object,
    table: &str,
    resource_type: &str,
    query: ResourceSearchQuery,
    search: ResourceStringSearch,
    synthetic_summary: bool,
) -> Result<Vec<serde_json::Value>, HtsError> {
    // PostgreSQL's default READ COMMITTED level takes a fresh snapshot per
    // statement, so use REPEATABLE READ to keep metadata selection and JSON
    // hydration on one read snapshot without taking write locks.
    let transaction = client
        .build_transaction()
        .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
        .start()
        .await
        .map_err(|error| HtsError::StorageError(error_chain(&error)))?;
    let result = search_resources_in_transaction(
        &transaction,
        table,
        resource_type,
        query,
        search,
        synthetic_summary,
    )
    .await;
    match result {
        Ok(resources) => {
            transaction
                .commit()
                .await
                .map_err(|error| HtsError::StorageError(error_chain(&error)))?;
            Ok(resources)
        }
        Err(search_error) => match transaction.rollback().await {
            Ok(()) => Err(search_error),
            Err(rollback_error) => Err(HtsError::StorageError(format!(
                "{search_error}; failed to roll back search snapshot: {rollback_error}"
            ))),
        },
    }
}

async fn search_resources_in_transaction<C>(
    client: &C,
    table: &str,
    resource_type: &str,
    query: ResourceSearchQuery,
    search: ResourceStringSearch,
    synthetic_summary: bool,
) -> Result<Vec<serde_json::Value>, HtsError>
where
    C: GenericClient + Sync,
{
    let metadata_sql = format!(
        "SELECT {columns}
         FROM {table}
         WHERE ($1::text IS NULL OR url = $1)
           AND ($2::text IS NULL OR version = $2)
           AND ($3::text IS NULL OR status = $3)
         ORDER BY created_at",
        columns = crate::string_search::metadata_select_list(table, &search, true)
    );
    let rows = client
        .query(&metadata_sql, &[&query.url, &query.version, &query.status])
        .await
        .map_err(|error| HtsError::StorageError(error_chain(&error)))?
        .iter()
        .map(search_row)
        .collect::<Vec<_>>();
    let selected = filter_rows(rows, &query, &search);

    let use_synthetic = synthetic_summary && query.summary.as_deref() == Some("true");
    let stored_resources = if selected.is_empty() || use_synthetic {
        HashMap::new()
    } else {
        let ids = selected
            .iter()
            .map(|row| row.id.clone())
            .collect::<Vec<_>>();
        let resource_sql = format!("SELECT id, resource_json FROM {table} WHERE id = ANY($1)");
        client
            .query(&resource_sql, &[&ids])
            .await
            .map_err(|error| HtsError::StorageError(error_chain(&error)))?
            .into_iter()
            .map(|row| {
                (
                    row.get::<_, String>(0),
                    row.get::<_, Option<serde_json::Value>>(1),
                )
            })
            .collect::<HashMap<_, _>>()
    };

    let has_fhir_id = fhir_ids::TABLES.contains(&table);
    let mut results = Vec::with_capacity(selected.len());
    for row in selected {
        let mut resource = stored_resources
            .get(&row.id)
            .cloned()
            .flatten()
            .unwrap_or_else(|| {
                code_system::build_synthetic_resource(
                    resource_type,
                    &row.id,
                    &row.url,
                    row.version.as_deref(),
                    row.name.as_deref(),
                    row.title.as_deref(),
                    &row.status,
                )
            });
        if has_fhir_id {
            crate::resource_ids::apply_id(
                &mut resource,
                &crate::resource_ids::reported_id(&row.id, row.fhir_id.as_deref()),
            );
        }
        results.push(resource);
    }
    Ok(results)
}

/// PostgreSQL-backed terminology service backend.
///
/// Wraps a `deadpool-postgres` connection pool. The schema is applied
/// automatically at construction time via [`schema::apply`].
///
/// All [`TerminologyBackend`] operations are implemented on this type,
/// enabling it to be placed directly in [`AppState`].
///
/// [`TerminologyBackend`]: crate::traits::TerminologyBackend
/// [`AppState`]: crate::state::AppState
#[derive(Clone)]
pub struct PostgresTerminologyBackend {
    pub(super) pool: Pool,
    /// Connection string, for the one-off connections of [`Self::sync_query`].
    pub(super) database_url: String,
    /// Process-local inline-compose expansion cache. Hot path for repeated
    /// POST $expand requests with identical compose bodies — k6 EX02/04/05/06
    /// pool entries hit the same compose hundreds of times during a 30s run.
    pub(super) inline_compose_cache: ComposeExpansionCache,
    /// `$lookup` response memo — LK01-04 hit the same `(system, code,
    /// version, lang)` tuples per pool entry across 50 VUs.
    pub(super) lookup_response_cache: LookupResponseCache,
    /// `resolve_code_system` memo, keyed by `(url, version)`. Drives LK*,
    /// VC*, and SS01 — every CS-touching op begins by resolving the URL to
    /// an internal `(system_id, name, version)` triple.
    pub(super) cs_resolved_meta_cache: ResolvedMetaCache,
    /// `$subsumes` response memo — SS01 hot path was at 17% of SQLite
    /// after iter 2 because find_concept + check_ancestor (recursive CTE)
    /// ran on every request.
    pub(super) subsumes_response_cache: SubsumesResponseCache,
    /// `$translate` response memo — CM01/CM02 hot paths repeat the same
    /// (system, code, target) tuples across pool entries.
    pub(super) translate_response_cache: TranslateResponseCache,
    /// `ValueSet/$validate-code` response memo (SQLite's
    /// `validate_code_response_cache`).
    pub(super) validate_code_response_cache: ValidateCodeResponseCache,
    /// CodeSystem / ValueSet metadata memos (see [`PgMeta`]).
    pub(super) meta: Arc<PgMeta>,
    /// Closure `COUNT(*)` memo per `(system_id, root)` (see `value_set.rs`).
    pub(super) closure_count_cache: value_set::ClosureCountCache,
    /// `?fhir_vs=isa/X` descendant-prefix memo (see `value_set.rs`).
    pub(super) root_prefix_cache: value_set::RootPrefixCache,
}

impl PostgresTerminologyBackend {
    /// Connect to PostgreSQL at `database_url` and apply the HTS schema.
    ///
    /// # Errors
    ///
    /// Returns [`HtsError::StorageError`] if the pool cannot be created or the
    /// schema migration fails.
    pub async fn new(database_url: &str) -> Result<Self, HtsError> {
        let pool = build_pool(database_url)?;

        {
            let mut client = pool.get().await.map_err(|e| {
                HtsError::StorageError(format!(
                    "Failed to acquire PG connection: {}",
                    error_chain(&e)
                ))
            })?;

            schema::apply(&client).await.map_err(|e| {
                HtsError::StorageError(format!("Failed to apply HTS schema: {}", error_chain(&e)))
            })?;
            schema::migrate_version_sort_pg(&client)
                .await
                .map_err(|e| {
                    HtsError::StorageError(format!(
                        "Failed to backfill version_sort: {}",
                        error_chain(&e)
                    ))
                })?;

            // Unique reported ids across versions; repairs databases where
            // versions still share one id (see `crate::resource_ids`).
            fhir_ids::repair(&mut client).await?;

            // Backfill `concept_closure` for any system that has hierarchy edges
            // but no closure rows yet. Idempotent and per-system, so existing
            // closures aren't rebuilt. For a fresh CI database, this is a no-op
            // because `hts import` already triggered the build via
            // `migrate_concept_closure_pg` at the end of the import phase.
            schema::migrate_concept_closure_pg(&mut client)
                .await
                .map_err(|e| {
                    HtsError::StorageError(format!(
                        "Failed to build concept_closure: {}",
                        error_chain(&e)
                    ))
                })?;
        }

        info!(
            database_url = %helios_persistence::core::bulk_submit_input::redact_url(database_url),
            "PostgreSQL terminology backend initialized"
        );

        Ok(Self {
            pool,
            database_url: database_url.to_owned(),
            inline_compose_cache: Arc::new(RwLock::new(HashMap::new())),
            lookup_response_cache: Arc::new(RwLock::new(HashMap::new())),
            cs_resolved_meta_cache: Arc::new(RwLock::new(HashMap::new())),
            subsumes_response_cache: Arc::new(RwLock::new(HashMap::new())),
            translate_response_cache: Arc::new(RwLock::new(HashMap::new())),
            validate_code_response_cache: Arc::new(RwLock::new(HashMap::new())),
            meta: Arc::new(PgMeta::default()),
            closure_count_cache: Arc::new(RwLock::new(HashMap::new())),
            root_prefix_cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Borrow the underlying `deadpool-postgres` connection pool.
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Build (or rebuild) `concept_closure` for every system that has
    /// hierarchy edges but no closure rows.
    ///
    /// Called once at the end of a CLI bulk import so the server's first
    /// request doesn't pay the build cost. For SNOMED CT (~20M closure pairs)
    /// this takes ~30–60 s.
    pub async fn rebuild_missing_closures(&self) -> Result<(), HtsError> {
        let mut client = self
            .pool
            .get()
            .await
            .map_err(|e| HtsError::StorageError(format!("Pool error: {}", error_chain(&e))))?;
        schema::migrate_concept_closure_pg(&mut client)
            .await
            .map_err(|e| {
                HtsError::StorageError(format!("concept_closure migration: {}", error_chain(&e)))
            })?;
        Ok(())
    }
}

impl TerminologyCaches for PostgresTerminologyBackend {
    /// Drop every per-instance response cache plus the two process-global
    /// closure memos, so stale `$lookup` / `$translate` / `resolve_code_system`
    /// answers cannot shadow content that has just changed.
    ///
    /// Invoked after a successful `import_bundle` / `import_parsed`, from
    /// `delete_normalized`, and — since issue #304 — from the generic CRUD
    /// write seam. The CRUD create/update path reaches this twice (once through
    /// the importer, once through the seam); the method is idempotent, and a
    /// second clear of already-empty maps is not worth branching to avoid.
    ///
    /// # Totality is enforced by the compiler
    ///
    /// The destructuring below names every field with no `..` rest pattern, so
    /// adding a sixth cache to [`PostgresTerminologyBackend`] fails to compile
    /// (E0027) until the author names it here. This mirrors the SQLite backend,
    /// where the same shape closes a real six-of-sixteen eviction gap.
    fn invalidate_caches(&self) {
        // Exhaustive by construction: no `..`. See the doc comment above.
        let Self {
            // Not caches: the connection pool and its connection string.
            pool: _,
            database_url: _,
            inline_compose_cache,
            lookup_response_cache,
            cs_resolved_meta_cache,
            subsumes_response_cache,
            translate_response_cache,
            validate_code_response_cache,
            meta,
            closure_count_cache,
            root_prefix_cache,
        } = self;

        // Clear one `RwLock`-guarded map, ignoring a poisoned lock: a poisoned
        // cache is already unusable, and failing a write because some unrelated
        // reader panicked would be worse than leaving it be.
        macro_rules! clear {
            ($($cache:expr),* $(,)?) => {
                $(if let Ok(mut guard) = $cache.write() { guard.clear(); })*
            };
        }

        clear!(
            inline_compose_cache,
            lookup_response_cache,
            cs_resolved_meta_cache,
            subsumes_response_cache,
            translate_response_cache,
            validate_code_response_cache,
            closure_count_cache,
            root_prefix_cache,
        );
        meta.clear();
        crate::loinc_filters::clear();
    }
}

/// Run `sql` with every parameter typed `TEXT` through the connection's
/// prepared-statement cache.
///
/// The first use on a connection prepares the statement (one extra round
/// trip); later uses only bind and execute, and after a few executions
/// PostgreSQL switches to a cached generic plan, so the hot lookup / validate
/// / subsumes / translate queries skip parsing and planning (0.5–2 ms per
/// statement on the production database with custom plans).
pub(crate) async fn query_text(
    client: &PgClient,
    sql: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
) -> Result<Vec<tokio_postgres::Row>, tokio_postgres::Error> {
    const TEXT: tokio_postgres::types::Type = tokio_postgres::types::Type::TEXT;
    let types = [TEXT; 8];
    let stmt = if params.len() <= types.len() {
        client
            .prepare_typed_cached(sql, &types[..params.len()])
            .await?
    } else {
        client
            .prepare_typed_cached(sql, &vec![TEXT; params.len()])
            .await?
    };
    client.query(&stmt, params).await
}

/// [`query_text`] returning the first row, if any.
pub(crate) async fn query_text_opt(
    client: &PgClient,
    sql: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
) -> Result<Option<tokio_postgres::Row>, tokio_postgres::Error> {
    Ok(query_text(client, sql, params).await?.into_iter().next())
}

/// Statement-cached `query` / `query_opt` / `query_one` on a pooled
/// connection (see [`query_text`]): the statement is prepared once per
/// connection, later calls bind and execute only.
pub(crate) trait CachedQuery {
    fn cq<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn tokio_postgres::types::ToSql + Sync)],
    ) -> impl std::future::Future<Output = Result<Vec<tokio_postgres::Row>, tokio_postgres::Error>>
    + Send
    + 'a;
    fn cq_opt<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn tokio_postgres::types::ToSql + Sync)],
    ) -> impl std::future::Future<
        Output = Result<Option<tokio_postgres::Row>, tokio_postgres::Error>,
    > + Send
    + 'a;
    fn cq_one<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn tokio_postgres::types::ToSql + Sync)],
    ) -> impl std::future::Future<Output = Result<tokio_postgres::Row, tokio_postgres::Error>> + Send + 'a;
}

impl CachedQuery for PgClient {
    fn cq<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn tokio_postgres::types::ToSql + Sync)],
    ) -> impl std::future::Future<Output = Result<Vec<tokio_postgres::Row>, tokio_postgres::Error>>
    + Send
    + 'a {
        async move {
            let stmt = self.prepare_cached(sql).await?;
            self.query(&stmt, params).await
        }
    }
    fn cq_opt<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn tokio_postgres::types::ToSql + Sync)],
    ) -> impl std::future::Future<
        Output = Result<Option<tokio_postgres::Row>, tokio_postgres::Error>,
    > + Send
    + 'a {
        async move {
            let stmt = self.prepare_cached(sql).await?;
            self.query_opt(&stmt, params).await
        }
    }
    fn cq_one<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn tokio_postgres::types::ToSql + Sync)],
    ) -> impl std::future::Future<Output = Result<tokio_postgres::Row, tokio_postgres::Error>> + Send + 'a
    {
        async move {
            let stmt = self.prepare_cached(sql).await?;
            self.query_one(&stmt, params).await
        }
    }
}

/// Statement-cached writes for the import path: `write_*` run on a pooled
/// connection or a transaction / savepoint of one, all of which keep a
/// per-connection prepared-statement cache, so each statement is parsed once
/// per connection instead of once per resource (two round trips → one).
trait CachedWrite: GenericClient {
    async fn cexec(
        &self,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<u64, tokio_postgres::Error> {
        let stmt = self.prepare_cached(sql).await?;
        self.execute(&stmt, params).await
    }
    async fn cquery(
        &self,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<Vec<tokio_postgres::Row>, tokio_postgres::Error> {
        let stmt = self.prepare_cached(sql).await?;
        self.query(&stmt, params).await
    }
    async fn cquery_opt(
        &self,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<Option<tokio_postgres::Row>, tokio_postgres::Error> {
        let stmt = self.prepare_cached(sql).await?;
        self.query_opt(&stmt, params).await
    }
    async fn cquery_one(
        &self,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<tokio_postgres::Row, tokio_postgres::Error> {
        let stmt = self.prepare_cached(sql).await?;
        self.query_one(&stmt, params).await
    }
}

impl<T: GenericClient + ?Sized> CachedWrite for T {}

/// Render an error together with its full `source()` chain.
///
/// `tokio_postgres::Error`'s `Display` impl only prints the error *kind* —
/// for a server-side failure that is the bare string `"db error"`. The actual
/// SQLSTATE and `ERROR: …` message live in `source()`, so formatting such an
/// error with `{e}` discards the one piece of information needed to diagnose
/// it. Walking the chain preserves messages like
/// `db error: ERROR: could not resize shared memory segment …`.
pub(crate) fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        out.push_str(": ");
        out.push_str(&s.to_string());
        src = s.source();
    }
    out
}

fn build_pool(database_url: &str) -> Result<Pool, HtsError> {
    let mut cfg = Config::new();
    cfg.url = Some(database_url.to_string());
    // 32 connections: warm requests bypass the pool entirely (see the
    // closure-page / inline-compose warm-hit fast paths in value_set.rs), so
    // the pool only gates *cold* expansions. 16 was too few to warm a
    // ~6.5k-entry benchmark pool within a 30s vu50 stage; 32 doubles the
    // cold-warming throughput while staying well under PG's
    // `max_connections=100`.
    cfg.pool = Some(deadpool_postgres::PoolConfig::new(32));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls).map_err(|e| {
        HtsError::StorageError(format!("Failed to create PG pool: {}", error_chain(&e)))
    })
}

// ── TerminologyMetadata ────────────────────────────────────────────────────────

impl PostgresTerminologyBackend {
    /// Run `f` against a connection from synchronous code (the
    /// [`TerminologyMetadata`] methods are sync).
    ///
    /// On a multi-thread runtime this blocks the worker in place and uses the
    /// pool. A current-thread runtime cannot be blocked that way — the pool's
    /// connection tasks would need the very thread being blocked — so there the
    /// query runs on a scoped helper thread with its own runtime and a one-off
    /// connection. (`block_in_place` alone panics on a current-thread runtime.)
    fn sync_query<T, F>(&self, f: F) -> Option<T>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(
                &'a tokio_postgres::Client,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Option<T>> + Send + 'a>,
            > + Send
            + 'static,
    {
        use tokio::runtime::{Handle, RuntimeFlavor};
        match Handle::try_current() {
            Ok(h) if h.runtime_flavor() == RuntimeFlavor::MultiThread => {
                let pool = self.pool.clone();
                tokio::task::block_in_place(|| {
                    h.block_on(async move {
                        let obj = pool.get().await.ok()?;
                        let client: &tokio_postgres::Client = &obj;
                        f(client).await
                    })
                })
            }
            _ => {
                let url = self.database_url.clone();
                std::thread::scope(|scope| {
                    scope
                        .spawn(move || {
                            let rt = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .ok()?;
                            rt.block_on(async move {
                                let (client, conn) =
                                    tokio_postgres::connect(&url, NoTls).await.ok()?;
                                let driver = tokio::spawn(conn);
                                let out = f(&client).await;
                                drop(client);
                                let _ = driver.await;
                                out
                            })
                        })
                        .join()
                        .ok()
                        .flatten()
                })
            }
        }
    }
}

impl TerminologyMetadata for PostgresTerminologyBackend {
    fn backend_name(&self) -> &'static str {
        "postgres"
    }

    fn supported_systems(&self) -> Vec<String> {
        self.sync_query(|client| {
            Box::pin(async move {
                let rows = client
                    .query("SELECT url FROM code_systems ORDER BY url", &[])
                    .await
                    .ok()?;
                Some(rows.into_iter().map(|r| r.get::<_, String>(0)).collect())
            })
        })
        .unwrap_or_default()
    }

    fn supports_subsumption(&self) -> bool {
        true
    }

    fn resource_url_by_id(&self, resource_type: &str, id: &str) -> Option<String> {
        let table = match resource_type {
            "CodeSystem" => "code_systems",
            "ValueSet" => "value_sets",
            "ConceptMap" => "concept_maps",
            _ => return None,
        };
        let id = id.to_owned();
        self.sync_query(move |client| {
            Box::pin(async move {
                if table == "concept_maps" {
                    let row = client
                        .query_opt("SELECT url FROM concept_maps WHERE id = $1", &[&id])
                        .await
                        .ok()??;
                    return Some(row.get::<_, String>(0));
                }
                // The row reporting `id`, a storage id or `<id>-<version>`
                // (see `fhir_ids::resolve`).
                let (sid, _) = fhir_ids::resolve(client, table, &id).await.ok()??;
                fhir_ids::instance_canonical(client, table, &sid)
                    .await
                    .ok()?
            })
        })
    }

    fn resource_json_by_id(&self, resource_type: &str, id: &str) -> Option<serde_json::Value> {
        let table = match resource_type {
            "CodeSystem" => "code_systems",
            "ValueSet" => "value_sets",
            "ConceptMap" => "concept_maps",
            _ => return None,
        };
        let id = id.to_owned();
        self.sync_query(move |client| {
            Box::pin(async move {
                if table == "concept_maps" {
                    let row = client
                        .query_opt(
                            "SELECT resource_json::text FROM concept_maps WHERE id = $1",
                            &[&id],
                        )
                        .await
                        .ok()??;
                    let raw: Option<String> = row.get(0);
                    return raw.and_then(|s| serde_json::from_str(&s).ok());
                }
                let (sid, reported) = fhir_ids::resolve(client, table, &id).await.ok()??;
                let row = client
                    .query_opt(
                        &format!("SELECT resource_json::text FROM {table} WHERE id = $1"),
                        &[&sid],
                    )
                    .await
                    .ok()??;
                let raw: Option<String> = row.get(0);
                let mut resource: serde_json::Value = serde_json::from_str(&raw?).ok()?;
                crate::resource_ids::apply_id(&mut resource, &reported);
                Some(resource)
            })
        })
    }
}

// ── BundleImportBackend ────────────────────────────────────────────────────────

impl PostgresTerminologyBackend {
    /// Write a bundle with a savepoint per resource: one failing statement
    /// aborts the whole PostgreSQL transaction, so without them a single bad
    /// resource would discard every other one in the bundle. With them a
    /// failure drops just that resource and is reported, like SQLite.
    async fn import_guarded(
        &self,
        client: &mut deadpool_postgres::Object,
        parsed: &bundle_parser::ParsedBundle,
    ) -> Result<ImportStats, HtsError> {
        let mut tx = client.transaction().await.map_err(|e| {
            HtsError::StorageError(format!("Begin transaction: {}", error_chain(&e)))
        })?;

        let mut stats = ImportStats::default();
        stats.errors.extend(parsed.parse_errors.iter().cloned());

        macro_rules! guarded {
            ($kind:literal, $url:expr, |$c:ident| $write:expr) => {{
                let sp = tx.savepoint("hts_resource").await.map_err(|e| {
                    HtsError::StorageError(format!("Savepoint: {}", error_chain(&e)))
                })?;
                let before = (
                    stats.code_systems,
                    stats.value_sets,
                    stats.concept_maps,
                    stats.concepts,
                );
                let res = {
                    let $c = &sp;
                    $write.await
                };
                match res {
                    Ok(()) => sp.commit().await.map_err(|e| {
                        HtsError::StorageError(format!("Release savepoint: {}", error_chain(&e)))
                    })?,
                    Err(e) => {
                        sp.rollback().await.map_err(|e| {
                            HtsError::StorageError(format!(
                                "Rollback to savepoint: {}",
                                error_chain(&e)
                            ))
                        })?;
                        (
                            stats.code_systems,
                            stats.value_sets,
                            stats.concept_maps,
                            stats.concepts,
                        ) = before;
                        stats
                            .errors
                            .push(format!("{} '{}' import failed: {e}", $kind, $url));
                    }
                }
            }};
        }

        for cs in &parsed.code_systems {
            let fresh = parsed.fresh_load;
            guarded!("CodeSystem", cs.url, |c| write_code_system(
                c, cs, &mut stats, fresh
            ));
        }
        for vs in &parsed.value_sets {
            guarded!("ValueSet", vs.url, |c| write_value_set(c, vs, &mut stats));
        }
        for cm in &parsed.concept_maps {
            guarded!("ConceptMap", cm.url, |c| write_concept_map(
                c, cm, &mut stats
            ));
        }

        tx.commit().await.map_err(|e| {
            HtsError::StorageError(format!("Commit transaction: {}", error_chain(&e)))
        })?;
        Ok(stats)
    }
}

#[async_trait]
impl BundleImportBackend for PostgresTerminologyBackend {
    /// Parse a FHIR Bundle via the shared [`bundle_parser`] and write all
    /// contained resources into the PostgreSQL normalized tables.
    async fn import_bundle(
        &self,
        ctx: &TenantContext,
        data: &[u8],
    ) -> Result<ImportStats, HtsError> {
        let parsed = bundle_parser::parse_bundle(data)?;
        self.import_parsed(ctx, parsed).await
    }

    /// Write an already-parsed bundle, skipping the JSON parse step.
    async fn import_parsed(
        &self,
        _ctx: &TenantContext,
        parsed: bundle_parser::ParsedBundle,
    ) -> Result<ImportStats, HtsError> {
        let mut client = self
            .pool
            .get()
            .await
            .map_err(|e| HtsError::StorageError(format!("Pool error: {}", error_chain(&e))))?;

        // Before the transaction: record which CodeSystems currently have zero
        // concepts in the DB so the post-commit closure build only touches
        // newly-imported (or empty-stub) systems. Systems being updated in a
        // batch import (e.g. SNOMED RF2 chunks) skip the per-batch rebuild
        // because rebuilding per chunk is O(n²) — the SNOMED full closure
        // is built once via the end-of-CLI `migrate_concept_closure_pg` call.
        // Mirrors the SQLite import_bundle pattern.
        // Probed with EXISTS rather than COUNT(*): the answer only
        // distinguishes empty from non-empty, and a COUNT over an
        // ever-growing concept set would make each batch of a chunked bulk
        // load scan all previously imported rows.
        let mut systems_needing_closure: Vec<String> = Vec::new();
        for cs in &parsed.code_systems {
            let row = client
                .query_opt(
                    "SELECT EXISTS(
                         SELECT 1 FROM concepts c
                         JOIN code_systems s ON c.system_id = s.id
                         WHERE s.url = $1
                     )",
                    &[&cs.url],
                )
                .await
                .map_err(|e| HtsError::StorageError(error_chain(&e)))?;
            let has_concepts: bool = row.map(|r| r.get(0)).unwrap_or(false);
            if !has_concepts {
                systems_needing_closure.push(cs.url.clone());
            }
        }

        // First attempt: the whole bundle in one transaction without
        // savepoints (two round trips per resource fewer). Only when a
        // resource fails is the bundle rolled back and written again with a
        // savepoint per resource, below.
        let optimistic = {
            let tx = client.transaction().await.map_err(|e| {
                HtsError::StorageError(format!("Begin transaction: {}", error_chain(&e)))
            })?;
            let mut stats = ImportStats::default();
            stats.errors.extend(parsed.parse_errors.iter().cloned());
            let res: Result<(), HtsError> = async {
                for cs in &parsed.code_systems {
                    write_code_system(&tx, cs, &mut stats, parsed.fresh_load).await?;
                }
                for vs in &parsed.value_sets {
                    write_value_set(&tx, vs, &mut stats).await?;
                }
                for cm in &parsed.concept_maps {
                    write_concept_map(&tx, cm, &mut stats).await?;
                }
                Ok(())
            }
            .await;
            match res {
                Ok(()) => {
                    tx.commit().await.map_err(|e| {
                        HtsError::StorageError(format!("Commit transaction: {}", error_chain(&e)))
                    })?;
                    Some(stats)
                }
                Err(_) => {
                    tx.rollback().await.map_err(|e| {
                        HtsError::StorageError(format!("Rollback: {}", error_chain(&e)))
                    })?;
                    None
                }
            }
        };

        let stats = match optimistic {
            Some(stats) => stats,
            None => self.import_guarded(&mut client, &parsed).await?,
        };

        // Unique reported ids across the versions sharing each written id.
        let cs_ids: Vec<String> = parsed.code_systems.iter().map(|cs| cs.id.clone()).collect();
        let vs_ids: Vec<String> = parsed.value_sets.iter().map(|vs| vs.id.clone()).collect();
        fhir_ids::reassign(&**client, "code_systems", &cs_ids).await?;
        fhir_ids::reassign(&**client, "value_sets", &vs_ids).await?;

        // Closure maintenance, as SQLite does it (`import_parsed_sync`):
        // `write_code_system` dropped the closure of every row it wrote.
        // Systems that were empty before the bundle are rebuilt now. Rows of
        // an already-populated URL are rebuilt now too when their closure is
        // gone and the hierarchy is small (a re-import or a new version);
        // above `CLOSURE_REBUILD_INLINE_MAX_EDGES` edges the system is a
        // chunked bulk load (SNOMED CT, LOINC), whose closure is built once at
        // the end of the CLI import / on the next start instead of per batch.
        //
        // A URL may map to several `code_systems` rows (versions sharing a
        // canonical URL); each row with hierarchy edges gets its own closure.
        let mut rebuilt: HashSet<String> = HashSet::new();
        for cs in &parsed.code_systems {
            let rows = client
                .query("SELECT id FROM code_systems WHERE url = $1", &[&cs.url])
                .await
                .map_err(|e| HtsError::StorageError(error_chain(&e)))?;
            for row in rows {
                let sid: String = row.get(0);
                if !rebuilt.insert(sid.clone()) {
                    continue;
                }
                let (edges, has_closure): (i64, bool) = client
                    .query_one(
                        "SELECT (SELECT COUNT(*) FROM (SELECT 1 FROM concept_hierarchy
                                   WHERE system_id = $1 LIMIT $2) e),
                                EXISTS(SELECT 1 FROM concept_closure WHERE system_id = $1)",
                        &[&sid, &(CLOSURE_REBUILD_INLINE_MAX_EDGES + 1)],
                    )
                    .await
                    .map(|r| (r.get(0), r.get(1)))
                    .map_err(|e| HtsError::StorageError(error_chain(&e)))?;
                if edges == 0 {
                    continue;
                }
                let wanted = if systems_needing_closure.contains(&cs.url) {
                    true
                } else {
                    !parsed.chunked && !has_closure && edges <= CLOSURE_REBUILD_INLINE_MAX_EDGES
                };
                if wanted {
                    if let Err(e) = schema::build_concept_closure_pg(&mut client, &sid).await {
                        tracing::warn!(
                            system_id = %sid,
                            error = %error_chain(&e),
                            "Failed to build concept_closure after import"
                        );
                    }
                }
            }
        }

        // Invalidate per-instance response caches — they may now shadow
        // newly-imported codes.
        self.invalidate_caches();

        Ok(stats)
    }

    async fn code_system_has_concepts(
        &self,
        _ctx: &TenantContext,
        url: &str,
    ) -> Result<bool, HtsError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| HtsError::StorageError(format!("Pool error: {}", error_chain(&e))))?;
        let row = client
            .query_one(
                "SELECT EXISTS(
                     SELECT 1 FROM concepts c
                     JOIN code_systems s ON c.system_id = s.id
                     WHERE s.url = $1
                 )",
                &[&url],
            )
            .await
            .map_err(|e| HtsError::StorageError(error_chain(&e)))?;
        Ok(row.get(0))
    }

    /// Replace the `refset_members` rows of `url|version` in one transaction.
    async fn import_refset_members(
        &self,
        _ctx: &TenantContext,
        url: &str,
        version: Option<&str>,
        members: Vec<(String, String)>,
    ) -> Result<u64, HtsError> {
        let mut client = self
            .pool
            .get()
            .await
            .map_err(|e| HtsError::StorageError(format!("Pool error: {}", error_chain(&e))))?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| HtsError::StorageError(error_chain(&e)))?;
        let system_id: Option<String> = tx
            .query_opt(
                "SELECT id FROM code_systems \
                 WHERE url = $1 AND COALESCE(version, '') = COALESCE($2, '')",
                &[&url, &version],
            )
            .await
            .map_err(|e| HtsError::StorageError(error_chain(&e)))?
            .map(|r| r.get(0));
        let Some(system_id) = system_id else {
            return Err(HtsError::NotFound(format!(
                "CodeSystem {url}|{} is not imported; cannot store its reference set members",
                version.unwrap_or("")
            )));
        };
        tx.execute(
            "DELETE FROM refset_members WHERE system_id = $1",
            &[&system_id],
        )
        .await
        .map_err(|e| HtsError::StorageError(error_chain(&e)))?;
        let mut written = 0u64;
        for chunk in members.chunks(50_000) {
            let refsets: Vec<&str> = chunk.iter().map(|(r, _)| r.as_str()).collect();
            let codes: Vec<&str> = chunk.iter().map(|(_, c)| c.as_str()).collect();
            written += tx
                .execute(
                    "INSERT INTO refset_members (system_id, refset_id, referenced_code)
                     SELECT $1, r, c FROM UNNEST($2::text[], $3::text[]) AS t(r, c)
                     ON CONFLICT DO NOTHING",
                    &[&system_id, &refsets, &codes],
                )
                .await
                .map_err(|e| HtsError::StorageError(error_chain(&e)))?;
        }
        tx.commit()
            .await
            .map_err(|e| HtsError::StorageError(error_chain(&e)))?;
        // Cached expansions may hold ECL `^` results.
        self.invalidate_caches();
        Ok(written)
    }

    async fn write_concept_properties(
        &self,
        _ctx: &TenantContext,
        url: &str,
        version: Option<&str>,
        clear: &[String],
        properties: Vec<crate::import::PropertyRow>,
        designations: Vec<(String, String, String)>,
    ) -> Result<(u64, u64), HtsError> {
        let db = |e: tokio_postgres::Error| HtsError::StorageError(error_chain(&e));
        let mut client = self
            .pool
            .get()
            .await
            .map_err(|e| HtsError::StorageError(format!("Pool error: {}", error_chain(&e))))?;
        let tx = client.transaction().await.map_err(db)?;
        let ids: Vec<String> = match version {
            Some(v) => tx
                .query(
                    "SELECT id FROM code_systems WHERE url = $1 AND version = $2",
                    &[&url, &v],
                )
                .await
                .map_err(db)?,
            None => tx
                .query("SELECT id FROM code_systems WHERE url = $1", &[&url])
                .await
                .map_err(db)?,
        }
        .into_iter()
        .map(|r| r.get(0))
        .collect();
        let system_id = match ids.as_slice() {
            [id] => id.clone(),
            [] => {
                return Err(HtsError::NotFound(format!(
                    "CodeSystem {url}{} is not imported",
                    version.map(|v| format!("|{v}")).unwrap_or_default()
                )));
            }
            _ => {
                return Err(HtsError::InvalidRequest(format!(
                    "CodeSystem {url} is stored in {} versions; name the version",
                    ids.len()
                )));
            }
        };
        if !clear.is_empty() {
            tx.execute(
                "DELETE FROM concept_properties cp USING concepts c
                  WHERE cp.concept_id = c.id AND c.system_id = $1
                    AND cp.property = ANY($2::text[])",
                &[&system_id, &clear],
            )
            .await
            .map_err(db)?;
        }
        let mut props = 0u64;
        for chunk in properties.chunks(50_000) {
            let codes: Vec<&str> = chunk.iter().map(|r| r.code.as_str()).collect();
            let names: Vec<&str> = chunk.iter().map(|r| r.property.as_str()).collect();
            let types: Vec<&str> = chunk.iter().map(|r| r.value_type.as_str()).collect();
            let values: Vec<&str> = chunk.iter().map(|r| r.value.as_str()).collect();
            props += tx
                .execute(
                    "INSERT INTO concept_properties (concept_id, property, value_type, value)
                     SELECT c.id, t.p, t.vt, t.v
                       FROM UNNEST($2::text[], $3::text[], $4::text[], $5::text[])
                            AS t(code, p, vt, v)
                       JOIN concepts c ON c.system_id = $1 AND c.code = t.code",
                    &[&system_id, &codes, &names, &types, &values],
                )
                .await
                .map_err(db)?;
        }
        let mut desigs = 0u64;
        for chunk in designations.chunks(50_000) {
            let codes: Vec<&str> = chunk.iter().map(|d| d.0.as_str()).collect();
            let langs: Vec<&str> = chunk.iter().map(|d| d.1.as_str()).collect();
            let values: Vec<&str> = chunk.iter().map(|d| d.2.as_str()).collect();
            desigs += tx
                .execute(
                    "INSERT INTO concept_designations (concept_id, language, value)
                     SELECT DISTINCT ON (c.id, t.l, lower(t.v)) c.id, t.l, t.v
                       FROM UNNEST($2::text[], $3::text[], $4::text[]) AS t(code, l, v)
                       JOIN concepts c ON c.system_id = $1 AND c.code = t.code
                      WHERE lower(COALESCE(c.display, '')) <> lower(t.v)
                        AND NOT EXISTS (SELECT 1 FROM concept_designations d
                                         WHERE d.concept_id = c.id
                                           AND d.language IS NOT DISTINCT FROM t.l
                                           AND lower(d.value) = lower(t.v))",
                    &[&system_id, &codes, &langs, &values],
                )
                .await
                .map_err(db)?;
        }
        let wrote = !clear.is_empty() || props > 0 || desigs > 0;
        if wrote {
            tx.execute(
                "DELETE FROM value_set_expansions WHERE system_url = $1",
                &[&url],
            )
            .await
            .map_err(db)?;
        }
        tx.commit().await.map_err(db)?;
        if wrote {
            self.invalidate_caches();
        }
        Ok((props, desigs))
    }

    async fn delete_normalized_by_id(&self, resource_type: &str, id: &str) -> Result<(), HtsError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| HtsError::StorageError(format!("Pool error: {}", error_chain(&e))))?;
        let db = |e: tokio_postgres::Error| HtsError::StorageError(error_chain(&e));
        // An id some row reports (unique per stored version) names exactly
        // that row; the plain id then moves to the next default version.
        let table = match resource_type {
            "CodeSystem" => Some("code_systems"),
            "ValueSet" => Some("value_sets"),
            _ => None,
        };
        if let Some(table) = table
            && let Some((sid, declared)) = fhir_ids::reported_row(&**client, table, id).await?
        {
            if table == "code_systems" {
                client
                    .execute(
                        "DELETE FROM value_set_expansions WHERE system_url IN \
                         (SELECT url COLLATE \"C\" FROM code_systems WHERE id = $1)",
                        &[&sid],
                    )
                    .await
                    .map_err(db)?;
            }
            client
                .execute(&format!("DELETE FROM {table} WHERE id = $1"), &[&sid])
                .await
                .map_err(db)?;
            if let Some(declared) = declared {
                fhir_ids::reassign(&**client, table, &[declared]).await?;
            }
            self.invalidate_caches();
            return Ok(());
        }
        match resource_type {
            "CodeSystem" => {
                // Expansions cached over this system are DB state too.
                client
                    .execute(
                        "DELETE FROM value_set_expansions WHERE system_url IN \
                         (SELECT url COLLATE \"C\" FROM code_systems \
                          WHERE id = $1 OR resource_json->>'id' = $1)",
                        &[&id],
                    )
                    .await
                    .map_err(db)?;
                client
                    .execute(
                        "DELETE FROM code_systems WHERE id = $1 OR resource_json->>'id' = $1",
                        &[&id],
                    )
                    .await
                    .map_err(db)?;
            }
            "ValueSet" => {
                client
                    .execute(
                        "DELETE FROM value_sets WHERE id = $1 OR split_part(id, '|', 1) = $1 \
                         AND position('|' in id) > 0",
                        &[&id],
                    )
                    .await
                    .map_err(db)?;
            }
            "ConceptMap" => {
                client
                    .execute("DELETE FROM concept_maps WHERE id = $1", &[&id])
                    .await
                    .map_err(db)?;
            }
            _ => {}
        }
        self.invalidate_caches();
        Ok(())
    }

    /// Delete all HTS normalized rows for the resource identified by `resource_url`.
    ///
    /// For CodeSystem: purges expansion cache entries that include codes from
    /// this system, then deletes the code_system row (FK CASCADE handles
    /// concepts, hierarchy, properties, designations).
    ///
    /// For ValueSet: deletes the value_set row (FK CASCADE handles expansions).
    ///
    /// For ConceptMap: deletes the concept_map row (FK CASCADE handles elements).
    async fn delete_normalized(
        &self,
        resource_type: &str,
        resource_url: &str,
    ) -> Result<(), HtsError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| HtsError::StorageError(format!("Pool error: {}", error_chain(&e))))?;

        match resource_type {
            "CodeSystem" => {
                // Invalidate any cached ValueSet expansions that include codes
                // from this system (not covered by FK cascade).
                client
                    .execute(
                        "DELETE FROM value_set_expansions WHERE system_url = $1",
                        &[&resource_url],
                    )
                    .await
                    .map_err(|e| HtsError::StorageError(error_chain(&e)))?;
                // Delete the system row; ON DELETE CASCADE removes concepts,
                // concept_hierarchy, concept_properties, concept_designations.
                client
                    .execute("DELETE FROM code_systems WHERE url = $1", &[&resource_url])
                    .await
                    .map_err(|e| HtsError::StorageError(error_chain(&e)))?;
            }
            "ValueSet" => {
                // ON DELETE CASCADE removes value_set_expansions.
                client
                    .execute("DELETE FROM value_sets WHERE url = $1", &[&resource_url])
                    .await
                    .map_err(|e| HtsError::StorageError(error_chain(&e)))?;
            }
            "ConceptMap" => {
                // ON DELETE CASCADE removes concept_map_elements.
                client
                    .execute("DELETE FROM concept_maps WHERE url = $1", &[&resource_url])
                    .await
                    .map_err(|e| HtsError::StorageError(error_chain(&e)))?;
            }
            _ => {} // Unknown resource type — nothing to clean up.
        }

        // Invalidate per-instance response caches — they may reference the
        // now-deleted resource.
        self.invalidate_caches();

        Ok(())
    }
}

// ── PostgreSQL write helpers ───────────────────────────────────────────────────

fn utc_now() -> String {
    chrono::Utc::now().to_rfc3339()
}

async fn write_code_system(
    client: &impl GenericClient,
    cs: &ParsedCodeSystem,
    stats: &mut ImportStats,
    // When `true` the caller guarantees this code system had no concepts before
    // the import session, so the per-concept delete-before-reinsert of
    // properties/designations is skipped (fast path for first-time bulk loads).
    // See [`crate::import::bundle_parser::ParsedBundle::fresh_load`].
    skip_child_delete: bool,
) -> Result<(), HtsError> {
    let resource_json = Some(cs.resource_json.clone());
    let now = utc_now();

    // Synthetic storage id encodes the version so multiple versions of the
    // same canonical URL coexist even when they share the same FHIR `id`
    // (tx-ecosystem `version/codesystem-version-1.json` + `-2.json` both ship
    // `"id":"version"`). When two DISTINCT CodeSystems share both fhir-id
    // AND version (e.g. two unrelated CSes ship id="status" with no
    // version), reuse the existing row for the matching (url, version) or
    // mint a fresh UUID rather than letting the second import silently
    // disappear via ON CONFLICT DO NOTHING. Mirrors
    // sqlite/import/fhir_bundle.rs:189-215.
    let preferred_id = crate::import::fhir_bundle::storage_id_for(&cs.id, cs.version.as_deref());
    let existing_for_url_version: Option<String> = client
        .cquery_opt(
            "SELECT id FROM code_systems \
             WHERE url = $1 AND COALESCE(version, '') = COALESCE($2, '')",
            &[&cs.url, &cs.version],
        )
        .await
        .map_err(|e| HtsError::StorageError(error_chain(&e)))?
        .and_then(|r| r.get::<_, Option<String>>(0));
    let storage_id = if let Some(id) = existing_for_url_version {
        id
    } else {
        let preferred_taken: bool = client
            .cquery_one(
                "SELECT EXISTS(SELECT 1 FROM code_systems WHERE id = $1)",
                &[&preferred_id],
            )
            .await
            .map(|r| r.get::<_, bool>(0))
            .unwrap_or(false);
        if preferred_taken {
            uuid::Uuid::new_v4().to_string()
        } else {
            preferred_id
        }
    };

    // Upsert keyed on (url, version). The composite UNIQUE index
    // `idx_code_systems_url_version` is the conflict arbiter; the legacy
    // `ON CONFLICT (url)` is no longer applicable now that two rows can share
    // a URL. `ON CONFLICT DO NOTHING` keeps a prior row in place; the UPDATE
    // below then refreshes its mutable columns by (url, version).
    client
        .cexec(
            "INSERT INTO code_systems
             (id, url, version, name, title, status, content, resource_json, created_at, updated_at,
              authority_rank, version_sort)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9, $10, $11)
             ON CONFLICT DO NOTHING",
            &[
                &storage_id,
                &cs.url,
                &cs.version,
                &cs.name,
                &cs.title,
                &cs.status,
                &cs.content,
                &resource_json,
                &now,
                &cs.authority_rank,
                &version_sort_key(cs.version.as_deref()),
            ],
        )
        .await
        .map_err(|e| HtsError::StorageError(error_chain(&e)))?;

    // authority_rank keeps the strongest claim ever asserted for this row; the
    // COALESCE sentinel (9 — above any real rank) means a row that predates the
    // column ("never claimed") is overwritten by the first source to claim it.
    // See the SQLite twin in `import/fhir_bundle.rs::write_code_system`.
    let cs_update = format!(
        "UPDATE code_systems SET
           name           = $1,
           title          = $2,
           status         = $3,
           content        = $4,
           resource_json  = $5,
           updated_at     = $6,
           authority_rank = LEAST(COALESCE(authority_rank, {unclaimed}), $9),
           version_sort   = $10
         WHERE url = $7 AND COALESCE(version, '') = COALESCE($8, '')
         RETURNING id",
        unclaimed = bundle_parser::AUTHORITY_UNCLAIMED
    );
    let cs_rows = client
        .cquery(
            &cs_update,
            &[
                &cs.name,
                &cs.title,
                &cs.status,
                &cs.content,
                &resource_json,
                &now,
                &cs.url,
                &cs.version,
                &cs.authority_rank,
                &version_sort_key(cs.version.as_deref()),
            ],
        )
        .await
        .map_err(|e| HtsError::StorageError(error_chain(&e)))?;

    let system_id: String = cs_rows
        .into_iter()
        .next()
        .ok_or_else(|| {
            HtsError::StorageError("UPDATE code_systems RETURNING id got no row".into())
        })?
        .get(0);

    // Concepts and their child rows go in set-based batches (one UNNEST
    // statement per table per batch) instead of one round trip per row: a
    // SNOMED CT import is ~4 M rows. A batch never holds the same code twice,
    // so each batch behaves exactly like the per-concept sequence it replaces
    // (upsert, drop the old child rows, insert the new ones).
    let mut batch: Vec<&ParsedConcept> = Vec::with_capacity(CONCEPT_BATCH);
    let mut batch_codes: HashSet<&str> = HashSet::with_capacity(CONCEPT_BATCH);
    for concept in &cs.concepts {
        if batch.len() >= CONCEPT_BATCH || batch_codes.contains(concept.code.as_str()) {
            write_concept_batch(client, &system_id, &batch, skip_child_delete).await?;
            stats.concepts += batch.len() as u32;
            batch.clear();
            batch_codes.clear();
        }
        batch_codes.insert(concept.code.as_str());
        batch.push(concept);
    }
    if !batch.is_empty() {
        write_concept_batch(client, &system_id, &batch, skip_child_delete).await?;
        stats.concepts += batch.len() as u32;
    }

    // Invalidate stale closure rows so the post-import / startup migration
    // knows to (re)build the full closure once all batches are loaded. Without
    // this, a partial closure from an earlier batch would be mistakenly treated
    // as complete. Mirrors sqlite/import/fhir_bundle.rs.
    client
        .cexec(
            "DELETE FROM concept_closure WHERE system_id = $1",
            &[&system_id],
        )
        .await
        .map_err(|e| HtsError::StorageError(error_chain(&e)))?;

    stats.code_systems += 1;
    Ok(())
}

/// Largest hierarchy (parent→child edges) whose closure is rebuilt inline
/// after a bundle import of an already-populated system; the SQLite constant
/// of the same name.
const CLOSURE_REBUILD_INLINE_MAX_EDGES: i64 = 250_000;

/// Concepts per set-based write batch (see [`write_code_system`]).
const CONCEPT_BATCH: usize = 5_000;

/// Upsert `batch` (distinct codes) and replace its properties, designations
/// and hierarchy edges, one statement per table.
async fn write_concept_batch(
    client: &impl GenericClient,
    system_id: &str,
    batch: &[&ParsedConcept],
    skip_child_delete: bool,
) -> Result<(), HtsError> {
    let db = |e: tokio_postgres::Error| HtsError::StorageError(error_chain(&e));
    let codes: Vec<&str> = batch.iter().map(|c| c.code.as_str()).collect();
    let displays: Vec<Option<&str>> = batch.iter().map(|c| c.display.as_deref()).collect();
    let definitions: Vec<Option<&str>> = batch.iter().map(|c| c.definition.as_deref()).collect();
    let rows = client
        .cquery(
            "INSERT INTO concepts (system_id, code, display, definition)
             SELECT $1, code, display, definition
             FROM UNNEST($2::text[], $3::text[], $4::text[]) AS t(code, display, definition)
             ON CONFLICT (system_id, code) DO UPDATE SET
               display    = EXCLUDED.display,
               definition = EXCLUDED.definition
             RETURNING id, code",
            &[&system_id, &codes, &displays, &definitions],
        )
        .await
        .map_err(db)?;
    let ids: HashMap<String, i64> = rows
        .into_iter()
        .map(|r| (r.get::<_, String>(1), r.get::<_, i64>(0)))
        .collect();
    let id_of = |code: &str| -> Result<i64, HtsError> {
        ids.get(code).copied().ok_or_else(|| {
            HtsError::StorageError(format!("INSERT concepts RETURNING id missed code {code}"))
        })
    };

    if !skip_child_delete {
        let all: Vec<i64> = ids.values().copied().collect();
        client
            .cexec(
                "DELETE FROM concept_properties WHERE concept_id = ANY($1)",
                &[&all],
            )
            .await
            .map_err(db)?;
        client
            .cexec(
                "DELETE FROM concept_designations WHERE concept_id = ANY($1)",
                &[&all],
            )
            .await
            .map_err(db)?;
    }

    let mut parents: Vec<&str> = Vec::new();
    let mut children: Vec<&str> = Vec::new();
    let (mut p_ids, mut p_codes, mut p_types, mut p_values) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut d_ids, mut d_langs, mut d_systems, mut d_codes, mut d_values) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for concept in batch {
        let concept_id = id_of(&concept.code)?;
        if let Some(ref parent) = concept.parent_code {
            parents.push(parent.as_str());
            children.push(concept.code.as_str());
        }
        for prop in &concept.properties {
            if prop.value.is_empty() {
                continue;
            }
            p_ids.push(concept_id);
            p_codes.push(prop.code.as_str());
            p_types.push(prop.value_type.as_str());
            p_values.push(prop.value.as_str());
            if prop.is_parent_edge {
                if let Some(ref pv) = prop.parent_code_value {
                    parents.push(pv.as_str());
                    children.push(concept.code.as_str());
                }
            }
        }
        for desig in &concept.designations {
            d_ids.push(concept_id);
            d_langs.push(desig.language.as_deref());
            d_systems.push(desig.use_system.as_deref());
            d_codes.push(desig.use_code.as_deref());
            d_values.push(desig.value.as_str());
        }
    }
    if !parents.is_empty() {
        client
            .cexec(
                "INSERT INTO concept_hierarchy (system_id, parent_code, child_code)
                 SELECT $1, p, c FROM UNNEST($2::text[], $3::text[]) AS t(p, c)
                 ON CONFLICT DO NOTHING",
                &[&system_id, &parents, &children],
            )
            .await
            .map_err(db)?;
    }
    if !p_ids.is_empty() {
        client
            .cexec(
                "INSERT INTO concept_properties (concept_id, property, value_type, value)
                 SELECT * FROM UNNEST($1::bigint[], $2::text[], $3::text[], $4::text[])",
                &[&p_ids, &p_codes, &p_types, &p_values],
            )
            .await
            .map_err(db)?;
    }
    if !d_ids.is_empty() {
        client
            .cexec(
                "INSERT INTO concept_designations (concept_id, language, use_system, use_code, value)
                 SELECT * FROM UNNEST($1::bigint[], $2::text[], $3::text[], $4::text[], $5::text[])",
                &[&d_ids, &d_langs, &d_systems, &d_codes, &d_values],
            )
            .await
            .map_err(db)?;
    }
    Ok(())
}

async fn write_value_set(
    client: &impl GenericClient,
    vs: &ParsedValueSet,
    stats: &mut ImportStats,
) -> Result<(), HtsError> {
    let resource_json = Some(vs.resource_json.clone());
    let now = utc_now();

    // Invalidate the expansion cache of the (url, version) row being
    // re-imported — not of whichever version of the url comes first.
    client
        .cexec(
            "DELETE FROM value_set_expansions WHERE value_set_id IN \
             (SELECT id FROM value_sets \
              WHERE url = $1 AND COALESCE(version, '') = COALESCE($2, ''))",
            &[&vs.url, &vs.version],
        )
        .await
        .map_err(|e| HtsError::StorageError(error_chain(&e)))?;

    // Synthetic storage id: `<fhir-id>|<version>` (or `<fhir-id>` when
    // version is absent). Mirrors the code_systems strategy so multiple
    // ValueSets that share a canonical URL but differ in version don't
    // collide on the primary key. When two distinct VSes share both a
    // fhir-id AND a version (e.g. tx-ecosystem ships several VSes whose
    // `id` field is reused across DIFFERENT canonical URLs like
    // `withdrawn`, `simple-import-bad`, etc.), reuse the existing row
    // for the matching (url, version) or mint a fresh UUID — otherwise
    // the second import gets silently dropped by ON CONFLICT DO NOTHING.
    // Mirrors sqlite/import/fhir_bundle.rs:443-478.
    let preferred_id = crate::import::fhir_bundle::storage_id_for(&vs.id, vs.version.as_deref());
    let existing_for_url_version: Option<String> = client
        .cquery_opt(
            "SELECT id FROM value_sets \
             WHERE url = $1 AND COALESCE(version, '') = COALESCE($2, '')",
            &[&vs.url, &vs.version],
        )
        .await
        .map_err(|e| HtsError::StorageError(error_chain(&e)))?
        .and_then(|r| r.get::<_, Option<String>>(0));
    let storage_id = if let Some(id) = existing_for_url_version {
        id
    } else {
        let preferred_taken: bool = client
            .cquery_one(
                "SELECT EXISTS(SELECT 1 FROM value_sets WHERE id = $1)",
                &[&preferred_id],
            )
            .await
            .map(|r| r.get::<_, bool>(0))
            .unwrap_or(false);
        if preferred_taken {
            uuid::Uuid::new_v4().to_string()
        } else {
            preferred_id
        }
    };

    client
        .cexec(
            "INSERT INTO value_sets
             (id, url, version, name, title, status, compose_json, resource_json, created_at, updated_at,
              authority_rank, version_sort)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9, $10, $11)
             ON CONFLICT DO NOTHING",
            &[
                &storage_id,
                &vs.url,
                &vs.version,
                &vs.name,
                &vs.title,
                &vs.status,
                &vs.compose_json,
                &resource_json,
                &now,
                &vs.authority_rank,
                &version_sort_key(vs.version.as_deref()),
            ],
        )
        .await
        .map_err(|e| HtsError::StorageError(error_chain(&e)))?;

    // See `write_code_system` for the LEAST/COALESCE-sentinel rationale.
    let vs_update = format!(
        "UPDATE value_sets SET
           name           = $1,
           title          = $2,
           status         = $3,
           compose_json   = $4,
           resource_json  = $5,
           updated_at     = $6,
           authority_rank = LEAST(COALESCE(authority_rank, {unclaimed}), $9),
           version_sort   = $10
         WHERE url = $7 AND COALESCE(version, '') = COALESCE($8, '')",
        unclaimed = bundle_parser::AUTHORITY_UNCLAIMED
    );
    client
        .cexec(
            &vs_update,
            &[
                &vs.name,
                &vs.title,
                &vs.status,
                &vs.compose_json,
                &resource_json,
                &now,
                &vs.url,
                &vs.version,
                &vs.authority_rank,
                &version_sort_key(vs.version.as_deref()),
            ],
        )
        .await
        .map_err(|e| HtsError::StorageError(error_chain(&e)))?;

    stats.value_sets += 1;
    Ok(())
}

async fn write_concept_map(
    client: &impl GenericClient,
    cm: &ParsedConceptMap,
    stats: &mut ImportStats,
) -> Result<(), HtsError> {
    let resource_json = Some(cm.resource_json.clone());
    let now = utc_now();

    // SQLite's `INSERT OR REPLACE`: whatever row holds this id or this url is
    // replaced (its elements go with it through the FK cascade), so the map
    // ends up under the new id even when only the url matched, and an id that
    // moved to a new url does not fail the import.
    client
        .cexec(
            "DELETE FROM concept_maps WHERE id = $1 OR url = $2",
            &[&cm.id, &cm.url],
        )
        .await
        .map_err(|e| HtsError::StorageError(error_chain(&e)))?;
    client
        .cexec(
            "INSERT INTO concept_maps
             (id, url, version, name, title, source_uri, target_uri, status, resource_json, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            &[
                &cm.id,
                &cm.url,
                &cm.version,
                &cm.name,
                &cm.title,
                &cm.source_uri,
                &cm.target_uri,
                &cm.status,
                &resource_json,
                &now,
            ],
        )
        .await
        .map_err(|e| HtsError::StorageError(error_chain(&e)))?;
    let map_id: &str = &cm.id;

    for chunk in cm.elements.chunks(10_000) {
        let src_sys: Vec<&str> = chunk.iter().map(|e| e.source_system.as_str()).collect();
        let src_code: Vec<&str> = chunk.iter().map(|e| e.source_code.as_str()).collect();
        let tgt_sys: Vec<&str> = chunk.iter().map(|e| e.target_system.as_str()).collect();
        let tgt_code: Vec<&str> = chunk.iter().map(|e| e.target_code.as_str()).collect();
        let equiv: Vec<&str> = chunk.iter().map(|e| e.equivalence.as_str()).collect();
        client
            .cexec(
                "INSERT INTO concept_map_elements
                 (map_id, source_system, source_code, target_system, target_code, equivalence)
                 SELECT $1, * FROM UNNEST($2::text[], $3::text[], $4::text[], $5::text[], $6::text[])
                 ON CONFLICT DO NOTHING",
                &[&map_id, &src_sys, &src_code, &tgt_sys, &tgt_code, &equiv],
            )
            .await
            .map_err(|e| HtsError::StorageError(error_chain(&e)))?;
    }

    stats.concept_maps += 1;
    Ok(())
}
