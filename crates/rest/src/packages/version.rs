//! Package version ordering and dependency ranges.
//!
//! FHIR packages use npm-style versions, but loosely: `4.0.1`,
//! `2022.4.20221006`, `1.0.0-ballot`, `0.1.0-cibuild`, and dependency
//! ranges are usually a pinned version, sometimes `1.0.x`, `^1.2.0`,
//! `~1.2.0`, `>=1.2.0`, or the tags `current` / `dev` / `latest`.
//! [`compare`] orders any two version strings (numeric parts numerically,
//! a pre-release below its release); [`Range`] covers the range forms above.

use std::cmp::Ordering;

/// Orders two package versions.
pub fn compare(a: &str, b: &str) -> Ordering {
    let (a_main, a_pre) = split_pre(a);
    let (b_main, b_pre) = split_pre(b);
    let ord = compare_dotted(a_main, b_main);
    if ord != Ordering::Equal {
        return ord;
    }
    match (a_pre, b_pre) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => compare_dotted(x, y),
    }
}

fn split_pre(v: &str) -> (&str, Option<&str>) {
    let v = v.split('+').next().unwrap_or(v);
    match v.split_once('-') {
        Some((main, pre)) => (main, Some(pre)),
        None => (v, None),
    }
}

fn compare_dotted(a: &str, b: &str) -> Ordering {
    let mut ai = a.split('.');
    let mut bi = b.split('.');
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return Ordering::Equal,
            (Some(x), None) => {
                return if x.chars().all(|c| c == '0') {
                    compare_dotted_rest(ai, Ordering::Greater)
                } else {
                    Ordering::Greater
                };
            }
            (None, Some(y)) => {
                return if y.chars().all(|c| c == '0') {
                    compare_dotted_rest(bi, Ordering::Less)
                } else {
                    Ordering::Less
                };
            }
            (Some(x), Some(y)) => {
                let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(x), Ok(y)) => x.cmp(&y),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => x.cmp(y),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

/// `1.0` vs `1.0.0.0`: trailing zero components are equal; anything else
/// left over decides in the given direction.
fn compare_dotted_rest<'a>(rest: impl Iterator<Item = &'a str>, longer: Ordering) -> Ordering {
    for part in rest {
        if !part.chars().all(|c| c == '0') {
            return longer;
        }
    }
    Ordering::Equal
}

/// Numeric `major.minor.patch` prefix of a version (missing parts are 0).
fn numeric_parts(v: &str) -> [u64; 3] {
    let (main, _) = split_pre(v);
    let mut out = [0u64; 3];
    for (slot, part) in out.iter_mut().zip(main.split('.')) {
        *slot = part.parse().unwrap_or(0);
    }
    out
}

/// A dependency requirement as written in `package.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Range {
    /// `current`, `dev`, `latest`, `*`, `x` or empty: any version, newest
    /// preferred.
    Latest,
    /// A pinned version.
    Exact(String),
    /// `1.0.x` / `1.x`: the given leading components must match.
    Wildcard(Vec<String>),
    /// `^1.2.3`: same major (or same minor for 0.x), at least this.
    Caret(String),
    /// `~1.2.3`: same major.minor, at least this.
    Tilde(String),
    /// `>=1.2.3`.
    AtLeast(String),
}

impl Range {
    /// Parses a requirement string.
    pub fn parse(raw: &str) -> Self {
        let r = raw.trim();
        let lower = r.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "" | "current" | "dev" | "latest" | "*" | "x"
        ) {
            return Self::Latest;
        }
        if let Some(v) = r.strip_prefix('^') {
            return Self::Caret(v.trim().to_string());
        }
        if let Some(v) = r.strip_prefix('~') {
            return Self::Tilde(v.trim().to_string());
        }
        if let Some(v) = r.strip_prefix(">=") {
            return Self::AtLeast(v.trim().to_string());
        }
        let parts: Vec<&str> = r.split('.').collect();
        if parts.iter().any(|p| matches!(*p, "x" | "X" | "*")) {
            let fixed = parts
                .iter()
                .take_while(|p| !matches!(**p, "x" | "X" | "*"))
                .map(|p| p.to_string())
                .collect();
            return Self::Wildcard(fixed);
        }
        Self::Exact(r.to_string())
    }

    /// Whether `version` satisfies the range exactly as written.
    pub fn matches(&self, version: &str) -> bool {
        match self {
            Self::Latest => true,
            Self::Exact(v) => compare(v, version) == Ordering::Equal,
            Self::Wildcard(fixed) => {
                let (main, _) = split_pre(version);
                let parts: Vec<&str> = main.split('.').collect();
                fixed.len() <= parts.len() && fixed.iter().zip(&parts).all(|(f, p)| f == p)
            }
            Self::Caret(base) => {
                let [bm, bn, _] = numeric_parts(base);
                let [m, n, _] = numeric_parts(version);
                compare(version, base) != Ordering::Less && m == bm && (bm != 0 || n == bn)
            }
            Self::Tilde(base) => {
                let [bm, bn, _] = numeric_parts(base);
                let [m, n, _] = numeric_parts(version);
                compare(version, base) != Ordering::Less && m == bm && n == bn
            }
            Self::AtLeast(base) => compare(version, base) != Ordering::Less,
        }
    }

    /// The smallest version the range admits, when it names one (`None` for
    /// [`Range::Latest`]). Used by the "a newer version satisfies" policy.
    pub fn minimum(&self) -> Option<String> {
        match self {
            Self::Latest => None,
            Self::Exact(v) | Self::Caret(v) | Self::Tilde(v) | Self::AtLeast(v) => Some(v.clone()),
            Self::Wildcard(fixed) if fixed.is_empty() => None,
            Self::Wildcard(fixed) => Some(fixed.join(".")),
        }
    }

    /// Whether an available `version` satisfies the requirement under
    /// `policy`: [`DependencyPolicy::Exact`] uses [`Range::matches`];
    /// [`DependencyPolicy::Newer`] also accepts any version at or above the
    /// range's minimum.
    pub fn satisfied_by(&self, version: &str, policy: DependencyPolicy) -> bool {
        if self.matches(version) {
            return true;
        }
        match policy {
            DependencyPolicy::Exact => false,
            DependencyPolicy::Newer => match self.minimum() {
                None => true,
                Some(min) => compare(version, &min) != Ordering::Less,
            },
        }
    }

    /// The best of `available` for this range: the newest that
    /// [`Range::matches`], preferring releases over pre-releases.
    pub fn pick<'a>(&self, available: &'a [String], latest: Option<&'a str>) -> Option<&'a str> {
        if *self == Self::Latest
            && let Some(latest) = latest
        {
            return Some(latest);
        }
        if let Self::Exact(v) = self {
            return available
                .iter()
                .find(|a| compare(a, v) == Ordering::Equal)
                .map(String::as_str);
        }
        let mut matching: Vec<&String> = available.iter().filter(|a| self.matches(a)).collect();
        matching.sort_by(|a, b| {
            let a_pre = split_pre(a).1.is_some();
            let b_pre = split_pre(b).1.is_some();
            b_pre.cmp(&a_pre).then_with(|| compare(a, b))
        });
        matching.last().map(|s| s.as_str())
    }
}

