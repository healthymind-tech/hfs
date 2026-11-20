//! An external FHIR server as a sync peer (design §9.2 "External source" /
//! "External target"): paged search reads following `next` links,
//! `_history?_since` incremental reads, Bulk `$export` initial loads, and
//! batch-Bundle writes with a fallback to individual `PUT`/`DELETE`.
//! Everything goes through [`FhirClient`] (SSRF policy, credentials,
//! rate limit, retries).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use url::Url;

use crate::change::{Change, valid_id, valid_type};
use crate::conflict::TargetInfo;
use crate::hash::content_hash;
use crate::http::{FhirClient, HttpError, display_url, outcome_detail};
use crate::peer::{ChangeWindow, InitialPage, PeerError, SourceReader, TargetWriter, WriteOp, WriteResult};

/// Initial-load "type" that drives a Bulk `$export`.
pub const BULK_EXPORT: &str = "$export";

/// Most history entries read for one incremental window.
pub const MAX_WINDOW_ENTRIES: usize = 200_000;

/// FHIR instant for query parameters (millisecond precision, `Z`).
pub fn fhir_instant(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// The `next` link of a Bundle.
pub fn next_link(bundle: &Value) -> Option<String> {
    bundle
        .get("link")?
        .as_array()?
        .iter()
        .find(|l| l.get("relation").and_then(Value::as_str) == Some("next"))?
        .get("url")?
        .as_str()
        .map(str::to_string)
}

/// `W/"3"` / `"3"` → `3`.
pub fn etag_version(etag: &str) -> Option<String> {
    let t = etag.trim().trim_start_matches("W/").trim_matches('"');
    (!t.is_empty()).then(|| t.to_string())
}

/// Version from a `Location` like `…/Patient/1/_history/3`.
pub fn location_version(loc: &str) -> Option<String> {
    let mut parts = loc.trim_end_matches('/').rsplit('/');
    let v = parts.next()?;
    (parts.next()? == "_history").then(|| v.to_string())
}

/// `Type/id[/_history/v]` → (type, id).
fn parse_ref(url: &str) -> Option<(String, String)> {
    let path = url.split('?').next()?;
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    // Absolute URLs: take the last Type/id pair.
    let (t, i) = match segs.as_slice() {
        [.., t, i, "_history", _] => (*t, *i),
        [.., t, i] => (*t, *i),
        _ => return None,
    };
    (valid_type(t) && valid_id(i)).then(|| (t.to_string(), i.to_string()))
}

/// Changes in a search-set or history Bundle.
pub fn bundle_changes(bundle: &Value, fallback_time: DateTime<Utc>) -> Vec<Change> {
    let mut out = Vec::new();
    let Some(entries) = bundle.get("entry").and_then(Value::as_array) else {
        return out;
    };
    for e in entries {
        let mode = e.get("search").and_then(|s| s.get("mode")).and_then(Value::as_str);
        if matches!(mode, Some("include") | Some("outcome")) {
            continue;
        }
        let method = e
            .get("request")
            .and_then(|r| r.get("method"))
            .and_then(Value::as_str);
        let resource = e.get("resource").filter(|r| {
            r.get("resourceType").and_then(Value::as_str) != Some("OperationOutcome")
        });
        let ts = e
            .get("response")
            .and_then(|r| r.get("lastModified"))
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc));
        if method == Some("DELETE") || (resource.is_none() && method.is_some()) {
            let url = e
                .get("request")
                .and_then(|r| r.get("url"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if let Some((t, i)) = parse_ref(url) {
                let v = e
                    .get("response")
                    .and_then(|r| r.get("etag"))
                    .and_then(Value::as_str)
                    .and_then(etag_version);
                out.push(Change::delete(t, i, v, ts.unwrap_or(fallback_time)));
            }
            continue;
        }
        if let Some(r) = resource
            && let Some(c) = Change::from_resource(r.clone(), ts.unwrap_or(fallback_time))
        {
            out.push(c);
        }
    }
    out
}

/// Resource types a CapabilityStatement lists.
pub fn capability_types(cs: &Value) -> Vec<String> {
    let mut v: Vec<String> = cs
        .get("rest")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|r| r.get("resource").and_then(Value::as_array).into_iter().flatten())
        .filter_map(|r| r.get("type").and_then(Value::as_str))
        .filter(|t| valid_type(t))
        .map(str::to_string)
        .collect();
    v.sort();
    v.dedup();
    v
}

