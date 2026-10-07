//! Which webhook addresses a deployment lets push notifications reach, and the address checks
//! that keep a webhook from being a way into the deployment's own network (SSRF).
//!
//! Two layers, both fail closed:
//!
//! * **At create time** [`PushPolicy::check`] parses the URL and refuses what the deployment did
//!   not allow: a scheme other than `https` (`http` only for a loopback host, and only when private
//!   addresses are allowed), a URL with credentials in it, a URL that matches no entry of the
//!   allow-list, and, without private addresses, a host that is an IP literal in a refused range
//!   or is `localhost`. The client gets `InvalidParams`.
//! * **At delivery time** [`GuardedResolver`] resolves the host itself and drops every address in
//!   a refused range, so a name that resolves to `10.0.0.5` (or that resolves there only after
//!   it was created) is never connected to. The connection is made to the addresses that passed,
//!   so there is no gap between the check and the connect. The delivery client also never follows a
//!   redirect (see [`crate::push::PushSender`]).
//!
//! Refused ranges: loopback, private (RFC 1918), link-local (which holds the cloud metadata
//! address `169.254.169.254`), unspecified, multicast, broadcast, carrier-grade NAT, the
//! documentation and benchmarking ranges, the reserved `240.0.0.0/4`, IPv6 unique-local
//! (`fc00::/7`), and an IPv4 address embedded in an IPv4-mapped or NAT64 IPv6 address.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::{Host, Url};

/// The longest webhook URL accepted.
pub const MAX_URL_LEN: usize = 2048;

/// Why a webhook URL was refused. The text is safe to show the client: it names the rule, never
/// the allow-list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused(pub(crate) &'static str);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Refused {}

/// An entry of the allow-list that cannot be read (an operator mistake, found at startup).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid push webhook allow-list entry {entry:?}: {reason}")]
pub struct PolicyEntryError {
    entry: String,
    reason: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Allowed {
    /// `https://hooks.example.com/a2a/`: scheme, host, port and a path prefix at a segment
    /// boundary.
    Prefix {
        scheme: String,
        host: String,
        port: u16,
        path: String,
    },
    /// `hooks.example.com`, `hooks.example.com:8443` or `*.example.com`: any path of that host.
    Host {
        host: String,
        wildcard: bool,
        port: Option<u16>,
    },
}

/// What the deployment allows push notifications to reach.
///
/// Empty by default, and an empty policy turns push notifications **off**: the card says
/// `pushNotifications: false` and the four push methods answer `PushNotificationNotSupported`.
#[derive(Clone, Debug, Default)]
pub struct PushPolicy {
    allowed: Vec<Allowed>,
    allow_private: bool,
}

impl PushPolicy {
    /// A policy that allows the given webhooks: each entry is a **URL prefix**
    /// (`https://hooks.example.com/a2a/`, matched on scheme, host, port and path segments) or a
    /// **host** (`hooks.example.com`, `hooks.example.com:8443`, `*.example.com` for any
    /// subdomain), which allows every path of it over `https`.
    ///
    /// # Errors
    ///
    /// [`PolicyEntryError`] for an entry that is empty, has credentials, a query or a fragment, or
    /// that cannot be read.
    pub fn new<I, S>(entries: I) -> Result<Self, PolicyEntryError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let allowed = entries
            .into_iter()
            .map(|e| parse_entry(e.as_ref().trim()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            allowed,
            allow_private: false,
        })
    }

    /// Also allow webhooks on loopback, private and link-local addresses, and `http` for a
    /// loopback host. For local development and tests; off by default.
    #[must_use]
    pub fn allow_private_addresses(mut self, allow: bool) -> Self {
        self.allow_private = allow;
        self
    }

    /// Whether the deployment allows any webhook at all (the allow-list is not empty).
    pub fn is_enabled(&self) -> bool {
        !self.allowed.is_empty()
    }

    /// Whether private addresses are allowed.
    pub fn allows_private_addresses(&self) -> bool {
        self.allow_private
    }

