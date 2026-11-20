//! FHIR server manager — PostgreSQL endpoint store (package P1).
//!
//! `helios_endpoints::pg::PgEndpointStore` over `hm-0008-endpoints`: the
//! shared store conformance suite, concurrent writers across pools, cluster
//! coherence of two registries on one database (epoch poll and `LISTEN
//! hfs_config` wake-up), and the full app serving a named endpoint from the
//! PostgreSQL primary.
//!
//! Uses the server named by `HFS_TEST_PG_HOST` / `HFS_TEST_PG_PORT`
//! (postgres/postgres superuser) when set — the HealthyMind CI service
//! container — else starts a PostgreSQL 16 testcontainer. Every test runs in
//! its own database.

#![cfg(feature = "postgres")]

#[path = "common/container_cleanup.rs"]
mod container_cleanup;

mod endpoints_postgres_tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use axum::http::StatusCode;
    use axum_test::TestServer;
    use helios_endpoints::pg::PgEndpointStore;
    use helios_endpoints::{
        Actor, EndpointName, EndpointRegistry, EndpointSpec, EndpointStore, FhirRelease,
        StoreError, ensure_default_endpoint,
    };
    use helios_persistence::backends::postgres::{PostgresBackend, PostgresConfig};
    use helios_persistence::core::ResourceStorage;
    use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
    use helios_rest::endpoint::EndpointRuntime;
    use helios_rest::{OperationsBundle, ServerConfig};
    use serde_json::{Value, json};
    use testcontainers::ImageExt;
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;
    use tokio::sync::OnceCell;

    struct SharedPg {
        host: String,
        port: u16,
        _container: Option<testcontainers::ContainerAsync<Postgres>>,
    }

    static SHARED_PG: OnceCell<SharedPg> = OnceCell::const_new();

    async fn shared_pg() -> &'static SharedPg {
        SHARED_PG
            .get_or_init(|| async {
                if let Ok(host) = std::env::var("HFS_TEST_PG_HOST") {
                    let port = std::env::var("HFS_TEST_PG_PORT")
                        .ok()
                        .and_then(|p| p.parse().ok())
                        .unwrap_or(5432);
                    return SharedPg {
                        host,
                        port,
                        _container: None,
                    };
                }
                let run_id = std::env::var("GITHUB_RUN_ID").unwrap_or_default();
                let container = super::container_cleanup::with_cleanup_label(
                    Postgres::default()
                        .with_tag("16-alpine")
                        .with_label("github.run_id", &run_id),
                )
                .start()
                .await
                .expect("failed to start PostgreSQL container");
                SharedPg {
                    host: container.get_host().await.expect("host").to_string(),
                    port: container.get_host_port_ipv4(5432).await.expect("port"),
                    _container: Some(container),
                }
            })
            .await
    }

    fn data_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("data"))
            .unwrap_or_else(|| PathBuf::from("data"))
    }

    fn pg_config(dbname: &str) -> PostgresConfig {
        let pg = SHARED_PG.get().expect("server started");
        PostgresConfig {
            host: pg.host.clone(),
            port: pg.port,
            dbname: dbname.to_string(),
            user: "postgres".to_string(),
            password: Some("postgres".to_string()),
            max_connections: 4,
            data_dir: Some(data_dir()),
            ..Default::default()
        }
    }

    /// A fresh, schema-initialized database; returns its name.
    async fn fresh_database() -> String {
        shared_pg().await;
        let dbname = format!("endpoints_{}", uuid::Uuid::new_v4().simple());
        PostgresBackend::new(pg_config("postgres"))
            .await
            .expect("connect to postgres")
            .get_client()
            .await
            .expect("checkout")
            .batch_execute(&format!("CREATE DATABASE {dbname}"))
            .await
            .expect("create database");
        let backend = PostgresBackend::new(pg_config(&dbname))
            .await
            .expect("connect for schema");
        backend.init_schema().await.expect("init schema");
        dbname
    }

    /// A backend ("instance") on `dbname`.
    async fn instance(dbname: &str) -> Arc<PostgresBackend> {
        Arc::new(
            PostgresBackend::new(pg_config(dbname))
                .await
                .expect("connect"),
        )
    }

    fn listen_config(dbname: &str) -> helios_endpoints::pg::tokio_postgres::Config {
        let c = pg_config(dbname);
        let mut cfg = helios_endpoints::pg::tokio_postgres::Config::new();
        cfg.host(&c.host)
            .port(c.port)
            .dbname(&c.dbname)
            .user(&c.user)
            .password(c.password.unwrap_or_default());
        cfg
    }

    fn named(name: &str) -> EndpointSpec {
        EndpointSpec::named(EndpointName::parse(name).unwrap(), name)
    }

    #[tokio::test]
    async fn postgres_integration_pg_store_conformance() {
        let db = fresh_database().await;
        let backend = instance(&db).await;
        let store = PgEndpointStore::new(backend.shared_pool());
        helios_endpoints::store::conformance::run_all(&store).await;
        // Revisions are stored redacted and the epoch counts every write.
        let epoch = store.epoch().await.unwrap();
        assert!(epoch >= 7, "epoch {epoch}");
    }

    #[tokio::test]
    async fn postgres_integration_pg_store_concurrent_writers() {
        let db = fresh_database().await;
        let a = PgEndpointStore::new(instance(&db).await.shared_pool());
        let b = PgEndpointStore::new(instance(&db).await.shared_pool());
        // Two instances starting together create one default endpoint.
        let (da, db_) = tokio::join!(
            ensure_default_endpoint(&a, "default", FhirRelease::R4),
            ensure_default_endpoint(&b, "default", FhirRelease::R4)
        );
        assert_eq!(da.unwrap().id, db_.unwrap().id);
        // Racing creates of one name: exactly one wins, the other is a Duplicate.
        let actor = Actor::bootstrap();
        let (ra, rb) = tokio::join!(
            a.create(named("race"), &actor),
            b.create(named("race"), &actor)
        );
        let oks = [&ra, &rb].iter().filter(|r| r.is_ok()).count();
        assert_eq!(oks, 1, "{ra:?} {rb:?}");
        let err = if ra.is_err() { ra } else { rb }.unwrap_err();
        assert!(
            matches!(err, StoreError::Duplicate { field: "name", .. }),
            "{err:?}"
        );
        // Racing updates: CAS lets exactly one through.
        let cur = a.get("race").await.unwrap().unwrap();
        let mut s1 = cur.spec.clone();
        s1.display_name = "one".into();
        let mut s2 = cur.spec.clone();
        s2.display_name = "two".into();
        let (u1, u2) = tokio::join!(
            a.update("race", cur.revision, s1, &actor),
            b.update("race", cur.revision, s2, &actor)
        );
        assert_eq!([&u1, &u2].iter().filter(|r| r.is_ok()).count(), 1);
        let conflict = if u1.is_err() { u1 } else { u2 }.unwrap_err();
        assert_eq!(conflict, StoreError::Conflict { current: 2 });
        assert_eq!(a.list(false).await.unwrap().len(), 2);
        let history = a.history(&cur.id, 10).await.unwrap();
        assert_eq!(history.len(), 2);
    }

    /// Two instances on one database: a change written through A is served
    /// by B within the poll interval (here 1 s), with no NOTIFY involved.
    #[tokio::test]
    async fn postgres_integration_pg_store_two_registries_converge() {
        let db = fresh_database().await;
        let store_a: Arc<dyn EndpointStore> =
            Arc::new(PgEndpointStore::new(instance(&db).await.shared_pool()));
        let store_b: Arc<dyn EndpointStore> =
            Arc::new(PgEndpointStore::new(instance(&db).await.shared_pool()));
        ensure_default_endpoint(store_a.as_ref(), "default", FhirRelease::R4)
            .await
            .unwrap();
        let reg_b = Arc::new(EndpointRegistry::load(store_b.as_ref()).await.unwrap());
        let watcher = tokio::spawn(helios_endpoints::run_watcher(
            Arc::clone(&reg_b),
            Arc::clone(&store_b),
            Duration::from_millis(1000),
        ));
        store_a
            .create(named("wide"), &Actor::bootstrap())
            .await
            .unwrap();
        let started = Instant::now();
        while reg_b.snapshot().by_name("wide").is_none() {
            assert!(
                started.elapsed() < Duration::from_millis(2500),
                "instance B did not converge"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let took = started.elapsed();
        watcher.abort();
        assert!(
            took <= Duration::from_millis(1500),
            "converged after {took:?}"
        );
    }

    /// `LISTEN hfs_config`: with the poll effectively off (1 h), B still sees
    /// A's change promptly because the NOTIFY pokes its watcher.
    #[tokio::test]
    async fn postgres_integration_pg_store_notify_wakes_watcher() {
        let db = fresh_database().await;
        let store_a: Arc<dyn EndpointStore> =
            Arc::new(PgEndpointStore::new(instance(&db).await.shared_pool()));
        let store_b: Arc<dyn EndpointStore> =
            Arc::new(PgEndpointStore::new(instance(&db).await.shared_pool()));
        ensure_default_endpoint(store_a.as_ref(), "default", FhirRelease::R4)
            .await
            .unwrap();
        let reg_b = Arc::new(EndpointRegistry::load(store_b.as_ref()).await.unwrap());
        let mut changed = reg_b.subscribe();
        let watcher = tokio::spawn(helios_endpoints::run_watcher(
            Arc::clone(&reg_b),
            Arc::clone(&store_b),
            Duration::from_secs(3600),
        ));
        let listener = tokio::spawn(helios_endpoints::pg::run_config_listener(
            listen_config(&db),
            Arc::clone(&reg_b),
        ));
        // Let the LISTEN session establish (it pokes once when it does).
        tokio::time::sleep(Duration::from_millis(500)).await;
        changed.borrow_and_update();
        store_a
            .create(named("pushed"), &Actor::bootstrap())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), changed.changed())
            .await
            .expect("NOTIFY did not wake the watcher")
            .unwrap();
        assert!(reg_b.snapshot().by_name("pushed").is_some());
        watcher.abort();
        listener.abort();
    }

    /// The full app on the PostgreSQL primary with the PostgreSQL endpoint
    /// store: a named endpoint serves CRUD in its own tenant, and a new
    /// endpoint becomes reachable without a restart.
    #[tokio::test]
    async fn postgres_integration_pg_store_app_end_to_end() {
        let db = fresh_database().await;
        let backend = instance(&db).await;
        let store: Arc<dyn EndpointStore> = Arc::new(PgEndpointStore::new(backend.shared_pool()));
        ensure_default_endpoint(store.as_ref(), "default", FhirRelease::R4)
            .await
            .unwrap();
        store
            .create(named("lab"), &Actor::bootstrap())
            .await
            .unwrap();
        let registry = Arc::new(EndpointRegistry::load(store.as_ref()).await.unwrap());
        let runtime = EndpointRuntime::new(Arc::clone(&registry), Arc::clone(&store));
        let config = ServerConfig {
            base_url: "https://hfs.example/fhir".to_string(),
            default_tenant: "default".to_string(),
            ..ServerConfig::for_testing()
        };
        let app = helios_rest::create_app_with_auth_bulk_settings_and_ops(
            Arc::clone(&backend),
            config,
            helios_auth::AuthConfig::default(),
            None,
            None,
            None,
            None,
            None,
            OperationsBundle {
                endpoints: Some(runtime.clone()),
                ..Default::default()
            },
        );
        let server = TestServer::new(app).unwrap();
        let res = server
            .post("/lab/Patient")
            .json(&json!({"resourceType": "Patient", "active": true}))
            .await;
        assert_eq!(res.status_code(), StatusCode::CREATED, "{}", res.text());
        let id = res.json::<Value>()["id"].as_str().unwrap().to_string();
        assert_eq!(
            res.headers().get("location").unwrap().to_str().unwrap(),
            format!("https://hfs.example/fhir/lab/Patient/{id}/_history/1")
        );
        let lab = TenantContext::new(TenantId::new("lab"), TenantPermissions::full_access());
        assert!(backend.read(&lab, "Patient", &id).await.unwrap().is_some());
        assert_eq!(
            server.get(&format!("/Patient/{id}")).await.status_code(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            server
                .get("/Patient")
                .add_header("x-tenant-id", "lab")
                .await
                .status_code(),
            StatusCode::FORBIDDEN
        );
        // Hot reload from the PostgreSQL store.
        store
            .create(named("late"), &Actor::bootstrap())
            .await
            .unwrap();
        assert!(runtime.reload_if_stale().await.unwrap());
        let meta: Value = server.get("/late/metadata").await.json();
        assert_eq!(
            meta["implementation"]["url"],
            "https://hfs.example/fhir/late"
        );
    }
}
