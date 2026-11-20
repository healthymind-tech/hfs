//! PostgreSQL [`EndpointStore`] over `hfs_endpoints`, `hfs_endpoint_revisions`
//! and `hfs_config_epochs` (fork migration `hm-0008-endpoints`, created by
//! `helios-persistence`'s schema initialization), plus the `LISTEN
//! hfs_config` task that wakes [`crate::run_watcher`] early.
//!
//! Package P1 of `docs/design/fhir-server-manager.md` (feature `postgres`).
//! The store runs over the server's shared `deadpool_postgres::Pool` (the
//! `helios-admin-auth` pattern); it creates no tables itself.
//!
//! Write serialization: every write takes the transaction-scoped advisory
//! lock [`WRITE_LOCK_KEY`] before reading the live endpoints, so the shared
//! [`check_write`] rules see a consistent set across instances; the partial
//! unique indexes are the backstop (a violation maps to
//! [`StoreError::Duplicate`]). The `hfs_endpoints` statement trigger bumps
//! `hfs_config_epochs('endpoints')` and NOTIFYs `hfs_config` in the same
//! transaction, so a committed write is always visible with a newer epoch.

use std::sync::Arc;
use std::time::Duration;

pub use deadpool_postgres;
pub use tokio_postgres;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deadpool_postgres::Pool;
use tokio_postgres::Row;

use crate::model::{Endpoint, EndpointSpec, EndpointStatus};
use crate::registry::EndpointRegistry;
use crate::store::{Actor, ChangeKind, EndpointRevision, EndpointStore, StoreError, check_write};

/// Advisory lock serializing endpoint writes across instances ("HFS\0ENDP").
pub const WRITE_LOCK_KEY: i64 = 0x4846_5300_454e_4450;

/// The NOTIFY channel the `hfs_bump_config_epoch()` trigger announces on.
pub const CONFIG_CHANNEL: &str = "hfs_config";

/// The epoch scope of endpoint configuration.
pub const ENDPOINTS_SCOPE: &str = "endpoints";

const COLUMNS: &str = "id, revision, status, spec, created_at, updated_at, created_by, updated_by";

/// [`EndpointStore`] in the primary PostgreSQL database.
#[derive(Clone)]
pub struct PgEndpointStore {
    pool: Pool,
}

impl std::fmt::Debug for PgEndpointStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgEndpointStore").finish_non_exhaustive()
    }
}

fn unavailable(e: impl std::fmt::Display) -> StoreError {
    StoreError::Unavailable(e.to_string())
}

/// Maps a database error, turning a unique violation of one of the
/// `uq_hfs_endpoints_*` indexes into [`StoreError::Duplicate`].
fn map_db_error(e: tokio_postgres::Error, spec: Option<&EndpointSpec>) -> StoreError {
    if let (Some(db), Some(spec)) = (e.as_db_error(), spec)
        && db.code() == &tokio_postgres::error::SqlState::UNIQUE_VIOLATION
    {
        let constraint = db.constraint().unwrap_or_default();
        let (field, value) = if constraint.ends_with("_path") {
            ("path", spec.path.as_str().to_string())
        } else if constraint.ends_with("_tenant") {
            ("storage.tenantId", spec.storage.tenant_id().to_string())
        } else {
            ("name", spec.name.as_str().to_string())
        };
        return StoreError::Duplicate {
            field,
            value,
            by: "another endpoint".into(),
        };
    }
    unavailable(e)
}

fn row_to_endpoint(row: &Row) -> Result<Endpoint, StoreError> {
    let spec_json: serde_json::Value = row.get("spec");
    let mut spec: EndpointSpec = serde_json::from_value(spec_json)
        .map_err(|e| StoreError::Unavailable(format!("stored endpoint spec is unreadable: {e}")))?;
    // The column is authoritative (status changes write both; a hand edit of
    // the column must still take effect).
    spec.status = parse_status(row.get::<_, &str>("status"))?;
    let revision: i64 = row.get("revision");
    Ok(Endpoint {
        id: row.get("id"),
        revision: u64::try_from(revision).unwrap_or(0),
        spec,
        created_at: row.get::<_, DateTime<Utc>>("created_at"),
        updated_at: row.get::<_, DateTime<Utc>>("updated_at"),
        created_by: row.get("created_by"),
        updated_by: row.get("updated_by"),
    })
}

