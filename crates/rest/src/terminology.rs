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
//! | ValueSet $expand | `POST /ValueSet/$expand` | `:in` / `:not-in` / token `:above` / `:below` search modifiers, SQL query ValueSet dependencies |
//! | ConceptMap search | `GET /ConceptMap?url=` | SQL query ConceptMap dependencies |
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

use helios_persistence::core::bulk_submit_input::redact_urls_in;
use helios_persistence::error::TerminologyFailureKind;
use helios_terminology::remote::{HttpTransport, OutboundRequest, RemoteTerminology};
use helios_terminology::{TerminologyService, TxError, TxRequest, TxResponse};
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

/// Per-request timeout of the search terminology client when
/// `HFS_TERMINOLOGY_TIMEOUT_SECS` is not set. It was a fixed 10 s, which the
/// first (cold) expansion of a 4,000–6,000-code eCQM ValueSet exceeded on a
/// loaded host, failing the search with 503 (line G, CQL against HFS).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Async HTTP client for FHIR terminology server operations.
///
/// Used by the HFS REST layer to expand ValueSets when processing `:in`,
/// `:not-in` and token `:above` / `:below` search modifiers. Targets the
/// Helios Terminology Server (HTS) by default but is compatible with any FHIR
/// R4+ terminology server that honours `$expand`'s `count`, `offset`,
/// `excludeNested` and `valueSetVersion` parameters.
#[derive(Clone)]
pub struct TerminologyServiceClient {
    /// The terminology service the requests go to: the server's embedded
    /// service, or a remote server over HTTP (design §4).
    service: Arc<dyn TerminologyService>,
    /// Base URL of the terminology server (no trailing slash), or a label
    /// for the embedded service.
    base_url: String,
    limits: ExpansionLimits,
    /// Per-request deadline.
    timeout: Duration,
}

