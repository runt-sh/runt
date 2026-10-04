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
        let name = format!("test-{tag}-{}", std::process::id());
        let o = runt(&["new", &name, "--json", "--mem", "512M"]);
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
