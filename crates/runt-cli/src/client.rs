//! Talking to runt-agent through the VM's socket.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::Duration;

use runt_proto::{
    Event, ExecRequest, Msg, Mux, STDERR, STDIN, STDOUT, Service, ServiceStatus, WinSize,
};

use crate::error::{CliError, Result};
use crate::term;

pub struct Conn {
    pub mux: Mux,
    pub rx: Receiver<Event>,
}

/// Connect and wait for the agent's Hello. libkrun accepts the unix
/// connection even before the guest is listening and then drops it, so a
/// missing Hello within `timeout` is reported as None (try again).
pub fn try_connect(sock: &Path, timeout: Duration) -> Option<Conn> {
    let s = UnixStream::connect(sock).ok()?;
    let (mux, rx) = Mux::new(s.try_clone().ok()?, s);
    match rx.recv_timeout(timeout) {
        Ok(Event::Msg(Msg::Hello { version, .. })) if version == runt_proto::VERSION => {
            Some(Conn { mux, rx })
        }
        _ => None,
    }
}

pub fn connect(name: &str, sock: &Path) -> Result<Conn> {
    try_connect(sock, Duration::from_secs(5)).ok_or_else(|| {
        CliError::new(
            "agent_unreachable",
            format!("cannot reach the agent in VM {name:?}"),
        )
        .hint(format!(
            "check `runt ls`; console log: {}",
            crate::state::vm_dir(name).join("console.log").display()
        ))
    })
}

pub fn shutdown(conn: &Conn) -> Result<()> {
    conn.mux.send(&Msg::Shutdown)?;
    Ok(())
}

/// Make the VM's recorded services its running set; returns their state.
pub fn set_services(rec: &crate::state::VmRecord) -> Result<Vec<ServiceStatus>> {
    let specs = rec
        .services
        .iter()
        .map(|s| Service {
            name: s.name.clone(),
            argv: vec!["/bin/sh".into(), "-c".into(), s.cmd.clone()],
            env: rec
                .env
                .iter()
                .chain(&s.env)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            cwd: s.cwd.clone(),
            restart: s.restart.proto(),
        })
        .collect();
    service_request(&rec.name, Msg::SetServices(specs))
}

pub fn get_services(name: &str) -> Result<Vec<ServiceStatus>> {
    service_request(name, Msg::GetServices)
}

fn service_request(name: &str, msg: Msg) -> Result<Vec<ServiceStatus>> {
    let conn = connect(name, &crate::state::socket_path(name))?;
    conn.mux.send(&msg)?;
    // Replacing services waits for old ones to stop (5 s grace each).
    match conn.rx.recv_timeout(Duration::from_secs(60)) {
        Ok(Event::Msg(Msg::ServiceList(list))) => Ok(list),
        _ => Err(CliError::new(
            "agent_unreachable",
            format!("VM {name:?} didn't answer about its services"),
        )
        .hint("the VM's agent may be too old; rebuild assets with `make assets`")),
    }
}

/// Where command output goes.
pub enum Output {
    /// Straight to our stdout/stderr.
    Stream,
    /// Collected (for `--json`).
    Capture { stdout: Vec<u8>, stderr: Vec<u8> },
}

pub struct ExecOpts {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
    pub tty: bool,
}

