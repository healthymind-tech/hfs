//! Backend-facing terminology management operations.
//!
//! [`TerminologyAdmin`] is the storage seam the admin API and job runner talk
//! to. It is deliberately separate from [`crate::traits::TerminologyBackend`]:
//! the read path must not grow destructive methods, and these operations are
//! coarse, slow and run one at a time from the job queue, while the read
//! traits are tuned for many concurrent small requests.
//!
//! Both storage backends implement it (`backends::sqlite::admin`,
//! `backends::postgres::admin`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::HtsError;

/// One `code_systems` row as the inventory shows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CodeSystemRow {
    /// Storage id (`<fhir-id>|<version>` or a minted id).
    pub id: String,
    pub url: String,
    pub version: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub status: String,
    pub content: String,
    /// `CodeSystem.supplements` when this row is a supplement.
    pub supplements: Option<String>,
    /// An operator pinned this row as current.
    pub pinned: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// Row counts (and a size estimate) for one stored CodeSystem version.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SystemStats {
    pub concepts: u64,
    pub designations: u64,
    pub properties: u64,
    pub hierarchy: u64,
    pub closure: u64,
    /// Estimated bytes occupied by the rows above plus their FTS entries.
    pub est_bytes: u64,
}

impl SystemStats {
    /// Average on-disk bytes per row, measured on a 2 GB production copy with
    /// `dbstat` (table + its indexes; the per-concept figure includes the three
    /// FTS indexes, which dominate). Good to ±30 % — it is labelled an
    /// estimate wherever it is shown.
    pub fn estimate_bytes(&mut self) {
        self.est_bytes = self.concepts * 964
            + self.designations * 200
            + self.properties * 160
            + self.hierarchy * 95
            + self.closure * 130;
    }
}

/// Database file footprint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DbFootprint {
    /// Main database file size (SQLite) or `pg_database_size` (Postgres).
    pub bytes: u64,
    /// Bytes on the free list that `VACUUM` would return to the filesystem.
    pub reclaimable_bytes: u64,
    /// WAL file size, SQLite only.
    pub wal_bytes: u64,
    /// Path of the database file, when it is a file.
    pub path: Option<String>,
}

/// Resource that refers to a CodeSystem version and would break (or change
/// meaning) if it were deleted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DependentRef {
    /// `ValueSet`, `CodeSystem` (supplement) or `ConceptMap`.
    pub resource_type: String,
    pub id: String,
    pub url: String,
    pub version: Option<String>,
    /// Why it depends: `compose.include`, `supplements`, `group.source` …
    pub relation: String,
    /// `true` when the reference pins this exact version; `false` when it is
    /// version-less and will silently fall back to another stored version.
    pub pins_version: bool,
}

/// Everything the operator should know before deleting a version.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Dependents {
    pub value_sets: Vec<DependentRef>,
    pub supplements: Vec<DependentRef>,
    pub concept_maps: Vec<DependentRef>,
    /// Other stored versions of the same url.
    pub other_versions: Vec<Option<String>>,
    /// Version that will answer version-less requests after the delete
    /// (`None` = no version left; the system disappears).
    pub current_after_delete: Option<Option<String>>,
}

/// Phase/counter sink a long operation reports into. Owned by the job.
#[derive(Debug, Default)]
pub struct OpProgress {
    phase: Mutex<String>,
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
}

