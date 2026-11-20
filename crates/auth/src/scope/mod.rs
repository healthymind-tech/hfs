pub mod permissions;
pub mod smart_v2;

pub use permissions::SmartPermissions;
pub use smart_v2::{ResourceTypeSpec, ScopeContext, SmartScope};

/// One way a token may reach resources of a type: optionally only inside the
/// launch patient's compartment, and optionally only resources matching every
/// granular-scope constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// Only resources in the launch patient's compartment (a `patient/` scope).
    pub patient_compartment: bool,
    /// SMART v2 granular constraints (`param`, `value`), all of which must match.
    pub constraints: Vec<(String, String)>,
}

impl Grant {
    /// Whether every resource this grant admits is also admitted by `self`.
    fn subsumes(&self, other: &Grant) -> bool {
        (!self.patient_compartment || other.patient_compartment)
            && self
                .constraints
                .iter()
                .all(|c| other.constraints.contains(c))
    }
}

/// How far a token reaches for one resource type and permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// No scope covers the request.
    Denied,
    /// A `system/` or `user/` scope without constraints covers it: no
    /// restriction beyond the type-level check.
    Full,
    /// Only the union of these grants is reachable. Never empty, and never
    /// contains an unrestricted grant (that would be [`Access::Full`]).
    Restricted(Vec<Grant>),
}

impl Access {
    /// Whether the request is allowed at all.
    pub fn is_allowed(&self) -> bool {
        !matches!(self, Access::Denied)
    }

    /// Whether any grant confines access to the patient compartment.
    pub fn needs_patient(&self) -> bool {
        matches!(self, Access::Restricted(grants) if grants.iter().any(|g| g.patient_compartment))
    }
}

/// A set of parsed SMART v2 scopes from a JWT token.
///
/// Retains the original raw scope tokens alongside the parsed resource scopes so
/// that *operation* scopes (e.g. `system/bulk-submit`), which do not match the
/// `context/resourceType.permissions` grammar and are therefore dropped by
/// [`SmartScope::parse`], can still be inspected — see [`ScopeSet::grants_operation`].
#[derive(Debug, Clone, Default)]
pub struct ScopeSet {
    scopes: Vec<SmartScope>,
    raw: Vec<String>,
}

impl ScopeSet {
    /// Create an empty scope set.
    pub fn empty() -> Self {
        Self {
            scopes: Vec::new(),
            raw: Vec::new(),
        }
    }

    /// Parse a space-delimited scope string (from JWT `scope` claim).
    ///
    /// Non-SMART scopes (e.g., `openid`, `profile`) are silently ignored for
    /// resource-permission checks but retained in [`ScopeSet::raw`].
    pub fn parse(scope_str: &str) -> Self {
        let raw: Vec<String> = scope_str.split_whitespace().map(str::to_string).collect();
        let scopes = raw.iter().filter_map(|s| SmartScope::parse(s)).collect();
        Self { scopes, raw }
    }

    /// Parse from an array of scope strings (from JWT `scp` claim, e.g., Okta).
    pub fn parse_array(scope_strs: &[String]) -> Self {
        let raw: Vec<String> = scope_strs.to_vec();
        let scopes = raw.iter().filter_map(|s| SmartScope::parse(s)).collect();
        Self { scopes, raw }
    }

    /// Check if any scope grants the given permission on the given resource type.
    pub fn is_permitted(&self, resource_type: &str, permission: SmartPermissions) -> bool {
        self.scopes
            .iter()
            .any(|scope| scope.permits(resource_type, permission))
    }

    /// How far this token reaches for `permission` on `resource_type`,
    /// taking patient context and SMART v2 granular constraints into account.
    ///
    /// [`is_permitted`](Self::is_permitted) is the coarse, type-level answer;
    /// this one says what a permitted request may actually see.
    pub fn access(&self, resource_type: &str, permission: SmartPermissions) -> Access {
        let mut grants: Vec<Grant> = Vec::new();
        for scope in self
            .scopes
            .iter()
            .filter(|s| s.covers(resource_type, permission))
        {
            let grant = Grant {
                patient_compartment: scope.context == ScopeContext::Patient,
                constraints: scope.constraints.clone(),
            };
            if !grant.patient_compartment && grant.constraints.is_empty() {
                return Access::Full;
            }
            if grants.iter().any(|g| g.subsumes(&grant)) {
                continue;
            }
            grants.retain(|g| !grant.subsumes(g));
            grants.push(grant);
        }
        if grants.is_empty() {
            Access::Denied
        } else {
            Access::Restricted(grants)
        }
    }

    /// Whether any scope is a `patient/` scope.
    pub fn has_patient_scopes(&self) -> bool {
        self.scopes
            .iter()
            .any(|s| s.context == ScopeContext::Patient)
    }

