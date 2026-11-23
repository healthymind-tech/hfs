//! A small browser for the diagnostic: per-run cookie jar, redirects
//! followed by hand (so the authorization response is read from
//! `Location` and never fetched), and HTML form extraction so the real
//! hosted login / consent / patient-picker forms are submitted with the
//! CSRF token and field names the page itself carries.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use reqwest::header::{self, HeaderMap, HeaderValue};
use scraper::{Html, Selector};
use url::Url;

/// One HTTP exchange as the steps see it.
#[derive(Debug, Clone)]
pub struct Exchange {
    pub url: String,
    pub status: u16,
    pub headers: HeaderMap,
    pub body: String,
    pub elapsed: Duration,
}

impl Exchange {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn location(&self) -> Option<&str> {
        self.header("location")
    }

    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_str(&self.body).ok()
    }

    pub fn is_json(&self) -> bool {
        self.header("content-type")
            .is_some_and(|c| c.contains("json"))
    }

    /// Server clock from the `Date` header.
    pub fn date(&self) -> Option<DateTime<Utc>> {
        self.header("date")
            .and_then(|d| DateTime::parse_from_rfc2822(d).ok())
            .map(|d| d.with_timezone(&Utc))
    }
}

/// Transport failure, classified for the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    Connect(String),
    Timeout,
    Tls(String),
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Connect(e) => write!(f, "connection failed: {e}"),
            FetchError::Timeout => write!(f, "timed out"),
            FetchError::Tls(e) => write!(f, "TLS error: {e}"),
            FetchError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl FetchError {
    pub fn code(&self) -> &'static str {
        match self {
            FetchError::Connect(_) => "net-connect",
            FetchError::Timeout => "net-timeout",
            FetchError::Tls(_) => "net-tls",
            FetchError::Other(_) => "net-other",
        }
    }
}

/// The per-run browser.
pub struct Browser {
    client: reqwest::Client,
    /// Cookies by (host, name) → value. Path/expiry are ignored: a run lasts
    /// seconds and talks to one origin.
    jar: BTreeMap<(String, String), String>,
    /// Requests are sent here instead of the public origin when set
    /// (in-network address of the same server); URLs in the report and every
    /// issuer comparison stay public.
    rewrite: Option<(Url, Url)>,
    /// Longest total wait for one request answered 429.
    pub max_rate_wait: Duration,
    /// Total time spent waiting out 429s (reported).
    pub rate_waited: Duration,
    /// A request stayed rate limited after the wait.
    pub rate_limited: bool,
}

impl Browser {
    pub fn new(timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .user_agent("hfs-idp-diagnostic/1")
            .build()
            .expect("reqwest client");
        Self {
            client,
            jar: BTreeMap::new(),
            rewrite: None,
            max_rate_wait: Duration::from_secs(65),
            rate_waited: Duration::ZERO,
            rate_limited: false,
        }
    }

    /// Sends requests for `public` origin URLs to `via` instead (same
    /// path), with the original `Host`/`X-Forwarded-*` headers.
    pub fn with_rewrite(mut self, public: Url, via: Url) -> Self {
        self.rewrite = Some((public, via));
        self
    }

    fn target(&self, url: &str) -> (String, Option<(String, String)>) {
        if let Some((public, via)) = &self.rewrite {
            let pb = public.as_str().trim_end_matches('/');
            if let Some(rest) = url.strip_prefix(pb) {
                let vb = via.as_str().trim_end_matches('/');
                let host = public.host_str().unwrap_or_default().to_string();
                let host = match public.port() {
                    Some(p) => format!("{host}:{p}"),
                    None => host,
                };
                return (
                    format!("{vb}{rest}"),
                    Some((host, public.scheme().to_string())),
                );
            }
        }
        (url.to_string(), None)
    }

