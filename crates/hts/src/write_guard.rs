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
//! With `HTS_AUTH_JWKS_URL` and `HTS_AUTH_ISSUER` set, a SMART/OIDC bearer
//! token is accepted for writes too ([`bearer_provider_from_env`]): one whose
//! `system/` or `user/` scopes grant the write on the target type
//! (`system/CodeSystem.cud`, `system/*.cruds`, SMART v1 `system/*.write`, …).
//! `/import` and write batches span every terminology type, so they need
//! create, update and delete on all of CodeSystem, ValueSet and ConceptMap.
//!
//! A `POST /` batch made only of reads (`GET` entries, or `POST` entries to
//! one of those operations) is a read too: HFS's validator sends its
//! `$validate-code` checks that way.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

use axum::http::{HeaderValue, header};
use helios_admin_auth::{AdminGate, Guard, Style, is_read_only, under};
use helios_auth::{Access, AuthConfig, AuthProvider, JwksBearerAuthProvider, JwksCache, Principal, SmartPermissions};
use serde_json::Value;

/// The terminology resource types writes act on.
const TERMINOLOGY_TYPES: &[&str] = &["CodeSystem", "ValueSet", "ConceptMap"];

/// A JWKS bearer validator from `HTS_AUTH_*`, or `None` when
/// `HTS_AUTH_JWKS_URL` is unset. `HTS_AUTH_ISSUER` is required with it;
/// `HTS_AUTH_AUDIENCE`, `HTS_AUTH_ALGORITHMS` and
/// `HTS_AUTH_CLOCK_SKEW_SECONDS` are optional. Keys are fetched on the first
/// token (and refetched on an unknown `kid`), so an IdP that is down at
/// startup does not keep HTS from starting.
pub fn bearer_provider_from_env() -> Option<Arc<dyn AuthProvider>> {
    bearer_provider_from_lookup(|k| std::env::var(k).ok())
}

/// [`bearer_provider_from_env`] reading through `get`.
pub fn bearer_provider_from_lookup(
    get: impl Fn(&str) -> Option<String>,
) -> Option<Arc<dyn AuthProvider>> {
    let var = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let jwks_url = var("HTS_AUTH_JWKS_URL")?;
    let Some(issuer) = var("HTS_AUTH_ISSUER") else {
        tracing::error!(
            "HTS_AUTH_JWKS_URL is set without HTS_AUTH_ISSUER: bearer tokens are NOT accepted for \
             terminology writes until the issuer is pinned"
        );
        return None;
    };
    let mut config = AuthConfig {
        enabled: true,
        jwks_url: Some(jwks_url.clone()),
        expected_issuer: Some(issuer.clone()),
        expected_audience: var("HTS_AUTH_AUDIENCE"),
        ..AuthConfig::default()
    };
    if let Some(algs) = var("HTS_AUTH_ALGORITHMS") {
        config.allowed_algorithms = algs.split(',').map(|a| a.trim().to_string()).collect();
    }
    if let Some(skew) = var("HTS_AUTH_CLOCK_SKEW_SECONDS").and_then(|v| v.parse().ok()) {
        config.clock_skew_secs = skew;
    }
    tracing::info!(
        jwks_url = %jwks_url,
        issuer = %issuer,
        audience = ?config.expected_audience,
        "SMART/OIDC bearer tokens accepted for terminology writes"
    );
    let cache = Arc::new(
        JwksCache::new(&jwks_url, config.jwks_min_refresh_interval)
            .with_max_ttl(config.jwks_max_age_secs),
    );
    Some(Arc::new(JwksBearerAuthProvider::new(cache, &config)))
}

/// Whether an `Authorization` value is a bearer JWT (`eyJ…`, three parts).
fn jwt_bearer(request: &Request) -> Option<String> {
    let value = request.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?;
    (token.starts_with("eyJ") && token.split('.').count() == 3).then(|| value.to_string())
}

