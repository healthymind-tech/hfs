//! Console sign-in configuration: environment bootstrap, pinned fields and
//! validation (design §7.4).
//!
//! The settings live in `hfs_console_settings` and are edited in the console
//! by admins. Environment variables bootstrap them and, when set, **pin**
//! their field: the effective value is always the environment's, the console
//! shows it read-only, and a save that tries to change it is refused. That
//! guarantees a recovery path from a bad configuration (set the variable and
//! restart).

use std::collections::BTreeSet;

use helios_endpoints::console::{
    ConsoleAuthMode, ConsoleAuthSettings, ConsoleRole, OidcClientSettings, RoleMapping,
};

use crate::settings::{Secret, normalize_origin};

/// Field paths (camelCase, as in the JSON body of `/admin/console/settings`).
pub mod field {
    /// `mode`.
    pub const MODE: &str = "mode";
    /// `anonymousRole`.
    pub const ANONYMOUS_ROLE: &str = "anonymousRole";
    /// `oidc.issuer`.
    pub const OIDC_ISSUER: &str = "oidc.issuer";
    /// `oidc.clientId`.
    pub const OIDC_CLIENT_ID: &str = "oidc.clientId";
    /// `oidc.clientSecret`.
    pub const OIDC_CLIENT_SECRET: &str = "oidc.clientSecret";
    /// `oidc.scopes`.
    pub const OIDC_SCOPES: &str = "oidc.scopes";
    /// `oidc.roleClaim`.
    pub const OIDC_ROLE_CLAIM: &str = "oidc.roleClaim";
    /// `oidc.roleMapping.admin`.
    pub const OIDC_ADMIN_VALUES: &str = "oidc.roleMapping.admin";
    /// `oidc.roleMapping.editor`.
    pub const OIDC_EDITOR_VALUES: &str = "oidc.roleMapping.editor";
    /// `oidc.roleMapping.viewer`.
    pub const OIDC_VIEWER_VALUES: &str = "oidc.roleMapping.viewer";
    /// `oidc.defaultRole`.
    pub const OIDC_DEFAULT_ROLE: &str = "oidc.defaultRole";
    /// `sessionIdleMinutes`.
    pub const SESSION_IDLE: &str = "sessionIdleMinutes";
    /// `sessionAbsoluteHours`.
    pub const SESSION_ABSOLUTE: &str = "sessionAbsoluteHours";
    /// `breakGlass`.
    pub const BREAK_GLASS: &str = "breakGlass";
    /// `local.minPasswordLength`.
    pub const MIN_PASSWORD_LENGTH: &str = "local.minPasswordLength";
    /// `local.lockoutFailures`.
    pub const LOCKOUT_FAILURES: &str = "local.lockoutFailures";
    /// `local.lockoutMinutes`.
    pub const LOCKOUT_MINUTES: &str = "local.lockoutMinutes";
    /// `allowedOrigins`.
    pub const ALLOWED_ORIGINS: &str = "allowedOrigins";
}

/// AAD of the sealed OIDC client secret.
pub const CLIENT_SECRET_AAD: &str = "console:oidc.clientSecret";

/// What the environment says: bootstrap values that pin their fields.
#[derive(Clone, Debug, Default)]
pub struct EnvBootstrap {
    /// Pinned field paths ([`field`]).
    pub pinned: BTreeSet<&'static str>,
    mode: Option<ConsoleAuthMode>,
    anonymous_role: Option<Option<ConsoleRole>>,
    issuer: Option<String>,
    client_id: Option<String>,
    /// `HFS_CONSOLE_OIDC_CLIENT_SECRET(_FILE)` — never stored in the DB.
    pub client_secret: Option<Secret>,
    scopes: Option<String>,
    role_claim: Option<String>,
    admin_values: Option<Vec<String>>,
    editor_values: Option<Vec<String>>,
    viewer_values: Option<Vec<String>>,
    default_role: Option<Option<ConsoleRole>>,
    session_idle: Option<u32>,
    session_absolute: Option<u32>,
    break_glass: Option<bool>,
    min_password_length: Option<u8>,
    lockout_failures: Option<u8>,
    lockout_minutes: Option<u16>,
    allowed_origins: Option<Vec<String>>,
    /// `HFS_CONSOLE_ALLOW_ANONYMOUS_ADMIN=true` (development only).
    pub allow_anonymous_admin: bool,
    /// Whether any `HFS_CONSOLE_AUTH_MODE` was given explicitly (the opt-in
    /// that installs console sign-in without `HFS_MANAGER=on`).
    pub explicit_mode: bool,
    /// Problems found reading the environment (logged at startup).
    pub warnings: Vec<String>,
}

