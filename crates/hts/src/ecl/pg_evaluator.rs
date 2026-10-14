//! PostgreSQL-backed ECL evaluator.
//!
//! The PostgreSQL twin of [`super::evaluator`]: it consumes the same
//! [`EclExpr`] AST and answers every operator from the same tables
//! (`concepts`, `concept_closure`, `concept_hierarchy`, `refset_members`), so
//! both backends select exactly the same concept sets.
//!
//! | Operator | Query |
//! |----------|-------|
//! | `< id` / `<< id` | `concept_closure` range scan on `(system_id, ancestor_code)` |
//! | `> id` / `>> id` | `concept_closure` scan on `idx_closure_descendant` |
//! | `id` | one `concepts` row |
//! | `*` | every `concepts` row of the system |
//! | `^ id` | `refset_members` rows of reference set `id` |
//!
//! When the closure is not built for the focus concept (a system whose
//! closure build is still pending), the hierarchy operators fall back to a
//! recursive CTE over `concept_hierarchy`, as the SQLite evaluator does.
//!
//! `AND` / `OR` / `MINUS` are evaluated as set operations in Rust.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;

use tokio_postgres::Client;

use crate::error::HtsError;

use super::ResolvedConcept;
use super::parser::{ConceptOperator, EclExpr, FocusConcept};

type Map = HashMap<String, ResolvedConcept>;
type BoxFut<'a, T> = Pin<Box<dyn Future<Output = Result<T, HtsError>> + Send + 'a>>;

fn db_err(e: tokio_postgres::Error) -> HtsError {
    HtsError::StorageError(crate::backends::postgres::error_chain(&e))
}

/// Evaluate `expr` against `system_id` and return the matching concepts
/// ordered by code.
pub async fn evaluate(
    client: &Client,
    system_id: &str,
    expr: &EclExpr,
) -> Result<Vec<ResolvedConcept>, HtsError> {
    let map = eval_to_map(client, system_id, expr).await?;
    let mut out: Vec<ResolvedConcept> = map.into_values().collect();
    out.sort_by(|a, b| a.code.cmp(&b.code));
    Ok(out)
}

/// The subset of `candidates` that `expr` selects, answered per operator for
/// the candidates only (see the SQLite `evaluator::filter_codes`).
pub fn filter_codes<'a>(
    client: &'a Client,
    system_id: &'a str,
    expr: &'a EclExpr,
    candidates: &'a [String],
) -> BoxFut<'a, HashSet<String>> {
    Box::pin(async move {
        if candidates.is_empty() {
            return Ok(HashSet::new());
        }
        match expr {
            EclExpr::And(l, r) => {
                let left: Vec<String> = filter_codes(client, system_id, l, candidates)
                    .await?
                    .into_iter()
                    .collect();
                filter_codes(client, system_id, r, &left).await
            }
            EclExpr::Or(l, r) => {
                let mut out = filter_codes(client, system_id, l, candidates).await?;
                let rest: Vec<String> = candidates
                    .iter()
                    .filter(|c| !out.contains(*c))
                    .cloned()
                    .collect();
                out.extend(filter_codes(client, system_id, r, &rest).await?);
                Ok(out)
            }
            EclExpr::Minus(l, r) => {
                let left: Vec<String> = filter_codes(client, system_id, l, candidates)
                    .await?
                    .into_iter()
                    .collect();
                let drop = filter_codes(client, system_id, r, &left).await?;
                Ok(left.into_iter().filter(|c| !drop.contains(c)).collect())
            }
            EclExpr::Focus {
                op: Some(op),
                concept: FocusConcept::Id(id),
            } if closure_has(client, system_id, id).await? => {
                let (select, given, include_self) = match op {
                    ConceptOperator::DescendantOf => ("descendant_code", "ancestor_code", false),
                    ConceptOperator::DescendantOrSelf => ("descendant_code", "ancestor_code", true),
                    ConceptOperator::AncestorOf => ("ancestor_code", "descendant_code", false),
                    ConceptOperator::AncestorOrSelf => ("ancestor_code", "descendant_code", true),
                };
                let sql = format!(
                    "SELECT {select} FROM concept_closure
                     WHERE system_id = $1 AND {given} = $2
                       AND {select} = ANY($3)
                       AND ($4::bool OR {select} <> $2)"
                );
                let rows = client
                    .query(&sql, &[&system_id, id, &candidates, &include_self])
                    .await
                    .map_err(db_err)?;
                Ok(rows.into_iter().map(|r| r.get::<_, String>(0)).collect())
            }
            EclExpr::MemberOf(refsets) => match refsets.as_ref() {
                EclExpr::Focus {
                    op: None,
                    concept: FocusConcept::Id(id),
                } => {
                    require_members(client, system_id).await?;
                    let rows = client
                        .query(
                            "SELECT referenced_code FROM refset_members
                             WHERE system_id = $1 AND refset_id = $2
                               AND referenced_code = ANY($3)",
                            &[&system_id, id, &candidates],
                        )
                        .await
                        .map_err(db_err)?;
                    Ok(rows.into_iter().map(|r| r.get::<_, String>(0)).collect())
                }
                _ => intersect_full(client, system_id, expr, candidates).await,
            },
            _ => intersect_full(client, system_id, expr, candidates).await,
        }
    })
}