impl TerminologyServiceClient {
    /// Creates a new client targeting the given base URL, with the default
    /// [`ExpansionLimits`].
    ///
    /// Trailing slashes in `base_url` are trimmed automatically.
    /// [`DEFAULT_REQUEST_TIMEOUT`] is applied to every request (configurable
    /// with `HFS_TERMINOLOGY_TIMEOUT_SECS` through
    /// [`SearchTerminology::from_config`]), with a short connect timeout so an
    /// unreachable server fails fast.
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
        Self::with_limits_and_timeout(base_url, limits, DEFAULT_REQUEST_TIMEOUT)
    }

    /// [`TerminologyServiceClient::with_limits`] with an explicit per-request
    /// timeout (a whole `$expand` page, connect to last byte).
    pub fn with_limits_and_timeout(
        base_url: String,
        limits: ExpansionLimits,
        timeout: Duration,
    ) -> Self {
        let base_url = base_url.trim_end_matches('/').to_string();
        Self {
            service: remote_service(&base_url, None),
            base_url,
            limits,
            timeout,
        }
    }

    /// A client over an existing [`TerminologyService`] (the server's
    /// embedded or remote service); `label` is what [`Self::base_url`] reports.
    pub fn from_service(
        label: impl Into<String>,
        service: Arc<dyn TerminologyService>,
        limits: ExpansionLimits,
        timeout: Duration,
    ) -> Self {
        Self {
            service,
            base_url: label.into(),
            limits,
            timeout,
        }
    }

    /// The service behind this client.
    pub fn service(&self) -> &Arc<dyn TerminologyService> {
        &self.service
    }

    /// Runs one request; a transport failure becomes [`TerminologyError::Network`].
    async fn send(&self, request: TxRequest) -> Result<TxResponse, TerminologyError> {
        self.service
            .call(request.with_timeout(self.timeout))
            .await
            .map_err(|e| TerminologyError::Network(redact_urls_in(&tx_error_text(&e))))
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
        let body = json!({ "resourceType": "Parameters", "parameter": params });
        let response = self
            .send(TxRequest::post_json("ValueSet/$expand", &body))
            .await?;
        if !response.is_success() {
            return Err(TerminologyError::ServerError {
                status: response.status,
                body: truncated_body(&response),
            });
        }
        serde_json::from_slice(&response.body)
            .map_err(|e| TerminologyError::Parse(redact_urls_in(&e.to_string())))
    }

    /// Fetches one page of a ValueSet expansion as the raw expanded ValueSet:
    /// `POST {base_url}/ValueSet/$expand` with `url`, `offset` and `count`.
    ///
    /// Returns `Ok(None)` when the server answers `404` (it knows no value set
    /// at `value_set_url`). Used by SQL-on-FHIR terminology relations (#1824),
    /// which need nesting, `abstract`, `version` and `inactive` intact.
    pub async fn expand_value_set_page(
        &self,
        value_set_url: &str,
        offset: usize,
        count: usize,
    ) -> Result<Option<Value>, TerminologyError> {
        let body = json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "url", "valueUri": value_set_url},
                {"name": "offset", "valueInteger": offset},
                {"name": "count", "valueInteger": count}
            ]
        });
        self.send_optional(TxRequest::post_json("ValueSet/$expand", &body))
            .await
    }

    /// Looks up a ConceptMap by canonical (`url`, or `url|version`) with
    /// `GET {base_url}/ConceptMap?url=…[&version=…]`.
    ///
    /// Returns `Ok(None)` when no map matches, and the first match otherwise
    /// (a server holding several versions of an unversioned canonical decides
    /// the order).
    pub async fn find_concept_map(
        &self,
        canonical: &str,
    ) -> Result<Option<Value>, TerminologyError> {
        let (url, version) = match canonical.split_once('|') {
            Some((url, version)) => (url, Some(version)),
            None => (canonical, None),
        };
        let mut request = TxRequest::get("ConceptMap").with_query_param("url", url);
        if let Some(version) = version {
            request = request.with_query_param("version", version);
        }
        let Some(bundle) = self.send_optional(request).await? else {
            return Ok(None);
        };
        Ok(bundle
            .get("entry")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.get("resource"))
            .find(|r| r.get("resourceType").and_then(Value::as_str) == Some("ConceptMap"))
            .cloned())
    }

    /// Sends `request` expecting a FHIR JSON body; `404` becomes `Ok(None)`.
    async fn send_optional(&self, request: TxRequest) -> Result<Option<Value>, TerminologyError> {
        let response = self.send(request).await?;
        if response.status == 404 {
            return Ok(None);
        }
        if !response.is_success() {
            return Err(TerminologyError::ServerError {
                status: response.status,
                body: truncated_body(&response),
            });
        }
        serde_json::from_slice(&response.body)
            .map(Some)
            .map_err(|e| TerminologyError::Parse(redact_urls_in(&e.to_string())))
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
    /// The service's change feed (embedded service): a terminology write
    /// here or on another instance empties the cache at once instead of
    /// after the TTL (design §4.5).
    changes: Option<Mutex<tokio::sync::watch::Receiver<u64>>>,
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
        let changes = client.service().changes().map(Mutex::new);
        Self {
            client,
            ttl,
            cache: Mutex::new(HashMap::new()),
            changes,
        }
    }

    /// The search service over the server's terminology `service`
    /// (embedded or remote), configured like [`Self::from_config`].
    pub fn from_service(
        label: &str,
        service: Arc<dyn TerminologyService>,
        config: &crate::config::SearchTerminologyConfig,
    ) -> Self {
        let limits = ExpansionLimits {
            page_size: config.expansion_page_size,
            max_codes: config.expansion_max_codes,
        };
        Self::new(
            TerminologyServiceClient::from_service(
                label,
                service,
                limits,
                Duration::from_secs(config.timeout_secs.max(1)),
            ),
            Duration::from_secs(config.expansion_cache_ttl_secs),
        )
    }

    /// Empty the cache when the terminology changed since the last look.
    fn sync_changes(&self) {
        let Some(changes) = &self.changes else {
            return;
        };
        let mut rx = changes.lock().unwrap_or_else(|e| e.into_inner());
        if rx.has_changed().unwrap_or(false) {
            rx.borrow_and_update();
            self.cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
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
            TerminologyServiceClient::with_limits_and_timeout(
                base_url.to_string(),
                limits,
                Duration::from_secs(config.timeout_secs.max(1)),
            ),
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
        self.sync_changes();
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

// ─── TerminologyService wiring (design §4, package U2) ────────────────────────

/// Text of a [`TxError`] as the old reqwest clients reported it.
pub fn tx_error_text(error: &TxError) -> String {
    match error {
        TxError::Unavailable(m) => m.clone(),
        other => other.to_string(),
    }
}

fn truncated_body(response: &TxResponse) -> String {
    String::from_utf8_lossy(&response.body)
        .chars()
        .take(512)
        .collect()
}

/// [`HttpTransport`] over reqwest for [`RemoteTerminology`]: a 2 s connect
/// timeout, the request's own deadline, and no proxy (the terminology server
/// is a configured internal endpoint; see [`TerminologyServiceClient::new`]).
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// A transport with the 2 s connect timeout and no proxy.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .no_proxy()
                .build()
                .expect("terminology HTTP client"),
        }
    }

    /// The same transport under the outbound policy (SSRF guard): for the
    /// terminology server an endpoint binds in the console, checked after
    /// DNS resolution and on every redirect (the save-time check alone let a
    /// name that later resolves internally, or a redirect, through).
    pub fn guarded(policy: &helios_endpoints::OutboundPolicy) -> Self {
        Self {
            client: helios_endpoints::http_guard::guard(
                reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(2))
                    .no_proxy(),
                policy,
            )
            .build()
            .expect("terminology HTTP client"),
        }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

