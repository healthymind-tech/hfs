//! Scope algebra of the authorization server: classify requested scopes,
//! intersect them with what a client (and the user's consent) allows, and
//! check refresh-time narrowing.
//!
//! The *resource server* side (enforcing granted scopes on FHIR requests)
//! stays in `helios-auth::scope`; this module only decides what to grant.
//!
//! Grammar (SMART App Launch 2.2, "Scopes and Launch Context"):
//! * resource: `(patient|user|system)/(Type|*).(perms)[?param=value&…]`,
//!   perms v2 = ordered subset of `cruds`, v1 = `read` | `write` | `*`
//!   (mapped to `rs`, `cud`, `cruds` for coverage checks).
//! * launch: `launch`, `launch/patient`, `launch/encounter`,
//!   `launch/<type>[?role=<uri>]`.
//! * identity: `openid`, `fhirUser`, `profile`, `email`.
//! * refresh: `offline_access`, `online_access`.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

/// The compartment / context prefix of a resource scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeContext {
    Patient,
    User,
    System,
}

impl ScopeContext {
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeContext::Patient => "patient",
            ScopeContext::User => "user",
            ScopeContext::System => "system",
        }
    }
}

/// `c r u d s` bit set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Perms(u8);

impl Perms {
    pub const C: u8 = 1;
    pub const R: u8 = 2;
    pub const U: u8 = 4;
    pub const D: u8 = 8;
    pub const S: u8 = 16;
    pub const ALL: Perms = Perms(31);
    /// `r` alone.
    pub const READ: Perms = Perms(Self::R);

    /// v2 letters in canonical order; `None` if out of order, repeated,
    /// empty or containing other letters.
    pub fn parse_v2(s: &str) -> Option<Perms> {
        let order = [
            (b'c', Self::C),
            (b'r', Self::R),
            (b'u', Self::U),
            (b'd', Self::D),
            (b's', Self::S),
        ];
        let mut idx = 0;
        let mut bits = 0u8;
        for b in s.bytes() {
            let pos = order[idx..].iter().position(|(c, _)| *c == b)?;
            bits |= order[idx + pos].1;
            idx += pos + 1;
        }
        (bits != 0).then_some(Perms(bits))
    }

    pub fn contains(self, other: Perms) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn letters(self) -> String {
        [
            (Self::C, 'c'),
            (Self::R, 'r'),
            (Self::U, 'u'),
            (Self::D, 'd'),
            (Self::S, 's'),
        ]
        .iter()
        .filter(|(b, _)| self.0 & b != 0)
        .map(|(_, c)| *c)
        .collect()
    }
}

/// A parsed resource scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourceScope {
    pub context: ScopeContext,
    /// Resource type or `*`.
    pub resource_type: String,
    pub perms: Perms,
    /// Whether it was written in v1 syntax (`.read` / `.write` / `.*`).
    pub v1: bool,
    /// Granular query (`category=…`), raw, without `?`.
    pub query: Option<String>,
}

impl ResourceScope {
    /// Does `self` (an allowed pattern) cover `req`?
    pub fn covers(&self, req: &ResourceScope) -> bool {
        self.context == req.context
            && (self.resource_type == "*" || self.resource_type == req.resource_type)
            && self.perms.contains(req.perms)
            && match (&self.query, &req.query) {
                (None, _) => true,
                (Some(a), Some(b)) => a == b,
                (Some(_), None) => false,
            }
    }
}

impl fmt::Display for ResourceScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{}.{}",
            self.context.as_str(),
            self.resource_type,
            self.perms.letters()
        )?;
        if let Some(q) = &self.query {
            write!(f, "?{q}")?;
        }
        Ok(())
    }
}

/// One requested scope token, classified.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Scope {
    Resource(ResourceScope),
    /// `launch` (EHR launch).
    Launch,
    /// `launch/<type>[?role=…]` (standalone context request), type lower case as sent.
    LaunchContext {
        resource: String,
        role: Option<String>,
    },
    OpenId,
    FhirUser,
    Profile,
    Email,
    OfflineAccess,
    OnlineAccess,
    /// Anything else (custom scopes; granted only if explicitly allowed).
    Other(String),
}

