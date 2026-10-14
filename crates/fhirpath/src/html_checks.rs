//! `htmlChecks()` / `htmlChecks2()`: the FHIR narrative rules (R4 §2.4
//! Narrative, invariants `txt-1` and `txt-2`).
//!
//! The FHIRPath spec leaves these functions to FHIR; the HL7 Java validator
//! (`FHIRPathEngine.funcHtmlChecks1/2`) is the reference, and the element
//! and attribute lists below are its lists:
//!
//! - **`htmlChecks()`** (`txt-1`): the div parses as well-formed XML, its root
//!   is a `div` in the XHTML namespace, every element is in that namespace and
//!   on the allowed list (HTML 4.0 chapters 7-11 except §9.4, chapter 15,
//!   `a`, `img`, tables — so no `script`, `style`, `form`, `iframe`,
//!   `object`, `head`/`body`, ...), and every attribute is on the allowed
//!   list (generic, table, or element-specific — so no `onclick` or other
//!   event attributes, no `xlink:*`). No DOCTYPE, no processing
//!   instructions, and no `javascript:` URL in `href`/`src`. From R6 on the
//!   function also requires content, since R6 folds `txt-2` into `txt-1`.
//! - **`htmlChecks2()`** (`txt-2`): some non-whitespace text, or an `img`.
//!
//! HTML named entities (`&nbsp;`, `&copy;`, ...) are not XML. The Java
//! validator reports them as a separate "Invalid entity in the XHTML"
//! error; here they are deliberately read as a non-whitespace character
//! instead, because hand-written narratives use them and they break none of
//! the txt-1 rules (allowed elements and attributes, no active content).

use crate::evaluator::EvaluationContext;
use helios_fhir::FhirVersion;
use helios_fhirpath_support::{EvaluationError, EvaluationResult};

const XHTML_NS: &str = "http://www.w3.org/1999/xhtml";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// Elements a narrative may use (HL7 validator `checkHtmlNames`).
const ALLOWED_ELEMENTS: &[&str] = &[
    "p",
    "br",
    "div",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "a",
    "span",
    "b",
    "em",
    "i",
    "strong",
    "small",
    "big",
    "tt",
    "dfn",
    "q",
    "var",
    "abbr",
    "acronym",
    "cite",
    "blockquote",
    "hr",
    "address",
    "bdo",
    "kbd",
    "sub",
    "sup",
    "ul",
    "ol",
    "li",
    "dl",
    "dt",
    "dd",
    "pre",
    "table",
    "caption",
    "colgroup",
    "col",
    "thead",
    "tr",
    "tfoot",
    "tbody",
    "th",
    "td",
    "code",
    "samp",
    "img",
    "map",
    "area",
];

/// Attributes allowed on any element (core attributes, i18n, tables).
const ALLOWED_ATTRIBUTES: &[&str] = &[
    "title",
    "style",
    "class",
    "id",
    "lang",
    "xml:lang",
    "dir",
    "accesskey",
    "tabindex",
    // tables
    "span",
    "width",
    "align",
    "valign",
    "char",
    "charoff",
    "abbr",
    "axis",
    "headers",
    "scope",
    "rowspan",
    "colspan",
];

/// `element.attribute` pairs allowed in addition.
const ALLOWED_ELEMENT_ATTRIBUTES: &[&str] = &[
    "a.href",
    "a.name",
    "a.charset",
    "a.type",
    "a.hreflang",
    "a.rel",
    "a.rev",
    "a.shape",
    "a.coords",
    "img.src",
    "img.border",
    "img.alt",
    "img.longdesc",
    "img.height",
    "img.width",
    "img.usemap",
    "img.ismap",
    "div.xmlns",
    "blockquote.cite",
    "q.cite",
    "map.name",
    "area.shape",
    "area.coords",
    "area.href",
    "area.nohref",
    "area.alt",
    "table.summary",
    "table.width",
    "table.border",
    "table.frame",
    "table.rules",
    "table.cellspacing",
    "table.cellpadding",
    "pre.space",
    "td.nowrap",
];

/// The five entities XML predefines; every other named reference is HTML.
const XML_ENTITIES: &[&str] = &["lt", "gt", "amp", "quot", "apos"];

