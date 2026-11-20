//! The endpoint administration service: every rule of the admin API, callable
//! without HTTP.
//!
//! The REST handlers ([`super::endpoints`]) and the manager console (P5's
//! `ManagerBackend`, through an adapter over [`AdminEndpointsState`]) call the
//! same methods, so both get the same validation, SSRF checks, secret
//! sealing, endpoint cap, hot reload and audit trail. The handlers add only
//! the HTTP concerns: JSON parsing (unknown members, write-only secret fields,
//! merge patch), `If-Match` → 412/428, status codes and headers.
//!
//! Typed callers pass an [`EndpointSpec`] plus [`SecretInputs`]. Sealed values
//! inside a typed spec are never trusted: on create they are dropped, on
//! update the stored values are carried over; secrets change only through
//! the inputs. Errors are [`ApiError`]s; [`ApiError::kind`] classifies them
//! for non-HTTP callers.

use helios_audit::AuditAction;
use helios_endpoints::model::{EndpointAuth, EndpointSpec, EndpointStatus, FhirRelease};
use helios_endpoints::name::check_segment;
use helios_endpoints::store::EndpointRevision;
use helios_endpoints::sync::DuplicateRequest;
use helios_endpoints::{Actor, Endpoint, ManagedBy, SecretInput, StoreError};
use serde::Serialize;

use super::audit::AdminEvent;
use super::error::{ApiError, ApiResult, Issue};
use super::hooks::{AuditPage, AuthTestReport, DuplicateStarted, TenantStats};
use super::spec_input::SecretInputs;
use super::{AdminEndpointsState, RewrapReport, outbound, secrets};

/// Whether this binary serves `release`.
pub fn release_supported(release: FhirRelease) -> bool {
    match release {
        FhirRelease::R4 => cfg!(feature = "R4"),
        FhirRelease::R4B => cfg!(feature = "R4B"),
        FhirRelease::R5 => cfg!(feature = "R5"),
        FhirRelease::R6 => cfg!(feature = "R6"),
    }
}

fn check_release(spec: &EndpointSpec) -> ApiResult<()> {
    if release_supported(spec.fhir_version) {
        Ok(())
    } else {
        Err(ApiError::invalid_field(
            "fhirVersion",
            format!(
                "this server was built without FHIR {:?} support",
                spec.fhir_version
            ),
        ))
    }
}

fn secret_verb(i: &SecretInput) -> &'static str {
    match i {
        SecretInput::Set(_) => "set",
        SecretInput::Keep => "keep",
        SecretInput::Clear => "clear",
    }
}

/// A successful create / update.
#[derive(Debug, Clone)]
pub struct Written {
    /// The stored endpoint.
    pub endpoint: Endpoint,
    /// Non-blocking findings (hosts that did not resolve…).
    pub warnings: Vec<String>,
}

/// A successful delete.
#[derive(Debug, Clone)]
pub struct Deleted {
    /// The tombstone.
    pub endpoint: Endpoint,
    /// Current resources purged, when a purge was asked for.
    pub resources_removed: Option<u64>,
}

/// Traffic of an endpoint on this instance.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Traffic {
    /// Window in seconds.
    pub window_seconds: i64,
    /// Requests sampled.
    pub samples: usize,
    /// Requests per minute.
    pub requests_per_minute: f64,
    /// 95th percentile latency.
    pub p95_ms: f64,
    /// Share of `5xx`/`4xx` answers.
    pub error_rate: f64,
}

/// `GET /admin/endpoints/{name}/stats`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointStatsView {
    /// Endpoint name.
    pub name: String,
    /// Its tenant.
    pub tenant: String,
    /// Its status.
    pub status: &'static str,
    /// Counts, total, last write.
    #[serde(flatten)]
    pub storage: TenantStats,
    /// Recent traffic.
    pub traffic: Traffic,
    /// Always `this-instance` (the request log is per process).
    pub traffic_scope: &'static str,
}

/// `POST /admin/secrets/$rewrap`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RewrapOutcome {
    /// Active key id.
    pub active_key: String,
    /// Secrets re-sealed.
    pub rewrapped: usize,
    /// Failures.
    pub failed: usize,
    /// Per scope.
    pub reports: Vec<RewrapReport>,
}

