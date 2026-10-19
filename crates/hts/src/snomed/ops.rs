//! Operation logic for post-coordinated SNOMED CT expressions: validation,
//! value set membership, `$lookup` and `$subsumes`, over the in-memory model.

use serde_json::{Value, json};

use super::ecl::{self, Evaluator, Virtual};
use super::model::SnomedModel;
use super::scg::{self, Expression};
use super::{SNOMED_URL, mrcm, nf};
use crate::error::{CodedIssue, HtsError};

/// The outcome of checking an expression.
pub enum Checked {
    Valid {
        expr: Expression,
        /// With a term after every concept (`id|term|`).
        display: String,
        /// The normal form, when `code` is not written in it.
        normal_form: Option<String>,
    },
    /// Why the expression is not valid ("Not a valid expression: …").
    Invalid(String),
}

/// Terms for rendering: the preferred term.
pub fn term_of(m: &SnomedModel) -> impl FnMut(&str) -> Option<String> + '_ {
    |id: &str| m.idx(id).and_then(|i| m.display_of(i)).map(str::to_string)
}

/// Parse `code` and check it against the concept model.
pub fn check(m: &SnomedModel, code: &str) -> Checked {
    let expr = match scg::parse(code) {
        Ok(e) => e,
        Err(e) => return Checked::Invalid(format!("Not a valid expression: {e}")),
    };
    if let Some(id) = expr.concept_ids().into_iter().find(|id| m.idx(id).is_none()) {
        return Checked::Invalid(format!("Not a valid expression: Concept {id} not found"));
    }
    if let Err(reason) = mrcm::check(m, &expr.body) {
        return Checked::Invalid(format!("Not a valid expression: {reason}"));
    }
    let plain = scg::render_plain(&expr);
    let normal_form = (scg::strip_terms(code) != plain).then_some(plain);
    let display = scg::render_with_terms(&expr, &mut term_of(m));
    Checked::Valid { expr, display, normal_form }
}

/// The model's view of a checked expression.
pub fn virtual_of(m: &SnomedModel, e: &Expression) -> Option<Virtual> {
    Virtual::from_sub(m, &e.body).ok()
}

/// An implicit SNOMED CT value set (`?fhir_vs`, `?fhir_vs=isa/X`,
/// `?fhir_vs=refset/X`, `?fhir_vs=ecl/…`).
pub enum Implicit {
    All,
    IsA(u32),
    Refset(u32),
    Ecl(ecl::Ecl),
}

