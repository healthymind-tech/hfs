use super::permissions::SmartPermissions;
use std::fmt;

/// The access context of a SMART scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ScopeContext {
    /// System-level access (backend services, no user context).
    System,
    /// User-level access (scoped to the authenticated user).
    User,
    /// Patient-level access (scoped to a specific patient).
    Patient,
}

impl fmt::Display for ScopeContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScopeContext::System => write!(f, "system"),
            ScopeContext::User => write!(f, "user"),
            ScopeContext::Patient => write!(f, "patient"),
        }
    }
}

/// The resource type specifier in a SMART scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResourceTypeSpec {
    /// A specific FHIR resource type (e.g., "Patient").
    Specific(String),
    /// Wildcard — applies to all resource types.
    Wildcard,
}

impl fmt::Display for ResourceTypeSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResourceTypeSpec::Specific(t) => write!(f, "{}", t),
            ResourceTypeSpec::Wildcard => write!(f, "*"),
        }
    }
}

/// A single parsed SMART v2 scope.
///
/// Follows the format: `context/resourceType.permissions`
///
/// Examples:
/// - `system/Patient.rs` — system-level read+search on Patient
/// - `system/*.cruds` — system-level full CRUD+search on all types
/// - `user/Observation.r` — user-level read on Observation
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SmartScope {
    /// The access context.
    pub context: ScopeContext,
    /// The resource type or wildcard.
    pub resource_type: ResourceTypeSpec,
    /// The granted permissions.
    pub permissions: SmartPermissions,
    /// SMART v2 granular-scope constraints (`?param=value&...`), ANDed.
    ///
    /// Empty for an ordinary scope. A scope such as
    /// `patient/Observation.rs?category=http://terminology.hl7.org/CodeSystem/observation-category|laboratory`
    /// only grants access to resources that match every constraint.
    pub constraints: Vec<(String, String)>,
}

impl SmartScope {
    /// Parse a single SMART v2 scope string.
    ///
    /// Expected format: `context/resourceType.permissions`
    /// Returns `None` if the string is malformed.
    pub fn parse(scope_str: &str) -> Option<Self> {
        // Split into context and rest: "system/Patient.rs"
        let (context_str, rest) = scope_str.split_once('/')?;

        let context = match context_str {
            "system" => ScopeContext::System,
            "user" => ScopeContext::User,
            "patient" => ScopeContext::Patient,
            _ => return None,
        };

        // A SMART v2 granular scope carries a query: "Observation.rs?category=...".
        let (rest, query) = match rest.split_once('?') {
            Some((head, query)) => (head, Some(query)),
            None => (rest, None),
        };

        // Split rest into resource type and permissions: "Patient.rs"
        let (resource_str, perm_str) = rest.split_once('.')?;

        if resource_str.is_empty() || perm_str.is_empty() {
            return None;
        }
        if resource_str != "*" && !resource_str.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }

        let resource_type = if resource_str == "*" {
            ResourceTypeSpec::Wildcard
        } else {
            ResourceTypeSpec::Specific(resource_str.to_string())
        };

        // SMART v1 (`read`, `write`, `*`) or SMART v2 (`cruds` letters).
        let permissions = match perm_str {
            "read" => SmartPermissions::READ | SmartPermissions::SEARCH,
            "write" => {
                SmartPermissions::CREATE | SmartPermissions::UPDATE | SmartPermissions::DELETE
            }
            "*" => SmartPermissions::all(),
            other => SmartPermissions::from_permission_str(other)?,
        };

        let constraints = match query {
            None => Vec::new(),
            Some(query) => {
                let pairs: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                // A `?` with nothing usable after it is malformed, not unconstrained:
                // reading it as unconstrained would widen the grant.
                if pairs.is_empty() || pairs.iter().any(|(k, v)| k.is_empty() || v.is_empty()) {
                    return None;
                }
                pairs
            }
        };

        Some(SmartScope {
            context,
            resource_type,
            permissions,
            constraints,
        })
    }

    /// Whether the scope is a SMART v2 granular scope (carries constraints).
    pub fn is_constrained(&self) -> bool {
        !self.constraints.is_empty()
    }

    /// Whether this scope's resource type and permissions cover the request,
    /// ignoring context and constraints.
    pub fn covers(&self, resource_type: &str, permission: SmartPermissions) -> bool {
        let type_matches = match &self.resource_type {
            ResourceTypeSpec::Wildcard => true,
            ResourceTypeSpec::Specific(t) => t == resource_type,
        };
        type_matches && self.permissions.contains(permission)
    }

    /// Check if this scope grants the given permission on the given resource type.
    ///
    /// This is the coarse, type-level answer: a patient-context or granular
    /// scope still "permits" here, and the restriction it carries is applied
    /// by [`ScopeSet::access`](super::ScopeSet::access) callers.
    pub fn permits(&self, resource_type: &str, permission: SmartPermissions) -> bool {
        self.covers(resource_type, permission)
    }
}

