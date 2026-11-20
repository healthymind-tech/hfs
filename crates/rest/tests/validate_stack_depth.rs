//! Stack-depth guard for `$validate` of a large Bundle.
//!
//! Tokio worker threads get 2 MiB of stack. In a debug build `$validate` of
//! a Bundle needed ~1.86 MiB (line M P1, with the manager off), the
//! validator's own recursion — the FHIRPath evaluator above all — being
//! 1.3 MiB of it. The validator now recurses on its own large-stack thread.
//! Here the whole request (routing, extractors, handler, validator) is
//! polled on one thread with `HFS_TEST_STACK_KIB` (default 1024) KiB of stack
//! and a current-thread runtime; a deeper path aborts the test process.
//! `HFS_TEST_STACK_ENTRIES` sizes the Bundle (default 200).
//! `hfs-eval/eval/scripts/p5-validator/stackprobe.sh` bisects the minimum.

use std::path::PathBuf;
use std::sync::Arc;

use axum_test::TestServer;
use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use helios_rest::ServerConfig;
use serde_json::{Value, json};

fn bundle(entries: usize) -> Value {
    let items: Vec<Value> = (0..entries)
        .map(|i| {
            let resource = if i % 2 == 0 {
                json!({"resourceType": "Patient", "id": format!("p{i}"),
                    "text": {"status": "generated", "div": "<div xmlns=\"http://www.w3.org/1999/xhtml\"><p>x</p></div>"},
                    "name": [{"family": "F", "given": ["G"]}], "gender": "female",
                    "contained": [{"resourceType": "Organization", "id": "o", "name": "O"}],
                    "managingOrganization": {"reference": "#o"}})
            } else {
                json!({"resourceType": "Observation", "id": format!("o{i}"), "status": "final",
                    "code": {"coding": [{"system": "http://loinc.org", "code": "29463-7"}]},
                    "subject": {"reference": format!("urn:uuid:00000000-0000-0000-0000-{:012}", i - 1)},
                    "valueQuantity": {"value": 70.5, "unit": "kg", "system": "http://unitsofmeasure.org", "code": "kg"}})
            };
            json!({"fullUrl": format!("urn:uuid:00000000-0000-0000-0000-{i:012}"), "resource": resource,
                "request": {"method": "POST", "url": resource["resourceType"]}})
        })
        .collect();
    json!({"resourceType": "Bundle", "type": "transaction", "entry": items})
}

#[test]
fn validate_of_a_large_bundle_fits_well_under_a_worker_stack() {
    let kib: usize = std::env::var("HFS_TEST_STACK_KIB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024);
    let entries: usize = std::env::var("HFS_TEST_STACK_ENTRIES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let status = std::thread::Builder::new()
        .stack_size(kib * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let data_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .and_then(|p| p.parent())
                    .map(|p| p.join("data"))
                    .unwrap();
                let backend = SqliteBackend::with_config(
                    ":memory:",
                    SqliteBackendConfig {
                        data_dir: Some(data_dir),
                        ..Default::default()
                    },
                )
                .unwrap();
                backend.init_schema().unwrap();
                let state = helios_rest::AppState::new(Arc::new(backend), ServerConfig::for_testing());
                let app = helios_rest::routing::fhir_routes::create_routes(state);
                let server = TestServer::new(app).unwrap();
                let response = server.post("/Bundle/$validate").json(&bundle(entries)).await;
                let outcome: Value = response.json();
                assert_eq!(outcome["resourceType"], "OperationOutcome");
                response.status_code().as_u16()
            })
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(status, 200);
}
