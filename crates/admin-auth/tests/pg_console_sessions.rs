//! PostgreSQL console session store and lockout (features `console` +
//! `postgres`). Runs against `HFS_TEST_PG_HOST`/`HFS_TEST_PG_PORT`
//! (superuser `postgres`/`postgres`, the HealthyMind CI service container);
//! without `HFS_TEST_PG_HOST` each test logs and returns.

use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue, header};
use helios_admin_auth::console::sessions::postgres::PgConsoleSessionStore;
use helios_admin_auth::console::sessions::{ConsoleSessionStore, KIND_PENDING, KIND_SESSION};
use helios_admin_auth::console::testing::env;
use helios_admin_auth::console::{
    ConsoleAuth, ConsoleOptions, LOCKOUT_SCOPE, LoginResult, NewUser, memory::MemoryConsoleStore,
};
use helios_admin_auth::store::postgres::PgAdminSessionStore;
use helios_admin_auth::{AdminGate, AdminSettings, CookieSecure, CookieSpec};
use helios_endpoints::console::ConsoleRole;

fn pool() -> Option<deadpool_postgres::Pool> {
    let Ok(host) = std::env::var("HFS_TEST_PG_HOST") else {
        eprintln!("HFS_TEST_PG_HOST unset; skipping");
        return None;
    };
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(host);
    cfg.port = std::env::var("HFS_TEST_PG_PORT")
        .ok()
        .and_then(|p| p.parse().ok());
    cfg.user = Some("postgres".into());
    cfg.password = Some("postgres".into());
    cfg.dbname = Some("postgres".into());
    Some(
        cfg.create_pool(
            Some(deadpool_postgres::Runtime::Tokio1),
            deadpool_postgres::tokio_postgres::NoTls,
        )
        .expect("pool"),
    )
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_integration_console_session_store() {
    let Some(pool) = pool() else { return };
    let store = PgConsoleSessionStore::connect(pool.clone()).await.unwrap();
    let key = format!("k{}", now()) + &uuid_like();
    let subject = format!("local:{key}");
    let doc = serde_json::json!({"subject": subject, "role": "viewer"});
    store
        .put(&key, KIND_SESSION, &doc, now() + 60)
        .await
        .unwrap();
    assert_eq!(
        store.get(&key, KIND_SESSION).await.unwrap(),
        Some(doc.clone())
    );
    assert!(
        store.get(&key, KIND_PENDING).await.unwrap().is_none(),
        "kind scoped"
    );
    // Replace (touch) keeps one row.
    let doc2 = serde_json::json!({"subject": subject, "role": "admin"});
    store
        .put(&key, KIND_SESSION, &doc2, now() + 60)
        .await
        .unwrap();
    assert_eq!(store.get(&key, KIND_SESSION).await.unwrap(), Some(doc2));
    assert!(
        store
            .list(KIND_SESSION)
            .await
            .unwrap()
            .iter()
            .any(|(k, _)| *k == key)
    );
    // Expired rows are invisible.
    let old = format!("{key}-old");
    store
        .put(&old, KIND_SESSION, &doc, now() - 5)
        .await
        .unwrap();
    assert!(store.get(&old, KIND_SESSION).await.unwrap().is_none());
    // Subject revocation and exactly-once delete.
    assert_eq!(store.delete_subject(&subject).await.unwrap(), 2);
    let p = format!("{key}-p");
    store.put(&p, KIND_PENDING, &doc, now() + 60).await.unwrap();
    assert!(store.delete(&p, KIND_PENDING).await.unwrap());
    assert!(!store.delete(&p, KIND_PENDING).await.unwrap());
    // The row id is prefixed so it never collides with upstream sessions.
    let c = pool.get().await.unwrap();
    let n: i64 = c
        .query_one("SELECT COUNT(*) FROM login_sessions WHERE id = $1", &[&key])
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 0);
}

