//! The dedicated, append-only audit-event store on PostgreSQL (fork
//! migration `hm-0030-audit-event-table`).
//!
//! An `AuditEvent` written here is one row of `audit_event` (partitioned by
//! month on `recorded`) and a few rows of `audit_event_ref`, the lookup table
//! behind the `patient`, `entity` and `agent` search parameters. It is not a
//! row of `resources`: no history row, no `search_index` rows, no full-text
//! row, no tenant gate and no advisory lock. A writer stores a whole batch
//! with one statement.
//!
//! Immutability is enforced in the database: a row trigger refuses every
//! `UPDATE`, and refuses `DELETE` unless the transaction set
//! `hfs.audit_maintenance = 'on'` (retention of the default partition, tenant
//! purge, moving rows out of the default partition). Whole expired months are
//! removed by dropping their partitions.
//!
//! Model and search semantics: [`helios_audit::store`].

use std::sync::Arc;

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use helios_audit::store::{
    AuditEventReader, AuditQuery, AuditReadError, AuditRow, DateBound, DatePrefix, RefField,
    RefFilter, StoredAuditEvent, TokenField, TokenFilter,
};
use serde_json::Value;
use tokio_postgres::types::ToSql;

use super::PostgresBackend;
use crate::core::retention::ExpiredResource;
use crate::error::{BackendError, StorageError, StorageResult};

/// Name of the fork migration creating the tables.
pub const FORK_AUDIT_EVENT_TABLE: &str = "hm-0030-audit-event-table";

/// Name under which a completed legacy backfill is recorded in
/// `hm_schema_migrations`. Never applied at start-up: see
/// [`PgAuditTable::backfill_legacy`].
pub const FORK_AUDIT_BACKFILL: &str = "hm-0031-audit-backfill";

/// Advisory-lock key serializing partition creation ("HFS\0AUDP").
const PARTITION_LOCK_KEY: i64 = 0x4846_5300_4155_4450;
/// Advisory-lock key serializing retention and backfill runs ("HFS\0AUDR").
const MAINTENANCE_LOCK_KEY: i64 = 0x4846_5300_4155_4452;

/// SQL of [`FORK_AUDIT_EVENT_TABLE`]. Additive and idempotent.
pub const FORK_AUDIT_EVENT_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS audit_event (
    tenant_id     TEXT        NOT NULL,
    recorded      TIMESTAMPTZ NOT NULL,
    id            TEXT        NOT NULL,
    action        TEXT,
    outcome       TEXT,
    type_code     TEXT,
    subtype_codes TEXT[]      NOT NULL DEFAULT '{}',
    event         JSONB       NOT NULL,
    PRIMARY KEY (tenant_id, recorded, id)
) PARTITION BY RANGE (recorded);

CREATE INDEX IF NOT EXISTS idx_audit_event_id ON audit_event (tenant_id, id);
CREATE INDEX IF NOT EXISTS idx_audit_event_failed ON audit_event (tenant_id, recorded)
    WHERE outcome IS DISTINCT FROM '0';

CREATE TABLE IF NOT EXISTS audit_event_ref (
    tenant_id TEXT        NOT NULL,
    ref_id    TEXT        NOT NULL,
    recorded  TIMESTAMPTZ NOT NULL,
    id        TEXT        NOT NULL,
    kind      TEXT        NOT NULL,
    ref_type  TEXT        NOT NULL,
    PRIMARY KEY (tenant_id, ref_id, recorded, id, kind, ref_type)
) PARTITION BY RANGE (recorded);

CREATE TABLE IF NOT EXISTS audit_event_default PARTITION OF audit_event DEFAULT;
CREATE TABLE IF NOT EXISTS audit_event_ref_default PARTITION OF audit_event_ref DEFAULT;

CREATE OR REPLACE FUNCTION hfs_audit_event_immutable() RETURNS trigger AS $$
BEGIN
    IF TG_OP = 'DELETE' AND current_setting('hfs.audit_maintenance', true) = 'on' THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'the audit trail is append-only: % on % refused', TG_OP, TG_TABLE_NAME
        USING ERRCODE = 'restrict_violation';
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS audit_event_immutable ON audit_event;
CREATE TRIGGER audit_event_immutable BEFORE UPDATE OR DELETE ON audit_event
    FOR EACH ROW EXECUTE FUNCTION hfs_audit_event_immutable();
DROP TRIGGER IF EXISTS audit_event_ref_immutable ON audit_event_ref;
CREATE TRIGGER audit_event_ref_immutable BEFORE UPDATE OR DELETE ON audit_event_ref
    FOR EACH ROW EXECUTE FUNCTION hfs_audit_event_immutable();

-- Creates the partitions of one month (of both tables) unless they exist.
-- Rows of that month already sitting in the default partition (written while
-- no partition existed) are moved into the new one. Serialized across
-- instances by a transaction-level advisory lock.
CREATE OR REPLACE FUNCTION hfs_audit_ensure_partition(month_start DATE) RETURNS boolean AS $$
DECLARE
    lo TIMESTAMPTZ := (date_trunc('month', month_start::timestamp) AT TIME ZONE 'UTC');
    hi TIMESTAMPTZ := ((date_trunc('month', month_start::timestamp) + INTERVAL '1 month') AT TIME ZONE 'UTC');
    suffix TEXT := to_char(month_start, '"p"YYYY_MM');
    parent TEXT;
    child TEXT;
    created BOOLEAN := false;