/// The FHIR release a CapabilityStatement declares (`4.0.1` → `R4`).
pub fn capability_release(cs: &Value) -> Option<helios_endpoints::FhirRelease> {
    use helios_endpoints::FhirRelease::*;
    let v = cs.get("fhirVersion")?.as_str()?;
    Some(if v.starts_with("4.0") {
        R4
    } else if v.starts_with("4.3") {
        R4B
    } else if v.starts_with("5.") {
        R5
    } else if v.starts_with("6.") {
        R6
    } else {
        return None;
    })
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Default)]
struct ExportState {
    #[serde(default)]
    poll: Option<String>,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    auth: bool,
    #[serde(default)]
    file: usize,
    #[serde(default)]
    skip: usize,
}

/// An external server read as a source.
pub struct ExternalSource {
    client: Arc<FhirClient>,
    filters: BTreeMap<String, String>,
    bulk: bool,
    export_types: Vec<String>,
    file_cache: Mutex<Option<(String, Arc<Vec<u8>>)>>,
    poll_interval: Duration,
}

impl ExternalSource {
    /// A source; `bulk` = initial load through `$export` of `export_types`.
    pub fn new(
        client: Arc<FhirClient>,
        filters: BTreeMap<String, String>,
        bulk: bool,
        export_types: Vec<String>,
    ) -> Self {
        Self {
            client,
            filters,
            bulk,
            export_types,
            file_cache: Mutex::new(None),
            poll_interval: Duration::from_secs(2),
        }
    }

    /// Overrides the `$export` status poll interval (tests).
    pub fn with_poll_interval(mut self, d: Duration) -> Self {
        self.poll_interval = d;
        self
    }

    /// The server's CapabilityStatement.
    pub async fn capability(&self) -> Result<Value, PeerError> {
        Ok(self.client.get_json(self.client.url("metadata")).await?)
    }

    async fn search_all(&self, first: Url, cap: usize, out: &mut Vec<Change>) -> Result<(), PeerError> {
        let mut url = Some(first);
        while let Some(u) = url {
            let b = self.client.get_json(u).await?;
            out.extend(bundle_changes(&b, Utc::now()));
            if out.len() > cap {
                return Err(PeerError(format!(
                    "more than {cap} changes in one incremental window; poll more often"
                )));
            }
            url = next_link(&b).map(|l| self.client.check_link(&l)).transpose()?;
        }
        Ok(())
    }

