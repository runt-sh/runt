//! runt: tiny, fast microVMs for agents and humans.

mod build;
mod client;
mod error;
mod mcp;
mod mounts;
mod names;
mod ops;
mod ports;
mod project;
mod recipe;
mod router;
mod state;
mod tar;
mod term;
mod vm;
mod volumes;

use std::io::Write;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::json;

use crate::error::{CliError, EXIT_RUNT_ERROR, Result};
use crate::state::Status;

#[derive(Parser)]
#[command(
    name = "runt",
    version,
    about = "Tiny, fast microVMs for agents and humans",
    after_help = "Every command accepts --json for machine-readable output.\n\
                  `runt exec` exits with the guest command's exit code; runt's own errors exit 125."
)]
struct Cli {
    /// Machine-readable JSON output (errors go to stderr as JSON too)
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create and boot a new VM
    New {
        /// VM name (default: random, e.g. "nimble-shrew")
        name: Option<String>,
        /// Virtual CPUs
        #[arg(long, default_value_t = 2)]
        cpus: u8,
        /// Memory, e.g. 512M or 2G
        #[arg(long, default_value = "1G", value_parser = parse_mem)]
        mem: u32,
        /// Networking: nat (outbound internet) or none (fully offline)
        #[arg(long, value_enum, default_value_t = state::NetMode::Nat)]
        net: state::NetMode,
        /// Only allow outbound connections to this domain (exact name),
        /// '*.domain' (any subdomain) or IPv4 address/network. Repeatable.
        /// Default: the whole public internet.
        #[arg(long = "allow", value_name = "DOMAIN|IP[/LEN]")]
        allow: Vec<String>,
        /// Also allow private networks around this machine (LAN, Tailscale;
        /// never this machine's own loopback)
        #[arg(long)]
        allow_lan: bool,
        /// Share a host directory: SRC[:DST][:ro]. DST defaults to the same
        /// path as on the host. Repeatable.
        #[arg(short = 'm', long = "mount", value_name = "SRC[:DST][:ro]")]
        mounts: Vec<String>,
        /// Serve this guest port at http://NAME.runt.localhost
        #[arg(long, value_name = "PORT")]
        http: Option<u16>,
    },
    /// Run a command in a VM
    #[command(trailing_var_arg = true)]
    Exec {
        vm: String,
        /// Allocate a terminal (default: when stdin and stdout are terminals)
        #[arg(short = 't', long, conflicts_with = "no_tty")]
        tty: bool,
        /// Never allocate a terminal
        #[arg(short = 'T', long)]
        no_tty: bool,
        /// Set an environment variable (KEY=VALUE, or KEY to copy it from here)
        #[arg(short = 'e', long = "env", value_name = "KEY[=VALUE]")]
        env: Vec<String>,
        /// Working directory inside the VM (default: the same path as here
        /// when this directory is shared with the VM, else /root)
        #[arg(short = 'w', long = "workdir")]
        workdir: Option<String>,
        /// Command and arguments
        #[arg(required = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Open an interactive shell in a VM
    Shell { vm: String },
    /// List VMs
    #[command(alias = "list")]
    Ls,
    /// Boot a stopped VM
    Start { vm: String },
    /// Shut a VM down (its disk is kept)
    Stop {
        vm: String,
        /// Kill immediately instead of shutting down cleanly
        #[arg(short, long)]
        force: bool,
    },
    /// Delete a VM and its disk
    #[command(alias = "remove")]
    Rm {
        vm: String,
        /// Stop it first if it is running
        #[arg(short, long)]
        force: bool,
    },
    /// Print a VM's console log, a service's output, or refused connections
    Logs {
        vm: String,
        /// Show connections and DNS lookups the VM's network policy refused
        #[arg(long, conflicts_with = "service")]
        egress: bool,
        /// Show this service's output (services come from runt.toml)
        #[arg(short, long, value_name = "NAME")]
        service: Option<String>,
        /// Keep printing new output (with --service)
        #[arg(short, long, requires = "service")]
        follow: bool,
    },
    /// Build the image in runt.toml (here or in a parent directory); steps
    /// whose inputs haven't changed come from the cache
    Build,
    /// Build runt.toml and run it: create or update the project's VM and
    /// start its services
    Up,
    /// Stop the project's VM (its disk is kept)
    Down {
        /// Remove the VM and its disk instead (volumes are kept)
        #[arg(long)]
        rm: bool,
        /// With --rm: delete the project's volumes, and their data, too
        #[arg(long, requires = "rm")]
        volumes: bool,
    },
    /// List or delete volumes (persistent data from runt.toml's [volumes])
    #[command(subcommand, alias = "volumes")]
    Volume(VolumeCmd),
    /// List ports forwarded from a VM to this machine (automatic: any port
    /// the guest listens on appears on 127.0.0.1)
    #[command(alias = "ports")]
    Port { vm: String },
    /// Serve the Model Context Protocol on stdin/stdout, so AI agents can
    /// create and use VMs as tools
    Mcp {
        /// Directory agents may share into VMs (repeatable; default: the
        /// current directory)
        #[arg(long = "mount-root", value_name = "DIR")]
        mount_roots: Vec<std::path::PathBuf>,
        /// Let agents give VMs access to private networks (LAN, Tailscale)
        #[arg(long)]
        allow_lan: bool,
        /// Also let agents use and remove this existing VM (repeatable).
        /// By default they only see VMs they created.
        #[arg(long = "vm", value_name = "NAME")]
        vms: Vec<String>,
        /// Let agents use and remove all of your VMs
        #[arg(long, conflicts_with = "vms")]
        all_vms: bool,
    },
    /// Print the runt skill for AI coding agents, or install it for Claude
    /// Code (~/.claude/skills/runt/SKILL.md)
    Skill {
        /// Write it to DIR/runt/SKILL.md instead of printing it
        #[arg(long)]
        install: bool,
        /// Skills directory for --install (default: ~/.claude/skills)
        #[arg(long, value_name = "DIR", requires = "install")]
        dir: Option<std::path::PathBuf>,
    },
    #[command(name = "__vmm", hide = true)]
    Vmm { name: String },
    #[command(name = "__router", hide = true)]
    Router,
    /// Apply a VM's sandbox to this process and try things it should and
    /// shouldn't be able to do (for tests).
    #[command(name = "__sandbox-check", hide = true)]
    SandboxCheck {
        name: String,
        #[arg(long)]
        read: Vec<std::path::PathBuf>,
        #[arg(long)]
        write: Vec<std::path::PathBuf>,
        #[arg(long)]
        connect: Vec<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
enum VolumeCmd {
    /// List volumes
    #[command(alias = "list")]
    Ls,
    /// Delete a VM's volume and its data, or all of the VM's volumes
    #[command(alias = "remove")]
    Rm { vm: String, name: Option<String> },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    match run(cli) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            e.print(json);
            ExitCode::from(EXIT_RUNT_ERROR as u8)
        }
    }
}

fn run(cli: Cli) -> Result<i32> {
    let json = cli.json;
    match cli.cmd {
        Cmd::New {
            name,
            cpus,
            mem,
            net,
            allow,
            allow_lan,
            mounts,
            http,
        } => {
            let cwd = std::env::current_dir()?;
            let mounts = mounts
                .iter()
                .map(|m| mounts::parse(m, &cwd))
                .collect::<Result<Vec<_>>>()?;
            if let Some(home) = std::env::var_os("HOME")
                && mounts.iter().any(|m| m.src == std::path::Path::new(&home))
            {
                eprintln!("runt: warning: sharing your entire home directory with the VM");
            }
            let (rec, boot_ms) = ops::new_vm(ops::NewSpec {
                name,
                cpus,
                mem_mib: mem,
                net,
                egress: state::Egress::new(&allow, allow_lan)?,
                mounts,
                created_by: None,
                http,
                ..Default::default()
            })?;
            if json {
                print_json(ops::created_json(&rec, boot_ms));
            } else {
                println!("{}", rec.name);
                for m in &rec.mounts {
                    let ro = if m.read_only { " (read-only)" } else { "" };
                    eprintln!("mounted {} at {}{ro}", m.src.display(), m.dst.display());
                }
                if let Some(e) = ops::egress_summary(&rec) {
                    eprintln!("network: {e}");
                }
                if let Some(url) = router::url(&rec) {
                    eprintln!("url: {url} (port {} in the VM)", http.unwrap_or(0));
                }
                eprintln!("booted in {boot_ms} ms; try `runt shell {}`", rec.name);
                warn_if_unsandboxed(&rec.name);
            }
            Ok(0)
        }
        Cmd::Exec {
            vm: name,
            tty,
            no_tty,
            env,
            workdir,
            command,
        } => {
            let tty = !json && !no_tty && (tty || (term::is_tty(0) && term::is_tty(1)));
            let env = env
                .iter()
                .map(|e| parse_env(e))
                .collect::<Result<Vec<_>>>()?;
            exec(&name, command, env, workdir, tty, json)
        }
        Cmd::Shell { vm: name } => {
            let argv = [
                "/bin/sh",
                "-c",
                "if command -v bash >/dev/null; then exec bash -l; else exec sh -l; fi",
            ];
            exec(
                &name,
                argv.map(String::from).to_vec(),
                vec![],
                None,
                true,
                false,
            )
        }
        Cmd::Ls => {
            let vms = state::list()?;
            if json {
                let items: Vec<_> = vms.iter().map(ops::vm_json).collect();
                print_json(json!(items));
            } else if vms.is_empty() {
                eprintln!("no VMs yet; create one with `runt new`");
            } else {
                println!(
                    "{:<24} {:<8} {:>4} {:>7}  CREATED",
                    "NAME", "STATUS", "CPUS", "MEM"
                );
                for r in &vms {
                    println!(
                        "{:<24} {:<8} {:>4} {:>6}M  {}",
                        r.name,
                        state::status(r).as_str(),
                        r.cpus,
                        r.mem_mib,
                        r.created
                    );
                }
            }
            Ok(0)
        }
        Cmd::Start { vm: name } => {
            let mut rec = state::load(&name)?;
            let boot_ms = vm::start(&mut rec)?;
            report(json, &name, Status::Running, Some(boot_ms));
            Ok(0)
        }
        Cmd::Stop { vm: name, force } => {
            let mut rec = state::load(&name)?;
            vm::stop(&mut rec, force)?;
            report(json, &name, Status::Stopped, None);
            Ok(0)
        }
        Cmd::Rm { vm: name, force } => {
            vm::remove(&name, force)?;
            if json {
                print_json(json!({ "name": name, "status": "removed" }));
            } else if volumes::list().iter().any(|v| v.vm == name) {
                eprintln!(
                    "kept its volumes; delete them with `runt volume rm {name}` (or use `runt down --rm --volumes`)"
                );
            }
            Ok(0)
        }
        Cmd::Volume(VolumeCmd::Ls) => {
            let vols = volumes::list();
            let records = state::list()?;
            let path = |vm: &str, name: &str| {
                records
                    .iter()
                    .find(|r| r.name == vm)
                    .and_then(|r| r.volumes.iter().find(|v| v.name == name))
                    .map(|v| v.path.clone())
            };
            if json {
                let items: Vec<_> = vols
                    .iter()
                    .map(|v| {
                        json!({
                            "vm": v.vm, "name": v.name, "path": path(&v.vm, &v.name),
                            "size_mib": v.size >> 20, "used_bytes": v.used,
                            "project": volumes::owner(&v.vm),
                        })
                    })
                    .collect();
                print_json(json!(items));
            } else if vols.is_empty() {
                eprintln!("no volumes; declare them in runt.toml under [volumes]");
            } else {
                println!(
                    "{:<24} {:<16} {:>6} {:>8}  PATH",
                    "VM", "NAME", "SIZE", "USED"
                );
                for v in &vols {
                    println!(
                        "{:<24} {:<16} {:>6} {:>7}M  {}",
                        v.vm,
                        v.name,
                        volumes::format_size(v.size >> 20),
                        v.used.div_ceil(1 << 20),
                        path(&v.vm, &v.name).unwrap_or_else(|| "-".into())
                    );
                }
            }
            Ok(0)
        }
        Cmd::Volume(VolumeCmd::Rm {
            vm: name,
            name: vol,
        }) => {
            let gone = volumes::remove(&name, vol.as_deref())?;
            if json {
                print_json(json!({ "vm": name, "removed": gone }));
            } else if gone.is_empty() {
                eprintln!("{name} has no volumes");
            } else {
                eprintln!("deleted {}", gone.join(", "));
            }
            Ok(0)
        }
        Cmd::Logs {
            vm: name,
            service: Some(service),
            follow,
            ..
        } => service_logs(&name, &service, follow, json),
        Cmd::Logs {
            vm: name,
            egress: true,
            ..
        } => {
            state::load(&name)?;
            let log = std::fs::read_to_string(state::egress_log_path(&name)).unwrap_or_default();
            if json {
                let items: Vec<_> = log.lines().filter_map(parse_denial).collect();
                print_json(json!({ "name": name, "denied": items }));
            } else {
                print!("{log}");
            }
            Ok(0)
        }
        Cmd::Logs { vm: name, .. } => {
            state::load(&name)?;
            let log = std::fs::read(vm::console_log(&name)).unwrap_or_default();
            if json {
                print_json(json!({ "name": name, "console": String::from_utf8_lossy(&log) }));
            } else {
                std::io::stdout().write_all(&log)?;
            }
            Ok(0)
        }
        Cmd::Build => {
            let r = recipe::load(&recipe::find(&std::env::current_dir()?)?)?;
            let b = build::build(&r, !json)?;
            if json {
                print_json(json!({
                    "name": r.name, "layers": b.layers, "steps": r.steps.len(),
                    "cached": b.cached, "ms": b.ms, "log": b.log,
                }));
            }
            Ok(0)
        }
        Cmd::Up => {
            let r = recipe::load(&recipe::find(&std::env::current_dir()?)?)?;
            let up = project::up(&r, !json)?;
            let name = &up.rec.name;
            let maps = ports::read(name);
            if json {
                print_json(json!({
                    "name": name, "status": "running", "action": up.action,
                    "reason": up.reason, "boot_ms": up.boot_ms,
                    "build": { "layers": up.built.layers, "cached": up.built.cached,
                               "ms": up.built.ms, "log": up.built.log },
                    "services": ops::services_json(&up.services),
                    "url": router::url(&up.rec), "volumes": up.rec.volumes,
                    "ports": maps.iter()
                        .map(|m| json!({ "guest": m.guest, "host": m.host, "url": m.url() }))
                        .collect::<Vec<_>>(),
                }));
            } else {
                let how = match (up.action, up.reason, up.boot_ms) {
                    ("unchanged", ..) => "already up to date".to_string(),
                    (a, Some(why), Some(ms)) => format!("{a} ({why}) and booted in {ms} ms"),
                    (a, Some(why), None) => format!("{a} ({why})"),
                    (a, None, Some(ms)) => format!("{a} and booted in {ms} ms"),
                    (a, None, None) => a.to_string(),
                };
                eprintln!("{name} is up: {how}");
                if up.action == "recreated" {
                    eprintln!("  (the VM's own disk starts fresh with a new image)");
                }
                for s in &up.services {
                    eprintln!("  service {}", project::describe(s));
                }
                for v in &up.rec.volumes {
                    eprintln!(
                        "  volume {} at {} ({})",
                        v.name,
                        v.path,
                        volumes::format_size(v.size_mib)
                    );
                }
                for m in &maps {
                    eprintln!("  {} -> port {} in the VM", m.url(), m.guest);
                }
                if let Some(url) = router::url(&up.rec) {
                    eprintln!("url: {url}");
                }
                if let Some(s) = up.services.first() {
                    eprintln!("logs: runt logs {name} -s {}", s.name);
                }
            }
            Ok(0)
        }
        Cmd::Down { rm, volumes } => {
            let r = recipe::load(&recipe::find(&std::env::current_dir()?)?)?;
            let existed = project::down(&r, rm, volumes)?;
            let status = match (existed, rm) {
                (false, _) => "absent",
                (true, true) => "removed",
                (true, false) => "stopped",
            };
            if json {
                print_json(json!({ "name": r.name, "status": status }));
            } else {
                eprintln!("{}: {status}", r.name);
            }
            Ok(0)
        }
        Cmd::Port { vm: name } => {
            let rec = state::load(&name)?;
            let maps = if state::status(&rec) == Status::Running {
                ports::read(&name)
            } else {
                vec![]
            };
            if json {
                let items: Vec<_> = maps
                    .iter()
                    .map(|m| json!({ "guest": m.guest, "host": m.host, "url": m.url() }))
                    .collect();
                print_json(json!(items));
            } else if maps.is_empty() {
                eprintln!("no ports forwarded; start a server in the VM and it will appear here");
            } else {
                println!("{:<7} {:<7} URL", "GUEST", "HOST");
                for m in &maps {
                    println!("{:<7} {:<7} {}", m.guest, m.host, m.url());
                }
            }
            Ok(0)
        }
        Cmd::SandboxCheck {
            name,
            read,
            write,
            connect,
        } => {
            let rec = state::load(&name)?;
            let assets = state::assets()?;
            let policy = vm::sandbox_policy(&rec, &assets, &state::vm_dir(&name));
            let status = runt_sandbox::apply(&policy)?;
            let outcome = |r: std::io::Result<()>| match r {
                Ok(()) => "allowed".to_string(),
                Err(e) => format!("denied: {e}"),
            };
            let mut results = serde_json::Map::new();
            for p in read {
                let r = std::fs::read(&p).map(drop);
                results.insert(format!("read {}", p.display()), json!(outcome(r)));
            }
            for p in write {
                let r = std::fs::write(&p, b"x");
                results.insert(format!("write {}", p.display()), json!(outcome(r)));
            }
            for p in connect {
                let r = std::os::unix::net::UnixStream::connect(&p).map(drop);
                results.insert(format!("connect {}", p.display()), json!(outcome(r)));
            }
            let r = std::process::Command::new("/bin/true").status().map(drop);
            results.insert("exec /bin/true".into(), json!(outcome(r)));
            // SAFETY: unshare(2) with a flag constant.
            let rc = unsafe { libc::unshare(libc::CLONE_NEWUSER) };
            let r = if rc == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            };
            results.insert("unshare user namespace".into(), json!(outcome(r)));
            print_json(json!({
                "landlock": status.landlock.as_str(),
                "landlock_abi": status.landlock_abi,
                "seccomp": status.seccomp,
                "results": results,
            }));
            Ok(0)
        }
        Cmd::Mcp {
            mount_roots,
            allow_lan,
            vms,
            all_vms,
        } => mcp::serve(mcp::Config::new(mount_roots, allow_lan, vms, all_vms)?).map(|()| 0),
        Cmd::Skill { install: false, .. } => {
            if json {
                print_json(json!({ "skill": SKILL }));
            } else {
                print!("{SKILL}");
            }
            Ok(0)
        }
        Cmd::Skill { dir, .. } => {
            let dir = match dir {
                Some(d) => d,
                None => std::path::PathBuf::from(std::env::var_os("HOME").ok_or_else(|| {
                    CliError::new("no_home", "HOME is not set").hint("pass --dir")
                })?)
                .join(".claude/skills"),
            };
            let path = dir.join("runt/SKILL.md");
            let status = match std::fs::read_to_string(&path) {
                Ok(old) if old == SKILL => "unchanged",
                Ok(_) => "updated",
                Err(_) => "installed",
            };
            if status != "unchanged" {
                std::fs::create_dir_all(dir.join("runt"))?;
                std::fs::write(&path, SKILL)?;
            }
            if json {
                print_json(json!({ "path": path, "status": status }));
            } else {
                eprintln!("{status} {}", path.display());
            }
            Ok(0)
        }
        Cmd::Vmm { name } => vm::supervise(&name).map(|()| 0),
        Cmd::Router => router::serve().map(|()| 0),
    }
}

