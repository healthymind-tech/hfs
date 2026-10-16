//! Backend-neutral matching for the FHIR `string` search parameter type.
//!
//! SQLite and PostgreSQL do not provide identical Unicode case and accent
//! handling. HTS therefore normalizes metadata in Rust and applies pagination
//! after matching. This module intentionally does not normalize punctuation or
//! non-significant whitespace, which FHIR describes as recommended behavior.

use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

use crate::types::ResourceSearchQuery;

/// FHIR `string` matching modes supported by HTS.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FhirStringSearchMode {
    /// Case- and accent-insensitive prefix matching.
    #[default]
    Prefix,
    /// Case- and accent-insensitive substring matching.
    Contains,
    /// Equality of the decoded Unicode scalar-value sequence.
    Exact,
}

/// A compiled search value. The normalized query is computed only once.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FhirStringSearch {
    value: String,
    normalized: String,
    mode: FhirStringSearchMode,
}

impl FhirStringSearch {
    /// Compile a non-empty effective search value.
    ///
    /// Values that are empty before or after normalization are ignored. In
    /// particular, a value containing only combining marks must not turn a
    /// `contains` search into a predicate that matches every resource.
    pub(crate) fn new(value: &str, mode: FhirStringSearchMode) -> Option<Self> {
        if value.is_empty() {
            return None;
        }
        let normalized = normalize(value);
        if normalized.is_empty() {
            return None;
        }
        Some(Self {
            value: value.to_owned(),
            normalized,
            mode,
        })
    }

    /// Whether a present candidate satisfies this search value.
    pub(crate) fn matches(&self, candidate: &str) -> bool {
        match self.mode {
            FhirStringSearchMode::Prefix => normalize(candidate).starts_with(&self.normalized),
            FhirStringSearchMode::Contains => normalize(candidate).contains(&self.normalized),
            FhirStringSearchMode::Exact => candidate == self.value,
        }
    }
}

/// The compiled search predicates HTS applies in Rust over metadata rows:
/// FHIR `string` matching of `name`/`title`/`publisher` plus the exact and
/// date predicates (`_id`, `system`, `supplements`, `content-mode`,
/// `_lastUpdated`) that the backends do not express in SQL.
///
/// `url`, `version` and `status` stay SQL predicates.
pub(crate) struct ResourceStringSearch {
    name: Option<FhirStringSearch>,
    title: Option<FhirStringSearch>,
    publisher: Option<FhirStringSearch>,
    id: Option<String>,
    system: Option<String>,
    supplements: Option<String>,
    content_mode: Option<String>,
    last_updated: Vec<DateSearch>,
}

impl ResourceStringSearch {
    /// Compile each query value exactly once.
    pub(crate) fn new(query: &ResourceSearchQuery) -> Self {
        Self {
            name: query
                .name
                .as_deref()
                .and_then(|value| FhirStringSearch::new(value, query.name_mode)),
            title: query
                .title
                .as_deref()
                .and_then(|value| FhirStringSearch::new(value, query.title_mode)),
            publisher: query
                .publisher
                .as_deref()
                .and_then(|value| FhirStringSearch::new(value, query.publisher_mode)),
            id: query.id.clone(),
            system: query.system.clone(),
            supplements: query.supplements.clone(),
            content_mode: query.content_mode.clone(),
            last_updated: query.last_updated.clone(),
        }
    }

