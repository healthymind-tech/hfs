//! Bundle-internal reference resolution and the HL7 validator's Bundle
//! rules.
//!
//! Resolution follows the FHIR rules for resolving references in Bundles
//! (R4 §2.36.4, as the HL7 Java validator implements them):
//!
//! - an absolute reference (`http://…`, `urn:uuid:…`) names the entry with
//!   that `fullUrl`;
//! - a relative reference (`Patient/1`) is resolved against the RESTful base
//!   of the *referencing* entry's `fullUrl` (`http://x/fhir/Composition/9` →
//!   `http://x/fhir/Patient/1`). An entry whose `fullUrl` is a URN has no
//!   base, so its relative references resolve nowhere — even when an entry
//!   of that type and id exists (the Java validator's
//!   `BUNDLE_BUNDLE_ENTRY_NOTFOUND_CANNOT`). An entry without a `fullUrl`
//!   matches by type and id.
//!
//! Rules checked over a whole Bundle ([`bundle_rules`]), each one an error
//! in the Java validator's default configuration:
//!
//! - an absolute `http(s)` `fullUrl` that contains `/{type}/` must end with
//!   `/{type}/{id}` (`BUNDLE_ENTRY_URL_MATCHES_TYPE_ID`);
//! - `document` Bundles: the Composition's references (subject, encounter,
//!   author, attester.party, custodian, event.detail, section author/focus/
//!   entry, recursively) must resolve inside the Bundle
//!   (`BUNDLE_BUNDLE_ENTRY_NOTFOUND`); every entry must be reachable from the
//!   Composition by following references forwards or backwards
//!   (`BUNDLE_BUNDLE_ENTRY_ORPHAN_DOCUMENT`);
//! - `document` and `message` Bundles: a relative reference anywhere in an
//!   entry that cannot resolve only because of the `fullUrl` rules above
//!   (`BUNDLE_BUNDLE_ENTRY_NOTFOUND_CANNOT`). A reference that names nothing
//!   in the Bundle at all is only informational there (it may point to a
//!   server), so it is not reported.

use super::errors::{ErrorKind, Severity, ValidationError};
use serde_json::Value;
use std::collections::HashSet;

/// One entry of a Bundle: its `fullUrl` and resource.
#[derive(Clone, Copy)]
pub(super) struct Entry<'a> {
    pub(super) full_url: Option<&'a str>,
    pub(super) resource: &'a Value,
}

impl<'a> Entry<'a> {
    fn resource_type(&self) -> Option<&'a str> {
        self.resource.get("resourceType").and_then(Value::as_str)
    }

    fn id(&self) -> Option<&'a str> {
        self.resource.get("id").and_then(Value::as_str)
    }
}

