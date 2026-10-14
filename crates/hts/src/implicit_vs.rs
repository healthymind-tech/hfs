//! Implicit ValueSet URLs: canonical URLs that describe a ValueSet by rule
//! instead of naming a stored resource.
//!
//! | URL | Pattern | Members |
//! |---|---|---|
//! | `<cs>?fhir_vs` | [`AllConcepts`] | every code of the CodeSystem (FHIR R4 §4.8.7) |
//! | `<cs>?fhir_vs=isa/<code>` | [`IsA`] | `code` and its descendants |
//! | `<cs>?fhir_vs=ecl/<expr>` | [`Ecl`] | the codes the (URL-encoded) ECL expression selects (SNOMED CT) |
//! | `<cs>?fhir_vs=refset/<id>` | [`Refset`] | the members of a SNOMED CT reference set |
//! | `http://loinc.org/vs` | [`AllConcepts`] of `http://loinc.org` | all of LOINC |
//! | `http://loinc.org/vs/<LP…>` | [`DescendantOf`] the part | the LOINC codes under a part in the multiaxial hierarchy |
//!
//! `http://loinc.org/vs/<LL…>` (an answer list) is deliberately *not* here:
//! the LOINC importer stores each answer list as a ValueSet resource with that
//! URL, so it resolves like any other stored ValueSet.
//!
//! [`AllConcepts`]: ImplicitVsPattern::AllConcepts
//! [`IsA`]: ImplicitVsPattern::IsA
//! [`DescendantOf`]: ImplicitVsPattern::DescendantOf
//! [`Ecl`]: ImplicitVsPattern::Ecl
//! [`Refset`]: ImplicitVsPattern::Refset

/// LOINC's CodeSystem URL.
pub(crate) const LOINC_URL: &str = "http://loinc.org";
/// LOINC's implicit "all of LOINC" ValueSet, and the prefix of its per-part
/// and per-answer-list ValueSets.
pub(crate) const LOINC_VS_URL: &str = "http://loinc.org/vs";

/// The rule an implicit ValueSet URL describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImplicitVsPattern {
    /// Every code in the CodeSystem.
    AllConcepts,
    /// The code and its descendants (`<<`).
    IsA(String),
    /// The code's descendants, not the code itself (`<`).
    DescendantOf(String),
    /// An ECL expression, already percent-decoded.
    Ecl(String),
    /// The members of a reference set (`^ id`).
    Refset(String),
}

impl ImplicitVsPattern {
    /// The compose this pattern is shorthand for, as a ValueSet `compose`.
    /// (The PostgreSQL backend expands implicit ValueSets through it.)
    #[cfg_attr(not(feature = "postgres"), allow(dead_code))]
    pub(crate) fn compose(&self, cs_url: &str) -> serde_json::Value {
        let filter = |property: &str, op: &str, value: &str| {
            serde_json::json!({
                "include": [{
                    "system": cs_url,
                    "filter": [{ "property": property, "op": op, "value": value }],
                }]
            })
        };
        match self {
            Self::AllConcepts => serde_json::json!({ "include": [{ "system": cs_url }] }),
            Self::IsA(code) => filter("concept", "is-a", code),
            Self::DescendantOf(code) => filter("concept", "descendent-of", code),
            Self::Ecl(expr) => filter("constraint", "=", expr),
            Self::Refset(id) => filter("constraint", "=", &format!("^ {id}")),
        }
    }
}

/// Parse an implicit ValueSet URL into `(CodeSystem URL, pattern)`.
///
/// Returns `None` for any URL that is not one of the forms in the module
/// docs, including `?fhir_vs=` forms HTS does not implement.
pub(crate) fn parse_implicit_vs_url(url: &str) -> Option<(String, ImplicitVsPattern)> {
    if let Some(rest) = url.strip_prefix(LOINC_VS_URL) {
        return parse_loinc_vs(rest).map(|p| (LOINC_URL.to_owned(), p));
    }
    let (base, query) = url.split_once('?')?;
    let rest = query.strip_prefix("fhir_vs")?;
    if rest.is_empty() {
        return Some((base.to_owned(), ImplicitVsPattern::AllConcepts));
    }
    let (kind, arg) = rest.strip_prefix('=')?.split_once('/')?;
    let pattern = match kind {
        "isa" if !arg.is_empty() => ImplicitVsPattern::IsA(arg.to_owned()),
        "ecl" => ImplicitVsPattern::Ecl(percent_decode(arg).filter(|e| !e.trim().is_empty())?),
        "refset" if !arg.is_empty() => ImplicitVsPattern::Refset(arg.to_owned()),
        _ => return None,
    };
    Some((base.to_owned(), pattern))
}

/// `true` when `url` is an implicit ValueSet URL (see [`parse_implicit_vs_url`]).
pub(crate) fn is_implicit_vs_url(url: &str) -> bool {
    parse_implicit_vs_url(url).is_some()
}

