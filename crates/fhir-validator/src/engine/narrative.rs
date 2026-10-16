//! Checks on resource content the HL7 validator applies outside the
//! profile walk: internal narrative hyperlinks, and the Observation
//! best-practice recommendations.
//!
//! Both run over the raw JSON after the structural walk, once per resource
//! in the instance (the root, Bundle entries, contained and other nested
//! resources), each resource judged on its own as the HL7 validator does.

use super::errors::{
    msg_narrative_link, msg_observation_performer, msg_observation_subject,
};
use super::{ErrorKind, Severity, ValidationError};
use serde_json::Value;
use std::collections::HashSet;

/// Every resource in `value` (itself included when it is one), with its
/// dotted path.
fn resources<'a>(value: &'a Value, path: String, out: &mut Vec<(String, &'a Value)>) {
    match value {
        Value::Object(map) => {
            if map.get("resourceType").is_some_and(Value::is_string) {
                out.push((path.clone(), value));
            }
            for (key, child) in map {
                if child.is_object() || child.is_array() {
                    resources(child, format!("{path}.{key}"), out);
                }
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                resources(item, format!("{path}.{index}"), out);
            }
        }
        _ => {}
    }
}

/// The resources of the instance to check, with their dotted paths.
fn all_resources(resource: &Value) -> Vec<(String, &Value)> {
    let root = resource
        .get("resourceType")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut out = Vec::new();
    resources(resource, root, &mut out);
    out
}

// ---------------------------------------------------------------------
// Narrative hyperlinks
// ---------------------------------------------------------------------

/// One start or end tag of an XHTML fragment.
struct Tag<'a> {
    name: &'a str,
    closing: bool,
    self_closing: bool,
    attrs: Vec<(&'a str, &'a str)>,
    /// Byte offset just past the tag.
    end: usize,
    /// Byte offset of the `<`.
    start: usize,
}

/// A tolerant tag scanner: comments, CDATA and processing instructions are
/// skipped, attribute values may be single- or double-quoted.
fn tags(xhtml: &str) -> Vec<Tag<'_>> {
    let bytes = xhtml.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = xhtml[i..].find('<') {
        let start = i + off;
        let rest = &xhtml[start..];
        if rest.starts_with("<!--") {
            i = rest.find("-->").map_or(xhtml.len(), |e| start + e + 3);
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            i = rest.find('>').map_or(xhtml.len(), |e| start + e + 1);
            continue;
        }
        let mut j = start + 1;
        let closing = bytes.get(j) == Some(&b'/');
        if closing {
            j += 1;
        }
        let name_start = j;
        while j < bytes.len() && !bytes[j].is_ascii_whitespace() && bytes[j] != b'>' && bytes[j] != b'/' {
            j += 1;
        }
        let name = &xhtml[name_start..j];
        let mut attrs = Vec::new();
        let mut self_closing = false;
        loop {
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j >= bytes.len() {
                break;
            }
            match bytes[j] {
                b'>' => {
                    j += 1;
                    break;
                }
                b'/' => {
                    self_closing = true;
                    j += 1;
                    continue;
                }
                _ => {}
            }
            let an_start = j;
            while j < bytes.len()
                && !bytes[j].is_ascii_whitespace()
                && bytes[j] != b'='
                && bytes[j] != b'>'
                && bytes[j] != b'/'
            {
                j += 1;
            }
            let attr = &xhtml[an_start..j];
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let mut value = "";
            if bytes.get(j) == Some(&b'=') {
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                match bytes.get(j) {
                    Some(&q) if q == b'"' || q == b'\'' => {
                        let v_start = j + 1;
                        let v_end = xhtml[v_start..]
                            .find(q as char)
                            .map_or(xhtml.len(), |e| v_start + e);
                        value = &xhtml[v_start..v_end];
                        j = (v_end + 1).min(xhtml.len());
                    }
                    _ => {
                        let v_start = j;
                        while j < bytes.len() && !bytes[j].is_ascii_whitespace() && bytes[j] != b'>' {
                            j += 1;
                        }
                        value = &xhtml[v_start..j];
                    }
                }
            }
            if attr.is_empty() {
                j += 1; // stray character; keep moving
                continue;
            }
            attrs.push((attr, value));
        }
        if !name.is_empty() {
            out.push(Tag {
                name,
                closing,
                self_closing,
                attrs,
                end: j,
                start,
            });
        }
        i = j.max(start + 1);
    }
    out
}

