//! SSRF guard for `reqwest` clients that fetch URLs the server did not choose
//! itself (feature `http-guard`).
//!
//! [`guard`] wraps a [`reqwest::ClientBuilder`] so that, under an
//! [`OutboundPolicy`]:
//!
//! * every host **name** is resolved by [`GuardedResolver`], which refuses the
//!   connection when any address it resolves to is internal (loopback,
//!   private, link-local / cloud metadata, …) and not allow-listed. The
//!   connection is made to the addresses that were checked, at connect time,
//!   so a name that answered a public address to an earlier check and an
//!   internal one now (DNS rebinding) is refused when it matters;
//! * every redirect hop is checked with
//!   [`OutboundPolicy::check_fetch_url`] (an IP-literal `Location` never
//!   reaches the resolver), at most 10 hops.
//!
//! The first URL must still be checked by the caller with
//! [`OutboundPolicy::check_fetch_url`] — [`check`] does it — because an
//! IP-literal URL is connected to without resolution.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use crate::outbound::{OutboundDenied, OutboundPolicy};

/// Maximum redirect hops followed by a guarded client.
pub const MAX_REDIRECTS: usize = 10;

/// A resolver that refuses names resolving to refused addresses.
#[derive(Debug, Clone)]
pub struct GuardedResolver {
    policy: OutboundPolicy,
}

impl GuardedResolver {
    /// A resolver under `policy`.
    pub fn new(policy: OutboundPolicy) -> Self {
        Self { policy }
    }
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let policy = self.policy.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if addrs.is_empty() {
                return Err(format!("{host} resolves to no address").into());
            }
            for a in &addrs {
                policy.check_ip(&host, a.ip())?;
            }
            let addrs: Addrs = Box::new(addrs.into_iter());
            Ok(addrs)
        })
    }
}

/// `builder` under `policy`: the guarded resolver and a redirect policy that
/// checks each hop. Callers must not replace either afterwards (setting
/// `redirect(Policy::none())` is fine).
pub fn guard(builder: reqwest::ClientBuilder, policy: &OutboundPolicy) -> reqwest::ClientBuilder {
    let hops = policy.clone();
    builder
        .dns_resolver(Arc::new(GuardedResolver::new(policy.clone())))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                return attempt.error("too many redirects");
            }
            match hops.check_fetch_url(attempt.url().as_str()) {
                Ok(_) => attempt.follow(),
                Err(e) => attempt.error(e),
            }
        }))
}

/// The first-URL check every guarded fetch starts with.
pub fn check(policy: &OutboundPolicy, url: &str) -> Result<url::Url, OutboundDenied> {
    policy.check_fetch_url(url)
}

/// Whether a `reqwest` error was a refusal by the guard (resolver or
/// redirect policy), for messages that tell the operator what to change.
pub fn is_refusal(e: &reqwest::Error) -> bool {
    let mut src: Option<&dyn std::error::Error> = Some(e);
    while let Some(s) = src {
        if s.downcast_ref::<OutboundDenied>().is_some() {
            return true;
        }
        src = s.source();
    }
    false
}