    fn cookie_header(&self, url: &str) -> Option<HeaderValue> {
        let host = Url::parse(url).ok()?.host_str()?.to_string();
        let v: Vec<String> = self
            .jar
            .iter()
            .filter(|((h, _), _)| *h == host)
            .map(|((_, n), v)| format!("{n}={v}"))
            .collect();
        if v.is_empty() {
            None
        } else {
            HeaderValue::from_str(&v.join("; ")).ok()
        }
    }

    fn store_cookies(&mut self, url: &str, headers: &HeaderMap) {
        let Some(host) = Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(String::from))
        else {
            return;
        };
        for v in headers.get_all(header::SET_COOKIE) {
            let Ok(s) = v.to_str() else { continue };
            let first = s.split(';').next().unwrap_or_default();
            if let Some((n, val)) = first.split_once('=') {
                let n = n.trim().to_string();
                let lower = s.to_ascii_lowercase();
                if val.is_empty() || lower.contains("max-age=0") {
                    self.jar.remove(&(host.clone(), n));
                } else {
                    self.jar.insert((host.clone(), n), val.trim().to_string());
                }
            }
        }
    }

    pub fn has_cookies(&self) -> bool {
        !self.jar.is_empty()
    }

    pub fn clear_cookies(&mut self) {
        self.jar.clear();
    }

    /// Sends, waiting out `429 Too Many Requests` (the realm's rate limits
    /// count the diagnostic's own sign-ins): honours `Retry-After` up to
    /// [`Browser::max_rate_wait`] in total, then returns the 429.
    async fn send(
        &mut self,
        method: reqwest::Method,
        url: &str,
        headers: Vec<(&str, String)>,
        body: Option<Body>,
    ) -> Result<Exchange, FetchError> {
        let mut waited = Duration::ZERO;
        loop {
            let x = self
                .send_once(method.clone(), url, headers.clone(), body.clone())
                .await?;
            if x.status != 429 {
                return Ok(x);
            }
            let secs = x
                .header("retry-after")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(5)
                .clamp(1, 30);
            let wait = Duration::from_secs(secs);
            if waited + wait > self.max_rate_wait {
                self.rate_limited = true;
                return Ok(x);
            }
            waited += wait;
            self.rate_waited += wait;
            tokio::time::sleep(wait).await;
        }
    }

    async fn send_once(
        &mut self,
        method: reqwest::Method,
        url: &str,
        headers: Vec<(&str, String)>,
        body: Option<Body>,
    ) -> Result<Exchange, FetchError> {
        let (target, forwarded) = self.target(url);
        let mut req = self.client.request(method, &target);
        if let Some((host, proto)) = forwarded {
            req = req
                .header(header::HOST, host.clone())
                .header("x-forwarded-host", host)
                .header("x-forwarded-proto", proto);
        }
        if let Some(c) = self.cookie_header(url) {
            req = req.header(header::COOKIE, c);
        }
        for (k, v) in headers {
            req = req.header(k, v);
        }
        match body {
            Some(Body::Form(pairs)) => req = req.form(&pairs),
            Some(Body::Raw(ct, b)) => req = req.header(header::CONTENT_TYPE, ct).body(b),
            None => {}
        }
        let start = Instant::now();
        let resp = req.send().await.map_err(classify)?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        self.store_cookies(url, &headers);
        let body = resp.text().await.map_err(classify)?;
        Ok(Exchange {
            url: url.to_string(),
            status,
            headers,
            body,
            elapsed: start.elapsed(),
        })
    }

    pub async fn get(&mut self, url: &str) -> Result<Exchange, FetchError> {
        self.send(reqwest::Method::GET, url, vec![], None).await
    }

    pub async fn get_with(
        &mut self,
        url: &str,
        headers: Vec<(&str, String)>,
    ) -> Result<Exchange, FetchError> {
        self.send(reqwest::Method::GET, url, headers, None).await
    }

    pub async fn post_form(
        &mut self,
        url: &str,
        pairs: Vec<(String, String)>,
        headers: Vec<(&str, String)>,
    ) -> Result<Exchange, FetchError> {
        self.send(reqwest::Method::POST, url, headers, Some(Body::Form(pairs)))
            .await
    }

    pub async fn post_raw(
        &mut self,
        url: &str,
        content_type: &str,
        body: String,
        headers: Vec<(&str, String)>,
    ) -> Result<Exchange, FetchError> {
        self.send(
            reqwest::Method::POST,
            url,
            headers,
            Some(Body::Raw(content_type.to_string(), body)),
        )
        .await
    }
}

