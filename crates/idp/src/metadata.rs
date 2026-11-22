//! Discovery documents derived from a realm spec (design §5.3):
//!
//! * `{issuer}/.well-known/openid-configuration` (OIDC Discovery §4) and
//!   `{origin}/.well-known/oauth-authorization-server/idp/{realm}`
//!   (RFC 8414 §3, path insertion) — the same JSON.
//! * `{fhir-base}/.well-known/smart-configuration` for each bound endpoint
//!   (SMART App Launch 2.2 "Conformance").

use serde_json::{Value, json};

use crate::realm::{DcrPolicy, RealmSpec};

/// Realm endpoint URLs, all under the issuer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealmUrls {
    pub issuer: String,
    pub authorization: String,
    pub token: String,
    pub jwks: String,
    pub userinfo: String,
    pub introspection: String,
    pub revocation: String,
    pub registration: String,
    pub par: String,
    pub device_authorization: String,
    pub end_session: String,
    /// Hosted account page (SMART `management_endpoint`: the user reviews
    /// and revokes app authorizations).
    pub management: String,
}

impl RealmUrls {
    pub fn new(issuer: &str) -> Self {
        let u = |p: &str| format!("{issuer}/{p}");
        Self {
            issuer: issuer.to_string(),
            authorization: u("authorize"),
            token: u("token"),
            jwks: u("jwks"),
            userinfo: u("userinfo"),
            introspection: u("introspect"),
            revocation: u("revoke"),
            registration: u("register"),
            par: u("par"),
            device_authorization: u("device_authorization"),
            end_session: u("logout"),
            management: u("account/apps"),
        }
    }
}

/// Client-assertion algorithms the token endpoint accepts (SMART asymmetric
/// auth: at least one of RS384 / ES384).
pub const CLIENT_ASSERTION_ALGS: &[&str] = &["RS384", "ES384", "RS256", "ES256", "PS256", "PS384"];

fn grant_types(r: &RealmSpec) -> Vec<&'static str> {
    let f = &r.features;
    let mut g = vec![];
    if f.launch_ehr || f.launch_standalone {
        g.push("authorization_code");
    }
    if f.offline_access || f.online_access {
        g.push("refresh_token");
    }
    if f.backend_services {
        g.push("client_credentials");
    }
    if f.device_authorization {
        g.push("urn:ietf:params:oauth:grant-type:device_code");
    }
    if f.token_exchange {
        g.push("urn:ietf:params:oauth:grant-type:token-exchange");
    }
    g
}

fn auth_methods(r: &RealmSpec) -> Vec<&'static str> {
    let f = &r.features;
    let mut m = vec![];
    if f.client_public {
        m.push("none");
    }
    if f.client_confidential_symmetric {
        m.push("client_secret_basic");
        m.push("client_secret_post");
    }
    if f.client_confidential_asymmetric {
        m.push("private_key_jwt");
    }
    m
}

/// The SMART `capabilities` array for this realm.
pub fn smart_capabilities(r: &RealmSpec) -> Vec<&'static str> {
    let f = &r.features;
    let mut c = vec![];
    let mut add = |on: bool, s: &'static str| {
        if on {
            c.push(s)
        }
    };
    add(f.launch_ehr, "launch-ehr");
    add(f.launch_standalone, "launch-standalone");
    add(f.launch_ehr || f.launch_standalone, "authorize-post");
    add(f.client_public, "client-public");
    add(
        f.client_confidential_symmetric,
        "client-confidential-symmetric",
    );
    add(
        f.client_confidential_asymmetric,
        "client-confidential-asymmetric",
    );
    add(f.openid_connect, "sso-openid-connect");
    add(f.context_banner, "context-banner");
    add(f.context_style, "context-style");
    add(f.launch_ehr, "context-ehr-patient");
    add(f.launch_ehr, "context-ehr-encounter");
    add(f.launch_standalone, "context-standalone-patient");
    add(f.launch_standalone, "context-standalone-encounter");
    add(f.offline_access, "permission-offline");
    add(f.online_access, "permission-online");
    add(f.permission_patient, "permission-patient");
    add(f.permission_user, "permission-user");
    add(f.permission_v1, "permission-v1");
    add(true, "permission-v2");
    c
}

