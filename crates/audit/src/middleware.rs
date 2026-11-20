//! Axum middleware that records a BALP `AuditEvent` for every FHIR REST
//! interaction.
//!
//! Install it as the **outermost** of the FHIR API's own layers (outside
//! authentication), so it also sees the requests authentication and
//! authorization refuse (401/403):
//!
//! ```rust,ignore
//! router.layer(axum::middleware::from_fn_with_state(audit_state, audit_middleware))
//! ```
//!
//! For each request the middleware
//!
//! 1. opens a [`RequestAudit`] context (task-local) that the inner layers and
//!    handlers report into — the resolved tenant, the authenticated user, the
//!    per-entry results of a batch/transaction (see [`crate::context`]);
//! 2. runs the request;
//! 3. classifies it ([`crate::interaction::classify`]), combines it with the
//!    response status, the failure text of an `OperationOutcome`, the
//!    [`AuditResponseContext`] a handler attached, and the captured context;
//! 4. hands the resulting `AuditEvent`(s) to the sink *before* returning the
//!    response. With the production [`QueuedSink`](crate::sinks::QueuedSink)
//!    that is a queue push, and the queue is drained on graceful shutdown, so
//!    no event of a completed request is lost.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::body::HttpBody as _;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;

pub use crate::agent::AuditAgent;
use crate::config::AuditConfig;
use crate::context::{Captured, RequestAudit};
use crate::correlation::{AuditCorrelation, BundleAuditEntry};
use crate::exclusion::ExclusionFilter;
use crate::interaction::RestInteraction;
use crate::rest_event::{AuditUser, RequestParties, RestEventSpec, build_rest_events};
use crate::sink::AuditSink;

/// Largest request/response body the middleware buffers to read a form search
/// or an `OperationOutcome` (bigger bodies are passed through untouched).
const MAX_INSPECTED_BODY: u64 = 64 * 1024;

/// The request header that carries a client-chosen request id.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Shared state for the audit middleware layer.
pub struct AuditMiddlewareState {
    /// The active audit sink.
    pub sink: Arc<dyn AuditSink>,
    /// Audit configuration.
    pub config: AuditConfig,
    /// Pre-built exclusion filter.
    pub exclusion_filter: ExclusionFilter,
    /// Public base URL of the server (`agent.network.address` of the server).
    pub server_address: String,
    /// Whether requests may carry a tenant as a URL path prefix. Used to strip
    /// the prefix of a request refused before its tenant was resolved.
    pub url_tenant_routing: bool,
}

impl AuditMiddlewareState {
    /// State with the configured exclusions and no server address.
    pub fn new(sink: Arc<dyn AuditSink>, config: AuditConfig) -> Self {
        let exclusion_filter = ExclusionFilter::from_config(&config);
        Self {
            sink,
            config,
            exclusion_filter,
            server_address: String::new(),
            url_tenant_routing: false,
        }
    }

    /// Sets the public base URL of the server.
    pub fn with_server_address(mut self, address: impl Into<String>) -> Self {
        self.server_address = address.into();
        self
    }

    /// Declares URL-path tenant routing.
    pub fn with_url_tenant_routing(mut self, enabled: bool) -> Self {
        self.url_tenant_routing = enabled;
        self
    }

    /// Record correlated audit events for each entry in a batch or transaction
    /// bundle, outside an audited request (the middleware records entries
    /// reported through [`crate::context::push_entry`] itself).
    pub async fn record_bundle_entries(
        &self,
        correlation: &AuditCorrelation,
        entries: Vec<BundleAuditEntry>,
        agent: Option<&str>,
    ) {
        let events: Vec<_> = entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                crate::correlation::build_entry_event(
                    correlation,
                    &self.config.source_observer,
                    i,
                    entry,
                    agent,
                )
            })
            .collect();
        self.sink.record_batch(events).await;
    }
}

