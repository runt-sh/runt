//! On-disk VM state. There is no daemon: each VM is a directory plus, while
//! running, a supervisor process and a socket.
//!
//! ```text
//! $XDG_STATE_HOME/runt/vms/<name>/vm.json      record (this file's VmRecord)
//!                                 upper.ext4   per-VM writable disk
//!                                 console.log  guest console
//!                                 vmm.log      supervisor stderr
//! $XDG_RUNTIME_DIR/runt/<name>.sock            agent socket (libkrun listens)
//! $XDG_CACHE_HOME/runt/{vmlinux,initramfs.cpio,images/base.erofs}
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{CliError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmRecord {
    pub name: String,
    pub cpus: u8,
    pub mem_mib: u32,
    pub created: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub net: NetMode,
}

/// How a VM is connected to the outside world.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum NetMode {
    /// Outbound NAT through runt's userspace network stack
    #[default]
    Nat,
    /// No network device at all
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Running,
    Stopped,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Stopped => "stopped",
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(fallback),
    }
}

pub fn vms_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("runt/vms")
}

pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join("runt")
}

pub fn vm_dir(name: &str) -> PathBuf {
    vms_dir().join(name)
}

/// Sockets live under the runtime dir: short paths (unix sockets are limited
/// to 108 bytes) on a tmpfs that is cleaned at logout.
pub fn socket_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.sock"))
}

/// Socket the CLI listens on during boot; the agent connects when ready.
pub fn ready_socket_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.ready"))
}

/// Socket the supervisor listens on for the agent's events (listening ports).
pub fn events_socket_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.events"))
}

/// Current port forwards, written by the supervisor.
pub fn ports_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.ports.json"))
}

fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v).join("runt"),
        // SAFETY: getuid never fails.
        _ => PathBuf::from(format!("/tmp/runt-{}", unsafe { libc::getuid() })),
    }
}

pub struct Assets {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub image: PathBuf,
}

pub fn assets() -> Result<Assets> {
    let cache = cache_dir();
    let pick =
        |var: &str, default: PathBuf| std::env::var_os(var).map(PathBuf::from).unwrap_or(default);
    let a = Assets {
        kernel: pick("RUNT_KERNEL", cache.join("vmlinux")),
        initramfs: pick("RUNT_INITRAMFS", cache.join("initramfs.cpio")),
        image: pick("RUNT_IMAGE", cache.join("images/base.erofs")),
    };
    for p in [&a.kernel, &a.initramfs, &a.image] {
        if !p.exists() {
            return Err(CliError::new(
                "assets_missing",
                format!("missing VM asset: {}", p.display()),
            )
            .hint("build them with `make assets` in the runt repo"));
        }
    }
    Ok(a)
}

pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 48
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err(
            CliError::new("invalid_name", format!("invalid VM name {name:?}"))
                .hint("use 1-48 lowercase letters, digits and dashes"),
        )
    }
}

pub fn load(name: &str) -> Result<VmRecord> {
    let path = vm_dir(name).join("vm.json");
    let data = fs::read(&path).map_err(|_| {
        CliError::new("vm_not_found", format!("no VM named {name:?}"))
            .hint("list VMs with `runt ls`")
    })?;
    serde_json::from_slice(&data)
        .map_err(|e| CliError::new("state_corrupt", format!("{}: {e}", path.display())))
}

pub fn save(rec: &VmRecord) -> Result<()> {
    let dir = vm_dir(&rec.name);
    let tmp = dir.join("vm.json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(rec).unwrap())?;
    fs::rename(&tmp, dir.join("vm.json"))?;
    Ok(())
}

pub fn list() -> Result<Vec<VmRecord>> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(vms_dir()) else {
        return Ok(out);
    };
    for e in entries.flatten() {
        if let Some(name) = e.file_name().to_str()
            && let Ok(rec) = load(name)
        {
            out.push(rec);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// A VM is running if its recorded supervisor pid is alive and really is the
/// supervisor for this VM (pids get reused).
pub fn status(rec: &VmRecord) -> Status {
    match rec.pid {
        Some(pid) if is_supervisor(pid, &rec.name) => Status::Running,
        _ => Status::Stopped,
    }
}

pub fn is_supervisor(pid: u32, name: &str) -> bool {
    let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    args.len() >= 3 && args[1] == b"__vmm" && args[2] == name.as_bytes()
}

pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339(secs)
}

/// Format unix seconds as an RFC 3339 UTC timestamp.
fn rfc3339(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

pub fn remove_socket(path: &Path) {
    let _ = fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_timestamps() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_791_072_000), "2026-10-04T00:00:00Z");
    }

    #[test]
    fn validates_names() {
        assert!(validate_name("brave-shrew").is_ok());
        assert!(validate_name("vm1").is_ok());
        for bad in ["", "-x", "x-", "Upper", "a b", "a/b", &"x".repeat(49)] {
            assert!(validate_name(bad).is_err(), "{bad:?} should be invalid");
        }
    }
}