impl AdminEndpointsState {
    async fn reload(&self) {
        let i = self.inner();
        if let Err(e) = i.registry.reload_if_stale(i.store.as_ref()).await {
            tracing::warn!(error = %e, "endpoint snapshot reload after an admin write failed; the watcher will retry");
        }
    }

    async fn record(&self, actor: &Actor, event: AdminEvent<'_>) {
        self.inner().audit.record(actor, event).await;
    }

    fn require_key_ring_for(&self, inputs: &SecretInputs) -> ApiResult<()> {
        if secrets::sets_any(inputs) && self.inner().key_ring.is_none() {
            return Err(secrets::no_key_ring());
        }
        Ok(())
    }

    /// Whether storing a secret is possible (a key ring is configured).
    pub fn secrets_available(&self) -> bool {
        self.inner().key_ring.is_some()
    }

    /// The public FHIR base of `endpoint`.
    pub fn public_base(&self, endpoint: &Endpoint) -> String {
        endpoint.public_base(&self.inner().server_base)
    }

    /// Store epoch.
    pub async fn epoch(&self) -> ApiResult<u64> {
        Ok(self.inner().store.epoch().await?)
    }

    /// Endpoints, default first then by name.
    pub async fn list_endpoints(&self, include_deleted: bool) -> ApiResult<Vec<Endpoint>> {
        Ok(self.inner().store.list(include_deleted).await?)
    }

    /// One live endpoint (`404` when unknown).
    pub async fn get_endpoint(&self, name: &str) -> ApiResult<Endpoint> {
        self.inner()
            .store
            .get(name)
            .await?
            .ok_or_else(|| ApiError::endpoint_not_found(name))
    }

    /// Creates an endpoint.
    pub async fn create_endpoint(
        &self,
        mut spec: EndpointSpec,
        inputs: SecretInputs,
        actor: &Actor,
    ) -> ApiResult<Written> {
        let i = self.inner();
        // Never trust a sealed value handed in with a spec.
        if let Some(t) = spec.terminology.as_mut() {
            t.token = None;
        }
        if !matches!(
            spec.status,
            EndpointStatus::Enabled | EndpointStatus::Disabled
        ) {
            return Err(ApiError::invalid_field(
                "status",
                "a new endpoint is enabled or disabled (provisioning is set by $duplicate)",
            ));
        }
        spec.validate()?;
        check_release(&spec)?;
        self.require_key_ring_for(&inputs)?;
        // Dry run of the secret inputs (sections present, key ring) before
        // anything is stored; the real seal needs the id the store assigns.
        secrets::apply(&mut spec.clone(), "pending", &inputs, i.key_ring.as_deref())?;
        let warnings = outbound::check_spec(&i.outbound, i.resolver.as_ref(), &spec, None).await?;
        let live = i.store.list(false).await?;
        if live.len() >= i.max_endpoints {
            return Err(ApiError::new(
                axum::http::StatusCode::CONFLICT,
                "business-rule",
                format!(
                    "this deployment already has {} endpoints (limit {})",
                    live.len(),
                    i.max_endpoints
                ),
            ));
        }
        let mut created = i.store.create(spec, actor).await?;
        // Secrets are bound to the endpoint id, which the store assigns: seal
        // them now and store them as the next revision.
        if !inputs.is_empty() {
            let mut with_secrets = created.spec.clone();
            let stored = match secrets::apply(
                &mut with_secrets,
                &created.id,
                &inputs,
                i.key_ring.as_deref(),
            ) {
                Ok(()) => i
                    .store
                    .update(created.name(), created.revision, with_secrets, actor)
                    .await
                    .map_err(ApiError::from),
                Err(e) => Err(e),
            };
            match stored {
                Ok(e) => created = e,
                Err(err) => {
                    // Do not leave a half-configured endpoint behind.
                    let _ = i
                        .store
                        .set_status(
                            created.name(),
                            created.revision,
                            EndpointStatus::Deleted,
                            actor,
                        )
                        .await;
                    self.reload().await;
                    return Err(err);
                }
            }
        }
        self.reload().await;
        self.record(
            actor,
            AdminEvent {
                action: AuditAction::Create,
                outcome: "0",
                endpoint: Some(created.name()),
                description: format!(
                    "created endpoint {} (path /{}, tenant {})",
                    created.name(),
                    created.spec.path,
                    created.tenant_id()
                ),
                details: vec![
                    ("revision", created.revision.to_string()),
                    ("change", "create".into()),
                    ("endpointId", created.id.clone()),
                ],
            },
        )
        .await;
        Ok(Written {
            endpoint: created,
            warnings,
        })
    }

