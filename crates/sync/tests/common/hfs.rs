//! A second, real HFS instance (helios-rest over in-memory SQLite) served
//! on a loopback port, for two-instance sync tests.
#![allow(dead_code)]

use std::path::PathBuf;

use helios_persistence::backends::sqlite::{SqliteBackend, SqliteBackendConfig};
use helios_rest::config::{PackageConfig, RegistryMode, ServerConfig};

/// Base URL of the started server.
pub async fn start_hfs() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let data_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("data"))
        .unwrap_or_else(|| PathBuf::from("data"));
    let backend = SqliteBackend::with_config(
        ":memory:",
        SqliteBackendConfig {
            data_dir: Some(data_dir),
            ..Default::default()
        },
    )
    .expect("sqlite");
    backend.init_schema().expect("schema");
    let cache = std::env::temp_dir().join(format!("hfs-sync-test-{}", uuid::Uuid::new_v4().simple()));
    let base = format!("http://127.0.0.1:{port}");
    let config = ServerConfig {
        seed_conformance: false,
        request_timeout: 60,
        base_url: base.clone(),
        packages: PackageConfig {
            registry: RegistryMode::Off,
            cache_dir: Some(cache),
            ..PackageConfig::default()
        },
        ..ServerConfig::for_testing()
    };
    let app = helios_rest::create_app_with_config(backend, config);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    base
}
