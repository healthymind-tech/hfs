//! PostgreSQL [`IdpStore`] over fork migration `hm-0020-idp-core`
//! (package I2). Uses the server's shared deadpool pool; creates nothing
//! (the persistence layer runs the migration).
//!
//! Atomicity (D-11): codes, launches and interactions are consumed with
//! `DELETE … RETURNING`; `jti` with `INSERT … ON CONFLICT DO NOTHING` (an
//! expired row is reclaimed with a conditional `DO UPDATE`); refresh
//! rotation with `UPDATE … WHERE current_generation = $n` inside a
//! transaction that also inserts the new token digest.

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
pub use deadpool_postgres;
use deadpool_postgres::Pool;
use helios_endpoints::SealedSecret;
use serde_json::Value;
pub use tokio_postgres;
use tokio_postgres::Row;
use tokio_postgres::error::SqlState;

use crate::client::ClientSpec;
use crate::error::IdpError;
use crate::interaction::PendingAuthorization;
use crate::keys::{KeyRecord, KeyStatus, SigningAlg};
use crate::launch::{LaunchContext, LaunchHandle};
use crate::realm::RealmSpec;
use crate::store::*;
use crate::token::RefreshFamily;
use crate::user::{FhirLink, RealmUser, UserKind};

/// The PostgreSQL store.
#[derive(Clone)]
pub struct PgStore {
    pool: Pool,
}

fn se(e: impl std::fmt::Display) -> IdpError {
    IdpError::Store(e.to_string())
}

fn pe(e: tokio_postgres::Error) -> IdpError {
    if e.code() == Some(&SqlState::UNIQUE_VIOLATION) {
        IdpError::Conflict(e.to_string())
    } else {
        IdpError::Store(e.to_string())
    }
}

fn to_json<T: serde::Serialize>(v: &T) -> Result<Value, IdpError> {
    serde_json::to_value(v).map_err(se)
}

fn from_json<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, IdpError> {
    serde_json::from_value(v).map_err(se)
}

fn status_str(s: KeyStatus) -> &'static str {
    match s {
        KeyStatus::Next => "next",
        KeyStatus::Active => "active",
        KeyStatus::Retiring => "retiring",
        KeyStatus::Retired => "retired",
    }
}

fn parse_status(s: &str) -> Result<KeyStatus, IdpError> {
    Ok(match s {
        "next" => KeyStatus::Next,
        "active" => KeyStatus::Active,
        "retiring" => KeyStatus::Retiring,
        "retired" => KeyStatus::Retired,
        o => return Err(IdpError::Store(format!("bad key status {o}"))),
    })
}

fn kind_str(k: UserKind) -> &'static str {
    match k {
        UserKind::Patient => "patient",
        UserKind::Practitioner => "practitioner",
        UserKind::RelatedPerson => "related-person",
        UserKind::Staff => "staff",
    }
}

fn parse_kind(s: &str) -> Result<UserKind, IdpError> {
    Ok(match s {
        "patient" => UserKind::Patient,
        "practitioner" => UserKind::Practitioner,
        "related-person" => UserKind::RelatedPerson,
        "staff" => UserKind::Staff,
        o => return Err(IdpError::Store(format!("bad user kind {o}"))),
    })
}

fn key_row(r: &Row) -> Result<KeyRecord, IdpError> {
    let alg: String = r.get("alg");
    Ok(KeyRecord {
        kid: r.get("kid"),
        alg: alg.parse::<SigningAlg>()?,
        status: parse_status(r.get::<_, &str>("status"))?,
        public_jwk: r.get("public_jwk"),
        created_at: r.get("created_at"),
        activate_at: r.get("activate_at"),
        retire_at: r.get("retire_at"),
        expire_at: r.get("expire_at"),
    })
}

fn realm_row(r: &Row) -> Result<RealmRecord, IdpError> {
    Ok(RealmRecord {
        id: r.get("id"),
        spec: from_json::<RealmSpec>(r.get("spec"))?,
        revision: r.get("revision"),
    })
}

fn client_row(r: &Row) -> Result<ClientRecord, IdpError> {
    let reg: String = r.get("registration");
    Ok(ClientRecord {
        spec: from_json::<ClientSpec>(r.get("spec"))?,
        registration: match reg.as_str() {
            "dcr" => Registration::Dcr,
            "udap" => Registration::Udap,
            _ => Registration::Admin,
        },
        revision: r.get("revision"),
    })
}

fn session_row(r: &Row) -> SsoSession {
    SsoSession {
        id_hash: r.get("id_hash"),
        realm_id: r.get("realm_id"),
        user_id: r.get("user_id"),
        sid: r.get("sid"),
        auth_time: r.get("auth_time"),
        amr: r.get("amr"),
        acr: r.get("acr"),
        created_at: r.get("created_at"),
        last_seen_at: r.get("last_seen_at"),
        expires_at: r.get("expires_at"),
        revoked_at: r.get("revoked_at"),
    }
}

fn consent_row(r: &Row) -> Consent {
    Consent {
        realm_id: r.get("realm_id"),
        user_id: r.get("user_id"),
        client_id: r.get("client_id"),
        endpoint: r.get("endpoint"),
        scopes: r.get("scopes"),
        granted_at: r.get("granted_at"),
        expires_at: r.get("expires_at"),
    }
}

fn grant_row(r: &Row) -> Result<GrantRecord, IdpError> {
    Ok(GrantRecord {
        id: r.get("id"),
        realm_id: r.get("realm_id"),
        client_id: r.get("client_id"),
        subject: r.get("subject"),
        scopes: r.get("scopes"),
        audience: r.get("audience"),
        context: from_json::<LaunchContext>(r.get("context"))?,
        fhir_user: r.get("fhir_user"),
        sso_session_id: r.get("sso_session_id"),
        auth_time: r.get("auth_time"),
        created_at: r.get("created_at"),
        expires_at: r.get("expires_at"),
        revoked_at: r.get("revoked_at"),
        revoked_reason: r.get("revoked_reason"),
    })
}

