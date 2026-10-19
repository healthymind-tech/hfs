//! SNOMED CT Expression Constraint Language (ECL 2.x): parser and an
//! evaluator over the in-memory [`SnomedModel`].
//!
//! Covers the constraint operators (`<`, `<<`, `<!`, `<<!`, `>`, `>>`, `>!`,
//! `>>!`, `!!>`, `!!<`), member-of (`^`), wildcards, conjunction /
//! disjunction / exclusion (which, as the specification requires, may not be
//! mixed without brackets), refinements with attribute groups, cardinality,
//! reverse attributes (`R`), `!=`, numeric, string and boolean comparisons on
//! concrete values, dotted attributes (`.`), and the concept filters
//! `{{ C active = … }}`, `{{ C definitionStatus = … }}` and the description
//! filter `{{ D term = "…" }}` (matched against the preferred term and the
//! first description, which is what the model holds).
//!
//! Besides evaluating a constraint to the concepts it selects, it tests
//! whether a post-coordinated expression satisfies a constraint
//! ([`Evaluator::expression_matches`]): an expression refines its focus
//! concepts, so it is a descendant of each of their ancestors-or-self (never
//! a child, parent, ancestor or reference set member) and carries their
//! relationships plus its own refinement.

use std::collections::{HashMap, HashSet};

use super::model::{RelValue, SnomedModel};
use super::scg::{self, AttributeValue, Concrete};

