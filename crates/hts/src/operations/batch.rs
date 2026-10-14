//! Handler for `POST /` — standard FHIR batch/transaction endpoint.
//!
//! Accepts a FHIR Bundle with `type` of `"batch"` or `"transaction"` and
//! dispatches each `entry` to the matching operation handler.  Each entry's
//! result is collected into a `batch-response` Bundle.
//!
//! # Supported entries
//!
//! `entry.request.method` + `entry.request.url` select the handler, exactly as
//! the same request would be routed over plain HTTP:
//!
//! | method | `entry.request.url`                         | Dispatched to                    |
//! |--------|---------------------------------------------|----------------------------------|
//! | POST   | `CodeSystem/$lookup` … `ConceptMap/$translate` (Parameters body) | the operation |
//! | GET    | `ValueSet/$expand?url=…`, `CodeSystem/$lookup?system=…`, …      | the operation |
//! | GET    | `CodeSystem/{id}`, `ValueSet/{id}`, `ConceptMap/{id}`             | read          |
//! | POST   | `CodeSystem`, `ValueSet`, `ConceptMap` (resource body)            | create        |
//! | PUT    | `CodeSystem/{id}`, …                                              | update        |
//! | DELETE | `CodeSystem/{id}`, …                                              | delete        |
//!
//! A missing `method` defaults to `POST`. Unsupported combinations return a
//! `400` entry-level OperationOutcome without failing the overall batch. A
//! `transaction` Bundle is processed entry by entry like a `batch` (the
//! terminology store has no multi-resource rollback).
//!
//! # FHIR specification
//! <https://hl7.org/fhir/http.html#transaction>

use axum::{
    Json,
    extract::{RawQuery, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use serde_json::{Value, json};

use crate::error::HtsError;
use crate::state::AppState;
use crate::traits::TerminologyBackend;

use super::expand::process_expand;
use super::format::{ResponseFormat, fhir_respond, negotiate_format};
use super::lookup::process_lookup;
use super::params::{extract_parameter_array, parse_query_string, query_params_to_fhir_params};
use super::subsumes::process_subsumes;
use super::translate::process_translate;
use super::validate_code::{process_validate_code, process_vs_validate_code};

// ── Error helpers ──────────────────────────────────────────────────────────────

/// HTTP status code (as a numeric string) for a given `HtsError`.
fn error_status(e: &HtsError) -> &'static str {
    match e {
        HtsError::NotFound(_) => "404",
        HtsError::NotSupported(_) => "501",
        HtsError::InvalidRequest(_) | HtsError::VsInvalid(_) => "400",
        HtsError::Internal(_) | HtsError::StorageError(_) => "500",
        HtsError::PreconditionFailed(_) => "412",
        HtsError::TooCostly(_) => "422",
    }
}

/// Build a FHIR OperationOutcome JSON value from an `HtsError`.
fn error_to_outcome(e: &HtsError) -> Value {
    let (code, diagnostics) = match e {
        HtsError::NotFound(msg) => ("not-found", msg.as_str()),
        HtsError::NotSupported(msg) => ("not-supported", msg.as_str()),
        // VsInvalid maps to FHIR issue code `invalid` (same as InvalidRequest)
        // but carries a distinct `tx-issue-type=vs-invalid` coding in the
        // top-level error response (see `HtsError::into_response`). Inside a
        // batch entry the fine-grained tx-issue-type is not surfaced; the
        // entry-level `invalid` code is sufficient for batch reporting.
        HtsError::InvalidRequest(msg) | HtsError::VsInvalid(msg) => ("invalid", msg.as_str()),
        HtsError::Internal(msg) => ("exception", msg.as_str()),
        HtsError::StorageError(msg) => ("exception", msg.as_str()),
        HtsError::PreconditionFailed(msg) => ("conflict", msg.as_str()),
        HtsError::TooCostly(msg) => ("too-costly", msg.as_str()),
    };
    json!({
        "resourceType": "OperationOutcome",
        "issue": [{"severity": "error", "code": code, "diagnostics": diagnostics}]
    })
}

// ── Entry dispatch ─────────────────────────────────────────────────────────────

/// Outcome of one batch entry: HTTP status (as FHIR wants it, a string) and
/// the response body (`None` for 204).
struct EntryResult {
    status: String,
    body: Option<Value>,
}

impl EntryResult {
    fn ok(body: Value) -> Self {
        Self {
            status: "200".into(),
            body: Some(body),
        }
    }
}

/// Split `entry.request.url` into `(resource_type, id, operation, query)`.
/// Accepts `Type`, `Type/$op`, `Type/{id}`, each with an optional `?query`
/// and an optional leading `/`.
fn parse_entry_url(url: &str) -> (String, Option<String>, Option<String>, Option<String>) {
    let url = url.trim_start_matches('/');
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p, Some(q.to_string())),
        None => (url, None),
    };
    let mut segs = path.split('/').filter(|s| !s.is_empty());
    let resource_type = segs.next().unwrap_or_default().to_string();
    let mut id = None;
    let mut op = None;
    for seg in segs {
        if let Some(name) = seg.strip_prefix('$') {
            op = Some(name.to_string());
        } else if id.is_none() {
            id = Some(seg.to_string());
        }
    }
    (resource_type, id, op, query)
}

