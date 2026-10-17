//! Administrator credential for state-changing terminology requests
//! (`HTS_REQUIRE_AUTH_FOR_WRITES`).
//!
//! On a terminology server, creating, replacing and deleting CodeSystems,
//! ValueSets and ConceptMaps, `POST /import` and write entries in a
//! batch/transaction are administrative. With the switch on (the default
//! once `HTS_ADMIN_PASSWORD` is set) they need the administrator credential —
//! `Authorization: Basic`/`Bearer`, the same as `/admin`. Reads and the
//! operations that persist nothing stay public, including their `POST` forms:
//! `$lookup`, `$validate-code`, `$subsumes`, `$expand`, `$batch-validate-code`,
//! `$translate` and `$closure` (which computes the transitive closure of the
//! codes it is given and stores no table).
//!
//! A `POST /` batch made only of reads (`GET` entries, or `POST` entries to
//! one of those operations) is a read too: HFS's validator sends its
//! `$validate-code` checks that way.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use helios_admin_auth::{AdminGate, Guard, Style, is_read_only, under};
use serde_json::Value;

/// Operations that persist nothing, public even when posted.
pub const READ_OPERATIONS: &[&str] = &[
    "$lookup",
    "$validate-code",
    "$subsumes",
    "$expand",
    "$batch-validate-code",
    "$translate",
    "$closure",
];

/// Whether `(method, path)` changes terminology state. `/ui` and `/admin`
/// are guarded by their own layers and are not classified here.
pub fn is_terminology_write(method: &Method, path: &str) -> bool {
    if is_read_only(method) || under(path, "/ui") || under(path, "/admin") {
        return false;
    }
    !is_read_operation(path)
}

/// Whether the last path segment of `path_or_url` (query dropped) names a
/// read-only operation.
fn is_read_operation(path_or_url: &str) -> bool {
    let path = path_or_url.split(['?', '#']).next().unwrap_or("");
    let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    READ_OPERATIONS.contains(&last)
}

/// Whether a batch/transaction Bundle only reads.
fn is_read_only_bundle(body: &[u8]) -> bool {
    let Ok(bundle) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    if bundle.get("resourceType").and_then(Value::as_str) != Some("Bundle") {
        return false;
    }
    let Some(entries) = bundle.get("entry").and_then(Value::as_array) else {
        return false;
    };
    entries.iter().all(|entry| {
        let method = entry
            .pointer("/request/method")
            .and_then(Value::as_str)
            .unwrap_or("");
        let url = entry
            .pointer("/request/url")
            .and_then(Value::as_str)
            .unwrap_or("");
        match method.to_ascii_uppercase().as_str() {
            "GET" | "HEAD" => true,
            "POST" => is_read_operation(url),
            _ => false,
        }
    })
}

/// State of [`guard`].
#[derive(Clone)]
pub struct WriteGuard {
    /// The gate for terminology writes (`Open` when the switch is off).
    pub gate: AdminGate,
    /// Largest batch body read to classify it (`HTS_MAX_BODY_SIZE`).
    pub max_body_size: usize,
}

/// Middleware applying [`WriteGuard`]. Sits inside the request
/// decompression layer, so a batch body is read decompressed.
pub async fn guard(State(state): State<WriteGuard>, request: Request, next: Next) -> Response {
    if matches!(state.gate, AdminGate::Open)
        || !is_terminology_write(request.method(), request.uri().path())
    {
        return next.run(request).await;
    }
    let mut request = request;
    if request.method() == Method::POST && request.uri().path() == "/" {
        let (parts, body) = request.into_parts();
        let bytes = match axum::body::to_bytes(body, state.max_body_size).await {
            Ok(bytes) => bytes,
            Err(_) => {
                return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response();
            }
        };
        let read_only = is_read_only_bundle(&bytes);
        let rebuilt = Request::from_parts(parts, Body::from(bytes));
        if read_only {
            return next.run(rebuilt).await;
        }
        request = rebuilt;
    }
    helios_admin_auth::guard(
        State(Guard {
            gate: state.gate,
            protects: is_terminology_write,
            style: Style::Api { fhir: true },
        }),
        request,
        next,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_reads_are_told_apart() {
        let post = Method::POST;
        for path in [
            "/",
            "/import",
            "/CodeSystem",
            "/ValueSet",
            "/ConceptMap",
            "/CodeSystem/x/$something-new",
        ] {
            assert!(is_terminology_write(&post, path), "POST {path}");
        }
        assert!(is_terminology_write(&Method::PUT, "/CodeSystem/x"));
        assert!(is_terminology_write(&Method::DELETE, "/ValueSet/x"));
        assert!(is_terminology_write(&Method::PATCH, "/ConceptMap/x"));
        for path in [
            "/CodeSystem/$lookup",
            "/CodeSystem/x/$lookup",
            "/CodeSystem/$validate-code",
            "/CodeSystem/$subsumes",
            "/ValueSet/$expand",
            "/ValueSet/x/$expand",
            "/ValueSet/$validate-code",
            "/ValueSet/$batch-validate-code",
            "/ConceptMap/$translate",
            "/ConceptMap/$closure",
            "/ui/hts/code-systems/x/lookup",
            "/admin/imports",
        ] {
            assert!(!is_terminology_write(&post, path), "POST {path}");
        }
        assert!(!is_terminology_write(&Method::GET, "/CodeSystem"));
        assert!(!is_terminology_write(&Method::DELETE, "/admin/value-sets"));
    }

    #[test]
    fn read_only_batches_are_recognised() {
        let reads = br#"{"resourceType":"Bundle","type":"batch","entry":[
            {"request":{"method":"POST","url":"ValueSet/$validate-code"},"resource":{}},
            {"request":{"method":"GET","url":"CodeSystem?url=x"}},
            {"request":{"method":"POST","url":"CodeSystem/$lookup?x=1"}}]}"#;
        assert!(is_read_only_bundle(reads));
        let write = br#"{"resourceType":"Bundle","type":"batch","entry":[
            {"request":{"method":"POST","url":"ValueSet/$validate-code"}},
            {"request":{"method":"PUT","url":"CodeSystem/x"},"resource":{}}]}"#;
        assert!(!is_read_only_bundle(write));
        let create = br#"{"resourceType":"Bundle","type":"transaction","entry":[
            {"request":{"method":"POST","url":"CodeSystem"},"resource":{}}]}"#;
        assert!(!is_read_only_bundle(create));
        assert!(!is_read_only_bundle(b"not json"));
        assert!(!is_read_only_bundle(br#"{"resourceType":"Bundle"}"#));
    }
}
