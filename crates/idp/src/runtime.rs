//! `IdpRuntime`: the hot snapshot every instance serves from (design §10.3).
//!
//! It holds, per realm, the spec, the issuer URLs, the opened signing keys
//! (ring key pairs; plaintext only in memory), the published JWKS, the bound
//! endpoints' `aud` URLs and the revoked-grant deny set. It is reloaded from
//! the store after every local write and whenever the `idp` configuration
//! epoch moves (another instance wrote), see [`IdpRuntime::spawn_watcher`].
//!
//! The internal resource-server verifier (package I5) reads the public keys
//! and the deny set from here; the protocol endpoints read everything.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::Utc;
use helios_endpoints::SecretKeyRing;
use serde_json::Value;
use url::Url;

use crate::error::IdpError;
use crate::events::{IdpEvent, IdpEventSink, TracingEventSink};
use crate::jws::{self, JwsError, Verified};
use crate::keys::{KeyRecord, KeyStatus, SigningAlg, SigningKey, jwks_document};
use crate::metadata::RealmUrls;
use crate::realm::RealmSpec;
use crate::store::IdpStore;

/// Maps a bound endpoint name to its public FHIR base URL (the `aud` value).
/// The server wires this to the endpoint registry.
pub trait EndpointUrls: Send + Sync + 'static {
    fn base_url(&self, endpoint: &str) -> Option<String>;
}

/// `{public_base}/fhir/{name}` (and `{public_base}/fhir` for `default`):
/// the deployment's path convention, used by tests and as a fallback.
#[derive(Debug, Clone)]
pub struct ConventionalEndpointUrls {
    pub public_base: String,
}

impl EndpointUrls for ConventionalEndpointUrls {
    fn base_url(&self, endpoint: &str) -> Option<String> {
        let b = self.public_base.trim_end_matches('/');
        Some(if endpoint == "default" {
            format!("{b}/fhir")
        } else {
            format!("{b}/fhir/{endpoint}")
        })
    }
}

impl<F> EndpointUrls for F
where
    F: Fn(&str) -> Option<String> + Send + Sync + 'static,
{
    fn base_url(&self, endpoint: &str) -> Option<String> {
        self(endpoint)
    }
}

fn norm(u: &str) -> &str {
    u.trim_end_matches('/')
}

/// One realm, ready to serve.
pub struct RealmRuntime {
    pub id: String,
    pub spec: RealmSpec,
    pub revision: i64,
    pub urls: RealmUrls,
    /// Active signing key per algorithm.
    signers: HashMap<SigningAlg, Arc<SigningKey>>,
    /// Published key records (`next`, `active`, `retiring`).
    pub published: Vec<KeyRecord>,
    /// Revoked, unexpired grant ids (reloaded from the store; local
    /// revocations are added at once by [`IdpRuntime::mark_revoked`]).
    revoked_grants: std::sync::RwLock<HashSet<String>>,
    /// (endpoint name, aud URL without trailing slash).
    pub endpoints: Vec<(String, String)>,
    /// Why no signer is available (no key ring, missing key…).
    pub key_problem: Option<String>,
}