#[derive(Clone)]
enum Body {
    Form(Vec<(String, String)>),
    Raw(String, String),
}

fn classify(e: reqwest::Error) -> FetchError {
    if e.is_timeout() {
        return FetchError::Timeout;
    }
    let s = format!("{e:#}");
    let chain = {
        let mut out = s.clone();
        let mut src = std::error::Error::source(&e);
        while let Some(x) = src {
            out.push_str(": ");
            out.push_str(&x.to_string());
            src = x.source();
        }
        out
    };
    if chain.to_ascii_lowercase().contains("certificate") || chain.contains("tls") {
        FetchError::Tls(chain)
    } else if e.is_connect() {
        FetchError::Connect(chain)
    } else {
        FetchError::Other(chain)
    }
}

/// Resolves a possibly relative `Location` / form action against a page URL.
pub fn resolve(base: &str, reference: &str) -> Option<String> {
    Url::parse(base)
        .ok()?
        .join(reference)
        .ok()
        .map(|u| u.to_string())
}

/// An HTML form as found on a hosted page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    pub action: String,
    pub method: String,
    /// Inputs with their default values (hidden fields, checked boxes and
    /// radios, selected options), in document order.
    pub fields: Vec<(String, String)>,
    /// Name of the password input, if any.
    pub password_field: Option<String>,
    /// Name of the username-like input (first text/email input).
    pub username_field: Option<String>,
    /// Submit buttons: (name, value, label) — buttons without a name too.
    pub submits: Vec<(String, String, String)>,
    /// Radio groups / selects: name → available values.
    pub choices: BTreeMap<String, Vec<String>>,
}

