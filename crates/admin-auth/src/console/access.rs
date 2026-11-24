//! Which console role each `/ui` route needs (design §7.2).
//!
//! | Route | Requirement |
//! |---|---|
//! | sign-in flow, assets, about | public |
//! | `/ui/account/**` | any signed-in user (not an anonymous visitor) |
//! | server administration: `/ui/settings/**`, `/ui/endpoints/**`, IG Packages, Tenants, Bulk Import/Export, SQL Export; writes under `/ui/sync/**` | global admin |
//! | authorization servers `/ui/idp/**` (realms, clients, users, keys, sessions) | global admin |
//! | a user's resource access `/ui/idp/{realm}/users/{user}/access[/…]` | viewer to read, editor to change |
//! | `/ui/e/{ep}/{config,security,validation,terminology,sync}` writes | admin on `ep` |
//! | `/ui/e/{ep}/**` other writes (data, bulk) | editor on `ep` |
//! | `/ui/e/{ep}/**` reads | viewer on `ep` |
//! | other reads; previews and per-browser preferences | viewer |
//! | any other write | editor (FHIR data writes: editor, batch, saved queries) |

use axum::http::Method;
use helios_endpoints::console::ConsoleRole;

use crate::{is_read_only, under};

/// What a request needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Requirement {
    /// Nothing (sign-in pages, assets).
    Public,
    /// A real session (not an anonymous `none`-mode visitor).
    SignedIn,
    /// At least this global role.
    Role(ConsoleRole),
    /// At least this role on the endpoint (`role_for(endpoint)`).
    EndpointRole(String, ConsoleRole),
}

/// Reachable without a session in every mode.
pub const PUBLIC_PATHS: &[&str] = &[
    "/ui/login",
    "/ui/callback",
    "/ui/logout",
    "/ui/admin/login",
    "/ui/admin/logout",
    "/ui/assets",
    "/ui/about",
];

/// Server-wide administration (global admin, any method).
pub const ADMIN_PAGES: &[&str] = &[
    "/ui/settings",
    "/ui/endpoints",
    "/ui/packages",
    "/ui/tenants",
    "/ui/bulk-import",
    "/ui/bulk-export",
    "/ui/sql/export",
];

/// Pages anyone may view that only admins may change.
pub const ADMIN_WRITES: &[&str] = &["/ui/sync"];

/// Writes that change nothing on the server (previews and this browser's
/// preferences): viewer.
pub const OPEN_WRITES: &[&str] = &[
    "/ui/capability-statement/json-expand",
    "/ui/sql/view-definitions/lint",
    "/ui/sql/view-definitions/complete",
    "/ui/sql/view-definitions/run",
    "/ui/sql/queries/run",
    "/ui/sql/queries/document",
    "/ui/sql/views/run",
    "/ui/sql/views/document",
    "/ui/lookup/patient-options",
    "/ui/lookup/group-options",
    "/ui/lookup/table-options",
    "/ui/editor/render",
    "/ui/json-view/render",
    "/ui/history/diff",
    "/ui/version",
    "/ui/tenant",
];

/// Endpoint sections whose writes change the endpoint's configuration.
const ENDPOINT_ADMIN_SECTIONS: &[&str] =
    &["config", "security", "validation", "terminology", "sync"];

/// The endpoint a console request acts on when its path does not name it:
/// `/ui/e/{ep}/data/X` is rewritten to `/ui/X` before the gate sees it, and
/// an htmx request to `/ui/X` from an endpoint page is scoped by its
/// `Referer`. The console's endpoint dispatcher inserts this extension so
/// the gate checks the role **on that endpoint** (bindings may raise or
/// lower the global role) instead of the global one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopedEndpoint(pub String);

/// [`requirement`] for a request acting on `scoped` (see
/// [`ScopedEndpoint`]): a viewer or editor requirement becomes the same
/// role on that endpoint; server administration (global admin) and the
/// requirements that name their endpoint already are unchanged.
pub fn scoped_requirement(
    method: &Method,
    path: &str,
    scoped: Option<&str>,
) -> Option<Requirement> {
    let need = requirement(method, path)?;
    Some(match (need, scoped) {
        (Requirement::Role(r), Some(ep)) if r < ConsoleRole::Admin && !ep.is_empty() => {
            Requirement::EndpointRole(ep.to_string(), r)
        }
        (need, _) => need,
    })
}