fn parse_status(s: &str) -> Result<EndpointStatus, StoreError> {
    Ok(match s {
        "enabled" => EndpointStatus::Enabled,
        "disabled" => EndpointStatus::Disabled,
        "provisioning" => EndpointStatus::Provisioning,
        "deleted" => EndpointStatus::Deleted,
        other => {
            return Err(StoreError::Unavailable(format!(
                "unknown endpoint status {other:?} in hfs_endpoints"
            )));
        }
    })
}

fn change_str(c: ChangeKind) -> &'static str {
    match c {
        ChangeKind::Create => "create",
        ChangeKind::Update => "update",
        ChangeKind::Status => "status",
    }
}

fn parse_change(s: &str) -> ChangeKind {
    match s {
        "create" => ChangeKind::Create,
        "status" => ChangeKind::Status,
        _ => ChangeKind::Update,
    }
}

fn spec_json(spec: &EndpointSpec) -> Result<serde_json::Value, StoreError> {
    serde_json::to_value(spec).map_err(|e| StoreError::Unavailable(e.to_string()))
}

fn redacted(spec: &EndpointSpec) -> Result<serde_json::Value, StoreError> {
    let mut v = spec_json(spec)?;
    crate::secret::redact_sealed(&mut v);
    Ok(v)
}

fn sort_key(e: &Endpoint) -> (bool, String, DateTime<Utc>) {
    (!e.spec.is_default(), e.name().to_string(), e.created_at)
}

impl PgEndpointStore {
    /// A store over `pool`. The schema (`hm-0008-endpoints`) must exist —
    /// `PostgresBackend::init_schema` creates it.
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    async fn client(&self) -> Result<deadpool_postgres::Client, StoreError> {
        self.pool.get().await.map_err(unavailable)
    }

    /// Live endpoints read inside a write transaction (after the lock).
    async fn live_in(tx: &deadpool_postgres::Transaction<'_>) -> Result<Vec<Endpoint>, StoreError> {
        let rows = tx
            .query(
                &format!("SELECT {COLUMNS} FROM hfs_endpoints WHERE status <> 'deleted'"),
                &[],
            )
            .await
            .map_err(unavailable)?;
        rows.iter().map(row_to_endpoint).collect()
    }

    async fn record(
        tx: &deadpool_postgres::Transaction<'_>,
        e: &Endpoint,
        change: ChangeKind,
        actor: &Actor,
    ) -> Result<(), StoreError> {
        let revision = i64::try_from(e.revision).unwrap_or(i64::MAX);
        tx.execute(
            "INSERT INTO hfs_endpoint_revisions
                 (endpoint_id, revision, change, status, spec, changed_at, changed_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            &[
                &e.id,
                &revision,
                &change_str(change),
                &e.spec.status.as_str(),
                &redacted(&e.spec)?,
                &e.updated_at,
                &actor.subject,
            ],
        )
        .await
        .map_err(unavailable)?;
        Ok(())
    }

    /// Shared body of `update` and `set_status`.
    async fn replace(
        &self,
        name: &str,
        expected_revision: u64,
        change: ChangeKind,
        make_spec: impl FnOnce(&Endpoint) -> EndpointSpec + Send,
        actor: &Actor,
    ) -> Result<Endpoint, StoreError> {
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(unavailable)?;
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&WRITE_LOCK_KEY])
            .await
            .map_err(unavailable)?;
        let live = Self::live_in(&tx).await?;
        let cur = live
            .iter()
            .find(|e| e.name() == name)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(name.into()))?;
        if cur.revision != expected_revision {
            return Err(StoreError::Conflict {
                current: cur.revision,
            });
        }
        let spec = make_spec(&cur);
        let refs: Vec<&Endpoint> = live.iter().collect();
        check_write(&spec, &refs, Some(&cur), actor)?;
        let now = Utc::now();
        let e = Endpoint {
            revision: cur.revision + 1,
            spec,
            updated_at: now,
            updated_by: Some(actor.subject.clone()),
            ..cur
        };
        let revision = i64::try_from(e.revision).unwrap_or(i64::MAX);
        let deleted_at = (e.spec.status == EndpointStatus::Deleted).then_some(now);
        tx.execute(
            "UPDATE hfs_endpoints
                SET path = $2, status = $3, managed_by = $4, spec = $5, revision = $6,
                    updated_at = $7, updated_by = $8, deleted_at = $9
              WHERE id = $1",
            &[
                &e.id,
                &e.spec.path.as_str(),
                &e.spec.status.as_str(),
                &managed_by_str(&e.spec),
                &spec_json(&e.spec)?,
                &revision,
                &now,
                &actor.subject,
                &deleted_at,
            ],
        )
        .await
        .map_err(|err| map_db_error(err, Some(&e.spec)))?;
        Self::record(&tx, &e, change, actor).await?;
        tx.commit()
            .await
            .map_err(|err| map_db_error(err, Some(&e.spec)))?;
        Ok(e)
    }
}