    /// Check `url` against the policy and return it parsed.
    ///
    /// # Errors
    ///
    /// [`Refused`], with the rule that was broken.
    pub fn check(&self, url: &str) -> Result<Url, Refused> {
        if url.len() > MAX_URL_LEN {
            return Err(Refused("the webhook URL is too long"));
        }
        let parsed = Url::parse(url).map_err(|_| Refused("the webhook URL is not a valid URL"))?;
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(Refused("the webhook URL must not carry credentials"));
        }
        let host = parsed
            .host()
            .ok_or(Refused("the webhook URL has no host"))?;
        let loopback = match &host {
            Host::Domain(name) => is_localhost_name(name),
            Host::Ipv4(ip) => ip.is_loopback(),
            Host::Ipv6(ip) => ip.is_loopback(),
        };
        match parsed.scheme() {
            "https" => {}
            "http" if self.allow_private && loopback => {}
            _ => return Err(Refused("the webhook URL must use https")),
        }
        if !self.allow_private {
            match &host {
                Host::Domain(name) if is_localhost_name(name) => {
                    return Err(Refused("the webhook address is not allowed"));
                }
                Host::Ipv4(ip) if is_refused_ip(IpAddr::V4(*ip)) => {
                    return Err(Refused("the webhook address is not allowed"));
                }
                Host::Ipv6(ip) if is_refused_ip(IpAddr::V6(*ip)) => {
                    return Err(Refused("the webhook address is not allowed"));
                }
                _ => {}
            }
        }
        if !self.allowed.iter().any(|a| a.matches(&parsed)) {
            return Err(Refused("the webhook URL is not on the allow-list"));
        }
        Ok(parsed)
    }
}

fn is_localhost_name(name: &str) -> bool {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    name == "localhost" || name.ends_with(".localhost")
}

fn parse_entry(entry: &str) -> Result<Allowed, PolicyEntryError> {
    let err = |reason| PolicyEntryError {
        entry: entry.to_owned(),
        reason,
    };
    if entry.is_empty() {
        return Err(err("empty"));
    }
    if entry.contains("://") {
        let url = Url::parse(entry).map_err(|_| err("not a valid URL"))?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(err("must not carry credentials"));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(err("must not carry a query or a fragment"));
        }
        if !matches!(url.scheme(), "https" | "http") {
            return Err(err(
                "the scheme must be https (or http for a local webhook)",
            ));
        }
        let host = url.host_str().ok_or_else(|| err("has no host"))?.to_owned();
        let port = url
            .port_or_known_default()
            .ok_or_else(|| err("has no port"))?;
        return Ok(Allowed::Prefix {
            scheme: url.scheme().to_owned(),
            host,
            port,
            path: url.path().to_owned(),
        });
    }
    if entry.contains(['/', '?', '#', '@', ' ']) {
        return Err(err("a host entry is a host name, with an optional port"));
    }
    let (name, port) = match entry.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') => (
            name,
            Some(port.parse::<u16>().map_err(|_| err("bad port"))?),
        ),
        _ => (entry, None),
    };
    let (wildcard, name) = match name.strip_prefix("*.") {
        Some(rest) => (true, rest),
        None => (false, name),
    };
    if name.is_empty() || name.contains('*') {
        return Err(err("a wildcard is only allowed as a leading `*.`"));
    }
    // Parse through `Url` so the host is normalised exactly as the URL's host will be.
    let probe = Url::parse(&format!("https://{name}/")).map_err(|_| err("not a valid host"))?;
    let host = probe
        .host_str()
        .ok_or_else(|| err("not a valid host"))?
        .to_owned();
    Ok(Allowed::Host {
        host,
        wildcard,
        port,
    })
}

impl Allowed {
    fn matches(&self, url: &Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        match self {
            Self::Prefix {
                scheme,
                host: h,
                port,
                path,
            } => {
                url.scheme() == scheme
                    && host == h
                    && url.port_or_known_default() == Some(*port)
                    && path_has_prefix(url.path(), path)
            }
            Self::Host {
                host: h,
                wildcard,
                port,
            } => {
                let host_ok = if *wildcard {
                    host.len() > h.len() + 1
                        && host.ends_with(h.as_str())
                        && host.as_bytes()[host.len() - h.len() - 1] == b'.'
                } else {
                    host == h
                };
                // A host entry says `https` (or whatever the policy lets through for loopback).
                let port_ok = match port {
                    Some(p) => url.port_or_known_default() == Some(*p),
                    None => url.port().is_none() || url.port_or_known_default() == Some(443),
                };
                host_ok && port_ok
            }
        }
    }
}

