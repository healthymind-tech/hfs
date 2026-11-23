//! SQLite answers for `$hm-explore` (see [`crate::explore`]); the same
//! shapes as the PostgreSQL backend.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;
use rusqlite::types::Value as Sql;
use serde_json::{Value, json};

use super::SqliteTerminologyBackend;
use crate::error::HtsError;
use crate::explore::{
    COUNT_CAP, ExploreMode, ExploreRequest, MAX_DEPTH, PROPS_CENSUS_MAX, like_escape,
};

fn err(e: rusqlite::Error) -> HtsError {
    HtsError::StorageError(e.to_string())
}

fn t(s: &str) -> Sql {
    Sql::Text(s.to_string())
}

fn opt(s: Option<&str>) -> Sql {
    s.map(t).unwrap_or(Sql::Null)
}

fn i(n: i64) -> Sql {
    Sql::Integer(n)
}

/// Runs `sql`, mapping each row with `f`.
fn query<T>(
    conn: &Connection,
    sql: &str,
    params: &[Sql],
    f: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>, HtsError> {
    let mut stmt = conn.prepare(sql).map_err(err)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), f)
        .map_err(err)?;
    rows.collect::<rusqlite::Result<Vec<T>>>().map_err(err)
}

fn count(conn: &Connection, inner: &str, params: &[Sql]) -> Result<i64, HtsError> {
    let sql = format!("SELECT count(*) FROM ({inner} LIMIT {}) t", COUNT_CAP + 1);
    Ok(query(conn, &sql, params, |r| r.get::<_, i64>(0))?
        .into_iter()
        .next()
        .unwrap_or(0))
}

/// `?n, ?n+1, …` for an `IN (…)` list of `len` values starting at `first`.
fn list(first: usize, len: usize) -> String {
    (0..len.max(1))
        .map(|k| format!("?{}", first + k))
        .collect::<Vec<_>>()
        .join(", ")
}

