//! Search plans chosen by how many rows each filter matches.
//!
//! SQLite materialises only a search's smallest membership and probes the rest
//! per candidate, reaching rows by rowid when that membership is selective (at
//! most 1,000 index rows) instead of walking the type in sort order. Every
//! shape, and the choice between them, must give exactly the answer the plain
//! plan gave: same members, same order, same pages, same count. The seed
//! crosses the 1,000-row threshold on purpose. Included by the SQLite test
//! binary via `#[path]`; generic over the backend.

use std::collections::HashSet;

use helios_fhir::FhirVersion;
use helios_persistence::core::{ResourceStorage, SearchProvider};
use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
use helios_persistence::types::{
    SearchParamType, SearchParameter, SearchQuery, SearchValue, StoredResource,
};
use serde_json::json;

const TOTAL: usize = 1_100;
const AMENDED: usize = 50;
const OF_P1: usize = 3;

/// `TOTAL` Observations with code 8867-4: the last `AMENDED` have status
/// `amended` (the rest `final`), and `OF_P1` of them, spread through the set,
/// belong to `Patient/p1` (the rest to `Patient/p2`).
async fn seed<S: ResourceStorage>(backend: &S, tenant_id: &str) -> TenantContext {
    let t = TenantContext::new(TenantId::new(tenant_id), TenantPermissions::full_access());
    for i in 0..TOTAL {
        let status = if i >= TOTAL - AMENDED {
            "amended"
        } else {
            "final"
        };
        let patient = if i % (TOTAL / OF_P1) == 7 { "p1" } else { "p2" };
        let id = format!("obs-{i:04}");
        backend
            .create_or_update(
                &t,
                "Observation",
                &id,
                json!({
                    "resourceType": "Observation",
                    "id": id,
                    "status": status,
                    "code": {"coding": [{"system": "http://loinc.org", "code": "8867-4"}]},
                    "subject": {"reference": format!("Patient/{patient}")}
                }),
                FhirVersion::default(),
            )
            .await
            .unwrap();
    }
    t
}

fn param(name: &str, param_type: SearchParamType, value: &str) -> SearchParameter {
    SearchParameter {
        name: name.to_string(),
        param_type,
        modifier: None,
        values: vec![SearchValue::eq(value)],
        chain: vec![],
        components: vec![],
    }
}

fn code() -> SearchParameter {
    param("code", SearchParamType::Token, "http://loinc.org|8867-4")
}

fn query(params: Vec<SearchParameter>, count: u32) -> SearchQuery {
    let mut q = SearchQuery::new("Observation");
    for p in params {
        q = q.with_parameter(p);
    }
    q.with_count(count)
}

fn ids(items: &[StoredResource]) -> Vec<String> {
    items.iter().map(|r| r.id().to_string()).collect()
}

/// Every page of `q`, following cursors, in order.
async fn all_pages<S: SearchProvider>(
    backend: &S,
    t: &TenantContext,
    q: &SearchQuery,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut q = q.clone();
    loop {
        let page = backend.search(t, &q).await.unwrap();
        out.extend(ids(&page.resources.items));
        match page.resources.page_info.next_cursor {
            Some(c) if page.resources.page_info.has_next => q.cursor = Some(c),
            _ => break,
        }
    }
    out
}

/// The default order is `last_updated DESC, id ASC`.
fn assert_default_order(items: &[StoredResource]) {
    for pair in items.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        assert!(
            a.last_modified() > b.last_modified()
                || (a.last_modified() == b.last_modified() && a.id() < b.id()),
            "{} before {} breaks last_updated DESC, id ASC",
            a.id(),
            b.id()
        );
    }
}

/// Every scenario on one seeded tenant (seeding is the slow part on
/// PostgreSQL).
pub async fn filter_plans_keep_the_answer<S>(backend: &S, tenant_id: &str)
where
    S: ResourceStorage + SearchProvider,
{
    let t = seed(backend, tenant_id).await;
    broad_and_selective_filters_give_the_same_members_order_and_count(backend, &t).await;
    a_driving_filter_still_excludes_deleted_resources(backend, &t).await;
}