fn attr<'a>(tag: &Tag<'a>, name: &str) -> Option<&'a str> {
    tag.attrs.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
}

/// Fragment targets inside one XHTML fragment: `id` attributes and
/// `<a name>` anchors.
fn xhtml_targets<'a>(xhtml: &'a str, out: &mut HashSet<&'a str>) {
    for tag in tags(xhtml) {
        if tag.closing {
            continue;
        }
        if let Some(id) = attr(&tag, "id") {
            out.insert(id);
        }
        if tag.name == "a"
            && let Some(name) = attr(&tag, "name")
        {
            out.insert(name);
        }
    }
}

/// Everything a `#fragment` link in `resource` may point at: element and
/// resource ids anywhere in it (contained resources included) and the
/// ids/anchors of every narrative in it — what the HL7 validator counts.
fn targets(value: &Value, out: &mut HashSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                match child {
                    Value::String(s) if key == "id" => {
                        out.insert(s.clone());
                    }
                    Value::String(s) if key == "div" => {
                        let mut found = HashSet::new();
                        xhtml_targets(s, &mut found);
                        out.extend(found.into_iter().map(str::to_string));
                    }
                    Value::Object(_) | Value::Array(_) => targets(child, out),
                    _ => {}
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                targets(item, out);
            }
        }
        _ => {}
    }
}

/// Text content between `from` and `to` in `xhtml`, tags dropped and
/// whitespace collapsed.
fn text_between(xhtml: &str, from: usize, to: usize) -> String {
    let mut text = String::new();
    let mut in_tag = false;
    for c in xhtml[from..to.min(xhtml.len())].chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Void HTML elements (never closed).
const VOID: &[&str] = &["br", "hr", "img", "col", "area", "input", "meta", "link", "wbr"];

/// Unresolved internal links (`<a href="#x">`) in one narrative `div`.
fn unresolved_links(xhtml: &str, known: &HashSet<String>) -> Vec<(String, String, String)> {
    let all = tags(xhtml);
    let mut out = Vec::new();
    let mut stack: Vec<&str> = Vec::new();
    for (index, tag) in all.iter().enumerate() {
        if tag.closing {
            if let Some(pos) = stack.iter().rposition(|n| *n == tag.name) {
                stack.truncate(pos);
            }
            continue;
        }
        let void = tag.self_closing || VOID.contains(&tag.name);
        if !void {
            stack.push(tag.name);
        }
        if tag.name != "a" {
            continue;
        }
        let Some(href) = attr(tag, "href") else {
            continue;
        };
        let Some(fragment) = href.strip_prefix('#') else {
            continue;
        };
        if fragment.is_empty() || known.contains(fragment) {
            continue;
        }
        let at = if void {
            let mut p = stack.join("/");
            p.push_str("/a");
            p
        } else {
            stack.join("/")
        };
        // The link text: up to the matching `</a>`.
        let close = all[index + 1..]
            .iter()
            .find(|t| t.closing && t.name == "a")
            .map_or(tag.end, |t| t.start);
        let text = if void {
            String::new()
        } else {
            text_between(xhtml, tag.end, close)
        };
        out.push((href.to_string(), at, text));
    }
    out
}

/// `<a href="#fragment">` links in a resource's narrative that point at
/// nothing in that resource (HL7 validator Type_Specific_Checks_DT_XHTML_Resolve,
/// error). A Bundle entry is its own resource: an anchor in another entry's
/// narrative does not count.
pub(crate) fn narrative_link_errors(resource: &Value) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    for (path, res) in all_resources(resource) {
        let Some(div) = res.pointer("/text/div").and_then(Value::as_str) else {
            continue;
        };
        if !div.contains("href") {
            continue;
        }
        let mut known = HashSet::new();
        targets(res, &mut known);
        for (href, at, text) in unresolved_links(div, &known) {
            errors.push(ValidationError::new(
                ErrorKind::Narrative,
                format!("{path}.text.div"),
                msg_narrative_link(&href, &at, &text),
            ));
        }
    }
    errors
}

// ---------------------------------------------------------------------
// Best practice
// ---------------------------------------------------------------------