BEGIN
    PERFORM pg_advisory_xact_lock(5207941279661769808);
    FOREACH parent IN ARRAY ARRAY['audit_event', 'audit_event_ref'] LOOP
        child := parent || '_' || suffix;
        IF to_regclass(format('%I.%I', current_schema(), child)) IS NOT NULL THEN
            CONTINUE;
        END IF;
        EXECUTE format('CREATE TABLE %I (LIKE %I INCLUDING DEFAULTS INCLUDING CONSTRAINTS)', child, parent);
        PERFORM set_config('hfs.audit_maintenance', 'on', true);
        EXECUTE format(
            'WITH moved AS (DELETE FROM %I WHERE recorded >= %L AND recorded < %L RETURNING *)
             INSERT INTO %I SELECT * FROM moved',
            parent || '_default', lo, hi, child);
        PERFORM set_config('hfs.audit_maintenance', 'off', true);
        EXECUTE format('ALTER TABLE %I ATTACH PARTITION %I FOR VALUES FROM (%L) TO (%L)', parent, child, lo, hi);
        created := true;
    END LOOP;
    RETURN created;
END;
$$ LANGUAGE plpgsql;

SELECT hfs_audit_ensure_partition((date_trunc('month', now() AT TIME ZONE 'UTC'))::date);
SELECT hfs_audit_ensure_partition((date_trunc('month', now() AT TIME ZONE 'UTC') + INTERVAL '1 month')::date);
SELECT hfs_audit_ensure_partition((date_trunc('month', now() AT TIME ZONE 'UTC') + INTERVAL '2 months')::date);

CREATE TABLE IF NOT EXISTS hm_audit_backfill_state (
    id          BOOLEAN PRIMARY KEY DEFAULT true CHECK (id),
    copied      BIGINT NOT NULL DEFAULT 0,
    purged      BIGINT NOT NULL DEFAULT 0,
    started_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at TIMESTAMPTZ
);
"#;

// The literal in `hfs_audit_ensure_partition` must be this key.
const _: () = assert!(PARTITION_LOCK_KEY == 5207941279661769808);

fn pg_error(message: String) -> StorageError {
    StorageError::Backend(BackendError::Internal {
        backend_name: "postgres".to_string(),
        message,
        source: None,
    })
}

/// A PostgreSQL error with the server's message (the driver's `Display` is
/// just "db error").
fn detail(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => format!("{} ({})", db.message(), db.code().code()),
        None => e.to_string(),
    }
}

/// Applies [`FORK_AUDIT_EVENT_TABLE`].
pub(super) async fn fork_audit_event_table(
    client: &deadpool_postgres::Client,
) -> StorageResult<()> {
    client
        .batch_execute(FORK_AUDIT_EVENT_TABLE_SQL)
        .await
        .map_err(|e| pg_error(format!("Failed to apply {FORK_AUDIT_EVENT_TABLE}: {}", detail(&e))))
}

/// Deletes a tenant's audit trail, inside the tenant purge's transaction.
pub(super) async fn purge_tenant_in_txn(
    tx: &tokio_postgres::Transaction<'_>,
    tenant_id: &str,
) -> Result<u64, tokio_postgres::Error> {
    tx.batch_execute("SET LOCAL hfs.audit_maintenance = 'on'")
        .await?;
    tx.execute(
        "DELETE FROM audit_event_ref WHERE tenant_id = $1",
        &[&tenant_id],
    )
    .await?;
    let removed = tx
        .execute("DELETE FROM audit_event WHERE tenant_id = $1", &[&tenant_id])
        .await?;
    tx.batch_execute("SET LOCAL hfs.audit_maintenance = 'off'")
        .await?;
    Ok(removed)
}

/// First instant of the month containing `at`.
pub fn month_start(at: DateTime<Utc>) -> NaiveDate {
    NaiveDate::from_ymd_opt(at.year(), at.month(), 1).expect("first of a month exists")
}

fn next_month(month: NaiveDate) -> NaiveDate {
    if month.month() == 12 {
        NaiveDate::from_ymd_opt(month.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(month.year(), month.month() + 1, 1)
    }
    .expect("first of a month exists")
}

/// The month a partition named `audit_event_pYYYY_MM` holds.
fn partition_month(name: &str) -> Option<NaiveDate> {
    let rest = name.strip_prefix("audit_event_p")?;
    let (year, month) = rest.split_once('_')?;
    if year.len() != 4 || month.len() != 2 {
        return None;
    }
    NaiveDate::from_ymd_opt(year.parse().ok()?, month.parse().ok()?, 1)
}

/// A monthly partition of `audit_event`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditPartition {
    /// Table name (`audit_event_pYYYY_MM`).
    pub name: String,
    /// First day of the month it holds.
    pub month: NaiveDate,
}

impl AuditPartition {
    /// First instant after the partition's range.
    pub fn end(&self) -> DateTime<Utc> {
        Utc.from_utc_datetime(
            &next_month(self.month)
                .and_hms_opt(0, 0, 0)
                .expect("midnight exists"),
        )
    }

    fn ref_name(&self) -> String {
        self.name.replacen("audit_event_", "audit_event_ref_", 1)
    }
}

/// Outcome of a [`PgAuditTable::backfill_legacy`] run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillReport {
    /// Legacy events copied into the table (already-present ones included).
    pub copied: u64,
    /// Legacy `AuditEvent` resources removed from the resource tables.
    pub purged: u64,
    /// Whether no legacy event is left.
    pub finished: bool,
}

type Params = Vec<Box<dyn ToSql + Sync + Send>>;

