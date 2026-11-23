//! Conformance test stack for the protocol layer (Inferno SMART App Launch,
//! OpenID conformance). NOT a production component.
//!
//! Serves on `--port` (default 19601):
//! * the real `helios-idp-server` router for realm `clinic`
//!   (issuer `{base}/idp/clinic`), on the memory store, keys sealed with a
//!   throw-away key ring;
//! * a stub FHIR endpoint `{base}/fhir/clinic` (smart-configuration,
//!   CapabilityStatement with oauth-uris, a few resources) that accepts
//!   only active access tokens of the realm whose `aud` is the endpoint;
//! * an **auto-approve interaction driver** at `/idp/clinic/login` standing
//!   in for the hosted pages of `helios-idp-pages` (package I4): it signs in
//!   `alice` (Patient p1) for standalone launches and `bob` (Practitioner)
//!   for EHR launches, approves every requested scope, picks patient `p1`
//!   and encounter `e1`, and finishes through the real
//!   `AuthorizationCompleter`. Everything after the browser leaves
//!   `/authorize` is therefore the production code path.
//! * `POST /dev/launch` → `{launch, iss}` (EHR launch handle, as the console
//!   launcher would create).
//!
//! Clients (redirect URIs from `INFERNO_BASE`, default
//! `http://localhost:19604`): `inferno-public` (public), `inferno-conf`
//! (secret `inferno-secret-…`, printed), `inferno-asym` and
//! `inferno-backend` (private_key_jwt, Inferno's JWKS by URL).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use helios_endpoints::{OutboundPolicy, SecretKeyRing};
use helios_idp::admin::{IdpAdmin, LaunchRequest};
use helios_idp::client::{ClientAuth, ClientSpec, GrantType, LaunchType};
use helios_idp::interaction::Decision;
use helios_idp::launch::LaunchContext;
use helios_idp::memory::MemoryStore;
use helios_idp::realm::{RealmName, RealmSpec};
use helios_idp::runtime::{ConventionalEndpointUrls, IdpRuntime};
use helios_idp::scope::ScopePolicy;
use helios_idp::token::{HandleKind, handle_hash};
use helios_idp::user::{FhirLink, RealmUser, UserKind};
use helios_idp_server::{IdpServer, IdpServerOptions};
use serde_json::{Value, json};

#[derive(Clone)]
struct App {
    server: IdpServer,
    rt: Arc<IdpRuntime>,
    admin: IdpAdmin,
    realm_id: String,
    base: String,
}

fn policy(s: &str) -> ScopePolicy {
    ScopePolicy {
        allowed: s.split(' ').map(str::to_string).collect(),
        accept_v1: true,
    }
}

