use serde::Serialize;
use serde_json::Value;

use crate::config::AuthConfig;

/// SMART on FHIR configuration document served at
/// `/.well-known/smart-configuration`.
#[derive(Debug, Serialize)]
pub struct SmartConfiguration {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwks_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub introspection_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub management_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revocation_endpoint: Option<String>,
    pub scopes_supported: Vec<String>,
    pub response_types_supported: Vec<String>,
    pub grant_types_supported: Vec<String>,
    pub token_endpoint_auth_methods_supported: Vec<String>,
    pub code_challenge_methods_supported: Vec<String>,
    pub token_endpoint_auth_signing_alg_values_supported: Vec<String>,
    pub capabilities: Vec<String>,
}

/// `scopes_supported` advertised when `HFS_SMART_SCOPES_SUPPORTED` is unset:
/// the scope shapes HFS enforces (SMART v1 and v2, patient/user/system).
pub const DEFAULT_SCOPES_SUPPORTED: &[&str] = &[
    "openid",
    "fhirUser",
    "profile",
    "launch/patient",
    "offline_access",
    "online_access",
    "patient/*.rs",
    "patient/*.read",
    "user/*.cruds",
    "user/*.rs",
    "user/*.read",
    "user/*.write",
    "system/*.cruds",
    "system/*.rs",
    "system/*.read",
];

impl SmartConfiguration {
    /// Build the SMART configuration document from `AuthConfig`.
    ///
    /// Every list can be overridden from the environment (`HFS_SMART_*`), so
    /// the document describes what the IdP in front of this server actually
    /// offers; the defaults describe a Keycloak-style IdP with public,
    /// client-secret and `private_key_jwt` clients.
    pub fn from_config(config: &AuthConfig) -> Self {
        let interactive = config.smart_authorize_endpoint.is_some();
        let owned = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        let response_types_supported = if interactive {
            owned(&["code"])
        } else {
            Vec::new()
        };
        let grant_types_supported = config.smart_grant_types.clone().unwrap_or_else(|| {
            if interactive {
                owned(&["authorization_code", "client_credentials", "refresh_token"])
            } else {
                owned(&["client_credentials"])
            }
        });
        let capabilities = config.smart_capabilities.clone().unwrap_or_else(|| {
            let mut caps = Vec::new();
            if interactive {
                caps.extend(owned(&[
                    "launch-standalone",
                    "context-standalone-patient",
                    "client-public",
                    "client-confidential-symmetric",
                    "sso-openid-connect",
                    "permission-offline",
                    "permission-online",
                    "permission-patient",
                    "permission-user",
                ]));
            }
            caps.extend(owned(&[
                "client-confidential-asymmetric",
                "permission-v1",
                "permission-v2",
            ]));
            caps
        });

        Self {
            issuer: config.expected_issuer.clone(),
            jwks_uri: config
                .smart_jwks_url
                .clone()
                .or_else(|| config.jwks_url.clone()),
            authorization_endpoint: config.smart_authorize_endpoint.clone(),
            token_endpoint: config.smart_token_endpoint.clone(),
            introspection_endpoint: config.smart_introspection_endpoint.clone(),
            management_endpoint: config.smart_management_endpoint.clone(),
            registration_endpoint: config.smart_registration_endpoint.clone(),
            revocation_endpoint: config.smart_revocation_endpoint.clone(),
            scopes_supported: config
                .smart_scopes_supported
                .clone()
                .unwrap_or_else(|| owned(DEFAULT_SCOPES_SUPPORTED)),
            response_types_supported,
            grant_types_supported,
            token_endpoint_auth_methods_supported: config
                .smart_token_auth_methods
                .clone()
                .unwrap_or_else(|| {
                    owned(&[
                        "client_secret_basic",
                        "client_secret_post",
                        "private_key_jwt",
                    ])
                }),
            code_challenge_methods_supported: owned(&["S256"]),
            token_endpoint_auth_signing_alg_values_supported: owned(&["RS384", "ES384"]),
            capabilities,
        }
    }