impl std::fmt::Debug for RealmRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealmRuntime")
            .field("id", &self.id)
            .field("name", &self.spec.name)
            .field("signers", &self.signers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl RealmRuntime {
    pub fn issuer(&self) -> &str {
        &self.urls.issuer
    }

    /// Signs access tokens: the first configured algorithm with an active key.
    pub fn access_token_signer(&self) -> Option<Arc<SigningKey>> {
        self.spec
            .signing_algs
            .iter()
            .find_map(|a| self.signers.get(a).cloned())
    }

    /// Signs id_tokens: RS256 (OIDC Core §3.1.3.7 default).
    pub fn id_token_signer(&self) -> Option<Arc<SigningKey>> {
        self.signers.get(&SigningAlg::RS256).cloned()
    }

    pub fn signer(&self, alg: SigningAlg) -> Option<Arc<SigningKey>> {
        self.signers.get(&alg).cloned()
    }

    /// The JWKS document (`/jwks`).
    pub fn jwks(&self) -> Value {
        jwks_document(&self.published)
    }

    /// Public JWKs of every published key (for verifying our own tokens).
    pub fn public_keys(&self) -> Vec<Value> {
        self.published
            .iter()
            .map(|k| k.public_jwk.clone())
            .collect()
    }

    /// The bound endpoint whose base URL equals `aud` (trailing slash
    /// ignored; everything else exact).
    pub fn endpoint_for_aud(&self, aud: &str) -> Option<&(String, String)> {
        self.endpoints.iter().find(|(_, b)| b == norm(aud))
    }

    pub fn endpoint_base(&self, name: &str) -> Option<&str> {
        self.endpoints
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, b)| b.as_str())
    }

    /// Whether the grant is in the deny set (internal verifier, design §5.2).
    pub fn grant_revoked(&self, gid: &str) -> bool {
        self.revoked_grants.read().expect("poisoned").contains(gid)
    }

    fn add_revoked(&self, gid: &str) {
        self.revoked_grants
            .write()
            .expect("poisoned")
            .insert(gid.to_string());
    }

    /// Verifies a JWT this realm signed: signature against a published key,
    /// `typ` (when given), `iss`. Expiry is left to the caller (introspection
    /// reports expired tokens as inactive, logout accepts expired hints).
    pub fn verify_own(&self, token: &str, typ: Option<&str>) -> Result<Verified, JwsError> {
        let v = jws::verify(token, &self.public_keys(), &SigningAlg::ALL)?;
        if let Some(t) = typ {
            let got = v.header.get("typ").and_then(Value::as_str).unwrap_or("");
            if !got.eq_ignore_ascii_case(t) {
                return Err(JwsError::Malformed("unexpected typ"));
            }
        }
        if v.claims.get("iss").and_then(Value::as_str) != Some(self.issuer()) {
            return Err(JwsError::Malformed("foreign issuer"));
        }
        Ok(v)
    }
}

#[derive(Default)]
struct Snapshot {
    by_name: HashMap<String, Arc<RealmRuntime>>,
    epoch: Option<i64>,
}

/// The shared runtime.
pub struct IdpRuntime {
    store: Arc<dyn IdpStore>,
    key_ring: Option<Arc<SecretKeyRing>>,
    public_base: Url,
    endpoints: Arc<dyn EndpointUrls>,
    snap: ArcSwap<Snapshot>,
    reload_lock: tokio::sync::Mutex<()>,
    events: std::sync::RwLock<Arc<dyn IdpEventSink>>,
}

