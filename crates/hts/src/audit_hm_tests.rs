//! Unit tests for HTS's audit wiring: the `/AuditEvent` read/search
//! handlers (status codes and OperationOutcome shapes), the dedicated
//! audit database opener, and the admin-user capture middleware.

use super::*;
use axum::body::Body;
use helios_audit::context::RequestAudit;
use tower::ServiceExt;

type SearchFn = fn(&[(String, String)]) -> Result<Vec<Value>, AuditSearchError>;
type ReadFn = fn(&str) -> Result<Option<Value>, AuditSearchError>;

/// A search that answers from a script.
struct Scripted {
    search: SearchFn,
    read: ReadFn,
}

#[async_trait::async_trait]
impl AuditEventSearch for Scripted {
    async fn search_audit_events(
        &self,
        params: &[(String, String)],
    ) -> Result<Vec<Value>, AuditSearchError> {
        (self.search)(params)
    }
    async fn read_audit_event(&self, id: &str) -> Result<Option<Value>, AuditSearchError> {
        (self.read)(id)
    }
}

async fn fetch(router: Router, uri: &str) -> (StatusCode, String, Value) {
    let resp = router
        .oneshot(axum::http::Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, ct, serde_json::from_slice(&body).unwrap())
}

fn ok_search() -> Scripted {
    Scripted {
        search: |params| {
            // Echo the parameters back so the test can see they arrived.
            Ok(params
                .iter()
                .map(|(k, v)| json!({"resourceType": "AuditEvent", "id": format!("{k}={v}")}))
                .collect())
        },
        read: |id| Ok((id == "known").then(|| json!({"resourceType": "AuditEvent", "id": "known"}))),
    }
}

#[tokio::test]
async fn search_returns_a_searchset_bundle() {
    let (status, ct, body) = fetch(
        router(Arc::new(ok_search())),
        "/AuditEvent?type=rest&_count=5",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct, "application/fhir+json");
    assert_eq!(body["resourceType"], "Bundle");
    assert_eq!(body["type"], "searchset");
    let entries = body["entry"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["resource"]["id"], "type=rest");
    assert_eq!(entries[1]["resource"]["id"], "_count=5");
    assert_eq!(entries[0]["search"]["mode"], "match");
}

#[tokio::test]
async fn search_errors_map_to_400_and_500() {
    let bad = Scripted {
        search: |_| Err(AuditSearchError::BadRequest("unknown parameter 'x'".into())),
        read: |_| Ok(None),
    };
    let (status, _, body) = fetch(router(Arc::new(bad)), "/AuditEvent?x=1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["resourceType"], "OperationOutcome");
    assert_eq!(body["issue"][0]["code"], "not-supported");
    assert_eq!(body["issue"][0]["diagnostics"], "unknown parameter 'x'");

    let broken = Scripted {
        search: |_| Err(AuditSearchError::Storage("disk on fire".into())),
        read: |_| Err(AuditSearchError::Storage("disk on fire".into())),
    };
    let (status, _, body) = fetch(router(Arc::new(broken)), "/AuditEvent").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["issue"][0]["code"], "exception");
    assert_eq!(body["issue"][0]["severity"], "error");
}

#[tokio::test]
async fn read_returns_the_event_404_or_500() {
    let (status, _, body) = fetch(router(Arc::new(ok_search())), "/AuditEvent/known").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], "known");

    let (status, _, body) = fetch(router(Arc::new(ok_search())), "/AuditEvent/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["issue"][0]["code"], "not-found");
    assert_eq!(
        body["issue"][0]["diagnostics"],
        "AuditEvent/nope is not known"
    );

    let broken = Scripted {
        search: |_| Ok(vec![]),
        read: |_| Err(AuditSearchError::Storage("gone".into())),
    };
    let (status, _, body) = fetch(router(Arc::new(broken)), "/AuditEvent/x").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["issue"][0]["code"], "exception");
}

#[tokio::test]
async fn a_dedicated_sqlite_audit_database_is_opened_and_searchable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.db");
    let (_storage, search, retention) =
        open_dedicated(path.to_str().unwrap(), &AuditConfig::default())
            .await
            .expect("open the dedicated SQLite audit database");
    assert!(
        retention.is_some(),
        "retention runs against the dedicated store"
    );
    assert!(path.exists(), "the database file was created");
    let found = search
        .search_audit_events(&[])
        .await
        .expect("an empty store searches fine");
    assert!(found.is_empty());
    assert!(search.read_audit_event("missing").await.unwrap().is_none());
}

#[cfg(not(feature = "postgres"))]
#[tokio::test]
async fn a_postgres_url_needs_the_postgres_feature() {
    for url in ["postgres://u@h/db", "  PostgreSQL://u@h/db"] {
        let err = open_dedicated(url, &AuditConfig::default())
            .await
            .err()
            .expect("postgres is not compiled in");
        assert!(err.to_string().contains("without `postgres`"), "{err}");
    }
}

#[tokio::test]
async fn the_admin_user_is_reported_to_the_audit_context() {
    let app = Router::new()
        .route("/admin/x", get(|| async { "ok" }))
        .layer(axum::middleware::from_fn(capture_admin_user));

    let ctx = RequestAudit::new();
    let mut req = axum::http::Request::get("/admin/x")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(helios_admin_auth::AdminUser("root".into()));
    let resp = ctx.clone().scope(app.clone().oneshot(req)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let user = ctx.snapshot().user.expect("user captured");
    assert_eq!(user.subject, "root");
    assert_eq!(user.issuer.as_deref(), Some("hts-admin"));
    assert_eq!(user.name.as_deref(), Some("root"));

    // Without an authenticated administrator nothing is reported.
    let ctx = RequestAudit::new();
    let req = axum::http::Request::get("/admin/x")
        .body(Body::empty())
        .unwrap();
    ctx.clone().scope(app.oneshot(req)).await.unwrap();
    assert!(ctx.snapshot().user.is_none());
}
