//! The in-memory SNOMED CT concept model of one stored edition: concepts,
//! is-a hierarchy, attribute relationships (with their groups and concrete
//! values), simple reference set membership and the MRCM attribute rules.
//!
//! It answers what SQL over the closure table cannot do cheaply — ECL
//! refinements, post-coordinated expressions, normal forms and the concept
//! model checks — and is built once per stored edition, on first need, from
//! the `snomed_*` tables the RF2 importer fills (see [`ModelRows`]).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use super::scg::Concrete;

/// `900000000000074008 |Not sufficiently defined by necessary conditions|`.
pub const PRIMITIVE: &str = "900000000000074008";
/// `410662002 |Concept model attribute|`.
pub const CONCEPT_MODEL_ATTRIBUTE: u64 = 410662002;
/// `106237007 |Linkage concept|`.
pub const LINKAGE_CONCEPT: u64 = 106237007;

/// The value of an attribute relationship.
#[derive(Debug, Clone, PartialEq)]
pub enum RelValue {
    Concept(u32),
    Concrete(Concrete),
}

/// One active attribute relationship (`group` 0 is ungrouped).
#[derive(Debug, Clone, PartialEq)]
pub struct Rel {
    pub group: u16,
    pub type_id: u32,
    pub value: RelValue,
}

/// An MRCM attribute domain rule.
#[derive(Debug, Clone)]
pub struct DomainRule {
    pub domain: u32,
    /// Whether the attribute belongs in a relationship group.
    pub grouped: bool,
    pub cardinality: (u32, Option<u32>),
    pub in_group_cardinality: (u32, Option<u32>),
}

/// An MRCM attribute range rule.
#[derive(Debug, Clone)]
pub struct RangeRule {
    /// As written in the reference set (`<< 182353008 |Side (qualifier value)|`,
    /// `dec(>#0..)`).
    pub constraint: String,
}

/// Rows the model is built from, as stored by the importer.
#[derive(Debug, Default, Clone)]
pub struct ModelRows {
    /// `(code, active, primitive, display, first term)`.
    pub concepts: Vec<(String, bool, bool, Option<String>, Option<String>)>,
    /// `(source, group, type, value, concrete)`; is-a included.
    pub relationships: Vec<(String, i32, String, String, bool)>,
    /// `(refset, referenced component)`.
    pub refset_members: Vec<(String, String)>,
    /// `(kind, attribute, c1, c2, c3, c4)`: `domain` rows carry
    /// `(domain, grouped, cardinality, in-group cardinality)`, `range` rows
    /// `(range constraint, attribute rule, "", "")`.
    pub mrcm: Vec<(String, String, String, String, String, String)>,
}

/// The concept model of one edition.
#[derive(Debug, Default)]
pub struct SnomedModel {
    /// The stored code system row and its version.
    pub system_id: String,
    pub version: Option<String>,
    pub codes: Vec<u64>,
    index: HashMap<u64, u32>,
    pub active: Vec<bool>,
    pub primitive: Vec<bool>,
    pub display: Vec<Option<Box<str>>>,
    pub first_term: Vec<Option<Box<str>>>,
    pub parents: Vec<Vec<u32>>,
    pub children: Vec<Vec<u32>>,
    pub rels: Vec<Vec<Rel>>,
    /// Reference set → members (concepts only).
    pub refsets: HashMap<u32, HashSet<u32>>,
    pub domains: HashMap<u32, Vec<DomainRule>>,
    pub ranges: HashMap<u32, Vec<RangeRule>>,
}

fn parse_card(s: &str) -> (u32, Option<u32>) {
    let (lo, hi) = s.split_once("..").unwrap_or((s, s));
    (lo.trim().parse().unwrap_or(0), hi.trim().parse().ok())
}

/// Parse a stored concrete value (`#5`, `#1.5`, `"text"`).
pub fn parse_concrete(v: &str) -> Option<Concrete> {
    if let Some(n) = v.strip_prefix('#') {
        if n.contains('.') {
            return n.parse().ok().map(|d| Concrete::Decimal(n.to_string(), d));
        }
        return n.parse().ok().map(Concrete::Integer);
    }
    if let Some(s) = v.strip_prefix('"') {
        return Some(Concrete::String(s.strip_suffix('"').unwrap_or(s).to_string()));
    }
    match v {
        "true" => Some(Concrete::Boolean(true)),
        "false" => Some(Concrete::Boolean(false)),
        _ => None,
    }
}

