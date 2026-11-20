//! The endpoint dispatcher (design §5.1, D-5).
//!
//! Installed in front of the FHIR stack (authentication, authorization,
//! tenant-prefix rewrite, FHIR router) the way `create_url_tenant_routes`
//! installs its rewrite: the stack becomes the fallback service of a router
//! whose middleware rewrites the request before the inner router matches it.
//! Explicit routes (`/ui`, `/admin`, `/console`, `/metrics`, `/health`) are
//! merged next to that fallback and match first, so the dispatcher never sees
//! them; reserved segments keep endpoints from shadowing them.
//!
//! Per request: `registry.snapshot().resolve(path)` →
//!
//! * `Serve(r)` — for a named endpoint the URI is rewritten to
//!   `r.remaining_path` (+ query) and `OriginalPath` + `ExtractedTenantFromUrl`
//!   are inserted, so existing tenant-prefix code sees a URL-routed tenant;
//!   for every endpoint the [`ResolvedEndpoint`] and the [`RequestSnapshot`]
//!   are inserted and the endpoint is made the task-local
//!   [`current`](super::current) one for the rest of the request.
//! * `Unavailable(r)` — `503` OperationOutcome (`transient`), with
//!   `Retry-After` while the endpoint is `provisioning`.
//! * `NotFound` — `404` OperationOutcome (no default endpoint).
//!
//! Implemented as a tower [`Layer`]/[`Service`] ([`DispatchLayer`]).
//!
//! The snapshot is taken once, so a request finishes on the configuration it
//! started with even if a reload swaps the registry meanwhile.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::{
    Router,
    body::Body,
    extract::Request,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use helios_endpoints::{EndpointRegistry, EndpointStatus, Resolution, ResolvedEndpoint};
use tower::{Layer, Service};

use super::RequestSnapshot;
use crate::middleware::tenant_prefix::{ExtractedTenantFromUrl, OriginalPath};

/// Seconds a client is told to wait for a `provisioning` endpoint.
pub const PROVISIONING_RETRY_AFTER_SECS: u32 = 30;

/// Wraps `fhir_stack` in the dispatcher reading `registry`.
///
/// The stack's only route is its fallback (the FHIR router behind the
/// tenant-prefix rewrite, wrapped by the auth layers), so a layer on it wraps
/// that fallback and its rewrite happens before the inner router matches — no
/// extra `Router` level per request.
pub fn install(fhir_stack: Router, registry: Arc<EndpointRegistry>) -> Router {
    fhir_stack.layer(DispatchLayer { registry })
}

/// Tower layer of the dispatcher. A hand-written service rather than
/// `middleware::from_fn`: the request is prepared synchronously in `call` and
/// the inner future is boxed once, so the dispatcher adds a single small
/// frame to the request's stack depth (debug builds run close to the 2 MiB
/// worker stack on heavy paths).
#[derive(Clone)]
pub struct DispatchLayer {
    registry: Arc<EndpointRegistry>,
}

impl<S> Layer<S> for DispatchLayer {
    type Service = Dispatch<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Dispatch {
            inner,
            registry: Arc::clone(&self.registry),
        }
    }
}

/// The dispatcher service (see the module docs).
#[derive(Clone)]
pub struct Dispatch<S> {
    inner: S,
    registry: Arc<EndpointRegistry>,
}

impl<S> Service<Request> for Dispatch<S>
where
    S: Service<Request, Response = Response, Error = Infallible> + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request) -> Self::Future {
        match prepare(&self.registry, &mut request) {
            Ok((resolved, name)) => {
                let inner = self.inner.call(request);
                let started = Instant::now();
                Box::pin(super::scope(resolved, async move {
                    let response = inner.await?;
                    record(&name, response.status(), started);
                    Ok(response)
                }))
            }
            Err(response) => Box::pin(std::future::ready(Ok(response))),
        }
    }
}

