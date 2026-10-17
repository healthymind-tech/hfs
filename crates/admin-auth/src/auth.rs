//! The credential check, the session table and the failed-sign-in limiter.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderValue, Method, header};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::client::{IpNet, client_ip};
use crate::settings::{AdminSettings, CookieSecure, Secret, normalize_origin, strip_default_port};

/// Failed attempts from one client address within [`FAILURE_WINDOW`] that
/// lock it out for [`LOCKOUT`].
pub const MAX_FAILURES: u32 = 5;
/// Window in which [`MAX_FAILURES`] failures trigger a lockout.
pub const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);
/// How long a client address is refused after too many failures — even with
/// the right password, so guessing gains nothing while locked.
pub const LOCKOUT: Duration = Duration::from_secs(15 * 60);
/// A session ends after this long even when in constant use (or after the
/// idle lifetime, when that is longer).
pub const ABSOLUTE_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

const MAX_SESSIONS: usize = 1024;
const MAX_TRACKED_CLIENTS: usize = 10_000;

/// Name and `Path` of the session cookie.
#[derive(Clone, Copy, Debug)]
pub struct CookieSpec {
    /// Cookie name, distinct per product (`hfs_admin`, `hts_admin`): both
    /// consoles are commonly served from one origin.
    pub name: &'static str,
    /// Cookie `Path`: the narrowest prefix covering every page and API the
    /// browser must present it to.
    pub path: &'static str,
}

/// How a request proved it is the administrator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Via {
    /// The session cookie of a browser sign-in.
    Session,
    /// `Authorization: Basic` or `Bearer` carrying the password.
    Credentials,
}

/// A successful [`AdminAuth::authorize`].
#[derive(Clone, Debug)]
pub struct Granted {
    /// The admin user name.
    pub user: String,
    /// Which credential was presented.
    pub via: Via,
}

/// Why [`AdminAuth::authorize`] refused a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Denial {
    /// No credential, or an expired / unknown session cookie.
    Unauthenticated,
    /// A wrong user name or password (counted towards the lockout).
    BadCredentials,
    /// Too many recent failures from this client.
    RateLimited(Duration),
    /// A cookie-authenticated write whose `Origin`/`Referer` is not this
    /// site (cross-site request forgery), or a write from a foreign origin.
    CrossOrigin,
    /// Administration is enabled but no password is configured.
    NotConfigured,
}

/// Result of a sign-in form submission.
#[derive(Debug)]
pub enum LoginOutcome {
    /// Signed in. Send this `Set-Cookie` with the redirect.
    Success {
        /// The session cookie.
        set_cookie: HeaderValue,
    },
    /// Wrong user name or password.
    Invalid,
    /// Locked out for this long.
    RateLimited(Duration),
    /// The form was not posted from this site.
    CrossOrigin,
    /// No password configured: sign-in is impossible.
    NotConfigured,
}

struct Session {
    user: String,
    created: Instant,
    last_seen: Instant,
}

struct Failures {
    count: u32,
    window_start: Instant,
    locked_until: Option<Instant>,
}

/// The configured administrator: one user name and password, a table of
/// browser sessions, and a failed-attempt limiter, all in memory (a restart
/// signs everyone out).
pub struct AdminAuth {
    prefix: &'static str,
    user: String,
    user_digest: [u8; 32],
    password: Secret,
    password_digest: [u8; 32],
    idle: Duration,
    absolute: Duration,
    trusted_proxies: Vec<IpNet>,
    allowed_origins: Vec<String>,
    cookie_secure: CookieSecure,
    cookie: CookieSpec,
    sessions: Mutex<HashMap<[u8; 32], Session>>,
    failures: Mutex<HashMap<IpAddr, Failures>>,
}

impl std::fmt::Debug for AdminAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminAuth")
            .field("prefix", &self.prefix)
            .field("user", &self.user)
            .field("cookie", &self.cookie)
            .finish_non_exhaustive()
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Constant-time equality of two digests.
fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