fn exec(
    name: &str,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    cwd: Option<String>,
    tty: bool,
    json: bool,
) -> Result<i32> {
    let rec = state::load(name)?;
    if state::status(&rec) != Status::Running {
        return Err(
            CliError::new("vm_not_running", format!("VM {name:?} is not running"))
                .hint(format!("start it with `runt start {name}`")),
        );
    }
    let cwd = cwd.or_else(|| {
        let here = std::env::current_dir().ok()?;
        mounts::default_workdir(&here, &rec.mounts).map(|p| p.to_string_lossy().into_owned())
    });
    let conn = client::connect(name, &state::socket_path(name))?;
    let mut env = ops::exec_env(&rec, env);
    if tty && let Ok(t) = std::env::var("TERM") {
        env.insert(0, ("TERM".into(), t));
    }
    let mut out = if json {
        client::Output::Capture {
            stdout: vec![],
            stderr: vec![],
        }
    } else {
        client::Output::Stream
    };
    let code = client::exec(
        conn,
        client::ExecOpts {
            argv,
            env,
            cwd,
            tty,
        },
        &mut out,
    )?;
    if let client::Output::Capture { stdout, stderr } = out {
        print_json(json!({
            "exit_code": code,
            "stdout": String::from_utf8_lossy(&stdout),
            "stderr": String::from_utf8_lossy(&stderr),
        }));
    }
    Ok(code)
}

