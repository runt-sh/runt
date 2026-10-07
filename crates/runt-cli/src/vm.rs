//! VM lifecycle: create, start (spawning the supervisor), stop, remove.

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::client;
use crate::error::{CliError, Result};
use crate::mounts::{self, Mount};
use crate::ports;
use crate::state::{self, Egress, NetMode, Status, VmRecord};
use crate::{router, volumes};

/// Size of the sparse per-VM disk. Only written blocks use host space.
const UPPER_DISK_BYTES: u64 = 20 * 1024 * 1024 * 1024;
const BOOT_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

pub fn create(
    name: &str,
    cpus: u8,
    mem_mib: u32,
    net: NetMode,
    egress: Egress,
    mounts: Vec<Mount>,
) -> Result<VmRecord> {
    state::validate_name(name)?;
    if net == NetMode::None && !egress.is_default() {
        return Err(
            CliError::new("invalid_egress", "--allow and --allow-lan need networking")
                .hint("drop --net none, or drop the --allow options"),
        );
    }
    mounts::validate_set(&mounts)?;
    if let Some(fs) = mounts::cmdline(&mounts) {
        mounts::check_cmdline_len(&fs)?;
    }
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
        make_ext4(&dir.join("upper.ext4"), UPPER_DISK_BYTES)?;
        let rec = VmRecord {
            name: name.into(),
            cpus,
            mem_mib,
            created: state::now_rfc3339(),
            pid: None,
            net,
            mounts,
            egress,
            ..Default::default()
        };
        state::save(&rec)?;
        Ok(rec)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&dir);
    }
    result
}

/// Create a sparse file of `bytes` holding an empty ext4 filesystem.
pub fn make_ext4(path: &Path, bytes: u64) -> Result<()> {
    File::create(path)?.set_len(bytes)?;
    // Formatting uses the host's e2fsprogs; doing it in-process is a TODO.
    let st = Command::new(tool("mkfs.ext4")?)
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
        let _ = fs::remove_file(path);
        return Err(CliError::new(
            "mkfs_failed",
            format!("mkfs.ext4 failed on {}", path.display()),
        ));
    }
    Ok(())
}

