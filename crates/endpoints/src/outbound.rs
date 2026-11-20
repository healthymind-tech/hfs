//! Outbound URL policy (SSRF guard) for URLs an administrator types into the
//! console: external sync servers, token endpoints, per-endpoint issuers and
//! JWKS URLs, terminology servers, OIDC discovery.
//!
//! Two checks, both required before every connection:
//!
//! 1. [`OutboundPolicy::check_url`] — syntax: `https` (or `http` only for an
//!    allow-listed host), no user-info, host present, not a known cloud
//!    metadata name.
//! 2. [`OutboundPolicy::check_ip`] — every address the host resolves to,
//!    **after** DNS resolution, with the connection then pinned to the
//!    checked address (reqwest `resolve`), and re-checked on each redirect
//!    hop (or redirects off). Loopback, private, link-local (incl.
//!    169.254.169.254), CGNAT, ULA, multicast, unspecified and documentation
//!    ranges are refused unless allow-listed.
//!
//! The allow list comes only from the environment (`HFS_OUTBOUND_ALLOW` =
//! comma list of host names, `*.suffix` patterns and CIDRs), never from the
//! console, so a compromised admin session cannot open the internal network.
//! URLs configured through the environment (`HFS_AUTH_JWKS_URL=http://keycloak:8080/…`)
//! are operator-trusted and not subject to this policy.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Environment variable with the allow list.
pub const ALLOW_ENV: &str = "HFS_OUTBOUND_ALLOW";

/// Host names that are cloud metadata services whatever they resolve to.
const METADATA_HOSTS: &[&str] = &[
    "metadata.google.internal",
    "metadata",
    "metadata.goog",
    "instance-data",
    "instance-data.ec2.internal",
];

/// Why a URL or address was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OutboundDenied {
    /// Not a parseable absolute URL.
    #[error("not an absolute URL")]
    Malformed,
    /// Scheme other than https (http only when allow-listed).
    #[error("scheme {0:?} is not allowed (use https)")]
    Scheme(String),
    /// `user:pass@` in the URL.
    #[error("credentials in the URL are not allowed")]
    UserInfo,
    /// A cloud metadata host name.
    #[error("host {0:?} is a cloud metadata service")]
    Metadata(String),
    /// An address in a refused range.
    #[error("address {0} is internal ({1}); add it to {ALLOW_ENV} to allow it")]
    Internal(IpAddr, &'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AllowEntry {
    Host(String),
    Suffix(String),
    Net(IpAddr, u8),
}

/// The policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboundPolicy {
    allow: Vec<AllowEntry>,
}

impl OutboundPolicy {
    /// No allow-list entries: public https only.
    pub fn strict() -> Self {
        Self::default()
    }

    /// Parses an allow list (`keycloak, *.hospital.local, 10.20.0.0/16`).
    /// Unparseable entries are skipped with a warning.
    pub fn parse(spec: &str) -> Self {
        let mut allow = Vec::new();
        for raw in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if let Some((ip, len)) = raw.split_once('/') {
                match (ip.parse::<IpAddr>(), len.parse::<u8>()) {
                    (Ok(ip), Ok(len)) if len <= if ip.is_ipv4() { 32 } else { 128 } => {
                        allow.push(AllowEntry::Net(ip, len))
                    }
                    _ => tracing::warn!(entry = raw, "{ALLOW_ENV}: ignoring bad CIDR"),
                }
            } else if let Ok(ip) = raw.parse::<IpAddr>() {
                allow.push(AllowEntry::Net(ip, if ip.is_ipv4() { 32 } else { 128 }));
            } else if let Some(sfx) = raw.strip_prefix("*.") {
                allow.push(AllowEntry::Suffix(format!(".{}", sfx.to_ascii_lowercase())));
            } else {
                allow.push(AllowEntry::Host(raw.to_ascii_lowercase()));
            }
        }
        Self { allow }
    }

    /// From [`ALLOW_ENV`].
    pub fn from_env() -> Self {
        Self::parse(&std::env::var(ALLOW_ENV).unwrap_or_default())
    }

    fn host_allowed(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.allow.iter().any(|a| match a {
            AllowEntry::Host(h) => *h == host,
            AllowEntry::Suffix(s) => host.ends_with(s.as_str()),
            AllowEntry::Net(..) => false,
        })
    }

    fn ip_allowed(&self, ip: IpAddr) -> bool {
        self.allow.iter().any(|a| match a {
            AllowEntry::Net(net, len) => in_net(ip, *net, *len),
            _ => false,
        })
    }

    /// Syntactic check. Returns the parsed URL.
    pub fn check_url(&self, raw: &str) -> Result<url::Url, OutboundDenied> {
        let u = url::Url::parse(raw).map_err(|_| OutboundDenied::Malformed)?;
        let host = u.host_str().ok_or(OutboundDenied::Malformed)?.to_string();
        if !u.username().is_empty() || u.password().is_some() {
            return Err(OutboundDenied::UserInfo);
        }
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        let listed =
            self.host_allowed(bare) || bare.parse::<IpAddr>().is_ok_and(|ip| self.ip_allowed(ip));
        match u.scheme() {
            "https" => {}
            "http" if listed => {}
            other => return Err(OutboundDenied::Scheme(other.to_string())),
        }
        if METADATA_HOSTS.contains(&bare.to_ascii_lowercase().as_str()) && !listed {
            return Err(OutboundDenied::Metadata(bare.to_string()));
        }
        if let Ok(ip) = bare.parse::<IpAddr>() {
            self.check_ip(bare, ip)?;
        }
        Ok(u)
    }

    /// Checks one resolved address of `host`. An allow-listed host name
    /// admits whatever it resolves to; otherwise the address must be public
    /// or inside an allow-listed CIDR.
    pub fn check_ip(&self, host: &str, ip: IpAddr) -> Result<(), OutboundDenied> {
        if self.host_allowed(host) || self.ip_allowed(ip) {
            return Ok(());
        }
        match internal_range(ip) {
            Some(why) => Err(OutboundDenied::Internal(ip, why)),
            None => Ok(()),
        }
    }
}

