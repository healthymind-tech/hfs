//! The per-endpoint configuration a request runs under.
//!
//! Every policy section of an [`EndpointSpec`](helios_endpoints::EndpointSpec)
//! is optional; `None` means "inherit the server's environment
//! configuration" (`HFS_VALIDATION_*`, `HFS_TERMINOLOGY_*`, `HFS_CORS_*`,
//! `HFS_RATE_LIMIT_*`). [`EffectiveConfig`] carries the overriding sections of
//! the request's endpoint; each consumer keeps its environment-driven path for
//! a `None` section, so an inherited endpoint behaves exactly like today.

use std::sync::Arc;

use axum::http::request::Parts;
use helios_endpoints::model::TerminologyValidation;
use helios_endpoints::{
    CorsSettings, Endpoint, RateLimitSettings, ResolvedEndpoint, TerminologyBinding,
    ValidationSettings,
};
use helios_fhir::FhirVersion;

/// What a request's endpoint overrides. `None` = inherit the environment.
#[derive(Debug, Clone, Default)]
pub struct EffectiveConfig {
    /// The endpoint, when the request came through the dispatcher.
    pub endpoint: Option<Arc<Endpoint>>,
    /// Write-time validation settings.
    pub validation: Option<ValidationSettings>,
    /// Terminology server binding.
    pub terminology: Option<TerminologyBinding>,
    /// CORS.
    pub cors: Option<CorsSettings>,
    /// Per-client rate limits (consumed by the P2 rate limiter).
    pub rate_limit: Option<RateLimitSettings>,
    /// The endpoint's default FHIR version (`None` when inherited or the
    /// binary lacks the release).
    pub fhir_version: Option<FhirVersion>,
}

impl EffectiveConfig {
    /// Everything inherited (no endpoint: manager off, or a request that did
    /// not come through the dispatcher).
    pub fn inherited() -> Self {
        Self::default()
    }

    /// The effective configuration of a request (reads the
    /// `ResolvedEndpoint` extension the dispatcher inserted).
    pub fn for_request(parts: &Parts) -> Self {
        Self::for_endpoint(parts.extensions.get::<ResolvedEndpoint>())
    }

    /// The effective configuration of the request running on this task.
    pub fn current() -> Self {
        super::with_current(|r| Self::for_endpoint(Some(r))).unwrap_or_default()
    }

    /// The effective configuration of `endpoint`.
    pub fn for_endpoint(endpoint: Option<&ResolvedEndpoint>) -> Self {
        let Some(r) = endpoint else {
            return Self::inherited();
        };
        let spec = &r.endpoint.spec;
        Self {
            endpoint: Some(Arc::clone(&r.endpoint)),
            validation: spec.validation.clone(),
            terminology: spec.terminology.clone(),
            cors: spec.cors.clone(),
            rate_limit: spec.rate_limit.clone(),
            fhir_version: super::fhir_version_of(spec.fhir_version),
        }
    }

    /// The FHIR version a request defaults to.
    pub fn fhir_version_or(&self, server_default: FhirVersion) -> FhirVersion {
        self.fhir_version.unwrap_or(server_default)
    }

    /// The terminology server URL the endpoint uses: its own binding when it
    /// has one (which may be "none"), else `env_server`.
    pub fn terminology_server<'a>(&'a self, env_server: Option<&'a str>) -> Option<&'a str> {
        match &self.terminology {
            Some(binding) => binding.server_url.as_deref(),
            None => env_server,
        }
    }
}

/// `HFS_VALIDATION_TERMINOLOGY` spelling of an endpoint's terminology mode.
pub fn terminology_mode_str(t: TerminologyValidation) -> &'static str {
    match t {
        TerminologyValidation::Off => "off",
        TerminologyValidation::Embedded => "embedded",
        TerminologyValidation::Remote => "remote",
        TerminologyValidation::RemoteOnly => "remote-only",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_endpoints::{EndpointName, EndpointSnapshot, EndpointSpec, FhirRelease, Resolution};

    fn resolve(spec: EndpointSpec, path: &str) -> ResolvedEndpoint {
        let default = EndpointSpec::default_endpoint("default", FhirRelease::R4);
        let mk = |spec: EndpointSpec| Endpoint {
            id: spec.name.as_str().into(),
            revision: 1,
            spec,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            created_by: None,
            updated_by: None,
        };
        let snap = EndpointSnapshot::new(1, vec![mk(default), mk(spec)]).unwrap();
        match snap.resolve(path) {
            Resolution::Serve(r) => r,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn inherited_without_endpoint() {
        let e = EffectiveConfig::for_endpoint(None);
        assert!(e.endpoint.is_none() && e.validation.is_none() && e.cors.is_none());
        assert_eq!(e.terminology_server(Some("http://tx")), Some("http://tx"));
        assert!(EffectiveConfig::current().endpoint.is_none());
    }

    #[cfg(feature = "R4")]
    #[test]
    fn named_endpoint_overrides() {
        let mut spec = EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab");
        spec.validation = Some(ValidationSettings::default());
        spec.terminology = Some(TerminologyBinding::default());
        spec.cors = Some(CorsSettings::default());
        spec.rate_limit = Some(RateLimitSettings::default());
        let r = resolve(spec, "/lab/Patient");
        let e = EffectiveConfig::for_endpoint(Some(&r));
        assert_eq!(e.endpoint.as_ref().unwrap().name(), "lab");
        assert!(e.validation.is_some() && e.cors.is_some() && e.rate_limit.is_some());
        // A binding without a server means "no terminology server here".
        assert_eq!(e.terminology_server(Some("http://tx")), None);
        assert_eq!(e.fhir_version_or(FhirVersion::R4), FhirVersion::R4);
    }

    #[test]
    fn terminology_mode_spelling() {
        assert_eq!(terminology_mode_str(TerminologyValidation::Off), "off");
        assert_eq!(
            terminology_mode_str(TerminologyValidation::Embedded),
            "embedded"
        );
        assert_eq!(
            terminology_mode_str(TerminologyValidation::Remote),
            "remote"
        );
        assert_eq!(
            terminology_mode_str(TerminologyValidation::RemoteOnly),
            "remote-only"
        );
    }
}