/// `runt logs VM --service NAME`: the service's log, read inside the VM.
fn service_logs(name: &str, service: &str, follow: bool, json: bool) -> Result<i32> {
    let rec = state::load(name)?;
    if !rec.services.iter().any(|s| s.name == service) {
        let names: Vec<_> = rec.services.iter().map(|s| s.name.as_str()).collect();
        let hint = if names.is_empty() {
            "this VM has no services; they come from runt.toml (`runt up`)".to_string()
        } else {
            format!("its services: {}", names.join(", "))
        };
        return Err(CliError::new(
            "no_service",
            format!("VM {name:?} has no service {service:?}"),
        )
        .hint(hint));
    }
    let log = format!("/var/log/runt/{service}.log");
    let argv = if follow {
        vec!["tail", "-n", "+1", "-F", &log]
    } else {
        // A service that hasn't written anything has no log yet.
        vec!["sh", "-c", "[ ! -e \"$1\" ] || cat \"$1\"", "sh", &log]
    };
    let argv = argv.into_iter().map(String::from).collect();
    if json {
        let conn = client::connect(name, &state::socket_path(name))?;
        let r = client::run_captured(
            conn,
            client::ExecOpts {
                argv,
                env: vec![],
                cwd: None,
                tty: false,
            },
            vec![],
            std::time::Duration::from_secs(30),
            1 << 20,
        )?;
        print_json(json!({ "name": name, "service": service, "log": r.stdout.text() }));
        return Ok(0);
    }
    exec(name, argv, vec![], Some("/".into()), false, false)
}

