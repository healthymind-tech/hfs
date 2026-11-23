//! `GET /CodeSystem/$hm-explore` — the console explorers' paged reads
//! (HealthyMind). Runs on SQLite by default and on PostgreSQL with
//! `HTS_TEST_BACKEND=postgres`.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::Value;

const TREE: &str = "http://hm.test/explore/tree";
const FLAT: &str = "http://hm.test/explore/flat";
const ZH: &str = "http://hm.test/explore/tree-zh";

fn bundle() -> String {
    serde_json::json!({
        "resourceType": "Bundle",
        "type": "collection",
        "entry": [
            {"resource": {
                "resourceType": "CodeSystem", "id": "hm-explore-tree", "url": TREE, "version": "1",
                "name": "ExploreTree", "title": "Explore tree", "status": "active", "content": "complete",
                "hierarchyMeaning": "is-a",
                "concept": [
                    {"code": "A", "display": "Alpha root",
                     "designation": [{"language": "zh-Hant", "value": "阿爾法"}],
                     "concept": [
                        {"code": "A-1", "display": "Alpha one", "property": [{"code": "kind", "valueCode": "x"}],
                         "concept": [{"code": "A-1-a", "display": "Alpha one a"}]},
                        {"code": "A-2", "display": "Alpha two", "property": [{"code": "kind", "valueCode": "x"}]},
                        {"code": "B-1", "display": "Beta one", "property": [{"code": "kind", "valueCode": "y"}]}
                    ]},
                    {"code": "Z", "display": "Zulu root"}
                ]
            }},
            {"resource": {
                "resourceType": "CodeSystem", "id": "hm-explore-flat", "url": FLAT, "version": "1",
                "name": "ExploreFlat", "status": "active", "content": "complete",
                "concept": [
                    {"code": "f1", "display": "First flat", "property": [{"code": "grp", "valueString": "g1"}]},
                    {"code": "f2", "display": "Second flat", "property": [{"code": "grp", "valueString": "g1"}]},
                    {"code": "f3", "display": "Third flat", "property": [{"code": "grp", "valueString": "g2"}]}
                ]
            }},
            {"resource": {
                "resourceType": "CodeSystem", "id": "hm-explore-zh", "url": ZH, "version": "1",
                "name": "ExploreZh", "status": "active", "content": "complete",
                "concept": [
                    {"code": "A-1", "display": "阿爾法一"},
                    {"code": "A-2", "display": "阿爾法二"}
                ]
            }}
        ]
    })
    .to_string()
}

async fn app() -> TestApp {
    let app = TestApp::new().await;
    app.import_bundle_ok(&bundle()).await;
    app
}

async fn get(app: &TestApp, q: &str) -> Value {
    let (status, body) = app.get_fhir(&format!("/CodeSystem/$hm-explore?{q}")).await;
    assert_eq!(status, StatusCode::OK, "{q}: {body}");
    body
}

