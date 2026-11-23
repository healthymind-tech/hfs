//! Form bodies of the manager pages → [`EndpointSpec`] changes.
//!
//! Each configuration page posts one section; [`apply_section`] applies it to
//! the stored spec, so saving Security can never touch Validation. Parse
//! problems become field errors keyed by the same JSON paths the admin API
//! reports (`auth.clockSkewSecs`), so client-side, parse and store errors all
//! land on the same input.

use std::collections::BTreeMap;

use helios_endpoints::model::{NonCompartmentPolicy, ProfileRequirement, TerminologyValidation};
use helios_endpoints::sync::{DuplicateData, DuplicateRequest};
use helios_endpoints::{
    AuthMode, ConsoleAccess, CorsSettings, EndpointAuth, EndpointName, EndpointPath, EndpointSpec,
    FhirRelease, RateLimitSettings, StorageBinding, TerminologyBinding, ValidationMode,
    ValidationSettings,
};

use super::backend::{SecretChange, SecretChanges};

/// Field errors by JSON path (`""` = form level).
pub type FieldErrors = BTreeMap<String, String>;

/// One problem of a failed save, as the error summary shows it (UX-21):
/// the field's label (never its key), the message, and a link to the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorLink {
    /// Translated field label ("" for a form-level error).
    pub label: String,
    /// Translated message.
    pub message: String,
    /// `#f-…` of the input.
    pub href: String,
    /// Id of the input (`f-…`).
    pub id: String,
    /// Wizard step (0-based) holding the field.
    pub step: Option<usize>,
}

/// Where a field path (`auth.issuer`) lives in the forms: its label key,
/// the input id and the wizard step.
pub fn field_ref(path: &str) -> Option<(&'static str, &'static str, usize)> {
    Some(match path {
        "displayName" => ("mgr-field-display-name", "f-display_name", 0),
        "name" => ("mgr-field-name", "f-name", 0),
        "path" => ("mgr-field-path", "f-path", 0),
        "storage.tenantId" => ("mgr-field-tenant", "f-tenant", 1),
        "fhirVersion" => ("mgr-field-fhir-version", "f-fhir_version", 1),
        "auth.mode" => ("mgr-field-auth-mode", "f-auth_mode", 2),
        "auth.issuer" => ("mgr-field-issuer", "f-auth_issuer", 2),
        "auth.jwksUrl" => ("mgr-field-jwks", "f-auth_jwks_url", 2),
        "auth.audience" => ("mgr-field-audience", "f-auth_audience", 2),
        "auth.algorithms" => ("mgr-field-algorithms", "f-auth_algorithms", 2),
        "auth.scopes" => ("mgr-field-scopes", "f-scopes_allow_patient", 2),
        "auth.clockSkewSecs" => ("mgr-field-clock-skew", "f-auth_clock_skew", 2),
        "auth.jwksMaxAgeSecs" => ("mgr-field-jwks-max-age", "f-auth_jwks_max_age", 2),
        "validation.requiredProfiles" => ("mgr-required-profiles", "f-val_required", 3),
        "terminology.serverUrl" => ("mgr-term-server", "f-term_server_url", 3),
        "terminology.publicUrl" => ("mgr-term-public", "f-term_public_url", 3),
        "terminology.timeoutSecs" => ("mgr-term-timeout", "f-term_timeout", 3),
        "terminology.token" => ("mgr-term-token", "f-term_token", 3),
        "cors.allowedOrigins" => ("mgr-cors-origins", "f-cors_origins", 2),
        "cors.allowCredentials" => ("mgr-cors-credentials", "f-cors_credentials", 2),
        _ => return None,
    })
}

/// A validation message from the endpoint model, in the request's language
/// (the model speaks English; the few messages it has are mapped here).
pub fn error_message(i18n: &crate::I18n, message: &str) -> String {
    let key = match message {
        "required when auth is on" => "mgr-err-required-auth",
        "must be an absolute http(s) URL" => "mgr-err-url",
        "must not contain credentials" => "mgr-err-url-credentials",
        "at least one scope context must be allowed" => "mgr-err-scopes",
        "must be 1-120 characters" => "mgr-err-display-name",
        "required" => "mgr-err-required",
        _ => {
            if let Some(alg) = message.strip_prefix("unsupported algorithm ") {
                return i18n.t_arg("mgr-err-algorithm", "alg", alg.to_string());
            }
            return message.to_string();
        }
    };
    i18n.t(key)
}

