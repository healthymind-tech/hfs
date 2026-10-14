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
            .http
            .post(format!("{}/import", self.base))
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
            .http
            .get(format!("{}/{resource_type}", self.base))
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

    /// Deletes one resource by id. A `404`/`410` counts as already gone.
    pub async fn delete(&self, resource_type: &str, id: &str) -> Result<(), String> {
        let response = self
            .http
            .delete(format!("{}/{resource_type}/{id}", self.base))
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
