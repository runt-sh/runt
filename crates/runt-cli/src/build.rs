//! `runt build`: turn a recipe's steps into cached image layers.
//!
//! Each step becomes one erofs layer, keyed by everything that can change
//! what it produces: the base image, the steps before it, the step itself,
//! the recipe's `[env]` and, for copies, the exact bytes copied. Steps whose
//! layer is cached are skipped. The rest run in a throwaway build VM (the
//! guest half is `build.sh`), which writes each new layer into a directory
//! shared with it for this build only. runt moves the layers into the cache
//! after the VM has shut down, and takes only the ones it asked for, so a
//! build can't plant layers for other steps or projects.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant, SystemTime};

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::client;
use crate::error::{CliError, Result};
use crate::mounts::Mount;
use crate::ops;
use crate::recipe::{Recipe, Step};
use crate::state::{self, NetMode};
use crate::tar;
use crate::vm;

const SCRIPT: &str = include_str!("build.sh");
/// Part of every layer key: bump it when the same step would build a
/// different layer (a new build.sh, say).
const FORMAT: &str = "runt-layer/1";
/// Unreferenced layers younger than this are kept, in case a concurrent
/// build is about to record them.
const GC_GRACE: Duration = Duration::from_secs(3600);

/// A finished build.
pub struct Built {
    /// Layer keys, bottom first.
    pub layers: Vec<String>,
    /// Steps that were already cached.
    pub cached: usize,
    pub ms: u128,
    /// Full output of this build.
    pub log: PathBuf,
}

/// Where build progress goes besides the build log.
#[derive(Clone, Copy)]
pub enum Output<'a> {
    /// Nowhere else (`--json`).
    Quiet,
    /// Everything, to stderr, as it happens.
    Stderr,
    /// Only each step's line as it starts: (step index, steps, line).
    Steps(&'a (dyn Fn(usize, usize, &str) + Sync)),
}

struct Log<'a> {
    file: Option<File>,
    out: Output<'a>,
}

impl Log<'_> {
    fn write(&mut self, d: &[u8]) {
        if let Some(f) = self.file.as_mut() {
            let _ = f.write_all(d);
        }
        if let Output::Stderr = self.out {
            let mut e = io::stderr().lock();
            let _ = e.write_all(d).and_then(|_| e.flush());
        }
    }

    fn line(&mut self, s: &str) {
        self.write(format!("{s}\n").as_bytes());
    }

    /// The line that starts step `i` of `n`.
    fn step(&mut self, i: usize, n: usize, s: &str) {
        self.line(s);
        if let Output::Steps(f) = self.out {
            f(i, n, s);
        }
    }
}

/// The build log of the project in `dir`.
pub fn log_path(dir: &Path) -> PathBuf {
    state::projects_dir().join(format!("{}.log", project_id(dir)))
}

