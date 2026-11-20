//! PostgreSQL [`ConsoleStore`] (package P3, feature `postgres`).
//!
//! Tables come from fork migration `hm-0009-console-auth`
//! (`crates/persistence/src/backends/postgres/schema.rs`):
//! `hfs_console_settings` (one row, id 1), `hfs_console_users`,
//! `hfs_console_role_bindings` (endpoint by id, FK to `hfs_endpoints`). Their
//! statement triggers bump `hfs_config_epochs('console')`, so every instance
//! learns of a change.
//!
//! Every write is a compare-and-set on `revision`; the "last enabled admin"
//! rule is checked inside the writing transaction under a transaction-scoped
//! advisory lock, so two concurrent demotions cannot both succeed.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::console::{
    ConsoleAuthSettings, ConsoleRole, ConsoleStore, ConsoleStoreError, ConsoleUser, RoleBinding,
};

/// Serializes last-admin checks across instances ("HMCONADM").
const ADMIN_LOCK: i64 = 0x484d_434f_4e41_444du64 as i64;

const USER_COLUMNS: &str = "id, username, display_name, email, role, password_hash, disabled, \
     must_change_password, revision, created_at, updated_at, last_login_at";

fn unavailable(e: impl std::fmt::Display) -> ConsoleStoreError {
    ConsoleStoreError::Unavailable(e.to_string())
}

fn user_from_row(row: &tokio_postgres::Row) -> Result<ConsoleUser, ConsoleStoreError> {
    let role: String = row.get(4);
    Ok(ConsoleUser {
        id: row.get(0),
        username: row.get(1),
        display_name: row.get(2),
        email: row.get(3),
        role: ConsoleRole::parse(&role)
            .ok_or_else(|| unavailable(format!("stored role {role:?} is unknown")))?,
        password_hash: row.get(5),
        disabled: row.get(6),
        must_change_password: row.get(7),
        revision: row.get::<_, i64>(8) as u64,
        created_at: row.get(9),
        updated_at: row.get(10),
        last_login_at: row.get(11),
    })
}

fn unique_violation(e: &tokio_postgres::Error) -> bool {
    e.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION)
}

/// [`ConsoleStore`] over the shared `deadpool_postgres::Pool`.
#[derive(Clone)]
pub struct PgConsoleStore {
    pool: deadpool_postgres::Pool,
}

impl std::fmt::Debug for PgConsoleStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgConsoleStore").finish_non_exhaustive()
    }
}

impl PgConsoleStore {
    /// Wraps `pool`. The tables must exist (fork migrations ran).
    pub fn new(pool: deadpool_postgres::Pool) -> Self {
        Self { pool }
    }

    async fn client(&self) -> Result<deadpool_postgres::Client, ConsoleStoreError> {
        self.pool.get().await.map_err(unavailable)
    }
}

/// Refuses a change that leaves no enabled admin. `excluding` is the user
/// being changed; `still_admin` whether it stays an enabled admin.
async fn check_last_admin(
    tx: &deadpool_postgres::Transaction<'_>,
    excluding: &str,
    still_admin: bool,
) -> Result<(), ConsoleStoreError> {
    if still_admin {
        return Ok(());
    }
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&ADMIN_LOCK])
        .await
        .map_err(unavailable)?;
    let row = tx
        .query_one(
            "SELECT
               (SELECT COUNT(*) FROM hfs_console_users
                 WHERE role = 'admin' AND NOT disabled AND id <> $1),
               (SELECT COUNT(*) FROM hfs_console_users
                 WHERE id = $1 AND role = 'admin' AND NOT disabled)",
            &[&excluding],
        )
        .await
        .map_err(unavailable)?;
    let others: i64 = row.get(0);
    let was_admin: i64 = row.get(1);
    if was_admin > 0 && others == 0 {
        return Err(ConsoleStoreError::Forbidden(
            "the last enabled admin account cannot be disabled, demoted or deleted".into(),
        ));
    }
    Ok(())
}

#[async_trait]
impl ConsoleStore for PgConsoleStore {
    async fn settings(&self) -> Result<Option<(ConsoleAuthSettings, u64)>, ConsoleStoreError> {
        let client = self.client().await?;
        let row = client
            .query_opt(
                "SELECT settings, revision FROM hfs_console_settings WHERE id = 1",
                &[],
            )
            .await
            .map_err(unavailable)?;
        match row {
            None => Ok(None),
            Some(row) => {
                let json: serde_json::Value = row.get(0);
                let settings = serde_json::from_value(json)
                    .map_err(|e| unavailable(format!("stored console settings: {e}")))?;
                Ok(Some((settings, row.get::<_, i64>(1) as u64)))
            }
        }
    }

