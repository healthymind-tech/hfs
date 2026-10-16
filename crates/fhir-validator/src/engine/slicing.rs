//! Array slicing: the mark/sweep pass.
//!
//! For every slicing-bearing schema in the element's set:
//! - **mark**: match each array item against each slice's pattern
//!   (`_.isMatch`-style partial deep equality), counting matches per slice;
//! - items matched by at least one slice **with a schema** are validated
//!   once, cooperatively, against the element set extended with *all* their
//!   matched slice schemas (the reference validator validates once per
//!   matched slice against `elset ∪ that slice`, and then re-validates every
//!   item in the plain pass — duplicating base-set errors; we consume
//!   matched items instead, which is indistinguishable on the conformance
//!   suite and saner on real resources);
//! - **sweep**: `rules: closed` (with `@default` absorption) and
//!   `rules: openAtEnd` violations, `ordered` violations, then per-slice
//!   `min`/`max` cardinality in slice declaration order.
//!
//! Divergences from the reference validator (which implements only pattern
//! matching + cardinality): `closed`/`openAtEnd`/`ordered`/`@default` are
//! enforced (pinned by `tests/fixtures/extended/slicing_rules.json`), and a
//! `max: 0` prohibited slice is enforced (the reference skips falsy bounds).
//!
//! Match types: `pattern` (partial deep match) and `all` — the converter's
//! translation of `type`/`profile` discriminators, whose parts are a
//! pattern, a type check on the value at a path (`resourceType`, or the
//! choice branch key), and a profile check (a `meta.profile` claim, or a
//! structural validation of the value against the profile). `binding` and
//! `resolve-ref` matchers are not evaluated. A slice whose matcher we
//! cannot evaluate matches nothing, and a slice with no `match` at all (a
//! constraining slice) also matches nothing.

use super::errors::{self, ErrorKind, Severity};
use super::walk::{SchemaSet, WalkCtx, add_schemas_to_set, is_partial_match, validate_node};
use crate::resolver::SchemaResolver;
use crate::schema::{Slice, Slicing};
use indexmap::IndexMap;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

/// The reserved slice name that absorbs unmatched items in closed slicing.
const DEFAULT_SLICE: &str = "@default";

struct LayerMark {
    slicing: Slicing,
    /// The profile the slicing came from, when its layer key names one.
    source: Option<String>,
    /// Matched-item count per slice, in declaration order.
    counters: IndexMap<String, u64>,
    /// For each array item, the slice names it matched (declaration order).
    item_matches: Vec<Vec<String>>,
}