fn list(raw: &str) -> Vec<String> {
    raw.split([',', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_role_opt(raw: &str) -> Result<Option<ConsoleRole>, ()> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "none" | "null" => Ok(None),
        other => ConsoleRole::parse(other).map(Some).ok_or(()),
    }
}

impl EnvBootstrap {
    /// From the process environment.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Through `get` (the environment, or a map in tests).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let var = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let mut env = Self::default();
        let mut warnings = Vec::new();

        if let Some(m) = var("HFS_CONSOLE_AUTH_MODE") {
            match m.to_ascii_lowercase().as_str() {
                "none" => env.mode = Some(ConsoleAuthMode::None),
                "oidc" => env.mode = Some(ConsoleAuthMode::Oidc),
                "local" => env.mode = Some(ConsoleAuthMode::Local),
                _ => warnings.push(format!(
                    "HFS_CONSOLE_AUTH_MODE={m:?} is not none|oidc|local; ignored"
                )),
            }
            if env.mode.is_some() {
                env.pinned.insert(field::MODE);
                env.explicit_mode = true;
            }
        }
        if let Some(r) = var("HFS_CONSOLE_ANONYMOUS_ROLE") {
            match parse_role_opt(&r) {
                Ok(role) => {
                    env.anonymous_role = Some(role);
                    env.pinned.insert(field::ANONYMOUS_ROLE);
                }
                Err(()) => warnings.push(format!(
                    "HFS_CONSOLE_ANONYMOUS_ROLE={r:?} is not viewer|editor|admin|none; ignored"
                )),
            }
        }

        // OIDC: the new variables, else the legacy `HFS_UI_LOGIN_*` (#1449)
        // with `HFS_AUTH_ISSUER`, so an upstream-style deployment keeps its
        // IdP client when it switches to mode oidc.
        let new_client = var("HFS_CONSOLE_OIDC_CLIENT_ID");
        let legacy = new_client.is_none() && var("HFS_UI_LOGIN_CLIENT_ID").is_some();
        let (issuer, client, secret, secret_file, scopes) = if legacy {
            (
                var("HFS_AUTH_ISSUER"),
                var("HFS_UI_LOGIN_CLIENT_ID"),
                var("HFS_UI_LOGIN_CLIENT_SECRET"),
                None,
                var("HFS_UI_LOGIN_SCOPES"),
            )
        } else {
            (
                var("HFS_CONSOLE_OIDC_ISSUER"),
                new_client,
                get("HFS_CONSOLE_OIDC_CLIENT_SECRET").filter(|v| !v.trim().is_empty()),
                var("HFS_CONSOLE_OIDC_CLIENT_SECRET_FILE"),
                var("HFS_CONSOLE_OIDC_SCOPES"),
            )
        };
        if let Some(v) = issuer {
            env.issuer = Some(v);
            env.pinned.insert(field::OIDC_ISSUER);
        }
        if let Some(v) = client {
            env.client_id = Some(v);
            env.pinned.insert(field::OIDC_CLIENT_ID);
        }
        match (secret_file, secret) {
            (Some(path), _) => match std::fs::read_to_string(&path) {
                Ok(s) if !s.trim().is_empty() => {
                    env.client_secret = Some(Secret::new(s.trim_end_matches(['\r', '\n'])));
                    env.pinned.insert(field::OIDC_CLIENT_SECRET);
                }
                Ok(_) => warnings.push(format!("HFS_CONSOLE_OIDC_CLIENT_SECRET_FILE ({path}) is empty")),
                Err(e) => warnings.push(format!(
                    "cannot read HFS_CONSOLE_OIDC_CLIENT_SECRET_FILE ({path}): {}",
                    e.kind()
                )),
            },
            (None, Some(s)) => {
                env.client_secret = Some(Secret::new(s));
                env.pinned.insert(field::OIDC_CLIENT_SECRET);
            }
            (None, None) => {}
        }
        if let Some(v) = scopes {
            env.scopes = Some(v);
            env.pinned.insert(field::OIDC_SCOPES);
        }
        if let Some(v) = var("HFS_CONSOLE_OIDC_ROLE_CLAIM") {
            env.role_claim = Some(v);
            env.pinned.insert(field::OIDC_ROLE_CLAIM);
        }
        for (name, slot, path) in [
            ("HFS_CONSOLE_OIDC_ADMIN_VALUES", &mut env.admin_values, field::OIDC_ADMIN_VALUES),
            ("HFS_CONSOLE_OIDC_EDITOR_VALUES", &mut env.editor_values, field::OIDC_EDITOR_VALUES),
            ("HFS_CONSOLE_OIDC_VIEWER_VALUES", &mut env.viewer_values, field::OIDC_VIEWER_VALUES),
        ] {
            if let Some(v) = var(name) {
                *slot = Some(list(&v));
                env.pinned.insert(path);
            }
        }
        if let Some(r) = var("HFS_CONSOLE_OIDC_DEFAULT_ROLE") {
            match parse_role_opt(&r) {
                Ok(role) => {
                    env.default_role = Some(role);
                    env.pinned.insert(field::OIDC_DEFAULT_ROLE);
                }
                Err(()) => warnings.push(format!(
                    "HFS_CONSOLE_OIDC_DEFAULT_ROLE={r:?} is not viewer|editor|admin|none; ignored"
                )),
            }
        }
        macro_rules! num {
            ($name:literal, $slot:expr, $path:expr) => {
                if let Some(v) = var($name) {
                    match v.parse() {
                        Ok(n) => {
                            $slot = Some(n);
                            env.pinned.insert($path);
                        }
                        Err(_) => warnings.push(format!("{}={v:?} is not a number; ignored", $name)),
                    }
                }
            };
        }
        num!("HFS_CONSOLE_SESSION_IDLE_MINUTES", env.session_idle, field::SESSION_IDLE);
        num!("HFS_CONSOLE_SESSION_ABSOLUTE_HOURS", env.session_absolute, field::SESSION_ABSOLUTE);
        num!("HFS_CONSOLE_PASSWORD_MIN_LENGTH", env.min_password_length, field::MIN_PASSWORD_LENGTH);
        num!("HFS_CONSOLE_LOCKOUT_FAILURES", env.lockout_failures, field::LOCKOUT_FAILURES);
        num!("HFS_CONSOLE_LOCKOUT_MINUTES", env.lockout_minutes, field::LOCKOUT_MINUTES);
        if let Some(v) = var("HFS_CONSOLE_BREAK_GLASS") {
            env.break_glass = Some(matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"));
            env.pinned.insert(field::BREAK_GLASS);
        }
        if let Some(v) = var("HFS_CONSOLE_ALLOWED_ORIGINS") {
            env.allowed_origins = Some(list(&v).iter().filter_map(|o| normalize_origin(o)).collect());
            env.pinned.insert(field::ALLOWED_ORIGINS);
        }
        env.allow_anonymous_admin = var("HFS_CONSOLE_ALLOW_ANONYMOUS_ADMIN")
            .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"));
        env.warnings = warnings;
        env
    }

    /// Whether `field` is pinned by the environment.
    pub fn is_pinned(&self, field: &str) -> bool {
        self.pinned.contains(field)
    }

    /// The effective settings: `stored` (or the defaults before the first
    /// save) with every pinned field taken from the environment.
    pub fn apply(&self, stored: Option<&ConsoleAuthSettings>) -> ConsoleAuthSettings {
        let mut s = stored.cloned().unwrap_or_default();
        if let Some(m) = self.mode {
            s.mode = m;
        }
        if let Some(r) = self.anonymous_role {
            s.anonymous_role = r;
        }
        let oidc_env = self.issuer.is_some()
            || self.client_id.is_some()
            || self.scopes.is_some()
            || self.role_claim.is_some()
            || self.admin_values.is_some()
            || self.editor_values.is_some()
            || self.viewer_values.is_some()
            || self.default_role.is_some();
        if oidc_env {
            let mut o = s.oidc.take().unwrap_or_else(|| OidcClientSettings {
                issuer: String::new(),
                client_id: String::new(),
                client_secret: None,
                scopes: "openid profile email".into(),
                authorization_endpoint: None,
                token_endpoint: None,
                end_session_endpoint: None,
                role_claim: "roles".into(),
                role_mapping: RoleMapping::default(),
                default_role: None,
            });
            if let Some(v) = &self.issuer {
                o.issuer = v.clone();
            }
            if let Some(v) = &self.client_id {
                o.client_id = v.clone();
            }
            if let Some(v) = &self.scopes {
                o.scopes = v.clone();
            }
            if let Some(v) = &self.role_claim {
                o.role_claim = v.clone();
            }
            if let Some(v) = &self.admin_values {
                o.role_mapping.admin = v.clone();
            }
            if let Some(v) = &self.editor_values {
                o.role_mapping.editor = v.clone();
            }
            if let Some(v) = &self.viewer_values {
                o.role_mapping.viewer = v.clone();
            }
            if let Some(v) = self.default_role {
                o.default_role = v;
            }
            if self.client_secret.is_some() {
                // The env secret wins; the sealed one is not used.
                o.client_secret = None;
            }
            s.oidc = Some(o);
        }
        if let Some(v) = self.session_idle {
            s.session_idle_minutes = v;
        }
        if let Some(v) = self.session_absolute {
            s.session_absolute_hours = v;
        }
        if let Some(v) = self.break_glass {
            s.break_glass = v;
        }
        if let Some(v) = self.min_password_length {
            s.local.min_password_length = v;
        }
        if let Some(v) = self.lockout_failures {
            s.local.lockout_failures = v;
        }
        if let Some(v) = self.lockout_minutes {
            s.local.lockout_minutes = v;
        }
        if let Some(v) = &self.allowed_origins {
            s.allowed_origins = v.clone();
        }
        s
    }

    /// Fields of `proposed` that differ from the effective value of a pinned
    /// field — a save must not try to change them.
    pub fn pinned_changes(&self, proposed: &ConsoleAuthSettings) -> Vec<&'static str> {
        let effective = self.apply(Some(proposed));
        let mut changed = Vec::new();
        let o = |s: &ConsoleAuthSettings| s.oidc.clone();
        let checks: [(&'static str, bool); 17] = [
            (field::MODE, proposed.mode != effective.mode),
            (field::ANONYMOUS_ROLE, proposed.anonymous_role != effective.anonymous_role),
            (
                field::OIDC_ISSUER,
                o(proposed).map(|x| x.issuer) != o(&effective).map(|x| x.issuer),
            ),
            (
                field::OIDC_CLIENT_ID,
                o(proposed).map(|x| x.client_id) != o(&effective).map(|x| x.client_id),
            ),
            (
                field::OIDC_SCOPES,
                o(proposed).map(|x| x.scopes) != o(&effective).map(|x| x.scopes),
            ),
            (
                field::OIDC_ROLE_CLAIM,
                o(proposed).map(|x| x.role_claim) != o(&effective).map(|x| x.role_claim),
            ),
            (
                field::OIDC_ADMIN_VALUES,
                o(proposed).map(|x| x.role_mapping.admin)
                    != o(&effective).map(|x| x.role_mapping.admin),
            ),
            (
                field::OIDC_EDITOR_VALUES,
                o(proposed).map(|x| x.role_mapping.editor)
                    != o(&effective).map(|x| x.role_mapping.editor),
            ),
            (
                field::OIDC_VIEWER_VALUES,
                o(proposed).map(|x| x.role_mapping.viewer)
                    != o(&effective).map(|x| x.role_mapping.viewer),
            ),
            (
                field::OIDC_DEFAULT_ROLE,
                o(proposed).map(|x| x.default_role) != o(&effective).map(|x| x.default_role),
            ),
            (field::SESSION_IDLE, proposed.session_idle_minutes != effective.session_idle_minutes),
            (
                field::SESSION_ABSOLUTE,
                proposed.session_absolute_hours != effective.session_absolute_hours,
            ),
            (field::BREAK_GLASS, proposed.break_glass != effective.break_glass),
            (
                field::MIN_PASSWORD_LENGTH,
                proposed.local.min_password_length != effective.local.min_password_length,
            ),
            (
                field::LOCKOUT_FAILURES,
                proposed.local.lockout_failures != effective.local.lockout_failures,
            ),
            (
                field::LOCKOUT_MINUTES,
                proposed.local.lockout_minutes != effective.local.lockout_minutes,
            ),
            (field::ALLOWED_ORIGINS, proposed.allowed_origins != effective.allowed_origins),
        ];
        for (f, differs) in checks {
            if differs && self.is_pinned(f) {
                changed.push(f);
            }
        }
        changed
    }
}

/// A refused setting: the field path and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsError {
    /// Field path.
    pub field: &'static str,
    /// Message.
    pub message: String,
}

