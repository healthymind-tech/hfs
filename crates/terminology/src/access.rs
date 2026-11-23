//! Who may do what on the terminology surface (design §7.2).
//!
//! One rule table for the console pages (`/ui/terminology/**`), the admin
//! API (`/admin/terminology/**`) and the `/tx` write guard, replacing the
//! HTS password gate. Roles are the console roles of helios-endpoints.

use helios_endpoints::ConsoleRole;

/// Terminology actions, coarse enough for one rule each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminologyAction {
    /// List / read CodeSystems, ValueSets, ConceptMaps, concepts; home.
    Browse,
    /// `$lookup` `$validate-code` `$subsumes` `$expand` `$translate` workbenches.
    Workbench,
    /// Create / update / delete a single terminology resource from the console.
    EditResource,
    /// Upload, server-side import, registry install, bootstrap re-sync.
    Import,
    /// Delete a code system version, set current version, compact.
    Maintain,
    /// Job list with requester addresses, terminology audit trail, config.
    ViewOperations,
}

/// Minimal console role for an action. `None` = public when the console
/// mode is `none` and the route is listed public (browsing stays as open as
/// it is today unless sign-in is required).
pub fn required_role(action: TerminologyAction) -> ConsoleRole {
    match action {
        TerminologyAction::Browse | TerminologyAction::Workbench => ConsoleRole::Viewer,
        TerminologyAction::EditResource => ConsoleRole::Editor,
        TerminologyAction::Import
        | TerminologyAction::Maintain
        | TerminologyAction::ViewOperations => ConsoleRole::Admin,
    }
}

/// Classify a unified console path (`/ui/terminology/...`) and method.
pub fn classify_console(method: &http::Method, path: &str) -> Option<TerminologyAction> {
    let rest = path.strip_prefix("/ui/terminology")?;
    let rest = rest.trim_start_matches('/');
    let first = rest.split('/').next().unwrap_or("");
    let write = method != http::Method::GET && method != http::Method::HEAD;
    Some(match first {
        "import" | "uploads" | "registry" => TerminologyAction::Import,
        "manage" => {
            if write {
                TerminologyAction::Maintain
            } else {
                TerminologyAction::ViewOperations
            }
        }
        "jobs" | "audit" => {
            if write {
                TerminologyAction::Import
            } else {
                TerminologyAction::ViewOperations
            }
        }
        _ if rest.contains("/workbench/")
            || rest.ends_with("/lookup")
            || rest.ends_with("/validate")
            || rest.ends_with("/subsumes")
            || rest.ends_with("/expand")
            || rest.ends_with("/translate") =>
        {
            TerminologyAction::Workbench
        }
        _ if write => TerminologyAction::EditResource,
        _ => TerminologyAction::Browse,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Method;

    #[test]
    fn rule_table() {
        let get = Method::GET;
        let post = Method::POST;
        let c = |m: &Method, p: &str| classify_console(m, p).map(required_role);
        assert_eq!(c(&get, "/ui/terminology"), Some(ConsoleRole::Viewer));
        assert_eq!(
            c(&get, "/ui/terminology/code-systems/abc"),
            Some(ConsoleRole::Viewer)
        );
        assert_eq!(
            c(&post, "/ui/terminology/value-sets/x/expand"),
            Some(ConsoleRole::Viewer)
        );
        assert_eq!(
            c(&post, "/ui/terminology/code-systems/x"),
            Some(ConsoleRole::Editor)
        );
        assert_eq!(
            c(&get, "/ui/terminology/explore/data"),
            Some(ConsoleRole::Viewer)
        );
        assert_eq!(c(&get, "/ui/terminology/import"), Some(ConsoleRole::Admin));
        assert_eq!(c(&get, "/ui/terminology/jobs/7"), Some(ConsoleRole::Admin));
        assert_eq!(
            c(&post, "/ui/terminology/manage/delete"),
            Some(ConsoleRole::Admin)
        );
        assert_eq!(c(&get, "/ui/e/default/data"), None);
    }
}
