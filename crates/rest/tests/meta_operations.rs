//! Integration tests for the `$meta`, `$meta-add` and `$meta-delete`
//! operations at system, type and instance level.

use axum::http::StatusCode;
use axum_test::TestServer;
use helios_persistence::backends::sqlite::SqliteBackend;
use serde_json::{Value, json};

use helios_rest::{ServerConfig, create_app_with_config};

/// An HFS over an in-memory SQLite backend with the spec search parameters
/// (so `_tag` / `_profile` / `_security` are indexed).
fn server() -> TestServer {
    let data_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data");
    let backend = SqliteBackend::with_config(
        ":memory:",
        helios_persistence::backends::sqlite::SqliteBackendConfig {
            data_dir: Some(data_dir),
            ..Default::default()
        },
    )
    .expect("SQLite in-memory failed");
    backend.init_schema().expect("Schema init failed");
    TestServer::new(create_app_with_config(backend, ServerConfig::for_testing()))
        .expect("TestServer init failed")
}

fn meta_param(name: &str, meta: Value) -> Value {
    json!({"resourceType": "Parameters", "parameter": [{"name": name, "valueMeta": meta}]})
}

/// The `return` Meta of an operation response.
fn returned_meta(body: &Value) -> Value {
    assert_eq!(body["resourceType"], "Parameters", "{body}");
    assert_eq!(body["parameter"][0]["name"], "return", "{body}");
    body["parameter"][0]["valueMeta"].clone()
}

