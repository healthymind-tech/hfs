//! End users of a realm and their links to FHIR resources (design §8.1).

use serde::{Deserialize, Serialize};

/// What kind of person a user is (drives consent wording, the patient
/// picker and which `fhirUser` resource type is linked).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UserKind {
    Patient,
    Practitioner,
    RelatedPerson,
    /// Staff/admin without a FHIR resource.
    Staff,
}

/// A link from a realm user to a FHIR resource on one bound endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FhirLink {
    /// Endpoint name.
    pub endpoint: String,
    /// `Patient/123`, `Practitioner/abc`, `PractitionerRole/…`,
    /// `RelatedPerson/…`, `Person/…`.
    pub reference: String,
    /// The link that becomes `fhirUser` on this endpoint.
    pub is_fhir_user: bool,
    /// Patients this user may pick (proxy / caregiver access): ids on the
    /// endpoint. A Patient user implicitly has its own id.
    #[serde(default)]
    pub accessible_patients: Vec<String>,
}

/// A realm user (`hfs_idp_users`; password/TOTP/WebAuthn credentials are in
/// `hfs_idp_credentials`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RealmUser {
    pub id: String,
    pub username: String,
    pub kind: UserKind,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub email_verified: bool,
    #[serde(default)]
    pub links: Vec<FhirLink>,
    /// Subject at an upstream IdP or the console (`console:{id}`).
    pub external_subject: Option<String>,
    pub disabled: bool,
}

impl RealmUser {
    /// The `fhirUser` claim for a token whose `aud` is `endpoint`, as an
    /// absolute URL (SMART: "the URL of a FHIR resource").
    pub fn fhir_user(&self, endpoint: &str, endpoint_base_url: &str) -> Option<String> {
        self.links
            .iter()
            .find(|l| l.endpoint == endpoint && l.is_fhir_user)
            .map(|l| {
                format!(
                    "{}/{}",
                    endpoint_base_url.trim_end_matches('/'),
                    l.reference
                )
            })
    }

    /// Patient ids the user may select in the standalone patient picker.
    pub fn selectable_patients(&self, endpoint: &str) -> Vec<String> {
        let mut out = vec![];
        for l in self.links.iter().filter(|l| l.endpoint == endpoint) {
            if let Some(id) = l.reference.strip_prefix("Patient/") {
                out.push(id.to_string());
            }
            out.extend(l.accessible_patients.iter().cloned());
        }
        out.sort();
        out.dedup();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fhir_user_and_picker() {
        let u = RealmUser {
            id: "u1".into(),
            username: "alice".into(),
            kind: UserKind::Patient,
            display_name: None,
            email: None,
            email_verified: false,
            links: vec![
                FhirLink {
                    endpoint: "clinic".into(),
                    reference: "Patient/p1".into(),
                    is_fhir_user: true,
                    accessible_patients: vec!["child1".into()],
                },
                FhirLink {
                    endpoint: "other".into(),
                    reference: "Patient/x".into(),
                    is_fhir_user: true,
                    accessible_patients: vec![],
                },
            ],
            external_subject: None,
            disabled: false,
        };
        assert_eq!(
            u.fhir_user("clinic", "https://h/fhir/clinic/").as_deref(),
            Some("https://h/fhir/clinic/Patient/p1")
        );
        assert_eq!(u.selectable_patients("clinic"), vec!["child1", "p1"]);
        assert!(u.fhir_user("nope", "https://h").is_none());
    }
}
