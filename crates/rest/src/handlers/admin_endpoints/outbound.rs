//! SSRF checks of the URLs an administrator types into an endpoint spec
//! (design D-13).
//!
//! The server itself connects to `auth.issuer` (discovery), `auth.jwksUrl`
//! and `terminology.serverUrl`, so a changed value of any of them must pass
//! the [`OutboundPolicy`]: `https` only (`http` for allow-listed hosts), no
//! credentials, no cloud metadata names, and — after DNS resolution — no
//! loopback / private / link-local / CGNAT / ULA address unless allow-listed
//! (`HFS_OUTBOUND_ALLOW`, env only). Values that did not change are not
//! re-checked, so tightening the allow list never blocks unrelated edits of an
//! endpoint, and env-configured URLs (inherited sections) are operator-trusted.
//!
//! This is the write-time check. The connection-time check (pinning the
//! connection to the checked address, re-checking redirects) belongs to the
//! clients that connect (P2's verifier cache and probe, the terminology client).
//! A host that does not resolve at write time is accepted with a warning:
//! a connection to it fails anyway, and the connection-time check still runs.

use std::net::IpAddr;

use async_trait::async_trait;
use helios_endpoints::OutboundPolicy;
use helios_endpoints::model::EndpointSpec;

use super::error::{ApiError, ApiResult, Issue, outbound_refused};

/// Resolves a host name to its addresses (injectable for tests).
#[async_trait]
pub trait HostResolver: Send + Sync {
    /// Addresses of `host:port`, or an error message.
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, String>;
}

/// The system resolver (`tokio::net::lookup_host`).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemResolver;

#[async_trait]
impl HostResolver for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, String> {
        let lookup = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::net::lookup_host((host, port)),
        )
        .await
        .map_err(|_| "DNS lookup timed out".to_string())?
        .map_err(|e| e.to_string())?;
        Ok(lookup.map(|a| a.ip()).collect())
    }
}

/// The fetched URLs of a spec, by field path.
pub fn fetched_urls(spec: &EndpointSpec) -> Vec<(&'static str, &str)> {
    let mut out = Vec::new();
    if let Some(a) = &spec.auth {
        if let Some(u) = a.issuer.as_deref() {
            out.push(("auth.issuer", u));
        }
        if let Some(u) = a.jwks_url.as_deref() {
            out.push(("auth.jwksUrl", u));
        }
    }
    if let Some(u) = spec
        .terminology
        .as_ref()
        .and_then(|t| t.server_url.as_deref())
    {
        out.push(("terminology.serverUrl", u));
    }
    out
}

/// Checks one URL. `Ok(Some(warning))` when it passed without resolving.
pub async fn check_one(
    policy: &OutboundPolicy,
    resolver: &dyn HostResolver,
    field: &str,
    raw: &str,
) -> Result<Option<String>, Issue> {
    let url = policy
        .check_url(raw.trim())
        .map_err(|e| outbound_refused(field, &e))?;
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    if host.parse::<IpAddr>().is_ok() {
        // Literal addresses were checked by `check_url`.
        return Ok(None);
    }
    let port = url.port_or_known_default().unwrap_or(443);
    match resolver.resolve(&host, port).await {
        Ok(addrs) => {
            for ip in addrs {
                policy
                    .check_ip(&host, ip)
                    .map_err(|e| outbound_refused(field, &e))?;
            }
            Ok(None)
        }
        Err(e) => Ok(Some(format!(
            "{field}: host {host} did not resolve ({e}); it is checked again when the server connects"
        ))),
    }
}

