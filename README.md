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
| `runt new [NAME] [--cpus N] [--mem 1G] [--mount SRC[:DST][:ro]] [--http PORT]` | Create and boot a VM |
| `runt exec VM [-t] [-e K=V] [-w DIR] -- CMD...` | Run a command; exits with its exit code |
| `runt shell VM` | Interactive shell |
| `runt ls` | List VMs |
| `runt stop VM` / `runt start VM` | Shut down / boot again (the disk is kept) |
| `runt rm [-f] VM` | Delete a VM and its disk |
| `runt port VM` | Ports forwarded from the VM to this machine |
| `runt logs VM [--egress] [-s SERVICE [-f]]` | Guest console log, refused network connections, or a service's output |
| `runt up` / `runt down [--rm [--volumes]]` | Run the project in `runt.toml` / stop (or remove) its VM |
| `runt build` | Build `runt.toml`'s image without running it |
| `runt schema` | JSON Schema of `runt.toml` |
| `runt volume ls` / `runt volume rm VM [NAME]` | List or delete project volumes |
| `runt mcp` | MCP server for AI agents (see below) |
| `runt skill [--install]` | Print or install the agent skill (see below) |

Add `--json` to any command for machine-readable output. runt's own errors
exit with 125 and, in JSON mode, print `{"error": {"code", "message", "hint"}}`
to stderr.

## Projects: `runt.toml`

A `runt.toml` at the root of a project describes its VM: how to build the
image, which services to keep running, where its data lives, and what the
network may reach. `runt up` builds it and runs it.

```toml
name = "myapp"                     # the VM's name

[vm]
cpus = 2
memory = "1G"

[build]
steps = [
  { run = "apt-get update && apt-get install -y python3-flask" },
  { copy = ".", to = "/app", exclude = [".git", "__pycache__"] },
]

[env]                              # for build steps, services and `runt exec`
PORT = "8000"

[services.web]
cmd = "flask --app app run --host 0.0.0.0 --port $PORT"
cwd = "/app"

[http]                             # served at http://myapp.runt.localhost:7080
port = 8000

[volumes]                          # kept when the VM is rebuilt
data = { path = "/data", size = "1G" }

[network]                          # optional, like `runt new --allow`
allow = ["api.github.com"]

[dev]                              # what `runt up` adds on this machine
mounts = [".:/app"]                # edit the live sources, not the copy
```

```console
$ runt up
building myapp (2 steps)
[1/2] run apt-get update && apt-get install -y python3-flask
...
[2/2] copy . -> /app
built myapp in 20.3 s (0 of 2 steps cached)
myapp is up: created and booted in 152 ms
  service web: running (pid 54)
  volume data at /data (1G)
  http://127.0.0.1:8000 -> port 8000 in the VM
url: http://myapp.runt.localhost:7080
logs: runt logs myapp -s web
```

- **Builds are cached step by step.** Each step becomes an image layer, keyed
  by everything that shapes it: the base image, the steps before it, the step
  itself, `[env]`, and for `copy`, the exact files copied (contents and
  permissions, not timestamps). Rerunning `runt up` redoes only steps whose
  inputs changed. A failed build keeps the steps that succeeded.
- **Builds run in a throwaway VM.** Steps run as root and can reach the
  public internet; nothing is needed on your machine besides runt. The build
  VM sees only the files being copied and writes only to a scratch
  directory, from which runt takes just the layers it asked for.
- **`copy`** takes paths relative to the project, which keep their relative
  paths under `to`. `exclude` takes gitignore-style patterns: `node_modules`
  matches anywhere, `/build` only at the top, `*.log` any log file. Copied
  files are owned by root and dated 1980-01-01, so builds are reproducible.
- **Services** are restarted when they exit (`restart = "always"`, the
  default; or `"on-failure"`, `"never"`), with backoff. Their output goes to
  `runt logs VM -s NAME`.
- **`runt up` changes as little as it can.** Changed services restart on
  their own; CPU, memory, mount, volume and network changes reboot the VM;
  and a new image replaces it with a fresh disk.
- **Volumes keep data.** Each `[volumes]` entry is an ext4 disk mounted at
  `path`, which belongs to the project rather than the VM: it survives new
  images and `runt down --rm`, and goes away with `runt down --rm --volumes`
  or `runt volume rm`. Raise `size` to grow one (it can't shrink). A volume
  starts empty and hides whatever the image has at its path.
- **Local URLs.** With `[http] port`, the app is served at
  `http://NAME.runt.localhost:7080` (`:80` is used, and left out of the URL,
  where your system lets runt bind it). Browsers and curl send `*.localhost`
  to this machine without any DNS setup, so each project gets its own origin
  and cookies whatever port it uses. Subdomains (`api.myapp.runt.localhost`)
  reach the same app. A small router process serves these URLs while any VM
  with an HTTP port runs; `runt new --http PORT` does the same for any VM.
- **Images build on runt's Debian base** (`runt/base`). Other bases, secrets
  and `runt deploy` are on the way.
- **Editors can check `runt.toml`.** [`schema/runt.schema.json`](schema/runt.schema.json)
  (also `runt schema`) gives completion, hover docs and errors in editors
  that read JSON Schema for TOML, such as VS Code with Even Better TOML: put
  this line at the top of the file.

  ```toml
  #:schema https://raw.githubusercontent.com/runt-sh/runt/main/schema/runt.schema.json
  ```

  `runt up` checks more than the schema can, such as overlapping paths and
  whether shared directories exist.

## AI agents

Agents that can run shell commands can use the CLI directly. `runt skill`
prints a short guide for them. `runt skill --install` installs it as a Claude
Code skill in `~/.claude/skills/runt/`; for other agents, paste it into their
instructions.

For agents that use tools, `runt mcp` is a [Model Context Protocol](https://modelcontextprotocol.io)
server on stdio with ten small tools: `vm_create`, `vm_exec`, `vm_list`,
`vm_start`, `vm_stop`, `vm_remove` and `vm_logs` for VMs, and
`project_build`, `project_up` and `project_down` for `runt.toml` projects.
Builds report each step as MCP progress, and a failed build returns the end
of its log. Without a `runt.toml`, `project_up` answers with the format.

```sh
# Claude Code, from your project directory
claude mcp add runt -- runt mcp
```

Other clients take the usual config: `{"command": "runt", "args": ["mcp"]}`.

You stay in charge of what an agent can reach on your machine:

- **Shares:** agents may share only the directory `runt mcp` was started in
  (or those given with `--mount-root DIR`). The same goes for projects: a
  `runt.toml` must be in one of those directories, and so must the folders
  its `[dev] mounts` share.
- **LAN:** agents can't open LAN access, by tool or by `runt.toml`, unless
  you start the server with `--allow-lan`.
- **VMs:** agents see and manage only the VMs they created, including
  project VMs from `project_up`. Use `--vm NAME` to also hand them an
  existing VM (such as a project you ran with `runt up`), or `--all-vms` to
  give them all of yours.
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
- **How many:** on x86_64, a VM has room for 4 shares, or 3 shares and
  volumes together for a VM built from `runt.toml` (libkrun has 11 interrupt
  lines for devices there). Share a common parent folder to need fewer.

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
  filesystem is a read-only, compressed erofs base image, plus any image
  layers built from `runt.toml` (erofs files on one read-only share), with a
  per-VM ext4 disk layered over them.
- **runt-agent is the guest's PID 1** (`crates/runt-agent`). It serves exec
  sessions to the host over vsock, using a small flow-controlled protocol
  (`crates/runt-proto`).

## License

Apache-2.0
