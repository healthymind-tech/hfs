//! Shared test helpers for HTS integration tests.
#![allow(dead_code)] // items are shared across test binaries; not all are used in each

pub mod bundles;

#[cfg(feature = "sqlite")]
use helios_hts::{backends::SqliteTerminologyBackend, config::HtsConfig, state::AppState};

#[cfg(feature = "postgres")]
use helios_hts::{config::HtsConfig as PgHtsConfig, state::AppState as PgAppState};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::Value;
use tower::ServiceExt;

// ── PostgreSQL shared container for HTTP tests ─────────────────────────────────

#[cfg(feature = "postgres")]
static PG_HTTP_CONTAINER: std::sync::OnceLock<
    testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
> = std::sync::OnceLock::new();

#[cfg(feature = "postgres")]
static PG_HTTP_URL: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();

/// Force-remove the PostgreSQL testcontainer started by this test binary when
/// the process exits. `static` values are never dropped, so `ContainerAsync`'s
/// async cleanup never runs; this atexit hook does it synchronously via the
/// `docker` CLI instead.
#[cfg(feature = "postgres")]
#[ctor::dtor]
fn cleanup_http_pg_container() {
    let Some(container) = PG_HTTP_CONTAINER.get() else {
        return;
    };
    let _ = std::process::Command::new("docker")
        .args(["rm", "-f", container.id()])
        .output();
}

