//! Asynchronous, batching front for any audit sink.
//!
//! Recording an audit event on the request path must cost next to nothing, and
//! must not lose the event when the server stops. [`QueuedSink`] puts events on
//! a bounded in-memory queue and returns; background workers drain the queue
//! in batches into the wrapped sink. [`AuditSink::flush`] waits until every
//! queued event has been written, which `hfs` calls during graceful shutdown
//! after the HTTP drain.
//!
//! When the queue is full the caller waits for room (backpressure) instead of
//! dropping the event: the audit trail is a compliance record, and slowing a
//! request down is the lesser harm.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::{Mutex, mpsc};

use crate::fhir_model::AuditEvent;
use crate::sink::{AuditRecord, AuditSink};
use helios_observability::audit_metrics as m;

/// Tuning of a [`QueuedSink`].
#[derive(Debug, Clone)]
pub struct QueueConfig {
    /// Events the queue holds before callers wait for room.
    pub capacity: usize,
    /// Most events handed to the wrapped sink in one call.
    pub batch_size: usize,
    /// Number of concurrent writer tasks.
    pub workers: usize,
    /// Longest [`AuditSink::flush`] waits for the queue to drain.
    pub flush_timeout: Duration,
    /// Where events still queued when the flush times out are written
    /// (NDJSON, the database sink's fallback format, replayed on the next
    /// start). `None`: they are lost.
    pub spill_file: Option<std::path::PathBuf>,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            capacity: 10_000,
            batch_size: 64,
            workers: 4,
            flush_timeout: Duration::from_secs(5),
            spill_file: None,
        }
    }
}

/// Counters of a [`QueuedSink`].
#[derive(Debug, Default)]
pub struct QueueStats {
    /// Events accepted.
    pub enqueued: AtomicU64,
    /// Events handed to the wrapped sink.
    pub written: AtomicU64,
    /// Calls that had to wait because the queue was full.
    pub waited_for_room: AtomicU64,
    /// Events written to the spill file at a flush timeout.
    pub spilled: AtomicU64,
}

/// A bounded queue in front of another sink, drained by background workers.
pub struct QueuedSink {
    tx: mpsc::Sender<AuditRecord>,
    inner: Arc<dyn AuditSink>,
    pending: Arc<AtomicU64>,
    stats: Arc<QueueStats>,
    flush_timeout: Duration,
    rx: Arc<Mutex<mpsc::Receiver<AuditRecord>>>,
    spill_file: Option<std::path::PathBuf>,
}

impl QueuedSink {
    /// Wraps `inner` and starts the writer tasks. Must be called inside a
    /// Tokio runtime.
    pub fn new(inner: Arc<dyn AuditSink>, config: QueueConfig) -> Self {
        let (tx, rx) = mpsc::channel(config.capacity.max(1));
        let rx = Arc::new(Mutex::new(rx));
        let pending = Arc::new(AtomicU64::new(0));
        let stats = Arc::new(QueueStats::default());
        for _ in 0..config.workers.max(1) {
            let rx: Arc<Mutex<mpsc::Receiver<AuditRecord>>> = Arc::clone(&rx);
            let inner = Arc::clone(&inner);
            let pending = Arc::clone(&pending);
            let stats = Arc::clone(&stats);
            let batch_size = config.batch_size.max(1);
            tokio::spawn(async move {
                loop {
                    let batch = {
                        let mut rx = rx.lock().await;
                        let Some(first) = rx.recv().await else {
                            break;
                        };
                        let mut batch = Vec::with_capacity(batch_size);
                        batch.push(first);
                        while batch.len() < batch_size {
                            match rx.try_recv() {
                                Ok(r) => batch.push(r),
                                Err(_) => break,
                            }
                        }
                        batch
                    };
                    let n = batch.len() as u64;
                    let started = Instant::now();
                    inner.record_records(batch).await;
                    m::observe_batch_write(started.elapsed());
                    m::record_written(n);
                    stats.written.fetch_add(n, Ordering::Relaxed);
                    let left = pending.fetch_sub(n, Ordering::AcqRel) - n;
                    m::set_queue_depth(left);
                }
            });
        }
        Self {
            tx,
            inner,
            pending,
            stats,
            flush_timeout: config.flush_timeout,
            rx,
            spill_file: config.spill_file,
        }
    }

    /// Events accepted but not yet written.
    pub fn pending(&self) -> u64 {
        self.pending.load(Ordering::Acquire)
    }

    /// Counters since start.
    pub fn stats(&self) -> &QueueStats {
        &self.stats
    }

