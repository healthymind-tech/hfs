//! Resource access of a realm user, as one flat list.
//!
//! The store keeps a user's access as [`FhirLink`]s (one row per linked
//! resource, the `fhirUser` row carrying the accessible-patient ids). The
//! console and the bulk admin API edit it as a flat list of
//! [`AccessEntry`] — `(endpoint, reference, kind)` — which converts both
//! ways without changing what the token and consent code reads
//! ([`crate::user::RealmUser::fhir_user`],
//! [`crate::user::RealmUser::selectable_patients`]):
//!
//! | entry kind | stored as |
//! |---|---|
//! | `fhirUser` | the endpoint's link with `isFhirUser` |
//! | `patient` | an id in the `fhirUser` link's `accessiblePatients`, or (no `fhirUser` link on that endpoint) a plain `Patient/…` link |
//! | `link` | a plain link (another `Practitioner`, `PractitionerRole`, …) |
//!
//! It also parses and writes the portable formats the console copies and
//! pastes and the API accepts: one reference per line, CSV
//! (`endpoint,reference,kind`) and JSON (the links array, or entries).

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::user::FhirLink;

/// At most this many references in one paste, import or API call.
pub const MAX_IMPORT: usize = 10_000;
/// At most this many entries per user.
pub const MAX_PER_USER: usize = 20_000;
/// At most this many bytes of pasted or uploaded text (10k rows of JSON
/// links fit easily).
pub const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;

/// Resource types a user may be linked to.
pub const LINK_TYPES: &[&str] = &[
    "Patient",
    "Practitioner",
    "PractitionerRole",
    "RelatedPerson",
    "Person",
];

/// What a granted reference means for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AccessKind {
    /// The user *is* this resource on the endpoint (the `fhirUser` claim).
    #[serde(rename = "fhirUser")]
    FhirUser,
    /// A patient the user may select (proxy / caregiver access).
    #[serde(rename = "patient")]
    Patient,
    /// Another linked resource (not the identity, not a patient).
    #[serde(rename = "link")]
    Link,
}

impl AccessKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AccessKind::FhirUser => "fhirUser",
            AccessKind::Patient => "patient",
            AccessKind::Link => "link",
        }
    }

    /// Parses `fhirUser` / `patient` / `link` (and a few spellings people
    /// type: `fhir-user`, `identity`, `accessible-patient`, …).
    pub fn parse(s: &str) -> Option<Self> {
        let k: String = s
            .trim()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        match k.as_str() {
            "fhiruser" | "identity" | "self" | "isfhiruser" => Some(AccessKind::FhirUser),
            "patient" | "accessiblepatient" | "accessiblepatients" => Some(AccessKind::Patient),
            "link" | "linked" | "other" => Some(AccessKind::Link),
            _ => None,
        }
    }

    /// The kind a reference gets when none is given.
    pub fn default_for(reference: &str) -> Self {
        if reference.starts_with("Patient/") {
            AccessKind::Patient
        } else {
            AccessKind::Link
        }
    }
}

/// One granted reference.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessEntry {
    pub endpoint: String,
    /// `Patient/123`.
    pub reference: String,
    pub kind: AccessKind,
}

impl AccessEntry {
    pub fn new(endpoint: &str, reference: &str, kind: AccessKind) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            reference: reference.to_string(),
            kind,
        }
    }

    /// `(endpoint, reference)`: the identity of an entry.
    pub fn key(&self) -> (String, String) {
        (self.endpoint.clone(), self.reference.clone())
    }

    /// The resource type (`Patient`).
    pub fn resource_type(&self) -> &str {
        self.reference.split('/').next().unwrap_or_default()
    }

    /// The resource id.
    pub fn id(&self) -> &str {
        self.reference
            .split_once('/')
            .map(|x| x.1)
            .unwrap_or_default()
    }
}

/// Whether `id` is a FHIR id (`[A-Za-z0-9\-\.]{1,64}`).
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// Whether `name` could be an endpoint name (`[a-z][a-z0-9-]*`).
fn endpoint_like(name: &str) -> bool {
    let mut cs = name.chars();
    matches!(cs.next(), Some(c) if c.is_ascii_lowercase())
        && name.len() <= 63
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Why an input row was not accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "code")]
pub enum RowIssue {
    /// Not `Type/id` (or a URL ending in it).
    BadReference,
    /// A resource type users cannot be linked to.
    UnsupportedType { resource_type: String },
    /// An unknown kind word.
    BadKind { kind: String },
    /// `patient` on a non-Patient reference.
    KindMismatch,
    /// No endpoint given and no default.
    NoEndpoint,
    /// The endpoint is not bound to the realm.
    UnknownEndpoint { endpoint: String },
}

impl RowIssue {
    /// Stable code (`bad-reference`, …) for i18n keys and the API.
    pub fn code(&self) -> &'static str {
        match self {
            RowIssue::BadReference => "bad-reference",
            RowIssue::UnsupportedType { .. } => "unsupported-type",
            RowIssue::BadKind { .. } => "bad-kind",
            RowIssue::KindMismatch => "kind-mismatch",
            RowIssue::NoEndpoint => "no-endpoint",
            RowIssue::UnknownEndpoint { .. } => "unknown-endpoint",
        }
    }

    /// English message (API errors).
    pub fn message(&self) -> String {
        match self {
            RowIssue::BadReference => "not a reference (Type/id)".into(),
            RowIssue::UnsupportedType { resource_type } => format!(
                "{resource_type} cannot be linked (use {})",
                LINK_TYPES.join(", ")
            ),
            RowIssue::BadKind { kind } => {
                format!("unknown kind {kind:?} (fhirUser, patient or link)")
            }
            RowIssue::KindMismatch => "kind patient needs a Patient reference".into(),
            RowIssue::NoEndpoint => "no endpoint given".into(),
            RowIssue::UnknownEndpoint { endpoint } => {
                format!("endpoint {endpoint:?} is not bound to this realm")
            }
        }
    }
}

