//! One change read at a source: a resource version or a delete.

use chrono::{DateTime, Utc};
use serde_json::Value;

/// A change read at the source.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// Resource type.
    pub resource_type: String,
    /// Logical id.
    pub id: String,
    /// Source `meta.versionId` (absent for some deletes).
    pub version: Option<String>,
    /// Source `meta.lastUpdated` (or the delete time).
    pub last_updated: DateTime<Utc>,
    /// `true` for a delete.
    pub deleted: bool,
    /// The content for a create/update.
    pub resource: Option<Value>,
}

impl Change {
    /// A create/update built from a resource's own `meta`; `None` when the
    /// resource has no type or id. `fallback_time` is used when the resource
    /// carries no `meta.lastUpdated`.
    pub fn from_resource(resource: Value, fallback_time: DateTime<Utc>) -> Option<Self> {
        let resource_type = resource.get("resourceType")?.as_str()?.to_string();
        let id = resource.get("id")?.as_str()?.to_string();
        if !valid_id(&id) || !valid_type(&resource_type) {
            return None;
        }
        let meta = resource.get("meta");
        let version = meta
            .and_then(|m| m.get("versionId"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let last_updated = meta
            .and_then(|m| m.get("lastUpdated"))
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or(fallback_time);
        Some(Self {
            resource_type,
            id,
            version,
            last_updated,
            deleted: false,
            resource: Some(resource),
        })
    }

    /// A delete.
    pub fn delete(
        resource_type: impl Into<String>,
        id: impl Into<String>,
        version: Option<String>,
        at: DateTime<Utc>,
    ) -> Self {
        Self {
            resource_type: resource_type.into(),
            id: id.into(),
            version,
            last_updated: at,
            deleted: true,
            resource: None,
        }
    }

    /// `type/id`.
    pub fn key(&self) -> String {
        format!("{}/{}", self.resource_type, self.id)
    }

    /// `type/id/version` — the cursor boundary token and the log form.
    pub fn token(&self) -> String {
        format!(
            "{}/{}/{}",
            self.resource_type,
            self.id,
            self.version
                .as_deref()
                .unwrap_or(if self.deleted { "deleted" } else { "-" })
        )
    }
}

/// FHIR id grammar: `[A-Za-z0-9\-\.]{1,64}`.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// Resource type grammar: an upper-case letter then letters.
pub fn valid_type(t: &str) -> bool {
    let mut chars = t.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase())
        && t.len() <= 64
        && chars.all(|c| c.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn from_resource_reads_meta() {
        let t0 = Utc::now();
        let c = Change::from_resource(
            json!({"resourceType": "Patient", "id": "p-1",
                   "meta": {"versionId": "3", "lastUpdated": "2026-01-02T03:04:05.678Z"}}),
            t0,
        )
        .unwrap();
        assert_eq!(c.version.as_deref(), Some("3"));
        assert_eq!(c.last_updated.to_rfc3339(), "2026-01-02T03:04:05.678+00:00");
        assert_eq!(c.key(), "Patient/p-1");
        assert_eq!(c.token(), "Patient/p-1/3");
        let bare =
            Change::from_resource(json!({"resourceType": "Patient", "id": "x"}), t0).unwrap();
        assert_eq!(bare.last_updated, t0);
        assert_eq!(bare.token(), "Patient/x/-");
        assert_eq!(
            Change::delete("Patient", "x", None, t0).token(),
            "Patient/x/deleted"
        );
    }

    #[test]
    fn rejects_bad_ids_and_types() {
        let t0 = Utc::now();
        assert!(Change::from_resource(json!({"resourceType": "Patient"}), t0).is_none());
        assert!(
            Change::from_resource(json!({"resourceType": "Patient", "id": "../x"}), t0).is_none()
        );
        assert!(Change::from_resource(json!({"resourceType": "patient", "id": "x"}), t0).is_none());
        assert!(valid_id("a.b-C9"));
        assert!(!valid_id(&"a".repeat(65)));
        assert!(!valid_type("Pa1"));
    }
}