/// The error summary of a failed save.
pub fn error_links(i18n: &crate::I18n, errors: &FieldErrors) -> Vec<ErrorLink> {
    errors
        .iter()
        .map(|(path, message)| {
            let message = error_message(i18n, message);
            match field_ref(path) {
                Some((label, id, step)) => ErrorLink {
                    label: i18n.t(label),
                    message,
                    href: format!("#{id}"),
                    id: id.to_string(),
                    step: Some(step),
                },
                None => ErrorLink {
                    label: String::new(),
                    message,
                    href: String::new(),
                    id: String::new(),
                    step: None,
                },
            }
        })
        .collect()
}

/// A parsed `application/x-www-form-urlencoded` body.
#[derive(Debug, Default, Clone)]
pub struct Form {
    pairs: Vec<(String, String)>,
}

impl Form {
    /// Parses a body.
    pub fn parse(body: &[u8]) -> Self {
        Self {
            pairs: form_urlencoded::parse(body)
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
        }
    }

    /// Last value of `name`, trimmed; `""` when absent.
    pub fn get(&self, name: &str) -> &str {
        self.pairs
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim())
            .unwrap_or("")
    }

    /// Optional non-empty value.
    pub fn opt(&self, name: &str) -> Option<String> {
        let v = self.get(name);
        (!v.is_empty()).then(|| v.to_string())
    }

    /// Checkbox state (present with any value but `off`/`false`).
    pub fn checked(&self, name: &str) -> bool {
        self.pairs
            .iter()
            .any(|(k, v)| k == name && !matches!(v.as_str(), "off" | "false" | "0"))
    }

    /// Replaces (or adds) the value of `name`.
    pub fn set(&mut self, name: &str, value: &str) {
        self.pairs.retain(|(k, _)| k != name);
        self.pairs.push((name.to_string(), value.to_string()));
    }

    /// Whether the field was sent at all.
    pub fn has(&self, name: &str) -> bool {
        self.pairs.iter().any(|(k, _)| k == name)
    }

    /// Non-empty lines (or comma-separated items) of a text area.
    pub fn list(&self, name: &str) -> Vec<String> {
        self.get(name)
            .split(['\n', ','])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// The `revision` hidden field.
    pub fn revision(&self) -> Option<u64> {
        self.get("revision").parse().ok()
    }

    fn u32_opt(&self, name: &str, path: &str, errs: &mut FieldErrors) -> Option<u32> {
        let v = self.get(name);
        if v.is_empty() {
            return None;
        }
        match v.parse::<u32>() {
            Ok(n) => Some(n),
            Err(_) => {
                errs.insert(path.into(), "must be a whole number ≥ 0".into());
                None
            }
        }
    }
}

/// The configuration sections a page can post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// Name, path, version, labels, console access.
    General,
    /// FHIR auth, CORS, rate limits.
    Security,
    /// Validation.
    Validation,
    /// Terminology binding.
    Terminology,
}

impl Section {
    /// From the page slug.
    pub fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "config" => Some(Section::General),
            "security" => Some(Section::Security),
            "validation" => Some(Section::Validation),
            "terminology" => Some(Section::Terminology),
            _ => None,
        }
    }

    /// Page slug.
    pub fn slug(self) -> &'static str {
        match self {
            Section::General => "config",
            Section::Security => "security",
            Section::Validation => "validation",
            Section::Terminology => "terminology",
        }
    }
}

/// Parses a FHIR release (`R4`…).
pub fn release(s: &str) -> Option<FhirRelease> {
    match s {
        "R4" => Some(FhirRelease::R4),
        "R4B" => Some(FhirRelease::R4B),
        "R5" => Some(FhirRelease::R5),
        "R6" => Some(FhirRelease::R6),
        _ => None,
    }
}

