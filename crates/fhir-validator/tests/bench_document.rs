//! Validation cost breakdown of one resource (line V2 latency work; perf is
//! not available on the shared test server). Ignored by default:
//!
//! ```text
//! BENCH_FILE=Bundle-bun-document-example.json BENCH_SDS=<dir of StructureDefinition-*.json> \
//!   cargo test --release -p helios-fhir-validator --features fhirpath --test bench_document -- --ignored --nocapture
//! ```
//!
//! Prints the median over `BENCH_N` (default 30) runs of: the structural walk,
//! the FHIRPath constraints (one `evaluate_all`), the embedded-terminology
//! binding pass, and the full `Validator::validate`; then the cost per
//! invariant id.

#![cfg(all(feature = "fhirpath", feature = "R4"))]

use helios_fhir::FhirVersion;
use helios_fhir_validator::converter::convert;
use helios_fhir_validator::fhirpath_effects::FhirPathConstraintEvaluator;
use helios_fhir_validator::packs::core_registry;
use helios_fhir_validator::{
    CompositeResolver, ConstraintEvaluator, Deferred, DeferredConstraint, EffectHandlers,
    SchemaRegistry, SchemaResolver, ValidationOptions, Validator,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

#[tokio::test]
#[ignore]
async fn document_validation_cost() {
    let Some(file) = std::env::var_os("BENCH_FILE") else {
        return;
    };
    let n: usize = std::env::var("BENCH_N").ok().and_then(|s| s.parse().ok()).unwrap_or(30);
    let resource: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    if let Ok(filter) = std::env::var("BENCH_TRACE") {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .with_writer(std::io::stderr)
            .try_init();
    }
    let core = core_registry(FhirVersion::R4);
    let resolver: Arc<dyn SchemaResolver> = match std::env::var_os("BENCH_SDS") {
        Some(dir) => {
            let mut overlay = SchemaRegistry::new();
            let mut count = 0;
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.starts_with("StructureDefinition-") || !name.ends_with(".json") {
                    continue;
                }
                let sd: Value = serde_json::from_str(&std::fs::read_to_string(entry.path()).unwrap()).unwrap();
                // As the server: a stored definition never shadows a core one.
                if sd.get("url").and_then(Value::as_str).is_some_and(|u| core.resolve(u).is_some()) {
                    continue;
                }
                if let Ok(c) = convert(&sd) {
                    if overlay.insert(c.schema) {
                        count += 1;
                    }
                }
            }
            eprintln!("overlay: {count} profiles");
            Arc::new(CompositeResolver::new(vec![Arc::new(overlay), core]))
        }
        None => core,
    };
    let validator = Validator::new(Arc::clone(&resolver));
    let evaluator = FhirPathConstraintEvaluator::new();
    let tx = helios_fhir_validator::core_terminology(FhirVersion::R4);
    let suppress = vec!["dom-6".to_string()];
    let handlers = EffectHandlers {
        constraints: Some(&evaluator),
        terminology: Some(tx.as_ref()),
        suppress_constraints: &suppress,
        check_extensible: true,
        narrative_links: true,
        resource_rules: true,
        best_practice: true,
        ..Default::default()
    };
    let opts = ValidationOptions::default();
    // warm-up (AST cache, packs)
    let issues = validator.validate(&resource, FhirVersion::R4, &opts, &handlers).await;
    eprintln!("issues: {} ({} errors)", issues.len(), issues.iter().filter(|e| e.severity == helios_fhir_validator::Severity::Error).count());

    let (mut walk, mut cons, mut full) = (Vec::new(), Vec::new(), Vec::new());
    let mut deferred_len = 0;
    let mut constraint_len = 0;
    for _ in 0..n {
        let t = Instant::now();
        let outcome = validator.validate_sync(&resource, &opts);
        walk.push(t.elapsed());
        deferred_len = outcome.deferred.len();
        let mut seen = std::collections::HashSet::new();
        let constraints: Vec<DeferredConstraint<'_>> = outcome
            .deferred
            .iter()
            .filter_map(|d| match d {
                Deferred::Constraint { path, id, expression, own_resource, severity, .. }
                    if id != "dom-6"
                        && matches!(severity.as_deref(), None | Some("error") | Some("warning"))
                        && seen.insert((path.clone(), id.clone())) =>
                {
                    Some(DeferredConstraint { path, id, expression, own_resource: *own_resource })
                }
                _ => None,
            })
            .collect();
        constraint_len = constraints.len();
        let t = Instant::now();
        evaluator.evaluate_all(&resource, FhirVersion::R4, &constraints);
        cons.push(t.elapsed());
        let t = Instant::now();
        let _ = validator.validate(&resource, FhirVersion::R4, &opts, &handlers).await;
        full.push(t.elapsed());
    }
    eprintln!(
        "median over {n}: walk {:?} (deferred {deferred_len}), constraints {:?} ({constraint_len} after dedup), full validate {:?}",
        median(walk),
        median(cons),
        median(full)
    );

    // Walk of each entry on its own (no Bundle context, no reference checks).
    if let Some(entries) = resource.get("entry").and_then(Value::as_array) {
        let sum: Duration = entries
            .iter()
            .filter_map(|e| e.get("resource"))
            .map(|r| {
                median((0..n).map(|_| {
                    let t = Instant::now();
                    let _ = validator.validate_sync(r, &opts);
                    t.elapsed()
                }).collect())
            })
            .sum();
        eprintln!("walk of the {} entries alone, summed: {sum:?}", entries.len());
        let core_only = Validator::new(core_registry(FhirVersion::R4));
        let no_profiles = ValidationOptions { use_meta_profiles: false, ..Default::default() };
        let t = median((0..n).map(|_| {
            let t = Instant::now();
            let _ = core_only.validate_sync(&resource, &no_profiles);
            t.elapsed()
        }).collect());
        eprintln!("walk of the Bundle, core only, meta.profile off: {t:?}");
    }

    // Per invariant id.
    let outcome = validator.validate_sync(&resource, &opts);
    let mut seen = std::collections::HashSet::new();
    let constraints: Vec<DeferredConstraint<'_>> = outcome
        .deferred
        .iter()
        .filter_map(|d| match d {
            Deferred::Constraint { path, id, expression, own_resource, .. }
                if id != "dom-6" && seen.insert((path.clone(), id.clone())) =>
            {
                Some(DeferredConstraint { path, id, expression, own_resource: *own_resource })
            }
            _ => None,
        })
        .collect();
    let mut groups: std::collections::BTreeMap<&str, Vec<DeferredConstraint<'_>>> = Default::default();
    for c in &constraints {
        groups.entry(c.id).or_default().push(*c);
    }
    let empty: Vec<DeferredConstraint<'_>> = vec![constraints[0]];
    let base = median((0..5).map(|_| {
        let t = Instant::now();
        evaluator.evaluate_all(&resource, FhirVersion::R4, &empty);
        t.elapsed()
    }).collect());
    let mut rows: Vec<(&str, usize, Duration)> = groups
        .iter()
        .map(|(id, group)| {
            let d = median((0..5).map(|_| {
                let t = Instant::now();
                evaluator.evaluate_all(&resource, FhirVersion::R4, group);
                t.elapsed()
            }).collect());
            (*id, group.len(), d.saturating_sub(base))
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.2));
    eprintln!("per-id cost above one typed parse + context ({base:?}):");
    for (id, count, d) in rows.iter().take(20) {
        eprintln!("{id:>14} x{count:<5} {d:?}");
    }
}
