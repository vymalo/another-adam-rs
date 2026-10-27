//! Which addresses the proxy may connect to: public IPv4 addresses, and nothing else.
//!
//! The decision is a pure function of the address ([`AddressFilter::admit`]), so every range has
//! a test, and it is applied to what the proxy's own resolver returned, never to what the client
//! said: a name that resolves to a private address (DNS rebinding) is dropped like a private
//! address written out.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr};

use adam_error::{Classify, ErrorClass};

/// An IPv4 range: a network address and a prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    network: u32,
    prefix: u8,
}

/// A text that is not a CIDR range.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{text:?} is not a CIDR range such as 10.0.0.0/8: {reason}")]
pub struct CidrError {
    text: String,
    reason: &'static str,
}

impl Classify for CidrError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

impl Cidr {
    /// A range from its parts; the bits of `addr` past `prefix` are cleared.
    const fn new(addr: Ipv4Addr, prefix: u8) -> Self {
        Self {
            network: u32::from_be_bytes(addr.octets()) & mask(prefix),
            prefix,
        }
    }

    /// Read `a.b.c.d/n` (a bare `a.b.c.d` is a `/32`). An IPv6 range is valid and gives `None`:
    /// every IPv6 address is dropped, so it has nothing to add, and a deployment can hand the
    /// proxy the same list of cluster ranges it hands its network policy.
    ///
    /// # Errors
    ///
    /// [`CidrError`] when the text is no range.
    pub fn parse(text: &str) -> Result<Option<Self>, CidrError> {
        let fail = |reason| CidrError {
            text: text.to_owned(),
            reason,
        };
        let (addr, prefix) = match text.split_once('/') {
            Some((addr, prefix)) => (
                addr,
                Some(
                    prefix
                        .parse::<u8>()
                        .map_err(|_| fail("the prefix length is not a number"))?,
                ),
            ),
            None => (text, None),
        };
        match addr
            .parse::<IpAddr>()
            .map_err(|_| fail("the address does not parse"))?
        {
            IpAddr::V4(addr) => {
                let prefix = prefix.unwrap_or(32);
                if prefix > 32 {
                    return Err(fail("an IPv4 prefix length is 0 to 32"));
                }
                Ok(Some(Self::new(addr, prefix)))
            }
            IpAddr::V6(_) => {
                if prefix.unwrap_or(128) > 128 {
                    return Err(fail("an IPv6 prefix length is 0 to 128"));
                }
                Ok(None)
            }
        }
    }

    /// Whether `addr` is in the range.
    pub const fn contains(self, addr: Ipv4Addr) -> bool {
        u32::from_be_bytes(addr.octets()) & mask(self.prefix) == self.network
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", Ipv4Addr::from(self.network), self.prefix)
    }
}

/// The netmask of a prefix length of 0 to 32.
const fn mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix as u32)
    }
}

/// The IPv4 ranges that are not the public internet, with what each is. Written out, not computed
/// from `Ipv4Addr::is_global`, which is unstable and changes with the standard library.
const SPECIAL: [(Cidr, &str); 15] = [
    (
        Cidr::new(Ipv4Addr::new(0, 0, 0, 0), 8),
        "this network (0.0.0.0/8)",
    ),
    (
        Cidr::new(Ipv4Addr::new(10, 0, 0, 0), 8),
        "private (10.0.0.0/8)",
    ),
    (
        Cidr::new(Ipv4Addr::new(100, 64, 0, 0), 10),
        "shared address space (100.64.0.0/10)",
    ),
    (
        Cidr::new(Ipv4Addr::new(127, 0, 0, 0), 8),
        "loopback (127.0.0.0/8)",
    ),
    (
        Cidr::new(Ipv4Addr::new(169, 254, 0, 0), 16),
        "link-local (169.254.0.0/16)",
    ),
    (
        Cidr::new(Ipv4Addr::new(172, 16, 0, 0), 12),
        "private (172.16.0.0/12)",
    ),
    (
        Cidr::new(Ipv4Addr::new(192, 0, 0, 0), 24),
        "IETF protocol assignments (192.0.0.0/24)",
    ),
    (
        Cidr::new(Ipv4Addr::new(192, 0, 2, 0), 24),
        "documentation (192.0.2.0/24)",
    ),
    (
        Cidr::new(Ipv4Addr::new(192, 88, 99, 0), 24),
        "6to4 relay anycast (192.88.99.0/24)",
    ),
    (
        Cidr::new(Ipv4Addr::new(192, 168, 0, 0), 16),
        "private (192.168.0.0/16)",
    ),
    (
        Cidr::new(Ipv4Addr::new(198, 18, 0, 0), 15),
        "benchmarking (198.18.0.0/15)",
    ),
    (
        Cidr::new(Ipv4Addr::new(198, 51, 100, 0), 24),
        "documentation (198.51.100.0/24)",
    ),
    (
        Cidr::new(Ipv4Addr::new(203, 0, 113, 0), 24),
        "documentation (203.0.113.0/24)",
    ),
    (
        Cidr::new(Ipv4Addr::new(224, 0, 0, 0), 4),
        "multicast (224.0.0.0/4)",
    ),
    (
        Cidr::new(Ipv4Addr::new(240, 0, 0, 0), 4),
        "reserved and broadcast (240.0.0.0/4)",
    ),
];

