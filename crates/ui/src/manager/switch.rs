//! The endpoint switcher keeps the page (console redesign, follow-up f):
//! `GET /ui/e/{ep}/switch?from=<path?query>` answers with the same sub-page
//! on endpoint `{ep}`.
//!
//! - A page outside any endpoint (Overview, Terminology, Settings) stays
//!   where it is; only the remembered sidebar endpoint changes.
//! - An endpoint page maps to the same route under `{ep}`. Query parameters
//!   that mean the same on every endpoint (a resource type, a filter, a tab)
//!   are kept; ids that belonged to the old endpoint (a resource, a job, a
//!   version) are dropped and the list page opens instead.
//! - A page `{ep}` does not have, or one the operator's role cannot open
//!   there, falls back to the nearest parent that exists, with a toast.
//!
//! What exists on `{ep}` is the sidebar's own answer: the entries
//! [`super::nav::build`] gives this operator on that endpoint, with every
//! page each one covers.

use axum::extract::{Path, Query};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Router, routing::get};
use serde::Deserialize;

use crate::i18n::{I18n, RequestLocale};
use crate::{RequestTenant, WebState};

pub(crate) fn routes() -> Router<WebState> {
    Router::new().route("/ui/e/{ep}/switch", get(switch))
}

/// Query parameters that mean the same thing on every endpoint.
const KEEP: &[&str] = &[
    "type", "q", "sort", "tab", "view", "status", "filter", "kind", "scope", "window", "series",
    "format", "mode", "_count",
];

/// Where switching to `target` from `from` lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mapped {
    pub href: String,
    /// The page does not exist on `target` (or the role cannot open it):
    /// the nearest parent opened instead.
    pub fell_back: bool,
}

/// One page the sidebar can open on the target endpoint: a path prefix it
/// covers and the entry's own page (the list a deeper page belongs to).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reachable {
    pub prefix: String,
    pub href: String,
}

/// The pages `nav` reaches, every covered prefix with its entry's page.
pub(crate) fn reachable(nav: &[super::nav::NavGroup]) -> Vec<Reachable> {
    fn walk(link: &helios_ui_chrome::nav::NavLink, out: &mut Vec<Reachable>) {
        let href = link.href.split(['?', '#']).next().unwrap_or("").to_string();
        if link.children.is_empty() {
            for prefix in link.match_prefix.split(' ').filter(|p| !p.is_empty()) {
                out.push(Reachable {
                    prefix: prefix.trim_end_matches('/').to_string(),
                    href: href.clone(),
                });
            }
            if !href.is_empty() {
                out.push(Reachable {
                    prefix: href.clone(),
                    href: href.clone(),
                });
            }
        }
        for child in &link.children {
            walk(child, out);
        }
    }
    let mut out = Vec::new();
    for group in nav {
        for link in &group.links {
            walk(link, &mut out);
        }
    }
    out
}

fn under(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|r| r.starts_with('/'))
}

