//! PostgreSQL `IdpStore` integration tests (package I2).
//!
//! Run against an existing PostgreSQL 16 (superuser `postgres`/`postgres`)
//! named by `HFS_TEST_PG_HOST` / `HFS_TEST_PG_PORT` — the HealthyMind CI
//! service container. Without `HFS_TEST_PG_HOST` each test logs and returns.
//! Every test gets a fresh database migrated by the real persistence schema
//! (fork migration `hm-0020-idp-core` included).

use std::sync::Arc;

use chrono::{Duration, Utc};
use helios_idp::client::{ClientAuth, ClientSpec, GrantType, LaunchType};
use helios_idp::interaction::{InteractionNeeds, PendingAuthorization, ResponseMode};
use helios_idp::keys::{KeyRecord, KeyStatus, RsaBits, SigningAlg, SigningKey};
use helios_idp::launch::{LaunchContext, LaunchHandle};
use helios_idp::pg::PgStore;
use helios_idp::realm::{RealmName, RealmSpec};
use helios_idp::scope::ScopePolicy;
use helios_idp::store::*;
use helios_idp::token::RefreshFamily;
use helios_idp::user::{FhirLink, RealmUser, UserKind};
use helios_persistence::backends::postgres::{PostgresBackend, PostgresConfig};

async fn fresh(prefix: &str) -> Option<(deadpool_postgres::Pool, PostgresBackend)> {
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
        max_connections: 16,
        ..Default::default()
    })
    .await
    .expect("backend");
    backend.init_schema().await.expect("migrations");
    Some((backend.shared_pool(), backend))
}

async fn store_with_realm(prefix: &str) -> Option<(PgStore, String, PostgresBackend)> {
    let (pool, backend) = fresh(prefix).await?;
    let store = PgStore::new(pool);
    let rec = store
        .put_realm(
            RealmRecord {
                id: "r1".into(),
                spec: RealmSpec::new(RealmName::parse("clinic").unwrap(), "Clinic"),
                revision: 0,
            },
            None,
        )
        .await
        .unwrap();
    Some((store, rec.id, backend))
}

