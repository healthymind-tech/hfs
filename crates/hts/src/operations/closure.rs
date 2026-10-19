//! `POST /ConceptMap/$closure` — maintained closure tables.
//!
//! A client keeps a closure table (a transitive subsumption table over the
//! concepts it has seen) in step with the server
//! (<https://hl7.org/fhir/conceptmap-operation-closure.html>):
//!
//! * `name` (+ `reset=true`, or the first call for a name) starts a table at
//!   version 0. Code systems supplied with that request (`tx-resource`) are
//!   kept with the table — later calls name only concepts.
//! * each call with `concept`s adds the ones not yet in the table and
//!   returns, as a ConceptMap, the subsumption pairs the new concepts form
//!   with everything already there (source = the narrower concept, target =
//!   the wider one). A call that adds a concept moves the table's version on
//!   by one; one that adds nothing leaves it.
//! * `version` asks for every pair added after that version (resync).
//!
//! The answer is the ConceptMap itself (the operation's single `return`
//! output). Tables live in memory: a restart forgets them, and clients
//! re-initialise.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::{
    Json,
    extract::{RawQuery, State},
    http::{HeaderMap, header},
    response::Response,
};
use helios_persistence::tenant::TenantContext;
use serde_json::{Value, json};

use crate::error::HtsError;
use crate::state::AppState;
use crate::traits::{CodeSystemOperations, TerminologyBackend};
use crate::types::{SubsumesRequest, SubsumptionOutcome};

use super::format::{fhir_respond, negotiate_format};
use super::params::{extract_parameter_array, find_str_param};

/// One closure table.
#[derive(Default)]
pub struct ClosureTable {
    version: u64,
    /// The concepts in the table, in the order they were added.
    concepts: Vec<(String, String)>,
    /// `(version added, narrower (system, code), wider (system, code))`.
    entries: Vec<(u64, (String, String), (String, String))>,
    /// The request-supplied code systems the table was started with.
    #[cfg(feature = "sqlite")]
    overlay: Option<Arc<AppState<crate::backends::SqliteTerminologyBackend>>>,
}

/// The closure tables of a server, by name.
pub type ClosureTables = Arc<tokio::sync::Mutex<HashMap<String, ClosureTable>>>;

/// How `a` and `b` (codes of `system`) relate, from the table's code systems.
async fn subsumption<B: TerminologyBackend>(
    state: &AppState<B>,
    table: &ClosureTable,
    system: &str,
    a: &str,
    b: &str,
) -> Option<SubsumptionOutcome> {
    let req = SubsumesRequest {
        system: system.to_string(),
        version: None,
        code_a: a.to_string(),
        code_b: b.to_string(),
    };
    let ctx = TenantContext::system();
    #[cfg(feature = "sqlite")]
    if let Some(overlay) = table.overlay.as_ref() {
        return CodeSystemOperations::subsumes(overlay.backend(), &ctx, req)
            .await
            .ok()
            .map(|r| r.outcome);
    }
    #[cfg(not(feature = "sqlite"))]
    let _ = table;
    CodeSystemOperations::subsumes(state.backend(), &ctx, req)
        .await
        .ok()
        .map(|r| r.outcome)
}

/// The ConceptMap for `name` at `version` holding `entries`.
fn closure_concept_map(
    name: &str,
    version: u64,
    entries: &[&(u64, (String, String), (String, String))],
) -> Value {
    // system → narrower code → wider codes
    let mut groups: BTreeMap<&str, BTreeMap<&str, Vec<&str>>> = BTreeMap::new();
    for (_, (sys, narrow), (_, wide)) in entries {
        let targets = groups
            .entry(sys.as_str())
            .or_default()
            .entry(narrow.as_str())
            .or_default();
        if !targets.contains(&wide.as_str()) {
            targets.push(wide.as_str());
        }
    }
    let mut cm = json!({
        "resourceType": "ConceptMap",
        "name": name,
        "status": "active",
        "version": version.to_string(),
    });
    if !groups.is_empty() {
        cm["group"] = Value::Array(
            groups
                .into_iter()
                .map(|(sys, elements)| {
                    json!({
                        "source": sys,
                        "target": sys,
                        "element": elements.into_iter().map(|(code, mut wides)| {
                            wides.sort_unstable();
                            json!({
                                "code": code,
                                "target": wides.into_iter().map(|w| {
                                    #[cfg(any(feature = "R5", feature = "R6"))]
                                    let t = json!({"code": w, "relationship": "source-is-narrower-than-target"});
                                    #[cfg(not(any(feature = "R5", feature = "R6")))]
                                    let t = json!({"code": w, "equivalence": "wider"});
                                    t
                                }).collect::<Vec<_>>(),
                            })
                        }).collect::<Vec<_>>(),
                    })
                })
                .collect(),
        );
    }
    cm
}

