//! Configuration: `<PREFIX>_ADMIN_*` environment variables.

use std::net::IpAddr;
use std::time::Duration;

use crate::client::IpNet;

/// Default admin user name (`<PREFIX>_ADMIN_USER`).
pub const DEFAULT_USER: &str = "admin";

/// Default idle lifetime of a browser session (`<PREFIX>_ADMIN_SESSION_IDLE_MINUTES`).
pub const DEFAULT_SESSION_IDLE: Duration = Duration::from_secs(8 * 60 * 60);

/// A secret string whose `Debug` never shows the value, so a config struct
/// that holds one can be logged or panicked over without leaking it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself. Callers must not log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

/// When the session cookie carries `Secure` (`<PREFIX>_ADMIN_COOKIE_SECURE`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CookieSecure {
    /// `Secure` when the sign-in request evidently came over HTTPS:
    /// `X-Forwarded-Proto: https`, or an `https://` `Origin`/`Referer` (what
    /// the browser itself reports, which survives a TLS-terminating proxy
    /// chain that rewrites `X-Forwarded-Proto` to `http`).
    #[default]
    Auto,
    /// Always `Secure` (`true`).
    Always,
    /// Never `Secure` (`false`) — plain-HTTP development only.
    Never,
}

/// Resolved admin sign-in configuration for one server.
#[derive(Clone, Debug)]
pub struct AdminSettings {
    /// `<PREFIX>_ADMIN_USER`, default [`DEFAULT_USER`].
    pub user: String,
    /// `<PREFIX>_ADMIN_PASSWORD`, or the contents of
    /// `<PREFIX>_ADMIN_PASSWORD_FILE` (which wins when both are set).
    pub password: Option<Secret>,
    /// Idle lifetime of a browser session.
    pub session_idle: Duration,
    /// Peers whose `X-Forwarded-For` / `X-Real-IP` are believed when working
    /// out the client address for the sign-in rate limit
    /// (`<PREFIX>_ADMIN_TRUSTED_PROXIES`, CIDR list; default loopback and
    /// private ranges; `none` trusts no proxy).
    pub trusted_proxies: Vec<IpNet>,
    /// Extra origins accepted on cookie-authenticated writes besides the
    /// request's own `Host` (`<PREFIX>_ADMIN_ALLOWED_ORIGINS`), normalized
    /// to `scheme://host[:port]`.
    pub allowed_origins: Vec<String>,
    /// `<PREFIX>_ADMIN_COOKIE_SECURE`: `auto` (default), `true`, `false`.
    pub cookie_secure: CookieSecure,
    /// A configuration error that must keep administration locked, such as
    /// an unreadable password file. Never contains the password.
    pub error: Option<String>,
}

impl Default for AdminSettings {
    fn default() -> Self {
        Self {
            user: DEFAULT_USER.to_string(),
            password: None,
            session_idle: DEFAULT_SESSION_IDLE,
            trusted_proxies: IpNet::default_trusted(),
            allowed_origins: Vec::new(),
            cookie_secure: CookieSecure::Auto,
            error: None,
        }
    }
}

impl AdminSettings {
    /// Reads `<prefix>_ADMIN_*` from the process environment.
    pub fn from_env(prefix: &str) -> Self {
        Self::from_lookup(prefix, |key| std::env::var(key).ok())
    }

