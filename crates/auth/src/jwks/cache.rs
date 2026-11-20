use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jsonwebtoken::DecodingKey;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use super::fetcher::JwksFetcher;
use crate::config::redact_url;
use crate::error::AuthError;

/// Default cache TTL when no Cache-Control header is present.
const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(3600);

/// Caches JWKS keys with Cache-Control awareness and background refresh.
pub struct JwksCache {
    keys: Arc<RwLock<HashMap<String, DecodingKey>>>,
    jwks_url: String,
    fetcher: JwksFetcher,
    expires_at: Arc<RwLock<Instant>>,
    last_refresh: Arc<RwLock<Instant>>,
    min_refresh_interval: Duration,
    max_ttl: Duration,
    /// The background refresher, aborted when the cache is dropped (a
    /// per-endpoint verifier that is no longer used must not keep polling
    /// its IdP forever).
    background: std::sync::Mutex<Option<tokio::task::AbortHandle>>,
}

impl Drop for JwksCache {
    fn drop(&mut self) {
        if let Some(handle) = self
            .background
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            handle.abort();
        }
    }
}

impl JwksCache {
    /// Create a new JWKS cache.
    ///
    /// Does not fetch keys — call `initial_fetch()` before use.
    pub fn new(jwks_url: &str, min_refresh_interval_secs: u64) -> Self {
        Self {
            keys: Arc::new(RwLock::new(HashMap::new())),
            jwks_url: jwks_url.to_string(),
            fetcher: JwksFetcher::new(),
            expires_at: Arc::new(RwLock::new(Instant::now())),
            last_refresh: Arc::new(RwLock::new(
                Instant::now() - Duration::from_secs(min_refresh_interval_secs + 1),
            )),
            min_refresh_interval: Duration::from_secs(min_refresh_interval_secs),
            max_ttl: DEFAULT_CACHE_TTL,
            background: std::sync::Mutex::new(None),
        }
    }

