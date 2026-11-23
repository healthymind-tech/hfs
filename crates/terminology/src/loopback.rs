//! In-process terminology over the embedded HTS router (design §4.2, phase 1).
//!
//! [`LoopbackTerminology`] drives any `tower::Service<http::Request<Full<Bytes>>>`
//! — in production the `axum::Router` returned by `helios_hts::server::create_app`
//! (terminology routes only, no UI, no `/metrics`) — with `oneshot`. No socket,
//! no TCP, no nginx, no connection pool, no second TLS/HTTP parse; the same
//! handlers, caches and response bodies as the `/tx` API, so remote and
//! embedded mode answer byte for byte the same.
//!
//! The caller's [`TxPrincipal`] travels as a request **extension** (never a
//! header), so nothing on the network can claim it; the embedded write guard
//! reads it (`helios_hts::write_guard`, package U2).

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use tower::{Service, ServiceExt};

use crate::service::{ServiceKind, TerminologyService, TxError, TxRequest, TxResponse};

/// Upper bound for a response body read into memory (an `$expand` of a
/// whole large code system is refused by HTS long before this).
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024 * 1024;

/// Embedded terminology service over a tower service.
#[derive(Clone)]
pub struct LoopbackTerminology<S> {
    service: S,
    default_timeout: Duration,
}

impl<S> LoopbackTerminology<S> {
    pub fn new(service: S, default_timeout: Duration) -> Self {
        Self {
            service,
            default_timeout,
        }
    }
}

/// Build the `http::Request` the embedded router sees.
pub fn to_http_request(request: &TxRequest) -> Result<http::Request<Full<Bytes>>, TxError> {
    let mut builder = http::Request::builder()
        .method(request.method.as_http())
        .uri(request.target())
        .header(http::header::ACCEPT, "application/fhir+json");
    if request.body.is_some() {
        builder = builder.header(http::header::CONTENT_TYPE, "application/fhir+json");
    }
    if let Some(lang) = &request.accept_language {
        builder = builder.header(http::header::ACCEPT_LANGUAGE, lang);
    }
    let mut req = builder
        .body(Full::new(request.body.clone().unwrap_or_default()))
        .map_err(|e| TxError::Unavailable(format!("bad terminology request: {e}")))?;
    req.extensions_mut().insert(request.principal.clone());
    Ok(req)
}

