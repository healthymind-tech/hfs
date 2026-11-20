//! The PostgreSQL admin session store (multi-instance sign-in), checked
//! scenario by scenario against the in-memory reference store so both keep
//! the same semantics: sessions (put / get / touch / delete, last-seen only
//! moving forward), product scoping (HFS and HTS share one database), and
//! failed-attempt lockout (count, window reset, lockout end, clear).
//!
//! Runs against `HFS_TEST_PG_HOST` / `HFS_TEST_PG_PORT` (postgres/postgres),
//! in a database created for this run; skipped when they are unset.

#![cfg(feature = "postgres")]

use deadpool_postgres::{Config, Pool, Runtime, tokio_postgres::NoTls};
use helios_admin_auth::store::postgres::PgAdminSessionStore;
use helios_admin_auth::store::testing::MemoryStore;
use helios_admin_auth::store::{AdminSessionStore, StoredSession};

async fn pool(name: &str) -> Option<Pool> {
    let host = std::env::var("HFS_TEST_PG_HOST").ok()?;
    let port: u16 = std::env::var("HFS_TEST_PG_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5432);
    let mk = |db: &str| {
        let mut c = Config::new();
        c.host = Some(host.clone());
        c.port = Some(port);
        c.user = Some("postgres".into());
        c.password = Some("postgres".into());
        c.dbname = Some(db.into());
        c.create_pool(Some(Runtime::Tokio1), NoTls).expect("pool")
    };
    let db = format!("hm_admin_{}_{name}", std::process::id());
    let admin = mk("postgres")
        .get()
        .await
        .expect("connect to HFS_TEST_PG_HOST");
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"))
        .await
        .unwrap();
    admin
        .batch_execute(&format!("CREATE DATABASE {db}"))
        .await
        .unwrap();
    Some(mk(&db))
}

macro_rules! stores_or_skip {
    ($name:expr, $scope:expr) => {
        match pool($name).await {
            Some(p) => (
                PgAdminSessionStore::connect(p.clone(), $scope)
                    .await
                    .expect("connect creates the tables"),
                p,
            ),
            None => {
                eprintln!("skipped: HFS_TEST_PG_HOST is not set");
                return;
            }
        }
    };
}

fn session(user: &str, created: i64, seen: i64) -> StoredSession {
    StoredSession {
        user: user.into(),
        created_unix: created,
        last_seen_unix: seen,
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// The session scenario, returning what each step observed.
async fn session_scenario(s: &dyn AdminSessionStore) -> Vec<Option<StoredSession>> {
    let t = now();
    let mut seen = Vec::new();
    seen.push(s.get("k1").await.unwrap());
    s.put("k1", &session("admin", t, t)).await.unwrap();
    seen.push(s.get("k1").await.unwrap());
    // touch moves last_seen forward only.
    s.touch("k1", t + 10).await.unwrap();
    s.touch("k1", t + 5).await.unwrap();
    seen.push(s.get("k1").await.unwrap());
    // Touching an unknown key creates nothing.
    s.touch("nobody", t).await.unwrap();
    seen.push(s.get("nobody").await.unwrap());
    s.delete("k1").await.unwrap();
    seen.push(s.get("k1").await.unwrap());
    // Deleting twice is fine.
    s.delete("k1").await.unwrap();
    seen
}

#[tokio::test]
async fn sessions_behave_like_the_reference_store() {
    let (pg, _pool) = stores_or_skip!("sessions", "HFS");
    let mem = MemoryStore::default();
    let expected = session_scenario(&mem).await;
    let actual = session_scenario(&pg).await;
    assert_eq!(actual, expected);
    assert_eq!(
        expected[2].as_ref().unwrap().last_seen_unix - expected[1].as_ref().unwrap().last_seen_unix,
        10
    );
    // PostgreSQL only: re-putting a key (two instances racing on one
    // sign-in) keeps the later last_seen.
    let t = now();
    pg.put("k2", &session("admin", t, t + 9)).await.unwrap();
    pg.put("k2", &session("admin", t, t + 1)).await.unwrap();
    assert_eq!(pg.get("k2").await.unwrap().unwrap().last_seen_unix, t + 9);
}

#[tokio::test]
async fn products_sharing_a_database_do_not_see_each_others_sessions() {
    let (hfs, pool) = stores_or_skip!("scope", "HFS");
    let hts = PgAdminSessionStore::connect(pool.clone(), "HTS")
        .await
        .expect("second scope on the same tables");
    let t = now();
    hfs.put("same-digest", &session("hfs-admin", t, t))
        .await
        .unwrap();
    assert!(hts.get("same-digest").await.unwrap().is_none());
    hts.put("same-digest", &session("hts-admin", t, t))
        .await
        .unwrap();
    assert_eq!(
        hfs.get("same-digest").await.unwrap().unwrap().user,
        "hfs-admin"
    );
    assert_eq!(
        hts.get("same-digest").await.unwrap().unwrap().user,
        "hts-admin"
    );
    hts.delete("same-digest").await.unwrap();
    assert!(hfs.get("same-digest").await.unwrap().is_some());

    // Lockouts are per product too.
    assert!(
        hfs.record_failure("10.0.0.1", 300, 1, 60)
            .await
            .unwrap()
            .is_some()
    );
    assert!(hts.locked_until("10.0.0.1").await.unwrap().is_none());
    // Connecting again (another instance starting) keeps the data.
    let again = PgAdminSessionStore::connect(pool, "HFS").await.unwrap();
    assert!(again.get("same-digest").await.unwrap().is_some());
    assert!(again.locked_until("10.0.0.1").await.unwrap().is_some());
}

#[tokio::test]
async fn sessions_past_twice_the_absolute_lifetime_are_pruned_on_put() {
    let (pg, _pool) = stores_or_skip!("prune", "HFS");
    let t = now();
    let ancient = t - 10 * 365 * 24 * 3600;
    pg.put("old", &session("admin", ancient, ancient))
        .await
        .unwrap();
    assert!(pg.get("old").await.unwrap().is_some());
    pg.put("new", &session("admin", t, t)).await.unwrap();
    assert!(
        pg.get("old").await.unwrap().is_none(),
        "pruned by the next put"
    );
    assert!(pg.get("new").await.unwrap().is_some());
}

/// The lockout scenario: `(locked after this failure?, locked_until?)`.
async fn lockout_scenario(s: &dyn AdminSessionStore) -> Vec<(bool, bool)> {
    let mut out = Vec::new();
    let mut step = |r: Option<i64>, l: Option<i64>| out.push((r.is_some(), l.is_some()));
    // Three attempts allowed in a long window; the third locks.
    for _ in 0..3 {
        let r = s.record_failure("c1", 3600, 3, 600).await.unwrap();
        let l = s.locked_until("c1").await.unwrap();
        step(r, l);
    }
    // Clearing (a successful sign-in) unlocks and resets the count.
    s.clear_failures("c1").await.unwrap();
    let l = s.locked_until("c1").await.unwrap();
    step(None, l);
    let r = s.record_failure("c1", 3600, 3, 600).await.unwrap();
    step(r, s.locked_until("c1").await.unwrap());
    // A zero-second window resets on every failure: never reaches 2.
    for _ in 0..3 {
        let r = s.record_failure("c2", 0, 2, 600).await.unwrap();
        step(r, s.locked_until("c2").await.unwrap());
    }
    // A zero-second lockout is over at once.
    let r = s.record_failure("c3", 3600, 1, 0).await.unwrap();
    step(r, s.locked_until("c3").await.unwrap());
    // Unknown clients are not locked.
    step(None, s.locked_until("never-failed").await.unwrap());
    out
}

#[tokio::test]
async fn lockout_behaves_like_the_reference_store() {
    let (pg, _pool) = stores_or_skip!("lockout", "HFS");
    let mem = MemoryStore::default();
    let expected = lockout_scenario(&mem).await;
    assert_eq!(
        expected,
        [
            (false, false),
            (false, false),
            (true, true),
            (false, false),
            (false, false),
            (false, false),
            (false, false),
            (false, false),
            (false, false),
            (false, false),
        ]
    );
    assert_eq!(lockout_scenario(&pg).await, expected);
}

#[tokio::test]
async fn the_lockout_ends_after_its_duration() {
    let (pg, _pool) = stores_or_skip!("lockend", "HFS");
    let until = pg
        .record_failure("c", 3600, 1, 2)
        .await
        .unwrap()
        .expect("locked");
    // The database's clock decides; allow a second of skew and rounding
    // either way against this process's clock.
    let t = now();
    assert!(until >= t && until <= t + 3, "until {until}, now {t}");
    std::thread::sleep(std::time::Duration::from_millis(3100));
    assert!(pg.locked_until("c").await.unwrap().is_none());
    // The next failure in the same window locks again (max 1).
    assert!(pg.record_failure("c", 3600, 1, 2).await.unwrap().is_some());
}