async fn intersect_full(
    client: &Client,
    system_id: &str,
    expr: &EclExpr,
    candidates: &[String],
) -> Result<HashSet<String>, HtsError> {
    let members = eval_to_map(client, system_id, expr).await?;
    Ok(candidates
        .iter()
        .filter(|c| members.contains_key(c.as_str()))
        .cloned()
        .collect())
}

fn eval_to_map<'a>(client: &'a Client, system_id: &'a str, expr: &'a EclExpr) -> BoxFut<'a, Map> {
    Box::pin(async move {
        match expr {
            EclExpr::Focus { op, concept } => {
                eval_focus(client, system_id, op.as_ref(), concept).await
            }
            EclExpr::And(left, right) => {
                let l = eval_to_map(client, system_id, left).await?;
                let r = eval_to_map(client, system_id, right).await?;
                Ok(l.into_iter()
                    .filter(|(code, _)| r.contains_key(code.as_str()))
                    .collect())
            }
            EclExpr::Or(left, right) => {
                let mut l = eval_to_map(client, system_id, left).await?;
                for (code, concept) in eval_to_map(client, system_id, right).await? {
                    l.entry(code).or_insert(concept);
                }
                Ok(l)
            }
            EclExpr::Minus(left, right) => {
                let l = eval_to_map(client, system_id, left).await?;
                let r: HashSet<String> = eval_to_map(client, system_id, right)
                    .await?
                    .into_keys()
                    .collect();
                Ok(l.into_iter()
                    .filter(|(code, _)| !r.contains(code.as_str()))
                    .collect())
            }
            EclExpr::MemberOf(refsets) => match refsets.as_ref() {
                EclExpr::Focus {
                    op: None,
                    concept: FocusConcept::Wildcard,
                } => fetch_members(client, system_id, &[], true).await,
                EclExpr::Focus {
                    op: None,
                    concept: FocusConcept::Id(id),
                } => fetch_members(client, system_id, std::slice::from_ref(id), false).await,
                inner => {
                    let ids: Vec<String> = eval_to_map(client, system_id, inner)
                        .await?
                        .into_keys()
                        .collect();
                    fetch_members(client, system_id, &ids, false).await
                }
            },
        }
    })
}

fn rows_to_map(rows: Vec<tokio_postgres::Row>) -> Map {
    rows.into_iter()
        .map(|r| {
            let code: String = r.get(0);
            let display: Option<String> = r.get(1);
            (code.clone(), ResolvedConcept { code, display })
        })
        .collect()
}

async fn fetch_members(
    client: &Client,
    system_id: &str,
    refsets: &[String],
    any: bool,
) -> Result<Map, HtsError> {
    require_members(client, system_id).await?;
    let rows = if any {
        client
            .query(
                "SELECT DISTINCT c.code, c.display FROM refset_members m
                 JOIN concepts c ON c.system_id = m.system_id AND c.code = m.referenced_code
                 WHERE m.system_id = $1",
                &[&system_id],
            )
            .await
    } else {
        client
            .query(
                "SELECT DISTINCT c.code, c.display FROM refset_members m
                 JOIN concepts c ON c.system_id = m.system_id AND c.code = m.referenced_code
                 WHERE m.system_id = $1 AND m.refset_id = ANY($2)",
                &[&system_id, &refsets],
            )
            .await
    }
    .map_err(db_err)?;
    Ok(rows_to_map(rows))
}

/// Fail unless the system has reference set members (see the SQLite twin).
async fn require_members(client: &Client, system_id: &str) -> Result<(), HtsError> {
    let imported: bool = client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM refset_members WHERE system_id = $1)",
            &[&system_id],
        )
        .await
        .map_err(db_err)?
        .get(0);
    if imported {
        return Ok(());
    }
    Err(HtsError::NotSupported(
        "Reference set membership (ECL `^`) is not available for this code system: no \
         reference set members were imported. Re-import the SNOMED CT RF2 release to load \
         the Simple reference sets."
            .into(),
    ))
}

/// `true` when `concept_closure` is built for `code` (it holds its self-link).
async fn closure_has(client: &Client, system_id: &str, code: &str) -> Result<bool, HtsError> {
    Ok(client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM concept_closure
                 WHERE system_id = $1 AND ancestor_code = $2 AND descendant_code = $2)",
            &[&system_id, &code],
        )
        .await
        .map_err(db_err)?
        .get(0))
}

