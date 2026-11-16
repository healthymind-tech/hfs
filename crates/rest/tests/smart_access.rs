//! SMART access control end to end through the full app (auth, authz and the
//! SMART access layer) on SQLite: patient-compartment enforcement for
//! `patient/` scopes, SMART v2 granular scopes, type leakage through
//! `_include`/`_revinclude`/`$everything`, system-wide reads, write-protect
//! mode, RFC 6750 challenges and the administrator credential.
//!
//! No IdP runs here: a stub provider turns `Bearer scopes=<a+b>;patient=<id>`
//! into a principal with those scopes and launch patient.

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::{HeaderValue, StatusCode, header};
use axum_test::{TestResponse, TestServer};
use helios_audit::ExclusionFilter;
use helios_auth::{AuthConfig, AuthError, AuthMode, AuthProvider, Principal, ScopeSet};
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use helios_rest::{AuthMiddlewareState, ServerConfig};
use serde_json::{Value, json};

struct StubProvider;

#[async_trait]
impl AuthProvider for StubProvider {
    async fn authenticate(&self, authorization_header: &str) -> Result<Principal, AuthError> {
        let token = authorization_header
            .strip_prefix("Bearer ")
            .ok_or(AuthError::InvalidSignature)?;
        // A JWT-shaped stand-in for the system token (the admin bridge only
        // lets JWT-shaped bearers through to the IdP path).
        let token = if token == SYSTEM_JWT { SYSTEM } else { token };
        let mut scopes = String::new();
        let mut claims = serde_json::Map::new();
        for part in token.split(';') {
            match part.split_once('=') {
                Some(("scopes", s)) => scopes = s.replace('+', " "),
                Some(("patient", p)) => {
                    claims.insert("patient".into(), json!(p));
                }
                _ => return Err(AuthError::InvalidSignature),
            }
        }
        Ok(Principal {
            subject: "app".to_string(),
            issuer: "https://idp.example.com".to_string(),
            tenant_id: None,
            scopes: ScopeSet::parse(&scopes),
            jti: None,
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
            custom_claims: claims,
            launch_context: None,
        })
    }

    fn name(&self) -> &str {
        "stub"
    }
}

const SYSTEM: &str = "scopes=system/*.cruds";
const SYSTEM_JWT: &str = "eyJzeXN0ZW0.c3lzdGVt.c2ln";
const LAB: &str = "http://terminology.hl7.org/CodeSystem/observation-category";

fn admin_gate() -> helios_admin_auth::AdminGate {
    let settings = helios_admin_auth::AdminSettings::from_lookup("HFS", |k| match k {
        "HFS_ADMIN_PASSWORD" => Some("correct horse battery staple".to_string()),
        _ => None,
    });
    helios_admin_auth::AdminGate::configure(
        "HFS",
        settings,
        true,
        helios_admin_auth::CookieSpec {
            name: "hfs_admin",
            path: "/",
        },
    )
}

fn server(mode: AuthMode, admin: bool) -> TestServer {
    // The spec SearchParameters (compartment parameters such as
    // Observation.subject) load from the workspace `data/` directory.
    let backend = SqliteBackend::with_config(
        ":memory:",
        SqliteBackendConfig {
            data_dir: Some(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data")),
            ..Default::default()
        },
    )
    .expect("in-memory sqlite");
    backend.init_schema().expect("init schema");
    let auth_config = AuthConfig {
        enabled: true,
        mode,
        ..AuthConfig::default()
    };
    let auth = Arc::new(AuthMiddlewareState {
        provider: Arc::new(StubProvider),
        config: Arc::new(auth_config.clone()),
        audit_sink: Arc::new(helios_audit::sinks::NullSink),
        audit_source_observer: "Device/test".to_string(),
        audit_exclusion_filter: ExclusionFilter::new(vec![]),
        tenant_url_routing: false,
        sessions: None,
        admin: admin.then(admin_gate),
    });
    let app = helios_rest::create_app_with_auth(
        backend,
        ServerConfig::for_testing(),
        auth_config,
        Some(auth),
        None,
    );
    TestServer::new(app).expect("test server")
}

fn bearer(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("Bearer {token}")).unwrap()
}

async fn put(server: &TestServer, resource: Value) {
    let path = format!(
        "/{}/{}",
        resource["resourceType"].as_str().unwrap(),
        resource["id"].as_str().unwrap()
    );
    let r = server
        .put(&path)
        .add_header(header::AUTHORIZATION, bearer(SYSTEM_JWT))
        .add_header(header::CONTENT_TYPE, HeaderValue::from_static("application/fhir+json"))
        .json(&resource)
        .await;
    assert!(r.status_code().is_success(), "seed {path}: {}", r.text());
}

