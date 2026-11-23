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
    /// Server-wide pages at the top of the sidebar (Overview).
    Manager,
    /// Built-in authorization servers (line IdP; hidden unless registered).
    /// Shown in the sidebar's top group, under Overview.
    AuthorizationServers,
    /// Server-wide operations (sync jobs of every endpoint, …), in the
    /// sidebar's top group.
    Server,
    /// The current endpoint: home and data pages. `href` may contain
    /// `{ep}`, replaced by the endpoint name.
    EndpointData,
    /// The current endpoint: operations (sync, audit, history).
    EndpointOperate,
    /// The current endpoint: configuration.
    EndpointConfigure,
    /// The terminology service (code systems, value sets, …).
    Terminology,
    /// Console settings (users, sign-in, tenants): the sidebar foot.
    Settings,
}

/// The visual groups of the sidebar (console redesign §5.1): the sections
/// above collapse into four.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NavBlock {
    /// Overview, Authorization servers, Sync jobs — no heading.
    Top,
    /// The current endpoint, headed by the endpoint switcher.
    Endpoint,
    /// The terminology service.
    Terminology,
    /// Console settings, in the sidebar foot.
    Foot,
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

    /// The sidebar block this section is drawn in.
    pub fn block(self) -> NavBlock {
        match self {
            NavSection::Manager | NavSection::AuthorizationServers | NavSection::Server => {
                NavBlock::Top
            }
            NavSection::EndpointData
            | NavSection::EndpointOperate
            | NavSection::EndpointConfigure => NavBlock::Endpoint,
            NavSection::Terminology => NavBlock::Terminology,
            NavSection::Settings => NavBlock::Foot,
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
    /// The sidebar entry this page shares with its siblings (console
    /// redesign §5.3): `(id, label key, icon)`. Items of one section with the
    /// same group id become ONE sidebar link (to the first one the requester
    /// may open) and a row of route tabs on each of their pages.
    pub group: Option<(&'static str, &'static str, &'static str)>,
    /// `false`: reachable (search, account menu, links) but not drawn in the
    /// sidebar.
    pub sidebar: bool,
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
            group: None,
            sidebar: true,
        }
    }

    /// Puts the item in a sidebar group (`id`, label key, icon); see
    /// [`NavItem::group`].
    pub const fn group(
        mut self,
        id: &'static str,
        label_key: &'static str,
        icon: &'static str,
    ) -> Self {
        self.group = Some((id, label_key, icon));
        self
    }

    /// Keeps the item out of the sidebar (still searchable and linkable).
    pub const fn hidden(mut self) -> Self {
        self.sidebar = false;
        self
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
    REGISTERED.read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// One rendered entry: a page, or a group of sibling pages (route tabs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavLink {
    /// `active_page` id (a group's own id for a group).
    pub id: &'static str,
    /// Fluent key.
    pub label_key: &'static str,
    /// Resolved href (a group: its first page).
    pub href: String,
    /// Icon name.
    pub icon: &'static str,
    /// Resolved active-state prefix(es), space-separated (a group: one per
    /// page).
    pub match_prefix: String,
    /// In-place navigation allowed.
    pub boost: bool,
    /// The `active_page` ids that make this entry current, space-separated.
    pub ids: String,
    /// A group's pages, in order (empty for a single page).
    pub children: Vec<NavLink>,
}

impl NavLink {
    /// Whether `active` (an `active_page` id) is this entry or one of its
    /// pages.
    pub fn covers(&self, active: &str) -> bool {
        !active.is_empty() && self.ids.split(' ').any(|i| i == active)
    }
}

/// One rendered block of the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavGroup {
    /// The first section of the block.
    pub section: NavSection,
    /// The block.
    pub block: NavBlock,
    /// Fluent key of the heading ("" = no heading).
    pub label_key: &'static str,
    /// Entries.
    pub links: Vec<NavLink>,
}

impl NavBlock {
    /// The sidebar foot (Console settings).
    pub fn is_foot(self) -> bool {
        self == NavBlock::Foot
    }

    /// The current endpoint's block (headed by the switcher).
    pub fn is_endpoint(self) -> bool {
        self == NavBlock::Endpoint
    }

    /// Fluent key of the block heading ("" for the top block and the foot).
    pub fn label_key(self) -> &'static str {
        match self {
            NavBlock::Top | NavBlock::Foot => "",
            NavBlock::Endpoint => "mgr-nav-section-endpoint",
            NavBlock::Terminology => "mgr-nav-section-terminology",
        }
    }

    /// Stable id, used in `data-nav-section` (the endpoint block keeps the
    /// former `data`).
    pub fn id(self) -> &'static str {
        match self {
            NavBlock::Top => "manager",
            NavBlock::Endpoint => "data",
            NavBlock::Terminology => "terminology",
            NavBlock::Foot => "settings",
        }
    }
}