impl Form {
    pub fn set(&mut self, name: &str, value: &str) {
        if let Some(f) = self.fields.iter_mut().find(|(n, _)| n == name) {
            f.1 = value.to_string();
        } else {
            self.fields.push((name.to_string(), value.to_string()));
        }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The submit button whose name/value/label best means "approve".
    pub fn approve_button(&self) -> Option<(String, String)> {
        const YES: &[&str] = &[
            "approve",
            "allow",
            "accept",
            "grant",
            "consent",
            "yes",
            "continue",
            "authorize",
            "submit",
            "select",
        ];
        const NO: &[&str] = &["deny", "reject", "cancel", "decline", "no"];
        let score = |s: &str| {
            let s = s.to_ascii_lowercase();
            if NO.iter().any(|w| s.contains(w)) {
                -1
            } else if YES.iter().any(|w| s.contains(w)) {
                1
            } else {
                0
            }
        };
        self.submits
            .iter()
            .filter(|(n, v, l)| {
                score(n) + score(v) + score(l) >= 0 && !(score(n) < 0 || score(v) < 0)
            })
            .max_by_key(|(n, v, l)| score(n) + score(v) + score(l))
            .map(|(n, v, _)| (n.clone(), v.clone()))
    }
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("static selector")
}

/// Every form on a page.
pub fn forms(html: &str, page_url: &str) -> Vec<Form> {
    let doc = Html::parse_document(html);
    let mut out = Vec::new();
    for f in doc.select(&sel("form")) {
        let action = f.value().attr("action").unwrap_or("");
        let action = if action.is_empty() {
            page_url.to_string()
        } else {
            resolve(page_url, action).unwrap_or_else(|| action.to_string())
        };
        let method = f
            .value()
            .attr("method")
            .unwrap_or("get")
            .to_ascii_lowercase();
        let mut form = Form {
            action,
            method,
            fields: Vec::new(),
            password_field: None,
            username_field: None,
            submits: Vec::new(),
            choices: BTreeMap::new(),
        };
        for el in f.select(&sel("input, select, textarea, button")) {
            let v = el.value();
            let name = v.attr("name").unwrap_or("").to_string();
            match v.name() {
                "input" => {
                    let ty = v.attr("type").unwrap_or("text").to_ascii_lowercase();
                    let value = v.attr("value").unwrap_or("").to_string();
                    match ty.as_str() {
                        "password" => {
                            if form.password_field.is_none() && !name.is_empty() {
                                form.password_field = Some(name.clone());
                            }
                            form.fields.push((name, String::new()));
                        }
                        "submit" => {
                            form.submits.push((name, value.clone(), value));
                        }
                        "checkbox" => {
                            if v.attr("checked").is_some() && !name.is_empty() {
                                form.fields.push((
                                    name,
                                    if value.is_empty() { "on".into() } else { value },
                                ));
                            }
                        }
                        "radio" => {
                            if name.is_empty() {
                                continue;
                            }
                            form.choices
                                .entry(name.clone())
                                .or_default()
                                .push(value.clone());
                            if v.attr("checked").is_some() {
                                form.fields.push((name, value));
                            }
                        }
                        "button" | "reset" | "image" | "file" => {}
                        _ => {
                            if name.is_empty() {
                                continue;
                            }
                            if form.username_field.is_none()
                                && matches!(ty.as_str(), "text" | "email")
                            {
                                form.username_field = Some(name.clone());
                            }
                            form.fields.push((name, value));
                        }
                    }
                }
                "select" => {
                    if name.is_empty() {
                        continue;
                    }
                    let mut values = Vec::new();
                    let mut chosen = None;
                    for o in el.select(&sel("option")) {
                        let val = o
                            .value()
                            .attr("value")
                            .map(String::from)
                            .unwrap_or_else(|| o.text().collect::<String>().trim().to_string());
                        if o.value().attr("selected").is_some() {
                            chosen = Some(val.clone());
                        }
                        values.push(val);
                    }
                    let first = values.iter().find(|v| !v.is_empty()).cloned();
                    form.choices.insert(name.clone(), values);
                    if let Some(c) = chosen.or(first) {
                        form.fields.push((name, c));
                    }
                }
                "textarea" => {
                    if !name.is_empty() {
                        form.fields.push((name, el.text().collect()));
                    }
                }
                "button" => {
                    let ty = v.attr("type").unwrap_or("submit");
                    if ty == "submit" {
                        let label = el.text().collect::<String>().trim().to_string();
                        form.submits
                            .push((name, v.attr("value").unwrap_or("").to_string(), label));
                    }
                }
                _ => {}
            }
        }
        out.push(form);
    }
    out
}

/// What kind of hosted page a response is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageKind {
    Login(Form),
    PatientPicker(Form),
    Consent(Form),
    Other,
}

/// Classifies a hosted page by its forms: a password input = login; a
/// `patient` choice = picker; another POST form with an approve button =
/// consent.
pub fn classify_page(html: &str, page_url: &str) -> PageKind {
    let fs = forms(html, page_url);
    if let Some(f) = fs.iter().find(|f| f.password_field.is_some()) {
        return PageKind::Login(f.clone());
    }
    if let Some(f) = fs.iter().find(|f| {
        f.choices
            .keys()
            .any(|k| k == "patient" || k.ends_with("patient"))
    }) {
        return PageKind::PatientPicker(f.clone());
    }
    if let Some(f) = fs
        .iter()
        .find(|f| f.method == "post" && f.approve_button().is_some())
    {
        return PageKind::Consent(f.clone());
    }
    PageKind::Other
}