    async fn export_page(&self, page: Option<&str>, since: Option<DateTime<Utc>>, count: u32) -> Result<InitialPage, PeerError> {
        let mut st: ExportState = match page {
            Some(p) => serde_json::from_str(p).map_err(|_| PeerError("bad export state".into()))?,
            None => {
                let mut q = String::from("$export?_outputFormat=application%2Ffhir%2Bndjson");
                if !self.export_types.is_empty() {
                    q.push_str(&format!("&_type={}", enc(&self.export_types.join(","))));
                }
                if let Some(s) = since {
                    q.push_str(&format!("&_since={}", enc(&fhir_instant(s))));
                }
                let url = self.client.url(&q);
                let resp = self
                    .client
                    .send(
                        reqwest::Method::GET,
                        url.clone(),
                        None,
                        &[("prefer", "respond-async".to_string())],
                    )
                    .await?;
                if resp.status != 202 {
                    return Err(HttpError::Status {
                        url: display_url(&url),
                        status: resp.status,
                        detail: outcome_detail(&resp.body),
                    }
                    .into());
                }
                let loc = resp
                    .header("content-location")
                    .ok_or_else(|| PeerError("$export kick-off without Content-Location".into()))?;
                let poll = self.client.check_link(loc)?;
                ExportState {
                    poll: Some(poll.to_string()),
                    ..ExportState::default()
                }
            }
        };
        if let Some(poll) = st.poll.clone() {
            let url = self.client.check_link(&poll)?;
            let resp = self.client.send(reqwest::Method::GET, url.clone(), None, &[]).await?;
            match resp.status {
                202 => {
                    let wait = resp
                        .header("retry-after")
                        .and_then(|v| crate::throttle::parse_retry_after(v, Utc::now()))
                        .unwrap_or(self.poll_interval)
                        .min(Duration::from_secs(30));
                    tokio::time::sleep(wait).await;
                    return Ok(InitialPage {
                        changes: vec![],
                        next: Some(serde_json::to_string(&st).expect("state")),
                    });
                }
                200 => {
                    let manifest = resp.json(&url)?;
                    st.auth = manifest
                        .get("requiresAccessToken")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    st.files = manifest
                        .get("output")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|o| o.get("url").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect();
                    st.poll = None;
                    st.file = 0;
                    st.skip = 0;
                }
                s => {
                    return Err(HttpError::Status {
                        url: display_url(&url),
                        status: s,
                        detail: outcome_detail(&resp.body),
                    }
                    .into());
                }
            }
        }
        let Some(file_url) = st.files.get(st.file).cloned() else {
            return Ok(InitialPage::default());
        };
        let body = self.download(&file_url, st.auth).await?;
        let text = std::str::from_utf8(&body).map_err(|_| PeerError("export file is not UTF-8".into()))?;
        let mut changes = Vec::new();
        let mut consumed = 0usize;
        let mut lines = text.lines().filter(|l| !l.trim().is_empty()).skip(st.skip).peekable();
        while consumed < count as usize {
            let Some(line) = lines.next() else { break };
            consumed += 1;
            if let Ok(v) = serde_json::from_str::<Value>(line)
                && let Some(c) = Change::from_resource(v, Utc::now())
            {
                changes.push(c);
            }
        }
        if lines.peek().is_some() {
            st.skip += consumed;
        } else {
            st.file += 1;
            st.skip = 0;
        }
        let next = (st.file < st.files.len()).then(|| serde_json::to_string(&st).expect("state"));
        Ok(InitialPage { changes, next })
    }

    async fn download(&self, file_url: &str, auth: bool) -> Result<Arc<Vec<u8>>, PeerError> {
        {
            let cache = self.file_cache.lock().await;
            if let Some((u, b)) = cache.as_ref()
                && u == file_url
            {
                return Ok(b.clone());
            }
        }
        let url = Url::parse(file_url).map_err(|e| PeerError(format!("export file URL: {e}")))?;
        let resp = self.client.get_foreign(url.clone(), auth).await?;
        if !resp.ok() {
            return Err(HttpError::Status {
                url: display_url(&url),
                status: resp.status,
                detail: String::new(),
            }
            .into());
        }
        let body = Arc::new(resp.body);
        *self.file_cache.lock().await = Some((file_url.to_string(), body.clone()));
        Ok(body)
    }
}

#[async_trait]
impl SourceReader for ExternalSource {
    async fn initial_types(&self) -> Result<Vec<String>, PeerError> {
        if self.bulk {
            return Ok(vec![BULK_EXPORT.to_string()]);
        }
        Ok(capability_types(&self.capability().await?))
    }

    async fn initial_page(
        &self,
        resource_type: &str,
        page: Option<&str>,
        since: Option<DateTime<Utc>>,
        count: u32,
    ) -> Result<InitialPage, PeerError> {
        if resource_type == BULK_EXPORT {
            return self.export_page(page, since, count).await;
        }
        let url = match page {
            Some(p) => self.client.check_link(p)?,
            None => {
                let mut q = format!("{resource_type}?_count={count}&_sort=_lastUpdated");
                if let Some(s) = since {
                    q.push_str(&format!("&_lastUpdated=ge{}", enc(&fhir_instant(s))));
                }
                if let Some(f) = self.filters.get(resource_type) {
                    q.push('&');
                    q.push_str(f.trim_start_matches(['?', '&']));
                }
                self.client.url(&q)
            }
        };
        let b = self.client.get_json(url).await?;
        let changes = bundle_changes(&b, Utc::now())
            .into_iter()
            .filter(|c| c.resource_type == resource_type)
            .collect();
        let next = next_link(&b).map(|l| self.client.check_link(&l)).transpose()?;
        Ok(InitialPage {
            changes,
            next: next.map(|u| u.to_string()),
        })
    }

