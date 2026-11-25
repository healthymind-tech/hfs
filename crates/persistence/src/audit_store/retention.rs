//! Audit retention: a background job that purges `AuditEvent`s older than
//! `HFS_AUDIT_RETENTION_DAYS`, optionally archiving them first.
//!
//! Each run works through the expired events oldest-first in pages of
//! `HFS_AUDIT_RETENTION_BATCH`. With `HFS_AUDIT_RETENTION_ARCHIVE_DIR` set, a
//! page is appended to a gzip NDJSON archive and the archive is flushed to
//! disk (`fsync`) *before* the page is deleted, so an event is never deleted
//! without having been archived; a failing archive stops the run. Every run
//! that removed anything records one `AuditEvent` of its own (action `D`,
//! `audit-operation=retention-purge`) — the purge of the audit trail is itself
//! audited, but not event by event.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::core::RetentionStorage;
use chrono::{DateTime, Utc};
use flate2::Compression;
use flate2::write::GzEncoder;
use helios_audit::{AuditAction, AuditEventBuilder, AuditSink, RetentionConfig};
use tracing::{error, info, warn};

const RESOURCE_TYPE: &str = "AuditEvent";

/// Outcome of one retention run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RetentionReport {
    /// Events removed.
    pub purged: u64,
    /// The archive written by the run, if any.
    pub archive: Option<PathBuf>,
}

struct Archive {
    path: PathBuf,
    encoder: GzEncoder<std::fs::File>,
}

impl Archive {
    fn create(dir: &Path, now: DateTime<Utc>) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!(
            "auditevent-{}.ndjson.gz",
            now.format("%Y%m%dT%H%M%S%.3fZ")
        ));
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        Ok(Self {
            path,
            encoder: GzEncoder::new(file, Compression::default()),
        })
    }

    /// Appends lines and makes them durable.
    fn append(&mut self, lines: &[String]) -> std::io::Result<()> {
        for line in lines {
            self.encoder.write_all(line.as_bytes())?;
            self.encoder.write_all(b"\n")?;
        }
        self.encoder.flush()?;
        self.encoder.get_ref().sync_data()
    }

    fn finish(self) -> std::io::Result<PathBuf> {
        let file = self.encoder.finish()?;
        file.sync_all()?;
        Ok(self.path)
    }
}

/// Purges every `AuditEvent` last updated before `now - days`.
pub async fn run_once(
    store: &dyn RetentionStorage,
    config: &RetentionConfig,
    now: DateTime<Utc>,
) -> Result<RetentionReport, Box<dyn std::error::Error + Send + Sync>> {
    let Some(days) = config.days else {
        return Ok(RetentionReport::default());
    };
    let cutoff = now - chrono::Duration::days(i64::from(days));
    let mut archive: Option<Archive> = None;
    let mut report = RetentionReport::default();

    loop {
        let page = store
            .expired_resources(RESOURCE_TYPE, cutoff, config.batch)
            .await?;
        if page.is_empty() {
            break;
        }
        if let Some(dir) = &config.archive_dir {
            if archive.is_none() {
                archive = Some(Archive::create(Path::new(dir), now)?);
            }
            let lines: Vec<String> = page
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "tenant": r.tenant_id,
                        "lastUpdated": r.last_updated.to_rfc3339(),
                        "resource": r.content,
                    })
                    .to_string()
                })
                .collect();
            let a = archive.as_mut().expect("created above");
            tokio::task::block_in_place(|| a.append(&lines))?;
        }
        let keys: Vec<(String, String)> = page
            .iter()
            .map(|r| (r.tenant_id.clone(), r.id.clone()))
            .collect();
        let removed = store.purge_expired(RESOURCE_TYPE, &keys).await?;
        report.purged += removed;
        if removed == 0 {
            // Nothing could be removed (rows vanished meanwhile); never spin.
            break;
        }
    }

    if let Some(a) = archive {
        report.archive = Some(tokio::task::block_in_place(|| a.finish())?);
    }
    Ok(report)
}