    /// Fetches through `client` instead of the default one (an outbound
    /// policy enforcing client for administrator-entered JWKS URLs).
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.fetcher = JwksFetcher::with_client(client);
        self
    }

    /// The JWKS URL this cache fetches.
    pub fn jwks_url(&self) -> &str {
        &self.jwks_url
    }

    /// Whether a key set has been fetched at least once and holds keys.
    pub async fn has_keys(&self) -> bool {
        !self.keys.read().await.is_empty()
    }

    /// Caps how long a fetched key set is trusted before it is refetched,
    /// whatever the IdP's `Cache-Control` says (or when it says nothing, as
    /// Keycloak does). This bounds how long a key the IdP has retired — or
    /// revoked after a compromise — keeps validating tokens here.
    pub fn with_max_ttl(mut self, max_ttl_secs: u64) -> Self {
        self.max_ttl = Duration::from_secs(max_ttl_secs.max(1));
        self
    }

    /// Perform the initial JWKS fetch. Must be called before serving requests.
    ///
    /// Also spawns a background task to refresh keys before expiry.
    pub async fn initial_fetch(&self) -> Result<(), AuthError> {
        // Redacted: a JWKS URL may carry userinfo or a query credential (#2149).
        info!(url = %redact_url(&self.jwks_url), "Performing initial JWKS fetch");
        self.refresh().await?;
        self.spawn_background_refresh();
        Ok(())
    }

    /// Start the cache without requiring the IdP to be reachable right now.
    ///
    /// Tries the initial fetch and, whatever its outcome, spawns the
    /// background refresher. When the IdP is down at startup the server still
    /// comes up: every token is refused (`401`/`503`) until a refresh — on the
    /// first unknown `kid`, or the background retry — reaches the JWKS. Use
    /// [`initial_fetch`](Self::initial_fetch) to fail fast instead.
    pub async fn start(&self) {
        info!(url = %redact_url(&self.jwks_url), "Performing initial JWKS fetch");
        if let Err(err) = self.refresh().await {
            warn!(
                url = %redact_url(&self.jwks_url),
                error = %err,
                "Initial JWKS fetch failed; starting anyway and retrying in the background"
            );
        }
        self.spawn_background_refresh();
    }

    /// Get the decoding key for a given key ID.
    ///
    /// If the `kid` is not found, triggers a rate-limited refresh and retries.
    pub async fn get_key(&self, kid: &str) -> Result<DecodingKey, AuthError> {
        // Try cached keys first
        {
            let keys = self.keys.read().await;
            if let Some(key) = keys.get(kid) {
                return Ok(key.clone());
            }
        }

        // Unknown kid — try refreshing (rate-limited)
        debug!(kid, "Key not found in cache, attempting refresh");
        self.try_refresh().await?;

        // Retry after refresh
        let keys = self.keys.read().await;
        keys.get(kid).cloned().ok_or_else(|| AuthError::UnknownKid {
            kid: kid.to_string(),
        })
    }

    /// Attempt a refresh, respecting the rate limit.
    async fn try_refresh(&self) -> Result<(), AuthError> {
        let now = Instant::now();
        {
            let last = self.last_refresh.read().await;
            if now.duration_since(*last) < self.min_refresh_interval {
                debug!("JWKS refresh rate-limited, skipping");
                return Ok(());
            }
        }
        self.refresh().await
    }

    /// Fetch keys from the JWKS endpoint and update the cache.
    async fn refresh(&self) -> Result<(), AuthError> {
        let response = self.fetcher.fetch(&self.jwks_url).await?;

        let ttl = response
            .max_age
            .unwrap_or(DEFAULT_CACHE_TTL)
            .min(self.max_ttl);

        {
            let mut keys = self.keys.write().await;
            *keys = response.keys;
        }
        {
            let mut expires = self.expires_at.write().await;
            *expires = Instant::now() + ttl;
        }
        {
            let mut last = self.last_refresh.write().await;
            *last = Instant::now();
        }

        info!(ttl_secs = ttl.as_secs(), "JWKS cache refreshed");
        Ok(())
    }

    /// Spawn a background task that refreshes keys before they expire.
    fn spawn_background_refresh(&self) {
        let keys = Arc::clone(&self.keys);
        let expires_at = Arc::clone(&self.expires_at);
        let last_refresh = Arc::clone(&self.last_refresh);
        let url = self.jwks_url.clone();
        let fetcher = self.fetcher.clone();
        let min_interval = self.min_refresh_interval;
        let max_ttl = self.max_ttl;

        let mut slot = self.background.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_some() {
            return;
        }
        let task = tokio::spawn(async move {
            loop {
                // Sleep until shortly before expiry
                let sleep_duration = {
                    let expires = expires_at.read().await;
                    let now = Instant::now();
                    if *expires > now {
                        let remaining = *expires - now;
                        // Refresh at 75% of TTL to avoid edge cases
                        remaining.mul_f64(0.75)
                    } else {
                        // Expired or never fetched: retry, but never spin.
                        min_interval.max(Duration::from_secs(5))
                    }
                };

                tokio::time::sleep(sleep_duration).await;

                debug!(url = %redact_url(&url), "Background JWKS refresh triggered");
                match fetcher.fetch(&url).await {
                    Ok(response) => {
                        let ttl = response.max_age.unwrap_or(DEFAULT_CACHE_TTL).min(max_ttl);
                        {
                            let mut k = keys.write().await;
                            *k = response.keys;
                        }
                        {
                            let mut e = expires_at.write().await;
                            *e = Instant::now() + ttl;
                        }
                        {
                            let mut l = last_refresh.write().await;
                            *l = Instant::now();
                        }
                        info!(ttl_secs = ttl.as_secs(), "Background JWKS refresh complete");
                    }
                    Err(e) => {
                        warn!(error = %e, "Background JWKS refresh failed, will retry");
                    }
                }
            }
        });
        *slot = Some(task.abort_handle());
    }
}
