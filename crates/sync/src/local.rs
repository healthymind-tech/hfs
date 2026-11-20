//! A local endpoint as a sync peer: in-process storage access in the
//! endpoint's tenant (design §9.2 "Local source" / "Local target"), never
//! HTTP. Target writes are reported to the server's write observers
//! (subscriptions, dashboard counters) like any other write.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use helios_fhir::FhirVersion;
use helios_persistence::core::{
    DifferentialHistoryProvider, HistoryMethod, HistoryPage, HistoryParams, ResourceStorage,
    ResourceWrite, SystemHistoryProvider, WriteEvent, WriteKind, WriteNotice, WriteObserver,
};
use helios_persistence::error::{ResourceError, StorageError, StorageResult};
use helios_persistence::tenant::{TenantContext, TenantId, TenantPermissions};
use helios_persistence::types::{Page, Pagination, StoredResource};
use serde_json::Value;

use crate::change::Change;
use crate::conflict::TargetInfo;
use crate::cursor::ALL_TYPES;
use crate::hash::content_hash;
use crate::peer::{
    ChangeWindow, InitialPage, PeerError, SourceReader, TargetWriter, WriteOp, WriteResult,
};

/// The storage operations sync needs, object-safe over any backend that
/// has versioned history and differential reads (SQLite, PostgreSQL,
/// composites over them).
#[async_trait]
pub trait LocalStore: Send + Sync {
    /// Read the current version.
    async fn read(
        &self,
        tenant: &TenantContext,
        rt: &str,
        id: &str,
    ) -> StorageResult<Option<StoredResource>>;
    /// PUT semantics.
    async fn put(
        &self,
        tenant: &TenantContext,
        rt: &str,
        id: &str,
        resource: Value,
        version: FhirVersion,
    ) -> StorageResult<(StoredResource, bool)>;
    /// Soft delete.
    async fn delete(&self, tenant: &TenantContext, rt: &str, id: &str) -> StorageResult<()>;
    /// Current versions changed after `since` (exclusive), oldest first.
    async fn modified_since(
        &self,
        tenant: &TenantContext,
        rt: Option<&str>,
        since: DateTime<Utc>,
        pagination: &Pagination,
    ) -> StorageResult<Page<StoredResource>>;
    /// System history, newest first.
    async fn history(
        &self,
        tenant: &TenantContext,
        params: &HistoryParams,
    ) -> StorageResult<HistoryPage>;
}

#[async_trait]
impl<S> LocalStore for S
where
    S: ResourceStorage + SystemHistoryProvider + DifferentialHistoryProvider + Send + Sync,
{
    async fn read(
        &self,
        tenant: &TenantContext,
        rt: &str,
        id: &str,
    ) -> StorageResult<Option<StoredResource>> {
        ResourceStorage::read(self, tenant, rt, id).await
    }

    async fn put(
        &self,
        tenant: &TenantContext,
        rt: &str,
        id: &str,
        resource: Value,
        version: FhirVersion,
    ) -> StorageResult<(StoredResource, bool)> {
        ResourceStorage::create_or_update(self, tenant, rt, id, resource, version).await
    }

    async fn delete(&self, tenant: &TenantContext, rt: &str, id: &str) -> StorageResult<()> {
        ResourceStorage::delete(self, tenant, rt, id).await
    }

    async fn modified_since(
        &self,
        tenant: &TenantContext,
        rt: Option<&str>,
        since: DateTime<Utc>,
        pagination: &Pagination,
    ) -> StorageResult<Page<StoredResource>> {
        DifferentialHistoryProvider::modified_since(self, tenant, rt, since, pagination).await
    }

    async fn history(
        &self,
        tenant: &TenantContext,
        params: &HistoryParams,
    ) -> StorageResult<HistoryPage> {
        SystemHistoryProvider::history_system(self, tenant, params).await
    }
}