/// Held while building or changing a project's VM, so that two runs for
/// one project (two terminals, or an agent and a person) take turns.
pub struct ProjectLock(#[allow(dead_code)] File);

pub fn lock(r: &Recipe, out: Output) -> Result<ProjectLock> {
    fs::create_dir_all(state::projects_dir())?;
    let path = state::projects_dir().join(format!("{}.lock", project_id(&r.dir)));
    let f = File::create(path)?;
    let flock = |how| {
        // SAFETY: flock(2) on a file we own.
        unsafe { libc::flock(f.as_raw_fd(), how) == 0 }
    };
    if !flock(libc::LOCK_EX | libc::LOCK_NB) {
        if let Output::Stderr = out {
            eprintln!("runt: waiting for another build of {}", r.name);
        }
        if !flock(libc::LOCK_EX) {
            return Err(io::Error::last_os_error().into());
        }
    }
    Ok(ProjectLock(f))
}

pub fn build(r: &Recipe, out: Output) -> Result<Built> {
    let _lock = lock(r, out)?;
    build_locked(r, out)
}

/// `build`, for a caller that holds the project's lock.
pub fn build_locked(r: &Recipe, out: Output) -> Result<Built> {
    let t0 = Instant::now();
    let assets = state::assets()?;
    for d in [state::layers_dir(), state::projects_dir()] {
        fs::create_dir_all(d)?;
    }
    let log_path = log_path(&r.dir);
    let mut log = Log {
        file: File::create(&log_path).ok(),
        out,
    };
    let n = r.steps.len();
    log.line(&format!(
        "building {} ({n} step{})",
        r.name,
        if n == 1 { "" } else { "s" }
    ));

    let keys = layer_keys(r, &assets.image)?;
    let cached = keys
        .iter()
        .take_while(|k| state::layer_path(k).is_file())
        .count();
    for (i, s) in r.steps.iter().enumerate().take(cached) {
        log.step(i, n, &format!("[{}/{n}] {} (cached)", i + 1, s.describe()));
    }
    let result = if cached < n {
        run_steps(r, &keys, cached, &mut log, &log_path)
    } else {
        Ok(())
    };
    // Even a failed build records what it got through, so the next one
    // resumes from there.
    let done = keys
        .iter()
        .take_while(|k| state::layer_path(k).is_file())
        .count();
    record(r, &keys[..done]);
    gc();
    result?;
    let ms = t0.elapsed().as_millis();
    log.line(&format!(
        "built {} in {:.1} s ({cached} of {n} steps cached)",
        r.name,
        ms as f64 / 1000.0
    ));
    Ok(Built {
        layers: keys,
        cached,
        ms,
        log: log_path,
    })
}

/// The cache key of every step's layer.
fn layer_keys(r: &Recipe, base: &Path) -> Result<Vec<String>> {
    let meta = fs::metadata(base)?;
    let base_id = format!("{}:{}:{}", meta.len(), meta.mtime(), meta.mtime_nsec());
    let mut prev = hash(&[FORMAT.as_bytes(), b"base", base_id.as_bytes()]);
    let env = serde_json::to_vec(&r.env).unwrap();
    let mut keys = Vec::new();
    for (i, step) in r.steps.iter().enumerate() {
        let input = match step {
            Step::Copy { paths, exclude, .. } => {
                let mut h = HashWriter(Sha256::new());
                tar::write(&r.dir, paths, exclude, &mut h).map_err(|e| copy_failed(i, &e))?;
                hex(&h.0.finalize())
            }
            Step::Run { .. } => String::new(),
        };
        let step_json = serde_json::to_vec(step).unwrap();
        prev = hash(&[
            FORMAT.as_bytes(),
            prev.as_bytes(),
            &step_json,
            &env,
            input.as_bytes(),
        ]);
        keys.push(prev.clone());
    }
    Ok(keys)
}

fn copy_failed(i: usize, e: &io::Error) -> CliError {
    CliError::new(
        "build_failed",
        format!("build step {}: cannot read files to copy: {e}", i + 1),
    )
}

/// Prefix of build VM names; the rest is the building process's pid and a
/// sequence number (`runt mcp` may run several builds at once).
const BUILD_VM: &str = "runt-build-";

/// The build VM; removed (with its shared directories) when dropped.
struct BuildVm {
    name: String,
    io: PathBuf,
}

impl BuildVm {
    fn new() -> BuildVm {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("{BUILD_VM}{}-{n}", std::process::id());
        let io = state::cache_dir().join("tmp").join(&name);
        lock_active().push((name.clone(), io.clone()));
        on_interrupt_remove();
        BuildVm { name, io }
    }
}

impl Drop for BuildVm {
    fn drop(&mut self) {
        let _ = vm::remove(&self.name, true);
        remove_dirs(&self.io);
        lock_active().retain(|(n, _)| *n != self.name);
    }
}

/// Build VMs of this process, for cleaning up after an interrupt.
static ACTIVE: Mutex<Vec<(String, PathBuf)>> = Mutex::new(Vec::new());

fn lock_active() -> std::sync::MutexGuard<'static, Vec<(String, PathBuf)>> {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Remove this process's build VMs; for when it is about to exit with
/// builds still running (`runt mcp` when its client goes away).
pub fn abort_all() {
    INTERRUPTED.store(true, Ordering::SeqCst);
    for (name, io) in lock_active().drain(..) {
        let _ = vm::remove(&name, true);
        remove_dirs(&io);
    }
}

/// Remove build VMs whose process is gone (killed outright, say).
fn remove_orphans() {
    for rec in state::list().unwrap_or_default() {
        let Some(pid) = rec
            .name
            .strip_prefix(BUILD_VM)
            .and_then(|r| r.split('-').next())
            .and_then(|p| p.parse::<u32>().ok())
        else {
            continue;
        };
        if rec.created_by.as_deref() != Some("build") || Path::new(&format!("/proc/{pid}")).exists()
        {
            continue;
        }
        let _ = vm::remove(&rec.name, true);
        remove_dirs(&state::cache_dir().join("tmp").join(&rec.name));
    }
}

/// The build VM's shares: `io` and the cached layers beside it.
fn layers_dir(io: &Path) -> PathBuf {
    io.with_extension("layers")
}

fn remove_dirs(io: &Path) {
    let _ = fs::remove_dir_all(io);
    let _ = fs::remove_dir_all(layers_dir(io));
}

/// Build steps `from..` in a build VM, leaving their layers in the cache.
fn run_steps(
    r: &Recipe,
    keys: &[String],
    from: usize,
    log: &mut Log,
    log_path: &Path,
) -> Result<()> {
    remove_orphans();
    let guard = BuildVm::new();
    let (name, io) = (guard.name.clone(), guard.io.clone());
    remove_dirs(&io);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&io)?;
    let layers = layers_dir(&io);
    vm::link_layers(&layers, keys[..from].iter().map(|k| state::layer_path(k)))?;
    for (i, step) in r.steps.iter().enumerate().skip(from) {
        if let Step::Copy { paths, exclude, .. } = step {
            let mut f = io::BufWriter::new(File::create(io.join(format!("{i}.tar")))?);
            let sum = tar::write(&r.dir, paths, exclude, &mut f).map_err(|e| copy_failed(i, &e))?;
            f.flush()?;
            if !sum.skipped.is_empty() {
                log.line(&format!(
                    "note: step {} skips special files: {}",
                    i + 1,
                    sum.skipped.join(", ")
                ));
            }
        }
    }

    let cpus = std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(2) as u8;
    ops::new_vm(ops::NewSpec {
        name: Some(name.clone()),
        cpus: cpus.max(r.cpus),
        mem_mib: r.mem_mib.max(2048),
        net: NetMode::Nat,
        mounts: vec![
            Mount {
                src: io.canonicalize()?,
                dst: PathBuf::from("/runt/io"),
                read_only: false,
            },
            // Read-only: the links share their files with the cache.
            Mount {
                src: layers.canonicalize()?,
                dst: PathBuf::from("/runt/layers"),
                read_only: true,
            },
        ],
        created_by: Some("build".into()),
        ..Default::default()
    })?;
    let script = |args: Vec<String>| {
        let mut argv = vec![
            "/bin/sh".to_string(),
            "-c".into(),
            SCRIPT.into(),
            "runt-build".into(),
        ];
        argv.extend(args);
        argv
    };
    let run = |argv: Vec<String>, log: &mut Log| -> Result<i32> {
        let conn = client::connect(&name, &state::socket_path(&name))?;
        let r = client::run_streamed(
            conn,
            client::ExecOpts {
                argv,
                env: vec![],
                cwd: None,
                tty: false,
            },
            &mut |d| log.write(d),
        );
        if r.is_err() && INTERRUPTED.load(Ordering::SeqCst) {
            // The VM went away because of Ctrl-C; the interrupt handler is
            // cleaning up and will exit.
            loop {
                std::thread::park();
            }
        }
        r
    };

    let n = r.steps.len();
    let mut failed = None;
    if run(script(vec!["prepare".into(), from.to_string()]), log)? != 0 {
        failed = Some((from, None));
    }
    for (i, step) in r.steps.iter().enumerate().skip(from) {
        if failed.is_some() {
            break;
        }
        log.step(i, n, &format!("[{}/{n}] {}", i + 1, step.describe()));
        let args = match step {
            Step::Run { cmd, cwd } => {
                let mut a = vec!["run".into(), i.to_string(), cwd.clone(), cmd.clone()];
                // Builds never wait for a person at a prompt.
                a.push("DEBIAN_FRONTEND=noninteractive".into());
                a.extend(r.env.iter().map(|(k, v)| format!("{k}={v}")));
                a
            }
            Step::Copy { to, .. } => vec!["copy".into(), i.to_string(), to.clone()],
        };
        let code = run(script(args), log)?;
        if code != 0 {
            failed = Some((i, Some(code)));
        }
    }

    // Collect layers only once the VM is down: nothing in it can swap a
    // file for a symlink between our check and the move.
    let mut rec = state::load(&name)?;
    vm::stop(&mut rec, false)?;
    let built = failed.map_or(n, |(i, _)| i);
    for (i, key) in keys.iter().enumerate().take(built).skip(from) {
        let src = io.join(format!("{i}.erofs"));
        match fs::symlink_metadata(&src) {
            Ok(m) if m.file_type().is_file() => fs::rename(&src, state::layer_path(key))?,
            _ => {
                return Err(CliError::new(
                    "build_failed",
                    format!("build step {} produced no layer", i + 1),
                )
                .hint(format!("see {}", log_path.display())));
            }
        }
    }
    drop(guard);
    match failed {
        None => Ok(()),
        Some((i, code)) => {
            let what = match code {
                Some(c) => format!("build step {} failed (exit code {c})", i + 1),
                None => "the build VM failed to prepare".to_string(),
            };
            Err(
                CliError::new("build_failed", format!("{what}: {}", r.steps[i].describe()))
                    .hint(format!("full output: {}", log_path.display())),
            )
        }
    }
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Ctrl-C during a build: remove build VMs instead of leaving them behind.
fn on_interrupt_remove() {
    static HANDLER: Once = Once::new();
    HANDLER.call_once(|| {
        let Ok(mut sigs) = crate::term::signal_pipe(&[libc::SIGINT, libc::SIGTERM, libc::SIGHUP])
        else {
            return;
        };
        std::thread::spawn(move || {
            if let Some(sig) = crate::term::next_signal(&mut sigs) {
                eprintln!("\nrunt: interrupted; removing build VMs");
                abort_all();
                std::process::exit(128 + sig);
            }
        });
    });
}

/// Remember a project's latest layers, so garbage collection keeps them.
fn record(r: &Recipe, layers: &[String]) {
    let path = state::projects_dir().join(format!("{}.json", project_id(&r.dir)));
    let v = json!({
        "dir": r.dir, "name": r.name, "layers": layers, "built": state::now_rfc3339(),
    });
    let tmp = path.with_extension("json.tmp");
    if fs::write(&tmp, v.to_string()).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
}

/// Delete layers that no VM and no project's latest build uses.
pub fn gc() {
    let mut keep = HashSet::new();
    if let Ok(entries) = fs::read_dir(state::projects_dir()) {
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let Some(v) = fs::read(&path)
                .ok()
                .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
            else {
                continue;
            };
            let alive = v["dir"]
                .as_str()
                .is_some_and(|d| Path::new(d).join(crate::recipe::FILE).is_file());
            if !alive {
                let _ = fs::remove_file(&path);
                let _ = fs::remove_file(path.with_extension("log"));
                continue;
            }
            for k in v["layers"].as_array().into_iter().flatten() {
                if let Some(k) = k.as_str() {
                    keep.insert(k.to_string());
                }
            }
        }
    }
    for rec in state::list().unwrap_or_default() {
        keep.extend(rec.layers.iter().cloned());
    }
    let Ok(entries) = fs::read_dir(state::layers_dir()) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        let stem = name.split('.').next().unwrap_or("");
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > GC_GRACE);
        if old && !keep.contains(stem) {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// A short, stable id for a project directory.
pub fn project_id(dir: &Path) -> String {
    hash(&[dir.as_os_str().as_encoded_bytes()])[..16].to_string()
}

/// SHA-256 of length-prefixed parts, so their boundaries are unambiguous.
fn hash(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct HashWriter(Sha256);

impl Write for HashWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe;

    fn keys_for(text: &str, dir: &Path, base: &Path) -> Vec<String> {
        layer_keys(&recipe::parse(text, dir).unwrap(), base).unwrap()
    }

    #[test]
    fn keys_change_with_what_shapes_a_layer() {
        let dir = std::env::temp_dir().join(format!("runt-keys-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let base = dir.join("base.erofs");
        fs::write(&base, "base").unwrap();
        fs::write(dir.join("app.txt"), "v1").unwrap();
        let text = r#"
            name = "x"
            [build]
            steps = [{ run = "echo a" }, { copy = "app.txt", to = "/app" }, { run = "echo b" }]
        "#;
        let a = keys_for(text, &dir, &base);
        assert_eq!(a.len(), 3);
        assert_eq!(a, keys_for(text, &dir, &base), "keys are stable");

        // Editing a copied file changes that layer and the ones above it.
        fs::write(dir.join("app.txt"), "v2").unwrap();
        let b = keys_for(text, &dir, &base);
        assert_eq!(a[0], b[0]);
        assert_ne!(a[1], b[1]);
        assert_ne!(a[2], b[2]);

        // So does changing a step, [env], or the base image.
        let c = keys_for(&text.replace("echo a", "echo A"), &dir, &base);
        assert!(a.iter().zip(&c).all(|(x, y)| x != y));
        let d = keys_for(&format!("{text}\n[env]\nX = \"1\""), &dir, &base);
        assert_ne!(b[0], d[0]);
        std::thread::sleep(Duration::from_millis(20));
        fs::write(&base, "base2").unwrap();
        assert_ne!(b[0], keys_for(text, &dir, &base)[0]);

        // Things that only affect running the VM don't.
        let e = keys_for(
            &format!("{text}\n[vm]\ncpus = 8\n[services.web]\ncmd = \"x\""),
            &dir,
            &base,
        );
        assert_eq!(&e[..], &keys_for(text, &dir, &base)[..]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