    /// `true` when no predicate needs the Rust-side filter, so the backends
    /// may page in SQL.
    pub(crate) fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.title.is_none()
            && self.publisher.is_none()
            && self.id.is_none()
            && self.system.is_none()
            && self.supplements.is_none()
            && self.content_mode.is_none()
            && self.last_updated.is_empty()
    }

    /// Whether the metadata rows must carry `publisher` (a JSON-derived
    /// column; selecting it otherwise would cost a JSON read per row).
    pub(crate) fn needs_publisher(&self) -> bool {
        self.publisher.is_some()
    }

    /// Whether the metadata rows must carry `supplements`.
    pub(crate) fn needs_supplements(&self) -> bool {
        self.supplements.is_some()
    }

    /// Whether a resource with this `name` and `title` satisfies the
    /// populated name/title predicates. A missing value never matches a
    /// populated one.
    pub(crate) fn matches(&self, name: Option<&str>, title: Option<&str>) -> bool {
        satisfies(&self.name, name) && satisfies(&self.title, title)
    }

    /// Whether a metadata row satisfies every populated predicate.
    pub(crate) fn matches_row(&self, row: &ResourceSearchRow) -> bool {
        self.matches(row.name.as_deref(), row.title.as_deref())
            && satisfies(&self.publisher, row.publisher.as_deref())
            && self.id.as_deref().is_none_or(|id| {
                row.id == id
                    || row
                        .id
                        .strip_prefix(id)
                        .is_some_and(|rest| rest.starts_with('|'))
            })
            && self.system.as_deref().is_none_or(|system| row.url == system)
            && self.content_mode.as_deref().is_none_or(|mode| row.content.as_deref() == Some(mode))
            && self.supplements.as_deref().is_none_or(|wanted| {
                row.supplements
                    .as_deref()
                    .is_some_and(|actual| canonical_matches(wanted, actual))
            })
            && (self.last_updated.is_empty() || {
                let instant = row.updated_at.as_deref().and_then(parse_stored_instant);
                instant.is_some_and(|t| self.last_updated.iter().all(|d| d.matches(t)))
            })
    }
}

fn satisfies(search: &Option<FhirStringSearch>, candidate: Option<&str>) -> bool {
    search
        .as_ref()
        .is_none_or(|search| candidate.is_some_and(|candidate| search.matches(candidate)))
}

/// Canonical reference matching for `supplements`: a bare `url` matches the
/// stored canonical with or without a `|version`; `url|version` matches only
/// that version.
fn canonical_matches(wanted: &str, actual: &str) -> bool {
    if wanted.contains('|') {
        return actual == wanted;
    }
    actual == wanted
        || actual
            .strip_prefix(wanted)
            .is_some_and(|rest| rest.starts_with('|'))
}

/// FHIR search prefixes for `date` parameters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatePrefix {
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
    Sa,
    Eb,
    Ap,
}

/// One compiled `date` search value: a prefix and the implicit range
/// `[low, high)` the value's precision covers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DateSearch {
    prefix: DatePrefix,
    low: chrono::DateTime<chrono::Utc>,
    high: chrono::DateTime<chrono::Utc>,
}

impl DateSearch {
    /// Parse `[prefix]value` where value is a FHIR date or dateTime
    /// (`YYYY`, `YYYY-MM`, `YYYY-MM-DD`, `YYYY-MM-DDThh:mm[:ss[.fff]][zone]`).
    /// A dateTime without a zone is taken as UTC.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let (prefix, value) = match raw.get(..2) {
            Some(p) if p.chars().all(|c| c.is_ascii_alphabetic()) => {
                let prefix = match p {
                    "eq" => DatePrefix::Eq,
                    "ne" => DatePrefix::Ne,
                    "gt" => DatePrefix::Gt,
                    "lt" => DatePrefix::Lt,
                    "ge" => DatePrefix::Ge,
                    "le" => DatePrefix::Le,
                    "sa" => DatePrefix::Sa,
                    "eb" => DatePrefix::Eb,
                    "ap" => DatePrefix::Ap,
                    other => return Err(format!("unknown date prefix `{other}`")),
                };
                (prefix, &raw[2..])
            }
            _ => (DatePrefix::Eq, raw),
        };
        let (low, high) =
            date_range(value).ok_or_else(|| format!("`{value}` is not a valid FHIR date"))?;
        Ok(Self { prefix, low, high })
    }

    /// Whether the instant `t` satisfies this predicate.
    pub fn matches(&self, t: chrono::DateTime<chrono::Utc>) -> bool {
        let within = self.low <= t && t < self.high;
        match self.prefix {
            DatePrefix::Eq | DatePrefix::Ap => within,
            DatePrefix::Ne => !within,
            DatePrefix::Gt | DatePrefix::Sa => t >= self.high,
            DatePrefix::Lt | DatePrefix::Eb => t < self.low,
            DatePrefix::Ge => t >= self.low,
            DatePrefix::Le => t < self.high,
        }
    }
}

