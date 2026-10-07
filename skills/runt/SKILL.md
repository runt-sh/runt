---
name: runt
description: Run commands, tests, servers and untrusted code in fast, isolated Linux microVMs with the `runt` CLI. Use when work should happen outside the host - installing packages or tools, running a project's tests or dev server in a clean Linux environment, trying unfamiliar or untrusted code, or anything that needs root, a different distro, or network limits.
---

# runt: disposable Linux VMs

`runt` creates real VMs (own kernel, Debian userland, root inside) that boot in
about 0.15 s. Nothing done inside a VM touches this machine except folders you
explicitly share. VMs persist until removed, so create one per task and reuse it.

## Workflow

```sh
runt new --json -m . work          # create + boot, sharing the current directory
runt exec work -- npm test         # runs in the same path as here (it's shared)
runt exec work -- sh -c 'apt-get update && apt-get install -y ripgrep'
runt rm -f work                    # when done
```

- `runt exec VM -- CMD...` exits with the command's exit code. runt's own errors
  exit **125**; add `--json` to get `{"error":{"code","message","hint"}}` on stderr
  and follow the hint.
- `runt exec --json VM -- CMD` returns `{"exit_code","stdout","stderr"}`.
- Use `sh -c '...'` for pipes, `&&`, redirects or globs.
- `-w DIR` sets the working directory; `-e KEY=VALUE` sets environment variables.
  Without `-w`, commands start at the same path as your current directory when
  it is shared, otherwise in `/root`.
- Never use `runt shell` (interactive). Pass input via stdin:
  `printf '%s' "$text" | runt exec work -- sh -c 'cat > /tmp/f'`.

## Files

- `-m SRC[:DST][:ro]` on `runt new` shares a host directory (DST defaults to the
  same path; `:ro` makes it read-only). Shares are fixed at creation.
- Edits are visible both ways immediately. Files the VM creates belong to you on
  the host.
- File watchers in the VM don't see host edits: use polling
  (`CHOKIDAR_USEPOLLING=1`, `WATCHPACK_POLLING=true`, Vite `server.watch.usePolling`).
- For heavy dependency trees (`node_modules`, `target/`), building on the VM's own
  disk is much faster than on a share.

## Network

- Default: the public internet only. The host's services and the LAN are unreachable.
- `--allow github.com --allow '*.npmjs.org' --allow 1.2.3.4` restricts egress to a
  list (exact names; `*.x` = subdomains). `--allow-lan` adds private networks.
  `--net none` is fully offline. Policy is fixed at creation.
- If something can't connect, `runt logs VM --egress` shows what was refused.
- Servers in the VM are forwarded automatically: any port it listens on appears
  on this machine's `127.0.0.1` (same port if free). `runt port VM` lists them.
  Start long-running servers in the background:
  `runt exec work -- sh -c 'nohup npm run dev >/tmp/dev.log 2>&1 &'`.

## Projects: runt.toml

For an app that should keep running (a dev server, a database), describe its
VM in `runt.toml` at the project root and run `runt up --json` there. It builds
the image (unchanged steps are cached), creates or updates the VM named `name`,
and keeps the services running. Rerun it after editing the file or the copied
sources; it only redoes what changed.

```toml
name = "myapp"                     # also the VM's name
[vm]
cpus = 2                           # default 2
memory = "1G"                      # default 1G
[build]                            # steps run as root, with internet access
steps = [
  { run = "apt-get update && apt-get install -y python3-flask" },
  { copy = ".", to = "/app", exclude = [".git", "__pycache__"] },
]
[env]                              # build steps, services and `runt exec`
PORT = "8000"
[services.web]                     # restart = "always" (default),
cmd = "flask --app app run --host 0.0.0.0 --port $PORT"  # "on-failure", "never"
cwd = "/app"
[http]                             # URL in `runt up --json` ("url")
port = 8000
[volumes]                          # persistent: survive new images
data = { path = "/data", size = "1G" }
[network]                          # optional: what the running VM may reach
allow = ["api.github.com"]         # (same rules as --allow)
[dev]                              # applied by `runt up` on this machine
mounts = [".:/app"]                # live-edit the sources instead of the copy
```

- `copy` paths are relative to the project and keep their relative paths
  under `to` (`copy = "src"` gives `/app/src`). `exclude` takes gitignore-style
  names (`node_modules`, `*.log`, `/build`).
- A new image recreates the VM with a fresh disk; anything the app must keep
  (databases, uploads) belongs on a volume. Other changes restart only what
  changed. `runt down --rm --volumes` also deletes volumes.
- With `[http] port`, the app is at `http://myapp.runt.localhost:7080` (exact
  URL in the JSON). Prefer it over the forwarded port in what you tell the
  user. `runt new --http PORT` gives any VM such a URL.
- `runt logs myapp -s web` shows a service's output (`-f` follows);
  `runt ls --json` shows whether services run and their last exit code.
- A failed step exits 125 with `build_failed`; the hint names the full build log.
- `runt down` stops the VM, `runt down --rm` removes it.

## Managing VMs

| Command | Purpose |
| --- | --- |
| `runt ls --json` | VMs with status, shares, network policy, ports, services |
| `runt stop VM` / `runt start VM` | Shut down / boot again; the disk persists |
| `runt rm -f VM` | Delete VM and disk |
| `runt logs VM` | Guest console (boot problems) |
| `runt new --cpus 4 --mem 4G ...` | More resources (default 2 CPUs, 1G) |

The VM's disk (20 GiB, sparse) survives stop/start; installed packages stay.
Clean up VMs you created when the task is finished.