/// The `$closure` operation.
pub(crate) async fn process_closure<B: TerminologyBackend>(
    state: &AppState<B>,
    params: Vec<Value>,
) -> Result<Value, HtsError> {
    let name = find_str_param(&params, "name")
        .filter(|n| !n.is_empty())
        .ok_or_else(|| HtsError::InvalidRequest("Missing required parameter: name".into()))?;
    let reset = find_str_param(&params, "reset").as_deref() == Some("true");
    let concepts: Vec<(String, String)> = params
        .iter()
        .filter(|p| p.get("name").and_then(|v| v.as_str()) == Some("concept"))
        .filter_map(|p| {
            let c = p.get("valueCoding")?;
            Some((
                c.get("system")?.as_str()?.to_string(),
                c.get("code")?.as_str()?.to_string(),
            ))
        })
        .collect();
    let since: Option<u64> = match find_str_param(&params, "version") {
        Some(v) => Some(v.parse().map_err(|_| {
            HtsError::InvalidRequest(format!("closure table version '{v}' is not a number"))
        })?),
        None => None,
    };

    let mut tables = state.closure_tables.lock().await;
    if reset || !tables.contains_key(&name) {
        let mut table = ClosureTable::default();
        #[cfg(feature = "sqlite")]
        {
            table.overlay = crate::tx_overlay::overlay_for(state, &params).await;
        }
        tables.insert(name.clone(), table);
    }
    let table = tables.get_mut(&name).expect("table just ensured");

    if let Some(v) = since {
        let entries: Vec<_> = table.entries.iter().filter(|e| e.0 > v).collect();
        return Ok(closure_concept_map(&name, table.version, &entries));
    }

    let first_new_version = table.version + 1;
    let mut added = false;
    for (system, code) in concepts {
        if table
            .concepts
            .iter()
            .any(|(s, c)| *s == system && *c == code)
        {
            continue;
        }
        if !added {
            table.version += 1;
            added = true;
        }
        let version = table.version;
        let mut new_entries = Vec::new();
        for (s, c) in &table.concepts {
            if *s != system {
                continue;
            }
            match subsumption(state, table, &system, &code, c).await {
                // The new concept is wider than one already there.
                Some(SubsumptionOutcome::Subsumes) => new_entries.push((
                    version,
                    (system.clone(), c.clone()),
                    (system.clone(), code.clone()),
                )),
                Some(SubsumptionOutcome::SubsumedBy) => new_entries.push((
                    version,
                    (system.clone(), code.clone()),
                    (system.clone(), c.clone()),
                )),
                _ => {}
            }
        }
        table.entries.extend(new_entries);
        table.concepts.push((system, code));
    }
    let entries: Vec<_> = table
        .entries
        .iter()
        .filter(|e| added && e.0 >= first_new_version)
        .collect();
    Ok(closure_concept_map(&name, table.version, &entries))
}

