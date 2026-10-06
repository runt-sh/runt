# Working on runt

Notes for AI agents (and people) changing this repository. To *use* runt
from an agent, see `skills/runt/SKILL.md` (also printed by `runt skill`).

## Layout

| Path | What |
| --- | --- |
| `crates/runt-cli` | The `runt` binary: commands, VM lifecycle (`vm.rs`), on-disk state (`state.rs`), MCP server (`mcp.rs`), shared operations (`ops.rs`), `runt.toml` (`recipe.rs`), the image builder (`build.rs`, guest half `build.sh`, inputs via `tar.rs`), `runt up`/`down` (`project.rs`) |
| `crates/runt-agent` | PID 1 inside the guest (static musl): boot (overlay of base, recipe layers and the VM's disk), mounts, networking, exec server, port watcher, services |
| `crates/runt-proto` | Host/guest wire protocol: framed, multiplexed, credit-based flow control |
| `crates/runt-vmm` | libkrun bindings (loaded with dlopen at runtime) |
| `crates/runt-net` | Userspace networking and egress policy; the only crate that names `smolvm-network` |
| `crates/runt-sandbox` | Landlock + seccomp confinement of each VM's host process |
| `images/` | Guest kernel config, initramfs and Debian base image builds |
| `skills/runt/SKILL.md` | Agent skill, compiled into the binary |

## Build and test

```sh
make build     # target/release/runt
make test      # unit tests, no VMs (what CI runs, with make lint)
make lint      # rustfmt check + clippy -D warnings, including the musl agent
make test-vm   # boots real VMs; needs /dev/kvm, libkrun and `make assets`
```

Run `make lint` and `make test` before every commit, and `make test-vm` for
anything touching VMs, the agent, networking, mounts or the sandbox. Changes
to `crates/runt-agent` only reach VMs after `make initramfs`.

## Conventions

- **Agents are first-class users.** Every command supports `--json`; nothing
  prompts without a TTY. runt's own errors exit 125 with a stable `code` and
  a `hint` (`CliError` in `error.rs`); `runt exec` passes the guest's exit
  code through.
- **Small and fast.** The `runt` binary is about 2.1 MB and VMs boot in about
  150 ms. Measure before and after anything that could change either, and
  justify new dependencies.
- **The sandbox is real.** Each VM's supervisor confines itself
  (`vm::confine`) before booting. Anything the supervisor needs to open after
  that must be added to `vm::sandbox_policy`, or it fails with EACCES. Do the
  work before confining when you can.
- **Isolation defaults don't loosen silently.** Egress is public-internet
  only, the host's loopback is never reachable, and MCP agents only reach
  what the operator granted. Changes there need a VM test proving the
  boundary.
- **Old `vm.json` files must keep loading:** new `VmRecord` fields get
  `#[serde(default)]`.
- **Layer keys are a cache contract.** Anything that changes what a build
  step produces must change its key (`build::layer_keys`); bump `FORMAT` in
  `build.rs` when `build.sh` changes what layers contain.
- **smolvm-network is pinned exactly.** Read its source before relying on
  behaviour, and keep its types inside `runt-net`.

## Testing pitfalls

- VM tests name VMs `test-<tag>-<pid>` and remove them on drop. Check
  `runt ls` afterwards for leftovers.
- Build layers are shared by every project with the same steps. Tests that
  assert on caching should make their first step unique to the run.
- Don't kill processes with `pkill -f` patterns that also match your own
  shell's command line.
- Benchmarks must remove the VMs they create.
