//! Terminology service client for HFS/HTS integration.
//!
//! This module provides an async HTTP client for communicating with the
//! Helios Terminology Server (HTS) or any compatible FHIR terminology server.
//! It is used internally by the FHIR search handler to resolve the `:in`,
//! `:not-in` and token `:above` / `:below` search modifiers via
//! `POST /ValueSet/$expand`.
//!
//! # Usage
//!
//! Configure the terminology server via the `HFS_TERMINOLOGY_SERVER` environment
//! variable (or `--terminology-server` CLI flag). When set, the search handler
//! expands the ValueSet (or code hierarchy) named by one of these modifiers
//! before querying the storage backend, and runs the search on the codes.
//!
//! # Architecture
//!
//! The client is intentionally lightweight — it only implements the subset of FHIR
//! terminology operations required by the REST layer:
//!
//! | Operation | Endpoint | Used by |
//! |-----------|----------|---------|
//! | ValueSet $expand | `POST /ValueSet/$expand` | `:in` / `:not-in` / token `:above` / `:below` search modifiers |
//!
//! Every expansion is requested **flat** (`excludeNested=true`) and **paged**
//! (`count` / `offset`), so a ValueSet defined by an `is-a` filter comes back
//! as the full list of descendants rather than a tree whose top level is the
//! root concept alone, and a large ValueSet is fetched in pages below the
//! terminology server's own expansion limit. A `url|version` canonical is
//! split into `url` + `valueSetVersion`. The number of codes an expansion may
//! yield is capped ([`ExpansionLimits::max_codes`]): past it the expansion
//! fails as too large rather than becoming a search filter with tens of
//! thousands of bound parameters.
//!
//! [`SearchTerminology`] wraps the client with a short-lived cache of
//! successful expansions, shared by every search the server runs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use helios_persistence::error::TerminologyFailureKind;
use reqwest::Client;
use serde_json::{Value, json};
use tracing::debug;

/// Errors that can occur when communicating with a terminology server.
#[derive(Debug, thiserror::Error)]
pub enum TerminologyError {
    /// The HTTP request itself failed (network issue, DNS, timeout).
    #[error("Terminology server request failed: {0}")]
    Network(String),

    /// The server responded with a non-2xx status code.
    #[error("Terminology server returned HTTP {status}: {body}")]
    ServerError {
        /// HTTP status code.
        status: u16,
        /// Response body (truncated at 512 bytes for safety).
        body: String,
    },

    /// The response body could not be parsed as expected.
    #[error("Failed to parse terminology server response: {0}")]
    Parse(String),

    /// The expansion has more codes than the configured cap allows a search
    /// filter to carry ([`ExpansionLimits::max_codes`]).
    #[error("ValueSet expansion has more than {max} codes (HFS_SEARCH_EXPANSION_MAX_CODES)")]
    TooLarge {
        /// The cap that was exceeded.
        max: usize,
    },
}

impl TerminologyError {
    /// Classifies the failure for the search layer's HTTP status.
    ///
    /// A `404` (unknown ValueSet, code system or code) or `400` (the server
    /// rejected the request — a malformed canonical, say) is the client's;
    /// a `422` is the server's `too-costly`, as is this client's own cap; a
    /// network failure, a `5xx` or an unparseable body is a transient outage.
    pub fn kind(&self) -> TerminologyFailureKind {
        match self {
            TerminologyError::ServerError { status, .. } if matches!(*status, 400 | 404) => {
                TerminologyFailureKind::NotFound
            }
            TerminologyError::ServerError { status: 422, .. }
            | TerminologyError::TooLarge { .. } => TerminologyFailureKind::TooCostly,
            TerminologyError::ServerError { .. }
            | TerminologyError::Network(_)
            | TerminologyError::Parse(_) => TerminologyFailureKind::Unavailable,
        }
    }

    /// The terminology server's own explanation of a [`ServerError`], when its
    /// body is an OperationOutcome: the first issue's `diagnostics` (or
    /// `details.text`).
    ///
    /// [`ServerError`]: TerminologyError::ServerError
    pub fn server_diagnostics(&self) -> Option<String> {
        let TerminologyError::ServerError { body, .. } = self else {
            return None;
        };
        let outcome: Value = serde_json::from_str(body).ok()?;
        let issue = outcome.get("issue")?.as_array()?.first()?;
        issue
            .get("diagnostics")
            .and_then(Value::as_str)
            .or_else(|| issue.pointer("/details/text").and_then(Value::as_str))
            .map(str::to_string)
    }

