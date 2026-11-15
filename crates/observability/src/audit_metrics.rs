//! Prometheus metrics for the audit trail's write path (`helios-audit`).
//!
//! The audit queue sits between the request path and the audit store; these
//! metrics show whether it keeps up, and when it does not, what happened to
//! the events:
//!
//! | metric | type | meaning |
//! |---|---|---|
//! | `audit_events_enqueued_total` | counter | events accepted onto the queue |
//! | `audit_events_written_total` | counter | events handed to the store (`rate()` = events/s) |
//! | `audit_queue_depth` | gauge | events accepted but not yet written (queued + in flight) |
//! | `audit_queue_full_waits_total` | counter | requests that waited because the queue was full (backpressure) |
//! | `audit_batch_write_duration_seconds` | histogram | wall time of one batch write |
//! | `audit_events_spilled_total{reason}` | counter | events written to the fallback file instead (`shutdown`, `write_failure`) |
//! | `audit_events_replayed_total` | counter | spilled events stored at start-up |
//! | `audit_events_dropped_total{reason}` | counter | events lost (`serialization`, `no_fallback`, `fallback_failed`) |
//! | `audit_retention_purged_total` | counter | events removed by the retention job |
//! | `audit_retention_runs_total{outcome}` | counter | retention runs (`ok`, `error`) |
//!
//! Process-level only — no tenant label (see [`crate::metrics`]). Every
//! function is safe without an installed recorder.

use std::time::Duration;

pub const ENQUEUED: &str = "audit_events_enqueued_total";
pub const WRITTEN: &str = "audit_events_written_total";
pub const QUEUE_DEPTH: &str = "audit_queue_depth";
pub const QUEUE_FULL_WAITS: &str = "audit_queue_full_waits_total";
pub const BATCH_WRITE_DURATION: &str = "audit_batch_write_duration_seconds";
pub const SPILLED: &str = "audit_events_spilled_total";
pub const REPLAYED: &str = "audit_events_replayed_total";
pub const DROPPED: &str = "audit_events_dropped_total";
pub const RETENTION_PURGED: &str = "audit_retention_purged_total";
pub const RETENTION_RUNS: &str = "audit_retention_runs_total";

/// Buckets (seconds) for [`BATCH_WRITE_DURATION`]: a sub-millisecond file
/// append up to a database stalled for many seconds.
pub(crate) const BATCH_BUCKETS: &[f64] = &[
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Why events went to the fallback file.
#[derive(Clone, Copy, Debug)]
pub enum SpillReason {
    /// The shutdown flush ran out of time.
    Shutdown,
    /// The store refused the write after retries.
    WriteFailure,
}

/// Why events were lost.
#[derive(Clone, Copy, Debug)]
pub enum DropReason {
    /// The event could not be serialized.
    Serialization,
    /// No fallback file is configured.
    NoFallback,
    /// Writing the fallback file failed.
    FallbackFailed,
}

pub fn record_enqueued(n: u64) {
    metrics::counter!(ENQUEUED).increment(n);
}

pub fn record_written(n: u64) {
    metrics::counter!(WRITTEN).increment(n);
}

pub fn set_queue_depth(depth: u64) {
    metrics::gauge!(QUEUE_DEPTH).set(depth as f64);
}

pub fn record_queue_full_wait() {
    metrics::counter!(QUEUE_FULL_WAITS).increment(1);
}

pub fn observe_batch_write(elapsed: Duration) {
    metrics::histogram!(BATCH_WRITE_DURATION).record(elapsed.as_secs_f64());
}

pub fn record_spilled(n: u64, reason: SpillReason) {
    let reason = match reason {
        SpillReason::Shutdown => "shutdown",
        SpillReason::WriteFailure => "write_failure",
    };
    metrics::counter!(SPILLED, "reason" => reason).increment(n);
}

pub fn record_replayed(n: u64) {
    metrics::counter!(REPLAYED).increment(n);
}

pub fn record_dropped(n: u64, reason: DropReason) {
    let reason = match reason {
        DropReason::Serialization => "serialization",
        DropReason::NoFallback => "no_fallback",
        DropReason::FallbackFailed => "fallback_failed",
    };
    metrics::counter!(DROPPED, "reason" => reason).increment(n);
}

pub fn record_retention_run(purged: u64, ok: bool) {
    metrics::counter!(RETENTION_PURGED).increment(purged);
    metrics::counter!(RETENTION_RUNS, "outcome" => if ok { "ok" } else { "error" }).increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_audit_metric_is_exported_without_tenant_labels() {
        let recorder = crate::metrics::builder("test").build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_enqueued(5);
            record_written(4);
            set_queue_depth(1);
            record_queue_full_wait();
            observe_batch_write(Duration::from_millis(3));
            record_spilled(2, SpillReason::Shutdown);
            record_spilled(1, SpillReason::WriteFailure);
            record_replayed(3);
            record_dropped(1, DropReason::NoFallback);
            record_retention_run(7, true);
        });
        let text = handle.render();
        let has = |needle: &str| text.lines().any(|l| l.contains(needle));
        assert!(
            has("audit_events_enqueued_total{service=\"test\"} 5"),
            "{text}"
        );
        assert!(
            has("audit_events_written_total{service=\"test\"} 4"),
            "{text}"
        );
        assert!(has("audit_queue_depth{service=\"test\"} 1"), "{text}");
        assert!(has("audit_queue_full_waits_total"), "{text}");
        assert!(
            has("audit_batch_write_duration_seconds_bucket"),
            "a real histogram, not a summary: {text}"
        );
        assert!(has("reason=\"shutdown\""), "{text}");
        assert!(has("reason=\"write_failure\""), "{text}");
        assert!(
            has("audit_events_replayed_total{service=\"test\"} 3"),
            "{text}"
        );
        assert!(has("reason=\"no_fallback\""), "{text}");
        assert!(
            has("audit_retention_purged_total{service=\"test\"} 7"),
            "{text}"
        );
        assert!(has("outcome=\"ok\""), "{text}");
        assert!(!text.contains("tenant"), "{text}");
    }
}
