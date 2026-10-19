//! The media type code system, BCP 13 (`urn:ietf:bcp:13`).
//!
//! Media types are a grammar, not a list: any well-formed
//! `type/subtype[; parameter=value]*` is a code. HTS answers for it without
//! stored content:
//!
//! * **Value sets** over it may enumerate codes, or filter on `base`
//!   (`= type` or `= type/subtype`, parameters ignored) and `registered`
//!   (whether `type/subtype` is in the IANA Media Types registry, a copy of
//!   which is compiled in). A value set taking the whole system, a `base`
//!   filter alone or `registered = false` cannot be expanded (the set is
//!   unbounded); `registered = true` can, and is marked unclosed because
//!   every registered type also has unboundedly many parameterised forms.
//! * **Subsumption**: type and subtype have no hierarchy; a parameter
//!   narrows a media type unless it only states the parameter's default
//!   (bare `text/plain` *is* `charset=us-ascii; format=fixed`), and a
//!   parameter whose meaning is unknown — or a `charset` of a text type
//!   whose default is unknown (RFC 6657 §3) — cannot be judged unless both
//!   codes carry it identically.

use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

use serde_json::Value;

use crate::types::SubsumptionOutcome;

/// The code system.
pub const SYSTEM: &str = "urn:ietf:bcp:13";

static REGISTRY: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    include_str!("data/iana-media-types.txt")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
});

/// The registered media types (`type/subtype`), sorted.
pub fn registered() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = REGISTRY.iter().copied().collect();
    v.sort_unstable();
    v
}

/// A parsed media type: lower-cased type, subtype and parameter names;
/// `charset` values lower-cased too (they are case-insensitive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaType {
    pub ty: String,
    pub sub: String,
    pub params: BTreeMap<String, String>,
}

impl MediaType {
    /// `type/subtype`.
    pub fn essence(&self) -> String {
        format!("{}/{}", self.ty, self.sub)
    }

    /// Whether `type/subtype` is in the IANA registry.
    pub fn is_registered(&self) -> bool {
        REGISTRY.contains(self.essence().as_str())
    }
}

fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '!' | '#'
                        | '$'
                        | '&'
                        | '-'
                        | '^'
                        | '_'
                        | '.'
                        | '+'
                        | '%'
                        | '*'
                        | '\''
                        | '`'
                        | '|'
                        | '~'
                )
        })
}

/// Parse a media type code; `None` when it is not well formed.
pub fn parse(code: &str) -> Option<MediaType> {
    let mut parts = code.split(';');
    let essence = parts.next()?.trim();
    let (ty, sub) = essence.split_once('/')?;
    if !is_token(ty) || !is_token(sub) {
        return None;
    }
    let mut params = BTreeMap::new();
    for p in parts {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        let (name, value) = p.split_once('=')?;
        let name = name.trim().to_ascii_lowercase();
        let mut value = value.trim();
        if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
            value = &value[1..value.len() - 1];
        } else if !is_token(value) {
            return None;
        }
        if !is_token(&name) {
            return None;
        }
        let value = if name == "charset" {
            value.to_ascii_lowercase()
        } else {
            value.to_string()
        };
        params.insert(name, value);
    }
    Some(MediaType {
        ty: ty.to_ascii_lowercase(),
        sub: sub.to_ascii_lowercase(),
        params,
    })
}

/// What HTS knows about parameter `name` of media type `mt`:
/// `Some(Some(default))` — known, with that default; `Some(None)` — known,
/// no default (it narrows); `None` — unknown.
fn param_knowledge(mt: &MediaType, name: &str) -> Option<Option<&'static str>> {
    match (mt.ty.as_str(), mt.sub.as_str(), name) {
        // RFC 6657 §4: text/plain keeps US-ASCII; RFC 3676 §4: format and
        // delsp default to fixed / no.
        ("text", "plain", "charset") => Some(Some("us-ascii")),
        ("text", "plain", "format") => Some(Some("fixed")),
        ("text", "plain", "delsp") => Some(Some("no")),
        // RFC 6350: vCard is UTF-8; `version` has no default.
        ("text", "vcard", "charset") => Some(Some("utf-8")),
        ("text", "vcard", "version") => Some(None),
        // RFC 5545: iCalendar is UTF-8; `method`, `component` narrow.
        ("text", "calendar", "charset") => Some(Some("utf-8")),
        ("text", "calendar", "method" | "component") => Some(None),
        // Text types whose registrations make the charset part of the
        // content (no default to assume): a stated charset narrows.
        ("text", "html" | "xml" | "markdown" | "css" | "javascript", "charset") => Some(None),
        // Any other text/* type: RFC 6657 §3 — the default is whatever its
        // registration says, which HTS does not know.
        ("text", _, "charset") => None,
        // Non-text types carrying a charset (application/xml, …): RFC 7303
        // §3.2 — no default, so a stated charset narrows.
        (_, _, "charset") => Some(None),
        (_, _, "fhirversion") => Some(None),
        _ => None,
    }
}

