//! Endpoint scope of a console request (design D-16).
//!
//! The endpoint is in the URL — `/ui/e/{endpoint}/…` — never a browser
//! preference, so deep links and several tabs on different endpoints work.
//! [`dispatch`] runs in front of the whole UI router:
//!
//! * `/ui/e/{ep}/data[/rest]` → rewritten to the unchanged data page
//!   `/ui[/rest]` (`/ui/dashboard` for the bare data root) with an
//!   [`EndpointScope`] extension;
//! * `/ui/e/{ep}[/page]` → the manager page, with the scope extension;
//! * any other `/ui/…` request a page of an endpoint makes (htmx fragments,
//!   form posts, script fetches) inherits that endpoint's scope from
//!   `HX-Current-URL` / `Referer`;
//! * a top-level navigation to an old data address (`/ui/resources?…`)
//!   answers `302` to `/ui/e/{ep}/data/resources?…`, `ep` being the endpoint
//!   the visitor came from, else the default endpoint.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use helios_endpoints::Endpoint;

use super::{EndpointRouting, ManagerRuntime};

/// The endpoint a console request works on.
#[derive(Debug, Clone)]
pub struct EndpointScope {
    /// The endpoint, as of the routing snapshot the request saw.
    pub endpoint: Arc<Endpoint>,
    /// How its FHIR API is addressed.
    pub routing: EndpointRouting,
    /// Whether the address named the endpoint explicitly (vs. inherited from
    /// the referring page or defaulted).
    pub explicit: bool,
}

impl EndpointScope {
    /// The endpoint name.
    pub fn name(&self) -> &str {
        self.endpoint.name()
    }

    /// `/ui/e/{name}{sub}`.
    pub fn href(&self, sub: &str) -> String {
        format!("/ui/e/{}{}", self.name(), sub)
    }

    /// Browser-facing FHIR API path of this endpoint, given the server's
    /// (`HFS_BASE_URL` path, `""` or `/fhir`).
    pub fn fhir_base(&self, server_base_path: &str) -> String {
        match self.routing {
            EndpointRouting::Dispatcher => format!("{server_base_path}{}", self.endpoint.prefix()),
            EndpointRouting::TenantHeader => server_base_path.to_string(),
        }
    }

    /// Prefix the UI's loopback self-calls put in front of FHIR paths.
    pub fn self_call_prefix(&self) -> String {
        match self.routing {
            EndpointRouting::Dispatcher => self.endpoint.prefix(),
            EndpointRouting::TenantHeader => String::new(),
        }
    }
}

/// UI paths that are never endpoint-scoped (and never redirected).
const UNSCOPED: &[&str] = &[
    "/ui/assets",
    "/ui/hts",
    "/ui/e",
    "/ui/endpoints",
    "/ui/about",
    "/ui/login",
    "/ui/logout",
    "/ui/callback",
    "/ui/admin",
    "/ui/account",
    "/ui/settings",
    "/ui/sync",
    "/ui/idp",
    "/ui/version",
    "/ui/tenant",
    "/ui/tenants",
    "/ui/json-view",
    "/ui/status",
];

/// Query keys that belong to the per-endpoint dashboard, not the overview.
const DASHBOARD_KEYS: &[&str] = &[
    "types", "type", "window", "all", "focus", "ctx", "retry", "notices", "open", "state",
];

fn under(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// `(endpoint, rest)` of a `/ui/e/{endpoint}[/rest]` path; `rest` keeps its
/// leading slash (`""` for the endpoint home).
pub(crate) fn split_endpoint_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/ui/e/")?;
    let (name, rest) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    (!name.is_empty()).then_some((name, rest))
}

/// The endpoint the referring console page was scoped to.
fn referring_endpoint(headers: &HeaderMap) -> Option<String> {
    for name in ["hx-current-url", header::REFERER.as_str()] {
        let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) else {
            continue;
        };
        // Absolute URL or path: only the path matters.
        let path = match value.find("://") {
            Some(i) => {
                let after = &value[i + 3..];
                after.find('/').map(|j| &after[j..]).unwrap_or("/")
            }
            None => value,
        };
        let path = path.split(['?', '#']).next().unwrap_or("");
        if let Some((name, _)) = split_endpoint_path(path) {
            return Some(name.to_string());
        }
    }
    None
}

/// Whether the request is a top-level page navigation (not htmx, not a
/// script fetch, not a form post).
fn is_navigation(method: &Method, headers: &HeaderMap) -> bool {
    if method != Method::GET || headers.contains_key("hx-request") {
        return false;
    }
    match headers.get("sec-fetch-mode").and_then(|v| v.to_str().ok()) {
        Some(mode) => mode == "navigate",
        None => headers
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|a| a.contains("text/html")),
    }
}