    /// Replaces the spec of `name` if its revision is `expected` (`409` with
    /// the current revision otherwise). `spec.status` must equal the current
    /// status (status changes go through [`Self::set_endpoint_status`] and
    /// [`Self::delete_endpoint`]). `change` names the edit in the audit
    /// trail (`replaced`, `patched`, `edited in the console`…).
    pub async fn update_endpoint(
        &self,
        name: &str,
        expected: u64,
        mut spec: EndpointSpec,
        inputs: SecretInputs,
        actor: &Actor,
        change: &str,
    ) -> ApiResult<Written> {
        let i = self.inner();
        let current = self.get_endpoint(name).await?;
        if spec.status != current.spec.status {
            return Err(ApiError::invalid_field(
                "status",
                "status changes go through $enable, $disable and DELETE",
            ));
        }
        // Stored secrets are carried over; only the inputs change them.
        if let Some(t) = spec.terminology.as_mut() {
            t.token = current
                .spec
                .terminology
                .as_ref()
                .and_then(|c| c.token.clone());
        }
        check_release(&spec)?;
        self.require_key_ring_for(&inputs)?;
        let warnings =
            outbound::check_spec(&i.outbound, i.resolver.as_ref(), &spec, Some(&current.spec))
                .await?;
        secrets::apply(&mut spec, &current.id, &inputs, i.key_ring.as_deref())?;
        let updated = i
            .store
            .update(current.name(), expected, spec, actor)
            .await?;
        self.reload().await;
        self.record(
            actor,
            AdminEvent {
                action: AuditAction::Update,
                outcome: "0",
                endpoint: Some(updated.name()),
                description: format!(
                    "{change} endpoint {} (revision {} → {})",
                    updated.name(),
                    expected,
                    updated.revision
                ),
                details: vec![
                    ("revision", updated.revision.to_string()),
                    ("change", change.to_string()),
                    (
                        "secrets",
                        inputs
                            .iter()
                            .map(|(k, v)| format!("{k}:{}", secret_verb(v)))
                            .collect::<Vec<_>>()
                            .join(","),
                    ),
                ],
            },
        )
        .await;
        Ok(Written {
            endpoint: updated,
            warnings,
        })
    }

    /// Enables or disables `name` (idempotent: no new revision when already
    /// in `status`). `expected` = the revision the caller saw, if any.
    pub async fn set_endpoint_status(
        &self,
        name: &str,
        expected: Option<u64>,
        status: EndpointStatus,
        actor: &Actor,
    ) -> ApiResult<Endpoint> {
        if matches!(status, EndpointStatus::Deleted) {
            return Err(ApiError::invalid_field(
                "status",
                "deleting an endpoint goes through DELETE",
            ));
        }
        let current = self.get_endpoint(name).await?;
        if current.spec.status == status {
            return Ok(current);
        }
        let expected = expected.unwrap_or(current.revision);
        let updated = self
            .inner()
            .store
            .set_status(name, expected, status, actor)
            .await?;
        self.reload().await;
        self.record(
            actor,
            AdminEvent {
                action: AuditAction::Update,
                outcome: "0",
                endpoint: Some(name),
                description: format!(
                    "endpoint {name}: {} → {}",
                    current.spec.status.as_str(),
                    status.as_str()
                ),
                details: vec![
                    ("revision", updated.revision.to_string()),
                    ("change", "status".into()),
                    ("status", status.as_str().into()),
                ],
            },
        )
        .await;
        Ok(updated)
    }