/// The requirement of `(method, path)`; `None` outside `/ui`.
pub fn requirement(method: &Method, path: &str) -> Option<Requirement> {
    if !under(path, "/ui") {
        return None;
    }
    if PUBLIC_PATHS.iter().any(|p| under(path, p)) {
        return Some(Requirement::Public);
    }
    if under(path, "/ui/account") {
        return Some(Requirement::SignedIn);
    }
    if ADMIN_PAGES.iter().any(|p| under(path, p)) {
        return Some(Requirement::Role(ConsoleRole::Admin));
    }
    let read = is_read_only(method);
    if under(path, "/ui/idp") {
        // The built-in authorization server's console: realms, clients
        // (secrets), users (passwords), keys and sessions are server
        // administration. A user's resource-access editor follows the same
        // roles as its admin API (`admin_api_requirement`).
        let role = if is_idp_user_access(path) {
            if read {
                ConsoleRole::Viewer
            } else {
                ConsoleRole::Editor
            }
        } else {
            ConsoleRole::Admin
        };
        return Some(Requirement::Role(role));
    }
    if let Some(rest) = path.strip_prefix("/ui/e/") {
        let mut parts = rest.split('/');
        // The handlers see the decoded name (axum `Path`): `/ui/e/l%61b`
        // is endpoint `lab` and needs the role on `lab`.
        let endpoint = percent_decode(parts.next().unwrap_or_default());
        if !endpoint.is_empty() {
            let section = parts.next().unwrap_or_default();
            let role = if read {
                ConsoleRole::Viewer
            } else if ENDPOINT_ADMIN_SECTIONS.contains(&section) {
                ConsoleRole::Admin
            } else {
                ConsoleRole::Editor
            };
            return Some(Requirement::EndpointRole(endpoint, role));
        }
    }
    if read || OPEN_WRITES.contains(&path) {
        return Some(Requirement::Role(ConsoleRole::Viewer));
    }
    if ADMIN_WRITES.iter().any(|p| under(path, p)) {
        return Some(Requirement::Role(ConsoleRole::Admin));
    }
    Some(Requirement::Role(ConsoleRole::Editor))
}

/// `%XX`-decodes a path segment (invalid escapes are kept as they are).
fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The console role an admin API request needs (design §7.2 and §8):
///
/// * `GET /admin/endpoints[/{name}[/…]]` — viewer (on `{name}` when
///   named): endpoint config is shown to viewers, redacted (no secrets);
/// * writes under `/admin/endpoints/{name}/…` other than delete — admin
///   **on that endpoint** (a per-endpoint admin binding may grant it);
/// * everything else — creating or deleting endpoints, `$rewrap`, console
///   users and settings, packages, tenants, sync — **global** admin.
///
/// * `/admin/idp/realms/{realm}/users/{user}/links` (a user's resource
///   access in bulk) — viewer to read (copy, export), editor to change,
///   the same as the console's resource-access editor;
///
/// Returns `(role, endpoint)`; `endpoint = None` means the global role.
pub fn admin_api_requirement(method: &Method, path: &str) -> (ConsoleRole, Option<String>) {
    if is_idp_user_links(path) {
        let role = if is_read_only(method) {
            ConsoleRole::Viewer
        } else {
            ConsoleRole::Editor
        };
        return (role, None);
    }
    let endpoint = path
        .strip_prefix("/admin/endpoints/")
        .map(|rest| rest.split('/').next().unwrap_or_default())
        .filter(|n| !n.is_empty() && !n.starts_with('$'))
        .map(str::to_string);
    let endpoints_api = path == "/admin/endpoints" || endpoint.is_some();
    if endpoints_api && is_read_only(method) {
        return (ConsoleRole::Viewer, endpoint);
    }
    if endpoint.is_some() && *method != Method::DELETE {
        return (ConsoleRole::Admin, endpoint);
    }
    (ConsoleRole::Admin, None)
}

/// `/ui/idp/{realm}/users/{user}/access[/…]` (the resource-access editor).
fn is_idp_user_access(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/ui/idp/") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    matches!(parts.as_slice(), [realm, "users", user, "access", ..] if !realm.is_empty() && !user.is_empty() && *realm != "new")
}

/// `/admin/idp/realms/{realm}/users/{user}/links` (exactly).
fn is_idp_user_links(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/admin/idp/realms/") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    matches!(parts.as_slice(), [realm, "users", user, "links"] if !realm.is_empty() && !user.is_empty())
}

