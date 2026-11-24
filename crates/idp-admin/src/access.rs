//! A user's resource access in bulk: the operations the console's
//! resource-access editor and `/admin/idp/realms/{realm}/users/{user}/links`
//! share (over [`helios_idp::access`]).
//!
//! | method | body | effect |
//! |---|---|---|
//! | `GET` | — | the list: JSON `{links, entries, total, fingerprint}`, or `text/csv` / `text/plain` (`Accept` or `?format=csv\|lines\|json`) |
//! | `PUT` | JSON (links, entries, strings), `text/csv` or `text/plain` | replaces the list |
//! | `PATCH` | the same, or JSON `{"add": [entries], "remove": [{endpoint, reference}]}` | adds (or re-kinds), removes |
//!
//! Query: `endpoint` (rows without one), `dryRun=true` (plan only),
//! `skipInvalid=true` (apply the good rows even if some are bad; otherwise
//! any bad row refuses the whole request with `422`). `If-Match` takes the
//! list's fingerprint (`ETag`). At most [`helios_idp::access::MAX_IMPORT`] rows per request; the
//! existence of the references on the FHIR endpoint is not checked here
//! (the console checks it, as the signed-in user, in its preview).

// Handlers return `Result<Response, Response>` (both arms are responses).
#![allow(clippy::result_large_err)]

use std::collections::HashSet;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::{Path, Query};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use helios_endpoints::Actor;
use helios_idp::access::{self, AccessEntry, Format, InputError, Mode, Parsed, Plan, RowStatus};
use helios_idp::admin::AccessChange;
use helios_idp::store::RealmRecord;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::service::{IdpConsole, OpError, OpResult, error_json};

/// The user's current list (flattened, canonical order).
pub async fn entries(
    c: &IdpConsole,
    rec: &RealmRecord,
    user_id: &str,
) -> OpResult<Vec<AccessEntry>> {
    let u = c
        .admin
        .runtime()
        .store()
        .get_user(&rec.id, user_id)
        .await?
        .ok_or_else(|| OpError::other(404, format!("no user {user_id}")))?;
    Ok(access::flatten(&u.links))
}

/// Parses input text, mapping whole-input errors.
pub fn parse_input(text: &str) -> OpResult<Parsed> {
    access::parse(text).map_err(|e| match e {
        InputError::TooLarge(_) | InputError::TooMany(_) => OpError {
            status: 413,
            field: Some("text".into()),
            message: e.to_string(),
        },
        InputError::BadJson(_) => OpError::invalid("text", e.to_string()),
    })
}

/// Plans `text` against the user's list.
pub async fn plan(
    c: &IdpConsole,
    rec: &RealmRecord,
    user_id: &str,
    text: &str,
    mode: Mode,
    default_endpoint: Option<&str>,
    skip: &HashSet<(String, String)>,
) -> OpResult<(Plan, String)> {
    let current = entries(c, rec, user_id).await?;
    let parsed = parse_input(text)?;
    let pl = access::plan(
        &current,
        &parsed,
        mode,
        default_endpoint.filter(|e| !e.is_empty()),
        &rec.spec.endpoints,
        skip,
    );
    Ok((pl, access::fingerprint(&current)))
}

/// Stores `result`, recording who did it.
pub async fn store(
    c: &IdpConsole,
    rec: &RealmRecord,
    user_id: &str,
    result: Vec<AccessEntry>,
    expect: Option<&str>,
    actor: &Actor,
    action: &str,
) -> OpResult<AccessChange> {
    Ok(c.admin
        .set_user_access(&rec.id, user_id, result, expect, &actor.subject, action)
        .await?)
}

/// Adds / re-kinds `add` and removes `remove` (console add, bulk remove,
/// undo; API `PATCH` with `{add, remove}`). Unknown endpoints are refused.
#[allow(clippy::too_many_arguments)]
pub async fn patch(
    c: &IdpConsole,
    rec: &RealmRecord,
    user_id: &str,
    add: &[AccessEntry],
    remove: &[(String, String)],
    expect: Option<&str>,
    actor: &Actor,
    action: &str,
) -> OpResult<AccessChange> {
    if add.len() + remove.len() > access::MAX_PER_USER {
        return Err(OpError {
            status: 413,
            field: None,
            message: format!("at most {} changes at once", access::MAX_PER_USER),
        });
    }
    if let Some(a) = add
        .iter()
        .find(|a| !rec.spec.endpoints.contains(&a.endpoint))
    {
        return Err(OpError::invalid(
            "endpoint",
            format!("endpoint {:?} is not bound to this realm", a.endpoint),
        ));
    }
    let current = entries(c, rec, user_id).await?;
    if let Some(fp) = expect
        && access::fingerprint(&current) != fp
    {
        return Err(OpError::other(
            409,
            "the user's resource access changed since it was read; reload and try again",
        ));
    }
    let next = access::patch(&current, add, remove);
    store(c, rec, user_id, next, expect, actor, action).await
}