/// Run the slicing pass over an array element. Returns the indices of items
/// that were already fully validated (consumed) here.
pub(super) fn validate_slices(
    ctx: &mut WalkCtx<'_>,
    elset: &SchemaSet,
    items: &[Value],
) -> HashSet<usize> {
    let slicings: Vec<(Slicing, Option<String>)> = elset
        .iter()
        .filter_map(|(key, s)| Some((s.slicing.clone()?, super::walk::key_origin(key))))
        .collect();
    if slicings.is_empty() {
        return HashSet::new();
    }

    let resolver = ctx.resolver_handle();

    // ---- mark ----
    let mut marks: Vec<LayerMark> = Vec::with_capacity(slicings.len());
    for (slicing, source) in slicings {
        let mut counters: IndexMap<String, u64> =
            slicing.slices.keys().map(|k| (k.clone(), 0)).collect();
        let mut item_matches: Vec<Vec<String>> = vec![Vec::new(); items.len()];
        for (index, item) in items.iter().enumerate() {
            for (name, slice) in &slicing.slices {
                if name == DEFAULT_SLICE {
                    continue;
                }
                if slice_matches_in(Some(resolver), slice, item) {
                    *counters.get_mut(name).expect("counter exists") += 1;
                    item_matches[index].push(name.clone());
                }
            }
        }
        // `@default` (closed slicing only) absorbs items no other slice
        // matched; count them so its own min/max can apply.
        if slicing.slices.contains_key(DEFAULT_SLICE) && slicing.rules.as_deref() == Some("closed")
        {
            let absorbed = item_matches.iter().filter(|m| m.is_empty()).count() as u64;
            *counters.get_mut(DEFAULT_SLICE).expect("counter exists") = absorbed;
        }
        marks.push(LayerMark {
            slicing,
            source,
            counters,
            item_matches,
        });
    }

    // ---- validate matched items (item order), consuming them ----
    let mut consumed: HashSet<usize> = HashSet::new();
    for (index, item) in items.iter().enumerate() {
        let mut slice_schemas: Vec<(String, Arc<crate::schema::FhirSchema>)> = Vec::new();
        for mark in &marks {
            for name in &mark.item_matches[index] {
                let slice = &mark.slicing.slices[name];
                if let Some(schema) = &slice.schema {
                    slice_schemas.push((slice_key(slice, name), Arc::clone(schema)));
                }
            }
        }
        if slice_schemas.is_empty() {
            continue;
        }
        let mut extended = elset.clone();
        for (key, schema) in &slice_schemas {
            add_schemas_to_set(ctx, &mut extended, Arc::clone(schema), key);
        }
        ctx.path.push_index(index);
        validate_node(ctx, &extended, item);
        ctx.path.pop();
        consumed.insert(index);
    }

    // ---- sweep: rules, order, cardinality (per layer, elset order) ----
    for mark in &marks {
        let rules = mark.slicing.rules.as_deref().unwrap_or("open");

        if rules == "closed" {
            let default_slice = mark.slicing.slices.get(DEFAULT_SLICE);
            for (index, item) in items.iter().enumerate() {
                if !mark.item_matches[index].is_empty() {
                    continue;
                }
                match default_slice {
                    Some(Slice {
                        schema: Some(schema),
                        ..
                    }) => {
                        let mut extended = elset.clone();
                        add_schemas_to_set(ctx, &mut extended, Arc::clone(schema), DEFAULT_SLICE);
                        ctx.path.push_index(index);
                        validate_node(ctx, &extended, item);
                        ctx.path.pop();
                        consumed.insert(index);
                    }
                    Some(_) => {
                        // @default without a schema: absorbed, nothing extra
                        // to check; the plain pass still validates the item.
                    }
                    None => {
                        ctx.path.push_index(index);
                        ctx.error_from(
                            ErrorKind::SliceUnmatched,
                            errors::msg_slice_closed_unmatched(),
                            Severity::Error,
                            mark.source.clone(),
                        );
                        ctx.path.pop();
                    }
                }
            }
        } else if rules == "openAtEnd"
            && let Some(last_matched) = mark.item_matches.iter().rposition(|m| !m.is_empty())
        {
            for (index, matches) in mark.item_matches.iter().enumerate().take(last_matched) {
                if matches.is_empty() {
                    ctx.path.push_index(index);
                    ctx.error(ErrorKind::SliceUnmatched, errors::msg_slice_open_at_end());
                    ctx.path.pop();
                }
            }
        }

        if mark.slicing.ordered == Some(true) {
            let mut max_order_seen: Option<u64> = None;
            for (index, matches) in mark.item_matches.iter().enumerate() {
                let Some(first) = matches.first() else {
                    continue;
                };
                let Some(order) = mark.slicing.slices[first].order else {
                    continue;
                };
                if let Some(seen) = max_order_seen
                    && order < seen
                {
                    ctx.path.push_index(index);
                    ctx.error(ErrorKind::SliceOrder, errors::msg_slice_order(first));
                    ctx.path.pop();
                }
                max_order_seen = Some(max_order_seen.unwrap_or(0).max(order));
            }
        }

        for (name, slice) in &mark.slicing.slices {
            let found = mark.counters[name];
            if let Some(min) = slice.min
                && min > 0
                && found < min
            {
                ctx.error_from(
                    ErrorKind::SliceCardinality,
                    errors::msg_slice_min(min, found),
                    Severity::Error,
                    mark.source.clone(),
                );
            }
            // Unlike the reference validator (which skips falsy bounds),
            // `max: 0` — a prohibited slice, a real FHIR pattern — is
            // enforced.
            if let Some(max) = slice.max
                && found > max
            {
                ctx.error_from(
                    ErrorKind::SliceCardinality,
                    errors::msg_slice_max(max, found),
                    Severity::Error,
                    mark.source.clone(),
                );
            }
        }
    }

    consumed
}

