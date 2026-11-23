//! PostgreSQL answers for `$hm-explore` (see [`crate::explore`]).
//!
//! Every request runs in one read-only transaction with a statement timeout,
//! so an unlucky text search cannot hold a pooled connection for long.

use serde_json::{Value, json};
use tokio_postgres::Transaction;
use tokio_postgres::types::ToSql;

use super::{PostgresTerminologyBackend, error_chain};
use crate::error::HtsError;
use crate::explore::{
    COUNT_CAP, ExploreMode, ExploreRequest, MAX_DEPTH, PROPS_CENSUS_MAX, like_escape,
};

type P = Box<dyn ToSql + Sync + Send>;

fn serr(e: tokio_postgres::Error) -> HtsError {
    if e.code() == Some(&tokio_postgres::error::SqlState::QUERY_CANCELED) {
        return HtsError::InvalidRequest(
            "$hm-explore: the query took too long; narrow it down".into(),
        );
    }
    HtsError::StorageError(error_chain(&e))
}

async fn rows(
    tx: &Transaction<'_>,
    sql: &str,
    params: &[P],
) -> Result<Vec<tokio_postgres::Row>, HtsError> {
    let refs: Vec<&(dyn ToSql + Sync)> = params
        .iter()
        .map(|p| p.as_ref() as &(dyn ToSql + Sync))
        .collect();
    tx.query(sql, &refs).await.map_err(serr)
}

async fn count(tx: &Transaction<'_>, inner: &str, params: &[P]) -> Result<i64, HtsError> {
    let sql = format!("SELECT count(*) FROM ({inner} LIMIT {}) t", COUNT_CAP + 1);
    let r = rows(tx, &sql, params).await?;
    Ok(r.first().map(|r| r.get::<_, i64>(0)).unwrap_or(0))
}

/// `(system id, version)` of `url` (`version`, else the precedence winner).
async fn resolve(
    tx: &Transaction<'_>,
    url: &str,
    version: Option<&str>,
) -> Result<Option<(String, Option<String>)>, HtsError> {
    let r = match version {
        Some(v) => {
            rows(
                tx,
                "SELECT id, version FROM code_systems WHERE url = $1 AND version = $2 LIMIT 1",
                &[Box::new(url.to_string()), Box::new(v.to_string())],
            )
            .await?
        }
        None => {
            let sql = format!(
                "SELECT id, version FROM code_systems WHERE url = $1 ORDER BY {} LIMIT 1",
                crate::backends::cs_precedence_order_by("code_systems")
            );
            rows(tx, &sql, &[Box::new(url.to_string())]).await?
        }
    };
    Ok(r.first().map(|r| (r.get(0), r.get(1))))
}

const CHILD_COUNT: &str = "(SELECT count(*) FROM (SELECT 1 FROM concept_hierarchy x \
     WHERE x.system_id = $1 AND x.parent_code = {col} LIMIT 100000) cc)";

fn child_count(col: &str) -> String {
    CHILD_COUNT.replace("{col}", col)
}

fn item(code: String, display: Option<String>, children: i64) -> Value {
    json!({"code": code, "display": display.unwrap_or_default(), "children": children})
}

fn codes_of(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .filter_map(|i| i["code"].as_str().map(str::to_string))
        .collect()
}