    /// Whether any scope is a SMART v2 granular scope.
    pub fn has_constrained_scopes(&self) -> bool {
        self.scopes.iter().any(|s| s.is_constrained())
    }

    /// Whether an unconstrained `system/*` or `user/*` scope grants every
    /// permission (`*.cruds`, `*.*`): nothing this token does is confined.
    pub fn writes_and_reads_everything(&self) -> bool {
        self.scopes.iter().any(|s| {
            s.context != ScopeContext::Patient
                && matches!(s.resource_type, ResourceTypeSpec::Wildcard)
                && s.permissions == SmartPermissions::all()
                && s.constraints.is_empty()
        })
    }

    /// Whether this token reads every resource type without restriction: a
    /// `system/*` or `user/*` scope with read, and no constraints.
    pub fn reads_everything(&self) -> bool {
        self.scopes.iter().any(|s| {
            s.context != ScopeContext::Patient
                && matches!(s.resource_type, ResourceTypeSpec::Wildcard)
                && s.permissions.contains(SmartPermissions::READ)
                && s.constraints.is_empty()
        })
    }

    /// Check whether this set holds a **system-context, all-resource** scope
    /// (`system/*.<perm>`) granting the given permission.
    ///
    /// Unlike [`is_permitted`](Self::is_permitted), this requires the scope's
    /// context to be [`ScopeContext::System`] **and** its resource specifier to
    /// be [`ResourceTypeSpec::Wildcard`] — a `user/*` or `patient/*` token, or a
    /// system token scoped to a single resource type (`system/Patient.rs`), never
    /// satisfies it.
    ///
    /// This is the gate for backend-service / administrative endpoints that span
    /// every tenant (e.g. the cross-tenant console metrics), which no per-user or
    /// per-patient app — however broad its wildcard — should be able to reach.
    pub fn has_system_scope(&self, permission: SmartPermissions) -> bool {
        self.scopes.iter().any(|scope| {
            scope.context == ScopeContext::System
                && matches!(scope.resource_type, ResourceTypeSpec::Wildcard)
                && scope.permissions.contains(permission)
        })
    }

    /// Whether a scope in a context **other than** `patient/` (`user/` or
    /// `system/`) grants the permission on the resource type.
    ///
    /// Such a grant is not confined to a patient compartment. Pair with
    /// [`is_permitted_by_patient_scope`](Self::is_permitted_by_patient_scope)
    /// to decide whether a request is unrestricted or compartment-confined;
    /// [`is_permitted`](Self::is_permitted) alone cannot tell the two apart.
    pub fn is_permitted_unrestricted(
        &self,
        resource_type: &str,
        permission: SmartPermissions,
    ) -> bool {
        self.scopes.iter().any(|scope| {
            scope.context != ScopeContext::Patient && scope.permits(resource_type, permission)
        })
    }

    /// Whether a `patient/` scope grants the permission on the resource type.
    ///
    /// A `patient/` grant is confined to the launch patient's compartment
    /// (SMART App Launch, "Patient-specific scopes"); it must never be
    /// honoured without a launch patient to confine it to.
    pub fn is_permitted_by_patient_scope(
        &self,
        resource_type: &str,
        permission: SmartPermissions,
    ) -> bool {
        self.scopes.iter().any(|scope| {
            scope.context == ScopeContext::Patient && scope.permits(resource_type, permission)
        })
    }

    /// True when the token carries at least one resource scope and every one
    /// of them is a `patient/` scope.
    ///
    /// Such a token has no grant that reaches outside a patient compartment,
    /// so endpoints that cannot be confined to one (system history, system
    /// export, administrative operations) must refuse it.
    pub fn is_patient_only(&self) -> bool {
        !self.scopes.is_empty()
            && self
                .scopes
                .iter()
                .all(|scope| scope.context == ScopeContext::Patient)
    }

    /// Returns the parsed scopes.
    pub fn scopes(&self) -> &[SmartScope] {
        &self.scopes
    }

    /// Returns the raw, unparsed scope tokens exactly as presented in the token.
    pub fn raw(&self) -> &[String] {
        &self.raw
    }

    /// Returns true if no SMART scopes were parsed.
    pub fn is_empty(&self) -> bool {
        self.scopes.is_empty()
    }

    /// Returns true if any parsed scope is a system-level wildcard (`system/*.<perms>`).
    ///
    /// Used as the ownership-bypass / broad-grant signal for bulk operations.
    pub fn has_system_wildcard(&self) -> bool {
        self.scopes.iter().any(|s| {
            s.context == ScopeContext::System
                && matches!(s.resource_type, ResourceTypeSpec::Wildcard)
        })
    }

