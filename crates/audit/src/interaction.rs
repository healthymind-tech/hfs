//! Classification of an HTTP request into a FHIR RESTful interaction.
//!
//! The audit trail records *what the client did* in FHIR terms — a `vread`, a
//! compartment search, an operation invocation — not merely which HTTP verb it
//! used. [`classify`] maps `(method, path)` to a [`RestInteraction`] (the codes
//! of `http://hl7.org/fhir/restful-interaction`) plus the resource coordinates
//! the path names, which is what the BALP profile selection and the entities of
//! the `AuditEvent` are built from.
//!
//! The path must already be stripped of any URL tenant prefix and of the
//! server's base path; the middleware takes care of both.

use crate::balp::AuditAction;

/// A FHIR RESTful interaction (`http://hl7.org/fhir/restful-interaction`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestInteraction {
    Read,
    Vread,
    Update,
    Patch,
    Delete,
    HistoryInstance,
    HistoryType,
    HistorySystem,
    Create,
    SearchType,
    SearchSystem,
    Capabilities,
    Transaction,
    Batch,
    /// An operation invocation (`$name`), with the operation name without `$`.
    Operation(String),
    /// A request on a non-FHIR endpoint of the server (bulk-data status and
    /// download, admin API, ...). Recorded, but under no BALP profile.
    Other,
}

impl RestInteraction {
    /// The `restful-interaction` code, or `None` for [`RestInteraction::Other`].
    pub fn code(&self) -> Option<&'static str> {
        Some(match self {
            Self::Read => "read",
            Self::Vread => "vread",
            Self::Update => "update",
            Self::Patch => "patch",
            Self::Delete => "delete",
            Self::HistoryInstance => "history-instance",
            Self::HistoryType => "history-type",
            Self::HistorySystem => "history-system",
            Self::Create => "create",
            Self::SearchType => "search-type",
            Self::SearchSystem => "search-system",
            Self::Capabilities => "capabilities",
            Self::Transaction => "transaction",
            Self::Batch => "batch",
            Self::Operation(_) => "operation",
            Self::Other => return None,
        })
    }

    /// The `AuditEvent.action` this interaction maps to.
    pub fn action(&self) -> AuditAction {
        match self {
            Self::Read | Self::Vread => AuditAction::Read,
            Self::Create => AuditAction::Create,
            Self::Update | Self::Patch => AuditAction::Update,
            Self::Delete => AuditAction::Delete,
            Self::HistoryInstance
            | Self::HistoryType
            | Self::HistorySystem
            | Self::SearchType
            | Self::SearchSystem => AuditAction::Query,
            Self::Capabilities
            | Self::Transaction
            | Self::Batch
            | Self::Operation(_)
            | Self::Other => AuditAction::Execute,
        }
    }

    /// Whether the interaction is query-shaped: it returns a set of resources
    /// chosen by criteria, and is recorded on the BALP Query pattern with the
    /// request carried in a query entity.
    pub fn is_query_like(&self) -> bool {
        matches!(
            self,
            Self::HistoryInstance
                | Self::HistoryType
                | Self::HistorySystem
                | Self::SearchType
                | Self::SearchSystem
                | Self::Operation(_)
        )
    }
}

/// A classified request: the interaction and the resource coordinates of its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedRequest {
    pub interaction: RestInteraction,
    pub resource_type: Option<String>,
    pub resource_id: Option<String>,
    pub version_id: Option<String>,
    /// Compartment of a compartment search (`/Patient/123/Observation`).
    pub compartment: Option<(String, String)>,
}

impl ClassifiedRequest {
    fn new(interaction: RestInteraction) -> Self {
        Self {
            interaction,
            resource_type: None,
            resource_id: None,
            version_id: None,
            compartment: None,
        }
    }

    fn typed(mut self, rt: &str) -> Self {
        self.resource_type = Some(rt.to_string());
        self
    }

    fn id(mut self, id: &str) -> Self {
        self.resource_id = Some(id.to_string());
        self
    }
}

