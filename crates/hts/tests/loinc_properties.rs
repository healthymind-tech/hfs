//! LOINC properties (axis columns, part links, part names) on both backends:
//! property filters by part code and by part name, `$lookup` Coding
//! properties, part-name designations, and the `loinc-properties` backfill
//! of an import that predates them.
//!
//! The PostgreSQL half needs the `postgres` feature and a server (Docker for
//! testcontainers, or `HTS_TEST_PG_URL`).

#![cfg(feature = "sqlite")]

mod common;

use std::io::Write;
use std::sync::Arc;

use helios_hts::backends::SqliteTerminologyBackend;
use helios_hts::import::loinc_csv::{backfill_loinc_properties, import_loinc_csv};
use helios_hts::import::{BundleImportBackend, LanguageFilter};
use axum::Router;
use helios_hts::traits::{TerminologyBackend, ValueSetOperations};
use helios_hts::types::{ExpandRequest, LookupRequest, ValidateCodeRequest};
use helios_persistence::tenant::TenantContext;
use serde_json::{Value, json};

trait Both: TerminologyBackend + BundleImportBackend {}
impl<T: TerminologyBackend + BundleImportBackend> Both for T {}
type Backend = Arc<dyn Both>;

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn ctx() -> TenantContext {
    TenantContext::system()
}

/// Each backend with the HTTP app over it (the display checks of
/// `$validate-code` live in the operation layer).
async fn backends() -> Vec<(&'static str, Backend, Router, Option<tempfile::TempDir>)> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hts.db");
    let sqlite = SqliteTerminologyBackend::new(path.to_str().unwrap()).unwrap();
    let config = helios_hts::config::HtsConfig::default();
    let router = helios_hts::server::create_app(
        &config,
        helios_hts::state::AppState::new(sqlite.clone()),
    );
    #[allow(unused_mut)]
    let mut out: Vec<(&'static str, Backend, Router, Option<tempfile::TempDir>)> =
        vec![("sqlite", Arc::new(sqlite), router, Some(dir))];
    #[cfg(feature = "postgres")]
    {
        let url = common::fresh_pg_database().await;
        let pg = helios_hts::backends::PostgresTerminologyBackend::new(&url)
            .await
            .unwrap();
        let router =
            helios_hts::server::create_app(&config, helios_hts::state::AppState::new(pg.clone()));
        out.push(("postgres", Arc::new(pg), router, None));
    }
    out
}

/// `GET path` on `app`, as JSON.
async fn get_json(app: &Router, path: &str) -> Value {
    use tower::ServiceExt;
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::get(path)
                .header("Accept", "application/fhir+json")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

fn param<'a>(params: &'a Value, name: &str) -> Option<&'a Value> {
    params["parameter"]
        .as_array()?
        .iter()
        .find(|p| p["name"] == name)
}

const LOINC_CSV: &str = "\
LOINC_NUM,COMPONENT,PROPERTY,TIME_ASPCT,SYSTEM,SCALE_TYP,METHOD_TYP,CLASS,STATUS,CLASSTYPE,ORDER_OBS,LONG_COMMON_NAME\r\n\
34133-9,Summary of episode note,Find,Pt,{Setting},Doc,{Role},DOC.ONTOLOGY,ACTIVE,2,Both,Summary of episode note\r\n\
18842-5,Discharge summary,Find,Pt,{Setting},Doc,{Role},DOC.ONTOLOGY,ACTIVE,2,Both,Discharge summary\r\n\
29545-1,Physical findings,Find,Pt,^Patient,Nar,Observed,H&P.PX,ACTIVE,2,Observation,Physical findings Narrative\r\n\
718-7,Hemoglobin,MCnc,Pt,Bld,Qn,,HEM/BC,ACTIVE,1,Both,Hemoglobin [Mass/volume] in Blood\r\n";

const HIERARCHY_CSV: &str = "\
PATH_TO_ROOT,SEQUENCE,IMMEDIATE_PARENT,CODE,CODE_TEXT\r\n\
LP7786-3,1,,LP7786-3,Laboratory\r\n\
LP7786-3.718-7,2,LP7786-3,718-7,Hemoglobin\r\n";

const PART_CSV: &str = "\
PartNumber,PartTypeName,PartName,PartDisplayName,Status\r\n\
LP32888-7,SCALE,Doc,Doc,ACTIVE\r\n\
LP441214-6,PROPERTY,Doc,Doc,ACTIVE\r\n\
LP7753-9,SCALE,Qn,Quantitative,ACTIVE\r\n\
LP7749-7,SCALE,Nar,Nar,ACTIVE\r\n\
LP7057-5,SYSTEM,Bld,Blood,ACTIVE\r\n\
LP32519-8,CLASS,DOC.ONTOLOGY,Document ontology,ACTIVE\r\n\
LP29684-5,CLASS,RAD,Radiology,ACTIVE\r\n";