/// What the target endpoint's write validation says about one resource.
#[derive(Debug, Clone, PartialEq)]
pub enum GateVerdict {
    /// Write it. `findings` are the validator's issues in `log` mode
    /// (recorded in the run log), empty otherwise.
    Accept {
        /// Issue summaries (`path: message`).
        findings: Vec<String>,
    },
    /// Do not write (`enforce` mode with errors): the reason and the
    /// OperationOutcome a normal write would have answered with.
    Reject {
        /// Short summary.
        reason: String,
        /// The OperationOutcome.
        outcome: Value,
    },
}

/// The server's normal write path for a local target endpoint: write-time
/// validation under the endpoint's settings (`off` / `log` / `enforce`,
/// required profiles, terminology) and the follow-ups of a committed write
/// (a StructureDefinition joins the endpoint's validation profile registry).
/// Implemented by `helios-rest` over its `ValidationService`; installed with
/// [`crate::SyncService::set_local_write_hooks`].
#[async_trait]
pub trait LocalWriteHooks: Send + Sync {
    /// Validates `resource` (may clean it, as a normal write does) for
    /// endpoint `endpoint`.
    async fn check(&self, endpoint: &str, resource_type: &str, resource: &mut Value)
    -> GateVerdict;
    /// A resource was committed at `endpoint`.
    fn written(&self, endpoint: &str, resource_type: &str, resource: &Value);
}

/// A tenant context with full access.
pub fn tenant_context(tenant: &str) -> TenantContext {
    TenantContext::new(TenantId::new(tenant), TenantPermissions::full_access())
}

fn storage_err(e: StorageError) -> PeerError {
    PeerError(format!("local storage: {e}"))
}

fn is_gone(e: &StorageError) -> bool {
    matches!(
        e,
        StorageError::Resource(ResourceError::Gone { .. } | ResourceError::NotFound { .. })
    )
}

/// The FHIR version of an endpoint release, when this binary has it.
pub fn fhir_version_of(release: helios_endpoints::FhirRelease) -> Option<FhirVersion> {
    FhirVersion::from_storage(&format!("{release:?}"))
}

fn change_of(stored: StoredResource) -> Change {
    let resource_type = stored.resource_type().to_string();
    let id = stored.id().to_string();
    let version = Some(stored.version_id().to_string());
    let last_updated = stored.last_modified();
    let deleted = stored.is_deleted();
    Change {
        resource_type,
        id,
        version,
        last_updated,
        deleted,
        resource: (!deleted).then(|| stored.into_content_with_meta()),
    }
}

/// A local endpoint read as a source.
pub struct LocalSource {
    store: Arc<dyn LocalStore>,
    tenant: TenantContext,
}

impl LocalSource {
    /// Source over `tenant`.
    pub fn new(store: Arc<dyn LocalStore>, tenant: &str) -> Self {
        Self {
            store,
            tenant: tenant_context(tenant),
        }
    }
}

fn epoch() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(0, 0).expect("epoch")
}

#[async_trait]
impl SourceReader for LocalSource {
    async fn initial_types(&self) -> Result<Vec<String>, PeerError> {
        Ok(vec![ALL_TYPES.to_string()])
    }

    async fn initial_page(
        &self,
        resource_type: &str,
        page: Option<&str>,
        since: Option<DateTime<Utc>>,
        count: u32,
    ) -> Result<InitialPage, PeerError> {
        let rt = (resource_type != ALL_TYPES).then_some(resource_type);
        let pagination = match page {
            Some(c) => Pagination::with_cursor(count, c.to_string()),
            None => Pagination::new(count),
        };
        let since = since
            .map(|s| s - Duration::microseconds(1))
            .unwrap_or_else(epoch);
        let p = self
            .store
            .modified_since(&self.tenant, rt, since, &pagination)
            .await
            .map_err(storage_err)?;
        let next = if p.page_info.has_next {
            p.page_info.next_cursor.clone()
        } else {
            None
        };
        Ok(InitialPage {
            changes: p.items.into_iter().map(change_of).collect(),
            next,
        })
    }

