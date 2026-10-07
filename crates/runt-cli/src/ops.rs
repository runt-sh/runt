//! Operations shared by the CLI commands and the MCP server.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::error::Result;
use crate::mounts::Mount;
use crate::recipe::ServiceDef;
use crate::state::{self, Egress, NetMode, Status, VmRecord};
use crate::{names, ports, router, vm};

#[derive(Default)]
pub struct NewSpec {
    pub name: Option<String>,
    pub cpus: u8,
    pub mem_mib: u32,
    pub net: NetMode,
    pub egress: Egress,
    pub mounts: Vec<Mount>,
    pub created_by: Option<String>,
    pub project: Option<PathBuf>,
    pub layers: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub services: Vec<ServiceDef>,
    pub volumes: Vec<crate::volumes::Volume>,
    pub http: Option<u16>,
}

/// Create and boot a VM; returns its record and boot time in ms. A VM that
/// fails to boot is removed again.
pub fn new_vm(spec: NewSpec) -> Result<(VmRecord, u128)> {
    let name = spec.name.unwrap_or_else(unused_random_name);
    let mut rec = vm::create(
        &name,
        spec.cpus,
        spec.mem_mib,
        spec.net,
        spec.egress,
        spec.mounts,
    )?;
    rec.created_by = spec.created_by;
    rec.project = spec.project;
    rec.layers = spec.layers;
    rec.env = spec.env;
    rec.services = spec.services;
    rec.volumes = spec.volumes;
    rec.http = spec.http;
    state::save(&rec)?;
    match vm::start(&mut rec) {
        Ok(ms) => Ok((rec, ms)),
        Err(e) => {
            let _ = vm::remove(&name, true);
            Err(e)
        }
    }
}

/// A command's environment in a VM: the VM's own (from its recipe), then
/// what the caller asked for.
pub fn exec_env(rec: &VmRecord, extra: Vec<(String, String)>) -> Vec<(String, String)> {
    rec.env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .chain(extra)
        .collect()
}

fn unused_random_name() -> String {
    loop {
        let n = names::random();
        if !state::vm_dir(&n).exists() {
            return n;
        }
    }
}

/// `runt new --json`.
pub fn created_json(rec: &VmRecord, boot_ms: u128) -> Value {
    json!({
        "name": rec.name, "status": "running", "cpus": rec.cpus,
        "mem_mib": rec.mem_mib, "boot_ms": boot_ms, "mounts": rec.mounts,
        "egress": egress_json(rec),
        "sandbox": vm::SandboxStatus::read(&rec.name),
    })
}

/// One `runt ls --json` entry.
pub fn vm_json(r: &VmRecord) -> Value {
    // Runtime files only describe a VM that is running.
    let status = state::status(r);
    let running = status == Status::Running;
    let ports = if running {
        ports::read(&r.name)
    } else {
        vec![]
    };
    let services = (running && !r.services.is_empty())
        .then(|| crate::client::get_services(&r.name).ok())
        .flatten();
    json!({
        "name": r.name, "status": status, "cpus": r.cpus,
        "mem_mib": r.mem_mib, "created": r.created, "created_by": r.created_by,
        "project": r.project, "net": r.net,
        "mounts": r.mounts, "volumes": r.volumes, "egress": egress_json(r),
        "http_port": r.http, "url": running.then(|| router::url(r)).flatten(),
        "services": services.map(|s| services_json(&s))
            .unwrap_or_else(|| json!(r.services.iter().map(|s| json!({"name": s.name})).collect::<Vec<_>>())),
        "sandbox": running.then(|| vm::SandboxStatus::read(&r.name)).flatten(),
        "ports": ports.iter()
            .map(|m| json!({ "guest": m.guest, "host": m.host }))
            .collect::<Vec<_>>(),
    })
}

pub fn services_json(list: &[runt_proto::ServiceStatus]) -> Value {
    json!(
        list.iter()
            .map(|s| json!({
                "name": s.name, "running": s.pid.is_some(), "pid": s.pid,
                "restarts": s.restarts, "last_exit": s.last_exit,
            }))
            .collect::<Vec<_>>()
    )
}

/// `null` for offline VMs; otherwise what the network may reach.
pub fn egress_json(rec: &VmRecord) -> Value {
    if rec.net == NetMode::None {
        return Value::Null;
    }
    json!({
        "internet": if rec.egress.allow.is_empty() { "all" } else { "allowlist" },
        "allow": rec.egress.allow,
        "lan": rec.egress.lan,
    })
}

/// One line for humans, when the policy isn't the default.
pub fn egress_summary(rec: &VmRecord) -> Option<String> {
    let e = &rec.egress;
    if rec.net == NetMode::None || e.is_default() {
        return None;
    }
    let mut parts = Vec::new();
    if !e.allow.is_empty() {
        parts.push(format!("only {}", e.allow.join(", ")));
    } else {
        parts.push("internet".into());
    }
    if e.lan {
        parts.push("LAN".into());
    }
    Some(parts.join(" + "))
}