/// Display label of a FHIR release.
pub fn release_label(r: FhirRelease) -> &'static str {
    match r {
        FhirRelease::R4 => "R4",
        FhirRelease::R4B => "R4B",
        FhirRelease::R5 => "R5",
        FhirRelease::R6 => "R6",
    }
}

fn auth_mode(s: &str) -> Option<AuthMode> {
    match s {
        "off" => Some(AuthMode::Off),
        "write-protect" => Some(AuthMode::WriteProtect),
        "full" => Some(AuthMode::Full),
        _ => None,
    }
}

fn validation_mode(s: &str) -> Option<ValidationMode> {
    match s {
        "off" => Some(ValidationMode::Off),
        "log" => Some(ValidationMode::Log),
        "enforce" => Some(ValidationMode::Enforce),
        _ => None,
    }
}

fn terminology_validation(s: &str) -> Option<TerminologyValidation> {
    match s {
        "off" => Some(TerminologyValidation::Off),
        "embedded" => Some(TerminologyValidation::Embedded),
        "remote" => Some(TerminologyValidation::Remote),
        "remote-only" => Some(TerminologyValidation::RemoteOnly),
        _ => None,
    }
}

fn console_access(s: &str) -> Option<ConsoleAccess> {
    match s {
        "off" => Some(ConsoleAccess::Off),
        "read-only" => Some(ConsoleAccess::ReadOnly),
        "role-based" => Some(ConsoleAccess::RoleBased),
        _ => None,
    }
}

/// `key=value` lines → labels.
fn labels(form: &Form, errs: &mut FieldErrors) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in form
        .get("labels")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
    {
        match line.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() => {
                out.insert(k.trim().to_string(), v.trim().to_string());
            }
            _ => {
                errs.insert("labels".into(), format!("{line:?} is not key=value"));
            }
        }
    }
    out
}

/// The auth section from a form (`auth_*` fields).
pub fn parse_auth(
    form: &Form,
    current: Option<&EndpointAuth>,
    errs: &mut FieldErrors,
) -> EndpointAuth {
    let mut auth = current.cloned().unwrap_or_default();
    match auth_mode(form.get("auth_mode")) {
        Some(m) => auth.mode = m,
        None => {
            errs.insert("auth.mode".into(), "choose a mode".into());
        }
    }
    auth.issuer = form.opt("auth_issuer");
    auth.jwks_url = form.opt("auth_jwks_url");
    auth.audience = form.opt("auth_audience");
    auth.algorithms = form.list("auth_algorithms");
    auth.clock_skew_secs = form.u32_opt("auth_clock_skew", "auth.clockSkewSecs", errs);
    auth.jwks_max_age_secs = form.u32_opt("auth_jwks_max_age", "auth.jwksMaxAgeSecs", errs);
    auth.scopes.accept_v1 = form.checked("scopes_accept_v1");
    auth.scopes.allow_patient = form.checked("scopes_allow_patient");
    auth.scopes.allow_user = form.checked("scopes_allow_user");
    auth.scopes.allow_system = form.checked("scopes_allow_system");
    auth.scopes.patient_non_compartment = if form.get("scopes_non_compartment") == "deny" {
        NonCompartmentPolicy::Deny
    } else {
        NonCompartmentPolicy::Allow
    };
    auth.smart.discover_from_issuer = form.checked("smart_discover");
    auth.smart.authorization_endpoint = form.opt("smart_authorization_endpoint");
    auth.smart.token_endpoint = form.opt("smart_token_endpoint");
    auth.smart.jwks_uri = form.opt("smart_jwks_uri");
    auth.accept_admin_credential = form.checked("accept_admin");
    auth
}

/// The validation section from a form (`val_*` fields).
pub fn parse_validation(form: &Form, errs: &mut FieldErrors) -> ValidationSettings {
    let mut v = ValidationSettings::default();
    match validation_mode(form.get("val_mode")) {
        Some(m) => v.mode = m,
        None => {
            errs.insert("validation.mode".into(), "choose a mode".into());
        }
    }
    match terminology_validation(form.get("val_terminology")) {
        Some(t) => v.terminology = t,
        None => {
            errs.insert("validation.terminology".into(), "choose a mode".into());
        }
    }
    v.stored_profiles = form.checked("val_stored_profiles");
    for line in form
        .get("val_required")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
    {
        match line.split_once(['|', ' ']) {
            Some((t, p)) if !t.trim().is_empty() && !p.trim().is_empty() => {
                v.required_profiles.push(ProfileRequirement {
                    resource_type: t.trim().to_string(),
                    profile: p.trim().to_string(),
                })
            }
            _ => {
                errs.insert(
                    "validation.requiredProfiles".into(),
                    format!("{line:?}: write one 'ResourceType canonical-url' per line"),
                );
            }
        }
    }
    v
}