fn managed_by_str(spec: &EndpointSpec) -> &'static str {
    match spec.managed_by {
        crate::model::ManagedBy::Ui => "ui",
        crate::model::ManagedBy::Env => "env",
    }
}

#[async_trait]
impl EndpointStore for PgEndpointStore {
    async fn epoch(&self) -> Result<u64, StoreError> {
        let client = self.client().await?;
        let row = client
            .query_opt(
                "SELECT epoch FROM hfs_config_epochs WHERE scope = $1",
                &[&ENDPOINTS_SCOPE],
            )
            .await
            .map_err(unavailable)?;
        Ok(row
            .map(|r| u64::try_from(r.get::<_, i64>(0)).unwrap_or(0))
            .unwrap_or(0))
    }

    async fn list(&self, include_deleted: bool) -> Result<Vec<Endpoint>, StoreError> {
        let client = self.client().await?;
        let sql = if include_deleted {
            format!("SELECT {COLUMNS} FROM hfs_endpoints")
        } else {
            format!("SELECT {COLUMNS} FROM hfs_endpoints WHERE status <> 'deleted'")
        };
        let rows = client.query(&sql, &[]).await.map_err(unavailable)?;
        let mut v = rows
            .iter()
            .map(row_to_endpoint)
            .collect::<Result<Vec<_>, _>>()?;
        v.sort_by_key(sort_key);
        Ok(v)
    }

    async fn get(&self, name: &str) -> Result<Option<Endpoint>, StoreError> {
        let client = self.client().await?;
        let row = client
            .query_opt(
                &format!(
                    "SELECT {COLUMNS} FROM hfs_endpoints WHERE name = $1 AND status <> 'deleted'"
                ),
                &[&name],
            )
            .await
            .map_err(unavailable)?;
        row.as_ref().map(row_to_endpoint).transpose()
    }

