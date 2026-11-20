//! Sync and duplicate contracts (package P6 implements the engines and the
//! PostgreSQL [`SyncStore`] over fork migration `hm-0010-endpoint-sync`).
//!
//! A [`SyncJob`] copies resources one way from a source to a target, each an
//! endpoint of this server or an external FHIR server. A job runs as
//! [`SyncRun`]s: an optional initial load, then incremental passes driven by
//! its [`SyncTrigger`]. Runs are claimed through the shared `cluster_jobs`
//! queue (kind `endpoint-sync`, lease + fencing token) so exactly one
//! instance works a run, a crashed instance's run resumes elsewhere from its
//! last checkpointed [`SyncCursor`], and progress / log / conflicts are
//! readable from every instance.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::FhirRelease;
use crate::secret::SealedSecret;

/// Resource types never synced unless listed explicitly in
/// [`SyncJobSpec::resource_types`]: audit trails belong to the server that
/// produced them, and subscriptions would fire from two places.
pub const DEFAULT_EXCLUDED_TYPES: &[&str] = &["AuditEvent", "Subscription", "SubscriptionStatus"];

/// One side of a sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SyncPeer {
    /// An endpoint of this server, by name (accessed in-process through
    /// storage, never over HTTP).
    Endpoint {
        /// Endpoint name.
        name: String,
    },
    /// An external FHIR server.
    External(ExternalServer),
}

/// An external FHIR server. Every URL passes the outbound policy
/// ([`crate::outbound::OutboundPolicy`]) before each connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalServer {
    /// FHIR base URL.
    pub base_url: String,
    /// Its FHIR release (`None` = read from its CapabilityStatement).
    #[serde(default)]
    pub fhir_version: Option<FhirRelease>,
    /// How to authenticate.
    #[serde(default)]
    pub auth: ExternalAuth,
    /// Extra non-secret request headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Requests per second ceiling towards this server (`None` = 10).
    #[serde(default)]
    pub max_requests_per_second: Option<u32>,
}

/// Credentials for an external server. Secrets are sealed with AAD
/// `sync-job:{job id}:{side}.auth.{field}` (`side` = `source` | `target`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ExternalAuth {
    /// No credentials.
    #[default]
    None,
    /// Static bearer token.
    Bearer {
        /// Sealed token.
        token: SealedSecret,
    },
    /// HTTP Basic.
    Basic {
        /// User name.
        username: String,
        /// Sealed password.
        password: SealedSecret,
    },
    /// OAuth client credentials with a client secret.
    #[serde(rename_all = "camelCase")]
    ClientCredentials {
        /// Token endpoint.
        token_url: String,
        /// Client id.
        client_id: String,
        /// Sealed client secret.
        client_secret: SealedSecret,
        /// Requested scope.
        #[serde(default)]
        scope: Option<String>,
    },
    /// SMART Backend Services (`private_key_jwt`).
    #[serde(rename_all = "camelCase")]
    PrivateKeyJwt {
        /// Token endpoint.
        token_url: String,
        /// Client id.
        client_id: String,
        /// Sealed private JWK (JSON).
        private_key_jwk: SealedSecret,
        /// `RS384` or `ES384`.
        alg: String,
        /// Key id.
        #[serde(default)]
        kid: Option<String>,
        /// Requested scope.
        #[serde(default)]
        scope: Option<String>,
    },
}

/// How the target is first filled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum InitialLoad {
    /// Start incremental from "now" (or from `since`).
    None,
    /// Endpoint → endpoint on PostgreSQL: one set-based copy of the
    /// tenant's rows (resources, optional history, search index) inside the
    /// database. Fastest; only between local endpoints.
    StorageCopy,
    /// Page through `GET [base]/{type}?_count=N&_sort=_lastUpdated` per type.
    /// Works against any FHIR server.
    #[default]
    PagedRead,
    /// Bulk `$export` at the source, then ingest the NDJSON at the target
    /// (in-process for a local target, `$bulk-submit` or batches for an
    /// external one).
    BulkExport,
}