/// The part of a `http://loinc.org/vs…` URL after `/vs`.
fn parse_loinc_vs(rest: &str) -> Option<ImplicitVsPattern> {
    if rest.is_empty() || rest == "/" {
        return Some(ImplicitVsPattern::AllConcepts);
    }
    let id = rest.strip_prefix('/')?;
    // LOINC part numbers: `LP` + digits + `-` + check digit.
    if id.len() > 2 && id[..2].eq_ignore_ascii_case("LP") && is_loinc_id_body(&id[2..]) {
        return Some(ImplicitVsPattern::DescendantOf(id.to_ascii_uppercase()));
    }
    None
}

/// `<digits>-<digit>`, the body of a LOINC LP/LL/LA identifier.
fn is_loinc_id_body(s: &str) -> bool {
    match s.split_once('-') {
        Some((num, check)) => {
            !num.is_empty()
                && num.bytes().all(|b| b.is_ascii_digit())
                && check.len() == 1
                && check.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

/// Decode `%XX` escapes (an ECL expression inside a URL is percent-encoded:
/// `ecl/%3C%3C%20404684003`). `+` is left alone — it is not an ECL character
/// and is not a space outside form encoding. `None` on invalid UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    if !s.contains('%') {
        return Some(s.to_owned());
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(url: &str) -> Option<(String, ImplicitVsPattern)> {
        parse_implicit_vs_url(url)
    }

    #[test]
    fn fhir_vs_forms() {
        assert_eq!(
            parse("http://snomed.info/sct?fhir_vs"),
            Some((
                "http://snomed.info/sct".into(),
                ImplicitVsPattern::AllConcepts
            ))
        );
        assert_eq!(
            parse("http://snomed.info/sct?fhir_vs=isa/404684003"),
            Some((
                "http://snomed.info/sct".into(),
                ImplicitVsPattern::IsA("404684003".into())
            ))
        );
        assert_eq!(
            parse("http://snomed.info/sct?fhir_vs=refset/723264001"),
            Some((
                "http://snomed.info/sct".into(),
                ImplicitVsPattern::Refset("723264001".into())
            ))
        );
        assert_eq!(parse("http://snomed.info/sct?fhir_vs=isa/"), None);
        assert_eq!(parse("http://snomed.info/sct?fhir_vs=refset"), None);
        assert_eq!(parse("http://snomed.info/sct?fhir_vs=unknown/1"), None);
        assert_eq!(parse("http://snomed.info/sct"), None);
    }

    #[test]
    fn ecl_is_percent_decoded() {
        assert_eq!(
            parse(
                "http://snomed.info/sct?fhir_vs=ecl/%3C%3C%20404684003%20%7CClinical%20finding%7C"
            ),
            Some((
                "http://snomed.info/sct".into(),
                ImplicitVsPattern::Ecl("<< 404684003 |Clinical finding|".into())
            ))
        );
        // Already-decoded (the HTTP layer decoded the query once).
        assert_eq!(
            parse("http://snomed.info/sct?fhir_vs=ecl/<< 73211009"),
            Some((
                "http://snomed.info/sct".into(),
                ImplicitVsPattern::Ecl("<< 73211009".into())
            ))
        );
        assert_eq!(parse("http://snomed.info/sct?fhir_vs=ecl/"), None);
        // A stray `%` that is not an escape stays literal.
        assert_eq!(percent_decode("a%zz%4").as_deref(), Some("a%zz%4"));
    }

    #[test]
    fn loinc_vs_forms() {
        assert_eq!(
            parse("http://loinc.org/vs"),
            Some(("http://loinc.org".into(), ImplicitVsPattern::AllConcepts))
        );
        assert_eq!(
            parse("http://loinc.org/vs/LP14635-4"),
            Some((
                "http://loinc.org".into(),
                ImplicitVsPattern::DescendantOf("LP14635-4".into())
            ))
        );
        // Answer lists are stored ValueSets, not implicit ones.
        assert_eq!(parse("http://loinc.org/vs/LL1-9"), None);
        assert_eq!(parse("http://loinc.org/vs/LP14635"), None);
        assert_eq!(parse("http://loinc.org/vsx"), None);
        assert_eq!(parse("http://loinc.org/vs/2345-7"), None);
    }

    #[test]
    fn compose_matches_pattern() {
        let c = ImplicitVsPattern::DescendantOf("LP1-1".into()).compose(LOINC_URL);
        assert_eq!(c["include"][0]["filter"][0]["op"], "descendent-of");
        let c = ImplicitVsPattern::Refset("1".into()).compose("http://snomed.info/sct");
        assert_eq!(c["include"][0]["filter"][0]["value"], "^ 1");
        let c = ImplicitVsPattern::AllConcepts.compose(LOINC_URL);
        assert!(c["include"][0].get("filter").is_none());
    }
}