/// The HL7 validator's best-practice recommendations for Observation
/// (warnings by default): every Observation should have a subject and a
/// performer.
pub(crate) fn best_practice_warnings(resource: &Value) -> Vec<ValidationError> {
    let mut warnings = Vec::new();
    for (path, res) in all_resources(resource) {
        if res.get("resourceType").and_then(Value::as_str) != Some("Observation") {
            continue;
        }
        let present = |key: &str| match res.get(key) {
            Some(Value::Array(items)) => !items.is_empty(),
            Some(Value::Null) | None => false,
            Some(_) => true,
        };
        if !present("subject") {
            warnings.push(
                ValidationError::new(ErrorKind::BestPractice, path.clone(), msg_observation_subject())
                    .with_severity(Severity::Warning),
            );
        }
        if !present("performer") {
            warnings.push(
                ValidationError::new(
                    ErrorKind::BestPractice,
                    path.clone(),
                    msg_observation_performer(),
                )
                .with_severity(Severity::Warning),
            );
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn links_resolve_to_anchors_ids_and_element_ids() {
        let patient = json!({
            "resourceType": "Patient", "id": "p1",
            "contained": [{"resourceType": "Organization", "id": "org1"}],
            "text": {"status": "generated", "div": concat!(
                "<div xmlns=\"http://www.w3.org/1999/xhtml\">",
                "<p id='top'><a name=\"opt-item./1\">x</a></p>",
                "<a href=\"#opt-item./1\">ok anchor</a>",
                "<a href='#top'>ok id</a>",
                "<a href=\"#org1\">ok contained</a>",
                "<a href=\"#\">bare</a>",
                "<a href=\"http://example.org/#x\">external</a>",
                "<ul><li><a href=\"#Patient_x\">See <b>above</b> (Patient/x)</a></li></ul>",
                "</div>")}
        });
        let errors = narrative_link_errors(&patient);
        assert_eq!(errors.len(), 1, "{errors:#?}");
        assert_eq!(errors[0].kind, ErrorKind::Narrative);
        assert_eq!(errors[0].severity, Severity::Error);
        assert_eq!(errors[0].path, "Patient.text.div");
        assert_eq!(
            errors[0].message,
            "Hyperlink '#Patient_x' at 'div/ul/li/a' for 'See above (Patient/x)' does not resolve"
        );
    }

    #[test]
    fn a_bundle_entry_is_judged_on_its_own() {
        let div = |body: &str| format!("<div xmlns=\"http://www.w3.org/1999/xhtml\">{body}</div>");
        let bundle = json!({
            "resourceType": "Bundle", "type": "document",
            "entry": [
                {"resource": {"resourceType": "Composition", "id": "c",
                    "text": {"status": "generated", "div": div("<a name=\"Patient_1\"/>")}}},
                {"resource": {"resourceType": "Condition", "id": "d",
                    "text": {"status": "generated", "div": div("<p><a href=\"#Patient_1\">See above</a></p>")}}}
            ]
        });
        let errors = narrative_link_errors(&bundle);
        assert_eq!(errors.len(), 1, "{errors:#?}");
        assert_eq!(errors[0].path, "Bundle.entry.1.resource.text.div");
        assert!(errors[0].message.contains("at 'div/p/a'"), "{}", errors[0].message);
    }

    #[test]
    fn observations_without_performer_or_subject_warn() {
        let obs = json!({"resourceType": "Observation", "status": "final",
            "code": {"text": "x"}, "subject": {"reference": "Patient/p"}});
        let w = best_practice_warnings(&obs);
        assert_eq!(w.len(), 1, "{w:#?}");
        assert_eq!(w[0].severity, Severity::Warning);
        assert_eq!(w[0].path, "Observation");
        assert!(w[0].message.contains("should have a performer"));

        let bundle = json!({"resourceType": "Bundle", "type": "collection", "entry": [
            {"resource": {"resourceType": "Observation", "status": "final", "code": {"text": "x"},
                "performer": [{"reference": "Practitioner/x"}]}},
            {"resource": {"resourceType": "Patient"}}]});
        let w = best_practice_warnings(&bundle);
        assert_eq!(w.len(), 1, "{w:#?}");
        assert_eq!(w[0].path, "Bundle.entry.0.resource");
        assert!(w[0].message.contains("should have a subject"));
    }
}
