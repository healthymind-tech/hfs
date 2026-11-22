//! Discovery documents and the realm JWKS (OIDC Discovery §4, RFC 8414 §3,
//! RFC 7517). Generated from the realm features, so metadata and SMART
//! capabilities never disagree (design §5.3).

use axum::extract::{Path, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};

use crate::IdpServer;
use crate::http;

fn cached_json(v: serde_json::Value, max_age: u32) -> Response {
    let mut r = axum::Json(v).into_response();
    if let Ok(h) = HeaderValue::from_str(&format!("public, max-age={max_age}")) {
        r.headers_mut().insert(header::CACHE_CONTROL, h);
    }
    r
}

pub(crate) async fn openid_configuration(
    State(s): State<IdpServer>,
    Path(realm): Path<String>,
) -> Response {
    match http::realm(&s, &realm) {
        Some(r) => cached_json(
            helios_idp::metadata::authorization_server_metadata(&r.spec, &r.urls),
            300,
        ),
        None => http::not_found(),
    }
}

pub(crate) async fn jwks(State(s): State<IdpServer>, Path(realm): Path<String>) -> Response {
    match http::realm(&s, &realm) {
        Some(r) => {
            let mut resp = cached_json(r.jwks(), 300);
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/jwk-set+json"),
            );
            resp
        }
        None => http::not_found(),
    }
}
