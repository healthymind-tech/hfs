//! What the console's scope builder needs to know about SMART scopes, on
//! top of the grant algebra in [`crate::scope`]:
//!
//! * [`check_syntax`] — registration-time grammar check of one allowed
//!   scope (the same grammar [`crate::scope::parse_scope`] grants, so a
//!   scope that passes here is one the token endpoint understands);
//! * [`type_group`] — the group a resource type is listed under;
//! * [`PRESETS`] and [`NON_RESOURCE`] — one-click starting points and the
//!   identity / launch / refresh scopes with their meaning;
//! * [`redundant`] — scopes another allowed scope already covers;
//! * [`analyze`] — warnings about combinations that cannot work (system/
//!   scopes on a public client, patient/ scopes with no way to pick a
//!   patient, scopes the realm does not offer, …).
//!
//! Nothing here changes what is stored: a client's policy stays the list of
//! scope strings in `ClientSpec.scopes.allowed`.

use serde::Serialize;

use crate::realm::RealmFeatures;
use crate::scope::{Perms, ResourceScope, Scope, ScopeContext, parse_scope};

/// Why a scope string is not one the authorization server can grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SyntaxError {
    /// Empty, or a character outside RFC 6749 §3.3 `scope-token`
    /// (`%x21 / %x23-5B / %x5D-7E`; no spaces, quotes or backslashes).
    Characters,
    /// `patient/` / `user/` / `system/` without `Type.perms`.
    ResourceShape,
    /// The resource type is not `*` or a capitalised FHIR type name.
    ResourceType,
    /// Permissions are not `read` / `write` / `*` or an ordered subset of
    /// `cruds` (`rs`, `cud`, …).
    Permissions,
    /// A v1 scope (`.read`, `.write`, `.*`) with a `?` query: granular
    /// scopes exist only in v2.
    V1Query,
    /// The `?` part is not `name=value` pairs joined by `&`.
    Query,
    /// `launch/` without a context name (`launch/patient`).
    Launch,
    /// A well-known scope in the wrong case (`fhiruser`, `Patient/…`):
    /// scopes are case sensitive.
    Case,
}

impl SyntaxError {
    pub fn code(self) -> &'static str {
        match self {
            SyntaxError::Characters => "characters",
            SyntaxError::ResourceShape => "resource-shape",
            SyntaxError::ResourceType => "resource-type",
            SyntaxError::Permissions => "permissions",
            SyntaxError::V1Query => "v1-query",
            SyntaxError::Query => "query",
            SyntaxError::Launch => "launch",
            SyntaxError::Case => "case",
        }
    }

    /// English explanation (API errors; the console localises by code).
    pub fn message(self) -> &'static str {
        match self {
            SyntaxError::Characters => "scopes are single words: no spaces, quotes or backslashes",
            SyntaxError::ResourceShape => {
                "a resource scope is context/Type.permissions, e.g. patient/Observation.rs"
            }
            SyntaxError::ResourceType => {
                "the resource type must be * or a FHIR type name such as Observation (case sensitive)"
            }
            SyntaxError::Permissions => {
                "permissions are read, write, * (SMART v1) or letters from c r u d s in that order (v2), e.g. rs or cruds"
            }
            SyntaxError::V1Query => {
                "query filters (?category=…) need v2 permissions, e.g. .rs instead of .read"
            }
            SyntaxError::Query => "the filter after ? must be name=value pairs joined by &",
            SyntaxError::Launch => {
                "launch context scopes look like launch/patient or launch/encounter"
            }
            SyntaxError::Case => "scopes are case sensitive",
        }
    }
}

const WELL_KNOWN: &[&str] = &[
    "openid",
    "fhirUser",
    "profile",
    "email",
    "launch",
    "offline_access",
    "online_access",
];

fn scope_token_chars(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b == 0x21 || (0x23..=0x5B).contains(&b) || (0x5D..=0x7E).contains(&b))
}

fn query_ok(q: &str) -> bool {
    q.split('&').all(|pair| match pair.split_once('=') {
        Some((k, v)) => !k.is_empty() && !v.is_empty() && !v.contains('='),
        None => false,
    })
}

