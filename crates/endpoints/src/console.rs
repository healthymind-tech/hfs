//! Web console authentication contracts (package P3 implements them).
//!
//! The console (`/ui`, and through its session cookie the browser's own
//! calls to the FHIR and admin APIs) has three modes ([`ConsoleAuthMode`]):
//!
//! * `none` — no sign-in; visitors get `anonymous_role` (default viewer).
//!   Administrative pages still need the break-glass administrator
//!   (`HFS_ADMIN_PASSWORD`) — fail closed as today.
//! * `oidc` — Authorization Code + PKCE against any OpenID Connect IdP
//!   (upstream #1449 flow), roles from a token claim ([`RoleMapping`]).
//! * `local` — built-in accounts ([`ConsoleUser`], argon2id hashes) with a
//!   sign-in form; **every** console page requires a session.
//!
//! In `oidc` and `local` the whole console requires sign-in on entry. The
//! break-glass administrator (`HFS_ADMIN_PASSWORD`) can always sign in on
//! `/ui/login?local=1` unless `break_glass` is turned off, so a broken IdP
//! configuration never locks the operator out.
//!
//! Settings live in `hfs_console_settings` (fork migration
//! `hm-0009-console-auth`), editable by admins in the console; environment
//! variables `HFS_CONSOLE_AUTH_*` bootstrap them and, when set, **pin** the
//! corresponding fields (shown read-only, so a recovery is always possible
//! from the environment).

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::ConsoleAccess;
use crate::secret::SealedSecret;

/// Console sign-in mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ConsoleAuthMode {
    /// No sign-in (today's behaviour).
    #[default]
    None,
    /// OpenID Connect.
    Oidc,
    /// Built-in accounts.
    Local,
}

/// Console role, ordered by power (`Viewer < Editor < Admin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConsoleRole {
    /// Browse endpoints, data, config (secrets never shown), jobs, audit.
    Viewer,
    /// Viewer + write FHIR data through the console (editor, batch, imports).
    Editor,
    /// Everything: endpoints, auth, users, packages, sync, purge.
    Admin,
}

impl ConsoleRole {
    /// Column / claim value.
    pub fn as_str(self) -> &'static str {
        match self {
            ConsoleRole::Viewer => "viewer",
            ConsoleRole::Editor => "editor",
            ConsoleRole::Admin => "admin",
        }
    }

    /// Parses a column value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "viewer" => Some(ConsoleRole::Viewer),
            "editor" => Some(ConsoleRole::Editor),
            "admin" => Some(ConsoleRole::Admin),
            _ => None,
        }
    }

    /// The SMART scopes the FHIR auth layer grants a console session of this
    /// role on an endpoint with the given [`ConsoleAccess`] (`None` = the
    /// session is no credential there). The console principal's issuer is
    /// [`CONSOLE_ISSUER`].
    pub fn data_scopes(self, access: ConsoleAccess) -> Option<&'static str> {
        match (access, self) {
            (ConsoleAccess::Off, _) => None,
            (ConsoleAccess::ReadOnly, _) | (ConsoleAccess::RoleBased, ConsoleRole::Viewer) => {
                Some("system/*.rs")
            }
            (ConsoleAccess::RoleBased, ConsoleRole::Editor) => Some("system/*.cruds"),
            (ConsoleAccess::RoleBased, ConsoleRole::Admin) => {
                Some("system/*.cruds system/purge system/reindex system/bulk-submit")
            }
        }
    }
}

/// Issuer stamped on principals derived from a console session.
pub const CONSOLE_ISSUER: &str = "urn:helios:console";

/// Maps IdP claim values to roles; the highest matching role wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct RoleMapping {
    /// Claim values granting admin (e.g. `hfs-admin`).
    pub admin: Vec<String>,
    /// Claim values granting editor.
    pub editor: Vec<String>,
    /// Claim values granting viewer.
    pub viewer: Vec<String>,
}

impl RoleMapping {
    /// The highest role any of `values` maps to, else `default`.
    pub fn resolve(&self, values: &[String], default: Option<ConsoleRole>) -> Option<ConsoleRole> {
        let has = |list: &Vec<String>| values.iter().any(|v| list.contains(v));
        if has(&self.admin) {
            Some(ConsoleRole::Admin)
        } else if has(&self.editor) {
            Some(ConsoleRole::Editor)
        } else if has(&self.viewer) {
            Some(ConsoleRole::Viewer)
        } else {
            default
        }
    }
}