    async fn changes_since(
        &self,
        since: DateTime<Utc>,
        _page: Option<&str>,
        count: u32,
    ) -> Result<ChangeWindow, PeerError> {
        let mut changes = Vec::new();
        let s = enc(&fhir_instant(since));
        self.search_all(
            self.client.url(&format!("_history?_since={s}&_count={count}")),
            MAX_WINDOW_ENTRIES,
            &mut changes,
        )
        .await?;
        if !self.filters.is_empty() {
            // History cannot apply search filters: filtered types are read
            // by search instead (their deletes are not propagated).
            changes.retain(|c| !self.filters.contains_key(&c.resource_type));
            for (t, f) in &self.filters {
                let q = format!(
                    "{t}?_lastUpdated=ge{s}&_count={count}&{}",
                    f.trim_start_matches(['?', '&'])
                );
                self.search_all(self.client.url(&q), MAX_WINDOW_ENTRIES, &mut changes).await?;
            }
        }
        Ok(ChangeWindow {
            changes,
            neutral: vec![],
            next: None,
        })
    }

    async fn read(&self, resource_type: &str, id: &str) -> Result<Option<Change>, PeerError> {
        let url = self.client.url(&format!("{resource_type}/{id}"));
        let resp = self.client.send(reqwest::Method::GET, url.clone(), None, &[]).await?;
        match resp.status {
            404 | 410 => Ok(None),
            s if (200..300).contains(&s) => Ok(Change::from_resource(resp.json(&url)?, Utc::now())),
            s => Err(HttpError::Status {
                url: display_url(&url),
                status: s,
                detail: outcome_detail(&resp.body),
            }
            .into()),
        }
    }
}

/// An external server written as a target.
pub struct ExternalTarget {
    client: Arc<FhirClient>,
    individual: AtomicBool,
}

impl ExternalTarget {
    /// A target.
    pub fn new(client: Arc<FhirClient>) -> Self {
        Self {
            client,
            individual: AtomicBool::new(false),
        }
    }

    /// Whether the server refused Bundles and writes go one by one.
    pub fn individual_mode(&self) -> bool {
        self.individual.load(Ordering::Relaxed)
    }

    fn result_of(status: u16, etag: Option<&str>, location: Option<&str>, delete: bool, detail: String) -> WriteResult {
        let version = etag.and_then(etag_version).or_else(|| location.and_then(location_version));
        match status {
            201 if !delete => WriteResult::Created(version),
            200..=299 if !delete => WriteResult::Updated(version),
            200..=299 | 404 | 410 if delete => WriteResult::Deleted,
            409 | 412 => WriteResult::Conflict(format!("HTTP {status}{detail}")),
            s => WriteResult::Failed(format!("HTTP {s}{detail}")),
        }
    }

    async fn apply_one(&self, op: &WriteOp) -> Result<WriteResult, PeerError> {
        let (method, rt, id, body, if_match) = match op {
            WriteOp::Put { resource_type, id, resource, if_match } => {
                (reqwest::Method::PUT, resource_type, id, Some(resource), if_match)
            }
            WriteOp::Delete { resource_type, id, if_match } => {
                (reqwest::Method::DELETE, resource_type, id, None, if_match)
            }
        };
        let url = self.client.url(&format!("{rt}/{id}"));
        let mut headers = Vec::new();
        if let Some(v) = if_match {
            headers.push(("if-match", format!("W/\"{v}\"")));
        }
        let delete = body.is_none();
        let resp = self.client.send(method, url, body, &headers).await?;
        Ok(Self::result_of(
            resp.status,
            resp.header("etag"),
            resp.header("location").or_else(|| resp.header("content-location")),
            delete,
            outcome_detail(&resp.body),
        ))
    }

