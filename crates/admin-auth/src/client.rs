//! Who is asking: the client address the sign-in rate limit is keyed on.
//!
//! The TCP peer is the client unless it is a trusted proxy
//! (`<PREFIX>_ADMIN_TRUSTED_PROXIES`, default loopback and private ranges —
//! a reverse proxy in the same host or container network). Behind a trusted
//! proxy the client is the right-most `X-Forwarded-For` entry that is not
//! itself a trusted proxy (entries further left are whatever the client
//! claimed and can be forged), falling back to `X-Real-IP`, then the peer.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use axum::http::HeaderMap;

/// An IP network in CIDR notation (`10.0.0.0/8`, `::1/128`, or a bare
/// address meaning a single host).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl IpNet {
    /// Parses `addr[/prefix]`.
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        let (addr, prefix) = match raw.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (raw, None),
        };
        let addr: IpAddr = addr.trim().parse().ok()?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p.trim().parse::<u8>().ok().filter(|p| *p <= max)?,
            None => max,
        };
        Some(Self { addr, prefix })
    }

    /// Whether `ip` lies inside this network. IPv4-mapped IPv6 addresses are
    /// compared as IPv4.
    pub fn contains(&self, ip: &IpAddr) -> bool {
        match (self.addr, canonical(*ip)) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = mask_u32(self.prefix);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = mask_u128(self.prefix);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }

    /// Loopback, RFC 1918, link-local, and IPv6 unique-local/link-local:
    /// where a reverse proxy in front of the server normally sits.
    pub fn default_trusted() -> Vec<Self> {
        [
            "127.0.0.0/8",
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "169.254.0.0/16",
            "::1/128",
            "fc00::/7",
            "fe80::/10",
        ]
        .iter()
        .filter_map(|s| Self::parse(s))
        .collect()
    }
}

fn mask_u32(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    }
}

fn mask_u128(prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - u32::from(prefix))
    }
}

/// `::ffff:a.b.c.d` → `a.b.c.d`.
pub(crate) fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

/// The key used when the peer address is unknown (requests that did not come
/// through a socket, e.g. in-process tests).
pub(crate) const UNKNOWN_PEER: IpAddr = IpAddr::V6(Ipv6Addr::UNSPECIFIED);

/// Resolves the client address of a request (see the module docs).
pub(crate) fn client_ip(headers: &HeaderMap, peer: Option<IpAddr>, trusted: &[IpNet]) -> IpAddr {
    let Some(peer) = peer.map(canonical) else {
        return UNKNOWN_PEER;
    };
    if !crate::settings::trusted(trusted, &peer) {
        return peer;
    }
    let forwarded: Vec<IpAddr> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(parse_forwarded_entry)
        .collect();
    if let Some(ip) = forwarded
        .iter()
        .rev()
        .copied()
        .find(|ip| !crate::settings::trusted(trusted, ip))
    {
        return ip;
    }
    if let Some(ip) = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_forwarded_entry)
    {
        return ip;
    }
    // Every hop is a trusted proxy: the left-most one is as close to the
    // client as the chain lets us see.
    forwarded.first().copied().unwrap_or(peer)
}

/// One `X-Forwarded-For` element: `1.2.3.4`, `1.2.3.4:5678`, `[::1]:80`, `::1`.
fn parse_forwarded_entry(raw: &str) -> Option<IpAddr> {
    let raw = raw.trim();
    if let Ok(ip) = raw.parse::<IpAddr>() {
        return Some(canonical(ip));
    }
    if let Some(rest) = raw.strip_prefix('[') {
        return rest
            .split(']')
            .next()?
            .parse::<Ipv6Addr>()
            .ok()
            .map(|ip| canonical(IpAddr::V6(ip)));
    }
    let (host, _port) = raw.rsplit_once(':')?;
    host.parse::<Ipv4Addr>().ok().map(IpAddr::V4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn cidr_membership() {
        let net = IpNet::parse("172.16.0.0/12").unwrap();
        assert!(net.contains(&ip("172.20.1.2")));
        assert!(!net.contains(&ip("172.32.0.1")));
        assert!(net.contains(&ip("::ffff:172.20.1.2")));
        let host = IpNet::parse("203.0.113.7").unwrap();
        assert!(host.contains(&ip("203.0.113.7")));
        assert!(!host.contains(&ip("203.0.113.8")));
        assert!(IpNet::parse("10.0.0.0/33").is_none());
        assert!(IpNet::parse("nope").is_none());
    }

    #[test]
    fn untrusted_peer_is_the_client_whatever_the_headers_say() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_static("1.1.1.1"));
        h.insert("x-real-ip", HeaderValue::from_static("1.1.1.1"));
        let got = client_ip(&h, Some(ip("203.0.113.9")), &IpNet::default_trusted());
        assert_eq!(got, ip("203.0.113.9"));
    }

    #[test]
    fn behind_a_trusted_proxy_the_rightmost_untrusted_hop_wins() {
        let mut h = HeaderMap::new();
        // Client forged "6.6.6.6"; the edge proxy appended the real client,
        // nginx (trusted, private) appended the edge's private address.
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("6.6.6.6, 198.51.100.4, 10.0.0.2"),
        );
        let got = client_ip(&h, Some(ip("172.18.0.5")), &IpNet::default_trusted());
        assert_eq!(got, ip("198.51.100.4"));
    }

    #[test]
    fn behind_a_trusted_proxy_x_real_ip_is_the_fallback() {
        let mut h = HeaderMap::new();
        h.insert("x-real-ip", HeaderValue::from_static("198.51.100.4"));
        let got = client_ip(&h, Some(ip("127.0.0.1")), &IpNet::default_trusted());
        assert_eq!(got, ip("198.51.100.4"));
        let none = client_ip(&HeaderMap::new(), Some(ip("127.0.0.1")), &[]);
        assert_eq!(none, ip("127.0.0.1"));
        assert_eq!(client_ip(&h, None, &IpNet::default_trusted()), UNKNOWN_PEER);
    }

    #[test]
    fn forwarded_entries_with_ports_parse() {
        assert_eq!(parse_forwarded_entry("1.2.3.4:55"), Some(ip("1.2.3.4")));
        assert_eq!(
            parse_forwarded_entry("[2001:db8::1]:80"),
            Some(ip("2001:db8::1"))
        );
        assert_eq!(
            parse_forwarded_entry("2001:db8::1"),
            Some(ip("2001:db8::1"))
        );
        assert_eq!(parse_forwarded_entry("unknown"), None);
    }
}