fn push<T: ToSql + Sync + Send + 'static>(params: &mut Params, value: T) -> String {
    params.push(Box::new(value));
    format!("${}", params.len())
}

fn date_sql(alias: &str, bound: &DateBound, params: &mut Params) -> String {
    let col = format!("{alias}.recorded");
    match bound.prefix {
        DatePrefix::Eq => {
            let lo = push(params, bound.lo);
            let hi = push(params, bound.hi);
            format!("({col} >= {lo} AND {col} < {hi})")
        }
        DatePrefix::Ne => {
            let lo = push(params, bound.lo);
            let hi = push(params, bound.hi);
            format!("({col} < {lo} OR {col} >= {hi})")
        }
        DatePrefix::Gt => format!("{col} >= {}", push(params, bound.hi)),
        DatePrefix::Lt => format!("{col} < {}", push(params, bound.lo)),
        DatePrefix::Ge => format!("{col} >= {}", push(params, bound.lo)),
        DatePrefix::Le => format!("{col} < {}", push(params, bound.hi)),
    }
}

fn dates_sql(alias: &str, query: &AuditQuery, params: &mut Params, out: &mut Vec<String>) {
    for alternatives in &query.dates {
        let parts: Vec<String> = alternatives
            .iter()
            .map(|b| date_sql(alias, b, params))
            .collect();
        out.push(format!("({})", parts.join(" OR ")));
    }
}

fn cursor_sql(alias: &str, query: &AuditQuery, params: &mut Params, out: &mut Vec<String>) {
    if let Some((recorded, id)) = &query.after {
        let r = push(params, *recorded);
        let i = push(params, id.clone());
        let op = if query.ascending { ">" } else { "<" };
        out.push(format!("({alias}.recorded, {alias}.id) {op} ({r}, {i})"));
    }
}

fn token_sql(filter: &TokenFilter, params: &mut Params) -> String {
    const ACTION_SYSTEM: &str = "http://hl7.org/fhir/audit-event-action";
    const OUTCOME_SYSTEM: &str = "http://hl7.org/fhir/audit-event-outcome";
    let mut parts = Vec::new();
    for value in &filter.values {
        let part = match filter.field {
            TokenField::Action | TokenField::Outcome => {
                let (col, implied) = if filter.field == TokenField::Action {
                    ("e.action", ACTION_SYSTEM)
                } else {
                    ("e.outcome", OUTCOME_SYSTEM)
                };
                if value.system.as_deref().is_some_and(|s| s != implied) {
                    "FALSE".to_string()
                } else {
                    match &value.code {
                        Some(code) => format!("{col} = {}", push(params, code.clone())),
                        None => format!("{col} IS NOT NULL"),
                    }
                }
            }
            TokenField::Type => match (&value.system, &value.code) {
                (None, Some(code)) => format!("e.type_code = {}", push(params, code.clone())),
                (Some(system), Some(code)) if system.is_empty() => format!(
                    "(e.type_code = {} AND (e.event->'type'->>'system') IS NULL)",
                    push(params, code.clone())
                ),
                (Some(system), Some(code)) => format!(
                    "(e.type_code = {} AND (e.event->'type'->>'system') = {})",
                    push(params, code.clone()),
                    push(params, system.clone())
                ),
                (Some(system), None) => format!(
                    "(e.event->'type'->>'system') = {}",
                    push(params, system.clone())
                ),
                (None, None) => "FALSE".to_string(),
            },
            TokenField::Subtype => match (&value.system, &value.code) {
                (Some(system), code) if !system.is_empty() => {
                    let mut coding = serde_json::Map::new();
                    coding.insert("system".into(), Value::String(system.clone()));
                    if let Some(code) = code {
                        coding.insert("code".into(), Value::String(code.clone()));
                    }
                    format!(
                        "(e.event->'subtype') @> {}::jsonb",
                        push(params, Value::Array(vec![Value::Object(coding)]))
                    )
                }
                (_, Some(code)) => {
                    format!("e.subtype_codes @> ARRAY[{}]::text[]", push(params, code.clone()))
                }
                (_, None) => "FALSE".to_string(),
            },
        };
        parts.push(part);
    }
    let any = format!("({})", parts.join(" OR "));
    if filter.negate {
        format!("NOT COALESCE({any}, FALSE)")
    } else {
        any
    }
}

fn ref_sql(filter: &RefFilter, params: &mut Params) -> String {
    let field = match filter.field {
        RefField::Patient => "r.ref_type = 'Patient'",
        RefField::Entity => "r.kind = 'e'",
        RefField::Agent => "r.kind = 'a'",
    };
    let alternatives: Vec<String> = filter
        .values
        .iter()
        .map(|v| {
            let id = push(params, v.ref_id.clone());
            match &v.ref_type {
                Some(t) => format!("(r.ref_id = {id} AND r.ref_type = {})", push(params, t.clone())),
                None => format!("r.ref_id = {id}"),
            }
        })
        .collect();
    format!("{field} AND ({})", alternatives.join(" OR "))
}

