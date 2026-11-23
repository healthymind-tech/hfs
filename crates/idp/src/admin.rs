//! Administrative service (design §12): every write the console wizard, the
//! management pages and `/admin/idp/**` perform goes through [`IdpAdmin`].
//! No HTTP here — package I7 maps it to routes, adds authentication, CSRF,
//! `If-Match` and audit.
//!
//! Rules enforced here (not in the HTTP layer, so no caller can skip them):
//! * specs are validated (`RealmSpec::validate`, `ClientSpec::validate`) and
//!   clients must fit the realm's enabled features;
//! * a realm cannot be created without `HFS_SECRET_KEYS` (keys are sealed);
//! * client secrets are generated here and returned exactly once; only the
//!   argon2id hash is stored;
//! * realm names are immutable (the issuer would change);
//! * every write reloads the local runtime before returning (§10.3).

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::client::{ClientAuth, ClientSpec, GrantType, LaunchType};
use crate::error::IdpError;
use crate::events::IdpEvent;
use crate::keys::{KeyRecord, KeyStatus, RotationAction, SigningAlg, SigningKey, plan_rotation};
use crate::launch::{LaunchContext, LaunchHandle};
use crate::password;
use crate::realm::RealmSpec;
use crate::runtime::IdpRuntime;
use crate::store::{
    ClientRecord, GrantFilter, GrantRecord, IdpStore, RealmRecord, Registration, SsoSession,
};
use crate::token::{HandleKind, mint_handle};
use crate::user::RealmUser;
use crate::util::random_b64;

/// A client as created: the record plus the secret, shown once.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedClient {
    pub record: ClientRecord,
    /// Present for `client_secret_*` clients on creation / secret reset.
    pub client_secret: Option<String>,
}

/// Request to create an EHR launch handle (console "Launch app", or the
/// admin API for external portals).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchRequest {
    /// The app being launched (`None` = any client of the realm).
    pub client_id: Option<String>,
    /// Bound endpoint name.
    pub endpoint: String,
    /// Console user who launched.
    pub launched_by: Option<String>,
    #[serde(default)]
    pub context: LaunchContext,
}

/// A created launch handle.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedLaunch {
    /// `launch` parameter value for the app (`hmlh_…`).
    pub launch: String,
    /// `iss` parameter for the app: the endpoint's FHIR base URL.
    pub iss: String,
    pub expires_at: DateTime<Utc>,
}

/// The admin service.
#[derive(Clone)]
pub struct IdpAdmin {
    rt: Arc<IdpRuntime>,
}

fn store_of(rt: &IdpRuntime) -> &Arc<dyn IdpStore> {
    rt.store()
}

impl IdpAdmin {
    pub fn new(rt: Arc<IdpRuntime>) -> Self {
        Self { rt }
    }

    pub fn runtime(&self) -> &Arc<IdpRuntime> {
        &self.rt
    }

    fn store(&self) -> &Arc<dyn IdpStore> {
        store_of(&self.rt)
    }

    async fn realm(&self, realm_id: &str) -> Result<RealmRecord, IdpError> {
        self.store()
            .get_realm(realm_id)
            .await?
            .ok_or_else(|| IdpError::NotFound(format!("realm {realm_id}")))
    }

    // ------------------------------------------------------------ realms

    pub async fn list_realms(&self) -> Result<Vec<RealmRecord>, IdpError> {
        self.store().list_realms().await
    }

    /// Creates a realm and one active signing key per configured algorithm.
    pub async fn create_realm(&self, spec: RealmSpec) -> Result<RealmRecord, IdpError> {
        spec.validate()?;
        if self.rt.key_ring().is_none() {
            return Err(IdpError::Conflict(
                "configure HFS_SECRET_KEYS before creating an authorization server (signing keys are sealed with it)"
                    .into(),
            ));
        }
        let rec = RealmRecord {
            id: format!("r_{}", random_b64(12)),
            spec,
            revision: 0,
        };
        let rec = self.store().put_realm(rec, None).await?;
        for alg in rec.spec.signing_algs.clone() {
            self.add_key(&rec, alg, KeyStatus::Active).await?;
        }
        self.rt.reload().await?;
        Ok(rec)
    }

