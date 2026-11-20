//! IHE BALP `AuditEvent`s for FHIR RESTful interactions.
//!
//! [`build_rest_events`] turns a [`RestEventSpec`] (what was done) and the
//! [`RequestParties`] (who did it, from where, to which server) into the
//! `AuditEvent`s the BALP profiles describe:
//!
//! | Interaction | Profile (`IHE.BasicAudit.*`) |
//! |---|---|
//! | read, vread | `Read` / `PatientRead` |
//! | create (incl. update-as-create) | `Create` / `PatientCreate` |
//! | update, patch | `Update` / `PatientUpdate` |
//! | delete | `Delete` / `PatientDelete` |
//! | search, history, operations | `Query` / `PatientQuery` |
//!
//! A profile is claimed in `meta.profile` only when the event conforms to it.
//! BALP fixes `outcome` to `0` (success), so failed interactions are recorded
//! with their real outcome (`4` for a 4xx response, `8` for a 5xx) and an
//! `outcomeDesc`, under no BALP profile. Operations have no BALP profile of
//! their own; they are recorded on the Query pattern (action `E`, the request
//! in a query entity) with subtype `operation`, which is what the Query
//! profile's open subtype slicing admits.
//!
//! When an interaction involves several patients, one event is produced per
//! patient, as BALP requires (the patient entity is `1..1`).

use base64::Engine;

use crate::balp::{AuditAction, profiles};
use crate::fhir_model::{
    AuditEvent, AuditEventAgent, AuditEventAgentNetwork, AuditEventEntity, AuditEventSource,
    Identifier, Meta, Reference,
};
use crate::helpers::*;
use crate::interaction::{RestInteraction, classify, is_resource_type};

const DCM: &str = "http://dicom.nema.org/resources/ontology/DCM";
const PARTICIPATION: &str = "http://terminology.hl7.org/CodeSystem/v3-ParticipationType";
const PROVENANCE_PARTICIPANT: &str =
    "http://terminology.hl7.org/CodeSystem/provenance-participant-type";
const EVENT_TYPE: &str = "http://terminology.hl7.org/CodeSystem/audit-event-type";
const RESTFUL: &str = "http://hl7.org/fhir/restful-interaction";
const ENTITY_TYPE: &str = "http://terminology.hl7.org/CodeSystem/audit-entity-type";
const OBJECT_ROLE: &str = "http://terminology.hl7.org/CodeSystem/object-role";
const SOURCE_TYPE: &str = "http://terminology.hl7.org/CodeSystem/security-source-type";
const BALP_ENTITY_TYPE: &str = "https://profiles.ihe.net/ITI/BALP/CodeSystem/BasicAuditEntityType";

/// Upper bound on how much of a request is copied into a query entity.
pub const MAX_QUERY_BYTES: usize = 8 * 1024;
/// Upper bound on per-patient fan-out of one interaction.
pub const MAX_PATIENTS_PER_REQUEST: usize = 10;

/// The authenticated user behind a request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditUser {
    /// Token subject (`sub`).
    pub subject: String,
    /// Token issuer (`iss`), qualifying the subject.
    pub issuer: Option<String>,
    /// Human-readable name, when the token carries one.
    pub name: Option<String>,
    /// SMART `fhirUser` (e.g. `Practitioner/123`).
    pub fhir_user: Option<String>,
    /// OAuth client the token was issued to (`azp` / `client_id`).
    pub client_id: Option<String>,
}

/// The participants of a request other than the resource(s) acted on.
#[derive(Debug, Clone, Default)]
pub struct RequestParties {
    /// Network address of the client (IP).
    pub client_address: Option<String>,
    /// Authenticated user, if any.
    pub user: Option<AuditUser>,
    /// Reference recorded as the server and as `source.observer` (`Device/hfs`).
    pub server_observer: String,
    /// Public base URL of the server.
    pub server_address: String,
    /// `source.site` (host name of the server).
    pub site: Option<String>,
    /// `X-Request-Id` of the request.
    pub request_id: Option<String>,
}

