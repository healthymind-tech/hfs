//! The console-wide search (`/ui/find`, `helios_ui::global_search`) end to
//! end through the mounted router: every section / field anchor of the index
//! exists on the rendered page it points at, results carry their path,
//! roles hide what a user cannot open, and every hit's link answers.

use std::sync::Arc;

use tokio::sync::{Mutex, MutexGuard};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use helios_endpoints::{
    Actor, ConsolePrincipal, ConsoleRole, EndpointName, EndpointRegistry, EndpointSpec,
    EndpointStatus, EndpointStore, FhirRelease, MemoryEndpointStore,
};
use helios_ui::manager::{EndpointRouting, ManagerRuntime, StoreBackend, set_manager};
use http_body_util::BodyExt;
use tower::ServiceExt;

static LOCK: Mutex<()> = Mutex::const_new(());

struct Fixture {
    app: Router,
    #[allow(dead_code)]
    store: Arc<dyn EndpointStore>,
    _guard: MutexGuard<'static, ()>,
}

async fn fixture() -> Fixture {
    fixture_as(None).await
}

/// The app with an optional console principal stamped on every request (what
/// the P3 console guard will do).
async fn fixture_as(role: Option<ConsoleRole>) -> Fixture {
    let guard = LOCK.lock().await;
    let store: Arc<dyn EndpointStore> = Arc::new(MemoryEndpointStore::new());
    store
        .create(
            EndpointSpec::default_endpoint("default", FhirRelease::R4),
            &Actor::bootstrap(),
        )
        .await
        .unwrap();
    let mut tw = EndpointSpec::named(EndpointName::parse("twcore").unwrap(), "TW Core sandbox");
    tw.description = Some("Taiwan Core IG testing".into());
    tw.labels.insert("env".into(), "test".into());
    store.create(tw, &Actor::bootstrap()).await.unwrap();
    let mut lab = EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab");
    lab.status = EndpointStatus::Disabled;
    store.create(lab, &Actor::bootstrap()).await.unwrap();

    let registry = Arc::new(EndpointRegistry::load(store.as_ref()).await.unwrap());
    let backend = Arc::new(StoreBackend::new(store.clone()).with_registry(registry.clone()));
    set_manager(ManagerRuntime::new(backend, registry).with_routing(EndpointRouting::TenantHeader));

    let mut app = helios_ui::mount_with_conformance_source(
        Router::new(),
        "9.9.9",
        Some(std::path::PathBuf::from("../../data")),
        helios_ui::NlSearch::default(),
        None,
        None,
        "default".to_string(),
        Arc::new(helios_ui::StaticConformanceSource::from_data_dir(
            std::path::Path::new("../../data"),
        )),
        helios_fhir::FhirVersion::R4,
        None,
        "http://localhost:8080/fhir".to_string(),
        None,
    );
    if let Some(role) = role {
        app = app.layer(axum::middleware::from_fn(
            move |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
                req.extensions_mut().insert(ConsolePrincipal {
                    subject: "local:test".into(),
                    display_name: "Test".into(),
                    role,
                    endpoint_roles: Default::default(),
                    via: helios_endpoints::console::ConsoleVia::Local,
                });
                next.run(req).await
            },
        ));
    }
    Fixture {
        app,
        store,
        _guard: guard,
    }
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

async fn get(app: &Router, uri: &str) -> (StatusCode, String) {
    let (s, _, b) = send(app, Request::get(uri).body(Body::empty()).unwrap()).await;
    (s, b)
}