/// Adds `alt`, `designation` and `descendants` to `items` as the request asks.
async fn decorate(
    tx: &Transaction<'_>,
    sid: &str,
    req: &ExploreRequest,
    items: &mut [Value],
) -> Result<(), HtsError> {
    if items.is_empty() {
        return Ok(());
    }
    let codes = codes_of(items);
    let set = |items: &mut [Value], key: &str, map: std::collections::HashMap<String, Value>| {
        for it in items.iter_mut() {
            if let Some(v) = it["code"].as_str().and_then(|c| map.get(c)) {
                it[key] = v.clone();
            }
        }
    };
    if let Some((url, version)) = &req.alt
        && let Some((alt_id, _)) = resolve(tx, url, version.as_deref()).await?
    {
        let r = rows(
            tx,
            "SELECT c.code, COALESCE(NULLIF(c.display, ''), (SELECT d.value FROM concept_designations d \
               WHERE d.concept_id = c.id ORDER BY (lower(COALESCE(d.language, '')) LIKE 'zh%') DESC, d.id LIMIT 1)) \
             FROM concepts c WHERE c.system_id = $1 AND c.code = ANY($2)",
            &[Box::new(alt_id), Box::new(codes.clone())],
        )
        .await?;
        let map = r
            .into_iter()
            .filter_map(|r| {
                let v: Option<String> = r.get(1);
                v.map(|v| (r.get::<_, String>(0), json!(v)))
            })
            .collect();
        set(items, "alt", map);
    }
    if let Some(lang) = &req.lang {
        let r = rows(
            tx,
            "SELECT c.code, d.value FROM concepts c JOIN concept_designations d ON d.concept_id = c.id \
             WHERE c.system_id = $1 AND c.code = ANY($2) AND lower(COALESCE(d.language, '')) LIKE $3 \
             ORDER BY d.id",
            &[
                Box::new(sid.to_string()),
                Box::new(codes.clone()),
                Box::new(format!("{}%", like_escape(lang))),
            ],
        )
        .await?;
        let mut map = std::collections::HashMap::new();
        for r in r {
            map.entry(r.get::<_, String>(0))
                .or_insert_with(|| json!(r.get::<_, String>(1)));
        }
        set(items, "designation", map);
    }
    if req.descendants && items.len() <= 60 {
        let r = rows(
            tx,
            "SELECT ancestor_code, count(*) FROM concept_closure \
             WHERE system_id = $1 AND ancestor_code = ANY($2) AND descendant_code <> ancestor_code \
             GROUP BY ancestor_code",
            &[Box::new(sid.to_string()), Box::new(codes)],
        )
        .await?;
        let map = r
            .into_iter()
            .map(|r| (r.get::<_, String>(0), json!(r.get::<_, i64>(1))))
            .collect();
        set(items, "descendants", map);
    }
    Ok(())
}

/// The concept-set CTE of the filters (and of the text, for `members`).
fn member_cte(sid: &str, req: &ExploreRequest, with_text: bool) -> (String, Vec<P>) {
    let mut params: Vec<P> = vec![Box::new(sid.to_string())];
    let mut sql = String::from("WITH m AS (SELECT c.id FROM concepts c");
    let mut wh = vec!["c.system_id = $1".to_string()];
    // The last filter (the latest drill-down, usually the narrowest) drives
    // the join through `idx_props_property_value`; the others are probes.
    let last = req.filters.len().saturating_sub(1);
    for (i, (prop, value)) in req.filters.iter().enumerate() {
        params.push(Box::new(prop.clone()));
        let pk = params.len();
        params.push(Box::new(value.clone()));
        let pv = params.len();
        if i == last {
            sql.push_str(&format!(
                " JOIN concept_properties f0 ON f0.concept_id = c.id AND f0.property = ${pk} AND f0.value = ${pv}"
            ));
        } else {
            wh.push(format!(
                "EXISTS (SELECT 1 FROM concept_properties f WHERE f.concept_id = c.id AND f.property = ${pk} AND f.value = ${pv})"
            ));
        }
    }
    if with_text {
        for w in req.words() {
            params.push(Box::new(format!("%{}%", like_escape(&w))));
            let n = params.len();
            wh.push(format!(
                "(c.display ILIKE ${n} ESCAPE '\\' OR c.code ILIKE ${n} ESCAPE '\\')"
            ));
        }
    }
    sql.push_str(" WHERE ");
    sql.push_str(&wh.join(" AND "));
    sql.push(')');
    (sql, params)
}