/// The schema-set key a matched slice's schema joins under: the extension
/// URL for an extension slice (so issues inside it name the extension
/// definition as their source), else the slice name.
fn slice_key(slice: &Slice, name: &str) -> String {
    slice
        .match_
        .as_ref()
        .and_then(|m| m.value.as_ref())
        .and_then(|v| v.get("url"))
        .and_then(Value::as_str)
        .filter(|url| url.contains("://"))
        .map_or_else(|| name.to_string(), str::to_string)
}

/// Does an item belong to a slice? (No resolver: `profile` parts match by
/// the item's `meta.profile` claims only.)
pub(crate) fn slice_matches(slice: &Slice, item: &Value) -> bool {
    slice_matches_in(None, slice, item)
}

/// Does an item belong to a slice? A `pattern` match is a partial deep
/// match; an `all` match (compiled from `type`/`profile` discriminators,
/// possibly with a pattern part) requires every part. A missing `match`
/// (constraining slice) matches nothing, and a `match` with no `value`
/// matches everything (lodash `_.isMatch` semantics for an empty source).
pub(crate) fn slice_matches_in(
    resolver: Option<&dyn SchemaResolver>,
    slice: &Slice,
    item: &Value,
) -> bool {
    if let Some(match_) = &slice.match_ {
        if match_.type_.as_deref() == Some("all") {
            return match match_.value.as_ref() {
                Some(Value::Array(parts)) => parts.iter().all(|p| part_matches(resolver, p, item)),
                _ => false,
            };
        }
        return match match_.value.as_ref() {
            Some(pattern) => is_partial_match(item, pattern),
            None => true,
        };
    }
    // The converter carries a pattern/value discriminator as the slice
    // schema's pattern (or fixed) keyword rather than an explicit match.
    if let Some(schema) = &slice.schema {
        if let Some(pattern) = &schema.pattern {
            return is_partial_match(item, pattern);
        }
        if let Some(fixed) = &schema.fixed {
            return is_partial_match(item, fixed);
        }
    }
    false
}

/// One part of an `all` match.
fn part_matches(resolver: Option<&dyn SchemaResolver>, part: &Value, item: &Value) -> bool {
    let path = part.get("path").and_then(Value::as_str).unwrap_or("$this");
    let strings = |key: &str| -> Vec<&str> {
        part.get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    };
    match part.get("type").and_then(Value::as_str) {
        Some("pattern") => part
            .get("value")
            .is_none_or(|pattern| is_partial_match(item, pattern)),
        Some("type") => type_matches(item, path, &strings("codes")),
        Some("profile") => profile_matches(resolver, item, path, &strings("profiles")),
        _ => false,
    }
}

/// The objects holding the last step of `path`, and that step's name.
fn parents_at<'v>(item: &'v Value, path: &str) -> (Vec<&'v Value>, Option<String>) {
    if path == "$this" {
        return (vec![item], None);
    }
    let segments: Vec<&str> = path.split('.').collect();
    let (last, parents) = segments.split_last().expect("split yields a segment");
    let mut current = vec![item];
    for segment in parents {
        let mut next = Vec::new();
        for value in current {
            match value.get(*segment) {
                Some(Value::Array(items)) => next.extend(items.iter()),
                Some(v) => next.push(v),
                None => {}
            }
        }
        current = next;
    }
    let last = last.strip_suffix("[x]").unwrap_or(last).to_string();
    (current, Some(last))
}

/// `type` discriminator: the value at `path` is one of `codes` — a resource
/// by its `resourceType`, a choice element by its branch key
/// (`valueQuantity` for `Quantity`).
fn type_matches(item: &Value, path: &str, codes: &[&str]) -> bool {
    let is_type = |value: &Value| match value.get("resourceType").and_then(Value::as_str) {
        Some(rt) => codes.contains(&rt),
        None => false,
    };
    let (parents, last) = parents_at(item, path);
    let Some(last) = last else {
        return parents.iter().any(|v| is_type(v));
    };
    parents.iter().any(|parent| {
        let Some(obj) = parent.as_object() else {
            return false;
        };
        if let Some(value) = obj.get(&last) {
            return match value {
                Value::Array(items) => items.iter().any(is_type),
                v if v.get("resourceType").is_some() => is_type(v),
                // A non-resource element of a single declared type.
                _ => codes.len() == 1,
            };
        }
        codes
            .iter()
            .any(|code| obj.contains_key(&choice_key(&last, code)))
    })
}

