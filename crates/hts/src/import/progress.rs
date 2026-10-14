//! Progress reporting and cancellation for importer runs.
//!
//! The filesystem importers were written for the `hts import` CLI, where
//! progress is an `eprintln!` per batch. The admin job runner
//! ([`crate::admin`]) drives the *same* importers in the background and needs
//! to show the operator "batch x / y", let them cancel, and apply a few
//! per-job transforms (version override, LOINC property selection) — without
//! forking every importer.
//!
//! Two pieces make that possible:
//!
//! * [`ProgressBackend`] wraps the real [`BundleImportBackend`]. Every batch an
//!   importer writes goes through [`BundleImportBackend::import_parsed`], so the
//!   wrapper sees each one: it counts batches and concepts, checks the cancel
//!   flag (returning an error that aborts the importer at the next batch), and
//!   applies the [`ImportTransforms`].
//! * [`report_batch`] is a one-line hook importers call next to their existing
//!   batch `eprintln!` to publish the *total* batch count they computed. It
//!   reads a task-local [`ImportProgress`] installed by [`with_progress`]; on
//!   the CLI path nothing is installed and the call is a no-op.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use helios_persistence::tenant::TenantContext;

use crate::error::HtsError;
use crate::import::bundle_parser::ParsedBundle;
use crate::import::{BundleImportBackend, ImportStats};

/// Message carried by the error a cancelled import fails with. The job runner
/// matches on the cancel flag, not on this text; it only has to be readable in
/// the job log.
pub const CANCELLED_MESSAGE: &str = "import cancelled by operator";

/// Shared, lock-free progress counters for one import run.
#[derive(Debug, Default)]
pub struct ImportProgress {
    /// Batches written so far (counted by [`ProgressBackend`]).
    pub batches_done: AtomicU64,
    /// Total batches, when the importer reported it via [`report_batch`]
    /// (`0` = unknown).
    pub batches_total: AtomicU64,
    /// Concept rows handed to the backend so far.
    pub concepts: AtomicU64,
    /// Set by the operator; checked before every batch.
    pub cancel: AtomicBool,
    /// Flipped on the first write: everything before it is the parse phase.
    pub writing: AtomicBool,
}

impl ImportProgress {
    /// `true` once cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Request cancellation at the next batch boundary.
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

tokio::task_local! {
    static CURRENT: Arc<ImportProgress>;
}

/// Run `fut` with `progress` installed as the task-local sink that
/// [`report_batch`] publishes to.
pub async fn with_progress<F: std::future::Future>(
    progress: Arc<ImportProgress>,
    fut: F,
) -> F::Output {
    CURRENT.scope(progress, fut).await
}

/// Importer hook: batch `done` of `total` was just written.
///
/// Only the total is taken from here — the done count is maintained by
/// [`ProgressBackend`], which sees every write regardless of whether the
/// importer calls this. A no-op outside [`with_progress`] (the CLI).
pub fn report_batch(done: usize, total: usize) {
    let _ = CURRENT.try_with(|p| {
        let total = total.max(done) as u64;
        p.batches_total.fetch_max(total, Ordering::Relaxed);
    });
}

/// Per-job rewrites applied to every batch before it is written.
#[derive(Debug, Clone, Default)]
pub struct ImportTransforms {
    /// Replace `CodeSystem.version` (column *and* `resource_json`) of every
    /// CodeSystem in the batch. Only offered for single-system formats, where
    /// "the version" is unambiguous.
    pub version_override: Option<String>,
    /// When set, keep only these concept property codes (plus the hierarchy
    /// and status properties every importer relies on). Used to trim LOINC's
    /// ~40 part properties down to the ones a site queries.
    pub keep_properties: Option<Vec<String>>,
}

impl ImportTransforms {
    /// `true` when no rewrite is configured.
    pub fn is_identity(&self) -> bool {
        self.version_override.is_none() && self.keep_properties.is_none()
    }

