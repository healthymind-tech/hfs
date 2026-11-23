//! The built-in authorization server in the `hfs` binary
//! (docs/design/authorization-server.md D-13, package I8).
//!
//! `HFS_IDP=on` mounts, on the same listener as the FHIR API and the
//! console:
//!
//! * `/idp/{realm}/{authorize,token,introspect,revoke,userinfo,jwks,
//!   .well-known/*}` and `/.well-known/oauth-authorization-server/idp/{realm}`
//!   (`helios-idp-server`);
//! * `/idp/{realm}/{login,select-patient,consent,logout,account/apps}`
//!   (`helios-idp-pages`), with the rate limiter around both;
//! * the internal-realm token verifier for FHIR endpoints whose auth
//!   provider is `internal` (`helios_auth::install_internal_realms`);
//! * BALP AuditEvents for every IdP security event (the deployment's audit
//!   sink).
//!
//! With the flag off nothing is mounted (`/idp/*` answers 404) and an
//! endpoint bound to an internal realm stays closed (503), never open.
//!
//! | variable | default | meaning |
//! |---|---|---|
//! | `HFS_IDP` | `off` | `on` enables the authorization server |
//! | `HFS_IDP_PUBLIC_BASE` | origin of `HFS_BASE_URL` | public origin of the issuers (`{base}/idp/{realm}`) |
//! | `HFS_IDP_TRUST_FORWARDED` | `true` | caller IP from `X-Forwarded-For` (nginx in front) |
//! | `HFS_IDP_INTERNAL_URL` | `http://127.0.0.1:{port}` | loopback base the patient picker calls the FHIR API on |
//! | `HFS_IDP_RATE_*` | see `helios_idp_pages::limits` | per-minute limits |
//! | `HFS_IDP_DEV_SEED` | unset | development / test stacks only: a JSON file of realms, clients and users created at start-up when the realm does not exist yet |
//!
//! Storage: PostgreSQL (the primary's pool, tables of migration
//! `hm-0020-idp-core`). Other backends get an in-memory store, which is
//! for development only and says so in the log.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::Router;
use helios_audit::AuditSink;
use helios_auth::{InternalRealmSource, RealmTrust, TrustedKey};
use helios_endpoints::EndpointRegistry;
use helios_idp::runtime::{EndpointUrls, IdpRuntime};
use helios_idp::store::IdpStore;
use helios_rest::ServerConfig;
use tracing::{info, warn};

/// The endpoint registry, recorded by `manager::attach`, so endpoint names
/// resolve to their public base URLs (`aud`).
pub static REGISTRY: OnceLock<Arc<EndpointRegistry>> = OnceLock::new();

/// The primary's PostgreSQL pool (PostgreSQL backends), recorded when the
/// admin session store opens.
pub static PG_POOL: OnceLock<PgPool> = OnceLock::new();

/// The pool type (unit without PostgreSQL support).
#[cfg(feature = "postgres")]
pub type PgPool = helios_endpoints::pg::deadpool_postgres::Pool;
/// The pool type (unit without PostgreSQL support).
#[cfg(not(feature = "postgres"))]
pub type PgPool = ();

/// Whether `HFS_IDP` turns the authorization server on.
pub fn enabled() -> bool {
    helios_idp::flag_enabled(std::env::var(helios_idp::FLAG_ENV).ok().as_deref())
}

/// `{server base}` + the endpoint's path (the default endpoint has none).
struct RegistryUrls {
    server_base: String,
}

impl EndpointUrls for RegistryUrls {
    fn base_url(&self, endpoint: &str) -> Option<String> {
        let base = self.server_base.trim_end_matches('/');
        match REGISTRY.get() {
            Some(reg) => {
                let snap = reg.snapshot();
                let ep = snap.by_name(endpoint)?;
                let path = ep.spec.path.as_str();
                Some(if path.is_empty() {
                    base.to_string()
                } else {
                    format!("{base}/{path}")
                })
            }
            // Without the manager only the default endpoint exists.
            None => (endpoint == helios_endpoints::DEFAULT_ENDPOINT_NAME).then(|| base.to_string()),
        }
    }
}

/// The resource server's view of the realms: public keys, issuer, bound
/// endpoints and the revoked-grant set straight from the IdP runtime
/// (in memory; revocation is visible on the next request).
pub struct RuntimeRealmSource {
    rt: Arc<IdpRuntime>,
    cache: Mutex<HashMap<String, (String, Arc<RealmTrust>)>>,
}

