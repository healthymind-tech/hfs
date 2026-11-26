//! Database audit sink.
//!
//! Persists `AuditEvent` resources via the FHIR storage backend by serializing
//! the typed struct to `serde_json::Value` and calling `ResourceStorage::create()`.
//!
//! A failed write is retried with exponential backoff; an event that still
//! cannot be written is spilled to the fallback NDJSON file (replayed on the
//! next start) or, without one, counted in `audit_events_dropped_total`.
//! (Fork design kept over upstream's transient-only retry from #2234: it never
//! drops an event while a fallback file is configured.)

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::fhir_model::AuditEvent;
use async_trait::async_trait;
use helios_fhir::FhirVersion;

use crate::sink::AuditSink;
use helios_observability::audit_metrics as m;

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

    /// Persist several resources of one type in `tenant`; one result per
    /// resource, in order. The default writes them one by one; stores with a
    /// batch write override it.
    async fn create_resources(
        &self,
        tenant: Option<&str>,
        resource_type: &str,
        resources: Vec<serde_json::Value>,
        fhir_version: FhirVersion,
    ) -> Vec<Result<(), String>> {
        let mut out = Vec::with_capacity(resources.len());
        for r in resources {
            out.push(
                self.create_resource(tenant, resource_type, r, fhir_version)
                    .await,
            );
        }
        out
    }

    /// Appends a batch of `AuditEvent`s (tenant, resource) in one write, for
    /// stores with a dedicated audit table. `None`: the store has no such
    /// write and the caller uses [`Self::create_resources`]. `Some(Err(_))`:
    /// nothing of the batch was stored.
    async fn append_events(
        &self,
        events: Vec<(Option<String>, serde_json::Value)>,
    ) -> Option<Result<(), String>> {
        let _ = events;
        None
    }

    /// Whether [`Self::append_events`] is implemented.
    fn appends(&self) -> bool {
        false
    }
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
    /// Create a new database sink (default retry delay, no fallback file).
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
                let mut value = value;
                if let Some(obj) = value.as_object_mut() {
                    obj.entry("resourceType")
                        .or_insert_with(|| "AuditEvent".into());
                }
                let line = serde_json::json!({ "tenant": tenant, "resource": value });
                let _guard = self.fallback_lock.lock().await;
                match append_line(path, &line.to_string()).await {
                    Ok(()) => {
                        m::record_spilled(1, m::SpillReason::WriteFailure);
                        tracing::error!(
                        error = %last_error,
                        fallback = %path.display(),
                        "Failed to persist audit event to database; spilled to fallback file"
                        )
                    }
                    Err(fe) => {
                        m::record_dropped(1, m::DropReason::FallbackFailed);
                        tracing::error!(
                        error = %last_error,
                        fallback_error = %fe,
                        "Failed to persist audit event to database and to the fallback file; event lost"
                        )
                    }
                }
            }
            None => {
                m::record_dropped(1, m::DropReason::NoFallback);
                tracing::error!(error = %last_error, "Failed to persist audit event to database; event lost")
            }
        }
    }

    /// Stores a batch with one append. A failing append is retried with
    /// back-off; if it keeps failing each event goes through the single write
    /// (and from there, if need be, to the fallback file), so one event the
    /// store refuses cannot hold back the rest of its batch.
    async fn append(&self, records: Vec<crate::sink::AuditRecord>) {
        let mut events = Vec::with_capacity(records.len());
        for r in records {
            match to_resource(&r.event) {
                Ok(v) => events.push((r.tenant, v)),
                Err(e) => {
                    m::record_dropped(1, m::DropReason::Serialization);
                    tracing::error!(error = %e, "Failed to serialize audit event to JSON Value");
                }
            }
        }
        if events.is_empty() {
            return;
        }
        let mut delay = self.retry_delay;
        for attempt in 1..=WRITE_ATTEMPTS {
            match self.storage.append_events(events.clone()).await {
                Some(Ok(())) => return,
                Some(Err(e)) => {
                    tracing::warn!(error = %e, attempt, events = events.len(), "Audit batch append failed");
                    if attempt < WRITE_ATTEMPTS {
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                    }
                }
                None => break,
            }
        }
        for (tenant, value) in events {
            self.write(tenant.as_deref(), value).await;
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
        m::record_replayed(replayed as u64);
        Ok(replayed)
    }
}

/// The event as a FHIR resource (the typed struct serializes without
/// `resourceType`, which the stored copy and the archives must carry).
fn to_resource(event: &AuditEvent) -> Result<serde_json::Value, serde_json::Error> {
    let mut value = serde_json::to_value(event)?;
    if let Some(obj) = value.as_object_mut() {
        obj.entry("resourceType")
            .or_insert_with(|| "AuditEvent".into());
    }
    Ok(value)
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
        let value = match to_resource(&event) {
            Ok(v) => v,
            Err(e) => {
                m::record_dropped(1, m::DropReason::Serialization);
                tracing::error!(error = %e, "Failed to serialize audit event to JSON Value");
                return;
            }
        };
        self.write(tenant, value).await;
    }

    /// Writes a batch with one storage call per tenant; an event the batch
    /// write refused goes through the retrying single write (and from there,
    /// if need be, to the fallback file).
    async fn record_records(&self, records: Vec<crate::sink::AuditRecord>) {
        if self.storage.appends() {
            return self.append(records).await;
        }
        let mut groups: Vec<(Option<String>, Vec<serde_json::Value>)> = Vec::new();
        for r in records {
            let value = match to_resource(&r.event) {
                Ok(v) => v,
                Err(e) => {
                    m::record_dropped(1, m::DropReason::Serialization);
                    tracing::error!(error = %e, "Failed to serialize audit event to JSON Value");
                    continue;
                }
            };
            match groups.iter_mut().find(|(t, _)| *t == r.tenant) {
                Some((_, values)) => values.push(value),
                None => groups.push((r.tenant, vec![value])),
            }
        }
        for (tenant, values) in groups {
            let results = self
                .storage
                .create_resources(
                    tenant.as_deref(),
                    "AuditEvent",
                    values.clone(),
                    self.fhir_version,
                )
                .await;
            for (value, result) in values.into_iter().zip(results) {
                if result.is_err() {
                    self.write(tenant.as_deref(), value).await;
                }
            }
        }
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
        assert_eq!(events[0]["resourceType"], "AuditEvent");
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
                Err("constraint violation".into())
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

    #[tokio::test]
    async fn test_batches_are_grouped_by_tenant() {
        let storage = Arc::new(FlakyStorage {
            up: true.into(),
            events: Mutex::new(Vec::new()),
        });
        let sink = DatabaseSink::new(Arc::clone(&storage) as _, FhirVersion::default());
        let rec = |t: &str| crate::sink::AuditRecord {
            tenant: Some(t.to_string()),
            event: AuditEventBuilder::new("Device/hfs").build(),
        };
        sink.record_records(vec![rec("a"), rec("b"), rec("a")])
            .await;
        let events = storage.events.lock().await;
        let tenants: Vec<_> = events.iter().map(|e| e.0.clone().unwrap()).collect();
        assert_eq!(tenants, vec!["a", "a", "b"]);
    }
}
