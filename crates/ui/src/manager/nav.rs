//! The console's navigation registry (now `helios_ui_chrome::nav`, shared
//! with the embedded terminology console and line IdP; re-exported here).
//!
//! The sidebar is built from [`NavItem`]s grouped by [`NavSection`]. The
//! manager registers its own items; other packages add theirs with
//! [`register`] (P3: Settings → Authentication / Users; P6: Sync jobs) from
//! the code that mounts their routes, so the layout never needs editing to
//! gain an entry.
//!
//! ```no_run
//! use helios_ui::manager::nav::{register, NavItem, NavRole, NavSection};
//! register(
//!     NavItem::new(NavSection::Settings, "settings-users", "nav-settings-users", "/ui/settings/users")
//!         .icon("users")
//!         .min_role(NavRole::Admin)
//!         .order(20),
//! );
//! ```

use helios_endpoints::ConsoleRole;

pub use helios_ui_chrome::nav::{
    NavGroup, NavItem, NavLink, NavRole, NavScope, NavSection, nav_key, register,
};

/// The registry's role for a console role.
pub fn nav_role(role: ConsoleRole) -> NavRole {
    match role {
        ConsoleRole::Viewer => NavRole::Viewer,
        ConsoleRole::Editor => NavRole::Editor,
        ConsoleRole::Admin => NavRole::Admin,
    }
}

const fn item(
    section: NavSection,
    id: &'static str,
    label_key: &'static str,
    href: &'static str,
    icon: &'static str,
    order: u16,
) -> NavItem {
    NavItem::new(section, id, label_key, href)
        .icon(icon)
        .order(order)
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
    for i in v.iter_mut() {
        if matches!(i.id, "tenants" | "settings-auth" | "settings-users") {
            i.min_role = NavRole::Admin;
        }
        // Pages whose own scripts are not lifecycle-clean yet load as a full
        // document (persistent-shell spec §9.4 item 1); every other page is
        // swapped in place.
        if matches!(
            i.id,
            "resources"
                | "batch"
                | "validate"
                | "bulk-import"
                | "bulk-export"
                | "sql-view-definitions"
                | "sql-queries"
                | "sql-views"
                | "sql-export"
                | "search-parameters"
                | "compartments"
                | "tenants"
                | "account"
        ) {
            i.boost = false;
        }
    }
    v
}

/// Every entry the sidebar can show (the manager's own merged with the
/// registered ones, a registered id replacing a built-in one), unfiltered:
/// the global search builds its page index from it.
pub(crate) fn all_items() -> Vec<NavItem> {
    let mut items = builtin();
    for extra in helios_ui_chrome::nav::registered() {
        items.retain(|i| i.id != extra.id);
        items.push(extra);
    }
    items.sort_by_key(|i| (i.section, i.order));
    items
}

/// The roles that decide what the sidebar of one request shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavRoles {
    /// Global console role (server sections, settings).
    pub global: ConsoleRole,
    /// Role on the sidebar's endpoint (per-endpoint sections).
    pub endpoint: ConsoleRole,
    /// Terminology role: max(global, `terminology` binding).
    pub terminology: ConsoleRole,
}

impl NavRoles {
    /// One role for everything.
    pub fn uniform(role: ConsoleRole) -> Self {
        Self {
            global: role,
            endpoint: role,
            terminology: role,
        }
    }

    fn allows(&self, item: &NavItem) -> bool {
        let role = match (item.scope, item.section.per_endpoint()) {
            (NavScope::Terminology, _) => self.terminology,
            (NavScope::Console, true) => self.endpoint,
            (NavScope::Console, false) => self.global,
        };
        nav_role(role) >= item.min_role
    }
}

/// The sidebar for a request: per-endpoint sections only with an endpoint,
/// entries above the role hidden, registered entries merged in.
pub fn build(endpoint: Option<&str>, roles: NavRoles) -> Vec<NavGroup> {
    helios_ui_chrome::nav::build(builtin(), endpoint, |i| roles.allows(i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_sections_need_an_endpoint() {
        let without = build(None, NavRoles::uniform(ConsoleRole::Admin));
        assert!(without.iter().all(|g| !g.section.per_endpoint()));
        let with = build(Some("twcore"), NavRoles::uniform(ConsoleRole::Admin));
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
        let viewer = build(None, NavRoles::uniform(ConsoleRole::Viewer));
        assert!(
            viewer
                .iter()
                .flat_map(|g| &g.links)
                .all(|l| l.id != "tenants")
        );
        register(
            NavItem::new(
                NavSection::Settings,
                "test-settings-extra",
                "mgr-nav-about",
                "/ui/settings/extra",
            )
            .icon("users")
            .min_role(NavRole::Admin)
            .order(1),
        );
        let admin = build(None, NavRoles::uniform(ConsoleRole::Admin));
        let settings = admin
            .iter()
            .find(|g| g.section == NavSection::Settings)
            .unwrap();
        assert_eq!(settings.links[0].id, "test-settings-extra");
        assert!(
            build(None, NavRoles::uniform(ConsoleRole::Viewer))
                .iter()
                .flat_map(|g| &g.links)
                .all(|l| l.id != "test-settings-extra")
        );
    }
}
