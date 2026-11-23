//! View models of the manager pages: plain data, no markup, no Fluent
//! lookups (templates resolve the `*_key` fields), so they are unit-testable.

use chrono::{DateTime, Utc};
use helios_endpoints::model::TerminologyValidation;
use helios_endpoints::sync::{SyncPeer, SyncRunState, SyncTrigger};
use helios_endpoints::{
    AuthMode, Endpoint, EndpointAuth, EndpointStatus, ManagedBy, ValidationMode, ValidationSettings,
};

use super::backend::{EndpointStats, SyncJobView};
use super::forms::release_label;

/// The server's environment defaults a section inherits when `null`
/// (`HFS_AUTH_*`, `HFS_VALIDATION_*`, …), shown read-only next to the
/// "Inherit from server" toggle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvDefaults {
    /// `HFS_AUTH_MODE`.
    pub auth_mode: AuthMode,
    /// `HFS_AUTH_ISSUER`.
    pub auth_issuer: Option<String>,
    /// `HFS_AUTH_AUDIENCE`.
    pub auth_audience: Option<String>,
    /// `HFS_AUTH_JWKS_URL`.
    pub auth_jwks_url: Option<String>,
    /// `HFS_VALIDATION_MODE`.
    pub validation_mode: ValidationMode,
    /// `HFS_VALIDATION_TERMINOLOGY`.
    pub validation_terminology: TerminologyValidation,
    /// `HFS_TERMINOLOGY_SERVER`.
    pub terminology_server: Option<String>,
    /// `HFS_CORS_ORIGINS`.
    pub cors_origins: Option<String>,
    /// `HFS_RATE_LIMIT_ANONYMOUS` / `_AUTHENTICATED`.
    pub rate_limit: (Option<String>, Option<String>),
}

impl EnvDefaults {
    /// Reads the process environment.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Reads through `get` (tests).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let get = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        Self {
            auth_mode: match get("HFS_AUTH_MODE").as_deref() {
                Some("write-protect") | Some("write_protect") => AuthMode::WriteProtect,
                Some("full") | Some("on") | Some("required") => AuthMode::Full,
                _ => AuthMode::Off,
            },
            auth_issuer: get("HFS_AUTH_ISSUER"),
            auth_audience: get("HFS_AUTH_AUDIENCE"),
            auth_jwks_url: get("HFS_AUTH_JWKS_URL"),
            validation_mode: match get("HFS_VALIDATION_MODE").as_deref() {
                Some("log") => ValidationMode::Log,
                Some("enforce") => ValidationMode::Enforce,
                _ => ValidationMode::Off,
            },
            validation_terminology: match get("HFS_VALIDATION_TERMINOLOGY").as_deref() {
                Some("off") => TerminologyValidation::Off,
                Some("remote") => TerminologyValidation::Remote,
                Some("remote-only") => TerminologyValidation::RemoteOnly,
                _ => TerminologyValidation::Embedded,
            },
            terminology_server: get("HFS_TERMINOLOGY_SERVER"),
            cors_origins: get("HFS_CORS_ORIGINS"),
            rate_limit: (
                get("HFS_RATE_LIMIT_ANONYMOUS"),
                get("HFS_RATE_LIMIT_AUTHENTICATED"),
            ),
        }
    }
}

/// A status / mode badge: icon + text, never colour alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Badge {
    /// Tone modifier: `ok`, `info`, `warn`, `danger`, `muted`, `accent`.
    pub tone: &'static str,
    /// Icon name.
    pub icon: &'static str,
    /// Fluent key of the text.
    pub key: &'static str,
    /// The value comes from the server environment (section inherited).
    pub inherited: bool,
}

/// Lifecycle badge.
pub fn status_badge(status: EndpointStatus) -> Badge {
    let (tone, icon, key) = match status {
        EndpointStatus::Enabled => ("ok", "check-circle", "mgr-status-enabled"),
        EndpointStatus::Disabled => ("muted", "stop-circle", "mgr-status-disabled"),
        EndpointStatus::Provisioning => ("info", "sync", "mgr-status-provisioning"),
        EndpointStatus::Deleted => ("danger", "trash", "mgr-status-deleted"),
    };
    Badge {
        tone,
        icon,
        key,
        inherited: false,
    }
}

