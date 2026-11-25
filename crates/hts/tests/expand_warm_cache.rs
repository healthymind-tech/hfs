//! A repeated `ValueSet/$expand` is answered from the handler response cache
//! without asking the backend anything.
//!
//! The `$expand` handlers run over a backend wrapper that records every
//! trait call, so a lookup added ahead of the cache shows up as a call on the
//! warm request.

#![cfg(feature = "sqlite")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::get,
};
use helios_hts::backends::SqliteTerminologyBackend;
use helios_hts::error::HtsError;
use helios_hts::explore::ExploreRequest;
use helios_hts::import::BundleImportBackend;
use helios_hts::operations::expand::{expand_handler, get_expand_handler};
use helios_hts::snomed::{ConceptInfo, SnomedModel};
use helios_hts::state::AppState;
use helios_hts::traits::{
    CodeSystemOperations, ConceptDesignation, ConceptExpansionFlags, ConceptMapOperations,
    SupplementInfo, TerminologyCaches, TerminologyMetadata, ValueSetOperations,
};
use helios_hts::types::{
    ClosureRequest, ClosureResponse, ExpandRequest, ExpandResponse, LookupRequest, LookupResponse,
    PropertyValue, ResourceSearchQuery, SubsumesRequest, SubsumesResponse, TranslateRequest,
    TranslateResponse, ValidateCodeRequest, ValidateCodeResponse,
};
use helios_persistence::tenant::TenantContext;
use serde_json::{Value, json};
use tower::ServiceExt;

// ── A backend that records every call ─────────────────────────────────────────

/// The calls made on a [`Counting`] backend, in order.
#[derive(Clone, Default)]
struct Calls(Arc<Mutex<Vec<String>>>);

impl Calls {
    fn record(&self, call: impl Into<String>) {
        self.0.lock().unwrap().push(call.into());
    }

    fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    /// The calls made since the first `from`.
    fn since(&self, from: usize) -> Vec<String> {
        self.0.lock().unwrap()[from..].to_vec()
    }
}

/// Forwards every backend trait method to `inner` and records it.
#[derive(Clone)]
struct Counting<B> {
    inner: B,
    calls: Calls,
}

fn search_call(kind: &str, q: &ResourceSearchQuery) -> String {
    format!(
        "{kind}.search url={} version={} count={}",
        q.url.as_deref().unwrap_or("-"),
        q.version.as_deref().unwrap_or("-"),
        q.count.map_or("-".to_string(), |c| c.to_string()),
    )
}

type Map<V> = HashMap<String, V>;

#[async_trait]
impl<B: CodeSystemOperations> CodeSystemOperations for Counting<B> {
    async fn lookup(
        &self,
        ctx: &TenantContext,
        req: LookupRequest,
    ) -> Result<LookupResponse, HtsError> {
        self.calls.record("CodeSystem.lookup");
        self.inner.lookup(ctx, req).await
    }

    async fn validate_code(
        &self,
        ctx: &TenantContext,
        req: ValidateCodeRequest,
    ) -> Result<ValidateCodeResponse, HtsError> {
        self.calls.record("CodeSystem.validate_code");
        self.inner.validate_code(ctx, req).await
    }

    async fn search(
        &self,
        ctx: &TenantContext,
        query: ResourceSearchQuery,
    ) -> Result<Vec<Value>, HtsError> {
        self.calls.record(search_call("CodeSystem", &query));
        self.inner.search(ctx, query).await
    }

    async fn count(
        &self,
        ctx: &TenantContext,
        query: ResourceSearchQuery,
    ) -> Result<u64, HtsError> {
        self.calls.record("CodeSystem.count");
        self.inner.count(ctx, query).await
    }

    async fn subsumes(
        &self,
        ctx: &TenantContext,
        req: SubsumesRequest,
    ) -> Result<SubsumesResponse, HtsError> {
        self.calls.record("CodeSystem.subsumes");
        self.inner.subsumes(ctx, req).await
    }

    async fn code_system_version_for_url(
        &self,
        ctx: &TenantContext,
        url: &str,
    ) -> Result<Option<String>, HtsError> {
        self.calls.record("CodeSystem.code_system_version_for_url");
        self.inner.code_system_version_for_url(ctx, url).await
    }

