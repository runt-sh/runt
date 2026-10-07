//! Local URLs: `http://NAME.runt.localhost:PORT` for each running VM with an
//! HTTP port (`[http] port` in runt.toml). Browsers and curl resolve
//! `*.localhost` to loopback by themselves, so no DNS setup is needed, and
//! each app gets its own origin (cookies, storage) however many there are.
//!
//! One small router process per user (`runt __router`) listens on loopback,
//! reads the Host header of each new connection, and splices it to that
//! VM's HTTP port through the VM's agent, so it doesn't matter which host
//! port the forward got. `runt up` and `runt start` start it when needed;
//! it exits once no running VM has an HTTP port. Its address is in
//! `$XDG_RUNTIME_DIR/runt/router.json`.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::error::{CliError, Result};
use crate::ports;
use crate::state::{self, Status, VmRecord};

/// Ports to try, in order: 80 gives the cleanest URLs where it's allowed
/// (macOS; Linux with a lowered `net.ipv4.ip_unprivileged_port_start`).
const PORTS: [u16; 2] = [80, 7080];
const SUFFIX: &str = ".runt.localhost";
/// Request heads larger than this are refused.
const MAX_HEAD: usize = 16 * 1024;
/// How often the router checks whether it is still needed.
const IDLE_CHECK: Duration = Duration::from_secs(15);

fn info_path() -> PathBuf {
    state::runtime_dir().join("router.json")
}

/// The router's port, if it is running.
pub fn port() -> Option<u16> {
    let v: serde_json::Value = serde_json::from_slice(&fs::read(info_path()).ok()?).ok()?;
    let pid = v["pid"].as_u64()?;
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let is_router = cmdline.split(|b| *b == 0).nth(1) == Some(b"__router");
    is_router
        .then(|| v["port"].as_u64())
        .flatten()
        .map(|p| p as u16)
}

/// The VM's URL, when it has an HTTP port and the router is running.
pub fn url(rec: &VmRecord) -> Option<String> {
    rec.http?;
    Some(format_url(&rec.name, port()?))
}

fn format_url(name: &str, port: u16) -> String {
    match port {
        80 => format!("http://{name}{SUFFIX}"),
        p => format!("http://{name}{SUFFIX}:{p}"),
    }
}

/// Start the router unless it is running; returns its port. Failing to
/// start it only costs the pretty URL, so this warns rather than fails.
pub fn ensure() -> Option<u16> {
    if let Some(p) = port() {
        return Some(p);
    }
    if let Err(e) = spawn() {
        eprintln!("runt: warning: cannot start the URL router: {e}");
        return None;
    }
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(2) {
        if let Some(p) = port() {
            return Some(p);
        }
        thread::sleep(Duration::from_millis(10));
    }
    eprintln!(
        "runt: warning: the URL router didn't start; see {}",
        state::runtime_dir().join("router.log").display()
    );
    None
}

fn spawn() -> std::io::Result<()> {
    let rt = state::runtime_dir();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&rt)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(rt.join("router.log"))?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("__router")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe; the router outlives this command.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    // Reap it if it exits while we're still around (the MCP server is).
    thread::spawn(move || child.wait());
    Ok(())
}

/// The hidden `runt __router` entry point.
pub fn serve() -> Result<()> {
    let rt = state::runtime_dir();
    // One router per user: whoever holds the lock is it.
    let lock = File::create(rt.join("router.lock"))?;
    // SAFETY: flock(2) on a file we own.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(());
    }
    let (listener, port) = bind()?;
    // Browsers may try ::1 first; serve it too when we can.
    let v6 = TcpListener::bind(("::1", port)).ok();
    confine();
    write_info(port)?;
    eprintln!("runt: router listening on {}", format_url("NAME", port));
    if let Some(v6) = v6 {
        thread::spawn(move || accept(v6, port));
    }
    thread::spawn(move || accept(listener, port));
    let mut idle = 0;
    loop {
        thread::sleep(IDLE_CHECK);
        idle = if routes().is_empty() { idle + 1 } else { 0 };
        if idle >= 2 {
            // Gone from router.json first, so that nobody starts relying on
            // us, then a last look in case someone already did.
            let _ = fs::remove_file(info_path());
            if routes().is_empty() {
                return Ok(());
            }
            write_info(port)?;
            idle = 0;
        }
    }
}

fn bind() -> Result<(TcpListener, u16)> {
    let ports = match std::env::var("RUNT_HTTP_PORT") {
        Ok(p) => vec![p.parse().map_err(|_| {
            CliError::new("invalid_port", format!("RUNT_HTTP_PORT={p:?} isn't a port"))
        })?],
        Err(_) => PORTS.to_vec(),
    };
    let mut last = None;
    for p in ports {
        match TcpListener::bind(("127.0.0.1", p)) {
            Ok(l) => return Ok((l, p)),
            Err(e) => last = Some(format!("port {p}: {e}")),
        }
    }
    Err(CliError::new(
        "router_failed",
        format!(
            "cannot listen for local URLs ({})",
            last.unwrap_or_default()
        ),
    ))
}

