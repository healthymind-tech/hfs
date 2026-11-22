//! The Verify panels' routes (configure-then-verify, UX-02/07/08/11): each
//! runs the backend's step-by-step verification of the settings the form
//! holds *right now* (saved or not) and answers the checklist fragment.
//!
//! * `POST /ui/e/{ep}/security/verify` — the endpoint's SMART settings; with
//!   "Inherit" on, the server's own (`HFS_AUTH_*`), so the panel works while
//!   inheriting (UX-11);
//! * `POST /ui/endpoints/verify-auth` — the same in the new-endpoint wizard;
//! * `POST /ui/e/{ep}/terminology/verify`, `POST /ui/endpoints/verify-terminology`
//!   — the terminology server;
//! * `POST /ui/sync/verify?side=source|target` — one external side of a sync job.

use axum::{
    Router,
    body::Bytes,
    extract::{Path, Query, State},
    response::Response,
    routing::post,
};
use helios_auth::verify::{PeerAuth, PeerRole, SyncPeerVerifyRequest, VerifyReport};
use helios_endpoints::EndpointAuth;
use serde::Deserialize;

use super::backend::BackendError;
use super::forms::{self, FieldErrors, Form};
use super::verify::{self, FieldMap, VerifyFragment};
use crate::i18n::{I18n, RequestLocale};
use crate::{WebState, render};

pub(crate) fn routes() -> Router<WebState> {
    Router::new()
        .route("/ui/e/{ep}/security/verify", post(security))
        .route("/ui/e/{ep}/security/test", post(security))
        .route("/ui/endpoints/verify-auth", post(wizard_auth))
        .route("/ui/e/{ep}/terminology/verify", post(terminology))
        .route("/ui/endpoints/verify-terminology", post(wizard_terminology))
        .route("/ui/sync/verify", post(sync_peer))
}

/// The server-level auth settings an inheriting endpoint uses.
pub(crate) fn inherited_auth() -> EndpointAuth {
    helios_auth::verify::endpoint_auth_from_config(&helios_auth::AuthConfig::from_env())
}

fn token(form: &Form) -> Option<String> {
    form.opt("verify_token")
        .map(|t| t.trim().trim_start_matches("Bearer ").trim().to_string())
        .filter(|t| !t.is_empty())
}

fn answer(i18n: I18n, result: Result<VerifyReport, BackendError>, fields: FieldMap) -> Response {
    let r = match result {
        Ok(report) => verify::view(&i18n, &report, fields),
        Err(BackendError::NotImplemented) => verify::unavailable(i18n.t("verify-unavailable")),
        Err(e @ BackendError::Invalid(_)) => {
            let reason = forms::error_links(&i18n, &e.field_errors())
                .into_iter()
                .map(|e| {
                    if e.label.is_empty() {
                        e.message
                    } else {
                        format!("{}: {}", e.label, e.message)
                    }
                })
                .collect::<Vec<_>>()
                .join("; ");
            verify::unavailable(i18n.t_arg("verify-failed-to-run", "reason", reason))
        }
        Err(e) => verify::unavailable(i18n.t_arg(
            "verify-failed-to-run",
            "reason",
            backend_reason(&e),
        )),
    };
    render(VerifyFragment { i18n, r })
}

fn backend_reason(e: &BackendError) -> String {
    match e {
        BackendError::Forbidden(m) | BackendError::Unavailable(m) => m.clone(),
        BackendError::NotFound => "not found".into(),
        _ => "unexpected answer from the server".into(),
    }
}

async fn run_auth(
    state: &WebState,
    endpoint: Option<&str>,
    form: &Form,
    inherit: bool,
) -> Result<VerifyReport, BackendError> {
    let Some(rt) = state.manager.as_ref() else {
        return Err(BackendError::NotImplemented);
    };
    let auth = if inherit {
        inherited_auth()
    } else {
        let mut errs = FieldErrors::new();
        forms::parse_auth(form, None, &mut errs)
    };
    let token = token(form);
    rt.backend
        .verify_auth(endpoint, &auth, token.as_deref())
        .await
}

/// `POST /ui/e/{ep}/security/verify`.
async fn security(
    State(state): State<WebState>,
    locale: RequestLocale,
    Path(ep): Path<String>,
    body: Bytes,
) -> Response {
    let form = Form::parse(&body);
    let inherit = form.checked("auth_inherit");
    let result = run_auth(&state, Some(&ep), &form, inherit).await;
    answer(I18n::new(locale), result, verify::AUTH_FIELDS)
}