/// The CORS section (`cors_*`).
pub fn parse_cors(form: &Form) -> CorsSettings {
    CorsSettings {
        enabled: form.checked("cors_enabled"),
        allowed_origins: form.list("cors_origins"),
        allow_credentials: form.checked("cors_credentials"),
        extra_headers: form.list("cors_headers"),
    }
}

/// The rate-limit section (`rl_*`).
pub fn parse_rate_limit(form: &Form, errs: &mut FieldErrors) -> RateLimitSettings {
    RateLimitSettings {
        anonymous_per_minute: form
            .u32_opt("rl_anonymous", "rateLimit.anonymousPerMinute", errs)
            .unwrap_or(0),
        authenticated_per_minute: form
            .u32_opt("rl_authenticated", "rateLimit.authenticatedPerMinute", errs)
            .unwrap_or(0),
        burst_seconds: form
            .u32_opt("rl_burst", "rateLimit.burstSeconds", errs)
            .unwrap_or(0),
    }
}

/// The terminology section (`term_*`) and its secret change.
pub fn parse_terminology(
    form: &Form,
    current: Option<&TerminologyBinding>,
    errs: &mut FieldErrors,
    secrets: &mut SecretChanges,
) -> TerminologyBinding {
    let mut t = TerminologyBinding {
        server_url: form.opt("term_server_url"),
        public_url: form.opt("term_public_url"),
        timeout_secs: form.u32_opt("term_timeout", "terminology.timeoutSecs", errs),
        token: current.and_then(|c| c.token.clone()),
    };
    if form.checked("term_token_clear") {
        t.token = None;
        secrets.insert("terminology.token".into(), SecretChange::Clear);
    } else if let Some(token) = form.opt("term_token") {
        secrets.insert("terminology.token".into(), SecretChange::Set(token));
    }
    t
}

/// Applies the posted `section` to `spec`. Returns the secret changes.
pub fn apply_section(
    section: Section,
    form: &Form,
    spec: &mut EndpointSpec,
    errs: &mut FieldErrors,
) -> SecretChanges {
    let mut secrets = SecretChanges::new();
    match section {
        Section::General => {
            spec.display_name = form.get("display_name").to_string();
            spec.description = form.opt("description");
            if form.has("path") && !spec.is_default() {
                match EndpointPath::parse(form.get("path")) {
                    Ok(p) if !p.is_root() => spec.path = p,
                    Ok(_) => {
                        errs.insert("path".into(), "a named endpoint needs a path".into());
                    }
                    Err(e) => {
                        errs.insert("path".into(), e.to_string());
                    }
                }
            }
            if form.has("fhir_version") {
                match release(form.get("fhir_version")) {
                    Some(r) => spec.fhir_version = r,
                    None => {
                        errs.insert("fhirVersion".into(), "unknown FHIR release".into());
                    }
                }
            }
            if let Some(a) = console_access(form.get("console_access")) {
                spec.console_access = a;
            }
            if spec.is_default() {
                spec.legacy_tenant_header = form.checked("legacy_tenant_header");
            }
            spec.labels = labels(form, errs);
        }
        Section::Security => {
            spec.auth = if form.checked("auth_inherit") {
                None
            } else {
                Some(parse_auth(form, spec.auth.as_ref(), errs))
            };
            spec.cors = if form.checked("cors_inherit") {
                None
            } else {
                Some(parse_cors(form))
            };
            spec.rate_limit = if form.checked("rl_inherit") {
                None
            } else {
                Some(parse_rate_limit(form, errs))
            };
        }
        Section::Validation => {
            spec.validation = if form.checked("val_inherit") {
                None
            } else {
                Some(parse_validation(form, errs))
            };
        }
        Section::Terminology => {
            spec.terminology = if form.checked("term_inherit") {
                None
            } else {
                Some(parse_terminology(
                    form,
                    spec.terminology.as_ref(),
                    errs,
                    &mut secrets,
                ))
            };
        }
    }
    secrets
}