fn choice_key(base: &str, code: &str) -> String {
    let mut chars = code.chars();
    match chars.next() {
        Some(first) => format!("{base}{}{}", first.to_uppercase(), chars.as_str()),
        None => base.to_string(),
    }
}

/// `profile` discriminator: the value at `path` conforms to one of
/// `profiles` — it claims it in `meta.profile`, or (with a resolver) it
/// validates against it without structural errors, as the HL7 validator
/// decides slice membership.
fn profile_matches(
    resolver: Option<&dyn SchemaResolver>,
    item: &Value,
    path: &str,
    profiles: &[&str],
) -> bool {
    let (parents, last) = parents_at(item, path);
    let values: Vec<&Value> = match last {
        None => parents,
        Some(last) => parents
            .iter()
            .filter_map(|p| p.get(&last))
            .flat_map(|v| match v {
                Value::Array(items) => items.iter().collect::<Vec<_>>(),
                other => vec![other],
            })
            .collect(),
    };
    let unversioned = |url: &str| url.split('|').next().unwrap_or(url).to_string();
    values.iter().any(|value| {
        let claimed: Vec<String> = value
            .get("meta")
            .and_then(|m| m.get("profile"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(unversioned)
                    .collect()
            })
            .unwrap_or_default();
        profiles.iter().any(|profile| {
            if claimed.contains(&unversioned(profile)) {
                return true;
            }
            let Some(resolver) = resolver else {
                return false;
            };
            if resolver.resolve(profile).is_none() {
                return false;
            }
            let opts = super::ValidationOptions {
                profiles: vec![profile.to_string()],
                use_meta_profiles: false,
                unknown_profile: super::UnknownProfilePolicy::Ignore,
                check_extension_context: false,
            };
            super::walk::validate(resolver, value, &opts)
                .errors
                .iter()
                .all(|e| e.severity != super::Severity::Error)
        })
    })
}

#[cfg(test)]
mod slice_match_tests {
    use super::*;
    use serde_json::json;

    fn slice(v: serde_json::Value) -> Slice {
        serde_json::from_value(v).expect("slice")
    }

    #[test]
    fn an_explicit_match_value_is_a_partial_match() {
        let s = slice(json!({ "match": { "type": "pattern", "value": { "system": "http://x" } } }));
        assert!(slice_matches(
            &s,
            &json!({ "system": "http://x", "value": "1" })
        ));
        assert!(!slice_matches(&s, &json!({ "system": "http://y" })));
    }

    #[test]
    fn a_match_without_a_value_matches_everything() {
        let s = slice(json!({ "match": { "type": "pattern" } }));
        assert!(slice_matches(&s, &json!({ "anything": true })));
    }

    #[test]
    fn a_schema_pattern_stands_in_for_the_match() {
        let s = slice(json!({ "schema": { "pattern": { "system": "http://x" } } }));
        assert!(slice_matches(&s, &json!({ "system": "http://x" })));
        assert!(!slice_matches(&s, &json!({ "system": "http://y" })));
    }

    #[test]
    fn a_schema_fixed_stands_in_for_the_match() {
        let s = slice(json!({ "schema": { "fixed": { "system": "http://x" } } }));
        assert!(slice_matches(&s, &json!({ "system": "http://x" })));
    }

    #[test]
    fn no_discriminator_at_all_matches_nothing() {
        let s = slice(json!({ "min": 1 }));
        assert!(!slice_matches(&s, &json!({ "system": "http://x" })));
    }

    #[test]
    fn a_schema_with_neither_pattern_nor_fixed_matches_nothing() {
        let s = slice(json!({ "schema": { "type": "Identifier" } }));
        assert!(!slice_matches(&s, &json!({ "system": "http://x" })));
    }
}