    async fn apply_bundle(&self, ops: &[WriteOp]) -> Result<Option<Vec<WriteResult>>, PeerError> {
        let entries: Vec<Value> = ops
            .iter()
            .map(|op| match op {
                WriteOp::Put { resource_type, id, resource, if_match } => {
                    let mut req = json!({"method": "PUT", "url": format!("{resource_type}/{id}")});
                    if let Some(v) = if_match {
                        req["ifMatch"] = json!(format!("W/\"{v}\""));
                    }
                    json!({"fullUrl": self.client.url(&format!("{resource_type}/{id}")).to_string(),
                           "resource": resource, "request": req})
                }
                WriteOp::Delete { resource_type, id, if_match } => {
                    let mut req = json!({"method": "DELETE", "url": format!("{resource_type}/{id}")});
                    if let Some(v) = if_match {
                        req["ifMatch"] = json!(format!("W/\"{v}\""));
                    }
                    json!({"request": req})
                }
            })
            .collect();
        let bundle = json!({"resourceType": "Bundle", "type": "batch", "entry": entries});
        let url = self.client.base().clone();
        let resp = self.client.send(reqwest::Method::POST, url.clone(), Some(&bundle), &[]).await?;
        if !resp.ok() {
            if matches!(resp.status, 400 | 403 | 404 | 405 | 415 | 422 | 501) {
                return Ok(None);
            }
            return Err(HttpError::Status {
                url: display_url(&url),
                status: resp.status,
                detail: outcome_detail(&resp.body),
            }
            .into());
        }
        let Ok(out) = resp.json(&url) else { return Ok(None) };
        let ok_type = matches!(
            out.get("type").and_then(Value::as_str),
            Some("batch-response") | Some("transaction-response")
        );
        let Some(rs) = out.get("entry").and_then(Value::as_array).filter(|e| ok_type && e.len() == ops.len()) else {
            return Ok(None);
        };
        Ok(Some(
            rs.iter()
                .zip(ops)
                .map(|(e, op)| {
                    let r = e.get("response");
                    let status = r
                        .and_then(|r| r.get("status"))
                        .and_then(Value::as_str)
                        .and_then(|s| s.split_whitespace().next())
                        .and_then(|s| s.parse::<u16>().ok())
                        .unwrap_or(500);
                    let detail = r
                        .and_then(|r| r.get("outcome"))
                        .map(|o| outcome_detail(o.to_string().as_bytes()))
                        .unwrap_or_default();
                    Self::result_of(
                        status,
                        r.and_then(|r| r.get("etag")).and_then(Value::as_str),
                        r.and_then(|r| r.get("location")).and_then(Value::as_str),
                        matches!(op, WriteOp::Delete { .. }),
                        detail,
                    )
                })
                .collect(),
        ))
    }
}

#[async_trait]
impl TargetWriter for ExternalTarget {
    async fn current(&self, resource_type: &str, id: &str) -> Result<Option<TargetInfo>, PeerError> {
        let url = self.client.url(&format!("{resource_type}/{id}"));
        let resp = self.client.send(reqwest::Method::GET, url.clone(), None, &[]).await?;
        match resp.status {
            404 | 410 => Ok(None),
            s if (200..300).contains(&s) => {
                let v = resp.json(&url)?;
                let meta = v.get("meta");
                Ok(Some(TargetInfo {
                    version: meta
                        .and_then(|m| m.get("versionId"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| resp.header("etag").and_then(etag_version)),
                    last_updated: meta
                        .and_then(|m| m.get("lastUpdated"))
                        .and_then(Value::as_str)
                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|d| d.with_timezone(&Utc)),
                    hash: content_hash(&v),
                }))
            }
            s => Err(HttpError::Status {
                url: display_url(&url),
                status: s,
                detail: outcome_detail(&resp.body),
            }
            .into()),
        }
    }

