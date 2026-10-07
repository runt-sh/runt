//! Port forwarding, host side. Runs inside the VM's supervisor process.
//!
//! runt-agent reports the guest's listening TCP ports over the events
//! socket. For each one we listen on host 127.0.0.1 (the same port number
//! when it's free and unprivileged, otherwise any free port) and splice each
//! accepted connection to the guest through a `Connect` request. The current
//! mappings are written to `<name>.ports.json` for `runt port`.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use runt_proto::{Event, Msg, Mux, STDIN, STDOUT};
use serde::{Deserialize, Serialize};

use crate::client;
use crate::state;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mapping {
    pub guest: u16,
    pub host: u16,
}

impl Mapping {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.host)
    }
}

/// Current forwards for a VM (empty if none or not running).
pub fn read(name: &str) -> Vec<Mapping> {
    fs::read(state::ports_path(name))
        .ok()
        .and_then(|d| serde_json::from_slice(&d).ok())
        .unwrap_or_default()
}

pub fn clear(name: &str) {
    let _ = fs::remove_file(state::ports_path(name));
}

struct Forward {
    host: u16,
    listener: TcpListener,
}

impl Drop for Forward {
    fn drop(&mut self) {
        // Wakes the accept thread (accept fails once the socket is shut down).
        // SAFETY: shutdown(2) on a socket we own.
        unsafe { libc::shutdown(self.listener.as_raw_fd(), libc::SHUT_RDWR) };
    }
}

/// Start listening for the agent's port reports. Call before the VM boots.
pub fn start(name: &str) -> std::io::Result<()> {
    let events_path = state::events_socket_path(name);
    state::remove_socket(&events_path);
    clear(name);
    let listener = UnixListener::bind(&events_path)?;
    let name = name.to_string();
    let forwards: Arc<Mutex<BTreeMap<u16, Forward>>> = Arc::default();
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let Ok(reader) = conn.try_clone() else {
                continue;
            };
            let (_mux, rx) = Mux::new(reader, conn);
            for ev in rx {
                match ev {
                    Event::Msg(Msg::Ports { listening }) => {
                        reconcile(&name, &forwards, &listening);
                    }
                    Event::Closed => break,
                    _ => {}
                }
            }
        }
    });
    Ok(())
}

fn reconcile(name: &str, forwards: &Mutex<BTreeMap<u16, Forward>>, listening: &[u16]) {
    let mut fwd = forwards.lock().unwrap();
    fwd.retain(|guest, _| listening.contains(guest));
    for &guest in listening {
        if fwd.contains_key(&guest) {
            continue;
        }
        match bind_host(guest) {
            Ok(listener) => {
                let host = listener.local_addr().map(|a| a.port()).unwrap_or(0);
                if let Ok(accepting) = listener.try_clone() {
                    let sock = state::socket_path(name);
                    let vm = name.to_string();
                    thread::spawn(move || accept_loop(accepting, vm, sock, guest));
                }
                fwd.insert(guest, Forward { host, listener });
            }
            Err(e) => eprintln!("runt: cannot forward guest port {guest}: {e}"),
        }
    }
    let mappings: Vec<Mapping> = fwd
        .iter()
        .map(|(&guest, f)| Mapping {
            guest,
            host: f.host,
        })
        .collect();
    write_mappings(name, &mappings);
}

/// Prefer the guest's own port number on the host; fall back to any port.
fn bind_host(guest: u16) -> std::io::Result<TcpListener> {
    if guest >= 1024
        && let Ok(l) = TcpListener::bind(("127.0.0.1", guest))
    {
        return Ok(l);
    }
    TcpListener::bind(("127.0.0.1", 0))
}

fn write_mappings(name: &str, mappings: &[Mapping]) {
    let path = state::ports_path(name);
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    if fs::write(&tmp, serde_json::to_vec(mappings).unwrap()).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
}

fn accept_loop(listener: TcpListener, vm: String, sock: PathBuf, guest: u16) {
    for conn in listener.incoming() {
        let Ok(tcp) = conn else { break };
        let sock = sock.clone();
        let vm = vm.clone();
        thread::spawn(move || {
            if let Err(e) = forward(tcp, &sock, guest) {
                eprintln!("runt: forward to {vm}:{guest} failed: {e}");
            }
        });
    }
}

/// Splice one host connection to guest loopback:port through the agent.
fn forward(tcp: TcpStream, sock: &std::path::Path, guest: u16) -> Result<(), String> {
    splice(tcp, open(sock, guest)?, &[]);
    Ok(())
}

/// Connect to guest loopback:port through the VM's agent.
pub fn open(sock: &std::path::Path, guest: u16) -> Result<client::Conn, String> {
    let conn = client::try_connect(sock, Duration::from_secs(5)).ok_or("agent unreachable")?;
    conn.mux
        .send(&Msg::Connect { port: guest })
        .map_err(|e| e.to_string())?;
    match conn.rx.recv() {
        Ok(Event::Msg(Msg::Connected)) => Ok(conn),
        Ok(Event::Msg(Msg::Error { message })) => Err(message),
        _ => Err("connection closed".into()),
    }
}

/// Copy bytes both ways between a host connection and a guest one until
/// both sides are done. `first` goes to the guest before anything else.
pub fn splice(tcp: TcpStream, conn: client::Conn, first: &[u8]) {
    let _ = tcp.set_nodelay(true);
    let Ok(mut from_host) = tcp.try_clone() else {
        return;
    };
    let out = conn.mux.clone();
    if !first.is_empty() && out.send_data(STDIN, first).is_err() {
        return;
    }
    thread::spawn(move || {
        let mut buf = vec![0u8; runt_proto::MAX_CHUNK];
        loop {
            match from_host.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out.send_data(STDIN, &buf[..n]).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = out.send_eof(STDIN);
    });
    let mut to_host = tcp;
    for ev in conn.rx {
        match ev {
            Event::Data(STDOUT, d) => {
                if to_host.write_all(&d).is_err() {
                    break;
                }
                let _ = conn.mux.grant(STDOUT, d.len());
            }
            Event::Eof(STDOUT) => {
                let _ = to_host.shutdown(Shutdown::Write);
            }
            Event::Closed => break,
            _ => {}
        }
    }
    let _ = to_host.shutdown(Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_json() {
        let m = vec![Mapping {
            guest: 3000,
            host: 3000,
        }];
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, r#"[{"guest":3000,"host":3000}]"#);
        assert_eq!(serde_json::from_str::<Vec<Mapping>>(&s).unwrap(), m);
        assert_eq!(m[0].url(), "http://127.0.0.1:3000");
    }
}
