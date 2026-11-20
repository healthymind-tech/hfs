//! Error responses of the endpoint admin API.
//!
//! Every failure is a FHIR `OperationOutcome` (`application/fhir+json`), the
//! same shape the rest of the server uses:
//!
//! | Status | When | Issue code |
//! |---|---|---|
//! | `400` | malformed JSON, bad query, missing `confirm` | `invalid` / `structure` |
//! | `401` / `403` / `429` / `503` | authorization (see [`super::authz`]) | `login` / `forbidden` / `throttled` / `transient` |
//! | `404` | unknown endpoint, package or route resource | `not-found` |
//! | `409` | revision conflict (with `currentRevision`), duplicate name/path/tenant, forbidden change, no key ring | `conflict` / `duplicate` / `business-rule` |
//! | `412` | stale `If-Match` | `conflict` |
//! | `422` | validation: one issue per field, `expression` = field path | `invalid` / `value` / `security` |
//! | `428` | write without `If-Match` | `required` |
//! | `501` | hook not installed (duplicate, auth probe, audit search) | `not-supported` |
//!
//! A revision conflict also carries the stored revision as the extension
//! [`CURRENT_REVISION_URL`] and as the response `ETag`, so a client can
//! re-read and retry without a second round trip to learn the revision.

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use helios_endpoints::StoreError;
use helios_endpoints::model::ValidationErrors;
use helios_endpoints::outbound::OutboundDenied;
use helios_endpoints::secret::SecretError;
use serde_json::{Value, json};

/// Extension on a `409`/`412` OperationOutcome naming the stored revision.
pub const CURRENT_REVISION_URL: &str =
    "https://healthymind.tech/fhir/StructureDefinition/hfs-current-revision";

/// One OperationOutcome issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// FHIR issue type code (`invalid`, `conflict`, …).
    pub code: &'static str,
    /// Human-readable message (`diagnostics`).
    pub message: String,
    /// JSON field path the issue is about (`expression`), if any.
    pub field: Option<String>,
}

impl Issue {
    /// An issue about one field.
    pub fn field(code: &'static str, field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            field: Some(field.into()),
        }
    }
}

/// An API failure, rendered as an OperationOutcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    /// HTTP status.
    pub status: StatusCode,
    /// Issues (at least one).
    pub issues: Vec<Issue>,
    /// Stored revision for conflicts.
    pub current_revision: Option<u64>,
    /// `Retry-After` seconds.
    pub retry_after: Option<u64>,
}

/// Result alias for handlers.
pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    /// One issue without a field.
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            issues: vec![Issue {
                code,
                message: message.into(),
                field: None,
            }],
            current_revision: None,
            retry_after: None,
        }
    }

    /// `400 invalid`.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid", message)
    }

    /// `404 not-found` for an endpoint.
    pub fn endpoint_not_found(name: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not-found",
            format!("endpoint {name} not found"),
        )
    }

    /// `422` with one issue per field.
    pub fn unprocessable(issues: Vec<Issue>) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            issues,
            current_revision: None,
            retry_after: None,
        }
    }

    /// `422` about one field.
    pub fn invalid_field(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::unprocessable(vec![Issue::field("invalid", field, message)])
    }

    /// `409 conflict` with the stored revision.
    pub fn conflict(current: u64) -> Self {
        Self {
            current_revision: Some(current),
            ..Self::new(
                StatusCode::CONFLICT,
                "conflict",
                format!(
                    "revision conflict: the endpoint is now at revision {current}; re-read it and retry"
                ),
            )
        }
    }

    /// `412` for a stale `If-Match`.
    pub fn precondition_failed(current: u64, sent: &str) -> Self {
        Self {
            current_revision: Some(current),
            ..Self::new(
                StatusCode::PRECONDITION_FAILED,
                "conflict",
                format!("If-Match {sent} does not match the current revision \"{current}\""),
            )
        }
    }

    /// `428` when a write must name the revision it replaces.
    pub fn precondition_required() -> Self {
        Self::new(
            StatusCode::PRECONDITION_REQUIRED,
            "required",
            "this write needs If-Match: \"<revision>\" (the ETag of the endpoint you edited)",
        )
    }

    /// `501` for a hook another package provides.
    pub fn not_implemented(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_IMPLEMENTED, "not-supported", message)
    }

    /// `500`.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "exception", message)
    }

    /// The OperationOutcome body.
    pub fn outcome(&self) -> Value {
        let issues: Vec<Value> = self
            .issues
            .iter()
            .map(|i| {
                let mut v = json!({
                    "severity": "error",
                    "code": i.code,
                    "diagnostics": i.message,
                });
                if let Some(f) = &i.field {
                    v["expression"] = json!([f]);
                }
                v
            })
            .collect();
        let mut body = json!({"resourceType": "OperationOutcome", "issue": issues});
        if let Some(rev) = self.current_revision {
            body["extension"] = json!([{"url": CURRENT_REVISION_URL, "valueInteger": rev}]);
        }
        body
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msgs: Vec<&str> = self.issues.iter().map(|i| i.message.as_str()).collect();
        write!(f, "{} {}", self.status.as_u16(), msgs.join("; "))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            [(header::CONTENT_TYPE, "application/fhir+json")],
            Json(self.outcome()),
        )
            .into_response();
        if let Some(rev) = self.current_revision
            && let Ok(v) = HeaderValue::from_str(&super::etag(rev))
        {
            response.headers_mut().insert(header::ETAG, v);
        }
        if let Some(secs) = self.retry_after
            && let Ok(v) = HeaderValue::from_str(&secs.to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, v);
        }
        response
    }
}

