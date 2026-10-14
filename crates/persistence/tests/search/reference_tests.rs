//! Tests for reference search parameters.
//!
//! This module tests reference-type search parameters including
//! relative references, absolute URLs, and logical references.

use serde_json::json;

use helios_persistence::core::{ResourceStorage, SearchProvider};
use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
use helios_persistence::types::{
    SearchModifier, SearchParamType, SearchParameter, SearchQuery, SearchValue,
};

use helios_fhir::FhirVersion;

#[cfg(feature = "sqlite")]
use helios_persistence::backends::sqlite::SqliteBackend;

// ============================================================================
// Helper Functions
// ============================================================================

#[cfg(feature = "sqlite")]
fn create_sqlite_backend() -> SqliteBackend {
    super::make_sqlite_backend()
}

fn create_tenant() -> TenantContext {
    TenantContext::new(
        TenantId::new("test-tenant"),
        TenantPermissions::full_access(),
    )
}

#[cfg(feature = "sqlite")]
async fn seed_test_data(backend: &SqliteBackend, tenant: &TenantContext) {
    // Create patients
    let patient1 =
        json!({"resourceType": "Patient", "id": "patient-1", "name": [{"family": "Smith"}]});
    let patient2 =
        json!({"resourceType": "Patient", "id": "patient-2", "name": [{"family": "Jones"}]});
    backend
        .create_or_update(
            tenant,
            "Patient",
            "patient-1",
            patient1,
            FhirVersion::default(),
        )
        .await
        .unwrap();
    backend
        .create_or_update(
            tenant,
            "Patient",
            "patient-2",
            patient2,
            FhirVersion::default(),
        )
        .await
        .unwrap();

    // Create observations referencing patients
    let obs1 = json!({
        "resourceType": "Observation",
        "status": "final",
        "subject": {"reference": "Patient/patient-1"},
        "code": {"coding": [{"code": "test1"}]}
    });
    let obs2 = json!({
        "resourceType": "Observation",
        "status": "final",
        "subject": {"reference": "Patient/patient-1"},
        "code": {"coding": [{"code": "test2"}]}
    });
    let obs3 = json!({
        "resourceType": "Observation",
        "status": "final",
        "subject": {"reference": "Patient/patient-2"},
        "code": {"coding": [{"code": "test3"}]}
    });
    backend
        .create(tenant, "Observation", obs1, FhirVersion::default())
        .await
        .unwrap();
    backend
        .create(tenant, "Observation", obs2, FhirVersion::default())
        .await
        .unwrap();
    backend
        .create(tenant, "Observation", obs3, FhirVersion::default())
        .await
        .unwrap();
}

// ============================================================================
// Reference Search Tests
// ============================================================================

/// Test reference search with relative reference.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn test_reference_search_relative() {
    let backend = create_sqlite_backend();
    let tenant = create_tenant();
    seed_test_data(&backend, &tenant).await;

    // Search for observations referencing patient-1
    let query = SearchQuery::new("Observation").with_parameter(SearchParameter {
        name: "subject".to_string(),
        param_type: SearchParamType::Reference,
        modifier: None,
        values: vec![SearchValue::eq("Patient/patient-1")],
        chain: vec![],
        components: vec![],
    });

    let result = backend
        .search(&tenant, &query.with_count(100))
        .await
        .unwrap();

    // Should find 2 observations for patient-1
    assert_eq!(result.resources.len(), 2);
    for resource in &result.resources.items {
        assert_eq!(
            resource.content()["subject"]["reference"],
            "Patient/patient-1"
        );
    }
}

/// Test reference search with ID only.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn test_reference_search_id_only() {
    let backend = create_sqlite_backend();
    let tenant = create_tenant();
    seed_test_data(&backend, &tenant).await;

    // Search with just the ID
    let query = SearchQuery::new("Observation").with_parameter(SearchParameter {
        name: "subject".to_string(),
        param_type: SearchParamType::Reference,
        modifier: None,
        values: vec![SearchValue::eq("patient-1")],
        chain: vec![],
        components: vec![],
    });

    let result = backend
        .search(&tenant, &query.with_count(100))
        .await
        .unwrap();

    // Should find observations for patient-1
    assert!(!result.resources.is_empty());
}

/// Test reference search with type modifier.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn test_reference_search_type_modifier() {
    let backend = create_sqlite_backend();
    let tenant = create_tenant();
    seed_test_data(&backend, &tenant).await;

    // Search with :Patient modifier. The type modifier is carried as
    // `SearchModifier::Type` — the REST layer splits `subject:Patient` into the
    // parameter name plus the modifier before the query reaches a backend.
    let query = SearchQuery::new("Observation").with_parameter(SearchParameter {
        name: "subject".to_string(),
        param_type: SearchParamType::Reference,
        modifier: Some(SearchModifier::Type("Patient".to_string())),
        values: vec![SearchValue::eq("patient-1")],
        chain: vec![],
        components: vec![],
    });

    let result = backend
        .search(&tenant, &query.with_count(100))
        .await
        .unwrap();

    assert!(!result.resources.is_empty());
}