/// Effective FHIR auth mode badge.
pub fn auth_badge(auth: Option<&EndpointAuth>, env: &EnvDefaults) -> Badge {
    let mode = auth.map(|a| a.mode).unwrap_or(env.auth_mode);
    let (tone, icon, key) = match mode {
        AuthMode::Off => ("warn", "unlock", "mgr-auth-off"),
        AuthMode::WriteProtect => ("info", "lock", "mgr-auth-write-protect"),
        AuthMode::Full => ("ok", "shield", "mgr-auth-full"),
    };
    Badge {
        tone,
        icon,
        key,
        inherited: auth.is_none(),
    }
}

/// Effective validation mode badge.
pub fn validation_badge(v: Option<&ValidationSettings>, env: &EnvDefaults) -> Badge {
    let mode = v.map(|v| v.mode).unwrap_or(env.validation_mode);
    let (tone, icon, key) = match mode {
        ValidationMode::Off => ("muted", "x", "mgr-validation-off"),
        ValidationMode::Log => ("info", "eye", "mgr-validation-log"),
        ValidationMode::Enforce => ("accent", "check-circle", "mgr-validation-enforce"),
    };
    Badge {
        tone,
        icon,
        key,
        inherited: v.is_none(),
    }
}

/// Fluent key of an auth mode (form options, summaries).
pub fn auth_mode_key(mode: AuthMode) -> &'static str {
    match mode {
        AuthMode::Off => "mgr-auth-off",
        AuthMode::WriteProtect => "mgr-auth-write-protect",
        AuthMode::Full => "mgr-auth-full",
    }
}

/// Form value of an auth mode.
pub fn auth_mode_value(mode: AuthMode) -> &'static str {
    match mode {
        AuthMode::Off => "off",
        AuthMode::WriteProtect => "write-protect",
        AuthMode::Full => "full",
    }
}

/// Form value of a validation mode.
pub fn validation_mode_value(mode: ValidationMode) -> &'static str {
    match mode {
        ValidationMode::Off => "off",
        ValidationMode::Log => "log",
        ValidationMode::Enforce => "enforce",
    }
}

/// Form value of a terminology validation mode.
pub fn terminology_validation_value(t: TerminologyValidation) -> &'static str {
    match t {
        TerminologyValidation::Off => "off",
        TerminologyValidation::Embedded => "embedded",
        TerminologyValidation::Remote => "remote",
        TerminologyValidation::RemoteOnly => "remote-only",
    }
}

/// Short relative time ("3 min", "2 h", "5 d"), the overview's last-write
/// column; the full timestamp goes in a `title`.
pub fn ago(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - at).num_seconds().max(0);
    match secs {
        0..=59 => format!("{secs} s"),
        60..=3599 => format!("{} min", secs / 60),
        3600..=86_399 => format!("{} h", secs / 3600),
        _ => format!("{} d", secs / 86_400),
    }
}

