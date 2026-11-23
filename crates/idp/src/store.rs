//! Store contract (design §9). One trait, [`IdpStore`], covers every table of
//! `hm-0020-idp-core`. [`crate::memory::MemoryStore`] implements it for tests
//! and single-node development; [`crate::pg::PgStore`] (feature `postgres`,
//! package I2) is the production implementation.
//!
//! Every mutating call that consumes a one-time artefact (code, launch handle,
//! interaction, client-assertion `jti`, refresh generation) is a single atomic
//! operation — `DELETE … RETURNING`, `INSERT … ON CONFLICT DO NOTHING` or
//! `UPDATE … WHERE current = $n` — so a cluster needs no extra locks (D-11).
//!
//! Every lookup is scoped by `realm_id`: a handle minted in one realm is never
//! found from another (cross-realm token reuse, §10.4).

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use helios_endpoints::SealedSecret;
use serde::{Deserialize, Serialize};

use crate::client::ClientSpec;
use crate::error::IdpError;
use crate::interaction::PendingAuthorization;
use crate::keys::{KeyRecord, KeyStatus};
use crate::launch::{LaunchContext, LaunchHandle};
use crate::realm::RealmSpec;
use crate::token::RefreshFamily;
use crate::user::RealmUser;

/// A stored realm row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RealmRecord {
    pub id: String,
    pub spec: RealmSpec,
    pub revision: i64,
}

/// How a client was registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Registration {
    #[default]
    Admin,
    Dcr,
    Udap,
}

impl Registration {
    pub fn as_str(self) -> &'static str {
        match self {
            Registration::Admin => "admin",
            Registration::Dcr => "dcr",
            Registration::Udap => "udap",
        }
    }
}

/// A stored client row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientRecord {
    pub spec: ClientSpec,
    pub registration: Registration,
    pub revision: i64,
}

/// A stored authorization code (looked up by hash; consumed once).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationCode {
    pub code_hash: String,
    pub realm_id: String,
    /// The grant the code belongs to (`gid` of the tokens it yields; revoked
    /// when the code is replayed).
    pub grant_id: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub subject: String,
    pub scopes: Vec<String>,
    pub audience: Vec<String>,
    /// Endpoint name the audience belongs to.
    pub endpoint: Option<String>,
    pub nonce: Option<String>,
    pub auth_time: DateTime<Utc>,
    pub sso_session_id: Option<String>,
    /// OIDC `sid` of the SSO session.
    #[serde(default)]
    pub sid: Option<String>,
    #[serde(default)]
    pub amr: Vec<String>,
    #[serde(default)]
    pub acr: Option<String>,
    #[serde(default)]
    pub fhir_user: Option<String>,
    pub context: LaunchContext,
    pub dpop_jkt: Option<String>,
    pub expires_at: DateTime<Utc>,
}

/// Outcome of [`IdpStore::take_code`].
#[derive(Debug, Clone, PartialEq)]
pub enum CodeTake {
    /// First (and only) redemption.
    Fresh(Box<AuthorizationCode>),
    /// The code was already redeemed: the caller revokes `grant_id`
    /// (RFC 6749 §4.1.2, RFC 9700 §4.2.4).
    Replayed { grant_id: String },
    /// Never issued, expired, or from another realm.
    Unknown,
}

/// A browser SSO session at a realm (`hfs_idp_sessions`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SsoSession {
    /// SHA-256 of the cookie value (the value itself is never stored).
    pub id_hash: String,
    pub realm_id: String,
    pub user_id: String,
    /// OIDC `sid` claim (random, not the cookie).
    pub sid: String,
    pub auth_time: DateTime<Utc>,
    #[serde(default)]
    pub amr: Vec<String>,
    pub acr: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl SsoSession {
    /// Alive at `now` given the realm's idle timeout.
    pub fn alive(&self, idle_secs: u32, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none()
            && now < self.expires_at
            && now < self.last_seen_at + chrono::Duration::seconds(idle_secs as i64)
    }
}

/// Remembered consent for (user, client, endpoint).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Consent {
    pub realm_id: String,
    pub user_id: String,
    pub client_id: String,
    pub endpoint: String,
    pub scopes: Vec<String>,
    pub granted_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// An authorization grant (`gid`): the unit of revocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantRecord {
    pub id: String,
    pub realm_id: String,
    pub client_id: String,
    pub subject: String,
    pub scopes: Vec<String>,
    pub audience: Vec<String>,
    pub context: LaunchContext,
    #[serde(default)]
    pub fhir_user: Option<String>,
    #[serde(default)]
    pub sso_session_id: Option<String>,
    #[serde(default)]
    pub auth_time: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// Last access/refresh token expiry (extended on refresh).
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub revoked_reason: Option<String>,
}

impl GrantRecord {
    pub fn active(&self, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none() && now < self.expires_at
    }
}

/// Filter for [`IdpStore::list_grants`].
#[derive(Debug, Clone, Default)]
pub struct GrantFilter {
    pub subject: Option<String>,
    pub client_id: Option<String>,
    pub include_revoked: bool,
    pub limit: Option<usize>,
}