fn is_safe(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

impl AdminAuth {
    /// Builds the authenticator. `settings.password` must be set.
    pub(crate) fn new(
        prefix: &'static str,
        settings: AdminSettings,
        password: Secret,
        cookie: CookieSpec,
    ) -> Self {
        Self {
            prefix,
            user_digest: digest(settings.user.as_bytes()),
            user: settings.user,
            password_digest: digest(password.expose().as_bytes()),
            password,
            idle: settings.session_idle,
            absolute: ABSOLUTE_LIFETIME.max(settings.session_idle),
            trusted_proxies: settings.trusted_proxies,
            allowed_origins: settings.allowed_origins,
            cookie_secure: settings.cookie_secure,
            cookie,
            sessions: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
        }
    }

    /// The environment-variable prefix (`HFS`, `HTS`), for messages.
    pub fn prefix(&self) -> &'static str {
        self.prefix
    }

    /// The configured admin user name.
    pub fn user(&self) -> &str {
        &self.user
    }

    /// The session cookie's name and path.
    pub fn cookie(&self) -> CookieSpec {
        self.cookie
    }

    /// An `Authorization` value carrying the admin credentials, for a
    /// process's own loopback calls to its admin API (the HTS console). The
    /// value is marked sensitive so it is never printed by `http`'s `Debug`.
    pub fn service_authorization(&self) -> HeaderValue {
        let raw = format!("{}:{}", self.user, self.password.expose());
        let mut value = HeaderValue::from_str(&format!("Basic {}", STANDARD.encode(raw)))
            .unwrap_or_else(|_| HeaderValue::from_static(""));
        value.set_sensitive(true);
        value
    }

    fn verify(&self, user: &str, password: &str) -> bool {
        let user_ok = ct_eq(&digest(user.as_bytes()), &self.user_digest);
        let password_ok = ct_eq(&digest(password.as_bytes()), &self.password_digest);
        user_ok & password_ok
    }

    /// The client address the limiter keys on.
    pub fn client_ip(&self, headers: &HeaderMap, peer: Option<IpAddr>) -> IpAddr {
        client_ip(headers, peer, &self.trusted_proxies)
    }

    fn locked_for(&self, ip: IpAddr) -> Option<Duration> {
        let now = Instant::now();
        let table = self.failures.lock().unwrap_or_else(|p| p.into_inner());
        table
            .get(&ip)
            .and_then(|f| f.locked_until)
            .and_then(|until| until.checked_duration_since(now))
            .filter(|left| !left.is_zero())
    }

    fn record_failure(&self, ip: IpAddr, what: &'static str) {
        let now = Instant::now();
        let mut table = self.failures.lock().unwrap_or_else(|p| p.into_inner());
        if table.len() >= MAX_TRACKED_CLIENTS {
            table.retain(|_, f| {
                f.locked_until.is_some_and(|u| u > now)
                    || now.duration_since(f.window_start) < FAILURE_WINDOW
            });
            if table.len() >= MAX_TRACKED_CLIENTS {
                // Still full of live entries: forget the oldest window.
                if let Some(oldest) = table
                    .iter()
                    .min_by_key(|(_, f)| f.window_start)
                    .map(|(ip, _)| *ip)
                {
                    table.remove(&oldest);
                }
            }
        }
        let entry = table.entry(ip).or_insert(Failures {
            count: 0,
            window_start: now,
            locked_until: None,
        });
        if now.duration_since(entry.window_start) >= FAILURE_WINDOW {
            entry.count = 0;
            entry.window_start = now;
            entry.locked_until = None;
        }
        entry.count += 1;
        let locked = entry.count >= MAX_FAILURES;
        if locked {
            entry.locked_until = Some(now + LOCKOUT);
        }
        let count = entry.count;
        drop(table);
        // Never the attempted user name or password: only who and how often.
        tracing::warn!(
            client = %ip,
            failures = count,
            locked_out = locked,
            "{} admin {what} failed",
            self.prefix
        );
    }

    fn clear_failures(&self, ip: IpAddr) {
        self.failures
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&ip);
    }

    /// Checks a credential and charges a failure to the client. `None` when
    /// it is right; the denial otherwise.
    fn check_credentials(
        &self,
        ip: IpAddr,
        user: &str,
        password: &str,
        what: &'static str,
    ) -> Option<Denial> {
        if let Some(left) = self.locked_for(ip) {
            tracing::warn!(client = %ip, "{} admin {what} refused: client locked out", self.prefix);
            return Some(Denial::RateLimited(left));
        }
        if self.verify(user, password) {
            self.clear_failures(ip);
            None
        } else {
            self.record_failure(ip, what);
            match self.locked_for(ip) {
                Some(left) => Some(Denial::RateLimited(left)),
                None => Some(Denial::BadCredentials),
            }
        }
    }

    fn new_session(&self) -> String {
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let token = URL_SAFE_NO_PAD.encode(bytes);
        let now = Instant::now();
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        if sessions.len() >= MAX_SESSIONS {
            sessions.retain(|_, s| self.alive(s, now));
            if sessions.len() >= MAX_SESSIONS
                && let Some(oldest) = sessions
                    .iter()
                    .min_by_key(|(_, s)| s.last_seen)
                    .map(|(k, _)| *k)
            {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(
            digest(token.as_bytes()),
            Session {
                user: self.user.clone(),
                created: now,
                last_seen: now,
            },
        );
        token
    }

    fn alive(&self, session: &Session, now: Instant) -> bool {
        now.duration_since(session.last_seen) < self.idle
            && now.duration_since(session.created) < self.absolute
    }

    fn cookie_token(&self, headers: &HeaderMap) -> Option<String> {
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .find(|(name, _)| *name == self.cookie.name)
            .map(|(_, value)| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    /// The signed-in admin of a request's session cookie, refreshing its
    /// idle timer; `None` without a live session.
    pub fn session_user(&self, headers: &HeaderMap) -> Option<String> {
        let token = self.cookie_token(headers)?;
        let key = digest(token.as_bytes());
        let now = Instant::now();
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        let alive = sessions.get(&key).map(|s| self.alive(s, now))?;
        if !alive {
            sessions.remove(&key);
            return None;
        }
        let session = sessions.get_mut(&key)?;
        session.last_seen = now;
        Some(session.user.clone())
    }

    /// Whether a request's `Origin` (else `Referer`) is this site: the
    /// `Host` it was sent to, or one of the configured allowed origins (for a
    /// proxy that rewrites `Host`). A request carrying neither header fails.
    pub fn same_origin(&self, headers: &HeaderMap) -> bool {
        let Some(origin) = request_origin(headers) else {
            return false;
        };
        if self.allowed_origins.contains(&origin) {
            return true;
        }
        let (scheme, authority) = origin.split_once("://").unwrap_or(("", ""));
        headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(|v| strip_default_port(&v.trim().to_ascii_lowercase(), scheme))
            .is_some_and(|host| host == authority)
    }

    /// Decides whether a request to an admin endpoint may proceed.
    ///
    /// `Authorization: Basic` (user + password) or `Bearer <password>` is
    /// checked first (scripts); otherwise the session cookie (browsers), for
    /// which a state-changing method must also come from this site. A write
    /// whose `Origin` names another site is refused whatever credential it
    /// carries.
    pub fn authorize(
        &self,
        method: &Method,
        headers: &HeaderMap,
        peer: Option<IpAddr>,
    ) -> Result<Granted, Denial> {
        let unsafe_method = !is_safe(method);
        if unsafe_method && foreign_origin(headers) && !self.same_origin(headers) {
            return Err(Denial::CrossOrigin);
        }
        if let Some(value) = headers.get(header::AUTHORIZATION) {
            let ip = self.client_ip(headers, peer);
            let (user, password) = parse_authorization(value, &self.user);
            if let Some(denial) = self.check_credentials(ip, &user, &password, "API authentication")
            {
                return Err(denial);
            }
            return Ok(Granted {
                user: self.user.clone(),
                via: Via::Credentials,
            });
        }
        match self.session_user(headers) {
            Some(user) => {
                if unsafe_method && !self.same_origin(headers) {
                    tracing::warn!(
                        client = %self.client_ip(headers, peer),
                        "{} admin request refused: cookie-authenticated write without a same-site Origin/Referer",
                        self.prefix
                    );
                    return Err(Denial::CrossOrigin);
                }
                Ok(Granted {
                    user,
                    via: Via::Session,
                })
            }
            None => Err(Denial::Unauthenticated),
        }
    }

    /// Handles a sign-in form submission.
    pub fn login(
        &self,
        headers: &HeaderMap,
        peer: Option<IpAddr>,
        user: &str,
        password: &str,
    ) -> LoginOutcome {
        if !self.same_origin(headers) {
            tracing::warn!(
                client = %self.client_ip(headers, peer),
                "{} admin sign-in refused: not posted from this site",
                self.prefix
            );
            return LoginOutcome::CrossOrigin;
        }
        let ip = self.client_ip(headers, peer);
        match self.check_credentials(ip, user, password, "sign-in") {
            Some(Denial::RateLimited(left)) => LoginOutcome::RateLimited(left),
            Some(_) => LoginOutcome::Invalid,
            None => {
                let token = self.new_session();
                tracing::info!(client = %ip, user = %self.user, "{} admin signed in", self.prefix);
                let secure = match self.cookie_secure {
                    CookieSecure::Always => true,
                    CookieSecure::Never => false,
                    CookieSecure::Auto => request_is_https(headers),
                };
                LoginOutcome::Success {
                    set_cookie: self.cookie_header(&token, secure, None),
                }
            }
        }
    }

    /// Ends the request's session (if any) and returns the `Set-Cookie` that
    /// clears the cookie.
    pub fn logout(&self, headers: &HeaderMap) -> HeaderValue {
        if let Some(token) = self.cookie_token(headers) {
            let removed = self
                .sessions
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&digest(token.as_bytes()))
                .is_some();
            if removed {
                tracing::info!(user = %self.user, "{} admin signed out", self.prefix);
            }
        }
        let secure =
            !matches!(self.cookie_secure, CookieSecure::Never) && request_is_https(headers);
        self.cookie_header("", secure, Some(0))
    }

    fn cookie_header(&self, value: &str, secure: bool, max_age: Option<u64>) -> HeaderValue {
        let mut cookie = format!(
            "{}={}; Path={}; HttpOnly; SameSite=Strict",
            self.cookie.name, value, self.cookie.path
        );
        if secure {
            cookie.push_str("; Secure");
        }
        if let Some(age) = max_age {
            cookie.push_str(&format!("; Max-Age={age}"));
        }
        let mut header = HeaderValue::from_str(&cookie)
            .unwrap_or_else(|_| HeaderValue::from_static("invalid=; Max-Age=0"));
        header.set_sensitive(true);
        header
    }
}

/// `Origin`, else the origin of `Referer`, normalized.
fn request_origin(headers: &HeaderMap) -> Option<String> {
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        return normalize_origin(origin);
    }
    headers
        .get(header::REFERER)
        .and_then(|v| v.to_str().ok())
        .and_then(normalize_origin)
}

/// Whether the request names an origin at all (a browser). Scripts send
/// neither header, so this only ever refuses browsers.
fn foreign_origin(headers: &HeaderMap) -> bool {
    headers.contains_key(header::ORIGIN)
}

/// Whether the browser reached us over HTTPS, as far as the request shows.
fn request_is_https(headers: &HeaderMap) -> bool {
    let forwarded_https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("https"));
    forwarded_https || request_origin(headers).is_some_and(|o| o.starts_with("https://"))
}