impl PostgresTerminologyBackend {
    pub(super) async fn explore_impl(&self, req: &ExploreRequest) -> Result<Value, HtsError> {
        let mut client = self
            .pool
            .get()
            .await
            .map_err(|e| HtsError::StorageError(format!("Pool error: {}", error_chain(&e))))?;
        let tx = client
            .build_transaction()
            .read_only(true)
            .start()
            .await
            .map_err(serr)?;
        let timeout = if req.mode == ExploreMode::Search {
            "4s"
        } else {
            "15s"
        };
        tx.batch_execute(&format!("SET LOCAL statement_timeout = '{timeout}'"))
            .await
            .map_err(serr)?;
        let Some((sid, version)) = resolve(&tx, &req.system, req.version.as_deref()).await? else {
            return Err(HtsError::NotFound(format!(
                "CodeSystem {}{} is not stored",
                req.system,
                req.version
                    .as_deref()
                    .map(|v| format!("|{v}"))
                    .unwrap_or_default()
            )));
        };
        let mut out = match req.mode {
            ExploreMode::Stats => stats(&tx, &sid, req).await?,
            ExploreMode::Children => children(&tx, &sid, req).await?,
            ExploreMode::Buckets => buckets(&tx, &sid, req).await?,
            ExploreMode::Facets => facets(&tx, &sid, req).await?,
            ExploreMode::Members => members(&tx, &sid, req).await?,
            ExploreMode::Search => search(&tx, &sid, req).await?,
            ExploreMode::Node => node(&tx, &sid, req).await?,
        };
        let _ = tx.commit().await;
        out["system"] = json!(req.system);
        out["version"] = json!(version);
        out["mode"] = json!(req.mode.as_str());
        Ok(out)
    }
}

async fn stats(tx: &Transaction<'_>, sid: &str, req: &ExploreRequest) -> Result<Value, HtsError> {
    let r = rows(
        tx,
        "SELECT s.url, s.name, s.title, s.content, s.resource_json->>'hierarchyMeaning', \
                s.resource_json->>'language', s.supplements, s.resource_json->>'description', s.fhir_id, s.status, \
                EXISTS (SELECT 1 FROM concept_hierarchy h WHERE h.system_id = s.id), \
                EXISTS (SELECT 1 FROM concept_closure x WHERE x.system_id = s.id), \
                s.resource_json->'property' \
         FROM code_systems s WHERE s.id = $1",
        &[Box::new(sid.to_string())],
    )
    .await?;
    let Some(r) = r.first() else {
        return Err(HtsError::NotFound(format!(
            "CodeSystem {sid} is not stored"
        )));
    };
    // Exact (an index-only count; the answer is cached when slow).
    let concepts = rows(
        tx,
        "SELECT count(*) FROM concepts WHERE system_id = $1",
        &[Box::new(sid.to_string())],
    )
    .await?
    .first()
    .map(|r| r.get::<_, i64>(0))
    .unwrap_or(0);
    let property_defs: Option<Value> = r.get(12);
    let mut out = json!({
        "id": r.get::<_, Option<String>>(8),
        "url": r.get::<_, String>(0),
        "name": r.get::<_, Option<String>>(1),
        "title": r.get::<_, Option<String>>(2),
        "content": r.get::<_, String>(3),
        "hierarchyMeaning": r.get::<_, Option<String>>(4),
        "language": r.get::<_, Option<String>>(5),
        "supplements": r.get::<_, Option<String>>(6),
        "description": r.get::<_, Option<String>>(7),
        "status": r.get::<_, String>(9),
        "hierarchy": r.get::<_, bool>(10),
        "closure": r.get::<_, bool>(11),
        "concepts": concepts,
        "propertyDefs": property_defs.unwrap_or(Value::Null),
    });
    if req.props && concepts <= PROPS_CENSUS_MAX {
        let p = rows(
            tx,
            "SELECT p.property, count(*), count(DISTINCT p.value) FROM concepts c \
             JOIN concept_properties p ON p.concept_id = c.id \
             WHERE c.system_id = $1 AND p.property NOT IN ('parent', 'child') \
             GROUP BY p.property ORDER BY count(*) DESC LIMIT 40",
            &[Box::new(sid.to_string())],
        )
        .await?;
        out["properties"] = json!(
            p.into_iter()
                .map(|r| json!({
                    "code": r.get::<_, String>(0),
                    "concepts": r.get::<_, i64>(1),
                    "distinct": r.get::<_, i64>(2),
                }))
                .collect::<Vec<_>>()
        );
        let d = rows(
            tx,
            "SELECT COALESCE(d.language, ''), count(*) FROM concepts c \
             JOIN concept_designations d ON d.concept_id = c.id \
             WHERE c.system_id = $1 GROUP BY 1 ORDER BY 2 DESC LIMIT 12",
            &[Box::new(sid.to_string())],
        )
        .await?;
        out["designationLanguages"] = json!(
            d.into_iter()
                .map(|r| json!({"language": r.get::<_, String>(0), "count": r.get::<_, i64>(1)}))
                .collect::<Vec<_>>()
        );
    }
    Ok(out)
}