    async fn code_system_exists(&self, ctx: &TenantContext, url: &str) -> Result<bool, HtsError> {
        self.calls.record("CodeSystem.code_system_exists");
        self.inner.code_system_exists(ctx, url).await
    }

    async fn code_system_language(
        &self,
        ctx: &TenantContext,
        url: &str,
    ) -> Result<Option<String>, HtsError> {
        self.calls.record("CodeSystem.code_system_language");
        self.inner.code_system_language(ctx, url).await
    }

    async fn code_system_is_hierarchical(
        &self,
        ctx: &TenantContext,
        url: &str,
        version: Option<&str>,
    ) -> Result<bool, HtsError> {
        self.calls.record("CodeSystem.code_system_is_hierarchical");
        self.inner
            .code_system_is_hierarchical(ctx, url, version)
            .await
    }

    async fn explore(&self, ctx: &TenantContext, req: &ExploreRequest) -> Result<Value, HtsError> {
        self.calls.record("CodeSystem.explore");
        self.inner.explore(ctx, req).await
    }

    async fn snomed_model(
        &self,
        ctx: &TenantContext,
        url: &str,
        version: Option<&str>,
    ) -> Result<Option<Arc<SnomedModel>>, HtsError> {
        self.calls.record("CodeSystem.snomed_model");
        self.inner.snomed_model(ctx, url, version).await
    }

    async fn snomed_concepts(
        &self,
        ctx: &TenantContext,
        url: &str,
        version: Option<&str>,
        codes: &[String],
    ) -> Result<Map<ConceptInfo>, HtsError> {
        self.calls.record("CodeSystem.snomed_concepts");
        self.inner.snomed_concepts(ctx, url, version, codes).await
    }

    async fn concept_designations(
        &self,
        ctx: &TenantContext,
        system_url: &str,
        codes: &[String],
    ) -> Result<Map<Vec<ConceptDesignation>>, HtsError> {
        self.calls.record("CodeSystem.concept_designations");
        self.inner
            .concept_designations(ctx, system_url, codes)
            .await
    }

    async fn concept_property_values(
        &self,
        ctx: &TenantContext,
        system_url: &str,
        codes: &[String],
        properties: &[String],
    ) -> Result<Map<Vec<PropertyValue>>, HtsError> {
        self.calls.record("CodeSystem.concept_property_values");
        self.inner
            .concept_property_values(ctx, system_url, codes, properties)
            .await
    }

    async fn concept_expansion_flags(
        &self,
        ctx: &TenantContext,
        system_url: &str,
        codes: &[String],
    ) -> Result<Map<ConceptExpansionFlags>, HtsError> {
        self.calls.record("CodeSystem.concept_expansion_flags");
        self.inner
            .concept_expansion_flags(ctx, system_url, codes)
            .await
    }

    async fn supplement_target(
        &self,
        ctx: &TenantContext,
        supplement_url: &str,
    ) -> Result<Option<SupplementInfo>, HtsError> {
        self.calls.record("CodeSystem.supplement_target");
        self.inner.supplement_target(ctx, supplement_url).await
    }

    async fn supplements_for_system(
        &self,
        ctx: &TenantContext,
        system_url: &str,
    ) -> Result<Vec<SupplementInfo>, HtsError> {
        self.calls.record("CodeSystem.supplements_for_system");
        self.inner.supplements_for_system(ctx, system_url).await
    }

    async fn supplement_designations(
        &self,
        ctx: &TenantContext,
        supplement_urls: &[String],
        codes: &[String],
    ) -> Result<Map<Vec<ConceptDesignation>>, HtsError> {
        self.calls.record("CodeSystem.supplement_designations");
        self.inner
            .supplement_designations(ctx, supplement_urls, codes)
            .await
    }

    async fn supplement_property_values(
        &self,
        ctx: &TenantContext,
        supplement_urls: &[String],
        codes: &[String],
        properties: &[String],
    ) -> Result<Map<Vec<PropertyValue>>, HtsError> {
        self.calls.record("CodeSystem.supplement_property_values");
        self.inner
            .supplement_property_values(ctx, supplement_urls, codes, properties)
            .await
    }

