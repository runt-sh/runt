//! `runt up` and `runt down`: run a project's VM from its runt.toml.
//!
//! The VM is named after the recipe and remembers which project it belongs
//! to. `runt up` builds, then brings the VM in line with the recipe doing as
//! little as it can: services are swapped in place, boot settings (CPUs,
//! memory, shares, network rules) take a restart, and only a new image
//! means a new VM with a fresh disk.

use std::time::{Duration, Instant};

use runt_proto::ServiceStatus;

use crate::build::{self, Built};
use crate::client;
use crate::error::{CliError, Result};
use crate::ops;
use crate::ports;
use crate::recipe::Recipe;
use crate::state::{self, NetMode, Status, VmRecord};
use crate::vm;

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
        ..Default::default()
    };
    let existing = state::vm_dir(&r.name)
        .exists()
        .then(|| state::load(&r.name))
        .transpose()?;
    let Some(mut rec) = existing else {
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
        let (rec, ms) = ops::new_vm(spec())?;
        return finish(rec, built, "recreated", Some("the image changed"), Some(ms));
    }
    let reboot = if (rec.cpus, rec.mem_mib) != (r.cpus, r.mem_mib) {
        Some("CPUs or memory changed")
    } else if rec.mounts != r.mounts {
        Some("shared folders changed")
    } else if rec.egress != r.egress || rec.net != NetMode::Nat {
        Some("network rules changed")
    } else {
        None
    };
    let services_changed = rec.env != r.run_env || rec.services != r.services;
    let s = spec();
    (rec.cpus, rec.mem_mib, rec.mounts, rec.egress, rec.net) =
        (s.cpus, s.mem_mib, s.mounts, s.egress, s.net);
    (rec.env, rec.services) = (s.env, s.services);
    state::save(&rec)?;
    let running = state::status(&rec) == Status::Running;
    if running && reboot.is_some() {
        vm::stop(&mut rec, false)?;
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
    finish(rec, built, "unchanged", None, None)
}

fn finish(
    rec: VmRecord,
    built: Built,
    action: &'static str,
    reason: Option<&'static str>,
    boot_ms: Option<u128>,
) -> Result<Up> {
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

/// `runt down`: stop the project's VM, or remove it with `rm`. Returns
/// false when there was no VM.
pub fn down(r: &Recipe, rm: bool) -> Result<bool> {
    if !state::vm_dir(&r.name).exists() {
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