/// What the new-endpoint form asks for.
#[derive(Debug, Clone)]
pub enum NewEndpoint {
    /// Create (empty or adopting an existing tenant).
    Create(Box<EndpointSpec>, SecretChanges),
    /// Duplicate an existing endpoint.
    Duplicate {
        /// Source endpoint.
        source: String,
        /// Request.
        request: DuplicateRequest,
    },
}

/// Parses the new-endpoint form. Every problem is reported, not just the
/// first.
pub fn parse_new(form: &Form) -> Result<NewEndpoint, FieldErrors> {
    let mut errs = FieldErrors::new();
    let display = form.get("display_name").to_string();
    if display.is_empty() {
        errs.insert("displayName".into(), "required".into());
    }
    let name_raw = form.get("name");
    let name = match EndpointName::parse(name_raw) {
        Ok(n) if n.is_default() => {
            errs.insert(
                "name".into(),
                "\"default\" is the default endpoint's name".into(),
            );
            None
        }
        Ok(n) => Some(n),
        Err(e) => {
            errs.insert("name".into(), e.to_string());
            None
        }
    };
    let path_raw = form.get("path");
    let path = if path_raw.is_empty() {
        name.as_ref()
            .and_then(|n| EndpointPath::parse(n.as_str()).ok())
    } else {
        match EndpointPath::parse(path_raw) {
            Ok(p) => Some(p),
            Err(e) => {
                errs.insert("path".into(), e.to_string());
                None
            }
        }
    };
    let version = release(form.get("fhir_version")).unwrap_or_default();
    if form.get("data_source") == "duplicate" {
        let source = form.get("duplicate_from").to_string();
        if source.is_empty() {
            errs.insert("duplicateFrom".into(), "choose the endpoint to copy".into());
        }
        if !errs.is_empty() {
            return Err(errs);
        }
        let data = match form.get("duplicate_data") {
            "current" => DuplicateData::Current,
            "history" => DuplicateData::History,
            _ => DuplicateData::None,
        };
        return Ok(NewEndpoint::Duplicate {
            source,
            request: DuplicateRequest {
                name: name_raw.to_string(),
                path: (!path_raw.is_empty()).then(|| path_raw.to_string()),
                display_name: Some(display),
                data,
                packages: form.checked("duplicate_packages"),
                auth: form.checked("duplicate_auth"),
                secrets: false,
                keep_in_sync: form.checked("duplicate_keep_in_sync"),
            },
        });
    }
    let (Some(name), Some(path)) = (name, path) else {
        return Err(errs);
    };
    let mut spec = EndpointSpec::named(name, display);
    spec.path = path;
    spec.fhir_version = version;
    spec.description = form.opt("description");
    if form.get("data_source") == "adopt" {
        match form.opt("adopt_tenant") {
            Some(t) => {
                spec.storage = StorageBinding::Tenant { tenant_id: t };
            }
            None => {
                errs.insert(
                    "storage.tenantId".into(),
                    "choose the tenant to adopt".into(),
                );
            }
        }
    }
    spec.labels = labels(form, &mut errs);
    let mut secrets = SecretChanges::new();
    if form.get("auth_choice") != "inherit" && form.has("auth_choice") {
        spec.auth = Some(parse_auth(form, None, &mut errs));
    }
    if form.get("val_choice") != "inherit" && form.has("val_choice") {
        spec.validation = Some(parse_validation(form, &mut errs));
    }
    if form.get("term_choice") == "custom" {
        spec.terminology = Some(parse_terminology(form, None, &mut errs, &mut secrets));
    }
    if let Err(v) = spec.validate() {
        for e in v.0 {
            errs.entry(e.field).or_insert(e.message);
        }
    }
    if errs.is_empty() {
        Ok(NewEndpoint::Create(Box::new(spec), secrets))
    } else {
        Err(errs)
    }
}