fn observation(id: &str, patient: &str, category: &str) -> Value {
    json!({
        "resourceType": "Observation", "id": id, "status": "final",
        "category": [{"coding": [{"system": LAB, "code": category}]}],
        "code": {"text": "x"},
        "subject": {"reference": format!("Patient/{patient}")},
        "performer": [{"reference": "Practitioner/pr1"}],
    })
}

/// p1 and p2; p1 has a lab (o1) and a vital (o3), p2 a lab (o2); both
/// observations name practitioner pr1; m1 is a Medication.
async fn seeded(mode: AuthMode) -> TestServer {
    let server = server(mode, false);
    seed(&server).await;
    server
}

async fn seed(server: &TestServer) {
    put(server, json!({"resourceType": "Patient", "id": "p1"})).await;
    put(server, json!({"resourceType": "Patient", "id": "p2"})).await;
    put(server, json!({"resourceType": "Practitioner", "id": "pr1"})).await;
    put(server, json!({"resourceType": "Medication", "id": "m1"})).await;
    put(server, observation("o1", "p1", "laboratory")).await;
    put(server, observation("o2", "p2", "laboratory")).await;
    put(server, observation("o3", "p1", "vital-signs")).await;
}

async fn get(server: &TestServer, path: &str, token: &str) -> TestResponse {
    server
        .get(path)
        .add_header(header::AUTHORIZATION, bearer(token))
        .await
}

/// `Type/id` of every entry resource in a Bundle response.
fn ids(response: &TestResponse) -> Vec<String> {
    let body: Value = serde_json::from_str(&response.text()).expect("json");
    let mut ids: Vec<String> = body["entry"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|e| {
            Some(format!(
                "{}/{}",
                e["resource"]["resourceType"].as_str()?,
                e["resource"]["id"].as_str()?
            ))
        })
        .collect();
    ids.sort();
    ids
}

const PATIENT_P1: &str = "scopes=patient/*.rs;patient=p1";

#[tokio::test]
async fn patient_search_is_confined_to_the_launch_patient() {
    let server = seeded(AuthMode::Full).await;
    let r = get(&server, "/Observation", PATIENT_P1).await;
    r.assert_status_ok();
    assert_eq!(ids(&r), vec!["Observation/o1", "Observation/o3"], "{}", r.text());

    let r = get(&server, "/Patient", PATIENT_P1).await;
    r.assert_status_ok();
    assert_eq!(ids(&r), vec!["Patient/p1"]);

    // The client's own criteria still apply, AND-ed with the compartment.
    let r = get(&server, "/Observation?category=vital-signs", PATIENT_P1).await;
    assert_eq!(ids(&r), vec!["Observation/o3"]);
}

#[tokio::test]
async fn patient_post_search_is_confined_too() {
    let server = seeded(AuthMode::Full).await;
    let r = server
        .post("/Observation/_search")
        .add_header(header::AUTHORIZATION, bearer(PATIENT_P1))
        .add_header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        )
        .text("category=laboratory")
        .await;
    r.assert_status_ok();
    assert_eq!(ids(&r), vec!["Observation/o1"]);
}

#[tokio::test]
#[ignore = "U-SMART: upstream #1618 handler checks refuse what the fork smart_access layer allows (non-compartment reads, instance history); reconcile before enabling auth"]
async fn patient_reads_outside_the_compartment_are_refused() {
    let server = seeded(AuthMode::Full).await;
    get(&server, "/Observation/o1", PATIENT_P1).await.assert_status_ok();
    get(&server, "/Patient/p1", PATIENT_P1).await.assert_status_ok();
    get(&server, "/Observation/o2", PATIENT_P1)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    get(&server, "/Patient/p2", PATIENT_P1)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    get(&server, "/Observation/o2/_history", PATIENT_P1)
        .await
        .assert_status_ok();
    let r = get(&server, "/Observation/o2/_history", PATIENT_P1).await;
    assert!(ids(&r).is_empty(), "history of another patient's resource leaks: {:?}", ids(&r));
    get(&server, "/Patient/p2/Observation", PATIENT_P1)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let r = get(&server, "/Patient/p1/Observation", PATIENT_P1).await;
    assert_eq!(ids(&r), vec!["Observation/o1", "Observation/o3"]);
}

#[tokio::test]
#[ignore = "U-SMART: upstream #1618 handler checks refuse what the fork smart_access layer allows (non-compartment reads, instance history); reconcile before enabling auth"]
async fn non_compartment_types_are_readable_but_includes_are_filtered() {
    let server = seeded(AuthMode::Full).await;
    get(&server, "/Medication/m1", PATIENT_P1).await.assert_status_ok();
    let r = get(
        &server,
        "/Practitioner?_revinclude=Observation:performer",
        PATIENT_P1,
    )
    .await;
    r.assert_status_ok();
    assert_eq!(
        ids(&r),
        vec!["Observation/o1", "Observation/o3", "Practitioner/pr1"],
        "another patient's Observation leaked through _revinclude"
    );
}