/// A constraint operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    DescendantOf,
    DescendantOrSelfOf,
    ChildOf,
    ChildOrSelfOf,
    AncestorOf,
    AncestorOrSelfOf,
    ParentOf,
    ParentOrSelfOf,
    Top,
    Bottom,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Focus {
    Concept(String),
    Wildcard,
    /// `^ x`: members of the reference sets `x` selects.
    MemberOf(Box<Ecl>),
    Nested(Box<Ecl>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    Active(bool),
    Primitive(bool),
    /// Words that must each start a word of the term.
    Term(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Ecl {
    Sub { op: Option<Op>, focus: Focus, filters: Vec<Filter> },
    Refined(Box<Ecl>, Refinement),
    Dotted(Box<Ecl>, Vec<Ecl>),
    And(Vec<Ecl>),
    Or(Vec<Ecl>),
    Minus(Box<Ecl>, Box<Ecl>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cardinality {
    pub min: u32,
    pub max: Option<u32>,
}

impl Cardinality {
    fn allows(&self, n: u32) -> bool {
        n >= self.min && self.max.is_none_or(|m| n <= m)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Comparison {
    Expression { negated: bool, value: Box<Ecl> },
    Number { op: NumOp, value: f64 },
    String { negated: bool, value: String },
    Boolean { negated: bool, value: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttributeConstraint {
    pub cardinality: Option<Cardinality>,
    pub reverse: bool,
    pub name: Box<Ecl>,
    pub comparison: Comparison,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Refinement {
    Attribute(AttributeConstraint),
    Group(Option<Cardinality>, Box<Refinement>),
    And(Vec<Refinement>),
    Or(Vec<Refinement>),
}

/// A parse error.
#[derive(Debug, Clone, PartialEq)]
pub struct EclError(pub String);

impl std::fmt::Display for EclError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ── Parser ──────────────────────────────────────────────────────────────────

pub fn parse(text: &str) -> Result<Ecl, EclError> {
    let mut p = P { s: text.as_bytes(), text, pos: 0 };
    let e = p.expression_constraint()?;
    p.ws();
    if p.pos < p.s.len() {
        return Err(p.err("unexpected text"));
    }
    Ok(e)
}

struct P<'a> {
    s: &'a [u8],
    text: &'a str,
    pos: usize,
}

impl P<'_> {
    fn err(&self, what: &str) -> EclError {
        EclError(format!("{what} at character {}", self.pos + 1))
    }

    fn ws(&mut self) {
        loop {
            while self.pos < self.s.len() && self.s[self.pos].is_ascii_whitespace() {
                self.pos += 1;
            }
            // Comments: /* … */
            if self.s[self.pos..].starts_with(b"/*") {
                match self.text[self.pos + 2..].find("*/") {
                    Some(e) => self.pos += e + 4,
                    None => self.pos = self.s.len(),
                }
            } else {
                return;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn starts(&self, t: &str) -> bool {
        self.s[self.pos..].starts_with(t.as_bytes())
    }

    fn eat(&mut self, t: &str) -> bool {
        if self.starts(t) {
            self.pos += t.len();
            true
        } else {
            false
        }
    }

    /// A keyword (`AND`, `OR`, `MINUS`), case-insensitive, followed by space
    /// or a bracket.
    fn keyword(&mut self, k: &str) -> bool {
        let end = self.pos + k.len();
        if end <= self.s.len()
            && self.text[self.pos..end].eq_ignore_ascii_case(k)
            && self
                .s
                .get(end)
                .is_none_or(|c| c.is_ascii_whitespace() || *c == b'(')
        {
            self.pos = end;
            true
        } else {
            false
        }
    }

    fn conjunction(&mut self) -> bool {
        self.eat(",") || self.keyword("AND")
    }

    fn expression_constraint(&mut self) -> Result<Ecl, EclError> {
        self.ws();
        let first = self.sub_expression_constraint()?;
        self.ws();
        if self.peek() == Some(b':') {
            self.pos += 1;
            self.ws();
            let r = self.refinement()?;
            return Ok(Ecl::Refined(Box::new(first), r));
        }
        if self.peek() == Some(b'.') {
            let mut attrs = Vec::new();
            while self.peek() == Some(b'.') {
                self.pos += 1;
                self.ws();
                attrs.push(self.sub_expression_constraint()?);
                self.ws();
            }
            return Ok(Ecl::Dotted(Box::new(first), attrs));
        }
        let save = self.pos;
        if self.conjunction() {
            let mut items = vec![first];
            loop {
                self.ws();
                items.push(self.sub_expression_constraint()?);
                self.ws();
                if !self.conjunction() {
                    break;
                }
            }
            self.mixed_check()?;
            return Ok(Ecl::And(items));
        }
        self.pos = save;
        if self.keyword("OR") {
            let mut items = vec![first];
            loop {
                self.ws();
                items.push(self.sub_expression_constraint()?);
                self.ws();
                if !self.keyword("OR") {
                    break;
                }
            }
            self.mixed_check()?;
            return Ok(Ecl::Or(items));
        }
        if self.keyword("MINUS") {
            self.ws();
            let second = self.sub_expression_constraint()?;
            self.mixed_check()?;
            return Ok(Ecl::Minus(Box::new(first), Box::new(second)));
        }
        self.pos = save;
        Ok(first)
    }

    /// Conjunction, disjunction and exclusion may not be mixed without
    /// brackets (ECL 2: their precedence is deliberately undefined).
    fn mixed_check(&mut self) -> Result<(), EclError> {
        self.ws();
        let save = self.pos;
        let mixed = self.conjunction() || self.keyword("OR") || self.keyword("MINUS");
        self.pos = save;
        if mixed {
            return Err(self.err(
                "AND, OR and MINUS cannot be combined without brackets to show their precedence",
            ));
        }
        Ok(())
    }

    fn operator(&mut self) -> Option<Op> {
        for (t, op) in [
            ("<<!", Op::ChildOrSelfOf),
            ("<!", Op::ChildOf),
            ("<<", Op::DescendantOrSelfOf),
            (">>!", Op::ParentOrSelfOf),
            (">!", Op::ParentOf),
            (">>", Op::AncestorOrSelfOf),
            ("!!>", Op::Top),
            ("!!<", Op::Bottom),
            ("<", Op::DescendantOf),
            (">", Op::AncestorOf),
        ] {
            if self.eat(t) {
                return Some(op);
            }
        }
        for (k, op) in [
            ("childOrSelfOf", Op::ChildOrSelfOf),
            ("childOf", Op::ChildOf),
            ("descendantOrSelfOf", Op::DescendantOrSelfOf),
            ("descendantOf", Op::DescendantOf),
            ("parentOrSelfOf", Op::ParentOrSelfOf),
            ("parentOf", Op::ParentOf),
            ("ancestorOrSelfOf", Op::AncestorOrSelfOf),
            ("ancestorOf", Op::AncestorOf),
        ] {
            if self.keyword(k) {
                return Some(op);
            }
        }
        None
    }

    fn sub_expression_constraint(&mut self) -> Result<Ecl, EclError> {
        let op = self.operator();
        self.ws();
        let member_of = self.eat("^") || self.keyword("memberOf");
        self.ws();
        let mut focus = self.focus()?;
        if member_of {
            focus = Focus::MemberOf(Box::new(Ecl::Sub { op: None, focus, filters: vec![] }));
        }
        let mut filters = Vec::new();
        loop {
            self.ws();
            if !self.starts("{{") {
                break;
            }
            self.pos += 2;
            filters.extend(self.filter_constraint()?);
        }
        Ok(Ecl::Sub { op, focus, filters })
    }

    fn focus(&mut self) -> Result<Focus, EclError> {
        match self.peek() {
            Some(b'*') => {
                self.pos += 1;
                Ok(Focus::Wildcard)
            }
            Some(b'(') => {
                self.pos += 1;
                let inner = self.expression_constraint()?;
                self.ws();
                if !self.eat(")") {
                    return Err(self.err("expected ')'"));
                }
                Ok(Focus::Nested(Box::new(inner)))
            }
            _ => self.concept_reference().map(Focus::Concept),
        }
    }

    fn concept_reference(&mut self) -> Result<String, EclError> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_alphanumeric()) {
            self.pos += 1;
        }
        let id = &self.text[start..self.pos];
        if id.is_empty() {
            return Err(self.err("expected a concept"));
        }
        if !scg::is_sctid(id) {
            self.pos = start;
            return Err(self.err(&format!("'{id}' is not a valid SNOMED CT identifier")));
        }
        let save = self.pos;
        self.ws();
        if self.eat("|") {
            match self.text[self.pos..].find('|') {
                Some(e) => self.pos += e + 1,
                None => return Err(self.err("unterminated term")),
            }
        } else {
            self.pos = save;
        }
        Ok(id.to_string())
    }

    fn filter_constraint(&mut self) -> Result<Vec<Filter>, EclError> {
        self.ws();
        let mut concept = false;
        if self.keyword("C") || self.keyword("c") {
            concept = true;
        } else if self.keyword("D") || self.keyword("d") {
        }
        let mut out = Vec::new();
        loop {
            self.ws();
            let start = self.pos;
            while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                self.pos += 1;
            }
            let name = self.text[start..self.pos].to_ascii_lowercase();
            self.ws();
            if !self.eat("=") {
                return Err(self.err("expected '=' in the filter"));
            }
            self.ws();
            match (concept, name.as_str()) {
                (true, "active") => out.push(Filter::Active(self.boolean()?)),
                (true, "definitionstatus") => {
                    let v = self.word();
                    out.push(Filter::Primitive(v.eq_ignore_ascii_case("primitive")));
                }
                (true, "definitionstatusid") => {
                    let v = self.concept_reference()?;
                    out.push(Filter::Primitive(v == super::model::PRIMITIVE));
                }
                (false, "term") => {
                    self.eat("match:");
                    let t = self.quoted()?;
                    out.push(Filter::Term(
                        t.split_whitespace().map(|w| w.to_lowercase()).collect(),
                    ));
                }
                (true, n) | (false, n) => {
                    return Err(EclError(format!("the ECL filter '{n}' is not supported")));
                }
            }
            self.ws();
            if self.eat("}}") {
                return Ok(out);
            }
            if !self.eat(",") {
                return Err(self.err("expected '}}'"));
            }
        }
    }

    fn word(&mut self) -> String {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_alphanumeric()) {
            self.pos += 1;
        }
        self.text[start..self.pos].to_string()
    }

    fn boolean(&mut self) -> Result<bool, EclError> {
        match self.word().to_ascii_lowercase().as_str() {
            "true" | "1" => Ok(true),
            "false" | "0" => Ok(false),
            _ => Err(self.err("expected true or false")),
        }
    }

    fn quoted(&mut self) -> Result<String, EclError> {
        if !self.eat("\"") {
            return Err(self.err("expected a quoted string"));
        }
        let mut out = String::new();
        let mut it = self.text[self.pos..].char_indices();
        while let Some((i, c)) = it.next() {
            match c {
                '"' => {
                    self.pos += i + 1;
                    return Ok(out);
                }
                '\\' => {
                    if let Some((_, n)) = it.next() {
                        out.push(n);
                    }
                }
                c => out.push(c),
            }
        }
        Err(self.err("unterminated string"))
    }

    fn refinement(&mut self) -> Result<Refinement, EclError> {
        let first = self.sub_refinement()?;
        self.ws();
        let save = self.pos;
        if self.conjunction() {
            let mut items = vec![first];
            loop {
                self.ws();
                items.push(self.sub_refinement()?);
                self.ws();
                let s = self.pos;
                if !self.conjunction() {
                    self.pos = s;
                    break;
                }
            }
            return Ok(Refinement::And(items));
        }
        self.pos = save;
        if self.keyword("OR") {
            let mut items = vec![first];
            loop {
                self.ws();
                items.push(self.sub_refinement()?);
                self.ws();
                if !self.keyword("OR") {
                    break;
                }
            }
            return Ok(Refinement::Or(items));
        }
        Ok(first)
    }

    fn sub_refinement(&mut self) -> Result<Refinement, EclError> {
        self.ws();
        let card = if self.peek() == Some(b'[') {
            Some(self.cardinality()?)
        } else {
            None
        };
        self.ws();
        if self.peek() == Some(b'{') && !self.starts("{{") {
            self.pos += 1;
            let inner = self.refinement()?;
            self.ws();
            if !self.eat("}") {
                return Err(self.err("expected '}'"));
            }
            return Ok(Refinement::Group(card, Box::new(inner)));
        }
        if card.is_none() && self.peek() == Some(b'(') {
            // A bracketed refinement — or an attribute whose name is a
            // bracketed constraint; try the refinement first.
            let save = self.pos;
            self.pos += 1;
            if let Ok(inner) = self.refinement() {
                self.ws();
                if self.eat(")") {
                    return Ok(inner);
                }
            }
            self.pos = save;
        }
        self.attribute(card).map(Refinement::Attribute)
    }

    fn cardinality(&mut self) -> Result<Cardinality, EclError> {
        self.pos += 1;
        self.ws();
        let num = |p: &mut Self| -> Option<u32> {
            let s = p.pos;
            while p.peek().is_some_and(|c| c.is_ascii_digit()) {
                p.pos += 1;
            }
            p.text[s..p.pos].parse().ok()
        };
        let min = num(self).ok_or_else(|| self.err("expected a minimum cardinality"))?;
        self.ws();
        if !self.eat("..") {
            return Err(self.err("expected '..'"));
        }
        self.ws();
        let max = if self.eat("*") {
            None
        } else {
            Some(num(self).ok_or_else(|| self.err("expected a maximum cardinality"))?)
        };
        self.ws();
        if !self.eat("]") {
            return Err(self.err("expected ']'"));
        }
        if max.is_some_and(|m| m < min) {
            return Err(self.err("the maximum cardinality is less than the minimum"));
        }
        Ok(Cardinality { min, max })
    }

    fn attribute(&mut self, card: Option<Cardinality>) -> Result<AttributeConstraint, EclError> {
        self.ws();
        let reverse = self.peek() == Some(b'R')
            && self.s.get(self.pos + 1).is_some_and(|c| c.is_ascii_whitespace() || c.is_ascii_digit() || *c == b'<' || *c == b'(' || *c == b'*');
        if reverse {
            self.pos += 1;
            self.ws();
        }
        let name = self.sub_expression_constraint()?;
        self.ws();
        let comparison = if self.eat("!=") {
            self.value_after(true, NumOp::Ne)?
        } else if self.eat("=") {
            self.value_after(false, NumOp::Eq)?
        } else {
            let op = if self.eat("<=") {
                NumOp::Le
            } else if self.eat(">=") {
                NumOp::Ge
            } else if self.eat("<") {
                NumOp::Lt
            } else if self.eat(">") {
                NumOp::Gt
            } else {
                return Err(self.err("expected a comparison operator"));
            };
            self.ws();
            if !self.eat("#") {
                return Err(self.err("expected a number"));
            }
            Comparison::Number { op, value: self.number()? }
        };
        if reverse && !matches!(comparison, Comparison::Expression { .. }) {
            return Err(self.err("a reverse attribute needs a concept value"));
        }
        Ok(AttributeConstraint { cardinality: card, reverse, name: Box::new(name), comparison })
    }

    fn value_after(&mut self, negated: bool, op: NumOp) -> Result<Comparison, EclError> {
        self.ws();
        if self.eat("#") {
            return Ok(Comparison::Number { op, value: self.number()? });
        }
        if self.peek() == Some(b'"') {
            return Ok(Comparison::String { negated, value: self.quoted()? });
        }
        for (k, v) in [("true", true), ("false", false)] {
            if self.keyword(k) || (self.starts(k) && self.s.get(self.pos + k.len()).is_none()) {
                if self.starts(k) {
                    self.pos += k.len();
                }
                return Ok(Comparison::Boolean { negated, value: v });
            }
        }
        let value = self.sub_expression_constraint()?;
        Ok(Comparison::Expression { negated, value: Box::new(value) })
    }

    fn number(&mut self) -> Result<f64, EclError> {
        let s = self.pos;
        if matches!(self.peek(), Some(b'-' | b'+')) {
            self.pos += 1;
        }
        while self.peek().is_some_and(|c| c.is_ascii_digit() || c == b'.') {
            self.pos += 1;
        }
        self.text[s..self.pos]
            .parse()
            .map_err(|_| self.err("expected a number"))
    }
}

// ── Analysis ────────────────────────────────────────────────────────────────

impl Ecl {
    /// Every concept id named, for existence checks.
    pub fn concept_ids(&self) -> Vec<String> {
        fn sub(e: &Ecl, out: &mut Vec<String>) {
            match e {
                Ecl::Sub { focus, .. } => focus_ids(focus, out),
                Ecl::Refined(b, r) => {
                    sub(b, out);
                    refinement(r, out);
                }
                Ecl::Dotted(b, attrs) => {
                    sub(b, out);
                    attrs.iter().for_each(|a| sub(a, out));
                }
                Ecl::And(v) | Ecl::Or(v) => v.iter().for_each(|x| sub(x, out)),
                Ecl::Minus(a, b) => {
                    sub(a, out);
                    sub(b, out);
                }
            }
        }
        fn focus_ids(f: &Focus, out: &mut Vec<String>) {
            match f {
                Focus::Concept(c) => out.push(c.clone()),
                Focus::Wildcard => {}
                // A reference set named directly need not be a concept of
                // the edition (an extension's refset); it just has members.
                Focus::MemberOf(x) => {
                    if !matches!(x.as_ref(), Ecl::Sub { op: None, focus: Focus::Concept(_), .. }) {
                        sub(x, out)
                    }
                }
                Focus::Nested(x) => sub(x, out),
            }
        }
        fn refinement(r: &Refinement, out: &mut Vec<String>) {
            match r {
                Refinement::Attribute(a) => {
                    sub(&a.name, out);
                    if let Comparison::Expression { value, .. } = &a.comparison {
                        sub(value, out);
                    }
                }
                Refinement::Group(_, r) => refinement(r, out),
                Refinement::And(v) | Refinement::Or(v) => v.iter().for_each(|x| refinement(x, out)),
            }
        }
        let mut out = Vec::new();
        sub(self, &mut out);
        out
    }

    /// The constraint in the subset the SQL evaluators handle over the
    /// closure table (`crate::ecl::EclExpr`), if it is in it.
    pub fn to_simple(&self) -> Option<crate::ecl::EclExpr> {
        use crate::ecl::{ConceptOperator as O, EclExpr as E, FocusConcept as F};
        let fold = |items: &[Ecl], f: fn(Box<E>, Box<E>) -> E| -> Option<E> {
            let mut it = items.iter();
            let mut acc = it.next()?.to_simple()?;
            for x in it {
                acc = f(Box::new(acc), Box::new(x.to_simple()?));
            }
            Some(acc)
        };
        match self {
            Ecl::Sub { op, focus, filters } if filters.is_empty() => {
                let op = match op {
                    None => None,
                    Some(Op::DescendantOf) => Some(O::DescendantOf),
                    Some(Op::DescendantOrSelfOf) => Some(O::DescendantOrSelf),
                    Some(Op::AncestorOf) => Some(O::AncestorOf),
                    Some(Op::AncestorOrSelfOf) => Some(O::AncestorOrSelf),
                    Some(_) => return None,
                };
                match focus {
                    Focus::Concept(c) => Some(E::Focus { op, concept: F::Id(c.clone()) }),
                    Focus::Wildcard => Some(E::Focus { op, concept: F::Wildcard }),
                    Focus::Nested(inner) if op.is_none() => inner.to_simple(),
                    Focus::MemberOf(inner) if op.is_none() => match inner.as_ref() {
                        Ecl::Sub { op: None, focus: Focus::Concept(c), filters } if filters.is_empty() => {
                            Some(E::MemberOf(Box::new(E::Focus { op: None, concept: F::Id(c.clone()) })))
                        }
                        Ecl::Sub { op: None, focus: Focus::Wildcard, filters } if filters.is_empty() => {
                            Some(E::MemberOf(Box::new(E::Focus { op: None, concept: F::Wildcard })))
                        }
                        _ => None,
                    },
                    _ => None,
                }
            }
            Ecl::And(v) => fold(v, E::And),
            Ecl::Or(v) => fold(v, E::Or),
            Ecl::Minus(a, b) => Some(E::Minus(Box::new(a.to_simple()?), Box::new(b.to_simple()?))),
            _ => None,
        }
    }
}

// ── Evaluation ──────────────────────────────────────────────────────────────

/// A post-coordinated expression as the evaluator sees it: its focus
/// concepts and its relationships (theirs plus its refinement).
#[derive(Debug, Clone, Default)]
pub struct Virtual {
    pub foci: Vec<u32>,
    /// Relationship groups; ungrouped attributes are one-member groups.
    pub groups: Vec<Vec<(u32, VValue)>>,
}

#[derive(Debug, Clone)]
pub enum VValue {
    Concept(u32),
    Expression(Box<Virtual>),
    Concrete(Concrete),
}

impl Virtual {
    /// Build from a parsed sub-expression; unknown concepts are `Err(id)`.
    pub fn from_sub(m: &SnomedModel, e: &scg::SubExpression) -> Result<Virtual, String> {
        let idx = |id: &str| m.idx(id).ok_or_else(|| id.to_string());
        let mut v = Virtual::default();
        for f in &e.focus {
            v.foci.push(idx(&f.id)?);
        }
        for &f in &v.foci {
            v.groups.extend(concept_groups(m, f));
        }
        let value = |a: &scg::Attribute| -> Result<(u32, VValue), String> {
            let t = idx(&a.name.id)?;
            let val = match &a.value {
                AttributeValue::Concept(c) => VValue::Concept(idx(&c.id)?),
                AttributeValue::Expression(x) => VValue::Expression(Box::new(Virtual::from_sub(m, x)?)),
                AttributeValue::Concrete(c) => VValue::Concrete(c.clone()),
            };
            Ok((t, val))
        };
        for a in &e.ungrouped {
            v.groups.push(vec![value(a)?]);
        }
        for g in &e.groups {
            v.groups.push(g.iter().map(value).collect::<Result<_, _>>()?);
        }
        Ok(v)
    }
}

/// The relationship groups of a concept; ungrouped ones each on their own.
pub fn concept_groups(m: &SnomedModel, c: u32) -> Vec<Vec<(u32, VValue)>> {
    let mut by_group: Vec<(u16, Vec<(u32, VValue)>)> = Vec::new();
    let mut out = Vec::new();
    for r in &m.rels[c as usize] {
        let v = match &r.value {
            RelValue::Concept(x) => VValue::Concept(*x),
            RelValue::Concrete(x) => VValue::Concrete(x.clone()),
        };
        if r.group == 0 {
            out.push(vec![(r.type_id, v)]);
        } else if let Some(g) = by_group.iter_mut().find(|(g, _)| *g == r.group) {
            g.1.push((r.type_id, v));
        } else {
            by_group.push((r.group, vec![(r.type_id, v)]));
        }
    }
    out.extend(by_group.into_iter().map(|(_, g)| g));
    out
}

/// What a refinement is tested against.
enum Subject<'a> {
    Concept(u32),
    Virtual(&'a Virtual),
}

/// Evaluates constraints over one model, memoising sub-results.
pub struct Evaluator<'m> {
    m: &'m SnomedModel,
    memo: HashMap<*const Ecl, std::rc::Rc<HashSet<u32>>>,
    reverse: Option<HashMap<u32, Vec<(u32, u32)>>>,
}

/// An unknown concept named in a constraint.
#[derive(Debug, Clone, PartialEq)]
pub struct UnknownConcept(pub String);

impl<'m> Evaluator<'m> {
    pub fn new(m: &'m SnomedModel) -> Self {
        Evaluator { m, memo: HashMap::new(), reverse: None }
    }

    /// The concepts `e` selects.
    pub fn eval(&mut self, e: &Ecl) -> Result<std::rc::Rc<HashSet<u32>>, UnknownConcept> {
        // Sub-results are memoised by node address, which only identifies a
        // node while the expression it belongs to is alive.
        self.memo.clear();
        self.eval_rc(e)
    }

    fn eval_rc(&mut self, e: &Ecl) -> Result<std::rc::Rc<HashSet<u32>>, UnknownConcept> {
        let key = e as *const Ecl;
        if let Some(s) = self.memo.get(&key) {
            return Ok(s.clone());
        }
        let set = self.eval_inner(e)?;
        let rc = std::rc::Rc::new(set);
        self.memo.insert(key, rc.clone());
        Ok(rc)
    }

    fn all(&self) -> HashSet<u32> {
        (0..self.m.len() as u32).collect()
    }

    fn eval_inner(&mut self, e: &Ecl) -> Result<HashSet<u32>, UnknownConcept> {
        let m = self.m;
        Ok(match e {
            Ecl::Sub { op, focus, filters } => {
                let base: HashSet<u32> = match focus {
                    Focus::Concept(c) => {
                        let i = m.idx(c).ok_or_else(|| UnknownConcept(c.clone()))?;
                        [i].into_iter().collect()
                    }
                    Focus::Wildcard => self.all(),
                    Focus::Nested(x) => (*self.eval_rc(x)?).clone(),
                    Focus::MemberOf(x) => {
                        let refsets = match x.as_ref() {
                            Ecl::Sub { op: None, focus: Focus::Concept(c), filters } if filters.is_empty() => {
                                std::rc::Rc::new(m.idx(c).into_iter().collect())
                            }
                            _ => self.eval_rc(x)?,
                        };
                        let mut out = HashSet::new();
                        for r in refsets.iter() {
                            if let Some(members) = m.refsets.get(r) {
                                out.extend(members.iter().copied());
                            }
                        }
                        out
                    }
                };
                let mut set = match op {
                    None => base,
                    Some(op) => self.apply_op(*op, &base, matches!(focus, Focus::Wildcard)),
                };
                if !filters.is_empty() {
                    set.retain(|&c| filters.iter().all(|f| self.filter(c, f)));
                }
                set
            }
            Ecl::Refined(base, r) => {
                let base = self.eval_rc(base)?;
                let mut out = HashSet::new();
                for &c in base.iter() {
                    if self.refinement(&Subject::Concept(c), r)? {
                        out.insert(c);
                    }
                }
                out
            }
            Ecl::Dotted(base, attrs) => {
                let mut cur = (*self.eval_rc(base)?).clone();
                for a in attrs {
                    let names = self.eval_rc(a)?;
                    let mut next = HashSet::new();
                    for &c in &cur {
                        for r in &m.rels[c as usize] {
                            if let RelValue::Concept(v) = r.value {
                                if names.contains(&r.type_id) {
                                    next.insert(v);
                                }
                            }
                        }
                    }
                    cur = next;
                }
                cur
            }
            Ecl::And(items) => {
                let mut acc: Option<HashSet<u32>> = None;
                for x in items {
                    let s = self.eval_rc(x)?;
                    acc = Some(match acc {
                        None => (*s).clone(),
                        Some(a) => a.into_iter().filter(|c| s.contains(c)).collect(),
                    });
                }
                acc.unwrap_or_default()
            }
            Ecl::Or(items) => {
                let mut acc = HashSet::new();
                for x in items {
                    acc.extend(self.eval_rc(x)?.iter().copied());
                }
                acc
            }
            Ecl::Minus(a, b) => {
                let a = self.eval_rc(a)?;
                let b = self.eval_rc(b)?;
                a.iter().filter(|c| !b.contains(c)).copied().collect()
            }
        })
    }

    fn apply_op(&self, op: Op, base: &HashSet<u32>, wildcard: bool) -> HashSet<u32> {
        let m = self.m;
        let mut out = HashSet::new();
        match op {
            Op::DescendantOf | Op::DescendantOrSelfOf if wildcard => {
                out = base.clone();
                if op == Op::DescendantOf {
                    out.retain(|&c| !m.parents[c as usize].is_empty());
                }
            }
            Op::AncestorOf | Op::AncestorOrSelfOf if wildcard => {
                out = base.clone();
                if op == Op::AncestorOf {
                    out.retain(|&c| !m.children[c as usize].is_empty());
                }
            }
            Op::DescendantOf | Op::DescendantOrSelfOf => {
                for &c in base {
                    out.extend(m.descendants(c));
                    if op == Op::DescendantOrSelfOf {
                        out.insert(c);
                    }
                }
            }
            Op::AncestorOf | Op::AncestorOrSelfOf => {
                for &c in base {
                    out.extend(m.ancestors(c));
                    if op == Op::AncestorOrSelfOf {
                        out.insert(c);
                    }
                }
            }
            Op::ChildOf | Op::ChildOrSelfOf => {
                for &c in base {
                    out.extend(m.children[c as usize].iter().copied());
                    if op == Op::ChildOrSelfOf {
                        out.insert(c);
                    }
                }
            }
            Op::ParentOf | Op::ParentOrSelfOf => {
                for &c in base {
                    out.extend(m.parents[c as usize].iter().copied());
                    if op == Op::ParentOrSelfOf {
                        out.insert(c);
                    }
                }
            }
            Op::Top => {
                for &c in base {
                    if !m.ancestors(c).iter().any(|a| base.contains(a)) {
                        out.insert(c);
                    }
                }
            }
            Op::Bottom => {
                for &c in base {
                    if !m.descendants(c).iter().any(|d| base.contains(d)) {
                        out.insert(c);
                    }
                }
            }
        }
        out
    }

    fn filter(&self, c: u32, f: &Filter) -> bool {
        let m = self.m;
        match f {
            Filter::Active(a) => m.active[c as usize] == *a,
            Filter::Primitive(p) => m.primitive[c as usize] == *p,
            Filter::Term(words) => [m.display[c as usize].as_deref(), m.first_term[c as usize].as_deref()]
                .into_iter()
                .flatten()
                .any(|t| {
                    let tw: Vec<String> = t
                        .split(|ch: char| !ch.is_alphanumeric())
                        .filter(|w| !w.is_empty())
                        .map(str::to_lowercase)
                        .collect();
                    words.iter().all(|w| tw.iter().any(|x| x.starts_with(w.as_str())))
                }),
        }
    }

    /// Whether the expression `v` satisfies `e`.
    pub fn expression_matches(&mut self, v: &Virtual, e: &Ecl) -> Result<bool, UnknownConcept> {
        self.memo.clear();
        self.matches(v, e)
    }

    fn matches(&mut self, v: &Virtual, e: &Ecl) -> Result<bool, UnknownConcept> {
        Ok(match e {
            Ecl::Sub { op, focus, filters } => {
                if !filters.is_empty() {
                    return Ok(false);
                }
                match (op, focus) {
                    (None, Focus::Wildcard) => true,
                    (None, Focus::Nested(x)) => self.matches(v, x)?,
                    (Some(Op::DescendantOf | Op::DescendantOrSelfOf), _) => {
                        let targets = match focus {
                            Focus::Wildcard => return Ok(true),
                            Focus::Concept(c) => {
                                let i = self.m.idx(c).ok_or_else(|| UnknownConcept(c.clone()))?;
                                std::rc::Rc::new([i].into_iter().collect())
                            }
                            Focus::Nested(x) => self.eval_rc(x)?,
                            Focus::MemberOf(_) => self.eval_rc(&Ecl::Sub {
                                op: None,
                                focus: focus.clone(),
                                filters: vec![],
                            })?,
                        };
                        v.foci.iter().any(|&f| {
                            targets.contains(&f) || self.m.ancestors(f).iter().any(|a| targets.contains(a))
                        })
                    }
                    _ => false,
                }
            }
            Ecl::Refined(base, r) => {
                self.matches(v, base)? && self.refinement(&Subject::Virtual(v), r)?
            }
            Ecl::Dotted(..) => false,
            Ecl::And(items) => {
                for x in items {
                    if !self.matches(v, x)? {
                        return Ok(false);
                    }
                }
                true
            }
            Ecl::Or(items) => {
                for x in items {
                    if self.matches(v, x)? {
                        return Ok(true);
                    }
                }
                false
            }
            Ecl::Minus(a, b) => self.matches(v, a)? && !self.matches(v, b)?,
        })
    }

    fn groups_of(&self, s: &Subject) -> Vec<Vec<(u32, VValue)>> {
        match s {
            Subject::Concept(c) => concept_groups(self.m, *c),
            Subject::Virtual(v) => v.groups.clone(),
        }
    }

    fn refinement(&mut self, s: &Subject, r: &Refinement) -> Result<bool, UnknownConcept> {
        let groups = self.groups_of(s);
        let all: Vec<&(u32, VValue)> = groups.iter().flatten().collect();
        self.refinement_in(s, r, &all, &groups)
    }

    fn refinement_in(
        &mut self,
        s: &Subject,
        r: &Refinement,
        scope: &[&(u32, VValue)],
        groups: &[Vec<(u32, VValue)>],
    ) -> Result<bool, UnknownConcept> {
        Ok(match r {
            Refinement::Attribute(a) => self.attribute(s, a, scope)?,
            Refinement::Group(card, inner) => {
                let mut n = 0;
                for g in groups {
                    let scope: Vec<&(u32, VValue)> = g.iter().collect();
                    if self.refinement_in(s, inner, &scope, groups)? {
                        n += 1;
                    }
                }
                card.unwrap_or(Cardinality { min: 1, max: None }).allows(n)
            }
            Refinement::And(items) => {
                for x in items {
                    if !self.refinement_in(s, x, scope, groups)? {
                        return Ok(false);
                    }
                }
                true
            }
            Refinement::Or(items) => {
                for x in items {
                    if self.refinement_in(s, x, scope, groups)? {
                        return Ok(true);
                    }
                }
                false
            }
        })
    }

    fn attribute(
        &mut self,
        s: &Subject,
        a: &AttributeConstraint,
        scope: &[&(u32, VValue)],
    ) -> Result<bool, UnknownConcept> {
        let card = a.cardinality.unwrap_or(Cardinality { min: 1, max: None });
        let names = self.eval_rc(&a.name)?;
        if a.reverse {
            let Subject::Concept(c) = s else { return Ok(card.allows(0)) };
            let Comparison::Expression { negated, value } = &a.comparison else {
                return Ok(false);
            };
            let sources = self.eval_rc(value)?;
            let reverse = self.reverse_index();
            let n = reverse
                .get(c)
                .map(|v| {
                    v.iter()
                        .filter(|(src, ty)| names.contains(ty) && (sources.contains(src) != *negated))
                        .count()
                })
                .unwrap_or(0);
            return Ok(card.allows(n as u32));
        }
        let mut n = 0u32;
        for (ty, val) in scope {
            if !names.contains(ty) {
                continue;
            }
            if self.value_matches(val, &a.comparison)? {
                n += 1;
            }
        }
        Ok(card.allows(n))
    }

    fn value_matches(&mut self, val: &VValue, cmp: &Comparison) -> Result<bool, UnknownConcept> {
        Ok(match (cmp, val) {
            (Comparison::Expression { negated, value }, VValue::Concept(c)) => {
                self.eval_rc(value)?.contains(c) != *negated
            }
            (Comparison::Expression { negated, value }, VValue::Expression(x)) => {
                self.matches(x, value)? != *negated
            }
            (Comparison::Number { op, value }, VValue::Concrete(c)) => match c.number() {
                Some(x) => match op {
                    NumOp::Eq => x == *value,
                    NumOp::Ne => x != *value,
                    NumOp::Lt => x < *value,
                    NumOp::Le => x <= *value,
                    NumOp::Gt => x > *value,
                    NumOp::Ge => x >= *value,
                },
                None => false,
            },
            (Comparison::String { negated, value }, VValue::Concrete(Concrete::String(s))) => {
                (s == value) != *negated
            }
            (Comparison::Boolean { negated, value }, VValue::Concrete(Concrete::Boolean(b))) => {
                (b == value) != *negated
            }
            _ => false,
        })
    }

    fn reverse_index(&mut self) -> &HashMap<u32, Vec<(u32, u32)>> {
        let m = self.m;
        self.reverse.get_or_insert_with(|| {
            let mut idx: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
            for (src, rels) in m.rels.iter().enumerate() {
                for r in rels {
                    if let RelValue::Concept(v) = r.value {
                        idx.entry(v).or_default().push((src as u32, r.type_id));
                    }
                }
            }
            idx
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_operators_and_refinements() {
        for t in [
            "<<! 10200004",
            ">>! 10200004",
            "< 64572001 : 363698007 = << 10200004",
            "< 64572001 : { 363698007 = << 10200004, 116676008 = 20946005 }",
            "< 64572001 : [1..1] { [1..1] 363698007 = << 10200004 }",
            "< 64572001 . 363698007",
            "<< 10200004 |Liver structure (body structure)|",
            "((((<< 10200004))))",
            "^ 900000000000526001",
            "< 373873005 : 1142135004 >= #5",
            "< 404684003 : R 363698007 = *",
            "<< 64572001 {{ C definitionStatus = primitive }}",
        ] {
            parse(t).unwrap_or_else(|e| panic!("{t}: {e}"));
        }
    }

    #[test]
    fn rejects_mixed_operators_and_bad_syntax() {
        for t in ["<< 128045006 OR << 10200004 AND << 64572001", "<<", "<< 10200004 <<", "<< abc", "<< 1234567 MINUS << 2345678 MINUS << 3456789"] {
            assert!(parse(t).is_err(), "{t}");
        }
    }

    #[test]
    fn simple_constraints_go_to_sql() {
        assert!(parse("<< 1234567 OR < 2345678").unwrap().to_simple().is_some());
        assert!(parse("<! 1234567").unwrap().to_simple().is_none());
        assert!(parse("< 1234567 : 2345678 = *").unwrap().to_simple().is_none());
    }
}