async fn broad_and_selective_filters_give_the_same_members_order_and_count<S>(
    backend: &S,
    t: &TenantContext,
) where
    S: ResourceStorage + SearchProvider,
{
    let status = |v| param("status", SearchParamType::Token, v);
    let patient = |v| param("subject", SearchParamType::Reference, v);

    // One broad membership (1,100 rows): the ordered walk.
    let everything = backend
        .search(t, &query(vec![code()], 5_000))
        .await
        .unwrap();
    assert_eq!(everything.resources.len(), TOTAL);
    assert_default_order(&everything.resources.items);
    let first = backend.search(t, &query(vec![code()], 50)).await.unwrap();
    assert_eq!(
        ids(&first.resources.items),
        ids(&everything.resources.items[..50]),
        "a page of the broad walk is the head of the full order"
    );
    assert_eq!(
        backend
            .search_count(t, &query(vec![code()], 50))
            .await
            .unwrap(),
        TOTAL as u64
    );

    // Two broad memberships (1,100 and 1,050 rows): the smaller is
    // materialised, the other probed per candidate.
    let finals = all_pages(backend, t, &query(vec![code(), status("final")], 400)).await;
    assert_eq!(finals.len(), TOTAL - AMENDED);
    assert_eq!(
        finals.iter().collect::<HashSet<_>>().len(),
        finals.len(),
        "cursor pages overlap"
    );
    let expected: Vec<String> = ids(&everything.resources.items)
        .into_iter()
        .filter(|id| id.as_str() < format!("obs-{:04}", TOTAL - AMENDED).as_str())
        .collect();
    assert_eq!(finals, expected, "same members in the same order");

    // A selective membership (50 rows) drives the broad one.
    let amended = backend
        .search(t, &query(vec![code(), status("amended")], 20))
        .await
        .unwrap();
    assert_eq!(amended.resources.len(), 20);
    assert_default_order(&amended.resources.items);
    let all_amended = all_pages(backend, t, &query(vec![status("amended"), code()], 20)).await;
    assert_eq!(all_amended.len(), AMENDED);
    assert_eq!(&all_amended[..20], ids(&amended.resources.items).as_slice());
    assert_eq!(
        backend
            .search_count(t, &query(vec![code(), status("amended")], 20))
            .await
            .unwrap(),
        AMENDED as u64
    );

    // A reference (3 rows) drives; a bare id resolves against the targets.
    for value in ["Patient/p1", "p1"] {
        let hits = backend
            .search(t, &query(vec![code(), patient(value)], 100))
            .await
            .unwrap();
        assert_eq!(hits.resources.len(), OF_P1, "subject={value}");
        assert!(
            hits.resources
                .items
                .iter()
                .all(|r| r.content()["subject"]["reference"] == "Patient/p1")
        );
        assert_eq!(
            backend
                .search_count(t, &query(vec![patient(value), code()], 100))
                .await
                .unwrap(),
            OF_P1 as u64
        );
    }

    // A filter that matches nothing, beside a broad one.
    let none = backend
        .search(t, &query(vec![code(), status("cancelled")], 20))
        .await
        .unwrap();
    assert!(none.resources.is_empty());
}

/// A deleted resource drops out of the selective (rowid-driven) shape too.
async fn a_driving_filter_still_excludes_deleted_resources<S>(backend: &S, t: &TenantContext)
where
    S: ResourceStorage + SearchProvider,
{
    let amended = query(
        vec![param("status", SearchParamType::Token, "amended"), code()],
        100,
    );
    let before = backend.search(t, &amended).await.unwrap();
    let victim = before.resources.items[0].id().to_string();
    backend.delete(t, "Observation", &victim).await.unwrap();

    let after = backend.search(t, &amended).await.unwrap();
    assert_eq!(after.resources.len(), AMENDED - 1);
    assert!(!ids(&after.resources.items).contains(&victim));
    assert_eq!(
        backend.search_count(t, &amended).await.unwrap(),
        (AMENDED - 1) as u64
    );
}