/// `(user, password)` from `Basic` or `Bearer` (the bearer token is the
/// password of `default_user`). Anything unparsable yields empty strings,
/// which never verify.
fn parse_authorization(value: &HeaderValue, default_user: &str) -> (String, String) {
    let Ok(raw) = value.to_str() else {
        return (String::new(), String::new());
    };
    let raw = raw.trim();
    let (scheme, rest) = raw.split_once(' ').unwrap_or((raw, ""));
    let rest = rest.trim();
    if scheme.eq_ignore_ascii_case("basic") {
        let decoded = STANDARD
            .decode(rest)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .unwrap_or_default();
        match decoded.split_once(':') {
            Some((u, p)) => (u.to_string(), p.to_string()),
            None => (String::new(), String::new()),
        }
    } else if scheme.eq_ignore_ascii_case("bearer") {
        (default_user.to_string(), rest.to_string())
    } else {
        (String::new(), String::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: CookieSpec = CookieSpec {
        name: "hfs_admin",
        path: "/",
    };

    fn auth() -> AdminAuth {
        let settings = AdminSettings::default().with_password("correct horse");
        let pw = settings.password.clone().unwrap();
        AdminAuth::new("HFS", settings, pw, SPEC)
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn basic(user: &str, pw: &str) -> String {
        format!("Basic {}", STANDARD.encode(format!("{user}:{pw}")))
    }

    #[test]
    fn basic_and_bearer_credentials() {
        let a = auth();
        let ok = headers(&[("authorization", &basic("admin", "correct horse"))]);
        assert!(a.authorize(&Method::POST, &ok, None).is_ok());
        let bearer = headers(&[("authorization", "Bearer correct horse")]);
        assert_eq!(
            a.authorize(&Method::GET, &bearer, None).unwrap().via,
            Via::Credentials
        );
        let wrong_user = headers(&[("authorization", &basic("root", "correct horse"))]);
        assert_eq!(
            a.authorize(&Method::GET, &wrong_user, None).unwrap_err(),
            Denial::BadCredentials
        );
        assert_eq!(
            a.authorize(&Method::GET, &HeaderMap::new(), None)
                .unwrap_err(),
            Denial::Unauthenticated
        );
    }

    #[test]
    fn five_failures_lock_the_client_out_even_for_the_right_password() {
        let a = auth();
        let peer = Some("203.0.113.5".parse().unwrap());
        let wrong = headers(&[("authorization", &basic("admin", "nope"))]);
        for _ in 0..MAX_FAILURES - 1 {
            assert_eq!(
                a.authorize(&Method::GET, &wrong, peer).unwrap_err(),
                Denial::BadCredentials
            );
        }
        assert!(matches!(
            a.authorize(&Method::GET, &wrong, peer).unwrap_err(),
            Denial::RateLimited(_)
        ));
        let right = headers(&[("authorization", &basic("admin", "correct horse"))]);
        assert!(matches!(
            a.authorize(&Method::GET, &right, peer).unwrap_err(),
            Denial::RateLimited(_)
        ));
        // Another client is unaffected.
        let other = Some("203.0.113.6".parse().unwrap());
        assert!(a.authorize(&Method::GET, &right, other).is_ok());
    }

    #[test]
    fn login_sets_a_strict_httponly_cookie_and_the_session_works() {
        let a = auth();
        let form = headers(&[("host", "hfs.example"), ("origin", "https://hfs.example")]);
        let LoginOutcome::Success { set_cookie } = a.login(&form, None, "admin", "correct horse")
        else {
            panic!("login failed");
        };
        let cookie = set_cookie.to_str().unwrap().to_string();
        assert!(cookie.starts_with("hfs_admin="), "{cookie}");
        assert!(
            cookie.contains("; Path=/; HttpOnly; SameSite=Strict; Secure"),
            "{cookie}"
        );
        let token = cookie
            .trim_start_matches("hfs_admin=")
            .split(';')
            .next()
            .unwrap();
        assert!(token.len() >= 43, "256-bit token: {token}");

        let with_cookie = headers(&[
            ("cookie", &format!("hfs_lang=en; hfs_admin={token}")),
            ("host", "hfs.example"),
        ]);
        let granted = a.authorize(&Method::GET, &with_cookie, None).unwrap();
        assert_eq!(granted.via, Via::Session);

        // A write with the cookie needs a same-site Origin/Referer.
        assert_eq!(
            a.authorize(&Method::POST, &with_cookie, None).unwrap_err(),
            Denial::CrossOrigin
        );
        let mut same = with_cookie.clone();
        same.insert("origin", HeaderValue::from_static("https://hfs.example"));
        assert!(a.authorize(&Method::POST, &same, None).is_ok());
        let mut referer = with_cookie.clone();
        referer.insert(
            "referer",
            HeaderValue::from_static("https://hfs.example/ui/packages"),
        );
        assert!(a.authorize(&Method::DELETE, &referer, None).is_ok());
        let mut evil = with_cookie.clone();
        evil.insert("origin", HeaderValue::from_static("https://evil.example"));
        assert_eq!(
            a.authorize(&Method::POST, &evil, None).unwrap_err(),
            Denial::CrossOrigin
        );

        // Logout ends it.
        let cleared = a.logout(&with_cookie);
        assert!(cleared.to_str().unwrap().contains("Max-Age=0"));
        assert_eq!(
            a.authorize(&Method::GET, &with_cookie, None).unwrap_err(),
            Denial::Unauthenticated
        );
    }

    #[test]
    fn plain_http_login_omits_secure_and_cross_site_login_is_refused() {
        let a = auth();
        let form = headers(&[
            ("host", "localhost:8080"),
            ("origin", "http://localhost:8080"),
        ]);
        let LoginOutcome::Success { set_cookie } = a.login(&form, None, "admin", "correct horse")
        else {
            panic!("login failed");
        };
        assert!(!set_cookie.to_str().unwrap().contains("Secure"));
        let evil = headers(&[("host", "localhost:8080"), ("origin", "http://evil")]);
        assert!(matches!(
            a.login(&evil, None, "admin", "correct horse"),
            LoginOutcome::CrossOrigin
        ));
        let none = headers(&[("host", "localhost:8080")]);
        assert!(matches!(
            a.login(&none, None, "admin", "correct horse"),
            LoginOutcome::CrossOrigin
        ));
        assert!(matches!(
            a.login(&form, None, "admin", "wrong"),
            LoginOutcome::Invalid
        ));
    }

    #[test]
    fn idle_sessions_expire() {
        let mut settings = AdminSettings::default().with_password("pw-pw-pw-pw");
        settings.session_idle = Duration::from_millis(30);
        let pw = settings.password.clone().unwrap();
        let a = AdminAuth::new("HTS", settings, pw, SPEC);
        let token = a.new_session();
        let h = headers(&[("cookie", &format!("hfs_admin={token}"))]);
        assert!(a.session_user(&h).is_some());
        std::thread::sleep(Duration::from_millis(60));
        assert!(a.session_user(&h).is_none());
    }

    #[test]
    fn service_authorization_is_valid_and_sensitive() {
        let a = auth();
        let value = a.service_authorization();
        assert!(value.is_sensitive());
        let mut h = HeaderMap::new();
        h.insert("authorization", value);
        assert!(a.authorize(&Method::POST, &h, None).is_ok());
    }

    #[test]
    fn allowed_origin_covers_a_rewritten_host() {
        let settings = AdminSettings::default()
            .with_password("pw-pw-pw-pw")
            .allow_origin_of("https://hfs.example/fhir");
        let pw = settings.password.clone().unwrap();
        let a = AdminAuth::new("HFS", settings, pw, SPEC);
        let h = headers(&[("host", "10.0.0.3:8080"), ("origin", "https://hfs.example")]);
        assert!(a.same_origin(&h));
        // X-Forwarded-Host is not believed: only Host and the allow-list.
        let fwd = headers(&[
            ("host", "hfs:8080"),
            ("x-forwarded-host", "console.example"),
            ("origin", "https://console.example"),
        ]);
        assert!(!a.same_origin(&fwd));
    }
}