    /// Reads `<prefix>_ADMIN_*` through `get` (the environment, or a map in
    /// tests).
    pub fn from_lookup(prefix: &str, get: impl Fn(&str) -> Option<String>) -> Self {
        let var = |name: &str| {
            get(&format!("{prefix}_ADMIN_{name}"))
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let mut settings = Self::default();
        if let Some(user) = var("USER") {
            settings.user = user;
        }

        let file = var("PASSWORD_FILE");
        // Read raw (untrimmed) so a password may contain leading/trailing
        // spaces; only an entirely blank value counts as unset.
        let inline = get(&format!("{prefix}_ADMIN_PASSWORD")).filter(|v| !v.trim().is_empty());
        match (file, inline) {
            (Some(path), inline) => {
                if inline.is_some() {
                    tracing::warn!(
                        "{prefix}_ADMIN_PASSWORD and {prefix}_ADMIN_PASSWORD_FILE are both set; \
                         using the file"
                    );
                }
                match std::fs::read_to_string(&path) {
                    Ok(contents) => {
                        // A secrets file normally ends with a newline the
                        // operator never meant as part of the password.
                        let value = contents.trim_end_matches(['\r', '\n']);
                        if value.trim().is_empty() {
                            settings.error =
                                Some(format!("{prefix}_ADMIN_PASSWORD_FILE ({path}) is empty"));
                        } else {
                            settings.password = Some(Secret::new(value));
                        }
                    }
                    Err(err) => {
                        settings.error = Some(format!(
                            "cannot read {prefix}_ADMIN_PASSWORD_FILE ({path}): {}",
                            err.kind()
                        ));
                    }
                }
            }
            (None, Some(inline)) => settings.password = Some(Secret::new(inline)),
            (None, None) => {}
        }

        if let Some(raw) = var("SESSION_IDLE_MINUTES") {
            match raw.parse::<u64>() {
                Ok(minutes) if minutes > 0 => {
                    settings.session_idle = Duration::from_secs(minutes.saturating_mul(60));
                }
                _ => tracing::warn!(
                    value = %raw,
                    "{prefix}_ADMIN_SESSION_IDLE_MINUTES is not a positive integer; using 480"
                ),
            }
        }

        if let Some(raw) = var("TRUSTED_PROXIES") {
            settings.trusted_proxies = if raw.eq_ignore_ascii_case("none") {
                Vec::new()
            } else {
                raw.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .filter_map(|entry| {
                        let parsed = IpNet::parse(entry);
                        if parsed.is_none() {
                            tracing::warn!(
                                entry = %entry,
                                "{prefix}_ADMIN_TRUSTED_PROXIES: not an IP address or CIDR; ignored"
                            );
                        }
                        parsed
                    })
                    .collect()
            };
        }

        if let Some(raw) = var("ALLOWED_ORIGINS") {
            for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                match normalize_origin(entry) {
                    Some(origin) => settings.allowed_origins.push(origin),
                    None => tracing::warn!(
                        entry = %entry,
                        "{prefix}_ADMIN_ALLOWED_ORIGINS: not an http(s) origin; ignored"
                    ),
                }
            }
        }

        if let Some(raw) = var("COOKIE_SECURE") {
            settings.cookie_secure = match raw.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "always" => CookieSecure::Always,
                "false" | "0" | "no" | "never" => CookieSecure::Never,
                "auto" => CookieSecure::Auto,
                _ => {
                    tracing::warn!(
                        value = %raw,
                        "{prefix}_ADMIN_COOKIE_SECURE must be auto, true or false; using auto"
                    );
                    CookieSecure::Auto
                }
            };
        }
        settings
    }

    /// The same settings with `password` set (tests, embedders).
    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(Secret::new(password));
        self.error = None;
        self
    }

    /// Also accept the origin of `url` (e.g. the server's public base URL)
    /// on cookie-authenticated writes. Ignored when `url` is not http(s).
    pub fn allow_origin_of(mut self, url: &str) -> Self {
        if let Some(origin) = normalize_origin(url)
            && !self.allowed_origins.contains(&origin)
        {
            self.allowed_origins.push(origin);
        }
        self
    }
}

/// `scheme://host[:port]` of an http(s) URL or origin, lower-cased, with the
/// scheme's default port dropped. `None` for anything else (including the
/// opaque `null` origin).
pub(crate) fn normalize_origin(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let (scheme, rest) = raw.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let authority = strip_default_port(&authority.to_ascii_lowercase(), &scheme);
    Some(format!("{scheme}://{authority}"))
}

