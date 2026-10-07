//! runt-agent: PID 1 inside every runt VM.
//!
//! Boots the guest (overlay root on the base image + per-VM disk), then
//! serves the runt protocol on vsock: exec sessions, port forwarding,
//! services and shutdown.

mod boot;
mod exec;
mod mounts;
mod net;
mod ports;
mod services;
mod sys;

use std::thread;
use std::time::Duration;

fn main() {
    if std::process::id() != 1 {
        eprintln!("runt-agent must run as PID 1 inside a runt VM");
        std::process::exit(1);
    }
    if let Err(e) = sys::start_reaper() {
        fatal(&format!("cannot start reaper: {e}"));
    }
    match boot::early() {
        Ok(c) => eprintln!(
            "runt-agent: booted {}",
            c.name.as_deref().unwrap_or("(unnamed)")
        ),
        Err(e) => fatal(&format!("boot failed: {e}")),
    }
    let listener = match sys::vsock_listen(runt_proto::AGENT_PORT) {
        Ok(l) => l,
        Err(e) => fatal(&format!("cannot listen on vsock: {e}")),
    };
    eprintln!("runt-agent: ready");
    // Tell the host we're serving. Nobody may be listening (e.g. after a
    // guest reboot), which is fine.
    drop(sys::vsock_connect_host(runt_proto::READY_PORT));
    thread::spawn(ports::watch);
    loop {
        match sys::accept(&listener) {
            Ok(conn) => {
                thread::spawn(move || exec::serve(conn));
            }
            Err(e) => {
                eprintln!("runt-agent: accept: {e}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Stop everything and power off. libkrun exits on the host when we do.
pub fn shutdown() -> ! {
    eprintln!("runt-agent: shutting down");
    // Services first, each with its grace period, so that none is
    // restarted and a database on a volume gets to shut down cleanly.
    services::set(vec![]);
    // SAFETY: plain syscalls; kill(-1) from PID 1 signals everything but us.
    unsafe {
        libc::kill(-1, libc::SIGTERM);
    }
    // Give the rest up to half a second to exit.
    wait_for_others(Duration::from_millis(500));
    unsafe { libc::kill(-1, libc::SIGKILL) };
    wait_for_others(Duration::from_millis(100));
    // Volumes are unmounted cleanly (the root disk can't be; it is synced).
    let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    for line in mounts.lines() {
        let f: Vec<&str> = line.split(' ').collect();
        if f.len() > 2 && f[2] == "ext4" && f[0] != "/dev/vdb" {
            let target = std::ffi::CString::new(f[1].replace("\\040", " ")).unwrap_or_default();
            // SAFETY: valid C string.
            unsafe { libc::umount2(target.as_ptr(), 0) };
        }
    }
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_AUTOBOOT);
    }
    // reboot only returns on failure.
    std::process::exit(1)
}

fn wait_for_others(limit: Duration) {
    let t0 = std::time::Instant::now();
    while others_alive() && t0.elapsed() < limit {
        thread::sleep(Duration::from_millis(5));
    }
}

/// Whether any process but us, kernel threads and zombies is left.
fn others_alive() -> bool {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return false;
    };
    dir.flatten().any(|e| {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            return false;
        };
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        // "pid (comm) state ppid ...": comm may hold spaces and parens.
        let mut rest = stat
            .rsplit_once(')')
            .map_or("", |(_, r)| r)
            .split_whitespace();
        let (state, ppid) = (rest.next(), rest.next());
        pid > 2 && ppid != Some("2") && state.is_some_and(|s| s != "Z")
    })
}

fn fatal(msg: &str) -> ! {
    eprintln!("runt-agent: fatal: {msg}");
    thread::sleep(Duration::from_secs(1));
    // SAFETY: plain syscalls.
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_AUTOBOOT);
    }
    std::process::exit(1)
}
