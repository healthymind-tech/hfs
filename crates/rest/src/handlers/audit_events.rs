//! The virtual `AuditEvent` resource (`HFS_AUDIT_STORE=table`).
//!
//! With the dedicated audit store an `AuditEvent` is not a row of the FHIR
//! resource tables, so the generic read and search paths cannot see it. This
//! module serves `read`, `vread` (version `1`, the only one an immutable
//! event has) and type-level search from the store
//! ([`helios_audit::store::AuditEventReader`]), in the tenant of the request.
//!
//! Search answers a fixed parameter set
//! ([`helios_audit::store::SUPPORTED_SEARCH_PARAMS`]) plus `_count`, `_sort`
//! by `date` / `_lastUpdated`, `_total`, `_summary`, `_elements` and paging.
//! Anything else is an unknown parameter: ignored with a warning, or a `400`
//! under `Prefer: handling=strict`.
//!
//! During the transition the trail has two sources: the store, and the
//! `AuditEvent` resources written before it existed. Every read merges both,
//! in `recorded` order, until the legacy resources have been moved
//! (`HFS_AUDIT_BACKFILL`) or have expired.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use chrono::{DateTime, Utc};
use helios_audit::store::{
    AuditEventReader, AuditQuery, AuditReadError, RefField, RefFilter, RefValue,
    SUPPORTED_SEARCH_PARAMS, StoredAuditEvent, parse_search,
};
use helios_fhir::FhirVersion;
use helios_persistence::core::{ResourceStorage, SearchProvider, SearchResult};
use helios_persistence::types::{Page, PageInfo, StoredResource};
use serde::{Deserialize, Serialize};

use crate::error::{RestError, RestResult};
use crate::extractors::{SearchParams, TenantExtractor};
use crate::patient_compartment::PatientCompartment;
use crate::responses::subsetting::SummaryMode;
use crate::state::AppState;

/// The resource type this module serves.
pub const AUDIT_EVENT: &str = "AuditEvent";

/// Whether `AuditEvent` takes part in searches that walk a whole compartment
/// without naming their types (`Patient/{id}/$everything`, `Patient/{id}/*`).
///
/// `AuditEvent` is a member of the R4 Patient compartment, but the record of
/// who accessed a chart is not part of the chart: by default such a walk
/// leaves it out. A request that names it (`_type=AuditEvent`) still gets it,
/// and `HFS_COMPARTMENT_INCLUDE_AUDIT_EVENTS=true` restores the old default.
pub fn in_unqualified_compartment_walks() -> bool {
    static INCLUDE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *INCLUDE.get_or_init(|| {
        std::env::var("HFS_COMPARTMENT_INCLUDE_AUDIT_EVENTS").is_ok_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
    })
}

/// Whether a compartment walk over `types` (the request's `_type`, if any)
/// covers `member`.
pub(crate) fn walk_includes(member: &str, types: Option<&[String]>) -> bool {
    member != AUDIT_EVENT
        || in_unqualified_compartment_walks()
        || types.is_some_and(|t| t.iter().any(|t| t == AUDIT_EVENT))
}

/// How long the answer to "does this tenant still have legacy AuditEvent
/// resources" is reused.
const LEGACY_PROBE_TTL: Duration = Duration::from_secs(30);

/// The audit store as the REST layer uses it.
pub struct AuditEventAdapter {
    reader: Arc<dyn AuditEventReader>,
    /// Per tenant: when it was probed and whether legacy events exist.
    legacy: parking_lot::Mutex<HashMap<String, (Instant, bool)>>,
}

impl AuditEventAdapter {
    /// Adapter over `reader`.
    pub fn new(reader: Arc<dyn AuditEventReader>) -> Self {
        Self {
            reader,
            legacy: parking_lot::Mutex::new(HashMap::new()),
        }
    }
}

fn read_error(e: AuditReadError) -> RestError {
    match e {
        AuditReadError::BadRequest(message) => RestError::BadRequest { message },
        AuditReadError::Storage(message) => RestError::InternalError { message },
    }
}