/// How dependency requirements are matched against installed packages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DependencyPolicy {
    /// An installed (or planned) version at or above the required one
    /// satisfies it, and when several packages need different versions of
    /// the same dependency the highest is installed once. This is what
    /// keeps a server with several IGs from carrying five copies of THO.
    #[default]
    Newer,
    /// Only the version range as written satisfies; other versions of the
    /// same package are installed side by side.
    Exact,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_versions_numerically_with_prereleases_first() {
        let mut v = vec![
            "1.10.0",
            "1.2.0",
            "1.2.0-ballot",
            "0.3.2",
            "2022.4.20221006",
            "1.2",
            "1.0.0",
        ];
        v.sort_by(|a, b| compare(a, b));
        assert_eq!(
            v,
            [
                "0.3.2",
                "1.0.0",
                "1.2.0-ballot",
                "1.2.0",
                "1.2",
                "1.10.0",
                "2022.4.20221006"
            ]
        );
        assert_eq!(compare("1.2", "1.2.0"), Ordering::Equal);
        assert_eq!(compare("7.0.0", "6.5.0"), Ordering::Greater);
    }

    #[test]
    fn parses_and_matches_range_forms() {
        assert_eq!(Range::parse("current"), Range::Latest);
        assert_eq!(Range::parse("4.0.1"), Range::Exact("4.0.1".into()));
        assert!(Range::parse("4.0.1").matches("4.0.1"));
        assert!(!Range::parse("4.0.1").matches("4.0.2"));
        assert!(Range::parse("1.0.x").matches("1.0.7"));
        assert!(!Range::parse("1.0.x").matches("1.1.0"));
        assert!(Range::parse("1.x").matches("1.9.0"));
        assert!(Range::parse("^1.2.0").matches("1.9.0"));
        assert!(!Range::parse("^1.2.0").matches("2.0.0"));
        assert!(!Range::parse("^0.2.0").matches("0.3.0"));
        assert!(Range::parse("~1.2.0").matches("1.2.5"));
        assert!(!Range::parse("~1.2.0").matches("1.3.0"));
        assert!(Range::parse(">=5.0.0").matches("7.4.0"));
    }

    #[test]
    fn newer_policy_accepts_higher_versions_exact_does_not() {
        let r = Range::parse("7.0.0");
        assert!(r.satisfied_by("7.4.0", DependencyPolicy::Newer));
        assert!(!r.satisfied_by("6.5.0", DependencyPolicy::Newer));
        assert!(!r.satisfied_by("7.4.0", DependencyPolicy::Exact));
        assert!(Range::parse("0.3.2").satisfied_by("1.0.0", DependencyPolicy::Newer));
        assert!(Range::Latest.satisfied_by("0.0.1", DependencyPolicy::Exact));
    }

    #[test]
    fn picks_the_newest_matching_release() {
        let available: Vec<String> = ["1.0.0", "1.1.0", "1.2.0-ballot", "2.0.0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(Range::parse("1.x").pick(&available, None), Some("1.1.0"));
        assert_eq!(
            Range::parse("current").pick(&available, Some("2.0.0")),
            Some("2.0.0")
        );
        assert_eq!(Range::parse("1.1.0").pick(&available, None), Some("1.1.0"));
        assert_eq!(Range::parse("3.0.0").pick(&available, None), None);
        assert_eq!(Range::parse("^1.0.0").pick(&available, None), Some("1.1.0"));
    }
}