/// A reqwest error with its source chain (`error sending request` alone
/// does not say whether the connection was refused, reset or closed).
fn reqwest_error_chain(e: &reqwest::Error) -> String {
    let mut out = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

#[async_trait::async_trait]
impl HttpTransport for ReqwestTransport {
    async fn send(&self, request: OutboundRequest) -> Result<TxResponse, TxError> {
        let mut builder = self
            .client
            .request(request.method, &request.url)
            .timeout(request.timeout);
        for (name, value) in &request.headers {
            if name == "authorization" {
                let mut v = reqwest::header::HeaderValue::from_str(value)
                    .map_err(|e| TxError::Unavailable(e.to_string()))?;
                v.set_sensitive(true);
                builder = builder.header(reqwest::header::AUTHORIZATION, v);
            } else {
                builder = builder.header(name.as_str(), value.as_str());
            }
        }
        if let Some(body) = request.body {
            builder = builder.body(body);
        }
        let response = builder.send().await.map_err(|e| {
            if e.is_timeout() {
                TxError::Timeout(request.timeout)
            } else {
                TxError::Unavailable(reqwest_error_chain(&e))
            }
        })?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = response.bytes().await.map_err(|e| {
            if e.is_timeout() {
                TxError::Timeout(request.timeout)
            } else {
                TxError::Unavailable(reqwest_error_chain(&e))
            }
        })?;
        Ok(TxResponse {
            status,
            content_type,
            body,
        })
    }
}

fn shared_transport() -> Arc<dyn HttpTransport> {
    static TRANSPORT: std::sync::OnceLock<Arc<ReqwestTransport>> = std::sync::OnceLock::new();
    TRANSPORT
        .get_or_init(|| Arc::new(ReqwestTransport::new()))
        .clone()
}

fn guarded_transport() -> Arc<dyn HttpTransport> {
    static TRANSPORT: std::sync::OnceLock<Arc<ReqwestTransport>> = std::sync::OnceLock::new();
    TRANSPORT
        .get_or_init(|| {
            Arc::new(ReqwestTransport::guarded(
                &helios_endpoints::OutboundPolicy::from_env(),
            ))
        })
        .clone()
}

/// The terminology service of an endpoint's own binding (`url` is not the
/// server's installed service): over the guarded transport, and with the
/// endpoint's sealed `terminology.token` (see [`endpoint_terminology_token`])
/// sent as `Authorization: Bearer` on the server's own calls — validation,
/// search expansion, SQL-on-FHIR — which are made on the endpoint's behalf.
/// Without a token the calls stay anonymous, as before.
pub fn endpoint_service(
    url: &str,
    token: Option<String>,
    caller: TxCaller,
) -> (String, Arc<dyn TerminologyService>) {
    endpoint_service_over(url, token, caller, guarded_transport())
}

fn endpoint_service_over(
    url: &str,
    token: Option<String>,
    caller: TxCaller,
    transport: Arc<dyn HttpTransport>,
) -> (String, Arc<dyn TerminologyService>) {
    let base = url.trim_end_matches('/');
    let remote: Arc<dyn TerminologyService> =
        match RemoteTerminology::new(base, token.clone(), DEFAULT_REQUEST_TIMEOUT, transport) {
            Ok(remote) => Arc::new(remote),
            Err(e) => Arc::new(Unreachable(tx_error_text(&e))),
        };
    let service: Arc<dyn TerminologyService> = if token.is_some() {
        Arc::new(OnBehalfOfEndpoint(remote))
    } else {
        remote
    };
    (base.to_string(), instrumented(service, caller))
}

/// Server-originated calls (no caller identity) to an endpoint's terminology
/// server are made as the endpoint: the `System` principal, which the remote
/// client turns into the endpoint's bearer token. A console user's calls keep
/// their own principal (never the endpoint's credential).
struct OnBehalfOfEndpoint(Arc<dyn TerminologyService>);

#[async_trait::async_trait]
impl TerminologyService for OnBehalfOfEndpoint {
    fn kind(&self) -> helios_terminology::ServiceKind {
        self.0.kind()
    }
    async fn call(&self, mut request: TxRequest) -> Result<TxResponse, TxError> {
        if request.principal == helios_terminology::TxPrincipal::Anonymous {
            request.principal = helios_terminology::TxPrincipal::System;
        }
        self.0.call(request).await
    }
}

/// The plaintext of an endpoint's sealed `terminology.token` (AAD
/// `endpoint:{id}:terminology.token`), opened with `HFS_SECRET_KEYS`. `None`
/// without a token or a key ring, or when it cannot be opened (logged).
pub fn endpoint_terminology_token(endpoint: &helios_endpoints::Endpoint) -> Option<String> {
    endpoint.spec.terminology.as_ref()?.token.as_ref()?;
    static RING: std::sync::OnceLock<Option<helios_endpoints::secret::SecretKeyRing>> =
        std::sync::OnceLock::new();
    let ring = RING
        .get_or_init(|| {
            helios_endpoints::secret::SecretKeyRing::from_env()
                .ok()
                .flatten()
        })
        .as_ref()?;
    open_endpoint_token(endpoint, ring)
}

fn open_endpoint_token(
    endpoint: &helios_endpoints::Endpoint,
    ring: &helios_endpoints::secret::SecretKeyRing,
) -> Option<String> {
    let sealed = endpoint.spec.terminology.as_ref()?.token.as_ref()?;
    match ring.open(
        sealed,
        &format!("endpoint:{}:terminology.token", endpoint.id),
    ) {
        Ok(plain) => plain
            .expose_str()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty()),
        Err(e) => {
            tracing::warn!(endpoint = %endpoint.id, error = %e, "endpoint terminology token cannot be opened");
            None
        }
    }
}