async fn has_hierarchy(tx: &Transaction<'_>, sid: &str) -> Result<bool, HtsError> {
    Ok(!rows(
        tx,
        "SELECT 1 FROM concept_hierarchy WHERE system_id = $1 LIMIT 1",
        &[Box::new(sid.to_string())],
    )
    .await?
    .is_empty())
}

async fn children(
    tx: &Transaction<'_>,
    sid: &str,
    req: &ExploreRequest,
) -> Result<Value, HtsError> {
    let prefix = req
        .prefix
        .as_deref()
        .map(|p| format!("{}%", like_escape(p)));
    let limit = req.count as i64;
    let offset = req.offset as i64;
    let (total, mut items, roots) = match req.code.as_deref() {
        Some(parent) => {
            let order = if req.order_display {
                "lower(COALESCE(c.display, h.child_code)), h.child_code"
            } else if req.order_desc {
                "h.child_code DESC"
            } else {
                "h.child_code"
            };
            let sql = format!(
                "SELECT h.child_code, c.display, {} FROM concept_hierarchy h \
                 LEFT JOIN concepts c ON c.system_id = h.system_id AND c.code = h.child_code \
                 WHERE h.system_id = $1 AND h.parent_code = $2 \
                   AND ($3::text IS NULL OR h.child_code LIKE $3 ESCAPE '\\') \
                 ORDER BY {order} LIMIT $4 OFFSET $5",
                child_count("h.child_code")
            );
            let r = rows(
                tx,
                &sql,
                &[
                    Box::new(sid.to_string()),
                    Box::new(parent.to_string()),
                    Box::new(prefix.clone()),
                    Box::new(limit),
                    Box::new(offset),
                ],
            )
            .await?;
            let total = count(
                tx,
                "SELECT 1 FROM concept_hierarchy h WHERE h.system_id = $1 AND h.parent_code = $2 \
                   AND ($3::text IS NULL OR h.child_code LIKE $3 ESCAPE '\\')",
                &[
                    Box::new(sid.to_string()),
                    Box::new(parent.to_string()),
                    Box::new(prefix),
                ],
            )
            .await?;
            let items: Vec<Value> = r
                .into_iter()
                .map(|r| item(r.get(0), r.get(1), r.get(2)))
                .collect();
            (total, items, false)
        }
        None if has_hierarchy(tx, sid).await? => {
            let sql = format!(
                "SELECT r.code, c.display, {} FROM (SELECT DISTINCT h.parent_code AS code \
                   FROM concept_hierarchy h WHERE h.system_id = $1 AND NOT EXISTS \
                   (SELECT 1 FROM concept_hierarchy p WHERE p.system_id = $1 AND p.child_code = h.parent_code)) r \
                 LEFT JOIN concepts c ON c.system_id = $1 AND c.code = r.code \
                 ORDER BY r.code LIMIT 501",
                child_count("r.code")
            );
            let r = rows(tx, &sql, &[Box::new(sid.to_string())]).await?;
            let all: Vec<Value> = r
                .into_iter()
                .map(|r| item(r.get(0), r.get(1), r.get(2)))
                .collect();
            let total = all.len() as i64;
            let items = all
                .into_iter()
                .skip(req.offset as usize)
                .take(req.count as usize)
                .collect();
            (total, items, true)
        }
        None => {
            let order = if req.order_display {
                "lower(COALESCE(display, code)), code"
            } else if req.order_desc {
                "code DESC"
            } else {
                "code"
            };
            let sql = format!(
                "SELECT code, display, 0::bigint FROM concepts WHERE system_id = $1 \
                   AND ($2::text IS NULL OR code LIKE $2 ESCAPE '\\') \
                 ORDER BY {order} LIMIT $3 OFFSET $4"
            );
            let r = rows(
                tx,
                &sql,
                &[
                    Box::new(sid.to_string()),
                    Box::new(prefix.clone()),
                    Box::new(limit),
                    Box::new(offset),
                ],
            )
            .await?;
            let total = count(
                tx,
                "SELECT 1 FROM concepts WHERE system_id = $1 AND ($2::text IS NULL OR code LIKE $2 ESCAPE '\\')",
                &[Box::new(sid.to_string()), Box::new(prefix)],
            )
            .await?;
            let items = r
                .into_iter()
                .map(|r| item(r.get(0), r.get(1), r.get(2)))
                .collect();
            (total, items, false)
        }
    };
    decorate(tx, sid, req, &mut items).await?;
    Ok(json!({
        "parent": req.code,
        "roots": roots,
        "total": total,
        "capped": total > COUNT_CAP,
        "offset": req.offset,
        "items": items,
    }))
}