impl OpProgress {
    pub fn set_phase(&self, phase: &str) {
        if let Ok(mut p) = self.phase.lock() {
            *p = phase.to_string();
        }
        self.done.store(0, Ordering::Relaxed);
        self.total.store(0, Ordering::Relaxed);
    }
    pub fn phase(&self) -> String {
        self.phase.lock().map(|p| p.clone()).unwrap_or_default()
    }
    pub fn add(&self, n: u64) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }
    pub fn set_total(&self, n: u64) {
        self.total.store(n, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

/// Rows removed by a delete, per table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeleteReport {
    pub concepts: u64,
    pub closure: u64,
    pub hierarchy: u64,
    pub fts: u64,
    pub expansion_cache: u64,
    pub resource_store: u64,
}

/// Storage operations behind the admin API.
#[async_trait]
pub trait TerminologyAdmin: Send + Sync {
    /// Short backend label (`sqlite`, `postgres`).
    fn kind(&self) -> &'static str;

    /// Every stored CodeSystem row (no concept data). Cheap.
    async fn code_system_rows(&self) -> Result<Vec<CodeSystemRow>, HtsError>;

    /// Number of stored ValueSets and ConceptMaps.
    async fn resource_counts(&self) -> Result<(u64, u64), HtsError>;

    /// Per-system row counts keyed by storage id. Scans the concept tables;
    /// callers cache the result.
    async fn system_stats(&self) -> Result<HashMap<String, SystemStats>, HtsError>;

    /// Database footprint.
    async fn footprint(&self) -> Result<DbFootprint, HtsError>;

    /// Resolve `(url, version)` to its stored row. `version = None` matches the
    /// un-versioned row only (not "whatever is current").
    async fn find_code_system(
        &self,
        url: &str,
        version: Option<&str>,
    ) -> Result<Option<CodeSystemRow>, HtsError>;

    /// Resources that reference this CodeSystem version.
    async fn dependents(&self, url: &str, version: Option<&str>) -> Result<Dependents, HtsError>;

    /// Delete one CodeSystem version with its concepts, designations,
    /// properties, hierarchy, closure, FTS rows and derived expansion caches.
    /// Works in bounded batches so the write lock is released between them.
    async fn delete_code_system(
        &self,
        row: &CodeSystemRow,
        progress: std::sync::Arc<OpProgress>,
    ) -> Result<DeleteReport, HtsError>;

    /// Delete one ValueSet version (`version = None` = the un-versioned row).
    /// Returns the number of rows removed (0 = not found).
    async fn delete_value_set(&self, url: &str, version: Option<&str>) -> Result<u64, HtsError>;

    /// Delete one ConceptMap (ConceptMaps are unique on url in both schemas).
    async fn delete_concept_map(&self, url: &str) -> Result<u64, HtsError>;

    /// Pin `version` as current for `url` (clearing any other pin), or clear
    /// the pin when `version` is `None`. Fails when the version is not stored.
    async fn set_current_version(
        &self,
        url: &str,
        version: Option<Option<&str>>,
    ) -> Result<(), HtsError>;

    /// Post-import finalisation, step by step so the job can show phases:
    /// `closure` (rebuild missing transitive closures), `fts` (build missing
    /// full-text indexes), `analyze` (planner statistics), `warm` (in-memory
    /// indexes). Unknown steps are a no-op.
    async fn finalize_step(&self, step: &str) -> Result<(), HtsError>;

    /// Reclaim free pages (`VACUUM`). Blocks every other writer — and, on
    /// SQLite, readers on the same connection — for its duration.
    async fn compact(&self) -> Result<(), HtsError>;

    /// `(url, version)` of every stored CodeSystem — used to undo a cancelled
    /// import by deleting only what it created.
    async fn code_system_keys(&self) -> Result<Vec<(String, Option<String>)>, HtsError> {
        Ok(self
            .code_system_rows()
            .await?
            .into_iter()
            .map(|r| (r.url, r.version))
            .collect())
    }
}

/// Pick which version answers a version-less request among `rows` of one url,
/// mirroring the SQL precedence ([`crate::backends`]'s
/// `cs_precedence_order_by`) for the inventory display: usable content first,
/// populated rows first, then the pin, then semantic version order.
pub fn effective_current<'a>(
    rows: &[&'a CodeSystemRow],
    concepts: &HashMap<String, SystemStats>,
) -> Option<&'a CodeSystemRow> {
    fn content_rank(c: &str) -> u8 {
        match c {
            "complete" | "supplement" => 0,
            "not-present" => 2,
            _ => 1,
        }
    }
    let mut v: Vec<&CodeSystemRow> = rows.to_vec();
    v.sort_by(|a, b| {
        let ca = concepts.get(&a.id).map(|s| s.concepts).unwrap_or(0) == 0;
        let cb = concepts.get(&b.id).map(|s| s.concepts).unwrap_or(0) == 0;
        content_rank(&a.content)
            .cmp(&content_rank(&b.content))
            .then(ca.cmp(&cb))
            .then(b.pinned.cmp(&a.pinned))
            .then_with(|| {
                crate::version_order::compare_versions(b.version.as_deref(), a.version.as_deref())
            })
            .then(a.id.cmp(&b.id))
    });
    v.first().copied()
}

