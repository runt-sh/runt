//! Serving one host connection: an exec session or a shutdown request.

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use runt_proto::{Event, ExecRequest, Msg, Mux, STDERR, STDIN, STDOUT};

use crate::sys::{self, ExitStatus};

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// After the command exits, keep forwarding output until it has been quiet
/// this long. Background processes that inherited stdout must not hold the
/// session open forever.
const DRAIN_QUIET: Duration = Duration::from_millis(100);

pub fn serve(conn: File) {
    let reader = match conn.try_clone() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("runt-agent: dup connection: {e}");
            return;
        }
    };
    let (mux, rx) = Mux::new(reader, conn);
    let hello = Msg::Hello {
        version: runt_proto::VERSION,
        agent: concat!("runt-agent/", env!("CARGO_PKG_VERSION")).into(),
    };
    if mux.send(&hello).is_err() {
        return;
    }
    match rx.recv() {
        Ok(Event::Msg(Msg::Exec(req))) => {
            if let Err(e) = exec(&mux, &rx, req) {
                let _ = mux.send(&Msg::Error {
                    message: e.to_string(),
                });
            }
        }
        Ok(Event::Msg(Msg::Shutdown)) => crate::shutdown(),
        _ => {}
    }
}

fn exec(mux: &Mux, rx: &Receiver<Event>, req: ExecRequest) -> io::Result<()> {
    if req.argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty command"));
    }
    let env = build_env(&req);
    let path_var = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.as_str());
    let Some(path) = resolve(&req.argv[0], path_var.unwrap_or(DEFAULT_PATH)) else {
        let msg = format!("runt: {}: command not found\n", req.argv[0]);
        return finish_early(mux, &msg, 127);
    };
    let cwd = req.cwd.clone().unwrap_or_else(|| "/root".into());

    // Everything the child touches is prepared before fork: after fork only
    // async-signal-safe calls are allowed.
    let cstr = |s: &[u8]| {
        CString::new(s).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in argument"))
    };
    let path_c = cstr(path.as_os_str().as_bytes())?;
    let argv_c: Vec<CString> = req
        .argv
        .iter()
        .map(|a| cstr(a.as_bytes()))
        .collect::<Result<_, _>>()?;
    let env_c: Vec<CString> = env
        .iter()
        .map(|(k, v)| cstr(format!("{k}={v}").as_bytes()))
        .collect::<Result<_, _>>()?;
    let cwd_c = cstr(cwd.as_bytes())?;
    let mut argv_p: Vec<*const libc::c_char> = argv_c.iter().map(|c| c.as_ptr()).collect();
    argv_p.push(std::ptr::null());
    let mut env_p: Vec<*const libc::c_char> = env_c.iter().map(|c| c.as_ptr()).collect();
    env_p.push(std::ptr::null());

    // Host-side ends we keep, and child-side ends we pass down.
    let (stdin_w, out_r, err_r, pty_master, child_fds): (
        OwnedFd,
        OwnedFd,
        Option<OwnedFd>,
        Option<RawFd>,
        Vec<OwnedFd>,
    );
    match req.tty {
        Some(ws) => {
            let (master, slave) = sys::openpty(ws.rows, ws.cols)?;
            let master_raw = master.as_raw_fd();
            stdin_w = master.try_clone()?;
            out_r = master;
            err_r = None;
            pty_master = Some(master_raw);
            child_fds = vec![slave];
        }
        None => {
            let (in_r, in_w) = sys::pipe()?;
            let (o_r, o_w) = sys::pipe()?;
            let (e_r, e_w) = sys::pipe()?;
            stdin_w = in_w;
            out_r = o_r;
            err_r = Some(e_r);
            pty_master = None;
            child_fds = vec![in_r, o_w, e_w];
        }
    }
    let raw: Vec<RawFd> = child_fds.iter().map(|f| f.as_raw_fd()).collect();
    let tty = req.tty.is_some();
    let (errno_r, errno_w) = sys::pipe()?;
    let errno_w_raw = errno_w.as_raw_fd();

    let (pid, exited) = sys::spawn_watched(|| {
        // SAFETY: fork; the child only makes async-signal-safe calls on
        // memory prepared above, then execs or _exits.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe {
                child(
                    tty,
                    &raw,
                    cwd_c.as_ptr(),
                    path_c.as_ptr(),
                    argv_p.as_ptr(),
                    env_p.as_ptr(),
                    errno_w_raw,
                )
            }
        }
        sys::cvt(pid)
    })?;
    drop(child_fds);
    drop(errno_w);

    // A successful exec closes the errno pipe without writing to it.
    let mut buf = [0u8; 4];
    let mut errno_file = File::from(errno_r);
    if errno_file.read_exact(&mut buf).is_ok() {
        let err = io::Error::from_raw_os_error(i32::from_le_bytes(buf));
        let _ = mux.send_data(STDERR, format!("runt: {}: {err}\n", req.argv[0]).as_bytes());
    }
    let _ = mux.send(&Msg::Started { pid: pid as u32 });

    let done = Arc::new(AtomicBool::new(false));
    let mut pumps = vec![pump(out_r, STDOUT, mux.clone(), done.clone())];
    if let Some(e) = err_r {
        pumps.push(pump(e, STDERR, mux.clone(), done.clone()));
    }
    if tty {
        // Keep stream numbering simple for clients: a PTY has no stderr.
        let _ = mux.send_eof(STDERR);
    }
    let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>();
    {
        let mux = mux.clone();
        thread::spawn(move || {
            let mut w = File::from(stdin_w);
            for chunk in stdin_rx {
                // If the command stopped reading, keep granting anyway so the
                // client doesn't block on a stdin nobody will consume.
                let _ = w.write_all(&chunk);
                let _ = mux.grant(STDIN, chunk.len());
            }
        });
    }
    {
        let mux = mux.clone();
        let done = done.clone();
        thread::spawn(move || {
            let status = exited.recv().unwrap_or(ExitStatus::Code(255));
            done.store(true, Ordering::SeqCst);
            for p in pumps {
                let _ = p.join();
            }
            let (code, signal) = match status {
                ExitStatus::Code(c) => (Some(c), None),
                ExitStatus::Signal(s) => (None, Some(s)),
            };
            let _ = mux.send(&Msg::Exit { code, signal });
        });
    }

    let mut stdin_tx = Some(stdin_tx);
    for ev in rx {
        match ev {
            Event::Data(STDIN, d) => match &stdin_tx {
                Some(tx) => {
                    let _ = tx.send(d);
                }
                None => {
                    let _ = mux.grant(STDIN, d.len());
                }
            },
            Event::Eof(STDIN) => stdin_tx = None,
            Event::Msg(Msg::Resize { rows, cols }) => {
                if let Some(m) = pty_master {
                    let _ = sys::set_winsize(m, rows, cols);
                }
            }
            Event::Msg(Msg::Signal { signo }) => {
                // SAFETY: plain kill(2) on the command's process group.
                unsafe { libc::kill(-pid, signo) };
            }
            Event::Closed => {
                // Client went away before the command finished: stop it.
                if !done.load(Ordering::SeqCst) {
                    // SAFETY: plain kill(2) on the command's process group.
                    unsafe { libc::kill(-pid, libc::SIGKILL) };
                }
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Runs in the forked child. Never returns.
unsafe fn child(
    tty: bool,
    fds: &[RawFd],
    cwd: *const libc::c_char,
    path: *const libc::c_char,
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
    errno_w: RawFd,
) -> ! {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::pthread_sigmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
        // Rust ignores SIGPIPE in its runtime; children expect the default.
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        libc::setsid();
        if tty {
            libc::ioctl(fds[0], libc::TIOCSCTTY, 0);
            for target in 0..3 {
                libc::dup2(fds[0], target);
            }
        } else {
            for (target, fd) in fds.iter().enumerate() {
                libc::dup2(*fd, target as RawFd);
            }
        }
        libc::chdir(cwd); // fall back to the current directory if missing
        libc::execve(path, argv, envp);
        let errno = *libc::__errno_location();
        libc::write(errno_w, errno.to_le_bytes().as_ptr().cast(), 4);
        libc::_exit(if errno == libc::ENOENT { 127 } else { 126 });
    }
}

/// Forward a command output fd to a stream until EOF, or until the command
/// has exited and output has gone quiet.
fn pump(fd: OwnedFd, stream: u32, mux: Mux, done: Arc<AtomicBool>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut f = File::from(fd);
        let mut buf = vec![0u8; runt_proto::MAX_CHUNK];
        loop {
            let mut pfd = libc::pollfd {
                fd: f.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            let n = unsafe { libc::poll(&mut pfd, 1, DRAIN_QUIET.as_millis() as libc::c_int) };
            if n == 0 {
                if done.load(Ordering::SeqCst) {
                    break;
                }
                continue;
            }
            match f.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if mux.send_data(stream, &buf[..n]).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break, // EIO: PTY slave closed
            }
        }
        let _ = mux.send_eof(stream);
    })
}