fn date_range(
    value: &str,
) -> Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    use chrono::{Datelike, Duration, NaiveDate, TimeZone, Utc};

    let utc_day = |d: NaiveDate| -> Option<chrono::DateTime<Utc>> {
        Some(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?))
    };
    let bytes = value.as_bytes();
    match value.len() {
        4 if bytes.iter().all(u8::is_ascii_digit) => {
            let year: i32 = value.parse().ok()?;
            Some((
                utc_day(NaiveDate::from_ymd_opt(year, 1, 1)?)?,
                utc_day(NaiveDate::from_ymd_opt(year + 1, 1, 1)?)?,
            ))
        }
        7 => {
            let first = NaiveDate::parse_from_str(&format!("{value}-01"), "%Y-%m-%d").ok()?;
            let next = if first.month() == 12 {
                NaiveDate::from_ymd_opt(first.year() + 1, 1, 1)?
            } else {
                NaiveDate::from_ymd_opt(first.year(), first.month() + 1, 1)?
            };
            Some((utc_day(first)?, utc_day(next)?))
        }
        10 => {
            let day = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
            Some((utc_day(day)?, utc_day(day.succ_opt()?)?))
        }
        _ => {
            let (date_time, zone) = split_zone(value);
            let (clock, fraction) = match date_time.split_once('.') {
                Some((clock, fraction)) => (clock, Some(fraction)),
                None => (date_time, None),
            };
            let (naive, step) = match clock.len() {
                16 => (
                    chrono::NaiveDateTime::parse_from_str(clock, "%Y-%m-%dT%H:%M").ok()?,
                    Duration::minutes(1),
                ),
                19 => (
                    chrono::NaiveDateTime::parse_from_str(clock, "%Y-%m-%dT%H:%M:%S").ok()?,
                    Duration::seconds(1),
                ),
                _ => return None,
            };
            let (naive, step) = match fraction {
                None => (naive, step),
                Some(f) if !f.is_empty() && f.len() <= 9 && f.bytes().all(|b| b.is_ascii_digit()) => {
                    if clock.len() != 19 {
                        return None;
                    }
                    let nanos: i64 = format!("{f:0<9}").parse().ok()?;
                    let unit = 10_i64.pow(9 - f.len() as u32);
                    (
                        naive + Duration::nanoseconds(nanos),
                        Duration::nanoseconds(unit),
                    )
                }
                Some(_) => return None,
            };
            let offset = match zone {
                None | Some("Z") => chrono::FixedOffset::east_opt(0)?,
                Some(z) => {
                    let sign = match z.as_bytes().first()? {
                        b'+' => 1,
                        b'-' => -1,
                        _ => return None,
                    };
                    let (h, m) = z[1..].split_once(':')?;
                    let secs = h.parse::<i32>().ok()? * 3600 + m.parse::<i32>().ok()? * 60;
                    chrono::FixedOffset::east_opt(sign * secs)?
                }
            };
            let low = offset
                .from_local_datetime(&naive)
                .single()?
                .with_timezone(&Utc);
            Some((low, low + step))
        }
    }
}

/// Split a trailing `Z` or `±hh:mm` zone off a dateTime.
fn split_zone(value: &str) -> (&str, Option<&str>) {
    if let Some(rest) = value.strip_suffix('Z') {
        return (rest, Some("Z"));
    }
    if value.len() > 6 {
        let at = value.len() - 6;
        let tail = &value[at..];
        if (tail.starts_with('+') || tail.starts_with('-')) && tail.as_bytes()[3] == b':' {
            return (&value[..at], Some(tail));
        }
    }
    (value, None)
}

