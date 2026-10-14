//! Integration tests for HTS (Helios Terminology Server) delegation.
//!
//! These tests verify that:
//! 1. With a terminology server configured, a token `:in` parameter is expanded
//!    via `POST /ValueSet/$expand` and replaced by a plain token parameter
//!    carrying the expanded codes; token `:above` / `:below` are expanded the
//!    same way (code subsumption). This covers the terminal parameter of a
//!    chained or `_has` search too.
//! 2. `:not-in` is expanded the same way and searched negated (`code:not=`),
//!    so the backends answer it at the resource level; without a terminology
//!    server it is a `501` like `:in`.
//! 3. Without a terminology server, `:in` and token `:above` / `:below` are a
//!    `501` naming `HFS_TERMINOLOGY_SERVER` — they are not passed through to
//!    the persistence layer, which would match the ValueSet URL as a literal
//!    code. (`:above` / `:below` on a uri or reference are structural and need
//!    no terminology server.)
//! 4. A terminology modifier the parameter's type does not define (`name:in`,
//!    `subject.name:in`, `_has:Observation:subject:date:in`) is a `400` either
//!    way, and never reaches the terminology server. The terminal parameter of
//!    a chained or `_has` search is typed by the chain resolver, which calls
//!    back into the REST layer's expansion once the modifier has passed.
//! 5. When the expansion fails — the terminology server cannot be reached,
//!    the ValueSet is unknown, the expansion is over the code cap — the search
//!    fails closed with an OperationOutcome whose status reflects the cause
//!    (`503`, `400`, `422`); `HFS_SEARCH_TERMINOLOGY_FAIL=open|empty` opt into
//!    the old fail-open or a match-nothing posture.
//! 6. Expansions are requested flat (`excludeNested=true`) and paged
//!    (`count` / `offset`), a `url|version` canonical is split into
//!    `valueSetVersion`, nested `contains` are flattened, and successful
//!    expansions are cached for the configured TTL.
//!
//! # How the mock server works
//!
//! Each test spins up a lightweight Axum server on an ephemeral port (`0`) that
//! handles `POST /ValueSet/$expand`.  HFS is configured to point at this server
//! via the `terminology_server` field of `ServerConfig`.
//!
//! The mock server returns a FHIR ValueSet with a canned expansion, letting us
//! assert that the token filter injected into the search query matches the codes
//! from that expansion.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum_test::TestServer;
use helios_persistence::backends::sqlite::SqliteBackend;
use serde_json::{Value, json};
use tokio::net::TcpListener;

use helios_rest::{ServerConfig, create_app_with_config};

// ─── Mock HTS server ─────────────────────────────────────────────────────────

/// Shared state for the mock HTS.
#[derive(Clone, Default)]
struct MockHtsState {
    /// Requests received by the mock server.
    requests: Arc<Mutex<Vec<Value>>>,
    /// Canned expansion to return for any $expand call.
    expansion: Arc<Value>,
    /// A canned error to return instead (status, OperationOutcome body).
    error: Option<(u16, Arc<Value>)>,
}

/// The integer value of a `count` / `offset` Parameters entry.
fn int_param(body: &Value, name: &str) -> Option<u64> {
    body["parameter"]
        .as_array()?
        .iter()
        .find(|p| p["name"] == name)
        .and_then(|p| p["valueInteger"].as_u64())
}

/// Handler for `POST /ValueSet/$expand`: serves the canned expansion a page
/// at a time, honouring `count` / `offset` like a real server and reporting
/// `total`, or the canned error.
async fn mock_expand_handler(
    State(state): State<MockHtsState>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let count = int_param(&body, "count").map(|c| c as usize);
    let offset = int_param(&body, "offset").map_or(0, |o| o as usize);
    state.requests.lock().unwrap().push(body);

    if let Some((status, outcome)) = &state.error {
        return (
            StatusCode::from_u16(*status).unwrap(),
            Json((**outcome).clone()),
        );
    }

    let mut expansion = (*state.expansion).clone();
    if let Some(all) = expansion
        .pointer("/expansion/contains")
        .and_then(Value::as_array)
        .cloned()
    {
        let total = all.len();
        let page: Vec<Value> = all
            .into_iter()
            .skip(offset)
            .take(count.unwrap_or(usize::MAX))
            .collect();
        expansion["expansion"]["contains"] = Value::Array(page);
        expansion["expansion"]["total"] = json!(total);
        expansion["expansion"]["offset"] = json!(offset);
    }
    (StatusCode::OK, Json(expansion))
}

/// Starts a mock HTS Axum server on an ephemeral port.
///
/// Returns `(base_url, shared_requests)` where `shared_requests` can be
/// inspected after sending requests to HFS.
async fn start_mock_hts(expansion: Value) -> (String, Arc<Mutex<Vec<Value>>>) {
    start_mock_hts_with(MockHtsState {
        expansion: Arc::new(expansion),
        ..Default::default()
    })
    .await
}

/// [`start_mock_hts`] for a server that answers every `$expand` with the
/// given error status and OperationOutcome.
async fn start_failing_mock_hts(
    status: u16,
    diagnostics: &str,
) -> (String, Arc<Mutex<Vec<Value>>>) {
    start_mock_hts_with(MockHtsState {
        error: Some((
            status,
            Arc::new(json!({
                "resourceType": "OperationOutcome",
                "issue": [{"severity": "error", "code": "processing", "diagnostics": diagnostics}]
            })),
        )),
        ..Default::default()
    })
    .await
}

