//! In-process upstream for the unified server (`HFS_TERMINOLOGY=embedded`,
//! docs/design/unified-server.md U2/U4).
//!
//! The terminology console normally reaches its terminology server over
//! HTTP ([`crate::UpstreamClient::new`]). Embedded in `hfs`, the same pages
//! run against the server's own [`TerminologyService`] (the loopback over
//! the embedded HTS router: same handlers, caches and write guard, no
//! socket) and the admin router that `hfs` mounts at `/admin/terminology`.
//! The request builders of [`crate::UpstreamClient`] are kept; only the
//! last hop changes, so every page, raw fold and error path is shared.
//!
//! Who acts: the host sets the signed-in console user for the duration of
//! a console request ([`scope_caller`]); terminology calls then carry
//! [`TxPrincipal::Console`] (the write guard and audit see the user) and
//! admin calls carry [`helios_admin_auth::AdminUser`]. Background work
//! (the metrics sampler) runs as [`TxPrincipal::System`].

use std::sync::Arc;

use axum::{body::Body, extract::Request, middleware::Next, response::Response};
use helios_endpoints::ConsoleRole;
use helios_terminology::TerminologyService;
use helios_terminology::service::{TxMethod, TxPrincipal, TxRequest};
use serde::de::DeserializeOwned;

use crate::upstream::UpstreamError;

/// Base URL of an in-process client: never dialled, it only gives the
/// request builders an absolute URL and the raw folds their public path
/// (`/tx/…`).
pub const IN_PROCESS_BASE: &str = "http://terminology.in-process/tx";

/// Upper bound for an in-process admin response body.
const MAX_ADMIN_BODY: usize = 256 * 1024 * 1024;

/// The server's terminology service and admin router.
#[derive(Clone)]
pub struct InProcessUpstream {
    service: Arc<dyn TerminologyService>,
    admin: Option<axum::Router>,
}

impl std::fmt::Debug for InProcessUpstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessUpstream")
            .field("kind", &self.service.kind())
            .field("admin", &self.admin.is_some())
            .finish()
    }
}

impl InProcessUpstream {
    /// `service`: the server's terminology service; `admin`: the
    /// terminology admin router (`/admin/terminology`), when enabled.
    pub fn new(service: Arc<dyn TerminologyService>, admin: Option<axum::Router>) -> Self {
        Self { service, admin }
    }

    /// The admin router, with the console user attached for its audit
    /// trail; `None` when the admin API is off.
    pub fn admin_router(&self) -> Option<axum::Router> {
        self.admin
            .clone()
            .map(|r| r.layer(axum::middleware::from_fn(attach_admin_user)))
    }

    /// Whether the management pages have an admin API behind them.
    pub fn has_admin(&self) -> bool {
        self.admin.is_some()
    }

