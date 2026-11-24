//! Version-grouped resource search (`_hm-current`), shared by the SQLite and
//! PostgreSQL backends.
//!
//! A canonical URL is often stored in several versions (US Core ValueSets ×4,
//! THO v3 CodeSystems ×5): a plain `GET /ValueSet` lists each version as its
//! own entry. With `_hm-current=true` the search returns **one entry per
//! canonical URL** — the version that answers a version-less request, chosen
//! by the same precedence order as every resolver
//! ([`super::cs_precedence_order_by`] / [`super::vs_precedence_order_by`] /
//! [`super::cm_precedence_order_by`]) — and pages and counts canonical groups.
//! A group matches when *any* of its versions matches the filters. Each entry
//! carries the number of stored versions (extension [`VERSION_COUNT_URL`]) and
//! its `date`. `_hm-current=older` returns the other versions of the matched
//! groups, in precedence order (the "+N older" rows of the console).
//!
//! Grouping, ranking and paging happen in SQL (a `ROW_NUMBER()` window per
//! URL); only the requested page leaves the database.

use crate::string_search::FhirStringSearchMode;
use crate::types::{ResourceSearchQuery, VersionGroup};

/// Extension on each grouped entry: how many versions the canonical has.
pub const VERSION_COUNT_URL: &str =
    "https://healthymind.tech/fhir/StructureDefinition/hm-version-count";

/// SQL dialect of the executing backend.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Dialect {
    Sqlite,
    Postgres,
}

/// The two statements of one grouped search, with their parameters in order:
/// `url, version, status, name pattern, title pattern` (then `limit, offset`
/// for [`GroupedSql::select`]).
#[derive(Debug)]
pub(crate) struct GroupedSql {
    pub select: String,
    pub count: String,
    pub params: [Option<String>; 5],
}

fn precedence(table: &str) -> std::borrow::Cow<'static, str> {
    match table {
        "code_systems" => super::cs_precedence_order_by("code_systems"),
        "concept_maps" => super::cm_precedence_order_by("concept_maps"),
        _ => super::vs_precedence_order_by("value_sets"),
    }
}

/// A ConceptMap's source / target scope column (the maps browser lists
/// both); a typed NULL for the other tables.
fn scope_column(table: &str, alias: &str, column: &str) -> String {
    if table == "concept_maps" {
        format!("{alias}{column}")
    } else {
        "CAST(NULL AS TEXT)".to_string()
    }
}

/// A `LIKE` pattern for a FHIR string filter (case-insensitive; `%`, `_` and
/// `\` in the value are escaped, `ESCAPE '\'`).
pub(crate) fn like_pattern(value: &str, mode: FhirStringSearchMode) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    for ch in value.to_lowercase().chars() {
        if matches!(ch, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    match mode {
        FhirStringSearchMode::Prefix => format!("{escaped}%"),
        FhirStringSearchMode::Contains => format!("%{escaped}%"),
        FhirStringSearchMode::Exact => escaped,
    }
}

/// Filters a grouped search cannot honour (answered with a 400 by the search
/// operation instead of being silently dropped).
pub(crate) fn unsupported_filter(query: &ResourceSearchQuery) -> Option<&'static str> {
    if query.id.is_some() {
        Some("_id")
    } else if query.system.is_some() {
        Some("system")
    } else if query.supplements.is_some() {
        Some("supplements")
    } else if query.content_mode.is_some() {
        Some("content-mode")
    } else if query.publisher.is_some() {
        Some("publisher")
    } else if !query.last_updated.is_empty() {
        Some("_lastUpdated")
    } else {
        None
    }
}

