//! BALP AuditEvents for the authorization server's security events
//! (design §10.1, D-12; package I8).
//!
//! [`BalpEventSink`] implements [`IdpEventSink`]: every [`IdpEvent`] becomes
//! one FHIR R4 `AuditEvent` handed to the deployment's `helios-audit` sink
//! (database, file, …) on a background task, so the caller never waits.
//!
//! | event | AuditEvent | profile claimed |
//! |---|---|---|
//! | token issued / refreshed | type DCM 110114 *User Authenticated*, subtype `urn:ihe:event-type-code#ITI-71`, agents client (DCM 110150) + auth-server (`AuthzOauthService`) + user (IRCP), one token-request entity (role 24, `query` = the request without secrets) | `IHE.IUA.71` (BALP 1.1.4) |
//! | consent granted for a patient | type DCM 110113, subtype `AuthZsubType#AuthZ-Consent`, agents client / user / userorg / authorizer (= the audit source), entities patient + consent | `IHE.BasicAudit.AuthZconsent` |
//! | login success / failure, lockout | type DCM 110114, subtype DCM 110122 *Login* | — (BALP defines no login profile; ATNA pattern) |
//! | logout | type DCM 110114, subtype DCM 110123 *Logout* | — |
//! | consent denied / withdrawn, consent without a patient | type DCM 110113 | — |
//! | lockout | type DCM 110114, subtype DCM 110122, outcome 8 | — |
//! | revocation, refresh reuse, code replay, client-auth failure, key change | type DCM 110113 *Security Alert* | — |
//!
//! Events never carry credentials. Usernames of *failed* sign-ins are kept
//! (an auditor needs them to see credential stuffing) but no password ever.

use std::sync::Arc;

use base64::Engine;
use helios_audit::AuditSink;
use helios_idp::events::{IdpEvent, IdpEventSink};
use serde_json::{Value, json};

const DCM: &str = "http://dicom.nema.org/resources/ontology/DCM";
const BALP_AGENT: &str = "https://profiles.ihe.net/ITI/BALP/CodeSystem/UserAgentTypes";
const PARTICIPATION: &str = "http://terminology.hl7.org/CodeSystem/v3-ParticipationType";
const ENTITY_TYPE: &str = "http://terminology.hl7.org/CodeSystem/audit-entity-type";
const OBJECT_ROLE: &str = "http://terminology.hl7.org/CodeSystem/object-role";
const IHE_EVENT: &str = "urn:ihe:event-type-code";
const PROFILE_IUA71: &str = "https://profiles.ihe.net/ITI/BALP/StructureDefinition/IHE.IUA.71";
const PROFILE_AUTHZ_CONSENT: &str =
    "https://profiles.ihe.net/ITI/BALP/StructureDefinition/IHE.BasicAudit.AuthZconsent";

/// Realm, issuer and caller address of an event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventOrigin {
    /// Realm name.
    pub realm: String,
    /// `{public base}/idp/{realm}`.
    pub issuer: String,
    /// Caller address, when known.
    pub ip: Option<String>,
}

/// Turns [`IdpEvent`]s into BALP AuditEvents on a `helios-audit` sink.
pub struct BalpEventSink {
    sink: Arc<dyn AuditSink>,
    observer: String,
    public_base: String,
    runtime: Option<tokio::runtime::Handle>,
}

impl BalpEventSink {
    /// `observer` is `AuditEvent.source.observer` (e.g. `Device/hfs`);
    /// `public_base` the IdP's public origin (issuers are
    /// `{public_base}/idp/{realm}`).
    pub fn new(
        sink: Arc<dyn AuditSink>,
        observer: impl Into<String>,
        public_base: impl Into<String>,
    ) -> Self {
        Self {
            sink,
            observer: observer.into(),
            public_base: public_base.into().trim_end_matches('/').to_string(),
            runtime: tokio::runtime::Handle::try_current().ok(),
        }
    }
}

