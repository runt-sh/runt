//! `runt.toml`: the recipe that defines a project's VM. `runt build` turns
//! its steps into image layers; `runt up` boots a VM from them and keeps its
//! services running.
//!
//! ```toml
//! name = "myapp"
//!
//! [vm]
//! cpus = 2
//! memory = "1G"
//!
//! [build]
//! steps = [
//!   { run = "apt-get update && apt-get install -y python3" },
//!   { copy = ".", to = "/app", exclude = [".git"] },
//! ]
//!
//! [env]
//! GREETING = "hi"
//!
//! [services.web]
//! cmd = "python3 -m http.server 8000"
//! cwd = "/app"
//!
//! [http]                    # served at http://myapp.runt.localhost
//! port = 8000
//!
//! [volumes]                 # kept when the VM is recreated
//! data = { path = "/data", size = "1G" }
//!
//! [network]
//! allow = ["pypi.org", "files.pythonhosted.org"]
//!
//! [dev]                     # what `runt up` changes on this machine
//! mounts = [".:/app"]
//! ```

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml::{Table, Value};

use crate::error::{CliError, Result};
use crate::mounts::{self, Mount};
use crate::state::{self, Egress};
use crate::volumes::{self, Volume};

pub const FILE: &str = "runt.toml";
/// JSON Schema for runt.toml, for editors and agents (`runt schema`).
pub const SCHEMA: &str = include_str!("../../../schema/runt.schema.json");
/// The only base image so far.
pub const BASE: &str = "runt/base";
/// Each step is an image layer.
pub const MAX_STEPS: usize = 16;

// The keys each table may have. The schema lists the same ones (a test
// checks), so add new keys to both.
const TOP_KEYS: &[&str] = &[
    "name", "vm", "build", "env", "services", "http", "volumes", "network", "dev",
];
const VM_KEYS: &[&str] = &["cpus", "memory"];
const BUILD_KEYS: &[&str] = &["base", "steps"];
const RUN_KEYS: &[&str] = &["run", "cwd"];
const COPY_KEYS: &[&str] = &["copy", "to", "exclude"];
const SERVICE_KEYS: &[&str] = &["cmd", "cwd", "env", "restart"];
const HTTP_KEYS: &[&str] = &["port"];
const VOLUME_KEYS: &[&str] = &["path", "size"];
const NETWORK_KEYS: &[&str] = &["allow", "allow_lan"];
const DEV_KEYS: &[&str] = &["mounts", "env", "services"];

/// A loaded, validated recipe, with `[dev]` applied (it is what `runt up`
/// runs on this machine).
#[derive(Debug, Clone)]
pub struct Recipe {
    /// Canonical directory holding runt.toml; relative paths start here.
    pub dir: PathBuf,
    pub name: String,
    pub cpus: u8,
    pub mem_mib: u32,
    pub steps: Vec<Step>,
    /// `[env]`: build steps, services and `runt exec` see it.
    pub env: BTreeMap<String, String>,
    /// `[env]` plus `[dev.env]`: what the running VM sees.
    pub run_env: BTreeMap<String, String>,
    /// `[services]` with `[dev.services]` applied.
    pub services: Vec<ServiceDef>,
    pub egress: Egress,
    /// `[dev] mounts`.
    pub mounts: Vec<Mount>,
    pub volumes: Vec<Volume>,
    /// `[http] port`.
    pub http: Option<u16>,
}

/// One build step. Its JSON form is part of the step's cache key.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Step {
    /// Copy files from the project (paths relative to its directory) into
    /// directory `to`, keeping their relative paths.
    Copy {
        paths: Vec<String>,
        to: String,
        exclude: Vec<String>,
    },
    /// Run a shell command as root in `cwd`.
    Run { cmd: String, cwd: String },
}

impl Step {
    /// One line for build output.
    pub fn describe(&self) -> String {
        match self {
            Step::Copy { paths, to, .. } => format!("copy {} -> {to}", paths.join(" ")),
            Step::Run { cmd, .. } => {
                let first = cmd.lines().next().unwrap_or("");
                let more = if cmd.trim_end().contains('\n') {
                    " ..."
                } else {
                    ""
                };
                format!("run {first}{more}")
            }
        }
    }
}

/// A service as stored in the VM's record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceDef {
    pub name: String,
    pub cmd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub restart: Restart,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    #[default]
    Always,
    OnFailure,
    Never,
}

