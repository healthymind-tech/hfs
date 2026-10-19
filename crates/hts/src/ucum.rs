//! UCUM (`http://unitsofmeasure.org`) as a grammar code system.
//!
//! The stored UCUM content (ucum-essence) lists atoms, not the expressions
//! people use (`mg/dL`, `kg.m/s2`, `{tbl}`); like tx.fhir.org, HTS judges
//! UCUM codes by their grammar and meaning:
//!
//! * **Canonical form**: a unit reduces to a magnitude times a product of
//!   base dimensions. As in UCUM, the mole is a number (6.02214076e23), so
//!   it carries no dimension — `g/mol` has the canonical unit `g`.
//!   Annotations (`{…}`) carry no meaning.
//! * **Subsumption**: UCUM has no hierarchy. Two codes are `equivalent`
//!   when they reduce to the same dimensions *and* the same magnitude
//!   (`1/min` = `min-1`, `N` = `kg.m/s2`, `mg{total}` = `mg`); anything else
//!   — including `m` against `cm`, or `mg/dL` against `g/L` — is
//!   `not-subsumed`. A special unit with an offset or logarithmic scale
//!   (`Cel`, `[pH]`) does not reduce, and is `not-subsumed` from anything
//!   but itself.
//! * **Value sets** over UCUM may take the whole grammar, enumerate codes,
//!   or filter on `canonical = <unit>` (codes whose canonical unit is that
//!   unit's). The whole grammar and filters cannot be listed; their
//!   expansions are based on the common units value set
//!   (`http://hl7.org/fhir/ValueSet/ucum-common`) and marked unclosed.

use crate::types::SubsumptionOutcome;
use crate::ucum_validate::{UCUM_URL, is_valid_ucum_code};

/// The common UCUM units value set expansions of the grammar are based on.
pub const UCUM_COMMON: &str = "http://hl7.org/fhir/ValueSet/ucum-common";
/// The UCUM release whose definitions HTS uses.
pub const UCUM_VERSION: &str = "2.2";
/// Why an expansion over the UCUM grammar is incomplete.
pub const UNCLOSED_REASON: &str = "The code System 'http://unitsofmeasure.org' has a grammar and \
     so has infinite members. This extension is based on http://hl7.org/fhir/ValueSet/ucum-common";

/// A reduced unit: magnitude, base dimensions (g, m, s, rad, K, C, cd —
/// UCUM's base units) and the arbitrary units it is made of (`[IU]`
/// compares only with itself).
#[derive(Debug, Clone, PartialEq)]
pub struct Canonical {
    pub factor: f64,
    pub dims: [i32; 7],
    pub arbitrary: Vec<String>,
}

const BASE: [&str; 7] = ["g", "m", "s", "rad", "K", "C", "cd"];

struct UnitDef {
    name: String,
    metric: bool,
    special: bool,
    arbitrary: bool,
    value: f64,
    unit: String,
}

struct Model {
    prefixes: std::collections::HashMap<String, (f64, String)>,
    base: std::collections::HashMap<String, (usize, String)>,
    units: std::collections::HashMap<String, UnitDef>,
}