/// Persistent store of one HFS deployment's realms.
#[async_trait]
pub trait IdpStore: Send + Sync + 'static {
    // ---- Realms
    async fn list_realms(&self) -> Result<Vec<RealmRecord>, IdpError>;
    async fn get_realm(&self, realm_id: &str) -> Result<Option<RealmRecord>, IdpError>;
    async fn get_realm_by_name(&self, name: &str) -> Result<Option<RealmRecord>, IdpError>;
    /// Creates (`expected_revision = None`, id must be new and the name free)
    /// or updates (revision must match; the stored revision is incremented).
    async fn put_realm(
        &self,
        rec: RealmRecord,
        expected_revision: Option<i64>,
    ) -> Result<RealmRecord, IdpError>;
    /// Soft-deletes the realm and frees its name; its rows stay until purge.
    async fn delete_realm(&self, realm_id: &str) -> Result<bool, IdpError>;

    // ---- Signing keys (sealed private part stored alongside the record)
    async fn list_keys(&self, realm_id: &str) -> Result<Vec<KeyRecord>, IdpError>;
    async fn insert_key(
        &self,
        realm_id: &str,
        rec: KeyRecord,
        sealed: SealedSecret,
    ) -> Result<(), IdpError>;
    async fn get_sealed_key(
        &self,
        realm_id: &str,
        kid: &str,
    ) -> Result<Option<SealedSecret>, IdpError>;
    /// Compare-and-set status change. `at` stamps `activate_at` (→ active) or
    /// `retire_at` (→ retiring/retired); `expire_at` is stored when given.
    async fn set_key_status(
        &self,
        realm_id: &str,
        kid: &str,
        from: KeyStatus,
        to: KeyStatus,
        at: DateTime<Utc>,
        expire_at: Option<DateTime<Utc>>,
    ) -> Result<bool, IdpError>;

    // ---- Clients
    async fn list_clients(&self, realm_id: &str) -> Result<Vec<ClientRecord>, IdpError>;
    async fn get_client(
        &self,
        realm_id: &str,
        client_id: &str,
    ) -> Result<Option<ClientSpec>, IdpError>;
    async fn get_client_record(
        &self,
        realm_id: &str,
        client_id: &str,
    ) -> Result<Option<ClientRecord>, IdpError>;
    /// Create (`expected_revision = None`, must not exist) or update.
    async fn put_client(
        &self,
        realm_id: &str,
        rec: ClientRecord,
        expected_revision: Option<i64>,
    ) -> Result<ClientRecord, IdpError>;
    async fn delete_client(&self, realm_id: &str, client_id: &str) -> Result<bool, IdpError>;

    // ---- Users and credentials
    async fn list_users(&self, realm_id: &str) -> Result<Vec<RealmUser>, IdpError>;
    async fn get_user(&self, realm_id: &str, user_id: &str) -> Result<Option<RealmUser>, IdpError>;
    /// Case-insensitive username lookup.
    async fn find_user_by_username(
        &self,
        realm_id: &str,
        username: &str,
    ) -> Result<Option<RealmUser>, IdpError>;
    async fn find_user_by_external_subject(
        &self,
        realm_id: &str,
        external_subject: &str,
    ) -> Result<Option<RealmUser>, IdpError>;
    /// Insert or replace (links replaced as a whole).
    async fn put_user(&self, realm_id: &str, user: RealmUser) -> Result<(), IdpError>;
    async fn delete_user(&self, realm_id: &str, user_id: &str) -> Result<bool, IdpError>;
    /// Stores the argon2id PHC hash of the user's password (replacing any).
    async fn set_password_hash(
        &self,
        realm_id: &str,
        user_id: &str,
        phc: &str,
        must_change: bool,
    ) -> Result<(), IdpError>;
    /// The password hash and the must-change flag.
    async fn get_password_hash(
        &self,
        realm_id: &str,
        user_id: &str,
    ) -> Result<Option<(String, bool)>, IdpError>;
    async fn record_login(
        &self,
        realm_id: &str,
        user_id: &str,
        at: DateTime<Utc>,
    ) -> Result<(), IdpError>;

    // ---- SSO sessions
    async fn put_session(&self, s: SsoSession) -> Result<(), IdpError>;
    async fn get_session(
        &self,
        realm_id: &str,
        id_hash: &str,
    ) -> Result<Option<SsoSession>, IdpError>;
    async fn get_session_by_sid(
        &self,
        realm_id: &str,
        sid: &str,
    ) -> Result<Option<SsoSession>, IdpError>;
    async fn touch_session(
        &self,
        realm_id: &str,
        id_hash: &str,
        at: DateTime<Utc>,
    ) -> Result<(), IdpError>;
    /// Revokes by cookie digest; `online_access` refresh families of the
    /// session die with it.
    async fn revoke_session(
        &self,
        realm_id: &str,
        id_hash: &str,
        at: DateTime<Utc>,
    ) -> Result<bool, IdpError>;
    async fn list_sessions(
        &self,
        realm_id: &str,
        user_id: Option<&str>,
    ) -> Result<Vec<SsoSession>, IdpError>;

    // ---- Consents
    async fn get_consent(
        &self,
        realm_id: &str,
        user_id: &str,
        client_id: &str,
        endpoint: &str,
    ) -> Result<Option<Consent>, IdpError>;
    async fn put_consent(&self, c: Consent) -> Result<(), IdpError>;
    async fn delete_consent(
        &self,
        realm_id: &str,
        user_id: &str,
        client_id: &str,
        endpoint: &str,
    ) -> Result<bool, IdpError>;
    async fn list_consents(&self, realm_id: &str, user_id: &str) -> Result<Vec<Consent>, IdpError>;

    // ---- Grants
    async fn put_grant(&self, g: GrantRecord) -> Result<(), IdpError>;
    async fn get_grant(
        &self,
        realm_id: &str,
        grant_id: &str,
    ) -> Result<Option<GrantRecord>, IdpError>;
    /// Moves `expires_at` forward (never backwards) on refresh.
    async fn extend_grant(
        &self,
        realm_id: &str,
        grant_id: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<(), IdpError>;
    /// Revokes the grant and every refresh family of it. Idempotent.
    async fn revoke_grant(
        &self,
        realm_id: &str,
        grant_id: &str,
        reason: &str,
        at: DateTime<Utc>,
    ) -> Result<bool, IdpError>;
    /// Revokes every grant of the realm (emergency key rotation).
    async fn revoke_all_grants(
        &self,
        realm_id: &str,
        reason: &str,
        at: DateTime<Utc>,
    ) -> Result<u64, IdpError>;
    async fn list_grants(
        &self,
        realm_id: &str,
        filter: &GrantFilter,
    ) -> Result<Vec<GrantRecord>, IdpError>;
    /// Ids of revoked grants that have not expired yet (the internal
    /// verifier's deny-set).
    async fn revoked_grant_ids(
        &self,
        realm_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, IdpError>;

    // ---- One-time artefacts
    async fn put_code(&self, code: AuthorizationCode) -> Result<(), IdpError>;
    /// Atomically removes the code and leaves a tombstone, so a second
    /// presentation is reported as [`CodeTake::Replayed`]. Expired codes are
    /// [`CodeTake::Unknown`].
    async fn take_code(
        &self,
        realm_id: &str,
        code_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<CodeTake, IdpError>;
    async fn put_launch(&self, h: LaunchHandle) -> Result<(), IdpError>;
    /// Atomically removes and returns an unexpired launch handle.
    async fn take_launch(
        &self,
        realm_id: &str,
        handle_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<LaunchHandle>, IdpError>;
    /// Records a client-assertion `jti`; `false` if already seen and not yet
    /// expired (replay).
    async fn record_jti(
        &self,
        realm_id: &str,
        client_id: &str,
        jti: &str,
        expires_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<bool, IdpError>;

    // ---- Pending authorizations (between /authorize and the hosted pages)
    async fn put_interaction(&self, p: PendingAuthorization) -> Result<(), IdpError>;
    async fn get_interaction(
        &self,
        realm_id: &str,
        id_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<PendingAuthorization>, IdpError>;
    /// Replaces the stored payload (the pages record progress, e.g. the
    /// selected patient) — only while unexpired.
    async fn update_interaction(&self, p: PendingAuthorization) -> Result<bool, IdpError>;
    /// Atomically removes and returns it (completion is single-use).
    async fn take_interaction(
        &self,
        realm_id: &str,
        id_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<PendingAuthorization>, IdpError>;

    // ---- Refresh families
    /// Creates a family whose generation-0 token digest is `first_token_hash`.
    async fn create_family(
        &self,
        realm_id: &str,
        family: RefreshFamily,
        first_token_hash: &str,
    ) -> Result<(), IdpError>;
    /// The family a presented token digest belongs to and its generation.
    async fn get_family_by_token(
        &self,
        realm_id: &str,
        token_hash: &str,
    ) -> Result<Option<(RefreshFamily, u32)>, IdpError>;
    /// Compare-and-set rotation: succeeds only if the family is unrevoked
    /// and still at `from_generation`; stores `new_token_hash` for
    /// `from_generation + 1` and stamps `last_used_at`.
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

    // ---- Maintenance
    /// Deletes expired codes, tombstones, launches, interactions, `jti`s and
    /// sessions, and grants/families expired for more than a day. Returns
    /// the number of rows removed.
    async fn purge_expired(&self, now: DateTime<Utc>) -> Result<u64, IdpError>;

    /// The `idp` scope of `hfs_config_epochs` (moves on every configuration
    /// change and grant revocation, cluster-wide). `None` = no epoch support
    /// (the runtime then reloads on every watcher tick).
    async fn config_epoch(&self) -> Result<Option<i64>, IdpError> {
        Ok(None)
    }
}

/// In-memory replay cache for client-assertion `jti`s (kept for callers that
/// need a cache without a store; the stores implement [`IdpStore::record_jti`]).
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