/// Why an address is not one the proxy connects to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Dropped {
    /// In one of the built-in ranges that are not the public internet; the text names it.
    Special(&'static str),
    /// In a range the deployment added (`EGRESS_DENY_CIDRS`).
    Denied(Cidr),
    /// An IPv6 address: the proxy connects over IPv4 only. (The embedded IPv4 address of a mapped
    /// one is judged first, so a mapped private address is reported as the private address it is.)
    Ipv6,
}

impl fmt::Display for Dropped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Special(what) => f.write_str(what),
            Self::Denied(cidr) => write!(f, "denied range ({cidr})"),
            Self::Ipv6 => f.write_str("IPv6 (the proxy connects over IPv4 only)"),
        }
    }
}

/// The rule for which resolved addresses are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressFilter {
    builtin: bool,
    extra: Vec<Cidr>,
}

impl AddressFilter {
    /// What the proxy runs with: the built-in ranges, and `extra` (the deployment's own).
    pub fn standard(extra: Vec<Cidr>) -> Self {
        Self {
            builtin: true,
            extra,
        }
    }

    /// Drop exactly `ranges` (and every IPv6 address) and nothing built in. **For tests**, so that
    /// a server on the loopback counts as public; the proxy never runs with it.
    pub fn only(ranges: Vec<Cidr>) -> Self {
        Self {
            builtin: false,
            extra: ranges,
        }
    }

