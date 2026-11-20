//! A small in-process FHIR server with HAPI-like behaviour: `_getpages`
//! paging links, `_history?_since` newest first with DELETE entries,
//! versioned PUT/DELETE with ETags and If-Match, batch Bundles (or their
//! refusal), 429 + Retry-After, an optional bearer requirement, and a Bulk
//! `$export` flow.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub struct Version {
    pub resource: Option<Value>,
    pub at: DateTime<Utc>,
    pub version: u64,
}

#[derive(Default)]
pub struct State {
    /// type/id → versions (last = current).
    pub data: BTreeMap<String, Vec<Version>>,
    pub refuse_bundles: bool,
    pub throttle_next: u32,
    pub require_bearer: Option<String>,
    pub evil_next: bool,
    pub requests: Vec<String>,
    pub bundle_posts: usize,
    pub individual_puts: usize,
    pub export_polls: usize,
    pub clock: Option<DateTime<Utc>>,
}

#[derive(Clone)]
pub struct Hapi {
    pub base: String,
    pub state: Arc<Mutex<State>>,
}

fn oo(status: StatusCode, msg: &str) -> Response {
    (
        status,
        [("content-type", "application/fhir+json")],
        json!({"resourceType": "OperationOutcome", "issue": [{"severity": "error", "code": "processing", "diagnostics": msg}]}).to_string(),
    )
        .into_response()
}

fn ok_json(status: StatusCode, v: Value, extra: &[(&str, String)]) -> Response {
    let mut r = (
        status,
        [("content-type", "application/fhir+json")],
        v.to_string(),
    )
        .into_response();
    for (k, val) in extra {
        r.headers_mut().insert(
            axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            val.parse().unwrap(),
        );
    }
    r
}

fn query(uri: &Uri) -> BTreeMap<String, String> {
    uri.query()
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_default()
}

impl State {
    fn now(&mut self) -> DateTime<Utc> {
        // Strictly increasing millisecond clock (FHIR instants are ms).
        let t = match self.clock {
            Some(c) => (c + Duration::milliseconds(5)).max(Utc::now()),
            None => Utc::now(),
        };
        let t = DateTime::from_timestamp_millis(t.timestamp_millis()).unwrap();
        self.clock = Some(t);
        t
    }

    pub fn put(&mut self, rt: &str, id: &str, mut body: Value) -> (u64, bool) {
        let at = self.now();
        let key = format!("{rt}/{id}");
        let versions = self.data.entry(key).or_default();
        let created = versions.last().is_none_or(|v| v.resource.is_none());
        let version = versions.last().map(|v| v.version + 1).unwrap_or(1);
        body["resourceType"] = json!(rt);
        body["id"] = json!(id);
        body["meta"] = json!({"versionId": version.to_string(), "lastUpdated": at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)});
        versions.push(Version {
            resource: Some(body),
            at,
            version,
        });
        (version, created)
    }

    pub fn delete(&mut self, rt: &str, id: &str) -> bool {
        let at = self.now();
        let key = format!("{rt}/{id}");
        match self.data.get_mut(&key) {
            Some(v) if v.last().is_some_and(|x| x.resource.is_some()) => {
                let version = v.last().unwrap().version + 1;
                v.push(Version {
                    resource: None,
                    at,
                    version,
                });
                true
            }
            _ => false,
        }
    }

    pub fn current(&self, rt: &str, id: &str) -> Option<Value> {
        self.data
            .get(&format!("{rt}/{id}"))?
            .last()?
            .resource
            .clone()
    }
}

impl Hapi {
    pub async fn start() -> Hapi {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hapi = Hapi {
            base: format!("http://127.0.0.1:{}/fhir", addr.port()),
            state: Arc::new(Mutex::new(State::default())),
        };
        let h = hapi.clone();
        let app = Router::new().fallback(
            move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                let h = h.clone();
                async move { h.handle(method, uri, headers, body) }
            },
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        hapi
    }

    fn page(
        &self,
        items: Vec<Value>,
        offset: usize,
        count: usize,
        kind: &str,
        token: &str,
    ) -> Value {
        let total = items.len();
        let slice: Vec<Value> = items.into_iter().skip(offset).take(count).collect();
        let mut links = vec![
            json!({"relation": "self", "url": format!("{}?_getpages={token}&_getpagesoffset={offset}", self.base)}),
        ];
        if offset + count < total {
            let next = if self.state.lock().unwrap().evil_next {
                format!(
                    "http://127.0.0.2:1/fhir?_getpages={token}&_getpagesoffset={}&_count={count}",
                    offset + count
                )
            } else {
                format!(
                    "{}?_getpages={token}&_getpagesoffset={}&_count={count}",
                    self.base,
                    offset + count
                )
            };
            links.push(json!({"relation": "next", "url": next}));
        }
        json!({"resourceType": "Bundle", "type": kind, "total": total, "link": links, "entry": slice})
    }