/// Builds the search (or, with `count`, the count) statement of `query`.
fn search_sql(tenant: &str, query: &AuditQuery, count: bool) -> (String, Params) {
    let mut params: Params = Vec::new();
    let tenant_param = push(&mut params, tenant.to_string());
    let order = if query.ascending { "ASC" } else { "DESC" };
    let limit = i64::try_from(query.limit).unwrap_or(i64::MAX);

    // One reference criterion and nothing else to filter by: drive the read
    // from the lookup table, which is already in (reference, time) order.
    if !count && query.refs.len() == 1 && query.tokens.is_empty() && query.ids.is_none() {
        let mut conds = vec![format!("r.tenant_id = {tenant_param}")];
        conds.push(ref_sql(&query.refs[0], &mut params));
        dates_sql("r", query, &mut params, &mut conds);
        cursor_sql("r", query, &mut params, &mut conds);
        let limit = push(&mut params, limit);
        let sql = format!(
            "SELECT e.id, e.recorded, e.event
             FROM (SELECT DISTINCT r.recorded, r.id FROM audit_event_ref r
                   WHERE {where_}
                   ORDER BY r.recorded {order}, r.id {order} LIMIT {limit}) h
             JOIN audit_event e
               ON e.tenant_id = {tenant_param} AND e.recorded = h.recorded AND e.id = h.id
             ORDER BY e.recorded {order}, e.id {order}",
            where_ = conds.join(" AND "),
        );
        return (sql, params);
    }

    let mut conds = vec![format!("e.tenant_id = {tenant_param}")];
    if let Some(ids) = &query.ids {
        conds.push(format!("e.id = ANY({})", push(&mut params, ids.clone())));
    }
    dates_sql("e", query, &mut params, &mut conds);
    for filter in &query.tokens {
        conds.push(token_sql(filter, &mut params));
    }
    for filter in &query.refs {
        let mut inner = vec![format!("r.tenant_id = {tenant_param}")];
        inner.push(ref_sql(filter, &mut params));
        dates_sql("r", query, &mut params, &mut inner);
        conds.push(format!(
            "(e.recorded, e.id) IN (SELECT r.recorded, r.id FROM audit_event_ref r WHERE {})",
            inner.join(" AND ")
        ));
    }
    if count {
        return (
            format!(
                "SELECT count(*)::bigint FROM audit_event e WHERE {}",
                conds.join(" AND ")
            ),
            params,
        );
    }
    cursor_sql("e", query, &mut params, &mut conds);
    let limit = push(&mut params, limit);
    (
        format!(
            "SELECT e.id, e.recorded, e.event FROM audit_event e WHERE {}
             ORDER BY e.recorded {order}, e.id {order} LIMIT {limit}",
            conds.join(" AND ")
        ),
        params,
    )
}

fn as_refs(params: &Params) -> Vec<&(dyn ToSql + Sync)> {
    params
        .iter()
        .map(|p| p.as_ref() as &(dyn ToSql + Sync))
        .collect()
}

/// Removes NUL characters, which `jsonb` cannot store.
fn strip_nul(value: &mut Value) {
    match value {
        Value::String(s) if s.contains('\0') => *s = s.replace('\0', ""),
        Value::Array(items) => items.iter_mut().for_each(strip_nul),
        Value::Object(map) => map.values_mut().for_each(strip_nul),
        _ => {}
    }
}

fn event_text(event: &mut Value) -> String {
    let text = event.to_string();
    if text.contains("\\u0000") {
        strip_nul(event);
        event.to_string()
    } else {
        text
    }
}

const APPEND_SQL: &str = "
WITH ev AS (
    INSERT INTO audit_event (tenant_id, recorded, id, action, outcome, type_code, subtype_codes, event)
    SELECT t, r, i, a, o, c, string_to_array(s, E'\\x1f'), e::jsonb
    FROM unnest($1::text[], $2::timestamptz[], $3::text[], $4::text[], $5::text[], $6::text[],
                $7::text[], $8::text[]) AS x(t, r, i, a, o, c, s, e)
    ON CONFLICT DO NOTHING
), rf AS (
    INSERT INTO audit_event_ref (tenant_id, ref_id, recorded, id, kind, ref_type)
    SELECT * FROM unnest($9::text[], $10::text[], $11::timestamptz[], $12::text[], $13::text[],
                         $14::text[])
    ON CONFLICT DO NOTHING
)
SELECT 1";

/// The audit tables of one PostgreSQL database.
#[derive(Clone)]
pub struct PgAuditTable {
    backend: Arc<PostgresBackend>,
}

impl PgAuditTable {
    /// Over `backend`'s pool. The schema must have been migrated
    /// ([`FORK_AUDIT_EVENT_TABLE`]).
    pub fn new(backend: Arc<PostgresBackend>) -> Self {
        Self { backend }
    }

    /// The backend the tables live in.
    pub fn backend(&self) -> &Arc<PostgresBackend> {
        &self.backend
    }

    async fn client(&self) -> Result<deadpool_postgres::Client, String> {
        self.backend.get_client().await.map_err(|e| e.to_string())
    }

