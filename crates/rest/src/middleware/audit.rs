//! Bridges the authenticated [`Principal`] into the request's audit context.
//!
//! The audit middleware runs *outside* authentication (so it also records the
//! requests authentication refuses), which means it cannot see the
//! `Principal` the auth middleware puts into the request extensions further
//! in. [`capture_principal`] sits just inside the auth layers and reports the
//! principal through [`helios_audit::context::set_user`].
//!
//! This is the one integration point between `helios-auth` and the audit
//! trail: richer identity (purpose of use, roles) only needs to be mapped in
//! [`audit_user`].

use axum::{extract::Request, middleware::Next, response::Response};
use helios_audit::AuditUser;
use helios_auth::Principal;

/// Maps a validated token to the audit trail's notion of a user.
pub fn audit_user(principal: &Principal) -> AuditUser {
    let claim = |name: &str| {
        principal
            .custom_claims
            .get(name)
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    AuditUser {
        subject: principal.subject().to_string(),
        issuer: Some(principal.issuer().to_string()).filter(|i| !i.is_empty()),
        name: claim("name").or_else(|| claim("preferred_username")),
        fhir_user: claim("fhirUser").or_else(|| claim("fhir_user")),
        client_id: claim("azp").or_else(|| claim("client_id")),
    }
}

/// Reports the request's [`Principal`], if any, to the audit context.
pub async fn capture_principal(request: Request, next: Next) -> Response {
    if let Some(principal) = request.extensions().get::<Principal>() {
        helios_audit::context::set_user(audit_user(principal));
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_standard_and_smart_claims() {
        let mut claims = serde_json::Map::new();
        claims.insert("name".into(), "Dr. Lin".into());
        claims.insert("fhirUser".into(), "Practitioner/9".into());
        claims.insert("azp".into(), "ward-app".into());
        let p = Principal {
            subject: "s-1".into(),
            issuer: "https://idp".into(),
            tenant_id: None,
            scopes: Default::default(),
            jti: None,
            expires_at: chrono::Utc::now(),
            custom_claims: claims,
            launch_context: None,
        };
        let u = audit_user(&p);
        assert_eq!(u.subject, "s-1");
        assert_eq!(u.issuer.as_deref(), Some("https://idp"));
        assert_eq!(u.name.as_deref(), Some("Dr. Lin"));
        assert_eq!(u.fhir_user.as_deref(), Some("Practitioner/9"));
        assert_eq!(u.client_id.as_deref(), Some("ward-app"));
    }
}