fn to_stored(tenant: &TenantExtractor, event: StoredAuditEvent) -> StoredResource {
    StoredResource::from_storage(
        AUDIT_EVENT,
        event.id,
        "1",
        tenant.context().tenant_id().clone(),
        event.event,
        event.recorded,
        event.recorded,
        None,
        FhirVersion::default(),
    )
}

/// The event `id` from the audit store, if the store is in use and has it.
/// `None` sends the caller to the resource tables (legacy events).
pub(crate) async fn read<S>(
    state: &AppState<S>,
    tenant: &TenantExtractor,
    resource_type: &str,
    id: &str,
) -> RestResult<Option<StoredResource>>
where
    S: ResourceStorage,
{
    if resource_type != AUDIT_EVENT {
        return Ok(None);
    }
    let Some(adapter) = state.audit_events() else {
        return Ok(None);
    };
    let event = adapter
        .reader
        .read(tenant.tenant_id(), id)
        .await
        .map_err(read_error)?;
    Ok(event.map(|e| to_stored(tenant, e)))
}

/// Position of a page in the merged trail.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
struct Cursor {
    /// `recorded` of the last store event already returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    r: Option<DateTime<Utc>>,
    /// Its id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    i: Option<String>,
    /// Legacy events already returned.
    #[serde(default)]
    l: u32,
}

impl Cursor {
    fn encode(&self) -> String {
        let json = serde_json::to_vec(self).unwrap_or_default();
        format!(
            "ae1.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
        )
    }

    fn decode(value: &str) -> Option<Self> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(value.strip_prefix("ae1.")?)
            .ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

/// Result parameters handled here rather than treated as criteria.
const RESULT_PARAMS: &[&str] = &[
    "_count",
    "_sort",
    "_cursor",
    "_offset",
    "_total",
    "_summary",
    "_elements",
    "_format",
    "_pretty",
];

fn recorded_of(resource: &StoredResource) -> DateTime<Utc> {
    resource
        .content()
        .get("recorded")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|| resource.last_modified())
}

/// A page of the merged trail.
struct Merged {
    items: Vec<StoredResource>,
    /// `(recorded, id)` of the last store event in `items`.
    last_table: Option<(DateTime<Utc>, String)>,
    /// Legacy events in `items`.
    from_legacy: usize,
    /// Whether either stream had more than fitted.
    more: bool,
}