static MODEL: std::sync::LazyLock<Model> = std::sync::LazyLock::new(|| {
    let xml = include_str!("../terminology-data/ucum-essence-v2.2.xml");
    let doc = roxmltree::Document::parse(xml).expect("embedded ucum-essence parses");
    let mut m = Model {
        prefixes: Default::default(),
        base: Default::default(),
        units: Default::default(),
    };
    let child_text = |n: roxmltree::Node, tag: &str| {
        n.children()
            .find(|c| c.has_tag_name(tag))
            .and_then(|c| c.text())
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    for n in doc.root_element().children().filter(|n| n.is_element()) {
        let Some(code) = n.attribute("Code") else {
            continue;
        };
        let name = child_text(n, "name");
        match n.tag_name().name() {
            "prefix" => {
                let v = n
                    .children()
                    .find(|c| c.has_tag_name("value"))
                    .and_then(|c| c.attribute("value"))
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(1.0);
                m.prefixes.insert(code.to_string(), (v, name));
            }
            "base-unit" => {
                let dim = match n.attribute("dim") {
                    Some("M") => 0,
                    Some("L") => 1,
                    Some("T") => 2,
                    Some("A") => 3,
                    Some("C") => 4,
                    Some("Q") => 5,
                    _ => 6,
                };
                m.base.insert(code.to_string(), (dim, name));
            }
            "unit" => {
                let value = n.children().find(|c| c.has_tag_name("value"));
                m.units.insert(
                    code.to_string(),
                    UnitDef {
                        name,
                        metric: n.attribute("isMetric") == Some("yes"),
                        special: n.attribute("isSpecial") == Some("yes"),
                        arbitrary: n.attribute("isArbitrary") == Some("yes"),
                        value: value
                            .and_then(|v| v.attribute("value"))
                            .and_then(|v| v.parse::<f64>().ok())
                            .unwrap_or(1.0),
                        unit: value
                            .and_then(|v| v.attribute("Unit"))
                            .unwrap_or("1")
                            .to_string(),
                    },
                );
            }
            _ => {}
        }
    }
    m
});

impl Canonical {
    fn one() -> Self {
        Canonical {
            factor: 1.0,
            dims: [0; 7],
            arbitrary: Vec::new(),
        }
    }
    fn mul(mut self, o: &Canonical, sign: i32) -> Self {
        self.factor *= if sign > 0 { o.factor } else { 1.0 / o.factor };
        for i in 0..7 {
            self.dims[i] += sign * o.dims[i];
        }
        for a in &o.arbitrary {
            self.arbitrary
                .push(if sign > 0 { a.clone() } else { format!("/{a}") });
        }
        self.arbitrary.sort();
        self
    }
    fn pow(mut self, e: i32) -> Self {
        self.factor = self.factor.powi(e);
        for d in &mut self.dims {
            *d *= e;
        }
        if e != 1 {
            self.arbitrary = self.arbitrary.iter().map(|a| format!("{a}^{e}")).collect();
        }
        self
    }
}

/// `(prefix, atom)` naming `symbol`, the atom a defined unit or base unit.
fn resolve_atom(symbol: &str) -> Option<(Option<&'static str>, &'static str)> {
    let m = &*MODEL;
    if let Some((k, _)) = m.units.get_key_value(symbol) {
        return Some((None, k.as_str()));
    }
    if let Some((k, _)) = m.base.get_key_value(symbol) {
        return Some((None, k.as_str()));
    }
    for (p, _) in m.prefixes.iter() {
        if let Some(rest) = symbol.strip_prefix(p.as_str()) {
            let metric = m
                .units
                .get(rest)
                .map(|u| u.metric)
                .or(m.base.get(rest).map(|_| true));
            if metric == Some(true) {
                let atom = m
                    .units
                    .get_key_value(rest)
                    .map(|(k, _)| k.as_str())
                    .or_else(|| m.base.get_key_value(rest).map(|(k, _)| k.as_str()))?;
                return Some((Some(p.as_str()), atom));
            }
        }
    }
    None
}

/// Split a trailing exponent off a unit symbol (`m2`, `s-1`, `10*3`).
fn split_exponent(term: &str) -> (&str, i32) {
    if resolve_atom(term).is_some() {
        return (term, 1);
    }
    let bytes = term.as_bytes();
    let mut cut = term.len();
    while cut > 0 && bytes[cut - 1].is_ascii_digit() {
        cut -= 1;
    }
    if cut > 0 && cut < term.len() && matches!(bytes[cut - 1], b'+' | b'-') {
        cut -= 1;
    }
    if cut == 0 || cut == term.len() {
        return (term, 1);
    }
    match term[cut..].parse::<i32>() {
        Ok(e) => (&term[..cut], e),
        Err(_) => (term, 1),
    }
}

fn eval_atom(symbol: &str, depth: usize) -> Option<Canonical> {
    let m = &*MODEL;
    if depth > 40 {
        return None;
    }
    let (prefix, atom) = resolve_atom(symbol)?;
    let pf = prefix.map_or(1.0, |p| m.prefixes[p].0);
    let mut c = if let Some((dim, _)) = m.base.get(atom) {
        let mut c = Canonical::one();
        c.dims[*dim] = 1;
        c
    } else {
        let u = &m.units[atom];
        if u.special {
            return None;
        }
        if u.arbitrary {
            let mut c = Canonical::one();
            c.arbitrary.push(atom.to_string());
            c
        } else {
            let mut c = eval_expr(&u.unit, depth + 1)?;
            c.factor *= u.value;
            c
        }
    };
    c.factor *= pf;
    Some(c)
}

/// Evaluate a UCUM expression (no annotations). `None` when it cannot be
/// reduced (special units, syntax this evaluator does not take).
fn eval_expr(expr: &str, depth: usize) -> Option<Canonical> {
    let chars: Vec<char> = expr.chars().collect();
    let mut pos = 0;
    let c = eval_term(&chars, &mut pos, depth)?;
    (pos == chars.len()).then_some(c)
}

fn eval_term(chars: &[char], pos: &mut usize, depth: usize) -> Option<Canonical> {
    let mut acc = Canonical::one();
    let mut sign = 1;
    if chars.get(*pos) == Some(&'/') {
        sign = -1;
        *pos += 1;
    }
    loop {
        let comp = eval_component(chars, pos, depth)?;
        acc = acc.mul(&comp, sign);
        match chars.get(*pos) {
            Some('.') => sign = 1,
            Some('/') => sign = -1,
            _ => return Some(acc),
        }
        *pos += 1;
    }
}

fn eval_component(chars: &[char], pos: &mut usize, depth: usize) -> Option<Canonical> {
    if chars.get(*pos) == Some(&'(') {
        *pos += 1;
        let inner = eval_term(chars, pos, depth)?;
        if chars.get(*pos) != Some(&')') {
            return None;
        }
        *pos += 1;
        // An exponent on a parenthesised term.
        let start = *pos;
        while *pos < chars.len()
            && (chars[*pos].is_ascii_digit() || (*pos == start && matches!(chars[*pos], '+' | '-')))
        {
            *pos += 1;
        }
        let e: String = chars[start..*pos].iter().collect();
        return Some(if e.is_empty() {
            inner
        } else {
            inner.pow(e.parse().ok()?)
        });
    }
    let start = *pos;
    let mut bracket = 0;
    while *pos < chars.len() {
        match chars[*pos] {
            '[' => bracket += 1,
            ']' => bracket -= 1,
            '.' | '/' | '(' | ')' if bracket == 0 => break,
            _ => {}
        }
        *pos += 1;
    }
    let term: String = chars[start..*pos].iter().collect();
    if term.is_empty() {
        return None;
    }
    if term.bytes().all(|b| b.is_ascii_digit()) {
        let mut c = Canonical::one();
        c.factor = term.parse().ok()?;
        return Some(c);
    }
    let (symbol, e) = split_exponent(&term);
    Some(eval_atom(symbol, depth)?.pow(e))
}

/// `code` without its annotations (stand-alone ones become `1`).
fn without_annotations(code: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for c in code.chars() {
        match c {
            '{' => {
                inside = true;
                if out.is_empty() || out.ends_with(['/', '.', '(']) {
                    out.push('1');
                }
            }
            '}' => inside = false,
            _ if !inside => out.push(c),
            _ => {}
        }
    }
    out
}

/// The canonical form of a valid UCUM code, or `None` for a special unit
/// (offset or logarithmic) that does not reduce.
pub fn canonical(code: &str) -> Option<Canonical> {
    eval_expr(&without_annotations(code), 0)
}

/// The canonical unit of `c`, written in UCUM base units (`g`, `m3`, …).
pub fn canonical_unit(c: &Canonical) -> String {
    let parts: Vec<String> = c
        .dims
        .iter()
        .enumerate()
        .filter(|(_, e)| **e != 0)
        .map(|(i, e)| {
            if *e == 1 {
                BASE[i].to_string()
            } else {
                format!("{}{e}", BASE[i])
            }
        })
        .collect();
    if parts.is_empty() {
        "1".into()
    } else {
        parts.join(".")
    }
}

fn same_magnitude(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() <= 1e-9 * a.abs().max(b.abs())
}

/// How `a` relates to `b`; `Err(code)` names the first code that is not a
/// valid UCUM unit.
pub fn subsumes<'a>(a: &'a str, b: &'a str) -> Result<SubsumptionOutcome, &'a str> {
    for code in [a, b] {
        if !is_valid_ucum_code(code) {
            return Err(code);
        }
    }
    if without_annotations(a) == without_annotations(b) {
        return Ok(SubsumptionOutcome::Equivalent);
    }
    Ok(match (canonical(a), canonical(b)) {
        (Some(ca), Some(cb)) if ca.dims == cb.dims && same_magnitude(ca.factor, cb.factor) => {
            SubsumptionOutcome::Equivalent
        }
        _ => SubsumptionOutcome::NotSubsumed,
    })
}

/// One lexical piece of a UCUM expression.
enum Token {
    Op(char),
    Open,
    Close,
    Annotation,
    Number(String),
    /// A unit symbol (prefix + atom) with an optional exponent, and where it
    /// starts (1-based).
    Unit {
        text: String,
        pos: usize,
    },
}

fn tokens(code: &str) -> Vec<Token> {
    let chars: Vec<char> = code.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '.' | '/' => {
                out.push(Token::Op(chars[i]));
                i += 1;
            }
            '(' => {
                out.push(Token::Open);
                i += 1;
            }
            ')' => {
                out.push(Token::Close);
                i += 1;
            }
            '{' => {
                while i < chars.len() && chars[i] != '}' {
                    i += 1;
                }
                i += 1;
                out.push(Token::Annotation);
            }
            _ => {
                let start = i;
                let mut depth = 0;
                while i < chars.len() {
                    let c = chars[i];
                    if c == '[' {
                        depth += 1;
                    } else if c == ']' {
                        depth -= 1;
                    } else if depth == 0 && matches!(c, '.' | '/' | '(' | ')' | '{') {
                        break;
                    }
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                if text.chars().all(|c| c.is_ascii_digit())
                    || text.contains('*') && text.starts_with("10")
                {
                    out.push(Token::Number(text));
                } else {
                    out.push(Token::Unit {
                        text,
                        pos: start + 1,
                    });
                }
            }
        }
    }
    out
}