/// Test reference search with no results.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn test_reference_search_no_results() {
    let backend = create_sqlite_backend();
    let tenant = create_tenant();
    seed_test_data(&backend, &tenant).await;

    let query = SearchQuery::new("Observation").with_parameter(SearchParameter {
        name: "subject".to_string(),
        param_type: SearchParamType::Reference,
        modifier: None,
        values: vec![SearchValue::eq("Patient/nonexistent")],
        chain: vec![],
        components: vec![],
    });

    let result = backend
        .search(&tenant, &query.with_count(100))
        .await
        .unwrap();

    assert!(result.resources.is_empty());
}

/// Seeds Observations whose `subject` is written in every form a bare-id or
/// `Type/id` search has to tell apart, and returns nothing: the ids carry the
/// expectation (`hit-*` must match `patient-1`, `miss-*` must not).
#[cfg(feature = "sqlite")]
async fn seed_reference_forms(backend: &SqliteBackend, tenant: &TenantContext) {
    let forms = [
        ("hit-relative", "Patient/patient-1"),
        ("hit-versioned", "Patient/patient-1/_history/3"),
        ("hit-absolute", "http://example.org/fhir/Patient/patient-1"),
        ("miss-dash", "Patient/patient-1-4"),
        ("miss-dot", "Patient/patient-1.5"),
        ("miss-longer", "Patient/patient-10"),
        ("miss-case", "Patient/PATIENT-1"),
        ("miss-other", "Patient/patient-2"),
    ];
    for (id, reference) in forms {
        let obs = json!({
            "resourceType": "Observation",
            "id": id,
            "status": "final",
            "code": {"coding": [{"system": "http://loinc.org", "code": "8867-4"}]},
            "subject": {"reference": reference}
        });
        backend
            .create_or_update(tenant, "Observation", id, obs, FhirVersion::default())
            .await
            .unwrap();
    }
}

#[cfg(feature = "sqlite")]
async fn reference_hits(
    backend: &SqliteBackend,
    tenant: &TenantContext,
    name: &str,
    value: &str,
) -> Vec<String> {
    let query = SearchQuery::new("Observation").with_parameter(SearchParameter {
        name: name.to_string(),
        param_type: SearchParamType::Reference,
        modifier: None,
        values: vec![SearchValue::eq(value)],
        chain: vec![],
        components: vec![],
    });
    let result = backend
        .search(tenant, &query.clone().with_count(100))
        .await
        .unwrap();
    let mut ids: Vec<String> = result
        .resources
        .items
        .iter()
        .map(|r| r.id().to_string())
        .collect();
    ids.sort();
    let count = backend.search_count(tenant, &query).await.unwrap();
    assert_eq!(
        count as usize,
        ids.len(),
        "{name}={value}: count and page agree"
    );
    ids
}

/// A bare id is a local reference to one of the parameter's target types: it
/// matches `Patient/<id>` and its `_history` versions, the absolute form the
/// suffix match always accepted, and nothing whose id merely starts with it
/// or differs in case. `patient` targets Patient only; `subject` has several
/// targets and must give the same answer.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn test_reference_bare_id_matches_local_references_to_targets() {
    let backend = create_sqlite_backend();
    let tenant = create_tenant();
    seed_reference_forms(&backend, &tenant).await;

    let expected = vec!["hit-absolute", "hit-relative", "hit-versioned"];
    for name in ["patient", "subject"] {
        assert_eq!(
            reference_hits(&backend, &tenant, name, "patient-1").await,
            expected,
            "{name}=patient-1"
        );
    }
}

/// `Type/id` matches the reference and its `_history` versions only; an id
/// that extends this one with `-`, `.` or a digit sorts inside the seekable
/// range but is not a match.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn test_reference_typed_id_excludes_neighbouring_ids() {
    let backend = create_sqlite_backend();
    let tenant = create_tenant();
    seed_reference_forms(&backend, &tenant).await;

    assert_eq!(
        reference_hits(&backend, &tenant, "subject", "Patient/patient-1").await,
        vec!["hit-relative", "hit-versioned"]
    );
    assert_eq!(
        reference_hits(&backend, &tenant, "patient", "Patient/patient-1/_history/9").await,
        vec!["hit-relative", "hit-versioned"],
        "a versioned search value is matched version-agnostically"
    );
}