const GRANT_COLS: &str = "id, realm_id, client_id, subject, scopes, audience, context, fhir_user, \
     sso_session_id, auth_time, created_at, expires_at, revoked_at, revoked_reason";
const SESSION_COLS: &str = "id_hash, realm_id, user_id, sid, auth_time, amr, acr, created_at, \
     last_seen_at, expires_at, revoked_at";
const FAMILY_COLS: &str =
    "f.payload, f.current_generation, f.last_used_at, f.revoked_at, f.revoked_reason";

fn family_row(r: &Row) -> Result<RefreshFamily, IdpError> {
    let mut f: RefreshFamily = from_json(r.get("payload"))?;
    let g: i32 = r.get("current_generation");
    f.current_generation = g as u32;
    f.last_used_at = r.get("last_used_at");
    f.revoked_at = r.get("revoked_at");
    f.revoked_reason = r.get("revoked_reason");
    Ok(f)
}

/// Profile JSON of `hfs_idp_users.profile`.
#[derive(serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Profile {
    display_name: Option<String>,
    email: Option<String>,
    #[serde(default)]
    email_verified: bool,
}

impl PgStore {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    async fn client(&self) -> Result<deadpool_postgres::Client, IdpError> {
        self.pool.get().await.map_err(se)
    }

    async fn users_where(
        &self,
        clause: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<Vec<RealmUser>, IdpError> {
        let c = self.client().await?;
        let rows = c
            .query(
                &format!(
                    "SELECT u.id, u.username, u.kind, u.profile, u.external_subject, u.disabled,
                            COALESCE((SELECT jsonb_agg(jsonb_build_object(
                                'endpoint', l.endpoint, 'reference', l.reference,
                                'isFhirUser', l.is_fhir_user,
                                'accessiblePatients', to_jsonb(l.accessible_patients))
                                ORDER BY l.endpoint, l.reference)
                              FROM hfs_idp_user_links l WHERE l.user_id = u.id), '[]'::jsonb) AS links
                     FROM hfs_idp_users u WHERE {clause} ORDER BY u.username"
                ),
                params,
            )
            .await
            .map_err(pe)?;
        rows.iter()
            .map(|r| {
                let p: Profile = from_json(r.get("profile")).unwrap_or_default();
                Ok(RealmUser {
                    id: r.get("id"),
                    username: r.get("username"),
                    kind: parse_kind(r.get::<_, &str>("kind"))?,
                    display_name: p.display_name,
                    email: p.email,
                    email_verified: p.email_verified,
                    links: from_json::<Vec<FhirLink>>(r.get("links"))?,
                    external_subject: r.get("external_subject"),
                    disabled: r.get("disabled"),
                })
            })
            .collect()
    }
}

#[async_trait]
impl IdpStore for PgStore {
    async fn list_realms(&self) -> Result<Vec<RealmRecord>, IdpError> {
        let c = self.client().await?;
        let rows = c
            .query(
                "SELECT id, spec, revision FROM hfs_idp_realms WHERE deleted_at IS NULL ORDER BY name",
                &[],
            )
            .await
            .map_err(pe)?;
        rows.iter().map(realm_row).collect()
    }

    async fn get_realm(&self, realm_id: &str) -> Result<Option<RealmRecord>, IdpError> {
        let c = self.client().await?;
        c.query_opt(
            "SELECT id, spec, revision FROM hfs_idp_realms WHERE id = $1 AND deleted_at IS NULL",
            &[&realm_id],
        )
        .await
        .map_err(pe)?
        .as_ref()
        .map(realm_row)
        .transpose()
    }

    async fn get_realm_by_name(&self, name: &str) -> Result<Option<RealmRecord>, IdpError> {
        let c = self.client().await?;
        c.query_opt(
            "SELECT id, spec, revision FROM hfs_idp_realms WHERE name = $1 AND deleted_at IS NULL",
            &[&name],
        )
        .await
        .map_err(pe)?
        .as_ref()
        .map(realm_row)
        .transpose()
    }

    async fn put_realm(
        &self,
        rec: RealmRecord,
        expected_revision: Option<i64>,
    ) -> Result<RealmRecord, IdpError> {
        let c = self.client().await?;
        let spec = to_json(&rec.spec)?;
        let name = rec.spec.name.as_str();
        match expected_revision {
            None => {
                let row = c
                    .query_one(
                        "INSERT INTO hfs_idp_realms (id, name, spec, enabled) VALUES ($1, $2, $3, $4)
                         RETURNING revision",
                        &[&rec.id, &name, &spec, &rec.spec.enabled],
                    )
                    .await
                    .map_err(pe)?;
                Ok(RealmRecord {
                    revision: row.get(0),
                    ..rec
                })
            }
            Some(rev) => {
                let row = c
                    .query_opt(
                        "UPDATE hfs_idp_realms SET name = $2, spec = $3, enabled = $4,
                                revision = revision + 1, updated_at = NOW()
                         WHERE id = $1 AND revision = $5 AND deleted_at IS NULL
                         RETURNING revision",
                        &[&rec.id, &name, &spec, &rec.spec.enabled, &rev],
                    )
                    .await
                    .map_err(pe)?;
                match row {
                    Some(r) => Ok(RealmRecord {
                        revision: r.get(0),
                        ..rec
                    }),
                    None => {
                        if self.get_realm(&rec.id).await?.is_some() {
                            Err(IdpError::Conflict(format!(
                                "realm {} was changed (revision is not {rev})",
                                rec.id
                            )))
                        } else {
                            Err(IdpError::NotFound(format!("realm {}", rec.id)))
                        }
                    }
                }
            }
        }
    }

