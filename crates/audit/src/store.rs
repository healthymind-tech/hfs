//! The dedicated audit-event store: row shape, search model and reader trait.
//!
//! With `HFS_AUDIT_STORE=table` (the default on PostgreSQL) an `AuditEvent`
//! is not a row of the general FHIR resource tables. It is one row of an
//! append-only `audit_event` table plus a few rows of a narrow lookup table
//! (`audit_event_ref`) for the references an auditor searches by. This module
//! holds what is independent of the database:
//!
//! - [`AuditRow`]: the columns extracted from an event's JSON;
//! - [`AuditQuery`] and [`parse_search`]: the fixed set of `AuditEvent`
//!   search parameters the store answers (`_id`, `_lastUpdated`, `date`,
//!   `patient`, `entity`, `agent`, `type`, `subtype`, `action`, `outcome`);
//! - [`AuditEventReader`]: what a server needs to serve `AuditEvent` read and
//!   search from the store.
//!
//! The event JSON itself is stored unchanged.

use async_trait::async_trait;
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde_json::Value;

/// Search parameters the table store answers, for error messages and the
/// CapabilityStatement.
pub const SUPPORTED_SEARCH_PARAMS: &[&str] = &[
    "_id",
    "_lastUpdated",
    "date",
    "patient",
    "entity",
    "agent",
    "type",
    "subtype",
    "action",
    "outcome",
];

/// Which reference of the event a lookup row was taken from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefKind {
    /// `AuditEvent.entity.what`.
    Entity,
    /// `AuditEvent.agent.who`.
    Agent,
}

impl RefKind {
    /// The one-letter code stored in `audit_event_ref.kind`.
    pub fn code(self) -> &'static str {
        match self {
            Self::Entity => "e",
            Self::Agent => "a",
        }
    }
}

/// One searchable reference of an event.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AuditRef {
    /// Where the reference sits.
    pub kind: RefKind,
    /// Resource type of a literal reference (`Patient`); empty for an
    /// identifier (`agent.who.identifier.value`, `entity.what.identifier.value`).
    pub ref_type: String,
    /// Logical id of the reference, or the identifier value.
    pub ref_id: String,
}

/// The columns of one stored event.
#[derive(Debug, Clone)]
pub struct AuditRow {
    /// Logical id (`AuditEvent.id`).
    pub id: String,
    /// `AuditEvent.recorded`.
    pub recorded: DateTime<Utc>,
    /// `AuditEvent.action`.
    pub action: Option<String>,
    /// `AuditEvent.outcome`.
    pub outcome: Option<String>,
    /// `AuditEvent.type.code`.
    pub type_code: Option<String>,
    /// `AuditEvent.subtype[*].code`.
    pub subtype_codes: Vec<String>,
    /// Lookup rows.
    pub refs: Vec<AuditRef>,
    /// The complete resource.
    pub event: Value,
}

/// Splits a reference into `(type, id)`: `Patient/1`, `http://x/fhir/Patient/1`
/// and `Patient/1/_history/2` give `("Patient", "1")`. `None` for a fragment
/// or something that is not `[base/]Type/id`.
pub fn split_reference(reference: &str) -> Option<(String, String)> {
    let reference = reference.split("/_history/").next().unwrap_or(reference);
    if reference.starts_with('#') || reference.starts_with("urn:") {
        return None;
    }
    let mut parts = reference.rsplit('/');
    let id = parts.next().filter(|s| !s.is_empty())?;
    let ty = parts.next().filter(|s| {
        s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && s.chars().all(|c| c.is_ascii_alphanumeric())
    })?;
    Some((ty.to_string(), id.to_string()))
}

