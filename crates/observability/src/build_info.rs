//! The `build_info` gauge: a constant `1` labelled with the running build's
//! `version` and `revision`, so a dashboard or an upgrade check can confirm
//! which build every instance runs (`count by (version, revision) (build_info)`).
//! Both labels are fixed at startup; `/metrics` stays free of tenant data.

/// Gauge name.
pub const BUILD_INFO: &str = "build_info";

/// Publish `build_info{version, revision} 1`. Call once at startup, after
/// [`crate::metrics::init`].
pub fn set(version: &'static str, revision: &'static str) {
    metrics::gauge!(BUILD_INFO, "version" => version, "revision" => revision).set(1.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_version_and_revision_labels() {
        let recorder = crate::metrics::builder("test").build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || set("0.2.4", "abc1234"));
        let text = handle.render();
        assert!(
            text.lines().any(|l| l.starts_with(BUILD_INFO)
                && l.contains("version=\"0.2.4\"")
                && l.contains("revision=\"abc1234\"")
                && l.contains("service=\"test\"")
                && l.ends_with(" 1")),
            "{text}"
        );
    }
}