    async fn delete_realm(&self, realm_id: &str) -> Result<bool, IdpError> {
        let c = self.client().await?;
        let n = c
            .execute(
                "UPDATE hfs_idp_realms SET deleted_at = NOW(), enabled = FALSE, updated_at = NOW()
                 WHERE id = $1 AND deleted_at IS NULL",
                &[&realm_id],
            )
            .await
            .map_err(pe)?;
        Ok(n == 1)
    }

    async fn list_keys(&self, realm_id: &str) -> Result<Vec<KeyRecord>, IdpError> {
        let c = self.client().await?;
        let rows = c
            .query(
                "SELECT kid, alg, status, public_jwk, created_at, activate_at, retire_at, expire_at
                 FROM hfs_idp_signing_keys WHERE realm_id = $1 ORDER BY created_at, kid",
                &[&realm_id],
            )
            .await
            .map_err(pe)?;
        rows.iter().map(key_row).collect()
    }

    async fn insert_key(
        &self,
        realm_id: &str,
        rec: KeyRecord,
        sealed: SealedSecret,
    ) -> Result<(), IdpError> {
        let c = self.client().await?;
        c.execute(
            "INSERT INTO hfs_idp_signing_keys
                 (realm_id, kid, alg, status, public_jwk, private_key, created_at, activate_at, retire_at, expire_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            &[
                &realm_id,
                &rec.kid,
                &rec.alg.as_str(),
                &status_str(rec.status),
                &rec.public_jwk,
                &sealed.envelope(),
                &rec.created_at,
                &rec.activate_at,
                &rec.retire_at,
                &rec.expire_at,
            ],
        )
        .await
        .map_err(pe)?;
        Ok(())
    }

    async fn get_sealed_key(
        &self,
        realm_id: &str,
        kid: &str,
    ) -> Result<Option<SealedSecret>, IdpError> {
        let c = self.client().await?;
        let row = c
            .query_opt(
                "SELECT private_key FROM hfs_idp_signing_keys WHERE realm_id = $1 AND kid = $2",
                &[&realm_id, &kid],
            )
            .await
            .map_err(pe)?;
        row.map(|r| SealedSecret::from_envelope(r.get::<_, String>(0)).map_err(IdpError::from))
            .transpose()
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
        let c = self.client().await?;
        let n = c
            .execute(
                "UPDATE hfs_idp_signing_keys SET status = $4,
                     activate_at = CASE WHEN $4 = 'active' THEN $5 ELSE activate_at END,
                     retire_at = CASE WHEN $4 IN ('retiring','retired') THEN COALESCE(retire_at, $5) ELSE retire_at END,
                     expire_at = COALESCE($6, expire_at)
                 WHERE realm_id = $1 AND kid = $2 AND status = $3",
                &[&realm_id, &kid, &status_str(from), &status_str(to), &at, &expire_at],
            )
            .await
            .map_err(pe)?;
        Ok(n == 1)
    }

    async fn list_clients(&self, realm_id: &str) -> Result<Vec<ClientRecord>, IdpError> {
        let c = self.client().await?;
        let rows = c
            .query(
                "SELECT spec, registration, revision FROM hfs_idp_clients WHERE realm_id = $1 ORDER BY client_id",
                &[&realm_id],
            )
            .await
            .map_err(pe)?;
        rows.iter().map(client_row).collect()
    }

    async fn get_client(
        &self,
        realm_id: &str,
        client_id: &str,
    ) -> Result<Option<ClientSpec>, IdpError> {
        Ok(self
            .get_client_record(realm_id, client_id)
            .await?
            .map(|r| r.spec))
    }

    async fn get_client_record(
        &self,
        realm_id: &str,
        client_id: &str,
    ) -> Result<Option<ClientRecord>, IdpError> {
        let c = self.client().await?;
        c.query_opt(
            "SELECT spec, registration, revision FROM hfs_idp_clients WHERE realm_id = $1 AND client_id = $2",
            &[&realm_id, &client_id],
        )
        .await
        .map_err(pe)?
        .as_ref()
        .map(client_row)
        .transpose()
    }

    async fn put_client(
        &self,
        realm_id: &str,
        rec: ClientRecord,
        expected_revision: Option<i64>,
    ) -> Result<ClientRecord, IdpError> {
        let c = self.client().await?;
        let spec = to_json(&rec.spec)?;
        let cid = rec.spec.client_id.as_str();
        let row = match expected_revision {
            None => c
                .query_opt(
                    "INSERT INTO hfs_idp_clients (realm_id, client_id, spec, registration, disabled)
                     VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING RETURNING revision",
                    &[&realm_id, &cid, &spec, &rec.registration.as_str(), &rec.spec.disabled],
                )
                .await
                .map_err(pe)?,
            Some(rev) => c
                .query_opt(
                    "UPDATE hfs_idp_clients SET spec = $3, disabled = $4, revision = revision + 1, updated_at = NOW()
                     WHERE realm_id = $1 AND client_id = $2 AND revision = $5 RETURNING revision",
                    &[&realm_id, &cid, &spec, &rec.spec.disabled, &rev],
                )
                .await
                .map_err(pe)?,
        };
        match row {
            Some(r) => Ok(ClientRecord {
                revision: r.get(0),
                ..rec
            }),
            None if expected_revision.is_none() => {
                Err(IdpError::Conflict(format!("client {cid} exists")))
            }
            None => {
                if self.get_client_record(realm_id, cid).await?.is_some() {
                    Err(IdpError::Conflict(format!("client {cid} was changed")))
                } else {
                    Err(IdpError::NotFound(format!("client {cid}")))
                }
            }
        }
    }

    async fn delete_client(&self, realm_id: &str, client_id: &str) -> Result<bool, IdpError> {
        let c = self.client().await?;
        Ok(c.execute(
            "DELETE FROM hfs_idp_clients WHERE realm_id = $1 AND client_id = $2",
            &[&realm_id, &client_id],
        )
        .await
        .map_err(pe)?
            == 1)
    }

    async fn list_users(&self, realm_id: &str) -> Result<Vec<RealmUser>, IdpError> {
        self.users_where("u.realm_id = $1", &[&realm_id]).await
    }

    async fn get_user(&self, realm_id: &str, user_id: &str) -> Result<Option<RealmUser>, IdpError> {
        Ok(self
            .users_where("u.realm_id = $1 AND u.id = $2", &[&realm_id, &user_id])
            .await?
            .pop())
    }

    async fn find_user_by_username(
        &self,
        realm_id: &str,
        username: &str,
    ) -> Result<Option<RealmUser>, IdpError> {
        Ok(self
            .users_where(
                "u.realm_id = $1 AND lower(u.username) = lower($2)",
                &[&realm_id, &username],
            )
            .await?
            .pop())
    }

    async fn find_user_by_external_subject(
        &self,
        realm_id: &str,
        external_subject: &str,
    ) -> Result<Option<RealmUser>, IdpError> {
        Ok(self
            .users_where(
                "u.realm_id = $1 AND u.external_subject = $2",
                &[&realm_id, &external_subject],
            )
            .await?
            .pop())
    }

    async fn put_user(&self, realm_id: &str, user: RealmUser) -> Result<(), IdpError> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(pe)?;
        let profile = to_json(&Profile {
            display_name: user.display_name.clone(),
            email: user.email.clone(),
            email_verified: user.email_verified,
        })?;
        let n = tx
            .execute(
                "INSERT INTO hfs_idp_users (id, realm_id, username, kind, profile, external_subject, disabled)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (id) DO UPDATE SET username = EXCLUDED.username, kind = EXCLUDED.kind,
                     profile = EXCLUDED.profile, external_subject = EXCLUDED.external_subject,
                     disabled = EXCLUDED.disabled, revision = hfs_idp_users.revision + 1, updated_at = NOW()
                 WHERE hfs_idp_users.realm_id = EXCLUDED.realm_id",
                &[
                    &user.id,
                    &realm_id,
                    &user.username,
                    &kind_str(user.kind),
                    &profile,
                    &user.external_subject,
                    &user.disabled,
                ],
            )
            .await
            .map_err(pe)?;
        if n != 1 {
            return Err(IdpError::Conflict(format!(
                "user id {} belongs to another realm",
                user.id
            )));
        }
        tx.execute(
            "DELETE FROM hfs_idp_user_links WHERE user_id = $1",
            &[&user.id],
        )
        .await
        .map_err(pe)?;
        // Links without patients (the bulk of a large access list) go in
        // one statement; the few carrying an accessible-patient array one
        // by one (a 2-D array does not UNNEST row by row).
        let (plain, carriers): (Vec<&FhirLink>, Vec<&FhirLink>) = user
            .links
            .iter()
            .partition(|l| l.accessible_patients.is_empty());
        for chunk in plain.chunks(5_000) {
            let endpoints: Vec<&str> = chunk.iter().map(|l| l.endpoint.as_str()).collect();
            let references: Vec<&str> = chunk.iter().map(|l| l.reference.as_str()).collect();
            let fhir_users: Vec<bool> = chunk.iter().map(|l| l.is_fhir_user).collect();
            tx.execute(
                "INSERT INTO hfs_idp_user_links (user_id, endpoint, reference, is_fhir_user)
                 SELECT $1, e, r, f FROM UNNEST($2::text[], $3::text[], $4::bool[]) AS t(e, r, f)",
                &[&user.id, &endpoints, &references, &fhir_users],
            )
            .await
            .map_err(pe)?;
        }
        for l in carriers {
            tx.execute(
                "INSERT INTO hfs_idp_user_links (user_id, endpoint, reference, is_fhir_user, accessible_patients)
                 VALUES ($1, $2, $3, $4, $5)",
                &[&user.id, &l.endpoint, &l.reference, &l.is_fhir_user, &l.accessible_patients],
            )
            .await
            .map_err(pe)?;
        }
        tx.commit().await.map_err(pe)
    }

