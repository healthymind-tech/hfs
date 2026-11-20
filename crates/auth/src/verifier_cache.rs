//! Shared token verifiers for FHIR endpoints configured in the FHIR server
//! manager (`docs/design/fhir-server-manager.md` §6, D-8).
//!
//! Every endpoint whose auth section is set gets its own
//! [`JwksBearerAuthProvider`] (its issuer, audience, algorithms and clock
//! skew), but the expensive part — the JWKS fetcher with its key cache and
//! background refresher — is shared by every endpoint naming the same
//! [`VerifierKey`] (issuer, JWKS URL, JWKS max age, whether the URL is
//! policy-guarded). Two endpoints on one IdP therefore fetch its keys once,
//! while each still checks its own `aud`, so a token minted for one endpoint
//! is refused by the other.
//!
//! [`VerifierCache::retain`] drops fetchers no endpoint uses any more (called
//! on every endpoint snapshot change); dropping a [`JwksCache`] stops its
//! background refresher.
//!
//! IdP-down behaviour is that of [`JwksCache::start`]: a new fetcher is
//! started in the background, tokens are answered `503` while the keys
//! cannot be fetched, and the refresher keeps retrying.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use crate::config::{AuthConfig, redact_url};
use crate::discovery::OutboundGuard;
use crate::error::AuthError;
use crate::jwks::JwksCache;
use crate::principal::Principal;
use crate::provider::AuthProvider;
use crate::provider::jwks_bearer::JwksBearerAuthProvider;

/// What makes two endpoints share one JWKS fetcher.
///
/// Algorithms, clock skew and audience are not part of it: they are checks
/// of the per-endpoint provider over the shared keys.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VerifierKey {
    /// Exact `iss`.
    pub issuer: String,
    /// The JWKS URL fetched.
    pub jwks_url: String,
    /// Longest a fetched key set is trusted, seconds.
    pub jwks_max_age_secs: u64,
    /// Fetched through the outbound policy (console-edited endpoint) or not
    /// (operator-configured).
    pub guarded: bool,
}

impl VerifierKey {
    /// The key of an enabled configuration; `None` when auth is off or the
    /// JWKS URL is missing.
    pub fn of(config: &AuthConfig, guard: &OutboundGuard) -> Option<Self> {
        if !config.enabled {
            return None;
        }
        let jwks_url = config.jwks_url.as_deref()?.trim();
        if jwks_url.is_empty() {
            return None;
        }
        Some(Self {
            issuer: config.expected_issuer.clone().unwrap_or_default(),
            jwks_url: jwks_url.to_string(),
            jwks_max_age_secs: config.jwks_max_age_secs,
            guarded: matches!(guard, OutboundGuard::Policy(_)),
        })
    }
}

/// The shared JWKS fetchers, one per [`VerifierKey`].
#[derive(Default)]
pub struct VerifierCache {
    fetchers: Mutex<HashMap<VerifierKey, Arc<JwksCache>>>,
}

impl std::fmt::Debug for VerifierCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifierCache")
            .field("fetchers", &self.fetcher_count())
            .finish()
    }
}

impl VerifierCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// The token verifier of one endpoint: its own provider (issuer,
    /// audience, algorithms, skew, claim names from `config`) over the
    /// fetcher shared by every endpoint with the same [`VerifierKey`].
    ///
    /// A JWKS URL the outbound policy refuses yields a verifier that answers
    /// every token with [`AuthError::JwksFetchError`] (the REST layer turns
    /// it into `503`), so a misconfigured endpoint fails closed.
    ///
    /// Must be called within a Tokio runtime when the fetcher is new (its
    /// first fetch and background refresher are spawned).
    pub fn provider(&self, config: &AuthConfig, guard: &OutboundGuard) -> Arc<dyn AuthProvider> {
        let Some(key) = VerifierKey::of(config, guard) else {
            return Arc::new(RefusingProvider(
                "the endpoint's auth configuration has no JWKS URL".to_string(),
            ));
        };
        if let Err(why) = guard.check_url(&key.jwks_url) {
            tracing::warn!(error = %why, "endpoint JWKS URL refused");
            return Arc::new(RefusingProvider(why));
        }
        let jwks = self.fetcher(&key, config, guard);
        Arc::new(JwksBearerAuthProvider::new(jwks, config))
    }

    fn fetcher(
        &self,
        key: &VerifierKey,
        config: &AuthConfig,
        guard: &OutboundGuard,
    ) -> Arc<JwksCache> {
        let mut fetchers = self.fetchers.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(existing) = fetchers.get(key) {
            return Arc::clone(existing);
        }
        let cache = Arc::new(
            JwksCache::new(&key.jwks_url, config.jwks_min_refresh_interval)
                .with_max_ttl(key.jwks_max_age_secs)
                .with_http_client(guard.client(Duration::from_secs(10))),
        );
        tracing::info!(
            jwks_url = %redact_url(&key.jwks_url),
            issuer = %key.issuer,
            "starting a shared JWKS fetcher for endpoint auth"
        );
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let starting = Arc::clone(&cache);
            rt.spawn(async move { starting.start().await });
        }
        fetchers.insert(key.clone(), Arc::clone(&cache));
        cache
    }

    /// Keeps only the fetchers in `live` (the keys of the current endpoint
    /// snapshot). Providers still holding a dropped fetcher keep working
    /// until they are dropped themselves; the background refresher stops
    /// with the last reference.
    pub fn retain(&self, live: &HashSet<VerifierKey>) {
        let mut fetchers = self.fetchers.lock().unwrap_or_else(|p| p.into_inner());
        let before = fetchers.len();
        fetchers.retain(|k, _| live.contains(k));
        let dropped = before - fetchers.len();
        if dropped > 0 {
            tracing::info!(dropped, "dropped JWKS fetchers no endpoint uses");
        }
    }

    /// Number of distinct fetchers (one per IdP key set in use).
    pub fn fetcher_count(&self) -> usize {
        self.fetchers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    /// The fetcher for `key`, if one exists.
    pub fn fetcher_for(&self, key: &VerifierKey) -> Option<Arc<JwksCache>> {
        self.fetchers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(key)
            .cloned()
    }
}

