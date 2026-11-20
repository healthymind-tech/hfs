//! Cross-instance wake-ups over PostgreSQL `NOTIFY` (feature `postgres`).
//!
//! A subscription notification (or a job edit) may reach an instance other
//! than the one whose worker holds the job's continuous run. The receiving
//! instance publishes `NOTIFY hfs_sync_wake, '<job id>'`; every instance
//! keeps a dedicated `LISTEN` session and wakes the job's run if it holds
//! it, so the pass starts at once instead of at the fallback poll.

use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use helios_endpoints::pg::{deadpool_postgres, tokio_postgres};

use crate::runtime::{RemoteWake, SyncRuntime};

/// The notification channel.
pub const CHANNEL: &str = "hfs_sync_wake";

/// Publishes wake-ups with `pg_notify` over the shared pool.
pub struct PgRemoteWake {
    pool: deadpool_postgres::Pool,
}

impl PgRemoteWake {
    /// Over the server's pool.
    pub fn new(pool: deadpool_postgres::Pool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl RemoteWake for PgRemoteWake {
    async fn publish(&self, job_id: &str) {
        match self.pool.get().await {
            Ok(c) => {
                if let Err(e) = c
                    .execute("SELECT pg_notify($1, $2)", &[&CHANNEL, &job_id])
                    .await
                {
                    tracing::warn!(error = %e, "sync wake NOTIFY failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "sync wake NOTIFY: no connection"),
        }
    }
}

/// Keeps a `LISTEN hfs_sync_wake` session open (reconnecting with backoff)
/// and wakes the named job's run on this instance. Ends when the runtime
/// is dropped.
pub async fn run_wake_listener(config: tokio_postgres::Config, runtime: Weak<SyncRuntime>) {
    let mut backoff = Duration::from_secs(1);
    while runtime.strong_count() > 0 {
        match listen_once(&config, &runtime).await {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => {
                tracing::warn!(error = %e, "sync wake LISTEN session failed; fallback polls still run");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

async fn listen_once(
    config: &tokio_postgres::Config,
    runtime: &Weak<SyncRuntime>,
) -> Result<(), tokio_postgres::Error> {
    let (client, mut connection) = config.connect(tokio_postgres::NoTls).await?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let driver = tokio::spawn(async move {
        while let Some(Ok(msg)) = std::future::poll_fn(|cx| connection.poll_message(cx)).await {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
    if let Err(e) = client.batch_execute(&format!("LISTEN {CHANNEL}")).await {
        driver.abort();
        return Err(e);
    }
    while let Some(msg) = rx.recv().await {
        if let tokio_postgres::AsyncMessage::Notification(n) = msg
            && n.channel() == CHANNEL
        {
            match runtime.upgrade() {
                Some(rt) => rt.env.wake.wake(n.payload()),
                None => break,
            }
        }
    }
    drop(client);
    driver.abort();
    Ok(())
}

/// Installs PostgreSQL wake-ups on a runtime: the publisher and the
/// listener task.
pub fn install(
    runtime: &Arc<SyncRuntime>,
    pool: deadpool_postgres::Pool,
    listen: tokio_postgres::Config,
) -> tokio::task::JoinHandle<()> {
    runtime.set_remote_wake(Arc::new(PgRemoteWake::new(pool)));
    tokio::spawn(run_wake_listener(listen, Arc::downgrade(runtime)))
}