fn resolve(
    conn: &Connection,
    url: &str,
    version: Option<&str>,
) -> Result<Option<(String, Option<String>)>, HtsError> {
    let r = match version {
        Some(v) => query(
            conn,
            "SELECT id, version FROM code_systems WHERE url = ?1 AND version = ?2 LIMIT 1",
            &[t(url), t(v)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?,
        None => query(
            conn,
            &format!(
                "SELECT id, version FROM code_systems WHERE url = ?1 ORDER BY {} LIMIT 1",
                crate::backends::cs_precedence_order_by("code_systems")
            ),
            &[t(url)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?,
    };
    Ok(r.into_iter().next())
}

fn child_count(col: &str) -> String {
    format!(
        "(SELECT count(*) FROM (SELECT 1 FROM concept_hierarchy x \
         WHERE x.system_id = ?1 AND x.parent_code = {col} LIMIT 100000))"
    )
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

fn set(items: &mut [Value], key: &str, map: &HashMap<String, Value>) {
    for it in items.iter_mut() {
        if let Some(v) = it["code"].as_str().and_then(|c| map.get(c)) {
            it[key] = v.clone();
        }
    }
}

fn with_codes(base: Vec<Sql>, codes: &[String]) -> Vec<Sql> {
    let mut p = base;
    p.extend(codes.iter().map(|c| t(c)));
    p
}

fn decorate(
    conn: &Connection,
    sid: &str,
    req: &ExploreRequest,
    items: &mut [Value],
) -> Result<(), HtsError> {
    if items.is_empty() {
        return Ok(());
    }
    let codes = codes_of(items);
    if let Some((url, version)) = &req.alt
        && let Some((alt_id, _)) = resolve(conn, url, version.as_deref())?
    {
        let sql = format!(
            "SELECT c.code, COALESCE(NULLIF(c.display, ''), (SELECT d.value FROM concept_designations d \
               WHERE d.concept_id = c.id ORDER BY (lower(COALESCE(d.language, '')) LIKE 'zh%') DESC, d.id LIMIT 1)) \
             FROM concepts c WHERE c.system_id = ?1 AND c.code IN ({})",
            list(2, codes.len())
        );
        let map: HashMap<String, Value> =
            query(conn, &sql, &with_codes(vec![t(&alt_id)], &codes), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })?
            .into_iter()
            .filter_map(|(c, v)| v.map(|v| (c, json!(v))))
            .collect();
        set(items, "alt", &map);
    }
    if let Some(lang) = &req.lang {
        let sql = format!(
            "SELECT c.code, d.value FROM concepts c JOIN concept_designations d ON d.concept_id = c.id \
             WHERE c.system_id = ?1 AND lower(COALESCE(d.language, '')) LIKE ?2 ESCAPE '\\' \
               AND c.code IN ({}) ORDER BY d.id",
            list(3, codes.len())
        );
        let mut map = HashMap::new();
        for (c, v) in query(
            conn,
            &sql,
            &with_codes(vec![t(sid), t(&format!("{}%", like_escape(lang)))], &codes),
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )? {
            map.entry(c).or_insert_with(|| json!(v));
        }
        set(items, "designation", &map);
    }
    if req.descendants && items.len() <= 60 {
        let sql = format!(
            "SELECT ancestor_code, count(*) FROM concept_closure WHERE system_id = ?1 \
               AND descendant_code <> ancestor_code AND ancestor_code IN ({}) GROUP BY ancestor_code",
            list(2, codes.len())
        );
        let map: HashMap<String, Value> =
            query(conn, &sql, &with_codes(vec![t(sid)], &codes), |r| {
                Ok((r.get::<_, String>(0)?, json!(r.get::<_, i64>(1)?)))
            })?
            .into_iter()
            .collect();
        set(items, "descendants", &map);
    }
    Ok(())
}

fn member_cte(sid: &str, req: &ExploreRequest, with_text: bool) -> (String, Vec<Sql>) {
    let mut params = vec![t(sid)];
    let mut wh = vec!["c.system_id = ?1".to_string()];
    for (prop, value) in &req.filters {
        params.push(t(prop));
        let pk = params.len();
        params.push(t(value));
        let pv = params.len();
        wh.push(format!(
            "EXISTS (SELECT 1 FROM concept_properties f WHERE f.concept_id = c.id AND f.property = ?{pk} AND f.value = ?{pv})"
        ));
    }
    if with_text {
        for w in req.words() {
            params.push(t(&format!("%{}%", like_escape(&w))));
            let n = params.len();
            wh.push(format!(
                "(c.display LIKE ?{n} ESCAPE '\\' OR c.code LIKE ?{n} ESCAPE '\\')"
            ));
        }
    }
    (
        format!(
            "WITH m AS (SELECT c.id FROM concepts c WHERE {})",
            wh.join(" AND ")
        ),
        params,
    )
}

impl SqliteTerminologyBackend {
    pub(super) async fn explore_impl(&self, req: &ExploreRequest) -> Result<Value, HtsError> {
        let pool = self.pool().clone();
        let req = req.clone();
        tokio::task::spawn_blocking(move || -> Result<Value, HtsError> {
            let conn = pool
                .get()
                .map_err(|e| HtsError::StorageError(format!("Pool error: {e}")))?;
            let Some((sid, version)) = resolve(&conn, &req.system, req.version.as_deref())? else {
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
                ExploreMode::Stats => stats(&conn, &sid, &req)?,
                ExploreMode::Children => children(&conn, &sid, &req)?,
                ExploreMode::Buckets => buckets(&conn, &sid, &req)?,
                ExploreMode::Facets => facets(&conn, &sid, &req)?,
                ExploreMode::Members => members(&conn, &sid, &req)?,
                ExploreMode::Search => search(&conn, &sid, &req)?,
                ExploreMode::Node => node(&conn, &sid, &req)?,
            };
            out["system"] = json!(req.system);
            out["version"] = json!(version);
            out["mode"] = json!(req.mode.as_str());
            Ok(out)
        })
        .await
        .map_err(|e| HtsError::Internal(format!("Blocking task error: {e}")))?
    }
}

fn stats(conn: &Connection, sid: &str, req: &ExploreRequest) -> Result<Value, HtsError> {
    let rows = query(
        conn,
        "SELECT s.url, s.name, s.title, s.content, json_extract(s.resource_json, '$.hierarchyMeaning'), \
                json_extract(s.resource_json, '$.language'), json_extract(s.resource_json, '$.supplements'), \
                json_extract(s.resource_json, '$.description'), json_extract(s.resource_json, '$.id'), s.status, \
                EXISTS (SELECT 1 FROM concept_hierarchy h WHERE h.system_id = s.id), \
                EXISTS (SELECT 1 FROM concept_closure x WHERE x.system_id = s.id), \
                json_extract(s.resource_json, '$.property') \
         FROM code_systems s WHERE s.id = ?1",
        &[t(sid)],
        |r| {
            Ok(json!({
                "url": r.get::<_, String>(0)?,
                "name": r.get::<_, Option<String>>(1)?,
                "title": r.get::<_, Option<String>>(2)?,
                "content": r.get::<_, String>(3)?,
                "hierarchyMeaning": r.get::<_, Option<String>>(4)?,
                "language": r.get::<_, Option<String>>(5)?,
                "supplements": r.get::<_, Option<String>>(6)?,
                "description": r.get::<_, Option<String>>(7)?,
                "id": r.get::<_, Option<String>>(8)?,
                "status": r.get::<_, String>(9)?,
                "hierarchy": r.get::<_, bool>(10)?,
                "closure": r.get::<_, bool>(11)?,
                "propertyDefs": r
                    .get::<_, Option<String>>(12)?
                    .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                    .unwrap_or(Value::Null),
            }))
        },
    )?;
    let Some(mut out) = rows.into_iter().next() else {
        return Err(HtsError::NotFound(format!(
            "CodeSystem {sid} is not stored"
        )));
    };
    let concepts = query(
        conn,
        "SELECT count(*) FROM concepts WHERE system_id = ?1",
        &[t(sid)],
        |r| r.get::<_, i64>(0),
    )?
    .into_iter()
    .next()
    .unwrap_or(0);
    out["concepts"] = json!(concepts);
    if req.props && concepts <= PROPS_CENSUS_MAX {
        let p = query(
            conn,
            "SELECT p.property, count(*), count(DISTINCT p.value) FROM concepts c \
             JOIN concept_properties p ON p.concept_id = c.id \
             WHERE c.system_id = ?1 AND p.property NOT IN ('parent', 'child') \
             GROUP BY p.property ORDER BY count(*) DESC LIMIT 40",
            &[t(sid)],
            |r| {
                Ok(json!({
                    "code": r.get::<_, String>(0)?,
                    "concepts": r.get::<_, i64>(1)?,
                    "distinct": r.get::<_, i64>(2)?,
                }))
            },
        )?;
        out["properties"] = json!(p);
        let d = query(
            conn,
            "SELECT COALESCE(d.language, ''), count(*) FROM concepts c \
             JOIN concept_designations d ON d.concept_id = c.id \
             WHERE c.system_id = ?1 GROUP BY 1 ORDER BY 2 DESC LIMIT 12",
            &[t(sid)],
            |r| Ok(json!({"language": r.get::<_, String>(0)?, "count": r.get::<_, i64>(1)?})),
        )?;
        out["designationLanguages"] = json!(d);
    }
    Ok(out)
}

fn has_hierarchy(conn: &Connection, sid: &str) -> Result<bool, HtsError> {
    Ok(!query(
        conn,
        "SELECT 1 FROM concept_hierarchy WHERE system_id = ?1 LIMIT 1",
        &[t(sid)],
        |r| r.get::<_, i64>(0),
    )?
    .is_empty())
}

fn row3(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(item(r.get(0)?, r.get(1)?, r.get(2)?))
}

fn children(conn: &Connection, sid: &str, req: &ExploreRequest) -> Result<Value, HtsError> {
    let prefix = req
        .prefix
        .as_deref()
        .map(|p| format!("{}%", like_escape(p)));
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
                 WHERE h.system_id = ?1 AND h.parent_code = ?2 \
                   AND (?3 IS NULL OR h.child_code LIKE ?3 ESCAPE '\\') \
                 ORDER BY {order} LIMIT ?4 OFFSET ?5",
                child_count("h.child_code")
            );
            let params = [
                t(sid),
                t(parent),
                opt(prefix.as_deref()),
                i(req.count as i64),
                i(req.offset as i64),
            ];
            let items = query(conn, &sql, &params, row3)?;
            let total = count(
                conn,
                "SELECT 1 FROM concept_hierarchy h WHERE h.system_id = ?1 AND h.parent_code = ?2 \
                   AND (?3 IS NULL OR h.child_code LIKE ?3 ESCAPE '\\')",
                &params[..3],
            )?;
            (total, items, false)
        }
        None if has_hierarchy(conn, sid)? => {
            let sql = format!(
                "SELECT r.code, c.display, {} FROM (SELECT DISTINCT h.parent_code AS code \
                   FROM concept_hierarchy h WHERE h.system_id = ?1 AND NOT EXISTS \
                   (SELECT 1 FROM concept_hierarchy p WHERE p.system_id = ?1 AND p.child_code = h.parent_code)) r \
                 LEFT JOIN concepts c ON c.system_id = ?1 AND c.code = r.code \
                 ORDER BY r.code LIMIT 501",
                child_count("r.code")
            );
            let all = query(conn, &sql, &[t(sid)], row3)?;
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
            let params = [
                t(sid),
                opt(prefix.as_deref()),
                i(req.count as i64),
                i(req.offset as i64),
            ];
            let items = query(
                conn,
                &format!(
                    "SELECT code, display, 0 FROM concepts WHERE system_id = ?1 \
                       AND (?2 IS NULL OR code LIKE ?2 ESCAPE '\\') ORDER BY {order} LIMIT ?3 OFFSET ?4"
                ),
                &params,
                row3,
            )?;
            let total = count(
                conn,
                "SELECT 1 FROM concepts WHERE system_id = ?1 AND (?2 IS NULL OR code LIKE ?2 ESCAPE '\\')",
                &params[..2],
            )?;
            (total, items, false)
        }
    };
    decorate(conn, sid, req, &mut items)?;
    Ok(json!({
        "parent": req.code,
        "roots": roots,
        "total": total,
        "capped": total > COUNT_CAP,
        "offset": req.offset,
        "items": items,
    }))
}

fn buckets(conn: &Connection, sid: &str, req: &ExploreRequest) -> Result<Value, HtsError> {
    let parent = req.code.clone().unwrap_or_default();
    let inner = "SELECT CASE WHEN instr(h.child_code, ?3) > 0 \
                 THEN substr(h.child_code, 1, instr(h.child_code, ?3) - 1) ELSE h.child_code END AS k, \
                 h.child_code FROM concept_hierarchy h WHERE h.system_id = ?1 AND h.parent_code = ?2";
    let params = [t(sid), t(&parent), t(&req.sep)];
    let mut p = params.to_vec();
    p.push(i(req.count as i64));
    p.push(i(req.offset as i64));
    let groups = query(
        conn,
        &format!(
            "SELECT t.k, count(*) AS n, min(t.child_code) FROM ({inner}) t \
             GROUP BY t.k ORDER BY n DESC, t.k LIMIT ?4 OFFSET ?5"
        ),
        &p,
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        },
    )?;
    let (total, members) = query(
        conn,
        &format!("SELECT count(DISTINCT t.k), count(*) FROM ({inner}) t"),
        &params,
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    )?
    .into_iter()
    .next()
    .unwrap_or((0, 0));
    let samples: Vec<String> = groups.iter().map(|g| g.2.clone()).collect();
    let names: HashMap<String, Option<String>> = query(
        conn,
        &format!(
            "SELECT code, display FROM concepts WHERE system_id = ?1 AND code IN ({})",
            list(2, samples.len())
        ),
        &with_codes(vec![t(sid)], &samples),
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?
    .into_iter()
    .collect();
    Ok(json!({
        "parent": parent,
        "sep": req.sep,
        "total": total,
        "members": members,
        "offset": req.offset,
        "items": groups.into_iter().map(|(k, n, s)| {
            let d = names.get(&s).cloned().flatten().unwrap_or_default();
            json!({"key": k, "count": n, "sample": {"code": s, "display": d}})
        }).collect::<Vec<_>>(),
    }))
}

fn facets(conn: &Connection, sid: &str, req: &ExploreRequest) -> Result<Value, HtsError> {
    let (cte, mut params) = member_cte(sid, req, false);
    let matched = count(conn, &format!("{cte} SELECT 1 FROM m"), &params)?;
    params.push(t(req.property.as_deref().unwrap_or_default()));
    let pp = params.len();
    let mut value_filter = String::new();
    if let Some(q) = &req.q {
        params.push(t(&format!("%{}%", like_escape(q))));
        let n = params.len();
        value_filter =
            format!(" WHERE (p.value LIKE ?{n} ESCAPE '\\' OR d.display LIKE ?{n} ESCAPE '\\')");
    }
    params.push(i(req.count as i64 + 1));
    let pl = params.len();
    let sql = format!(
        "{cte} SELECT p.value, count(*) AS n, max(d.display) FROM m \
         JOIN concept_properties p ON p.concept_id = m.id AND p.property = ?{pp} \
         LEFT JOIN concepts d ON d.system_id = ?1 AND d.code = p.value{value_filter} \
         GROUP BY p.value ORDER BY n DESC, p.value LIMIT ?{pl}"
    );
    let values = query(conn, &sql, &params, |r| {
        Ok(json!({
            "value": r.get::<_, String>(0)?,
            "count": r.get::<_, i64>(1)?,
            "display": r.get::<_, Option<String>>(2)?,
        }))
    })?;
    let truncated = values.len() > req.count as usize;
    Ok(json!({
        "property": req.property,
        "filters": req.filters.iter().map(|(p, v)| json!({"property": p, "value": v})).collect::<Vec<_>>(),
        "matched": matched,
        "capped": matched > COUNT_CAP,
        "truncated": truncated,
        "values": values.into_iter().take(req.count as usize).collect::<Vec<_>>(),
    }))
}

fn members(conn: &Connection, sid: &str, req: &ExploreRequest) -> Result<Value, HtsError> {
    let (cte, mut params) = member_cte(sid, req, true);
    let total = count(conn, &format!("{cte} SELECT 1 FROM m"), &params)?;
    params.push(i(req.count as i64));
    let pl = params.len();
    params.push(i(req.offset as i64));
    let po = params.len();
    let mut items = query(
        conn,
        &format!(
            "{cte} SELECT c.code, c.display FROM m JOIN concepts c ON c.id = m.id \
             ORDER BY c.code LIMIT ?{pl} OFFSET ?{po}"
        ),
        &params,
        |r| {
            Ok(json!({
                "code": r.get::<_, String>(0)?,
                "display": r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            }))
        },
    )?;
    if !req.show.is_empty() && !items.is_empty() {
        let codes = codes_of(&items);
        let sql = format!(
            "SELECT c.code, p.property, p.value, d.display FROM concepts c \
             JOIN concept_properties p ON p.concept_id = c.id \
             LEFT JOIN concepts d ON d.system_id = ?1 AND d.code = p.value \
             WHERE c.system_id = ?1 AND c.code IN ({}) AND p.property IN ({})",
            list(2, codes.len()),
            list(2 + codes.len(), req.show.len())
        );
        let mut p = with_codes(vec![t(sid)], &codes);
        p.extend(req.show.iter().map(|s| t(s)));
        let mut map: HashMap<String, serde_json::Map<String, Value>> = HashMap::new();
        for (code, prop, value, display) in query(conn, &sql, &p, |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })? {
            map.entry(code)
                .or_default()
                .insert(prop, json!({"value": value, "display": display}));
        }
        for it in items.iter_mut() {
            if let Some(m) = it["code"].as_str().and_then(|c| map.remove(c)) {
                it["props"] = Value::Object(m);
            }
        }
    }
    decorate(conn, sid, req, &mut items)?;
    Ok(json!({
        "filters": req.filters.iter().map(|(p, v)| json!({"property": p, "value": v})).collect::<Vec<_>>(),
        "total": total,
        "capped": total > COUNT_CAP,
        "offset": req.offset,
        "items": items,
    }))
}

