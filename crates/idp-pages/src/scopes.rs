//! Plain-language descriptions of requested scopes for the consent screen
//! (design §8.2 step 4: "a plain-language scope list (resource names,
//! read/write, granular filters), with per-scope checkboxes").

use helios_idp::scope::{Scope, ScopeContext, parse_scope};

use crate::i18n::I18n;

/// One line of the consent screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeLine {
    /// The scope token as requested.
    pub scope: String,
    /// What it allows, in the user's language.
    pub label: String,
    /// Secondary text (context, filter).
    pub detail: String,
    /// The user may untick it. Identity and launch-context scopes are
    /// always part of the request: without them the app cannot start.
    pub optional: bool,
}

fn ops(i: &I18n, letters: &str) -> String {
    let words: Vec<String> = letters
        .chars()
        .map(|c| i.t(&format!("idp-op-{c}")))
        .collect();
    words.join(", ")
}

/// Describes `scope` for the consent screen.
pub fn describe(i: &I18n, scope: &str) -> ScopeLine {
    let line = |label: String, detail: String, optional: bool| ScopeLine {
        scope: scope.to_string(),
        label,
        detail,
        optional,
    };
    match parse_scope(scope) {
        Scope::OpenId => line(i.t("idp-scope-openid"), String::new(), false),
        Scope::FhirUser => line(i.t("idp-scope-fhiruser"), String::new(), false),
        Scope::Profile => line(i.t("idp-scope-profile"), String::new(), true),
        Scope::Email => line(i.t("idp-scope-email"), String::new(), true),
        Scope::OfflineAccess => line(i.t("idp-scope-offline"), String::new(), true),
        Scope::OnlineAccess => line(i.t("idp-scope-online"), String::new(), true),
        Scope::Launch => line(i.t("idp-scope-launch"), String::new(), false),
        Scope::LaunchContext { resource, .. } => {
            let label = match resource.as_str() {
                "patient" => i.t("idp-scope-launch-patient"),
                "encounter" => i.t("idp-scope-launch-encounter"),
                other => i.t_arg("idp-scope-launch-other", "resource", other),
            };
            line(label, String::new(), false)
        }
        Scope::Resource(r) => {
            let letters = r.perms.letters();
            let verb = match letters.as_str() {
                "rs" | "r" => i.t("idp-verb-read"),
                "cud" => i.t("idp-verb-write"),
                "cruds" => i.t("idp-verb-rw"),
                other => i.t_arg("idp-verb-custom", "ops", ops(i, other)),
            };
            let what = if r.resource_type == "*" {
                i.t("idp-what-all")
            } else {
                i.t_arg("idp-what-type", "type", r.resource_type.clone())
            };
            let mut detail = match r.context {
                ScopeContext::Patient => i.t("idp-ctx-patient"),
                ScopeContext::User => i.t("idp-ctx-user"),
                ScopeContext::System => i.t("idp-ctx-system"),
            };
            if let Some(q) = &r.query {
                detail.push(' ');
                detail.push_str(&i.t_arg("idp-scope-filter", "filter", q.clone()));
            }
            line(
                i.t_arg2("idp-scope-resource", "verb", verb, "what", what),
                detail,
                true,
            )
        }
        Scope::Other(s) => line(i.t_arg("idp-scope-other", "scope", s), String::new(), true),
    }
}

/// The granted scopes from the consent form: every non-optional requested
/// scope, plus the optional ones the user left ticked — never anything that
/// was not requested.
pub fn granted(requested: &[String], ticked: &[String]) -> Vec<String> {
    requested
        .iter()
        .filter(|s| {
            !matches!(
                parse_scope(s),
                Scope::Profile
                    | Scope::Email
                    | Scope::OfflineAccess
                    | Scope::OnlineAccess
                    | Scope::Resource(_)
                    | Scope::Other(_)
            ) || ticked.iter().any(|t| t == *s)
        })
        .cloned()
        .collect()
}

/// Whether a stored consent covers every requested scope.
pub fn covered(stored: &[String], requested: &[String]) -> bool {
    requested.iter().all(|r| stored.iter().any(|s| s == r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions() {
        let i = I18n::default();
        let d = describe(&i, "patient/Observation.rs?category=laboratory");
        assert_eq!(d.label, "Read Observation records");
        assert!(d.detail.contains("selected patient"));
        assert!(d.detail.contains("category=laboratory"));
        assert!(d.optional);
        assert_eq!(
            describe(&i, "user/*.cruds").label,
            "Read, create, change and delete all data"
        );
        assert_eq!(
            describe(&i, "patient/Patient.read").label,
            "Read Patient records"
        );
        assert_eq!(
            describe(&i, "system/Condition.cu").label,
            "create, change Condition records"
        );
        assert!(!describe(&i, "openid").optional);
        assert!(!describe(&i, "launch/patient").optional);
        assert!(describe(&i, "offline_access").optional);
        assert!(describe(&i, "weird").label.contains("weird"));
    }

    #[test]
    fn granted_never_adds_unrequested_scopes() {
        let req: Vec<String> = [
            "openid",
            "launch/patient",
            "patient/Observation.rs",
            "offline_access",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let ticked = vec![
            "patient/Observation.rs".to_string(),
            "system/*.cruds".to_string(),
        ];
        assert_eq!(
            granted(&req, &ticked),
            vec!["openid", "launch/patient", "patient/Observation.rs"]
        );
        assert_eq!(granted(&req, &[]), vec!["openid", "launch/patient"]);
        assert!(covered(&req, &req[..2]));
        assert!(!covered(&req[..2], &req));
    }
}