/// The relationship of `a` to `b` (`Subsumes` = `a` is the wider), or the
/// parameter that makes it impossible to judge.
pub fn subsumes(a: &MediaType, b: &MediaType) -> Result<SubsumptionOutcome, String> {
    if a.ty != b.ty || a.sub != b.sub {
        return Ok(SubsumptionOutcome::NotSubsumed);
    }
    let names: Vec<&String> = a.params.keys().chain(b.params.keys()).collect();
    let mut a_full: BTreeMap<String, String> = BTreeMap::new();
    let mut b_full: BTreeMap<String, String> = BTreeMap::new();
    for name in names {
        let (va, vb) = (a.params.get(name), b.params.get(name));
        match param_knowledge(a, name) {
            // Unknown: harmless only when both carry the same value.
            None => {
                if va != vb {
                    return Err(name.clone());
                }
            }
            Some(default) => {
                if let Some(v) = va.map(String::as_str).or(default) {
                    a_full.insert(name.clone(), v.to_string());
                }
                if let Some(v) = vb.map(String::as_str).or(default) {
                    b_full.insert(name.clone(), v.to_string());
                }
            }
        }
    }
    // Defaults of known parameters neither code wrote.
    for name in ["charset", "format", "delsp"] {
        if let Some(Some(d)) = param_knowledge(a, name) {
            a_full.entry(name.into()).or_insert_with(|| d.into());
            b_full.entry(name.into()).or_insert_with(|| d.into());
        }
    }
    let a_in_b = a_full.iter().all(|(k, v)| b_full.get(k) == Some(v));
    let b_in_a = b_full.iter().all(|(k, v)| a_full.get(k) == Some(v));
    Ok(match (a_in_b, b_in_a) {
        (true, true) => SubsumptionOutcome::Equivalent,
        (true, false) => SubsumptionOutcome::Subsumes,
        (false, true) => SubsumptionOutcome::SubsumedBy,
        (false, false) => SubsumptionOutcome::NotSubsumed,
    })
}

fn includes<'a>(vs: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    vs.get("compose")
        .and_then(|c| c.get(key))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// Whether `vs` draws only on the media type code system.
pub fn is_media_type_value_set(vs: &Value) -> bool {
    let mut any = false;
    for key in ["include", "exclude"] {
        for inc in includes(vs, key) {
            if inc.get("system").and_then(Value::as_str) != Some(SYSTEM)
                || inc.get("valueSet").is_some()
            {
                return false;
            }
            any = true;
        }
    }
    any
}

fn include_matches(inc: &Value, mt: &MediaType) -> bool {
    if let Some(concepts) = inc
        .get("concept")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    {
        return concepts.iter().any(|c| {
            c.get("code")
                .and_then(Value::as_str)
                .and_then(parse)
                .is_some_and(|p| p == *mt)
        });
    }
    inc.get("filter")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .all(|f| {
            let value = f.get("value").and_then(Value::as_str).unwrap_or_default();
            match f.get("property").and_then(Value::as_str) {
                Some("base") => match value.to_ascii_lowercase().split_once('/') {
                    Some((t, s)) => mt.ty == t && mt.sub == s,
                    None => mt.ty == value.to_ascii_lowercase(),
                },
                Some("registered") => mt.is_registered() == (value == "true"),
                _ => false,
            }
        })
}

/// Whether `mt` is in the media type value set `vs`.
pub fn in_value_set(vs: &Value, mt: &MediaType) -> bool {
    includes(vs, "include").any(|inc| include_matches(inc, mt))
        && !includes(vs, "exclude").any(|inc| include_matches(inc, mt))
}

/// The codes of the media type value set `vs`: `Ok((codes, unclosed))`, or
/// `Err(())` when it cannot be enumerated.
#[allow(clippy::result_unit_err)]
pub fn expand(vs: &Value) -> Result<(Vec<String>, bool), ()> {
    let mut codes: Vec<String> = Vec::new();
    let mut unclosed = false;
    for inc in includes(vs, "include") {
        if let Some(concepts) = inc
            .get("concept")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
        {
            for c in concepts {
                if let Some(code) = c.get("code").and_then(Value::as_str) {
                    codes.push(code.to_string());
                }
            }
            continue;
        }
        let filters: Vec<&Value> = inc
            .get("filter")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .collect();
        let walkable = filters.iter().any(|f| {
            f.get("property").and_then(Value::as_str) == Some("registered")
                && f.get("value").and_then(Value::as_str) == Some("true")
        });
        if !walkable {
            return Err(());
        }
        unclosed = true;
        for essence in registered() {
            if let Some(mt) = parse(essence) {
                if include_matches(inc, &mt) {
                    codes.push(essence.to_string());
                }
            }
        }
    }
    codes.retain(|c| {
        parse(c).is_some_and(|mt| !includes(vs, "exclude").any(|inc| include_matches(inc, &mt)))
    });
    codes.sort();
    codes.dedup();
    Ok((codes, unclosed))
}

