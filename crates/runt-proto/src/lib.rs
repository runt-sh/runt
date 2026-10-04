//! Wire protocol between the runt CLI and runt-agent.
//!
//! One connection carries several logical streams. Every frame is
//!
//! ```text
//! [u32 len][u32 stream][u8 kind][len bytes of payload]   (little endian)
//! ```
//!
//! Stream [`CTRL`] carries postcard-encoded [`Msg`] values. Data streams carry
//! raw bytes and are flow controlled: a sender may only have [`WINDOW`] bytes
//! outstanding per stream until the receiver returns credit. This keeps a slow
//! reader from making the other side buffer without bound (and, on some
//! hypervisors, from stalling the VM).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 0;

/// Guest vsock port runt-agent listens on.
pub const AGENT_PORT: u32 = 1024;

/// Host vsock port runt-agent connects to once it is ready to serve. The
/// host listens there so boot completion is an event, not a poll.
pub const READY_PORT: u32 = 1025;

/// Host vsock port runt-agent connects to for pushing events (currently the
/// set of listening TCP ports, for automatic port forwarding).
pub const EVENTS_PORT: u32 = 1026;

/// Control stream.
pub const CTRL: u32 = 0;
/// Exec stdin (client -> agent).
pub const STDIN: u32 = 1;
/// Exec stdout, or the PTY output (agent -> client).
pub const STDOUT: u32 = 2;
/// Exec stderr (agent -> client).
pub const STDERR: u32 = 3;

/// Initial per-stream send window, in bytes.
pub const WINDOW: u32 = 256 * 1024;
/// Largest data payload in one frame.
pub const MAX_CHUNK: usize = 32 * 1024;
/// Largest frame we accept.
const MAX_FRAME: u32 = 1024 * 1024;

