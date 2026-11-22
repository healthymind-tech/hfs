//! Store contract (design §9). The PostgreSQL implementation (package I2)
//! lives behind feature `postgres`; tests and single-node SQLite deployments
//! use the in-memory parts. Every mutating call that consumes a one-time
//! artefact (code, launch handle, client-assertion `jti`, PAR request URI,
//! refresh generation) is a single atomic statement — `DELETE … RETURNING`
//! or `UPDATE … WHERE current = $n` — so the cluster needs no extra locks.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::client::ClientSpec;
use crate::error::IdpError;
use crate::keys::{KeyRecord, KeyStatus};
use crate::launch::{LaunchContext, LaunchHandle};
use crate::realm::RealmSpec;
use crate::token::RefreshFamily;

/// A stored realm row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RealmRecord {
    pub id: String,
    pub spec: RealmSpec,
    pub revision: i64,
}

/// A stored authorization code (looked up by hash; consumed once).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationCode {
    pub code_hash: String,
    pub realm_id: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub subject: String,
    pub scopes: Vec<String>,
    pub audience: Vec<String>,
    pub nonce: Option<String>,
    pub auth_time: DateTime<Utc>,
    pub sso_session_id: Option<String>,
    pub context: LaunchContext,
    pub dpop_jkt: Option<String>,
    pub expires_at: DateTime<Utc>,
}

/// Persistent store of one HFS deployment's realms.
#[async_trait]
pub trait IdpStore: Send + Sync + 'static {
    // Realms
    async fn list_realms(&self) -> Result<Vec<RealmRecord>, IdpError>;
    async fn put_realm(
        &self,
        rec: RealmRecord,
        expected_revision: Option<i64>,
    ) -> Result<RealmRecord, IdpError>;

    // Signing keys (sealed private part stored alongside the record).
    async fn list_keys(&self, realm_id: &str) -> Result<Vec<KeyRecord>, IdpError>;
    async fn set_key_status(
        &self,
        realm_id: &str,
        kid: &str,
        from: KeyStatus,
        to: KeyStatus,
    ) -> Result<bool, IdpError>;

    // Clients
    async fn get_client(
        &self,
        realm_id: &str,
        client_id: &str,
    ) -> Result<Option<ClientSpec>, IdpError>;

    // One-time artefacts
    async fn put_code(&self, code: AuthorizationCode) -> Result<(), IdpError>;
    /// Atomically removes and returns the code. A second call returns
    /// `None`; the caller then revokes tokens of the grant (RFC 6749 §4.1.2).
    async fn take_code(
        &self,
        realm_id: &str,
        code_hash: &str,
    ) -> Result<Option<AuthorizationCode>, IdpError>;
    async fn put_launch(&self, h: LaunchHandle) -> Result<(), IdpError>;
    async fn take_launch(
        &self,
        realm_id: &str,
        handle_hash: &str,
    ) -> Result<Option<LaunchHandle>, IdpError>;
    /// Records a client-assertion `jti`; `false` if already seen (replay).
    async fn record_jti(
        &self,
        realm_id: &str,
        client_id: &str,
        jti: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<bool, IdpError>;

    // Refresh families
    async fn get_family_by_token(
        &self,
        realm_id: &str,
        token_hash: &str,
    ) -> Result<Option<(RefreshFamily, u32)>, IdpError>;
    /// Compare-and-set rotation: succeeds only if the family is still at
    /// `from_generation`; stores `new_token_hash` for `from_generation + 1`.
    async fn rotate_family(
        &self,
        realm_id: &str,
        family_id: &str,
        from_generation: u32,
        new_token_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, IdpError>;
    async fn revoke_family(
        &self,
        realm_id: &str,
        family_id: &str,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<(), IdpError>;
}

/// In-memory replay cache for client-assertion `jti`s (single node and
/// tests; the PG store uses `hfs_idp_jti` with a unique key).
#[derive(Debug, Default)]
pub struct MemoryJtiCache {
    seen: Mutex<HashMap<(String, String, String), DateTime<Utc>>>,
}

impl MemoryJtiCache {
    /// `true` the first time a `(realm, client, jti)` is seen before it expires.
    pub fn record(
        &self,
        realm_id: &str,
        client_id: &str,
        jti: &str,
        expires_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> bool {
        let mut m = self.seen.lock().expect("poisoned");
        m.retain(|_, exp| *exp > now);
        let key = (realm_id.to_string(), client_id.to_string(), jti.to_string());
        if m.contains_key(&key) {
            return false;
        }
        m.insert(key, expires_at);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn jti_replay_refused_until_expiry() {
        let c = MemoryJtiCache::default();
        let now = Utc::now();
        assert!(c.record("r", "app", "j1", now + Duration::minutes(5), now));
        assert!(!c.record("r", "app", "j1", now + Duration::minutes(5), now));
        assert!(c.record("r", "other", "j1", now + Duration::minutes(5), now));
        assert!(c.record(
            "r",
            "app",
            "j1",
            now + Duration::minutes(15),
            now + Duration::minutes(6)
        ));
    }
}