fn with_query(path: &str, uri: &Uri) -> String {
    match uri.query() {
        Some(q) if !q.is_empty() => format!("{path}?{q}"),
        _ => path.to_string(),
    }
}

fn rewrite(request: &mut Request, path: &str) {
    let target = with_query(path, request.uri());
    if let Ok(uri) = target.parse::<Uri>() {
        *request.uri_mut() = uri;
    }
}

/// What [`dispatch`] decided for a request (pure, for tests).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Leave the request alone.
    Pass,
    /// Scope to `endpoint` (explicit when named in the URL), optionally
    /// rewriting the path.
    Scope {
        endpoint: String,
        explicit: bool,
        rewrite_to: Option<String>,
    },
    /// Redirect to this location.
    Redirect(String),
}

pub(crate) fn decide(
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    exists: impl Fn(&str) -> bool,
    default_endpoint: &str,
) -> Decision {
    let path = uri.path();
    if !under(path, "/ui") {
        return Decision::Pass;
    }
    if let Some((name, rest)) = split_endpoint_path(path) {
        if !exists(name) {
            // The manager's not-found page answers it.
            return Decision::Pass;
        }
        if let Some(data) = rest.strip_prefix("/data")
            && (data.is_empty() || data.starts_with('/'))
        {
            let inner = if data.is_empty() || data == "/" {
                "/ui/dashboard".to_string()
            } else {
                format!("/ui{data}")
            };
            return Decision::Scope {
                endpoint: name.to_string(),
                explicit: true,
                rewrite_to: Some(inner),
            };
        }
        return Decision::Scope {
            endpoint: name.to_string(),
            explicit: true,
            rewrite_to: None,
        };
    }
    if UNSCOPED.iter().any(|p| under(path, p)) {
        return Decision::Pass;
    }
    let referred = referring_endpoint(headers).filter(|n| exists(n));
    if path == "/ui" {
        // The overview, unless this is the dashboard of an endpoint talking
        // to itself (its chart picker and live refresh address `/ui?…`).
        let dashboard_query = uri.query().is_some_and(|q| {
            form_urlencoded::parse(q.as_bytes()).any(|(k, _)| DASHBOARD_KEYS.contains(&k.as_ref()))
        });
        if !dashboard_query {
            return Decision::Pass;
        }
        let endpoint = referred.unwrap_or_else(|| default_endpoint.to_string());
        if is_navigation(method, headers) {
            return Decision::Redirect(with_query(&format!("/ui/e/{endpoint}/data"), uri));
        }
        return Decision::Scope {
            endpoint,
            explicit: false,
            rewrite_to: Some("/ui/dashboard".into()),
        };
    }
    let endpoint = referred.unwrap_or_else(|| default_endpoint.to_string());
    if is_navigation(method, headers) {
        let rest = &path["/ui".len()..];
        return Decision::Redirect(with_query(&format!("/ui/e/{endpoint}/data{rest}"), uri));
    }
    Decision::Scope {
        endpoint,
        explicit: false,
        rewrite_to: None,
    }
}