impl SnomedModel {
    pub fn build(rows: ModelRows) -> Self {
        let mut m = SnomedModel::default();
        for (code, active, primitive, display, first) in rows.concepts {
            let Ok(id) = code.parse::<u64>() else { continue };
            let i = m.codes.len() as u32;
            m.codes.push(id);
            m.index.insert(id, i);
            m.active.push(active);
            m.primitive.push(primitive);
            m.display.push(display.map(String::into_boxed_str));
            m.first_term.push(first.map(String::into_boxed_str));
        }
        let n = m.codes.len();
        m.parents = vec![Vec::new(); n];
        m.children = vec![Vec::new(); n];
        m.rels = vec![Vec::new(); n];
        for (src, group, ty, value, concrete) in rows.relationships {
            if ty == "116680003" {
                if let (Some(s), Some(v)) = (m.idx(&src), m.idx(&value)) {
                    if !m.parents[s as usize].contains(&v) {
                        m.parents[s as usize].push(v);
                        m.children[v as usize].push(s);
                    }
                }
                continue;
            }
            let (Some(s), Some(t)) = (m.idx(&src), m.idx(&ty)) else { continue };
            if concrete {
                if let Some(c) = parse_concrete(&value) {
                    m.rels[s as usize].push(Rel {
                        group: group.max(0) as u16,
                        type_id: t,
                        value: RelValue::Concrete(c),
                    });
                }
                continue;
            }
            let Some(v) = m.idx(&value) else { continue };
            m.rels[s as usize].push(Rel {
                group: group.max(0) as u16,
                type_id: t,
                value: RelValue::Concept(v),
            });
        }
        for (refset, member) in rows.refset_members {
            if let (Some(r), Some(c)) = (m.idx(&refset), m.idx(&member)) {
                m.refsets.entry(r).or_default().insert(c);
            }
        }
        for (kind, attr, c1, c2, c3, c4) in rows.mrcm {
            let Some(a) = m.idx(&attr) else { continue };
            match kind.as_str() {
                "domain" => {
                    if let Some(d) = m.idx(&c1) {
                        m.domains.entry(a).or_default().push(DomainRule {
                            domain: d,
                            grouped: c2 == "1",
                            cardinality: parse_card(&c3),
                            in_group_cardinality: parse_card(&c4),
                        });
                    }
                }
                "range" => m.ranges.entry(a).or_default().push(RangeRule { constraint: c1 }),
                _ => {}
            }
        }
        m
    }

    pub fn len(&self) -> usize {
        self.codes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    /// The index of concept `code`.
    pub fn idx(&self, code: &str) -> Option<u32> {
        code.parse::<u64>().ok().and_then(|c| self.index.get(&c).copied())
    }

    pub fn code(&self, i: u32) -> String {
        self.codes[i as usize].to_string()
    }

    /// The preferred term, falling back to the first description.
    pub fn display_of(&self, i: u32) -> Option<&str> {
        self.display[i as usize]
            .as_deref()
            .or(self.first_term[i as usize].as_deref())
    }

    /// The first active description in id order (how tx.fhir.org names a
    /// concept in concept model messages).
    pub fn first_term_of(&self, i: u32) -> Option<&str> {
        self.first_term[i as usize]
            .as_deref()
            .or(self.display[i as usize].as_deref())
    }

    /// `code |term|` for messages.
    pub fn label(&self, i: u32) -> String {
        match self.first_term_of(i) {
            Some(t) => format!("{} |{t}|", self.codes[i as usize]),
            None => self.codes[i as usize].to_string(),
        }
    }

    pub fn ancestors(&self, i: u32) -> HashSet<u32> {
        let mut out = HashSet::new();
        let mut stack = vec![i];
        while let Some(x) = stack.pop() {
            for &p in &self.parents[x as usize] {
                if out.insert(p) {
                    stack.push(p);
                }
            }
        }
        out
    }

    pub fn descendants(&self, i: u32) -> HashSet<u32> {
        let mut out = HashSet::new();
        let mut stack = vec![i];
        while let Some(x) = stack.pop() {
            for &c in &self.children[x as usize] {
                if out.insert(c) {
                    stack.push(c);
                }
            }
        }
        out
    }

    /// Whether `a` is `b` or one of its ancestors.
    pub fn subsumes(&self, a: u32, b: u32) -> bool {
        if a == b {
            return true;
        }
        let mut seen = HashSet::new();
        let mut stack = vec![b];
        while let Some(x) = stack.pop() {
            for &p in &self.parents[x as usize] {
                if p == a {
                    return true;
                }
                if seen.insert(p) {
                    stack.push(p);
                }
            }
        }
        false
    }

    /// Whether some member of `set` subsumes `b`.
    pub fn subsumed_by_any(&self, set: &[u32], b: u32) -> bool {
        set.iter().any(|&a| self.subsumes(a, b))
    }

    /// The proximal primitive supertypes of `i` (itself when primitive).
    pub fn proximal_primitives(&self, i: u32) -> Vec<u32> {
        if self.primitive[i as usize] {
            return vec![i];
        }
        let prims: Vec<u32> = self
            .ancestors(i)
            .into_iter()
            .filter(|&a| self.primitive[a as usize])
            .collect();
        self.most_specific(prims)
    }

    /// `set` without the members that subsume another member.
    pub fn most_specific(&self, mut set: Vec<u32>) -> Vec<u32> {
        set.sort_unstable();
        set.dedup();
        let keep: Vec<u32> = set
            .iter()
            .copied()
            .filter(|&a| !set.iter().any(|&b| b != a && self.subsumes(a, b)))
            .collect();
        keep
    }

    pub fn is_refset(&self, i: u32) -> bool {
        self.refsets.contains_key(&i)
    }
}

/// Built models by their import stamp; the few most recent are kept.
static MODELS: Mutex<Vec<(String, Arc<SnomedModel>)>> = Mutex::new(Vec::new());
const MODELS_KEPT: usize = 3;

/// The model cached for `stamp`.
pub fn cached(stamp: &str) -> Option<Arc<SnomedModel>> {
    let mut models = MODELS.lock().unwrap_or_else(|e| e.into_inner());
    let pos = models.iter().position(|(s, _)| s == stamp)?;
    let entry = models.remove(pos);
    let model = entry.1.clone();
    models.push(entry);
    Some(model)
}

/// Cache `model` under `stamp`.
pub fn remember(stamp: &str, model: SnomedModel) -> Arc<SnomedModel> {
    let model = Arc::new(model);
    let mut models = MODELS.lock().unwrap_or_else(|e| e.into_inner());
    models.retain(|(s, _)| s != stamp);
    if models.len() >= MODELS_KEPT {
        models.remove(0);
    }
    models.push((stamp.to_string(), model.clone()));
    model
}