/// Normalizes a reference: `Patient/1`, `Patient/1/_history/2`, or an
/// absolute URL ending in either. `Ok((type, id))`.
pub fn parse_reference(raw: &str) -> Result<(String, String), RowIssue> {
    let s = raw
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '<' || c == '>');
    let path = match s.find("://") {
        Some(i) => {
            let rest = &s[i + 3..];
            rest.find('/').map(|j| &rest[j..]).unwrap_or("")
        }
        None => s,
    };
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let mut segs: Vec<&str> = path.split('/').filter(|x| !x.is_empty()).collect();
    if segs.len() >= 4 && segs[segs.len() - 2] == "_history" {
        segs.truncate(segs.len() - 2);
    }
    if segs.len() < 2 {
        return Err(RowIssue::BadReference);
    }
    let (t, id) = (segs[segs.len() - 2], segs[segs.len() - 1]);
    if !(t.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && t.chars().all(|c| c.is_ascii_alphabetic()))
        || !valid_id(id)
    {
        return Err(RowIssue::BadReference);
    }
    if !LINK_TYPES.contains(&t) {
        return Err(RowIssue::UnsupportedType {
            resource_type: t.to_string(),
        });
    }
    Ok((t.to_string(), id.to_string()))
}

/// Flattens stored links into entries: one per `(endpoint, reference)`,
/// in a stable order (endpoint, then kind, then reference).
pub fn flatten(links: &[FhirLink]) -> Vec<AccessEntry> {
    let mut by_key: HashMap<(String, String), AccessKind> = HashMap::new();
    let mut put = |ep: &str, r: String, k: AccessKind| {
        let e = by_key.entry((ep.to_string(), r)).or_insert(k);
        // The identity wins over any other spelling of the same resource.
        if k < *e {
            *e = k;
        }
    };
    for l in links {
        let kind = if l.is_fhir_user {
            AccessKind::FhirUser
        } else {
            AccessKind::default_for(&l.reference)
        };
        put(&l.endpoint, l.reference.clone(), kind);
        for p in &l.accessible_patients {
            put(&l.endpoint, format!("Patient/{p}"), AccessKind::Patient);
        }
    }
    let mut out: Vec<AccessEntry> = by_key
        .into_iter()
        .map(|((endpoint, reference), kind)| AccessEntry {
            endpoint,
            reference,
            kind,
        })
        .collect();
    sort(&mut out);
    out
}

/// The canonical order: endpoint, kind (identity first), reference.
pub fn sort(entries: &mut [AccessEntry]) {
    entries.sort_by(|a, b| {
        (a.endpoint.as_str(), a.kind, a.reference.as_str()).cmp(&(
            b.endpoint.as_str(),
            b.kind,
            b.reference.as_str(),
        ))
    });
}

/// Turns entries back into links (the inverse of [`flatten`] up to order).
/// Entries must be unique by key with at most one `fhirUser` per endpoint
/// ([`check`]).
pub fn to_links(entries: &[AccessEntry]) -> Vec<FhirLink> {
    let mut eps: BTreeMap<&str, Vec<&AccessEntry>> = BTreeMap::new();
    for e in entries {
        eps.entry(e.endpoint.as_str()).or_default().push(e);
    }
    let mut out = vec![];
    for (ep, list) in eps {
        let identity = list.iter().find(|e| e.kind == AccessKind::FhirUser);
        let mut patients: Vec<String> = list
            .iter()
            .filter(|e| e.kind == AccessKind::Patient)
            .filter(|e| identity.is_none_or(|i| i.reference != e.reference))
            .map(|e| e.id().to_string())
            .collect();
        patients.sort();
        if let Some(i) = identity {
            out.push(FhirLink {
                endpoint: ep.to_string(),
                reference: i.reference.clone(),
                is_fhir_user: true,
                accessible_patients: patients,
            });
        } else {
            for p in patients {
                out.push(FhirLink {
                    endpoint: ep.to_string(),
                    reference: format!("Patient/{p}"),
                    is_fhir_user: false,
                    accessible_patients: vec![],
                });
            }
        }
        let mut others: Vec<&&AccessEntry> =
            list.iter().filter(|e| e.kind == AccessKind::Link).collect();
        others.sort_by(|a, b| a.reference.cmp(&b.reference));
        for e in others {
            out.push(FhirLink {
                endpoint: ep.to_string(),
                reference: e.reference.clone(),
                is_fhir_user: false,
                accessible_patients: vec![],
            });
        }
    }
    out
}

/// Why a whole list was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListError {
    /// More than [`MAX_PER_USER`] entries.
    TooMany(usize),
    /// Two identities on one endpoint.
    TwoIdentities { endpoint: String },
    /// The same reference twice.
    Duplicate { endpoint: String, reference: String },
    /// A bad row.
    Row { reference: String, issue: RowIssue },
}

impl std::fmt::Display for ListError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ListError::TooMany(n) => write!(f, "{n} entries (at most {MAX_PER_USER} per user)"),
            ListError::TwoIdentities { endpoint } => {
                write!(f, "more than one fhirUser on endpoint {endpoint}")
            }
            ListError::Duplicate {
                endpoint,
                reference,
            } => write!(f, "{endpoint} {reference} is listed twice"),
            ListError::Row { reference, issue } => {
                write!(f, "{reference}: {}", issue.message())
            }
        }
    }
}

/// Validates a complete list.
pub fn check(entries: &[AccessEntry]) -> Result<(), ListError> {
    if entries.len() > MAX_PER_USER {
        return Err(ListError::TooMany(entries.len()));
    }
    let mut keys = HashSet::new();
    let mut ids = HashSet::new();
    for e in entries {
        match parse_reference(&e.reference) {
            Ok((t, id)) if format!("{t}/{id}") == e.reference => {}
            Ok(_) => {
                return Err(ListError::Row {
                    reference: e.reference.clone(),
                    issue: RowIssue::BadReference,
                });
            }
            Err(issue) => {
                return Err(ListError::Row {
                    reference: e.reference.clone(),
                    issue,
                });
            }
        }
        if e.kind == AccessKind::Patient && !e.reference.starts_with("Patient/") {
            return Err(ListError::Row {
                reference: e.reference.clone(),
                issue: RowIssue::KindMismatch,
            });
        }
        if !keys.insert((e.endpoint.as_str(), e.reference.as_str())) {
            return Err(ListError::Duplicate {
                endpoint: e.endpoint.clone(),
                reference: e.reference.clone(),
            });
        }
        if e.kind == AccessKind::FhirUser && !ids.insert(e.endpoint.as_str()) {
            return Err(ListError::TwoIdentities {
                endpoint: e.endpoint.clone(),
            });
        }
    }
    Ok(())
}

