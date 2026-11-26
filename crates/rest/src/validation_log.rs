//! Rate-limited logging of write-path validation issues.
//!
//! `HFS_VALIDATION_MODE=log` used to write one `WARN` line per issue. A bulk
//! load of generated data repeats the same handful of issues for every
//! resource: 527,113 Synthea resources produced 463,325 lines, 71% of them
//! six messages. Every line is formatted and written to a locked stdout on
//! the request's path.
//!
//! Issues are now grouped by a key — tenant, resource type, severity, issue
//! kind, the element path with array indices normalised, and the message
//! with numbers and UUIDs normalised:
//!
//! - **Within one request** (a resource, or a whole Bundle) identical keys
//!   collapse into one line carrying `occurrences`.
//! - **Per interval** (60 s) the first lines of a key (5) are written as they
//!   were, with the real path and message; the rest are only counted.
//! - **At the end of an interval** each key that had lines held back gets one
//!   summary line with its totals.
//!
//! At most [`ValidationLogLimits::max_keys`] keys are tracked per interval;
//! issues of further keys are counted together and reported in one line.
//! Nothing is lost from the numbers: every issue is counted in
//! `hfs_validation_log_issues_total{resource_type,severity}`, every line not
//! written in `hfs_validation_log_lines_suppressed_total`
//! ([`helios_observability::validation_metrics`]).
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `HFS_VALIDATION_LOG_LINES_PER_KEY` | 5 | Lines written per key and interval before counting only |
//! | `HFS_VALIDATION_LOG_INTERVAL_SECS` | 60 | Interval length; `0` turns the rate limit off (every line is written) |
//! | `HFS_VALIDATION_LOG_MAX_KEYS` | 2000 | Distinct keys tracked per interval |

use helios_fhir_validator::{Severity, ValidationError};
use helios_observability::validation_metrics;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tracing::warn;

/// Distinct `resource_type` label values passed to the metrics; further
/// types are counted as `other`.
const MAX_RESOURCE_TYPE_LABELS: usize = 256;

/// Limits of the validation issue log (`HFS_VALIDATION_LOG_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidationLogLimits {
    /// Lines written per key and interval before the rest are only counted.
    pub lines_per_key: u32,
    /// Interval length; zero writes every line (no rate limit).
    pub interval: Duration,
    /// Distinct keys tracked per interval.
    pub max_keys: usize,
}

impl Default for ValidationLogLimits {
    fn default() -> Self {
        Self {
            lines_per_key: 5,
            interval: Duration::from_secs(60),
            max_keys: 2000,
        }
    }
}

impl ValidationLogLimits {
    /// The limits from `HFS_VALIDATION_LOG_*`; an unset or unparsable
    /// variable keeps its default.
    pub fn from_env() -> Self {
        fn var<T: std::str::FromStr>(name: &str) -> Option<T> {
            std::env::var(name).ok()?.trim().parse().ok()
        }
        let d = Self::default();
        Self {
            lines_per_key: var("HFS_VALIDATION_LOG_LINES_PER_KEY").unwrap_or(d.lines_per_key),
            interval: var("HFS_VALIDATION_LOG_INTERVAL_SECS")
                .map(Duration::from_secs)
                .unwrap_or(d.interval),
            max_keys: var::<usize>("HFS_VALIDATION_LOG_MAX_KEYS")
                .unwrap_or(d.max_keys)
                .max(1),
        }
    }
}

/// Which write-path report an issue belongs to; the prefix of its line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IssueSource {
    /// `HFS_VALIDATION_MODE=log`: errors and warnings, write proceeds.
    LogMode,
    /// `enforce`: the warnings of a write that passed.
    EnforceWarning,
}

impl IssueSource {
    fn label(self) -> &'static str {
        match self {
            Self::LogMode => "validation (log mode)",
            Self::EnforceWarning => "validation warning on write",
        }
    }
}

/// What makes two issues "the same" for the log.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct IssueKey {
    source: IssueSource,
    tenant: String,
    resource_type: String,
    severity: &'static str,
    kind: String,
    /// The element path, array indices as `N`.
    path: String,
    /// The message, numbers as `N` and UUIDs as `{uuid}`.
    template: String,
}

