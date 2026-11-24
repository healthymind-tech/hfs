//! `/admin/idp/realms/{realm}/users/{user}/links`: a user's resource access
//! in bulk (GET in three formats, PUT replace, PATCH add / explicit
//! changes), over the memory store. Its own test binary: the console
//! service is a process-wide global.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use helios_endpoints::Actor;
use helios_idp::admin::IdpAdmin;
use helios_idp::events::{IdpEvent, MemoryEventSink};
use helios_idp::memory::MemoryStore;
use helios_idp::runtime::{ConventionalEndpointUrls, IdpRuntime};
use helios_idp_admin::api::{self, Authorize};
use helios_idp_admin::service::{self, IdpConsole};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use url::Url;

fn auth() -> Authorize {
    api::authorize_sync(|_| {
        Ok(Actor {
            subject: "local:editor".into(),
            display: None,
            via: "console-session".into(),
        })
    })
}

async fn install() -> Arc<MemoryEventSink> {
    let base = Url::parse("http://127.0.0.1:9").unwrap();
    let ring = Arc::new(
        helios_endpoints::SecretKeyRing::parse(&helios_endpoints::SecretKeyRing::generate_spec(
            "t1",
        ))
        .unwrap(),
    );
    let rt = IdpRuntime::new(
        Arc::new(MemoryStore::new()),
        Some(ring),
        base.clone(),
        Arc::new(ConventionalEndpointUrls {
            public_base: base.to_string(),
        }),
    );
    let sink = Arc::new(MemoryEventSink::default());
    rt.set_event_sink(sink.clone());
    service::install(Arc::new(IdpConsole::new(IdpAdmin::new(rt), None)));
    sink
}

struct Resp {
    status: StatusCode,
    json: Value,
    text: String,
    etag: Option<String>,
    content_type: String,
}

async fn call(
    method: &str,
    path: &str,
    ctype: Option<&str>,
    body: &str,
    headers: &[(&str, &str)],
) -> Resp {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(c) = ctype {
        b = b.header("content-type", c);
    }
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let resp = api::router(Some(auth()))
        .oneshot(b.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let h = |n: &str| {
        resp.headers()
            .get(n)
            .map(|v| v.to_str().unwrap().to_string())
    };
    let etag = h("etag");
    let content_type = h("content-type").unwrap_or_default();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes).to_string();
    Resp {
        status,
        json: serde_json::from_str(&text).unwrap_or(Value::Null),
        text,
        etag,
        content_type,
    }
}

const LINKS: &str = "/admin/idp/realms/clinic/users/u1/links";