impl IdpEventSink for BalpEventSink {
    fn emit(&self, event: IdpEvent) {
        let json = to_audit_json(&event, &self.observer, &self.public_base);
        let parsed: Result<helios_audit::AuditEvent, _> = serde_json::from_value(json);
        let Ok(audit) = parsed else {
            tracing::error!(?event, "idp audit event did not map to an AuditEvent");
            return;
        };
        let sink = Arc::clone(&self.sink);
        let rt = self
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok());
        match rt {
            Some(rt) => {
                rt.spawn(async move { sink.record(audit).await });
            }
            None => tracing::warn!("idp audit event dropped: no tokio runtime"),
        }
    }
}

fn coding(system: &str, code: &str, display: &str) -> Value {
    json!({"system": system, "code": code, "display": display})
}

fn ident(system: &str, value: &str) -> Value {
    json!({"identifier": {"system": system, "value": value}, "display": value})
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn host_of(issuer: &str) -> String {
    url::Url::parse(issuer)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_else(|| issuer.to_string())
}

/// The authorization server as an agent.
fn server_agent(o: &EventOrigin, requestor: bool) -> Value {
    json!({
        "type": {"coding": [coding(BALP_AGENT, "AuthzOauthService", "OAuth Authorization Server")]},
        "who": ident(&o.issuer, &o.realm),
        "requestor": requestor,
        "network": {"address": host_of(&o.issuer), "type": "1"},
    })
}

/// The OAuth client as an agent (network = the caller's address).
fn client_agent(o: &EventOrigin, client_id: &str) -> Value {
    let mut a = json!({
        "type": {"coding": [coding(DCM, "110150", "Application")]},
        "who": ident(&format!("{}/clients", o.issuer), client_id),
        "requestor": false,
    });
    a["network"] =
        json!({"address": o.ip.clone().unwrap_or_else(|| "unknown".into()), "type": "2"});
    a
}

/// The person as an agent (IRCP = information recipient).
fn user_agent(o: &EventOrigin, user: &str, requestor: bool, with_network: bool) -> Value {
    let mut a = json!({
        "type": {"coding": [coding(PARTICIPATION, "IRCP", "information recipient")]},
        "who": ident(&format!("{}/users", o.issuer), user),
        "requestor": requestor,
    });
    if with_network && let Some(ip) = &o.ip {
        a["network"] = json!({"address": ip, "type": "2"});
    }
    a
}

fn detail(name: &str, value: &str) -> Value {
    let b64 = base64::engine::general_purpose::STANDARD.encode(value.as_bytes());
    json!({"type": name, "valueBase64Binary": b64})
}

fn base(
    observer: &str,
    type_coding: Value,
    subtype: Vec<Value>,
    outcome: &str,
    outcome_desc: Option<&str>,
) -> Value {
    let mut ev = json!({
        "resourceType": "AuditEvent",
        "type": type_coding,
        "subtype": subtype,
        "action": "E",
        "recorded": now(),
        "outcome": outcome,
        "source": {
            "observer": {"reference": observer},
            "type": [coding("http://terminology.hl7.org/CodeSystem/security-source-type", "6", "Security Server")],
        },
    });
    if let Some(d) = outcome_desc {
        ev["outcomeDesc"] = json!(d);
    }
    ev
}

fn security_alert(observer: &str, what: &str, outcome: &str, o: &EventOrigin) -> Value {
    base(
        observer,
        coding(DCM, "110113", "Security Alert"),
        vec![coding(
            "https://hfs.healthymind.dev/CodeSystem/idp-event",
            what,
            what,
        )],
        outcome,
        Some(what),
    )
    .as_object()
    .cloned()
    .map(|mut m| {
        m.insert("agent".into(), json!([server_agent(o, false)]));
        Value::Object(m)
    })
    .unwrap_or_default()
}

/// The AuditEvent JSON of an event (see the module table).
pub fn to_audit_json(event: &IdpEvent, observer: &str, public_base: &str) -> Value {
    let origin = |realm: &str, ip: &Option<String>| EventOrigin {
        realm: realm.to_string(),
        issuer: format!("{public_base}/idp/{realm}"),
        ip: ip.clone(),
    };
    match event {
        IdpEvent::TokenIssued {
            realm,
            client_id,
            subject,
            scope,
            aud,
            gid,
            jti,
            grant_type,
            patient,
            ip,
        } => {
            let mut query = format!(
                "grant_type={}&client_id={}&scope={}",
                enc(grant_type),
                enc(client_id),
                enc(scope)
            );
            for a in aud {
                query.push_str(&format!("&aud={}", enc(a)));
            }
            token_event(
                observer,
                &origin(realm, ip),
                client_id,
                (grant_type != "client_credentials").then_some(subject.as_str()),
                &query,
                &[("gid", gid), ("jti", jti), ("grant_type", grant_type)],
                patient.as_deref(),
            )
        }
        IdpEvent::TokenRefreshed {
            realm,
            client_id,
            subject,
            scope,
            gid,
            jti,
            ip,
        } => {
            let query = format!(
                "grant_type=refresh_token&client_id={}&scope={}",
                enc(client_id),
                enc(scope)
            );
            token_event(
                observer,
                &origin(realm, ip),
                client_id,
                Some(subject),
                &query,
                &[("gid", gid), ("jti", jti), ("grant_type", "refresh_token")],
                None,
            )
        }
        IdpEvent::LoginSucceeded {
            realm,
            user_id,
            client_id,
            amr,
            ip,
        } => {
            let o = origin(realm, ip);
            let mut ev = base(
                observer,
                coding(DCM, "110114", "User Authentication"),
                vec![coding(DCM, "110122", "Login")],
                "0",
                None,
            );
            let mut agents = vec![user_agent(&o, user_id, true, true), server_agent(&o, false)];
            if let Some(c) = client_id {
                agents.push(client_agent(&o, c));
            }
            ev["agent"] = json!(agents);
            ev["entity"] = json!([{
                "type": coding(ENTITY_TYPE, "2", "System Object"),
                "role": coding(OBJECT_ROLE, "4", "Domain Resource"),
                "what": ident(&o.issuer, &o.realm),
                "detail": [detail("amr", &amr.join(" "))],
            }]);
            ev
        }
        IdpEvent::LoginFailed {
            realm,
            reason,
            username,
            client_id,
            ip,
        } => {
            let o = origin(realm, ip);
            // A lockout is a refusal by policy ("8" serious failure);
            // a wrong credential a minor failure ("4").
            let outcome = if reason.starts_with("locked") {
                "8"
            } else {
                "4"
            };
            let mut ev = base(
                observer,
                coding(DCM, "110114", "User Authentication"),
                vec![coding(DCM, "110122", "Login")],
                outcome,
                Some(reason),
            );
            let name = username.clone().unwrap_or_else(|| "(none)".into());
            let mut user = json!({
                "type": {"coding": [coding(PARTICIPATION, "IRCP", "information recipient")]},
                "who": {"display": name},
                "name": name,
                "requestor": true,
            });
            if let Some(ip) = &o.ip {
                user["network"] = json!({"address": ip, "type": "2"});
            }
            let mut agents = vec![user, server_agent(&o, false)];
            if let Some(c) = client_id {
                agents.push(client_agent(&o, c));
            }
            ev["agent"] = json!(agents);
            ev
        }
        IdpEvent::Logout {
            realm,
            user_id,
            client_id,
            ip,
        } => {
            let o = origin(realm, ip);
            let mut ev = base(
                observer,
                coding(DCM, "110114", "User Authentication"),
                vec![coding(DCM, "110123", "Logout")],
                "0",
                None,
            );
            let mut agents = vec![];
            if let Some(u) = user_id {
                agents.push(user_agent(&o, u, true, true));
            }
            agents.push(server_agent(&o, user_id.is_none()));
            if let Some(c) = client_id {
                agents.push(client_agent(&o, c));
            }
            ev["agent"] = json!(agents);
            ev
        }
        IdpEvent::ConsentGranted {
            realm,
            user_id,
            client_id,
            endpoint,
            scopes,
            patient,
            remembered,
            ip,
        } => consent_event(
            observer,
            &origin(realm, ip),
            user_id,
            client_id,
            endpoint,
            patient.as_deref(),
            "consent-granted",
            "0",
            &[
                ("scope", &scopes.join(" ")),
                ("remembered", if *remembered { "true" } else { "false" }),
            ],
        ),
        IdpEvent::ConsentDenied {
            realm,
            user_id,
            client_id,
            endpoint,
            ip,
        } => consent_event(
            observer,
            &origin(realm, ip),
            user_id,
            client_id,
            endpoint,
            None,
            "consent-denied",
            "4",
            &[],
        ),
        IdpEvent::LockedOut {
            realm,
            username,
            until_unix,
            ip,
        } => {
            let o = origin(realm, ip);
            let mut ev = base(
                observer,
                coding(DCM, "110114", "User Authentication"),
                vec![coding(DCM, "110122", "Login")],
                "8",
                Some(&format!("account locked until {until_unix}")),
            );
            let name = username.clone().unwrap_or_else(|| "(none)".into());
            let mut user = json!({
                "type": {"coding": [coding(PARTICIPATION, "IRCP", "information recipient")]},
                "who": {"display": name},
                "requestor": true,
            });
            if let Some(ip) = &o.ip {
                user["network"] = json!({"address": ip, "type": "2"});
            }
            ev["agent"] = json!([user, server_agent(&o, false)]);
            ev
        }
        IdpEvent::ConsentWithdrawn {
            realm,
            user_id,
            client_id,
            endpoint,
            ip,
        } => consent_event(
            observer,
            &origin(realm, ip),
            user_id,
            client_id,
            endpoint.as_deref().unwrap_or(""),
            None,
            "consent-withdrawn",
            "0",
            &[],
        ),
        IdpEvent::TokenRevoked {
            realm,
            client_id,
            gid,
            ip,
        } => {
            let o = origin(realm, ip);
            let mut ev = security_alert(observer, "token-revoked", "0", &o);
            push_agent(&mut ev, client_agent(&o, client_id));
            ev["entity"] = json!([{
                "type": coding(ENTITY_TYPE, "2", "System Object"),
                "role": coding(OBJECT_ROLE, "4", "Domain Resource"),
                "what": ident(&format!("{}/grants", o.issuer), gid),
            }]);
            ev
        }
        IdpEvent::RefreshReuse {
            realm,
            client_id,
            family,
            ip,
        } => {
            let o = origin(realm, ip);
            let mut ev = security_alert(observer, "refresh-token-reuse", "8", &o);
            push_agent(&mut ev, client_agent(&o, client_id));
            ev["entity"] = json!([{
                "type": coding(ENTITY_TYPE, "2", "System Object"),
                "what": ident(&format!("{}/refresh-families", o.issuer), family),
            }]);
            ev
        }
        IdpEvent::CodeReplay {
            realm,
            client_id,
            ip,
        } => {
            let o = origin(realm, ip);
            let mut ev = security_alert(observer, "authorization-code-replay", "8", &o);
            push_agent(&mut ev, client_agent(&o, client_id));
            ev
        }
        IdpEvent::ClientAuthFailed {
            realm,
            client_id,
            reason,
            ip,
        } => {
            let o = origin(realm, ip);
            let mut ev = security_alert(observer, "client-authentication-failed", "4", &o);
            ev["outcomeDesc"] = json!(reason);
            if let Some(c) = client_id {
                push_agent(&mut ev, client_agent(&o, c));
            }
            ev
        }
        IdpEvent::KeyRotated { realm, kid, action } => {
            let o = origin(realm, &None);
            let mut ev = security_alert(observer, "signing-key-change", "0", &o);
            ev["entity"] = json!([{
                "type": coding(ENTITY_TYPE, "2", "System Object"),
                "what": ident(&format!("{}/jwks", o.issuer), kid),
                "detail": [detail("action", action)],
            }]);
            ev
        }
        IdpEvent::UserAccessChanged {
            realm,
            user_id,
            actor,
            action,
            added,
            removed,
            changed,
            total,
        } => {
            let o = origin(realm, &None);
            let mut ev = security_alert(observer, "user-access-change", "0", &o);
            ev["action"] = json!("U");
            push_agent(
                &mut ev,
                json!({
                    "type": {"coding": [coding(DCM, "110153", "Source Role ID")]},
                    "who": {"identifier": {"value": actor}, "display": actor},
                    "requestor": true,
                }),
            );
            ev["entity"] = json!([{
                "type": coding(ENTITY_TYPE, "2", "System Object"),
                "what": ident(&format!("{}/users", o.issuer), user_id),
                "detail": [
                    detail("action", action),
                    detail("added", &added.to_string()),
                    detail("removed", &removed.to_string()),
                    detail("changed", &changed.to_string()),
                    detail("total", &total.to_string()),
                ],
            }]);
            ev
        }
    }
}

fn push_agent(ev: &mut Value, agent: Value) {
    if let Some(a) = ev["agent"].as_array_mut() {
        a.push(agent);
    }
}

fn enc(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// ITI-71 "Get Access Token" (IHE.IUA.71).
fn token_event(
    observer: &str,
    origin: &EventOrigin,
    client_id: &str,
    user: Option<&str>,
    query: &str,
    details: &[(&str, &str)],
    patient: Option<&str>,
) -> Value {
    let mut ev = base(
        observer,
        coding(DCM, "110114", "User Authentication"),
        vec![coding(IHE_EVENT, "ITI-71", "Get Access Token")],
        "0",
        None,
    );
    ev["meta"] = json!({"profile": [PROFILE_IUA71]});
    let mut agents = vec![client_agent(origin, client_id), server_agent(origin, false)];
    if let Some(u) = user {
        agents.push(user_agent(origin, u, true, false));
    } else {
        agents[0]["requestor"] = json!(true);
    }
    ev["agent"] = json!(agents);
    let mut det: Vec<Value> = details.iter().map(|(n, v)| detail(n, v)).collect();
    if let Some(p) = patient {
        det.push(detail("patient", p));
    }
    ev["entity"] = json!([{
        "type": coding(ENTITY_TYPE, "2", "System Object"),
        "role": coding(OBJECT_ROLE, "24", "Query"),
        "query": base64::engine::general_purpose::STANDARD.encode(query.as_bytes()),
        "detail": det,
    }]);
    ev
}

/// Consent decisions; with a patient, the BALP AuthZconsent shape.
#[allow(clippy::too_many_arguments)]
fn consent_event(
    observer: &str,
    origin: &EventOrigin,
    user_id: &str,
    client_id: &str,
    endpoint: &str,
    patient: Option<&str>,
    what: &str,
    outcome: &str,
    details: &[(&str, &str)],
) -> Value {
    let mut ev = base(
        observer,
        coding(DCM, "110113", "Security Alert"),
        vec![coding(
            "https://hfs.healthymind.dev/CodeSystem/idp-event",
            what,
            what,
        )],
        outcome,
        Some(what),
    );
    let consent_id = format!("{user_id}|{client_id}|{endpoint}");
    let userorg = json!({
        "type": {"coding": [coding("http://terminology.hl7.org/CodeSystem/v3-RoleClass", "PROV", "healthcare provider")]},
        "who": {"display": origin.realm.clone(), "identifier": {"system": origin.issuer.clone(), "value": origin.realm.clone()}},
        "requestor": false,
    });
    // BALP val-audit-source: the authorizer is the audit source itself.
    let authorizer = json!({
        "type": {"coding": [coding("http://terminology.hl7.org/CodeSystem/extra-security-role-type", "authserver", "authorization server")]},
        "who": {"reference": observer},
        "requestor": false,
    });
    ev["agent"] = json!([
        client_agent(origin, client_id),
        user_agent(origin, user_id, true, false),
        userorg,
        authorizer,
    ]);
    let det: Vec<Value> = details.iter().map(|(n, v)| detail(n, v)).collect();
    let mut consent = json!({
        "what": ident(&format!("{}/consents", origin.issuer), &consent_id),
        "type": coding("http://hl7.org/fhir/resource-types", "Consent", "Consent"),
    });
    if !det.is_empty() {
        consent["detail"] = json!(det);
    }
    let mut entities = vec![];
    if let Some(p) = patient {
        let reference = if p.contains('/') {
            p.to_string()
        } else {
            format!("Patient/{p}")
        };
        entities.push(json!({
            "what": {"reference": reference},
            "type": coding(ENTITY_TYPE, "1", "Person"),
            "role": coding(OBJECT_ROLE, "1", "Patient"),
        }));
        if outcome == "0" && what == "consent-granted" {
            ev["meta"] = json!({"profile": [PROFILE_AUTHZ_CONSENT]});
            ev["subtype"] = json!([coding(
                "https://profiles.ihe.net/ITI/BALP/CodeSystem/AuthZsubType",
                "AuthZ-Consent",
                "Authorization Decision using Roles and Consent"
            )]);
        }
    }
    entities.push(consent);
    ev["entity"] = json!(entities);
    ev
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://hfs.test";

    fn parse(v: Value) -> helios_audit::AuditEvent {
        serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{e}: {v:#}"))
    }

    fn ip() -> Option<String> {
        Some("203.0.113.9".into())
    }

    #[test]
    fn token_issued_is_iua71() {
        let v = to_audit_json(
            &IdpEvent::TokenIssued {
                realm: "clinic".into(),
                client_id: "app".into(),
                subject: "u1".into(),
                scope: "openid patient/*.rs".into(),
                aud: vec!["https://hfs.test/fhir/lab".into()],
                gid: "g1".into(),
                jti: "j1".into(),
                grant_type: "authorization_code".into(),
                patient: Some("p1".into()),
                ip: ip(),
            },
            "Device/hfs",
            BASE,
        );
        assert_eq!(v["meta"]["profile"][0], PROFILE_IUA71);
        assert_eq!(v["type"]["code"], "110114");
        assert_eq!(v["subtype"][0]["code"], "ITI-71");
        assert_eq!(v["action"], "E");
        assert_eq!(v["outcome"], "0");
        let agents = v["agent"].as_array().unwrap();
        assert_eq!(agents.len(), 3);
        assert_eq!(agents[0]["type"]["coding"][0]["code"], "110150");
        assert_eq!(agents[0]["network"]["address"], "203.0.113.9");
        assert_eq!(agents[1]["type"]["coding"][0]["code"], "AuthzOauthService");
        assert_eq!(agents[1]["network"]["address"], "hfs.test");
        assert_eq!(
            agents[1]["who"]["identifier"]["system"],
            "https://hfs.test/idp/clinic"
        );
        assert_eq!(agents[2]["type"]["coding"][0]["code"], "IRCP");
        assert!(agents[2].get("network").is_none());
        let e = v["entity"].as_array().unwrap();
        assert_eq!(e.len(), 1);
        assert!(e[0].get("what").is_none());
        assert_eq!(e[0]["role"]["code"], "24");
        let q = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(e[0]["query"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert!(q.contains("grant_type=authorization_code"), "{q}");
        assert!(!q.contains("code_verifier") && !q.contains("code="), "{q}");
        parse(v);
    }

    #[test]
    fn backend_token_has_no_user_agent() {
        let v = to_audit_json(
            &IdpEvent::TokenIssued {
                realm: "clinic".into(),
                client_id: "svc".into(),
                subject: "svc".into(),
                scope: "system/*.rs".into(),
                aud: vec![],
                gid: "g".into(),
                jti: "j".into(),
                grant_type: "client_credentials".into(),
                patient: None,
                ip: None,
            },
            "Device/hfs",
            BASE,
        );
        assert_eq!(v["agent"].as_array().unwrap().len(), 2);
        assert_eq!(v["agent"][0]["requestor"], true);
        parse(v);
    }

    #[test]
    fn login_events() {
        let ok = to_audit_json(
            &IdpEvent::LoginSucceeded {
                realm: "clinic".into(),
                user_id: "u1".into(),
                client_id: Some("app".into()),
                amr: vec!["pwd".into()],
                ip: ip(),
            },
            "Device/hfs",
            BASE,
        );
        assert_eq!(ok["subtype"][0]["code"], "110122");
        assert_eq!(ok["outcome"], "0");
        parse(ok);
        let bad = to_audit_json(
            &IdpEvent::LoginFailed {
                realm: "clinic".into(),
                reason: "bad password".into(),
                username: Some("alice".into()),
                client_id: None,
                ip: ip(),
            },
            "Device/hfs",
            BASE,
        );
        assert_eq!(bad["outcome"], "4");
        parse(bad);
        let locked = to_audit_json(
            &IdpEvent::LoginFailed {
                realm: "clinic".into(),
                reason: "locked out".into(),
                username: Some("alice".into()),
                client_id: None,
                ip: ip(),
            },
            "Device/hfs",
            BASE,
        );
        assert_eq!(locked["outcome"], "8");
        let lo = to_audit_json(
            &IdpEvent::LockedOut {
                realm: "clinic".into(),
                username: Some("alice".into()),
                until_unix: 1,
                ip: ip(),
            },
            "Device/hfs",
            BASE,
        );
        assert_eq!(lo["outcome"], "8");
        parse(lo);
        let out = to_audit_json(
            &IdpEvent::Logout {
                realm: "clinic".into(),
                user_id: Some("u1".into()),
                client_id: None,
                ip: ip(),
            },
            "Device/hfs",
            BASE,
        );
        assert_eq!(out["subtype"][0]["code"], "110123");
        parse(out);
    }

    #[test]
    fn consent_with_patient_is_authz_consent() {
        let v = to_audit_json(
            &IdpEvent::ConsentGranted {
                realm: "clinic".into(),
                user_id: "u1".into(),
                client_id: "app".into(),
                endpoint: "lab".into(),
                scopes: vec!["patient/*.rs".into()],
                patient: Some("p1".into()),
                remembered: true,
                ip: ip(),
            },
            "Device/hfs",
            BASE,
        );
        assert_eq!(v["meta"]["profile"][0], PROFILE_AUTHZ_CONSENT);
        assert_eq!(v["agent"].as_array().unwrap().len(), 4);
        assert_eq!(v["entity"][0]["what"]["reference"], "Patient/p1");
        assert_eq!(v["entity"][1]["type"]["code"], "Consent");
        parse(v);
        let no_patient = to_audit_json(
            &IdpEvent::ConsentGranted {
                realm: "clinic".into(),
                user_id: "u1".into(),
                client_id: "app".into(),
                endpoint: "lab".into(),
                scopes: vec!["user/*.rs".into()],
                patient: None,
                remembered: false,
                ip: None,
            },
            "Device/hfs",
            BASE,
        );
        assert!(no_patient.get("meta").is_none(), "no profile claimed");
        parse(no_patient);
    }

    #[test]
    fn security_alerts_parse() {
        for e in [
            IdpEvent::TokenRevoked {
                realm: "clinic".into(),
                client_id: "app".into(),
                gid: "g".into(),
                ip: ip(),
            },
            IdpEvent::RefreshReuse {
                realm: "clinic".into(),
                client_id: "app".into(),
                family: "f".into(),
                ip: ip(),
            },
            IdpEvent::CodeReplay {
                realm: "clinic".into(),
                client_id: "app".into(),
                ip: ip(),
            },
            IdpEvent::ClientAuthFailed {
                realm: "clinic".into(),
                client_id: None,
                reason: "bad assertion".into(),
                ip: ip(),
            },
            IdpEvent::KeyRotated {
                realm: "clinic".into(),
                kid: "k".into(),
                action: "promoted".into(),
            },
            IdpEvent::ConsentWithdrawn {
                realm: "clinic".into(),
                user_id: "u".into(),
                client_id: "app".into(),
                endpoint: Some("lab".into()),
                ip: ip(),
            },
            IdpEvent::UserAccessChanged {
                realm: "clinic".into(),
                user_id: "u".into(),
                actor: "local:admin".into(),
                action: "replace".into(),
                added: 3,
                removed: 1,
                changed: 0,
                total: 120,
            },
            IdpEvent::ConsentDenied {
                realm: "clinic".into(),
                user_id: "u".into(),
                client_id: "app".into(),
                endpoint: "lab".into(),
                ip: ip(),
            },
        ] {
            let v = to_audit_json(&e, "Device/hfs", BASE);
            assert_eq!(v["resourceType"], "AuditEvent");
            assert_eq!(v["type"]["code"], "110113");
            parse(v);
        }
    }
}
