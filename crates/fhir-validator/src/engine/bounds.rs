//! `minValue[x]` / `maxValue[x]` comparisons (ElementDefinition).
//!
//! Numbers compare numerically; dates, dateTimes, instants and times as
//! ISO 8601 strings at their common precision (`2024-06` is before
//! `2025-01-01`; `2025-01` against `2025-01-15` is undecided); Quantities by
//! value, after converting metric-prefixed UCUM units of the same base unit
//! (`g` / `kg`, `mL` / `L`). Anything else — different kinds, other units,
//! unparseable values — is undecided and not reported.

use serde_json::Value;
use std::cmp::Ordering;

/// How `data` compares with `bound`, when that can be decided.
pub(super) fn compare(data: &Value, bound: &Value) -> Option<Ordering> {
    match (data, bound) {
        (Value::Number(a), Value::Number(b)) => a.as_f64()?.partial_cmp(&b.as_f64()?),
        (Value::String(a), Value::String(b)) => compare_temporal(a, b),
        (Value::Object(_), Value::Object(_)) => compare_quantity(data, bound),
        _ => None,
    }
}

/// The HL7 validator's wording, close enough to match on.
pub(super) fn message(data: &Value, bound: &Value, below: bool) -> String {
    let show = |v: &Value| match v {
        Value::Object(q) => format!(
            "{} {}",
            q.get("value").map(Value::to_string).unwrap_or_default(),
            q.get("code")
                .or_else(|| q.get("unit"))
                .and_then(Value::as_str)
                .unwrap_or_default()
        )
        .trim()
        .to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if below {
        format!(
            "The value in the instance ({}) is less than the specified minimum value ({})",
            show(data),
            show(bound)
        )
    } else {
        format!(
            "The value in the instance ({}) is greater than the specified maximum value ({})",
            show(data),
            show(bound)
        )
    }
}

/// Date/time strings at their common precision: the shorter one's length
/// (`YYYY`, `YYYY-MM`, `YYYY-MM-DD`, `…Thh:mm:ss`, `hh:mm:ss`). Equal at the
/// common precision but of different precision is undecided.
fn compare_temporal(a: &str, b: &str) -> Option<Ordering> {
    let shape = |s: &str| {
        s.len() >= 4
            && s.chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '-' | ':' | 'T' | '.' | '+' | 'Z'))
    };
    if !shape(a) || !shape(b) {
        return None;
    }
    // Timezones and fractions are ignored: compare the calendar/clock part.
    let core = |s: &str| -> String {
        let s = s.split(['Z', '+']).next().unwrap_or(s);
        let s = match s.find('T') {
            // a trailing -hh:mm offset after the time
            Some(t) => match s[t..].rfind('-') {
                Some(off) => &s[..t + off],
                None => s,
            },
            None => s,
        };
        s.split('.').next().unwrap_or(s).to_string()
    };
    let (a, b) = (core(a), core(b));
    let n = a.len().min(b.len());
    match a[..n].cmp(&b[..n]) {
        Ordering::Equal if a.len() != b.len() => None,
        other => Some(other),
    }
}

/// UCUM metric prefixes and their factors.
const PREFIXES: &[(&str, f64)] = &[
    ("da", 1e1),
    ("k", 1e3),
    ("h", 1e2),
    ("d", 1e-1),
    ("c", 1e-2),
    ("m", 1e-3),
    ("u", 1e-6),
    ("n", 1e-9),
    ("p", 1e-12),
];
/// Base units that take metric prefixes.
const ATOMS: &[&str] = &["g", "m", "L", "l", "s", "mol", "Pa", "eq"];

/// `(base unit, factor)` of a simple UCUM code (`mg` → `("g", 1e-3)`).
fn base_unit(code: &str) -> Option<(&str, f64)> {
    if ATOMS.contains(&code) {
        return Some((code, 1.0));
    }
    PREFIXES.iter().find_map(|(prefix, factor)| {
        let atom = code.strip_prefix(prefix)?;
        ATOMS.contains(&atom).then_some((atom, *factor))
    })
}

fn compare_quantity(data: &Value, bound: &Value) -> Option<Ordering> {
    let value = |q: &Value| q.get("value").and_then(Value::as_f64);
    let unit = |q: &Value| {
        q.get("code")
            .or_else(|| q.get("unit"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let (a, b) = (value(data)?, value(bound)?);
    match (unit(data), unit(bound)) {
        (None, None) => a.partial_cmp(&b),
        (Some(x), Some(y)) if x == y => a.partial_cmp(&b),
        (Some(x), Some(y)) => {
            let ((ux, fx), (uy, fy)) = (base_unit(&x)?, base_unit(&y)?);
            if ux.eq_ignore_ascii_case(uy) && (ux == uy || ux.to_ascii_uppercase() == "L") {
                (a * fx).partial_cmp(&(b * fy))
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_dates_times_and_quantities() {
        assert_eq!(compare(&json!(2), &json!(3)), Some(Ordering::Less));
        assert_eq!(compare(&json!(2.5), &json!(2.5)), Some(Ordering::Equal));
        assert_eq!(
            compare(&json!("2024-06"), &json!("2025-01-01")),
            Some(Ordering::Less)
        );
        assert_eq!(compare(&json!("2025-01"), &json!("2025-01-15")), None);
        assert_eq!(
            compare(&json!("2026-01-01T10:00:00+08:00"), &json!("2025-01-01")),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare(&json!("00:30:00"), &json!("01:00:00")),
            Some(Ordering::Less)
        );
        let q =
            |v: f64, c: &str| json!({"value": v, "system": "http://unitsofmeasure.org", "code": c});
        assert_eq!(compare(&q(-1.0, "g"), &q(0.0, "kg")), Some(Ordering::Less));
        assert_eq!(
            compare(&q(11000.0, "g"), &q(10.0, "kg")),
            Some(Ordering::Greater)
        );
        assert_eq!(compare(&q(500.0, "mL"), &q(1.0, "L")), Some(Ordering::Less));
        assert_eq!(compare(&q(1.0, "mm[Hg]"), &q(1.0, "kg")), None);
        assert_eq!(compare(&json!("abc"), &json!("2025")), None);
    }
}
