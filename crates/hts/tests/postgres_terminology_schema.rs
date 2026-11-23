//! One database (docs/design/unified-server.md §5.3), HTS side: the HFS
//! schema in `public`, then HTS's backend and its embedded resource store
//! opened with `search_path=terminology,public` exactly as the unified server
//! opens them. Every HTS table must land in `terminology`, `public` must be
//! unchanged, and dropping HTS tables then restarting recreates them in
//! `terminology` (the fall-through hazard: an unqualified lookup would find
//! nothing of HTS's in `public` here, but would adopt HFS's objects for the
//! embedded store's tables).
//!
//! Runs against `HTS_TEST_PG_URL` (a superuser URL; HealthyMind CI sets it),
//! in a database of its own; skipped without it. The static guard over the
//! SQL lives in `crates/persistence/tests/terminology_schema.rs`.

#![cfg(feature = "postgres")]

use helios_hts::backends::PostgresTerminologyBackend;
use helios_persistence::backends::postgres::PostgresBackend;

/// `helios_terminology::schema::TERMINOLOGY_TABLES`.
const TERMINOLOGY_TABLES: &[&str] = &[
    "code_systems",
    "concepts",
    "concept_hierarchy",
    "concept_properties",
    "concept_designations",
    "value_sets",
    "value_set_expansions",
    "value_set_dependencies",
    "concept_maps",
    "concept_map_elements",
    "concept_closure",
    "refset_members",
    "snomed_concepts",
    "snomed_relationships",
    "snomed_mrcm",
    "snomed_model_stamps",
    "bootstrap_imports",
    "terminology_epoch",
    "hts_schema_meta",
];

struct Db {
    base: String,
    name: String,
}

impl Db {
    fn url(&self) -> String {
        format!("{}/{}", self.base, self.name)
    }
    fn terminology_url(&self) -> String {
        format!(
            "{}?options=-c%20search_path%3Dterminology%2Cpublic",
            self.url()
        )
    }
    async fn client(&self) -> tokio_postgres::Client {
        let (c, conn) = tokio_postgres::connect(&self.url(), tokio_postgres::NoTls)
            .await
            .expect("connect");
        tokio::spawn(conn);
        c
    }
}

async fn fresh_db(tag: &str) -> Option<Db> {
    let admin = std::env::var("HTS_TEST_PG_URL").ok()?;
    let (base, _) = admin.rsplit_once('/')?;
    let name = format!("hts_u1_{}_{tag}", std::process::id());
    let (c, conn) = tokio_postgres::connect(&admin, tokio_postgres::NoTls)
        .await
        .expect("connect HTS_TEST_PG_URL");
    tokio::spawn(conn);
    for sql in [
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
        format!("CREATE DATABASE {name}"),
    ] {
        c.batch_execute(&sql).await.expect("create database");
    }
    Some(Db {
        base: base.to_string(),
        name,
    })
}

async fn tables_in(c: &tokio_postgres::Client, schema: &str) -> Vec<String> {
    c.query(
        "SELECT table_name::text FROM information_schema.tables WHERE table_schema = $1 ORDER BY 1",
        &[&schema],
    )
    .await
    .unwrap()
    .into_iter()
    .map(|r| r.get(0))
    .collect()
}

/// Relations, columns, constraints, triggers, functions and row counts of
/// schema `public`.
async fn public_snapshot(c: &tokio_postgres::Client) -> Vec<String> {
    let mut lines: Vec<String> = c
        .query(
            "SELECT 'rel ' || c.relname || ' ' || c.relkind::text || ' ' || coalesce(pg_get_indexdef(c.oid), '')
               FROM pg_class c WHERE c.relnamespace = 'public'::regnamespace
             UNION ALL
             SELECT 'col ' || c.relname || '.' || a.attname || ' ' || format_type(a.atttypid, a.atttypmod)
                    || ' ' || a.attnotnull || ' ' || coalesce(a.attstattarget::text, '-')
               FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid
              WHERE c.relnamespace = 'public'::regnamespace AND a.attnum > 0 AND NOT a.attisdropped
             UNION ALL
             SELECT 'con ' || conrelid::regclass::text || ' ' || conname || ' ' || pg_get_constraintdef(oid)
               FROM pg_constraint WHERE connamespace = 'public'::regnamespace
             UNION ALL
             SELECT 'trg ' || c.relname || ' ' || t.tgname
               FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid
              WHERE c.relnamespace = 'public'::regnamespace AND NOT t.tgisinternal
             UNION ALL
             SELECT 'fn ' || p.proname || ' ' || md5(p.prosrc)
               FROM pg_proc p WHERE p.pronamespace = 'public'::regnamespace
                AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.objid = p.oid AND d.deptype = 'e')",
            &[],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.get::<_, String>(0))
        .collect();
    for t in tables_in(c, "public").await {
        let n: i64 = c
            .query_one(&format!("SELECT count(*) FROM public.\"{t}\""), &[])
            .await
            .unwrap()
            .get(0);
        lines.push(format!("rows {t} {n}"));
    }
    lines.sort();
    lines
}