    async fn delete_user(&self, realm_id: &str, user_id: &str) -> Result<bool, IdpError> {
        let c = self.client().await?;
        Ok(c.execute(
            "DELETE FROM hfs_idp_users WHERE realm_id = $1 AND id = $2",
            &[&realm_id, &user_id],
        )
        .await
        .map_err(pe)?
            == 1)
    }

    async fn set_password_hash(
        &self,
        realm_id: &str,
        user_id: &str,
        phc: &str,
        must_change: bool,
    ) -> Result<(), IdpError> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(pe)?;
        let n = tx
            .execute(
                "UPDATE hfs_idp_users SET must_change_password = $3, updated_at = NOW()
                 WHERE realm_id = $1 AND id = $2",
                &[&realm_id, &user_id, &must_change],
            )
            .await
            .map_err(pe)?;
        if n != 1 {
            return Err(IdpError::NotFound(format!("user {user_id}")));
        }
        tx.execute(
            "DELETE FROM hfs_idp_credentials WHERE user_id = $1 AND kind = 'password'",
            &[&user_id],
        )
        .await
        .map_err(pe)?;
        let id = format!("cred_{}", crate::util::random_b64(12));
        tx.execute(
            "INSERT INTO hfs_idp_credentials (id, user_id, kind, secret) VALUES ($1, $2, 'password', $3)",
            &[&id, &user_id, &phc],
        )
        .await
        .map_err(pe)?;
        tx.commit().await.map_err(pe)
    }

