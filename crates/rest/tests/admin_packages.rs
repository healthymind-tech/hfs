//! Integration tests for the FHIR package (IG) admin API (`/admin/packages`).
//!
//! The full app (`create_app_with_config`, so the router is wired exactly as
//! `build_app` wires it) over in-memory SQLite, auth off, the registry off
//! (`HFS_PACKAGE_REGISTRY=off`): every package arrives as an uploaded `.tgz`,
//! which is also the offline-hospital path. No terminology server is
//! configured, so terminology resources land in this server.

use std::io::Write;
use std::path::PathBuf;

use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum_test::TestServer;
use flate2::{Compression, write::GzEncoder};
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use helios_rest::ServerConfig;
use helios_rest::config::{PackageConfig, RegistryMode};
use serde_json::{Value, json};

const CONTENT_TYPE: HeaderName = HeaderName::from_static("content-type");

/// A tar archive written by hand, so hostile member names survive.
fn tar_bytes(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
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
        out.extend_from_slice(&header);
        out.extend_from_slice(body);
        out.extend(std::iter::repeat_n(0u8, (512 - body.len() % 512) % 512));
    }
    out.extend(std::iter::repeat_n(0u8, 1024));
    out
}

fn tgz(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
    gz.write_all(&tar_bytes(members)).unwrap();
    gz.finish().unwrap()
}

/// A package with a manifest and `(path under package/, resource)` members.
fn package(name: &str, version: &str, deps: Value, resources: &[(&str, Value)]) -> Vec<u8> {
    let manifest = json!({
        "name": name, "version": version, "fhirVersions": ["4.0.1"],
        "title": format!("{name} title"), "canonical": "http://acme.org",
        "dependencies": deps,
    })
    .to_string();
    let bodies: Vec<(String, String)> = resources
        .iter()
        .map(|(p, r)| (format!("package/{p}"), r.to_string()))
        .collect();
    let mut members: Vec<(&str, &[u8])> = vec![("package/package.json", manifest.as_bytes())];
    for (p, b) in &bodies {
        members.push((p.as_str(), b.as_bytes()));
    }
    tgz(&members)
}

fn profile(id: &str, version: &str, element: &str) -> Value {
    json!({
        "resourceType": "StructureDefinition", "id": id,
        "url": format!("http://acme.org/StructureDefinition/{id}"), "version": version,
        "name": id.replace('-', ""), "status": "active", "kind": "resource", "abstract": false,
        "type": "Patient", "baseDefinition": "http://hl7.org/fhir/StructureDefinition/Patient",
        "derivation": "constraint",
        "differential": {"element": [
            {"id": "Patient", "path": "Patient"},
            {"id": format!("Patient.{element}"), "path": format!("Patient.{element}"), "min": 1}
        ]}
    })
}

fn base_v1() -> Vec<u8> {
    package(
        "acme.base",
        "1.0.0",
        json!({"hl7.fhir.r4.core": "4.0.1"}),
        &[
            (
                "StructureDefinition-acme-patient.json",
                profile("acme-patient", "1.0.0", "birthDate"),
            ),
            (
                "CodeSystem-acme-codes.json",
                json!({
                    "resourceType": "CodeSystem", "id": "acme-codes", "url": "http://acme.org/CodeSystem/codes",
                    "version": "1.0.0", "status": "active", "content": "complete",
                    "concept": [{"code": "a", "display": "A"}]
                }),
            ),
            (
                "example/Patient-acme-example.json",
                json!({"resourceType": "Patient", "id": "acme-example"}),
            ),
        ],
    )
}

fn base_v2() -> Vec<u8> {
    package(
        "acme.base",
        "2.0.0",
        json!({"hl7.fhir.r4.core": "4.0.1"}),
        &[(
            "StructureDefinition-acme-patient.json",
            profile("acme-patient", "2.0.0", "birthDate"),
        )],
    )
}