fn refs_of(
    node: Option<&Value>,
    key: &str,
    kind: RefKind,
    skip: Option<&str>,
    out: &mut Vec<AuditRef>,
) {
    for item in node.and_then(Value::as_array).into_iter().flatten() {
        let Some(target) = item.get(key) else {
            continue;
        };
        if let Some((ref_type, ref_id)) = target
            .get("reference")
            .and_then(Value::as_str)
            .filter(|r| Some(*r) != skip)
            .and_then(split_reference)
        {
            out.push(AuditRef {
                kind,
                ref_type,
                ref_id,
            });
        }
        if let Some(value) = target
            .get("identifier")
            .and_then(|i| i.get("value"))
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
        {
            out.push(AuditRef {
                kind,
                ref_type: String::new(),
                ref_id: value.to_string(),
            });
        }
    }
}

impl AuditRow {
    /// Extracts the row of `event`. An event without an id gets a UUIDv7; an
    /// event without a parseable `recorded` is filed under `now`.
    pub fn from_event(mut event: Value, now: DateTime<Utc>) -> Self {
        let id = match event.get("id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => {
                let id = uuid::Uuid::now_v7().to_string();
                if let Some(obj) = event.as_object_mut() {
                    obj.insert("id".into(), Value::String(id.clone()));
                }
                id
            }
        };
        if let Some(obj) = event.as_object_mut() {
            obj.entry("resourceType")
                .or_insert_with(|| Value::String("AuditEvent".into()));
        }
        let recorded = event
            .get("recorded")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or(now);
        let text = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);
        let mut refs = Vec::new();
        refs_of(
            event.get("entity"),
            "what",
            RefKind::Entity,
            None,
            &mut refs,
        );
        // The server itself is an agent of every event it records
        // (`source.observer`): a lookup row for it would select nothing.
        let observer = event
            .get("source")
            .and_then(|s| s.get("observer"))
            .and_then(|o| o.get("reference"))
            .and_then(Value::as_str);
        refs_of(
            event.get("agent"),
            "who",
            RefKind::Agent,
            observer,
            &mut refs,
        );
        let mut seen = std::collections::HashSet::new();
        refs.retain(|r| seen.insert(r.clone()));
        Self {
            id,
            recorded,
            action: text(event.get("action")),
            outcome: text(event.get("outcome")),
            type_code: text(event.get("type").and_then(|t| t.get("code"))),
            subtype_codes: event
                .get("subtype")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|s| s.get("code").and_then(Value::as_str))
                .map(str::to_string)
                .collect(),
            refs,
            event,
        }
    }
}

/// The time a UUIDv7 id was generated at, when `id` is one. New events carry
/// such ids, which lets a read by id look at one monthly partition only.
pub fn uuid_v7_time(id: &str) -> Option<DateTime<Utc>> {
    let uuid = uuid::Uuid::parse_str(id).ok()?;
    if uuid.get_version_num() != 7 {
        return None;
    }
    let (secs, nanos) = uuid.get_timestamp()?.to_unix();
    Utc.timestamp_opt(i64::try_from(secs).ok()?, nanos).single()
}

/// A FHIR search prefix on a date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatePrefix {
    /// `eq` (also `ap`, which this store does not widen).
    Eq,
    /// `ne`.
    Ne,
    /// `gt` / `sa`.
    Gt,
    /// `lt` / `eb`.
    Lt,
    /// `ge`.
    Ge,
    /// `le`.
    Le,
}

/// One date criterion: the prefix and the instant range `[lo, hi)` the value
/// covers at its precision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateBound {
    /// Comparison.
    pub prefix: DatePrefix,
    /// First instant of the value.
    pub lo: DateTime<Utc>,
    /// First instant after the value.
    pub hi: DateTime<Utc>,
}

/// One alternative of a token criterion (`system|code`, `code`, `|code`,
/// `system|`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenValue {
    /// `Some("")` means "no system" (`|code`).
    pub system: Option<String>,
    /// `None` for `system|`.
    pub code: Option<String>,
}

/// Which element a token criterion tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenField {
    /// `AuditEvent.action`.
    Action,
    /// `AuditEvent.outcome`.
    Outcome,
    /// `AuditEvent.type`.
    Type,
    /// `AuditEvent.subtype`.
    Subtype,
}

