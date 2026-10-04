//! A seccomp-bpf denylist for the VM process.
//!
//! A denylist rather than an allowlist: libkrun's exact syscall set varies
//! by version and backend, and an allowlist that's wrong kills the VM. This
//! blocks what a VMM never needs and an escaped guest would want; refused
//! calls fail with EPERM (or ENOSYS for clone3, so libc falls back to clone,
//! whose flags we can inspect).

use std::io;

const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;

// struct seccomp_data offsets
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const OFF_ARG0: u32 = 16; // low 32 bits on little-endian

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xc000_00b7;

/// x32 ABI syscalls on x86_64 carry this bit; we refuse them all.
#[cfg(target_arch = "x86_64")]
const X32_BIT: u32 = 0x4000_0000;

const CLONE_NEW_MASK: u32 = (libc::CLONE_NEWNS
    | libc::CLONE_NEWUTS
    | libc::CLONE_NEWIPC
    | libc::CLONE_NEWUSER
    | libc::CLONE_NEWPID
    | libc::CLONE_NEWNET
    | libc::CLONE_NEWCGROUP) as u32;

fn denied() -> Vec<libc::c_long> {
    let mut v = vec![
        libc::SYS_execve,
        libc::SYS_execveat,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_pidfd_getfd,
        libc::SYS_kcmp,
        libc::SYS_kexec_load,
        libc::SYS_kexec_file_load,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_mount_setattr,
        libc::SYS_fsopen,
        libc::SYS_fsconfig,
        libc::SYS_fsmount,
        libc::SYS_fspick,
        libc::SYS_open_tree,
        libc::SYS_move_mount,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_open_by_handle_at,
        libc::SYS_name_to_handle_at,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_fanotify_init,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_reboot,
        libc::SYS_acct,
        libc::SYS_quotactl,
        libc::SYS_syslog,
        libc::SYS_settimeofday,
        libc::SYS_clock_settime,
        libc::SYS_clock_adjtime,
        libc::SYS_adjtimex,
        libc::SYS_sethostname,
        libc::SYS_setdomainname,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ];
    #[cfg(target_arch = "x86_64")]
    v.extend([
        libc::SYS_iopl,
        libc::SYS_ioperm,
        libc::SYS_uselib,
        libc::SYS_lookup_dcookie,
    ]);
    v
}

/// One classic-BPF instruction (`struct sock_filter`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct Insn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

const LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
const JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
const JGE_K: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
const JSET_K: u16 = 0x45; // BPF_JMP | BPF_JSET | BPF_K
const RET_K: u16 = 0x06; // BPF_RET | BPF_K

fn stmt(code: u16, k: u32) -> Insn {
    Insn {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> Insn {
    Insn { code, jt, jf, k }
}

fn errno(e: i32) -> u32 {
    SECCOMP_RET_ERRNO | (e as u32 & 0xffff)
}

/// Build the filter program.
fn program() -> Vec<Insn> {
    let mut p = vec![
        // Wrong architecture: kill (a syscall table we didn't vet).
        stmt(LD_W_ABS, OFF_ARCH),
        jump(JEQ_K, AUDIT_ARCH, 1, 0),
        stmt(RET_K, SECCOMP_RET_KILL_PROCESS),
        stmt(LD_W_ABS, OFF_NR),
    ];
    #[cfg(target_arch = "x86_64")]
    {
        p.push(jump(JGE_K, X32_BIT, 0, 1));
        p.push(stmt(RET_K, errno(libc::EPERM)));
    }
    for nr in denied() {
        p.push(jump(JEQ_K, nr as u32, 0, 1));
        p.push(stmt(RET_K, errno(libc::EPERM)));
    }
    // clone3 hides its flags in memory we can't inspect: make libc fall back
    // to clone(2), then refuse namespace flags there.
    p.push(jump(JEQ_K, libc::SYS_clone3 as u32, 0, 1));
    p.push(stmt(RET_K, errno(libc::ENOSYS)));
    p.push(jump(JEQ_K, libc::SYS_clone as u32, 0, 3));
    p.push(stmt(LD_W_ABS, OFF_ARG0));
    p.push(jump(JSET_K, CLONE_NEW_MASK, 0, 1));
    p.push(stmt(RET_K, errno(libc::EPERM)));
    p.push(stmt(RET_K, SECCOMP_RET_ALLOW));
    p
}

#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const Insn,
}

/// Install the filter on every thread of the process.
pub fn install() -> io::Result<bool> {
    let prog = program();
    let fprog = SockFprog {
        len: prog.len() as u16,
        filter: prog.as_ptr(),
    };
    // SAFETY: prctl/seccomp with valid arguments; fprog outlives the call.
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
        let rc = libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            libc::SECCOMP_FILTER_FLAG_TSYNC,
            &fprog as *const SockFprog,
        );
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_is_well_formed() {
        let p = program();
        assert!(p.len() < 4096, "BPF programs are limited to 4096 insns");
        assert_eq!(p.last(), Some(&stmt(RET_K, SECCOMP_RET_ALLOW)));
        // Every jump target must stay inside the program.
        for (i, insn) in p.iter().enumerate() {
            if insn.code & 0x07 == 0x05 {
                assert!(i + 1 + (insn.jt.max(insn.jf) as usize) < p.len());
            }
        }
    }

    #[test]
    fn denies_the_essentials() {
        let d = denied();
        for nr in [
            libc::SYS_execve,
            libc::SYS_ptrace,
            libc::SYS_mount,
            libc::SYS_unshare,
            libc::SYS_open_by_handle_at,
            libc::SYS_bpf,
        ] {
            assert!(d.contains(&nr));
        }
    }
}
