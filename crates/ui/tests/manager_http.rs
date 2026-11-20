//! The manager console (package P5) end to end through the mounted router:
//! every page renders in en and zh-Hant without a missing catalog key, forms
//! save / refuse / conflict as designed, roles hide and refuse admin actions,
//! old addresses redirect, data pages are scoped to their endpoint.
//!
//! One binary, one installed manager: `set_manager` is process-global, and
//! each test gets a fresh store through [`fixture`] (the runtime is replaced
//! under a lock so tests do not see each other's endpoints).

use std::sync::Arc;

use tokio::sync::{Mutex, MutexGuard};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use helios_endpoints::{
    Actor, AuthMode, ConsolePrincipal, ConsoleRole, EndpointName, EndpointRegistry, EndpointSpec,
    EndpointStatus, EndpointStore, FhirRelease, MemoryEndpointStore,
};
use helios_ui::manager::{EndpointRouting, ManagerRuntime, StoreBackend, set_manager};
use http_body_util::BodyExt;
use tower::ServiceExt;

static LOCK: Mutex<()> = Mutex::const_new(());

struct Fixture {
    app: Router,
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

async fn post(app: &Router, uri: &str, form: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    send(
        app,
        Request::post(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(form.to_string()))
            .unwrap(),
    )
    .await
}

const PAGES: &[&str] = &[
    "/ui",
    "/ui/endpoints/new",
    "/ui/e/twcore",
    "/ui/e/twcore/config",
    "/ui/e/twcore/security",
    "/ui/e/twcore/validation",
    "/ui/e/twcore/terminology",
    "/ui/e/twcore/sync",
    "/ui/e/twcore/audit",
    "/ui/e/twcore/history",
    "/ui/e/twcore/duplicate",
    "/ui/e/default",
    "/ui/e/default/config",
    "/ui/about",
];

#[tokio::test]
async fn every_manager_page_renders_in_en_and_zh_hant_without_missing_keys() {
    let f = fixture().await;
    for lang in ["en", "zh-Hant"] {
        for page in PAGES {
            let (status, html) = get_lang(&f.app, page, lang).await;
            assert_eq!(status, StatusCode::OK, "{page} [{lang}]");
            assert!(
                !html.contains("mgr-"),
                "{page} [{lang}] shows an untranslated key near: {}",
                &html[html.find("mgr-").unwrap().saturating_sub(80)..][..160]
            );
            assert!(html.contains(&format!(r#"<html lang="{lang}">"#)), "{page}");
            // One h1, the skip link, the attribution footer, the brand.
            assert_eq!(html.matches("<h1").count(), 1, "{page} [{lang}] h1 count");
            assert!(
                html.contains(r##"class="skip-link" href="#main""##),
                "{page}"
            );
            assert!(html.contains("Helios FHIR Server"), "{page} attribution");
            assert!(
                html.contains("brand/healthymind-logo-horizontal.png"),
                "{page}"
            );
        }
    }
    let (_, zh) = get_lang(&f.app, "/ui", "zh-Hant").await;
    assert!(zh.contains("療心智能 FHIR 伺服器管理平台"));
    let (_, en) = get_lang(&f.app, "/ui", "en").await;
    assert!(en.contains("HealthyMind FHIR Server Manager"));
}

#[tokio::test]
async fn overview_lists_endpoints_with_badges_and_filters() {
    let f = fixture().await;
    let (status, html) = get(&f.app, "/ui").await;
    assert_eq!(status, StatusCode::OK);
    for needle in [
        r#"href="/ui/e/twcore""#,
        "TW Core sandbox",
        "http://localhost:8080/fhir/twcore",
        "http://localhost:8080/fhir",
        "FHIR R4",
        "env=test",
        r#"href="/ui/endpoints/new""#,
        "health-strip",
        "status-pill--muted",
    ] {
        assert!(html.contains(needle), "overview lacks {needle}");
    }
    let (_, filtered) = get(&f.app, "/ui?q=tw").await;
    assert!(filtered.contains("TW Core sandbox"));
    assert!(!filtered.contains(r#"href="/ui/e/lab" class="endpoint-card__link""#));
    let (_, disabled) = get(&f.app, "/ui?status=disabled").await;
    assert!(disabled.contains(r#"href="/ui/e/lab" class="endpoint-card__link""#));
    assert!(!disabled.contains(r#"href="/ui/e/twcore" class="endpoint-card__link""#));
    let (_, none) = get(&f.app, "/ui?q=zzz").await;
    assert!(none.contains("empty-state"));
}

#[tokio::test]
async fn unknown_endpoints_get_the_not_found_page() {
    let f = fixture().await;
    let (status, html) = get(&f.app, "/ui/e/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(html.contains("error-page"));
    let (status, _) = get(&f.app, "/ui/e/twcore/bogus").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn saving_a_section_validates_then_persists() {
    let f = fixture().await;
    // Auth on without issuer / JWKS: 422 with the errors on their fields.
    let (status, _, html) = post(
        &f.app,
        "/ui/e/twcore/security",
        "revision=1&auth_mode=full&scopes_allow_user=on&cors_inherit=on&rl_inherit=on",
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        html.contains(r#"id="f-auth_issuer-err""#),
        "issuer error shown"
    );
    assert!(html.contains(r#"aria-invalid="true""#));
    assert_eq!(f.store.get("twcore").await.unwrap().unwrap().revision, 1);

    let (status, headers, _) = post(
        &f.app,
        "/ui/e/twcore/security",
        "revision=1&auth_mode=write-protect&auth_issuer=https%3A%2F%2Fidp.example%2Fr\
         &auth_jwks_url=https%3A%2F%2Fidp.example%2Fc&auth_audience=tw&scopes_allow_user=on\
         &cors_inherit=on&rl_inherit=on",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "save redirects");
    assert_eq!(headers[header::LOCATION], "/ui/e/twcore/security?saved=1");
    let stored = f.store.get("twcore").await.unwrap().unwrap();
    assert_eq!(
        stored.spec.auth.as_ref().unwrap().mode,
        AuthMode::WriteProtect
    );
    assert!(stored.spec.cors.is_none());

    let (_, html) = get(&f.app, "/ui/e/twcore/security?saved=1").await;
    assert!(html.contains("notice--ok"));
    // The page now reflects the override and the new revision.
    assert!(html.contains(r#"value="https://idp.example/r""#));
    let (_, history) = get(&f.app, "/ui/e/twcore/history").await;
    assert!(history.contains("auth"), "history shows the auth diff");
}

#[tokio::test]
async fn a_stale_revision_shows_the_conflict_banner() {
    let f = fixture().await;
    let (s, _, _) = post(
        &f.app,
        "/ui/e/twcore/config",
        "revision=1&display_name=First",
    )
    .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let (status, _, html) = post(
        &f.app,
        "/ui/e/twcore/config",
        "revision=1&display_name=Second",
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(html.contains("notice--warn"));
    assert!(
        html.contains(r#"name="revision" value="2""#),
        "overwrite button"
    );
    assert_eq!(
        f.store
            .get("twcore")
            .await
            .unwrap()
            .unwrap()
            .spec
            .display_name,
        "First"
    );
}

#[tokio::test]
async fn preview_shows_a_field_diff() {
    let f = fixture().await;
    let (status, _, html) = post(
        &f.app,
        "/ui/e/twcore/validation/preview",
        "val_mode=enforce&val_terminology=embedded",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("<code>validation</code>"), "{html}");
    assert!(!html.contains("mgr-"));
}

#[tokio::test]
async fn creating_an_endpoint_end_to_end() {
    let f = fixture().await;
    let (status, _, html) = post(&f.app, "/ui/endpoints", "name=UI&display_name=").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains(r#"id="f-name-err""#));
    let (status, _, html) = post(&f.app, "/ui/endpoints", "name=twcore&display_name=Dup").await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "duplicate name refused"
    );
    assert!(html.contains("twcore"));

    let (status, headers, _) = post(
        &f.app,
        "/ui/endpoints",
        "display_name=Partner+A&name=partner-a&fhir_version=R4&data_source=empty&auth_choice=inherit&val_choice=custom&val_mode=log&val_terminology=embedded",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/ui/e/partner-a?created=1");
    let created = f.store.get("partner-a").await.unwrap().unwrap();
    assert_eq!(created.tenant_id(), "partner-a");
    let (status, html) = get(&f.app, "/ui/e/partner-a").await;
    assert_eq!(status, StatusCode::OK, "the registry was reloaded");
    assert!(html.contains("Partner A"));

    // Duplicate is not available before P6: a friendly error, not a 500.
    let (status, _, html) = post(
        &f.app,
        "/ui/endpoints",
        "display_name=Copy&name=copy&data_source=duplicate&duplicate_from=twcore&duplicate_data=current",
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("notice--danger"));
}

#[tokio::test]
async fn availability_check_and_new_preview() {
    let f = fixture().await;
    let (_, taken) = get(&f.app, "/ui/endpoints/check?name=twcore").await;
    assert!(taken.contains("availability--bad"));
    let (_, reserved) = get(&f.app, "/ui/endpoints/check?name=admin").await;
    assert!(reserved.contains("availability--bad"));
    let (_, free) = get(&f.app, "/ui/endpoints/check?name=fresh").await;
    assert!(free.contains("availability--ok"));
    assert!(free.contains("http://localhost:8080/fhir/fresh"));
    let (status, _, preview) = post(
        &f.app,
        "/ui/endpoints/preview",
        "display_name=X&name=x&auth_choice=custom&auth_mode=full&auth_issuer=https%3A%2F%2Fi.example&auth_jwks_url=https%3A%2F%2Fi.example%2Fk&scopes_allow_user=on",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(preview.contains("json-preview"));
    assert!(preview.contains("notice--warn"), "no-audience warning");
}

#[tokio::test]
async fn disabling_and_deleting_with_typed_confirmation() {
    let f = fixture().await;
    let (status, headers, _) =
        post(&f.app, "/ui/e/twcore/status", "action=disable&revision=1").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/ui/e/twcore");
    let ep = f.store.get("twcore").await.unwrap().unwrap();
    assert_eq!(ep.spec.status, EndpointStatus::Disabled);

    let (status, _, html) = post(
        &f.app,
        "/ui/e/twcore/status",
        "action=delete&revision=2&confirm=wrong",
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains(r#"id="f-confirm-err""#));
    let (status, headers, _) = post(
        &f.app,
        "/ui/e/twcore/status",
        "action=delete&revision=2&confirm=twcore",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/ui?changed=twcore");
    assert!(f.store.get("twcore").await.unwrap().is_none());
    // The default endpoint can never be disabled from the console.
    let (status, _, _) = post(&f.app, "/ui/e/default/status", "action=disable").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn viewers_see_no_admin_actions_and_are_refused() {
    let f = fixture_as(Some(ConsoleRole::Viewer)).await;
    let (_, overview) = get(&f.app, "/ui").await;
    assert!(!overview.contains(r#"href="/ui/endpoints/new""#));
    let (_, config) = get(&f.app, "/ui/e/twcore/config").await;
    assert!(config.contains("<fieldset class=\"config-form__fields\" disabled>"));
    assert!(!config.contains("save-bar"));
    let (status, _) = get(&f.app, "/ui/endpoints/new").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = post(&f.app, "/ui/e/twcore/config", "revision=1&display_name=X").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = post(&f.app, "/ui/endpoints", "name=x&display_name=X").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Admin sees everything.
    drop(f);
    let f = fixture_as(Some(ConsoleRole::Admin)).await;
    let (_, overview) = get(&f.app, "/ui").await;
    assert!(overview.contains(r#"href="/ui/endpoints/new""#));
}

#[tokio::test]
async fn old_addresses_redirect_to_the_endpoint_they_came_from() {
    let f = fixture().await;
    let (status, headers, _) = send(
        &f.app,
        Request::get("/ui/resources?type=Patient")
            .header("sec-fetch-mode", "navigate")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers[header::LOCATION],
        "/ui/e/default/data/resources?type=Patient"
    );
    let (_, headers, _) = send(
        &f.app,
        Request::get("/ui/batch")
            .header("sec-fetch-mode", "navigate")
            .header(
                header::REFERER,
                "http://localhost/ui/e/twcore/data/resources",
            )
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(headers[header::LOCATION], "/ui/e/twcore/data/batch");
}

#[tokio::test]
async fn data_pages_are_scoped_to_their_endpoint() {
    let f = fixture().await;
    let (status, html) = get(&f.app, "/ui/e/twcore/data/batch").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains(r#"<meta name="hfs-tenant" content="twcore">"#));
    // TenantHeader routing: the FHIR base stays the server's, the tenant
    // header selects the endpoint's data.
    assert!(html.contains(r#"<meta name="hfs-fhir-base" content="/fhir">"#));
    // The sidebar carries the endpoint's sections with scoped links.
    assert!(html.contains(r#"href="/ui/e/twcore/data/resources""#));
    assert!(html.contains(r#"href="/ui/e/twcore/security""#));
    assert!(html.contains("TW Core sandbox"));
    let (status, _) = get(&f.app, "/ui/e/twcore/data").await;
    assert_eq!(status, StatusCode::OK, "the endpoint dashboard");
}

#[tokio::test]
async fn about_names_the_brand_and_the_upstream_license() {
    let f = fixture().await;
    let (_, html) = get(&f.app, "/ui/about").await;
    assert!(html.contains("healthymind-logo-square.png"));
    assert!(html.contains("MIT"));
    assert!(html.contains("github.com/HeliosSoftware/hfs"));
}

#[tokio::test]
async fn brand_assets_are_served() {
    let f = fixture().await;
    for asset in [
        "/ui/assets/brand/favicon.svg",
        "/ui/assets/brand/favicon-32.png",
        "/ui/assets/brand/healthymind-logo-horizontal.png",
        "/ui/assets/brand/healthymind-logo-square.png",
        "/ui/assets/brand/manifest.webmanifest",
        "/ui/assets/manager.js",
    ] {
        let (status, _) = get(&f.app, asset).await;
        assert_eq!(status, StatusCode::OK, "{asset}");
    }
}

#[test]
fn design_tokens_in_the_stylesheet_match_the_asserted_ones() {
    let css = include_str!("../assets/app.css").to_lowercase();
    for theme in helios_ui_chrome::tokens::THEMES {
        for value in [
            theme.bg,
            theme.surface,
            theme.text,
            theme.muted,
            theme.primary,
            theme.on_primary,
            theme.accent,
            theme.info,
            theme.ok,
            theme.warn,
            theme.danger,
            theme.ok_pill.0,
            theme.ok_pill.1,
            theme.warn_pill.0,
            theme.warn_pill.1,
            theme.danger_pill.0,
            theme.danger_pill.1,
            theme.info_pill.0,
            theme.info_pill.1,
            theme.muted_pill.0,
            theme.muted_pill.1,
            theme.focus,
        ] {
            assert!(
                css.contains(value),
                "{} token {value} missing from app.css",
                theme.name
            );
        }
    }
    assert!(css.contains(helios_ui_chrome::tokens::BRAND_GRADIENT));
}

/// Every `mgr-*` key a template or the manager code uses exists in the
/// English catalog, and zh-Hant defines exactly the same keys.
#[test]
fn manager_catalog_is_complete_in_en_and_zh_hant() {
    use std::collections::BTreeSet;
    fn keys(locale: &str) -> BTreeSet<String> {
        let path = format!("../../locales/{locale}/manager.ftl");
        let src = std::fs::read_to_string(&path).unwrap();
        let res =
            fluent_syntax::parser::parse(src).unwrap_or_else(|(_, e)| panic!("{path}: {e:?}"));
        res.body
            .iter()
            .filter_map(|e| match e {
                fluent_syntax::ast::Entry::Message(m) => Some(m.id.name.clone()),
                _ => None,
            })
            .collect()
    }
    let en = keys("en");
    let zh = keys("zh-Hant");
    assert_eq!(
        en.symmetric_difference(&zh).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "en and zh-Hant manager.ftl differ"
    );
    let mut used = BTreeSet::new();
    let mut stack = vec![
        std::path::PathBuf::from("templates"),
        std::path::PathBuf::from("src/manager"),
    ];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let mut rest = text.as_str();
            while let Some(i) = rest.find("\"mgr-") {
                let tail = &rest[i + 1..];
                let end = tail
                    .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
                    .unwrap_or(tail.len());
                let key = &tail[..end];
                // Prefix probes (`w.starts_with("mgr-warn")`) are not keys.
                if !key.ends_with('-') && key != "mgr-warn" {
                    used.insert(key.to_string());
                }
                rest = &tail[end..];
            }
        }
    }
    let missing: Vec<_> = used.difference(&en).collect();
    assert!(
        missing.is_empty(),
        "keys used but not in en/manager.ftl: {missing:?}"
    );
}