impl Restart {
    pub fn proto(self) -> runt_proto::Restart {
        match self {
            Restart::Always => runt_proto::Restart::Always,
            Restart::OnFailure => runt_proto::Restart::OnFailure,
            Restart::Never => runt_proto::Restart::Never,
        }
    }
}

fn invalid(msg: impl Into<String>) -> CliError {
    CliError::new("invalid_recipe", format!("{FILE}: {}", msg.into()))
        .hint("see the runt.toml section of `runt skill`")
}

/// Find runt.toml in `start` or the nearest parent directory that has one.
pub fn find(start: &Path) -> Result<PathBuf> {
    start
        .ancestors()
        .map(|d| d.join(FILE))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            CliError::new(
                "no_recipe",
                format!("no {FILE} in {} or its parents", start.display()),
            )
            .hint("write one (see the runt.toml section of `runt skill`)")
        })
}

pub fn load(path: &Path) -> Result<Recipe> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CliError::new("no_recipe", format!("{}: {e}", path.display())))?;
    let dir = path
        .parent()
        .unwrap_or(Path::new("."))
        .canonicalize()
        .map_err(|e| CliError::new("no_recipe", format!("{}: {e}", path.display())))?;
    parse(&text, &dir)
}

/// One TOML table being read: typed getters that name the offending key in
/// their errors, and a check for keys we don't know (usually typos).
struct Fields<'a> {
    table: &'a Table,
    /// Where it is, for errors: "" (top level), "[vm]", "build step 2".
    at: String,
}

impl<'a> Fields<'a> {
    fn new(table: &'a Table, at: impl Into<String>, known: &[&str]) -> Result<Fields<'a>> {
        let f = Fields {
            table,
            at: at.into(),
        };
        if let Some(k) = table.keys().find(|k| !known.contains(&k.as_str())) {
            return Err(f.err(k, "is not a known key"));
        }
        Ok(f)
    }

    fn err(&self, key: &str, what: &str) -> CliError {
        if self.at.is_empty() {
            invalid(format!("`{key}` {what}"))
        } else {
            invalid(format!("{} `{key}` {what}", self.at))
        }
    }

    fn str(&self, key: &str) -> Result<Option<String>> {
        match self.table.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(self.err(key, "must be a string")),
        }
    }

    fn strings(&self, key: &str) -> Result<Vec<String>> {
        match self.table.get(key) {
            None => Ok(vec![]),
            Some(Value::Array(a)) => a
                .iter()
                .map(|v| match v {
                    Value::String(s) => Ok(s.clone()),
                    _ => Err(self.err(key, "must be a list of strings")),
                })
                .collect(),
            Some(_) => Err(self.err(key, "must be a list of strings")),
        }
    }

    fn table(&self, key: &str) -> Result<Option<&'a Table>> {
        match self.table.get(key) {
            None => Ok(None),
            Some(Value::Table(t)) => Ok(Some(t)),
            Some(_) => Err(self.err(key, "must be a table")),
        }
    }

    /// A table whose keys are names, each holding a table.
    fn tables(&self, key: &str) -> Result<Vec<(&'a str, &'a Table)>> {
        let Some(t) = self.table(key)? else {
            return Ok(vec![]);
        };
        t.iter()
            .map(|(name, v)| match v {
                Value::Table(t) => Ok((name.as_str(), t)),
                _ => Err(self.err(&format!("{key}.{name}"), "must be a table")),
            })
            .collect()
    }

    /// Environment variables; integers and booleans become strings.
    fn env(&self, key: &str) -> Result<BTreeMap<String, String>> {
        let Some(t) = self.table(key)? else {
            return Ok(BTreeMap::new());
        };
        let at = format!(
            "{}{key}",
            if self.at.is_empty() {
                String::new()
            } else {
                format!("{} ", self.at)
            }
        );
        t.iter()
            .map(|(k, v)| {
                let valid = k
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                    && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                if !valid {
                    return Err(invalid(format!("{at}: {k:?} isn't a valid variable name")));
                }
                let v = match v {
                    Value::String(s) => s.clone(),
                    Value::Integer(i) => i.to_string(),
                    Value::Boolean(b) => b.to_string(),
                    _ => {
                        return Err(invalid(format!(
                            "{at}: {k} must be a string (secrets aren't supported yet)"
                        )));
                    }
                };
                Ok((k.clone(), v))
            })
            .collect()
    }
}

