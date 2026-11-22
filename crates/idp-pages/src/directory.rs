//! Patient lookups for the patient picker (design §8.2 step 3).
//!
//! "The search runs against the FHIR layer **with the practitioner's own
//! permissions**, so the picker cannot reveal patients the user could not
//! read." [`FhirPatientDirectory`] does exactly that: it mints a 60-second
//! access token *for the signed-in user* with the realm's own signer —
//! `user/Patient.rs` for a practitioner, `patient/Patient.r` confined to one
//! patient for a patient or caregiver — and calls the bound endpoint over
//! the loopback interface, through the normal auth middleware (internal
//! verifier, scope policy, patient confinement). The token never leaves the
//! process. Tests use [`MemoryPatientDirectory`].

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use helios_idp::launch::LaunchContext;
use helios_idp::runtime::RealmRuntime;
use helios_idp::token::{AccessTokenClaims, AccessTokenInput};
use helios_idp::user::{RealmUser, UserKind};
use serde_json::Value;

/// One row of the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatientSummary {
    pub id: String,
    pub name: String,
    pub birth_date: Option<String>,
    pub gender: Option<String>,
}

impl PatientSummary {
    /// From a FHIR Patient resource.
    pub fn from_resource(r: &Value) -> Option<Self> {
        let id = r.get("id")?.as_str()?.to_string();
        let name = r
            .get("name")
            .and_then(|n| n.as_array())
            .and_then(|a| a.first())
            .map(|n| {
                if let Some(t) = n.get("text").and_then(Value::as_str) {
                    return t.to_string();
                }
                let given: Vec<&str> = n
                    .get("given")
                    .and_then(Value::as_array)
                    .map(|g| g.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let family = n.get("family").and_then(Value::as_str).unwrap_or("");
                format!("{} {}", given.join(" "), family).trim().to_string()
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("Patient/{id}"));
        Some(Self {
            id,
            name,
            birth_date: r.get("birthDate").and_then(Value::as_str).map(String::from),
            gender: r.get("gender").and_then(Value::as_str).map(String::from),
        })
    }
}

/// Patient lookups on behalf of a realm user.
#[async_trait]
pub trait PatientDirectory: Send + Sync + 'static {
    /// Patients matching `query` that `user` may read on `endpoint`
    /// (practitioner search). At most 20.
    async fn search(
        &self,
        realm: &RealmRuntime,
        endpoint: &str,
        user: &RealmUser,
        query: &str,
    ) -> Result<Vec<PatientSummary>, String>;

    /// The patient `id` as `user` may read it on `endpoint`; `None` when the
    /// user cannot read it (or it does not exist).
    async fn get(
        &self,
        realm: &RealmRuntime,
        endpoint: &str,
        user: &RealmUser,
        id: &str,
    ) -> Result<Option<PatientSummary>, String>;
}

/// In-memory directory (tests): `endpoint → patients`; practitioners may
/// read every patient of the endpoint, patients only their links.
#[derive(Debug, Default)]
pub struct MemoryPatientDirectory {
    pub patients: HashMap<String, Vec<PatientSummary>>,
}

#[async_trait]
impl PatientDirectory for MemoryPatientDirectory {
    async fn search(
        &self,
        _realm: &RealmRuntime,
        endpoint: &str,
        user: &RealmUser,
        query: &str,
    ) -> Result<Vec<PatientSummary>, String> {
        if user.kind != UserKind::Practitioner {
            return Ok(vec![]);
        }
        let q = query.to_lowercase();
        Ok(self
            .patients
            .get(endpoint)
            .map(|v| {
                v.iter()
                    .filter(|p| {
                        q.is_empty()
                            || p.name.to_lowercase().contains(&q)
                            || p.birth_date.as_deref() == Some(query)
                            || p.id == query
                    })
                    .take(20)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn get(
        &self,
        _realm: &RealmRuntime,
        endpoint: &str,
        user: &RealmUser,
        id: &str,
    ) -> Result<Option<PatientSummary>, String> {
        let allowed = user.kind == UserKind::Practitioner
            || user.selectable_patients(endpoint).iter().any(|p| p == id);
        if !allowed {
            return Ok(None);
        }
        Ok(self
            .patients
            .get(endpoint)
            .and_then(|v| v.iter().find(|p| p.id == id).cloned()))
    }
}

/// Internal base URL of an endpoint's FHIR API (loopback).
pub type InternalBase = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Looks patients up through the FHIR API of the same process (see the
/// module documentation).
pub struct FhirPatientDirectory {
    http: reqwest::Client,
    internal_base: InternalBase,
}

impl FhirPatientDirectory {
    pub fn new(internal_base: InternalBase) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default(),
            internal_base,
        }
    }

    fn token(
        &self,
        realm: &RealmRuntime,
        endpoint: &str,
        user: &RealmUser,
        patient: Option<&str>,
    ) -> Result<String, String> {
        let aud = realm
            .endpoint_base(endpoint)
            .ok_or("endpoint is not bound to the realm")?
            .to_string();
        let signer = realm
            .access_token_signer()
            .ok_or("realm has no active signing key")?;
        let scope = if patient.is_some() {
            "patient/Patient.r"
        } else {
            "user/Patient.rs"
        };
        let gid = format!("picker-{}", uuid_like());
        let claims = AccessTokenClaims::build(
            AccessTokenInput {
                issuer: realm.issuer(),
                subject: &user.id,
                audience: vec![aud],
                client_id: "hfs-idp-patient-picker",
                scope,
                ttl_secs: 60,
                auth_time: None,
                fhir_user: None,
                context: LaunchContext {
                    patient: patient.map(String::from),
                    ..Default::default()
                },
                cnf: None,
                grant_id: &gid,
            },
            Utc::now(),
        );
        signer
            .sign_jws("at+jwt", &claims)
            .map_err(|e| e.to_string())
    }

    async fn fetch(&self, url: &str, token: &str) -> Result<Option<Value>, String> {
        let r = self
            .http
            .get(url)
            .bearer_auth(token)
            .header("accept", "application/fhir+json")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        match r.status().as_u16() {
            200 => r.json().await.map(Some).map_err(|e| e.to_string()),
            401 | 403 | 404 | 410 => Ok(None),
            s => Err(format!("FHIR lookup answered {s}")),
        }
    }
}

fn uuid_like() -> String {
    use rand::RngCore;
    let mut b = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Search parameters for a free-text picker query: a date searches
/// `birthdate`, `system|value` or a digit-heavy string `identifier`,
/// anything else `name`.
pub fn search_params(query: &str) -> Vec<(&'static str, String)> {
    let q = query.trim();
    let is_date = q.len() == 10
        && q.chars().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                c == '-'
            } else {
                c.is_ascii_digit()
            }
        });
    let mut p = vec![("_count", "20".to_string())];
    if q.is_empty() {
    } else if is_date {
        p.push(("birthdate", q.to_string()));
    } else if q.contains('|') || q.chars().filter(char::is_ascii_digit).count() * 2 > q.len() {
        p.push(("identifier", q.to_string()));
    } else {
        p.push(("name", q.to_string()));
    }
    p
}

#[async_trait]
impl PatientDirectory for FhirPatientDirectory {
    async fn search(
        &self,
        realm: &RealmRuntime,
        endpoint: &str,
        user: &RealmUser,
        query: &str,
    ) -> Result<Vec<PatientSummary>, String> {
        if user.kind != UserKind::Practitioner {
            return Ok(vec![]);
        }
        let base = (self.internal_base)(endpoint).ok_or("endpoint has no internal URL")?;
        let token = self.token(realm, endpoint, user, None)?;
        let mut url = url::Url::parse(&format!("{}/Patient", base.trim_end_matches('/')))
            .map_err(|e| e.to_string())?;
        for (k, v) in search_params(query) {
            url.query_pairs_mut().append_pair(k, &v);
        }
        let Some(bundle) = self.fetch(url.as_str(), &token).await? else {
            return Ok(vec![]);
        };
        Ok(bundle
            .get("entry")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|e| e.get("resource"))
                    .filter_map(PatientSummary::from_resource)
                    .take(20)
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn get(
        &self,
        realm: &RealmRuntime,
        endpoint: &str,
        user: &RealmUser,
        id: &str,
    ) -> Result<Option<PatientSummary>, String> {
        if id.is_empty()
            || id.len() > 64
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        {
            return Ok(None);
        }
        let base = (self.internal_base)(endpoint).ok_or("endpoint has no internal URL")?;
        // Practitioners read with their own user/ permissions; everyone
        // else only patients linked to their account, each read with a
        // token confined to that one patient.
        let token = match user.kind {
            UserKind::Practitioner => self.token(realm, endpoint, user, None)?,
            _ => {
                if !user.selectable_patients(endpoint).iter().any(|p| p == id) {
                    return Ok(None);
                }
                self.token(realm, endpoint, user, Some(id))?
            }
        };
        let url = format!("{}/Patient/{id}", base.trim_end_matches('/'));
        Ok(self
            .fetch(&url, &token)
            .await?
            .as_ref()
            .and_then(PatientSummary::from_resource))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_from_resource() {
        let p = PatientSummary::from_resource(&serde_json::json!({
            "resourceType": "Patient", "id": "p1", "birthDate": "1970-01-02",
            "name": [{"given": ["Amy", "B"], "family": "Shaw"}]
        }))
        .unwrap();
        assert_eq!(p.name, "Amy B Shaw");
        assert_eq!(p.birth_date.as_deref(), Some("1970-01-02"));
        let anon = PatientSummary::from_resource(&serde_json::json!({"id": "x"})).unwrap();
        assert_eq!(anon.name, "Patient/x");
    }

    #[test]
    fn query_classification() {
        assert_eq!(search_params("1970-01-02")[1], ("birthdate", "1970-01-02".into()));
        assert_eq!(search_params("Shaw")[1], ("name", "Shaw".into()));
        assert_eq!(search_params("urn:mrn|123")[1].0, "identifier");
        assert_eq!(search_params("A123456")[1].0, "identifier");
        assert_eq!(search_params("").len(), 1);
    }
}
