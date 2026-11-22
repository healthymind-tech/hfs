//! The terminology contract every HFS subsystem talks to (design §4).
//!
//! One required method, [`TerminologyService::call`], carries a FHIR
//! terminology request expressed exactly as it would travel over HTTP
//! (method, path relative to the terminology base, query, body). The typed
//! helpers (`expand`, `validate_code`, `lookup`, …) build such a request and
//! have default bodies, so:
//!
//! * the **remote** implementation ([`crate::remote::RemoteTerminology`]) only
//!   has to send it over the wire — byte-identical to today's
//!   `HFS_TERMINOLOGY_SERVER` behaviour;
//! * the **in-process** implementation ([`crate::loopback::LoopbackTerminology`])
//!   hands it to the embedded HTS router without a socket — same handlers,
//!   same caches, same responses (phase 1);
//! * a later in-process implementation may override individual typed
//!   helpers with direct calls into the HTS operations (phase 2 fast paths)
//!   without touching any caller.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde_json::{Value, json};

/// The identity a terminology request is made under.
///
/// In-process calls carry it as a request extension, which no network peer
/// can forge; the embedded HTS write guard trusts [`TxPrincipal::System`]
/// and maps [`TxPrincipal::Console`] to the console role check (design §7).
/// The remote implementation turns `System` into the configured bearer
/// token (`HFS_TERMINOLOGY_SERVER_TOKEN`) and drops the others.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TxPrincipal {
    /// No caller identity: reads only (the embedded write guard refuses writes).
    #[default]
    Anonymous,
    /// The server itself: package manager imports and uninstall deletes,
    /// validation, search expansion. May write.
    System,
    /// A signed-in console user acting through a console page.
    Console {
        /// Console user name (audit agent).
        user: String,
        /// Effective role for terminology (global role or terminology binding).
        role: helios_endpoints::ConsoleRole,
    },
}

/// HTTP method subset the terminology API uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxMethod {
    Get,
    Post,
    Put,
    Delete,
}

impl TxMethod {
    pub fn as_http(self) -> http::Method {
        match self {
            TxMethod::Get => http::Method::GET,
            TxMethod::Post => http::Method::POST,
            TxMethod::Put => http::Method::PUT,
            TxMethod::Delete => http::Method::DELETE,
        }
    }

    /// Whether the request can change terminology content (`POST` operations
    /// such as `$expand` are classified by the server, not here).
    pub fn may_write(self) -> bool {
        !matches!(self, TxMethod::Get)
    }
}

/// A terminology request relative to the terminology base
/// (`ValueSet/$expand`, `CodeSystem?url=…`, `import`, or `""` for a batch).
#[derive(Debug, Clone, PartialEq)]
pub struct TxRequest {
    pub method: TxMethod,
    /// Path without a leading slash, relative to the terminology base.
    pub path: String,
    /// Raw query string without `?` (already percent-encoded), may be empty.
    pub query: String,
    /// JSON body (`application/fhir+json`), if any.
    pub body: Option<Bytes>,
    pub principal: TxPrincipal,
    /// Per-call deadline; `None` = the service default.
    pub timeout: Option<Duration>,
    /// `Accept-Language` / `displayLanguage` passthrough.
    pub accept_language: Option<String>,
}

impl TxRequest {
    pub fn get(path: impl Into<String>) -> Self {
        Self {
            method: TxMethod::Get,
            path: normalize_path(path.into()),
            query: String::new(),
            body: None,
            principal: TxPrincipal::Anonymous,
            timeout: None,
            accept_language: None,
        }
    }

    pub fn post_json(path: impl Into<String>, body: &Value) -> Self {
        Self {
            method: TxMethod::Post,
            body: Some(Bytes::from(serde_json::to_vec(body).unwrap_or_default())),
            ..Self::get(path)
        }
    }

    pub fn delete(path: impl Into<String>) -> Self {
        Self {
            method: TxMethod::Delete,
            ..Self::get(path)
        }
    }

    /// Append `name=value` to the query, percent-encoding both.
    pub fn with_query_param(mut self, name: &str, value: &str) -> Self {
        let pair: String = url::form_urlencoded::Serializer::new(String::new())
            .append_pair(name, value)
            .finish();
        if self.query.is_empty() {
            self.query = pair;
        } else {
            self.query.push('&');
            self.query.push_str(&pair);
        }
        self
    }