/// Checks one allowed scope. Custom scopes (anything that is not SMART
/// syntax) are accepted as long as they are a valid OAuth scope token:
/// they are granted only when an app asks for exactly that string.
pub fn check_syntax(tok: &str) -> Result<(), SyntaxError> {
    if !scope_token_chars(tok) {
        return Err(SyntaxError::Characters);
    }
    if WELL_KNOWN
        .iter()
        .any(|k| k.eq_ignore_ascii_case(tok) && *k != tok)
    {
        return Err(SyntaxError::Case);
    }
    let parsed = parse_scope(tok);
    if let Scope::Resource(r) = &parsed {
        if let Some(q) = &r.query
            && !query_ok(q)
        {
            return Err(SyntaxError::Query);
        }
        return Ok(());
    }
    if !matches!(parsed, Scope::Other(_)) {
        return Ok(());
    }
    // Something that looks like SMART but is not: say what is wrong.
    if tok.starts_with("launch/") {
        return Err(SyntaxError::Launch);
    }
    let Some((ctx, rest)) = tok.split_once('/') else {
        return Ok(());
    };
    let ctx_known = ["patient", "user", "system"];
    if !ctx_known.contains(&ctx) {
        if ctx_known.iter().any(|c| c.eq_ignore_ascii_case(ctx))
            || ctx.eq_ignore_ascii_case("launch")
        {
            return Err(SyntaxError::Case);
        }
        // `https://example.org/custom` and the like: a custom scope.
        return Ok(());
    }
    let (body, query) = match rest.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (rest, None),
    };
    let Some((rt, perm)) = body.split_once('.') else {
        return Err(SyntaxError::ResourceShape);
    };
    if rt.is_empty() || perm.is_empty() {
        return Err(SyntaxError::ResourceShape);
    }
    let type_ok = rt == "*"
        || (rt.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && rt.chars().all(|c| c.is_ascii_alphanumeric()));
    if !type_ok {
        return Err(SyntaxError::ResourceType);
    }
    let v1 = matches!(perm, "read" | "write" | "*");
    if !v1 && Perms::parse_v2(perm).is_none() {
        return Err(SyntaxError::Permissions);
    }
    match query {
        Some(_) if v1 => Err(SyntaxError::V1Query),
        Some(q) if q.is_empty() || !query_ok(q) => Err(SyntaxError::Query),
        _ => Err(SyntaxError::ResourceShape),
    }
}

/// The SMART v1 spelling apps may also ask for (only when the permissions
/// map exactly: `rs` → `read`, `cud` → `write`, `cruds` → `*`).
pub fn v1_equivalent(r: &ResourceScope) -> Option<String> {
    if r.query.is_some() || r.v1 {
        return None;
    }
    let p = match r.perms.letters().as_str() {
        "rs" => "read",
        "cud" => "write",
        "cruds" => "*",
        _ => return None,
    };
    Some(format!("{}/{}.{}", r.context.as_str(), r.resource_type, p))
}

// ------------------------------------------------------------ type groups

/// Where a resource type is listed in the builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TypeGroup {
    Clinical,
    Medications,
    Diagnostics,
    Administrative,
    Documents,
    Financial,
    Workflow,
    Conformance,
    Other,
}

impl TypeGroup {
    pub const ALL: [TypeGroup; 9] = [
        TypeGroup::Clinical,
        TypeGroup::Medications,
        TypeGroup::Diagnostics,
        TypeGroup::Administrative,
        TypeGroup::Documents,
        TypeGroup::Financial,
        TypeGroup::Workflow,
        TypeGroup::Conformance,
        TypeGroup::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            TypeGroup::Clinical => "clinical",
            TypeGroup::Medications => "medications",
            TypeGroup::Diagnostics => "diagnostics",
            TypeGroup::Administrative => "administrative",
            TypeGroup::Documents => "documents",
            TypeGroup::Financial => "financial",
            TypeGroup::Workflow => "workflow",
            TypeGroup::Conformance => "conformance",
            TypeGroup::Other => "other",
        }
    }
}