/// Map `from` onto endpoint `target`. `pages` are the pages reachable on
/// `target` ([`reachable`]).
pub(crate) fn map_route(from: &str, target: &str, pages: &[Reachable]) -> Mapped {
    let home = format!("/ui/e/{target}");
    let (path, query) = from.split_once('?').unwrap_or((from, ""));
    let path = path.split('#').next().unwrap_or(path).trim_end_matches('/');
    if !(path == "/ui" || path.starts_with("/ui/")) {
        return Mapped {
            href: home,
            fell_back: false,
        };
    }
    // Outside any endpoint: stay (the sidebar follows the remembered
    // endpoint).
    let Some(rest) = path.strip_prefix("/ui/e/") else {
        return Mapped {
            href: from.to_string(),
            fell_back: false,
        };
    };
    let sub = rest.split_once('/').map_or("", |(_, s)| s);
    let mapped = if sub.is_empty() {
        home.clone()
    } else {
        format!("{home}/{sub}")
    };
    let pairs: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let kept: Vec<&(String, String)> = pairs
        .iter()
        .filter(|(k, _)| KEEP.contains(&k.as_str()))
        .collect();
    let dropped_ids = kept.len() < pairs.len();
    let with_query = |p: &str| {
        if kept.is_empty() {
            p.to_string()
        } else {
            let q: String = form_urlencoded::Serializer::new(String::new())
                .extend_pairs(kept.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .finish();
            format!("{p}?{q}")
        }
    };
    // The deepest page the target reaches that covers the mapped path.
    let best = pages
        .iter()
        .filter(|r| r.prefix.starts_with(&home) && under(&mapped, &r.prefix))
        .max_by_key(|r| r.prefix.len());
    match best {
        // The page itself (a list, a settings section): keep the
        // endpoint-independent query.
        Some(r) if r.prefix == mapped && !dropped_ids => Mapped {
            href: with_query(&mapped),
            fell_back: false,
        },
        // Deeper than any page the sidebar knows (`…/jobs/{id}`), or a page
        // opened on one of the old endpoint's ids: its list.
        Some(r) => Mapped {
            href: with_query(&r.href),
            fell_back: false,
        },
        // Not reachable on the target at all: the nearest parent that is.
        None => {
            let mut p = mapped.as_str();
            while let Some((parent, _)) = p.rsplit_once('/') {
                p = parent;
                if p.len() < home.len() {
                    break;
                }
                if p == home || pages.iter().any(|r| r.prefix == p) {
                    return Mapped {
                        href: p.to_string(),
                        fell_back: true,
                    };
                }
            }
            Mapped {
                href: home,
                fell_back: true,
            }
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct SwitchQuery {
    #[serde(default)]
    from: String,
}

/// Cookie the shell turns into a toast after the navigation (`shell.js`).
pub(crate) const TOAST_COOKIE: &str = "hm_toast";

async fn switch(
    locale: RequestLocale,
    rt: RequestTenant,
    Path(ep): Path<String>,
    Query(q): Query<SwitchQuery>,
) -> Response {
    let nav = super::nav::build(Some(&ep), rt.nav.roles);
    let pages = reachable(&nav);
    let from = if q.from.starts_with('/') && !q.from.starts_with("//") {
        q.from.as_str()
    } else {
        ""
    };
    let mapped = map_route(from, &ep, &pages);
    let mut response = (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, mapped.href.clone())],
    )
        .into_response();
    let headers = response.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&format!(
        "{}={ep}; Path=/ui; Max-Age=31536000; SameSite=Lax",
        super::NAV_ENDPOINT_COOKIE
    )) {
        headers.append(header::SET_COOKIE, v);
    }
    if mapped.fell_back {
        let i18n = I18n::new(locale);
        let msg = i18n.t("mgr-switch-fallback");
        let enc: String = form_urlencoded::byte_serialize(msg.as_bytes()).collect();
        if let Ok(v) = HeaderValue::from_str(&format!(
            "{TOAST_COOKIE}={enc}; Path=/ui; Max-Age=30; SameSite=Lax"
        )) {
            headers.append(header::SET_COOKIE, v);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_endpoints::ConsoleRole;

    fn pages(ep: &str, role: ConsoleRole) -> Vec<Reachable> {
        reachable(&super::super::nav::build(
            Some(ep),
            super::super::nav::NavRoles::uniform(role),
        ))
    }

    #[test]
    fn the_same_list_page_keeps_its_endpoint_independent_query() {
        let p = pages("b", ConsoleRole::Admin);
        let m = map_route("/ui/e/a/data/resources?type=Patient&q=smith", "b", &p);
        assert_eq!(m.href, "/ui/e/b/data/resources?type=Patient&q=smith");
        assert!(!m.fell_back);
        let m = map_route("/ui/e/a/security", "b", &p);
        assert_eq!(m.href, "/ui/e/b/security");
        assert_eq!(map_route("/ui/e/a", "b", &p).href, "/ui/e/b");
    }

    #[test]
    fn ids_of_the_old_endpoint_are_dropped_for_the_list_page() {
        let p = pages("b", ConsoleRole::Admin);
        let m = map_route("/ui/e/a/data/editor?type=Patient&id=123", "b", &p);
        assert!(!m.href.contains("id=123"), "{m:?}");
        assert!(m.href.starts_with("/ui/e/b/data/"), "{m:?}");
        assert!(!m.fell_back);
        let m = map_route("/ui/e/a/data/history?type=Patient&id=9&vid=2", "b", &p);
        assert!(!m.href.contains("id="), "{m:?}");
    }

    #[test]
    fn a_page_the_target_lacks_falls_back_to_its_nearest_parent() {
        let p = pages("b", ConsoleRole::Admin);
        let m = map_route("/ui/e/a/no-such-section/deeper", "b", &p);
        assert_eq!(m.href, "/ui/e/b");
        assert!(m.fell_back);
    }

    #[test]
    fn a_page_the_role_cannot_open_falls_back() {
        let admin = pages("b", ConsoleRole::Admin);
        let viewer = pages("b", ConsoleRole::Viewer);
        // Some endpoint page an admin reaches and a viewer does not.
        let only_admin = admin
            .iter()
            .find(|r| {
                r.prefix.starts_with("/ui/e/b/") && !viewer.iter().any(|v| v.prefix == r.prefix)
            })
            .expect("an admin-only endpoint page");
        let from = only_admin.prefix.replacen("/ui/e/b", "/ui/e/a", 1);
        let m = map_route(&from, "b", &viewer);
        assert!(m.fell_back, "{m:?}");
        assert!(m.href.starts_with("/ui/e/b"), "{m:?}");
        assert_ne!(m.href, only_admin.prefix);
    }

    #[test]
    fn pages_outside_an_endpoint_stay_put() {
        let p = pages("b", ConsoleRole::Admin);
        assert_eq!(
            map_route("/ui/terminology/code-systems?name=x", "b", &p).href,
            "/ui/terminology/code-systems?name=x"
        );
        assert_eq!(map_route("/ui", "b", &p).href, "/ui");
        assert_eq!(map_route("https://evil.example/x", "b", &p).href, "/ui/e/b");
    }
}
