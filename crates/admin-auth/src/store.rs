//! Cluster-shared administrator sessions and sign-in lockout.
//!
//! [`crate::AdminAuth`] keeps its session table and failed-attempt limiter in
//! process memory. Behind a load balancer that breaks both: a cookie issued
//! by one instance is unknown to the next (the administrator is bounced back
//! to the sign-in page on every hop), and each instance counts failures on
//! its own, so a guesser gets `N × MAX_FAILURES` attempts.
//!
//! An [`AdminSessionStore`] shared by every instance (the PostgreSQL
//! implementation lives behind the `postgres` feature) fixes both. The memory
//! tables stay as the fast path; the store is consulted from the async
//! middleware for a cookie this instance has not confirmed recently, and is
//! written on sign-in, sign-out and failed attempts. Sign-out on one instance
//! reaches the others within [`crate::auth::STORE_RECHECK`].

use async_trait::async_trait;

/// A session as the shared store holds it (Unix seconds).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSession {
    /// The admin user name.
    pub user: String,
    /// When the session was created.
    pub created_unix: i64,
    /// When it was last used (as far as any instance reported).
    pub last_seen_unix: i64,
}

/// Shared session table and lockout counters. Keys are the SHA-256 digest of
/// the session token (the token itself is never stored), hex encoded.
#[async_trait]
pub trait AdminSessionStore: Send + Sync {
    /// Stores a new session.
    async fn put(&self, key: &str, session: &StoredSession) -> Result<(), String>;
    /// Loads a session; `None` when unknown or signed out.
    async fn get(&self, key: &str) -> Result<Option<StoredSession>, String>;
    /// Moves a session's last-use time forward (never backward).
    async fn touch(&self, key: &str, last_seen_unix: i64) -> Result<(), String>;
    /// Removes a session (sign-out, expiry).
    async fn delete(&self, key: &str) -> Result<(), String>;
    /// Counts one failed attempt from `client`: resets the count when the
    /// previous window (`window_secs`) is over, and locks the client for
    /// `lockout_secs` once `max` failures fall in one window. Returns the
    /// lockout end (Unix seconds) when the client is now locked.
    async fn record_failure(
        &self,
        client: &str,
        window_secs: i64,
        max: u32,
        lockout_secs: i64,
    ) -> Result<Option<i64>, String>;
    /// The end of `client`'s lockout (Unix seconds), when it is locked now.
    async fn locked_until(&self, client: &str) -> Result<Option<i64>, String>;
    /// Forgets `client`'s failures (after a successful sign-in).
    async fn clear_failures(&self, client: &str) -> Result<(), String>;
}

/// An in-memory store shared by `Arc` — the reference model for tests (two
/// [`crate::AdminAuth`] values sharing one of these behave like two
/// instances sharing a database).
pub mod testing {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    /// A client's failure count, window start and lockout end (unix seconds).
    type FailureRecord = (u32, i64, Option<i64>);

    /// See the module docs.
    #[derive(Default)]
    pub struct MemoryStore {
        sessions: Mutex<HashMap<String, StoredSession>>,
        failures: Mutex<HashMap<String, FailureRecord>>,
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    #[async_trait]
    impl AdminSessionStore for MemoryStore {
        async fn put(&self, key: &str, session: &StoredSession) -> Result<(), String> {
            self.sessions
                .lock()
                .unwrap()
                .insert(key.to_string(), session.clone());
            Ok(())
        }
        async fn get(&self, key: &str) -> Result<Option<StoredSession>, String> {
            Ok(self.sessions.lock().unwrap().get(key).cloned())
        }
        async fn touch(&self, key: &str, last_seen_unix: i64) -> Result<(), String> {
            if let Some(s) = self.sessions.lock().unwrap().get_mut(key) {
                s.last_seen_unix = s.last_seen_unix.max(last_seen_unix);
            }
            Ok(())
        }
        async fn delete(&self, key: &str) -> Result<(), String> {
            self.sessions.lock().unwrap().remove(key);
            Ok(())
        }
        async fn record_failure(
            &self,
            client: &str,
            window_secs: i64,
            max: u32,
            lockout_secs: i64,
        ) -> Result<Option<i64>, String> {
            let now = now();
            let mut table = self.failures.lock().unwrap();
            let entry = table.entry(client.to_string()).or_insert((0, now, None));
            if now - entry.1 >= window_secs {
                *entry = (0, now, None);
            }
            entry.0 += 1;
            if entry.0 >= max {
                entry.2 = Some(now + lockout_secs);
            }
            Ok(entry.2.filter(|until| *until > now))
        }
        async fn locked_until(&self, client: &str) -> Result<Option<i64>, String> {
            let now = now();
            Ok(self
                .failures
                .lock()
                .unwrap()
                .get(client)
                .and_then(|e| e.2)
                .filter(|until| *until > now))
        }
        async fn clear_failures(&self, client: &str) -> Result<(), String> {
            self.failures.lock().unwrap().remove(client);
            Ok(())
        }
    }
}

/// PostgreSQL implementation (feature `postgres`).
#[cfg(feature = "postgres")]
pub mod postgres {
    use super::*;