/// Build the grouped `select` and `count` statements for `table`.
pub(crate) fn build(table: &str, dialect: Dialect, query: &ResourceSearchQuery) -> GroupedSql {
    let p = |n: usize| match dialect {
        Dialect::Postgres => format!("${n}::text"),
        Dialect::Sqlite => format!("?{n}"),
    };
    let pl = |n: usize| match dialect {
        Dialect::Postgres => format!("${n}"),
        Dialect::Sqlite => format!("?{n}"),
    };
    let lower = |col: &str| format!("LOWER(COALESCE({col}, ''))");
    let filter = format!(
        "({u} IS NULL OR url = {u}) \
         AND ({v} IS NULL OR version = {v}) \
         AND ({s} IS NULL OR status = {s}) \
         AND ({n} IS NULL OR {name} LIKE {n} ESCAPE '\\') \
         AND ({t} IS NULL OR {title} LIKE {t} ESCAPE '\\')",
        u = p(1),
        v = p(2),
        s = p(3),
        n = p(4),
        t = p(5),
        name = lower("name"),
        title = lower("title"),
    );
    let date = match dialect {
        Dialect::Postgres => format!("({table}.resource_json->>'date')"),
        Dialect::Sqlite => format!("json_extract({table}.resource_json, '$.date')"),
    };
    let updated = if table == "concept_maps" {
        "created_at"
    } else {
        "updated_at"
    };
    let filtered = query.url.is_some()
        || query.version.is_some()
        || query.status.is_some()
        || query.name.is_some()
        || query.title.is_some();
    // Byte-order comparison of the lower-cased label: sorting ~30,000 group
    // labels under a linguistic collation dominated the query on PostgreSQL.
    let fast = match dialect {
        Dialect::Postgres => " COLLATE \"C\"",
        Dialect::Sqlite => "",
    };
    let order = match (query.group, query.sort) {
        (Some(VersionGroup::Older), _) => "url, hm_rn".to_string(),
        (_, Some(sort)) => {
            let mut order = super::explicit_sort_order_by(table, sort);
            for column in ["name", "title", "url", "version", "status"] {
                order = order.replace(
                    &format!("LOWER({column}) "),
                    &format!("LOWER({column}){fast} "),
                );
            }
            order
        }
        (_, None) => format!(
            "(CASE WHEN COALESCE(title, name, '') = '' THEN 1 ELSE 0 END), \
             LOWER(COALESCE(title, name, url)){fast}, url"
        ),
    };
    let select = match query.group {
        // `older`: the other versions of the matched canonicals. Used for one
        // canonical at a time (the "+N older" rows), so a window over its
        // few rows is cheap.
        Some(VersionGroup::Older) => format!(
            "WITH hm_ranked AS ( \
               SELECT {table}.id, {table}.url, {table}.version, {table}.name, {table}.title, \
                      {table}.status, {table}.fhir_id, {date} AS hm_date, {src} AS hm_src, {tgt} AS hm_tgt, \
                      ROW_NUMBER() OVER (PARTITION BY {table}.url ORDER BY {prec}) AS hm_rn, \
                      COUNT(*) OVER (PARTITION BY {table}.url) AS hm_n \
               FROM {table} WHERE {table}.url IN (SELECT url FROM {table} WHERE {filter})) \
             SELECT id, url, version, name, title, status, fhir_id, hm_n, hm_date, hm_src, hm_tgt \
             FROM hm_ranked WHERE hm_rn > 1 ORDER BY {order} LIMIT {limit} OFFSET {offset}",
            prec = precedence(table),
            limit = pl(6),
            offset = pl(7),
            src = scope_column(table, "", "source_uri"),
            tgt = scope_column(table, "", "target_uri"),
        ),
        // `true`: page the canonical groups first (one aggregate pass over
        // the url index), then pick the precedence winner of only the page's
        // groups — 25 small index lookups instead of ranking every row.
        _ => format!(
            "WITH hm_groups AS ( \
               SELECT url, COUNT(*) AS hm_n, MAX(name) AS name, MAX(title) AS title, \
                      MAX(version) AS version, MAX(status) AS status, MAX({updated}) AS {updated}, \
                      MAX(id) AS id \
               FROM {table} WHERE {scope} \
               GROUP BY url), \
             hm_page AS ( \
               SELECT hm_groups.*, ROW_NUMBER() OVER (ORDER BY {order}) AS hm_pos \
               FROM hm_groups ORDER BY {order} LIMIT {limit} OFFSET {offset}), \
             hm_rep AS ( \
               SELECT hm_page.hm_pos, hm_page.hm_n, \
                      (SELECT {table}.id FROM {table} WHERE {table}.url = hm_page.url \
                       ORDER BY {prec} LIMIT 1) AS hm_id \
               FROM hm_page) \
             SELECT t.id, t.url, t.version, t.name, t.title, t.status, t.fhir_id, hm_rep.hm_n, \
                    {tdate} AS hm_date, {src} AS hm_src, {tgt} AS hm_tgt \
             FROM hm_rep JOIN {table} t ON t.id = hm_rep.hm_id ORDER BY hm_rep.hm_pos",
            prec = precedence(table),
            limit = pl(6),
            offset = pl(7),
            src = scope_column(table, "t.", "source_uri"),
            tgt = scope_column(table, "t.", "target_uri"),
            // Unfiltered (the common case): one pass, no semi-join. Filtered:
            // a group matches when any of its versions does, and still
            // counts all of them.
            scope = if filtered {
                format!("url IN (SELECT url FROM {table} WHERE {filter})")
            } else {
                filter.clone()
            },
            tdate = match dialect {
                Dialect::Postgres => "(t.resource_json->>'date')".to_string(),
                Dialect::Sqlite => "json_extract(t.resource_json, '$.date')".to_string(),
            },
        ),
    };
    let count = match query.group {
        Some(VersionGroup::Older) => format!(
            "SELECT COUNT(*) - COUNT(DISTINCT url) FROM {table} \
             WHERE url IN (SELECT url FROM {table} WHERE {filter})"
        ),
        _ => format!("SELECT COUNT(DISTINCT url) FROM {table} WHERE {filter}"),
    };
    let pattern = |value: &Option<String>, mode| value.as_deref().map(|v| like_pattern(v, mode));
    GroupedSql {
        select,
        count,
        params: [
            query.url.clone(),
            query.version.clone(),
            query.status.clone(),
            pattern(&query.name, query.name_mode),
            pattern(&query.title, query.title_mode),
        ],
    }
}