/// Resolves the request on the current snapshot. `Ok` = serve (the request
/// is rewritten and carries its extensions), `Err` = answer this instead.
fn prepare(
    registry: &EndpointRegistry,
    request: &mut Request,
) -> Result<(ResolvedEndpoint, String), Response> {
    let snapshot = registry.snapshot();
    let path = request.uri().path().to_string();
    match snapshot.resolve(&path) {
        Resolution::Serve(resolved) => {
            // The audit layer sits outside: tell it which endpoint (and which
            // prefix to strip before classifying the path).
            helios_audit::context::set_endpoint(resolved.endpoint.name(), &resolved.prefix);
            if !resolved.is_default() {
                rewrite_path(request, &resolved.remaining_path);
                request.extensions_mut().insert(OriginalPath(path));
                request.extensions_mut().insert(ExtractedTenantFromUrl(
                    resolved.endpoint.tenant_id().to_string(),
                ));
            }
            let name = resolved.endpoint.name().to_string();
            request.extensions_mut().insert(resolved.clone());
            request
                .extensions_mut()
                .insert(RequestSnapshot(Arc::clone(&snapshot)));
            Ok((resolved, name))
        }
        Resolution::Unavailable(resolved) => {
            helios_audit::context::set_endpoint(resolved.endpoint.name(), &resolved.prefix);
            let response = unavailable(&resolved);
            record(resolved.endpoint.name(), response.status(), Instant::now());
            Err(response)
        }
        Resolution::NotFound => Err(not_found(&path)),
    }
}

/// Per-endpoint request metrics. A separate series family from
/// `http_requests_total` (whose label set stays unchanged): bounded by the
/// endpoint cap (200) times five status classes.
fn record(endpoint: &str, status: StatusCode, started: Instant) {
    let class = match status.as_u16() {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    };
    metrics::counter!(
        "hfs_endpoint_requests_total",
        "endpoint" => endpoint.to_string(),
        "status_class" => class,
    )
    .increment(1);
    metrics::histogram!(
        "hfs_endpoint_request_duration_seconds",
        "endpoint" => endpoint.to_string(),
    )
    .record(started.elapsed().as_secs_f64());
}

/// Replaces the request path, keeping the query string.
pub(crate) fn rewrite_path(request: &mut Request, new_path: &str) {
    let original = request.uri().clone();
    let mut parts = original.clone().into_parts();
    let pq = match original.query() {
        Some(q) => format!("{new_path}?{q}"),
        None => new_path.to_string(),
    };
    if let Ok(pq) = pq.parse() {
        parts.path_and_query = Some(pq);
        if let Ok(uri) = axum::http::Uri::from_parts(parts) {
            *request.uri_mut() = uri;
        }
    }
}

fn outcome(code: &str, diagnostics: &str) -> serde_json::Value {
    serde_json::json!({
        "resourceType": "OperationOutcome",
        "issue": [{
            "severity": "error",
            "code": code,
            "diagnostics": diagnostics,
        }]
    })
}

fn fhir_json(status: StatusCode, body: serde_json::Value) -> Response {
    let mut response = (status, Body::from(body.to_string())).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/fhir+json"),
    );
    response
}

/// `503` for a disabled or provisioning endpoint.
pub fn unavailable(resolved: &ResolvedEndpoint) -> Response {
    let name = resolved.endpoint.name();
    let status = resolved.endpoint.spec.status;
    let msg = match status {
        EndpointStatus::Provisioning => format!("endpoint {name} is being provisioned"),
        _ => format!("endpoint {name} is disabled"),
    };
    let mut response = fhir_json(StatusCode::SERVICE_UNAVAILABLE, outcome("transient", &msg));
    if status == EndpointStatus::Provisioning {
        response.headers_mut().insert(
            header::RETRY_AFTER,
            HeaderValue::from(PROVISIONING_RETRY_AFTER_SECS),
        );
    }
    response
}