impl IdpRuntime {
    pub fn new(
        store: Arc<dyn IdpStore>,
        key_ring: Option<Arc<SecretKeyRing>>,
        public_base: Url,
        endpoints: Arc<dyn EndpointUrls>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            key_ring,
            public_base,
            endpoints,
            snap: ArcSwap::from_pointee(Snapshot::default()),
            reload_lock: tokio::sync::Mutex::new(()),
            events: std::sync::RwLock::new(Arc::new(TracingEventSink)),
        })
    }

    /// Installs the security event sink (audit). Default: [`TracingEventSink`].
    pub fn set_event_sink(&self, sink: Arc<dyn IdpEventSink>) {
        *self.events.write().expect("poisoned") = sink;
    }

    /// Emits a security event.
    pub fn emit(&self, e: IdpEvent) {
        let sink = self.events.read().expect("poisoned").clone();
        sink.emit(e);
    }

    /// Adds a revoked grant to the local deny set immediately (the store
    /// already holds the revocation; other instances pick it up on the
    /// epoch reload).
    pub fn mark_revoked(&self, realm_name: &str, gid: &str) {
        if let Some(r) = self.realm(realm_name) {
            r.add_revoked(gid);
        }
    }

    pub fn store(&self) -> &Arc<dyn IdpStore> {
        &self.store
    }

    pub fn key_ring(&self) -> Option<&Arc<SecretKeyRing>> {
        self.key_ring.as_ref()
    }

    pub fn public_base(&self) -> &Url {
        &self.public_base
    }

    pub fn endpoint_urls(&self) -> &Arc<dyn EndpointUrls> {
        &self.endpoints
    }

    /// The realm called `name` (enabled or not; callers check `spec.enabled`).
    pub fn realm(&self, name: &str) -> Option<Arc<RealmRuntime>> {
        self.snap.load().by_name.get(name).cloned()
    }

    pub fn realms(&self) -> Vec<Arc<RealmRuntime>> {
        self.snap.load().by_name.values().cloned().collect()
    }

    /// Rebuilds the snapshot from the store.
    pub async fn reload(&self) -> Result<(), IdpError> {
        let _g = self.reload_lock.lock().await;
        let epoch = self.store.config_epoch().await.unwrap_or(None);
        let now = Utc::now();
        let old = self.snap.load_full();
        let mut by_name = HashMap::new();
        for rec in self.store.list_realms().await? {
            let keys = self.store.list_keys(&rec.id).await?;
            let mut signers = HashMap::new();
            let mut key_problem = None;
            // Reuse already opened keys (opening RSA keys is not free).
            let previous = old.by_name.get(rec.spec.name.as_str());
            for k in keys.iter().filter(|k| k.status == KeyStatus::Active) {
                if let Some(p) = previous.and_then(|p| p.signers.get(&k.alg))
                    && p.kid() == k.kid
                {
                    signers.insert(k.alg, p.clone());
                    continue;
                }
                let Some(ring) = &self.key_ring else {
                    key_problem = Some("HFS_SECRET_KEYS is not configured".to_string());
                    continue;
                };
                match self.store.get_sealed_key(&rec.id, &k.kid).await? {
                    Some(sealed) => match SigningKey::open(ring, &sealed, &rec.id, &k.kid, k.alg) {
                        Ok(sk) => {
                            signers.insert(k.alg, Arc::new(sk));
                        }
                        Err(e) => {
                            tracing::error!(realm = %rec.spec.name.as_str(), kid = %k.kid, error = %e, "cannot open IdP signing key");
                            key_problem = Some(format!("signing key {} cannot be opened", k.kid));
                        }
                    },
                    None => {
                        key_problem = Some(format!("signing key {} has no private part", k.kid))
                    }
                }
            }
            if signers.is_empty() && key_problem.is_none() {
                key_problem = Some("no active signing key".to_string());
            }
            let published: Vec<KeyRecord> =
                keys.into_iter().filter(|k| k.status.published()).collect();
            let revoked_grants: HashSet<String> = self
                .store
                .revoked_grant_ids(&rec.id, now)
                .await?
                .into_iter()
                .collect();
            let endpoints = rec
                .spec
                .endpoints
                .iter()
                .filter_map(|e| {
                    self.endpoints
                        .base_url(e)
                        .map(|b| (e.clone(), norm(&b).to_string()))
                })
                .collect();
            let issuer = rec.spec.issuer(&self.public_base);
            let rt = RealmRuntime {
                id: rec.id.clone(),
                urls: RealmUrls::new(&issuer),
                spec: rec.spec.clone(),
                revision: rec.revision,
                signers,
                published,
                revoked_grants: std::sync::RwLock::new(revoked_grants),
                endpoints,
                key_problem,
            };
            by_name.insert(rec.spec.name.as_str().to_string(), Arc::new(rt));
        }
        self.snap.store(Arc::new(Snapshot { by_name, epoch }));
        Ok(())
    }

    /// Reloads whenever the store's configuration epoch moves (polled every
    /// `every`; stores without an epoch reload on every tick).
    pub fn spawn_watcher(self: &Arc<Self>, every: Duration) -> tokio::task::JoinHandle<()> {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let current = me.store.config_epoch().await.unwrap_or(None);
                let seen = me.snap.load().epoch;
                if current.is_none() || current != seen {
                    if let Err(e) = me.reload().await {
                        tracing::warn!(error = %e, "IdP runtime reload failed");
                    }
                }
            }
        })
    }
}
