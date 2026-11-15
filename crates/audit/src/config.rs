//! Audit subsystem configuration.
//!
//! Loaded from environment variables prefixed with `HFS_AUDIT_`.

use std::env;
use std::fmt;

use crate::exclusion::ExclusionRule;

/// Which audit backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditBackend {
    /// Audit logging disabled (default).
    None,
    /// Append-only NDJSON file.
    File,
    /// Persist via the FHIR storage backend (SQLite / PostgreSQL / S3).
    Database,
    /// AWS CloudWatch Logs (requires `cloudwatch` feature).
    CloudWatch,
}

impl fmt::Display for AuditBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::File => write!(f, "file"),
            Self::Database => write!(f, "database"),
            Self::CloudWatch => write!(f, "cloudwatch"),
        }
    }
}

/// Top-level audit configuration.
#[derive(Debug, Clone)]
pub struct AuditConfig {
    /// Active backend.
    pub backend: AuditBackend,
    /// File path for the file sink (required when `backend = File`).
    pub file_path: Option<String>,
    /// Optional dedicated database URL for audit events.
    /// When absent the main HFS storage backend is reused.
    pub database_url: Option<String>,
    /// Optional dedicated MongoDB database name for audit events.
    /// Used when the primary backend family is MongoDB.
    pub mongodb_database: Option<String>,
    /// Optional dedicated S3 bucket for audit events.
    /// Used when the primary backend family is S3.
    pub s3_bucket: Option<String>,
    /// Optional dedicated S3 prefix for audit events.
    /// Used when the primary backend family is S3.
    pub s3_prefix: Option<String>,
    /// Optional dedicated S3 region for audit events.
    /// Used when the primary backend family is S3.
    pub s3_region: Option<String>,
    /// Optional dedicated S3 bucket validation toggle for audit events.
    /// Used when the primary backend family is S3.
    pub s3_validate_buckets: Option<bool>,
    /// Path/method pairs that should be excluded from audit logging.
    pub exclusions: Vec<ExclusionRule>,
    /// FHIR Reference used as `AuditEvent.source.observer`
    /// (e.g. `"Device/hfs"`).
    pub source_observer: String,
    /// CloudWatch Logs log group name (required when `backend = CloudWatch`).
    pub cloudwatch_log_group: Option<String>,
    /// CloudWatch Logs log stream name (defaults to `"hfs-audit"`).
    pub cloudwatch_log_stream: Option<String>,
    /// AWS region override for CloudWatch Logs.
    pub cloudwatch_region: Option<String>,
    /// Use the first `X-Forwarded-For` address as the client address
    /// (`HFS_AUDIT_TRUST_FORWARDED_FOR`). Only behind a reverse proxy that
    /// sets the header, since a client can send any value.
    pub trust_forwarded_for: bool,
    /// Tenant that receives every audit event (`HFS_AUDIT_TENANT`). `None`:
    /// the tenant of the audited request, or the server's default tenant for
    /// events outside any request.
    pub tenant: Option<String>,
    /// Capacity of the in-memory write queue (`HFS_AUDIT_QUEUE_CAPACITY`).
    pub queue_capacity: usize,
    /// Events written per batch (`HFS_AUDIT_BATCH_SIZE`).
    pub batch_size: usize,
    /// Concurrent writer tasks (`HFS_AUDIT_WRITERS`).
    pub writers: usize,
    /// Longest wait for the queue to drain on shutdown, in seconds
    /// (`HFS_AUDIT_SHUTDOWN_TIMEOUT`, default 5); what is still queued then is
    /// written to the fallback file. Keep it below `HFS_SHUTDOWN_TIMEOUT`.
    pub shutdown_timeout_secs: u64,
    /// NDJSON file events are spilled to when the database cannot take them
    /// (`HFS_AUDIT_FALLBACK_FILE`); replayed on the next start.
    pub fallback_file: Option<String>,
    /// Also record the per-request authentication/authorization events of the
    /// auth layer (`HFS_AUDIT_AUTH_EVENTS`, default `false`). The REST events
    /// already carry the user and record 401/403 refusals, so these only
    /// triple the volume.
    pub auth_events: bool,
    /// Retention policy (`HFS_AUDIT_RETENTION_*`).
    pub retention: RetentionConfig,
}