    pub fn as_principal(mut self, principal: TxPrincipal) -> Self {
        self.principal = principal;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// `/path?query` as an origin-form request target.
    pub fn target(&self) -> String {
        if self.query.is_empty() {
            format!("/{}", self.path)
        } else {
            format!("/{}?{}", self.path, self.query)
        }
    }
}

fn normalize_path(path: String) -> String {
    path.trim_start_matches('/').to_string()
}

/// A terminology response: status and JSON body.
#[derive(Debug, Clone, PartialEq)]
pub struct TxResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Bytes,
}

impl TxResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn json(&self) -> Result<Value, TxError> {
        if self.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&self.body).map_err(|e| TxError::Decode(e.to_string()))
    }

    /// The body as JSON when 2xx, else a [`TxError::Status`] carrying the
    /// OperationOutcome text (first `issue.diagnostics` / `details.text`).
    pub fn into_json(self) -> Result<Value, TxError> {
        if self.is_success() {
            return self.json();
        }
        let message = self
            .json()
            .ok()
            .and_then(|v| outcome_message(&v))
            .unwrap_or_else(|| {
                String::from_utf8_lossy(&self.body)
                    .chars()
                    .take(300)
                    .collect()
            });
        Err(TxError::Status {
            status: self.status,
            message,
        })
    }
}

fn outcome_message(v: &Value) -> Option<String> {
    let issue = v.get("issue")?.as_array()?.first()?;
    issue
        .get("diagnostics")
        .and_then(Value::as_str)
        .or_else(|| issue.pointer("/details/text").and_then(Value::as_str))
        .map(str::to_string)
}

/// Errors common to every implementation; callers map them onto their own
/// failure policies (`HFS_SEARCH_TERMINOLOGY_FAIL`,
/// `HFS_VALIDATION_TERMINOLOGY_FAIL`, package import retry).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TxError {
    /// Terminology is switched off (`HFS_TERMINOLOGY=off`).
    #[error("terminology service is disabled")]
    Disabled,
    /// Could not reach / run the service (connect error, embedded service
    /// not ready, overload shed).
    #[error("terminology service unavailable: {0}")]
    Unavailable(String),
    #[error("terminology request timed out after {0:?}")]
    Timeout(Duration),
    /// Non-2xx answer.
    #[error("terminology server answered {status}: {message}")]
    Status { status: u16, message: String },
    #[error("terminology response could not be decoded: {0}")]
    Decode(String),
}

impl TxError {
    /// Whether a retry may succeed (package import retry, validator
    /// `fail=open` decision).
    pub fn is_transient(&self) -> bool {
        match self {
            TxError::Unavailable(_) | TxError::Timeout(_) => true,
            TxError::Status { status, .. } => matches!(status, 429 | 502 | 503 | 504),
            TxError::Disabled | TxError::Decode(_) => false,
        }
    }
}

/// Which implementation is behind a service handle (health tile, metrics label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceKind {
    /// HTS linked into this process.
    Embedded,
    /// An external terminology server over HTTP.
    Remote,
    /// No terminology.
    Disabled,
}

impl fmt::Display for ServiceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ServiceKind::Embedded => "embedded",
            ServiceKind::Remote => "remote",
            ServiceKind::Disabled => "off",
        })
    }
}

/// Health as the console overview and `/_readiness` report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthState {
    Ok,
    Degraded(String),
    Down(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminologyHealth {
    pub kind: ServiceKind,
    pub state: HealthState,
}

/// `ValueSet/$expand` parameters HFS uses (search `:in`, SoF, editor).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExpandParams {
    pub url: String,
    pub value_set_version: Option<String>,
    pub filter: Option<String>,
    pub count: Option<u32>,
    pub offset: Option<u32>,
    pub exclude_nested: bool,
}

impl ExpandParams {
    pub fn to_parameters(&self) -> Value {
        let mut p = vec![json!({"name": "url", "valueUri": self.url})];
        if let Some(v) = &self.value_set_version {
            p.push(json!({"name": "valueSetVersion", "valueString": v}));
        }
        if let Some(f) = &self.filter {
            p.push(json!({"name": "filter", "valueString": f}));
        }
        if let Some(c) = self.count {
            p.push(json!({"name": "count", "valueInteger": c}));
        }
        if let Some(o) = self.offset {
            p.push(json!({"name": "offset", "valueInteger": o}));
        }
        if self.exclude_nested {
            p.push(json!({"name": "excludeNested", "valueBoolean": true}));
        }
        json!({"resourceType": "Parameters", "parameter": p})
    }
}

