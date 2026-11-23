//! One configuration surface (design §6).
//!
//! Terminology settings live under `HFS_TERMINOLOGY_*`. Every `HTS_*` variable
//! the standalone server read is classified in [`HTS_ENV_ALIASES`]:
//!
//! * **Renamed** — same meaning, new name (`HTS_PG_POOL_SIZE` →
//!   `HFS_TERMINOLOGY_PG_POOL_SIZE`). The legacy value is adopted when the new
//!   name is unset; when both are set and differ, the new name wins.
//! * **MergedInto** — the unified server has one of these for everything
//!   (`HTS_ADMIN_PASSWORD` → `HFS_ADMIN_PASSWORD`, the break-glass account;
//!   `HTS_LOG_LEVEL` → `HFS_LOG_LEVEL`). Adopted only when the HFS variable is
//!   unset; otherwise reported and ignored.
//! * **Ignored** — meaningless in one process (`HTS_SERVER_PORT`,
//!   `HTS_UI_UPSTREAM_URL`, `HTS_AUDIT_*`).
//!
//! Every hit becomes a [`Deprecation`] the server logs once at startup (WARN)
//! and the console's About page lists. Aliases are honoured for two releases
//! after the unified release, then removed (design §6.3).
//!
//! The standalone `hts` binary keeps reading `HTS_*` unchanged; this module is
//! only consulted by `hfs`.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::schema::{self, SchemaError};

/// How HFS gets terminology.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminologyMode {
    /// HTS runs inside this process (default).
    Embedded,
    /// An external terminology server at `HFS_TERMINOLOGY_SERVER`.
    Remote,
    /// No terminology (`:in` search answers 501, validation uses core
    /// value sets only).
    Off,
}

impl TerminologyMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "embedded" | "in-process" | "internal" | "on" => Some(Self::Embedded),
            "remote" | "external" => Some(Self::Remote),
            "off" | "none" | "disabled" => Some(Self::Off),
            _ => None,
        }
    }
}