/// A short fingerprint of a list (order-independent): the console's and
/// the API's optimistic-concurrency token (`ETag`).
pub fn fingerprint(entries: &[AccessEntry]) -> String {
    let mut v: Vec<String> = entries
        .iter()
        .map(|e| {
            format!(
                "{}\u{1f}{}\u{1f}{}",
                e.endpoint,
                e.reference,
                e.kind.as_str()
            )
        })
        .collect();
    v.sort();
    let mut h = Sha256::new();
    for s in &v {
        h.update(s.as_bytes());
        h.update(b"\n");
    }
    let d = h.finalize();
    d.iter().take(10).map(|b| format!("{b:02x}")).collect()
}

// ------------------------------------------------------------------ formats

/// A portable text format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// One reference per line (`Patient/123`; `endpoint/Patient/123` when
    /// the list spans endpoints).
    Lines,
    /// `endpoint,reference,kind` with a header row.
    Csv,
    /// The links array (as stored, and as `GET …/users/{id}` shows it).
    Json,
}

impl Format {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "lines" | "text" | "txt" | "plain" | "references" => Some(Format::Lines),
            "csv" => Some(Format::Csv),
            "json" => Some(Format::Json),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Lines => "lines",
            Format::Csv => "csv",
            Format::Json => "json",
        }
    }
    pub fn media_type(self) -> &'static str {
        match self {
            Format::Lines => "text/plain; charset=utf-8",
            Format::Csv => "text/csv; charset=utf-8",
            Format::Json => "application/json",
        }
    }
    pub fn extension(self) -> &'static str {
        match self {
            Format::Lines => "txt",
            Format::Csv => "csv",
            Format::Json => "json",
        }
    }
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Writes `entries` in `format`.
pub fn export(entries: &[AccessEntry], format: Format) -> String {
    match format {
        Format::Lines => {
            let one = entries
                .first()
                .map(|f| entries.iter().all(|e| e.endpoint == f.endpoint))
                .unwrap_or(true);
            let mut out = String::new();
            for e in entries {
                if !one {
                    out.push_str(&e.endpoint);
                    out.push('/');
                }
                out.push_str(&e.reference);
                out.push('\n');
            }
            out
        }
        Format::Csv => {
            let mut out = String::from("endpoint,reference,kind\n");
            for e in entries {
                out.push_str(&format!(
                    "{},{},{}\n",
                    csv_field(&e.endpoint),
                    csv_field(&e.reference),
                    e.kind.as_str()
                ));
            }
            out
        }
        Format::Json => {
            serde_json::to_string_pretty(&to_links(entries)).unwrap_or_else(|_| "[]".into())
        }
    }
}

/// One row of pasted or uploaded input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InputRow {
    /// 1-based line (text formats) or item index (JSON).
    pub line: usize,
    /// What the row said (for the preview).
    pub raw: String,
    /// Endpoint as given (empty = the default).
    pub endpoint: String,
    /// Normalized `Type/id`, or the raw text when it did not parse.
    pub reference: String,
    /// Kind as given.
    pub kind: Option<AccessKind>,
    pub issue: Option<RowIssue>,
}

/// Why a whole input was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputError {
    /// Larger than [`MAX_TEXT_BYTES`].
    TooLarge(usize),
    /// More than [`MAX_IMPORT`] rows.
    TooMany(usize),
    /// JSON that does not parse or has an unknown shape.
    BadJson(String),
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputError::TooLarge(n) => {
                write!(f, "{n} bytes (at most {MAX_TEXT_BYTES})")
            }
            InputError::TooMany(n) => write!(f, "{n} references (at most {MAX_IMPORT} at once)"),
            InputError::BadJson(m) => write!(f, "JSON: {m}"),
        }
    }
}

/// Parsed input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub format: Format,
    pub rows: Vec<InputRow>,
}

fn row(line: usize, raw: &str, endpoint: &str, reference: &str, kind: Option<&str>) -> InputRow {
    let mut r = InputRow {
        line,
        raw: raw.chars().take(200).collect(),
        endpoint: endpoint.trim().to_string(),
        reference: reference.trim().chars().take(200).collect(),
        kind: None,
        issue: None,
    };
    // `endpoint/Type/id` in a bare reference.
    if r.endpoint.is_empty() && !r.reference.contains("://") {
        let segs: Vec<&str> = r.reference.split('/').collect();
        if segs.len() == 3 && endpoint_like(segs[0]) {
            r.endpoint = segs[0].to_string();
            r.reference = format!("{}/{}", segs[1], segs[2]);
        }
    }
    match parse_reference(&r.reference) {
        Ok((t, id)) => r.reference = format!("{t}/{id}"),
        Err(e) => r.issue = Some(e),
    }
    if let Some(k) = kind.map(str::trim).filter(|k| !k.is_empty()) {
        match AccessKind::parse(k) {
            Some(k) => r.kind = Some(k),
            None => {
                r.issue.get_or_insert(RowIssue::BadKind {
                    kind: k.chars().take(40).collect(),
                });
            }
        }
    }
    if r.issue.is_none()
        && r.kind == Some(AccessKind::Patient)
        && !r.reference.starts_with("Patient/")
    {
        r.issue = Some(RowIssue::KindMismatch);
    }
    r
}

/// Splits one CSV line (RFC 4180 quoting; `,` `;` or tab).
fn csv_split(line: &str, sep: char) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            c if c == sep && !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().to_string()).collect()
}

