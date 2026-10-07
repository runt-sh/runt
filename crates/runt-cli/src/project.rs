//! `runt up` and `runt down`: run a project's VM from its runt.toml.
//!
//! The VM is named after the recipe and remembers which project it belongs
//! to. `runt up` builds, then brings the VM in line with the recipe doing as
//! little as it can: services are swapped in place, boot settings (CPUs,
//! memory, shares, volumes, network rules) take a restart, and only a new
//! image means a new VM with a fresh disk. Volumes belong to the project and
//! outlive all of that.

use std::time::{Duration, Instant};

use runt_proto::ServiceStatus;

use crate::build::{self, Built};
use crate::client;
use crate::error::{CliError, Result};
use crate::ops;
use crate::ports;
use crate::recipe::Recipe;
use crate::router;
use crate::state::{self, NetMode, Status, VmRecord};
use crate::vm;
use crate::volumes;

/// How long `runt up` waits for freshly started services to open a port.
const PORT_WAIT: Duration = Duration::from_secs(2);

pub struct Up {
    pub rec: VmRecord,
    pub built: Built,
    /// "created", "recreated", "restarted", "started", "updated" or "unchanged".
    pub action: &'static str,
    pub reason: Option<&'static str>,
    pub boot_ms: Option<u128>,
    pub services: Vec<ServiceStatus>,
}

pub fn up(r: &Recipe, echo: bool) -> Result<Up> {
    // Refuse what we can't do before building or touching the VM.
    volumes::check(&r.name, &r.dir, &r.volumes)?;
    vm::check_devices(&VmRecord {
        name: r.name.clone(),
        layers: r.steps.iter().map(|_| String::new()).collect(),
        volumes: r.volumes.clone(),
        mounts: r.mounts.clone(),
        ..Default::default()
    })?;
    let built = build::build(r, echo)?;
    let spec = || ops::NewSpec {
        name: Some(r.name.clone()),
        cpus: r.cpus,
        mem_mib: r.mem_mib,
        net: NetMode::Nat,
        egress: r.egress.clone(),
        mounts: r.mounts.clone(),
        project: Some(r.dir.clone()),
        layers: built.layers.clone(),
        env: r.run_env.clone(),
        services: r.services.clone(),
        volumes: r.volumes.clone(),
        http: r.http,
        ..Default::default()
    };
    let prepare_volumes = || volumes::prepare(&r.name, &r.dir, &r.volumes);
    let existing = state::vm_dir(&r.name)
        .exists()
        .then(|| state::load(&r.name))
        .transpose()?;
    let Some(mut rec) = existing else {
        prepare_volumes()?;
        let (rec, ms) = ops::new_vm(spec())?;
        return finish(rec, built, "created", None, Some(ms));
    };
    if rec.project.as_deref() != Some(r.dir.as_path()) {
        return Err(CliError::new(
            "vm_exists",
            format!(
                "a VM named {:?} already exists and isn't this project's",
                r.name
            ),
        )
        .hint(format!(
            "change `name` in runt.toml, or remove that VM with `runt rm -f {}`",
            r.name
        )));
    }
    if rec.layers != built.layers {
        vm::remove(&r.name, true)?;
        prepare_volumes()?;
        let (rec, ms) = ops::new_vm(spec())?;
        return finish(rec, built, "recreated", Some("the image changed"), Some(ms));
    }
    let reboot = if (rec.cpus, rec.mem_mib) != (r.cpus, r.mem_mib) {
        Some("CPUs or memory changed")
    } else if rec.mounts != r.mounts {
        Some("shared folders changed")
    } else if rec.volumes != r.volumes {
        Some("volumes changed")
    } else if rec.egress != r.egress || rec.net != NetMode::Nat {
        Some("network rules changed")
    } else {
        None
    };
    let services_changed = rec.env != r.run_env || rec.services != r.services;
    let http_changed = rec.http != r.http;
    let running = state::status(&rec) == Status::Running;
    if running && reboot.is_some() {
        vm::stop(&mut rec, false)?;
    }
    // Volumes grow while the VM is stopped, before the record says so.
    prepare_volumes()?;
    let s = spec();
    (rec.cpus, rec.mem_mib, rec.mounts, rec.egress, rec.net) =
        (s.cpus, s.mem_mib, s.mounts, s.egress, s.net);
    (rec.env, rec.services, rec.volumes, rec.http) = (s.env, s.services, s.volumes, s.http);
    state::save(&rec)?;
    if running && reboot.is_some() {
        let ms = vm::start(&mut rec)?;
        return finish(rec, built, "restarted", reboot, Some(ms));
    }
    if !running {
        let ms = vm::start(&mut rec)?;
        return finish(rec, built, "started", None, Some(ms));
    }
    if services_changed {
        client::set_services(&rec)?;
        return finish(rec, built, "updated", Some("services changed"), None);
    }
    if http_changed {
        return finish(rec, built, "updated", Some("the HTTP port changed"), None);
    }
    finish(rec, built, "unchanged", None, None)
}

fn finish(
    rec: VmRecord,
    built: Built,
    action: &'static str,
    reason: Option<&'static str>,
    boot_ms: Option<u128>,
) -> Result<Up> {
    if rec.http.is_some() {
        // Also brings back a router that someone stopped.
        router::ensure();
    }
    let services = if rec.services.is_empty() {
        vec![]
    } else {
        // Freshly started services usually listen within moments; wait a
        // little so the ports we print are there.
        if action != "unchanged" {
            let t0 = Instant::now();
            while ports::read(&rec.name).is_empty() && t0.elapsed() < PORT_WAIT {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        client::get_services(&rec.name)?
    };
    Ok(Up {
        rec,
        built,
        action,
        reason,
        boot_ms,
        services,
    })
}

/// `runt down`: stop the project's VM, or remove it with `rm` (and its
/// volumes too with `rm_volumes`). Returns false when there was no VM.
pub fn down(r: &Recipe, rm: bool, rm_volumes: bool) -> Result<bool> {
    if !state::vm_dir(&r.name).exists() {
        if rm_volumes && volumes::owner(&r.name).as_deref() == Some(r.dir.as_path()) {
            volumes::remove(&r.name, None)?;
        }
        return Ok(false);
    }
    let mut rec = state::load(&r.name)?;
    if rec.project.as_deref() != Some(r.dir.as_path()) {
        return Err(CliError::new(
            "vm_exists",
            format!("the VM named {:?} isn't this project's", r.name),
        )
        .hint("leaving it alone; manage it with `runt stop` or `runt rm`"));
    }
    if rm {
        vm::remove(&r.name, true)?;
        if rm_volumes {
            volumes::remove(&r.name, None)?;
        }
    } else {
        vm::stop(&mut rec, false)?;
    }
    Ok(true)
}

/// One line about a service, for humans.
pub fn describe(s: &ServiceStatus) -> String {
    let state = match (s.pid, s.last_exit) {
        (Some(pid), _) => format!("running (pid {pid})"),
        (None, Some(code)) => format!("exited ({code})"),
        (None, None) => "starting".into(),
    };
    let restarts = match s.restarts {
        0 => String::new(),
        1 => ", restarted once".into(),
        n => format!(", restarted {n} times"),
    };
    format!("{}: {state}{restarts}", s.name)
}