/// `(display name, exponent)` of a unit symbol with optional prefix and
/// exponent (`mg`, `dL`, `m2`, `s-1`).
fn unit_name(symbol: &str) -> Option<(String, Option<String>)> {
    let m = &*MODEL;
    let (atom_symbol, e) = split_exponent(symbol);
    let (prefix, atom) = resolve_atom(atom_symbol)?;
    let name = m
        .units
        .get(atom)
        .map(|u| u.name.clone())
        .or_else(|| m.base.get(atom).map(|b| b.1.clone()))?;
    let name = match prefix {
        Some(p) => format!("{}{name}", m.prefixes[p].1),
        None => name,
    };
    Some((name, (e != 1).then(|| e.to_string())))
}

/// The tx.fhir.org-style description of a valid UCUM code:
/// `mg/dL` → `(milligram) / (deciliter)`.
pub fn description(code: &str) -> String {
    let mut out = String::new();
    let mut after_unit = false;
    for t in tokens(code) {
        match t {
            Token::Op('.') => {
                out.push_str(" * ");
                after_unit = false;
            }
            Token::Op(_) => {
                out.push_str(" / ");
                after_unit = false;
            }
            Token::Open => out.push('('),
            Token::Close => out.push(')'),
            Token::Annotation => {
                out.push_str(if after_unit { " * 1" } else { "1" });
            }
            Token::Number(n) => {
                out.push_str(&n);
                after_unit = true;
            }
            Token::Unit { text, .. } => {
                match unit_name(&text) {
                    Some((name, Some(exp))) => out.push_str(&format!("({name}) ^ {exp}")),
                    Some((name, None)) => out.push_str(&format!("({name})")),
                    None => out.push_str(&text),
                }
                after_unit = true;
            }
        }
    }
    out
}

