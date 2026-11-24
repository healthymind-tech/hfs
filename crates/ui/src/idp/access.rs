//! "Resource access" of a realm user (`/ui/idp/{realm}/users/{user}`):
//! the FHIR resources the user is (`fhirUser`) or may open (accessible
//! patients, other links), as a paged, filterable table with an add picker
//! that searches the bound FHIR endpoints, bulk remove, copy / paste /
//! export / import in three formats, and undo.
//!
//! | route | |
//! |---|---|
//! | `GET  …/access` | the table (fragment; the page renders the first one inline) |
//! | `GET  …/access/export?format=lines\|csv\|json[&download=1]` | every entry (Copy all, Export) |
//! | `GET  …/access/search?ep=&type=&q=` | picker results from the endpoint |
//! | `POST …/access/preview` | paste / import preview (fragment) |
//! | `POST …/access/apply` | paste / import, one transaction (JSON + undo) |
//! | `POST …/access/patch` | add, remove, set identity, undo (JSON in, JSON out) |
//!
//! Names, birth dates and the "exists / not found / no permission" status
//! come from the endpoint's own FHIR API, called as the console user (the
//! loopback self-call of the other console pages) — only for the rows on
//! screen (one `_id` search per endpoint and type), and for new rows of a
//! paste preview. Reads need the viewer role, writes editor (the console
//! guard, `/ui/idp/**`); every write is one store transaction audited as
//! `user-access-change`.

use std::collections::{BTreeMap, HashMap, HashSet};

use askama::Template;
use axum::Extension;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use helios_endpoints::{ConsolePrincipal, ConsoleRole};
use helios_idp::access::{self, AccessEntry, AccessKind, Format, Mode, RowStatus};
use helios_idp::store::RealmRecord;
use helios_idp_admin::access as ops;
use helios_idp_admin::service::{IdpConsole, OpError};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{actor, console};
use crate::i18n::{I18n, RequestLocale};
use crate::manager::forms::Form;
use crate::{RequestTenant, WebState, render};

/// Page sizes offered.
pub(crate) const PAGE_SIZES: [usize; 3] = [25, 50, 100];
/// Rows of a paste preview rendered (the counts cover every row).
const PREVIEW_ROWS: usize = 300;
/// References checked against the endpoint in one preview.
const PREVIEW_CHECK: usize = access::MAX_IMPORT;

/// Table state (query string of the user page and of `…/access`).
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct TableQuery {
    #[serde(default)]
    pub q: String,
    #[serde(default, rename = "type")]
    pub rtype: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub ep: String,
    #[serde(default)]
    pub sort: String,
    #[serde(default)]
    pub dir: String,
    #[serde(default)]
    pub page: Option<usize>,
    #[serde(default)]
    pub size: Option<usize>,
    /// `done=` flash of the user page (ignored here).
    #[serde(default)]
    pub done: Option<String>,
}

impl TableQuery {
    fn size(&self) -> usize {
        self.size
            .filter(|s| PAGE_SIZES.contains(s))
            .unwrap_or(PAGE_SIZES[0])
    }
    fn desc(&self) -> bool {
        self.dir == "desc"
    }
    fn sort_key(&self) -> &str {
        match self.sort.as_str() {
            "endpoint" | "kind" => self.sort.as_str(),
            _ => "resource",
        }
    }
    /// The query string for another page / sort (empty values left out).
    pub fn with(&self, page: usize, sort: Option<&str>) -> String {
        let (sort, dir) = match sort {
            Some(s) if s == self.sort_key() => (s, if self.desc() { "" } else { "desc" }),
            Some(s) => (s, ""),
            None => (self.sort.as_str(), self.dir.as_str()),
        };
        let mut s = form_urlencoded::Serializer::new(String::new());
        for (k, v) in [
            ("q", self.q.as_str()),
            ("type", self.rtype.as_str()),
            ("kind", self.kind.as_str()),
            ("ep", self.ep.as_str()),
            ("sort", if sort == "resource" { "" } else { sort }),
            ("dir", dir),
        ] {
            if !v.is_empty() {
                s.append_pair(k, v);
            }
        }
        if page > 1 {
            s.append_pair("page", &page.to_string());
        }
        if self.size() != PAGE_SIZES[0] {
            s.append_pair("size", &self.size().to_string());
        }
        s.finish()
    }
    fn filtered(&self) -> bool {
        !(self.q.trim().is_empty()
            && self.rtype.is_empty()
            && self.kind.is_empty()
            && self.ep.is_empty())
    }
}

/// What the endpoint said about a reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Seen {
    Found,
    Missing,
    Denied,
    Unknown,
}

impl Seen {
    pub fn tone(self) -> &'static str {
        match self {
            Seen::Found => "ok",
            Seen::Missing => "danger",
            Seen::Denied => "warn",
            Seen::Unknown => "muted",
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Seen::Found => "idpa-status-found",
            Seen::Missing => "idpa-status-missing",
            Seen::Denied => "idpa-status-denied",
            Seen::Unknown => "idpa-status-unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Resolved {
    pub name: String,
    pub detail: String,
    pub seen: Seen,
}

/// One table row.
pub(crate) struct Row {
    pub endpoint: String,
    pub reference: String,
    pub rtype: String,
    pub id: String,
    pub kind: &'static str,
    pub kind_key: &'static str,
    pub name: String,
    pub detail: String,
    pub seen: Seen,
    /// `endpoint|reference` (checkbox value).
    pub key: String,
}

pub(crate) fn kind_key(k: AccessKind) -> &'static str {
    match k {
        AccessKind::FhirUser => "idpa-kind-fhiruser",
        AccessKind::Patient => "idpa-kind-patient",
        AccessKind::Link => "idpa-kind-link",
    }
}