    async fn create(&self, spec: EndpointSpec, actor: &Actor) -> Result<Endpoint, StoreError> {
        let mut client = self.client().await?;
        let tx = client.transaction().await.map_err(unavailable)?;
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&WRITE_LOCK_KEY])
            .await
            .map_err(unavailable)?;
        let live = Self::live_in(&tx).await?;
        let refs: Vec<&Endpoint> = live.iter().collect();
        check_write(&spec, &refs, None, actor)?;
        let now = Utc::now();
        let e = Endpoint {
            id: uuid::Uuid::new_v4().to_string(),
            revision: 1,
            spec,
            created_at: now,
            updated_at: now,
            created_by: Some(actor.subject.clone()),
            updated_by: Some(actor.subject.clone()),
        };
        tx.execute(
            "INSERT INTO hfs_endpoints
                 (id, name, path, tenant_id, status, managed_by, spec, revision,
                  created_at, updated_at, created_by, updated_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, 1, $8, $8, $9, $9)",
            &[
                &e.id,
                &e.spec.name.as_str(),
                &e.spec.path.as_str(),
                &e.spec.storage.tenant_id(),
                &e.spec.status.as_str(),
                &managed_by_str(&e.spec),
                &spec_json(&e.spec)?,
                &now,
                &actor.subject,
            ],
        )
        .await
        .map_err(|err| map_db_error(err, Some(&e.spec)))?;
        Self::record(&tx, &e, ChangeKind::Create, actor).await?;
        tx.commit()
            .await
            .map_err(|err| map_db_error(err, Some(&e.spec)))?;
        Ok(e)
    }

    async fn update(
        &self,
        name: &str,
        expected_revision: u64,
        spec: EndpointSpec,
        actor: &Actor,
    ) -> Result<Endpoint, StoreError> {
        self.replace(
            name,
            expected_revision,
            ChangeKind::Update,
            move |_| spec,
            actor,
        )
        .await
    }

    async fn set_status(
        &self,
        name: &str,
        expected_revision: u64,
        status: EndpointStatus,
        actor: &Actor,
    ) -> Result<Endpoint, StoreError> {
        self.replace(
            name,
            expected_revision,
            ChangeKind::Status,
            move |cur| {
                let mut spec = cur.spec.clone();
                spec.status = status;
                spec
            },
            actor,
        )
        .await
    }

    async fn history(
        &self,
        endpoint_id: &str,
        limit: usize,
    ) -> Result<Vec<EndpointRevision>, StoreError> {
        let client = self.client().await?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = client
            .query(
                "SELECT endpoint_id, revision, change, spec, changed_at, changed_by
                   FROM hfs_endpoint_revisions
                  WHERE endpoint_id = $1
                  ORDER BY revision DESC
                  LIMIT $2",
                &[&endpoint_id, &limit],
            )
            .await
            .map_err(unavailable)?;
        Ok(rows
            .iter()
            .map(|r| EndpointRevision {
                endpoint_id: r.get(0),
                revision: u64::try_from(r.get::<_, i64>(1)).unwrap_or(0),
                change: parse_change(r.get::<_, &str>(2)),
                spec: r.get(3),
                changed_at: r.get(4),
                changed_by: r.get(5),
            })
            .collect())
    }
}

/// Keeps a dedicated `LISTEN hfs_config` session open and pokes `registry`
/// on every `endpoints` notification — and once after each (re)connect, in
/// case a change landed while the session was down. Runs until the task is
/// aborted. `LISTEN` needs a non-pooled connection (a pooler in transaction
/// mode breaks it), so this connects with `config` directly; the epoch poll
/// of [`crate::run_watcher`] keeps the registry correct when it cannot.
pub async fn run_config_listener(config: tokio_postgres::Config, registry: Arc<EndpointRegistry>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match listen_once(&config, &registry).await {
            Ok(()) => {
                tracing::warn!("endpoint configuration LISTEN session ended; reconnecting");
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    backoff_secs = backoff.as_secs(),
                    "endpoint configuration LISTEN session failed; relying on the epoch poll"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

async fn listen_once(
    config: &tokio_postgres::Config,
    registry: &EndpointRegistry,
) -> Result<(), tokio_postgres::Error> {
    let (client, mut connection) = config.connect(tokio_postgres::NoTls).await?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let driver = tokio::spawn(async move {
        loop {
            match std::future::poll_fn(|cx| connection.poll_message(cx)).await {
                Some(Ok(msg)) => {
                    if tx.send(msg).is_err() {
                        break;
                    }
                }
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "endpoint configuration LISTEN connection error");
                    break;
                }
                None => break,
            }
        }
    });
    if let Err(e) = client
        .batch_execute(&format!("LISTEN {CONFIG_CHANNEL}"))
        .await
    {
        driver.abort();
        return Err(e);
    }
    registry.poke();
    while let Some(msg) = rx.recv().await {
        if let tokio_postgres::AsyncMessage::Notification(n) = msg
            && n.channel() == CONFIG_CHANNEL
            && n.payload() == ENDPOINTS_SCOPE
        {
            registry.poke();
        }
    }
    drop(client);
    driver.abort();
    Ok(())
}
