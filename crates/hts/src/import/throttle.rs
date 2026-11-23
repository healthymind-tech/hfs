//! Import throttling for the embedded terminology service (design §8.2,
//! U3 follow-up "isolation p95").
//!
//! A full SNOMED CT / LOINC import writes millions of rows in `INSERT …
//! UNNEST` batches. In one process with FHIR traffic the CPU side is already
//! isolated (import runtime, niced threads, own pool); what remains shared is
//! the PostgreSQL instance and its disk: WAL, checkpoints and lock/IO queues
//! behind the import's commits are what inflate FHIR transaction and
//! `$validate-code` tails. [`ThrottledBackend`] paces the import so the
//! database gets idle time between batches:
//!
//! * **duty cycle** — after each write batch that took `t`, the importer
//!   sleeps `t · (1 − duty) / duty` (`duty = 1` disables pacing);
//! * **adaptive back-off** — an optional probe measures a trivial query on
//!   the *serving* pool after each batch; while it is slower than
//!   `probe_slow`, the pause doubles (up to `max_pause`), and it decays back
//!   once the probe is fast again;
//! * the batch size itself is bounded by the caller (`HFS_TERMINOLOGY_IMPORT_BATCH_SIZE`)
//!   and the import pool runs with `synchronous_commit=off`
//!   (`helios_hts::embedded`), so import commits do not wait for WAL flushes
//!   the FHIR commits are queueing on.
//!
//! An import gets slower; FHIR traffic keeps its latency.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use helios_persistence::tenant::TenantContext;

use crate::error::HtsError;
use crate::import::bundle_parser::ParsedBundle;
use crate::import::{BundleImportBackend, ImportStats, PropertyRow};

/// Measures how long a trivial query takes on the serving pool.
pub type Probe = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Duration> + Send>> + Send + Sync>;

/// How hard to pace an import.
#[derive(Clone)]
pub struct ThrottlePolicy {
    /// Fraction of wall time the importer may spend writing (0 < duty ≤ 1).
    pub duty: f64,
    /// Upper bound of one pause.
    pub max_pause: Duration,
    /// Serving-pool latency above which the pause grows.
    pub probe_slow: Duration,
    /// The serving-pool probe; `None` = duty cycle only.
    pub probe: Option<Probe>,
}

impl std::fmt::Debug for ThrottlePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThrottlePolicy")
            .field("duty", &self.duty)
            .field("max_pause", &self.max_pause)
            .field("probe_slow", &self.probe_slow)
            .field("probe", &self.probe.is_some())
            .finish()
    }
}

impl Default for ThrottlePolicy {
    fn default() -> Self {
        Self {
            duty: 0.5,
            max_pause: Duration::from_secs(2),
            probe_slow: Duration::from_millis(25),
            probe: None,
        }
    }
}

impl ThrottlePolicy {
    /// Whether this policy changes anything.
    pub fn is_active(&self) -> bool {
        self.duty < 1.0 || self.probe.is_some()
    }

    /// The duty-cycle pause after a write that took `elapsed`.
    pub fn base_pause(&self, elapsed: Duration) -> Duration {
        let duty = self.duty.clamp(0.01, 1.0);
        if duty >= 1.0 {
            return Duration::ZERO;
        }
        elapsed.mul_f64((1.0 - duty) / duty).min(self.max_pause)
    }
}

/// A [`BundleImportBackend`] that paces write batches (see the module docs).
pub struct ThrottledBackend {
    inner: Arc<dyn BundleImportBackend>,
    policy: ThrottlePolicy,
    /// Current back-off multiplier ×1000 (1000 = no extra back-off).
    backoff_milli: AtomicU64,
    /// Total time spent pausing (ms), for the job log / tests.
    paused_ms: AtomicU64,
}

impl ThrottledBackend {
    pub fn new(inner: Arc<dyn BundleImportBackend>, policy: ThrottlePolicy) -> Self {
        Self {
            inner,
            policy,
            backoff_milli: AtomicU64::new(1000),
            paused_ms: AtomicU64::new(0),
        }
    }

    /// Milliseconds spent pausing so far.
    pub fn paused_ms(&self) -> u64 {
        self.paused_ms.load(Ordering::Relaxed)
    }

    /// Pause after a write that took `elapsed`.
    async fn pace(&self, elapsed: Duration) {
        let mut backoff = self.backoff_milli.load(Ordering::Relaxed);
        if let Some(probe) = &self.policy.probe {
            let latency = probe().await;
            backoff = if latency > self.policy.probe_slow {
                (backoff * 2).min(64_000)
            } else {
                (backoff * 3 / 4).max(1000)
            };
            self.backoff_milli.store(backoff, Ordering::Relaxed);
        }
        let base = self.policy.base_pause(elapsed);
        // A slow probe also pauses an import that is fast per batch.
        let floor = if backoff > 1000 {
            Duration::from_millis(10)
        } else {
            Duration::ZERO
        };
        let pause = base
            .max(floor)
            .mul_f64(backoff as f64 / 1000.0)
            .min(self.policy.max_pause);
        if !pause.is_zero() {
            self.paused_ms
                .fetch_add(pause.as_millis() as u64, Ordering::Relaxed);
            tokio::time::sleep(pause).await;
        }
    }