/// The table fragment (also included in the user page).
#[derive(Template)]
#[template(path = "partials/idp/access-table.html")]
pub(crate) struct AccessTable {
    pub i18n: I18n,
    pub page_href: String,
    pub can_edit: bool,
    pub rows: Vec<Row>,
    pub total: usize,
    pub matching: usize,
    pub page: usize,
    pub pages: usize,
    pub from: usize,
    pub to: usize,
    pub q: TableQuery,
    pub fingerprint: String,
    /// `{"Patient": 3, "fhirUser": 1, …}`: per-type and per-role counts of
    /// the whole list (the script refreshes the filter options with them).
    pub counts: String,
    /// The name search on the endpoint failed (only references matched).
    pub name_search_failed: bool,
}

impl AccessTable {
    pub fn sorted(&self, col: &str) -> &'static str {
        if self.q.sort_key() != col {
            "none"
        } else if self.q.desc() {
            "descending"
        } else {
            "ascending"
        }
    }
    pub fn href(&self, page: usize) -> String {
        let qs = self.q.with(page, None);
        if qs.is_empty() {
            self.page_href.clone()
        } else {
            format!("{}?{qs}", self.page_href)
        }
    }
    pub fn query_string(&self) -> String {
        self.q.with(self.page, None)
    }
    pub fn sort_href(&self, col: &str) -> String {
        let qs = self.q.with(1, Some(col));
        if qs.is_empty() {
            self.page_href.clone()
        } else {
            format!("{}?{qs}", self.page_href)
        }
    }
    pub fn filtered(&self) -> bool {
        self.q.filtered()
    }
    /// `(size, " selected" | "")` for the page-size select.
    pub fn sizes(&self) -> Vec<(usize, &'static str)> {
        PAGE_SIZES
            .iter()
            .map(|n| (*n, if *n == self.q.size() { " selected" } else { "" }))
            .collect()
    }
}

/// Everything the user page needs for the card.
pub(crate) struct AccessCard {
    pub base: String,
    pub can_edit: bool,
    pub endpoints: Vec<String>,
    pub types: Vec<(String, usize)>,
    pub kinds: Vec<(&'static str, &'static str, usize)>,
    pub link_types: Vec<&'static str>,
    pub table: String,
    pub total: usize,
    pub q: TableQuery,
    pub max_import: i64,
}

impl AccessCard {
    pub fn is_type(&self, t: &str) -> bool {
        self.q.rtype == t
    }
    pub fn is_kind(&self, k: &str) -> bool {
        self.q.kind == k
    }
    pub fn is_ep(&self, e: &str) -> bool {
        self.q.ep == e
    }
}

pub(crate) fn can_edit(rt: &RequestTenant) -> bool {
    rt.role >= ConsoleRole::Editor
}

fn base(realm: &str, user: &str) -> String {
    format!("/ui/idp/{realm}/users/{user}/access")
}

// ------------------------------------------------------------- FHIR calls

/// Who reads the endpoints, and how (one per request).
pub(crate) struct Reader<'a> {
    pub st: &'a WebState,
    pub headers: &'a HeaderMap,
    /// The console user (`local:alice`), the subject of minted tokens.
    pub subject: String,
}

impl<'a> Reader<'a> {
    pub fn new(
        st: &'a WebState,
        headers: &'a HeaderMap,
        rt: &RequestTenant,
        principal: Option<&ConsolePrincipal>,
    ) -> Self {
        Self {
            st,
            headers,
            subject: actor(rt, principal).subject,
        }
    }
}

/// Resource types the console reads for this editor (read + search).
const READ_SCOPE: &str = "user/Patient.rs user/Practitioner.rs user/PractitionerRole.rs user/RelatedPerson.rs user/Person.rs";

/// A 60-second access token of the realm `ep` is bound to (Internal
/// security), for the console user: the endpoint only accepts its realm's
/// tokens, so the console's own credential would be refused. Read-only and
/// limited to the linkable types; never leaves the process (the same
/// pattern as the hosted patient picker, `helios_idp_pages::directory`).
fn realm_token(subject: &str, e: &helios_endpoints::Endpoint) -> Option<String> {
    let ep = e.name();
    let realm_name = e.spec.auth.as_ref()?.provider.internal_realm()?;
    let c = helios_idp_admin::service::global()?;
    let realm = c.admin.runtime().realm(realm_name)?;
    let aud = realm.endpoint_base(ep)?.to_string();
    let signer = realm.access_token_signer()?;
    let gid = format!("console-{}", uuid::Uuid::new_v4().simple());
    let claims = helios_idp::token::AccessTokenClaims::build(
        helios_idp::token::AccessTokenInput {
            issuer: realm.issuer(),
            subject: &format!("console:{subject}"),
            audience: vec![aud],
            client_id: "hfs-console-resource-access",
            scope: READ_SCOPE,
            ttl_secs: 60,
            auth_time: None,
            fhir_user: None,
            context: Default::default(),
            cnf: None,
            grant_id: &gid,
        },
        chrono::Utc::now(),
    );
    signer.sign_jws("at+jwt", &claims).ok()
}

/// A FHIR read on endpoint `ep` of this server, as the console user (the
/// browser's or session's credential, else the service credential; a
/// realm-minted read token on an endpoint with Internal security):
/// `Ok(json)` or `Err(status)` (`0` = no answer).
pub(crate) async fn fhir_get(r: &Reader<'_>, ep: &str, path_and_query: &str) -> Result<Value, u16> {
    let st = r.st;
    let Some(runtime) = st.manager.as_ref() else {
        return Err(0);
    };
    let snap = runtime.registry.snapshot();
    let Some(e) = snap.by_name(ep) else {
        return Err(404);
    };
    let scope = crate::manager::EndpointScope {
        endpoint: e.clone(),
        routing: runtime.routing,
        explicit: true,
    };
    let url = format!(
        "{}{}{}",
        st.self_base_url.trim_end_matches('/'),
        scope.self_call_prefix(),
        path_and_query
    );
    let Ok(client) = crate::bulk_export::no_redirect_client() else {
        return Err(0);
    };
    let req = client
        .get(&url)
        .header("Accept", "application/fhir+json")
        .header("X-Tenant-ID", e.tenant_id())
        .timeout(std::time::Duration::from_secs(10));
    let req = match realm_token(&r.subject, e) {
        Some(token) => req.bearer_auth(token),
        None => match crate::bulk_export::forward_credential(st, req, r.headers, &url).await {
            Ok(req) => req,
            Err(_) => return Err(0),
        },
    };
    let resp = req.send().await.map_err(|_| 0u16)?;
    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        return Err(status);
    }
    resp.json().await.map_err(|_| 0u16)
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

