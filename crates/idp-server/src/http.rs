//! HTTP plumbing shared by the endpoints: parameter parsing (duplicates are
//! errors, RFC 6749 §3.1/§3.2), error responses, CORS, security headers.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use helios_idp::runtime::RealmRuntime;
use helios_idp::{OAuthError, OAuthErrorCode};
use serde_json::Value;

use crate::IdpServer;

/// The client's address, inserted as a request extension by the
/// composition root (behind a trusted proxy). Falls back to the socket peer.
#[derive(Debug, Clone)]
pub struct ClientIp(pub String);

pub(crate) fn client_ip(req_ext: &axum::http::Extensions) -> Option<String> {
    if let Some(ClientIp(ip)) = req_ext.get::<ClientIp>() {
        return Some(ip.clone());
    }
    req_ext
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string())
}

/// Form or query parameters with duplicate detection.
#[derive(Debug, Default, Clone)]
pub(crate) struct Params {
    map: HashMap<String, String>,
    dups: Vec<String>,
}

impl Params {
    pub fn parse(raw: &[u8]) -> Self {
        let mut p = Params::default();
        for (k, v) in url::form_urlencoded::parse(raw) {
            let k = k.into_owned();
            if p.map.contains_key(&k) {
                if !p.dups.contains(&k) {
                    p.dups.push(k);
                }
            } else {
                p.map.insert(k, v.into_owned());
            }
        }
        p
    }

    /// A single-valued parameter; empty values count as absent (RFC 6749
    /// §3.1: "Parameters sent without a value MUST be treated as if they
    /// were omitted").
    pub fn get(&self, k: &str) -> Option<&str> {
        self.map
            .get(k)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    pub fn is_dup(&self, k: &str) -> bool {
        self.dups.iter().any(|d| d == k)
    }

    pub fn first_dup(&self) -> Option<&str> {
        self.dups.first().map(String::as_str)
    }

    pub fn has(&self, k: &str) -> bool {
        self.get(k).is_some()
    }
}

pub(crate) fn is_form(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/x-www-form-urlencoded")
        })
        .unwrap_or(false)
}

fn no_store(h: &mut HeaderMap) {
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
}

/// JSON 200 with `Cache-Control: no-store` / `Pragma: no-cache`.
pub(crate) fn json_no_store(v: Value) -> Response {
    let mut r = (StatusCode::OK, axum::Json(v)).into_response();
    no_store(r.headers_mut());
    r
}

fn quote(s: &str) -> String {
    s.replace(['"', '\\', '\r', '\n'], "")
}

/// Token/introspection/revocation endpoint error (RFC 6749 §5.2). Every
/// `invalid_client` is a 401 with a `Basic` challenge (§5.2: required when
/// the client used the Authorization header; harmless otherwise).
pub(crate) fn oauth_error(e: &OAuthError, realm_issuer: &str) -> Response {
    let status =
        StatusCode::from_u16(e.code.token_endpoint_status()).unwrap_or(StatusCode::BAD_REQUEST);
    let mut r = (
        status,
        axum::Json(serde_json::to_value(e).unwrap_or_default()),
    )
        .into_response();
    no_store(r.headers_mut());
    if e.code == OAuthErrorCode::InvalidClient
        && let Ok(v) = HeaderValue::from_str(&format!("Basic realm=\"{}\"", quote(realm_issuer)))
    {
        r.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    r
}

/// Protected-resource style error (userinfo, bearer-authenticated
/// introspection): RFC 6750 §3.
pub(crate) fn bearer_error(
    status: StatusCode,
    code: Option<OAuthErrorCode>,
    desc: &str,
    realm_issuer: &str,
) -> Response {
    let mut challenge = format!("Bearer realm=\"{}\"", quote(realm_issuer));
    let mut body = serde_json::Map::new();
    if let Some(c) = code {
        challenge.push_str(&format!(
            ", error=\"{}\", error_description=\"{}\"",
            c.as_str(),
            quote(desc)
        ));
        body.insert("error".into(), c.as_str().into());
        body.insert("error_description".into(), desc.into());
    }
    let mut r = (status, axum::Json(Value::Object(body))).into_response();
    no_store(r.headers_mut());
    if let Ok(v) = HeaderValue::from_str(&challenge) {
        r.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    r
}

pub(crate) fn html_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

/// The error page of the authorization endpoint for requests that must not
/// be redirected (unknown client, unregistered redirect URI — RFC 6749
/// §4.1.2.1, RFC 9700 §4.1.3).
pub(crate) fn error_page(status: StatusCode, err: &OAuthError) -> Response {
    let body = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Authorization error</title></head><body>\
         <main><h1>The sign-in request cannot be processed</h1>\
         <p>{}</p><p><code>{}</code></p>\
         <p>Return to the application and try again. If the problem persists, contact the application's provider.</p>\
         </main></body></html>",
        html_escape(&err.description),
        html_escape(err.code.as_str())
    );
    let mut r = (status, body).into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'; base-uri 'none'"),
    );
    no_store(h);
    r
}

/// The realm, if it exists and is enabled.
pub(crate) fn realm(server: &IdpServer, name: &str) -> Option<Arc<RealmRuntime>> {
    server.inner.rt.realm(name).filter(|r| r.spec.enabled)
}

pub(crate) fn not_found() -> Response {
    let mut r = (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({"error": "not_found", "error_description": "unknown authorization server"})),
    )
        .into_response();
    no_store(r.headers_mut());
    r
}

/// CORS for the non-interactive endpoints (no cookies, so `*` is safe).
pub(crate) async fn cors(req: Request<Body>, next: Next) -> Response {
    if req.method() == Method::OPTIONS {
        let mut r = StatusCode::NO_CONTENT.into_response();
        let h = r.headers_mut();
        h.insert("access-control-allow-origin", HeaderValue::from_static("*"));
        h.insert(
            "access-control-allow-methods",
            HeaderValue::from_static("GET, POST, OPTIONS"),
        );
        h.insert(
            "access-control-allow-headers",
            HeaderValue::from_static("Authorization, Content-Type, Accept, DPoP"),
        );
        h.insert("access-control-max-age", HeaderValue::from_static("600"));
        return r;
    }
    let mut r = next.run(req).await;
    let h = r.headers_mut();
    h.insert("access-control-allow-origin", HeaderValue::from_static("*"));
    h.insert(
        "access-control-expose-headers",
        HeaderValue::from_static("WWW-Authenticate"),
    );
    r
}

pub(crate) async fn security_headers(req: Request<Body>, next: Next) -> Response {
    let mut r = next.run(req).await;
    let h = r.headers_mut();
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_detect_duplicates_and_treat_empty_as_absent() {
        let p = Params::parse(b"a=1&b=&a=2&c=x%20y");
        assert!(p.is_dup("a"));
        assert_eq!(p.get("b"), None);
        assert_eq!(p.get("c"), Some("x y"));
        assert_eq!(p.first_dup(), Some("a"));
    }
}
