//! Audit trail for administrative actions.
//!
//! Every action is written as one FHIR R4 `AuditEvent` JSON object per line to
//! `<HTS_ADMIN_DATA_DIR>/audit.ndjson`, and emitted as a `tracing` event on
//! the `hts::audit` target so it reaches the normal log pipeline too. The file
//! is append-only; [`AuditLog::recent`] reads its tail for the console.
//!
//! "Who" is best effort until SMART/OIDC lands (spec: Phase 3): a reverse
//! proxy's `X-Forwarded-User` / `X-Remote-User`, else the client address.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use axum::http::HeaderMap;
use serde_json::{Value, json};

/// The actor recorded on an audit event.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Actor {
    pub user: Option<String>,
    pub address: Option<String>,
    pub user_agent: Option<String>,
}

impl Actor {
    /// Derive the actor from request headers.
    pub fn from_headers(h: &HeaderMap) -> Self {
        let get = |n: &str| {
            h.get(n)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        Actor {
            user: get("x-forwarded-user").or_else(|| get("x-remote-user")),
            address: get("x-forwarded-for")
                .map(|v| v.split(',').next().unwrap_or("").trim().to_string())
                .or_else(|| get("x-real-ip")),
            user_agent: get("user-agent"),
        }
    }

    /// Short label for job listings.
    pub fn label(&self) -> String {
        self.user
            .clone()
            .or_else(|| self.address.clone())
            .unwrap_or_else(|| "anonymous".into())
    }

    /// The internal actor for system-initiated events (startup recovery).
    pub fn system() -> Self {
        Actor {
            user: Some("system".into()),
            ..Default::default()
        }
    }
}

/// Outcome codes per FHIR `AuditEvent.outcome`.
#[derive(Debug, Clone, Copy)]
pub enum Outcome {
    Success,
    MinorFailure,
    SeriousFailure,
}

impl Outcome {
    fn code(self) -> &'static str {
        match self {
            Outcome::Success => "0",
            Outcome::MinorFailure => "4",
            Outcome::SeriousFailure => "8",
        }
    }
}

/// Append-only audit sink.
pub struct AuditLog {
    path: PathBuf,
    lock: Mutex<()>,
}

impl AuditLog {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    /// Record `action` (`import`, `delete-code-system`, `set-current`, …) on
    /// `entity` (`http://snomed.info/sct|20250601`, an upload id …).
    pub fn record(
        &self,
        actor: &Actor,
        action: &str,
        entity: &str,
        outcome: Outcome,
        detail: Option<&str>,
    ) {
        let now = chrono::Utc::now().to_rfc3339();
        // DICOM/FHIR action codes: C create, R read, U update, D delete, E execute.
        let fhir_action = match action {
            a if a.starts_with("delete") => "D",
            a if a.starts_with("set-") => "U",
            "upload" => "C",
            _ => "E",
        };
        let event = json!({
            "resourceType": "AuditEvent",
            "type": {
                "system": "http://dicom.nema.org/resources/ontology/DCM",
                "code": "110100",
                "display": "Application Activity"
            },
            "subtype": [{
                "system": "urn:helios:hts:admin-action",
                "code": action
            }],
            "action": fhir_action,
            "recorded": now,
            "outcome": outcome.code(),
            "outcomeDesc": detail,
            "agent": [{
                "requestor": true,
                "who": { "display": actor.label() },
                "altId": actor.user,
                "network": actor.address.as_ref().map(|a| json!({"address": a, "type": "2"})),
                "name": actor.user_agent,
            }],
            "source": {
                "observer": { "display": "Helios Terminology Server" },
                "type": [{
                    "system": "http://terminology.hl7.org/CodeSystem/security-source-type",
                    "code": "4"
                }]
            },
            "entity": [{ "what": { "display": entity } }]
        });
        let event = strip_nulls(event);
        tracing::info!(
            target: "hts::audit",
            action,
            entity,
            outcome = outcome.code(),
            who = %actor.label(),
            detail = detail.unwrap_or(""),
            "admin action"
        );
        let _guard = self.lock.lock();
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(mut f) => {
                let _ = writeln!(f, "{event}");
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %self.path.display(), "audit log write failed")
            }
        }
    }

    /// The most recent `limit` events, newest first.
    pub fn recent(&self, limit: usize) -> Vec<Value> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        text.lines()
            .rev()
            .filter_map(|l| serde_json::from_str(l).ok())
            .take(limit)
            .collect()
    }
}

fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_nulls(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(strip_nulls).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_audit_events_as_ndjson() {
        let d = tempfile::tempdir().unwrap();
        let log = AuditLog::new(d.path().join("audit.ndjson"));
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "10.0.0.7, 172.16.0.1".parse().unwrap());
        let actor = Actor::from_headers(&h);
        assert_eq!(actor.label(), "10.0.0.7");
        log.record(
            &actor,
            "delete-code-system",
            "http://x|1",
            Outcome::Success,
            None,
        );
        log.record(
            &actor,
            "import",
            "upload abc",
            Outcome::SeriousFailure,
            Some("boom"),
        );
        let recent = log.recent(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0]["outcome"], "8");
        assert_eq!(recent[1]["action"], "D");
        assert_eq!(recent[1]["resourceType"], "AuditEvent");
        assert!(recent[1].get("outcomeDesc").is_none());
    }
}