/// Whether `path` is `prefix` or below it, at a segment boundary: `/a/b` is under `/a` and `/a/`,
/// `/ab` is not.
fn path_has_prefix(path: &str, prefix: &str) -> bool {
    if prefix == "/" || path == prefix {
        return true;
    }
    let base = prefix.trim_end_matches('/');
    path.strip_prefix(base)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether a connection to `ip` is refused when private addresses are not allowed.
pub fn is_refused_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_refused_v4(v4),
        IpAddr::V6(v6) => is_refused_v6(v6),
    }
}

fn is_refused_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_broadcast()
        // 100.64.0.0/10 carrier-grade NAT
        || (a == 100 && (b & 0b1100_0000) == 0b0100_0000)
        // 192.0.0.0/24 IETF protocol assignments
        || (a == 192 && b == 0 && c == 0)
        // documentation: 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        // 198.18.0.0/15 benchmarking
        || (a == 198 && (b & 0b1111_1110) == 18)
        // 240.0.0.0/4 reserved
        || a >= 240
        // 0.0.0.0/8 "this network"
        || a == 0
}

fn is_refused_v6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    let seg = ip.segments();
    // fc00::/7 unique local, fe80::/10 link-local
    if (seg[0] & 0xfe00) == 0xfc00 || (seg[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // ::ffff:a.b.c.d (IPv4-mapped) and ::a.b.c.d (IPv4-compatible): judge the IPv4 address.
    if let Some(v4) = ip.to_ipv4_mapped().or_else(|| ip.to_ipv4()) {
        return is_refused_v4(v4);
    }
    // 64:ff9b::/96 (NAT64): judge the embedded IPv4 address.
    if seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2..6].iter().all(|s| *s == 0) {
        let v4 = Ipv4Addr::new(
            (seg[6] >> 8) as u8,
            seg[6] as u8,
            (seg[7] >> 8) as u8,
            seg[7] as u8,
        );
        return is_refused_v4(v4);
    }
    // 2001:db8::/32 documentation
    seg[0] == 0x2001 && seg[1] == 0x0db8
}

/// The DNS resolver of the delivery client: resolves with the system resolver and **drops the
/// addresses that are refused** (see the module docs), unless private addresses are allowed. A
/// name that resolves only to refused addresses fails to resolve, so nothing is connected to.
#[derive(Clone, Copy, Debug)]
pub struct GuardedResolver {
    allow_private: bool,
}

impl GuardedResolver {
    /// A resolver that follows `policy`.
    pub fn new(policy: &PushPolicy) -> Self {
        Self {
            allow_private: policy.allow_private,
        }
    }
}