fn assert_same(before: &[String], after: &[String]) {
    let gone: Vec<_> = before.iter().filter(|l| !after.contains(l)).collect();
    let new: Vec<_> = after.iter().filter(|l| !before.contains(l)).collect();
    assert!(
        gone.is_empty() && new.is_empty(),
        "schema `public` changed.\nremoved: {gone:#?}\nadded: {new:#?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_integration_hts_tables_stay_in_the_terminology_schema() {
    let Some(db) = fresh_db("iso").await else {
        eprintln!("skipped: HTS_TEST_PG_URL is not set");
        return;
    };
    // HFS first, in public, as in production.
    let hfs = PostgresBackend::from_connection_string(&db.url())
        .await
        .expect("hfs backend");
    hfs.init_schema().await.expect("hfs schema");
    hfs.apply_unified_migrations("terminology")
        .await
        .expect("hm-0011/hm-0012");
    let admin = db.client().await;
    let before = public_snapshot(&admin).await;

    // HTS backend + its embedded store, both on the derived URL.
    let backend = PostgresTerminologyBackend::with_pool_size(&db.terminology_url(), 2)
        .await
        .expect("hts backend in schema terminology");
    let store = PostgresBackend::from_connection_string(&db.terminology_url())
        .await
        .expect("store");
    store.init_schema().await.expect("store schema");

    let tx_tables = tables_in(&admin, "terminology").await;
    for t in TERMINOLOGY_TABLES {
        assert!(
            tx_tables.iter().any(|x| x == t),
            "{t} not in schema terminology: {tx_tables:?}"
        );
    }
    let public_tables = tables_in(&admin, "public").await;
    for t in TERMINOLOGY_TABLES {
        assert!(
            !public_tables.iter().any(|x| x == t),
            "{t} leaked into public"
        );
    }
    assert_same(&before, &public_snapshot(&admin).await);
    // The pool really resolves to the schema.
    let schema: String = backend
        .pool()
        .get()
        .await
        .unwrap()
        .query_one("SELECT current_schema()::text", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(schema, "terminology");
    drop(backend);

    // Restart after losing tables: recreated in terminology (fingerprint row
    // gone too, so the DDL really re-runs).
    admin
        .batch_execute(
            "DROP TABLE terminology.hts_schema_meta;
             DROP TABLE terminology.concept_designations;
             DROP TABLE terminology.snomed_mrcm;",
        )
        .await
        .unwrap();
    let _backend = PostgresTerminologyBackend::with_pool_size(&db.terminology_url(), 2)
        .await
        .expect("restart");
    let again = tables_in(&admin, "terminology").await;
    for t in ["hts_schema_meta", "concept_designations", "snomed_mrcm"] {
        assert!(again.iter().any(|x| x == t), "{t} not recreated");
    }
    assert_same(&before, &public_snapshot(&admin).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_hts_creates_a_missing_schema_instead_of_using_public() {
    let Some(db) = fresh_db("mk").await else {
        eprintln!("skipped: HTS_TEST_PG_URL is not set");
        return;
    };
    let admin = db.client().await;
    let _backend = PostgresTerminologyBackend::with_pool_size(&db.terminology_url(), 2)
        .await
        .expect("hts backend");
    let tx = tables_in(&admin, "terminology").await;
    assert!(tx.iter().any(|t| t == "concepts"), "{tx:?}");
    assert!(
        !tables_in(&admin, "public")
            .await
            .iter()
            .any(|t| t == "concepts")
    );
}
