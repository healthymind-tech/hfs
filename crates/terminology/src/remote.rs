//! External terminology server over HTTP (design §4.3) — today's
//! `HFS_TERMINOLOGY_SERVER` behaviour behind the [`TerminologyService`] trait.
//!
//! The wire client is abstracted as [`HttpTransport`] so this crate stays free
//! of reqwest/TLS feature choices: helios-rest (package U2) implements it over
//! its existing `reqwest::Client` (2 s connect timeout, SSRF-checked through
//! `OutboundPolicy` for console-entered URLs). The token rules live here, so
//! every caller gets the same ones:
//!
//! * [`TxPrincipal::System`] → `Authorization: Bearer {HFS_TERMINOLOGY_SERVER_TOKEN}`;
//! * any other principal → no credential (a console user's session is never
//!   forwarded to a foreign server).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::service::{
    ServiceKind, TerminologyService, TxError, TxPrincipal, TxRequest, TxResponse,
};

/// A fully resolved outbound request.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboundRequest {
    pub method: http::Method,
    /// Absolute URL.
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<bytes::Bytes>,
    pub timeout: Duration,
}

/// The HTTP client seam.
#[async_trait]
pub trait HttpTransport: Send + Sync + 'static {
    async fn send(&self, request: OutboundRequest) -> Result<TxResponse, TxError>;
}

/// Remote terminology service.
pub struct RemoteTerminology {
    base: String,
    token: Option<String>,
    default_timeout: Duration,
    transport: Arc<dyn HttpTransport>,
}

impl RemoteTerminology {
    /// `base` is the terminology FHIR base (`http://hts:8090`,
    /// `https://tx.fhir.org/r4`); a trailing slash is ignored.
    pub fn new(
        base: impl Into<String>,
        token: Option<String>,
        default_timeout: Duration,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, TxError> {
        let base = base.into().trim_end_matches('/').to_string();
        let parsed = url::Url::parse(&base)
            .map_err(|e| TxError::Unavailable(format!("terminology base {base:?}: {e}")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(TxError::Unavailable(format!(
                "terminology base {base:?} must be http(s)"
            )));
        }
        Ok(Self {
            base,
            token: token.filter(|t| !t.is_empty()),
            default_timeout,
            transport,
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// The outbound request for `request` (exposed for tests and for the
    /// console's "test connection" button).
    pub fn outbound(&self, request: &TxRequest) -> OutboundRequest {
        let mut url = format!("{}/{}", self.base, request.path);
        if !request.query.is_empty() {
            url.push('?');
            url.push_str(&request.query);
        }
        let mut headers = vec![("accept".to_string(), "application/fhir+json".to_string())];
        if request.body.is_some() {
            headers.push((
                "content-type".to_string(),
                "application/fhir+json".to_string(),
            ));
        }
        if let Some(lang) = &request.accept_language {
            headers.push(("accept-language".to_string(), lang.clone()));
        }
        if request.principal == TxPrincipal::System
            && let Some(token) = &self.token
        {
            headers.push(("authorization".to_string(), format!("Bearer {token}")));
        }
        OutboundRequest {
            method: request.method.as_http(),
            url,
            headers,
            body: request.body.clone(),
            timeout: request.timeout.unwrap_or(self.default_timeout),
        }
    }
}

#[async_trait]
impl TerminologyService for RemoteTerminology {
    fn kind(&self) -> ServiceKind {
        ServiceKind::Remote
    }

    async fn call(&self, request: TxRequest) -> Result<TxResponse, TxError> {
        self.transport.send(self.outbound(&request)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<OutboundRequest>>);

    #[async_trait]
    impl HttpTransport for Recorder {
        async fn send(&self, request: OutboundRequest) -> Result<TxResponse, TxError> {
            self.0.lock().unwrap().push(request);
            Ok(TxResponse {
                status: 200,
                content_type: None,
                body: bytes::Bytes::from_static(br#"{"status":"ok"}"#),
            })
        }
    }

    fn remote(rec: Arc<Recorder>) -> RemoteTerminology {
        RemoteTerminology::new(
            "http://hts:8090/",
            Some("s3cret".into()),
            Duration::from_secs(30),
            rec,
        )
        .unwrap()
    }

    #[test]
    fn token_only_for_system_principal() {
        let r = remote(Arc::new(Recorder::default()));
        let sys = r.outbound(&TxRequest::get("import").as_principal(TxPrincipal::System));
        assert!(
            sys.headers
                .contains(&("authorization".into(), "Bearer s3cret".into()))
        );
        let console = r.outbound(
            &TxRequest::get("import").as_principal(TxPrincipal::Console {
                user: "alice".into(),
                role: helios_endpoints::ConsoleRole::Admin,
            }),
        );
        assert!(console.headers.iter().all(|(k, _)| k != "authorization"));
        assert_eq!(sys.url, "http://hts:8090/import");
    }

    #[tokio::test]
    async fn health_uses_short_timeout() {
        let rec = Arc::new(Recorder::default());
        let r = remote(rec.clone());
        assert_eq!(r.health().await.state, crate::service::HealthState::Ok);
        let sent = rec.0.lock().unwrap();
        assert_eq!(sent[0].url, "http://hts:8090/health");
        assert_eq!(sent[0].timeout, Duration::from_secs(3));
    }

    #[test]
    fn rejects_non_http_base() {
        let err = RemoteTerminology::new(
            "file:///etc/passwd",
            None,
            Duration::from_secs(1),
            Arc::new(Recorder::default()),
        );
        assert!(err.is_err());
    }
}
