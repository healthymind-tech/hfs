//! Security events of the authorization server (design §10.1). The audit
//! package (I8, `helios-idp-pages`) implements [`IdpEventSink`] with BALP
//! AuditEvents; [`TracingEventSink`] logs, [`NoopEventSink`] drops.
//! Events never carry token values, secrets or assertions — only
//! identifiers (client, subject, grant id, token `jti`).
//!
//! `ip` is the client address as seen by the server (after the trusted
//! proxy), when known.

use serde::Serialize;

/// One event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum IdpEvent {
    LoginSucceeded {
        realm: String,
        user_id: String,
        client_id: Option<String>,
        amr: Vec<String>,
        ip: Option<String>,
    },
    LoginFailed {
        realm: String,
        /// Internal only (never shown): e.g. "unknown user", "bad password", "locked".
        reason: String,
        username: Option<String>,
        client_id: Option<String>,
        ip: Option<String>,
    },
    Logout {
        realm: String,
        user_id: Option<String>,
        client_id: Option<String>,
        ip: Option<String>,
    },
    ConsentGranted {
        realm: String,
        user_id: String,
        client_id: String,
        endpoint: String,
        scopes: Vec<String>,
        /// Patient in context (BALP AuthZconsent patient entity).
        patient: Option<String>,
        /// Whether the consent was stored for later requests.
        remembered: bool,
        ip: Option<String>,
    },
    ConsentDenied {
        realm: String,
        user_id: String,
        client_id: String,
        endpoint: String,
        ip: Option<String>,
    },
    LockedOut {
        realm: String,
        username: Option<String>,
        until_unix: i64,
        ip: Option<String>,
    },
    ConsentWithdrawn {
        realm: String,
        user_id: String,
        client_id: String,
        endpoint: Option<String>,
        ip: Option<String>,
    },
    /// Tokens issued at the token endpoint (authorization_code,
    /// client_credentials).
    TokenIssued {
        realm: String,
        client_id: String,
        subject: String,
        scope: String,
        aud: Vec<String>,
        gid: String,
        jti: String,
        grant_type: String,
        /// Patient in the launch context, if any.
        patient: Option<String>,
        ip: Option<String>,
    },
    /// Tokens issued from a refresh token (rotation).
    TokenRefreshed {
        realm: String,
        client_id: String,
        subject: String,
        scope: String,
        gid: String,
        jti: String,
        ip: Option<String>,
    },
    TokenRevoked {
        realm: String,
        client_id: String,
        gid: String,
        ip: Option<String>,
    },
    /// A rotated refresh token came back: family and grant revoked.
    RefreshReuse {
        realm: String,
        client_id: String,
        family: String,
        ip: Option<String>,
    },
    /// A redeemed authorization code came back: its grant was revoked.
    CodeReplay {
        realm: String,
        client_id: String,
        ip: Option<String>,
    },
    ClientAuthFailed {
        realm: String,
        client_id: Option<String>,
        /// Internal only (the client sees a generic `invalid_client`).
        reason: String,
        ip: Option<String>,
    },
    KeyRotated {
        realm: String,
        kid: String,
        /// `created`, `promoted`, `retired`, `emergency`, `imported`.
        action: String,
    },
}

/// Receives events. Synchronous and fire-and-forget: implementations
/// buffer and return; they must never block a request.
pub trait IdpEventSink: Send + Sync + 'static {
    fn emit(&self, e: IdpEvent);
}

/// Drops every event.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopEventSink;

impl IdpEventSink for NoopEventSink {
    fn emit(&self, _e: IdpEvent) {}
}

/// Logs events (security alerts at `warn`).
#[derive(Debug, Default, Clone, Copy)]
pub struct TracingEventSink;

impl IdpEventSink for TracingEventSink {
    fn emit(&self, e: IdpEvent) {
        let json = serde_json::to_string(&e).unwrap_or_default();
        match e {
            IdpEvent::CodeReplay { .. }
            | IdpEvent::RefreshReuse { .. }
            | IdpEvent::ClientAuthFailed { .. }
            | IdpEvent::LoginFailed { .. }
            | IdpEvent::LockedOut { .. } => {
                tracing::warn!(target: "helios_idp::events", "{json}")
            }
            _ => tracing::info!(target: "helios_idp::events", "{json}"),
        }
    }
}

/// Collects events in memory (tests).
#[derive(Debug, Default)]
pub struct MemoryEventSink(pub std::sync::Mutex<Vec<IdpEvent>>);

impl IdpEventSink for MemoryEventSink {
    fn emit(&self, e: IdpEvent) {
        self.0.lock().expect("poisoned").push(e);
    }
}

impl MemoryEventSink {
    pub fn take(&self) -> Vec<IdpEvent> {
        std::mem::take(&mut *self.0.lock().expect("poisoned"))
    }
}
