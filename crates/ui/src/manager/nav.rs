//! The console's navigation registry.
//!
//! The sidebar is built from [`NavItem`]s grouped by [`NavSection`]. The
//! manager registers its own items; other packages add theirs with
//! [`register`] (P3: Settings → Authentication / Users; P6: Sync jobs) from
//! the code that mounts their routes, so the layout never needs editing to
//! gain an entry.
//!
//! ```no_run
//! use helios_ui::manager::nav::{register, NavItem, NavSection};
//! use helios_endpoints::ConsoleRole;
//! register(NavItem {
//!     section: NavSection::Settings,
//!     id: "settings-users",
//!     label_key: "nav-settings-users",
//!     href: "/ui/settings/users",
//!     icon: "users",
//!     min_role: ConsoleRole::Admin,
//!     order: 20,
//! });
//! ```

use std::sync::RwLock;

use helios_endpoints::ConsoleRole;

/// Where an item lives in the sidebar.
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
    /// Server-wide operations (sync jobs of every endpoint).
    Server,
    /// Console settings (users, sign-in, server).
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
}

/// One sidebar entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavItem {
    /// Section.
    pub section: NavSection,
    /// The page's `active_page` id (marks the entry current).
    pub id: &'static str,
    /// Fluent key of the label.
    pub label_key: &'static str,
    /// Target; `{ep}` = current endpoint name.
    pub href: &'static str,
    /// Icon name (see `templates/partials/nav/icon.html`).
    pub icon: &'static str,
    /// Hidden below this role.
    pub min_role: ConsoleRole,
    /// Sort key within the section.
    pub order: u16,
}

static REGISTERED: RwLock<Vec<NavItem>> = RwLock::new(Vec::new());

/// Adds an entry (idempotent per `id`: a second registration replaces the
/// first).
pub fn register(item: NavItem) {
    let mut items = REGISTERED.write().unwrap_or_else(|p| p.into_inner());
    items.retain(|i| i.id != item.id);
    items.push(item);
}

const fn item(
    section: NavSection,
    id: &'static str,
    label_key: &'static str,
    href: &'static str,
    icon: &'static str,
    order: u16,
) -> NavItem {
    NavItem {
        section,
        id,
        label_key,
        href,
        icon,
        min_role: ConsoleRole::Viewer,
        order,
    }
}