/// The sidebar of a request: `items` (built-ins of the host merged with
/// [`registered`] ones — a registered id replaces a built-in one), the
/// per-endpoint sections only with an `endpoint`, entries `allow` refuses
/// hidden, sorted by section then order, sibling pages of one group folded
/// into one entry, sections folded into the four [`NavBlock`]s.
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
    items.retain(|i| i.sidebar && (endpoint.is_some() || !i.section.per_endpoint()) && allow(i));
    items.sort_by_key(|i| (i.section, i.order));
    let resolve = |s: &str| match endpoint {
        Some(ep) => s.replace("{ep}", ep),
        None => s.to_string(),
    };
    let page = |i: &NavItem| {
        let href = resolve(i.href);
        let match_prefix = i
            .match_prefix
            .map(resolve)
            .unwrap_or_else(|| href.split('?').next().unwrap_or("").to_string());
        NavLink {
            id: i.id,
            label_key: i.label_key,
            href,
            icon: i.icon,
            match_prefix,
            boost: i.boost,
            ids: i.id.to_string(),
            children: Vec::new(),
        }
    };
    let mut groups: Vec<NavGroup> = Vec::new();
    for i in &items {
        let block = i.section.block();
        if groups.last().is_none_or(|g| g.block != block) {
            groups.push(NavGroup {
                section: i.section,
                block,
                label_key: block.label_key(),
                links: Vec::new(),
            });
        }
        let links = &mut groups.last_mut().expect("pushed above").links;
        let link = page(i);
        match i.group {
            Some((gid, label, icon)) => {
                if let Some(g) = links.iter_mut().find(|l| l.id == gid) {
                    g.match_prefix = format!("{} {}", g.match_prefix, link.match_prefix);
                    g.ids = format!("{} {}", g.ids, link.ids);
                    g.children.push(link);
                } else {
                    links.push(NavLink {
                        id: gid,
                        label_key: label,
                        href: link.href.clone(),
                        icon,
                        match_prefix: link.match_prefix.clone(),
                        boost: link.boost,
                        ids: format!("{gid} {}", link.ids),
                        children: vec![link],
                    });
                }
            }
            None => links.push(link),
        }
    }
    // A group the requester can open one page of is that page under the
    // group's label: it keeps the page's id (bookmarked selectors, the
    // server-side current mark) and has no tabs.
    for g in &mut groups {
        for l in &mut g.links {
            if l.children.len() == 1 {
                let only = l.children.pop().expect("one child");
                l.id = only.id;
                l.ids = format!("{} {}", only.ids, l.ids);
            }
        }
    }
    groups
}

/// The route tabs of the page `active` (console redesign §5.1): the pages
/// of the sidebar group it belongs to, when that group has more than one
/// the requester may open.
pub fn tabs<'a>(groups: &'a [NavGroup], active: &str) -> Option<&'a [NavLink]> {
    groups
        .iter()
        .flat_map(|g| &g.links)
        .find(|l| l.children.len() > 1 && l.children.iter().any(|c| c.id == active))
        .map(|l| l.children.as_slice())
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
        assert!(
            viewer
                .iter()
                .flat_map(|g| &g.links)
                .all(|l| l.id != "users")
        );
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
        let top = &admin[0];
        assert_eq!(top.block, NavBlock::Top);
        let ids: Vec<_> = top.links.iter().map(|l| l.id).collect();
        assert_eq!(
            ids,
            ["overview", "test-idp-realms"],
            "IdP sits in the top block, under Overview"
        );
        assert_eq!(admin.last().unwrap().block, NavBlock::Foot);
        assert!(nav_key(None, NavRole::Admin, "").contains("idp"));
        unregister_section(NavSection::AuthorizationServers);
        assert!(
            build(builtin(), None, |_| true)
                .iter()
                .flat_map(|g| &g.links)
                .all(|l| l.id != "test-idp-realms")
        );
    }

    #[test]
    fn sibling_pages_fold_into_one_entry_with_route_tabs() {
        let items = vec![
            NavItem::new(
                NavSection::EndpointData,
                "batch",
                "b",
                "/ui/e/{ep}/data/batch",
            )
            .group("data-tools", "dt", "tool")
            .order(20),
            NavItem::new(
                NavSection::EndpointData,
                "validate",
                "v",
                "/ui/e/{ep}/data/validate",
            )
            .group("data-tools", "dt", "tool")
            .order(30)
            .full_load(),
            NavItem::new(
                NavSection::EndpointData,
                "subs",
                "s",
                "/ui/e/{ep}/data/subs",
            )
            .order(40),
            NavItem::new(
                NavSection::EndpointConfigure,
                "cfg",
                "c",
                "/ui/e/{ep}/config",
            )
            .group("settings", "st", "sliders"),
            NavItem::new(
                NavSection::EndpointConfigure,
                "sec",
                "sc",
                "/ui/e/{ep}/security",
            )
            .group("settings", "st", "sliders")
            .min_role(NavRole::Admin)
            .order(10),
            NavItem::new(NavSection::Settings, "about", "a", "/ui/about").hidden(),
        ];
        // (another test registers an IdP entry in the shared registry meanwhile)
        let all = build(items.clone(), Some("x"), |i| {
            i.section.per_endpoint() || i.id == "about"
        });
        assert_eq!(
            all.len(),
            1,
            "one endpoint block; hidden items draw nothing"
        );
        let links = &all[0].links;
        assert_eq!(links.len(), 3);
        let tools = &links[0];
        assert_eq!(
            (tools.id, tools.label_key, tools.icon),
            ("data-tools", "dt", "tool")
        );
        assert_eq!(tools.href, "/ui/e/x/data/batch");
        assert_eq!(
            tools.match_prefix,
            "/ui/e/x/data/batch /ui/e/x/data/validate"
        );
        assert!(tools.covers("validate") && tools.covers("data-tools") && !tools.covers("subs"));
        assert_eq!(tabs(&all, "validate").unwrap().len(), 2);
        assert!(tabs(&all, "subs").is_none());
        // Without the Admin page the settings group has one page: no tabs.
        let viewer = build(items, Some("x"), |i| {
            i.section.per_endpoint() && i.min_role == NavRole::Viewer
        });
        assert!(tabs(&viewer, "cfg").is_none());
        assert_eq!(viewer[0].links[2].href, "/ui/e/x/config");
    }
}