/// Start a Postgres testcontainer, retrying transient failures.
///
/// Why: the testcontainers postgres image's wait-for-log strategy occasionally
/// hits `EndOfStream` before the "ready" line on hosts where initdb emits a
/// `locale: not found` warning during bootstrap. Retry a few times before
/// giving up so a single unlucky first-caller doesn't fail an otherwise green
/// suite.
#[cfg(feature = "postgres")]
async fn start_http_postgres_for_test()
-> testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres> {
    use testcontainers::{ImageExt, runners::AsyncRunner};
    use testcontainers_modules::postgres::Postgres;

    let mut last_err = None;
    for attempt in 1..=3u32 {
        // Pin the major version. testcontainers-modules defaults to
        // postgres:11, which is EOL and predates `plan_cache_mode` — a GUC
        // the backend sends as a startup option, so PG 11 rejects every
        // connection FATAL. The rest of the repo runs 16.
        match Postgres::default()
            .with_tag("16-alpine")
            .with_cmd(["-c", "fsync=off", "-c", "max_connections=2000"])
            .start()
            .await
        {
            Ok(c) => return c,
            Err(e) => {
                eprintln!("postgres container start attempt {attempt}/3 failed: {e:?}");
                last_err = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
    panic!(
        "Failed to start Postgres container for HTTP tests after 3 attempts: {:?}",
        last_err.unwrap()
    )
}

/// Returns the PostgreSQL URL for the shared HTTP-test container, starting it
/// on the first call. Shared across all `TestAppPg` instances in a test binary.
#[cfg(feature = "postgres")]
pub async fn pg_http_url() -> &'static str {
    PG_HTTP_URL
        .get_or_init(|| async {
            // The static keeps the container alive until process exit. Its
            // destructor removes this container by ID because statics are not dropped.
            let container = start_http_postgres_for_test().await;

            let port = container
                .get_host_port_ipv4(5432)
                .await
                .expect("Failed to get postgres port");

            let host = container
                .get_host()
                .await
                .expect("Failed to get postgres host");

            let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
            let _ = PG_HTTP_CONTAINER.set(container);
            url
        })
        .await
}

/// `true` when the generic HTTP tests should run against PostgreSQL:
/// `HTS_TEST_BACKEND=postgres` (needs the `postgres` feature and Docker, or
/// `HTS_TEST_PG_URL` pointing at a server the tests may create databases on).
pub fn pg_mode() -> bool {
    cfg!(feature = "postgres")
        && std::env::var("HTS_TEST_BACKEND").is_ok_and(|v| v.eq_ignore_ascii_case("postgres"))
}

/// Base URL (without database name) of the server the per-test PostgreSQL
/// databases are created on.
#[cfg(feature = "postgres")]
async fn pg_server_base() -> String {
    let url = match std::env::var("HTS_TEST_PG_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => pg_http_url().await.to_owned(),
    };
    match url.rfind('/') {
        Some(i) if i > "postgres://".len() => url[..i].to_owned(),
        _ => url,
    }
}

#[cfg(feature = "postgres")]
static PG_TEMPLATE: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();

/// Run one statement on the server's maintenance database.
#[cfg(feature = "postgres")]
async fn pg_admin_exec(base: &str, sql: &str) -> Result<(), tokio_postgres::Error> {
    let (client, conn) =
        tokio_postgres::connect(&format!("{base}/postgres"), tokio_postgres::NoTls).await?;
    let driver = tokio::spawn(conn);
    let res = client.batch_execute(sql).await;
    drop(client);
    let _ = driver.await;
    res
}

/// A fresh database holding the HTS and resource-store schemas, cloned from a
/// per-process template so each test starts empty and isolated.
#[cfg(feature = "postgres")]
pub async fn fresh_pg_database() -> String {
    let base = pg_server_base().await;
    let template = PG_TEMPLATE
        .get_or_init(|| async {
            use helios_hts::backends::PostgresTerminologyBackend;
            use helios_persistence::backends::postgres::PostgresBackend;
            let name = format!("hts_tpl_{}", std::process::id());
            pg_admin_exec(&base, &format!("DROP DATABASE IF EXISTS {name}"))
                .await
                .expect("drop stale template");
            pg_admin_exec(&base, &format!("CREATE DATABASE {name}"))
                .await
                .expect("create template database");
            let url = format!("{base}/{name}");
            {
                let backend = PostgresTerminologyBackend::new(&url)
                    .await
                    .expect("HTS schema on template");
                let store = PostgresBackend::from_connection_string(&url)
                    .await
                    .expect("resource store on template");
                store.init_schema().await.expect("resource store schema");
                backend.pool().close();
                drop(store);
            }
            name
        })
        .await;
    let db = format!("hts_t_{}", uuid::Uuid::new_v4().simple());
    // The template must have no open sessions; the ones used to build it may
    // take a moment to go away.
    let mut last = None;
    for _ in 0..100 {
        match pg_admin_exec(&base, &format!("CREATE DATABASE {db} TEMPLATE {template}")).await {
            Ok(()) => return format!("{base}/{db}"),
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
    panic!("could not create test database from template: {last:?}");
}

/// A lightweight test application that wraps the full HTS Axum router backed
/// by an isolated store: an in-memory SQLite database, or — with
/// `HTS_TEST_BACKEND=postgres` — a fresh PostgreSQL database per instance.
pub struct TestApp {
    router: Router,
}

impl TestApp {
    /// Create a new `TestApp` with a fresh, empty store.
    pub async fn new() -> Self {
        #[cfg(feature = "postgres")]
        if pg_mode() {
            return Self::new_pg(false).await;
        }
        #[cfg(feature = "sqlite")]
        {
            let backend = SqliteTerminologyBackend::in_memory()
                .expect("failed to create in-memory HTS backend");
            let state = AppState::new(backend);
            let config = HtsConfig::default();
            let router = helios_hts::server::create_app(&config, state);
            TestApp { router }
        }
        #[cfg(not(feature = "sqlite"))]
        panic!("no backend: build with `sqlite`, or set HTS_TEST_BACKEND=postgres")
    }

    /// Like [`Self::new`] but wired the way `hts run` wires production: with a
    /// persistence resource store and the HTS re-index path, so the REST CRUD
    /// endpoints (`POST`/`PUT`/`DELETE /CodeSystem/{id}` …) are live.
    pub async fn with_resource_store() -> Self {
        #[cfg(feature = "postgres")]
        if pg_mode() {
            return Self::new_pg(true).await;
        }
        #[cfg(feature = "sqlite")]
        {
            use helios_persistence::backends::sqlite::SqliteBackend;
            let backend = SqliteTerminologyBackend::in_memory()
                .expect("failed to create in-memory HTS backend");
            let hts_pool = backend.pool().clone();
            let resource_store = SqliteBackend::in_memory().expect("persistence in-memory backend");
            resource_store
                .init_schema()
                .expect("persistence schema init");
            let state = AppState::new(backend)
                .with_resource_store(resource_store)
                .with_hts_pool(hts_pool);
            let config = HtsConfig::default();
            let router = helios_hts::server::create_app(&config, state);
            TestApp { router }
        }
        #[cfg(not(feature = "sqlite"))]
        panic!("no backend: build with `sqlite`, or set HTS_TEST_BACKEND=postgres")
    }

    /// A PostgreSQL-backed app on a fresh database, wired like
    /// `run_server_postgres` (resource store + importer) when `crud` is set.
    #[cfg(feature = "postgres")]
    async fn new_pg(crud: bool) -> Self {
        use helios_hts::backends::PostgresTerminologyBackend;
        use helios_persistence::backends::postgres::PostgresBackend;
        let url = fresh_pg_database().await;
        let backend = PostgresTerminologyBackend::new(&url)
            .await
            .expect("Failed to create PostgresTerminologyBackend");
        let mut state = PgAppState::new(backend.clone());
        if crud {
            let resource_store = PostgresBackend::from_connection_string(&url)
                .await
                .expect("Failed to open PostgreSQL resource store");
            state = state
                .with_resource_store_pg(resource_store)
                .with_terminology_importer(std::sync::Arc::new(backend));
        }
        let config = PgHtsConfig {
            storage_backend: "postgres".into(),
            database_url: url,
            ..PgHtsConfig::default()
        };
        let router = helios_hts::server::create_app(&config, state);
        TestApp { router }
    }

    /// PUT a FHIR JSON body to `path`, returning `(status, response_body)`.
    pub async fn put_fhir(&self, path: &str, body: impl Into<String>) -> (StatusCode, Value) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(path)
                    .header("content-type", "application/fhir+json")
                    .body(Body::from(body.into()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    /// DELETE `path`, returning the status code.
    pub async fn delete_fhir(&self, path: &str) -> StatusCode {
        self.router
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    /// POST a FHIR JSON body to `path`, returning `(status, response_body)`.
    pub async fn post_fhir(&self, path: &str, body: impl Into<String>) -> (StatusCode, Value) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/fhir+json")
                    .body(Body::from(body.into()))
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    /// GET `path`, returning `(status, response_body)`.
    pub async fn get_fhir(&self, path: &str) -> (StatusCode, Value) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    /// POST a FHIR JSON body to `path` requesting XML, returning `(status, body_string)`.
    pub async fn post_fhir_xml(&self, path: &str, body: impl Into<String>) -> (StatusCode, String) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/fhir+json")
                    .header("accept", "application/fhir+xml")
                    .body(Body::from(body.into()))
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// GET `path` requesting XML, returning `(status, body_string, content_type)`.
    pub async fn get_fhir_xml(&self, path: &str) -> (StatusCode, String, String) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(path)
                    .header("accept", "application/fhir+xml")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();
        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned(), ct)
    }

    /// Import a FHIR Bundle JSON string, returning `(status, response_body)`.
    pub async fn import_bundle(&self, bundle: &str) -> (StatusCode, Value) {
        self.post_fhir("/import", bundle).await
    }

    /// Import a FHIR Bundle and assert success (200 or 207 with no errors).
    pub async fn import_bundle_ok(&self, bundle: &str) -> Value {
        let (status, body) = self.import_bundle(bundle).await;
        assert!(
            status == StatusCode::OK || status == StatusCode::MULTI_STATUS,
            "import failed with {status}: {body}"
        );
        if status == StatusCode::MULTI_STATUS {
            let errors = &body["errors"];
            assert!(
                errors.is_null() || errors.as_array().is_none_or(|a| a.is_empty()),
                "import returned errors: {errors}"
            );
        }
        body
    }

    /// Build a FHIR Parameters JSON body from a slice of `(name, value_key, value)` triples.
    pub fn params(entries: &[(&str, &str, &str)]) -> String {
        build_params(entries)
    }
}

// ── Shared Parameters builder ──────────────────────────────────────────────────

/// Build a FHIR Parameters JSON body from a slice of `(name, value_key, value)` triples.
pub fn build_params(entries: &[(&str, &str, &str)]) -> String {
    let params: Vec<Value> = entries
        .iter()
        .map(|(name, key, val)| {
            let mut obj = serde_json::Map::new();
            obj.insert("name".into(), Value::String(name.to_string()));
            obj.insert(key.to_string(), Value::String(val.to_string()));
            Value::Object(obj)
        })
        .collect();
    serde_json::json!({
        "resourceType": "Parameters",
        "parameter": params
    })
    .to_string()
}

// ── TestAppPg — PostgreSQL HTTP test harness ───────────────────────────────────

/// A test application that wraps the full HTS Axum router backed by a live
/// PostgreSQL instance (via testcontainers).
///
/// Uses the same `pg_http_url()` shared container so all `TestAppPg` instances
/// within a single test binary share one Docker container, but each instance
/// gets its own database (see [`fresh_pg_database`]): tests importing the
/// same fixtures concurrently would otherwise replace each other's rows
/// between a search and the request that uses its result.
#[cfg(feature = "postgres")]
pub struct TestAppPg {
    router: Router,
    url: String,
}

#[cfg(feature = "postgres")]
impl TestAppPg {
    /// Create a new `TestAppPg` on a fresh database in the shared container.
    pub async fn new() -> Self {
        use helios_hts::backends::PostgresTerminologyBackend;
        use helios_persistence::backends::postgres::PostgresBackend;
        use std::sync::Arc;

        let db_url = fresh_pg_database().await;
        let url = db_url.as_str();

        let backend = PostgresTerminologyBackend::new(url)
            .await
            .expect("Failed to create PostgresTerminologyBackend");

        let resource_store = PostgresBackend::from_connection_string(url)
            .await
            .expect("Failed to open PostgreSQL resource store");

        let state = PgAppState::new(backend.clone())
            .with_resource_store_pg(resource_store)
            .with_terminology_importer(Arc::new(backend));

        let config = PgHtsConfig::default();
        let router = helios_hts::server::create_app(&config, state);
        TestAppPg {
            router,
            url: db_url,
        }
    }

    /// Connection URL of this app's database.
    #[allow(dead_code)]
    pub fn database_url(&self) -> &str {
        &self.url
    }

    /// POST a FHIR JSON body to `path`, returning `(status, response_body)`.
    pub async fn post_fhir(&self, path: &str, body: impl Into<String>) -> (StatusCode, Value) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/fhir+json")
                    .body(Body::from(body.into()))
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    /// GET `path`, returning `(status, response_body)`.
    pub async fn get_fhir(&self, path: &str) -> (StatusCode, Value) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    /// PUT a FHIR JSON body to `path`, with optional `If-Match` header.
    pub async fn put_fhir(
        &self,
        path: &str,
        body: impl Into<String>,
        if_match: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method("PUT")
            .uri(path)
            .header("content-type", "application/fhir+json");
        if let Some(etag) = if_match {
            builder = builder.header("if-match", etag);
        }
        let response = self
            .router
            .clone()
            .oneshot(builder.body(Body::from(body.into())).unwrap())
            .await
            .unwrap();

        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    /// DELETE `path`, returning status code.
    pub async fn delete_fhir(&self, path: &str) -> StatusCode {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        response.status()
    }

    /// POST a FHIR JSON body to `path` requesting XML, returning `(status, body_string)`.
    pub async fn post_fhir_xml(&self, path: &str, body: impl Into<String>) -> (StatusCode, String) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/fhir+json")
                    .header("accept", "application/fhir+xml")
                    .body(Body::from(body.into()))
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// GET `path` requesting XML, returning `(status, body_string, content_type)`.
    pub async fn get_fhir_xml(&self, path: &str) -> (StatusCode, String, String) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(path)
                    .header("accept", "application/fhir+xml")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();
        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned(), ct)
    }

    /// Import a FHIR Bundle JSON string, returning `(status, response_body)`.
    pub async fn import_bundle(&self, bundle: &str) -> (StatusCode, Value) {
        self.post_fhir("/import", bundle).await
    }

    /// Import a FHIR Bundle and assert success (200 or 207 with no errors).
    pub async fn import_bundle_ok(&self, bundle: &str) -> Value {
        let (status, body) = self.import_bundle(bundle).await;
        assert!(
            status == StatusCode::OK || status == StatusCode::MULTI_STATUS,
            "import failed with {status}: {body}"
        );
        if status == StatusCode::MULTI_STATUS {
            let errors = &body["errors"];
            assert!(
                errors.is_null() || errors.as_array().is_none_or(|a| a.is_empty()),
                "import returned errors: {errors}"
            );
        }
        body
    }

    /// Build a FHIR Parameters JSON body from a slice of `(name, value_key, value)` triples.
    pub fn params(entries: &[(&str, &str, &str)]) -> String {
        build_params(entries)
    }
}
