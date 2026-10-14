//! Reference checks on a `Reference` node: the literal reference must be
//! well formed, agree with `Reference.type`, and name a resource type every
//! layer of the element allows (`refers`, from `type.targetProfile`).
//!
//! Only what the reference itself states is judged; nothing is resolved
//! against storage. A target type is taken from a relative or absolute
//! RESTful URL (`Patient/1`, `Patient?identifier=…`,
//! `https://x/fhir/Patient/1/_history/2`), a contained `#id`, or
//! `Reference.type`. `urn:uuid:` and logical (identifier-only) references
//! name no type here.
//!
//! A `refers` entry that does not resolve to a known type (an uninstalled
//! profile) makes that layer unjudgeable rather than restrictive, so
//! missing packages never produce false target-type errors.

use super::errors::{self, ErrorKind};
use super::walk::{SchemaSet, WalkCtx};
use serde_json::Value;

const CORE_SD_PREFIX: &str = "http://hl7.org/fhir/StructureDefinition/";

/// Run the reference checks when `data` is a Reference object.
pub(super) fn check_reference(ctx: &mut WalkCtx<'_>, set: &SchemaSet, data: &Value) {
    if !set
        .schemas()
        .any(|s| s.type_.as_deref() == Some("Reference"))
    {
        return;
    }
    let Some(obj) = data.as_object() else {
        return;
    };
    let literal = obj.get("reference").and_then(Value::as_str);
    let declared = obj
        .get("type")
        .and_then(Value::as_str)
        .map(|t| t.strip_prefix(CORE_SD_PREFIX).unwrap_or(t));

    let mut literal_type: Option<String> = None;
    if let Some(reference) = literal {
        if !is_well_formed(reference) {
            ctx.error(
                ErrorKind::ReferenceInvalid,
                errors::msg_reference_malformed(reference),
            );
            return;
        }
        literal_type = target_type(ctx, reference);
    }

    if let (Some(lit), Some(declared)) = (literal_type.as_deref(), declared)
        && lit != declared
        && is_resource_type(ctx, declared)
    {
        ctx.error(
            ErrorKind::ReferenceInvalid,
            errors::msg_reference_type_mismatch(literal.unwrap_or_default(), lit, declared),
        );
    }

    let Some(found) = literal_type.or_else(|| {
        declared
            .filter(|d| is_resource_type(ctx, d))
            .map(str::to_string)
    }) else {
        return;
    };

    // Every layer that restricts the target must allow it; report the
    // narrowest list that excludes it.
    let mut narrowest: Option<Vec<String>> = None;
    let layers: Vec<Vec<String>> = set.schemas().filter_map(|s| s.refers.clone()).collect();
    for refers in layers {
        let Some(allowed) = allowed_types(ctx, &refers) else {
            continue;
        };
        if !allowed.iter().any(|a| a == &found)
            && narrowest.as_ref().is_none_or(|n| allowed.len() < n.len())
        {
            narrowest = Some(allowed);
        }
    }
    if let Some(allowed) = narrowest {
        ctx.error(
            ErrorKind::ReferenceInvalid,
            errors::msg_reference_target_type(&found, &allowed),
        );
    }
}

/// The resource types a `refers` list allows, or `None` when it cannot be
/// judged (empty, `Resource`, or an entry that does not resolve to a type).
fn allowed_types(ctx: &WalkCtx<'_>, refers: &[String]) -> Option<Vec<String>> {
    if refers.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(refers.len());
    for url in refers {
        let url = url.split('|').next().unwrap_or(url);
        let ty = match url.strip_prefix(CORE_SD_PREFIX) {
            Some(name) if is_resource_type(ctx, name) => name.to_string(),
            _ => {
                let schema = ctx.resolver().resolve(url)?;
                if !crate::editor::is_resource(&schema) {
                    return None;
                }
                schema.type_.clone().or_else(|| schema.name.clone())?
            }
        };
        if matches!(ty.as_str(), "Resource" | "DomainResource") {
            return None;
        }
        out.push(ty);
    }
    Some(out)
}

fn is_resource_type(ctx: &WalkCtx<'_>, name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_uppercase())
        && ctx
            .resolver()
            .resolve(name)
            .is_some_and(|s| crate::editor::is_resource(&s))
}

/// The resource type a literal reference names, when it names one.
fn target_type(ctx: &WalkCtx<'_>, reference: &str) -> Option<String> {
    if let Some(id) = reference.strip_prefix('#') {
        return ctx.contained_type(id);
    }
    if reference.starts_with("urn:") {
        return None;
    }
    let path = reference.split(['?', '#']).next().unwrap_or_default();
    let conditional = reference.contains('?');
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // `Type?criteria`, `Type/id`, `…/Type/id`, `…/Type/id/_history/vid`.
    let candidate = if conditional && !path.contains('/') {
        segments.last().copied()
    } else {
        match segments.as_slice() {
            [.., ty, _id, "_history", _vid] => Some(*ty),
            [.., ty, _id] => Some(*ty),
            _ => None,
        }
    }?;
    is_resource_type(ctx, candidate).then(|| candidate.to_string())
}

/// Syntactic sanity of `Reference.reference`: no whitespace, and either a
/// fragment, a URN, an absolute URL, a conditional reference, or a relative
/// `Type/id` path.
fn is_well_formed(reference: &str) -> bool {
    if reference.is_empty() || reference.chars().any(char::is_whitespace) {
        return false;
    }
    if reference.starts_with('#') || reference.starts_with("urn:") || reference.contains("://") {
        return true;
    }
    if reference.contains('?') {
        return reference.starts_with(|c: char| c.is_ascii_uppercase());
    }
    let segments: Vec<&str> = reference.split('/').collect();
    segments.len() >= 2 && segments.iter().all(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::is_well_formed;

    #[test]
    fn reference_syntax() {
        for ok in [
            "Patient/1",
            "Patient/1/_history/2",
            "#c1",
            "urn:uuid:11111111-1111-1111-1111-111111111111",
            "urn:oid:1.2.3",
            "https://example.org/fhir/Patient/1",
            "Patient?identifier=http://x|1",
        ] {
            assert!(is_well_formed(ok), "{ok}");
        }
        for bad in ["not a reference at all", "Patient", "", "Patient//1", "x?y"] {
            assert!(!is_well_formed(bad), "{bad}");
        }
    }
}
