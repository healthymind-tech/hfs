//! Persistent-shell plumbing on the server side (unified-server design
//! §9.4): the build id, fingerprinted immutable asset URLs, the build-change
//! refresh, the Content-Security-Policy, and `Vary` for in-place
//! navigation.
//!
//! The browser side is `assets/shell.js`.

use std::sync::OnceLock;

use axum::{
    extract::Request,
    http::{HeaderValue, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::Assets;

/// Identifies this build's assets: the server version plus a digest of
/// every embedded asset, so a rebuilt asset gets a new URL.
pub(crate) fn build_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        // FNV-1a over names and bytes: stable across processes of one build.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut feed = |bytes: &[u8]| {
            for b in bytes {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        };
        feed(env!("CARGO_PKG_VERSION").as_bytes());
        let mut names: Vec<_> = Assets::iter().collect();
        names.sort();
        for name in names {
            feed(name.as_bytes());
            if let Some(file) = Assets::get(&name) {
                feed(&file.data);
            }
        }
        format!("{h:016x}")[..12].to_string()
    })
}

/// Prefix of the fingerprinted asset URLs: `/ui/assets/v/{build}`.
pub(crate) fn asset_base() -> &'static str {
    static BASE: OnceLock<String> = OnceLock::new();
    BASE.get_or_init(|| format!("/ui/assets/v/{}", build_id()))
}

/// Middleware on `/ui/assets`: `/ui/assets/v/{build}/{path}` is the same
/// file as `/ui/assets/{path}`, cached for a year (`immutable`) when the
/// build matches; the unversioned path keeps `no-cache` (revalidation).
pub(crate) async fn versioned_assets(mut request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    let Some(rest) = path.strip_prefix("/ui/assets/v/") else {
        let mut response = next.run(request).await;
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        return response;
    };
    let (build, tail) = rest.split_once('/').unwrap_or((rest, ""));
    let current = build == build_id();
    let target = match request.uri().query() {
        Some(q) => format!("/ui/assets/{tail}?{q}"),
        None => format!("/ui/assets/{tail}"),
    };
    match target.parse() {
        Ok(uri) => *request.uri_mut() = uri,
        Err(_) => return axum::http::StatusCode::NOT_FOUND.into_response(),
    }
    let mut response = next.run(request).await;
    let value = if current && response.status().is_success() {
        HeaderValue::from_static("public, max-age=31536000, immutable")
    } else {
        HeaderValue::from_static("no-cache")
    };
    response.headers_mut().insert(header::CACHE_CONTROL, value);
    response
}

/// The SHA-256 of the inline first-paint theme script, for the CSP.
fn theme_boot_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| {
        let html = helios_ui_chrome::theme_boot().unwrap_or_default();
        let script = html
            .split_once("<script>")
            .and_then(|(_, rest)| rest.split_once("</script>"))
            .map(|(body, _)| body)
            .unwrap_or("");
        use sha2::Digest;
        let digest = sha2::Sha256::digest(script.as_bytes());
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(digest)
    })
}

/// The console's Content-Security-Policy (design §12): scripts only from
/// this origin (plus the hashed first-paint theme stamp), no plugins, no
/// framing. Styles allow inline attributes (`style=` on a few elements).
pub(crate) fn content_security_policy() -> &'static str {
    static CSP: OnceLock<String> = OnceLock::new();
    CSP.get_or_init(|| {
        format!(
            "default-src 'self'; script-src 'self' 'sha256-{}'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data: https:; font-src 'self' data:; connect-src 'self'; \
             object-src 'none'; base-uri 'self'; frame-ancestors 'none'",
            theme_boot_hash()
        )
    })
}

/// Middleware: a boosted request (`HX-Boosted: true`, an in-place page
/// navigation of the shell) reaches the handlers without `HX-Request` /
/// `HX-Target` / `HX-Trigger`, so every handler renders its full page; the
/// shell keeps `#main` of it.
pub(crate) async fn boosted_is_a_page(mut request: Request, next: Next) -> Response {
    if request.headers().contains_key("hx-boosted")
        || request.headers().contains_key("hx-history-restore-request")
    {
        let h = request.headers_mut();
        for name in [
            "hx-request",
            "hx-target",
            "hx-trigger",
            "hx-trigger-name",
            "hx-current-url",
        ] {
            h.remove(name);
        }
    }
    next.run(request).await
}

/// Middleware on every `/ui` response:
///
/// * HTML pages get the CSP and `Vary: HX-Request, HX-Boosted`;
/// * an htmx request from a page of another build (`X-HM-Build` differs)
///   answers `HX-Refresh: true`, so stale shell scripts never talk to a new
///   server (§9.4 item 9).
pub(crate) async fn shell_headers(request: Request, next: Next) -> Response {
    let stale = request
        .headers()
        .get("x-hm-build")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|b| b != build_id());
    let htmx = request.headers().contains_key("hx-request");
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    if stale && htmx {
        headers.insert("hx-refresh", HeaderValue::from_static("true"));
    }
    let is_html = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/html"));
    if is_html {
        if !headers.contains_key(header::CONTENT_SECURITY_POLICY)
            && let Ok(v) = HeaderValue::from_str(content_security_policy())
        {
            headers.insert(header::CONTENT_SECURITY_POLICY, v);
        }
        headers.append(header::VARY, HeaderValue::from_static("HX-Boosted"));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_id_is_stable_and_short() {
        assert_eq!(build_id(), build_id());
        assert_eq!(build_id().len(), 12);
        assert!(asset_base().ends_with(build_id()));
    }

    #[tokio::test]
    async fn a_boosted_request_reaches_the_handler_as_a_page_request() {
        use axum::{Router, body::Body, routing::get};
        let app = Router::new()
            .route(
                "/x",
                get(|h: axum::http::HeaderMap| async move {
                    if h.contains_key("hx-request") {
                        "fragment"
                    } else {
                        "page"
                    }
                }),
            )
            .layer(axum::middleware::from_fn(boosted_is_a_page));
        for (boosted, want) in [(true, "page"), (false, "fragment")] {
            let mut req = Request::builder().uri("/x").header("hx-request", "true");
            if boosted {
                req = req.header("hx-boosted", "true");
            }
            let resp = tower::ServiceExt::oneshot(app.clone(), req.body(Body::empty()).unwrap())
                .await
                .unwrap();
            let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
            assert_eq!(&body[..], want.as_bytes());
        }
    }

    #[test]
    fn the_csp_allows_only_the_theme_stamp_inline() {
        let csp = content_security_policy();
        assert!(csp.contains("script-src 'self' 'sha256-"));
        assert!(!csp.contains("unsafe-eval"));
        assert!(!theme_boot_hash().is_empty());
    }
}