fn finish_early(mux: &Mux, stderr_msg: &str, code: i32) -> io::Result<()> {
    mux.send_data(STDERR, stderr_msg.as_bytes())?;
    mux.send_eof(STDOUT)?;
    mux.send_eof(STDERR)?;
    mux.send(&Msg::Exit {
        code: Some(code),
        signal: None,
    })
}

fn build_env(req: &ExecRequest) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = [
        ("PATH", DEFAULT_PATH),
        ("HOME", "/root"),
        ("USER", "root"),
        ("LOGNAME", "root"),
        ("SHELL", "/bin/bash"),
        ("LANG", "C.UTF-8"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    if req.tty.is_some() {
        env.push(("TERM".into(), "xterm-256color".into()));
    }
    for (k, v) in &req.env {
        env.retain(|(ek, _)| ek != k);
        env.push((k.clone(), v.clone()));
    }
    env
}

/// Find `cmd` the way execvp would, so a missing command is reported as
/// 127 without forking.
fn resolve(cmd: &str, path_var: &str) -> Option<PathBuf> {
    if cmd.contains('/') {
        return Some(PathBuf::from(cmd));
    }
    path_var
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(cmd))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    let Ok(c) = CString::new(p.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: valid C string.
    let executable = unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0;
    executable && std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_overrides() {
        let req = ExecRequest {
            argv: vec!["x".into()],
            env: vec![
                ("PATH".into(), "/opt/bin".into()),
                ("FOO".into(), "1".into()),
            ],
            ..Default::default()
        };
        let env = build_env(&req);
        assert_eq!(env.iter().filter(|(k, _)| k == "PATH").count(), 1);
        assert!(env.contains(&("PATH".into(), "/opt/bin".into())));
        assert!(env.contains(&("FOO".into(), "1".into())));
        assert!(!env.iter().any(|(k, _)| k == "TERM"));
    }

    #[test]
    fn resolves_commands() {
        assert_eq!(resolve("/x/y", ""), Some(PathBuf::from("/x/y")));
        assert!(resolve("sh", DEFAULT_PATH).is_some());
        assert!(resolve("definitely-not-a-command-xyz", DEFAULT_PATH).is_none());
    }
}
