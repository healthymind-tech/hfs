//! The package admin API's online paths (`/admin/packages/registry/*` and an
//! install fetched from a registry), against an in-process mock of an
//! npm-style FHIR package registry and the FHIR IG marketplace index.
//! `admin_packages.rs` covers the offline (upload) paths.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum_test::TestServer;
use flate2::{Compression, write::GzEncoder};
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use helios_rest::ServerConfig;
use helios_rest::config::{PackageConfig, RegistryMode};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};

fn tgz(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut tar = Vec::new();
    for (path, body) in members {
        let mut header = [0u8; 512];
        header[..path.len()].copy_from_slice(path.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", body.len()).as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[148..156].copy_from_slice(b"        ");
        let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        tar.extend_from_slice(&header);
        tar.extend_from_slice(body);
        tar.extend(std::iter::repeat_n(0u8, (512 - body.len() % 512) % 512));
    }
    tar.extend(std::iter::repeat_n(0u8, 1024));
    let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

fn acme_package() -> Vec<u8> {
    let manifest = json!({
        "name": "acme.online", "version": "1.0.0", "fhirVersions": ["4.0.1"],
        "title": "Acme online", "canonical": "http://acme.org/online",
        "dependencies": {"hl7.fhir.r4.core": "4.0.1"},
    })
    .to_string();
    let profile = json!({
        "resourceType": "StructureDefinition", "id": "acme-online-patient",
        "url": "http://acme.org/online/StructureDefinition/acme-online-patient",
        "version": "1.0.0", "name": "AcmeOnlinePatient", "status": "active",
        "kind": "resource", "abstract": false, "type": "Patient",
        "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
        "derivation": "constraint",
        "differential": {"element": [
            {"id": "Patient", "path": "Patient"},
            {"id": "Patient.gender", "path": "Patient.gender", "min": 1}
        ]}
    })
    .to_string();
    tgz(&[
        ("package/package.json", manifest.as_bytes()),
        (
            "package/StructureDefinition-acme-online-patient.json",
            profile.as_bytes(),
        ),
    ])
}

#[derive(Clone)]
struct Mock {
    base: Arc<std::sync::OnceLock<String>>,
    archive: Arc<Vec<u8>>,
    catalog_up: bool,
}

async fn registry_path(State(m): State<Mock>, Path(rest): Path<String>) -> Response {
    let base = m.base.get().cloned().unwrap_or_default();
    match rest.as_str() {
        "acme.online" => axum::Json(json!({
            "name": "acme.online",
            "description": "Acme online IG",
            "dist-tags": {"latest": "1.0.0"},
            "versions": {"1.0.0": {"fhirVersion": "R4", "dist": {
                "tarball": format!("{base}/acme.online/1.0.0"),
                "shasum": hex(&Sha1::digest(m.archive.as_slice())),
            }}}
        }))
        .into_response(),
        "acme.online/1.0.0" => m.archive.as_ref().clone().into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn catalog(
    State(m): State<Mock>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if !m.catalog_up {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let name = q.get("name").cloned().unwrap_or_default();
    let all = vec![
        json!({"name": "acme.online", "version": "1.0.0", "fhirVersion": "4.0.1"}),
        json!({"name": "zz.other", "version": "2.0.0", "fhirVersion": "4.0.1"}),
        json!({"name": "tw.gov.mohw.twcore", "fhirVersion": "4.0.1"}),
    ];
    let hits: Vec<Value> = all
        .into_iter()
        .filter(|e| e["name"].as_str().unwrap().contains(&name))
        .collect();
    axum::Json(Value::Array(hits)).into_response()
}

async fn market() -> Response {
    axum::Json(json!({"guides": [
        {"name": "Taiwan Core", "npm-name": "tw.gov.mohw.twcore", "category": "National Base",
         "country": "TW", "authority": "MOHW", "description": "Taiwan core profiles",
         "canonical": "https://twcore.mohw.gov.tw/ig/twcore",
         "editions": [{"name": "STU1", "ig-version": "1.0.0",
                       "package": "tw.gov.mohw.twcore#1.0.0", "fhir-version": ["4.0.1"]}]},
        {"name": "Acme Online", "npm-name": "acme.online", "category": "Test",
         "country": "us", "authority": "Acme", "description": "Online test guide",
         "canonical": "http://acme.org/online",
         "editions": [{"name": "1", "ig-version": "1.0.0",
                       "package": "acme.online#1.0.0", "fhir-version": ["4.0.1"]}]},
        {"name": "R5 Thing", "npm-name": "r5.thing", "category": "Test",
         "country": "uv", "authority": "HL7", "description": "An R5 guide",
         "canonical": "http://hl7.org/r5thing",
         "editions": [{"name": "1", "ig-version": "0.1.0",
                       "package": "r5.thing#0.1.0", "fhir-version": ["5.0.0"]}]}
    ]}))
    .into_response()
}

async fn mock(catalog_up: bool) -> String {
    let m = Mock {
        base: Arc::new(std::sync::OnceLock::new()),
        archive: Arc::new(acme_package()),
        catalog_up,
    };
    let app = Router::new()
        .route("/catalog", get(catalog))
        .route("/market.json", get(market))
        .route("/{*rest}", get(registry_path))
        .with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    m.base.set(base.clone()).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

fn server(base: &str) -> (TestServer, tempfile::TempDir) {
    let data_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("data"))
        .unwrap();
    let backend = SqliteBackend::with_config(
        ":memory:",
        SqliteBackendConfig {
            data_dir: Some(data_dir),
            ..Default::default()
        },
    )
    .expect("sqlite");
    backend.init_schema().expect("schema");
    let cache = tempfile::tempdir().unwrap();
    let config = ServerConfig {
        seed_conformance: false,
        request_timeout: 60,
        packages: PackageConfig {
            admin_ui: true,
            registry: RegistryMode::Mirrors(vec![base.to_string()]),
            marketplace_url: Some(format!("{base}/market.json")),
            cache_dir: Some(cache.path().to_path_buf()),
            core_terminology: false,
            ..PackageConfig::default()
        },
        ..ServerConfig::for_testing()
    };
    let app = helios_rest::create_app_with_config(backend, config);
    (TestServer::new(app).unwrap(), cache)
}

fn names(v: &Value) -> Vec<String> {
    v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn registry_search_merges_marketplace_details_and_prefers_a_country() {
    let base = mock(true).await;
    let (server, _cache) = server(&base);
    let r: Value = server
        .get("/admin/packages/registry/search?q=&fhirversion=4.0.1")
        .await
        .json();
    assert_eq!(r["source"], "registry");
    assert!(r["error"].is_null());
    assert_eq!(r["total"], 3);
    // Taiwan first (default prefer=tw), then by name.
    assert_eq!(names(&r), ["tw.gov.mohw.twcore", "acme.online", "zz.other"]);
    let tw = &r["results"][0];
    assert_eq!(tw["title"], "Taiwan Core");
    assert_eq!(tw["category"], "National Base");
    // The catalog entry had no version: the marketplace's latest edition.
    assert_eq!(tw["version"], "1.0.0");
    assert_eq!(tw["canonical"], "https://twcore.mohw.gov.tw/ig/twcore");
    assert_eq!(tw["installed"], json!([]));

    // prefer=us and paging.
    let r: Value = server
        .get("/admin/packages/registry/search?q=&prefer=US&offset=1&limit=1")
        .await
        .json();
    assert_eq!(r["total"], 3);
    assert_eq!(r["offset"], 1);
    assert_eq!(names(&r), ["tw.gov.mohw.twcore"]);
}

#[tokio::test]
async fn registry_search_falls_back_to_the_marketplace() {
    let base = mock(false).await;
    let (server, _cache) = server(&base);
    let r: Value = server
        .get("/admin/packages/registry/search?q=acme")
        .await
        .json();
    assert_eq!(r["source"], "marketplace");
    assert!(r["error"].as_str().unwrap().contains("503"), "{r}");
    assert_eq!(names(&r), ["acme.online"]);
    assert_eq!(r["results"][0]["fhirVersion"], "4.0.1");
    assert_eq!(r["results"][0]["version"], "1.0.0");
    // Title matches too, case-insensitively.
    let r: Value = server
        .get("/admin/packages/registry/search?q=TAIWAN")
        .await
        .json();
    assert_eq!(names(&r), ["tw.gov.mohw.twcore"]);
}

#[tokio::test]
async fn marketplace_filters_facets_and_paging() {
    let base = mock(true).await;
    let (server, _cache) = server(&base);
    let r: Value = server
        .get("/admin/packages/registry/marketplace")
        .await
        .json();
    assert_eq!(r["available"], true);
    assert_eq!(r["total"], 3);
    assert_eq!(r["facets"]["categories"]["Test"], 2);
    assert_eq!(r["facets"]["countries"]["tw"], 1);
    assert_eq!(r["facets"]["authorities"]["HL7"], 1);
    // Preferred country first, then by title.
    assert_eq!(names(&r), ["tw.gov.mohw.twcore", "acme.online", "r5.thing"]);
    assert_eq!(r["results"][0]["latest"], "1.0.0");

    let get = |q: &'static str| {
        let server = &server;
        async move {
            server
                .get(&format!("/admin/packages/registry/marketplace?{q}"))
                .await
                .json::<Value>()
        }
    };
    assert_eq!(names(&get("fhirversion=R5").await), ["r5.thing"]);
    assert_eq!(names(&get("fhirVersion=4.0").await).len(), 2);
    assert_eq!(names(&get("category=test").await).len(), 2);
    assert_eq!(names(&get("country=US").await), ["acme.online"]);
    assert_eq!(names(&get("authority=mohw").await), ["tw.gov.mohw.twcore"]);
    assert_eq!(names(&get("q=r5%20guide").await), ["r5.thing"]);
    assert_eq!(names(&get("q=online&category=").await), ["acme.online"]);
    let paged = get("offset=2&limit=5").await;
    assert_eq!(paged["total"], 3);
    assert_eq!(names(&paged), ["r5.thing"]);
    assert_eq!(get("offset=99").await["results"], json!([]));
}

#[tokio::test]
async fn versions_lookup_validates_names_and_reports_unknown_packages() {
    let base = mock(true).await;
    let (server, _cache) = server(&base);
    let r = server
        .get("/admin/packages/registry/versions/acme.online")
        .await;
    r.assert_status_ok();
    let doc: Value = r.json();
    assert_eq!(doc["latest"], "1.0.0");
    assert_eq!(doc["versions"][0]["version"], "1.0.0");
    assert_eq!(doc["description"], "Acme online IG");

    let r = server.get("/admin/packages/registry/versions/-bad").await;
    r.assert_status(StatusCode::BAD_REQUEST);
    let r = server
        .get("/admin/packages/registry/versions/nobody.knows")
        .await;
    r.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(r.json::<Value>()["issue"][0]["code"], "not-found");
}

#[tokio::test]
async fn an_install_is_fetched_from_the_registry_verified_and_cached() {
    let base = mock(true).await;
    let (server, cache) = server(&base);
    let r = server
        .post("/admin/packages?wait=30")
        .json(&json!({"package": "acme.online#1.0.0"}))
        .await;
    r.assert_status_ok();
    let job: Value = r.json();
    assert_eq!(job["status"], "succeeded", "{job}");
    // The archive is now in the package cache.
    assert!(cache.path().join("acme.online#1.0.0.tgz").exists());
    let c: Value = server.get("/admin/packages/cache").await.json();
    assert_eq!(c["packages"][0]["package"], "acme.online#1.0.0");
    // Listed as installed, and reported so by search and the marketplace.
    let r: Value = server
        .get("/admin/packages/registry/search?q=acme")
        .await
        .json();
    assert_eq!(r["results"][0]["installed"], json!(["1.0.0"]));
    let m: Value = server
        .get("/admin/packages/registry/marketplace?q=acme")
        .await
        .json();
    assert_eq!(m["results"][0]["installed"], json!(["1.0.0"]));
}