/// A short, non-reversible fingerprint of a token for cache keys (a changed
/// token must build a new client; the key must not hold the secret).
pub fn token_fingerprint(token: Option<&str>) -> String {
    use std::hash::{Hash, Hasher};
    match token {
        None => String::new(),
        Some(t) => {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            t.hash(&mut h);
            format!("{:016x}", h.finish())
        }
    }
}

/// A remote terminology service for `base` (an endpoint's own server, or
/// the server's when nothing is installed). `token` is sent only for
/// [`helios_terminology::TxPrincipal::System`] requests.
pub fn remote_service(base: &str, token: Option<String>) -> Arc<dyn TerminologyService> {
    match RemoteTerminology::new(base, token, DEFAULT_REQUEST_TIMEOUT, shared_transport()) {
        Ok(remote) => Arc::new(remote),
        // Not http(s): every call fails the way an unreachable server does.
        Err(e) => Arc::new(Unreachable(tx_error_text(&e))),
    }
}

/// A service whose every call fails with `Unavailable` (invalid base URL).
struct Unreachable(String);

#[async_trait::async_trait]
impl TerminologyService for Unreachable {
    fn kind(&self) -> helios_terminology::ServiceKind {
        helios_terminology::ServiceKind::Remote
    }
    async fn call(&self, _request: TxRequest) -> Result<TxResponse, TxError> {
        Err(TxError::Unavailable(self.0.clone()))
    }
}

