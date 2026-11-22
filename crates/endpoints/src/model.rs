//! The FHIR endpoint model.
//!
//! An [`Endpoint`] is one FHIR service the server exposes: a base path, a FHIR
//! version, a storage binding (v1: one tenant of the shared store) and the
//! per-endpoint policy sections — authentication, validation, terminology,
//! CORS and rate limits. Every policy section is an `Option`: `None` means
//! **inherit the server's environment configuration** (`HFS_AUTH_*`,
//! `HFS_VALIDATION_*`, `HFS_TERMINOLOGY_*`, `HFS_CORS_*`, `HFS_RATE_LIMIT_*`),
//! which is how the default endpoint is created on upgrade so that behaviour
//! does not change until an administrator overrides a section.
//!
//! The JSON form of [`EndpointSpec`] is both the admin REST API body
//! (`PUT /admin/endpoints/{name}`) and the `spec` column of `hfs_endpoints`.
//! Secrets inside it are [`SealedSecret`]s; the API never returns them (see
//! [`Endpoint::redacted`]).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::name::{EndpointName, EndpointPath};
use crate::secret::SealedSecret;

/// FHIR release an endpoint speaks by default (requests may still negotiate
/// a version with the `fhirVersion` media-type parameter where the build
/// supports it). Mirrors `helios_fhir::FhirVersion` without depending on the
/// generated models; the REST layer converts and refuses a release the
/// binary was not compiled with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum FhirRelease {
    /// FHIR R4 (4.0.1).
    #[default]
    R4,
    /// FHIR R4B (4.3.0).
    R4B,
    /// FHIR R5 (5.0.0).
    R5,
    /// FHIR R6 (ballot).
    R6,
}

impl FhirRelease {
    /// The `fhirVersion` string advertised in the CapabilityStatement.
    pub fn fhir_version_string(self) -> &'static str {
        match self {
            FhirRelease::R4 => "4.0.1",
            FhirRelease::R4B => "4.3.0",
            FhirRelease::R5 => "5.0.0",
            FhirRelease::R6 => "6.0.0-ballot3",
        }
    }
}

/// Lifecycle state of an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EndpointStatus {
    /// Routed and serving.
    #[default]
    Enabled,
    /// Routed, but every FHIR request answers `503` + OperationOutcome
    /// (`code=transient`, "endpoint disabled"). Data and config are kept.
    Disabled,
    /// Being created by a duplicate / initial load: routed like `Disabled`
    /// until the job finishes and flips it to `Enabled`.
    Provisioning,
    /// Tombstoned: not routed; its name, path and tenant are free again; the
    /// row and its revisions stay for audit. Data is removed only by an
    /// explicit purge.
    Deleted,
}

impl EndpointStatus {
    /// Whether the routing snapshot carries endpoints in this state.
    pub fn is_routed(self) -> bool {
        !matches!(self, EndpointStatus::Deleted)
    }

    /// Whether FHIR requests are served (vs `503`).
    pub fn is_serving(self) -> bool {
        matches!(self, EndpointStatus::Enabled)
    }

    /// Column value in `hfs_endpoints.status`.
    pub fn as_str(self) -> &'static str {
        match self {
            EndpointStatus::Enabled => "enabled",
            EndpointStatus::Disabled => "disabled",
            EndpointStatus::Provisioning => "provisioning",
            EndpointStatus::Deleted => "deleted",
        }
    }
}

/// Where an endpoint's resources live.
///
/// v1 supports exactly one binding, a tenant of the shared primary store; the
/// enum leaves room for a dedicated database later without changing the
/// JSON shape (`{"kind":"tenant","tenantId":"twcore"}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum StorageBinding {
    /// Rows of the shared store with `tenant_id = tenant_id`. One endpoint per
    /// tenant (unique among non-deleted endpoints).
    #[serde(rename_all = "camelCase")]
    Tenant {
        /// The tenant id (`TenantId::parse`-valid).
        tenant_id: String,
    },
}

impl StorageBinding {
    /// The tenant this binding stores into.
    pub fn tenant_id(&self) -> &str {
        match self {
            StorageBinding::Tenant { tenant_id } => tenant_id,
        }
    }
}

/// Who owns an endpoint's configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ManagedBy {
    /// Created and edited through the admin API / console.
    #[default]
    Ui,
    /// Declared in `HFS_ENDPOINTS_FILE`: reconciled at startup, read-only in
    /// the console and refused (`409`) by the admin API.
    Env,
}

