//! An endpoint bound to an internal realm while the built-in authorization
//! server is off (no realm source installed, `HFS_IDP=off`): every token is
//! answered 503 and nothing is served anonymously — closed, never open
//! (docs/design/authorization-server.md §5.2, D-13).

use std::sync::Arc;

use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum_test::TestServer;
use helios_auth::AuthConfig;
use helios_endpoints::model::{AuthMode, AuthProviderRef, EndpointAuth};
use helios_endpoints::{
    Actor, EndpointName, EndpointRegistry, EndpointSpec, EndpointStore, FhirRelease, ManagedBy,
    MemoryEndpointStore, ensure_default_endpoint,
};
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use helios_rest::endpoint::EndpointRuntime;
use helios_rest::{OperationsBundle, ServerConfig};

#[tokio::test]
async fn internal_endpoint_is_closed_while_the_idp_is_off() {
    assert!(helios_auth::internal_realms().is_none());
    let store = Arc::new(MemoryEndpointStore::new());
    ensure_default_endpoint(store.as_ref(), "default", FhirRelease::R4)
        .await
        .unwrap();
    for mode in [AuthMode::Full, AuthMode::WriteProtect] {
        let name = format!("off-{}", if mode == AuthMode::Full { "full" } else { "wp" });
        let mut spec = EndpointSpec::named(EndpointName::parse(&name).unwrap(), &name);
        spec.managed_by = ManagedBy::Env;
        spec.auth = Some(EndpointAuth {
            mode,
            provider: AuthProviderRef::Internal {
                realm: "clinic".into(),
            },
            ..EndpointAuth::default()
        });
        store.create(spec, &Actor::bootstrap()).await.unwrap();
    }
    let registry = Arc::new(EndpointRegistry::load(store.as_ref()).await.unwrap());
    let backend = Arc::new(
        SqliteBackend::with_config(":memory:", SqliteBackendConfig::default()).expect("sqlite"),
    );
    backend.init_schema().expect("schema");
    let app = helios_rest::create_app_with_auth_bulk_settings_and_ops(
        backend,
        ServerConfig::for_testing(),
        AuthConfig::default(),
        None,
        None,
        None,
        None,
        None,
        OperationsBundle {
            endpoints: Some(EndpointRuntime::new(
                registry,
                Arc::clone(&store) as Arc<dyn EndpointStore>,
            )),
            ..Default::default()
        },
    );
    let s = TestServer::new(app).unwrap();
    let auth = HeaderName::from_static("authorization");
    let tok = HeaderValue::from_static(
        "Bearer eyJhbGciOiJFUzI1NiIsInR5cCI6ImF0K2p3dCIsImtpZCI6ImsifQ.e30.c2ln",
    );
    let r = s
        .get("/off-full/Patient")
        .add_header(auth.clone(), tok.clone())
        .await;
    assert_eq!(
        r.status_code(),
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        r.text()
    );
    assert_eq!(
        s.get("/off-full/Patient").await.status_code(),
        StatusCode::UNAUTHORIZED
    );
    let r = s.get("/off-wp/Patient").add_header(auth, tok).await;
    assert_eq!(r.status_code(), StatusCode::SERVICE_UNAVAILABLE);
    // The SMART document advertises no authorization endpoints.
    let doc: serde_json::Value = s
        .get("/off-full/.well-known/smart-configuration")
        .await
        .json();
    assert!(doc.get("authorization_endpoint").is_none(), "{doc}");
}