    /// Apply the configured rewrites to one parsed batch.
    pub fn apply(&self, parsed: &mut ParsedBundle) {
        if let Some(v) = &self.version_override {
            for cs in &mut parsed.code_systems {
                cs.version = Some(v.clone());
                if let Some(obj) = cs.resource_json.as_object_mut() {
                    obj.insert("version".into(), serde_json::Value::String(v.clone()));
                }
            }
        }
        if let Some(keep) = &self.keep_properties {
            for cs in &mut parsed.code_systems {
                for concept in &mut cs.concepts {
                    concept.properties.retain(|p| {
                        p.is_parent_edge
                            || ALWAYS_KEPT_PROPERTIES.contains(&p.code.as_str())
                            || keep.iter().any(|k| k.eq_ignore_ascii_case(&p.code))
                    });
                }
            }
        }
    }
}

/// Properties that drive hierarchy, status and display logic in HTS itself
/// and are therefore never dropped by [`ImportTransforms::keep_properties`].
const ALWAYS_KEPT_PROPERTIES: &[&str] = &[
    "parent",
    "child",
    "status",
    "inactive",
    "notSelectable",
    "deprecated",
    "STATUS",
    "CLASSTYPE",
];

/// A [`BundleImportBackend`] decorator that reports progress, honours
/// cancellation and applies [`ImportTransforms`].
pub struct ProgressBackend {
    inner: Arc<dyn BundleImportBackend>,
    progress: Arc<ImportProgress>,
    transforms: ImportTransforms,
}

impl ProgressBackend {
    /// Wrap `inner`.
    pub fn new(
        inner: Arc<dyn BundleImportBackend>,
        progress: Arc<ImportProgress>,
        transforms: ImportTransforms,
    ) -> Self {
        Self {
            inner,
            progress,
            transforms,
        }
    }

    fn check_cancel(&self) -> Result<(), HtsError> {
        if self.progress.is_cancelled() {
            Err(HtsError::Internal(CANCELLED_MESSAGE.into()))
        } else {
            Ok(())
        }
    }

    fn record(&self, parsed: &ParsedBundle) {
        let concepts: usize = parsed.code_systems.iter().map(|c| c.concepts.len()).sum();
        self.progress.writing.store(true, Ordering::SeqCst);
        // Only concept batches count toward "batch x / y": importers report
        // `y` as their concept-batch total, and the seed write (an empty
        // CodeSystem) or trailing ConceptMap bundles would overshoot it.
        if concepts > 0 {
            self.progress.batches_done.fetch_add(1, Ordering::Relaxed);
            self.progress
                .concepts
                .fetch_add(concepts as u64, Ordering::Relaxed);
        }
    }
}

#[async_trait]
impl BundleImportBackend for ProgressBackend {
    async fn import_bundle(
        &self,
        ctx: &TenantContext,
        data: &[u8],
    ) -> Result<ImportStats, HtsError> {
        self.check_cancel()?;
        if self.transforms.is_identity() {
            self.progress.writing.store(true, Ordering::SeqCst);
            let stats = self.inner.import_bundle(ctx, data).await?;
            // Same rule as `record`: ConceptMap/ValueSet-only bundles (the
            // SNOMED association refsets) are not concept batches.
            if stats.concepts > 0 {
                self.progress.batches_done.fetch_add(1, Ordering::Relaxed);
                self.progress
                    .concepts
                    .fetch_add(u64::from(stats.concepts), Ordering::Relaxed);
            }
            return Ok(stats);
        }
        let parsed = crate::import::bundle_parser::parse_bundle(data)?;
        self.import_parsed(ctx, parsed).await
    }

    async fn import_parsed(
        &self,
        ctx: &TenantContext,
        mut parsed: ParsedBundle,
    ) -> Result<ImportStats, HtsError> {
        self.check_cancel()?;
        self.transforms.apply(&mut parsed);
        self.record(&parsed);
        self.inner.import_parsed(ctx, parsed).await
    }

    async fn code_system_has_concepts(
        &self,
        ctx: &TenantContext,
        url: &str,
    ) -> Result<bool, HtsError> {
        self.inner.code_system_has_concepts(ctx, url).await
    }

    async fn import_refset_members(
        &self,
        ctx: &TenantContext,
        url: &str,
        version: Option<&str>,
        members: Vec<(String, String)>,
    ) -> Result<u64, HtsError> {
        self.check_cancel()?;
        self.inner
            .import_refset_members(ctx, url, version, members)
            .await
    }