/// SVG polyline points for a sparkline of `values` in a `w`×`h` box.
pub fn sparkline_points(values: &[u64], w: f64, h: f64) -> String {
    if values.len() < 2 {
        return String::new();
    }
    let max = values.iter().copied().max().unwrap_or(0).max(1) as f64;
    let step = w / (values.len() - 1) as f64;
    values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = i as f64 * step;
            let y = h - (*v as f64 / max) * (h - 2.0) - 1.0;
            format!("{x:.1},{y:.1}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// One endpoint card of the overview.
#[derive(Debug, Clone, PartialEq)]
pub struct EndpointCard {
    /// Name (URL segment of the console).
    pub name: String,
    /// Display name.
    pub display_name: String,
    /// Description.
    pub description: String,
    /// Public FHIR base URL.
    pub base_url: String,
    /// `R4`…
    pub fhir_version: &'static str,
    /// Lifecycle.
    pub status: Badge,
    /// Raw status slug (filters).
    pub status_slug: &'static str,
    /// FHIR auth.
    pub auth: Badge,
    /// Validation.
    pub validation: Badge,
    /// Stored resources, when counted.
    pub resources: Option<u64>,
    /// Resource types in use, when counted.
    pub types: Option<usize>,
    /// Traffic.
    pub stats: Option<EndpointStats>,
    /// Sparkline points (empty = no series).
    pub sparkline: String,
    /// Last write, relative.
    pub last_write: Option<String>,
    /// Sync state across jobs touching this endpoint.
    pub sync: Option<Badge>,
    /// `key=value` labels.
    pub labels: Vec<String>,
    /// Declared in `HFS_ENDPOINTS_FILE`.
    pub env_managed: bool,
    /// The default endpoint.
    pub is_default: bool,
    /// Whether the card's Enable / Disable action applies.
    pub can_toggle: bool,
    /// Current revision (for the toggle form).
    pub revision: u64,
}

impl EndpointCard {
    /// Builds a card.
    pub fn new(
        endpoint: &Endpoint,
        server_base: &str,
        env: &EnvDefaults,
        counts: Option<(u64, usize)>,
        stats: Option<EndpointStats>,
        jobs: &[SyncJobView],
        now: DateTime<Utc>,
    ) -> Self {
        let spec = &endpoint.spec;
        let mut labels: Vec<String> = spec
            .labels
            .iter()
            .map(|(k, v)| {
                if v.is_empty() {
                    k.clone()
                } else {
                    format!("{k}={v}")
                }
            })
            .collect();
        labels.sort();
        let sparkline = stats
            .as_ref()
            .map(|s| sparkline_points(&s.sparkline, 120.0, 28.0))
            .unwrap_or_default();
        let last_write = stats
            .as_ref()
            .and_then(|s| s.last_write)
            .map(|t| ago(t, now));
        Self {
            name: endpoint.name().to_string(),
            display_name: spec.display_name.clone(),
            description: spec.description.clone().unwrap_or_default(),
            base_url: endpoint.public_base(server_base),
            fhir_version: release_label(spec.fhir_version),
            status: status_badge(spec.status),
            status_slug: spec.status.as_str(),
            auth: auth_badge(spec.auth.as_ref(), env),
            validation: validation_badge(spec.validation.as_ref(), env),
            resources: counts.map(|c| c.0),
            types: counts.map(|c| c.1),
            stats,
            sparkline,
            last_write,
            sync: sync_badge(endpoint.name(), jobs),
            labels,
            env_managed: spec.managed_by == ManagedBy::Env,
            is_default: spec.is_default(),
            can_toggle: !spec.is_default()
                && spec.managed_by == ManagedBy::Ui
                && matches!(
                    spec.status,
                    EndpointStatus::Enabled | EndpointStatus::Disabled
                ),
            revision: endpoint.revision,
        }
    }

    /// Whether the card matches a search term and status filter.
    pub fn matches(&self, q: &str, status: &str) -> bool {
        let q = q.trim().to_lowercase();
        let text_ok = q.is_empty()
            || self.name.to_lowercase().contains(&q)
            || self.display_name.to_lowercase().contains(&q)
            || self.description.to_lowercase().contains(&q)
            || self.labels.iter().any(|l| l.to_lowercase().contains(&q));
        let status_ok = status.is_empty() || status == "all" || status == self.status_slug;
        text_ok && status_ok
    }
}

/// Whether `peer` is the endpoint `name`.
pub fn peer_is(peer: &SyncPeer, name: &str) -> bool {
    matches!(peer, SyncPeer::Endpoint { name: n } if n == name)
}

/// Human label of a sync peer.
pub fn peer_label(peer: &SyncPeer) -> String {
    match peer {
        SyncPeer::Endpoint { name } => name.clone(),
        SyncPeer::External(s) => s.base_url.clone(),
    }
}

/// Badge of a run state.
pub fn run_badge(state: SyncRunState) -> Badge {
    let (tone, icon, key) = match state {
        SyncRunState::Queued => ("muted", "history", "mgr-run-queued"),
        SyncRunState::Running => ("info", "sync", "mgr-run-running"),
        SyncRunState::Succeeded => ("ok", "check-circle", "mgr-run-succeeded"),
        SyncRunState::Failed => ("danger", "alert", "mgr-run-failed"),
        SyncRunState::Cancelled => ("muted", "stop-circle", "mgr-run-cancelled"),
    };
    Badge {
        tone,
        icon,
        key,
        inherited: false,
    }
}

/// Fluent key of a trigger.
pub fn trigger_key(t: &SyncTrigger) -> &'static str {
    match t {
        SyncTrigger::Manual => "mgr-trigger-manual",
        SyncTrigger::Interval { .. } => "mgr-trigger-interval",
        SyncTrigger::Continuous { .. } => "mgr-trigger-continuous",
        SyncTrigger::Subscription { .. } => "mgr-trigger-subscription",
    }
}

/// Aggregated sync state of the jobs touching `name`: failed beats running
/// beats succeeded; `None` without jobs.
pub fn sync_badge(name: &str, jobs: &[SyncJobView]) -> Option<Badge> {
    let states: Vec<SyncRunState> = jobs
        .iter()
        .filter(|j| peer_is(&j.job.spec.source, name) || peer_is(&j.job.spec.target, name))
        .filter_map(|j| j.last_run.as_ref().map(|r| r.state))
        .collect();
    let touching = jobs
        .iter()
        .any(|j| peer_is(&j.job.spec.source, name) || peer_is(&j.job.spec.target, name));
    if !touching {
        return None;
    }
    let pick = if states.contains(&SyncRunState::Failed) {
        SyncRunState::Failed
    } else if states.contains(&SyncRunState::Running) {
        SyncRunState::Running
    } else if states.contains(&SyncRunState::Queued) {
        SyncRunState::Queued
    } else {
        SyncRunState::Succeeded
    };
    Some(run_badge(pick))
}

/// One row of an endpoint's sync page.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncRow {
    /// Job id.
    pub id: String,
    /// Job name.
    pub name: String,
    /// Source label.
    pub source: String,
    /// Target label.
    pub target: String,
    /// Whether this endpoint is the source.
    pub outgoing: bool,
    /// Trigger key.
    pub trigger_key: &'static str,
    /// Enabled.
    pub enabled: bool,
    /// Last run badge.
    pub last_run: Option<Badge>,
    /// Progress percent of a running run, when estimable.
    pub percent: Option<u8>,
    /// Items written so far by the last run.
    pub written: u64,
}

