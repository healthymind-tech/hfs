//! URI parameter SQL handler.

use crate::types::{SearchModifier, SearchValue};

use super::super::query_builder::{SqlFragment, SqlParam};

/// Handles URI parameter SQL generation.
pub struct UriHandler;

impl UriHandler {
    /// Builds SQL for a URI parameter value.
    ///
    /// Default behavior is exact match.
    ///
    /// Modifiers:
    /// - `:below` - matches URIs that start with the given value
    /// - `:above` - matches URIs that the given value starts with
    pub fn build_sql(
        value: &SearchValue,
        modifier: Option<&SearchModifier>,
        param_offset: usize,
    ) -> SqlFragment {
        let param_num = param_offset + 1;
        let uri_value = &value.value;

        match modifier {
            // The `IS NOT NULL` leads are for the planner: SQLite cannot infer
            // non-null from `LIKE`, and they are what let the partial
            // `idx_search_uri` serve these instead of a type-wide scan.
            Some(SearchModifier::Contains) => {
                // :contains - case-insensitive substring match on the URI.
                SqlFragment::with_params(
                    format!(
                        "value_uri IS NOT NULL AND value_uri LIKE '%' || ?{} || '%'",
                        param_num
                    ),
                    vec![SqlParam::string(uri_value)],
                )
            }
            Some(SearchModifier::Below) => {
                // Below: match the URI or any URI that starts with it, on a
                // `/` boundary. For "http://example.org", matches
                // "http://example.org" and "http://example.org/foo". A value
                // written with a trailing slash — `url:below=http://acme.org/fhir/`,
                // the spec's own example — is trimmed first, or the pattern
                // would be `http://acme.org/fhir//%` and match nothing.
                let base = trim_trailing_slashes(uri_value);
                SqlFragment::with_params(
                    format!(
                        "value_uri IS NOT NULL AND (value_uri = ?{} OR value_uri LIKE ?{} || '/%')",
                        param_num,
                        param_num + 1
                    ),
                    vec![SqlParam::string(base), SqlParam::string(base)],
                )
            }
            Some(SearchModifier::Above) => {
                // Above: match URIs that the given value starts with
                // For "http://example.org/foo/bar", matches "http://example.org", "http://example.org/foo", etc.
                let base = trim_trailing_slashes(uri_value);
                SqlFragment::with_params(
                    format!(
                        "value_uri IS NOT NULL AND (?{} = value_uri OR ?{} LIKE value_uri || '/%')",
                        param_num,
                        param_num + 1
                    ),
                    vec![SqlParam::string(base), SqlParam::string(base)],
                )
            }
            _ => {
                // Default: exact match
                SqlFragment::with_params(
                    format!("value_uri = ?{}", param_num),
                    vec![SqlParam::string(uri_value)],
                )
            }
        }
    }
}

/// A uri less its trailing slashes, for the `:below` / `:above` prefix
/// patterns; a uri that is nothing but slashes is left alone.
fn trim_trailing_slashes(uri: &str) -> &str {
    let trimmed = uri.trim_end_matches('/');
    if trimmed.is_empty() { uri } else { trimmed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SearchPrefix;

    /// A trailing slash on the value (`url:below=http://acme.org/fhir/`) must
    /// not become a `//%` pattern that matches nothing.
    #[test]
    fn test_uri_below_trims_trailing_slash() {
        let value = SearchValue::new(SearchPrefix::Eq, "http://example.org/fhir/");
        let frag = UriHandler::build_sql(&value, Some(&SearchModifier::Below), 0);
        assert_eq!(
            frag.params,
            vec![
                SqlParam::string("http://example.org/fhir"),
                SqlParam::string("http://example.org/fhir"),
            ]
        );
        let frag = UriHandler::build_sql(&value, Some(&SearchModifier::Above), 0);
        assert_eq!(frag.params[0], SqlParam::string("http://example.org/fhir"));
        // Exact match keeps the value as written.
        let frag = UriHandler::build_sql(&value, None, 0);
        assert_eq!(frag.params[0], SqlParam::string("http://example.org/fhir/"));
    }

    #[test]
    fn test_uri_exact() {
        let value = SearchValue::new(SearchPrefix::Eq, "http://example.org/fhir/ValueSet/123");
        let frag = UriHandler::build_sql(&value, None, 0);

        assert!(frag.sql.contains("value_uri = ?1"));
        assert_eq!(frag.params.len(), 1);
    }

    #[test]
    fn test_uri_contains() {
        let value = SearchValue::new(SearchPrefix::Eq, "example.org");
        let frag = UriHandler::build_sql(&value, Some(&SearchModifier::Contains), 0);

        assert!(frag.sql.contains("LIKE '%' ||"));
        assert!(frag.sql.contains("|| '%'"));
        assert_eq!(frag.params.len(), 1);
    }

    #[test]
    fn test_uri_below() {
        let value = SearchValue::new(SearchPrefix::Eq, "http://example.org");
        let frag = UriHandler::build_sql(&value, Some(&SearchModifier::Below), 0);

        assert!(frag.sql.contains("OR"));
        assert!(frag.sql.contains("LIKE"));
        assert!(frag.sql.contains("|| '/%'"));
    }

    #[test]
    fn test_uri_above() {
        let value = SearchValue::new(SearchPrefix::Eq, "http://example.org/fhir/ValueSet/123");
        let frag = UriHandler::build_sql(&value, Some(&SearchModifier::Above), 0);

        assert!(frag.sql.contains("OR"));
        assert!(frag.sql.contains("LIKE"));
    }
}