/// One grouped entry as a summary resource: id, url, version, name, title,
/// status, date and the version-count extension.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resource(
    resource_type: &str,
    storage_id: &str,
    url: &str,
    version: Option<&str>,
    name: Option<&str>,
    title: Option<&str>,
    status: &str,
    fhir_id: Option<&str>,
    versions: i64,
    date: Option<&str>,
    scope: [Option<&str>; 2],
) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "resourceType": resource_type,
        "id": crate::resource_ids::reported_id(storage_id, fhir_id),
        "meta": {
            "tag": [{"system": "http://terminology.hl7.org/CodeSystem/v3-ObservationValue", "code": "SUBSETTED"}],
            "extension": [{"url": VERSION_COUNT_URL, "valueInteger": versions}]
        },
        "url": url,
        "status": status,
    });
    if let Some(v) = version {
        obj["version"] = v.into();
    }
    if let Some(n) = name {
        obj["name"] = n.into();
    }
    if let Some(t) = title {
        obj["title"] = t.into();
    }
    if let Some(d) = date.filter(|d| !d.is_empty()) {
        obj["date"] = d.into();
    }
    for (key, value) in ["sourceUri", "targetUri"].into_iter().zip(scope) {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            obj[key] = v.into();
        }
    }
    obj
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_patterns_escape_wildcards_and_follow_the_mode() {
        assert_eq!(
            like_pattern("US_Core", FhirStringSearchMode::Contains),
            "%us\\_core%"
        );
        assert_eq!(like_pattern("Lo", FhirStringSearchMode::Prefix), "lo%");
        assert_eq!(like_pattern("50%", FhirStringSearchMode::Exact), "50\\%");
    }

    #[test]
    fn the_grouped_select_ranks_by_the_resolver_precedence_and_pages_groups() {
        let mut q = ResourceSearchQuery {
            group: Some(VersionGroup::Current),
            ..Default::default()
        };
        let g = build("code_systems", Dialect::Postgres, &q);
        assert!(g.select.contains("GROUP BY url), hm_page AS"));
        assert!(
            g.select
                .contains(&*super::super::cs_precedence_order_by("code_systems"))
        );
        assert!(g.select.contains("LIMIT $6 OFFSET $7"));
        assert!(g.select.ends_with("ORDER BY hm_rep.hm_pos"));
        assert!(
            g.count
                .starts_with("SELECT COUNT(DISTINCT url) FROM code_systems")
        );
        q.group = Some(VersionGroup::Older);
        let older = build("value_sets", Dialect::Sqlite, &q);
        assert!(older.select.contains("WHERE hm_rn > 1 ORDER BY url, hm_rn"));
        assert!(
            older
                .select
                .contains("json_extract(value_sets.resource_json, '$.date')")
        );
        assert!(older.select.ends_with("LIMIT ?6 OFFSET ?7"));
    }

    #[test]
    fn grouped_entries_carry_the_version_count_and_date() {
        let r = resource(
            "ValueSet",
            "us-core-race|7.0.0",
            "http://x/vs",
            Some("7.0.0"),
            None,
            Some("Race"),
            "active",
            Some("us-core-race"),
            4,
            Some("2024-01-01"),
            [None, None],
        );
        assert_eq!(r["id"], "us-core-race");
        assert!(r.get("sourceUri").is_none());
        let m = resource(
            "ConceptMap",
            "m",
            "http://x/cm",
            None,
            None,
            None,
            "active",
            None,
            2,
            None,
            [Some("http://x/a"), Some("")],
        );
        assert_eq!(m["sourceUri"], "http://x/a");
        assert!(m.get("targetUri").is_none());
        assert_eq!(r["meta"]["extension"][0]["valueInteger"], 4);
        assert_eq!(r["date"], "2024-01-01");
    }
}
