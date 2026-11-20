//! Unit tests for the audit middleware's request helpers.

use super::*;

#[test]
fn read_request_classification_edge_cases() {
    assert!(is_read_request("get", "/Patient"));
    assert!(is_read_request("HEAD", "/metadata"));
    assert!(is_read_request("POST", "/Patient/_search/"));
    assert!(is_read_request("POST", "/Patient/1/$everything"));
    assert!(is_read_request("post", "/$meta"));
    for op in ["$lookup", "$translate", "$subsumes", "$validate", "$versions"] {
        assert!(is_read_request("POST", &format!("/CodeSystem/{op}")), "{op}");
    }
    assert!(!is_read_request("PUT", "/Patient/1"));
    assert!(!is_read_request("PATCH", "/Patient/1"));
    assert!(!is_read_request("POST", "/"));
    assert!(!is_read_request("POST", "/Patient/$meta-add"));
    assert!(!is_read_request("OPTIONS", "/Patient"));
}

#[test]
fn location_header_parsing() {
    assert_eq!(
        resource_from_location("http://h/fhir/Patient/p1?x=1#frag"),
        Some(("Patient".into(), "p1".into()))
    );
    assert_eq!(
        resource_from_location("Observation/o-1/_history/7"),
        Some(("Observation".into(), "o-1".into()))
    );
    // A tenant prefix before the type is skipped.
    assert_eq!(
        resource_from_location("https://h/acme/Encounter/e"),
        Some(("Encounter".into(), "e".into()))
    );
    assert_eq!(resource_from_location(""), None);
    assert_eq!(resource_from_location("http://h/Patient"), None);
    assert_eq!(resource_from_location("http://h/Patient/$everything"), None);
    // Only capitalised segments can be types.
    assert_eq!(resource_from_location("http://h/fhir/x"), None);
}

#[test]
fn site_of_base_urls() {
    assert_eq!(site_of("https://fhir.example.org/r4").as_deref(), Some("fhir.example.org"));
    assert_eq!(site_of("http://localhost:8080").as_deref(), Some("localhost:8080"));
    assert_eq!(site_of("http://h?x").as_deref(), Some("h"));
    assert_eq!(site_of("no-scheme/path"), None);
    assert_eq!(site_of("https:///path"), None);
}

#[test]
fn strip_tenant_cases() {
    let url_tenant = Captured {
        tenant: Some("acme".into()),
        tenant_from_url: true,
        ..Default::default()
    };
    // Resolved from the URL but the path does not start with it: unchanged.
    assert_eq!(strip_tenant("/other/Patient", &url_tenant, true), "/other/Patient");
    assert_eq!(strip_tenant("/acme", &url_tenant, true), "/");
    // Resolved from a header: never stripped.
    let header_tenant = Captured {
        tenant: Some("acme".into()),
        tenant_from_url: false,
        ..Default::default()
    };
    assert_eq!(strip_tenant("/acme/Patient", &header_tenant, true), "/acme/Patient");
    // Unresolved under URL routing: FHIR surface segments are not tenants,
    // and a lone segment is not stripped.
    let none = Captured::default();
    for p in ["/$export/x", "/_history/1", "/metadata/x", "/Patient/1"] {
        assert_eq!(strip_tenant(p, &none, true), p, "{p}");
    }
    assert_eq!(strip_tenant("/acme", &none, true), "/acme");
}

#[test]
fn request_id_sanitising_keeps_safe_punctuation() {
    assert_eq!(sanitize_request_id("req-1_a.b:c"), "req-1_a.b:c");
    assert_eq!(sanitize_request_id("心/\"'"), "");
}

fn request(xff: Option<&str>, peer: Option<&str>) -> axum::extract::Request {
    let mut r = axum::http::Request::get("/Patient")
        .body(Body::empty())
        .unwrap();
    if let Some(x) = xff {
        r.headers_mut().insert("x-forwarded-for", x.parse().unwrap());
    }
    if let Some(p) = peer {
        r.extensions_mut()
            .insert(ConnectInfo::<SocketAddr>(p.parse().unwrap()));
    }
    r
}

#[test]
fn client_address_honours_forwarded_for_only_when_trusted() {
    let r = request(Some("203.0.113.5, 10.0.0.1"), Some("10.0.0.1:443"));
    assert_eq!(client_address(&r, true).as_deref(), Some("203.0.113.5"));
    assert_eq!(client_address(&r, false).as_deref(), Some("10.0.0.1"));
    // Empty first entry falls back to the peer.
    let r = request(Some(" , 1.2.3.4"), Some("10.0.0.1:443"));
    assert_eq!(client_address(&r, true).as_deref(), Some("10.0.0.1"));
    // Long values are bounded.
    let long = "a".repeat(200);
    let r = request(Some(&long), None);
    assert_eq!(client_address(&r, true).unwrap().len(), 64);
    // Neither header nor peer.
    assert_eq!(client_address(&request(None, None), true), None);
}
