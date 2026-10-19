//! SNOMED CT Compositional Grammar (SCG) — post-coordinated expressions.
//!
//! Parses the grammar of the SNOMED CT Compositional Grammar specification
//! (<https://confluence.ihtsdotools.org/display/DOCSCG>) into an [`Expression`]
//! and renders it back, with or without terms. Concept ids are checked for
//! shape only; whether they exist and how they may be combined is the
//! concept model's business (`super::mrcm`).

use std::fmt::Write as _;

/// `===` (equivalent to) or `<<<` (subtype of) before an expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefinitionStatus {
    Equivalent,
    SubtypeOf,
}

/// A concept id with the term written after it, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConceptRef {
    pub id: String,
    pub term: Option<String>,
}

/// A concrete attribute value: `#12`, `#1.5`, `"text"`, `true`.
#[derive(Debug, Clone, PartialEq)]
pub enum Concrete {
    Integer(i64),
    /// The decimal as written, and its value.
    Decimal(String, f64),
    String(String),
    Boolean(bool),
}

impl Concrete {
    /// The value as SCG writes it (`#5`, `#2.5`, `"x"`, `true`).
    pub fn render(&self) -> String {
        match self {
            Concrete::Integer(i) => format!("#{i}"),
            Concrete::Decimal(s, _) => format!("#{s}"),
            Concrete::String(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
            Concrete::Boolean(b) => b.to_string(),
        }
    }

    /// The numeric value of a number.
    pub fn number(&self) -> Option<f64> {
        match self {
            Concrete::Integer(i) => Some(*i as f64),
            Concrete::Decimal(_, d) => Some(*d),
            _ => None,
        }
    }
}

/// The value of an attribute.
#[derive(Debug, Clone, PartialEq)]
pub enum AttributeValue {
    Concept(ConceptRef),
    /// A nested, parenthesised expression.
    Expression(Box<SubExpression>),
    Concrete(Concrete),
}

/// `name = value`.
#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    pub name: ConceptRef,
    pub value: AttributeValue,
}

/// Focus concepts and their refinement.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SubExpression {
    pub focus: Vec<ConceptRef>,
    /// Attributes outside any group.
    pub ungrouped: Vec<Attribute>,
    /// Attribute groups (`{…}`).
    pub groups: Vec<Vec<Attribute>>,
}

impl SubExpression {
    pub fn is_refined(&self) -> bool {
        !self.ungrouped.is_empty() || !self.groups.is_empty()
    }
}

/// A whole expression.
#[derive(Debug, Clone, PartialEq)]
pub struct Expression {
    pub status: Option<DefinitionStatus>,
    pub body: SubExpression,
}

impl Expression {
    /// A single concept with no refinement.
    pub fn as_concept(&self) -> Option<&str> {
        (self.body.focus.len() == 1 && !self.body.is_refined())
            .then(|| self.body.focus[0].id.as_str())
    }

    /// Every concept id the expression mentions, in order.
    pub fn concept_ids(&self) -> Vec<&str> {
        fn walk<'a>(e: &'a SubExpression, out: &mut Vec<&'a str>) {
            for f in &e.focus {
                out.push(&f.id);
            }
            for a in e.ungrouped.iter().chain(e.groups.iter().flatten()) {
                out.push(&a.name.id);
                match &a.value {
                    AttributeValue::Concept(c) => out.push(&c.id),
                    AttributeValue::Expression(x) => walk(x, out),
                    AttributeValue::Concrete(_) => {}
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.body, &mut out);
        out
    }
}

/// Whether `code` is written in the compositional grammar rather than being
/// a bare concept id.
pub fn is_expression(code: &str) -> bool {
    !code.is_empty() && !code.bytes().all(|b| b.is_ascii_digit())
}

/// Whether `s` has the shape of an SCTID (6–18 digits, no leading zero).
pub fn is_sctid(s: &str) -> bool {
    (6..=18).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit()) && !s.starts_with('0')
}

/// A syntax error, with the byte offset it was found at.
#[derive(Debug, Clone, PartialEq)]
pub struct SyntaxError {
    pub offset: usize,
    pub message: String,
}

