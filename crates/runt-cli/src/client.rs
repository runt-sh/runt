//! Talking to runt-agent through the VM's socket.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::Duration;

use runt_proto::{Event, ExecRequest, Msg, Mux, STDERR, STDIN, STDOUT, WinSize};

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