/// What starts runs after the initial load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum SyncTrigger {
    /// Only when an operator presses "Run" (or the API is called).
    #[default]
    Manual,
    /// One incremental pass every `every_secs` (≥ 60).
    #[serde(rename_all = "camelCase")]
    Interval {
        /// Period.
        every_secs: u32,
    },
    /// A long-lived run polling `_history?_since` every `poll_secs` (≥ 1 for
    /// local sources, ≥ 10 for external ones).
    #[serde(rename_all = "camelCase")]
    Continuous {
        /// Poll period.
        poll_secs: u32,
    },
    /// External source pushes id-only rest-hook notifications to
    /// `/admin/sync/jobs/{id}/notify` (a Subscription the job creates at the
    /// source); each notification wakes an incremental pass. Falls back to
    /// polling every `fallback_poll_secs`.
    #[serde(rename_all = "camelCase")]
    Subscription {
        /// Safety-net poll period.
        fallback_poll_secs: u32,
    },
}

/// What happens when the target already holds a different version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictPolicy {
    /// Overwrite the target (one-way mirror).
    #[default]
    SourceWins,
    /// Keep the target when its `meta.lastUpdated` is newer than the source
    /// version's; record a conflict.
    SkipIfTargetNewer,
    /// Never overwrite a target resource changed since this job last wrote
    /// it (content hash differs from `hfs_sync_item_state`); record a
    /// conflict and continue.
    Fail,
}

/// Resource ids at the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum IdStrategy {
    /// Same ids (`PUT [type]/[id]`); the target must allow client ids.
    #[default]
    Preserve,
}

/// The definition of a sync job (`hfs_sync_jobs.spec`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncJobSpec {
    /// Unique human name.
    pub name: String,
    /// Free text.
    #[serde(default)]
    pub description: Option<String>,
    /// Where resources come from.
    pub source: SyncPeer,
    /// Where they go.
    pub target: SyncPeer,
    /// Types to sync; empty = every type except [`DEFAULT_EXCLUDED_TYPES`].
    #[serde(default)]
    pub resource_types: Vec<String>,
    /// Per-type search filters (`Observation` → `category=laboratory`),
    /// applied at the source.
    #[serde(default)]
    pub filters: BTreeMap<String, String>,
    /// Only resources changed at the source since then.
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    /// Initial load.
    #[serde(default)]
    pub initial_load: InitialLoad,
    /// Copy history versions too (initial `StorageCopy` only).
    #[serde(default)]
    pub include_history: bool,
    /// Incremental trigger.
    #[serde(default)]
    pub trigger: SyncTrigger,
    /// Conflict policy.
    #[serde(default)]
    pub conflict: ConflictPolicy,
    /// Propagate source deletes.
    #[serde(default = "yes")]
    pub propagate_deletes: bool,
    /// Ids at the target.
    #[serde(default)]
    pub id_strategy: IdStrategy,
    /// Resources per target write batch.
    #[serde(default = "default_batch")]
    pub batch_size: u32,
    /// Whether triggers fire.
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

fn default_batch() -> u32 {
    200
}

