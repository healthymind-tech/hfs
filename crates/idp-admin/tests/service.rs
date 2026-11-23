//! The console service over IdP A's admin service with the memory store:
//! wizard drafts, endpoint binding through the endpoint store, fixtures
//! created and removed by the diagnostic.

use std::sync::Arc;

use helios_endpoints::{
    Actor, EndpointName, EndpointSpec, EndpointStore, MemoryEndpointStore, SecretKeyRing,
};
use helios_idp::admin::IdpAdmin;
use helios_idp::memory::MemoryStore;
use helios_idp::runtime::{ConventionalEndpointUrls, IdpRuntime};
use helios_idp_admin::binding::{BindMode, StoreBinder};
use helios_idp_admin::diagnostic::EndpointProvider;
use helios_idp_admin::service::{DIAGNOSTIC_PREFIX, IdpConsole, Template, WizardState};
use url::Url;

fn actor() -> Actor {
    Actor {
        subject: "local:test".into(),
        display: None,
        via: "console-session".into(),
    }
}

async fn console(with_ring: bool) -> (Arc<IdpConsole>, Arc<dyn EndpointStore>) {
    let base = Url::parse("http://127.0.0.1:9").unwrap();
    let store = Arc::new(MemoryStore::new());
    let ring = with_ring
        .then(|| Arc::new(SecretKeyRing::parse(&SecretKeyRing::generate_spec("t1")).unwrap()));
    let rt = IdpRuntime::new(
        store,
        ring,
        base.clone(),
        Arc::new(ConventionalEndpointUrls {
            public_base: base.to_string(),
        }),
    );
    let admin = IdpAdmin::new(rt.clone());
    let eps: Arc<dyn EndpointStore> = Arc::new(MemoryEndpointStore::default());
    let spec = EndpointSpec::named(EndpointName::parse("clinic").unwrap(), "Clinic");
    eps.create(spec, &actor()).await.unwrap();
    let rt2 = rt.clone();
    let binder = StoreBinder {
        store: eps.clone(),
        server_base: format!("{}fhir", base),
        public_base: base.clone(),
        jwks_via: None,
        realms: Arc::new(move || rt2.realms().iter().map(|r| r.spec.clone()).collect()),
    };
    (
        Arc::new(IdpConsole::new(admin, Some(Arc::new(binder)))),
        eps,
    )
}

#[tokio::test]
async fn wizard_draft_lifecycle() {
    let (c, _) = console(true).await;
    // Field-anchored validation.
    let e = c
        .create_draft("Bad Name", "X", Template::Patient)
        .await
        .unwrap_err();
    assert_eq!(e.field.as_deref(), Some("name"));
    let e = c
        .create_draft("ok", " ", Template::Patient)
        .await
        .unwrap_err();
    assert_eq!(e.field.as_deref(), Some("displayName"));

    let r = c
        .create_draft("patients", "Patient apps", Template::Patient)
        .await
        .unwrap();
    assert!(!r.spec.enabled);
    let w = WizardState::of(&r.spec).unwrap();
    assert_eq!(w.step, 2);
    assert_eq!(w.template, Some(Template::Patient));
    assert!(!r.spec.features.backend_services);
    // Keys generated with the draft.
    assert_eq!(c.admin.list_keys(&r.id).await.unwrap().len(), 2);
    // Duplicate name.
    assert_eq!(
        c.create_draft("patients", "P", Template::Patient)
            .await
            .unwrap_err()
            .field
            .as_deref(),
        Some("name")
    );
    // A step saves server-side.
    let r = c
        .save_step("patients", 4, |s, w| {
            s.security.password_min_length = 14;
            w.endpoints = vec!["clinic".into()];
            Ok(())
        })
        .await
        .unwrap();
    let w = WizardState::of(&r.spec).unwrap();
    assert_eq!(
        (w.step, w.endpoints.clone()),
        (4, vec!["clinic".to_string()])
    );
    assert_eq!(
        c.summaries().await.unwrap()[0]
            .draft
            .as_ref()
            .map(|d| d.step),
        Some(4)
    );
    c.discard_draft("patients").await.unwrap();
    assert!(c.realms().await.unwrap().is_empty());
}

#[tokio::test]
async fn no_key_ring_is_explained() {
    let (c, _) = console(false).await;
    let e = c
        .create_draft("p", "P", Template::Patient)
        .await
        .unwrap_err();
    assert_eq!(e.status, 409);
    assert!(e.message.contains("HFS_SECRET_KEYS"));
}

#[tokio::test]
async fn binding_switches_the_endpoint_and_unbinding_restores_inherit() {
    let (c, eps) = console(true).await;
    c.create_draft("clin", "Clinicians", Template::Clinician)
        .await
        .unwrap();
    let d = c.bind_diff("clin", "clinic", BindMode::Keep).await.unwrap();
    assert_eq!(d.before, "inherit");
    assert_eq!(d.mode_after, "full");
    c.bind("clin", "clinic", BindMode::Keep, &actor())
        .await
        .unwrap();
    let e = eps.get("clinic").await.unwrap().unwrap();
    let auth = e.spec.auth.clone().unwrap();
    assert_eq!(auth.issuer.as_deref(), Some("http://127.0.0.1:9/idp/clin"));
    assert_eq!(
        c.realm("clin").await.unwrap().spec.endpoints,
        vec!["clinic"]
    );
    let views = c.endpoint_views().await;
    assert_eq!(
        views[0].provider,
        EndpointProvider::Internal {
            realm: "clin".into()
        }
    );
    // Preflight agrees.
    let pre = c.preflight("clin", None).await.unwrap();
    assert!(
        pre.check("preflight.endpoint.clinic")
            .is_some_and(|c| c.code == "endpoint-bound"),
        "{:#?}",
        pre.steps
    );
    c.unbind("clin", "clinic", &actor()).await.unwrap();
    assert!(
        eps.get("clinic")
            .await
            .unwrap()
            .unwrap()
            .spec
            .auth
            .is_none()
    );
    assert!(c.realm("clin").await.unwrap().spec.endpoints.is_empty());
    let _ = c.binder.as_ref().unwrap().views().await.unwrap();
}

#[tokio::test]
async fn diagnostic_cleans_up_its_fixture_even_when_unreachable() {
    let (c, _) = console(true).await;
    let r = c
        .create_draft("every", "Everything", Template::Everything)
        .await
        .unwrap();
    let report = c.diagnose("every", Default::default()).await.unwrap();
    // 127.0.0.1:9 is closed → discovery fails, everything else skipped.
    assert_eq!(report.check("discovery.oidc").unwrap().code, "net-connect");
    assert_eq!(c.history("every").len(), 1);
    let clients = c.admin.list_clients(&r.id).await.unwrap();
    assert!(
        clients
            .iter()
            .all(|cl| !cl.spec.client_id.starts_with(DIAGNOSTIC_PREFIX))
    );
    let users = c.admin.list_users(&r.id).await.unwrap();
    assert!(
        users
            .iter()
            .all(|u| !u.username.starts_with(DIAGNOSTIC_PREFIX))
    );
    // A deleted realm's history is not inherited by a new realm of that name.
    c.delete_realm("every", &actor()).await.unwrap();
    assert!(c.history("every").is_empty());
}
