//! VM lifecycle: create, start (spawning the supervisor), stop, remove.

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::client;
use crate::error::{CliError, Result};
use crate::state::{self, Status, VmRecord};

/// Size of the sparse per-VM disk. Only written blocks use host space.
const UPPER_DISK_BYTES: u64 = 20 * 1024 * 1024 * 1024;
const BOOT_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

pub fn create(name: &str, cpus: u8, mem_mib: u32) -> Result<VmRecord> {
    state::validate_name(name)?;
    state::assets()?;
    let dir = state::vm_dir(name);
    if dir.exists() {
        return Err(
            CliError::new("vm_exists", format!("a VM named {name:?} already exists")).hint(
                format!("pick another name, or remove it with `runt rm -f {name}`"),
            ),
        );
    }
    fs::create_dir_all(&dir)?;
    let result = (|| {
        make_upper_disk(&dir.join("upper.ext4"))?;
        let rec = VmRecord {
            name: name.into(),
            cpus,
            mem_mib,
            created: state::now_rfc3339(),
            pid: None,
        };
        state::save(&rec)?;
        Ok(rec)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&dir);
    }
    result
}

fn make_upper_disk(path: &Path) -> Result<()> {
    File::create(path)?.set_len(UPPER_DISK_BYTES)?;
    // L0 uses the host's mkfs.ext4; formatting in-process is a TODO.
    let mkfs = ["mkfs.ext4", "/usr/sbin/mkfs.ext4", "/sbin/mkfs.ext4"]
        .into_iter()
        .find(|p| {
            Command::new(p)
                .arg("-V")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok()
        })
        .ok_or_else(|| {
            CliError::new("mkfs_missing", "mkfs.ext4 not found").hint("install e2fsprogs")
        })?;
    let st = Command::new(mkfs)
        .args([
            "-q",
            "-F",
            "-E",
            "lazy_itable_init=1,lazy_journal_init=1,root_owner=0:0",
        ])
        .arg(path)
        .stdout(Stdio::null())
        .status()?;
    if !st.success() {
        return Err(CliError::new(
            "mkfs_failed",
            format!("mkfs.ext4 failed on {}", path.display()),
        ));
    }
    Ok(())
}

/// Boot a stopped VM. Returns the time until the agent answered, in ms.
pub fn start(rec: &mut VmRecord) -> Result<u128> {
    if state::status(rec) == Status::Running {
        return Err(CliError::new(
            "vm_running",
            format!("VM {:?} is already running", rec.name),
        ));
    }
    runt_vmm::probe().map_err(|e| CliError::new("libkrun_missing", e.to_string()))?;
    let dir = state::vm_dir(&rec.name);
    let sock = state::socket_path(&rec.name);
    if let Some(parent) = sock.parent() {
        fs::create_dir_all(parent)?;
    }
    state::remove_socket(&sock);
    let _ = fs::remove_file(dir.join("console.log"));

    let ready_path = state::ready_socket_path(&rec.name);
    state::remove_socket(&ready_path);
    let ready = UnixListener::bind(&ready_path)?;

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("vmm.log"))?;
    let t0 = Instant::now();
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("__vmm")
        .arg(&rec.name)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe; detaches the supervisor from our
    // session so it survives the terminal closing.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    rec.pid = Some(child.id());
    state::save(rec)?;

    let result = wait_ready(&ready, &mut child, t0);
    drop(ready);
    state::remove_socket(&state::ready_socket_path(&rec.name));
    match result {
        Ok(()) => {
            // The agent is serving; confirm with a real handshake.
            client::connect(&rec.name, &sock)?;
            Ok(t0.elapsed().as_millis())
        }
        Err(why) => {
            if child.try_wait().ok().flatten().is_none() {
                // SAFETY: plain kill(2) on our own child.
                unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) };
                let _ = child.wait();
            }
            rec.pid = None;
            state::save(rec)?;
            Err(boot_failed(&rec.name, &why))
        }
    }
}