async fn buckets(tx: &Transaction<'_>, sid: &str, req: &ExploreRequest) -> Result<Value, HtsError> {
    let parent = req.code.clone().unwrap_or_default();
    let inner = "SELECT CASE WHEN strpos(h.child_code, $3) > 0 THEN split_part(h.child_code, $3, 1) \
                 ELSE h.child_code END AS k, h.child_code \
                 FROM concept_hierarchy h WHERE h.system_id = $1 AND h.parent_code = $2";
    let sql = format!(
        "SELECT t.k, count(*) AS n, min(t.child_code), \
           (SELECT c.display FROM concepts c WHERE c.system_id = $1 AND c.code = min(t.child_code)) \
         FROM ({inner}) t GROUP BY t.k ORDER BY n DESC, t.k LIMIT $4 OFFSET $5"
    );
    let r = rows(
        tx,
        &sql,
        &[
            Box::new(sid.to_string()),
            Box::new(parent.clone()),
            Box::new(req.sep.clone()),
            Box::new(req.count as i64),
            Box::new(req.offset as i64),
        ],
    )
    .await?;
    let total = rows(
        tx,
        &format!("SELECT count(DISTINCT t.k), count(*) FROM ({inner}) t"),
        &[
            Box::new(sid.to_string()),
            Box::new(parent.clone()),
            Box::new(req.sep.clone()),
        ],
    )
    .await?;
    let (groups, members) = total
        .first()
        .map(|r| (r.get::<_, i64>(0), r.get::<_, i64>(1)))
        .unwrap_or((0, 0));
    Ok(json!({
        "parent": parent,
        "sep": req.sep,
        "total": groups,
        "members": members,
        "offset": req.offset,
        "items": r.into_iter().map(|r| json!({
            "key": r.get::<_, String>(0),
            "count": r.get::<_, i64>(1),
            "sample": {"code": r.get::<_, String>(2), "display": r.get::<_, Option<String>>(3).unwrap_or_default()},
        })).collect::<Vec<_>>(),
    }))
}