    /// Runs one request built by [`crate::UpstreamClient`].
    pub(crate) async fn execute(
        &self,
        request: reqwest::Request,
        op: &'static str,
        url: &str,
    ) -> Result<UpResponse, UpstreamError> {
        let base_path = reqwest::Url::parse(IN_PROCESS_BASE)
            .map(|u| u.path().to_string())
            .unwrap_or_else(|_| "/tx".to_string());
        let path = request.url().path();
        let rest = path.strip_prefix(base_path.as_str()).unwrap_or(path);
        let query = request.url().query().unwrap_or("").to_string();
        let body = request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .map(bytes::Bytes::copy_from_slice);
        if let Some(admin_tail) = rest.strip_prefix("/admin") {
            return self
                .admin(&request, admin_tail, &query, body, op, url)
                .await;
        }
        let method = match *request.method() {
            reqwest::Method::GET | reqwest::Method::HEAD => TxMethod::Get,
            reqwest::Method::POST => TxMethod::Post,
            reqwest::Method::PUT => TxMethod::Put,
            reqwest::Method::DELETE => TxMethod::Delete,
            _ => {
                return Err(UpstreamError::Connect {
                    op,
                    url: url.to_owned(),
                    message: format!("method {} is not supported in-process", request.method()),
                });
            }
        };
        let mut tx = TxRequest::get(rest.trim_start_matches('/'));
        tx.method = method;
        tx.query = query;
        tx.body = body;
        tx.principal = current_principal();
        tx.timeout = request.timeout().copied();
        tx.accept_language = request
            .headers()
            .get(reqwest::header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        match self.service.call(tx).await {
            Ok(response) => Ok(UpResponse {
                status: reqwest::StatusCode::from_u16(response.status)
                    .unwrap_or(reqwest::StatusCode::BAD_GATEWAY),
                body: response.body,
            }),
            Err(helios_terminology::TxError::Timeout(d)) => Err(UpstreamError::Timeout {
                op,
                url: url.to_owned(),
                message: format!("in-process call timed out after {d:?}"),
            }),
            Err(e) => Err(UpstreamError::Connect {
                op,
                url: url.to_owned(),
                message: e.to_string(),
            }),
        }
    }

    async fn admin(
        &self,
        request: &reqwest::Request,
        tail: &str,
        query: &str,
        body: Option<bytes::Bytes>,
        op: &'static str,
        url: &str,
    ) -> Result<UpResponse, UpstreamError> {
        let Some(router) = self.admin_router() else {
            return Ok(UpResponse {
                status: reqwest::StatusCode::NOT_FOUND,
                body: bytes::Bytes::new(),
            });
        };
        let target = match (tail.is_empty(), query.is_empty()) {
            (true, true) => "/".to_string(),
            (true, false) => format!("/?{query}"),
            (false, true) => tail.to_string(),
            (false, false) => format!("{tail}?{query}"),
        };
        let mut builder = axum::http::Request::builder()
            .method(request.method().as_str())
            .uri(target);
        for (name, value) in request.headers() {
            builder = builder.header(name.as_str(), value.as_bytes());
        }
        let http_request = builder
            .body(Body::from(body.unwrap_or_default()))
            .map_err(|e| UpstreamError::Connect {
                op,
                url: url.to_owned(),
                message: e.to_string(),
            })?;
        let call = async move {
            let response = match tower::ServiceExt::oneshot(router, http_request).await {
                Ok(r) => r,
                Err(never) => match never {},
            };
            let status = response.status().as_u16();
            let bytes = axum::body::to_bytes(response.into_body(), MAX_ADMIN_BODY).await;
            (status, bytes)
        };
        let (status, bytes) = match request.timeout() {
            Some(d) => {
                tokio::time::timeout(*d, call)
                    .await
                    .map_err(|_| UpstreamError::Timeout {
                        op,
                        url: url.to_owned(),
                        message: format!("in-process admin call timed out after {d:?}"),
                    })?
            }
            None => call.await,
        };
        let body = bytes.map_err(|e| UpstreamError::Decode {
            op,
            url: url.to_owned(),
            message: e.to_string(),
        })?;
        Ok(UpResponse {
            status: reqwest::StatusCode::from_u16(status)
                .unwrap_or(reqwest::StatusCode::BAD_GATEWAY),
            body,
        })
    }
}

/// A fully read upstream answer, over HTTP or in-process. Mirrors the
/// parts of `reqwest::Response` the client uses.
pub(crate) struct UpResponse {
    pub(crate) status: reqwest::StatusCode,
    pub(crate) body: bytes::Bytes,
}

impl UpResponse {
    pub(crate) fn status(&self) -> reqwest::StatusCode {
        self.status
    }

    pub(crate) async fn json<T: DeserializeOwned>(self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }

    pub(crate) async fn text(self) -> Result<String, std::convert::Infallible> {
        Ok(String::from_utf8_lossy(&self.body).into_owned())
    }