/// `text` with every number that is not part of a word replaced by `N` and
/// every UUID by `{uuid}`: `Claim.item.3.net` → `Claim.item.N.net`,
/// `…/currencies|4.0.1` → `…/currencies|N.N.N`, while `hl7` and `rfc3986`
/// keep their digits.
pub(crate) fn normalise(text: &str) -> String {
    fn is_uuid_at(b: &[u8], at: usize) -> bool {
        let Some(candidate) = b.get(at..at + 36) else {
            return false;
        };
        let shaped = candidate.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        });
        shaped
            && (at == 0 || !b[at - 1].is_ascii_alphanumeric())
            && b.get(at + 36).is_none_or(|c| !c.is_ascii_alphanumeric())
    }
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_hexdigit() && is_uuid_at(b, i) {
            out.push_str("{uuid}");
            i += 36;
            continue;
        }
        if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            let in_word = (start > 0 && b[start - 1].is_ascii_alphabetic())
                || b.get(i).is_some_and(u8::is_ascii_alphabetic);
            if in_word {
                out.push_str(&text[start..i]);
            } else {
                out.push('N');
            }
            continue;
        }
        // `i` is always on a char boundary: only whole ASCII runs are skipped.
        let ch = text[i..].chars().next().expect("in bounds");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// One key's issues within a request, with the first one as the sample.
#[derive(Debug)]
struct Group {
    key: IssueKey,
    occurrences: u64,
    path: String,
    message: String,
}

/// The loggable issues of one request (a resource, or every entry of a
/// Bundle), identical keys collapsed, in first-seen order.
#[derive(Debug, Default)]
pub struct IssueBatch {
    groups: Vec<Group>,
    index: HashMap<IssueKey, usize>,
}

impl IssueBatch {
    /// Nothing to log.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Number of distinct keys.
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    /// Add the non-information issues of one resource.
    pub fn add_issues(
        &mut self,
        source: IssueSource,
        tenant: &str,
        resource_type: &str,
        issues: &[ValidationError],
    ) {
        for issue in issues
            .iter()
            .filter(|i| i.severity != Severity::Information)
        {
            let key = IssueKey {
                source,
                tenant: tenant.to_string(),
                resource_type: resource_type.to_string(),
                severity: if issue.severity == Severity::Error {
                    "error"
                } else {
                    "warning"
                },
                kind: format!("{:?}", issue.kind),
                path: normalise(&issue.path),
                template: normalise(&issue.message),
            };
            self.push(Group {
                key,
                occurrences: 1,
                path: issue.path.clone(),
                message: issue.message.clone(),
            });
        }
    }

    /// Fold `other` in (the next entries of the same Bundle).
    pub fn merge(&mut self, other: IssueBatch) {
        for group in other.groups {
            self.push(group);
        }
    }

    fn push(&mut self, group: Group) {
        match self.index.get(&group.key) {
            Some(&at) => self.groups[at].occurrences += group.occurrences,
            None => {
                self.index.insert(group.key.clone(), self.groups.len());
                self.groups.push(group);
            }
        }
    }
}

/// One distinct issue of the current interval ([`ValidationLog::snapshot`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueCount {
    /// The tenant written to.
    pub tenant: String,
    /// The type of the written resource.
    pub resource_type: String,
    /// `error` or `warning`.
    pub severity: &'static str,
    /// The element path, array indices as `N`.
    pub path: String,
    /// The message, numbers as `N` and UUIDs as `{uuid}`.
    pub message: String,
    /// Issues counted, written or not.
    pub issues: u64,
    /// Lines written for it in this interval.
    pub lines_written: u32,
    /// Lines the rate limit held back.
    pub lines_suppressed: u64,
}

/// One key's counts in the current interval.
#[derive(Debug, Default)]
struct KeyState {
    /// Lines written.
    written: u32,
    /// Lines held back.
    suppressed: u64,
    /// Issues, written or not.
    issues: u64,
}

#[derive(Debug)]
struct Interval {
    started: Instant,
    keys: HashMap<IssueKey, KeyState>,
    /// Issues of keys past `max_keys`.
    untracked_issues: u64,
    /// `resource_type` label values handed to the metrics so far (kept
    /// across intervals).
    resource_types: HashSet<String>,
}

#[derive(Debug)]
struct Inner {
    limits: ValidationLogLimits,
    interval: Mutex<Interval>,
    ticker_started: AtomicBool,
}

/// What an interval leaves to report when it ends.
struct Summary {
    keys: Vec<(IssueKey, KeyState)>,
    untracked_issues: u64,
}

