//! The console administration REST API (design §8, package P3):
//!
//! ```text
//! GET|PUT /admin/console/settings                 redacted; PUT needs If-Match: "rev"
//! POST    /admin/console/settings/$test-oidc      discovery probe of the OIDC client
//! GET|POST /admin/console/users ; GET|PUT|DELETE /admin/console/users/{id}
//! POST    /admin/console/users/{id}/$reset-password
//! GET     /admin/console/bindings ; PUT|DELETE /admin/console/bindings/{subject}/{endpoint}
//! GET     /admin/console/sessions ; DELETE /admin/console/sessions/{key}
//! ```
//!
//! Mounted by the server under its admin guard ([`crate::guard`]), which
//! admits a console admin session (same-site + `X-CSRF-Token` on writes) or
//! the break-glass credential and stamps [`AdminUser`]; this router refuses
//! anything that arrives without that stamp (e.g. a request that skipped the
//! password guard because it carried a JWT). Errors are OperationOutcomes:
//! `422` with one issue per field (`expression` = field path), `409`/`412`
//! revision conflicts, `428` without `If-Match`.

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use helios_endpoints::SecretInput;
use helios_endpoints::console::{ConsoleAuthSettings, ConsoleRole, ConsoleUser};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ConsoleAuth, ConsoleError, NewUser, OidcSwitchCheck, UserPatch, settings};
use crate::AdminUser;

/// The router (state: the runtime).
pub fn router(console: Arc<ConsoleAuth>) -> Router {
    Router::new()
        .route(
            "/admin/console/settings",
            get(get_settings).put(put_settings),
        )
        .route("/admin/console/settings/$test-oidc", post(test_oidc))
        .route("/admin/console/users", get(list_users).post(create_user))
        .route(
            "/admin/console/users/{id}",
            get(get_user).put(update_user).delete(delete_user),
        )
        .route(
            "/admin/console/users/{id}/$reset-password",
            post(reset_password),
        )
        .route("/admin/console/bindings", get(list_bindings))
        .route(
            "/admin/console/bindings/{subject}/{endpoint}",
            put(put_binding).delete(delete_binding),
        )
        .route("/admin/console/sessions", get(list_sessions))
        .route(
            "/admin/console/sessions/{key}",
            axum::routing::delete(revoke_session),
        )
        .layer(axum::middleware::from_fn(require_admin))
        .with_state(console)
}

async fn require_admin(request: Request, next: Next) -> Response {
    if request.extensions().get::<AdminUser>().is_none() {
        return outcome(
            StatusCode::UNAUTHORIZED,
            "login",
            "administrator sign-in required",
            None,
        );
    }
    next.run(request).await
}

fn outcome(status: StatusCode, code: &str, message: &str, expression: Option<&str>) -> Response {
    let mut issue = json!({"severity": "error", "code": code, "diagnostics": message});
    if let Some(e) = expression {
        issue["expression"] = json!([e]);
    }
    json_response(
        status,
        json!({"resourceType": "OperationOutcome", "issue": [issue]}),
    )
}

fn json_response(status: StatusCode, body: Value) -> Response {
    let mut r = (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// Maps a [`ConsoleError`] to its OperationOutcome response.
pub fn error_response(e: ConsoleError) -> Response {
    match e {
        ConsoleError::Invalid(errors) => {
            let issues: Vec<Value> = errors
                .iter()
                .map(|e| {
                    json!({"severity": "error", "code": "invalid", "diagnostics": e.message,
                           "expression": [e.field]})
                })
                .collect();
            json_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({"resourceType": "OperationOutcome", "issue": issues}),
            )
        }
        ConsoleError::Policy(p) => outcome(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            &p.to_string(),
            Some("password"),
        ),
        ConsoleError::Conflict(current) => {
            let mut r = outcome(
                StatusCode::CONFLICT,
                "conflict",
                &format!("changed meanwhile; current revision is {current}"),
                None,
            );
            if let Ok(v) = HeaderValue::from_str(&format!("\"{current}\"")) {
                r.headers_mut().insert(header::ETAG, v);
            }
            r
        }
        ConsoleError::NotFound => outcome(StatusCode::NOT_FOUND, "not-found", "not found", None),
        ConsoleError::Duplicate(n) => outcome(
            StatusCode::CONFLICT,
            "duplicate",
            &format!("user name {n:?} is taken"),
            Some("username"),
        ),
        ConsoleError::Forbidden(m) => outcome(StatusCode::CONFLICT, "business-rule", &m, None),
        ConsoleError::Unavailable(m) => {
            outcome(StatusCode::SERVICE_UNAVAILABLE, "transient", &m, None)
        }
    }
}

fn if_match(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.trim()
                .trim_start_matches("W/")
                .trim_matches('"')
                .to_string()
        })
        .and_then(|v| v.parse().ok())
}

