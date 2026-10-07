//! Services: long-running programs from `runt.toml` that the agent keeps
//! running, restarting them when they exit (with backoff), and logging their
//! output to /var/log/runt/<name>.log.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use runt_proto::{Restart, Service, ServiceStatus};

use crate::sys::{self, ExitStatus};

pub const LOG_DIR: &str = "/var/log/runt";
/// A log is rotated to `<name>.log.1` when it reaches this size.
const LOG_MAX: u64 = 8 << 20;
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A run this long resets the backoff.
const HEALTHY_RUN: Duration = Duration::from_secs(10);
/// How long a service gets to exit after SIGTERM before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(5);

struct Handle {
    spec: Service,
    shared: Arc<Shared>,
    thread: JoinHandle<()>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    cv: Condvar,
}

#[derive(Default)]
struct State {
    pid: Option<libc::pid_t>,
    restarts: u32,
    last_exit: Option<i32>,
    stop: bool,
}

fn services() -> &'static Mutex<HashMap<String, Handle>> {
    static SERVICES: OnceLock<Mutex<HashMap<String, Handle>>> = OnceLock::new();
    SERVICES.get_or_init(Default::default)
}

/// Make `specs` the running set: unchanged services keep running, changed
/// ones are restarted, and ones no longer listed are stopped.
pub fn set(specs: Vec<Service>) -> Vec<ServiceStatus> {
    let mut map = services().lock().unwrap();
    let stale: Vec<String> = map
        .iter()
        .filter(|(name, h)| !specs.iter().any(|s| &s.name == *name && *s == h.spec))
        .map(|(name, _)| name.clone())
        .collect();
    let stopping: Vec<Handle> = stale.iter().filter_map(|n| map.remove(n)).collect();
    for h in &stopping {
        signal_stop(&h.shared);
    }
    for h in stopping {
        finish_stop(&h.shared);
        let _ = h.thread.join();
    }
    for spec in specs {
        if !map.contains_key(&spec.name) {
            let shared = Arc::new(Shared::default());
            let thread = {
                let (spec, shared) = (spec.clone(), shared.clone());
                thread::spawn(move || supervise(spec, shared))
            };
            map.insert(
                spec.name.clone(),
                Handle {
                    spec,
                    shared,
                    thread,
                },
            );
        }
    }
    status(&map)
}

pub fn list() -> Vec<ServiceStatus> {
    status(&services().lock().unwrap())
}