/// Slot for the tenant a request resolved to (#1515).
///
/// The tenant is resolved inside the handler (by the REST layer's tenant
/// extractor), after this middleware has handed the request on. The middleware
/// inserts an empty slot into the request extensions; whatever resolves the
/// tenant fills it, and the middleware reads it back once the handler returns
/// to tag the event. A request that never resolved a tenant (rejected before
/// the handler, or naming a reserved tenant) leaves the event untagged.
#[derive(Clone, Debug, Default)]
pub struct AuditTenant(Arc<OnceLock<String>>);

impl AuditTenant {
    /// Records the resolved tenant. Only the first value is kept.
    pub fn set(&self, tenant_id: &str) {
        let _ = self.0.set(tenant_id.to_string());
    }

    /// The resolved tenant, if any.
    pub fn get(&self) -> Option<&str> {
        self.0.get().map(String::as_str)
    }
}

/// Optional audit context attached by handlers to enrich middleware audit events.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct AuditResponseContext {
    /// Final resource type after handler processing.
    pub resource_type: Option<String>,
    /// Final resource ID after handler processing (e.g., create-assigned IDs).
    pub resource_id: Option<String>,
    /// Resolved patient reference from the resulting resource.
    pub patient_reference: Option<String>,
}

/// Axum middleware function that records audit events for FHIR operations.
pub async fn audit_middleware(
    State(state): State<Arc<AuditMiddlewareState>>,
    mut request: axum::extract::Request,
    next: Next,
) -> Response {
    let method = request.method().as_str().to_string();
    let path = request.uri().path().to_string();

    if state.exclusion_filter.is_excluded(&path, &method) {
        return next.run(request).await;
    }
    if !state.config.reads && is_read_request(&method, &path) {
        return next.run(request).await;
    }

    let query = request.uri().query().map(str::to_string);
    let client_address = client_address(&request, state.config.trust_forwarded_for);
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(sanitize_request_id)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    // A legacy `AuditAgent` (set by an outer layer) is the best we have when
    // no user is reported from inside.
    let legacy_agent = request.extensions().get::<AuditAgent>().cloned();
    // Upstream's tenant slot (#1515): the REST tenant extractor fills it; used
    // when the task-local context captured no tenant.
    let tenant_slot = AuditTenant::default();
    request.extensions_mut().insert(tenant_slot.clone());

    let (request, form_body) = capture_form_search(request, &method, &path).await;

    let ctx = RequestAudit::new();
    let mut response = ctx.clone().scope(next.run(request)).await;

    if !response.headers().contains_key(REQUEST_ID_HEADER)
        && let Ok(v) = HeaderValue::from_str(&request_id)
    {
        response.headers_mut().insert(REQUEST_ID_HEADER, v);
    }

    let status = response.status().as_u16();
    let outcome_desc = if status >= 400 {
        let (resp, text) = failure_text(response).await;
        response = resp;
        let reason = axum::http::StatusCode::from_u16(status)
            .ok()
            .and_then(|s| s.canonical_reason())
            .unwrap_or("");
        Some(match text {
            Some(t) => format!("HTTP {status} {reason}: {t}"),
            None => format!("HTTP {status} {reason}"),
        })
    } else {
        None
    };
    let mut response_ctx = response.extensions().get::<AuditResponseContext>().cloned();
    // A create answered by a handler that sets no audit context still names
    // what it created in `Location` (HTS, and any embedder).
    if response_ctx
        .as_ref()
        .is_none_or(|c| c.resource_id.is_none())
        && let Some((rt, id)) = response
            .headers()
            .get(header::LOCATION)
            .or_else(|| response.headers().get(header::CONTENT_LOCATION))
            .and_then(|v| v.to_str().ok())
            .and_then(resource_from_location)
    {
        let ctx = response_ctx.get_or_insert_with(AuditResponseContext::default);
        ctx.resource_type.get_or_insert(rt);
        ctx.resource_id = Some(id);
    }

    let mut captured = ctx.take();
    if captured.user.is_none()
        && let Some(agent) = legacy_agent
    {
        captured.user = Some(AuditUser {
            subject: agent.subject,
            issuer: agent.issuer,
            name: agent.display,
            fhir_user: agent.reference,
            ..Default::default()
        });
    }

    let parties = RequestParties {
        client_address,
        user: captured.user.clone(),
        server_observer: state.config.source_observer.clone(),
        server_address: state.server_address.clone(),
        site: site_of(&state.server_address),
        request_id: Some(request_id),
    };

    if captured.tenant.is_none()
        && let Some(t) = tenant_slot.get()
    {
        captured.tenant = Some(t.to_string());
    }

    let fhir_path = strip_tenant(&path, &captured, state.url_tenant_routing);
    let events = events_for_request(
        &method,
        &fhir_path,
        query.as_deref(),
        form_body.as_deref(),
        status,
        outcome_desc,
        response_ctx,
        &mut captured,
        &parties,
    );

    let tenant = captured.tenant.clone();
    let endpoint_extension = captured.endpoint.as_deref().and_then(endpoint_extension);
    for mut event in events {
        if let Some(ext) = &endpoint_extension {
            event
                .extension
                .get_or_insert_with(Vec::new)
                .push(ext.clone());
        }
        // Upstream #1515: tag the event with the request's tenant (searchable
        // with `_tag`); reserved `__…` tenants are left untagged.
        if let Some(t) = tenant
            .as_deref()
            .filter(|t| !t.is_empty() && !t.starts_with("__"))
        {
            let meta = event.meta.get_or_insert_with(Default::default);
            let tags = meta.tag.get_or_insert_with(Vec::new);
            let already = tags.iter().any(|c| {
                c.system.as_ref().and_then(|s| s.value.as_deref())
                    == Some(crate::balp::code_systems::HFS_TENANT)
            });
            if !already {
                tags.push(crate::helpers::coding(
                    crate::balp::code_systems::HFS_TENANT,
                    t,
                ));
            }
        }
        state.sink.record_in(tenant.as_deref(), event).await;
    }

    response
}