const USER_SCOPES: &str = "openid fhirUser profile email launch launch/patient launch/encounter \
    offline_access online_access patient/*.cruds user/*.cruds patient/*.read patient/*.write patient/*.* \
    user/*.read user/*.write user/*.* \
    patient/Patient.rs patient/Patient.read patient/Observation.rs patient/Encounter.rs patient/Condition.rs \
    patient/Observation.rs?category=laboratory patient/Observation.rs?category=social-history \
    patient/Condition.rs?category=problem-list-item patient/Condition.rs?category=encounter-diagnosis \
    user/Patient.rs user/Observation.rs user/Encounter.rs user/Practitioner.rs introspect";

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args()
        .skip_while(|a| a != "--port")
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(19601);
    let base = std::env::var("STACK_BASE").unwrap_or(format!("http://localhost:{port}"));
    let inferno = std::env::var("INFERNO_BASE").unwrap_or("http://localhost:19604".into());
    let store = Arc::new(MemoryStore::new());
    let ring = Arc::new(SecretKeyRing::parse(&SecretKeyRing::generate_spec("conf")).unwrap());
    let rt = IdpRuntime::new(
        store,
        Some(ring),
        url::Url::parse(&base).unwrap(),
        Arc::new(ConventionalEndpointUrls {
            public_base: base.clone(),
        }),
    );
    rt.set_event_sink(Arc::new(helios_idp::events::TracingEventSink));
    let admin = IdpAdmin::new(rt.clone());
    let mut spec = RealmSpec::new(RealmName::parse("clinic").unwrap(), "Clinic");
    spec.endpoints = vec!["clinic".into()];
    let realm = admin.create_realm(spec).await.unwrap();
    let rid = realm.id.clone();

    let redirects = vec![
        format!("{inferno}/custom/smart_stu2_2/redirect"),
        format!("{inferno}/custom/smart_stu2/redirect"),
        format!("{inferno}/custom/smart/redirect"),
        "https://localhost.emobix.co.uk:8443/test/a/hfs-idp/callback".to_string(),
        "https://www.certification.openid.net/test/a/hfs-idp/callback".to_string(),
    ];
    let mk = |id: &str, auth: ClientAuth, grants: Vec<GrantType>, scopes: &str| ClientSpec {
        client_id: id.into(),
        name: id.to_string(),
        auth,
        grant_types: grants,
        redirect_uris: redirects.clone(),
        post_logout_redirect_uris: vec![format!("{inferno}/")],
        launch_types: vec![LaunchType::Standalone, LaunchType::Ehr],
        scopes: policy(scopes),
        trusted: false,
        require_dpop: false,
        require_par: false,
        backchannel_logout_uri: None,
        access_token_ttl_secs: None,
        disabled: false,
    };
    let code_refresh = vec![GrantType::AuthorizationCode, GrantType::RefreshToken];
    admin
        .create_client(
            &rid,
            mk(
                "inferno-public",
                ClientAuth::None,
                code_refresh.clone(),
                USER_SCOPES,
            ),
        )
        .await
        .unwrap();
    // Fixed secret from CONF_SECRET (the Inferno inputs need it up front).
    let secret_hash = std::env::var("CONF_SECRET")
        .map(|s| helios_idp::password::hash(&s))
        .unwrap_or_default();
    let conf = admin
        .create_client(
            &rid,
            mk(
                "inferno-conf",
                ClientAuth::ClientSecret {
                    secret_hash: secret_hash.clone(),
                },
                code_refresh.clone(),
                USER_SCOPES,
            ),
        )
        .await
        .unwrap();
    // Resource server that introspects (Inferno's token introspection group).
    let mut rs = mk(
        "inferno-rs",
        ClientAuth::ClientSecret { secret_hash },
        vec![],
        "",
    );
    rs.redirect_uris.clear();
    rs.launch_types.clear();
    admin.create_client(&rid, rs).await.unwrap();
    let jwks_uri = std::env::var("INFERNO_JWKS").unwrap_or(format!(
        "{inferno}/custom/smart_stu2_2/.well-known/jwks.json"
    ));
    // http/localhost JWKS only because the test stack allow-lists it below.
    let pkjwt = |uri: &str| ClientAuth::PrivateKeyJwt {
        jwks_uri: Some(uri.to_string()),
        jwks: None,
        algs: vec![],
    };
    let mut asym = mk(
        "inferno-asym",
        pkjwt("https://placeholder.invalid/jwks"),
        code_refresh,
        USER_SCOPES,
    );
    asym.auth = pkjwt("https://placeholder.invalid/jwks");
    admin.create_client(&rid, asym).await.unwrap();
    let mut backend = mk(
        "inferno-backend",
        pkjwt("https://placeholder.invalid/jwks"),
        vec![GrantType::ClientCredentials],
        "system/*.rs system/*.read system/*.cruds system/Patient.rs system/Observation.rs introspect",
    );
    backend.launch_types = vec![LaunchType::Backend];
    backend.redirect_uris.clear();
    admin.create_client(&rid, backend).await.unwrap();
    // Point the asymmetric clients at Inferno's JWKS (ClientSpec::validate
    // insists on https; the test stack writes the http URL to the store).
    for id in ["inferno-asym", "inferno-backend"] {
        let mut rec = rt
            .store()
            .get_client_record(&rid, id)
            .await
            .unwrap()
            .unwrap();
        rec.spec.auth = pkjwt(&jwks_uri);
        let r = rec.revision;
        rt.store().put_client(&rid, rec, Some(r)).await.unwrap();
    }
    rt.reload().await.unwrap();
    for (id, kind, link) in [
        ("alice", UserKind::Patient, "Patient/p1"),
        ("bob", UserKind::Practitioner, "Practitioner/dr1"),
    ] {
        admin
            .create_user(
                &rid,
                RealmUser {
                    id: id.into(),
                    username: id.into(),
                    kind,
                    display_name: Some(format!("{id} example")),
                    email: Some(format!("{id}@example.org")),
                    email_verified: true,
                    links: vec![FhirLink {
                        endpoint: "clinic".into(),
                        reference: link.into(),
                        is_fhir_user: true,
                        accessible_patients: vec![],
                    }],
                    external_subject: None,
                    disabled: false,
                },
                Some("correct horse battery staple"),
                false,
            )
            .await
            .unwrap();
    }
    println!(
        "issuer {base}/idp/clinic\nfhir {base}/fhir/clinic\ninferno-conf secret generated: {}",
        conf.client_secret.is_some()
    );
    let server = IdpServer::new(
        rt.clone(),
        IdpServerOptions {
            outbound: Arc::new(OutboundPolicy::parse("localhost,127.0.0.1")),
            ..Default::default()
        },
    );
    let app = App {
        server: server.clone(),
        rt: rt.clone(),
        admin,
        realm_id: rid,
        base: base.clone(),
    };
    let router = Router::new()
        .route("/idp/{realm}/login", get(auto_login))
        .route("/dev/launch", post(dev_launch))
        .route(
            "/fhir/clinic/.well-known/smart-configuration",
            get(smart_config),
        )
        .route("/fhir/clinic/metadata", get(metadata))
        .route("/fhir/clinic/{rtype}/{id}", get(read))
        .route("/fhir/clinic/{rtype}", get(search))
        .layer(axum::middleware::map_response(cors))
        .with_state(app)
        .merge(server.router());
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let l = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(
        l,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}