fn status(map: &HashMap<String, Handle>) -> Vec<ServiceStatus> {
    let mut out: Vec<ServiceStatus> = map
        .iter()
        .map(|(name, h)| {
            let st = h.shared.state.lock().unwrap();
            ServiceStatus {
                name: name.clone(),
                pid: st.pid.map(|p| p as u32),
                restarts: st.restarts,
                last_exit: st.last_exit,
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn signal_stop(shared: &Shared) {
    let mut st = shared.state.lock().unwrap();
    st.stop = true;
    if let Some(pid) = st.pid {
        // SAFETY: plain kill(2) on the service's process group.
        unsafe { libc::kill(-pid, libc::SIGTERM) };
    }
    shared.cv.notify_all();
}

/// Wait for a stopping service to exit, killing it after the grace period.
fn finish_stop(shared: &Shared) {
    let st = shared.state.lock().unwrap();
    let (st, timeout) = shared
        .cv
        .wait_timeout_while(st, STOP_GRACE, |st| st.pid.is_some())
        .unwrap();
    if timeout.timed_out()
        && let Some(pid) = st.pid
    {
        // SAFETY: plain kill(2) on the service's process group.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
}

/// One service's lifetime: run it, and run it again when it exits (as its
/// restart policy says) until it is stopped.
fn supervise(spec: Service, shared: Arc<Shared>) {
    let name = &spec.name;
    let mut backoff = BACKOFF_MIN;
    loop {
        let started = Instant::now();
        let exited = {
            let mut st = shared.state.lock().unwrap();
            if st.stop {
                return;
            }
            match spawn(&spec) {
                Ok((pid, exited)) => {
                    eprintln!("runt-agent: service {name} started (pid {pid})");
                    st.pid = Some(pid);
                    Some(exited)
                }
                Err(e) => {
                    eprintln!("runt-agent: service {name}: cannot start: {e}");
                    log_line(name, &format!("runt: cannot start {name}: {e}\n"));
                    st.last_exit = Some(127);
                    None
                }
            }
        };
        let code = match exited.map(|rx| rx.recv()) {
            Some(Ok(ExitStatus::Code(c))) => c,
            Some(Ok(ExitStatus::Signal(s))) => 128 + s,
            Some(Err(_)) => 255,
            None => 127,
        };
        let mut st = shared.state.lock().unwrap();
        st.pid = None;
        st.last_exit = Some(code);
        shared.cv.notify_all();
        if st.stop {
            return;
        }
        let again = match spec.restart {
            Restart::Always => true,
            Restart::OnFailure => code != 0,
            Restart::Never => false,
        };
        if !again {
            eprintln!("runt-agent: service {name} exited ({code})");
            return;
        }
        if started.elapsed() >= HEALTHY_RUN {
            backoff = BACKOFF_MIN;
        }
        eprintln!(
            "runt-agent: service {name} exited ({code}); restarting in {}s",
            backoff.as_secs()
        );
        let (st, _) = shared
            .cv
            .wait_timeout_while(st, backoff, |st| !st.stop)
            .unwrap();
        if st.stop {
            return;
        }
        drop(st);
        shared.state.lock().unwrap().restarts += 1;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Start one run of a service in its own session, with stdin from
/// /dev/null and stdout/stderr going to its log.
fn spawn(spec: &Service) -> io::Result<(libc::pid_t, std::sync::mpsc::Receiver<ExitStatus>)> {
    let env = crate::exec::build_env(&spec.env, false);
    let path_var = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let argv0 = spec
        .argv
        .first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty command"))?;
    let path = crate::exec::resolve(argv0, &path_var).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{argv0}: command not found"),
        )
    })?;
    let cstr = |s: &[u8]| {
        CString::new(s).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in argument"))
    };
    let path_c = cstr(path.as_os_str().as_bytes())?;
    let argv_c: Vec<CString> = spec
        .argv
        .iter()
        .map(|a| cstr(a.as_bytes()))
        .collect::<Result<_, _>>()?;
    let env_c: Vec<CString> = env
        .iter()
        .map(|(k, v)| cstr(format!("{k}={v}").as_bytes()))
        .collect::<Result<_, _>>()?;
    let cwd_c = cstr(spec.cwd.as_deref().unwrap_or("/").as_bytes())?;
    let mut argv_p: Vec<*const libc::c_char> = argv_c.iter().map(|c| c.as_ptr()).collect();
    argv_p.push(std::ptr::null());
    let mut env_p: Vec<*const libc::c_char> = env_c.iter().map(|c| c.as_ptr()).collect();
    env_p.push(std::ptr::null());

    let devnull = File::open("/dev/null")?;
    let (out_r, out_w) = sys::pipe()?;
    let (null_fd, out_fd) = (devnull.as_raw_fd(), out_w.as_raw_fd());
    let (pid, exited) = sys::spawn_watched(|| {
        // SAFETY: fork; the child only makes async-signal-safe calls on
        // memory prepared above, then execs or _exits.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::pthread_sigmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                libc::setsid();
                libc::dup2(null_fd, 0);
                libc::dup2(out_fd, 1);
                libc::dup2(out_fd, 2);
                libc::chdir(cwd_c.as_ptr());
                libc::execve(path_c.as_ptr(), argv_p.as_ptr(), env_p.as_ptr());
                let msg = b"runt: cannot execute the service command\n";
                libc::write(2, msg.as_ptr().cast(), msg.len());
                libc::_exit(127);
            }
        }
        sys::cvt(pid)
    })?;
    drop(out_w);
    let name = spec.name.clone();
    thread::spawn(move || pump_log(&name, out_r));
    Ok((pid, exited))
}

pub fn log_path(name: &str) -> String {
    format!("{LOG_DIR}/{name}.log")
}

fn open_log(name: &str) -> io::Result<File> {
    fs::create_dir_all(LOG_DIR)?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(name))
}

fn log_line(name: &str, line: &str) {
    if let Ok(mut f) = open_log(name) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Copy a service's output into its log until every writer has closed it.
fn pump_log(name: &str, r: OwnedFd) {
    let mut r = File::from(r);
    let mut log = open_log(name).ok();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        let full = log
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .is_some_and(|m| m.len() + n as u64 > LOG_MAX);
        if full {
            let path = log_path(name);
            let _ = fs::rename(&path, format!("{path}.1"));
            log = open_log(name).ok();
        }
        if let Some(f) = log.as_mut() {
            let _ = f.write_all(&buf[..n]);
        }
    }
}