/// An e2fsprogs program, which may live outside a user's PATH in sbin.
pub fn tool(name: &str) -> Result<PathBuf> {
    [
        PathBuf::from(name),
        Path::new("/usr/sbin").join(name),
        Path::new("/sbin").join(name),
    ]
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
        CliError::new("mkfs_missing", format!("{name} not found")).hint("install e2fsprogs")
    })
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
    check_devices(rec)?;
    let layers = rec.layers.iter().map(|k| state::layer_path(k));
    if let Some(missing) = layers.clone().find(|p| !p.is_file()) {
        let hint = match &rec.project {
            Some(dir) => format!("rebuild it with `runt up` in {}", dir.display()),
            None => format!("remove it with `runt rm {}`", rec.name),
        };
        return Err(CliError::new(
            "image_missing",
            format!(
                "VM {:?} needs {}, which is gone",
                rec.name,
                missing.display()
            ),
        )
        .hint(hint));
    }
    if let Some(v) = rec
        .volumes
        .iter()
        .find(|v| !volumes::file(&rec.name, &v.name).is_file())
    {
        let hint = match &rec.project {
            Some(dir) => format!("`runt up` in {} creates it again, empty", dir.display()),
            None => format!("remove the VM with `runt rm {}`", rec.name),
        };
        return Err(CliError::new(
            "volume_missing",
            format!(
                "VM {:?} needs its volume {:?}, which is gone",
                rec.name, v.name
            ),
        )
        .hint(hint));
    }
    if !rec.layers.is_empty() {
        link_layers(&state::vm_layers_dir(&rec.name), layers)?;
    }
    let dir = state::vm_dir(&rec.name);
    let sock = state::socket_path(&rec.name);
    // A fresh, private runtime dir per boot: the VM's sandbox is granted
    // exactly this directory.
    let rt = state::vm_runtime_dir(&rec.name);
    let _ = fs::remove_dir_all(&rt);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&rt)?;
    let _ = fs::remove_file(dir.join("console.log"));

    let ready_path = state::ready_socket_path(&rec.name);
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
    runt_net::prepare_supervisor(&mut cmd, &rec.egress.policy()?);
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
            let ms = t0.elapsed().as_millis();
            if !rec.services.is_empty() {
                client::set_services(rec)?;
            }
            if rec.http.is_some() {
                router::ensure();
            }
            Ok(ms)
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

    // Everything that needs the wider filesystem happens before we confine
    // ourselves: load libkrun (and its libraries), read the host's resolver.
    runt_vmm::probe().map_err(|e| CliError::new("libkrun_missing", e.to_string()))?;
    let dns = runt_net::host_resolver();
    confine(&rec, &assets, &dir);

    let mut cmdline = format!("console=hvc0 quiet panic=-1 runt.name={name}");
    // Lives as long as this process, which is as long as the VM.
    let net = match rec.net {
        NetMode::Nat => {
            let policy = rec.egress.policy()?;
            let log = state::egress_log_path(name);
            let net = runt_net::start(dns, &policy, Some(log)).map_err(|e| {
                CliError::new("net_failed", format!("cannot start networking: {e}"))
            })?;
            cmdline.push(' ');
            cmdline.push_str(&net.guest.cmdline());
            Some(net)
        }
        NetMode::None => None,
    };
    if let Some(fs) = mounts::cmdline(&rec.mounts) {
        cmdline.push(' ');
        cmdline.push_str(&fs);
    }
    if !rec.layers.is_empty() {
        cmdline.push_str(&format!(" runt.layers={}", rec.layers.len()));
    }
    if !rec.volumes.is_empty() {
        let paths: Vec<String> = rec.volumes.iter().map(|v| mounts::hex(&v.path)).collect();
        cmdline.push_str(&format!(" runt.vols={}", paths.join(",")));
    }
    mounts::check_cmdline_len(&cmdline)?;
    let mut disks = vec![
        runt_vmm::Disk {
            id: "base".into(),
            path: assets.image.clone(),
            read_only: true,
        },
        runt_vmm::Disk {
            id: "upper".into(),
            path: dir.join("upper.ext4"),
            read_only: false,
        },
    ];
    for (i, v) in rec.volumes.iter().enumerate() {
        disks.push(runt_vmm::Disk {
            id: format!("vol{i}"),
            path: volumes::file(name, &v.name),
            read_only: false,
        });
    }
    ports::start(name)
        .map_err(|e| CliError::new("ports_failed", format!("cannot start port forwarding: {e}")))?;
    let cfg = runt_vmm::VmConfig {
        vcpus: rec.cpus,
        mem_mib: rec.mem_mib,
        kernel: assets.kernel,
        initramfs: assets.initramfs,
        cmdline,
        disks,
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
            runt_vmm::VsockPort {
                port: runt_proto::EVENTS_PORT,
                socket: state::events_socket_path(name),
                host_connects: false,
            },
        ],
        console_log: dir.join("console.log"),
        shares: rec
            .mounts
            .iter()
            .enumerate()
            .map(|(i, m)| runt_vmm::Share {
                tag: mounts::tag(i),
                path: m.src.clone(),
                read_only: m.read_only,
            })
            .chain((!rec.layers.is_empty()).then(|| runt_vmm::Share {
                tag: LAYERS_TAG.into(),
                path: state::vm_layers_dir(name),
                read_only: true,
            }))
            .collect(),
        net: net.as_ref().map(|n| runt_vmm::NetDevice {
            fd: n.vmm_fd,
            mac: n.guest.mac,
            features: 0,
        }),
    };
    match runt_vmm::run(&cfg) {
        Ok(never) => match never {},
        Err(e) => Err(CliError::new("vmm_failed", e.to_string())),
    }
}

/// Confine this (supervisor) process to what the VM needs, and record what
/// was enforced for `runt ls`. Best effort: an unsupported kernel leaves the
/// VM running unconfined, with a warning.
fn confine(rec: &VmRecord, assets: &state::Assets, dir: &Path) {
    let status = match runt_sandbox::apply(&sandbox_policy(rec, assets, dir)) {
        Ok(s) => SandboxStatus {
            landlock: s.landlock.as_str().into(),
            landlock_abi: s.landlock_abi,
            seccomp: s.seccomp,
        },
        Err(e) => {
            eprintln!("runt: warning: cannot sandbox the VM process: {e}");
            SandboxStatus::default()
        }
    };
    eprintln!(
        "runt: sandbox: landlock={} (abi {}), seccomp={}",
        status.landlock, status.landlock_abi, status.seccomp
    );
    let _ = fs::write(
        state::sandbox_path(&rec.name),
        serde_json::to_vec(&status).unwrap(),
    );
}