/// What happened to a legacy variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasKind {
    Renamed(&'static str),
    MergedInto(&'static str),
    Ignored(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvAlias {
    pub legacy: &'static str,
    pub kind: AliasKind,
}

const fn renamed(legacy: &'static str, canonical: &'static str) -> EnvAlias {
    EnvAlias {
        legacy,
        kind: AliasKind::Renamed(canonical),
    }
}
const fn merged(legacy: &'static str, canonical: &'static str) -> EnvAlias {
    EnvAlias {
        legacy,
        kind: AliasKind::MergedInto(canonical),
    }
}
const fn ignored(legacy: &'static str, why: &'static str) -> EnvAlias {
    EnvAlias {
        legacy,
        kind: AliasKind::Ignored(why),
    }
}

/// Every `HTS_*` variable of the standalone server (crates/hts config.rs,
/// write_guard.rs, postgres/mod.rs, audit.rs, hts-ui login.rs, importers).
/// `HTS_AUDIT_*` is handled by prefix in [`classify`].
pub static HTS_ENV_ALIASES: &[EnvAlias] = &[
    // storage
    renamed("HTS_DATABASE_URL", "HFS_TERMINOLOGY_DATABASE_URL"),
    merged("HTS_STORAGE_BACKEND", "HFS_STORAGE_BACKEND"),
    renamed("HTS_PG_POOL_SIZE", "HFS_TERMINOLOGY_PG_POOL_SIZE"),
    renamed(
        "HTS_PG_POOL_WAIT_TIMEOUT_SECS",
        "HFS_TERMINOLOGY_PG_POOL_WAIT_TIMEOUT_SECS",
    ),
    renamed(
        "HTS_PG_CONNECT_TIMEOUT_SECS",
        "HFS_TERMINOLOGY_PG_CONNECT_TIMEOUT_SECS",
    ),
    renamed(
        "HTS_PG_STATEMENT_TIMEOUT_MS",
        "HFS_TERMINOLOGY_PG_STATEMENT_TIMEOUT_MS",
    ),
    // terminology behaviour
    renamed(
        "HTS_MAX_EXPANSION_SIZE",
        "HFS_TERMINOLOGY_MAX_EXPANSION_SIZE",
    ),
    renamed("HTS_AUTO_SUPPLEMENTS", "HFS_TERMINOLOGY_AUTO_SUPPLEMENTS"),
    renamed("HTS_BOOTSTRAP_DIR", "HFS_TERMINOLOGY_BOOTSTRAP_DIR"),
    renamed(
        "HTS_BOOTSTRAP_BATCH_SIZE",
        "HFS_TERMINOLOGY_BOOTSTRAP_BATCH_SIZE",
    ),
    renamed(
        "HTS_CACHE_SYNC_INTERVAL_MS",
        "HFS_TERMINOLOGY_CACHE_SYNC_INTERVAL_MS",
    ),
    renamed("HTS_IMPORT_LANGUAGES", "HFS_TERMINOLOGY_IMPORT_LANGUAGES"),
    renamed("HTS_LOINC_PROPERTIES", "HFS_TERMINOLOGY_LOINC_PROPERTIES"),
    renamed(
        "HTS_LOINC_PARTS_AND_ANSWERS",
        "HFS_TERMINOLOGY_LOINC_PARTS_AND_ANSWERS",
    ),
    // terminology administration
    renamed("HTS_ADMIN_UI", "HFS_TERMINOLOGY_ADMIN"),
    renamed("HTS_ADMIN_DATA_DIR", "HFS_TERMINOLOGY_DATA_DIR"),
    renamed("HTS_IMPORT_DIR", "HFS_TERMINOLOGY_IMPORT_DIR"),
    renamed("HTS_PACKAGE_REGISTRY", "HFS_TERMINOLOGY_PACKAGE_REGISTRY"),
    renamed("HTS_UPLOAD_CHUNK_SIZE", "HFS_TERMINOLOGY_UPLOAD_CHUNK_SIZE"),
    renamed("HTS_MAX_UPLOAD_SIZE", "HFS_TERMINOLOGY_MAX_UPLOAD_SIZE"),
    renamed("HTS_DISK_HEADROOM", "HFS_TERMINOLOGY_DISK_HEADROOM"),
    // /tx HTTP surface
    renamed(
        "HTS_REQUIRE_AUTH_FOR_WRITES",
        "HFS_TERMINOLOGY_REQUIRE_AUTH_FOR_WRITES",
    ),
    renamed("HTS_MAX_BODY_SIZE", "HFS_TERMINOLOGY_MAX_BODY_SIZE"),
    renamed("HTS_REQUEST_TIMEOUT", "HFS_TERMINOLOGY_REQUEST_TIMEOUT"),
    renamed("HTS_PUBLIC_BASE_URL", "HFS_TERMINOLOGY_PUBLIC_URL"),
    renamed("HTS_AUTH_ISSUER", "HFS_TERMINOLOGY_AUTH_ISSUER"),
    renamed("HTS_AUTH_JWKS_URL", "HFS_TERMINOLOGY_AUTH_JWKS_URL"),
    renamed("HTS_AUTH_AUDIENCE", "HFS_TERMINOLOGY_AUTH_AUDIENCE"),
    renamed("HTS_AUTH_ALGORITHMS", "HFS_TERMINOLOGY_AUTH_ALGORITHMS"),
    renamed(
        "HTS_AUTH_CLOCK_SKEW_SECONDS",
        "HFS_TERMINOLOGY_AUTH_CLOCK_SKEW_SECONDS",
    ),
    // one process: one of each
    merged("HTS_LOG_LEVEL", "HFS_LOG_LEVEL"),
    merged("HTS_SHUTDOWN_TIMEOUT", "HFS_SHUTDOWN_TIMEOUT"),
    merged("HTS_ENABLE_CORS", "HFS_ENABLE_CORS"),
    merged("HTS_CORS_ORIGINS", "HFS_CORS_ORIGINS"),
    merged("HTS_UI_ENABLED", "HFS_UI_ENABLED"),
    merged("HTS_ADMIN_PASSWORD", "HFS_ADMIN_PASSWORD"),
    merged("HTS_ADMIN_PASSWORD_FILE", "HFS_ADMIN_PASSWORD_FILE"),
    merged("HTS_ADMIN_USER", "HFS_ADMIN_USER"),
    merged(
        "HTS_ADMIN_SESSION_IDLE_MINUTES",
        "HFS_ADMIN_SESSION_IDLE_MINUTES",
    ),
    merged("HTS_ADMIN_TRUSTED_PROXIES", "HFS_ADMIN_TRUSTED_PROXIES"),
    merged("HTS_ADMIN_ALLOWED_ORIGINS", "HFS_ADMIN_ALLOWED_ORIGINS"),
    merged("HTS_ADMIN_COOKIE_SECURE", "HFS_ADMIN_COOKIE_SECURE"),
    ignored(
        "HTS_CONSOLE_REQUIRE_LOGIN",
        "terminology pages follow HFS_CONSOLE_AUTH_MODE (one sign-in)",
    ),
    ignored(
        "HTS_SERVER_PORT",
        "terminology is served on the HFS port under /tx",
    ),
    ignored(
        "HTS_SERVER_HOST",
        "terminology is served on the HFS listener",
    ),
    ignored(
        "HTS_UI_UPSTREAM_URL",
        "the terminology console calls the in-process service",
    ),
    ignored("HTS_UI_HFS_CONSOLE_URL", "one console: no product switcher"),
];

/// Classify any `HTS_*` name (`None` for non-HTS names).
pub fn classify(name: &str) -> Option<AliasKind> {
    if !name.starts_with("HTS_") {
        return None;
    }
    if let Some(a) = HTS_ENV_ALIASES.iter().find(|a| a.legacy == name) {
        return Some(a.kind);
    }
    if name.starts_with("HTS_AUDIT_") {
        return Some(AliasKind::Ignored(
            "one audit trail: terminology events use HFS_AUDIT_*",
        ));
    }
    Some(AliasKind::Ignored("unknown HTS_* variable"))
}

/// A legacy variable found in the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deprecation {
    pub legacy: String,
    pub kind: AliasKind,
    /// Whether its value was adopted (the canonical name was unset).
    pub adopted: bool,
    /// Canonical name was also set to a different value; canonical won.
    pub conflict: bool,
}

impl Deprecation {
    pub fn message(&self) -> String {
        match self.kind {
            AliasKind::Renamed(c) | AliasKind::MergedInto(c) if self.conflict => format!(
                "{} is deprecated and differs from {c}; using {c}",
                self.legacy
            ),
            AliasKind::Renamed(c) | AliasKind::MergedInto(c) if self.adopted => {
                format!(
                    "{} is deprecated; using its value for {c} — rename it",
                    self.legacy
                )
            }
            AliasKind::Renamed(c) | AliasKind::MergedInto(c) => {
                format!("{} is deprecated and ignored ({c} is set)", self.legacy)
            }
            AliasKind::Ignored(why) => format!("{} is ignored: {why}", self.legacy),
        }
    }
}

/// A read-only view of the environment with legacy values folded in.
#[derive(Debug, Clone, Default)]
pub struct EnvView {
    values: BTreeMap<String, String>,
    pub deprecations: Vec<Deprecation>,
}

impl EnvView {
    /// Build from `(name, value)` pairs (`std::env::vars()` in production).
    pub fn from_vars<I, K, V>(vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let raw: BTreeMap<String, String> = vars
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        let mut values = raw.clone();
        let mut deprecations = Vec::new();
        for (name, value) in raw.iter().filter(|(k, _)| k.starts_with("HTS_")) {
            let Some(kind) = classify(name) else { continue };
            let (adopted, conflict) = match kind {
                AliasKind::Renamed(c) | AliasKind::MergedInto(c) => match raw.get(c) {
                    None => {
                        values.insert(c.to_string(), value.clone());
                        (true, false)
                    }
                    Some(existing) => (false, existing != value),
                },
                AliasKind::Ignored(_) => (false, false),
            };
            deprecations.push(Deprecation {
                legacy: name.clone(),
                kind,
                adopted,
                conflict,
            });
        }
        Self {
            values,
            deprecations,
        }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .get(name)
            .map(String::as_str)
            .filter(|v| !v.trim().is_empty())
    }

    fn parse<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>, ConfigError> {
        match self.get(name) {
            None => Ok(None),
            Some(v) => v
                .trim()
                .parse()
                .map(Some)
                .map_err(|_| ConfigError::Invalid {
                    name: name.to_string(),
                    value: v.to_string(),
                }),
        }
    }

    fn flag(&self, name: &str) -> Result<Option<bool>, ConfigError> {
        match self.get(name).map(|v| v.trim().to_ascii_lowercase()) {
            None => Ok(None),
            Some(v) if matches!(v.as_str(), "1" | "true" | "yes" | "on") => Ok(Some(true)),
            Some(v) if matches!(v.as_str(), "0" | "false" | "no" | "off") => Ok(Some(false)),
            Some(v) => Err(ConfigError::Invalid {
                name: name.to_string(),
                value: v,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("{name}={value:?} is not valid")]
    Invalid { name: String, value: String },
    #[error("HFS_TERMINOLOGY=remote needs HFS_TERMINOLOGY_SERVER")]
    RemoteWithoutServer,
    #[error("HFS_TERMINOLOGY=embedded but this build has no embedded terminology")]
    EmbeddedNotBuilt,
    #[error(transparent)]
    Schema(#[from] SchemaError),
}

/// Pools, runtime and limits of the embedded service (design §8).
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddedSettings {
    /// Connection string of the terminology store: derived from
    /// `HFS_DATABASE_URL` + schema unless `HFS_TERMINOLOGY_DATABASE_URL` is set
    /// (Stage 1 keeps pointing it at the old `hts` database).
    pub database_url: Option<String>,
    pub schema: String,
    pub pg_pool_size: usize,
    pub pg_pool_wait_timeout: Duration,
    pub pg_statement_timeout_ms: u64,
    /// Admin API + import jobs on this instance (`HFS_TERMINOLOGY_ADMIN`).
    pub admin: bool,
    pub data_dir: Option<String>,
    pub import_dir: Option<String>,
    pub bootstrap_dir: Option<String>,
    /// Worker threads of the dedicated import runtime (§8.2).
    pub import_threads: usize,
    /// Max blocking threads that runtime may use for parsing.
    pub import_blocking_threads: usize,
    /// Concurrent terminology operations admitted from FHIR request paths
    /// before callers queue (validation, `:in`).
    pub operation_permits: usize,
    pub max_body_size: Option<usize>,
    pub request_timeout: Option<Duration>,
    pub require_auth_for_writes: Option<bool>,
}

/// Resolved terminology configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminologySettings {
    pub mode: TerminologyMode,
    /// Remote base (`HFS_TERMINOLOGY_SERVER`), remote mode only.
    pub server_url: Option<String>,
    /// Public URL printed in CapabilityStatement / console links
    /// (`HFS_TERMINOLOGY_PUBLIC_URL`; embedded default `{origin}/tx`).
    pub public_url: Option<String>,
    pub timeout: Duration,
    pub embedded: EmbeddedSettings,
}

/// Defaults (design §8.2): leave ≥ 3/4 of the cores to FHIR traffic.
pub fn default_import_threads(cores: usize) -> usize {
    (cores / 4).clamp(1, 4)
}

impl TerminologySettings {
    /// Resolve from the environment view. `embedded_built` = the binary was
    /// compiled with the `terminology` feature; `cores` = available
    /// parallelism.
    pub fn resolve(env: &EnvView, embedded_built: bool, cores: usize) -> Result<Self, ConfigError> {
        let server_url = env.get("HFS_TERMINOLOGY_SERVER").map(str::to_string);
        let mode = match env.get("HFS_TERMINOLOGY") {
            Some(v) => TerminologyMode::parse(v).ok_or_else(|| ConfigError::Invalid {
                name: "HFS_TERMINOLOGY".into(),
                value: v.into(),
            })?,
            // Compatibility: a deployment that names a server keeps using it
            // until it opts in; a fresh deployment gets the embedded service.
            None if server_url.is_some() => TerminologyMode::Remote,
            None if embedded_built => TerminologyMode::Embedded,
            None => TerminologyMode::Off,
        };
        match mode {
            TerminologyMode::Remote if server_url.is_none() => {
                return Err(ConfigError::RemoteWithoutServer);
            }
            TerminologyMode::Embedded if !embedded_built => {
                return Err(ConfigError::EmbeddedNotBuilt);
            }
            _ => {}
        }
        let schema_name = env
            .get("HFS_TERMINOLOGY_SCHEMA")
            .unwrap_or(schema::TERMINOLOGY_SCHEMA)
            .to_string();
        schema::validate_schema_name(&schema_name)?;
        let database_url = match env.get("HFS_TERMINOLOGY_DATABASE_URL") {
            Some(url) => Some(url.to_string()),
            None => match env.get("HFS_DATABASE_URL") {
                Some(hfs) if schema::is_postgres_url(hfs) => {
                    Some(schema::with_search_path(hfs, &schema_name)?)
                }
                Some(hfs) => schema::sqlite_terminology_url(hfs),
                None => None,
            },
        };
        let embedded = EmbeddedSettings {
            database_url,
            schema: schema_name,
            pg_pool_size: env.parse("HFS_TERMINOLOGY_PG_POOL_SIZE")?.unwrap_or(16),
            pg_pool_wait_timeout: Duration::from_secs(
                env.parse("HFS_TERMINOLOGY_PG_POOL_WAIT_TIMEOUT_SECS")?
                    .unwrap_or(30),
            ),
            pg_statement_timeout_ms: env
                .parse("HFS_TERMINOLOGY_PG_STATEMENT_TIMEOUT_MS")?
                .unwrap_or(0),
            admin: env.flag("HFS_TERMINOLOGY_ADMIN")?.unwrap_or(false),
            data_dir: env.get("HFS_TERMINOLOGY_DATA_DIR").map(str::to_string),
            import_dir: env.get("HFS_TERMINOLOGY_IMPORT_DIR").map(str::to_string),
            bootstrap_dir: env.get("HFS_TERMINOLOGY_BOOTSTRAP_DIR").map(str::to_string),
            import_threads: env
                .parse("HFS_TERMINOLOGY_IMPORT_THREADS")?
                .unwrap_or_else(|| default_import_threads(cores)),
            import_blocking_threads: env
                .parse("HFS_TERMINOLOGY_IMPORT_BLOCKING_THREADS")?
                .unwrap_or(4),
            operation_permits: env
                .parse("HFS_TERMINOLOGY_OPERATION_PERMITS")?
                .unwrap_or(64),
            max_body_size: env.parse("HFS_TERMINOLOGY_MAX_BODY_SIZE")?,
            request_timeout: env
                .parse::<u64>("HFS_TERMINOLOGY_REQUEST_TIMEOUT")?
                .map(Duration::from_secs),
            require_auth_for_writes: env.flag("HFS_TERMINOLOGY_REQUIRE_AUTH_FOR_WRITES")?,
        };
        let public_url = env
            .get("HFS_TERMINOLOGY_PUBLIC_URL")
            .map(str::to_string)
            .or_else(|| match mode {
                TerminologyMode::Embedded => env.get("HFS_BASE_URL").and_then(tx_public_url),
                TerminologyMode::Remote => server_url.clone(),
                TerminologyMode::Off => None,
            });
        Ok(Self {
            mode,
            server_url: if mode == TerminologyMode::Remote {
                server_url
            } else {
                None
            },
            public_url,
            timeout: Duration::from_secs(env.parse("HFS_TERMINOLOGY_TIMEOUT_SECS")?.unwrap_or(30)),
            embedded,
        })
    }
}

/// `https://host/fhir` → `https://host/tx` (the public terminology base in
/// embedded mode: same origin, `/tx` path, as nginx serves it today).
pub fn tx_public_url(hfs_base: &str) -> Option<String> {
    let url = url::Url::parse(hfs_base).ok()?;
    let mut origin = url.origin().ascii_serialization();
    if origin == "null" {
        return None;
    }
    origin.push_str("/tx");
    Some(origin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(pairs: &[(&str, &str)]) -> EnvView {
        EnvView::from_vars(pairs.iter().map(|(k, v)| (*k, *v)))
    }

    #[test]
    fn every_hts_clap_variable_is_classified() {
        // The HtsConfig clap args (crates/hts/src/config.rs) at the time of
        // writing; package U2 turns this into a test against the real list.
        for name in [
            "HTS_SERVER_PORT",
            "HTS_SERVER_HOST",
            "HTS_LOG_LEVEL",
            "HTS_DATABASE_URL",
            "HTS_STORAGE_BACKEND",
            "HTS_ENABLE_CORS",
            "HTS_CORS_ORIGINS",
            "HTS_MAX_BODY_SIZE",
            "HTS_REQUEST_TIMEOUT",
            "HTS_MAX_EXPANSION_SIZE",
            "HTS_AUTO_SUPPLEMENTS",
            "HTS_BOOTSTRAP_DIR",
            "HTS_CACHE_SYNC_INTERVAL_MS",
            "HTS_BOOTSTRAP_BATCH_SIZE",
            "HTS_IMPORT_LANGUAGES",
            "HTS_UI_ENABLED",
            "HTS_ADMIN_UI",
            "HTS_ADMIN_DATA_DIR",
            "HTS_IMPORT_DIR",
            "HTS_PACKAGE_REGISTRY",
            "HTS_UPLOAD_CHUNK_SIZE",
            "HTS_MAX_UPLOAD_SIZE",
            "HTS_DISK_HEADROOM",
            "HTS_REQUIRE_AUTH_FOR_WRITES",
        ] {
            let kind = classify(name).unwrap();
            assert_ne!(
                kind,
                AliasKind::Ignored("unknown HTS_* variable"),
                "{name} unclassified"
            );
        }
        assert!(classify("HFS_LOG_LEVEL").is_none());
        assert!(matches!(
            classify("HTS_AUDIT_DATABASE_URL"),
            Some(AliasKind::Ignored(_))
        ));
    }

    #[test]
    fn legacy_values_adopted_unless_canonical_set() {
        let v = view(&[
            ("HTS_PG_POOL_SIZE", "24"),
            ("HTS_ADMIN_PASSWORD", "old"),
            ("HFS_ADMIN_PASSWORD", "new"),
            ("HTS_SERVER_PORT", "8090"),
        ]);
        assert_eq!(v.get("HFS_TERMINOLOGY_PG_POOL_SIZE"), Some("24"));
        assert_eq!(v.get("HFS_ADMIN_PASSWORD"), Some("new"));
        let pw = v
            .deprecations
            .iter()
            .find(|d| d.legacy == "HTS_ADMIN_PASSWORD")
            .unwrap();
        assert!(pw.conflict && !pw.adopted);
        assert!(pw.message().contains("using HFS_ADMIN_PASSWORD"));
        // The message never contains a value.
        assert!(v.deprecations.iter().all(|d| !d.message().contains("old")));
        assert_eq!(v.deprecations.len(), 3);
    }

    #[test]
    fn mode_defaults_preserve_existing_deployments() {
        // Production today: HFS_TERMINOLOGY_SERVER=http://hts:8090 → remote.
        let s = TerminologySettings::resolve(
            &view(&[("HFS_TERMINOLOGY_SERVER", "http://hts:8090")]),
            true,
            8,
        )
        .unwrap();
        assert_eq!(s.mode, TerminologyMode::Remote);
        assert_eq!(s.public_url.as_deref(), Some("http://hts:8090"));
        // Fresh deployment → embedded, store derived from HFS_DATABASE_URL.
        let s = TerminologySettings::resolve(
            &view(&[
                ("HFS_DATABASE_URL", "postgres://hfs:pw@hfs-pg:5432/hfs"),
                ("HFS_BASE_URL", "https://hfs.gugulu.tw/fhir"),
            ]),
            true,
            8,
        )
        .unwrap();
        assert_eq!(s.mode, TerminologyMode::Embedded);
        assert_eq!(s.public_url.as_deref(), Some("https://hfs.gugulu.tw/tx"));
        assert!(
            s.embedded
                .database_url
                .unwrap()
                .contains("search_path%3Dterminology")
        );
        assert_eq!(s.embedded.import_threads, 2);
        // Explicit opt-in wins over a configured server.
        let s = TerminologySettings::resolve(
            &view(&[
                ("HFS_TERMINOLOGY", "embedded"),
                ("HFS_TERMINOLOGY_SERVER", "http://hts:8090"),
            ]),
            true,
            8,
        )
        .unwrap();
        assert_eq!(s.mode, TerminologyMode::Embedded);
        assert!(s.server_url.is_none());
    }

    #[test]
    fn stage_one_keeps_the_old_database() {
        let s = TerminologySettings::resolve(
            &view(&[
                ("HFS_TERMINOLOGY", "embedded"),
                ("HFS_DATABASE_URL", "postgres://hfs:pw@hfs-pg:5432/hfs"),
                ("HTS_DATABASE_URL", "postgresql://hts:pw@hfs-pg:5432/hts"),
            ]),
            true,
            48,
        )
        .unwrap();
        assert_eq!(
            s.embedded.database_url.as_deref(),
            Some("postgresql://hts:pw@hfs-pg:5432/hts")
        );
        assert_eq!(s.embedded.import_threads, 4);
    }

    #[test]
    fn invalid_combinations_fail_fast() {
        assert_eq!(
            TerminologySettings::resolve(&view(&[("HFS_TERMINOLOGY", "remote")]), true, 4),
            Err(ConfigError::RemoteWithoutServer)
        );
        assert_eq!(
            TerminologySettings::resolve(&view(&[("HFS_TERMINOLOGY", "embedded")]), false, 4),
            Err(ConfigError::EmbeddedNotBuilt)
        );
        assert!(matches!(
            TerminologySettings::resolve(&view(&[("HFS_TERMINOLOGY", "maybe")]), true, 4),
            Err(ConfigError::Invalid { .. })
        ));
        assert!(matches!(
            TerminologySettings::resolve(
                &view(&[("HFS_TERMINOLOGY_SCHEMA", "public; drop")]),
                true,
                4
            ),
            Err(ConfigError::Schema(_))
        ));
        // Built without embedded and nothing configured → off.
        assert_eq!(
            TerminologySettings::resolve(&view(&[]), false, 4)
                .unwrap()
                .mode,
            TerminologyMode::Off
        );
    }
}
