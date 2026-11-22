//! The console's navigation registry (unified-server design §9.3).
//!
//! One sidebar for the whole console: FHIR endpoints, Terminology,
//! Authorization servers, Server and Settings. Every package adds its own
//! entries with [`register`] from the code that mounts its routes — the
//! layout never has to be edited to gain an entry:
//!
//! * `helios-ui` registers Overview, the per-endpoint Data / Configure /
//!   Operate groups, Server and Settings;
//! * `helios-hts-ui` registers the Terminology section when it is embedded;
//! * the built-in authorization server (line IdP) registers
//!   [`NavSection::AuthorizationServers`].
//!
//! ```
//! use helios_ui_chrome::nav::{register, NavItem, NavRole, NavSection};
//! register(
//!     NavItem::new(NavSection::AuthorizationServers, "idp-realms", "idp-nav-realms", "/ui/idp")
//!         .icon("key")
//!         .min_role(NavRole::Admin)
//!         .order(10),
//! );
//! ```
//!
//! The host (`helios-ui`) builds the sidebar of a request with [`build`],
//! passing a filter that decides, per item, whether the requester's role
//! allows it (items carry [`NavItem::min_role`] and the [`NavScope`] whose
//! role applies, so a terminology-only administrator binding can raise one
//! section without raising the others).
//!
//! The active entry is marked twice: by the server on a full page load
//! (`active_page` id) and by `shell.js` after every in-place navigation,
//! from the longest [`NavLink::match_prefix`] that prefixes the location.

use std::sync::RwLock;

/// The console roles, lowest first (mirrors `helios_endpoints::ConsoleRole`
/// without depending on it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum NavRole {
    /// Read-only.
    #[default]
    Viewer,
    /// May change data.
    Editor,
    /// May change configuration.
    Admin,
}

/// Whose role decides whether an item is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum NavScope {
    /// The console role (global, or the current endpoint's binding for
    /// per-endpoint sections).
    #[default]
    Console,
    /// The terminology role: max(global role, `terminology` binding).
    Terminology,
}

/// Where an item lives in the sidebar. Sections render in declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NavSection {
    /// Server-wide pages above the endpoint block (Overview).
    Manager,
    /// The current endpoint: home and data pages. `href` may contain
    /// `{ep}`, replaced by the endpoint name.
    EndpointData,
    /// The current endpoint: configuration.
    EndpointConfigure,
    /// The current endpoint: operations (sync, audit, history).
    EndpointOperate,
    /// The terminology service (code systems, value sets, …).
    Terminology,
    /// Built-in authorization servers (line IdP; hidden unless registered).
    AuthorizationServers,
    /// Server-wide operations (sync jobs of every endpoint, packages, …).
    Server,
    /// Console settings (users, sign-in, about).
    Settings,
}

impl NavSection {
    /// Fluent key of the section heading.
    pub fn label_key(self) -> &'static str {
        match self {
            NavSection::Manager => "mgr-nav-section-manager",
            NavSection::EndpointData => "mgr-nav-section-data",
            NavSection::EndpointConfigure => "mgr-nav-section-configure",
            NavSection::EndpointOperate => "mgr-nav-section-operate",
            NavSection::Terminology => "mgr-nav-section-terminology",
            NavSection::AuthorizationServers => "mgr-nav-section-idp",
            NavSection::Server => "mgr-nav-section-server",
            NavSection::Settings => "mgr-nav-section-settings",
        }
    }

    /// Whether items of this section need a current endpoint.
    pub fn per_endpoint(self) -> bool {
        matches!(
            self,
            NavSection::EndpointData | NavSection::EndpointConfigure | NavSection::EndpointOperate
        )
    }

    /// Stable id, used in `data-nav-section` attributes.
    pub fn id(self) -> &'static str {
        match self {
            NavSection::Manager => "manager",
            NavSection::EndpointData => "data",
            NavSection::EndpointConfigure => "configure",
            NavSection::EndpointOperate => "operate",
            NavSection::Terminology => "terminology",
            NavSection::AuthorizationServers => "idp",
            NavSection::Server => "server",
            NavSection::Settings => "settings",
        }
    }
}