/// Rows for the jobs touching `name`.
pub fn sync_rows(name: &str, jobs: &[SyncJobView]) -> Vec<SyncRow> {
    jobs.iter()
        .filter(|j| peer_is(&j.job.spec.source, name) || peer_is(&j.job.spec.target, name))
        .map(|j| {
            let p = j.last_run.as_ref().map(|r| &r.progress);
            let written = p.map(|p| p.created + p.updated + p.deleted).unwrap_or(0);
            let percent = p.and_then(|p| {
                p.estimated_total
                    .filter(|t| *t > 0)
                    .map(|t| ((p.read.min(t) * 100) / t) as u8)
            });
            SyncRow {
                id: j.job.id.clone(),
                name: j.job.spec.name.clone(),
                source: peer_label(&j.job.spec.source),
                target: peer_label(&j.job.spec.target),
                outgoing: peer_is(&j.job.spec.source, name),
                trigger_key: trigger_key(&j.job.spec.trigger),
                enabled: j.job.spec.enabled,
                last_run: j.last_run.as_ref().map(|r| run_badge(r.state)),
                percent,
                written,
            }
        })
        .collect()
}

/// One changed field of a configuration diff preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRow {
    /// JSON path (`auth.mode`).
    pub path: String,
    /// Old value (JSON text, `—` when absent).
    pub before: String,
    /// New value.
    pub after: String,
}