    /// Takes everything still queued off the queue and appends it to the
    /// spill file, so a flush that runs out of time loses nothing (the
    /// batches the writers are already storing complete on their own).
    async fn spill(&self) {
        let mut taken = Vec::new();
        {
            // Writers hold the lock only while they take a batch.
            let mut rx = self.rx.lock().await;
            while let Ok(r) = rx.try_recv() {
                taken.push(r);
            }
        }
        if taken.is_empty() {
            return;
        }
        let n = taken.len() as u64;
        let Some(path) = &self.spill_file else {
            tracing::error!(
                lost = n,
                timeout_secs = self.flush_timeout.as_secs(),
                "Audit queue did not drain before the flush timeout and no spill file is \
                 configured; queued events are lost"
            );
            m::record_dropped(n, m::DropReason::NoFallback);
            self.pending.fetch_sub(n, Ordering::AcqRel);
            return;
        };
        let mut buf = String::new();
        for r in &taken {
            match serde_json::to_value(&r.event) {
                Ok(mut resource) => {
                    if let Some(obj) = resource.as_object_mut() {
                        obj.entry("resourceType")
                            .or_insert_with(|| "AuditEvent".into());
                    }
                    buf.push_str(
                        &serde_json::json!({ "tenant": r.tenant, "resource": resource })
                            .to_string(),
                    );
                    buf.push('\n');
                }
                Err(e) => tracing::error!(error = %e, "Failed to serialize a queued audit event"),
            }
        }
        match append(path, buf.as_bytes()).await {
            Ok(()) => {
                self.stats.spilled.fetch_add(n, Ordering::Relaxed);
                m::record_spilled(n, m::SpillReason::Shutdown);
                tracing::warn!(
                    spilled = n,
                    file = %path.display(),
                    "Audit queue did not drain before the flush timeout; queued events were \
                     written to the spill file and are replayed at the next start"
                );
            }
            Err(e) => {
                m::record_dropped(n, m::DropReason::FallbackFailed);
                tracing::error!(
                lost = n,
                error = %e,
                file = %path.display(),
                "Audit queue did not drain and the spill file could not be written; queued events are lost"
                );
            }
        }
        let left = self.pending.fetch_sub(n, Ordering::AcqRel) - n;
        m::set_queue_depth(left);
    }
}

async fn append(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    f.write_all(bytes).await?;
    f.sync_data().await
}

#[async_trait]
impl AuditSink for QueuedSink {
    async fn record(&self, event: AuditEvent) {
        self.record_in(None, event).await;
    }

    async fn record_in(&self, tenant: Option<&str>, event: AuditEvent) {
        let record = AuditRecord {
            tenant: tenant.map(str::to_string),
            event,
        };
        let depth = self.pending.fetch_add(1, Ordering::AcqRel) + 1;
        self.stats.enqueued.fetch_add(1, Ordering::Relaxed);
        m::record_enqueued(1);
        m::set_queue_depth(depth);
        let record = match self.tx.try_send(record) {
            Ok(()) => return,
            Err(mpsc::error::TrySendError::Full(r)) => {
                self.stats.waited_for_room.fetch_add(1, Ordering::Relaxed);
                m::record_queue_full_wait();
                r
            }
            Err(mpsc::error::TrySendError::Closed(r)) => r,
        };
        if let Err(mpsc::error::SendError(r)) = self.tx.send(record).await {
            // No worker left (runtime shutting down): write it ourselves.
            self.inner.record_records(vec![r]).await;
            self.stats.written.fetch_add(1, Ordering::Relaxed);
            m::record_written(1);
            self.pending.fetch_sub(1, Ordering::AcqRel);
        }
    }