#[tokio::test]
async fn everything_is_limited_to_the_launch_patient() {
    let server = seeded(AuthMode::Full).await;
    get(&server, "/Patient/p2/$everything", PATIENT_P1)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // Type level is narrowed to the launch patient (upstream #1618).
    let r = get(&server, "/Patient/$everything", PATIENT_P1).await;
    r.assert_status_ok();
    let found = ids(&r);
    assert!(!found.iter().any(|id| id == "Observation/o2" || id == "Patient/p2"), "{found:?}");
    let r = get(&server, "/Patient/p1/$everything", PATIENT_P1).await;
    r.assert_status_ok();
    let found = ids(&r);
    assert!(found.contains(&"Observation/o1".to_string()), "{found:?}");
    assert!(!found.iter().any(|id| id == "Observation/o2" || id == "Patient/p2"), "{found:?}");
}

#[tokio::test]
async fn history_batch_and_conditional_writes_are_refused_for_patient_tokens() {
    let server = seeded(AuthMode::Full).await;
    let token = "scopes=patient/*.cruds;patient=p1";
    get(&server, "/_history", token).await.assert_status(StatusCode::FORBIDDEN);
    get(&server, "/Observation/_history", token)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .post("/")
        .add_header(header::AUTHORIZATION, bearer(token))
        .json(&json!({"resourceType": "Bundle", "type": "batch", "entry": [
            {"request": {"method": "GET", "url": "Observation/o2"}}
        ]}))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .delete("/Observation?subject=Patient/p2")
        .add_header(header::AUTHORIZATION, bearer(token))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn patient_writes_must_stay_inside_the_compartment() {
    let server = seeded(AuthMode::Full).await;
    let token = "scopes=patient/Observation.cud+patient/Observation.rs;patient=p1";
    let post = |body: Value| {
        server
            .post("/Observation")
            .add_header(header::AUTHORIZATION, bearer(token))
            .add_header(header::CONTENT_TYPE, HeaderValue::from_static("application/fhir+json"))
            .json(&body)
    };
    let mut other = observation("x", "p2", "laboratory");
    other.as_object_mut().unwrap().remove("id");
    post(other).await.assert_status(StatusCode::FORBIDDEN);
    let mut own = observation("x", "p1", "laboratory");
    own.as_object_mut().unwrap().remove("id");
    post(own).await.assert_status(StatusCode::CREATED);

    // Moving p2's observation to p1 is refused: the stored one is p2's.
    server
        .put("/Observation/o2")
        .add_header(header::AUTHORIZATION, bearer(token))
        .add_header(header::CONTENT_TYPE, HeaderValue::from_static("application/fhir+json"))
        .json(&observation("o2", "p1", "laboratory"))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // Moving p1's observation to p2 is refused: the new one is p2's.
    server
        .put("/Observation/o1")
        .add_header(header::AUTHORIZATION, bearer(token))
        .add_header(header::CONTENT_TYPE, HeaderValue::from_static("application/fhir+json"))
        .json(&observation("o1", "p2", "laboratory"))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .delete("/Observation/o2")
        .add_header(header::AUTHORIZATION, bearer(token))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .delete("/Observation/o3")
        .add_header(header::AUTHORIZATION, bearer(token))
        .await
        .assert_status(StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_patient_scope_without_a_launch_patient_reaches_nothing() {
    let server = seeded(AuthMode::Full).await;
    get(&server, "/Observation", "scopes=patient/*.rs")
        .await
        .assert_status(StatusCode::FORBIDDEN);
    get(&server, "/Observation/o1", "scopes=patient/*.rs")
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn granular_scopes_restrict_search_and_read() {
    let server = seeded(AuthMode::Full).await;
    let token = format!("scopes=patient/Observation.rs?category={LAB}|laboratory;patient=p1");
    let r = get(&server, "/Observation", &token).await;
    r.assert_status_ok();
    assert_eq!(ids(&r), vec!["Observation/o1"]);
    get(&server, "/Observation/o1", &token).await.assert_status_ok();
    get(&server, "/Observation/o3", &token)
        .await
        .assert_status(StatusCode::FORBIDDEN);

    // Two categories OR together.
    let token = format!(
        "scopes=user/Observation.rs?category={LAB}|laboratory+user/Observation.rs?category={LAB}|vital-signs"
    );
    let r = get(&server, "/Observation", &token).await;
    assert_eq!(
        ids(&r),
        vec!["Observation/o1", "Observation/o2", "Observation/o3"]
    );
}

#[tokio::test]
async fn includes_of_unscoped_types_are_dropped() {
    let server = seeded(AuthMode::Full).await;
    let token = "scopes=system/Patient.rs";
    let r = get(&server, "/Patient?_revinclude=Observation:subject", token).await;
    r.assert_status_ok();
    assert_eq!(ids(&r), vec!["Patient/p1", "Patient/p2"]);
    let r = get(&server, "/Patient/p1/$everything", token).await;
    r.assert_status_ok();
    assert_eq!(ids(&r), vec!["Patient/p1"]);
}

#[tokio::test]
async fn system_wide_reads_need_a_wildcard_read() {
    let server = seeded(AuthMode::Full).await;
    get(&server, "/_history", "scopes=system/Patient.rs")
        .await
        .assert_status(StatusCode::FORBIDDEN);
    get(&server, "/_history", "scopes=system/*.rs").await.assert_status_ok();
}

#[tokio::test]
async fn smart_v1_scopes_are_enforced() {
    let server = seeded(AuthMode::Full).await;
    let token = "scopes=user/Observation.read";
    get(&server, "/Observation", token).await.assert_status_ok();
    server
        .post("/Observation")
        .add_header(header::AUTHORIZATION, bearer(token))
        .json(&observation("y", "p1", "laboratory"))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let v1_patient = "scopes=patient/*.read;patient=p1";
    let r = get(&server, "/Observation", v1_patient).await;
    assert_eq!(ids(&r), vec!["Observation/o1", "Observation/o3"]);
}

#[tokio::test]
async fn challenges_follow_rfc_6750() {
    let server = seeded(AuthMode::Full).await;
    let r = server.get("/Patient").await;
    r.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(r.header(header::WWW_AUTHENTICATE), "Bearer realm=\"hfs\"");

    let r = get(&server, "/Patient", "garbage").await;
    r.assert_status(StatusCode::UNAUTHORIZED);
    let challenge = r.header(header::WWW_AUTHENTICATE);
    let challenge = challenge.to_str().unwrap();
    assert!(challenge.contains("error=\"invalid_token\""), "{challenge}");

    let r = get(&server, "/Observation", "scopes=system/Patient.rs").await;
    r.assert_status(StatusCode::FORBIDDEN);
    let challenge = r.header(header::WWW_AUTHENTICATE);
    assert!(
        challenge.to_str().unwrap().contains("error=\"insufficient_scope\""),
        "{challenge:?}"
    );
}

#[tokio::test]
async fn write_protect_mode_reads_anonymously_and_writes_with_a_token() {
    let server = seeded(AuthMode::WriteProtect).await;
    let r = server.get("/Observation").await;
    r.assert_status_ok();
    assert_eq!(ids(&r).len(), 3);
    server.get("/Patient/p1").await.assert_status_ok();
    server
        .post("/Patient/$validate")
        .json(&json!({"resourceType": "Patient"}))
        .await
        .assert_status_ok();

    let r = server
        .post("/Patient")
        .json(&json!({"resourceType": "Patient"}))
        .await;
    r.assert_status(StatusCode::UNAUTHORIZED);
    assert!(r.maybe_header(header::WWW_AUTHENTICATE).is_some());
    server
        .delete("/Patient/p1")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .post("/")
        .json(&json!({"resourceType": "Bundle", "type": "batch", "entry": []}))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);

    // A presented token is still validated and scope-checked.
    get(&server, "/Patient", "garbage")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    get(&server, "/Observation", PATIENT_P1).await.assert_status_ok();
    assert_eq!(
        ids(&get(&server, "/Observation", PATIENT_P1).await).len(),
        2
    );
    server
        .post("/Patient")
        .add_header(header::AUTHORIZATION, bearer(SYSTEM))
        .json(&json!({"resourceType": "Patient"}))
        .await
        .assert_status(StatusCode::CREATED);
}

#[tokio::test]
async fn the_administrator_credential_works_alongside_tokens() {
    let server = server(AuthMode::WriteProtect, true);
    seed(&server).await;
    let basic = |pw: &str| {
        use base64::Engine;
        HeaderValue::from_str(&format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("admin:{pw}"))
        ))
        .unwrap()
    };
    server
        .post("/Patient")
        .add_header(header::AUTHORIZATION, basic("correct horse battery staple"))
        .json(&json!({"resourceType": "Patient"}))
        .await
        .assert_status(StatusCode::CREATED);
    server
        .post("/Patient")
        .add_header(header::AUTHORIZATION, basic("wrong"))
        .json(&json!({"resourceType": "Patient"}))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    // A JWT-shaped bearer still goes to the IdP path.
    server
        .post("/Patient")
        .add_header(header::AUTHORIZATION, bearer(SYSTEM_JWT))
        .json(&json!({"resourceType": "Patient"}))
        .await
        .assert_status(StatusCode::CREATED);
}