impl fmt::Display for SmartScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{}.{}",
            self.context, self.resource_type, self.permissions
        )?;
        if !self.constraints.is_empty() {
            let query: String = form_urlencoded::Serializer::new(String::new())
                .extend_pairs(self.constraints.iter())
                .finish();
            write!(f, "?{query}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_system_patient_rs() {
        let scope = SmartScope::parse("system/Patient.rs").unwrap();
        assert_eq!(scope.context, ScopeContext::System);
        assert_eq!(
            scope.resource_type,
            ResourceTypeSpec::Specific("Patient".to_string())
        );
        assert_eq!(
            scope.permissions,
            SmartPermissions::READ | SmartPermissions::SEARCH
        );
    }

    #[test]
    fn test_parse_wildcard_cruds() {
        let scope = SmartScope::parse("system/*.cruds").unwrap();
        assert_eq!(scope.context, ScopeContext::System);
        assert_eq!(scope.resource_type, ResourceTypeSpec::Wildcard);
        assert!(scope.permissions.contains(SmartPermissions::CREATE));
        assert!(scope.permissions.contains(SmartPermissions::READ));
        assert!(scope.permissions.contains(SmartPermissions::UPDATE));
        assert!(scope.permissions.contains(SmartPermissions::DELETE));
        assert!(scope.permissions.contains(SmartPermissions::SEARCH));
    }

    #[test]
    fn test_parse_user_context() {
        let scope = SmartScope::parse("user/Observation.r").unwrap();
        assert_eq!(scope.context, ScopeContext::User);
        assert_eq!(
            scope.resource_type,
            ResourceTypeSpec::Specific("Observation".to_string())
        );
        assert_eq!(scope.permissions, SmartPermissions::READ);
    }

    #[test]
    fn test_parse_patient_context() {
        let scope = SmartScope::parse("patient/MedicationRequest.crud").unwrap();
        assert_eq!(scope.context, ScopeContext::Patient);
    }

    #[test]
    fn test_parse_invalid_scopes() {
        assert!(SmartScope::parse("").is_none());
        assert!(SmartScope::parse("system").is_none());
        assert!(SmartScope::parse("system/").is_none());
        assert!(SmartScope::parse("system/Patient").is_none());
        assert!(SmartScope::parse("system/.rs").is_none());
        assert!(SmartScope::parse("system/Patient.").is_none());
        assert!(SmartScope::parse("system/Patient.xyz").is_none());
        assert!(SmartScope::parse("unknown/Patient.rs").is_none());
        assert!(SmartScope::parse("foo/bar/baz").is_none());
    }

    #[test]
    fn test_permits() {
        let scope = SmartScope::parse("system/Patient.rs").unwrap();
        assert!(scope.permits("Patient", SmartPermissions::READ));
        assert!(scope.permits("Patient", SmartPermissions::SEARCH));
        assert!(!scope.permits("Patient", SmartPermissions::CREATE));
        assert!(!scope.permits("Observation", SmartPermissions::READ));
    }

    #[test]
    fn test_permits_wildcard() {
        let scope = SmartScope::parse("system/*.rs").unwrap();
        assert!(scope.permits("Patient", SmartPermissions::READ));
        assert!(scope.permits("Observation", SmartPermissions::SEARCH));
        assert!(!scope.permits("Patient", SmartPermissions::DELETE));
    }

    #[test]
    fn smart_v1_read_is_read_and_search() {
        let scope = SmartScope::parse("patient/Observation.read").unwrap();
        assert_eq!(scope.context, ScopeContext::Patient);
        assert_eq!(
            scope.permissions,
            SmartPermissions::READ | SmartPermissions::SEARCH
        );
    }

    #[test]
    fn smart_v1_write_is_create_update_delete() {
        let scope = SmartScope::parse("user/Observation.write").unwrap();
        assert_eq!(
            scope.permissions,
            SmartPermissions::CREATE | SmartPermissions::UPDATE | SmartPermissions::DELETE
        );
        assert!(!scope.permissions.contains(SmartPermissions::READ));
    }

    #[test]
    fn smart_v1_star_is_everything() {
        let scope = SmartScope::parse("user/*.*").unwrap();
        assert_eq!(scope.resource_type, ResourceTypeSpec::Wildcard);
        assert_eq!(scope.permissions, SmartPermissions::all());
    }

    #[test]
    fn granular_scope_parses_constraints() {
        let scope = SmartScope::parse(
            "patient/Observation.rs?category=http://terminology.hl7.org/CodeSystem/observation-category|laboratory",
        )
        .unwrap();
        assert_eq!(
            scope.constraints,
            vec![(
                "category".to_string(),
                "http://terminology.hl7.org/CodeSystem/observation-category|laboratory".to_string()
            )]
        );
        assert!(scope.is_constrained());
        assert!(scope.covers("Observation", SmartPermissions::READ));
    }

    #[test]
    fn granular_scope_without_constraints_is_malformed() {
        assert!(SmartScope::parse("patient/Observation.rs?").is_none());
        assert!(SmartScope::parse("patient/Observation.rs?category=").is_none());
        assert!(SmartScope::parse("patient/Observation.rs?=x").is_none());
    }

    #[test]
    fn resource_type_must_be_a_name() {
        assert!(SmartScope::parse("system/Pat-ient.rs").is_none());
        assert!(SmartScope::parse("system/../x.rs").is_none());
    }

    #[test]
    fn granular_display_roundtrip() {
        let original = "patient/Observation.rs?category=laboratory";
        let scope = SmartScope::parse(original).unwrap();
        assert_eq!(format!("{}", scope), original);
    }

    #[test]
    fn test_display_roundtrip() {
        let original = "system/Patient.rs";
        let scope = SmartScope::parse(original).unwrap();
        assert_eq!(format!("{}", scope), original);
    }
}