/// The rate limiter. One per [`ValidationService`](crate::validation::ValidationService).
#[derive(Debug, Clone)]
pub struct ValidationLog {
    inner: Arc<Inner>,
}

impl Default for ValidationLog {
    fn default() -> Self {
        Self::new(ValidationLogLimits::from_env())
    }
}

impl ValidationLog {
    /// A log with explicit limits.
    pub fn new(limits: ValidationLogLimits) -> Self {
        Self {
            inner: Arc::new(Inner {
                limits,
                interval: Mutex::new(Interval {
                    started: Instant::now(),
                    keys: HashMap::new(),
                    untracked_issues: 0,
                    resource_types: HashSet::new(),
                }),
                ticker_started: AtomicBool::new(false),
            }),
        }
    }

    /// The limits in force.
    pub fn limits(&self) -> ValidationLogLimits {
        self.inner.limits
    }

    /// Log one request's issues: count them, write the lines the rate limit
    /// allows. Returns `(lines written, lines held back)`.
    pub fn record(&self, batch: IssueBatch) -> (usize, usize) {
        if batch.is_empty() {
            return (0, 0);
        }
        self.record_at(batch, Instant::now())
    }

    fn record_at(&self, batch: IssueBatch, now: Instant) -> (usize, usize) {
        let limits = self.inner.limits;
        let unlimited = limits.interval.is_zero();
        if !unlimited {
            self.start_ticker();
        }
        let mut write = Vec::new();
        let mut held_back = 0usize;
        let mut metrics: Vec<(String, &'static str, u64)> = Vec::new();
        let summary;
        {
            let mut interval = self
                .inner
                .interval
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            summary = Inner::rotate_if_due(&limits, &mut interval, now);
            for group in batch.groups {
                let label = if interval.resource_types.contains(&group.key.resource_type) {
                    group.key.resource_type.clone()
                } else if interval.resource_types.len() < MAX_RESOURCE_TYPE_LABELS {
                    interval
                        .resource_types
                        .insert(group.key.resource_type.clone());
                    group.key.resource_type.clone()
                } else {
                    "other".to_string()
                };
                metrics.push((label, group.key.severity, group.occurrences));
                if unlimited {
                    write.push(group);
                    continue;
                }
                let tracked = interval.keys.len();
                let state = match interval.keys.get_mut(&group.key) {
                    Some(state) => state,
                    None if tracked < limits.max_keys => {
                        interval.keys.entry(group.key.clone()).or_default()
                    }
                    None => {
                        interval.untracked_issues += group.occurrences;
                        held_back += 1;
                        continue;
                    }
                };
                state.issues += group.occurrences;
                if state.written < limits.lines_per_key {
                    state.written += 1;
                    write.push(group);
                } else {
                    state.suppressed += 1;
                    held_back += 1;
                }
            }
        }
        // Everything below runs without the lock: stdout may block.
        for (resource_type, severity, issues) in metrics {
            validation_metrics::record_log_issues(&resource_type, severity, issues);
        }
        validation_metrics::record_suppressed_lines(held_back as u64);
        if let Some(summary) = summary {
            Inner::write_summary(&limits, summary);
        }
        let written = write.len();
        for group in write {
            write_line(&group);
        }
        (written, held_back)
    }

    /// What the current interval has counted so far, most frequent first.
    pub fn snapshot(&self) -> Vec<IssueCount> {
        let interval = self
            .inner
            .interval
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut counts: Vec<IssueCount> = interval
            .keys
            .iter()
            .map(|(key, state)| IssueCount {
                tenant: key.tenant.clone(),
                resource_type: key.resource_type.clone(),
                severity: key.severity,
                path: key.path.clone(),
                message: key.template.clone(),
                issues: state.issues,
                lines_written: state.written,
                lines_suppressed: state.suppressed,
            })
            .collect();
        counts.sort_by(|a, b| b.issues.cmp(&a.issues));
        counts
    }

    /// End the current interval now and write its summary lines (shutdown,
    /// tests). Returns the number of summary lines.
    pub fn flush(&self) -> usize {
        let summary = {
            let mut interval = self
                .inner
                .interval
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            Inner::take_summary(&mut interval, Instant::now())
        };
        Inner::write_summary(&self.inner.limits, summary)
    }

    /// The interval's summary must not wait for the next issue: a ticker on
    /// the runtime ends intervals on time. Started with the first issue, when
    /// there is a runtime; without one the next `record` ends the interval.
    fn start_ticker(&self) {
        if self.inner.ticker_started.load(Ordering::Relaxed) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self.inner.ticker_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let inner: Weak<Inner> = Arc::downgrade(&self.inner);
        let period = self.inner.limits.interval;
        handle.spawn(async move {
            loop {
                tokio::time::sleep(period).await;
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                let summary = {
                    let mut interval = inner.interval.lock().unwrap_or_else(|e| e.into_inner());
                    Inner::rotate_if_due(&inner.limits, &mut interval, Instant::now())
                };
                if let Some(summary) = summary {
                    Inner::write_summary(&inner.limits, summary);
                }
            }
        });
    }
}

impl Inner {
    fn rotate_if_due(
        limits: &ValidationLogLimits,
        interval: &mut Interval,
        now: Instant,
    ) -> Option<Summary> {
        if limits.interval.is_zero()
            || now.saturating_duration_since(interval.started) < limits.interval
        {
            return None;
        }
        Some(Self::take_summary(interval, now))
    }