    async fn concept_resource_entries(
        &self,
        ctx: &TenantContext,
        system_url: &str,
        codes: &[String],
    ) -> Result<Map<Value>, HtsError> {
        self.calls.record("CodeSystem.concept_resource_entries");
        self.inner
            .concept_resource_entries(ctx, system_url, codes)
            .await
    }

    async fn supplement_concept_entries(
        &self,
        ctx: &TenantContext,
        supplement_urls: &[String],
        codes: &[String],
    ) -> Result<Map<Vec<Value>>, HtsError> {
        self.calls.record("CodeSystem.supplement_concept_entries");
        self.inner
            .supplement_concept_entries(ctx, supplement_urls, codes)
            .await
    }
}

#[async_trait]
impl<B: ValueSetOperations> ValueSetOperations for Counting<B> {
    async fn search(
        &self,
        ctx: &TenantContext,
        query: ResourceSearchQuery,
    ) -> Result<Vec<Value>, HtsError> {
        self.calls.record(search_call("ValueSet", &query));
        self.inner.search(ctx, query).await
    }

    async fn count(
        &self,
        ctx: &TenantContext,
        query: ResourceSearchQuery,
    ) -> Result<u64, HtsError> {
        self.calls.record("ValueSet.count");
        self.inner.count(ctx, query).await
    }

    async fn expand(
        &self,
        ctx: &TenantContext,
        req: ExpandRequest,
    ) -> Result<ExpandResponse, HtsError> {
        self.calls.record("ValueSet.expand");
        self.inner.expand(ctx, req).await
    }

    async fn value_set_version_for_url(
        &self,
        ctx: &TenantContext,
        url: &str,
    ) -> Result<Option<String>, HtsError> {
        self.calls.record("ValueSet.value_set_version_for_url");
        self.inner.value_set_version_for_url(ctx, url).await
    }

    async fn validate_code(
        &self,
        ctx: &TenantContext,
        req: ValidateCodeRequest,
    ) -> Result<ValidateCodeResponse, HtsError> {
        self.calls.record("ValueSet.validate_code");
        self.inner.validate_code(ctx, req).await
    }
}

#[async_trait]
impl<B: ConceptMapOperations> ConceptMapOperations for Counting<B> {
    async fn search(
        &self,
        ctx: &TenantContext,
        query: ResourceSearchQuery,
    ) -> Result<Vec<Value>, HtsError> {
        self.calls.record(search_call("ConceptMap", &query));
        self.inner.search(ctx, query).await
    }

    async fn count(
        &self,
        ctx: &TenantContext,
        query: ResourceSearchQuery,
    ) -> Result<u64, HtsError> {
        self.calls.record("ConceptMap.count");
        self.inner.count(ctx, query).await
    }

    async fn translate(
        &self,
        ctx: &TenantContext,
        req: TranslateRequest,
    ) -> Result<TranslateResponse, HtsError> {
        self.calls.record("ConceptMap.translate");
        self.inner.translate(ctx, req).await
    }

    async fn concept_maps_for_system(
        &self,
        ctx: &TenantContext,
        system: &str,
        reverse: bool,
    ) -> Result<Vec<(String, Option<String>)>, HtsError> {
        self.calls.record("ConceptMap.concept_maps_for_system");
        self.inner
            .concept_maps_for_system(ctx, system, reverse)
            .await
    }

    async fn closure(
        &self,
        ctx: &TenantContext,
        req: ClosureRequest,
    ) -> Result<ClosureResponse, HtsError> {
        self.calls.record("ConceptMap.closure");
        self.inner.closure(ctx, req).await
    }
}

impl<B: TerminologyMetadata> TerminologyMetadata for Counting<B> {
    fn backend_name(&self) -> &'static str {
        self.calls.record("Metadata.backend_name");
        self.inner.backend_name()
    }

    fn supported_systems(&self) -> Vec<String> {
        self.calls.record("Metadata.supported_systems");
        self.inner.supported_systems()
    }

    fn supports_subsumption(&self) -> bool {
        self.calls.record("Metadata.supports_subsumption");
        self.inner.supports_subsumption()
    }

    fn resource_url_by_id(&self, resource_type: &str, id: &str) -> Option<String> {
        self.calls.record("Metadata.resource_url_by_id");
        self.inner.resource_url_by_id(resource_type, id)
    }

    fn resource_json_by_id(&self, resource_type: &str, id: &str) -> Option<Value> {
        self.calls.record("Metadata.resource_json_by_id");
        self.inner.resource_json_by_id(resource_type, id)
    }
}