fn is_resource_type(s: &str) -> bool {
    s == "*"
        || (s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && s.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Classifies one scope token.
pub fn parse_scope(tok: &str) -> Scope {
    match tok {
        "launch" => return Scope::Launch,
        "openid" => return Scope::OpenId,
        "fhirUser" => return Scope::FhirUser,
        "profile" => return Scope::Profile,
        "email" => return Scope::Email,
        "offline_access" => return Scope::OfflineAccess,
        "online_access" => return Scope::OnlineAccess,
        _ => {}
    }
    if let Some(rest) = tok.strip_prefix("launch/") {
        let (resource, role) = match rest.split_once("?role=") {
            Some((r, role)) => (r, Some(role.to_string())),
            None => (rest, None),
        };
        if !resource.is_empty() && resource.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Scope::LaunchContext {
                resource: resource.to_string(),
                role,
            };
        }
        return Scope::Other(tok.to_string());
    }
    let Some((ctx, rest)) = tok.split_once('/') else {
        return Scope::Other(tok.to_string());
    };
    let context = match ctx {
        "patient" => ScopeContext::Patient,
        "user" => ScopeContext::User,
        "system" => ScopeContext::System,
        _ => return Scope::Other(tok.to_string()),
    };
    let (body, query) = match rest.split_once('?') {
        Some((b, q)) if !q.is_empty() => (b, Some(q.to_string())),
        Some(_) => return Scope::Other(tok.to_string()),
        None => (rest, None),
    };
    let Some((rt, perm)) = body.split_once('.') else {
        return Scope::Other(tok.to_string());
    };
    if !is_resource_type(rt) {
        return Scope::Other(tok.to_string());
    }
    let (perms, v1) = match perm {
        "read" => (Perms(Perms::R | Perms::S), true),
        "write" => (Perms(Perms::C | Perms::U | Perms::D), true),
        "*" => (Perms::ALL, true),
        p => match Perms::parse_v2(p) {
            Some(p) => (p, false),
            None => return Scope::Other(tok.to_string()),
        },
    };
    if v1 && query.is_some() {
        return Scope::Other(tok.to_string());
    }
    Scope::Resource(ResourceScope {
        context,
        resource_type: rt.to_string(),
        perms,
        v1,
        query,
    })
}

/// Splits a `scope` parameter (RFC 6749 §3.3: space-delimited, case
/// sensitive), dropping duplicates while keeping order.
pub fn split_scopes(s: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    s.split(' ')
        .filter(|t| !t.is_empty())
        .filter(|t| seen.insert(t.to_string()))
        .map(str::to_string)
        .collect()
}

/// What a client may be granted (registration-time policy, possibly
/// narrowed further by the realm and by user consent).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScopePolicy {
    /// Allowed scope tokens, including resource patterns such as
    /// `patient/*.rs` or `user/Observation.cruds?category=laboratory`.
    pub allowed: Vec<String>,
    /// Grant v1 tokens (`patient/*.read`) by mapping to v2 coverage.
    pub accept_v1: bool,
}

/// Result of [`grant`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grant {
    /// Granted tokens, as requested (v1 stays v1 so the app recognises them).
    pub granted: Vec<String>,
    /// Refused tokens (not allowed or not understood). The server MAY grant
    /// less than requested (SMART: "scope … may differ from the requested").
    pub dropped: Vec<String>,
}

impl Grant {
    pub fn scope_string(&self) -> String {
        self.granted.join(" ")
    }

    pub fn has(&self, s: &str) -> bool {
        self.granted.iter().any(|g| g == s)
    }
}

/// Intersects the requested scopes with a policy.
pub fn grant(requested: &[String], policy: &ScopePolicy) -> Grant {
    let allowed_res: Vec<ResourceScope> = policy
        .allowed
        .iter()
        .filter_map(|a| match parse_scope(a) {
            Scope::Resource(r) => Some(r),
            _ => None,
        })
        .collect();
    let mut g = Grant::default();
    for tok in requested {
        let ok = match parse_scope(tok) {
            Scope::Resource(r) => {
                (!r.v1 || policy.accept_v1) && allowed_res.iter().any(|a| a.covers(&r))
            }
            Scope::Other(_)
            | Scope::Launch
            | Scope::LaunchContext { .. }
            | Scope::OpenId
            | Scope::FhirUser
            | Scope::Profile
            | Scope::Email
            | Scope::OfflineAccess
            | Scope::OnlineAccess => policy.allowed.iter().any(|a| a == tok),
        };
        if ok {
            g.granted.push(tok.clone());
        } else {
            g.dropped.push(tok.clone());
        }
    }
    g
}

