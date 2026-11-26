//! Process-level Prometheus metrics for write-path validation issues that
//! are logged rather than rejected (`HFS_VALIDATION_MODE=log`, and the
//! warnings of a write `enforce` let through).
//!
//! The server rate-limits those log lines (`HFS_VALIDATION_LOG_*`): a bulk
//! load of generated data repeats the same few issues hundreds of thousands
//! of times. The counters keep the full numbers:
//!
//! | Series | Type | Meaning |
//! |--------|------|---------|
//! | `hfs_validation_log_issues_total{resource_type,severity}` | counter | issues found (`severity`: `error` or `warning`), written to the log or not |
//! | `hfs_validation_log_lines_suppressed_total` | counter | log lines the rate limit did not write |
//!
//! ## Tenant privacy and cardinality
//!
//! `/metrics` is public (see [`crate::metrics`]). There is no tenant label.
//! `resource_type` is the type of the written resource; the caller bounds
//! the number of distinct values it passes (anything past the bound is
//! `other`). Which tenant and which element is in the log lines.
//!
//! Every function is safe to call without an installed recorder: the
//! [`metrics`] facade then records into its no-op recorder.

/// Counter: validation issues found on writes that were not rejected for
/// them, by `resource_type` and `severity`.
pub const LOG_ISSUES: &str = "hfs_validation_log_issues_total";
/// Counter: log lines about such issues the rate limit did not write.
pub const LOG_LINES_SUPPRESSED: &str = "hfs_validation_log_lines_suppressed_total";

/// Count `issues` issues of one resource type and severity (`error` or
/// `warning`).
pub fn record_log_issues(resource_type: &str, severity: &'static str, issues: u64) {
    metrics::counter!(
        LOG_ISSUES,
        "resource_type" => resource_type.to_string(),
        "severity" => severity
    )
    .increment(issues);
}

/// Count log lines the rate limit did not write. Called with `0` too, so the
/// series exists from the first issue and `increase()` sees the first
/// suppressed line.
pub fn record_suppressed_lines(lines: u64) {
    metrics::counter!(LOG_LINES_SUPPRESSED).increment(lines);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_everything() {
        record_log_issues("Observation", "warning", 3);
        record_log_issues("Observation", "warning", 2);
        record_log_issues("Observation", "error", 1);
        record_log_issues("Claim", "error", 7);
        record_suppressed_lines(0);
        record_suppressed_lines(4);
    }

    #[test]
    fn recording_without_a_recorder_does_not_panic() {
        record_everything();
    }

    fn value_of(text: &str, name: &str, labels: &[(&str, &str)]) -> Option<String> {
        text.lines()
            .filter(|line| !line.starts_with('#'))
            .find(|line| {
                let Some((series, _)) = line.rsplit_once(' ') else {
                    return false;
                };
                let (series_name, series_labels) = match series.split_once('{') {
                    Some((n, l)) => (n, l.trim_end_matches('}')),
                    None => (series, ""),
                };
                series_name == name
                    && labels
                        .iter()
                        .all(|(k, v)| series_labels.contains(&format!("{k}=\"{v}\"")))
            })
            .and_then(|line| line.rsplit_once(' ').map(|(_, v)| v.to_string()))
    }

    /// Renders through a recorder built exactly as [`crate::metrics::init`]
    /// builds the global one, installed only for this thread.
    #[test]
    fn metrics_render_by_resource_type_and_severity_without_a_tenant() {
        let recorder = crate::metrics::builder("test").build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, record_everything);
        let text = handle.render();

        let issues = |resource_type: &str, severity: &str| {
            value_of(
                &text,
                LOG_ISSUES,
                &[("resource_type", resource_type), ("severity", severity)],
            )
        };
        assert_eq!(
            issues("Observation", "warning").as_deref(),
            Some("5"),
            "{text}"
        );
        assert_eq!(
            issues("Observation", "error").as_deref(),
            Some("1"),
            "{text}"
        );
        assert_eq!(issues("Claim", "error").as_deref(), Some("7"), "{text}");
        assert_eq!(issues("Claim", "warning"), None, "{text}");
        assert_eq!(
            value_of(&text, LOG_LINES_SUPPRESSED, &[]).as_deref(),
            Some("4"),
            "{text}"
        );
        assert!(!text.contains("tenant"), "no tenant label:\n{text}");
    }
}
