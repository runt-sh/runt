//! Thin syscall helpers.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread;

pub fn cvt(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

/// Bring up the loopback interface so `localhost` works inside the VM.
pub fn loopback_up() -> io::Result<()> {
    // SAFETY: plain socket/ioctl calls on a zeroed, NUL-terminated ifreq.
    unsafe {
        let fd = cvt(libc::socket(
            libc::AF_INET,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            0,
        ))?;
        let fd = OwnedFd::from_raw_fd(fd);
        let mut ifr: libc::ifreq = std::mem::zeroed();
        for (i, b) in b"lo\0".iter().enumerate() {
            ifr.ifr_name[i] = *b as libc::c_char;
        }
        cvt(libc::ioctl(
            fd.as_raw_fd(),
            libc::SIOCGIFFLAGS as _,
            &mut ifr,
        ))?;
        ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
        cvt(libc::ioctl(fd.as_raw_fd(), libc::SIOCSIFFLAGS as _, &ifr))?;
    }
    Ok(())
}

/// Listen on a vsock port for connections from the host.
pub fn vsock_listen(port: u32) -> io::Result<OwnedFd> {
    // SAFETY: plain socket/bind/listen with a correctly sized sockaddr_vm.
    unsafe {
        let fd = cvt(libc::socket(
            libc::AF_VSOCK,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
        ))?;
        let fd = OwnedFd::from_raw_fd(fd);
        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_port = port;
        addr.svm_cid = libc::VMADDR_CID_ANY;
        cvt(libc::bind(
            fd.as_raw_fd(),
            (&addr as *const libc::sockaddr_vm).cast(),
            size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        ))?;
        cvt(libc::listen(fd.as_raw_fd(), 64))?;
        Ok(fd)
    }
}

pub fn accept(listener: &OwnedFd) -> io::Result<File> {
    loop {
        // SAFETY: accept4 with null address is valid.
        let rc = unsafe {
            libc::accept4(
                listener.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        };
        match cvt(rc) {
            // SAFETY: accept4 returned a new owned fd.
            Ok(fd) => return Ok(unsafe { File::from_raw_fd(fd) }),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

/// A close-on-exec pipe: (read end, write end).
pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: fds has room for two descriptors.
    cvt(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) })?;
    // SAFETY: pipe2 returned two new owned fds.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Open a PTY pair: (master, slave). Both are close-on-exec.
pub fn openpty(rows: u16, cols: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let (mut master, mut slave) = (0, 0);
    let ws = winsize(rows, cols);
    // SAFETY: out-pointers are valid; name/termios may be null.
    cvt(unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws,
        )
    })?;
    // SAFETY: openpty returned two new owned fds.
    let (m, s) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [&m, &s] {
        // SAFETY: valid fd.
        cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) })?;
    }
    Ok((m, s))
}

pub fn set_winsize(fd: RawFd, rows: u16, cols: u16) -> io::Result<()> {
    let ws = winsize(rows, cols);
    // SAFETY: valid fd and winsize pointer.
    cvt(unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) }).map(drop)
}

fn winsize(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// How a child process ended.
#[derive(Debug, Clone, Copy)]
pub enum ExitStatus {
    Code(i32),
    Signal(i32),
}

fn decode(status: libc::c_int) -> ExitStatus {
    if libc::WIFSIGNALED(status) {
        ExitStatus::Signal(libc::WTERMSIG(status))
    } else {
        ExitStatus::Code(libc::WEXITSTATUS(status))
    }
}

/// As PID 1 we inherit every orphan, so one thread reaps all children and
/// hands statuses to whoever registered interest in a pid. Orphans nobody
/// registered are simply reaped.
static WAITING: OnceLock<Mutex<HashMap<libc::pid_t, Sender<ExitStatus>>>> = OnceLock::new();

/// Start the reaper. Must be called before any other thread is spawned so
/// that every thread inherits the blocked SIGCHLD mask.
pub fn start_reaper() -> io::Result<()> {
    WAITING.get_or_init(Default::default);
    // SAFETY: standard sigset manipulation and signalfd creation.
    let sfd = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGCHLD);
        cvt(libc::pthread_sigmask(
            libc::SIG_BLOCK,
            &set,
            std::ptr::null_mut(),
        ))?;
        let fd = cvt(libc::signalfd(-1, &set, libc::SFD_CLOEXEC))?;
        File::from_raw_fd(fd)
    };
    thread::spawn(move || {
        let mut info = [0u8; size_of::<libc::signalfd_siginfo>()];
        loop {
            // Blocks until at least one SIGCHLD is pending.
            // SAFETY: reading into a correctly sized buffer.
            unsafe { libc::read(sfd.as_raw_fd(), info.as_mut_ptr().cast(), info.len()) };
            reap_all();
        }
    });
    Ok(())
}

fn reap_all() {
    loop {
        let mut status = 0;
        // SAFETY: valid out-pointer.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            return;
        }
        let tx = WAITING.get().unwrap().lock().unwrap().remove(&pid);
        if let Some(tx) = tx {
            let _ = tx.send(decode(status));
        }
    }
}

/// Run `fork` (which must return the child's pid) and register the child
/// with the reaper. The registry lock is held across the fork, so the reaper
/// cannot collect the child before we know about it.
pub fn spawn_watched(
    fork: impl FnOnce() -> io::Result<libc::pid_t>,
) -> io::Result<(libc::pid_t, Receiver<ExitStatus>)> {
    let mut waiting = WAITING.get().unwrap().lock().unwrap();
    let pid = fork()?;
    let (tx, rx) = mpsc::channel();
    waiting.insert(pid, tx);
    Ok((pid, rx))
}
