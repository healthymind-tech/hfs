//! Database audit sink.
//!
//! Persists `AuditEvent` resources via the FHIR storage backend by serializing
//! the typed struct to `serde_json::Value` and calling `ResourceStorage::create()`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::fhir_model::AuditEvent;
use async_trait::async_trait;
use helios_fhir::FhirVersion;

use crate::sink::AuditSink;

/// Trait for the subset of storage operations the database sink needs.
///
/// This avoids a direct dependency on `helios-persistence` — the HFS binary
/// provides a concrete implementation that delegates to `ResourceStorage`.
#[async_trait]
pub trait AuditStorage: Send + Sync + 'static {
    /// Persist a FHIR resource as JSON in `tenant` (`None`: the store's
    /// default audit tenant).
    async fn create_resource(
        &self,
        tenant: Option<&str>,
        resource_type: &str,
        resource: serde_json::Value,
        fhir_version: FhirVersion,
    ) -> Result<(), String>;
}

/// How often a failed write is attempted before the event is spilled to the
/// fallback file.
const WRITE_ATTEMPTS: u32 = 3;

/// Database-backed audit sink.
///
/// Serializes `AuditEvent` to JSON and delegates to an [`AuditStorage`]
/// implementation for persistence. A write that keeps failing (the database
/// is down) is retried a few times and then appended to an optional fallback
/// NDJSON file, which [`DatabaseSink::replay_fallback`] loads back on the next
/// start, so an outage loses no events.
pub struct DatabaseSink {
    storage: Arc<dyn AuditStorage>,
    fhir_version: FhirVersion,
    fallback: Option<PathBuf>,
    fallback_lock: Mutex<()>,
    retry_delay: Duration,
}

impl DatabaseSink {
    /// Create a new database sink.
    pub fn new(storage: Arc<dyn AuditStorage>, fhir_version: FhirVersion) -> Self {
        Self {
            storage,
            fhir_version,
            fallback: None,
            fallback_lock: Mutex::new(()),
            retry_delay: Duration::from_millis(200),
        }
    }

    /// Spill events that cannot be written to this NDJSON file.
    pub fn with_fallback(mut self, path: impl Into<PathBuf>) -> Self {
        self.fallback = Some(path.into());
        self
    }

    /// Delay before the first retry (doubled for each further attempt).
    pub fn with_retry_delay(mut self, delay: Duration) -> Self {
        self.retry_delay = delay;
        self
    }

    async fn write(&self, tenant: Option<&str>, value: serde_json::Value) {
        let mut delay = self.retry_delay;
        let mut last_error = String::new();
        for attempt in 1..=WRITE_ATTEMPTS {
            match self
                .storage
                .create_resource(tenant, "AuditEvent", value.clone(), self.fhir_version)
                .await
            {
                Ok(()) => return,
                Err(e) => {
                    last_error = e;
                    if attempt < WRITE_ATTEMPTS {
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                    }
                }
            }
        }
        match &self.fallback {
            Some(path) => {
                let line = serde_json::json!({ "tenant": tenant, "resource": value });
                let _guard = self.fallback_lock.lock().await;
                match append_line(path, &line.to_string()).await {
                    Ok(()) => tracing::error!(
                        error = %last_error,
                        fallback = %path.display(),
                        "Failed to persist audit event to database; spilled to fallback file"
                    ),
                    Err(fe) => tracing::error!(
                        error = %last_error,
                        fallback_error = %fe,
                        "Failed to persist audit event to database and to the fallback file; event lost"
                    ),
                }
            }
            None => {
                tracing::error!(error = %last_error, "Failed to persist audit event to database; event lost")
            }
        }
    }

    /// Writes the events of a previous run's fallback file to the database and
    /// renames the file (`*.replayed-<timestamp>`). Returns how many events
    /// were replayed. Events that still cannot be written stay in a new
    /// fallback file.
    pub async fn replay_fallback(&self) -> std::io::Result<usize> {
        let Some(path) = self.fallback.clone() else {
            return Ok(0);
        };
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(0);
        }
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let moved = path.with_extension(format!("replayed-{stamp}"));
        tokio::fs::rename(&path, &moved).await?;
        let content = tokio::fs::read_to_string(&moved).await?;
        let mut replayed = 0;
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                tracing::warn!("Skipping malformed line in audit fallback file");
                continue;
            };
            let tenant = v.get("tenant").and_then(|t| t.as_str()).map(str::to_string);
            if let Some(resource) = v.get("resource").cloned() {
                self.write(tenant.as_deref(), resource).await;
                replayed += 1;
            }
        }
        Ok(replayed)
    }
}

async fn append_line(path: &PathBuf, line: &str) -> std::io::Result<()> {
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
    f.write_all(line.as_bytes()).await?;
    f.write_all(b"\n").await?;
    f.sync_data().await
}

#[async_trait]
impl AuditSink for DatabaseSink {
    async fn record(&self, event: AuditEvent) {
        self.record_in(None, event).await;
    }

