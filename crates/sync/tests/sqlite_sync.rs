//! Endpoint → endpoint sync over SQLite storage (two tenants of one
//! backend): initial load, incremental updates and deletes, idempotency,
//! resume after a lost lease with fencing, conflict policies and their
//! resolution, continuous latency, scheduling, orphan sweep, duplication.

mod common;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use common::*;
use helios_endpoints::sync::{RunReason, SyncProgress, SyncRunState, SyncStoreError};
use helios_persistence::core::{JobKind, WorkerId};
use helios_sync::engine::{RunHandle, execute};
use serde_json::json;

#[tokio::test]
async fn initial_load_incremental_and_idempotency() {
    let f = fixture(config(&[])).await;
    for i in 0..5 {
        f.put("a", "Patient", &format!("p{i}"), json!({"active": true}))
            .await;
    }
    f.put(
        "a",
        "Observation",
        "o1",
        json!({"status": "final", "code": {"text": "x"}}),
    )
    .await;
    f.put(
        "a",
        "AuditEvent",
        "ae1",
        json!({"recorded": "2026-01-01T00:00:00Z"}),
    )
    .await;
    let job = f.create_job(local_job("a to b", "a", "b")).await;

    let run = f.run_and_wait(&job).await;
    assert_eq!(run.state, SyncRunState::Succeeded, "{:?}", run.error);
    assert_eq!(run.progress.created, 6, "{:?}", run.progress);
    assert_eq!(run.cursor.phase, "incremental");
    assert!(f.get("b", "Patient", "p3").await.is_some());
    assert!(
        f.get("b", "AuditEvent", "ae1").await.is_none(),
        "AuditEvent is excluded by default"
    );
    let (v1, _) = f.get("b", "Patient", "p0").await.unwrap();

    // Nothing changed: the next run writes nothing (no version churn).
    let again = f.run_and_wait(&job).await;
    assert_eq!(again.state, SyncRunState::Succeeded);
    assert_eq!(
        again.progress.created + again.progress.updated,
        0,
        "{:?}",
        again.progress
    );
    assert_eq!(f.get("b", "Patient", "p0").await.unwrap().0, v1);

    // Update + delete at the source flow to the target.
    tokio::time::sleep(Duration::from_millis(20)).await;
    f.put("a", "Patient", "p1", json!({"active": false})).await;
    f.delete("a", "Patient", "p2").await;
    f.put("a", "Patient", "p9", json!({"active": true})).await;
    let inc = f.run_and_wait(&job).await;
    assert_eq!(inc.state, SyncRunState::Succeeded, "{:?}", inc.error);
    assert_eq!(
        f.get("b", "Patient", "p1").await.unwrap().1["active"],
        json!(false)
    );
    assert!(
        f.get("b", "Patient", "p2").await.is_none(),
        "delete propagated"
    );
    assert!(f.get("b", "Patient", "p9").await.is_some());
    assert!(
        inc.progress.updated >= 1 && inc.progress.deleted == 1 && inc.progress.created == 1,
        "{:?}",
        inc.progress
    );
    // Unchanged resources were not rewritten.
    assert_eq!(f.get("b", "Patient", "p0").await.unwrap().0, v1);

    // Log carries type/id/version only and a summary.
    let log = f.svc.log(&inc.id, 0, 100).await.unwrap();
    assert!(log.iter().any(|l| l.message.starts_with("run finished")));
    assert!(
        log.iter().all(|l| !l.message.contains("\"active\"")),
        "no resource content in logs"
    );
}