#[async_trait]
impl<S, B> TerminologyService for LoopbackTerminology<S>
where
    S: Service<http::Request<Full<Bytes>>, Response = http::Response<B>>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send + 'static,
    S::Error: std::fmt::Display + Send,
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    fn kind(&self) -> ServiceKind {
        ServiceKind::Embedded
    }

    async fn call(&self, request: TxRequest) -> Result<TxResponse, TxError> {
        let timeout = request.timeout.unwrap_or(self.default_timeout);
        let http_request = to_http_request(&request)?;
        let service = self.service.clone();
        let fut = async move {
            let response = service
                .oneshot(http_request)
                .await
                .map_err(|e| TxError::Unavailable(e.to_string()))?;
            let (parts, body) = response.into_parts();
            let limited = http_body_util::Limited::new(body, MAX_RESPONSE_BYTES);
            let bytes = limited
                .collect()
                .await
                .map_err(|e| TxError::Unavailable(format!("reading terminology body: {e}")))?
                .to_bytes();
            Ok(TxResponse {
                status: parts.status.as_u16(),
                content_type: parts
                    .headers
                    .get(http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string),
                body: bytes,
            })
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(result) => result,
            Err(_) => Err(TxError::Timeout(timeout)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{ExpandParams, TxPrincipal, ValidateCodeParams};
    use std::convert::Infallible;

    fn fake_hts() -> impl Service<
        http::Request<Full<Bytes>>,
        Response = http::Response<Full<Bytes>>,
        Error = Infallible,
        Future = impl Send,
    > + Clone
    + Send
    + Sync
    + 'static {
        tower::service_fn(|req: http::Request<Full<Bytes>>| async move {
            let principal = req.extensions().get::<TxPrincipal>().cloned();
            let path = req.uri().path().to_string();
            let method = req.method().clone();
            let body = req.into_body().collect().await.unwrap().to_bytes();
            let (status, json) = match (method.as_str(), path.as_str()) {
                ("POST", "/ValueSet/$expand") => {
                    let p: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    let url = p["parameter"][0]["valueUri"].as_str().unwrap().to_string();
                    (
                        200,
                        serde_json::json!({"resourceType":"ValueSet","url":url,
                            "expansion":{"contains":[{"system":"http://loinc.org","code":"1-8"}]}}),
                    )
                }
                ("POST", "/ValueSet/$validate-code") => (
                    200,
                    serde_json::json!({"resourceType":"Parameters","parameter":[
                        {"name":"result","valueBoolean":true},
                        {"name":"display","valueString":"Acyclovir"}]}),
                ),
                ("POST", "/import") => {
                    if principal == Some(TxPrincipal::System) {
                        (
                            200,
                            serde_json::json!({"code_systems":2,"value_sets":3,"concept_maps":0}),
                        )
                    } else {
                        (
                            401,
                            serde_json::json!({"resourceType":"OperationOutcome",
                                "issue":[{"severity":"error","code":"login","diagnostics":"admin required"}]}),
                        )
                    }
                }
                ("GET", "/health") => (200, serde_json::json!({"status":"ok"})),
                _ => (
                    404,
                    serde_json::json!({"resourceType":"OperationOutcome",
                    "issue":[{"severity":"error","code":"not-found","diagnostics":"no route"}]}),
                ),
            };
            Ok::<_, Infallible>(
                http::Response::builder()
                    .status(status)
                    .header("content-type", "application/fhir+json")
                    .body(Full::new(Bytes::from(json.to_string())))
                    .unwrap(),
            )
        })
    }

    #[tokio::test]
    async fn typed_helpers_round_trip_through_the_router() {
        let tx = LoopbackTerminology::new(fake_hts(), Duration::from_secs(5));
        assert_eq!(tx.kind(), ServiceKind::Embedded);
        let vs = tx
            .expand(&ExpandParams {
                url: "http://example.org/vs".into(),
                count: Some(10),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(vs["url"], "http://example.org/vs");
        let outcome = tx
            .validate_code(&ValidateCodeParams {
                value_set: Some("http://example.org/vs".into()),
                system: "http://loinc.org".into(),
                code: "1-8".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(outcome.result);
        assert_eq!(outcome.display.as_deref(), Some("Acyclovir"));
        assert_eq!(tx.health().await.state, crate::service::HealthState::Ok);
    }

    #[tokio::test]
    async fn principal_travels_as_extension_and_gates_writes() {
        let tx = LoopbackTerminology::new(fake_hts(), Duration::from_secs(5));
        let report = tx
            .import_bundle(&serde_json::json!({"resourceType":"Bundle","type":"collection"}))
            .await
            .unwrap();
        assert_eq!((report.code_systems, report.value_sets), (2, 3));
        // The same write without the System principal is refused, and the
        // OperationOutcome text becomes the error message.
        let anon = TxRequest::post_json("import", &serde_json::json!({}));
        let err = tx.call(anon).await.unwrap().into_json().unwrap_err();
        assert_eq!(
            err,
            TxError::Status {
                status: 401,
                message: "admin required".into()
            }
        );
    }

    #[test]
    fn request_target_and_query_encoding() {
        let r = TxRequest::get("/CodeSystem")
            .with_query_param("url", "http://snomed.info/sct")
            .with_query_param("version", "a b");
        assert_eq!(
            r.target(),
            "/CodeSystem?url=http%3A%2F%2Fsnomed.info%2Fsct&version=a+b"
        );
        let http_req = to_http_request(&r.as_principal(TxPrincipal::System)).unwrap();
        assert_eq!(
            http_req.extensions().get::<TxPrincipal>(),
            Some(&TxPrincipal::System)
        );
        assert!(http_req.headers().get("x-principal").is_none());
    }
}
