//! Path/method exclusion rules for skipping audit on certain requests.

/// A single exclusion rule: skip audit when the request matches.
#[derive(Debug, Clone)]
pub struct ExclusionRule {
    /// URL path to match (e.g. `"/health"`). Trailing slashes are ignored. A
    /// path ending in `/*` matches everything below it (`/console/*`).
    pub path: String,
    /// HTTP method to match (e.g. `"GET"`). `None` matches all methods.
    pub method: Option<String>,
}

impl ExclusionRule {
    fn any(path: &str) -> Self {
        Self {
            path: path.to_string(),
            method: None,
        }
    }

    /// Parses `"/path"` or `"METHOD /path"` (as in `HFS_AUDIT_EXCLUDE_PATHS`).
    pub fn parse(spec: &str) -> Self {
        match spec.split_once(char::is_whitespace) {
            Some((method, path)) if !method.starts_with('/') => Self {
                path: path.trim().to_string(),
                method: Some(method.to_ascii_uppercase()),
            },
            _ => Self::any(spec),
        }
    }

    fn matches(&self, path: &str, method: &str) -> bool {
        let normalized = path.trim_end_matches('/');
        let path_match = match self.path.strip_suffix("/*") {
            Some(prefix) => normalized == prefix || normalized.starts_with(&format!("{prefix}/")),
            None => {
                let rule = self.path.trim_end_matches('/');
                normalized == rule || normalized.starts_with(&format!("{rule}?"))
            }
        };
        path_match
            && self
                .method
                .as_ref()
                .is_none_or(|m| m.eq_ignore_ascii_case(method))
    }
}

/// Collection of exclusion rules applied before recording an audit event.
#[derive(Debug, Clone)]
pub struct ExclusionFilter {
    rules: Vec<ExclusionRule>,
}

impl ExclusionFilter {
    /// Create a filter from a list of rules.
    pub fn new(rules: Vec<ExclusionRule>) -> Self {
        Self { rules }
    }

    /// Default exclusions: operational endpoints that disclose no patient data
    /// and are polled often (health probes, capability and discovery documents,
    /// console metrics, admin page reads). Admin *writes* stay audited.
    pub fn default_exclusions() -> Self {
        Self {
            rules: vec![
                ExclusionRule::any("/health"),
                ExclusionRule::any("/_liveness"),
                ExclusionRule::any("/_readiness"),
                ExclusionRule::any("/metrics"),
                ExclusionRule::any("/metadata"),
                ExclusionRule::any("/.well-known/*"),
                ExclusionRule::any("/$versions"),
                ExclusionRule::any("/console/*"),
                ExclusionRule::parse("GET /admin/*"),
                ExclusionRule::parse("HEAD /admin/*"),
            ],
        }
    }

    /// The default exclusions plus those configured in `HFS_AUDIT_EXCLUDE_PATHS`.
    pub fn from_config(config: &crate::config::AuditConfig) -> Self {
        let mut filter = Self::default_exclusions();
        filter.rules.extend(config.exclusions.iter().cloned());
        filter
    }

    /// Returns `true` if the given path/method combination is excluded from audit.
    pub fn is_excluded(&self, path: &str, method: &str) -> bool {
        self.rules.iter().any(|rule| rule.matches(path, method))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_exclusions_health() {
        let filter = ExclusionFilter::default_exclusions();
        assert!(filter.is_excluded("/health", "GET"));
        assert!(filter.is_excluded("/health", "HEAD"));
    }

    #[test]
    fn test_default_exclusions_metadata() {
        let filter = ExclusionFilter::default_exclusions();
        assert!(filter.is_excluded("/metadata", "GET"));
    }

    #[test]
    fn test_default_exclusions_smart_config() {
        let filter = ExclusionFilter::default_exclusions();
        assert!(filter.is_excluded("/.well-known/smart-configuration", "GET"));
    }

    #[test]
    fn test_non_excluded_path() {
        let filter = ExclusionFilter::default_exclusions();
        assert!(!filter.is_excluded("/Patient", "GET"));
        assert!(!filter.is_excluded("/Patient/123", "PUT"));
    }

    #[test]
    fn test_trailing_slash_ignored() {
        let filter = ExclusionFilter::default_exclusions();
        assert!(filter.is_excluded("/health/", "GET"));
    }

    #[test]
    fn test_method_specific_exclusion() {
        let filter = ExclusionFilter::new(vec![ExclusionRule {
            path: "/some-path".to_string(),
            method: Some("GET".to_string()),
        }]);
        assert!(filter.is_excluded("/some-path", "GET"));
        assert!(!filter.is_excluded("/some-path", "POST"));
    }

    #[test]
    fn test_empty_filter_excludes_nothing() {
        let filter = ExclusionFilter::new(vec![]);
        assert!(!filter.is_excluded("/health", "GET"));
        assert!(!filter.is_excluded("/Patient", "POST"));
    }

    #[test]
    fn test_prefix_and_method_rules() {
        let filter = ExclusionFilter::default_exclusions();
        assert!(filter.is_excluded("/console/metrics/uptime", "GET"));
        assert!(filter.is_excluded("/.well-known/smart-configuration", "GET"));
        assert!(filter.is_excluded("/admin/packages", "GET"));
        assert!(!filter.is_excluded("/admin/packages", "POST"));
        assert!(!filter.is_excluded("/administrator", "GET"));
        assert!(!filter.is_excluded("/Patient", "GET"));
    }

    #[test]
    fn test_parse_rule() {
        let r = ExclusionRule::parse("get /Basic/*");
        assert_eq!(r.method.as_deref(), Some("GET"));
        assert!(r.matches("/Basic/1", "GET"));
        assert!(!r.matches("/Basic/1", "PUT"));
        let r = ExclusionRule::parse("/ping");
        assert!(r.method.is_none());
    }
}
