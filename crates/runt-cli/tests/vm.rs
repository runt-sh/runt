//! End-to-end tests that boot real VMs. Ignored by default; run with
//! `make test-vm` (needs /dev/kvm, libkrun and `make assets`).

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn runt(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_runt"))
        .args(args)
        .output()
        .expect("run runt")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

struct Vm(String);

impl Vm {
    fn new(tag: &str) -> Vm {
        Vm::with(tag, &[])
    }

    fn with(tag: &str, extra: &[&str]) -> Vm {
        let name = format!("test-{tag}-{}", std::process::id());
        let mut args = vec!["new", &name, "--json", "--mem", "512M"];
        args.extend_from_slice(extra);
        let o = runt(&args);
        assert!(
            o.status.success(),
            "runt new failed: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
        assert_eq!(v["status"], "running");
        assert!(v["boot_ms"].as_u64().unwrap() > 0);
        Vm(name)
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        runt(&["rm", "-f", &self.0]);
    }
}

#[test]
#[ignore]
fn exec_semantics() {
    let vm = Vm::new("exec");
    let o = runt(&["exec", &vm.0, "--", "uname", "-r"]);
    assert!(stdout(&o).starts_with("6.18"), "kernel: {}", stdout(&o));

    let o = runt(&["exec", &vm.0, "--", "sh", "-c", "exit 7"]);
    assert_eq!(o.status.code(), Some(7));

    let o = runt(&["exec", &vm.0, "--", "no-such-command"]);
    assert_eq!(o.status.code(), Some(127));

    let o = runt(&["exec", &vm.0, "--", "cat", "/etc/os-release"]);
    assert!(stdout(&o).contains("trixie"));

    let mut child = Command::new(env!("CARGO_BIN_EXE_runt"))
        .args(["exec", &vm.0, "--", "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"hello stdin\n")
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(stdout(&o), "hello stdin\n");

    let o = runt(&[
        "exec",
        "--json",
        &vm.0,
        "--",
        "sh",
        "-c",
        "echo out; echo err >&2; exit 3",
    ]);
    assert_eq!(o.status.code(), Some(3));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["exit_code"], 3);
    assert_eq!(v["stdout"], "out\n");
    assert_eq!(v["stderr"], "err\n");

    // A background job that inherited stdout must not hold the session open.
    let t = std::time::Instant::now();
    let o = runt(&["exec", &vm.0, "--", "sh", "-c", "sleep 30 &"]);
    assert!(o.status.success());
    assert!(t.elapsed().as_secs() < 5);
}

#[test]
#[ignore]
fn stop_start_persists_disk() {
    let vm = Vm::new("persist");
    let o = runt(&["exec", &vm.0, "--", "sh", "-c", "echo kept > /root/note"]);
    assert!(o.status.success());

    assert!(runt(&["stop", &vm.0]).status.success());
    let o = runt(&["ls", "--json"]);
    let list: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let me = list
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == vm.0.as_str())
        .unwrap();
    assert_eq!(me["status"], "stopped");

    let o = runt(&["exec", &vm.0, "--", "true"]);
    assert_eq!(o.status.code(), Some(125));

    assert!(runt(&["start", &vm.0]).status.success());
    let o = runt(&["exec", &vm.0, "--", "cat", "/root/note"]);
    assert_eq!(stdout(&o), "kept\n");
}

#[test]
#[ignore]
fn rm_refuses_running_vm_without_force() {
    let vm = Vm::new("rm");
    let o = runt(&["rm", &vm.0]);
    assert_eq!(o.status.code(), Some(125));
    let o = runt(&["rm", "-f", "--json", &vm.0]);
    assert!(o.status.success());
    assert!(runt(&["exec", &vm.0, "--", "true"]).status.code() == Some(125));
}

/// Run a command in the VM; return (exit code, stdout).
fn vm_exec(vm: &Vm, cmd: &str) -> (Option<i32>, String) {
    let o = runt(&["exec", &vm.0, "--", "sh", "-c", cmd]);
    (o.status.code(), stdout(&o))
}

#[test]
#[ignore]
fn outbound_network() {
    let vm = Vm::new("net");
    let (code, _) = vm_exec(&vm, "getent hosts deb.debian.org");
    assert_eq!(code, Some(0), "DNS lookup failed");
    let (code, out) = vm_exec(
        &vm,
        "curl -fsS -o /dev/null -w '%{http_code}' https://deb.debian.org/",
    );
    assert_eq!((code, out.as_str()), (Some(0), "200"));
    // The gateway must not relay to services on the host's loopback.
    let host = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = host.local_addr().unwrap().port();
    let (code, _) = vm_exec(&vm, &format!("curl -s -m 3 http://100.96.0.1:{port}/"));
    assert_ne!(
        code,
        Some(0),
        "guest reached host loopback through the gateway"
    );
    // Cloud metadata is never reachable.
    let (code, _) = vm_exec(&vm, "curl -s -m 3 http://169.254.169.254/");
    assert_ne!(code, Some(0));
}

#[test]
#[ignore]
fn offline_vm_has_no_network() {
    let name = format!("test-offline-{}", std::process::id());
    let o = runt(&["new", &name, "--net", "none", "--mem", "512M"]);
    assert!(o.status.success());
    let vm = Vm(name);
    let (code, _) = vm_exec(&vm, "getent hosts deb.debian.org");
    assert_ne!(code, Some(0));
    let (code, out) = vm_exec(&vm, "ip -br link show eth0 2>&1 || true");
    assert_eq!(code, Some(0));
    assert!(out.contains("does not exist") || out.is_empty(), "{out}");
}

#[test]
#[ignore]
fn guest_ports_are_forwarded() {
    use std::io::Read;
    use std::time::{Duration, Instant};

    let vm = Vm::new("ports");
    // A server bound to the guest's loopback only, like most dev servers.
    let server = r#"perl -MIO::Socket::INET -e '$s=IO::Socket::INET->new(LocalAddr=>"127.0.0.1",LocalPort=>8123,Listen=>5,ReuseAddr=>1) or die; while($c=$s->accept){print $c "hello from the guest\n"; close $c}' >/dev/null 2>&1 &"#;
    assert_eq!(vm_exec(&vm, server).0, Some(0));

    let host_port = wait_for(Duration::from_secs(5), || {
        let o = runt(&["port", "--json", &vm.0]);
        let v: serde_json::Value = serde_json::from_slice(&o.stdout).ok()?;
        v.as_array()?
            .iter()
            .find(|m| m["guest"] == 8123)
            .and_then(|m| m["host"].as_u64())
    })
    .expect("port 8123 was not forwarded");

    let mut s = std::net::TcpStream::connect(("127.0.0.1", host_port as u16)).unwrap();
    let mut got = String::new();
    s.read_to_string(&mut got).unwrap();
    assert_eq!(got, "hello from the guest\n");

    // Stopping the server removes the forward.
    vm_exec(&vm, "pkill -f IO::Socket::INET");
    let gone = wait_for(Duration::from_secs(5), || {
        let o = runt(&["port", "--json", &vm.0]);
        (stdout(&o).trim() == "[]").then_some(())
    });
    assert!(gone.is_some(), "forward was not removed");

    fn wait_for<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
        let t = Instant::now();
        while t.elapsed() < timeout {
            if let Some(v) = f() {
                return Some(v);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        None
    }
}

#[test]
#[ignore]
fn shared_folders() {
    use std::os::unix::fs::MetadataExt;

    let base = std::env::temp_dir().join(format!("runt-test-share-{}", std::process::id()));
    let rw = base.join("project");
    let ro = base.join("readonly");
    std::fs::create_dir_all(rw.join("sub")).unwrap();
    std::fs::create_dir_all(&ro).unwrap();
    std::fs::write(rw.join("hello.txt"), "from host\n").unwrap();
    std::fs::write(ro.join("data.txt"), "read only\n").unwrap();
    let rw = rw.canonicalize().unwrap();
    let rw_s = rw.to_str().unwrap().to_string();

    let name = format!("test-share-{}", std::process::id());
    let o = Command::new(env!("CARGO_BIN_EXE_runt"))
        .args(["new", &name, "--mem", "512M", "--mount", "."])
        .arg("--mount")
        .arg(format!("{}:/data:ro", ro.display()))
        .current_dir(&rw)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let vm = Vm(name);

    // Host -> guest, at the same path.
    let (code, out) = vm_exec(&vm, &format!("cat {rw_s}/hello.txt"));
    assert_eq!((code, out.as_str()), (Some(0), "from host\n"));

    // Guest -> host, owned by the host user.
    vm_exec(&vm, &format!("echo from guest > {rw_s}/guest.txt"));
    let written = rw.join("guest.txt");
    assert_eq!(std::fs::read_to_string(&written).unwrap(), "from guest\n");
    // SAFETY: getuid never fails.
    let me = unsafe { libc::getuid() };
    assert_eq!(std::fs::metadata(&written).unwrap().uid(), me);

    // Read-only share: even after a guest remount, writes are refused.
    let (code, _) = vm_exec(&vm, "mount -o remount,rw /data; echo x > /data/new.txt");
    assert_ne!(code, Some(0));
    assert!(!ro.join("new.txt").exists());

    // `runt exec` starts where you are, when that's inside a mount.
    let pwd = |dir: &std::path::Path| {
        let o = Command::new(env!("CARGO_BIN_EXE_runt"))
            .args(["exec", &vm.0, "--", "pwd"])
            .current_dir(dir)
            .output()
            .unwrap();
        stdout(&o)
    };
    assert_eq!(pwd(&rw.join("sub")), format!("{rw_s}/sub\n"));
    assert_eq!(pwd(std::path::Path::new("/")), "/root\n");

    // `..` out of a mount stays in the guest: the host's sibling dir isn't there.
    let (_, out) = vm_exec(&vm, &format!("ls -a {rw_s}/.."));
    assert!(
        !out.contains("readonly"),
        "host parent leaked into guest: {out}"
    );

    // Mounts survive stop/start.
    assert!(runt(&["stop", &vm.0]).status.success());
    assert!(runt(&["start", &vm.0]).status.success());
    let (_, out) = vm_exec(&vm, &format!("cat {rw_s}/guest.txt /data/data.txt"));
    assert_eq!(out, "from guest\nread only\n");

    drop(vm);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
#[ignore]
fn vm_process_is_sandboxed() {
    let base = std::env::temp_dir().join(format!("runt-test-sandbox-{}", std::process::id()));
    let share = base.join("share");
    std::fs::create_dir_all(&share).unwrap();
    let outside = base.join("outside.txt");
    std::fs::write(&outside, "secret").unwrap();

    let a = format!("test-sba-{}", std::process::id());
    let o = Command::new(env!("CARGO_BIN_EXE_runt"))
        .args(["new", &a, "--json", "--mem", "512M", "--mount"])
        .arg(&share)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let new: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let a = Vm(a);
    let b = Vm::new("sbb");
    if new["sandbox"]["landlock"] != "full" {
        eprintln!("skipping: kernel lacks full Landlock ({})", new["sandbox"]);
        return;
    }
    assert_eq!(new["sandbox"]["seccomp"], true);

    let rt = |vm: &Vm| {
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap();
        std::path::PathBuf::from(dir)
            .join("runt")
            .join(&vm.0)
            .join("agent.sock")
    };
    let o = Command::new(env!("CARGO_BIN_EXE_runt"))
        .args(["__sandbox-check", &a.0])
        .arg("--read")
        .arg(&outside)
        .arg("--write")
        .arg(share.join("ok.txt"))
        .arg("--write")
        .arg(base.join("nope.txt"))
        .arg("--connect")
        .arg(rt(&a))
        .arg("--connect")
        .arg(rt(&b))
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let r = &v["results"];
    let denied = |k: String| {
        let s = r[&k]
            .as_str()
            .unwrap_or_else(|| panic!("no result for {k}: {v}"));
        assert!(s.starts_with("denied"), "{k} should be denied: {s}");
    };
    let allowed = |k: String| assert_eq!(r[&k], "allowed", "{k} should be allowed");
    denied(format!("read {}", outside.display()));
    allowed(format!("write {}", share.join("ok.txt").display()));
    denied(format!("write {}", base.join("nope.txt").display()));
    allowed(format!("connect {}", rt(&a).display()));
    denied(format!("connect {}", rt(&b).display()));
    denied("exec /bin/true".into());
    denied("unshare user namespace".into());

    drop((a, b));
    let _ = std::fs::remove_dir_all(&base);
}

/// This machine's address on its LAN, if it has a private one.
fn host_lan_ip() -> Option<std::net::Ipv4Addr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("1.1.1.1:53").ok()?;
    match s.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if ip.is_private() => Some(ip),
        _ => None,
    }
}

fn http_code(vm: &Vm, url: &str) -> String {
    vm_exec(
        vm,
        &format!("curl -s -o /dev/null -m 5 -w '%{{http_code}}' {url}"),
    )
    .1
}

#[test]
#[ignore]
fn egress_allowlist() {
    let vm = Vm::with(
        "allow",
        &["--allow", "deb.debian.org", "--allow", "1.1.1.1"],
    );
    assert_eq!(http_code(&vm, "https://deb.debian.org/"), "200");
    // Not listed: DNS refuses, and direct connections are dropped.
    let (code, _) = vm_exec(&vm, "getent hosts example.com");
    assert_ne!(code, Some(0), "unlisted domain resolved");
    assert_eq!(http_code(&vm, "http://8.8.8.8/"), "000");
    // Exact names don't cover subdomains.
    let (code, _) = vm_exec(&vm, "getent hosts security.debian.org");
    assert_ne!(code, Some(0));
    // A listed address works without DNS.
    assert_ne!(http_code(&vm, "http://1.1.1.1/"), "000");
    // Refusals are logged for `runt logs --egress`.
    let o = runt(&["logs", &vm.0, "--egress", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let denied = v["denied"].as_array().unwrap();
    assert!(
        denied
            .iter()
            .any(|d| d["op"] == "resolve" && d["dest"] == "example.com")
    );
    assert!(
        denied
            .iter()
            .any(|d| d["op"] == "connect" && d["dest"] == "8.8.8.8:80")
    );
}

#[test]
#[ignore]
fn egress_lan_never_reaches_host_loopback() {
    let lan_server = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let lan_port = lan_server.local_addr().unwrap().port();
    let lo_server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let lo_port = lo_server.local_addr().unwrap().port();
    let accept = |l: std::net::TcpListener| {
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                use std::io::Write;
                let _ = (&c).write_all(b"HTTP/1.0 204 No Content\r\n\r\n");
            }
        })
    };
    accept(lan_server);
    accept(lo_server);

    let lan = Vm::with("lan", &["--allow-lan"]);
    let default = Vm::new("nolan");
    if let Some(ip) = host_lan_ip() {
        let url = format!("http://{ip}:{lan_port}/");
        assert_eq!(
            http_code(&lan, &url),
            "204",
            "--allow-lan VM can't reach the LAN"
        );
        assert_eq!(
            http_code(&default, &url),
            "000",
            "default VM reached the LAN"
        );
    }
    for vm in [&lan, &default] {
        assert_eq!(http_code(vm, "https://deb.debian.org/"), "200");
        let gw = format!("http://100.96.0.1:{lo_port}/");
        assert_eq!(http_code(vm, &gw), "000", "{} reached host loopback", vm.0);
        assert_eq!(http_code(vm, "http://169.254.169.254/"), "000");
    }
}

#[test]
fn egress_rejects_bad_rules() {
    for args in [
        &["--allow", "github.com", "--allow-lan"][..],
        &["--allow", "127.0.0.1"],
        &["--allow", "not a domain"],
        &["--allow", "fd00::/8"],
        &["--net", "none", "--allow-lan"],
    ] {
        let mut a = vec!["new", "never-created", "--json"];
        a.extend_from_slice(args);
        let o = runt(&a);
        assert_eq!(o.status.code(), Some(125), "{args:?} should be refused");
        let err: serde_json::Value = serde_json::from_slice(&o.stderr).unwrap();
        assert_eq!(err["error"]["code"], "invalid_egress", "{args:?}");
    }
}

#[test]
#[ignore]
fn mcp_server() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;

    let dir = std::env::temp_dir().join(format!("runt-test-mcp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hello.txt"), "from the host\n").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_runt"))
        .arg("mcp")
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut id = 0;
    let mut rpc = |method: &str, params: serde_json::Value| {
        id += 1;
        let req =
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(stdin, "{req}").unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], id);
        v["result"].clone()
    };
    let call = |rpc: &mut dyn FnMut(&str, serde_json::Value) -> serde_json::Value,
                tool: &str,
                args: serde_json::Value| {
        rpc(
            "tools/call",
            serde_json::json!({ "name": tool, "arguments": args }),
        )
    };

    let init = rpc(
        "initialize",
        serde_json::json!({ "protocolVersion": "2025-06-18" }),
    );
    assert_eq!(init["protocolVersion"], "2025-06-18");
    let name = format!("test-mcp-{}", std::process::id());
    let r = call(
        &mut rpc,
        "vm_create",
        serde_json::json!({ "name": name, "mounts": ["."] }),
    );
    assert_ne!(r["isError"], true, "{r}");
    let vm = Vm(name.clone());

    // Runs in the shared directory by default, with stdin.
    let r = call(
        &mut rpc,
        "vm_exec",
        serde_json::json!({ "vm": name, "command": "cat hello.txt; cat; exit 4", "stdin": "piped\n" }),
    );
    assert_eq!(r["structuredContent"]["exit_code"], 4);
    assert_eq!(r["structuredContent"]["stdout"], "from the host\npiped\n");

    let r = call(
        &mut rpc,
        "vm_exec",
        serde_json::json!({ "vm": name, "command": "sleep 60", "timeout": 1 }),
    );
    assert_eq!(r["structuredContent"]["timed_out"], true);

    // VMs a person created are off limits unless granted.
    let theirs = Vm::new("mcp-theirs");
    let r = call(
        &mut rpc,
        "vm_exec",
        serde_json::json!({ "vm": theirs.0, "command": "true" }),
    );
    assert_eq!(r["isError"], true, "{r}");
    let r = call(&mut rpc, "vm_remove", serde_json::json!({ "vm": theirs.0 }));
    assert_eq!(r["isError"], true, "{r}");
    let r = call(&mut rpc, "vm_list", serde_json::json!({}));
    let names: Vec<_> = r["structuredContent"]["vms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.contains(&name) && !names.contains(&theirs.0),
        "{names:?}"
    );

    // Shares outside the server's directory are refused.
    let r = call(
        &mut rpc,
        "vm_create",
        serde_json::json!({ "name": "never-created", "mounts": ["/tmp"] }),
    );
    assert_eq!(r["isError"], true);

    let r = call(&mut rpc, "vm_remove", serde_json::json!({ "vm": name }));
    assert_ne!(r["isError"], true);
    std::mem::forget(vm);
    drop(stdin);
    assert!(child.wait().unwrap().success());
    std::fs::remove_dir_all(&dir).unwrap();
}

