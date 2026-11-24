//! `/admin/idp/**` end to end over the memory store.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use helios_endpoints::{
    Actor, EndpointName, EndpointSpec, EndpointStore, MemoryEndpointStore, SecretKeyRing,
};
use helios_idp::admin::IdpAdmin;
use helios_idp::memory::MemoryStore;
use helios_idp::runtime::{ConventionalEndpointUrls, IdpRuntime};
use helios_idp_admin::api::{self, Authorize, OPENAPI, ROUTES};
use helios_idp_admin::binding::StoreBinder;
use helios_idp_admin::service::{self, IdpConsole};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use url::Url;

fn auth() -> Authorize {
    api::authorize_sync(|parts| {
        if parts.headers.get("x-test-admin").is_some() {
            Ok(Actor {
                subject: "local:admin".into(),
                display: None,
                via: "console-session".into(),
            })
        } else {
            Err((
                StatusCode::UNAUTHORIZED,
                "sign in as an administrator".into(),
            ))
        }
    })
}

async fn install() -> Arc<dyn EndpointStore> {
    let base = Url::parse("http://127.0.0.1:9").unwrap();
    let ring = Arc::new(SecretKeyRing::parse(&SecretKeyRing::generate_spec("t1")).unwrap());
    let rt = IdpRuntime::new(
        Arc::new(MemoryStore::new()),
        Some(ring),
        base.clone(),
        Arc::new(ConventionalEndpointUrls {
            public_base: base.to_string(),
        }),
    );
    let eps: Arc<dyn EndpointStore> = Arc::new(MemoryEndpointStore::default());
    let actor = Actor {
        subject: "t".into(),
        display: None,
        via: "t".into(),
    };
    eps.create(
        EndpointSpec::named(EndpointName::parse("clinic").unwrap(), "Clinic"),
        &actor,
    )
    .await
    .unwrap();
    let rt2 = rt.clone();
    let binder = StoreBinder {
        store: eps.clone(),
        server_base: format!("{base}fhir"),
        public_base: base,
        jwks_via: None,
        realms: Arc::new(move || rt2.realms().iter().map(|r| r.spec.clone()).collect()),
    };
    service::install(Arc::new(
        IdpConsole::new(IdpAdmin::new(rt), Some(Arc::new(binder))).with_type_provider(Some(
            Arc::new(|release: &str| {
                assert_eq!(release, "R4");
                [
                    "Patient",
                    "Observation",
                    "Condition",
                    "Claim",
                    "ValueSet",
                    "Basic",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect()
            }),
        )),
    ));
    eps
}

async fn call(
    method: &str,
    path: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value, Option<String>) {
    let app = api::router(Some(auth()));
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("x-test-admin", "1");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let req = match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let etag = resp
        .headers()
        .get("etag")
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        etag,
    )
}

fn realm_body(name: &str) -> Value {
    json!({
        "name": name, "displayName": "Clinic apps", "enabled": true,
        "userSource": {"kind": "local"}, "signingAlgs": ["RS256", "ES256"],
    })
}