    fn take_summary(interval: &mut Interval, now: Instant) -> Summary {
        interval.started = now;
        Summary {
            keys: std::mem::take(&mut interval.keys).into_iter().collect(),
            untracked_issues: std::mem::take(&mut interval.untracked_issues),
        }
    }

    /// One line per key that had lines held back, plus one for the issues
    /// past the key cap. Returns the number of lines.
    fn write_summary(limits: &ValidationLogLimits, summary: Summary) -> usize {
        let mut held: Vec<(IssueKey, KeyState)> = summary
            .keys
            .into_iter()
            .filter(|(_, state)| state.suppressed > 0)
            .collect();
        held.sort_by(|a, b| b.1.issues.cmp(&a.1.issues));
        let mut lines = held.len();
        for (key, state) in held {
            warn!(
                tenant = %key.tenant,
                resource_type = %key.resource_type,
                path = %key.path,
                kind = %key.kind,
                severity = key.severity,
                occurrences = state.issues,
                lines_written = state.written,
                lines_suppressed = state.suppressed,
                interval_secs = limits.interval.as_secs(),
                "{} summary: {}",
                key.source.label(),
                key.template
            );
        }
        if summary.untracked_issues > 0 {
            lines += 1;
            warn!(
                occurrences = summary.untracked_issues,
                max_keys = limits.max_keys,
                interval_secs = limits.interval.as_secs(),
                "validation issue summary: issues of further distinct kinds were counted but \
                 not itemised (HFS_VALIDATION_LOG_MAX_KEYS)"
            );
        }
        lines
    }
}

/// One issue line: the first occurrence as it was reported.
fn write_line(group: &Group) {
    let key = &group.key;
    match (key.source, group.occurrences) {
        (IssueSource::LogMode, 1) => warn!(
            tenant = %key.tenant,
            resource_type = %key.resource_type,
            path = %group.path,
            kind = %key.kind,
            severity = key.severity,
            "validation (log mode): {}",
            group.message
        ),
        (IssueSource::LogMode, occurrences) => warn!(
            tenant = %key.tenant,
            resource_type = %key.resource_type,
            path = %group.path,
            kind = %key.kind,
            severity = key.severity,
            occurrences,
            "validation (log mode): {}",
            group.message
        ),
        (IssueSource::EnforceWarning, 1) => warn!(
            tenant = %key.tenant,
            resource_type = %key.resource_type,
            path = %group.path,
            "validation warning on write: {}",
            group.message
        ),
        (IssueSource::EnforceWarning, occurrences) => warn!(
            tenant = %key.tenant,
            resource_type = %key.resource_type,
            path = %group.path,
            occurrences,
            "validation warning on write: {}",
            group.message
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_fhir_validator::ErrorKind;

    fn issue(path: &str, message: &str, severity: Severity) -> ValidationError {
        ValidationError::new(
            ErrorKind::TerminologyBinding,
            path.to_string(),
            message.to_string(),
        )
        .with_severity(severity)
    }

    fn batch(tenant: &str, resource_type: &str, issues: &[ValidationError]) -> IssueBatch {
        let mut batch = IssueBatch::default();
        batch.add_issues(IssueSource::LogMode, tenant, resource_type, issues);
        batch
    }

    fn limits(lines_per_key: u32, max_keys: usize) -> ValidationLogLimits {
        ValidationLogLimits {
            lines_per_key,
            interval: Duration::from_secs(60),
            max_keys,
        }
    }

    #[test]
    fn numbers_and_uuids_are_normalised_but_words_keep_their_digits() {
        assert_eq!(
            normalise("ExplanationOfBenefit.item.12.adjudication.0.amount"),
            "ExplanationOfBenefit.item.N.adjudication.N.amount"
        );
        assert_eq!(
            normalise("in the ValueSet 'http://hl7.org/fhir/ValueSet/currencies|4.0.1'"),
            "in the ValueSet 'http://hl7.org/fhir/ValueSet/currencies|N.N.N'"
        );
        assert_eq!(
            normalise("Reference urn:uuid:0f3a9c1e-5b7d-4e2a-9c61-2f8d3b7a1e55 (entry 7) rfc3986"),
            "Reference urn:uuid:{uuid} (entry N) rfc3986"
        );
        assert_eq!(normalise("值 42 無效"), "值 N 無效");
        assert_eq!(normalise(""), "");
    }

    #[test]
    fn identical_issues_of_one_request_collapse_into_one_group() {
        let mut all = IssueBatch::default();
        for entry in 0..40 {
            let issues = [
                issue(
                    &format!("Claim.item.{entry}.net.currency"),
                    "code 'USD' cannot be checked",
                    Severity::Warning,
                ),
                issue("Claim", "informational", Severity::Information),
            ];
            all.merge(batch("t1", "Claim", &issues));
        }
        all.merge(batch(
            "t1",
            "Claim",
            &[issue(
                "Claim.total.currency",
                "code 'USD' cannot be checked",
                Severity::Warning,
            )],
        ));
        // Same message for another tenant, and as an error: different keys.
        all.merge(batch(
            "t2",
            "Claim",
            &[issue(
                "Claim.item.0.net.currency",
                "code 'USD' cannot be checked",
                Severity::Warning,
            )],
        ));
        all.merge(batch(
            "t1",
            "Claim",
            &[issue(
                "Claim.item.0.net.currency",
                "code 'USD' cannot be checked",
                Severity::Error,
            )],
        ));
        assert_eq!(all.len(), 4);
        assert_eq!(all.groups[0].occurrences, 40);
        assert_eq!(
            all.groups[0].path, "Claim.item.0.net.currency",
            "first one is the sample"
        );

        let log = ValidationLog::new(limits(5, 100));
        assert_eq!(log.record(all), (4, 0), "one line per key");
    }

    #[test]
    fn a_key_is_written_a_few_times_per_interval_then_counted() {
        let log = ValidationLog::new(limits(5, 100));
        let start = Instant::now();
        let one = || {
            batch(
                "t1",
                "Observation",
                &[issue(
                    "Observation.code.coding.0.system",
                    "unknown system",
                    Severity::Warning,
                )],
            )
        };
        let mut written = 0;
        let mut held = 0;
        for _ in 0..1000 {
            let (w, h) = log.record_at(one(), start);
            written += w;
            held += h;
        }
        assert_eq!((written, held), (5, 995));
        // Another key is not affected by the first one's quota.
        let other = batch(
            "t1",
            "Patient",
            &[issue("Patient.gender", "unknown system", Severity::Warning)],
        );
        assert_eq!(log.record_at(other, start), (1, 0));

        // The next interval: the summary is due, and the key writes again.
        let later = start + Duration::from_secs(61);
        {
            let interval = log.inner.interval.lock().unwrap();
            let state = interval.keys.values().find(|s| s.issues == 1000).unwrap();
            assert_eq!((state.written, state.suppressed), (5, 995));
        }
        assert_eq!(log.record_at(one(), later), (1, 0));
        let interval = log.inner.interval.lock().unwrap();
        assert_eq!(interval.keys.len(), 1, "the old interval's keys are gone");
    }

    #[test]
    fn the_summary_reports_only_keys_with_lines_held_back() {
        let log = ValidationLog::new(limits(2, 100));
        for _ in 0..10 {
            log.record(batch(
                "t1",
                "Claim",
                &[issue("Claim", "noisy", Severity::Error)],
            ));
        }
        log.record(batch(
            "t1",
            "Claim",
            &[issue("Claim", "quiet", Severity::Error)],
        ));
        assert_eq!(log.flush(), 1, "one summary line, for the noisy key");
        assert_eq!(log.flush(), 0, "nothing left");
    }

    #[test]
    fn the_number_of_keys_is_bounded() {
        let log = ValidationLog::new(limits(5, 3));
        let mut written = 0;
        let mut held = 0;
        for n in 0..50 {
            // Distinct words: distinct templates, so distinct keys.
            let message = format!("message-{}", "x".repeat(n + 1));
            let (w, h) = log.record(batch(
                "t1",
                "Claim",
                &[issue("Claim", &message, Severity::Error)],
            ));
            written += w;
            held += h;
        }
        assert_eq!((written, held), (3, 47));
        {
            let interval = log.inner.interval.lock().unwrap();
            assert_eq!(interval.keys.len(), 3);
            assert_eq!(interval.untracked_issues, 47);
        }
        assert_eq!(log.flush(), 1, "one line for everything past the cap");
    }

    #[test]
    fn a_zero_interval_writes_every_line() {
        let log = ValidationLog::new(ValidationLogLimits {
            interval: Duration::ZERO,
            ..limits(1, 1)
        });
        let mut written = 0;
        for n in 0..20 {
            let message = format!("message-{}", "x".repeat(n % 4 + 1));
            written += log
                .record(batch(
                    "t1",
                    "Claim",
                    &[issue("Claim", &message, Severity::Error)],
                ))
                .0;
        }
        assert_eq!(written, 20);
        assert!(log.inner.interval.lock().unwrap().keys.is_empty());
    }

    #[test]
    fn issues_and_suppressed_lines_are_counted_in_the_metrics() {
        let recorder = helios_observability::metrics::builder("test").build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let log = ValidationLog::new(limits(1, 100));
            for _ in 0..4 {
                let mut b = batch(
                    "tenant-a",
                    "Claim",
                    &[
                        issue("Claim.item.0.net", "bad", Severity::Error),
                        issue("Claim.item.1.net", "bad", Severity::Error),
                        issue("Claim", "hm", Severity::Warning),
                    ],
                );
                b.merge(batch(
                    "tenant-b",
                    "Patient",
                    &[issue("Patient", "hm", Severity::Warning)],
                ));
                log.record(b);
            }
        });
        let text = handle.render();
        let line = |needle: &[&str]| {
            text.lines()
                .find(|l| !l.starts_with('#') && needle.iter().all(|n| l.contains(n)))
                .and_then(|l| l.rsplit_once(' '))
                .map(|(_, v)| v.to_string())
        };
        let issues = validation_metrics::LOG_ISSUES;
        assert_eq!(
            line(&[issues, "resource_type=\"Claim\"", "severity=\"error\""]).as_deref(),
            Some("8"),
            "{text}"
        );
        assert_eq!(
            line(&[issues, "resource_type=\"Claim\"", "severity=\"warning\""]).as_deref(),
            Some("4"),
            "{text}"
        );
        assert_eq!(
            line(&[issues, "resource_type=\"Patient\""]).as_deref(),
            Some("4"),
            "{text}"
        );
        // 3 keys, 4 rounds, 1 line each per interval: 9 lines held back.
        assert_eq!(
            line(&[validation_metrics::LOG_LINES_SUPPRESSED]).as_deref(),
            Some("9"),
            "{text}"
        );
        assert!(!text.contains("tenant-a"), "no tenant label:\n{text}");
    }

    #[tokio::test(start_paused = true)]
    async fn the_ticker_ends_an_interval_without_further_issues() {
        let log = ValidationLog::new(limits(1, 100));
        for _ in 0..3 {
            log.record(batch(
                "t1",
                "Claim",
                &[issue("Claim", "noisy", Severity::Error)],
            ));
        }
        assert_eq!(log.inner.interval.lock().unwrap().keys.len(), 1);
        // Paused time advances both tokio's clock and `Instant::now()` only
        // for tokio; the interval's own clock is std's, so give it a zero
        // start to make it due.
        log.inner.interval.lock().unwrap().started = Instant::now() - Duration::from_secs(120);
        tokio::time::sleep(Duration::from_secs(61)).await;
        tokio::task::yield_now().await;
        assert!(
            log.inner.interval.lock().unwrap().keys.is_empty(),
            "the ticker ended the interval"
        );
    }
}