/// Turn an HTTP `Response` produced by a CRUD handler into an entry result.
async fn response_to_entry(response: Response) -> Result<EntryResult, HtsError> {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(|e| HtsError::Internal(format!("batch entry body: {e}")))?;
    let body = if bytes.is_empty() {
        None
    } else {
        Some(
            serde_json::from_slice::<Value>(&bytes)
                .map_err(|e| HtsError::Internal(format!("batch entry body: {e}")))?,
        )
    };
    Ok(EntryResult {
        status: status.as_u16().to_string(),
        body,
    })
}

/// Run a terminology operation by name against a parameter list.
async fn dispatch_operation<B: TerminologyBackend>(
    state: &AppState<B>,
    resource_type: &str,
    op: &str,
    params: Vec<Value>,
) -> Result<Value, HtsError> {
    match (resource_type, op) {
        ("CodeSystem", "lookup") => process_lookup(state, params).await,
        ("CodeSystem", "validate-code") => process_validate_code(state, params).await,
        ("CodeSystem", "subsumes") => process_subsumes(state, params).await,
        ("ValueSet", "validate-code") => process_vs_validate_code(state, params).await,
        ("ValueSet", "expand") => {
            let bytes = process_expand(state, params).await?;
            serde_json::from_slice::<Value>(&bytes)
                .map_err(|e| HtsError::Internal(format!("expand response: {e}")))
        }
        ("ConceptMap", "translate") => process_translate(state, params).await,
        _ => Err(HtsError::InvalidRequest(format!(
            "Unsupported batch operation: {resource_type}/${op}. Supported: CodeSystem/$lookup, \
             CodeSystem/$validate-code, CodeSystem/$subsumes, ValueSet/$expand, \
             ValueSet/$validate-code, ConceptMap/$translate"
        ))),
    }
}