fn scopes_supported(r: &RealmSpec) -> Vec<&'static str> {
    let f = &r.features;
    let mut s = vec![];
    if f.openid_connect {
        s.extend(["openid", "fhirUser", "profile", "email"]);
    }
    if f.launch_ehr {
        s.push("launch");
    }
    if f.launch_standalone {
        s.extend(["launch/patient", "launch/encounter"]);
    }
    if f.offline_access {
        s.push("offline_access");
    }
    if f.online_access {
        s.push("online_access");
    }
    if f.permission_patient {
        s.push("patient/*.cruds");
    }
    if f.permission_user {
        s.push("user/*.cruds");
    }
    if f.backend_services {
        s.push("system/*.cruds");
    }
    s
}

/// OIDC Discovery / RFC 8414 metadata.
pub fn authorization_server_metadata(r: &RealmSpec, urls: &RealmUrls) -> Value {
    let f = &r.features;
    let mut m = json!({
        "issuer": urls.issuer,
        "authorization_endpoint": urls.authorization,
        "token_endpoint": urls.token,
        "jwks_uri": urls.jwks,
        "response_types_supported": ["code"],
        "response_modes_supported": ["query", "form_post"],
        "grant_types_supported": grant_types(r),
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": auth_methods(r),
        "token_endpoint_auth_signing_alg_values_supported": CLIENT_ASSERTION_ALGS,
        "scopes_supported": scopes_supported(r),
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": r.signing_algs.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
        "claims_supported": ["iss","sub","aud","exp","iat","auth_time","nonce","acr","amr","sid","at_hash","fhirUser","name","given_name","family_name","email","email_verified"],
        "request_parameter_supported": false,
        "request_uri_parameter_supported": false,
        "claims_parameter_supported": false,
        "prompt_values_supported": ["none", "login", "consent", "select_account"],
        // RFC 9207: `iss` in every authorization response (mix-up defence).
        "authorization_response_iss_parameter_supported": true,
        "ui_locales_supported": ["en", "zh-TW"],
    });
    let o = m.as_object_mut().expect("object");
    if f.openid_connect {
        o.insert("userinfo_endpoint".into(), json!(urls.userinfo));
        o.insert("end_session_endpoint".into(), json!(urls.end_session));
        o.insert("frontchannel_logout_supported".into(), json!(false));
        o.insert("backchannel_logout_supported".into(), json!(false));
    }
    if f.introspection {
        o.insert("introspection_endpoint".into(), json!(urls.introspection));
        o.insert(
            "introspection_endpoint_auth_methods_supported".into(),
            json!(auth_methods(r)),
        );
    }
    if f.revocation {
        o.insert("revocation_endpoint".into(), json!(urls.revocation));
        o.insert(
            "revocation_endpoint_auth_methods_supported".into(),
            json!(auth_methods(r)),
        );
    }
    if f.dynamic_registration != DcrPolicy::Off {
        o.insert("registration_endpoint".into(), json!(urls.registration));
    }
    if f.par {
        o.insert(
            "pushed_authorization_request_endpoint".into(),
            json!(urls.par),
        );
        o.insert("require_pushed_authorization_requests".into(), json!(false));
    }
    if f.dpop {
        o.insert(
            "dpop_signing_alg_values_supported".into(),
            json!(["ES256", "RS256", "PS256"]),
        );
    }
    if f.device_authorization {
        o.insert(
            "device_authorization_endpoint".into(),
            json!(urls.device_authorization),
        );
    }
    m
}

