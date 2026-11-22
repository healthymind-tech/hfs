//! [`AuthorizationCompleter`] — ends an interaction started by `/authorize`
//! (design §8.2 steps 3–5). The decision from the hosted pages is
//! re-validated here; nothing from the browser is trusted.

use async_trait::async_trait;
use chrono::{Duration, Utc};
use helios_idp::OAuthError;
use helios_idp::interaction::{
    AuthorizationCompleter, AuthorizationResponse, CompleteError, Decision, Prompt,
    deny_code_allowed,
};
use helios_idp::store::{AuthorizationCode, GrantRecord};
use helios_idp::token::{HandleKind, handle_hash, mint_handle};
use helios_idp::user::UserKind;
use helios_idp::{OAuthErrorCode, util::random_b64};

use crate::IdpServer;

fn internal(e: impl std::fmt::Display) -> CompleteError {
    tracing::error!(error = %e, "idp completion failed");
    CompleteError::Internal("temporary failure".into())
}

impl IdpServer {
    async fn take(
        &self,
        realm: &str,
        interaction: &str,
    ) -> Result<
        (
            std::sync::Arc<helios_idp::runtime::RealmRuntime>,
            helios_idp::interaction::PendingAuthorization,
        ),
        CompleteError,
    > {
        let rt = self
            .inner
            .rt
            .realm(realm)
            .filter(|r| r.spec.enabled)
            .ok_or(CompleteError::UnknownInteraction)?;
        let hash = handle_hash(HandleKind::Interaction, interaction)
            .ok_or(CompleteError::UnknownInteraction)?;
        let p = self
            .inner
            .rt
            .store()
            .take_interaction(&rt.id, &hash, Utc::now())
            .await
            .map_err(internal)?
            .ok_or(CompleteError::UnknownInteraction)?;
        Ok((rt, p))
    }
}