/// Field-level differences between two redacted spec JSON documents.
pub fn diff_rows(before: &serde_json::Value, after: &serde_json::Value) -> Vec<DiffRow> {
    fn walk(
        path: &str,
        a: Option<&serde_json::Value>,
        b: Option<&serde_json::Value>,
        out: &mut Vec<DiffRow>,
    ) {
        use serde_json::Value;
        match (a, b) {
            (Some(Value::Object(x)), Some(Value::Object(y))) => {
                let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
                keys.sort();
                keys.dedup();
                for k in keys {
                    let p = if path.is_empty() {
                        k.clone()
                    } else {
                        format!("{path}.{k}")
                    };
                    walk(&p, x.get(k), y.get(k), out);
                }
            }
            (x, y) if x == y => {}
            (x, y) => {
                let show = |v: Option<&Value>| match v {
                    None | Some(Value::Null) => "—".to_string(),
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                };
                if show(x) != show(y) {
                    out.push(DiffRow {
                        path: path.to_string(),
                        before: show(x),
                        after: show(y),
                    });
                }
            }
        }
    }
    let mut out = Vec::new();
    walk("", Some(before), Some(after), &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_endpoints::{EndpointName, EndpointSpec, FhirRelease};

    fn endpoint(spec: EndpointSpec) -> Endpoint {
        Endpoint {
            id: "id".into(),
            revision: 3,
            spec,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            created_by: None,
            updated_by: None,
        }
    }

    #[test]
    fn env_defaults_parse() {
        let env = EnvDefaults::from_lookup(|k| match k {
            "HFS_AUTH_MODE" => Some("write-protect".into()),
            "HFS_VALIDATION_MODE" => Some("log".into()),
            "HFS_TERMINOLOGY_SERVER" => Some(" http://hts:8090 ".into()),
            _ => None,
        });
        assert_eq!(env.auth_mode, AuthMode::WriteProtect);
        assert_eq!(env.validation_mode, ValidationMode::Log);
        assert_eq!(env.terminology_server.as_deref(), Some("http://hts:8090"));
        assert_eq!(EnvDefaults::from_lookup(|_| None).auth_mode, AuthMode::Off);
    }

    #[test]
    fn badges_use_icon_and_text_and_mark_inheritance() {
        let env = EnvDefaults {
            auth_mode: AuthMode::WriteProtect,
            ..Default::default()
        };
        let inherited = auth_badge(None, &env);
        assert_eq!(inherited.key, "mgr-auth-write-protect");
        assert!(inherited.inherited);
        let own = auth_badge(
            Some(&EndpointAuth {
                mode: AuthMode::Full,
                ..Default::default()
            }),
            &env,
        );
        assert_eq!((own.key, own.inherited), ("mgr-auth-full", false));
        for s in [
            EndpointStatus::Enabled,
            EndpointStatus::Disabled,
            EndpointStatus::Provisioning,
            EndpointStatus::Deleted,
        ] {
            let b = status_badge(s);
            assert!(!b.icon.is_empty() && b.key.starts_with("mgr-status-"));
        }
        assert_eq!(validation_badge(None, &env).key, "mgr-validation-off");
    }

    #[test]
    fn cards_and_filters() {
        let mut spec = EndpointSpec::named(EndpointName::parse("twcore").unwrap(), "TW Core");
        spec.labels.insert("env".into(), "test".into());
        spec.fhir_version = FhirRelease::R4;
        let card = EndpointCard::new(
            &endpoint(spec),
            "https://h.example/fhir/",
            &EnvDefaults::default(),
            Some((10, 2)),
            Some(EndpointStats {
                sparkline: vec![1, 5, 3],
                last_write: Some(Utc::now() - chrono::Duration::minutes(3)),
                ..Default::default()
            }),
            &[],
            Utc::now(),
        );
        assert_eq!(card.base_url, "https://h.example/fhir/twcore");
        assert_eq!(card.labels, vec!["env=test"]);
        assert!(card.can_toggle && !card.is_default);
        assert_eq!(card.last_write.as_deref(), Some("3 min"));
        assert_eq!(card.sparkline.split(' ').count(), 3);
        assert!(card.matches("tw", ""));
        assert!(card.matches("env=test", "enabled"));
        assert!(!card.matches("", "disabled"));
        assert!(!card.matches("zzz", ""));
        let default = EndpointCard::new(
            &endpoint(EndpointSpec::default_endpoint("default", FhirRelease::R4)),
            "https://h.example/fhir",
            &EnvDefaults::default(),
            None,
            None,
            &[],
            Utc::now(),
        );
        assert_eq!(default.base_url, "https://h.example/fhir");
        assert!(!default.can_toggle && default.resources.is_none());
    }

    #[test]
    fn sparkline_and_ago() {
        assert_eq!(sparkline_points(&[1], 10.0, 10.0), "");
        assert_eq!(sparkline_points(&[0, 10], 10.0, 10.0), "0.0,9.0 10.0,1.0");
        let now = Utc::now();
        assert_eq!(ago(now - chrono::Duration::seconds(5), now), "5 s");
        assert_eq!(ago(now - chrono::Duration::hours(5), now), "5 h");
        assert_eq!(ago(now - chrono::Duration::days(2), now), "2 d");
    }

    #[test]
    fn diff_rows_report_changed_leaves() {
        let a = serde_json::json!({"auth": null, "displayName": "A", "labels": {"x": "1"}});
        let b =
            serde_json::json!({"auth": {"mode": "full"}, "displayName": "B", "labels": {"x": "1"}});
        let rows = diff_rows(&a, &b);
        let paths: Vec<_> = rows.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["auth", "displayName"]);
        assert_eq!(rows[1].before, "A");
        assert!(diff_rows(&a, &a).is_empty());
    }
}