    /// Appends `events` (tenant, event) with one statement and one commit.
    /// An event already stored (same tenant, `recorded` and id) is left as it
    /// is, so a replayed batch is harmless.
    pub async fn append(&self, events: Vec<(String, Value)>) -> Result<(), String> {
        if events.is_empty() {
            return Ok(());
        }
        let now = Utc::now();
        let n = events.len();
        let mut tenants = Vec::with_capacity(n);
        let mut recorded = Vec::with_capacity(n);
        let mut ids = Vec::with_capacity(n);
        let mut actions = Vec::with_capacity(n);
        let mut outcomes = Vec::with_capacity(n);
        let mut type_codes = Vec::with_capacity(n);
        let mut subtypes = Vec::with_capacity(n);
        let mut bodies = Vec::with_capacity(n);
        let (mut r_tenant, mut r_ref, mut r_recorded, mut r_id, mut r_kind, mut r_type) = (
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        for (tenant, event) in events {
            let mut row = AuditRow::from_event(event, now);
            for r in &row.refs {
                r_tenant.push(tenant.clone());
                r_ref.push(r.ref_id.clone());
                r_recorded.push(row.recorded);
                r_id.push(row.id.clone());
                r_kind.push(r.kind.code());
                r_type.push(r.ref_type.clone());
            }
            bodies.push(event_text(&mut row.event));
            tenants.push(tenant);
            recorded.push(row.recorded);
            ids.push(row.id);
            actions.push(row.action);
            outcomes.push(row.outcome);
            type_codes.push(row.type_code);
            subtypes.push(row.subtype_codes.join("\u{1f}"));
        }
        let client = self.client().await?;
        let statement = client
            .prepare_cached(APPEND_SQL)
            .await
            .map_err(|e| format!("audit append prepare: {}", detail(&e)))?;
        client
            .execute(
                &statement,
                &[
                    &tenants, &recorded, &ids, &actions, &outcomes, &type_codes, &subtypes,
                    &bodies, &r_tenant, &r_ref, &r_recorded, &r_id, &r_kind, &r_type,
                ],
            )
            .await
            .map_err(|e| format!("audit append: {}", detail(&e)))?;
        Ok(())
    }

    /// Creates the partitions of the month of `now` and of the next `ahead`
    /// months, unless they exist. Returns how many months were created.
    pub async fn ensure_partitions(&self, now: DateTime<Utc>, ahead: u32) -> Result<u32, String> {
        let mut month = month_start(now);
        let mut created = 0;
        for _ in 0..=ahead {
            if self.ensure_partition(month).await? {
                created += 1;
            }
            month = next_month(month);
        }
        Ok(created)
    }

    /// Creates the partitions of `month` (its first day) unless they exist.
    pub async fn ensure_partition(&self, month: NaiveDate) -> Result<bool, String> {
        let client = self.client().await?;
        // Cheap check first: the function takes an advisory lock.
        let name = format!("audit_event_p{}", month.format("%Y_%m"));
        let exists: bool = client
            .query_one(
                "SELECT to_regclass(format('%I.%I', current_schema(), $1::text)) IS NOT NULL",
                &[&name],
            )
            .await
            .map_err(|e| format!("audit partition check: {}", detail(&e)))?
            .get(0);
        if exists {
            return Ok(false);
        }
        let created: bool = client
            .query_one("SELECT hfs_audit_ensure_partition($1::date)", &[&month])
            .await
            .map_err(|e| format!("audit partition {name}: {}", detail(&e)))?
            .get(0);
        if created {
            tracing::info!(partition = %name, "Created audit partition");
        }
        Ok(created)
    }

    /// The monthly partitions of `audit_event`, oldest first.
    pub async fn partitions(&self) -> Result<Vec<AuditPartition>, String> {
        let client = self.client().await?;
        let rows = client
            .query(
                "SELECT c.relname::text FROM pg_inherits i
                 JOIN pg_class c ON c.oid = i.inhrelid
                 WHERE i.inhparent = to_regclass(format('%I.%I', current_schema(), 'audit_event'))",
                &[],
            )
            .await
            .map_err(|e| format!("audit partitions: {}", detail(&e)))?;
        let mut out: Vec<AuditPartition> = rows
            .iter()
            .filter_map(|row| {
                let name: String = row.get(0);
                partition_month(&name).map(|month| AuditPartition { name, month })
            })
            .collect();
        out.sort_by_key(|p| p.month);
        Ok(out)
    }

    /// Tries to become the one instance doing audit maintenance (retention,
    /// backfill). The lock lives as long as the returned guard.
    pub async fn maintenance_lock(&self) -> Result<Option<MaintenanceLock>, String> {
        let client = self.client().await?;
        let got: bool = client
            .query_one("SELECT pg_try_advisory_lock($1)", &[&MAINTENANCE_LOCK_KEY])
            .await
            .map_err(|e| format!("audit maintenance lock: {}", detail(&e)))?
            .get(0);
        Ok(got.then_some(MaintenanceLock {
            client: Some(client),
        }))
    }

    /// One page of a partition's events (`table` is a partition name from
    /// [`Self::partitions`] or `audit_event_default`), in `(recorded, id)`
    /// order after `after`, optionally only those recorded before `before`.
    pub async fn partition_page(
        &self,
        table: &str,
        after: Option<&(String, DateTime<Utc>, String)>,
        before: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<ExpiredResource>, String> {
        if table != "audit_event_default" && partition_month(table).is_none() {
            return Err(format!("not an audit partition: {table:?}"));
        }
        let client = self.client().await?;
        let mut params: Params = Vec::new();
        let mut conds = vec!["TRUE".to_string()];
        if let Some((tenant, recorded, id)) = after {
            let t = push(&mut params, tenant.clone());
            let r = push(&mut params, *recorded);
            let i = push(&mut params, id.clone());
            conds.push(format!("(tenant_id, recorded, id) > ({t}, {r}, {i})"));
        }
        if let Some(before) = before {
            conds.push(format!("recorded < {}", push(&mut params, before)));
        }
        let limit = push(&mut params, i64::try_from(limit).unwrap_or(i64::MAX));
        let sql = format!(
            "SELECT tenant_id, id, recorded, event FROM {table} WHERE {}
             ORDER BY tenant_id, recorded, id LIMIT {limit}",
            conds.join(" AND ")
        );
        let rows = client
            .query(&sql, &as_refs(&params))
            .await
            .map_err(|e| format!("audit partition read: {}", detail(&e)))?;
        Ok(rows
            .into_iter()
            .map(|row| ExpiredResource {
                tenant_id: row.get(0),
                id: row.get(1),
                last_updated: row.get(2),
                content: row.get(3),
            })
            .collect())
    }

    /// Drops a whole monthly partition (events and lookup rows). Returns the
    /// number of events it held.
    pub async fn drop_partition(&self, partition: &AuditPartition) -> Result<u64, String> {
        if partition_month(&partition.name) != Some(partition.month) {
            return Err(format!("not an audit partition: {:?}", partition.name));
        }
        let mut client = self.client().await?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| format!("audit partition drop: {}", detail(&e)))?;
        // Never queue behind a long reader while holding up the writers.
        tx.batch_execute("SET LOCAL lock_timeout = '5s'")
            .await
            .map_err(|e| format!("audit partition drop: {}", detail(&e)))?;
        let rows: i64 = tx
            .query_one(
                &format!("SELECT count(*)::bigint FROM {}", partition.name),
                &[],
            )
            .await
            .map_err(|e| format!("audit partition count: {}", detail(&e)))?
            .get(0);
        tx.batch_execute(&format!(
            "ALTER TABLE audit_event_ref DETACH PARTITION {r}; DROP TABLE {r};
             ALTER TABLE audit_event DETACH PARTITION {e}; DROP TABLE {e};",
            r = partition.ref_name(),
            e = partition.name,
        ))
        .await
        .map_err(|e| format!("audit partition drop {}: {}", partition.name, detail(&e)))?;
        tx.commit()
            .await
            .map_err(|e| format!("audit partition drop commit: {}", detail(&e)))?;
        Ok(u64::try_from(rows).unwrap_or(0))
    }

    /// Deletes the given `(tenant, recorded, id)` events from the default
    /// partition (events whose month had no partition and that have expired).
    pub async fn delete_default_rows(
        &self,
        keys: &[(String, DateTime<Utc>, String)],
    ) -> Result<u64, String> {
        if keys.is_empty() {
            return Ok(0);
        }
        let tenants: Vec<&str> = keys.iter().map(|k| k.0.as_str()).collect();
        let recorded: Vec<DateTime<Utc>> = keys.iter().map(|k| k.1).collect();
        let ids: Vec<&str> = keys.iter().map(|k| k.2.as_str()).collect();
        let mut client = self.client().await?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| format!("audit delete: {}", detail(&e)))?;
        tx.batch_execute("SET LOCAL hfs.audit_maintenance = 'on'")
            .await
            .map_err(|e| format!("audit delete: {}", detail(&e)))?;
        tx.execute(
            "DELETE FROM audit_event_ref_default r
             USING unnest($1::text[], $2::timestamptz[], $3::text[]) AS k(t, r, i)
             WHERE r.tenant_id = k.t AND r.recorded = k.r AND r.id = k.i",
            &[&tenants, &recorded, &ids],
        )
        .await
        .map_err(|e| format!("audit delete: {}", detail(&e)))?;
        let removed = tx
            .execute(
                "DELETE FROM audit_event_default e
                 USING unnest($1::text[], $2::timestamptz[], $3::text[]) AS k(t, r, i)
                 WHERE e.tenant_id = k.t AND e.recorded = k.r AND e.id = k.i",
                &[&tenants, &recorded, &ids],
            )
            .await
            .map_err(|e| format!("audit delete: {}", detail(&e)))?;
        tx.commit()
            .await
            .map_err(|e| format!("audit delete commit: {}", detail(&e)))?;
        Ok(removed)
    }

