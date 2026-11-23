//! The admin API, `/admin/idp/**` (design §12), over [`IdpConsole`].
//!
//! * Authorization is the host's: [`router`] takes an [`Authorize`] hook
//!   (the `hfs` wiring passes the endpoint admin API's authorizer: console
//!   session with role admin + CSRF, administrator credential, or an admin
//!   token). Without one every request is refused.
//! * Realms are addressed by **name** (`/admin/idp/realms/clinic`).
//! * Optimistic concurrency: `GET` returns `ETag: "{revision}"`; `PUT`
//!   honours `If-Match`.
//! * Secrets are write-only: client secret hashes come back as
//!   `{"set": true}`; a generated secret is returned once, on creation or
//!   `$reset-secret`.
//! * Errors: `{"error": "...", "field": "lifetimes.accessToken"|null, "status": 400}`.
//!
//! [`ROUTES`] lists every route; a test checks it against the OpenAPI
//! document served at `/admin/idp/openapi.json` and against the router.

// Handlers return `Result<Response, Response>` (both arms are responses).
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use helios_endpoints::Actor;
use helios_idp::admin::LaunchRequest;
use helios_idp::client::{ClientAuth, ClientSpec};
use helios_idp::realm::RealmSpec;
use helios_idp::store::GrantFilter;
use helios_idp::user::RealmUser;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::binding::BindMode;
use crate::service::{self, DIAGNOSTIC_PREFIX, DiagnoseOptions, IdpConsole, OpError, error_json};

/// Result of an authorization decision.
pub type AuthResult = Result<Actor, (StatusCode, String)>;

/// Decides whether a request may use the API, and who it acts as (async:
/// the host's authorizer may consult session stores).
pub type Authorize = Arc<
    dyn for<'a> Fn(&'a axum::http::request::Parts) -> std::pin::Pin<Box<dyn std::future::Future<Output = AuthResult> + Send + 'a>>
        + Send
        + Sync,
>;

/// An [`Authorize`] from a synchronous decision.
pub fn authorize_sync(f: impl Fn(&axum::http::request::Parts) -> AuthResult + Send + Sync + 'static) -> Authorize {
    Arc::new(move |p| {
        let r = f(p);
        Box::pin(async move { r })
    })
}

/// The OpenAPI 3.1 description of this API.
pub const OPENAPI: &str = include_str!("../openapi/admin-idp.json");

/// Every route: `(method, path)`.
pub const ROUTES: &[(&str, &str)] = &[
    ("GET", "/admin/idp/openapi.json"),
    ("GET", "/admin/idp/realms"),
    ("POST", "/admin/idp/realms"),
    ("GET", "/admin/idp/realms/{realm}"),
    ("PUT", "/admin/idp/realms/{realm}"),
    ("DELETE", "/admin/idp/realms/{realm}"),
    ("POST", "/admin/idp/realms/{realm}/$diagnose"),
    ("GET", "/admin/idp/realms/{realm}/diagnostics"),
    ("GET", "/admin/idp/realms/{realm}/keys"),
    ("POST", "/admin/idp/realms/{realm}/$rotate-keys"),
    ("POST", "/admin/idp/realms/{realm}/$emergency-rotate"),
    ("GET", "/admin/idp/realms/{realm}/clients"),
    ("POST", "/admin/idp/realms/{realm}/clients"),
    ("GET", "/admin/idp/realms/{realm}/clients/{client}"),
    ("PUT", "/admin/idp/realms/{realm}/clients/{client}"),
    ("DELETE", "/admin/idp/realms/{realm}/clients/{client}"),
    ("POST", "/admin/idp/realms/{realm}/clients/{client}/$reset-secret"),
    ("GET", "/admin/idp/realms/{realm}/users"),
    ("POST", "/admin/idp/realms/{realm}/users"),
    ("GET", "/admin/idp/realms/{realm}/users/{user}"),
    ("PUT", "/admin/idp/realms/{realm}/users/{user}"),
    ("DELETE", "/admin/idp/realms/{realm}/users/{user}"),
    ("POST", "/admin/idp/realms/{realm}/users/{user}/$set-password"),
    ("GET", "/admin/idp/realms/{realm}/sessions"),
    ("DELETE", "/admin/idp/realms/{realm}/sessions/{session}"),
    ("GET", "/admin/idp/realms/{realm}/grants"),
    ("DELETE", "/admin/idp/realms/{realm}/grants/{grant}"),
    ("POST", "/admin/idp/realms/{realm}/launches"),
    ("GET", "/admin/idp/realms/{realm}/endpoints/{endpoint}"),
    ("PUT", "/admin/idp/realms/{realm}/endpoints/{endpoint}"),
    ("DELETE", "/admin/idp/realms/{realm}/endpoints/{endpoint}"),
];