    /// Shared admin sessions in two small tables, scoped by product
    /// (`HFS`, `HTS`) so both servers may share one database.
    pub struct PgAdminSessionStore {
        pool: deadpool_postgres::Pool,
        scope: &'static str,
    }

    const SCHEMA_LOCK: i64 = 0x484d_4144_4d53_4553u64 as i64; // "HMADMSES"

    fn err(e: impl std::fmt::Display) -> String {
        e.to_string()
    }

    impl PgAdminSessionStore {
        /// Opens the store over `pool`, creating its tables when missing.
        pub async fn connect(
            pool: deadpool_postgres::Pool,
            scope: &'static str,
        ) -> Result<Self, String> {
            let mut client = pool.get().await.map_err(err)?;
            let tx = client.transaction().await.map_err(err)?;
            // Concurrent `CREATE TABLE IF NOT EXISTS` from instances starting
            // together can collide on the catalog; serialize it.
            tx.execute("SELECT pg_advisory_xact_lock($1)", &[&SCHEMA_LOCK])
                .await
                .map_err(err)?;
            tx.batch_execute(
                "CREATE TABLE IF NOT EXISTS admin_sessions (
                     scope          TEXT NOT NULL,
                     token_digest   TEXT NOT NULL,
                     user_name      TEXT NOT NULL,
                     created_unix   BIGINT NOT NULL,
                     last_seen_unix BIGINT NOT NULL,
                     PRIMARY KEY (scope, token_digest)
                 );
                 CREATE TABLE IF NOT EXISTS admin_login_failures (
                     scope             TEXT NOT NULL,
                     client            TEXT NOT NULL,
                     failures          INTEGER NOT NULL,
                     window_start_unix BIGINT NOT NULL,
                     locked_until_unix BIGINT,
                     PRIMARY KEY (scope, client)
                 );",
            )
            .await
            .map_err(err)?;
            tx.commit().await.map_err(err)?;
            Ok(Self { pool, scope })
        }

        async fn client(&self) -> Result<deadpool_postgres::Client, String> {
            self.pool.get().await.map_err(err)
        }
    }

    #[async_trait]
    impl AdminSessionStore for PgAdminSessionStore {
        async fn put(&self, key: &str, session: &StoredSession) -> Result<(), String> {
            let client = self.client().await?;
            // Opportunistic cleanup: nothing outlives the absolute lifetime.
            client
                .execute(
                    "DELETE FROM admin_sessions
                     WHERE scope = $1 AND created_unix < $2",
                    &[
                        &self.scope,
                        &(session.created_unix
                            - crate::auth::ABSOLUTE_LIFETIME.as_secs() as i64 * 2),
                    ],
                )
                .await
                .map_err(err)?;
            client
                .execute(
                    "INSERT INTO admin_sessions
                         (scope, token_digest, user_name, created_unix, last_seen_unix)
                     VALUES ($1, $2, $3, $4, $5)
                     ON CONFLICT (scope, token_digest) DO UPDATE
                         SET last_seen_unix = GREATEST(admin_sessions.last_seen_unix,
                                                       EXCLUDED.last_seen_unix)",
                    &[
                        &self.scope,
                        &key,
                        &session.user,
                        &session.created_unix,
                        &session.last_seen_unix,
                    ],
                )
                .await
                .map_err(err)?;
            Ok(())
        }