    async fn get_password_hash(
        &self,
        realm_id: &str,
        user_id: &str,
    ) -> Result<Option<(String, bool)>, IdpError> {
        let c = self.client().await?;
        let row = c
            .query_opt(
                "SELECT c.secret, u.must_change_password FROM hfs_idp_credentials c
                 JOIN hfs_idp_users u ON u.id = c.user_id
                 WHERE u.realm_id = $1 AND u.id = $2 AND c.kind = 'password'",
                &[&realm_id, &user_id],
            )
            .await
            .map_err(pe)?;
        Ok(row.map(|r| (r.get(0), r.get(1))))
    }

    async fn record_login(
        &self,
        realm_id: &str,
        user_id: &str,
        at: DateTime<Utc>,
    ) -> Result<(), IdpError> {
        let c = self.client().await?;
        c.execute(
            "UPDATE hfs_idp_users SET last_login_at = $3 WHERE realm_id = $1 AND id = $2",
            &[&realm_id, &user_id, &at],
        )
        .await
        .map_err(pe)?;
        Ok(())
    }

    async fn put_session(&self, s: SsoSession) -> Result<(), IdpError> {
        let c = self.client().await?;
        c.execute(
            "INSERT INTO hfs_idp_sessions (id_hash, realm_id, user_id, sid, auth_time, amr, acr,
                 created_at, last_seen_at, expires_at, revoked_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (id_hash) DO UPDATE SET auth_time = EXCLUDED.auth_time, amr = EXCLUDED.amr,
                 acr = EXCLUDED.acr, last_seen_at = EXCLUDED.last_seen_at,
                 expires_at = EXCLUDED.expires_at, revoked_at = EXCLUDED.revoked_at
             WHERE hfs_idp_sessions.realm_id = EXCLUDED.realm_id
               AND hfs_idp_sessions.user_id = EXCLUDED.user_id",
            &[
                &s.id_hash,
                &s.realm_id,
                &s.user_id,
                &s.sid,
                &s.auth_time,
                &s.amr,
                &s.acr,
                &s.created_at,
                &s.last_seen_at,
                &s.expires_at,
                &s.revoked_at,
            ],
        )
        .await
        .map_err(pe)?;
        Ok(())
    }

    async fn get_session(
        &self,
        realm_id: &str,
        id_hash: &str,
    ) -> Result<Option<SsoSession>, IdpError> {
        let c = self.client().await?;
        Ok(c.query_opt(
            &format!(
                "SELECT {SESSION_COLS} FROM hfs_idp_sessions WHERE realm_id = $1 AND id_hash = $2"
            ),
            &[&realm_id, &id_hash],
        )
        .await
        .map_err(pe)?
        .as_ref()
        .map(session_row))
    }

    async fn get_session_by_sid(
        &self,
        realm_id: &str,
        sid: &str,
    ) -> Result<Option<SsoSession>, IdpError> {
        let c = self.client().await?;
        Ok(c
            .query_opt(
                &format!(
                    "SELECT {SESSION_COLS} FROM hfs_idp_sessions WHERE realm_id = $1 AND sid = $2 LIMIT 1"
                ),
                &[&realm_id, &sid],
            )
            .await
            .map_err(pe)?
            .as_ref()
            .map(session_row))
    }

    async fn touch_session(
        &self,
        realm_id: &str,
        id_hash: &str,
        at: DateTime<Utc>,
    ) -> Result<(), IdpError> {
        let c = self.client().await?;
        c.execute(
            "UPDATE hfs_idp_sessions SET last_seen_at = $3 WHERE realm_id = $1 AND id_hash = $2",
            &[&realm_id, &id_hash, &at],
        )
        .await
        .map_err(pe)?;
        Ok(())
    }