/// `name[0]` as text.
fn human_name(r: &Value) -> String {
    let Some(names) = r.get("name").and_then(Value::as_array) else {
        return s(&r["name"]).to_string();
    };
    let n = names
        .iter()
        .find(|n| s(&n["use"]) == "official")
        .or_else(|| names.first());
    let Some(n) = n else {
        return String::new();
    };
    if !s(&n["text"]).is_empty() {
        return s(&n["text"]).to_string();
    }
    let given: Vec<&str> = n["given"]
        .as_array()
        .map(|g| g.iter().map(s).collect())
        .unwrap_or_default();
    let family = s(&n["family"]);
    let mut out = given.join(" ");
    if !family.is_empty() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(family);
    }
    out
}

fn first_identifier(r: &Value) -> String {
    r["identifier"]
        .as_array()
        .and_then(|a| a.first())
        .map(|i| s(&i["value"]).to_string())
        .unwrap_or_default()
}

/// Name and a detail line of a resource.
pub(crate) fn describe(i18n: &I18n, r: &Value) -> (String, String) {
    let t = s(&r["resourceType"]);
    let mut parts: Vec<String> = vec![];
    let name = match t {
        "PractitionerRole" => {
            let p = &r["practitioner"];
            let mut n = s(&p["display"]).to_string();
            if n.is_empty() {
                n = s(&p["reference"]).to_string();
            }
            let code = r["code"]
                .as_array()
                .and_then(|c| c.first())
                .map(|c| {
                    let t = s(&c["text"]);
                    if !t.is_empty() {
                        t.to_string()
                    } else {
                        c["coding"]
                            .as_array()
                            .and_then(|x| x.first())
                            .map(|x| {
                                let d = s(&x["display"]);
                                if d.is_empty() { s(&x["code"]) } else { d }.to_string()
                            })
                            .unwrap_or_default()
                    }
                })
                .unwrap_or_default();
            if !code.is_empty() {
                parts.push(code);
            }
            let org = s(&r["organization"]["display"]);
            if !org.is_empty() {
                parts.push(org.to_string());
            }
            n
        }
        _ => human_name(r),
    };
    let birth = s(&r["birthDate"]);
    if !birth.is_empty() {
        parts.push(i18n.t_arg("idpa-born", "date", birth.to_string()));
    }
    let gender = s(&r["gender"]);
    if !gender.is_empty() && t == "Patient" {
        parts.push(i18n.t(&format!("idpa-gender-{gender}")));
    }
    if t == "RelatedPerson" {
        let p = s(&r["patient"]["display"]);
        let p = if p.is_empty() {
            s(&r["patient"]["reference"])
        } else {
            p
        };
        if !p.is_empty() {
            parts.push(i18n.t_arg("idpa-related-to", "patient", p.to_string()));
        }
    }
    let ident = first_identifier(r);
    if !ident.is_empty() {
        parts.push(ident);
    }
    (name, parts.join(" · "))
}

/// Looks the references up on their endpoints: one `_id` search per
/// (endpoint, type) and 50 ids, at most 8 at a time.
pub(crate) async fn resolve(
    r: &Reader<'_>,
    i18n: &I18n,
    entries: &[(String, String)],
) -> HashMap<(String, String), Resolved> {
    let mut groups: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (ep, r) in entries {
        if let Some((t, id)) = r.split_once('/') {
            groups
                .entry((ep.clone(), t.to_string()))
                .or_default()
                .push(id.to_string());
        }
    }
    let mut jobs = vec![];
    for ((ep, t), ids) in groups {
        for chunk in ids.chunks(50) {
            jobs.push((ep.clone(), t.clone(), chunk.to_vec()));
        }
    }
    let mut out = HashMap::new();
    for batch in jobs.chunks(8) {
        let futs = batch.iter().map(|(ep, t, ids)| async move {
            let path = format!(
                "/{t}?_id={}&_count={}",
                ids.iter()
                    .map(|i| form_urlencoded::byte_serialize(i.as_bytes()).collect::<String>())
                    .collect::<Vec<_>>()
                    .join(","),
                ids.len()
            );
            (ep, t, ids, fhir_get(r, ep, &path).await)
        });
        for (ep, t, ids, res) in join_all(futs).await {
            match res {
                Ok(bundle) => {
                    let mut found: HashMap<String, Value> = HashMap::new();
                    for e in bundle["entry"].as_array().into_iter().flatten() {
                        let r = &e["resource"];
                        if s(&r["resourceType"]) == t.as_str() {
                            found.insert(s(&r["id"]).to_string(), r.clone());
                        }
                    }
                    for id in ids {
                        let key = (ep.clone(), format!("{t}/{id}"));
                        let v = match found.get(id) {
                            Some(r) => {
                                let (name, detail) = describe(i18n, r);
                                Resolved {
                                    name,
                                    detail,
                                    seen: Seen::Found,
                                }
                            }
                            None => Resolved {
                                name: String::new(),
                                detail: String::new(),
                                seen: Seen::Missing,
                            },
                        };
                        out.insert(key, v);
                    }
                }
                Err(code) => {
                    let seen = if matches!(code, 401 | 403) {
                        Seen::Denied
                    } else {
                        Seen::Unknown
                    };
                    for id in ids {
                        out.insert(
                            (ep.clone(), format!("{t}/{id}")),
                            Resolved {
                                name: String::new(),
                                detail: String::new(),
                                seen,
                            },
                        );
                    }
                }
            }
        }
    }
    out
}