/// SMART configuration for one bound FHIR endpoint.
pub fn smart_configuration(r: &RealmSpec, urls: &RealmUrls) -> Value {
    let f = &r.features;
    let mut m = json!({
        "issuer": urls.issuer,
        "jwks_uri": urls.jwks,
        "authorization_endpoint": urls.authorization,
        "token_endpoint": urls.token,
        "grant_types_supported": grant_types(r),
        "token_endpoint_auth_methods_supported": auth_methods(r),
        "token_endpoint_auth_signing_alg_values_supported": CLIENT_ASSERTION_ALGS,
        "scopes_supported": scopes_supported(r),
        "response_types_supported": ["code"],
        "code_challenge_methods_supported": ["S256"],
        "capabilities": smart_capabilities(r),
        "management_endpoint": urls.management,
    });
    let o = m.as_object_mut().expect("object");
    if f.introspection {
        o.insert("introspection_endpoint".into(), json!(urls.introspection));
    }
    if f.revocation {
        o.insert("revocation_endpoint".into(), json!(urls.revocation));
    }
    if f.dynamic_registration != DcrPolicy::Off {
        o.insert("registration_endpoint".into(), json!(urls.registration));
    }
    if f.openid_connect {
        o.insert("userinfo_endpoint".into(), json!(urls.userinfo));
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realm::RealmName;

    fn realm() -> RealmSpec {
        RealmSpec::new(RealmName::parse("patients").unwrap(), "Patients")
    }

    #[test]
    fn smart_required_fields() {
        let r = realm();
        let urls = RealmUrls::new("https://h/idp/patients");
        let s = smart_configuration(&r, &urls);
        for k in [
            "grant_types_supported",
            "token_endpoint",
            "capabilities",
            "code_challenge_methods_supported",
            "issuer",
            "jwks_uri",
            "authorization_endpoint",
        ] {
            assert!(s.get(k).is_some(), "{k}");
        }
        assert_eq!(s["code_challenge_methods_supported"], json!(["S256"]));
        let caps: Vec<&str> = s["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for c in [
            "launch-ehr",
            "launch-standalone",
            "client-public",
            "client-confidential-symmetric",
            "client-confidential-asymmetric",
            "sso-openid-connect",
            "context-ehr-patient",
            "context-standalone-patient",
            "permission-offline",
            "permission-patient",
            "permission-user",
            "permission-v1",
            "permission-v2",
            "authorize-post",
        ] {
            assert!(caps.contains(&c), "{c}");
        }
        assert!(!caps.contains(&"permission-online"));
        let algs = s["token_endpoint_auth_signing_alg_values_supported"]
            .as_array()
            .unwrap();
        assert!(algs.contains(&json!("RS384")) && algs.contains(&json!("ES384")));
    }

    #[test]
    fn as_metadata_follows_features() {
        let mut r = realm();
        let urls = RealmUrls::new("https://h/idp/patients");
        let m = authorization_server_metadata(&r, &urls);
        assert_eq!(m["issuer"], "https://h/idp/patients");
        assert!(m.get("registration_endpoint").is_none());
        assert!(m.get("pushed_authorization_request_endpoint").is_none());
        assert_eq!(m["authorization_response_iss_parameter_supported"], true);
        assert!(
            m["grant_types_supported"]
                .as_array()
                .unwrap()
                .contains(&json!("client_credentials"))
        );
        r.features.par = true;
        r.features.dynamic_registration = DcrPolicy::InitialAccessToken;
        r.features.backend_services = false;
        let m = authorization_server_metadata(&r, &urls);
        assert_eq!(
            m["pushed_authorization_request_endpoint"],
            "https://h/idp/patients/par"
        );
        assert_eq!(
            m["registration_endpoint"],
            "https://h/idp/patients/register"
        );
        assert!(
            !m["grant_types_supported"]
                .as_array()
                .unwrap()
                .contains(&json!("client_credentials"))
        );
    }
}