// ------------------------------------------------------------------ API

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LinksQuery {
    format: Option<String>,
    endpoint: Option<String>,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    skip_invalid: bool,
}

fn fail(e: &OpError) -> Response {
    let status = StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, axum::Json(error_json(e))).into_response()
}

fn console() -> Result<std::sync::Arc<IdpConsole>, Response> {
    crate::service::global().ok_or_else(|| {
        fail(&OpError::other(
            503,
            "the built-in authorization server is not enabled (HFS_IDP=on)",
        ))
    })
}

fn if_match(h: &HeaderMap) -> Option<String> {
    h.get(header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.trim()
                .trim_start_matches("W/")
                .trim_matches('"')
                .to_string()
        })
        .filter(|v| !v.is_empty() && v != "*")
}

fn etag(fp: &str) -> (header::HeaderName, String) {
    (header::ETAG, format!("\"{fp}\""))
}

/// The format a `GET` asks for (`?format=` wins over `Accept`).
fn wanted(q: &LinksQuery, h: &HeaderMap) -> Format {
    if let Some(f) = q.format.as_deref().and_then(Format::parse) {
        return f;
    }
    let accept = h
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if accept.contains("text/csv") {
        Format::Csv
    } else if accept.contains("text/plain") {
        Format::Lines
    } else {
        Format::Json
    }
}

pub(crate) async fn get_links(
    Path((realm, user)): Path<(String, String)>,
    Query(q): Query<LinksQuery>,
    h: HeaderMap,
) -> Result<Response, Response> {
    let c = console()?;
    let rec = c.realm(&realm).await.map_err(|e| fail(&e))?;
    let list = entries(&c, &rec, &user).await.map_err(|e| fail(&e))?;
    let fp = access::fingerprint(&list);
    let f = wanted(&q, &h);
    if f == Format::Json && q.format.is_none() {
        return Ok((
            [etag(&fp)],
            axum::Json(json!({
                "links": access::to_links(&list),
                "entries": list,
                "total": list.len(),
                "fingerprint": fp,
            })),
        )
            .into_response());
    }
    Ok((
        [
            (header::CONTENT_TYPE, f.media_type().to_string()),
            etag(&fp),
        ],
        access::export(&list, f),
    )
        .into_response())
}

/// A `PATCH` body of explicit changes.
#[derive(Deserialize)]
struct Changes {
    #[serde(default)]
    add: Vec<AccessEntry>,
    #[serde(default)]
    remove: Vec<Key>,
}

#[derive(Deserialize)]
struct Key {
    endpoint: String,
    reference: String,
}

fn body_text(body: &Bytes) -> Result<String, Response> {
    if body.len() > access::MAX_TEXT_BYTES {
        return Err(fail(&OpError {
            status: 413,
            field: None,
            message: format!("at most {} bytes", access::MAX_TEXT_BYTES),
        }));
    }
    String::from_utf8(body.to_vec())
        .map_err(|_| fail(&OpError::invalid("body", "the body is not UTF-8 text")))
}

fn plan_json(pl: &Plan, fp: &str) -> Value {
    let rejected: Vec<Value> = pl
        .rows
        .iter()
        .filter(|r| {
            matches!(
                r.status,
                RowStatus::Invalid | RowStatus::Duplicate | RowStatus::Conflict
            )
        })
        .take(500)
        .map(|r| {
            json!({
                "line": r.line,
                "input": r.raw,
                "endpoint": r.endpoint,
                "reference": r.reference,
                "status": r.status.code(),
                "issue": r.issue.as_ref().map(|i| i.code()),
                "message": r.issue.as_ref().map(|i| i.message()),
            })
        })
        .collect();
    json!({
        "mode": pl.mode,
        "format": pl.format,
        "rows": pl.rows.len(),
        "counts": pl.counts(),
        "rejected": rejected,
        "added": pl.added.len(),
        "removed": pl.removed.len(),
        "changed": pl.changed.len(),
        "total": pl.result.len(),
        "fingerprint": fp,
    })
}