fn report(json: bool, name: &str, status: Status, boot_ms: Option<u128>) {
    if json {
        let mut v = json!({ "name": name, "status": status });
        if let Some(ms) = boot_ms {
            v["boot_ms"] = json!(ms);
        }
        print_json(v);
    } else if let Some(ms) = boot_ms {
        eprintln!("{name}: {} ({ms} ms)", status.as_str());
    } else {
        eprintln!("{name}: {}", status.as_str());
    }
}

/// Tell humans when the VM's host process couldn't be confined.
fn warn_if_unsandboxed(name: &str) {
    match vm::SandboxStatus::read(name) {
        Some(s) if s.landlock == "full" && s.seccomp => {}
        Some(s) => eprintln!(
            "runt: warning: the VM's host process is only partly sandboxed \
             (landlock: {}, seccomp: {}); a newer kernel enables full isolation",
            s.landlock, s.seccomp
        ),
        None => eprintln!("runt: warning: unknown sandbox status for the VM's host process"),
    }
}

/// The agent skill, shipped inside the binary so it matches this version.
const SKILL: &str = include_str!("../../../skills/runt/SKILL.md");

fn print_json(v: serde_json::Value) {
    println!("{v}");
}

fn parse_env(s: &str) -> Result<(String, String)> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.into(), v.into())),
        None if !s.is_empty() => Ok((s.into(), std::env::var(s).unwrap_or_default())),
        _ => Err(CliError::new(
            "invalid_env",
            format!("invalid environment variable {s:?}"),
        )),
    }
}

