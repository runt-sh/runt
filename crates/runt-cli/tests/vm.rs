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