    /// The address as an IPv4 address to connect to, or why it is not.
    ///
    /// # Errors
    ///
    /// [`Dropped`] naming the range, or IPv6.
    pub fn admit(&self, addr: IpAddr) -> Result<Ipv4Addr, Dropped> {
        let (v4, mapped) = match addr {
            IpAddr::V4(v4) => (v4, false),
            // `::ffff:a.b.c.d`: a different family around an IPv4 address that a dual-stack socket
            // hands on to the embedded one, so the same hole as the address written out.
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => (v4, true),
                None => return Err(Dropped::Ipv6),
            },
        };
        if self.builtin
            && let Some((_, what)) = SPECIAL.iter().find(|(cidr, _)| cidr.contains(v4))
        {
            return Err(Dropped::Special(what));
        }
        if let Some(cidr) = self.extra.iter().find(|cidr| cidr.contains(v4)) {
            return Err(Dropped::Denied(*cidr));
        }
        if mapped {
            // Public, but written as IPv6: dropped with the rest of them. A future `ipv6` option
            // starts from here, with the embedded address already judged.
            return Err(Dropped::Ipv6);
        }
        Ok(v4)
    }

    /// The addresses to connect to, in the order the resolver gave them (each once), and the
    /// reason for the first one dropped, if any was.
    pub fn keep(&self, resolved: &[IpAddr]) -> (Vec<Ipv4Addr>, Option<Dropped>) {
        let mut kept = Vec::new();
        let mut first_dropped = None;
        for addr in resolved {
            match self.admit(*addr) {
                Ok(v4) if !kept.contains(&v4) => kept.push(v4),
                Ok(_) => {}
                Err(why) => {
                    first_dropped.get_or_insert(why);
                }
            }
        }
        (kept, first_dropped)
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv6Addr;

    use super::*;

    fn v4(text: &str) -> IpAddr {
        IpAddr::V4(text.parse().unwrap())
    }

    fn v6(text: &str) -> IpAddr {
        IpAddr::V6(text.parse::<Ipv6Addr>().unwrap())
    }

    fn cidr(text: &str) -> Cidr {
        Cidr::parse(text).unwrap().unwrap()
    }

    fn standard() -> AddressFilter {
        AddressFilter::standard(Vec::new())
    }

    /// Every built-in range, its first address and its last.
    #[test]
    fn every_special_range_is_dropped_from_its_first_address_to_its_last() {
        let table: &[(&str, &str)] = &[
            ("0.0.0.0", "0.255.255.255"),
            ("10.0.0.0", "10.255.255.255"),
            ("100.64.0.0", "100.127.255.255"),
            ("127.0.0.0", "127.255.255.255"),
            ("169.254.0.0", "169.254.255.255"),
            ("172.16.0.0", "172.31.255.255"),
            ("192.0.0.0", "192.0.0.255"),
            ("192.0.2.0", "192.0.2.255"),
            ("192.88.99.0", "192.88.99.255"),
            ("192.168.0.0", "192.168.255.255"),
            ("198.18.0.0", "198.19.255.255"),
            ("198.51.100.0", "198.51.100.255"),
            ("203.0.113.0", "203.0.113.255"),
            ("224.0.0.0", "239.255.255.255"),
            ("240.0.0.0", "255.255.255.255"),
        ];
        assert_eq!(table.len(), SPECIAL.len(), "a range without a row");
        let filter = standard();
        for ((first, last), (range, _)) in table.iter().zip(SPECIAL.iter()) {
            assert_eq!(
                first.parse::<Ipv4Addr>().map(|a| range.contains(a)),
                Ok(true),
                "{first} opens {range}"
            );
            assert_eq!(
                last.parse::<Ipv4Addr>().map(|a| range.contains(a)),
                Ok(true),
                "{last} closes {range}"
            );
            for inside in [first, last] {
                assert!(
                    matches!(filter.admit(v4(inside)), Err(Dropped::Special(_))),
                    "{inside} is in a special range"
                );
            }
        }
    }

    #[test]
    fn the_addresses_just_outside_the_ranges_are_public() {
        let filter = standard();
        for public in [
            "1.0.0.0",
            "9.255.255.255",
            "11.0.0.0",
            "100.63.255.255",
            "100.128.0.0",
            "126.255.255.255",
            "128.0.0.0",
            "169.253.255.255",
            "169.255.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "191.255.255.255",
            "192.0.1.0",
            "192.0.3.0",
            "192.88.98.255",
            "192.88.100.0",
            "192.167.255.255",
            "192.169.0.0",
            "198.17.255.255",
            "198.20.0.0",
            "198.51.99.255",
            "198.51.101.0",
            "203.0.112.255",
            "203.0.114.0",
            "223.255.255.255",
            // Controls: real services.
            "1.1.1.1",
            "8.8.8.8",
            "140.82.112.3",
            "104.16.0.35",
        ] {
            assert_eq!(
                filter.admit(v4(public)),
                Ok(public.parse().unwrap()),
                "{public}"
            );
        }
    }

    #[test]
    fn single_addresses_that_matter_are_dropped_with_a_reason_that_names_them() {
        let filter = standard();
        for (addr, word) in [
            ("127.0.0.1", "loopback"),
            ("10.1.2.3", "private"),
            ("172.16.0.1", "private"),
            ("172.31.255.254", "private"),
            ("192.168.1.1", "private"),
            ("169.254.169.254", "link-local"),
            ("100.64.0.1", "shared"),
            ("0.0.0.0", "this network"),
            ("255.255.255.255", "broadcast"),
            ("224.0.0.1", "multicast"),
            ("192.0.2.1", "documentation"),
            ("198.51.100.7", "documentation"),
            ("203.0.113.9", "documentation"),
            ("198.18.0.1", "benchmarking"),
            ("192.88.99.1", "6to4"),
            ("192.0.0.8", "IETF"),
        ] {
            let why = filter.admit(v4(addr)).unwrap_err().to_string();
            assert!(why.contains(word), "{addr}: {why}");
        }
    }

    #[test]
    fn ipv6_is_always_dropped() {
        let filter = standard();
        for addr in [
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "ff02::1",
            "2001:db8::1",
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "64:ff9b::808:808",
        ] {
            assert_eq!(filter.admit(v6(addr)), Err(Dropped::Ipv6), "{addr}");
        }
    }

    #[test]
    fn an_ipv4_mapped_address_is_judged_by_the_address_inside_it() {
        let filter = standard();
        for (mapped, inside) in [
            ("::ffff:10.0.0.1", "private"),
            ("::ffff:127.0.0.1", "loopback"),
            ("::ffff:169.254.169.254", "link-local"),
            ("::ffff:192.168.0.1", "private"),
            ("::ffff:0.0.0.0", "this network"),
            ("::ffff:255.255.255.255", "broadcast"),
        ] {
            let why = filter.admit(v6(mapped)).unwrap_err();
            assert!(
                matches!(why, Dropped::Special(what) if what.contains(inside)),
                "{mapped}: {why}"
            );
        }
        // A public address inside it is still IPv6, and dropped as such.
        assert_eq!(filter.admit(v6("::ffff:8.8.8.8")), Err(Dropped::Ipv6));
        assert_eq!(filter.admit(v6("::ffff:1.1.1.1")), Err(Dropped::Ipv6));
        // And the deployment's own ranges apply to the address inside as well.
        let own = AddressFilter::standard(vec![cidr("8.8.8.0/24")]);
        assert!(matches!(
            own.admit(v6("::ffff:8.8.8.8")),
            Err(Dropped::Denied(_))
        ));
    }

    #[test]
    fn the_deployments_ranges_are_dropped_on_top_of_the_built_in_ones() {
        let filter = AddressFilter::standard(vec![cidr("8.8.8.0/24"), cidr("1.1.1.1")]);
        assert_eq!(
            filter.admit(v4("8.8.8.8")),
            Err(Dropped::Denied(cidr("8.8.8.0/24")))
        );
        assert_eq!(
            filter.admit(v4("1.1.1.1")),
            Err(Dropped::Denied(cidr("1.1.1.1/32")))
        );
        assert_eq!(filter.admit(v4("1.1.1.2")), Ok("1.1.1.2".parse().unwrap()));
        assert!(matches!(
            filter.admit(v4("10.0.0.1")),
            Err(Dropped::Special(_))
        ));
    }

    #[test]
    fn the_test_filter_drops_what_it_is_told_to_and_nothing_built_in() {
        let filter = AddressFilter::only(Vec::new());
        assert_eq!(filter.admit(v4("127.0.0.1")), Ok(Ipv4Addr::LOCALHOST));
        assert_eq!(
            filter.admit(v4("10.0.0.1")),
            Ok("10.0.0.1".parse().unwrap())
        );
        assert_eq!(filter.admit(v6("::1")), Err(Dropped::Ipv6));
        let some = AddressFilter::only(vec![cidr("10.0.0.0/8")]);
        assert!(some.admit(v4("10.0.0.1")).is_err());
        assert!(some.admit(v4("127.0.0.1")).is_ok());
    }

    #[test]
    fn keep_gives_the_public_addresses_in_order_once_and_the_first_reason() {
        let filter = standard();
        let resolved = [
            v6("2606:4700::1"),
            v4("10.0.0.5"),
            v4("93.184.216.34"),
            v4("1.1.1.1"),
            v4("93.184.216.34"),
            v4("127.0.0.1"),
        ];
        let (kept, why) = filter.keep(&resolved);
        assert_eq!(
            kept,
            [
                "93.184.216.34".parse::<Ipv4Addr>().unwrap(),
                "1.1.1.1".parse().unwrap()
            ]
        );
        assert_eq!(why, Some(Dropped::Ipv6));
        assert_eq!(filter.keep(&[]), (Vec::new(), None));
        let (kept, why) = filter.keep(&[v4("192.168.0.1")]);
        assert!(kept.is_empty());
        assert!(matches!(why, Some(Dropped::Special(_))));
    }

    #[test]
    fn a_cidr_is_read_masked_and_displayed() {
        assert_eq!(cidr("10.0.0.0/8").to_string(), "10.0.0.0/8");
        assert_eq!(cidr("10.1.2.3/8").to_string(), "10.0.0.0/8");
        assert_eq!(cidr("192.168.1.7").to_string(), "192.168.1.7/32");
        assert_eq!(cidr("0.0.0.0/0").to_string(), "0.0.0.0/0");
        assert!(cidr("0.0.0.0/0").contains("8.8.8.8".parse().unwrap()));
        assert!(cidr("10.0.0.0/8").contains("10.255.255.255".parse().unwrap()));
        assert!(!cidr("10.0.0.0/8").contains("11.0.0.0".parse().unwrap()));
        assert!(cidr("192.168.1.7/32").contains("192.168.1.7".parse().unwrap()));
        assert!(!cidr("192.168.1.7/32").contains("192.168.1.8".parse().unwrap()));
    }

    #[test]
    fn an_ipv6_range_is_valid_and_has_no_effect() {
        assert_eq!(Cidr::parse("fd00::/8"), Ok(None));
        assert_eq!(Cidr::parse("::1"), Ok(None));
        assert_eq!(Cidr::parse("2001:db8::/32"), Ok(None));
    }

    #[test]
    fn a_bad_cidr_is_refused_and_quoted() {
        for text in [
            "",
            "10.0.0.0/33",
            "10.0.0.0/-1",
            "10.0.0.0/",
            "10.0.0/8",
            "10.0.0.256/8",
            "ten/8",
            "10.0.0.0/8/8",
            "fd00::/129",
            "10.0.0.0/x",
            "example.com",
        ] {
            let e = Cidr::parse(text).unwrap_err();
            assert!(e.to_string().contains(&format!("{text:?}")), "{e}");
            assert_eq!(e.class(), ErrorClass::Invalid);
        }
    }
}