/// Who is calling terminology (`terminology_calls_total{caller}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxCaller {
    /// Resource validation (terminology bindings).
    Validator,
    /// `:in` / `:not-in` / `:above` / `:below` search expansion.
    Search,
    /// SQL-on-FHIR terminology dependencies.
    Sof,
    /// FHIRPath `memberOf()` / `%terminologies`.
    Fhirpath,
    /// IG package terminology installs.
    Packages,
    /// Console pages.
    Console,
}

impl TxCaller {
    /// The metrics label.
    pub fn as_str(self) -> &'static str {
        match self {
            TxCaller::Validator => "validator",
            TxCaller::Search => "search",
            TxCaller::Sof => "sof",
            TxCaller::Fhirpath => "fhirpath",
            TxCaller::Packages => "packages",
            TxCaller::Console => "console",
        }
    }

    /// FHIR request paths take operation permits (design §8.2); the package
    /// manager and the console do not.
    fn permitted(self) -> bool {
        matches!(
            self,
            TxCaller::Validator | TxCaller::Search | TxCaller::Sof | TxCaller::Fhirpath
        )
    }
}

/// The operation label of a request path (`$expand`, `batch`, `read`, ...).
fn op_label(request: &TxRequest) -> &'static str {
    let last = request.path.rsplit('/').next().unwrap_or("");
    match last {
        "$expand" => "expand",
        "$validate-code" => "validate-code",
        "$lookup" => "lookup",
        "$subsumes" => "subsumes",
        "$translate" => "translate",
        "$batch-validate-code" => "batch-validate-code",
        "$closure" => "closure",
        "" => "batch",
        "import" => "import",
        "health" => "health",
        _ if request.method == helios_terminology::service::TxMethod::Delete => "delete",
        _ => "read",
    }
}

/// Records `terminology_calls_total` and `terminology_call_duration_seconds`.
struct Instrumented {
    inner: Arc<dyn TerminologyService>,
    caller: TxCaller,
}

#[async_trait::async_trait]
impl TerminologyService for Instrumented {
    fn kind(&self) -> helios_terminology::ServiceKind {
        self.inner.kind()
    }

    async fn call(&self, request: TxRequest) -> Result<TxResponse, TxError> {
        let op = op_label(&request);
        let started = Instant::now();
        let result = self.inner.call(request).await;
        let outcome = match &result {
            Ok(r) if r.is_success() => "ok",
            Ok(r) if r.status == 404 => "not-found",
            Ok(r) if r.status < 500 => "client-error",
            Ok(_) => "server-error",
            Err(TxError::Timeout(_)) => "timeout",
            Err(TxError::Disabled) => "disabled",
            Err(_) => "unavailable",
        };
        let caller = self.caller.as_str();
        metrics::counter!("terminology_calls_total", "caller" => caller, "op" => op, "outcome" => outcome)
            .increment(1);
        metrics::histogram!("terminology_call_duration_seconds", "caller" => caller, "op" => op)
            .record(started.elapsed().as_secs_f64());
        result
    }

    fn changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.inner.changes()
    }

    async fn health(&self) -> helios_terminology::TerminologyHealth {
        self.inner.health().await
    }
}

/// The server's terminology service (`HFS_TERMINOLOGY`), installed once by
/// the `hfs` binary at startup.
struct ServerTerminology {
    /// The service as built (embedded loopback or remote).
    service: Arc<dyn TerminologyService>,
    /// The same behind the operation-permit semaphore (FHIR request paths).
    permitted: Arc<helios_terminology::PermittedTerminology<dyn TerminologyService>>,
    /// `HFS_TERMINOLOGY_SERVER` in remote mode (an endpoint binding naming
    /// the same URL shares this service); `None` when embedded.
    url: Option<String>,
    /// Label reported as the "base URL" (the remote URL, or `embedded`).
    label: String,
}

