//! Error types for the Helios Terminology Server.
//!
//! All fallible operations in HTS return [`HtsError`].  The [`IntoResponse`]
//! implementation converts each variant to the appropriate HTTP status code and
//! a FHIR `OperationOutcome` response body, so handlers can propagate errors
//! with the `?` operator without any extra mapping.
//!
//! ## HTTP mapping
//!
//! | Variant | HTTP status | FHIR issue code | tx-issue-type |
//! |---------|-------------|-----------------|---------------|
//! | [`NotFound`] | 404 | `not-found` | `not-found` |
//! | [`NotSupported`] | 501 | `not-supported` | `not-supported` |
//! | [`InvalidRequest`] | 400 | `invalid` | `invalid-data` |
//! | [`Coded`] | 400 | as given | as given |
//! | [`VsInvalid`] | 400 | `invalid` | `vs-invalid` |
//! | [`Internal`] | 500 | `exception` | `exception` |
//! | [`StorageError`] | 500 | `exception` | `exception` |
//! | [`PreconditionFailed`] | 412 | `conflict` | `conflict` |
//! | [`TooCostly`] | 422 | `too-costly` | `too-costly` |
//!
//! [`NotFound`]: HtsError::NotFound
//! [`NotSupported`]: HtsError::NotSupported
//! [`InvalidRequest`]: HtsError::InvalidRequest
//! [`VsInvalid`]: HtsError::VsInvalid
//! [`Internal`]: HtsError::Internal
//! [`StorageError`]: HtsError::StorageError
//! [`PreconditionFailed`]: HtsError::PreconditionFailed
//! [`TooCostly`]: HtsError::TooCostly
//! [`Coded`]: HtsError::Coded

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use thiserror::Error;

/// All errors that the HTS service can produce.
///
/// Each variant carries a human-readable diagnostic message that is embedded
/// verbatim in the `OperationOutcome.issue[0].details.text` field of the HTTP
/// response.  See the module-level documentation for the HTTP status code each
/// variant maps to.
#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum HtsError {
    /// A requested resource (CodeSystem, ValueSet, ConceptMap, or concept) does
    /// not exist in the store.  Maps to HTTP 404.
    #[error("Resource not found: {0}")]
    NotFound(String),

    /// The operation or parameter is valid FHIR but is not implemented by this
    /// server (e.g., SNOMED post-coordination expressions).  Maps to HTTP 501.
    #[error("Operation not supported: {0}")]
    NotSupported(String),

    /// The request body or parameters are malformed, missing required fields,
    /// or violate FHIR operation constraints.  Maps to HTTP 400.
    #[error("Invalid request: {0}")]
    InvalidRequest(String),

    /// A ValueSet definition is itself invalid — for example a compose filter
    /// that omits the required `value`, names an unknown operator, or supplies
    /// a regular expression that fails to compile.  Maps to HTTP 400 with FHIR
    /// issue code `invalid` and a `tx-issue-type=vs-invalid` coding so the
    /// HL7 tx-ecosystem fixtures can distinguish ValueSet-definition errors
    /// from other 400-class request problems.
    #[error("Invalid ValueSet: {0}")]
    VsInvalid(String),

    /// An unexpected server-side error that is not attributable to the caller.
    /// Maps to HTTP 500.
    #[error("Internal error: {0}")]
    Internal(String),

    /// A database or connection-pool error.  Maps to HTTP 500.
    #[error("Storage error: {0}")]
    StorageError(String),

    /// An optimistic-concurrency conflict: the `If-Match` version does not
    /// match the stored version.  Maps to HTTP 412.
    #[error("Precondition failed: {0}")]
    PreconditionFailed(String),

    /// The requested expansion would exceed the server's configured size limit
    /// (`HTS_MAX_EXPANSION_SIZE`).  Maps to HTTP 422 with FHIR issue code
    /// `too-costly`, per the FHIR `$expand` specification.
    #[error("Expansion too costly: {0}")]
    TooCostly(String),

    /// A problem with the request whose OperationOutcome issue needs an
    /// explicit classification — FHIR issue code, `tx-issue-type` code and
    /// FHIRPath expression — e.g. an unknown code passed to `$subsumes`
    /// (`code-invalid` / `invalid-code` at `codeA`). Maps to HTTP 400.
    #[error("{}", .0.text)]
    Coded(Box<CodedIssue>),
}

/// The issue carried by [`HtsError::Coded`].
#[derive(Debug, Clone)]
pub struct CodedIssue {
    /// FHIR `OperationOutcome.issue.code`.
    pub code: &'static str,
    /// `http://hl7.org/fhir/tools/CodeSystem/tx-issue-type` code.
    pub tx_code: &'static str,
    /// Human-readable `details.text`.
    pub text: String,
    /// FHIRPath `expression` naming the offending request parameter.
    pub expression: Option<String>,
}

impl HtsError {
    /// An [`HtsError::Coded`] error.
    pub fn coded(
        code: &'static str,
        tx_code: &'static str,
        text: impl Into<String>,
        expression: Option<&str>,
    ) -> Self {
        HtsError::Coded(Box::new(CodedIssue {
            code,
            tx_code,
            text: text.into(),
            expression: expression.map(str::to_owned),
        }))
    }

