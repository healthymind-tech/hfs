//! The package manager's calls to the terminology server
//! (`HFS_TERMINOLOGY_SERVER`): `POST /import` for CodeSystem / ValueSet /
//! ConceptMap, `GET /{type}?url=&version=` to see what is already loaded,
//! and `DELETE /{type}/{id}` on uninstall.
//!
//! HTS keys terminology by `(url, version)` and may store a resource under
//! a different id than the package's, so terminology is always addressed by
//! canonical here, never by id.

use std::sync::Arc;
use std::time::Duration;

use helios_terminology::{ServiceKind, TerminologyService, TxPrincipal, TxRequest, TxResponse};

use serde_json::{Value, json};

/// Result of one `/import` call.
#[derive(Debug, Clone, Default)]
pub struct ImportOutcome {
    /// HTTP status.
    pub status: u16,
    /// Resources the server reports imported.
    pub imported: u64,
    /// Non-fatal errors the server reported (207).
    pub errors: Vec<String>,
    /// Resources the server deliberately did not write (e.g. a truncated
    /// copy of a complete CodeSystem it already has).
    pub warnings: Vec<String>,
}

/// The string array `doc[key]` (empty when absent).
fn strings(doc: &Value, key: &str) -> Vec<String> {
    doc.get(key)
        .and_then(Value::as_array)
        .map(|e| {
            e.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The package manager's terminology client: the server's terminology
/// service (embedded: in-process, no token) or a remote server over HTTP
/// (`HFS_TERMINOLOGY_SERVER`, with `HFS_TERMINOLOGY_SERVER_TOKEN`).
/// Writes and the `find` lookups run as the `System` principal: the
/// embedded write guard trusts it, the remote service turns it into the
/// bearer token, as before.
#[derive(Clone)]
pub struct HtsClient {
    base: String,
    service: Arc<dyn TerminologyService>,
}

impl HtsClient {
    /// A client for the remote server at `base` (no trailing slash needed).
    pub fn new(base: &str) -> Self {
        let base = base.trim_end_matches('/').to_string();
        Self {
            service: crate::terminology::remote_service(&base, None),
            base,
        }
    }

    /// A client over an existing service; `label` is what [`Self::base`] reports.
    pub fn from_service(label: impl Into<String>, service: Arc<dyn TerminologyService>) -> Self {
        Self {
            base: label.into(),
            service,
        }
    }

    /// The same remote client, sending `token` as `Authorization: Bearer`
    /// on its `System` calls — HTS requires its administrator credential for
    /// `/import` and deletes (`HTS_REQUIRE_AUTH_FOR_WRITES`). A client over
    /// the embedded service ignores it (no token is needed in-process).
    pub fn with_token(mut self, token: Option<&helios_admin_auth::Secret>) -> Self {
        if self.service.kind() == ServiceKind::Remote {
            self.service = crate::terminology::remote_service(
                &self.base,
                token.map(|t| t.expose().to_string()),
            );
        }
        self
    }

    /// The base URL (or `embedded`).
    pub fn base(&self) -> &str {
        &self.base
    }

    async fn send(&self, request: TxRequest) -> Result<TxResponse, String> {
        self.service.call(request).await.map_err(|e| {
            format!(
                "terminology server unreachable: {}",
                crate::terminology::tx_error_text(&e)
            )
        })
    }

    /// Imports resources as one collection Bundle.
    pub async fn import(&self, resources: &[Value]) -> Result<ImportOutcome, String> {
        let bundle = json!({
            "resourceType": "Bundle",
            "type": "collection",
            "entry": resources.iter().map(|r| json!({"resource": r})).collect::<Vec<_>>(),
        });
        let response = self
            .send(
                TxRequest::post_json("import", &bundle)
                    .as_principal(TxPrincipal::System)
                    .with_timeout(Duration::from_secs(900)),
            )
            .await?;
        let status = response.status;
        let text = String::from_utf8_lossy(&response.body);
        let doc: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !(200..300).contains(&status) {
            let detail = doc
                .pointer("/issue/0/diagnostics")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| text.chars().take(300).collect());
            return Err(format!("terminology import HTTP {status}: {detail}"));
        }
        let count = |k: &str| doc.get(k).and_then(Value::as_u64).unwrap_or(0);
        Ok(ImportOutcome {
            status,
            imported: count("code_systems") + count("value_sets") + count("concept_maps"),
            errors: strings(&doc, "errors"),
            warnings: strings(&doc, "warnings"),
        })
    }

    /// Ids of stored `resource_type` resources with this canonical (and
    /// business version, when given).
    pub async fn find(
        &self,
        resource_type: &str,
        url: &str,
        version: Option<&str>,
    ) -> Result<Vec<String>, String> {
        // The summary carries id, url and version without the concept
        // list: 14 ms instead of 0.7 s (3.4 MB) for THO's v3-ActCode.
        let mut request = TxRequest::get(resource_type)
            .with_query_param("url", url)
            .with_query_param("_summary", "true");
        if let Some(v) = version {
            request = request.with_query_param("version", v);
        }
        let response = self
            .send(
                request
                    .as_principal(TxPrincipal::System)
                    .with_timeout(Duration::from_secs(60)),
            )
            .await?;
        if !response.is_success() {
            return Err(format!(
                "terminology search HTTP {}",
                status_text(response.status)
            ));
        }
        let bundle: Value = serde_json::from_slice(&response.body).map_err(|e| e.to_string())?;
        Ok(bundle
            .get("entry")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|e| {
                        // HTS matches `version` exactly; double-check the
                        // url too in case a server treats it as a prefix.
                        e.pointer("/resource/url").and_then(Value::as_str) == Some(url)
                            && version.is_none_or(|v| {
                                e.pointer("/resource/version").and_then(Value::as_str) == Some(v)
                            })
                    })
                    .filter_map(|e| e.pointer("/resource/id").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// `(url, version)` of every stored `resource_type` resource, read in
    /// pages of summaries (no concept lists).
    pub async fn list_canonicals(
        &self,
        resource_type: &str,
    ) -> Result<Vec<(String, Option<String>)>, String> {
        const PAGE: usize = 1000;
        let mut out = Vec::new();
        let mut first_of_first_page: Option<Value> = None;
        for page in 0..10_000 {
            let offset = (page * PAGE).to_string();
            let response = self
                .send(
                    TxRequest::get(resource_type)
                        .with_query_param("_summary", "true")
                        .with_query_param("_count", &PAGE.to_string())
                        .with_query_param("_offset", &offset)
                        .with_timeout(Duration::from_secs(120)),
                )
                .await?;
            if !response.is_success() {
                return Err(format!(
                    "terminology search HTTP {}",
                    status_text(response.status)
                ));
            }
            let bundle: Value =
                serde_json::from_slice(&response.body).map_err(|e| e.to_string())?;
            let entries = bundle
                .get("entry")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let n = entries.len();
            // A server that ignores `_offset` repeats the first page.
            let first = entries.first().map(|e| e.pointer("/resource/id").cloned());
            if page == 0 {
                first_of_first_page = first.flatten();
            } else if first.flatten() == first_of_first_page {
                break;
            }
            out.extend(entries.iter().filter_map(|e| {
                let url = e.pointer("/resource/url").and_then(Value::as_str)?;
                let version = e
                    .pointer("/resource/version")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Some((url.to_string(), version))
            }));
            // A last (short) page; a server that ignores `_count` returns
            // everything at once.
            if n != PAGE {
                break;
            }
        }
        Ok(out)
    }

    /// Deletes one resource by id. A `404`/`410` counts as already gone.
    pub async fn delete(&self, resource_type: &str, id: &str) -> Result<(), String> {
        let response = self
            .send(
                TxRequest::delete(format!("{resource_type}/{id}"))
                    .as_principal(TxPrincipal::System)
                    .with_timeout(Duration::from_secs(120)),
            )
            .await?;
        match response.status {
            200..=299 | 404 | 410 => Ok(()),
            status => Err(format!("terminology delete HTTP {status}")),
        }
    }
}

/// `404 Not Found` style text, as reqwest's `StatusCode` printed it.
fn status_text(status: u16) -> String {
    match reqwest::StatusCode::from_u16(status) {
        Ok(s) => s.to_string(),
        Err(_) => status.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{delete, post};
    use axum::{Json, Router};

    /// A terminology server that, like HTS with `HTS_REQUIRE_AUTH_FOR_WRITES`,
    /// refuses writes without its administrator credential.
    async fn guarded_server() -> String {
        fn authorized(headers: &HeaderMap) -> bool {
            headers.get("authorization").and_then(|v| v.to_str().ok())
                == Some("Bearer hts-admin-test-token")
        }
        let app = Router::new()
            .route(
                "/import",
                post(|headers: HeaderMap| async move {
                    if authorized(&headers) {
                        (StatusCode::OK, Json(json!({"code_systems": 1})))
                    } else {
                        (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"resourceType": "OperationOutcome", "issue": [
                                {"severity": "error", "code": "login",
                                 "diagnostics": "administrator sign-in required"}]})),
                        )
                    }
                }),
            )
            .route(
                "/CodeSystem/{id}",
                delete(|headers: HeaderMap| async move {
                    if authorized(&headers) {
                        StatusCode::NO_CONTENT
                    } else {
                        StatusCode::UNAUTHORIZED
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }

    #[tokio::test]
    async fn writes_carry_the_terminology_server_token() {
        let base = guarded_server().await;
        let cs = vec![json!({"resourceType": "CodeSystem", "url": "http://x/cs"})];

        let anonymous = HtsClient::new(&base);
        let err = anonymous.import(&cs).await.unwrap_err();
        assert!(err.contains("401"), "{err}");
        assert!(anonymous.delete("CodeSystem", "x").await.is_err());

        let token = helios_admin_auth::Secret::new("hts-admin-test-token");
        let client = HtsClient::new(&base).with_token(Some(&token));
        assert_eq!(client.import(&cs).await.unwrap().imported, 1);
        client.delete("CodeSystem", "x").await.unwrap();
    }
}