static SERVER_TERMINOLOGY: std::sync::RwLock<Option<Arc<ServerTerminology>>> =
    std::sync::RwLock::new(None);

/// Install the server's terminology service. `url` is the remote base in
/// remote mode (`None` embedded); `permits` caps concurrent calls from FHIR
/// request paths (0 = unlimited). Also makes it the default terminology of
/// FHIRPath evaluations (`memberOf()`, `%terminologies`), replacing the old
/// `FHIRPATH_TERMINOLOGY_SERVER` environment hand-off.
pub fn install_server_terminology(
    service: Arc<dyn TerminologyService>,
    url: Option<String>,
    permits: usize,
    timeout: Duration,
) {
    let label = url.clone().unwrap_or_else(|| "embedded".to_string());
    let permitted = Arc::new(helios_terminology::PermittedTerminology::new(
        Arc::clone(&service),
        permits,
        timeout,
    ));
    let installed = Arc::new(ServerTerminology {
        service,
        permitted,
        url,
        label,
    });
    helios_fhirpath::set_default_transport(Some(Arc::new(FhirpathTransport {
        service: instrumented(installed.permitted.clone(), TxCaller::Fhirpath),
        label: installed.label.clone(),
    })));
    *SERVER_TERMINOLOGY
        .write()
        .unwrap_or_else(|e| e.into_inner()) = Some(installed);
}

/// Remove the installed service (tests).
pub fn uninstall_server_terminology() {
    helios_fhirpath::set_default_transport(None);
    *SERVER_TERMINOLOGY
        .write()
        .unwrap_or_else(|e| e.into_inner()) = None;
}

fn installed() -> Option<Arc<ServerTerminology>> {
    SERVER_TERMINOLOGY
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn instrumented(
    inner: Arc<dyn TerminologyService>,
    caller: TxCaller,
) -> Arc<dyn TerminologyService> {
    Arc::new(Instrumented { inner, caller })
}

/// Operation permits currently free (`terminology_permits_available`);
/// `None` without an installed service.
pub fn permits_available() -> Option<usize> {
    installed().map(|s| s.permitted.available())
}

/// The raw installed service (health probe, `/tx` mount).
pub fn server_terminology() -> Option<Arc<dyn TerminologyService>> {
    installed().map(|s| Arc::clone(&s.service))
}

/// The remote URL of the installed service (`None` embedded or not installed).
pub fn server_terminology_url() -> Option<String> {
    installed().and_then(|s| s.url.clone())
}

/// Whether the installed service is the embedded one.
pub fn server_terminology_is_embedded() -> bool {
    installed().is_some_and(|s| s.url.is_none())
}

/// The terminology service a caller should use for `url`:
///
/// * `None` — the server's service (embedded or remote), when installed;
/// * the installed remote URL — the server's service;
/// * any other URL (an endpoint's own binding, or no service installed —
///   tests and other embedders) — a remote service for that URL, without a
///   token, as before.
///
/// Returns `(label, service)`; the label is the URL or `embedded`.
pub fn terminology_for(
    url: Option<&str>,
    caller: TxCaller,
) -> Option<(String, Arc<dyn TerminologyService>)> {
    let server = installed();
    let pick = |s: &Arc<ServerTerminology>| {
        let inner: Arc<dyn TerminologyService> = if caller.permitted() {
            s.permitted.clone()
        } else {
            Arc::clone(&s.service)
        };
        (s.label.clone(), instrumented(inner, caller))
    };
    match (url.map(|u| u.trim_end_matches('/')), &server) {
        (None, Some(s)) => Some(pick(s)),
        (None, None) => None,
        (Some(u), Some(s)) if s.url.as_deref().map(|x| x.trim_end_matches('/')) == Some(u) => {
            Some(pick(s))
        }
        (Some(u), _) => Some((u.to_string(), instrumented(remote_service(u, None), caller))),
    }
}

/// Last terminology health seen by the background probe (`hfs`).
static LAST_HEALTH: std::sync::RwLock<Option<helios_terminology::TerminologyHealth>> =
    std::sync::RwLock::new(None);

/// Record a health probe result (`/health` `components.terminology`,
/// `hfs_terminology_up`).
pub fn record_health(health: helios_terminology::TerminologyHealth) {
    use helios_terminology::service::HealthState;
    let up = matches!(health.state, HealthState::Ok | HealthState::Degraded(_));
    metrics::gauge!("hfs_terminology_up").set(if up { 1.0 } else { 0.0 });
    *LAST_HEALTH.write().unwrap_or_else(|e| e.into_inner()) = Some(health);
}

/// `{mode, state[, detail]}` for `/health`; `None` before the first probe.
pub fn health_component() -> Option<Value> {
    use helios_terminology::service::HealthState;
    let health = LAST_HEALTH
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()?;
    let (state, detail) = match &health.state {
        HealthState::Ok => ("ok", None),
        HealthState::Degraded(d) => ("degraded", Some(d.clone())),
        HealthState::Down(d) => ("down", Some(d.clone())),
    };
    let mut v = json!({"mode": health.kind.to_string(), "state": state});
    if let Some(d) = detail {
        v["detail"] = json!(redact_urls_in(&d));
    }
    Some(v)
}

/// Whether the last probe found the terminology service down.
pub fn terminology_down() -> bool {
    matches!(
        LAST_HEALTH
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|h| &h.state),
        Some(helios_terminology::service::HealthState::Down(_))
    )
}