/// The manager's own entries.
fn builtin() -> Vec<NavItem> {
    use NavSection::*;
    let mut v = vec![
        item(Manager, "overview", "mgr-nav-overview", "/ui", "grid", 0),
        item(
            EndpointData,
            "ep-home",
            "mgr-nav-endpoint-home",
            "/ui/e/{ep}",
            "home",
            0,
        ),
        item(
            EndpointData,
            "resources",
            "nav-resources",
            "/ui/e/{ep}/data/resources",
            "layers",
            10,
        ),
        item(
            EndpointData,
            "batch",
            "nav-batch-transaction",
            "/ui/e/{ep}/data/batch",
            "send",
            20,
        ),
        item(
            EndpointData,
            "validate",
            "nav-validate",
            "/ui/e/{ep}/data/validate",
            "check-circle",
            30,
        ),
        item(
            EndpointData,
            "bulk-import",
            "nav-import",
            "/ui/e/{ep}/data/bulk-import",
            "import",
            40,
        ),
        item(
            EndpointData,
            "bulk-export",
            "nav-export",
            "/ui/e/{ep}/data/bulk-export",
            "export",
            50,
        ),
        item(
            EndpointData,
            "sql-view-definitions",
            "nav-sql-view-definitions",
            "/ui/e/{ep}/data/sql/view-definitions",
            "grid",
            60,
        ),
        item(
            EndpointData,
            "sql-queries",
            "nav-sql-queries",
            "/ui/e/{ep}/data/sql/queries",
            "code",
            61,
        ),
        item(
            EndpointData,
            "sql-views",
            "nav-sql-views",
            "/ui/e/{ep}/data/sql/views",
            "layers-platforms",
            62,
        ),
        item(
            EndpointData,
            "sql-export",
            "nav-sql-export",
            "/ui/e/{ep}/data/sql/export",
            "export",
            63,
        ),
        item(
            EndpointData,
            "capability-statement",
            "nav-capability-conformance",
            "/ui/e/{ep}/data/capability-statement",
            "shield",
            70,
        ),
        item(
            EndpointData,
            "search-parameters",
            "nav-search-parameters",
            "/ui/e/{ep}/data/search-parameters",
            "search",
            71,
        ),
        item(
            EndpointData,
            "compartments",
            "nav-compartments",
            "/ui/e/{ep}/data/compartments",
            "compartments",
            72,
        ),
        item(
            EndpointData,
            "subscriptions",
            "nav-subscriptions",
            "/ui/e/{ep}/data/subscriptions",
            "sync",
            73,
        ),
        item(
            EndpointConfigure,
            "ep-config",
            "mgr-nav-config",
            "/ui/e/{ep}/config",
            "sliders",
            0,
        ),
        item(
            EndpointConfigure,
            "ep-security",
            "mgr-nav-security",
            "/ui/e/{ep}/security",
            "shield",
            10,
        ),
        item(
            EndpointConfigure,
            "ep-validation",
            "mgr-nav-validation",
            "/ui/e/{ep}/validation",
            "check-circle",
            20,
        ),
        item(
            EndpointConfigure,
            "ep-terminology",
            "mgr-nav-terminology",
            "/ui/e/{ep}/terminology",
            "book",
            30,
        ),
        item(
            EndpointOperate,
            "ep-sync",
            "mgr-nav-sync",
            "/ui/e/{ep}/sync",
            "sync",
            0,
        ),
        item(
            EndpointOperate,
            "ep-audit",
            "mgr-nav-audit",
            "/ui/e/{ep}/audit",
            "history",
            10,
        ),
        item(
            EndpointOperate,
            "ep-history",
            "mgr-nav-history",
            "/ui/e/{ep}/history",
            "pencil",
            20,
        ),
        item(
            Settings,
            "settings-auth",
            "mgr-nav-settings-auth",
            "/ui/settings/auth",
            "shield",
            10,
        ),
        item(
            Settings,
            "settings-users",
            "mgr-nav-settings-users",
            "/ui/settings/users",
            "users",
            20,
        ),
        item(
            Settings,
            "account",
            "mgr-nav-account",
            "/ui/account",
            "key",
            80,
        ),
        item(
            Settings,
            "tenants",
            "nav-tenants",
            "/ui/tenants",
            "building",
            50,
        ),
        item(Settings, "about", "mgr-nav-about", "/ui/about", "info", 90),
    ];
    v.iter_mut()
        .filter(|i| matches!(i.id, "tenants" | "settings-auth" | "settings-users"))
        .for_each(|i| i.min_role = ConsoleRole::Admin);
    v
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

/// The sidebar for a request: per-endpoint sections only with an endpoint,
/// entries above the role hidden, registered entries merged in.
pub fn build(endpoint: Option<&str>, role: ConsoleRole) -> Vec<NavGroup> {
    let mut items = builtin();
    for extra in REGISTERED.read().unwrap_or_else(|p| p.into_inner()).iter() {
        items.retain(|i| i.id != extra.id);
        items.push(extra.clone());
    }
    items.retain(|i| role >= i.min_role && (endpoint.is_some() || !i.section.per_endpoint()));
    items.sort_by_key(|i| (i.section, i.order));
    let mut groups: Vec<NavGroup> = Vec::new();
    for i in items {
        let href = match endpoint {
            Some(ep) => i.href.replace("{ep}", ep),
            None => i.href.to_string(),
        };
        let link = NavLink {
            id: i.id,
            label_key: i.label_key,
            href,
            icon: i.icon,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_sections_need_an_endpoint() {
        let without = build(None, ConsoleRole::Admin);
        assert!(without.iter().all(|g| !g.section.per_endpoint()));
        let with = build(Some("twcore"), ConsoleRole::Admin);
        let data = with
            .iter()
            .find(|g| g.section == NavSection::EndpointData)
            .unwrap();
        assert_eq!(data.links[0].href, "/ui/e/twcore");
        assert!(
            data.links
                .iter()
                .any(|l| l.href == "/ui/e/twcore/data/resources")
        );
        // Sections are in declaration order.
        let order: Vec<_> = with.iter().map(|g| g.section).collect();
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(order, sorted);
    }

    #[test]
    fn roles_hide_entries_and_registration_merges() {
        let viewer = build(None, ConsoleRole::Viewer);
        assert!(
            viewer
                .iter()
                .flat_map(|g| &g.links)
                .all(|l| l.id != "tenants")
        );
        register(NavItem {
            section: NavSection::Settings,
            id: "test-settings-extra",
            label_key: "mgr-nav-about",
            href: "/ui/settings/extra",
            icon: "users",
            min_role: ConsoleRole::Admin,
            order: 10,
        });
        let admin = build(None, ConsoleRole::Admin);
        let settings = admin
            .iter()
            .find(|g| g.section == NavSection::Settings)
            .unwrap();
        assert_eq!(settings.links[0].id, "test-settings-extra");
        assert!(
            build(None, ConsoleRole::Viewer)
                .iter()
                .flat_map(|g| &g.links)
                .all(|l| l.id != "test-settings-extra")
        );
    }
}