/// How much of the FHIR API needs a credential (same meaning as
/// `HFS_AUTH_MODE`, see docs/ops/auth.md §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMode {
    /// No credential needed; tokens ignored.
    #[default]
    Off,
    /// Anonymous reads; writes need a token.
    WriteProtect,
    /// Everything but discovery, `/metadata` and health needs a token.
    Full,
}

/// Whether `patient/` scopes may read shared reference types outside the
/// Patient compartment (`HFS_AUTH_PATIENT_NON_COMPARTMENT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NonCompartmentPolicy {
    /// Readable (US Core apps need them).
    #[default]
    Allow,
    /// Unreachable.
    Deny,
}

/// Scope policy of an endpoint (what the authorization layer accepts).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScopePolicy {
    /// Accept SMART v1 scopes (`.read`, `.write`, `.*`) besides v2.
    pub accept_v1: bool,
    /// Accept `patient/` scopes.
    pub allow_patient: bool,
    /// Accept `user/` scopes.
    pub allow_user: bool,
    /// Accept `system/` scopes.
    pub allow_system: bool,
    /// See [`NonCompartmentPolicy`].
    pub patient_non_compartment: NonCompartmentPolicy,
    /// Shared reference types opened by `Allow`; `None` = the server default
    /// (`HFS_AUTH_PATIENT_SHARED_TYPES`).
    pub patient_shared_types: Option<Vec<String>>,
}

impl Default for ScopePolicy {
    fn default() -> Self {
        Self {
            accept_v1: true,
            allow_patient: true,
            allow_user: true,
            allow_system: true,
            patient_non_compartment: NonCompartmentPolicy::Allow,
            patient_shared_types: None,
        }
    }
}

/// What `/.well-known/smart-configuration` and the CapabilityStatement's
/// `oauth-uris` advertise for an endpoint (`HFS_SMART_*`). Unset endpoints
/// are discovered from `{issuer}/.well-known/openid-configuration` when
/// `discover_from_issuer` is on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SmartDiscovery {
    /// Fill unset endpoints from the issuer's OIDC discovery document.
    pub discover_from_issuer: bool,
    /// `authorization_endpoint`.
    pub authorization_endpoint: Option<String>,
    /// `token_endpoint`.
    pub token_endpoint: Option<String>,
    /// `jwks_uri` advertised to clients (may differ from the in-network
    /// [`EndpointAuth::jwks_url`] the server fetches).
    pub jwks_uri: Option<String>,
    /// `introspection_endpoint`.
    pub introspection_endpoint: Option<String>,
    /// `revocation_endpoint`.
    pub revocation_endpoint: Option<String>,
    /// `registration_endpoint`.
    pub registration_endpoint: Option<String>,
    /// `management_endpoint`.
    pub management_endpoint: Option<String>,
    /// `end_session_endpoint`.
    pub end_session_endpoint: Option<String>,
    /// `capabilities`; `None` = server default.
    pub capabilities: Option<Vec<String>>,
    /// `scopes_supported`; `None` = server default.
    pub scopes_supported: Option<Vec<String>>,
}

impl Default for SmartDiscovery {
    fn default() -> Self {
        Self {
            discover_from_issuer: true,
            authorization_endpoint: None,
            token_endpoint: None,
            jwks_uri: None,
            introspection_endpoint: None,
            revocation_endpoint: None,
            registration_endpoint: None,
            management_endpoint: None,
            end_session_endpoint: None,
            capabilities: None,
            scopes_supported: None,
        }
    }
}