/// Starts the periodic retention job. Returns `None` when no retention is
/// configured.
pub fn spawn(
    store: Arc<dyn RetentionStorage>,
    config: RetentionConfig,
    sink: Arc<dyn AuditSink>,
    source_observer: String,
) -> Option<tokio::task::JoinHandle<()>> {
    let days = config.days?;
    info!(
        days,
        interval_hours = config.interval_hours,
        archive_dir = ?config.archive_dir,
        "Audit retention is ENABLED"
    );
    Some(tokio::spawn(async move {
        // Let the server finish starting before the first (possibly long) run.
        tokio::time::sleep(Duration::from_secs(60)).await;
        let mut tick = tokio::time::interval(Duration::from_secs(config.interval_hours * 3600));
        loop {
            tick.tick().await;
            let started = Utc::now();
            match run_once(store.as_ref(), &config, started).await {
                Ok(report) => {
                    helios_observability::audit_metrics::record_retention_run(report.purged, true);
                    if report.purged > 0 {
                        info!(purged = report.purged, archive = ?report.archive, "Audit retention run purged expired AuditEvents");
                        record(&*sink, &source_observer, &config, &report, "0", None).await;
                    }
                }
                Err(e) => {
                    helios_observability::audit_metrics::record_retention_run(0, false);
                    error!(error = %e, "Audit retention run failed");
                    record(
                        &*sink,
                        &source_observer,
                        &config,
                        &RetentionReport::default(),
                        "8",
                        Some(e.to_string()),
                    )
                    .await;
                }
            }
        }
    }))
}

/// Retention over the dedicated audit tables: archives (when configured) and
/// then **drops** every monthly partition that lies entirely before
/// `now - days`, and row-deletes expired events of the default partition.
///
/// Whole months only: an event is kept for at least the retention period and
/// for at most one month longer. Only one instance runs it at a time (an
/// advisory lock); the others return an empty report.
#[cfg(feature = "postgres")]
pub async fn run_table_once(
    table: &crate::backends::postgres::audit_table::PgAuditTable,
    config: &RetentionConfig,
    now: DateTime<Utc>,
) -> Result<RetentionReport, Box<dyn std::error::Error + Send + Sync>> {
    let Some(days) = config.days else {
        return Ok(RetentionReport::default());
    };
    let Some(lock) = table.maintenance_lock().await? else {
        info!("Audit retention is running on another instance; skipping this run");
        return Ok(RetentionReport::default());
    };
    let result = run_table_locked(table, config, days, now).await;
    lock.release().await;
    result
}

#[cfg(feature = "postgres")]
async fn run_table_locked(
    table: &crate::backends::postgres::audit_table::PgAuditTable,
    config: &RetentionConfig,
    days: u32,
    now: DateTime<Utc>,
) -> Result<RetentionReport, Box<dyn std::error::Error + Send + Sync>> {
    let cutoff = now - chrono::Duration::days(i64::from(days));
    let mut archive: Option<Archive> = None;
    let mut report = RetentionReport::default();
    let line = |r: &crate::core::retention::ExpiredResource| {
        serde_json::json!({
            "tenant": r.tenant_id,
            "lastUpdated": r.last_updated.to_rfc3339(),
            "resource": r.content,
        })
        .to_string()
    };

    for partition in table.partitions().await? {
        if partition.end() > cutoff {
            continue;
        }
        if let Some(dir) = &config.archive_dir {
            let mut after = None;
            loop {
                let page = table
                    .partition_page(&partition.name, after.as_ref(), None, config.batch)
                    .await?;
                let Some(last) = page.last() else {
                    break;
                };
                after = Some((last.tenant_id.clone(), last.last_updated, last.id.clone()));
                if archive.is_none() {
                    archive = Some(Archive::create(Path::new(dir), now)?);
                }
                let lines: Vec<String> = page.iter().map(line).collect();
                let a = archive.as_mut().expect("created above");
                tokio::task::block_in_place(|| a.append(&lines))?;
            }
        }
        // Everything of the partition is in the archive and on disk.
        let dropped = table.drop_partition(&partition).await?;
        info!(partition = %partition.name, events = dropped, "Dropped expired audit partition");
        report.purged += dropped;
    }

    // Events filed in the default partition (their month had no partition).
    loop {
        let page = table
            .partition_page("audit_event_default", None, Some(cutoff), config.batch)
            .await?;
        if page.is_empty() {
            break;
        }
        if let Some(dir) = &config.archive_dir {
            if archive.is_none() {
                archive = Some(Archive::create(Path::new(dir), now)?);
            }
            let lines: Vec<String> = page.iter().map(line).collect();
            let a = archive.as_mut().expect("created above");
            tokio::task::block_in_place(|| a.append(&lines))?;
        }
        let keys: Vec<(String, DateTime<Utc>, String)> = page
            .into_iter()
            .map(|r| (r.tenant_id, r.last_updated, r.id))
            .collect();
        let removed = table.delete_default_rows(&keys).await?;
        report.purged += removed;
        if removed == 0 {
            break;
        }
    }

    if let Some(a) = archive {
        report.archive = Some(tokio::task::block_in_place(|| a.finish())?);
    }
    Ok(report)
}