/// `host:443` → `host` for https, `host:80` → `host` for http.
pub(crate) fn strip_default_port(authority: &str, scheme: &str) -> String {
    let default = if scheme == "https" { ":443" } else { ":80" };
    authority
        .strip_suffix(default)
        .unwrap_or(authority)
        .to_string()
}

/// Whether `ip` is one of `nets`.
pub(crate) fn trusted(nets: &[IpNet], ip: &IpAddr) -> bool {
    nets.iter().any(|net| net.contains(ip))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn defaults_without_any_variable() {
        let s = AdminSettings::from_lookup("HFS", lookup(&[]));
        assert_eq!(s.user, "admin");
        assert!(s.password.is_none());
        assert!(s.error.is_none());
        assert_eq!(s.session_idle, DEFAULT_SESSION_IDLE);
        assert_eq!(s.cookie_secure, CookieSecure::Auto);
        assert!(!s.trusted_proxies.is_empty());
    }

    #[test]
    fn reads_inline_password_user_and_idle() {
        let s = AdminSettings::from_lookup(
            "HTS",
            lookup(&[
                ("HTS_ADMIN_PASSWORD", "s3cret pass"),
                ("HTS_ADMIN_USER", "ops"),
                ("HTS_ADMIN_SESSION_IDLE_MINUTES", "30"),
                ("HTS_ADMIN_COOKIE_SECURE", "true"),
                ("HTS_ADMIN_TRUSTED_PROXIES", "none"),
                (
                    "HTS_ADMIN_ALLOWED_ORIGINS",
                    "https://Example.org:443/x, ftp://bad",
                ),
            ]),
        );
        assert_eq!(s.password.as_ref().map(Secret::expose), Some("s3cret pass"));
        assert_eq!(s.user, "ops");
        assert_eq!(s.session_idle, Duration::from_secs(1800));
        assert_eq!(s.cookie_secure, CookieSecure::Always);
        assert!(s.trusted_proxies.is_empty());
        assert_eq!(s.allowed_origins, vec!["https://example.org".to_string()]);
    }

    #[test]
    fn password_file_wins_and_loses_its_trailing_newline() {
        let dir = std::env::temp_dir().join(format!("admin-auth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pw");
        std::fs::write(&path, "from-file\n").unwrap();
        let s = AdminSettings::from_lookup(
            "HFS",
            lookup(&[
                ("HFS_ADMIN_PASSWORD", "inline"),
                ("HFS_ADMIN_PASSWORD_FILE", path.to_str().unwrap()),
            ]),
        );
        assert_eq!(s.password.as_ref().map(Secret::expose), Some("from-file"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unreadable_password_file_is_an_error_not_an_open_door() {
        let s = AdminSettings::from_lookup(
            "HFS",
            lookup(&[
                ("HFS_ADMIN_PASSWORD", "inline"),
                ("HFS_ADMIN_PASSWORD_FILE", "/nonexistent/admin-password"),
            ]),
        );
        assert!(s.password.is_none());
        let err = s.error.expect("error recorded");
        assert!(err.contains("HFS_ADMIN_PASSWORD_FILE"), "{err}");
        assert!(!err.contains("inline"), "{err}");
    }

    #[test]
    fn secret_debug_is_redacted() {
        let s = AdminSettings::default().with_password("hunter2-hunter2");
        let debug = format!("{s:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("redacted"), "{debug}");
    }

    #[test]
    fn origins_normalize() {
        assert_eq!(
            normalize_origin("https://HFS.example:443"),
            Some("https://hfs.example".into())
        );
        assert_eq!(
            normalize_origin("http://localhost:8080/ui/x?y"),
            Some("http://localhost:8080".into())
        );
        assert_eq!(normalize_origin("null"), None);
        assert_eq!(normalize_origin("https://user@evil"), None);
    }
}