#[allow(clippy::too_many_arguments)]
fn events_for_request(
    method: &str,
    path: &str,
    query: Option<&str>,
    form_body: Option<&str>,
    status: u16,
    outcome_desc: Option<String>,
    response_ctx: Option<AuditResponseContext>,
    captured: &mut Captured,
    parties: &RequestParties,
) -> Vec<crate::fhir_model::AuditEvent> {
    let mut spec =
        RestEventSpec::from_request(method, path, query, form_body, &parties.server_address);
    spec.status = status;
    spec.outcome_desc = outcome_desc;

    if let Some(ctx) = response_ctx {
        if spec.resource_type.is_none() {
            spec.resource_type = ctx.resource_type;
        }
        if spec.resource_id.is_none() {
            spec.resource_id = ctx.resource_id;
        }
        if let Some(p) = ctx.patient_reference {
            spec.add_patient(&p);
        }
    }
    for p in &captured.patients {
        spec.add_patient(p);
    }

    let is_bundle = matches!(
        spec.interaction,
        Some(RestInteraction::Batch | RestInteraction::Transaction)
    );
    if is_bundle && captured.bundle_type.as_deref() == Some("transaction") {
        spec.interaction = Some(RestInteraction::Transaction);
    }

    let mut events = Vec::new();
    let entries = std::mem::take(&mut captured.entries);
    for mut entry in entries {
        if entry.request_line.is_none() {
            entry.request_line = spec.request_line.clone();
        }
        events.extend(build_rest_events(&entry, parties));
    }
    // A bundle is recorded through its entries; the envelope itself only
    // when it failed as a whole or produced no entry events.
    if !is_bundle || status >= 400 || events.is_empty() {
        events.extend(build_rest_events(&spec, parties));
    }
    events
}

/// The client's address: the peer, or with `trust_forwarded_for` the first
/// address of `X-Forwarded-For` set by a trusted reverse proxy.
fn client_address(request: &axum::extract::Request, trust_forwarded_for: bool) -> Option<String> {
    if trust_forwarded_for
        && let Some(first) = request
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    {
        return Some(first.chars().take(64).collect());
    }
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip().to_string())
}

/// Read-only FHIR operations: invoking one changes nothing.
const READ_ONLY_OPERATIONS: &[&str] = &[
    "$expand",
    "$lookup",
    "$validate-code",
    "$batch-validate-code",
    "$translate",
    "$subsumes",
    "$validate",
    "$everything",
    "$meta",
    "$versions",
];