/// `$validate-code` against a ValueSet (`value_set = Some`) or a CodeSystem.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValidateCodeParams {
    pub value_set: Option<String>,
    pub system: String,
    pub code: String,
    pub version: Option<String>,
    pub display: Option<String>,
    pub display_language: Option<String>,
}

impl ValidateCodeParams {
    pub fn path(&self) -> &'static str {
        if self.value_set.is_some() {
            "ValueSet/$validate-code"
        } else {
            "CodeSystem/$validate-code"
        }
    }

    pub fn to_parameters(&self) -> Value {
        let mut p = Vec::new();
        if let Some(vs) = &self.value_set {
            p.push(json!({"name": "url", "valueUri": vs}));
            p.push(json!({"name": "system", "valueUri": self.system}));
        } else {
            p.push(json!({"name": "url", "valueUri": self.system}));
        }
        p.push(json!({"name": "code", "valueCode": self.code}));
        if let Some(v) = &self.version {
            let name = if self.value_set.is_some() {
                "systemVersion"
            } else {
                "version"
            };
            p.push(json!({"name": name, "valueString": v}));
        }
        if let Some(d) = &self.display {
            p.push(json!({"name": "display", "valueString": d}));
        }
        if let Some(l) = &self.display_language {
            p.push(json!({"name": "displayLanguage", "valueCode": l}));
        }
        json!({"resourceType": "Parameters", "parameter": p})
    }
}

/// Outcome of `$validate-code` reduced to what callers branch on.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidateCodeOutcome {
    pub result: bool,
    pub message: Option<String>,
    pub display: Option<String>,
    /// The full `Parameters` resource, for callers that need more.
    pub raw: Value,
}

impl ValidateCodeOutcome {
    pub fn from_parameters(raw: Value) -> Result<Self, TxError> {
        let find = |name: &str| -> Option<&Value> {
            raw.get("parameter")?
                .as_array()?
                .iter()
                .find(|p| p.get("name").and_then(Value::as_str) == Some(name))
        };
        let result = find("result")
            .and_then(|p| p.get("valueBoolean"))
            .and_then(Value::as_bool)
            .ok_or_else(|| TxError::Decode("$validate-code without result".into()))?;
        let message = find("message")
            .and_then(|p| p.get("valueString"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let display = find("display")
            .and_then(|p| p.get("valueString"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(Self {
            result,
            message,
            display,
            raw,
        })
    }
}

/// Counts reported by `POST /import` (package manager).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub code_systems: u64,
    pub value_sets: u64,
    pub concept_maps: u64,
    pub errors: Vec<String>,
}

impl ImportReport {
    pub fn from_json(v: &Value) -> Self {
        let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        let errors = v
            .get("errors")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            code_systems: n("code_systems"),
            value_sets: n("value_sets"),
            concept_maps: n("concept_maps"),
            errors,
        }
    }
}

/// The terminology service (design §4.1).
///
/// Object safe; held as `Arc<dyn TerminologyService>` in `AppState`, the UI
/// state, the validator's provider and the FHIRPath resolver.
#[async_trait]
pub trait TerminologyService: Send + Sync + 'static {
    fn kind(&self) -> ServiceKind;

    /// Run one terminology request.
    async fn call(&self, request: TxRequest) -> Result<TxResponse, TxError>;

    /// Terminology content epoch (`terminology_epoch`), when the
    /// implementation can observe it. The embedded service forwards HTS's
    /// cache-sync poll (local writes immediately, other instances within
    /// `HFS_TERMINOLOGY_CACHE_SYNC_INTERVAL_MS`); HFS-side caches (validator
    /// verdicts, `:in` expansions, SoF dependencies) clear on every change
    /// instead of waiting for their TTL (design §4.5). Remote: `None` (TTL).
    fn changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        None
    }

    /// Health for the console tile and readiness. Default: `GET health`.
    async fn health(&self) -> TerminologyHealth {
        let kind = self.kind();
        let state = match self
            .call(TxRequest::get("health").with_timeout(Duration::from_secs(3)))
            .await
        {
            Ok(r) if r.is_success() => {
                let body = r.json().unwrap_or(Value::Null);
                match body.get("status").and_then(Value::as_str) {
                    Some("degraded") => HealthState::Degraded("server reports degraded".into()),
                    _ => HealthState::Ok,
                }
            }
            Ok(r) => HealthState::Down(format!("health answered {}", r.status)),
            Err(e) => HealthState::Down(e.to_string()),
        };
        TerminologyHealth { kind, state }
    }