fn runt_in(dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_runt"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run runt")
}

fn json(o: &Output) -> serde_json::Value {
    assert!(
        o.status.success(),
        "runt failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    serde_json::from_slice(&o.stdout).unwrap()
}

/// Removes a project's VM (and its directory) however the test ends.
struct Project(std::path::PathBuf, String);

impl Drop for Project {
    fn drop(&mut self) {
        runt(&["rm", "-f", &self.1]);
        runt(&["volume", "rm", &self.1]);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore]
fn recipes_build_and_run() {
    let name = format!("test-recipe-{}", std::process::id());
    let dir = std::env::temp_dir().join(&name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("skip")).unwrap();
    let p = Project(dir.clone(), name.clone());
    std::fs::write(dir.join("hello.txt"), "hello\n").unwrap();
    std::fs::write(dir.join("skip/secret.txt"), "nope").unwrap();
    // Layers are shared by every project with the same steps, so the first
    // step names this test run to start from an empty cache. Layer 1 also
    // deletes a base file and makes a tree; layer 3 replaces part
    // of it, so whiteouts and opaque directories have to survive stacking,
    // both fresh and (on the rebuild below) from cached layer files.
    std::fs::write(
        dir.join("runt.toml"),
        format!(
            r#"
name = "{name}"
[vm]
memory = "512M"
[build]
steps = [
  {{ run = "echo {name} > /name && rm /usr/bin/curl && mkdir -p /data/sub && echo base > /data/sub/x && echo gone > /data/gone" }},
  {{ copy = ".", to = "/app", exclude = ["skip"] }},
  {{ run = "test ! -e /usr/bin/curl && rm /data/gone && rm -r /data/sub && mkdir /data/sub && echo new > /data/sub/y && cat hello.txt > /built && echo $FROM_ENV > /env-at-build", cwd = "/app" }},
]
[env]
FROM_ENV = "recipe-env"
[services.counter]
cmd = "while true; do echo tick $FROM_ENV $OWN; sleep 0.2; done"
env = {{ OWN = "svc" }}
[services.oneshot]
cmd = "echo ran"
restart = "never"
"#
        ),
    )
    .unwrap();

    let v = json(&runt_in(&dir, &["up", "--json"]));
    assert_eq!(v["action"], "created");
    assert_eq!(v["build"]["cached"], 0);
    assert_eq!(v["build"]["layers"].as_array().unwrap().len(), 3);

    let check = |script: &str| {
        let o = runt(&["exec", &name, "--", "sh", "-c", script]);
        assert!(
            o.status.success(),
            "{script}: {}{}",
            stdout(&o),
            String::from_utf8_lossy(&o.stderr)
        );
        stdout(&o)
    };
    check("test ! -e /usr/bin/curl && test ! -e /data/gone && test ! -e /data/sub/x");
    assert_eq!(
        check("cat /data/sub/y /built /env-at-build"),
        "new\nhello\nrecipe-env\n"
    );
    assert_eq!(check("ls /app"), "hello.txt\nrunt.toml\n");
    assert_eq!(check("stat -c %U /app/hello.txt"), "root\n");
    assert_eq!(check("printenv FROM_ENV"), "recipe-env\n");

    // Services run with the recipe's environment and their own; a
    // restart = "never" service runs once.
    std::thread::sleep(std::time::Duration::from_millis(600));
    let o = runt(&["logs", &name, "-s", "counter"]);
    assert!(stdout(&o).contains("tick recipe-env svc"), "{}", stdout(&o));
    assert_eq!(stdout(&runt(&["logs", &name, "-s", "oneshot"])), "ran\n");
    let list = json(&runt(&["ls", "--json"]));
    let me = list
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == name.as_str())
        .unwrap();
    let svc = |n: &str| {
        me["services"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == n)
            .unwrap()
            .clone()
    };
    assert_eq!(svc("counter")["running"], true);
    assert_eq!(svc("oneshot")["running"], false);
    assert_eq!(svc("oneshot")["last_exit"], 0);
    assert_eq!(svc("oneshot")["restarts"], 0);

    // Nothing changed: nothing happens.
    let v = json(&runt_in(&dir, &["up", "--json"]));
    assert_eq!(v["action"], "unchanged");
    assert_eq!(v["build"]["cached"], 3);

    // A copied file changed: that layer and the ones above it rebuild on
    // the cached first layer, and the VM is recreated from the new image.
    std::fs::write(dir.join("hello.txt"), "hello again\n").unwrap();
    let v = json(&runt_in(&dir, &["up", "--json"]));
    assert_eq!(v["action"], "recreated");
    assert_eq!(v["build"]["cached"], 1);
    assert_eq!(check("cat /built"), "hello again\n");
    check("test ! -e /usr/bin/curl && test ! -e /data/sub/x");

    let v = json(&runt_in(&dir, &["down", "--rm", "--json"]));
    assert_eq!(v["status"], "removed");
    assert!(!stdout(&runt(&["ls"])).contains(&name));
    drop(p);
}

#[test]
#[ignore]
fn failed_builds_clean_up_and_resume() {
    let name = format!("test-failbuild-{}", std::process::id());
    let dir = std::env::temp_dir().join(&name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let _p = Project(dir.clone(), name.clone());
    let recipe = |second: &str| {
        let text = format!(
            "name = \"{name}\"\n[build]\nsteps = [{{ run = \"echo {name} > /one\" }}, {{ run = \"{second}\" }}]\n"
        );
        std::fs::write(dir.join("runt.toml"), text).unwrap();
    };
    recipe("echo failing; exit 3");
    let o = runt_in(&dir, &["build", "--json"]);
    assert_eq!(o.status.code(), Some(125));
    let e: serde_json::Value = serde_json::from_slice(&o.stderr).unwrap();
    assert_eq!(e["error"]["code"], "build_failed");
    assert!(e["error"]["message"].as_str().unwrap().contains("step 2"));
    let log = e["error"]["hint"].as_str().unwrap();
    let log = std::fs::read_to_string(log.trim_start_matches("full output: ")).unwrap();
    assert!(log.contains("failing"), "{log}");
    assert!(!stdout(&runt(&["ls"])).contains("runt-build-"));

    // The first step's layer was kept.
    recipe("true");
    let v = json(&runt_in(&dir, &["build", "--json"]));
    assert_eq!(v["cached"], 1);
}

/// A GET through the local URL router: (status code, body).
fn get_via_router(url: &str) -> (u16, String) {
    use std::io::Read;
    let rest = url.strip_prefix("http://").unwrap();
    let (host, port) = rest.split_once(':').unwrap_or((rest, "80"));
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
    write!(
        s,
        "GET / HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let code = resp.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = resp
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (code, body)
}

#[test]
#[ignore]
fn volumes_and_local_urls() {
    use std::time::{Duration, Instant};

    let name = format!("test-vol-{}", std::process::id());
    let dir = std::env::temp_dir().join(&name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = Project(dir.clone(), name.clone());
    // Counts its visits in a file on the volume.
    std::fs::write(
        dir.join("serve.pl"),
        r#"use IO::Socket::INET;
my $s = IO::Socket::INET->new(LocalAddr => "127.0.0.1", LocalPort => 8000, Listen => 5, ReuseAddr => 1) or die;
while (my $c = $s->accept) {
  my $req = ""; while (my $l = <$c>) { $req .= $l; last if $l eq "\r\n"; }
  my ($host) = $req =~ /^Host: (\S+)/mi;
  open(my $f, "+>>", "/data/visits"); print $f "x"; seek($f, 0, 0); my $n = length(<$f>); close $f;
  my $body = "visit $n via $host\n";
  print $c "HTTP/1.1 200 OK\r\nContent-Length: " . length($body) . "\r\nConnection: close\r\n\r\n$body";
  close $c;
}
"#,
    )
    .unwrap();
    let recipe = |size: &str| {
        let text = format!(
            r#"
name = "{name}"
[vm]
memory = "512M"
[build]
steps = [{{ run = "echo {name} > /name" }}, {{ copy = "serve.pl", to = "/app" }}]
[services.web]
cmd = "perl /app/serve.pl"
[http]
port = 8000
[volumes]
data = {{ path = "/data", size = "{size}" }}
"#
        );
        std::fs::write(dir.join("runt.toml"), text).unwrap();
    };
    recipe("64M");
    let v = json(&runt_in(&dir, &["up", "--json"]));
    assert_eq!(v["action"], "created");
    assert_eq!(v["volumes"][0]["path"], "/data");
    let url = v["url"].as_str().expect("a local URL").to_string();
    assert!(
        url.starts_with(&format!("http://{name}.runt.localhost")),
        "{url}"
    );
    let visit = |n: u32| {
        let want = format!("visit {n} via {}", url.trim_start_matches("http://"));
        let t = Instant::now();
        loop {
            let (code, body) = get_via_router(&url);
            if code == 200 || t.elapsed() > Duration::from_secs(5) {
                assert_eq!((code, body.trim()), (200, want.as_str()));
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    visit(1);
    visit(2);

    // A new image means a new VM; the volume's data carries over.
    std::fs::write(
        dir.join("serve.pl"),
        std::fs::read_to_string(dir.join("serve.pl")).unwrap() + "# v2\n",
    )
    .unwrap();
    let v = json(&runt_in(&dir, &["up", "--json"]));
    assert_eq!(v["action"], "recreated");
    visit(3);

    // Growing restarts the VM with the bigger volume; shrinking is refused
    // without touching it.
    recipe("128M");
    let v = json(&runt_in(&dir, &["up", "--json"]));
    assert_eq!(
        (v["action"].as_str(), v["reason"].as_str()),
        (Some("restarted"), Some("volumes changed"))
    );
    let o = runt(&["exec", &name, "--", "df", "-m", "--output=size", "/data"]);
    let mb: u32 = stdout(&o).lines().nth(1).unwrap().trim().parse().unwrap();
    assert!(mb > 100, "{mb}M");
    visit(4);
    recipe("64M");
    let o = runt_in(&dir, &["up", "--json"]);
    let e: serde_json::Value = serde_json::from_slice(&o.stderr).unwrap();
    assert_eq!(e["error"]["code"], "volume_shrink");
    recipe("128M");
    visit(5);

    // The router explains what it can't route.
    let other = url.replace(&name, "no-such-vm");
    let (code, body) = get_via_router(&other);
    assert_eq!(code, 404);
    assert!(body.contains("no VM named"), "{body}");

    // Volumes outlive `down --rm` unless asked.
    json(&runt_in(&dir, &["down", "--rm", "--json"]));
    let vols = json(&runt(&["volume", "ls", "--json"]));
    assert!(
        vols.as_array()
            .unwrap()
            .iter()
            .any(|v| v["vm"] == name.as_str())
    );
    json(&runt_in(&dir, &["up", "--json"]));
    visit(6);
    json(&runt_in(&dir, &["down", "--rm", "--volumes", "--json"]));
    let vols = json(&runt(&["volume", "ls", "--json"]));
    assert!(
        !vols
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["vm"] == name.as_str())
    );
    drop(p);
}