/// Whether a request only reads: `GET`/`HEAD`, a `POST _search`, or a `POST`
/// to a read-only operation. Used when reads are not audited
/// (`*_AUDIT_READS=false`).
pub fn is_read_request(method: &str, path: &str) -> bool {
    match method.to_ascii_uppercase().as_str() {
        "GET" | "HEAD" => true,
        "POST" => {
            let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
            last == "_search" || READ_ONLY_OPERATIONS.contains(&last)
        }
        _ => false,
    }
}

/// `(type, id)` named by a `Location` header (`<base>/CodeSystem/x/_history/1`).
fn resource_from_location(location: &str) -> Option<(String, String)> {
    let path = location.split(['?', '#']).next()?;
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    (0..segs.len().saturating_sub(1)).rev().find_map(|i| {
        let (rt, id) = (segs[i], segs[i + 1]);
        (crate::interaction::is_resource_type(rt) && !id.starts_with(['_', '$']))
            .then(|| (rt.to_string(), id.to_string()))
    })
}

/// Keeps a client-supplied request id printable and bounded.
fn sanitize_request_id(v: &str) -> String {
    v.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
        .take(128)
        .collect()
}

fn site_of(base_url: &str) -> Option<String> {
    let rest = base_url.split("://").nth(1)?;
    let host = rest.split(['/', '?']).next()?;
    (!host.is_empty()).then(|| host.to_string())
}

/// The `hfs-endpoint` extension naming the endpoint an event belongs to.
///
/// Built directly rather than through serde: deserializing an `Extension`
/// (its `value[x]` choice spans every data type) costs ~100 KiB of stack in
/// a debug build, on every audited request.
fn endpoint_extension(name: &str) -> Option<crate::fhir_model::Extension> {
    Some(crate::fhir_model::Extension {
        id: None,
        extension: None,
        url: crate::context::ENDPOINT_EXTENSION_URL.to_string().into(),
        value: Some(crate::fhir_model::ExtensionValue::String(
            crate::helpers::fhir_string(name),
        )),
    })
}

/// The request path without its tenant prefix (or the FHIR endpoint prefix
/// the endpoint dispatcher stripped).
fn strip_tenant(path: &str, captured: &Captured, url_tenant_routing: bool) -> String {
    if let Some(prefix) = captured.endpoint_prefix.as_deref()
        && let Some(rest) = path.strip_prefix(prefix)
        && (rest.is_empty() || rest.starts_with('/'))
    {
        return if rest.is_empty() {
            "/".to_string()
        } else {
            rest.to_string()
        };
    }
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let first_is_tenant = match (&captured.tenant, captured.tenant_from_url) {
        (Some(t), true) => segs.first() == Some(&t.as_str()),
        // Refused before the tenant was resolved: under URL routing a leading
        // segment that is not part of the FHIR surface is the tenant.
        (None, _) if url_tenant_routing => {
            segs.first().is_some_and(|s| {
                !crate::interaction::is_resource_type(s)
                    && !s.starts_with('$')
                    && !s.starts_with('_')
                    && *s != "metadata"
            }) && segs.len() > 1
        }
        _ => false,
    };
    if first_is_tenant {
        format!("/{}", segs[1..].join("/"))
    } else {
        path.to_string()
    }
}

fn content_length_ok(body: &Body) -> bool {
    body.size_hint()
        .upper()
        .is_some_and(|u| u <= MAX_INSPECTED_BODY)
}

/// For a `POST [type]/_search` with a form body, buffers the (small) body so
/// the search parameters can be recorded, and puts it back.
async fn capture_form_search(
    request: axum::extract::Request,
    method: &str,
    path: &str,
) -> (axum::extract::Request, Option<String>) {
    let is_form = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"));
    if method != "POST" || !path.ends_with("/_search") || !is_form {
        return (request, None);
    }
    let declared_small = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n <= MAX_INSPECTED_BODY);
    if !declared_small && !content_length_ok(request.body()) {
        return (request, None);
    }
    let (parts, body) = request.into_parts();
    match axum::body::to_bytes(body, MAX_INSPECTED_BODY as usize).await {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            (
                axum::extract::Request::from_parts(parts, Body::from(bytes)),
                Some(text),
            )
        }
        // Only reachable when the body lied about its length; the request
        // body is gone, so the handler sees an empty one and rejects it.
        Err(_) => (
            axum::extract::Request::from_parts(parts, Body::empty()),
            None,
        ),
    }
}

