//! In-memory [`IdpStore`] (tests, single-node development). Same semantics as
//! the PostgreSQL store, including atomic consumption and compare-and-set
//! rotation (one mutex guards everything).

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use helios_endpoints::SealedSecret;

use crate::client::ClientSpec;
use crate::error::IdpError;
use crate::interaction::PendingAuthorization;
use crate::keys::{KeyRecord, KeyStatus};
use crate::launch::LaunchHandle;
use crate::store::*;
use crate::token::RefreshFamily;
use crate::user::RealmUser;

#[derive(Default)]
struct Inner {
    realms: HashMap<String, (RealmRecord, bool)>, // (record, deleted)
    keys: HashMap<String, Vec<(KeyRecord, SealedSecret)>>,
    clients: HashMap<(String, String), ClientRecord>,
    users: HashMap<(String, String), RealmUser>,
    passwords: HashMap<(String, String), (String, bool)>,
    sessions: HashMap<(String, String), SsoSession>,
    consents: HashMap<(String, String, String, String), Consent>,
    grants: HashMap<(String, String), GrantRecord>,
    codes: HashMap<(String, String), AuthorizationCode>,
    tombstones: HashMap<(String, String), (String, DateTime<Utc>)>,
    launches: HashMap<(String, String), LaunchHandle>,
    jti: HashMap<(String, String, String), DateTime<Utc>>,
    interactions: HashMap<(String, String), PendingAuthorization>,
    families: HashMap<String, (String, RefreshFamily)>, // family_id -> (realm, family)
    refresh_tokens: HashMap<String, (String, u32)>,     // token_hash -> (family, gen)
}

/// In-memory store.
#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("idp memory store poisoned")
    }
}

fn k2(a: &str, b: &str) -> (String, String) {
    (a.to_string(), b.to_string())
}

fn revoke_families_of_grant(
    i: &mut Inner,
    realm_id: &str,
    grant_id: &str,
    reason: &str,
    at: DateTime<Utc>,
) {
    for (r, f) in i.families.values_mut() {
        if r == realm_id && f.grant_id == grant_id && f.revoked_at.is_none() {
            f.revoked_at = Some(at);
            f.revoked_reason = Some(reason.to_string());
        }
    }
}

#[async_trait]
impl IdpStore for MemoryStore {
    async fn list_realms(&self) -> Result<Vec<RealmRecord>, IdpError> {
        let mut v: Vec<RealmRecord> = self
            .lock()
            .realms
            .values()
            .filter(|(_, d)| !d)
            .map(|(r, _)| r.clone())
            .collect();
        v.sort_by(|a, b| a.spec.name.as_str().cmp(b.spec.name.as_str()));
        Ok(v)
    }

    async fn get_realm(&self, realm_id: &str) -> Result<Option<RealmRecord>, IdpError> {
        Ok(self
            .lock()
            .realms
            .get(realm_id)
            .filter(|(_, d)| !d)
            .map(|(r, _)| r.clone()))
    }

    async fn get_realm_by_name(&self, name: &str) -> Result<Option<RealmRecord>, IdpError> {
        Ok(self
            .lock()
            .realms
            .values()
            .find(|(r, d)| !d && r.spec.name.as_str() == name)
            .map(|(r, _)| r.clone()))
    }

    async fn put_realm(
        &self,
        mut rec: RealmRecord,
        expected_revision: Option<i64>,
    ) -> Result<RealmRecord, IdpError> {
        let mut i = self.lock();
        let name_taken = i
            .realms
            .values()
            .any(|(r, d)| !d && r.spec.name == rec.spec.name && r.id != rec.id);
        if name_taken {
            return Err(IdpError::Conflict(format!(
                "realm name {} is taken",
                rec.spec.name.as_str()
            )));
        }
        match expected_revision {
            None => {
                if i.realms.contains_key(&rec.id) {
                    return Err(IdpError::Conflict(format!("realm {} exists", rec.id)));
                }
                rec.revision = 1;
            }
            Some(rev) => {
                let Some((cur, false)) = i.realms.get(&rec.id) else {
                    return Err(IdpError::NotFound(format!("realm {}", rec.id)));
                };
                if cur.revision != rev {
                    return Err(IdpError::Conflict(format!(
                        "realm revision is {}, not {rev}",
                        cur.revision
                    )));
                }
                rec.revision = rev + 1;
            }
        }
        i.realms.insert(rec.id.clone(), (rec.clone(), false));
        Ok(rec)
    }