/// One token criterion: any of `values` (or none of them with `:not`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenFilter {
    /// Element.
    pub field: TokenField,
    /// `:not`.
    pub negate: bool,
    /// Alternatives.
    pub values: Vec<TokenValue>,
}

/// Which references a reference criterion matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefField {
    /// `patient`: an agent or entity that is a `Patient`.
    Patient,
    /// `entity`.
    Entity,
    /// `agent`.
    Agent,
}

/// One alternative of a reference criterion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefValue {
    /// Resource type, when the value named one.
    pub ref_type: Option<String>,
    /// Logical id (or identifier value).
    pub ref_id: String,
}

/// One reference criterion: any of `values`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefFilter {
    /// Parameter.
    pub field: RefField,
    /// Alternatives.
    pub values: Vec<RefValue>,
}

/// A search of the audit store. Criteria are AND-ed; the alternatives inside
/// one criterion are OR-ed.
#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    /// `_id`.
    pub ids: Option<Vec<String>>,
    /// `date` and `_lastUpdated` (an event is never updated, so both test
    /// `recorded`). Each inner list is one parameter instance.
    pub dates: Vec<Vec<DateBound>>,
    /// `patient`, `entity`, `agent`.
    pub refs: Vec<RefFilter>,
    /// `type`, `subtype`, `action`, `outcome`.
    pub tokens: Vec<TokenFilter>,
    /// Oldest first (`_sort=date`) instead of newest first.
    pub ascending: bool,
    /// Rows to return.
    pub limit: usize,
    /// Keyset position: return events strictly after this `(recorded, id)` in
    /// the sort order.
    pub after: Option<(DateTime<Utc>, String)>,
}

impl AuditQuery {
    /// Whether the query has no criteria at all.
    pub fn is_unfiltered(&self) -> bool {
        self.ids.is_none()
            && self.dates.is_empty()
            && self.refs.is_empty()
            && self.tokens.is_empty()
    }
}

/// A stored event as the API returns it.
#[derive(Debug, Clone)]
pub struct StoredAuditEvent {
    /// Logical id.
    pub id: String,
    /// `recorded`, which is also `meta.lastUpdated`.
    pub recorded: DateTime<Utc>,
    /// The resource, with `meta.versionId = "1"` and `meta.lastUpdated`.
    pub event: Value,
}

impl StoredAuditEvent {
    /// Adds the server-side `meta` to a stored event.
    pub fn new(id: String, recorded: DateTime<Utc>, mut event: Value) -> Self {
        if let Value::Object(obj) = &mut event {
            let meta = obj
                .entry("meta")
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if let Value::Object(meta) = meta {
                meta.insert("versionId".into(), Value::String("1".into()));
                meta.insert(
                    "lastUpdated".into(),
                    Value::String(recorded.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
                );
            }
        }
        Self {
            id,
            recorded,
            event,
        }
    }
}

/// Why a read of the audit store failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditReadError {
    /// The request named something the store does not support (HTTP 400).
    BadRequest(String),
    /// The store failed.
    Storage(String),
}

impl std::fmt::Display for AuditReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(m) | Self::Storage(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for AuditReadError {}

/// Reading the dedicated audit store. Every call is confined to `tenant`.
#[async_trait]
pub trait AuditEventReader: Send + Sync + 'static {
    /// Up to `query.limit` matching events in the query's order.
    async fn search(
        &self,
        tenant: &str,
        query: &AuditQuery,
    ) -> Result<Vec<StoredAuditEvent>, AuditReadError>;

    /// Number of matching events (`limit` and `after` are ignored).
    async fn count(&self, tenant: &str, query: &AuditQuery) -> Result<u64, AuditReadError>;

    /// One event by id.
    async fn read(
        &self,
        tenant: &str,
        id: &str,
    ) -> Result<Option<StoredAuditEvent>, AuditReadError>;
}