#[derive(Clone)]
struct ApiState {
    auth: Option<Authorize>,
}

/// The `/admin/idp` router. Mount it only with `HFS_IDP=on`; requests are
/// answered `503` until [`service::install`] has run.
pub fn router(auth: Option<Authorize>) -> Router {
    let st = ApiState { auth };
    let r = "/admin/idp/realms/{realm}";
    Router::new()
        .route("/admin/idp/realms", get(list_realms).post(create_realm))
        .route(r, get(get_realm).put(put_realm).delete(delete_realm))
        .route(&format!("{r}/$diagnose"), post(diagnose))
        .route(&format!("{r}/diagnostics"), get(diagnostics))
        .route(&format!("{r}/keys"), get(list_keys))
        .route(&format!("{r}/$rotate-keys"), post(rotate_keys))
        .route(&format!("{r}/$emergency-rotate"), post(emergency_rotate))
        .route(&format!("{r}/clients"), get(list_clients).post(create_client))
        .route(&format!("{r}/clients/{{client}}"), get(get_client).put(put_client).delete(delete_client))
        .route(&format!("{r}/clients/{{client}}/$reset-secret"), post(reset_secret))
        .route(&format!("{r}/users"), get(list_users).post(create_user))
        .route(&format!("{r}/users/{{user}}"), get(get_user).put(put_user).delete(delete_user))
        .route(&format!("{r}/users/{{user}}/$set-password"), post(set_password))
        .route(&format!("{r}/sessions"), get(list_sessions))
        .route(&format!("{r}/sessions/{{session}}"), axum::routing::delete(revoke_session))
        .route(&format!("{r}/grants"), get(list_grants))
        .route(&format!("{r}/grants/{{grant}}"), axum::routing::delete(revoke_grant))
        .route(&format!("{r}/launches"), post(create_launch))
        .route(&format!("{r}/endpoints/{{endpoint}}"), get(bind_diff).put(bind).delete(unbind))
        .route_layer(middleware::from_fn_with_state(st.clone(), authorize))
        .route("/admin/idp/openapi.json", get(openapi))
        .with_state(st)
}

async fn authorize(State(st): State<ApiState>, req: Request, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();
    let Some(auth) = st.auth.as_ref() else {
        return err(&OpError::other(401, "the IdP admin API has no authorizer configured"));
    };
    match auth(&parts).await {
        Ok(actor) => {
            parts.extensions.insert(actor);
            next.run(Request::from_parts(parts, body)).await
        }
        Err((status, msg)) => err(&OpError::other(status.as_u16(), msg)),
    }
}

async fn openapi() -> Response {
    ([(header::CONTENT_TYPE, "application/json")], OPENAPI).into_response()
}

fn err(e: &OpError) -> Response {
    let status = StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(error_json(e))).into_response()
}

fn console() -> Result<Arc<IdpConsole>, Response> {
    service::global().ok_or_else(|| err(&OpError::other(503, "the built-in authorization server is not enabled (HFS_IDP=on)")))
}

type ApiResult = Result<Response, Response>;