impl<B: TerminologyCaches> TerminologyCaches for Counting<B> {
    fn invalidate_caches(&self) {
        self.calls.record("Caches.invalidate_caches");
        self.inner.invalidate_caches();
    }
}

// ── Fixture ───────────────────────────────────────────────────────────────────

const CS_URL: &str = "http://hts.test/warm/cs";
const VS_URL: &str = "http://hts.test/warm/vs";
/// The implicit "all codes of the CodeSystem" value set, as a query value.
const IMPLICIT_VS: &str = "http://hts.test/warm/cs%3Ffhir_vs";

fn bundle() -> String {
    let concepts: Vec<Value> = [
        ("g1", "Glucose in blood"),
        ("g2", "Glucose in urine"),
        ("g3", "Glucose tolerance"),
        ("h1", "Hemoglobin"),
        ("h2", "Hematocrit"),
        ("s1", "Sodium"),
        ("p1", "Potassium"),
        ("c1", "Creatinine"),
    ]
    .iter()
    .map(|(code, display)| json!({"code": code, "display": display}))
    .collect();
    json!({
        "resourceType": "Bundle", "type": "collection",
        "entry": [
            {"resource": {"resourceType": "CodeSystem", "id": "warm-cs", "url": CS_URL,
                "version": "1", "status": "active", "content": "complete", "concept": concepts}},
            {"resource": {"resourceType": "ValueSet", "id": "warm-vs", "url": VS_URL,
                "version": "1", "status": "active",
                "compose": {"include": [{"system": CS_URL}]}}}
        ]
    })
    .to_string()
}

struct App {
    router: Router,
    calls: Calls,
}

impl App {
    async fn new() -> Self {
        let backend = SqliteTerminologyBackend::in_memory().expect("in-memory backend");
        backend
            .import_bundle(&TenantContext::system(), bundle().as_bytes())
            .await
            .expect("import fixture");
        let calls = Calls::default();
        let state = AppState::new(Counting {
            inner: backend,
            calls: calls.clone(),
        });
        let router = Router::new()
            .route(
                "/ValueSet/$expand",
                get(get_expand_handler::<Counting<SqliteTerminologyBackend>>)
                    .post(expand_handler::<Counting<SqliteTerminologyBackend>>),
            )
            .with_state(state);
        Self { router, calls }
    }

    async fn send(&self, request: &Req) -> Vec<u8> {
        let builder = Request::builder().uri(request.uri());
        let http = match request {
            Req::Get(_) => builder.method("GET").body(Body::empty()),
            Req::Post(body) => builder
                .method("POST")
                .header("content-type", "application/fhir+json")
                .body(Body::from(body.to_string())),
        }
        .unwrap();
        let response = self.router.clone().oneshot(http).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        assert_eq!(
            status,
            StatusCode::OK,
            "{}: {}",
            request.uri(),
            String::from_utf8_lossy(&bytes)
        );
        bytes
    }
}

enum Req {
    /// `GET /ValueSet/$expand?<query>`.
    Get(String),
    /// `POST /ValueSet/$expand` with a `Parameters` body.
    Post(Value),
}

impl Req {
    fn uri(&self) -> String {
        match self {
            Req::Get(query) => format!("/ValueSet/$expand?{query}"),
            Req::Post(_) => "/ValueSet/$expand".to_string(),
        }
    }
}

/// `url=` with a text filter, paged, and plain — for a stored and for an
/// implicit value set — and an inline compose with a filter.
fn requests() -> Vec<Req> {
    let mut all = Vec::new();
    for url in [VS_URL, IMPLICIT_VS] {
        all.push(Req::Get(format!("url={url}&filter=glucose&count=20")));
        all.push(Req::Get(format!("url={url}&offset=2&count=3")));
        all.push(Req::Get(format!("url={url}")));
    }
    all.push(Req::Post(json!({
        "resourceType": "Parameters",
        "parameter": [
            {"name": "valueSet", "resource": {"resourceType": "ValueSet", "status": "active",
                "compose": {"include": [{"system": CS_URL}]}}},
            {"name": "filter", "valueString": "glucose"},
            {"name": "count", "valueInteger": 20}
        ]
    })));
    all
}

