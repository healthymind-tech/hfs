//! One database (docs/design/unified-server.md §5): the terminology store
//! lives in schema `terminology` of the HFS database and its connections use
//! `search_path=terminology,public`. Any unqualified catalog lookup of an
//! object missing from `terminology` falls through to HFS's `public` objects
//! (§5.3). This file guards that whole class:
//!
//! - `no_unqualified_catalog_lookups` (no database): every SQL string literal
//!   in the PostgreSQL persistence backend and in HTS that looks a relation
//!   up in the catalogs (`to_regclass`, `information_schema`, `pg_class`,
//!   `pg_attribute`, `pg_constraint`, `pg_indexes`, `pg_stats`, …) must pin
//!   it to `current_schema()` in the same statement, and no literal may use a
//!   bare `'name'::regclass` cast.
//! - `postgres_integration_terminology_schema_isolation`: the full HFS schema
//!   in `public`, then the embedded resource store initialised in
//!   `terminology` — every store table is in `terminology` and `public` is
//!   byte-for-byte unchanged (relations, columns, statistics targets,
//!   constraints, triggers, functions, row counts); dropping a store table
//!   and restarting recreates it in `terminology`.
//! - the store creating its missing schema instead of falling back to
//!   `public`; `hm-0011`/`hm-0012` (idempotent, privilege preflight) and
//!   `hfs_terminology_state`.
//!
//! The PostgreSQL tests run against an existing server named by
//! `HFS_TEST_PG_HOST` / `HFS_TEST_PG_PORT` (user and password `postgres`),
//! each in a database of its own; without those variables they are skipped
//! (HealthyMind CI sets them, docs/ops/ci.md). HTS's own tables are covered
//! by `crates/hts/tests/postgres_terminology_schema.rs`.

use std::path::{Path, PathBuf};

fn workspace_crates() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .to_path_buf()
}

// ── static guard ────────────────────────────────────────────────────────────

/// A Rust source file split into code and string-literal pieces. Comments
/// are dropped; char literals and lifetimes stay code.
#[derive(Debug)]
enum Piece {
    Code(String),
    Str { text: String, line: usize },
}

fn lex(src: &str) -> Vec<Piece> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut code = String::new();
    let mut i = 0;
    let line_at = |pos: usize| src[..pos].bytes().filter(|&c| c == b'\n').count() + 1;
    while i < b.len() {
        let c = b[i];
        // comments
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        // raw strings r"..." r#"..."# (also br)
        let raw_start = if c == b'r' && !prev_is_ident(b, i) {
            Some(i + 1)
        } else if c == b'b' && b.get(i + 1) == Some(&b'r') && !prev_is_ident(b, i) {
            Some(i + 2)
        } else {
            None
        };
        if let Some(mut j) = raw_start {
            let mut hashes = 0;
            while b.get(j) == Some(&b'#') {
                hashes += 1;
                j += 1;
            }
            if b.get(j) == Some(&b'"') {
                let start = j + 1;
                let close: String = std::iter::once('"')
                    .chain(std::iter::repeat_n('#', hashes))
                    .collect();
                let end = src[start..]
                    .find(&close)
                    .map(|k| start + k)
                    .unwrap_or(b.len());
                out.push(Piece::Code(std::mem::take(&mut code)));
                out.push(Piece::Str {
                    text: src[start..end].to_string(),
                    line: line_at(start),
                });
                i = end + close.len();
                continue;
            }
        }
        if c == b'"' {
            let start = i + 1;
            let mut j = start;
            let mut text = String::new();
            while j < b.len() && b[j] != b'"' {
                if b[j] == b'\\' {
                    // `\` + newline: line continuation (skip leading spaces)
                    match b.get(j + 1) {
                        Some(b'\n') => {
                            j += 2;
                            while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
                                j += 1;
                            }
                            text.push(' ');
                            continue;
                        }
                        Some(b'n') | Some(b't') => {
                            text.push(' ');
                            j += 2;
                            continue;
                        }
                        Some(&e) => {
                            text.push(e as char);
                            j += 2;
                            continue;
                        }
                        None => break,
                    }
                }
                let ch = src[j..].chars().next().unwrap();
                text.push(ch);
                j += ch.len_utf8();
            }
            out.push(Piece::Code(std::mem::take(&mut code)));
            out.push(Piece::Str {
                text,
                line: line_at(start),
            });
            i = j + 1;
            continue;
        }
        if c == b'\'' {
            // char literal ('x', '\n', '\'', '\u{..}') or lifetime ('a)
            if b.get(i + 1) == Some(&b'\\') {
                let end = src[i + 2..]
                    .find('\'')
                    .map(|k| i + 2 + k)
                    .unwrap_or(b.len());
                code.push_str("'?'");
                i = end + 1;
                continue;
            }
            let next = src[i + 1..].chars().next();
            if let Some(ch) = next {
                let after = i + 1 + ch.len_utf8();
                if b.get(after) == Some(&b'\'') {
                    code.push_str("'?'");
                    i = after + 1;
                    continue;
                }
            }
            code.push('\'');
            i += 1;
            continue;
        }
        let ch = src[i..].chars().next().unwrap();
        code.push(ch);
        i += ch.len_utf8();
    }
    out.push(Piece::Code(code));
    out
}