    pub(crate) async fn bytes(self) -> Result<bytes::Bytes, std::convert::Infallible> {
        Ok(self.body)
    }
}

/// The signed-in console user a request runs for.
#[derive(Debug, Clone)]
struct Caller {
    user: String,
    role: ConsoleRole,
}

tokio::task_local! {
    static CALLER: Caller;
}

/// Runs `fut` (one console request) as `user` with terminology role
/// `role`: in-process terminology calls carry it as their principal.
pub async fn scope_caller<F: std::future::Future>(
    user: String,
    role: ConsoleRole,
    fut: F,
) -> F::Output {
    CALLER.scope(Caller { user, role }, fut).await
}

fn current_principal() -> TxPrincipal {
    CALLER
        .try_with(|c| TxPrincipal::Console {
            user: c.user.clone(),
            role: c.role,
        })
        .unwrap_or(TxPrincipal::System)
}

async fn attach_admin_user(mut request: Request, next: Next) -> Response {
    if let Ok(user) = CALLER.try_with(|c| c.user.clone()) {
        request
            .extensions_mut()
            .insert(helios_admin_auth::AdminUser(user));
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UpstreamClient;
    use helios_terminology::{ServiceKind, TxError, TxResponse};
    use std::sync::Mutex;

    /// Records what reached the service and answers a fixed body.
    struct Recorder {
        seen: Mutex<Vec<TxRequest>>,
    }

    #[async_trait::async_trait]
    impl TerminologyService for Recorder {
        fn kind(&self) -> ServiceKind {
            ServiceKind::Embedded
        }
        async fn call(&self, request: TxRequest) -> Result<TxResponse, TxError> {
            let path = request.path.clone();
            self.seen.lock().unwrap().push(request);
            let body = match path.as_str() {
                "metrics" => "hts_requests_total 3\n".to_string(),
                _ => r#"{"status":"ok"}"#.to_string(),
            };
            Ok(TxResponse {
                status: 200,
                content_type: Some("application/json".into()),
                body: bytes::Bytes::from(body),
            })
        }
    }

    fn client(admin: Option<axum::Router>) -> (UpstreamClient, Arc<Recorder>) {
        let rec = Arc::new(Recorder {
            seen: Mutex::new(Vec::new()),
        });
        let service: Arc<dyn TerminologyService> = rec.clone();
        let c = UpstreamClient::in_process(InProcessUpstream::new(service, admin)).unwrap();
        (c, rec)
    }

    #[tokio::test]
    async fn reads_go_to_the_service_without_a_socket() {
        let (c, rec) = client(None);
        assert!(c.is_in_process());
        assert_eq!(c.metrics_text().await.unwrap(), "hts_requests_total 3\n");
        let seen = rec.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].path, "metrics");
        assert_eq!(seen[0].method, TxMethod::Get);
        // Background work (no console request in scope) acts as the server.
        assert_eq!(seen[0].principal, TxPrincipal::System);
    }

    #[tokio::test]
    async fn console_requests_carry_the_signed_in_user() {
        let (c, rec) = client(None);
        scope_caller("local:ana".into(), ConsoleRole::Editor, async {
            c.metrics_text().await.unwrap();
        })
        .await;
        let seen = rec.seen.lock().unwrap();
        assert_eq!(
            seen[0].principal,
            TxPrincipal::Console {
                user: "local:ana".into(),
                role: ConsoleRole::Editor
            }
        );
    }

    #[tokio::test]
    async fn admin_calls_reach_the_admin_router_with_the_user() {
        use axum::{Extension, Json, routing::get};
        let admin = axum::Router::new().route(
            "/jobs",
            get(
                |user: Option<Extension<helios_admin_auth::AdminUser>>,
                 q: axum::extract::RawQuery| async move {
                    Json(serde_json::json!({
                        "user": user.map(|u| u.0 .0.clone()),
                        "query": q.0,
                    }))
                },
            ),
        );
        let (c, rec) = client(Some(admin));
        let (status, body) = scope_caller("local:root".into(), ConsoleRole::Admin, async {
            c.admin_call(
                reqwest::Method::GET,
                "/jobs?limit=5",
                None,
                std::time::Duration::from_secs(5),
                &[],
            )
            .await
            .unwrap()
        })
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["user"], "local:root");
        assert_eq!(body["query"], "limit=5");
        assert!(
            rec.seen.lock().unwrap().is_empty(),
            "admin calls bypass the tx service"
        );
    }

    #[tokio::test]
    async fn admin_calls_without_an_admin_router_answer_404() {
        let (c, _) = client(None);
        let (status, _) = c
            .admin_call(
                reqwest::Method::GET,
                "/jobs",
                None,
                std::time::Duration::from_secs(5),
                &[],
            )
            .await
            .unwrap();
        assert_eq!(status, 404);
    }
}