/// The group of a resource type (FHIR R4–R6 names; unknown → Other).
pub fn type_group(t: &str) -> TypeGroup {
    use TypeGroup::*;
    match t {
        "Observation"
        | "Condition"
        | "Procedure"
        | "AllergyIntolerance"
        | "AdverseEvent"
        | "CarePlan"
        | "CareTeam"
        | "ClinicalImpression"
        | "ClinicalAssessment"
        | "DetectedIssue"
        | "FamilyMemberHistory"
        | "Flag"
        | "Goal"
        | "RiskAssessment"
        | "ServiceRequest"
        | "NutritionOrder"
        | "NutritionIntake"
        | "VisionPrescription"
        | "DeviceUseStatement"
        | "DeviceUsage" => Clinical,
        "Medication"
        | "MedicationAdministration"
        | "MedicationDispense"
        | "MedicationKnowledge"
        | "MedicationRequest"
        | "MedicationStatement"
        | "MedicationUsage"
        | "Immunization"
        | "ImmunizationEvaluation"
        | "ImmunizationRecommendation"
        | "MedicinalProduct"
        | "MedicinalProductAuthorization"
        | "MedicinalProductContraindication"
        | "MedicinalProductIndication"
        | "MedicinalProductIngredient"
        | "MedicinalProductInteraction"
        | "MedicinalProductManufactured"
        | "MedicinalProductPackaged"
        | "MedicinalProductPharmaceutical"
        | "MedicinalProductUndesirableEffect"
        | "MedicinalProductDefinition"
        | "PackagedProductDefinition"
        | "AdministrableProductDefinition"
        | "ManufacturedItemDefinition"
        | "Ingredient"
        | "ClinicalUseDefinition"
        | "RegulatedAuthorization"
        | "Substance"
        | "SubstanceDefinition"
        | "SubstanceNucleicAcid"
        | "SubstancePolymer"
        | "SubstanceProtein"
        | "SubstanceReferenceInformation"
        | "SubstanceSourceMaterial"
        | "SubstanceSpecification"
        | "NutritionProduct"
        | "FormularyItem" => Medications,
        "DiagnosticReport"
        | "ImagingStudy"
        | "ImagingSelection"
        | "Media"
        | "Specimen"
        | "SpecimenDefinition"
        | "MolecularSequence"
        | "GenomicStudy"
        | "BodyStructure"
        | "ObservationDefinition" => Diagnostics,
        "Patient"
        | "Practitioner"
        | "PractitionerRole"
        | "RelatedPerson"
        | "Person"
        | "Group"
        | "Organization"
        | "OrganizationAffiliation"
        | "HealthcareService"
        | "Endpoint"
        | "Location"
        | "Encounter"
        | "EpisodeOfCare"
        | "EncounterHistory"
        | "Appointment"
        | "AppointmentResponse"
        | "Schedule"
        | "Slot"
        | "Device"
        | "DeviceDefinition"
        | "DeviceMetric"
        | "DeviceAssociation"
        | "BiologicallyDerivedProduct"
        | "BiologicallyDerivedProductDispense"
        | "VerificationResult"
        | "Linkage" => Administrative,
        "Composition"
        | "DocumentReference"
        | "DocumentManifest"
        | "Binary"
        | "Bundle"
        | "List"
        | "Consent"
        | "Questionnaire"
        | "QuestionnaireResponse"
        | "Provenance"
        | "AuditEvent" => Documents,
        "Account"
        | "ChargeItem"
        | "ChargeItemDefinition"
        | "Claim"
        | "ClaimResponse"
        | "Contract"
        | "Coverage"
        | "CoverageEligibilityRequest"
        | "CoverageEligibilityResponse"
        | "EnrollmentRequest"
        | "EnrollmentResponse"
        | "ExplanationOfBenefit"
        | "Invoice"
        | "PaymentNotice"
        | "PaymentReconciliation"
        | "InsurancePlan"
        | "InsuranceProduct" => Financial,
        "Task"
        | "Communication"
        | "CommunicationRequest"
        | "DeviceRequest"
        | "DeviceDispense"
        | "SupplyDelivery"
        | "SupplyRequest"
        | "RequestGroup"
        | "RequestOrchestration"
        | "Transport"
        | "GuidanceResponse"
        | "ActivityDefinition"
        | "PlanDefinition"
        | "Subscription"
        | "SubscriptionStatus"
        | "SubscriptionTopic"
        | "MessageHeader"
        | "EventDefinition"
        | "InventoryItem"
        | "InventoryReport"
        | "Measure"
        | "MeasureReport"
        | "Library"
        | "ResearchStudy"
        | "ResearchSubject" => Workflow,
        "CapabilityStatement"
        | "CapabilityStatement2"
        | "StructureDefinition"
        | "StructureMap"
        | "ImplementationGuide"
        | "SearchParameter"
        | "OperationDefinition"
        | "CompartmentDefinition"
        | "GraphDefinition"
        | "MessageDefinition"
        | "NamingSystem"
        | "CodeSystem"
        | "ValueSet"
        | "ConceptMap"
        | "TerminologyCapabilities"
        | "ExampleScenario"
        | "TestScript"
        | "TestReport"
        | "TestPlan"
        | "Requirements"
        | "ActorDefinition"
        | "ViewDefinition" => Conformance,
        _ => Other,
    }
}

/// Types grouped for the builder, in group order, each group sorted.
pub fn group_types<S: AsRef<str>>(types: &[S]) -> Vec<(TypeGroup, Vec<String>)> {
    let mut out: Vec<(TypeGroup, Vec<String>)> =
        TypeGroup::ALL.iter().map(|g| (*g, Vec::new())).collect();
    for t in types {
        let t = t.as_ref();
        if t.is_empty() || t == "*" {
            continue;
        }
        let g = type_group(t);
        if let Some((_, v)) = out.iter_mut().find(|(x, _)| *x == g)
            && !v.iter().any(|x| x == t)
        {
            v.push(t.to_string());
        }
    }
    for (_, v) in &mut out {
        v.sort();
    }
    out.retain(|(_, v)| !v.is_empty());
    out
}

// ---------------------------------------------------------------- presets

/// A one-click starting point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Preset {
    pub id: &'static str,
    pub scopes: &'static [&'static str],
    /// Backend Services: only `client_credentials` makes sense.
    pub backend: bool,
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "patient-read",
        scopes: &[
            "openid",
            "fhirUser",
            "launch/patient",
            "offline_access",
            "patient/*.rs",
        ],
        backend: false,
    },
    Preset {
        id: "clinician-read",
        scopes: &["openid", "fhirUser", "launch", "user/*.rs"],
        backend: false,
    },
    Preset {
        id: "clinician-write",
        scopes: &["openid", "fhirUser", "launch", "user/*.cruds"],
        backend: false,
    },
    Preset {
        id: "ehr-launch",
        scopes: &["launch", "openid", "fhirUser", "patient/*.rs"],
        backend: false,
    },
    Preset {
        id: "backend-export",
        scopes: &["system/*.rs"],
        backend: true,
    },
    Preset {
        id: "identity",
        scopes: &["openid", "fhirUser", "profile"],
        backend: false,
    },
];

pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

/// The identity, launch and refresh scopes the builder offers as toggles,
/// in display order.
pub const NON_RESOURCE: &[&str] = &[
    "openid",
    "fhirUser",
    "profile",
    "launch",
    "launch/patient",
    "launch/encounter",
    "offline_access",
    "online_access",
];

/// Whether the realm offers `tok` at all (its SMART capabilities). Scopes
/// it does not offer are dropped from every request. `system/` scopes are
/// offered only through `client_credentials` (`via_client_credentials`).
pub fn realm_offers(f: &RealmFeatures, tok: &str, via_client_credentials: bool) -> bool {
    match parse_scope(tok) {
        Scope::Resource(r) => {
            (!r.v1 || f.permission_v1)
                && match r.context {
                    ScopeContext::Patient => f.permission_patient,
                    ScopeContext::User => f.permission_user,
                    ScopeContext::System => via_client_credentials && f.backend_services,
                }
        }
        Scope::Launch => f.launch_ehr,
        Scope::LaunchContext { .. } => f.launch_standalone || f.launch_ehr,
        Scope::OpenId | Scope::FhirUser | Scope::Profile | Scope::Email => f.openid_connect,
        Scope::OfflineAccess => f.offline_access,
        Scope::OnlineAccess => f.online_access,
        Scope::Other(_) => true,
    }
}

// ------------------------------------------------------------- redundancy

/// An allowed scope that adds nothing: another allowed scope covers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Redundant {
    pub scope: String,
    pub covered_by: String,
}

/// Scopes covered by another scope of the list (exact duplicates, a type
/// under a `*` with at least its permissions, a granular scope under the
/// same unfiltered one, `.read` next to `.rs`). Of two scopes that cover
/// each other the later one is reported.
pub fn redundant(scopes: &[String]) -> Vec<Redundant> {
    let parsed: Vec<Option<ResourceScope>> = scopes
        .iter()
        .map(|s| match parse_scope(s) {
            Scope::Resource(r) => Some(r),
            _ => None,
        })
        .collect();
    let mut out = vec![];
    for (i, s) in scopes.iter().enumerate() {
        let hit = scopes.iter().enumerate().find(|(j, other)| {
            if *j == i {
                return false;
            }
            match (&parsed[*j], &parsed[i]) {
                (Some(a), Some(b)) => a.covers(b) && (!b.covers(a) || *j < i),
                _ => *other == s && *j < i,
            }
        });
        if let Some((_, by)) = hit {
            out.push(Redundant {
                scope: s.clone(),
                covered_by: by.clone(),
            });
        }
    }
    out
}

// --------------------------------------------------------------- analysis

/// What the analysis needs to know about the client.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClientShape {
    /// Public client (`token_endpoint_auth_method=none`).
    pub public: bool,
    pub authorization_code: bool,
    pub client_credentials: bool,
    pub standalone: bool,
    pub ehr: bool,
    pub accept_v1: bool,
}

impl ClientShape {
    pub fn of(c: &crate::client::ClientSpec) -> Self {
        use crate::client::{GrantType, LaunchType};
        ClientShape {
            public: !c.auth.is_confidential(),
            authorization_code: c.grant_types.contains(&GrantType::AuthorizationCode),
            client_credentials: c.grant_types.contains(&GrantType::ClientCredentials),
            standalone: c.launch_types.contains(&LaunchType::Standalone),
            ehr: c.launch_types.contains(&LaunchType::Ehr),
            accept_v1: c.scopes.accept_v1,
        }
    }
}

/// How serious a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Something will not work as configured.
    Warn,
    /// Worth knowing; works as configured.
    Info,
}

/// One finding. `code` is stable (the console localises it); `scopes` are
/// the scopes it is about; `detail` is a parameter of the message (a realm
/// feature, endpoint names).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub code: &'static str,
    pub level: Level,
    pub scopes: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

impl Finding {
    fn new(code: &'static str, level: Level, scopes: Vec<String>) -> Self {
        Finding {
            code,
            level,
            scopes,
            detail: String::new(),
        }
    }

    fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = d.into();
        self
    }

    /// English text (API, logs); the console renders `idps-warn-{code}`.
    pub fn message(&self) -> String {
        let list = self.scopes.join(" ");
        match self.code {
            "empty" => "No scopes are allowed: the app cannot get a token.".into(),
            "system-public" => format!(
                "{list}: system/ scopes need a confidential client using client_credentials; a public client can never get them."
            ),
            "system-no-cc" => format!(
                "{list}: system/ scopes are granted only through client_credentials (Backend Services), which this app does not use."
            ),
            "user-scopes-backend" => format!(
                "{list}: patient/ and user/ scopes need a signed-in user; a backend (client_credentials) app never gets them."
            ),
            "standalone-no-launch-patient" => format!(
                "{list}: a standalone app needs launch/patient to choose the patient these patient/ scopes are confined to."
            ),
            "ehr-no-launch" => {
                "EHR launch is enabled but the launch scope is not allowed: EHR launches will be refused.".into()
            }
            "patient-no-context" => format!(
                "{list}: patient/ scopes need a patient: allow launch/patient (standalone) or launch (EHR launch)."
            ),
            "fhiruser-no-openid" => format!(
                "{list}: fhirUser and profile are returned in the ID token, which needs openid."
            ),
            "online-offline" => {
                "online_access and offline_access both allowed: an app asking for both gets refresh tokens that outlive the session.".into()
            }
            "v1-syntax" => format!(
                "{list}: written in SMART v1 syntax; v2 requests are covered, but v1 apps can use them only with \"Accept SMART v1 scopes\" on."
            ),
            "realm-off" => format!(
                "{list}: the realm does not offer {} — these scopes are never granted.",
                self.detail
            ),
            "write-protect-reads" => format!(
                "Endpoint {} allows anyone to read without a token (write-protect), so read scopes do not restrict reading there; write scopes are enforced.",
                self.detail
            ),
            "no-resource" => {
                "No FHIR resource scopes: the app can sign the user in but cannot read or write data.".into()
            }
            "launch-not-ehr" => {
                "launch is allowed but EHR launch is not enabled for this app.".into()
            }
            "invalid" => format!("{list}: not a valid scope."),
            "redundant" => format!("{list}: already covered by {}.", self.detail),
            other => other.to_string(),
        }
    }
}

/// Everything the builder shows about a scope list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Analysis {
    /// Scopes that fail [`check_syntax`]: (scope, error).
    pub invalid: Vec<(String, SyntaxError)>,
    pub redundant: Vec<Redundant>,
    pub findings: Vec<Finding>,
}