pub fn parse(text: &str, dir: &Path) -> Result<Recipe> {
    let doc: Table = text
        .parse()
        .map_err(|e: toml::de::Error| invalid(e.to_string().trim_end()))?;
    if doc.contains_key("deploy") {
        return Err(invalid("[deploy] isn't supported yet"));
    }
    let top = Fields::new(&doc, "", TOP_KEYS)?;
    let name = top
        .str("name")?
        .ok_or_else(|| invalid("`name` is missing (it names the project and its VM)"))?;
    state::validate_name(&name).map_err(|_| {
        invalid(format!(
            "invalid name {name:?}: use 1-48 lowercase letters, digits and dashes"
        ))
    })?;

    let empty = Table::new();
    let vm = Fields::new(top.table("vm")?.unwrap_or(&empty), "[vm]", VM_KEYS)?;
    let cpus = match vm.table.get("cpus") {
        None => 2,
        Some(Value::Integer(n)) if (1..=64).contains(n) => *n as u8,
        Some(_) => return Err(vm.err("cpus", "must be a number from 1 to 64")),
    };
    let mem_mib = match vm.str("memory")? {
        Some(m) => {
            crate::parse_mem(&m).map_err(|e| vm.err("memory", &format!("is invalid: {e}")))?
        }
        None => 1024,
    };

    let build = Fields::new(top.table("build")?.unwrap_or(&empty), "[build]", BUILD_KEYS)?;
    match build.str("base")?.as_deref() {
        None | Some(BASE) => {}
        Some(other) => {
            return Err(invalid(format!(
                "base {other:?} isn't available; the only base so far is {BASE:?}"
            )));
        }
    }
    let raw_steps = match build.table.get("steps") {
        None => &vec![],
        Some(Value::Array(a)) => a,
        Some(_) => return Err(build.err("steps", "must be a list of steps")),
    };
    if raw_steps.len() > MAX_STEPS {
        return Err(invalid(format!(
            "{} build steps; at most {MAX_STEPS} are allowed (combine run steps with &&)",
            raw_steps.len()
        )));
    }
    let steps = raw_steps
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let at = format!("build step {}", i + 1);
            match v {
                Value::Table(t) => {
                    let keys: Vec<&str> = RUN_KEYS.iter().chain(COPY_KEYS).copied().collect();
                    step(&Fields::new(t, at, &keys)?)
                }
                _ => Err(invalid(format!(
                    "{at} must be a table like {{ run = \"...\" }}"
                ))),
            }
        })
        .collect::<Result<Vec<_>>>()?;

    let dev = Fields::new(top.table("dev")?.unwrap_or(&empty), "[dev]", DEV_KEYS)?;
    let env = top.env("env")?;
    let mut run_env = env.clone();
    run_env.extend(dev.env("env")?);

    let dev_services = dev.tables("services")?;
    let mut services = Vec::new();
    for (name, t) in top.tables("services")? {
        let patch = dev_services
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, t)| *t);
        services.push(service(name, Some(t), patch)?);
    }
    for (name, t) in &dev_services {
        if !services.iter().any(|d| d.name == *name) {
            services.push(service(name, None, Some(t))?);
        }
    }
    services.sort_unstable_by(|a, b| a.name.cmp(&b.name));

    let network = Fields::new(
        top.table("network")?.unwrap_or(&empty),
        "[network]",
        NETWORK_KEYS,
    )?;
    let allow_lan = match network.table.get("allow_lan") {
        None => false,
        Some(Value::Boolean(b)) => *b,
        Some(_) => return Err(network.err("allow_lan", "must be true or false")),
    };
    let egress = Egress::new(&network.strings("allow")?, allow_lan)
        .map_err(|e| invalid(format!("[network]: {}", e.message)))?;
    let mounts = dev
        .strings("mounts")?
        .iter()
        .map(|m| mounts::parse(m, dir).map_err(|e| invalid(format!("[dev] mounts: {}", e.message))))
        .collect::<Result<Vec<_>>>()?;
    mounts::validate_set(&mounts)?;

    let http = Fields::new(top.table("http")?.unwrap_or(&empty), "[http]", HTTP_KEYS)?;
    let http = match http.table.get("port") {
        None => None,
        Some(Value::Integer(n)) if (1..=65535).contains(n) => Some(*n as u16),
        Some(_) => return Err(http.err("port", "must be a port number")),
    };
    let volumes = top
        .table("volumes")?
        .map(|t| {
            t.iter()
                .map(|(name, v)| volume(name, v))
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    if volumes.len() > volumes::MAX_VOLUMES {
        return Err(invalid(format!(
            "at most {} volumes are supported",
            volumes::MAX_VOLUMES
        )));
    }
    let mut dirs: Vec<(String, &Path)> = volumes
        .iter()
        .map(|v| (format!("volume {:?}", v.name), Path::new(&v.path)))
        .collect();
    dirs.extend(
        mounts
            .iter()
            .map(|m| (format!("mount {}", m.dst.display()), m.dst.as_path())),
    );
    for (i, (a, pa)) in dirs.iter().enumerate() {
        for (b, pb) in &dirs[i + 1..] {
            if pa.starts_with(pb) || pb.starts_with(pa) {
                return Err(invalid(format!("{a} and {b} overlap inside the VM")));
            }
        }
    }

    Ok(Recipe {
        dir: dir.to_path_buf(),
        name,
        cpus,
        mem_mib,
        steps,
        env,
        run_env,
        services,
        egress,
        mounts,
        volumes,
        http,
    })
}

/// `NAME = { path = "/data", size = "1G" }`, or just `NAME = "/data"`.
fn volume(name: &str, v: &Value) -> Result<Volume> {
    let at = format!("[volumes] {name}");
    if !valid_name(name) {
        return Err(invalid(format!(
            "volume name {name:?}: use 1-32 lowercase letters, digits, - and _"
        )));
    }
    let (path, size) = match v {
        Value::String(p) => (p.clone(), None),
        Value::Table(t) => {
            let f = Fields::new(t, at.clone(), VOLUME_KEYS)?;
            let path = f
                .str("path")?
                .ok_or_else(|| invalid(format!("{at} needs `path`, where it is mounted")))?;
            (path, f.str("size")?)
        }
        _ => {
            return Err(invalid(format!(
                "{at} must be like {{ path = \"/data\", size = \"1G\" }}"
            )));
        }
    };
    let path = absolute_path(&path, "path").map_err(|e| invalid(format!("{at}: {e}")))?;
    volumes::validate_path(&path).map_err(|e| invalid(format!("{at}: {e}")))?;
    let size_mib = match size {
        Some(s) => volumes::parse_size(&s).map_err(|e| invalid(format!("{at} size: {e}")))?,
        None => volumes::DEFAULT_SIZE_MIB,
    };
    Ok(Volume {
        name: name.into(),
        path,
        size_mib,
    })
}

/// Service and volume names.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn step(f: &Fields) -> Result<Step> {
    let bad = |what: &str| invalid(format!("{}: {what}", f.at));
    let paths = match f.table.get("copy") {
        None => None,
        Some(Value::String(s)) => Some(s.split_whitespace().map(String::from).collect()),
        Some(Value::Array(_)) => Some(f.strings("copy")?),
        Some(_) => return Err(f.err("copy", "must be a path or a list of paths")),
    };
    match (paths, f.str("run")?) {
        (Some(paths), None) => {
            if f.table.contains_key("cwd") {
                return Err(bad("`cwd` goes with `run`; `copy` takes `to`"));
            }
            let paths: Vec<String> = paths;
            if paths.is_empty() {
                return Err(bad("`copy` needs at least one path"));
            }
            for p in &paths {
                relative_path(p).map_err(|e| bad(&e))?;
            }
            let to = f
                .str("to")?
                .ok_or_else(|| bad("`copy` needs `to`, the directory to copy into"))?;
            Ok(Step::Copy {
                paths,
                to: absolute_path(&to, "to").map_err(|e| bad(&e))?,
                exclude: f.strings("exclude")?,
            })
        }
        (None, Some(cmd)) => {
            if f.table.contains_key("to") || f.table.contains_key("exclude") {
                return Err(bad("`to` and `exclude` go with `copy`"));
            }
            if cmd.trim().is_empty() {
                return Err(bad("`run` is empty"));
            }
            let cwd = match f.str("cwd")? {
                Some(c) => absolute_path(&c, "cwd").map_err(|e| bad(&e))?,
                None => "/".into(),
            };
            Ok(Step::Run { cmd, cwd })
        }
        (Some(_), Some(_)) => Err(bad("a step is either `copy` or `run`, not both")),
        (None, None) => Err(bad("a step needs `copy` or `run`")),
    }
}