#[async_trait]
impl AuthorizationCompleter for IdpServer {
    async fn complete(
        &self,
        realm: &str,
        interaction: &str,
        d: Decision,
    ) -> Result<AuthorizationResponse, CompleteError> {
        let (rt, p) = self.take(realm, interaction).await?;
        let store = self.inner.rt.store();
        let now = Utc::now();
        let bad = |m: &str| Err(CompleteError::InvalidDecision(m.to_string()));

        // Scopes: a subset of what /authorize accepted, nothing new.
        if d.granted_scopes.is_empty() {
            return bad("no scope approved (use deny with access_denied)");
        }
        if let Some(s) = d.granted_scopes.iter().find(|s| !p.scopes.contains(s)) {
            return Err(CompleteError::InvalidDecision(format!(
                "scope {s:?} was not requested"
            )));
        }
        let mut scopes: Vec<String> = p
            .scopes
            .iter()
            .filter(|s| d.granted_scopes.contains(s))
            .cloned()
            .collect();
        // The user and the client must still be valid.
        let user = match store.get_user(&rt.id, &d.subject).await.map_err(internal)? {
            Some(u) if !u.disabled => u,
            _ => return bad("unknown or disabled user"),
        };
        let client = match store
            .get_client(&rt.id, &p.client_id)
            .await
            .map_err(internal)?
        {
            Some(c) if !c.disabled => c,
            _ => return bad("the client was disabled"),
        };
        if d.auth_time > now + Duration::seconds(30) {
            return bad("auth_time is in the future");
        }
        if let Some(max) = p.max_age
            && now.signed_duration_since(d.auth_time) > Duration::seconds(max as i64)
        {
            return bad("max_age exceeded: the user must sign in again");
        }
        if p.prompt.contains(&Prompt::Login) && d.auth_time < p.created_at - Duration::seconds(5) {
            return bad("prompt=login requires a fresh sign-in");
        }

        // Launch context: the EHR launch handle's values win; the pages may
        // only fill what is missing.
        let mut ctx = p.launch_context.clone();
        let wants_patient = scopes.iter().any(|s| s == "launch/patient");
        let wants_encounter = scopes.iter().any(|s| s == "launch/encounter");
        if ctx.patient.is_none() && wants_patient {
            ctx.patient = d.context.patient.clone();
        }
        if ctx.encounter.is_none() && wants_encounter {
            ctx.encounter = d.context.encounter.clone();
        }
        if !p.ehr_launch && ctx.fhir_context.is_empty() {
            ctx.fhir_context = d
                .context
                .fhir_context
                .iter()
                .filter(|i| i.is_valid())
                .cloned()
                .collect();
        }
        if wants_patient && ctx.patient.is_none() {
            return bad("launch/patient was approved but no patient was selected");
        }
        if let Some(pid) = &ctx.patient {
            let valid_id = !pid.is_empty()
                && pid.len() <= 64
                && pid
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
            if !valid_id {
                return bad("patient id is not a FHIR id");
            }
            // Patients and proxies can only pick from their own set; the
            // picker for practitioners searches with their own permissions.
            if p.launch_context.patient.is_none()
                && matches!(user.kind, UserKind::Patient | UserKind::RelatedPerson)
                && !user.selectable_patients(&p.endpoint).contains(pid)
            {
                return bad("the selected patient is not accessible to this user");
            }
        }
        if ctx.patient.is_some()
            && ctx.need_patient_banner.is_none()
            && rt.spec.features.context_banner
        {
            ctx.need_patient_banner = Some(true);
        }
        if !scopes.iter().any(|s| s == "launch/patient") && !p.ehr_launch {
            ctx.patient = None;
        }
        // `fhirUser` from the user's own link (not from the decision).
        let base = rt
            .endpoint_base(&p.endpoint)
            .unwrap_or(&p.audience)
            .to_string();
        let fhir_user = if scopes.iter().any(|s| s == "fhirUser") {
            user.fhir_user(&p.endpoint, &base)
        } else {
            None
        };
        if scopes.iter().any(|s| s == "fhirUser") && fhir_user.is_none() {
            // Not linked on this endpoint: the scope cannot be honoured.
            scopes.retain(|s| s != "fhirUser");
        }

        let gid = format!("g_{}", random_b64(16));
        let lt = &rt.spec.lifetimes;
        store
            .put_grant(GrantRecord {
                id: gid.clone(),
                realm_id: rt.id.clone(),
                client_id: client.client_id.clone(),
                subject: user.id.clone(),
                scopes: scopes.clone(),
                audience: vec![p.audience.clone()],
                context: ctx.clone(),
                fhir_user: fhir_user.clone(),
                sso_session_id: d.session_id_hash.clone(),
                auth_time: Some(d.auth_time),
                created_at: now,
                expires_at: now
                    + Duration::seconds(lt.authorization_code as i64 + lt.access_token as i64),
                revoked_at: None,
                revoked_reason: None,
            })
            .await
            .map_err(internal)?;
        let code = mint_handle(HandleKind::AuthorizationCode);
        store
            .put_code(AuthorizationCode {
                code_hash: code.hash,
                realm_id: rt.id.clone(),
                grant_id: gid.clone(),
                client_id: client.client_id.clone(),
                redirect_uri: p.redirect_uri.clone(),
                code_challenge: p.code_challenge.clone(),
                subject: user.id.clone(),
                scopes,
                audience: vec![p.audience.clone()],
                endpoint: Some(p.endpoint.clone()),
                nonce: p.nonce.clone(),
                auth_time: d.auth_time,
                sso_session_id: d.session_id_hash.clone(),
                sid: d.sid.clone(),
                amr: d.amr.clone(),
                acr: d.acr.clone(),
                fhir_user,
                context: ctx,
                dpop_jkt: None,
                expires_at: now + Duration::seconds(lt.authorization_code as i64),
            })
            .await
            .map_err(internal)?;
        let mut params = vec![("code".to_string(), code.value)];
        if let Some(s) = &p.state {
            params.push(("state".into(), s.clone()));
        }
        params.push(("iss".into(), rt.issuer().to_string()));
        Ok(AuthorizationResponse {
            redirect_uri: p.redirect_uri,
            mode: p.response_mode,
            params,
        })
    }

    async fn deny(
        &self,
        realm: &str,
        interaction: &str,
        error: OAuthError,
    ) -> Result<AuthorizationResponse, CompleteError> {
        let (rt, p) = self.take(realm, interaction).await?;
        let err = if deny_code_allowed(error.code) {
            error
        } else {
            OAuthError::new(OAuthErrorCode::AccessDenied, "the request was denied")
        };
        Ok(AuthorizationResponse::error(
            &p.redirect_uri,
            p.response_mode,
            &err,
            p.state.as_deref(),
            rt.issuer(),
        ))
    }
}