/// POST /ConceptMap/$closure
pub async fn closure_handler<B: TerminologyBackend>(
    State(state): State<AppState<B>>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, HtsError> {
    let params = extract_parameter_array(&body)?;
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    let format = negotiate_format(raw.as_deref(), accept);
    Ok(fhir_respond(process_closure(&state, params).await?, format))
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, http::Request, routing::post};
    use tower::ServiceExt;

    use crate::backends::sqlite::SqliteTerminologyBackend;
    use crate::import::BundleImportBackend;

    fn hierarchy_cs() -> Value {
        json!({"resourceType": "CodeSystem", "url": "http://example.org/cs", "version": "1.0",
        "status": "active", "content": "complete", "hierarchyMeaning": "is-a",
        "concept": [
            {"code": "A", "concept": [{"code": "B", "concept": [{"code": "C"}]}]},
            {"code": "D"}
        ]})
    }

    async fn make_app(stored: bool) -> Router {
        let backend = SqliteTerminologyBackend::in_memory().unwrap();
        if stored {
            let bundle = json!({"resourceType": "Bundle", "type": "collection",
                "entry": [{"resource": hierarchy_cs()}]});
            backend
                .import_bundle(&TenantContext::system(), bundle.to_string().as_bytes())
                .await
                .unwrap();
        }
        Router::new()
            .route(
                "/ConceptMap/$closure",
                post(closure_handler::<SqliteTerminologyBackend>),
            )
            .with_state(AppState::new(backend))
    }

    async fn call(app: &Router, params: Value) -> (u16, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ConceptMap/$closure")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"resourceType": "Parameters", "parameter": params}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn concept(code: &str) -> Value {
        json!({"name": "concept", "valueCoding": {"system": "http://example.org/cs", "code": code}})
    }

    fn pairs(cm: &Value) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for g in cm["group"].as_array().into_iter().flatten() {
            for e in g["element"].as_array().into_iter().flatten() {
                for t in e["target"].as_array().into_iter().flatten() {
                    out.push((
                        e["code"].as_str().unwrap().to_string(),
                        t["code"].as_str().unwrap().to_string(),
                    ));
                }
            }
        }
        out.sort();
        out
    }

    #[tokio::test]
    async fn closure_table_grows_one_concept_at_a_time() {
        let app = make_app(true).await;
        let name = json!({"name": "name", "valueString": "t1"});
        let (st, cm) = call(&app, json!([name, {"name": "reset", "valueBoolean": true}])).await;
        assert_eq!(st, 200, "{cm}");
        assert_eq!(cm["resourceType"], "ConceptMap");
        assert_eq!(cm["version"], "0");

        let (_, cm) = call(&app, json!([name, concept("B")])).await;
        assert_eq!(cm["version"], "1");
        assert!(pairs(&cm).is_empty());

        let (_, cm) = call(&app, json!([name, concept("C")])).await;
        assert_eq!(cm["version"], "2");
        assert_eq!(pairs(&cm), [("C".into(), "B".into())]);

        // An ancestor of both: two new pairs.
        let (_, cm) = call(&app, json!([name, concept("A")])).await;
        assert_eq!(cm["version"], "3");
        assert_eq!(
            pairs(&cm),
            [("B".into(), "A".into()), ("C".into(), "A".into())]
        );

        // Unrelated: version moves, no pairs; again: nothing moves.
        let (_, cm) = call(&app, json!([name, concept("D")])).await;
        assert_eq!(cm["version"], "4");
        assert!(pairs(&cm).is_empty());
        let (_, cm) = call(&app, json!([name, concept("D")])).await;
        assert_eq!(cm["version"], "4");

        // Resync from version 2: everything added after it.
        let (_, cm) = call(&app, json!([name, {"name": "version", "valueString": "2"}])).await;
        assert_eq!(cm["version"], "4");
        assert_eq!(
            pairs(&cm),
            [("B".into(), "A".into()), ("C".into(), "A".into())]
        );
    }

    /// The code system the table was started with came with the request.
    #[tokio::test]
    async fn closure_table_keeps_its_request_supplied_code_system() {
        let app = make_app(false).await;
        let name = json!({"name": "name", "valueString": "t2"});
        let (st, _) = call(
            &app,
            json!([name, {"name": "reset", "valueBoolean": true},
                   {"name": "tx-resource", "resource": hierarchy_cs()}]),
        )
        .await;
        assert_eq!(st, 200);
        call(&app, json!([name, concept("A")])).await;
        let (_, cm) = call(&app, json!([name, concept("C")])).await;
        assert_eq!(pairs(&cm), [("C".into(), "A".into())]);
    }

    #[tokio::test]
    async fn closure_missing_name_returns_400() {
        let app = make_app(true).await;
        let (st, _) = call(&app, json!([concept("A")])).await;
        assert_eq!(st, 400);
    }
}
