//! Online paths of the package registry, against an in-process mock of an
//! npm-style FHIR package registry: registry documents, download with
//! SHA-1 verification and fallback URLs, archive identity checks, size
//! limits, mirror fallback, catalog search and the marketplace caches.

use super::*;
use crate::packages::tarball::test_support::package;
use axum::{
    Json, Router,
    extract::{Path as AxPath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::json;
use std::collections::HashMap as Map;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone)]
struct Mock {
    /// `{name}` → registry document.
    docs: Arc<Map<String, Value>>,
    /// `{name}/{version}` or a custom path → archive bytes.
    files: Arc<Map<String, Vec<u8>>>,
    catalog: Arc<Value>,
    market: Arc<Value>,
    market_hits: Arc<AtomicUsize>,
}

async fn doc_or_file(State(m): State<Mock>, AxPath(rest): AxPath<String>) -> Response {
    if let Some(bytes) = m.files.get(&rest) {
        return bytes.clone().into_response();
    }
    if rest == "catalog" {
        return Json((*m.catalog).clone()).into_response();
    }
    match m.docs.get(&rest) {
        Some(doc) => Json(doc.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn catalog(State(m): State<Mock>, Query(q): Query<Map<String, String>>) -> Response {
    // Echo the query so the test can check what was sent.
    let mut items = m.catalog.as_array().cloned().unwrap_or_default();
    items.push(json!({"name": "echo.query", "description": format!(
        "name={} fhirversion={}",
        q.get("name").cloned().unwrap_or_default(),
        q.get("fhirversion").cloned().unwrap_or_default()
    )}));
    Json(Value::Array(items)).into_response()
}

async fn market(State(m): State<Mock>) -> Response {
    m.market_hits.fetch_add(1, Ordering::SeqCst);
    Json((*m.market).clone()).into_response()
}

/// Starts the mock and returns its base URL.
async fn serve(mock: Mock) -> String {
    let app = Router::new()
        .route("/catalog", get(catalog))
        .route("/market.json", get(market))
        .route("/{*rest}", get(doc_or_file))
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn registry(dir: &Path, bases: Vec<String>, market: Option<String>) -> Registry {
    Registry::new(
        RegistryMode::Mirrors(bases),
        market,
        dir.to_path_buf(),
        Limits::default(),
    )
}

struct Fixture {
    base: String,
    good: Vec<u8>,
    market_hits: Arc<AtomicUsize>,
}

/// A registry with:
/// - `acme.ig` 1.0.0 (listed tarball + correct shasum), 2.0.0 (wrong
///   shasum on the listed tarball; the direct URL serves the same bytes),
///   3.0.0 (archive claims to be another package), 4.0.0 (listed only, no
///   archive anywhere)
async fn fixture() -> Fixture {
    let good = package("acme.ig", "1.0.0", &[], &[]);
    let v2 = package("acme.ig", "2.0.0", &[], &[]);
    let impostor = package("other.ig", "3.0.0", &[], &[]);
    let docs = Map::from([(
        "acme.ig".to_string(),
        json!({
            "name": "acme.ig",
            "description": "Acme IG",
            "dist-tags": {"latest": "2.0.0"},
            "versions": {
                "1.0.0": {"fhirVersion": "R4", "dist": {
                    "tarball": "PLACEHOLDER/files/acme-1.tgz",
                    "shasum": sha1_hex(&good)}},
                "2.0.0": {"fhirVersion": "R4", "dist": {
                    "tarball": "PLACEHOLDER/files/acme-2.tgz",
                    "shasum": "0000000000000000000000000000000000000000"}},
                "3.0.0": {"dist": {"tarball": "PLACEHOLDER/files/acme-3.tgz"}},
                "4.0.0": {}
            }
        }),
    )]);
    let files = Map::from([
        ("files/acme-1.tgz".to_string(), good.clone()),
        ("files/acme-2.tgz".to_string(), v2.clone()),
        ("acme.ig/2.0.0".to_string(), v2.clone()),
        ("files/acme-3.tgz".to_string(), impostor),
    ]);
    let market_hits = Arc::new(AtomicUsize::new(0));
    let mock = Mock {
        docs: Arc::new(docs),
        files: Arc::new(files),
        catalog: Arc::new(json!([
            {"name": "acme.ig", "version": "2.0.0", "fhirVersion": "4.0.1"}
        ])),
        market: Arc::new(json!({"guides": [
            {"name": "Acme", "npm-name": "acme.ig", "category": "Test", "country": "TW",
             "editions": [{"name": "1", "ig-version": "1.0.0", "package": "acme.ig#1.0.0"}]}
        ]})),
        market_hits: market_hits.clone(),
    };
    // The documents need the server's own address in their tarball URLs:
    // serve once to learn the port, then rewrite.
    let base = serve(mock.clone()).await;
    let docs: Map<String, Value> = mock
        .docs
        .iter()
        .map(|(k, v)| {
            let text = v.to_string().replace("PLACEHOLDER", &base);
            (k.clone(), serde_json::from_str(&text).unwrap())
        })
        .collect();
    let base = serve(Mock {
        docs: Arc::new(docs),
        ..mock
    })
    .await;
    // The rewritten documents point at the first server, which serves the
    // same files, so either base works for archives.
    Fixture {
        base,
        good,
        market_hits,
    }
}

#[tokio::test]
async fn a_listed_archive_is_downloaded_verified_and_cached() {
    let fx = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let reg = registry(dir.path(), vec![fx.base.clone()], None);
    let (bytes, info) = reg.fetch("acme.ig", "1.0.0").await.unwrap();
    assert_eq!(bytes, fx.good);
    assert!(info.verified, "the registry published a shasum");
    assert!(
        info.source.ends_with("/files/acme-1.tgz"),
        "{}",
        info.source
    );
    // Now served from the cache.
    assert_eq!(reg.cached_versions("acme.ig"), ["1.0.0"]);
    let (_, info) = reg.fetch("acme.ig", "1.0.0").await.unwrap();
    assert_eq!(info.source, "cache");
}

#[tokio::test]
async fn a_shasum_mismatch_rejects_every_url_with_that_listing() {
    let fx = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let reg = registry(dir.path(), vec![fx.base.clone()], None);
    // Both the listed tarball and the direct URL are checked against the
    // listed shasum, so neither is accepted.
    let err = reg.fetch("acme.ig", "2.0.0").await.unwrap_err();
    assert!(err.contains("shasum mismatch"), "{err}");
    assert!(err.contains("/files/acme-2.tgz"), "{err}");
    assert!(err.contains("/acme.ig/2.0.0"), "{err}");
    assert!(reg.cached_versions("acme.ig").is_empty(), "nothing cached");
}

#[tokio::test]
async fn an_archive_of_another_package_is_refused() {
    let fx = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let reg = registry(dir.path(), vec![fx.base.clone()], None);
    let err = reg.fetch("acme.ig", "3.0.0").await.unwrap_err();
    assert!(
        err.contains("archive is other.ig#3.0.0 not acme.ig#3.0.0"),
        "{err}"
    );
}

#[tokio::test]
async fn a_missing_archive_reports_each_url_tried() {
    let fx = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let reg = registry(dir.path(), vec![fx.base.clone()], None);
    let err = reg.fetch("acme.ig", "4.0.0").await.unwrap_err();
    assert!(err.starts_with("could not download acme.ig#4.0.0"), "{err}");
    assert!(err.contains("HTTP 404"), "{err}");
    // Invalid names never reach the network.
    let err = reg.fetch("../evil", "1.0.0").await.unwrap_err();
    assert_eq!(err, "invalid package name or version");
}

#[tokio::test]
async fn archives_over_the_size_limit_are_refused() {
    let fx = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let reg = Registry::new(
        RegistryMode::Mirrors(vec![fx.base.clone()]),
        None,
        dir.path().to_path_buf(),
        Limits {
            max_compressed: 16,
            ..Limits::default()
        },
    );
    let err = reg.fetch("acme.ig", "1.0.0").await.unwrap_err();
    assert!(
        err.contains("larger than the 16-byte package limit"),
        "{err}"
    );
}

#[tokio::test]
async fn a_dead_mirror_falls_through_to_the_next() {
    let fx = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    // Nothing listens on port 9 (discard) on loopback in CI.
    let reg = registry(
        dir.path(),
        vec!["http://127.0.0.1:9".to_string(), fx.base.clone()],
        None,
    );
    let doc = reg.doc("acme.ig").await.unwrap();
    assert_eq!(doc.source, fx.base);
    assert_eq!(doc.latest.as_deref(), Some("2.0.0"));
    assert_eq!(doc.description.as_deref(), Some("Acme IG"));
    let (bytes, _) = reg.fetch("acme.ig", "1.0.0").await.unwrap();
    assert_eq!(bytes, fx.good);
}

#[tokio::test]
async fn registry_documents_merge_cached_versions_and_report_unknown_names() {
    let fx = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let reg = registry(dir.path(), vec![fx.base.clone()], None);
    // A hand-dropped cached version the registry does not list.
    reg.store(&package("acme.ig", "0.9.0", &[], &[])).unwrap();
    let doc = reg.doc("acme.ig").await.unwrap();
    let versions: Vec<&str> = doc.versions.iter().map(|v| v.version.as_str()).collect();
    assert_eq!(versions, ["4.0.0", "3.0.0", "2.0.0", "1.0.0", "0.9.0"]);

    let err = reg.doc("nobody.knows").await.unwrap_err();
    assert!(err.starts_with("nobody.knows not found"), "{err}");
    assert!(err.contains("HTTP 404"), "{err}");

    // Only cached versions and every registry failing: the cache answers.
    reg.store(&package("local.only", "1.2.3", &[], &[]))
        .unwrap();
    let doc = reg.doc("local.only").await.unwrap();
    assert_eq!(doc.source, "cache");
    assert_eq!(doc.latest.as_deref(), Some("1.2.3"));
}

#[tokio::test]
async fn catalog_search_sends_name_and_fhir_version() {
    let fx = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let reg = registry(dir.path(), vec![fx.base.clone()], None);
    let hits = reg.search("acme", Some("4.0.1")).await.unwrap();
    let echo = hits.iter().find(|e| e.name == "echo.query").unwrap();
    assert_eq!(
        echo.description.as_deref(),
        Some("name=acme fhirversion=4.0.1")
    );
    assert!(hits.iter().any(|e| e.name == "acme.ig"));
    // All registries down.
    let dead = registry(dir.path(), vec!["http://127.0.0.1:9".to_string()], None);
    assert!(dead.search("acme", None).await.is_err());
}

#[tokio::test]
async fn the_marketplace_is_cached_in_memory_and_on_disk() {
    let fx = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("{}/market.json", fx.base);
    let reg = registry(dir.path(), vec![fx.base.clone()], Some(url.clone()));
    assert!(reg.marketplace_enabled());
    let list = reg.marketplace().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].country, "tw");
    assert_eq!(fx.market_hits.load(Ordering::SeqCst), 1);
    // Memory hit.
    reg.marketplace().await.unwrap();
    assert_eq!(fx.market_hits.load(Ordering::SeqCst), 1);
    // A new registry over the same cache dir reads the fresh disk copy.
    let again = registry(dir.path(), vec![fx.base.clone()], Some(url));
    assert_eq!(again.marketplace().await.unwrap().len(), 1);
    assert_eq!(fx.market_hits.load(Ordering::SeqCst), 1);
    // Offline: the disk copy is all there is.
    let offline = Registry::new(
        RegistryMode::Off,
        Some("http://127.0.0.1:9/market.json".into()),
        dir.path().to_path_buf(),
        Limits::default(),
    );
    assert_eq!(offline.marketplace().await.unwrap().len(), 1);
    // Off with no copy: an error naming the file.
    let empty = tempfile::tempdir().unwrap();
    let offline_empty = Registry::new(
        RegistryMode::Off,
        Some("http://127.0.0.1:9/market.json".into()),
        empty.path().to_path_buf(),
        Limits::default(),
    );
    let err = offline_empty.marketplace().await.unwrap_err();
    assert!(err.contains("no marketplace index is cached"), "{err}");
    // Disabled.
    let disabled = registry(dir.path(), vec![], None);
    assert!(!disabled.marketplace_enabled());
    assert!(disabled.marketplace().await.is_err());
}

#[tokio::test]
async fn an_empty_or_unreachable_marketplace_is_an_error_without_a_disk_copy() {
    let fx = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    // The registry document endpoint answers JSON with no `guides`.
    let reg = registry(
        dir.path(),
        vec![fx.base.clone()],
        Some(format!("{}/acme.ig", fx.base)),
    );
    let err = reg.marketplace().await.unwrap_err();
    assert!(err.contains("no guides in the marketplace index"), "{err}");
    let down = registry(
        dir.path(),
        vec![fx.base.clone()],
        Some("http://127.0.0.1:9/market.json".into()),
    );
    assert!(down.marketplace().await.is_err());
}