    /// Serialize to a JSON `Value`.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("SmartConfiguration serialization cannot fail")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_smart_config_from_default() {
        let config = AuthConfig::default();
        let smart = SmartConfiguration::from_config(&config);

        assert!(smart.issuer.is_none());
        assert!(smart.token_endpoint.is_none());
        assert!(smart.capabilities.contains(&"permission-v2".to_string()));
        assert_eq!(smart.code_challenge_methods_supported, vec!["S256"]);
    }

    #[test]
    fn test_smart_config_with_values() {
        let config = AuthConfig {
            expected_issuer: Some("https://idp.example.com".to_string()),
            smart_token_endpoint: Some("https://idp.example.com/token".to_string()),
            smart_authorize_endpoint: Some("https://idp.example.com/authorize".to_string()),
            smart_jwks_url: Some("https://idp.example.com/.well-known/jwks.json".to_string()),
            ..AuthConfig::default()
        };
        let smart = SmartConfiguration::from_config(&config);

        assert_eq!(smart.issuer.as_deref(), Some("https://idp.example.com"));
        assert_eq!(
            smart.token_endpoint.as_deref(),
            Some("https://idp.example.com/token")
        );
        assert!(
            smart
                .grant_types_supported
                .contains(&"authorization_code".to_string())
        );
        assert!(smart.response_types_supported.contains(&"code".to_string()));
        assert_eq!(
            smart.jwks_uri.as_deref(),
            Some("https://idp.example.com/.well-known/jwks.json")
        );
    }

    #[test]
    fn test_smart_config_json_serialization() {
        let config = AuthConfig {
            expected_issuer: Some("https://idp.example.com".to_string()),
            ..AuthConfig::default()
        };
        let smart = SmartConfiguration::from_config(&config);
        let json = smart.to_json();

        assert!(json["capabilities"].is_array());
        assert!(json["scopes_supported"].is_array());
        assert_eq!(
            json["code_challenge_methods_supported"],
            serde_json::json!(["S256"])
        );
        // Fields that are None should be omitted
        assert!(json.get("authorization_endpoint").is_none());
    }

    #[test]
    fn interactive_deployments_advertise_launch_and_patient_capabilities() {
        let config = AuthConfig {
            smart_authorize_endpoint: Some("https://idp/auth".to_string()),
            smart_token_endpoint: Some("https://idp/token".to_string()),
            ..AuthConfig::default()
        };
        let smart = SmartConfiguration::from_config(&config);
        for cap in [
            "launch-standalone",
            "permission-patient",
            "permission-v1",
            "permission-v2",
            "sso-openid-connect",
        ] {
            assert!(smart.capabilities.contains(&cap.to_string()), "{cap}");
        }
        assert_eq!(smart.response_types_supported, vec!["code"]);
        assert!(
            smart
                .grant_types_supported
                .contains(&"refresh_token".to_string())
        );
        assert!(
            !smart
                .code_challenge_methods_supported
                .contains(&"plain".to_string())
        );
    }

    #[test]
    fn backend_only_deployments_do_not_claim_a_launch() {
        let smart = SmartConfiguration::from_config(&AuthConfig::default());
        assert!(
            !smart
                .capabilities
                .contains(&"launch-standalone".to_string())
        );
        assert_eq!(smart.grant_types_supported, vec!["client_credentials"]);
        assert!(smart.response_types_supported.is_empty());
    }

    #[test]
    fn lists_are_overridable() {
        let config = AuthConfig {
            smart_capabilities: Some(vec!["launch-ehr".to_string()]),
            smart_scopes_supported: Some(vec!["system/*.rs".to_string()]),
            smart_token_auth_methods: Some(vec!["private_key_jwt".to_string()]),
            ..AuthConfig::default()
        };
        let smart = SmartConfiguration::from_config(&config);
        assert_eq!(smart.capabilities, vec!["launch-ehr"]);
        assert_eq!(smart.scopes_supported, vec!["system/*.rs"]);
        assert_eq!(
            smart.token_endpoint_auth_methods_supported,
            vec!["private_key_jwt"]
        );
    }
}