/// The virtio-fs tag of the share holding a VM's image layers.
const LAYERS_TAG: &str = "layers";

/// Make `dir` hold exactly `files`, as `0.erofs`, `1.erofs`, ...: hard links
/// where possible (no copying, and a layer garbage-collected meanwhile stays
/// readable), copies otherwise. The guest mounts them in that order.
pub fn link_layers(dir: &Path, files: impl Iterator<Item = PathBuf>) -> Result<()> {
    let _ = fs::remove_dir_all(dir);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    for (i, src) in files.enumerate() {
        let dst = dir.join(format!("{i}.erofs"));
        if fs::hard_link(&src, &dst).is_err() {
            fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Refuse a VM with more disks and shares than the VMM can attach.
pub fn check_devices(rec: &VmRecord) -> Result<()> {
    let layers = usize::from(!rec.layers.is_empty());
    let net = usize::from(rec.net == NetMode::Nat);
    let used = 2 + layers + rec.volumes.len() + rec.mounts.len() + net;
    if used <= runt_vmm::DEVICE_SLOTS {
        return Ok(());
    }
    let room = runt_vmm::DEVICE_SLOTS - 2 - layers - net;
    let what = if rec.volumes.is_empty() {
        format!("{} shared folders", rec.mounts.len())
    } else {
        format!(
            "{} volumes and {} shared folders",
            rec.volumes.len(),
            rec.mounts.len()
        )
    };
    let with_image = if layers > 0 {
        " with a built image"
    } else {
        ""
    };
    Err(CliError::new(
        "too_many_devices",
        format!(
            "VM {:?} needs {what}; a VM{with_image} here can have {room} in total",
            rec.name
        ),
    )
    .hint(if rec.volumes.is_empty() {
        "share a common parent folder instead of several"
    } else {
        "share a common parent folder instead of several, or combine volumes"
    }))
}

/// Everything a VM's process may touch: its own state and runtime dirs, its
/// kernel, image and layers, its volumes, /dev/kvm, and the folders the
/// user shared.
pub fn sandbox_policy(rec: &VmRecord, assets: &state::Assets, dir: &Path) -> runt_sandbox::Policy {
    let (rw_shares, ro_shares): (Vec<&Mount>, Vec<&Mount>) =
        rec.mounts.iter().partition(|m| !m.read_only);
    let mut rw_dirs = vec![dir.to_path_buf(), state::vm_runtime_dir(&rec.name)];
    rw_dirs.extend(rw_shares.iter().map(|m| m.src.clone()));
    runt_sandbox::Policy {
        read_files: vec![
            assets.kernel.clone(),
            assets.initramfs.clone(),
            assets.image.clone(),
        ],
        rw_files: rec
            .volumes
            .iter()
            .map(|v| volumes::file(&rec.name, &v.name))
            .collect(),
        rw_dirs,
        ro_dirs: ro_shares
            .iter()
            .map(|m| m.src.clone())
            .chain((!rec.layers.is_empty()).then(|| state::vm_layers_dir(&rec.name)))
            .collect(),
        devices: vec![PathBuf::from("/dev/kvm")],
    }
}

/// What the supervisor's sandbox enforced (see `runt_sandbox::Status`).
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct SandboxStatus {
    /// "full", "partial" or "none".
    pub landlock: String,
    pub landlock_abi: i32,
    pub seccomp: bool,
}

impl SandboxStatus {
    pub fn read(name: &str) -> Option<SandboxStatus> {
        serde_json::from_slice(&fs::read(state::sandbox_path(name)).ok()?).ok()
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
    let _ = fs::remove_dir_all(state::vm_runtime_dir(&rec.name));
    let _ = fs::remove_dir_all(state::vm_layers_dir(&rec.name));
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
        // Killing the VM would lose writes its volumes haven't flushed.
        let force = rec.volumes.is_empty();
        stop(&mut rec, force)?;
    }
    fs::remove_dir_all(state::vm_dir(name))?;
    let _ = fs::remove_dir_all(state::vm_runtime_dir(name));
    let _ = fs::remove_dir_all(state::vm_layers_dir(name));
    Ok(())
}

pub fn console_log(name: &str) -> PathBuf {
    state::vm_dir(name).join("console.log")
}
