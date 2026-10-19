//! Normal forms and subsumption of post-coordinated expressions.
//!
//! An expression's normal form replaces its focus concepts by their
//! proximal primitive supertypes, adds the focus concepts' (inferred)
//! relationship groups to its refinement, normalises nested values and drops
//! groups another group makes redundant. `A` subsumes `B` when every primitive
//! focus of `A` subsumes one of `B`'s and every group of `A` subsumes a group
//! of `B` (each of its attributes subsumes one of the other's: type and value
//! both subsume). Equivalence is subsumption both ways.

use super::ecl::{VValue, Virtual, concept_groups};
use super::model::SnomedModel;

/// The virtual form of a single concept.
pub fn of_concept(m: &SnomedModel, c: u32) -> Virtual {
    Virtual { foci: vec![c], groups: concept_groups(m, c) }
}

/// The normal form of `v`.
pub fn normal_form(m: &SnomedModel, v: &Virtual) -> Virtual {
    let mut prims = Vec::new();
    for &f in &v.foci {
        prims.extend(m.proximal_primitives(f));
    }
    let foci = m.most_specific(prims);
    let groups: Vec<Vec<(u32, VValue)>> = v
        .groups
        .iter()
        .map(|g| {
            g.iter()
                .map(|(t, val)| {
                    let val = match val {
                        VValue::Expression(x) => VValue::Expression(Box::new(normal_form(m, x))),
                        other => other.clone(),
                    };
                    (*t, val)
                })
                .collect()
        })
        .collect();
    let mut keep: Vec<Vec<(u32, VValue)>> = Vec::new();
    for (i, g) in groups.iter().enumerate() {
        let redundant = groups.iter().enumerate().any(|(j, h)| {
            j != i && group_subsumes(m, g, h) && (!group_subsumes(m, h, g) || j < i)
        });
        if !redundant {
            keep.push(g.clone());
        }
    }
    Virtual { foci, groups: keep }
}

fn value_subsumes(m: &SnomedModel, a: &VValue, b: &VValue) -> bool {
    match (a, b) {
        (VValue::Concept(x), VValue::Concept(y)) => m.subsumes(*x, *y),
        (VValue::Concrete(x), VValue::Concrete(y)) => match (x.number(), y.number()) {
            (Some(p), Some(q)) => p == q,
            _ => x == y,
        },
        (VValue::Concept(x), VValue::Expression(y)) => {
            nf_subsumes(m, &normal_form(m, &of_concept(m, *x)), y)
        }
        (VValue::Expression(x), VValue::Concept(y)) => {
            nf_subsumes(m, x, &normal_form(m, &of_concept(m, *y)))
        }
        (VValue::Expression(x), VValue::Expression(y)) => nf_subsumes(m, x, y),
        _ => false,
    }
}

fn group_subsumes(m: &SnomedModel, a: &[(u32, VValue)], b: &[(u32, VValue)]) -> bool {
    a.iter().all(|(ta, va)| {
        b.iter()
            .any(|(tb, vb)| m.subsumes(*ta, *tb) && value_subsumes(m, va, vb))
    })
}

/// Whether normal form `a` subsumes normal form `b`.
pub fn nf_subsumes(m: &SnomedModel, a: &Virtual, b: &Virtual) -> bool {
    a.foci.iter().all(|&p| b.foci.iter().any(|&q| m.subsumes(p, q)))
        && a.groups
            .iter()
            .all(|ga| b.groups.iter().any(|gb| group_subsumes(m, ga, gb)))
}

/// The `$subsumes` outcome of `a` against `b`, or `None` when neither
/// subsumes the other (which, for expressions, cannot be told apart from
/// "not subsumed" without a classifier).
pub fn outcome(m: &SnomedModel, a: &Virtual, b: &Virtual) -> Option<&'static str> {
    let (na, nb) = (normal_form(m, a), normal_form(m, b));
    match (nf_subsumes(m, &na, &nb), nf_subsumes(m, &nb, &na)) {
        (true, true) => Some("equivalent"),
        (true, false) => Some("subsumes"),
        (false, true) => Some("subsumed-by"),
        (false, false) => None,
    }
}