/// Extracts the string values at a dotted claim path (`roles`,
/// `realm_access.roles`, `resource_access.hfs-web.roles`, `groups`) from a
/// decoded ID/access token. A string becomes one value, an array of strings
/// its elements; anything else none.
pub fn claim_values(claims: &serde_json::Value, path: &str) -> Vec<String> {
    let mut cur = claims;
    for part in path.split('.').filter(|p| !p.is_empty()) {
        match cur.get(part) {
            Some(v) => cur = v,
            None => return Vec::new(),
        }
    }
    match cur {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// The console's OIDC client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OidcClientSettings {
    /// Issuer (discovery at `{issuer}/.well-known/openid-configuration`).
    pub issuer: String,
    /// Client id registered for the console (e.g. `hfs-web`).
    pub client_id: String,
    /// Confidential client secret (sealed, AAD `console:oidc.clientSecret`);
    /// `None` = public client with PKCE only.
    #[serde(default)]
    pub client_secret: Option<SealedSecret>,
    /// Requested scopes.
    #[serde(default = "default_oidc_scopes")]
    pub scopes: String,
    /// Overrides of discovered endpoints.
    #[serde(default)]
    pub authorization_endpoint: Option<String>,
    /// Token endpoint override.
    #[serde(default)]
    pub token_endpoint: Option<String>,
    /// End-session endpoint override.
    #[serde(default)]
    pub end_session_endpoint: Option<String>,
    /// Dotted claim path carrying roles/groups (ID token, then userinfo).
    #[serde(default = "default_role_claim")]
    pub role_claim: String,
    /// Claim value → role.
    #[serde(default)]
    pub role_mapping: RoleMapping,
    /// Role of a signed-in user no mapping matches; `None` = refused
    /// ("you have no access to this console").
    #[serde(default)]
    pub default_role: Option<ConsoleRole>,
}

fn default_oidc_scopes() -> String {
    "openid profile email".into()
}

fn default_role_claim() -> String {
    "roles".into()
}

/// Built-in account policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LocalAuthSettings {
    /// Minimum password length.
    pub min_password_length: u8,
    /// Failed sign-ins per client address before lockout.
    pub lockout_failures: u8,
    /// Lockout and failure window, minutes.
    pub lockout_minutes: u16,
}

impl Default for LocalAuthSettings {
    fn default() -> Self {
        Self {
            min_password_length: 12,
            lockout_failures: 5,
            lockout_minutes: 15,
        }
    }
}

/// The console authentication configuration (`hfs_console_settings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConsoleAuthSettings {
    /// Mode.
    pub mode: ConsoleAuthMode,
    /// Role of visitors in `none` mode (`None` = no access without the
    /// break-glass sign-in). Never `admin` unless
    /// `HFS_CONSOLE_ALLOW_ANONYMOUS_ADMIN=true` (development only).
    pub anonymous_role: Option<ConsoleRole>,
    /// OIDC client (`oidc` mode).
    pub oidc: Option<OidcClientSettings>,
    /// Local account policy (`local` mode).
    pub local: LocalAuthSettings,
    /// Idle session lifetime, minutes.
    pub session_idle_minutes: u32,
    /// Absolute session lifetime, hours.
    pub session_absolute_hours: u32,
    /// Allow the `HFS_ADMIN_PASSWORD` administrator to sign in in every mode.
    pub break_glass: bool,
    /// Extra origins accepted on cookie-authenticated writes (CSRF), on top
    /// of the request host and `HFS_BASE_URL`'s origin.
    pub allowed_origins: Vec<String>,
}

impl Default for ConsoleAuthSettings {
    fn default() -> Self {
        Self {
            mode: ConsoleAuthMode::None,
            anonymous_role: Some(ConsoleRole::Viewer),
            oidc: None,
            local: LocalAuthSettings::default(),
            session_idle_minutes: 480,
            session_absolute_hours: 24,
            break_glass: true,
            allowed_origins: Vec::new(),
        }
    }
}