/// Merges two streams already in the requested order; on equal instants the
/// store's events come first. Keeps the first `count` items.
fn merge(
    table: Vec<StoredResource>,
    legacy: Vec<StoredResource>,
    count: usize,
    ascending: bool,
) -> Merged {
    let total = table.len() + legacy.len();
    let mut table = table.into_iter().peekable();
    let mut legacy = legacy.into_iter().peekable();
    let mut merged = Merged {
        items: Vec::with_capacity(count.min(total)),
        last_table: None,
        from_legacy: 0,
        more: total > count,
    };
    while merged.items.len() < count {
        let take_table = match (table.peek(), legacy.peek()) {
            (Some(t), Some(l)) => {
                let (t, l) = (recorded_of(t), recorded_of(l));
                if ascending { t <= l } else { t >= l }
            }
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        if take_table {
            if let Some(r) = table.next() {
                // A store row's `last_modified` is its exact stored `recorded`.
                merged.last_table = Some((r.last_modified(), r.id().to_string()));
                merged.items.push(r);
            }
        } else {
            merged.items.extend(legacy.next());
            merged.from_legacy += 1;
        }
    }
    merged
}

async fn has_legacy<S>(
    adapter: &AuditEventAdapter,
    state: &AppState<S>,
    tenant: &TenantExtractor,
) -> bool
where
    S: ResourceStorage + SearchProvider + Send + Sync,
{
    let key = tenant.tenant_id().to_string();
    if let Some((at, value)) = adapter.legacy.lock().get(&key).copied()
        && at.elapsed() < LEGACY_PROBE_TTL
    {
        return value;
    }
    let probe = helios_persistence::types::SearchQuery::new(AUDIT_EVENT).with_count(1);
    // A failing probe must not hide events: assume there are some.
    let value = match state.storage().search(tenant.context(), &probe).await {
        Ok(result) => !result.resources.items.is_empty(),
        Err(_) => true,
    };
    adapter.legacy.lock().insert(key, (Instant::now(), value));
    value
}

/// The searchset Bundle of an `AuditEvent` search, or `None` when the audit
/// store is not in use (the generic search then answers).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn search_bundle<S>(
    state: &AppState<S>,
    tenant: &TenantExtractor,
    compartment: &PatientCompartment,
    resource_type: &str,
    pairs: &[(String, String)],
    strict: bool,
) -> RestResult<Option<serde_json::Value>>
where
    S: ResourceStorage + SearchProvider + Send + Sync,
{
    if resource_type != AUDIT_EVENT {
        return Ok(None);
    }
    let Some(adapter) = state.audit_events() else {
        return Ok(None);
    };
    let pairs = crate::extractors::drop_empty_parameters(pairs.to_vec());
    let last = |name: &str| {
        pairs
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let mut warnings: Vec<String> = Vec::new();

    // Criteria.
    let criteria: Vec<(String, String)> = pairs
        .iter()
        .filter(|(k, _)| !RESULT_PARAMS.contains(&k.as_str()))
        .cloned()
        .collect();
    let parsed = parse_search(&criteria).map_err(read_error)?;
    if !parsed.unsupported.is_empty() {
        let message = format!(
            "search parameter(s) not supported on AuditEvent: {} (supported: {})",
            parsed.unsupported.join(", "),
            SUPPORTED_SEARCH_PARAMS.join(", ")
        );
        if strict {
            return Err(RestError::InvalidParameter {
                param: parsed.unsupported.join(", "),
                message,
            });
        }
        warnings.push(format!("{message}; ignored"));
    }
    let mut query: AuditQuery = parsed.query;
    let mut shown = parsed.used.clone();

    // A `patient/` scope sees the launch patient's events only.
    if let Some(patient_id) = compartment.patient_id() {
        query.refs.push(RefFilter {
            field: RefField::Patient,
            values: vec![RefValue {
                ref_type: Some("Patient".to_string()),
                ref_id: patient_id.to_string(),
            }],
        });
    }

    // Result parameters.
    let mut sort_param: Option<&str> = None;
    if let Some(sort) = last("_sort") {
        match sort {
            "date" | "_lastUpdated" => {
                query.ascending = true;
                sort_param = Some(sort);
            }
            "-date" | "-_lastUpdated" => sort_param = Some(sort),
            other => {
                let message = format!(
                    "_sort value '{other}' is not supported on AuditEvent (supported: date, \
                     -date, _lastUpdated, -_lastUpdated)"
                );
                if strict {
                    return Err(RestError::InvalidParameter {
                        param: "_sort".to_string(),
                        message,
                    });
                }
                warnings.push(format!("{message}; results are sorted by -date"));
            }
        }
    }
    if let Some(sort) = sort_param {
        shown.push(("_sort".to_string(), sort.to_string()));
    }
    let requested_count = match last("_count") {
        Some(v) => Some(v.parse::<usize>().map_err(|_| RestError::InvalidParameter {
            param: "_count".to_string(),
            message: format!("invalid _count value '{v}'"),
        })?),
        None => None,
    };
    let count = requested_count
        .unwrap_or(state.default_page_size())
        .min(state.max_page_size());
    if let Some(v) = last("_count") {
        shown.push(("_count".to_string(), v.to_string()));
    }
    let summary = last("_summary").and_then(SummaryMode::parse);
    let total_mode = last("_total");
    if let Some(v) = total_mode
        && !matches!(v, "none" | "estimate" | "accurate")
    {
        let message = format!("_total value '{v}' is not one of 'none', 'estimate' or 'accurate'");
        if strict {
            return Err(RestError::InvalidParameter {
                param: "_total".to_string(),
                message,
            });
        }
        warnings.push(format!("{message}; it was ignored"));
    }
    let count_only = summary == Some(SummaryMode::Count) || requested_count == Some(0);
    let want_total = matches!(total_mode, Some("accurate" | "estimate"))
        || (count_only && total_mode != Some("none"));
    for name in ["_total", "_summary", "_elements"] {
        if let Some(v) = last(name) {
            shown.push((name.to_string(), v.to_string()));
        }
    }
    let cursor = match last("_cursor") {
        Some(v) => Cursor::decode(v).ok_or_else(|| RestError::InvalidParameter {
            param: "_cursor".to_string(),
            message: "the paging cursor is not valid for an AuditEvent search".to_string(),
        })?,
        None => Cursor::default(),
    };
    if let Some(v) = last("_cursor") {
        shown.push(("_cursor".to_string(), v.to_string()));
    }

    let tenant_id = tenant.tenant_id();
    let legacy = has_legacy(adapter, state, tenant).await;

    // The legacy half of the query, built by the generic query builder.
    let legacy_query = |count: usize, offset: u32| -> RestResult<_> {
        let mut legacy_pairs = parsed.used.clone();
        legacy_pairs.push((
            "_sort".to_string(),
            if query.ascending { "date" } else { "-date" }.to_string(),
        ));
        legacy_pairs.push(("_count".to_string(), count.to_string()));
        if offset > 0 {
            legacy_pairs.push(("_offset".to_string(), offset.to_string()));
        }
        let reg = state.storage().search_param_registry(tenant.context());
        let registry = reg.read();
        let mut q = crate::extractors::build_search_query_for_version(
            AUDIT_EVENT,
            &SearchParams::from_pairs(legacy_pairs),
            &registry,
            FhirVersion::default(),
        )?;
        compartment.confine_query(FhirVersion::default(), &mut q)?;
        Ok(q)
    };

    let total = if want_total {
        let mut total = adapter
            .reader
            .count(tenant_id, &query)
            .await
            .map_err(read_error)?;
        if legacy {
            let q = legacy_query(1, 0)?;
            total += state.storage().search_count(tenant.context(), &q).await?;
        }
        Some(total)
    } else {
        None
    };

    let public_base = state.public_base_url_for_request(tenant);
    let self_link = super::search::build_search_url(
        &public_base,
        AUDIT_EVENT,
        &SearchParams::from_pairs(shown),
    );

    let mut page = Vec::new();
    let mut next_cursor = None;
    if !count_only && count > 0 {
        query.limit = count + 1;
        query.after = cursor.r.zip(cursor.i.clone());
        let table: Vec<StoredResource> = adapter
            .reader
            .search(tenant_id, &query)
            .await
            .map_err(read_error)?
            .into_iter()
            .map(|e| to_stored(tenant, e))
            .collect();
        let legacy_rows = if legacy {
            let q = legacy_query(count + 1, cursor.l)?;
            state
                .storage()
                .search(tenant.context(), &q)
                .await?
                .resources
                .items
        } else {
            Vec::new()
        };
        let merged = merge(table, legacy_rows, count, query.ascending);
        if merged.more {
            let (r, i) = match merged.last_table {
                Some((r, i)) => (Some(r), Some(i)),
                None => (cursor.r, cursor.i.clone()),
            };
            next_cursor = Some(
                Cursor {
                    r,
                    i,
                    l: cursor.l + merged.from_legacy as u32,
                }
                .encode(),
            );
        }
        page = merged.items;
    }

    let mut result = SearchResult::new(Page::new(
        page,
        PageInfo {
            has_next: next_cursor.is_some(),
            next_cursor,
            previous_cursor: None,
            total: None,
            has_previous: false,
        },
    ));
    result.total = total;
    let mut bundle = result.into_bundle_with_page_size(&public_base, &self_link, Some(count));
    crate::public_url::rewrite_bundle_full_urls(&mut bundle, |resource_type, id| {
        state.public_url_for_request(tenant, [resource_type, id])
    });
    let elements_owned = last("_elements").map(|v| v.to_string());
    let elements: Option<Vec<&str>> = elements_owned
        .as_deref()
        .map(|v| v.split(',').map(str::trim).collect());
    let summary = if count_only {
        Some(SummaryMode::Count)
    } else {
        summary
    };
    let mut json = super::search::bundle_to_json_with_subsetting(
        bundle,
        summary,
        want_total,
        elements.as_deref(),
        FhirVersion::default(),
    )?;
    if !count_only {
        for message in &warnings {
            super::search::append_warning_outcome(&mut json, message);
        }
    }
    Ok(Some(json))
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_persistence::tenant::TenantId;

    fn res(id: &str, recorded: &str, _table: bool) -> StoredResource {
        let at = DateTime::parse_from_rfc3339(recorded)
            .unwrap()
            .with_timezone(&Utc);
        StoredResource::from_storage(
            AUDIT_EVENT,
            id,
            "1",
            TenantId::new("t"),
            serde_json::json!({"resourceType": "AuditEvent", "id": id, "recorded": recorded}),
            at,
            at,
            None,
            FhirVersion::default(),
        )
    }

    fn ids(v: &[StoredResource]) -> Vec<&str> {
        v.iter().map(|r| r.id()).collect()
    }

    #[test]
    fn cursors_round_trip_and_reject_foreign_values() {
        let c = Cursor {
            r: Some(
                DateTime::parse_from_rfc3339("2026-11-20T10:00:00.123456Z")
                    .unwrap()
                    .into(),
            ),
            i: Some("abc".into()),
            l: 7,
        };
        assert_eq!(Cursor::decode(&c.encode()), Some(c));
        assert_eq!(
            Cursor::decode(&Cursor::default().encode()),
            Some(Cursor::default())
        );
        assert_eq!(Cursor::decode("eyJ2IjoxfQ"), None);
        assert_eq!(Cursor::decode("ae1.!!!"), None);
    }

    #[test]
    fn merging_keeps_order_counts_sources_and_reports_leftovers() {
        let table = vec![
            res("t3", "2026-11-03T00:00:00Z", true),
            res("t1", "2026-11-01T00:00:00Z", true),
        ];
        let legacy = vec![
            res("l3", "2026-11-03T00:00:00Z", false),
            res("l2", "2026-11-02T00:00:00Z", false),
        ];
        let m = merge(table.clone(), legacy.clone(), 3, false);
        assert_eq!(ids(&m.items), vec!["t3", "l3", "l2"], "store first on a tie");
        assert_eq!(m.last_table.as_ref().map(|k| k.1.as_str()), Some("t3"));
        assert_eq!((m.from_legacy, m.more), (2, true));
        let m = merge(table, legacy, 10, false);
        assert_eq!(ids(&m.items), vec!["t3", "l3", "l2", "t1"]);
        assert_eq!(m.last_table.as_ref().map(|k| k.1.as_str()), Some("t1"));
        assert_eq!((m.from_legacy, m.more), (2, false));

        let asc_t = vec![res("t1", "2026-11-01T00:00:00Z", true)];
        let asc_l = vec![
            res("l0", "2026-10-01T00:00:00Z", false),
            res("l2", "2026-11-02T00:00:00Z", false),
        ];
        let m = merge(asc_t, asc_l, 10, true);
        assert_eq!(ids(&m.items), vec!["l0", "t1", "l2"]);
        let m = merge(Vec::new(), Vec::new(), 5, false);
        assert!(walk_includes("Observation", None));
        assert!(!walk_includes(AUDIT_EVENT, None), "left out unless asked for");
        assert!(!walk_includes(AUDIT_EVENT, Some(&["Observation".to_string()])));
        assert!(walk_includes(AUDIT_EVENT, Some(&[AUDIT_EVENT.to_string()])));
        assert!(m.items.is_empty() && m.last_table.is_none() && m.from_legacy == 0 && !m.more);
    }
}
