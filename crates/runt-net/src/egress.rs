//! Egress policy: what a VM's network may reach.
//!
//! By default a VM reaches the public internet and nothing else. Users can
//! narrow that to an allowlist of domains and IPv4 networks
//! (`--allow github.com --allow '*.npmjs.org' --allow 203.0.113.0/24`) and/or
//! open the private networks around the host (`--allow-lan`).
//!
//! smolvm enforces this with two layers: a *floor* of address ranges no rule
//! can re-open, chosen once per process from the environment, and an
//! allowlist. Two properties of the floor shape this module:
//!
//! - Only the `strict` floor blocks private and CGNAT ranges. Anything that
//!   reaches the LAN must use the softer local floor.
//! - The guest's gateway address is relayed to the *host's* loopback, and
//!   only `strict` floors it. So whenever the floor is soft, we always pass
//!   an explicit list of IPv4 networks with loopback, link-local and the
//!   guest's own subnet carved out, and never a domain list: addresses
//!   learned from DNS answers would bypass the carve-outs (DNS rebinding onto
//!   the gateway). Domains plus LAN access is refused for that reason.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr};

/// A parsed egress policy. Build with [`Policy::new`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    /// Domain rules in smolvm's encoding (`=exact`, `*.suffix`).
    hosts: Vec<String>,
    nets: Vec<Net>,
    lan: bool,
}

/// One `--allow` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
    /// `example.com` (exactly) or `*.example.com` (any subdomain).
    Domain(String),
    /// An IPv4 address or network.
    Net(Net),
}

/// An IPv4 network: address and prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Net {
    pub addr: Ipv4Addr,
    pub len: u8,
}

impl fmt::Display for Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.len)
    }
}

impl Net {
    pub(crate) fn new(addr: Ipv4Addr, len: u8) -> Self {
        let r = Range::of(u32::from(addr), len);
        Net {
            addr: Ipv4Addr::from(r.0),
            len,
        }
    }

    fn range(self) -> Range {
        Range::of(u32::from(self.addr), self.len)
    }
}

/// Private networks `--allow-lan` opens: RFC 1918 and CGNAT (100.64.0.0/10,
/// where Tailscale and similar overlays live).
const LAN: &[(Ipv4Addr, u8)] = &[
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
];

/// Never reachable from a VM, whatever the rules say.
const NEVER: &[(Ipv4Addr, u8)] = &[
    (Ipv4Addr::new(0, 0, 0, 0), 8),      // "this host"
    (Ipv4Addr::new(127, 0, 0, 0), 8),    // the host's loopback
    (Ipv4Addr::new(169, 254, 0, 0), 16), // link-local, cloud metadata
    (Ipv4Addr::new(224, 0, 0, 0), 4),    // multicast
    (Ipv4Addr::new(240, 0, 0, 0), 4),    // reserved, broadcast
];

/// Parse one `--allow` value.
pub fn parse_rule(s: &str) -> Result<Rule, String> {
    let s = s.trim();
    if let Ok(ip) = s.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(a) => Ok(Rule::Net(Net::new(a, 32))),
            IpAddr::V6(_) => Err(v6_unsupported(s)),
        };
    }
    if let Some((addr, len)) = s.split_once('/') {
        let len: u8 = len
            .parse()
            .ok()
            .filter(|l| *l <= 32)
            .ok_or_else(|| format!("invalid network {s:?}: bad prefix length"))?;
        return match addr.parse::<IpAddr>() {
            Ok(IpAddr::V4(a)) => Ok(Rule::Net(Net::new(a, len))),
            Ok(IpAddr::V6(_)) => Err(v6_unsupported(s)),
            Err(_) => Err(format!("invalid network {s:?}")),
        };
    }
    parse_domain(s).map(Rule::Domain)
}

fn v6_unsupported(s: &str) -> String {
    format!("cannot allow {s:?}: VMs have no IPv6 yet")
}

/// Validate a domain pattern; returns its canonical (lowercase) form.
fn parse_domain(s: &str) -> Result<String, String> {
    let bad = || format!("invalid domain {s:?}: use example.com or *.example.com");
    let lower = s.strip_suffix('.').unwrap_or(s).to_ascii_lowercase();
    let host = lower.strip_prefix("*.").unwrap_or(&lower);
    let valid_label = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    if host.is_empty() || host.len() > 253 || !host.split('.').all(valid_label) {
        return Err(bad());
    }
    Ok(lower)
}