    fn search_items(&self, rt: &str, q: &BTreeMap<String, String>) -> Vec<Value> {
        let st = self.state.lock().unwrap();
        let ge = q
            .get("_lastUpdated")
            .and_then(|v| v.strip_prefix("ge"))
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc));
        let mut v: Vec<(DateTime<Utc>, Value)> = st
            .data
            .iter()
            .filter(|(k, _)| k.starts_with(&format!("{rt}/")))
            .filter_map(|(_, vs)| {
                vs.last()
                    .and_then(|x| x.resource.clone().map(|r| (x.at, r)))
            })
            .filter(|(at, _)| ge.is_none_or(|g| *at >= g))
            .collect();
        v.sort_by_key(|x| x.0);
        v.into_iter()
            .map(|(_, r)| json!({"fullUrl": format!("{}/{}/{}", self.base, rt, r["id"].as_str().unwrap()), "resource": r, "search": {"mode": "match"}}))
            .collect()
    }

    fn history_items(&self, since: Option<DateTime<Utc>>) -> Vec<Value> {
        let st = self.state.lock().unwrap();
        let mut v: Vec<(DateTime<Utc>, Value)> = Vec::new();
        for (key, versions) in &st.data {
            for x in versions {
                if since.is_some_and(|s| x.at < s) {
                    continue;
                }
                let entry = match &x.resource {
                    Some(r) => json!({"fullUrl": format!("{}/{key}", self.base), "resource": r,
                                      "request": {"method": "PUT", "url": key},
                                      "response": {"status": "200 OK", "etag": format!("W/\"{}\"", x.version)}}),
                    None => {
                        json!({"request": {"method": "DELETE", "url": format!("{key}/_history/{}", x.version)},
                                   "response": {"status": "204 No Content", "etag": format!("W/\"{}\"", x.version),
                                                "lastModified": x.at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)}})
                    }
                };
                v.push((x.at, entry));
            }
        }
        v.sort_by(|a, b| b.0.cmp(&a.0)); // newest first, like HAPI
        v.into_iter().map(|x| x.1).collect()
    }

    fn write(
        &self,
        method: &str,
        rt: &str,
        id: &str,
        body: Option<Value>,
        if_match: Option<&str>,
    ) -> (u16, Option<u64>, Value) {
        let mut st = self.state.lock().unwrap();
        if let Some(m) = if_match {
            let want = m.trim_start_matches("W/").trim_matches('"');
            let cur = st
                .data
                .get(&format!("{rt}/{id}"))
                .and_then(|v| v.last())
                .map(|v| v.version.to_string());
            if cur.as_deref() != Some(want) {
                return (
                    412,
                    None,
                    json!({"resourceType": "OperationOutcome", "issue": [{"severity": "error", "code": "conflict", "diagnostics": "version mismatch"}]}),
                );
            }
        }
        match method {
            "PUT" => {
                let (v, created) = st.put(rt, id, body.unwrap_or(json!({})));
                (
                    if created { 201 } else { 200 },
                    Some(v),
                    st.current(rt, id).unwrap(),
                )
            }
            _ => {
                st.delete(rt, id);
                (204, None, json!({}))
            }
        }
    }

    fn handle(&self, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
        let q = query(&uri);
        {
            let mut st = self.state.lock().unwrap();
            st.requests.push(format!("{method} {uri}"));
            if let Some(tok) = &st.require_bearer {
                let ok = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v == format!("Bearer {tok}"));
                if !ok {
                    return oo(StatusCode::UNAUTHORIZED, "bearer token required");
                }
            }
            if st.throttle_next > 0 {
                st.throttle_next -= 1;
                let mut r = oo(StatusCode::TOO_MANY_REQUESTS, "slow down");
                r.headers_mut().insert("retry-after", "1".parse().unwrap());
                return r;
            }
        }
        let path = uri
            .path()
            .trim_start_matches("/fhir")
            .trim_matches('/')
            .to_string();
        let segs: Vec<&str> = if path.is_empty() {
            vec![]
        } else {
            path.split('/').collect()
        };
        let count: usize = q
            .get("_count")
            .and_then(|c| c.parse().ok())
            .unwrap_or(20)
            .min(2);
        let offset: usize = q
            .get("_getpagesoffset")
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        match (method.as_str(), segs.as_slice()) {
            ("GET", ["metadata"]) => ok_json(
                StatusCode::OK,
                json!({"resourceType": "CapabilityStatement", "fhirVersion": "4.0.1", "software": {"name": "HAPI-like mock"},
                       "rest": [{"mode": "server", "resource": [{"type": "Patient"}, {"type": "Observation"}]}]}),
                &[],
            ),
            ("GET", []) if q.contains_key("_getpages") => {
                let token = q["_getpages"].clone();
                let items = if let Some(since) = token.strip_prefix("history:") {
                    let since = DateTime::parse_from_rfc3339(since)
                        .ok()
                        .map(|d| d.with_timezone(&Utc));
                    self.history_items(since)
                } else {
                    let rt = token.split(':').next().unwrap_or("").to_string();
                    let mut qq = BTreeMap::new();
                    if let Some(ge) = token.split(':').nth(1).filter(|s| !s.is_empty()) {
                        qq.insert(
                            "_lastUpdated".to_string(),
                            format!("ge{}", ge.replace('_', ":")),
                        );
                    }
                    self.search_items(&rt, &qq)
                };
                let kind = if token.starts_with("history:") {
                    "history"
                } else {
                    "searchset"
                };
                ok_json(
                    StatusCode::OK,
                    self.page(items, offset, count, kind, &token),
                    &[],
                )
            }
            ("GET", ["_history"]) => {
                let since_raw = q.get("_since").cloned().unwrap_or_default();
                let since = DateTime::parse_from_rfc3339(&since_raw)
                    .ok()
                    .map(|d| d.with_timezone(&Utc));
                let items = self.history_items(since);
                ok_json(
                    StatusCode::OK,
                    self.page(items, 0, count, "history", &format!("history:{since_raw}")),
                    &[],
                )
            }
            ("GET", ["$export"]) => {
                let mut r = StatusCode::ACCEPTED.into_response();
                r.headers_mut().insert(
                    "content-location",
                    format!("{}/export-status/1", self.base).parse().unwrap(),
                );
                r
            }
            ("GET", ["export-status", _]) => {
                let polls = {
                    let mut st = self.state.lock().unwrap();
                    st.export_polls += 1;
                    st.export_polls
                };
                if polls < 2 {
                    let mut r = StatusCode::ACCEPTED.into_response();
                    r.headers_mut().insert("retry-after", "0".parse().unwrap());
                    return r;
                }
                ok_json(
                    StatusCode::OK,
                    json!({"transactionTime": Utc::now().to_rfc3339(), "request": format!("{}/$export", self.base),
                           "requiresAccessToken": false,
                           "output": [{"type": "Patient", "url": format!("{}/export-file/Patient.ndjson", self.base)}],
                           "error": []}),
                    &[],
                )
            }
            ("GET", ["export-file", _]) => {
                let st = self.state.lock().unwrap();
                let lines: Vec<String> = st
                    .data
                    .iter()
                    .filter(|(k, _)| k.starts_with("Patient/"))
                    .filter_map(|(_, v)| v.last()?.resource.as_ref().map(|r| r.to_string()))
                    .collect();
                (
                    StatusCode::OK,
                    [("content-type", "application/fhir+ndjson")],
                    lines.join("\n") + "\n",
                )
                    .into_response()
            }
            ("GET", [rt]) => {
                let items = self.search_items(rt, &q);
                let ge = q
                    .get("_lastUpdated")
                    .and_then(|v| v.strip_prefix("ge"))
                    .unwrap_or("")
                    .replace(':', "_");
                ok_json(
                    StatusCode::OK,
                    self.page(items, 0, count, "searchset", &format!("{rt}:{ge}")),
                    &[],
                )
            }
            ("GET", [rt, id]) => {
                let st = self.state.lock().unwrap();
                match st.data.get(&format!("{rt}/{id}")).and_then(|v| v.last()) {
                    Some(Version {
                        resource: Some(r),
                        version,
                        ..
                    }) => ok_json(
                        StatusCode::OK,
                        r.clone(),
                        &[("etag", format!("W/\"{version}\""))],
                    ),
                    Some(_) => oo(StatusCode::GONE, "deleted"),
                    None => oo(StatusCode::NOT_FOUND, "not found"),
                }
            }
            ("PUT", [rt, id]) | ("DELETE", [rt, id]) => {
                self.state.lock().unwrap().individual_puts += 1;
                let body = serde_json::from_slice::<Value>(&body).ok();
                let im = headers.get("if-match").and_then(|v| v.to_str().ok());
                let (status, v, res) = self.write(method.as_str(), rt, id, body, im);
                let mut extra = vec![];
                if let Some(v) = v {
                    extra.push(("etag", format!("W/\"{v}\"")));
                }
                ok_json(StatusCode::from_u16(status).unwrap(), res, &extra)
            }
            ("POST", []) => {
                let refuse = {
                    let mut st = self.state.lock().unwrap();
                    st.bundle_posts += 1;
                    st.refuse_bundles
                };
                if refuse {
                    return oo(
                        StatusCode::BAD_REQUEST,
                        "batch processing is disabled on this server",
                    );
                }
                let b: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
                let mut out = Vec::new();
                for e in b["entry"].as_array().cloned().unwrap_or_default() {
                    let m = e["request"]["method"].as_str().unwrap_or("").to_string();
                    let url = e["request"]["url"].as_str().unwrap_or("").to_string();
                    let (rt, id) = url.split_once('/').unwrap_or(("", ""));
                    let (status, v, _) = self.write(
                        &m,
                        rt,
                        id,
                        e.get("resource").cloned(),
                        e["request"]["ifMatch"].as_str(),
                    );
                    let mut resp = json!({"status": format!("{status}")});
                    if let Some(v) = v {
                        resp["etag"] = json!(format!("W/\"{v}\""));
                    }
                    out.push(json!({"response": resp}));
                }
                ok_json(
                    StatusCode::OK,
                    json!({"resourceType": "Bundle", "type": "batch-response", "entry": out}),
                    &[],
                )
            }
            _ => Response::builder().status(404).body(Body::empty()).unwrap(),
        }
    }
}
