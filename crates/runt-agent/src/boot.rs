//! Early boot: everything PID 1 does before serving requests.
//!
//! The kernel starts us from the initramfs. We mount the read-only base image
//! (vda, erofs) and the per-VM writable disk (vdb, ext4), combine them with
//! overlayfs, and switch into the result. We stay PID 1 the whole time; the
//! agent binary is already in memory.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

use crate::mounts::GuestMount;
use crate::net::NetConfig;
use crate::sys::{self, cvt};

const LOWER: &str = "/mnt/lower";
const UPPER: &str = "/mnt/upper";
const ROOT: &str = "/mnt/root";

/// Settings passed on the kernel command line as `runt.<key>=<value>`.
#[derive(Debug, Default)]
pub struct Cmdline {
    pub name: Option<String>,
    /// Static network config; None means the VM has no network.
    pub net: Option<NetConfig>,
    /// Shared folders to mount.
    pub mounts: Vec<GuestMount>,
}

impl Cmdline {
    fn parse(s: &str) -> Cmdline {
        let get = |key: &str| s.split_whitespace().find_map(|w| w.strip_prefix(key));
        Cmdline {
            name: get("runt.name=").map(String::from),
            net: NetConfig::parse(get("runt.ip="), get("runt.gw="), get("runt.dns=")),
            mounts: get("runt.fs=")
                .map(|v| {
                    let (mounts, errors) = crate::mounts::parse(v);
                    for e in errors {
                        eprintln!("runt-agent: warning: {e}");
                    }
                    mounts
                })
                .unwrap_or_default(),
        }
    }
}

pub fn early() -> io::Result<Cmdline> {
    mount("devtmpfs", "/dev", "devtmpfs", libc::MS_NOSUID, "mode=0755")?;
    mount(
        "proc",
        "/proc",
        "proc",
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        "",
    )?;
    mount(
        "sysfs",
        "/sys",
        "sysfs",
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        "",
    )?;
    let cmdline = Cmdline::parse(&fs::read_to_string("/proc/cmdline")?);

    for d in [LOWER, UPPER, ROOT] {
        mkdir_p(d)?;
    }
    mount("/dev/vda", LOWER, "erofs", libc::MS_RDONLY, "")?;
    mount("/dev/vdb", UPPER, "ext4", libc::MS_NOATIME, "")?;
    mkdir_p(&format!("{UPPER}/upper"))?;
    mkdir_p(&format!("{UPPER}/work"))?;
    mount(
        "overlay",
        ROOT,
        "overlay",
        0,
        &format!("lowerdir={LOWER},upperdir={UPPER}/upper,workdir={UPPER}/work"),
    )?;

    for d in ["dev", "proc", "sys"] {
        let target = format!("{ROOT}/{d}");
        mkdir_p(&target)?;
        mount(&format!("/{d}"), &target, "", libc::MS_MOVE, "")?;
    }

    // Free the initramfs copy of ourselves, then switch root the way
    // switch_root(8) does (pivot_root doesn't work from initramfs).
    let _ = fs::remove_file("/init");
    std::env::set_current_dir(ROOT)?;
    mount(".", "/", "", libc::MS_MOVE, "")?;
    let dot = CString::new(".").unwrap();
    // SAFETY: valid C string.
    cvt(unsafe { libc::chroot(dot.as_ptr()) })?;
    std::env::set_current_dir("/")?;

    late_mounts()?;
    crate::mounts::mount_all(&cmdline.mounts);
    if let Some(name) = &cmdline.name {
        // SAFETY: pointer/len describe a valid buffer.
        cvt(unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) })?;
    }
    if let Err(e) = sys::loopback_up() {
        eprintln!("runt-agent: warning: cannot bring up lo: {e}");
    }
    if let Some(net) = &cmdline.net
        && let Err(e) = crate::net::configure(net, cmdline.name.as_deref())
    {
        eprintln!("runt-agent: warning: network setup failed: {e}");
    }
    Ok(cmdline)
}

fn late_mounts() -> io::Result<()> {
    mkdir_p("/dev/pts")?;
    mount(
        "devpts",
        "/dev/pts",
        "devpts",
        libc::MS_NOSUID | libc::MS_NOEXEC,
        "gid=5,mode=620,ptmxmode=666",
    )?;
    mkdir_p("/dev/shm")?;
    mount(
        "tmpfs",
        "/dev/shm",
        "tmpfs",
        libc::MS_NOSUID | libc::MS_NODEV,
        "mode=1777",
    )?;
    mkdir_p("/run")?;
    mount(
        "tmpfs",
        "/run",
        "tmpfs",
        libc::MS_NOSUID | libc::MS_NODEV,
        "mode=0755",
    )?;
    mkdir_p("/sys/fs/cgroup")?;
    if let Err(e) = mount(
        "cgroup2",
        "/sys/fs/cgroup",
        "cgroup2",
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        "",
    ) {
        eprintln!("runt-agent: warning: cannot mount cgroup2: {e}");
    }
    for (link, target) in [
        ("/dev/fd", "/proc/self/fd"),
        ("/dev/stdin", "/proc/self/fd/0"),
        ("/dev/stdout", "/proc/self/fd/1"),
        ("/dev/stderr", "/proc/self/fd/2"),
    ] {
        let _ = std::os::unix::fs::symlink(target, link);
    }
    Ok(())
}

pub fn mkdir_p(p: &str) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(Path::new(p))
}

pub fn mount(
    src: &str,
    target: &str,
    fstype: &str,
    flags: libc::c_ulong,
    data: &str,
) -> io::Result<()> {
    let c = |s: &str| CString::new(s).unwrap();
    let (src_c, target_c, fstype_c, data_c) = (c(src), c(target), c(fstype), c(data));
    // SAFETY: all pointers are valid NUL-terminated strings (or null).
    let rc = unsafe {
        libc::mount(
            src_c.as_ptr(),
            target_c.as_ptr(),
            if fstype.is_empty() {
                std::ptr::null()
            } else {
                fstype_c.as_ptr()
            },
            flags,
            if data.is_empty() {
                std::ptr::null()
            } else {
                data_c.as_ptr().cast()
            },
        )
    };
    cvt(rc)
        .map(drop)
        .map_err(|e| io::Error::new(e.kind(), format!("mount {src} on {target} ({fstype}): {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cmdline() {
        let c = Cmdline::parse("console=hvc0 runt.name=brave-shrew panic=-1");
        assert_eq!(c.name.as_deref(), Some("brave-shrew"));
        assert!(Cmdline::parse("console=hvc0").name.is_none());
        let c = Cmdline::parse("runt.ip=100.96.0.2/30 runt.gw=100.96.0.1 runt.dns=100.96.0.1");
        assert_eq!(
            c.net.unwrap().gateway,
            std::net::Ipv4Addr::new(100, 96, 0, 1)
        );
        assert!(Cmdline::parse("runt.name=x").net.is_none());
    }
}