    /// Tombstones `name` (its name, path and tenant become reusable). With
    /// `purge` the endpoint is first disabled (so it stops taking writes),
    /// then its tenant's data is purged, then it is tombstoned — in the call,
    /// so the tenant cannot be re-bound to a new endpoint while a purge is
    /// running. A failed purge leaves the endpoint disabled; retrying finishes it.
    pub async fn delete_endpoint(
        &self,
        name: &str,
        expected: Option<u64>,
        purge: bool,
        actor: &Actor,
    ) -> ApiResult<Deleted> {
        let i = self.inner();
        let current = self.get_endpoint(name).await?;
        let mut expected = expected.unwrap_or(current.revision);
        if current.revision != expected {
            return Err(ApiError::conflict(current.revision));
        }
        if current.spec.is_default() {
            return Err(StoreError::Forbidden(
                "the default endpoint cannot be deleted (disable it instead)".into(),
            )
            .into());
        }
        if current.spec.managed_by == ManagedBy::Env {
            return Err(StoreError::Forbidden(
                "endpoint is managed by HFS_ENDPOINTS_FILE; edit the file and restart".into(),
            )
            .into());
        }
        let mut removed: Option<u64> = None;
        if purge {
            let ops = i.tenant_ops.as_ref().ok_or_else(|| {
                ApiError::not_implemented("tenant purge is not available in this server")
            })?;
            if current.spec.status != EndpointStatus::Disabled {
                let disabled = i
                    .store
                    .set_status(name, expected, EndpointStatus::Disabled, actor)
                    .await?;
                expected = disabled.revision;
                self.reload().await;
            }
            match ops.purge(current.tenant_id()).await {
                Ok(n) => removed = Some(n),
                Err(e) => {
                    self.record(
                        actor,
                        AdminEvent {
                            action: AuditAction::Delete,
                            outcome: "8",
                            endpoint: Some(name),
                            description: format!(
                                "purge of endpoint {name} (tenant {}) failed: {e}; the endpoint is left disabled",
                                current.tenant_id()
                            ),
                            details: vec![("change", "purge".into())],
                        },
                    )
                    .await;
                    return Err(e);
                }
            }
        }
        let gone = i
            .store
            .set_status(name, expected, EndpointStatus::Deleted, actor)
            .await?;
        self.reload().await;
        self.record(
            actor,
            AdminEvent {
                action: AuditAction::Delete,
                outcome: "0",
                endpoint: Some(name),
                description: match removed {
                    Some(n) => format!(
                        "deleted endpoint {name} and purged tenant {} ({n} resources)",
                        current.tenant_id()
                    ),
                    None => format!(
                        "deleted endpoint {name} (tenant {} data kept)",
                        current.tenant_id()
                    ),
                },
                details: vec![
                    ("revision", gone.revision.to_string()),
                    ("change", "delete".into()),
                    ("purged", removed.is_some().to_string()),
                ],
            },
        )
        .await;
        Ok(Deleted {
            endpoint: gone,
            resources_removed: removed,
        })
    }

    /// Revisions (newest first, at most `limit`, secrets redacted by the
    /// store) of the live endpoint `name`, or of the most recently deleted
    /// endpoint of that name.
    pub async fn endpoint_history(
        &self,
        name: &str,
        limit: usize,
    ) -> ApiResult<(Endpoint, Vec<EndpointRevision>)> {
        let i = self.inner();
        let e = match i.store.get(name).await? {
            Some(e) => e,
            None => i
                .store
                .list(true)
                .await?
                .into_iter()
                .filter(|e| e.name() == name)
                .max_by_key(|e| e.updated_at)
                .ok_or_else(|| ApiError::endpoint_not_found(name))?,
        };
        let revisions = i.store.history(&e.id, limit.clamp(1, 500)).await?;
        Ok((e, revisions))
    }