/// The section a warning of [`warnings`] belongs to.
pub fn warning_section(key: &str) -> Section {
    match key {
        "mgr-info-enforce" => Section::Validation,
        _ => Section::Security,
    }
}

/// Whether `url`'s host is in `HFS_OUTBOUND_ALLOW` (plain http is then
/// fetched on purpose, so warning about it on every page is noise).
fn outbound_allowed(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    std::env::var("HFS_OUTBOUND_ALLOW")
        .ok()
        .is_some_and(|v| v.split(',').any(|h| h.trim().eq_ignore_ascii_case(host)))
}

/// Non-blocking warnings about a spec (design §6 "Warnings surfaced by the
/// console"), as Fluent keys.
pub fn warnings(spec: &EndpointSpec, others: &[&EndpointSpec]) -> Vec<&'static str> {
    let mut out = Vec::new();
    if let Some(auth) = &spec.auth
        && auth.mode != AuthMode::Off
    {
        if auth.audience.as_deref().is_none_or(str::is_empty) {
            out.push("mgr-warn-no-audience");
        }
        if others.iter().any(|o| {
            o.name != spec.name
                && o.auth.as_ref().is_some_and(|a| {
                    a.mode != AuthMode::Off
                        && a.issuer == auth.issuer
                        && a.audience == auth.audience
                })
        }) {
            out.push("mgr-warn-shared-audience");
        }
        if auth
            .jwks_url
            .as_deref()
            .is_some_and(|u| u.starts_with("http://") && !outbound_allowed(u))
        {
            out.push("mgr-warn-http-jwks");
        }
        if auth.mode == AuthMode::WriteProtect && auth.accept_admin_credential {
            out.push("mgr-info-admin-credential");
        }
    }
    if spec
        .validation
        .as_ref()
        .is_some_and(|v| v.mode == ValidationMode::Enforce)
    {
        out.push("mgr-info-enforce");
    }
    if spec
        .cors
        .as_ref()
        .is_some_and(|c| c.enabled && c.allowed_origins.iter().any(|o| o == "*"))
    {
        out.push("mgr-info-cors-any");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(s: &str) -> Form {
        Form::parse(s.as_bytes())
    }

    fn base() -> EndpointSpec {
        EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab")
    }

    #[test]
    fn security_section_inherit_and_override() {
        let mut spec = base();
        let mut errs = FieldErrors::new();
        apply_section(
            Section::Security,
            &form("auth_inherit=on&cors_inherit=on&rl_inherit=on"),
            &mut spec,
            &mut errs,
        );
        assert!(spec.auth.is_none() && spec.cors.is_none() && spec.rate_limit.is_none());
        let f = form(
            "auth_mode=full&auth_issuer=https%3A%2F%2Fidp.example%2Fr&auth_jwks_url=https%3A%2F%2Fidp.example%2Fc\
             &auth_audience=lab-api&scopes_allow_patient=on&scopes_allow_system=on&smart_discover=on\
             &auth_clock_skew=abc&cors_enabled=on&cors_origins=https%3A%2F%2Fapp.example%0A&rl_anonymous=60",
        );
        apply_section(Section::Security, &f, &mut spec, &mut errs);
        let auth = spec.auth.as_ref().unwrap();
        assert_eq!(auth.mode, AuthMode::Full);
        assert_eq!(auth.audience.as_deref(), Some("lab-api"));
        assert!(auth.scopes.allow_patient && !auth.scopes.allow_user && auth.scopes.allow_system);
        assert!(!auth.accept_admin_credential);
        assert_eq!(
            errs.get("auth.clockSkewSecs").map(String::as_str),
            Some("must be a whole number ≥ 0")
        );
        assert_eq!(
            spec.cors.as_ref().unwrap().allowed_origins,
            vec!["https://app.example"]
        );
        assert_eq!(spec.rate_limit.as_ref().unwrap().anonymous_per_minute, 60);
    }

    #[test]
    fn general_section_validates_path_and_labels() {
        let mut spec = base();
        let mut errs = FieldErrors::new();
        apply_section(
            Section::General,
            &form("display_name=Lab+2&path=UI&fhir_version=R5&labels=env%3Dtest%0Abad"),
            &mut spec,
            &mut errs,
        );
        assert_eq!(spec.display_name, "Lab 2");
        assert_eq!(spec.fhir_version, FhirRelease::R5);
        assert!(errs.contains_key("path"));
        assert!(errs.contains_key("labels"));
        assert_eq!(spec.labels.get("env").map(String::as_str), Some("test"));
    }

    #[test]
    fn validation_and_terminology_sections() {
        let mut spec = base();
        let mut errs = FieldErrors::new();
        apply_section(
            Section::Validation,
            &form(
                "val_mode=enforce&val_terminology=remote&val_required=Patient+http%3A%2F%2Fx.org%2Fp%0Ajunk",
            ),
            &mut spec,
            &mut errs,
        );
        let v = spec.validation.as_ref().unwrap();
        assert_eq!(v.mode, ValidationMode::Enforce);
        assert_eq!(v.required_profiles.len(), 1);
        assert!(errs.contains_key("validation.requiredProfiles"));
        let secrets = apply_section(
            Section::Terminology,
            &form("term_server_url=http%3A%2F%2Fhts%3A8090&term_token=abc"),
            &mut spec,
            &mut errs,
        );
        assert_eq!(
            secrets.get("terminology.token"),
            Some(&SecretChange::Set("abc".into()))
        );
        let secrets = apply_section(
            Section::Terminology,
            &form("term_token_clear=on"),
            &mut spec,
            &mut errs,
        );
        assert_eq!(secrets.get("terminology.token"), Some(&SecretChange::Clear));
    }

    #[test]
    fn new_endpoint_form() {
        let ok = parse_new(&form(
            "display_name=TW+Core&name=twcore&fhir_version=R4&data_source=empty&auth_choice=inherit&val_choice=custom&val_mode=log&val_terminology=embedded",
        ))
        .unwrap();
        match ok {
            NewEndpoint::Create(spec, _) => {
                assert_eq!(spec.path.as_str(), "twcore");
                assert_eq!(spec.storage.tenant_id(), "twcore");
                assert!(spec.auth.is_none());
                assert_eq!(spec.validation.unwrap().mode, ValidationMode::Log);
            }
            other => panic!("{other:?}"),
        }
        let errs = parse_new(&form("name=UI&display_name=")).unwrap_err();
        assert!(errs.contains_key("name") && errs.contains_key("displayName"));
        let errs = parse_new(&form("name=default&display_name=x")).unwrap_err();
        assert!(errs.contains_key("name"));
        let errs = parse_new(&form("name=lab&display_name=Lab&data_source=adopt")).unwrap_err();
        assert!(errs.contains_key("storage.tenantId"));
        match parse_new(&form(
            "name=copy&display_name=Copy&data_source=duplicate&duplicate_from=default&duplicate_data=current",
        ))
        .unwrap()
        {
            NewEndpoint::Duplicate { source, request } => {
                assert_eq!(source, "default");
                assert_eq!(request.data, DuplicateData::Current);
            }
            other => panic!("{other:?}"),
        }
        // Auth on without an issuer is caught by the model's validation.
        let errs = parse_new(&form(
            "name=lab&display_name=Lab&auth_choice=custom&auth_mode=full&scopes_allow_user=on",
        ))
        .unwrap_err();
        assert!(errs.contains_key("auth.issuer"), "{errs:?}");
    }

    #[test]
    fn warnings_cover_the_design_list() {
        let mut spec = base();
        spec.auth = Some(EndpointAuth {
            mode: AuthMode::WriteProtect,
            issuer: Some("https://i".into()),
            jwks_url: Some("http://k/certs".into()),
            ..Default::default()
        });
        let mut other = EndpointSpec::named(EndpointName::parse("b").unwrap(), "B");
        other.auth = spec.auth.clone();
        let w = warnings(&spec, &[&other]);
        for k in [
            "mgr-warn-no-audience",
            "mgr-warn-shared-audience",
            "mgr-warn-http-jwks",
            "mgr-info-admin-credential",
        ] {
            assert!(w.contains(&k), "{k} in {w:?}");
        }
        assert!(warnings(&base(), &[]).is_empty());
    }
}
