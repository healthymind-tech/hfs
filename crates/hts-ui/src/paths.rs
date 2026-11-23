//! Where the console's pages, assets and browser admin API live
//! (`TerminologyPaths`, unified-server design §9.2).
//!
//! Stand-alone in the `hts` binary the console is mounted at `/ui/hts`
//! (assets `/ui/hts/assets`, inventory `/ui/hts/terminology`). Embedded in
//! the HFS console (`helios-ui`) it is the Terminology section at
//! `/ui/terminology` — inventory renamed `manage`, assets served once from
//! `/ui/assets`. Internal routes keep their `/hts/...` shape; the host maps
//! `/ui/terminology/...` onto them (see [`internal_path`]).
//!
//! Process-wide: one binary hosts the console in one place.

use std::sync::OnceLock;

/// The resolved prefixes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminologyPaths {
    /// Console home (`/ui/hts` or `/ui/terminology`).
    pub ui: &'static str,
    /// Inventory / management page.
    pub manage: &'static str,
    /// Static assets.
    pub assets: &'static str,
    /// Browser-side admin API (chunked upload, job progress).
    pub admin_api: &'static str,
    /// Whether the console is embedded in a host console (one sign-in, the
    /// host's shell and guard).
    pub embedded: bool,
}

impl TerminologyPaths {
    /// The stand-alone `hts` binary layout.
    pub const STANDALONE: TerminologyPaths = TerminologyPaths {
        ui: "/ui/hts",
        manage: "/ui/hts/terminology",
        assets: "/ui/hts/assets",
        admin_api: "/ui/hts/api/admin",
        embedded: false,
    };

    /// Embedded in the HFS console.
    pub const EMBEDDED: TerminologyPaths = TerminologyPaths {
        ui: "/ui/terminology",
        manage: "/ui/terminology/manage",
        assets: "/ui/assets",
        admin_api: "/ui/terminology/api/admin",
        embedded: true,
    };
}

static PATHS: OnceLock<TerminologyPaths> = OnceLock::new();

/// Chooses the layout; the first call wins (call it before building the
/// router). Returns whether this call set it.
pub fn configure(paths: TerminologyPaths) -> bool {
    PATHS.set(paths).is_ok()
}

/// The layout in force.
pub fn get() -> &'static TerminologyPaths {
    PATHS.get().unwrap_or(&TerminologyPaths::STANDALONE)
}

/// Console home.
pub fn ui() -> &'static str {
    get().ui
}

/// Inventory / management page.
pub fn manage() -> &'static str {
    get().manage
}

/// Static assets.
pub fn assets() -> &'static str {
    get().assets
}

/// Browser admin API.
pub fn admin_api() -> &'static str {
    get().admin_api
}

/// Whether the console is embedded.
pub fn embedded() -> bool {
    get().embedded
}

/// A stand-alone console path (`/ui/hts/...`) as the browser should follow
/// it under the layout in force. Anything else is returned unchanged.
pub fn link(legacy: &str) -> String {
    map_with(get(), legacy)
}

fn map_with(p: &TerminologyPaths, legacy: &str) -> String {
    let Some(rest) = legacy.strip_prefix("/ui/hts") else {
        return legacy.to_string();
    };
    if !(rest.is_empty() || rest.starts_with('/') || rest.starts_with('?')) {
        return legacy.to_string();
    }
    for (old, new) in [
        ("/terminology", p.manage),
        ("/assets", p.assets),
        ("/api/admin", p.admin_api),
    ] {
        if let Some(tail) = rest.strip_prefix(old)
            && (tail.is_empty() || tail.starts_with('/') || tail.starts_with('?'))
        {
            return format!("{new}{tail}");
        }
    }
    format!("{}{rest}", p.ui)
}

/// [`link`] for a fixed path, interned so it can be handed out as
/// `&'static str` (each distinct path is mapped once per process).
pub fn fixed(legacy: &'static str) -> &'static str {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static CACHE: OnceLock<Mutex<HashMap<&'static str, &'static str>>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    cache
        .entry(legacy)
        .or_insert_with(|| Box::leak(link(legacy).into_boxed_str()))
}

/// The router path (`/hts/...`) for a browser path under the embedded
/// prefix (`/ui/terminology/...`); `None` outside it.
pub fn internal_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix(TerminologyPaths::EMBEDDED.ui)?;
    if !(rest.is_empty() || rest.starts_with('/')) {
        return None;
    }
    if let Some(tail) = rest.strip_prefix("/manage")
        && (tail.is_empty() || tail.starts_with('/'))
    {
        return Some(format!("/hts/terminology{tail}"));
    }
    Some(format!("/hts{rest}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_links_follow_the_layout() {
        let e = &TerminologyPaths::EMBEDDED;
        assert_eq!(map_with(e, "/ui/hts"), "/ui/terminology");
        assert_eq!(
            map_with(e, "/ui/hts/code-systems?x=1"),
            "/ui/terminology/code-systems?x=1"
        );
        assert_eq!(
            map_with(e, "/ui/hts/terminology?q=a"),
            "/ui/terminology/manage?q=a"
        );
        assert_eq!(
            map_with(e, "/ui/hts/terminology/delete?url=x"),
            "/ui/terminology/manage/delete?url=x"
        );
        assert_eq!(map_with(e, "/ui/hts/assets/app.css"), "/ui/assets/app.css");
        assert_eq!(map_with(e, "/ui/htsx"), "/ui/htsx");
        let s = &TerminologyPaths::STANDALONE;
        assert_eq!(map_with(s, "/ui/hts/terminology"), "/ui/hts/terminology");
    }

    #[test]
    fn embedded_paths_map_onto_the_router() {
        assert_eq!(internal_path("/ui/terminology").as_deref(), Some("/hts"));
        assert_eq!(
            internal_path("/ui/terminology/value-sets/x").as_deref(),
            Some("/hts/value-sets/x")
        );
        assert_eq!(
            internal_path("/ui/terminology/manage").as_deref(),
            Some("/hts/terminology")
        );
        assert_eq!(
            internal_path("/ui/terminology/manage/delete").as_deref(),
            Some("/hts/terminology/delete")
        );
        assert_eq!(internal_path("/ui/terminologyx"), None);
    }
}