/// One sidebar entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavItem {
    /// Section.
    pub section: NavSection,
    /// The page's `active_page` id (marks the entry current on a full load).
    pub id: &'static str,
    /// Fluent key of the label.
    pub label_key: &'static str,
    /// Target; `{ep}` = current endpoint name.
    pub href: &'static str,
    /// Icon name (the host's icon set).
    pub icon: &'static str,
    /// Hidden below this role.
    pub min_role: NavRole,
    /// Which role [`NavItem::min_role`] is compared with.
    pub scope: NavScope,
    /// Sort key within the section.
    pub order: u16,
    /// Path prefix that makes this entry current after an in-place
    /// navigation; `None` = the resolved `href` itself. `{ep}` allowed.
    pub match_prefix: Option<&'static str>,
    /// Whether the target page may be swapped in place (persistent shell).
    /// `false` for pages whose scripts are not lifecycle-clean yet; those
    /// load as a full document (with a cross-document view transition).
    pub boost: bool,
}

impl NavItem {
    /// An entry with defaults: viewer, console scope, order 0, boosted.
    pub const fn new(
        section: NavSection,
        id: &'static str,
        label_key: &'static str,
        href: &'static str,
    ) -> Self {
        Self {
            section,
            id,
            label_key,
            href,
            icon: "grid",
            min_role: NavRole::Viewer,
            scope: NavScope::Console,
            order: 0,
            match_prefix: None,
            boost: true,
        }
    }

    /// Sets the icon.
    pub const fn icon(mut self, icon: &'static str) -> Self {
        self.icon = icon;
        self
    }

    /// Sets the minimal role.
    pub const fn min_role(mut self, role: NavRole) -> Self {
        self.min_role = role;
        self
    }

    /// Sets the role scope.
    pub const fn scope(mut self, scope: NavScope) -> Self {
        self.scope = scope;
        self
    }

    /// Sets the sort key.
    pub const fn order(mut self, order: u16) -> Self {
        self.order = order;
        self
    }

    /// Sets the active-state prefix.
    pub const fn match_prefix(mut self, prefix: &'static str) -> Self {
        self.match_prefix = Some(prefix);
        self
    }

    /// Marks the target as a full-document page (no in-place swap).
    pub const fn full_load(mut self) -> Self {
        self.boost = false;
        self
    }
}

static REGISTERED: RwLock<Vec<NavItem>> = RwLock::new(Vec::new());

/// Adds an entry (idempotent per `id`: a second registration replaces the
/// first).
pub fn register(item: NavItem) {
    let mut items = REGISTERED.write().unwrap_or_else(|p| p.into_inner());
    items.retain(|i| i.id != item.id);
    items.push(item);
}

/// Removes every entry of `section` (a package switched off at runtime).
pub fn unregister_section(section: NavSection) {
    let mut items = REGISTERED.write().unwrap_or_else(|p| p.into_inner());
    items.retain(|i| i.section != section);
}

/// A snapshot of every registered entry.
pub fn registered() -> Vec<NavItem> {
    REGISTERED
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

/// One rendered entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavLink {
    /// `active_page` id.
    pub id: &'static str,
    /// Fluent key.
    pub label_key: &'static str,
    /// Resolved href.
    pub href: String,
    /// Icon name.
    pub icon: &'static str,
    /// Resolved active-state prefix.
    pub match_prefix: String,
    /// In-place navigation allowed.
    pub boost: bool,
}

/// One rendered section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavGroup {
    /// Section.
    pub section: NavSection,
    /// Fluent key of the heading.
    pub label_key: &'static str,
    /// Entries.
    pub links: Vec<NavLink>,
}