    /// Moves `AuditEvent` resources written before the table existed out of
    /// the general resource tables: copies up to `batch` of them (oldest
    /// first) into the audit tables, then erases them from `resources`,
    /// `resource_history`, `search_index` and `resource_fts`; repeats until
    /// none is left or `max_batches` is reached.
    ///
    /// Safe to interrupt and to repeat: a copy is idempotent, and an event is
    /// erased only after the batch that copied it committed. Records
    /// [`FORK_AUDIT_BACKFILL`] in `hm_schema_migrations` once nothing is left.
    /// Never run automatically: the operator starts it (`HFS_AUDIT_BACKFILL`).
    pub async fn backfill_legacy(
        &self,
        batch: usize,
        max_batches: Option<u64>,
    ) -> Result<BackfillReport, String> {
        let Some(lock) = self.maintenance_lock().await? else {
            return Err("another instance is running audit maintenance".to_string());
        };
        let result = self.backfill_locked(batch, max_batches).await;
        lock.release().await;
        result
    }

    async fn backfill_locked(
        &self,
        batch: usize,
        max_batches: Option<u64>,
    ) -> Result<BackfillReport, String> {
        use crate::core::RetentionStorage;
        let mut report = BackfillReport::default();
        let far_future = Utc::now() + chrono::Duration::days(365_000);
        let mut batches = 0u64;
        loop {
            if max_batches.is_some_and(|m| batches >= m) {
                break;
            }
            batches += 1;
            let page = self
                .backend
                .expired_resources("AuditEvent", far_future, batch.max(1))
                .await
                .map_err(|e| e.to_string())?;
            if page.is_empty() {
                report.finished = true;
                break;
            }
            let now = Utc::now();
            let mut months = std::collections::BTreeSet::new();
            let mut events = Vec::with_capacity(page.len());
            let mut keys = Vec::with_capacity(page.len());
            for r in page {
                let mut content = r.content;
                if let Some(obj) = content.as_object_mut() {
                    obj.insert("id".into(), Value::String(r.id.clone()));
                    // `meta.versionId` / `lastUpdated` are served, not stored.
                    if let Some(Value::Object(meta)) = obj.get_mut("meta") {
                        meta.remove("versionId");
                        meta.remove("lastUpdated");
                    }
                    if obj.get("meta").and_then(Value::as_object).is_some_and(|m| m.is_empty()) {
                        obj.remove("meta");
                    }
                    if !obj.get("recorded").is_some_and(Value::is_string) {
                        obj.insert(
                            "recorded".into(),
                            Value::String(r.last_updated.to_rfc3339()),
                        );
                    }
                }
                months.insert(month_start(AuditRow::from_event(content.clone(), now).recorded));
                keys.push((r.tenant_id.clone(), r.id));
                events.push((r.tenant_id, content));
            }
            for month in months {
                self.ensure_partition(month).await?;
            }
            let copied = events.len() as u64;
            self.append(events).await?;
            let purged = self
                .backend
                .purge_expired("AuditEvent", &keys)
                .await
                .map_err(|e| e.to_string())?;
            report.copied += copied;
            report.purged += purged;
            let client = self.client().await?;
            client
                .execute(
                    "INSERT INTO hm_audit_backfill_state (id, copied, purged) VALUES (true, $1, $2)
                     ON CONFLICT (id) DO UPDATE SET
                         copied = hm_audit_backfill_state.copied + EXCLUDED.copied,
                         purged = hm_audit_backfill_state.purged + EXCLUDED.purged,
                         updated_at = now()",
                    &[&(copied as i64), &(purged as i64)],
                )
                .await
                .map_err(|e| format!("audit backfill state: {}", detail(&e)))?;
            if purged == 0 {
                return Err("audit backfill made no progress (legacy rows could not be removed)"
                    .to_string());
            }
        }
        if report.finished {
            let client = self.client().await?;
            client
                .batch_execute(&format!(
                    "INSERT INTO hm_audit_backfill_state (id) VALUES (true) ON CONFLICT (id) DO NOTHING;
                     UPDATE hm_audit_backfill_state SET finished_at = COALESCE(finished_at, now());
                     INSERT INTO hm_schema_migrations (name, note)
                     VALUES ('{FORK_AUDIT_BACKFILL}', 'legacy AuditEvent resources moved to audit_event')
                     ON CONFLICT (name) DO NOTHING;"
                ))
                .await
                .map_err(|e| format!("audit backfill record: {}", detail(&e)))?;
        }
        Ok(report)
    }