impl SyncJobSpec {
    /// Static checks (peers exist / loops are checked by the API with the
    /// registry and the other jobs).
    pub fn validate(&self) -> Result<(), crate::model::ValidationErrors> {
        let mut e = crate::model::ValidationErrors::default();
        if self.name.trim().is_empty() || self.name.chars().count() > 120 {
            e.push("name", "must be 1-120 characters");
        }
        if self.source == self.target {
            e.push("target", "must differ from the source");
        }
        if matches!(self.initial_load, InitialLoad::StorageCopy)
            && !(matches!(self.source, SyncPeer::Endpoint { .. })
                && matches!(self.target, SyncPeer::Endpoint { .. }))
        {
            e.push(
                "initialLoad",
                "storage-copy works only between local endpoints",
            );
        }
        if self.include_history && !matches!(self.initial_load, InitialLoad::StorageCopy) {
            e.push("includeHistory", "only with storage-copy");
        }
        match &self.trigger {
            SyncTrigger::Interval { every_secs } if *every_secs < 60 => {
                e.push("trigger.everySecs", "must be at least 60")
            }
            SyncTrigger::Continuous { poll_secs } if *poll_secs < 1 => {
                e.push("trigger.pollSecs", "must be at least 1")
            }
            SyncTrigger::Continuous { poll_secs }
                if *poll_secs < 10 && matches!(self.source, SyncPeer::External(_)) =>
            {
                e.push(
                    "trigger.pollSecs",
                    "must be at least 10 for an external source",
                )
            }
            SyncTrigger::Subscription { .. } if !matches!(self.source, SyncPeer::External(_)) => {
                e.push("trigger", "subscription triggers need an external source")
            }
            _ => {}
        }
        if !(1..=1000).contains(&self.batch_size) {
            e.push("batchSize", "must be 1-1000");
        }
        for peer in [&self.source, &self.target] {
            if let SyncPeer::External(x) = peer {
                match url::Url::parse(&x.base_url) {
                    Ok(u) if matches!(u.scheme(), "http" | "https") && u.username().is_empty() => {}
                    _ => e.push(
                        "baseUrl",
                        "must be an absolute http(s) URL without credentials",
                    ),
                }
            }
        }
        e.into_result()
    }

    /// Whether `resource_type` is in scope.
    pub fn includes_type(&self, resource_type: &str) -> bool {
        if self.resource_types.is_empty() {
            !DEFAULT_EXCLUDED_TYPES.contains(&resource_type)
        } else {
            self.resource_types.iter().any(|t| t == resource_type)
        }
    }
}

/// A stored job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncJob {
    /// UUID.
    pub id: String,
    /// CAS revision.
    pub revision: u64,
    /// Definition.
    pub spec: SyncJobSpec,
    /// Next scheduled run (interval trigger).
    #[serde(default)]
    pub next_run_at: Option<DateTime<Utc>>,
    /// Created.
    pub created_at: DateTime<Utc>,
    /// Updated.
    pub updated_at: DateTime<Utc>,
    /// Actor subject.
    #[serde(default)]
    pub created_by: Option<String>,
}

/// Run state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncRunState {
    /// Waiting for a worker.
    Queued,
    /// Being worked.
    Running,
    /// Finished.
    Succeeded,
    /// Finished with an error (resumable with "Retry": a new run continues
    /// from the failed run's cursor).
    Failed,
    /// Cancelled by an operator.
    Cancelled,
}

impl SyncRunState {
    /// Terminal?
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            SyncRunState::Succeeded | SyncRunState::Failed | SyncRunState::Cancelled
        )
    }
}

/// Why a run started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunReason {
    /// Operator / API.
    Manual,
    /// Interval schedule.
    Schedule,
    /// Continuous trigger.
    Continuous,
    /// Source notification.
    Notification,
    /// Retry of a failed run (resumes its cursor).
    Retry,
    /// Endpoint duplication.
    Duplicate,
}

/// Where a run is, checkpointed after every committed target batch so a run
/// resumes without redoing work (re-applying a batch is harmless: writes are
/// idempotent `PUT`s guarded by the item state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SyncCursor {
    /// `initial` or `incremental`.
    pub phase: String,
    /// Types not yet started (initial load).
    #[serde(default)]
    pub pending_types: Vec<String>,
    /// Type being worked.
    #[serde(default)]
    pub current_type: Option<String>,
    /// Opaque source paging state (next link, export status URL, last id).
    #[serde(default)]
    pub page: Option<String>,
    /// Incremental high-water mark: source `meta.lastUpdated` /
    /// history `_since` of the newest fully applied change.
    #[serde(default)]
    pub high_water: Option<DateTime<Utc>>,
    /// `(type/id/version)` already applied at exactly `high_water`, so the
    /// next `_since=high_water` page (inclusive) skips them.
    #[serde(default)]
    pub boundary: Vec<String>,
}

/// Counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SyncProgress {
    /// Read at the source.
    pub read: u64,
    /// Created at the target.
    pub created: u64,
    /// Updated at the target.
    pub updated: u64,
    /// Deleted at the target.
    pub deleted: u64,
    /// Unchanged (item-state hash match).
    pub skipped: u64,
    /// Conflicts recorded.
    pub conflicts: u64,
    /// Items that failed permanently.
    pub failed: u64,
    /// Estimated total (initial load), if known.
    #[serde(default)]
    pub estimated_total: Option<u64>,
}

/// One run (`hfs_sync_runs`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncRun {
    /// UUID.
    pub id: String,
    /// Job.
    pub job_id: String,
    /// State.
    pub state: SyncRunState,
    /// Why.
    pub reason: RunReason,
    /// Checkpoint.
    pub cursor: SyncCursor,
    /// Counters.
    pub progress: SyncProgress,
    /// Last error.
    #[serde(default)]
    pub error: Option<String>,
    /// Created.
    pub created_at: DateTime<Utc>,
    /// Started.
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    /// Finished.
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
}

/// Log severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Info.
    Info,
    /// Warning.
    Warn,
    /// Error.
    Error,
}

/// One run log line (`hfs_sync_log`). Never contains credentials or
/// resource content beyond `type/id/version`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncLogLine {
    /// Sequence within the run.
    pub seq: u64,
    /// When.
    pub at: DateTime<Utc>,
    /// Severity.
    pub level: LogLevel,
    /// Message.
    pub message: String,
}

/// A recorded conflict (`hfs_sync_conflicts`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncConflict {
    /// Run.
    pub run_id: String,
    /// Type.
    pub resource_type: String,
    /// Id.
    pub resource_id: String,
    /// Source version.
    #[serde(default)]
    pub source_version: Option<String>,
    /// Target version.
    #[serde(default)]
    pub target_version: Option<String>,
    /// Why.
    pub reason: String,
    /// When.
    pub at: DateTime<Utc>,
}

/// What a duplicate copies (`POST /admin/endpoints/{name}/$duplicate`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateRequest {
    /// New endpoint name (also its path and tenant unless given).
    pub name: String,
    /// New path (default = name).
    #[serde(default)]
    pub path: Option<String>,
    /// New display name (default = "{source} (copy)").
    #[serde(default)]
    pub display_name: Option<String>,
    /// Data to copy.
    #[serde(default)]
    pub data: DuplicateData,
    /// Re-install the source's IG packages into the copy (registry rows and
    /// conformance resources are copied with `current`/`history` data; with
    /// `none` they are installed fresh).
    #[serde(default = "yes")]
    pub packages: bool,
    /// Copy the auth section (issuer etc.; audience should then be changed).
    #[serde(default = "yes")]
    pub auth: bool,
    /// Copy sealed secrets (re-sealed under the new endpoint id's AAD).
    #[serde(default)]
    pub secrets: bool,
    /// Keep a continuous sync from the source afterwards.
    #[serde(default)]
    pub keep_in_sync: bool,
}

/// Data scope of a duplicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DuplicateData {
    /// Configuration only.
    #[default]
    None,
    /// Current versions.
    Current,
    /// Current versions and history.
    History,
}

/// Store errors.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SyncStoreError {
    /// `404`.
    #[error("not found")]
    NotFound,
    /// `409`.
    #[error("revision conflict: current revision is {current}")]
    Conflict {
        /// Stored revision.
        current: u64,
    },
    /// `409`.
    #[error("job name {0:?} is taken")]
    DuplicateName(String),
    /// The fencing token is stale: another worker owns the run now; stop.
    #[error("lease lost")]
    LeaseLost,
    /// `503`.
    #[error("sync store unavailable: {0}")]
    Unavailable(String),
}

