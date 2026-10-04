# runt

A tiny manager for tiny virtual machines, local and in the cloud.

`runt` gives you (or your coding agent) a real Linux VM that boots in about a
tenth of a second, with a CLI built for scripts and agents: `--json`
everywhere, the guest's exit codes passed through, and no prompts.

```console
$ runt new
nimble-shrew
booted in 121 ms; try `runt shell nimble-shrew`
$ runt exec nimble-shrew -- uname -r
6.18.55
$ runt exec --json nimble-shrew -- sh -c 'echo hi; exit 3'
{"exit_code":3,"stderr":"","stdout":"hi\n"}
```

> **Status: early development.** Linux (x86_64, KVM) only for now. macOS and
> Windows support, shared folders and `runt deploy` to [runt.sh](https://runt.sh)
> are on the way.

## Commands

| Command | What it does |
| --- | --- |
| `runt new [NAME] [--cpus N] [--mem 1G]` | Create and boot a VM |
| `runt exec VM [-t] [-e K=V] [-w DIR] -- CMD...` | Run a command; exits with its exit code |
| `runt shell VM` | Interactive shell |
| `runt ls` | List VMs |
| `runt stop VM` / `runt start VM` | Shut down / boot again (the disk is kept) |
| `runt rm [-f] VM` | Delete a VM and its disk |
| `runt port VM` | Ports forwarded from the VM to this machine |
| `runt logs VM` | Guest console log |

Add `--json` to any command for machine-readable output. runt's own errors
exit with 125 and, in JSON mode, print `{"error": {"code", "message", "hint"}}`
to stderr.

## Networking

VMs get outbound internet access through a userspace network stack, so no
root, TAP devices or bridges are involved. DNS uses your machine's own
resolver, so VPN and internal names work.

Any TCP port a program listens on inside the VM is forwarded to the same
port on your machine's `127.0.0.1` (or a free port if that one is taken).
This works even for servers that bind only to the VM's localhost. Run
`runt port VM` to see the mappings.

For safety, a VM can reach **only the public internet**. It can't reach
services on your machine (including ones bound to `127.0.0.1`), your LAN,
or cloud metadata endpoints. Use `runt new --net none` for a fully offline
VM.

## Building from source

You need Linux with KVM, Rust (via rustup), and a few packages. On Fedora:

```sh
sudo dnf install libkrun erofs-utils podman e2fsprogs gcc make flex bison bc elfutils-libelf-devel openssl-devel
rustup target add x86_64-unknown-linux-musl
```

Then:

```sh
make assets   # guest kernel, initramfs (runt-agent) and Debian base image -> ~/.cache/runt
make build    # target/release/runt
make test     # unit tests
make test-vm  # boots real VMs
```

Your user needs read/write access to `/dev/kvm` (Fedora and Arch grant it by
default; on Debian/Ubuntu add yourself to the `kvm` group).

## How it works

- **Each VM is a [libkrun](https://github.com/containers/libkrun) microVM**
  running in its own detached `runt` process. There is no daemon.
- **Networking is a userspace TCP/IP stack**
  ([smolvm-network](https://crates.io/crates/smolvm-network), wrapped by
  `crates/runt-net`) inside each VM's process.
- **The guest runs runt's own minimal kernel** (`images/kernel/`). Its root
  filesystem is a read-only, compressed erofs base image with a per-VM ext4
  disk layered over it.
- **runt-agent is the guest's PID 1** (`crates/runt-agent`). It serves exec
  sessions to the host over vsock, using a small flow-controlled protocol
  (`crates/runt-proto`).

## License

Apache-2.0
