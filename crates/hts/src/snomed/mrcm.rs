//! Concept model (MRCM) checks on post-coordinated expressions.
//!
//! Each refinement attribute must be a concept model attribute (or linkage
//! concept), be allowed on the focus by the MRCM attribute domain rules,
//! appear grouped or ungrouped as the rule says and no more often than its
//! cardinalities allow, and have a value within its attribute range (an
//! ECL constraint, or a concrete `int(…)` / `dec(…)` / `str(…)` range).
//!
//! Domain rules whose domain is a reference set (the lateralizable body
//! structures, `723264001`) restrict the attribute to its members; the
//! others are alternatives — the focus must be in one of them.

use super::ecl::{self, Evaluator, Virtual};
use super::model::{CONCEPT_MODEL_ATTRIBUTE, LINKAGE_CONCEPT, SnomedModel};
use super::scg::{Attribute, AttributeValue, Concrete, SubExpression, render_sub_plain};

/// Check `e`; `Err` carries the reason (the text after "Not a valid
/// expression: ").
pub fn check(m: &SnomedModel, e: &SubExpression) -> Result<(), String> {
    let idx = |id: &str| m.idx(id).ok_or_else(|| format!("Concept {id} not found"));
    let foci: Vec<u32> = e.focus.iter().map(|f| idx(&f.id)).collect::<Result<_, _>>()?;
    let mut counts: Vec<(u32, u32)> = Vec::new();
    let all = e
        .ungrouped
        .iter()
        .map(|a| (a, false))
        .chain(e.groups.iter().flatten().map(|a| (a, true)));
    for (a, grouped) in all {
        let t = idx(&a.name.id)?;
        let roots = [CONCEPT_MODEL_ATTRIBUTE, LINKAGE_CONCEPT];
        if !roots
            .iter()
            .filter_map(|r| m.idx(&r.to_string()))
            .any(|r| m.subsumes(r, t))
        {
            return Err(format!(
                "Concept {} is not valid in this context (must be a descendent of one of {CONCEPT_MODEL_ATTRIBUTE},{LINKAGE_CONCEPT})",
                a.name.id
            ));
        }
        match &a.value {
            AttributeValue::Concept(c) => {
                idx(&c.id)?;
            }
            AttributeValue::Expression(x) => check(m, x)?,
            AttributeValue::Concrete(_) => {}
        }
        range(m, t, a)?;
        let rule = domain(m, t, &foci)?;
        if let Some(rule) = rule {
            if grouped && !rule.grouped {
                return Err(format!(
                    "The SNOMED CT concept model does not allow the attribute {} to appear in a relationship group",
                    m.label(t)
                ));
            }
            if !grouped && rule.grouped {
                return Err(format!(
                    "The SNOMED CT concept model requires the attribute {} to be in a relationship group",
                    m.label(t)
                ));
            }
        }
        match counts.iter_mut().find(|(x, _)| *x == t) {
            Some(c) => c.1 += 1,
            None => counts.push((t, 1)),
        }
    }
    for (t, n) in counts {
        if let Some(rule) = domain(m, t, &foci)? {
            if let Some(max) = rule.cardinality.1 {
                if n > max {
                    return Err(format!(
                        "The SNOMED CT concept model allows the attribute {} at most {max} time(s) on one concept, but it is used {n} times",
                        m.label(t)
                    ));
                }
            }
        }
    }
    for g in &e.groups {
        let mut seen: Vec<(u32, u32)> = Vec::new();
        for a in g {
            let t = idx(&a.name.id)?;
            match seen.iter_mut().find(|(x, _)| *x == t) {
                Some(c) => c.1 += 1,
                None => seen.push((t, 1)),
            }
        }
        for (t, n) in seen {
            if let Some(rule) = domain(m, t, &foci)? {
                if let Some(max) = rule.in_group_cardinality.1.filter(|&x| x > 0) {
                    if n > max {
                        return Err(format!(
                            "The SNOMED CT concept model allows the attribute {} at most {max} time(s) in a relationship group, but it is used {n} times",
                            m.label(t)
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// The domain rule that admits attribute `t` on `foci`; `Ok(None)` when the
/// attribute has no rules.
fn domain<'m>(
    m: &'m SnomedModel,
    t: u32,
    foci: &[u32],
) -> Result<Option<&'m super::model::DomainRule>, String> {
    let Some(rules) = m.domains.get(&t) else { return Ok(None) };
    let focus_label = foci.first().map(|&f| m.label(f)).unwrap_or_default();
    let (refsets, concepts): (Vec<_>, Vec<_>) = rules.iter().partition(|r| m.is_refset(r.domain));
    for r in &refsets {
        let members = &m.refsets[&r.domain];
        if !foci.iter().any(|f| members.contains(f)) {
            return Err(format!(
                "The SNOMED CT concept model does not allow the attribute {} on {focus_label} (the attribute is only valid for concepts in {})",
                m.label(t),
                m.label(r.domain)
            ));
        }
    }
    if concepts.is_empty() {
        return Ok(refsets.first().copied());
    }
    if let Some(r) = concepts
        .iter()
        .find(|r| foci.iter().any(|&f| m.subsumes(r.domain, f)))
    {
        return Ok(Some(r));
    }
    let domains: Vec<String> = concepts
        .iter()
        .map(|r| format!("in the domain {}", m.label(r.domain)))
        .collect();
    Err(format!(
        "The SNOMED CT concept model does not allow the attribute {} on {focus_label} (the attribute is only valid {})",
        m.label(t),
        domains.join(", or ")
    ))
}

fn value_label(m: &SnomedModel, a: &Attribute) -> String {
    match &a.value {
        AttributeValue::Concept(c) => m.idx(&c.id).map(|i| m.label(i)).unwrap_or_else(|| c.id.clone()),
        AttributeValue::Expression(x) => format!("({})", render_sub_plain(x)),
        AttributeValue::Concrete(c) => c.render(),
    }
}

fn range(m: &SnomedModel, t: u32, a: &Attribute) -> Result<(), String> {
    let Some(rules) = m.ranges.get(&t) else { return Ok(()) };
    let Some(rule) = rules.first() else { return Ok(()) };
    let c = rule.constraint.trim();
    let refuse = || {
        Err(format!(
            "The SNOMED CT concept model does not allow {} as the value of the attribute {} (the value must satisfy \"{c}\")",
            value_label(m, a),
            m.label(t)
        ))
    };
    if let Some(ok) = concrete_range(c, &a.value) {
        return if ok { Ok(()) } else { refuse() };
    }
    let Ok(constraint) = ecl::parse(c) else { return Ok(()) };
    let mut ev = Evaluator::new(m);
    let ok = match &a.value {
        AttributeValue::Concept(x) => match m.idx(&x.id) {
            Some(i) => ev.eval(&constraint).map(|s| s.contains(&i)).unwrap_or(true),
            None => true,
        },
        AttributeValue::Expression(x) => match Virtual::from_sub(m, x) {
            Ok(v) => ev.expression_matches(&v, &constraint).unwrap_or(true),
            Err(_) => true,
        },
        AttributeValue::Concrete(_) => false,
    };
    if ok { Ok(()) } else { refuse() }
}

/// `Some(ok)` when `constraint` is a concrete range (`int(>#0..)`,
/// `dec(#0..#10)`, `str()`), `None` for an ECL constraint.
fn concrete_range(constraint: &str, value: &AttributeValue) -> Option<bool> {
    let (kind, inner) = ["int(", "dec(", "str(", "bool("]
        .iter()
        .find_map(|k| constraint.strip_prefix(k).map(|r| (&k[..k.len() - 1], r)))?;
    let inner = inner.trim_end().strip_suffix(')').unwrap_or(inner).trim();
    let AttributeValue::Concrete(c) = value else { return Some(false) };
    Some(match (kind, c) {
        ("int", Concrete::Integer(i)) => in_range(inner, *i as f64),
        ("dec", Concrete::Integer(i)) => in_range(inner, *i as f64),
        ("dec", Concrete::Decimal(_, d)) => in_range(inner, *d),
        ("str", Concrete::String(_)) => true,
        ("bool", Concrete::Boolean(_)) => true,
        _ => false,
    })
}

/// `x` within `[>]#lo..[<]#hi` (either end optional; several ranges may be
/// listed, separated by spaces).
fn in_range(spec: &str, x: f64) -> bool {
    if spec.is_empty() {
        return true;
    }
    spec.split_whitespace().any(|r| {
        let Some((lo, hi)) = r.split_once("..") else {
            return r.trim_start_matches('#').parse::<f64>().is_ok_and(|v| v == x);
        };
        let bound = |s: &str, strict_mark: char| -> Option<(f64, bool)> {
            let strict = s.starts_with(strict_mark);
            let s = s.trim_start_matches(strict_mark).trim_start_matches('#');
            s.parse().ok().map(|v| (v, strict))
        };
        let lo_ok = match bound(lo, '>') {
            Some((v, true)) => x > v,
            Some((v, false)) => x >= v,
            None => true,
        };
        let hi_ok = match bound(hi, '<') {
            Some((v, true)) => x < v,
            Some((v, false)) => x <= v,
            None => true,
        };
        lo_ok && hi_ok
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concrete_ranges() {
        let v = |c| AttributeValue::Concrete(c);
        assert_eq!(concrete_range("dec(>#0..)", &v(Concrete::Integer(500))), Some(true));
        assert_eq!(concrete_range("dec(>#0..)", &v(Concrete::Integer(0))), Some(false));
        assert_eq!(
            concrete_range("int(>#0..)", &v(Concrete::Decimal("2.5".into(), 2.5))),
            Some(false)
        );
        assert_eq!(concrete_range("<< 182353008", &v(Concrete::Integer(1))), None);
    }
}
