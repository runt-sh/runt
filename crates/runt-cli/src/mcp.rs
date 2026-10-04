//! `runt mcp`: a Model Context Protocol server on stdio.
//!
//! JSON-RPC 2.0, one message per line. A handful of tools with short
//! descriptions (clients pay for every schema in their context), compact
//! results, and long-running calls handled on their own threads so a slow
//! command doesn't block the rest.
//!
//! The operator decides what an agent may reach on this machine: shares are
//! limited to `--mount-root` directories (default: the directory the server
//! was started in), and LAN access needs `--allow-lan`.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::error::{CliError, Result};
use crate::state::{self, Egress, NetMode, Status};
use crate::{client, mounts, ops, vm};

/// Protocol revisions we speak, newest first.
const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const DEFAULT_TIMEOUT_S: u64 = 120;
const MAX_TIMEOUT_S: u64 = 3600;
/// Per output stream, per call.
const OUTPUT_LIMIT: usize = 64 * 1024;
const LOG_LIMIT: usize = 16 * 1024;

const INSTRUCTIONS: &str = "runt runs commands in fast, isolated Linux microVMs (Debian). \
Create a VM once with vm_create (about 0.2 s), then run commands in it with vm_exec. \
VMs persist until removed; ports a VM listens on are forwarded to 127.0.0.1 on this machine.";

pub struct Config {
    /// Directories shares may come from (canonical).
    pub mount_roots: Vec<PathBuf>,
    pub allow_lan: bool,
    /// Where relative mount sources resolve and exec's default workdir maps from.
    pub cwd: PathBuf,
}

impl Config {
    pub fn new(mount_roots: Vec<PathBuf>, allow_lan: bool) -> Result<Config> {
        let cwd = std::env::current_dir()?;
        let roots = if mount_roots.is_empty() {
            vec![cwd.clone()]
        } else {
            mount_roots
        };
        let mount_roots = roots
            .iter()
            .map(|r| {
                cwd.join(r).canonicalize().map_err(|e| {
                    CliError::new(
                        "invalid_mount_root",
                        format!("--mount-root {}: {e}", r.display()),
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Config {
            mount_roots,
            allow_lan,
            cwd,
        })
    }
}

/// Serve until stdin closes.
pub fn serve(cfg: Config) -> Result<()> {
    let cfg = Arc::new(cfg);
    let out = Arc::new(Mutex::new(io::stdout()));
    for line in io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                send(
                    &out,
                    &error_response(Value::Null, -32700, &format!("parse error: {e}")),
                );
                continue;
            }
        };
        let Some(method) = msg.get("method").and_then(Value::as_str).map(String::from) else {
            // A response to something we never send, or junk: ignore.
            continue;
        };
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            continue; // notifications: initialized, cancelled, ...
        };
        if method == "tools/call" {
            let (cfg, out) = (cfg.clone(), out.clone());
            std::thread::spawn(move || {
                let reply = call_tool(&cfg, &params);
                send(
                    &out,
                    &json!({ "jsonrpc": "2.0", "id": id, "result": reply }),
                );
            });
            continue;
        }
        let reply = match method.as_str() {
            "initialize" => Ok(initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        let resp = match reply {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, m)) => error_response(id, code, &m),
        };
        send(&out, &resp);
    }
    Ok(())
}

fn send(out: &Mutex<io::Stdout>, v: &Value) {
    let mut out = out.lock().unwrap_or_else(|e| e.into_inner());
    let _ = writeln!(out, "{v}").and_then(|_| out.flush());
}