/// Whether a ValueSet `compose` JSON references `system_url`, and whether it
/// pins `version` (when given). Returns `None` when it does not reference the
/// system at all, `Some(pins)` otherwise.
pub fn compose_references(
    compose_json: &str,
    system_url: &str,
    version: Option<&str>,
) -> Option<bool> {
    let compose: serde_json::Value = serde_json::from_str(compose_json).ok()?;
    let mut found: Option<bool> = None;
    for key in ["include", "exclude"] {
        let Some(items) = compose.get(key).and_then(|v| v.as_array()) else {
            continue;
        };
        for inc in items {
            let Some(sys) = inc.get("system").and_then(|s| s.as_str()) else {
                continue;
            };
            // `system` may carry a `|version` suffix in some producers.
            let (sys_url, sys_ver) = match sys.split_once('|') {
                Some((u, v)) => (u, Some(v)),
                None => (sys, None),
            };
            if sys_url != system_url {
                continue;
            }
            let inc_ver = inc.get("version").and_then(|v| v.as_str()).or(sys_ver);
            let pins = match (inc_ver, version) {
                (Some(iv), Some(v)) => iv == v,
                _ => false,
            };
            // A pinned reference to a *different* version does not depend on
            // this one.
            if let (Some(iv), Some(v)) = (inc_ver, version)
                && iv != v
            {
                continue;
            }
            found = Some(found.unwrap_or(false) || pins);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, version: Option<&str>, pinned: bool) -> CodeSystemRow {
        CodeSystemRow {
            id: id.into(),
            url: "http://x".into(),
            version: version.map(String::from),
            name: None,
            title: None,
            status: "active".into(),
            content: "complete".into(),
            supplements: None,
            pinned,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn stats(ids: &[&str]) -> HashMap<String, SystemStats> {
        ids.iter()
            .map(|id| {
                (
                    id.to_string(),
                    SystemStats {
                        concepts: 1,
                        ..Default::default()
                    },
                )
            })
            .collect()
    }

    #[test]
    fn newest_semantic_version_is_current_unless_pinned() {
        let a = row("a", Some("9.0.0"), false);
        let b = row("b", Some("10.0.0"), false);
        let s = stats(&["a", "b"]);
        assert_eq!(effective_current(&[&a, &b], &s).unwrap().id, "b");
        let a = row("a", Some("9.0.0"), true);
        assert_eq!(effective_current(&[&a, &b], &s).unwrap().id, "a");
    }

    #[test]
    fn pin_on_empty_row_does_not_win() {
        let a = row("a", Some("9.0.0"), true);
        let b = row("b", Some("10.0.0"), false);
        let s = stats(&["b"]);
        assert_eq!(effective_current(&[&a, &b], &s).unwrap().id, "b");
    }

    #[test]
    fn compose_reference_detection() {
        let c = r#"{"include":[{"system":"http://snomed.info/sct","filter":[]}]}"#;
        assert_eq!(
            compose_references(c, "http://snomed.info/sct", Some("1")),
            Some(false)
        );
        assert_eq!(compose_references(c, "http://loinc.org", None), None);
        let pinned = r#"{"include":[{"system":"http://x","version":"2"}]}"#;
        assert_eq!(
            compose_references(pinned, "http://x", Some("2")),
            Some(true)
        );
        assert_eq!(compose_references(pinned, "http://x", Some("1")), None);
    }

    #[test]
    fn estimate_scales_with_rows() {
        let mut s = SystemStats {
            concepts: 1000,
            closure: 10_000,
            ..Default::default()
        };
        s.estimate_bytes();
        assert_eq!(s.est_bytes, 964_000 + 1_300_000);
    }
}