/// Validates settings an admin wants to save (structure only; the mode
/// switch preconditions are checked by the runtime).
pub fn validate(s: &ConsoleAuthSettings, allow_anonymous_admin: bool) -> Vec<SettingsError> {
    let mut errors = Vec::new();
    let mut err = |field: &'static str, message: &str| {
        errors.push(SettingsError {
            field,
            message: message.to_string(),
        })
    };
    if s.anonymous_role == Some(ConsoleRole::Admin) && !allow_anonymous_admin {
        err(
            field::ANONYMOUS_ROLE,
            "anonymous visitors cannot be admins (HFS_CONSOLE_ALLOW_ANONYMOUS_ADMIN is for development only)",
        );
    }
    if s.mode == ConsoleAuthMode::Oidc {
        match &s.oidc {
            None => err(field::OIDC_ISSUER, "OIDC mode needs an issuer and a client id"),
            Some(o) => {
                if url::Url::parse(&o.issuer)
                    .ok()
                    .filter(|u| matches!(u.scheme(), "https" | "http"))
                    .is_none()
                {
                    err(field::OIDC_ISSUER, "the issuer must be an http(s) URL");
                }
                if o.client_id.trim().is_empty() {
                    err(field::OIDC_CLIENT_ID, "the client id is required");
                }
            }
        }
    }
    if let Some(o) = &s.oidc {
        if !o.scopes.split_whitespace().any(|x| x == "openid") {
            err(field::OIDC_SCOPES, "the scopes must include openid");
        }
        if o.role_claim.trim().is_empty() {
            err(field::OIDC_ROLE_CLAIM, "the role claim path is required");
        }
    }
    if !(5..=10_080).contains(&s.session_idle_minutes) {
        err(field::SESSION_IDLE, "between 5 minutes and 7 days");
    }
    if !(1..=720).contains(&s.session_absolute_hours) {
        err(field::SESSION_ABSOLUTE, "between 1 hour and 30 days");
    }
    if !(8..=128).contains(&s.local.min_password_length) {
        err(field::MIN_PASSWORD_LENGTH, "between 8 and 128");
    }
    if !(1..=100).contains(&s.local.lockout_failures) {
        err(field::LOCKOUT_FAILURES, "between 1 and 100");
    }
    if !(1..=1_440).contains(&s.local.lockout_minutes) {
        err(field::LOCKOUT_MINUTES, "between 1 minute and 24 hours");
    }
    if s.allowed_origins.iter().any(|o| normalize_origin(o).as_deref() != Some(o.as_str())) {
        err(field::ALLOWED_ORIGINS, "origins are scheme://host[:port]");
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> EnvBootstrap {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        EnvBootstrap::from_lookup(|k| map.get(k).cloned())
    }

    #[test]
    fn empty_env_pins_nothing_and_applies_defaults() {
        let e = env(&[]);
        assert!(e.pinned.is_empty());
        assert!(!e.explicit_mode);
        assert_eq!(e.apply(None), ConsoleAuthSettings::default());
    }

    #[test]
    fn env_pins_and_overrides_stored_values() {
        let e = env(&[
            ("HFS_CONSOLE_AUTH_MODE", "oidc"),
            ("HFS_CONSOLE_ANONYMOUS_ROLE", "none"),
            ("HFS_CONSOLE_OIDC_ISSUER", "https://idp.example/realms/r"),
            ("HFS_CONSOLE_OIDC_CLIENT_ID", "hfs-web"),
            ("HFS_CONSOLE_OIDC_CLIENT_SECRET", "s3cret"),
            ("HFS_CONSOLE_OIDC_ROLE_CLAIM", "realm_access.roles"),
            ("HFS_CONSOLE_OIDC_ADMIN_VALUES", "hfs-admin, ops"),
            ("HFS_CONSOLE_OIDC_DEFAULT_ROLE", "viewer"),
            ("HFS_CONSOLE_SESSION_IDLE_MINUTES", "30"),
            ("HFS_CONSOLE_BREAK_GLASS", "false"),
            ("HFS_CONSOLE_LOCKOUT_FAILURES", "nope"),
        ]);
        assert!(e.explicit_mode);
        assert!(e.is_pinned(field::MODE) && e.is_pinned(field::OIDC_CLIENT_SECRET));
        assert!(!e.is_pinned(field::LOCKOUT_FAILURES));
        assert_eq!(e.warnings.len(), 1);
        let stored = ConsoleAuthSettings {
            mode: ConsoleAuthMode::Local,
            session_idle_minutes: 999,
            ..Default::default()
        };
        let s = e.apply(Some(&stored));
        assert_eq!(s.mode, ConsoleAuthMode::Oidc);
        assert_eq!(s.anonymous_role, None);
        assert_eq!(s.session_idle_minutes, 30);
        assert!(!s.break_glass);
        let o = s.oidc.clone().unwrap();
        assert_eq!(o.client_id, "hfs-web");
        assert_eq!(o.role_mapping.admin, vec!["hfs-admin", "ops"]);
        assert_eq!(o.default_role, Some(ConsoleRole::Viewer));
        assert_eq!(e.client_secret.as_ref().unwrap().expose(), "s3cret");
        // Saving the effective value is fine; changing a pinned one is not.
        assert!(e.pinned_changes(&s).is_empty());
        let mut changed = s.clone();
        changed.mode = ConsoleAuthMode::None;
        changed.local.lockout_minutes = 3; // not pinned
        assert_eq!(e.pinned_changes(&changed), vec![field::MODE]);
        assert!(validate(&s, false).is_empty(), "{:?}", validate(&s, false));
    }

    #[test]
    fn legacy_ui_login_variables_bootstrap_oidc() {
        let e = env(&[
            ("HFS_UI_LOGIN_CLIENT_ID", "hfs-web"),
            ("HFS_AUTH_ISSUER", "https://kc/realms/hfs-test"),
            ("HFS_UI_LOGIN_SCOPES", "openid email"),
        ]);
        let o = e.apply(None).oidc.unwrap();
        assert_eq!(o.issuer, "https://kc/realms/hfs-test");
        assert_eq!(o.scopes, "openid email");
        // New variables win over legacy ones.
        let e = env(&[
            ("HFS_UI_LOGIN_CLIENT_ID", "legacy"),
            ("HFS_CONSOLE_OIDC_CLIENT_ID", "new"),
        ]);
        assert_eq!(e.apply(None).oidc.unwrap().client_id, "new");
    }

    #[test]
    fn validation_catches_bad_values() {
        let mut s = ConsoleAuthSettings {
            mode: ConsoleAuthMode::Oidc,
            anonymous_role: Some(ConsoleRole::Admin),
            session_idle_minutes: 1,
            allowed_origins: vec!["https://ok.example".into(), "not an origin".into()],
            ..Default::default()
        };
        s.local.min_password_length = 4;
        let fields: Vec<_> = validate(&s, false).into_iter().map(|e| e.field).collect();
        for f in [
            field::ANONYMOUS_ROLE,
            field::OIDC_ISSUER,
            field::SESSION_IDLE,
            field::MIN_PASSWORD_LENGTH,
            field::ALLOWED_ORIGINS,
        ] {
            assert!(fields.contains(&f), "{f} in {fields:?}");
        }
        s.anonymous_role = Some(ConsoleRole::Admin);
        assert!(
            !validate(&s, true)
                .iter()
                .any(|e| e.field == field::ANONYMOUS_ROLE)
        );
        s.oidc = Some(OidcClientSettings {
            issuer: "ftp://x".into(),
            client_id: " ".into(),
            client_secret: None,
            scopes: "profile".into(),
            authorization_endpoint: None,
            token_endpoint: None,
            end_session_endpoint: None,
            role_claim: "".into(),
            role_mapping: RoleMapping::default(),
            default_role: None,
        });
        let fields: Vec<_> = validate(&s, true).into_iter().map(|e| e.field).collect();
        for f in [
            field::OIDC_ISSUER,
            field::OIDC_CLIENT_ID,
            field::OIDC_SCOPES,
            field::OIDC_ROLE_CLAIM,
        ] {
            assert!(fields.contains(&f), "{f} in {fields:?}");
        }
    }
}
