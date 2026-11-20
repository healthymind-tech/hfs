//! PostgreSQL `ConsoleStore` integration tests (package P3).
//!
//! Run against an existing PostgreSQL 16 (superuser `postgres`/`postgres`)
//! named by `HFS_TEST_PG_HOST` / `HFS_TEST_PG_PORT` — the HealthyMind CI
//! service container. Without `HFS_TEST_PG_HOST` each test logs and returns.
//! Every test gets a fresh database migrated by the real persistence schema
//! (fork migrations `hm-0008`/`hm-0009` included).

use chrono::Utc;
use helios_endpoints::console::{
    ConsoleAuthMode, ConsoleAuthSettings, ConsoleRole, ConsoleStore, ConsoleStoreError,
    ConsoleUser,
};
use helios_endpoints::pg_console::PgConsoleStore;
use helios_persistence::backends::postgres::{PostgresBackend, PostgresConfig};

async fn fresh_pool(prefix: &str) -> Option<deadpool_postgres::Pool> {
    let Ok(host) = std::env::var("HFS_TEST_PG_HOST") else {
        eprintln!("HFS_TEST_PG_HOST unset; skipping {prefix}");
        return None;
    };
    let port: u16 = std::env::var("HFS_TEST_PG_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5432);
    let dbname = format!("{prefix}_{}", uuid::Uuid::new_v4().simple());
    let (admin, conn) = tokio_postgres::Config::new()
        .host(&host)
        .port(port)
        .user("postgres")
        .password("postgres")
        .dbname("postgres")
        .connect(tokio_postgres::NoTls)
        .await
        .expect("connect to the test server");
    tokio::spawn(conn);
    admin
        .batch_execute(&format!("CREATE DATABASE {dbname}"))
        .await
        .expect("create database");
    let backend = PostgresBackend::new(PostgresConfig {
        host,
        port,
        dbname,
        user: "postgres".into(),
        password: Some("postgres".into()),
        max_connections: 4,
        ..Default::default()
    })
    .await
    .expect("backend");
    backend.init_schema().await.expect("migrations");
    Some(backend.shared_pool())
}

fn user(name: &str, role: ConsoleRole) -> ConsoleUser {
    ConsoleUser {
        id: String::new(),
        username: name.into(),
        display_name: Some(format!("{name} display")),
        email: None,
        role,
        disabled: false,
        must_change_password: true,
        password_hash: Some("$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaA".into()),
        revision: 0,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        last_login_at: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_integration_console_store_settings_cas() {
    let Some(pool) = fresh_pool("p3_settings").await else {
        return;
    };
    let store = PgConsoleStore::new(pool);
    assert!(store.settings().await.unwrap().is_none());
    let mut s = ConsoleAuthSettings {
        mode: ConsoleAuthMode::Local,
        ..Default::default()
    };
    let r1 = store.save_settings(&s, 0, "admin").await.unwrap();
    assert_eq!(r1, 1);
    // A second "first save" conflicts.
    assert_eq!(
        store.save_settings(&s, 0, "admin").await,
        Err(ConsoleStoreError::Conflict { current: 1 })
    );
    s.session_idle_minutes = 30;
    let r2 = store.save_settings(&s, r1, "admin").await.unwrap();
    assert_eq!(r2, 2);
    assert_eq!(
        store.save_settings(&s, r1, "admin").await,
        Err(ConsoleStoreError::Conflict { current: 2 })
    );
    let (loaded, rev) = store.settings().await.unwrap().unwrap();
    assert_eq!(rev, 2);
    assert_eq!(loaded, s);
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_integration_console_store_users_and_last_admin() {
    let Some(pool) = fresh_pool("p3_users").await else {
        return;
    };
    let store = PgConsoleStore::new(pool.clone());
    let alice = store
        .create_user(user("Alice", ConsoleRole::Admin))
        .await
        .unwrap();
    assert!(!alice.id.is_empty());
    assert_eq!(alice.revision, 1);
    assert_eq!(
        store.create_user(user("alice", ConsoleRole::Viewer)).await,
        Err(ConsoleStoreError::DuplicateUsername("alice".into()))
    );
    assert_eq!(
        store.user_by_name("ALICE").await.unwrap().unwrap().id,
        alice.id
    );
    assert_eq!(
        store.user_by_id(&alice.id).await.unwrap().unwrap().username,
        "Alice"
    );

    // Last enabled admin: cannot demote, disable or delete.
    let mut demoted = alice.clone();
    demoted.role = ConsoleRole::Viewer;
    demoted.password_hash = None;
    assert!(matches!(
        store.update_user(demoted.clone(), 1).await,
        Err(ConsoleStoreError::Forbidden(_))
    ));
    let mut disabled = alice.clone();
    disabled.disabled = true;
    assert!(matches!(
        store.update_user(disabled, 1).await,
        Err(ConsoleStoreError::Forbidden(_))
    ));
    assert!(matches!(
        store.delete_user(&alice.id).await,
        Err(ConsoleStoreError::Forbidden(_))
    ));

    // With a second admin, demotion works; revision CAS is enforced; a
    // `None` hash keeps the stored one.
    let bob = store
        .create_user(user("bob", ConsoleRole::Admin))
        .await
        .unwrap();
    let updated = store.update_user(demoted.clone(), 1).await.unwrap();
    assert_eq!(updated.role, ConsoleRole::Viewer);
    assert_eq!(updated.revision, 2);
    assert!(updated.password_hash.is_some());
    assert_eq!(
        store.update_user(demoted, 1).await,
        Err(ConsoleStoreError::Conflict { current: 2 })
    );
    // Now bob is the last admin.
    assert!(matches!(
        store.delete_user(&bob.id).await,
        Err(ConsoleStoreError::Forbidden(_))
    ));
    store.delete_user(&alice.id).await.unwrap();
    assert_eq!(
        store.delete_user(&alice.id).await,
        Err(ConsoleStoreError::NotFound)
    );

    store.record_login(&bob.id, Utc::now()).await.unwrap();
    let bob2 = store.user_by_id(&bob.id).await.unwrap().unwrap();
    assert!(bob2.last_login_at.is_some());
    assert_eq!(bob2.revision, 1, "sign-in is not an edit");
    assert_eq!(store.list_users().await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_integration_console_store_role_bindings() {
    let Some(pool) = fresh_pool("p3_bindings").await else {
        return;
    };
    {
        let client = pool.get().await.unwrap();
        client
            .execute(
                "INSERT INTO hfs_endpoints (id, name, path, tenant_id, status, managed_by, spec)
                 VALUES ('e1', 'lab', 'lab', 'lab', 'enabled', 'ui', '{}'::jsonb)",
                &[],
            )
            .await
            .expect("insert endpoint");
    }
    let store = PgConsoleStore::new(pool.clone());
    store
        .set_role_binding("local:bob", "lab", ConsoleRole::Editor, "admin")
        .await
        .unwrap();
    assert_eq!(
        store.set_role_binding("local:bob", "nope", ConsoleRole::Editor, "admin").await,
        Err(ConsoleStoreError::NotFound)
    );
    let roles = store.endpoint_roles("local:bob").await.unwrap();
    assert_eq!(roles.get("lab"), Some(&ConsoleRole::Editor));
    store
        .set_role_binding("local:bob", "lab", ConsoleRole::Admin, "admin")
        .await
        .unwrap();
    let all = store.list_role_bindings().await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].role, ConsoleRole::Admin);
    store.delete_role_binding("local:bob", "lab").await.unwrap();
    assert_eq!(
        store.delete_role_binding("local:bob", "lab").await,
        Err(ConsoleStoreError::NotFound)
    );
    assert!(store.endpoint_roles("local:bob").await.unwrap().is_empty());
}