/// A built-in account (`hfs_console_users`). `password_hash` is a PHC string
/// (`$argon2id$v=19$m=19456,t=2,p=1$…`) and is never serialized to API
/// responses (`skip_serializing`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleUser {
    /// UUID.
    pub id: String,
    /// Sign-in name (unique, case-insensitive).
    pub username: String,
    /// Display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// E-mail.
    #[serde(default)]
    pub email: Option<String>,
    /// Global role.
    pub role: ConsoleRole,
    /// Disabled accounts cannot sign in; their sessions end within 10 s.
    #[serde(default)]
    pub disabled: bool,
    /// Forced password change at next sign-in.
    #[serde(default)]
    pub must_change_password: bool,
    /// PHC hash; never leaves the server.
    #[serde(skip_serializing, default)]
    pub password_hash: Option<String>,
    /// Optimistic-concurrency revision.
    pub revision: u64,
    /// Created.
    pub created_at: DateTime<Utc>,
    /// Updated.
    pub updated_at: DateTime<Utc>,
    /// Last successful sign-in.
    #[serde(default)]
    pub last_login_at: Option<DateTime<Utc>>,
}

/// How a console principal authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConsoleVia {
    /// `none` mode visitor.
    Anonymous,
    /// Built-in account.
    Local,
    /// OIDC.
    Oidc,
    /// `HFS_ADMIN_PASSWORD`.
    BreakGlass,
}

/// The signed-in console user as every layer sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsolePrincipal {
    /// `local:{username}`, `oidc:{iss}|{sub}`, `admin`, `anonymous`.
    pub subject: String,
    /// For the account menu.
    pub display_name: String,
    /// Global role.
    pub role: ConsoleRole,
    /// Per-endpoint role bindings (endpoint name → role). A binding
    /// replaces the global role on that endpoint (it may raise or lower it);
    /// server-wide administration (endpoints, users, console settings)
    /// always needs the global admin role.
    #[serde(default)]
    pub endpoint_roles: BTreeMap<String, ConsoleRole>,
    /// How.
    pub via: ConsoleVia,
}

impl ConsolePrincipal {
    /// The role on `endpoint`.
    pub fn role_for(&self, endpoint: &str) -> ConsoleRole {
        self.endpoint_roles
            .get(endpoint)
            .copied()
            .unwrap_or(self.role)
    }

    /// Whether this principal may change server configuration.
    pub fn is_admin(&self) -> bool {
        self.role == ConsoleRole::Admin
    }
}

/// Resolves the console session of a request. Implemented by P3; consumed
/// by the UI guard, the admin API guard and the FHIR auth layer (P2), which
/// turns a principal into an `helios_auth::Principal` with
/// [`ConsoleRole::data_scopes`]. Takes the raw `Cookie` header so this crate
/// needs no HTTP types.
#[async_trait]
pub trait ConsoleSessionResolver: Send + Sync {
    /// The principal of a valid session, else `None` (also for anonymous
    /// `none`-mode visitors: callers apply `anonymous_role` themselves).
    async fn resolve(&self, cookie_header: &str) -> Option<ConsolePrincipal>;
}

/// Store failures for console settings and users.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ConsoleStoreError {
    /// `404`.
    #[error("not found")]
    NotFound,
    /// `409` with the current revision.
    #[error("revision conflict: current revision is {current}")]
    Conflict {
        /// Stored revision.
        current: u64,
    },
    /// `409`.
    #[error("username {0:?} is taken")]
    DuplicateUsername(String),
    /// `409`: e.g. demoting or disabling the last enabled admin.
    #[error("{0}")]
    Forbidden(String),
    /// `503`.
    #[error("console store unavailable: {0}")]
    Unavailable(String),
}