/// SMART on FHIR / OAuth resource-server settings of one endpoint.
///
/// Validated tokens are checked against `issuer` (exact `iss`), the JWKS at
/// `jwks_url` and `audience`. The verifier for a given
/// `(issuer, jwks_url, algorithms)` is shared by every endpoint that names
/// it (package P2's verifier cache), so two endpoints on one IdP fetch its
/// keys once; give each endpoint its own `audience` so a token minted for one
/// is refused by the other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EndpointAuth {
    /// What needs a credential.
    pub mode: AuthMode,
    /// Exact `iss`. Required unless `mode` is `off`.
    pub issuer: Option<String>,
    /// JWKS the server fetches. Required unless `mode` is `off`.
    pub jwks_url: Option<String>,
    /// Required `aud`. Strongly recommended; the console warns without it.
    pub audience: Option<String>,
    /// Accepted algorithms; empty = `RS256,RS384,ES256,ES384`.
    pub algorithms: Vec<String>,
    /// `exp`/`nbf` leeway; `None` = 60.
    pub clock_skew_secs: Option<u32>,
    /// Longest a fetched JWKS is trusted; `None` = 300.
    pub jwks_max_age_secs: Option<u32>,
    /// Scope policy.
    pub scopes: ScopePolicy,
    /// SMART discovery document.
    pub smart: SmartDiscovery,
    /// Accept the administrator credential (`HFS_ADMIN_PASSWORD`, admin
    /// console session) as an all-scopes administrator on this endpoint.
    pub accept_admin_credential: bool,
    /// Who issues the tokens: an external IdP (`issuer` + `jwksUrl`, the
    /// default, so rows written before the built-in authorization server
    /// existed are unchanged) or an internal realm of this HFS
    /// (docs/design/authorization-server.md §5.2, D-3).
    pub provider: AuthProviderRef,
}

/// The authorization server behind an endpoint's auth section
/// (docs/design/authorization-server.md §5.2, D-3).
///
/// `Internal` endpoints verify tokens with the realm's in-process public
/// keys (no JWKS fetch) and its revoked-grant set (revocation is immediate);
/// `issuer`, `jwksUrl` and the SMART URLs are derived from the realm, and
/// `audience` defaults to the endpoint's public FHIR base URL. With
/// `HFS_IDP=off` an internal endpoint stays closed (`503`), never open.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AuthProviderRef {
    /// An external issuer (Keycloak, Entra, Epic, …): `issuer` + `jwksUrl`.
    #[default]
    External,
    /// A realm of the built-in authorization server (`/idp/{realm}`).
    Internal {
        /// Realm name (same grammar as endpoint path segments).
        realm: String,
    },
}

impl AuthProviderRef {
    /// The realm of an internal provider.
    pub fn internal_realm(&self) -> Option<&str> {
        match self {
            Self::Internal { realm } => Some(realm.as_str()),
            Self::External => None,
        }
    }
}

impl Default for EndpointAuth {
    fn default() -> Self {
        Self {
            mode: AuthMode::Off,
            issuer: None,
            jwks_url: None,
            audience: None,
            algorithms: Vec::new(),
            clock_skew_secs: None,
            jwks_max_age_secs: None,
            scopes: ScopePolicy::default(),
            smart: SmartDiscovery::default(),
            accept_admin_credential: true,
            provider: AuthProviderRef::External,
        }
    }
}

/// Write-time validation (`HFS_VALIDATION_MODE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ValidationMode {
    /// Writes are not validated (`$validate` still works).
    #[default]
    Off,
    /// Validated; issues logged, writes accepted.
    Log,
    /// Validated; a write with errors is refused (`422`).
    Enforce,
}

/// Terminology checking during validation (`HFS_VALIDATION_TERMINOLOGY`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TerminologyValidation {
    /// Bindings not checked.
    Off,
    /// Embedded core value sets only.
    #[default]
    Embedded,
    /// Embedded first, then the endpoint's terminology server.
    Remote,
    /// Only the terminology server.
    RemoteOnly,
}

/// "Resources of `resource_type` written to this endpoint must conform to
/// `profile`" — validated in addition to `meta.profile` claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileRequirement {
    /// Resource type, e.g. `Patient`.
    pub resource_type: String,
    /// Canonical URL, optionally `|version`.
    pub profile: String,
}

/// Validation settings of one endpoint. Which IG packages are installed is
/// state of the endpoint's tenant (`/admin/endpoints/{name}/packages`), not
/// part of this section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ValidationSettings {
    /// Write-time mode.
    pub mode: ValidationMode,
    /// Terminology checking.
    pub terminology: TerminologyValidation,
    /// Validate against StructureDefinitions stored in the endpoint
    /// (`HFS_VALIDATION_STORED_PROFILES`).
    pub stored_profiles: bool,
    /// Extra profiles every write of a type must meet.
    pub required_profiles: Vec<ProfileRequirement>,
}

impl Default for ValidationSettings {
    fn default() -> Self {
        Self {
            mode: ValidationMode::Off,
            terminology: TerminologyValidation::Embedded,
            stored_profiles: true,
            required_profiles: Vec::new(),
        }
    }
}