/// `POST /ui/endpoints/verify-auth` (wizard step 3).
async fn wizard_auth(
    State(state): State<WebState>,
    locale: RequestLocale,
    body: Bytes,
) -> Response {
    let form = Form::parse(&body);
    let inherit = form.get("auth_choice") == "inherit";
    let result = run_auth(&state, None, &form, inherit).await;
    answer(I18n::new(locale), result, verify::AUTH_FIELDS)
}

async fn run_terminology(
    state: &WebState,
    form: &Form,
    inherit: bool,
) -> Result<VerifyReport, BackendError> {
    let Some(rt) = state.manager.as_ref() else {
        return Err(BackendError::NotImplemented);
    };
    let (url, token, timeout) = if inherit {
        (
            std::env::var("HFS_TERMINOLOGY_SERVER").unwrap_or_default(),
            std::env::var("HFS_TERMINOLOGY_SERVER_TOKEN")
                .ok()
                .filter(|t| !t.trim().is_empty()),
            std::env::var("HFS_TERMINOLOGY_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.trim().parse().ok()),
        )
    } else {
        (
            form.get("term_server_url").trim().to_string(),
            form.opt("term_token").filter(|t| !t.trim().is_empty()),
            form.get("term_timeout").trim().parse().ok(),
        )
    };
    rt.backend
        .verify_terminology(&url, token.as_deref(), timeout)
        .await
}

/// `POST /ui/e/{ep}/terminology/verify`.
async fn terminology(
    State(state): State<WebState>,
    locale: RequestLocale,
    Path(_ep): Path<String>,
    body: Bytes,
) -> Response {
    let form = Form::parse(&body);
    let inherit = form.checked("term_inherit");
    let result = run_terminology(&state, &form, inherit).await;
    answer(I18n::new(locale), result, verify::TERMINOLOGY_FIELDS)
}

/// `POST /ui/endpoints/verify-terminology` (wizard step 4).
async fn wizard_terminology(
    State(state): State<WebState>,
    locale: RequestLocale,
    body: Bytes,
) -> Response {
    let form = Form::parse(&body);
    let inherit = form.get("term_choice") == "inherit";
    let result = run_terminology(&state, &form, inherit).await;
    answer(I18n::new(locale), result, verify::TERMINOLOGY_FIELDS)
}

#[derive(Deserialize)]
struct SideQuery {
    side: String,
}

/// The sync form's fields for one side.
fn peer_fields(side: &str) -> FieldMap {
    if side == "target" {
        &[
            ("url", "f-target_url"),
            ("authType", "f-target_auth"),
            ("tokenUrl", "f-target_token_url"),
            ("clientId", "f-target_client_id"),
            ("clientSecret", "f-target_secret"),
            ("token", "f-target_secret"),
            ("username", "f-target_username"),
            ("password", "f-target_secret"),
            ("scope", "f-target_scope"),
            ("resourceTypes", "f-resource_types"),
        ]
    } else {
        &[
            ("url", "f-source_url"),
            ("authType", "f-source_auth"),
            ("tokenUrl", "f-source_token_url"),
            ("clientId", "f-source_client_id"),
            ("clientSecret", "f-source_secret"),
            ("token", "f-source_secret"),
            ("username", "f-source_username"),
            ("password", "f-source_secret"),
            ("scope", "f-source_scope"),
            ("resourceTypes", "f-resource_types"),
        ]
    }
}

/// `POST /ui/sync/verify?side=source|target`.
async fn sync_peer(
    State(state): State<WebState>,
    locale: RequestLocale,
    Query(q): Query<SideQuery>,
    body: Bytes,
) -> Response {
    let i18n = I18n::new(locale);
    let form = Form::parse(&body);
    let side = if q.side == "target" { "target" } else { "source" };
    let f = |name: &str| form.get(&format!("{side}_{name}")).trim().to_string();
    let secret = f("secret");
    let auth = match f("auth").as_str() {
        "bearer" => PeerAuth::Bearer { token: secret },
        "basic" => PeerAuth::Basic {
            username: f("username"),
            password: secret,
        },
        "client-credentials" => PeerAuth::ClientCredentials {
            token_url: f("token_url"),
            client_id: f("client_id"),
            client_secret: secret,
            scope: Some(f("scope")).filter(|s| !s.is_empty()),
        },
        _ => PeerAuth::None,
    };
    let request = SyncPeerVerifyRequest {
        role: if side == "target" {
            PeerRole::Target
        } else {
            PeerRole::Source
        },
        url: f("url"),
        auth,
        resource_types: form
            .get("resource_types")
            .split([',', ' ', '\n'])
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect(),
        fhir_version: None,
        job_id: None,
    };
    let result = match state.manager.as_ref() {
        Some(rt) => rt.backend.verify_sync_peer(request).await,
        None => Err(BackendError::NotImplemented),
    };
    answer(i18n, result, peer_fields(side))
}
