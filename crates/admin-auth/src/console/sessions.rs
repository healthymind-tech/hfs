//! Console sessions and pending OIDC logins, kept server-side.
//!
//! A session cookie (`hfs_console`) carries a random 256-bit token; only its
//! SHA-256 (hex) is stored. On PostgreSQL the rows live in upstream's
//! `login_sessions` table (v44) with `kind = 'console'` (sessions) and
//! `kind = 'console-pending'` (OIDC logins waiting for the IdP callback), so
//! every instance of a cluster resolves them and they survive restarts.

use async_trait::async_trait;
use helios_endpoints::console::{ConsoleRole, ConsoleVia};
use serde::{Deserialize, Serialize};

/// `login_sessions.kind` of a console session.
pub const KIND_SESSION: &str = "console";
/// `login_sessions.kind` of a pending console OIDC login.
pub const KIND_PENDING: &str = "console-pending";

/// IdP tokens of an OIDC-backed session (never sent to the browser).
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct OidcSessionTokens {
    /// ID token, for RP-initiated logout (`id_token_hint`).
    #[serde(default)]
    pub id_token: Option<String>,
    /// Refresh token, when issued.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// When the access token expires (Unix seconds); the session is
    /// refreshed (and the role re-mapped) after that.
    pub access_expires_unix: i64,
}

impl std::fmt::Debug for OidcSessionTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcSessionTokens")
            .field("access_expires_unix", &self.access_expires_unix)
            .finish_non_exhaustive()
    }
}

/// One signed-in console session.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleSession {
    /// `local:{username}`, `oidc:{iss}|{sub}`, `admin`.
    pub subject: String,
    /// For the account menu.
    pub display_name: String,
    /// Global role at sign-in (refreshed when the account changes).
    pub role: ConsoleRole,
    /// How the user signed in.
    pub via: ConsoleVia,
    /// Local account id (for re-reading the account).
    #[serde(default)]
    pub user_id: Option<String>,
    /// Local account revision the session was last checked against.
    #[serde(default)]
    pub user_revision: u64,
    /// Synchronizer CSRF token (128-bit, base64url).
    pub csrf: String,
    /// Unix seconds.
    pub created_unix: i64,
    /// Unix seconds.
    pub last_seen_unix: i64,
    /// The account must choose a new password before anything else.
    #[serde(default)]
    pub must_change_password: bool,
    /// OIDC tokens.
    #[serde(default)]
    pub oidc: Option<OidcSessionTokens>,
    /// Client address at sign-in (shown on the sessions list).
    #[serde(default)]
    pub client: Option<String>,
    /// Fingerprint of an OIDC configuration whose "Test sign-in" succeeded
    /// in this session (required before switching to mode oidc).
    #[serde(default)]
    pub oidc_test_passed: Option<String>,
}

/// A pending OIDC login (browser sent to the IdP, callback not yet seen).
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PendingLogin {
    /// `state` the IdP must echo.
    pub state: String,
    /// PKCE verifier.
    pub verifier: String,
    /// OpenID Connect nonce.
    pub nonce: String,
    /// Where to land.
    pub next: String,
    /// Unix seconds.
    pub started_unix: i64,
    /// A "Test sign-in" of a draft OIDC configuration (by fingerprint): the
    /// callback validates it and records the result instead of signing in.
    #[serde(default)]
    pub test_of: Option<String>,
    /// The session that started the test (digest), to record the result.
    #[serde(default)]
    pub test_session: Option<String>,
}

impl std::fmt::Debug for PendingLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingLogin")
            .field("next", &self.next)
            .field("started_unix", &self.started_unix)
            .field("test_of", &self.test_of)
            .finish_non_exhaustive()
    }
}