/// The criteria of a request, parsed by [`parse_search`].
#[derive(Debug, Clone, Default)]
pub struct ParsedSearch {
    /// The criteria (`limit`, `ascending` and `after` are left at their
    /// defaults; the caller sets them from `_count`, `_sort` and its cursor).
    pub query: AuditQuery,
    /// The `name=value` pairs that were understood, for `Bundle.link[self]`.
    pub used: Vec<(String, String)>,
    /// Names of parameters this store does not support.
    pub unsupported: Vec<String>,
}

fn bad(message: String) -> AuditReadError {
    AuditReadError::BadRequest(message)
}

/// Parses a FHIR date/dateTime search value into its instant range.
pub fn parse_date_range(value: &str) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let utc = |d: NaiveDate| -> Option<DateTime<Utc>> {
        Some(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?))
    };
    if let Some((date, _)) = value.split_once('T') {
        NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
        if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
            let lo = dt.with_timezone(&Utc);
            // Precision of the value: whole seconds unless it carries a fraction.
            let step = match value.split_once('.') {
                Some((_, frac)) => {
                    let digits = frac.chars().take_while(char::is_ascii_digit).count() as u32;
                    chrono::Duration::nanoseconds(10i64.pow(9u32.saturating_sub(digits.min(9))))
                }
                None => chrono::Duration::seconds(1),
            };
            return Some((lo, lo + step));
        }
        // No offset: read as UTC, with or without seconds.
        for (fmt, step) in [
            ("%Y-%m-%dT%H:%M:%S%.f", chrono::Duration::seconds(1)),
            ("%Y-%m-%dT%H:%M", chrono::Duration::minutes(1)),
        ] {
            if let Ok(naive) = NaiveDateTime::parse_from_str(value, fmt) {
                let lo = Utc.from_utc_datetime(&naive);
                return Some((lo, lo + step));
            }
        }
        // hh:mm with an offset is not RFC 3339; accept it.
        if let Ok(dt) = DateTime::parse_from_str(value, "%Y-%m-%dT%H:%M%:z") {
            let lo = dt.with_timezone(&Utc);
            return Some((lo, lo + chrono::Duration::minutes(1)));
        }
        return None;
    }
    match value.len() {
        4 => {
            let year: i32 = value.parse().ok()?;
            Some((
                utc(NaiveDate::from_ymd_opt(year, 1, 1)?)?,
                utc(NaiveDate::from_ymd_opt(year + 1, 1, 1)?)?,
            ))
        }
        7 => {
            let first = NaiveDate::parse_from_str(&format!("{value}-01"), "%Y-%m-%d").ok()?;
            let next = if first.month() == 12 {
                NaiveDate::from_ymd_opt(first.year() + 1, 1, 1)?
            } else {
                NaiveDate::from_ymd_opt(first.year(), first.month() + 1, 1)?
            };
            Some((utc(first)?, utc(next)?))
        }
        10 => {
            let day = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
            Some((utc(day)?, utc(day.succ_opt()?)?))
        }
        _ => None,
    }
}

fn parse_date_bound(name: &str, value: &str) -> Result<DateBound, AuditReadError> {
    let (prefix, rest) = match value.get(..2) {
        Some("eq") | Some("ap") => (DatePrefix::Eq, &value[2..]),
        Some("ne") => (DatePrefix::Ne, &value[2..]),
        Some("gt") | Some("sa") => (DatePrefix::Gt, &value[2..]),
        Some("lt") | Some("eb") => (DatePrefix::Lt, &value[2..]),
        Some("ge") => (DatePrefix::Ge, &value[2..]),
        Some("le") => (DatePrefix::Le, &value[2..]),
        _ => (DatePrefix::Eq, value),
    };
    let (lo, hi) = parse_date_range(rest).ok_or_else(|| {
        bad(format!(
            "invalid date value {value:?} for parameter {name:?}"
        ))
    })?;
    Ok(DateBound { prefix, lo, hi })
}

fn parse_token(value: &str) -> TokenValue {
    match value.split_once('|') {
        Some((system, code)) => TokenValue {
            system: Some(system.to_string()),
            code: (!code.is_empty()).then(|| code.to_string()),
        },
        None => TokenValue {
            system: None,
            code: Some(value.to_string()),
        },
    }
}