    /// Whether `tenant` still has `AuditEvent` rows in the resource tables.
    pub async fn has_legacy_events(&self, tenant: &str) -> Result<bool, String> {
        let client = self.client().await?;
        client
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM resources
                                WHERE tenant_id = $1 AND resource_type = 'AuditEvent')",
                &[&tenant],
            )
            .await
            .map(|row| row.get(0))
            .map_err(|e| format!("audit legacy check: {}", detail(&e)))
    }
}

/// Holds the audit maintenance advisory lock. Call [`Self::release`] when
/// done; a guard that is merely dropped releases the lock in the background.
pub struct MaintenanceLock {
    client: Option<deadpool_postgres::Client>,
}

impl MaintenanceLock {
    /// Releases the lock and waits until the server has let go of it.
    pub async fn release(mut self) {
        if let Some(client) = self.client.take()
            && client
                .execute("SELECT pg_advisory_unlock($1)", &[&MAINTENANCE_LOCK_KEY])
                .await
                .is_err()
        {
            // The lock is session-level: close the session.
            let _ = deadpool_postgres::Object::take(client);
        }
    }
}

impl Drop for MaintenanceLock {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            // The lock is session-level: unlock before the connection goes
            // back to the pool. If this cannot run, drop the connection.
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn(async move {
                        if client
                            .execute("SELECT pg_advisory_unlock($1)", &[&MAINTENANCE_LOCK_KEY])
                            .await
                            .is_err()
                        {
                            let _ = deadpool_postgres::Object::take(client);
                        }
                    });
                }
                Err(_) => {
                    let _ = deadpool_postgres::Object::take(client);
                }
            }
        }
    }
}

fn read_error(e: impl std::fmt::Display) -> AuditReadError {
    AuditReadError::Storage(e.to_string())
}

fn stored(row: &tokio_postgres::Row) -> StoredAuditEvent {
    StoredAuditEvent::new(row.get(0), row.get(1), row.get(2))
}

#[async_trait::async_trait]
impl AuditEventReader for PgAuditTable {
    async fn search(
        &self,
        tenant: &str,
        query: &AuditQuery,
    ) -> Result<Vec<StoredAuditEvent>, AuditReadError> {
        if query.limit == 0 || query.ids.as_ref().is_some_and(Vec::is_empty) {
            return Ok(Vec::new());
        }
        let (sql, params) = search_sql(tenant, query, false);
        let client = self.client().await.map_err(read_error)?;
        let rows = client
            .query(&sql, &as_refs(&params))
            .await
            .map_err(|e| read_error(format!("audit search: {}", detail(&e))))?;
        Ok(rows.iter().map(stored).collect())
    }