/// Storage of session and pending-login documents, keyed by token digest.
#[async_trait]
pub trait ConsoleSessionStore: Send + Sync {
    /// Inserts or replaces a document.
    async fn put(
        &self,
        key: &str,
        kind: &str,
        data: &serde_json::Value,
        expires_unix: i64,
    ) -> Result<(), String>;
    /// Loads a live (unexpired) document.
    async fn get(&self, key: &str, kind: &str) -> Result<Option<serde_json::Value>, String>;
    /// Deletes; `true` when this call removed it (exactly-once consumption).
    async fn delete(&self, key: &str, kind: &str) -> Result<bool, String>;
    /// Deletes every session of `subject`; returns how many.
    async fn delete_subject(&self, subject: &str) -> Result<u64, String>;
    /// Every live session `(key, document)`.
    async fn list(&self, kind: &str) -> Result<Vec<(String, serde_json::Value)>, String>;
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// In-process store (single instance, tests).
#[derive(Default)]
pub struct MemorySessionStore {
    rows: std::sync::Mutex<std::collections::HashMap<(String, String), (serde_json::Value, i64)>>,
}

#[async_trait]
impl ConsoleSessionStore for MemorySessionStore {
    async fn put(
        &self,
        key: &str,
        kind: &str,
        data: &serde_json::Value,
        expires_unix: i64,
    ) -> Result<(), String> {
        let mut rows = self.rows.lock().unwrap_or_else(|p| p.into_inner());
        let now = now_unix();
        if rows.len() > 10_000 {
            rows.retain(|_, (_, exp)| *exp > now);
        }
        rows.insert((key.into(), kind.into()), (data.clone(), expires_unix));
        Ok(())
    }
    async fn get(&self, key: &str, kind: &str) -> Result<Option<serde_json::Value>, String> {
        let rows = self.rows.lock().unwrap_or_else(|p| p.into_inner());
        Ok(rows
            .get(&(key.to_string(), kind.to_string()))
            .filter(|(_, exp)| *exp > now_unix())
            .map(|(v, _)| v.clone()))
    }
    async fn delete(&self, key: &str, kind: &str) -> Result<bool, String> {
        let mut rows = self.rows.lock().unwrap_or_else(|p| p.into_inner());
        Ok(rows.remove(&(key.to_string(), kind.to_string())).is_some())
    }
    async fn delete_subject(&self, subject: &str) -> Result<u64, String> {
        let mut rows = self.rows.lock().unwrap_or_else(|p| p.into_inner());
        let before = rows.len();
        rows.retain(|(_, kind), (v, _)| {
            !(kind == KIND_SESSION && v.get("subject").and_then(|s| s.as_str()) == Some(subject))
        });
        Ok((before - rows.len()) as u64)
    }
    async fn list(&self, kind: &str) -> Result<Vec<(String, serde_json::Value)>, String> {
        let rows = self.rows.lock().unwrap_or_else(|p| p.into_inner());
        let now = now_unix();
        Ok(rows
            .iter()
            .filter(|((_, k), (_, exp))| k == kind && *exp > now)
            .map(|((key, _), (v, _))| (key.clone(), v.clone()))
            .collect())
    }
}

/// PostgreSQL store over `login_sessions` (features `console` + `postgres`).
#[cfg(feature = "postgres")]
pub mod postgres {
    use super::*;

    /// See the module docs. Row ids are `c:{digest}` so they can never
    /// collide with upstream's own (random) session ids.
    pub struct PgConsoleSessionStore {
        pool: deadpool_postgres::Pool,
    }

    fn err(e: impl std::fmt::Display) -> String {
        e.to_string()
    }

    /// Same key as the admin session store's DDL lock ("HMADMSES"): both
    /// create tables at startup.
    const SCHEMA_LOCK: i64 = 0x484d_4144_4d53_4553u64 as i64;

    fn row_id(key: &str) -> String {
        format!("c:{key}")
    }

    impl PgConsoleSessionStore {
        /// Opens the store, creating `login_sessions` when missing (the same
        /// DDL as upstream's v44 migration, which normally ran already).
        pub async fn connect(pool: deadpool_postgres::Pool) -> Result<Self, String> {
            let mut client = pool.get().await.map_err(err)?;
            let tx = client.transaction().await.map_err(err)?;
            // Instances starting together must not race on the catalog.
            tx.execute("SELECT pg_advisory_xact_lock($1)", &[&SCHEMA_LOCK])
                .await
                .map_err(err)?;
            tx
                .batch_execute(
                    "CREATE TABLE IF NOT EXISTS login_sessions (
                        id         TEXT PRIMARY KEY,
                        kind       TEXT NOT NULL,
                        data       JSONB NOT NULL,
                        version    BIGINT NOT NULL DEFAULT 1,
                        expires_at TIMESTAMPTZ NOT NULL,
                        updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
                    );
                    CREATE INDEX IF NOT EXISTS idx_login_sessions_expires
                        ON login_sessions (expires_at);",
                )
                .await
                .map_err(err)?;
            tx.commit().await.map_err(err)?;
            Ok(Self { pool })
        }
    }