/// `404` when no endpoint serves the path (no default endpoint).
pub fn not_found(path: &str) -> Response {
    fhir_json(
        StatusCode::NOT_FOUND,
        outcome(
            "not-found",
            &format!("no FHIR endpoint is served at {path}"),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use helios_endpoints::{
        Actor, EndpointName, EndpointSpec, EndpointStore, FhirRelease, MemoryEndpointStore,
        ensure_default_endpoint,
    };
    use tower::ServiceExt;

    async fn registry(with_default: bool) -> (Arc<EndpointRegistry>, Arc<MemoryEndpointStore>) {
        let store = Arc::new(MemoryEndpointStore::new());
        if with_default {
            ensure_default_endpoint(store.as_ref(), "default", FhirRelease::R4)
                .await
                .unwrap();
        }
        store
            .create(
                EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab"),
                &Actor::bootstrap(),
            )
            .await
            .unwrap();
        let mut off = EndpointSpec::named(EndpointName::parse("off").unwrap(), "Off");
        off.status = EndpointStatus::Disabled;
        store.create(off, &Actor::bootstrap()).await.unwrap();
        let mut prov = EndpointSpec::named(EndpointName::parse("prov").unwrap(), "Prov");
        prov.status = EndpointStatus::Provisioning;
        store.create(prov, &Actor::bootstrap()).await.unwrap();
        (
            Arc::new(EndpointRegistry::load(store.as_ref()).await.unwrap()),
            store,
        )
    }

    fn echo() -> Router {
        Router::new().fallback(get(|req: Request| async move {
            let ep = req
                .extensions()
                .get::<ResolvedEndpoint>()
                .map(|r| r.endpoint.name().to_string())
                .unwrap_or_default();
            let tenant = req
                .extensions()
                .get::<ExtractedTenantFromUrl>()
                .map(|t| t.0.clone())
                .unwrap_or_default();
            let current = super::super::current()
                .map(|r| r.prefix)
                .unwrap_or_else(|| "-".into());
            format!(
                "{ep}|{}|{tenant}|{current}|{}",
                req.uri(),
                req.extensions().get::<RequestSnapshot>().is_some()
            )
        }))
    }

    async fn call(app: &Router, uri: &str) -> (StatusCode, Option<String>, String) {
        let res = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let retry = res
            .headers()
            .get(header::RETRY_AFTER)
            .map(|v| v.to_str().unwrap().to_string());
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, retry, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn routes_named_and_default() {
        let (reg, _) = registry(true).await;
        let app = install(echo(), reg);
        let (s, _, b) = call(&app, "/lab/Patient/1?_elements=id").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b, "lab|/Patient/1?_elements=id|lab|/lab|true");
        let (_, _, b) = call(&app, "/lab").await;
        assert_eq!(b, "lab|/|lab|/lab|true");
        let (_, _, b) = call(&app, "/Patient/1").await;
        assert_eq!(b, "default|/Patient/1|||true");
    }

    #[tokio::test]
    async fn unavailable_and_not_found() {
        let (reg, _) = registry(true).await;
        let app = install(echo(), Arc::clone(&reg));
        let (s, retry, b) = call(&app, "/off/Patient").await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        assert!(retry.is_none());
        assert!(b.contains("\"transient\"") && b.contains("endpoint off is disabled"));
        let (s, retry, b) = call(&app, "/prov/metadata").await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(retry.as_deref(), Some("30"));
        assert!(b.contains("being provisioned"));

        let (reg, _) = registry(false).await;
        let app = install(echo(), reg);
        let (s, _, b) = call(&app, "/Patient").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert!(b.contains("not-found"));
        assert_eq!(call(&app, "/lab/Patient").await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn hot_reload_is_seen_by_the_next_request() {
        let (reg, store) = registry(true).await;
        let app = install(echo(), Arc::clone(&reg));
        assert!(
            call(&app, "/new/Patient")
                .await
                .2
                .starts_with("default|/new/Patient")
        );
        store
            .create(
                EndpointSpec::named(EndpointName::parse("new").unwrap(), "New"),
                &Actor::bootstrap(),
            )
            .await
            .unwrap();
        reg.reload_if_stale(store.as_ref()).await.unwrap();
        assert!(
            call(&app, "/new/Patient")
                .await
                .2
                .starts_with("new|/Patient")
        );
    }
}
