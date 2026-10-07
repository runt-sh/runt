//! Volumes: a project's persistent data. Each is a sparse ext4 file that
//! belongs to the project, not to its VM, so it survives the VM being
//! recreated from a new image (and `runt down --rm`).
//!
//! ```text
//! $XDG_STATE_HOME/runt/volumes/<vm>/<name>.ext4
//!                                   project     the runt.toml dir that owns them
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::error::{CliError, Result};
use crate::state;
use crate::vm;

pub const MAX_VOLUMES: usize = 8;
pub const DEFAULT_SIZE_MIB: u64 = 1024;
const MIN_SIZE_MIB: u64 = 64;
const MAX_SIZE_MIB: u64 = 16 << 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Volume {
    pub name: String,
    /// Where it is mounted in the VM.
    pub path: String,
    pub size_mib: u64,
}

/// Parse sizes like "512M", "10G" or "1T" into MiB.
pub fn parse_size(s: &str) -> std::result::Result<u64, String> {
    let s = s.trim();
    let (num, mult) = match s.char_indices().last() {
        Some((i, 'T' | 't')) => (&s[..i], 1 << 20),
        Some((i, 'G' | 'g')) => (&s[..i], 1024),
        Some((i, 'M' | 'm')) => (&s[..i], 1),
        _ => return Err(format!("{s:?} needs a unit, like 512M or 10G")),
    };
    let mib = num
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .ok_or_else(|| format!("invalid size {s:?} (try 512M or 10G)"))?;
    if !(MIN_SIZE_MIB..=MAX_SIZE_MIB).contains(&mib) {
        return Err(format!("{s:?} is out of range (64M to 16T)"));
    }
    Ok(mib)
}

/// "1G", "512M": the shortest exact form.
pub fn format_size(mib: u64) -> String {
    match mib {
        m if m % (1 << 20) == 0 => format!("{}T", m >> 20),
        m if m % 1024 == 0 => format!("{}G", m / 1024),
        m => format!("{m}M"),
    }
}

/// A volume may go anywhere but over a system directory itself (data under
/// /var/lib is fine) or into the kernel's and runtime filesystems.
pub fn validate_path(path: &str) -> std::result::Result<(), String> {
    let p = Path::new(path);
    let virt = ["/proc", "/sys", "/dev", "/run"];
    if p == Path::new("/")
        || crate::mounts::RESERVED.iter().any(|r| p == Path::new(r))
        || virt.iter().any(|v| p.starts_with(v))
    {
        return Err(format!("{path} is one of the VM's system directories"));
    }
    Ok(())
}

pub fn dir(vm: &str) -> PathBuf {
    state::volumes_dir().join(vm)
}

pub fn file(vm: &str, name: &str) -> PathBuf {
    dir(vm).join(format!("{name}.ext4"))
}

fn owner_path(vm: &str) -> PathBuf {
    dir(vm).join("project")
}

/// The project that owns `vm`'s volumes, if any exist.
pub fn owner(vm: &str) -> Option<PathBuf> {
    fs::read_to_string(owner_path(vm)).ok().map(PathBuf::from)
}

/// Check that `project` may use these volumes as declared: they aren't
/// another project's, and none would shrink. Call before touching the VM.
pub fn check(vm: &str, project: &Path, vols: &[Volume]) -> Result<()> {
    if vols.is_empty() {
        return Ok(());
    }
    if let Some(p) = owner(vm)
        && p != project
    {
        return Err(CliError::new(
            "volume_exists",
            format!(
                "volumes named after {vm:?} belong to the project in {}",
                p.display()
            ),
        )
        .hint(format!(
            "change `name` in runt.toml, or delete them with `runt volume rm {vm}`"
        )));
    }
    for v in vols {
        let have = fs::metadata(file(vm, &v.name)).map_or(0, |m| m.len());
        if v.size_mib << 20 < have {
            return Err(CliError::new(
                "volume_shrink",
                format!(
                    "volume {:?} is {}; it can't shrink to {}",
                    v.name,
                    format_size(have >> 20),
                    format_size(v.size_mib)
                ),
            )
            .hint(format!(
                "set its size back, or delete it (and its data) with `runt volume rm {vm} {}`",
                v.name
            )));
        }
    }
    Ok(())
}