    async fn changes_since(
        &self,
        since: DateTime<Utc>,
        page: Option<&str>,
        count: u32,
    ) -> Result<ChangeWindow, PeerError> {
        // Inclusive of `since` (the store's bound is exclusive).
        let from = since - Duration::microseconds(1);
        let pagination = match page {
            Some(c) => Pagination::with_cursor(count, c.to_string()),
            None => Pagination::new(count),
        };
        let p = self
            .store
            .modified_since(&self.tenant, None, from, &pagination)
            .await
            .map_err(storage_err)?;
        let next = if p.page_info.has_next {
            p.page_info.next_cursor.clone()
        } else {
            None
        };
        let changes = p.items.into_iter().map(change_of).collect();
        // Deletes are not in the differential read: take them from history
        // once per window (first page), newest first, bounded.
        let mut neutral = Vec::new();
        if page.is_none() {
            let mut hp = HistoryParams::new()
                .since(from)
                .count(500)
                .include_deleted(true);
            for _ in 0..200 {
                let h = self
                    .store
                    .history(&self.tenant, &hp)
                    .await
                    .map_err(storage_err)?;
                for e in &h.items {
                    if e.method == HistoryMethod::Delete || e.resource.is_deleted() {
                        neutral.push(Change::delete(
                            e.resource.resource_type(),
                            e.resource.id(),
                            Some(e.resource.version_id().to_string()),
                            e.timestamp,
                        ));
                    }
                }
                match (h.page_info.has_next, h.page_info.next_cursor) {
                    (true, Some(c)) => {
                        hp.pagination = Pagination::with_cursor(500, c);
                    }
                    _ => break,
                }
            }
        }
        Ok(ChangeWindow {
            changes,
            neutral,
            next,
        })
    }

    async fn read(&self, resource_type: &str, id: &str) -> Result<Option<Change>, PeerError> {
        match self.store.read(&self.tenant, resource_type, id).await {
            Ok(Some(s)) if !s.is_deleted() => Ok(Some(change_of(s))),
            Ok(_) => Ok(None),
            Err(e) if is_gone(&e) => Ok(None),
            Err(e) => Err(storage_err(e)),
        }
    }
}

/// A local endpoint written as a target.
pub struct LocalTarget {
    store: Arc<dyn LocalStore>,
    tenant: TenantContext,
    version: FhirVersion,
    observer: Option<Arc<dyn WriteObserver>>,
    endpoint: String,
    hooks: Option<Arc<dyn LocalWriteHooks>>,
    findings: std::sync::Mutex<Vec<String>>,
}