/// Runs futures concurrently (a tiny `join_all`).
async fn join_all<F: std::future::Future>(futs: impl IntoIterator<Item = F>) -> Vec<F::Output> {
    let handles: Vec<_> = futs.into_iter().collect();
    let mut pinned: Vec<std::pin::Pin<Box<F>>> = handles.into_iter().map(Box::pin).collect();
    let mut results: Vec<Option<F::Output>> = (0..pinned.len()).map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (i, f) in pinned.iter_mut().enumerate() {
            if results[i].is_none() {
                match f.as_mut().poll(cx) {
                    std::task::Poll::Ready(v) => results[i] = Some(v),
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    results
        .into_iter()
        .map(|r| r.expect("polled to completion"))
        .collect()
}

/// Ids on `ep` whose name matches `q` (the text filter also finds people
/// by name). `None` when the search failed.
async fn name_matches(
    r: &Reader<'_>,
    list: &[AccessEntry],
    q: &str,
) -> Option<HashSet<(String, String)>> {
    let mut pairs: Vec<(String, String)> = list
        .iter()
        .filter(|e| e.resource_type() != "PractitionerRole")
        .map(|e| (e.endpoint.clone(), e.resource_type().to_string()))
        .collect();
    pairs.sort();
    pairs.dedup();
    let enc: String = form_urlencoded::byte_serialize(q.as_bytes()).collect();
    let futs = pairs.iter().map(|(ep, t)| {
        let path = format!("/{t}?name={enc}&_elements=id&_count=1000");
        async move { (ep, t, fhir_get(r, ep, &path).await) }
    });
    let mut out = HashSet::new();
    let mut ok = false;
    for (ep, t, res) in join_all(futs).await {
        if let Ok(b) = res {
            ok = true;
            for e in b["entry"].as_array().into_iter().flatten() {
                out.insert((ep.clone(), format!("{t}/{}", s(&e["resource"]["id"]))));
            }
        }
    }
    (ok || pairs.is_empty()).then_some(out)
}

// ------------------------------------------------------------- table

/// Filters a list by the table query (`None`: the name search failed).
async fn filter(
    r: &Reader<'_>,
    list: Vec<AccessEntry>,
    q: &TableQuery,
) -> (Vec<AccessEntry>, bool) {
    let mut v: Vec<AccessEntry> = list
        .into_iter()
        .filter(|e| q.rtype.is_empty() || e.resource_type() == q.rtype)
        .filter(|e| q.kind.is_empty() || e.kind.as_str() == q.kind)
        .filter(|e| q.ep.is_empty() || e.endpoint == q.ep)
        .collect();
    let text = q.q.trim().to_lowercase();
    let mut failed = false;
    if !text.is_empty() {
        let by_name = if text.chars().count() >= 2
            && text.chars().any(char::is_alphabetic)
            && !text.contains('/')
        {
            let m = name_matches(r, &v, q.q.trim()).await;
            failed = m.is_none();
            m.unwrap_or_default()
        } else {
            HashSet::new()
        };
        v.retain(|e| {
            e.reference.to_lowercase().contains(&text)
                || e.endpoint.to_lowercase().contains(&text)
                || by_name.contains(&e.key())
        });
    }
    match q.sort_key() {
        "endpoint" => {
            v.sort_by(|a, b| (&a.endpoint, &a.reference).cmp(&(&b.endpoint, &b.reference)))
        }
        "kind" => v.sort_by(|a, b| (a.kind, &a.reference).cmp(&(b.kind, &b.reference))),
        _ => v.sort_by(|a, b| {
            (a.resource_type(), natural(a.id()), &a.endpoint).cmp(&(
                b.resource_type(),
                natural(b.id()),
                &b.endpoint,
            ))
        }),
    }
    if q.desc() {
        v.reverse();
    }
    (v, failed)
}

/// Ids sort numerically when they are numbers (`2` before `10`).
fn natural(id: &str) -> (usize, String) {
    if id.bytes().all(|b| b.is_ascii_digit()) {
        (id.len(), id.to_string())
    } else {
        (usize::MAX, id.to_string())
    }
}

pub(crate) async fn table(
    r: &Reader<'_>,
    i18n: I18n,
    c: &IdpConsole,
    rec: &RealmRecord,
    user: &str,
    q: TableQuery,
    can_edit: bool,
) -> Result<AccessTable, OpError> {
    let list = ops::entries(c, rec, user).await?;
    let fingerprint = access::fingerprint(&list);
    let total = list.len();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for e in &list {
        *counts.entry(e.resource_type().to_string()).or_default() += 1;
        *counts.entry(e.kind.as_str().to_string()).or_default() += 1;
    }
    let counts = serde_json::to_string(&counts).unwrap_or_default();
    let (matched, name_search_failed) = filter(r, list, &q).await;
    let size = q.size();
    let matching = matched.len();
    let pages = matching.div_ceil(size).max(1);
    let page = q.page.unwrap_or(1).clamp(1, pages);
    let slice: Vec<AccessEntry> = matched
        .into_iter()
        .skip((page - 1) * size)
        .take(size)
        .collect();
    let keys: Vec<(String, String)> = slice.iter().map(AccessEntry::key).collect();
    let resolved = resolve(r, &i18n, &keys).await;
    let rows = slice
        .into_iter()
        .map(|e| {
            let r = resolved.get(&e.key()).cloned().unwrap_or(Resolved {
                name: String::new(),
                detail: String::new(),
                seen: Seen::Unknown,
            });
            Row {
                key: format!("{}|{}", e.endpoint, e.reference),
                rtype: e.resource_type().to_string(),
                id: e.id().to_string(),
                kind: e.kind.as_str(),
                kind_key: kind_key(e.kind),
                name: r.name,
                detail: r.detail,
                seen: r.seen,
                endpoint: e.endpoint,
                reference: e.reference,
            }
        })
        .collect();
    let realm = rec.spec.name.as_str();
    Ok(AccessTable {
        i18n,
        page_href: format!("/ui/idp/{realm}/users/{user}"),
        can_edit,
        rows,
        total,
        matching,
        page,
        pages,
        from: if matching == 0 {
            0
        } else {
            (page - 1) * size + 1
        },
        to: ((page - 1) * size + size).min(matching),
        q,
        fingerprint,
        counts,
        name_search_failed,
    })
}

/// The card of the user page.
pub(crate) async fn card(
    r: &Reader<'_>,
    locale: RequestLocale,
    rt: &RequestTenant,
    c: &IdpConsole,
    rec: &RealmRecord,
    user: &str,
    q: TableQuery,
) -> AccessCard {
    let edit = can_edit(rt);
    let list = ops::entries(c, rec, user).await.unwrap_or_default();
    let mut types: BTreeMap<String, usize> = BTreeMap::new();
    let mut kinds: BTreeMap<AccessKind, usize> = BTreeMap::new();
    for e in &list {
        *types.entry(e.resource_type().to_string()).or_default() += 1;
        *kinds.entry(e.kind).or_default() += 1;
    }
    let table = match table(r, I18n::new(locale), c, rec, user, q.clone(), edit).await {
        Ok(t) => t.render().unwrap_or_default(),
        Err(e) => format!(
            "<p class=\"notice notice--danger\">{}</p>",
            askama_escape(&e.message)
        ),
    };
    AccessCard {
        base: base(rec.spec.name.as_str(), user),
        can_edit: edit,
        endpoints: rec.spec.endpoints.clone(),
        types: access::LINK_TYPES
            .iter()
            .map(|t| (t.to_string(), types.get(*t).copied().unwrap_or(0)))
            .collect(),
        kinds: [AccessKind::FhirUser, AccessKind::Patient, AccessKind::Link]
            .into_iter()
            .map(|k| (k.as_str(), kind_key(k), kinds.get(&k).copied().unwrap_or(0)))
            .collect(),
        link_types: access::LINK_TYPES.to_vec(),
        table,
        total: list.len(),
        q,
        max_import: access::MAX_IMPORT as i64,
    }
}

fn askama_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

macro_rules! ctx {
    ($realm:expr) => {{
        let c = match console() {
            Ok(c) => c,
            Err(r) => return r,
        };
        match c.realm(&$realm).await {
            Ok(rec) => (c, rec),
            Err(e) => return fail(&e),
        }
    }};
}

fn fail(e: &OpError) -> Response {
    (
        super::op_status(e),
        axum::Json(json!({"error": e.message, "status": e.status, "field": e.field})),
    )
        .into_response()
}

pub(crate) async fn table_fragment(
    State(st): State<WebState>,
    locale: RequestLocale,
    rt: RequestTenant,
    headers: HeaderMap,
    principal: Option<Extension<ConsolePrincipal>>,
    Path((realm, user)): Path<(String, String)>,
    Query(q): Query<TableQuery>,
) -> Response {
    let (c, rec) = ctx!(realm);
    let reader = Reader::new(&st, &headers, &rt, principal.as_ref().map(|p| &p.0));
    match table(
        &reader,
        I18n::new(locale),
        &c,
        &rec,
        &user,
        q,
        can_edit(&rt),
    )
    .await
    {
        Ok(t) => render(t),
        Err(e) => fail(&e),
    }
}

#[derive(Deserialize, Default)]
pub(crate) struct ExportQuery {
    format: Option<String>,
    #[serde(default)]
    download: Option<String>,
}

pub(crate) async fn export(
    Path((realm, user)): Path<(String, String)>,
    Query(q): Query<ExportQuery>,
) -> Response {
    let (c, rec) = ctx!(realm);
    let list = match ops::entries(&c, &rec, &user).await {
        Ok(l) => l,
        Err(e) => return fail(&e),
    };
    let f = q
        .format
        .as_deref()
        .and_then(Format::parse)
        .unwrap_or(Format::Lines);
    let mut headers = vec![
        (header::CONTENT_TYPE, f.media_type().to_string()),
        (header::CACHE_CONTROL, "no-store".to_string()),
        (
            header::HeaderName::from_static("x-access-count"),
            list.len().to_string(),
        ),
    ];
    if q.download.is_some() {
        let username = c
            .admin
            .runtime()
            .store()
            .get_user(&rec.id, &user)
            .await
            .ok()
            .flatten()
            .map(|u| u.username)
            .unwrap_or_else(|| user.clone());
        let safe: String = username
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        headers.push((
            header::CONTENT_DISPOSITION,
            format!(
                "attachment; filename=\"{}-{safe}-access.{}\"",
                rec.spec.name.as_str(),
                f.extension()
            ),
        ));
    }
    let mut r = access::export(&list, f).into_response();
    for (k, v) in headers {
        if let Ok(v) = v.parse() {
            r.headers_mut().insert(k, v);
        }
    }
    r
}

// ------------------------------------------------------------- picker

#[derive(Deserialize, Default)]
pub(crate) struct SearchQuery {
    #[serde(default)]
    ep: String,
    #[serde(default, rename = "type")]
    rtype: String,
    #[serde(default)]
    q: String,
}

pub(crate) struct PickOption {
    pub key: String,
    pub reference: String,
    pub name: String,
    pub detail: String,
    pub granted: Option<&'static str>,
}

#[derive(Template)]
#[template(path = "partials/idp/access-search.html")]
pub(crate) struct SearchResults {
    pub i18n: I18n,
    pub options: Vec<PickOption>,
    /// `idpa-search-…` message key when there is nothing to list.
    pub message: Option<&'static str>,
    pub more: bool,
}

pub(crate) async fn search(
    State(st): State<WebState>,
    locale: RequestLocale,
    rt: RequestTenant,
    headers: HeaderMap,
    principal: Option<Extension<ConsolePrincipal>>,
    Path((realm, user)): Path<(String, String)>,
    Query(q): Query<SearchQuery>,
) -> Response {
    let (c, rec) = ctx!(realm);
    let reader = Reader::new(&st, &headers, &rt, principal.as_ref().map(|p| &p.0));
    let i18n = I18n::new(locale);
    let ep = if q.ep.is_empty() {
        rec.spec.endpoints.first().cloned().unwrap_or_default()
    } else {
        q.ep.clone()
    };
    let out = |options, message, more| {
        render(SearchResults {
            i18n: i18n.clone(),
            options,
            message,
            more,
        })
    };
    if !rec.spec.endpoints.contains(&ep) || !access::LINK_TYPES.contains(&q.rtype.as_str()) {
        return out(vec![], Some("idpa-search-bad"), false);
    }
    let text = q.q.trim();
    if text.chars().count() > 100 {
        return out(vec![], Some("idpa-search-bad"), false);
    }
    let granted: HashMap<(String, String), AccessKind> = ops::entries(&c, &rec, &user)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|e| (e.key(), e.kind))
        .collect();
    let t = q.rtype.as_str();
    let enc: String = form_urlencoded::byte_serialize(text.as_bytes()).collect();
    // A reference or an id: read it; otherwise the newest (empty query),
    // or name / identifier matches.
    let mut paths = vec![];
    let id_like = text
        .rsplit('/')
        .next()
        .filter(|i| access::valid_id(i))
        .map(str::to_string);
    if text.is_empty() {
        paths.push(format!("/{t}?_count=20&_sort=-_lastUpdated"));
    } else {
        if let Some(id) = &id_like {
            paths.push(format!("/{t}?_id={id}"));
        }
        if !text.contains('/') {
            if t != "PractitionerRole" {
                paths.push(format!("/{t}?name={enc}&_count=20"));
            }
            paths.push(format!("/{t}?identifier={enc}&_count=20"));
        }
    }
    let futs = paths.iter().map(|p| fhir_get(&reader, &ep, p));
    let mut options: Vec<PickOption> = vec![];
    let mut seen = HashSet::new();
    let mut denied = false;
    let mut failed = 0;
    let n = paths.len();
    let mut more = false;
    for res in join_all(futs).await {
        match res {
            Ok(b) => {
                if b["link"]
                    .as_array()
                    .is_some_and(|l| l.iter().any(|x| s(&x["relation"]) == "next"))
                {
                    more = true;
                }
                for e in b["entry"].as_array().into_iter().flatten() {
                    let r = &e["resource"];
                    if s(&r["resourceType"]) != t {
                        continue;
                    }
                    let reference = format!("{t}/{}", s(&r["id"]));
                    if !seen.insert(reference.clone()) {
                        continue;
                    }
                    let (name, detail) = describe(&i18n, r);
                    options.push(PickOption {
                        key: format!("{ep}|{reference}"),
                        granted: granted
                            .get(&(ep.clone(), reference.clone()))
                            .map(|k| kind_key(*k)),
                        reference,
                        name,
                        detail,
                    });
                }
            }
            Err(401 | 403) => {
                denied = true;
                failed += 1;
            }
            Err(_) => failed += 1,
        }
    }
    options.truncate(40);
    let message = if !options.is_empty() {
        None
    } else if denied {
        Some("idpa-search-denied")
    } else if failed == n && n > 0 {
        Some("idpa-search-failed")
    } else {
        Some("idpa-search-none")
    };
    out(options, message, more)
}

// ------------------------------------------------------------- paste

/// Paste / import preview.
#[derive(Template)]
#[template(path = "partials/idp/access-preview.html")]
pub(crate) struct Preview {
    pub i18n: I18n,
    pub mode: &'static str,
    pub format: &'static str,
    pub counts: Vec<(&'static str, &'static str, &'static str, usize)>,
    pub rows: Vec<PreviewRow>,
    pub shown: usize,
    pub total_rows: usize,
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
    pub result: usize,
    pub base: String,
    pub skip: String,
    pub error: Option<String>,
    pub checked: bool,
}

pub(crate) struct PreviewRow {
    pub line: usize,
    pub raw: String,
    pub endpoint: String,
    pub reference: String,
    pub kind_key: &'static str,
    pub status: &'static str,
    pub status_key: &'static str,
    pub tone: &'static str,
    pub note: String,
}

fn status_look(code: &str) -> (&'static str, &'static str) {
    match code {
        "new" => ("idpa-pv-new", "ok"),
        "granted" => ("idpa-pv-granted", "muted"),
        "changed" => ("idpa-pv-changed", "info"),
        "invalid" => ("idpa-pv-invalid", "danger"),
        "duplicate" => ("idpa-pv-duplicate", "warn"),
        "conflict" => ("idpa-pv-conflict", "danger"),
        "missing" => ("idpa-pv-missing", "warn"),
        "denied" => ("idpa-pv-denied", "warn"),
        _ => ("idpa-pv-skipped", "muted"),
    }
}

fn issue_text(i18n: &I18n, issue: &access::RowIssue) -> String {
    match issue {
        access::RowIssue::UnsupportedType { resource_type } => {
            i18n.t_arg("idpa-issue-unsupported-type", "type", resource_type.clone())
        }
        access::RowIssue::UnknownEndpoint { endpoint } => {
            i18n.t_arg("idpa-issue-unknown-endpoint", "endpoint", endpoint.clone())
        }
        access::RowIssue::BadKind { kind } => {
            i18n.t_arg("idpa-issue-bad-kind", "kind", kind.clone())
        }
        other => i18n.t(&format!("idpa-issue-{}", other.code())),
    }
}

fn skip_set(text: &str) -> HashSet<(String, String)> {
    text.lines()
        .filter_map(|l| l.split_once('|'))
        .map(|(a, b)| (a.trim().to_string(), b.trim().to_string()))
        .collect()
}

fn mode_of(f: &Form) -> Mode {
    if f.get("mode") == "replace" {
        Mode::Replace
    } else {
        Mode::Add
    }
}

pub(crate) async fn preview(
    State(st): State<WebState>,
    locale: RequestLocale,
    rt: RequestTenant,
    headers: HeaderMap,
    principal: Option<Extension<ConsolePrincipal>>,
    Path((realm, user)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let (c, rec) = ctx!(realm);
    let reader = Reader::new(&st, &headers, &rt, principal.as_ref().map(|p| &p.0));
    let i18n = I18n::new(locale);
    let f = Form::parse(&body);
    let mode = mode_of(&f);
    let check = f.get("check") != "0";
    let error_preview = |msg: String| {
        render(Preview {
            i18n: i18n.clone(),
            mode: "add",
            format: "",
            counts: vec![],
            rows: vec![],
            shown: 0,
            total_rows: 0,
            added: 0,
            removed: 0,
            changed: 0,
            result: 0,
            base: String::new(),
            skip: String::new(),
            error: Some(msg),
            checked: false,
        })
    };
    let (pl, fp) = match ops::plan(
        &c,
        &rec,
        &user,
        f.get("text"),
        mode,
        f.opt("ep").as_deref(),
        &HashSet::new(),
    )
    .await
    {
        Ok(x) => x,
        Err(e) if e.status == 413 => {
            return error_preview(i18n.t_arg("idpa-too-many", "max", access::MAX_IMPORT as i64));
        }
        Err(e) => return error_preview(e.message),
    };
    // New rows: do they exist on the endpoint (as this console user)?
    let mut seen: HashMap<(String, String), Seen> = HashMap::new();
    if check {
        let fresh: Vec<(String, String)> = pl
            .rows
            .iter()
            .filter(|r| matches!(r.status, RowStatus::New | RowStatus::Changed { .. }))
            .map(|r| (r.endpoint.clone(), r.reference.clone()))
            .take(PREVIEW_CHECK)
            .collect();
        for (k, v) in resolve(&reader, &i18n, &fresh).await {
            seen.insert(k, v.seen);
        }
    }
    let mut skip = vec![];
    let mut rows: Vec<PreviewRow> = pl
        .rows
        .iter()
        .map(|r| {
            let key = (r.endpoint.clone(), r.reference.clone());
            let mut code = r.status.code();
            if matches!(r.status, RowStatus::New) {
                match seen.get(&key) {
                    Some(Seen::Missing) => code = "missing",
                    Some(Seen::Denied) => code = "denied",
                    _ => {}
                }
                if matches!(code, "missing") {
                    skip.push(format!("{}|{}", r.endpoint, r.reference));
                }
            }
            let (status_key, tone) = status_look(code);
            let note = match (&r.status, &r.issue) {
                (_, Some(i)) => issue_text(&i18n, i),
                (RowStatus::Changed { from }, _) => {
                    i18n.t_arg("idpa-pv-changed-from", "kind", i18n.t(kind_key(*from)))
                }
                (RowStatus::Conflict, _) => i18n.t("idpa-pv-conflict-note"),
                _ => String::new(),
            };
            PreviewRow {
                line: r.line,
                raw: r.raw.clone(),
                endpoint: r.endpoint.clone(),
                reference: r.reference.clone(),
                kind_key: kind_key(r.kind),
                status: code,
                status_key,
                tone,
                note,
            }
        })
        .collect();
    let order = |s: &str| match s {
        "invalid" | "conflict" => 0,
        "missing" | "denied" => 1,
        "duplicate" => 2,
        "changed" => 3,
        "new" => 4,
        _ => 5,
    };
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for r in &rows {
        *counts.entry(r.status).or_default() += 1;
    }
    rows.sort_by_key(|r| (order(r.status), r.line));
    let total_rows = rows.len();
    rows.truncate(PREVIEW_ROWS);
    let skipped: HashSet<(String, String)> = skip_set(&skip.join("\n"));
    let missing_added = pl
        .added
        .iter()
        .filter(|a| skipped.contains(&a.key()))
        .count();
    let mut counts: Vec<(&'static str, &'static str, &'static str, usize)> = counts
        .into_iter()
        .map(|(code, n)| {
            let (k, tone) = status_look(code);
            (code, k, tone, n)
        })
        .collect();
    counts.sort_by_key(|c| order(c.0));
    render(Preview {
        i18n,
        mode: if mode == Mode::Replace {
            "replace"
        } else {
            "add"
        },
        format: pl.format.as_str(),
        counts,
        shown: rows.len(),
        rows,
        total_rows,
        // What applying does with "skip not found" on (the default).
        added: pl.added.len() - missing_added,
        removed: pl.removed.len(),
        changed: pl.changed.len(),
        result: pl.result.len() - missing_added,
        base: fp,
        skip: skip.join("\n"),
        error: None,
        checked: check,
    })
}

/// Undo payload: what restores `before` from `after`.
fn undo_of(before: &[AccessEntry], after: &[AccessEntry]) -> Value {
    let b: HashMap<(String, String), AccessKind> =
        before.iter().map(|e| (e.key(), e.kind)).collect();
    let a: HashMap<(String, String), AccessKind> =
        after.iter().map(|e| (e.key(), e.kind)).collect();
    let add: Vec<&AccessEntry> = before
        .iter()
        .filter(|e| a.get(&e.key()) != Some(&e.kind))
        .collect();
    let remove: Vec<Value> = after
        .iter()
        .filter(|e| !b.contains_key(&e.key()))
        .map(|e| json!({"endpoint": e.endpoint, "reference": e.reference}))
        .collect();
    json!({"add": add, "remove": remove})
}

async fn after_change(
    c: &IdpConsole,
    rec: &RealmRecord,
    user: &str,
    ch: &helios_idp::admin::AccessChange,
) -> Value {
    let now = ops::entries(c, rec, user).await.unwrap_or_default();
    json!({
        "added": ch.added,
        "removed": ch.removed,
        "changed": ch.changed,
        "total": ch.total,
        "fingerprint": ch.fingerprint,
        "undo": undo_of(&ch.before, &now),
    })
}

pub(crate) async fn apply(
    rt: RequestTenant,
    principal: Option<Extension<ConsolePrincipal>>,
    Path((realm, user)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let (c, rec) = ctx!(realm);
    let f = Form::parse(&body);
    let mode = mode_of(&f);
    let skip = if f.get("include_missing") == "1" {
        HashSet::new()
    } else {
        skip_set(f.get("skip"))
    };
    let (pl, fp) = match ops::plan(
        &c,
        &rec,
        &user,
        f.get("text"),
        mode,
        f.opt("ep").as_deref(),
        &skip,
    )
    .await
    {
        Ok(x) => x,
        Err(e) => return fail(&e),
    };
    let base = f.opt("base");
    if let Some(b) = &base
        && *b != fp
    {
        return fail(&OpError::other(
            409,
            "the user's resource access changed since the preview; preview again",
        ));
    }
    let who = actor(&rt, principal.as_ref().map(|p| &p.0));
    let action = if mode == Mode::Replace {
        "replace"
    } else {
        "add"
    };
    match ops::store(&c, &rec, &user, pl.result.clone(), Some(&fp), &who, action).await {
        Ok(ch) => axum::Json(after_change(&c, &rec, &user, &ch).await).into_response(),
        Err(e) => fail(&e),
    }
}

/// `POST …/access/patch` body.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PatchBody {
    #[serde(default)]
    add: Vec<AccessEntry>,
    #[serde(default)]
    remove: Vec<KeyBody>,
    /// Remove every entry matching this table filter.
    #[serde(default)]
    remove_matching: Option<TableQuery>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    base: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct KeyBody {
    endpoint: String,
    reference: String,
}

pub(crate) async fn patch(
    State(st): State<WebState>,
    rt: RequestTenant,
    headers: HeaderMap,
    principal: Option<Extension<ConsolePrincipal>>,
    Path((realm, user)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let (c, rec) = ctx!(realm);
    let reader = Reader::new(&st, &headers, &rt, principal.as_ref().map(|p| &p.0));
    let Ok(b) = serde_json::from_slice::<PatchBody>(&body) else {
        return fail(&OpError::invalid("body", "expected {add, remove}"));
    };
    for a in &b.add {
        if let Err(e) = access::check(std::slice::from_ref(a)) {
            return fail(&OpError::invalid("add", e.to_string()));
        }
    }
    let mut remove: Vec<(String, String)> = b
        .remove
        .into_iter()
        .map(|k| (k.endpoint, k.reference))
        .collect();
    if let Some(q) = b.remove_matching {
        let list = match ops::entries(&c, &rec, &user).await {
            Ok(l) => l,
            Err(e) => return fail(&e),
        };
        let (m, failed) = filter(&reader, list, &q).await;
        if failed {
            return fail(&OpError::other(
                502,
                "the name search on the endpoint failed; nothing was removed",
            ));
        }
        remove.extend(m.iter().map(AccessEntry::key));
    }
    let action = match b.action.as_deref() {
        Some(a @ ("add" | "remove" | "undo" | "identity")) => a,
        _ => "patch",
    };
    let who = actor(&rt, principal.as_ref().map(|p| &p.0));
    match ops::patch(
        &c,
        &rec,
        &user,
        &b.add,
        &remove,
        b.base.as_deref(),
        &who,
        action,
    )
    .await
    {
        Ok(ch) => axum::Json(after_change(&c, &rec, &user, &ch).await).into_response(),
        Err(e) => fail(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_strings() {
        let q = TableQuery {
            q: "chen wu".into(),
            rtype: "Patient".into(),
            size: Some(50),
            ..Default::default()
        };
        assert_eq!(q.with(2, None), "q=chen+wu&type=Patient&page=2&size=50");
        // Sorting by the current column flips the direction.
        assert_eq!(
            q.with(1, Some("resource")),
            "q=chen+wu&type=Patient&dir=desc&size=50"
        );
        assert_eq!(
            q.with(1, Some("kind")),
            "q=chen+wu&type=Patient&sort=kind&size=50"
        );
        let q = TableQuery {
            size: Some(7),
            ..Default::default()
        };
        assert_eq!(q.size(), 25);
        assert!(!q.filtered());
    }

    #[test]
    fn names_and_details() {
        let i18n = I18n::new(RequestLocale::default());
        let p = json!({"resourceType": "Patient", "id": "1", "gender": "female", "birthDate": "1980-02-03",
            "name": [{"use": "nickname", "text": "Ann"}, {"use": "official", "family": "Chen", "given": ["Mei", "Ling"]}],
            "identifier": [{"value": "A123"}]});
        let (n, d) = describe(&i18n, &p);
        assert_eq!(n, "Mei Ling Chen");
        assert!(d.contains("1980-02-03") && d.contains("A123"), "{d}");
        let r = json!({"resourceType": "PractitionerRole", "practitioner": {"display": "Dr Wu"},
            "code": [{"coding": [{"code": "doctor"}]}], "organization": {"display": "Clinic"}});
        assert_eq!(
            describe(&i18n, &r),
            ("Dr Wu".to_string(), "doctor · Clinic".to_string())
        );
        assert_eq!(natural("10") > natural("9"), true);
        assert!(natural("abc") > natural("99999"));
    }

    #[test]
    fn undo_restores() {
        let e = |r: &str, k| AccessEntry::new("c", r, k);
        let before = vec![
            e("Practitioner/1", AccessKind::FhirUser),
            e("Patient/1", AccessKind::Patient),
        ];
        let after = vec![
            e("Practitioner/1", AccessKind::Link),
            e("Patient/2", AccessKind::Patient),
        ];
        let u = undo_of(&before, &after);
        let add: Vec<AccessEntry> = serde_json::from_value(u["add"].clone()).unwrap();
        let remove: Vec<(String, String)> = u["remove"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| {
                (
                    k["endpoint"].as_str().unwrap().into(),
                    k["reference"].as_str().unwrap().into(),
                )
            })
            .collect();
        let back = access::patch(&after, &add, &remove);
        let mut b = before.clone();
        access::sort(&mut b);
        assert_eq!(back, b);
    }
}