    async fn timed<T, F>(&self, fut: F) -> T
    where
        F: Future<Output = T>,
    {
        // tokio's clock, so paused-time tests see the batch duration.
        let started = tokio::time::Instant::now();
        let out = fut.await;
        self.pace(started.elapsed()).await;
        out
    }
}

#[async_trait]
impl BundleImportBackend for ThrottledBackend {
    async fn import_bundle(
        &self,
        ctx: &TenantContext,
        data: &[u8],
    ) -> Result<ImportStats, HtsError> {
        self.timed(self.inner.import_bundle(ctx, data)).await
    }

    async fn import_parsed(
        &self,
        ctx: &TenantContext,
        parsed: ParsedBundle,
    ) -> Result<ImportStats, HtsError> {
        self.timed(self.inner.import_parsed(ctx, parsed)).await
    }

    async fn import_bundle_deferred(
        &self,
        ctx: &TenantContext,
        data: &[u8],
    ) -> Result<ImportStats, HtsError> {
        self.timed(self.inner.import_bundle_deferred(ctx, data))
            .await
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
        self.timed(self.inner.import_refset_members(ctx, url, version, members))
            .await
    }

    async fn import_snomed_model(
        &self,
        ctx: &TenantContext,
        url: &str,
        version: Option<&str>,
        data: crate::snomed::ModelImport,
    ) -> Result<u64, HtsError> {
        self.timed(self.inner.import_snomed_model(ctx, url, version, data))
            .await
    }

    async fn write_concept_properties(
        &self,
        ctx: &TenantContext,
        url: &str,
        version: Option<&str>,
        clear: &[String],
        properties: Vec<PropertyRow>,
        designations: Vec<(String, String, String)>,
    ) -> Result<(u64, u64), HtsError> {
        self.timed(self.inner.write_concept_properties(
            ctx,
            url,
            version,
            clear,
            properties,
            designations,
        ))
        .await
    }

    async fn rebuild_missing_closures(&self) -> Result<usize, HtsError> {
        self.inner.rebuild_missing_closures().await
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

    async fn delete_normalized_by_id(&self, resource_type: &str, id: &str) -> Result<(), HtsError> {
        self.inner.delete_normalized_by_id(resource_type, id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every batch "writes" for 100 ms.
    struct Slow;

    #[async_trait]
    impl BundleImportBackend for Slow {
        async fn import_bundle(
            &self,
            _ctx: &TenantContext,
            _data: &[u8],
        ) -> Result<ImportStats, HtsError> {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(ImportStats::default())
        }
        async fn import_parsed(
            &self,
            _ctx: &TenantContext,
            _parsed: ParsedBundle,
        ) -> Result<ImportStats, HtsError> {
            Ok(ImportStats::default())
        }
    }

    #[test]
    fn base_pause_follows_the_duty_cycle() {
        let p = ThrottlePolicy {
            duty: 0.25,
            ..Default::default()
        };
        assert_eq!(
            p.base_pause(Duration::from_millis(100)),
            Duration::from_millis(300)
        );
        let off = ThrottlePolicy {
            duty: 1.0,
            ..Default::default()
        };
        assert!(off.base_pause(Duration::from_secs(5)).is_zero());
        assert!(!off.is_active());
        // Capped.
        assert_eq!(p.base_pause(Duration::from_secs(60)), p.max_pause);
    }

    #[tokio::test(start_paused = true)]
    async fn half_duty_doubles_wall_time() {
        let b = ThrottledBackend::new(Arc::new(Slow), ThrottlePolicy::default());
        let ctx = TenantContext::system();
        let t0 = tokio::time::Instant::now();
        for _ in 0..10 {
            b.import_bundle(&ctx, b"{}").await.unwrap();
        }
        let wall = t0.elapsed();
        assert!(wall >= Duration::from_millis(2000), "{wall:?}");
        assert_eq!(b.paused_ms(), 1000);
    }

    #[tokio::test(start_paused = true)]
    async fn slow_probe_backs_off_and_recovers() {
        let slow = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let flag = slow.clone();
        let probe: Probe = Arc::new(move || {
            let slow = flag.load(Ordering::Relaxed);
            Box::pin(async move {
                if slow {
                    Duration::from_millis(100)
                } else {
                    Duration::from_millis(1)
                }
            })
        });
        let policy = ThrottlePolicy {
            duty: 1.0,
            probe: Some(probe),
            ..Default::default()
        };
        assert!(policy.is_active());
        let b = ThrottledBackend::new(Arc::new(Slow), policy);
        let ctx = TenantContext::system();
        for _ in 0..4 {
            b.import_bundle(&ctx, b"{}").await.unwrap();
        }
        let backed_off = b.backoff_milli.load(Ordering::Relaxed);
        assert!(backed_off >= 16_000, "{backed_off}");
        assert!(b.paused_ms() > 0);
        slow.store(false, Ordering::Relaxed);
        for _ in 0..20 {
            b.import_bundle(&ctx, b"{}").await.unwrap();
        }
        assert_eq!(b.backoff_milli.load(Ordering::Relaxed), 1000);
    }
}