/// What a request (or one bundle entry) did.
#[derive(Debug, Clone, Default)]
pub struct RestEventSpec {
    /// The interaction; `None` is recorded as [`RestInteraction::Other`].
    pub interaction: Option<RestInteraction>,
    pub resource_type: Option<String>,
    pub resource_id: Option<String>,
    pub version_id: Option<String>,
    /// The request as text (`GET <base>/Observation?patient=1`), carried in the
    /// query entity of query-shaped events.
    pub request_line: Option<String>,
    /// HTTP status of the response (or of the bundle entry).
    pub status: u16,
    pub outcome_desc: Option<String>,
    /// Patients involved (`Patient/<id>`), deduplicated.
    pub patients: Vec<String>,
    /// Extra name/value details (bundle correlation).
    pub details: Vec<(String, String)>,
}

impl RestEventSpec {
    /// Describes a request from its method, path (no tenant prefix) and raw
    /// query string. `form_body` is the `application/x-www-form-urlencoded`
    /// body of a `POST _search`, when captured.
    pub fn from_request(
        method: &str,
        path: &str,
        query: Option<&str>,
        form_body: Option<&str>,
        base_url: &str,
    ) -> Self {
        let c = classify(method, path);
        let mut spec = RestEventSpec {
            interaction: Some(c.interaction.clone()),
            resource_type: c.resource_type.clone(),
            resource_id: c.resource_id.clone(),
            version_id: c.version_id.clone(),
            ..Default::default()
        };

        let mut line = format!(
            "{} {}{}",
            method.to_ascii_uppercase(),
            base_url.trim_end_matches('/'),
            if path.starts_with('/') {
                path.to_string()
            } else {
                format!("/{path}")
            }
        );
        if let Some(q) = query.filter(|q| !q.is_empty()) {
            line.push('?');
            line.push_str(q);
        }
        if let Some(body) = form_body.filter(|b| !b.is_empty()) {
            line.push_str("\n\n");
            line.push_str(body);
        }
        spec.request_line = Some(truncate(line, MAX_QUERY_BYTES));

        // Patients named by the path.
        if c.resource_type.as_deref() == Some("Patient")
            && let Some(id) = &c.resource_id
        {
            spec.add_patient(&format!("Patient/{id}"));
        }
        if let Some((ct, cid)) = &c.compartment
            && ct == "Patient"
        {
            spec.add_patient(&format!("Patient/{cid}"));
        }
        // Patients named by search parameters.
        let searched_type = c.resource_type.as_deref();
        for params in [query, form_body].into_iter().flatten() {
            for p in patients_in_params(searched_type, params) {
                spec.add_patient(&p);
            }
        }
        spec
    }

    /// Adds a patient reference (normalized to `Patient/<id>`), ignoring
    /// duplicates and anything that is not a Patient reference.
    pub fn add_patient(&mut self, reference: &str) {
        if let Some(p) = normalize_patient_ref(reference)
            && !self.patients.contains(&p)
        {
            self.patients.push(p);
        }
    }
}

fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

/// `Patient/<id>` for a Patient reference (relative, absolute or versioned).
pub fn normalize_patient_ref(reference: &str) -> Option<String> {
    let segs: Vec<&str> = reference.split('/').filter(|s| !s.is_empty()).collect();
    let pos = segs.iter().rposition(|s| *s == "Patient")?;
    let id = segs.get(pos + 1)?;
    if id.is_empty() || id.starts_with('$') || id.starts_with('_') {
        return None;
    }
    Some(format!("Patient/{id}"))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Patients named by search parameters: `patient`, `subject` (when it points
/// at a Patient), their `:Patient` modifiers, and `_id` on a Patient search.
fn patients_in_params(resource_type: Option<&str>, params: &str) -> Vec<String> {
    let mut found = Vec::new();
    for pair in params.split('&') {
        let mut kv = pair.splitn(2, '=');
        let key = percent_decode(kv.next().unwrap_or(""));
        let value = percent_decode(kv.next().unwrap_or(""));
        if value.is_empty() {
            continue;
        }
        let (name, modifier) = match key.split_once(':') {
            Some((n, m)) => (n.to_string(), Some(m.to_string())),
            None => (key.clone(), None),
        };
        // Chained parameters (`patient.name=...`) do not name a patient id.
        if name.contains('.') {
            continue;
        }
        let typed_patient = modifier.as_deref() == Some("Patient");
        for v in value.split(',') {
            let v = v.trim();
            if v.is_empty() {
                continue;
            }
            match name.as_str() {
                "patient" if modifier.is_none() || typed_patient => {
                    if v.contains('/') {
                        found.extend(normalize_patient_ref(v));
                    } else {
                        found.push(format!("Patient/{v}"));
                    }
                }
                "subject" if typed_patient => {
                    found.push(format!("Patient/{}", v.rsplit('/').next().unwrap_or(v)))
                }
                "subject" if modifier.is_none() => found.extend(normalize_patient_ref(v)),
                "_id" if resource_type == Some("Patient") && modifier.is_none() => {
                    found.push(format!("Patient/{v}"))
                }
                _ => {}
            }
        }
    }
    found
}

/// Maps an HTTP status to `AuditEvent.outcome`: `0` success, `4` minor
/// failure (4xx — the request was refused), `8` serious failure (5xx).
pub fn outcome_for_status(status: u16) -> &'static str {
    match status {
        0..=399 => "0",
        400..=499 => "4",
        _ => "8",
    }
}

fn cc(system: &str, code: &str, display: &str) -> crate::fhir_model::CodeableConcept {
    codeable_concept(coding_display(system, code, display))
}

fn identifier(system: Option<&str>, value: &str) -> Box<Identifier> {
    Box::new(Identifier {
        system: system.map(uri),
        value: Some(fhir_string(value)),
        ..Default::default()
    })
}

/// Which BALP profile family an event belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Family {
    Read,
    Create,
    Update,
    Delete,
    Query,
    None,
}