fn codes(meta: &Value, key: &str) -> Vec<String> {
    let mut codes: Vec<String> = meta[key]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|c| c["code"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default();
    codes.sort();
    codes
}

fn profiles(meta: &Value) -> Vec<String> {
    let mut p: Vec<String> = meta["profile"]
        .as_array()
        .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
        .unwrap_or_default();
    p.sort();
    p
}

#[tokio::test]
async fn instance_meta_add_and_delete_store_new_versions_and_feed_tag_search() {
    let server = server();
    let patient = json!({"resourceType": "Patient", "id": "p1", "active": true,
        "meta": {"tag": [{"system": "http://t", "code": "existing"}]}});
    assert!(
        server
            .put("/Patient/p1")
            .json(&patient)
            .await
            .status_code()
            .is_success()
    );

    // $meta: the stored meta, versionId included.
    let response = server.get("/Patient/p1/$meta").await;
    assert_eq!(response.status_code(), StatusCode::OK);
    let meta = returned_meta(&response.json());
    assert_eq!(meta["versionId"], "1");
    assert_eq!(codes(&meta, "tag"), ["existing"]);
    assert!(response.headers().get("etag").is_some());

    // $meta-add: new tag, security label and profile; `existing` is not
    // duplicated (display differs, system + code match).
    let add = meta_param(
        "meta",
        json!({
            "tag": [{"system": "http://t", "code": "added", "display": "Added"},
                    {"system": "http://t", "code": "existing", "display": "Existing"}],
            "security": [{"system": "http://terminology.hl7.org/CodeSystem/v3-Confidentiality", "code": "R"}],
            "profile": ["http://example.org/StructureDefinition/p"]
        }),
    );
    let response = server.post("/Patient/p1/$meta-add").json(&add).await;
    assert_eq!(
        response.status_code(),
        StatusCode::OK,
        "{}",
        response.text()
    );
    let meta = returned_meta(&response.json());
    assert_eq!(meta["versionId"], "2", "{meta}");
    assert_eq!(codes(&meta, "tag"), ["added", "existing"]);
    assert_eq!(codes(&meta, "security"), ["R"]);
    assert_eq!(
        profiles(&meta),
        ["http://example.org/StructureDefinition/p"]
    );
    assert_eq!(
        response.headers().get("etag").unwrap().to_str().unwrap(),
        "W/\"2\""
    );

    // The resource keeps its content and carries the new meta; history has
    // both versions.
    let stored: Value = server.get("/Patient/p1").await.json();
    assert_eq!(stored["active"], true);
    assert_eq!(stored["meta"]["versionId"], "2");
    assert_eq!(codes(&stored["meta"], "tag"), ["added", "existing"]);
    let history: Value = server.get("/Patient/p1/_history").await.json();
    assert_eq!(history["entry"].as_array().unwrap().len(), 2, "{history}");

    // Tag / security / profile searches see the change at once.
    for (query, expected) in [
        ("_tag=http://t|added", 1),
        (
            "_security=http://terminology.hl7.org/CodeSystem/v3-Confidentiality|R",
            1,
        ),
        ("_profile=http://example.org/StructureDefinition/p", 1),
        ("_tag=http://t|absent", 0),
    ] {
        let body: Value = server.get(&format!("/Patient?{query}")).await.json();
        assert_eq!(
            body["entry"].as_array().map_or(0, Vec::len),
            expected,
            "{query}: {body}"
        );
    }

    // Adding what is already there stores nothing.
    let response = server.post("/Patient/p1/$meta-add").json(&add).await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(returned_meta(&response.json())["versionId"], "2");

    // $meta-delete: by system + code; the profile too; an absent one is ignored.
    let delete = meta_param(
        "meta",
        json!({
            "tag": [{"system": "http://t", "code": "added"}, {"system": "http://other", "code": "existing"}],
            "profile": ["http://example.org/StructureDefinition/p"]
        }),
    );
    let response = server.post("/Patient/p1/$meta-delete").json(&delete).await;
    assert_eq!(
        response.status_code(),
        StatusCode::OK,
        "{}",
        response.text()
    );
    let meta = returned_meta(&response.json());
    assert_eq!(meta["versionId"], "3", "{meta}");
    assert_eq!(codes(&meta, "tag"), ["existing"]);
    assert_eq!(codes(&meta, "security"), ["R"]);
    assert!(meta.get("profile").is_none(), "{meta}");
    let body: Value = server.get("/Patient?_tag=http://t|added").await.json();
    assert_eq!(body["entry"].as_array().map_or(0, Vec::len), 0);

    // Deleting what is not there stores nothing.
    let response = server.post("/Patient/p1/$meta-delete").json(&delete).await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(returned_meta(&response.json())["versionId"], "3");

    // POST form of $meta.
    let response = server
        .post("/Patient/p1/$meta")
        .json(&json!({"resourceType": "Parameters"}))
        .await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(returned_meta(&response.json())["versionId"], "3");
}

#[tokio::test]
async fn type_and_system_meta_list_what_is_in_use() {
    let server = server();
    for resource in [
        json!({"resourceType": "Patient", "id": "p1",
            "meta": {"tag": [{"system": "http://t", "code": "a", "display": "A"}],
                     "profile": ["http://example.org/StructureDefinition/pat"]}}),
        json!({"resourceType": "Patient", "id": "p2",
            "meta": {"tag": [{"system": "http://t", "code": "a"}, {"system": "http://t", "code": "b"}],
                     "security": [{"system": "http://s", "code": "R"}]}}),
        json!({"resourceType": "Observation", "id": "o1", "status": "final", "code": {"text": "x"},
            "meta": {"tag": [{"system": "http://t", "code": "obs"}],
                     "profile": ["http://example.org/StructureDefinition/obs"]}}),
        // Deleted below: must not be listed.
        json!({"resourceType": "Patient", "id": "p3",
            "meta": {"tag": [{"system": "http://t", "code": "gone"}]}}),
    ] {
        let path = format!(
            "/{}/{}",
            resource["resourceType"].as_str().unwrap(),
            resource["id"].as_str().unwrap()
        );
        assert!(
            server
                .put(&path)
                .json(&resource)
                .await
                .status_code()
                .is_success()
        );
    }
    assert!(
        server
            .delete("/Patient/p3")
            .await
            .status_code()
            .is_success()
    );

    // Type level: Patient only.
    let response = server.get("/Patient/$meta").await;
    assert_eq!(response.status_code(), StatusCode::OK);
    let meta = returned_meta(&response.json());
    assert_eq!(codes(&meta, "tag"), ["a", "b"], "{meta}");
    assert_eq!(codes(&meta, "security"), ["R"]);
    assert_eq!(
        profiles(&meta),
        ["http://example.org/StructureDefinition/pat"]
    );
    assert!(meta.get("versionId").is_none());
    // Distinct: `a` once, with the display that was indexed.
    let a: Vec<&Value> = meta["tag"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["code"] == "a")
        .collect();
    assert_eq!(a.len(), 1, "{meta}");
    assert_eq!(a[0]["system"], "http://t");

    // System level: every type, the deleted Patient's tag excluded.
    let response = server.get("/$meta").await;
    assert_eq!(response.status_code(), StatusCode::OK);
    let meta = returned_meta(&response.json());
    assert_eq!(codes(&meta, "tag"), ["a", "b", "obs"], "{meta}");
    assert_eq!(
        profiles(&meta),
        [
            "http://example.org/StructureDefinition/obs",
            "http://example.org/StructureDefinition/pat"
        ]
    );
    let response = server
        .post("/$meta")
        .json(&json!({"resourceType": "Parameters"}))
        .await;
    assert_eq!(response.status_code(), StatusCode::OK);

    // A type with nothing tagged: an empty Meta.
    let response = server.get("/Encounter/$meta").await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(returned_meta(&response.json()), json!({}));
}

#[tokio::test]
async fn meta_operations_reject_bad_input_and_unknown_resources() {
    let server = server();
    assert!(
        server
            .put("/Patient/p1")
            .json(&json!({"resourceType": "Patient", "id": "p1"}))
            .await
            .status_code()
            .is_success()
    );

    for (path, body, needle) in [
        (
            "/Patient/p1/$meta-add",
            json!({"resourceType": "Parameters", "parameter": []}),
            "exactly one 'meta' parameter",
        ),
        (
            "/Patient/p1/$meta-add",
            meta_param("meta", json!({"tag": "nope"})),
            "meta.tag must be an array",
        ),
        (
            "/Patient/p1/$meta-add",
            meta_param("meta", json!({"tag": [{"system": "http://t"}]})),
            "Coding with a code",
        ),
        (
            "/Patient/p1/$meta-delete",
            meta_param("meta", json!({"versionId": "9"})),
            "at least one of profile, security or tag",
        ),
        (
            "/Patient/p1/$meta-add",
            meta_param("other", json!({"tag": [{"code": "x"}]})),
            "exactly one 'meta' parameter",
        ),
    ] {
        let response = server.post(path).json(&body).await;
        assert_eq!(
            response.status_code(),
            StatusCode::BAD_REQUEST,
            "{path} {body}"
        );
        let outcome: Value = response.json();
        assert_eq!(outcome["resourceType"], "OperationOutcome");
        assert!(
            outcome["issue"][0].to_string().contains(needle),
            "{path} {body}: {outcome}"
        );
    }

    // Unknown resource: 404 on all three.
    for path in [
        "/Patient/nope/$meta",
        "/Patient/nope/$meta-add",
        "/Patient/nope/$meta-delete",
    ] {
        let response = if path.ends_with("$meta") {
            server.get(path).await
        } else {
            server
                .post(path)
                .json(&meta_param(
                    "meta",
                    json!({"tag": [{"system": "http://t", "code": "x"}]}),
                ))
                .await
        };
        assert_eq!(response.status_code(), StatusCode::NOT_FOUND, "{path}");
    }

    // Deleted resource: 410.
    assert!(
        server
            .delete("/Patient/p1")
            .await
            .status_code()
            .is_success()
    );
    let response = server.get("/Patient/p1/$meta").await;
    assert_eq!(response.status_code(), StatusCode::GONE);
}