    /// The HTTP status this error is reported with.
    pub fn status(&self) -> StatusCode {
        match self {
            HtsError::NotFound(_) => StatusCode::NOT_FOUND,
            HtsError::NotSupported(_) => StatusCode::NOT_IMPLEMENTED,
            HtsError::InvalidRequest(_) | HtsError::VsInvalid(_) | HtsError::Coded(_) => {
                StatusCode::BAD_REQUEST
            }
            HtsError::Internal(_) | HtsError::StorageError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            HtsError::PreconditionFailed(_) => StatusCode::PRECONDITION_FAILED,
            HtsError::TooCostly(_) => StatusCode::UNPROCESSABLE_ENTITY,
        }
    }

    /// The FHIR `OperationOutcome` this error is reported as — the response
    /// body of a failed operation, and the resource of a failed batch entry.
    pub fn to_operation_outcome(&self) -> serde_json::Value {
        if let HtsError::TooCostly(msg) = self {
            // The IG `big/expand-no-limit-outcome` fixture expects the
            // `operationoutcome-message-id` extension `VALUESET_TOO_COSTLY`
            // (optional for tx.fhir.org) and, like every issue, a
            // `tx-issue-type` coding (`too-costly`) in `details`.
            return json!({
                "resourceType": "OperationOutcome",
                "issue": [{
                    "extension": [{
                        "url": "http://hl7.org/fhir/StructureDefinition/operationoutcome-message-id",
                        "valueString": "VALUESET_TOO_COSTLY",
                    }],
                    "severity": "error",
                    "code": "too-costly",
                    "details": {
                        "coding": [{
                            "system": "http://hl7.org/fhir/tools/CodeSystem/tx-issue-type",
                            "code": "too-costly",
                        }],
                        "text": msg,
                    },
                }]
            });
        }

        // (FHIR-issue-code, tx-issue-type, text, expression)
        // The FHIR issue `code` and the `tx-issue-type` coding usually differ
        // only in vocabulary: a malformed request is `invalid` / `invalid-data`
        // (tx-issue-type has no `invalid` code), a bad ValueSet definition is
        // `invalid` / `vs-invalid`.
        let (code, tx_issue_type, text, expression): (&str, &str, &str, Option<&str>) = match self {
            HtsError::NotFound(msg) => ("not-found", "not-found", msg.as_str(), None),
            HtsError::NotSupported(msg) => ("not-supported", "not-supported", msg.as_str(), None),
            HtsError::InvalidRequest(msg) => ("invalid", "invalid-data", msg.as_str(), None),
            HtsError::VsInvalid(msg) => ("invalid", "vs-invalid", msg.as_str(), None),
            HtsError::Internal(msg) | HtsError::StorageError(msg) => {
                ("exception", "exception", msg.as_str(), None)
            }
            HtsError::PreconditionFailed(msg) => ("conflict", "conflict", msg.as_str(), None),
            HtsError::Coded(issue) => (
                issue.code,
                issue.tx_code,
                issue.text.as_str(),
                issue.expression.as_deref(),
            ),
            HtsError::TooCostly(_) => unreachable!("handled above"),
        };

        // The message goes in `details.text` only, with the tx-issue-type
        // coding beside it. The HL7 IG validator strips issues that have
        // `diagnostics` but no `details`, and on some operations
        // (CodeSystem/$validate-code) rejects a `diagnostics` that
        // duplicates `details.text`.
        let mut issue = json!({
            "severity": "error",
            "code": code,
            "details": {
                "coding": [{
                    "system": "http://hl7.org/fhir/tools/CodeSystem/tx-issue-type",
                    "code": tx_issue_type,
                }],
                "text": text,
            },
        });
        if let Some(expr) = expression {
            issue["expression"] = json!([expr]);
        }
        json!({"resourceType": "OperationOutcome", "issue": [issue]})
    }
}

/// Converts an [`HtsError`] into an Axum HTTP response: the
/// [`HtsError::status`] with the [`HtsError::to_operation_outcome`] body.
impl IntoResponse for HtsError {
    fn into_response(self) -> Response {
        (self.status(), Json(self.to_operation_outcome())).into_response()
    }
}

#[cfg(test)]
mod outcome_tests {
    use super::*;

    fn tx_code(e: &HtsError) -> String {
        e.to_operation_outcome()["issue"][0]["details"]["coding"][0]["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    /// Every error issue carries a tx-issue-type coding from that code
    /// system (tx-ecosystem: "every issue is expected to carry both").
    #[test]
    fn every_error_carries_a_tx_issue_type() {
        assert_eq!(
            tx_code(&HtsError::InvalidRequest("x".into())),
            "invalid-data"
        );
        assert_eq!(tx_code(&HtsError::VsInvalid("x".into())), "vs-invalid");
        assert_eq!(tx_code(&HtsError::NotFound("x".into())), "not-found");
        assert_eq!(tx_code(&HtsError::TooCostly("x".into())), "too-costly");
        let coded = HtsError::coded("code-invalid", "invalid-code", "x", Some("codeA"));
        assert_eq!(tx_code(&coded), "invalid-code");
        assert_eq!(coded.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            coded.to_operation_outcome()["issue"][0]["expression"],
            json!(["codeA"])
        );
        // The text is in details.text only (no duplicate `diagnostics`).
        assert!(coded.to_operation_outcome()["issue"][0].get("diagnostics").is_none());
    }
}