/// Extracts the human-readable reason from a failure response carrying a
/// (small) `OperationOutcome`, and rebuilds the response.
async fn failure_text(response: Response) -> (Response, Option<String>) {
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("json"));
    if !is_json || !content_length_ok(response.body()) {
        return (response, None);
    }
    let (parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_INSPECTED_BODY as usize).await else {
        return (Response::from_parts(parts, Body::empty()), None);
    };
    let text = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| operation_outcome_text(&v));
    (Response::from_parts(parts, Body::from(bytes)), text)
}

fn operation_outcome_text(v: &serde_json::Value) -> Option<String> {
    if v.get("resourceType")?.as_str()? != "OperationOutcome" {
        return None;
    }
    let texts: Vec<String> = v
        .get("issue")?
        .as_array()?
        .iter()
        .filter_map(|i| {
            i.get("diagnostics")
                .and_then(|d| d.as_str())
                .or_else(|| i.pointer("/details/text").and_then(|d| d.as_str()))
                .map(str::to_string)
        })
        .take(3)
        .collect();
    let joined = texts.join("; ");
    (!joined.is_empty()).then(|| joined.chars().take(1024).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fhir_model::AuditEvent;
    use crate::sink::AuditRecord;
    use async_trait::async_trait;
    use axum::Router;
    use axum::routing::{get, post};
    use tower::ServiceExt;

    #[derive(Default)]
    struct Collect(std::sync::Mutex<Vec<AuditRecord>>);

    #[async_trait]
    impl AuditSink for Collect {
        async fn record(&self, event: AuditEvent) {
            self.record_in(None, event).await
        }
        async fn record_in(&self, tenant: Option<&str>, event: AuditEvent) {
            self.0.lock().unwrap().push(AuditRecord {
                tenant: tenant.map(str::to_string),
                event,
            });
        }
        async fn flush(&self) {}
        fn name(&self) -> &str {
            "collect"
        }
    }

    impl Collect {
        fn json(&self) -> Vec<(Option<String>, serde_json::Value)> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|r| (r.tenant.clone(), serde_json::to_value(&r.event).unwrap()))
                .collect()
        }
    }

    fn app(sink: Arc<Collect>) -> Router {
        let state = Arc::new(
            AuditMiddlewareState::new(sink, AuditConfig::default())
                .with_server_address("http://fhir.test/fhir"),
        );
        Router::new()
            .route(
                "/Patient/{id}",
                get(|| async {
                    crate::context::set_tenant("t1", false);
                    crate::context::set_user(AuditUser {
                        subject: "alice".into(),
                        ..Default::default()
                    });
                    "{}"
                }),
            )
            .route(
                "/Observation/{id}",
                get(|| async {
                    let mut r = Response::new(Body::from("{}"));
                    r.extensions_mut().insert(AuditResponseContext {
                        patient_reference: Some("Patient/p7".into()),
                        ..Default::default()
                    });
                    r
                }),
            )
            .route(
                "/Patient/_search",
                post(|body: String| async move { body }),
            )
            .route(
                "/Basic/{id}",
                get(|| async {
                    (
                        axum::http::StatusCode::NOT_FOUND,
                        [(header::CONTENT_TYPE, "application/fhir+json")],
                        r#"{"resourceType":"OperationOutcome","issue":[{"severity":"error","code":"not-found","diagnostics":"Basic/x is not known"}]}"#,
                    )
                }),
            )
            .route(
                "/",
                post(|| async {
                    crate::context::set_bundle_type("transaction");
                    let mut e = RestEventSpec::from_request(
                        "POST",
                        "/Patient",
                        None,
                        None,
                        "http://fhir.test/fhir",
                    );
                    e.status = 201;
                    e.resource_id = Some("new".into());
                    crate::context::push_entry(e);
                    "{}"
                }),
            )
            .route("/health", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state, audit_middleware))
    }

    async fn call(app: Router, req: axum::http::Request<Body>) -> Response {
        app.oneshot(req).await.unwrap()
    }

    #[tokio::test]
    async fn records_tenant_user_and_request_id() {
        let sink = Arc::new(Collect::default());
        let mut req = axum::http::Request::get("/Patient/p1")
            .header(REQUEST_ID_HEADER, "abc-123")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo::<SocketAddr>("192.0.2.4:5000".parse().unwrap()));
        let resp = call(app(Arc::clone(&sink)), req).await;
        assert_eq!(resp.headers()[REQUEST_ID_HEADER], "abc-123");
        let events = sink.json();
        assert_eq!(events.len(), 1);
        let (tenant, e) = &events[0];
        assert_eq!(tenant.as_deref(), Some("t1"));
        assert_eq!(e["meta"]["profile"][0], crate::balp::profiles::PATIENT_READ);
        assert_eq!(e["agent"][0]["network"]["address"], "192.0.2.4");
        assert_eq!(e["agent"][1]["network"]["address"], "http://fhir.test/fhir");
        assert_eq!(e["agent"][2]["who"]["identifier"]["value"], "alice");
        assert_eq!(e["source"]["site"], "fhir.test");
        let rid = e["entity"]
            .as_array()
            .unwrap()
            .iter()
            .find(|en| en["type"]["code"] == "XrequestId")
            .unwrap();
        assert_eq!(rid["what"]["identifier"]["value"], "abc-123");
    }

    #[tokio::test]
    async fn patient_from_response_context() {
        let sink = Arc::new(Collect::default());
        call(
            app(Arc::clone(&sink)),
            axum::http::Request::get("/Observation/o1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let (_, e) = &sink.json()[0];
        assert_eq!(e["meta"]["profile"][0], crate::balp::profiles::PATIENT_READ);
        assert_eq!(e["entity"][1]["what"]["reference"], "Patient/p7");
    }

    #[tokio::test]
    async fn form_search_is_recorded_and_still_reaches_the_handler() {
        let sink = Arc::new(Collect::default());
        let resp = call(
            app(Arc::clone(&sink)),
            axum::http::Request::post("/Patient/_search")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::CONTENT_LENGTH, "8")
                .body(Body::from("_id=p3x1"))
                .unwrap(),
        )
        .await;
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"_id=p3x1");
        let (_, e) = &sink.json()[0];
        assert_eq!(
            e["meta"]["profile"][0],
            crate::balp::profiles::PATIENT_QUERY
        );
        assert!(
            e["entity"][0]["description"]
                .as_str()
                .unwrap()
                .ends_with("\n\n_id=p3x1")
        );
        assert_eq!(e["entity"][1]["what"]["reference"], "Patient/p3x1");
    }

    #[tokio::test]
    async fn failures_record_the_operation_outcome_text() {
        let sink = Arc::new(Collect::default());
        let resp = call(
            app(Arc::clone(&sink)),
            axum::http::Request::get("/Basic/x")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), 404);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("is not known"));
        let (_, e) = &sink.json()[0];
        assert_eq!(e["outcome"], "4");
        assert_eq!(e["outcomeDesc"], "HTTP 404 Not Found: Basic/x is not known");
        assert!(e.get("meta").is_none());
    }

    #[tokio::test]
    async fn bundles_are_recorded_per_entry() {
        let sink = Arc::new(Collect::default());
        call(
            app(Arc::clone(&sink)),
            axum::http::Request::post("/")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
        let events = sink.json();
        assert_eq!(events.len(), 1, "a successful bundle: entries only");
        assert_eq!(events[0].1["subtype"][0]["code"], "create");
        assert_eq!(events[0].1["entity"][0]["what"]["reference"], "Patient/new");
    }

    #[tokio::test]
    async fn excluded_paths_are_not_recorded() {
        let sink = Arc::new(Collect::default());
        call(
            app(Arc::clone(&sink)),
            axum::http::Request::get("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert!(sink.json().is_empty());
    }

    #[test]
    fn tenant_prefix_is_stripped() {
        let c = Captured {
            tenant: Some("acme".into()),
            tenant_from_url: true,
            ..Default::default()
        };
        assert_eq!(strip_tenant("/acme/Patient/1", &c, true), "/Patient/1");
        let none = Captured::default();
        assert_eq!(strip_tenant("/acme/Patient", &none, true), "/Patient");
        assert_eq!(strip_tenant("/Patient/1", &none, true), "/Patient/1");
        assert_eq!(strip_tenant("/acme/Patient", &none, false), "/acme/Patient");
    }

    #[test]
    fn endpoint_prefix_is_stripped() {
        let c = Captured {
            tenant: Some("lab".into()),
            tenant_from_url: true,
            endpoint: Some("lab".into()),
            endpoint_prefix: Some("/laboratory".into()),
            ..Default::default()
        };
        assert_eq!(
            strip_tenant("/laboratory/Patient/1", &c, false),
            "/Patient/1"
        );
        assert_eq!(strip_tenant("/laboratory", &c, false), "/");
        assert_eq!(
            strip_tenant("/laboratoryx/Patient", &c, false),
            "/laboratoryx/Patient"
        );
        let ext = endpoint_extension("lab").expect("extension");
        let v = serde_json::to_value(&ext).unwrap();
        assert_eq!(v["url"], crate::context::ENDPOINT_EXTENSION_URL);
        assert_eq!(v["valueString"], "lab");
    }

    #[test]
    fn read_requests_are_told_apart() {
        assert!(is_read_request("GET", "/CodeSystem/x"));
        assert!(is_read_request("POST", "/ValueSet/$expand"));
        assert!(is_read_request("POST", "/ValueSet/vs1/$validate-code"));
        assert!(is_read_request("POST", "/Patient/_search"));
        assert!(!is_read_request("POST", "/CodeSystem"));
        assert!(!is_read_request("POST", "/import"));
        assert!(
            !is_read_request("POST", "/$closure"),
            "$closure keeps state"
        );
        assert!(!is_read_request("DELETE", "/ValueSet/x"));
    }

    #[test]
    fn location_names_the_created_resource() {
        assert_eq!(
            resource_from_location("http://h/fhir/CodeSystem/cs-1/_history/1"),
            Some(("CodeSystem".into(), "cs-1".into()))
        );
        assert_eq!(
            resource_from_location("/ValueSet/vs9"),
            Some(("ValueSet".into(), "vs9".into()))
        );
        assert_eq!(resource_from_location("http://h/fhir/metadata"), None);
    }

    #[tokio::test]
    async fn reads_can_be_left_unaudited() {
        let sink = Arc::new(Collect::default());
        let config = AuditConfig {
            reads: false,
            ..AuditConfig::default()
        };
        let state = Arc::new(AuditMiddlewareState::new(sink.clone(), config));
        let app = Router::new()
            .route("/CodeSystem/{id}", get(|| async { "{}" }))
            .route(
                "/CodeSystem",
                post(|| async {
                    (
                        axum::http::StatusCode::CREATED,
                        [(header::LOCATION, "http://t/CodeSystem/new-cs/_history/1")],
                        "{}",
                    )
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                state,
                audit_middleware,
            ));
        call(
            app.clone(),
            axum::http::Request::get("/CodeSystem/a")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert!(sink.json().is_empty(), "a read is not recorded");
        call(
            app,
            axum::http::Request::post("/CodeSystem")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
        let events = sink.json();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].1["meta"]["profile"][0],
            crate::balp::profiles::CREATE
        );
        assert_eq!(
            events[0].1["entity"][0]["what"]["reference"],
            "CodeSystem/new-cs"
        );
    }

    #[test]
    fn request_ids_are_sanitized() {
        assert_eq!(sanitize_request_id("ab c\n<x>-1"), "abcx-1");
        assert_eq!(sanitize_request_id(&"a".repeat(500)).len(), 128);
    }
}

#[cfg(test)]
#[path = "middleware_hm_tests.rs"]
mod hm_tests;