    /// What the client is told, for the expansion of `subject` (a ValueSet
    /// canonical or a `system|code`). Names the subject and the cause, never
    /// the terminology server's address or its raw error body.
    pub fn client_message(&self, subject: &str) -> String {
        match self {
            TerminologyError::ServerError { status: 404, .. } => {
                format!("'{subject}' is not known to the terminology server")
            }
            TerminologyError::ServerError {
                status: 400 | 422, ..
            } => match self.server_diagnostics() {
                Some(diagnostics) => {
                    format!("the terminology server could not expand '{subject}': {diagnostics}")
                }
                None => format!("the terminology server could not expand '{subject}'"),
            },
            TerminologyError::TooLarge { max } => format!(
                "the expansion of '{subject}' has more than {max} codes, the most a search \
                 filter may carry (HFS_SEARCH_EXPANSION_MAX_CODES)"
            ),
            TerminologyError::ServerError { .. }
            | TerminologyError::Network(_)
            | TerminologyError::Parse(_) => format!(
                "the terminology server could not be reached to expand '{subject}'; \
                 retry later"
            ),
        }
    }
}

/// A single expanded code from a FHIR ValueSet expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedCode {
    /// Code system URL (e.g. `http://snomed.info/sct`). May be empty.
    pub system: String,
    /// The code value (e.g. `73211009`).
    pub code: String,
    /// Optional human-readable display text.
    pub display: Option<String>,
}

impl ExpandedCode {
    /// Formats the code as `system|code` if a system is present, or just `code`.
    ///
    /// This is the format expected by FHIR token search parameters. A `,` or
    /// `\` inside the system or code is escaped as FHIR search values require,
    /// so the token survives being joined into a comma-separated OR list.
    pub fn as_token(&self) -> String {
        if self.system.is_empty() {
            escape_search_value(&self.code)
        } else {
            format!(
                "{}|{}",
                escape_search_value(&self.system),
                escape_search_value(&self.code)
            )
        }
    }
}

/// Escapes the two characters the OR-list splitter unescapes (`\`, `,`).
fn escape_search_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | ',') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Splits a canonical reference `url|version` into its URL and version.
///
/// `url` alone gives `(url, None)`; a trailing `|` gives `(url, None)` too.
pub fn split_canonical(canonical: &str) -> (&str, Option<&str>) {
    match canonical.split_once('|') {
        Some((url, version)) if !version.is_empty() => (url, Some(version)),
        Some((url, _)) => (url, None),
        None => (canonical, None),
    }
}

/// How much of an expansion the search layer will fetch and use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpansionLimits {
    /// `$expand` page size (`count`).
    pub page_size: u32,
    /// Largest expansion a search filter may be rewritten into.
    pub max_codes: usize,
}

impl Default for ExpansionLimits {
    fn default() -> Self {
        Self {
            page_size: 1000,
            max_codes: 10_000,
        }
    }
}

/// Async HTTP client for FHIR terminology server operations.
///
/// Used by the HFS REST layer to expand ValueSets when processing `:in`,
/// `:not-in` and token `:above` / `:below` search modifiers. Targets the
/// Helios Terminology Server (HTS) by default but is compatible with any FHIR
/// R4+ terminology server that honours `$expand`'s `count`, `offset`,
/// `excludeNested` and `valueSetVersion` parameters.
#[derive(Clone)]
pub struct TerminologyServiceClient {
    client: Client,
    /// Base URL of the terminology server (no trailing slash).
    base_url: String,
    limits: ExpansionLimits,
}

impl TerminologyServiceClient {
    /// Creates a new client targeting the given base URL, with the default
    /// [`ExpansionLimits`].
    ///
    /// Trailing slashes in `base_url` are trimmed automatically.
    /// A 10-second timeout is applied to all requests, with a short connect
    /// timeout so an unreachable server fails fast.
    ///
    /// The client bypasses any proxy environment: the terminology server is a
    /// configured internal endpoint, and routing it through an egress proxy
    /// turns "connection refused in milliseconds" into a multi-minute hang on
    /// runners whose `no_proxy` uses CIDR ranges reqwest cannot parse. The
    /// previous silent `unwrap_or_default()` fallback could also discard the
    /// timeouts entirely; a client build failure is a startup-visible
    /// misconfiguration, not something to paper over.
    pub fn new(base_url: String) -> Self {
        Self::with_limits(base_url, ExpansionLimits::default())
    }