/// A project path: relative, inside the project.
fn relative_path(p: &str) -> std::result::Result<(), String> {
    let path = Path::new(p);
    let ok = !p.is_empty()
        && path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "copy path {p:?} must be relative to the project directory and stay inside it"
        ))
    }
}

/// An absolute guest path, normalized (no `.`, `..` or repeated slashes).
fn absolute_path(p: &str, what: &str) -> std::result::Result<String, String> {
    let path = Path::new(p);
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return Err(format!("`{what}` must be an absolute path without `..`"));
    }
    let parts: Vec<_> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    Ok(format!("/{}", parts.join("/")))
}

/// A service from `[services.NAME]`, with `[dev.services.NAME]` on top
/// (either may be missing, not both).
fn service(name: &str, base: Option<&Table>, dev: Option<&Table>) -> Result<ServiceDef> {
    if !valid_name(name) {
        return Err(invalid(format!(
            "service name {name:?}: use 1-32 lowercase letters, digits, - and _"
        )));
    }
    let base = base
        .map(|t| Fields::new(t, format!("[services.{name}]"), SERVICE_KEYS))
        .transpose()?;
    let dev = dev
        .map(|t| Fields::new(t, format!("[dev.services.{name}]"), SERVICE_KEYS))
        .transpose()?;
    // The dev table wins, key by key.
    let layers: Vec<&Fields> = base.iter().chain(dev.iter()).collect();
    let mut def = ServiceDef {
        name: name.into(),
        cmd: String::new(),
        cwd: None,
        env: BTreeMap::new(),
        restart: Restart::default(),
    };
    for f in &layers {
        if let Some(cmd) = f.str("cmd")? {
            def.cmd = cmd;
        }
        if let Some(cwd) = f.str("cwd")? {
            def.cwd = Some(absolute_path(&cwd, "cwd").map_err(|e| f.err("cwd", &e))?);
        }
        def.env.extend(f.env("env")?);
        def.restart = match f.str("restart")?.as_deref() {
            None => def.restart,
            Some("always") => Restart::Always,
            Some("on-failure") => Restart::OnFailure,
            Some("never") => Restart::Never,
            Some(_) => {
                return Err(f.err("restart", "must be \"always\", \"on-failure\" or \"never\""));
            }
        };
    }
    if def.cmd.trim().is_empty() {
        return Err(invalid(format!("[services.{name}] needs `cmd`")));
    }
    Ok(def)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(text: &str) -> Recipe {
        parse(text, Path::new("/tmp")).unwrap()
    }

    fn parse_err(text: &str) -> String {
        parse(text, Path::new("/tmp")).unwrap_err().message
    }

    #[test]
    fn parses_a_full_recipe() {
        let r = parse_ok(
            r#"
            name = "myapp"
            [vm]
            cpus = 4
            memory = "2G"
            [build]
            base = "runt/base"
            steps = [
              { copy = "package.json src", to = "/app/" },
              { copy = ["."], to = "/srv", exclude = ["node_modules"] },
              { run = "npm ci", cwd = "/app" },
              { run = "echo hi" },
            ]
            [env]
            NODE_ENV = "production"
            PORT = 3000
            [services.web]
            cmd = "node server.js"
            cwd = "/app"
            env = { DEBUG = false }
            [services.worker]
            cmd = "node worker.js"
            restart = "on-failure"
            [network]
            allow = ["registry.npmjs.org"]
            [dev]
            env = { NODE_ENV = "development" }
            [dev.services.web]
            cmd = "npm run dev"
            "#,
        );
        assert_eq!((r.name.as_str(), r.cpus, r.mem_mib), ("myapp", 4, 2048));
        assert_eq!(
            r.steps[0],
            Step::Copy {
                paths: vec!["package.json".into(), "src".into()],
                to: "/app".into(),
                exclude: vec![],
            }
        );
        assert_eq!(
            r.steps[2],
            Step::Run {
                cmd: "npm ci".into(),
                cwd: "/app".into()
            }
        );
        assert_eq!(
            r.steps[3],
            Step::Run {
                cmd: "echo hi".into(),
                cwd: "/".into()
            }
        );
        assert_eq!(r.env["PORT"], "3000");
        assert_eq!(r.env["NODE_ENV"], "production");
        assert_eq!(r.run_env["NODE_ENV"], "development");
        let web = &r.services[0];
        assert_eq!(web.name, "web");
        assert_eq!(web.cmd, "npm run dev");
        assert_eq!(web.cwd.as_deref(), Some("/app"));
        assert_eq!(web.env["DEBUG"], "false");
        assert_eq!(r.services[1].restart, Restart::OnFailure);
        assert_eq!(r.egress.allow, vec!["registry.npmjs.org"]);
    }

    #[test]
    fn defaults() {
        let r = parse_ok("name = \"x\"");
        assert_eq!((r.cpus, r.mem_mib), (2, 1024));
        assert!(r.steps.is_empty() && r.services.is_empty() && r.mounts.is_empty());
        assert!(r.egress.is_default());
    }

    #[test]
    fn rejects_mistakes() {
        for (text, needle) in [
            ("", "`name` is missing"),
            ("name = \"Bad Name\"", "invalid name"),
            ("name = \"x\"\nnmae = 1", "`nmae` is not a known key"),
            (
                "name = \"x\"\n[vm]\ncpu = 1",
                "[vm] `cpu` is not a known key",
            ),
            (
                "name = \"x\"\n[vm]\ncpus = \"2\"",
                "[vm] `cpus` must be a number",
            ),
            (
                "name = \"x\"\n[build]\nsteps = [{ run = \"x\", cdw = \"/\" }]",
                "build step 1 `cdw`",
            ),
            ("name = \"x\"\n[deploy]\n", "[deploy] isn't supported yet"),
            ("name = \"x\"\n[http]\nport = 0", "[http] `port`"),
            ("name = \"x\"\n[http]\nprot = 1", "[http] `prot`"),
            ("name = \"x\"\n[volumes]\nd = \"rel\"", "absolute"),
            ("name = \"x\"\n[volumes]\nd = \"/etc\"", "system"),
            ("name = \"x\"\n[volumes]\nd = \"/proc/x\"", "system"),
            ("name = \"x\"\n[volumes]\nD = \"/d\"", "volume name"),
            (
                "name = \"x\"\n[volumes]\nd = { size = \"1G\" }",
                "needs `path`",
            ),
            (
                "name = \"x\"\n[volumes]\nd = { path = \"/d\", size = \"1\" }",
                "unit",
            ),
            (
                "name = \"x\"\n[volumes]\na = \"/d\"\nb = \"/d/e\"",
                "overlap",
            ),
            (
                "name = \"x\"\n[volumes]\na = \"/app/data\"\n[dev]\nmounts = [\".:/app\"]",
                "overlap",
            ),
            ("name = \"x\"\n[build]\nbase = \"debian\"", "only base"),
            (
                "name = \"x\"\n[build]\nsteps = [{ copy = \"a\" }]",
                "needs `to`",
            ),
            (
                "name = \"x\"\n[build]\nsteps = [{ copy = \"../a\", to = \"/a\" }]",
                "stay inside",
            ),
            (
                "name = \"x\"\n[build]\nsteps = [{ copy = \"/etc\", to = \"/a\" }]",
                "relative",
            ),
            (
                "name = \"x\"\n[build]\nsteps = [{ copy = \"a\", to = \"rel\" }]",
                "absolute",
            ),
            (
                "name = \"x\"\n[build]\nsteps = [{ run = \"a\", copy = \"b\", to = \"/\" }]",
                "not both",
            ),
            (
                "name = \"x\"\n[build]\nsteps = [{ cwd = \"/\" }]",
                "`copy` or `run`",
            ),
            ("name = \"x\"\n[env]\nA = { secret = \"A\" }", "secrets"),
            ("name = \"x\"\n[env]\n\"1A\" = \"x\"", "valid variable name"),
            ("name = \"x\"\n[services.web]\ncwd = \"/\"", "needs `cmd`"),
            ("name = \"x\"\n[services.Web]\ncmd = \"x\"", "service name"),
            (
                "name = \"x\"\n[services.w]\ncmd = \"x\"\nrestart = \"sometimes\"",
                "`restart` must be",
            ),
            ("name = \"x\"\n[vm]\nmemory = \"lots\"", "memory"),
            (
                "name = \"x\"\n[network]\nallow = [\"127.0.0.1\"]",
                "[network]",
            ),
            (
                "name = \"x\"\n[dev]\nmounts = [\"/nonexistent-dir\"]",
                "mounts",
            ),
        ] {
            let err = parse_err(text);
            assert!(err.contains(needle), "{text:?}: {err:?} lacks {needle:?}");
        }
        let many = format!(
            "name = \"x\"\n[build]\nsteps = [{}]",
            vec!["{ run = \"true\" }"; MAX_STEPS + 1].join(",")
        );
        assert!(parse_err(&many).contains("at most"));
    }

    #[test]
    fn volumes_and_http() {
        let r = parse_ok(
            "name = \"x\"\n[http]\nport = 3000\n[volumes]\ndb = { path = \"/var/lib//db/\", size = \"10G\" }\ncache = \"/cache\"",
        );
        assert_eq!(r.http, Some(3000));
        assert_eq!(
            r.volumes,
            vec![
                Volume {
                    name: "cache".into(),
                    path: "/cache".into(),
                    size_mib: 1024
                },
                Volume {
                    name: "db".into(),
                    path: "/var/lib/db".into(),
                    size_mib: 10240
                },
            ]
        );
    }

    #[test]
    fn dev_only_services_and_mounts() {
        let r = parse_ok(
            "name = \"x\"\n[dev]\nmounts = [\".:/app:ro\"]\n[dev.services.watch]\ncmd = \"w\"",
        );
        assert_eq!(r.services.len(), 1);
        assert_eq!(r.mounts[0].src, Path::new("/tmp").canonicalize().unwrap());
        assert_eq!(r.mounts[0].dst, Path::new("/app"));
        assert!(r.mounts[0].read_only);
    }

    #[test]
    fn schema_matches_the_parser() {
        let s: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
        let keys = |v: &serde_json::Value| {
            let mut k: Vec<String> = v["properties"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            k.sort();
            k
        };
        let want = |list: &[&str]| {
            let mut k: Vec<String> = list.iter().map(|s| s.to_string()).collect();
            k.sort();
            k
        };
        let (top, defs) = (&s, &s["definitions"]);
        let p = &top["properties"];
        assert_eq!(keys(top), want(TOP_KEYS));
        assert_eq!(keys(&p["vm"]), want(VM_KEYS));
        assert_eq!(keys(&p["build"]), want(BUILD_KEYS));
        assert_eq!(keys(&defs["runStep"]), want(RUN_KEYS));
        assert_eq!(keys(&defs["copyStep"]), want(COPY_KEYS));
        assert_eq!(keys(&defs["serviceFields"]), want(SERVICE_KEYS));
        assert_eq!(keys(&p["http"]), want(HTTP_KEYS));
        let volume = &p["volumes"]["additionalProperties"]["oneOf"][1];
        assert_eq!(keys(volume), want(VOLUME_KEYS));
        assert_eq!(keys(&p["network"]), want(NETWORK_KEYS));
        assert_eq!(keys(&p["dev"]), want(DEV_KEYS));

        // Limits and choices.
        assert_eq!(p["build"]["properties"]["steps"]["maxItems"], MAX_STEPS);
        assert_eq!(
            p["build"]["properties"]["base"]["enum"],
            serde_json::json!([BASE])
        );
        assert_eq!(p["volumes"]["maxProperties"], volumes::MAX_VOLUMES);
        let restart = &defs["serviceFields"]["properties"]["restart"]["enum"];
        for r in restart.as_array().unwrap() {
            let r: Restart = serde_json::from_value(r.clone()).unwrap();
            assert_ne!(serde_json::to_value(r).unwrap(), serde_json::Value::Null);
        }
        assert_eq!(restart.as_array().unwrap().len(), 3);
        assert_eq!(p["vm"]["properties"]["cpus"]["maximum"], 64);
    }

    #[test]
    fn describes_steps() {
        let s = Step::Run {
            cmd: "apt-get update\napt-get install -y x\n".into(),
            cwd: "/".into(),
        };
        assert_eq!(s.describe(), "run apt-get update ...");
        let s = Step::Copy {
            paths: vec![".".into()],
            to: "/app".into(),
            exclude: vec![],
        };
        assert_eq!(s.describe(), "copy . -> /app");
    }
}