/// The sidebar of a request: `items` (built-ins of the host merged with
/// [`registered`] ones — a registered id replaces a built-in one), the
/// per-endpoint sections only with an `endpoint`, entries `allow` refuses
/// hidden, sorted by section then order.
pub fn build(
    builtin: Vec<NavItem>,
    endpoint: Option<&str>,
    allow: impl Fn(&NavItem) -> bool,
) -> Vec<NavGroup> {
    let mut items = builtin;
    for extra in registered() {
        items.retain(|i| i.id != extra.id);
        items.push(extra);
    }
    items.retain(|i| (endpoint.is_some() || !i.section.per_endpoint()) && allow(i));
    items.sort_by_key(|i| (i.section, i.order));
    let resolve = |s: &str| match endpoint {
        Some(ep) => s.replace("{ep}", ep),
        None => s.to_string(),
    };
    let mut groups: Vec<NavGroup> = Vec::new();
    for i in items {
        let href = resolve(i.href);
        let match_prefix = i
            .match_prefix
            .map(resolve)
            .unwrap_or_else(|| href.split('?').next().unwrap_or("").to_string());
        let link = NavLink {
            id: i.id,
            label_key: i.label_key,
            href,
            icon: i.icon,
            match_prefix,
            boost: i.boost,
        };
        match groups.last_mut() {
            Some(g) if g.section == i.section => g.links.push(link),
            _ => groups.push(NavGroup {
                section: i.section,
                label_key: i.section.label_key(),
                links: vec![link],
            }),
        }
    }
    groups
}

/// The navigation context key (§9.4 item 3): when it differs between the
/// page in the browser and the page fetched in place, the shell replaces
/// the sidebar's nav; otherwise the sidebar DOM is kept as is.
pub fn nav_key(endpoint: Option<&str>, role: NavRole, extra: &str) -> String {
    let role = match role {
        NavRole::Viewer => "v",
        NavRole::Editor => "e",
        NavRole::Admin => "a",
    };
    let sections: Vec<String> = {
        let items = REGISTERED.read().unwrap_or_else(|p| p.into_inner());
        let mut s: Vec<&str> = items.iter().map(|i| i.section.id()).collect();
        s.sort_unstable();
        s.dedup();
        s.into_iter().map(str::to_string).collect()
    };
    format!(
        "{}:{role}:{}:{extra}",
        endpoint.unwrap_or("-"),
        sections.join(",")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin() -> Vec<NavItem> {
        vec![
            NavItem::new(NavSection::Manager, "overview", "ov", "/ui"),
            NavItem::new(NavSection::EndpointData, "ep-home", "home", "/ui/e/{ep}")
                .match_prefix("/ui/e/{ep}/home"),
            NavItem::new(
                NavSection::EndpointData,
                "resources",
                "res",
                "/ui/e/{ep}/data/resources",
            )
            .order(10)
            .full_load(),
            NavItem::new(NavSection::Settings, "users", "u", "/ui/settings/users")
                .min_role(NavRole::Admin),
        ]
    }

    #[test]
    fn endpoint_sections_need_an_endpoint_and_resolve_placeholders() {
        let without = build(builtin(), None, |_| true);
        assert!(without.iter().all(|g| !g.section.per_endpoint()));
        let with = build(builtin(), Some("twcore"), |_| true);
        let data = with
            .iter()
            .find(|g| g.section == NavSection::EndpointData)
            .unwrap();
        assert_eq!(data.links[0].href, "/ui/e/twcore");
        assert_eq!(data.links[0].match_prefix, "/ui/e/twcore/home");
        assert_eq!(data.links[1].match_prefix, "/ui/e/twcore/data/resources");
        assert!(!data.links[1].boost);
        let order: Vec<_> = with.iter().map(|g| g.section).collect();
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(order, sorted);
    }

    #[test]
    fn the_filter_hides_entries_and_registration_merges() {
        let viewer = build(builtin(), None, |i| i.min_role <= NavRole::Viewer);
        assert!(viewer.iter().flat_map(|g| &g.links).all(|l| l.id != "users"));
        register(
            NavItem::new(
                NavSection::AuthorizationServers,
                "test-idp-realms",
                "idp",
                "/ui/idp",
            )
            .min_role(NavRole::Admin),
        );
        let admin = build(builtin(), None, |_| true);
        let idp = admin
            .iter()
            .position(|g| g.section == NavSection::AuthorizationServers)
            .expect("registered section shows");
        let settings = admin
            .iter()
            .position(|g| g.section == NavSection::Settings)
            .unwrap();
        assert!(idp < settings, "IdP sits above Server/Settings");
        assert!(nav_key(None, NavRole::Admin, "").contains("idp"));
        unregister_section(NavSection::AuthorizationServers);
        assert!(
            build(builtin(), None, |_| true)
                .iter()
                .all(|g| g.section != NavSection::AuthorizationServers)
        );
    }
}