/// Bridges the server's service into FHIRPath's terminology client.
struct FhirpathTransport {
    service: Arc<dyn TerminologyService>,
    label: String,
}

impl helios_fhirpath::TerminologyTransport for FhirpathTransport {
    fn label(&self) -> &str {
        &self.label
    }

    fn send(
        &self,
        request: helios_fhirpath::TransportRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<helios_fhirpath::TransportResponse, String>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let mut tx = match request.body {
                Some(body) => TxRequest::post_json(request.path.clone(), &body),
                None => TxRequest::get(request.path.clone()),
            };
            for (k, v) in &request.query {
                tx = tx.with_query_param(k, v);
            }
            if let Some(timeout) = request.timeout {
                tx = tx.with_timeout(timeout);
            }
            match self.service.call(tx).await {
                Ok(response) => Ok(helios_fhirpath::TransportResponse {
                    status: response.status,
                    retry_after: None,
                    body: response.body.to_vec(),
                }),
                Err(e) => Err(tx_error_text(&e)),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ─── an endpoint's own terminology server (token, outbound guard) ─────

    fn endpoint_with_token(
        ring: &helios_endpoints::secret::SecretKeyRing,
        url: &str,
    ) -> helios_endpoints::Endpoint {
        let id = "8d6f9b0e-0000-4000-8000-000000000001";
        let sealed = ring.seal_str("tx-secret", &format!("endpoint:{id}:terminology.token"));
        serde_json::from_value(json!({
            "id": id, "revision": 1,
            "name": "lab", "path": "lab", "displayName": "Lab",
            "storage": {"kind": "tenant", "tenantId": "lab"},
            "terminology": {"serverUrl": url, "token": sealed},
            "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z"
        }))
        .expect("endpoint")
    }

    /// A token-protected terminology server bound to an endpoint answers the
    /// server's calls once the endpoint's sealed token is opened and sent; an
    /// endpoint without a token stays anonymous (and is refused there).
    #[tokio::test]
    async fn an_endpoint_terminology_token_is_sent_on_server_calls() {
        use wiremock::matchers::{header, method};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(header("authorization", "Bearer tx-secret"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"resourceType": "Parameters"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let ring = helios_endpoints::secret::SecretKeyRing::parse(
            &helios_endpoints::secret::SecretKeyRing::generate_spec("k1"),
        )
        .unwrap();
        let endpoint = endpoint_with_token(&ring, &server.uri());
        let token = open_endpoint_token(&endpoint, &ring);
        assert_eq!(token.as_deref(), Some("tx-secret"));
        // The loopback mock must be allow-listed for the guarded transport.
        let transport: Arc<dyn HttpTransport> = Arc::new(ReqwestTransport::guarded(
            &helios_endpoints::OutboundPolicy::parse("127.0.0.1"),
        ));
        let (_, with) =
            endpoint_service_over(&server.uri(), token, TxCaller::Validator, transport.clone());
        let ok = with
            .call(TxRequest::get("CodeSystem/$lookup"))
            .await
            .unwrap();
        assert_eq!(ok.status, 200, "the endpoint token was not sent");
        let (_, without) =
            endpoint_service_over(&server.uri(), None, TxCaller::Validator, transport);
        let refused = without
            .call(TxRequest::get("CodeSystem/$lookup"))
            .await
            .unwrap();
        assert_eq!(refused.status, 401);
        // A different ring (or a token sealed for another endpoint) does not open.
        let other = helios_endpoints::secret::SecretKeyRing::parse(
            &helios_endpoints::secret::SecretKeyRing::generate_spec("k1"),
        )
        .unwrap();
        assert!(open_endpoint_token(&endpoint, &other).is_none());
        // The cache key changes with the token and never holds it.
        let fp = token_fingerprint(Some("tx-secret"));
        assert!(
            !fp.contains("tx-secret")
                && fp != token_fingerprint(Some("other"))
                && token_fingerprint(None).is_empty()
        );
    }

    /// An endpoint's terminology URL is console-entered: its calls refuse an
    /// internal address unless allow-listed (after resolution, not only at save).
    #[tokio::test]
    async fn an_endpoint_terminology_url_is_guarded() {
        let transport: Arc<dyn HttpTransport> = Arc::new(ReqwestTransport::guarded(
            &helios_endpoints::OutboundPolicy::strict(),
        ));
        let (_, service) =
            endpoint_service_over("http://localhost:9/fhir", None, TxCaller::Search, transport);
        assert!(service.call(TxRequest::get("metadata")).await.is_err());
    }

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

    /// A request that outlives the configured timeout fails at that timeout,
    /// and one inside it succeeds: the timeout is the configured value, not a
    /// fixed 10 s.
    #[tokio::test]
    async fn test_client_uses_the_configured_timeout() {
        use axum::{Router, routing::post};
        let app = Router::new().route(
            "/ValueSet/$expand",
            post(|| async {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                (
                    [("content-type", "application/fhir+json")],
                    r#"{"resourceType":"ValueSet","expansion":{"total":1,"contains":[{"system":"s","code":"c"}]}}"#,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let short = TerminologyServiceClient::with_limits_and_timeout(
            base.clone(),
            ExpansionLimits::default(),
            Duration::from_millis(300),
        );
        let started = std::time::Instant::now();
        assert!(short.expand_value_set("http://vs").await.is_err());
        assert!(started.elapsed() < Duration::from_millis(1200));

        let long = TerminologyServiceClient::with_limits_and_timeout(
            base,
            ExpansionLimits::default(),
            Duration::from_secs(5),
        );
        let codes = long
            .expand_value_set("http://vs")
            .await
            .expect("within timeout");
        assert_eq!(codes.len(), 1);
    }

    #[test]
    fn test_from_config_applies_timeout_and_default_is_thirty_seconds() {
        assert_eq!(DEFAULT_REQUEST_TIMEOUT, Duration::from_secs(30));
        let config = crate::config::SearchTerminologyConfig::default();
        assert_eq!(config.timeout_secs, 30);
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

    /// A request error names the URL it failed on; the configured base URL's
    /// query must not come along with it (#2149).
    #[tokio::test]
    async fn test_network_error_omits_the_url_query() {
        // Port 1 on loopback refuses the connection immediately.
        let client =
            TerminologyServiceClient::new("http://127.0.0.1:1?token=QUERY-SECRET".to_string());
        let err = client
            .expand_value_set("http://hl7.org/fhir/ValueSet/administrative-gender")
            .await
            .expect_err("nothing listens on port 1");
        let message = err.to_string();
        assert!(matches!(err, TerminologyError::Network(_)), "{message}");
        assert!(!message.contains("SECRET"), "query leaked: {message}");
        // reqwest names the URL; it is still there, redacted.
        assert!(message.contains("127.0.0.1:1"), "{message}");
    }
}