impl std::fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at character {}", self.message, self.offset + 1)
    }
}

/// Parse an expression.
pub fn parse(text: &str) -> Result<Expression, SyntaxError> {
    let mut p = Parser { s: text.as_bytes(), text, pos: 0 };
    p.ws();
    let status = if p.eat_str("===") {
        Some(DefinitionStatus::Equivalent)
    } else if p.eat_str("<<<") {
        Some(DefinitionStatus::SubtypeOf)
    } else {
        None
    };
    p.ws();
    let body = p.sub_expression()?;
    p.ws();
    if p.pos < p.s.len() {
        return Err(p.err("unexpected text after the expression"));
    }
    Ok(Expression { status, body })
}

struct Parser<'a> {
    s: &'a [u8],
    text: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn err(&self, message: &str) -> SyntaxError {
        SyntaxError { offset: self.pos, message: message.to_string() }
    }

    fn ws(&mut self) {
        while self.pos < self.s.len() && self.s[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_str(&mut self, t: &str) -> bool {
        if self.s[self.pos..].starts_with(t.as_bytes()) {
            self.pos += t.len();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), SyntaxError> {
        self.ws();
        if self.eat(c) {
            Ok(())
        } else {
            Err(self.err(&format!("expected '{}'", c as char)))
        }
    }

    fn sub_expression(&mut self) -> Result<SubExpression, SyntaxError> {
        let mut e = SubExpression { focus: vec![self.concept_ref()?], ..Default::default() };
        loop {
            self.ws();
            if !self.eat(b'+') {
                break;
            }
            self.ws();
            e.focus.push(self.concept_ref()?);
        }
        self.ws();
        if self.eat(b':') {
            self.ws();
            self.refinement(&mut e)?;
        }
        Ok(e)
    }

    /// `(attributeSet / attributeGroup) *(["," ws] attributeGroup)`.
    fn refinement(&mut self, e: &mut SubExpression) -> Result<(), SyntaxError> {
        if self.peek() == Some(b'{') {
            e.groups.push(self.group()?);
        } else {
            e.ungrouped = self.attribute_set()?;
        }
        loop {
            self.ws();
            let save = self.pos;
            let comma = self.eat(b',');
            self.ws();
            if self.peek() == Some(b'{') {
                e.groups.push(self.group()?);
            } else {
                if comma {
                    return Err(self.err("expected an attribute group"));
                }
                self.pos = save;
                return Ok(());
            }
        }
    }

    fn group(&mut self) -> Result<Vec<Attribute>, SyntaxError> {
        self.expect(b'{')?;
        self.ws();
        let set = self.attribute_set()?;
        self.expect(b'}')?;
        Ok(set)
    }

    fn attribute_set(&mut self) -> Result<Vec<Attribute>, SyntaxError> {
        let mut set = vec![self.attribute()?];
        loop {
            self.ws();
            let save = self.pos;
            if !self.eat(b',') {
                return Ok(set);
            }
            self.ws();
            if self.peek() == Some(b'{') {
                // The comma separates the set from a following group.
                self.pos = save;
                return Ok(set);
            }
            set.push(self.attribute()?);
        }
    }

    fn attribute(&mut self) -> Result<Attribute, SyntaxError> {
        let name = self.concept_ref()?;
        self.expect(b'=')?;
        self.ws();
        let value = match self.peek() {
            Some(b'(') => {
                self.pos += 1;
                self.ws();
                let e = self.sub_expression()?;
                self.expect(b')')?;
                AttributeValue::Expression(Box::new(e))
            }
            Some(b'#') => {
                self.pos += 1;
                AttributeValue::Concrete(self.number()?)
            }
            Some(b'"') => AttributeValue::Concrete(Concrete::String(self.string()?)),
            Some(b't') if self.eat_str("true") => AttributeValue::Concrete(Concrete::Boolean(true)),
            Some(b'f') if self.eat_str("false") => {
                AttributeValue::Concrete(Concrete::Boolean(false))
            }
            _ => AttributeValue::Concept(self.concept_ref()?),
        };
        Ok(Attribute { name, value })
    }

    fn number(&mut self) -> Result<Concrete, SyntaxError> {
        let start = self.pos;
        if matches!(self.peek(), Some(b'-' | b'+')) {
            self.pos += 1;
        }
        let digits = |p: &mut Self| {
            let s = p.pos;
            while p.peek().is_some_and(|c| c.is_ascii_digit()) {
                p.pos += 1;
            }
            p.pos > s
        };
        if !digits(self) {
            return Err(self.err("expected a number"));
        }
        let text = |p: &Self| p.text[start..p.pos].trim_start_matches('+').to_string();
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !digits(self) {
                return Err(self.err("expected the decimal digits"));
            }
            let t = text(self);
            let v = t.parse().map_err(|_| self.err("invalid decimal"))?;
            return Ok(Concrete::Decimal(t, v));
        }
        let t = text(self);
        t.parse()
            .map(Concrete::Integer)
            .map_err(|_| self.err("invalid integer"))
    }

    fn string(&mut self) -> Result<String, SyntaxError> {
        self.pos += 1;
        let mut out = String::new();
        let mut chars = self.text[self.pos..].char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '"' => {
                    self.pos += i + 1;
                    return Ok(out);
                }
                '\\' => {
                    if let Some((_, n)) = chars.next() {
                        out.push(n);
                    }
                }
                c => out.push(c),
            }
        }
        self.pos = self.s.len();
        Err(self.err("unterminated string"))
    }

    fn concept_ref(&mut self) -> Result<ConceptRef, SyntaxError> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        let id = &self.text[start..self.pos];
        if id.is_empty() {
            return Err(self.err("expected a concept id"));
        }
        if !is_sctid(id) {
            self.pos = start;
            return Err(self.err(&format!("'{id}' is not a valid SNOMED CT identifier")));
        }
        let id = id.to_string();
        let save = self.pos;
        self.ws();
        let term = if self.eat(b'|') {
            let rest = &self.text[self.pos..];
            let Some(end) = rest.find('|') else {
                return Err(self.err("unterminated term"));
            };
            let term = rest[..end].trim().to_string();
            self.pos += end + 1;
            Some(term)
        } else {
            self.pos = save;
            None
        };
        Ok(ConceptRef { id, term })
    }
}