/// The resolution failed: the text is for the operator's log and the delivery's `last_error`.
#[derive(Debug, thiserror::Error)]
#[error("the webhook host resolves only to addresses that are not allowed")]
struct OnlyRefused;

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_private = self.allow_private;
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let found: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let allowed: Vec<SocketAddr> = found
                .into_iter()
                .filter(|a| allow_private || !is_refused_ip(a.ip()))
                .collect();
            if allowed.is_empty() {
                return Err(Box::new(OnlyRefused) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(allowed.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(entries: &[&str]) -> PushPolicy {
        PushPolicy::new(entries).unwrap()
    }

    #[test]
    fn an_empty_policy_is_off() {
        let none = PushPolicy::new(Vec::<String>::new()).unwrap();
        assert!(!none.is_enabled());
        assert!(none.check("https://hooks.example.com/x").is_err());
        assert!(policy(&["hooks.example.com"]).is_enabled());
    }

    #[test]
    fn a_url_prefix_matches_on_scheme_host_port_and_path_segments() {
        let p = policy(&["https://hooks.example.com/a2a/"]);
        assert!(p.check("https://hooks.example.com/a2a/").is_ok());
        assert!(
            p.check("https://hooks.example.com/a2a/tenant-1?x=1")
                .is_ok()
        );
        assert!(
            p.check("https://hooks.example.com/a2a").is_err(),
            "the prefix has a trailing slash"
        );
        assert!(p.check("https://hooks.example.com/a2ab/x").is_err());
        assert!(p.check("https://hooks.example.com.evil.com/a2a/x").is_err());
        assert!(p.check("https://hooks.example.com:8443/a2a/x").is_err());
        assert!(p.check("https://evil.com/a2a/").is_err());
        assert!(
            p.check("http://hooks.example.com/a2a/x").is_err(),
            "https only"
        );
        let bare = policy(&["https://hooks.example.com/a2a"]);
        assert!(bare.check("https://hooks.example.com/a2a").is_ok());
        assert!(bare.check("https://hooks.example.com/a2a/x").is_ok());
        assert!(bare.check("https://hooks.example.com/a2ax").is_err());
    }

    #[test]
    fn a_host_entry_allows_every_path_of_that_host() {
        let p = policy(&[
            "hooks.example.com",
            "*.partner.io",
            "other.example.com:8443",
        ]);
        assert!(p.check("https://hooks.example.com/anything?q=1").is_ok());
        assert!(
            p.check("https://HOOKS.example.com/x").is_ok(),
            "case-insensitive"
        );
        assert!(p.check("https://a.partner.io/x").is_ok());
        assert!(p.check("https://a.b.partner.io/x").is_ok());
        assert!(
            p.check("https://partner.io/x").is_err(),
            "a wildcard needs a subdomain"
        );
        assert!(p.check("https://evilpartner.io/x").is_err());
        assert!(p.check("https://other.example.com:8443/x").is_ok());
        assert!(
            p.check("https://other.example.com/x").is_err(),
            "the port is part of the entry"
        );
        assert!(p.check("https://hooks.example.com:9999/x").is_err());
    }

    #[test]
    fn credentials_odd_schemes_and_long_urls_are_refused() {
        let p = policy(&["hooks.example.com"]);
        assert!(p.check("https://user:pw@hooks.example.com/x").is_err());
        assert!(p.check("https://user@hooks.example.com/x").is_err());
        assert!(p.check("ftp://hooks.example.com/x").is_err());
        assert!(p.check("file:///etc/passwd").is_err());
        assert!(p.check("not a url").is_err());
        let long = format!("https://hooks.example.com/{}", "a".repeat(MAX_URL_LEN));
        assert!(p.check(&long).is_err());
    }

    #[test]
    fn private_literals_and_localhost_are_refused_unless_allowed() {
        let hosts = [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.9",
            "172.16.0.1",
            "169.254.169.254",
            "[::1]",
            "[fd00::1]",
            "[::ffff:10.0.0.1]",
            "localhost",
            "foo.localhost",
            "localhost.",
        ];
        let strict = policy(&[
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.9",
            "172.16.0.1",
            "169.254.169.254",
            "[::1]",
            "[fd00::1]",
            "[::ffff:10.0.0.1]",
            "localhost",
            "foo.localhost",
        ]);
        for host in hosts {
            assert!(
                strict.check(&format!("https://{host}/x")).is_err(),
                "{host} must be refused"
            );
        }
        let dev = policy(&["127.0.0.1:8080", "localhost:8080"]).allow_private_addresses(true);
        assert!(
            dev.check("http://127.0.0.1:8080/hook").is_ok(),
            "loopback over http in dev"
        );
        assert!(dev.check("https://127.0.0.1:8080/hook").is_ok());
        assert!(dev.check("http://localhost:8080/hook").is_ok());
        let dev_remote = policy(&["hooks.example.com"]).allow_private_addresses(true);
        assert!(
            dev_remote.check("http://hooks.example.com/x").is_err(),
            "http is for loopback only"
        );
    }

    #[test]
    fn refused_ranges() {
        for ip in [
            "0.0.0.0",
            "127.0.0.1",
            "10.0.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "198.18.0.1",
            "198.19.255.255",
            "192.0.0.1",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fe80::1",
            "fc00::1",
            "fdff::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:192.168.0.1",
            "64:ff9b::a00:1",
            "2001:db8::1",
        ] {
            assert!(is_refused_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "100.63.255.255",
            "100.128.0.1",
            "172.32.0.1",
            "198.20.0.1",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert!(!is_refused_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn bad_entries_are_refused_at_startup() {
        for bad in [
            "",
            "  ",
            "https://user:pw@h.example/",
            "https://h.example/?q=1",
            "ftp://h.example/",
            "h.example/path",
            "*h.example",
            "a.*.example",
            "h.example:notaport",
            "https://",
        ] {
            assert!(PushPolicy::new([bad]).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn the_guarded_resolver_drops_refused_addresses() {
        let strict = GuardedResolver::new(&policy(&["localhost"]));
        let name: Name = "localhost".parse().unwrap();
        let err = strict
            .resolve(name)
            .await
            .err()
            .expect("localhost is refused");
        assert!(err.to_string().contains("not allowed"), "{err}");
        let dev = GuardedResolver::new(&policy(&["localhost"]).allow_private_addresses(true));
        let name: Name = "localhost".parse().unwrap();
        assert!(dev.resolve(name).await.unwrap().next().is_some());
    }
}