#[test]
fn migration_sql_is_the_persistence_copy() {
    assert_eq!(
        helios_persistence::backends::postgres::FORK_IDP_CORE_SQL,
        helios_idp::schema::IDP_CORE_SQL
    );
    assert_eq!(
        helios_persistence::backends::postgres::FORK_IDP_CORE,
        helios_idp::schema::FORK_IDP_CORE
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_integration_idp_migration_is_idempotent_and_epoch_moves() {
    let Some((pool, backend)) = fresh("idp_mig").await else {
        return;
    };
    let c = pool.get().await.unwrap();
    // Running the body again changes nothing.
    c.batch_execute(helios_idp::schema::IDP_CORE_SQL)
        .await
        .unwrap();
    backend.init_schema().await.unwrap();
    let n: i64 = c
        .query_one(
            "SELECT count(*) FROM hm_schema_migrations WHERE name = 'hm-0020-idp-core'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 1);
    let store = PgStore::new(pool.clone());
    let e0 = store.config_epoch().await.unwrap().unwrap();
    store
        .put_realm(
            RealmRecord {
                id: "r1".into(),
                spec: RealmSpec::new(RealmName::parse("a").unwrap(), "A"),
                revision: 0,
            },
            None,
        )
        .await
        .unwrap();
    let e1 = store.config_epoch().await.unwrap().unwrap();
    assert!(e1 > e0, "realm writes bump the idp epoch");
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_integration_idp_realm_client_user_roundtrip() {
    let Some((store, rid, _b)) = store_with_realm("idp_crud").await else {
        return;
    };
    // Realm CAS + name uniqueness.
    let r = store.get_realm_by_name("clinic").await.unwrap().unwrap();
    assert_eq!(r.revision, 1);
    let mut spec = r.spec.clone();
    spec.display_name = "Clinic 2".into();
    let r2 = store
        .put_realm(
            RealmRecord {
                id: rid.clone(),
                spec: spec.clone(),
                revision: 1,
            },
            Some(1),
        )
        .await
        .unwrap();
    assert_eq!(r2.revision, 2);
    assert!(matches!(
        store
            .put_realm(
                RealmRecord {
                    id: rid.clone(),
                    spec: spec.clone(),
                    revision: 1
                },
                Some(1)
            )
            .await,
        Err(helios_idp::IdpError::Conflict(_))
    ));
    assert!(
        store
            .put_realm(
                RealmRecord {
                    id: "r2".into(),
                    spec: spec.clone(),
                    revision: 0
                },
                None
            )
            .await
            .is_err(),
        "name taken"
    );

    // Keys: one active per alg.
    let k = SigningKey::generate(SigningAlg::ES256, RsaBits::B2048).unwrap();
    let ring = helios_endpoints::SecretKeyRing::parse(
        &helios_endpoints::SecretKeyRing::generate_spec("t1"),
    )
    .unwrap();
    let rec = KeyRecord {
        kid: k.kid().into(),
        alg: SigningAlg::ES256,
        status: KeyStatus::Active,
        public_jwk: k.public_jwk().clone(),
        created_at: Utc::now(),
        activate_at: Some(Utc::now()),
        retire_at: None,
        expire_at: None,
    };
    store
        .insert_key(&rid, rec.clone(), k.seal(&ring, &rid))
        .await
        .unwrap();
    let k2 = SigningKey::generate(SigningAlg::ES256, RsaBits::B2048).unwrap();
    let mut rec2 = rec.clone();
    rec2.kid = k2.kid().into();
    rec2.public_jwk = k2.public_jwk().clone();
    assert!(
        store
            .insert_key(&rid, rec2.clone(), k2.seal(&ring, &rid))
            .await
            .is_err(),
        "second active ES256 key refused by the partial unique index"
    );
    let sealed = store.get_sealed_key(&rid, k.kid()).await.unwrap().unwrap();
    let opened = SigningKey::open(&ring, &sealed, &rid, k.kid(), SigningAlg::ES256).unwrap();
    assert_eq!(opened.kid(), k.kid());
    assert!(
        store
            .set_key_status(
                &rid,
                k.kid(),
                KeyStatus::Active,
                KeyStatus::Retiring,
                Utc::now(),
                Some(Utc::now() + Duration::hours(1))
            )
            .await
            .unwrap()
    );
    assert!(
        !store
            .set_key_status(
                &rid,
                k.kid(),
                KeyStatus::Active,
                KeyStatus::Retiring,
                Utc::now(),
                None
            )
            .await
            .unwrap(),
        "CAS on from-status"
    );
    let keys = store.list_keys(&rid).await.unwrap();
    assert_eq!(keys[0].status, KeyStatus::Retiring);
    assert!(keys[0].retire_at.is_some() && keys[0].expire_at.is_some());

    // Clients.
    let spec = ClientSpec {
        client_id: "app".into(),
        name: "App".into(),
        auth: ClientAuth::None,
        grant_types: vec![GrantType::AuthorizationCode],
        redirect_uris: vec!["https://app.example/cb".into()],
        post_logout_redirect_uris: vec![],
        launch_types: vec![LaunchType::Standalone],
        scopes: ScopePolicy::default(),
        trusted: false,
        require_dpop: false,
        require_par: false,
        backchannel_logout_uri: None,
        access_token_ttl_secs: None,
        disabled: false,
    };
    let c = store
        .put_client(
            &rid,
            ClientRecord {
                spec: spec.clone(),
                registration: Registration::Admin,
                revision: 0,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(c.revision, 1);
    assert!(store.put_client(&rid, c.clone(), None).await.is_err());
    let c2 = store.put_client(&rid, c.clone(), Some(1)).await.unwrap();
    assert_eq!(c2.revision, 2);
    assert_eq!(store.get_client(&rid, "app").await.unwrap(), Some(spec));
    assert!(store.get_client("other", "app").await.unwrap().is_none());

    // Users + links + password.
    let u = RealmUser {
        id: "u1".into(),
        username: "Alice".into(),
        kind: UserKind::Patient,
        display_name: Some("Alice A".into()),
        email: Some("a@example.org".into()),
        email_verified: true,
        links: vec![FhirLink {
            endpoint: "clinic".into(),
            reference: "Patient/p1".into(),
            is_fhir_user: true,
            accessible_patients: vec!["child".into()],
        }],
        external_subject: None,
        disabled: false,
    };
    store.put_user(&rid, u.clone()).await.unwrap();
    assert_eq!(
        store.find_user_by_username(&rid, "alice").await.unwrap(),
        Some(u.clone())
    );
    let mut dup = u.clone();
    dup.id = "u2".into();
    dup.username = "ALICE".into();
    dup.links.clear();
    assert!(
        store.put_user(&rid, dup).await.is_err(),
        "case-insensitive unique"
    );
    store
        .set_password_hash(&rid, "u1", "$argon2id$x", true)
        .await
        .unwrap();
    store
        .set_password_hash(&rid, "u1", "$argon2id$y", false)
        .await
        .unwrap();
    assert_eq!(
        store.get_password_hash(&rid, "u1").await.unwrap(),
        Some(("$argon2id$y".into(), false))
    );

    // Sessions + consents.
    let now = Utc::now();
    let s = SsoSession {
        id_hash: "sh".into(),
        realm_id: rid.clone(),
        user_id: "u1".into(),
        sid: "sid1".into(),
        auth_time: now,
        amr: vec!["pwd".into()],
        acr: None,
        created_at: now,
        last_seen_at: now,
        expires_at: now + Duration::hours(1),
        revoked_at: None,
    };
    store.put_session(s.clone()).await.unwrap();
    assert_eq!(
        store
            .get_session_by_sid(&rid, "sid1")
            .await
            .unwrap()
            .unwrap()
            .user_id,
        "u1"
    );
    assert!(store.revoke_session(&rid, "sh", now).await.unwrap());
    assert!(!store.revoke_session(&rid, "sh", now).await.unwrap());
    store
        .put_consent(Consent {
            realm_id: rid.clone(),
            user_id: "u1".into(),
            client_id: "app".into(),
            endpoint: "clinic".into(),
            scopes: vec!["patient/*.rs".into()],
            granted_at: now,
            expires_at: None,
        })
        .await
        .unwrap();
    assert_eq!(store.list_consents(&rid, "u1").await.unwrap().len(), 1);
    assert!(store.delete_user(&rid, "u1").await.unwrap());
    assert!(store.list_consents(&rid, "u1").await.unwrap().is_empty());
}

fn code(rid: &str, hash: &str, gid: &str, exp: chrono::DateTime<Utc>) -> AuthorizationCode {
    AuthorizationCode {
        code_hash: hash.into(),
        realm_id: rid.into(),
        grant_id: gid.into(),
        client_id: "app".into(),
        redirect_uri: "https://app.example/cb".into(),
        code_challenge: "c".into(),
        subject: "u1".into(),
        scopes: vec!["openid".into()],
        audience: vec!["https://h/fhir".into()],
        endpoint: Some("clinic".into()),
        nonce: None,
        auth_time: Utc::now(),
        sso_session_id: None,
        sid: None,
        amr: vec![],
        acr: None,
        fhir_user: None,
        context: LaunchContext::default(),
        dpop_jkt: None,
        expires_at: exp,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_integration_idp_code_consumed_exactly_once_under_concurrency() {
    let Some((store, rid, _b)) = store_with_realm("idp_code").await else {
        return;
    };
    let store = Arc::new(store);
    let now = Utc::now();
    store
        .put_code(code(&rid, "h1", "g1", now + Duration::seconds(60)))
        .await
        .unwrap();
    let mut tasks = vec![];
    for _ in 0..100 {
        let s = store.clone();
        let rid = rid.clone();
        tasks.push(tokio::spawn(async move {
            s.take_code(&rid, "h1", Utc::now()).await.unwrap()
        }));
    }
    let mut fresh = 0;
    let mut replayed = 0;
    for t in tasks {
        match t.await.unwrap() {
            CodeTake::Fresh(c) => {
                assert_eq!(c.grant_id, "g1");
                fresh += 1
            }
            CodeTake::Replayed { grant_id } => {
                assert_eq!(grant_id, "g1");
                replayed += 1
            }
            CodeTake::Unknown => {}
        }
    }
    assert_eq!(fresh, 1, "exactly one redemption wins");
    assert!(
        replayed >= 90,
        "later presentations are seen as replays ({replayed})"
    );
    // Other realm never sees it; expired code is unknown.
    assert_eq!(
        store.take_code("other", "h1", now).await.unwrap(),
        CodeTake::Unknown
    );
    store
        .put_code(code(&rid, "h2", "g2", now - Duration::seconds(1)))
        .await
        .unwrap();
    assert_eq!(
        store.take_code(&rid, "h2", now).await.unwrap(),
        CodeTake::Unknown
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_integration_idp_refresh_cas_race_and_family_revocation() {
    let Some((store, rid, _b)) = store_with_realm("idp_rt").await else {
        return;
    };
    let store = Arc::new(store);
    let now = Utc::now();
    store
        .put_grant(GrantRecord {
            id: "g1".into(),
            realm_id: rid.clone(),
            client_id: "app".into(),
            subject: "u1".into(),
            scopes: vec!["offline_access".into()],
            audience: vec![],
            context: LaunchContext {
                patient: Some("p1".into()),
                ..Default::default()
            },
            fhir_user: None,
            sso_session_id: None,
            auth_time: Some(now),
            created_at: now,
            expires_at: now + Duration::hours(1),
            revoked_at: None,
            revoked_reason: None,
        })
        .await
        .unwrap();
    let fam = RefreshFamily {
        family_id: "f1".into(),
        grant_id: "g1".into(),
        client_id: "app".into(),
        subject: "u1".into(),
        scopes: vec!["offline_access".into()],
        audience: vec![],
        endpoint: None,
        fhir_user: None,
        auth_time: None,
        context: LaunchContext::default(),
        current_generation: 0,
        created_at: now,
        last_used_at: now,
        absolute_expiry: now + Duration::days(1),
        sso_session_id: None,
        revoked_at: None,
        revoked_reason: None,
    };
    store.create_family(&rid, fam, "t0").await.unwrap();
    let (f, g) = store
        .get_family_by_token(&rid, "t0")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((f.family_id.as_str(), g), ("f1", 0));
    // 50 concurrent rotations from generation 0: exactly one wins.
    let mut tasks = vec![];
    for i in 0..50 {
        let s = store.clone();
        let rid = rid.clone();
        tasks.push(tokio::spawn(async move {
            s.rotate_family(&rid, "f1", 0, &format!("t1-{i}"), Utc::now())
                .await
                .unwrap()
        }));
    }
    let mut wins = 0;
    for t in tasks {
        if t.await.unwrap() {
            wins += 1;
        }
    }
    assert_eq!(wins, 1);
    let (f, g) = store
        .get_family_by_token(&rid, "t0")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (f.current_generation, g),
        (1, 0),
        "old token is generation 0 of a gen-1 family"
    );
    assert!(
        store
            .get_family_by_token("other", "t0")
            .await
            .unwrap()
            .is_none()
    );
    // Revoking the grant revokes the family and lands in the deny set.
    assert!(store.revoke_grant(&rid, "g1", "test", now).await.unwrap());
    let (f, _) = store
        .get_family_by_token(&rid, "t0")
        .await
        .unwrap()
        .unwrap();
    assert!(f.revoked_at.is_some());
    assert!(!store.rotate_family(&rid, "f1", 1, "t2", now).await.unwrap());
    assert_eq!(
        store.revoked_grant_ids(&rid, now).await.unwrap(),
        vec!["g1".to_string()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_integration_idp_jti_launch_interaction_and_purge() {
    let Some((store, rid, _b)) = store_with_realm("idp_once").await else {
        return;
    };
    let now = Utc::now();
    let exp = now + Duration::minutes(5);
    assert!(store.record_jti(&rid, "app", "j1", exp, now).await.unwrap());
    assert!(!store.record_jti(&rid, "app", "j1", exp, now).await.unwrap());
    assert!(
        store
            .record_jti(&rid, "other", "j1", exp, now)
            .await
            .unwrap()
    );
    // After expiry the jti can be reused (the row is reclaimed).
    assert!(
        store
            .record_jti(
                &rid,
                "app",
                "j1",
                exp + Duration::minutes(10),
                exp + Duration::seconds(1)
            )
            .await
            .unwrap()
    );

    store
        .put_launch(LaunchHandle {
            handle_hash: "lh".into(),
            realm_id: rid.clone(),
            client_id: Some("app".into()),
            endpoint: "clinic".into(),
            launched_by: None,
            context: LaunchContext {
                patient: Some("p1".into()),
                ..Default::default()
            },
            expires_at: exp,
        })
        .await
        .unwrap();
    assert!(
        store
            .take_launch("other", "lh", now)
            .await
            .unwrap()
            .is_none()
    );
    let l = store.take_launch(&rid, "lh", now).await.unwrap().unwrap();
    assert_eq!(l.context.patient.as_deref(), Some("p1"));
    assert!(store.take_launch(&rid, "lh", now).await.unwrap().is_none());

    let p = PendingAuthorization {
        id_hash: "ix".into(),
        realm_id: rid.clone(),
        realm_name: "clinic".into(),
        client_id: "app".into(),
        client_name: "App".into(),
        redirect_uri: "https://app.example/cb".into(),
        response_mode: ResponseMode::Query,
        state: Some("s".into()),
        nonce: None,
        code_challenge: "c".into(),
        scopes: vec!["openid".into()],
        dropped_scopes: vec![],
        endpoint: "clinic".into(),
        audience: "https://h/fhir".into(),
        prompt: vec![],
        max_age: None,
        login_hint: None,
        ui_locales: vec![],
        launch_context: LaunchContext::default(),
        ehr_launch: false,
        launched_by: None,
        needs: InteractionNeeds::default(),
        progress: serde_json::json!({}),
        created_at: now,
        expires_at: exp,
    };
    store.put_interaction(p.clone()).await.unwrap();
    let mut p2 = p.clone();
    p2.progress = serde_json::json!({"user":"u1"});
    assert!(store.update_interaction(p2.clone()).await.unwrap());
    assert_eq!(
        store.get_interaction(&rid, "ix", now).await.unwrap(),
        Some(p2.clone())
    );
    assert_eq!(
        store.take_interaction(&rid, "ix", now).await.unwrap(),
        Some(p2)
    );
    assert!(
        store
            .take_interaction(&rid, "ix", now)
            .await
            .unwrap()
            .is_none()
    );

    store
        .put_code(code(&rid, "old", "g", now - Duration::seconds(5)))
        .await
        .unwrap();
    assert!(store.purge_expired(now).await.unwrap() >= 1);
}
