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
> Windows support and `runt deploy` to [runt.sh](https://runt.sh) are on the way.

## Commands

| Command | What it does |
| --- | --- |
| `runt new [NAME] [--cpus N] [--mem 1G] [--mount SRC[:DST][:ro]]` | Create and boot a VM |
| `runt exec VM [-t] [-e K=V] [-w DIR] -- CMD...` | Run a command; exits with its exit code |
| `runt shell VM` | Interactive shell |
| `runt ls` | List VMs |
| `runt stop VM` / `runt start VM` | Shut down / boot again (the disk is kept) |
| `runt rm [-f] VM` | Delete a VM and its disk |
| `runt port VM` | Ports forwarded from the VM to this machine |
| `runt logs VM [--egress]` | Guest console log, or refused network connections |
| `runt mcp` | MCP server for AI agents (see below) |
| `runt skill [--install]` | Print or install the agent skill (see below) |

Add `--json` to any command for machine-readable output. runt's own errors
exit with 125 and, in JSON mode, print `{"error": {"code", "message", "hint"}}`
to stderr.

## AI agents

Agents that can run shell commands can use the CLI directly. `runt skill`
prints a short guide for them. `runt skill --install` installs it as a Claude
Code skill in `~/.claude/skills/runt/`; for other agents, paste it into their
instructions.

For agents that use tools, `runt mcp` is a [Model Context Protocol](https://modelcontextprotocol.io)
server on stdio with seven small tools: `vm_create`, `vm_exec`, `vm_list`,
`vm_start`, `vm_stop`, `vm_remove` and `vm_logs`.

```sh
# Claude Code, from your project directory
claude mcp add runt -- runt mcp
```

Other clients take the usual config: `{"command": "runt", "args": ["mcp"]}`.

You stay in charge of what an agent can reach on your machine:

- **Shares:** agents may share only the directory `runt mcp` was started in
  (or those given with `--mount-root DIR`).
- **LAN:** agents can't open LAN access unless you start the server with
  `--allow-lan`.
- **VMs:** agents see and manage only the VMs they created. Use `--vm NAME`
  to also hand them an existing VM, or `--all-vms` to give them all of yours.
- **Output:** `vm_exec` keeps the first and last 32 KiB of each output
  stream and has a timeout (default 120 s).
- **Workdir:** commands start in the same directory as the server when that
  directory is shared.

## Shared folders

```console
$ cd ~/src/myapp
$ runt new dev --mount .
$ runt exec dev -- npm test        # runs in ~/src/myapp inside the VM
```

`--mount SRC[:DST][:ro]` shares a host directory with the VM, and you can
repeat it. Without a DST, the folder appears at the **same path** inside the
VM as on your machine, so paths in logs and errors match on both sides.
`runt exec` and `runt shell` start in your current directory when it's inside
a mount. Add `:ro` to make a share read-only; this is enforced outside the
guest, so the VM can't remount it writable.

- **Ownership:** files the VM creates in a share are owned by you on the host.
- **Hot reload:** changes you make on the host don't trigger file-watch
  (inotify) events inside the VM. If a dev server running in the VM needs to
  pick up edits made on the host, turn on polling, for example
  `CHOKIDAR_USEPOLLING=1`, `WATCHPACK_POLLING=true`, or Vite's
  `server.watch.usePolling`.
- **Speed:** heavy metadata work (huge `node_modules` trees) is faster on the
  VM's own disk than on a share.
- **Trust:** treat files the VM writes into a share as untrusted, just as you
  would files from any sandboxed program. For example, symlinks it creates
  are followed by programs on your machine.

## Networking

VMs get outbound internet access through a userspace network stack, so no
root, TAP devices or bridges are involved. DNS uses your machine's own
resolver, so VPN and internal names work.

Any TCP port a program listens on inside the VM is forwarded to the same
port on your machine's `127.0.0.1` (or a free port if that one is taken).
This works even for servers that bind only to the VM's localhost. Run
`runt port VM` to see the mappings.

For safety, a VM can reach **only the public internet** by default. It can't
reach services on your machine (including ones bound to `127.0.0.1`), your
LAN, or cloud metadata endpoints. You can narrow or widen that when you
create the VM:

```sh
# Only these destinations; everything else is refused
runt new --allow github.com --allow '*.githubusercontent.com' --allow 203.0.113.7

# The internet plus private networks around you (LAN, Tailscale)
runt new --allow-lan

# Fully offline
runt new --net none
```

- `example.com` matches exactly that name and `*.example.com` matches any
  subdomain. Unlisted names don't resolve, and connections to unlisted
  addresses are dropped.
- Domains are enforced through DNS: the VM may connect to the addresses an
  allowed name resolves to. Sites sharing a CDN address can share access.
- `--allow-lan` and private addresses (`--allow 192.168.1.20`) can't be
  combined with domain rules yet.
- Your machine's own loopback services are never reachable, even with
  `--allow-lan`. Services listening on its LAN address are, with
  `--allow-lan`.

`runt logs VM --egress` lists what was refused, which helps when building an
allowlist.

## Isolation

Your code runs in a real VM with its own kernel, not in a container that
shares the host's kernel. On top of that, runt confines each VM's host-side
process, because the virtual devices run there:

- **Landlock** limits it to the VM's own files, the guest kernel and image,
  `/dev/kvm`, and the folders you shared. It can't read the rest of your
  home directory or connect to other VMs, your desktop's D-Bus session, or
  container engine sockets.
- **seccomp** blocks system calls a VMM never needs: running programs,
  ptrace, mounting, creating namespaces, loading kernel modules, bpf, and
  more.

`runt ls --json` shows what was enforced for each VM. On kernels without
full Landlock support, the VM still runs and `runt new` warns that isolation
is partial.

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