    async fn record_in(&self, tenant: Option<&str>, event: AuditEvent) {
        let value = match serde_json::to_value(&event) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "Failed to serialize audit event to JSON Value");
                return;
            }
        };
        self.write(tenant, value).await;
    }

    async fn flush(&self) {
        // Database writes are immediate — nothing to flush.
    }

    fn name(&self) -> &str {
        "database"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::balp::AuditAction;
    use crate::builder::AuditEventBuilder;

    /// In-memory mock storage for testing.
    struct MockStorage {
        events: Mutex<Vec<serde_json::Value>>,
    }

    impl MockStorage {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl AuditStorage for MockStorage {
        async fn create_resource(
            &self,
            _tenant: Option<&str>,
            _resource_type: &str,
            resource: serde_json::Value,
            _fhir_version: FhirVersion,
        ) -> Result<(), String> {
            self.events.lock().await.push(resource);
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_record_persists_event() {
        let mock = Arc::new(MockStorage::new());
        let sink = DatabaseSink::new(
            Arc::clone(&mock) as Arc<dyn AuditStorage>,
            FhirVersion::default(),
        );

        let event = AuditEventBuilder::new("Device/hfs")
            .action(AuditAction::Read)
            .outcome("0")
            .build();
        sink.record(event).await;

        let events = mock.events.lock().await;
        assert_eq!(events.len(), 1);
        // Verify it's a valid AuditEvent by checking for key fields
        assert!(events[0]["id"].is_string());
        assert!(events[0]["recorded"].is_string());
    }

    #[tokio::test]
    async fn test_record_includes_correct_fields() {
        let mock = Arc::new(MockStorage::new());
        let sink = DatabaseSink::new(
            Arc::clone(&mock) as Arc<dyn AuditStorage>,
            FhirVersion::default(),
        );

        let event = AuditEventBuilder::new("Device/hfs")
            .action(AuditAction::Create)
            .outcome("0")
            .resource("Patient", "123")
            .build();
        sink.record(event).await;

        let events = mock.events.lock().await;
        let e = &events[0];
        assert!(e["id"].is_string());
        assert!(e["recorded"].is_string());
        assert!(e["source"]["observer"]["reference"].is_string());
        assert!(e["agent"].is_array());
    }

    #[tokio::test]
    async fn test_storage_error_is_logged_not_propagated() {
        struct FailingStorage;

        #[async_trait]
        impl AuditStorage for FailingStorage {
            async fn create_resource(
                &self,
                _: Option<&str>,
                _: &str,
                _: serde_json::Value,
                _: FhirVersion,
            ) -> Result<(), String> {
                Err("connection refused".to_string())
            }
        }

        let sink = DatabaseSink::new(Arc::new(FailingStorage), FhirVersion::default())
            .with_retry_delay(Duration::from_millis(1));
        let event = AuditEventBuilder::new("Device/hfs").build();
        // Should not panic
        sink.record(event).await;
    }

    /// Storage that fails until switched on, then records what it receives.
    struct FlakyStorage {
        up: std::sync::atomic::AtomicBool,
        events: Mutex<Vec<(Option<String>, serde_json::Value)>>,
    }

    #[async_trait]
    impl AuditStorage for FlakyStorage {
        async fn create_resource(
            &self,
            tenant: Option<&str>,
            _: &str,
            resource: serde_json::Value,
            _: FhirVersion,
        ) -> Result<(), String> {
            if !self.up.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("database is down".into());
            }
            self.events
                .lock()
                .await
                .push((tenant.map(str::to_string), resource));
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_outage_spills_to_fallback_and_replays() {
        let dir = tempfile::tempdir().unwrap();
        let fallback = dir.path().join("audit-fallback.ndjson");
        let storage = Arc::new(FlakyStorage {
            up: false.into(),
            events: Mutex::new(Vec::new()),
        });
        let sink = DatabaseSink::new(Arc::clone(&storage) as _, FhirVersion::default())
            .with_fallback(&fallback)
            .with_retry_delay(Duration::from_millis(1));

        let event = AuditEventBuilder::new("Device/hfs")
            .action(AuditAction::Read)
            .build();
        let id = event.id.clone().unwrap().value.unwrap();
        sink.record_in(Some("acme"), event).await;
        assert!(storage.events.lock().await.is_empty());
        let spilled = std::fs::read_to_string(&fallback).unwrap();
        assert_eq!(spilled.lines().count(), 1);

        storage.up.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(sink.replay_fallback().await.unwrap(), 1);
        let events = storage.events.lock().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0.as_deref(), Some("acme"));
        assert_eq!(events[0].1["id"], id.as_str());
        assert!(!fallback.exists(), "the fallback file is moved aside");
        // Nothing left to replay.
        drop(events);
        assert_eq!(sink.replay_fallback().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_tenant_is_passed_to_storage() {
        let storage = Arc::new(FlakyStorage {
            up: true.into(),
            events: Mutex::new(Vec::new()),
        });
        let sink = DatabaseSink::new(Arc::clone(&storage) as _, FhirVersion::default());
        sink.record_in(Some("t1"), AuditEventBuilder::new("Device/hfs").build())
            .await;
        sink.record(AuditEventBuilder::new("Device/hfs").build())
            .await;
        let events = storage.events.lock().await;
        assert_eq!(events[0].0.as_deref(), Some("t1"));
        assert_eq!(events[1].0, None);
    }

    #[test]
    fn test_name() {
        let storage = Arc::new(MockStorage::new());
        let sink = DatabaseSink::new(storage, FhirVersion::default());
        assert_eq!(sink.name(), "database");
    }
}
