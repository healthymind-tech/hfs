//! SNOMED CT Expression Constraint Language (ECL) support.
//!
//! This module provides parsing and SQLite-backed evaluation of ECL expressions
//! as defined by SNOMED International:
//! <https://confluence.ihtsdotools.org/display/DOCECL>
//!
//! ## Usage
//!
//! ```rust,ignore
//! use helios_hts::ecl;
//!
//! let results = ecl::parse_and_evaluate(&conn, &system_id, "<< 404684003")?;
//! for concept in results {
//!     println!("{}: {}", concept.code, concept.display.unwrap_or_default());
//! }
//! ```
//!
//! ## Integration with `$expand`
//!
//! ECL is evaluated during `ValueSet/$expand` when a ValueSet compose include
//! contains a filter of the form:
//!
//! ```json
//! {
//!   "filter": [{
//!     "property": "constraint",
//!     "op": "=",
//!     "value": "<< 404684003 |Clinical finding|"
//!   }]
//! }
//! ```
//!
//! The simpler `concept is-a` filter is also supported:
//!
//! ```json
//! {
//!   "filter": [{
//!     "property": "concept",
//!     "op": "is-a",
//!     "value": "404684003"
//!   }]
//! }
//! ```

// Parser is dialect-independent (pure syntax → AST) and serves every backend.
// Each backend has its own evaluator over the same AST and the same tables:
// `evaluator` (rusqlite) and `pg_evaluator` (tokio-postgres).
#[cfg(feature = "sqlite")]
pub mod evaluator;
pub mod parser;
#[cfg(feature = "postgres")]
pub mod pg_evaluator;

pub use parser::{ConceptOperator, EclExpr, FocusConcept};

/// A resolved concept from the database.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedConcept {
    pub code: String,
    pub display: Option<String>,
}

/// Parse `ecl`, mapping a syntax error to `InvalidRequest`.
pub fn parse_expr(ecl: &str) -> Result<EclExpr, crate::error::HtsError> {
    parser::parse(ecl)
        .map_err(|e| crate::error::HtsError::InvalidRequest(format!("Invalid ECL expression: {e}")))
}

/// How a constraint on a SNOMED CT edition with a stored concept model is
/// answered: by the SQL evaluators when it is in their subset, else by the
/// in-memory model ([`crate::snomed::ecl`]). SNOMED CT constraints are
/// parsed by the full ECL grammar; a syntax error or an unknown concept makes
/// the value set invalid.
enum SnomedPlan {
    Sql(EclExpr),
    Model(crate::snomed::ecl::Ecl),
}

fn snomed_parse(ecl: &str) -> Result<crate::snomed::ecl::Ecl, crate::error::HtsError> {
    crate::snomed::ecl::parse(ecl).map_err(|e| {
        crate::error::HtsError::VsInvalid(format!(
            "The SNOMED CT expression constraint '{ecl}' is not valid: {e}"
        ))
    })
}

fn unknown_concept(ecl: &str, code: &str) -> crate::error::HtsError {
    crate::error::HtsError::VsInvalid(format!(
        "The SNOMED CT expression constraint '{ecl}' refers to the concept {code}, which is not in the code system"
    ))
}

fn model_results(
    model: &crate::snomed::SnomedModel,
    ecl: &str,
    expr: &crate::snomed::ecl::Ecl,
) -> Result<Vec<ResolvedConcept>, crate::error::HtsError> {
    let set = crate::snomed::ecl::Evaluator::new(model)
        .eval(expr)
        .map_err(|u| unknown_concept(ecl, &u.0))?;
    let mut out: Vec<ResolvedConcept> = set
        .iter()
        .map(|&i| ResolvedConcept {
            code: model.code(i),
            display: model.display_of(i).map(str::to_string),
        })
        .collect();
    out.sort_by(|a, b| a.code.cmp(&b.code));
    Ok(out)
}

/// PostgreSQL twin of [`parse_and_evaluate`].
#[cfg(feature = "postgres")]
pub async fn parse_and_evaluate_pg(
    client: &tokio_postgres::Client,
    system_id: &str,
    ecl: &str,
) -> Result<Vec<ResolvedConcept>, crate::error::HtsError> {
    match snomed_plan_pg(client, system_id, ecl).await? {
        Some(SnomedPlan::Model(e)) => {
            let model = crate::snomed::store::load_pg(client, system_id)
                .await?
                .ok_or_else(|| {
                    crate::error::HtsError::Internal("SNOMED CT model vanished".into())
                })?;
            model_results(&model, ecl, &e)
        }
        Some(SnomedPlan::Sql(expr)) => pg_evaluator::evaluate(client, system_id, &expr).await,
        None => {
            let expr = parse_expr(ecl)?;
            pg_evaluator::evaluate(client, system_id, &expr).await
        }
    }
}

#[cfg(feature = "postgres")]
async fn snomed_plan_pg(
    client: &tokio_postgres::Client,
    system_id: &str,
    ecl: &str,
) -> Result<Option<SnomedPlan>, crate::error::HtsError> {
    if crate::snomed::store::stamp_pg(client, system_id)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    let e = snomed_parse(ecl)?;
    let missing = crate::snomed::store::missing_pg(client, system_id, &e.concept_ids()).await?;
    if let Some(c) = missing.first() {
        return Err(unknown_concept(ecl, c));
    }
    Ok(Some(match e.to_simple() {
        Some(s) => SnomedPlan::Sql(s),
        None => SnomedPlan::Model(e),
    }))
}