async fn get_lang(app: &Router, uri: &str, lang: &str) -> (StatusCode, String) {
    let (s, _, b) = send(
        app,
        Request::get(uri)
            .header(header::COOKIE, format!("hfs_lang={lang}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    (s, b)
}

use helios_ui::global_search::{When, spot_table};

fn hrefs(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find("href=\"") {
        rest = &rest[i + 6..];
        let end = rest.find('"').unwrap();
        out.push(rest[..end].replace("&amp;", "&"));
        rest = &rest[end..];
    }
    out
}

#[tokio::test]
async fn every_anchor_exists_on_its_rendered_page() {
    let f = fixture().await;
    let pages = [
        ("ep-home", ""),
        ("ep-config", "/config"),
        ("ep-security", "/security"),
        ("ep-validation", "/validation"),
        ("ep-terminology", "/terminology"),
    ];
    let mut checked = 0;
    for (page, route) in pages {
        for (ep, is_default) in [("default", true), ("twcore", false)] {
            let (status, body) = get(&f.app, &format!("/ui/e/{ep}{route}")).await;
            assert_eq!(status, StatusCode::OK, "{ep}{route}");
            for s in spot_table().iter().filter(|s| s.page == page) {
                match s.when {
                    When::DefaultEndpoint if !is_default => continue,
                    When::OtherEndpoints if is_default => continue,
                    When::Conditional => continue,
                    _ => {}
                }
                assert!(
                    body.contains(&format!("id=\"{}\"", s.anchor)),
                    "/ui/e/{ep}{route}: no element #{} ({})",
                    s.anchor,
                    s.label
                );
                checked += 1;
            }
        }
    }
    let (status, body) = get(&f.app, "/ui/about").await;
    assert_eq!(status, StatusCode::OK);
    for s in spot_table().iter().filter(|s| s.page == "about") {
        assert!(
            body.contains(&format!("id=\"{}\"", s.anchor)),
            "/ui/about #{}",
            s.anchor
        );
        checked += 1;
    }
    assert!(checked > 60, "only {checked} anchors checked");
}

#[tokio::test]
async fn results_name_their_path_and_open() {
    let f = fixture().await;
    let (status, body) = get(&f.app, "/ui/find?part=static&q=jwks").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("role=\"option\""));
    assert!(
        body.contains("href=\"/ui/e/default/security#f-auth_jwks_url\""),
        "{body}"
    );
    assert!(body.contains("data-gs-target=\"f-auth_jwks_url\""));
    assert!(body.contains("FHIR endpoints"));
    assert!(body.contains("<mark>JWKS</mark>"));
    // Chinese labels, English words still match.
    let (_, zh) = get_lang(&f.app, "/ui/find?part=static&q=jwks", "zh-Hant").await;
    assert!(zh.contains("FHIR 端點"), "{zh}");
    // Every link a few common queries return answers.
    let mut seen = std::collections::BTreeSet::new();
    for q in [
        "jwks",
        "issuer",
        "rate limit",
        "cors",
        "validation",
        "audit",
        "sync",
        "resources",
        "terminology",
        "about",
        "endpoint",
    ] {
        let (_, body) = get(
            &f.app,
            &format!("/ui/find?part=static&q={}", q.replace(' ', "+")),
        )
        .await;
        for h in hrefs(&body) {
            let path = h.split('#').next().unwrap().to_string();
            if seen.insert(path.clone()) {
                let (status, _) = get(&f.app, &path).await;
                assert!(
                    status.is_success() || status.is_redirection(),
                    "{q}: {path} answered {status}"
                );
            }
        }
    }
    assert!(seen.len() > 10, "{seen:?}");
    // Data objects: endpoints by display name.
    let (_, data) = get(&f.app, "/ui/find?part=data&q=TW+Core").await;
    assert!(data.contains("href=\"/ui/e/twcore\""), "{data}");
    // Without a terminology service the terminology part is empty, not an error.
    let (status, tx) = get(&f.app, "/ui/find?part=tx&q=loinc").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!tx.contains("role=\"option\""));
    // A browser fetch (Accept: text/html) is never redirected onto an
    // endpoint's data pages: the search is server-wide.
    let (status, _, body) = send(
        &f.app,
        Request::get("/ui/find?part=static&q=cors")
            .header(header::ACCEPT, "text/html")
            .header("sec-fetch-mode", "cors")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("#sec-cors"));
    let (status, _, _) = send(
        &f.app,
        Request::get("/ui/find?q=cors")
            .header(header::ACCEPT, "text/html")
            .header("sec-fetch-mode", "navigate")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The no-JavaScript page.
    let (status, page) = get(&f.app, "/ui/find?q=cors").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("class=\"page-head__title\""));
    assert!(page.contains("/ui/e/default/security#sec-cors"));
}

#[tokio::test]
async fn roles_hide_what_cannot_be_opened() {
    let f = fixture_as(Some(ConsoleRole::Viewer)).await;
    for q in ["new endpoint", "tenants", "packages"] {
        let (_, body) = get(
            &f.app,
            &format!("/ui/find?part=static&q={}", q.replace(' ', "+")),
        )
        .await;
        for h in hrefs(&body) {
            assert!(
                !h.starts_with("/ui/endpoints")
                    && !h.starts_with("/ui/tenants")
                    && !h.contains("/packages"),
                "viewer sees {h} for {q}"
            );
        }
    }
    // Endpoint settings stay findable (read-only pages).
    let (_, body) = get(&f.app, "/ui/find?part=static&q=jwks").await;
    assert!(body.contains("/ui/e/default/security#f-auth_jwks_url"));
    drop(f);
    let f = fixture_as(Some(ConsoleRole::Admin)).await;
    let (_, body) = get(&f.app, "/ui/find?part=static&q=new+endpoint").await;
    assert!(body.contains("href=\"/ui/endpoints/new\""), "{body}");
}