/// Dispatch a single batch entry to the handler its `request.method` +
/// `request.url` would reach over HTTP.
///
/// Returns `Ok(result)` on success or `Err(HtsError)` on failure. Both
/// outcomes are captured as entry-level results in the batch response —
/// neither aborts the remaining entries.
async fn dispatch_entry<B: TerminologyBackend + Clone>(
    state: &AppState<B>,
    method: &str,
    url: &str,
    resource: Value,
) -> Result<EntryResult, HtsError> {
    let (resource_type, id, op, query) = parse_entry_url(url);
    if !matches!(
        resource_type.as_str(),
        "CodeSystem" | "ValueSet" | "ConceptMap"
    ) {
        return Err(HtsError::InvalidRequest(format!(
            "Unsupported batch entry url: {url}"
        )));
    }
    // `&'static str` for the CRUD helpers, which key on the type name.
    let rt: &'static str = match resource_type.as_str() {
        "CodeSystem" => "CodeSystem",
        "ValueSet" => "ValueSet",
        _ => "ConceptMap",
    };
    let method = method.to_ascii_uppercase();

    match (method.as_str(), op.as_deref(), id) {
        // Operations: POST with a Parameters body, or GET with a query string.
        ("POST", Some(op), None) => {
            let params = extract_parameter_array(&resource)?;
            dispatch_operation(state, rt, op, params)
                .await
                .map(EntryResult::ok)
        }
        ("GET", Some(op), None) => {
            let params =
                query_params_to_fhir_params(parse_query_string(query.as_deref().unwrap_or("")));
            dispatch_operation(state, rt, op, params)
                .await
                .map(EntryResult::ok)
        }
        // Instance-level operations need the id resolved to a canonical URL.
        ("GET", Some(op), Some(id)) => {
            let canonical = state
                .backend()
                .resource_url_by_id(rt, &id)
                .ok_or_else(|| HtsError::NotFound(format!("{rt}/{id}")))?;
            let mut params =
                query_params_to_fhir_params(parse_query_string(query.as_deref().unwrap_or("")));
            let key = if rt == "CodeSystem" { "system" } else { "url" };
            params.retain(|p| p.get("name").and_then(|v| v.as_str()) != Some(key));
            params.push(json!({"name": key, "valueUri": canonical}));
            dispatch_operation(state, rt, op, params)
                .await
                .map(EntryResult::ok)
        }
        // CRUD
        ("POST", None, None) => {
            if resource.get("resourceType").and_then(|v| v.as_str()) != Some(rt) {
                return Err(HtsError::InvalidRequest(format!(
                    "Batch entry for POST {rt} must carry a {rt} resource"
                )));
            }
            let response =
                super::crud::create_resource(rt, state.clone(), resource, ResponseFormat::Json)
                    .await?;
            response_to_entry(response).await
        }
        ("GET", None, Some(id)) => {
            let response =
                super::crud::read_resource(rt, state.clone(), id, ResponseFormat::Json).await?;
            response_to_entry(response).await
        }
        ("PUT", None, Some(id)) => {
            if resource.get("resourceType").and_then(|v| v.as_str()) != Some(rt) {
                return Err(HtsError::InvalidRequest(format!(
                    "Batch entry for PUT {rt}/{id} must carry a {rt} resource"
                )));
            }
            let response = super::crud::update_resource(
                rt,
                state.clone(),
                id,
                None,
                resource,
                ResponseFormat::Json,
            )
            .await?;
            response_to_entry(response).await
        }
        ("DELETE", None, Some(id)) => {
            let response = super::crud::delete_resource(rt, state.clone(), id).await?;
            let mut entry = response_to_entry(response).await?;
            if entry.status == StatusCode::NO_CONTENT.as_u16().to_string() {
                entry.body = None;
            }
            Ok(entry)
        }
        _ => Err(HtsError::InvalidRequest(format!(
            "Unsupported batch entry: {method} {url}"
        ))),
    }
}

// ── Handler ────────────────────────────────────────────────────────────────────