fn ok(v: impl serde::Serialize) -> ApiResult {
    Ok(Json(v).into_response())
}

fn op<T>(r: Result<T, OpError>) -> Result<T, Response> {
    r.map_err(|e| err(&e))
}

fn idp<T>(r: Result<T, helios_idp::IdpError>) -> Result<T, Response> {
    r.map_err(|e| err(&OpError::from(e)))
}

fn if_match(h: &HeaderMap) -> Option<i64> {
    h.get(header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().trim_start_matches("W/").trim_matches('"').parse().ok())
}

fn etag(rev: i64) -> (header::HeaderName, String) {
    (header::ETAG, format!("\"{rev}\""))
}

/// A client as the API shows it: secret hash replaced by `{"set": true}`.
pub fn redact_client(spec: &ClientSpec) -> Value {
    let mut v = serde_json::to_value(spec).unwrap_or(Value::Null);
    if let ClientAuth::ClientSecret { secret_hash } = &spec.auth {
        v["auth"]["secret_hash"] = json!({"set": !secret_hash.is_empty()});
    }
    v
}

/// Accepts a client body whose secret is absent (generated) or `{"set": …}`.
fn client_from_body(mut v: Value) -> Result<ClientSpec, Response> {
    if v["auth"]["method"] == "client_secret" {
        let keep = match &v["auth"]["secret_hash"] {
            Value::String(s) if s.starts_with("$argon2id$") => s.clone(),
            _ => String::new(),
        };
        v["auth"]["secret_hash"] = json!(keep);
    }
    serde_json::from_value(v).map_err(|e| err(&OpError::invalid("body", e.to_string())))
}

// ------------------------------------------------------------ realms

async fn list_realms() -> ApiResult {
    let c = console()?;
    ok(json!({"realms": op(c.summaries().await)?}))
}

async fn create_realm(Json(spec): Json<RealmSpec>) -> ApiResult {
    let c = console()?;
    let rec = idp(c.admin.create_realm(spec).await)?;
    Ok((StatusCode::CREATED, [etag(rec.revision)], Json(realm_json(&c, &rec))).into_response())
}

fn realm_json(c: &IdpConsole, rec: &helios_idp::store::RealmRecord) -> Value {
    json!({
        "id": rec.id,
        "revision": rec.revision,
        "issuer": c.issuer(&rec.spec),
        "spec": rec.spec,
    })
}

async fn get_realm(Path(realm): Path<String>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    Ok(([etag(rec.revision)], Json(realm_json(&c, &rec))).into_response())
}

async fn put_realm(Path(realm): Path<String>, h: HeaderMap, Json(spec): Json<RealmSpec>) -> ApiResult {
    let c = console()?;
    let rec = op(c.update(&realm, if_match(&h), move |s| {
        *s = spec;
        Ok(())
    })
    .await)?;
    Ok(([etag(rec.revision)], Json(realm_json(&c, &rec))).into_response())
}

async fn delete_realm(Path(realm): Path<String>, Extension(actor): Extension<Actor>) -> ApiResult {
    let c = console()?;
    op(c.delete_realm(&realm, &actor).await)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize, Default)]
struct DiagnoseQuery {
    /// `only` = configuration checks only.
    preflight: Option<String>,
}

async fn diagnose(Path(realm): Path<String>, Query(q): Query<DiagnoseQuery>, body: Option<Json<DiagnoseOptions>>) -> ApiResult {
    let c = console()?;
    let opts = body.map(|b| b.0).unwrap_or_default();
    let report = if q.preflight.as_deref() == Some("only") {
        op(c.preflight(&realm, opts.app.as_ref()).await)?
    } else {
        op(c.diagnose(&realm, opts).await)?
    };
    ok(report)
}

async fn diagnostics(Path(realm): Path<String>) -> ApiResult {
    let c = console()?;
    op(c.realm(&realm).await)?;
    ok(json!({"runs": c.history(&realm)}))
}

// ------------------------------------------------------------ keys