/// Make each volume's file exist with at least its size: new ones are
/// created and formatted, grown ones resized. The VM must be stopped.
pub fn prepare(vm: &str, project: &Path, vols: &[Volume]) -> Result<()> {
    check(vm, project, vols)?;
    if vols.is_empty() {
        return Ok(());
    }
    if owner(vm).is_none() {
        fs::create_dir_all(dir(vm))?;
        fs::write(owner_path(vm), project.as_os_str().as_encoded_bytes())?;
    }
    for v in vols {
        let path = file(vm, &v.name);
        let want = v.size_mib << 20;
        match fs::metadata(&path) {
            Err(_) => vm::make_ext4(&path, want)?,
            Ok(m) if m.len() < want => grow(&path, want)?,
            Ok(_) => {}
        }
    }
    Ok(())
}

fn grow(path: &Path, bytes: u64) -> Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .set_len(bytes)?;
    // resize2fs insists on a freshly checked filesystem. e2fsck -p exits 1
    // when it fixed something harmless.
    let fsck = Command::new(vm::tool("e2fsck")?)
        .args(["-f", "-p"])
        .arg(path)
        .stdout(Stdio::null())
        .status()?;
    let resize = Command::new(vm::tool("resize2fs")?)
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if fsck.code().is_none_or(|c| c > 1) || !resize.success() {
        return Err(CliError::new(
            "volume_resize_failed",
            format!("cannot grow {}", path.display()),
        )
        .hint(format!("check it with `e2fsck -f {}`", path.display())));
    }
    Ok(())
}

/// A volume file on disk, for `runt volume ls`.
pub struct OnDisk {
    pub vm: String,
    pub name: String,
    pub size: u64,
    /// Bytes actually used on the host (the file is sparse).
    pub used: u64,
}

pub fn list() -> Vec<OnDisk> {
    use std::os::unix::fs::MetadataExt;
    let mut out = Vec::new();
    let Ok(vms) = fs::read_dir(state::volumes_dir()) else {
        return out;
    };
    for d in vms.flatten() {
        let Ok(files) = fs::read_dir(d.path()) else {
            continue;
        };
        for f in files.flatten() {
            let name = f.file_name().to_string_lossy().into_owned();
            if let (Some(name), Ok(m)) = (name.strip_suffix(".ext4"), f.metadata()) {
                out.push(OnDisk {
                    vm: d.file_name().to_string_lossy().into_owned(),
                    name: name.into(),
                    size: m.len(),
                    used: m.blocks() * 512,
                });
            }
        }
    }
    out.sort_unstable_by(|a, b| (&a.vm, &a.name).cmp(&(&b.vm, &b.name)));
    out
}

/// Delete `vm`'s volume `name`, or all of its volumes. Returns the names
/// deleted. Refused while the VM runs.
pub fn remove(vm: &str, name: Option<&str>) -> Result<Vec<String>> {
    if state::vm_dir(vm).exists() && state::status(&state::load(vm)?) == state::Status::Running {
        return Err(CliError::new(
            "vm_running",
            format!("VM {vm:?} is running and using its volumes"),
        )
        .hint("stop it first with `runt down` (or `runt stop`)"));
    }
    let names: Vec<String> = match name {
        Some(n) => vec![n.to_string()],
        None => list()
            .into_iter()
            .filter(|v| v.vm == vm)
            .map(|v| v.name)
            .collect(),
    };
    for n in &names {
        fs::remove_file(file(vm, n)).map_err(|_| {
            CliError::new("volume_not_found", format!("VM {vm:?} has no volume {n:?}"))
                .hint("list volumes with `runt volume ls`")
        })?;
    }
    if list().iter().all(|v| v.vm != vm) {
        let _ = fs::remove_dir_all(dir(vm));
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1G"), Ok(1024));
        assert_eq!(parse_size("512m"), Ok(512));
        assert_eq!(parse_size("2T"), Ok(2 << 20));
        for bad in ["1", "10M", "lots", "G", "99999T"] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
        assert_eq!(format_size(1024), "1G");
        assert_eq!(format_size(1536), "1536M");
        assert_eq!(format_size(1 << 20), "1T");
    }
}