/// The terminology server an endpoint uses (`$expand` for `:in`, remote
/// validation, FHIRPath `memberOf`, IG terminology imports).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct TerminologyBinding {
    /// Base URL the server calls (`HFS_TERMINOLOGY_SERVER`); `None` = no
    /// terminology server for this endpoint.
    pub server_url: Option<String>,
    /// Browser-reachable console URL (`HFS_TERMINOLOGY_PUBLIC_URL`).
    pub public_url: Option<String>,
    /// Request timeout; `None` = server default.
    pub timeout_secs: Option<u32>,
    /// Bearer sent on writes (IG terminology imports). Sealed; AAD
    /// `endpoint:{id}:terminology.token`.
    pub token: Option<SealedSecret>,
}

/// CORS for browser clients of one endpoint (`HFS_CORS_*`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CorsSettings {
    /// Answer CORS at all.
    pub enabled: bool,
    /// Allowed origins (`https://app.example.org`), or `["*"]`.
    pub allowed_origins: Vec<String>,
    /// `Access-Control-Allow-Credentials` (never with `*`).
    pub allow_credentials: bool,
    /// Extra request headers on top of the default list.
    pub extra_headers: Vec<String>,
}

impl Default for CorsSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_origins: vec!["*".to_string()],
            allow_credentials: false,
            extra_headers: Vec::new(),
        }
    }
}

/// Per-client rate limits of one endpoint (`HFS_RATE_LIMIT_*`), per process.
/// `0` = unlimited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct RateLimitSettings {
    /// Requests / minute / client IP without credentials.
    pub anonymous_per_minute: u32,
    /// Requests / minute / OAuth client.
    pub authenticated_per_minute: u32,
    /// Token-bucket burst, seconds of budget (`0` = server default 10).
    pub burst_seconds: u32,
}

/// What a signed-in console user may do on this endpoint's FHIR API through
/// the console session cookie (see `console::ConsoleRole`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ConsoleAccess {
    /// The console session is not a credential on this endpoint: the data
    /// browser shows only what an anonymous caller (or a pasted token) gets.
    Off,
    /// Every console role reads; nobody writes through the console.
    ReadOnly,
    /// Viewer reads, editor and admin read and write.
    #[default]
    RoleBased,
}

/// The mutable part of an endpoint: what an administrator edits, the admin
/// API accepts and `hfs_endpoints.spec` stores.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointSpec {
    /// Immutable identifier (must equal the stored one on update).
    pub name: EndpointName,
    /// URL segment under the FHIR base; root only for the default endpoint.
    pub path: EndpointPath,
    /// Human name for the console.
    pub display_name: String,
    /// Free text.
    #[serde(default)]
    pub description: Option<String>,
    /// Default FHIR release.
    #[serde(default)]
    pub fhir_version: FhirRelease,
    /// Where the data lives. Immutable after creation in v1.
    pub storage: StorageBinding,
    /// Lifecycle state.
    #[serde(default)]
    pub status: EndpointStatus,
    /// `None` = inherit `HFS_AUTH_*` / `HFS_SMART_*`.
    #[serde(default)]
    pub auth: Option<EndpointAuth>,
    /// `None` = inherit `HFS_VALIDATION_*`.
    #[serde(default)]
    pub validation: Option<ValidationSettings>,
    /// `None` = inherit `HFS_TERMINOLOGY_*`.
    #[serde(default)]
    pub terminology: Option<TerminologyBinding>,
    /// `None` = inherit `HFS_CORS_*`.
    #[serde(default)]
    pub cors: Option<CorsSettings>,
    /// `None` = inherit `HFS_RATE_LIMIT_*`.
    #[serde(default)]
    pub rate_limit: Option<RateLimitSettings>,
    /// Console session as a credential on this endpoint.
    #[serde(default)]
    pub console_access: ConsoleAccess,
    /// Default endpoint only: whether `X-Tenant-ID` may still select a tenant
    /// that no named endpoint is bound to (legacy header multi-tenancy). A
    /// tenant bound to a named endpoint is never selectable this way.
    #[serde(default)]
    pub legacy_tenant_header: bool,
    /// Who owns the configuration.
    #[serde(default)]
    pub managed_by: ManagedBy,
    /// Free-form labels (`env=test`, `owner=lab`), shown and filterable in
    /// the console.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