fn codes(v: &Value) -> Vec<String> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["code"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn stats_report_size_and_hierarchy() {
    let app = app().await;
    let s = get(&app, &format!("mode=stats&system={TREE}&props=1")).await;
    assert_eq!(s["concepts"], 6, "{s}");
    assert_eq!(s["hierarchy"], true);
    assert_eq!(s["hierarchyMeaning"], "is-a");
    assert_eq!(s["version"], "1");
    let props = s["properties"].as_array().unwrap();
    assert!(
        props
            .iter()
            .any(|p| p["code"] == "kind" && p["distinct"] == 2),
        "{s}"
    );
    let f = get(&app, &format!("mode=stats&system={FLAT}")).await;
    assert_eq!(f["hierarchy"], false);
    assert_eq!(f["concepts"], 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn children_are_paged_with_child_counts() {
    let app = app().await;
    let roots = get(&app, &format!("mode=children&system={TREE}&lang=zh")).await;
    assert_eq!(roots["roots"], true);
    assert_eq!(
        codes(&roots),
        vec!["A"],
        "only roots with children: {roots}"
    );
    assert_eq!(roots["items"][0]["children"], 3);
    assert_eq!(roots["items"][0]["designation"], "阿爾法");

    let page = get(
        &app,
        &format!("mode=children&system={TREE}&code=A&count=2&alt={ZH}"),
    )
    .await;
    assert_eq!(page["total"], 3);
    assert_eq!(codes(&page), vec!["A-1", "A-2"]);
    assert_eq!(page["items"][0]["children"], 1);
    assert_eq!(page["items"][0]["alt"], "阿爾法一");
    let next = get(
        &app,
        &format!("mode=children&system={TREE}&code=A&count=2&offset=2"),
    )
    .await;
    assert_eq!(codes(&next), vec!["B-1"]);

    let prefixed = get(
        &app,
        &format!("mode=children&system={TREE}&code=A&prefix=B"),
    )
    .await;
    assert_eq!(codes(&prefixed), vec!["B-1"]);
    assert_eq!(prefixed["total"], 1);

    let flat = get(&app, &format!("mode=children&system={FLAT}&order=display")).await;
    assert_eq!(flat["roots"], false);
    assert_eq!(flat["total"], 3);
    assert_eq!(codes(&flat), vec!["f1", "f2", "f3"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn buckets_group_children_by_code_segment() {
    let app = app().await;
    let b = get(&app, &format!("mode=buckets&system={TREE}&code=A")).await;
    assert_eq!(b["total"], 2, "{b}");
    assert_eq!(b["members"], 3);
    assert_eq!(b["items"][0]["key"], "A");
    assert_eq!(b["items"][0]["count"], 2);
    assert_eq!(b["items"][0]["sample"]["code"], "A-1");
    assert_eq!(b["items"][0]["sample"]["display"], "Alpha one");
}

#[tokio::test(flavor = "multi_thread")]
async fn facets_and_members_follow_the_filters() {
    let app = app().await;
    let f = get(&app, &format!("mode=facets&system={FLAT}&property=grp")).await;
    assert_eq!(f["matched"], 3, "{f}");
    let values = f["values"].as_array().unwrap();
    assert_eq!(values[0]["value"], "g1");
    assert_eq!(values[0]["count"], 2);
    let narrowed = get(
        &app,
        &format!("mode=facets&system={FLAT}&property=grp&filter=grp%3Dg2"),
    )
    .await;
    assert_eq!(narrowed["matched"], 1);
    assert_eq!(narrowed["values"].as_array().unwrap().len(), 1);

    let m = get(
        &app,
        &format!("mode=members&system={FLAT}&filter=grp%3Dg1&show=grp"),
    )
    .await;
    assert_eq!(m["total"], 2);
    assert_eq!(codes(&m), vec!["f1", "f2"]);
    assert_eq!(m["items"][0]["props"]["grp"]["value"], "g1");
    let text = get(&app, &format!("mode=members&system={FLAT}&q=second")).await;
    assert_eq!(codes(&text), vec!["f2"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn search_matches_codes_words_and_companion_labels() {
    let app = app().await;
    let exact = get(&app, &format!("mode=search&system={TREE}&q=A-1")).await;
    assert_eq!(codes(&exact)[0], "A-1", "{exact}");
    assert!(codes(&exact).contains(&"A-1-a".to_string()));
    let words = get(&app, &format!("mode=search&system={TREE}&q=one%20alpha")).await;
    assert_eq!(codes(&words), vec!["A-1", "A-1-a"], "{words}");
    let zh = get(
        &app,
        &format!("mode=search&system={TREE}&q=阿爾法二&alt={ZH}"),
    )
    .await;
    assert_eq!(codes(&zh), vec!["A-2"], "{zh}");
    let desig = get(&app, &format!("mode=search&system={TREE}&q=阿爾法")).await;
    assert_eq!(codes(&desig), vec!["A"], "{desig}");
}

#[tokio::test(flavor = "multi_thread")]
async fn node_has_parents_children_and_path() {
    let app = app().await;
    let n = get(&app, &format!("mode=node&system={TREE}&code=A-1-a")).await;
    assert_eq!(n["display"], "Alpha one a");
    assert_eq!(n["parents"][0]["code"], "A-1");
    assert_eq!(n["children"]["total"], 0);
    let path: Vec<&str> = n["path"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["code"].as_str().unwrap())
        .collect();
    assert_eq!(path, vec!["A", "A-1", "A-1-a"]);
    let (status, _) = app
        .get_fhir(&format!(
            "/CodeSystem/$hm-explore?mode=node&system={TREE}&code=nope"
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_requests_are_400_and_unknown_systems_404() {
    let app = app().await;
    for q in [
        "mode=children".to_string(),
        format!("mode=wat&system={TREE}"),
        format!("mode=facets&system={TREE}"),
        format!("mode=members&system={TREE}&filter=novalue"),
    ] {
        let (status, body) = app.get_fhir(&format!("/CodeSystem/$hm-explore?{q}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{q}: {body}");
    }
    let (status, _) = app
        .get_fhir("/CodeSystem/$hm-explore?mode=stats&system=http://nope")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
