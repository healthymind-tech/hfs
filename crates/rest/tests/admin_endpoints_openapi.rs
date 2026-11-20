//! `docs/api/admin-endpoints.yaml` must describe exactly the routes the
//! endpoint admin router serves:
//!
//! 1. the spec's `(method, path)` set equals `routing::admin_endpoints::ROUTES`;
//! 2. the router answers every spec operation (no router-level `404`/`405`);
//! 3. every other method on a spec path is `405` (nothing undocumented).
//!
//! The YAML is read with a small indentation scanner (path keys at two
//! spaces under `paths:`, operations at four), so no YAML dependency is
//! needed.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use helios_endpoints::{EndpointRegistry, MemoryEndpointStore, ensure_default_endpoint};
use helios_rest::handlers::admin_endpoints::AdminEndpointsState;
use helios_rest::handlers::admin_endpoints::authz::AdminCredentialAuthorizer;
use helios_rest::routing::admin_endpoints::{ROUTES, routes};
use tower::ServiceExt;

const METHODS: &[&str] = &["get", "put", "post", "patch", "delete", "head", "options"];

fn spec_text() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/api/admin-endpoints.yaml");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// `(METHOD, path)` operations under `paths:`.
fn spec_operations(text: &str) -> BTreeSet<(String, String)> {
    let mut ops = BTreeSet::new();
    let mut in_paths = false;
    let mut current: Option<String> = None;
    for line in text.lines() {
        if !line.starts_with(' ') && !line.trim().is_empty() {
            in_paths = line.trim_end() == "paths:";
            current = None;
            continue;
        }
        if !in_paths {
            continue;
        }
        if let Some(rest) = line.strip_prefix("  ")
            && !rest.starts_with(' ')
            && rest.trim_end().ends_with(':')
        {
            current = Some(rest.trim_end().trim_end_matches(':').to_string());
            continue;
        }
        if let (Some(path), Some(rest)) = (&current, line.strip_prefix("    "))
            && !rest.starts_with(' ')
        {
            let key = rest.trim_end().trim_end_matches(':');
            if METHODS.contains(&key) {
                ops.insert((key.to_ascii_uppercase(), path.clone()));
            }
        }
    }
    ops
}

#[test]
fn spec_lists_exactly_the_router_routes() {
    let spec = spec_operations(&spec_text());
    assert!(!spec.is_empty(), "no operations found in the spec");
    let code: BTreeSet<(String, String)> = ROUTES
        .iter()
        .map(|(m, p)| (m.to_string(), p.to_string()))
        .collect();
    let only_spec: Vec<_> = spec.difference(&code).collect();
    let only_code: Vec<_> = code.difference(&spec).collect();
    assert!(
        only_spec.is_empty() && only_code.is_empty(),
        "OpenAPI and router differ.\n  only in docs/api/admin-endpoints.yaml: {only_spec:?}\n  only in ROUTES: {only_code:?}"
    );
}

fn concrete(path: &str) -> String {
    path.replace("{name}", "default")
        .replace("{package}", "acme.base")
        .replace("{version}", "1.0.0")
}

async fn app() -> axum::Router {
    let store = Arc::new(MemoryEndpointStore::new());
    ensure_default_endpoint(store.as_ref(), "default", helios_endpoints::FhirRelease::R4)
        .await
        .unwrap();
    let registry = Arc::new(EndpointRegistry::load(store.as_ref()).await.unwrap());
    let state = AdminEndpointsState::builder(store, registry)
        .authorizer(Arc::new(
            AdminCredentialAuthorizer::new(None).allow_anonymous(true),
        ))
        .build();
    routes(state)
}

async fn call(app: &axum::Router, method: &str, path: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn router_answers_every_spec_operation() {
    let app = app().await;
    for (method, path) in spec_operations(&spec_text()) {
        let (status, body) = call(&app, &method, &concrete(&path)).await;
        assert_ne!(status, StatusCode::METHOD_NOT_ALLOWED, "{method} {path}");
        // A handler's 404 is an OperationOutcome; the router's own is empty.
        assert!(
            status != StatusCode::NOT_FOUND || body.contains("OperationOutcome"),
            "{method} {path}: router has no such route"
        );
    }
}

#[tokio::test]
async fn undocumented_methods_are_not_served() {
    let app = app().await;
    let spec = spec_operations(&spec_text());
    let paths: BTreeSet<&String> = spec.iter().map(|(_, p)| p).collect();
    for path in paths {
        for m in ["GET", "PUT", "POST", "PATCH", "DELETE"] {
            if spec.contains(&(m.to_string(), path.clone())) {
                continue;
            }
            let (status, _) = call(&app, m, &concrete(path)).await;
            assert_eq!(
                status,
                StatusCode::METHOD_NOT_ALLOWED,
                "{m} {path} is served but not documented"
            );
        }
    }
}

#[test]
fn scanner_reads_nested_operations() {
    let text = "openapi: 3.1.0\npaths:\n  /a:\n    parameters: []\n    get:\n      responses: {}\n    post:\n      x: 1\n  /b/{id}:\n    delete:\n      y: 2\ncomponents:\n  schemas:\n    get:\n      z: 3\n";
    let ops = spec_operations(text);
    let want: BTreeSet<(String, String)> = [("GET", "/a"), ("POST", "/a"), ("DELETE", "/b/{id}")]
        .iter()
        .map(|(m, p)| (m.to_string(), p.to_string()))
        .collect();
    assert_eq!(ops, want);
}
