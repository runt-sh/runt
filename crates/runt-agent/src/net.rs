//! Guest network setup: static IPv4 on eth0 from the kernel command line.
//!
//! The host's userspace network stack has no DHCP server, so the supervisor
//! passes `runt.ip=A.B.C.D/N runt.gw=... runt.dns=...` and we apply it with
//! the classic interface/route ioctls (no netlink needed for IPv4).

use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::sys::cvt;

#[derive(Debug, Clone, PartialEq)]
pub struct NetConfig {
    pub ip: Ipv4Addr,
    pub prefix_len: u8,
    pub gateway: Ipv4Addr,
    pub dns: Ipv4Addr,
}

impl NetConfig {
    /// Parse from `runt.ip`, `runt.gw` and `runt.dns` values. None if any is
    /// missing or malformed (the VM then runs without a network).
    pub fn parse(ip: Option<&str>, gw: Option<&str>, dns: Option<&str>) -> Option<NetConfig> {
        let (addr, prefix) = ip?.split_once('/')?;
        let prefix_len: u8 = prefix.parse().ok().filter(|p| *p <= 32)?;
        Some(NetConfig {
            ip: addr.parse().ok()?,
            prefix_len,
            gateway: gw?.parse().ok()?,
            dns: dns?.parse().ok()?,
        })
    }

    fn netmask(&self) -> Ipv4Addr {
        let bits = if self.prefix_len == 0 {
            0
        } else {
            u32::MAX << (32 - self.prefix_len)
        };
        Ipv4Addr::from(bits)
    }
}

/// Configure eth0, the default route, and resolv.conf. Must run after the
/// switch into the real root (it writes /etc).
pub fn configure(cfg: &NetConfig, hostname: Option<&str>) -> io::Result<()> {
    let sock = inet_socket()?;
    let mut ifr = ifreq("eth0");
    set_addr(&mut ifr, cfg.ip);
    ioctl(&sock, libc::SIOCSIFADDR as _, &mut ifr, "set eth0 address")?;
    let mut ifr = ifreq("eth0");
    set_addr(&mut ifr, cfg.netmask());
    ioctl(
        &sock,
        libc::SIOCSIFNETMASK as _,
        &mut ifr,
        "set eth0 netmask",
    )?;
    let mut ifr = ifreq("eth0");
    ioctl(&sock, libc::SIOCGIFFLAGS as _, &mut ifr, "get eth0 flags")?;
    // SAFETY: ifru_flags is the active union member after SIOCGIFFLAGS.
    unsafe { ifr.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short };
    ioctl(&sock, libc::SIOCSIFFLAGS as _, &mut ifr, "bring eth0 up")?;

    // SAFETY: zeroed rtentry is valid; we fill the fields the kernel reads.
    let mut rt: RtEntry = unsafe { std::mem::zeroed() };
    write_sockaddr(&mut rt.rt_dst, Ipv4Addr::UNSPECIFIED);
    write_sockaddr(&mut rt.rt_gateway, cfg.gateway);
    write_sockaddr(&mut rt.rt_genmask, Ipv4Addr::UNSPECIFIED);
    rt.rt_flags = libc::RTF_UP | libc::RTF_GATEWAY;
    ioctl(&sock, libc::SIOCADDRT as _, &mut rt, "add default route")?;

    fs::write("/etc/resolv.conf", format!("nameserver {}\n", cfg.dns))?;
    if let Some(name) = hostname {
        ensure_hosts_entry(name)?;
    }
    Ok(())
}

/// Make sure /etc/hosts resolves localhost and our hostname. OCI images often
/// ship it empty because container runtimes bind-mount their own.
fn ensure_hosts_entry(name: &str) -> io::Result<()> {
    let current = fs::read_to_string("/etc/hosts").unwrap_or_default();
    let updated = hosts_with(&current, name);
    if updated != current {
        fs::write("/etc/hosts", updated)?;
    }
    Ok(())
}