fn with_etag(mut r: Response, revision: u64) -> Response {
    if let Ok(v) = HeaderValue::from_str(&format!("\"{revision}\"")) {
        r.headers_mut().insert(header::ETAG, v);
    }
    r
}

fn precondition_required() -> Response {
    outcome(
        StatusCode::PRECONDITION_REQUIRED,
        "required",
        "send If-Match with the revision you read",
        None,
    )
}

/// Settings as JSON with the sealed secret replaced by `{"set": true}`.
pub fn redacted_settings(s: &ConsoleAuthSettings) -> Value {
    let mut v = serde_json::to_value(s).unwrap_or(Value::Null);
    helios_endpoints::secret::redact_sealed(&mut v);
    v
}

async fn get_settings(State(c): State<Arc<ConsoleAuth>>) -> Response {
    c.reload().await;
    let (stored, revision) = c.stored_settings();
    let effective = c.settings();
    let mut eff = redacted_settings(&effective);
    if c.env().client_secret.is_some()
        && let Some(o) = eff.get_mut("oidc").and_then(Value::as_object_mut)
    {
        o.insert("clientSecret".into(), json!({"set": true, "pinned": true}));
    }
    let body = json!({
        "revision": revision,
        "stored": stored.as_ref().map(redacted_settings),
        "effective": eff,
        "pinned": c.env().pinned.iter().collect::<Vec<_>>(),
        "breakGlassAvailable": c.break_glass_available(),
        "oidcFingerprint": c.oidc_fingerprint(),
        "persistent": c.options().persistent,
        "secretKeyRing": c.options().key_ring.is_some(),
    });
    with_etag(json_response(StatusCode::OK, body), revision)
}