    /// `POST ValueSet/$expand`, returns the expanded ValueSet.
    async fn expand(&self, params: &ExpandParams) -> Result<Value, TxError> {
        let req = TxRequest::post_json("ValueSet/$expand", &params.to_parameters())
            .as_principal(TxPrincipal::System);
        self.call(req).await?.into_json()
    }

    /// `POST {ValueSet|CodeSystem}/$validate-code`.
    async fn validate_code(
        &self,
        params: &ValidateCodeParams,
    ) -> Result<ValidateCodeOutcome, TxError> {
        let req = TxRequest::post_json(params.path(), &params.to_parameters())
            .as_principal(TxPrincipal::System);
        ValidateCodeOutcome::from_parameters(self.call(req).await?.into_json()?)
    }

    /// `GET CodeSystem/$lookup?system=&code=`.
    async fn lookup(&self, system: &str, code: &str) -> Result<Value, TxError> {
        let req = TxRequest::get("CodeSystem/$lookup")
            .with_query_param("system", system)
            .with_query_param("code", code);
        self.call(req).await?.into_json()
    }

    /// `GET CodeSystem/$subsumes?system=&codeA=&codeB=`; returns the outcome
    /// code (`equivalent`, `subsumes`, `subsumed-by`, `not-subsumed`).
    async fn subsumes(&self, system: &str, code_a: &str, code_b: &str) -> Result<String, TxError> {
        let req = TxRequest::get("CodeSystem/$subsumes")
            .with_query_param("system", system)
            .with_query_param("codeA", code_a)
            .with_query_param("codeB", code_b);
        let v = self.call(req).await?.into_json()?;
        v.get("parameter")
            .and_then(Value::as_array)
            .and_then(|a| {
                a.iter()
                    .find(|p| p.get("name").and_then(Value::as_str) == Some("outcome"))
            })
            .and_then(|p| p.get("valueCode"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| TxError::Decode("$subsumes without outcome".into()))
    }

    /// `POST ConceptMap/$translate` with a caller-built `Parameters`.
    async fn translate(&self, parameters: &Value) -> Result<Value, TxError> {
        self.call(TxRequest::post_json("ConceptMap/$translate", parameters))
            .await?
            .into_json()
    }

    /// `POST /` batch Bundle (validator batches of read operations).
    async fn batch(&self, bundle: &Value) -> Result<Value, TxError> {
        let req = TxRequest::post_json("", bundle).as_principal(TxPrincipal::System);
        self.call(req).await?.into_json()
    }

    /// `GET {resource_type}?url=&version=` search (Bundle).
    async fn find_by_url(
        &self,
        resource_type: &str,
        url: &str,
        version: Option<&str>,
    ) -> Result<Value, TxError> {
        let mut req = TxRequest::get(resource_type).with_query_param("url", url);
        if let Some(v) = version {
            req = req.with_query_param("version", v);
        }
        self.call(req).await?.into_json()
    }

    /// `POST import` of a collection Bundle (package manager). Writes:
    /// always made as [`TxPrincipal::System`].
    async fn import_bundle(&self, bundle: &Value) -> Result<ImportReport, TxError> {
        let req = TxRequest::post_json("import", bundle)
            .as_principal(TxPrincipal::System)
            .with_timeout(Duration::from_secs(900));
        Ok(ImportReport::from_json(&self.call(req).await?.into_json()?))
    }

    /// `DELETE {resource_type}/{id}` (package uninstall).
    async fn delete(&self, resource_type: &str, id: &str) -> Result<(), TxError> {
        let req =
            TxRequest::delete(format!("{resource_type}/{id}")).as_principal(TxPrincipal::System);
        let resp = self.call(req).await?;
        if resp.is_success() || resp.status == 404 || resp.status == 410 {
            Ok(())
        } else {
            resp.into_json().map(|_| ())
        }
    }
}

/// The `HFS_TERMINOLOGY=off` service: every call is [`TxError::Disabled`].
#[derive(Debug, Default, Clone, Copy)]
pub struct DisabledTerminology;

#[async_trait]
impl TerminologyService for DisabledTerminology {
    fn kind(&self) -> ServiceKind {
        ServiceKind::Disabled
    }

    async fn call(&self, _request: TxRequest) -> Result<TxResponse, TxError> {
        Err(TxError::Disabled)
    }

    async fn health(&self) -> TerminologyHealth {
        TerminologyHealth {
            kind: ServiceKind::Disabled,
            state: HealthState::Down("terminology is off".into()),
        }
    }
}