    /// Updates a realm (optimistic concurrency on `expected_revision`).
    /// Algorithms added to the spec get an active key.
    pub async fn update_realm(
        &self,
        realm_id: &str,
        spec: RealmSpec,
        expected_revision: i64,
    ) -> Result<RealmRecord, IdpError> {
        spec.validate()?;
        let cur = self.realm(realm_id).await?;
        if cur.spec.name != spec.name {
            return Err(IdpError::invalid(
                "name",
                "a realm cannot be renamed (its issuer URL would change)",
            ));
        }
        let rec = self
            .store()
            .put_realm(
                RealmRecord {
                    id: realm_id.to_string(),
                    spec,
                    revision: expected_revision,
                },
                Some(expected_revision),
            )
            .await?;
        let keys = self.store().list_keys(realm_id).await?;
        for alg in rec.spec.signing_algs.clone() {
            if !keys
                .iter()
                .any(|k| k.alg == alg && k.status == KeyStatus::Active)
            {
                self.add_key(&rec, alg, KeyStatus::Active).await?;
            }
        }
        self.rt.reload().await?;
        Ok(rec)
    }

    pub async fn delete_realm(&self, realm_id: &str) -> Result<bool, IdpError> {
        let r = self.store().delete_realm(realm_id).await?;
        self.rt.reload().await?;
        Ok(r)
    }

    // ------------------------------------------------------------ keys

    async fn add_key(
        &self,
        rec: &RealmRecord,
        alg: SigningAlg,
        status: KeyStatus,
    ) -> Result<KeyRecord, IdpError> {
        let key = SigningKey::generate(alg, rec.spec.rsa_bits)?;
        self.store_key(&rec.id, key, status).await
    }

    async fn store_key(
        &self,
        realm_id: &str,
        key: SigningKey,
        status: KeyStatus,
    ) -> Result<KeyRecord, IdpError> {
        let ring = self
            .rt
            .key_ring()
            .ok_or_else(|| IdpError::Conflict("HFS_SECRET_KEYS is not configured".into()))?;
        let now = Utc::now();
        let kr = KeyRecord {
            kid: key.kid().to_string(),
            alg: key.alg(),
            status,
            public_jwk: key.public_jwk().clone(),
            created_at: now,
            activate_at: (status == KeyStatus::Active).then_some(now),
            retire_at: None,
            expire_at: None,
        };
        self.store()
            .insert_key(realm_id, kr.clone(), key.seal(ring, realm_id))
            .await?;
        self.key_event(realm_id, &kr.kid, "created");
        Ok(kr)
    }

    fn key_event(&self, realm_id: &str, kid: &str, action: &str) {
        let realm = self
            .rt
            .realms()
            .into_iter()
            .find(|r| r.id == realm_id)
            .map(|r| r.spec.name.as_str().to_string())
            .unwrap_or_else(|| realm_id.to_string());
        self.rt.emit(IdpEvent::KeyRotated {
            realm,
            kid: kid.to_string(),
            action: action.to_string(),
        });
    }

    pub async fn list_keys(&self, realm_id: &str) -> Result<Vec<KeyRecord>, IdpError> {
        self.store().list_keys(realm_id).await
    }

    /// Imports an operator-provided PKCS#8 (DER) key as `next`; the rotation
    /// job promotes it after the pre-publication period.
    pub async fn import_key(
        &self,
        realm_id: &str,
        alg: SigningAlg,
        pkcs8_der: &[u8],
    ) -> Result<KeyRecord, IdpError> {
        self.realm(realm_id).await?;
        let key = SigningKey::from_pkcs8(alg, pkcs8_der)?;
        let kr = self.store_key(realm_id, key, KeyStatus::Next).await?;
        self.rt.reload().await?;
        Ok(kr)
    }