/// Checks every fetched URL of `next` that differs from `previous`.
/// Returns warnings for hosts that did not resolve.
pub async fn check_spec(
    policy: &OutboundPolicy,
    resolver: &dyn HostResolver,
    next: &EndpointSpec,
    previous: Option<&EndpointSpec>,
) -> ApiResult<Vec<String>> {
    let before = previous.map(fetched_urls).unwrap_or_default();
    let mut issues = Vec::new();
    let mut warnings = Vec::new();
    for (field, url) in fetched_urls(next) {
        if before.iter().any(|(f, u)| *f == field && *u == url) {
            continue;
        }
        match check_one(policy, resolver, field, url).await {
            Ok(Some(w)) => warnings.push(w),
            Ok(None) => {}
            Err(issue) => issues.push(issue),
        }
    }
    if issues.is_empty() {
        Ok(warnings)
    } else {
        Err(ApiError::unprocessable(issues))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use helios_endpoints::EndpointName;
    use helios_endpoints::model::{AuthMode, EndpointAuth, TerminologyBinding};
    use std::collections::HashMap;

    /// Fixed answers; unknown hosts fail to resolve.
    #[derive(Default)]
    pub struct FakeResolver(pub HashMap<String, Vec<IpAddr>>);

    #[async_trait]
    impl HostResolver for FakeResolver {
        async fn resolve(&self, host: &str, _port: u16) -> Result<Vec<IpAddr>, String> {
            self.0
                .get(host)
                .cloned()
                .ok_or_else(|| "NXDOMAIN".to_string())
        }
    }

    fn resolver() -> FakeResolver {
        let mut m = HashMap::new();
        m.insert(
            "idp.example.org".to_string(),
            vec!["93.184.216.34".parse().unwrap()],
        );
        m.insert(
            "evil.example.org".to_string(),
            vec!["10.0.0.5".parse().unwrap()],
        );
        m.insert("keycloak".to_string(), vec!["172.18.0.4".parse().unwrap()]);
        FakeResolver(m)
    }

    fn spec(issuer: &str, jwks: &str, tx: Option<&str>) -> EndpointSpec {
        let mut s = EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab");
        s.auth = Some(EndpointAuth {
            mode: AuthMode::Full,
            issuer: Some(issuer.into()),
            jwks_url: Some(jwks.into()),
            ..Default::default()
        });
        s.terminology = tx.map(|u| TerminologyBinding {
            server_url: Some(u.into()),
            ..Default::default()
        });
        s
    }

    #[tokio::test]
    async fn public_https_passes_internal_is_refused() {
        let p = OutboundPolicy::strict();
        let r = resolver();
        let ok = spec(
            "https://idp.example.org/realms/a",
            "https://idp.example.org/certs",
            None,
        );
        assert!(check_spec(&p, &r, &ok, None).await.unwrap().is_empty());

        let bad = spec(
            "https://evil.example.org/realms/a",
            "http://idp.example.org/certs",
            Some("https://169.254.169.254/latest"),
        );
        let e = check_spec(&p, &r, &bad, None).await.unwrap_err();
        let fields: Vec<_> = e.issues.iter().filter_map(|i| i.field.as_deref()).collect();
        assert_eq!(
            fields,
            vec!["auth.issuer", "auth.jwksUrl", "terminology.serverUrl"]
        );
        assert!(e.issues.iter().all(|i| i.code == "security"));
    }

    #[tokio::test]
    async fn allow_list_admits_internal_host_and_metadata_is_refused() {
        let p = OutboundPolicy::parse("keycloak");
        let r = resolver();
        let s = spec(
            "http://keycloak:8080/realms/a",
            "http://keycloak:8080/certs",
            None,
        );
        assert!(check_spec(&p, &r, &s, None).await.is_ok());
        let s = spec(
            "https://metadata.google.internal/x",
            "https://idp.example.org/c",
            None,
        );
        let e = check_spec(&OutboundPolicy::strict(), &r, &s, None)
            .await
            .unwrap_err();
        assert_eq!(e.issues.len(), 1);
    }

    #[tokio::test]
    async fn unchanged_urls_are_not_rechecked_and_unresolvable_warns() {
        let p = OutboundPolicy::strict();
        let r = resolver();
        let s = spec(
            "http://keycloak:8080/realms/a",
            "http://keycloak:8080/certs",
            None,
        );
        // Previously stored (e.g. allowed before, or from the env file): no re-check.
        assert!(check_spec(&p, &r, &s, Some(&s)).await.unwrap().is_empty());
        let s2 = spec(
            "https://nowhere.invalid/realms/a",
            "https://idp.example.org/certs",
            None,
        );
        let w = check_spec(&p, &r, &s2, None).await.unwrap();
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("did not resolve"));
    }

    #[tokio::test]
    async fn system_resolver_resolves_localhost() {
        let addrs = SystemResolver.resolve("localhost", 80).await.unwrap();
        assert!(!addrs.is_empty());
        let w = check_one(
            &OutboundPolicy::strict(),
            &SystemResolver,
            "f",
            "https://localhost/x",
        )
        .await
        .unwrap_err();
        assert_eq!(w.code, "security");
    }
}