    async fn delete_realm(&self, realm_id: &str) -> Result<bool, IdpError> {
        let mut i = self.lock();
        match i.realms.get_mut(realm_id) {
            Some((_, d)) if !*d => {
                *d = true;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn list_keys(&self, realm_id: &str) -> Result<Vec<KeyRecord>, IdpError> {
        Ok(self
            .lock()
            .keys
            .get(realm_id)
            .map(|v| v.iter().map(|(k, _)| k.clone()).collect())
            .unwrap_or_default())
    }

    async fn insert_key(
        &self,
        realm_id: &str,
        rec: KeyRecord,
        sealed: SealedSecret,
    ) -> Result<(), IdpError> {
        let mut i = self.lock();
        let v = i.keys.entry(realm_id.to_string()).or_default();
        if v.iter().any(|(k, _)| k.kid == rec.kid) {
            return Err(IdpError::Conflict(format!("key {} exists", rec.kid)));
        }
        if rec.status == KeyStatus::Active
            && v.iter()
                .any(|(k, _)| k.alg == rec.alg && k.status == KeyStatus::Active)
        {
            return Err(IdpError::Conflict(format!(
                "an active {} key exists",
                rec.alg
            )));
        }
        v.push((rec, sealed));
        Ok(())
    }

    async fn get_sealed_key(
        &self,
        realm_id: &str,
        kid: &str,
    ) -> Result<Option<SealedSecret>, IdpError> {
        Ok(self
            .lock()
            .keys
            .get(realm_id)
            .and_then(|v| v.iter().find(|(k, _)| k.kid == kid).map(|(_, s)| s.clone())))
    }

    async fn set_key_status(
        &self,
        realm_id: &str,
        kid: &str,
        from: KeyStatus,
        to: KeyStatus,
        at: DateTime<Utc>,
        expire_at: Option<DateTime<Utc>>,
    ) -> Result<bool, IdpError> {
        let mut i = self.lock();
        let Some(v) = i.keys.get_mut(realm_id) else {
            return Ok(false);
        };
        let Some(pos) = v.iter().position(|(k, _)| k.kid == kid && k.status == from) else {
            return Ok(false);
        };
        let alg = v[pos].0.alg;
        if to == KeyStatus::Active
            && v.iter()
                .any(|(k, _)| k.alg == alg && k.status == KeyStatus::Active && k.kid != kid)
        {
            return Err(IdpError::Conflict(format!("an active {alg} key exists")));
        }
        let k = &mut v[pos].0;
        k.status = to;
        match to {
            KeyStatus::Active => k.activate_at = Some(at),
            KeyStatus::Retiring | KeyStatus::Retired => {
                if k.retire_at.is_none() {
                    k.retire_at = Some(at)
                }
            }
            KeyStatus::Next => {}
        }
        if expire_at.is_some() {
            k.expire_at = expire_at;
        }
        Ok(true)
    }

    async fn list_clients(&self, realm_id: &str) -> Result<Vec<ClientRecord>, IdpError> {
        let mut v: Vec<ClientRecord> = self
            .lock()
            .clients
            .iter()
            .filter(|((r, _), _)| r == realm_id)
            .map(|(_, c)| c.clone())
            .collect();
        v.sort_by(|a, b| a.spec.client_id.cmp(&b.spec.client_id));
        Ok(v)
    }

    async fn get_client(
        &self,
        realm_id: &str,
        client_id: &str,
    ) -> Result<Option<ClientSpec>, IdpError> {
        Ok(self
            .lock()
            .clients
            .get(&k2(realm_id, client_id))
            .map(|c| c.spec.clone()))
    }

    async fn get_client_record(
        &self,
        realm_id: &str,
        client_id: &str,
    ) -> Result<Option<ClientRecord>, IdpError> {
        Ok(self.lock().clients.get(&k2(realm_id, client_id)).cloned())
    }

    async fn put_client(
        &self,
        realm_id: &str,
        mut rec: ClientRecord,
        expected_revision: Option<i64>,
    ) -> Result<ClientRecord, IdpError> {
        let mut i = self.lock();
        let key = k2(realm_id, &rec.spec.client_id);
        match (expected_revision, i.clients.get(&key)) {
            (None, Some(_)) => {
                return Err(IdpError::Conflict(format!(
                    "client {} exists",
                    rec.spec.client_id
                )));
            }
            (None, None) => rec.revision = 1,
            (Some(_), None) => {
                return Err(IdpError::NotFound(format!("client {}", rec.spec.client_id)));
            }
            (Some(rev), Some(cur)) => {
                if cur.revision != rev {
                    return Err(IdpError::Conflict(format!(
                        "client revision is {}, not {rev}",
                        cur.revision
                    )));
                }
                rec.revision = rev + 1;
            }
        }
        i.clients.insert(key, rec.clone());
        Ok(rec)
    }

    async fn delete_client(&self, realm_id: &str, client_id: &str) -> Result<bool, IdpError> {
        Ok(self
            .lock()
            .clients
            .remove(&k2(realm_id, client_id))
            .is_some())
    }

    async fn list_users(&self, realm_id: &str) -> Result<Vec<RealmUser>, IdpError> {
        let mut v: Vec<RealmUser> = self
            .lock()
            .users
            .iter()
            .filter(|((r, _), _)| r == realm_id)
            .map(|(_, u)| u.clone())
            .collect();
        v.sort_by(|a, b| a.username.cmp(&b.username));
        Ok(v)
    }

    async fn get_user(&self, realm_id: &str, user_id: &str) -> Result<Option<RealmUser>, IdpError> {
        Ok(self.lock().users.get(&k2(realm_id, user_id)).cloned())
    }

    async fn find_user_by_username(
        &self,
        realm_id: &str,
        username: &str,
    ) -> Result<Option<RealmUser>, IdpError> {
        let lower = username.to_lowercase();
        Ok(self
            .lock()
            .users
            .iter()
            .find(|((r, _), u)| r == realm_id && u.username.to_lowercase() == lower)
            .map(|(_, u)| u.clone()))
    }

    async fn find_user_by_external_subject(
        &self,
        realm_id: &str,
        external_subject: &str,
    ) -> Result<Option<RealmUser>, IdpError> {
        Ok(self
            .lock()
            .users
            .iter()
            .find(|((r, _), u)| {
                r == realm_id && u.external_subject.as_deref() == Some(external_subject)
            })
            .map(|(_, u)| u.clone()))
    }

    async fn put_user(&self, realm_id: &str, user: RealmUser) -> Result<(), IdpError> {
        let mut i = self.lock();
        let lower = user.username.to_lowercase();
        if i.users.iter().any(|((r, id), u)| {
            r == realm_id && *id != user.id && u.username.to_lowercase() == lower
        }) {
            return Err(IdpError::Conflict(format!(
                "username {} is taken",
                user.username
            )));
        }
        i.users.insert(k2(realm_id, &user.id), user);
        Ok(())
    }

    async fn delete_user(&self, realm_id: &str, user_id: &str) -> Result<bool, IdpError> {
        let mut i = self.lock();
        i.passwords.remove(&k2(realm_id, user_id));
        i.sessions
            .retain(|(r, _), s| !(r == realm_id && s.user_id == user_id));
        i.consents
            .retain(|(r, u, _, _), _| !(r == realm_id && u == user_id));
        Ok(i.users.remove(&k2(realm_id, user_id)).is_some())
    }

    async fn set_password_hash(
        &self,
        realm_id: &str,
        user_id: &str,
        phc: &str,
        must_change: bool,
    ) -> Result<(), IdpError> {
        let mut i = self.lock();
        if !i.users.contains_key(&k2(realm_id, user_id)) {
            return Err(IdpError::NotFound(format!("user {user_id}")));
        }
        i.passwords
            .insert(k2(realm_id, user_id), (phc.to_string(), must_change));
        Ok(())
    }

    async fn get_password_hash(
        &self,
        realm_id: &str,
        user_id: &str,
    ) -> Result<Option<(String, bool)>, IdpError> {
        Ok(self.lock().passwords.get(&k2(realm_id, user_id)).cloned())
    }

    async fn record_login(
        &self,
        _realm_id: &str,
        _user_id: &str,
        _at: DateTime<Utc>,
    ) -> Result<(), IdpError> {
        Ok(())
    }

    async fn put_session(&self, s: SsoSession) -> Result<(), IdpError> {
        self.lock().sessions.insert(k2(&s.realm_id, &s.id_hash), s);
        Ok(())
    }

    async fn get_session(
        &self,
        realm_id: &str,
        id_hash: &str,
    ) -> Result<Option<SsoSession>, IdpError> {
        Ok(self.lock().sessions.get(&k2(realm_id, id_hash)).cloned())
    }

    async fn get_session_by_sid(
        &self,
        realm_id: &str,
        sid: &str,
    ) -> Result<Option<SsoSession>, IdpError> {
        Ok(self
            .lock()
            .sessions
            .values()
            .find(|s| s.realm_id == realm_id && s.sid == sid)
            .cloned())
    }

    async fn touch_session(
        &self,
        realm_id: &str,
        id_hash: &str,
        at: DateTime<Utc>,
    ) -> Result<(), IdpError> {
        if let Some(s) = self.lock().sessions.get_mut(&k2(realm_id, id_hash)) {
            s.last_seen_at = at;
        }
        Ok(())
    }

    async fn revoke_session(
        &self,
        realm_id: &str,
        id_hash: &str,
        at: DateTime<Utc>,
    ) -> Result<bool, IdpError> {
        let mut i = self.lock();
        let Some(s) = i.sessions.get_mut(&k2(realm_id, id_hash)) else {
            return Ok(false);
        };
        if s.revoked_at.is_some() {
            return Ok(false);
        }
        s.revoked_at = Some(at);
        for (r, f) in i.families.values_mut() {
            if r == realm_id
                && f.sso_session_id.as_deref() == Some(id_hash)
                && f.revoked_at.is_none()
            {
                f.revoked_at = Some(at);
                f.revoked_reason = Some("session ended".into());
            }
        }
        Ok(true)
    }

    async fn list_sessions(
        &self,
        realm_id: &str,
        user_id: Option<&str>,
    ) -> Result<Vec<SsoSession>, IdpError> {
        let mut v: Vec<SsoSession> = self
            .lock()
            .sessions
            .values()
            .filter(|s| s.realm_id == realm_id && user_id.is_none_or(|u| s.user_id == u))
            .cloned()
            .collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        Ok(v)
    }

    async fn get_consent(
        &self,
        realm_id: &str,
        user_id: &str,
        client_id: &str,
        endpoint: &str,
    ) -> Result<Option<Consent>, IdpError> {
        Ok(self
            .lock()
            .consents
            .get(&(
                realm_id.into(),
                user_id.into(),
                client_id.into(),
                endpoint.into(),
            ))
            .cloned())
    }

    async fn put_consent(&self, c: Consent) -> Result<(), IdpError> {
        self.lock().consents.insert(
            (
                c.realm_id.clone(),
                c.user_id.clone(),
                c.client_id.clone(),
                c.endpoint.clone(),
            ),
            c,
        );
        Ok(())
    }

    async fn delete_consent(
        &self,
        realm_id: &str,
        user_id: &str,
        client_id: &str,
        endpoint: &str,
    ) -> Result<bool, IdpError> {
        Ok(self
            .lock()
            .consents
            .remove(&(
                realm_id.into(),
                user_id.into(),
                client_id.into(),
                endpoint.into(),
            ))
            .is_some())
    }

    async fn list_consents(&self, realm_id: &str, user_id: &str) -> Result<Vec<Consent>, IdpError> {
        Ok(self
            .lock()
            .consents
            .values()
            .filter(|c| c.realm_id == realm_id && c.user_id == user_id)
            .cloned()
            .collect())
    }

    async fn put_grant(&self, g: GrantRecord) -> Result<(), IdpError> {
        let mut i = self.lock();
        let key = k2(&g.realm_id, &g.id);
        if i.grants.contains_key(&key) {
            return Err(IdpError::Conflict(format!("grant {} exists", g.id)));
        }
        i.grants.insert(key, g);
        Ok(())
    }

    async fn get_grant(
        &self,
        realm_id: &str,
        grant_id: &str,
    ) -> Result<Option<GrantRecord>, IdpError> {
        Ok(self.lock().grants.get(&k2(realm_id, grant_id)).cloned())
    }

    async fn extend_grant(
        &self,
        realm_id: &str,
        grant_id: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<(), IdpError> {
        if let Some(g) = self.lock().grants.get_mut(&k2(realm_id, grant_id))
            && expires_at > g.expires_at
        {
            g.expires_at = expires_at;
        }
        Ok(())
    }

    async fn revoke_grant(
        &self,
        realm_id: &str,
        grant_id: &str,
        reason: &str,
        at: DateTime<Utc>,
    ) -> Result<bool, IdpError> {
        let mut i = self.lock();
        let changed = match i.grants.get_mut(&k2(realm_id, grant_id)) {
            Some(g) if g.revoked_at.is_none() => {
                g.revoked_at = Some(at);
                g.revoked_reason = Some(reason.to_string());
                true
            }
            _ => false,
        };
        revoke_families_of_grant(&mut i, realm_id, grant_id, reason, at);
        Ok(changed)
    }

    async fn revoke_all_grants(
        &self,
        realm_id: &str,
        reason: &str,
        at: DateTime<Utc>,
    ) -> Result<u64, IdpError> {
        let mut i = self.lock();
        let mut n = 0;
        for ((r, _), g) in i.grants.iter_mut() {
            if r == realm_id && g.revoked_at.is_none() {
                g.revoked_at = Some(at);
                g.revoked_reason = Some(reason.to_string());
                n += 1;
            }
        }
        for (r, f) in i.families.values_mut() {
            if r == realm_id && f.revoked_at.is_none() {
                f.revoked_at = Some(at);
                f.revoked_reason = Some(reason.to_string());
            }
        }
        Ok(n)
    }

    async fn list_grants(
        &self,
        realm_id: &str,
        filter: &GrantFilter,
    ) -> Result<Vec<GrantRecord>, IdpError> {
        let mut v: Vec<GrantRecord> = self
            .lock()
            .grants
            .values()
            .filter(|g| {
                g.realm_id == realm_id
                    && filter.subject.as_ref().is_none_or(|s| *s == g.subject)
                    && filter.client_id.as_ref().is_none_or(|c| *c == g.client_id)
                    && (filter.include_revoked || g.revoked_at.is_none())
            })
            .cloned()
            .collect();
        v.sort_by_key(|g| std::cmp::Reverse(g.created_at));
        if let Some(l) = filter.limit {
            v.truncate(l);
        }
        Ok(v)
    }

    async fn revoked_grant_ids(
        &self,
        realm_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, IdpError> {
        Ok(self
            .lock()
            .grants
            .values()
            .filter(|g| g.realm_id == realm_id && g.revoked_at.is_some() && g.expires_at > now)
            .map(|g| g.id.clone())
            .collect())
    }

    async fn put_code(&self, code: AuthorizationCode) -> Result<(), IdpError> {
        self.lock()
            .codes
            .insert(k2(&code.realm_id, &code.code_hash), code);
        Ok(())
    }

    async fn take_code(
        &self,
        realm_id: &str,
        code_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<CodeTake, IdpError> {
        let mut i = self.lock();
        let key = k2(realm_id, code_hash);
        if let Some(c) = i.codes.remove(&key) {
            // Tombstone outlives the code so a late replay still revokes.
            let tomb_exp = c.expires_at.max(now) + Duration::hours(1);
            i.tombstones.insert(key, (c.grant_id.clone(), tomb_exp));
            if c.expires_at <= now {
                return Ok(CodeTake::Unknown);
            }
            return Ok(CodeTake::Fresh(Box::new(c)));
        }
        match i.tombstones.get(&key) {
            Some((gid, exp)) if *exp > now => Ok(CodeTake::Replayed {
                grant_id: gid.clone(),
            }),
            _ => Ok(CodeTake::Unknown),
        }
    }

    async fn put_launch(&self, h: LaunchHandle) -> Result<(), IdpError> {
        self.lock()
            .launches
            .insert(k2(&h.realm_id, &h.handle_hash), h);
        Ok(())
    }

    async fn take_launch(
        &self,
        realm_id: &str,
        handle_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<LaunchHandle>, IdpError> {
        Ok(self
            .lock()
            .launches
            .remove(&k2(realm_id, handle_hash))
            .filter(|h| h.expires_at > now))
    }

    async fn record_jti(
        &self,
        realm_id: &str,
        client_id: &str,
        jti: &str,
        expires_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<bool, IdpError> {
        let mut i = self.lock();
        i.jti.retain(|_, e| *e > now);
        let key = (realm_id.to_string(), client_id.to_string(), jti.to_string());
        if i.jti.contains_key(&key) {
            return Ok(false);
        }
        i.jti.insert(key, expires_at);
        Ok(true)
    }

    async fn put_interaction(&self, p: PendingAuthorization) -> Result<(), IdpError> {
        self.lock()
            .interactions
            .insert(k2(&p.realm_id, &p.id_hash), p);
        Ok(())
    }

    async fn get_interaction(
        &self,
        realm_id: &str,
        id_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<PendingAuthorization>, IdpError> {
        Ok(self
            .lock()
            .interactions
            .get(&k2(realm_id, id_hash))
            .filter(|p| p.expires_at > now)
            .cloned())
    }

    async fn update_interaction(&self, p: PendingAuthorization) -> Result<bool, IdpError> {
        let mut i = self.lock();
        match i.interactions.get_mut(&k2(&p.realm_id, &p.id_hash)) {
            Some(cur) if cur.expires_at > Utc::now() => {
                *cur = p;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn take_interaction(
        &self,
        realm_id: &str,
        id_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<PendingAuthorization>, IdpError> {
        Ok(self
            .lock()
            .interactions
            .remove(&k2(realm_id, id_hash))
            .filter(|p| p.expires_at > now))
    }

    async fn create_family(
        &self,
        realm_id: &str,
        family: RefreshFamily,
        first_token_hash: &str,
    ) -> Result<(), IdpError> {
        let mut i = self.lock();
        if i.families.contains_key(&family.family_id) {
            return Err(IdpError::Conflict("family exists".into()));
        }
        i.refresh_tokens.insert(
            first_token_hash.to_string(),
            (family.family_id.clone(), family.current_generation),
        );
        i.families
            .insert(family.family_id.clone(), (realm_id.to_string(), family));
        Ok(())
    }

    async fn get_family_by_token(
        &self,
        realm_id: &str,
        token_hash: &str,
    ) -> Result<Option<(RefreshFamily, u32)>, IdpError> {
        let i = self.lock();
        let Some((fid, generation)) = i.refresh_tokens.get(token_hash) else {
            return Ok(None);
        };
        Ok(i.families
            .get(fid)
            .filter(|(r, _)| r == realm_id)
            .map(|(_, f)| (f.clone(), *generation)))
    }

    async fn rotate_family(
        &self,
        realm_id: &str,
        family_id: &str,
        from_generation: u32,
        new_token_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, IdpError> {
        let mut i = self.lock();
        let ok = match i.families.get_mut(family_id) {
            Some((r, f))
                if r == realm_id
                    && f.revoked_at.is_none()
                    && f.current_generation == from_generation =>
            {
                f.current_generation = from_generation + 1;
                f.last_used_at = now;
                true
            }
            _ => false,
        };
        if ok {
            i.refresh_tokens.insert(
                new_token_hash.to_string(),
                (family_id.to_string(), from_generation + 1),
            );
        }
        Ok(ok)
    }

    async fn revoke_family(
        &self,
        realm_id: &str,
        family_id: &str,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<(), IdpError> {
        if let Some((r, f)) = self.lock().families.get_mut(family_id)
            && r == realm_id
            && f.revoked_at.is_none()
        {
            f.revoked_at = Some(now);
            f.revoked_reason = Some(reason.to_string());
        }
        Ok(())
    }

    async fn purge_expired(&self, now: DateTime<Utc>) -> Result<u64, IdpError> {
        let mut i = self.lock();
        let before = i.codes.len()
            + i.tombstones.len()
            + i.launches.len()
            + i.interactions.len()
            + i.jti.len()
            + i.sessions.len();
        i.codes.retain(|_, c| c.expires_at > now);
        i.tombstones.retain(|_, (_, e)| *e > now);
        i.launches.retain(|_, h| h.expires_at > now);
        i.interactions.retain(|_, p| p.expires_at > now);
        i.jti.retain(|_, e| *e > now);
        i.sessions.retain(|_, s| s.expires_at > now);
        let after = i.codes.len()
            + i.tombstones.len()
            + i.launches.len()
            + i.interactions.len()
            + i.jti.len()
            + i.sessions.len();
        Ok((before - after) as u64)
    }
}