    async fn revoke_session(
        &self,
        realm_id: &str,
        id_hash: &str,
        at: DateTime<Utc>,
    ) -> Result<bool, IdpError> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(pe)?;
        let n = tx
            .execute(
                "UPDATE hfs_idp_sessions SET revoked_at = $3
                 WHERE realm_id = $1 AND id_hash = $2 AND revoked_at IS NULL",
                &[&realm_id, &id_hash, &at],
            )
            .await
            .map_err(pe)?;
        tx.execute(
            "UPDATE hfs_idp_refresh_families SET revoked_at = $3, revoked_reason = 'session ended'
             WHERE realm_id = $1 AND sso_session_id = $2 AND revoked_at IS NULL",
            &[&realm_id, &id_hash, &at],
        )
        .await
        .map_err(pe)?;
        tx.commit().await.map_err(pe)?;
        Ok(n == 1)
    }

    async fn list_sessions(
        &self,
        realm_id: &str,
        user_id: Option<&str>,
    ) -> Result<Vec<SsoSession>, IdpError> {
        let c = self.client().await?;
        let rows = c
            .query(
                &format!(
                    "SELECT {SESSION_COLS} FROM hfs_idp_sessions
                     WHERE realm_id = $1 AND ($2::text IS NULL OR user_id = $2)
                     ORDER BY created_at DESC"
                ),
                &[&realm_id, &user_id],
            )
            .await
            .map_err(pe)?;
        Ok(rows.iter().map(session_row).collect())
    }

    async fn get_consent(
        &self,
        realm_id: &str,
        user_id: &str,
        client_id: &str,
        endpoint: &str,
    ) -> Result<Option<Consent>, IdpError> {
        let c = self.client().await?;
        Ok(c.query_opt(
            "SELECT realm_id, user_id, client_id, endpoint, scopes, granted_at, expires_at
                 FROM hfs_idp_consents
                 WHERE realm_id = $1 AND user_id = $2 AND client_id = $3 AND endpoint = $4",
            &[&realm_id, &user_id, &client_id, &endpoint],
        )
        .await
        .map_err(pe)?
        .as_ref()
        .map(consent_row))
    }

    async fn put_consent(&self, k: Consent) -> Result<(), IdpError> {
        let c = self.client().await?;
        c.execute(
            "INSERT INTO hfs_idp_consents (realm_id, user_id, client_id, endpoint, scopes, granted_at, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (realm_id, user_id, client_id, endpoint) DO UPDATE
                 SET scopes = EXCLUDED.scopes, granted_at = EXCLUDED.granted_at, expires_at = EXCLUDED.expires_at",
            &[
                &k.realm_id,
                &k.user_id,
                &k.client_id,
                &k.endpoint,
                &k.scopes,
                &k.granted_at,
                &k.expires_at,
            ],
        )
        .await
        .map_err(pe)?;
        Ok(())
    }

    async fn delete_consent(
        &self,
        realm_id: &str,
        user_id: &str,
        client_id: &str,
        endpoint: &str,
    ) -> Result<bool, IdpError> {
        let c = self.client().await?;
        Ok(c.execute(
            "DELETE FROM hfs_idp_consents WHERE realm_id = $1 AND user_id = $2 AND client_id = $3 AND endpoint = $4",
            &[&realm_id, &user_id, &client_id, &endpoint],
        )
        .await
        .map_err(pe)?
            == 1)
    }

    async fn list_consents(&self, realm_id: &str, user_id: &str) -> Result<Vec<Consent>, IdpError> {
        let c = self.client().await?;
        let rows = c
            .query(
                "SELECT realm_id, user_id, client_id, endpoint, scopes, granted_at, expires_at
                 FROM hfs_idp_consents WHERE realm_id = $1 AND user_id = $2 ORDER BY client_id, endpoint",
                &[&realm_id, &user_id],
            )
            .await
            .map_err(pe)?;
        Ok(rows.iter().map(consent_row).collect())
    }

    async fn put_grant(&self, g: GrantRecord) -> Result<(), IdpError> {
        let c = self.client().await?;
        c.execute(
            "INSERT INTO hfs_idp_grants (id, realm_id, client_id, subject, scopes, audience, context,
                 fhir_user, sso_session_id, auth_time, created_at, expires_at, revoked_at, revoked_reason)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
            &[
                &g.id,
                &g.realm_id,
                &g.client_id,
                &g.subject,
                &g.scopes,
                &g.audience,
                &to_json(&g.context)?,
                &g.fhir_user,
                &g.sso_session_id,
                &g.auth_time,
                &g.created_at,
                &g.expires_at,
                &g.revoked_at,
                &g.revoked_reason,
            ],
        )
        .await
        .map_err(pe)?;
        Ok(())
    }

    async fn get_grant(
        &self,
        realm_id: &str,
        grant_id: &str,
    ) -> Result<Option<GrantRecord>, IdpError> {
        let c = self.client().await?;
        c.query_opt(
            &format!("SELECT {GRANT_COLS} FROM hfs_idp_grants WHERE realm_id = $1 AND id = $2"),
            &[&realm_id, &grant_id],
        )
        .await
        .map_err(pe)?
        .as_ref()
        .map(grant_row)
        .transpose()
    }

    async fn extend_grant(
        &self,
        realm_id: &str,
        grant_id: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<(), IdpError> {
        let c = self.client().await?;
        c.execute(
            "UPDATE hfs_idp_grants SET expires_at = $3
             WHERE realm_id = $1 AND id = $2 AND expires_at < $3",
            &[&realm_id, &grant_id, &expires_at],
        )
        .await
        .map_err(pe)?;
        Ok(())
    }

    async fn revoke_grant(
        &self,
        realm_id: &str,
        grant_id: &str,
        reason: &str,
        at: DateTime<Utc>,
    ) -> Result<bool, IdpError> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(pe)?;
        let n = tx
            .execute(
                "UPDATE hfs_idp_grants SET revoked_at = $3, revoked_reason = $4
                 WHERE realm_id = $1 AND id = $2 AND revoked_at IS NULL",
                &[&realm_id, &grant_id, &at, &reason],
            )
            .await
            .map_err(pe)?;
        tx.execute(
            "UPDATE hfs_idp_refresh_families SET revoked_at = $3, revoked_reason = $4
             WHERE realm_id = $1 AND grant_id = $2 AND revoked_at IS NULL",
            &[&realm_id, &grant_id, &at, &reason],
        )
        .await
        .map_err(pe)?;
        tx.commit().await.map_err(pe)?;
        Ok(n == 1)
    }

    async fn revoke_all_grants(
        &self,
        realm_id: &str,
        reason: &str,
        at: DateTime<Utc>,
    ) -> Result<u64, IdpError> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(pe)?;
        let n = tx
            .execute(
                "UPDATE hfs_idp_grants SET revoked_at = $2, revoked_reason = $3
                 WHERE realm_id = $1 AND revoked_at IS NULL",
                &[&realm_id, &at, &reason],
            )
            .await
            .map_err(pe)?;
        tx.execute(
            "UPDATE hfs_idp_refresh_families SET revoked_at = $2, revoked_reason = $3
             WHERE realm_id = $1 AND revoked_at IS NULL",
            &[&realm_id, &at, &reason],
        )
        .await
        .map_err(pe)?;
        tx.commit().await.map_err(pe)?;
        Ok(n)
    }

    async fn list_grants(
        &self,
        realm_id: &str,
        f: &GrantFilter,
    ) -> Result<Vec<GrantRecord>, IdpError> {
        let c = self.client().await?;
        let limit = f.limit.unwrap_or(1000).min(10_000) as i64;
        let rows = c
            .query(
                &format!(
                    "SELECT {GRANT_COLS} FROM hfs_idp_grants
                     WHERE realm_id = $1 AND ($2::text IS NULL OR subject = $2)
                       AND ($3::text IS NULL OR client_id = $3)
                       AND ($4 OR revoked_at IS NULL)
                     ORDER BY created_at DESC LIMIT $5"
                ),
                &[
                    &realm_id,
                    &f.subject,
                    &f.client_id,
                    &f.include_revoked,
                    &limit,
                ],
            )
            .await
            .map_err(pe)?;
        rows.iter().map(grant_row).collect()
    }

    async fn revoked_grant_ids(
        &self,
        realm_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, IdpError> {
        let c = self.client().await?;
        let rows = c
            .query(
                "SELECT id FROM hfs_idp_grants
                 WHERE realm_id = $1 AND revoked_at IS NOT NULL AND expires_at > $2",
                &[&realm_id, &now],
            )
            .await
            .map_err(pe)?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    async fn put_code(&self, code: AuthorizationCode) -> Result<(), IdpError> {
        let c = self.client().await?;
        c.execute(
            "INSERT INTO hfs_idp_codes (code_hash, realm_id, grant_id, payload, expires_at)
             VALUES ($1, $2, $3, $4, $5)",
            &[
                &code.code_hash,
                &code.realm_id,
                &code.grant_id,
                &to_json(&code)?,
                &code.expires_at,
            ],
        )
        .await
        .map_err(pe)?;
        Ok(())
    }

    async fn take_code(
        &self,
        realm_id: &str,
        code_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<CodeTake, IdpError> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(pe)?;
        let taken = tx
            .query_opt(
                "DELETE FROM hfs_idp_codes WHERE realm_id = $1 AND code_hash = $2
                 RETURNING payload, grant_id, expires_at",
                &[&realm_id, &code_hash],
            )
            .await
            .map_err(pe)?;
        if let Some(r) = taken {
            let gid: String = r.get(1);
            let exp: DateTime<Utc> = r.get(2);
            let tomb = exp.max(now) + Duration::hours(1);
            tx.execute(
                "INSERT INTO hfs_idp_code_tombstones (code_hash, realm_id, grant_id, expires_at)
                 VALUES ($1, $2, $3, $4) ON CONFLICT (code_hash) DO NOTHING",
                &[&code_hash, &realm_id, &gid, &tomb],
            )
            .await
            .map_err(pe)?;
            tx.commit().await.map_err(pe)?;
            if exp <= now {
                return Ok(CodeTake::Unknown);
            }
            return Ok(CodeTake::Fresh(Box::new(from_json(r.get(0))?)));
        }
        let tomb = tx
            .query_opt(
                "SELECT grant_id FROM hfs_idp_code_tombstones
                 WHERE realm_id = $1 AND code_hash = $2 AND expires_at > $3",
                &[&realm_id, &code_hash, &now],
            )
            .await
            .map_err(pe)?;
        tx.commit().await.map_err(pe)?;
        Ok(match tomb {
            Some(r) => CodeTake::Replayed { grant_id: r.get(0) },
            None => CodeTake::Unknown,
        })
    }

    async fn put_launch(&self, h: LaunchHandle) -> Result<(), IdpError> {
        let c = self.client().await?;
        c.execute(
            "INSERT INTO hfs_idp_launches (handle_hash, realm_id, payload, expires_at) VALUES ($1, $2, $3, $4)",
            &[&h.handle_hash, &h.realm_id, &to_json(&h)?, &h.expires_at],
        )
        .await
        .map_err(pe)?;
        Ok(())
    }

    async fn take_launch(
        &self,
        realm_id: &str,
        handle_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<LaunchHandle>, IdpError> {
        let c = self.client().await?;
        let r = c
            .query_opt(
                "DELETE FROM hfs_idp_launches WHERE realm_id = $1 AND handle_hash = $2
                 RETURNING payload, expires_at",
                &[&realm_id, &handle_hash],
            )
            .await
            .map_err(pe)?;
        match r {
            Some(r) if r.get::<_, DateTime<Utc>>(1) > now => Ok(Some(from_json(r.get(0))?)),
            _ => Ok(None),
        }
    }

    async fn record_jti(
        &self,
        realm_id: &str,
        client_id: &str,
        jti: &str,
        expires_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<bool, IdpError> {
        let c = self.client().await?;
        // Insert, or reclaim an expired row; a live row means replay.
        let n = c
            .execute(
                "INSERT INTO hfs_idp_jti (realm_id, client_id, jti, expires_at) VALUES ($1, $2, $3, $4)
                 ON CONFLICT (realm_id, client_id, jti) DO UPDATE SET expires_at = EXCLUDED.expires_at
                 WHERE hfs_idp_jti.expires_at <= $5",
                &[&realm_id, &client_id, &jti, &expires_at, &now],
            )
            .await
            .map_err(pe)?;
        Ok(n == 1)
    }

    async fn put_interaction(&self, p: PendingAuthorization) -> Result<(), IdpError> {
        let c = self.client().await?;
        c.execute(
            "INSERT INTO hfs_idp_interactions (id_hash, realm_id, payload, expires_at) VALUES ($1, $2, $3, $4)",
            &[&p.id_hash, &p.realm_id, &to_json(&p)?, &p.expires_at],
        )
        .await
        .map_err(pe)?;
        Ok(())
    }

    async fn get_interaction(
        &self,
        realm_id: &str,
        id_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<PendingAuthorization>, IdpError> {
        let c = self.client().await?;
        c.query_opt(
            "SELECT payload FROM hfs_idp_interactions WHERE realm_id = $1 AND id_hash = $2 AND expires_at > $3",
            &[&realm_id, &id_hash, &now],
        )
        .await
        .map_err(pe)?
        .map(|r| from_json(r.get(0)))
        .transpose()
    }

    async fn update_interaction(&self, p: PendingAuthorization) -> Result<bool, IdpError> {
        let c = self.client().await?;
        Ok(c.execute(
            "UPDATE hfs_idp_interactions SET payload = $3
             WHERE realm_id = $1 AND id_hash = $2 AND expires_at > NOW()",
            &[&p.realm_id, &p.id_hash, &to_json(&p)?],
        )
        .await
        .map_err(pe)?
            == 1)
    }

    async fn take_interaction(
        &self,
        realm_id: &str,
        id_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<PendingAuthorization>, IdpError> {
        let c = self.client().await?;
        let r = c
            .query_opt(
                "DELETE FROM hfs_idp_interactions WHERE realm_id = $1 AND id_hash = $2
                 RETURNING payload, expires_at",
                &[&realm_id, &id_hash],
            )
            .await
            .map_err(pe)?;
        match r {
            Some(r) if r.get::<_, DateTime<Utc>>(1) > now => Ok(Some(from_json(r.get(0))?)),
            _ => Ok(None),
        }
    }

    async fn create_family(
        &self,
        realm_id: &str,
        f: RefreshFamily,
        first_token_hash: &str,
    ) -> Result<(), IdpError> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(pe)?;
        let generation = f.current_generation as i32;
        tx.execute(
            "INSERT INTO hfs_idp_refresh_families (family_id, realm_id, grant_id, client_id, subject,
                 current_generation, payload, sso_session_id, created_at, last_used_at, absolute_expiry)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
            &[
                &f.family_id,
                &realm_id,
                &f.grant_id,
                &f.client_id,
                &f.subject,
                &generation,
                &to_json(&f)?,
                &f.sso_session_id,
                &f.created_at,
                &f.last_used_at,
                &f.absolute_expiry,
            ],
        )
        .await
        .map_err(pe)?;
        tx.execute(
            "INSERT INTO hfs_idp_refresh_tokens (token_hash, family_id, generation) VALUES ($1, $2, $3)",
            &[&first_token_hash, &f.family_id, &generation],
        )
        .await
        .map_err(pe)?;
        tx.commit().await.map_err(pe)
    }

    async fn get_family_by_token(
        &self,
        realm_id: &str,
        token_hash: &str,
    ) -> Result<Option<(RefreshFamily, u32)>, IdpError> {
        let c = self.client().await?;
        let r = c
            .query_opt(
                &format!(
                    "SELECT {FAMILY_COLS}, t.generation FROM hfs_idp_refresh_tokens t
                     JOIN hfs_idp_refresh_families f ON f.family_id = t.family_id
                     WHERE t.token_hash = $2 AND f.realm_id = $1"
                ),
                &[&realm_id, &token_hash],
            )
            .await
            .map_err(pe)?;
        match r {
            Some(r) => {
                let g: i32 = r.get("generation");
                Ok(Some((family_row(&r)?, g as u32)))
            }
            None => Ok(None),
        }
    }

    async fn rotate_family(
        &self,
        realm_id: &str,
        family_id: &str,
        from_generation: u32,
        new_token_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, IdpError> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(pe)?;
        let from = from_generation as i32;
        let next = from + 1;
        let n = tx
            .execute(
                "UPDATE hfs_idp_refresh_families SET current_generation = $4, last_used_at = $5
                 WHERE realm_id = $1 AND family_id = $2 AND current_generation = $3 AND revoked_at IS NULL",
                &[&realm_id, &family_id, &from, &next, &now],
            )
            .await
            .map_err(pe)?;
        if n != 1 {
            tx.rollback().await.map_err(pe)?;
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO hfs_idp_refresh_tokens (token_hash, family_id, generation) VALUES ($1, $2, $3)",
            &[&new_token_hash, &family_id, &next],
        )
        .await
        .map_err(pe)?;
        tx.commit().await.map_err(pe)?;
        Ok(true)
    }

    async fn revoke_family(
        &self,
        realm_id: &str,
        family_id: &str,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<(), IdpError> {
        let c = self.client().await?;
        c.execute(
            "UPDATE hfs_idp_refresh_families SET revoked_at = $3, revoked_reason = $4
             WHERE realm_id = $1 AND family_id = $2 AND revoked_at IS NULL",
            &[&realm_id, &family_id, &now, &reason],
        )
        .await
        .map_err(pe)?;
        Ok(())
    }

    async fn purge_expired(&self, now: DateTime<Utc>) -> Result<u64, IdpError> {
        let c = self.client().await?;
        let day = now - Duration::days(1);
        let mut n = 0;
        for (sql, t) in [
            ("DELETE FROM hfs_idp_codes WHERE expires_at <= $1", now),
            (
                "DELETE FROM hfs_idp_code_tombstones WHERE expires_at <= $1",
                now,
            ),
            ("DELETE FROM hfs_idp_launches WHERE expires_at <= $1", now),
            (
                "DELETE FROM hfs_idp_interactions WHERE expires_at <= $1",
                now,
            ),
            ("DELETE FROM hfs_idp_jti WHERE expires_at <= $1", now),
            ("DELETE FROM hfs_idp_sessions WHERE expires_at <= $1", day),
            (
                "DELETE FROM hfs_idp_refresh_families WHERE absolute_expiry <= $1",
                day,
            ),
            (
                "DELETE FROM hfs_idp_grants g WHERE g.expires_at <= $1
                   AND NOT EXISTS (SELECT 1 FROM hfs_idp_refresh_families f
                                   WHERE f.grant_id = g.id)",
                day,
            ),
        ] {
            n += c.execute(sql, &[&t]).await.map_err(pe)?;
        }
        Ok(n)
    }

    async fn config_epoch(&self) -> Result<Option<i64>, IdpError> {
        let c = self.client().await?;
        let r = c
            .query_opt(
                "SELECT epoch FROM hfs_config_epochs WHERE scope = 'idp'",
                &[],
            )
            .await
            .map_err(pe)?;
        Ok(r.map(|r| r.get::<_, i64>(0)))
    }
}