/// Starts the maintenance of the dedicated audit tables: creates the monthly
/// partitions ahead of time (now and every five minutes) and, when retention
/// is configured, runs [`run_table_once`] at the retention interval (first a
/// few minutes after start).
#[cfg(feature = "postgres")]
pub fn spawn_table(
    table: crate::backends::postgres::audit_table::PgAuditTable,
    config: RetentionConfig,
    sink: Arc<dyn AuditSink>,
    source_observer: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let started = tokio::time::Instant::now();
        let retention_every = Duration::from_secs(config.interval_hours.max(1) * 3600);
        let mut next_retention = started + Duration::from_secs(60);
        let mut tick = tokio::time::interval(Duration::from_secs(300));
        loop {
            tick.tick().await;
            if let Err(e) = table.ensure_partitions(Utc::now(), 2).await {
                // Not fatal: events of a month without a partition go to the
                // default partition and are moved when the partition appears.
                warn!(error = %e, "Could not create audit partitions ahead of time");
            }
            if config.days.is_none() || tokio::time::Instant::now() < next_retention {
                continue;
            }
            next_retention = tokio::time::Instant::now() + retention_every;
            match run_table_once(&table, &config, Utc::now()).await {
                Ok(report) => {
                    helios_observability::audit_metrics::record_retention_run(report.purged, true);
                    if report.purged > 0 {
                        info!(purged = report.purged, archive = ?report.archive, "Audit retention run dropped expired audit partitions");
                        record(&*sink, &source_observer, &config, &report, "0", None).await;
                    }
                }
                Err(e) => {
                    helios_observability::audit_metrics::record_retention_run(0, false);
                    error!(error = %e, "Audit table retention run failed");
                    record(
                        &*sink,
                        &source_observer,
                        &config,
                        &RetentionReport::default(),
                        "8",
                        Some(e.to_string()),
                    )
                    .await;
                }
            }
        }
    })
}

async fn record(
    sink: &dyn AuditSink,
    source_observer: &str,
    config: &RetentionConfig,
    report: &RetentionReport,
    outcome: &str,
    failure: Option<String>,
) {
    let mut b = AuditEventBuilder::new(source_observer)
        .event_type(
            "http://terminology.hl7.org/CodeSystem/audit-event-type",
            "object",
        )
        .action(AuditAction::Delete)
        .outcome(outcome)
        .detail("audit-operation", "retention-purge")
        .detail("resource-type", RESOURCE_TYPE)
        .detail("retention-days", config.days.unwrap_or(0).to_string())
        .detail("count", report.purged.to_string());
    if let Some(path) = &report.archive {
        b = b.detail("archive", path.display().to_string());
    }
    if let Some(f) = failure {
        b = b.outcome_desc(f);
    }
    sink.record(b.build()).await;
}