/// Refresh-time check (RFC 6749 §6, SMART "Refresh access token"): every
/// requested token must be covered by the original grant.
pub fn is_narrowing(requested: &[String], original: &[String]) -> bool {
    let orig_res: Vec<ResourceScope> = original
        .iter()
        .filter_map(|a| match parse_scope(a) {
            Scope::Resource(r) => Some(r),
            _ => None,
        })
        .collect();
    requested.iter().all(|tok| match parse_scope(tok) {
        Scope::Resource(r) => orig_res.iter().any(|a| a.covers(&r)),
        _ => original.iter().any(|o| o == tok),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        split_scopes(s)
    }

    #[test]
    fn parse_v2_and_v1() {
        let Scope::Resource(r) = parse_scope("patient/Observation.rs?category=laboratory") else {
            panic!()
        };
        assert_eq!(r.context, ScopeContext::Patient);
        assert_eq!(r.perms.letters(), "rs");
        assert_eq!(r.query.as_deref(), Some("category=laboratory"));
        assert_eq!(r.to_string(), "patient/Observation.rs?category=laboratory");
        let Scope::Resource(r1) = parse_scope("user/*.read") else {
            panic!()
        };
        assert!(r1.v1);
        assert_eq!(r1.perms.letters(), "rs");
        assert!(matches!(
            parse_scope("patient/Observation.sr"),
            Scope::Other(_)
        ));
        assert!(matches!(
            parse_scope("patient/Observation.rr"),
            Scope::Other(_)
        ));
        assert!(matches!(
            parse_scope("patient/observation.rs"),
            Scope::Other(_)
        ));
        assert!(matches!(parse_scope("patient/*.read?x=1"), Scope::Other(_)));
        assert!(matches!(
            parse_scope("launch/patient"),
            Scope::LaunchContext { .. }
        ));
        assert_eq!(
            parse_scope("launch/encounter?role=http://x/y"),
            Scope::LaunchContext {
                resource: "encounter".into(),
                role: Some("http://x/y".into())
            }
        );
    }

    #[test]
    fn grant_intersects() {
        let policy = ScopePolicy {
            allowed: v(
                "openid fhirUser launch/patient offline_access patient/*.rs user/Observation.cruds?category=laboratory",
            ),
            accept_v1: true,
        };
        let g = grant(
            &v(
                "openid fhirUser launch/patient patient/Observation.rs patient/Patient.read patient/Observation.c user/Observation.r?category=laboratory user/Observation.r online_access junk",
            ),
            &policy,
        );
        assert_eq!(
            g.granted,
            v(
                "openid fhirUser launch/patient patient/Observation.rs patient/Patient.read user/Observation.r?category=laboratory"
            )
        );
        assert_eq!(
            g.dropped,
            v("patient/Observation.c user/Observation.r online_access junk")
        );
    }

    #[test]
    fn v1_refused_without_accept_v1() {
        let policy = ScopePolicy {
            allowed: v("patient/*.cruds"),
            accept_v1: false,
        };
        assert!(grant(&v("patient/*.read"), &policy).granted.is_empty());
    }

    #[test]
    fn refresh_narrowing() {
        let orig = v("openid patient/*.rs offline_access");
        assert!(is_narrowing(&v("patient/Observation.r"), &orig));
        assert!(is_narrowing(&v("openid patient/*.rs"), &orig));
        assert!(!is_narrowing(&v("patient/*.cruds"), &orig));
        assert!(!is_narrowing(&v("user/*.rs"), &orig));
        assert!(!is_narrowing(&v("fhirUser"), &orig));
    }

    #[test]
    fn split_dedups() {
        assert_eq!(
            split_scopes("a  b a "),
            vec!["a".to_string(), "b".to_string()]
        );
    }
}