fn uuid_like() -> String {
    use rand::Rng;
    let mut r = rand::thread_rng();
    (0..12)
        .map(|_| char::from(b'a' + r.gen_range(0..26)))
        .collect()
}

async fn instance(
    pool: &deadpool_postgres::Pool,
    store: Arc<MemoryConsoleStore>,
) -> Arc<ConsoleAuth> {
    let gate = AdminGate::configure(
        "HFS",
        AdminSettings::default().with_password("break-glass-password-1"),
        true,
        CookieSpec {
            name: "hfs_admin",
            path: "/",
        },
    );
    ConsoleAuth::new(
        env(&[
            ("HFS_CONSOLE_AUTH_MODE", "local"),
            ("HFS_CONSOLE_LOCKOUT_FAILURES", "3"),
        ]),
        store,
        Arc::new(PgConsoleSessionStore::connect(pool.clone()).await.unwrap()),
        Arc::new(
            PgAdminSessionStore::connect(pool.clone(), LOCKOUT_SCOPE)
                .await
                .unwrap(),
        ),
        gate,
        ConsoleOptions {
            public_base_url: "http://hfs.test".into(),
            cookie_secure: CookieSecure::Never,
            recheck: Duration::from_millis(0),
            persistent: true,
            ..Default::default()
        },
    )
    .await
}

fn site() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(header::HOST, HeaderValue::from_static("hfs.test"));
    h.insert(header::ORIGIN, HeaderValue::from_static("http://hfs.test"));
    h
}

/// Two instances on one database: a session made on A works on B, a
/// sign-out on B ends it on A, and failed attempts on both share one
/// lockout.
#[tokio::test(flavor = "multi_thread")]
async fn postgres_integration_console_sessions_are_cluster_wide() {
    let Some(pool) = pool() else { return };
    // A per-run client address: the lockout table outlives the test.
    let peer = Some(std::net::IpAddr::from([198, 51, 100, rand::random::<u8>()]));
    let accounts = Arc::new(MemoryConsoleStore::new());
    let a = instance(&pool, accounts.clone()).await;
    let b = instance(&pool, accounts.clone()).await;
    let name = format!("u{}", uuid_like());
    let (u, _) = a
        .create_user(
            "t",
            NewUser {
                username: name.clone(),
                role: Some(ConsoleRole::Editor),
                password: Some("tidy-Lantern-41-orbit".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut u2 = u.clone();
    u2.must_change_password = false;
    u2.password_hash = None;
    helios_endpoints::console::ConsoleStore::update_user(accounts.as_ref(), u2, u.revision)
        .await
        .unwrap();
    let LoginResult::Success { cookies, .. } = a
        .login_password(&site(), peer, &name, "tidy-Lantern-41-orbit")
        .await
    else {
        panic!("sign-in")
    };
    let cookie: Vec<&str> = cookies
        .iter()
        .filter_map(|c| c.to_str().ok())
        .filter_map(|c| c.split(';').next())
        .collect();
    let mut h = site();
    h.insert(
        header::COOKIE,
        HeaderValue::from_str(&cookie.join("; ")).unwrap(),
    );
    assert_eq!(
        b.resolve(&h).await.unwrap().principal.role,
        ConsoleRole::Editor
    );
    let _ = b.logout(&h).await;
    assert!(a.resolve(&h).await.is_none(), "signed out everywhere");

    // Lockout shared: 2 failures on A + 1 on B lock the account on both.
    for rt in [&a, &a] {
        assert!(matches!(
            rt.login_password(&site(), peer, &name, "x").await,
            LoginResult::Invalid
        ));
    }
    assert!(matches!(
        b.login_password(&site(), peer, &name, "x").await,
        LoginResult::RateLimited(_)
    ));
    assert!(matches!(
        a.login_password(&site(), peer, &name, "tidy-Lantern-41-orbit")
            .await,
        LoginResult::RateLimited(_)
    ));
}
