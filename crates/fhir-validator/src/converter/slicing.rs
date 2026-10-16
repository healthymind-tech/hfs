//! Discriminator → slice-match translation.
//!
//! A `value`/`pattern` discriminator at path `P` becomes a partial-match
//! pattern built from the `fixed[x]`/`pattern[x]` constant found at `P`
//! inside the slice's subtree (`$this` meaning the item root; constants
//! inside nested slices count, as in the core `bp` profile). `type` and
//! `profile` discriminators read the types/profiles declared at `P` and
//! compile to an `all` match (see `build_match`). Discriminators we cannot
//! evaluate (`exists`, and any path containing `resolve()` or
//! `extension(...)`) produce a slice with **no match and no minimum**: it
//! can never produce false cardinality errors, its constraints simply stay
//! dormant, and the generator surfaces a warning.
//!
//! Under `ordered: true` each slice also gets its declaration ordinal as
//! `order` — FHIR's ordered slicing means "matched items appear in the order
//! the slices are declared", and the engine's check needs a number to compare.
//! Emitted only when the slicing is ordered, matching upstream's generated
//! schemas (an unordered slicing carries no `order`).

use super::EdDiscriminator;
use super::tree::{SliceNode, finalize};
use crate::schema::{Match, Slice, Slicing};
use indexmap::IndexMap;
use serde_json::{Map, Value};
use std::sync::Arc;

pub(super) fn build_slicing(
    slices: IndexMap<String, SliceNode>,
    discriminators: &[EdDiscriminator],
    rules: Option<String>,
    ordered: Option<bool>,
    warnings: &mut Vec<String>,
) -> Option<Slicing> {
    let is_ordered = ordered == Some(true);
    let mut out: IndexMap<String, Slice> = IndexMap::new();
    for (position, (name, slice_node)) in slices.into_iter().enumerate() {
        let SliceNode {
            node,
            min,
            max,
            extension_profile: _,
        } = slice_node;

        let match_ = build_match(&node, discriminators);
        if match_.is_none() {
            warnings.push(format!(
                "slice '{name}': discriminator(s) {:?} not translatable to a match; \
                 slice kept without match or min",
                discriminators
                    .iter()
                    .map(|d| format!("{}@{}", d.type_, d.path))
                    .collect::<Vec<_>>()
            ));
        }

        let schema = finalize(node, warnings);
        out.insert(
            name,
            Slice {
                // Without a matcher the slice must not enforce a minimum —
                // nothing can ever match it.
                min: if match_.is_some() { min } else { None },
                max,
                match_,
                // Ordinals are relative — only their sequence is compared —
                // so gaps from lifted extension-sugar slices are harmless.
                order: is_ordered.then_some(position as u64),
                reslice: None,
                slice_is_constraining: None,
                schema: schema.map(Arc::new),
            },
        );
    }

    if out.is_empty() {
        return None;
    }
    Some(Slicing {
        slices: out,
        rules,
        ordered,
    })
}

/// Build the slice match from the discriminators.
///
/// `value`/`pattern` discriminators compile to one partial-match pattern
/// built from the constants at their paths. `type` and `profile`
/// discriminators (`Bundle.entry` by `resource`, `component` by `value`)
/// read the slice's declared types/profiles at their path; a slicing that
/// uses any of them compiles to an `all` match whose parts the engine
/// checks together (`pattern`, `type`, `profile`). Paths through
/// `resolve()` or `extension(...)`, and `exists` discriminators, stay
/// untranslatable.
fn build_match(node: &super::tree::Node, discriminators: &[EdDiscriminator]) -> Option<Match> {
    if discriminators.is_empty() {
        return None;
    }

    let mut this_constant: Option<Value> = None;
    let mut pattern = Map::new();
    let mut parts: Vec<Value> = Vec::new();

    for disc in discriminators {
        if disc.path.contains("resolve()") || disc.path.contains("extension(") {
            return None;
        }
        match disc.type_.as_str() {
            "value" | "pattern" => {
                let constant = constant_at(node, &disc.path)?;
                if disc.path == "$this" {
                    if discriminators.len() > 1 {
                        return None; // $this plus siblings — ambiguous
                    }
                    this_constant = Some(constant);
                } else {
                    insert_nested(&mut pattern, &disc.path, constant);
                }
            }
            "type" => {
                let target = node_at(node, &disc.path)?;
                if target.type_codes.is_empty() {
                    return None;
                }
                parts.push(serde_json::json!({
                    "type": "type", "path": disc.path, "codes": target.type_codes,
                }));
            }
            "profile" => {
                let target = node_at(node, &disc.path)?;
                if target.type_profiles.is_empty() {
                    return None;
                }
                parts.push(serde_json::json!({
                    "type": "profile", "path": disc.path, "profiles": target.type_profiles,
                }));
            }
            _ => return None,
        }
    }

    let pattern_value = match this_constant {
        Some(v) => Some(v),
        None if !pattern.is_empty() => Some(Value::Object(pattern)),
        None => None,
    };
    if parts.is_empty() {
        return Some(Match {
            type_: Some("pattern".to_string()),
            value: Some(pattern_value?),
            resolve_ref: None,
        });
    }
    if let Some(value) = pattern_value {
        parts.insert(0, serde_json::json!({"type": "pattern", "value": value}));
    }
    Some(Match {
        type_: Some("all".to_string()),
        value: Some(Value::Array(parts)),
        resolve_ref: None,
    })
}