/// The tx.fhir.org-style reason a code is not a valid UCUM unit.
pub fn error_message(code: &str) -> String {
    for t in tokens(code) {
        if let Token::Unit { text, pos } = t {
            if unit_name(&text).is_none() {
                return format!(
                    "Error processing unit '{code}': The unit '{text}' is unknown at character {pos}"
                );
            }
        }
    }
    format!("Error processing unit '{code}': The unit is not a valid UCUM expression")
}

fn ucum_includes<'a>(vs: &'a serde_json::Value, key: &str) -> Vec<&'a serde_json::Value> {
    vs.get("compose")
        .and_then(|c| c.get(key))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

/// Whether `vs` draws only on UCUM (and does not enumerate it — an
/// enumerated value set is an ordinary list).
pub fn is_ucum_grammar_value_set(vs: &serde_json::Value) -> bool {
    let includes = ucum_includes(vs, "include");
    !includes.is_empty()
        && includes
            .iter()
            .chain(ucum_includes(vs, "exclude").iter())
            .all(|inc| {
                inc.get("system").and_then(|s| s.as_str()) == Some(UCUM_URL)
                    && inc.get("valueSet").is_none()
            })
        && includes.iter().any(|inc| {
            inc.get("concept")
                .and_then(|c| c.as_array())
                .is_none_or(|a| a.is_empty())
        })
}

fn include_matches(inc: &serde_json::Value, code: &str) -> bool {
    if let Some(concepts) = inc
        .get("concept")
        .and_then(|c| c.as_array())
        .filter(|a| !a.is_empty())
    {
        return concepts
            .iter()
            .any(|c| c.get("code").and_then(|v| v.as_str()) == Some(code));
    }
    let Some(filters) = inc.get("filter").and_then(|f| f.as_array()) else {
        return true;
    };
    filters.iter().all(|f| {
        let value = f.get("value").and_then(|v| v.as_str()).unwrap_or_default();
        match (
            f.get("property").and_then(|v| v.as_str()),
            f.get("op").and_then(|v| v.as_str()),
        ) {
            (Some("canonical"), Some("=")) => match (canonical(code), canonical(value)) {
                (Some(a), Some(b)) => a.dims == b.dims,
                _ => false,
            },
            _ => false,
        }
    })
}