async fn facets(tx: &Transaction<'_>, sid: &str, req: &ExploreRequest) -> Result<Value, HtsError> {
    let (cte, mut params) = member_cte(sid, req, false);
    params.push(Box::new(req.property.clone().unwrap_or_default()));
    let pp = params.len();
    let mut value_filter = String::new();
    if let Some(q) = &req.q {
        params.push(Box::new(format!("%{}%", like_escape(q))));
        let n = params.len();
        value_filter =
            format!(" WHERE (p.value ILIKE ${n} ESCAPE '\\' OR d.display ILIKE ${n} ESCAPE '\\')");
    }
    params.push(Box::new(req.count as i64 + 1));
    let pl = params.len();
    // Without a value search, count first and name only the top values.
    let sql = if req.q.is_some() {
        format!(
            "{cte} SELECT p.value, count(*) AS n, max(d.display) FROM m \
             JOIN concept_properties p ON p.concept_id = m.id AND p.property = ${pp} \
             LEFT JOIN concepts d ON d.system_id = $1 AND d.code = p.value{value_filter} \
             GROUP BY p.value ORDER BY n DESC, p.value LIMIT ${pl}"
        )
    } else {
        format!(
            "{cte}, agg AS (SELECT p.value, count(*) AS n FROM m \
               JOIN concept_properties p ON p.concept_id = m.id AND p.property = ${pp} \
               GROUP BY p.value ORDER BY n DESC, p.value LIMIT ${pl}) \
             SELECT agg.value, agg.n, d.display FROM agg \
             LEFT JOIN concepts d ON d.system_id = $1 AND d.code = agg.value \
             ORDER BY agg.n DESC, agg.value"
        )
    };
    let r = rows(tx, &sql, &params).await?;
    let truncated = r.len() > req.count as usize;
    let (cte2, params2) = member_cte(sid, req, false);
    let matched = count(tx, &format!("{cte2} SELECT 1 FROM m"), &params2).await?;
    Ok(json!({
        "property": req.property,
        "filters": req.filters.iter().map(|(p, v)| json!({"property": p, "value": v})).collect::<Vec<_>>(),
        "matched": matched,
        "capped": matched > COUNT_CAP,
        "truncated": truncated,
        "values": r.into_iter().take(req.count as usize).map(|r| json!({
            "value": r.get::<_, String>(0),
            "count": r.get::<_, i64>(1),
            "display": r.get::<_, Option<String>>(2),
        })).collect::<Vec<_>>(),
    }))
}

async fn members(tx: &Transaction<'_>, sid: &str, req: &ExploreRequest) -> Result<Value, HtsError> {
    let (cte, mut params) = member_cte(sid, req, true);
    let total = count(tx, &format!("{cte} SELECT 1 FROM m"), &params).await?;
    params.push(Box::new(req.count as i64));
    let pl = params.len();
    params.push(Box::new(req.offset as i64));
    let po = params.len();
    let sql = format!(
        "{cte} SELECT c.code, c.display FROM m JOIN concepts c ON c.id = m.id \
         ORDER BY c.code LIMIT ${pl} OFFSET ${po}"
    );
    let r = rows(tx, &sql, &params).await?;
    let mut items: Vec<Value> = r
        .into_iter()
        .map(|r| {
            json!({"code": r.get::<_, String>(0), "display": r.get::<_, Option<String>>(1).unwrap_or_default()})
        })
        .collect();
    if !req.show.is_empty() && !items.is_empty() {
        let codes = codes_of(&items);
        let pr = rows(
            tx,
            "SELECT c.code, p.property, p.value, d.display FROM concepts c \
             JOIN concept_properties p ON p.concept_id = c.id \
             LEFT JOIN concepts d ON d.system_id = $1 AND d.code = p.value \
             WHERE c.system_id = $1 AND c.code = ANY($2) AND p.property = ANY($3)",
            &[
                Box::new(sid.to_string()),
                Box::new(codes),
                Box::new(req.show.clone()),
            ],
        )
        .await?;
        let mut map: std::collections::HashMap<String, serde_json::Map<String, Value>> =
            Default::default();
        for r in pr {
            let value: String = r.get(2);
            let display: Option<String> = r.get(3);
            map.entry(r.get(0))
                .or_default()
                .insert(r.get(1), json!({"value": value, "display": display}));
        }
        for it in items.iter_mut() {
            if let Some(m) = it["code"].as_str().and_then(|c| map.remove(c)) {
                it["props"] = Value::Object(m);
            }
        }
    }
    decorate(tx, sid, req, &mut items).await?;
    Ok(json!({
        "filters": req.filters.iter().map(|(p, v)| json!({"property": p, "value": v})).collect::<Vec<_>>(),
        "total": total,
        "capped": total > COUNT_CAP,
        "offset": req.offset,
        "items": items,
    }))
}