/// CORS on the stub FHIR endpoint (SMART: smart-configuration and the
/// CapabilityStatement must be readable by browser apps).
async fn cors(mut r: Response) -> Response {
    r.headers_mut().insert(
        "access-control-allow-origin",
        axum::http::HeaderValue::from_static("*"),
    );
    r
}

#[derive(serde::Deserialize)]
struct Ix {
    ix: String,
}

/// Test-only stand-in for the hosted pages: approves everything.
async fn auto_login(
    State(a): State<App>,
    Path(realm): Path<String>,
    Query(q): Query<Ix>,
) -> Response {
    let Some(rt) = a.rt.realm(&realm) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(hash) = handle_hash(HandleKind::Interaction, &q.ix) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(p) =
        a.rt.store()
            .get_interaction(&rt.id, &hash, chrono::Utc::now())
            .await
            .unwrap()
    else {
        return (StatusCode::BAD_REQUEST, "expired").into_response();
    };
    let subject = if p.ehr_launch { "bob" } else { "alice" };
    let d = Decision {
        subject: subject.into(),
        sid: Some(format!("sid-{}", helios_idp::util::random_b64(8))),
        session_id_hash: None,
        auth_time: chrono::Utc::now(),
        amr: vec!["pwd".into()],
        acr: None,
        granted_scopes: p.scopes.clone(),
        context: LaunchContext {
            patient: Some("p1".into()),
            encounter: Some("e1".into()),
            ..Default::default()
        },
        fhir_user: None,
    };
    match a.server.completer().complete(&realm, &q.ix, d).await {
        Ok(resp) => match resp.mode {
            helios_idp::interaction::ResponseMode::Query => {
                Redirect::to(&resp.redirect_url()).into_response()
            }
            helios_idp::interaction::ResponseMode::FormPost => {
                let n = helios_idp::util::random_b64(16);
                (
                    [
                        (header::CONTENT_TYPE, "text/html".to_string()),
                        (header::CONTENT_SECURITY_POLICY, resp.form_post_csp(&n)),
                    ],
                    resp.form_post_html(&n),
                )
                    .into_response()
            }
        },
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct LaunchQ {
    client_id: Option<String>,
}

async fn dev_launch(State(a): State<App>, Query(q): Query<LaunchQ>) -> Response {
    let l = a
        .admin
        .create_launch(
            &a.realm_id,
            LaunchRequest {
                client_id: q.client_id,
                endpoint: "clinic".into(),
                launched_by: Some("console:bob".into()),
                context: LaunchContext {
                    patient: Some("p1".into()),
                    encounter: Some("e1".into()),
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();
    Json(json!({"launch": l.launch, "iss": l.iss})).into_response()
}

async fn smart_config(State(a): State<App>) -> Response {
    let rt = a.rt.realm("clinic").unwrap();
    Json(helios_idp_server::smart_configuration(&rt)).into_response()
}

async fn metadata(State(a): State<App>) -> Response {
    let iss = format!("{}/idp/clinic", a.base);
    (
        [(header::CONTENT_TYPE, "application/fhir+json")],
        Json(json!({
            "resourceType": "CapabilityStatement",
            "status": "active",
            "date": "2026-11-23",
            "kind": "instance",
            "fhirVersion": "4.0.1",
            "format": ["json"],
            "rest": [{
                "mode": "server",
                "security": {
                    "service": [{"coding": [{"system": "http://terminology.hl7.org/CodeSystem/restful-security-service", "code": "SMART-on-FHIR"}]}],
                    "extension": [{
                        "url": "http://fhir-registry.smarthealthit.org/StructureDefinition/oauth-uris",
                        "extension": [
                            {"url": "authorize", "valueUri": format!("{iss}/authorize")},
                            {"url": "token", "valueUri": format!("{iss}/token")},
                            {"url": "introspect", "valueUri": format!("{iss}/introspect")},
                            {"url": "revoke", "valueUri": format!("{iss}/revoke")},
                            {"url": "manage", "valueUri": format!("{iss}/account/apps")}
                        ]
                    }]
                },
                "resource": [
                    {"type": "Patient", "interaction": [{"code": "read"}, {"code": "search-type"}]},
                    {"type": "Practitioner", "interaction": [{"code": "read"}]},
                    {"type": "Encounter", "interaction": [{"code": "read"}]}
                ]
            }]
        })),
    )
        .into_response()
}

async fn authorized(a: &App, headers: &HeaderMap) -> Option<serde_json::Map<String, Value>> {
    let tok = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    let rt = a.rt.realm("clinic")?;
    let v = rt.verify_own(tok, Some("at+jwt")).ok()?;
    let exp = v.claims.get("exp")?.as_i64()?;
    if exp <= chrono::Utc::now().timestamp() {
        return None;
    }
    let aud_ok = v.claims.get("aud").is_some_and(|aud| {
        let want = format!("{}/fhir/clinic", a.base);
        match aud {
            Value::String(s) => *s == want,
            Value::Array(xs) => xs.iter().any(|x| x == &Value::String(want.clone())),
            _ => false,
        }
    });
    let gid = v.claims.get("gid")?.as_str()?;
    if !aud_ok || rt.grant_revoked(gid) {
        return None;
    }
    let g = a.rt.store().get_grant(&rt.id, gid).await.ok()??;
    g.revoked_at.is_none().then_some(v.claims)
}

fn resource(rtype: &str, id: &str) -> Option<Value> {
    Some(match (rtype, id) {
        ("Patient", "p1") => {
            json!({"resourceType":"Patient","id":"p1","name":[{"family":"Example","given":["Alice"]}],"gender":"female","birthDate":"1980-01-01"})
        }
        ("Practitioner", "dr1") => {
            json!({"resourceType":"Practitioner","id":"dr1","name":[{"family":"Example","given":["Bob"]}]})
        }
        ("Encounter", "e1") => {
            json!({"resourceType":"Encounter","id":"e1","status":"finished","class":{"system":"http://terminology.hl7.org/CodeSystem/v3-ActCode","code":"AMB"},"subject":{"reference":"Patient/p1"}})
        }
        _ => return None,
    })
}

async fn read(
    State(a): State<App>,
    headers: HeaderMap,
    Path((rtype, id)): Path<(String, String)>,
) -> Response {
    if authorized(&a, &headers).await.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer error=\"invalid_token\"")],
        )
            .into_response();
    }
    match resource(&rtype, &id) {
        Some(r) => ([(header::CONTENT_TYPE, "application/fhir+json")], Json(r)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn search(State(a): State<App>, headers: HeaderMap, Path(rtype): Path<String>) -> Response {
    if authorized(&a, &headers).await.is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let entries: Vec<Value> = match rtype.as_str() {
        "Patient" => vec![json!({"resource": resource("Patient", "p1").unwrap()})],
        _ => vec![],
    };
    (
        [(header::CONTENT_TYPE, "application/fhir+json")],
        Json(json!({"resourceType":"Bundle","type":"searchset","total":entries.len(),"entry":entries})),
    )
        .into_response()
}
