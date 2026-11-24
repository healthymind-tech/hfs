//! The SSRF guard for server-side fetches (`http_guard`), over real sockets.

use helios_endpoints::OutboundPolicy;
use helios_endpoints::http_guard::{check, guard, is_refusal};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A one-route HTTP server on 127.0.0.1: answers every request with
/// `response` (a full HTTP/1.1 response head + body).
async fn server(response: String) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let response = response.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s.write_all(response.as_bytes()).await;
                let _ = s.shutdown().await;
            });
        }
    });
    port
}

fn ok_body() -> String {
    "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok".to_string()
}

fn redirect_to(location: &str) -> String {
    format!(
        "HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    )
}

fn client(policy: &OutboundPolicy) -> reqwest::Client {
    guard(reqwest::Client::builder(), policy).build().unwrap()
}

#[tokio::test]
async fn an_ip_literal_to_loopback_is_refused_before_connecting() {
    let port = server(ok_body()).await;
    let strict = OutboundPolicy::strict();
    let url = format!("http://127.0.0.1:{port}/manifest.json");
    assert!(check(&strict, &url).is_err());
    // The metadata service, by address and by name.
    assert!(check(&strict, "http://169.254.169.254/latest/meta-data/").is_err());
    assert!(check(&strict, "http://metadata.google.internal/").is_err());
    // A public https URL passes the first check; so does plain http (server
    // fetches are judged by destination, not transport).
    assert!(check(&strict, "https://example.org/x").is_ok());
    assert!(check(&strict, "http://example.org/x").is_ok());
    assert!(check(&strict, "file:///etc/passwd").is_err());
}

#[tokio::test]
async fn a_name_resolving_to_an_internal_address_is_refused_at_connect_time() {
    // `localhost` stands for any name whose DNS answer is internal (a
    // rebinding name included): the resolver checks the answer it connects to.
    let port = server(ok_body()).await;
    let url = format!("http://localhost:{port}/");
    assert!(
        check(&OutboundPolicy::strict(), &url).is_ok(),
        "names pass the syntactic check"
    );
    let err = client(&OutboundPolicy::strict())
        .get(&url)
        .send()
        .await
        .unwrap_err();
    assert!(is_refusal(&err), "{err:?}");
    // Allow-listed, the same request goes through.
    let allowed = OutboundPolicy::parse("localhost");
    let resp = client(&allowed).get(&url).send().await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn a_redirect_to_an_internal_address_is_refused() {
    let inner = server(ok_body()).await;
    // An allow-listed host that redirects to an address that is not.
    let outer = server(redirect_to(&format!("http://127.0.0.2:{inner}/secret"))).await;
    let policy = OutboundPolicy::parse("localhost");
    let err = client(&policy)
        .get(format!("http://localhost:{outer}/start"))
        .send()
        .await
        .unwrap_err();
    assert!(is_refusal(&err), "{err:?}");
    // To the metadata service.
    let meta = server(redirect_to("http://169.254.169.254/latest/meta-data/")).await;
    let err = client(&policy)
        .get(format!("http://localhost:{meta}/"))
        .send()
        .await
        .unwrap_err();
    assert!(is_refusal(&err), "{err:?}");
    // A redirect inside the allow list is followed.
    let fine = server(redirect_to(&format!("http://localhost:{inner}/ok"))).await;
    let resp = client(&policy)
        .get(format!("http://localhost:{fine}/"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn an_allow_listed_cidr_admits_its_addresses_only() {
    let port = server(ok_body()).await;
    let policy = OutboundPolicy::parse("127.0.0.1/32");
    assert!(check(&policy, &format!("http://127.0.0.1:{port}/")).is_ok());
    assert!(check(&policy, &format!("http://127.0.0.2:{port}/")).is_err());
    assert!(check(&policy, "http://10.0.0.1/").is_err());
}
