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

## Managing VMs

| Command | Purpose |
| --- | --- |
| `runt ls --json` | VMs with status, shares, network policy, ports |
| `runt stop VM` / `runt start VM` | Shut down / boot again; the disk persists |
| `runt rm -f VM` | Delete VM and disk |
| `runt logs VM` | Guest console (boot problems) |
| `runt new --cpus 4 --mem 4G ...` | More resources (default 2 CPUs, 1G) |

The VM's disk (20 GiB, sparse) survives stop/start; installed packages stay.
Clean up VMs you created when the task is finished.
