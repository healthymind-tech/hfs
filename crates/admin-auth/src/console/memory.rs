//! In-process [`ConsoleStore`]: the reference model for tests and the store
//! of non-PostgreSQL deployments (settings and accounts then live until
//! restart; the console says so).

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use helios_endpoints::console::{
    ConsoleAuthSettings, ConsoleRole, ConsoleStore, ConsoleStoreError, ConsoleUser, RoleBinding,
};

#[derive(Default)]
struct Inner {
    settings: Option<(ConsoleAuthSettings, u64)>,
    users: Vec<ConsoleUser>,
    /// (subject, endpoint name) → role.
    bindings: BTreeMap<(String, String), ConsoleRole>,
}

/// See the module docs. `endpoints` lists the endpoint names bindings may
/// name (`None` = any).
#[derive(Default)]
pub struct MemoryConsoleStore {
    inner: Mutex<Inner>,
    endpoints: Option<Vec<String>>,
}

impl MemoryConsoleStore {
    /// An empty store accepting bindings on any endpoint name.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty store accepting bindings only on `endpoints`.
    pub fn with_endpoints(endpoints: Vec<String>) -> Self {
        Self {
            inner: Mutex::default(),
            endpoints: Some(endpoints),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn would_leave_no_admin(users: &[ConsoleUser], id: &str, still_admin: bool) -> bool {
    if still_admin {
        return false;
    }
    let enabled_admin = |u: &ConsoleUser| u.role == ConsoleRole::Admin && !u.disabled;
    let was = users.iter().any(|u| u.id == id && enabled_admin(u));
    let others = users
        .iter()
        .filter(|u| u.id != id && enabled_admin(u))
        .count();
    was && others == 0
}

const LAST_ADMIN: &str = "the last enabled admin account cannot be disabled, demoted or deleted";

#[async_trait]
impl ConsoleStore for MemoryConsoleStore {
    async fn settings(&self) -> Result<Option<(ConsoleAuthSettings, u64)>, ConsoleStoreError> {
        Ok(self.lock().settings.clone())
    }

    async fn save_settings(
        &self,
        settings: &ConsoleAuthSettings,
        expected_revision: u64,
        _actor: &str,
    ) -> Result<u64, ConsoleStoreError> {
        let mut inner = self.lock();
        let current = inner.settings.as_ref().map(|(_, r)| *r).unwrap_or(0);
        if current != expected_revision {
            return Err(ConsoleStoreError::Conflict { current });
        }
        inner.settings = Some((settings.clone(), current + 1));
        Ok(current + 1)
    }

    async fn list_users(&self) -> Result<Vec<ConsoleUser>, ConsoleStoreError> {
        let mut users = self.lock().users.clone();
        users.sort_by_key(|u| u.username.to_lowercase());
        Ok(users)
    }

    async fn user_by_name(&self, username: &str) -> Result<Option<ConsoleUser>, ConsoleStoreError> {
        let lower = username.to_lowercase();
        Ok(self
            .lock()
            .users
            .iter()
            .find(|u| u.username.to_lowercase() == lower)
            .cloned())
    }

    async fn user_by_id(&self, id: &str) -> Result<Option<ConsoleUser>, ConsoleStoreError> {
        Ok(self.lock().users.iter().find(|u| u.id == id).cloned())
    }

    async fn create_user(&self, mut user: ConsoleUser) -> Result<ConsoleUser, ConsoleStoreError> {
        let mut inner = self.lock();
        let lower = user.username.to_lowercase();
        if inner
            .users
            .iter()
            .any(|u| u.username.to_lowercase() == lower)
        {
            return Err(ConsoleStoreError::DuplicateUsername(user.username));
        }
        if user.id.is_empty() {
            user.id = uuid::Uuid::new_v4().to_string();
        }
        let now = Utc::now();
        user.revision = 1;
        user.created_at = now;
        user.updated_at = now;
        user.last_login_at = None;
        inner.users.push(user.clone());
        Ok(user)
    }

    async fn update_user(
        &self,
        mut user: ConsoleUser,
        expected_revision: u64,
    ) -> Result<ConsoleUser, ConsoleStoreError> {
        let mut inner = self.lock();
        let lower = user.username.to_lowercase();
        if inner
            .users
            .iter()
            .any(|u| u.id != user.id && u.username.to_lowercase() == lower)
        {
            return Err(ConsoleStoreError::DuplicateUsername(user.username));
        }
        let still_admin = user.role == ConsoleRole::Admin && !user.disabled;
        if would_leave_no_admin(&inner.users, &user.id, still_admin) {
            // Checked after the existence/revision checks below in PG; keep
            // the same precedence here.
            let current = inner.users.iter().find(|u| u.id == user.id);
            match current {
                None => return Err(ConsoleStoreError::NotFound),
                Some(c) if c.revision != expected_revision => {
                    return Err(ConsoleStoreError::Conflict {
                        current: c.revision,
                    });
                }
                _ => return Err(ConsoleStoreError::Forbidden(LAST_ADMIN.into())),
            }
        }
        let existing = inner
            .users
            .iter_mut()
            .find(|u| u.id == user.id)
            .ok_or(ConsoleStoreError::NotFound)?;
        if existing.revision != expected_revision {
            return Err(ConsoleStoreError::Conflict {
                current: existing.revision,
            });
        }
        if user.password_hash.is_none() {
            user.password_hash = existing.password_hash.clone();
        }
        user.revision = existing.revision + 1;
        user.created_at = existing.created_at;
        user.last_login_at = existing.last_login_at;
        user.updated_at = Utc::now();
        *existing = user.clone();
        Ok(user)
    }

    async fn delete_user(&self, id: &str) -> Result<(), ConsoleStoreError> {
        let mut inner = self.lock();
        let Some(user) = inner.users.iter().find(|u| u.id == id).cloned() else {
            return Err(ConsoleStoreError::NotFound);
        };
        if would_leave_no_admin(&inner.users, id, false) {
            return Err(ConsoleStoreError::Forbidden(LAST_ADMIN.into()));
        }
        inner.users.retain(|u| u.id != id);
        let subject = format!("local:{}", user.username.to_lowercase());
        inner.bindings.retain(|(s, _), _| *s != subject);
        Ok(())
    }

    async fn record_login(&self, id: &str, at: DateTime<Utc>) -> Result<(), ConsoleStoreError> {
        if let Some(u) = self.lock().users.iter_mut().find(|u| u.id == id) {
            u.last_login_at = Some(at);
        }
        Ok(())
    }

    async fn endpoint_roles(
        &self,
        subject: &str,
    ) -> Result<BTreeMap<String, ConsoleRole>, ConsoleStoreError> {
        Ok(self
            .lock()
            .bindings
            .iter()
            .filter(|((s, _), _)| s == subject)
            .map(|((_, e), r)| (e.clone(), *r))
            .collect())
    }

    async fn list_role_bindings(&self) -> Result<Vec<RoleBinding>, ConsoleStoreError> {
        Ok(self
            .lock()
            .bindings
            .iter()
            .map(|((s, e), r)| RoleBinding {
                subject: s.clone(),
                endpoint: e.clone(),
                role: *r,
            })
            .collect())
    }

    async fn set_role_binding(
        &self,
        subject: &str,
        endpoint: &str,
        role: ConsoleRole,
        _actor: &str,
    ) -> Result<(), ConsoleStoreError> {
        if let Some(known) = &self.endpoints
            && !known.iter().any(|e| e == endpoint)
        {
            return Err(ConsoleStoreError::NotFound);
        }
        self.lock()
            .bindings
            .insert((subject.into(), endpoint.into()), role);
        Ok(())
    }

    async fn delete_role_binding(
        &self,
        subject: &str,
        endpoint: &str,
    ) -> Result<(), ConsoleStoreError> {
        self.lock()
            .bindings
            .remove(&(subject.to_string(), endpoint.to_string()))
            .map(|_| ())
            .ok_or(ConsoleStoreError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(name: &str, role: ConsoleRole) -> ConsoleUser {
        ConsoleUser {
            id: String::new(),
            username: name.into(),
            display_name: None,
            email: None,
            role,
            disabled: false,
            must_change_password: false,
            password_hash: Some("h".into()),
            revision: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_login_at: None,
        }
    }

    #[tokio::test]
    async fn crud_cas_and_last_admin_rule() {
        let s = MemoryConsoleStore::with_endpoints(vec!["lab".into()]);
        assert_eq!(s.settings().await.unwrap(), None);
        let r = s
            .save_settings(&ConsoleAuthSettings::default(), 0, "a")
            .await
            .unwrap();
        assert_eq!(r, 1);
        assert_eq!(
            s.save_settings(&ConsoleAuthSettings::default(), 0, "a")
                .await,
            Err(ConsoleStoreError::Conflict { current: 1 })
        );

        let alice = s
            .create_user(user("Alice", ConsoleRole::Admin))
            .await
            .unwrap();
        assert_eq!(
            s.create_user(user("ALICE", ConsoleRole::Viewer)).await,
            Err(ConsoleStoreError::DuplicateUsername("ALICE".into()))
        );
        let mut demote = alice.clone();
        demote.role = ConsoleRole::Editor;
        demote.password_hash = None;
        assert!(matches!(
            s.update_user(demote.clone(), 1).await,
            Err(ConsoleStoreError::Forbidden(_))
        ));
        assert!(matches!(
            s.delete_user(&alice.id).await,
            Err(ConsoleStoreError::Forbidden(_))
        ));
        let bob = s
            .create_user(user("bob", ConsoleRole::Admin))
            .await
            .unwrap();
        let u = s.update_user(demote.clone(), 1).await.unwrap();
        assert_eq!((u.role, u.revision), (ConsoleRole::Editor, 2));
        assert_eq!(u.password_hash.as_deref(), Some("h"), "None keeps the hash");
        assert_eq!(
            s.update_user(demote, 1).await,
            Err(ConsoleStoreError::Conflict { current: 2 })
        );
        let mut rename = bob.clone();
        rename.username = "alice".into();
        assert!(matches!(
            s.update_user(rename, 1).await,
            Err(ConsoleStoreError::DuplicateUsername(_))
        ));

        s.set_role_binding("local:alice", "lab", ConsoleRole::Admin, "a")
            .await
            .unwrap();
        assert_eq!(
            s.set_role_binding("local:alice", "x", ConsoleRole::Admin, "a")
                .await,
            Err(ConsoleStoreError::NotFound)
        );
        assert_eq!(s.list_role_bindings().await.unwrap().len(), 1);
        assert_eq!(
            s.endpoint_roles("local:alice").await.unwrap().get("lab"),
            Some(&ConsoleRole::Admin)
        );
        s.delete_user(&alice.id).await.unwrap();
        assert!(s.list_role_bindings().await.unwrap().is_empty());
        assert_eq!(
            s.delete_role_binding("local:alice", "lab").await,
            Err(ConsoleStoreError::NotFound)
        );
        s.record_login(&bob.id, Utc::now()).await.unwrap();
        assert!(
            s.user_by_id(&bob.id)
                .await
                .unwrap()
                .unwrap()
                .last_login_at
                .is_some()
        );
        assert!(s.user_by_name("BOB").await.unwrap().is_some());
    }
}