/// Wait for the agent to connect to the ready socket, the VM to die, or the
/// boot timeout, whichever comes first.
fn wait_ready(
    ready: &UnixListener,
    child: &mut Child,
    t0: Instant,
) -> std::result::Result<(), String> {
    loop {
        let mut pfd = libc::pollfd {
            fd: ready.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut pfd, 1, 20) } > 0 && ready.accept().is_ok() {
            return Ok(());
        }
        if let Ok(Some(st)) = child.try_wait() {
            return Err(format!("the VM exited during boot ({st})"));
        }
        if t0.elapsed() > BOOT_TIMEOUT {
            return Err("timed out waiting for the VM to boot".into());
        }
    }
}

fn boot_failed(name: &str, why: &str) -> CliError {
    let dir = state::vm_dir(name);
    CliError::new("boot_failed", format!("VM {name:?} failed to start: {why}")).hint(format!(
        "see {} and {}",
        dir.join("console.log").display(),
        dir.join("vmm.log").display()
    ))
}

/// The hidden `runt __vmm <name>` entry point: become the VM. libkrun takes
/// over this process and exits it when the guest powers off.
pub fn supervise(name: &str) -> Result<()> {
    let rec = state::load(name)?;
    let assets = state::assets()?;
    let dir = state::vm_dir(name);
    let cfg = runt_vmm::VmConfig {
        vcpus: rec.cpus,
        mem_mib: rec.mem_mib,
        kernel: assets.kernel,
        initramfs: assets.initramfs,
        cmdline: format!("console=hvc0 quiet panic=-1 runt.name={name}"),
        disks: vec![
            runt_vmm::Disk {
                id: "base".into(),
                path: assets.image,
                read_only: true,
            },
            runt_vmm::Disk {
                id: "upper".into(),
                path: dir.join("upper.ext4"),
                read_only: false,
            },
        ],
        vsock_ports: vec![
            runt_vmm::VsockPort {
                port: runt_proto::AGENT_PORT,
                socket: state::socket_path(name),
                host_connects: true,
            },
            runt_vmm::VsockPort {
                port: runt_proto::READY_PORT,
                socket: state::ready_socket_path(name),
                host_connects: false,
            },
        ],
        console_log: dir.join("console.log"),
    };
    match runt_vmm::run(&cfg) {
        Ok(never) => match never {},
        Err(e) => Err(CliError::new("vmm_failed", e.to_string())),
    }
}

pub fn stop(rec: &mut VmRecord, force: bool) -> Result<()> {
    let Some(pid) = rec.pid.filter(|_| state::status(rec) == Status::Running) else {
        rec.pid = None;
        state::save(rec)?;
        return Ok(());
    };
    let sock = state::socket_path(&rec.name);
    if !force && let Some(conn) = client::try_connect(&sock, Duration::from_secs(2)) {
        client::shutdown(&conn)?;
    }
    let deadline = Instant::now() + if force { Duration::ZERO } else { STOP_TIMEOUT };
    while state::is_supervisor(pid, &rec.name) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    for sig in [libc::SIGTERM, libc::SIGKILL] {
        if !state::is_supervisor(pid, &rec.name) {
            break;
        }
        // SAFETY: plain kill(2); pid verified to be this VM's supervisor.
        unsafe { libc::kill(pid as libc::pid_t, sig) };
        let deadline = Instant::now() + Duration::from_secs(2);
        while state::is_supervisor(pid, &rec.name) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
    }
    state::remove_socket(&sock);
    rec.pid = None;
    state::save(rec)?;
    Ok(())
}

pub fn remove(name: &str, force: bool) -> Result<()> {
    let mut rec = state::load(name)?;
    if state::status(&rec) == Status::Running {
        if !force {
            return Err(
                CliError::new("vm_running", format!("VM {name:?} is running")).hint(format!(
                    "stop it first with `runt stop {name}`, or use `runt rm -f {name}`"
                )),
            );
        }
        stop(&mut rec, true)?;
    }
    fs::remove_dir_all(state::vm_dir(name))?;
    state::remove_socket(&state::socket_path(name));
    Ok(())
}

pub fn console_log(name: &str) -> PathBuf {
    state::vm_dir(name).join("console.log")
}
