//! A cached `ValueSet/$expand` response never outlives the content it was
//! computed from, and the value sets answered ahead of the response cache
//! (media types, the UCUM grammar) keep being answered there.
//!
//! Every test first repeats its requests, so the responses are in the
//! handler cache, then changes the stored content and expects the next
//! response to reflect it.
//!
//! Runs on SQLite, or on PostgreSQL with `HTS_TEST_BACKEND=postgres`.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};

const CS_URL: &str = "http://hts.test/inval/cs";
const VS_URL: &str = "http://hts.test/inval/vs";
/// The implicit "all codes of the CodeSystem" value set, as a query value.
const IMPLICIT_VS: &str = "http://hts.test/inval/cs%3Ffhir_vs";

fn code_system(version: &str, codes: &[(&str, &str)]) -> Value {
    json!({
        "resourceType": "CodeSystem", "id": "inval-cs", "url": CS_URL, "version": version,
        "name": "InvalCS", "status": "active", "content": "complete",
        "concept": codes.iter().map(|(c, d)| json!({"code": c, "display": d})).collect::<Vec<_>>()
    })
}

const BASE_CODES: &[(&str, &str)] = &[
    ("g1", "Glucose in blood"),
    ("g2", "Glucose in urine"),
    ("h1", "Hemoglobin"),
    ("s1", "Sodium"),
];

fn value_set(version: &str, compose: Value) -> Value {
    json!({
        "resourceType": "ValueSet", "id": "inval-vs", "url": VS_URL, "version": version,
        "name": "InvalVS", "status": "active", "compose": compose
    })
}

fn whole_system() -> Value {
    json!({"include": [{"system": CS_URL}]})
}

fn only(codes: &[&str]) -> Value {
    json!({"include": [{"system": CS_URL,
        "concept": codes.iter().map(|c| json!({"code": c})).collect::<Vec<_>>()}]})
}

fn bundle(resources: &[Value]) -> String {
    json!({
        "resourceType": "Bundle", "type": "collection",
        "entry": resources.iter().map(|r| json!({"resource": r})).collect::<Vec<_>>()
    })
    .to_string()
}

async fn app_with_base_content() -> TestApp {
    let app = TestApp::with_resource_store().await;
    app.import_bundle_ok(&bundle(&[
        code_system("1", BASE_CODES),
        value_set("1", whole_system()),
    ]))
    .await;
    app
}