impl From<ValidationErrors> for ApiError {
    fn from(e: ValidationErrors) -> Self {
        Self::unprocessable(
            e.0.into_iter()
                .map(|f| Issue::field("invalid", f.field, f.message))
                .collect(),
        )
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound(name) => Self::endpoint_not_found(&name),
            StoreError::Conflict { current } => Self::conflict(current),
            StoreError::Duplicate { field, value, by } => Self {
                issues: vec![Issue::field(
                    "duplicate",
                    field,
                    format!("{field} {value:?} is already used by endpoint {by}"),
                )],
                ..Self::new(StatusCode::CONFLICT, "duplicate", "")
            },
            StoreError::Invalid(v) => v.into(),
            StoreError::Forbidden(m) => Self::new(StatusCode::CONFLICT, "business-rule", m),
            StoreError::Unavailable(m) => Self {
                retry_after: Some(5),
                ..Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "transient",
                    format!("endpoint store unavailable: {m}"),
                )
            },
        }
    }
}

impl From<SecretError> for ApiError {
    fn from(e: SecretError) -> Self {
        match e {
            SecretError::NoKeyRing => Self::new(
                StatusCode::CONFLICT,
                "business-rule",
                "storing a secret needs a key ring: configure HFS_SECRET_KEYS (or HFS_SECRET_KEYS_FILE) and restart",
            ),
            other => Self::internal(format!("secret handling failed: {other}")),
        }
    }
}

/// `422 security` for a URL the outbound policy refuses.
pub fn outbound_refused(field: &str, e: &OutboundDenied) -> Issue {
    Issue::field(
        "security",
        field,
        format!("URL refused by the outbound policy: {e}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_errors_map_to_statuses() {
        let cases: Vec<(StoreError, StatusCode)> = vec![
            (StoreError::NotFound("x".into()), StatusCode::NOT_FOUND),
            (StoreError::Conflict { current: 4 }, StatusCode::CONFLICT),
            (
                StoreError::Duplicate {
                    field: "path",
                    value: "a".into(),
                    by: "b".into(),
                },
                StatusCode::CONFLICT,
            ),
            (
                StoreError::Invalid(ValidationErrors::default()),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (StoreError::Forbidden("no".into()), StatusCode::CONFLICT),
            (
                StoreError::Unavailable("down".into()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ];
        for (e, status) in cases {
            assert_eq!(ApiError::from(e).status, status);
        }
    }

    #[test]
    fn conflict_carries_revision_extension_and_etag() {
        let e = ApiError::conflict(7);
        let body = e.outcome();
        assert_eq!(body["extension"][0]["valueInteger"], 7);
        assert_eq!(body["extension"][0]["url"], CURRENT_REVISION_URL);
        let r = e.into_response();
        assert_eq!(r.headers()[header::ETAG], "\"7\"");
        assert_eq!(r.headers()[header::CONTENT_TYPE], "application/fhir+json");
    }

    #[test]
    fn validation_errors_become_one_issue_per_field() {
        let mut v = ValidationErrors::default();
        v.push("displayName", "too long");
        v.push("auth.issuer", "required");
        let e = ApiError::from(v);
        assert_eq!(e.status, StatusCode::UNPROCESSABLE_ENTITY);
        let body = e.outcome();
        assert_eq!(body["issue"].as_array().unwrap().len(), 2);
        assert_eq!(body["issue"][1]["expression"][0], "auth.issuer");
    }

    #[test]
    fn duplicate_names_the_field() {
        let e = ApiError::from(StoreError::Duplicate {
            field: "storage.tenantId",
            value: "t".into(),
            by: "other".into(),
        });
        let body = e.outcome();
        assert_eq!(body["issue"][0]["code"], "duplicate");
        assert_eq!(body["issue"][0]["expression"][0], "storage.tenantId");
    }

    #[test]
    fn no_key_ring_is_a_conflict() {
        let e = ApiError::from(SecretError::NoKeyRing);
        assert_eq!(e.status, StatusCode::CONFLICT);
        assert!(e.issues[0].message.contains("HFS_SECRET_KEYS"));
        assert_eq!(
            ApiError::from(SecretError::Open).status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn retry_after_and_display() {
        let e = ApiError::from(StoreError::Unavailable("pg".into()));
        assert!(e.to_string().starts_with("503"));
        let r = e.into_response();
        assert_eq!(r.headers()[header::RETRY_AFTER], "5");
    }
}