/// Run a command and return its exit code (128+N if killed by signal N).
pub fn exec(conn: Conn, opts: ExecOpts, out: &mut Output) -> Result<i32> {
    let Conn { mux, rx } = conn;
    let stdin_fd = term::stdin_fd();
    let tty = if opts.tty {
        let (rows, cols) = term::winsize(stdin_fd)
            .or_else(|| term::winsize(1))
            .unwrap_or((24, 80));
        Some(WinSize { rows, cols })
    } else {
        None
    };
    mux.send(&Msg::Exec(ExecRequest {
        argv: opts.argv,
        env: opts.env,
        cwd: opts.cwd,
        tty,
    }))?;

    let mut raw = if opts.tty && term::is_tty(stdin_fd) {
        term::RawMode::enable(stdin_fd).ok()
    } else {
        None
    };

    // Forward resizes (tty) or interrupts (no tty) to the guest.
    let signals: &[libc::c_int] = if opts.tty {
        &[libc::SIGWINCH]
    } else {
        &[libc::SIGINT, libc::SIGTERM, libc::SIGHUP]
    };
    if let Ok(mut sigs) = term::signal_pipe(signals) {
        let mux = mux.clone();
        thread::spawn(move || {
            while let Some(sig) = term::next_signal(&mut sigs) {
                let msg = if sig == libc::SIGWINCH {
                    match term::winsize(term::stdin_fd()) {
                        Some((rows, cols)) => Msg::Resize { rows, cols },
                        None => continue,
                    }
                } else {
                    Msg::Signal { signo: sig }
                };
                if mux.send(&msg).is_err() {
                    return;
                }
            }
        });
    }

    {
        let mux = mux.clone();
        thread::spawn(move || {
            let mut stdin = io::stdin().lock();
            let mut buf = vec![0u8; runt_proto::MAX_CHUNK];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if mux.send_data(STDIN, &buf[..n]).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = mux.send_eof(STDIN);
        });
    }

    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    let code = loop {
        let Ok(ev) = rx.recv() else { break None };
        match ev {
            Event::Data(stream, d) => {
                match (stream, &mut *out) {
                    (STDOUT, Output::Stream) => {
                        let _ = stdout.write_all(&d).and_then(|_| stdout.flush());
                    }
                    (STDERR, Output::Stream) => {
                        let _ = stderr.write_all(&d).and_then(|_| stderr.flush());
                    }
                    (STDOUT, Output::Capture { stdout, .. }) => stdout.extend_from_slice(&d),
                    (STDERR, Output::Capture { stderr, .. }) => stderr.extend_from_slice(&d),
                    _ => {}
                }
                let _ = mux.grant(stream, d.len());
            }
            Event::Msg(Msg::Exit { code, signal }) => {
                break Some(code.unwrap_or(128 + signal.unwrap_or(0)));
            }
            Event::Msg(Msg::Error { message }) => {
                if let Some(r) = raw.as_mut() {
                    r.restore();
                }
                return Err(CliError::new("exec_failed", message));
            }
            Event::Closed => break None,
            _ => {}
        }
    };
    if let Some(r) = raw.as_mut() {
        r.restore();
    }
    code.ok_or_else(|| {
        CliError::new(
            "connection_lost",
            "lost connection to the VM before the command finished",
        )
    })
}

/// Run a non-interactive command with no stdin, handing its output (stdout
/// and stderr interleaved) to `sink` as it arrives. Returns the exit code.
pub fn run_streamed(conn: Conn, opts: ExecOpts, sink: &mut dyn FnMut(&[u8])) -> Result<i32> {
    let Conn { mux, rx } = conn;
    mux.send(&Msg::Exec(ExecRequest {
        argv: opts.argv,
        env: opts.env,
        cwd: opts.cwd,
        tty: None,
    }))?;
    mux.send_eof(STDIN)?;
    loop {
        match rx.recv() {
            Ok(Event::Data(stream, d)) => {
                sink(&d);
                let _ = mux.grant(stream, d.len());
            }
            Ok(Event::Msg(Msg::Exit { code, signal })) => {
                return Ok(code.unwrap_or(128 + signal.unwrap_or(0)));
            }
            Ok(Event::Msg(Msg::Error { message })) => {
                return Err(CliError::new("exec_failed", message));
            }
            Ok(Event::Closed) | Err(_) => {
                return Err(CliError::new(
                    "connection_lost",
                    "lost connection to the VM before the command finished",
                ));
            }
            Ok(_) => {}
        }
    }
}

/// Output of [`run_captured`].
pub struct Captured {
    /// Exit code (128+N if killed by signal N); None if the timeout hit.
    pub code: Option<i32>,
    pub stdout: Clipped,
    pub stderr: Clipped,
}