/// Render `e` without terms, in the normal punctuation (`,` before every
/// group that follows something, no spaces).
pub fn render_plain(e: &Expression) -> String {
    let mut out = String::new();
    match e.status {
        Some(DefinitionStatus::Equivalent) => out.push_str("==="),
        Some(DefinitionStatus::SubtypeOf) => out.push_str("<<<"),
        None => {}
    }
    render_sub(&e.body, &mut |_: &ConceptRef| None, &mut out);
    out
}

/// Render `e` with a term after every concept: the one written, or what
/// `term_of` gives for the id.
pub fn render_with_terms(e: &Expression, term_of: &mut dyn FnMut(&str) -> Option<String>) -> String {
    let mut out = String::new();
    match e.status {
        Some(DefinitionStatus::Equivalent) => out.push_str("==="),
        Some(DefinitionStatus::SubtypeOf) => out.push_str("<<<"),
        None => {}
    }
    render_sub(
        &e.body,
        &mut |c: &ConceptRef| c.term.clone().or_else(|| term_of(&c.id)),
        &mut out,
    );
    out
}

/// [`render_with_terms`] of a sub-expression.
pub fn render_sub_with_terms(
    e: &SubExpression,
    term_of: &mut dyn FnMut(&str) -> Option<String>,
) -> String {
    let mut out = String::new();
    render_sub(e, &mut |c: &ConceptRef| c.term.clone().or_else(|| term_of(&c.id)), &mut out);
    out
}

/// [`render_plain`] of a sub-expression.
pub fn render_sub_plain(e: &SubExpression) -> String {
    let mut out = String::new();
    render_sub(e, &mut |_: &ConceptRef| None, &mut out);
    out
}