/// The node at a discriminator path within the slice subtree (`$this` →
/// the subtree root). A `foo[x]` / `foo` step resolves to the choice
/// declarer `foo`.
fn node_at<'a>(node: &'a super::tree::Node, path: &str) -> Option<&'a super::tree::Node> {
    if path == "$this" {
        return Some(node);
    }
    let mut current = node;
    for segment in path.split('.') {
        let name = segment.strip_suffix("[x]").unwrap_or(segment);
        current = current.children.get(name)?;
    }
    Some(current)
}

/// The fixed/pattern constant at `path` within the slice subtree
/// (`$this` → the subtree root itself).
///
/// The constant may sit below a nested slice: the core blood-pressure
/// profile slices `component` by `code.coding.code`, and the constants live
/// at `component:SystolicBP.code.coding:SBPCode.code` — inside the
/// `coding` slice. Required (`min >= 1`) nested slices are searched first.
/// A constant on an intermediate element (`patternCodeableConcept` on
/// `code`) answers for the path below it.
fn constant_at(node: &super::tree::Node, path: &str) -> Option<Value> {
    if path == "$this" {
        return own_constant(node);
    }
    let segments: Vec<&str> = path.split('.').collect();
    find_constant(node, &segments)
}

fn own_constant(node: &super::tree::Node) -> Option<Value> {
    node.schema
        .fixed
        .clone()
        .or_else(|| node.schema.pattern.clone())
}

fn find_constant(node: &super::tree::Node, segments: &[&str]) -> Option<Value> {
    let Some((segment, rest)) = segments.split_first() else {
        return own_constant(node);
    };
    let child = node.children.get(*segment)?;
    if let Some(v) = find_constant(child, rest) {
        return Some(v);
    }
    let mut slices: Vec<&super::tree::SliceNode> = child.slices.values().collect();
    slices.sort_by_key(|s| std::cmp::Reverse(s.min.unwrap_or(0).min(1)));
    for slice in slices {
        if let Some(v) = find_constant(&slice.node, rest) {
            return Some(v);
        }
    }
    if !rest.is_empty()
        && let Some(constant) = own_constant(child)
    {
        return json_at(&constant, rest);
    }
    None
}

/// The value at `segments` inside a constant, through arrays (first item
/// that has it).
fn json_at(value: &Value, segments: &[&str]) -> Option<Value> {
    let Some((segment, rest)) = segments.split_first() else {
        return Some(value.clone());
    };
    match value {
        Value::Array(items) => items.iter().find_map(|item| json_at(item, segments)),
        Value::Object(map) => json_at(map.get(*segment)?, rest),
        _ => None,
    }
}

/// `insert_nested(map, "a.b", v)` → `{a: {b: v}}` (merging siblings).
fn insert_nested(map: &mut Map<String, Value>, path: &str, value: Value) {
    let mut segments = path.split('.').peekable();
    let mut current = map;
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            current.insert(segment.to_string(), value);
            return;
        }
        current = current
            .entry(segment.to_string())
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("nested pattern segment is an object");
    }
}