async fn eval_focus(
    client: &Client,
    system_id: &str,
    op: Option<&ConceptOperator>,
    concept: &FocusConcept,
) -> Result<Map, HtsError> {
    match (op, concept) {
        (_, FocusConcept::Wildcard) => {
            let rows = client
                .query(
                    "SELECT code, display FROM concepts WHERE system_id = $1",
                    &[&system_id],
                )
                .await
                .map_err(db_err)?;
            Ok(rows_to_map(rows))
        }
        (None, FocusConcept::Id(id)) => fetch_exact(client, system_id, id).await,
        (Some(ConceptOperator::DescendantOf), FocusConcept::Id(id)) => {
            fetch_descendants(client, system_id, id, false).await
        }
        (Some(ConceptOperator::DescendantOrSelf), FocusConcept::Id(id)) => {
            fetch_descendants(client, system_id, id, true).await
        }
        (Some(ConceptOperator::AncestorOf), FocusConcept::Id(id)) => {
            fetch_ancestors(client, system_id, id, false).await
        }
        (Some(ConceptOperator::AncestorOrSelf), FocusConcept::Id(id)) => {
            fetch_ancestors(client, system_id, id, true).await
        }
    }
}

async fn fetch_exact(client: &Client, system_id: &str, code: &str) -> Result<Map, HtsError> {
    let rows = client
        .query(
            "SELECT code, display FROM concepts WHERE system_id = $1 AND code = $2",
            &[&system_id, &code],
        )
        .await
        .map_err(db_err)?;
    Ok(rows_to_map(rows))
}

/// Descendants of `root` (plus `root` itself when `include_self`).
pub async fn fetch_descendants(
    client: &Client,
    system_id: &str,
    root: &str,
    include_self: bool,
) -> Result<Map, HtsError> {
    if closure_has(client, system_id, root).await? {
        let rows = client
            .query(
                "SELECT c.code, c.display
                 FROM concept_closure cc
                 JOIN concepts c ON c.system_id = cc.system_id AND c.code = cc.descendant_code
                 WHERE cc.system_id = $1 AND cc.ancestor_code = $2
                   AND ($3::bool OR cc.descendant_code <> $2)",
                &[&system_id, &root, &include_self],
            )
            .await
            .map_err(db_err)?;
        return Ok(rows_to_map(rows));
    }
    let rows = client
        .query(
            "WITH RECURSIVE d(code) AS (
                 SELECT child_code FROM concept_hierarchy
                 WHERE system_id = $1 AND parent_code = $2
                 UNION
                 SELECT h.child_code FROM concept_hierarchy h
                 JOIN d ON h.parent_code = d.code
                 WHERE h.system_id = $1
             )
             SELECT c.code, c.display FROM concepts c
             JOIN d ON c.code = d.code
             WHERE c.system_id = $1",
            &[&system_id, &root],
        )
        .await
        .map_err(db_err)?;
    let mut map = rows_to_map(rows);
    if include_self {
        for (code, c) in fetch_exact(client, system_id, root).await? {
            map.entry(code).or_insert(c);
        }
    }
    Ok(map)
}

/// Ancestors of `start` (plus `start` itself when `include_self`).
async fn fetch_ancestors(
    client: &Client,
    system_id: &str,
    start: &str,
    include_self: bool,
) -> Result<Map, HtsError> {
    if closure_has(client, system_id, start).await? {
        let rows = client
            .query(
                "SELECT c.code, c.display
                 FROM concept_closure cc
                 JOIN concepts c ON c.system_id = cc.system_id AND c.code = cc.ancestor_code
                 WHERE cc.system_id = $1 AND cc.descendant_code = $2
                   AND ($3::bool OR cc.ancestor_code <> $2)",
                &[&system_id, &start, &include_self],
            )
            .await
            .map_err(db_err)?;
        return Ok(rows_to_map(rows));
    }
    let rows = client
        .query(
            "WITH RECURSIVE a(code) AS (
                 SELECT parent_code FROM concept_hierarchy
                 WHERE system_id = $1 AND child_code = $2
                 UNION
                 SELECT h.parent_code FROM concept_hierarchy h
                 JOIN a ON h.child_code = a.code
                 WHERE h.system_id = $1
             )
             SELECT c.code, c.display FROM concepts c
             JOIN a ON c.code = a.code
             WHERE c.system_id = $1",
            &[&system_id, &start],
        )
        .await
        .map_err(db_err)?;
    let mut map = rows_to_map(rows);
    if include_self {
        for (code, c) in fetch_exact(client, system_id, start).await? {
            map.entry(code).or_insert(c);
        }
    }
    Ok(map)
}