/// `POST /` — FHIR batch / transaction endpoint.
///
/// Each `entry` is dispatched independently.  Errors in individual entries do
/// **not** abort processing of subsequent entries — they are recorded as
/// `response.status: "4xx"` entries in the `batch-response` Bundle.
pub async fn batch_handler<B: TerminologyBackend + Clone>(
    State(state): State<AppState<B>>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, HtsError> {
    // ── Validate the outer Bundle ─────────────────────────────────────────────
    let rt = body["resourceType"].as_str().unwrap_or("");
    if rt != "Bundle" {
        return Err(HtsError::InvalidRequest(format!(
            "Expected resourceType 'Bundle', got '{rt}'"
        )));
    }

    let bundle_type = body["type"].as_str().unwrap_or("");
    if bundle_type != "batch" && bundle_type != "transaction" {
        return Err(HtsError::InvalidRequest(format!(
            "Bundle type must be 'batch' or 'transaction', got '{bundle_type}'"
        )));
    }

    // ── Process entries ───────────────────────────────────────────────────────
    let entries = body["entry"].as_array().cloned().unwrap_or_default();
    let mut response_entries: Vec<Value> = Vec::with_capacity(entries.len());

    for entry in &entries {
        let url = entry["request"]["url"].as_str().unwrap_or("");
        let method = entry["request"]["method"].as_str().unwrap_or("POST");
        let resource = entry.get("resource").cloned().unwrap_or(Value::Null);

        let response_entry = match dispatch_entry(&state, method, url, resource).await {
            Ok(EntryResult { status, body }) => {
                let mut e = json!({"response": { "status": status }});
                if let Some(body) = body {
                    e["resource"] = body;
                }
                e
            }
            Err(e) => json!({
                "resource": error_to_outcome(&e),
                "response": { "status": error_status(&e) }
            }),
        };
        response_entries.push(response_entry);
    }

    // ── Build batch-response Bundle ───────────────────────────────────────────
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    let format = negotiate_format(raw.as_deref(), accept);
    Ok(fhir_respond(
        json!({
            "resourceType": "Bundle",
            "type": "batch-response",
            "entry": response_entries
        }),
        format,
    ))
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, http::Request, routing::post};
    use tower::ServiceExt;

    use crate::backends::sqlite::SqliteTerminologyBackend;
    use crate::state::AppState;

    fn make_app() -> Router {
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        {
            let conn = backend.pool().get().unwrap();
            conn.execute_batch(
                "INSERT INTO code_systems
                     (id, url, version, name, status, content, created_at, updated_at)
                 VALUES ('cs1', 'http://example.org/cs', '1.0', 'Test CS',
                         'active', 'complete', '2024-01-01', '2024-01-01');

                 INSERT INTO concepts (id, system_id, code, display)
                 VALUES (1, 'cs1', 'A', 'Alpha'),
                        (2, 'cs1', 'B', 'Beta');

                 INSERT INTO value_sets
                     (id, url, name, status, compose_json, created_at, updated_at)
                 VALUES ('vs1', 'http://example.org/vs', 'TestVS', 'active',
                         '{\"include\":[{\"system\":\"http://example.org/cs\",\"concept\":[{\"code\":\"A\"}]}]}',
                         '2024-01-01', '2024-01-01');

                 INSERT INTO concept_maps
                     (id, url, name, status, source_uri, target_uri, created_at, resource_json)
                 VALUES ('cm1', 'http://example.org/cm', 'TestCM', 'active',
                         'http://example.org/cs', 'http://example.org/cs2',
                         '2024-01-01', '{\"resourceType\":\"ConceptMap\"}');

                 INSERT INTO concept_map_elements
                     (id, map_id, source_system, source_code, target_system, target_code, equivalence)
                 VALUES (1, 'cm1', 'http://example.org/cs', 'A',
                         'http://example.org/cs2', 'A2', 'equivalent');",
            )
            .unwrap();
        }
        let state = AppState::new(backend);
        Router::new()
            .route("/", post(batch_handler::<SqliteTerminologyBackend>))
            .with_state(state)
    }

    async fn post_json(app: Router, body: Value) -> axum::response::Response {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header("content-type", "application/fhir+json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    async fn body_json(response: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // ── Happy path ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn batch_all_valid_entries_returns_200_entries() {
        let app = make_app();
        let body = json!({
            "resourceType": "Bundle",
            "type": "batch",
            "entry": [
                {
                    "resource": {
                        "resourceType": "Parameters",
                        "parameter": [
                            {"name": "url", "valueUri": "http://example.org/cs"},
                            {"name": "code", "valueCode": "A"}
                        ]
                    },
                    "request": {"method": "POST", "url": "CodeSystem/$validate-code"}
                },
                {
                    "resource": {
                        "resourceType": "Parameters",
                        "parameter": [
                            {"name": "url", "valueUri": "http://example.org/vs"},
                            {"name": "code", "valueCode": "A"}
                        ]
                    },
                    "request": {"method": "POST", "url": "ValueSet/$validate-code"}
                },
                {
                    "resource": {
                        "resourceType": "Parameters",
                        "parameter": [
                            {"name": "url", "valueUri": "http://example.org/cm"},
                            {"name": "code", "valueCode": "A"}
                        ]
                    },
                    "request": {"method": "POST", "url": "ConceptMap/$translate"}
                }
            ]
        });

        let resp = post_json(app, body).await;
        assert_eq!(resp.status(), 200);

        let json = body_json(resp).await;
        assert_eq!(json["resourceType"], "Bundle");
        assert_eq!(json["type"], "batch-response");

        let entries = json["entry"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        for e in entries {
            assert_eq!(
                e["response"]["status"], "200",
                "expected 200 for entry: {e}"
            );
        }
    }

    #[tokio::test]
    async fn batch_invalid_entry_returns_400_others_succeed() {
        let app = make_app();
        let body = json!({
            "resourceType": "Bundle",
            "type": "batch",
            "entry": [
                {
                    "resource": {
                        "resourceType": "Parameters",
                        "parameter": [
                            {"name": "url", "valueUri": "http://example.org/cs"},
                            {"name": "code", "valueCode": "A"}
                        ]
                    },
                    "request": {"method": "POST", "url": "CodeSystem/$validate-code"}
                },
                {
                    "resource": {
                        "resourceType": "Parameters",
                        "parameter": []
                    },
                    "request": {"method": "POST", "url": "CodeSystem/$validate-code"}
                },
                {
                    "resource": {
                        "resourceType": "Parameters",
                        "parameter": [
                            {"name": "url", "valueUri": "http://example.org/vs"},
                            {"name": "code", "valueCode": "A"}
                        ]
                    },
                    "request": {"method": "POST", "url": "ValueSet/$validate-code"}
                }
            ]
        });

        let resp = post_json(app, body).await;
        assert_eq!(resp.status(), 200);

        let json = body_json(resp).await;
        let entries = json["entry"].as_array().unwrap();
        assert_eq!(entries.len(), 3);

        // First entry: valid → 200
        assert_eq!(entries[0]["response"]["status"], "200");

        // Second entry: missing required params → 400
        assert_eq!(entries[1]["response"]["status"], "400");
        assert_eq!(
            entries[1]["resource"]["resourceType"], "OperationOutcome",
            "error entry should have OperationOutcome resource"
        );

        // Third entry: valid → 200
        assert_eq!(entries[2]["response"]["status"], "200");
    }

    // ── Unsupported operation ──────────────────────────────────────────────────

    #[tokio::test]
    async fn batch_unsupported_url_returns_400_entry() {
        let app = make_app();
        let body = json!({
            "resourceType": "Bundle",
            "type": "batch",
            "entry": [{
                "resource": {"resourceType": "Parameters", "parameter": []},
                "request": {"method": "POST", "url": "CodeSystem/$find-matches"}
            }]
        });

        let resp = post_json(app, body).await;
        assert_eq!(resp.status(), 200);

        let json = body_json(resp).await;
        let entries = json["entry"].as_array().unwrap();
        assert_eq!(entries[0]["response"]["status"], "400");
    }

    #[tokio::test]
    async fn batch_get_entries_run_lookup_and_expand() {
        let app = make_app();
        let body = json!({
            "resourceType": "Bundle",
            "type": "batch",
            "entry": [
                {"request": {"method": "GET", "url": "CodeSystem/$lookup?system=http://example.org/cs&code=A"}},
                {"request": {"method": "GET", "url": "/ValueSet/$expand?url=http://example.org/vs"}},
                {"request": {"method": "GET", "url": "CodeSystem/$lookup?system=http://example.org/cs&code=NOPE"}}
            ]
        });

        let resp = post_json(app, body).await;
        assert_eq!(resp.status(), 200);

        let json = body_json(resp).await;
        let entries = json["entry"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["response"]["status"], "200", "{json}");
        assert_eq!(entries[0]["resource"]["resourceType"], "Parameters");
        assert_eq!(entries[1]["response"]["status"], "200", "{json}");
        assert_eq!(entries[1]["resource"]["resourceType"], "ValueSet");
        assert_eq!(
            entries[1]["resource"]["expansion"]["contains"][0]["code"],
            "A"
        );
        assert_eq!(entries[2]["response"]["status"], "404", "{json}");
    }

    #[test]
    fn parse_entry_url_shapes() {
        assert_eq!(
            parse_entry_url("CodeSystem/$lookup?system=x&code=y"),
            (
                "CodeSystem".to_string(),
                None,
                Some("lookup".to_string()),
                Some("system=x&code=y".to_string())
            )
        );
        assert_eq!(
            parse_entry_url("/ValueSet/abc"),
            ("ValueSet".to_string(), Some("abc".to_string()), None, None)
        );
        assert_eq!(
            parse_entry_url("ValueSet/abc/$expand"),
            (
                "ValueSet".to_string(),
                Some("abc".to_string()),
                Some("expand".to_string()),
                None
            )
        );
        assert_eq!(
            parse_entry_url("ConceptMap"),
            ("ConceptMap".to_string(), None, None, None)
        );
    }

    // ── Empty batch ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn batch_empty_entries_returns_empty_response_bundle() {
        let app = make_app();
        let body = json!({
            "resourceType": "Bundle",
            "type": "batch",
            "entry": []
        });

        let resp = post_json(app, body).await;
        assert_eq!(resp.status(), 200);

        let json = body_json(resp).await;
        assert_eq!(json["type"], "batch-response");
        assert_eq!(json["entry"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn batch_missing_entry_field_treated_as_empty() {
        let app = make_app();
        let body = json!({"resourceType": "Bundle", "type": "batch"});

        let resp = post_json(app, body).await;
        assert_eq!(resp.status(), 200);

        let json = body_json(resp).await;
        assert_eq!(json["type"], "batch-response");
    }

    // ── Validation errors ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn non_bundle_resource_returns_400() {
        let app = make_app();
        let body = json!({"resourceType": "Parameters", "parameter": []});

        let resp = post_json(app, body).await;
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn wrong_bundle_type_returns_400() {
        let app = make_app();
        let body = json!({"resourceType": "Bundle", "type": "searchset", "entry": []});

        let resp = post_json(app, body).await;
        assert_eq!(resp.status(), 400);
    }

    // ── transaction type also accepted ────────────────────────────────────────

    #[tokio::test]
    async fn transaction_type_bundle_accepted() {
        let app = make_app();
        let body = json!({
            "resourceType": "Bundle",
            "type": "transaction",
            "entry": [{
                "resource": {
                    "resourceType": "Parameters",
                    "parameter": [
                        {"name": "url", "valueUri": "http://example.org/cs"},
                        {"name": "code", "valueCode": "A"}
                    ]
                },
                "request": {"method": "POST", "url": "CodeSystem/$validate-code"}
            }]
        });

        let resp = post_json(app, body).await;
        assert_eq!(resp.status(), 200);

        let json = body_json(resp).await;
        assert_eq!(json["type"], "batch-response");
    }
}