async fn put_settings(
    State(c): State<Arc<ConsoleAuth>>,
    Extension(actor): Extension<AdminUser>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Response {
    let Some(expected) = if_match(&headers) else {
        return precondition_required();
    };
    let secret = body
        .as_object_mut()
        .and_then(|o| o.remove("secrets"))
        .and_then(|s| s.get(settings::field::OIDC_CLIENT_SECRET).cloned())
        .map(serde_json::from_value::<SecretInput>)
        .transpose();
    let secret = match secret {
        Ok(s) => s.unwrap_or_default(),
        Err(e) => {
            return outcome(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid",
                &format!("secrets: {e}"),
                Some(settings::field::OIDC_CLIENT_SECRET),
            );
        }
    };
    // Secrets never travel in the settings document itself.
    if let Some(o) = body.get_mut("oidc").and_then(Value::as_object_mut) {
        o.remove("clientSecret");
    }
    let proposed: ConsoleAuthSettings = match serde_json::from_value(body) {
        Ok(s) => s,
        Err(e) => {
            return outcome(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid",
                &e.to_string(),
                None,
            );
        }
    };
    match c
        .save_settings(&actor.0, proposed, secret, expected, OidcSwitchCheck::Probe)
        .await
    {
        Ok(rev) => with_etag(get_settings(State(c)).await, rev),
        Err(ConsoleError::Conflict(current)) => {
            let mut r = error_response(ConsoleError::Conflict(current));
            *r.status_mut() = StatusCode::PRECONDITION_FAILED;
            r
        }
        Err(e) => error_response(e),
    }
}

async fn test_oidc(State(c): State<Arc<ConsoleAuth>>) -> Response {
    let Some(o) = c.settings().oidc else {
        return error_response(ConsoleError::Invalid(vec![super::SettingsError {
            field: settings::field::OIDC_ISSUER,
            message: "no OIDC client is configured".into(),
        }]));
    };
    match c.probe_oidc(&o).await {
        Ok(issuer) => json_response(
            StatusCode::OK,
            json!({"ok": true, "issuer": issuer, "fingerprint": c.oidc_fingerprint()}),
        ),
        Err(e) => json_response(StatusCode::OK, json!({"ok": false, "error": e})),
    }
}

async fn list_users(State(c): State<Arc<ConsoleAuth>>) -> Response {
    match c.list_users().await {
        Ok(users) => json_response(StatusCode::OK, json!({"users": users})),
        Err(e) => error_response(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateBody {
    username: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    role: Option<ConsoleRole>,
    #[serde(default)]
    password: Option<String>,
}

async fn create_user(
    State(c): State<Arc<ConsoleAuth>>,
    Extension(actor): Extension<AdminUser>,
    Json(b): Json<CreateBody>,
) -> Response {
    match c
        .create_user(
            &actor.0,
            NewUser {
                username: b.username,
                display_name: b.display_name,
                email: b.email,
                role: b.role,
                password: b.password,
            },
        )
        .await
    {
        Ok((user, generated)) => {
            let rev = user.revision;
            let mut body = json!({"user": user});
            if let Some(p) = generated {
                body["oneTimePassword"] = json!(p);
            }
            with_etag(json_response(StatusCode::CREATED, body), rev)
        }
        Err(e) => error_response(e),
    }
}

async fn find_user(c: &ConsoleAuth, id: &str) -> Result<ConsoleUser, ConsoleError> {
    c.store()
        .user_by_id(id)
        .await?
        .ok_or(ConsoleError::NotFound)
}

async fn get_user(State(c): State<Arc<ConsoleAuth>>, Path(id): Path<String>) -> Response {
    match find_user(&c, &id).await {
        Ok(u) => {
            let rev = u.revision;
            with_etag(json_response(StatusCode::OK, json!(u)), rev)
        }
        Err(e) => error_response(e),
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UpdateBody {
    #[serde(default, with = "double_option")]
    display_name: Option<Option<String>>,
    #[serde(default, with = "double_option")]
    email: Option<Option<String>>,
    #[serde(default)]
    role: Option<ConsoleRole>,
    #[serde(default)]
    disabled: Option<bool>,
}

mod double_option {
    use serde::{Deserialize, Deserializer};
    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Option<String>>, D::Error> {
        Option::<String>::deserialize(d).map(Some)
    }
}

async fn update_user(
    State(c): State<Arc<ConsoleAuth>>,
    Extension(actor): Extension<AdminUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(b): Json<UpdateBody>,
) -> Response {
    let Some(expected) = if_match(&headers) else {
        return precondition_required();
    };
    match c
        .update_user(
            &actor.0,
            &id,
            UserPatch {
                display_name: b.display_name,
                email: b.email,
                role: b.role,
                disabled: b.disabled,
            },
            expected,
        )
        .await
    {
        Ok(u) => {
            let rev = u.revision;
            with_etag(json_response(StatusCode::OK, json!(u)), rev)
        }
        Err(ConsoleError::Conflict(current)) => {
            let mut r = error_response(ConsoleError::Conflict(current));
            *r.status_mut() = StatusCode::PRECONDITION_FAILED;
            r
        }
        Err(e) => error_response(e),
    }
}

async fn delete_user(
    State(c): State<Arc<ConsoleAuth>>,
    Extension(actor): Extension<AdminUser>,
    Path(id): Path<String>,
) -> Response {
    match c.delete_user(&actor.0, &id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => error_response(e),
    }
}

async fn reset_password(
    State(c): State<Arc<ConsoleAuth>>,
    Extension(actor): Extension<AdminUser>,
    Path(id): Path<String>,
) -> Response {
    match c.reset_password(&actor.0, &id).await {
        Ok(p) => json_response(StatusCode::OK, json!({"oneTimePassword": p})),
        Err(e) => error_response(e),
    }
}

async fn list_bindings(State(c): State<Arc<ConsoleAuth>>) -> Response {
    match c.store().list_role_bindings().await {
        Ok(b) => json_response(StatusCode::OK, json!({"bindings": b})),
        Err(e) => error_response(e.into()),
    }
}

#[derive(Deserialize)]
struct BindingBody {
    role: ConsoleRole,
}

async fn put_binding(
    State(c): State<Arc<ConsoleAuth>>,
    Extension(actor): Extension<AdminUser>,
    Path((subject, endpoint)): Path<(String, String)>,
    Json(b): Json<BindingBody>,
) -> Response {
    match c
        .store()
        .set_role_binding(&subject, &endpoint, b.role, &actor.0)
        .await
    {
        Ok(()) => {
            c.invalidate_subject(&subject);
            tracing::info!(actor = %actor.0, subject, endpoint, role = b.role.as_str(), "console role binding set");
            json_response(
                StatusCode::OK,
                json!({"subject": subject, "endpoint": endpoint, "role": b.role}),
            )
        }
        Err(e) => error_response(e.into()),
    }
}

async fn delete_binding(
    State(c): State<Arc<ConsoleAuth>>,
    Extension(actor): Extension<AdminUser>,
    Path((subject, endpoint)): Path<(String, String)>,
) -> Response {
    match c.store().delete_role_binding(&subject, &endpoint).await {
        Ok(()) => {
            c.invalidate_subject(&subject);
            tracing::info!(actor = %actor.0, subject, endpoint, "console role binding removed");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => error_response(e.into()),
    }
}

async fn list_sessions(State(c): State<Arc<ConsoleAuth>>) -> Response {
    match c.list_sessions().await {
        Ok(list) => {
            let rows: Vec<Value> = list
                .into_iter()
                .map(|(key, s)| {
                    json!({
                        "key": key, "subject": s.subject, "displayName": s.display_name,
                        "role": s.role, "via": s.via, "client": s.client,
                        "createdUnix": s.created_unix, "lastSeenUnix": s.last_seen_unix,
                    })
                })
                .collect();
            json_response(StatusCode::OK, json!({"sessions": rows}))
        }
        Err(e) => error_response(e),
    }
}

async fn revoke_session(
    State(c): State<Arc<ConsoleAuth>>,
    Extension(actor): Extension<AdminUser>,
    Path(key): Path<String>,
) -> Response {
    match c.revoke_session(&actor.0, &key).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => error_response(e),
    }
}
