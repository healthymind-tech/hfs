//! Audit hook for console actions (design §7, §5.2).
//!
//! The console runtime reports every security-relevant action — sign-in
//! (success and failure), sign-out, account and role-binding changes,
//! password resets, session revocation, console settings — as a
//! [`ConsoleAuditEvent`]. The server installs a [`ConsoleAuditHook`] that
//! turns them into FHIR `AuditEvent`s in its audit sink (HFS:
//! `crates/hfs/src/manager/console_auth.rs`); without a hook the events are
//! only logged.

use std::sync::Arc;

/// What happened, in FHIR `AuditEvent.action` terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleAuditAction {
    /// `C`.
    Create,
    /// `U`.
    Update,
    /// `D`.
    Delete,
    /// `E` (sign-in, sign-out, test sign-in, revocation).
    Execute,
}

/// One console action.
#[derive(Debug, Clone)]
pub struct ConsoleAuditEvent {
    /// Who: a console subject (`local:alice`, `oidc:…`, `admin`) or, for a
    /// failed sign-in, the user name that was tried.
    pub actor: String,
    /// Display name of the actor, when known.
    pub actor_display: Option<String>,
    /// How the actor authenticated (`local`, `oidc`, `break-glass`,
    /// `console`, `anonymous`).
    pub via: String,
    /// The action.
    pub action: ConsoleAuditAction,
    /// `true` for success (`outcome` 0), `false` for a refusal (4).
    pub success: bool,
    /// Human-readable description (`outcomeDesc`).
    pub description: String,
    /// The object, as `(type, id)` (e.g. `("ConsoleUser", "alice")`).
    pub entity: Option<(String, String)>,
    /// Extra `entity.detail` pairs.
    pub details: Vec<(&'static str, String)>,
}

impl ConsoleAuditEvent {
    /// A successful action of `actor`.
    pub fn new(
        actor: impl Into<String>,
        action: ConsoleAuditAction,
        description: impl Into<String>,
    ) -> Self {
        Self {
            actor: actor.into(),
            actor_display: None,
            via: "console".into(),
            action,
            success: true,
            description: description.into(),
            entity: None,
            details: Vec::new(),
        }
    }

    /// Sets how the actor authenticated.
    pub fn via(mut self, via: impl Into<String>) -> Self {
        self.via = via.into();
        self
    }

    /// Sets the display name.
    pub fn display(mut self, display: impl Into<String>) -> Self {
        self.actor_display = Some(display.into());
        self
    }

    /// Marks it a refusal.
    pub fn failed(mut self) -> Self {
        self.success = false;
        self
    }

    /// Sets the object.
    pub fn entity(mut self, kind: impl Into<String>, id: impl Into<String>) -> Self {
        self.entity = Some((kind.into(), id.into()));
        self
    }

    /// Adds a detail.
    pub fn detail(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.details.push((name, value.into()));
        self
    }
}

/// Receives console actions (installed with
/// [`ConsoleAuth::set_audit_hook`](super::ConsoleAuth::set_audit_hook)).
#[async_trait::async_trait]
pub trait ConsoleAuditHook: Send + Sync {
    /// Records one event. Must not fail the action; errors are the hook's.
    async fn record(&self, event: ConsoleAuditEvent);
}

/// A hook that keeps the events in memory (tests).
#[derive(Default)]
pub struct MemoryAuditHook(pub std::sync::Mutex<Vec<ConsoleAuditEvent>>);

#[async_trait::async_trait]
impl ConsoleAuditHook for MemoryAuditHook {
    async fn record(&self, event: ConsoleAuditEvent) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).push(event);
    }
}

impl MemoryAuditHook {
    /// The recorded descriptions, in order.
    pub fn descriptions(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|e| e.description.clone())
            .collect()
    }
}

/// Shared handle type.
pub type SharedAuditHook = Arc<dyn ConsoleAuditHook>;