fn prev_is_ident(b: &[u8], i: usize) -> bool {
    i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')
}

/// The string literals of `src` outside `#[cfg(test)] mod … { … }` blocks.
fn production_literals(src: &str) -> Vec<(String, usize)> {
    let pieces = lex(src);
    let mut out = Vec::new();
    // Brace depth at which a skipped test module closes.
    let mut skip_until: Option<i64> = None;
    let mut depth: i64 = 0;
    let mut pending_cfg_test = false;
    for piece in pieces {
        match piece {
            Piece::Code(code) => {
                let mut rest = code.as_str();
                while !rest.is_empty() {
                    if skip_until.is_none() {
                        if let Some(stripped) = rest.strip_prefix("#[cfg(test)]") {
                            pending_cfg_test = true;
                            rest = stripped;
                            continue;
                        }
                    }
                    let ch = rest.chars().next().unwrap();
                    if pending_cfg_test && skip_until.is_none() {
                        let trimmed = rest.trim_start();
                        if trimmed.starts_with("mod ") || trimmed.starts_with("pub mod ") {
                            // `mod name {` → skip its body; `mod name;` → nothing to skip.
                            let semi = trimmed.find(';');
                            let brace = trimmed.find('{');
                            if let Some(bp) = brace {
                                if semi.is_none_or(|s| bp < s) {
                                    skip_until = Some(depth);
                                }
                            }
                            pending_cfg_test = false;
                        } else if !ch.is_whitespace() && ch != '#' {
                            // `#[cfg(test)]` on an item other than a module (a
                            // function, an import): only modules are skipped.
                            pending_cfg_test = false;
                        }
                    }
                    if ch == '{' {
                        depth += 1;
                    } else if ch == '}' {
                        depth -= 1;
                        if skip_until == Some(depth) {
                            skip_until = None;
                        }
                    }
                    rest = &rest[ch.len_utf8()..];
                }
            }
            Piece::Str { text, line } => {
                if skip_until.is_none() {
                    out.push((text, line));
                }
            }
        }
    }
    out
}

/// Catalog objects whose lookups resolve or list relations across schemas.
const CATALOG_MARKERS: &[&str] = &[
    "to_regclass",
    "information_schema.",
    "pg_class",
    "pg_attribute",
    "pg_constraint",
    "pg_index ",
    "pg_index\n",
    "pg_indexes",
    "pg_tables",
    "pg_views",
    "pg_matviews",
    "pg_sequences",
    "pg_trigger",
    "pg_stats",
    "pg_stat_user_tables",
    "pg_stat_user_indexes",
    "pg_statio_user_tables",
    "pg_statistic_ext",
];

/// Violations in one source text: `(line, statement excerpt, reason)`.
fn unqualified_lookups(src: &str) -> Vec<(usize, String, &'static str)> {
    let mut found = Vec::new();
    for (text, line) in production_literals(src) {
        let lower = text.to_ascii_lowercase();
        // A bare `'name'::regclass` resolves through search_path.
        let bytes = lower.as_bytes();
        let mut from = 0;
        while let Some(k) = lower[from..].find("'::regclass") {
            let end = from + k;
            let start = lower[..end].rfind('\'').unwrap_or(end);
            let name = &lower[start + 1..end];
            let ident = !name.is_empty()
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'{' || c == b'}');
            if ident && !name.starts_with("pg_") {
                found.push((line, excerpt(&text, end), "bare 'name'::regclass cast"));
            }
            from = end + 1;
            let _ = bytes;
        }
        // Every statement that reads the catalogs names current_schema().
        for statement in lower.split(';') {
            if CATALOG_MARKERS.iter().any(|m| statement.contains(m))
                && !statement.contains("current_schema()")
            {
                found.push((
                    line,
                    statement.split_whitespace().collect::<Vec<_>>().join(" "),
                    "catalog lookup without current_schema()",
                ));
            }
        }
    }
    found
}