/// Rewrite HTML named entity references to `&#160;` so the XML parser
/// accepts them. Anything that is not a well-formed `&name;` is left alone
/// (the parser then rejects it).
fn normalize_entities(xhtml: &str) -> std::borrow::Cow<'_, str> {
    if !xhtml.contains('&') {
        return std::borrow::Cow::Borrowed(xhtml);
    }
    let mut out = String::with_capacity(xhtml.len());
    let mut rest = xhtml;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 1..];
        let name_len = after
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(after.len());
        let name = &after[..name_len];
        let is_named_ref = name_len > 0
            && name.starts_with(|c: char| c.is_ascii_alphabetic())
            && after[name_len..].starts_with(';');
        if is_named_ref && !XML_ENTITIES.contains(&name) {
            out.push_str("&#160;");
            rest = &after[name_len + 1..];
        } else {
            out.push('&');
            rest = after;
        }
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out)
}

fn parse(xhtml: &str) -> Option<roxmltree::Document<'_>> {
    roxmltree::Document::parse_with_options(
        xhtml,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            ..Default::default()
        },
    )
    .ok()
}

/// The `txt-1` rules: namespace, root, elements, attributes, active content.
pub fn check_html_names(xhtml: &str) -> bool {
    let normalized = normalize_entities(xhtml);
    let Some(doc) = parse(&normalized) else {
        return false;
    };
    let root = doc.root_element();
    if root.tag_name().name() != "div" {
        return false;
    }
    doc.root().descendants().all(node_is_allowed)
}

fn node_is_allowed(node: roxmltree::Node<'_, '_>) -> bool {
    if node.is_pi() {
        return false;
    }
    if !node.is_element() {
        return true;
    }
    let tag = node.tag_name();
    let name = tag.name();
    if tag.namespace() != Some(XHTML_NS) || !ALLOWED_ELEMENTS.contains(&name) {
        return false;
    }
    node.attributes().all(|attr| {
        let qualified = match attr.namespace() {
            None => attr.name().to_string(),
            Some(XML_NS) => format!("xml:{}", attr.name()),
            // Foreign-namespace attributes (xlink:href, ...) are not HTML.
            Some(_) => return false,
        };
        let allowed = ALLOWED_ATTRIBUTES.contains(&qualified.as_str())
            || ALLOWED_ELEMENT_ATTRIBUTES.contains(&format!("{name}.{qualified}").as_str());
        allowed && !(matches!(qualified.as_str(), "href" | "src") && is_script_url(attr.value()))
    })
}

/// `javascript:` (and `vbscript:`) URLs are active content.
fn is_script_url(value: &str) -> bool {
    let compact: String = value
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .take(11)
        .collect::<String>()
        .to_ascii_lowercase();
    compact.starts_with("javascript:") || compact.starts_with("vbscript:")
}

/// The `txt-2` rule: some non-whitespace text (Java `trim()` semantics:
/// anything above U+0020 counts), or an image.
pub fn check_html_content(xhtml: &str) -> bool {
    let normalized = normalize_entities(xhtml);
    let Some(doc) = parse(&normalized) else {
        return false;
    };
    doc.root().descendants().any(|node| {
        (node.is_text() && node.text().is_some_and(|t| t.chars().any(|c| c > ' ')))
            || (node.is_element() && node.tag_name().name() == "img")
    })
}

/// The xhtml string the function was invoked on, if it is exactly one.
fn single_string(base: &EvaluationResult) -> Option<&str> {
    match base {
        EvaluationResult::String(s, _, _) => Some(s.as_str()),
        EvaluationResult::Collection { items, .. } if items.len() == 1 => single_string(&items[0]),
        _ => None,
    }
}

/// R6 merged `txt-2` into `txt-1`, so there `htmlChecks()` also requires
/// content (as the Java validator does for R6+).
fn folds_content_rule(version: FhirVersion) -> bool {
    #[cfg(feature = "R6")]
    if matches!(version, FhirVersion::R6) {
        return true;
    }
    let _ = version;
    false
}

/// `htmlChecks()`. Anything but a single xhtml string is `false`, as in the
/// Java validator.
pub fn html_checks_function(
    base: &EvaluationResult,
    args: &[EvaluationResult],
    context: &EvaluationContext,
) -> Result<EvaluationResult, EvaluationError> {
    if !args.is_empty() {
        return Err(EvaluationError::InvalidArity(
            "Function 'htmlChecks' expects 0 arguments".to_string(),
        ));
    }
    let ok = single_string(base).is_some_and(|x| {
        check_html_names(x) && (!folds_content_rule(context.fhir_version) || check_html_content(x))
    });
    Ok(EvaluationResult::boolean(ok))
}

