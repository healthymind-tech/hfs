//! Per-endpoint CORS (design §5.2, "CORS").
//!
//! With the manager on, this middleware takes the place of the global
//! `CorsLayer` in `build_app`. It must sit where the global layer sat — outside
//! the dispatcher — because a CORS layer answers preflights itself without
//! calling inner services, so it has to know the endpoint up front: it
//! resolves the request path on the registry's current snapshot and
//!
//! * for the console / admin / metrics surface (`/ui`, `/admin`, `/console`,
//!   `/metrics`) and for an endpoint whose `cors` section is inherited, runs
//!   the global layer (`HFS_CORS_*`), exactly as before — or nothing when
//!   `HFS_ENABLE_CORS=false`;
//! * for an endpoint with its own `cors` section, runs a layer built from it
//!   (`enabled=false` → no CORS headers at all). Built layers are cached per
//!   `(endpoint id, revision)` and dropped when the snapshot moves on.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{
    extract::{Request, State},
    http::{HeaderName, HeaderValue, Method},
    middleware::Next,
    response::Response,
};
use helios_endpoints::{CorsSettings, EndpointRegistry, Resolution};
use tower::{Layer, ServiceExt};
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, Any, CorsLayer};

/// First path segments that are never FHIR endpoints' CORS business.
const NON_FHIR_PREFIXES: &[&str] = &["ui", "admin", "console", "metrics"];

/// State of [`endpoint_cors`].
#[derive(Clone)]
pub struct EndpointCors {
    registry: Arc<EndpointRegistry>,
    global: Option<CorsLayer>,
    env_methods: String,
    env_headers: String,
    cache: Arc<Mutex<CorsCache>>,
}

#[derive(Default)]
struct CorsCache {
    epoch: u64,
    layers: HashMap<(String, u64), Option<CorsLayer>>,
}

impl EndpointCors {
    /// `global` is the `HFS_CORS_*` layer (`None` when `HFS_ENABLE_CORS` is
    /// off); `env_methods` / `env_headers` are `HFS_CORS_METHODS` /
    /// `HFS_CORS_HEADERS`, which endpoint sections extend rather than replace.
    pub fn new(
        registry: Arc<EndpointRegistry>,
        global: Option<CorsLayer>,
        env_methods: impl Into<String>,
        env_headers: impl Into<String>,
    ) -> Self {
        Self {
            registry,
            global,
            env_methods: env_methods.into(),
            env_headers: env_headers.into(),
            cache: Arc::new(Mutex::new(CorsCache::default())),
        }
    }

    /// The layer for `path`: `Ok(layer)` (possibly none) decided by an
    /// endpoint section, `Err(())` to use the global layer.
    fn layer_for(&self, path: &str) -> Result<Option<CorsLayer>, ()> {
        let first = path.trim_start_matches('/').split('/').next().unwrap_or("");
        if NON_FHIR_PREFIXES.contains(&first) {
            return Err(());
        }
        let snapshot = self.registry.snapshot();
        let endpoint = match snapshot.resolve(path) {
            Resolution::Serve(r) | Resolution::Unavailable(r) => r.endpoint,
            Resolution::NotFound => return Err(()),
        };
        let Some(settings) = endpoint.spec.cors.as_ref() else {
            return Err(());
        };
        let key = (endpoint.id.clone(), endpoint.revision);
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        if cache.epoch != snapshot.epoch() {
            cache.layers.clear();
            cache.epoch = snapshot.epoch();
        }
        Ok(cache
            .layers
            .entry(key)
            .or_insert_with(|| build_layer(settings, &self.env_methods, &self.env_headers))
            .clone())
    }
}

/// Builds a CORS layer from an endpoint section. `None` when disabled.
pub fn build_layer(
    settings: &CorsSettings,
    env_methods: &str,
    env_headers: &str,
) -> Option<CorsLayer> {
    if !settings.enabled {
        return None;
    }
    let mut cors = CorsLayer::new();
    let wildcard = settings.allowed_origins.iter().any(|o| o == "*");
    cors = if wildcard {
        cors.allow_origin(Any)
    } else {
        let origins: Vec<HeaderValue> = settings
            .allowed_origins
            .iter()
            .filter_map(|o| o.parse().ok())
            .collect();
        cors.allow_origin(AllowOrigin::list(origins))
    };
    // tower-http refuses `*` methods/headers together with credentials, so a
    // credentialed endpoint mirrors the request's own instead.
    let credentials = settings.allow_credentials && !wildcard;
    cors = if env_methods.trim() == "*" {
        if credentials {
            cors.allow_methods(AllowMethods::mirror_request())
        } else {
            cors.allow_methods(Any)
        }
    } else {
        let methods: Vec<Method> = env_methods
            .split(',')
            .filter_map(|m| m.trim().parse().ok())
            .collect();
        cors.allow_methods(methods)
    };
    cors = if env_headers.trim() == "*" {
        if credentials {
            cors.allow_headers(AllowHeaders::mirror_request())
        } else {
            cors.allow_headers(Any)
        }
    } else {
        let headers: Vec<HeaderName> = env_headers
            .split(',')
            .chain(settings.extra_headers.iter().map(String::as_str))
            .filter_map(|h| h.trim().parse().ok())
            .collect();
        cors.allow_headers(headers)
    };
    if credentials {
        cors = cors.allow_credentials(true);
    }
    Some(cors)
}