impl Policy {
    /// Build a policy from `--allow` rules and `--allow-lan`.
    pub fn new(rules: &[Rule], lan: bool) -> Result<Policy, String> {
        let mut p = Policy {
            lan,
            ..Policy::default()
        };
        for r in rules {
            match r {
                Rule::Domain(d) if d.starts_with("*.") => p.hosts.push(d.clone()),
                Rule::Domain(d) => p.hosts.push(format!("={d}")),
                Rule::Net(n) => {
                    if subtract(&[n.range()], &ranges(NEVER)).is_empty() {
                        return Err(format!(
                            "cannot allow {n}: loopback, link-local and multicast \
                             addresses are never reachable from a VM"
                        ));
                    }
                    p.nets.push(*n);
                }
            }
        }
        if !p.hosts.is_empty() && !p.strict() {
            return Err("domain rules cannot be combined with LAN access yet: \
                        drop --allow-lan and private networks, or allow LAN \
                        hosts by address only"
                .into());
        }
        Ok(p)
    }

    /// Whether any rule narrows the default (public internet only).
    pub fn is_default(&self) -> bool {
        self.hosts.is_empty() && self.nets.is_empty() && !self.lan
    }

    /// Whether the strict floor applies (nothing private is reachable).
    pub fn strict(&self) -> bool {
        let private = ranges(LAN);
        !self.lan
            && self
                .nets
                .iter()
                .all(|n| subtract(&[n.range()], &private) == [n.range()])
    }

    /// Arguments for `smolvm_network::EgressPolicy::new`: allowed CIDRs and
    /// domains, `None` meaning "no list". `guest` is the guest's own subnet,
    /// which a soft-floor list must exclude.
    pub(crate) fn smolvm_lists(&self, guest: Net) -> (Option<Vec<String>>, Option<Vec<String>>) {
        if self.strict() {
            if self.hosts.is_empty() && self.nets.is_empty() {
                return (None, None);
            }
            let nets = self.nets.iter().map(Net::to_string).collect();
            let hosts = (!self.hosts.is_empty()).then(|| self.hosts.clone());
            return (Some(nets), hosts);
        }
        let mut allow: Vec<Range> = if self.nets.is_empty() {
            vec![Range(0, u32::MAX)] // --allow-lan alone: internet + LAN
        } else {
            self.nets.iter().map(|n| n.range()).collect()
        };
        if self.lan {
            allow.extend(ranges(LAN));
        }
        let mut never = ranges(NEVER);
        never.push(guest.range());
        let nets = to_cidrs(&subtract(&allow, &never));
        (Some(nets.iter().map(Net::to_string).collect()), None)
    }
}

/// An inclusive range of IPv4 addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Range(u32, u32);

impl Range {
    fn of(addr: u32, len: u8) -> Range {
        let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
        Range(addr & mask, (addr & mask) | !mask)
    }
}

fn ranges(nets: &[(Ipv4Addr, u8)]) -> Vec<Range> {
    nets.iter()
        .map(|(a, l)| Range::of(u32::from(*a), *l))
        .collect()
}

/// Sorted, merged union.
fn normalize(rs: &[Range]) -> Vec<Range> {
    let mut rs = rs.to_vec();
    rs.sort();
    let mut out: Vec<Range> = Vec::with_capacity(rs.len());
    for r in rs {
        match out.last_mut() {
            Some(last) if r.0 <= last.1.saturating_add(1) => last.1 = last.1.max(r.1),
            _ => out.push(r),
        }
    }
    out
}

/// `a` minus `b`.
fn subtract(a: &[Range], b: &[Range]) -> Vec<Range> {
    let b = normalize(b);
    let mut out = Vec::new();
    for mut r in normalize(a) {
        let mut keep = true;
        for cut in &b {
            if cut.1 < r.0 || cut.0 > r.1 {
                continue;
            }
            if cut.0 > r.0 {
                out.push(Range(r.0, cut.0 - 1));
            }
            if cut.1 >= r.1 {
                keep = false;
                break;
            }
            r.0 = cut.1 + 1;
        }
        if keep {
            out.push(r);
        }
    }
    out
}