/// Whether the valid UCUM code `code` is in the UCUM value set `vs`.
pub fn in_value_set(vs: &serde_json::Value, code: &str) -> bool {
    is_valid_ucum_code(code)
        && ucum_includes(vs, "include")
            .iter()
            .any(|inc| include_matches(inc, code))
        && !ucum_includes(vs, "exclude")
            .iter()
            .any(|inc| include_matches(inc, code))
}

/// Whether `vs` takes the whole UCUM grammar (an include with neither
/// concepts nor filters).
pub fn takes_whole_grammar(vs: &serde_json::Value) -> bool {
    ucum_includes(vs, "include").iter().any(|inc| {
        ["concept", "filter"].iter().all(|k| {
            inc.get(*k)
                .and_then(|v| v.as_array())
                .is_none_or(|a| a.is_empty())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use SubsumptionOutcome::*;

    #[test]
    fn subsumption_compares_canonical_forms_and_magnitudes() {
        assert_eq!(subsumes("mg/dL", "mg/dL"), Ok(Equivalent));
        assert_eq!(subsumes("1/min", "min-1"), Ok(Equivalent));
        assert_eq!(subsumes("N", "kg.m/s2"), Ok(Equivalent));
        assert_eq!(subsumes("mg{total}", "mg"), Ok(Equivalent));
        assert_eq!(subsumes("mg/dL", "mm[Hg]"), Ok(NotSubsumed));
        assert_eq!(subsumes("m", "cm"), Ok(NotSubsumed));
        assert_eq!(subsumes("mg/dL", "g/L"), Ok(NotSubsumed));
        assert_eq!(subsumes("[in_i]", "cm"), Ok(NotSubsumed));
        assert_eq!(subsumes("%", "1"), Ok(NotSubsumed));
        assert_eq!(subsumes("Cel", "K"), Ok(NotSubsumed));
        assert_eq!(subsumes("not-a-unit", "mg/dL"), Err("not-a-unit"));
    }

    #[test]
    fn canonical_units_treat_the_mole_as_a_number() {
        let g = canonical("g").unwrap();
        assert_eq!(canonical_unit(&g), "g");
        assert_eq!(canonical("g/mol").unwrap().dims, g.dims);
        assert_eq!(canonical_unit(&canonical("mL").unwrap()), "m3");
    }

    #[test]
    fn descriptions_and_errors_read_like_tx_fhir_org() {
        assert_eq!(description("mg/dL"), "(milligram) / (deciliter)");
        assert_eq!(
            description("mg/dL{milligram per deciliter}"),
            "(milligram) / (deciliter) * 1"
        );
        assert_eq!(
            error_message("kgXX"),
            "Error processing unit 'kgXX': The unit 'kgXX' is unknown at character 1"
        );
    }
}

/// The concepts (code, display) of the stored common UCUM units value set.
pub async fn common_units<B: crate::traits::TerminologyBackend>(
    backend: &B,
) -> (Vec<(String, Option<String>)>, Option<serde_json::Value>) {
    let vs = crate::traits::ValueSetOperations::search(
        backend,
        &helios_persistence::tenant::TenantContext::system(),
        crate::types::ResourceSearchQuery {
            url: Some(UCUM_COMMON.to_string()),
            count: Some(1),
            ..Default::default()
        },
    )
    .await
    .ok()
    .and_then(|mut v| v.pop());
    let mut out = Vec::new();
    if let Some(vs) = vs.as_ref() {
        for inc in ucum_includes(vs, "include") {
            for c in inc
                .get("concept")
                .and_then(|c| c.as_array())
                .into_iter()
                .flatten()
            {
                if let Some(code) = c.get("code").and_then(|v| v.as_str()) {
                    out.push((
                        code.to_string(),
                        c.get("display")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                    ));
                }
            }
        }
    }
    (out, vs)
}

/// The version of the stored UCUM code system, else the one HTS implements.
pub async fn stored_version<B: crate::traits::TerminologyBackend>(backend: &B) -> String {
    crate::traits::CodeSystemOperations::search(
        backend,
        &helios_persistence::tenant::TenantContext::system(),
        crate::types::ResourceSearchQuery {
            url: Some(UCUM_URL.to_string()),
            count: Some(1),
            ..Default::default()
        },
    )
    .await
    .ok()
    .and_then(|mut v| v.pop())
    .and_then(|cs| {
        cs.get("version")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    })
    .unwrap_or_else(|| UCUM_VERSION.to_string())
}