/// The implicit value set `url` names; `Err` when it names no concept or
/// reference set (the value set does not exist).
pub fn implicit(m: &SnomedModel, url: &str) -> Option<Result<Implicit, HtsError>> {
    let rest = url.strip_prefix(SNOMED_URL)?;
    let query = rest.split_once("?fhir_vs").map(|(_, q)| q)?;
    let missing = || {
        Err(HtsError::NotFound(format!(
            "The value set '{url}' could not be found: it names no SNOMED CT concept or reference set"
        )))
    };
    Some(match query.strip_prefix('=') {
        None if query.is_empty() => Ok(Implicit::All),
        Some(q) => {
            if let Some(c) = q.strip_prefix("isa/") {
                match m.idx(c) {
                    Some(i) => Ok(Implicit::IsA(i)),
                    None => missing(),
                }
            } else if let Some(c) = q.strip_prefix("refset/") {
                match m.idx(c).filter(|&i| m.is_refset(i)) {
                    Some(i) => Ok(Implicit::Refset(i)),
                    None => missing(),
                }
            } else if let Some(e) = q.strip_prefix("ecl/") {
                let e = percent_decode(e);
                ecl::parse(&e)
                    .map(Implicit::Ecl)
                    .map_err(|err| HtsError::VsInvalid(format!("The ECL '{e}' is not valid: {err}")))
            } else {
                missing()
            }
        }
        _ => missing(),
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether the expression `e` (`v` in the model) is in the implicit value set.
pub fn in_implicit(m: &SnomedModel, imp: &Implicit, v: &Virtual) -> bool {
    match imp {
        Implicit::All => true,
        Implicit::IsA(x) => v.foci.iter().any(|&f| m.subsumes(*x, f)),
        Implicit::Refset(_) => false,
        Implicit::Ecl(e) => Evaluator::new(m).expression_matches(v, e).unwrap_or(false),
    }
}

/// Whether the expression is in the SNOMED CT part of `vs`'s compose.
pub fn in_value_set(m: &SnomedModel, vs: &Value, e: &Expression, v: &Virtual) -> bool {
    let Some(compose) = vs.get("compose") else { return false };
    let matches = |inc: &Value| -> bool {
        if inc.get("system").and_then(Value::as_str) != Some(SNOMED_URL) {
            return false;
        }
        if inc.get("valueSet").is_some() {
            return false;
        }
        let plain = scg::render_plain(e);
        if let Some(concepts) = inc.get("concept").and_then(Value::as_array) {
            return concepts.iter().any(|c| {
                c.get("code")
                    .and_then(Value::as_str)
                    .and_then(|code| scg::parse(code).ok())
                    .is_some_and(|x| scg::render_plain(&x) == plain)
            });
        }
        let Some(filters) = inc.get("filter").and_then(Value::as_array) else {
            return true;
        };
        filters.iter().all(|f| {
            let prop = f.get("property").and_then(Value::as_str).unwrap_or("");
            let op = f.get("op").and_then(Value::as_str).unwrap_or("");
            let value = f.get("value").and_then(Value::as_str).unwrap_or("");
            match (prop, op) {
                ("concept" | "code", "is-a" | "descendent-of") => m
                    .idx(value)
                    .is_some_and(|x| v.foci.iter().any(|&f| m.subsumes(x, f))),
                ("constraint" | "expression", "=") => ecl::parse(value)
                    .ok()
                    .is_some_and(|c| Evaluator::new(m).expression_matches(v, &c).unwrap_or(false)),
                ("expressions", "=") => value == "true",
                _ => false,
            }
        })
    };
    let includes = compose.get("include").and_then(Value::as_array);
    let excludes = compose.get("exclude").and_then(Value::as_array);
    includes.is_some_and(|i| i.iter().any(matches))
        && !excludes.is_some_and(|x| x.iter().any(matches))
}

fn coded(code: &'static str, tx_code: &'static str, text: String) -> HtsError {
    HtsError::Coded(Box::new(CodedIssue {
        code,
        tx_code,
        text,
        expression: None,
    }))
}

/// `$subsumes` where either side is an expression.
pub fn subsumes(m: &SnomedModel, a: &str, b: &str) -> Result<&'static str, HtsError> {
    let side = |code: &str| -> Result<Virtual, HtsError> {
        match check(m, code) {
            Checked::Valid { expr, .. } => virtual_of(m, &expr).ok_or_else(|| {
                coded("code-invalid", "invalid-code", format!("Invalid expression '{code}'"))
            }),
            Checked::Invalid(reason) => Err(coded(
                "code-invalid",
                "invalid-code",
                format!("The expression '{code}' is not valid for {SNOMED_URL}: {reason}"),
            )),
        }
    };
    let (va, vb) = (side(a)?, side(b)?);
    nf::outcome(m, &va, &vb).ok_or_else(|| {
        coded(
            "not-supported",
            "cannot-determine",
            format!(
                "Unable to determine whether '{a}' subsumes '{b}' or not: neither normal form subsumes the other, and without a classifier that does not show they are unrelated"
            ),
        )
    })
}

/// Rewrite the `$lookup` response of the focus concept of `expr` into the
/// expression's: its code and display, a single preferred designation, and
/// a property per refinement attribute.
pub fn lookup_response(m: &SnomedModel, code: &str, display: &str, expr: &Expression, focus: Value) -> Value {
    let mut params: Vec<Value> = focus
        .get("parameter")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    params.retain(|p| !matches!(p.get("name").and_then(Value::as_str), Some("designation" | "display" | "code")));
    params.push(json!({"name": "code", "valueCode": code}));
    params.push(json!({"name": "display", "valueString": display}));
    params.push(json!({"name": "designation", "part": [
        {"name": "language", "valueCode": "en-US"},
        {"name": "use", "valueCoding": {"system": "http://terminology.hl7.org/CodeSystem/hl7TermMaintInfra",
            "code": "preferredForLanguage", "display": "Preferred For Language"}},
        {"name": "value", "valueString": display},
    ]}));
    let mut term = term_of(m);
    for a in expr.body.ungrouped.iter().chain(expr.body.groups.iter().flatten()) {
        let (value, description) = match &a.value {
            scg::AttributeValue::Concept(c) => (
                c.id.clone(),
                c.term.clone().or_else(|| term(&c.id)).unwrap_or_default(),
            ),
            scg::AttributeValue::Expression(x) => {
                (scg::render_sub_plain(x), scg::render_sub_with_terms(x, &mut term))
            }
            scg::AttributeValue::Concrete(c) => (c.render(), c.render()),
        };
        let mut parts = vec![json!({"name": "code", "valueCode": a.name.id})];
        if let Some(t) = term(&a.name.id) {
            parts.push(json!({"name": "code-display", "valueString": t}));
        }
        parts.push(json!({"name": "description", "valueString": description}));
        parts.push(json!({"name": "value", "valueCode": value}));
        params.push(json!({"name": "property", "part": parts}));
    }
    json!({"resourceType": "Parameters", "parameter": params})
}
