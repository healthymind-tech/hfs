//! The Verify panel (UX audit UX-02) for diagnostic reports: an ordered
//! checklist, each step pass / warn / fail / skipped with duration, each
//! check with a human message, "How to fix", "Go to field" and the URL
//! contacted. Messages are translated by code (`idpdiag-<code>`,
//! `idpdiag-<code>-fix`, with the check's params); a code without a catalog
//! entry falls back to the report's English text. Status is conveyed in
//! text and icon, not colour only.
//!
//! The markup is `templates/partials/idp/verify.html`; it follows the
//! contract of the console's shared Verify component (U45X) so it can be
//! swapped for it without changing the report.

use helios_idp_admin::diagnostic::{Check, Report, Status, Step};

use crate::i18n::I18n;

pub(crate) struct CheckView {
    pub status: &'static str,
    pub icon: &'static str,
    pub status_label: String,
    pub message: String,
    pub fix: String,
    pub goto: Option<String>,
    pub url: String,
    pub spec: String,
}

pub(crate) struct StepView {
    pub n: u8,
    pub title: String,
    pub status: &'static str,
    pub icon: &'static str,
    pub status_label: String,
    pub duration: String,
    pub checks: Vec<CheckView>,
    /// Open by default (failed or warned).
    pub open: bool,
}

pub(crate) struct VerifyView {
    pub status: &'static str,
    pub icon: &'static str,
    pub headline: String,
    pub detail: String,
    pub when: String,
    pub steps: Vec<StepView>,
    /// The first failing check (fix this first).
    pub first_problem: Option<CheckView>,
}

pub(crate) fn look(s: Status) -> (&'static str, &'static str, &'static str) {
    match s {
        Status::Pass => ("ok", "check-circle", "idpc-verify-pass"),
        Status::Warn => ("warn", "alert", "idpc-verify-warn"),
        Status::Fail => ("danger", "alert", "idpc-verify-fail"),
        Status::Skipped => ("muted", "info", "idpc-verify-skipped"),
    }
}

/// Where "Go to field" leads for a check's field anchor.
pub(crate) fn field_href(realm: &str, field: &str, in_wizard: bool) -> String {
    let w = |step: u8, anchor: &str| format!("/ui/idp/new/{realm}/{step}#{anchor}");
    let m = |page: &str, anchor: &str| format!("/ui/idp/{realm}/{page}#{anchor}");
    if let Some(rest) = field.strip_prefix("client.") {
        let (client, f) = rest.split_once('.').unwrap_or((rest, ""));
        let anchor = if f.is_empty() {
            "f-name".to_string()
        } else {
            format!("f-{f}")
        };
        return if in_wizard {
            w(4, &format!("client-{client}"))
        } else {
            format!("/ui/idp/{realm}/clients/{client}#{anchor}")
        };
    }
    if field == "keys" {
        return if in_wizard {
            w(2, "keys")
        } else {
            m("keys", "keys")
        };
    }
    if field == "clients" {
        return if in_wizard {
            w(4, "f-client_name")
        } else {
            m("clients", "f-client_name")
        };
    }
    if field == "users" {
        return if in_wizard {
            w(3, "f-user_username")
        } else {
            m("users", "f-user_username")
        };
    }
    if field == "endpoints" || field.starts_with("endpoints.") {
        let ep = field.strip_prefix("endpoints.").unwrap_or("");
        let anchor = if ep.is_empty() {
            "endpoints".to_string()
        } else {
            format!("ep-{ep}")
        };
        return if in_wizard {
            w(6, &anchor)
        } else {
            m("settings", &anchor)
        };
    }
    if let Some(f) = field.strip_prefix("realm.") {
        let step = if f.starts_with("features") || f.starts_with("lifetimes") {
            5
        } else if f.starts_with("security") || f.starts_with("userSource") {
            3
        } else if f.starts_with("signingAlgs")
            || f.starts_with("rotation")
            || f.starts_with("rsaBits")
        {
            2
        } else {
            1
        };
        let anchor = format!("f-{f}");
        return if in_wizard {
            w(step, &anchor)
        } else {
            m("settings", &anchor)
        };
    }
    m("settings", &format!("f-{field}"))
}