fn search(conn: &Connection, sid: &str, req: &ExploreRequest) -> Result<Value, HtsError> {
    let q = req.q.clone().unwrap_or_default();
    let n = req.count as usize;
    let mut seen = HashSet::new();
    let mut items: Vec<Value> = Vec::new();
    let mut push = |items: &mut Vec<Value>, code: String, display: Option<String>, how: &str| {
        if items.len() < n && seen.insert(code.clone()) {
            items.push(json!({"code": code, "display": display.unwrap_or_default(), "match": how}));
        }
    };
    let pair = |r: &rusqlite::Row<'_>| -> rusqlite::Result<(String, Option<String>)> {
        Ok((r.get(0)?, r.get(1)?))
    };
    for (c, d) in query(
        conn,
        "SELECT code, display FROM concepts WHERE system_id = ?1 AND code = ?2",
        &[t(sid), t(&q)],
        pair,
    )? {
        push(&mut items, c, d, "code");
    }
    if !q.contains(char::is_whitespace) {
        for (c, d) in query(
            conn,
            "SELECT code, display FROM concepts WHERE system_id = ?1 AND code LIKE ?2 ESCAPE '\\' \
             ORDER BY length(code), code LIMIT 10",
            &[t(sid), t(&format!("{}%", like_escape(&q)))],
            pair,
        )? {
            push(&mut items, c, d, "code");
        }
    }
    let words = req.words();
    let clause = |first: usize, col: &str| -> String {
        (0..words.len())
            .map(|k| format!("{col} LIKE ?{} ESCAPE '\\'", first + k))
            .collect::<Vec<_>>()
            .join(" AND ")
    };
    let with_words = |base: Vec<Sql>| -> Vec<Sql> {
        let mut p = base;
        p.extend(words.iter().map(|w| t(&format!("%{}%", like_escape(w)))));
        p
    };
    if !words.is_empty() && items.len() < n {
        let mut p = with_words(vec![t(sid)]);
        p.push(t(&format!("{}%", like_escape(&words[0]))));
        let pf = p.len();
        let sql = format!(
            "SELECT code, display FROM concepts WHERE system_id = ?1 AND {} \
             ORDER BY (lower(display) LIKE ?{pf} ESCAPE '\\') DESC, length(display), code LIMIT {n}",
            clause(2, "display")
        );
        for (c, d) in query(conn, &sql, &p, pair)? {
            push(&mut items, c, d, "display");
        }
    }
    if !words.is_empty()
        && items.len() < n
        && let Some((url, version)) = &req.alt
        && let Some((alt_id, _)) = resolve(conn, url, version.as_deref())?
    {
        let sql = format!(
            "SELECT a.code, b.display FROM concepts a JOIN concepts b ON b.system_id = ?1 AND b.code = a.code \
             WHERE a.system_id = ?2 AND {} ORDER BY length(a.display), a.code LIMIT {n}",
            clause(3, "a.display")
        );
        for (c, d) in query(conn, &sql, &with_words(vec![t(sid), t(&alt_id)]), pair)? {
            push(&mut items, c, d, "alt");
        }
    }
    if !words.is_empty()
        && items.len() < n
        && count(
            conn,
            "SELECT 1 FROM concepts WHERE system_id = ?1",
            &[t(sid)],
        )? <= 30_000
    {
        let sql = format!(
            "SELECT c.code, c.display FROM concepts c WHERE c.system_id = ?1 AND EXISTS \
             (SELECT 1 FROM concept_designations d WHERE d.concept_id = c.id AND {}) ORDER BY c.code LIMIT {n}",
            clause(2, "d.value")
        );
        for (c, d) in query(conn, &sql, &with_words(vec![t(sid)]), pair)? {
            push(&mut items, c, d, "designation");
        }
    }
    for it in items.iter_mut() {
        let code = it["code"].as_str().unwrap_or_default().to_string();
        let n = count(
            conn,
            "SELECT 1 FROM concept_hierarchy WHERE system_id = ?1 AND parent_code = ?2",
            &[t(sid), t(&code)],
        )?;
        it["children"] = json!(n);
    }
    decorate(conn, sid, req, &mut items)?;
    Ok(json!({"q": q, "items": items}))
}