/// Parse sizes like "512M", "2G", "1024" (MiB) into MiB.
pub fn parse_mem(s: &str) -> std::result::Result<u32, String> {
    let s = s.trim();
    let (num, mult) = match s.char_indices().last() {
        Some((i, 'G' | 'g')) => (&s[..i], 1024),
        Some((i, 'M' | 'm')) => (&s[..i], 1),
        _ => (s, 1),
    };
    let n: u32 = num
        .parse()
        .map_err(|_| format!("invalid size {s:?} (try 512M or 2G)"))?;
    let mib = n.checked_mul(mult).ok_or("size too large")?;
    if mib < 128 {
        return Err("at least 128M of memory is needed".into());
    }
    Ok(mib)
}

/// `[2026-10-04T12:00:00Z]: egress policy denied connect to 1.2.3.4:443`
fn parse_denial(line: &str) -> Option<serde_json::Value> {
    let (time, rest) = line.strip_prefix('[')?.split_once("]: ")?;
    let rest = rest.strip_prefix("egress policy denied ")?;
    let (op, dest) = rest.split_once(' ')?;
    let dest = dest.strip_prefix("to ").unwrap_or(dest);
    Some(json!({ "time": time, "op": op, "dest": dest }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mem() {
        assert_eq!(parse_mem("512M"), Ok(512));
        assert_eq!(parse_mem("2G"), Ok(2048));
        assert_eq!(parse_mem("1024"), Ok(1024));
        assert!(parse_mem("64M").is_err());
        assert!(parse_mem("lots").is_err());
    }

    #[test]
    fn parses_env() {
        assert_eq!(parse_env("A=b=c").unwrap(), ("A".into(), "b=c".into()));
        assert!(parse_env("=x").is_err());
    }

    #[test]
    fn cli_is_well_formed() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_denials() {
        let v = parse_denial("[2026-10-04T06:18:29Z]: egress policy denied connect to 1.1.1.1:80")
            .unwrap();
        assert_eq!(v["op"], "connect");
        assert_eq!(v["dest"], "1.1.1.1:80");
        assert_eq!(v["time"], "2026-10-04T06:18:29Z");
        let v = parse_denial("[t]: egress policy denied resolve example.com").unwrap();
        assert_eq!(v["dest"], "example.com");
        assert!(parse_denial("garbage").is_none());
    }
}