async fn list_keys(Path(realm): Path<String>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    ok(json!({"keys": idp(c.admin.list_keys(&rec.id).await)?}))
}

async fn rotate_keys(Path(realm): Path<String>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    let created = idp(c.admin.rotate_keys(&rec.id).await)?;
    ok(json!({"created": created, "keys": idp(c.admin.list_keys(&rec.id).await)?}))
}

async fn emergency_rotate(Path(realm): Path<String>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    let created = idp(c.admin.emergency_rotate(&rec.id).await)?;
    ok(json!({"created": created, "keys": idp(c.admin.list_keys(&rec.id).await)?}))
}

// ------------------------------------------------------------ clients

async fn list_clients(Path(realm): Path<String>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    let list: Vec<Value> = idp(c.admin.list_clients(&rec.id).await)?
        .iter()
        .filter(|r| !r.spec.client_id.starts_with(DIAGNOSTIC_PREFIX))
        .map(|r| json!({"revision": r.revision, "registration": r.registration.as_str(), "spec": redact_client(&r.spec)}))
        .collect();
    ok(json!({"clients": list}))
}

async fn create_client(Path(realm): Path<String>, Json(body): Json<Value>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    let spec = client_from_body(body)?;
    let created = idp(c.admin.create_client(&rec.id, spec).await)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "revision": created.record.revision,
            "spec": redact_client(&created.record.spec),
            "clientSecret": created.client_secret,
        })),
    )
        .into_response())
}

async fn find_client(c: &IdpConsole, realm_id: &str, client: &str) -> Result<helios_idp::store::ClientRecord, Response> {
    idp(c.admin.list_clients(realm_id).await)?
        .into_iter()
        .find(|r| r.spec.client_id == client)
        .ok_or_else(|| err(&OpError::other(404, format!("no client {client}"))))
}

async fn get_client(Path((realm, client)): Path<(String, String)>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    let cl = find_client(&c, &rec.id, &client).await?;
    Ok(([etag(cl.revision)], Json(json!({"revision": cl.revision, "spec": redact_client(&cl.spec)}))).into_response())
}

async fn put_client(Path((realm, client)): Path<(String, String)>, h: HeaderMap, Json(body): Json<Value>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    let cur = find_client(&c, &rec.id, &client).await?;
    let mut spec = client_from_body(body)?;
    if spec.client_id != client {
        return Err(err(&OpError::invalid("clientId", "the client id cannot be changed")));
    }
    if let (ClientAuth::ClientSecret { secret_hash }, ClientAuth::ClientSecret { secret_hash: old }) = (&mut spec.auth, &cur.spec.auth)
        && secret_hash.is_empty()
    {
        old.clone_into(secret_hash);
    }
    let r = idp(c.admin.update_client(&rec.id, spec, if_match(&h).unwrap_or(cur.revision)).await)?;
    Ok(([etag(r.revision)], Json(json!({"revision": r.revision, "spec": redact_client(&r.spec)}))).into_response())
}