fn write_info(port: u16) -> Result<()> {
    let path = info_path();
    let tmp = path.with_extension("json.tmp");
    fs::write(
        &tmp,
        json!({ "pid": std::process::id(), "port": port }).to_string(),
    )?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// The router parses requests from anything on this machine (including
/// web pages), so it confines itself like a VM's process: it reads VM
/// records and /proc, and talks to agents in the runtime dir. Nothing else.
fn confine() {
    let policy = runt_sandbox::Policy {
        rw_dirs: vec![state::runtime_dir()],
        ro_dirs: vec![state::vms_dir(), PathBuf::from("/proc")],
        ..Default::default()
    };
    match runt_sandbox::apply(&policy) {
        Ok(s) => eprintln!(
            "runt: router sandbox: landlock={} (abi {}), seccomp={}",
            s.landlock.as_str(),
            s.landlock_abi,
            s.seccomp
        ),
        Err(e) => eprintln!("runt: warning: cannot sandbox the router: {e}"),
    }
}

/// Running VMs with an HTTP port.
fn routes() -> Vec<VmRecord> {
    let mut vms = state::list().unwrap_or_default();
    vms.retain(|r| r.http.is_some() && state::status(r) == Status::Running);
    vms
}

fn accept(listener: TcpListener, port: u16) {
    for conn in listener.incoming() {
        let Ok(tcp) = conn else { continue };
        thread::spawn(move || handle(tcp, port));
    }
}

fn handle(mut tcp: TcpStream, port: u16) {
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(10)));
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        if head.len() > MAX_HEAD {
            return respond(&mut tcp, 431, "request header too large\n");
        }
        match tcp.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    let _ = tcp.set_read_timeout(None);
    let Some(name) = host(&head).and_then(|h| vm_name(&h)) else {
        let mut body = "This is runt's router for local URLs.".to_string();
        let vms = routes();
        if vms.is_empty() {
            body.push_str(" No running VM has an HTTP port.\n");
        } else {
            body.push_str(" Running:\n");
            for r in vms {
                body.push_str(&format!("  {}\n", format_url(&r.name, port)));
            }
        }
        return respond(&mut tcp, 404, &body);
    };
    let rec = match state::load(&name) {
        Ok(r) => r,
        Err(_) => return respond(&mut tcp, 404, &format!("runt: no VM named {name:?}\n")),
    };
    let Some(guest) = rec.http else {
        return respond(
            &mut tcp,
            404,
            &format!("runt: VM {name:?} has no HTTP port (set [http] port in runt.toml)\n"),
        );
    };
    if state::status(&rec) != Status::Running {
        return respond(
            &mut tcp,
            503,
            &format!("runt: VM {name:?} isn't running (`runt up` starts it)\n"),
        );
    }
    match ports::open(&state::socket_path(&name), guest) {
        Ok(conn) => ports::splice(tcp, conn, &head),
        Err(e) => respond(
            &mut tcp,
            502,
            &format!("runt: nothing answered on port {guest} in VM {name:?}: {e}\n"),
        ),
    }
}

fn respond(tcp: &mut TcpStream, code: u16, body: &str) {
    let reason = match code {
        404 => "Not Found",
        431 => "Request Header Fields Too Large",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let _ = write!(
        tcp,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// The Host header of a request head.
fn host(head: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(head).ok()?;
    text.split("\r\n").skip(1).find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.eq_ignore_ascii_case("host").then(|| v.trim().to_string())
    })
}

/// `myapp.runt.localhost:7080` (or `api.myapp.runt.localhost`) -> myapp.
fn vm_name(host: &str) -> Option<String> {
    let host = host.to_ascii_lowercase();
    let host = match host.rsplit_once(':') {
        Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => &host,
    };
    let rest = host.trim_end_matches('.').strip_suffix(SUFFIX)?;
    let name = rest.rsplit('.').next()?;
    state::validate_name(name).ok()?;
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_vm() {
        let head = b"GET / HTTP/1.1\r\nUser-Agent: x\r\nHOST: MyApp.runt.localhost:7080\r\n\r\n";
        assert_eq!(host(head).as_deref(), Some("MyApp.runt.localhost:7080"));
        assert_eq!(
            vm_name("MyApp.runt.localhost:7080").as_deref(),
            Some("myapp")
        );
        assert_eq!(vm_name("myapp.runt.localhost.").as_deref(), Some("myapp"));
        assert_eq!(
            vm_name("api.my-app.runt.localhost").as_deref(),
            Some("my-app")
        );
        for bad in [
            "localhost:7080",
            "runt.localhost",
            "x.localhost",
            "-x.runt.localhost",
            "",
        ] {
            assert_eq!(vm_name(bad), None, "{bad}");
        }
        assert_eq!(host(b"GET / HTTP/1.0\r\n\r\n"), None);
        assert_eq!(format_url("a", 80), "http://a.runt.localhost");
        assert_eq!(format_url("a", 7080), "http://a.runt.localhost:7080");
    }
}