/// Parse a stored `updated_at`/`created_at` timestamp (RFC 3339 as written
/// by HTS, or SQLite's `YYYY-MM-DD HH:MM:SS[.fff]`, taken as UTC).
fn parse_stored_instant(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::TimeZone;
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(t.with_timezone(&chrono::Utc));
    }
    ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"]
        .iter()
        .find_map(|format| chrono::NaiveDateTime::parse_from_str(raw, format).ok())
        .map(|naive| chrono::Utc.from_utc_datetime(&naive))
}

/// Metadata needed to apply the Rust-side filters before pagination.
#[derive(Clone, Debug, Default)]
pub(crate) struct ResourceSearchRow {
    pub id: String,
    pub url: String,
    pub version: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub status: String,
    /// CodeSystem `content` (NULL for other resource types).
    pub content: Option<String>,
    /// CodeSystem `supplements` (selected only when filtered on).
    pub supplements: Option<String>,
    /// `publisher` (selected only when filtered on).
    pub publisher: Option<String>,
    /// Last-modified timestamp (`updated_at`; `created_at` for ConceptMap).
    pub updated_at: Option<String>,
}

/// Apply the Rust-side filters and then `_offset`/`_count`.
pub(crate) fn filter_rows(
    rows: impl IntoIterator<Item = ResourceSearchRow>,
    query: &ResourceSearchQuery,
    search: &ResourceStringSearch,
) -> Vec<ResourceSearchRow> {
    let offset = query.offset.unwrap_or(0) as usize;
    let count = query.count.unwrap_or(20) as usize;

    rows.into_iter()
        .filter(|row| search.matches_row(row))
        .skip(offset)
        .take(count)
        .collect()
}

/// The metadata select list for `table` (see [`ResourceSearchRow`]): `id, url,
/// version, name, title, status, content, supplements, publisher,
/// updated_at`. Columns a table lacks, or that the search does not need,
/// are `NULL` — `supplements`/`publisher` are derived from `resource_json`,
/// so reading them for every row costs a JSON read per row.
///
/// `publisher` is a generated column on every SQLite table and on
/// PostgreSQL's `code_systems`; PostgreSQL's `value_sets`/`concept_maps`
/// (hundreds of MB of JSON on a production server) read it from the JSON on
/// demand instead, so adding the column never rewrites those tables.
pub(crate) fn metadata_select_list(
    table: &str,
    search: &ResourceStringSearch,
    postgres: bool,
) -> String {
    let content = if table == "code_systems" { "content" } else { "NULL" };
    let supplements = if table == "code_systems" && search.needs_supplements() {
        "supplements"
    } else {
        "NULL"
    };
    let publisher = match (search.needs_publisher(), postgres && table != "code_systems") {
        (false, _) => "NULL",
        (true, false) => "publisher",
        (true, true) => "resource_json->>'publisher'",
    };
    let updated = if table == "concept_maps" { "created_at" } else { "updated_at" };
    format!(
        "id, url, version, name, title, status, {content}, {supplements}, {publisher}, {updated}"
    )
}