#[tokio::test]
async fn admin_api_end_to_end() {
    let eps = install().await;

    // Refused without an administrator.
    let resp = api::router(Some(auth()))
        .oneshot(
            Request::get("/admin/idp/realms")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = api::router(None)
        .oneshot(
            Request::get("/admin/idp/realms")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Realms.
    let (s, v, _) = call("GET", "/admin/idp/realms", None, &[]).await;
    assert_eq!(
        (s, v["realms"].as_array().unwrap().len()),
        (StatusCode::OK, 0)
    );
    let (s, v, _) = call("POST", "/admin/idp/realms", Some(realm_body("clinic")), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["issuer"], "http://127.0.0.1:9/idp/clinic");
    let mut bad = realm_body("clinic2");
    bad["lifetimes"] = json!({"accessToken": 99999});
    let (s, v, _) = call("POST", "/admin/idp/realms", Some(bad), &[]).await;
    assert_eq!(
        (s, v["field"].as_str()),
        (StatusCode::BAD_REQUEST, Some("lifetimes.accessToken")),
        "{v}"
    );
    let (s, v, etag) = call("GET", "/admin/idp/realms/clinic", None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    let etag = etag.unwrap();
    let mut spec = v["spec"].clone();
    spec["displayName"] = json!("Clinic (renamed)");
    let (s, _, _) = call(
        "PUT",
        "/admin/idp/realms/clinic",
        Some(spec.clone()),
        &[("if-match", "\"999\"")],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, v, _) = call(
        "PUT",
        "/admin/idp/realms/clinic",
        Some(spec),
        &[("if-match", &etag)],
    )
    .await;
    assert_eq!(
        (s, v["spec"]["displayName"].as_str()),
        (StatusCode::OK, Some("Clinic (renamed)"))
    );
    let (s, _, _) = call("GET", "/admin/idp/realms/nope", None, &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Keys.
    let (_, v, _) = call("GET", "/admin/idp/realms/clinic/keys", None, &[]).await;
    assert_eq!(v["keys"].as_array().unwrap().len(), 2);
    assert!(v["keys"][0]["publicJwk"].get("d").is_none());
    let (s, v, _) = call("POST", "/admin/idp/realms/clinic/$rotate-keys", None, &[]).await;
    assert_eq!(
        (s, v["created"].as_array().unwrap().len()),
        (StatusCode::OK, 2)
    );
    let (_, v, _) = call(
        "POST",
        "/admin/idp/realms/clinic/$emergency-rotate",
        None,
        &[],
    )
    .await;
    let active = v["keys"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|k| k["status"] == "active")
        .count();
    assert_eq!(active, 2);

    // Clients: secret returned once, write-only afterwards.
    let client = json!({
        "clientId": "portal", "name": "Portal", "auth": {"method": "client_secret"},
        "grantTypes": ["authorization_code"], "redirectUris": ["https://portal.example.org/cb"],
        "launchTypes": ["standalone"], "scopes": {"allowed": ["openid", "patient/*.rs"]},
    });
    let (s, v, _) = call(
        "POST",
        "/admin/idp/realms/clinic/clients",
        Some(client.clone()),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let secret = v["clientSecret"].as_str().unwrap().to_string();
    assert!(secret.len() >= 32);
    assert_eq!(v["spec"]["auth"]["secret_hash"], json!({"set": true}));
    let (_, v, _) = call("GET", "/admin/idp/realms/clinic/clients/portal", None, &[]).await;
    assert!(!v.to_string().contains("argon2"), "{v}");
    let (_, v, _) = call(
        "POST",
        "/admin/idp/realms/clinic/clients/portal/$reset-secret",
        None,
        &[],
    )
    .await;
    assert_ne!(v["clientSecret"].as_str().unwrap(), secret);
    let mut wrong = client.clone();
    wrong["redirectUris"] = json!(["http://portal.example.org/cb"]);
    wrong["clientId"] = json!("x2");
    let (s, v, _) = call("POST", "/admin/idp/realms/clinic/clients", Some(wrong), &[]).await;
    assert_eq!(
        (s, v["field"].as_str()),
        (StatusCode::BAD_REQUEST, Some("redirectUris")),
        "{v}"
    );
    let mut upd = client.clone();
    upd["disabled"] = json!(true);
    let (s, v, _) = call(
        "PUT",
        "/admin/idp/realms/clinic/clients/portal",
        Some(upd),
        &[],
    )
    .await;
    assert_eq!(
        (s, &v["spec"]["disabled"]),
        (StatusCode::OK, &json!(true)),
        "{v}"
    );

    // Users.
    let (s, v, _) = call("POST", "/admin/idp/realms/clinic/users", Some(json!({
        "id": "", "username": "alice", "kind": "patient", "displayName": "Alice", "email": null, "emailVerified": false,
        "links": [{"endpoint": "clinic", "reference": "Patient/p1", "isFhirUser": true}], "externalSubject": null, "disabled": false,
        "password": "a sufficiently long password",
    })), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let uid = v["id"].as_str().unwrap().to_string();
    let (s, v, _) = call(
        "POST",
        &format!("/admin/idp/realms/clinic/users/{uid}/$set-password"),
        Some(json!({"password": "short"})),
        &[],
    )
    .await;
    assert_eq!(
        (s, v["field"].as_str()),
        (StatusCode::BAD_REQUEST, Some("password"))
    );
    let (s, _, _) = call(
        "POST",
        &format!("/admin/idp/realms/clinic/users/{uid}/$set-password"),
        Some(json!({"password": "another long password!"})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, v, _) = call("GET", "/admin/idp/realms/clinic/users", None, &[]).await;
    assert_eq!(v["users"].as_array().unwrap().len(), 1);
    assert!(!v.to_string().contains("argon2"));
    let (_, v, _) = call("GET", "/admin/idp/realms/clinic/sessions", None, &[]).await;
    assert_eq!(v["sessions"], json!([]));
    let (_, v, _) = call("GET", "/admin/idp/realms/clinic/grants", None, &[]).await;
    assert_eq!(v["grants"], json!([]));

    // Binding.
    let (s, v, _) = call(
        "GET",
        "/admin/idp/realms/clinic/endpoints/clinic",
        None,
        &[],
    )
    .await;
    assert_eq!(
        (s, v["before"].as_str()),
        (StatusCode::OK, Some("inherit")),
        "{v}"
    );
    let (s, _, _) = call(
        "PUT",
        "/admin/idp/realms/clinic/endpoints/clinic",
        Some(json!({"mode": "write-protect"})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let auth = eps.get("clinic").await.unwrap().unwrap().spec.auth.unwrap();
    assert_eq!(
        auth.issuer.as_deref(),
        Some("http://127.0.0.1:9/idp/clinic")
    );
    // Launch on a bound endpoint.
    let (s, v, _) = call(
        "POST",
        "/admin/idp/realms/clinic/launches",
        Some(json!({"endpoint": "clinic", "context": {"patient": "p1"}})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert!(v["launch"].as_str().unwrap().starts_with("hmlh_"));

    // Scope catalog: the bound endpoint's types, grouped; presets.
    let (s, v, _) = call("GET", "/admin/idp/scopes/catalog?realm=clinic", None, &[]).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["source"], "bound");
    assert_eq!(v["endpoints"][0]["name"], "clinic");
    assert_eq!(v["endpoints"][0]["bound"], true);
    assert_eq!(v["endpoints"][0]["authMode"], "write-protect");
    assert_eq!(v["endpoints"][0]["resourceTypes"], 6);
    assert_eq!(
        v["groups"],
        json!([
            {"id": "clinical", "types": ["Condition", "Observation"]},
            {"id": "administrative", "types": ["Patient"]},
            {"id": "financial", "types": ["Claim"]},
            {"id": "conformance", "types": ["ValueSet"]},
            {"id": "other", "types": ["Basic"]},
        ])
    );
    let presets: Vec<&str> = v["presets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        presets,
        [
            "patient-read",
            "clinician-read",
            "clinician-write",
            "ehr-launch",
            "backend-export",
            "identity"
        ]
    );
    assert_eq!(v["nonResource"].as_array().unwrap().len(), 8);
    let (s, v, _) = call("GET", "/admin/idp/scopes/catalog", None, &[]).await;
    assert_eq!((s, v["source"].as_str()), (StatusCode::OK, Some("server")));
    let (s, _, _) = call("GET", "/admin/idp/scopes/catalog?realm=nope", None, &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // Analysis of a registered app (standalone, confidential, no client_credentials).
    let (s, v, _) = call(
        "POST",
        "/admin/idp/realms/clinic/scopes/$analyze",
        Some(json!({"clientId": "portal", "scopes": ["openid", "patient/*.rs", "patient/Observation.rs", "system/*.rs", "patient/observation.rs"]})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["invalid"][0]["scope"], "patient/observation.rs");
    assert_eq!(v["invalid"][0]["code"], "resource-type");
    assert_eq!(v["redundant"][0]["scope"], "patient/Observation.rs");
    assert_eq!(v["redundant"][0]["covered_by"], "patient/*.rs");
    let codes: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap())
        .collect();
    assert_eq!(
        codes,
        [
            "system-no-cc",
            "standalone-no-launch-patient",
            "write-protect-reads"
        ],
        "{v}"
    );
    assert!(
        v["findings"][2]["message"]
            .as_str()
            .unwrap()
            .contains("clinic")
    );
    let (s, v, _) = call(
        "POST",
        "/admin/idp/realms/clinic/scopes/$analyze",
        Some(json!({"shape": {"public": true, "authorizationCode": true, "standalone": true}, "scopes": ["system/*.rs"]})),
        &[],
    )
    .await;
    assert_eq!(v["findings"][0]["code"], "system-public", "{s} {v}");
    let (s, v, _) = call(
        "POST",
        "/admin/idp/realms/clinic/scopes/$analyze",
        Some(json!({"scopes": []})),
        &[],
    )
    .await;
    assert_eq!(
        (s, v["field"].as_str()),
        (StatusCode::BAD_REQUEST, Some("clientId"))
    );
    // Invalid scopes are refused on save.
    let mut bad_scope = client.clone();
    bad_scope["clientId"] = json!("x3");
    bad_scope["scopes"] = json!({"allowed": ["openid", "Patient/*.rs"]});
    let (s, v, _) = call(
        "POST",
        "/admin/idp/realms/clinic/clients",
        Some(bad_scope),
        &[],
    )
    .await;
    assert_eq!(
        (s, v["field"].as_str()),
        (StatusCode::BAD_REQUEST, Some("scopes")),
        "{v}"
    );

    // Diagnostic (preflight only; the disabled client is pinpointed).
    let (s, v, _) = call(
        "POST",
        "/admin/idp/realms/clinic/$diagnose?preflight=only",
        Some(
            json!({"app": {"clientId": "portal", "redirectUri": "https://portal.example.org/cb"}}),
        ),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let checks: Vec<&Value> = v["steps"][0]["checks"].as_array().unwrap().iter().collect();
    let dis = checks
        .iter()
        .find(|c| c["code"] == "app-disabled")
        .expect("app-disabled");
    assert_eq!(dis["field"], "client.portal.disabled");

    // Delete client, realm (unbinds first).
    let (s, _, _) = call(
        "DELETE",
        "/admin/idp/realms/clinic/clients/portal",
        None,
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _, _) = call(
        "DELETE",
        "/admin/idp/realms/clinic/clients/portal",
        None,
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = call("DELETE", "/admin/idp/realms/clinic", None, &[]).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(
        eps.get("clinic")
            .await
            .unwrap()
            .unwrap()
            .spec
            .auth
            .is_none()
    );
    service::uninstall();
    let (s, _, _) = call("GET", "/admin/idp/realms", None, &[]).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
}

#[test]
fn openapi_matches_routes() {
    let doc: Value = serde_json::from_str(OPENAPI).unwrap();
    let paths = doc["paths"].as_object().unwrap();
    let mut documented = vec![];
    for (p, ops) in paths {
        for m in ops.as_object().unwrap().keys() {
            documented.push((m.to_uppercase(), p.clone()));
        }
    }
    let mut routes: Vec<(String, String)> = ROUTES
        .iter()
        .map(|(m, p)| (m.to_string(), p.to_string()))
        .collect();
    documented.sort();
    routes.sort();
    assert_eq!(documented, routes);
}