fn ext_v1() -> Vec<u8> {
    package(
        "acme.ext",
        "1.0.0",
        json!({"hl7.fhir.r4.core": "4.0.1", "acme.base": "1.0.0"}),
        &[(
            "StructureDefinition-acme-ext-patient.json",
            profile("acme-ext-patient", "1.0.0", "gender"),
        )],
    )
}

fn server(admin: bool) -> (TestServer, tempfile::TempDir) {
    let data_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("data"))
        .unwrap_or_else(|| PathBuf::from("data"));
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
            admin_ui: admin,
            registry: RegistryMode::Off,
            cache_dir: Some(cache.path().to_path_buf()),
            ..PackageConfig::default()
        },
        ..ServerConfig::for_testing()
    };
    let app = helios_rest::create_app_with_config(backend, config);
    (TestServer::new(app).unwrap(), cache)
}

async fn upload(server: &TestServer, bytes: Vec<u8>, query: &str) -> axum_test::TestResponse {
    server
        .post(&format!("/admin/packages?wait=30{query}"))
        .add_header(CONTENT_TYPE, HeaderValue::from_static("application/gzip"))
        .bytes(bytes.into())
        .await
}

async fn install(server: &TestServer, spec: &str) -> Value {
    let response = server
        .post("/admin/packages?wait=30")
        .json(&json!({"package": spec}))
        .await;
    response.assert_status(StatusCode::OK);
    let job: Value = response.json();
    assert_eq!(job["kind"], "install");
    job
}

async fn validate(server: &TestServer, profile_id: &str, patient: Value) -> Vec<Value> {
    let mut patient = patient;
    patient["meta"] =
        json!({"profile": [format!("http://acme.org/StructureDefinition/{profile_id}")]});
    let outcome: Value = server
        .post("/Patient/$validate")
        .add_header(
            CONTENT_TYPE,
            HeaderValue::from_static("application/fhir+json"),
        )
        .json(&patient)
        .await
        .json();
    outcome["issue"].as_array().cloned().unwrap_or_default()
}

fn has_error(issues: &[Value]) -> bool {
    issues.iter().any(|i| i["severity"] == "error")
}

fn unresolved(issues: &[Value]) -> bool {
    issues.iter().any(|i| {
        i.to_string().contains("could not be found") || i.to_string().contains("acme-ext-patient")
    })
}

#[tokio::test]
async fn package_api_is_forbidden_unless_admin_ui_is_on() {
    let (server, _cache) = server(false);
    server
        .get("/admin/packages")
        .await
        .assert_status(StatusCode::FORBIDDEN);
    upload(&server, base_v1(), "")
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // The configuration summary stays readable so the console can explain.
    let config: Value = server.get("/admin/packages/config").await.json();
    assert_eq!(config["adminEnabled"], false);
    assert_eq!(config["registry"], "off");
}