/// Verifier of an endpoint whose keys cannot be fetched by configuration.
struct RefusingProvider(String);

#[async_trait]
impl AuthProvider for RefusingProvider {
    async fn authenticate(&self, _authorization_header: &str) -> Result<Principal, AuthError> {
        Err(AuthError::JwksFetchError(self.0.clone()))
    }

    fn name(&self) -> &str {
        "refusing"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_endpoints::OutboundPolicy;

    fn config(issuer: &str, jwks: &str, audience: &str) -> AuthConfig {
        AuthConfig {
            enabled: true,
            expected_issuer: Some(issuer.into()),
            jwks_url: Some(jwks.into()),
            expected_audience: Some(audience.into()),
            ..AuthConfig::default()
        }
    }

    #[tokio::test]
    async fn endpoints_on_one_idp_share_a_fetcher() {
        let cache = VerifierCache::new();
        let a = config(
            "https://idp.example/realms/a",
            "http://127.0.0.1:9/certs",
            "api-a",
        );
        let b = config(
            "https://idp.example/realms/a",
            "http://127.0.0.1:9/certs",
            "api-b",
        );
        let c = config(
            "https://idp.example/realms/c",
            "http://127.0.0.1:9/c-certs",
            "api-c",
        );
        let _pa = cache.provider(&a, &OutboundGuard::Trusted);
        let _pb = cache.provider(&b, &OutboundGuard::Trusted);
        assert_eq!(cache.fetcher_count(), 1, "same issuer + JWKS: one fetcher");
        let _pc = cache.provider(&c, &OutboundGuard::Trusted);
        assert_eq!(cache.fetcher_count(), 2);

        let key_a = VerifierKey::of(&a, &OutboundGuard::Trusted).unwrap();
        let key_c = VerifierKey::of(&c, &OutboundGuard::Trusted).unwrap();
        assert_eq!(key_a, VerifierKey::of(&b, &OutboundGuard::Trusted).unwrap());
        cache.retain(&HashSet::from([key_c.clone()]));
        assert_eq!(cache.fetcher_count(), 1);
        assert!(cache.fetcher_for(&key_a).is_none());
        assert!(cache.fetcher_for(&key_c).is_some());
    }

    #[tokio::test]
    async fn a_guarded_and_a_trusted_endpoint_do_not_share() {
        let cache = VerifierCache::new();
        let policy = OutboundGuard::Policy(Arc::new(OutboundPolicy::parse("127.0.0.1")));
        let a = config("https://idp", "http://127.0.0.1:9/certs", "a");
        let _ = cache.provider(&a, &OutboundGuard::Trusted);
        let _ = cache.provider(&a, &policy);
        assert_eq!(cache.fetcher_count(), 2);
    }

    #[tokio::test]
    async fn a_refused_jwks_url_fails_closed() {
        let cache = VerifierCache::new();
        let strict = OutboundGuard::Policy(Arc::new(OutboundPolicy::strict()));
        let p = cache.provider(
            &config("https://idp", "http://10.0.0.5/certs", "a"),
            &strict,
        );
        assert_eq!(cache.fetcher_count(), 0, "no fetcher for a refused URL");
        match p.authenticate("Bearer eyJ.x.y").await {
            Err(AuthError::JwksFetchError(why)) => {
                assert!(why.contains("outbound policy"), "{why}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        let off = AuthConfig::default();
        assert!(VerifierKey::of(&off, &OutboundGuard::Trusted).is_none());
        let p = cache.provider(&off, &OutboundGuard::Trusted);
        assert!(p.authenticate("Bearer eyJ.x.y").await.is_err());
    }
}