/// Parses text in any [`Format`] (detected): JSON when it starts with `[`
/// or `{`; CSV when a line has a `,` `;` or tab; else one reference per
/// line. Blank lines and `#` comments are skipped.
pub fn parse(text: &str) -> Result<Parsed, InputError> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(InputError::TooLarge(text.len()));
    }
    let text = text.trim_start_matches('\u{feff}');
    let t = text.trim_start();
    let parsed = if t.starts_with('[') || t.starts_with('{') {
        Parsed {
            format: Format::Json,
            rows: parse_json(t)?,
        }
    } else {
        let lines: Vec<(usize, &str)> = text
            .lines()
            .enumerate()
            .map(|(i, l)| (i + 1, l.trim()))
            .filter(|(_, l)| !l.is_empty() && !l.starts_with('#'))
            .collect();
        let sep = [',', '\t', ';']
            .into_iter()
            .find(|s| lines.iter().any(|(_, l)| l.contains(*s)));
        match sep {
            Some(sep) => Parsed {
                format: Format::Csv,
                rows: parse_csv(&lines, sep),
            },
            None => Parsed {
                format: Format::Lines,
                rows: lines.iter().map(|(n, l)| row(*n, l, "", l, None)).collect(),
            },
        }
    };
    if parsed.rows.len() > MAX_IMPORT {
        return Err(InputError::TooMany(parsed.rows.len()));
    }
    Ok(parsed)
}

fn parse_csv(lines: &[(usize, &str)], sep: char) -> Vec<InputRow> {
    let Some((_, first)) = lines.first() else {
        return vec![];
    };
    let head: Vec<String> = csv_split(first, sep)
        .into_iter()
        .map(|h| h.to_ascii_lowercase())
        .collect();
    let col = |names: &[&str]| head.iter().position(|h| names.contains(&h.as_str()));
    let header = col(&["reference", "ref", "resource"]);
    let (ep_col, ref_col, kind_col, skip) = match header {
        Some(r) => (
            col(&["endpoint", "ep"]),
            r,
            col(&["kind", "role", "type"]).filter(|k| *k != r),
            1,
        ),
        None => (None, 0, None, 0),
    };
    let mut out = vec![];
    for (n, l) in lines.iter().skip(skip) {
        let f = csv_split(l, sep);
        let get = |i: Option<usize>| i.and_then(|i| f.get(i)).map(String::as_str);
        if header.is_some() {
            out.push(row(
                *n,
                l,
                get(ep_col).unwrap_or(""),
                get(Some(ref_col)).unwrap_or(""),
                get(kind_col),
            ));
            continue;
        }
        // No header: `reference`, `reference,kind`, `endpoint,reference`
        // or `endpoint,reference,kind`, told apart by which field parses
        // as a reference.
        let is_ref = |s: &str| s.contains('/');
        let r = match f.len() {
            0 | 1 => row(*n, l, "", f.first().map(String::as_str).unwrap_or(""), None),
            2 if is_ref(&f[0]) => row(*n, l, "", &f[0], Some(&f[1])),
            2 => row(*n, l, &f[0], &f[1], None),
            _ if is_ref(&f[0]) => row(*n, l, "", &f[0], Some(&f[1])),
            _ => row(*n, l, &f[0], &f[1], Some(&f[2])),
        };
        out.push(r);
    }
    out
}