fn translate(i18n: &I18n, c: &Check) -> (String, String) {
    let key = format!("idpdiag-{}", c.code);
    let msg = i18n.t_args(&key, &c.params);
    let message = if msg == key { c.message.clone() } else { msg };
    let fix = if c.status == Status::Pass || c.status == Status::Skipped {
        String::new()
    } else {
        let fkey = format!("{key}-fix");
        let f = i18n.t_args(&fkey, &c.params);
        if f == fkey { c.fix.clone() } else { f }
    };
    (message, fix)
}

fn check_view(i18n: &I18n, realm: &str, c: &Check, in_wizard: bool) -> CheckView {
    let (status, icon, label) = look(c.status);
    let (message, fix) = translate(i18n, c);
    CheckView {
        status,
        icon,
        status_label: i18n.t(label),
        message,
        fix,
        goto: c
            .field
            .as_deref()
            .filter(|_| matches!(c.status, Status::Fail | Status::Warn))
            .map(|f| field_href(realm, f, in_wizard)),
        url: c.url.clone().unwrap_or_default(),
        spec: c.spec.clone().unwrap_or_default(),
    }
}

fn step_title(i18n: &I18n, s: &Step) -> String {
    let key = format!("idpdiag-step-{}", s.id);
    let t = i18n.t(&key);
    if t == key { s.title.clone() } else { t }
}

pub(crate) fn view(i18n: &I18n, r: &Report, in_wizard: bool) -> VerifyView {
    let realm = r.realm.as_str();
    let steps: Vec<StepView> = r
        .steps
        .iter()
        .map(|s| {
            let (status, icon, label) = look(s.status);
            StepView {
                n: s.n,
                title: step_title(i18n, s),
                status,
                icon,
                status_label: i18n.t(label),
                duration: format!("{} ms", s.duration_ms),
                checks: s
                    .checks
                    .iter()
                    .map(|c| check_view(i18n, realm, c, in_wizard))
                    .collect(),
                open: matches!(s.status, Status::Fail | Status::Warn),
            }
        })
        .collect();
    let problems = r.problems();
    let warnings = r.warnings();
    let (status, icon, _) = look(r.status);
    let (headline, detail) = match r.status {
        Status::Pass if warnings.is_empty() => (
            i18n.t("idpc-verify-all-passed"),
            i18n.t("idpc-verify-all-passed-detail"),
        ),
        Status::Pass | Status::Warn => (
            i18n.t_arg("idpc-verify-warnings", "count", warnings.len() as i64),
            i18n.t("idpc-verify-warnings-detail"),
        ),
        Status::Fail => (
            i18n.t_arg("idpc-verify-problems", "count", problems.len() as i64),
            i18n.t("idpc-verify-problems-detail"),
        ),
        Status::Skipped => (i18n.t("idpc-verify-nothing"), String::new()),
    };
    VerifyView {
        status,
        icon,
        headline,
        detail,
        when: r.finished_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        steps,
        first_problem: problems
            .first()
            .map(|c| check_view(i18n, realm, c, in_wizard)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_links() {
        assert_eq!(
            field_href("r", "client.app.redirectUris", false),
            "/ui/idp/r/clients/app#f-redirectUris"
        );
        assert_eq!(
            field_href("r", "client.app.redirectUris", true),
            "/ui/idp/new/r/4#client-app"
        );
        assert_eq!(field_href("r", "keys", false), "/ui/idp/r/keys#keys");
        assert_eq!(
            field_href("r", "endpoints.ep1", false),
            "/ui/idp/r/settings#ep-ep1"
        );
        assert_eq!(
            field_href("r", "endpoints.ep1", true),
            "/ui/idp/new/r/6#ep-ep1"
        );
        assert_eq!(
            field_href("r", "realm.lifetimes.accessToken", true),
            "/ui/idp/new/r/5#f-lifetimes.accessToken"
        );
        assert_eq!(
            field_href("r", "realm.enabled", false),
            "/ui/idp/r/settings#f-enabled"
        );
    }
}
