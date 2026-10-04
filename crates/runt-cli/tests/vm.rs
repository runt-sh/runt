//! End-to-end tests that boot real VMs. Ignored by default; run with
//! `make test-vm` (needs /dev/kvm, libkrun and `make assets`).

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn runt(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_runt")).args(args).output().expect("run runt")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

struct Vm(String);

impl Vm {
    fn new(tag: &str) -> Vm {
        let name = format!("test-{tag}-{}", std::process::id());
        let o = runt(&["new", &name, "--json", "--mem", "512M"]);
        assert!(o.status.success(), "runt new failed: {}", String::from_utf8_lossy(&o.stderr));
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
    child.stdin.take().unwrap().write_all(b"hello stdin\n").unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(stdout(&o), "hello stdin\n");

    let o = runt(&["exec", "--json", &vm.0, "--", "sh", "-c", "echo out; echo err >&2; exit 3"]);
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
    let me = list.as_array().unwrap().iter().find(|v| v["name"] == vm.0.as_str()).unwrap();
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