/// Persistence of console settings, users and per-endpoint role bindings
/// (`hfs_console_settings`, `hfs_console_users`, `hfs_console_role_bindings`).
#[async_trait]
pub trait ConsoleStore: Send + Sync {
    /// Settings and their revision (`None` before first save → env defaults).
    async fn settings(&self) -> Result<Option<(ConsoleAuthSettings, u64)>, ConsoleStoreError>;
    /// CAS save (`expected_revision` 0 = first save).
    async fn save_settings(
        &self,
        settings: &ConsoleAuthSettings,
        expected_revision: u64,
        actor: &str,
    ) -> Result<u64, ConsoleStoreError>;
    /// All users, by username.
    async fn list_users(&self) -> Result<Vec<ConsoleUser>, ConsoleStoreError>;
    /// One user by username (case-insensitive).
    async fn user_by_name(&self, username: &str) -> Result<Option<ConsoleUser>, ConsoleStoreError>;
    /// Creates a user (id, revision, timestamps assigned by the store).
    async fn create_user(&self, user: ConsoleUser) -> Result<ConsoleUser, ConsoleStoreError>;
    /// CAS update of profile, role, flags and (when `Some`) the hash.
    /// Refuses to leave no enabled admin.
    async fn update_user(
        &self,
        user: ConsoleUser,
        expected_revision: u64,
    ) -> Result<ConsoleUser, ConsoleStoreError>;
    /// Deletes a user (same last-admin rule) and ends their sessions.
    async fn delete_user(&self, id: &str) -> Result<(), ConsoleStoreError>;
    /// Stamps `last_login_at`.
    async fn record_login(&self, id: &str, at: DateTime<Utc>) -> Result<(), ConsoleStoreError>;
    /// Per-endpoint role bindings of a subject (endpoint name → role).
    async fn endpoint_roles(
        &self,
        subject: &str,
    ) -> Result<BTreeMap<String, ConsoleRole>, ConsoleStoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_order_and_scopes() {
        assert!(
            ConsoleRole::Viewer < ConsoleRole::Editor && ConsoleRole::Editor < ConsoleRole::Admin
        );
        assert_eq!(ConsoleRole::Admin.data_scopes(ConsoleAccess::Off), None);
        assert_eq!(
            ConsoleRole::Admin.data_scopes(ConsoleAccess::ReadOnly),
            Some("system/*.rs")
        );
        assert_eq!(
            ConsoleRole::Viewer.data_scopes(ConsoleAccess::RoleBased),
            Some("system/*.rs")
        );
        assert_eq!(
            ConsoleRole::Editor.data_scopes(ConsoleAccess::RoleBased),
            Some("system/*.cruds")
        );
        for r in [ConsoleRole::Viewer, ConsoleRole::Editor, ConsoleRole::Admin] {
            assert_eq!(ConsoleRole::parse(r.as_str()), Some(r));
        }
    }

    #[test]
    fn role_mapping_highest_wins() {
        let m = RoleMapping {
            admin: vec!["hfs-admin".into()],
            editor: vec!["hfs-editor".into()],
            viewer: vec!["staff".into()],
        };
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            m.resolve(&v(&["staff", "hfs-admin"]), None),
            Some(ConsoleRole::Admin)
        );
        assert_eq!(m.resolve(&v(&["staff"]), None), Some(ConsoleRole::Viewer));
        assert_eq!(m.resolve(&v(&["other"]), None), None);
        assert_eq!(
            m.resolve(&v(&["other"]), Some(ConsoleRole::Viewer)),
            Some(ConsoleRole::Viewer)
        );
    }

    #[test]
    fn claim_paths() {
        let c = serde_json::json!({
            "realm_access": {"roles": ["hfs-admin", "offline_access"]},
            "groups": "lab",
            "n": 1
        });
        assert_eq!(
            claim_values(&c, "realm_access.roles"),
            vec!["hfs-admin", "offline_access"]
        );
        assert_eq!(claim_values(&c, "groups"), vec!["lab"]);
        assert!(claim_values(&c, "n").is_empty());
        assert!(claim_values(&c, "missing.path").is_empty());
    }

    #[test]
    fn principal_endpoint_roles() {
        let mut p = ConsolePrincipal {
            subject: "local:bob".into(),
            display_name: "Bob".into(),
            role: ConsoleRole::Viewer,
            endpoint_roles: BTreeMap::new(),
            via: ConsoleVia::Local,
        };
        p.endpoint_roles.insert("lab".into(), ConsoleRole::Editor);
        assert_eq!(p.role_for("lab"), ConsoleRole::Editor);
        assert_eq!(p.role_for("twcore"), ConsoleRole::Viewer);
        assert!(!p.is_admin());
    }

    #[test]
    fn user_hash_never_serialized() {
        let u = ConsoleUser {
            id: "1".into(),
            username: "alice".into(),
            display_name: None,
            email: None,
            role: ConsoleRole::Admin,
            disabled: false,
            must_change_password: false,
            password_hash: Some("$argon2id$v=19$secret".into()),
            revision: 1,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_login_at: None,
        };
        let s = serde_json::to_string(&u).unwrap();
        assert!(!s.contains("argon2id"), "{s}");
    }

    #[test]
    fn settings_defaults_are_todays_behaviour() {
        let s: ConsoleAuthSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.mode, ConsoleAuthMode::None);
        assert_eq!(s.anonymous_role, Some(ConsoleRole::Viewer));
        assert!(s.break_glass);
    }
}