/// PostgreSQL twin of [`filter_candidates`].
#[cfg(feature = "postgres")]
pub async fn filter_candidates_pg(
    client: &tokio_postgres::Client,
    system_id: &str,
    ecl: &str,
    candidates: &[String],
) -> Result<std::collections::HashSet<String>, crate::error::HtsError> {
    if candidates.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    match snomed_plan_pg(client, system_id, ecl).await? {
        Some(SnomedPlan::Model(e)) => {
            let model = crate::snomed::store::load_pg(client, system_id)
                .await?
                .ok_or_else(|| {
                    crate::error::HtsError::Internal("SNOMED CT model vanished".into())
                })?;
            let all = model_results(&model, ecl, &e)?;
            let want: std::collections::HashSet<&String> = candidates.iter().collect();
            Ok(all
                .into_iter()
                .map(|r| r.code)
                .filter(|c| want.contains(c))
                .collect())
        }
        Some(SnomedPlan::Sql(expr)) => {
            pg_evaluator::filter_codes(client, system_id, &expr, candidates).await
        }
        None => {
            let expr = parse_expr(ecl)?;
            pg_evaluator::filter_codes(client, system_id, &expr, candidates).await
        }
    }
}

#[cfg(feature = "sqlite")]
use std::collections::HashSet;

#[cfg(feature = "sqlite")]
use rusqlite::Connection;

#[cfg(feature = "sqlite")]
use crate::error::HtsError;

/// Parse an ECL string and evaluate it against the given code system.
///
/// Returns a sorted `Vec<ResolvedConcept>` (sorted by code).
///
/// # Errors
///
/// - Returns `HtsError::InvalidRequest` if the ECL expression cannot be parsed.
/// - Returns `HtsError::StorageError` if a database query fails.
#[cfg(feature = "sqlite")]
pub fn parse_and_evaluate(
    conn: &Connection,
    system_id: &str,
    ecl: &str,
) -> Result<Vec<ResolvedConcept>, HtsError> {
    match snomed_plan_sqlite(conn, system_id, ecl)? {
        Some(SnomedPlan::Model(e)) => {
            let model = crate::snomed::store::load_sqlite(conn, system_id)?
                .ok_or_else(|| HtsError::Internal("SNOMED CT model vanished".into()))?;
            model_results(&model, ecl, &e)
        }
        Some(SnomedPlan::Sql(expr)) => evaluator::evaluate(conn, system_id, &expr),
        None => {
            let expr = parse_expr(ecl)?;
            evaluator::evaluate(conn, system_id, &expr)
        }
    }
}

#[cfg(feature = "sqlite")]
fn snomed_plan_sqlite(
    conn: &Connection,
    system_id: &str,
    ecl: &str,
) -> Result<Option<SnomedPlan>, HtsError> {
    if crate::snomed::store::stamp_sqlite(conn, system_id)?.is_none() {
        return Ok(None);
    }
    let e = snomed_parse(ecl)?;
    let missing = crate::snomed::store::missing_sqlite(conn, system_id, &e.concept_ids())?;
    if let Some(c) = missing.first() {
        return Err(unknown_concept(ecl, c));
    }
    Ok(Some(match e.to_simple() {
        Some(s) => SnomedPlan::Sql(s),
        None => SnomedPlan::Model(e),
    }))
}

/// Return the subset of `candidates` that are members of the ECL constraint.
///
/// Tests the candidates against the expression (see
/// [`evaluator::filter_codes`]) rather than evaluating it in full, so a text
/// filter's few hundred hits are checked against `<< 404684003` without
/// materialising its 120 000 members. An empty `candidates` slice short-circuits to an empty set without
/// evaluating the expression.
///
/// # Errors
///
/// - Returns `HtsError::InvalidRequest` if the ECL expression cannot be parsed.
/// - Returns `HtsError::StorageError` if a database query fails.
#[cfg(feature = "sqlite")]
pub fn filter_candidates(
    conn: &Connection,
    system_id: &str,
    ecl: &str,
    candidates: &[String],
) -> Result<HashSet<String>, HtsError> {
    if candidates.is_empty() {
        return Ok(HashSet::new());
    }
    match snomed_plan_sqlite(conn, system_id, ecl)? {
        Some(SnomedPlan::Model(e)) => {
            let model = crate::snomed::store::load_sqlite(conn, system_id)?
                .ok_or_else(|| HtsError::Internal("SNOMED CT model vanished".into()))?;
            let all = model_results(&model, ecl, &e)?;
            let want: HashSet<&String> = candidates.iter().collect();
            Ok(all
                .into_iter()
                .map(|r| r.code)
                .filter(|c| want.contains(c))
                .collect())
        }
        Some(SnomedPlan::Sql(expr)) => evaluator::filter_codes(conn, system_id, &expr, candidates),
        None => {
            let expr = parse_expr(ecl)?;
            evaluator::filter_codes(conn, system_id, &expr, candidates)
        }
    }
}