/// Whether `principal` may make this admin API request.
pub fn admin_api_allows(
    principal: &helios_endpoints::console::ConsolePrincipal,
    method: &Method,
    path: &str,
) -> bool {
    let (need, endpoint) = admin_api_requirement(method, path);
    let have = match &endpoint {
        Some(ep) => principal.role_for(ep),
        None => principal.role,
    };
    have >= need
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConsoleRole::*;

    fn req(m: Method, p: &str) -> Option<Requirement> {
        requirement(&m, p)
    }

    #[test]
    fn classification() {
        assert_eq!(req(Method::GET, "/Patient"), None);
        assert_eq!(req(Method::GET, "/uix"), None);
        for p in [
            "/ui/login",
            "/ui/login/local",
            "/ui/callback",
            "/ui/assets/app.css",
            "/ui/about",
        ] {
            assert_eq!(req(Method::GET, p), Some(Requirement::Public), "{p}");
        }
        assert_eq!(req(Method::POST, "/ui/logout"), Some(Requirement::Public));
        assert_eq!(req(Method::GET, "/ui/account"), Some(Requirement::SignedIn));
        assert_eq!(
            req(Method::POST, "/ui/account/password"),
            Some(Requirement::SignedIn)
        );
        for p in [
            "/ui/settings/users",
            "/ui/settings/auth",
            "/ui/endpoints/new",
            "/ui/packages",
            "/ui/tenants/rows",
            "/ui/bulk-export",
            "/ui/sql/export/new",
        ] {
            assert_eq!(req(Method::GET, p), Some(Requirement::Role(Admin)), "{p}");
        }
        assert_eq!(req(Method::GET, "/ui"), Some(Requirement::Role(Viewer)));
        assert_eq!(
            req(Method::GET, "/ui/resources"),
            Some(Requirement::Role(Viewer))
        );
        assert_eq!(
            req(Method::GET, "/ui/sync"),
            Some(Requirement::Role(Viewer))
        );
        assert_eq!(
            req(Method::POST, "/ui/sync/jobs"),
            Some(Requirement::Role(Admin))
        );
        assert_eq!(
            req(Method::POST, "/ui/version"),
            Some(Requirement::Role(Viewer))
        );
        assert_eq!(
            req(Method::POST, "/ui/editor/render"),
            Some(Requirement::Role(Viewer))
        );
        assert_eq!(
            req(Method::POST, "/ui/sql/view-definitions"),
            Some(Requirement::Role(Editor))
        );
        assert_eq!(
            req(Method::DELETE, "/ui/anything"),
            Some(Requirement::Role(Editor))
        );
        assert_eq!(
            req(Method::GET, "/ui/e/lab/data/Patient"),
            Some(Requirement::EndpointRole("lab".into(), Viewer))
        );
        assert_eq!(
            req(Method::POST, "/ui/e/lab/data/Patient"),
            Some(Requirement::EndpointRole("lab".into(), Editor))
        );
        assert_eq!(
            req(Method::PUT, "/ui/e/lab/security"),
            Some(Requirement::EndpointRole("lab".into(), Admin))
        );
        assert_eq!(req(Method::GET, "/ui/e/"), Some(Requirement::Role(Viewer)));
        // A percent-encoded name is the endpoint the handler will act on.
        assert_eq!(
            req(Method::POST, "/ui/e/l%61b/security"),
            Some(Requirement::EndpointRole("lab".into(), Admin))
        );
        assert_eq!(
            req(Method::GET, "/ui/e/l%6/x"),
            Some(Requirement::EndpointRole("l%6".into(), Viewer))
        );
    }

    /// A data page reached as `/ui/e/{ep}/data/X` is checked against the
    /// role on `ep` (the dispatcher rewrote the path to `/ui/X`).
    #[test]
    fn scoped_requests_use_the_endpoint_role() {
        let sreq = |m: Method, p: &str, ep: Option<&str>| scoped_requirement(&m, p, ep);
        assert_eq!(
            sreq(Method::POST, "/ui/sql/view-definitions", Some("lab")),
            Some(Requirement::EndpointRole("lab".into(), Editor))
        );
        assert_eq!(
            sreq(Method::GET, "/ui/resources", Some("lab")),
            Some(Requirement::EndpointRole("lab".into(), Viewer))
        );
        assert_eq!(
            sreq(Method::POST, "/ui/sql/view-definitions", None),
            Some(Requirement::Role(Editor))
        );
        // Server administration stays global.
        assert_eq!(
            sreq(Method::GET, "/ui/bulk-export", Some("lab")),
            Some(Requirement::Role(Admin))
        );
        assert_eq!(
            sreq(Method::POST, "/ui/sync/jobs", Some("lab")),
            Some(Requirement::Role(Admin))
        );
        assert_eq!(
            sreq(Method::GET, "/ui/login", Some("lab")),
            Some(Requirement::Public)
        );
    }

    /// The built-in authorization server's console is server administration
    /// (it resets client secrets and sets user passwords); only a user's
    /// resource-access editor is open to viewers (read) and editors (write).
    #[test]
    fn idp_console_needs_admin() {
        for (m, p) in [
            (Method::GET, "/ui/idp"),
            (Method::GET, "/ui/idp/demo"),
            (Method::GET, "/ui/idp/demo/users/u1"),
            (Method::GET, "/ui/idp/demo/clients/app"),
            (Method::GET, "/ui/idp/demo/test/report.json"),
            (Method::POST, "/ui/idp/new"),
            (Method::POST, "/ui/idp/demo/clients/app/reset-secret"),
            (Method::POST, "/ui/idp/demo/users/u1/password"),
            (Method::POST, "/ui/idp/demo/keys/rotate"),
            (Method::POST, "/ui/idp/demo/settings/unbind"),
            (Method::GET, "/ui/idp/new/users/x/access"),
        ] {
            assert_eq!(req(m.clone(), p), Some(Requirement::Role(Admin)), "{m} {p}");
        }
        for p in [
            "/ui/idp/demo/users/u1/access",
            "/ui/idp/demo/users/u1/access/export",
            "/ui/idp/demo/users/u1/access/search",
        ] {
            assert_eq!(req(Method::GET, p), Some(Requirement::Role(Viewer)), "{p}");
        }
        for p in [
            "/ui/idp/demo/users/u1/access/preview",
            "/ui/idp/demo/users/u1/access/apply",
            "/ui/idp/demo/users/u1/access/patch",
        ] {
            assert_eq!(req(Method::POST, p), Some(Requirement::Role(Editor)), "{p}");
        }
    }

    #[test]
    fn admin_api_policy() {
        use helios_endpoints::console::{ConsolePrincipal, ConsoleVia};
        let mut p = ConsolePrincipal {
            subject: "local:v".into(),
            display_name: "v".into(),
            role: Viewer,
            endpoint_roles: Default::default(),
            via: ConsoleVia::Local,
        };
        assert!(admin_api_allows(&p, &Method::GET, "/admin/endpoints"));
        assert!(admin_api_allows(
            &p,
            &Method::GET,
            "/admin/endpoints/lab/history"
        ));
        assert!(!admin_api_allows(&p, &Method::PUT, "/admin/endpoints/lab"));
        assert!(!admin_api_allows(&p, &Method::GET, "/admin/console/users"));
        assert!(!admin_api_allows(&p, &Method::GET, "/admin/packages"));
        p.endpoint_roles.insert("lab".into(), Admin);
        assert!(admin_api_allows(&p, &Method::PUT, "/admin/endpoints/lab"));
        assert!(admin_api_allows(
            &p,
            &Method::POST,
            "/admin/endpoints/lab/$disable"
        ));
        assert!(!admin_api_allows(
            &p,
            &Method::DELETE,
            "/admin/endpoints/lab"
        ));
        assert!(!admin_api_allows(&p, &Method::POST, "/admin/endpoints"));
        assert!(!admin_api_allows(
            &p,
            &Method::POST,
            "/admin/secrets/$rewrap"
        ));
        assert!(!admin_api_allows(
            &p,
            &Method::PUT,
            "/admin/endpoints/other"
        ));
        p.role = Admin;
        assert!(admin_api_allows(
            &p,
            &Method::DELETE,
            "/admin/endpoints/lab"
        ));
        assert!(admin_api_allows(&p, &Method::GET, "/admin/console/users"));
    }

    #[test]
    fn idp_user_links_follow_the_console_roles() {
        use helios_endpoints::console::{ConsolePrincipal, ConsoleVia};
        let links = "/admin/idp/realms/demo/users/u_1/links";
        let mut p = ConsolePrincipal {
            subject: "local:v".into(),
            display_name: "v".into(),
            role: Viewer,
            endpoint_roles: Default::default(),
            via: ConsoleVia::Local,
        };
        assert!(admin_api_allows(&p, &Method::GET, links));
        assert!(!admin_api_allows(&p, &Method::PUT, links));
        assert!(!admin_api_allows(&p, &Method::PATCH, links));
        // The rest of the IdP API stays administrator-only.
        assert!(!admin_api_allows(
            &p,
            &Method::GET,
            "/admin/idp/realms/demo/users/u_1"
        ));
        assert!(!admin_api_allows(
            &p,
            &Method::GET,
            "/admin/idp/realms/demo/users/u_1/links/x"
        ));
        assert!(!admin_api_allows(
            &p,
            &Method::GET,
            "/admin/idp/realms/demo/users//links"
        ));
        p.role = Editor;
        assert!(admin_api_allows(&p, &Method::PUT, links));
        assert!(admin_api_allows(&p, &Method::PATCH, links));
        assert!(!admin_api_allows(
            &p,
            &Method::PUT,
            "/admin/idp/realms/demo/users/u_1"
        ));
        assert!(!admin_api_allows(&p, &Method::POST, "/admin/idp/realms"));
    }
}