fn codes(body: &[u8]) -> Vec<String> {
    let json: Value = serde_json::from_slice(body).unwrap();
    json["expansion"]["contains"]
        .as_array()
        .map(|c| {
            c.iter()
                .filter_map(|e| e["code"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The response without the parts that differ between two computations of
/// the same expansion.
fn comparable(body: &[u8]) -> Value {
    let mut json: Value = serde_json::from_slice(body).unwrap();
    if let Some(expansion) = json["expansion"].as_object_mut() {
        expansion.remove("identifier");
        expansion.remove("timestamp");
    }
    json
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_warm_expand_makes_no_backend_call() {
    let app = App::new().await;
    for request in requests() {
        let before = app.calls.len();
        let cold = app.send(&request).await;
        let after_cold = app.calls.len();
        assert!(
            after_cold > before,
            "{}: the first request is computed by the backend",
            request.uri()
        );

        for _ in 0..3 {
            let warm = app.send(&request).await;
            assert_eq!(
                app.calls.since(after_cold),
                Vec::<String>::new(),
                "{}: a repeated request must be answered from the response cache",
                request.uri()
            );
            assert_eq!(
                warm,
                cold,
                "{}: the cached response is the computed one",
                request.uri()
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_expansions_have_the_expected_codes() {
    let app = App::new().await;
    let all = requests();
    let expected: [&[&str]; 7] = [
        &["g1", "g2", "g3"],
        // A stored value set pages in the order of its definition, …
        &["g3", "h1", "h2"],
        &["c1", "g1", "g2", "g3", "h1", "h2", "p1", "s1"],
        &["g1", "g2", "g3"],
        // … an implicit one in code order.
        &["g2", "g3", "h1"],
        &["c1", "g1", "g2", "g3", "h1", "h2", "p1", "s1"],
        &["g1", "g2", "g3"],
    ];
    for (request, expected) in all.iter().zip(expected) {
        for pass in ["cold", "warm"] {
            let mut got = codes(&app.send(request).await);
            // Paged requests are positional; the others are compared as sets.
            if !request.uri().contains("offset=") {
                got.sort();
            }
            assert_eq!(got, expected, "{} ({pass})", request.uri());
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cached_response_equals_a_fresh_computation() {
    let warmed = App::new().await;
    let fresh = App::new().await;
    for request in requests() {
        warmed.send(&request).await;
        let warm = warmed.send(&request).await;
        let cold = fresh.send(&request).await;
        assert_eq!(
            comparable(&warm),
            comparable(&cold),
            "{}: cached and freshly computed responses differ",
            request.uri()
        );
    }
}

/// The `ValueSet.search` calls of `calls` that read one row by url: the
/// lookup of the value set a request names.
fn requested_value_set_lookups(calls: &[String]) -> Vec<&String> {
    calls
        .iter()
        .filter(|c| c.starts_with("ValueSet.search ") && c.ends_with(" count=1"))
        .collect()
}

/// A paged request, so the expansion itself does not read the ValueSet to
/// decide on nesting: what is left is the lookup ahead of it.
#[tokio::test(flavor = "multi_thread")]
async fn a_cold_expand_reads_the_requested_value_set_once() {
    let app = App::new().await;
    let request = Req::Get(format!("url={VS_URL}&offset=1&count=2"));
    app.send(&request).await;
    let calls = app.calls.since(0);
    assert_eq!(
        requested_value_set_lookups(&calls).len(),
        1,
        "one lookup of the stored ValueSet: {calls:#?}"
    );
}

/// An implicit ValueSet URL names no stored resource: nothing is looked up.
#[tokio::test(flavor = "multi_thread")]
async fn a_cold_expand_of_an_implicit_value_set_reads_no_stored_value_set() {
    let app = App::new().await;
    for query in [
        format!("url={IMPLICIT_VS}&offset=1&count=2"),
        format!("url={IMPLICIT_VS}&filter=glucose&count=20"),
    ] {
        let from = app.calls.len();
        app.send(&Req::Get(query.clone())).await;
        let calls = app.calls.since(from);
        assert_eq!(
            requested_value_set_lookups(&calls),
            Vec::<&String>::new(),
            "{query}: {calls:#?}"
        );
    }
}