#[tokio::test]
async fn offline_upload_install_validate_uninstall_and_upgrade() {
    let (server, _cache) = server(true);

    // Unsafe and malformed uploads are rejected before anything is kept.
    let evil = tgz(&[
        (
            "package/package.json",
            br#"{"name":"evil","version":"1.0.0"}"#,
        ),
        ("package/../../escape.json", b"{}"),
    ]);
    let r = upload(&server, evil, "").await;
    r.assert_status(StatusCode::BAD_REQUEST);
    assert!(r.text().contains("unsafe path"), "{}", r.text());
    upload(&server, b"not a package".to_vec(), "")
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let cache: Value = server.get("/admin/packages/cache").await.json();
    assert_eq!(cache["packages"].as_array().unwrap().len(), 0);

    // Upload the dependent first: offline, its dependency is reported
    // missing in the plan, not fatal.
    let r = upload(&server, ext_v1(), "").await;
    r.assert_status(StatusCode::OK);
    let plan: Value = r.json();
    assert_eq!(plan["kind"], "plan");
    assert_eq!(plan["status"], "succeeded", "{plan}");
    let nodes = plan["result"]["plan"]["nodes"].as_array().unwrap();
    let base = nodes.iter().find(|n| n["name"] == "acme.base").unwrap();
    assert_eq!(base["action"]["kind"], "missing", "{base}");
    let core = nodes
        .iter()
        .find(|n| n["name"] == "hl7.fhir.r4.core")
        .unwrap();
    assert_eq!(core["action"]["kind"], "embedded");

    // With the dependency uploaded the plan pulls it in, dependencies first,
    // and counts what goes where (the example is skipped).
    let r = upload(&server, base_v1(), "").await;
    r.assert_status(StatusCode::OK);
    let r = server
        .post("/admin/packages?wait=30")
        .json(&json!({"package": "acme.ext#1.0.0", "dryRun": true}))
        .await;
    let plan: Value = r.json();
    assert_eq!(
        plan["result"]["plan"]["installOrder"],
        json!(["acme.base#1.0.0", "acme.ext#1.0.0"])
    );
    let base_report = plan["result"]["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["package"] == "acme.base#1.0.0")
        .unwrap()
        .clone();
    assert_eq!(base_report["counts"]["hfs"]["StructureDefinition"], 1);
    assert_eq!(base_report["counts"]["hfs"]["CodeSystem"], 1);
    assert_eq!(base_report["counts"]["examplesSkipped"]["Patient"], 1);

    // Install the dependent: both packages land.
    let job = install(&server, "acme.ext#1.0.0").await;
    assert_eq!(job["status"], "succeeded", "{job}");
    assert_eq!(
        job["result"]["installed"],
        json!(["acme.base#1.0.0", "acme.ext#1.0.0"])
    );
    let list: Value = server.get("/admin/packages").await.json();
    let packages = list["packages"].as_array().unwrap();
    assert_eq!(packages.len(), 2);
    let base = packages.iter().find(|p| p["name"] == "acme.base").unwrap();
    assert_eq!(base["dependents"], json!(["acme.ext#1.0.0"]));
    assert_eq!(base["explicit"], false);
    let ext = packages.iter().find(|p| p["name"] == "acme.ext").unwrap();
    assert_eq!(ext["resolved"]["acme.base"], "acme.base#1.0.0");
    assert_eq!(ext["resolved"]["hl7.fhir.r4.core"], "embedded");
    assert_eq!(ext["explicit"], true);

    // The profiles validate immediately; the example was not imported.
    assert!(has_error(
        &validate(&server, "acme-patient", json!({"resourceType": "Patient"})).await
    ));
    assert!(!has_error(
        &validate(
            &server,
            "acme-patient",
            json!({"resourceType": "Patient", "birthDate": "2000-01-01"})
        )
        .await
    ));
    assert!(has_error(
        &validate(
            &server,
            "acme-ext-patient",
            json!({"resourceType": "Patient"})
        )
        .await
    ));
    server
        .get("/Patient/acme-example")
        .await
        .assert_status(StatusCode::NOT_FOUND);

    // Detail lists resources by type.
    let detail: Value = server.get("/admin/packages/acme.base/1.0.0").await.json();
    assert_eq!(
        detail["resources"]["StructureDefinition"][0]["id"],
        "acme-patient"
    );
    assert_eq!(detail["resourceSummary"]["CodeSystem"]["hfs"], 1);
    server
        .get("/admin/packages/acme.base/9.9.9")
        .await
        .assert_status(StatusCode::NOT_FOUND);

    // A dependency cannot be removed while its dependent is installed.
    let r = server.delete("/admin/packages/acme.base/1.0.0").await;
    r.assert_status(StatusCode::CONFLICT);
    assert!(r.text().contains("acme.ext#1.0.0"), "{}", r.text());

    // Removing the dependent deletes its profile only.
    let r = server
        .delete("/admin/packages/acme.ext/1.0.0?wait=30")
        .await;
    r.assert_status(StatusCode::OK);
    let job: Value = r.json();
    assert_eq!(job["status"], "succeeded", "{job}");
    assert_eq!(job["result"]["deleted"]["StructureDefinition"], 1);
    let gone = server.get("/StructureDefinition/acme-ext-patient").await;
    assert!(
        matches!(gone.status_code(), StatusCode::GONE | StatusCode::NOT_FOUND),
        "{}",
        gone.status_code()
    );
    let issues = validate(
        &server,
        "acme-ext-patient",
        json!({"resourceType": "Patient"}),
    )
    .await;
    assert!(!has_error(&issues) && unresolved(&issues), "{issues:?}");
    // ...and the other package is unaffected.
    assert!(has_error(
        &validate(&server, "acme-patient", json!({"resourceType": "Patient"})).await
    ));
    server
        .get("/StructureDefinition/acme-patient")
        .await
        .assert_status(StatusCode::OK);

    // Upgrade: 2.0.0 replaces 1.0.0 and retires what 2.0.0 no longer ships.
    upload(&server, base_v2(), "")
        .await
        .assert_status(StatusCode::OK);
    let job = install(&server, "acme.base#2.0.0").await;
    assert_eq!(job["status"], "succeeded", "{job}");
    assert_eq!(
        job["result"]["plan"]["nodes"][0]["action"],
        json!({"kind": "upgrade", "from": ["1.0.0"]})
    );
    let list: Value = server.get("/admin/packages").await.json();
    let versions: Vec<&str> = list["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["version"].as_str().unwrap())
        .collect();
    assert_eq!(versions, ["2.0.0"]);
    let sd: Value = server.get("/StructureDefinition/acme-patient").await.json();
    assert_eq!(sd["version"], "2.0.0");
    let cs = server.get("/CodeSystem/acme-codes").await;
    assert!(
        matches!(cs.status_code(), StatusCode::GONE | StatusCode::NOT_FOUND),
        "the replaced version's leftovers are removed"
    );
    assert!(has_error(
        &validate(&server, "acme-patient", json!({"resourceType": "Patient"})).await
    ));

    // Remove the last package: its profile stops resolving.
    let job: Value = server
        .delete("/admin/packages/acme.base/2.0.0?wait=30")
        .await
        .json();
    assert_eq!(job["status"], "succeeded", "{job}");
    let issues = validate(&server, "acme-patient", json!({"resourceType": "Patient"})).await;
    assert!(!has_error(&issues), "{issues:?}");
    let list: Value = server.get("/admin/packages").await.json();
    assert!(list["packages"].as_array().unwrap().is_empty());

    // Every install and uninstall left an AuditEvent.
    let audit: Value = server.get("/AuditEvent?_count=100").await.json();
    let descriptions: Vec<String> = audit["entry"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|e| e["resource"]["outcomeDesc"].as_str().map(str::to_string))
        .collect();
    for needle in [
        "Installed FHIR package acme.base#1.0.0",
        "Installed FHIR package acme.ext#1.0.0",
        "Uninstalled FHIR package acme.ext#1.0.0",
        "Removed FHIR package acme.base#1.0.0",
        "Uninstalled FHIR package acme.base#2.0.0",
    ] {
        assert!(
            descriptions.iter().any(|d| d.starts_with(needle)),
            "{needle} not in {descriptions:?}"
        );
    }

    // Jobs are listed, newest first.
    let jobs: Value = server.get("/admin/packages/jobs").await.json();
    assert!(jobs["jobs"].as_array().unwrap().len() >= 6);
    server
        .get("/admin/packages/jobs/no-such-job")
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn invalid_requests_are_rejected() {
    let (server, _cache) = server(true);
    server
        .post("/admin/packages")
        .json(&json!({"package": "../etc#1"}))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post("/admin/packages")
        .json(&json!({}))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post("/admin/packages")
        .add_header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
        .bytes("not json".into())
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    // Offline and not cached: the plan job fails with the reason.
    let job: Value = server
        .post("/admin/packages?wait=30")
        .json(&json!({"package": "hl7.fhir.us.core#6.1.0", "dryRun": true}))
        .await
        .json();
    assert_eq!(job["status"], "failed");
    assert!(
        job["result"]["error"]
            .as_str()
            .unwrap()
            .contains("registry is off"),
        "{job}"
    );
    // Registry lookups need the registry.
    let search: Value = server
        .get("/admin/packages/registry/search?q=tw")
        .await
        .json();
    assert!(
        search["error"].as_str().unwrap_or_default().contains("off"),
        "{search}"
    );
    let market: Value = server
        .get("/admin/packages/registry/marketplace")
        .await
        .json();
    assert_eq!(market["available"], false);
}

/// Two IGs that use the same id for different resources (TW Core and EMR
/// both ship a `SearchParameter/Condition-code`) never overwrite each other:
/// the second gets an id derived from its canonical, and removing it leaves
/// the first intact.
#[tokio::test]
async fn id_clash_with_a_different_canonical_gets_its_own_id() {
    let (server, _cache) = server(true);
    upload(&server, base_v1(), "")
        .await
        .assert_status(StatusCode::OK);
    install(&server, "acme.base#1.0.0").await;
    let mut other = profile("acme-patient", "1.0.0", "gender");
    other["url"] = json!("http://other.org/StructureDefinition/acme-patient");
    let pkg = package(
        "other.ig",
        "1.0.0",
        json!({}),
        &[("StructureDefinition-acme-patient.json", other)],
    );
    upload(&server, pkg, "").await.assert_status(StatusCode::OK);
    let plan: Value = server
        .post("/admin/packages?wait=30")
        .json(&json!({"package": "other.ig#1.0.0", "dryRun": true}))
        .await
        .json();
    assert_eq!(
        plan["result"]["packages"][0]["conflicts"]["counts"]["renamed-clash"], 1,
        "{plan}"
    );
    let job = install(&server, "other.ig#1.0.0").await;
    assert_eq!(job["status"], "succeeded", "{job}");
    let detail: Value = server.get("/admin/packages/other.ig/1.0.0").await.json();
    let new_id = detail["resources"]["StructureDefinition"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        new_id.starts_with("acme-patient-") && new_id.len() == "acme-patient".len() + 9,
        "{new_id}"
    );
    let original: Value = server.get("/StructureDefinition/acme-patient").await.json();
    assert_eq!(
        original["url"],
        "http://acme.org/StructureDefinition/acme-patient"
    );
    let renamed: Value = server
        .get(&format!("/StructureDefinition/{new_id}"))
        .await
        .json();
    assert_eq!(
        renamed["url"],
        "http://other.org/StructureDefinition/acme-patient"
    );
    let job: Value = server
        .delete("/admin/packages/other.ig/1.0.0?wait=30")
        .await
        .json();
    assert_eq!(job["status"], "succeeded");
    server
        .get("/StructureDefinition/acme-patient")
        .await
        .assert_status(StatusCode::OK);
    assert!(has_error(
        &validate(&server, "acme-patient", json!({"resourceType": "Patient"})).await
    ));
}

/// A terminology server stand-in: `POST /import` keeps CodeSystem /
/// ValueSet / ConceptMap by (type, url, version), `GET /{type}?url=&version=`
/// searches, `DELETE /{type}/{id}` removes. Returns its base URL and store.
async fn mock_hts() -> (String, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
    use axum::{
        Json, Router,
        extract::{Path, Query, State},
        routing::{delete, get, post},
    };
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    type Store = Arc<Mutex<Vec<Value>>>;
    async fn import(State(store): State<Store>, Json(bundle): Json<Value>) -> Json<Value> {
        let mut s = store.lock().unwrap();
        let mut n = 0;
        for e in bundle["entry"].as_array().cloned().unwrap_or_default() {
            let r = e["resource"].clone();
            s.retain(|x| {
                !(x["resourceType"] == r["resourceType"]
                    && x["url"] == r["url"]
                    && x["version"] == r["version"])
            });
            s.push(r);
            n += 1;
        }
        Json(json!({"code_systems": n, "value_sets": 0, "concept_maps": 0, "concepts": 0}))
    }
    async fn search(
        State(store): State<Store>,
        Path(t): Path<String>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        let s = store.lock().unwrap();
        let entry: Vec<Value> = s
            .iter()
            .filter(|r| r["resourceType"] == t.as_str())
            .filter(|r| q.get("url").is_none_or(|u| r["url"] == u.as_str()))
            .filter(|r| q.get("version").is_none_or(|v| r["version"] == v.as_str()))
            .map(|r| json!({"resource": r}))
            .collect();
        Json(
            json!({"resourceType": "Bundle", "type": "searchset", "total": entry.len(), "entry": entry}),
        )
    }
    async fn remove(
        State(store): State<Store>,
        Path((t, id)): Path<(String, String)>,
    ) -> axum::http::StatusCode {
        store
            .lock()
            .unwrap()
            .retain(|r| !(r["resourceType"] == t.as_str() && r["id"] == id.as_str()));
        axum::http::StatusCode::NO_CONTENT
    }
    let store: Store = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/import", post(import))
        .route("/{t}", get(search))
        .route("/{t}/{id}", delete(remove))
        .with_state(Arc::clone(&store));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), store)
}

/// With a terminology server configured, CodeSystems go there (not into
/// HFS), uninstall deletes them there, and terminology that was already
/// loaded before the install is left alone.
#[tokio::test]
async fn terminology_goes_to_the_terminology_server_and_is_removed_from_it() {
    let (hts, store) = mock_hts().await;
    // Already on the terminology server before any package.
    store.lock().unwrap().push(json!({
        "resourceType": "CodeSystem", "id": "shared", "url": "http://acme.org/CodeSystem/shared",
        "version": "1.0.0", "status": "active", "content": "complete"
    }));
    let backend = SqliteBackend::in_memory().expect("sqlite");
    backend.init_schema().expect("schema");
    let cache = tempfile::tempdir().unwrap();
    let config = ServerConfig {
        seed_conformance: false,
        request_timeout: 60,
        terminology_server: Some(hts),
        packages: PackageConfig {
            admin_ui: true,
            registry: RegistryMode::Off,
            cache_dir: Some(cache.path().to_path_buf()),
            ..PackageConfig::default()
        },
        ..ServerConfig::for_testing()
    };
    let server = TestServer::new(helios_rest::create_app_with_config(backend, config)).unwrap();
    let cs = |id: &str| {
        json!({
            "resourceType": "CodeSystem", "id": id, "url": format!("http://acme.org/CodeSystem/{id}"),
            "version": "1.0.0", "status": "active", "content": "complete", "concept": [{"code": "a"}]
        })
    };
    let pkg = package(
        "acme.terms",
        "1.0.0",
        json!({}),
        &[
            ("CodeSystem-own.json", cs("own")),
            ("CodeSystem-shared.json", cs("shared")),
            (
                "StructureDefinition-acme-patient.json",
                profile("acme-patient", "1.0.0", "birthDate"),
            ),
        ],
    );
    upload(&server, pkg, "").await.assert_status(StatusCode::OK);
    let job = install(&server, "acme.terms#1.0.0").await;
    assert_eq!(job["status"], "succeeded", "{job}");
    assert_eq!(
        store.lock().unwrap().len(),
        2,
        "own + shared on the terminology server"
    );
    server
        .get("/CodeSystem/own")
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let detail: Value = server.get("/admin/packages/acme.terms/1.0.0").await.json();
    let rows = detail["resources"]["CodeSystem"]
        .as_array()
        .unwrap()
        .clone();
    assert!(rows.iter().all(|r| r["target"] == "hts"), "{rows:?}");
    assert!(
        rows.iter()
            .any(|r| r["url"] == "http://acme.org/CodeSystem/shared" && r["preexisting"] == true)
    );

    let job: Value = server
        .delete("/admin/packages/acme.terms/1.0.0?wait=30")
        .await
        .json();
    assert_eq!(job["status"], "succeeded", "{job}");
    let left: Vec<Value> = store.lock().unwrap().clone();
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0]["id"], "shared", "pre-existing terminology stays");
    assert_eq!(job["result"]["keptPreexisting"], 1);
}