async fn search(tx: &Transaction<'_>, sid: &str, req: &ExploreRequest) -> Result<Value, HtsError> {
    let q = req.q.clone().unwrap_or_default();
    let n = req.count as usize;
    let mut seen = std::collections::HashSet::new();
    let mut items: Vec<Value> = Vec::new();
    let mut push = |items: &mut Vec<Value>, code: String, display: Option<String>, how: &str| {
        if items.len() < n && seen.insert(code.clone()) {
            items.push(json!({"code": code, "display": display.unwrap_or_default(), "match": how}));
        }
    };
    let sidp = || -> P { Box::new(sid.to_string()) };
    for r in rows(
        tx,
        "SELECT code, display FROM concepts WHERE system_id = $1 AND code = $2",
        &[sidp(), Box::new(q.clone())],
    )
    .await?
    {
        push(&mut items, r.get(0), r.get(1), "code");
    }
    if !q.contains(char::is_whitespace) {
        for r in rows(
            tx,
            "SELECT code, display FROM concepts WHERE system_id = $1 AND code LIKE $2 ESCAPE '\\' \
             ORDER BY length(code), code LIMIT 10",
            &[sidp(), Box::new(format!("{}%", like_escape(&q)))],
        )
        .await?
        {
            push(&mut items, r.get(0), r.get(1), "code");
        }
    }
    let words = req.words();
    let text_clause = |first: usize, col: &str| -> String {
        (0..words.len())
            .map(|i| format!("{col} ILIKE ${} ESCAPE '\\'", first + i))
            .collect::<Vec<_>>()
            .join(" AND ")
    };
    let word_params = |base: Vec<P>| -> Vec<P> {
        let mut p = base;
        for w in &words {
            p.push(Box::new(format!("%{}%", like_escape(w))));
        }
        p
    };
    if !words.is_empty() && items.len() < n {
        let first = format!("{}%", like_escape(&words[0]));
        let mut params = word_params(vec![sidp()]);
        params.push(Box::new(first));
        let pf = params.len();
        let sql = format!(
            "SELECT code, display FROM concepts WHERE system_id = $1 AND {} \
             ORDER BY (lower(display) LIKE ${pf} ESCAPE '\\') DESC, length(display), code LIMIT {}",
            text_clause(2, "display"),
            n
        );
        for r in rows(tx, &sql, &params).await? {
            push(&mut items, r.get(0), r.get(1), "display");
        }
    }
    // Labels of a companion system (the Chinese edition of the same codes).
    if !words.is_empty()
        && items.len() < n
        && let Some((url, version)) = &req.alt
        && let Some((alt_id, _)) = resolve(tx, url, version.as_deref()).await?
    {
        let params = word_params(vec![sidp(), Box::new(alt_id)]);
        let sql = format!(
            "SELECT a.code, b.display FROM concepts a JOIN concepts b ON b.system_id = $1 AND b.code = a.code \
             WHERE a.system_id = $2 AND {} ORDER BY length(a.display), a.code LIMIT {}",
            text_clause(3, "a.display"),
            n
        );
        for r in rows(tx, &sql, &params).await? {
            push(&mut items, r.get(0), r.get(1), "alt");
        }
    }
    // Designations, for systems small enough to scan.
    if !words.is_empty() && items.len() < n {
        let small =
            count(tx, "SELECT 1 FROM concepts WHERE system_id = $1", &[sidp()]).await? <= 30_000;
        if small {
            let params = word_params(vec![sidp()]);
            let sql = format!(
                "SELECT c.code, c.display FROM concepts c WHERE c.system_id = $1 AND EXISTS \
                 (SELECT 1 FROM concept_designations d WHERE d.concept_id = c.id AND {}) \
                 ORDER BY c.code LIMIT {}",
                text_clause(2, "d.value"),
                n
            );
            for r in rows(tx, &sql, &params).await? {
                push(&mut items, r.get(0), r.get(1), "designation");
            }
        }
    }
    if !items.is_empty() {
        let codes = codes_of(&items);
        let r = rows(
            tx,
            &format!(
                "SELECT c.code, {} FROM unnest($2::text[]) AS c(code)",
                child_count("c.code")
            ),
            &[sidp(), Box::new(codes)],
        )
        .await?;
        let counts: std::collections::HashMap<String, i64> =
            r.into_iter().map(|r| (r.get(0), r.get(1))).collect();
        for it in items.iter_mut() {
            if let Some(c) = it["code"].as_str().and_then(|c| counts.get(c)) {
                it["children"] = json!(c);
            }
        }
    }
    decorate(tx, sid, req, &mut items).await?;
    Ok(json!({"q": q, "items": items}))
}

