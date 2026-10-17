//! The package manager's calls to the terminology server
//! (`HFS_TERMINOLOGY_SERVER`): `POST /import` for CodeSystem / ValueSet /
//! ConceptMap, `GET /{type}?url=&version=` to see what is already loaded,
//! and `DELETE /{type}/{id}` on uninstall.
//!
//! HTS keys terminology by `(url, version)` and may store a resource under
//! a different id than the package's, so terminology is always addressed by
//! canonical here, never by id.

use std::time::Duration;

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
}

/// HTS client.
#[derive(Clone)]
pub struct HtsClient {
    base: String,
    http: reqwest::Client,
    /// `Authorization: Bearer` for the terminology server
    /// (`HFS_TERMINOLOGY_SERVER_TOKEN`), marked sensitive.
    authorization: Option<reqwest::header::HeaderValue>,
}

impl HtsClient {
    /// A client for `base` (no trailing slash needed).
    pub fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .use_rustls_tls()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(900))
                .build()
                .unwrap_or_default(),
            authorization: None,
        }
    }

    /// The same client, sending `token` as `Authorization: Bearer` on every
    /// call — HTS requires its administrator credential for `/import` and
    /// deletes (`HTS_REQUIRE_AUTH_FOR_WRITES`).
    pub fn with_token(mut self, token: Option<&helios_admin_auth::Secret>) -> Self {
        self.authorization = token.and_then(|t| {
            let mut value =
                reqwest::header::HeaderValue::from_str(&format!("Bearer {}", t.expose())).ok()?;
            value.set_sensitive(true);
            Some(value)
        });
        self
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.authorization {
            Some(value) => request.header(reqwest::header::AUTHORIZATION, value.clone()),
            None => request,
        }
    }

    /// The base URL.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Imports resources as one collection Bundle.
    pub async fn import(&self, resources: &[Value]) -> Result<ImportOutcome, String> {
        let bundle = json!({
            "resourceType": "Bundle",
            "type": "collection",
            "entry": resources.iter().map(|r| json!({"resource": r})).collect::<Vec<_>>(),
        });
        let body = serde_json::to_vec(&bundle).map_err(|e| e.to_string())?;
        let response = self
            .authorized(self.http.post(format!("{}/import", self.base)))
            .header("Content-Type", "application/fhir+json")
            .header("Accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| format!("terminology server unreachable: {e}"))?;
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
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
            errors: doc
                .get("errors")
                .and_then(Value::as_array)
                .map(|e| {
                    e.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
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
        let mut request = self
            .authorized(self.http.get(format!("{}/{resource_type}", self.base)))
            .header("Accept", "application/fhir+json")
            .timeout(Duration::from_secs(60))
            // The summary carries id, url and version without the concept
            // list: 14 ms instead of 0.7 s (3.4 MB) for THO's v3-ActCode.
            .query(&[("url", url), ("_summary", "true")]);
        if let Some(v) = version {
            request = request.query(&[("version", v)]);
        }
        let response = request
            .send()
            .await
            .map_err(|e| format!("terminology server unreachable: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("terminology search HTTP {}", response.status()));
        }
        let bundle: Value = response.json().await.map_err(|e| e.to_string())?;
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
                .http
                .get(format!("{}/{resource_type}", self.base))
                .header("Accept", "application/fhir+json")
                .timeout(Duration::from_secs(120))
                .query(&[
                    ("_summary", "true"),
                    ("_count", &PAGE.to_string()),
                    ("_offset", &offset),
                ])
                .send()
                .await
                .map_err(|e| format!("terminology server unreachable: {e}"))?;
            if !response.status().is_success() {
                return Err(format!("terminology search HTTP {}", response.status()));
            }
            let bundle: Value = response.json().await.map_err(|e| e.to_string())?;
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
            .authorized(
                self.http
                    .delete(format!("{}/{resource_type}/{id}", self.base)),
            )
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .map_err(|e| format!("terminology server unreachable: {e}"))?;
        match response.status().as_u16() {
            200..=299 | 404 | 410 => Ok(()),
            status => Err(format!("terminology delete HTTP {status}")),
        }
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