fn change_json(ch: &AccessChange, pl: Option<&Plan>) -> Value {
    let mut v = json!({
        "added": ch.added,
        "removed": ch.removed,
        "changed": ch.changed,
        "total": ch.total,
        "fingerprint": ch.fingerprint,
    });
    if let Some(pl) = pl {
        v["counts"] = json!(pl.counts());
    }
    v
}

async fn import(
    realm: String,
    user: String,
    q: LinksQuery,
    h: HeaderMap,
    actor: Actor,
    body: Bytes,
    mode: Mode,
) -> Result<Response, Response> {
    let c = console()?;
    let rec = c.realm(&realm).await.map_err(|e| fail(&e))?;
    let text = body_text(&body)?;
    let expect = if_match(&h);
    // PATCH with explicit {add, remove}.
    if mode == Mode::Add
        && let Ok(ch) = serde_json::from_str::<Changes>(text.trim())
        && text.trim_start().starts_with('{')
        && (text.contains("\"add\"") || text.contains("\"remove\""))
    {
        for a in &ch.add {
            access::check(std::slice::from_ref(a)).map_err(|e| {
                fail(&OpError {
                    status: 422,
                    field: Some("add".into()),
                    message: e.to_string(),
                })
            })?;
        }
        let remove: Vec<(String, String)> = ch
            .remove
            .into_iter()
            .map(|k| (k.endpoint, k.reference))
            .collect();
        if q.dry_run {
            let current = entries(&c, &rec, &user).await.map_err(|e| fail(&e))?;
            let next = access::patch(&current, &ch.add, &remove);
            access::check(&next).map_err(|e| {
                fail(&OpError {
                    status: 422,
                    field: None,
                    message: e.to_string(),
                })
            })?;
            return Ok(axum::Json(json!({"dryRun": true, "total": next.len()})).into_response());
        }
        let done = patch(
            &c,
            &rec,
            &user,
            &ch.add,
            &remove,
            expect.as_deref(),
            &actor,
            "patch",
        )
        .await
        .map_err(|e| fail(&e))?;
        return Ok((
            [etag(&done.fingerprint)],
            axum::Json(change_json(&done, None)),
        )
            .into_response());
    }
    let (pl, fp) = plan(
        &c,
        &rec,
        &user,
        &text,
        mode,
        q.endpoint.as_deref(),
        &HashSet::new(),
    )
    .await
    .map_err(|e| fail(&e))?;
    if let Some(e) = &expect
        && *e != fp
    {
        return Err(fail(&OpError::other(
            412,
            "the user's resource access changed since it was read (If-Match)",
        )));
    }
    let mut out = plan_json(&pl, &fp);
    if q.dry_run {
        out["dryRun"] = json!(true);
        return Ok(axum::Json(out).into_response());
    }
    if pl.has_rejects() && !q.skip_invalid {
        out["error"] = json!("some rows were not accepted; fix them or pass skipInvalid=true");
        out["status"] = json!(422);
        return Err((StatusCode::UNPROCESSABLE_ENTITY, axum::Json(out)).into_response());
    }
    let action = match mode {
        Mode::Add => "add",
        Mode::Replace => "replace",
    };
    let done = store(
        &c,
        &rec,
        &user,
        pl.result.clone(),
        Some(&fp),
        &actor,
        action,
    )
    .await
    .map_err(|e| fail(&e))?;
    Ok((
        [etag(&done.fingerprint)],
        axum::Json(change_json(&done, Some(&pl))),
    )
        .into_response())
}

pub(crate) async fn put_links(
    Path((realm, user)): Path<(String, String)>,
    Query(q): Query<LinksQuery>,
    Extension(actor): Extension<Actor>,
    h: HeaderMap,
    body: Bytes,
) -> Result<Response, Response> {
    import(realm, user, q, h, actor, body, Mode::Replace).await
}

pub(crate) async fn patch_links(
    Path((realm, user)): Path<(String, String)>,
    Query(q): Query<LinksQuery>,
    Extension(actor): Extension<Actor>,
    h: HeaderMap,
    body: Bytes,
) -> Result<Response, Response> {
    import(realm, user, q, h, actor, body, Mode::Add).await
}
