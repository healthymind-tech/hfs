//! Brute-force protection of the sign-in form (design §8.6, §10.4).
//!
//! Cluster-wide counters in `admin_login_failures` (scope `IDP`, through
//! `helios_admin_auth::AdminSessionStore`): per **account** —
//! `{realm id}:user:{user id}`, or `{realm id}:name:{lower-cased name}`
//! for names that do not exist, so an unknown name locks exactly like a real
//! one and lockout cannot be used to enumerate accounts — and per **client
//! address** `{realm id}:ip:{ip}`. The account threshold and duration come
//! from the realm (`security.lockoutThreshold`, `security.lockoutSecs`,
//! default 5 failures → 15 min); the address threshold is four times the
//! account threshold, so one person mistyping behind a shared NAT does not
//! lock the whole site while spraying from one address still stops.

use std::sync::Arc;

use helios_admin_auth::AdminSessionStore;
use helios_idp::realm::SecurityPolicy;

/// The lockout keys of one attempt.
#[derive(Debug, Clone)]
pub struct Attempt {
    pub account: String,
    pub address: String,
}

impl Attempt {
    pub fn new(realm_id: &str, user_id: Option<&str>, username: &str, ip: &str) -> Self {
        let account = match user_id {
            Some(id) => format!("{realm_id}:user:{id}"),
            None => format!(
                "{realm_id}:name:{}",
                username.trim().to_lowercase().chars().take(256).collect::<String>()
            ),
        };
        Self {
            account,
            address: format!("{realm_id}:ip:{ip}"),
        }
    }
}

/// The lockout service.
#[derive(Clone)]
pub struct Lockout {
    store: Arc<dyn AdminSessionStore>,
}

impl Lockout {
    pub fn new(store: Arc<dyn AdminSessionStore>) -> Self {
        Self { store }
    }

    fn address_max(p: &SecurityPolicy) -> u32 {
        p.lockout_threshold.max(1).saturating_mul(4)
    }

    /// The end of the lockout (Unix seconds) when either key is locked now.
    /// A store failure locks (fail closed).
    pub async fn locked_until(&self, a: &Attempt) -> Option<i64> {
        let mut until = None;
        for key in [&a.account, &a.address] {
            match self.store.locked_until(key).await {
                Ok(Some(t)) => until = Some(until.map_or(t, |u: i64| u.max(t))),
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(error = %e, "idp lockout store unavailable; refusing sign-in");
                    return Some(chrono::Utc::now().timestamp() + 60);
                }
            }
        }
        until
    }

    /// Counts a failure; returns the lockout end when this failure locked.
    pub async fn failed(&self, a: &Attempt, p: &SecurityPolicy) -> Option<i64> {
        let secs = p.lockout_secs.max(1) as i64;
        let mut locked = None;
        let account = self
            .store
            .record_failure(&a.account, secs, p.lockout_threshold.max(1), secs)
            .await;
        let address = self
            .store
            .record_failure(&a.address, secs, Self::address_max(p), secs)
            .await;
        for r in [account, address] {
            match r {
                Ok(Some(t)) => locked = Some(locked.map_or(t, |u: i64| u.max(t))),
                Ok(None) => {}
                Err(e) => tracing::error!(error = %e, "idp lockout store: cannot count a failure"),
            }
        }
        locked
    }

    /// Forgets the account's failures after a successful sign-in (the
    /// address counter keeps running: one success must not reset a spray).
    pub async fn succeeded(&self, a: &Attempt) {
        if let Err(e) = self.store.clear_failures(&a.account).await {
            tracing::warn!(error = %e, "idp lockout store: cannot clear failures");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_admin_auth::store::testing::MemoryStore;

    #[tokio::test]
    async fn locks_after_threshold_and_unknown_names_behave_alike() {
        let l = Lockout::new(Arc::new(MemoryStore::default()));
        let p = SecurityPolicy {
            lockout_threshold: 3,
            lockout_secs: 600,
            ..Default::default()
        };
        let real = Attempt::new("r1", Some("u1"), "alice", "10.0.0.1");
        let ghost = Attempt::new("r1", None, "Nobody", "10.0.0.2");
        for a in [&real, &ghost] {
            assert!(l.locked_until(a).await.is_none());
            assert!(l.failed(a, &p).await.is_none());
            assert!(l.failed(a, &p).await.is_none());
            assert!(l.failed(a, &p).await.is_some(), "third failure locks");
            assert!(l.locked_until(a).await.is_some());
        }
        // Another account from the first address is not locked yet (4×3).
        let other = Attempt::new("r1", Some("u2"), "bob", "10.0.0.1");
        assert!(l.locked_until(&other).await.is_none());
        // Realms are separate.
        let elsewhere = Attempt::new("r2", Some("u1"), "alice", "10.0.0.9");
        assert!(l.locked_until(&elsewhere).await.is_none());
    }

    #[tokio::test]
    async fn address_threshold_stops_spraying() {
        let l = Lockout::new(Arc::new(MemoryStore::default()));
        let p = SecurityPolicy {
            lockout_threshold: 2,
            lockout_secs: 600,
            ..Default::default()
        };
        for i in 0..8 {
            let a = Attempt::new("r", None, &format!("user{i}"), "198.51.100.7");
            l.failed(&a, &p).await;
        }
        let fresh = Attempt::new("r", Some("u-new"), "new", "198.51.100.7");
        assert!(l.locked_until(&fresh).await.is_some());
    }
}