/// How long stored audit events are kept (database backend).
#[derive(Debug, Clone, Default)]
pub struct RetentionConfig {
    /// Purge events last updated more than this many days ago
    /// (`HFS_AUDIT_RETENTION_DAYS`). `None` keeps events forever.
    pub days: Option<u32>,
    /// Hours between purge runs (`HFS_AUDIT_RETENTION_INTERVAL_HOURS`, default 24).
    pub interval_hours: u64,
    /// Rows deleted per transaction (`HFS_AUDIT_RETENTION_BATCH`, default 1000).
    pub batch: usize,
    /// Directory purged events are archived to as gzip NDJSON before they are
    /// deleted (`HFS_AUDIT_RETENTION_ARCHIVE_DIR`). `None`: no archive.
    pub archive_dir: Option<String>,
}

impl RetentionConfig {
    fn from_env() -> Self {
        Self {
            days: env::var("HFS_AUDIT_RETENTION_DAYS")
                .ok()
                .and_then(|v| v.trim().parse::<u32>().ok())
                .filter(|d| *d > 0),
            interval_hours: env_parse("HFS_AUDIT_RETENTION_INTERVAL_HOURS", 24u64).max(1),
            batch: env_parse("HFS_AUDIT_RETENTION_BATCH", 1000usize).max(1),
            archive_dir: env::var("HFS_AUDIT_RETENTION_ARCHIVE_DIR")
                .ok()
                .filter(|v| !v.trim().is_empty()),
        }
    }
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn env_bool(key: &str) -> bool {
    env::var(key)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

impl AuditConfig {
    /// Load configuration from `HFS_AUDIT_*` environment variables.
    pub fn from_env() -> Self {
        let backend = match env::var("HFS_AUDIT_BACKEND")
            .unwrap_or_else(|_| "none".to_string())
            .to_lowercase()
            .as_str()
        {
            "file" => AuditBackend::File,
            "database" | "db" => AuditBackend::Database,
            "cloudwatch" | "cloudwatch-logs" | "cwl" => AuditBackend::CloudWatch,
            _ => AuditBackend::None,
        };

        let exclusions = env::var("HFS_AUDIT_EXCLUDE_PATHS")
            .ok()
            .map(|paths| {
                paths
                    .split(',')
                    .filter(|p| !p.trim().is_empty())
                    .map(|p| ExclusionRule::parse(p.trim()))
                    .collect()
            })
            .unwrap_or_default();

        Self {
            backend,
            file_path: env::var("HFS_AUDIT_FILE_PATH").ok(),
            database_url: env::var("HFS_AUDIT_DATABASE_URL").ok(),
            mongodb_database: env::var("HFS_AUDIT_MONGODB_DATABASE").ok(),
            s3_bucket: env::var("HFS_AUDIT_S3_BUCKET").ok(),
            s3_prefix: env::var("HFS_AUDIT_S3_PREFIX").ok(),
            s3_region: env::var("HFS_AUDIT_S3_REGION").ok(),
            s3_validate_buckets: env::var("HFS_AUDIT_S3_VALIDATE_BUCKETS").ok().map(|v| {
                let normalized = v.trim().to_ascii_lowercase();
                !(normalized == "false" || normalized == "0")
            }),
            exclusions,
            source_observer: env::var("HFS_AUDIT_SOURCE_OBSERVER")
                .unwrap_or_else(|_| "Device/hfs".to_string()),
            cloudwatch_log_group: env::var("HFS_AUDIT_CLOUDWATCH_LOG_GROUP").ok(),
            cloudwatch_log_stream: env::var("HFS_AUDIT_CLOUDWATCH_LOG_STREAM").ok(),
            cloudwatch_region: env::var("HFS_AUDIT_CLOUDWATCH_REGION").ok(),
            trust_forwarded_for: env_bool("HFS_AUDIT_TRUST_FORWARDED_FOR"),
            tenant: env::var("HFS_AUDIT_TENANT")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            queue_capacity: env_parse("HFS_AUDIT_QUEUE_CAPACITY", 10_000usize).max(1),
            batch_size: env_parse("HFS_AUDIT_BATCH_SIZE", 64usize).max(1),
            writers: env_parse("HFS_AUDIT_WRITERS", 4usize).max(1),
            shutdown_timeout_secs: env_parse("HFS_AUDIT_SHUTDOWN_TIMEOUT", 5u64),
            fallback_file: env::var("HFS_AUDIT_FALLBACK_FILE")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            auth_events: env_bool("HFS_AUDIT_AUTH_EVENTS"),
            retention: RetentionConfig::from_env(),
        }
    }

    /// Queue tuning derived from this configuration.
    pub fn queue_config(&self) -> crate::sinks::QueueConfig {
        crate::sinks::QueueConfig {
            capacity: self.queue_capacity,
            batch_size: self.batch_size,
            workers: self.writers,
            flush_timeout: std::time::Duration::from_secs(self.shutdown_timeout_secs),
            spill_file: self.fallback_file.as_ref().map(std::path::PathBuf::from),
        }
    }
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            backend: AuditBackend::None,
            file_path: None,
            database_url: None,
            mongodb_database: None,
            s3_bucket: None,
            s3_prefix: None,
            s3_region: None,
            s3_validate_buckets: None,
            exclusions: Vec::new(),
            source_observer: "Device/hfs".to_string(),
            cloudwatch_log_group: None,
            cloudwatch_log_stream: None,
            cloudwatch_region: None,
            trust_forwarded_for: false,
            tenant: None,
            queue_capacity: 10_000,
            batch_size: 64,
            writers: 4,
            shutdown_timeout_secs: 5,
            fallback_file: None,
            auth_events: false,
            retention: RetentionConfig {
                days: None,
                interval_hours: 24,
                batch: 1000,
                archive_dir: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    const AUDIT_ENV_KEYS: &[&str] = &[
        "HFS_AUDIT_BACKEND",
        "HFS_AUDIT_FILE_PATH",
        "HFS_AUDIT_DATABASE_URL",
        "HFS_AUDIT_MONGODB_DATABASE",
        "HFS_AUDIT_S3_BUCKET",
        "HFS_AUDIT_S3_PREFIX",
        "HFS_AUDIT_S3_REGION",
        "HFS_AUDIT_S3_VALIDATE_BUCKETS",
        "HFS_AUDIT_EXCLUDE_PATHS",
        "HFS_AUDIT_SOURCE_OBSERVER",
        "HFS_AUDIT_CLOUDWATCH_LOG_GROUP",
        "HFS_AUDIT_CLOUDWATCH_LOG_STREAM",
        "HFS_AUDIT_CLOUDWATCH_REGION",
        "HFS_AUDIT_TRUST_FORWARDED_FOR",
        "HFS_AUDIT_TENANT",
        "HFS_AUDIT_QUEUE_CAPACITY",
        "HFS_AUDIT_BATCH_SIZE",
        "HFS_AUDIT_WRITERS",
        "HFS_AUDIT_SHUTDOWN_TIMEOUT",
        "HFS_AUDIT_FALLBACK_FILE",
        "HFS_AUDIT_AUTH_EVENTS",
        "HFS_AUDIT_RETENTION_DAYS",
        "HFS_AUDIT_RETENTION_INTERVAL_HOURS",
        "HFS_AUDIT_RETENTION_BATCH",
        "HFS_AUDIT_RETENTION_ARCHIVE_DIR",
        "HFS_AUDIT_EXCLUDE_PATHS",
    ];

    fn clear_audit_env() {
        for key in AUDIT_ENV_KEYS {
            unsafe { env::remove_var(key) };
        }
    }

    #[test]
    fn test_default_config() {
        let config = AuditConfig::default();
        assert_eq!(config.backend, AuditBackend::None);
        assert!(config.file_path.is_none());
        assert!(config.database_url.is_none());
        assert!(config.mongodb_database.is_none());
        assert!(config.s3_bucket.is_none());
        assert!(config.s3_prefix.is_none());
        assert!(config.s3_region.is_none());
        assert!(config.s3_validate_buckets.is_none());
        assert!(config.exclusions.is_empty());
        assert_eq!(config.source_observer, "Device/hfs");
    }

    #[test]
    fn test_backend_display() {
        assert_eq!(AuditBackend::None.to_string(), "none");
        assert_eq!(AuditBackend::File.to_string(), "file");
        assert_eq!(AuditBackend::Database.to_string(), "database");
        assert_eq!(AuditBackend::CloudWatch.to_string(), "cloudwatch");
    }

    #[test]
    fn test_from_env_file_backend() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        clear_audit_env();
        unsafe {
            env::set_var("HFS_AUDIT_BACKEND", "file");
            env::set_var("HFS_AUDIT_FILE_PATH", "/tmp/audit.log");
        }
        let config = AuditConfig::from_env();
        assert_eq!(config.backend, AuditBackend::File);
        assert_eq!(config.file_path.as_deref(), Some("/tmp/audit.log"));
        clear_audit_env();
    }

    #[test]
    fn test_from_env_database_backend() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        clear_audit_env();
        unsafe {
            env::set_var("HFS_AUDIT_BACKEND", "database");
        }
        let config = AuditConfig::from_env();
        assert_eq!(config.backend, AuditBackend::Database);
        clear_audit_env();
    }

    #[test]
    fn test_from_env_db_alias() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        clear_audit_env();
        unsafe {
            env::set_var("HFS_AUDIT_BACKEND", "db");
        }
        let config = AuditConfig::from_env();
        assert_eq!(config.backend, AuditBackend::Database);
        clear_audit_env();
    }