    /// [`TerminologyServiceClient::new`] with explicit [`ExpansionLimits`].
    pub fn with_limits(base_url: String, limits: ExpansionLimits) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(2))
            .no_proxy()
            .build()
            .expect("terminology HTTP client");

        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            limits,
        }
    }

    /// Returns the configured base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Returns the expansion limits in force.
    pub fn limits(&self) -> ExpansionLimits {
        self.limits
    }

    /// Expands a ValueSet by canonical and returns the codes in its expansion.
    ///
    /// Sends `POST {base_url}/ValueSet/$expand` with a FHIR Parameters body
    /// naming the ValueSet (`url`, plus `valueSetVersion` when `canonical` is
    /// `url|version`), flat and paged — see the module docs. The pages'
    /// `expansion.contains` entries are returned as one `Vec<ExpandedCode>`.
    ///
    /// # Errors
    ///
    /// Returns a [`TerminologyError`] if:
    /// - The HTTP request fails (network error, timeout)
    /// - The server returns a non-2xx status (`404` for an unknown ValueSet)
    /// - The response cannot be parsed as a FHIR ValueSet with `expansion.contains`
    /// - The expansion exceeds [`ExpansionLimits::max_codes`]
    pub async fn expand_value_set(
        &self,
        canonical: &str,
    ) -> Result<Vec<ExpandedCode>, TerminologyError> {
        let (url, version) = split_canonical(canonical);
        let mut params = vec![json!({ "name": "url", "valueUri": url })];
        if let Some(version) = version {
            params.push(json!({ "name": "valueSetVersion", "valueString": version }));
        }
        self.expand_paged(params).await
    }

    /// Expands the concepts subsumed by (or subsuming) a code, via an inline
    /// ValueSet `$expand` with a filter.
    ///
    /// - `op = "is-a"` returns the code and all its descendants (for `:below`).
    /// - `op = "generalizes"` returns the code and all its ancestors (for `:above`).
    ///
    /// Both include the code itself, per FHIR subsumption-testing semantics.
    /// The expansion is flat and paged like [`expand_value_set`]'s.
    ///
    /// [`expand_value_set`]: TerminologyServiceClient::expand_value_set
    pub async fn expand_subsumption(
        &self,
        system: &str,
        code: &str,
        op: &str,
    ) -> Result<Vec<ExpandedCode>, TerminologyError> {
        let (system, system_version) = split_canonical(system);
        let mut include = json!({
            "system": system,
            "filter": [{ "property": "concept", "op": op, "value": code }]
        });
        if let Some(system_version) = system_version {
            include["version"] = json!(system_version);
        }
        let params = vec![json!({
            "name": "valueSet",
            "resource": {
                "resourceType": "ValueSet",
                "status": "active",
                "compose": { "include": [include] }
            }
        })];
        self.expand_paged(params).await
    }

    /// Runs one `$expand`, page by page, with `base_params` plus the paging
    /// and flattening parameters, until the expansion is complete or over the
    /// code cap.
    async fn expand_paged(
        &self,
        base_params: Vec<Value>,
    ) -> Result<Vec<ExpandedCode>, TerminologyError> {
        let page_size = self.limits.page_size.max(1);
        let mut codes: Vec<ExpandedCode> = Vec::new();
        let mut offset: usize = 0;
        loop {
            let mut params = base_params.clone();
            params.push(json!({ "name": "excludeNested", "valueBoolean": true }));
            params.push(json!({ "name": "count", "valueInteger": page_size }));
            params.push(json!({ "name": "offset", "valueInteger": offset }));
            let value = self.post_expand(params).await?;
            let page = extract_expansion_codes(&value)?;
            let total = expansion_total(&value);
            let page_len = page.len();
            codes.extend(page);
            if codes.len() > self.limits.max_codes {
                return Err(TerminologyError::TooLarge {
                    max: self.limits.max_codes,
                });
            }
            // Complete when the server says so (`total`), or when a page came
            // back short or empty. A page *longer* than asked is a server that
            // ignored `count` and returned everything: complete too, or the
            // next `offset` would fetch it all again.
            let complete = match total {
                Some(total) => codes.len() >= total,
                None => (page_len as u32) < page_size,
            };
            if complete || page_len == 0 || (page_len as u32) > page_size {
                break;
            }
            offset += page_len;
        }
        debug!(
            codes = codes.len(),
            pages = offset / usize::try_from(page_size).unwrap_or(usize::MAX) + 1,
            "Expanded ValueSet for search"
        );
        Ok(codes)
    }

    /// Sends one `POST /ValueSet/$expand` with the given parameters and
    /// returns the response body as JSON.
    async fn post_expand(&self, params: Vec<Value>) -> Result<Value, TerminologyError> {
        let endpoint = format!("{}/ValueSet/$expand", self.base_url);
        let body = json!({ "resourceType": "Parameters", "parameter": params });

        let response = self
            .client
            .post(&endpoint)
            .json(&body)
            .header("Content-Type", "application/fhir+json")
            .header("Accept", "application/fhir+json")
            .send()
            .await
            .map_err(|e| TerminologyError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body_text = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(512)
                .collect::<String>();
            return Err(TerminologyError::ServerError {
                status,
                body: body_text,
            });
        }

        response
            .json()
            .await
            .map_err(|e| TerminologyError::Parse(e.to_string()))
    }
}