fn in_net(ip: IpAddr, net: IpAddr, len: u8) -> bool {
    match (canonical(ip), canonical(net)) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let mask = if len == 0 {
                0
            } else {
                u32::MAX << (32 - len.min(32))
            };
            u32::from(a) & mask == u32::from(b) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let mask = if len == 0 {
                0
            } else {
                u128::MAX << (128 - len.min(128))
            };
            u128::from(a) & mask == u128::from(b) & mask
        }
        _ => false,
    }
}

/// IPv4-mapped / -compatible IPv6 addresses are judged as IPv4.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => ip,
        },
        v4 => v4,
    }
}

/// The refused range an address falls in, if any.
pub fn internal_range(ip: IpAddr) -> Option<&'static str> {
    match canonical(ip) {
        IpAddr::V4(v4) => internal_v4(v4),
        IpAddr::V6(v6) => internal_v6(v6),
    }
}

fn internal_v4(ip: Ipv4Addr) -> Option<&'static str> {
    let o = ip.octets();
    if ip.is_loopback() {
        Some("loopback")
    } else if ip.is_private() {
        Some("private")
    } else if ip.is_link_local() {
        Some("link-local / cloud metadata")
    } else if ip.is_unspecified() || o[0] == 0 {
        Some("unspecified")
    } else if ip.is_broadcast() || ip.is_multicast() || o[0] >= 240 {
        Some("multicast / reserved")
    } else if o[0] == 100 && (o[1] & 0xC0) == 64 {
        Some("carrier-grade NAT")
    } else if ip.is_documentation() || (o[0] == 198 && (o[1] & 0xFE) == 18) {
        Some("documentation / benchmarking")
    } else if o[0] == 192 && o[1] == 0 && o[2] == 0 {
        Some("IETF protocol assignments")
    } else {
        None
    }
}

fn internal_v6(ip: Ipv6Addr) -> Option<&'static str> {
    let s = ip.segments();
    if ip.is_loopback() {
        Some("loopback")
    } else if ip.is_unspecified() {
        Some("unspecified")
    } else if ip.is_multicast() {
        Some("multicast")
    } else if (s[0] & 0xFFC0) == 0xFE80 {
        Some("link-local")
    } else if (s[0] & 0xFE00) == 0xFC00 {
        Some("unique local (incl. fd00:ec2::254 metadata)")
    } else if s[0] == 0x2001 && s[1] == 0x0DB8 {
        Some("documentation")
    } else if s[0] == 0x0064 && s[1] == 0xFF9B {
        Some("NAT64 (could reach IPv4 internals)")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn strict_policy() {
        let p = OutboundPolicy::strict();
        assert!(p.check_url("https://hapi.fhir.org/baseR4").is_ok());
        assert_eq!(
            p.check_url("http://hapi.fhir.org/baseR4").unwrap_err(),
            OutboundDenied::Scheme("http".into())
        );
        assert_eq!(
            p.check_url("https://u:p@x.org/").unwrap_err(),
            OutboundDenied::UserInfo
        );
        assert!(matches!(
            p.check_url("https://metadata.google.internal/computeMetadata/v1/"),
            Err(OutboundDenied::Metadata(_))
        ));
        assert!(matches!(
            p.check_url("https://169.254.169.254/latest"),
            Err(OutboundDenied::Internal(..))
        ));
        assert!(matches!(
            p.check_url("https://[::1]/"),
            Err(OutboundDenied::Internal(..))
        ));
        assert!(matches!(
            p.check_url("https://[::ffff:10.0.0.1]/"),
            Err(OutboundDenied::Internal(..))
        ));
        assert!(p.check_url("file:///etc/passwd").is_err());
        assert!(p.check_url("not a url").is_err());
    }

    #[test]
    fn resolved_addresses() {
        let p = OutboundPolicy::strict();
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "172.20.0.5",
            "192.168.1.1",
            "169.254.169.254",
            "100.100.100.200",
            "0.0.0.0",
            "224.0.0.1",
            "::1",
            "fe80::1",
            "fd00:ec2::254",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
        ] {
            assert!(p.check_ip("evil.example", ip(bad)).is_err(), "{bad}");
        }
        for good in ["8.8.8.8", "162.38.2.210", "2606:4700::1111"] {
            assert!(p.check_ip("x.example", ip(good)).is_ok(), "{good}");
        }
    }

    #[test]
    fn allow_list() {
        let p = OutboundPolicy::parse("keycloak, *.hospital.local, 10.20.0.0/16, bad/99");
        assert!(p.check_url("http://keycloak:8080/realms/x/certs").is_ok());
        assert!(p.check_ip("keycloak", ip("172.18.0.4")).is_ok());
        assert!(p.check_url("https://his.hospital.local/fhir").is_ok());
        assert!(p.check_ip("his.hospital.local", ip("10.9.9.9")).is_ok());
        assert!(p.check_ip("other.example", ip("10.20.3.4")).is_ok());
        assert!(p.check_ip("other.example", ip("10.21.3.4")).is_err());
        assert!(p.check_url("http://other.example/").is_err());
        assert!(p.check_url("http://10.20.1.1/").is_ok());
    }
}
