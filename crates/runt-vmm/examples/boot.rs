//! Spike harness: boot a kernel + initramfs (+ disks) in the foreground.
//!
//!   cargo run -p runt-vmm --example boot -- VMLINUX INITRAMFS CONSOLE_LOG [DISK[:ro]...]
use std::path::PathBuf;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 3 {
        eprintln!("usage: boot VMLINUX INITRAMFS CONSOLE_LOG [DISK[:ro]...]");
        std::process::exit(2);
    }
    let disks = a[3..]
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let (path, ro) = match d.strip_suffix(":ro") {
                Some(p) => (p, true),
                None => (d.as_str(), false),
            };
            runt_vmm::Disk {
                id: format!("disk{i}"),
                path: path.into(),
                read_only: ro,
            }
        })
        .collect();
    let cfg = runt_vmm::VmConfig {
        vcpus: 1,
        mem_mib: 512,
        kernel: PathBuf::from(&a[0]),
        initramfs: PathBuf::from(&a[1]),
        cmdline: std::env::var("CMDLINE").unwrap_or_else(|_| "console=hvc0 panic=-1".into()),
        disks,
        vsock_ports: vec![runt_vmm::VsockPort {
            port: 1024,
            socket: std::env::temp_dir().join("runt-boot.sock"),
            host_connects: true,
        }],
        console_log: PathBuf::from(&a[2]),
        net: None,
    };
    let err = runt_vmm::run(&cfg).unwrap_err();
    eprintln!("boot: {err}");
    std::process::exit(1);
}
