//! Terminal helpers: raw mode, window size, and forwarding signals through a
//! self-pipe so a normal thread can act on them.

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};

pub fn is_tty(fd: RawFd) -> bool {
    // SAFETY: isatty on any fd is safe.
    unsafe { libc::isatty(fd) == 1 }
}

pub fn winsize(fd: RawFd) -> Option<(u16, u16)> {
    // SAFETY: zeroed winsize is a valid out-parameter.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: valid fd and pointer.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_row > 0).then_some((ws.ws_row, ws.ws_col))
}

/// Puts a terminal in raw mode; restores it on drop (or `restore`).
pub struct RawMode {
    fd: RawFd,
    saved: Option<libc::termios>,
}

impl RawMode {
    pub fn enable(fd: RawFd) -> io::Result<RawMode> {
        // SAFETY: zeroed termios is a valid out-parameter.
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: valid fd and pointers.
        unsafe {
            if libc::tcgetattr(fd, &mut t) != 0 {
                return Err(io::Error::last_os_error());
            }
            let saved = t;
            libc::cfmakeraw(&mut t);
            if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(RawMode {
                fd,
                saved: Some(saved),
            })
        }
    }

    pub fn restore(&mut self) {
        if let Some(t) = self.saved.take() {
            // SAFETY: valid fd and termios.
            unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &t) };
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        self.restore();
    }
}

static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(signo: libc::c_int) {
    let fd = SIGNAL_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        let b = signo as u8;
        // SAFETY: write(2) is async-signal-safe.
        unsafe { libc::write(fd, (&b as *const u8).cast(), 1) };
    }
}

/// Route the given signals to a pipe; read signal numbers from the returned
/// file. Call once per process.
pub fn signal_pipe(signals: &[libc::c_int]) -> io::Result<File> {
    let mut fds = [0; 2];
    // SAFETY: fds has room for two descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    SIGNAL_PIPE.store(fds[1], Ordering::Relaxed);
    for &s in signals {
        // SAFETY: installing a handler that only calls write(2).
        unsafe { libc::signal(s, on_signal as *const () as libc::sighandler_t) };
    }
    // SAFETY: pipe2 returned a new owned fd.
    Ok(unsafe { File::from_raw_fd(fds[0]) })
}

/// Read the next forwarded signal number.
pub fn next_signal(f: &mut File) -> Option<libc::c_int> {
    let mut b = [0u8; 1];
    loop {
        match f.read(&mut b) {
            Ok(1) => return Some(b[0] as libc::c_int),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            _ => return None,
        }
    }
}

pub fn stdin_fd() -> RawFd {
    io::stdin().as_raw_fd()
}