fn node(conn: &Connection, sid: &str, req: &ExploreRequest) -> Result<Value, HtsError> {
    let code = req.code.clone().unwrap_or_default();
    let Some(display) = query(
        conn,
        "SELECT display FROM concepts WHERE system_id = ?1 AND code = ?2",
        &[t(sid), t(&code)],
        |r| r.get::<_, Option<String>>(0),
    )?
    .into_iter()
    .next() else {
        return Err(HtsError::NotFound(format!(
            "{code} is not a concept of {}",
            req.system
        )));
    };
    let mut parents = query(
        conn,
        &format!(
            "SELECT h.parent_code, c.display, {} FROM concept_hierarchy h \
             LEFT JOIN concepts c ON c.system_id = h.system_id AND c.code = h.parent_code \
             WHERE h.system_id = ?1 AND h.child_code = ?2 ORDER BY h.parent_code LIMIT 50",
            child_count("h.parent_code")
        ),
        &[t(sid), t(&code)],
        row3,
    )?;
    let mut child_req = req.clone();
    child_req.mode = ExploreMode::Children;
    child_req.offset = 0;
    child_req.prefix = None;
    let kids = children(conn, sid, &child_req)?;
    let mut path = vec![code.clone()];
    while path.len() < MAX_DEPTH {
        let cur = path.last().cloned().unwrap_or_default();
        let next = query(
            conn,
            "SELECT min(parent_code) FROM concept_hierarchy WHERE system_id = ?1 AND child_code = ?2",
            &[t(sid), t(&cur)],
            |r| r.get::<_, Option<String>>(0),
        )?
        .into_iter()
        .next()
        .flatten();
        match next {
            Some(p) if !path.contains(&p) => path.push(p),
            _ => break,
        }
    }
    path.reverse();
    let names: HashMap<String, Option<String>> = query(
        conn,
        &format!(
            "SELECT code, display FROM concepts WHERE system_id = ?1 AND code IN ({})",
            list(2, path.len())
        ),
        &with_codes(vec![t(sid)], &path),
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?
    .into_iter()
    .collect();
    let mut path: Vec<Value> = path
        .into_iter()
        .map(|c| {
            let d = names.get(&c).cloned().flatten().unwrap_or_default();
            json!({"code": c, "display": d})
        })
        .collect();
    decorate(conn, sid, req, &mut parents)?;
    decorate(conn, sid, req, &mut path)?;
    let mut me = vec![json!({"code": code, "display": display.clone().unwrap_or_default()})];
    decorate(conn, sid, req, &mut me)?;
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
