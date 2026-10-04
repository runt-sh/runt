//! runt-agent: PID 1 inside every runt VM.
//!
//! Boots the guest (overlay root on the base image + per-VM disk), then
//! serves the runt protocol on vsock: exec sessions and shutdown.

mod boot;
mod exec;
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
    // SAFETY: plain syscalls; kill(-1) from PID 1 signals everything but us.
    unsafe {
        libc::kill(-1, libc::SIGTERM);
    }
    thread::sleep(Duration::from_millis(500));
    unsafe {
        libc::kill(-1, libc::SIGKILL);
        libc::sync();
        libc::reboot(libc::RB_AUTOBOOT);
    }
    // reboot only returns on failure.
    std::process::exit(1)
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