/// True for a path segment that names a FHIR resource type.
///
/// Resource types are PascalCase ASCII identifiers; nothing else on the FHIR
/// surface (ids, `_history`, `$op`, `export-status`) starts with an uppercase
/// letter.
pub fn is_resource_type(segment: &str) -> bool {
    let mut chars = segment.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase()) && chars.all(|c| c.is_ascii_alphanumeric())
}

/// Classify a request into a FHIR RESTful interaction.
///
/// `path` excludes the query string and any tenant prefix.
pub fn classify(method: &str, path: &str) -> ClassifiedRequest {
    let method = method.to_ascii_uppercase();
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let get = matches!(method.as_str(), "GET" | "HEAD");

    // An operation is named by the last segment, whatever the verb.
    if let Some(last) = segs.last()
        && let Some(op) = last.strip_prefix('$')
    {
        let mut c = ClassifiedRequest::new(RestInteraction::Operation(op.to_string()));
        if let Some(first) = segs.first()
            && is_resource_type(first)
        {
            c = c.typed(first);
            if segs.len() == 3 {
                c = c.id(segs[1]);
            }
        }
        return c;
    }

    match segs.as_slice() {
        [] => match method.as_str() {
            // The envelope's type (batch or transaction) is in the body; the
            // handler reports it. Batch is the conservative default.
            "POST" => ClassifiedRequest::new(RestInteraction::Batch),
            _ => ClassifiedRequest::new(RestInteraction::SearchSystem),
        },
        ["metadata"] => ClassifiedRequest::new(RestInteraction::Capabilities),
        ["_history"] if get => ClassifiedRequest::new(RestInteraction::HistorySystem),
        ["_search"] => ClassifiedRequest::new(RestInteraction::SearchSystem),
        [rt] if is_resource_type(rt) => {
            let interaction = match method.as_str() {
                "GET" | "HEAD" => RestInteraction::SearchType,
                "POST" => RestInteraction::Create,
                // Conditional update / patch / delete: the target id is
                // resolved by the handler and reported through the response.
                "PUT" => RestInteraction::Update,
                "PATCH" => RestInteraction::Patch,
                "DELETE" => RestInteraction::Delete,
                _ => RestInteraction::Other,
            };
            ClassifiedRequest::new(interaction).typed(rt)
        }
        [rt, "_search"] if is_resource_type(rt) => {
            ClassifiedRequest::new(RestInteraction::SearchType).typed(rt)
        }
        [rt, "_history"] if is_resource_type(rt) && get => {
            ClassifiedRequest::new(RestInteraction::HistoryType).typed(rt)
        }
        [rt, id] if is_resource_type(rt) => {
            let interaction = match method.as_str() {
                "GET" | "HEAD" => RestInteraction::Read,
                "PUT" => RestInteraction::Update,
                "PATCH" => RestInteraction::Patch,
                "DELETE" => RestInteraction::Delete,
                _ => RestInteraction::Other,
            };
            ClassifiedRequest::new(interaction).typed(rt).id(id)
        }
        [rt, id, "_history"] if is_resource_type(rt) => {
            let interaction = if get {
                RestInteraction::HistoryInstance
            } else if method == "DELETE" {
                RestInteraction::Delete
            } else {
                RestInteraction::Other
            };
            ClassifiedRequest::new(interaction).typed(rt).id(id)
        }
        [rt, id, "_history", vid] if is_resource_type(rt) => {
            let interaction = if get {
                RestInteraction::Vread
            } else if method == "DELETE" {
                RestInteraction::Delete
            } else {
                RestInteraction::Other
            };
            let mut c = ClassifiedRequest::new(interaction).typed(rt).id(id);
            c.version_id = Some((*vid).to_string());
            c
        }
        [ct, cid, target] if is_resource_type(ct) && get => {
            let mut c = ClassifiedRequest::new(RestInteraction::SearchType);
            if is_resource_type(target) {
                c = c.typed(target);
            }
            c.compartment = Some(((*ct).to_string(), (*cid).to_string()));
            c
        }
        _ => ClassifiedRequest::new(RestInteraction::Other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i(method: &str, path: &str) -> RestInteraction {
        classify(method, path).interaction
    }

    #[test]
    fn instance_level_interactions() {
        assert_eq!(i("GET", "/Patient/1"), RestInteraction::Read);
        assert_eq!(i("HEAD", "/Patient/1"), RestInteraction::Read);
        assert_eq!(i("PUT", "/Patient/1"), RestInteraction::Update);
        assert_eq!(i("PATCH", "/Patient/1"), RestInteraction::Patch);
        assert_eq!(i("DELETE", "/Patient/1"), RestInteraction::Delete);
        let c = classify("GET", "/Observation/o1/_history/3");
        assert_eq!(c.interaction, RestInteraction::Vread);
        assert_eq!(c.resource_type.as_deref(), Some("Observation"));
        assert_eq!(c.resource_id.as_deref(), Some("o1"));
        assert_eq!(c.version_id.as_deref(), Some("3"));
        assert_eq!(
            i("GET", "/Patient/1/_history"),
            RestInteraction::HistoryInstance
        );
    }

    #[test]
    fn type_and_system_level_interactions() {
        assert_eq!(i("GET", "/Patient"), RestInteraction::SearchType);
        assert_eq!(i("POST", "/Patient/_search"), RestInteraction::SearchType);
        assert_eq!(i("POST", "/Patient"), RestInteraction::Create);
        assert_eq!(i("PUT", "/Patient"), RestInteraction::Update);
        assert_eq!(i("DELETE", "/Patient"), RestInteraction::Delete);
        assert_eq!(i("GET", "/Patient/_history"), RestInteraction::HistoryType);
        assert_eq!(i("GET", "/_history"), RestInteraction::HistorySystem);
        assert_eq!(i("GET", "/"), RestInteraction::SearchSystem);
        assert_eq!(i("POST", "/"), RestInteraction::Batch);
        assert_eq!(i("GET", "/metadata"), RestInteraction::Capabilities);
    }

    #[test]
    fn compartment_search_names_target_type_and_compartment() {
        let c = classify("GET", "/Patient/p1/Observation");
        assert_eq!(c.interaction, RestInteraction::SearchType);
        assert_eq!(c.resource_type.as_deref(), Some("Observation"));
        assert_eq!(c.resource_id, None);
        assert_eq!(
            c.compartment,
            Some(("Patient".to_string(), "p1".to_string()))
        );
    }

    #[test]
    fn operations_at_every_level() {
        let c = classify("GET", "/Patient/p1/$everything");
        assert_eq!(
            c.interaction,
            RestInteraction::Operation("everything".into())
        );
        assert_eq!(c.resource_type.as_deref(), Some("Patient"));
        assert_eq!(c.resource_id.as_deref(), Some("p1"));

        let c = classify("POST", "/Patient/$validate");
        assert_eq!(c.interaction, RestInteraction::Operation("validate".into()));
        assert_eq!(c.resource_id, None);

        let c = classify("GET", "/$export");
        assert_eq!(c.interaction, RestInteraction::Operation("export".into()));
        assert_eq!(c.resource_type, None);

        // A destructive operation is never mistaken for a delete or create.
        assert_eq!(
            i("DELETE", "/Patient/p1/$purge"),
            RestInteraction::Operation("purge".into())
        );
        assert_eq!(
            i("POST", "/Patient/$purge"),
            RestInteraction::Operation("purge".into())
        );
    }

    #[test]
    fn dollar_inside_an_id_is_not_an_operation() {
        assert_eq!(i("GET", "/Patient/a$b"), RestInteraction::Read);
    }

    #[test]
    fn non_fhir_endpoints_are_other() {
        assert_eq!(i("GET", "/export-status/abc"), RestInteraction::Other);
        assert_eq!(i("GET", "/export-file/abc/1"), RestInteraction::Other);
        assert_eq!(i("POST", "/admin/packages"), RestInteraction::Other);
    }

    #[test]
    fn actions_follow_interactions() {
        assert_eq!(RestInteraction::Vread.action(), AuditAction::Read);
        assert_eq!(RestInteraction::Patch.action(), AuditAction::Update);
        assert_eq!(RestInteraction::HistoryType.action(), AuditAction::Query);
        assert_eq!(
            RestInteraction::Operation("everything".into()).action(),
            AuditAction::Execute
        );
        assert_eq!(RestInteraction::Batch.action(), AuditAction::Execute);
    }
}