/// Runs `inner` with the request scoped per [`decide`].
pub(crate) async fn dispatch(
    runtime: Arc<ManagerRuntime>,
    inner: axum::Router,
    mut request: Request<Body>,
) -> Response {
    let snapshot = runtime.registry.snapshot();
    let default_name = snapshot
        .default_endpoint()
        .map(|e| e.name().to_string())
        .unwrap_or_else(|| helios_endpoints::DEFAULT_ENDPOINT_NAME.to_string());
    let decision = decide(
        request.method(),
        request.uri(),
        request.headers(),
        |name| snapshot.by_name(name).is_some(),
        &default_name,
    );
    match decision {
        Decision::Pass => {}
        Decision::Redirect(location) => return Redirect::to(&location).into_response(),
        Decision::Scope {
            endpoint,
            explicit,
            rewrite_to,
        } => {
            if let Some(found) = snapshot.by_name(&endpoint) {
                request.extensions_mut().insert(EndpointScope {
                    endpoint: found.clone(),
                    routing: runtime.routing,
                    explicit,
                });
            }
            if let Some(path) = rewrite_to {
                rewrite(&mut request, &path);
            }
        }
    }
    match tower::ServiceExt::oneshot(inner, request).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn decide_for(method: Method, uri: &str, headers: &[(&str, &str)]) -> Decision {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            map.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        decide(
            &method,
            &uri.parse().unwrap(),
            &map,
            |n| matches!(n, "default" | "twcore"),
            "default",
        )
    }

    const NAV: &[(&str, &str)] = &[("sec-fetch-mode", "navigate")];

    #[test]
    fn data_pages_are_rewritten_under_their_endpoint() {
        assert_eq!(
            decide_for(Method::GET, "/ui/e/twcore/data/resources?type=Patient", &[]),
            Decision::Scope {
                endpoint: "twcore".into(),
                explicit: true,
                rewrite_to: Some("/ui/resources".into())
            }
        );
        assert_eq!(
            decide_for(Method::GET, "/ui/e/twcore/data", &[]),
            Decision::Scope {
                endpoint: "twcore".into(),
                explicit: true,
                rewrite_to: Some("/ui/dashboard".into())
            }
        );
        assert_eq!(
            decide_for(Method::GET, "/ui/e/twcore/config", &[]),
            Decision::Scope {
                endpoint: "twcore".into(),
                explicit: true,
                rewrite_to: None
            }
        );
        // `/ui/e/twcore/database` is not `/data`.
        assert!(matches!(
            decide_for(Method::GET, "/ui/e/twcore/database", &[]),
            Decision::Scope {
                rewrite_to: None,
                ..
            }
        ));
        // Unknown endpoint: left to the not-found page.
        assert_eq!(
            decide_for(Method::GET, "/ui/e/nope/data", &[]),
            Decision::Pass
        );
    }

    #[test]
    fn legacy_pages_redirect_to_the_referring_or_default_endpoint() {
        assert_eq!(
            decide_for(Method::GET, "/ui/resources?type=Patient", NAV),
            Decision::Redirect("/ui/e/default/data/resources?type=Patient".into())
        );
        let from_twcore = [
            ("sec-fetch-mode", "navigate"),
            ("referer", "https://h.example/ui/e/twcore/data/resources"),
        ];
        assert_eq!(
            decide_for(Method::GET, "/ui/editor?type=Patient&id=1", &from_twcore),
            Decision::Redirect("/ui/e/twcore/data/editor?type=Patient&id=1".into())
        );
        // An unknown referring endpoint falls back to the default.
        let from_gone = [
            ("sec-fetch-mode", "navigate"),
            ("referer", "/ui/e/gone/config"),
        ];
        assert_eq!(
            decide_for(Method::GET, "/ui/batch", &from_gone),
            Decision::Redirect("/ui/e/default/data/batch".into())
        );
    }

    #[test]
    fn fragments_and_posts_inherit_scope_without_redirect() {
        assert_eq!(
            decide_for(
                Method::GET,
                "/ui/resources/params?type=Patient",
                &[
                    ("hx-request", "true"),
                    ("hx-current-url", "http://x/ui/e/twcore/data/resources")
                ]
            ),
            Decision::Scope {
                endpoint: "twcore".into(),
                explicit: false,
                rewrite_to: None
            }
        );
        assert_eq!(
            decide_for(
                Method::POST,
                "/ui/history/diff",
                &[("referer", "/ui/e/twcore/data/history")]
            ),
            Decision::Scope {
                endpoint: "twcore".into(),
                explicit: false,
                rewrite_to: None
            }
        );
    }

    #[test]
    fn overview_assets_and_server_pages_pass() {
        for path in [
            "/ui",
            "/ui?q=lab",
            "/ui/assets/app.css",
            "/ui/about",
            "/ui/endpoints/new",
            "/ui/tenants",
            "/ui/login",
            "/fhir/Patient",
            "/Patient",
            "/uix",
        ] {
            assert_eq!(decide_for(Method::GET, path, NAV), Decision::Pass, "{path}");
        }
    }

    #[test]
    fn dashboard_queries_on_the_root_reach_the_endpoint_dashboard() {
        assert_eq!(
            decide_for(
                Method::GET,
                "/ui?types=Patient&window=1h",
                &[
                    ("hx-request", "true"),
                    ("hx-current-url", "/ui/e/twcore/data")
                ]
            ),
            Decision::Scope {
                endpoint: "twcore".into(),
                explicit: false,
                rewrite_to: Some("/ui/dashboard".into())
            }
        );
        assert_eq!(
            decide_for(Method::GET, "/ui?window=1h", NAV),
            Decision::Redirect("/ui/e/default/data?window=1h".into())
        );
    }

    #[test]
    fn scope_urls_follow_routing() {
        let endpoint = Endpoint {
            id: "1".into(),
            revision: 1,
            spec: helios_endpoints::EndpointSpec::named(
                helios_endpoints::EndpointName::parse("twcore").unwrap(),
                "TW",
            ),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            created_by: None,
            updated_by: None,
        };
        let mut scope = EndpointScope {
            endpoint: Arc::new(endpoint),
            routing: EndpointRouting::Dispatcher,
            explicit: true,
        };
        assert_eq!(scope.href("/config"), "/ui/e/twcore/config");
        assert_eq!(scope.fhir_base("/fhir"), "/fhir/twcore");
        assert_eq!(scope.self_call_prefix(), "/twcore");
        scope.routing = EndpointRouting::TenantHeader;
        assert_eq!(scope.fhir_base("/fhir"), "/fhir");
        assert_eq!(scope.self_call_prefix(), "");
    }
}