async fn delete_client(Path((realm, client)): Path<(String, String)>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    if !idp(c.admin.delete_client(&rec.id, &client).await)? {
        return Err(err(&OpError::other(404, format!("no client {client}"))));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn reset_secret(Path((realm, client)): Path<(String, String)>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    let created = idp(c.admin.reset_client_secret(&rec.id, &client).await)?;
    ok(json!({"spec": redact_client(&created.record.spec), "clientSecret": created.client_secret}))
}

// ------------------------------------------------------------ users

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NewUser {
    #[serde(flatten)]
    user: RealmUser,
    password: Option<String>,
    #[serde(default)]
    must_change_password: bool,
}

async fn list_users(Path(realm): Path<String>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    let users: Vec<RealmUser> = idp(c.admin.list_users(&rec.id).await)?
        .into_iter()
        .filter(|u| !u.username.starts_with(DIAGNOSTIC_PREFIX))
        .collect();
    ok(json!({"users": users}))
}

async fn create_user(Path(realm): Path<String>, Json(b): Json<NewUser>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    let u = idp(c.admin.create_user(&rec.id, b.user, b.password.as_deref(), b.must_change_password).await)?;
    Ok((StatusCode::CREATED, Json(u)).into_response())
}

async fn get_user(Path((realm, user)): Path<(String, String)>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    let u = idp(c.admin.runtime().store().get_user(&rec.id, &user).await)?
        .ok_or_else(|| err(&OpError::other(404, format!("no user {user}"))))?;
    ok(u)
}

async fn put_user(Path((realm, user)): Path<(String, String)>, Json(mut u): Json<RealmUser>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    u.id = user;
    ok(idp(c.admin.update_user(&rec.id, u).await)?)
}

async fn delete_user(Path((realm, user)): Path<(String, String)>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    if !idp(c.admin.delete_user(&rec.id, &user).await)? {
        return Err(err(&OpError::other(404, format!("no user {user}"))));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetPassword {
    password: String,
    #[serde(default)]
    must_change: bool,
}

async fn set_password(Path((realm, user)): Path<(String, String)>, Json(b): Json<SetPassword>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    idp(c.admin.set_password(&rec.id, &user, &b.password, b.must_change).await)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ------------------------------------------------------------ sessions / grants / launches

#[derive(Deserialize, Default)]
struct UserQuery {
    user: Option<String>,
    client: Option<String>,
    #[serde(default)]
    include_revoked: bool,
}

async fn list_sessions(Path(realm): Path<String>, Query(q): Query<UserQuery>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    ok(json!({"sessions": idp(c.admin.list_sessions(&rec.id, q.user.as_deref()).await)?}))
}

async fn revoke_session(Path((realm, session)): Path<(String, String)>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    idp(c.admin.revoke_session(&rec.id, &session).await)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn list_grants(Path(realm): Path<String>, Query(q): Query<UserQuery>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    let f = GrantFilter { subject: q.user, client_id: q.client, include_revoked: q.include_revoked, limit: Some(500) };
    ok(json!({"grants": idp(c.admin.list_grants(&rec.id, &f).await)?}))
}

async fn revoke_grant(Path((realm, grant)): Path<(String, String)>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    idp(c.admin.revoke_grant(&rec.id, &grant).await)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn create_launch(Path(realm): Path<String>, Extension(actor): Extension<Actor>, Json(mut req): Json<LaunchRequest>) -> ApiResult {
    let c = console()?;
    let rec = op(c.realm(&realm).await)?;
    if req.launched_by.is_none() {
        req.launched_by = Some(actor.subject.clone());
    }
    Ok((StatusCode::CREATED, Json(idp(c.admin.create_launch(&rec.id, req).await)?)).into_response())
}

// ------------------------------------------------------------ endpoint binding

#[derive(Deserialize, Default)]
struct BindBody {
    #[serde(default)]
    mode: Option<String>,
}

async fn bind_diff(Path((realm, endpoint)): Path<(String, String)>, Query(q): Query<BindBody>) -> ApiResult {
    let c = console()?;
    let mode = BindMode::parse(q.mode.as_deref().unwrap_or(""));
    ok(op(c.bind_diff(&realm, &endpoint, mode).await)?)
}

async fn bind(Path((realm, endpoint)): Path<(String, String)>, Extension(actor): Extension<Actor>, body: Option<Json<BindBody>>) -> ApiResult {
    let c = console()?;
    let mode = BindMode::parse(body.and_then(|b| b.0.mode).as_deref().unwrap_or(""));
    op(c.bind(&realm, &endpoint, mode, &actor).await)?;
    ok(json!({"bound": endpoint, "realm": realm}))
}

async fn unbind(Path((realm, endpoint)): Path<(String, String)>, Extension(actor): Extension<Actor>) -> ApiResult {
    let c = console()?;
    op(c.unbind(&realm, &endpoint, &actor).await)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
