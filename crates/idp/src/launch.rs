//! SMART launch context (SMART App Launch 2.2, "Scopes and Launch Context":
//! token response parameters) and the EHR-launch handle.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One `fhirContext` entry: at least one of `reference`, `canonical`,
/// `identifier`; optional `type` and `role` (absent role ≡ `launch`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FhirContextItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identifier: Option<serde_json::Value>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

impl FhirContextItem {
    pub fn is_valid(&self) -> bool {
        self.reference.is_some() || self.canonical.is_some() || self.identifier.is_some()
    }
}

/// Context returned beside the access token (and kept on the grant so
/// refreshes and introspection return the same values).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchContext {
    /// Patient id (logical id on the bound endpoint).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patient: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encounter: Option<String>,
    #[serde(rename = "fhirContext", skip_serializing_if = "Vec::is_empty", default)]
    pub fhir_context: Vec<FhirContextItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub need_patient_banner: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smart_style_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}

impl LaunchContext {
    pub fn is_empty(&self) -> bool {
        *self == LaunchContext::default()
    }

    /// Merges the context into a token / introspection JSON object.
    pub fn write_into(&self, obj: &mut serde_json::Map<String, serde_json::Value>) {
        if let serde_json::Value::Object(m) = serde_json::to_value(self).expect("serializable") {
            obj.extend(m);
        }
    }
}

/// An EHR launch handle: created by the console's "Launch app" button (or
/// the `POST /idp/{realm}/launch` admin API), passed to the app as
/// `launch=…`, consumed once at `/authorize` (design §8.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchHandle {
    /// SHA-256 of the opaque value (the value itself is never stored).
    pub handle_hash: String,
    pub realm_id: String,
    /// The client it was created for (`None` = any client of the realm).
    pub client_id: Option<String>,
    /// The FHIR endpoint (`aud`) the launch is for.
    pub endpoint: String,
    /// The console user who launched (becomes the session subject when the
    /// realm's user source is `console`).
    pub launched_by: Option<String>,
    pub context: LaunchContext,
    pub expires_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_response_shape() {
        let c = LaunchContext {
            patient: Some("123".into()),
            fhir_context: vec![FhirContextItem {
                reference: Some("Appointment/a1".into()),
                canonical: None,
                identifier: None,
                resource_type: None,
                role: Some("https://example.org/role".into()),
            }],
            need_patient_banner: Some(true),
            ..Default::default()
        };
        let mut m = serde_json::Map::new();
        m.insert("access_token".into(), "x".into());
        c.write_into(&mut m);
        let v = serde_json::Value::Object(m);
        assert_eq!(v["patient"], "123");
        assert_eq!(v["need_patient_banner"], true);
        assert_eq!(v["fhirContext"][0]["reference"], "Appointment/a1");
        assert!(v.get("encounter").is_none());
        assert!(v["fhirContext"][0].get("canonical").is_none());
        assert!(LaunchContext::default().is_empty());
    }
}