const LINK_HEADER: &str = "LoincNumber,LongCommonName,PartNumber,PartName,PartCodeSystem,PartTypeName,LinkTypeName,Property\r\n";

fn link(loinc: &str, part: &str, kind: &str, property: &str) -> String {
    format!("{loinc},x,{part},x,http://loinc.org,x,{kind},http://loinc.org/property/{property}\r\n")
}

fn primary_links() -> String {
    let mut s = LINK_HEADER.to_string();
    for code in ["34133-9", "18842-5"] {
        s += &link(code, "LP32888-7", "Primary", "SCALE_TYP");
    }
    s += &link("29545-1", "LP7749-7", "Primary", "SCALE_TYP");
    s += &link("718-7", "LP7753-9", "Primary", "SCALE_TYP");
    s += &link("718-7", "LP7057-5", "Primary", "SYSTEM");
    s
}

fn supplementary_links() -> String {
    let mut s = LINK_HEADER.to_string();
    for code in ["34133-9", "18842-5"] {
        s += &link(code, "LP32519-8", "Metadata", "CLASS");
        // A DetailedModel duplicate of the primary link: stored once.
        s += &link(code, "LP32888-7", "DetailedModel", "SCALE_TYP");
        // Search keywords are not stored.
        s += &link(code, "LP7753-9", "Search", "search");
    }
    // A link into another code system is not a LOINC part.
    s += "718-7,x,RID123,x,http://www.radlex.org,x,Radiology,http://loinc.org/property/rad-modality-type\r\n";
    s
}