fn error_response(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = requested
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "runt", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

fn tools() -> Value {
    let vm = json!({ "type": "string", "description": "VM name" });
    let strings =
        |d: &str| json!({ "type": "array", "items": { "type": "string" }, "description": d });
    json!([
        {
            "name": "vm_create",
            "description": "Create and boot a Linux VM. Returns its name.",
            "inputSchema": { "type": "object", "properties": {
                "name": { "type": "string", "description": "Default: random" },
                "cpus": { "type": "integer", "minimum": 1, "maximum": 64, "description": "Default 2" },
                "memory": { "type": "string", "description": "e.g. 512M, 2G. Default 1G" },
                "mounts": strings("Host dirs to share, SRC[:DST][:ro]; DST defaults to the same path"),
                "allow": strings("Only allow these egress destinations: example.com, *.example.com, IPv4[/len]. Default: whole internet"),
                "allow_lan": { "type": "boolean", "description": "Also allow private networks" },
                "offline": { "type": "boolean", "description": "No network at all" },
            }},
        },
        {
            "name": "vm_exec",
            "description": "Run a shell command in a VM (sh -c, as root). Returns exit code, stdout, stderr.",
            "inputSchema": { "type": "object", "required": ["vm", "command"], "properties": {
                "vm": vm,
                "command": { "type": "string" },
                "stdin": { "type": "string" },
                "workdir": { "type": "string", "description": "Default: same path as the server's cwd if shared, else /root" },
                "env": { "type": "object", "additionalProperties": { "type": "string" } },
                "timeout": { "type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_S, "description": "Seconds, default 120" },
            }},
        },
        {
            "name": "vm_list",
            "description": "List VMs with status, mounts, network policy and forwarded ports.",
            "inputSchema": { "type": "object", "properties": {} },
        },
        {
            "name": "vm_start",
            "description": "Boot a stopped VM.",
            "inputSchema": { "type": "object", "required": ["vm"], "properties": { "vm": vm } },
        },
        {
            "name": "vm_stop",
            "description": "Shut a VM down; its disk is kept.",
            "inputSchema": { "type": "object", "required": ["vm"], "properties": { "vm": vm } },
        },
        {
            "name": "vm_remove",
            "description": "Delete a VM and its disk (stops it first).",
            "inputSchema": { "type": "object", "required": ["vm"], "properties": { "vm": vm } },
        },
        {
            "name": "vm_logs",
            "description": "A VM's console log, or with egress=true the connections its network policy refused.",
            "inputSchema": { "type": "object", "required": ["vm"], "properties": {
                "vm": vm,
                "egress": { "type": "boolean" },
            }},
        },
    ])
}

/// A `tools/call` result: text for the model, structured content for
/// programs, `isError` for failures of the tool itself.
fn call_tool(cfg: &Config, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let empty = Map::new();
    let args = params
        .get("arguments")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let r = match name {
        "vm_create" => vm_create(cfg, args),
        "vm_exec" => vm_exec(cfg, args),
        "vm_list" => vm_list(),
        "vm_start" => vm_start(args),
        "vm_stop" => vm_stop(args),
        "vm_remove" => vm_remove(args),
        "vm_logs" => vm_logs(args),
        _ => Err(CliError::new(
            "unknown_tool",
            format!("unknown tool {name:?}"),
        )),
    };
    match r {
        Ok((text, structured)) => json!({
            "content": [{ "type": "text", "text": text }],
            "structuredContent": structured,
        }),
        Err(e) => {
            let mut text = format!("error ({}): {}", e.code, e.message);
            if let Some(h) = &e.hint {
                text.push_str(&format!("\nhint: {h}"));
            }
            json!({ "content": [{ "type": "text", "text": text }], "isError": true })
        }
    }
}

type ToolResult = Result<(String, Value)>;

fn bad_arg(msg: impl Into<String>) -> CliError {
    CliError::new("invalid_argument", msg)
}

fn str_arg<'a>(args: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(bad_arg(format!("{key} must be a string"))),
    }
}

fn required<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    str_arg(args, key)?.ok_or_else(|| bad_arg(format!("{key} is required")))
}

fn bool_arg(args: &Map<String, Value>, key: &str) -> Result<bool> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(bad_arg(format!("{key} must be a boolean"))),
    }
}

fn strings_arg(args: &Map<String, Value>, key: &str) -> Result<Vec<String>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(String::from)
                    .ok_or_else(|| bad_arg(format!("{key} must be a list of strings")))
            })
            .collect(),
        Some(_) => Err(bad_arg(format!("{key} must be a list of strings"))),
    }
}

fn int_arg(args: &Map<String, Value>, key: &str) -> Result<Option<u64>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| bad_arg(format!("{key} must be a positive integer"))),
    }
}