    /// Validates a duplicate request and hands it to the sync service.
    pub async fn duplicate_endpoint(
        &self,
        source: &str,
        request: DuplicateRequest,
        actor: &Actor,
    ) -> ApiResult<DuplicateStarted> {
        let i = self.inner();
        let src = self.get_endpoint(source).await?;
        let mut issues = Vec::new();
        if let Err(e) = check_segment(&request.name) {
            issues.push(Issue::field("invalid", "name", format!("name: {e}")));
        }
        let path = request.path.clone().unwrap_or_else(|| request.name.clone());
        if let Err(e) = check_segment(&path) {
            issues.push(Issue::field("invalid", "path", format!("path: {e}")));
        }
        if let Some(d) = &request.display_name
            && (d.trim().is_empty() || d.chars().count() > 120)
        {
            issues.push(Issue::field(
                "invalid",
                "displayName",
                "must be 1-120 characters",
            ));
        }
        if !issues.is_empty() {
            return Err(ApiError::unprocessable(issues));
        }
        for e in i.store.list(false).await? {
            let clash = if e.name() == request.name {
                Some(("name", request.name.as_str()))
            } else if e.spec.path.as_str() == path {
                Some(("path", path.as_str()))
            } else if e.tenant_id() == request.name {
                Some(("storage.tenantId", request.name.as_str()))
            } else {
                None
            };
            if let Some((field, value)) = clash {
                return Err(StoreError::Duplicate {
                    field,
                    value: value.to_string(),
                    by: e.name().to_string(),
                }
                .into());
            }
        }
        if request.secrets && i.key_ring.is_none() && !secrets::sealed_fields(&src.spec).is_empty()
        {
            return Err(secrets::no_key_ring());
        }
        let started = i.duplicate.start(&src, request.clone(), actor).await?;
        self.reload().await;
        self.record(
            actor,
            AdminEvent {
                action: AuditAction::Create,
                outcome: "0",
                endpoint: Some(source),
                description: format!(
                    "duplicate of endpoint {source} as {} started (job {}, data {:?})",
                    request.name, started.job_id, request.data
                ),
                details: vec![
                    ("change", "duplicate".into()),
                    ("target", request.name.clone()),
                    ("job", started.job_id.clone()),
                ],
            },
        )
        .await;
        Ok(started)
    }

    /// Probes `auth` (or, when `None`, the stored auth section of `endpoint`):
    /// issuer discovery, JWKS and, with `token`, a token check. With no
    /// endpoint (the new-endpoint wizard) an `auth` must be given. URLs pass
    /// the SSRF policy first, changed or not (the probe connects now); an
    /// env-managed endpoint's stored URLs are operator-trusted.
    pub async fn test_endpoint_auth(
        &self,
        endpoint: Option<&str>,
        auth: Option<EndpointAuth>,
        token: Option<&str>,
    ) -> ApiResult<AuthTestReport> {
        let i = self.inner();
        let current = match endpoint {
            Some(name) => Some(self.get_endpoint(name).await?),
            None => None,
        };
        let stored = auth.is_none();
        let auth = match (auth, &current) {
            (Some(a), _) => a,
            (None, Some(e)) => e.spec.auth.clone().ok_or_else(|| {
                ApiError::invalid_field(
                    "auth",
                    "the endpoint inherits the server's authentication (HFS_AUTH_*); there is no endpoint auth to test",
                )
            })?,
            (None, None) => {
                return Err(ApiError::invalid_field(
                    "auth",
                    "an auth section is required",
                ));
            }
        };
        let managed_by = match &current {
            Some(e) if stored => e.spec.managed_by,
            _ => ManagedBy::Ui,
        };
        if auth.mode != helios_endpoints::model::AuthMode::Off {
            if auth.issuer.as_deref().is_none_or(|s| s.trim().is_empty()) {
                return Err(ApiError::unprocessable(vec![Issue::field(
                    "required",
                    "auth.issuer",
                    "an issuer is required to test authentication",
                )]));
            }
            if managed_by == ManagedBy::Ui {
                let mut probe_spec = match &current {
                    Some(e) => e.spec.clone(),
                    None => EndpointSpec::default_endpoint("probe", FhirRelease::R4),
                };
                probe_spec.auth = Some(auth.clone());
                probe_spec.terminology = None;
                outbound::check_spec(&i.outbound, i.resolver.as_ref(), &probe_spec, None).await?;
            }
        }
        i.auth_probe.test(&auth, managed_by, token).await
    }