/// The text of the error for a value set that cannot be expanded.
pub const NOT_ENUMERABLE: &str = "The code system 'urn:ietf:bcp:13' cannot be expanded because \
     its codes cannot be iterated or enumerated in any meaningful sense";

#[cfg(test)]
mod tests {
    use super::*;
    use SubsumptionOutcome::*;

    fn sub(a: &str, b: &str) -> Result<SubsumptionOutcome, String> {
        subsumes(&parse(a).unwrap(), &parse(b).unwrap())
    }

    #[test]
    fn subsumption_follows_parameter_defaults() {
        assert_eq!(sub("text/plain", "text/plain"), Ok(Equivalent));
        assert_eq!(sub("text/plain", "application/json"), Ok(NotSubsumed));
        assert_eq!(
            sub("application/xml", "application/fhir+xml"),
            Ok(NotSubsumed)
        );
        assert_eq!(
            sub("application/xml", "application/xml; charset=utf-8"),
            Ok(Subsumes)
        );
        assert_eq!(
            sub("application/xml; charset=utf-8", "application/xml"),
            Ok(SubsumedBy)
        );
        assert_eq!(
            sub("text/plain; charset=utf-8", "text/plain; charset=utf-16"),
            Ok(NotSubsumed)
        );
        assert_eq!(
            sub(
                "text/vcard; charset=utf-8",
                "text/vcard; charset=utf-8; version=4.0"
            ),
            Ok(Subsumes)
        );
        assert_eq!(
            sub("text/plain; charset=utf-8", "text/plain; format=flowed"),
            Ok(NotSubsumed)
        );
        assert_eq!(
            sub("text/plain", "text/plain; charset=us-ascii"),
            Ok(Equivalent)
        );
        assert_eq!(
            sub("text/plain", "text/plain; charset=utf-8"),
            Ok(NotSubsumed)
        );
        assert_eq!(
            sub("text/plain", "text/plain; format=flowed"),
            Ok(NotSubsumed)
        );
        assert_eq!(
            sub("text/csv", "text/csv; charset=utf-8"),
            Err("charset".into())
        );
        assert_eq!(sub("text/plain", "text/plain; foo=bar"), Err("foo".into()));
        assert_eq!(
            sub("text/plain; foo=bar", "text/plain; foo=baz"),
            Err("foo".into())
        );
        assert_eq!(
            sub(
                "application/xml; foo=bar",
                "application/xml; charset=utf-8; foo=bar"
            ),
            Ok(Subsumes)
        );
        assert_eq!(
            sub("Text/Plain; CharSet=UTF-8", "text/plain; charset=utf-8"),
            Ok(Equivalent)
        );
        assert!(parse("not a mime type").is_none());
    }

    #[test]
    fn value_set_membership_and_expansion() {
        let vs = |compose: Value| serde_json::json!({"compose": compose});
        let base = vs(serde_json::json!({"include": [{"system": SYSTEM,
            "filter": [{"property": "base", "op": "=", "value": "text"}]}]}));
        assert!(in_value_set(&base, &parse("text/html").unwrap()));
        assert!(in_value_set(
            &base,
            &parse("text/plain; charset=utf-8").unwrap()
        ));
        assert!(!in_value_set(&base, &parse("application/json").unwrap()));
        assert!(expand(&base).is_err());

        let reg = vs(
            serde_json::json!({"include": [{"system": SYSTEM, "filter": [
            {"property": "registered", "op": "=", "value": "true"},
            {"property": "base", "op": "=", "value": "text/plain"}]}]}),
        );
        assert_eq!(expand(&reg), Ok((vec!["text/plain".to_string()], true)));
        let unreg = vs(serde_json::json!({"include": [{"system": SYSTEM,
            "filter": [{"property": "registered", "op": "=", "value": "false"}]}]}));
        assert!(in_value_set(
            &unreg,
            &parse("application/x-tx-ecosystem-test; charset=utf-8").unwrap()
        ));
        assert!(!in_value_set(&unreg, &parse("text/plain").unwrap()));
    }
}