fn sorted_codes(body: &Value) -> Vec<String> {
    let mut codes: Vec<String> = body["expansion"]["contains"]
        .as_array()
        .map(|c| {
            c.iter()
                .filter_map(|e| e["code"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    codes.sort();
    codes
}

/// The codes of `GET /ValueSet/$expand?<query>`, asked three times so the
/// answer is served from the response cache, which must agree with the
/// first, computed one.
async fn expand(app: &TestApp, query: &str) -> Vec<String> {
    let path = format!("/ValueSet/$expand?{query}");
    let (status, first) = app.get_fhir(&path).await;
    assert_eq!(status, StatusCode::OK, "{path}: {first}");
    for _ in 0..2 {
        let (status, again) = app.get_fhir(&path).await;
        assert_eq!(status, StatusCode::OK, "{path}: {again}");
        assert_eq!(
            sorted_codes(&again),
            sorted_codes(&first),
            "{path}: a repeated request answers differently"
        );
    }
    sorted_codes(&first)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_value_set_write_replaces_the_cached_expansions() {
    let app = app_with_base_content().await;
    let plain = format!("url={VS_URL}");
    let filtered = format!("url={VS_URL}&filter=glucose&count=20");
    let paged = format!("url={VS_URL}&offset=0&count=10");
    assert_eq!(expand(&app, &plain).await, ["g1", "g2", "h1", "s1"]);
    assert_eq!(expand(&app, &filtered).await, ["g1", "g2"]);
    assert_eq!(expand(&app, &paged).await, ["g1", "g2", "h1", "s1"]);

    let (status, body) = app
        .put_fhir(
            "/ValueSet/inval-vs",
            value_set("1", only(&["g2", "h1"])).to_string(),
        )
        .await;
    assert!(status.is_success(), "{status}: {body}");
    assert_eq!(expand(&app, &plain).await, ["g2", "h1"]);
    assert_eq!(expand(&app, &filtered).await, ["g2"]);
    assert_eq!(expand(&app, &paged).await, ["g2", "h1"]);

    let status = app.delete_fhir("/ValueSet/inval-vs").await;
    assert!(status.is_success(), "{status}");
    for query in [&plain, &filtered, &paged] {
        let (status, body) = app.get_fhir(&format!("/ValueSet/$expand?{query}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{query}: {body}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_code_system_write_replaces_the_cached_expansions() {
    let app = app_with_base_content().await;
    let stored = format!("url={VS_URL}");
    let implicit = format!("url={IMPLICIT_VS}");
    let filtered = format!("url={IMPLICIT_VS}&filter=glucose&count=20");
    assert_eq!(expand(&app, &stored).await, ["g1", "g2", "h1", "s1"]);
    assert_eq!(expand(&app, &implicit).await, ["g1", "g2", "h1", "s1"]);
    assert_eq!(expand(&app, &filtered).await, ["g1", "g2"]);

    let mut codes = BASE_CODES.to_vec();
    codes.push(("g3", "Glucose tolerance"));
    codes.retain(|(code, _)| *code != "s1");
    let (status, body) = app
        .put_fhir("/CodeSystem/inval-cs", code_system("1", &codes).to_string())
        .await;
    assert!(status.is_success(), "{status}: {body}");
    assert_eq!(expand(&app, &stored).await, ["g1", "g2", "g3", "h1"]);
    assert_eq!(expand(&app, &implicit).await, ["g1", "g2", "g3", "h1"]);
    assert_eq!(expand(&app, &filtered).await, ["g1", "g2", "g3"]);
}

/// A newer ValueSet version arriving by import becomes the answer of a
/// version-less request; a request for the older version keeps its answer.
#[tokio::test(flavor = "multi_thread")]
async fn an_imported_newer_version_replaces_the_cached_expansion() {
    let app = app_with_base_content().await;
    let latest = format!("url={VS_URL}");
    let first = format!("url={VS_URL}&valueSetVersion=1");
    let piped = format!("url={VS_URL}%7C1");
    assert_eq!(expand(&app, &latest).await, ["g1", "g2", "h1", "s1"]);
    assert_eq!(expand(&app, &first).await, ["g1", "g2", "h1", "s1"]);
    assert_eq!(expand(&app, &piped).await, ["g1", "g2", "h1", "s1"]);

    let mut second = value_set("2", only(&["s1"]));
    second["id"] = json!("inval-vs-2");
    app.import_bundle_ok(&bundle(&[second])).await;
    assert_eq!(expand(&app, &latest).await, ["s1"]);
    assert_eq!(expand(&app, &first).await, ["g1", "g2", "h1", "s1"]);
    assert_eq!(expand(&app, &piped).await, ["g1", "g2", "h1", "s1"]);
    assert_eq!(
        expand(&app, &format!("url={VS_URL}&valueSetVersion=2")).await,
        ["s1"]
    );
}

fn designation_languages(body: &Value, code: &str) -> Vec<String> {
    body["expansion"]["contains"]
        .as_array()
        .and_then(|c| c.iter().find(|e| e["code"] == code))
        .and_then(|e| e["designation"].as_array())
        .map(|d| {
            d.iter()
                .filter_map(|x| x["language"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// A supplement imported after an expansion was cached shows up in the next
/// one (supplements are applied automatically).
#[tokio::test(flavor = "multi_thread")]
async fn an_imported_supplement_replaces_the_cached_expansion() {
    let app = app_with_base_content().await;
    let path = format!("/ValueSet/$expand?url={VS_URL}&includeDesignations=true");
    for _ in 0..3 {
        let (status, body) = app.get_fhir(&path).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            !designation_languages(&body, "s1").contains(&"zh-Hant".to_string()),
            "{body}"
        );
    }

    app.import_bundle_ok(&bundle(&[json!({
        "resourceType": "CodeSystem", "id": "inval-cs-zh", "url": "http://hts.test/inval/cs-zh",
        "version": "1", "status": "active", "content": "supplement", "supplements": CS_URL,
        "concept": [{"code": "s1", "designation": [{"language": "zh-Hant", "value": "鈉"}]}]
    })]))
    .await;
    for _ in 0..3 {
        let (status, body) = app.get_fhir(&path).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            designation_languages(&body, "s1").contains(&"zh-Hant".to_string()),
            "{body}"
        );
    }
}

/// A value set over media types is answered ahead of the response cache.
/// Replacing an ordinary, cached value set with one — and back — changes the
/// answer both ways.
#[tokio::test(flavor = "multi_thread")]
async fn a_value_set_that_becomes_a_media_type_value_set_is_answered_as_one() {
    let app = app_with_base_content().await;
    let plain = format!("url={VS_URL}");
    assert_eq!(expand(&app, &plain).await, ["g1", "g2", "h1", "s1"]);

    let media_types = json!({"include": [{"system": "urn:ietf:bcp:13", "filter": [
        {"property": "registered", "op": "=", "value": "true"},
        {"property": "base", "op": "=", "value": "text/plain"}]}]});
    let (status, body) = app
        .put_fhir(
            "/ValueSet/inval-vs",
            value_set("1", media_types).to_string(),
        )
        .await;
    assert!(status.is_success(), "{status}: {body}");
    assert_eq!(expand(&app, &plain).await, ["text/plain"]);

    let (status, body) = app
        .put_fhir(
            "/ValueSet/inval-vs",
            value_set("1", only(&["h1"])).to_string(),
        )
        .await;
    assert!(status.is_success(), "{status}: {body}");
    assert_eq!(expand(&app, &plain).await, ["h1"]);
}

fn is_unclosed(body: &Value) -> bool {
    body["expansion"]["extension"].as_array().is_some_and(|e| {
        e.iter().any(|x| {
            x["url"] == "http://hl7.org/fhir/StructureDefinition/valueset-unclosed"
                && x["valueBoolean"] == true
        })
    })
}

/// The same for a value set over the UCUM grammar (an unclosed expansion).
#[tokio::test(flavor = "multi_thread")]
async fn a_value_set_that_becomes_a_ucum_value_set_is_answered_as_one() {
    let app = app_with_base_content().await;
    let path = format!("/ValueSet/$expand?url={VS_URL}");
    for _ in 0..3 {
        let (status, body) = app.get_fhir(&path).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!is_unclosed(&body), "{body}");
    }

    let ucum = json!({"include": [{"system": "http://unitsofmeasure.org"}]});
    let (status, body) = app
        .put_fhir("/ValueSet/inval-vs", value_set("1", ucum).to_string())
        .await;
    assert!(status.is_success(), "{status}: {body}");
    for _ in 0..3 {
        let (status, body) = app.get_fhir(&path).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(is_unclosed(&body), "{body}");
        assert!(
            body["expansion"]["parameter"]
                .as_array()
                .is_some_and(|p| p.iter().any(|x| x["name"] == "used-codesystem"
                    && x["valueUri"]
                        .as_str()
                        .is_some_and(|u| u.starts_with("http://unitsofmeasure.org")))),
            "{body}"
        );
    }

    let (status, body) = app
        .put_fhir(
            "/ValueSet/inval-vs",
            value_set("1", only(&["h1"])).to_string(),
        )
        .await;
    assert!(status.is_success(), "{status}: {body}");
    assert_eq!(expand(&app, &format!("url={VS_URL}")).await, ["h1"]);
}

// ── "Set as current" (admin API) ──────────────────────────────────────────────

#[cfg(feature = "sqlite")]
mod set_current {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use helios_hts::config::HtsConfig;
    use helios_hts::import::BundleImportBackend;
    use helios_hts::state::AppState;
    use helios_persistence::tenant::TenantContext;
    use tower::ServiceExt;

    const ADMIN_PASSWORD: &str = "expand-cache-test-password";
    // Two versions of a CodeSystem, under URLs of this test's own.
    const PIN_CS_URL: &str = "http://hts.test/inval-pin/cs";
    const PIN_VS_URL: &str = "http://hts.test/inval-pin/vs";
    const PIN_IMPLICIT_VS: &str = "http://hts.test/inval-pin/cs%3Ffhir_vs";

    fn pin_code_system(id: &str, version: &str, codes: &[(&str, &str)]) -> Value {
        let mut cs = code_system(version, codes);
        cs["id"] = json!(id);
        cs["url"] = json!(PIN_CS_URL);
        cs
    }

    async fn send(
        app: &Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        use base64::Engine;
        let auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("admin:{ADMIN_PASSWORD}"))
        );
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("x-forwarded-for", "10.1.2.3")
            .header("authorization", auth);
        let body = match body {
            Some(v) => {
                request = request.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let response = app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn codes(app: &Router, query: &str) -> Vec<String> {
        let path = format!("/ValueSet/$expand?{query}");
        let (status, first) = send(app, "GET", &path, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {first}");
        for _ in 0..2 {
            let (_, again) = send(app, "GET", &path, None).await;
            assert_eq!(sorted_codes(&again), sorted_codes(&first), "{path}");
        }
        sorted_codes(&first)
    }

    /// Pinning an older CodeSystem version as current, and clearing the pin,
    /// changes what a version-less expansion returns — also for responses
    /// already cached.
    #[tokio::test(flavor = "multi_thread")]
    async fn pinning_a_code_system_version_replaces_the_cached_expansions() {
        let dir = tempfile::tempdir().unwrap();
        let mut vs = value_set("1", json!({"include": [{"system": PIN_CS_URL}]}));
        vs["id"] = json!("inval-pin-vs");
        vs["url"] = json!(PIN_VS_URL);
        // One version per import, as an operator loads them.
        let content = [
            bundle(&[pin_code_system("inval-pin-cs", "1", BASE_CODES), vs]),
            bundle(&[pin_code_system(
                "inval-pin-cs-2",
                "2",
                &[("g1", "Glucose in blood"), ("n2", "New in two")],
            )]),
        ];
        let config = HtsConfig {
            admin_ui: true,
            admin_auth: Some(
                helios_admin_auth::AdminSettings::default().with_password(ADMIN_PASSWORD),
            ),
            admin_data_dir: dir.path().join("admin").to_string_lossy().into_owned(),
            import_dir: dir.path().join("import").to_string_lossy().into_owned(),
            package_registry: "off".into(),
            disk_headroom: 0,
            ..HtsConfig::default()
        };
        #[cfg(feature = "postgres")]
        if common::pg_mode() {
            let url = common::fresh_pg_database().await;
            let backend = common::pg_hts_backend(&url).await.unwrap();
            for one in &content {
                backend
                    .import_bundle(&TenantContext::system(), one.as_bytes())
                    .await
                    .unwrap();
            }
            let config = HtsConfig {
                storage_backend: "postgres".into(),
                database_url: url,
                ..config
            };
            let app = helios_hts::server::create_app(&config, AppState::new(backend));
            return pin_and_unpin(&app).await;
        }
        let db = dir.path().join("hts.db");
        let backend =
            helios_hts::backends::SqliteTerminologyBackend::new(db.to_str().unwrap()).unwrap();
        for one in &content {
            backend
                .import_bundle(&TenantContext::system(), one.as_bytes())
                .await
                .unwrap();
        }
        let pool = backend.pool().clone();
        let state = AppState::new(backend).with_hts_pool(pool);
        let config = HtsConfig {
            database_url: db.to_string_lossy().into_owned(),
            ..config
        };
        let app = helios_hts::server::create_app(&config, state);
        pin_and_unpin(&app).await;
    }

    async fn pin_and_unpin(app: &Router) {
        let stored = format!("url={PIN_VS_URL}");
        let implicit = format!("url={PIN_IMPLICIT_VS}");
        assert_eq!(codes(app, &stored).await, ["g1", "n2"]);
        assert_eq!(codes(app, &implicit).await, ["g1", "n2"]);

        let (status, body) = send(
            app,
            "PUT",
            "/admin/code-systems/current",
            Some(json!({"url": PIN_CS_URL, "version": "1"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(codes(app, &stored).await, ["g1", "g2", "h1", "s1"]);
        assert_eq!(codes(app, &implicit).await, ["g1", "g2", "h1", "s1"]);

        let (status, body) = send(
            app,
            "PUT",
            "/admin/code-systems/current",
            Some(json!({"url": PIN_CS_URL, "version": null})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(codes(app, &stored).await, ["g1", "n2"]);
        assert_eq!(codes(app, &implicit).await, ["g1", "n2"]);
    }
}