/// Analyses `scopes` for a client of `shape` in a realm with `features`
/// (`None`: realm not known yet), bound to endpoints of which
/// `write_protected` allow anonymous reads.
pub fn analyze(
    scopes: &[String],
    shape: &ClientShape,
    features: Option<&RealmFeatures>,
    write_protected: &[String],
) -> Analysis {
    let mut a = Analysis::default();
    for s in scopes {
        if let Err(e) = check_syntax(s) {
            a.invalid.push((s.clone(), e));
        }
    }
    a.redundant = redundant(scopes);
    let f = &mut a.findings;
    if scopes.is_empty() {
        f.push(Finding::new("empty", Level::Warn, vec![]));
        return a;
    }
    let has = |t: &str| scopes.iter().any(|s| s == t);
    let by_ctx = |c: ScopeContext| -> Vec<String> {
        scopes
            .iter()
            .filter(|s| matches!(parse_scope(s), Scope::Resource(r) if r.context == c))
            .cloned()
            .collect()
    };
    let patient = by_ctx(ScopeContext::Patient);
    let user = by_ctx(ScopeContext::User);
    let system = by_ctx(ScopeContext::System);
    let resources: Vec<ResourceScope> = scopes
        .iter()
        .filter_map(|s| match parse_scope(s) {
            Scope::Resource(r) => Some(r),
            _ => None,
        })
        .collect();

    if !system.is_empty() {
        if shape.public {
            f.push(Finding::new("system-public", Level::Warn, system.clone()));
        } else if !shape.client_credentials {
            f.push(Finding::new("system-no-cc", Level::Warn, system.clone()));
        }
    }
    let user_facing: Vec<String> = patient.iter().chain(user.iter()).cloned().collect();
    if !user_facing.is_empty() && shape.client_credentials && !shape.authorization_code {
        f.push(Finding::new(
            "user-scopes-backend",
            Level::Warn,
            user_facing.clone(),
        ));
    }
    if !patient.is_empty() && shape.authorization_code {
        if shape.standalone && !has("launch/patient") {
            f.push(Finding::new(
                "standalone-no-launch-patient",
                Level::Warn,
                patient.clone(),
            ));
        } else if !shape.standalone && !shape.ehr && !has("launch/patient") && !has("launch") {
            f.push(Finding::new(
                "patient-no-context",
                Level::Warn,
                patient.clone(),
            ));
        }
    }
    if shape.ehr && shape.authorization_code && !has("launch") {
        f.push(Finding::new("ehr-no-launch", Level::Warn, vec![]));
    }
    if has("launch") && !shape.ehr && shape.authorization_code {
        f.push(Finding::new(
            "launch-not-ehr",
            Level::Info,
            vec!["launch".into()],
        ));
    }
    let id_claims: Vec<String> = ["fhirUser", "profile"]
        .iter()
        .filter(|s| has(s))
        .map(|s| s.to_string())
        .collect();
    if !id_claims.is_empty() && !has("openid") {
        f.push(Finding::new("fhiruser-no-openid", Level::Warn, id_claims));
    }
    if has("online_access") && has("offline_access") {
        f.push(Finding::new(
            "online-offline",
            Level::Info,
            vec!["online_access".into(), "offline_access".into()],
        ));
    }
    let v1: Vec<String> = scopes
        .iter()
        .filter(|s| matches!(parse_scope(s), Scope::Resource(r) if r.v1))
        .cloned()
        .collect();
    if !v1.is_empty() && !shape.accept_v1 {
        f.push(Finding::new("v1-syntax", Level::Info, v1));
    }
    if let Some(ft) = features {
        let mut off = |feature: &str, list: Vec<String>| {
            if !list.is_empty() {
                f.push(Finding::new("realm-off", Level::Warn, list).detail(feature));
            }
        };
        if !ft.permission_patient {
            off("permission-patient", patient.clone());
        }
        if !ft.permission_user {
            off("permission-user", user.clone());
        }
        if !ft.backend_services {
            off("backend-services", system.clone());
        }
        if !ft.openid_connect {
            off(
                "sso-openid-connect",
                ["openid", "fhirUser", "profile", "email"]
                    .iter()
                    .filter(|s| has(s))
                    .map(|s| s.to_string())
                    .collect(),
            );
        }
        if !ft.offline_access && has("offline_access") {
            off("offline-access", vec!["offline_access".into()]);
        }
        if !ft.online_access && has("online_access") {
            off("online-access", vec!["online_access".into()]);
        }
        if !ft.launch_ehr && has("launch") {
            off("launch-ehr", vec!["launch".into()]);
        }
        if !ft.launch_standalone && !ft.launch_ehr {
            off(
                "launch-standalone",
                scopes
                    .iter()
                    .filter(|s| s.starts_with("launch/"))
                    .cloned()
                    .collect(),
            );
        }
    }
    let reads = resources
        .iter()
        .any(|r| r.context != ScopeContext::System && r.perms.contains(Perms::READ));
    if reads && !write_protected.is_empty() {
        f.push(
            Finding::new("write-protect-reads", Level::Info, vec![])
                .detail(write_protected.join(", ")),
        );
    }
    if resources.is_empty() && !has("launch") && !scopes.iter().any(|s| s.starts_with("launch/")) {
        f.push(Finding::new("no-resource", Level::Info, vec![]));
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::split_scopes;

    fn v(s: &str) -> Vec<String> {
        split_scopes(s)
    }

    fn codes(a: &Analysis) -> Vec<&'static str> {
        a.findings.iter().map(|f| f.code).collect()
    }

    fn public_standalone() -> ClientShape {
        ClientShape {
            public: true,
            authorization_code: true,
            standalone: true,
            ..Default::default()
        }
    }

    #[test]
    fn syntax_accepts_the_grammar_the_token_endpoint_grants() {
        for ok in [
            "openid",
            "fhirUser",
            "profile",
            "email",
            "launch",
            "launch/patient",
            "launch/encounter",
            "launch/encounter?role=https://example.org/role",
            "offline_access",
            "online_access",
            "patient/*.rs",
            "patient/Observation.rs",
            "user/*.cruds",
            "system/*.rs",
            "patient/*.read",
            "user/Observation.write",
            "user/*.*",
            "patient/Observation.rs?category=laboratory",
            "patient/Observation.rs?category=http://terminology.hl7.org/CodeSystem/observation-category|vital-signs",
            "patient/Condition.rs?category=problem-list-item&clinical-status=active",
            "custom_scope",
            "https://example.org/scopes/x",
        ] {
            assert_eq!(check_syntax(ok), Ok(()), "{ok}");
            if ok.starts_with("patient/") || ok.starts_with("user/") || ok.starts_with("system/") {
                assert!(matches!(parse_scope(ok), Scope::Resource(_)), "{ok}");
            }
        }
    }

    #[test]
    fn syntax_rejects_near_misses_with_a_reason() {
        use SyntaxError::*;
        for (bad, why) in [
            ("", Characters),
            ("patient/*.rs user/*.rs", Characters),
            ("a\"b", Characters),
            ("fhiruser", Case),
            ("OpenID", Case),
            ("Patient/Observation.rs", Case),
            ("Launch/patient", Case),
            ("patient/Observation", ResourceShape),
            ("patient/.rs", ResourceShape),
            ("patient/Observation.", ResourceShape),
            ("patient/observation.rs", ResourceType),
            ("patient/Obs-ervation.rs", ResourceType),
            ("patient/Observation.sr", Permissions),
            ("patient/Observation.rr", Permissions),
            ("patient/Observation.x", Permissions),
            ("patient/Observation.read?category=x", V1Query),
            ("patient/Observation.rs?", Query),
            ("patient/Observation.rs?category", Query),
            ("patient/Observation.rs?=x", Query),
            ("patient/Observation.rs?a=b&", Query),
            ("launch/", Launch),
            ("launch/pat-ient", Launch),
        ] {
            assert_eq!(check_syntax(bad), Err(why), "{bad:?}");
            assert!(!why.message().is_empty());
        }
    }

    #[test]
    fn round_trip_parse_display() {
        for s in [
            "patient/Observation.rs",
            "user/*.cruds",
            "system/Patient.r",
            "patient/Observation.rs?category=laboratory",
        ] {
            let Scope::Resource(r) = parse_scope(s) else {
                panic!("{s}")
            };
            assert_eq!(r.to_string(), s);
        }
        // v1 parses to v2 permissions and prints as v2.
        let Scope::Resource(r) = parse_scope("patient/*.read") else {
            panic!()
        };
        assert_eq!(r.to_string(), "patient/*.rs");
    }

    #[test]
    fn v1_equivalents() {
        let eq = |s: &str| match parse_scope(s) {
            Scope::Resource(r) => v1_equivalent(&r),
            _ => None,
        };
        assert_eq!(
            eq("patient/Observation.rs").as_deref(),
            Some("patient/Observation.read")
        );
        assert_eq!(eq("user/*.cruds").as_deref(), Some("user/*.*"));
        assert_eq!(eq("user/*.cud").as_deref(), Some("user/*.write"));
        assert_eq!(eq("user/*.r"), None);
        assert_eq!(eq("patient/Observation.rs?category=laboratory"), None);
        assert_eq!(eq("patient/*.read"), None);
    }

    #[test]
    fn groups() {
        assert_eq!(type_group("Observation"), TypeGroup::Clinical);
        assert_eq!(type_group("Condition"), TypeGroup::Clinical);
        assert_eq!(type_group("MedicationRequest"), TypeGroup::Medications);
        assert_eq!(type_group("Immunization"), TypeGroup::Medications);
        assert_eq!(type_group("DiagnosticReport"), TypeGroup::Diagnostics);
        assert_eq!(type_group("Patient"), TypeGroup::Administrative);
        assert_eq!(type_group("Encounter"), TypeGroup::Administrative);
        assert_eq!(type_group("DocumentReference"), TypeGroup::Documents);
        assert_eq!(type_group("Claim"), TypeGroup::Financial);
        assert_eq!(type_group("Task"), TypeGroup::Workflow);
        assert_eq!(type_group("ValueSet"), TypeGroup::Conformance);
        assert_eq!(type_group("Basic"), TypeGroup::Other);
        assert_eq!(type_group("SomethingNew"), TypeGroup::Other);
        let g = group_types(&[
            "Patient",
            "Observation",
            "*",
            "Basic",
            "Condition",
            "Patient",
        ]);
        assert_eq!(
            g,
            vec![
                (
                    TypeGroup::Clinical,
                    vec!["Condition".to_string(), "Observation".into()]
                ),
                (TypeGroup::Administrative, vec!["Patient".to_string()]),
                (TypeGroup::Other, vec!["Basic".to_string()]),
            ]
        );
    }

    #[test]
    fn presets_are_valid_and_as_specified() {
        let p = |id: &str| preset(id).unwrap().scopes.join(" ");
        assert_eq!(
            p("patient-read"),
            "openid fhirUser launch/patient offline_access patient/*.rs"
        );
        assert_eq!(p("clinician-read"), "openid fhirUser launch user/*.rs");
        assert_eq!(p("clinician-write"), "openid fhirUser launch user/*.cruds");
        assert_eq!(p("ehr-launch"), "launch openid fhirUser patient/*.rs");
        assert_eq!(p("backend-export"), "system/*.rs");
        assert!(preset("backend-export").unwrap().backend);
        assert_eq!(p("identity"), "openid fhirUser profile");
        for pr in PRESETS {
            for s in pr.scopes {
                assert_eq!(check_syntax(s), Ok(()), "{s}");
            }
            let list: Vec<String> = pr.scopes.iter().map(|s| s.to_string()).collect();
            assert!(redundant(&list).is_empty(), "{}", pr.id);
        }
        // Each preset is clean for the kind of app it is meant for.
        let backend = ClientShape {
            client_credentials: true,
            ..Default::default()
        };
        let ehr = ClientShape {
            authorization_code: true,
            ehr: true,
            ..Default::default()
        };
        let clinician = ClientShape {
            authorization_code: true,
            ehr: true,
            ..Default::default()
        };
        for (id, shape) in [
            ("patient-read", public_standalone()),
            ("clinician-read", clinician.clone()),
            ("clinician-write", clinician),
            ("ehr-launch", ehr),
            ("backend-export", backend),
        ] {
            let list: Vec<String> = preset(id)
                .unwrap()
                .scopes
                .iter()
                .map(|s| s.to_string())
                .collect();
            let a = analyze(&list, &shape, Some(&RealmFeatures::default()), &[]);
            assert!(
                a.findings.iter().all(|f| f.level == Level::Info),
                "{id}: {:?}",
                a.findings
            );
        }
        for s in NON_RESOURCE {
            assert_eq!(check_syntax(s), Ok(()));
        }
    }

    #[test]
    fn redundancy() {
        let r = redundant(&v(
            "patient/*.rs patient/Observation.rs patient/Observation.r patient/Condition.cruds openid openid user/Observation.rs?category=laboratory user/Observation.rs patient/*.read",
        ));
        let pairs: Vec<(&str, &str)> = r
            .iter()
            .map(|x| (x.scope.as_str(), x.covered_by.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("patient/Observation.rs", "patient/*.rs"),
                ("patient/Observation.r", "patient/*.rs"),
                (
                    "user/Observation.rs?category=laboratory",
                    "user/Observation.rs"
                ),
                ("patient/*.read", "patient/*.rs"),
            ]
        );
        // split_scopes dropped the duplicate openid; duplicates are found too.
        let d = redundant(&["openid".into(), "openid".into()]);
        assert_eq!(d.len(), 1);
        // Different contexts never cover each other; more permissions are not redundant.
        assert!(redundant(&v("patient/*.rs user/*.rs patient/Observation.cruds")).is_empty());
    }

    #[test]
    fn warnings() {
        let feat = RealmFeatures {
            online_access: true,
            ..Default::default()
        };
        // system/ on a public client.
        let a = analyze(&v("system/*.rs"), &public_standalone(), Some(&feat), &[]);
        assert_eq!(codes(&a), vec!["system-public"]);
        // system/ on a confidential client without client_credentials.
        let conf = ClientShape {
            authorization_code: true,
            standalone: true,
            ..Default::default()
        };
        let a = analyze(
            &v("openid fhirUser launch/patient patient/*.rs system/*.rs"),
            &conf,
            Some(&feat),
            &[],
        );
        assert_eq!(codes(&a), vec!["system-no-cc"]);
        // patient/ on a standalone app without launch/patient.
        let a = analyze(
            &v("openid fhirUser patient/*.rs"),
            &public_standalone(),
            Some(&feat),
            &[],
        );
        assert_eq!(codes(&a), vec!["standalone-no-launch-patient"]);
        // EHR launch without the launch scope.
        let ehr = ClientShape {
            authorization_code: true,
            ehr: true,
            ..Default::default()
        };
        let a = analyze(&v("openid fhirUser patient/*.rs"), &ehr, Some(&feat), &[]);
        assert_eq!(codes(&a), vec!["ehr-no-launch"]);
        // neither launch type: patient scopes have no patient.
        let none = ClientShape {
            authorization_code: true,
            ..Default::default()
        };
        let a = analyze(&v("patient/*.rs"), &none, None, &[]);
        assert_eq!(codes(&a), vec!["patient-no-context"]);
        // user scopes on a backend client.
        let backend = ClientShape {
            client_credentials: true,
            ..Default::default()
        };
        let a = analyze(&v("system/*.rs user/*.rs"), &backend, Some(&feat), &[]);
        assert_eq!(codes(&a), vec!["user-scopes-backend"]);
        assert_eq!(a.findings[0].scopes, v("user/*.rs"));
        // fhirUser without openid; online + offline.
        let a = analyze(
            &v("fhirUser launch/patient patient/*.rs online_access offline_access"),
            &public_standalone(),
            Some(&feat),
            &[],
        );
        assert_eq!(codes(&a), vec!["fhiruser-no-openid", "online-offline"]);
        // v1 syntax without accept_v1.
        let a = analyze(
            &v("launch/patient patient/*.read"),
            &public_standalone(),
            Some(&feat),
            &[],
        );
        assert_eq!(codes(&a), vec!["v1-syntax"]);
        let mut v1 = public_standalone();
        v1.accept_v1 = true;
        assert!(
            analyze(&v("launch/patient patient/*.read"), &v1, Some(&feat), &[])
                .findings
                .is_empty()
        );
        // Realm features switched off.
        let mut off = RealmFeatures::default();
        off.permission_user = false;
        off.offline_access = false;
        let a = analyze(
            &v("openid launch user/*.rs offline_access"),
            &ehr,
            Some(&off),
            &[],
        );
        assert_eq!(codes(&a), vec!["realm-off", "realm-off"]);
        assert_eq!(a.findings[0].detail, "permission-user");
        assert_eq!(a.findings[1].scopes, v("offline_access"));
        // Write-protect endpoints: reads are open anyway.
        let a = analyze(
            &v("launch/patient patient/*.rs"),
            &public_standalone(),
            Some(&feat),
            &["sandbox".into()],
        );
        assert_eq!(codes(&a), vec!["write-protect-reads"]);
        assert!(a.findings[0].message().contains("sandbox"));
        // Identity only, empty, invalid.
        let a = analyze(
            &v("openid fhirUser profile"),
            &public_standalone(),
            Some(&feat),
            &[],
        );
        assert_eq!(codes(&a), vec!["no-resource"]);
        let a = analyze(&[], &public_standalone(), Some(&feat), &[]);
        assert_eq!(codes(&a), vec!["empty"]);
        let a = analyze(
            &v("launch/patient patient/observation.rs"),
            &public_standalone(),
            None,
            &[],
        );
        assert_eq!(
            a.invalid,
            vec![(
                "patient/observation.rs".to_string(),
                SyntaxError::ResourceType
            )]
        );
        for f in &a.findings {
            assert!(!f.message().is_empty());
        }
    }
}