    async fn count(&self, tenant: &str, query: &AuditQuery) -> Result<u64, AuditReadError> {
        if query.ids.as_ref().is_some_and(Vec::is_empty) {
            return Ok(0);
        }
        let (sql, params) = search_sql(tenant, query, true);
        let client = self.client().await.map_err(read_error)?;
        let n: i64 = client
            .query_one(&sql, &as_refs(&params))
            .await
            .map_err(|e| read_error(format!("audit count: {}", detail(&e))))?
            .get(0);
        Ok(u64::try_from(n).unwrap_or(0))
    }

    async fn read(
        &self,
        tenant: &str,
        id: &str,
    ) -> Result<Option<StoredAuditEvent>, AuditReadError> {
        let client = self.client().await.map_err(read_error)?;
        // A UUIDv7 id says when the event was written: look at that month
        // only. Anything else (or a miss) probes every partition's id index.
        if let Some(at) = helios_audit::store::uuid_v7_time(id) {
            let lo = at - chrono::Duration::days(1);
            let hi = at + chrono::Duration::days(1);
            let row = client
                .query_opt(
                    "SELECT id, recorded, event FROM audit_event
                     WHERE tenant_id = $1 AND id = $2 AND recorded >= $3 AND recorded < $4
                     LIMIT 1",
                    &[&tenant, &id, &lo, &hi],
                )
                .await
                .map_err(|e| read_error(format!("audit read: {}", detail(&e))))?;
            if let Some(row) = row {
                return Ok(Some(stored(&row)));
            }
        }
        let row = client
            .query_opt(
                "SELECT id, recorded, event FROM audit_event
                 WHERE tenant_id = $1 AND id = $2 ORDER BY recorded DESC LIMIT 1",
                &[&tenant, &id],
            )
            .await
            .map_err(|e| read_error(format!("audit read: {}", detail(&e))))?;
        Ok(row.as_ref().map(stored))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_audit::store::parse_search;

    fn sql(pairs: &[(&str, &str)], count: bool) -> (String, usize) {
        let params: Vec<(String, String)> = pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        let mut query = parse_search(&params).unwrap().query;
        query.limit = 20;
        let (sql, params) = search_sql("t1", &query, count);
        (sql.split_whitespace().collect::<Vec<_>>().join(" "), params.len())
    }

    #[test]
    fn partition_names_round_trip() {
        let month = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        assert_eq!(partition_month("audit_event_p2026_11"), Some(month));
        assert_eq!(partition_month("audit_event_default"), None);
        assert_eq!(partition_month("audit_event_ref_p2026_11"), None);
        assert_eq!(partition_month("audit_event_p2026_13"), None);
        assert_eq!(partition_month("audit_event_p2026_11; DROP"), None);
        let p = AuditPartition {
            name: "audit_event_p2026_12".into(),
            month: NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(),
        };
        assert_eq!(p.ref_name(), "audit_event_ref_p2026_12");
        assert_eq!(p.end().to_rfc3339(), "2027-01-01T00:00:00+00:00");
        assert_eq!(
            month_start(DateTime::parse_from_rfc3339("2026-11-25T10:00:00Z").unwrap().into()),
            month
        );
    }

    #[test]
    fn an_unfiltered_search_walks_the_primary_key() {
        let (s, n) = sql(&[], false);
        assert_eq!(
            s,
            "SELECT e.id, e.recorded, e.event FROM audit_event e WHERE e.tenant_id = $1 \
             ORDER BY e.recorded DESC, e.id DESC LIMIT $2"
        );
        assert_eq!(n, 2);
    }

    #[test]
    fn a_single_reference_drives_from_the_lookup_table() {
        let (s, n) = sql(&[("patient", "p1"), ("date", "ge2026-11-01")], false);
        assert!(s.contains("FROM (SELECT DISTINCT r.recorded, r.id FROM audit_event_ref r"), "{s}");
        assert!(s.contains("r.ref_type = 'Patient' AND ((r.ref_id = $2 AND r.ref_type = $3))"), "{s}");
        assert!(s.contains("(r.recorded >= $4)"), "{s}");
        assert!(s.contains("JOIN audit_event e ON e.tenant_id = $1"), "{s}");
        assert_eq!(n, 5);
    }

    #[test]
    fn several_criteria_filter_the_event_table() {
        let (s, _) = sql(
            &[("entity", "Observation/o1"), ("action", "C"), ("outcome:not", "0"), ("subtype", "create")],
            false,
        );
        assert!(s.contains("e.action = $"), "{s}");
        assert!(s.contains("NOT COALESCE((e.outcome = $"), "{s}");
        assert!(s.contains("e.subtype_codes @> ARRAY[$"), "{s}");
        assert!(s.contains("(e.recorded, e.id) IN (SELECT r.recorded, r.id FROM audit_event_ref r"), "{s}");
        assert!(s.contains("r.kind = 'e'"), "{s}");
    }

    #[test]
    fn a_count_has_no_order_limit_or_cursor() {
        let (s, n) = sql(&[("patient", "p1")], true);
        assert!(s.starts_with("SELECT count(*)::bigint FROM audit_event e WHERE"), "{s}");
        assert!(!s.contains("ORDER BY") && !s.contains("LIMIT"), "{s}");
        assert_eq!(n, 3);
    }

    #[test]
    fn nul_characters_never_reach_jsonb() {
        let mut v = serde_json::json!({"a": "x\u{0}y", "b": [{"c": "\u{0}"}]});
        let text = event_text(&mut v);
        assert!(!text.contains("\\u0000"), "{text}");
        assert_eq!(v["a"], "xy");
    }
}