    /// Counts, last write and this instance's traffic over `window` seconds
    /// (clamped to 60..=86400).
    pub async fn endpoint_stats(&self, name: &str, window: i64) -> ApiResult<EndpointStatsView> {
        let e = self.get_endpoint(name).await?;
        let ops = self.inner().tenant_ops.as_ref().ok_or_else(|| {
            ApiError::not_implemented("endpoint statistics are not available in this server")
        })?;
        let storage = ops.stats(e.tenant_id()).await?;
        let window = window.clamp(60, 86_400);
        let traffic = helios_observability::reqlog::per_tenant(window)
            .into_iter()
            .find(|t| t.tenant == e.tenant_id())
            .map(|t| Traffic {
                window_seconds: window,
                samples: t.sample_count,
                requests_per_minute: t.requests_per_second * 60.0,
                p95_ms: t.p95_ms,
                error_rate: t.error_rate,
            })
            .unwrap_or(Traffic {
                window_seconds: window,
                ..Traffic::default()
            });
        Ok(EndpointStatsView {
            name: e.name().to_string(),
            tenant: e.tenant_id().to_string(),
            status: e.spec.status.as_str(),
            storage,
            traffic,
            traffic_scope: "this-instance",
        })
    }

    /// Counts of the tenant of `endpoint` (for list views), when available.
    pub async fn tenant_stats(&self, endpoint: &Endpoint) -> Option<ApiResult<TenantStats>> {
        match &self.inner().tenant_ops {
            Some(ops) => Some(ops.stats(endpoint.tenant_id()).await),
            None => None,
        }
    }

    /// One page (newest first) of the AuditEvents of `name`'s tenant.
    pub async fn endpoint_audit(
        &self,
        name: &str,
        count: u32,
        cursor: Option<String>,
    ) -> ApiResult<AuditPage> {
        let e = self.get_endpoint(name).await?;
        let search = self.inner().audit_search.as_ref().ok_or_else(|| {
            ApiError::not_implemented("audit search is not available in this server")
        })?;
        search
            .search(e.tenant_id(), count.clamp(1, 200), cursor)
            .await
    }

    /// Re-seals every endpoint secret (and those of the registered
    /// participants) under the active key.
    pub async fn rewrap_secrets(&self, actor: &Actor) -> ApiResult<RewrapOutcome> {
        self.rewrap_secrets_with(actor, &[]).await
    }