        async fn get(&self, key: &str) -> Result<Option<StoredSession>, String> {
            let client = self.client().await?;
            let row = client
                .query_opt(
                    "SELECT user_name, created_unix, last_seen_unix FROM admin_sessions
                     WHERE scope = $1 AND token_digest = $2",
                    &[&self.scope, &key],
                )
                .await
                .map_err(err)?;
            Ok(row.map(|r| StoredSession {
                user: r.get(0),
                created_unix: r.get(1),
                last_seen_unix: r.get(2),
            }))
        }

        async fn touch(&self, key: &str, last_seen_unix: i64) -> Result<(), String> {
            let client = self.client().await?;
            client
                .execute(
                    "UPDATE admin_sessions SET last_seen_unix = GREATEST(last_seen_unix, $3)
                     WHERE scope = $1 AND token_digest = $2",
                    &[&self.scope, &key, &last_seen_unix],
                )
                .await
                .map_err(err)?;
            Ok(())
        }

        async fn delete(&self, key: &str) -> Result<(), String> {
            let client = self.client().await?;
            client
                .execute(
                    "DELETE FROM admin_sessions WHERE scope = $1 AND token_digest = $2",
                    &[&self.scope, &key],
                )
                .await
                .map_err(err)?;
            Ok(())
        }

        async fn record_failure(
            &self,
            client_key: &str,
            window_secs: i64,
            max: u32,
            lockout_secs: i64,
        ) -> Result<Option<i64>, String> {
            let client = self.client().await?;
            // One atomic upsert: concurrent failures on several instances
            // each count exactly once.
            let row = client
                .query_one(
                    "WITH now AS (SELECT EXTRACT(EPOCH FROM NOW())::BIGINT AS t)
                     INSERT INTO admin_login_failures AS f
                         (scope, client, failures, window_start_unix, locked_until_unix)
                     SELECT $1, $2, 1, now.t,
                            CASE WHEN 1 >= $4 THEN now.t + $5 END
                     FROM now
                     ON CONFLICT (scope, client) DO UPDATE SET
                         failures = CASE
                             WHEN f.window_start_unix + $3 <= EXCLUDED.window_start_unix THEN 1
                             ELSE f.failures + 1 END,
                         window_start_unix = CASE
                             WHEN f.window_start_unix + $3 <= EXCLUDED.window_start_unix
                                 THEN EXCLUDED.window_start_unix
                             ELSE f.window_start_unix END,
                         locked_until_unix = CASE
                             WHEN (CASE
                                     WHEN f.window_start_unix + $3 <= EXCLUDED.window_start_unix
                                         THEN 1
                                     ELSE f.failures + 1 END) >= $4
                                 THEN EXCLUDED.window_start_unix + $5
                             WHEN f.window_start_unix + $3 <= EXCLUDED.window_start_unix THEN NULL
                             ELSE f.locked_until_unix END
                     RETURNING locked_until_unix,
                               (SELECT t FROM now)",
                    &[
                        &self.scope,
                        &client_key,
                        &window_secs,
                        &(max as i32),
                        &lockout_secs,
                    ],
                )
                .await
                .map_err(err)?;
            let until: Option<i64> = row.get(0);
            let now: i64 = row.get(1);
            Ok(until.filter(|u| *u > now))
        }

        async fn locked_until(&self, client_key: &str) -> Result<Option<i64>, String> {
            let client = self.client().await?;
            let row = client
                .query_opt(
                    "SELECT locked_until_unix FROM admin_login_failures
                     WHERE scope = $1 AND client = $2
                       AND locked_until_unix > EXTRACT(EPOCH FROM NOW())::BIGINT",
                    &[&self.scope, &client_key],
                )
                .await
                .map_err(err)?;
            Ok(row.and_then(|r| r.get::<_, Option<i64>>(0)))
        }

        async fn clear_failures(&self, client_key: &str) -> Result<(), String> {
            let client = self.client().await?;
            client
                .execute(
                    "DELETE FROM admin_login_failures WHERE scope = $1 AND client = $2",
                    &[&self.scope, &client_key],
                )
                .await
                .map_err(err)?;
            Ok(())
        }
    }
}
