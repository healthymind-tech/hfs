//! Route namespace of the unified server (design §3.2, §9.2).
//!
//! Public paths keep working byte for byte; the console moves from
//! `/ui/hts/**` to `/ui/terminology/**` with permanent redirects.

/// Inside the HFS process the terminology FHIR API is nested here; nginx's
/// `/tx/` location then proxies to `hfs:8080/tx/` instead of `hts:8090/`.
pub const TX_PREFIX: &str = "/tx";
/// Terminology admin API (was HTS `/admin`, public `/tx/admin`).
pub const ADMIN_PREFIX: &str = "/admin/terminology";
/// Unified console section.
pub const CONSOLE_PREFIX: &str = "/ui/terminology";
/// Old console mount.
pub const LEGACY_CONSOLE_PREFIX: &str = "/ui/hts";
/// Built-in authorization servers (line IdP, docs/design/authorization-server.md):
/// reserved here so no endpoint, console or terminology route takes it.
pub const IDP_PREFIX: &str = "/idp";

/// Path segments the unified server reserves at the HFS root, in addition
/// to helios-endpoints' `RESERVED_SEGMENTS` (endpoint names must avoid them).
pub const RESERVED_ROOT_SEGMENTS: &[&str] = &["tx", "idp", "ui", "admin", "metrics", "health"];

/// `/ui/hts/...` → `/ui/terminology/...` (308 for non-GET, 301 for GET).
/// Old page names that changed are mapped; everything else keeps its tail.
pub fn legacy_console_redirect(path_and_query: &str) -> Option<String> {
    let rest = path_and_query.strip_prefix(LEGACY_CONSOLE_PREFIX)?;
    if !(rest.is_empty() || rest.starts_with('/') || rest.starts_with('?')) {
        return None; // e.g. /ui/htsx
    }
    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (rest, None),
    };
    let mapped = match path.trim_end_matches('/') {
        "" => String::new(),
        // HTS "Terminology" inventory page becomes "Manage".
        p if p == "/terminology" || p.starts_with("/terminology/") => {
            format!("/manage{}", &p["/terminology".len()..])
        }
        "/login" | "/logout" => return Some(format!("/ui{}", path)),
        p if p.starts_with("/api/admin") => {
            return Some(format!("{ADMIN_PREFIX}{}", &p["/api/admin".len()..]));
        }
        p => p.to_string(),
    };
    let mut target = format!("{CONSOLE_PREFIX}{mapped}");
    if let Some(q) = query {
        target.push('?');
        target.push_str(q);
    }
    Some(target)
}

/// Terminology FHIR API path inside the process for a public `/tx/...` path.
pub fn strip_tx(path: &str) -> Option<&str> {
    let rest = path.strip_prefix(TX_PREFIX)?;
    match rest {
        "" => Some("/"),
        r if r.starts_with('/') => Some(r),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_redirects() {
        assert_eq!(
            legacy_console_redirect("/ui/hts").as_deref(),
            Some("/ui/terminology")
        );
        assert_eq!(
            legacy_console_redirect("/ui/hts/code-systems?sort=name").as_deref(),
            Some("/ui/terminology/code-systems?sort=name")
        );
        assert_eq!(
            legacy_console_redirect("/ui/hts/terminology?populated=true").as_deref(),
            Some("/ui/terminology/manage?populated=true")
        );
        assert_eq!(
            legacy_console_redirect("/ui/hts/login").as_deref(),
            Some("/ui/login")
        );
        assert_eq!(
            legacy_console_redirect("/ui/hts/api/admin/jobs").as_deref(),
            Some("/admin/terminology/jobs")
        );
        assert_eq!(legacy_console_redirect("/ui/htsx"), None);
        assert_eq!(legacy_console_redirect("/ui/e/default"), None);
    }

    #[test]
    fn tx_prefix() {
        assert_eq!(strip_tx("/tx"), Some("/"));
        assert_eq!(strip_tx("/tx/ValueSet/$expand"), Some("/ValueSet/$expand"));
        assert_eq!(strip_tx("/txt"), None);
    }

    #[test]
    fn reserved_segments_cover_new_prefixes() {
        for p in [TX_PREFIX, IDP_PREFIX] {
            assert!(RESERVED_ROOT_SEGMENTS.contains(&&p[1..]));
        }
        // helios-endpoints already refuses these as endpoint paths.
        for seg in ["tx", "ui", "admin", "metrics", "idp", "terminology"] {
            assert!(helios_endpoints::RESERVED_SEGMENTS.contains(&seg), "{seg}");
        }
    }
}