fn hosts_with(current: &str, name: &str) -> String {
    let mut out = current.to_string();
    let has = |host: &str| {
        current
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .any(|l| l.split_whitespace().skip(1).any(|h| h == host))
    };
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !has("localhost") {
        out.push_str("127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n");
    }
    if !has(name) {
        out.push_str(&format!("127.0.1.1\t{name}\n"));
    }
    out
}

/// `struct rtentry` from <net/route.h> (not exposed by the libc crate on
/// every target).
#[repr(C)]
struct RtEntry {
    rt_pad1: libc::c_ulong,
    rt_dst: libc::sockaddr,
    rt_gateway: libc::sockaddr,
    rt_genmask: libc::sockaddr,
    rt_flags: libc::c_ushort,
    rt_pad2: libc::c_short,
    rt_pad3: libc::c_ulong,
    rt_pad4: *mut libc::c_void,
    rt_metric: libc::c_short,
    rt_dev: *mut libc::c_char,
    rt_mtu: libc::c_ulong,
    rt_window: libc::c_ulong,
    rt_irtt: libc::c_ushort,
}

fn inet_socket() -> io::Result<OwnedFd> {
    // SAFETY: plain socket(2).
    let fd = cvt(unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) })?;
    // SAFETY: socket returned a new owned fd.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn ifreq(name: &str) -> libc::ifreq {
    // SAFETY: zeroed ifreq is valid.
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    for (i, b) in name.bytes().take(libc::IFNAMSIZ - 1).enumerate() {
        ifr.ifr_name[i] = b as libc::c_char;
    }
    ifr
}

fn set_addr(ifr: &mut libc::ifreq, ip: Ipv4Addr) {
    // SAFETY: ifru_addr is a plain sockaddr; we write it whole.
    write_sockaddr(unsafe { &mut ifr.ifr_ifru.ifru_addr }, ip);
}

fn write_sockaddr(sa: &mut libc::sockaddr, ip: Ipv4Addr) {
    let sin = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from(ip).to_be(),
        },
        sin_zero: [0; 8],
    };
    // SAFETY: sockaddr_in and sockaddr have the same size on Linux.
    *sa = unsafe { std::mem::transmute::<libc::sockaddr_in, libc::sockaddr>(sin) };
}

fn ioctl<T>(sock: &OwnedFd, req: libc::Ioctl, arg: &mut T, what: &str) -> io::Result<()> {
    // SAFETY: arg is a valid, correctly typed struct for this request.
    cvt(unsafe { libc::ioctl(sock.as_raw_fd(), req, arg as *mut T) })
        .map(drop)
        .map_err(|e| io::Error::new(e.kind(), format!("{what}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config() {
        let c = NetConfig::parse(
            Some("100.96.0.2/30"),
            Some("100.96.0.1"),
            Some("100.96.0.1"),
        )
        .unwrap();
        assert_eq!(c.ip, Ipv4Addr::new(100, 96, 0, 2));
        assert_eq!(c.netmask(), Ipv4Addr::new(255, 255, 255, 252));
        assert!(NetConfig::parse(Some("100.96.0.2"), Some("1.1.1.1"), Some("1.1.1.1")).is_none());
        assert!(
            NetConfig::parse(Some("100.96.0.2/33"), Some("1.1.1.1"), Some("1.1.1.1")).is_none()
        );
        assert!(NetConfig::parse(None, None, None).is_none());
    }

    #[test]
    fn rtentry_matches_c_layout() {
        // x86_64 and aarch64 glibc/musl: sizeof(struct rtentry) == 120.
        assert_eq!(size_of::<RtEntry>(), 120);
    }

    #[test]
    fn hosts_file() {
        let h = hosts_with("", "vm1");
        assert!(h.contains("127.0.0.1\tlocalhost\n"));
        assert!(h.contains("127.0.1.1\tvm1\n"));
        assert_eq!(hosts_with(&h, "vm1"), h);
        let existing = "127.0.0.1 localhost\n# 1.2.3.4 vm2\n";
        assert_eq!(
            hosts_with(existing, "vm2"),
            format!("{existing}127.0.1.1\tvm2\n")
        );
    }
}