fn zip(files: &[(&str, &str)]) -> tempfile::NamedTempFile {
    let tmp = tempfile::Builder::new()
        .prefix("Loinc_2.80")
        .suffix(".zip")
        .tempfile()
        .unwrap();
    {
        let mut zip = zip::ZipWriter::new(tmp.reopen().unwrap());
        let opts = zip::write::FileOptions::default();
        for (name, body) in files {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }
    tmp
}

fn full_zip() -> tempfile::NamedTempFile {
    let primary = primary_links();
    let supplementary = supplementary_links();
    zip(&[
        ("LoincTable/Loinc.csv", LOINC_CSV),
        ("AccessoryFiles/MultiAxialHierarchy/MultiAxialHierarchy.csv", HIERARCHY_CSV),
        ("AccessoryFiles/PartFile/Part.csv", PART_CSV),
        ("AccessoryFiles/PartFile/LoincPartLink_Primary.csv", &primary),
        ("AccessoryFiles/PartFile/LoincPartLink_Supplementary.csv", &supplementary),
    ])
}

async fn import(b: &Backend, file: &tempfile::NamedTempFile) {
    let stats = import_loinc_csv(
        &**b,
        &ctx(),
        file.path(),
        2,
        false,
        &LanguageFilter::default(),
    )
    .await
    .expect("LOINC import");
    assert!(stats.errors.is_empty(), "{:?}", stats.errors);
}

fn include(filters: Vec<Value>) -> Value {
    json!({"system": "http://loinc.org", "filter": filters})
}

fn filter(property: &str, op: &str, value: &str) -> Value {
    json!({"property": property, "op": op, "value": value})
}

async fn expand_inline(b: &Backend, includes: Vec<Value>) -> Vec<String> {
    let vs = json!({
        "resourceType": "ValueSet",
        "status": "active",
        "compose": {"include": includes},
    });
    let mut codes: Vec<String> = b
        .expand(
            &ctx(),
            ExpandRequest {
                value_set: Some(vs),
                ..Default::default()
            },
        )
        .await
        .expect("expand")
        .contains
        .into_iter()
        .map(|c| c.code)
        .collect();
    codes.sort();
    codes
}

async fn validates(b: &Backend, url: &str, code: &str) -> bool {
    ValueSetOperations::validate_code(
        &**b,
        &ctx(),
        ValidateCodeRequest {
            url: Some(url.into()),
            system: Some("http://loinc.org".into()),
            code: code.into(),
            ..Default::default()
        },
    )
    .await
    .unwrap_or_else(|e| panic!("validate {url} {code}: {e:?}"))
    .result
}

/// Checks every property-backed behaviour on one backend.
async fn assert_properties(name: &str, b: &Backend, app: &Router) {
    let docs = vec!["18842-5".to_string(), "34133-9".to_string()];
    // By name, in any case (US Core uses `DOC`; Loinc.csv says `Doc`).
    for value in ["DOC", "Doc", "doc"] {
        assert_eq!(
            expand_inline(b, vec![include(vec![filter("SCALE_TYP", "=", value)])]).await,
            docs,
            "{name}: SCALE_TYP = {value}"
        );
    }
    // By part code (NHI CI uses LP codes), and two includes.
    assert_eq!(
        expand_inline(
            b,
            vec![
                include(vec![filter("SCALE_TYP", "=", "LP32888-7")]),
                include(vec![filter("SCALE_TYP", "=", "LP7749-7")]),
            ]
        )
        .await,
        ["18842-5", "29545-1", "34133-9"],
        "{name}: SCALE_TYP by part code"
    );
    assert_eq!(
        expand_inline(b, vec![include(vec![filter("SCALE_TYP", "in", "Doc,Qn")])]).await,
        ["18842-5", "34133-9", "718-7"],
        "{name}: SCALE_TYP in names"
    );
    assert_eq!(
        expand_inline(b, vec![include(vec![filter("SCALE_TYP", "!=", "DOC")])])
            .await
            .into_iter()
            .filter(|c| !c.starts_with("LP"))
            .collect::<Vec<_>>(),
        ["29545-1", "718-7"],
        "{name}: SCALE_TYP != DOC"
    );
    // CLASS from the supplementary links, by name and by code.
    for value in ["DOC.ONTOLOGY", "LP32519-8"] {
        assert_eq!(
            expand_inline(b, vec![include(vec![filter("CLASS", "=", value)])]).await,
            docs,
            "{name}: CLASS = {value}"
        );
    }
    // Two properties AND-ed, and string properties from Loinc.csv.
    assert_eq!(
        expand_inline(
            b,
            vec![include(vec![
                filter("SCALE_TYP", "=", "Qn"),
                filter("SYSTEM", "=", "Bld"),
            ])]
        )
        .await,
        ["718-7"],
        "{name}: SCALE_TYP + SYSTEM"
    );
    assert_eq!(
        expand_inline(b, vec![include(vec![filter("CLASSTYPE", "=", "1")])]).await,
        ["718-7"],
        "{name}: CLASSTYPE"
    );
    assert_eq!(
        expand_inline(b, vec![include(vec![filter("ORDER_OBS", "=", "Observation")])]).await,
        ["29545-1"],
        "{name}: ORDER_OBS"
    );
    // A name no part has selects nothing.
    assert!(
        expand_inline(b, vec![include(vec![filter("SCALE_TYP", "=", "Nonsense")])])
            .await
            .is_empty(),
        "{name}: unknown part name"
    );

    // $validate-code against a stored ValueSet (the US Core shape).
    let vs = json!({"resourceType": "Bundle", "type": "collection", "entry": [{"resource": {
        "resourceType": "ValueSet", "id": "doc-types", "url": "http://example.org/vs/doc-types",
        "status": "active",
        "compose": {"include": [include(vec![filter("SCALE_TYP", "=", "DOC")])]},
    }}]});
    b.import_bundle(&ctx(), &serde_json::to_vec(&vs).unwrap())
        .await
        .unwrap();
    assert!(validates(b, "http://example.org/vs/doc-types", "34133-9").await, "{name}");
    assert!(!validates(b, "http://example.org/vs/doc-types", "718-7").await, "{name}");

    // $lookup: Coding properties, without the search links.
    let term = b
        .lookup(
            &ctx(),
            LookupRequest {
                system: "http://loinc.org".into(),
                code: "34133-9".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let props: Vec<(&str, &str, &str)> = term
        .properties
        .iter()
        .map(|p| (p.code.as_str(), p.value_type.as_str(), p.value.as_str()))
        .collect();
    assert!(props.contains(&("SCALE_TYP", "coding", "LP32888-7")), "{name}: {props:?}");
    assert!(props.contains(&("CLASS", "coding", "LP32519-8")), "{name}: {props:?}");
    assert!(props.contains(&("CLASSTYPE", "string", "2")), "{name}: {props:?}");
    assert_eq!(
        props.iter().filter(|p| p.0 == "SCALE_TYP").count(),
        1,
        "{name}: duplicate links are stored once: {props:?}"
    );
    assert!(!props.iter().any(|p| p.0 == "search"), "{name}: {props:?}");
    let hgb = b
        .lookup(
            &ctx(),
            LookupRequest {
                system: "http://loinc.org".into(),
                code: "718-7".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        !hgb.properties.iter().any(|p| p.code == "rad-modality-type"),
        "{name}: links into other systems are not LOINC parts"
    );

    // Part names are designations: `Bld` is a valid display of LP7057-5.
    let designations = b
        .concept_designations(&ctx(), "http://loinc.org", &["LP7057-5".to_string()])
        .await
        .unwrap();
    let values: Vec<&str> = designations
        .get("LP7057-5")
        .map(|d| d.iter().map(|d| d.value.as_str()).collect())
        .unwrap_or_default();
    assert!(values.contains(&"Bld"), "{name}: {values:?}");
    for display in ["Bld", "Blood"] {
        let out = get_json(
            app,
            &format!(
                "/CodeSystem/$validate-code?url=http://loinc.org&code=LP7057-5&display={display}"
            ),
        )
        .await;
        assert_eq!(
            param(&out, "result").and_then(|p| p["valueBoolean"].as_bool()),
            Some(true),
            "{name}: display {display}: {out}"
        );
    }
    let wrong = get_json(
        app,
        "/CodeSystem/$validate-code?url=http://loinc.org&code=LP7057-5&display=Serum",
    )
    .await;
    assert_eq!(
        param(&wrong, "result").and_then(|p| p["valueBoolean"].as_bool()),
        Some(false),
        "{name}: a wrong display is still wrong: {wrong}"
    );
    // $lookup renders the part links as Codings of LOINC.
    let lookup = get_json(app, "/CodeSystem/$lookup?system=http://loinc.org&code=34133-9").await;
    let scale = lookup["parameter"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "property" && p["part"][0]["valueCode"] == "SCALE_TYP")
        .unwrap_or_else(|| panic!("{name}: {lookup}"));
    assert_eq!(
        scale["part"][1]["valueCoding"],
        json!({"system": "http://loinc.org", "code": "LP32888-7"}),
        "{name}"
    );
}

#[tokio::test]
async fn loinc_property_filters_lookup_and_part_names() {
    let _serial = SERIAL.lock().await;
    let file = full_zip();
    for (name, b, app, _dir) in backends().await {
        import(&b, &file).await;
        assert_properties(name, &b, &app).await;
    }
}

/// Imports `file` the way HTS did before it stored LOINC properties
/// (`HTS_LOINC_PROPERTIES=false`: concepts, parts and answers only). Every
/// test in this binary holds [`SERIAL`], so the variable is not raced.
async fn import_without_properties(b: &Backend, file: &tempfile::NamedTempFile) {
    // SAFETY: the tests of this binary run one at a time (SERIAL), and no
    // other thread reads the environment meanwhile.
    unsafe { std::env::set_var("HTS_LOINC_PROPERTIES", "false") };
    import(b, file).await;
    unsafe { std::env::remove_var("HTS_LOINC_PROPERTIES") };
}

async fn property_count(b: &Backend, code: &str) -> usize {
    b.lookup(
        &ctx(),
        LookupRequest {
            system: "http://loinc.org".into(),
            code: code.into(),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .properties
    .len()
}

#[tokio::test]
async fn backfill_adds_properties_to_an_existing_import_idempotently() {
    let _serial = SERIAL.lock().await;
    let full = full_zip();
    for (name, b, app, _dir) in backends().await {
        import_without_properties(&b, &full).await;
        let before = property_count(&b, "34133-9").await;
        assert!(
            expand_inline(&b, vec![include(vec![filter("SCALE_TYP", "=", "LP32888-7")])])
                .await
                .is_empty(),
            "{name}: no properties before the backfill"
        );
        // Dry run writes nothing.
        backfill_loinc_properties(&*b, &ctx(), full.path(), 1000, true)
            .await
            .unwrap();
        assert_eq!(property_count(&b, "34133-9").await, before, "{name}: dry run");
        let mut counts = Vec::new();
        for _ in 0..2 {
            // Batch size 1 → several write calls; only the first clears.
            let stats = backfill_loinc_properties(&*b, &ctx(), full.path(), 1, false)
                .await
                .unwrap();
            assert!(stats.errors.is_empty(), "{name}: {:?}", stats.errors);
            counts.push(property_count(&b, "34133-9").await);
        }
        assert!(counts[0] > before, "{name}: {counts:?}");
        assert_eq!(counts[0], counts[1], "{name}: a rerun replaces, not duplicates");
        let designations = b
            .concept_designations(&ctx(), "http://loinc.org", &["LP7057-5".to_string()])
            .await
            .unwrap();
        assert_eq!(
            designations.get("LP7057-5").map(Vec::len),
            Some(1),
            "{name}: one Bld designation after two runs"
        );
        assert_properties(name, &b, &app).await;
    }
}

#[tokio::test]
async fn backfill_of_a_missing_code_system_fails() {
    let _serial = SERIAL.lock().await;
    let full = full_zip();
    for (name, b, _app, _dir) in backends().await {
        let err = backfill_loinc_properties(&*b, &ctx(), full.path(), 1000, false).await;
        assert!(err.is_err(), "{name}: {err:?}");
    }
}