/// Normalize case and combining marks consistently across storage backends.
fn normalize(value: &str) -> String {
    value
        .nfd()
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .nfd()
        .filter(|character| !is_combining_mark(*character))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_and_contains_are_case_and_accent_insensitive() {
        let prefix = FhirStringSearch::new("cafe", FhirStringSearchMode::Prefix).unwrap();
        assert!(prefix.matches("Café terminology"));
        assert!(prefix.matches("Cafe\u{301} terminology"));
        assert!(!prefix.matches("Terminology café"));

        let contains = FhirStringSearch::new("CAFE", FhirStringSearchMode::Contains).unwrap();
        assert!(contains.matches("Terminology café"));
    }

    #[test]
    fn normalization_handles_non_ascii_case_expansion() {
        let search = FhirStringSearch::new("STRASSE", FhirStringSearchMode::Prefix).unwrap();
        assert!(search.matches("Straße terminology"));
    }

    #[test]
    fn exact_preserves_case_accents_and_unicode_sequence() {
        let search = FhirStringSearch::new("Café", FhirStringSearchMode::Exact).unwrap();
        assert!(search.matches("Café"));
        assert!(!search.matches("café"));
        assert!(!search.matches("Cafe\u{301}"));
        assert!(!search.matches("Cafe"));
    }

    #[test]
    fn empty_and_normalization_empty_values_are_ignored() {
        assert!(FhirStringSearch::new("", FhirStringSearchMode::Prefix).is_none());
        assert!(FhirStringSearch::new("\u{301}", FhirStringSearchMode::Contains).is_none());
    }

    #[test]
    fn missing_fields_do_not_match_and_pagination_follows_filtering() {
        let query = ResourceSearchQuery {
            name: Some("match".to_owned()),
            count: Some(1),
            offset: Some(1),
            ..ResourceSearchQuery::default()
        };
        let rows = [None, Some("MatchOne"), Some("other"), Some("matchTwo")]
            .into_iter()
            .enumerate()
            .map(|(index, name)| ResourceSearchRow {
                id: index.to_string(),
                url: format!("http://example.org/{index}"),
                name: name.map(str::to_owned),
                status: "active".to_owned(),
                ..ResourceSearchRow::default()
            });

        let search = ResourceStringSearch::new(&query);
        let selected = filter_rows(rows, &query, &search);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name.as_deref(), Some("matchTwo"));
    }

    fn instant(raw: &str) -> chrono::DateTime<chrono::Utc> {
        parse_stored_instant(raw).unwrap()
    }

    #[test]
    fn date_search_ranges_follow_precision() {
        let t = instant("2026-10-15T12:34:56.789+00:00");
        for (query, expected) in [
            ("2026", true),
            ("2025", false),
            ("2026-10", true),
            ("eq2026-10-15", true),
            ("ne2026-10-15", false),
            ("gt2026-10-14", true),
            ("gt2026-10-15", false),
            ("ge2026-10-15", true),
            ("lt2026-10-16", true),
            ("lt2026-10-15", false),
            ("le2026-10-15", true),
            ("2026-10-15T12:34", true),
            ("2026-10-15T12:34:56", true),
            ("2026-10-15T12:34:57", false),
            ("2026-10-15T12:34:56.7", true),
            ("2026-10-15T12:34:56.8", false),
            ("2026-10-15T20:34:56+08:00", true),
            ("lt2026-10-15T12:34:57Z", true),
            ("sa2026-12", false),
            ("eb2027", true),
        ] {
            let search = DateSearch::parse(query).unwrap();
            assert_eq!(search.matches(t), expected, "{query}");
        }
    }

    #[test]
    fn date_search_rejects_malformed_values() {
        for bad in ["", "yesterday", "xx2026", "2026-13", "2026-10-15T25:00", "2026-1", "20261015"] {
            assert!(DateSearch::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn stored_timestamps_parse_in_both_formats() {
        assert_eq!(
            instant("2026-10-15 12:00:00"),
            instant("2026-10-15T12:00:00+00:00")
        );
        assert_eq!(
            instant("2026-10-15T20:00:00+08:00"),
            instant("2026-10-15T12:00:00Z")
        );
    }

    #[test]
    fn supplements_canonical_matching() {
        assert!(canonical_matches("http://loinc.org", "http://loinc.org"));
        assert!(canonical_matches("http://loinc.org", "http://loinc.org|2.80"));
        assert!(!canonical_matches("http://loinc.org", "http://loinc.org/extra"));
        assert!(canonical_matches("http://loinc.org|2.80", "http://loinc.org|2.80"));
        assert!(!canonical_matches("http://loinc.org|2.80", "http://loinc.org"));
    }
}