async fn start_mock_hts_with(mut state: MockHtsState) -> (String, Arc<Mutex<Vec<Value>>>) {
    let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
    state.requests = Arc::clone(&requests);

    let router = Router::new()
        .route(
            "/ValueSet/$expand",
            axum::routing::post(mock_expand_handler),
        )
        .with_state(state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{}", port);

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    (base_url, requests)
}

/// Creates a minimal FHIR ValueSet expansion response with the given codes.
fn make_expansion(system: &str, codes: &[&str]) -> Value {
    let contains: Vec<Value> = codes
        .iter()
        .map(|code| {
            json!({
                "system": system,
                "code": code,
                "display": code
            })
        })
        .collect();

    json!({
        "resourceType": "ValueSet",
        "expansion": {
            "total": contains.len(),
            "contains": contains
        }
    })
}

// ─── Helper: in-memory HFS backed by SQLite ──────────────────────────────────

fn create_hfs_with_terminology_server(ts_url: &str) -> TestServer {
    let backend = SqliteBackend::in_memory().expect("SQLite in-memory failed");
    backend.init_schema().expect("Schema init failed");

    let config = ServerConfig {
        terminology_server: Some(ts_url.to_string()),
        ..ServerConfig::for_testing()
    };

    let app = create_app_with_config(backend, config);
    TestServer::new(app).expect("TestServer init failed")
}

/// An HFS over an in-memory SQLite backend loaded with the spec search
/// parameters (the embedded fallback set knows neither `code` nor `subject`),
/// with the given configuration.
fn create_hfs_with_spec_params(config: ServerConfig) -> TestServer {
    let data_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data");
    let backend = SqliteBackend::with_config(
        ":memory:",
        helios_persistence::backends::sqlite::SqliteBackendConfig {
            data_dir: Some(data_dir),
            ..Default::default()
        },
    )
    .expect("SQLite in-memory failed");
    backend.init_schema().expect("Schema init failed");
    TestServer::new(create_app_with_config(backend, config)).expect("TestServer init failed")
}

/// Seeds Observations `o-<code>` with the given LOINC codes.
async fn seed_observations(server: &TestServer, codes: &[&str]) {
    for code in codes {
        let resource = json!({"resourceType": "Observation", "id": format!("o-{code}"),
            "status": "final",
            "code": {"coding": [{"system": "http://loinc.org", "code": code}]}});
        let response = server
            .put(&format!("/Observation/o-{code}"))
            .json(&resource)
            .await;
        assert!(response.status_code().is_success(), "seeding o-{code}");
    }
}

/// The sorted ids of a searchset Bundle's entries.
fn entry_ids(body: &Value) -> Vec<String> {
    let mut ids: Vec<String> = body["entry"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|e| e["resource"]["id"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids
}

/// The named parameter of the `n`th `$expand` request the mock saw.
fn request_param(requests: &Arc<Mutex<Vec<Value>>>, n: usize, name: &str) -> Option<Value> {
    requests.lock().unwrap()[n]["parameter"]
        .as_array()?
        .iter()
        .find(|p| p["name"] == name)
        .cloned()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// When a `:in` search parameter is used and a terminology server is configured,
/// HFS should call `POST /ValueSet/$expand` on the HTS and translate the
/// expansion to a plain token search (so the storage backend receives codes,
/// not a ValueSet URL).
///
/// We verify this indirectly: the mock HTS records the expansion request, and
/// HFS issues a search against the backend that the SQLite layer can process.
#[tokio::test]
async fn test_in_modifier_calls_hts_expand() {
    let expansion = make_expansion("http://example.org/cs", &["A", "B", "C"]);
    let (ts_url, requests) = start_mock_hts(expansion).await;

    let server = create_hfs_with_terminology_server(&ts_url);

    // Send a search with :in modifier.  The response will be an empty bundle
    // (no patients seeded) but the HTS should have received one $expand call.
    let response = server
        .get("/Patient")
        .add_query_param("code:in", "http://example.org/vs")
        .await;

    assert_eq!(response.status_code(), StatusCode::OK);

    // Verify the mock HTS received exactly one $expand request.
    let reqs = requests.lock().unwrap();
    assert_eq!(reqs.len(), 1, "Expected exactly one $expand call to HTS");

    // Verify the URL parameter in the request body.
    let params_array = reqs[0]["parameter"].as_array().unwrap();
    let url_param = params_array
        .iter()
        .find(|p| p["name"] == "url")
        .expect("Expected 'url' parameter in $expand request");
    assert_eq!(url_param["valueUri"], "http://example.org/vs");
}

/// When no terminology server is configured, a token `:in` modifier cannot be
/// satisfied and must be rejected with `501` rather than silently falling
/// through to literal matching (which would return misleading results).
#[tokio::test]
async fn test_in_modifier_returns_not_implemented_without_terminology_server() {
    let backend = SqliteBackend::in_memory().expect("SQLite in-memory failed");
    backend.init_schema().expect("Schema init failed");

    // No terminology_server set
    let config = ServerConfig::for_testing();
    let app = create_app_with_config(backend, config);
    let server = TestServer::new(app).expect("TestServer init failed");

    let response = server
        .get("/Patient")
        .add_query_param("code:in", "http://example.org/vs")
        .await;

    // Fail loud: `:in` needs a terminology server, so it returns 501 (no HTTP
    // call to an external server is made because none is configured).
    assert_eq!(
        response.status_code().as_u16(),
        501,
        "Expected 501 for :in without a configured terminology server"
    );
}

/// `:not-in` expands the ValueSet like `:in` and searches the codes negated:
/// the resources whose parameter has *no* value in the set (H8b). It used to
/// be a `501`.
#[tokio::test]
async fn test_not_in_modifier_expands_and_excludes_the_codes() {
    let expansion = make_expansion("http://loinc.org", &["1234-5", "2345-6"]);
    let (ts_url, requests) = start_mock_hts(expansion).await;
    let server = create_hfs_with_spec_params(ServerConfig {
        terminology_server: Some(ts_url),
        ..ServerConfig::for_testing()
    });
    seed_observations(&server, &["1234-5", "2345-6", "9999-9"]).await;

    let response = server
        .get("/Observation")
        .add_query_param("code:not-in", "http://example.org/vs")
        .await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(entry_ids(&response.json()), ["o-9999-9"]);
    assert_eq!(requests.lock().unwrap().len(), 1);

    // Positive control, same expansion: `:in` is the complement.
    let response = server
        .get("/Observation")
        .add_query_param("code:in", "http://example.org/vs")
        .await;
    assert_eq!(entry_ids(&response.json()), ["o-1234-5", "o-2345-6"]);

    // An empty ValueSet excludes nothing.
    let (ts_url, _) = start_mock_hts(make_expansion("http://loinc.org", &[])).await;
    let server = create_hfs_with_spec_params(ServerConfig {
        terminology_server: Some(ts_url),
        ..ServerConfig::for_testing()
    });
    seed_observations(&server, &["1234-5"]).await;
    let response = server
        .get("/Observation")
        .add_query_param("code:not-in", "http://example.org/vs")
        .await;
    assert_eq!(entry_ids(&response.json()), ["o-1234-5"]);
}

/// When the HTS is unreachable, a `:in` search fails closed with a `503`
/// OperationOutcome (H8a). It used to drop the parameter and return every
/// resource — a superset the client had no way to detect.
/// `HFS_SEARCH_TERMINOLOGY_FAIL=open` restores that, `empty` matches nothing.
#[tokio::test]
async fn test_in_modifier_fails_closed_on_hts_unavailable() {
    // Use a port that has nothing listening on it.
    let ts_url = "http://127.0.0.1:19999";

    let server_with_policy = |policy: &str| {
        let mut config = ServerConfig {
            terminology_server: Some(ts_url.to_string()),
            ..ServerConfig::for_testing()
        };
        config.search_terminology.fail = policy.to_string();
        create_hfs_with_spec_params(config)
    };

    // Default: closed.
    let server = server_with_policy("closed");
    seed_observations(&server, &["1234-5"]).await;
    for key in ["code:in", "code:not-in", "code:below", "code:above"] {
        let value = if key.ends_with(":in") {
            "http://example.org/vs"
        } else {
            "http://loinc.org|1234-5"
        };
        let response = server
            .get("/Observation")
            .add_query_param(key, value)
            .add_query_param("status", "final")
            .await;
        assert_eq!(
            response.status_code(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{key}"
        );
        let body: Value = response.json();
        assert_eq!(body["resourceType"], "OperationOutcome", "{key}");
        let issue = body["issue"][0].to_string();
        assert!(issue.contains("could not be reached"), "{key}: {issue}");
        assert!(
            issue.contains("'code' "),
            "{key} names the parameter: {issue}"
        );
        // Nothing about the server's address leaks.
        assert!(!issue.contains("19999"), "{key}: {issue}");
    }

    // Opt-in fail-open: the parameter is dropped, the rest of the search runs.
    let server = server_with_policy("open");
    seed_observations(&server, &["1234-5"]).await;
    let response = server
        .get("/Observation")
        .add_query_param("code:in", "http://example.org/vs")
        .add_query_param("status", "final")
        .await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(entry_ids(&response.json()), ["o-1234-5"]);

    // Opt-in match-nothing.
    let server = server_with_policy("empty");
    seed_observations(&server, &["1234-5"]).await;
    let response = server
        .get("/Observation")
        .add_query_param("code:in", "http://example.org/vs")
        .add_query_param("_total", "accurate")
        .await;
    assert_eq!(response.status_code(), StatusCode::OK);
    let body: Value = response.json();
    assert_eq!(body["total"], 0, "{body}");
    assert!(entry_ids(&body).is_empty());
}

/// The expansion request is flat and paged, and a `url|version` canonical is
/// sent as `url` + `valueSetVersion` (which is what HTS resolves — `url|v`
/// as one string is a `404` there).
#[tokio::test]
async fn test_in_modifier_requests_flat_pages_and_splits_the_version() {
    let expansion = make_expansion("http://loinc.org", &["1234-5"]);
    let (ts_url, requests) = start_mock_hts(expansion).await;
    let server = create_hfs_with_terminology_server(&ts_url);

    let response = server
        .get("/Patient")
        .add_query_param("code:in", "http://example.org/vs|2.0.0")
        .await;
    assert_eq!(response.status_code(), StatusCode::OK);

    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(
        request_param(&requests, 0, "url").unwrap()["valueUri"],
        "http://example.org/vs"
    );
    assert_eq!(
        request_param(&requests, 0, "valueSetVersion").unwrap()["valueString"],
        "2.0.0"
    );
    assert_eq!(
        request_param(&requests, 0, "excludeNested").unwrap()["valueBoolean"],
        true
    );
    assert_eq!(
        request_param(&requests, 0, "count").unwrap()["valueInteger"],
        1000
    );
    assert_eq!(
        request_param(&requests, 0, "offset").unwrap()["valueInteger"],
        0
    );

    // Without a version, none is sent.
    let response = server
        .get("/Patient")
        .add_query_param("code:in", "http://example.org/vs")
        .await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert!(request_param(&requests, 1, "valueSetVersion").is_none());
}

/// A `:below` / `:above` on a token asks for an `is-a` / `generalizes`
/// expansion of an inline ValueSet, flat and paged like a `:in`.
#[tokio::test]
async fn test_hierarchy_modifiers_request_flat_subsumption_expansions() {
    let expansion = make_expansion("http://loinc.org", &["1234-5"]);
    let (ts_url, requests) = start_mock_hts(expansion).await;
    let server = create_hfs_with_terminology_server(&ts_url);

    for (key, op) in [("code:below", "is-a"), ("code:above", "generalizes")] {
        let before = requests.lock().unwrap().len();
        let response = server
            .get("/Patient")
            .add_query_param(key, "http://loinc.org|1234-5")
            .await;
        assert_eq!(response.status_code(), StatusCode::OK, "{key}");
        let include = request_param(&requests, before, "valueSet").unwrap()["resource"]["compose"]
            ["include"][0]
            .clone();
        assert_eq!(include["system"], "http://loinc.org", "{key}");
        assert_eq!(include["filter"][0]["op"], op, "{key}");
        assert_eq!(include["filter"][0]["value"], "1234-5", "{key}");
        assert_eq!(
            request_param(&requests, before, "excludeNested").unwrap()["valueBoolean"],
            true,
            "{key}"
        );
        assert!(request_param(&requests, before, "count").is_some(), "{key}");
    }
}

/// A large ValueSet is fetched in pages of `HFS_SEARCH_EXPANSION_PAGE_SIZE`,
/// and every page's codes end up in the filter (H8c: the fork used to read
/// only the first level of a single unpaged, nested response).
#[tokio::test]
async fn test_in_modifier_pages_through_a_large_expansion() {
    let codes = ["1000-1", "1000-2", "1000-3", "1000-4", "1000-5"];
    let (ts_url, requests) = start_mock_hts(make_expansion("http://loinc.org", &codes)).await;
    let mut config = ServerConfig {
        terminology_server: Some(ts_url),
        ..ServerConfig::for_testing()
    };
    config.search_terminology.expansion_page_size = 2;
    let server = create_hfs_with_spec_params(config);
    seed_observations(&server, &["1000-1", "1000-3", "1000-5", "9999-9"]).await;

    let response = server
        .get("/Observation")
        .add_query_param("code:in", "http://example.org/vs")
        .await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(
        entry_ids(&response.json()),
        ["o-1000-1", "o-1000-3", "o-1000-5"]
    );

    // Three pages: offsets 0, 2, 4 — and no fourth once `total` is reached.
    // Read the count first: a guard taken inside the `let` below would live
    // until its end and deadlock `request_param`'s own lock.
    let sent = requests.lock().unwrap().len();
    let offsets: Vec<u64> = (0..sent)
        .map(|n| {
            request_param(&requests, n, "offset").unwrap()["valueInteger"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(offsets, [0, 2, 4]);
}

/// An expansion larger than `HFS_SEARCH_EXPANSION_MAX_CODES` is refused as
/// `422` rather than turned into a filter with that many bound parameters.
#[tokio::test]
async fn test_in_modifier_rejects_an_expansion_over_the_code_cap() {
    let codes = ["1000-1", "1000-2", "1000-3", "1000-4", "1000-5"];
    let (ts_url, _requests) = start_mock_hts(make_expansion("http://loinc.org", &codes)).await;
    let mut config = ServerConfig {
        terminology_server: Some(ts_url),
        ..ServerConfig::for_testing()
    };
    config.search_terminology.expansion_max_codes = 3;
    let server = create_hfs_with_spec_params(config);

    let response = server
        .get("/Observation")
        .add_query_param("code:in", "http://example.org/vs")
        .await;
    assert_eq!(response.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
    let issue = response.json::<Value>()["issue"][0].to_string();
    assert!(issue.contains("more than 3 codes"), "{issue}");
}

/// The terminology server's own answers map onto the search's status: an
/// unknown ValueSet (`404`) is the client's `400`, `too-costly` (`422`) stays
/// a `422`, and a server error is a `503`. The server's diagnostics are
/// relayed for the first two.
#[tokio::test]
async fn test_terminology_server_errors_map_to_search_statuses() {
    for (hts_status, expected, diagnostics) in [
        (
            404,
            StatusCode::BAD_REQUEST,
            "ValueSet not found: http://example.org/vs",
        ),
        (400, StatusCode::BAD_REQUEST, "Missing required parameter"),
        (
            422,
            StatusCode::UNPROCESSABLE_ENTITY,
            "47162 codes exceeds the server limit",
        ),
        (500, StatusCode::SERVICE_UNAVAILABLE, "boom"),
    ] {
        let (ts_url, requests) = start_failing_mock_hts(hts_status, diagnostics).await;
        let server = create_hfs_with_spec_params(ServerConfig {
            terminology_server: Some(ts_url),
            ..ServerConfig::for_testing()
        });
        let response = server
            .get("/Observation")
            .add_query_param("code:in", "http://example.org/vs")
            .await;
        assert_eq!(response.status_code(), expected, "HTS {hts_status}");
        let body: Value = response.json();
        assert_eq!(body["resourceType"], "OperationOutcome", "HTS {hts_status}");
        let issue = body["issue"][0].to_string();
        assert!(
            issue.contains("http://example.org/vs"),
            "HTS {hts_status}: {issue}"
        );
        match hts_status {
            404 => assert!(
                issue.contains("is not known to the terminology server"),
                "{issue}"
            ),
            400 | 422 => assert!(issue.contains(diagnostics), "{issue}"),
            _ => assert!(!issue.contains(diagnostics), "{issue}"),
        }
        assert_eq!(requests.lock().unwrap().len(), 1, "HTS {hts_status}");
    }
}

/// A server that answers a tree despite `excludeNested=true` is flattened:
/// every nested code is searched for, not just the roots.
#[tokio::test]
async fn test_in_modifier_flattens_a_nested_expansion() {
    let expansion = json!({
        "resourceType": "ValueSet",
        "expansion": { "contains": [{
            "system": "http://loinc.org", "code": "1000-1",
            "contains": [
                {"system": "http://loinc.org", "code": "1000-2",
                 "contains": [{"system": "http://loinc.org", "code": "1000-3"}]}
            ]
        }]}
    });
    let (ts_url, _requests) = start_mock_hts(expansion).await;
    let server = create_hfs_with_spec_params(ServerConfig {
        terminology_server: Some(ts_url),
        ..ServerConfig::for_testing()
    });
    seed_observations(&server, &["1000-1", "1000-2", "1000-3", "9999-9"]).await;

    let response = server
        .get("/Observation")
        .add_query_param("code:in", "http://example.org/vs")
        .await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(
        entry_ids(&response.json()),
        ["o-1000-1", "o-1000-2", "o-1000-3"]
    );
}

/// With a cache TTL, identical expansions within it are served from the
/// cache: one `$expand` for any number of searches.
#[tokio::test]
async fn test_expansions_are_cached_for_the_configured_ttl() {
    let (ts_url, requests) = start_mock_hts(make_expansion("http://loinc.org", &["1234-5"])).await;
    let mut config = ServerConfig {
        terminology_server: Some(ts_url),
        ..ServerConfig::for_testing()
    };
    config.search_terminology.expansion_cache_ttl_secs = 60;
    let server = create_hfs_with_spec_params(config);
    seed_observations(&server, &["1234-5", "9999-9"]).await;

    for _ in 0..3 {
        let response = server
            .get("/Observation")
            .add_query_param("code:in", "http://example.org/vs")
            .await;
        assert_eq!(entry_ids(&response.json()), ["o-1234-5"]);
    }
    // `:not-in` of the same ValueSet shares the expansion.
    let response = server
        .get("/Observation")
        .add_query_param("code:not-in", "http://example.org/vs")
        .await;
    assert_eq!(entry_ids(&response.json()), ["o-9999-9"]);
    assert_eq!(requests.lock().unwrap().len(), 1);

    // A different canonical (another version) is its own expansion.
    let response = server
        .get("/Observation")
        .add_query_param("code:in", "http://example.org/vs|2")
        .await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(requests.lock().unwrap().len(), 2);
}

/// With a terminology server configured, a terminology-backed modifier on the
/// terminal parameter of a chained or `_has` search is expanded like a direct
/// one, and the chain resolves against the expanded codes (#1317: the `501` a
/// server *without* terminology gives these must not reach this path).
#[tokio::test]
async fn test_chained_in_and_below_modifiers_resolve_with_terminology_server() {
    let expansion = make_expansion("http://loinc.org", &["1234-5"]);
    let (ts_url, requests) = start_mock_hts(expansion).await;

    // The spec search parameters are needed here: the embedded fallback set
    // knows neither `code` nor `subject`, so every chain would match nothing.
    let data_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data");
    let backend = SqliteBackend::with_config(
        ":memory:",
        helios_persistence::backends::sqlite::SqliteBackendConfig {
            data_dir: Some(data_dir),
            ..Default::default()
        },
    )
    .expect("SQLite in-memory failed");
    backend.init_schema().expect("Schema init failed");
    let config = ServerConfig {
        terminology_server: Some(ts_url),
        ..ServerConfig::for_testing()
    };
    let server = TestServer::new(create_app_with_config(backend, config)).unwrap();

    for resource in [
        json!({"resourceType": "Patient", "id": "p-match"}),
        json!({"resourceType": "Patient", "id": "p-other"}),
        json!({"resourceType": "Observation", "id": "o-match", "status": "final",
               "code": {"coding": [{"system": "http://loinc.org", "code": "1234-5"}]},
               "subject": {"reference": "Patient/p-match"}}),
        json!({"resourceType": "Observation", "id": "o-other", "status": "final",
               "code": {"coding": [{"system": "http://loinc.org", "code": "9999-9"}]},
               "subject": {"reference": "Patient/p-other"}}),
        json!({"resourceType": "DiagnosticReport", "id": "d-match", "status": "final",
               "code": {"text": "report"},
               "result": [{"reference": "Observation/o-match"}]}),
        json!({"resourceType": "DiagnosticReport", "id": "d-other", "status": "final",
               "code": {"text": "report"},
               "result": [{"reference": "Observation/o-other"}]}),
    ] {
        let path = format!(
            "/{}/{}",
            resource["resourceType"].as_str().unwrap(),
            resource["id"].as_str().unwrap()
        );
        let response = server.put(&path).json(&resource).await;
        assert!(response.status_code().is_success(), "seeding {path}");
    }

    let ids = |body: &Value| -> Vec<String> {
        let mut ids: Vec<String> = body["entry"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| e["resource"]["id"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids
    };

    // Positive control: the plain chain finds both reports.
    let response = server
        .get("/DiagnosticReport")
        .add_query_param("result.status", "final")
        .await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(ids(&response.json()), ["d-match", "d-other"]);

    for (resource_type, key, value, expected) in [
        (
            "DiagnosticReport",
            "result.code:in",
            "http://example.org/vs",
            "d-match",
        ),
        (
            "DiagnosticReport",
            "result:Observation.code:in",
            "http://example.org/vs",
            "d-match",
        ),
        (
            "Patient",
            "_has:Observation:subject:code:in",
            "http://example.org/vs",
            "p-match",
        ),
        (
            "DiagnosticReport",
            "result:Observation.code:below",
            "http://loinc.org|1234-5",
            "d-match",
        ),
        (
            "Patient",
            "_has:Observation:subject:code:below",
            "http://loinc.org|1234-5",
            "p-match",
        ),
    ] {
        let before = requests.lock().unwrap().len();
        let response = server
            .get(&format!("/{resource_type}"))
            .add_query_param(key, value)
            .await;
        assert_eq!(response.status_code(), StatusCode::OK, "{key}");
        assert_eq!(ids(&response.json()), [expected], "{key}");
        assert_eq!(
            requests.lock().unwrap().len(),
            before + 1,
            "{key}: expected one $expand call"
        );
    }
}

/// #1339: a terminology-backed modifier on a parameter whose type does not
/// define it (`name:in` — a string) is a `400` with a terminology server too.
/// It used to be expanded like a token's and searched as `name=<codes>`, an
/// empty `200`. The terminology server is not consulted for it.
#[tokio::test]
async fn test_terminology_modifier_on_wrong_parameter_type_is_rejected_before_expansion() {
    let expansion = make_expansion("http://example.org/cs", &["Smith"]);
    let (ts_url, requests) = start_mock_hts(expansion).await;

    // The spec search parameters are needed: the embedded fallback set does not
    // know `name` or `birthdate`, and an unregistered parameter is not gated.
    let data_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data");
    let backend = SqliteBackend::with_config(
        ":memory:",
        helios_persistence::backends::sqlite::SqliteBackendConfig {
            data_dir: Some(data_dir),
            ..Default::default()
        },
    )
    .expect("SQLite in-memory failed");
    backend.init_schema().expect("Schema init failed");
    let config = ServerConfig {
        terminology_server: Some(ts_url),
        ..ServerConfig::for_testing()
    };
    let server = TestServer::new(create_app_with_config(backend, config)).unwrap();

    let patient = json!({"resourceType": "Patient", "id": "p1", "gender": "male",
                         "name": [{"family": "Smith"}]});
    let response = server.put("/Patient/p1").json(&patient).await;
    assert!(response.status_code().is_success());

    // Positive control: the parameter itself finds the patient.
    let response = server
        .get("/Patient")
        .add_query_param("name", "Smith")
        .await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(response.json::<Value>()["entry"][0]["resource"]["id"], "p1");

    for (key, value) in [
        ("name:in", "http://example.org/vs"),
        ("name:not-in", "http://example.org/vs"),
        ("name:below", "http://example.org/cs|Smith"),
        ("birthdate:above", "http://example.org/cs|1980"),
    ] {
        let response = server.get("/Patient").add_query_param(key, value).await;
        assert_eq!(response.status_code(), StatusCode::BAD_REQUEST, "{key}");
        assert!(
            response.json::<Value>()["issue"][0]
                .to_string()
                .contains("is not supported for"),
            "{key}"
        );
    }
    assert!(
        requests.lock().unwrap().is_empty(),
        "an invalid modifier must not reach the terminology server"
    );

    // A valid one still does.
    let response = server
        .get("/Patient")
        .add_query_param("gender:in", "http://example.org/vs")
        .await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(requests.lock().unwrap().len(), 1);
}

/// #1365: the same on the terminal parameter of a chained or `_has` search.
/// `subject.name:in` used to be rewritten into `subject.name=<expanded codes>`
/// before the chain resolver could type `name`, so it was a string search for
/// the codes — a silent `200`. The terminal is now typed first: an invalid
/// modifier is a `400` and never reaches the terminology server, a valid one
/// is expanded (once), a structural one (uri / reference `:below`) is left to
/// the backend, and `:not-in` is a `400` or expanded as on a direct parameter.
#[tokio::test]
async fn test_chained_terminology_modifier_is_checked_against_the_terminal_type() {
    // One expansion for every call: a code of each value set searched below.
    let expansion = json!({
        "resourceType": "ValueSet",
        "expansion": { "contains": [
            {"system": "http://loinc.org", "code": "1234-5"},
            {"system": "http://snomed.info/sct", "code": "706767009"},
            // What the wrong-typed searches would find, were they run.
            {"system": "http://example.org/cs", "code": "Smith"},
        ]}
    });
    let (ts_url, requests) = start_mock_hts(expansion).await;

    let data_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data");
    let backend = SqliteBackend::with_config(
        ":memory:",
        helios_persistence::backends::sqlite::SqliteBackendConfig {
            data_dir: Some(data_dir),
            ..Default::default()
        },
    )
    .expect("SQLite in-memory failed");
    backend.init_schema().expect("Schema init failed");
    let config = ServerConfig {
        terminology_server: Some(ts_url),
        ..ServerConfig::for_testing()
    };
    let server = TestServer::new(create_app_with_config(backend, config)).unwrap();

    for resource in [
        json!({"resourceType": "Practitioner", "id": "gp"}),
        json!({"resourceType": "Patient", "id": "p-match", "gender": "female",
               "birthDate": "1980-01-01", "name": [{"family": "Smith"}],
               "generalPractitioner": [{"reference": "Practitioner/gp"}]}),
        json!({"resourceType": "Patient", "id": "p-other", "gender": "male",
               "name": [{"family": "Jones"}]}),
        json!({"resourceType": "Device", "id": "dev", "url": "http://dev.example.org/a/b",
               "type": {"coding": [{"system": "http://snomed.info/sct", "code": "706767009"}]}}),
        json!({"resourceType": "Observation", "id": "o-match", "status": "final",
               "code": {"coding": [{"system": "http://loinc.org", "code": "1234-5"}]},
               "effectiveDateTime": "2020-01-01",
               "subject": {"reference": "Patient/p-match"}}),
        json!({"resourceType": "Observation", "id": "o-other", "status": "final",
               "code": {"coding": [{"system": "http://loinc.org", "code": "9999-9"}]},
               "subject": {"reference": "Patient/p-other"}}),
        json!({"resourceType": "Observation", "id": "o-dev", "status": "final",
               "code": {"coding": [{"system": "http://loinc.org", "code": "9999-9"}]},
               "subject": {"reference": "Device/dev"}}),
        json!({"resourceType": "DiagnosticReport", "id": "d-match", "status": "final",
               "code": {"text": "report"},
               "result": [{"reference": "Observation/o-match"}]}),
        json!({"resourceType": "DiagnosticReport", "id": "d-other", "status": "final",
               "code": {"text": "report"},
               "result": [{"reference": "Observation/o-other"}]}),
        json!({"resourceType": "DiagnosticReport", "id": "d-dev", "status": "final",
               "code": {"text": "report"},
               "result": [{"reference": "Observation/o-dev"}]}),
    ] {
        let path = format!(
            "/{}/{}",
            resource["resourceType"].as_str().unwrap(),
            resource["id"].as_str().unwrap()
        );
        let response = server.put(&path).json(&resource).await;
        assert!(response.status_code().is_success(), "seeding {path}");
    }

    let ids = |body: &Value| -> Vec<String> {
        let mut ids: Vec<String> = body["entry"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| e["resource"]["id"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids
    };

    // Positive controls: every chain used below resolves, on the terminal
    // parameters the invalid modifiers are put on.
    for (resource_type, key, value, expected) in [
        ("Observation", "subject.name", "Smith", "o-match"),
        (
            "Observation",
            "subject:Patient.birthdate",
            "1980-01-01",
            "o-match",
        ),
        (
            "DiagnosticReport",
            "result.subject:Patient.name",
            "Smith",
            "d-match",
        ),
        (
            "Patient",
            "_has:Observation:subject:date",
            "2020-01-01",
            "p-match",
        ),
        ("Observation", "subject.type", "706767009", "o-dev"),
    ] {
        let response = server
            .get(&format!("/{resource_type}"))
            .add_query_param(key, value)
            .await;
        assert_eq!(response.status_code(), StatusCode::OK, "{key}");
        assert_eq!(ids(&response.json()), [expected], "{key}");
    }

    // Invalid for the terminal's type: `400`, with or without a `:Type`.
    for (resource_type, key, value) in [
        ("Observation", "subject.name:in", "http://example.org/vs"),
        (
            "Observation",
            "subject:Patient.name:in",
            "http://example.org/vs",
        ),
        (
            "Observation",
            "subject:Patient.name:not-in",
            "http://example.org/vs",
        ),
        (
            "Observation",
            "subject:Patient.name:below",
            "http://example.org/cs|Smith",
        ),
        (
            "Observation",
            "subject:Patient.birthdate:above",
            "http://example.org/cs|1980",
        ),
        (
            "DiagnosticReport",
            "result.subject:Patient.name:in",
            "http://example.org/vs",
        ),
        (
            "DiagnosticReport",
            "result.subject:Patient.name:above",
            "http://example.org/cs|Smith",
        ),
        (
            "Patient",
            "_has:Observation:subject:date:in",
            "http://example.org/vs",
        ),
        (
            "Patient",
            "_has:Observation:subject:date:not-in",
            "http://example.org/vs",
        ),
        (
            "Patient",
            "_has:Observation:subject:date:below",
            "http://example.org/cs|2020",
        ),
    ] {
        let response = server
            .get(&format!("/{resource_type}"))
            .add_query_param(key, value)
            .await;
        assert_eq!(response.status_code(), StatusCode::BAD_REQUEST, "{key}");
        assert!(
            response.json::<Value>()["issue"][0]
                .to_string()
                .contains("is not supported for"),
            "{key}"
        );
    }
    assert!(
        requests.lock().unwrap().is_empty(),
        "an invalid modifier must not reach the terminology server"
    );

    // Valid and structural (reference / uri `:below`): the backend's, and no
    // business of the terminology server.
    for (key, value, expected) in [
        (
            "subject:Patient.general-practitioner:below",
            "Practitioner/gp",
            "o-match",
        ),
        (
            "subject:Device.url:below",
            "http://dev.example.org/a",
            "o-dev",
        ),
    ] {
        let response = server.get("/Observation").add_query_param(key, value).await;
        assert_eq!(response.status_code(), StatusCode::OK, "{key}");
        assert_eq!(ids(&response.json()), [expected], "{key}");
    }
    assert!(requests.lock().unwrap().is_empty());

    // Valid `:not-in` on a token terminal: expanded and searched negated, as
    // the direct form is. No patient's gender is in the set, so every
    // Observation with a Patient subject; the Observations whose code is not
    // in the set are o-other and o-dev, whose Patient subject is p-other.
    for (resource_type, key, expected) in [
        (
            "Observation",
            "subject:Patient.gender:not-in",
            vec!["o-match", "o-other"],
        ),
        (
            "Patient",
            "_has:Observation:subject:code:not-in",
            vec!["p-other"],
        ),
    ] {
        let before = requests.lock().unwrap().len();
        let response = server
            .get(&format!("/{resource_type}"))
            .add_query_param(key, "http://example.org/vs")
            .await;
        assert_eq!(response.status_code(), StatusCode::OK, "{key}");
        assert_eq!(ids(&response.json()), expected, "{key}");
        assert_eq!(requests.lock().unwrap().len(), before + 1, "{key}");
    }

    // Valid on a token terminal: expanded, one `$expand` call each. The first
    // is an untyped hop over a polymorphic reference: `type` is a token on
    // every `Observation.subject` target that defines it.
    let device_type = "http://snomed.info/sct|706767009";
    for (resource_type, key, value, expected) in [
        (
            "Observation",
            "subject.type:in",
            "http://example.org/vs",
            "o-dev",
        ),
        (
            "Observation",
            "subject:Device.type:in",
            "http://example.org/vs",
            "o-dev",
        ),
        (
            "Observation",
            "subject:Device.type:above",
            device_type,
            "o-dev",
        ),
        (
            "Observation",
            "subject:Device.type:below",
            device_type,
            "o-dev",
        ),
        (
            "DiagnosticReport",
            "result.subject:Device.type:in",
            "http://example.org/vs",
            "d-dev",
        ),
        (
            "DiagnosticReport",
            "result.code:below",
            "http://loinc.org|1234-5",
            "d-match",
        ),
        (
            "Patient",
            "_has:Observation:subject:code:in",
            "http://example.org/vs",
            "p-match",
        ),
        (
            "Patient",
            "_has:Observation:subject:code:above",
            "http://loinc.org|1234-5",
            "p-match",
        ),
    ] {
        let before = requests.lock().unwrap().len();
        let response = server
            .get(&format!("/{resource_type}"))
            .add_query_param(key, value)
            .await;
        assert_eq!(response.status_code(), StatusCode::OK, "{key}");
        assert_eq!(ids(&response.json()), [expected], "{key}");
        assert_eq!(
            requests.lock().unwrap().len(),
            before + 1,
            "{key}: expected one $expand call"
        );
    }
}

/// When the terminology server is unreachable, a chained `:in` fails closed as
/// a direct one does — a `503` naming the chained parameter — and fails open
/// only under `HFS_SEARCH_TERMINOLOGY_FAIL=open`, where the chained parameter
/// is dropped and the search goes on without it.
#[tokio::test]
async fn test_chained_in_modifier_fails_closed_on_hts_unavailable() {
    let data_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data");
    let backend = SqliteBackend::with_config(
        ":memory:",
        helios_persistence::backends::sqlite::SqliteBackendConfig {
            data_dir: Some(data_dir),
            ..Default::default()
        },
    )
    .expect("SQLite in-memory failed");
    backend.init_schema().expect("Schema init failed");
    let config = ServerConfig {
        // Nothing listens here.
        terminology_server: Some("http://127.0.0.1:19999".to_string()),
        ..ServerConfig::for_testing()
    };
    let server = TestServer::new(create_app_with_config(backend, config)).unwrap();

    for resource in [
        json!({"resourceType": "Patient", "id": "p1", "gender": "female"}),
        json!({"resourceType": "Observation", "id": "o1", "status": "final",
               "code": {"coding": [{"system": "http://loinc.org", "code": "1234-5"}]},
               "subject": {"reference": "Patient/p1"}}),
        json!({"resourceType": "Observation", "id": "o2", "status": "final",
               "code": {"coding": [{"system": "http://loinc.org", "code": "9999-9"}]}}),
    ] {
        let path = format!(
            "/{}/{}",
            resource["resourceType"].as_str().unwrap(),
            resource["id"].as_str().unwrap()
        );
        assert!(
            server
                .put(&path)
                .json(&resource)
                .await
                .status_code()
                .is_success()
        );
    }

    // Positive control: the chain constrains the search.
    let response = server
        .get("/Observation")
        .add_query_param("subject.gender", "female")
        .await;
    assert_eq!(
        response.json::<Value>()["entry"].as_array().unwrap().len(),
        1
    );

    let cases = [
        ("Observation", "subject.gender:in", None, 2),
        (
            "Observation",
            "subject:Patient.gender:in",
            Some(("code", "9999-9")),
            1,
        ),
        ("Patient", "_has:Observation:subject:code:in", None, 1),
    ];

    // Default: closed, naming the whole chained key.
    for (resource_type, key, other, _) in cases {
        let mut request = server
            .get(&format!("/{resource_type}"))
            .add_query_param(key, "http://example.org/vs");
        if let Some((k, v)) = other {
            request = request.add_query_param(k, v);
        }
        let response = request.await;
        assert_eq!(
            response.status_code(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{key}"
        );
        let issue = response.json::<Value>()["issue"][0].to_string();
        let display = key.trim_end_matches(":in");
        assert!(issue.contains(&format!("'{display}'")), "{key}: {issue}");
    }

    // Opt-in fail-open: the chained parameter is dropped.
    let mut config = ServerConfig {
        terminology_server: Some("http://127.0.0.1:19999".to_string()),
        ..ServerConfig::for_testing()
    };
    config.search_terminology.fail = "open".to_string();
    let server = create_hfs_with_spec_params(config);
    for resource in [
        json!({"resourceType": "Patient", "id": "p1", "gender": "female"}),
        json!({"resourceType": "Observation", "id": "o1", "status": "final",
               "code": {"coding": [{"system": "http://loinc.org", "code": "1234-5"}]},
               "subject": {"reference": "Patient/p1"}}),
        json!({"resourceType": "Observation", "id": "o2", "status": "final",
               "code": {"coding": [{"system": "http://loinc.org", "code": "9999-9"}]}}),
    ] {
        let path = format!(
            "/{}/{}",
            resource["resourceType"].as_str().unwrap(),
            resource["id"].as_str().unwrap()
        );
        assert!(
            server
                .put(&path)
                .json(&resource)
                .await
                .status_code()
                .is_success()
        );
    }
    for (resource_type, key, other, expected) in cases {
        let mut request = server
            .get(&format!("/{resource_type}"))
            .add_query_param(key, "http://example.org/vs");
        if let Some((k, v)) = other {
            request = request.add_query_param(k, v);
        }
        let response = request.await;
        assert_eq!(response.status_code(), StatusCode::OK, "{key}");
        let body: Value = response.json();
        assert_eq!(
            body["entry"].as_array().map_or(0, Vec::len),
            expected,
            "{key}: {body}"
        );
    }
}