/// Extracts `ExpandedCode` entries from a FHIR ValueSet resource with expansion.
///
/// Expects the standard FHIR structure:
/// ```json
/// { "expansion": { "contains": [{ "system": "...", "code": "...", "display": "..." }] } }
/// ```
///
/// Nested `contains` (a hierarchical expansion from a server that ignored
/// `excludeNested`) are flattened, depth first, so every code is seen.
fn extract_expansion_codes(value: &Value) -> Result<Vec<ExpandedCode>, TerminologyError> {
    let contains = value
        .pointer("/expansion/contains")
        .and_then(|c| c.as_array())
        .ok_or_else(|| {
            TerminologyError::Parse(
                "ValueSet expansion response is missing /expansion/contains".to_string(),
            )
        })?;

    let mut codes = Vec::with_capacity(contains.len());
    collect_contains(contains, &mut codes);
    Ok(codes)
}

fn collect_contains(contains: &[Value], codes: &mut Vec<ExpandedCode>) {
    for entry in contains {
        let code = entry
            .get("code")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if !code.is_empty() {
            let system = entry
                .get("system")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let display = entry
                .get("display")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            codes.push(ExpandedCode {
                system,
                code,
                display,
            });
        }

        if let Some(nested) = entry.get("contains").and_then(|c| c.as_array()) {
            collect_contains(nested, codes);
        }
    }
}

/// The expansion's `total`, when the server reports one.
fn expansion_total(value: &Value) -> Option<usize> {
    value
        .pointer("/expansion/total")
        .and_then(Value::as_u64)
        .and_then(|t| usize::try_from(t).ok())
}

/// What a search does when a terminology expansion behind one of its
/// modifiers fails (`HFS_SEARCH_TERMINOLOGY_FAIL`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExpansionFailurePolicy {
    /// The search fails with an OperationOutcome whose status reflects the
    /// cause (the default).
    #[default]
    Closed,
    /// The parameter matches nothing: the search returns an empty result.
    Empty,
    /// The parameter is dropped and the search runs without it, returning a
    /// superset of what was asked for (the pre-fork behaviour).
    Open,
}

impl ExpansionFailurePolicy {
    /// Parses the `HFS_SEARCH_TERMINOLOGY_FAIL` value; anything but `empty`
    /// or `open` is `Closed`.
    pub fn parse(value: &str) -> Self {
        match value {
            "empty" => Self::Empty,
            "open" => Self::Open,
            _ => Self::Closed,
        }
    }
}

/// The search layer's terminology service: a [`TerminologyServiceClient`]
/// plus a short-lived cache of successful expansions, keyed by what was
/// expanded, shared by every search the server runs.
///
/// Only successes are cached; a failure is retried on the next search that
/// needs the expansion. The cache holds at most [`CACHE_MAX_ENTRIES`] entries:
/// when full, expired entries are purged, then the oldest remaining one is
/// evicted.
pub struct SearchTerminology {
    client: TerminologyServiceClient,
    ttl: Duration,
    cache: Mutex<HashMap<String, CacheEntry>>,
}

struct CacheEntry {
    inserted: Instant,
    codes: Arc<Vec<ExpandedCode>>,
}

