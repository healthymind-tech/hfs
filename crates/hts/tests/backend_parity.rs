//! Backend parity: the same scenarios run against the SQLite and the
//! PostgreSQL backend through the storage traits, and both must give the same
//! answer (and the answer SQLite has always given).
//!
//! The PostgreSQL half needs the `postgres` feature and a server (Docker for
//! testcontainers, or `HTS_TEST_PG_URL`); each scenario gets a fresh
//! database. Without the feature only SQLite runs, which still pins the
//! expected behaviour.

#![cfg(feature = "sqlite")]

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use helios_hts::backends::SqliteTerminologyBackend;
use helios_hts::error::HtsError;
use helios_hts::import::BundleImportBackend;
use helios_hts::traits::{TerminologyBackend, ValueSetOperations};
use helios_hts::types::{ExpandRequest, ExpandResponse, TranslateRequest, ValidateCodeRequest};
use helios_persistence::tenant::TenantContext;
use serde_json::{Value, json};

trait Both: TerminologyBackend + BundleImportBackend {}
impl<T: TerminologyBackend + BundleImportBackend> Both for T {}

type Backend = Arc<dyn Both>;

fn ctx() -> TenantContext {
    TenantContext::system()
}

/// The scenarios run one at a time: each file-backed SQLite backend opens a
/// 64-connection pool eagerly, and ten at once exceed a 1024 descriptor limit.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// One fresh SQLite backend (file-backed, as in production) and — when built
/// with `postgres` — one on a fresh PostgreSQL database.
async fn backends() -> Vec<(&'static str, Backend, Option<tempfile::TempDir>)> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hts.db");
    let sqlite = SqliteTerminologyBackend::new(path.to_str().unwrap()).unwrap();
    #[allow(unused_mut)]
    let mut out: Vec<(&'static str, Backend, Option<tempfile::TempDir>)> =
        vec![("sqlite", Arc::new(sqlite), Some(dir))];
    #[cfg(feature = "postgres")]
    {
        let url = common::fresh_pg_database().await;
        let pg = helios_hts::backends::PostgresTerminologyBackend::new(&url)
            .await
            .unwrap();
        out.push(("postgres", Arc::new(pg), None));
    }
    out
}

async fn import(b: &Backend, bundle: &Value) {
    let stats = b
        .import_bundle(&ctx(), &serde_json::to_vec(bundle).unwrap())
        .await
        .unwrap();
    assert!(stats.errors.is_empty(), "import errors: {:?}", stats.errors);
}

fn bundle(resources: Vec<Value>) -> Value {
    json!({
        "resourceType": "Bundle",
        "type": "collection",
        "entry": resources.into_iter().map(|r| json!({"resource": r})).collect::<Vec<_>>()
    })
}

/// Codes of an expansion in response order, plus its total.
fn shape(r: &ExpandResponse) -> (Vec<String>, Option<u32>) {
    fn walk(c: &[helios_hts::types::ExpansionContains], out: &mut Vec<String>) {
        for x in c {
            out.push(x.code.clone());
            walk(&x.contains, out);
        }
    }
    let mut codes = Vec::new();
    walk(&r.contains, &mut codes);
    (codes, r.total)
}

async fn expand(b: &Backend, req: ExpandRequest) -> Result<(Vec<String>, Option<u32>), String> {
    b.expand(&ctx(), req)
        .await
        .map(|r| shape(&r))
        .map_err(|e| error_kind(&e))
}

fn error_kind(e: &HtsError) -> String {
    match e {
        HtsError::NotFound(_) => "not-found".into(),
        HtsError::InvalidRequest(_) => "invalid".into(),
        HtsError::VsInvalid(m) => format!("vs-invalid: {m}"),
        HtsError::TooCostly(_) => "too-costly".into(),
        HtsError::NotSupported(_) => "not-supported".into(),
        other => format!("other: {other}"),
    }
}

fn url_req(url: &str) -> ExpandRequest {
    ExpandRequest {
        url: Some(url.into()),
        ..Default::default()
    }
}

async fn validates(b: &Backend, url: &str, system: Option<&str>, code: &str) -> bool {
    ValueSetOperations::validate_code(
        &**b,
        &ctx(),
        ValidateCodeRequest {
            url: Some(url.into()),
            system: system.map(str::to_owned),
            code: code.into(),
            ..Default::default()
        },
    )
    .await
    .unwrap_or_else(|e| panic!("validate {url} {code}: {e:?}"))
    .result
}

/// Run `check` on every backend and assert they all returned the same value;
/// returns it so the caller can pin the expected answer too.
macro_rules! same_on_all {
    ($backends:expr, |$b:ident| $body:expr) => {{
        let mut results: Vec<(&str, _)> = Vec::new();
        for (name, $b, _) in $backends.iter() {
            let $b: &Backend = $b;
            results.push((*name, $body));
        }
        let first = results[0].1.clone();
        for (name, r) in &results[1..] {
            assert_eq!(
                r,
                &first,
                "{name} differs from {} for {}",
                results[0].0,
                stringify!($body)
            );
        }
        first
    }};
}

// ── Fixtures ──────────────────────────────────────────────────────────────────

fn loinc_like() -> Value {
    json!({
        "resourceType": "CodeSystem", "id": "loinc", "url": "http://loinc.org",
        "version": "2.80", "status": "active", "content": "complete",
        "concept": [
            {"code": "LP1-1", "display": "Chemistry", "concept": [
                {"code": "LP2-2", "display": "Glucose", "concept": [
                    {"code": "2345-7", "display": "Glucose [Mass/volume] in Serum or Plasma"},
                    {"code": "10000-8", "display": "Glucose tolerance 2h"},
                    {"code": "1000-9", "display": "Glucose post meal"}
                ]}
            ]},
            {"code": "999-9", "display": "Unrelated term"},
            {"code": "Glu-1", "display": "glucose"}
        ]
    })
}

fn hierarchy_cs(url: &str, extra_child: bool) -> Value {
    let mut children = vec![
        json!({"code": "child1", "display": "Child one", "concept": [
            {"code": "grand1", "display": "Grandchild one"}
        ]}),
        json!({"code": "child2", "display": "Child two"}),
    ];
    if extra_child {
        children.push(json!({"code": "child3", "display": "Child three"}));
    }
    json!({
        "resourceType": "CodeSystem", "id": "cs-fvs", "url": url, "version": "1",
        "status": "active", "content": "complete",
        "concept": [{"code": "root", "display": "Root", "concept": children}]
    })
}

fn property_cs() -> Value {
    json!({
        "resourceType": "CodeSystem", "id": "props", "url": "http://p.test/cs",
        "status": "active", "content": "complete",
        "property": [
            {"code": "colour", "type": "code"},
            {"code": "notSelectable", "type": "boolean"}
        ],
        "concept": [
            {"code": "A1", "display": "Apple", "property": [{"code": "colour", "valueCode": "red"}]},
            {"code": "A2", "display": "Avocado", "property": [{"code": "colour", "valueCode": "green"}]},
            {"code": "B1", "display": "Banana", "property": [{"code": "notSelectable", "valueBoolean": true}]},
            {"code": "B2", "display": "Blueberry"}
        ]
    })
}

fn inline_vs(include: Value) -> Value {
    json!({"resourceType": "ValueSet", "status": "active", "compose": {"include": [include]}})
}

fn inline_req(include: Value) -> ExpandRequest {
    ExpandRequest {
        value_set: Some(inline_vs(include)),
        ..Default::default()
    }
}

// ── Scenarios ─────────────────────────────────────────────────────────────────

/// `http://loinc.org/vs` (all of LOINC) and `/vs/<LP…>` (the codes under a
/// part) expand and validate alike, paged in byte order of the code.
#[tokio::test(flavor = "multi_thread")]
async fn loinc_implicit_value_sets() {
    let _serial = SERIAL.lock().await;
    let bs = backends().await;
    for (_, b, _) in &bs {
        import(b, &bundle(vec![loinc_like()])).await;
    }
    let all = same_on_all!(bs, |b| expand(b, url_req("http://loinc.org/vs")).await);
    assert_eq!(
        all.clone().unwrap().0,
        [
            "1000-9", "10000-8", "2345-7", "999-9", "Glu-1", "LP1-1", "LP2-2"
        ],
        "SQLite byte order: 1000-9 before 10000-8"
    );
    let page = same_on_all!(bs, |b| expand(
        b,
        ExpandRequest {
            count: Some(2),
            offset: Some(1),
            ..url_req("http://loinc.org/vs")
        }
    )
    .await);
    assert_eq!(page, Ok((vec!["10000-8".into(), "2345-7".into()], Some(7))));
    let part = same_on_all!(bs, |b| expand(b, url_req("http://loinc.org/vs/LP1-1"))
        .await);
    assert_eq!(part.clone().unwrap().1, Some(4));
    let v = same_on_all!(bs, |b| (
        validates(b, "http://loinc.org/vs", None, "999-9").await,
        validates(b, "http://loinc.org/vs/LP1-1", None, "2345-7").await,
        validates(b, "http://loinc.org/vs/LP1-1", None, "LP1-1").await,
        validates(b, "http://loinc.org/vs/LP1-1", None, "999-9").await,
    ));
    assert_eq!(v, (true, true, false, false));
}

/// A text filter on an implicit ValueSet keeps expansion order; on a stored
/// ValueSet the hits are typeahead-ranked. Totals are exact either way.
#[tokio::test(flavor = "multi_thread")]
async fn text_filter_order_and_total() {
    let _serial = SERIAL.lock().await;
    let bs = backends().await;
    let vs = json!({
        "resourceType": "ValueSet", "id": "all-loinc", "url": "http://p.test/vs/all-loinc",
        "status": "active", "compose": {"include": [{"system": "http://loinc.org"}]}
    });
    for (_, b, _) in &bs {
        import(b, &bundle(vec![loinc_like(), vs.clone()])).await;
    }
    let implicit = same_on_all!(bs, |b| expand(
        b,
        ExpandRequest {
            filter: Some("glucose".into()),
            ..url_req("http://loinc.org/vs")
        }
    )
    .await);
    assert_eq!(
        implicit.clone().unwrap().0,
        ["1000-9", "10000-8", "2345-7", "Glu-1", "LP2-2"]
    );
    // Twice: the second request is a cache hit and must not change anything.
    for _ in 0..2 {
        let stored = same_on_all!(bs, |b| expand(
            b,
            ExpandRequest {
                filter: Some("glucose".into()),
                count: Some(3),
                ..url_req("http://p.test/vs/all-loinc")
            }
        )
        .await);
        assert_eq!(
            stored,
            Ok((
                vec!["Glu-1".into(), "LP2-2".into(), "1000-9".into()],
                Some(5)
            ))
        );
    }
}

/// `?fhir_vs=ecl/…`, `?fhir_vs=refset/…` and compose `constraint` filters.
#[tokio::test(flavor = "multi_thread")]
async fn ecl_refsets_and_constraint_filters() {
    let _serial = SERIAL.lock().await;
    let bs = backends().await;
    let url = "http://example.org/cs-fvs";
    for (_, b, _) in &bs {
        import(b, &bundle(vec![hierarchy_cs(url, false)])).await;
        let n = b
            .import_refset_members(
                &ctx(),
                url,
                Some("1"),
                vec![
                    ("rs1".into(), "child1".into()),
                    ("rs1".into(), "grand1".into()),
                ],
            )
            .await
            .unwrap();
        assert_eq!(n, 2);
    }
    let ecl = same_on_all!(bs, |b| expand(
        b,
        url_req(&format!("{url}?fhir_vs=ecl/%3C%3C%20child1"))
    )
    .await);
    assert_eq!(ecl.clone().unwrap().0, ["child1", "grand1"]);
    let refset = same_on_all!(bs, |b| expand(
        b,
        url_req(&format!("{url}?fhir_vs=refset/rs1"))
    )
    .await);
    assert_eq!(refset.clone().unwrap().0, ["child1", "grand1"]);
    let minus = same_on_all!(bs, |b| {
        let mut r = expand(
            b,
            inline_req(json!({"system": url, "filter": [
                {"property": "constraint", "op": "=", "value": "<< root MINUS ^ rs1"}
            ]})),
        )
        .await;
        if let Ok((codes, _)) = r.as_mut() {
            codes.sort();
        }
        r
    });
    assert_eq!(minus.clone().unwrap().0, ["child2", "root"]);
    let bad = same_on_all!(bs, |b| expand(
        b,
        inline_req(json!({"system": url, "filter": [
            {"property": "constraint", "op": "=", "value": "<< (root"}
        ]}))
    )
    .await);
    assert_eq!(bad, Err("invalid".to_string()));
    let v = same_on_all!(bs, |b| (
        validates(b, &format!("{url}?fhir_vs=refset/rs1"), Some(url), "grand1").await,
        validates(b, &format!("{url}?fhir_vs=refset/rs1"), Some(url), "child2").await,
        validates(
            b,
            &format!("{url}?fhir_vs=ecl/%3C%20root"),
            Some(url),
            "root"
        )
        .await,
    ));
    assert_eq!(v, (true, false, false));
}

/// Compose filter edge cases: missing value, `exists` on the concept itself,
/// hierarchy operators on a non-concept property, regex dialect and errors.
#[tokio::test(flavor = "multi_thread")]
async fn compose_filter_edge_cases() {
    let _serial = SERIAL.lock().await;
    let bs = backends().await;
    for (_, b, _) in &bs {
        import(b, &bundle(vec![property_cs()])).await;
    }
    let sys = "http://p.test/cs";
    let f = |filters: Value| inline_req(json!({"system": sys, "filter": filters}));
    let sorted = |r: Result<(Vec<String>, Option<u32>), String>| {
        r.map(|(mut c, t)| {
            c.sort();
            (c, t)
        })
    };
    let no_value = same_on_all!(bs, |b| expand(
        b,
        f(json!([{"property": "colour", "op": "="}]))
    )
    .await);
    assert_eq!(
        no_value,
        Err("vs-invalid: The system http://p.test/cs filter with property = colour, op = = has no value".to_string())
    );
    let exists_true = same_on_all!(bs, |b| sorted(
        expand(
            b,
            f(json!([{"property": "concept", "op": "exists", "value": "true"}]))
        )
        .await
    ));
    assert_eq!(exists_true.unwrap().0, ["A1", "A2", "B1", "B2"]);
    let exists_false = same_on_all!(bs, |b| expand(
        b,
        f(json!([{"property": "code", "op": "exists", "value": "false"}]))
    )
    .await);
    assert_eq!(exists_false.unwrap().0, Vec::<String>::new());
    let colour = same_on_all!(bs, |b| sorted(
        expand(
            b,
            f(json!([{"property": "colour", "op": "exists", "value": "true"}]))
        )
        .await
    ));
    assert_eq!(colour.unwrap().0, ["A1", "A2"]);
    let isa_on_property = same_on_all!(bs, |b| expand(
        b,
        f(json!([{"property": "colour", "op": "is-a", "value": "A1"}]))
    )
    .await);
    assert_eq!(isa_on_property.unwrap().0, Vec::<String>::new());
    let regex = same_on_all!(bs, |b| sorted(
        expand(
            b,
            f(json!([{"property": "code", "op": "regex", "value": "[AB]1"}]))
        )
        .await
    ));
    assert_eq!(regex.unwrap().0, ["A1", "B1"]);
    // `\d` is Rust-regex syntax (PostgreSQL's POSIX `~` would differ).
    let regex_d = same_on_all!(bs, |b| sorted(
        expand(
            b,
            f(json!([{"property": "code", "op": "regex", "value": "A\\d"}]))
        )
        .await
    ));
    assert_eq!(regex_d.unwrap().0, ["A1", "A2"]);
    let regex_bad = same_on_all!(bs, |b| expand(
        b,
        f(json!([{"property": "code", "op": "regex", "value": "A("}]))
    )
    .await);
    assert!(
        regex_bad
            .unwrap_err()
            .starts_with("vs-invalid: ValueSet compose filter has an invalid regular expression")
    );
    let not_in = same_on_all!(bs, |b| sorted(
        expand(
            b,
            f(json!([{"property": "notSelectable", "op": "not-in", "value": "true"}]))
        )
        .await
    ));
    assert_eq!(not_in.unwrap().0, ["A1", "A2", "B2"]);
}

/// `max_expansion_size` applies to every request without `count`, also when
/// an earlier paged request has already cached the expansion.
#[tokio::test(flavor = "multi_thread")]
async fn expansion_cap_holds_on_cache_hits() {
    let _serial = SERIAL.lock().await;
    let bs = backends().await;
    let vs = json!({
        "resourceType": "ValueSet", "id": "pvs", "url": "http://p.test/vs/props",
        "status": "active", "compose": {"include": [{"system": "http://p.test/cs"}]}
    });
    for (_, b, _) in &bs {
        import(b, &bundle(vec![property_cs(), vs.clone()])).await;
    }
    let capped = |count: Option<u32>, filter: Option<&str>| ExpandRequest {
        count,
        filter: filter.map(str::to_owned),
        max_expansion_size: Some(2),
        ..url_req("http://p.test/vs/props")
    };
    let paged = same_on_all!(bs, |b| expand(b, capped(Some(10), None)).await);
    assert_eq!(paged.unwrap().1, Some(4));
    let full = same_on_all!(bs, |b| expand(b, capped(None, None)).await);
    assert_eq!(full, Err("too-costly".to_string()));
    // The cap counts the filtered expansion.
    let filtered = same_on_all!(bs, |b| expand(b, capped(None, Some("apple"))).await);
    assert_eq!(filtered.unwrap().1, Some(1));
}

/// A request dated before the ValueSet is a 404 even right after the
/// undated expansion was cached.
#[tokio::test(flavor = "multi_thread")]
async fn dated_request_is_not_served_from_cache() {
    let _serial = SERIAL.lock().await;
    let bs = backends().await;
    let vs = json!({
        "resourceType": "ValueSet", "id": "dvs", "url": "http://p.test/vs/dated",
        "status": "active", "date": "2020-01-01",
        "compose": {"include": [{"system": "http://p.test/cs"}]}
    });
    for (_, b, _) in &bs {
        import(b, &bundle(vec![property_cs(), vs.clone()])).await;
    }
    let undated = same_on_all!(bs, |b| expand(b, url_req("http://p.test/vs/dated")).await);
    assert_eq!(undated.unwrap().1, Some(4));
    let early = same_on_all!(bs, |b| expand(
        b,
        ExpandRequest {
            date: Some("2010-01-01".into()),
            ..url_req("http://p.test/vs/dated")
        }
    )
    .await);
    assert_eq!(early, Err("not-found".to_string()));
}

/// A `force-system-version` request and an unpinned one, in either order,
/// each get their own version's codes (the persistent expansion cache must
/// not mix them).
#[tokio::test(flavor = "multi_thread")]
async fn version_pins_do_not_share_the_expansion_cache() {
    let _serial = SERIAL.lock().await;
    let bs = backends().await;
    let cs = |version: &str, codes: &[&str]| {
        json!({
            "resourceType": "CodeSystem", "id": format!("mv-{version}"), "url": "http://p.test/mv",
            "version": version, "status": "active", "content": "complete",
            "concept": codes.iter().map(|c| json!({"code": c, "display": c})).collect::<Vec<_>>()
        })
    };
    let vs = json!({
        "resourceType": "ValueSet", "id": "mvs", "url": "http://p.test/vs/mv",
        "status": "active", "compose": {"include": [{"system": "http://p.test/mv"}]}
    });
    for (_, b, _) in &bs {
        import(
            b,
            &bundle(vec![
                cs("1.0.0", &["a", "b"]),
                cs("2.0.0", &["a", "c"]),
                vs.clone(),
            ]),
        )
        .await;
    }
    let pinned = || ExpandRequest {
        force_system_versions: HashMap::from([(
            "http://p.test/mv".to_string(),
            "1.0.0".to_string(),
        )]),
        ..url_req("http://p.test/vs/mv")
    };
    let sorted = |r: Result<(Vec<String>, Option<u32>), String>| {
        r.map(|(mut c, _)| {
            c.sort();
            c
        })
    };
    let first = same_on_all!(bs, |b| sorted(expand(b, pinned()).await));
    assert_eq!(first.unwrap(), ["a", "b"]);
    let unpinned = same_on_all!(bs, |b| sorted(
        expand(b, url_req("http://p.test/vs/mv")).await
    ));
    assert_eq!(unpinned.unwrap(), ["a", "c"]);
    let again = same_on_all!(bs, |b| sorted(expand(b, pinned()).await));
    assert_eq!(again.unwrap(), ["a", "b"]);
}

/// Re-importing a CodeSystem rebuilds its closure: is-a and `isa/` see the
/// new hierarchy without a restart.
#[tokio::test(flavor = "multi_thread")]
async fn reimport_rebuilds_the_closure() {
    let _serial = SERIAL.lock().await;
    let bs = backends().await;
    let url = "http://example.org/cs-reimport";
    for (_, b, _) in &bs {
        import(b, &bundle(vec![hierarchy_cs(url, false)])).await;
    }
    let isa = |b: &Backend| {
        let b = b.clone();
        async move {
            let mut r = expand(
                &b,
                inline_req(json!({"system": url, "filter": [
                    {"property": "concept", "op": "is-a", "value": "root"}
                ]})),
            )
            .await
            .unwrap()
            .0;
            r.sort();
            r
        }
    };
    let before = same_on_all!(bs, |b| isa(b).await);
    assert_eq!(before, ["child1", "child2", "grand1", "root"]);
    for (_, b, _) in &bs {
        import(b, &bundle(vec![hierarchy_cs(url, true)])).await;
    }
    let after = same_on_all!(bs, |b| isa(b).await);
    assert_eq!(after, ["child1", "child2", "child3", "grand1", "root"]);
    let implicit = same_on_all!(bs, |b| expand(
        b,
        url_req(&format!("{url}?fhir_vs=isa/root"))
    )
    .await);
    assert_eq!(implicit.unwrap().1, Some(5));
}

/// One resource that cannot be stored does not take the rest of the bundle
/// with it; the failure is reported.
#[tokio::test(flavor = "multi_thread")]
async fn a_failing_resource_does_not_discard_the_bundle() {
    let _serial = SERIAL.lock().await;
    let bs = backends().await;
    let good = json!({
        "resourceType": "CodeSystem", "id": "good", "url": "http://p.test/good",
        "status": "active", "content": "complete", "concept": [{"code": "g", "display": "G"}]
    });
    let vs = json!({
        "resourceType": "ValueSet", "id": "gvs", "url": "http://p.test/vs/good",
        "status": "active", "compose": {"include": [{"system": "http://p.test/good"}]}
    });
    // No `url`: a parse error on both backends.
    let broken = json!({"resourceType": "CodeSystem", "id": "broken", "status": "active"});
    let stored = same_on_all!(bs, |b| {
        let stats = b
            .import_bundle(
                &ctx(),
                &serde_json::to_vec(&bundle(vec![good.clone(), broken.clone(), vs.clone()]))
                    .unwrap(),
            )
            .await
            .unwrap();
        (
            stats.code_systems,
            stats.value_sets,
            !stats.errors.is_empty(),
        )
    });
    assert_eq!(stored, (1, 1, true));
    let codes = same_on_all!(bs, |b| expand(b, url_req("http://p.test/vs/good")).await);
    assert_eq!(codes.unwrap().0, ["g"]);
}

/// `$translate` against a ConceptMap url that is not loaded is a 404; a map
/// re-imported under a new id is found under that id.
#[tokio::test(flavor = "multi_thread")]
async fn translate_and_concept_map_replacement() {
    let _serial = SERIAL.lock().await;
    let bs = backends().await;
    let cm = |id: &str, target: &str| {
        json!({
            "resourceType": "ConceptMap", "id": id, "url": "http://p.test/cm",
            "status": "active", "sourceUri": "http://p.test/cs", "targetUri": "http://p.test/t",
            "group": [{"source": "http://p.test/cs", "target": "http://p.test/t",
                       "element": [{"code": "A1", "target": [{"code": target, "equivalence": "equivalent"}]}]}]
        })
    };
    for (_, b, _) in &bs {
        import(b, &bundle(vec![cm("cm-a", "T1")])).await;
        import(b, &bundle(vec![cm("cm-b", "T2")])).await;
    }
    let tr = |url: Option<&str>| TranslateRequest {
        url: url.map(str::to_owned),
        system: Some("http://p.test/cs".into()),
        code: "A1".into(),
        ..Default::default()
    };
    let unknown = same_on_all!(bs, |b| b
        .translate(&ctx(), tr(Some("http://p.test/cm-missing")))
        .await
        .map(|r| r.result)
        .map_err(|e| error_kind(&e)));
    assert_eq!(unknown, Err("not-found".to_string()));
    let hit = same_on_all!(bs, |b| b
        .translate(&ctx(), tr(Some("http://p.test/cm")))
        .await
        .map(|r| r
            .matches
            .iter()
            .map(|m| m.concept_code.clone())
            .collect::<Vec<_>>())
        .map_err(|e| error_kind(&e)));
    assert_eq!(hit.unwrap(), ["T2"]);
    let ids = same_on_all!(bs, |b| (
        b.resource_url_by_id("ConceptMap", "cm-b"),
        b.resource_url_by_id("ConceptMap", "cm-a"),
    ));
    assert_eq!(ids, (Some("http://p.test/cm".to_string()), None));
}

/// Inline ValueSets: warnings for what could not be resolved, a 404 when
/// nothing could, `#contained` and `tx-resource` references, and cycles
/// (a warning in an include, an error in an exclude).
#[tokio::test(flavor = "multi_thread")]
async fn inline_references_warnings_and_cycles() {
    let _serial = SERIAL.lock().await;
    let bs = backends().await;
    let vs_a = json!({
        "resourceType": "ValueSet", "id": "cyc-a", "url": "http://p.test/vs/cyc-a", "status": "active",
        "compose": {"include": [{"valueSet": ["http://p.test/vs/cyc-b"]}, {"system": "http://p.test/cs", "concept": [{"code": "A1"}]}]}
    });
    let vs_b = json!({
        "resourceType": "ValueSet", "id": "cyc-b", "url": "http://p.test/vs/cyc-b", "status": "active",
        "compose": {"include": [{"valueSet": ["http://p.test/vs/cyc-a"]}]}
    });
    for (_, b, _) in &bs {
        import(b, &bundle(vec![property_cs(), vs_a.clone(), vs_b.clone()])).await;
    }
    let run = |b: &Backend, req: ExpandRequest| {
        let b = b.clone();
        async move {
            b.expand(&ctx(), req)
                .await
                .map(|r| {
                    let (mut codes, total) = shape(&r);
                    codes.sort();
                    (codes, total, r.warnings)
                })
                .map_err(|e| error_kind(&e))
        }
    };
    let vs = |compose: Value| ExpandRequest {
        value_set: Some(
            json!({"resourceType": "ValueSet", "status": "active", "compose": compose}),
        ),
        ..Default::default()
    };
    let nothing = same_on_all!(bs, |b| run(
        b,
        vs(json!({"include": [{"system": "http://nowhere.test/cs"}]}))
    )
    .await);
    assert_eq!(nothing, Err("not-found".to_string()));
    let partial = same_on_all!(bs, |b| run(
        b,
        vs(json!({"include": [
            {"system": "http://nowhere.test/cs"},
            {"system": "http://p.test/cs", "concept": [{"code": "B2"}]}
        ]}))
    )
    .await);
    let partial = partial.unwrap();
    assert_eq!(partial.0, ["B2"]);
    assert_eq!(
        partial.2,
        [
            "CodeSystem http://nowhere.test/cs was not found and has been excluded from the expansion"
        ]
    );
    let contained = same_on_all!(bs, |b| {
        run(
        b,
        ExpandRequest {
            value_set: Some(json!({
                "resourceType": "ValueSet", "status": "active",
                "contained": [{"resourceType": "ValueSet", "id": "c1", "status": "active",
                               "compose": {"include": [{"system": "http://p.test/cs", "concept": [{"code": "A2"}]}]}}],
                "compose": {"include": [{"valueSet": ["#c1"]}, {"valueSet": ["#missing"]}]}
            })),
            ..Default::default()
        }
    )
    .await
    });
    let contained = contained.unwrap();
    assert_eq!(contained.0, ["A2"]);
    assert_eq!(
        contained.2,
        ["Referenced contained ValueSet #missing not found; excluded from expansion"]
    );
    let tx = same_on_all!(bs, |b| {
        run(
        b,
        ExpandRequest {
            tx_resources: vec![json!({
                "resourceType": "ValueSet", "url": "http://p.test/vs/tx-only", "status": "active",
                "compose": {"include": [{"system": "http://p.test/cs", "concept": [{"code": "B1"}]}]}
            })],
            ..vs(json!({"include": [{"valueSet": ["http://p.test/vs/tx-only"]}]}))
        }
    )
    .await
    });
    assert_eq!(tx.unwrap().0, ["B1"]);
    let include_cycle = same_on_all!(bs, |b| run(
        b,
        vs(json!({"include": [{"valueSet": ["http://p.test/vs/cyc-a"]}]}))
    )
    .await);
    let include_cycle = include_cycle.unwrap();
    assert_eq!(include_cycle.0, ["A1"]);
    assert_eq!(
        include_cycle.2,
        [
            "Cyclic ValueSet reference detected for http://p.test/vs/cyc-a; excluded from expansion (vs-invalid)"
        ]
    );
    let exclude_cycle = same_on_all!(bs, |b| {
        run(
        b,
        vs(json!({"include": [{"system": "http://p.test/cs"}], "exclude": [{"valueSet": ["http://p.test/vs/cyc-a"]}]}))
    )
    .await
    });
    assert!(exclude_cycle.unwrap_err().starts_with(
        "vs-invalid: Cyclic reference detected when excluding http://p.test/vs/cyc-a via"
    ));
}

/// Two includes of one system that overlap: both backends return the same
/// expansion.
#[tokio::test(flavor = "multi_thread")]
async fn overlapping_includes() {
    let _serial = SERIAL.lock().await;
    let bs = backends().await;
    for (_, b, _) in &bs {
        import(
            b,
            &bundle(vec![
                property_cs(),
                hierarchy_cs("http://example.org/cs-ov", false),
            ]),
        )
        .await;
    }
    let vs = |compose: Value| ExpandRequest {
        value_set: Some(
            json!({"resourceType": "ValueSet", "status": "active", "compose": compose}),
        ),
        ..Default::default()
    };
    let sorted = |r: Result<(Vec<String>, Option<u32>), String>| {
        r.map(|(mut c, t)| {
            c.sort();
            (c, t)
        })
    };
    let props = same_on_all!(bs, |b| {
        sorted(
        expand(
            b,
            vs(json!({"include": [
                {"system": "http://p.test/cs", "filter": [{"property": "colour", "op": "exists", "value": "true"}]},
                {"system": "http://p.test/cs", "filter": [{"property": "colour", "op": "=", "value": "red"}]}
            ]}))
        )
        .await
    )
    });
    // Both backends currently repeat a code two includes select (a shared
    // defect, recorded here so a change on either side is noticed).
    assert_eq!(props.unwrap().0, ["A1", "A1", "A2"]);
    let hier = same_on_all!(bs, |b| {
        sorted(
        expand(
            b,
            vs(json!({"include": [
                {"system": "http://example.org/cs-ov", "filter": [{"property": "concept", "op": "is-a", "value": "child1"}]},
                {"system": "http://example.org/cs-ov", "filter": [{"property": "concept", "op": "is-a", "value": "root"}]}
            ]}))
        )
        .await
    )
    });
    assert!(hier.is_ok());
}

/// Which expansions carry a per-code version (it decides how
/// `used-codesystem` is reported): none for implicit ValueSets and for
/// cacheable inline ones (SQLite serves those from indexes without it), the
/// CodeSystem version for stored ValueSets. Second pass = steady state.
#[tokio::test(flavor = "multi_thread")]
async fn contains_versions() {
    let _serial = SERIAL.lock().await;
    let bs = backends().await;
    for (_, b, _) in &bs {
        import(b, &bundle(vec![loinc_like()])).await;
    }
    let versions = |b: &Backend, req: ExpandRequest| {
        let b = b.clone();
        async move {
            let r = b.expand(&ctx(), req).await.unwrap();
            let mut v: Vec<Option<String>> = r.contains.iter().map(|c| c.version.clone()).collect();
            v.dedup();
            v
        }
    };
    let inline = |filter: Option<&str>| ExpandRequest {
        value_set: Some(inline_vs(json!({"system": "http://loinc.org"}))),
        filter: filter.map(str::to_owned),
        ..Default::default()
    };
    let vs = json!({
        "resourceType": "ValueSet", "id": "all-l", "url": "http://p.test/vs/all-l",
        "status": "active", "compose": {"include": [{"system": "http://loinc.org"}]}
    });
    for (_, b, _) in &bs {
        import(b, &bundle(vec![vs.clone()])).await;
        let _ = versions(b, inline(None)).await;
    }
    let implicit = same_on_all!(bs, |b| versions(b, url_req("http://loinc.org/vs")).await);
    assert_eq!(implicit, [None]);
    let plain = same_on_all!(bs, |b| versions(b, inline(None)).await);
    assert_eq!(plain, [None]);
    let filtered = same_on_all!(bs, |b| versions(b, inline(Some("glucose"))).await);
    assert_eq!(filtered, [None]);
    let stored = same_on_all!(bs, |b| versions(b, url_req("http://p.test/vs/all-l")).await);
    assert_eq!(stored, [Some("2.80".to_string())]);
}

/// A stored ValueSet whose includes overlap: the request that computes the
/// expansion gets the computed list (duplicates, compose order); later
/// requests with other paging are read from the persistent expansion cache
/// (one row per code, code order) — on both backends.
#[tokio::test(flavor = "multi_thread")]
async fn stored_expansion_first_and_cached_shape() {
    let _serial = SERIAL.lock().await;
    let bs = backends().await;
    let url = "http://example.org/cs-shape";
    let vs = json!({
        "resourceType": "ValueSet", "id": "shape", "url": "http://p.test/vs/shape", "status": "active",
        "compose": {"include": [
            {"system": url, "concept": [{"code": "grand1"}, {"code": "child2"}]},
            {"system": url, "filter": [{"property": "concept", "op": "is-a", "value": "child1"}]}
        ]}
    });
    for (_, b, _) in &bs {
        import(b, &bundle(vec![hierarchy_cs(url, false), vs.clone()])).await;
    }
    let req = |count: Option<u32>, offset: Option<u32>| ExpandRequest {
        count,
        offset,
        ..url_req("http://p.test/vs/shape")
    };
    // Within the first answer, SQLite's order for the enumerated include
    // follows its query plan (code order for a tiny system, the listed order
    // otherwise); PostgreSQL keeps the listed order. Compare as multisets.
    let first = same_on_all!(bs, |b| expand(b, req(Some(10), None)).await.map(
        |(mut c, t)| {
            c.sort();
            (c, t)
        }
    ));
    assert_eq!(
        first,
        Ok((
            vec![
                "child1".into(),
                "child2".into(),
                "grand1".into(),
                "grand1".into()
            ],
            Some(4)
        ))
    );
    let later = same_on_all!(bs, |b| expand(b, req(Some(10), Some(1))).await);
    assert_eq!(later, Ok((vec!["child2".into(), "grand1".into()], Some(3))));
}

/// Supplement designations and properties, before and after the supplement
/// is imported (the PostgreSQL metadata memos must notice the new
/// supplement), and the base CodeSystem's inline concept entries.
#[tokio::test(flavor = "multi_thread")]
async fn supplements_reach_lookup_helpers_after_import() {
    use helios_hts::traits::CodeSystemOperations;
    let _g = SERIAL.lock().await;
    let bs = backends().await;
    let base = json!({
        "resourceType": "CodeSystem", "id": "supp-base", "url": "http://parity.test/supp-base",
        "version": "1", "status": "active", "content": "complete",
        "concept": [{"code": "a", "display": "Alpha"}, {"code": "b", "display": "Beta"}]
    });
    let supp = json!({
        "resourceType": "CodeSystem", "id": "supp-de", "url": "http://parity.test/supp-de",
        "version": "2", "status": "active", "content": "supplement",
        "supplements": "http://parity.test/supp-base",
        "concept": [{"code": "a", "designation": [{"language": "de", "value": "Alfa"}],
                     "property": [{"code": "note", "valueString": "n-a"}]}]
    });
    for (_, b, _) in &bs {
        import(b, &bundle(vec![base.clone()])).await;
    }
    let urls = vec!["http://parity.test/supp-de".to_string()];
    let codes = vec!["a".to_string(), "b".to_string()];
    let probe = |b: &Backend| {
        let (urls, codes) = (urls.clone(), codes.clone());
        let b = b.clone();
        async move {
            let target = b
                .supplement_target(&ctx(), "http://parity.test/supp-de")
                .await
                .unwrap()
                .map(|i| (i.target_url, i.supplement_canonical));
            let mut d: Vec<(String, String, Option<String>)> = b
                .supplement_designations(&ctx(), &urls, &codes)
                .await
                .unwrap()
                .into_iter()
                .flat_map(|(c, v)| v.into_iter().map(move |x| (c.clone(), x.value, x.source)))
                .collect();
            d.sort();
            let mut p: Vec<(String, String, String)> = b
                .supplement_property_values(&ctx(), &urls, &codes, &[])
                .await
                .unwrap()
                .into_iter()
                .flat_map(|(c, v)| v.into_iter().map(move |(k, x)| (c.clone(), k, x)))
                .collect();
            p.sort();
            let mut entries: Vec<String> = b
                .concept_resource_entries(&ctx(), "http://parity.test/supp-base", &codes)
                .await
                .unwrap()
                .into_keys()
                .collect();
            entries.sort();
            let version = b
                .code_system_version_for_url(&ctx(), "http://parity.test/supp-base")
                .await
                .unwrap();
            (target, d, p, entries, version)
        }
    };
    let before = same_on_all!(bs, |b| probe(b).await);
    assert_eq!(before.0, None);
    assert!(before.1.is_empty() && before.2.is_empty());
    assert_eq!(before.3, ["a", "b"]);
    for (_, b, _) in &bs {
        import(b, &bundle(vec![supp.clone()])).await;
    }
    let after = same_on_all!(bs, |b| probe(b).await);
    assert_eq!(
        after.0,
        Some((
            "http://parity.test/supp-base".into(),
            "http://parity.test/supp-de|2".into()
        ))
    );
    assert_eq!(
        after.1,
        [(
            "a".to_string(),
            "Alfa".to_string(),
            Some("http://parity.test/supp-de|2".to_string())
        )]
    );
    assert_eq!(
        after.2,
        [("a".to_string(), "note".to_string(), "n-a".to_string())]
    );
    assert_eq!(after.4.as_deref(), Some("1"));
}