impl EndpointSpec {
    /// The spec of the default endpoint as the upgrade creates it: root
    /// path, the server's default tenant and FHIR version, every policy
    /// section inherited from the environment — so behaviour is unchanged.
    pub fn default_endpoint(default_tenant: &str, fhir_version: FhirRelease) -> Self {
        Self {
            name: EndpointName::default_endpoint(),
            path: EndpointPath::root(),
            display_name: "Default".to_string(),
            description: None,
            fhir_version,
            storage: StorageBinding::Tenant {
                tenant_id: default_tenant.to_string(),
            },
            status: EndpointStatus::Enabled,
            auth: None,
            validation: None,
            terminology: None,
            cors: None,
            rate_limit: None,
            console_access: ConsoleAccess::RoleBased,
            legacy_tenant_header: true,
            managed_by: ManagedBy::Ui,
            labels: BTreeMap::new(),
        }
    }

    /// A new named endpoint with its own tenant (= its name) and every
    /// section inherited.
    pub fn named(name: EndpointName, display_name: impl Into<String>) -> Self {
        let path = EndpointPath::parse(name.as_str()).expect("a valid name is a valid path");
        let tenant_id = name.as_str().to_string();
        Self {
            name,
            path,
            display_name: display_name.into(),
            description: None,
            fhir_version: FhirRelease::R4,
            storage: StorageBinding::Tenant { tenant_id },
            status: EndpointStatus::Enabled,
            auth: None,
            validation: None,
            terminology: None,
            cors: None,
            rate_limit: None,
            console_access: ConsoleAccess::RoleBased,
            legacy_tenant_header: false,
            managed_by: ManagedBy::Ui,
            labels: BTreeMap::new(),
        }
    }

    /// Whether this is the default endpoint.
    pub fn is_default(&self) -> bool {
        self.name.is_default()
    }

    /// Structural checks that need no other endpoint (uniqueness is the
    /// store's job). Returns every problem, not just the first.
    pub fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errs = ValidationErrors::default();
        if self.is_default() != self.path.is_root() {
            errs.push(
                "path",
                "the default endpoint and only it has the root (empty) path",
            );
        }
        let display = self.display_name.trim();
        if display.is_empty() || display.chars().count() > 120 {
            errs.push("displayName", "must be 1-120 characters");
        }
        let tenant = self.storage.tenant_id();
        if tenant.is_empty()
            || tenant.len() > 64
            || !tenant
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            || tenant.starts_with('_')
        {
            errs.push(
                "storage.tenantId",
                "must be 1-64 characters of A-Z a-z 0-9 - _ . and not start with '_'",
            );
        }
        if !self.is_default() && self.legacy_tenant_header {
            errs.push(
                "legacyTenantHeader",
                "only the default endpoint may accept X-Tenant-ID",
            );
        }
        if let Some(auth) = &self.auth {
            if let AuthProviderRef::Internal { realm } = &auth.provider
                && let Err(e) = crate::name::check_segment_grammar(realm)
            {
                errs.push("auth.provider.realm", e.to_string());
            }
            if auth.mode != AuthMode::Off && auth.provider == AuthProviderRef::External {
                match auth.issuer.as_deref().map(str::trim) {
                    Some(i) if !i.is_empty() => check_http_url(&mut errs, "auth.issuer", i),
                    _ => errs.push("auth.issuer", "required when auth is on"),
                }
                match auth.jwks_url.as_deref().map(str::trim) {
                    Some(u) if !u.is_empty() => check_http_url(&mut errs, "auth.jwksUrl", u),
                    _ => errs.push("auth.jwksUrl", "required when auth is on"),
                }
            }
            if auth.mode != AuthMode::Off
                && !(auth.scopes.allow_patient
                    || auth.scopes.allow_user
                    || auth.scopes.allow_system)
            {
                errs.push("auth.scopes", "at least one scope context must be allowed");
            }
            for a in &auth.algorithms {
                if !matches!(
                    a.as_str(),
                    "RS256" | "RS384" | "RS512" | "ES256" | "ES384" | "PS256" | "PS384" | "PS512"
                ) {
                    errs.push("auth.algorithms", format!("unsupported algorithm {a}"));
                }
            }
        }
        if let Some(t) = &self.terminology {
            if let Some(u) = &t.server_url {
                check_http_url(&mut errs, "terminology.serverUrl", u);
            }
            if let Some(u) = &t.public_url {
                check_http_url(&mut errs, "terminology.publicUrl", u);
            }
        }
        if let Some(cors) = &self.cors {
            for o in &cors.allowed_origins {
                if o == "*" {
                    if cors.allow_credentials {
                        errs.push(
                            "cors.allowCredentials",
                            "cannot be combined with origin '*'",
                        );
                    }
                    continue;
                }
                match url::Url::parse(o) {
                    Ok(u)
                        if matches!(u.scheme(), "http" | "https")
                            && u.host_str().is_some()
                            && u.path() == "/"
                            && !o.ends_with('/') => {}
                    _ => errs.push(
                        "cors.allowedOrigins",
                        format!("{o:?} is not an origin (scheme://host[:port])"),
                    ),
                }
            }
        }
        if let Some(v) = &self.validation {
            for r in &v.required_profiles {
                if r.resource_type.is_empty()
                    || !r
                        .resource_type
                        .starts_with(|c: char| c.is_ascii_uppercase())
                {
                    errs.push(
                        "validation.requiredProfiles",
                        "resourceType must be a type name",
                    );
                }
                if url::Url::parse(r.profile.split('|').next().unwrap_or("")).is_err() {
                    errs.push(
                        "validation.requiredProfiles",
                        "profile must be a canonical URL",
                    );
                }
            }
        }
        errs.into_result()
    }
}

