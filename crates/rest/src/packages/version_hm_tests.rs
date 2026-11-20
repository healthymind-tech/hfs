//! Edge cases of package version ordering and dependency ranges.

use super::*;

fn v(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn compare_handles_build_metadata_prereleases_and_text_parts() {
    // Build metadata is ignored.
    assert_eq!(compare("1.0.0+build.7", "1.0.0"), Ordering::Equal);
    // A pre-release sorts below its release, also against a shorter form.
    assert_eq!(compare("1.0", "1.0.0-ballot"), Ordering::Greater);
    assert_eq!(compare("1.0.0-ballot", "1.0"), Ordering::Less);
    // Pre-release tags compare part by part, numbers numerically.
    assert_eq!(compare("1.0.0-ballot.2", "1.0.0-ballot.10"), Ordering::Less);
    assert_eq!(compare("1.0.0-alpha", "1.0.0-beta"), Ordering::Less);
    // A numeric part sorts below a textual one.
    assert_eq!(compare("1.0.1", "1.0.x"), Ordering::Less);
    assert_eq!(compare("1.0.x", "1.0.1"), Ordering::Greater);
    // Trailing zero components are insignificant; anything else is not.
    assert_eq!(compare("1.0.0.0", "1"), Ordering::Equal);
    assert_eq!(compare("1.0.0.1", "1"), Ordering::Greater);
    assert_eq!(compare("1", "1.0.0.1"), Ordering::Less);
    assert_eq!(compare("1.0.0", "1.0.0.0.0"), Ordering::Equal);
}

#[test]
fn numeric_parts_tolerate_short_and_odd_versions() {
    assert_eq!(numeric_parts("4"), [4, 0, 0]);
    assert_eq!(numeric_parts("4.0.1-ballot"), [4, 0, 1]);
    assert_eq!(numeric_parts("2022.4.20221006"), [2022, 4, 20221006]);
    assert_eq!(numeric_parts("x.y"), [0, 0, 0]);
    assert_eq!(numeric_parts("1.2.3.4"), [1, 2, 3]);
}

#[test]
fn range_parse_forms_and_whitespace() {
    for latest in ["", "  ", "CURRENT", "dev", "Latest", "*", "x"] {
        assert_eq!(Range::parse(latest), Range::Latest, "{latest:?}");
    }
    assert_eq!(Range::parse(" ^ 1.2.0 "), Range::Caret("1.2.0".into()));
    assert_eq!(Range::parse("~1.2"), Range::Tilde("1.2".into()));
    assert_eq!(Range::parse(">= 3.0.0"), Range::AtLeast("3.0.0".into()));
    assert_eq!(
        Range::parse("1.0.X"),
        Range::Wildcard(vec!["1".into(), "0".into()])
    );
    assert_eq!(Range::parse("2.*"), Range::Wildcard(vec!["2".into()]));
    assert_eq!(Range::parse("x.x"), Range::Wildcard(vec![]));
    assert_eq!(Range::parse("1.0.0-ballot"), Range::Exact("1.0.0-ballot".into()));
}

#[test]
fn minimum_of_each_range() {
    assert_eq!(Range::Latest.minimum(), None);
    assert_eq!(Range::parse("x.x").minimum(), None);
    assert_eq!(Range::parse("1.0.x").minimum().as_deref(), Some("1.0"));
    assert_eq!(Range::parse("^1.2.0").minimum().as_deref(), Some("1.2.0"));
    assert_eq!(Range::parse("~1.2.0").minimum().as_deref(), Some("1.2.0"));
    assert_eq!(Range::parse(">=1.2.0").minimum().as_deref(), Some("1.2.0"));
    assert_eq!(Range::parse("4.0.1").minimum().as_deref(), Some("4.0.1"));
}

#[test]
fn wildcard_and_bounds_matching() {
    let w = Range::parse("1.0.x");
    assert!(w.matches("1.0.0-ballot"), "pre-release of a matching line");
    assert!(!w.matches("1"), "too few components to know");
    assert!(!w.matches("11.0.0"));
    assert!(Range::parse("x.x").matches("9.9.9"));
    // Caret / tilde / at-least never go below their base.
    assert!(!Range::parse("^1.2.0").matches("1.1.9"));
    assert!(!Range::parse("~1.2.3").matches("1.2.2"));
    assert!(!Range::parse(">=1.2.0").matches("1.2.0-ballot"));
    assert!(Range::parse(">=1.2.0").matches("1.2"));
    // ^0.x keeps the minor.
    assert!(Range::parse("^0.2.0").matches("0.2.9"));
}

#[test]
fn newer_policy_with_wildcards_and_latest() {
    let ig = "hl7.fhir.us.core";
    // Wildcard minimum 1.0 → any 1.x at or above it.
    assert!(Range::parse("1.0.x").satisfied_by(ig, "1.3.0", DependencyPolicy::Newer));
    assert!(!Range::parse("1.0.x").satisfied_by(ig, "1.3.0", DependencyPolicy::Exact));
    assert!(!Range::parse("1.0.x").satisfied_by(ig, "2.0.0", DependencyPolicy::Newer));
    // An all-wildcard range has no minimum, so anything is newer.
    assert!(Range::parse("x.x").satisfied_by(ig, "0.0.1", DependencyPolicy::Newer));
    // Older is never accepted.
    assert!(!Range::parse("^1.2.0").satisfied_by(ig, "1.1.0", DependencyPolicy::Newer));
}

#[test]
fn cumulative_packages_are_recognised_by_prefix() {
    for name in [
        "hl7.terminology",
        "hl7.terminology.r4",
        "hl7.terminology.r5",
        "hl7.fhir.uv.extensions.r4",
        "hl7.fhir.uv.tools",
        "us.nlm.vsac",
        "us.cdc.phinvads",
        "fhir.dicom",
    ] {
        assert!(cumulative(name), "{name}");
        assert!(interchangeable(name, "1.0.0", "99.0.0"), "{name}");
    }
    for name in ["hl7.fhir.us.core", "us.nlm.vsac.extra", "fhir.dicom2"] {
        assert!(!cumulative(name), "{name}");
    }
    // IGs: same major, or same minor in 0.x; pre-releases use their numbers.
    assert!(interchangeable("ig", "2.0.0", "2.9.9-ballot"));
    assert!(!interchangeable("ig", "0.1.0", "0.2.0"));
    assert!(interchangeable("ig", "0.1.0", "0.1.7"));
}

#[test]
fn pick_prefers_releases_and_honours_exact_forms() {
    let available = v(&["1.0.0", "1.1.0", "2.0.0-ballot"]);
    // Latest without a registry `latest` tag: the newest release, not the
    // newer pre-release.
    assert_eq!(Range::Latest.pick(&available, None), Some("1.1.0"));
    // Only pre-releases match: the newest of them.
    let pre = v(&["3.0.0-ballot", "3.0.0-ballot2", "1.0.0"]);
    assert_eq!(Range::parse("3.x").pick(&pre, None), Some("3.0.0-ballot2"));
    // An exact pin finds an equal spelling.
    let short = v(&["4.0", "4.0.1"]);
    assert_eq!(Range::parse("4.0.0").pick(&short, None), Some("4.0"));
    // Nothing available.
    assert_eq!(Range::parse("^1.0.0").pick(&[], None), None);
    assert_eq!(Range::Latest.pick(&[], None), None);
}

#[test]
fn dependency_policy_serialises_kebab_case_and_defaults_to_newer() {
    assert_eq!(DependencyPolicy::default(), DependencyPolicy::Newer);
    assert_eq!(serde_json::to_string(&DependencyPolicy::Exact).unwrap(), "\"exact\"");
    let p: DependencyPolicy = serde_json::from_str("\"newer\"").unwrap();
    assert_eq!(p, DependencyPolicy::Newer);
    assert!(serde_json::from_str::<DependencyPolicy>("\"Newer\"").is_err());
}