fn family_of(interaction: &RestInteraction, status: u16) -> Family {
    match interaction {
        RestInteraction::Read | RestInteraction::Vread => Family::Read,
        RestInteraction::Create => Family::Create,
        // An update that created the resource is recorded as a create.
        RestInteraction::Update if status == 201 => Family::Create,
        RestInteraction::Update | RestInteraction::Patch => Family::Update,
        RestInteraction::Delete => Family::Delete,
        i if i.is_query_like() => Family::Query,
        _ => Family::None,
    }
}

fn profile_for(family: Family, patient: bool) -> Option<&'static str> {
    Some(match (family, patient) {
        (Family::Read, false) => profiles::READ,
        (Family::Read, true) => profiles::PATIENT_READ,
        (Family::Create, false) => profiles::CREATE,
        (Family::Create, true) => profiles::PATIENT_CREATE,
        (Family::Update, false) => profiles::UPDATE,
        (Family::Update, true) => profiles::PATIENT_UPDATE,
        (Family::Delete, false) => profiles::DELETE,
        (Family::Delete, true) => profiles::PATIENT_DELETE,
        (Family::Query, false) => profiles::QUERY,
        (Family::Query, true) => profiles::PATIENT_QUERY,
        (Family::None, _) => return None,
    })
}

/// Builds the `AuditEvent`(s) for one REST interaction.
pub fn build_rest_events(spec: &RestEventSpec, parties: &RequestParties) -> Vec<AuditEvent> {
    let patients: Vec<Option<&String>> = if spec.patients.is_empty() {
        vec![None]
    } else {
        spec.patients
            .iter()
            .take(MAX_PATIENTS_PER_REQUEST)
            .map(Some)
            .collect()
    };
    patients
        .into_iter()
        .map(|p| build_one(spec, parties, p.map(String::as_str)))
        .collect()
}