/// The CORS middleware (see the module docs).
pub async fn endpoint_cors(
    State(state): State<EndpointCors>,
    request: Request,
    next: Next,
) -> Response {
    let chosen = match state.layer_for(request.uri().path()) {
        Ok(layer) => layer,
        Err(()) => state.global.clone(),
    };
    match chosen {
        Some(layer) => match layer.layer(next).oneshot(request).await {
            Ok(response) => response,
            Err(never) => match never {},
        },
        None => next.run(request).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, routing::get};
    use helios_endpoints::{
        Actor, EndpointName, EndpointSpec, EndpointStore, FhirRelease, MemoryEndpointStore,
        ensure_default_endpoint,
    };

    async fn app() -> Router {
        let store = MemoryEndpointStore::new();
        ensure_default_endpoint(&store, "default", FhirRelease::R4)
            .await
            .unwrap();
        let mut a = EndpointSpec::named(EndpointName::parse("app").unwrap(), "App");
        a.cors = Some(CorsSettings {
            enabled: true,
            allowed_origins: vec!["https://app.example".into()],
            allow_credentials: true,
            extra_headers: vec!["x-extra".into()],
        });
        store.create(a, &Actor::bootstrap()).await.unwrap();
        let mut closed = EndpointSpec::named(EndpointName::parse("closed").unwrap(), "Closed");
        closed.cors = Some(CorsSettings {
            enabled: false,
            ..Default::default()
        });
        store.create(closed, &Actor::bootstrap()).await.unwrap();
        let reg = Arc::new(EndpointRegistry::load(&store).await.unwrap());
        let state = EndpointCors::new(
            reg,
            Some(CorsLayer::new().allow_origin(Any).allow_methods(Any)),
            "*",
            "*",
        );
        Router::new()
            .fallback(get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state, endpoint_cors))
    }

    async fn origin_header(app: &Router, path: &str, origin: &str) -> Option<String> {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("origin", origin)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        res.headers()
            .get("access-control-allow-origin")
            .map(|v| v.to_str().unwrap().to_string())
    }

    #[tokio::test]
    async fn per_endpoint_policies() {
        let app = app().await;
        // Inherited (default endpoint) and console paths use the global layer.
        assert_eq!(
            origin_header(&app, "/Patient", "https://x")
                .await
                .as_deref(),
            Some("*")
        );
        assert_eq!(
            origin_header(&app, "/ui/x", "https://x").await.as_deref(),
            Some("*")
        );
        // Own section: listed origin only, credentials.
        assert_eq!(
            origin_header(&app, "/app/Patient", "https://app.example")
                .await
                .as_deref(),
            Some("https://app.example")
        );
        assert_eq!(
            origin_header(&app, "/app/Patient", "https://evil.example").await,
            None
        );
        // Disabled.
        assert_eq!(
            origin_header(&app, "/closed/Patient", "https://x").await,
            None
        );
    }

    #[tokio::test]
    async fn preflight_is_answered_per_endpoint() {
        let app = app().await;
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/app/Patient")
                    .header("origin", "https://app.example")
                    .header("access-control-request-method", "POST")
                    .header("access-control-request-headers", "x-extra")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.headers()
                .get("access-control-allow-credentials")
                .unwrap(),
            "true"
        );
        assert_eq!(
            res.headers().get("access-control-allow-methods").unwrap(),
            "POST"
        );
    }

    #[test]
    fn explicit_lists_and_disabled() {
        let s = CorsSettings {
            enabled: true,
            allowed_origins: vec!["*".into()],
            allow_credentials: false,
            extra_headers: vec!["x-a".into()],
        };
        assert!(build_layer(&s, "GET,POST", "content-type").is_some());
        let off = CorsSettings {
            enabled: false,
            ..s
        };
        assert!(build_layer(&off, "*", "*").is_none());
    }
}