fn parse_ref(field: RefField, value: &str) -> RefValue {
    match split_reference(value) {
        Some((ref_type, ref_id)) => RefValue {
            ref_type: Some(ref_type),
            ref_id,
        },
        None => RefValue {
            ref_type: (field == RefField::Patient).then(|| "Patient".to_string()),
            ref_id: value.to_string(),
        },
    }
}

/// Parses the search criteria of a request. Result parameters (`_count`,
/// `_sort`, `_summary`, paging, formatting) are not criteria: the caller
/// removes them first. A parameter this store does not answer is listed in
/// [`ParsedSearch::unsupported`]; a supported parameter with a modifier or a
/// value it cannot interpret is an error.
pub fn parse_search(params: &[(String, String)]) -> Result<ParsedSearch, AuditReadError> {
    let mut out = ParsedSearch::default();
    for (raw_name, value) in params {
        let (name, modifier) = match raw_name.split_once(':') {
            Some((n, m)) => (n, Some(m)),
            None => (raw_name.as_str(), None),
        };
        let values: Vec<&str> = value.split(',').filter(|v| !v.is_empty()).collect();
        let no_modifier = |what: &str| match modifier {
            None => Ok(()),
            Some(m) => Err(bad(format!(
                "modifier :{m} is not supported on AuditEvent parameter {what:?}"
            ))),
        };
        match name {
            "_id" => {
                no_modifier(name)?;
                let ids: Vec<String> = values.iter().map(|v| v.to_string()).collect();
                out.query.ids = Some(match out.query.ids.take() {
                    // Repeated `_id` is an intersection.
                    Some(prev) => prev.into_iter().filter(|p| ids.contains(p)).collect(),
                    None => ids,
                });
            }
            "date" | "_lastUpdated" => {
                no_modifier(name)?;
                if values.is_empty() {
                    continue;
                }
                let bounds = values
                    .iter()
                    .map(|v| parse_date_bound(name, v))
                    .collect::<Result<Vec<_>, _>>()?;
                out.query.dates.push(bounds);
            }
            "patient" | "entity" | "agent" => {
                no_modifier(name)?;
                if values.is_empty() {
                    continue;
                }
                let field = match name {
                    "patient" => RefField::Patient,
                    "entity" => RefField::Entity,
                    _ => RefField::Agent,
                };
                out.query.refs.push(RefFilter {
                    field,
                    values: values.iter().map(|v| parse_ref(field, v)).collect(),
                });
            }
            "type" | "subtype" | "action" | "outcome" => {
                let negate = match modifier {
                    None => false,
                    Some("not") => true,
                    Some(m) => {
                        return Err(bad(format!(
                            "modifier :{m} is not supported on AuditEvent parameter {name:?} \
                             (supported: :not)"
                        )));
                    }
                };
                if values.is_empty() {
                    continue;
                }
                let field = match name {
                    "type" => TokenField::Type,
                    "subtype" => TokenField::Subtype,
                    "action" => TokenField::Action,
                    _ => TokenField::Outcome,
                };
                out.query.tokens.push(TokenFilter {
                    field,
                    negate,
                    values: values.iter().map(|v| parse_token(v)).collect(),
                });
            }
            _ => {
                if !out.unsupported.iter().any(|u| u == raw_name) {
                    out.unsupported.push(raw_name.clone());
                }
                continue;
            }
        }
        out.used.push((raw_name.clone(), value.clone()));
    }
    Ok(out)
}