fn check_http_url(errs: &mut ValidationErrors, field: &str, value: &str) {
    match url::Url::parse(value) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => {
            if !u.username().is_empty() || u.password().is_some() {
                errs.push(field, "must not contain credentials");
            }
        }
        _ => errs.push(field, "must be an absolute http(s) URL"),
    }
}

/// A stored endpoint: the spec plus bookkeeping the server maintains.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    /// Stable id (UUID v4), never reused; AAD of the endpoint's secrets.
    pub id: String,
    /// Optimistic-concurrency revision, `1` on create, `+1` per change.
    pub revision: u64,
    /// The configuration.
    #[serde(flatten)]
    pub spec: EndpointSpec,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last change.
    pub updated_at: DateTime<Utc>,
    /// Actor subject of the creation.
    #[serde(default)]
    pub created_by: Option<String>,
    /// Actor subject of the last change.
    #[serde(default)]
    pub updated_by: Option<String>,
}

impl Endpoint {
    /// Name shortcut.
    pub fn name(&self) -> &str {
        self.spec.name.as_str()
    }

    /// Tenant shortcut.
    pub fn tenant_id(&self) -> &str {
        self.spec.storage.tenant_id()
    }

    /// Server-relative prefix (`""` or `"/{path}"`).
    pub fn prefix(&self) -> String {
        self.spec.path.prefix()
    }

    /// Public base URL given the server's `HFS_BASE_URL`.
    pub fn public_base(&self, server_base: &str) -> String {
        format!("{}{}", server_base.trim_end_matches('/'), self.prefix())
    }

    /// JSON for API responses: every sealed secret replaced by
    /// `{"set": true}` (absent secrets stay `null`).
    pub fn redacted(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).expect("endpoint serializes");
        crate::secret::redact_sealed(&mut v);
        v
    }
}

/// Field-level validation problems, as the admin API reports them
/// (`422` + OperationOutcome with one issue per entry, `expression` = field).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("invalid endpoint: {}", .0.iter().map(|e| format!("{}: {}", e.field, e.message)).collect::<Vec<_>>().join("; "))]
pub struct ValidationErrors(pub Vec<FieldError>);

/// One validation problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldError {
    /// JSON field path in camelCase, e.g. `auth.issuer`.
    pub field: String,
    /// What is wrong.
    pub message: String,
}

impl ValidationErrors {
    /// Records a problem.
    pub fn push(&mut self, field: &str, message: impl Into<String>) {
        self.0.push(FieldError {
            field: field.to_string(),
            message: message.into(),
        });
    }