fn excerpt(text: &str, at: usize) -> String {
    let lo = text[..at]
        .char_indices()
        .rev()
        .nth(60)
        .map_or(0, |(i, _)| i);
    let hi = text[at..]
        .char_indices()
        .nth(30)
        .map_or(text.len(), |(i, _)| at + i);
    text[lo..hi]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn guard_flags_unqualified_lookups_and_skips_test_modules() {
    let bad = r#"
        fn f() {
            let a = "SELECT to_regclass('hm_schema_migrations') IS NOT NULL";
            let b = "SELECT 1 FROM information_schema.columns WHERE table_name = 'x'";
            let c = "SELECT attstattarget FROM pg_attribute WHERE attrelid = 'search_index'::regclass";
            let d = "DO $$ BEGIN FOR r IN SELECT conname FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid WHERE t.relname = 'x' LOOP EXECUTE 'y'; END LOOP; END $$;";
        }
    "#;
    let found = unqualified_lookups(bad);
    assert_eq!(found.len(), 5, "{found:#?}");

    let good = r#"
        // to_regclass('in a comment') is not SQL
        fn f() {
            let a = "SELECT to_regclass(format('%I.%I', current_schema(), 'hm_schema_migrations')) IS NOT NULL";
            let b = "SELECT 1 FROM information_schema.columns
                     WHERE table_schema = current_schema() AND table_name = 'x'";
            let c = "SELECT 1 FROM pg_index i \
                     JOIN pg_class c ON c.oid = i.indexrelid \
                     WHERE c.relnamespace = to_regnamespace(current_schema())";
            let ch = '"'; let lt: &'static str = "plain";
            let catalog = "SELECT 1 FROM pg_depend WHERE classid = 'pg_class'::regclass AND x IN (SELECT oid FROM pg_class WHERE relnamespace = to_regnamespace(current_schema()))";
        }
        #[cfg(test)]
        mod tests {
            fn t() { let x = "SELECT to_regclass('schema_version')"; { let y = "{"; } }
        }
        fn after() { let z = "SELECT 1"; }
    "#;
    assert!(
        unqualified_lookups(good).is_empty(),
        "{:#?}",
        unqualified_lookups(good)
    );
    // The code after the skipped test module is scanned again.
    let after = format!("{good}\nfn g() {{ let q = \"SELECT to_regclass('x')\"; }}");
    assert_eq!(unqualified_lookups(&after).len(), 1);
}

#[test]
fn no_unqualified_catalog_lookups() {
    let crates = workspace_crates();
    let mut files = Vec::new();
    rust_files(
        &crates.join("persistence/src/backends/postgres"),
        &mut files,
    );
    rust_files(&crates.join("hts/src"), &mut files);
    files.sort();
    assert!(
        files.len() > 30,
        "expected the postgres backend and HTS sources, found {}",
        files.len()
    );
    let mut report = Vec::new();
    for file in &files {
        let src = std::fs::read_to_string(file).expect("read source");
        for (line, statement, reason) in unqualified_lookups(&src) {
            let rel = file.strip_prefix(&crates).unwrap_or(file);
            report.push(format!(
                "{}:{line}: {reason}: {}",
                rel.display(),
                statement.chars().take(220).collect::<String>()
            ));
        }
    }
    assert!(
        report.is_empty(),
        "unqualified catalog lookups (they fall through to HFS's `public` objects when the \
         terminology store's own object is missing; pin them with current_schema(), e.g. \
         to_regclass(format('%I.%I', current_schema(), 'x')) or table_schema = current_schema()):\n{}",
        report.join("\n")
    );
}

/// `hm-0011`'s SQL lives in two places (persistence applies it; the
/// `helios-terminology` contract documents it). They must not drift.
#[test]
fn hm_0011_sql_matches_the_terminology_contract() {
    #[cfg(feature = "postgres")]
    {
        let contract =
            std::fs::read_to_string(workspace_crates().join("terminology/src/schema.rs"))
                .expect("crates/terminology/src/schema.rs");
        let literals = production_literals(&contract);
        let theirs = literals
            .iter()
            .map(|(t, _)| t.as_str())
            .find(|t| t.contains("CREATE TABLE IF NOT EXISTS hfs_terminology_state"))
            .expect("HM_0011_SQL in the terminology crate");
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(
            norm(theirs),
            norm(helios_persistence::backends::postgres::HM_0011_SQL)
        );
        assert_eq!(
            helios_persistence::backends::postgres::UNIFIED_FORK_MIGRATIONS,
            [
                "hm-0011-terminology-schema",
                "hm-0012-console-terminology-bindings"
            ]
        );
    }
}

// ── PostgreSQL ──────────────────────────────────────────────────────────────

#[cfg(feature = "postgres")]
mod pg {
    use helios_persistence::backends::postgres::{PostgresBackend, TerminologyState};

    const SCHEMA: &str = "terminology";

    /// Tables the embedded store must own in `terminology`
    /// (`helios_terminology::schema::EMBEDDED_STORE_TABLES` plus the fork
    /// tables every persistence schema carries).
    const STORE_TABLES: &[&str] = &[
        "resources",
        "resource_history",
        "search_index",
        "schema_version",
        "hm_schema_migrations",
        "search_parameter_epochs",
        "cluster_jobs",
        "resource_change_feed",
    ];

    struct Db {
        host: String,
        port: u16,
        name: String,
    }

    impl Db {
        fn url(&self) -> String {
            format!(
                "postgres://postgres:postgres@{}:{}/{}",
                self.host, self.port, self.name
            )
        }
        fn url_as(&self, user: &str, password: &str) -> String {
            format!(
                "postgres://{user}:{password}@{}:{}/{}",
                self.host, self.port, self.name
            )
        }
        /// What `helios_terminology::schema::with_search_path` produces.
        fn terminology_url(&self) -> String {
            format!(
                "{}?options=-c%20search_path%3D{SCHEMA}%2Cpublic",
                self.url()
            )
        }
        async fn client(&self) -> tokio_postgres::Client {
            let (client, conn) = tokio_postgres::connect(&self.url(), tokio_postgres::NoTls)
                .await
                .expect("connect");
            tokio::spawn(conn);
            client
        }
    }

    async fn fresh_db(tag: &str) -> Option<Db> {
        let host = std::env::var("HFS_TEST_PG_HOST").ok()?;
        let port: u16 = std::env::var("HFS_TEST_PG_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(5432);
        let name = format!("hm_u1_{}_{tag}", std::process::id());
        let admin = format!("postgres://postgres:postgres@{host}:{port}/postgres");
        let (client, conn) = tokio_postgres::connect(&admin, tokio_postgres::NoTls)
            .await
            .expect("connect to HFS_TEST_PG_HOST");
        tokio::spawn(conn);
        for sql in [
            format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
            format!("CREATE DATABASE {name}"),
        ] {
            client
                .batch_execute(&sql)
                .await
                .expect("create test database");
        }
        Some(Db { host, port, name })
    }

    macro_rules! db_or_skip {
        ($tag:expr) => {
            match fresh_db($tag).await {
                Some(db) => db,
                None => {
                    eprintln!("skipped: HFS_TEST_PG_HOST is not set");
                    return;
                }
            }
        };
    }

    /// Everything about schema `public` an errant store could change:
    /// relations (kind, reloptions, index definitions), columns (type, not
    /// null, default, statistics target), constraints, triggers, functions,
    /// extended statistics and every table's row count.
    async fn public_snapshot(client: &tokio_postgres::Client) -> Vec<String> {
        let mut lines: Vec<String> = client
            .query(
                "SELECT 'rel ' || c.relname || ' ' || c.relkind::text || ' ' || coalesce(array_to_string(c.reloptions, ','), '')
                        || ' ' || coalesce(pg_get_indexdef(c.oid), '')
                   FROM pg_class c WHERE c.relnamespace = 'public'::regnamespace
                 UNION ALL
                 SELECT 'col ' || c.relname || '.' || a.attname || ' ' || format_type(a.atttypid, a.atttypmod)
                        || ' ' || a.attnotnull || ' ' || coalesce(a.attstattarget::text, '-')
                        || ' ' || coalesce(pg_get_expr(d.adbin, d.adrelid), '')
                   FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid
                   LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
                  WHERE c.relnamespace = 'public'::regnamespace AND a.attnum > 0 AND NOT a.attisdropped
                 UNION ALL
                 SELECT 'con ' || conrelid::regclass::text || ' ' || conname || ' ' || pg_get_constraintdef(oid)
                   FROM pg_constraint WHERE connamespace = 'public'::regnamespace
                 UNION ALL
                 SELECT 'trg ' || c.relname || ' ' || t.tgname || ' ' || t.tgenabled::text || ' ' || t.tgfoid::regproc::text
                   FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid
                  WHERE c.relnamespace = 'public'::regnamespace AND NOT t.tgisinternal
                 UNION ALL
                 SELECT 'fn ' || p.proname || '(' || pg_get_function_identity_arguments(p.oid) || ') ' || md5(p.prosrc)
                   FROM pg_proc p WHERE p.pronamespace = 'public'::regnamespace
                    AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.objid = p.oid AND d.deptype = 'e')
                 UNION ALL
                 SELECT 'stx ' || stxname || ' ' || coalesce(stxstattarget::text, '-')
                   FROM pg_statistic_ext WHERE stxnamespace = 'public'::regnamespace",
                &[],
            )
            .await
            .expect("catalog snapshot")
            .into_iter()
            .map(|r| r.get::<_, String>(0))
            .collect();
        let tables: Vec<String> = client
            .query(
                "SELECT relname::text FROM pg_class
                  WHERE relnamespace = 'public'::regnamespace AND relkind IN ('r', 'p')",
                &[],
            )
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.get(0))
            .collect();
        for t in tables {
            let n: i64 = client
                .query_one(&format!("SELECT count(*) FROM public.\"{t}\""), &[])
                .await
                .unwrap()
                .get(0);
            lines.push(format!("rows {t} {n}"));
        }
        lines.sort();
        lines
    }

    async fn tables_in(client: &tokio_postgres::Client, schema: &str) -> Vec<String> {
        client
            .query(
                "SELECT table_name::text FROM information_schema.tables
                  WHERE table_schema = $1 ORDER BY 1",
                &[&schema],
            )
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.get(0))
            .collect()
    }

    fn assert_same(before: &[String], after: &[String]) {
        let gone: Vec<_> = before.iter().filter(|l| !after.contains(l)).collect();
        let new: Vec<_> = after.iter().filter(|l| !before.contains(l)).collect();
        assert!(
            gone.is_empty() && new.is_empty(),
            "schema `public` changed.\nremoved: {gone:#?}\nadded: {new:#?}"
        );
    }

    /// HFS in `public`, as production: full schema plus the unified
    /// migrations.
    async fn hfs_public(db: &Db) -> PostgresBackend {
        let hfs = PostgresBackend::from_connection_string(&db.url())
            .await
            .expect("hfs backend");
        hfs.init_schema().await.expect("hfs schema");
        hfs
    }

    #[tokio::test]
    async fn postgres_integration_terminology_schema_isolation() {
        let db = db_or_skip!("iso");
        let hfs = hfs_public(&db).await;
        assert_eq!(hfs.current_schema().await.unwrap(), "public");
        let applied = hfs.apply_unified_migrations(SCHEMA).await.unwrap();
        assert_eq!(
            applied,
            [
                "hm-0011-terminology-schema",
                "hm-0012-console-terminology-bindings"
            ]
        );
        let admin = db.client().await;
        assert!(
            tables_in(&admin, SCHEMA).await.is_empty(),
            "hm-0011 only creates the schema"
        );
        let before = public_snapshot(&admin).await;

        // The embedded resource store, as the unified server opens it.
        let store = PostgresBackend::from_connection_string(&db.terminology_url())
            .await
            .expect("store backend");
        store.init_schema().await.expect("store schema");
        assert_eq!(store.current_schema().await.unwrap(), SCHEMA);

        let in_terminology = tables_in(&admin, SCHEMA).await;
        for t in STORE_TABLES {
            assert!(
                in_terminology.iter().any(|x| x == t),
                "{t} missing from schema terminology: {in_terminology:?}"
            );
        }
        assert_same(&before, &public_snapshot(&admin).await);

        // Its own ladder and fork track, not HFS's.
        let (version, forks): (i32, i64) = {
            let r = admin
                .query_one(
                    "SELECT (SELECT max(version) FROM terminology.schema_version),
                            (SELECT count(*) FROM terminology.hm_schema_migrations)",
                    &[],
                )
                .await
                .unwrap();
            (r.get(0), r.get(1))
        };
        let hfs_version: i32 = admin
            .query_one("SELECT max(version) FROM public.schema_version", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(version, hfs_version);
        assert!(forks >= 10, "store fork track has {forks} rows");
        let unified_in_store: i64 = admin
            .query_one(
                "SELECT count(*) FROM terminology.hm_schema_migrations WHERE name LIKE 'hm-001_-%' AND name >= 'hm-0011'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(unified_in_store, 0, "hm-0011.. never run inside the store");

        // A write through the store lands in terminology.resources only.
        use helios_persistence::core::ResourceStorage;
        use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
        let tenant = TenantContext::new(TenantId::new("default"), TenantPermissions::full_access());
        store
            .create(
                &tenant,
                "CodeSystem",
                serde_json::json!({"resourceType": "CodeSystem", "id": "u1-iso", "url": "http://example.org/u1", "status": "active", "content": "complete"}),
                helios_fhir::FhirVersion::default(),
            )
            .await
            .expect("create through the store");
        let (in_store, in_public): (i64, i64) = {
            let r = admin
                .query_one(
                    "SELECT (SELECT count(*) FROM terminology.resources WHERE id = 'u1-iso'),
                            (SELECT count(*) FROM public.resources WHERE id = 'u1-iso')",
                    &[],
                )
                .await
                .unwrap();
            (r.get(0), r.get(1))
        };
        assert_eq!((in_store, in_public), (1, 0));
        let before_restart = public_snapshot(&admin).await;
        assert_eq!(
            before
                .iter()
                .filter(|l| l.starts_with("rows resources "))
                .count(),
            1
        );

        // The classic hazard: the store's fork table is missing while HFS's
        // exists. A restart must recreate it in terminology, not adopt HFS's.
        admin
            .batch_execute("DROP TABLE terminology.hm_schema_migrations; DROP TABLE terminology.search_parameter_epochs CASCADE")
            .await
            .unwrap();
        drop(store);
        let store = PostgresBackend::from_connection_string(&db.terminology_url())
            .await
            .unwrap();
        store.init_schema().await.expect("restart");
        let again = tables_in(&admin, SCHEMA).await;
        assert!(again.iter().any(|t| t == "hm_schema_migrations"));
        assert!(again.iter().any(|t| t == "search_parameter_epochs"));
        assert_same(&before_restart, &public_snapshot(&admin).await);

        // HFS restarting next to it is equally unaffected.
        let hfs2 = hfs_public(&db).await;
        assert!(
            hfs2.apply_unified_migrations(SCHEMA)
                .await
                .unwrap()
                .is_empty()
        );
        assert_same(&before_restart, &public_snapshot(&admin).await);
    }

    #[tokio::test]
    async fn store_creates_its_missing_schema_instead_of_using_public() {
        let db = db_or_skip!("mk");
        let _hfs = hfs_public(&db).await;
        let admin = db.client().await;
        async fn exists(c: &tokio_postgres::Client) -> i64 {
            c.query_one(
                "SELECT count(*) FROM pg_namespace WHERE nspname = 'terminology'",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0)
        }
        assert_eq!(exists(&admin).await, 0);
        let before = public_snapshot(&admin).await;
        let store = PostgresBackend::from_connection_string(&db.terminology_url())
            .await
            .unwrap();
        store.init_schema().await.expect("store schema");
        assert_eq!(exists(&admin).await, 1);
        assert!(
            tables_in(&admin, SCHEMA)
                .await
                .iter()
                .any(|t| t == "resources")
        );
        assert_same(&before, &public_snapshot(&admin).await);
    }

    /// Remote terminology: only hm-0012 (console bindings), no terminology
    /// schema; going embedded later adds hm-0011 alone.
    #[tokio::test]
    async fn console_migration_without_terminology_schema_then_embedded() {
        let db = db_or_skip!("cm");
        let hfs = hfs_public(&db).await;
        let admin = db.client().await;
        assert_eq!(
            hfs.apply_console_fork_migrations().await.unwrap(),
            vec!["hm-0012-console-terminology-bindings"]
        );
        assert!(hfs.apply_console_fork_migrations().await.unwrap().is_empty());
        let schemas: i64 = admin
            .query_one(
                "SELECT count(*) FROM pg_namespace WHERE nspname = 'terminology'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(schemas, 0, "no terminology schema before the data move");
        let bindings: i64 = admin
            .query_one(
                "SELECT count(*) FROM information_schema.tables \
                 WHERE table_schema = 'public' AND table_name = 'hfs_console_scope_bindings'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(bindings, 1);
        assert_eq!(
            hfs.apply_unified_migrations(SCHEMA).await.unwrap(),
            vec!["hm-0011-terminology-schema"]
        );
    }

    #[tokio::test]
    async fn unified_migrations_preflight_and_terminology_state() {
        let db = db_or_skip!("st");
        let hfs = hfs_public(&db).await;
        assert_eq!(hfs.terminology_state().await.unwrap(), None);

        // A role that may create tables in public but not schemas.
        let admin = db.client().await;
        let role = format!("u1_nocreate_{}", std::process::id());
        admin
            .batch_execute(&format!(
                "DROP ROLE IF EXISTS {role}; CREATE ROLE {role} LOGIN PASSWORD 'pw';
                 REVOKE CREATE ON DATABASE {db} FROM PUBLIC;
                 GRANT CONNECT ON DATABASE {db} TO {role};
                 GRANT USAGE, CREATE ON SCHEMA public TO {role};
                 GRANT SELECT, INSERT ON public.hm_schema_migrations TO {role};",
                db = db.name
            ))
            .await
            .unwrap();
        let limited = PostgresBackend::from_connection_string(&db.url_as(&role, "pw"))
            .await
            .unwrap();
        let err = limited
            .apply_unified_migrations(SCHEMA)
            .await
            .expect_err("no CREATE on the database");
        let msg = err.to_string();
        assert!(msg.contains("needs CREATE on the database"), "{msg}");
        assert!(msg.contains(&role), "{msg}");
        let n: i64 = admin
            .query_one(
                "SELECT count(*) FROM pg_namespace WHERE nspname = 'terminology'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(n, 0, "nothing half-applied");
        drop(limited);

        // As the owner: applied once, then a no-op.
        assert_eq!(hfs.apply_unified_migrations(SCHEMA).await.unwrap().len(), 2);
        assert!(
            hfs.apply_unified_migrations(SCHEMA)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(hfs.apply_unified_migrations("Bad;Name").await.is_err());
        assert_eq!(hfs.terminology_state().await.unwrap(), None);

        let moved_at = chrono::Utc::now();
        let mut state = TerminologyState {
            store: "schema:terminology".into(),
            moved_from: Some("database:hts".into()),
            moved_at: Some(moved_at),
            source_fingerprint: Some("tables=24 rows=123 fp=abc".into()),
            note: Some("verify".into()),
            updated_at: None,
        };
        hfs.record_terminology_state(&state).await.unwrap();
        let read = hfs.terminology_state().await.unwrap().expect("row");
        assert_eq!(read.store, state.store);
        assert_eq!(read.moved_from, state.moved_from);
        assert_eq!(
            read.moved_at.map(|t| t.timestamp_micros()),
            Some(moved_at.timestamp_micros())
        );
        assert_eq!(read.note.as_deref(), Some("verify"));
        assert!(read.updated_at.is_some());
        state.note = Some("done".into());
        hfs.record_terminology_state(&state).await.unwrap();
        assert_eq!(
            hfs.terminology_state()
                .await
                .unwrap()
                .unwrap()
                .note
                .as_deref(),
            Some("done")
        );
        let rows: i64 = admin
            .query_one("SELECT count(*) FROM public.hfs_terminology_state", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(rows, 1);

        // hm-0012: scope bindings bump the console epoch like endpoint bindings.
        async fn epoch(c: &tokio_postgres::Client) -> i64 {
            c.query_one(
                "SELECT coalesce((SELECT epoch FROM hfs_config_epochs WHERE scope = 'console'), 0)",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0)
        }
        let e0 = epoch(&admin).await;
        admin
            .batch_execute(
                "INSERT INTO hfs_console_scope_bindings (subject, scope, role) VALUES ('alice', 'terminology', 'admin')",
            )
            .await
            .unwrap();
        assert!(epoch(&admin).await > e0);
        assert!(
            admin
                .batch_execute("INSERT INTO hfs_console_scope_bindings (subject, scope, role) VALUES ('bob', 'fhir', 'admin')")
                .await
                .is_err(),
            "unknown scope refused"
        );
        admin
            .batch_execute(&format!("DROP OWNED BY {role}; DROP ROLE {role}"))
            .await
            .unwrap();
    }
}