    async fn save_settings(
        &self,
        settings: &ConsoleAuthSettings,
        expected_revision: u64,
        actor: &str,
    ) -> Result<u64, ConsoleStoreError> {
        let json = serde_json::to_value(settings).map_err(unavailable)?;
        let client = self.client().await?;
        if expected_revision == 0 {
            let row = client
                .query_opt(
                    "INSERT INTO hfs_console_settings (id, settings, revision, updated_by)
                     VALUES (1, $1, 1, $2)
                     ON CONFLICT (id) DO NOTHING
                     RETURNING revision",
                    &[&json, &actor],
                )
                .await
                .map_err(unavailable)?;
            if let Some(row) = row {
                return Ok(row.get::<_, i64>(0) as u64);
            }
        } else {
            let row = client
                .query_opt(
                    "UPDATE hfs_console_settings
                        SET settings = $1, revision = revision + 1, updated_at = NOW(),
                            updated_by = $2
                      WHERE id = 1 AND revision = $3
                      RETURNING revision",
                    &[&json, &actor, &(expected_revision as i64)],
                )
                .await
                .map_err(unavailable)?;
            if let Some(row) = row {
                return Ok(row.get::<_, i64>(0) as u64);
            }
        }
        let current = client
            .query_opt(
                "SELECT revision FROM hfs_console_settings WHERE id = 1",
                &[],
            )
            .await
            .map_err(unavailable)?
            .map(|r| r.get::<_, i64>(0) as u64)
            .unwrap_or(0);
        Err(ConsoleStoreError::Conflict { current })
    }

    async fn list_users(&self) -> Result<Vec<ConsoleUser>, ConsoleStoreError> {
        let client = self.client().await?;
        let rows = client
            .query(
                &format!("SELECT {USER_COLUMNS} FROM hfs_console_users ORDER BY lower(username)"),
                &[],
            )
            .await
            .map_err(unavailable)?;
        rows.iter().map(user_from_row).collect()
    }

    async fn user_by_name(&self, username: &str) -> Result<Option<ConsoleUser>, ConsoleStoreError> {
        let client = self.client().await?;
        let row = client
            .query_opt(
                &format!(
                    "SELECT {USER_COLUMNS} FROM hfs_console_users WHERE lower(username) = lower($1)"
                ),
                &[&username],
            )
            .await
            .map_err(unavailable)?;
        row.as_ref().map(user_from_row).transpose()
    }

    async fn user_by_id(&self, id: &str) -> Result<Option<ConsoleUser>, ConsoleStoreError> {
        let client = self.client().await?;
        let row = client
            .query_opt(
                &format!("SELECT {USER_COLUMNS} FROM hfs_console_users WHERE id = $1"),
                &[&id],
            )
            .await
            .map_err(unavailable)?;
        row.as_ref().map(user_from_row).transpose()
    }

