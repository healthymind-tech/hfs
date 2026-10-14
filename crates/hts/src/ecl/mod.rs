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

/// PostgreSQL twin of [`parse_and_evaluate`].
#[cfg(feature = "postgres")]
pub async fn parse_and_evaluate_pg(
    client: &tokio_postgres::Client,
    system_id: &str,
    ecl: &str,
) -> Result<Vec<ResolvedConcept>, crate::error::HtsError> {
    let expr = parse_expr(ecl)?;
    pg_evaluator::evaluate(client, system_id, &expr).await
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
    let expr = parse_expr(ecl)?;
    pg_evaluator::filter_codes(client, system_id, &expr, candidates).await
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
    let expr = parse_expr(ecl)?;
    evaluator::evaluate(conn, system_id, &expr)
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
    let expr = parse_expr(ecl)?;
    evaluator::filter_codes(conn, system_id, &expr, candidates)
}