    #[test]
    fn test_from_env_cloudwatch_backend_aliases() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        clear_audit_env();
        unsafe {
            env::set_var("HFS_AUDIT_BACKEND", "cloudwatch-logs");
            env::set_var("HFS_AUDIT_CLOUDWATCH_LOG_GROUP", "/hfs/audit");
            env::set_var("HFS_AUDIT_CLOUDWATCH_LOG_STREAM", "ci-stream");
            env::set_var("HFS_AUDIT_CLOUDWATCH_REGION", "us-east-1");
        }

        let config = AuditConfig::from_env();
        assert_eq!(config.backend, AuditBackend::CloudWatch);
        assert_eq!(config.cloudwatch_log_group.as_deref(), Some("/hfs/audit"));
        assert_eq!(config.cloudwatch_log_stream.as_deref(), Some("ci-stream"));
        assert_eq!(config.cloudwatch_region.as_deref(), Some("us-east-1"));

        clear_audit_env();
    }

    #[test]
    fn test_from_env_unset_defaults_to_none() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        clear_audit_env();
        let config = AuditConfig::from_env();
        assert_eq!(config.backend, AuditBackend::None);
    }

    #[test]
    fn test_from_env_queue_tenant_and_retention() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        clear_audit_env();
        let defaults = AuditConfig::from_env();
        assert_eq!(defaults.retention.days, None);
        assert_eq!(defaults.retention.interval_hours, 24);
        assert!(!defaults.trust_forwarded_for);
        assert!(!defaults.auth_events);
        assert_eq!(defaults.tenant, None);
        assert_eq!(defaults.writers, 4);
        unsafe {
            env::set_var("HFS_AUDIT_TENANT", "audit");
            env::set_var("HFS_AUDIT_TRUST_FORWARDED_FOR", "true");
            env::set_var("HFS_AUDIT_QUEUE_CAPACITY", "50");
            env::set_var("HFS_AUDIT_WRITERS", "2");
            env::set_var("HFS_AUDIT_RETENTION_DAYS", "2190");
            env::set_var(
                "HFS_AUDIT_RETENTION_ARCHIVE_DIR",
                "/var/lib/hfs/audit-archive",
            );
            env::set_var("HFS_AUDIT_EXCLUDE_PATHS", "/ping, GET /Basic/*");
        }
        let c = AuditConfig::from_env();
        assert_eq!(c.tenant.as_deref(), Some("audit"));
        assert!(c.trust_forwarded_for);
        assert_eq!(c.queue_config().capacity, 50);
        assert_eq!(c.queue_config().workers, 2);
        assert_eq!(c.retention.days, Some(2190));
        assert_eq!(
            c.retention.archive_dir.as_deref(),
            Some("/var/lib/hfs/audit-archive")
        );
        assert_eq!(c.exclusions.len(), 2);
        assert_eq!(c.exclusions[1].method.as_deref(), Some("GET"));
        assert_eq!(c.exclusions[1].path, "/Basic/*");
        clear_audit_env();
    }

    #[test]
    fn test_from_env_custom_source_observer() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        clear_audit_env();
        unsafe {
            env::set_var("HFS_AUDIT_SOURCE_OBSERVER", "Device/my-server");
        }
        let config = AuditConfig::from_env();
        assert_eq!(config.source_observer, "Device/my-server");
        clear_audit_env();
    }

    #[test]
    fn test_from_env_database_overrides() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        clear_audit_env();
        unsafe {
            env::set_var(
                "HFS_AUDIT_DATABASE_URL",
                "postgresql://audit:pass@localhost:5432/audit",
            );
            env::set_var("HFS_AUDIT_MONGODB_DATABASE", "helios_audit");
            env::set_var("HFS_AUDIT_S3_BUCKET", "audit-bucket");
            env::set_var("HFS_AUDIT_S3_PREFIX", "audit/");
            env::set_var("HFS_AUDIT_S3_REGION", "us-east-1");
            env::set_var("HFS_AUDIT_S3_VALIDATE_BUCKETS", "false");
        }

        let config = AuditConfig::from_env();
        assert_eq!(
            config.database_url.as_deref(),
            Some("postgresql://audit:pass@localhost:5432/audit")
        );
        assert_eq!(config.mongodb_database.as_deref(), Some("helios_audit"));
        assert_eq!(config.s3_bucket.as_deref(), Some("audit-bucket"));
        assert_eq!(config.s3_prefix.as_deref(), Some("audit/"));
        assert_eq!(config.s3_region.as_deref(), Some("us-east-1"));
        assert_eq!(config.s3_validate_buckets, Some(false));

        clear_audit_env();
    }
}