    /// [`Self::rewrap_secrets`] plus `extra` participants known only per
    /// request (the console runtime of the hfs wiring).
    pub async fn rewrap_secrets_with(
        &self,
        actor: &Actor,
        extra: &[&dyn super::RewrapParticipant],
    ) -> ApiResult<RewrapOutcome> {
        let i = self.inner();
        let ring = i.key_ring.clone().ok_or_else(secrets::no_key_ring)?;
        let mut report = RewrapReport {
            scope: "endpoints".into(),
            ..RewrapReport::default()
        };
        let mut touched = false;
        for e in i.store.list(false).await? {
            let mut spec = e.spec.clone();
            let out = secrets::rewrap(&mut spec, &e.id, &ring);
            report.current += out.current;
            report.failed.extend(
                out.failed
                    .iter()
                    .map(|f| format!("endpoint {}/{f}", e.name())),
            );
            if out.rewrapped.is_empty() {
                continue;
            }
            match i.store.update(e.name(), e.revision, spec, actor).await {
                Ok(_) => {
                    touched = true;
                    report.rewrapped += out.rewrapped.len();
                }
                Err(err) => report.failed.push(format!("endpoint {}: {err}", e.name())),
            }
        }
        if touched {
            self.reload().await;
        }
        let mut reports = vec![report];
        for p in &i.rewrap_participants {
            reports.push(p.rewrap(&ring, actor).await);
        }
        for p in extra {
            reports.push(p.rewrap(&ring, actor).await);
        }
        let rewrapped: usize = reports.iter().map(|r| r.rewrapped).sum();
        let failed: usize = reports.iter().map(|r| r.failed.len()).sum();
        self.record(
            actor,
            AdminEvent {
                action: AuditAction::Update,
                outcome: if failed == 0 { "0" } else { "4" },
                endpoint: None,
                description: format!(
                    "re-sealed {rewrapped} secret(s) under key {} ({failed} failure(s))",
                    ring.active_key_id()
                ),
                details: vec![("change", "rewrap".into())],
            },
        )
        .await;
        Ok(RewrapOutcome {
            active_key: ring.active_key_id().to_string(),
            rewrapped,
            failed,
            reports,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::admin_endpoints::audit::tests::CaptureSink;
    use crate::handlers::admin_endpoints::authz::AdminCredentialAuthorizer;
    use crate::handlers::admin_endpoints::error::ErrorKind;
    use crate::handlers::admin_endpoints::outbound::tests::FakeResolver;
    use helios_endpoints::model::TerminologyBinding;
    use helios_endpoints::{
        EndpointName, EndpointRegistry, MemoryEndpointStore, SecretKeyRing, ensure_default_endpoint,
    };
    use std::sync::Arc;

    fn admin() -> Actor {
        Actor {
            subject: "local:alice".into(),
            display: None,
            via: "console-session".into(),
        }
    }

    async fn state(ring: bool) -> (AdminEndpointsState, Arc<CaptureSink>) {
        let store = Arc::new(MemoryEndpointStore::new());
        ensure_default_endpoint(store.as_ref(), "default", FhirRelease::R4)
            .await
            .unwrap();
        let registry = Arc::new(EndpointRegistry::load(store.as_ref()).await.unwrap());
        let sink = Arc::new(CaptureSink::default());
        let s = AdminEndpointsState::builder(store, registry)
            .key_ring(ring.then(|| {
                Arc::new(SecretKeyRing::parse(&SecretKeyRing::generate_spec("k")).unwrap())
            }))
            .resolver(Arc::new(FakeResolver::default()))
            .authorizer(Arc::new(AdminCredentialAuthorizer::new(None)))
            .audit(Some(sink.clone()), "Device/hfs", "default")
            .server_base("https://h/fhir")
            .build();
        (s, sink)
    }

    fn spec(name: &str) -> EndpointSpec {
        EndpointSpec::named(EndpointName::parse(name).unwrap(), name)
    }

    #[test]
    fn release_and_verbs() {
        assert!(release_supported(FhirRelease::R4));
        assert!(check_release(&spec("a")).is_ok());
        assert_eq!(secret_verb(&SecretInput::Keep), "keep");
        assert_eq!(secret_verb(&SecretInput::Clear), "clear");
        assert_eq!(secret_verb(&SecretInput::Set("x".into())), "set");
    }

    #[cfg(not(feature = "R6"))]
    #[test]
    fn missing_release_is_refused() {
        let mut s = spec("a");
        s.fhir_version = FhirRelease::R6;
        let e = check_release(&s).unwrap_err();
        assert_eq!(e.issues[0].field.as_deref(), Some("fhirVersion"));
    }

    /// The typed path (what the console uses) enforces the same rules as HTTP.
    #[tokio::test]
    async fn typed_lifecycle_matches_the_api_rules() {
        let (s, sink) = state(true).await;
        assert!(s.secrets_available());
        // An injected sealed value is dropped; secrets only through inputs.
        let mut lab = spec("lab");
        let foreign = SecretKeyRing::parse(&SecretKeyRing::generate_spec("x"))
            .unwrap()
            .seal_str("evil", "endpoint:other:terminology.token");
        lab.terminology = Some(TerminologyBinding {
            server_url: Some("https://tx.example.org/fhir".into()),
            token: Some(foreign.clone()),
            ..Default::default()
        });
        let w = s
            .create_endpoint(lab, SecretInputs::new(), &admin())
            .await
            .unwrap();
        assert!(
            w.endpoint
                .spec
                .terminology
                .as_ref()
                .unwrap()
                .token
                .is_none()
        );
        assert_eq!(s.public_base(&w.endpoint), "https://h/fhir/lab");

        // Update carries the stored secret, ignores a typed one.
        let mut inputs = SecretInputs::new();
        inputs.insert("terminology.token".into(), SecretInput::Set("t".into()));
        let mut edit = w.endpoint.spec.clone();
        edit.display_name = "Lab".into();
        let w2 = s
            .update_endpoint("lab", 1, edit, inputs, &admin(), "edited in the console")
            .await
            .unwrap();
        let sealed = w2.endpoint.spec.terminology.clone().unwrap().token.unwrap();
        let mut edit = w2.endpoint.spec.clone();
        edit.terminology.as_mut().unwrap().token = Some(foreign);
        let w3 = s
            .update_endpoint("lab", 2, edit, SecretInputs::new(), &admin(), "edited")
            .await
            .unwrap();
        assert_eq!(
            w3.endpoint.spec.terminology.clone().unwrap().token.unwrap(),
            sealed
        );

        // Stale revision → conflict with current; status via the status call.
        let e = s
            .update_endpoint(
                "lab",
                1,
                w3.endpoint.spec.clone(),
                SecretInputs::new(),
                &admin(),
                "x",
            )
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Conflict { current: 3 });
        let mut st = w3.endpoint.spec.clone();
        st.status = EndpointStatus::Disabled;
        let e = s
            .update_endpoint("lab", 3, st, SecretInputs::new(), &admin(), "x")
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Invalid);
        assert_eq!(e.field_errors()[0].field, "status");
        let d = s
            .set_endpoint_status("lab", None, EndpointStatus::Disabled, &admin())
            .await
            .unwrap();
        assert_eq!(d.spec.status, EndpointStatus::Disabled);
        assert!(
            s.set_endpoint_status("lab", None, EndpointStatus::Deleted, &admin())
                .await
                .is_err()
        );
        let e = s
            .delete_endpoint("lab", Some(1), false, &admin())
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Conflict { current: 4 });
        let gone = s
            .delete_endpoint("lab", None, false, &admin())
            .await
            .unwrap();
        assert_eq!(gone.endpoint.spec.status, EndpointStatus::Deleted);
        assert!(gone.resources_removed.is_none());
        let (_, revs) = s.endpoint_history("lab", 100).await.unwrap();
        assert_eq!(revs.len(), 5);

        // Duplicates and not-found classify as such.
        s.create_endpoint(spec("dup"), SecretInputs::new(), &admin())
            .await
            .unwrap();
        let e = s
            .create_endpoint(spec("dup"), SecretInputs::new(), &admin())
            .await
            .unwrap_err();
        assert_eq!(
            e.kind(),
            ErrorKind::Duplicate {
                field: "name".into(),
                value: "dup".into(),
                by: "dup".into()
            }
        );
        assert_eq!(
            s.get_endpoint("nope").await.unwrap_err().kind(),
            ErrorKind::NotFound
        );
        // Same audit trail as the API.
        assert!(sink.0.lock().unwrap().len() >= 6);
        // Hooks that are not installed.
        assert_eq!(
            s.endpoint_stats("dup", 60).await.unwrap_err().kind(),
            ErrorKind::NotImplemented
        );
        assert_eq!(
            s.endpoint_audit("dup", 10, None).await.unwrap_err().kind(),
            ErrorKind::NotImplemented
        );
        assert!(s.tenant_stats(&w3.endpoint).await.is_none());
        let rq: DuplicateRequest =
            serde_json::from_value(serde_json::json!({"name": "copy"})).unwrap();
        assert_eq!(
            s.duplicate_endpoint("dup", rq, &admin())
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::NotImplemented
        );
    }

    #[tokio::test]
    async fn wizard_auth_test_without_an_endpoint() {
        let (s, _) = state(false).await;
        assert!(!s.secrets_available());
        let e = s.test_endpoint_auth(None, None, None).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Invalid);
        let auth = EndpointAuth {
            mode: helios_endpoints::AuthMode::Full,
            issuer: Some("https://idp.example.org/r".into()),
            jwks_url: Some("https://idp.example.org/c".into()),
            ..Default::default()
        };
        // SSRF passes, the placeholder probe answers not-implemented.
        let e = s
            .test_endpoint_auth(None, Some(auth), None)
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::NotImplemented);
        let bad = EndpointAuth {
            mode: helios_endpoints::AuthMode::Full,
            issuer: Some("https://10.0.0.1/r".into()),
            ..Default::default()
        };
        let e = s
            .test_endpoint_auth(None, Some(bad), None)
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Invalid);
        // No key ring: rewrap refused.
        assert_eq!(
            s.rewrap_secrets(&admin()).await.unwrap_err().kind(),
            ErrorKind::Forbidden
        );
        assert_eq!(s.epoch().await.unwrap(), 1);
        assert_eq!(s.list_endpoints(false).await.unwrap().len(), 1);
    }
}
