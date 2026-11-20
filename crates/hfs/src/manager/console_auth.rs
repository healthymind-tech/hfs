//! Console sign-in wiring (FHIR server manager package P3, design §7).
//!
//! Installs [`helios_admin_auth::console::ConsoleAuth`] when console sign-in
//! is switched on — `HFS_MANAGER=on`, or an explicit `HFS_CONSOLE_AUTH_MODE`
//! — and the web UI is served. Without either the server behaves exactly as
//! before (no console runtime, today's admin-pages-only gate).
//!
//! What it does:
//! * builds the runtime over PostgreSQL stores when the primary is
//!   PostgreSQL (`hfs_console_*`, `login_sessions kind='console'`,
//!   `admin_login_failures` scope `HFS-CONSOLE`), else memory stores;
//! * [`routes`]: merges the console admin API (`/admin/console/**`) — before
//!   the admin guard layer, so the guard covers it;
//! * [`layer`]: puts `Extension<Arc<ConsoleAuth>>` on every request,
//!   outermost, which switches the UI gate (`/ui/**` needs a session in
//!   modes oidc/local), the admin API guard (console admin sessions + CSRF
//!   token) and the console pages on.
//!
//! When package P1's `ManagerContext` lands, `install_console_auth(&mut ctx)`
//! becomes the single call site and these functions its body.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use helios_admin_auth::console::{
    ConsoleAuth, ConsoleOptions, EnvBootstrap, MemorySessionStore, memory::MemoryConsoleStore,
};
use helios_admin_auth::{AdminGate, AdminSessionStore, AdminSettings};
use tracing::{info, warn};

/// The shared PostgreSQL pool, recorded by the PG backend's startup.
#[cfg(feature = "postgres")]
static PG_POOL: OnceLock<helios_endpoints::pg::deadpool_postgres::Pool> = OnceLock::new();

/// Records the primary's pool for the console stores (PostgreSQL backends).
#[cfg(feature = "postgres")]
pub fn record_pool(pool: helios_endpoints::pg::deadpool_postgres::Pool) {
    let _ = PG_POOL.set(pool);
}

static CONSOLE: OnceLock<Option<Arc<ConsoleAuth>>> = OnceLock::new();

fn env_on(name: &str) -> bool {
    std::env::var(name)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "on" | "true" | "1" | "yes"
            )
        })
        .unwrap_or(false)
}

/// Whether console sign-in is switched on for this process.
pub fn enabled(env: &EnvBootstrap) -> bool {
    env_on("HFS_MANAGER") || env.explicit_mode
}

/// The console's public origin: `HFS_CONSOLE_PUBLIC_URL`, else the origin of
/// `HFS_BASE_URL` (the console is served at `/ui` on that origin; the FHIR
/// base may carry a path such as `/fhir`).
pub fn public_origin(base_url: &str) -> String {
    if let Ok(v) = std::env::var("HFS_CONSOLE_PUBLIC_URL")
        && !v.trim().is_empty()
    {
        return v.trim().trim_end_matches('/').to_string();
    }
    match url::Url::parse(base_url) {
        Ok(u) => u.origin().ascii_serialization(),
        Err(_) => base_url.trim_end_matches('/').to_string(),
    }
}

/// Builds (once) the runtime, or `None` when console sign-in is off.
pub async fn build(
    base_url: &str,
    ui_enabled: bool,
    break_glass: AdminGate,
) -> Option<Arc<ConsoleAuth>> {
    if let Some(c) = CONSOLE.get() {
        return c.clone();
    }
    let env = EnvBootstrap::from_env();
    let console = if !enabled(&env) {
        None
    } else if !ui_enabled {
        warn!("console sign-in is configured but the web UI is not served; nothing to protect");
        None
    } else {
        Some(build_runtime(env, base_url, break_glass).await)
    };
    let _ = CONSOLE.set(console.clone());
    console
}

async fn build_runtime(
    env: EnvBootstrap,
    base_url: &str,
    break_glass: AdminGate,
) -> Arc<ConsoleAuth> {
    let admin = AdminSettings::from_env("HFS").allow_origin_of(base_url);
    let origin = public_origin(base_url);
    let key_ring = match helios_endpoints::SecretKeyRing::from_env() {
        Ok(ring) => ring.map(Arc::new),
        Err(e) => {
            warn!(error = %e, "HFS_SECRET_KEYS is invalid; console secrets cannot be stored");
            None
        }
    };
    let mut options = ConsoleOptions {
        public_base_url: origin.clone(),
        cookie_secure: admin.cookie_secure,
        trusted_proxies: admin.trusted_proxies.clone(),
        allowed_origins: {
            let mut o = admin.allowed_origins.clone();
            if let Ok(u) = url::Url::parse(&origin) {
                let s = u.origin().ascii_serialization();
                if !o.contains(&s) {
                    o.push(s);
                }
            }
            o
        },
        outbound: helios_endpoints::OutboundPolicy::from_env(),
        key_ring,
        recheck: Duration::from_millis(
            std::env::var("HFS_CONSOLE_RECHECK_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10_000),
        ),
        persistent: false,
    };

    #[cfg(feature = "postgres")]
    if let Some(pool) = PG_POOL.get().cloned() {
        let sessions =
            helios_admin_auth::console::sessions::postgres::PgConsoleSessionStore::connect(
                pool.clone(),
            )
            .await;
        let lockout = helios_admin_auth::store::postgres::PgAdminSessionStore::connect(
            pool.clone(),
            helios_admin_auth::console::LOCKOUT_SCOPE,
        )
        .await;
        match (sessions, lockout) {
            (Ok(sessions), Ok(lockout)) => {
                options.persistent = true;
                info!(public_url = %origin, "console sign-in: PostgreSQL stores");
                return ConsoleAuth::new(
                    env,
                    Arc::new(helios_endpoints::pg_console::PgConsoleStore::new(pool)),
                    Arc::new(sessions),
                    Arc::new(lockout) as Arc<dyn AdminSessionStore>,
                    break_glass,
                    options,
                )
                .await;
            }
            (Err(e), _) | (_, Err(e)) => warn!(
                error = %e,
                "console sign-in: PostgreSQL stores unavailable; falling back to memory"
            ),
        }
    }

    warn!(
        public_url = %origin,
        "console sign-in: memory stores (settings, accounts and sessions last until restart)"
    );
    ConsoleAuth::new(
        env,
        Arc::new(MemoryConsoleStore::new()),
        Arc::new(MemorySessionStore::default()),
        Arc::new(helios_admin_auth::store::testing::MemoryStore::default()),
        break_glass,
        options,
    )
    .await
}

/// Merges the console admin API. Call **before** the admin guard layer.
pub fn routes(app: axum::Router, console: Option<&Arc<ConsoleAuth>>) -> axum::Router {
    match console {
        Some(c) => app.merge(helios_admin_auth::console::api::router(c.clone())),
        None => app,
    }
}

/// Makes the runtime visible to every layer. Call **last** (outermost).
pub fn layer(app: axum::Router, console: Option<&Arc<ConsoleAuth>>) -> axum::Router {
    match console {
        Some(c) => app.layer(axum::Extension(c.clone())),
        None => app,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_origin_strips_the_fhir_path() {
        // SAFETY of the env read: the variable is not set in tests.
        if std::env::var("HFS_CONSOLE_PUBLIC_URL").is_ok() {
            return;
        }
        assert_eq!(
            public_origin("https://hfs.gugulu.tw/fhir"),
            "https://hfs.gugulu.tw"
        );
        assert_eq!(
            public_origin("http://localhost:8080"),
            "http://localhost:8080"
        );
    }
}
