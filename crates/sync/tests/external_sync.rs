//! Sync with external FHIR servers: a HAPI-like mock (paging links,
//! `_history?_since`, deletes, 429 Retry-After, batch refusal, If-Match,
//! bearer auth, Bulk `$export`) and a second, real HFS instance (push and
//! pull, initial + incremental).

mod common;

use common::hapi::Hapi;
use common::hfs::start_hfs;
use common::*;
use helios_endpoints::sync::{RunReason, SyncRunState};
use serde_json::{Value, json};

fn pull_job(name: &str, base: &str, dst: &str) -> Value {
    json!({
        "name": name,
        "source": {"kind": "external", "baseUrl": base, "maxRequestsPerSecond": 100},
        "target": {"kind": "endpoint", "name": dst},
        "resourceTypes": ["Patient", "Observation"],
        "batchSize": 3,
    })
}

fn push_job(name: &str, src: &str, base: &str) -> Value {
    json!({
        "name": name,
        "source": {"kind": "endpoint", "name": src},
        "target": {"kind": "external", "baseUrl": base, "maxRequestsPerSecond": 100},
        "batchSize": 3,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_from_hapi_like_server_paging_history_deletes_and_429() {
    let hapi = Hapi::start().await;
    {
        let mut st = hapi.state.lock().unwrap();
        for i in 0..5 {
            st.put("Patient", &format!("h{i}"), json!({"active": true}));
        }
        st.put(
            "Observation",
            "o1",
            json!({"status": "final", "code": {"text": "x"}}),
        );
        st.throttle_next = 1; // the first request answers 429 Retry-After: 1
    }
    let f = fixture(config(&[])).await;
    let job = f.create_job(pull_job("pull", &hapi.base, "b")).await;
    let run = f.run_and_wait(&job).await;
    assert_eq!(run.state, SyncRunState::Succeeded, "{:?}", run.error);
    assert_eq!(run.progress.created, 6, "{:?}", run.progress);
    assert!(f.get("b", "Patient", "h4").await.is_some());
    {
        let st = hapi.state.lock().unwrap();
        assert!(
            st.requests.iter().any(|r| r.contains("_getpages")),
            "followed paging links"
        );
        assert!(st.requests.iter().any(|r| r.contains("_sort=_lastUpdated")));
    }

    // Changes at the source: update, delete, create.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    {
        let mut st = hapi.state.lock().unwrap();
        st.put("Patient", "h1", json!({"active": false}));
        st.delete("Patient", "h2");
        st.put("Patient", "h9", json!({"active": true}));
        for i in 10..14 {
            st.put("Patient", &format!("h{i}"), json!({}));
        }
    }
    let inc = f.run_and_wait(&job).await;
    assert_eq!(inc.state, SyncRunState::Succeeded, "{:?}", inc.error);
    assert_eq!(
        f.get("b", "Patient", "h1").await.unwrap().1["active"],
        json!(false)
    );
    assert!(
        f.get("b", "Patient", "h2").await.is_none(),
        "remote delete propagated"
    );
    assert!(
        f.get("b", "Patient", "h13").await.is_some(),
        "every history page read"
    );
    assert!(
        hapi.state
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|r| r.contains("_history?_since="))
    );
    assert_eq!(inc.progress.deleted, 1, "{:?}", inc.progress);

    // A further run sees nothing new.
    let quiet = f.run_and_wait(&job).await;
    assert_eq!(
        quiet.progress.created + quiet.progress.updated + quiet.progress.deleted,
        0,
        "{:?}",
        quiet.progress
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_with_bearer_auth_and_batch_fallback() {
    let hapi = Hapi::start().await;
    {
        let mut st = hapi.state.lock().unwrap();
        st.refuse_bundles = true;
        st.require_bearer = Some("s3cret".into());
    }
    let f = fixture(config(&[])).await;
    for i in 0..4 {
        f.put("a", "Patient", &format!("p{i}"), json!({"active": true}))
            .await;
    }
    let mut body = push_job("push", "a", &hapi.base);
    body["target"]["auth"] = json!({"type": "bearer"});
    body["secrets"] = json!({"target.auth.token": {"set": "s3cret"}});
    let created = f.svc.create(body, "t").await.unwrap();
    let text = serde_json::to_string(&created).unwrap();
    assert!(
        !text.contains("s3cret") && !text.contains("hmsec:"),
        "credentials never echoed"
    );
    let job = created.job["id"].as_str().unwrap().to_string();
    let run = f.run_and_wait(&job).await;
    assert_eq!(run.state, SyncRunState::Succeeded, "{:?}", run.error);
    assert_eq!(run.progress.created, 4, "{:?}", run.progress);
    let st = hapi.state.lock().unwrap();
    assert_eq!(
        st.bundle_posts, 1,
        "one refused batch, then individual writes"
    );
    assert!(st.individual_puts >= 4);
    assert!(st.current("Patient", "p3").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_fail_policy_detects_remote_edits() {
    let hapi = Hapi::start().await;
    let f = fixture(config(&[])).await;
    f.put("a", "Patient", "p1", json!({"gender": "male"})).await;
    f.put("a", "Patient", "p2", json!({"gender": "male"})).await;
    let mut body = push_job("push fail", "a", &hapi.base);
    body["conflict"] = json!("fail");
    let job = f.create_job(body).await;
    assert_eq!(f.run_and_wait(&job).await.progress.created, 2);
    assert_eq!(
        hapi.state.lock().unwrap().bundle_posts,
        1,
        "batch Bundle accepted"
    );
    // Remote edit, then a source change.
    hapi.state
        .lock()
        .unwrap()
        .put("Patient", "p1", json!({"gender": "other"}));
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    f.put("a", "Patient", "p1", json!({"gender": "female"}))
        .await;
    f.put("a", "Patient", "p2", json!({"gender": "female"}))
        .await;
    let r = f.run_and_wait(&job).await;
    assert_eq!(r.progress.conflicts, 1, "{:?}", r.progress);
    assert_eq!(r.progress.updated, 1, "{:?}", r.progress);
    let st = hapi.state.lock().unwrap();
    assert_eq!(
        st.current("Patient", "p1").unwrap()["gender"],
        json!("other")
    );
    assert_eq!(
        st.current("Patient", "p2").unwrap()["gender"],
        json!("female")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bulk_export_initial_load() {
    let hapi = Hapi::start().await;
    {
        let mut st = hapi.state.lock().unwrap();
        for i in 0..5 {
            st.put("Patient", &format!("x{i}"), json!({}));
        }
    }
    let f = fixture(config(&[])).await;
    let mut body = pull_job("bulk", &hapi.base, "b");
    body["initialLoad"] = json!("bulk-export");
    body["resourceTypes"] = json!(["Patient"]);
    let job = f.create_job(body).await;
    let run = f.run_and_wait(&job).await;
    assert_eq!(run.state, SyncRunState::Succeeded, "{:?}", run.error);
    assert!(f.get("b", "Patient", "x4").await.is_some());
    let st = hapi.state.lock().unwrap();
    assert!(
        st.requests.iter().any(|r| r.contains("export")),
        "{:?}",
        st.requests
    );
    assert!(st.export_polls >= 2, "polled until complete");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn links_leaving_the_server_are_refused() {
    let hapi = Hapi::start().await;
    {
        let mut st = hapi.state.lock().unwrap();
        for i in 0..5 {
            st.put("Patient", &format!("e{i}"), json!({}));
        }
        st.evil_next = true;
    }
    let f = fixture(config(&[])).await;
    let job = f.create_job(pull_job("evil", &hapi.base, "b")).await;
    let run = f.run_and_wait(&job).await;
    assert_eq!(run.state, SyncRunState::Failed);
    assert!(run.error.unwrap_or_default().contains("origin"));
    // Resumable: fix the server, retry continues.
    hapi.state.lock().unwrap().evil_next = false;
    let retry = f.svc.retry(&job, "t").await.unwrap();
    assert_eq!(retry.reason, RunReason::Retry);
    f.drain().await;
    assert_eq!(
        f.svc.run(&retry.id).await.unwrap().state,
        SyncRunState::Succeeded
    );
    assert!(f.get("b", "Patient", "e4").await.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strict_outbound_policy_refuses_loopback_at_run_time() {
    let hapi = Hapi::start().await;
    // Created where loopback is allowed…
    let f = fixture(config(&[])).await;
    let job = f.create_job(pull_job("ssrf", &hapi.base, "b")).await;
    // …then worked by an instance without the allow list.
    let strict = config(&[("HFS_OUTBOUND_ALLOW", "")]);
    let env = f.svc.runtime().env.clone();
    let strict_env = helios_sync::EngineEnv {
        store: env.store.clone(),
        registry: env.registry.clone(),
        local: env.local.clone(),
        copier: None,
        http: std::sync::Arc::new(helios_sync::http::GuardedHttp::new(
            strict.outbound.clone(),
            Default::default(),
        )),
        key_ring: env.key_ring.clone(),
        observer: None,
        hooks: Default::default(),
        settings: env.settings.clone(),
        wake: Default::default(),
    };
    let rec = f.svc.job(&job).await.unwrap();
    let err = match helios_sync::engine::build_peers(&strict_env, &rec) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("loopback must be refused"),
    };
    assert!(
        err.contains("outbound policy") || err.contains("scheme"),
        "{err}"
    );
    assert!(
        hapi.state.lock().unwrap().requests.is_empty(),
        "no connection was made"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscription_notifications_wake_a_pass() {
    let hapi = Hapi::start().await;
    hapi.state.lock().unwrap().put("Patient", "n1", json!({}));
    let f = fixture(config(&[])).await;
    let mut body = pull_job("notified", &hapi.base, "b");
    body["initialLoad"] = json!("none");
    body["since"] = json!("2000-01-01T00:00:00Z");
    body["trigger"] = json!({"type": "subscription", "fallbackPollSecs": 3600});
    let created = f.svc.create(body, "t").await.unwrap();
    let secret = created.notify_secret.clone().expect("secret shown once");
    assert!(created.notify_path.as_deref().unwrap().contains("/notify"));
    let job = created.job["id"].as_str().unwrap().to_string();
    assert!(!f.svc.notify(&job, Some("wrong")).await.unwrap());
    assert!(!f.svc.notify(&job, None).await.unwrap());
    assert!(!f.svc.notify("nope", Some(&secret)).await.unwrap());
    assert!(f.svc.notify(&job, Some(&secret)).await.unwrap());
    let runs = f.svc.runs(&job, 5).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].reason, RunReason::Notification);
    // GET never shows the secret.
    let shown = helios_sync::secrets::redacted_job(&f.svc.job(&job).await.unwrap()).to_string();
    assert!(!shown.contains(&secret) && !shown.contains("hmsec:"));
    let tests = f
        .svc
        .test_connection(pull_job("t", &hapi.base, "b"))
        .await
        .unwrap();
    assert!(
        tests[0].ok && tests[0].fhir_version.as_deref() == Some("4.0.1"),
        "{tests:?}"
    );
    assert!(tests[1].ok);
    f.svc.runtime().shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hfs_instances_push_and_pull() {
    let remote = start_hfs().await;
    let f = fixture(config(&[])).await;
    for i in 0..5 {
        f.put(
            "a",
            "Patient",
            &format!("t{i}"),
            json!({"active": true, "name": [{"family": format!("F{i}")}]}),
        )
        .await;
    }
    // Endpoint → second HFS instance (push).
    let push = f.create_job(push_job("to remote", "a", &remote)).await;
    let r = f.run_and_wait(&push).await;
    assert_eq!(r.state, SyncRunState::Succeeded, "{:?}", r.error);
    assert_eq!(r.progress.created, 5, "{:?}", r.progress);
    let http = reqwest::Client::new();
    let got: Value = http
        .get(format!("{remote}/Patient/t3"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["name"][0]["family"], "F3");

    // Second HFS instance → another endpoint (pull: paged read, then _history).
    let pull = f
        .create_job(json!({
            "name": "from remote",
            "source": {"kind": "external", "baseUrl": remote, "maxRequestsPerSecond": 100},
            "target": {"kind": "endpoint", "name": "c"},
            "resourceTypes": ["Patient"],
            "batchSize": 2,
        }))
        .await;
    let r = f.run_and_wait(&pull).await;
    assert_eq!(r.state, SyncRunState::Succeeded, "{:?}", r.error);
    assert_eq!(r.progress.created, 5, "{:?}", r.progress);

    // Edit and delete on the remote server, then pull incrementally.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let mut p1 = got.clone();
    p1["active"] = json!(false);
    let put = http
        .put(format!("{remote}/Patient/t3"))
        .header("content-type", "application/fhir+json")
        .body(p1.to_string())
        .send()
        .await
        .unwrap();
    assert!(put.status().is_success());
    let del = http
        .delete(format!("{remote}/Patient/t4"))
        .send()
        .await
        .unwrap();
    assert!(del.status().is_success());
    let r = f.run_and_wait(&pull).await;
    assert_eq!(r.state, SyncRunState::Succeeded, "{:?}", r.error);
    assert_eq!(
        f.get("c", "Patient", "t3").await.unwrap().1["active"],
        json!(false)
    );
    assert!(
        f.get("c", "Patient", "t4").await.is_none(),
        "remote delete pulled"
    );

    // Pushing again with nothing changed is a no-op at the remote.
    let before: Value = http
        .get(format!("{remote}/Patient/t0"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let r = f.run_and_wait(&push).await;
    assert_eq!(
        r.progress.created + r.progress.updated,
        0,
        "{:?}",
        r.progress
    );
    let after: Value = http
        .get(format!("{remote}/Patient/t0"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before["meta"]["versionId"], after["meta"]["versionId"]);
}