fn build_one(spec: &RestEventSpec, parties: &RequestParties, patient: Option<&str>) -> AuditEvent {
    let interaction = spec.interaction.clone().unwrap_or(RestInteraction::Other);
    let family = family_of(&interaction, spec.status);
    let action = match family {
        Family::Create => AuditAction::Create,
        _ => interaction.action(),
    };
    let subtype_code = match family {
        Family::Create => Some("create"),
        _ => interaction.code(),
    };
    let outcome = outcome_for_status(spec.status);

    // ── Entities ────────────────────────────────────────────────────────
    let mut entities = Vec::new();
    let details = (!spec.details.is_empty()).then(|| {
        spec.details
            .iter()
            .map(|(n, v)| entity_detail(n, v))
            .collect::<Vec<_>>()
    });
    let mut details_placed = false;
    let mut has_subject_entity = false;

    if family == Family::Query {
        let line = spec.request_line.clone().unwrap_or_default();
        entities.push(AuditEventEntity {
            r#type: Some(coding_display(ENTITY_TYPE, "2", "System Object")),
            role: Some(coding_display(OBJECT_ROLE, "24", "Query")),
            description: Some(fhir_string(line.clone())),
            query: Some(fhir_string(
                base64::engine::general_purpose::STANDARD.encode(line.as_bytes()),
            )),
            ..Default::default()
        });
        has_subject_entity = !line.is_empty();
    } else if let (Some(rt), Some(id)) = (&spec.resource_type, &spec.resource_id)
        && is_resource_type(rt)
        && !id.is_empty()
    {
        let what = match &spec.version_id {
            Some(v) => format!("{rt}/{id}/_history/{v}"),
            None => format!("{rt}/{id}"),
        };
        entities.push(AuditEventEntity {
            what: Some(reference(&what)),
            r#type: Some(coding_display(ENTITY_TYPE, "2", "System Object")),
            role: Some(coding_display(OBJECT_ROLE, "4", "Domain Resource")),
            detail: details.clone(),
            ..Default::default()
        });
        details_placed = true;
        has_subject_entity = true;
    } else if let Some(line) = &spec.request_line {
        // Nothing addressable (a non-FHIR endpoint, a conditional write that
        // matched nothing): keep the request itself, untyped so that it can
        // never be mistaken for a BALP data entity.
        entities.push(AuditEventEntity {
            description: Some(fhir_string(line.clone())),
            ..Default::default()
        });
    }

    if let Some(p) = patient {
        entities.push(AuditEventEntity {
            what: Some(reference(p)),
            r#type: Some(coding_display(ENTITY_TYPE, "1", "Person")),
            role: Some(coding_display(OBJECT_ROLE, "1", "Patient")),
            ..Default::default()
        });
    }

    if let Some(rid) = &parties.request_id {
        entities.push(AuditEventEntity {
            what: Some(Reference {
                identifier: Some(identifier(None, rid)),
                ..Default::default()
            }),
            r#type: Some(coding(BALP_ENTITY_TYPE, "XrequestId")),
            ..Default::default()
        });
    }

    if !details_placed && let Some(details) = details {
        entities.push(AuditEventEntity {
            detail: Some(details),
            ..Default::default()
        });
    }

    // ── Agents ──────────────────────────────────────────────────────────
    let (client_type, server_type) = match family {
        Family::Read => (
            cc(DCM, "110152", "Destination Role ID"),
            cc(DCM, "110153", "Source Role ID"),
        ),
        Family::Delete => (
            cc(DCM, "110150", "Application"),
            cc(PROVENANCE_PARTICIPANT, "custodian", "Custodian"),
        ),
        _ => (
            cc(DCM, "110153", "Source Role ID"),
            cc(DCM, "110152", "Destination Role ID"),
        ),
    };

    let user = parties.user.as_ref();
    let client_who = match user.and_then(|u| u.client_id.as_deref()) {
        Some(client_id) => Reference {
            identifier: Some(identifier(None, client_id)),
            display: parties.client_address.as_deref().map(fhir_string),
            ..Default::default()
        },
        None => Reference {
            display: Some(fhir_string(
                parties
                    .client_address
                    .as_deref()
                    .unwrap_or("unknown client"),
            )),
            ..Default::default()
        },
    };
    let client_network = AuditEventAgentNetwork {
        address: Some(fhir_string(
            parties.client_address.as_deref().unwrap_or("unknown"),
        )),
        r#type: parties.client_address.as_ref().map(|_| code("2")),
        ..Default::default()
    };

    let mut agents = vec![
        AuditEventAgent {
            r#type: Some(client_type),
            who: Some(client_who),
            requestor: boolean(user.is_none()),
            network: Some(client_network),
            ..Default::default()
        },
        AuditEventAgent {
            r#type: Some(server_type),
            who: Some(reference(&parties.server_observer)),
            requestor: boolean(false),
            network: Some(AuditEventAgentNetwork {
                address: Some(fhir_string(parties.server_address.clone())),
                r#type: Some(code("5")),
                ..Default::default()
            }),
            ..Default::default()
        },
    ];

    if let Some(u) = user {
        let user_type = match family {
            Family::Create | Family::Update | Family::Delete => {
                cc(PARTICIPATION, "AUT", "author (originator)")
            }
            _ => cc(PARTICIPATION, "IRCP", "information recipient"),
        };
        agents.push(AuditEventAgent {
            r#type: Some(user_type),
            who: Some(Reference {
                reference: u.fhir_user.as_deref().map(fhir_string),
                identifier: Some(identifier(u.issuer.as_deref(), &u.subject)),
                ..Default::default()
            }),
            name: u.name.as_deref().map(fhir_string),
            requestor: boolean(true),
            ..Default::default()
        });
    }

    // ── Profile ─────────────────────────────────────────────────────────
    let conforms = outcome == "0" && has_subject_entity;
    let profile = conforms
        .then(|| profile_for(family, patient.is_some()))
        .flatten();

    AuditEvent {
        id: Some(fhir_string(uuid::Uuid::new_v4().to_string())),
        meta: profile.map(|p| Meta {
            profile: Some(vec![canonical(p)]),
            ..Default::default()
        }),
        r#type: coding_display(EVENT_TYPE, "rest", "Restful Operation"),
        subtype: subtype_code.map(|c| vec![coding_display(RESTFUL, c, c)]),
        action: Some(code(action.to_code())),
        recorded: instant_now(),
        outcome: Some(code(outcome)),
        outcome_desc: spec.outcome_desc.as_deref().map(fhir_string),
        agent: Some(agents),
        source: AuditEventSource {
            site: parties.site.as_deref().map(fhir_string),
            observer: reference(&parties.server_observer),
            r#type: Some(vec![coding_display(SOURCE_TYPE, "4", "Application Server")]),
            ..Default::default()
        },
        entity: (!entities.is_empty()).then_some(entities),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parties(user: bool) -> RequestParties {
        RequestParties {
            client_address: Some("10.0.0.7".into()),
            user: user.then(|| AuditUser {
                subject: "u1".into(),
                issuer: Some("https://idp.example".into()),
                name: Some("Dr. Wang".into()),
                fhir_user: Some("Practitioner/pr1".into()),
                client_id: Some("ehr-app".into()),
            }),
            server_observer: "Device/hfs".into(),
            server_address: "https://fhir.example/fhir".into(),
            site: Some("fhir.example".into()),
            request_id: Some("req-1".into()),
        }
    }

    fn spec(method: &str, path: &str, query: Option<&str>, status: u16) -> RestEventSpec {
        let mut s =
            RestEventSpec::from_request(method, path, query, None, "https://fhir.example/fhir");
        s.status = status;
        s
    }

    fn json(e: &AuditEvent) -> serde_json::Value {
        serde_json::to_value(e).unwrap()
    }

    fn profile(e: &AuditEvent) -> Option<String> {
        json(e)["meta"]["profile"][0].as_str().map(str::to_string)
    }

    #[test]
    fn read_of_a_patient_is_patient_read() {
        let events = build_rest_events(&spec("GET", "/Patient/p1", None, 200), &parties(false));
        assert_eq!(events.len(), 1);
        let e = json(&events[0]);
        assert_eq!(e["meta"]["profile"][0], profiles::PATIENT_READ);
        assert_eq!(e["action"], "R");
        assert_eq!(e["outcome"], "0");
        assert_eq!(e["subtype"][0]["code"], "read");
        assert_eq!(e["subtype"][0]["display"], "read");
        assert_eq!(e["type"]["display"], "Restful Operation");
        assert_eq!(e["entity"][0]["what"]["reference"], "Patient/p1");
        assert_eq!(e["entity"][1]["what"]["reference"], "Patient/p1");
        assert_eq!(e["entity"][1]["role"]["code"], "1");
        assert_eq!(e["entity"][2]["what"]["identifier"]["value"], "req-1");
        // Read: the client is the destination of the data.
        assert_eq!(e["agent"][0]["type"]["coding"][0]["code"], "110152");
        assert_eq!(e["agent"][0]["network"]["address"], "10.0.0.7");
        assert_eq!(e["agent"][0]["network"]["type"], "2");
        assert_eq!(e["agent"][0]["requestor"], true);
        assert_eq!(e["agent"][1]["type"]["coding"][0]["code"], "110153");
        assert_eq!(e["agent"][1]["network"]["type"], "5");
        assert_eq!(e["source"]["type"][0]["code"], "4");
    }

    #[test]
    fn vread_references_the_version() {
        let mut s = spec("GET", "/Observation/o1/_history/2", None, 200);
        s.add_patient("Patient/p9");
        let e = json(&build_rest_events(&s, &parties(false))[0]);
        assert_eq!(e["subtype"][0]["code"], "vread");
        assert_eq!(
            e["entity"][0]["what"]["reference"],
            "Observation/o1/_history/2"
        );
        assert_eq!(e["meta"]["profile"][0], profiles::PATIENT_READ);
    }

    #[test]
    fn search_records_the_query_and_the_patient() {
        let s = spec("GET", "/Observation", Some("patient=p1&code=1234-5"), 200);
        let events = build_rest_events(&s, &parties(true));
        let e = json(&events[0]);
        assert_eq!(e["meta"]["profile"][0], profiles::PATIENT_QUERY);
        assert_eq!(e["action"], "E");
        assert_eq!(e["subtype"][0]["code"], "search-type");
        assert_eq!(e["entity"][0]["role"]["code"], "24");
        let q = e["entity"][0]["query"].as_str().unwrap();
        let decoded = base64::engine::general_purpose::STANDARD.decode(q).unwrap();
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            "GET https://fhir.example/fhir/Observation?patient=p1&code=1234-5"
        );
        assert!(e["entity"][0].get("what").is_none());
        assert_eq!(e["entity"][1]["what"]["reference"], "Patient/p1");
        // With a user: the client is not the requestor, the user is.
        assert_eq!(e["agent"][0]["requestor"], false);
        assert_eq!(e["agent"][0]["who"]["identifier"]["value"], "ehr-app");
        assert_eq!(e["agent"][2]["type"]["coding"][0]["code"], "IRCP");
        assert_eq!(e["agent"][2]["who"]["reference"], "Practitioner/pr1");
        assert_eq!(e["agent"][2]["who"]["identifier"]["value"], "u1");
        assert_eq!(e["agent"][2]["requestor"], true);
    }

    #[test]
    fn search_without_patient_is_query() {
        let s = spec("GET", "/Patient", Some("name=chen"), 200);
        let events = build_rest_events(&s, &parties(false));
        assert_eq!(profile(&events[0]).as_deref(), Some(profiles::QUERY));
    }

    #[test]
    fn several_patients_fan_out_to_one_event_each() {
        let s = spec(
            "GET",
            "/Observation",
            Some("subject=Patient/a,Patient/b"),
            200,
        );
        let events = build_rest_events(&s, &parties(false));
        assert_eq!(events.len(), 2);
        assert_eq!(
            json(&events[1])["entity"][1]["what"]["reference"],
            "Patient/b"
        );
    }

    #[test]
    fn patient_parameters_are_recognized() {
        let p = |t: Option<&str>, q: &str| patients_in_params(t, q);
        assert_eq!(p(Some("Observation"), "patient=1"), vec!["Patient/1"]);
        assert_eq!(
            p(Some("Observation"), "patient=Patient%2F1"),
            vec!["Patient/1"]
        );
        assert_eq!(
            p(Some("Observation"), "subject=Group/1"),
            Vec::<String>::new()
        );
        assert_eq!(
            p(Some("Observation"), "subject:Patient=7"),
            vec!["Patient/7"]
        );
        assert_eq!(
            p(Some("Patient"), "_id=3,4"),
            vec!["Patient/3", "Patient/4"]
        );
        assert_eq!(p(Some("Observation"), "_id=3"), Vec::<String>::new());
        assert_eq!(
            p(Some("Observation"), "patient.name=x"),
            Vec::<String>::new()
        );
        assert_eq!(
            p(Some("Observation"), "patient=http://h/fhir/Patient/9"),
            vec!["Patient/9"]
        );
    }

    #[test]
    fn compartment_search_is_patient_query() {
        let s = spec("GET", "/Patient/p1/Observation", None, 200);
        let e = json(&build_rest_events(&s, &parties(false))[0]);
        assert_eq!(e["meta"]["profile"][0], profiles::PATIENT_QUERY);
        assert_eq!(e["entity"][1]["what"]["reference"], "Patient/p1");
    }

    #[test]
    fn create_update_delete_agents_and_profiles() {
        let mut s = spec("POST", "/Observation", None, 201);
        s.resource_id = Some("o1".into());
        s.add_patient("Patient/p1");
        let e = json(&build_rest_events(&s, &parties(true))[0]);
        assert_eq!(e["meta"]["profile"][0], profiles::PATIENT_CREATE);
        assert_eq!(e["action"], "C");
        assert_eq!(e["agent"][0]["type"]["coding"][0]["code"], "110153");
        assert_eq!(e["agent"][2]["type"]["coding"][0]["code"], "AUT");

        // Update-as-create is a create.
        let s = spec("PUT", "/Basic/b1", None, 201);
        let e = json(&build_rest_events(&s, &parties(false))[0]);
        assert_eq!(e["action"], "C");
        assert_eq!(e["subtype"][0]["code"], "create");
        assert_eq!(e["meta"]["profile"][0], profiles::CREATE);

        let s = spec("PATCH", "/Basic/b1", None, 200);
        let e = json(&build_rest_events(&s, &parties(false))[0]);
        assert_eq!(e["meta"]["profile"][0], profiles::UPDATE);
        assert_eq!(e["subtype"][0]["code"], "patch");

        let s = spec("DELETE", "/Basic/b1", None, 204);
        let e = json(&build_rest_events(&s, &parties(false))[0]);
        assert_eq!(e["meta"]["profile"][0], profiles::DELETE);
        assert_eq!(e["agent"][0]["type"]["coding"][0]["code"], "110150");
        assert_eq!(e["agent"][1]["type"]["coding"][0]["code"], "custodian");
    }

    #[test]
    fn failures_carry_their_outcome_and_claim_no_profile() {
        for (status, outcome) in [(401, "4"), (403, "4"), (404, "4"), (422, "4"), (500, "8")] {
            let mut s = spec("GET", "/Patient/p1", None, status);
            s.outcome_desc = Some(format!("HTTP {status}"));
            let e = json(&build_rest_events(&s, &parties(false))[0]);
            assert_eq!(e["outcome"], outcome, "status {status}");
            assert_eq!(e["outcomeDesc"], format!("HTTP {status}"));
            assert!(e.get("meta").is_none(), "status {status}");
        }
    }

    #[test]
    fn operations_use_the_query_pattern() {
        let s = spec("GET", "/Patient/p1/$everything", None, 200);
        let e = json(&build_rest_events(&s, &parties(false))[0]);
        assert_eq!(e["meta"]["profile"][0], profiles::PATIENT_QUERY);
        assert_eq!(e["subtype"][0]["code"], "operation");
        assert_eq!(e["action"], "E");
        assert!(
            e["entity"][0]["description"]
                .as_str()
                .unwrap()
                .ends_with("$everything")
        );
    }

    #[test]
    fn non_fhir_endpoints_record_the_request_untyped() {
        let s = spec("GET", "/export-file/job1/1", None, 200);
        let e = json(&build_rest_events(&s, &parties(false))[0]);
        assert!(e.get("meta").is_none());
        assert!(e.get("subtype").is_none());
        assert!(e["entity"][0].get("type").is_none());
        assert!(
            e["entity"][0]["description"]
                .as_str()
                .unwrap()
                .contains("export-file")
        );
    }

    #[test]
    fn bundle_details_land_on_the_data_entity_or_their_own() {
        let mut s = spec("GET", "/Patient/p1", None, 200);
        s.details.push(("bundle-id".into(), "b1".into()));
        let e = json(&build_rest_events(&s, &parties(false))[0]);
        assert_eq!(e["entity"][0]["detail"][0]["type"], "bundle-id");

        let mut s = spec("GET", "/Observation", Some("code=x"), 200);
        s.details.push(("bundle-id".into(), "b1".into()));
        let e = json(&build_rest_events(&s, &parties(false))[0]);
        // The query entity must not carry details (BALP: detail 0..0).
        assert!(e["entity"][0].get("detail").is_none());
        let last = e["entity"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last["detail"][0]["valueString"], "b1");
        assert!(last.get("type").is_none());
    }

    #[test]
    fn query_capture_is_bounded() {
        let long = "x".repeat(MAX_QUERY_BYTES * 2);
        let s = spec("GET", "/Patient", Some(&format!("name={long}")), 200);
        assert!(s.request_line.unwrap().len() <= MAX_QUERY_BYTES);
    }
}

#[cfg(test)]
#[path = "rest_event_hm_tests.rs"]
mod hm_tests;