/// Warns at startup about a retention configuration that cannot work.
pub fn warn_unsupported(config: &RetentionConfig, reason: &str) {
    if config.days.is_some() {
        warn!(
            reason,
            "HFS_AUDIT_RETENTION_DAYS is set but audit retention is not available"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ExpiredResource;
    use crate::error::StorageResult;
    use std::sync::Mutex;

    /// In-memory store: (tenant, id, last_updated).
    struct Mem(Mutex<Vec<(String, String, DateTime<Utc>)>>);

    #[async_trait::async_trait]
    impl RetentionStorage for Mem {
        async fn expired_resources(
            &self,
            _rt: &str,
            cutoff: DateTime<Utc>,
            limit: usize,
        ) -> StorageResult<Vec<ExpiredResource>> {
            let mut v: Vec<_> = self
                .0
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, _, t)| *t < cutoff)
                .cloned()
                .collect();
            v.sort_by_key(|(_, _, t)| *t);
            Ok(v.into_iter()
                .take(limit)
                .map(|(tenant_id, id, last_updated)| ExpiredResource {
                    content: serde_json::json!({"resourceType": "AuditEvent", "id": id}),
                    tenant_id,
                    id,
                    last_updated,
                })
                .collect())
        }
        async fn purge_expired(&self, _rt: &str, keys: &[(String, String)]) -> StorageResult<u64> {
            let mut v = self.0.lock().unwrap();
            let before = v.len();
            v.retain(|(t, i, _)| !keys.iter().any(|(kt, ki)| kt == t && ki == i));
            Ok((before - v.len()) as u64)
        }
    }

    fn store(now: DateTime<Utc>) -> Mem {
        let mut v = Vec::new();
        for d in 0..10i64 {
            v.push((
                "default".into(),
                format!("e{d}"),
                now - chrono::Duration::days(d * 10),
            ));
        }
        Mem(Mutex::new(v))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn purges_only_expired_events_and_archives_them_first() {
        let now = Utc::now();
        let mem = store(now);
        let dir = tempfile::tempdir().unwrap();
        let config = RetentionConfig {
            days: Some(35),
            interval_hours: 24,
            batch: 2,
            archive_dir: Some(dir.path().display().to_string()),
        };
        let report = run_once(&mem, &config, now).await.unwrap();
        // Ages 40..90 days are expired: 6 events.
        assert_eq!(report.purged, 6);
        let left: Vec<String> = mem.0.lock().unwrap().iter().map(|e| e.1.clone()).collect();
        assert_eq!(left, vec!["e0", "e1", "e2", "e3"]);

        let path = report.archive.unwrap();
        let mut text = String::new();
        use std::io::Read;
        flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap())
            .read_to_string(&mut text)
            .unwrap();
        let ids: Vec<String> = text
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .map(|v| {
                assert_eq!(v["tenant"], "default");
                v["resource"]["id"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(ids, vec!["e9", "e8", "e7", "e6", "e5", "e4"]);

        // A second run finds nothing and writes no archive.
        let again = run_once(&mem, &config, now).await.unwrap();
        assert_eq!(again, RetentionReport::default());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_retention_configured_is_a_no_op() {
        let now = Utc::now();
        let mem = store(now);
        let report = run_once(&mem, &RetentionConfig::default(), now)
            .await
            .unwrap();
        assert_eq!(report.purged, 0);
        assert_eq!(mem.0.lock().unwrap().len(), 10);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unwritable_archive_stops_the_purge() {
        let now = Utc::now();
        let mem = store(now);
        let file = tempfile::NamedTempFile::new().unwrap();
        let config = RetentionConfig {
            days: Some(35),
            interval_hours: 24,
            batch: 100,
            // A file, not a directory: the archive cannot be created.
            archive_dir: Some(file.path().join("sub").display().to_string()),
        };
        assert!(run_once(&mem, &config, now).await.is_err());
        assert_eq!(
            mem.0.lock().unwrap().len(),
            10,
            "nothing deleted unarchived"
        );
    }
}
