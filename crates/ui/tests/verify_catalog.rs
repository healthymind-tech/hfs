//! `locales/{en,zh-Hant}/verify.ftl` — the messages of the configuration
//! verifier (`helios_auth::verify`): both parse, define the same keys, and
//! cover every message code the verifier and the admin service emit
//! (`verify-{code}`) and every step id (`verify-step-{id}`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use fluent_syntax::ast::{Entry, Expression, InlineExpression, PatternElement};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Message ids of a catalog, each with the variables it references.
fn catalog(locale: &str) -> Vec<(String, BTreeSet<String>)> {
    let path = root().join("locales").join(locale).join("verify.ftl");
    let source =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let resource = fluent_syntax::parser::parse(source)
        .unwrap_or_else(|(_, errors)| panic!("{locale}/verify.ftl: {errors:?}"));
    resource
        .body
        .iter()
        .filter_map(|entry| match entry {
            Entry::Message(m) => {
                let mut vars = BTreeSet::new();
                if let Some(p) = &m.value {
                    for el in &p.elements {
                        if let PatternElement::Placeable {
                            expression:
                                Expression::Inline(InlineExpression::VariableReference { id }),
                        } = el
                        {
                            vars.insert(id.name.clone());
                        }
                    }
                }
                Some((m.id.name.clone(), vars))
            }
            _ => None,
        })
        .collect()
}

/// String literals of the verifier's non-test source that look like codes.
fn emitted_codes() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for file in [
        "crates/auth/src/verify.rs",
        "crates/rest/src/handlers/admin_endpoints/service.rs",
    ] {
        let text = std::fs::read_to_string(root().join(file)).unwrap();
        let text = text.split("#[cfg(test)]").next().unwrap();
        for (i, _) in text.match_indices("VerifyStep::new(") {
            // The code is the third argument: the first string literal
            // after the status (`Pass` / `VerifyStatus::Warn` / `code` var).
            let rest = &text[i..];
            let end = rest.find(')').map(|_| rest.len().min(600)).unwrap();
            let call = &rest[..end];
            let literals: Vec<&str> = call.split('"').skip(1).step_by(2).collect();
            if let Some(code) = literals.get(1)
                && code.contains('-')
                && code
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                out.insert(code.to_string());
            }
        }
        // Codes chosen through a variable (`(code, detail)` tuples, smart).
        for lit in text.split('"').skip(1).step_by(2) {
            let prefixes = ["outbound-", "connect-", "smart-no-"];
            if prefixes.iter().any(|p| lit.starts_with(p))
                && lit.chars().all(|c| c.is_ascii_lowercase() || c == '-')
            {
                out.insert(lit.to_string());
            }
        }
    }
    out
}

const STEP_IDS: &[&str] = &[
    "mode",
    "input",
    "outbound",
    "connect",
    "discovery",
    "issuer",
    "jwks-url",
    "jwks",
    "algorithms",
    "smart",
    "audience",
    "audience-shared",
    "token",
    "metadata",
    "lookup",
    "expand",
    "latency",
    "counts",
    "write",
    "incremental",
];

#[test]
fn catalogs_parse_and_share_keys_and_variables() {
    let en = catalog("en");
    let zh = catalog("zh-Hant");
    assert!(en.len() > 100);
    assert_eq!(
        en, zh,
        "en and zh-Hant verify.ftl differ (keys or variables)"
    );
    let ids: BTreeSet<&str> = en.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(ids.len(), en.len(), "duplicate keys");
}

#[test]
fn every_code_and_step_has_a_message() {
    let en: BTreeSet<String> = catalog("en").into_iter().map(|(k, _)| k).collect();
    let codes = emitted_codes();
    assert!(codes.len() > 80, "scanner found only {codes:?}");
    for must in [
        "discovery-404",
        "issuer-mismatch",
        "token-audience",
        "audience-shared",
    ] {
        assert!(codes.contains(must), "scanner missed {must}");
    }
    let missing: Vec<_> = codes
        .iter()
        .filter(|c| !en.contains(&format!("verify-{c}")))
        .collect();
    assert!(
        missing.is_empty(),
        "codes without verify-{{code}}: {missing:?}"
    );
    let missing: Vec<_> = STEP_IDS
        .iter()
        .filter(|id| !en.contains(&format!("verify-step-{id}")))
        .collect();
    assert!(missing.is_empty(), "steps without a title: {missing:?}");
    // Every fix belongs to a code.
    for k in &en {
        if let Some(code) = k
            .strip_prefix("verify-")
            .and_then(|c| c.strip_suffix("-fix"))
        {
            assert!(en.contains(&format!("verify-{code}")), "{k} has no message");
        }
    }
}

/// Codes reported only as pass or skipped: no "how to fix" needed.
const NO_FIX: &[&str] = &[
    "mode-off",
    "token-mode-off",
    "outbound-trusted",
    "token-ok",
    "token-accepted",
    "token-not-checkable",
    "token-issued",
    "token-not-needed",
    "counts-no-types",
];

#[test]
fn every_failure_or_warning_code_has_a_fix() {
    let en: BTreeSet<String> = catalog("en").into_iter().map(|(k, _)| k).collect();
    let missing: Vec<_> = en
        .iter()
        .filter_map(|k| k.strip_prefix("verify-"))
        .filter(|c| !c.starts_with("step-") && !c.ends_with("-fix") && !c.ends_with("-ok"))
        .filter(|c| !NO_FIX.contains(c))
        .filter(|c| !en.contains(&format!("verify-{c}-fix")))
        .collect();
    assert!(missing.is_empty(), "codes without a -fix: {missing:?}");
}

#[test]
fn traditional_chinese_messages_are_translated() {
    let path = root().join("locales/zh-Hant/verify.ftl");
    let text = std::fs::read_to_string(path).unwrap();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(" = ") else {
            continue;
        };
        if key.starts_with('#') {
            continue;
        }
        // Text outside placeables must contain Chinese, unless the message
        // is a bare placeable (`verify-counts-ok = { $summary }`).
        let mut outside = String::new();
        let mut depth = 0;
        for c in value.chars() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ if depth == 0 => outside.push(c),
                _ => {}
            }
        }
        if outside.trim().is_empty() {
            continue;
        }
        assert!(
            outside
                .chars()
                .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
            "{key} has no Chinese text: {value}"
        );
    }
}