fn parse_json(t: &str) -> Result<Vec<InputRow>, InputError> {
    let v: Value = serde_json::from_str(t).map_err(|e| InputError::BadJson(e.to_string()))?;
    let items: Vec<Value> = match v {
        Value::Array(a) => a,
        Value::Object(ref o) => {
            if let Some(a) = ["links", "entries", "references"]
                .iter()
                .find_map(|k| o.get(*k).and_then(Value::as_array))
            {
                a.clone()
            } else if o.get("resourceType").and_then(Value::as_str) == Some("Bundle") {
                // A FHIR search result: its resources.
                o.get("entry")
                    .and_then(Value::as_array)
                    .map(|es| {
                        es.iter()
                            .filter_map(|e| {
                                let r = e.get("resource")?;
                                Some(Value::String(format!(
                                    "{}/{}",
                                    r.get("resourceType")?.as_str()?,
                                    r.get("id")?.as_str()?
                                )))
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            } else if o.contains_key("reference") {
                vec![v.clone()]
            } else {
                return Err(InputError::BadJson(
                    "expected an array, or an object with \"links\" or \"entries\"".into(),
                ));
            }
        }
        _ => return Err(InputError::BadJson("expected an array".into())),
    };
    let mut out = vec![];
    for (i, it) in items.iter().enumerate() {
        let n = i + 1;
        match it {
            Value::String(s) => out.push(row(n, s, "", s, None)),
            Value::Object(o) => {
                let s = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or("");
                let raw = serde_json::to_string(it).unwrap_or_default();
                let ep = s("endpoint");
                let reference = s("reference");
                // Entry shape (`kind`) or link shape (`isFhirUser`,
                // `accessiblePatients`).
                let kind = if o.contains_key("kind") {
                    Some(s("kind").to_string())
                } else if o.get("isFhirUser").and_then(Value::as_bool) == Some(true) {
                    Some("fhirUser".to_string())
                } else {
                    None
                };
                out.push(row(n, &raw, ep, reference, kind.as_deref()));
                if let Some(ps) = o.get("accessiblePatients").and_then(Value::as_array) {
                    for p in ps {
                        let id = p.as_str().unwrap_or_default();
                        let pr = if id.contains('/') {
                            id.to_string()
                        } else {
                            format!("Patient/{id}")
                        };
                        out.push(row(n, &pr, ep, &pr, Some("patient")));
                    }
                }
            }
            other => out.push(InputRow {
                line: n,
                raw: other.to_string().chars().take(200).collect(),
                endpoint: String::new(),
                reference: other.to_string().chars().take(200).collect(),
                kind: None,
                issue: Some(RowIssue::BadReference),
            }),
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ plans

/// Add to the current list, or replace it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Add,
    Replace,
}

/// What applying a row does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RowStatus {
    /// Granted by this import.
    New,
    /// Already granted with the same kind.
    Granted,
    /// Already granted; its kind changes.
    Changed { from: AccessKind },
    /// Bad input (see `issue`).
    Invalid,
    /// Repeats an earlier row of the same input.
    Duplicate,
    /// A second identity on one endpoint.
    Conflict,
    /// Left out on request (e.g. not found on the endpoint).
    Skipped,
}

impl RowStatus {
    pub fn code(&self) -> &'static str {
        match self {
            RowStatus::New => "new",
            RowStatus::Granted => "granted",
            RowStatus::Changed { .. } => "changed",
            RowStatus::Invalid => "invalid",
            RowStatus::Duplicate => "duplicate",
            RowStatus::Conflict => "conflict",
            RowStatus::Skipped => "skipped",
        }
    }
}

/// One planned row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanRow {
    pub line: usize,
    pub raw: String,
    pub endpoint: String,
    pub reference: String,
    pub kind: AccessKind,
    pub status: RowStatus,
    pub issue: Option<RowIssue>,
}

/// The effect of an input on a user's list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub mode: Mode,
    pub format: Format,
    pub rows: Vec<PlanRow>,
    /// The list after applying.
    pub result: Vec<AccessEntry>,
    pub added: Vec<AccessEntry>,
    pub removed: Vec<AccessEntry>,
    /// Entries whose kind changes (new kind).
    pub changed: Vec<AccessEntry>,
}

impl Plan {
    /// Rows per status code.
    pub fn counts(&self) -> BTreeMap<&'static str, usize> {
        let mut m = BTreeMap::new();
        for r in &self.rows {
            *m.entry(r.status.code()).or_insert(0) += 1;
        }
        m
    }
    /// Whether some row was not accepted (invalid, duplicate, conflict).
    pub fn has_rejects(&self) -> bool {
        self.rows.iter().any(|r| {
            matches!(
                r.status,
                RowStatus::Invalid | RowStatus::Duplicate | RowStatus::Conflict
            )
        })
    }
    /// Whether applying changes anything.
    pub fn is_noop(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// Plans an input against the `current` list.
///
/// * `default_endpoint` — for rows without one;
/// * `endpoints` — the realm's endpoints (others are invalid; empty = any);
/// * `skip` — keys to leave out (status [`RowStatus::Skipped`]).
pub fn plan(
    current: &[AccessEntry],
    parsed: &Parsed,
    mode: Mode,
    default_endpoint: Option<&str>,
    endpoints: &[String],
    skip: &HashSet<(String, String)>,
) -> Plan {
    let cur: HashMap<(String, String), AccessKind> =
        current.iter().map(|e| (e.key(), e.kind)).collect();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut rows = vec![];
    let mut accepted: Vec<AccessEntry> = vec![];
    for r in &parsed.rows {
        let endpoint = if r.endpoint.is_empty() {
            default_endpoint.unwrap_or_default().to_string()
        } else {
            r.endpoint.clone()
        };
        let mut issue = r.issue.clone();
        if issue.is_none() && endpoint.is_empty() {
            issue = Some(RowIssue::NoEndpoint);
        }
        if issue.is_none() && !endpoints.is_empty() && !endpoints.contains(&endpoint) {
            issue = Some(RowIssue::UnknownEndpoint {
                endpoint: endpoint.clone(),
            });
        }
        let key = (endpoint.clone(), r.reference.clone());
        let kind = r
            .kind
            .or_else(|| cur.get(&key).copied())
            .unwrap_or_else(|| AccessKind::default_for(&r.reference));
        let status = if issue.is_some() {
            RowStatus::Invalid
        } else if !seen.insert(key.clone()) {
            RowStatus::Duplicate
        } else if skip.contains(&key) {
            RowStatus::Skipped
        } else {
            match cur.get(&key) {
                Some(k) if *k == kind => RowStatus::Granted,
                Some(k) => RowStatus::Changed { from: *k },
                None => RowStatus::New,
            }
        };
        if matches!(
            status,
            RowStatus::New | RowStatus::Granted | RowStatus::Changed { .. }
        ) {
            accepted.push(AccessEntry {
                endpoint: endpoint.clone(),
                reference: r.reference.clone(),
                kind,
            });
        }
        rows.push(PlanRow {
            line: r.line,
            raw: r.raw.clone(),
            endpoint,
            reference: r.reference.clone(),
            kind,
            status,
            issue,
        });
    }
    // Identities: at most one per endpoint. In "add", an identity already
    // there (and not replaced by this input) wins over a new one.
    let mut identity: HashMap<String, String> = HashMap::new();
    if mode == Mode::Add {
        for e in current.iter().filter(|e| e.kind == AccessKind::FhirUser) {
            let demoted = accepted
                .iter()
                .any(|a| a.key() == e.key() && a.kind != AccessKind::FhirUser);
            if !demoted {
                identity.insert(e.endpoint.clone(), e.reference.clone());
            }
        }
    }
    let mut conflicts: HashSet<(String, String)> = HashSet::new();
    for a in accepted.iter().filter(|a| a.kind == AccessKind::FhirUser) {
        match identity.get(&a.endpoint) {
            Some(r) if *r != a.reference => {
                conflicts.insert(a.key());
            }
            Some(_) => {}
            None => {
                identity.insert(a.endpoint.clone(), a.reference.clone());
            }
        }
    }
    if !conflicts.is_empty() {
        accepted.retain(|a| !(a.kind == AccessKind::FhirUser && conflicts.contains(&a.key())));
        for r in rows.iter_mut() {
            if r.kind == AccessKind::FhirUser
                && matches!(
                    r.status,
                    RowStatus::New | RowStatus::Changed { .. } | RowStatus::Granted
                )
                && conflicts.contains(&(r.endpoint.clone(), r.reference.clone()))
            {
                r.status = RowStatus::Conflict;
            }
        }
    }
    let mut result: Vec<AccessEntry> = match mode {
        Mode::Replace => accepted.clone(),
        Mode::Add => {
            let incoming: HashMap<(String, String), AccessKind> =
                accepted.iter().map(|a| (a.key(), a.kind)).collect();
            let mut r: Vec<AccessEntry> = current
                .iter()
                .map(|e| AccessEntry {
                    kind: incoming.get(&e.key()).copied().unwrap_or(e.kind),
                    ..e.clone()
                })
                .collect();
            r.extend(
                accepted
                    .iter()
                    .filter(|a| !cur.contains_key(&a.key()))
                    .cloned(),
            );
            r
        }
    };
    sort(&mut result);
    let after: HashMap<(String, String), AccessKind> =
        result.iter().map(|e| (e.key(), e.kind)).collect();
    let added = result
        .iter()
        .filter(|e| !cur.contains_key(&e.key()))
        .cloned()
        .collect();
    let removed = current
        .iter()
        .filter(|e| !after.contains_key(&e.key()))
        .cloned()
        .collect();
    let changed = result
        .iter()
        .filter(|e| cur.get(&e.key()).is_some_and(|k| *k != e.kind))
        .cloned()
        .collect();
    Plan {
        mode,
        format: parsed.format,
        rows,
        result,
        added,
        removed,
        changed,
    }
}

/// Removes `remove` keys, then adds or re-kinds `add` entries (an added
/// identity replaces the endpoint's previous one, which stays granted as
/// a `patient`/`link`). The console's add, bulk remove and undo; the
/// API's `PATCH`.
pub fn patch(
    current: &[AccessEntry],
    add: &[AccessEntry],
    remove: &[(String, String)],
) -> Vec<AccessEntry> {
    let gone: HashSet<&(String, String)> = remove.iter().collect();
    let mut out: Vec<AccessEntry> = current
        .iter()
        .filter(|e| !gone.contains(&e.key()))
        .cloned()
        .collect();
    for a in add {
        if a.kind == AccessKind::FhirUser {
            for e in out.iter_mut() {
                if e.endpoint == a.endpoint
                    && e.kind == AccessKind::FhirUser
                    && e.reference != a.reference
                {
                    e.kind = AccessKind::default_for(&e.reference);
                }
            }
        }
        match out.iter_mut().find(|e| e.key() == a.key()) {
            Some(e) => e.kind = a.kind,
            None => out.push(a.clone()),
        }
    }
    sort(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(ep: &str, r: &str, me: bool, ps: &[&str]) -> FhirLink {
        FhirLink {
            endpoint: ep.into(),
            reference: r.into(),
            is_fhir_user: me,
            accessible_patients: ps.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn e(ep: &str, r: &str, k: AccessKind) -> AccessEntry {
        AccessEntry::new(ep, r, k)
    }

    fn set(v: &[AccessEntry]) -> HashSet<AccessEntry> {
        v.iter().cloned().collect()
    }

    fn user(links: Vec<FhirLink>) -> crate::user::RealmUser {
        crate::user::RealmUser {
            id: "u".into(),
            username: "u".into(),
            kind: crate::user::UserKind::Practitioner,
            display_name: None,
            email: None,
            email_verified: false,
            links,
            external_subject: None,
            disabled: false,
        }
    }

    #[test]
    fn flatten_and_back_keep_token_semantics() {
        let links = vec![
            link("clinic", "Practitioner/d1", true, &["p1", "p2"]),
            link("clinic", "Patient/p3", false, &[]),
            link("clinic", "PractitionerRole/r1", false, &[]),
            link("lab", "Patient/x", false, &["y"]),
            // The same patient twice: once as a link, once in the array.
            link("clinic", "Patient/p1", false, &[]),
        ];
        let flat = flatten(&links);
        assert_eq!(
            flat,
            vec![
                e("clinic", "Practitioner/d1", AccessKind::FhirUser),
                e("clinic", "Patient/p1", AccessKind::Patient),
                e("clinic", "Patient/p2", AccessKind::Patient),
                e("clinic", "Patient/p3", AccessKind::Patient),
                e("clinic", "PractitionerRole/r1", AccessKind::Link),
                e("lab", "Patient/x", AccessKind::Patient),
                e("lab", "Patient/y", AccessKind::Patient),
            ]
        );
        let back = to_links(&flat);
        assert_eq!(set(&flatten(&back)), set(&flat));
        let (before, after) = (user(links), user(back));
        for ep in ["clinic", "lab", "none"] {
            assert_eq!(
                before.selectable_patients(ep),
                after.selectable_patients(ep)
            );
            assert_eq!(
                before.fhir_user(ep, "https://h/"),
                after.fhir_user(ep, "https://h/")
            );
        }
        // The identity carries the patients; no stray Patient rows.
        assert_eq!(after.links[0].accessible_patients, vec!["p1", "p2", "p3"]);
        assert_eq!(after.links.len(), 2 + 2);
        check(&flat).unwrap();
    }

    #[test]
    fn identity_patient_is_not_its_own_accessible_patient() {
        let flat = vec![
            e("c", "Patient/me", AccessKind::FhirUser),
            e("c", "Patient/kid", AccessKind::Patient),
        ];
        let links = to_links(&flat);
        assert_eq!(links, vec![link("c", "Patient/me", true, &["kid"])]);
        assert_eq!(flatten(&links), flat);
    }

    #[test]
    fn references() {
        assert_eq!(
            parse_reference("Patient/123").unwrap(),
            ("Patient".into(), "123".into())
        );
        assert_eq!(
            parse_reference(" https://h/fhir/sandbox/Patient/a.b-1/_history/3 ").unwrap(),
            ("Patient".into(), "a.b-1".into())
        );
        assert_eq!(parse_reference("Patient/"), Err(RowIssue::BadReference));
        assert_eq!(parse_reference("123"), Err(RowIssue::BadReference));
        assert_eq!(parse_reference("Patient/a b"), Err(RowIssue::BadReference));
        assert_eq!(parse_reference("patient/1"), Err(RowIssue::BadReference));
        assert_eq!(
            parse_reference(&format!("Patient/{}", "x".repeat(65))),
            Err(RowIssue::BadReference)
        );
        assert_eq!(
            parse_reference("Observation/1"),
            Err(RowIssue::UnsupportedType {
                resource_type: "Observation".into()
            })
        );
    }

    fn sample() -> Vec<AccessEntry> {
        vec![
            e("clinic", "Practitioner/d1", AccessKind::FhirUser),
            e("clinic", "Patient/p1", AccessKind::Patient),
            e("clinic", "PractitionerRole/r1", AccessKind::Link),
            e("lab", "Patient/x", AccessKind::Patient),
        ]
    }

    #[test]
    fn every_format_round_trips() {
        let entries = sample();
        for f in [Format::Lines, Format::Csv, Format::Json] {
            let text = export(&entries, f);
            let p = parse(&text).unwrap();
            assert_eq!(p.format, f, "{text}");
            let pl = plan(&entries, &p, Mode::Replace, None, &[], &HashSet::new());
            assert!(!pl.has_rejects(), "{f:?}: {:?}", pl.rows);
            assert!(pl.is_noop(), "{f:?}: {pl:?}");
            assert_eq!(set(&pl.result), set(&entries));
        }
        // Lines without endpoints when the list has one endpoint.
        let one: Vec<_> = entries
            .iter()
            .filter(|e| e.endpoint == "clinic")
            .cloned()
            .collect();
        assert_eq!(
            export(&one, Format::Lines),
            "Practitioner/d1\nPatient/p1\nPractitionerRole/r1\n"
        );
        assert!(export(&entries, Format::Lines).contains("lab/Patient/x\n"));
    }

    #[test]
    fn csv_quoting_and_headers() {
        // (A comma is not valid in an id, but endpoint names and future
        // columns get quoted all the same.)
        let text = export(&[e("a,b", "Patient/1", AccessKind::Patient)], Format::Csv);
        assert!(text.contains("\"a,b\",Patient/1"), "{text}");
        let f = csv_split("a,\"b,\"\"c\"\"\",d", ',');
        assert_eq!(f, vec!["a", "b,\"c\"", "d"]);
        // Header in another order, semicolons, BOM, extra columns.
        let p = parse("\u{feff}Kind;Reference;Endpoint;note\npatient;Patient/1;lab;x\n").unwrap();
        assert_eq!(p.format, Format::Csv);
        assert_eq!(p.rows[0].endpoint, "lab");
        assert_eq!(p.rows[0].reference, "Patient/1");
        assert_eq!(p.rows[0].kind, Some(AccessKind::Patient));
        // No header: endpoint,reference / reference,kind.
        let p = parse("lab,Patient/1\nPatient/2,fhirUser\nlab,Patient/3,link\n").unwrap();
        assert_eq!(
            p.rows
                .iter()
                .map(|r| (r.endpoint.as_str(), r.reference.as_str(), r.kind))
                .collect::<Vec<_>>(),
            vec![
                ("lab", "Patient/1", None),
                ("", "Patient/2", Some(AccessKind::FhirUser)),
                ("lab", "Patient/3", Some(AccessKind::Link)),
            ]
        );
        // Tabs (pasted from a spreadsheet).
        let p = parse("endpoint\treference\nlab\tPatient/9\n").unwrap();
        assert_eq!(p.rows[0].reference, "Patient/9");
    }

    #[test]
    fn lines_with_comments_urls_and_garbage() {
        let p = parse(
            "# exported\n\nPatient/1\n  https://h/fhir/lab/Patient/2  \nlab/Patient/3\nnot a ref\nObservation/4\n",
        )
        .unwrap();
        assert_eq!(p.format, Format::Lines);
        let v: Vec<_> = p
            .rows
            .iter()
            .map(|r| {
                (
                    r.line,
                    r.endpoint.as_str(),
                    r.reference.as_str(),
                    r.issue.as_ref().map(RowIssue::code),
                )
            })
            .collect();
        assert_eq!(
            v,
            vec![
                (3, "", "Patient/1", None),
                (4, "", "Patient/2", None),
                (5, "lab", "Patient/3", None),
                (6, "", "not a ref", Some("bad-reference")),
                (7, "", "Observation/4", Some("unsupported-type")),
            ]
        );
    }

    #[test]
    fn json_shapes() {
        // Links array (accessible patients expand).
        let p = parse(r#"[{"endpoint":"c","reference":"Practitioner/d","isFhirUser":true,"accessiblePatients":["p1"]}]"#).unwrap();
        assert_eq!(p.rows.len(), 2);
        assert_eq!(p.rows[0].kind, Some(AccessKind::FhirUser));
        assert_eq!(p.rows[1].reference, "Patient/p1");
        // Entries / user / strings / Bundle.
        let p = parse(r#"{"entries":[{"endpoint":"c","reference":"Patient/1","kind":"patient"}]}"#)
            .unwrap();
        assert_eq!(p.rows[0].kind, Some(AccessKind::Patient));
        let p = parse(
            r#"{"id":"u","links":[{"endpoint":"c","reference":"Patient/1","isFhirUser":false}]}"#,
        )
        .unwrap();
        assert_eq!(p.rows[0].kind, None);
        let p = parse(r#"["Patient/1", 7]"#).unwrap();
        assert_eq!(p.rows[1].issue, Some(RowIssue::BadReference));
        let p = parse(r#"{"resourceType":"Bundle","entry":[{"resource":{"resourceType":"Patient","id":"b1"}}]}"#).unwrap();
        assert_eq!(p.rows[0].reference, "Patient/b1");
        assert!(matches!(parse("[1,"), Err(InputError::BadJson(_))));
        assert!(matches!(parse(r#"{"x":1}"#), Err(InputError::BadJson(_))));
        // A bad kind and a kind/type mismatch.
        let p = parse(r#"[{"reference":"Patient/1","kind":"boss"},{"reference":"Practitioner/1","kind":"patient"}]"#).unwrap();
        assert_eq!(
            p.rows[0].issue.as_ref().map(RowIssue::code),
            Some("bad-kind")
        );
        assert_eq!(p.rows[1].issue, Some(RowIssue::KindMismatch));
    }

    #[test]
    fn limits() {
        let many: String = (0..=MAX_IMPORT).map(|i| format!("Patient/{i}\n")).collect();
        assert_eq!(parse(&many), Err(InputError::TooMany(MAX_IMPORT + 1)));
        let ok: String = (0..MAX_IMPORT).map(|i| format!("Patient/{i}\n")).collect();
        assert_eq!(parse(&ok).unwrap().rows.len(), MAX_IMPORT);
        let huge = "x".repeat(MAX_TEXT_BYTES + 1);
        assert!(matches!(parse(&huge), Err(InputError::TooLarge(_))));
        let big: Vec<AccessEntry> = (0..=MAX_PER_USER)
            .map(|i| e("c", &format!("Patient/{i}"), AccessKind::Patient))
            .collect();
        assert_eq!(check(&big), Err(ListError::TooMany(MAX_PER_USER + 1)));
    }

    #[test]
    fn plan_add_dedups_and_flags() {
        let current = vec![
            e("clinic", "Practitioner/d1", AccessKind::FhirUser),
            e("clinic", "Patient/p1", AccessKind::Patient),
        ];
        let p = parse(
            "Patient/p1\nPatient/p2\nPatient/p2\nbogus\nPractitioner/d2,fhirUser\nother/Patient/z\nPatient/skip\n",
        )
        .unwrap();
        let skip: HashSet<_> = [("clinic".to_string(), "Patient/skip".to_string())].into();
        let pl = plan(
            &current,
            &p,
            Mode::Add,
            Some("clinic"),
            &["clinic".to_string()],
            &skip,
        );
        let st: Vec<&str> = pl.rows.iter().map(|r| r.status.code()).collect();
        assert_eq!(
            st,
            vec![
                "granted",
                "new",
                "duplicate",
                "invalid",
                "conflict",
                "invalid",
                "skipped"
            ]
        );
        assert_eq!(
            pl.rows[5].issue,
            Some(RowIssue::UnknownEndpoint {
                endpoint: "other".into()
            })
        );
        assert_eq!(
            pl.added,
            vec![e("clinic", "Patient/p2", AccessKind::Patient)]
        );
        assert!(pl.removed.is_empty());
        assert_eq!(pl.result.len(), 3);
        check(&pl.result).unwrap();
        assert_eq!(pl.counts()["granted"], 1);
        assert!(pl.has_rejects());
    }

    #[test]
    fn plan_replace_and_kind_changes() {
        let current = sample();
        // Replace: p1 stays, d1 becomes a plain link, x goes, n comes.
        let p = parse("endpoint,reference,kind\nclinic,Patient/p1,\nclinic,Practitioner/d1,link\nclinic,Patient/n,\n").unwrap();
        let pl = plan(&current, &p, Mode::Replace, None, &[], &HashSet::new());
        assert_eq!(
            pl.rows.iter().map(|r| r.status.code()).collect::<Vec<_>>(),
            vec!["granted", "changed", "new"]
        );
        assert_eq!(
            pl.added,
            vec![e("clinic", "Patient/n", AccessKind::Patient)]
        );
        assert_eq!(
            set(&pl.removed),
            set(&[
                e("clinic", "PractitionerRole/r1", AccessKind::Link),
                e("lab", "Patient/x", AccessKind::Patient)
            ])
        );
        assert_eq!(
            pl.changed,
            vec![e("clinic", "Practitioner/d1", AccessKind::Link)]
        );
        // Two identities in one replace: the second conflicts.
        let p = parse("Practitioner/a,fhirUser\nPractitioner/b,fhirUser\n").unwrap();
        let pl = plan(&[], &p, Mode::Replace, Some("c"), &[], &HashSet::new());
        assert_eq!(
            pl.rows.iter().map(|r| r.status.code()).collect::<Vec<_>>(),
            vec!["new", "conflict"]
        );
        check(&pl.result).unwrap();
        // Add a new identity after demoting the old one in the same input.
        let p = parse("Practitioner/d1,link\nPractitioner/d9,fhirUser\n").unwrap();
        let pl = plan(
            &sample(),
            &p,
            Mode::Add,
            Some("clinic"),
            &[],
            &HashSet::new(),
        );
        assert!(!pl.has_rejects(), "{:?}", pl.rows);
        check(&pl.result).unwrap();
        // Replace with nothing clears.
        let pl = plan(
            &sample(),
            &parse("").unwrap(),
            Mode::Replace,
            None,
            &[],
            &HashSet::new(),
        );
        assert!(pl.result.is_empty());
        assert_eq!(pl.removed.len(), 4);
    }

    #[test]
    fn patch_and_undo() {
        let cur = sample();
        let remove = vec![("lab".to_string(), "Patient/x".to_string())];
        let add = vec![
            e("clinic", "Practitioner/d2", AccessKind::FhirUser),
            e("clinic", "Patient/p9", AccessKind::Patient),
        ];
        let after = patch(&cur, &add, &remove);
        check(&after).unwrap();
        assert!(after.contains(&e("clinic", "Practitioner/d1", AccessKind::Link)));
        assert!(after.contains(&e("clinic", "Practitioner/d2", AccessKind::FhirUser)));
        assert!(!after.iter().any(|x| x.endpoint == "lab"));
        // Undo: put back what was removed or re-kinded, drop what was added.
        let before: HashMap<_, _> = cur.iter().map(|x| (x.key(), x.clone())).collect();
        let undo_add: Vec<AccessEntry> =
            cur.iter().filter(|x| !after.contains(x)).cloned().collect();
        let undo_remove: Vec<(String, String)> = after
            .iter()
            .filter(|x| !before.contains_key(&x.key()))
            .map(AccessEntry::key)
            .collect();
        assert_eq!(set(&patch(&after, &undo_add, &undo_remove)), set(&cur));
    }

    #[test]
    fn fingerprint_is_order_independent() {
        let mut a = sample();
        let f = fingerprint(&a);
        a.reverse();
        assert_eq!(fingerprint(&a), f);
        a[0].kind = AccessKind::Link;
        assert_ne!(fingerprint(&a), f);
        assert_eq!(f.len(), 20);
    }

    #[test]
    fn kinds_parse() {
        for (s, k) in [
            ("fhirUser", AccessKind::FhirUser),
            ("fhir-user", AccessKind::FhirUser),
            ("Patient", AccessKind::Patient),
            ("accessible patient", AccessKind::Patient),
            ("LINK", AccessKind::Link),
        ] {
            assert_eq!(AccessKind::parse(s), Some(k), "{s}");
        }
        assert_eq!(AccessKind::parse("x"), None);
    }
}