fn entries(v: &Value) -> Vec<(String, String, String)> {
    let mut out: Vec<_> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["endpoint"].as_str().unwrap().to_string(),
                e["reference"].as_str().unwrap().to_string(),
                e["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn bulk_links_api() {
    let sink = install().await;
    let r = call(
        "POST",
        "/admin/idp/realms",
        Some("application/json"),
        &json!({"name": "clinic", "displayName": "Clinic", "enabled": true,
                "userSource": {"kind": "local"}, "signingAlgs": ["RS256", "ES256"],
                "endpoints": ["clinic", "lab"]})
        .to_string(),
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    let r = call(
        "POST",
        "/admin/idp/realms/clinic/users",
        Some("application/json"),
        &json!({"id": "u1", "username": "drwu", "kind": "practitioner", "displayName": null,
                "email": null, "emailVerified": false, "externalSubject": null, "disabled": false,
                "links": [{"endpoint": "clinic", "reference": "Practitioner/d1", "isFhirUser": true,
                           "accessiblePatients": ["p1"]}]})
        .to_string(),
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);

    // GET: JSON with entries + ETag; CSV / lines by Accept or ?format.
    let r = call("GET", LINKS, None, "", &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["total"], 2);
    let etag = r.etag.clone().unwrap();
    assert_eq!(
        etag,
        format!("\"{}\"", r.json["fingerprint"].as_str().unwrap())
    );
    let r = call("GET", LINKS, None, "", &[("accept", "text/csv")]).await;
    assert!(r.content_type.starts_with("text/csv"));
    assert_eq!(
        r.text,
        "endpoint,reference,kind\nclinic,Practitioner/d1,fhirUser\nclinic,Patient/p1,patient\n"
    );
    let r = call("GET", &format!("{LINKS}?format=lines"), None, "", &[]).await;
    assert_eq!(r.text, "Practitioner/d1\nPatient/p1\n");
    let r = call("GET", &format!("{LINKS}?format=json"), None, "", &[]).await;
    assert_eq!(r.json[0]["accessiblePatients"], json!(["p1"]));

    // PATCH with lines: adds, keeps what is there, reports duplicates and
    // bad rows with 422 and stores nothing.
    let body = "Patient/p1\nPatient/p2\nPatient/p2\nObservation/9\nnope\n";
    let r = call(
        "PATCH",
        &format!("{LINKS}?endpoint=clinic"),
        Some("text/plain"),
        body,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.text);
    assert_eq!(r.json["counts"]["invalid"], 2);
    assert_eq!(r.json["counts"]["duplicate"], 1);
    let issues: Vec<&Value> = r.json["rejected"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| &x["issue"])
        .collect();
    assert!(
        issues.contains(&&json!("unsupported-type")) && issues.contains(&&json!("bad-reference")),
        "{issues:?}"
    );
    assert_eq!(r.json["rejected"][0]["status"], "duplicate");
    assert_eq!(call("GET", LINKS, None, "", &[]).await.json["total"], 2);
    // dryRun plans only.
    let r = call(
        "PATCH",
        &format!("{LINKS}?endpoint=clinic&dryRun=true&skipInvalid=true"),
        Some("text/plain"),
        body,
        &[],
    )
    .await;
    assert_eq!(
        (r.status, &r.json["added"]),
        (StatusCode::OK, &json!(1)),
        "{}",
        r.text
    );
    assert_eq!(call("GET", LINKS, None, "", &[]).await.json["total"], 2);
    // skipInvalid applies the good rows.
    let r = call(
        "PATCH",
        &format!("{LINKS}?endpoint=clinic&skipInvalid=true"),
        Some("text/plain"),
        body,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(
        (r.json["added"].clone(), r.json["total"].clone()),
        (json!(1), json!(3))
    );
    let ev = sink.take();
    assert!(
        ev.iter().any(|e| matches!(e, IdpEvent::UserAccessChanged { action, added: 1, actor, .. } if action == "add" && actor == "local:editor")),
        "{ev:?}"
    );

    // A stale If-Match is refused.
    let r = call(
        "PUT",
        LINKS,
        Some("text/csv"),
        "endpoint,reference,kind\nclinic,Patient/x,patient\n",
        &[("if-match", &etag)],
    )
    .await;
    assert_eq!(r.status, StatusCode::PRECONDITION_FAILED, "{}", r.text);

    // PUT CSV replaces; an unknown endpoint is a bad row.
    let csv = "endpoint,reference,kind\nclinic,Practitioner/d1,fhirUser\nlab,Patient/l1,patient\nmars,Patient/m,patient\n";
    let r = call("PUT", LINKS, Some("text/csv"), csv, &[]).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json["rejected"][0]["issue"], "unknown-endpoint");
    let r = call(
        "PUT",
        &format!("{LINKS}?skipInvalid=true"),
        Some("text/csv"),
        csv,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(
        (r.json["removed"].clone(), r.json["total"].clone()),
        (json!(2), json!(2))
    );
    let now = call("GET", LINKS, None, "", &[]).await;
    assert_eq!(
        entries(&now.json),
        vec![
            ("clinic".into(), "Practitioner/d1".into(), "fhirUser".into()),
            ("lab".into(), "Patient/l1".into(), "patient".into()),
        ]
    );

    // Round trip: what GET gives in each format PUTs back unchanged.
    for (fmt, ctype) in [
        ("csv", "text/csv"),
        ("lines", "text/plain"),
        ("json", "application/json"),
    ] {
        let r = call("GET", &format!("{LINKS}?format={fmt}"), None, "", &[]).await;
        let back = call(
            "PUT",
            LINKS,
            Some(ctype),
            &r.text,
            &[("if-match", now.etag.as_deref().unwrap())],
        )
        .await;
        assert_eq!(back.status, StatusCode::OK, "{fmt}: {}", back.text);
        assert_eq!(
            (
                back.json["added"].clone(),
                back.json["removed"].clone(),
                back.json["changed"].clone()
            ),
            (json!(0), json!(0), json!(0)),
            "{fmt}"
        );
    }

    // PATCH with explicit changes: a new identity demotes the old one.
    let r = call(
        "PATCH",
        LINKS,
        Some("application/json"),
        &json!({"add": [{"endpoint": "clinic", "reference": "PractitionerRole/r1", "kind": "fhirUser"}],
                "remove": [{"endpoint": "lab", "reference": "Patient/l1"}]})
        .to_string(),
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let now = call("GET", LINKS, None, "", &[]).await;
    assert_eq!(
        entries(&now.json),
        vec![
            ("clinic".into(), "Practitioner/d1".into(), "link".into()),
            (
                "clinic".into(),
                "PractitionerRole/r1".into(),
                "fhirUser".into()
            ),
        ]
    );
    // Unknown endpoints in explicit changes are refused.
    let r = call(
        "PATCH",
        LINKS,
        Some("application/json"),
        &json!({"add": [{"endpoint": "mars", "reference": "Patient/1", "kind": "patient"}]})
            .to_string(),
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.text);

    // Limits: more than 10,000 rows in one request.
    let many: String = (0..10_001).map(|i| format!("Patient/{i}\n")).collect();
    let r = call(
        "PATCH",
        &format!("{LINKS}?endpoint=clinic"),
        Some("text/plain"),
        &many,
        &[],
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        &r.text[..r.text.len().min(200)]
    );
    // 10,000 is fine (and the token-facing links keep them on the identity).
    let ok: String = (0..10_000).map(|i| format!("Patient/{i}\n")).collect();
    let r = call(
        "PATCH",
        &format!("{LINKS}?endpoint=clinic"),
        Some("text/plain"),
        &ok,
        &[],
    )
    .await;
    assert_eq!(
        (r.status, r.json["total"].clone()),
        (StatusCode::OK, json!(10_002)),
        "{}",
        r.text
    );
    let r = call("GET", &format!("{LINKS}?format=json"), None, "", &[]).await;
    let links = r.json.as_array().unwrap();
    let identity = links.iter().find(|l| l["isFhirUser"] == true).unwrap();
    assert_eq!(
        identity["accessiblePatients"].as_array().unwrap().len(),
        10_000
    );

    // Unknown user / realm.
    let r = call(
        "GET",
        "/admin/idp/realms/clinic/users/nobody/links",
        None,
        "",
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = call(
        "GET",
        "/admin/idp/realms/nope/users/u1/links",
        None,
        "",
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // Bad JSON.
    let r = call("PUT", LINKS, Some("application/json"), "[1,", &[]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.text);
}