/// `htmlChecks2()`: the content rule on its own.
pub fn html_checks2_function(
    base: &EvaluationResult,
    args: &[EvaluationResult],
) -> Result<EvaluationResult, EvaluationError> {
    if !args.is_empty() {
        return Err(EvaluationError::InvalidArity(
            "Function 'htmlChecks2' expects 0 arguments".to_string(),
        ));
    }
    Ok(EvaluationResult::boolean(
        single_string(base).is_some_and(check_html_content),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn div(inner: &str) -> String {
        format!(r#"<div xmlns="http://www.w3.org/1999/xhtml">{inner}</div>"#)
    }

    #[test]
    fn plain_narratives_pass() {
        assert!(check_html_names(&div("<p>Hello <b>world</b></p>")));
        assert!(check_html_names(&div(
            r#"<table class="grid"><tr><td colspan="2" style="color:red">x</td></tr></table>"#
        )));
        assert!(check_html_names(&div(
            r#"<a href="http://example.org">link</a><img src="a.png" alt="pic"/>"#
        )));
        assert!(check_html_names(&div(
            r#"<span xml:lang="zh-TW">中文</span>"#
        )));
    }

    #[test]
    fn root_and_namespace_are_enforced() {
        assert!(!check_html_names("<div>no namespace</div>"));
        assert!(!check_html_names(
            r#"<p xmlns="http://www.w3.org/1999/xhtml">not a div</p>"#
        ));
        assert!(!check_html_names(&div(
            r#"<svg xmlns="http://www.w3.org/2000/svg"><circle r="1"/></svg>"#
        )));
    }

    #[test]
    fn active_content_and_unknown_elements_fail() {
        assert!(!check_html_names(&div("<script>alert(1)</script>")));
        assert!(!check_html_names(&div("<style>p{}</style>")));
        assert!(!check_html_names(&div("<iframe src=\"x\"></iframe>")));
        assert!(!check_html_names(&div("<form><input/></form>")));
        assert!(!check_html_names(&div("<ins>x</ins>")));
    }

    #[test]
    fn event_and_foreign_attributes_fail() {
        assert!(!check_html_names(&div(r#"<p onclick="x()">x</p>"#)));
        assert!(!check_html_names(&div(
            r#"<img src="a.png" onerror="x()"/>"#
        )));
        assert!(!check_html_names(&div(
            r#"<a xmlns:xlink="http://www.w3.org/1999/xlink" xlink:href="x">x</a>"#
        )));
        // `href` is only allowed on `a`/`area`.
        assert!(!check_html_names(&div(r#"<p href="x">x</p>"#)));
    }

    #[test]
    fn script_urls_fail() {
        assert!(!check_html_names(&div(
            r#"<a href="javascript:alert(1)">x</a>"#
        )));
        assert!(!check_html_names(&div(r#"<a href=" JavaScript:x">x</a>"#)));
        assert!(!check_html_names(&div(r#"<img src="vbscript:x"/>"#)));
        assert!(check_html_names(&div(
            r#"<a href="https://x/javascript:">x</a>"#
        )));
    }

    #[test]
    fn malformed_xml_and_doctype_fail() {
        assert!(!check_html_names(&div("<p>unclosed")));
        assert!(!check_html_names("not xml at all"));
        assert!(!check_html_names(
            r#"<!DOCTYPE div><div xmlns="http://www.w3.org/1999/xhtml">x</div>"#
        ));
        assert!(!check_html_names(&div("<?php echo 1 ?>x")));
    }

    #[test]
    fn html_entities_are_accepted() {
        assert!(check_html_names(&div("a&nbsp;b &copy; &amp; &#169;")));
        assert!(check_html_content(&div("&nbsp;")));
        // A bare ampersand is still malformed XML.
        assert!(!check_html_names(&div("a & b")));
    }

    #[test]
    fn content_rule() {
        assert!(check_html_content(&div("<p>x</p>")));
        assert!(check_html_content(&div(r#"<img src="a.png"/>"#)));
        assert!(!check_html_content(&div("")));
        assert!(!check_html_content(&div("  \n\t<p> </p><br/>")));
        // Comments are not content.
        assert!(!check_html_content(&div("<!-- note -->")));
    }
}