impl RuntimeRealmSource {
    pub fn new(rt: Arc<IdpRuntime>) -> Self {
        Self {
            rt,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

impl InternalRealmSource for RuntimeRealmSource {
    fn realm(&self, realm: &str) -> Option<Arc<RealmTrust>> {
        let r = self.rt.realm(realm).filter(|r| r.spec.enabled)?;
        // Re-parse keys only when the realm or its published keys change.
        let stamp = format!(
            "{}:{}:{}:{:?}",
            r.id,
            r.revision,
            r.published
                .iter()
                .map(|k| k.kid.as_str())
                .collect::<Vec<_>>()
                .join(","),
            r.endpoints
        );
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((s, t)) = cache.get(realm)
            && *s == stamp
        {
            return Some(Arc::clone(t));
        }
        let mut keys = HashMap::new();
        for jwk in r.public_keys() {
            match TrustedKey::from_public_jwk(&jwk) {
                Ok((kid, k)) => {
                    keys.insert(kid, k);
                }
                Err(e) => warn!(realm, error = %e, "idp: unusable public key skipped"),
            }
        }
        let smart = helios_idp_server::smart_configuration(&r);
        let trust = Arc::new(RealmTrust {
            issuer: r.issuer().to_string(),
            keys,
            audiences: r.endpoints.iter().cloned().collect(),
            smart_configuration: r
                .endpoints
                .iter()
                .map(|(name, _)| (name.clone(), smart.clone()))
                .collect(),
        });
        cache.insert(realm.to_string(), (stamp, Arc::clone(&trust)));
        Some(trust)
    }

    fn is_grant_revoked(&self, realm: &str, gid: &str) -> bool {
        // Unknown realm ⇒ treat as revoked (the verifier refuses anyway).
        self.rt.realm(realm).is_none_or(|r| r.grant_revoked(gid))
    }
}

/// The public origin of the issuers.
fn public_base(config: &ServerConfig) -> Option<url::Url> {
    if let Ok(v) = std::env::var("HFS_IDP_PUBLIC_BASE")
        && !v.trim().is_empty()
    {
        return url::Url::parse(v.trim()).ok();
    }
    let u = url::Url::parse(&config.base_url).ok()?;
    url::Url::parse(&u.origin().ascii_serialization()).ok()
}

/// Mounts the authorization server when `HFS_IDP=on` (see the module docs).
pub async fn attach(
    app: Router,
    config: &ServerConfig,
    audit: Option<(Arc<dyn AuditSink>, String)>,
    #[cfg_attr(not(feature = "postgres"), allow(unused_variables))] pool: Option<PgPool>,
) -> anyhow::Result<Router> {
    if !enabled() {
        return Ok(app);
    }
    let Some(base) = public_base(config) else {
        anyhow::bail!("HFS_IDP=on needs a valid HFS_BASE_URL or HFS_IDP_PUBLIC_BASE");
    };
    let store: Arc<dyn IdpStore> = {
        #[cfg(feature = "postgres")]
        {
            match pool.clone() {
                Some(p) => Arc::new(helios_idp::pg::PgStore::new(p)),
                None => {
                    warn!(
                        "HFS_IDP=on without PostgreSQL: the authorization server keeps its data in memory (development only)"
                    );
                    Arc::new(helios_idp::memory::MemoryStore::new())
                }
            }
        }
        #[cfg(not(feature = "postgres"))]
        {
            warn!(
                "HFS_IDP=on without PostgreSQL: the authorization server keeps its data in memory (development only)"
            );
            Arc::new(helios_idp::memory::MemoryStore::new())
        }
    };
    let key_ring = match helios_endpoints::SecretKeyRing::from_env() {
        Ok(r) => r.map(Arc::new),
        Err(e) => anyhow::bail!("HFS_SECRET_KEYS is invalid: {e}"),
    };
    if key_ring.is_none() {
        warn!("HFS_IDP=on but HFS_SECRET_KEYS is not set: realms cannot be created or sign tokens");
    }
    let server_base = config.base_url.clone();
    let rt = IdpRuntime::new(
        store,
        key_ring,
        base.clone(),
        Arc::new(RegistryUrls {
            server_base: server_base.clone(),
        }),
    );
    if let Err(e) = rt.reload().await {
        warn!(error = %e, "idp: initial load failed; retrying in the background");
    }
    if let Ok(path) = std::env::var("HFS_IDP_DEV_SEED")
        && !path.trim().is_empty()
    {
        seed(&rt, path.trim()).await?;
    }
    let _watcher = rt.spawn_watcher(Duration::from_secs(1));
    spawn_maintenance(rt.clone(), pool.clone());
    // Endpoint changes (created, renamed path) change the realms' audience
    // URLs: reload the realms when the endpoint registry moves on.
    {
        let rt = rt.clone();
        tokio::spawn(async move {
            let mut seen = None;
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                let Some(reg) = REGISTRY.get() else { continue };
                let epoch = reg.snapshot().epoch();
                if seen.is_some_and(|s| s != epoch)
                    && let Err(e) = rt.reload().await
                {
                    warn!(error = %e, "idp: reload after an endpoint change failed");
                }
                seen = Some(epoch);
            }
        });
    }
    if let Some((sink, observer)) = audit {
        rt.set_event_sink(Arc::new(helios_idp_pages::audit::BalpEventSink::new(
            sink,
            observer,
            base.as_str(),
        )));
    }
    helios_auth::install_internal_realms(Arc::new(RuntimeRealmSource::new(rt.clone())));
    // Console pages, wizard, diagnostic and /admin/idp (crate::idp_console).
    crate::idp_console::install(&rt, &server_base);

    let outbound = Arc::new(helios_endpoints::OutboundPolicy::from_env());
    let server = helios_idp_server::IdpServer::new(
        rt.clone(),
        helios_idp_server::IdpServerOptions {
            outbound,
            ..Default::default()
        },
    );
    let secure = base.scheme() == "https";
    let trust_forwarded = std::env::var("HFS_IDP_TRUST_FORWARDED")
        .map(|v| {
            !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "false" | "0" | "no" | "off"
            )
        })
        .unwrap_or(true);
    let internal = std::env::var("HFS_IDP_INTERNAL_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| format!("http://127.0.0.1:{}", config.port));
    let urls = RegistryUrls {
        server_base: internal.trim_end_matches('/').to_string(),
    };
    let directory = helios_idp_pages::directory::FhirPatientDirectory::new(Arc::new(move |ep| {
        urls.base_url(ep)
    }));
    let csrf = match std::env::var("HFS_SECRET_KEYS") {
        Ok(k) if !k.trim().is_empty() => helios_idp_pages::csrf::CsrfKey::derive(k.as_bytes()),
        _ => helios_idp_pages::csrf::CsrfKey::random(),
    };
    let lockout_store: Arc<dyn helios_admin_auth::AdminSessionStore> = {
        #[cfg(feature = "postgres")]
        {
            match pool {
                Some(p) => {
                    match helios_admin_auth::store::postgres::PgAdminSessionStore::connect(p, "IDP")
                        .await
                    {
                        Ok(s) => Arc::new(s),
                        Err(e) => anyhow::bail!("idp lockout store: {e}"),
                    }
                }
                None => Arc::new(helios_admin_auth::store::testing::MemoryStore::default()),
            }
        }
        #[cfg(not(feature = "postgres"))]
        {
            Arc::new(helios_admin_auth::store::testing::MemoryStore::default())
        }
    };
    let pages = helios_idp_pages::Pages::new(
        rt.clone(),
        server.completer(),
        Arc::new(directory),
        helios_idp_pages::lockout::Lockout::new(lockout_store),
        csrf,
        helios_idp_pages::PagesConfig {
            public_origin: base.origin().ascii_serialization(),
            secure_cookies: secure,
            trust_forwarded,
        },
    );
    let limiter = helios_idp_pages::limits::RateLimiter::new(
        helios_idp_pages::limits::LimitConfig::from_env(),
        trust_forwarded,
    );
    let idp = server
        .router()
        .merge(helios_idp_pages::router(pages))
        // The caller address for A's events (trusted-proxy aware).
        .layer(axum::middleware::from_fn(caller_ip_for_protocol))
        .layer(axum::middleware::from_fn_with_state(
            limiter,
            helios_idp_pages::limits::layer,
        ));
    info!(public_base = %base, realms = rt.realms().len(), "built-in authorization server on (/idp)");
    Ok(app.merge(idp))
}

/// Copies the limiter's resolved caller address into the protocol server's
/// `ClientIp` extension.
async fn caller_ip_for_protocol(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Some(helios_idp_pages::limits::CallerIp(ip)) = req
        .extensions()
        .get::<helios_idp_pages::limits::CallerIp>()
        .cloned()
    {
        req.extensions_mut().insert(helios_idp_server::ClientIp(ip));
    }
    next.run(req).await
}

/// One realm of a development seed file (`HFS_IDP_DEV_SEED`).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeedRealm {
    spec: helios_idp::realm::RealmSpec,
    #[serde(default)]
    clients: Vec<helios_idp::client::ClientSpec>,
    #[serde(default)]
    users: Vec<SeedUser>,
}

#[derive(serde::Deserialize)]
struct SeedUser {
    user: helios_idp::user::RealmUser,
    password: Option<String>,
}

#[derive(serde::Deserialize)]
struct SeedFile {
    realms: Vec<SeedRealm>,
}

/// Applies a development seed: realms that do not exist yet are created
/// with their clients and users (existing realms are left alone). Meant for
/// test stacks and CI; production realms come from the console wizard.
async fn seed(rt: &Arc<IdpRuntime>, path: &str) -> anyhow::Result<()> {
    warn!(
        path,
        "HFS_IDP_DEV_SEED is set: seeding authorization-server realms (development only)"
    );
    let raw = std::fs::read_to_string(path)?;
    let file: SeedFile = serde_json::from_str(&raw)?;
    let admin = helios_idp::admin::IdpAdmin::new(rt.clone());
    for r in file.realms {
        if rt
            .store()
            .get_realm_by_name(r.spec.name.as_str())
            .await?
            .is_some()
        {
            continue;
        }
        let rec = admin
            .create_realm(r.spec)
            .await
            .map_err(|e| anyhow::anyhow!("seed realm: {e}"))?;
        for c in r.clients {
            let created = admin
                .create_client(&rec.id, c)
                .await
                .map_err(|e| anyhow::anyhow!("seed client: {e}"))?;
            if let Some(secret) = created.client_secret {
                info!(client = %created.record.spec.client_id, "seeded client secret: {secret}");
            }
        }
        for u in r.users {
            admin
                .create_user(&rec.id, u.user, u.password.as_deref(), false)
                .await
                .map_err(|e| anyhow::anyhow!("seed user: {e}"))?;
        }
        info!(realm = %rec.spec.name.as_str(), "seeded realm");
    }
    rt.reload().await?;
    Ok(())
}

/// Advisory-lock key of the maintenance lease ("HMIDPMNT").
#[cfg(feature = "postgres")]
const MAINTENANCE_LOCK: i64 = 0x484d_4944_504d_4e54;

/// Expired-artefact purge and key rotation (`IdpAdmin::run_maintenance`)
/// every minute. With PostgreSQL only the instance holding the
/// transaction-scoped advisory lock runs it (a cluster lease: the others
/// skip that tick); single-node deployments just run it.
fn spawn_maintenance(
    rt: Arc<IdpRuntime>,
    #[cfg_attr(not(feature = "postgres"), allow(unused_variables))] pool: Option<PgPool>,
) {
    tokio::spawn(async move {
        let admin = helios_idp::admin::IdpAdmin::new(rt);
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            #[cfg(feature = "postgres")]
            if let Some(pool) = &pool {
                match maintenance_under_lease(pool, &admin).await {
                    Ok(Some((purged, actions))) if purged > 0 || actions > 0 => {
                        info!(purged, key_actions = actions, "idp maintenance")
                    }
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "idp maintenance failed"),
                }
                continue;
            }
            match admin.run_maintenance(chrono::Utc::now()).await {
                Ok((purged, actions)) if purged > 0 || actions > 0 => {
                    info!(purged, key_actions = actions, "idp maintenance")
                }
                Ok(_) => {}
                Err(e) => warn!(error = %e, "idp maintenance failed"),
            }
        }
    });
}

#[cfg(feature = "postgres")]
async fn maintenance_under_lease(
    pool: &PgPool,
    admin: &helios_idp::admin::IdpAdmin,
) -> anyhow::Result<Option<(u64, usize)>> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;
    let row = tx
        .query_one("SELECT pg_try_advisory_xact_lock($1)", &[&MAINTENANCE_LOCK])
        .await?;
    let held: bool = row.get(0);
    let out = if held {
        Some(admin.run_maintenance(chrono::Utc::now()).await?)
    } else {
        None
    };
    tx.commit().await?;
    Ok(out)
}