    #[async_trait]
    impl ConsoleSessionStore for PgConsoleSessionStore {
        async fn put(
            &self,
            key: &str,
            kind: &str,
            data: &serde_json::Value,
            expires_unix: i64,
        ) -> Result<(), String> {
            let client = self.pool.get().await.map_err(err)?;
            let text = data.to_string();
            client
                .execute(
                    "INSERT INTO login_sessions (id, kind, data, version, expires_at, updated_at)
                     VALUES ($1, $2, $3::text::jsonb, 1, to_timestamp($4), NOW())
                     ON CONFLICT (id) DO UPDATE SET
                         data = EXCLUDED.data, expires_at = EXCLUDED.expires_at,
                         version = login_sessions.version + 1, updated_at = NOW()",
                    &[&row_id(key), &kind, &text, &(expires_unix as f64)],
                )
                .await
                .map_err(err)?;
            Ok(())
        }

        async fn get(&self, key: &str, kind: &str) -> Result<Option<serde_json::Value>, String> {
            let client = self.pool.get().await.map_err(err)?;
            let row = client
                .query_opt(
                    "SELECT data::text FROM login_sessions
                     WHERE id = $1 AND kind = $2 AND expires_at > NOW()",
                    &[&row_id(key), &kind],
                )
                .await
                .map_err(err)?;
            match row {
                None => Ok(None),
                Some(r) => {
                    let text: String = r.get(0);
                    serde_json::from_str(&text).map(Some).map_err(err)
                }
            }
        }

        async fn delete(&self, key: &str, kind: &str) -> Result<bool, String> {
            let client = self.pool.get().await.map_err(err)?;
            let n = client
                .execute(
                    "DELETE FROM login_sessions WHERE id = $1 AND kind = $2",
                    &[&row_id(key), &kind],
                )
                .await
                .map_err(err)?;
            Ok(n > 0)
        }

        async fn delete_subject(&self, subject: &str) -> Result<u64, String> {
            let client = self.pool.get().await.map_err(err)?;
            client
                .execute(
                    "DELETE FROM login_sessions WHERE kind = $1 AND data->>'subject' = $2",
                    &[&KIND_SESSION, &subject],
                )
                .await
                .map_err(err)
        }

        async fn list(&self, kind: &str) -> Result<Vec<(String, serde_json::Value)>, String> {
            let client = self.pool.get().await.map_err(err)?;
            // Opportunistic sweep of this kind's expired rows.
            let _ = client
                .execute(
                    "DELETE FROM login_sessions WHERE kind = $1 AND expires_at <= NOW()",
                    &[&kind],
                )
                .await;
            let rows = client
                .query(
                    "SELECT id, data::text FROM login_sessions
                     WHERE kind = $1 AND expires_at > NOW() ORDER BY updated_at DESC LIMIT 1000",
                    &[&kind],
                )
                .await
                .map_err(err)?;
            Ok(rows
                .iter()
                .filter_map(|r| {
                    let id: String = r.get(0);
                    let text: String = r.get(1);
                    let key = id.strip_prefix("c:")?.to_string();
                    serde_json::from_str(&text).ok().map(|v| (key, v))
                })
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_store_roundtrip_expiry_and_subject_delete() {
        let s = MemorySessionStore::default();
        let now = now_unix();
        let doc = serde_json::json!({"subject": "local:bob"});
        s.put("k1", KIND_SESSION, &doc, now + 60).await.unwrap();
        s.put("k2", KIND_SESSION, &doc, now - 1).await.unwrap();
        s.put("k3", KIND_PENDING, &doc, now + 60).await.unwrap();
        assert!(s.get("k1", KIND_SESSION).await.unwrap().is_some());
        assert!(s.get("k1", KIND_PENDING).await.unwrap().is_none());
        assert!(s.get("k2", KIND_SESSION).await.unwrap().is_none(), "expired");
        assert_eq!(s.list(KIND_SESSION).await.unwrap().len(), 1);
        assert_eq!(s.delete_subject("local:bob").await.unwrap(), 2);
        assert!(s.get("k3", KIND_PENDING).await.unwrap().is_some());
        assert!(s.delete("k3", KIND_PENDING).await.unwrap());
        assert!(!s.delete("k3", KIND_PENDING).await.unwrap(), "exactly once");
    }

    #[test]
    fn debug_never_prints_tokens() {
        let t = OidcSessionTokens {
            id_token: Some("idtok".into()),
            refresh_token: Some("rtok".into()),
            access_expires_unix: 1,
        };
        let shown = format!("{t:?}");
        assert!(!shown.contains("idtok") && !shown.contains("rtok"));
        let p = PendingLogin {
            state: "st4te".into(),
            verifier: "verif".into(),
            nonce: "n0nce".into(),
            next: "/ui".into(),
            started_unix: 0,
            test_of: None,
            test_session: None,
        };
        let shown = format!("{p:?}");
        assert!(!shown.contains("st4te") && !shown.contains("verif") && !shown.contains("n0nce"));
    }
}