/// Whether `principal`'s scopes grant this write. Patient-context and
/// granular scopes never do: terminology is not patient data.
pub fn bearer_may_write(principal: &Principal, method: &Method, path: &str) -> bool {
    let full = |ty: &str, perm: SmartPermissions| principal.scopes.access(ty, perm) == Access::Full;
    let everything = || {
        TERMINOLOGY_TYPES.iter().all(|ty| {
            full(ty, SmartPermissions::CREATE)
                && full(ty, SmartPermissions::UPDATE)
                && full(ty, SmartPermissions::DELETE)
        })
    };
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some(first) = segments.first() else {
        return everything(); // a write batch
    };
    if !TERMINOLOGY_TYPES.contains(first) {
        return everything(); // `/import` and anything else administrative
    }
    let operation = segments.last().is_some_and(|s| s.starts_with('$'));
    match *method {
        Method::POST if !operation => full(first, SmartPermissions::CREATE),
        Method::PUT | Method::PATCH if !operation => full(first, SmartPermissions::UPDATE),
        Method::DELETE => full(first, SmartPermissions::DELETE),
        _ => {
            full(first, SmartPermissions::CREATE)
                && full(first, SmartPermissions::UPDATE)
                && full(first, SmartPermissions::DELETE)
        }
    }
}

fn refusal(status: StatusCode, code: &str, error: &str, message: &str) -> Response {
    let body = serde_json::json!({
        "resourceType": "OperationOutcome",
        "issue": [{"severity": "error", "code": code, "diagnostics": message}],
    });
    let mut response = (
        status,
        [(header::CONTENT_TYPE, "application/fhir+json")],
        body.to_string(),
    )
        .into_response();
    let challenge = format!(
        "Bearer realm=\"hts\", error=\"{error}\", error_description=\"{}\"",
        message.replace(['"', '\\'], "'")
    );
    if let Ok(v) = HeaderValue::from_str(&challenge) {
        response.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    response
}

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
    /// SMART/OIDC bearer validation (`HTS_AUTH_*`), accepted besides the
    /// administrator credential.
    pub bearer: Option<Arc<dyn AuthProvider>>,
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
    if let (Some(provider), Some(authorization)) = (&state.bearer, jwt_bearer(&request)) {
        return match provider.authenticate(&authorization).await {
            Ok(principal) => {
                if bearer_may_write(&principal, request.method(), request.uri().path()) {
                    next.run(request).await
                } else {
                    tracing::warn!(sub = %principal.subject, path = %request.uri().path(),
                        "terminology write refused: insufficient scope");
                    refusal(
                        StatusCode::FORBIDDEN,
                        "forbidden",
                        "insufficient_scope",
                        "the token's scopes do not grant this terminology write",
                    )
                }
            }
            Err(err) => refusal(
                StatusCode::UNAUTHORIZED,
                "login",
                "invalid_token",
                &err.to_string(),
            ),
        };
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

    fn principal(scopes: &str) -> Principal {
        Principal {
            subject: "svc".into(),
            issuer: "https://idp".into(),
            tenant_id: None,
            scopes: helios_auth::ScopeSet::parse(scopes),
            jti: None,
            expires_at: chrono::Utc::now(),
            custom_claims: serde_json::Map::new(),
        }
    }

    #[test]
    fn bearer_scopes_gate_terminology_writes() {
        let cs = principal("system/CodeSystem.cud");
        assert!(bearer_may_write(&cs, &Method::POST, "/CodeSystem"));
        assert!(bearer_may_write(&cs, &Method::PUT, "/CodeSystem/x"));
        assert!(bearer_may_write(&cs, &Method::DELETE, "/CodeSystem/x"));
        assert!(!bearer_may_write(&cs, &Method::POST, "/ValueSet"));
        assert!(!bearer_may_write(&cs, &Method::POST, "/import"));
        assert!(!bearer_may_write(&cs, &Method::POST, "/"));

        let all = principal("system/*.write");
        assert!(bearer_may_write(&all, &Method::POST, "/import"));
        assert!(bearer_may_write(&all, &Method::POST, "/"));
        assert!(bearer_may_write(&all, &Method::POST, "/ValueSet/x/$new-op"));

        // Patient context and read-only scopes never write terminology.
        assert!(!bearer_may_write(&principal("patient/*.cruds"), &Method::POST, "/CodeSystem"));
        assert!(!bearer_may_write(&principal("system/*.rs"), &Method::POST, "/CodeSystem"));
    }

    #[test]
    fn bearer_provider_needs_an_issuer() {
        assert!(bearer_provider_from_lookup(|_| None).is_none());
        assert!(bearer_provider_from_lookup(|k| (k == "HTS_AUTH_JWKS_URL")
            .then(|| "https://idp/jwks".to_string()))
        .is_none());
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