    /// `Ok` when empty.
    pub fn into_result(self) -> Result<(), ValidationErrors> {
        if self.0.is_empty() { Ok(()) } else { Err(self) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_endpoint_inherits_everything() {
        let spec = EndpointSpec::default_endpoint("default", FhirRelease::R4);
        assert!(spec.validate().is_ok());
        assert!(spec.auth.is_none() && spec.validation.is_none() && spec.cors.is_none());
        assert!(spec.path.is_root());
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["path"], "");
        assert_eq!(json["storage"]["kind"], "tenant");
        assert_eq!(json["storage"]["tenantId"], "default");
        assert_eq!(json["auth"], serde_json::Value::Null);
    }

    #[test]
    fn named_endpoint_roundtrip() {
        let mut spec = EndpointSpec::named(EndpointName::parse("twcore").unwrap(), "TW Core");
        spec.auth = Some(EndpointAuth {
            mode: AuthMode::Full,
            issuer: Some("https://idp.example.org/realms/fhir".into()),
            jwks_url: Some("https://idp.example.org/realms/fhir/certs".into()),
            audience: Some("twcore-api".into()),
            ..EndpointAuth::default()
        });
        spec.validation = Some(ValidationSettings {
            mode: ValidationMode::Enforce,
            ..Default::default()
        });
        assert!(spec.validate().is_ok());
        let s = serde_json::to_string(&spec).unwrap();
        let back: EndpointSpec = serde_json::from_str(&s).unwrap();
        assert_eq!(back, spec);
        assert!(s.contains("\"mode\":\"full\""));
        assert!(s.contains("\"mode\":\"enforce\""));
    }

    #[test]
    fn minimal_json_uses_defaults() {
        let spec: EndpointSpec = serde_json::from_str(
            r#"{"name":"lab","path":"lab","displayName":"Lab",
                "storage":{"kind":"tenant","tenantId":"lab"}}"#,
        )
        .unwrap();
        assert_eq!(spec.status, EndpointStatus::Enabled);
        assert_eq!(spec.fhir_version, FhirRelease::R4);
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn validation_reports_every_problem() {
        let mut spec = EndpointSpec::named(EndpointName::parse("lab").unwrap(), " ");
        spec.legacy_tenant_header = true;
        spec.auth = Some(EndpointAuth {
            mode: AuthMode::WriteProtect,
            issuer: Some("not a url".into()),
            ..Default::default()
        });
        spec.cors = Some(CorsSettings {
            allowed_origins: vec!["*".into(), "https://a.example/path".into()],
            allow_credentials: true,
            ..Default::default()
        });
        let errs = spec.validate().unwrap_err();
        let fields: Vec<_> = errs.0.iter().map(|e| e.field.as_str()).collect();
        for f in [
            "displayName",
            "legacyTenantHeader",
            "auth.issuer",
            "auth.jwksUrl",
            "cors.allowCredentials",
            "cors.allowedOrigins",
        ] {
            assert!(fields.contains(&f), "missing {f} in {fields:?}");
        }
    }

    #[test]
    fn internal_provider_needs_a_realm_not_an_issuer() {
        let mut spec = EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab");
        spec.auth = Some(EndpointAuth {
            mode: AuthMode::Full,
            provider: AuthProviderRef::Internal {
                realm: "clinic".into(),
            },
            ..Default::default()
        });
        assert!(spec.validate().is_ok(), "{:?}", spec.validate());
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["auth"]["provider"]["kind"], "internal");
        assert_eq!(json["auth"]["provider"]["realm"], "clinic");
        let back: EndpointSpec = serde_json::from_value(json).unwrap();
        assert_eq!(back, spec);

        spec.auth.as_mut().unwrap().provider = AuthProviderRef::Internal {
            realm: "Bad Realm!".into(),
        };
        let errs = spec.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.field == "auth.provider.realm"));
    }

    #[test]
    fn rows_without_a_provider_stay_external() {
        let auth: EndpointAuth = serde_json::from_value(serde_json::json!({
            "mode": "full", "issuer": "https://idp", "jwksUrl": "https://idp/certs"
        }))
        .unwrap();
        assert_eq!(auth.provider, AuthProviderRef::External);
        assert_eq!(auth.provider.internal_realm(), None);
    }

    #[test]
    fn root_path_only_for_default() {
        let mut spec = EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab");
        spec.path = EndpointPath::root();
        assert!(spec.validate().is_err());
        let mut d = EndpointSpec::default_endpoint("default", FhirRelease::R4);
        d.path = EndpointPath::parse("dflt").unwrap();
        assert!(d.validate().is_err());
    }
}