fn vm_create(cfg: &Config, args: &Map<String, Value>) -> ToolResult {
    let cpus = int_arg(args, "cpus")?.unwrap_or(2);
    if !(1..=64).contains(&cpus) {
        return Err(bad_arg("cpus must be between 1 and 64"));
    }
    let mem_mib = match str_arg(args, "memory")? {
        Some(m) => crate::parse_mem(m).map_err(bad_arg)?,
        None => 1024,
    };
    let mounts = strings_arg(args, "mounts")?
        .iter()
        .map(|m| share(cfg, m))
        .collect::<Result<Vec<_>>>()?;
    let allow_lan = bool_arg(args, "allow_lan")?;
    if allow_lan && !cfg.allow_lan {
        return Err(CliError::new(
            "not_permitted",
            "LAN access is disabled for this MCP server",
        )
        .hint("the user can enable it by starting `runt mcp --allow-lan`"));
    }
    let net = if bool_arg(args, "offline")? {
        NetMode::None
    } else {
        NetMode::Nat
    };
    let egress = Egress::new(&strings_arg(args, "allow")?, allow_lan)?;
    let (rec, boot_ms) = ops::new_vm(ops::NewSpec {
        name: str_arg(args, "name")?.map(String::from),
        cpus: cpus as u8,
        mem_mib,
        net,
        egress,
        mounts,
    })?;
    let mut text = format!("created VM {:?} (booted in {boot_ms} ms)", rec.name);
    for m in &rec.mounts {
        let ro = if m.read_only { ", read-only" } else { "" };
        text.push_str(&format!(
            "\n{} is shared at {}{ro}",
            m.src.display(),
            m.dst.display()
        ));
    }
    if let Some(e) = ops::egress_summary(&rec) {
        text.push_str(&format!("\nnetwork: {e}"));
    } else if rec.net == NetMode::None {
        text.push_str("\nnetwork: none");
    }
    Ok((text, ops::created_json(&rec, boot_ms)))
}

/// Parse a mount for an agent: the source must lie under a mount root.
fn share(cfg: &Config, spec: &str) -> Result<mounts::Mount> {
    let m = mounts::parse(spec, &cfg.cwd)?;
    if !cfg.mount_roots.iter().any(|r| m.src.starts_with(r)) {
        let roots: Vec<_> = cfg
            .mount_roots
            .iter()
            .map(|r| r.display().to_string())
            .collect();
        return Err(CliError::new(
            "not_permitted",
            format!(
                "{} is outside the directories this MCP server may share ({})",
                m.src.display(),
                roots.join(", ")
            ),
        )
        .hint("the user can allow more with `runt mcp --mount-root DIR`"));
    }
    Ok(m)
}

fn running(name: &str) -> Result<state::VmRecord> {
    let rec = state::load(name)?;
    if state::status(&rec) != Status::Running {
        return Err(
            CliError::new("vm_not_running", format!("VM {name:?} is not running"))
                .hint("start it with vm_start"),
        );
    }
    Ok(rec)
}

fn vm_exec(cfg: &Config, args: &Map<String, Value>) -> ToolResult {
    let name = required(args, "vm")?;
    let command = required(args, "command")?;
    let rec = running(name)?;
    let timeout = int_arg(args, "timeout")?.unwrap_or(DEFAULT_TIMEOUT_S);
    if !(1..=MAX_TIMEOUT_S).contains(&timeout) {
        return Err(bad_arg(format!(
            "timeout must be 1..={MAX_TIMEOUT_S} seconds"
        )));
    }
    let env = match args.get("env") {
        None | Some(Value::Null) => vec![],
        Some(Value::Object(o)) => o
            .iter()
            .map(|(k, v)| match v.as_str() {
                Some(v) if !k.is_empty() => Ok((k.clone(), v.to_string())),
                _ => Err(bad_arg("env must map names to strings")),
            })
            .collect::<Result<Vec<_>>>()?,
        Some(_) => return Err(bad_arg("env must be an object")),
    };
    let cwd = match str_arg(args, "workdir")? {
        Some(w) => Some(w.to_string()),
        None => {
            mounts::default_workdir(&cfg.cwd, &rec.mounts).map(|p| p.to_string_lossy().into_owned())
        }
    };
    let stdin = str_arg(args, "stdin")?.unwrap_or("").as_bytes().to_vec();
    let conn = client::connect(name, &state::socket_path(name))?;
    let r = client::run_captured(
        conn,
        client::ExecOpts {
            argv: vec!["/bin/sh".into(), "-c".into(), command.into()],
            env,
            cwd,
            tty: false,
        },
        stdin,
        Duration::from_secs(timeout),
        OUTPUT_LIMIT,
    )?;
    let (stdout, stderr) = (r.stdout.text(), r.stderr.text());
    let mut text = String::new();
    text.push_str(&stdout);
    if !stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("[stderr]\n");
        text.push_str(&stderr);
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    match r.code {
        Some(c) => text.push_str(&format!("[exit code {c}]")),
        None => text.push_str(&format!("[timed out after {timeout} s; command killed]")),
    }
    Ok((
        text,
        json!({
            "exit_code": r.code, "timed_out": r.code.is_none(),
            "stdout": stdout, "stderr": stderr,
            "stdout_omitted_bytes": r.stdout.omitted(),
            "stderr_omitted_bytes": r.stderr.omitted(),
        }),
    ))
}

