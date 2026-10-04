//! Userspace networking for runt VMs.
//!
//! The guest's virtio-net device is connected (by libkrun) to one end of a
//! socketpair; a userspace TCP/IP stack on the other end turns guest flows
//! into ordinary host sockets. No TAP devices, bridges or privileges.
//!
//! The stack itself is `smolvm-network`, pinned to an exact version. Nothing
//! outside this crate names its types, so it can be vendored or replaced.
//!
//! Default egress floor (from smolvm-network): the guest cannot reach the
//! cloud-metadata range (169.254.0.0/16) or the host's own loopback; the
//! internet and the host's LAN are reachable.

use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{IntoRawFd, RawFd};
use std::os::unix::net::UnixStream;

use smolvm_network::{BoundPublishedPorts, EgressPolicy, GuestNetworkConfig, VirtioNetworkRuntime};

/// Addresses the guest is configured with (statically, via the kernel
/// command line; there is no DHCP).
#[derive(Debug, Clone)]
pub struct GuestAddrs {
    pub ip: Ipv4Addr,
    pub prefix_len: u8,
    pub gateway: Ipv4Addr,
    pub dns: Ipv4Addr,
    pub mac: [u8; 6],
}

impl GuestAddrs {
    /// `runt.ip=... runt.gw=... runt.dns=...` for the guest kernel cmdline.
    pub fn cmdline(&self) -> String {
        format!(
            "runt.ip={}/{} runt.gw={} runt.dns={}",
            self.ip, self.prefix_len, self.gateway, self.dns
        )
    }
}

/// A running network for one VM. Keep it alive for the VM's lifetime.
pub struct Net {
    /// The libkrun end of the socketpair (unixstream framing). Ownership of
    /// the fd passes to libkrun when it is handed over.
    pub vmm_fd: RawFd,
    pub guest: GuestAddrs,
    _runtime: VirtioNetworkRuntime,
}

/// Start the userspace network. `upstream_dns` defaults to the host's own
/// resolver so VPN and split-horizon DNS keep working inside the VM.
pub fn start(upstream_dns: Option<Ipv4Addr>) -> io::Result<Net> {
    let mut cfg = GuestNetworkConfig::default();
    cfg.upstream_dns = upstream_dns
        .or_else(host_resolver)
        .unwrap_or(Ipv4Addr::new(1, 1, 1, 1));
    let guest = GuestAddrs {
        ip: cfg.guest_ip,
        prefix_len: cfg.prefix_len,
        gateway: cfg.gateway_ip,
        dns: cfg.dns_server,
        mac: cfg.guest_mac,
    };
    let (vmm_end, stack_end) = UnixStream::pair()?;
    let runtime = smolvm_network::start_virtio_network(
        socket2::Socket::from(std::os::fd::OwnedFd::from(stack_end)),
        cfg,
        BoundPublishedPorts::bind(&[])?,
        EgressPolicy::unrestricted(),
        None,
    )?;
    Ok(Net {
        vmm_fd: vmm_end.into_raw_fd(),
        guest,
        _runtime: runtime,
    })
}

/// The host's DNS server. A loopback stub (systemd-resolved's 127.0.0.53) is
/// fine: the DNS relay queries it from the host, not from the guest, and the
/// stub knows the host's VPN routing domains.
fn host_resolver() -> Option<Ipv4Addr> {
    first_ipv4_nameserver(&fs::read_to_string("/etc/resolv.conf").ok()?)
}

fn first_ipv4_nameserver(resolv_conf: &str) -> Option<Ipv4Addr> {
    resolv_conf.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next() == Some("nameserver")).then(|| words.next()?.parse().ok())?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_resolv_conf() {
        let conf = "# comment\nsearch lan\nnameserver fe80::1\nnameserver 127.0.0.53\nnameserver 8.8.8.8\n";
        assert_eq!(
            first_ipv4_nameserver(conf),
            Some(Ipv4Addr::new(127, 0, 0, 53))
        );
        assert_eq!(first_ipv4_nameserver("search lan\n"), None);
    }

    #[test]
    fn guest_cmdline() {
        let g = GuestAddrs {
            ip: Ipv4Addr::new(100, 96, 0, 2),
            prefix_len: 30,
            gateway: Ipv4Addr::new(100, 96, 0, 1),
            dns: Ipv4Addr::new(100, 96, 0, 1),
            mac: [0; 6],
        };
        assert_eq!(
            g.cmdline(),
            "runt.ip=100.96.0.2/30 runt.gw=100.96.0.1 runt.dns=100.96.0.1"
        );
    }
}