/// The entries of a Bundle that carry a resource, with their entry index.
pub(super) fn entries(bundle: &Value) -> Vec<(usize, Entry<'_>)> {
    bundle
        .get("entry")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .enumerate()
                .filter_map(|(index, e)| {
                    let resource = e.get("resource").filter(|r| r.is_object())?;
                    Some((
                        index,
                        Entry {
                            // A relative fullUrl identifies nothing (and is
                            // reported by [`bundle_rules`]): the entry
                            // resolves by type and id, like one without.
                            full_url: e
                                .get("fullUrl")
                                .and_then(Value::as_str)
                                .filter(|u| is_absolute(u)),
                            resource,
                        },
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether `url` is absolute: it starts with a scheme (`http:`, `urn:`).
fn is_absolute(url: &str) -> bool {
    url.split_once(':').is_some_and(|(scheme, _)| {
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

/// The outcome of resolving a reference inside a Bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Resolution {
    /// Resolved to the entry at this position (in the slice passed in).
    Found(usize),
    /// A relative reference whose type/id an entry has, but which the
    /// `fullUrl` rules keep from resolving to it.
    Cannot(usize),
    /// Nothing in the Bundle answers to the reference.
    NotFound,
}

/// The RESTful base of a `fullUrl` (`http://x/fhir/Patient/1` →
/// `http://x/fhir`), when it has one.
fn restful_base(full_url: &str) -> Option<&str> {
    if !(full_url.starts_with("http://") || full_url.starts_with("https://")) {
        return None;
    }
    let path = full_url.split(['?', '#']).next().unwrap_or(full_url);
    let path = match path.find("/_history/") {
        Some(i) => &path[..i],
        None => path,
    };
    let id_start = path.rfind('/')?;
    let type_start = path[..id_start].rfind('/')?;
    let ty = &path[type_start + 1..id_start];
    if !ty.starts_with(|c: char| c.is_ascii_uppercase()) || id_start + 1 >= path.len() {
        return None;
    }
    Some(&path[..type_start])
}

/// `Type/id` of a relative reference (history and fragments dropped).
fn relative_type_id(reference: &str) -> Option<(&str, &str)> {
    let path = reference.split(['?', '#']).next()?;
    let mut parts = path.split('/');
    let ty = parts.next()?;
    let id = parts.next()?;
    if ty.is_empty() || id.is_empty() || !ty.starts_with(|c: char| c.is_ascii_uppercase()) {
        return None;
    }
    match (parts.next(), parts.next(), parts.next()) {
        (None, ..) | (Some("_history"), Some(_), None) => Some((ty, id)),
        _ => None,
    }
}

fn strip_history(url: &str) -> &str {
    match url.find("/_history/") {
        Some(i) => &url[..i],
        None => url,
    }
}

/// Resolve `reference` among `entries`, from an entry whose `fullUrl` is
/// `source_full_url`. Contained (`#id`) and conditional references are not
/// Bundle references: [`Resolution::NotFound`].
pub(super) fn resolve(
    entries: &[Entry<'_>],
    reference: &str,
    source_full_url: Option<&str>,
) -> Resolution {
    if reference.starts_with('#') || reference.contains('?') {
        return Resolution::NotFound;
    }
    if reference.starts_with("urn:") || reference.contains("://") {
        let wanted = strip_history(reference);
        return entries
            .iter()
            .position(|e| e.full_url.is_some_and(|u| strip_history(u) == wanted))
            .map_or(Resolution::NotFound, Resolution::Found);
    }
    let Some((ty, id)) = relative_type_id(reference) else {
        return Resolution::NotFound;
    };
    let same_type_id = |e: &Entry<'_>| e.resource_type() == Some(ty) && e.id() == Some(id);
    if let Some(base) = source_full_url.and_then(restful_base) {
        let wanted = format!("{base}/{ty}/{id}");
        if let Some(found) = entries
            .iter()
            .position(|e| e.full_url.is_some_and(|u| strip_history(u) == wanted))
        {
            return Resolution::Found(found);
        }
    }
    // Entries without a fullUrl answer by type and id; a referencing entry
    // without a fullUrl resolves by type and id too.
    if let Some(found) = entries
        .iter()
        .position(|e| e.full_url.is_none() && same_type_id(e))
    {
        return Resolution::Found(found);
    }
    match entries.iter().position(same_type_id) {
        Some(found) if source_full_url.is_none() => Resolution::Found(found),
        Some(found) => Resolution::Cannot(found),
        None => Resolution::NotFound,
    }
}

fn msg_not_found(reference: &str, location: &str) -> String {
    format!("Can't find '{reference}' in the bundle ({location})")
}

fn msg_cannot(reference: &str, location: &str, found: &str, source: &str) -> String {
    format!(
        "Can't find '{reference}' in the bundle ({location}). Note that there is a resource in \
         the bundle with the same type and id, but it does not match because of the fullUrl \
         based rules around matching relative references. Found '{found}', but it doesn't \
         match because of the source fullUrl '{source}'"
    )
}

/// Every `Reference.reference` inside `value` (not descending into nested
/// resources' own Bundles), with its dotted path below `prefix`.
fn collect_references<'v>(value: &'v Value, path: &mut String, out: &mut Vec<(String, &'v str)>) {
    match value {
        Value::Object(map) => {
            if let Some(reference) = map.get("reference").and_then(Value::as_str) {
                out.push((path.clone(), reference));
            }
            for (key, child) in map {
                if child.get("resourceType").and_then(Value::as_str) == Some("Bundle") {
                    continue;
                }
                let len = path.len();
                path.push('.');
                path.push_str(key);
                collect_references(child, path, out);
                path.truncate(len);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                let len = path.len();
                path.push('.');
                path.push_str(&index.to_string());
                collect_references(item, path, out);
                path.truncate(len);
            }
        }
        _ => {}
    }
}

/// The Composition elements the Java validator resolves in a document.
const COMPOSITION_REFERENCES: &[&str] = &["subject", "encounter", "author", "custodian"];

/// `(dotted path, reference)` of the Composition references a document
/// must resolve.
fn composition_references(composition: &Value, prefix: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut push = |path: String, reference: Option<&Value>| {
        if let Some(r) = reference
            .and_then(|r| r.get("reference"))
            .and_then(Value::as_str)
        {
            out.push((path, r.to_string()));
        }
    };
    let each = |value: Option<&Value>| -> Vec<(Option<usize>, Value)> {
        match value {
            Some(Value::Array(items)) => items
                .iter()
                .enumerate()
                .map(|(i, v)| (Some(i), v.clone()))
                .collect(),
            Some(v) => vec![(None, v.clone())],
            None => Vec::new(),
        }
    };
    let at = |base: &str, key: &str, index: Option<usize>| match index {
        Some(i) => format!("{base}.{key}.{i}"),
        None => format!("{base}.{key}"),
    };
    for key in COMPOSITION_REFERENCES {
        for (i, v) in each(composition.get(*key)) {
            push(at(prefix, key, i), Some(&v));
        }
    }
    for (i, attester) in each(composition.get("attester")) {
        let base = at(prefix, "attester", i);
        push(format!("{base}.party"), attester.get("party"));
    }
    for (i, event) in each(composition.get("event")) {
        let base = at(prefix, "event", i);
        for (j, detail) in each(event.get("detail")) {
            push(at(&base, "detail", j), Some(&detail));
        }
    }
    fn sections(value: Option<&Value>, base: &str, out: &mut Vec<(String, String)>) {
        let Some(Value::Array(items)) = value else {
            return;
        };
        for (i, section) in items.iter().enumerate() {
            let here = format!("{base}.section.{i}");
            for key in ["author", "focus", "entry"] {
                let refs: Vec<(String, &Value)> = match section.get(key) {
                    Some(Value::Array(rs)) => rs
                        .iter()
                        .enumerate()
                        .map(|(j, r)| (format!("{here}.{key}.{j}"), r))
                        .collect(),
                    Some(r) => vec![(format!("{here}.{key}"), r)],
                    None => Vec::new(),
                };
                for (path, r) in refs {
                    if let Some(reference) = r.get("reference").and_then(Value::as_str) {
                        out.push((path, reference.to_string()));
                    }
                }
            }
            sections(section.get("section"), &here, out);
        }
    }
    sections(composition.get("section"), prefix, &mut out);
    out
}

fn error(kind: ErrorKind, path: String, message: String) -> ValidationError {
    ValidationError::new(kind, path, message).with_severity(Severity::Error)
}

/// The Bundle-level rules (see the module docs). Nested Bundles are judged
/// when the walk validates them on their own; only the root is checked here.
pub(super) fn bundle_rules(bundle: &Value) -> Vec<ValidationError> {
    bundle_rules_with(bundle, &|t: &str| {
        t.starts_with(|c: char| c.is_ascii_uppercase())
    })
}

/// [`bundle_rules`] with `is_resource_type` telling resource type names
/// (`Observation`) from other path segments (`Obervation`) in RESTful
/// fullUrls.
pub(super) fn bundle_rules_with(
    bundle: &Value,
    is_resource_type: &dyn Fn(&str) -> bool,
) -> Vec<ValidationError> {
    let mut out = Vec::new();
    if bundle.get("resourceType").and_then(Value::as_str) != Some("Bundle") {
        return out;
    }
    let indexed = entries(bundle);
    let list: Vec<Entry<'_>> = indexed.iter().map(|(_, e)| *e).collect();

    // fullUrl must be absolute (BUNDLE_BUNDLE_ENTRY_FULLURL_NOT_ABSOLUTE).
    for (index, entry) in bundle
        .get("entry")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        if let Some(full_url) = entry.get("fullUrl").and_then(Value::as_str)
            && !is_absolute(full_url)
        {
            out.push(error(
                ErrorKind::BundleEntryMismatch,
                format!("Bundle.entry.{index}.fullUrl"),
                format!("The fullUrl must be an absolute URL (not '{full_url}')"),
            ));
        }
    }

    // fullUrl vs type/id: an http(s) fullUrl ending in `/{Type}/{id}`
    // is a RESTful URL and must name the entry's own type and id.
    for (index, entry) in &indexed {
        let (Some(full_url), Some(ty), Some(id)) =
            (entry.full_url, entry.resource_type(), entry.id())
        else {
            continue;
        };
        if !(full_url.starts_with("http://") || full_url.starts_with("https://")) {
            continue;
        }
        let url = strip_history(full_url.split(['?', '#']).next().unwrap_or(full_url));
        let mut tail = url.rsplit('/');
        let (url_id, url_type) = (
            tail.next().unwrap_or_default(),
            tail.next().unwrap_or_default(),
        );
        let restful = url_type.starts_with(|c: char| c.is_ascii_uppercase())
            && url_type.chars().all(|c| c.is_ascii_alphabetic())
            && is_resource_type(url_type)
            && !url_id.is_empty();
        if restful && (url_type != ty || url_id != id) {
            out.push(error(
                ErrorKind::BundleEntryMismatch,
                format!("Bundle.entry.{index}"),
                format!(
                    "The fullUrl '{full_url}' looks like a RESTful server URL, so it must end with the correct type and id (/{ty}/{id})"
                ),
            ));
        }
    }

    let bundle_type = bundle.get("type").and_then(Value::as_str);
    // Every entry with a resource has a fullUrl, except in transactions
    // and batches (BUNDLE_BUNDLE_ENTRY_FULLURL_REQD).
    if bundle_type.is_some()
        && !matches!(
            bundle_type,
            Some("transaction" | "batch" | "transaction-response" | "batch-response")
        )
    {
        for (index, entry) in bundle
            .get("entry")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            if entry.get("resource").is_some() && entry.get("fullUrl").is_none() {
                out.push(error(
                    ErrorKind::Required,
                    format!("Bundle.entry.{index}"),
                    "Except for transactions and batches, each entry in a Bundle must have a fullUrl which is the identity of the resource in the entry".to_string(),
                ));
            }
        }
    }
    if bundle_type == Some("searchset") {
        searchset_rules(bundle, &mut out);
    }
    if !matches!(bundle_type, Some("document" | "message")) || list.is_empty() {
        return out;
    }

    // Forward links between entries, and fullUrl-rule failures anywhere.
    let mut links: Vec<HashSet<usize>> = vec![HashSet::new(); list.len()];
    for (pos, (index, entry)) in indexed.iter().enumerate() {
        let mut refs = Vec::new();
        let mut path = format!("Bundle.entry.{index}.resource");
        collect_references(entry.resource, &mut path, &mut refs);
        for (ref_path, reference) in refs {
            match resolve(&list, reference, entry.full_url) {
                Resolution::Found(target) => {
                    if target != pos {
                        links[pos].insert(target);
                    }
                }
                // A document's Composition reports its own references
                // below (once, at the reference).
                Resolution::Cannot(_) if pos == 0 && bundle_type == Some("document") => {}
                Resolution::Cannot(target) => out.push(error(
                    ErrorKind::ReferenceUnresolved,
                    "Bundle".to_string(),
                    msg_cannot(
                        reference,
                        &ref_path,
                        list[target].full_url.unwrap_or_default(),
                        entry.full_url.unwrap_or_default(),
                    ),
                )),
                Resolution::NotFound => {}
            }
        }
    }

    if bundle_type != Some("document") {
        return out;
    }
    let (first_index, first) = indexed[0];
    if first.resource_type() != Some("Composition") {
        return out;
    }

    // The Composition's own references must resolve.
    let prefix = format!("Bundle.entry.{first_index}.resource");
    for (path, reference) in composition_references(first.resource, &prefix) {
        if reference.starts_with('#') {
            continue;
        }
        let message = match resolve(&list, &reference, first.full_url) {
            Resolution::Found(_) => continue,
            Resolution::Cannot(target) => msg_cannot(
                &reference,
                "Composition",
                list[target].full_url.unwrap_or_default(),
                first.full_url.unwrap_or_default(),
            ),
            Resolution::NotFound => msg_not_found(&reference, "Composition"),
        };
        out.push(error(ErrorKind::ReferenceUnresolved, path, message));
    }

    // Orphans: entries not reachable from the Composition following links
    // in either direction.
    let mut reached: HashSet<usize> = HashSet::from([0]);
    let mut changed = true;
    while changed {
        changed = false;
        for (from, targets) in links.iter().enumerate() {
            for &to in targets {
                let (a, b) = (reached.contains(&from), reached.contains(&to));
                if a != b {
                    reached.insert(from);
                    reached.insert(to);
                    changed = true;
                }
            }
        }
    }
    for (pos, (index, entry)) in indexed.iter().enumerate() {
        if !reached.contains(&pos) {
            out.push(error(
                ErrorKind::BundleEntryMismatch,
                format!("Bundle.entry.{index}"),
                format!(
                    "Entry '{}' isn't reachable by traversing links (forward or backward) from the Composition",
                    entry.full_url.unwrap_or_default()
                ),
            ));
        }
    }
    out
}

/// The resource types a search's `self` link asks for: the type in the
/// URL path (`…/Patient?name=x`) or the `_type` parameter. `None` when the
/// link does not say.
fn searched_types(self_link: &str) -> Option<Vec<String>> {
    let (path, query) = self_link.split_once('?').unwrap_or((self_link, ""));
    let last = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    if last.starts_with(|c: char| c.is_ascii_uppercase())
        && last.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return Some(vec![last.to_string()]);
    }
    query.split('&').find_map(|p| {
        let types = p.strip_prefix("_type=")?;
        Some(types.split(',').map(str::to_string).collect())
    })
}

/// `searchset` Bundles (HL7 validator): a link relation occurs once;
/// `match` entries carry an identified resource of a searched type;
/// `outcome` entries carry an OperationOutcome.
fn searchset_rules(bundle: &Value, out: &mut Vec<ValidationError>) {
    let links = bundle.get("link").and_then(Value::as_array);
    let mut seen = HashSet::new();
    let mut self_link = None;
    for (index, link) in links.into_iter().flatten().enumerate() {
        let Some(relation) = link.get("relation").and_then(Value::as_str) else {
            continue;
        };
        if relation == "self" && self_link.is_none() {
            self_link = link.get("url").and_then(Value::as_str);
        }
        if !seen.insert(relation) {
            out.push(error(
                ErrorKind::BundleEntryMismatch,
                format!("Bundle.link.{index}"),
                format!("The link relationship type '{relation}' can only occur once"),
            ));
        }
    }
    let searched = self_link.and_then(searched_types);
    for (index, entry) in bundle
        .get("entry")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let Some(resource) = entry.get("resource") else {
            continue;
        };
        let rt = resource
            .get("resourceType")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let path = format!("Bundle.entry.{index}.resource");
        match entry.pointer("/search/mode").and_then(Value::as_str) {
            Some("match") => {
                if resource.get("id").is_none() {
                    out.push(error(
                        ErrorKind::Required,
                        path.clone(),
                        "Search results must have ids".to_string(),
                    ));
                }
                if let Some(types) = &searched
                    && !types.iter().any(|t| t == rt)
                {
                    out.push(error(
                        ErrorKind::BundleEntryMismatch,
                        path,
                        format!(
                            "This is not a matching resource type for the specified search ({rt} expecting [{}])",
                            types.join(", ")
                        ),
                    ));
                }
            }
            Some("outcome") if rt != "OperationOutcome" => out.push(error(
                ErrorKind::BundleEntryMismatch,
                path,
                format!("This is not an OperationOutcome ({rt})"),
            )),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry<'a>(full_url: Option<&'a str>, resource: &'a Value) -> Entry<'a> {
        Entry { full_url, resource }
    }

    #[test]
    fn resolution_rules() {
        let patient = json!({"resourceType": "Patient", "id": "p1"});
        let obs = json!({"resourceType": "Observation", "id": "o1"});
        let urn = [
            entry(Some("urn:uuid:aaaa"), &patient),
            entry(Some("urn:uuid:bbbb"), &obs),
        ];
        assert_eq!(
            resolve(&urn, "urn:uuid:aaaa", Some("urn:uuid:bbbb")),
            Resolution::Found(0)
        );
        assert_eq!(
            resolve(&urn, "urn:uuid:cccc", Some("urn:uuid:bbbb")),
            Resolution::NotFound
        );
        // A relative reference from a URN entry cannot resolve.
        assert_eq!(
            resolve(&urn, "Patient/p1", Some("urn:uuid:bbbb")),
            Resolution::Cannot(0)
        );
        assert_eq!(
            resolve(&urn, "Patient/p2", Some("urn:uuid:bbbb")),
            Resolution::NotFound
        );

        let rest = [
            entry(Some("http://x/fhir/Patient/p1"), &patient),
            entry(Some("http://x/fhir/Observation/o1"), &obs),
        ];
        assert_eq!(
            resolve(&rest, "Patient/p1", Some("http://x/fhir/Observation/o1")),
            Resolution::Found(0)
        );
        assert_eq!(
            resolve(
                &rest,
                "Patient/p1/_history/2",
                Some("http://x/fhir/Observation/o1")
            ),
            Resolution::Found(0)
        );
        assert_eq!(
            resolve(
                &rest,
                "http://x/fhir/Patient/p1",
                Some("http://y/Observation/o1")
            ),
            Resolution::Found(0)
        );
        // Different base: same type/id but no match.
        assert_eq!(
            resolve(&rest, "Patient/p1", Some("http://y/fhir/Observation/o1")),
            Resolution::Cannot(0)
        );

        let bare = [entry(None, &patient), entry(None, &obs)];
        assert_eq!(resolve(&bare, "Patient/p1", None), Resolution::Found(0));
        assert_eq!(resolve(&bare, "#x", None), Resolution::NotFound);
    }

    #[test]
    fn searchset_rules_match_the_hl7_validator() {
        let messages =
            |b: Value| -> Vec<String> { bundle_rules(&b).into_iter().map(|e| e.message).collect() };
        let link = |rel: &str, url: &str| json!({"relation": rel, "url": url});
        let b = json!({"resourceType": "Bundle", "type": "searchset",
            "link": [link("self", "base/Patient?name=x"), link("self", "base/Patient?name=x"),
                     link("first", "base/Patient?name=x")],
            "entry": [{"fullUrl": "http://x/fhir/OperationOutcome/o", "search": {"mode": "outcome"},
                       "resource": {"resourceType": "OperationOutcome"}},
                      {"fullUrl": "http://x/fhir/Patient/p", "search": {"mode": "match"},
                       "resource": {"resourceType": "Patient", "id": "p"}}]});
        assert_eq!(
            messages(b),
            vec!["The link relationship type 'self' can only occur once"]
        );
        let b = json!({"resourceType": "Bundle", "type": "searchset",
            "link": [link("self", "base?name=x&_type=Observation,DocumentReference")],
            "entry": [{"fullUrl": "urn:uuid:1", "search": {"mode": "outcome"},
                       "resource": {"resourceType": "Immunization"}},
                      {"fullUrl": "urn:uuid:2", "search": {"mode": "match"},
                       "resource": {"resourceType": "OperationOutcome"}}]});
        assert_eq!(
            messages(b),
            vec![
                "This is not an OperationOutcome (Immunization)",
                "Search results must have ids",
                "This is not a matching resource type for the specified search (OperationOutcome expecting [Observation, DocumentReference])",
            ]
        );
    }

    #[test]
    fn fullurls_are_required_and_restful_ones_must_name_the_entry() {
        let messages =
            |b: Value| -> Vec<String> { bundle_rules(&b).into_iter().map(|e| e.message).collect() };
        let b = json!({"resourceType": "Bundle", "type": "collection", "entry": [
            {"resource": {"resourceType": "Patient", "id": "p"}},
            {"fullUrl": "http://acme.com/Patient/pat1",
             "resource": {"resourceType": "DocumentManifest", "id": "pat1"}},
            {"fullUrl": "http://acme.com/fhir/DocumentManifest/m",
             "resource": {"resourceType": "DocumentManifest", "id": "m"}}]});
        let got = messages(b);
        assert_eq!(got.len(), 2, "{got:#?}");
        assert!(
            got.iter()
                .any(|m| m.starts_with("Except for transactions and batches"))
        );
        assert!(
            got.iter()
                .any(|m| m
                    .contains("must end with the correct type and id (/DocumentManifest/pat1)"))
        );
        let tx = json!({"resourceType": "Bundle", "type": "transaction", "entry": [
            {"resource": {"resourceType": "Patient"}, "request": {"method": "POST", "url": "Patient"}}]});
        assert!(messages(tx).is_empty());
    }

    #[test]
    fn relative_fullurls_are_errors_and_resolve_by_type_and_id() {
        let bundle = json!({"resourceType": "Bundle", "type": "document", "entry": [
            {"fullUrl": "Composition/1", "resource": {"resourceType": "Composition", "id": "1",
                "subject": {"reference": "Patient/1"}}},
            {"fullUrl": "Patient/1", "resource": {"resourceType": "Patient", "id": "1"}},
            {"fullUrl": "urn:uuid:3", "resource": {"resourceType": "Patient", "id": "3",
                "link": [{"other": {"reference": "Patient/1"}}]}}
        ]});
        let messages: Vec<String> = bundle_rules(&bundle)
            .into_iter()
            .map(|e| e.message)
            .collect();
        assert_eq!(
            messages,
            vec![
                "The fullUrl must be an absolute URL (not 'Composition/1')".to_string(),
                "The fullUrl must be an absolute URL (not 'Patient/1')".to_string(),
            ],
        );
        assert!(is_absolute("urn:uuid:1") && is_absolute("http://x/Patient/1"));
        assert!(!is_absolute("Patient/1") && !is_absolute("1a19a371-91b8") && !is_absolute(":x"));
    }

    #[test]
    fn restful_fullurl_must_end_with_type_and_id() {
        let bundle = json!({"resourceType": "Bundle", "type": "collection", "entry": [
            {"fullUrl": "http://h/fhir/Endpoint/Endpoint-71", "resource": {"resourceType": "Endpoint", "id": "71"}},
            {"fullUrl": "http://h/fhir/Endpoint/72", "resource": {"resourceType": "Endpoint", "id": "72"}},
            {"fullUrl": "urn:uuid:1", "resource": {"resourceType": "Endpoint", "id": "73"}}
        ]});
        let issues = bundle_rules(&bundle);
        assert_eq!(issues.len(), 1, "{issues:#?}");
        assert_eq!(issues[0].path, "Bundle.entry.0");
    }

    #[test]
    fn document_rules() {
        let bundle = json!({"resourceType": "Bundle", "type": "document", "entry": [
            {"fullUrl": "urn:uuid:c", "resource": {"resourceType": "Composition", "id": "c",
                "subject": {"reference": "Patient/p"},
                "author": [{"reference": "urn:uuid:a"}],
                "section": [{"entry": [{"reference": "urn:uuid:o"}]}]}},
            {"fullUrl": "urn:uuid:p", "resource": {"resourceType": "Patient", "id": "p"}},
            {"fullUrl": "urn:uuid:a", "resource": {"resourceType": "Practitioner", "id": "a"}},
            {"fullUrl": "urn:uuid:o", "resource": {"resourceType": "Observation", "id": "o",
                "performer": [{"reference": "urn:uuid:x"}]}},
            {"fullUrl": "urn:uuid:x", "resource": {"resourceType": "Organization", "id": "x"}},
            {"fullUrl": "urn:uuid:z", "resource": {"resourceType": "Organization", "id": "z"}}
        ]});
        let issues = bundle_rules(&bundle);
        let texts: Vec<String> = issues
            .iter()
            .map(|e| format!("{} {}", e.path, e.message))
            .collect();
        // subject: reported once, at the Composition's reference.
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with("Bundle.entry.0.resource.subject Can't find 'Patient/p'")),
            "{texts:#?}"
        );
        // Patient unreachable (its only link failed) and z orphaned; x is
        // reached through the Observation.
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with("Bundle.entry.1 Entry 'urn:uuid:p'")),
            "{texts:#?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with("Bundle.entry.5 Entry 'urn:uuid:z'")),
            "{texts:#?}"
        );
        assert_eq!(issues.len(), 3, "{texts:#?}");
    }
}