    async fn delete_normalized(
        &self,
        resource_type: &str,
        resource_url: &str,
    ) -> Result<(), HtsError> {
        self.inner
            .delete_normalized(resource_type, resource_url)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::bundle_parser::{ParsedCodeSystem, ParsedConcept, ParsedProperty};

    fn prop(code: &str) -> ParsedProperty {
        ParsedProperty {
            code: code.into(),
            value_type: "string".into(),
            value: "x".into(),
            is_parent_edge: false,
            parent_code_value: None,
        }
    }

    fn bundle() -> ParsedBundle {
        ParsedBundle {
            code_systems: vec![ParsedCodeSystem {
                id: "loinc".into(),
                url: "http://loinc.org".into(),
                version: Some("2.80".into()),
                name: None,
                title: None,
                status: "active".into(),
                content: "complete".into(),
                concepts: vec![ParsedConcept {
                    code: "1-8".into(),
                    display: None,
                    definition: None,
                    properties: vec![prop("COMPONENT"), prop("SYSTEM"), prop("STATUS")],
                    designations: vec![],
                    parent_code: None,
                }],
                resource_json: serde_json::json!({"resourceType":"CodeSystem","version":"2.80"}),
                authority_rank: 0,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn version_override_rewrites_column_and_json() {
        let mut b = bundle();
        ImportTransforms {
            version_override: Some("2.80-tw".into()),
            keep_properties: None,
        }
        .apply(&mut b);
        assert_eq!(b.code_systems[0].version.as_deref(), Some("2.80-tw"));
        assert_eq!(b.code_systems[0].resource_json["version"], "2.80-tw");
    }

    #[test]
    fn property_filter_keeps_selection_and_status() {
        let mut b = bundle();
        ImportTransforms {
            version_override: None,
            keep_properties: Some(vec!["component".into()]),
        }
        .apply(&mut b);
        let codes: Vec<&str> = b.code_systems[0].concepts[0]
            .properties
            .iter()
            .map(|p| p.code.as_str())
            .collect();
        assert_eq!(codes, vec!["COMPONENT", "STATUS"]);
    }

    #[tokio::test]
    async fn only_concept_bundles_count_as_batches() {
        let backend = Arc::new(crate::backends::SqliteTerminologyBackend::in_memory().unwrap());
        let progress = Arc::new(ImportProgress::default());
        let pb = ProgressBackend::new(backend, progress.clone(), ImportTransforms::default());
        let ctx = TenantContext::system();
        let cs = br#"{"resourceType":"Bundle","type":"collection","entry":[{"resource":
            {"resourceType":"CodeSystem","url":"http://x","status":"active","content":"complete",
             "concept":[{"code":"a"},{"code":"b"}]}}]}"#;
        let cm = br#"{"resourceType":"Bundle","type":"collection","entry":[{"resource":
            {"resourceType":"ConceptMap","url":"http://x/cm","status":"active",
             "group":[{"source":"http://x","target":"http://y","element":[{"code":"a","target":[{"code":"z","equivalence":"equal"}]}]}]}}]}"#;
        pb.import_bundle(&ctx, cs).await.unwrap();
        pb.import_bundle(&ctx, cm).await.unwrap();
        pb.import_parsed(&ctx, ParsedBundle::default())
            .await
            .unwrap();
        assert_eq!(progress.batches_done.load(Ordering::Relaxed), 1);
        assert_eq!(progress.concepts.load(Ordering::Relaxed), 2);
        progress.request_cancel();
        assert!(
            pb.import_bundle(&ctx, cs).await.is_err(),
            "cancel stops the next batch"
        );
    }

    #[tokio::test]
    async fn report_batch_is_noop_outside_scope_and_records_inside() {
        report_batch(1, 10); // must not panic without a scope
        let p = Arc::new(ImportProgress::default());
        with_progress(p.clone(), async {
            report_batch(1, 10);
            report_batch(2, 10);
        })
        .await;
        assert_eq!(p.batches_total.load(Ordering::Relaxed), 10);
    }
}