/// A byte stream kept within a budget: the start and the end survive, the
/// middle is dropped (errors tend to be at the end, context at the start).
pub struct Clipped {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    total: usize,
    half: usize,
}

impl Clipped {
    pub fn new(limit: usize) -> Clipped {
        Clipped {
            head: Vec::new(),
            tail: std::collections::VecDeque::new(),
            total: 0,
            half: limit / 2,
        }
    }

    fn push(&mut self, mut d: &[u8]) {
        self.total += d.len();
        let room = self.half.saturating_sub(self.head.len());
        let n = room.min(d.len());
        self.head.extend_from_slice(&d[..n]);
        d = &d[n..];
        self.tail.extend(d);
        let excess = self.tail.len().saturating_sub(self.half);
        self.tail.drain(..excess);
    }

    /// Bytes dropped from the middle.
    pub fn omitted(&self) -> usize {
        self.total - self.head.len() - self.tail.len()
    }

    /// The kept text, with a marker where bytes were dropped.
    pub fn text(&self) -> String {
        let mut s = String::from_utf8_lossy(&self.head).into_owned();
        if self.omitted() > 0 {
            s.push_str(&format!("\n[... {} bytes omitted ...]\n", self.omitted()));
        }
        let (a, b) = self.tail.as_slices();
        s.push_str(&String::from_utf8_lossy(&[a, b].concat()));
        s
    }
}

/// Run a non-interactive command with the given stdin, collecting at most
/// `limit` bytes of each output stream. Never touches this process's stdin
/// or signals. On timeout the command's process group is killed.
pub fn run_captured(
    conn: Conn,
    opts: ExecOpts,
    stdin: Vec<u8>,
    timeout: Duration,
    limit: usize,
) -> Result<Captured> {
    let Conn { mux, rx } = conn;
    mux.send(&Msg::Exec(ExecRequest {
        argv: opts.argv,
        env: opts.env,
        cwd: opts.cwd,
        tty: None,
    }))?;
    {
        let mux = mux.clone();
        thread::spawn(move || {
            for chunk in stdin.chunks(runt_proto::MAX_CHUNK) {
                if mux.send_data(STDIN, chunk).is_err() {
                    return;
                }
            }
            let _ = mux.send_eof(STDIN);
        });
    }
    let mut out = Captured {
        code: None,
        stdout: Clipped::new(limit),
        stderr: Clipped::new(limit),
    };
    let deadline = std::time::Instant::now() + timeout;
    let mut killed = false;
    loop {
        let wait = deadline.saturating_duration_since(std::time::Instant::now());
        let ev = match rx.recv_timeout(if killed { Duration::from_secs(2) } else { wait }) {
            Ok(ev) => ev,
            Err(_) if !killed => {
                // Timed out: kill it, then collect what's left and the exit.
                mux.send(&Msg::Signal {
                    signo: libc::SIGKILL,
                })?;
                killed = true;
                continue;
            }
            Err(_) => break,
        };
        match ev {
            Event::Data(stream, d) => {
                match stream {
                    STDOUT => out.stdout.push(&d),
                    STDERR => out.stderr.push(&d),
                    _ => {}
                }
                let _ = mux.grant(stream, d.len());
            }
            Event::Msg(Msg::Exit { code, signal }) => {
                if !killed {
                    out.code = Some(code.unwrap_or(128 + signal.unwrap_or(0)));
                }
                return Ok(out);
            }
            Event::Msg(Msg::Error { message }) => {
                return Err(CliError::new("exec_failed", message));
            }
            Event::Closed if killed => return Ok(out),
            Event::Closed => {
                return Err(CliError::new(
                    "connection_lost",
                    "lost connection to the VM before the command finished",
                ));
            }
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_the_middle() {
        let mut c = Clipped::new(8);
        c.push(b"abc");
        assert_eq!((c.text().as_str(), c.omitted()), ("abc", 0));
        c.push(b"defghij");
        c.push(b"klmn");
        assert_eq!(c.omitted(), 6);
        assert_eq!(c.text(), "abcd\n[... 6 bytes omitted ...]\nklmn");
    }
}
