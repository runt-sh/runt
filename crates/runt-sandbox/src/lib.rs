//! Confining a runt VM's host process.
//!
//! libkrun runs the guest's devices inside our process and treats guest and
//! VMM as one trust domain: a guest that escapes into the VMM gets whatever
//! this process can do. So before the VM starts, the supervisor confines
//! itself:
//!
//! - **Landlock** limits filesystem access to the VM's own state and runtime
//!   directories, its kernel and image (read-only), `/dev/kvm`, and the
//!   folders the user chose to share. Connecting to other unix sockets
//!   (other VMs' agents, the D-Bus session bus, container engine sockets)
//!   is denied, as are abstract unix sockets and signalling processes
//!   outside the sandbox.
//! - **seccomp** refuses syscalls a VMM never needs but an attacker would
//!   want: exec, ptrace, mount, new namespaces, kernel modules, bpf, ...
//!
//! Network access is deliberately not restricted here: the userspace network
//! stack relays guest traffic from this process. Egress policy lives there.
//!
//! Both layers are best effort: on kernels without (full) Landlock the VM
//! still runs and [`Status`] says what was enforced.

use std::io;
use std::path::PathBuf;

use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, RestrictSelfAttr, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus, Scope, path_beneath_rules,
};

mod seccomp;

/// The newest Landlock ABI we know how to use.
const ABI_TARGET: ABI = ABI::V9;

/// What the VM process may touch.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// Individual files readable (kernel, initramfs, base image).
    pub read_files: Vec<PathBuf>,
    /// Individual files readable and writable (volume disks).
    pub rw_files: Vec<PathBuf>,
    /// Directories with full read/write access beneath them (no exec).
    pub rw_dirs: Vec<PathBuf>,
    /// Directories readable beneath them.
    pub ro_dirs: Vec<PathBuf>,
    /// Device nodes usable read/write with ioctls (`/dev/kvm`).
    pub devices: Vec<PathBuf>,
}

/// What was actually enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub landlock: Enforcement,
    /// Landlock ABI version the kernel offers (0 = unavailable).
    pub landlock_abi: i32,
    pub seccomp: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enforcement {
    Full,
    Partial,
    None,
}

impl Enforcement {
    pub fn as_str(self) -> &'static str {
        match self {
            Enforcement::Full => "full",
            Enforcement::Partial => "partial",
            Enforcement::None => "none",
        }
    }
}

/// Confine the calling process (all of its threads). Irreversible.
pub fn apply(policy: &Policy) -> io::Result<Status> {
    let landlock = landlock(policy).map_err(io::Error::other)?;
    let seccomp = seccomp::install()?;
    Ok(Status {
        landlock,
        landlock_abi: landlock_abi(),
        seccomp,
    })
}

fn landlock(p: &Policy) -> Result<Enforcement, landlock::RulesetError> {
    let all = AccessFs::from_all(ABI_TARGET);
    let rw = all & !AccessFs::Execute;
    let ro = AccessFs::ReadFile | AccessFs::ReadDir;
    let device = AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::IoctlDev;

    let status = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(all)?
        .scope(Scope::from_all(ABI_TARGET))?
        .create()?
        .add_rules(path_beneath_rules(&p.read_files, AccessFs::ReadFile))?
        .add_rules(path_beneath_rules(
            &p.rw_files,
            AccessFs::ReadFile | AccessFs::WriteFile,
        ))?
        .add_rules(path_beneath_rules(&p.ro_dirs, ro))?
        .add_rules(path_beneath_rules(&p.rw_dirs, rw))?
        .add_rules(path_beneath_rules(&p.devices, device))?
        .all_threads(true)?
        .restrict_self()?;
    Ok(match status.ruleset {
        RulesetStatus::FullyEnforced => Enforcement::Full,
        RulesetStatus::PartiallyEnforced => Enforcement::Partial,
        RulesetStatus::NotEnforced => Enforcement::None,
    })
}

fn landlock_abi() -> i32 {
    // landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)
    // SAFETY: querying the ABI version takes no pointers.
    let v = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0usize,
            1u32,
        )
    };
    v.max(0) as i32
}