    async fn apply(&self, ops: Vec<WriteOp>) -> Result<Vec<WriteResult>, PeerError> {
        if ops.is_empty() {
            return Ok(vec![]);
        }
        if !self.individual_mode() {
            match self.apply_bundle(&ops).await? {
                Some(r) => return Ok(r),
                None => {
                    tracing::info!(
                        server = %display_url(self.client.base()),
                        "server refused batch Bundles; writing resources individually"
                    );
                    self.individual.store(true, Ordering::Relaxed);
                }
            }
        }
        let mut out = Vec::with_capacity(ops.len());
        for op in &ops {
            out.push(self.apply_one(op).await?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_parsing() {
        let t0 = Utc::now();
        let b = json!({
            "resourceType": "Bundle", "type": "history",
            "link": [{"relation": "self", "url": "x"}, {"relation": "next", "url": "https://s/fhir?page=2"}],
            "entry": [
                {"resource": {"resourceType": "Patient", "id": "a", "meta": {"versionId": "2", "lastUpdated": "2026-01-01T00:00:01Z"}},
                 "request": {"method": "PUT", "url": "Patient/a"}},
                {"request": {"method": "DELETE", "url": "Patient/b"},
                 "response": {"status": "204", "etag": "W/\"3\"", "lastModified": "2026-01-01T00:00:02Z"}},
                {"request": {"method": "DELETE", "url": "https://s/fhir/Observation/o/_history/5"}},
                {"resource": {"resourceType": "Organization", "id": "inc"}, "search": {"mode": "include"}},
                {"resource": {"resourceType": "OperationOutcome", "id": "w"}, "search": {"mode": "outcome"}},
                {"request": {"method": "DELETE", "url": "../../etc"}},
            ]
        });
        assert_eq!(next_link(&b).as_deref(), Some("https://s/fhir?page=2"));
        let c = bundle_changes(&b, t0);
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].token(), "Patient/a/2");
        assert!(c[1].deleted && c[1].version.as_deref() == Some("3"));
        assert_eq!(c[1].last_updated.to_rfc3339(), "2026-01-01T00:00:02+00:00");
        assert_eq!(c[2].key(), "Observation/o");
        assert_eq!(c[2].last_updated, t0);
        assert!(next_link(&json!({})).is_none());
        assert!(bundle_changes(&json!({}), t0).is_empty());
    }

    #[test]
    fn helpers() {
        assert_eq!(etag_version("W/\"12\"").as_deref(), Some("12"));
        assert_eq!(etag_version("\"a\"").as_deref(), Some("a"));
        assert_eq!(etag_version("W/\"\""), None);
        assert_eq!(location_version("https://s/Patient/1/_history/4").as_deref(), Some("4"));
        assert_eq!(location_version("https://s/Patient/1"), None);
        let t = DateTime::parse_from_rfc3339("2026-01-01T00:00:00.123456Z").unwrap().with_timezone(&Utc);
        assert_eq!(fhir_instant(t), "2026-01-01T00:00:00.123Z");
        let cs = json!({"fhirVersion": "4.0.1", "rest": [{"resource": [{"type": "Patient"}, {"type": "Observation"}, {"type": "bad1"}]}]});
        assert_eq!(capability_types(&cs), vec!["Observation", "Patient"]);
        assert_eq!(capability_release(&cs), Some(helios_endpoints::FhirRelease::R4));
        assert_eq!(capability_release(&json!({"fhirVersion": "5.0.0"})), Some(helios_endpoints::FhirRelease::R5));
        assert_eq!(capability_release(&json!({"fhirVersion": "4.3.0"})), Some(helios_endpoints::FhirRelease::R4B));
        assert_eq!(capability_release(&json!({"fhirVersion": "6.0.0-ballot3"})), Some(helios_endpoints::FhirRelease::R6));
        assert_eq!(capability_release(&json!({"fhirVersion": "3.0.2"})), None);
    }

    #[test]
    fn write_results() {
        use ExternalTarget as T;
        assert_eq!(T::result_of(201, Some("W/\"1\""), None, false, String::new()), WriteResult::Created(Some("1".into())));
        assert_eq!(T::result_of(200, None, Some("x/Patient/1/_history/7"), false, String::new()), WriteResult::Updated(Some("7".into())));
        assert_eq!(T::result_of(204, None, None, true, String::new()), WriteResult::Deleted);
        assert_eq!(T::result_of(404, None, None, true, String::new()), WriteResult::Deleted);
        assert!(matches!(T::result_of(412, None, None, false, String::new()), WriteResult::Conflict(_)));
        assert!(matches!(T::result_of(422, None, None, false, ": bad".into()), WriteResult::Failed(m) if m.contains("bad")));
    }

    #[test]
    fn export_state_roundtrip() {
        let s = ExportState { poll: None, files: vec!["a".into()], auth: true, file: 0, skip: 5 };
        let back: ExportState = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