/// Persistence of jobs, runs, logs, conflicts and item state.
#[async_trait]
pub trait SyncStore: Send + Sync {
    /// Creates a job.
    async fn create_job(&self, spec: SyncJobSpec, actor: &str) -> Result<SyncJob, SyncStoreError>;
    /// CAS update.
    async fn update_job(
        &self,
        id: &str,
        expected_revision: u64,
        spec: SyncJobSpec,
    ) -> Result<SyncJob, SyncStoreError>;
    /// One job.
    async fn job(&self, id: &str) -> Result<Option<SyncJob>, SyncStoreError>;
    /// All jobs.
    async fn jobs(&self) -> Result<Vec<SyncJob>, SyncStoreError>;
    /// Deletes a job with its runs, logs, conflicts and item state.
    async fn delete_job(&self, id: &str) -> Result<(), SyncStoreError>;
    /// Records a queued run (the caller enqueues the `cluster_jobs` row).
    async fn create_run(
        &self,
        job_id: &str,
        reason: RunReason,
        resume_from: Option<SyncCursor>,
    ) -> Result<SyncRun, SyncStoreError>;
    /// One run.
    async fn run(&self, id: &str) -> Result<Option<SyncRun>, SyncStoreError>;
    /// A job's runs, newest first.
    async fn runs(&self, job_id: &str, limit: usize) -> Result<Vec<SyncRun>, SyncStoreError>;
    /// Saves cursor + progress (+ state) iff `fencing_token` still owns it.
    async fn checkpoint(
        &self,
        run_id: &str,
        fencing_token: i64,
        state: SyncRunState,
        cursor: &SyncCursor,
        progress: &SyncProgress,
        error: Option<&str>,
    ) -> Result<(), SyncStoreError>;
    /// Appends a log line (oldest lines beyond 10 000 per run are pruned).
    async fn log(&self, run_id: &str, level: LogLevel, message: &str)
    -> Result<(), SyncStoreError>;
    /// Log lines after `after_seq`.
    async fn log_lines(
        &self,
        run_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SyncLogLine>, SyncStoreError>;
    /// Records a conflict.
    async fn conflict(&self, c: &SyncConflict) -> Result<(), SyncStoreError>;
    /// A run's conflicts.
    async fn conflicts(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<SyncConflict>, SyncStoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SyncJobSpec {
        serde_json::from_value(serde_json::json!({
            "name": "prod to test",
            "source": {"kind": "endpoint", "name": "default"},
            "target": {"kind": "external", "baseUrl": "https://hapi.example.org/fhir",
                       "auth": {"type": "none"}},
            "trigger": {"type": "interval", "everySecs": 300}
        }))
        .unwrap()
    }

    #[test]
    fn json_shape_and_defaults() {
        let s = spec();
        assert_eq!(s.initial_load, InitialLoad::PagedRead);
        assert_eq!(s.conflict, ConflictPolicy::SourceWins);
        assert!(s.propagate_deletes && s.enabled);
        assert_eq!(s.batch_size, 200);
        assert!(s.validate().is_ok());
        let back: SyncJobSpec = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn validation() {
        let mut s = spec();
        s.initial_load = InitialLoad::StorageCopy;
        s.trigger = SyncTrigger::Interval { every_secs: 5 };
        s.batch_size = 0;
        let f: Vec<_> = s
            .validate()
            .unwrap_err()
            .0
            .into_iter()
            .map(|e| e.field)
            .collect();
        assert!(f.contains(&"initialLoad".to_string()));
        assert!(f.contains(&"trigger.everySecs".to_string()));
        assert!(f.contains(&"batchSize".to_string()));
        let mut same = spec();
        same.target = same.source.clone();
        assert!(same.validate().is_err());
    }

    #[test]
    fn type_scope() {
        let mut s = spec();
        assert!(s.includes_type("Patient"));
        assert!(!s.includes_type("AuditEvent"));
        s.resource_types = vec!["AuditEvent".into()];
        assert!(s.includes_type("AuditEvent"));
        assert!(!s.includes_type("Patient"));
    }

    #[test]
    fn duplicate_defaults() {
        let d: DuplicateRequest = serde_json::from_str(r#"{"name":"copy"}"#).unwrap();
        assert_eq!(d.data, DuplicateData::None);
        assert!(d.packages && d.auth && !d.secrets && !d.keep_in_sync);
    }
}