fn vm_list() -> ToolResult {
    let vms = state::list()?;
    let items: Vec<Value> = vms.iter().map(ops::vm_json).collect();
    let text = if vms.is_empty() {
        "no VMs".to_string()
    } else {
        vms.iter()
            .zip(&items)
            .map(|(r, v)| {
                let mut line = format!("{} {}", r.name, state::status(r).as_str());
                for m in &r.mounts {
                    line.push_str(&format!(" mount:{}", m.dst.display()));
                }
                if r.net == NetMode::None {
                    line.push_str(" offline");
                } else if let Some(e) = ops::egress_summary(r) {
                    line.push_str(&format!(" net:{e}"));
                }
                for p in v["ports"].as_array().into_iter().flatten() {
                    line.push_str(&format!(" port:{}->127.0.0.1:{}", p["guest"], p["host"]));
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok((text, json!({ "vms": items })))
}

fn vm_start(args: &Map<String, Value>) -> ToolResult {
    let name = required(args, "vm")?;
    let mut rec = state::load(name)?;
    let ms = vm::start(&mut rec)?;
    Ok((
        format!("started {name:?} ({ms} ms)"),
        json!({ "name": name, "status": "running", "boot_ms": ms }),
    ))
}

fn vm_stop(args: &Map<String, Value>) -> ToolResult {
    let name = required(args, "vm")?;
    let mut rec = state::load(name)?;
    vm::stop(&mut rec, false)?;
    Ok((
        format!("stopped {name:?}"),
        json!({ "name": name, "status": "stopped" }),
    ))
}

fn vm_remove(args: &Map<String, Value>) -> ToolResult {
    let name = required(args, "vm")?;
    vm::remove(name, true)?;
    Ok((
        format!("removed {name:?}"),
        json!({ "name": name, "status": "removed" }),
    ))
}

fn vm_logs(args: &Map<String, Value>) -> ToolResult {
    let name = required(args, "vm")?;
    state::load(name)?;
    let (path, empty) = if bool_arg(args, "egress")? {
        (state::egress_log_path(name), "nothing refused")
    } else {
        (vm::console_log(name), "console log is empty")
    };
    let log = std::fs::read(&path).unwrap_or_default();
    // The end of a log is what matters.
    let start = log.len().saturating_sub(LOG_LIMIT);
    let text = String::from_utf8_lossy(&log[start..]).into_owned();
    let shown = if text.is_empty() {
        empty.to_string()
    } else {
        text.clone()
    };
    Ok((
        shown,
        json!({ "name": name, "log": text, "omitted_bytes": start }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn cfg(root: &Path) -> Config {
        Config {
            mount_roots: vec![root.to_path_buf()],
            allow_lan: false,
            cwd: root.to_path_buf(),
        }
    }

    #[test]
    fn negotiates_protocol_version() {
        let v = initialize(&json!({ "protocolVersion": "2025-06-18" }));
        assert_eq!(v["protocolVersion"], "2025-06-18");
        let v = initialize(&json!({ "protocolVersion": "1999-01-01" }));
        assert_eq!(v["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn tool_schemas_are_small() {
        let t = tools();
        assert_eq!(t.as_array().unwrap().len(), 7);
        // Every schema costs context in every client: keep the total small.
        assert!(t.to_string().len() < 4000, "{}", t.to_string().len());
    }

    #[test]
    fn shares_are_confined_to_mount_roots() {
        let base = std::env::temp_dir().join(format!("runt-mcp-test-{}", std::process::id()));
        let inside = base.join("proj");
        std::fs::create_dir_all(&inside).unwrap();
        let base = base.canonicalize().unwrap();
        let c = cfg(&base.join("proj"));
        assert!(share(&c, ".").is_ok());
        assert!(share(&c, &format!("{}:/w:ro", inside.display())).is_ok());
        let e = share(&c, "..").unwrap_err();
        assert_eq!(e.code, "not_permitted");
        assert!(share(&c, "/tmp").is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn lan_needs_operator_consent() {
        let c = cfg(Path::new("/"));
        let args = json!({ "allow_lan": true });
        let e = vm_create(&c, args.as_object().unwrap()).unwrap_err();
        assert_eq!(e.code, "not_permitted");
    }

    #[test]
    fn bad_arguments_are_tool_errors() {
        let c = cfg(Path::new("/"));
        let r = call_tool(&c, &json!({ "name": "vm_exec", "arguments": { "vm": 3 } }));
        assert_eq!(r["isError"], true);
        let r = call_tool(&c, &json!({ "name": "nope" }));
        assert_eq!(r["isError"], true);
    }
}