/// Upper bound on cached expansions.
pub const CACHE_MAX_ENTRIES: usize = 512;

impl SearchTerminology {
    /// Wraps `client`; `ttl` of zero disables the cache.
    pub fn new(client: TerminologyServiceClient, ttl: Duration) -> Self {
        Self {
            client,
            ttl,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Builds the service for the terminology server at `base_url` from the
    /// server's [`SearchTerminologyConfig`](crate::config::SearchTerminologyConfig).
    pub fn from_config(base_url: &str, config: &crate::config::SearchTerminologyConfig) -> Self {
        let limits = ExpansionLimits {
            page_size: config.expansion_page_size,
            max_codes: config.expansion_max_codes,
        };
        Self::new(
            TerminologyServiceClient::with_limits(base_url.to_string(), limits),
            Duration::from_secs(config.expansion_cache_ttl_secs),
        )
    }

    /// The underlying client.
    pub fn client(&self) -> &TerminologyServiceClient {
        &self.client
    }

    /// [`TerminologyServiceClient::expand_value_set`], cached.
    pub async fn expand_value_set(
        &self,
        canonical: &str,
    ) -> Result<Arc<Vec<ExpandedCode>>, TerminologyError> {
        let key = format!("in\u{0}{canonical}");
        if let Some(hit) = self.cached(&key) {
            return Ok(hit);
        }
        let codes = Arc::new(self.client.expand_value_set(canonical).await?);
        self.store(key, Arc::clone(&codes));
        Ok(codes)
    }

    /// [`TerminologyServiceClient::expand_subsumption`], cached.
    pub async fn expand_subsumption(
        &self,
        system: &str,
        code: &str,
        op: &str,
    ) -> Result<Arc<Vec<ExpandedCode>>, TerminologyError> {
        let key = format!("{op}\u{0}{system}|{code}");
        if let Some(hit) = self.cached(&key) {
            return Ok(hit);
        }
        let codes = Arc::new(self.client.expand_subsumption(system, code, op).await?);
        self.store(key, Arc::clone(&codes));
        Ok(codes)
    }

    fn cached(&self, key: &str) -> Option<Arc<Vec<ExpandedCode>>> {
        if self.ttl.is_zero() {
            return None;
        }
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .get(key)
            .filter(|entry| entry.inserted.elapsed() < self.ttl)
            .map(|entry| Arc::clone(&entry.codes))
    }

    fn store(&self, key: String, codes: Arc<Vec<ExpandedCode>>) {
        if self.ttl.is_zero() {
            return;
        }
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= CACHE_MAX_ENTRIES && !cache.contains_key(&key) {
            let ttl = self.ttl;
            cache.retain(|_, entry| entry.inserted.elapsed() < ttl);
            if cache.len() >= CACHE_MAX_ENTRIES
                && let Some(oldest) = cache
                    .iter()
                    .min_by_key(|(_, entry)| entry.inserted)
                    .map(|(k, _)| k.clone())
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(
            key,
            CacheEntry {
                inserted: Instant::now(),
                codes,
            },
        );
    }

    /// Number of cached expansions (expired ones included until evicted).
    pub fn cache_len(&self) -> usize {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ─── extract_expansion_codes ────────────────────────────────────────────

    #[test]
    fn test_extract_two_codes() {
        let response = json!({
            "resourceType": "ValueSet",
            "expansion": {
                "total": 2,
                "contains": [
                    {"system": "http://example.org/cs", "code": "A", "display": "Alpha"},
                    {"system": "http://example.org/cs", "code": "B", "display": "Beta"}
                ]
            }
        });

        let codes = extract_expansion_codes(&response).unwrap();
        assert_eq!(codes.len(), 2);
        assert_eq!(codes[0].code, "A");
        assert_eq!(codes[0].system, "http://example.org/cs");
        assert_eq!(codes[0].display, Some("Alpha".to_string()));
        assert_eq!(codes[1].code, "B");
        assert_eq!(codes[1].display, Some("Beta".to_string()));
        assert_eq!(expansion_total(&response), Some(2));
    }

    #[test]
    fn test_extract_empty_expansion() {
        let response = json!({
            "resourceType": "ValueSet",
            "expansion": {
                "total": 0,
                "contains": []
            }
        });

        let codes = extract_expansion_codes(&response).unwrap();
        assert!(codes.is_empty());
    }

    #[test]
    fn test_extract_missing_expansion() {
        let response = json!({"resourceType": "ValueSet"});
        let result = extract_expansion_codes(&response);
        assert!(matches!(result, Err(TerminologyError::Parse(_))));
        assert_eq!(expansion_total(&response), None);
    }

    #[test]
    fn test_extract_skips_empty_codes() {
        let response = json!({
            "expansion": {
                "contains": [
                    {"system": "http://example.org/cs", "code": ""},
                    {"system": "http://example.org/cs", "code": "X"}
                ]
            }
        });

        let codes = extract_expansion_codes(&response).unwrap();
        assert_eq!(codes.len(), 1);
        assert_eq!(codes[0].code, "X");
    }

    #[test]
    fn test_extract_no_system() {
        let response = json!({
            "expansion": {
                "contains": [{"code": "Z"}]
            }
        });

        let codes = extract_expansion_codes(&response).unwrap();
        assert_eq!(codes.len(), 1);
        assert_eq!(codes[0].code, "Z");
        assert_eq!(codes[0].system, "");
    }

    /// A hierarchical expansion (a server that ignored `excludeNested`) is
    /// flattened: an `is-a E11` tree must yield E11.9, not just E11.
    #[test]
    fn test_extract_flattens_nested_contains() {
        let response = json!({
            "expansion": {
                "contains": [{
                    "system": "http://hl7.org/fhir/sid/icd-10-cm", "code": "E11",
                    "contains": [
                        {"system": "http://hl7.org/fhir/sid/icd-10-cm", "code": "E11.9"},
                        {"system": "http://hl7.org/fhir/sid/icd-10-cm", "code": "E11.2",
                         "contains": [{"system": "http://hl7.org/fhir/sid/icd-10-cm", "code": "E11.21"}]}
                    ]
                }, {
                    // An abstract grouper with no code of its own.
                    "abstract": true,
                    "contains": [{"system": "http://hl7.org/fhir/sid/icd-10-cm", "code": "E10"}]
                }]
            }
        });

        let codes: Vec<String> = extract_expansion_codes(&response)
            .unwrap()
            .into_iter()
            .map(|c| c.code)
            .collect();
        assert_eq!(codes, ["E11", "E11.9", "E11.2", "E11.21", "E10"]);
    }

    // ─── ExpandedCode::as_token ─────────────────────────────────────────────

    #[test]
    fn test_as_token_with_system() {
        let c = ExpandedCode {
            system: "http://snomed.info/sct".to_string(),
            code: "73211009".to_string(),
            display: None,
        };
        assert_eq!(c.as_token(), "http://snomed.info/sct|73211009");
    }

    #[test]
    fn test_as_token_without_system() {
        let c = ExpandedCode {
            system: String::new(),
            code: "active".to_string(),
            display: None,
        };
        assert_eq!(c.as_token(), "active");
    }

    /// A code containing a search-reserved character must not split the OR
    /// list it is joined into.
    #[test]
    fn test_as_token_escapes_reserved_characters() {
        let c = ExpandedCode {
            system: "http://example.org/cs".to_string(),
            code: "a,b\\c".to_string(),
            display: None,
        };
        assert_eq!(c.as_token(), "http://example.org/cs|a\\,b\\\\c");
    }

    // ─── split_canonical ────────────────────────────────────────────────────

    #[test]
    fn test_split_canonical() {
        assert_eq!(
            split_canonical("http://example.org/vs|1.0.0"),
            ("http://example.org/vs", Some("1.0.0"))
        );
        assert_eq!(
            split_canonical("http://example.org/vs"),
            ("http://example.org/vs", None)
        );
        assert_eq!(
            split_canonical("http://example.org/vs|"),
            ("http://example.org/vs", None)
        );
    }

    // ─── TerminologyError ───────────────────────────────────────────────────

    #[test]
    fn test_error_kinds_and_messages() {
        let not_found = TerminologyError::ServerError {
            status: 404,
            body: json!({"resourceType": "OperationOutcome", "issue": [{"severity": "error",
                "code": "not-found", "diagnostics": "ValueSet not found"}]})
            .to_string(),
        };
        assert_eq!(not_found.kind(), TerminologyFailureKind::NotFound);
        assert_eq!(
            not_found.client_message("http://example.org/vs"),
            "'http://example.org/vs' is not known to the terminology server"
        );

        let too_costly = TerminologyError::ServerError {
            status: 422,
            body: json!({"resourceType": "OperationOutcome", "issue": [{"severity": "error",
                "code": "too-costly", "diagnostics": "47162 codes exceeds the server limit"}]})
            .to_string(),
        };
        assert_eq!(too_costly.kind(), TerminologyFailureKind::TooCostly);
        assert!(
            too_costly
                .client_message("http://example.org/vs")
                .contains("47162 codes exceeds the server limit")
        );

        let bad_request = TerminologyError::ServerError {
            status: 400,
            body: "not json".to_string(),
        };
        assert_eq!(bad_request.kind(), TerminologyFailureKind::NotFound);
        assert_eq!(
            bad_request.client_message("x"),
            "the terminology server could not expand 'x'"
        );

        let too_large = TerminologyError::TooLarge { max: 10 };
        assert_eq!(too_large.kind(), TerminologyFailureKind::TooCostly);
        assert!(too_large.client_message("x").contains("more than 10 codes"));

        for transient in [
            TerminologyError::Network("connection refused".into()),
            TerminologyError::ServerError {
                status: 500,
                body: String::new(),
            },
            TerminologyError::Parse("eof".into()),
        ] {
            assert_eq!(transient.kind(), TerminologyFailureKind::Unavailable);
            assert!(
                transient
                    .client_message("x")
                    .contains("could not be reached")
            );
            // Nothing about the server itself leaks to the client.
            assert!(!transient.client_message("x").contains("connection refused"));
        }
    }

    // ─── ExpansionFailurePolicy ─────────────────────────────────────────────

    #[test]
    fn test_failure_policy_parse() {
        assert_eq!(
            ExpansionFailurePolicy::parse("closed"),
            ExpansionFailurePolicy::Closed
        );
        assert_eq!(
            ExpansionFailurePolicy::parse("empty"),
            ExpansionFailurePolicy::Empty
        );
        assert_eq!(
            ExpansionFailurePolicy::parse("open"),
            ExpansionFailurePolicy::Open
        );
        assert_eq!(
            ExpansionFailurePolicy::parse("anything"),
            ExpansionFailurePolicy::Closed
        );
    }

    // ─── TerminologyServiceClient ────────────────────────────────────────────

    #[test]
    fn test_client_trims_trailing_slash() {
        let client = TerminologyServiceClient::new("http://localhost:8090/".to_string());
        assert_eq!(client.base_url(), "http://localhost:8090");
        assert_eq!(client.limits(), ExpansionLimits::default());
    }

    #[test]
    fn test_client_no_trailing_slash() {
        let client = TerminologyServiceClient::new("http://localhost:8090".to_string());
        assert_eq!(client.base_url(), "http://localhost:8090");
    }

    // ─── SearchTerminology cache ────────────────────────────────────────────

    #[test]
    fn test_cache_store_hit_and_ttl() {
        let client = TerminologyServiceClient::new("http://localhost:8090".to_string());
        let codes = Arc::new(vec![ExpandedCode {
            system: "s".into(),
            code: "c".into(),
            display: None,
        }]);

        let service = SearchTerminology::new(client.clone(), Duration::from_secs(60));
        service.store("k".into(), Arc::clone(&codes));
        assert_eq!(service.cached("k").as_deref(), Some(&*codes));
        assert_eq!(service.cached("other"), None);
        assert_eq!(service.cache_len(), 1);

        // Zero TTL disables the cache entirely.
        let disabled = SearchTerminology::new(client, Duration::ZERO);
        disabled.store("k".into(), Arc::clone(&codes));
        assert_eq!(disabled.cached("k"), None);
        assert_eq!(disabled.cache_len(), 0);
    }

    #[test]
    fn test_cache_is_bounded() {
        let client = TerminologyServiceClient::new("http://localhost:8090".to_string());
        let service = SearchTerminology::new(client, Duration::from_secs(60));
        let codes = Arc::new(Vec::new());
        for i in 0..(CACHE_MAX_ENTRIES + 5) {
            service.store(format!("k{i}"), Arc::clone(&codes));
        }
        assert_eq!(service.cache_len(), CACHE_MAX_ENTRIES);
        // The newest entries survive; the oldest were evicted.
        assert!(
            service
                .cached(&format!("k{}", CACHE_MAX_ENTRIES + 4))
                .is_some()
        );
        assert!(service.cached("k0").is_none());
    }
}