async fn node(tx: &Transaction<'_>, sid: &str, req: &ExploreRequest) -> Result<Value, HtsError> {
    let code = req.code.clone().unwrap_or_default();
    let sidp = || -> P { Box::new(sid.to_string()) };
    let me = rows(
        tx,
        "SELECT display FROM concepts WHERE system_id = $1 AND code = $2",
        &[sidp(), Box::new(code.clone())],
    )
    .await?;
    let Some(me) = me.first() else {
        return Err(HtsError::NotFound(format!(
            "{code} is not a concept of {}",
            req.system
        )));
    };
    let display: Option<String> = me.get(0);
    let parents_sql = format!(
        "SELECT h.parent_code, c.display, {} FROM concept_hierarchy h \
         LEFT JOIN concepts c ON c.system_id = h.system_id AND c.code = h.parent_code \
         WHERE h.system_id = $1 AND h.child_code = $2 ORDER BY h.parent_code LIMIT 50",
        child_count("h.parent_code")
    );
    let mut parents: Vec<Value> = rows(tx, &parents_sql, &[sidp(), Box::new(code.clone())])
        .await?
        .into_iter()
        .map(|r| item(r.get(0), r.get(1), r.get(2)))
        .collect();
    let mut child_req = req.clone();
    child_req.mode = ExploreMode::Children;
    child_req.offset = 0;
    child_req.prefix = None;
    let kids = children(tx, sid, &child_req).await?;
    // Path to a root along the first parent.
    let mut path = vec![code.clone()];
    while path.len() < MAX_DEPTH {
        let cur = path.last().cloned().unwrap_or_default();
        let r = rows(
            tx,
            "SELECT min(parent_code) FROM concept_hierarchy WHERE system_id = $1 AND child_code = $2",
            &[sidp(), Box::new(cur)],
        )
        .await?;
        match r.first().and_then(|r| r.get::<_, Option<String>>(0)) {
            Some(p) if !path.contains(&p) => path.push(p),
            _ => break,
        }
    }
    path.reverse();
    let names: std::collections::HashMap<String, Option<String>> = rows(
        tx,
        "SELECT code, display FROM concepts WHERE system_id = $1 AND code = ANY($2)",
        &[sidp(), Box::new(path.clone())],
    )
    .await?
    .into_iter()
    .map(|r| (r.get(0), r.get(1)))
    .collect();
    let mut path: Vec<Value> = path
        .into_iter()
        .map(|c| {
            let d = names.get(&c).cloned().flatten();
            json!({"code": c, "display": d.unwrap_or_default()})
        })
        .collect();
    decorate(tx, sid, req, &mut parents).await?;
    decorate(tx, sid, req, &mut path).await?;
    let mut me = vec![json!({"code": code, "display": display.clone().unwrap_or_default()})];
    decorate(tx, sid, req, &mut me).await?;
    let mut out = json!({
        "code": code,
        "display": display.unwrap_or_default(),
        "parents": parents,
        "children": {"total": kids["total"], "items": kids["items"]},
        "path": path,
    });
    for k in ["alt", "designation"] {
        if let Some(v) = me[0].get(k) {
            out[k] = v.clone();
        }
    }
    Ok(out)
}