/// Whether `event` matches `query`'s criteria. The reference semantics of the
/// SQL store, for tests and for stores without an index.
pub fn matches(query: &AuditQuery, row: &AuditRow) -> bool {
    if let Some(ids) = &query.ids
        && !ids.contains(&row.id)
    {
        return false;
    }
    let date_ok = |b: &DateBound| match b.prefix {
        DatePrefix::Eq => row.recorded >= b.lo && row.recorded < b.hi,
        DatePrefix::Ne => !(row.recorded >= b.lo && row.recorded < b.hi),
        DatePrefix::Gt => row.recorded >= b.hi,
        DatePrefix::Lt => row.recorded < b.lo,
        DatePrefix::Ge => row.recorded >= b.lo,
        DatePrefix::Le => row.recorded < b.hi,
    };
    if !query.dates.iter().all(|alts| alts.iter().any(date_ok)) {
        return false;
    }
    let ref_ok = |f: &RefFilter| {
        f.values.iter().any(|v| {
            row.refs.iter().any(|r| {
                r.ref_id == v.ref_id
                    && v.ref_type.as_ref().is_none_or(|t| *t == r.ref_type)
                    && match f.field {
                        RefField::Patient => r.ref_type == "Patient",
                        RefField::Entity => r.kind == RefKind::Entity,
                        RefField::Agent => r.kind == RefKind::Agent,
                    }
            })
        })
    };
    if !query.refs.iter().all(ref_ok) {
        return false;
    }
    let codings = |field: TokenField| -> Vec<(String, String)> {
        let coding = |c: &Value| {
            (
                c.get("system")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                c.get("code")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )
        };
        match field {
            TokenField::Action => vec![(
                "http://hl7.org/fhir/audit-event-action".to_string(),
                row.action.clone().unwrap_or_default(),
            )],
            TokenField::Outcome => vec![(
                "http://hl7.org/fhir/audit-event-outcome".to_string(),
                row.outcome.clone().unwrap_or_default(),
            )],
            TokenField::Type => row.event.get("type").map(coding).into_iter().collect(),
            TokenField::Subtype => row
                .event
                .get("subtype")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(coding)
                .collect(),
        }
    };
    query.tokens.iter().all(|f| {
        let have = codings(f.field);
        let hit = f.values.iter().any(|v| {
            have.iter().any(|(system, code)| {
                v.code.as_ref().is_none_or(|c| c == code)
                    && match (&v.system, f.field) {
                        (None, _) => true,
                        // `action` and `outcome` are `code` elements: their
                        // system is implied, so any system the client names is
                        // accepted when it is the element's own.
                        (Some(s), _) => s == system,
                    }
            })
        });
        hit != f.negate
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn event() -> Value {
        json!({
            "resourceType": "AuditEvent", "id": "0193a5c0-0000-7000-8000-000000000001",
            "type": {"system": "http://terminology.hl7.org/CodeSystem/audit-event-type", "code": "rest"},
            "subtype": [{"system": "http://hl7.org/fhir/restful-interaction", "code": "create"}],
            "action": "C", "recorded": "2026-11-20T10:11:12.345Z", "outcome": "0",
            "agent": [
                {"requestor": true, "who": {"identifier": {"value": "user-7"}, "display": "Dr. Who"}},
                {"requestor": false, "who": {"reference": "Device/hfs"}}
            ],
            "source": {"observer": {"reference": "Device/hfs"}},
            "entity": [
                {"what": {"reference": "Observation/o1"}},
                {"what": {"reference": "http://x.test/fhir/Patient/p1/_history/3"}},
                {"what": {"identifier": {"value": "req-1"}}},
                {"what": {"reference": "Patient/p1"}}
            ]
        })
    }

    fn p(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn a_row_carries_the_searchable_columns_and_each_reference_once() {
        let row = AuditRow::from_event(event(), Utc::now());
        assert_eq!(row.id, "0193a5c0-0000-7000-8000-000000000001");
        assert_eq!(row.recorded, t("2026-11-20T10:11:12.345Z"));
        assert_eq!(row.action.as_deref(), Some("C"));
        assert_eq!(row.outcome.as_deref(), Some("0"));
        assert_eq!(row.type_code.as_deref(), Some("rest"));
        assert_eq!(row.subtype_codes, vec!["create"]);
        let refs: Vec<(&str, &str, &str)> = row
            .refs
            .iter()
            .map(|r| (r.kind.code(), r.ref_type.as_str(), r.ref_id.as_str()))
            .collect();
        assert_eq!(
            refs,
            vec![
                ("e", "Observation", "o1"),
                ("e", "Patient", "p1"),
                ("e", "", "req-1"),
                ("a", "", "user-7"),
            ],
            "the observing server's own agent entry is not indexed"
        );
        // The stored JSON is the event, untouched.
        assert_eq!(row.event, event());
    }

    #[test]
    fn an_event_without_id_or_recorded_is_still_storable() {
        let now = t("2026-11-25T00:00:00Z");
        let row = AuditRow::from_event(json!({"action": "R"}), now);
        assert_eq!(row.recorded, now);
        assert_eq!(row.event["id"], row.id.as_str());
        assert_eq!(row.event["resourceType"], "AuditEvent");
        assert!(uuid_v7_time(&row.id).is_some());
    }

    #[test]
    fn uuid_v7_ids_give_their_time_and_other_ids_do_not() {
        let id = uuid::Uuid::now_v7().to_string();
        let at = uuid_v7_time(&id).unwrap();
        assert!((Utc::now() - at).num_seconds().abs() < 5);
        assert!(uuid_v7_time(&uuid::Uuid::new_v4().to_string()).is_none());
        assert!(uuid_v7_time("a1").is_none());
    }

    #[test]
    fn date_values_cover_their_precision() {
        assert_eq!(
            parse_date_range("2026").unwrap(),
            (t("2026-01-01T00:00:00Z"), t("2027-01-01T00:00:00Z"))
        );
        assert_eq!(
            parse_date_range("2026-12").unwrap(),
            (t("2026-12-01T00:00:00Z"), t("2027-01-01T00:00:00Z"))
        );
        assert_eq!(
            parse_date_range("2026-11-02").unwrap(),
            (t("2026-11-02T00:00:00Z"), t("2026-11-03T00:00:00Z"))
        );
        assert_eq!(
            parse_date_range("2026-11-02T10:00:00+08:00").unwrap(),
            (t("2026-11-02T02:00:00Z"), t("2026-11-02T02:00:01Z"))
        );
        assert_eq!(
            parse_date_range("2026-11-02T10:00:00.5Z").unwrap(),
            (t("2026-11-02T10:00:00.5Z"), t("2026-11-02T10:00:00.6Z"))
        );
        assert_eq!(
            parse_date_range("2026-11-02T10:00").unwrap(),
            (t("2026-11-02T10:00:00Z"), t("2026-11-02T10:01:00Z"))
        );
        assert_eq!(
            parse_date_range("2026-11-02T10:00:00").unwrap(),
            (t("2026-11-02T10:00:00Z"), t("2026-11-02T10:00:01Z"))
        );
        for bad in [
            "",
            "20",
            "2026-13",
            "2026-02-30",
            "yesterday",
            "2026-11-02Tnoon",
        ] {
            assert!(parse_date_range(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_fixed_parameter_set_is_parsed_and_the_rest_reported() {
        let parsed = parse_search(&p(&[
            ("date", "ge2026-11-01"),
            ("date", "lt2026-12"),
            ("patient", "p1"),
            ("entity", "Observation/o1,Observation/o2"),
            ("agent", "user-7"),
            ("type", "rest"),
            ("subtype", "http://hl7.org/fhir/restful-interaction|create"),
            ("action", "C,U"),
            ("outcome:not", "0"),
            ("_id", "a,b"),
            ("_lastUpdated", "gt2026-01-01"),
            ("entity-name", "x"),
            ("_content", "y"),
            ("entity-name", "z"),
        ]))
        .unwrap();
        assert_eq!(parsed.unsupported, vec!["entity-name", "_content"]);
        assert_eq!(parsed.used.len(), 11);
        let q = parsed.query;
        assert_eq!(q.dates.len(), 3);
        assert_eq!(q.dates[0][0].prefix, DatePrefix::Ge);
        assert_eq!(q.refs.len(), 3);
        assert_eq!(
            q.refs[0].values[0],
            RefValue {
                ref_type: Some("Patient".into()),
                ref_id: "p1".into()
            }
        );
        assert_eq!(q.refs[1].values.len(), 2);
        assert_eq!(q.refs[2].values[0].ref_type, None);
        assert_eq!(q.tokens.len(), 4);
        assert!(q.tokens[3].negate);
        assert_eq!(q.ids, Some(vec!["a".to_string(), "b".to_string()]));
        assert!(!q.is_unfiltered());
        assert!(parse_search(&[]).unwrap().query.is_unfiltered());
    }

    #[test]
    fn bad_values_and_modifiers_are_client_errors() {
        for pairs in [
            [("date", "soon")],
            [("patient:identifier", "x")],
            [("action:text", "C")],
            [("_id:missing", "true")],
        ] {
            assert!(
                matches!(parse_search(&p(&pairs)), Err(AuditReadError::BadRequest(_))),
                "{pairs:?}"
            );
        }
    }

    #[test]
    fn matching_follows_fhir_semantics() {
        let row = AuditRow::from_event(event(), Utc::now());
        let m = |pairs: &[(&str, &str)]| matches(&parse_search(&p(pairs)).unwrap().query, &row);
        assert!(m(&[]));
        assert!(m(&[("date", "2026-11-20")]));
        assert!(m(&[
            ("date", "ge2026-11-20T10:11:12Z"),
            ("date", "le2026-11-20")
        ]));
        assert!(!m(&[("date", "gt2026-11-20")]));
        assert!(!m(&[("date", "lt2026-11-20")]));
        assert!(m(&[("date", "ne2026-11-21")]));
        assert!(m(&[("date", "2025,2026")]));
        assert!(m(&[("patient", "p1")]));
        assert!(m(&[("patient", "Patient/p1")]));
        assert!(!m(&[("patient", "o1")]), "an Observation is not a patient");
        assert!(m(&[("entity", "Observation/o1")]));
        assert!(m(&[("entity", "o1")]));
        assert!(!m(&[("entity", "Encounter/o1")]));
        assert!(!m(&[("entity", "Device/hfs")]), "the device is an agent");
        assert!(
            !m(&[("agent", "Device/hfs")]),
            "the observer is not indexed as an agent"
        );
        assert!(m(&[("agent", "user-7")]), "identifier value");
        assert!(m(&[("entity", "req-1")]), "X-Request-Id entity");
        assert!(m(&[("action", "C")]));
        assert!(m(&[("action", "R,C")]));
        assert!(!m(&[("action", "R")]));
        assert!(m(&[("action:not", "R")]));
        assert!(m(&[("outcome", "0")]));
        assert!(!m(&[("outcome:not", "0")]));
        assert!(m(&[("type", "rest")]));
        assert!(m(&[(
            "type",
            "http://terminology.hl7.org/CodeSystem/audit-event-type|rest"
        )]));
        assert!(!m(&[("type", "http://other|rest")]));
        assert!(m(&[("subtype", "create")]));
        assert!(m(&[(
            "subtype",
            "http://hl7.org/fhir/restful-interaction|"
        )]));
        assert!(!m(&[("subtype", "delete")]));
        assert!(m(&[("_id", "0193a5c0-0000-7000-8000-000000000001,zz")]));
        assert!(!m(&[("_id", "zz")]));
        assert!(m(&[("patient", "p1"), ("entity", "o1"), ("action", "C")]));
        assert!(!m(&[("patient", "p1"), ("entity", "o9")]));
    }

    #[test]
    fn stored_events_get_version_one_and_last_updated() {
        let s = StoredAuditEvent::new("x".into(), t("2026-11-20T10:11:12.345Z"), event());
        assert_eq!(s.event["meta"]["versionId"], "1");
        assert_eq!(s.event["meta"]["lastUpdated"], "2026-11-20T10:11:12.345Z");
        assert_eq!(s.event["action"], "C");
    }
}