fn render_sub(
    e: &SubExpression,
    term: &mut dyn FnMut(&ConceptRef) -> Option<String>,
    out: &mut String,
) {
    let concept = |c: &ConceptRef, out: &mut String, term: &mut dyn FnMut(&ConceptRef) -> Option<String>| {
        out.push_str(&c.id);
        if let Some(t) = term(c) {
            let _ = write!(out, "|{t}|");
        }
    };
    for (i, f) in e.focus.iter().enumerate() {
        if i > 0 {
            out.push('+');
        }
        concept(f, out, term);
    }
    if !e.is_refined() {
        return;
    }
    out.push(':');
    let attrs = |set: &[Attribute], out: &mut String, term: &mut dyn FnMut(&ConceptRef) -> Option<String>| {
        for (i, a) in set.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            concept(&a.name, out, term);
            out.push('=');
            match &a.value {
                AttributeValue::Concept(c) => concept(c, out, term),
                AttributeValue::Expression(x) => {
                    out.push('(');
                    render_sub(x, term, out);
                    out.push(')');
                }
                AttributeValue::Concrete(c) => out.push_str(&c.render()),
            }
        }
    };
    attrs(&e.ungrouped, out, term);
    for (i, g) in e.groups.iter().enumerate() {
        if i > 0 || !e.ungrouped.is_empty() {
            out.push(',');
        }
        out.push('{');
        attrs(g, out, term);
        out.push('}');
    }
}

/// `code` with its terms and white space removed — what [`render_plain`]
/// gives for an expression already in the normal punctuation.
pub fn strip_terms(code: &str) -> String {
    let mut out = String::new();
    let mut in_term = false;
    let mut in_string = false;
    for c in code.chars() {
        match c {
            '"' if !in_term => {
                in_string = !in_string;
                out.push(c);
            }
            '|' if !in_string => in_term = !in_term,
            _ if in_term => {}
            c if c.is_whitespace() && !in_string => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_renders_nested_refinements() {
        let e = parse("367430006:{405813007=(85562004:272741003=24028007)}").unwrap();
        assert_eq!(e.body.groups.len(), 1);
        assert!(matches!(e.body.groups[0][0].value, AttributeValue::Expression(_)));
        assert_eq!(render_plain(&e), "367430006:{405813007=(85562004:272741003=24028007)}");
    }

    #[test]
    fn terms_and_spaces_are_kept_apart() {
        let e = parse("39607008 |Lung| : 272741003 |Laterality| = 24028007 |Right|").unwrap();
        assert_eq!(e.body.focus[0].term.as_deref(), Some("Lung"));
        assert_eq!(render_plain(&e), "39607008:272741003=24028007");
        let shown = render_with_terms(&e, &mut |_| Some("x".into()));
        assert_eq!(shown, "39607008|Lung|:272741003|Laterality|=24028007|Right|");
    }

    #[test]
    fn a_group_may_follow_the_attribute_set_without_a_comma() {
        let e = parse("373873005:1142139005=#2{1142135004=#500}").unwrap();
        assert_eq!(render_plain(&e), "373873005:1142139005=#2,{1142135004=#500}");
        assert_ne!(render_plain(&e), strip_terms("373873005:1142139005=#2{1142135004=#500}"));
    }

    #[test]
    fn a_refined_value_must_be_bracketed() {
        assert!(parse("367430006:{405813007=85562004:272741003=24028007}").is_err());
        assert!(parse("10200004 <<").is_err());
        assert!(parse("abc").is_err());
    }

    #[test]
    fn concrete_values() {
        let e = parse("373873005:{1142135004=#2.5,1142139005=#-3}").unwrap();
        let g = &e.body.groups[0];
        assert!(matches!(&g[0].value, AttributeValue::Concrete(Concrete::Decimal(s, _)) if s == "2.5"));
        assert!(matches!(g[1].value, AttributeValue::Concrete(Concrete::Integer(-3))));
    }

    #[test]
    fn conjunction_of_focus_concepts() {
        let e = parse("40468003+3738000").unwrap();
        assert_eq!(e.body.focus.len(), 2);
        assert_eq!(e.concept_ids(), vec!["40468003", "3738000"]);
    }
}
