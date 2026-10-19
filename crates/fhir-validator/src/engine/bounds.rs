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

/// A `minValue`/`maxValue` given as a Duration on a date/time element: the
/// bound is now minus (min) or plus (max) the duration. Returns the bound
/// rendered as a dateTime and the duration as written (`100a`).
pub(super) fn duration_bound(
    bound: &Value,
    below: bool,
    now_secs: i64,
) -> Option<(String, String)> {
    let q = bound.as_object()?;
    let value = q.get("value")?.as_f64()?;
    let code = q.get("code").or_else(|| q.get("unit"))?.as_str()?;
    let sign: i64 = if below { -1 } else { 1 };
    let (days, secs) = (now_secs.div_euclid(86_400), now_secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    let whole = value.trunc() as i64;
    let shifted_secs = |delta: f64| now_secs + sign * delta.round() as i64;
    let at = match code {
        "a" | "mo" => {
            let months = if code == "a" { whole * 12 } else { whole };
            let total = y * 12 + (m - 1) + sign * months;
            let (ny, nm) = (total.div_euclid(12), total.rem_euclid(12) + 1);
            let nd = d.min(days_in_month(ny, nm));
            days_from_civil(ny, nm, nd) * 86_400 + secs
        }
        "wk" => shifted_secs(value * 7.0 * 86_400.0),
        "d" => shifted_secs(value * 86_400.0),
        "h" => shifted_secs(value * 3_600.0),
        "min" => shifted_secs(value * 60.0),
        "s" => shifted_secs(value),
        _ => return None,
    };
    let (dy, dm, dd) = civil_from_days(at.div_euclid(86_400));
    let t = at.rem_euclid(86_400);
    let rendered = format!(
        "{dy:04}-{dm:02}-{dd:02}T{:02}:{:02}:{:02}Z",
        t / 3600,
        t / 60 % 60,
        t % 60
    );
    let shown = q.get("value").map(Value::to_string).unwrap_or_default();
    Some((rendered, format!("{shown}{code}")))
}

/// How a date/time string compares with a Duration bound (see
/// [`duration_bound`]), and the HL7 validator's message when violated.
pub(super) fn duration_violation(data: &Value, bound: &Value, below: bool) -> Option<String> {
    let text = data.as_str()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let (at, written) = duration_bound(bound, below, now)?;
    let ordering = compare_temporal(text, &at)?;
    let violated = if below {
        ordering == Ordering::Less
    } else {
        ordering == Ordering::Greater
    };
    violated.then(|| {
        format!(
            "The date/time value '{text}' is not legal: {} is {written} {} now (which is {at})",
            if below { "minValue" } else { "maxValue" },
            if below { "before" } else { "after" },
        )
    })
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
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
    fn duration_bounds_relative_to_now() {
        // 2026-10-20T00:00:00Z
        let now = days_from_civil(2026, 10, 20) * 86_400;
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        let years = json!({"value": 100, "code": "a"});
        assert_eq!(
            duration_bound(&years, true, now),
            Some(("1926-10-20T00:00:00Z".into(), "100a".into()))
        );
        let days = json!({"value": 10, "unit": "d"});
        assert_eq!(
            duration_bound(&days, false, now).unwrap().0,
            "2026-10-30T00:00:00Z"
        );
        assert!(duration_violation(&json!("1850-01-01"), &years, true).is_some());
        assert!(duration_violation(&json!("1950-01-01"), &years, true).is_none());
        let ten = json!({"value": 10, "code": "a"});
        assert!(duration_violation(&json!("2100-01-01"), &ten, false).is_some());
        assert!(duration_violation(&json!("2020-01-01"), &ten, false).is_none());
    }

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