    /// Planned rotation: creates a `next` key for every configured algorithm
    /// that has none. It is published at once and promoted by
    /// [`IdpAdmin::run_rotation`] after `prepublish_hours`.
    pub async fn rotate_keys(&self, realm_id: &str) -> Result<Vec<KeyRecord>, IdpError> {
        let rec = self.realm(realm_id).await?;
        let keys = self.store().list_keys(realm_id).await?;
        let mut out = vec![];
        for alg in rec.spec.signing_algs.clone() {
            if !keys
                .iter()
                .any(|k| k.alg == alg && k.status == KeyStatus::Next)
            {
                out.push(self.add_key(&rec, alg, KeyStatus::Next).await?);
            }
        }
        self.rt.reload().await?;
        Ok(out)
    }

    /// Applies the rotation planner's actions for one realm (run by the
    /// cluster-leased job). Returns the actions taken.
    pub async fn run_rotation(
        &self,
        realm_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<RotationAction>, IdpError> {
        let rec = self.realm(realm_id).await?;
        let keys = self.store().list_keys(realm_id).await?;
        let actions = plan_rotation(&keys, &rec.spec.signing_algs, &rec.spec.rotation, now);
        let retain = Duration::hours(rec.spec.rotation.retain_hours as i64);
        for a in &actions {
            match a {
                RotationAction::CreateNext(alg) => {
                    // A realm without any key for alg (new alg) gets an
                    // active key directly; otherwise a pre-published next.
                    let has_active = keys
                        .iter()
                        .any(|k| k.alg == *alg && k.status == KeyStatus::Active);
                    let status = if has_active {
                        KeyStatus::Next
                    } else {
                        KeyStatus::Active
                    };
                    self.add_key(&rec, *alg, status).await?;
                }
                RotationAction::Promote { next, demote } => {
                    if let Some(old) = demote {
                        self.store()
                            .set_key_status(
                                realm_id,
                                old,
                                KeyStatus::Active,
                                KeyStatus::Retiring,
                                now,
                                Some(now + retain),
                            )
                            .await?;
                    }
                    self.store()
                        .set_key_status(
                            realm_id,
                            next,
                            KeyStatus::Next,
                            KeyStatus::Active,
                            now,
                            None,
                        )
                        .await?;
                    self.key_event(realm_id, next, "promoted");
                }
                RotationAction::Retire(kid) => {
                    self.store()
                        .set_key_status(
                            realm_id,
                            kid,
                            KeyStatus::Retiring,
                            KeyStatus::Retired,
                            now,
                            None,
                        )
                        .await?;
                    self.key_event(realm_id, kid, "retired");
                }
            }
        }
        if !actions.is_empty() {
            self.rt.reload().await?;
        }
        Ok(actions)
    }

    /// Emergency rotation ("key compromised", design §6): every published
    /// key is retired at once, fresh active keys are generated and every
    /// grant of the realm is revoked, so all tokens die immediately on
    /// internal endpoints and refresh tokens stop working.
    pub async fn emergency_rotate(&self, realm_id: &str) -> Result<Vec<KeyRecord>, IdpError> {
        let rec = self.realm(realm_id).await?;
        let now = Utc::now();
        for k in self.store().list_keys(realm_id).await? {
            if k.status.published() {
                self.store()
                    .set_key_status(
                        realm_id,
                        &k.kid,
                        k.status,
                        KeyStatus::Retired,
                        now,
                        Some(now),
                    )
                    .await?;
                self.key_event(realm_id, &k.kid, "emergency");
            }
        }
        let mut out = vec![];
        for alg in rec.spec.signing_algs.clone() {
            out.push(self.add_key(&rec, alg, KeyStatus::Active).await?);
        }
        self.store()
            .revoke_all_grants(realm_id, "emergency key rotation", now)
            .await?;
        self.rt.reload().await?;
        Ok(out)
    }

    // ------------------------------------------------------------ clients

    fn check_client_fits_realm(spec: &RealmSpec, c: &ClientSpec) -> Result<(), IdpError> {
        let f = &spec.features;
        match &c.auth {
            ClientAuth::None if !f.client_public => {
                return Err(IdpError::invalid(
                    "auth",
                    "public clients are disabled in this realm",
                ));
            }
            ClientAuth::ClientSecret { .. } if !f.client_confidential_symmetric => {
                return Err(IdpError::invalid(
                    "auth",
                    "client secrets are disabled in this realm",
                ));
            }
            ClientAuth::PrivateKeyJwt { .. } if !f.client_confidential_asymmetric => {
                return Err(IdpError::invalid(
                    "auth",
                    "private_key_jwt is disabled in this realm",
                ));
            }
            ClientAuth::TlsClientAuth { .. } => {
                return Err(IdpError::invalid(
                    "auth",
                    "mTLS client authentication is phase 3",
                ));
            }
            _ => {}
        }
        for g in &c.grant_types {
            let ok = match g {
                GrantType::AuthorizationCode => f.launch_ehr || f.launch_standalone,
                GrantType::RefreshToken => f.offline_access || f.online_access,
                GrantType::ClientCredentials => f.backend_services,
                _ => false,
            };
            if !ok {
                return Err(IdpError::invalid(
                    "grantTypes",
                    format!("{g:?} is not enabled in this realm"),
                ));
            }
        }
        if c.launch_types.contains(&LaunchType::Ehr) && !f.launch_ehr {
            return Err(IdpError::invalid(
                "launchTypes",
                "EHR launch is disabled in this realm",
            ));
        }
        if c.launch_types.contains(&LaunchType::Standalone) && !f.launch_standalone {
            return Err(IdpError::invalid(
                "launchTypes",
                "standalone launch is disabled in this realm",
            ));
        }
        if c.require_dpop || c.require_par {
            return Err(IdpError::invalid("requireDpop", "DPoP and PAR are phase 2"));
        }
        Ok(())
    }

    pub async fn list_clients(&self, realm_id: &str) -> Result<Vec<ClientRecord>, IdpError> {
        self.store().list_clients(realm_id).await
    }

    /// Creates a client. For `client_secret` clients pass an empty
    /// `secretHash`: a secret is generated and returned once. A provided
    /// hash must be an argon2id PHC string.
    pub async fn create_client(
        &self,
        realm_id: &str,
        mut spec: ClientSpec,
    ) -> Result<CreatedClient, IdpError> {
        let realm = self.realm(realm_id).await?;
        let mut secret = None;
        if let ClientAuth::ClientSecret { secret_hash } = &mut spec.auth {
            if secret_hash.is_empty() {
                let s = password::generate_client_secret();
                *secret_hash = password::hash(&s);
                secret = Some(s);
            } else if !secret_hash.starts_with("$argon2id$") {
                return Err(IdpError::invalid(
                    "auth.secretHash",
                    "must be an argon2id PHC hash (or empty to generate a secret)",
                ));
            }
        }
        spec.validate()?;
        Self::check_client_fits_realm(&realm.spec, &spec)?;
        let record = self
            .store()
            .put_client(
                realm_id,
                ClientRecord {
                    spec,
                    registration: Registration::Admin,
                    revision: 0,
                },
                None,
            )
            .await?;
        self.rt.reload().await?;
        Ok(CreatedClient {
            record,
            client_secret: secret,
        })
    }

    /// Updates a client. A `client_secret` spec with an empty hash keeps the
    /// stored secret.
    pub async fn update_client(
        &self,
        realm_id: &str,
        mut spec: ClientSpec,
        expected_revision: i64,
    ) -> Result<ClientRecord, IdpError> {
        let realm = self.realm(realm_id).await?;
        let cur = self
            .store()
            .get_client_record(realm_id, &spec.client_id)
            .await?
            .ok_or_else(|| IdpError::NotFound(format!("client {}", spec.client_id)))?;
        if let ClientAuth::ClientSecret { secret_hash } = &mut spec.auth
            && secret_hash.is_empty()
        {
            match &cur.spec.auth {
                ClientAuth::ClientSecret { secret_hash: old } => *secret_hash = old.clone(),
                _ => {
                    return Err(IdpError::invalid(
                        "auth",
                        "switching to client_secret needs a secret reset",
                    ));
                }
            }
        }
        spec.validate()?;
        Self::check_client_fits_realm(&realm.spec, &spec)?;
        let rec = self
            .store()
            .put_client(
                realm_id,
                ClientRecord {
                    spec,
                    registration: cur.registration,
                    revision: expected_revision,
                },
                Some(expected_revision),
            )
            .await?;
        self.rt.reload().await?;
        Ok(rec)
    }

    /// New secret for a `client_secret` client (or converts a client to
    /// `client_secret`); returned once.
    pub async fn reset_client_secret(
        &self,
        realm_id: &str,
        client_id: &str,
    ) -> Result<CreatedClient, IdpError> {
        let realm = self.realm(realm_id).await?;
        let cur = self
            .store()
            .get_client_record(realm_id, client_id)
            .await?
            .ok_or_else(|| IdpError::NotFound(format!("client {client_id}")))?;
        let s = password::generate_client_secret();
        let mut spec = cur.spec.clone();
        spec.auth = ClientAuth::ClientSecret {
            secret_hash: password::hash(&s),
        };
        spec.validate()?;
        Self::check_client_fits_realm(&realm.spec, &spec)?;
        let record = self
            .store()
            .put_client(
                realm_id,
                ClientRecord {
                    spec,
                    registration: cur.registration,
                    revision: cur.revision,
                },
                Some(cur.revision),
            )
            .await?;
        self.rt.reload().await?;
        Ok(CreatedClient {
            record,
            client_secret: Some(s),
        })
    }

    pub async fn delete_client(&self, realm_id: &str, client_id: &str) -> Result<bool, IdpError> {
        let r = self.store().delete_client(realm_id, client_id).await?;
        // Outstanding grants of the client die with it.
        let now = Utc::now();
        for g in self
            .store()
            .list_grants(
                realm_id,
                &GrantFilter {
                    client_id: Some(client_id.to_string()),
                    ..Default::default()
                },
            )
            .await?
        {
            self.store()
                .revoke_grant(realm_id, &g.id, "client deleted", now)
                .await?;
        }
        self.rt.reload().await?;
        Ok(r)
    }

    // ------------------------------------------------------------ users

    pub async fn list_users(&self, realm_id: &str) -> Result<Vec<RealmUser>, IdpError> {
        self.store().list_users(realm_id).await
    }

    fn check_password(realm: &RealmSpec, user: &RealmUser, pw: &str) -> Result<(), IdpError> {
        let min = realm.security.password_min_length.max(8) as usize;
        if pw.chars().count() < min {
            return Err(IdpError::invalid(
                "password",
                format!("at least {min} characters"),
            ));
        }
        if pw.len() > password::MAX_LENGTH {
            return Err(IdpError::invalid("password", "too long"));
        }
        if pw.eq_ignore_ascii_case(&user.username) {
            return Err(IdpError::invalid(
                "password",
                "must differ from the username",
            ));
        }
        let first = pw.chars().next().unwrap_or_default();
        if pw.chars().all(|c| c == first) {
            return Err(IdpError::invalid(
                "password",
                "must not repeat one character",
            ));
        }
        Ok(())
    }

    fn check_user(user: &RealmUser) -> Result<(), IdpError> {
        let u = user.username.trim();
        if u.is_empty() || u.len() > 128 || u != user.username || u.chars().any(char::is_control) {
            return Err(IdpError::invalid(
                "username",
                "1-128 characters, no surrounding spaces",
            ));
        }
        let fhir_users: Vec<&str> = user
            .links
            .iter()
            .filter(|l| l.is_fhir_user)
            .map(|l| l.endpoint.as_str())
            .collect();
        let mut dedup = fhir_users.clone();
        dedup.sort();
        dedup.dedup();
        if dedup.len() != fhir_users.len() {
            return Err(IdpError::invalid(
                "links",
                "at most one fhirUser link per endpoint",
            ));
        }
        for l in &user.links {
            let ok = l.reference.split_once('/').is_some_and(|(t, id)| {
                matches!(
                    t,
                    "Patient" | "Practitioner" | "PractitionerRole" | "RelatedPerson" | "Person"
                ) && !id.is_empty()
                    && id.len() <= 64
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
            });
            if !ok {
                return Err(IdpError::invalid(
                    "links.reference",
                    format!(
                        "{:?} is not a Patient/Practitioner/PractitionerRole/RelatedPerson/Person reference",
                        l.reference
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Creates a user (id generated when empty), optionally with a password.
    pub async fn create_user(
        &self,
        realm_id: &str,
        mut user: RealmUser,
        password: Option<&str>,
        must_change_password: bool,
    ) -> Result<RealmUser, IdpError> {
        let realm = self.realm(realm_id).await?;
        if user.id.is_empty() {
            user.id = format!("u_{}", random_b64(12));
        }
        Self::check_user(&user)?;
        if self.store().get_user(realm_id, &user.id).await?.is_some() {
            return Err(IdpError::Conflict(format!("user {} exists", user.id)));
        }
        if let Some(pw) = password {
            Self::check_password(&realm.spec, &user, pw)?;
        }
        self.store().put_user(realm_id, user.clone()).await?;
        if let Some(pw) = password {
            self.store()
                .set_password_hash(
                    realm_id,
                    &user.id,
                    &password::hash(pw),
                    must_change_password,
                )
                .await?;
        }
        Ok(user)
    }

    pub async fn update_user(
        &self,
        realm_id: &str,
        user: RealmUser,
    ) -> Result<RealmUser, IdpError> {
        self.realm(realm_id).await?;
        Self::check_user(&user)?;
        if self.store().get_user(realm_id, &user.id).await?.is_none() {
            return Err(IdpError::NotFound(format!("user {}", user.id)));
        }
        self.store().put_user(realm_id, user.clone()).await?;
        if user.disabled {
            self.end_user_sessions(realm_id, &user.id).await?;
        }
        Ok(user)
    }

    pub async fn set_password(
        &self,
        realm_id: &str,
        user_id: &str,
        password: &str,
        must_change: bool,
    ) -> Result<(), IdpError> {
        let realm = self.realm(realm_id).await?;
        let user = self
            .store()
            .get_user(realm_id, user_id)
            .await?
            .ok_or_else(|| IdpError::NotFound(format!("user {user_id}")))?;
        Self::check_password(&realm.spec, &user, password)?;
        self.store()
            .set_password_hash(realm_id, user_id, &password::hash(password), must_change)
            .await
    }

    pub async fn delete_user(&self, realm_id: &str, user_id: &str) -> Result<bool, IdpError> {
        self.end_user_sessions(realm_id, user_id).await?;
        let now = Utc::now();
        for g in self
            .store()
            .list_grants(
                realm_id,
                &GrantFilter {
                    subject: Some(user_id.to_string()),
                    ..Default::default()
                },
            )
            .await?
        {
            self.store()
                .revoke_grant(realm_id, &g.id, "user deleted", now)
                .await?;
        }
        let r = self.store().delete_user(realm_id, user_id).await?;
        self.rt.reload().await?;
        Ok(r)
    }

    async fn end_user_sessions(&self, realm_id: &str, user_id: &str) -> Result<(), IdpError> {
        let now = Utc::now();
        for s in self.store().list_sessions(realm_id, Some(user_id)).await? {
            if s.revoked_at.is_none() {
                self.store()
                    .revoke_session(realm_id, &s.id_hash, now)
                    .await?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ sessions / grants

    pub async fn list_sessions(
        &self,
        realm_id: &str,
        user_id: Option<&str>,
    ) -> Result<Vec<SsoSession>, IdpError> {
        self.store().list_sessions(realm_id, user_id).await
    }

    pub async fn revoke_session(&self, realm_id: &str, id_hash: &str) -> Result<bool, IdpError> {
        self.store()
            .revoke_session(realm_id, id_hash, Utc::now())
            .await
    }

    pub async fn list_grants(
        &self,
        realm_id: &str,
        filter: &GrantFilter,
    ) -> Result<Vec<GrantRecord>, IdpError> {
        self.store().list_grants(realm_id, filter).await
    }

    pub async fn revoke_grant(&self, realm_id: &str, grant_id: &str) -> Result<bool, IdpError> {
        let r = self
            .store()
            .revoke_grant(realm_id, grant_id, "revoked by administrator", Utc::now())
            .await?;
        self.rt.reload().await?;
        Ok(r)
    }

    // ------------------------------------------------------------ maintenance

    /// One maintenance pass: purge expired one-time artefacts, sessions and
    /// grants, then apply the key rotation planner to every realm. Run it
    /// under a cluster lease (`cluster_jobs`) so one instance does it at a
    /// time; every step is safe to repeat. Returns (rows purged, rotation
    /// actions taken).
    pub async fn run_maintenance(&self, now: DateTime<Utc>) -> Result<(u64, usize), IdpError> {
        let purged = self.store().purge_expired(now).await?;
        let mut actions = 0;
        for r in self.store().list_realms().await? {
            match self.run_rotation(&r.id, now).await {
                Ok(a) => actions += a.len(),
                Err(e) => {
                    tracing::error!(realm = %r.spec.name.as_str(), error = %e, "IdP key rotation failed")
                }
            }
        }
        Ok((purged, actions))
    }

    /// Runs [`IdpAdmin::run_maintenance`] every `every` (single-node use; a
    /// cluster wraps the call in its lease instead).
    pub fn spawn_maintenance(&self, every: std::time::Duration) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if let Err(e) = me.run_maintenance(Utc::now()).await {
                    tracing::warn!(error = %e, "IdP maintenance failed");
                }
            }
        })
    }

    // ------------------------------------------------------------ EHR launch

    /// Creates a single-use EHR launch handle (design §8.3).
    pub async fn create_launch(
        &self,
        realm_id: &str,
        req: LaunchRequest,
    ) -> Result<CreatedLaunch, IdpError> {
        let realm = self.realm(realm_id).await?;
        if !realm.spec.features.launch_ehr {
            return Err(IdpError::invalid("realm", "EHR launch is disabled"));
        }
        if !realm.spec.endpoints.contains(&req.endpoint) {
            return Err(IdpError::invalid(
                "endpoint",
                "the endpoint is not bound to this realm",
            ));
        }
        if let Some(cid) = &req.client_id {
            let c = self
                .store()
                .get_client(realm_id, cid)
                .await?
                .ok_or_else(|| IdpError::NotFound(format!("client {cid}")))?;
            if c.disabled || !c.launch_types.contains(&LaunchType::Ehr) {
                return Err(IdpError::invalid(
                    "clientId",
                    "the client is not an EHR-launch app",
                ));
            }
        }
        if req.context.fhir_context.iter().any(|i| !i.is_valid()) {
            return Err(IdpError::invalid(
                "context.fhirContext",
                "each item needs reference, canonical or identifier",
            ));
        }
        let iss = self
            .rt
            .endpoint_urls()
            .base_url(&req.endpoint)
            .ok_or_else(|| IdpError::invalid("endpoint", "unknown endpoint"))?;
        let h = mint_handle(HandleKind::LaunchHandle);
        let expires_at = Utc::now() + Duration::seconds(realm.spec.lifetimes.launch_handle as i64);
        self.store()
            .put_launch(LaunchHandle {
                handle_hash: h.hash,
                realm_id: realm_id.to_string(),
                client_id: req.client_id,
                endpoint: req.endpoint,
                launched_by: req.launched_by,
                context: req.context,
                expires_at,
            })
            .await?;
        Ok(CreatedLaunch {
            launch: h.value,
            iss: iss.trim_end_matches('/').to_string(),
            expires_at,
        })
    }
}