    async fn create_user(&self, user: ConsoleUser) -> Result<ConsoleUser, ConsoleStoreError> {
        let id = if user.id.is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            user.id.clone()
        };
        let client = self.client().await?;
        let row = client
            .query_one(
                &format!(
                    "INSERT INTO hfs_console_users
                       (id, username, display_name, email, role, password_hash, disabled,
                        must_change_password, revision)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 1)
                     RETURNING {USER_COLUMNS}"
                ),
                &[
                    &id,
                    &user.username,
                    &user.display_name,
                    &user.email,
                    &user.role.as_str(),
                    &user.password_hash,
                    &user.disabled,
                    &user.must_change_password,
                ],
            )
            .await
            .map_err(|e| {
                if unique_violation(&e) {
                    ConsoleStoreError::DuplicateUsername(user.username.clone())
                } else {
                    unavailable(e)
                }
            })?;
        user_from_row(&row)
    }

    async fn update_user(
        &self,
        user: ConsoleUser,
        expected_revision: u64,
    ) -> Result<ConsoleUser, ConsoleStoreError> {
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(unavailable)?;
        let current = tx
            .query_opt(
                "SELECT revision FROM hfs_console_users WHERE id = $1 FOR UPDATE",
                &[&user.id],
            )
            .await
            .map_err(unavailable)?
            .map(|r| r.get::<_, i64>(0) as u64)
            .ok_or(ConsoleStoreError::NotFound)?;
        if current != expected_revision {
            return Err(ConsoleStoreError::Conflict { current });
        }
        check_last_admin(
            &tx,
            &user.id,
            user.role == ConsoleRole::Admin && !user.disabled,
        )
        .await?;
        let row = tx
            .query_one(
                &format!(
                    "UPDATE hfs_console_users SET
                        username = $2, display_name = $3, email = $4, role = $5,
                        password_hash = COALESCE($6, password_hash), disabled = $7,
                        must_change_password = $8, revision = revision + 1, updated_at = NOW()
                      WHERE id = $1
                      RETURNING {USER_COLUMNS}"
                ),
                &[
                    &user.id,
                    &user.username,
                    &user.display_name,
                    &user.email,
                    &user.role.as_str(),
                    &user.password_hash,
                    &user.disabled,
                    &user.must_change_password,
                ],
            )
            .await
            .map_err(|e| {
                if unique_violation(&e) {
                    ConsoleStoreError::DuplicateUsername(user.username.clone())
                } else {
                    unavailable(e)
                }
            })?;
        let updated = user_from_row(&row)?;
        tx.commit().await.map_err(unavailable)?;
        Ok(updated)
    }

    async fn delete_user(&self, id: &str) -> Result<(), ConsoleStoreError> {
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(unavailable)?;
        let found = tx
            .query_opt(
                "SELECT username FROM hfs_console_users WHERE id = $1 FOR UPDATE",
                &[&id],
            )
            .await
            .map_err(unavailable)?;
        let Some(found) = found else {
            return Err(ConsoleStoreError::NotFound);
        };
        let username: String = found.get(0);
        check_last_admin(&tx, id, false).await?;
        tx.execute("DELETE FROM hfs_console_users WHERE id = $1", &[&id])
            .await
            .map_err(unavailable)?;
        // Bindings are keyed by subject, not user id: drop the account's.
        tx.execute(
            "DELETE FROM hfs_console_role_bindings WHERE subject = $1",
            &[&format!("local:{}", username.to_lowercase())],
        )
        .await
        .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        Ok(())
    }

    async fn record_login(&self, id: &str, at: DateTime<Utc>) -> Result<(), ConsoleStoreError> {
        let client = self.client().await?;
        // Not a revision bump: signing in is not an edit, and must not make
        // an admin's open edit form conflict.
        client
            .execute(
                "UPDATE hfs_console_users SET last_login_at = $2 WHERE id = $1",
                &[&id, &at],
            )
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    async fn endpoint_roles(
        &self,
        subject: &str,
    ) -> Result<BTreeMap<String, ConsoleRole>, ConsoleStoreError> {
        let client = self.client().await?;
        let rows = client
            .query(
                "SELECT e.name, b.role FROM hfs_console_role_bindings b
                   JOIN hfs_endpoints e ON e.id = b.endpoint_id
                  WHERE b.subject = $1 AND e.status <> 'deleted'",
                &[&subject],
            )
            .await
            .map_err(unavailable)?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let role: String = r.get(1);
                ConsoleRole::parse(&role).map(|role| (r.get::<_, String>(0), role))
            })
            .collect())
    }

    async fn list_role_bindings(&self) -> Result<Vec<RoleBinding>, ConsoleStoreError> {
        let client = self.client().await?;
        let rows = client
            .query(
                "SELECT b.subject, e.name, b.role FROM hfs_console_role_bindings b
                   JOIN hfs_endpoints e ON e.id = b.endpoint_id
                  WHERE e.status <> 'deleted'
                  ORDER BY b.subject, e.name",
                &[],
            )
            .await
            .map_err(unavailable)?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let role: String = r.get(2);
                ConsoleRole::parse(&role).map(|role| RoleBinding {
                    subject: r.get(0),
                    endpoint: r.get(1),
                    role,
                })
            })
            .collect())
    }

    async fn set_role_binding(
        &self,
        subject: &str,
        endpoint: &str,
        role: ConsoleRole,
        actor: &str,
    ) -> Result<(), ConsoleStoreError> {
        let client = self.client().await?;
        let n = client
            .execute(
                "INSERT INTO hfs_console_role_bindings (subject, endpoint_id, role, created_by)
                 SELECT $1, id, $3, $4 FROM hfs_endpoints
                  WHERE name = $2 AND status <> 'deleted'
                 ON CONFLICT (subject, endpoint_id) DO UPDATE SET role = EXCLUDED.role",
                &[&subject, &endpoint, &role.as_str(), &actor],
            )
            .await
            .map_err(unavailable)?;
        if n == 0 {
            return Err(ConsoleStoreError::NotFound);
        }
        Ok(())
    }

    async fn delete_role_binding(
        &self,
        subject: &str,
        endpoint: &str,
    ) -> Result<(), ConsoleStoreError> {
        let client = self.client().await?;
        let n = client
            .execute(
                "DELETE FROM hfs_console_role_bindings
                  WHERE subject = $1
                    AND endpoint_id IN (SELECT id FROM hfs_endpoints WHERE name = $2)",
                &[&subject, &endpoint],
            )
            .await
            .map_err(unavailable)?;
        if n == 0 {
            return Err(ConsoleStoreError::NotFound);
        }
        Ok(())
    }
}
