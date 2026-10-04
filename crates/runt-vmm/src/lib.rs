//! VM backends for runt.
//!
//! L0 has one backend: libkrun (KVM on Linux). libkrun is loaded with
//! `dlopen` at runtime rather than linked, so the `runt` binary builds and
//! runs without it for commands that don't boot VMs, and so swapping in a
//! vendored static build later stays local to this crate.
//!
//! libkrun's `krun_start_enter` takes over the calling process and `exit()`s
//! when the guest powers off, so [`run`] never returns on success. Callers
//! run it in a dedicated supervisor process (`runt __vmm`).

use std::ffi::{CStr, CString, c_char, c_void};
use std::fmt;
use std::path::PathBuf;

/// Everything needed to boot one VM.
#[derive(Debug, Clone)]
pub struct VmConfig {
    pub vcpus: u8,
    pub mem_mib: u32,
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub cmdline: String,
    /// Block devices in order: the guest sees them as vda, vdb, ...
    pub disks: Vec<Disk>,
    /// Guest vsock port, and the host unix socket libkrun listens on for it.
    pub vsock_port: u32,
    pub vsock_socket: PathBuf,
    /// File the guest console is written to.
    pub console_log: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Disk {
    pub id: String,
    pub path: PathBuf,
    pub read_only: bool,
}

#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

const KRUN_KERNEL_FORMAT_ELF: u32 = 1;
const LIBKRUN_SONAME: &str = "libkrun.so.1";

/// Resolved libkrun 1.x entry points.
struct Krun {
    create_ctx: unsafe extern "C" fn() -> i32,
    set_vm_config: unsafe extern "C" fn(u32, u8, u32) -> i32,
    set_kernel: unsafe extern "C" fn(u32, *const c_char, u32, *const c_char, *const c_char) -> i32,
    add_disk: unsafe extern "C" fn(u32, *const c_char, *const c_char, bool) -> i32,
    disable_implicit_vsock: unsafe extern "C" fn(u32) -> i32,
    add_vsock: unsafe extern "C" fn(u32, u32) -> i32,
    add_vsock_port2: unsafe extern "C" fn(u32, u32, *const c_char, bool) -> i32,
    set_console_output: unsafe extern "C" fn(u32, *const c_char) -> i32,
    start_enter: unsafe extern "C" fn(u32) -> i32,
}

impl Krun {
    fn load() -> Result<Krun, Error> {
        let name = CString::new(LIBKRUN_SONAME).unwrap();
        // SAFETY: dlopen with a valid C string; the handle is intentionally
        // leaked because libkrun owns the process from start_enter onwards.
        let h = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if h.is_null() {
            return Err(Error(format!(
                "cannot load {LIBKRUN_SONAME}: {} (install libkrun, e.g. `dnf install libkrun`)",
                dlerror()
            )));
        }
        // SAFETY: each field's type matches the C signature in libkrun.h.
        unsafe {
            Ok(Krun {
                create_ctx: sym(h, "krun_create_ctx")?,
                set_vm_config: sym(h, "krun_set_vm_config")?,
                set_kernel: sym(h, "krun_set_kernel")?,
                add_disk: sym(h, "krun_add_disk")?,
                disable_implicit_vsock: sym(h, "krun_disable_implicit_vsock")?,
                add_vsock: sym(h, "krun_add_vsock")?,
                add_vsock_port2: sym(h, "krun_add_vsock_port2")?,
                set_console_output: sym(h, "krun_set_console_output")?,
                start_enter: sym(h, "krun_start_enter")?,
            })
        }
    }
}

/// Look up a function symbol and cast it to the function pointer type `F`.
///
/// # Safety
/// `F` must be an `extern "C" fn` type matching the symbol's real signature.
unsafe fn sym<F: Copy>(handle: *mut c_void, name: &str) -> Result<F, Error> {
    assert_eq!(size_of::<F>(), size_of::<*mut c_void>());
    let c = CString::new(name).unwrap();
    // SAFETY: valid handle and C string.
    let p = unsafe { libc::dlsym(handle, c.as_ptr()) };
    if p.is_null() {
        return Err(Error(format!("libkrun is missing {name} (need 1.19+)")));
    }
    // SAFETY: caller guarantees F is the matching function pointer type.
    Ok(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&p) })
}

fn dlerror() -> String {
    // SAFETY: dlerror returns a thread-local C string or null.
    let p = unsafe { libc::dlerror() };
    if p.is_null() {
        "unknown error".into()
    } else {
        // SAFETY: non-null pointer from dlerror is a valid C string.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

fn cstr(p: &std::path::Path) -> Result<CString, Error> {
    CString::new(p.as_os_str().as_encoded_bytes())
        .map_err(|_| Error(format!("path contains NUL: {}", p.display())))
}

fn check(what: &str, rc: i32) -> Result<(), Error> {
    if rc < 0 {
        let err = std::io::Error::from_raw_os_error(-rc);
        Err(Error(format!("libkrun {what} failed: {err}")))
    } else {
        Ok(())
    }
}

/// Check that libkrun can be loaded, without starting anything.
pub fn probe() -> Result<(), Error> {
    Krun::load().map(|_| ())
}

/// Configure and boot the VM. Only returns if setup fails; once the VM
/// starts, libkrun owns this process and exits it when the guest stops.
pub fn run(cfg: &VmConfig) -> Result<std::convert::Infallible, Error> {
    let k = Krun::load()?;
    let kernel = cstr(&cfg.kernel)?;
    let initramfs = cstr(&cfg.initramfs)?;
    let cmdline = CString::new(cfg.cmdline.as_str()).map_err(|_| Error("bad cmdline".into()))?;
    let sock = cstr(&cfg.vsock_socket)?;
    let console = cstr(&cfg.console_log)?;
    let disks: Vec<(CString, CString, bool)> = cfg
        .disks
        .iter()
        .map(|d| {
            Ok((
                CString::new(d.id.as_str()).map_err(|_| Error("bad disk id".into()))?,
                cstr(&d.path)?,
                d.read_only,
            ))
        })
        .collect::<Result<_, Error>>()?;

    // SAFETY: all pointers are valid NUL-terminated strings that outlive the
    // calls (libkrun copies what it keeps); ctx comes from create_ctx.
    unsafe {
        let ctx = (k.create_ctx)();
        check("create_ctx", ctx)?;
        let ctx = ctx as u32;
        check(
            "set_vm_config",
            (k.set_vm_config)(ctx, cfg.vcpus, cfg.mem_mib),
        )?;
        check(
            "set_kernel",
            (k.set_kernel)(
                ctx,
                kernel.as_ptr(),
                KRUN_KERNEL_FORMAT_ELF,
                initramfs.as_ptr(),
                cmdline.as_ptr(),
            ),
        )?;
        for (id, path, ro) in &disks {
            check(
                "add_disk",
                (k.add_disk)(ctx, id.as_ptr(), path.as_ptr(), *ro),
            )?;
        }
        // vsock without TSI: the guest gets no implicit host socket access.
        check("disable_implicit_vsock", (k.disable_implicit_vsock)(ctx))?;
        check("add_vsock", (k.add_vsock)(ctx, 0))?;
        check(
            "add_vsock_port2",
            (k.add_vsock_port2)(ctx, cfg.vsock_port, sock.as_ptr(), true),
        )?;
        check(
            "set_console_output",
            (k.set_console_output)(ctx, console.as_ptr()),
        )?;
        check("start_enter", (k.start_enter)(ctx))?;
    }
    Err(Error("libkrun start_enter returned unexpectedly".into()))
}