/// Visible text of a page, collapsed (for error-page messages in the report).
pub fn page_text(html: &str, max: usize) -> String {
    let doc = Html::parse_document(html);
    let mut s = String::new();
    for sel_str in ["main", "body"] {
        if let Some(n) = doc.select(&sel(sel_str)).next() {
            s = n.text().collect::<Vec<_>>().join(" ");
            break;
        }
    }
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    s.chars().take(max).collect()
}

/// Query parameters of a URL as a map (last value wins).
pub fn query_map(url: &str) -> BTreeMap<String, String> {
    Url::parse(url)
        .map(|u| u.query_pairs().into_owned().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOGIN: &str = r#"<html><body><main><h1>Sign in</h1>
      <form method="post" action="/idp/p/login?x=1">
        <input type="hidden" name="_csrf" value="tok123">
        <input type="hidden" name="tx" value="abc">
        <input type="text" name="username" autocomplete="username">
        <input type="password" name="password">
        <button type="submit">Sign in</button>
      </form></main></body></html>"#;

    const CONSENT: &str = r#"<form method="post" action="consent">
        <input type="hidden" name="_csrf" value="c">
        <input type="checkbox" name="scope" value="openid" checked>
        <input type="checkbox" name="scope" value="patient/*.rs" checked>
        <button name="decision" value="deny">Deny</button>
        <button name="decision" value="approve">Allow</button>
      </form>"#;

    const PICKER: &str = r#"<form method="post" action="/idp/p/select-patient">
        <input type="hidden" name="_csrf" value="c">
        <label><input type="radio" name="patient" value="p1"> A</label>
        <label><input type="radio" name="patient" value="p2"> B</label>
        <button type="submit">Continue</button></form>"#;

    #[test]
    fn login_form_is_found_with_csrf_and_fields() {
        match classify_page(LOGIN, "https://h/idp/p/authorize?client_id=x") {
            PageKind::Login(f) => {
                assert_eq!(f.action, "https://h/idp/p/login?x=1");
                assert_eq!(f.get("_csrf"), Some("tok123"));
                assert_eq!(f.username_field.as_deref(), Some("username"));
                assert_eq!(f.password_field.as_deref(), Some("password"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn consent_and_picker() {
        match classify_page(CONSENT, "https://h/idp/p/consent?tx=1") {
            PageKind::Consent(f) => {
                assert_eq!(f.action, "https://h/idp/p/consent");
                assert_eq!(
                    f.approve_button(),
                    Some(("decision".into(), "approve".into()))
                );
                assert_eq!(f.fields.iter().filter(|(n, _)| n == "scope").count(), 2);
            }
            other => panic!("{other:?}"),
        }
        match classify_page(PICKER, "https://h/x") {
            PageKind::PatientPicker(f) => {
                assert_eq!(f.choices["patient"], vec!["p1", "p2"]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(classify_page("<p>error</p>", "https://h"), PageKind::Other);
    }

    #[test]
    fn resolve_and_query() {
        assert_eq!(
            resolve("https://h/idp/p/authorize", "login?a=1").unwrap(),
            "https://h/idp/p/login?a=1"
        );
        let q = query_map("https://app/cb?code=c1&state=s&iss=https%3A%2F%2Fh%2Fidp%2Fp");
        assert_eq!(q["iss"], "https://h/idp/p");
    }

    #[test]
    fn rewrite_targets_in_network_address() {
        let b = Browser::new(Duration::from_secs(1)).with_rewrite(
            Url::parse("https://hfs.example.org").unwrap(),
            Url::parse("http://127.0.0.1:8080").unwrap(),
        );
        let (t, fwd) = b.target("https://hfs.example.org/idp/p/jwks");
        assert_eq!(t, "http://127.0.0.1:8080/idp/p/jwks");
        assert_eq!(fwd.unwrap().0, "hfs.example.org");
        let (t, fwd) = b.target("https://other.example/x");
        assert_eq!(t, "https://other.example/x");
        assert!(fwd.is_none());
    }
}