impl LocalTarget {
    /// Target over `tenant`, writing resources as `version`.
    pub fn new(
        store: Arc<dyn LocalStore>,
        tenant: &str,
        version: FhirVersion,
        observer: Option<Arc<dyn WriteObserver>>,
    ) -> Self {
        Self {
            store,
            tenant: tenant_context(tenant),
            version,
            observer,
            endpoint: String::new(),
            hooks: None,
            findings: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Routes every write through the endpoint's normal write path.
    pub fn with_hooks(mut self, endpoint: &str, hooks: Option<Arc<dyn LocalWriteHooks>>) -> Self {
        self.endpoint = endpoint.to_string();
        self.hooks = hooks;
        self
    }

    fn notify(
        &self,
        rt: &str,
        kind: WriteKind,
        id: &str,
        version: &str,
        resource: Option<Value>,
        delta: i64,
    ) {
        let Some(o) = &self.observer else { return };
        o.on_write(&WriteEvent::Resource(ResourceWrite {
            tenant: self.tenant.tenant_id().clone(),
            fhir_version: self.version,
            resource_type: rt.to_string(),
            live_delta: delta,
            notice: Some(WriteNotice {
                kind,
                resource_id: id.to_string(),
                version_id: version.to_string(),
                resource,
                previous: None,
            }),
            at: Utc::now(),
        }));
    }
}

#[async_trait]
impl TargetWriter for LocalTarget {
    fn take_findings(&self) -> Vec<String> {
        std::mem::take(&mut *self.findings.lock().unwrap())
    }

    async fn current(
        &self,
        resource_type: &str,
        id: &str,
    ) -> Result<Option<TargetInfo>, PeerError> {
        match self.store.read(&self.tenant, resource_type, id).await {
            Ok(Some(s)) if !s.is_deleted() => {
                let content = s.content_with_meta();
                Ok(Some(TargetInfo {
                    version: Some(s.version_id().to_string()),
                    last_updated: Some(s.last_modified()),
                    hash: content_hash(&content),
                }))
            }
            Ok(_) => Ok(None),
            Err(e) if is_gone(&e) => Ok(None),
            Err(e) => Err(storage_err(e)),
        }
    }

    async fn apply(&self, ops: Vec<WriteOp>) -> Result<Vec<WriteResult>, PeerError> {
        let mut out = Vec::with_capacity(ops.len());
        for op in ops {
            match op {
                WriteOp::Put {
                    resource_type,
                    id,
                    resource,
                    if_match,
                } => {
                    if let Some(expected) = &if_match {
                        match self.current(&resource_type, &id).await? {
                            Some(t) if t.version.as_deref() != Some(expected.as_str()) => {
                                out.push(WriteResult::Conflict(format!(
                                    "target version is {} not {expected}",
                                    t.version.as_deref().unwrap_or("?")
                                )));
                                continue;
                            }
                            _ => {}
                        }
                    }
                    // The target stamps its own version; an emptied `meta`
                    // would be an empty element (`ele-1`) to the validator.
                    let mut resource = crate::hash::strip_volatile(&resource);
                    if let Some(o) = resource.as_object_mut() {
                        o.insert("id".into(), Value::String(id.clone()));
                    }
                    if let Some(h) = &self.hooks {
                        match h.check(&self.endpoint, &resource_type, &mut resource).await {
                            GateVerdict::Reject { reason, outcome } => {
                                let mut text = format!("{reason} | OperationOutcome: {outcome}");
                                if text.len() > 4000 {
                                    let mut cut = 4000;
                                    while !text.is_char_boundary(cut) {
                                        cut -= 1;
                                    }
                                    text.truncate(cut);
                                }
                                out.push(WriteResult::Invalid(text));
                                continue;
                            }
                            GateVerdict::Accept { findings } if !findings.is_empty() => {
                                let mut f = self.findings.lock().unwrap();
                                for x in findings {
                                    f.push(format!("{resource_type}/{id}: {x}"));
                                }
                            }
                            GateVerdict::Accept { .. } => {}
                        }
                    }
                    match self
                        .store
                        .put(&self.tenant, &resource_type, &id, resource, self.version)
                        .await
                    {
                        Ok((stored, created)) => {
                            let v = stored.version_id().to_string();
                            if let Some(h) = &self.hooks {
                                h.written(
                                    &self.endpoint,
                                    &resource_type,
                                    &stored.content_with_meta(),
                                );
                            }
                            let kind = if created {
                                WriteKind::Create
                            } else {
                                WriteKind::Update
                            };
                            self.notify(
                                &resource_type,
                                kind,
                                &id,
                                &v,
                                Some(stored.content_with_meta()),
                                i64::from(created),
                            );
                            out.push(if created {
                                WriteResult::Created(Some(v))
                            } else {
                                WriteResult::Updated(Some(v))
                            });
                        }
                        Err(e) => out.push(WriteResult::Failed(e.to_string())),
                    }
                }
                WriteOp::Delete {
                    resource_type,
                    id,
                    if_match,
                } => {
                    let current = self.current(&resource_type, &id).await?;
                    match (&current, &if_match) {
                        (None, _) => {
                            out.push(WriteResult::Deleted);
                            continue;
                        }
                        (Some(t), Some(expected))
                            if t.version.as_deref() != Some(expected.as_str()) =>
                        {
                            out.push(WriteResult::Conflict(format!(
                                "target version is {} not {expected}",
                                t.version.as_deref().unwrap_or("?")
                            )));
                            continue;
                        }
                        _ => {}
                    }
                    match self.store.delete(&self.tenant, &resource_type, &id).await {
                        Ok(()) => {
                            self.notify(&resource_type, WriteKind::Delete, &id, "", None, -1);
                            out.push(WriteResult::Deleted);
                        }
                        Err(e) if is_gone(&e) => out.push(WriteResult::Deleted),
                        Err(e) => out.push(WriteResult::Failed(e.to_string())),
                    }
                }
            }
        }
        Ok(out)
    }
}