/// The fewest CIDRs covering exactly these ranges.
fn to_cidrs(rs: &[Range]) -> Vec<Net> {
    let mut out = Vec::new();
    for r in normalize(rs) {
        let (mut start, end) = (u64::from(r.0), u64::from(r.1));
        while start <= end {
            // Largest aligned block starting at `start` that fits.
            let mut size = if start == 0 {
                1u64 << 32
            } else {
                start.isolate_lowest_one()
            };
            while start + size - 1 > end {
                size >>= 1;
            }
            out.push(Net {
                addr: Ipv4Addr::from(start as u32),
                len: 32 - size.trailing_zeros() as u8,
            });
            start += size;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(s: &str) -> Net {
        match parse_rule(s).unwrap() {
            Rule::Net(n) => n,
            r => panic!("{r:?}"),
        }
    }

    fn policy(rules: &[&str], lan: bool) -> Result<Policy, String> {
        let rules: Vec<Rule> = rules.iter().map(|r| parse_rule(r).unwrap()).collect();
        Policy::new(&rules, lan)
    }

    const GUEST: Net = Net {
        addr: Ipv4Addr::new(100, 96, 0, 0),
        len: 30,
    };

    #[test]
    fn parses_rules() {
        assert_eq!(
            parse_rule("GitHub.com.").unwrap(),
            Rule::Domain("github.com".into())
        );
        assert_eq!(
            parse_rule("*.npmjs.org").unwrap(),
            Rule::Domain("*.npmjs.org".into())
        );
        assert_eq!(net("1.2.3.4").to_string(), "1.2.3.4/32");
        assert_eq!(net("10.1.2.3/8").to_string(), "10.0.0.0/8");
        for bad in [
            "",
            "*.",
            "a..b",
            "-a.com",
            "a b.com",
            "*.*.com",
            "1.2.3.4/33",
            "x/8",
            "::1",
            "fd00::/8",
        ] {
            assert!(parse_rule(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn default_is_strict_and_unlisted() {
        let p = policy(&[], false).unwrap();
        assert!(p.is_default() && p.strict());
        assert_eq!(p.smolvm_lists(GUEST), (None, None));
    }

    #[test]
    fn public_allowlist_stays_strict() {
        let p = policy(&["github.com", "*.npmjs.org", "203.0.113.0/24"], false).unwrap();
        assert!(p.strict());
        assert_eq!(
            p.smolvm_lists(GUEST),
            (
                Some(vec!["203.0.113.0/24".into()]),
                Some(vec!["=github.com".into(), "*.npmjs.org".into()])
            )
        );
    }

    #[test]
    fn lan_opens_private_ranges_but_never_the_host() {
        let p = policy(&[], true).unwrap();
        assert!(!p.strict());
        let (nets, hosts) = p.smolvm_lists(GUEST);
        assert!(hosts.is_none());
        let nets: Vec<Range> = nets.unwrap().iter().map(|s| net(s).range()).collect();
        let covers = |ip: [u8; 4]| {
            let ip = u32::from(Ipv4Addr::from(ip));
            nets.iter().any(|r| r.0 <= ip && ip <= r.1)
        };
        assert!(covers([8, 8, 8, 8]) && covers([192, 168, 1, 1]) && covers([100, 100, 1, 1]));
        assert!(covers([100, 96, 0, 4]));
        for never in [
            [127, 0, 0, 1],
            [0, 0, 0, 0],
            [169, 254, 169, 254],
            [100, 96, 0, 1],
            [100, 96, 0, 2],
            [255, 255, 255, 255],
            [224, 0, 0, 1],
        ] {
            assert!(!covers(never), "{never:?} must not be reachable");
        }
    }

    #[test]
    fn private_networks_imply_the_soft_floor_with_carve_outs() {
        let p = policy(&["100.64.0.0/10"], false).unwrap();
        assert!(!p.strict());
        let (nets, _) = p.smolvm_lists(GUEST);
        let nets = nets.unwrap();
        assert!(!nets.iter().any(
            |n| net(n).range().0 <= u32::from(Ipv4Addr::new(100, 96, 0, 1))
                && u32::from(Ipv4Addr::new(100, 96, 0, 1)) <= net(n).range().1
        ));
        assert!(nets.contains(&"100.64.0.0/11".to_string()));
    }

    #[test]
    fn refuses_unsafe_combinations() {
        assert!(policy(&["github.com"], true).is_err());
        assert!(policy(&["github.com", "10.0.0.0/8"], false).is_err());
        assert!(policy(&["127.0.0.1"], false).is_err());
        assert!(policy(&["169.254.169.254"], false).is_err());
        assert!(policy(&["github.com", "1.1.1.1"], false).is_ok());
    }

    #[test]
    fn range_arithmetic() {
        let all = [Range(0, u32::MAX)];
        assert_eq!(to_cidrs(&all), vec![net("0.0.0.0/0")]);
        let rest = subtract(&all, &ranges(&[(Ipv4Addr::new(128, 0, 0, 0), 1)]));
        assert_eq!(to_cidrs(&rest), vec![net("0.0.0.0/1")]);
        let r = subtract(&[net("10.0.0.0/8").range()], &[net("10.0.0.0/9").range()]);
        assert_eq!(to_cidrs(&r), vec![net("10.128.0.0/9")]);
        assert_eq!(
            to_cidrs(&[Range(1, 6)]),
            vec![
                net("0.0.0.1/32"),
                net("0.0.0.2/31"),
                net("0.0.0.4/31"),
                net("0.0.0.6/32")
            ]
        );
        assert!(subtract(&[net("1.2.3.4/32").range()], &all).is_empty());
    }
}