#[tokio::test]
async fn deletes_not_propagated_when_disabled_and_type_scope() {
    let f = fixture(config(&[])).await;
    f.put("a", "Patient", "p1", json!({})).await;
    f.put(
        "a",
        "Observation",
        "o1",
        json!({"status": "final", "code": {"text": "x"}}),
    )
    .await;
    let mut body = local_job("scoped", "a", "b");
    body["resourceTypes"] = json!(["Patient"]);
    body["propagateDeletes"] = json!(false);
    let job = f.create_job(body).await;
    f.run_and_wait(&job).await;
    assert!(f.get("b", "Patient", "p1").await.is_some());
    assert!(f.get("b", "Observation", "o1").await.is_none());
    f.delete("a", "Patient", "p1").await;
    let r = f.run_and_wait(&job).await;
    assert_eq!(r.state, SyncRunState::Succeeded);
    assert!(
        f.get("b", "Patient", "p1").await.is_some(),
        "deletes not propagated"
    );
}

#[tokio::test]
async fn resume_after_lost_lease_without_duplicates_and_fencing() {
    let f = fixture(config(&[])).await;
    for i in 0..7 {
        f.put("a", "Patient", &format!("p{i}"), json!({"n": i}))
            .await;
    }
    let job = f.create_job(local_job("resume", "a", "b")).await;
    let run = f.svc.run_now(&job, "t").await.unwrap();

    // Worker A claims with a tiny lease and "crashes" after one batch.
    let wa = WorkerId::new("worker-a");
    let (lease_a, _) = f
        .cluster
        .claim_next(JobKind::EndpointSync, &wa, Duration::from_millis(50))
        .await
        .unwrap()
        .unwrap();
    let token_a = lease_a.fencing_token as i64;
    let claimed = f
        .store
        .claim_run(&run.id, lease_a.job_id.as_str(), token_a)
        .await
        .unwrap();
    let job_rec = f.svc.job(&job).await.unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let handle = RunHandle {
        run_id: run.id.clone(),
        token: token_a,
        stop: stop.clone(),
    };
    // Worker A "crashes" after its second target write: a write observer
    // flips its stop flag, so exactly one batch is applied and checkpointed.
    struct Crash(Arc<AtomicBool>, std::sync::atomic::AtomicUsize);
    impl helios_persistence::core::WriteObserver for Crash {
        fn on_write(&self, _: &helios_persistence::core::WriteEvent) {
            if self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 >= 2 {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
    let base = f.svc.runtime().env.clone();
    let env_a = helios_sync::EngineEnv {
        store: base.store.clone(),
        registry: base.registry.clone(),
        local: base.local.clone(),
        copier: None,
        http: base.http.clone(),
        key_ring: base.key_ring.clone(),
        observer: Some(Arc::new(Crash(stop.clone(), Default::default()))),
        hooks: Default::default(),
        settings: base.settings.clone(),
        wake: Default::default(),
    };
    let res = execute(&env_a, &job_rec, &claimed, &handle).await;
    assert!(res.is_err(), "worker A stopped");
    let partial = f.store.run(&run.id).await.unwrap().unwrap();
    assert_eq!(partial.progress.created, 2, "one batch checkpointed");
    let applied_by_a = partial.progress.created;

    // Lease expires; worker B (the runtime) reclaims and finishes from the checkpoint.
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(f.drain().await, 1);
    let done = f.svc.run(&run.id).await.unwrap();
    assert_eq!(done.state, SyncRunState::Succeeded, "{:?}", done.error);
    for i in 0..7 {
        let (v, _) = f.get("b", "Patient", &format!("p{i}")).await.unwrap();
        assert_eq!(v, "1", "p{i} written exactly once");
    }
    assert_eq!(
        done.progress.created, 7,
        "B continued A's counters: {:?} (A applied {applied_by_a})",
        done.progress
    );

    // The stale worker is fenced.
    assert!(matches!(
        f.store
            .checkpoint(
                &run.id,
                token_a,
                SyncRunState::Running,
                &done.cursor,
                &SyncProgress::default(),
                None
            )
            .await,
        Err(SyncStoreError::LeaseLost)
    ));
}

#[tokio::test]
async fn conflict_policies_and_resolution() {
    let f = fixture(config(&[])).await;
    f.put("a", "Patient", "p1", json!({"gender": "male"})).await;
    f.put("a", "Patient", "p2", json!({"gender": "female"}))
        .await;
    let mut body = local_job("fail policy", "a", "b");
    body["conflict"] = json!("fail");
    let job = f.create_job(body).await;
    assert_eq!(f.run_and_wait(&job).await.progress.created, 2);

    // Someone edits the target copy, then the source changes.
    f.put("b", "Patient", "p1", json!({"gender": "other"}))
        .await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    f.put("a", "Patient", "p1", json!({"gender": "unknown"}))
        .await;
    f.put("a", "Patient", "p2", json!({"gender": "male"})).await;
    let r = f.run_and_wait(&job).await;
    assert_eq!(r.state, SyncRunState::Succeeded);
    assert_eq!(r.progress.conflicts, 1, "{:?}", r.progress);
    assert_eq!(
        f.get("b", "Patient", "p1").await.unwrap().1["gender"],
        json!("other"),
        "target kept"
    );
    assert_eq!(
        f.get("b", "Patient", "p2").await.unwrap().1["gender"],
        json!("male"),
        "untouched resource synced"
    );
    let conflicts = f.svc.conflicts(&r.id, 10).await.unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].resource_id, "p1");

    // Overwrite = source wins for this item.
    f.svc
        .resolve_conflict(&r.id, "Patient", "p1", "overwrite", "t")
        .await
        .unwrap();
    assert_eq!(
        f.get("b", "Patient", "p1").await.unwrap().1["gender"],
        json!("unknown")
    );
    assert!(f.svc.conflicts(&r.id, 10).await.unwrap().is_empty());
    assert!(
        f.svc
            .resolve_conflict(&r.id, "Patient", "p1", "keep", "t")
            .await
            .is_err(),
        "already resolved"
    );

    // skip-if-target-newer: target edited after the source change wins.
    let mut body = local_job("newer policy", "a", "c");
    body["conflict"] = json!("skip-if-target-newer");
    let job2 = f.create_job(body).await;
    f.run_and_wait(&job2).await;
    f.put("a", "Patient", "p2", json!({"gender": "female"}))
        .await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    f.put("c", "Patient", "p2", json!({"gender": "other"}))
        .await;
    let r2 = f.run_and_wait(&job2).await;
    assert_eq!(r2.progress.conflicts, 1, "{:?}", r2.progress);
    // Keep = accept the target; the next run reports nothing new.
    f.svc
        .resolve_conflict(&r2.id, "Patient", "p2", "keep", "t")
        .await
        .unwrap();
    assert_eq!(
        f.get("c", "Patient", "p2").await.unwrap().1["gender"],
        json!("other")
    );
}

#[tokio::test]
async fn validation_rules() {
    let f = fixture(config(&[])).await;
    let bad = |b: serde_json::Value| async {
        match f.svc.create(b, "t").await {
            Err(helios_sync::SyncError::Invalid(v)) => {
                v.0.into_iter().map(|e| e.field).collect::<Vec<_>>()
            }
            other => panic!("expected 422, got {other:?}"),
        }
    };
    assert!(
        bad(local_job("x", "a", "nope"))
            .await
            .contains(&"target.name".to_string())
    );
    assert!(
        bad(local_job("x", "a", "a"))
            .await
            .contains(&"target".to_string())
    );
    let mut sc = local_job("x", "a", "b");
    sc["initialLoad"] = json!("storage-copy");
    assert!(
        bad(sc).await.contains(&"initialLoad".to_string()),
        "no copier on SQLite"
    );
    let mut flt = local_job("x", "a", "b");
    flt["filters"] = json!({"Observation": "category=laboratory"});
    assert!(bad(flt).await.contains(&"filters".to_string()));
    // SSRF: internal address not allow-listed, plain http to a public host.
    let ext = |url: &str| {
        json!({"name": "e", "source": {"kind": "endpoint", "name": "a"},
                                 "target": {"kind": "external", "baseUrl": url}})
    };
    assert!(
        bad(ext("https://10.1.2.3/fhir"))
            .await
            .contains(&"target.baseUrl".to_string())
    );
    assert!(
        bad(ext("https://169.254.169.254/latest"))
            .await
            .contains(&"target.baseUrl".to_string())
    );
    assert!(
        bad(ext("http://fhir.example.org/fhir"))
            .await
            .contains(&"target.baseUrl".to_string())
    );
    // Cycles: a → b exists, b → a refused; disabled jobs don't count.
    f.create_job(local_job("ab", "a", "b")).await;
    assert!(
        bad(local_job("ba", "b", "a"))
            .await
            .contains(&"target".to_string())
    );
    let mut off = local_job("ba off", "b", "a");
    off["enabled"] = json!(false);
    f.create_job(off).await;
    // Duplicate names.
    assert!(matches!(
        f.svc.create(local_job("ab", "a", "c"), "t").await,
        Err(helios_sync::SyncError::Conflict(_))
    ));
    // External peers switched off by the operator.
    let g = fixture(config(&[("HFS_SYNC_EXTERNAL", "off")])).await;
    assert!(matches!(
        g.svc
            .create(ext("https://hapi.example.org/fhir"), "t")
            .await,
        Err(helios_sync::SyncError::Invalid(_))
    ));
}

#[tokio::test]
async fn update_cancel_retry_delete() {
    let f = fixture(config(&[])).await;
    f.put("a", "Patient", "p1", json!({})).await;
    let job = f.create_job(local_job("ops", "a", "b")).await;
    let j = f.svc.job(&job).await.unwrap();
    // Update with a stale revision is refused.
    let mut spec = serde_json::to_value(&j.spec).unwrap();
    spec["description"] = json!("changed");
    assert!(matches!(
        f.svc.update(&job, Some(99), spec.clone(), "t").await,
        Err(helios_sync::SyncError::Revision { .. })
    ));
    let u = f
        .svc
        .update(&job, Some(j.revision), spec, "t")
        .await
        .unwrap();
    assert_eq!(u.revision, j.revision + 1);

    // Cancel a queued run; retry resumes it.
    let run = f.svc.run_now(&job, "t").await.unwrap();
    assert!(matches!(
        f.svc.run_now(&job, "t").await,
        Err(helios_sync::SyncError::Conflict(_))
    ));
    let c = f.svc.cancel(&job, "t").await.unwrap().unwrap();
    assert_eq!(c.state, SyncRunState::Cancelled);
    assert_eq!(
        f.drain().await,
        0,
        "the cancelled run's queue entry is cancelled too"
    );
    assert!(f.get("b", "Patient", "p1").await.is_none());
    assert!(f.svc.cancel(&job, "t").await.unwrap().is_none());
    let retry = f.svc.retry(&job, "t").await.unwrap();
    assert_eq!(retry.reason, RunReason::Retry);
    f.drain().await;
    assert_eq!(
        f.svc.run(&retry.id).await.unwrap().state,
        SyncRunState::Succeeded
    );
    assert!(
        f.svc.retry(&job, "t").await.is_err(),
        "nothing to retry after success"
    );
    assert_eq!(f.svc.runs(&job, 10).await.unwrap().len(), 2);
    assert_ne!(run.id, retry.id);
    f.svc.delete(&job, "t").await.unwrap();
    assert!(f.svc.job(&job).await.is_err());
}

#[tokio::test]
async fn scheduler_interval_and_orphan_sweep() {
    let f = fixture(config(&[])).await;
    f.put("a", "Patient", "p1", json!({})).await;
    let mut body = local_job("every minute", "a", "b");
    body["trigger"] = json!({"type": "interval", "everySecs": 60});
    let job = f.create_job(body).await;
    f.svc.runtime().schedule_once().await.unwrap();
    let runs = f.svc.runs(&job, 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].reason, RunReason::Schedule);
    let next = f.svc.job(&job).await.unwrap().next_run_at.unwrap();
    assert!(next > chrono::Utc::now() + chrono::Duration::seconds(50));
    f.svc.runtime().schedule_once().await.unwrap();
    assert_eq!(f.svc.runs(&job, 10).await.unwrap().len(), 1, "not due yet");
    f.drain().await;
    assert!(f.get("b", "Patient", "p1").await.is_some());

    // Orphan: a queued run whose cluster job vanished is re-enqueued.
    let manual = f.create_job(local_job("orphan", "a", "c")).await;
    let orphan = f
        .store
        .create_run(&manual, RunReason::Manual, None)
        .await
        .unwrap();
    let mut settings = helios_sync::RuntimeSettings::default();
    settings.orphan_after = Duration::ZERO;
    let rt = helios_sync::SyncRuntime::new(
        f.svc.runtime().env.clone(),
        f.cluster.clone(),
        "sweeper",
        settings,
        None,
        None,
    );
    tokio::time::sleep(Duration::from_millis(5)).await;
    rt.schedule_once().await.unwrap();
    assert!(
        f.store
            .run(&orphan.id)
            .await
            .unwrap()
            .unwrap()
            .cluster_job_id
            .is_some()
    );
    f.drain().await;
    assert_eq!(
        f.svc.run(&orphan.id).await.unwrap().state,
        SyncRunState::Succeeded
    );
    assert!(f.get("c", "Patient", "p1").await.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continuous_sync_latency() {
    let f = fixture(config(&[("HFS_SYNC_ENABLED", "true")])).await;
    let mut body = local_job("live", "a", "b");
    body["initialLoad"] = json!("none");
    body["trigger"] = json!({"type": "continuous", "pollSecs": 1});
    let job = f.create_job(body).await;
    // Start it now (the running scheduler may already have).
    let _ = f.svc.run_now(&job, "t").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let mut worst = Duration::ZERO;
    for i in 0..3 {
        let id = format!("live{i}");
        f.put("a", "Patient", &id, json!({"n": i})).await;
        // What the server's write observer does on a committed write
        // (`SyncWriteWaker`): wake the continuous jobs reading tenant `a`.
        f.svc.runtime().wake_tenant("a");
        let t0 = Instant::now();
        loop {
            if f.get("b", "Patient", &id).await.is_some() {
                break;
            }
            if t0.elapsed() > Duration::from_secs(10) {
                let runs = f.svc.runs(&job, 3).await.unwrap();
                let log = f.svc.log(&runs[0].id, 0, 100).await.unwrap();
                panic!("not synced in 10 s: {runs:#?} {log:#?}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        worst = worst.max(t0.elapsed());
    }
    println!("continuous local->local worst latency: {worst:?}");
    assert!(worst <= Duration::from_secs(2), "latency {worst:?}");
    // Disabling the job ends the continuous run.
    let j = f.svc.job(&job).await.unwrap();
    let mut spec = serde_json::to_value(&j.spec).unwrap();
    spec["enabled"] = json!(false);
    f.svc.update(&job, None, spec, "t").await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let last = f.svc.runs(&job, 1).await.unwrap().remove(0);
    assert!(last.state.is_terminal(), "{:?}", last.state);
    f.svc.runtime().shutdown();
}

#[tokio::test]
async fn duplicate_endpoint_with_data_and_keep_in_sync() {
    let f = fixture(config(&[])).await;
    f.put("a", "Patient", "p1", json!({"active": true})).await;
    f.put(
        "a",
        "StructureDefinition",
        "sd1",
        json!({"url": "http://x/sd1", "name": "X", "status": "active",
        "kind": "resource", "abstract": false, "type": "Patient"}),
    )
    .await;
    let actor = helios_endpoints::store::Actor::bootstrap();

    // Configuration only, no packages: enabled immediately, no job.
    let out = f
        .svc
        .duplicate(
            "a",
            serde_json::from_value(json!({"name": "empty", "packages": false})).unwrap(),
            &actor,
        )
        .await
        .unwrap();
    assert!(out.job_id.is_none());
    assert_eq!(out.endpoint["status"], "enabled");

    // Current data + keep in sync.
    let out = f
        .svc
        .duplicate(
            "a",
            serde_json::from_value(json!({"name": "copy", "data": "current", "keepInSync": true}))
                .unwrap(),
            &actor,
        )
        .await
        .unwrap();
    assert_eq!(out.endpoint["status"], "provisioning");
    assert_eq!(out.endpoint["storage"]["tenantId"], "copy");
    f.drain().await;
    let run = f.svc.run(out.run_id.as_deref().unwrap()).await.unwrap();
    assert_eq!(run.state, SyncRunState::Succeeded, "{:?}", run.error);
    assert!(f.get("copy", "Patient", "p1").await.is_some());
    assert!(f.get("copy", "StructureDefinition", "sd1").await.is_some());
    let ep = f.endpoints.get("copy").await.unwrap().unwrap();
    assert_eq!(
        ep.spec.status,
        helios_endpoints::model::EndpointStatus::Enabled,
        "enabled after the copy"
    );
    let follow = f
        .svc
        .job(out.follow_up_job_id.as_deref().unwrap())
        .await
        .unwrap();
    assert!(
        follow.spec.enabled,
        "keep-in-sync job switched on after the copy"
    );

    // Packages only: conformance resources, no patient data.
    let out = f
        .svc
        .duplicate(
            "a",
            serde_json::from_value(json!({"name": "pkgs"})).unwrap(),
            &actor,
        )
        .await
        .unwrap();
    f.drain().await;
    assert_eq!(
        f.svc
            .run(out.run_id.as_deref().unwrap())
            .await
            .unwrap()
            .state,
        SyncRunState::Succeeded
    );
    assert!(f.get("pkgs", "StructureDefinition", "sd1").await.is_some());
    assert!(f.get("pkgs", "Patient", "p1").await.is_none());

    // Errors.
    assert!(matches!(
        f.svc
            .duplicate(
                "nope",
                serde_json::from_value(json!({"name": "x"})).unwrap(),
                &actor
            )
            .await,
        Err(helios_sync::SyncError::NotFound(_))
    ));
    assert!(matches!(
        f.svc
            .duplicate(
                "a",
                serde_json::from_value(json!({"name": "admin"})).unwrap(),
                &actor
            )
            .await,
        Err(helios_sync::SyncError::Invalid(_))
    ));
    assert!(
        f.svc
            .duplicate(
                "a",
                serde_json::from_value(json!({"name": "copy"})).unwrap(),
                &actor
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn store_conformance_in_memory() {
    helios_sync::memory::conformance::run_all(&helios_sync::memory::MemorySyncStore::new()).await;
    let _ = JobKind::EndpointSync.as_str();
    assert_eq!(JobKind::parse("endpoint-sync"), Some(JobKind::EndpointSync));
}

#[tokio::test]
async fn write_waker_wakes_continuous_jobs_of_the_written_tenant() {
    use helios_persistence::core::WriteObserver;
    let f = fixture(config(&[])).await;
    let mut body = local_job("woken", "a", "b");
    body["initialLoad"] = json!("none");
    body["trigger"] = json!({"type": "continuous", "pollSecs": 3600});
    let job = f.create_job(body).await;
    let notify = f.svc.runtime().env.wake.get(&job);
    let waker = helios_sync::runtime::SyncWriteWaker(Arc::downgrade(f.svc.runtime()));
    let write = |tenant: &str| {
        helios_persistence::core::WriteEvent::Resource(helios_persistence::core::ResourceWrite {
            tenant: helios_persistence::tenant::TenantId::new(tenant),
            fhir_version: helios_fhir::FhirVersion::R4,
            resource_type: "Patient".into(),
            live_delta: 1,
            notice: None,
            at: chrono::Utc::now(),
        })
    };
    // A write elsewhere does not wake it.
    waker.on_write(&write("c"));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), notify.notified()).await.is_err()
    );
    // A write in its source tenant does.
    waker.on_write(&write("a"));
    assert!(
        tokio::time::timeout(Duration::from_millis(500), notify.notified()).await.is_ok()
    );
    assert!(!waker.retired());
}
