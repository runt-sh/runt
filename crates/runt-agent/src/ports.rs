//! Port forwarding, guest side.
//!
//! - A watcher notices which TCP ports are listening and pushes the set to
//!   the host over an events connection, so the host can forward them.
//! - `Connect` requests splice a host connection to a guest loopback port.
//!   Going through loopback means servers bound only to 127.0.0.1 (most dev
//!   servers' default) are reachable too.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{Duration, Instant};

use runt_proto::{Event, Msg, Mux, STDIN, STDOUT};

use crate::sys;

const SCAN_INTERVAL: Duration = Duration::from_millis(250);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);

/// Watch listening ports forever, pushing changes to the host.
pub fn watch() {
    let mut events: Option<Mux> = None;
    let mut sent: Option<BTreeSet<u16>> = None;
    let mut last_attempt: Option<Instant> = None;
    loop {
        let ports = listening_ports();
        if events.is_none() && last_attempt.is_none_or(|t| t.elapsed() >= RECONNECT_INTERVAL) {
            last_attempt = Some(Instant::now());
            events = connect_events();
            sent = None;
        }
        if let Some(mux) = &events
            && sent.as_ref() != Some(&ports)
        {
            let msg = Msg::Ports {
                listening: ports.iter().copied().collect(),
            };
            if mux.send(&msg).is_ok() {
                sent = Some(ports);
            } else {
                events = None;
            }
        }
        thread::sleep(SCAN_INTERVAL);
    }
}

fn connect_events() -> Option<Mux> {
    let fd = sys::vsock_connect_host(runt_proto::EVENTS_PORT).ok()?;
    let conn = File::from(fd);
    let (mux, _rx) = Mux::new(conn.try_clone().ok()?, conn);
    Some(mux)
}

fn listening_ports() -> BTreeSet<u16> {
    let mut ports = BTreeSet::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(table) = fs::read_to_string(path) {
            ports.extend(parse_listening(&table));
        }
    }
    ports
}

/// Ports in LISTEN state (st == 0A) from a /proc/net/tcp{,6} table.
fn parse_listening(table: &str) -> impl Iterator<Item = u16> + '_ {
    table.lines().skip(1).filter_map(|line| {
        let mut f = line.split_whitespace();
        let local = f.nth(1)?;
        let state = f.nth(1)?;
        if state != "0A" {
            return None;
        }
        u16::from_str_radix(local.rsplit_once(':')?.1, 16).ok()
    })
}

/// Serve a `Connect` request: splice the connection to guest loopback:port.
pub fn splice(mux: &Mux, rx: &Receiver<Event>, port: u16) -> io::Result<()> {
    let tcp = TcpStream::connect(("127.0.0.1", port))
        .or_else(|_| TcpStream::connect(("::1", port)))
        .map_err(|e| io::Error::new(e.kind(), format!("connect to port {port}: {e}")))?;
    let _ = tcp.set_nodelay(true);
    mux.send(&Msg::Connected)?;

    let mut from_guest = tcp.try_clone()?;
    let out = mux.clone();
    let pump = thread::spawn(move || {
        let mut buf = vec![0u8; runt_proto::MAX_CHUNK];
        loop {
            match from_guest.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out.send_data(STDOUT, &buf[..n]).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = out.send_eof(STDOUT);
    });

    let mut to_guest = tcp;
    for ev in rx {
        match ev {
            Event::Data(STDIN, d) => {
                if to_guest.write_all(&d).is_err() {
                    break;
                }
                let _ = mux.grant(STDIN, d.len());
            }
            Event::Eof(STDIN) => {
                let _ = to_guest.shutdown(Shutdown::Write);
            }
            Event::Closed => break,
            _ => {}
        }
    }
    let _ = to_guest.shutdown(Shutdown::Both);
    let _ = pump.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_net_tcp() {
        let table = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1 0000000000000000 100 0 0 10 0
   1: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 2 1 0000000000000000 100 0 0 10 0
   2: 0200A8C0:A1B2 0101A8C0:01BB 01 00000000:00000000 00:00000000 00000000     0        0 3 1 0000000000000000 20 4 30 10 -1
";
        let ports: Vec<u16> = parse_listening(table).collect();
        assert_eq!(ports, vec![8080, 22]);
        let v6 = "\
  sl  local_address                         remote_address                        st
   0: 00000000000000000000000001000000:0BB8 00000000000000000000000000000000:0000 0A 0
";
        assert_eq!(parse_listening(v6).collect::<Vec<_>>(), vec![3000]);
    }
}