const KIND_CTRL: u8 = 0;
const KIND_DATA: u8 = 1;
const KIND_CREDIT: u8 = 2;
const KIND_EOF: u8 = 3;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Msg {
    /// Sent by the agent when a connection opens.
    Hello {
        version: u32,
        agent: String,
    },
    /// Run a command.
    Exec(ExecRequest),
    /// The command started.
    Started {
        pid: u32,
    },
    /// Terminal size changed (PTY execs only).
    Resize {
        rows: u16,
        cols: u16,
    },
    /// Deliver a signal to the command.
    Signal {
        signo: i32,
    },
    /// The command finished. Exactly one of `code` / `signal` is set.
    Exit {
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// Power the VM off.
    Shutdown,
    /// Open a TCP connection to `port` on the guest's loopback and splice it
    /// to this connection: host->guest bytes on [`STDIN`], guest->host on
    /// [`STDOUT`].
    Connect {
        port: u16,
    },
    /// The `Connect` succeeded; data may flow.
    Connected,
    /// The TCP ports listening inside the guest (sent on the events
    /// connection whenever the set changes).
    Ports {
        listening: Vec<u16>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ExecRequest {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
    pub tty: Option<WinSize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WinSize {
    pub rows: u16,
    pub cols: u16,
}

/// Something that arrived on the connection.
#[derive(Debug)]
pub enum Event {
    Msg(Msg),
    /// Data on a stream. Call [`Mux::grant`] once it has been consumed.
    Data(u32, Vec<u8>),
    /// The peer will send no more data on this stream.
    Eof(u32),
    /// The connection is gone.
    Closed,
}

struct Shared {
    writer: Mutex<Box<dyn Write + Send>>,
    credit: Mutex<Credit>,
    credit_cv: Condvar,
}

struct Credit {
    by_stream: HashMap<u32, u64>,
    closed: bool,
}

/// Sending half of a connection. Cheap to clone; all clones share the socket.
#[derive(Clone)]
pub struct Mux {
    shared: Arc<Shared>,
}

impl Mux {
    /// Start a mux over a connection. Spawns a reader thread; incoming events
    /// are delivered on the returned channel.
    pub fn new(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
    ) -> (Mux, Receiver<Event>) {
        let shared = Arc::new(Shared {
            writer: Mutex::new(Box::new(writer)),
            credit: Mutex::new(Credit {
                by_stream: HashMap::new(),
                closed: false,
            }),
            credit_cv: Condvar::new(),
        });
        let (tx, rx) = mpsc::channel();
        let reader_shared = shared.clone();
        thread::spawn(move || read_loop(reader, reader_shared, tx));
        (Mux { shared }, rx)
    }

    pub fn send(&self, msg: &Msg) -> io::Result<()> {
        let body = postcard::to_stdvec(msg).map_err(io::Error::other)?;
        self.write_frame(CTRL, KIND_CTRL, &body)
    }

    /// Send data, blocking while the peer has not granted enough credit.
    pub fn send_data(&self, stream: u32, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let n = self.take_credit(stream, data.len().min(MAX_CHUNK))?;
            self.write_frame(stream, KIND_DATA, &data[..n])?;
            data = &data[n..];
        }
        Ok(())
    }

    pub fn send_eof(&self, stream: u32) -> io::Result<()> {
        self.write_frame(stream, KIND_EOF, &[])
    }

    /// Tell the peer we consumed `n` bytes of `stream`, so it may send more.
    pub fn grant(&self, stream: u32, n: usize) -> io::Result<()> {
        self.write_frame(stream, KIND_CREDIT, &(n as u32).to_le_bytes())
    }

    fn take_credit(&self, stream: u32, want: usize) -> io::Result<usize> {
        let mut c = self.shared.credit.lock().unwrap();
        loop {
            if c.closed {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            let avail = c.by_stream.entry(stream).or_insert(WINDOW as u64);
            if *avail > 0 {
                let n = (*avail).min(want as u64);
                *avail -= n;
                return Ok(n as usize);
            }
            c = self.shared.credit_cv.wait(c).unwrap();
        }
    }

    fn write_frame(&self, stream: u32, kind: u8, payload: &[u8]) -> io::Result<()> {
        let mut buf = Vec::with_capacity(9 + payload.len());
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&stream.to_le_bytes());
        buf.push(kind);
        buf.extend_from_slice(payload);
        let mut w = self.shared.writer.lock().unwrap();
        w.write_all(&buf)?;
        w.flush()
    }
}

fn read_loop(mut r: impl Read, shared: Arc<Shared>, tx: Sender<Event>) {
    let _ = read_frames(&mut r, &shared, &tx);
    shared.credit.lock().unwrap().closed = true;
    shared.credit_cv.notify_all();
    let _ = tx.send(Event::Closed);
}

fn read_frames(r: &mut impl Read, shared: &Shared, tx: &Sender<Event>) -> io::Result<()> {
    let mut hdr = [0u8; 9];
    loop {
        r.read_exact(&mut hdr)?;
        let len = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let stream = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
        let kind = hdr[8];
        if len > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame too large",
            ));
        }
        let mut payload = vec![0u8; len as usize];
        r.read_exact(&mut payload)?;
        let ev = match kind {
            KIND_CTRL => {
                let msg = postcard::from_bytes(&payload)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Event::Msg(msg)
            }
            KIND_DATA => Event::Data(stream, payload),
            KIND_EOF => Event::Eof(stream),
            KIND_CREDIT => {
                let n = u32::from_le_bytes(
                    payload
                        .get(..4)
                        .and_then(|b| b.try_into().ok())
                        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad credit"))?,
                );
                let mut c = shared.credit.lock().unwrap();
                *c.by_stream.entry(stream).or_insert(WINDOW as u64) += n as u64;
                shared.credit_cv.notify_all();
                continue;
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unknown frame kind",
                ));
            }
        };
        if tx.send(ev).is_err() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn pair() -> ((Mux, Receiver<Event>), (Mux, Receiver<Event>)) {
        let (a, b) = UnixStream::pair().unwrap();
        (
            Mux::new(a.try_clone().unwrap(), a),
            Mux::new(b.try_clone().unwrap(), b),
        )
    }

    #[test]
    fn msg_roundtrip() {
        let ((a, _arx), (_b, brx)) = pair();
        let msgs = [
            Msg::Hello {
                version: VERSION,
                agent: "test".into(),
            },
            Msg::Exec(ExecRequest {
                argv: vec!["sh".into(), "-c".into(), "exit 7".into()],
                env: vec![("A".into(), "b".into())],
                cwd: Some("/tmp".into()),
                tty: Some(WinSize { rows: 24, cols: 80 }),
            }),
            Msg::Exit {
                code: Some(7),
                signal: None,
            },
            Msg::Shutdown,
            Msg::Connect { port: 3000 },
            Msg::Connected,
            Msg::Ports {
                listening: vec![22, 3000, 8080],
            },
        ];
        for m in &msgs {
            a.send(m).unwrap();
        }
        for m in msgs {
            match brx.recv().unwrap() {
                Event::Msg(got) => assert_eq!(got, m),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn data_and_eof() {
        let ((a, _arx), (b, brx)) = pair();
        let big = vec![7u8; 100_000];
        a.send_data(STDOUT, &big).unwrap();
        a.send_eof(STDOUT).unwrap();
        let mut got = Vec::new();
        loop {
            match brx.recv().unwrap() {
                Event::Data(STDOUT, d) => {
                    b.grant(STDOUT, d.len()).unwrap();
                    got.extend(d);
                }
                Event::Eof(STDOUT) => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(got, big);
    }

    #[test]
    fn stalled_reader_blocks_sender() {
        let ((a, _arx), (b, brx)) = pair();
        let sent = Arc::new(AtomicUsize::new(0));
        let sent2 = sent.clone();
        thread::spawn(move || {
            let chunk = vec![0u8; MAX_CHUNK];
            loop {
                if a.send_data(STDOUT, &chunk).is_err() {
                    return;
                }
                sent2.fetch_add(chunk.len(), Ordering::SeqCst);
            }
        });
        thread::sleep(Duration::from_millis(200));
        // Nothing granted yet: sender must stop at the window.
        assert_eq!(sent.load(Ordering::SeqCst), WINDOW as usize);

        // Consume and grant one chunk; exactly one more chunk may flow.
        let mut received = 0;
        while received < WINDOW as usize {
            if let Event::Data(_, d) = brx.recv().unwrap() {
                received += d.len();
            }
        }
        b.grant(STDOUT, MAX_CHUNK).unwrap();
        thread::sleep(Duration::from_millis(200));
        assert_eq!(sent.load(Ordering::SeqCst), WINDOW as usize + MAX_CHUNK);
    }

    #[test]
    fn closed_connection_unblocks_sender() {
        let (a_sock, mut b_sock) = UnixStream::pair().unwrap();
        let (a, _arx) = Mux::new(a_sock.try_clone().unwrap(), a_sock);
        // Peer drains raw bytes (never grants credit), then hangs up.
        let drain = thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            let mut total = 0;
            while total < WINDOW as usize {
                total += b_sock.read(&mut buf).unwrap();
            }
            thread::sleep(Duration::from_millis(100));
            drop(b_sock);
        });
        let chunk = vec![0u8; WINDOW as usize];
        a.send_data(STDOUT, &chunk).unwrap(); // uses the whole window
        // This blocks for credit until the peer hangs up, then fails.
        assert!(a.send_data(STDOUT, &[1]).is_err());
        drain.join().unwrap();
    }
}
