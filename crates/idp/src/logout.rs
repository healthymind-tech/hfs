//! OpenID Connect RP-Initiated Logout 1.0 request validation (design §8.5).
//! The hosted logout page (package I4) owns the session cookie; it calls
//! [`check_logout`] first and only redirects to the returned URI.

use serde_json::Value;
use url::Url;

use crate::error::OAuthError;
use crate::runtime::RealmRuntime;
use crate::store::IdpStore;

/// Parameters of `GET|POST /idp/{realm}/logout` (RP-Initiated Logout §2).
#[derive(Debug, Clone, Default)]
pub struct LogoutRequest {
    pub id_token_hint: Option<String>,
    pub client_id: Option<String>,
    pub post_logout_redirect_uri: Option<String>,
    pub state: Option<String>,
}

/// What the logout page may do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogoutPlan {
    /// The relying party (from the hint's `aud` or `client_id`).
    pub client_id: Option<String>,
    /// `sub` and `sid` of the hint: the session to end must match them.
    pub subject: Option<String>,
    pub sid: Option<String>,
    /// Where to send the user afterwards (`state` appended); only set when
    /// the URI is registered for the client (exact match).
    pub redirect: Option<String>,
    /// Whether the hint proved which RP asked (the page may skip the
    /// "do you want to sign out?" confirmation only then).
    pub hint_valid: bool,
}

/// Validates a logout request. Errors are shown on the OP's page, never
/// redirected (an unverified redirect would be an open redirector).
pub async fn check_logout(
    realm: &RealmRuntime,
    store: &dyn IdpStore,
    req: &LogoutRequest,
) -> Result<LogoutPlan, OAuthError> {
    let mut plan = LogoutPlan::default();
    if let Some(hint) = req.id_token_hint.as_deref() {
        // Expired hints are fine (§2: "the OP SHOULD accept … even if expired").
        let v = realm.verify_own(hint, None).map_err(|_| {
            OAuthError::invalid_request("id_token_hint is not an id_token of this server")
        })?;
        let aud = match v.claims.get("aud") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Array(a)) if a.len() == 1 => a[0].as_str().map(str::to_string),
            _ => None,
        };
        let aud =
            aud.ok_or_else(|| OAuthError::invalid_request("id_token_hint has no single aud"))?;
        if v.claims.get("client_id").is_some() && v.claims.get("gid").is_some() {
            // An access token is not an id_token.
            return Err(OAuthError::invalid_request(
                "id_token_hint is not an id_token",
            ));
        }
        if let Some(cid) = &req.client_id
            && *cid != aud
        {
            return Err(OAuthError::invalid_request(
                "client_id does not match id_token_hint",
            ));
        }
        plan.client_id = Some(aud);
        plan.subject = v
            .claims
            .get("sub")
            .and_then(Value::as_str)
            .map(str::to_string);
        plan.sid = v
            .claims
            .get("sid")
            .and_then(Value::as_str)
            .map(str::to_string);
        plan.hint_valid = true;
    } else {
        plan.client_id = req.client_id.clone();
    }
    if let Some(uri) = req.post_logout_redirect_uri.as_deref() {
        let Some(cid) = plan.client_id.as_deref() else {
            return Err(OAuthError::invalid_request(
                "post_logout_redirect_uri needs id_token_hint or client_id",
            ));
        };
        let client = store
            .get_client(&realm.id, cid)
            .await
            .map_err(|_| OAuthError::new(crate::OAuthErrorCode::ServerError, "store unavailable"))?
            .filter(|c| !c.disabled)
            .ok_or_else(|| OAuthError::invalid_request("unknown client"))?;
        if !client.post_logout_redirect_uris.iter().any(|r| r == uri) {
            return Err(OAuthError::invalid_request(
                "post_logout_redirect_uri is not registered for this client",
            ));
        }
        let mut u = Url::parse(uri).map_err(|_| OAuthError::invalid_request("bad URI"))?;
        if let Some(s) = &req.state {
            u.query_pairs_mut().append_pair("state", s);
        }
        plan.redirect = Some(u.to_string());
    }
    Ok(plan)
}