    /// Returns true if the token grants the named system-level *operation* scope.
    ///
    /// Matches the literal raw scope `system/{name}` (e.g. `system/bulk-submit`)
    /// or any system-level wildcard scope (`system/*.<perms>`), which is treated
    /// as granting all operations.
    pub fn grants_operation(&self, name: &str) -> bool {
        let target = format!("system/{name}");
        self.raw.iter().any(|s| s == &target) || self.has_system_wildcard()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_space_delimited() {
        let set = ScopeSet::parse("system/Patient.rs system/Observation.r openid profile");
        assert_eq!(set.scopes().len(), 2);
        assert!(set.is_permitted("Patient", SmartPermissions::READ));
        assert!(set.is_permitted("Patient", SmartPermissions::SEARCH));
        assert!(set.is_permitted("Observation", SmartPermissions::READ));
        assert!(!set.is_permitted("Observation", SmartPermissions::SEARCH));
    }

    #[test]
    fn test_parse_array() {
        let scopes = vec![
            "system/Patient.rs".to_string(),
            "system/*.crud".to_string(),
            "openid".to_string(),
        ];
        let set = ScopeSet::parse_array(&scopes);
        assert_eq!(set.scopes().len(), 2);
        assert!(set.is_permitted("Patient", SmartPermissions::READ));
        // Wildcard scope grants CRUD on everything
        assert!(set.is_permitted("Condition", SmartPermissions::CREATE));
    }

    #[test]
    fn test_empty_scope() {
        let set = ScopeSet::parse("");
        assert!(set.is_empty());
        assert!(!set.is_permitted("Patient", SmartPermissions::READ));
    }

    #[test]
    fn test_wildcard_scope() {
        let set = ScopeSet::parse("system/*.cruds");
        assert!(set.is_permitted("Patient", SmartPermissions::CREATE));
        assert!(set.is_permitted("Observation", SmartPermissions::DELETE));
        assert!(set.is_permitted("Condition", SmartPermissions::SEARCH));
    }

    #[test]
    fn test_non_smart_scopes_ignored() {
        let set = ScopeSet::parse("openid profile email launch/patient");
        assert!(set.is_empty());
    }

    #[test]
    fn test_grants_operation_literal() {
        let set = ScopeSet::parse("openid system/bulk-submit profile");
        // Operation scope is dropped from parsed resource scopes but retained raw.
        assert!(set.is_empty());
        assert!(set.grants_operation("bulk-submit"));
        assert!(!set.grants_operation("export"));
        assert!(!set.has_system_wildcard());
    }

    #[test]
    fn test_grants_operation_via_wildcard() {
        let set = ScopeSet::parse("system/*.cruds");
        assert!(set.has_system_wildcard());
        // A system wildcard is treated as granting any operation.
        assert!(set.grants_operation("bulk-submit"));
    }

    #[test]
    fn test_grants_operation_array_claim() {
        let set = ScopeSet::parse_array(&[
            "system/bulk-submit".to_string(),
            "system/Patient.rs".to_string(),
        ]);
        assert!(set.grants_operation("bulk-submit"));
        assert!(set.is_permitted("Patient", SmartPermissions::READ));
        assert!(!set.has_system_wildcard());
    }

    #[test]
    fn test_raw_retained() {
        let set = ScopeSet::parse("system/Patient.rs system/bulk-submit");
        assert_eq!(set.raw().len(), 2);
    }

    #[test]
    fn test_has_system_scope_grants_for_system_wildcard() {
        assert!(ScopeSet::parse("system/*.r").has_system_scope(SmartPermissions::READ));
        assert!(ScopeSet::parse("system/*.rs").has_system_scope(SmartPermissions::READ));
        assert!(ScopeSet::parse("system/*.cruds").has_system_scope(SmartPermissions::READ));
        // The permission bit must actually be present.
        assert!(!ScopeSet::parse("system/*.cud").has_system_scope(SmartPermissions::READ));
    }

    #[test]
    fn test_has_system_scope_rejects_non_system_context() {
        // This is the exact class of token that leaks today: a broad, wildcard,
        // read-capable scope that is NOT system-context. It must be rejected.
        assert!(!ScopeSet::parse("patient/*.rs").has_system_scope(SmartPermissions::READ));
        assert!(!ScopeSet::parse("user/*.cruds").has_system_scope(SmartPermissions::READ));
    }

    #[test]
    fn test_has_system_scope_rejects_non_wildcard_system() {
        // System context but scoped to a single resource type is not system-wide.
        assert!(!ScopeSet::parse("system/Patient.rs").has_system_scope(SmartPermissions::READ));
    }

    #[test]
    fn access_is_full_for_system_and_user_scopes() {
        assert_eq!(
            ScopeSet::parse("system/*.rs").access("Observation", SmartPermissions::READ),
            Access::Full
        );
        assert_eq!(
            ScopeSet::parse("user/Observation.read")
                .access("Observation", SmartPermissions::SEARCH),
            Access::Full
        );
    }

    #[test]
    fn access_is_denied_without_a_covering_scope() {
        let set = ScopeSet::parse("patient/Patient.rs");
        assert_eq!(
            set.access("Observation", SmartPermissions::READ),
            Access::Denied
        );
        assert_eq!(
            set.access("Patient", SmartPermissions::CREATE),
            Access::Denied
        );
    }

    #[test]
    fn patient_scope_is_restricted_to_the_compartment() {
        let access = ScopeSet::parse("patient/*.rs").access("Observation", SmartPermissions::READ);
        assert_eq!(
            access,
            Access::Restricted(vec![Grant {
                patient_compartment: true,
                constraints: vec![]
            }])
        );
        assert!(access.needs_patient());
    }

    #[test]
    fn an_unrestricted_scope_wins_over_a_patient_scope() {
        let set = ScopeSet::parse("patient/*.rs user/Observation.rs");
        assert_eq!(
            set.access("Observation", SmartPermissions::READ),
            Access::Full
        );
        assert!(
            set.access("Condition", SmartPermissions::READ)
                .needs_patient()
        );
    }

    #[test]
    fn granular_scopes_are_or_ed_and_subsumed() {
        let set = ScopeSet::parse(
            "patient/Observation.rs?category=laboratory patient/Observation.rs?category=vital-signs",
        );
        match set.access("Observation", SmartPermissions::SEARCH) {
            Access::Restricted(grants) => assert_eq!(grants.len(), 2),
            other => panic!("expected restricted, got {other:?}"),
        }
        // A plain patient scope subsumes the granular ones.
        let set =
            ScopeSet::parse("patient/Observation.rs?category=laboratory patient/Observation.rs");
        assert_eq!(
            set.access("Observation", SmartPermissions::SEARCH),
            Access::Restricted(vec![Grant {
                patient_compartment: true,
                constraints: vec![]
            }])
        );
    }

    #[test]
    fn reads_everything_needs_an_unconstrained_wildcard_read() {
        assert!(ScopeSet::parse("system/*.rs").reads_everything());
        assert!(ScopeSet::parse("user/*.read").reads_everything());
        assert!(!ScopeSet::parse("patient/*.rs").reads_everything());
        assert!(!ScopeSet::parse("system/*.s").reads_everything());
        assert!(!ScopeSet::parse("system/Patient.rs").reads_everything());
    }

    #[test]
    fn test_patient_scope_is_not_an_unrestricted_grant() {
        let set = ScopeSet::parse("patient/*.rs");
        assert!(set.is_permitted("Observation", SmartPermissions::READ));
        assert!(!set.is_permitted_unrestricted("Observation", SmartPermissions::READ));
        assert!(set.is_permitted_by_patient_scope("Observation", SmartPermissions::READ));
        assert!(!set.is_permitted_by_patient_scope("Observation", SmartPermissions::CREATE));
    }

    #[test]
    fn test_user_and_system_scopes_are_unrestricted_grants() {
        for raw in ["user/Observation.rs", "system/*.r"] {
            let set = ScopeSet::parse(raw);
            assert!(
                set.is_permitted_unrestricted("Observation", SmartPermissions::READ),
                "{raw}"
            );
            assert!(
                !set.is_permitted_by_patient_scope("Observation", SmartPermissions::READ),
                "{raw}"
            );
        }
    }

    #[test]
    fn test_mixed_contexts_are_decided_per_resource_type() {
        let set = ScopeSet::parse("user/Observation.rs patient/Patient.rs");
        assert!(set.is_permitted_unrestricted("Observation", SmartPermissions::SEARCH));
        assert!(!set.is_permitted_unrestricted("Patient", SmartPermissions::SEARCH));
        assert!(set.is_permitted_by_patient_scope("Patient", SmartPermissions::SEARCH));
        assert!(!set.is_patient_only());
    }

    #[test]
    fn test_is_patient_only() {
        assert!(ScopeSet::parse("patient/*.rs patient/Patient.c").is_patient_only());
        assert!(ScopeSet::parse("openid patient/*.rs").is_patient_only());
        assert!(!ScopeSet::parse("patient/*.rs user/Patient.r").is_patient_only());
        // No resource scope at all is not "patient only" — it is nothing.
        assert!(!ScopeSet::parse("openid launch/patient").is_patient_only());
        assert!(!ScopeSet::empty().is_patient_only());
    }

    #[test]
    fn test_has_system_scope_finds_grant_among_many() {
        let set = ScopeSet::parse("patient/Patient.rs user/Observation.r system/*.r");
        assert!(set.has_system_scope(SmartPermissions::READ));
    }
}