    async fn flush(&self) {
        let deadline = Instant::now() + self.flush_timeout;
        while self.pending() > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        if self.pending() > 0 {
            self.spill().await;
            // What is left is in the writers' hands; let those batches land
            // (a batch is one transaction — it commits or it is spilled by the
            // database sink's own fallback).
            let landing = Instant::now() + Duration::from_secs(2);
            while self.pending() > 0 && Instant::now() < landing {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if self.pending() > 0 {
                tracing::error!(
                    in_flight = self.pending(),
                    "Audit batches still being written at shutdown were abandoned"
                );
            }
        }
        self.inner.flush().await;
        tracing::info!(
            enqueued = self.stats.enqueued.load(Ordering::Relaxed),
            written = self.stats.written.load(Ordering::Relaxed),
            spilled = self.stats.spilled.load(Ordering::Relaxed),
            waited_for_room = self.stats.waited_for_room.load(Ordering::Relaxed),
            "Audit queue flushed"
        );
    }

    fn name(&self) -> &str {
        self.inner.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::AuditEventBuilder;

    /// Slow sink that keeps what it receives.
    #[derive(Default)]
    struct Slow {
        got: std::sync::Mutex<Vec<(Option<String>, usize)>>,
        batches: AtomicU64,
    }

    #[async_trait]
    impl AuditSink for Slow {
        async fn record(&self, _event: AuditEvent) {
            unreachable!("the queue hands over records")
        }
        async fn record_records(&self, records: Vec<AuditRecord>) {
            tokio::time::sleep(Duration::from_millis(2)).await;
            self.batches.fetch_add(1, Ordering::Relaxed);
            let n = records.len();
            let mut got = self.got.lock().unwrap();
            for r in records {
                got.push((r.tenant, n));
            }
        }
        async fn flush(&self) {}
        fn name(&self) -> &str {
            "slow"
        }
    }

    #[tokio::test]
    async fn flush_drains_everything_and_batches() {
        let inner = Arc::new(Slow::default());
        let sink = QueuedSink::new(
            Arc::clone(&inner) as _,
            QueueConfig {
                capacity: 16,
                batch_size: 8,
                workers: 2,
                flush_timeout: Duration::from_secs(10),
                spill_file: None,
            },
        );
        for i in 0..200 {
            let tenant = format!("t{}", i % 3);
            sink.record_in(Some(&tenant), AuditEventBuilder::new("Device/hfs").build())
                .await;
        }
        sink.flush().await;
        assert_eq!(sink.pending(), 0);
        let got = inner.got.lock().unwrap();
        assert_eq!(got.len(), 200, "no event is lost");
        assert_eq!(
            got.iter()
                .filter(|(t, _)| t.as_deref() == Some("t0"))
                .count(),
            67
        );
        assert!(
            inner.batches.load(Ordering::Relaxed) < 200,
            "events are written in batches"
        );
        assert!(
            sink.stats().waited_for_room.load(Ordering::Relaxed) > 0,
            "a full queue applies backpressure rather than dropping"
        );
    }

    #[tokio::test]
    async fn record_returns_before_the_write() {
        let inner = Arc::new(Slow::default());
        let sink = QueuedSink::new(Arc::clone(&inner) as _, QueueConfig::default());
        let started = Instant::now();
        sink.record(AuditEventBuilder::new("Device/hfs").build())
            .await;
        assert!(started.elapsed() < Duration::from_millis(2));
        sink.flush().await;
        assert_eq!(inner.got.lock().unwrap().len(), 1);
    }

    /// A sink that never finishes a write.
    struct Stuck;

    #[async_trait]
    impl AuditSink for Stuck {
        async fn record(&self, _event: AuditEvent) {}
        async fn record_records(&self, _records: Vec<AuditRecord>) {
            std::future::pending::<()>().await;
        }
        async fn flush(&self) {}
        fn name(&self) -> &str {
            "stuck"
        }
    }

    #[tokio::test]
    async fn a_flush_that_times_out_spills_the_queue_to_the_fallback_file() {
        let dir = tempfile::tempdir().unwrap();
        let spill = dir.path().join("audit-fallback.ndjson");
        let sink = QueuedSink::new(
            Arc::new(Stuck),
            QueueConfig {
                capacity: 100,
                batch_size: 1,
                workers: 1,
                flush_timeout: Duration::from_millis(50),
                spill_file: Some(spill.clone()),
            },
        );
        for _ in 0..10 {
            sink.record_in(Some("t"), AuditEventBuilder::new("Device/hfs").build())
                .await;
        }
        sink.flush().await;
        let lines: Vec<serde_json::Value> = std::fs::read_to_string(&spill)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        // One event is stuck inside the writer; the other nine are spilled.
        assert_eq!(lines.len(), 9);
        assert_eq!(lines[0]["tenant"], "t");
        assert_eq!(lines[0]["resource"]["resourceType"], "AuditEvent");
        assert_eq!(sink.stats().spilled.load(Ordering::Relaxed), 9);
    }

    #[test]
    fn the_write_path_is_visible_in_prometheus_metrics() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            // Current-thread runtime: the writer tasks record on this thread.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let inner = Arc::new(Slow::default());
                let sink = QueuedSink::new(Arc::clone(&inner) as _, QueueConfig::default());
                for _ in 0..3 {
                    sink.record(AuditEventBuilder::new("Device/hfs").build())
                        .await;
                }
                sink.flush().await;
            });
        });
        let text = handle.render();
        let value = |name: &str| {
            text.lines()
                .find(|l| l.starts_with(name) && !l.starts_with(&format!("{name}_")))
                .and_then(|l| l.rsplit(' ').next())
                .and_then(|v| v.parse::<f64>().ok())
        };
        assert_eq!(value("audit_events_enqueued_total"), Some(3.0), "{text}");
        assert_eq!(value("audit_events_written_total"), Some(3.0), "{text}");
        assert_eq!(value("audit_queue_depth"), Some(0.0), "{text}");
        assert!(
            text.contains("audit_batch_write_duration_seconds"),
            "{text}"
        );
    }
}
